//! `quinn` 封装：[`Transport`] 的 QUIC 实现。
//!
//! # async → 同步桥接（坑 5）
//! 本项目的管线是同步模型，而 quinn 的接收侧（`read_datagram` / `RecvStream::read`）
//! 是 async。桥接方式：
//!
//! - **端点自驱**：quinn 跑在 `runtime-smol` 上，driver 任务由 smol 全局执行器的
//!   后台线程推进，不需要外部 `block_on` 事件循环；
//! - **接收线程化**：每个连接三条专属 OS 线程（datagram / 控制 / urgent 流），
//!   用本模块自带的 [`block_on`]（std 的 `Wake` + thread parking，无 unsafe）
//!   独自等待各自的 future，结果经 `std::sync::mpsc` 汇入事件队列；
//! - **发送不阻塞**：`send_datagram` 只入 pacing 队列，真正写 socket 由
//!   发送线程按速率放行（N3）；控制流的 reliable 写用 [`block_on`] 直接
//!   完成（QUIC 流控让它天然反压）；urgent（输入）流的写再进一层专职
//!   写线程的入队口——connection 级流控窗口被控制流占满时，被挡住的
//!   是写线程而不是输入的调用方（见 [`Transport::send_reliable_urgent`]）。
//!
//! 这样 [`Transport::poll`] 只剩「带超时地取事件」（N4），路径上没有任何
//! 嵌套 runtime 或 `Future` 暴露给调用方。
//!
//! # 通道语义（N1）
//! - 媒体：QUIC datagram（RFC 9221），丢包不重传；
//! - 控制：一条双向 reliable 流，消息以 4 字节小端长度前缀成帧，
//!   边界明确、顺序保证；
//! - 输入：**独立的第二条**双向 reliable 流（urgent），同样的长度前缀成帧，
//!   叠加 quinn stream priority（[`URGENT_STREAM_PRIORITY`]）——两条流的
//!   已缓冲数据竞争发送时输入字节先出，控制流大流量不挡输入。
//!
//! 两条流的角色按 **QUIC 流 ID 序号**判定（先打开的 = 控制流），与
//! `accept_bi` 的交付顺序无关——弱网重传可能颠倒交付顺序（见
//! [`control_is_first`]）。urgent 的发送入队口是**有界**通道
//! （[`URGENT_QUEUE_CAPACITY`]），满队即报错，不无界堆积。
//!
//! # 丢包估计（N8）
//! 接收侧的帧到达跟踪（[`ArrivalTracker`]）按 `packet_idx` 去重记录
//! 分片到达；帧凑齐立即结案，未凑齐的帧在 500ms（[`TRACK_STALE_US`]，
//! 对齐重组层放弃线）后**主动结算**成 arrival / nack 计数。因此
//! `stats().loss_rate` 在真实丢包开始后几秒内就有读数——FEC 的冗余率
//! 与拥塞控制靠它行动，失真等于瞎眼。
//!
//! # datagram 启用与尺寸（坑 1 / 坑 2 / N2）
//! quinn 里 datagram 的「接收开关」与「尺寸协商」是同一个旋钮：
//! `TransportConfig::datagram_receive_buffer_size(Some(n))`——非 0 才会收
//! datagram，同时把 `max_datagram_frame_size = min(n, u16::MAX)` 协商给对端。
//! 两端的 [`transport_config`] 都设置了它。应用层的尺寸纪律由
//! [`QUIC_OVERHEAD`] 预算强制：单个数据报（含 `rc-proto` 包头）≤ `mtu - 48`。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use rc_proto::{MediaHeader, HEADER_LEN};
use rc_trace::{Mark, Outcome, TraceCollector, TraceSummary};
use rc_types::{Error, FrameSeq, Result, Timestamp};

use crate::mtls::{build_mtls_client_tls, build_mtls_server_tls, resolve_mtls_server_name};
pub use crate::mtls::{cert_sha256_hex, parse_rc_san_uri, MtlsConfig, PeerMtlsIdentity};
use crate::pacer::Pacer;
use crate::reassemble::FRAME_STALE_US;
use crate::stats::LossEstimator;
use crate::{NetConfig, NetEvent, NetStats, Role, Transport};

/// QUIC 包头 + 加密 + AEAD 的每包固定开销预算（字节）。
///
/// 单个应用数据报不得超过 `mtu - QUIC_OVERHEAD`（N2）：QUIC datagram
/// 不能跨包，超限要么被拒收要么触发 IP 分片——丢一等于丢全。
pub const QUIC_OVERHEAD: usize = 48;

/// 默认路径 MTU 预算（字节）。1200 是 QUIC 保证不分片的最小值。
pub const DEFAULT_MTU: usize = 1200;

/// pacing 初始速率（bps）。M3 之前不做自定义拥塞控制，这个值只用于
/// 削平突发；之后由拥塞控制经 [`QuicTransport::set_pacing_rate`] 驱动。
pub const DEFAULT_INITIAL_PACING_BPS: u64 = 50_000_000;

/// ALPN 协议标识。两端不一致则握手失败（防误连）。
pub const ALPN: &[u8] = b"rabbitcontrol/1";

/// urgent（输入）流的 quinn stream priority。
///
/// quinn 语义：本地已缓冲数据按 priority 从高到低发送。控制流保持默认
/// 0，urgent 流设 1——两条流竞争同一个拥塞窗口时，输入字节先出。
/// 这是独立流之上的叠加优化：不设它，独立流本身也已消除队头阻塞。
pub const URGENT_STREAM_PRIORITY: i32 = 1;

/// datagram 收发缓冲（字节）。非 0 = 启用 datagram（坑 1）。
const DATAGRAM_BUFFER_BYTES: usize = 1024 * 1024;

/// 空闲超时。迁移 / 对端卡顿期间靠 keep-alive 维持（N9）。
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// 发送线程空闲时的最长睡眠（有新包入队会被 condvar 唤醒）。
const WORKER_IDLE_WAIT: Duration = Duration::from_millis(50);

/// 单条 reliable 消息的长度上限（长度前缀是 u32，必须设界防恶意长度）。
const MAX_RELIABLE_MSG: usize = 4 * 1024 * 1024;

/// 丢包估计的帧跟踪表上限：超过后按帧序号窗口强制结算最老一批。
/// 正常流量在 [`TRACK_STALE_US`] 的主动结算下根本到不了上限——
/// 500ms 内同时挂着 4096 个未凑齐帧需要 >8000fps 的异常/恶意流量。
const RECV_TRACK_LIMIT: usize = 4096;

/// 帧到达跟踪的结算超时：与重组层的放弃线 [`FRAME_STALE_US`]
/// （500ms）同一条线。超过仍未凑齐的帧按「实到 / 缺失」主动结算进
/// [`LossEstimator`]——**不等**跟踪表攒满上限。
const TRACK_STALE_US: u32 = FRAME_STALE_US;

/// urgent（输入）入队口的容量（有界通道，防消费侧卡顿时高频输入无界堆积）。
///
/// 满队策略是**向调用方返回错误**，而不是静默丢弃或无界堆积：rc-net
/// 不理解输入消息的语义（哪条是可丢的移动事件、哪条是不可丢的按键），
/// 丢弃 / 合并的决策留给持有输入语义的上层（rc-input / rc-pipeline）。
/// 64 条对键鼠事件是数秒的缓冲余量，连这也填满即视为消费侧已卡死，
/// 上层应当降频或合并移动类事件后重试。
const URGENT_QUEUE_CAPACITY: usize = 64;

/// 控制流（Stream A）可靠接收的有界通道容量。
///
/// 接收侧消费速度跟不上高速 LAN 发送方时，控制流读线程阻塞在
/// `sync_channel::send`，让 QUIC 原生流控反压对端而不丢弃任何可靠帧。
const RELIABLE_RECV_CAPACITY: usize = 256;

/// 单次 [`Transport::poll`] 最多排空的控制流可靠帧数量。
///
/// 避免单次 `rc-session::tick()` 瞬间涌入超量 `ToolChannel` 帧触发上层溢出丢包，
/// 剩余帧留在有界通道内于下一次 `poll` 立即零等待取出。
const MAX_RELIABLE_PER_POLL: usize = 64;

/// 在 `mtu` 预算下，单个媒体分片可承载的最大负载（字节）。
///
/// 数据报 = 32 字节包头 + 媒体片 ≤ `mtu - QUIC_OVERHEAD`（N2），
/// 所以媒体片 ≤ `mtu - QUIC_OVERHEAD - 32`。默认 1200 → 1120。
pub const fn max_media_payload(mtu: usize) -> usize {
    mtu.saturating_sub(QUIC_OVERHEAD + HEADER_LEN)
}

/// 加 FEC 时的媒体分片预算（字节）：比 [`max_media_payload`] 再留一个包头。
///
/// XOR 冗余包的负载 = 组内最长数据报（头 + 媒体片），它自己还要再带一个
/// 32 字节包头。不留这份预算，冗余包就会超出 N2 上限——本卡的实测教训。
pub const fn max_media_payload_with_fec(mtu: usize) -> usize {
    mtu.saturating_sub(QUIC_OVERHEAD + 2 * HEADER_LEN)
}

// ─────────────────────────────────────────────────────────────────
// TLS 身份
// ─────────────────────────────────────────────────────────────────

/// 一端的 TLS 身份（证书链 + 私钥）。
///
/// 本卡不做证书体系（那是 M5 的会话层职责）：测试与内网场景用
/// 自签证书 + 客户端固定信任（`connect` 的 `server_certs` 参数）。
#[derive(Debug)]
pub struct Identity {
    cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
}

impl Identity {
    /// 从 PEM 文本构造（证书链 + 私钥）。
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self> {
        let cert_chain: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<std::result::Result<_, std::io::Error>>()
            .map_err(|e| Error::InvalidConfig(format!("cert pem: {e}")))?;
        if cert_chain.is_empty() {
            return Err(Error::InvalidConfig(
                "cert pem contains no certificates".into(),
            ));
        }
        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
            .map_err(|e| Error::InvalidConfig(format!("key pem: {e}")))?
            .ok_or_else(|| Error::InvalidConfig("key pem contains no private key".into()))?;
        Ok(Self { cert_chain, key })
    }

    /// 只解析证书链（客户端固定信任用）。
    pub fn certs_from_pem(
        cert_pem: &str,
    ) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<std::result::Result<_, std::io::Error>>()
            .map_err(|e| Error::InvalidConfig(format!("cert pem: {e}")))
    }

    /// 证书链（DER）。
    pub fn cert_chain(&self) -> &[rustls::pki_types::CertificateDer<'static>] {
        &self.cert_chain
    }

    pub(crate) fn clone_key(&self) -> rustls::pki_types::PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

// ─────────────────────────────────────────────────────────────────
// 端点
// ─────────────────────────────────────────────────────────────────

/// 共享的端点关闭守卫：最后一个持有者（[`QuicEndpoint`] 或由其创建的
/// [`QuicTransport`]）释放时立刻调用 `quinn::Endpoint::close`，避免已关闭端点
/// 继续占着 UDP 套接字进入 3s draining，同时确保测试/调用方提前丢弃临时端点句柄
/// 不会误杀仍在存活的连接。
struct EndpointCloser(quinn::Endpoint);

impl Drop for EndpointCloser {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"endpoint closed");
    }
}

/// QUIC 端点：绑定本地端口，可发起（Controller）或接受（Host）连接。
///
/// 一个端点可以同时做两件事（QUIC 天然支持）；本卡的用法是
/// Host `accept`、Controller `connect`。
pub struct QuicEndpoint {
    closer: Arc<EndpointCloser>,
    mtu: usize,
    role: Role,
}

impl Drop for QuicEndpoint {
    fn drop(&mut self) {
        // 没有由本端点派生的活跃 `QuicTransport` 时立即关闭底层 `quinn::Endpoint`，
        // 避免空闲/失败端点或已结束会话继续占用 UDP 套接字；若仍有活跃传输，则由
        // 最后一个 `EndpointCloser` 持有者释放时执行关闭。
        if Arc::strong_count(&self.closer) == 1 {
            self.closer.0.close(0u32.into(), b"endpoint closed");
        }
    }
}

impl QuicEndpoint {
    fn endpoint(&self) -> &quinn::Endpoint {
        &self.closer.0
    }

    /// 绑定并监听。`identity` 是本端作为服务端时的证书与私钥。
    pub fn new(cfg: &NetConfig, identity: &Identity) -> Result<Self> {
        let server_config = build_server_config(identity)?;
        Self::bind_with_server_config(cfg, Some(server_config))
    }

    /// 绑定并监听（强制双向 mTLS 校验对端设备证书）。
    pub fn new_mtls(cfg: &NetConfig, mtls: &MtlsConfig) -> Result<Self> {
        let server_config = build_mtls_server_config(mtls)?;
        Self::bind_with_server_config(cfg, Some(server_config))
    }

    fn bind_with_server_config(
        cfg: &NetConfig,
        server_config: Option<quinn::ServerConfig>,
    ) -> Result<Self> {
        validate_mtu(cfg.mtu)?;
        let socket = UdpSocket::bind(cfg.local_addr)
            .map_err(|e| Error::Network(format!("bind {}: {e}", cfg.local_addr)))?;
        Self::endpoint_from_socket(socket, cfg.mtu, cfg.role, server_config)
    }

    /// 基于已绑定（如完成 UDP 打洞）的 [`UdpSocket`] 构造 QUIC 端点。
    ///
    /// 当 `identity` 为 `Some` 时配置服务端 TLS 身份以支持 [`Self::accept`]；
    /// 为 `None` 时仅作为客户端发起 [`Self::connect`]（无需服务端证书）。
    pub fn from_socket(
        socket: UdpSocket,
        config: NetConfig,
        identity: Option<Identity>,
    ) -> Result<Self> {
        let server_config = match identity.as_ref() {
            Some(id) => Some(build_server_config(id)?),
            None => None,
        };
        Self::endpoint_from_socket(socket, config.mtu, config.role, server_config)
    }

    /// 基于已绑定 [`UdpSocket`] 构造支持双向 mTLS 的 QUIC 端点。
    ///
    /// 当 `server_mtls` 为 `Some` 时配置服务端 mTLS 验证器以支持 [`Self::accept`]；
    /// 为 `None` 时仅作为客户端发起 [`Self::connect_mtls`]。
    pub fn from_socket_mtls(
        socket: UdpSocket,
        config: NetConfig,
        server_mtls: Option<&MtlsConfig>,
    ) -> Result<Self> {
        let server_config = match server_mtls {
            Some(mtls) => Some(build_mtls_server_config(mtls)?),
            None => None,
        };
        Self::endpoint_from_socket(socket, config.mtu, config.role, server_config)
    }

    fn endpoint_from_socket(
        socket: UdpSocket,
        mtu: usize,
        role: Role,
        server_config: Option<quinn::ServerConfig>,
    ) -> Result<Self> {
        validate_mtu(mtu)?;
        socket
            .set_nonblocking(true)
            .map_err(|e| Error::Network(format!("set_nonblocking: {e}")))?;

        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            server_config,
            socket,
            Arc::new(quinn::SmolRuntime),
        )
        .map_err(|e| Error::Network(format!("endpoint: {e}")))?;

        Ok(Self {
            closer: Arc::new(EndpointCloser(endpoint)),
            mtu,
            role,
        })
    }

    /// 实际绑定的本地地址（端口填 0 时由此得知）。
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint()
            .local_addr()
            .map_err(|e| Error::Network(format!("local_addr: {e}")))
    }

    /// 本端配置角色（诊断用）。
    pub fn role(&self) -> Role {
        self.role
    }

    /// 作为 Controller 连接对端（阻塞至握手完成或超时）。
    ///
    /// `server_certs` 是固定信任的服务端证书链（自签场景直接给对端证书）。
    /// `server_name` 必须与证书 SAN 匹配（自签证书生成时要带上）。
    pub fn connect(
        &self,
        peer: SocketAddr,
        server_certs: &[rustls::pki_types::CertificateDer<'static>],
        server_name: &str,
        timeout: Duration,
    ) -> Result<QuicTransport> {
        self.connect_inner(peer, server_certs, server_name, timeout, None)
    }

    /// 可被外部取消标志打断的 [`Self::connect`] 变体（供 P2P 建连与快速回退使用）。
    ///
    /// 握手进行中不会因为分片超时而丢弃在途 `Connecting` future；每 50ms
    /// 轮询一次 `cancel`，置位时立即中止并返回网络错误。
    pub fn connect_cancelable(
        &self,
        peer: SocketAddr,
        server_certs: &[rustls::pki_types::CertificateDer<'static>],
        server_name: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<QuicTransport> {
        self.connect_inner(peer, server_certs, server_name, timeout, Some(cancel))
    }

    /// 作为 Controller 以双向 mTLS 连接对端（阻塞至握手完成或超时）。
    ///
    /// 会向服务端出示 `mtls.identity` 客户端证书，并校验证书链（`mtls.ca_certs`）、
    /// `urn:rabbitcontrol:user:{uid}:device:{did}` SAN URI 以及可选的
    /// `expected_peer_device_id` / `expected_peer_user_id`。
    /// `server_name` 为空且设置了 `expected_peer_device_id` 时，自动使用
    /// `device-{did}.rc.internal`。
    pub fn connect_mtls(
        &self,
        peer: SocketAddr,
        mtls: &MtlsConfig,
        server_name: &str,
        timeout: Duration,
    ) -> Result<QuicTransport> {
        self.connect_mtls_inner(peer, mtls, server_name, timeout, None)
    }

    /// 可被外部取消标志打断的 [`Self::connect_mtls`] 变体。
    pub fn connect_mtls_cancelable(
        &self,
        peer: SocketAddr,
        mtls: &MtlsConfig,
        server_name: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<QuicTransport> {
        self.connect_mtls_inner(peer, mtls, server_name, timeout, Some(cancel))
    }

    fn connect_mtls_inner(
        &self,
        peer: SocketAddr,
        mtls: &MtlsConfig,
        server_name: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<QuicTransport> {
        let effective_name = resolve_mtls_server_name(mtls, server_name)?;
        let tls = build_mtls_client_tls(mtls, ALPN)?;
        self.connect_with_tls(tls, peer, &effective_name, timeout, cancel)
    }

    fn connect_inner(
        &self,
        peer: SocketAddr,
        server_certs: &[rustls::pki_types::CertificateDer<'static>],
        server_name: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<QuicTransport> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        for c in server_certs {
            roots
                .add(c.clone())
                .map_err(|e| Error::InvalidConfig(format!("trust cert: {e}")))?;
        }
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| Error::InvalidConfig(format!("tls versions: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        self.connect_with_tls(tls, peer, server_name, timeout, cancel)
    }

    fn connect_with_tls(
        &self,
        tls: rustls::ClientConfig,
        peer: SocketAddr,
        server_name: &str,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<QuicTransport> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::InvalidConfig("connect timeout overflow".into()))?;

        let quic_client = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls))
            .map_err(|e| Error::InvalidConfig(format!("quic client crypto: {e}")))?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_client));
        client_config.transport_config(transport_config());

        let connecting = self
            .endpoint()
            .connect_with(client_config, peer, server_name)
            .map_err(|e| Error::Network(format!("connect: {e}")))?;

        let conn =
            block_on_deadline_cancelable(connecting, deadline, cancel, || Error::Timeout(timeout))
                .and_then(|r| r.map_err(|e| Error::Network(format!("handshake: {e}"))))?;
        // 连接发起方打开两条 reliable 双向流（先控制、后 urgent），各自写入
        // 一条长度为 0 的引导消息：QUIC 的流是「隐式打开」的——不写数据，
        // 对端的 accept_bi 永远看不到这个流（open_bi 只是本地动作）。
        // 打开顺序即角色（open_bi 的流 ID 单调递增），与到达顺序无关。
        let control = open_bootstrap_stream(&conn, deadline, timeout, cancel)?;
        let urgent = open_bootstrap_stream(&conn, deadline, timeout, cancel)?;
        // 输入流优先级高于控制流（叠加优化，见 URGENT_STREAM_PRIORITY）。
        // 流此刻必然存活，失败也无害：真正的发送错误由后续写操作上报。
        let _ = urgent.send.set_priority(URGENT_STREAM_PRIORITY);
        QuicTransport::new(conn, control, urgent, self.mtu, Arc::clone(&self.closer))
    }

    /// 作为 Host 等待并接受一个连接（阻塞至握手完成或超时）。
    pub fn accept(&self, timeout: Duration) -> Result<QuicTransport> {
        self.accept_inner(None, timeout, None)
    }

    /// 可被外部取消标志打断的 [`Self::accept`] 变体（供 P2P Host 建连与快速回退使用）。
    ///
    /// 在 `timeout` 内持续等待入站连接并完成完整握手，每 50ms 检查 `cancel`；
    /// 不会因为短分片轮询而误丢已经开始握手的连接。
    pub fn accept_cancelable(
        &self,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<QuicTransport> {
        self.accept_from_cancelable(None, timeout, cancel)
    }

    /// 可限定预期对端地址且可取消的 [`Self::accept`] 变体（供 P2P Host 定向接受打洞对端）。
    ///
    /// - 当 `expected_peer` 为 `Some(addr)` 时，对任何规范化 `(IP, port)` 不匹配的入站
    ///   `Incoming` 调用 `refuse()` 并继续等待，且单个入站握手或引导流失败时（只要尚未超时
    ///   且未被取消）继续等待合法对端接入，防止公网扫描器或陈旧对端抢占 P2P 握手；
    /// - 当 `expected_peer` 为 `None` 时，单个入站握手失败立即返回 `Err(Error::Network)`，
    ///   以便上层会话统计 `accept_errors`。
    pub fn accept_from_cancelable(
        &self,
        expected_peer: Option<SocketAddr>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<QuicTransport> {
        self.accept_inner(expected_peer, timeout, Some(cancel))
    }

    fn accept_inner(
        &self,
        expected_peer: Option<SocketAddr>,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<QuicTransport> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::InvalidConfig("accept timeout overflow".into()))?;
        let expected_canon = expected_peer.map(canonical_ip_port);

        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
                return Err(Error::Network("cancelled".into()));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(timeout));
            }

            let incoming =
                block_on_deadline_cancelable(self.endpoint().accept(), deadline, cancel, || {
                    Error::Timeout(timeout)
                })?
                .ok_or_else(|| Error::Network("endpoint stopped before accept".into()))?;

            if let Some(exp) = expected_canon {
                if canonical_ip_port(incoming.remote_address()) != exp {
                    incoming.refuse();
                    continue;
                }
            }

            // Incoming → Connecting：0.11.12 里 Incoming 本身不是 Future。
            let connecting = match incoming.accept() {
                Ok(c) => c,
                Err(_) if expected_canon.is_some() => continue,
                Err(e) => return Err(Error::Network(format!("accept: {e}"))),
            };
            let conn = match block_on_deadline_cancelable(connecting, deadline, cancel, || {
                Error::Timeout(timeout)
            }) {
                Ok(Ok(conn)) => conn,
                Ok(Err(_)) if expected_canon.is_some() => continue,
                Ok(Err(e)) => return Err(Error::Network(format!("handshake: {e}"))),
                Err(err) => return Err(err),
            };
            // 接受对端打开的两条 reliable 双向流。**角色按流 ID 序号判定**
            // （先打开的 = control、后打开的 = urgent，见 [`control_is_first`]）：
            // 弱网丢包重传会让 accept_bi 的交付顺序与打开顺序颠倒，按到达
            // 顺序分配角色会让两条流静默互换（输入进了控制流、控制进了
            // 输入流）——流 ID 是打开顺序的权威记录，不受乱序影响。
            let first = match accept_stream(&conn, deadline, timeout, cancel) {
                Ok(s) => s,
                Err(Error::Timeout(t)) => return Err(Error::Timeout(t)),
                Err(_) if cancel.is_some_and(|c| c.load(Ordering::Acquire)) => {
                    return Err(Error::Network("cancelled".into()));
                }
                Err(_) if expected_canon.is_some() => {
                    conn.close(quinn::VarInt::from(0u32), b"bootstrap failed");
                    continue;
                }
                Err(err) => return Err(err),
            };
            let second = match accept_stream(&conn, deadline, timeout, cancel) {
                Ok(s) => s,
                Err(Error::Timeout(t)) => return Err(Error::Timeout(t)),
                Err(_) if cancel.is_some_and(|c| c.load(Ordering::Acquire)) => {
                    return Err(Error::Network("cancelled".into()));
                }
                Err(_) if expected_canon.is_some() => {
                    conn.close(quinn::VarInt::from(0u32), b"bootstrap failed");
                    continue;
                }
                Err(err) => return Err(err),
            };
            let (control, urgent) = match order_by_stream_index(first, second) {
                Ok(pair) => pair,
                Err(_) if expected_canon.is_some() => {
                    conn.close(quinn::VarInt::from(0u32), b"invalid streams");
                    continue;
                }
                Err(err) => return Err(err),
            };
            let _ = urgent.send.set_priority(URGENT_STREAM_PRIORITY);
            return QuicTransport::new(conn, control, urgent, self.mtu, Arc::clone(&self.closer));
        }
    }

    /// 迁移到新的 UDP socket（N9：模拟 NAT rebinding 的路径迁移）。
    ///
    /// 影响本端点上的所有活动连接：连接 ID 不变，对端凭 ID 识别并跟随
    /// 新地址（服务端需允许迁移，[`QuicEndpoint::new`] 已开启）。对端
    /// 观察到地址变化后会上报 [`NetEvent::PathChanged`]。
    pub fn rebind(&self, socket: UdpSocket) -> Result<()> {
        self.endpoint()
            .rebind(socket)
            .map_err(|e| Error::Network(format!("rebind: {e}")))
    }
}

/// 规范化 `(IP, port)`，将 IPv4-mapped IPv6（`::ffff:a.b.c.d`）统一还原为 IPv4，
/// 确保双栈套接字与纯 IPv4 套接字间对端地址比对一致。
fn canonical_ip_port(addr: SocketAddr) -> (IpAddr, u16) {
    let ip = match addr.ip() {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        IpAddr::V4(v4) => IpAddr::V4(v4),
    };
    (ip, addr.port())
}

/// 一条 reliable 双向流（发送 + 接收两半）。
///
/// 把两半收进一个小结构体，让 [`QuicTransport::new`] 的参数列表保持在
/// clippy 限度内（项目纪律：不用 `allow` 掩盖参数过多）。
struct ReliableStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl ReliableStream {
    /// 流 ID 序号（quinn `StreamId::index()`，同一发起方按打开顺序递增）：
    /// 两条流谁承载控制、谁承载输入，以此判定（与到达顺序无关）。
    fn index(&self) -> u64 {
        self.recv.id().index()
    }
}

/// 流角色判定的纯核心：两个流 ID 序号中，**小者（先打开的）承载控制流**，
/// 大者承载 urgent（输入）流。
///
/// 返回 `Some(true)` = `a` 是控制流；`Some(false)` = `b` 是控制流；
/// 序号相等（同一条流被接受两次，协议异常）返回 `None`。
///
/// 为什么不看 accept_bi 的交付顺序：QUIC 只保证同一连接内帧的可靠性，
/// 不保证不同流的「可见顺序」——携带先开流 STREAM 帧的包丢了要重传，
/// 后开流可能先被交付。流 ID 序号是打开顺序的权威记录。
fn control_is_first(a: u64, b: u64) -> Option<bool> {
    (a != b).then_some(a < b)
}

/// 按 [`control_is_first`] 把两条已接受的流排成 `(control, urgent)`。
fn order_by_stream_index(
    a: ReliableStream,
    b: ReliableStream,
) -> Result<(ReliableStream, ReliableStream)> {
    match control_is_first(a.index(), b.index()) {
        Some(true) => Ok((a, b)),
        Some(false) => Ok((b, a)),
        None => Err(Error::Network(format!(
            "accepted the same stream twice (id index {})",
            a.index()
        ))),
    }
}

/// 构造 QUIC 服务端 TLS 与传输配置（`QuicEndpoint::new` 与 `from_socket` 共用）。
fn build_server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("tls versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(identity.cert_chain.clone(), identity.key.clone_key())
        .map_err(|e| Error::InvalidConfig(format!("server cert: {e}")))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    finish_server_config(tls)
}

/// 构造强制双向 mTLS 的 QUIC 服务端配置（`QuicEndpoint::new_mtls` 与 `from_socket_mtls` 共用）。
fn build_mtls_server_config(mtls: &MtlsConfig) -> Result<quinn::ServerConfig> {
    let tls = build_mtls_server_tls(mtls, ALPN)?;
    finish_server_config(tls)
}

fn finish_server_config(tls: rustls::ServerConfig) -> Result<quinn::ServerConfig> {
    let quic_server = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
        .map_err(|e| Error::InvalidConfig(format!("quic server crypto: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server));
    server_config
        .migration(true) // N9：允许连接迁移
        .transport_config(transport_config());
    Ok(server_config)
}

/// 打开一条 reliable 双向流并写入 0 长度引导消息（让对端 accept_bi 可见）。
fn open_bootstrap_stream(
    conn: &quinn::Connection,
    deadline: Instant,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<ReliableStream> {
    let (mut send, recv) =
        block_on_deadline_cancelable(conn.open_bi(), deadline, cancel, || Error::Timeout(timeout))
            .and_then(|r| r.map_err(|e| Error::Network(format!("open stream: {e}"))))?;
    block_on_deadline_cancelable(
        send.write_all(&0u32.to_le_bytes()),
        deadline,
        cancel,
        || Error::Timeout(timeout),
    )
    .and_then(|r| r.map_err(|e| Error::Network(format!("stream bootstrap: {e}"))))?;
    Ok(ReliableStream { send, recv })
}

/// 等待并接受一条对端打开的 reliable 双向流。
fn accept_stream(
    conn: &quinn::Connection,
    deadline: Instant,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<ReliableStream> {
    let (send, recv) = block_on_deadline_cancelable(conn.accept_bi(), deadline, cancel, || {
        Error::Timeout(timeout)
    })
    .and_then(|r| r.map_err(|e| Error::Network(format!("open stream: {e}"))))?;
    Ok(ReliableStream { send, recv })
}

/// 校验 `mtu` 预算：至少要装得下一个 QUIC 开销 + 媒体包头 + 1 字节负载。
fn validate_mtu(mtu: usize) -> Result<()> {
    if mtu <= QUIC_OVERHEAD + HEADER_LEN {
        return Err(Error::InvalidConfig(format!(
            "mtu {mtu} too small: need > {} (QUIC overhead + media header)",
            QUIC_OVERHEAD + HEADER_LEN
        )));
    }
    Ok(())
}

/// 两端一致的 [`quinn::TransportConfig`]。
///
/// datagram 缓冲非 0 是「能收 datagram」的前提，同时把尺寸上限协商给
/// 对端（坑 1 / 坑 2）；keep-alive + 空闲超时配合，迁移或短暂丢包
/// 不会断连（N9）。
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut tc = quinn::TransportConfig::default();
    tc.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_BYTES));
    tc.datagram_send_buffer_size(DATAGRAM_BUFFER_BYTES);
    tc.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
    tc.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(IDLE_TIMEOUT).expect("30s fits in VarInt idle timeout"),
    ));
    Arc::new(tc)
}

// ─────────────────────────────────────────────────────────────────
// 连接
// ─────────────────────────────────────────────────────────────────

/// 发送线程与 [`QuicTransport`] 共享的状态。
struct Shared {
    stop: AtomicBool,
    conn: quinn::Connection,
    /// pacing 队列（N3）。锁只被发送路径短持有。
    pacer: Mutex<Pacer>,
    pacer_cv: Condvar,
    /// 双接收通道（`events_rx` + 有界 `reliable_rx`）的统一唤醒信号，
    /// 让 [`Transport::poll`] 在任意通道到达事件或关闭时 < 1ms 唤醒。
    event_wake_seq: Mutex<u64>,
    event_wake_cv: Condvar,
    trace: Mutex<TraceCollector>,
    session_start: Instant,
    sent_bytes: AtomicU64,
    recv_bytes: AtomicU64,
    datagram_send_errors: AtomicU64,
}

impl Shared {
    fn notify_event(&self) {
        let mut seq = self
            .event_wake_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *seq = seq.wrapping_add(1);
        self.event_wake_cv.notify_all();
    }
}

/// 后台读线程退出时自动触发事件唤醒的 RAII 守卫，防止所有发送端退出后 `poll` 空等到超时。
struct WakeOnDrop<'a>(&'a Shared);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_event();
    }
}

/// `Transport` 的 QUIC 实现。
///
/// 构造途径只有 [`QuicEndpoint::connect`] / [`QuicEndpoint::accept`]。
/// Drop 时关闭连接并回收后台线程。
pub struct QuicTransport {
    conn: quinn::Connection,
    shared: Arc<Shared>,
    events_rx: Receiver<NetEvent>,
    /// Stream A（控制流）有界可靠接收通道（容量 [`RELIABLE_RECV_CAPACITY`]）。
    ///
    /// `Option` 用于在 [`Drop`] 阶段先释放接收端，立即解除可能阻塞在
    /// `sync_channel::send` 的控制流读线程，避免 `join` 死锁。
    reliable_rx: Option<Receiver<Vec<u8>>>,
    /// 控制流的发送半边（单流，天然保序）
    send_stream: quinn::SendStream,
    /// urgent（输入）流的发送入队口：专职写线程持有流本体。
    ///
    /// 为什么 urgent 不直接 `write_all`：QUIC 的 connection 级流控窗口
    /// 被所有流共享，控制流大流量会把连接信用占满，直接写会连本地
    /// 缓冲都进不去、把调用方挡住。入队 + 专职写线程让
    /// [`Transport::send_reliable_urgent`] 永不阻塞；写线程一旦拿到
    /// 信用，priority 保证输入字节先于控制数据发出。
    ///
    /// 通道是**有界**的（容量 [`URGENT_QUEUE_CAPACITY`]）：消费侧卡顿时
    /// 高频输入不得无界堆积，满队即向调用方报错（见
    /// [`Transport::send_reliable_urgent`] 的实现）。`Option` 只为
    /// [`Drop`] 里提前关闭（`take` 后写线程立即以 Disconnected 退出，
    /// 不必多等一轮 50ms 的 recv_timeout）。
    urgent_tx: Option<SyncSender<Vec<u8>>>,
    /// 最近的远端地址（检测迁移 → `PathChanged`，N9）
    last_remote: SocketAddr,
    mtu: usize,
    /// 丢包估计（N8）
    loss: Mutex<LossEstimator>,
    /// 接收侧的帧到达跟踪（按 packet_idx 去重 + 过期主动结算），喂 `loss`
    recv_track: ArrivalTracker,
    threads: Vec<JoinHandle<()>>,
    /// 持有端点引用，确保最后释放时关闭底层 `quinn::Endpoint`。
    _endpoint_closer: Arc<EndpointCloser>,
}

impl QuicTransport {
    fn new(
        conn: quinn::Connection,
        control: ReliableStream,
        urgent: ReliableStream,
        mtu: usize,
        endpoint_closer: Arc<EndpointCloser>,
    ) -> Result<Self> {
        let ReliableStream {
            send: send_stream,
            recv: control_recv,
        } = control;
        let ReliableStream {
            send: urgent_send,
            recv: urgent_recv,
        } = urgent;

        let (tx, events_rx) = std::sync::mpsc::channel::<NetEvent>();
        let (reliable_tx, reliable_rx) =
            std::sync::mpsc::sync_channel::<Vec<u8>>(RELIABLE_RECV_CAPACITY);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            conn: conn.clone(),
            pacer: Mutex::new(Pacer::new(DEFAULT_INITIAL_PACING_BPS)),
            pacer_cv: Condvar::new(),
            event_wake_seq: Mutex::new(0),
            event_wake_cv: Condvar::new(),
            trace: Mutex::new(TraceCollector::new(4096)),
            session_start: Instant::now(),
            sent_bytes: AtomicU64::new(0),
            recv_bytes: AtomicU64::new(0),
            datagram_send_errors: AtomicU64::new(0),
        });
        let last_remote = conn.remote_address();

        // 连接就绪事件排队，poll 首次调用即可见。
        let _ = tx.send(NetEvent::Connected);

        // urgent 专职写线程：吸收 connection 级流控的等待，让
        // send_reliable_urgent 对调用方永不被控制流反压挡住。
        // 入队口有界（URGENT_QUEUE_CAPACITY），满队策略见实现处文档。
        let (urgent_tx, urgent_rx) =
            std::sync::mpsc::sync_channel::<Vec<u8>>(URGENT_QUEUE_CAPACITY);

        let threads = vec![
            spawn_named("rc-net-datagram", {
                let tx = tx.clone();
                let shared = shared.clone();
                let conn = conn.clone();
                move || datagram_reader(conn, tx, shared)
            }),
            spawn_named("rc-net-stream", {
                let tx = tx.clone();
                let shared = shared.clone();
                move || control_stream_reader(control_recv, reliable_tx, tx, shared)
            }),
            spawn_named("rc-net-stream-urgent", {
                let tx = tx.clone();
                let shared = shared.clone();
                move || urgent_stream_reader(urgent_recv, tx, shared)
            }),
            spawn_named("rc-net-urgent-writer", {
                let shared = shared.clone();
                move || urgent_writer(urgent_send, urgent_rx, shared)
            }),
            spawn_named("rc-net-pacer", {
                let shared = shared.clone();
                move || pacer_worker(shared)
            }),
        ];

        Ok(Self {
            conn,
            shared,
            events_rx,
            reliable_rx: Some(reliable_rx),
            send_stream,
            urgent_tx: Some(urgent_tx),
            last_remote,
            mtu,
            loss: Mutex::new(LossEstimator::new()),
            recv_track: ArrivalTracker::new(),
            threads,
            _endpoint_closer: endpoint_closer,
        })
    }

    /// 单个数据报的尺寸上限：`mtu` 预算与对端协商值取小（N2）。
    fn max_datagram(&self) -> usize {
        let budget = self.mtu.saturating_sub(QUIC_OVERHEAD);
        match self.conn.max_datagram_size() {
            Some(peer) => budget.min(peer),
            None => budget,
        }
    }

    /// 对端传输层地址（QUIC 连接实际远端地址，供会话层审批展示等使用，T19）。
    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// 提取对端通过 TLS 握手出示的叶子证书身份（需包含 RabbitControl SAN URI）。
    ///
    /// 非 mTLS 服务端连接（未要求客户端证书）或对端证书不含合法
    /// `urn:rabbitcontrol:user:{user_id}:device:{device_id}` SAN URI 时返回 `None`。
    pub fn peer_mtls_identity(&self) -> Option<PeerMtlsIdentity> {
        let certs = self
            .conn
            .peer_identity()?
            .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
            .ok()?;
        let leaf = certs.first()?;
        let (user_id, device_id) = parse_rc_san_uri(leaf.as_ref())?;
        let cert_fingerprint_sha256 = cert_sha256_hex(leaf.as_ref());
        Some(PeerMtlsIdentity {
            user_id,
            device_id,
            cert_fingerprint_sha256,
        })
    }

    /// 更新 pacing 速率（M3 的拥塞控制从这里接入）。
    pub fn set_pacing_rate(&mut self, bps: u64) {
        let mut p = self.shared.pacer.lock().expect("pacer");
        p.set_rate(bps);
    }

    /// 埋点汇总（诊断导出）。
    pub fn trace_summary(&self) -> TraceSummary {
        self.shared.trace.lock().expect("trace").summary()
    }

    /// 上层把一帧的最终结局回报给传输层埋点（如重组层 gc 丢弃后记
    /// `LostInNetwork`，N6）。不存在的帧会新建时间线——迟到回报好过缺失。
    pub fn mark_frame_outcome(&self, seq: FrameSeq, outcome: Outcome) {
        let mut tc = self.shared.trace.lock().expect("trace");
        if let Some(t) = tc.get_mut(seq) {
            t.finish(outcome);
        } else {
            tc.begin(seq).finish(outcome);
        }
    }

    /// 会话相对微秒时钟。
    ///
    /// **故意**直接截断成 u32（不是 [`Timestamp::since`] 的饱和语义）：
    /// 配合判定处的 [`u32::wrapping_sub`]，71.5 分钟的回绕点自动安全
    /// （真实经过时间远小于回绕周期），8 小时 soak 不会把时钟冻结在
    /// `u32::MAX` 上。
    fn now_us(&self) -> u32 {
        self.shared.session_start.elapsed().as_micros() as u32
    }

    /// 记录一个数据报事件到帧到达跟踪（丢包估计的输入）。
    fn track_arrival(&mut self, datagram: &[u8], now_us: u32) {
        let Ok(h) = MediaHeader::parse(datagram) else {
            return; // 非媒体数据报不参与统计
        };
        if h.flags.is_parity() {
            return; // 冗余分片不计到达（它兜底的是缺的那片）
        }
        let mut loss = self.loss.lock().expect("loss");
        self.recv_track.observe(&h, now_us, &mut loss);
    }
}

// ─────────────────────────────────────────────────────────────────
// 帧到达跟踪（丢包估计的输入侧，N8）
// ─────────────────────────────────────────────────────────────────

/// 一个未结算帧的到达状态。
struct TrackedFrame {
    /// 应到分片数（首片包头声明的 `packet_cnt`，同一帧以首片为准）
    expected: u16,
    /// 已到的分片序号（**按 `packet_idx` 去重**：重复投递不重复计数）
    seen: HashSet<u16>,
    /// 首片到达时刻（微秒，调用方注入的时钟）
    first_seen_us: u32,
}

/// 帧到达跟踪表：把「应到 / 实到」喂给 [`LossEstimator`]。
///
/// 修复的两处失真：
/// - **主动结算**：未凑齐的帧在 [`TRACK_STALE_US`]（500ms，对齐重组层
///   放弃线）后立即按「实到记 arrival、缺失记 nack」结算——不再等
///   跟踪表攒满 [`RECV_TRACK_LIMIT`]。旧逻辑在 60fps / 3% 丢包下要
///   30-60 分钟才首次产生 nack，期间 `stats().loss_rate` 恒 0.0，
///   FEC 与拥塞控制全部失明；
/// - **分片去重**：到达按 `packet_idx` 记录，重复投递同一分片不会
///   把缺包帧误判完整（凑齐的判定因此可信）。
///
/// 时间由调用方注入（u32 微秒自然回绕，经过时间用 [`u32::wrapping_sub`]
/// 计算，与重组层 [`crate::reassemble::FrameAssembler::gc`] 同一纪律）。
struct ArrivalTracker {
    /// 在途未结算帧，key = `frame_seq.0`
    frames: HashMap<u64, TrackedFrame>,
    /// 已结案（凑齐或结算缺失）的最大帧序号：**不在途**的更老帧的迟到
    /// 分片直接忽略，否则已结案的帧会被迟到分片「复活」成一条假丢包
    /// 记录。所有结案路径（凑齐 / `settle_one` / `evict_oldest`）都先
    /// 把帧从在途表移除再推进本值，因此 [`ArrivalTracker::observe`] 中
    /// 排在在途表查找**之后**的本拦截只命中真正已结案的帧——在途帧
    /// 不受它影响（在途优先于结案拦截，与重组层同一纪律：乱序网络下
    /// 更新的帧可能先结案，不得饿死仍在途的老帧）。
    highest_retired: Option<u64>,
}

impl ArrivalTracker {
    fn new() -> Self {
        Self {
            frames: HashMap::new(),
            highest_retired: None,
        }
    }

    /// 当前跟踪的未结算帧数（诊断 / 测试用）。
    fn tracked_frames(&self) -> usize {
        self.frames.len()
    }

    /// 记录一个数据分片到达（冗余分片由调用方过滤）。
    ///
    /// 帧凑齐时立即全部记到达并结案；重复分片被忽略（不重复计数）。
    ///
    /// **在途优先于结案拦截**：先查在途表、再比对 `highest_retired`
    /// （与重组层 [`crate::reassemble::FrameAssembler::insert_packet`]
    /// 同一纪律）。乱序网络下单片帧（如 seq=10）可能抢在多片帧（如
    /// seq=9）凑齐之前结案、把结案线推到更高——此时在途帧的剩余分片
    /// 仍必须照常入账。若把结案拦截放在在途查找之前，seq=9 的最后
    /// 一片会被 `9 <= 10` 直接吞掉，帧永远凑不齐，500ms 后被
    /// [`ArrivalTracker::settle`] 结算成幻影丢包（loss_rate 虚高）。
    fn observe(&mut self, h: &MediaHeader, now_us: u32, loss: &mut LossEstimator) {
        let cnt = h.packet_cnt;
        if cnt == 0 || h.packet_idx >= cnt {
            return; // 非法头不参与统计（parse 已挡，双保险）
        }
        let seq = h.frame_seq.0;
        if let Some(f) = self.frames.get_mut(&seq) {
            // 仍在途的帧：分片照常入账，哪怕更新的帧已把结案线推到
            // seq 之上（在途优先，见方法文档）。
            // 同一帧的分片以首片的 expected 为准（协议保证一致）。
            if !f.seen.insert(h.packet_idx) {
                return; // 重复分片：缺包帧不得被判完整
            }
            if f.seen.len() >= f.expected as usize {
                let f = self.frames.remove(&seq).expect("just borrowed");
                loss.observe_arrival(f.expected as u64);
                self.note_retired(seq);
            }
            return;
        }
        if self.highest_retired.is_some_and(|r| seq <= r) {
            return; // 已结案且不在途的帧：迟到分片直接忽略
        }
        if cnt == 1 {
            // 单片帧：首片即凑齐，无需进表。
            loss.observe_arrival(1);
            self.note_retired(seq);
            return;
        }
        let mut seen = HashSet::with_capacity(4);
        seen.insert(h.packet_idx);
        self.frames.insert(
            seq,
            TrackedFrame {
                expected: cnt,
                seen,
                first_seen_us: now_us,
            },
        );
        // 上界保护：异常 / 恶意流量不得让跟踪表无限增长（与 N10 同一精神）。
        if self.frames.len() > RECV_TRACK_LIMIT {
            self.evict_oldest(loss);
        }
    }

    /// 主动结算过期未凑齐帧：实到记 arrival、缺失记 nack。
    ///
    /// 每次事件循环都应调用（时间注入），这样丢包几秒内就反映进
    /// `loss_rate`——FEC 的冗余率与拥塞控制据此行动。
    fn settle(&mut self, now_us: u32, loss: &mut LossEstimator) {
        if self.frames.is_empty() {
            return;
        }
        let doomed: Vec<u64> = self
            .frames
            .iter()
            .filter(|(_, f)| now_us.wrapping_sub(f.first_seen_us) > TRACK_STALE_US)
            .map(|(s, _)| *s)
            .collect();
        for s in doomed {
            self.settle_one(s, loss);
        }
    }

    /// 结算单个帧（从表中移除并记账）。
    fn settle_one(&mut self, seq: u64, loss: &mut LossEstimator) {
        if let Some(f) = self.frames.remove(&seq) {
            let arrived = f.seen.len() as u64;
            loss.observe_arrival(arrived);
            // 饱和防御：恶意的不一致包头（同帧不同 cnt）不得让缺失数为负。
            loss.observe_nack((f.expected as u64).saturating_sub(arrived));
            self.note_retired(seq);
        }
    }

    /// 表超上限时的兜底：按帧序号窗口强制结算最老一批（防恶意增长）。
    /// 正常流量在 500ms 主动结算下到不了上限。
    fn evict_oldest(&mut self, loss: &mut LossEstimator) {
        let max_seq = self.frames.keys().copied().max().unwrap_or(0);
        let cutoff = max_seq.saturating_sub(512);
        let doomed: Vec<u64> = self
            .frames
            .keys()
            .filter(|s| **s < cutoff)
            .copied()
            .collect();
        for s in doomed {
            self.settle_one(s, loss);
        }
    }

    fn note_retired(&mut self, seq: u64) {
        if self.highest_retired.map_or(true, |h| seq > h) {
            self.highest_retired = Some(seq);
        }
    }
}

impl std::fmt::Debug for ArrivalTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不展开分片集合（可能很大），只给拓扑信息（与重组层 Debug 同一模式）。
        f.debug_struct("ArrivalTracker")
            .field("tracked", &self.tracked_frames())
            .field("highest_retired", &self.highest_retired)
            .finish()
    }
}

impl Transport for QuicTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        let max = self.max_datagram();
        if data.len() > max {
            return Err(Error::Network(format!(
                "datagram {} bytes exceeds budget {max} (N2: mtu - QUIC_OVERHEAD)",
                data.len()
            )));
        }
        {
            let mut p = self.shared.pacer.lock().expect("pacer");
            p.enqueue(data.to_vec());
        }
        // 唤醒发送线程：空闲后首包立即放行（延迟优先）。
        self.shared.pacer_cv.notify_one();
        Ok(())
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        let n = write_framed(&mut self.send_stream, data)?;
        self.shared
            .sent_bytes
            .fetch_add(n as u64, Ordering::Relaxed);
        Ok(())
    }

    /// 输入走独立 urgent 流：只校验并入队，**永不阻塞调用方**。
    ///
    /// - 连接级流控的等待由专职写线程（`rc-net-urgent-writer`）吸收；
    ///   一旦拿到信用，stream priority 保证输入字节先于控制数据发出；
    /// - 入队口是**有界**通道（容量 [`URGENT_QUEUE_CAPACITY`] = 64）：
    ///   消费侧卡死、队列满时立即返回错误——既不静默无界堆积，也不替
    ///   上层偷偷丢输入。rc-net 不知道哪条输入消息可丢（移动事件可丢、
    ///   按键不可丢），丢弃 / 合并的决策留给持有输入语义的上层
    ///   （rc-input / rc-pipeline：降频、合并移动事件后重试）。
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        ensure_reliable_len(data)?;
        let Some(tx) = self.urgent_tx.as_ref() else {
            return Err(Error::Network("urgent writer stopped".into()));
        };
        tx.try_send(data.to_vec()).map_err(urgent_enqueue_error)
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        let mut out = Vec::new();
        let mut deferred_disconnected = Vec::new();
        let mut reliable_drained = 0usize;

        let drain_ready = |events_rx: &Receiver<NetEvent>,
                           reliable_rx: Option<&Receiver<Vec<u8>>>,
                           out: &mut Vec<NetEvent>,
                           deferred_disconnected: &mut Vec<NetEvent>,
                           reliable_drained: &mut usize|
         -> (bool, bool) {
            let mut events_disc = false;
            loop {
                match events_rx.try_recv() {
                    Ok(ev @ NetEvent::Disconnected { .. }) => deferred_disconnected.push(ev),
                    Ok(ev) => out.push(ev),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        events_disc = true;
                        break;
                    }
                }
            }
            let mut reliable_disc = reliable_rx.is_none();
            if let Some(rx) = reliable_rx {
                while *reliable_drained < MAX_RELIABLE_PER_POLL {
                    match rx.try_recv() {
                        Ok(payload) => {
                            out.push(NetEvent::Reliable(payload));
                            *reliable_drained += 1;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            reliable_disc = true;
                            break;
                        }
                    }
                }
            }
            (events_disc, reliable_disc)
        };

        // N4 + H1：最多阻塞到 deadline；任一通道（events_rx 或有界 reliable_rx）
        // 已有就绪事件时立即返回，不空等 timeout；均为空时通过 condvar 在 < 1ms
        // 内响应任意通道的新事件或退出信号。
        loop {
            let observed_seq = *self
                .shared
                .event_wake_seq
                .lock()
                .unwrap_or_else(|e| e.into_inner());

            let (events_disc, reliable_disc) = drain_ready(
                &self.events_rx,
                self.reliable_rx.as_ref(),
                &mut out,
                &mut deferred_disconnected,
                &mut reliable_drained,
            );

            if !out.is_empty()
                || !deferred_disconnected.is_empty()
                || (events_disc && reliable_disc)
            {
                break;
            }

            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if remaining.is_zero() {
                break;
            }

            let mut seq_guard = self
                .shared
                .event_wake_seq
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *seq_guard == observed_seq {
                let (next_guard, wait_res) = self
                    .shared
                    .event_wake_cv
                    .wait_timeout(seq_guard, remaining)
                    .unwrap_or_else(|e| e.into_inner());
                seq_guard = next_guard;
                if wait_res.timed_out() {
                    drop(seq_guard);
                    let _ = drain_ready(
                        &self.events_rx,
                        self.reliable_rx.as_ref(),
                        &mut out,
                        &mut deferred_disconnected,
                        &mut reliable_drained,
                    );
                    break;
                }
            }
            drop(seq_guard);
        }

        // 本批次内把 Disconnected 排在已就绪 Reliable 帧之后，确保收尾消息不被提前截断。
        out.extend(deferred_disconnected);

        // N9：地址变化 = 路径迁移。QUIC 连接 ID 不变，媒体流天然不断，
        // 这里只负责把变化暴露成事件让上层知情。
        let remote = self.conn.remote_address();
        if remote != self.last_remote {
            self.last_remote = remote;
            out.push(NetEvent::PathChanged);
        }

        // 丢包估计（N8）：数据报事件喂给帧到达跟踪，随后**主动结算**
        // 过期未凑齐帧（对齐重组层 500ms 放弃线）。结算无条件执行：
        // 几秒内 loss_rate 必须反映真实丢包，不能等跟踪表攒满上限。
        let now_us = self.now_us();
        for ev in &out {
            if let NetEvent::Datagram(b) = ev {
                self.track_arrival(b, now_us);
            }
        }
        let mut loss = self.loss.lock().expect("loss");
        self.recv_track.settle(now_us, &mut loss);
        drop(loss);
        Ok(out)
    }

    fn stats(&self) -> NetStats {
        let qs = self.conn.stats();
        NetStats {
            rtt_us: u32::try_from(qs.path.rtt.as_micros()).unwrap_or(u32::MAX),
            loss_rate: self.loss.lock().expect("loss").loss_rate(),
            pacing_rate_bps: self.shared.pacer.lock().expect("pacer").rate_bps(),
            cwnd_bytes: qs.path.cwnd,
            bytes_sent: self.shared.sent_bytes.load(Ordering::Relaxed),
            bytes_recv: self.shared.recv_bytes.load(Ordering::Relaxed),
        }
    }
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.pacer_cv.notify_all();
        // 先关闭 urgent 入队口：写线程的 recv_timeout 立即以 Disconnected
        // 返回，不必再多空等一轮 50ms 的 WORKER_IDLE_WAIT。
        self.urgent_tx = None;
        // 先丢弃有界 reliable 接收端：若控制流读线程正因队列满阻塞在
        // sync_channel::send，此处立即以 SendError 解除阻塞再 join。
        self.reliable_rx = None;
        // 关闭连接让阻塞中的读 future 立即出错返回，线程随之退出。
        self.conn
            .close(quinn::VarInt::from(0u32), b"transport dropped");
        self.shared.notify_event();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 后台线程
// ─────────────────────────────────────────────────────────────────

fn spawn_named(name: &str, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(f)
        .expect("spawn transport thread")
}

/// datagram 接收线程：独自等待 `read_datagram`，结果入事件队列。
fn datagram_reader(conn: quinn::Connection, tx: Sender<NetEvent>, shared: Arc<Shared>) {
    let _wake = WakeOnDrop(&shared);
    loop {
        match block_on(conn.read_datagram()) {
            Ok(bytes) => {
                shared
                    .recv_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                if tx.send(NetEvent::Datagram(bytes.to_vec())).is_err() {
                    return; // 接收端已销毁
                }
                shared.notify_event();
            }
            Err(e) => {
                // 连接断开（正常关闭 / 超时 / 出错）是最后一个事件。
                let _ = tx.send(NetEvent::Disconnected {
                    reason: e.to_string(),
                });
                shared.notify_event();
                return;
            }
        }
    }
}

/// Stream A（控制流）接收线程：解长度前缀帧并通过有界 `sync_channel` 交付。
///
/// 当上层消费速度慢于对端发送速度时，此处阻塞在 `reliable_tx.send(payload)`，
/// 停止继续从 QUIC `RecvStream` 拉取字节，从而触发 QUIC 原生流控反压对端，
/// 保证零丢帧且不在内存无界堆积。
fn control_stream_reader(
    mut stream: quinn::RecvStream,
    reliable_tx: SyncSender<Vec<u8>>,
    events_tx: Sender<NetEvent>,
    shared: Arc<Shared>,
) {
    let _wake = WakeOnDrop(&shared);
    loop {
        let mut len_buf = [0u8; 4];
        if block_on(stream.read_exact(&mut len_buf)).is_err() {
            return; // 对端关闭或连接断开
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_RELIABLE_MSG {
            let _ = events_tx.send(NetEvent::Disconnected {
                reason: format!("oversized reliable message: {len} bytes"),
            });
            shared.notify_event();
            return;
        }
        // 长度 0 是连接引导占位（让对端看见这条流），不作为事件交付。
        if len == 0 {
            continue;
        }
        let mut payload = vec![0u8; len];
        if block_on(stream.read_exact(&mut payload)).is_err() {
            return;
        }
        shared.recv_bytes.fetch_add(len as u64, Ordering::Relaxed);
        if reliable_tx.send(payload).is_err() {
            return;
        }
        shared.notify_event();
    }
}

/// Stream B（urgent 输入流）接收线程：解长度前缀帧，低延迟交付 [`NetEvent::ReliableUrgent`]。
fn urgent_stream_reader(mut stream: quinn::RecvStream, tx: Sender<NetEvent>, shared: Arc<Shared>) {
    let _wake = WakeOnDrop(&shared);
    loop {
        let mut len_buf = [0u8; 4];
        if block_on(stream.read_exact(&mut len_buf)).is_err() {
            return; // 对端关闭或连接断开
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len > MAX_RELIABLE_MSG {
            let _ = tx.send(NetEvent::Disconnected {
                reason: format!("oversized reliable message: {len} bytes"),
            });
            shared.notify_event();
            return;
        }
        // 长度 0 是连接引导占位（让对端看见这条流），不作为事件交付。
        if len == 0 {
            continue;
        }
        let mut payload = vec![0u8; len];
        if block_on(stream.read_exact(&mut payload)).is_err() {
            return;
        }
        shared.recv_bytes.fetch_add(len as u64, Ordering::Relaxed);
        if tx.send(NetEvent::ReliableUrgent(payload)).is_err() {
            return;
        }
        shared.notify_event();
    }
}

/// 校验单条 reliable 消息长度（长度前缀是 u32，必须设界防恶意长度）。
fn ensure_reliable_len(data: &[u8]) -> Result<()> {
    if data.len() > MAX_RELIABLE_MSG {
        return Err(Error::Network(format!(
            "reliable message {} bytes exceeds limit {MAX_RELIABLE_MSG}",
            data.len()
        )));
    }
    Ok(())
}

/// 把一条消息以 4 字节小端长度前缀成帧写入流（控制 / urgent 共用），
/// 返回写出的总字节数（含前缀）。
///
/// quinn 的流控会让 write_all 反压，[`block_on`] 不会忙等。
fn write_framed(stream: &mut quinn::SendStream, data: &[u8]) -> Result<usize> {
    ensure_reliable_len(data)?;
    let mut framed = Vec::with_capacity(4 + data.len());
    framed.extend_from_slice(&(data.len() as u32).to_le_bytes());
    framed.extend_from_slice(data);
    let n = framed.len();
    block_on(stream.write_all(&framed))
        .map_err(|e| Error::Network(format!("reliable write: {e}")))?;
    Ok(n)
}

/// urgent 入队结果 → 错误的映射（纯函数，便于单测满队策略）。
///
/// 满队报错而不是丢弃：rc-net 不理解输入消息的语义（移动可丢、按键
/// 不可丢），丢弃决策留给持有语义的上层；调用方看到错误后应降频 /
/// 合并移动类事件再重试。
fn urgent_enqueue_error(err: TrySendError<Vec<u8>>) -> Error {
    match err {
        TrySendError::Full(_) => Error::Network(format!(
            "urgent queue full ({URGENT_QUEUE_CAPACITY}): consumer stalled; \
             coalesce/drop stale move events at the caller"
        )),
        TrySendError::Disconnected(_) => Error::Network("urgent writer stopped".into()),
    }
}

/// urgent（输入）流专职写线程：从入队口取消息写入流。
///
/// 它存在的意义是吸收 connection 级流控的等待：控制流把共享窗口占满时，
/// 这里阻塞的是本线程而不是调用方。等待期间控制流最多领先一个流控
/// 窗口的授予周期——下一个信用一到，27 字节的输入消息比 64KB 的控制
/// 消息优先获得发送资格。
fn urgent_writer(mut stream: quinn::SendStream, rx: Receiver<Vec<u8>>, shared: Arc<Shared>) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        match rx.recv_timeout(WORKER_IDLE_WAIT) {
            Ok(msg) => match write_framed(&mut stream, &msg) {
                Ok(n) => {
                    shared.sent_bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
                // 连接断开：读线程会把它转成 Disconnected 事件。
                Err(_) => return,
            },
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// 发送线程：按 pacing 速率把数据报写进 QUIC（N3）。
fn pacer_worker(shared: Arc<Shared>) {
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let due = shared.pacer.lock().expect("pacer").poll(now);
        for d in due {
            let n = d.len() as u64;
            // 帧尾包离栈 = 整帧发出：打 Sent 点（rc-trace 语义）。
            if let Ok(h) = MediaHeader::parse(&d) {
                if h.is_last() && !h.flags.is_parity() {
                    let ts = Timestamp::since(shared.session_start, Instant::now());
                    shared
                        .trace
                        .lock()
                        .expect("trace")
                        .begin(h.frame_seq)
                        .mark(Mark::Sent, ts);
                }
            }
            match shared.conn.send_datagram(Bytes::from(d)) {
                Ok(()) => {
                    shared.sent_bytes.fetch_add(n, Ordering::Relaxed);
                }
                Err(e) => {
                    // 记数不致命：连接级错误由读线程转成 Disconnected 事件。
                    shared.datagram_send_errors.fetch_add(1, Ordering::Relaxed);
                    let _ = e;
                }
            }
        }

        // 睡到下一个放行时刻；入队会经 condvar 提前唤醒。
        let p = shared.pacer.lock().expect("pacer");
        let wait = match p.next_release_at() {
            Some(t) => t
                .saturating_duration_since(Instant::now())
                .max(Duration::from_micros(50)),
            None => WORKER_IDLE_WAIT,
        };
        drop(shared.pacer_cv.wait_timeout(p, wait).expect("pacer cv"));
    }
}

// ─────────────────────────────────────────────────────────────────
// 同步桥（无 unsafe 的 block_on）
// ─────────────────────────────────────────────────────────────────

/// 用线程 parking 实现的极简 `block_on`。
///
/// quinn 的连接由 smol 后台线程推进，这里的 future 只是等结果，
/// 不需要驱动任何 IO，所以标准库的 `Wake` + `park` 就够了。
fn block_on<F: Future>(fut: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl std::task::Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut fut = Box::pin(fut);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// 带截止时间与可选外部取消信号的 `block_on`。
///
/// 当 `cancel` 存在时，以 50ms 粒度唤醒检查取消标志，而不丢弃在途 future；
/// 置位时立即返回取消错误，超时返回 `Err(on_timeout())`。
fn block_on_deadline_cancelable<F: Future>(
    fut: F,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
    on_timeout: impl FnOnce() -> Error,
) -> Result<F::Output> {
    struct ThreadWaker(std::thread::Thread);
    impl std::task::Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    const CANCEL_POLL_STEP: Duration = Duration::from_millis(50);
    let mut fut = Box::pin(fut);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Acquire)) {
            return Err(Error::Network("cancelled".into()));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(on_timeout());
        }
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return Ok(v),
            Poll::Pending => {
                let remaining = deadline - now;
                let wait = if cancel.is_some() {
                    remaining.min(CANCEL_POLL_STEP)
                } else {
                    remaining
                };
                std::thread::park_timeout(wait);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::HeaderFlags;

    #[test]
    fn media_payload_budget_matches_n2() {
        // N2：默认 mtu 1200 → 单数据报 ≤ 1152 → 媒体片 ≤ 1120。
        assert_eq!(DEFAULT_MTU - QUIC_OVERHEAD, 1152);
        assert_eq!(max_media_payload(DEFAULT_MTU), 1152 - HEADER_LEN);
        assert_eq!(max_media_payload(DEFAULT_MTU), 1120);
        // 加 FEC：冗余包 = 最长数据报 + 自身包头，媒体片再让 32。
        assert_eq!(max_media_payload_with_fec(DEFAULT_MTU), 1088);
        // 冗余数据报尺寸闭环：32 + (32 + 1088) = 1152 = 预算上限。
        assert_eq!(
            HEADER_LEN + HEADER_LEN + max_media_payload_with_fec(DEFAULT_MTU),
            1152
        );
    }

    #[test]
    fn tiny_mtu_is_rejected() {
        assert!(validate_mtu(QUIC_OVERHEAD + HEADER_LEN).is_err());
        assert!(validate_mtu(0).is_err());
        assert!(validate_mtu(DEFAULT_MTU).is_ok());
        assert!(validate_mtu(QUIC_OVERHEAD + HEADER_LEN + 1).is_ok());
    }

    #[test]
    fn empty_identity_pem_is_rejected() {
        // 不变量违反路径：无证书 / 无私钥的 PEM 必须报配置错误，不能 panic。
        assert!(Identity::from_pem("", "").is_err());
        let cert_only = Identity::from_pem("not a pem at all", "");
        assert!(cert_only.is_err());
    }

    // ─────────────────────────────────────────────────────────────
    // 帧到达跟踪（审计修复 1：loss_rate 失真）
    // ─────────────────────────────────────────────────────────────

    /// 直接构造媒体包头（不经过 wire 序列化，纯逻辑测试）。
    fn header(seq: u64, idx: u16, cnt: u16) -> MediaHeader {
        MediaHeader {
            frame_seq: FrameSeq(seq),
            capture_ts: Timestamp(0),
            encode_done_ts: Timestamp(0),
            packet_idx: idx,
            packet_cnt: cnt,
            fec_group: 0,
            flags: HeaderFlags::empty(),
            layer_id: 0,
            payload_len: 0,
        }
    }

    #[test]
    fn loss_settles_within_stale_window_not_track_limit() {
        // 注入式模拟 3% 丢包：60fps、每帧 4 片、确定性丢包落点
        // （k*37 mod 100 < 3，37 与 100 互素 → 每 100 片恰好丢 3 片）。
        // 修复前：nack 只在跟踪表攒满 4096 帧时才产生（60fps 下要
        // 30-60 分钟），期间 loss_rate 恒 0.0。修复后：500ms 过期主动
        // 结算，~1s 内 loss_rate 必须非零，且量级贴近 3%。
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        const FRAMES: u64 = 120; // 2s @ 60fps
        let mut now_us: u32 = 0;
        let mut rate_at_1s = None;
        for seq in 1..=FRAMES {
            for idx in 0..4u16 {
                let k = (seq - 1) * 4 + idx as u64;
                if k * 37 % 100 < 3 {
                    continue; // 该分片在网络中丢失
                }
                t.observe(&header(seq, idx, 4), now_us, &mut loss);
            }
            now_us += 16_667; // 60fps 帧间隔
            t.settle(now_us, &mut loss);
            if seq == 60 && rate_at_1s.is_none() {
                rate_at_1s = Some(loss.loss_rate());
            }
        }
        // 1s（60 帧）内：最早的帧在 ~500ms 已结算，loss_rate 必须非零。
        assert!(
            rate_at_1s.unwrap() > 0.0,
            "1s 内 loss_rate 必须非零（修复前恒 0.0），got {}",
            rate_at_1s.unwrap()
        );
        // 2s 后：量级必须正确（期望 ~3%，容差 ±3 个百分点）。
        let rate = loss.loss_rate();
        assert!(
            (0.01..=0.06).contains(&rate),
            "3% 丢包的 loss_rate={rate} 量级错误"
        );
    }

    #[test]
    fn duplicate_fragments_do_not_complete_missing_frame() {
        // 修复点二：entry.1 += 1 不去重时，重复分片会把缺包帧误判完整。
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        // 帧 7 应到 4 片，只到 0/1/2，其中 0 号重复投递 3 次。
        t.observe(&header(7, 0, 4), 0, &mut loss);
        t.observe(&header(7, 0, 4), 1_000, &mut loss); // 重复
        t.observe(&header(7, 0, 4), 2_000, &mut loss); // 再重复
        t.observe(&header(7, 1, 4), 3_000, &mut loss);
        t.observe(&header(7, 2, 4), 4_000, &mut loss);
        assert_eq!(t.tracked_frames(), 1, "缺 1 片的帧仍在跟踪（不得被判完整）");
        assert_eq!(loss.arrivals(), 0, "未凑齐不得记到达");
        // 过期结算：3 到达、1 缺失。
        t.settle(600_000, &mut loss);
        assert_eq!(loss.arrivals(), 3, "重复分片只计一次");
        assert_eq!(loss.nacks(), 1);
        assert!((loss.loss_rate() - 0.25).abs() < 1e-6);
        assert_eq!(t.tracked_frames(), 0);
        // 结案后的迟到分片不得把帧「复活」成假丢包记录。
        t.observe(&header(7, 3, 4), 700_000, &mut loss);
        assert_eq!(t.tracked_frames(), 0);
        assert_eq!(loss.arrivals(), 3, "已结案帧的迟到分片不重新计数");
    }

    #[test]
    fn inflight_frame_outranks_retirement_gate() {
        // 乱序误报丢包（审计：优先级逻辑 bug）。旧顺序（结案拦截先于
        // 在途表查找）下的推演：
        //   1. 帧 9（cnt=2）第 0 片入在途表；
        //   2. 单片帧 10 抢先到达 → cnt==1 分支结案，highest_retired=10；
        //   3. 帧 9 的第 1 片到达 → `9 <= 10` 直接 return，帧 9 永远凑不齐；
        //   4. 500ms 后 settle 把它结算成 arrival=1/nack=1 的幻影丢包，
        //      loss_rate 虚高（FEC 冗余率与拥塞控制据此误动作）。
        // 修复后在途优先于结案拦截（与 reassemble 同一纪律）：仍在途的
        // 帧分片照常入账，凑齐即结案。
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        t.observe(&header(9, 0, 2), 0, &mut loss); // 帧 9 首片入在途表
        t.observe(&header(10, 0, 1), 1_000, &mut loss); // 单片帧抢先结案
        assert_eq!(t.tracked_frames(), 1, "帧 9 仍在途等待第 1 片");
        assert_eq!(loss.arrivals(), 1, "此刻仅帧 10 记到达");
        t.observe(&header(9, 1, 2), 2_000, &mut loss); // 迟到，但帧 9 仍在途
        assert_eq!(t.tracked_frames(), 0, "帧 9 凑齐即结案（不得滞留在途）");
        assert_eq!(loss.arrivals(), 3, "帧 9 的 2 片 + 帧 10 的 1 片");
        assert_eq!(loss.nacks(), 0, "不得记任何幻影缺失");
        assert_eq!(loss.loss_rate(), 0.0, "乱序不应制造丢包");
    }

    #[test]
    fn complete_frame_accounts_all_arrivals_immediately() {
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        for idx in 0..3u16 {
            t.observe(&header(1, idx, 3), 100, &mut loss);
        }
        assert_eq!(t.tracked_frames(), 0, "凑齐即结案");
        assert_eq!(loss.arrivals(), 3);
        assert_eq!(loss.nacks(), 0);
        // 没有过期帧时 settle 是空操作。
        t.settle(1_000_000, &mut loss);
        assert_eq!((loss.arrivals(), loss.nacks()), (3, 0));
    }

    #[test]
    fn tracker_settle_survives_u32_us_wraparound() {
        // 与重组层 gc 同一纪律：u32 微秒回绕点（8 小时 soak 必踩）。
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        let t0 = u32::MAX - 5_000; // 回绕前 5ms
        t.observe(&header(9, 0, 2), t0, &mut loss); // 缺 1 片
                                                    // 跨回绕后恰好 500ms：不结算（严格大于才结算）。
        t.settle(t0.wrapping_add(TRACK_STALE_US), &mut loss);
        assert_eq!(loss.nacks(), 0);
        // 501ms：结算 1 到达 + 1 缺失。
        t.settle(t0.wrapping_add(TRACK_STALE_US).wrapping_add(1), &mut loss);
        assert_eq!((loss.arrivals(), loss.nacks()), (1, 1));
    }

    #[test]
    fn tracker_evicts_when_table_exceeds_limit() {
        // 上界保护（N10 同一精神）：时间冻结时表也不得无限增长，
        // 被强制结算的最老帧必须按缺失记账。
        let mut t = ArrivalTracker::new();
        let mut loss = LossEstimator::new();
        for seq in 1..=(RECV_TRACK_LIMIT as u64 + 16) {
            t.observe(&header(seq, 0, 2), 0, &mut loss); // 每帧只到 1/2 片
        }
        assert!(t.tracked_frames() <= RECV_TRACK_LIMIT, "表不得无限增长");
        assert!(loss.nacks() > 0, "被强制结算的最老帧必须记缺失");
        assert!(loss.arrivals() > 0);
    }

    // ─────────────────────────────────────────────────────────────
    // 流角色判定（审计修复 2：流角色可能互换）
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn stream_roles_are_assigned_by_id_not_arrival_order() {
        // connect 先开 control（index 小）、后开 urgent（index 大）。
        // accept 侧即使先收到后开的流（弱网重传导致 accept_bi 交付
        // 顺序颠倒），角色仍按流 ID 序号判定。
        assert_eq!(
            control_is_first(0, 1),
            Some(true),
            "先开的（小 index）是 control"
        );
        assert_eq!(
            control_is_first(1, 0),
            Some(false),
            "乱序到达（先收到后开的流）：后到的 index 0 仍是 control"
        );
        assert_eq!(control_is_first(4, 7), Some(true));
        assert_eq!(control_is_first(9, 2), Some(false));
        assert_eq!(control_is_first(5, 5), None, "同一条流被接受两次是协议异常");
    }

    // ─────────────────────────────────────────────────────────────
    // urgent 队列（审计修复 5：无界通道）
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn urgent_queue_is_bounded_and_full_maps_to_error() {
        // 容量确为 URGENT_QUEUE_CAPACITY（有界），满队必须映射为
        // 说明原因的错误——不是静默丢弃，也不是无界堆积。
        assert_eq!(URGENT_QUEUE_CAPACITY, 64);
        let (tx, _rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(URGENT_QUEUE_CAPACITY);
        for _ in 0..URGENT_QUEUE_CAPACITY {
            tx.try_send(vec![0u8; 8]).expect("容量内必须可入队");
        }
        let err = urgent_enqueue_error(tx.try_send(vec![0u8; 8]).unwrap_err());
        match err {
            Error::Network(msg) => assert!(msg.contains("full"), "满队错误必须说明原因：{msg}"),
            other => panic!("满队必须映射为网络错误，got {other:?}"),
        }
    }

    #[test]
    fn from_socket_roundtrip_datagram_reliable_and_urgent() {
        const CERT_PEM: &str = include_str!("../tests/certs/server-cert.pem");
        const KEY_PEM: &str = include_str!("../tests/certs/server-key.pem");

        let host_sock = UdpSocket::bind("127.0.0.1:0").expect("bind host udp");
        let host_addr = host_sock.local_addr().expect("host addr");
        let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client udp");
        let client_addr = client_sock.local_addr().expect("client addr");

        let ident = Identity::from_pem(CERT_PEM, KEY_PEM).expect("identity");
        let certs = Identity::certs_from_pem(CERT_PEM).expect("certs");

        let host_ep = QuicEndpoint::from_socket(
            host_sock,
            NetConfig::new(Role::Host, host_addr),
            Some(ident),
        )
        .expect("host from_socket");
        let client_ep = QuicEndpoint::from_socket(
            client_sock,
            NetConfig::new(Role::Controller, client_addr),
            None,
        )
        .expect("client from_socket");

        assert_eq!(host_ep.local_addr().expect("host local_addr"), host_addr);
        assert_eq!(
            client_ep.local_addr().expect("client local_addr"),
            client_addr
        );

        let timeout = Duration::from_secs(10);
        let accepter = std::thread::spawn(move || host_ep.accept(timeout).expect("host accept"));
        let mut client = client_ep
            .connect(host_addr, &certs, "rabbitcontrol", timeout)
            .expect("client connect");
        let mut host = accepter.join().expect("accepter join");

        // 双向验证 datagram / reliable / urgent 三条通道
        let dgram_h2c = b"p2p-dgram-from-host".to_vec();
        let rel_h2c = b"p2p-reliable-from-host".to_vec();
        let urg_c2h = b"p2p-urgent-from-client".to_vec();
        let rel_c2h = b"p2p-reliable-from-client".to_vec();

        host.send_datagram(&dgram_h2c).expect("host send_datagram");
        host.send_reliable(&rel_h2c).expect("host send_reliable");
        client
            .send_reliable(&rel_c2h)
            .expect("client send_reliable");
        client
            .send_reliable_urgent(&urg_c2h)
            .expect("client send_reliable_urgent");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_dgram = false;
        let mut got_rel_h2c = false;
        let mut got_rel_c2h = false;
        let mut got_urg_c2h = false;

        while Instant::now() < deadline && !(got_dgram && got_rel_h2c && got_rel_c2h && got_urg_c2h)
        {
            for ev in client.poll(Duration::from_millis(20)).expect("client poll") {
                match ev {
                    NetEvent::Datagram(b) if b == dgram_h2c => got_dgram = true,
                    NetEvent::Reliable(b) if b == rel_h2c => got_rel_h2c = true,
                    _ => {}
                }
            }
            for ev in host.poll(Duration::from_millis(20)).expect("host poll") {
                match ev {
                    NetEvent::Reliable(b) if b == rel_c2h => got_rel_c2h = true,
                    NetEvent::ReliableUrgent(b) if b == urg_c2h => got_urg_c2h = true,
                    _ => {}
                }
            }
        }

        assert!(got_dgram, "client must receive datagram over from_socket");
        assert!(got_rel_h2c, "client must receive reliable over from_socket");
        assert!(got_rel_c2h, "host must receive reliable over from_socket");
        assert!(
            got_urg_c2h,
            "host must receive reliable_urgent over from_socket"
        );
    }

    #[test]
    fn accept_from_cancelable_refuses_wrong_peer_and_accepts_expected_peer() {
        const CERT_PEM: &str = include_str!("../tests/certs/server-cert.pem");
        const KEY_PEM: &str = include_str!("../tests/certs/server-key.pem");

        let ident = Identity::from_pem(CERT_PEM, KEY_PEM).expect("identity");
        let certs = Identity::certs_from_pem(CERT_PEM).expect("certs");

        let host_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("host ep");
        let host_addr = host_ep.local_addr().expect("host addr");

        let wrong_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("wrong ep");
        let good_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("good ep");
        let good_addr = good_ep.local_addr().expect("good addr");

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_host = Arc::clone(&cancel);
        let accepter = std::thread::spawn(move || {
            host_ep.accept_from_cancelable(Some(good_addr), Duration::from_secs(5), &cancel_host)
        });

        // 先由非预期对端发起连接：Host 必须 refuse 且继续等待合法对端。
        let wrong_res = wrong_ep.connect(
            host_addr,
            &certs,
            "rabbitcontrol",
            Duration::from_millis(600),
        );
        assert!(
            wrong_res.is_err(),
            "unexpected peer must be refused by accept_from_cancelable"
        );

        // 再由预期对端连接：必须握手成功且远端地址精确匹配。
        let mut client = good_ep
            .connect(host_addr, &certs, "rabbitcontrol", Duration::from_secs(5))
            .expect("expected peer must connect");
        let mut host = accepter
            .join()
            .expect("accepter join")
            .expect("host must accept expected peer");
        assert_eq!(
            canonical_ip_port(host.remote_addr()),
            canonical_ip_port(good_addr)
        );

        client.send_reliable(b"hello-expected").expect("send");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = false;
        while Instant::now() < deadline && !got {
            for ev in host.poll(Duration::from_millis(20)).expect("poll") {
                if matches!(ev, NetEvent::Reliable(ref b) if b == b"hello-expected") {
                    got = true;
                }
            }
        }
        assert!(got, "expected peer reliable payload must arrive");
    }

    #[test]
    fn bounded_reliable_backpressure_delivers_over_512_messages_without_drops() {
        const CERT_PEM: &str = include_str!("../tests/certs/server-cert.pem");
        const KEY_PEM: &str = include_str!("../tests/certs/server-key.pem");
        const TOTAL: u32 = 700;

        let ident = Identity::from_pem(CERT_PEM, KEY_PEM).expect("identity");
        let certs = Identity::certs_from_pem(CERT_PEM).expect("certs");

        let host_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("host ep");
        let host_addr = host_ep.local_addr().expect("host addr");
        let client_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("client ep");

        let accepter =
            std::thread::spawn(move || host_ep.accept(Duration::from_secs(10)).expect("accept"));
        let mut client = client_ep
            .connect(host_addr, &certs, "rabbitcontrol", Duration::from_secs(10))
            .expect("connect");
        let mut host = accepter.join().expect("join");

        // 先排掉初始 Connected，并验证空闲 poll(timeout) 在单条 Reliable 到达时低延迟唤醒。
        let init_events = host.poll(Duration::from_millis(50)).expect("init poll");
        assert!(init_events.iter().any(|e| matches!(e, NetEvent::Connected)));

        let wake_sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            client.send_reliable(b"wake-probe").expect("send wake");
            client
        });
        let t_wake = Instant::now();
        let wake_events = host.poll(Duration::from_secs(2)).expect("wake poll");
        let wake_elapsed = t_wake.elapsed();
        assert!(
            wake_events
                .iter()
                .any(|e| matches!(e, NetEvent::Reliable(b) if b == b"wake-probe")),
            "poll must wake on reliable_rx arrival"
        );
        assert!(
            wake_elapsed < Duration::from_millis(300),
            "poll wake latency too high: {wake_elapsed:?}"
        );
        let mut client = wake_sender.join().expect("wake sender join");

        // 连续灌入 700 条（> 512 且 > RELIABLE_RECV_CAPACITY=256）4 KiB 可靠帧，
        // 接收侧故意延迟消费以触发有界通道与 QUIC 流控反压，验证零丢帧且每次 poll <= 64 帧。
        let sender = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            for seq in 0..TOTAL {
                buf[..4].copy_from_slice(&seq.to_le_bytes());
                client.send_reliable(&buf).expect("send reliable frame");
            }
            client
        });

        // 刻意停顿让有界接收队列填满并反压对端。
        std::thread::sleep(Duration::from_millis(60));

        let mut received = Vec::with_capacity(TOTAL as usize);
        let deadline = Instant::now() + Duration::from_secs(10);
        while received.len() < TOTAL as usize && Instant::now() < deadline {
            let events = host.poll(Duration::from_millis(50)).expect("host poll");
            let mut reliable_in_batch = 0usize;
            for ev in events {
                if let NetEvent::Reliable(payload) = ev {
                    reliable_in_batch += 1;
                    assert_eq!(payload.len(), 4096);
                    let seq = u32::from_le_bytes(payload[..4].try_into().unwrap());
                    received.push(seq);
                }
            }
            assert!(
                reliable_in_batch <= MAX_RELIABLE_PER_POLL,
                "single poll returned {reliable_in_batch} > MAX_RELIABLE_PER_POLL ({MAX_RELIABLE_PER_POLL})"
            );
        }

        let mut client = sender.join().expect("sender join");
        assert_eq!(
            received.len(),
            TOTAL as usize,
            "must not drop any reliable frame"
        );
        let expected: Vec<u32> = (0..TOTAL).collect();
        assert_eq!(
            received, expected,
            "reliable frames must remain strictly ordered"
        );

        // 验证当有界队列满载阻塞读线程时，Drop 仍能迅速释放而不死锁。
        let flooder = std::thread::spawn(move || {
            let payload = vec![0xABu8; 1024];
            for _ in 0..(RELIABLE_RECV_CAPACITY + 64) {
                if client.send_reliable(&payload).is_err() {
                    break;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(40));
        let drop_start = Instant::now();
        drop(host);
        assert!(
            drop_start.elapsed() < Duration::from_secs(2),
            "QuicTransport::drop must unblock full reliable_rx without deadlock"
        );
        let _ = flooder.join();
    }
}
