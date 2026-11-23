//! TCP 中继传输层实现（`RCR1` 握手 + 通道复用帧，实现 [`rc_net::Transport`]）。
//!
//! 对接 `server/docs/02-protocol-and-api.md` §4 的零拷贝 TCP 中继服务（`:21117/tcp`）：
//! 1. 建连后发送 `[b"RCR1"][role: 1B (1=Host, 2=Client)][token_len: 1B][token]`；
//! 2. 读取 1 字节应答（`0x00` 表示配对成功，随后切入全双工裸字节流）；
//! 3. 在中继字节流上使用 5 字节定长头 `[ch: u8][len: u32 BE][payload]` 复用
//!    `Datagram` (`1`)、`Reliable` (`2`)、`ReliableUrgent` (`3`) 三类通道。
//!
//! 写路径采用独立有界优先级写线程（`rc-relay-writer`）：`ReliableUrgent > Reliable > Datagram`
//! （`Datagram` 队列满 16 帧时丢最旧），确保调用线程永不被 TCP 背压阻塞且输入永不被视频队头阻塞。
//!
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rc_net::mtls::{cert_sha256_hex, parse_rc_san_uri, MtlsConfig, PeerMtlsIdentity};
use rc_net::quic::Identity;
use rc_net::{NetEvent, NetStats, Transport};
use rc_types::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, DistinguishedName,
    OtherError, RootCertStore, ServerConfig, ServerConnection, SignatureScheme,
};
use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, TcpKeepalive, Type};

use crate::server_config::parse_device_id_input;

/// `RCR1` 中继握手魔数。
const RELAY_MAGIC: &[u8; 4] = b"RCR1";
/// 握手成功应答字节。
const REPLY_OK: u8 = 0x00;
/// 协议允许的最大令牌字节长度（见 `server/docs/02-protocol-and-api.md` §4.1）。
const MAX_TOKEN_LEN: usize = 128;

/// 应用层通道复用标识：媒体数据报（不可靠语义，接收队列满时丢最旧）。
const CH_DATAGRAM: u8 = 1;
/// 应用层通道复用标识：可靠控制流。
const CH_RELIABLE: u8 = 2;
/// 应用层通道复用标识：高优可靠输入流。
const CH_RELIABLE_URGENT: u8 = 3;

/// 单帧负载最大字节数（4 MiB）。
pub const MAX_RELAY_FRAME_LEN: usize = 4 * 1024 * 1024;

/// 接收事件有界队列容量（满载时对 `Datagram` 丢最旧，支撑单帧 IDR 100+ 分片突发，永不死锁）。
pub const RELAY_RX_QUEUE_CAP: usize = 4096;

/// 接收事件队列总字节上限（16 MiB），与 [`RELAY_RX_QUEUE_CAP`] 双重限流防 OOM。
pub const MAX_RX_QUEUE_BYTES: usize = 16 * 1024 * 1024;

/// 发送端 `Datagram` 队列有界容量（支撑 1512×982+ 高画质 IDR 多片突发而不截断丢包）。
const RELAY_TX_DATAGRAM_CAP: usize = 4096;
/// 发送端 `Datagram` 队列总字节上限（4 MiB），防止突发拥塞无限积压内存。
const RELAY_TX_DATAGRAM_MAX_BYTES: usize = 4 * 1024 * 1024;
/// 发送端 `Reliable` / `ReliableUrgent` 队列有界容量。
const RELAY_TX_RELIABLE_CAP: usize = 512;
/// 后台写线程单次 TCP 写超时（防止对端黑洞永久卡住写线程）。
const RELAY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// 握手期间轮询取消标志的切片上限（<= 100ms）。
const HANDSHAKE_POLL_SLICE: Duration = Duration::from_millis(50);
/// 多候选地址拨号时的单地址最小超时预算。
const MIN_ADDR_TIMEOUT: Duration = Duration::from_millis(250);
/// TCP Keepalive 空闲探测时间（30s）。
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// TCP Keepalive 探测间隔（10s）。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// 渐进式读帧的初始预分配上限（64 KiB），避免恶意/异常大长度头一次性分配 4 MiB。
const INCREMENTAL_READ_INIT_CAP: usize = 64 * 1024;

/// 中继 `:21117` 外层 TLS 1.3 双向 mTLS 认证配置。
#[derive(Clone, PartialEq, Eq)]
pub struct RelayMtlsConfig {
    /// 锚定的服务端 Root CA 证书 PEM。
    pub ca_cert_pem: String,
    /// 本端设备叶子证书 PEM。
    pub client_cert_pem: String,
    /// 本端设备 TLS 私钥 PEM（PKCS#8）。
    pub client_key_pem: String,
    /// 可选 TLS SNI 服务器名称；为 `None` 时自动从 `relay_addr` 解析主机/IP。
    pub server_name: Option<String>,
}

impl fmt::Debug for RelayMtlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayMtlsConfig")
            .field("ca_cert_pem_len", &self.ca_cert_pem.len())
            .field("client_cert_pem_len", &self.client_cert_pem.len())
            .field("client_key_pem", &"<redacted>")
            .field("server_name", &self.server_name)
            .finish()
    }
}

/// 会话层双向 mTLS 证书包与对端身份校验期望（供 LAN / P2P / E2EE 中继复用）。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionMtlsBundle {
    /// 锚定的服务端 Root CA 证书 PEM。
    pub ca_cert_pem: String,
    /// 本端设备叶子证书 PEM。
    pub cert_pem: String,
    /// 本端设备 TLS 私钥 PEM（PKCS#8）。
    pub key_pem: String,
    /// 本端绑定用户 ID（用于同账号自控校验与诊断）。
    pub local_user_id: String,
    /// 可选的预期对端 9 位设备 ID；若设置，对端证书 SAN URI 中的 `device_id` 必须精确匹配。
    pub expected_peer_device_id: Option<String>,
    /// 可选的预期对端用户 ID；若设置，对端证书 SAN URI 中的 `user_id` 必须精确匹配。
    pub expected_peer_user_id: Option<String>,
}

impl fmt::Debug for SessionMtlsBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionMtlsBundle")
            .field("ca_cert_pem_len", &self.ca_cert_pem.len())
            .field("cert_pem_len", &self.cert_pem.len())
            .field("key_pem", &"<redacted>")
            .field("local_user_id", &self.local_user_id)
            .field("expected_peer_device_id", &self.expected_peer_device_id)
            .field("expected_peer_user_id", &self.expected_peer_user_id)
            .finish()
    }
}

impl SessionMtlsBundle {
    /// 复制并覆写预期的对端 `(device_id, user_id)` 校验条件。
    pub fn with_expected_peer(
        mut self,
        device_id: Option<String>,
        user_id: Option<String>,
    ) -> Self {
        self.expected_peer_device_id = device_id;
        self.expected_peer_user_id = user_id;
        self
    }

    /// 构造用于 QUIC 双向 mTLS 的 [`MtlsConfig`]。
    pub fn to_quic_mtls_config(&self) -> std::result::Result<MtlsConfig, String> {
        let identity = Identity::from_pem(&self.cert_pem, &self.key_pem)
            .map_err(|e| format!("构造 mTLS 本端身份失败：{e}"))?;
        let ca_certs = Identity::certs_from_pem(&self.ca_cert_pem)
            .map_err(|e| format!("解析 Root CA PEM 失败：{e}"))?;
        if ca_certs.is_empty() {
            return Err("Root CA PEM 不包含任何证书".to_string());
        }
        let norm_peer_did = match self.expected_peer_device_id.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(
                parse_device_id_input(raw)
                    .ok_or_else(|| format!("expected_peer_device_id 格式无效：{raw:?}"))?,
            ),
            _ => None,
        };
        let norm_peer_uid = self
            .expected_peer_user_id
            .as_deref()
            .map(|u| u.trim().to_ascii_lowercase())
            .filter(|u| !u.is_empty());

        Ok(MtlsConfig {
            identity,
            ca_certs,
            expected_peer_device_id: norm_peer_did,
            expected_peer_user_id: norm_peer_uid,
        })
    }

    /// 导出用于 `:21117` 中继外层 mTLS 的 [`RelayMtlsConfig`]。
    pub fn to_relay_mtls_config(&self, server_name: Option<String>) -> RelayMtlsConfig {
        RelayMtlsConfig {
            ca_cert_pem: self.ca_cert_pem.clone(),
            client_cert_pem: self.cert_pem.clone(),
            client_key_pem: self.key_pem.clone(),
            server_name,
        }
    }
}

/// 中继内层端到端 E2EE TLS 1.3 mTLS 握手策略。
#[derive(Debug, Clone)]
pub enum RelayE2eeMode {
    /// 不启用内层 E2EE（仅使用外层 mTLS 或测试明文流）。
    Disabled,
    /// 控制端（Client）强制发起内层 E2EE TLS 1.3 mTLS 握手。
    ClientRequired(SessionMtlsBundle),
    /// 被控端（Host）强制要求控制端完成内层 E2EE TLS 1.3 mTLS 握手。
    HostRequired(SessionMtlsBundle),
    /// 被控端（Host）自动协商：若配对后首字节为 TLS Handshake (`0x16`) 则升级内层 E2EE mTLS，
    /// 若为通道标识 (`1..=3`) 则兼容未开启「加密连接」的控制端。
    HostAuto(SessionMtlsBundle),
}

/// 中继会话中的本端角色（对应 `RCR1` 握手的 `role` 字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayRole {
    /// 被控端（`0x01`）。
    Host = 1,
    /// 控制端（`0x02`）。
    Client = 2,
}

/// 可克隆的中继传输主动中断句柄。
///
/// 调用 [`RelayShutdownHandle::shutdown`] 会立即置位取消标志、唤醒写队列并执行
/// `TcpStream::shutdown(Shutdown::Both)`，用于从外部线程无锁中断正在进行的读/写操作。
#[derive(Clone, Debug)]
pub struct RelayShutdownHandle {
    cancel: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<TcpStream>>>,
    tx: Option<Arc<TxQueue>>,
}

impl RelayShutdownHandle {
    /// 触发取消标志并立刻关闭底层 TCP 套接字（可多次幂等调用，锁中毒安全）。
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(tx) = &self.tx {
            tx.mark_closed();
        }
        let stream = self.stream.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(s) = stream {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

#[derive(Debug)]
enum TxItem {
    Frame(Vec<u8>),
    FlushTls,
}

#[derive(Debug)]
struct TxQueueInner {
    urgent: VecDeque<Vec<u8>>,
    reliable: VecDeque<Vec<u8>>,
    datagram: VecDeque<Vec<u8>>,
    datagram_bytes: usize,
    flush_needed: bool,
    closed: bool,
}

#[derive(Debug)]
struct TxQueue {
    inner: Mutex<TxQueueInner>,
    cv: Condvar,
    datagram_cap: usize,
    datagram_max_bytes: usize,
    reliable_cap: usize,
}

impl TxQueue {
    fn new(datagram_cap: usize, datagram_max_bytes: usize, reliable_cap: usize) -> Self {
        Self {
            inner: Mutex::new(TxQueueInner {
                urgent: VecDeque::with_capacity(16),
                reliable: VecDeque::with_capacity(16),
                datagram: VecDeque::with_capacity(datagram_cap.min(256)),
                datagram_bytes: 0,
                flush_needed: false,
                closed: false,
            }),
            cv: Condvar::new(),
            datagram_cap,
            datagram_max_bytes,
            reliable_cap,
        }
    }

    fn enqueue_urgent(&self, frame: Vec<u8>) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(Error::Network("relay transport closed".to_string()));
        }
        if guard.urgent.len() >= self.reliable_cap {
            return Err(Error::Network("relay urgent tx queue full".to_string()));
        }
        guard.urgent.push_back(frame);
        self.cv.notify_one();
        Ok(())
    }

    fn enqueue_reliable(&self, frame: Vec<u8>) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(Error::Network("relay transport closed".to_string()));
        }
        if guard.reliable.len() >= self.reliable_cap {
            return Err(Error::Network("relay reliable tx queue full".to_string()));
        }
        guard.reliable.push_back(frame);
        self.cv.notify_one();
        Ok(())
    }

    fn enqueue_datagram(&self, frame: Vec<u8>) -> Result<()> {
        let frame_len = frame.len();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return Err(Error::Network("relay transport closed".to_string()));
        }
        if frame_len > self.datagram_max_bytes {
            return Ok(());
        }
        while guard.datagram.len() >= self.datagram_cap
            || guard.datagram_bytes.saturating_add(frame_len) > self.datagram_max_bytes
        {
            if let Some(dropped) = guard.datagram.pop_front() {
                guard.datagram_bytes = guard.datagram_bytes.saturating_sub(dropped.len());
            } else {
                break;
            }
        }
        guard.datagram_bytes = guard.datagram_bytes.saturating_add(frame_len);
        guard.datagram.push_back(frame);
        self.cv.notify_one();
        Ok(())
    }

    fn request_flush(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.closed {
            guard.flush_needed = true;
            self.cv.notify_one();
        }
    }

    /// 按严格优先级弹出下一发送任务：`urgent > reliable > datagram > flush_tls`。
    /// 队列全部为空且已关闭时返回 `None`。
    fn pop_next_item(&self) -> Option<TxItem> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(frame) = guard.urgent.pop_front() {
                return Some(TxItem::Frame(frame));
            }
            if let Some(frame) = guard.reliable.pop_front() {
                return Some(TxItem::Frame(frame));
            }
            if let Some(frame) = guard.datagram.pop_front() {
                guard.datagram_bytes = guard.datagram_bytes.saturating_sub(frame.len());
                return Some(TxItem::Frame(frame));
            }
            if guard.flush_needed {
                guard.flush_needed = false;
                return Some(TxItem::FlushTls);
            }
            if guard.closed {
                return None;
            }
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    fn pop_next(&self) -> Option<Vec<u8>> {
        loop {
            match self.pop_next_item()? {
                TxItem::Frame(f) => return Some(f),
                TxItem::FlushTls => continue,
            }
        }
    }

    fn mark_closed(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.closed = true;
        self.cv.notify_all();
    }
}

struct RxQueueInner {
    events: VecDeque<NetEvent>,
    total_bytes: usize,
    closed: bool,
    shutdown: bool,
}

struct RxQueue {
    inner: Mutex<RxQueueInner>,
    cv: Condvar,
    cap: usize,
    max_bytes: usize,
}

fn event_payload_bytes(ev: &NetEvent) -> usize {
    match ev {
        NetEvent::Datagram(v) | NetEvent::Reliable(v) | NetEvent::ReliableUrgent(v) => v.len(),
        NetEvent::Disconnected { reason } => reason.len(),
        NetEvent::Connected | NetEvent::PathChanged => 0,
    }
}

fn evict_oldest_datagram(guard: &mut RxQueueInner) -> bool {
    if let Some(pos) = guard
        .events
        .iter()
        .position(|ev| matches!(ev, NetEvent::Datagram(_)))
    {
        if let Some(removed) = guard.events.remove(pos) {
            guard.total_bytes = guard
                .total_bytes
                .saturating_sub(event_payload_bytes(&removed));
        }
        true
    } else {
        false
    }
}

impl RxQueue {
    fn new(cap: usize, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(RxQueueInner {
                events: VecDeque::with_capacity(cap.min(64)),
                total_bytes: 0,
                closed: false,
                shutdown: false,
            }),
            cv: Condvar::new(),
            cap,
            max_bytes,
        }
    }

    /// 推入 `Datagram`：永不阻塞；队列满（条数或总字节超限）时优先淘汰最旧的 `Datagram`。
    fn push_datagram(&self, payload: Vec<u8>) {
        let incoming_bytes = payload.len();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.shutdown || incoming_bytes > self.max_bytes {
            return;
        }
        while guard.events.len() >= self.cap
            || guard.total_bytes.saturating_add(incoming_bytes) > self.max_bytes
        {
            if !evict_oldest_datagram(&mut guard) {
                // 队列已被 Reliable/ReliableUrgent 占满，直接丢弃本 datagram 防阻塞与超内存。
                return;
            }
        }
        guard.total_bytes = guard.total_bytes.saturating_add(incoming_bytes);
        guard.events.push_back(NetEvent::Datagram(payload));
        self.cv.notify_one();
    }

    /// 推入 `Reliable` / `ReliableUrgent`：满（条数或总字节超限）时优先淘汰最旧 `Datagram`；
    /// 若全为可靠消息则短暂等待消费，且 `shutdown` 时立即唤醒，绝不死锁。
    fn push_reliable(&self, ev: NetEvent) {
        let incoming_bytes = event_payload_bytes(&ev);
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if guard.shutdown {
                return;
            }
            let fits_count = guard.events.len() < self.cap;
            let fits_bytes = guard.events.is_empty()
                || guard.total_bytes.saturating_add(incoming_bytes) <= self.max_bytes;
            if fits_count && fits_bytes {
                guard.total_bytes = guard.total_bytes.saturating_add(incoming_bytes);
                guard.events.push_back(ev);
                self.cv.notify_one();
                return;
            }
            if evict_oldest_datagram(&mut guard) {
                continue;
            }
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 推入断连事件并标记读端关闭（永不阻塞）。
    fn push_disconnected(&self, reason: String) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.closed {
            return;
        }
        guard.closed = true;
        if !guard.shutdown {
            let reason_len = reason.len();
            while guard.events.len() >= self.cap
                || guard.total_bytes.saturating_add(reason_len) > self.max_bytes
            {
                if !evict_oldest_datagram(&mut guard) {
                    break;
                }
            }
            guard.total_bytes = guard.total_bytes.saturating_add(reason_len);
            guard.events.push_back(NetEvent::Disconnected { reason });
        }
        self.cv.notify_all();
    }

    /// 标记本地主动关闭并唤醒所有阻塞等待者。
    fn mark_shutdown(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.shutdown = true;
        guard.closed = true;
        self.cv.notify_all();
    }

    /// 拉取当前所有就绪事件；若队列为空且未关闭，则最多等待 `timeout`。
    fn poll(&self, timeout: Duration) -> Vec<NetEvent> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.events.is_empty() && !guard.closed && !timeout.is_zero() {
            let deadline = Instant::now() + timeout;
            while guard.events.is_empty() && !guard.closed {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let rem = deadline - now;
                let (next_guard, wait_res) = self
                    .cv
                    .wait_timeout(guard, rem)
                    .unwrap_or_else(|e| e.into_inner());
                guard = next_guard;
                if wait_res.timed_out() {
                    break;
                }
            }
        }
        if guard.events.is_empty() {
            return Vec::new();
        }
        let out: Vec<NetEvent> = guard.events.drain(..).collect();
        guard.total_bytes = 0;
        self.cv.notify_all();
        out
    }
}

/// 基于 RabbitControl `RCR1` TCP 中继服务器的 [`Transport`] 实现（支持明文与外层 TLS 1.3 mTLS）。
pub struct TcpRelayTransport {
    tx: Arc<TxQueue>,
    rx: Arc<RxQueue>,
    shutdown_handle: RelayShutdownHandle,
    bytes_sent: Arc<AtomicU64>,
    bytes_recv: Arc<AtomicU64>,
    rtt_us: u32,
    peer_addr: SocketAddr,
    reader_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
}

/// 基于 `:21117` TCP 中继的双层加密 [`Transport`] 封装（外层 mTLS + 内层端到端 E2EE TLS 1.3 mTLS）。
///
/// 内层 E2EE 在 `RCR1` 配对成功后于建连阶段执行一次控制端（TLS Client）↔ 被控端（TLS Server）
/// 双向 mTLS 握手，之后全会话零额外帧头开销，中继服务器全程仅透传双层密文。
pub struct E2eeRelayTransport {
    inner: TcpRelayTransport,
    peer_identity: Option<PeerMtlsIdentity>,
}

impl E2eeRelayTransport {
    /// 可中断地连接中继服务器并按 `e2ee_mode` 完成外层 mTLS + 内层 E2EE TLS 1.3 mTLS 握手。
    pub fn connect_cancellable(
        relay_addr: &str,
        role: RelayRole,
        token: &str,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        outer_mtls: Option<&RelayMtlsConfig>,
        e2ee_mode: RelayE2eeMode,
    ) -> Result<Self> {
        let (transport, peer_identity) = connect_relay_internal(
            relay_addr, role, token, timeout, cancel, outer_mtls, e2ee_mode,
        )?;
        Ok(Self {
            inner: transport,
            peer_identity,
        })
    }

    /// 返回内层端到端 E2EE mTLS 握手提取并校验后的对端设备身份；未启用内层 E2EE 时返回 `None`。
    pub fn peer_mtls_identity(&self) -> Option<&PeerMtlsIdentity> {
        self.peer_identity.as_ref()
    }

    /// 本次中继传输是否已启用内层端到端 E2EE mTLS 加密。
    pub fn is_e2ee_active(&self) -> bool {
        self.peer_identity.is_some()
    }

    /// 返回当前已连接的中继服务器对端地址。
    pub fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }

    /// 返回可克隆的主动中断句柄。
    pub fn shutdown_handle(&self) -> RelayShutdownHandle {
        self.inner.shutdown_handle()
    }
}

impl Transport for E2eeRelayTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_datagram(data)
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_reliable(data)
    }

    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_reliable_urgent(data)
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
        self.inner.poll(timeout)
    }

    fn stats(&self) -> NetStats {
        self.inner.stats()
    }
}

impl TcpRelayTransport {
    /// 连接到中继服务器 `relay_addr`，完成 `RCR1` 握手并启动后台读/写线程。
    pub fn connect(
        relay_addr: &str,
        role: RelayRole,
        token: &str,
        timeout: Duration,
    ) -> Result<Self> {
        Self::connect_cancellable(
            relay_addr,
            role,
            token,
            timeout,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// 可中断地连接到中继服务器 `relay_addr`，完成 `RCR1` 握手并启动后台读/写线程。
    ///
    /// 在 TCP 拨号与等待 `RCR1` 1 字节配对应答阶段均以不超过 50ms 的切片响应 `cancel`，
    /// 避免关闭会话或应用退出时卡在长达数秒的同步建连上。
    pub fn connect_cancellable(
        relay_addr: &str,
        role: RelayRole,
        token: &str,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self> {
        let (transport, _) = connect_relay_internal(
            relay_addr,
            role,
            token,
            timeout,
            cancel,
            None,
            RelayE2eeMode::Disabled,
        )?;
        Ok(transport)
    }

    /// 可中断地以**外层强制 TLS 1.3 双向 mTLS** 连接中继服务器 `:21117` 并完成 `RCR1` 握手。
    ///
    /// 先完成 `rustls` TLS 1.3 mTLS 握手（校验证书链并向服务端出示客户端设备证书），
    /// 再在加密通道内发送 `RCR1` 握手帧与等待配对应答。
    pub fn connect_mtls_cancellable(
        relay_addr: &str,
        role: RelayRole,
        token: &str,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        mtls: &RelayMtlsConfig,
    ) -> Result<Self> {
        let (transport, _) = connect_relay_internal(
            relay_addr,
            role,
            token,
            timeout,
            cancel,
            Some(mtls),
            RelayE2eeMode::Disabled,
        )?;
        Ok(transport)
    }

    /// 返回当前已连接的中继服务器对端地址。
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// 返回可克隆的主动中断句柄，用于在外部线程即时中断中继套接字。
    pub fn shutdown_handle(&self) -> RelayShutdownHandle {
        self.shutdown_handle.clone()
    }

    fn build_frame(ch: u8, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() > MAX_RELAY_FRAME_LEN {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "relay frame payload {} bytes exceeds limit {MAX_RELAY_FRAME_LEN}",
                    data.len()
                ),
            });
        }
        let len_u32 = u32::try_from(data.len()).map_err(|_| Error::Protocol {
            offset: 1,
            msg: "relay frame length overflow u32".to_string(),
        })?;
        let mut frame = Vec::with_capacity(5 + data.len());
        frame.push(ch);
        frame.extend_from_slice(&len_u32.to_be_bytes());
        frame.extend_from_slice(data);
        Ok(frame)
    }
}

impl Transport for TcpRelayTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        let frame = Self::build_frame(CH_DATAGRAM, data)?;
        self.tx.enqueue_datagram(frame)
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        let frame = Self::build_frame(CH_RELIABLE, data)?;
        self.tx.enqueue_reliable(frame)
    }

    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        let frame = Self::build_frame(CH_RELIABLE_URGENT, data)?;
        self.tx.enqueue_urgent(frame)
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
        Ok(self.rx.poll(timeout))
    }

    fn stats(&self) -> NetStats {
        NetStats {
            rtt_us: self.rtt_us,
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_recv: self.bytes_recv.load(Ordering::Relaxed),
            ..NetStats::default()
        }
    }
}

impl Drop for TcpRelayTransport {
    fn drop(&mut self) {
        self.rx.mark_shutdown();
        self.tx.mark_closed();
        self.shutdown_handle.shutdown();
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// TLS 1.3 mTLS 与内层 E2EE 证书校验器及双层状态机
// ─────────────────────────────────────────────────────────────────

fn cert_rejected(reason: impl Into<String>) -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(
        io::Error::new(ErrorKind::InvalidData, reason.into()),
    ))))
}

fn validate_e2ee_peer_san_uri(
    end_entity: &[u8],
    expected_user_id: Option<&str>,
    expected_device_id: Option<&str>,
) -> std::result::Result<(), rustls::Error> {
    let Some((user_id, device_id)) = parse_rc_san_uri(end_entity) else {
        return Err(cert_rejected(
            "peer certificate missing valid urn:rabbitcontrol:user:<uid>:device:<did> SAN URI",
        ));
    };
    if let Some(exp_uid) = expected_user_id {
        if !user_id.eq_ignore_ascii_case(exp_uid) {
            return Err(cert_rejected(format!(
                "peer certificate user_id mismatch: expected {exp_uid}, got {user_id}"
            )));
        }
    }
    if let Some(exp_did) = expected_device_id {
        if device_id != exp_did {
            return Err(cert_rejected(format!(
                "peer certificate device_id mismatch: expected {exp_did}, got {device_id}"
            )));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct RcE2eeClientCertVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    expected_user_id: Option<String>,
    expected_device_id: Option<String>,
}

impl ClientCertVerifier for RcE2eeClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        validate_e2ee_peer_san_uri(
            end_entity.as_ref(),
            self.expected_user_id.as_deref(),
            self.expected_device_id.as_deref(),
        )?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
}

#[derive(Debug)]
struct RcE2eeServerCertVerifier {
    inner: Arc<WebPkiServerVerifier>,
    expected_user_id: Option<String>,
    expected_device_id: Option<String>,
}

impl ServerCertVerifier for RcE2eeServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        validate_e2ee_peer_san_uri(
            end_entity.as_ref(),
            self.expected_user_id.as_deref(),
            self.expected_device_id.as_deref(),
        )?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

fn build_root_cert_store(ca_cert_pem: &str) -> Result<RootCertStore> {
    let ca_certs = Identity::certs_from_pem(ca_cert_pem)?;
    if ca_certs.is_empty() {
        return Err(Error::InvalidConfig(
            "relay CA PEM contains no certificates".to_string(),
        ));
    }
    let mut roots = RootCertStore::empty();
    for c in ca_certs {
        roots
            .add(c)
            .map_err(|e| Error::InvalidConfig(format!("invalid relay CA cert: {e}")))?;
    }
    Ok(roots)
}

fn parse_relay_server_name(
    relay_addr: &str,
    override_name: Option<&str>,
) -> Result<ServerName<'static>> {
    let host_str = if let Some(over) = override_name.map(str::trim).filter(|s| !s.is_empty()) {
        over.trim_start_matches('[')
            .trim_end_matches(']')
            .to_string()
    } else {
        let trimmed = relay_addr.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            if let Some((ip_part, _)) = rest.split_once(']') {
                ip_part.to_string()
            } else {
                trimmed.to_string()
            }
        } else if trimmed.bytes().filter(|&b| b == b':').count() == 1 {
            let (host, _) = trimmed.split_once(':').expect("single colon");
            host.to_string()
        } else {
            trimmed.to_string()
        }
    };
    ServerName::try_from(host_str)
        .map_err(|e| Error::InvalidConfig(format!("invalid relay TLS server name: {e}")))
}

fn build_outer_relay_client_config(mtls: &RelayMtlsConfig) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = build_root_cert_store(&mtls.ca_cert_pem)?;
    let identity = Identity::from_pem(&mtls.client_cert_pem, &mtls.client_key_pem)?;
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("relay outer tls versions: {e}")))?
        .with_root_certificates(roots)
        .with_client_auth_cert(
            identity.cert_chain().to_vec(),
            identity
                .cert_chain()
                .first()
                .and_then(|_| {
                    rustls_pemfile::private_key(&mut mtls.client_key_pem.as_bytes())
                        .ok()
                        .flatten()
                })
                .ok_or_else(|| Error::InvalidConfig("invalid relay client key pem".to_string()))?,
        )
        .map_err(|e| Error::InvalidConfig(format!("relay outer client auth cert: {e}")))
}

fn build_e2ee_server_config(bundle: &SessionMtlsBundle) -> Result<ServerConfig> {
    let mtls = bundle.to_quic_mtls_config().map_err(Error::InvalidConfig)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for c in &mtls.ca_certs {
        roots
            .add(c.clone())
            .map_err(|e| Error::InvalidConfig(format!("e2ee root cert: {e}")))?;
    }
    let inner = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
        .build()
        .map_err(|e| Error::InvalidConfig(format!("e2ee client verifier: {e}")))?;
    let verifier = Arc::new(RcE2eeClientCertVerifier {
        inner,
        expected_user_id: mtls.expected_peer_user_id,
        expected_device_id: mtls.expected_peer_device_id,
    });
    let key = rustls_pemfile::private_key(&mut bundle.key_pem.as_bytes())
        .map_err(|e| Error::InvalidConfig(format!("e2ee key pem: {e}")))?
        .ok_or_else(|| Error::InvalidConfig("e2ee key pem empty".to_string()))?;
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("e2ee tls versions: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(mtls.identity.cert_chain().to_vec(), key)
        .map_err(|e| Error::InvalidConfig(format!("e2ee server cert: {e}")))
}

fn build_e2ee_client_config(
    bundle: &SessionMtlsBundle,
) -> Result<(ClientConfig, ServerName<'static>)> {
    let mtls = bundle.to_quic_mtls_config().map_err(Error::InvalidConfig)?;
    let peer_did = mtls.expected_peer_device_id.as_deref().ok_or_else(|| {
        Error::InvalidConfig(
            "E2EE relay client requires expected_peer_device_id to verify peer certificate"
                .to_string(),
        )
    })?;
    let server_name = ServerName::try_from(format!("device-{peer_did}.rc.internal"))
        .map_err(|e| Error::InvalidConfig(format!("e2ee server name: {e}")))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    for c in &mtls.ca_certs {
        roots
            .add(c.clone())
            .map_err(|e| Error::InvalidConfig(format!("e2ee root cert: {e}")))?;
    }
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
        .build()
        .map_err(|e| Error::InvalidConfig(format!("e2ee server verifier: {e}")))?;
    let verifier = Arc::new(RcE2eeServerCertVerifier {
        inner,
        expected_user_id: mtls.expected_peer_user_id,
        expected_device_id: mtls.expected_peer_device_id,
    });
    let key = rustls_pemfile::private_key(&mut bundle.key_pem.as_bytes())
        .map_err(|e| Error::InvalidConfig(format!("e2ee key pem: {e}")))?
        .ok_or_else(|| Error::InvalidConfig("e2ee key pem empty".to_string()))?;
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("e2ee tls versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(mtls.identity.cert_chain().to_vec(), key)
        .map_err(|e| Error::InvalidConfig(format!("e2ee client cert: {e}")))?;
    Ok((cfg, server_name))
}

enum E2eeTlsConn {
    Client(Box<ClientConnection>),
    Server(Box<ServerConnection>),
}

impl E2eeTlsConn {
    fn is_handshaking(&self) -> bool {
        match self {
            Self::Client(c) => c.is_handshaking(),
            Self::Server(s) => s.is_handshaking(),
        }
    }

    fn wants_read(&self) -> bool {
        match self {
            Self::Client(c) => c.wants_read(),
            Self::Server(s) => s.wants_read(),
        }
    }

    fn wants_write(&self) -> bool {
        match self {
            Self::Client(c) => c.wants_write(),
            Self::Server(s) => s.wants_write(),
        }
    }

    fn read_tls(&mut self, rd: &mut dyn Read) -> io::Result<usize> {
        match self {
            Self::Client(c) => c.read_tls(rd),
            Self::Server(s) => s.read_tls(rd),
        }
    }

    fn write_tls(&mut self, wr: &mut dyn Write) -> io::Result<usize> {
        match self {
            Self::Client(c) => c.write_tls(wr),
            Self::Server(s) => s.write_tls(wr),
        }
    }

    fn process_new_packets(&mut self) -> std::result::Result<rustls::IoState, rustls::Error> {
        match self {
            Self::Client(c) => c.process_new_packets(),
            Self::Server(s) => s.process_new_packets(),
        }
    }

    fn write_plaintext(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Client(c) => c.writer().write(buf),
            Self::Server(s) => s.writer().write(buf),
        }
    }

    fn read_plaintext(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(c) => c.reader().read(buf),
            Self::Server(s) => s.reader().read(buf),
        }
    }

    fn peer_identity(&self) -> Option<PeerMtlsIdentity> {
        let certs = match self {
            Self::Client(c) => c.peer_certificates(),
            Self::Server(s) => s.peer_certificates(),
        }?;
        let leaf = certs.first()?.as_ref();
        let (user_id, device_id) = parse_rc_san_uri(leaf)?;
        Some(PeerMtlsIdentity {
            user_id,
            device_id,
            cert_fingerprint_sha256: cert_sha256_hex(leaf),
        })
    }
}

struct TlsStack {
    outer: Option<Box<ClientConnection>>,
    inner: Option<E2eeTlsConn>,
}

impl TlsStack {
    fn drain_client_out(conn: &mut ClientConnection, out: &mut Vec<u8>) -> io::Result<()> {
        while conn.wants_write() {
            conn.write_tls(out)?;
        }
        Ok(())
    }

    fn write_all_client(
        conn: &mut ClientConnection,
        mut plain: &[u8],
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        while !plain.is_empty() {
            let n = conn.writer().write(plain)?;
            if n == 0 {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "tls writer returned 0",
                ));
            }
            plain = &plain[n..];
            Self::drain_client_out(conn, out)?;
        }
        Self::drain_client_out(conn, out)?;
        Ok(())
    }

    fn drain_e2ee_out(conn: &mut E2eeTlsConn, out: &mut Vec<u8>) -> io::Result<()> {
        while conn.wants_write() {
            conn.write_tls(out)?;
        }
        Ok(())
    }

    fn write_all_e2ee(
        conn: &mut E2eeTlsConn,
        mut plain: &[u8],
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        while !plain.is_empty() {
            let n = conn.write_plaintext(plain)?;
            if n == 0 {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "e2ee tls writer returned 0",
                ));
            }
            plain = &plain[n..];
            Self::drain_e2ee_out(conn, out)?;
        }
        Self::drain_e2ee_out(conn, out)?;
        Ok(())
    }

    fn encrypt_frame(&mut self, frame: &[u8]) -> io::Result<Vec<u8>> {
        let mid = match self.inner.as_mut() {
            Some(inner) => {
                let mut buf = Vec::with_capacity(frame.len() + 64);
                Self::write_all_e2ee(inner, frame, &mut buf)?;
                buf
            }
            None => frame.to_vec(),
        };
        match self.outer.as_mut() {
            Some(outer) => {
                let mut wire = Vec::with_capacity(mid.len() + 64);
                Self::write_all_client(outer, &mid, &mut wire)?;
                Ok(wire)
            }
            None => Ok(mid),
        }
    }

    fn drain_pending_writes(&mut self) -> io::Result<Vec<u8>> {
        let mut mid = Vec::new();
        if let Some(inner) = self.inner.as_mut() {
            Self::drain_e2ee_out(inner, &mut mid)?;
        }
        match self.outer.as_mut() {
            Some(outer) => {
                let mut wire = Vec::new();
                if !mid.is_empty() {
                    Self::write_all_client(outer, &mid, &mut wire)?;
                } else {
                    Self::drain_client_out(outer, &mut wire)?;
                }
                Ok(wire)
            }
            None => Ok(mid),
        }
    }

    fn drain_client_reader(conn: &mut ClientConnection, out: &mut Vec<u8>) -> io::Result<bool> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            match conn.reader().read(&mut tmp) {
                Ok(0) => return Ok(true),
                Ok(n) => out.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }

    fn drain_e2ee_reader(conn: &mut E2eeTlsConn, out: &mut Vec<u8>) -> io::Result<bool> {
        let mut tmp = [0u8; 16 * 1024];
        loop {
            match conn.read_plaintext(&mut tmp) {
                Ok(0) => return Ok(true),
                Ok(n) => out.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }

    fn feed_client_ciphertext(
        conn: &mut ClientConnection,
        mut net: &[u8],
        out: &mut Vec<u8>,
    ) -> io::Result<bool> {
        let mut eof = false;
        while !net.is_empty() {
            let n = conn.read_tls(&mut net)?;
            let state = conn
                .process_new_packets()
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
            if Self::drain_client_reader(conn, out)? || state.peer_has_closed() {
                eof = true;
            }
            if n == 0 {
                break;
            }
        }
        Ok(eof)
    }

    fn feed_e2ee_ciphertext(
        conn: &mut E2eeTlsConn,
        mut net: &[u8],
        out: &mut Vec<u8>,
    ) -> io::Result<bool> {
        let mut eof = false;
        while !net.is_empty() {
            let n = conn.read_tls(&mut net)?;
            let state = conn
                .process_new_packets()
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
            if Self::drain_e2ee_reader(conn, out)? || state.peer_has_closed() {
                eof = true;
            }
            if n == 0 {
                break;
            }
        }
        Ok(eof)
    }

    fn drain_buffered_plaintext(&mut self, out_plain: &mut Vec<u8>) -> io::Result<(bool, bool)> {
        let mut eof = false;
        let mut mid = Vec::new();
        if let Some(outer) = self.outer.as_mut() {
            if Self::drain_client_reader(outer, &mut mid)? {
                eof = true;
            }
        }
        if let Some(inner) = self.inner.as_mut() {
            if !mid.is_empty() && Self::feed_e2ee_ciphertext(inner, &mid, out_plain)? {
                eof = true;
            }
            if Self::drain_e2ee_reader(inner, out_plain)? {
                eof = true;
            }
        } else {
            out_plain.extend_from_slice(&mid);
        }
        Ok((self.wants_write(), eof))
    }

    fn decrypt_chunk(
        &mut self,
        net_bytes: &[u8],
        out_plain: &mut Vec<u8>,
    ) -> io::Result<(bool, bool)> {
        let mut eof = false;
        let mid_buf: Vec<u8>;
        let mid_slice: &[u8] = if let Some(outer) = self.outer.as_mut() {
            let mut tmp_mid = Vec::with_capacity(net_bytes.len());
            if Self::feed_client_ciphertext(outer, net_bytes, &mut tmp_mid)? {
                eof = true;
            }
            mid_buf = tmp_mid;
            &mid_buf
        } else {
            net_bytes
        };

        if let Some(inner) = self.inner.as_mut() {
            if !mid_slice.is_empty() && Self::feed_e2ee_ciphertext(inner, mid_slice, out_plain)? {
                eof = true;
            }
        } else {
            out_plain.extend_from_slice(mid_slice);
        }

        Ok((self.wants_write(), eof))
    }

    fn wants_write(&self) -> bool {
        self.outer.as_ref().is_some_and(|c| c.wants_write())
            || self.inner.as_ref().is_some_and(|c| c.wants_write())
    }
}

fn outer_or_tcp_write_all(
    stream: &mut TcpStream,
    outer: Option<&mut ClientConnection>,
    data: &[u8],
    timeout: Duration,
) -> Result<()> {
    match outer {
        Some(conn) => {
            let mut wire = Vec::with_capacity(data.len() + 64);
            TlsStack::write_all_client(conn, data, &mut wire)
                .map_err(|e| map_handshake_io(e, timeout))?;
            if !wire.is_empty() {
                write_all_retry(stream, &wire).map_err(|e| map_handshake_io(e, timeout))?;
                stream.flush().map_err(|e| map_handshake_io(e, timeout))?;
            }
            Ok(())
        }
        None => {
            write_all_retry(stream, data).map_err(|e| map_handshake_io(e, timeout))?;
            stream.flush().map_err(|e| map_handshake_io(e, timeout))?;
            Ok(())
        }
    }
}

fn outer_or_tcp_read_some(
    stream: &mut TcpStream,
    mut outer: Option<&mut ClientConnection>,
    buf: &mut [u8],
    deadline: Instant,
    total_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<usize> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Network("relay connect cancelled".to_string()));
        }
        if let Some(conn) = outer.as_deref_mut() {
            match conn.reader().read(buf) {
                Ok(0) => return Ok(0),
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(map_handshake_io(e, total_timeout)),
            }
        }

        let rem = remaining_or_timeout(deadline, total_timeout)?;
        let slice = rem.min(HANDSHAKE_POLL_SLICE);
        stream.set_read_timeout(Some(slice))?;

        if let Some(conn) = outer.as_deref_mut() {
            let mut net_tmp = [0u8; 16 * 1024];
            match stream.read(&mut net_tmp) {
                Ok(0) => return Ok(0),
                Ok(n) => {
                    let mut slice = &net_tmp[..n];
                    while !slice.is_empty() {
                        let consumed = conn
                            .read_tls(&mut slice)
                            .map_err(|e| map_handshake_io(e, total_timeout))?;
                        conn.process_new_packets()
                            .map_err(|e| Error::Network(format!("outer relay tls error: {e}")))?;
                        if consumed == 0 {
                            break;
                        }
                    }
                    if conn.wants_write() {
                        let mut ack_wire = Vec::new();
                        TlsStack::drain_client_out(conn, &mut ack_wire)
                            .map_err(|e| map_handshake_io(e, total_timeout))?;
                        if !ack_wire.is_empty() {
                            write_all_retry(stream, &ack_wire)
                                .map_err(|e| map_handshake_io(e, total_timeout))?;
                        }
                    }
                    match conn.reader().read(buf) {
                        Ok(0) => return Ok(0),
                        Ok(rn) => return Ok(rn),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                        Err(e) => return Err(map_handshake_io(e, total_timeout)),
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue;
                }
                Err(e) => return Err(map_handshake_io(e, total_timeout)),
            }
        } else {
            match stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue;
                }
                Err(e) => return Err(map_handshake_io(e, total_timeout)),
            }
        }
    }
}

fn complete_outer_tls_handshake(
    stream: &mut TcpStream,
    conn: &mut ClientConnection,
    deadline: Instant,
    total_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    while conn.is_handshaking() {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Network("relay connect cancelled".to_string()));
        }
        let rem = remaining_or_timeout(deadline, total_timeout)?;
        let slice = rem.min(HANDSHAKE_POLL_SLICE);
        stream.set_read_timeout(Some(slice))?;
        stream.set_write_timeout(Some(rem))?;
        match conn.complete_io(stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(e) => {
                return Err(Error::Network(format!(
                    "relay outer mTLS handshake failed: {e}"
                )));
            }
        }
    }
    Ok(())
}

fn complete_e2ee_handshake(
    stream: &mut TcpStream,
    mut outer: Option<&mut ClientConnection>,
    inner: &mut E2eeTlsConn,
    mut initial_bytes: Vec<u8>,
    deadline: Instant,
    total_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut read_buf = [0u8; 16 * 1024];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Network("relay connect cancelled".to_string()));
        }
        if inner.wants_write() {
            let mut out_inner = Vec::new();
            TlsStack::drain_e2ee_out(inner, &mut out_inner)
                .map_err(|e| map_handshake_io(e, total_timeout))?;
            if !out_inner.is_empty() {
                outer_or_tcp_write_all(stream, outer.as_deref_mut(), &out_inner, total_timeout)?;
            }
        }
        if !inner.is_handshaking() {
            break;
        }

        if !initial_bytes.is_empty() {
            let mut slice = initial_bytes.as_slice();
            while !slice.is_empty() {
                let n = inner
                    .read_tls(&mut slice)
                    .map_err(|e| map_handshake_io(e, total_timeout))?;
                inner
                    .process_new_packets()
                    .map_err(|e| Error::Network(format!("E2EE mTLS handshake failed: {e}")))?;
                if n == 0 {
                    break;
                }
            }
            initial_bytes.clear();
            continue;
        }

        if inner.wants_read() {
            let n = outer_or_tcp_read_some(
                stream,
                outer.as_deref_mut(),
                &mut read_buf,
                deadline,
                total_timeout,
                cancel,
            )?;
            if n == 0 {
                return Err(Error::Network(
                    "relay stream hit EOF before completing E2EE mTLS handshake".to_string(),
                ));
            }
            let mut slice = &read_buf[..n];
            while !slice.is_empty() {
                let consumed = inner
                    .read_tls(&mut slice)
                    .map_err(|e| map_handshake_io(e, total_timeout))?;
                inner
                    .process_new_packets()
                    .map_err(|e| Error::Network(format!("E2EE mTLS handshake failed: {e}")))?;
                if consumed == 0 {
                    break;
                }
            }
        }
    }

    if inner.wants_write() {
        let mut out_inner = Vec::new();
        TlsStack::drain_e2ee_out(inner, &mut out_inner)
            .map_err(|e| map_handshake_io(e, total_timeout))?;
        if !out_inner.is_empty() {
            outer_or_tcp_write_all(stream, outer, &out_inner, total_timeout)?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn connect_relay_internal(
    relay_addr: &str,
    role: RelayRole,
    token: &str,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    outer_mtls: Option<&RelayMtlsConfig>,
    e2ee_mode: RelayE2eeMode,
) -> Result<(TcpRelayTransport, Option<PeerMtlsIdentity>)> {
    let token_bytes = token.as_bytes();
    if token_bytes.is_empty() || token_bytes.len() > MAX_TOKEN_LEN {
        return Err(Error::InvalidConfig(format!(
            "invalid relay token length {} (expected 1..={MAX_TOKEN_LEN})",
            token_bytes.len()
        )));
    }
    if cancel.load(Ordering::Acquire) {
        return Err(Error::Network("relay connect cancelled".to_string()));
    }

    match (&e2ee_mode, role) {
        (RelayE2eeMode::Disabled, _) => {}
        (RelayE2eeMode::ClientRequired(_), RelayRole::Client) => {}
        (RelayE2eeMode::HostRequired(_) | RelayE2eeMode::HostAuto(_), RelayRole::Host) => {}
        _ => {
            return Err(Error::InvalidConfig(
                "RelayE2eeMode does not match RelayRole".to_string(),
            ));
        }
    }

    let connect_start = Instant::now();
    let deadline = connect_start + timeout;
    let addrs: Vec<SocketAddr> = relay_addr
        .to_socket_addrs()
        .map_err(|e| Error::Network(format!("resolve relay addr {relay_addr:?} failed: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::Network(format!(
            "relay addr {relay_addr:?} resolved to no socket addresses"
        )));
    }

    let mut outer_conn = match outer_mtls {
        Some(mtls) => {
            let tls_cfg = build_outer_relay_client_config(mtls)?;
            let server_name = parse_relay_server_name(relay_addr, mtls.server_name.as_deref())?;
            let conn = ClientConnection::new(Arc::new(tls_cfg), server_name).map_err(|e| {
                Error::InvalidConfig(format!("init outer relay mTLS connection: {e}"))
            })?;
            Some(Box::new(conn))
        }
        None => None,
    };

    let mut stream = connect_any_with_deadline(&addrs, deadline, timeout, &cancel)?;
    let peer_addr = stream.peer_addr()?;
    stream.set_nodelay(true)?;
    configure_tcp_keepalive(&stream);

    if cancel.load(Ordering::Acquire) {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(Error::Network("relay connect cancelled".to_string()));
    }

    if let Some(ref mut conn) = outer_conn {
        complete_outer_tls_handshake(&mut stream, conn, deadline, timeout, &cancel)?;
    }

    let rem = remaining_or_timeout(deadline, timeout)?;
    stream.set_write_timeout(Some(rem))?;

    // RCR1 握手帧：[b"RCR1"][role: u8][token_len: u8][token]
    let mut handshake = Vec::with_capacity(6 + token_bytes.len());
    handshake.extend_from_slice(RELAY_MAGIC);
    handshake.push(role as u8);
    handshake.push(token_bytes.len() as u8);
    handshake.extend_from_slice(token_bytes);
    outer_or_tcp_write_all(&mut stream, outer_conn.as_deref_mut(), &handshake, timeout)?;

    // 可中断等待对端配对并读取 1 字节服务器应答。
    let mut reply = [0u8; 1];
    let n_reply = outer_or_tcp_read_some(
        &mut stream,
        outer_conn.as_deref_mut(),
        &mut reply,
        deadline,
        timeout,
        &cancel,
    )?;
    if n_reply == 0 {
        return Err(Error::Network(
            "relay stream hit EOF before completing handshake".to_string(),
        ));
    }
    if reply[0] != REPLY_OK {
        return Err(Error::Network(format!(
            "relay handshake rejected by {peer_addr} (reply=0x{:02x})",
            reply[0]
        )));
    }

    // 记录建连 + 外层 TLS + RCR1 握手 RTT（微秒，下限 1us）用于初始化 HUD / NetStats RTT。
    let rtt_us = (connect_start
        .elapsed()
        .as_micros()
        .min(u128::from(u32::MAX)) as u32)
        .max(1);

    // 可选内层端到端 E2EE TLS 1.3 mTLS 握手。
    let mut initial_plaintext = Vec::new();
    let (inner_conn, peer_identity) = match e2ee_mode {
        RelayE2eeMode::Disabled => (None, None),
        RelayE2eeMode::ClientRequired(bundle) => {
            let (cfg, server_name) = build_e2ee_client_config(&bundle)?;
            let conn = ClientConnection::new(Arc::new(cfg), server_name)
                .map_err(|e| Error::InvalidConfig(format!("init E2EE client tls: {e}")))?;
            let mut inner = E2eeTlsConn::Client(Box::new(conn));
            complete_e2ee_handshake(
                &mut stream,
                outer_conn.as_deref_mut(),
                &mut inner,
                Vec::new(),
                deadline,
                timeout,
                &cancel,
            )?;
            let id = inner.peer_identity();
            (Some(inner), id)
        }
        RelayE2eeMode::HostRequired(bundle) => {
            let cfg = build_e2ee_server_config(&bundle)?;
            let conn = ServerConnection::new(Arc::new(cfg))
                .map_err(|e| Error::InvalidConfig(format!("init E2EE server tls: {e}")))?;
            let mut inner = E2eeTlsConn::Server(Box::new(conn));
            complete_e2ee_handshake(
                &mut stream,
                outer_conn.as_deref_mut(),
                &mut inner,
                Vec::new(),
                deadline,
                timeout,
                &cancel,
            )?;
            let id = inner.peer_identity();
            (Some(inner), id)
        }
        RelayE2eeMode::HostAuto(bundle) => {
            let mut first = [0u8; 1];
            let n = outer_or_tcp_read_some(
                &mut stream,
                outer_conn.as_deref_mut(),
                &mut first,
                deadline,
                timeout,
                &cancel,
            )?;
            if n == 0 {
                return Err(Error::Network(
                    "relay stream hit EOF before receiving client hello or first frame".to_string(),
                ));
            }
            if first[0] == 0x16 {
                let cfg = build_e2ee_server_config(&bundle)?;
                let conn = ServerConnection::new(Arc::new(cfg))
                    .map_err(|e| Error::InvalidConfig(format!("init E2EE server tls: {e}")))?;
                let mut inner = E2eeTlsConn::Server(Box::new(conn));
                complete_e2ee_handshake(
                    &mut stream,
                    outer_conn.as_deref_mut(),
                    &mut inner,
                    vec![0x16],
                    deadline,
                    timeout,
                    &cancel,
                )?;
                let id = inner.peer_identity();
                (Some(inner), id)
            } else if matches!(first[0], CH_DATAGRAM | CH_RELIABLE | CH_RELIABLE_URGENT) {
                initial_plaintext.push(first[0]);
                (None, None)
            } else {
                return Err(Error::Protocol {
                    offset: 0,
                    msg: format!("unexpected first byte 0x{:02x} after RCR1 ack", first[0]),
                });
            }
        }
    };

    // 握手成功：读端不设超时（由 shutdown_handle 中断），写端设有限超时防黑洞卡死。
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(Some(RELAY_WRITE_TIMEOUT))?;

    if cancel.load(Ordering::Acquire) {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(Error::Network("relay connect cancelled".to_string()));
    }

    let read_stream = stream.try_clone()?;
    let shutdown_stream = stream.try_clone()?;
    let write_stream = stream;

    let rx = Arc::new(RxQueue::new(RELAY_RX_QUEUE_CAP, MAX_RX_QUEUE_BYTES));
    let tx = Arc::new(TxQueue::new(
        RELAY_TX_DATAGRAM_CAP,
        RELAY_TX_DATAGRAM_MAX_BYTES,
        RELAY_TX_RELIABLE_CAP,
    ));
    let shutdown_handle = RelayShutdownHandle {
        cancel,
        stream: Arc::new(Mutex::new(Some(shutdown_stream))),
        tx: Some(Arc::clone(&tx)),
    };

    let tls_stack = if outer_conn.is_some() || inner_conn.is_some() {
        Some(Arc::new(Mutex::new(TlsStack {
            outer: outer_conn,
            inner: inner_conn,
        })))
    } else {
        None
    };

    let bytes_sent = Arc::new(AtomicU64::new(0));
    let bytes_recv = Arc::new(AtomicU64::new(0));

    let rx_for_reader = Arc::clone(&rx);
    let tx_for_reader = Arc::clone(&tx);
    let recv_counter = Arc::clone(&bytes_recv);
    let tls_for_reader = tls_stack.clone();
    let reader_thread = thread::Builder::new()
        .name("rc-relay-rx".to_string())
        .spawn(move || {
            if let Some(tls) = tls_for_reader {
                reader_loop_tls(
                    read_stream,
                    tls,
                    initial_plaintext,
                    tx_for_reader,
                    rx_for_reader,
                    recv_counter,
                );
            } else {
                reader_loop_plain(read_stream, initial_plaintext, rx_for_reader, recv_counter);
            }
        })
        .map_err(|e| Error::Network(format!("spawn relay reader thread failed: {e}")))?;

    let tx_for_writer = Arc::clone(&tx);
    let rx_for_writer = Arc::clone(&rx);
    let shutdown_for_writer = shutdown_handle.clone();
    let sent_counter = Arc::clone(&bytes_sent);
    let writer_thread = match thread::Builder::new()
        .name("rc-relay-writer".to_string())
        .spawn(move || {
            writer_loop(
                write_stream,
                tls_stack,
                tx_for_writer,
                rx_for_writer,
                shutdown_for_writer,
                sent_counter,
            );
        }) {
        Ok(handle) => handle,
        Err(e) => {
            rx.mark_shutdown();
            shutdown_handle.shutdown();
            let _ = reader_thread.join();
            return Err(Error::Network(format!(
                "spawn relay writer thread failed: {e}"
            )));
        }
    };

    Ok((
        TcpRelayTransport {
            tx,
            rx,
            shutdown_handle,
            bytes_sent,
            bytes_recv,
            rtt_us,
            peer_addr,
            reader_thread: Some(reader_thread),
            writer_thread: Some(writer_thread),
        },
        peer_identity,
    ))
}

fn writer_loop(
    mut stream: TcpStream,
    tls_stack: Option<Arc<Mutex<TlsStack>>>,
    tx: Arc<TxQueue>,
    rx: Arc<RxQueue>,
    shutdown_handle: RelayShutdownHandle,
    bytes_sent: Arc<AtomicU64>,
) {
    while let Some(item) = tx.pop_next_item() {
        let (wire_bytes, payload_len) = match (&tls_stack, item) {
            (None, TxItem::Frame(frame)) => {
                let len = frame.len().saturating_sub(5) as u64;
                (frame, len)
            }
            (None, TxItem::FlushTls) => continue,
            (Some(tls), TxItem::Frame(frame)) => {
                let len = frame.len().saturating_sub(5) as u64;
                let res = tls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .encrypt_frame(&frame);
                match res {
                    Ok(wire) => (wire, len),
                    Err(err) => {
                        shutdown_handle.shutdown();
                        rx.push_disconnected(format!("relay tls encrypt error: {err}"));
                        return;
                    }
                }
            }
            (Some(tls), TxItem::FlushTls) => {
                let res = tls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .drain_pending_writes();
                match res {
                    Ok(wire) if wire.is_empty() => continue,
                    Ok(wire) => (wire, 0),
                    Err(err) => {
                        shutdown_handle.shutdown();
                        rx.push_disconnected(format!("relay tls flush error: {err}"));
                        return;
                    }
                }
            }
        };

        if let Err(err) = write_all_retry(&mut stream, &wire_bytes) {
            // 任何写错误（含部分写超时）必须立刻切断套接字，避免帧边界错位。
            shutdown_handle.shutdown();
            rx.push_disconnected(format!("relay write error: {err}"));
            return;
        }
        if payload_len > 0 {
            bytes_sent.fetch_add(payload_len, Ordering::Relaxed);
        }
    }
}

#[derive(Debug)]
enum FrameReadError {
    Eof,
    Io(io::Error),
    Protocol(String),
}

/// 从字节流读取一条完整的中继复用帧 `[ch: u8][len: u32 BE][payload]`。
///
/// 使用 `stream.take(len as u64)` 渐进式读入负载，初始容量封顶 64 KiB，
/// 防止恶意或损坏的大长度头在未收到实际数据时一次性分配 4 MiB。
fn read_exact_frame(stream: &mut impl Read) -> std::result::Result<(u8, Vec<u8>), FrameReadError> {
    let mut header = [0u8; 5];
    if let Err(err) = read_exact_retry(stream, &mut header) {
        return if err.kind() == ErrorKind::UnexpectedEof {
            Err(FrameReadError::Eof)
        } else {
            Err(FrameReadError::Io(err))
        };
    }

    let ch = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if !matches!(ch, CH_DATAGRAM | CH_RELIABLE | CH_RELIABLE_URGENT) {
        return Err(FrameReadError::Protocol(format!(
            "relay invalid channel id: {ch}"
        )));
    }
    if len > MAX_RELAY_FRAME_LEN {
        return Err(FrameReadError::Protocol(format!(
            "relay frame length {len} exceeds maximum {MAX_RELAY_FRAME_LEN}"
        )));
    }

    let mut payload = Vec::with_capacity(len.min(INCREMENTAL_READ_INIT_CAP));
    if len > 0 {
        let mut limited = stream.take(len as u64);
        let mut chunk = [0u8; 16 * 1024];
        while payload.len() < len {
            match limited.read(&mut chunk) {
                Ok(0) => {
                    return Err(FrameReadError::Io(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "relay stream hit EOF before completing frame payload",
                    )));
                }
                Ok(n) => payload.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(FrameReadError::Io(e)),
            }
        }
    }

    Ok((ch, payload))
}

fn reader_loop_plain(
    stream: TcpStream,
    initial_plaintext: Vec<u8>,
    rx: Arc<RxQueue>,
    bytes_recv: Arc<AtomicU64>,
) {
    if initial_plaintext.is_empty() {
        let mut stream = stream;
        loop {
            match read_exact_frame(&mut stream) {
                Ok((ch, payload)) => {
                    dispatch_decoded_frame(ch, payload, &rx, &bytes_recv);
                }
                Err(FrameReadError::Eof) => {
                    rx.push_disconnected("relay stream closed by peer".to_string());
                    return;
                }
                Err(FrameReadError::Protocol(reason)) => {
                    rx.push_disconnected(reason);
                    return;
                }
                Err(FrameReadError::Io(err)) => {
                    rx.push_disconnected(format!("relay read error: {err}"));
                    return;
                }
            }
        }
    } else {
        let mut chained = io::Cursor::new(initial_plaintext).chain(stream);
        loop {
            match read_exact_frame(&mut chained) {
                Ok((ch, payload)) => {
                    dispatch_decoded_frame(ch, payload, &rx, &bytes_recv);
                }
                Err(FrameReadError::Eof) => {
                    rx.push_disconnected("relay stream closed by peer".to_string());
                    return;
                }
                Err(FrameReadError::Protocol(reason)) => {
                    rx.push_disconnected(reason);
                    return;
                }
                Err(FrameReadError::Io(err)) => {
                    rx.push_disconnected(format!("relay read error: {err}"));
                    return;
                }
            }
        }
    }
}

fn dispatch_decoded_frame(ch: u8, payload: Vec<u8>, rx: &RxQueue, bytes_recv: &AtomicU64) {
    bytes_recv.fetch_add(payload.len() as u64, Ordering::Relaxed);
    match ch {
        CH_DATAGRAM => rx.push_datagram(payload),
        CH_RELIABLE => rx.push_reliable(NetEvent::Reliable(payload)),
        CH_RELIABLE_URGENT => rx.push_reliable(NetEvent::ReliableUrgent(payload)),
        _ => unreachable!("channel validated in read_exact_frame"),
    }
}

fn drain_plain_frames(
    plain_buf: &mut Vec<u8>,
    read_pos: &mut usize,
    rx: &RxQueue,
    bytes_recv: &AtomicU64,
) -> std::result::Result<(), String> {
    loop {
        let avail = &plain_buf[*read_pos..];
        if avail.len() < 5 {
            break;
        }
        let ch = avail[0];
        let len = u32::from_be_bytes([avail[1], avail[2], avail[3], avail[4]]) as usize;
        if !matches!(ch, CH_DATAGRAM | CH_RELIABLE | CH_RELIABLE_URGENT) {
            return Err(format!("relay invalid channel id: {ch}"));
        }
        if len > MAX_RELAY_FRAME_LEN {
            return Err(format!(
                "relay frame length {len} exceeds maximum {MAX_RELAY_FRAME_LEN}"
            ));
        }
        let total = 5 + len;
        if avail.len() < total {
            break;
        }
        let payload = avail[5..total].to_vec();
        *read_pos += total;
        dispatch_decoded_frame(ch, payload, rx, bytes_recv);
    }
    if *read_pos > 0 {
        if *read_pos >= plain_buf.len() {
            plain_buf.clear();
            *read_pos = 0;
        } else if *read_pos >= 64 * 1024 {
            plain_buf.drain(..*read_pos);
            *read_pos = 0;
        }
    }
    Ok(())
}

fn reader_loop_tls(
    mut stream: TcpStream,
    tls: Arc<Mutex<TlsStack>>,
    initial_plaintext: Vec<u8>,
    tx: Arc<TxQueue>,
    rx: Arc<RxQueue>,
    bytes_recv: Arc<AtomicU64>,
) {
    let mut plain_buf = initial_plaintext;
    let mut read_pos = 0usize;
    let mut net_buf = vec![0u8; 32 * 1024];

    // 先排空握手结束时 TLS 缓冲中可能已携带的首批明文载荷（无需额外 TCP 读）。
    let init_res = tls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain_buffered_plaintext(&mut plain_buf);
    match init_res {
        Ok((wants_write, init_eof)) => {
            if wants_write {
                tx.request_flush();
            }
            if let Err(reason) = drain_plain_frames(&mut plain_buf, &mut read_pos, &rx, &bytes_recv)
            {
                rx.push_disconnected(reason);
                return;
            }
            if init_eof {
                if read_pos < plain_buf.len() {
                    rx.push_disconnected(
                        "relay stream hit EOF before completing frame".to_string(),
                    );
                } else {
                    rx.push_disconnected("relay stream closed by peer".to_string());
                }
                return;
            }
        }
        Err(err) => {
            rx.push_disconnected(format!("relay tls read error: {err}"));
            return;
        }
    }

    loop {
        // 绝不持有 tls 锁阻塞在 TcpStream::read() 上，保证写线程零阻塞全双工转发。
        let n = match stream.read(&mut net_buf) {
            Ok(0) => {
                if read_pos < plain_buf.len() {
                    rx.push_disconnected(
                        "relay stream hit EOF before completing frame".to_string(),
                    );
                } else {
                    rx.push_disconnected("relay stream closed by peer".to_string());
                }
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                rx.push_disconnected(format!("relay read error: {e}"));
                return;
            }
        };

        let decrypt_res = tls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decrypt_chunk(&net_buf[..n], &mut plain_buf);
        match decrypt_res {
            Ok((wants_write, eof)) => {
                if wants_write {
                    tx.request_flush();
                }
                if let Err(reason) =
                    drain_plain_frames(&mut plain_buf, &mut read_pos, &rx, &bytes_recv)
                {
                    rx.push_disconnected(reason);
                    return;
                }
                if eof {
                    if read_pos < plain_buf.len() {
                        rx.push_disconnected(
                            "relay stream hit EOF before completing frame".to_string(),
                        );
                    } else {
                        rx.push_disconnected("relay stream closed by peer".to_string());
                    }
                    return;
                }
            }
            Err(err) => {
                rx.push_disconnected(format!("relay tls read error: {err}"));
                return;
            }
        }
    }
}

fn per_addr_timeout(remaining: Duration, remaining_addrs: usize) -> Duration {
    let count = remaining_addrs.max(1) as u32;
    (remaining / count).max(MIN_ADDR_TIMEOUT).min(remaining)
}

fn configure_tcp_keepalive(stream: &TcpStream) {
    let sock_ref = SockRef::from(stream);
    let _ = sock_ref.set_keepalive(true);
    let ka = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    let _ = sock_ref.set_tcp_keepalive(&ka);
}

enum SingleConnectErr {
    Cancelled,
    TimedOut,
    Io(io::Error),
}

fn is_connect_in_progress(err: &io::Error) -> bool {
    if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) {
        return true;
    }
    #[cfg(target_os = "macos")]
    if matches!(err.raw_os_error(), Some(36 | 37)) {
        return true;
    }
    #[cfg(target_os = "linux")]
    if matches!(err.raw_os_error(), Some(115 | 114)) {
        return true;
    }
    false
}

fn connect_single_cancellable(
    addr: SocketAddr,
    timeout: Duration,
    cancel: &AtomicBool,
) -> std::result::Result<TcpStream, SingleConnectErr> {
    if cancel.load(Ordering::Acquire) {
        return Err(SingleConnectErr::Cancelled);
    }

    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(SingleConnectErr::Io)?;
    socket.set_nonblocking(true).map_err(SingleConnectErr::Io)?;

    let sock_addr = SockAddr::from(addr);
    match socket.connect(&sock_addr) {
        Ok(()) => {}
        Err(err) if is_connect_in_progress(&err) => {
            let deadline = Instant::now() + timeout;
            let mut sleep_ms = 5u64;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(SingleConnectErr::Cancelled);
                }
                if let Some(so_err) = socket.take_error().map_err(SingleConnectErr::Io)? {
                    return Err(SingleConnectErr::Io(so_err));
                }
                match socket.peer_addr() {
                    Ok(_) => {
                        if let Some(so_err) = socket.take_error().map_err(SingleConnectErr::Io)? {
                            return Err(SingleConnectErr::Io(so_err));
                        }
                        break;
                    }
                    Err(err) => {
                        if let Some(so_err) = socket.take_error().map_err(SingleConnectErr::Io)? {
                            return Err(SingleConnectErr::Io(so_err));
                        }
                        if !matches!(
                            err.kind(),
                            ErrorKind::NotConnected
                                | ErrorKind::WouldBlock
                                | ErrorKind::Interrupted
                        ) {
                            return Err(SingleConnectErr::Io(err));
                        }
                    }
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(SingleConnectErr::TimedOut);
                }
                let step = Duration::from_millis(sleep_ms).min(deadline - now);
                thread::sleep(step);
                sleep_ms = (sleep_ms * 2).min(50);
            }
        }
        Err(err) => return Err(SingleConnectErr::Io(err)),
    }

    socket
        .set_nonblocking(false)
        .map_err(SingleConnectErr::Io)?;
    Ok(socket.into())
}

fn connect_any_with_deadline(
    addrs: &[SocketAddr],
    deadline: Instant,
    total_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<TcpStream> {
    let mut last_err: Option<io::Error> = None;
    let mut timed_out_any = false;
    for (idx, &addr) in addrs.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Network("relay connect cancelled".to_string()));
        }
        let rem = remaining_or_timeout(deadline, total_timeout)?;
        let budget = per_addr_timeout(rem, addrs.len() - idx);
        match connect_single_cancellable(addr, budget, cancel) {
            Ok(stream) => return Ok(stream),
            Err(SingleConnectErr::Cancelled) => {
                return Err(Error::Network("relay connect cancelled".to_string()));
            }
            Err(SingleConnectErr::TimedOut) => {
                timed_out_any = true;
            }
            Err(SingleConnectErr::Io(err)) => last_err = Some(err),
        }
    }
    match last_err {
        Some(err) if matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
            Err(Error::Timeout(total_timeout))
        }
        Some(err) => Err(Error::Network(format!("connect relay failed: {err}"))),
        None if timed_out_any => Err(Error::Timeout(total_timeout)),
        None => Err(Error::Network("no relay addresses to connect".to_string())),
    }
}

fn remaining_or_timeout(deadline: Instant, total_timeout: Duration) -> Result<Duration> {
    let now = Instant::now();
    if now >= deadline {
        Err(Error::Timeout(total_timeout))
    } else {
        Ok(deadline - now)
    }
}

fn map_handshake_io(err: io::Error, total_timeout: Duration) -> Error {
    if matches!(err.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) {
        Error::Timeout(total_timeout)
    } else {
        Error::Network(format!("relay handshake I/O error: {err}"))
    }
}

fn write_all_retry(stream: &mut TcpStream, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match stream.write(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "failed to write whole buffer to relay stream",
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_exact_retry(stream: &mut impl Read, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match stream.read(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "relay stream hit EOF before completing frame",
                ));
            }
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    /// 本地模拟 `RCR1` 中继服务器：验证双方握手令牌与角色，回复 `0x00` 后双向透明转发。
    fn spawn_mock_relay_pair_server(expected_token: String) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = thread::spawn(move || {
            let (mut s1, _) = listener.accept().unwrap();
            let (mut s2, _) = listener.accept().unwrap();
            let _ = s1.set_nodelay(true);
            let _ = s2.set_nodelay(true);

            let r1 = read_rcr1_handshake(&mut s1, &expected_token);
            let r2 = read_rcr1_handshake(&mut s2, &expected_token);
            assert_ne!(r1, r2, "host and client must use complementary roles");
            assert!(matches!(r1, 1 | 2) && matches!(r2, 1 | 2));

            s1.write_all(&[REPLY_OK]).unwrap();
            s2.write_all(&[REPLY_OK]).unwrap();

            let mut s1_r = s1.try_clone().unwrap();
            let mut s2_w = s2.try_clone().unwrap();
            let t1 = thread::spawn(move || {
                let _ = io::copy(&mut s1_r, &mut s2_w);
                let _ = s2_w.shutdown(Shutdown::Write);
            });
            let t2 = thread::spawn(move || {
                let _ = io::copy(&mut s2, &mut s1);
                let _ = s1.shutdown(Shutdown::Write);
            });
            let _ = t1.join();
            let _ = t2.join();
        });

        (addr, handle)
    }

    fn read_rcr1_handshake(stream: &mut TcpStream, expected_token: &str) -> u8 {
        let mut head = [0u8; 6];
        stream.read_exact(&mut head).unwrap();
        assert_eq!(&head[0..4], b"RCR1");
        let role = head[4];
        let token_len = head[5] as usize;
        let mut token = vec![0u8; token_len];
        stream.read_exact(&mut token).unwrap();
        assert_eq!(std::str::from_utf8(&token).unwrap(), expected_token);
        role
    }

    fn collect_until(
        transport: &mut TcpRelayTransport,
        expected: usize,
        deadline: Instant,
    ) -> Vec<NetEvent> {
        let mut out = Vec::new();
        while out.len() < expected && Instant::now() < deadline {
            let mut batch = transport.poll(Duration::from_millis(50)).unwrap();
            out.append(&mut batch);
        }
        out
    }

    #[test]
    fn relay_loopback_rcr1_and_all_three_channels_roundtrip() {
        let token = "9f8e7d6c5b4a3210".to_string();
        let (relay_addr, server_handle) = spawn_mock_relay_pair_server(token.clone());
        let addr_str = relay_addr.to_string();

        let addr_for_host = addr_str.clone();
        let token_for_host = token.clone();
        let host_thread = thread::spawn(move || {
            TcpRelayTransport::connect(
                &addr_for_host,
                RelayRole::Host,
                &token_for_host,
                Duration::from_secs(3),
            )
            .expect("host relay connect must succeed")
        });

        let mut client = TcpRelayTransport::connect(
            &addr_str,
            RelayRole::Client,
            &token,
            Duration::from_secs(3),
        )
        .expect("client relay connect must succeed");
        let mut host = host_thread.join().unwrap();

        assert_eq!(client.peer_addr(), relay_addr);
        assert_eq!(host.peer_addr(), relay_addr);
        // L3: RTT 必须在建连 + RCR1 握手后初始化为 >= 1us
        assert!(client.stats().rtt_us >= 1);
        assert!(host.stats().rtt_us >= 1);

        // Host -> Client: Datagram + Reliable + ReliableUrgent
        host.send_datagram(b"video-shard-1").unwrap();
        host.send_reliable(b"control-welcome").unwrap();
        host.send_reliable_urgent(b"urgent-sync").unwrap();

        let got = collect_until(&mut client, 3, Instant::now() + Duration::from_secs(2));
        assert_eq!(got.len(), 3);
        assert!(got.contains(&NetEvent::Datagram(b"video-shard-1".to_vec())));
        assert!(got.contains(&NetEvent::Reliable(b"control-welcome".to_vec())));
        assert!(got.contains(&NetEvent::ReliableUrgent(b"urgent-sync".to_vec())));

        // Client -> Host: ReliableUrgent input + Reliable control
        client.send_reliable_urgent(b"key-down-a").unwrap();
        client.send_reliable(b"set-bitrate").unwrap();
        let host_got = collect_until(&mut host, 2, Instant::now() + Duration::from_secs(2));
        assert_eq!(host_got.len(), 2);
        assert!(host_got.contains(&NetEvent::ReliableUrgent(b"key-down-a".to_vec())));
        assert!(host_got.contains(&NetEvent::Reliable(b"set-bitrate".to_vec())));

        let expected_h2c =
            (b"video-shard-1".len() + b"control-welcome".len() + b"urgent-sync".len()) as u64;
        let expected_c2h = (b"key-down-a".len() + b"set-bitrate".len()) as u64;
        // 后台写线程更新 bytes_sent 在 write 返回后立即可见，给极短暂让出以消除调度竞态
        let deadline = Instant::now() + Duration::from_millis(200);
        while (host.stats().bytes_sent < expected_h2c || client.stats().bytes_sent < expected_c2h)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(host.stats().bytes_sent, expected_h2c);
        assert_eq!(client.stats().bytes_recv, expected_h2c);
        assert_eq!(client.stats().bytes_sent, expected_c2h);
        assert_eq!(host.stats().bytes_recv, expected_c2h);

        // Closing host must surface Disconnected on client.
        drop(host);
        let disconnect_events =
            collect_until(&mut client, 1, Instant::now() + Duration::from_secs(2));
        assert!(
            disconnect_events
                .iter()
                .any(|e| matches!(e, NetEvent::Disconnected { .. })),
            "expected Disconnected event after peer drop, got {disconnect_events:?}"
        );

        drop(client);
        let _ = server_handle.join();
    }

    #[test]
    fn relay_rejects_error_reply_and_invalid_token() {
        let err = TcpRelayTransport::connect(
            "127.0.0.1:21117",
            RelayRole::Client,
            "",
            Duration::from_millis(200),
        )
        .err()
        .expect("empty token must fail");
        assert!(matches!(err, Error::InvalidConfig(_)));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 10];
            let _ = s.read_exact(&mut buf);
            let _ = s.write_all(&[0x01]);
        });

        let res = TcpRelayTransport::connect(
            &addr.to_string(),
            RelayRole::Client,
            "tok1",
            Duration::from_secs(2),
        );
        assert!(res.is_err());
        let _ = server.join();
    }

    #[test]
    fn relay_connect_cancellable_aborts_waiting_for_rcr1_ack_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut head = [0u8; 10];
            let _ = s.read_exact(&mut head);
            let _ = accepted_tx.send(());
            // 模拟对端一直未配对：长时间不回复 1 字节 ACK
            let mut sink = [0u8; 8];
            let _ = s.read(&mut sink);
        });

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_trigger = Arc::clone(&cancel);
        let killer = thread::spawn(move || {
            let _ = accepted_rx.recv_timeout(Duration::from_secs(2));
            thread::sleep(Duration::from_millis(40));
            cancel_trigger.store(true, Ordering::Release);
        });

        let start = Instant::now();
        let res = TcpRelayTransport::connect_cancellable(
            &addr.to_string(),
            RelayRole::Client,
            "tok1",
            Duration::from_secs(10),
            cancel,
        );
        let elapsed = start.elapsed();
        assert!(res.is_err());
        assert!(
            elapsed < Duration::from_secs(2),
            "cancellable relay connect should abort promptly, took {elapsed:?}"
        );

        let _ = killer.join();
        let _ = server.join();
    }

    #[test]
    fn relay_shutdown_handle_unblocks_running_transport_and_drop_is_poison_safe() {
        let token = "shutdown-tok-01".to_string();
        let (relay_addr, server_handle) = spawn_mock_relay_pair_server(token.clone());
        let addr_str = relay_addr.to_string();

        let addr_h = addr_str.clone();
        let tok_h = token.clone();
        let host_thread = thread::spawn(move || {
            TcpRelayTransport::connect(&addr_h, RelayRole::Host, &tok_h, Duration::from_secs(3))
                .unwrap()
        });
        let mut client = TcpRelayTransport::connect(
            &addr_str,
            RelayRole::Client,
            &token,
            Duration::from_secs(3),
        )
        .unwrap();
        let host = host_thread.join().unwrap();

        // 人为制造 stream 锁中毒，验证 shutdown_handle & Drop 均不会 panic
        let handle = client.shutdown_handle();
        let poison_stream = Arc::clone(&handle.stream);
        let _ = thread::spawn(move || {
            let _g = poison_stream.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        handle.shutdown();
        let events = collect_until(&mut client, 1, Instant::now() + Duration::from_secs(2));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, NetEvent::Disconnected { .. })),
            "shutdown_handle.shutdown() should unblock reader with Disconnected, got {events:?}"
        );
        assert!(client.send_reliable(b"after-shutdown").is_err());

        drop(client);
        drop(host);
        let _ = server_handle.join();
    }

    #[test]
    fn relay_tx_queue_prioritizes_urgent_over_reliable_and_bounds_datagrams() {
        let q = TxQueue::new(3, 64 * 1024, 8);
        for i in 0u8..5 {
            q.enqueue_datagram(vec![CH_DATAGRAM, 0, 0, 0, 1, i])
                .unwrap();
        }
        q.enqueue_reliable(vec![CH_RELIABLE, 0, 0, 0, 1, 10])
            .unwrap();
        q.enqueue_urgent(vec![CH_RELIABLE_URGENT, 0, 0, 0, 1, 99])
            .unwrap();
        q.mark_closed();

        let mut popped = Vec::new();
        while let Some(frame) = q.pop_next() {
            popped.push((frame[0], frame[5]));
        }
        assert_eq!(
            popped,
            vec![
                (CH_RELIABLE_URGENT, 99),
                (CH_RELIABLE, 10),
                // Datagram 上限 3，最早的 0 与 1 被丢弃
                (CH_DATAGRAM, 2),
                (CH_DATAGRAM, 3),
                (CH_DATAGRAM, 4),
            ]
        );

        // 字节上限淘汰：10B 上限下连续入队 3 个 4B datagram 只留后 2 个
        let qb = TxQueue::new(64, 10, 8);
        qb.enqueue_datagram(vec![1; 4]).unwrap();
        qb.enqueue_datagram(vec![2; 4]).unwrap();
        qb.enqueue_datagram(vec![3; 4]).unwrap();
        qb.mark_closed();
        let mut popped_b = Vec::new();
        while let Some(frame) = qb.pop_next() {
            popped_b.push(frame[0]);
        }
        assert_eq!(popped_b, vec![2, 3]);
    }

    #[test]
    fn relay_tx_queue_holds_large_idr_400_datagrams_without_dropping() {
        let q = TxQueue::new(
            RELAY_TX_DATAGRAM_CAP,
            RELAY_TX_DATAGRAM_MAX_BYTES,
            RELAY_TX_RELIABLE_CAP,
        );
        for seq in 0u16..400 {
            let mut pkt = vec![0u8; 1200];
            pkt[0] = (seq & 0xFF) as u8;
            pkt[1] = (seq >> 8) as u8;
            q.enqueue_datagram(pkt).unwrap();
        }
        q.mark_closed();

        let mut count = 0u16;
        while let Some(pkt) = q.pop_next() {
            let seq = u16::from(pkt[0]) | (u16::from(pkt[1]) << 8);
            assert_eq!(seq, count, "大帧 IDR 分片应按序无丢包弹出");
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn relay_rx_queue_drops_oldest_datagram_when_full_by_count_or_bytes() {
        let q = RxQueue::new(4, MAX_RX_QUEUE_BYTES);
        q.push_reliable(NetEvent::Reliable(b"ctrl".to_vec()));
        for i in 0u8..6 {
            q.push_datagram(vec![i]);
        }
        let events = q.poll(Duration::ZERO);
        assert_eq!(
            events,
            vec![
                NetEvent::Reliable(b"ctrl".to_vec()),
                NetEvent::Datagram(vec![3]),
                NetEvent::Datagram(vec![4]),
                NetEvent::Datagram(vec![5]),
            ]
        );

        // 字节上限测试：max_bytes = 10，每条 datagram 4 字节，最多留 2 条（8 字节）
        let q_bytes = RxQueue::new(64, 10);
        q_bytes.push_datagram(vec![1; 4]);
        q_bytes.push_datagram(vec![2; 4]);
        q_bytes.push_datagram(vec![3; 4]);
        let evs = q_bytes.poll(Duration::ZERO);
        assert_eq!(
            evs,
            vec![
                NetEvent::Datagram(vec![2; 4]),
                NetEvent::Datagram(vec![3; 4])
            ]
        );
    }

    #[test]
    fn read_exact_frame_reads_incrementally_and_rejects_truncated_large_frame() {
        // 合法帧读取
        let mut wire = Vec::new();
        wire.push(CH_RELIABLE_URGENT);
        wire.extend_from_slice(&(5u32).to_be_bytes());
        wire.extend_from_slice(b"hello");
        let mut cursor = Cursor::new(wire);
        let (ch, payload) = read_exact_frame(&mut cursor).unwrap();
        assert_eq!(ch, CH_RELIABLE_URGENT);
        assert_eq!(payload, b"hello");

        // 声明 4 MiB 但仅给 4 字节后 EOF：渐进读取应立刻返回 Io(UnexpectedEof) 而不 OOM
        let mut truncated = Vec::new();
        truncated.push(CH_DATAGRAM);
        truncated.extend_from_slice(&(MAX_RELAY_FRAME_LEN as u32).to_be_bytes());
        truncated.extend_from_slice(b"abcd");
        let mut cursor = Cursor::new(truncated);
        match read_exact_frame(&mut cursor) {
            Err(FrameReadError::Io(e)) => assert_eq!(e.kind(), ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }

        // 超限长度应返回 Protocol 错误
        let mut huge = Vec::new();
        huge.push(CH_DATAGRAM);
        huge.extend_from_slice(&((MAX_RELAY_FRAME_LEN as u32) + 1).to_be_bytes());
        let mut cursor = Cursor::new(huge);
        assert!(matches!(
            read_exact_frame(&mut cursor),
            Err(FrameReadError::Protocol(_))
        ));
    }

    #[test]
    fn per_addr_timeout_divides_budget_fairly() {
        assert_eq!(
            per_addr_timeout(Duration::from_secs(10), 2),
            Duration::from_secs(5)
        );
        assert_eq!(
            per_addr_timeout(Duration::from_millis(300), 2),
            Duration::from_millis(250)
        );
        assert_eq!(
            per_addr_timeout(Duration::from_millis(120), 2),
            Duration::from_millis(120)
        );
    }

    pub(crate) mod mtls_test_certs {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;
        use ring::rand::SystemRandom;
        use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

        const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
        const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
        const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
        const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

        fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(body.len() + 4);
            out.push(tag);
            if body.len() < 128 {
                out.push(body.len() as u8);
            } else if body.len() <= 0xFF {
                out.push(0x81);
                out.push(body.len() as u8);
            } else {
                out.push(0x82);
                out.push((body.len() >> 8) as u8);
                out.push((body.len() & 0xFF) as u8);
            }
            out.extend_from_slice(body);
            out
        }

        fn der_sequence(body: &[u8]) -> Vec<u8> {
            der_tlv(0x30, body)
        }

        fn der_oid(oid: &[u8]) -> Vec<u8> {
            der_tlv(0x06, oid)
        }

        fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
            let mut body = Vec::with_capacity(bytes.len() + 1);
            body.push(0x00);
            body.extend_from_slice(bytes);
            der_tlv(0x03, &body)
        }

        fn encode_pem(label: &str, der: &[u8]) -> String {
            let b64 = B64.encode(der);
            let mut pem = format!("-----BEGIN {label}-----\n");
            for chunk in b64.as_bytes().chunks(64) {
                pem.push_str(std::str::from_utf8(chunk).unwrap());
                pem.push('\n');
            }
            pem.push_str(&format!("-----END {label}-----\n"));
            pem
        }

        fn build_ec_spki_der(uncompressed_pub: &[u8]) -> Vec<u8> {
            let alg_body = [der_oid(OID_EC_PUBLIC_KEY), der_oid(OID_PRIME256V1)].concat();
            let spki = [der_sequence(&alg_body), der_bit_string(uncompressed_pub)].concat();
            der_sequence(&spki)
        }

        fn build_cn_dn_der(cn: &str) -> Vec<u8> {
            let atv = [der_oid(OID_COMMON_NAME), der_tlv(0x0C, cn.as_bytes())].concat();
            der_sequence(&der_tlv(0x31, &der_sequence(&atv)))
        }

        fn build_ca_extensions_der() -> Vec<u8> {
            let bc = [
                der_oid(&[0x55, 0x1D, 0x13]),
                der_tlv(0x01, &[0xFF]),
                der_tlv(0x04, &der_sequence(&der_tlv(0x01, &[0xFF]))),
            ]
            .concat();
            let ku = [
                der_oid(&[0x55, 0x1D, 0x0F]),
                der_tlv(0x01, &[0xFF]),
                der_tlv(0x04, &der_tlv(0x03, &[0x01, 0x86])),
            ]
            .concat();
            der_tlv(
                0xA3,
                &der_sequence(&[der_sequence(&bc), der_sequence(&ku)].concat()),
            )
        }

        fn build_leaf_extensions_der(san_entries: &[Vec<u8>]) -> Vec<u8> {
            let bc = [
                der_oid(&[0x55, 0x1D, 0x13]),
                der_tlv(0x04, &der_sequence(&[])),
            ]
            .concat();
            let ku = [
                der_oid(&[0x55, 0x1D, 0x0F]),
                der_tlv(0x01, &[0xFF]),
                der_tlv(0x04, &der_tlv(0x03, &[0x05, 0xA0])),
            ]
            .concat();
            let eku_oids = [
                der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]),
                der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02]),
            ]
            .concat();
            let eku = [
                der_oid(&[0x55, 0x1D, 0x25]),
                der_tlv(0x04, &der_sequence(&eku_oids)),
            ]
            .concat();
            let san = [
                der_oid(&[0x55, 0x1D, 0x11]),
                der_tlv(0x04, &der_sequence(&san_entries.concat())),
            ]
            .concat();
            der_tlv(
                0xA3,
                &der_sequence(
                    &[
                        der_sequence(&bc),
                        der_sequence(&ku),
                        der_sequence(&eku),
                        der_sequence(&san),
                    ]
                    .concat(),
                ),
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn sign_x509_v3(
            ca_kp: &EcdsaKeyPair,
            serial: &[u8],
            issuer_dn: &[u8],
            subject_dn: &[u8],
            spki_der: &[u8],
            exts: &[u8],
        ) -> Vec<u8> {
            let sig_alg = der_sequence(&der_oid(OID_ECDSA_WITH_SHA256));
            let validity = der_sequence(
                &[
                    der_tlv(0x17, b"250101000000Z"),
                    der_tlv(0x17, b"350101000000Z"),
                ]
                .concat(),
            );
            let mut tbs_body = Vec::new();
            tbs_body.extend_from_slice(&der_tlv(0xA0, &der_tlv(0x02, &[0x02])));
            tbs_body.extend_from_slice(&der_tlv(0x02, serial));
            tbs_body.extend_from_slice(&sig_alg);
            tbs_body.extend_from_slice(issuer_dn);
            tbs_body.extend_from_slice(&validity);
            tbs_body.extend_from_slice(subject_dn);
            tbs_body.extend_from_slice(spki_der);
            tbs_body.extend_from_slice(exts);
            let tbs = der_sequence(&tbs_body);
            let sig = ca_kp.sign(&SystemRandom::new(), &tbs).unwrap();
            let cert_body = [tbs, sig_alg, der_bit_string(sig.as_ref())].concat();
            der_sequence(&cert_body)
        }

        pub struct TestCa {
            pub ca_pem: String,
            ca_dn: Vec<u8>,
            ca_kp: EcdsaKeyPair,
            next_serial: u8,
        }

        impl TestCa {
            pub fn new(cn: &str) -> Self {
                let rng = SystemRandom::new();
                let pk8 =
                    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
                let ca_kp =
                    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng)
                        .unwrap();
                let ca_spki = build_ec_spki_der(ca_kp.public_key().as_ref());
                let ca_dn = build_cn_dn_der(cn);
                let ca_der = sign_x509_v3(
                    &ca_kp,
                    &[0x01],
                    &ca_dn,
                    &ca_dn,
                    &ca_spki,
                    &build_ca_extensions_der(),
                );
                Self {
                    ca_pem: encode_pem("CERTIFICATE", &ca_der),
                    ca_dn,
                    ca_kp,
                    next_serial: 2,
                }
            }

            pub fn issue_device(&mut self, user_id: &str, device_id: &str) -> (String, String) {
                let rng = SystemRandom::new();
                let pk8 =
                    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
                let kp =
                    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng)
                        .unwrap();
                let spki = build_ec_spki_der(kp.public_key().as_ref());
                let dn = build_cn_dn_der(&format!("rc-device-{device_id}"));
                let san_uri = format!("urn:rabbitcontrol:user:{user_id}:device:{device_id}");
                let san_dns = format!("device-{device_id}.rc.internal");
                let serial = self.next_serial;
                self.next_serial += 1;
                let cert_der = sign_x509_v3(
                    &self.ca_kp,
                    &[serial],
                    &self.ca_dn,
                    &dn,
                    &spki,
                    &build_leaf_extensions_der(&[
                        der_tlv(0x86, san_uri.as_bytes()),
                        der_tlv(0x82, san_dns.as_bytes()),
                        der_tlv(0x87, &[127, 0, 0, 1]),
                    ]),
                );
                (
                    encode_pem("CERTIFICATE", &cert_der),
                    encode_pem("PRIVATE KEY", pk8.as_ref()),
                )
            }

            pub fn issue_server_for_loopback(&mut self) -> (String, String) {
                let rng = SystemRandom::new();
                let pk8 =
                    EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
                let kp =
                    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng)
                        .unwrap();
                let spki = build_ec_spki_der(kp.public_key().as_ref());
                let dn = build_cn_dn_der("127.0.0.1");
                let serial = self.next_serial;
                self.next_serial += 1;
                let cert_der = sign_x509_v3(
                    &self.ca_kp,
                    &[serial],
                    &self.ca_dn,
                    &dn,
                    &spki,
                    &build_leaf_extensions_der(&[
                        der_tlv(0x82, b"localhost"),
                        der_tlv(0x87, &[127, 0, 0, 1]),
                    ]),
                );
                (
                    encode_pem("CERTIFICATE", &cert_der),
                    encode_pem("PRIVATE KEY", pk8.as_ref()),
                )
            }
        }
    }

    fn build_mock_mtls_relay_server_config(
        ca_pem: &str,
        srv_cert_pem: &str,
        srv_key_pem: &str,
    ) -> Arc<rustls::ServerConfig> {
        let mut root_store = rustls::RootCertStore::empty();
        for cert in Identity::certs_from_pem(ca_pem).unwrap() {
            root_store.add(cert).unwrap();
        }
        let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(root_store),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        let certs = Identity::certs_from_pem(srv_cert_pem).unwrap();
        let key = rustls_pemfile::private_key(&mut srv_key_pem.as_bytes())
            .unwrap()
            .unwrap();
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .unwrap();
        Arc::new(cfg)
    }

    fn spawn_mock_mtls_relay_server(
        server_cfg: Arc<rustls::ServerConfig>,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut conns: Vec<(
                u8,
                String,
                rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
            )> = Vec::new();
            for _ in 0..2 {
                let (tcp, _) = listener.accept().unwrap();
                let conn = rustls::ServerConnection::new(Arc::clone(&server_cfg)).unwrap();
                let mut stream = rustls::StreamOwned::new(conn, tcp);
                let mut hdr = [0u8; 6];
                if stream.read_exact(&mut hdr).is_err() {
                    return;
                }
                assert_eq!(&hdr[..4], b"RCR1");
                let role = hdr[4];
                let tok_len = usize::from(hdr[5]);
                let mut tok = vec![0u8; tok_len];
                stream.read_exact(&mut tok).unwrap();
                let tok_s = String::from_utf8(tok).unwrap();
                conns.push((role, tok_s, stream));
            }
            assert_eq!(conns[0].1, conns[1].1);
            for (_, _, s) in &mut conns {
                s.write_all(&[0x00]).unwrap();
                s.flush().unwrap();
            }
            let (mut s0, mut s1) = {
                let mut it = conns.into_iter();
                (it.next().unwrap().2, it.next().unwrap().2)
            };
            let s0_tcp = s0.sock.try_clone().unwrap();
            let s1_tcp = s1.sock.try_clone().unwrap();
            // 简单轮询双向转发，使用 StreamOwned（各自独占一端）需拆半工；改用直接读取每条可用记录转发太复杂，
            // 这里设置短 read_timeout 轮流读取并转发。
            s0.sock
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            s1.sock
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            let f = thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let mut closed0 = false;
                let mut closed1 = false;
                let deadline = Instant::now() + Duration::from_secs(5);
                while !(closed0 && closed1) && Instant::now() < deadline {
                    if !closed0 {
                        match s0.read(&mut buf) {
                            Ok(0) => {
                                closed0 = true;
                                let _ = s1_tcp.shutdown(Shutdown::Write);
                            }
                            Ok(n) => {
                                if s1.write_all(&buf[..n]).is_err() || s1.flush().is_err() {
                                    break;
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::TimedOut
                                ) => {}
                            Err(_) => closed0 = true,
                        }
                    }
                    if !closed1 {
                        match s1.read(&mut buf) {
                            Ok(0) => {
                                closed1 = true;
                                let _ = s0_tcp.shutdown(Shutdown::Write);
                            }
                            Ok(n) => {
                                if s0.write_all(&buf[..n]).is_err() || s0.flush().is_err() {
                                    break;
                                }
                            }
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    ErrorKind::WouldBlock | ErrorKind::TimedOut
                                ) => {}
                            Err(_) => closed1 = true,
                        }
                    }
                }
            });
            let _ = f.join();
        });
        (addr, handle)
    }

    fn wait_for_event(transport: &mut impl Transport, want: &NetEvent) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            for ev in transport.poll(Duration::from_millis(20)).unwrap() {
                if &ev == want {
                    return;
                }
            }
        }
        panic!("timed out waiting for NetEvent: {want:?}");
    }

    #[test]
    fn connect_mtls_cancellable_roundtrip_and_rejects_untrusted_client() {
        let mut ca = mtls_test_certs::TestCa::new("RelayTestCA");
        let (srv_cert, srv_key) = ca.issue_server_for_loopback();
        let (c_cert, c_key) = ca.issue_device("u1", "111111111");
        let (h_cert, h_key) = ca.issue_device("u1", "222222222");
        let server_cfg = build_mock_mtls_relay_server_config(&ca.ca_pem, &srv_cert, &srv_key);
        let (addr, srv_jh) = spawn_mock_mtls_relay_server(Arc::clone(&server_cfg));

        let c_mtls = RelayMtlsConfig {
            ca_cert_pem: ca.ca_pem.clone(),
            client_cert_pem: c_cert,
            client_key_pem: c_key,
            server_name: None,
        };
        let h_mtls = RelayMtlsConfig {
            ca_cert_pem: ca.ca_pem.clone(),
            client_cert_pem: h_cert,
            client_key_pem: h_key,
            server_name: None,
        };

        let addr_s = addr.to_string();
        let addr_s2 = addr_s.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_h = Arc::clone(&cancel);
        let h_jh = thread::spawn(move || {
            TcpRelayTransport::connect_mtls_cancellable(
                &addr_s2,
                RelayRole::Host,
                "mtls-tok-1",
                Duration::from_secs(3),
                cancel_h,
                &h_mtls,
            )
            .unwrap()
        });
        let mut client_t = TcpRelayTransport::connect_mtls_cancellable(
            &addr_s,
            RelayRole::Client,
            "mtls-tok-1",
            Duration::from_secs(3),
            cancel,
            &c_mtls,
        )
        .unwrap();
        let mut host_t = h_jh.join().unwrap();

        client_t.send_reliable(b"ping-mtls").unwrap();
        wait_for_event(&mut host_t, &NetEvent::Reliable(b"ping-mtls".to_vec()));
        host_t.send_datagram(b"pong-mtls").unwrap();
        wait_for_event(&mut client_t, &NetEvent::Datagram(b"pong-mtls".to_vec()));

        drop(client_t);
        drop(host_t);
        let _ = srv_jh.join();

        // 不可信 CA 签发的客户端证书必须被外层 mTLS Relay 拒绝
        let mut rogue_ca = mtls_test_certs::TestCa::new("RogueCA");
        let (rogue_cert, rogue_key) = rogue_ca.issue_device("u1", "333333333");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let reject_addr = listener.local_addr().unwrap();
        let rej_srv = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(server_cfg).unwrap();
            let mut s = rustls::StreamOwned::new(conn, tcp);
            let mut b = [0u8; 8];
            let _ = s.read(&mut b);
        });
        let rogue_mtls = RelayMtlsConfig {
            ca_cert_pem: ca.ca_pem,
            client_cert_pem: rogue_cert,
            client_key_pem: rogue_key,
            server_name: None,
        };
        let err = TcpRelayTransport::connect_mtls_cancellable(
            &reject_addr.to_string(),
            RelayRole::Client,
            "tok-rogue",
            Duration::from_secs(2),
            Arc::new(AtomicBool::new(false)),
            &rogue_mtls,
        );
        assert!(
            err.is_err(),
            "untrusted client cert must fail mTLS handshake"
        );
        let _ = rej_srv.join();
    }

    #[test]
    fn e2ee_relay_transport_roundtrip_host_auto_and_san_rejection() {
        let mut ca = mtls_test_certs::TestCa::new("E2eeTestCA");
        let (srv_cert, srv_key) = ca.issue_server_for_loopback();
        let (c_cert, c_key) = ca.issue_device("owner-1", "100000001");
        let (h_cert, h_key) = ca.issue_device("owner-1", "200000002");
        let server_cfg = build_mock_mtls_relay_server_config(&ca.ca_pem, &srv_cert, &srv_key);

        // 1) ClientRequired <-> HostAuto (0x16) 成功握手并验证身份
        let (addr1, srv_jh1) = spawn_mock_mtls_relay_server(Arc::clone(&server_cfg));
        let client_bundle = SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem.clone(),
            cert_pem: c_cert.clone(),
            key_pem: c_key.clone(),
            local_user_id: "owner-1".to_string(),
            expected_peer_device_id: Some("200000002".to_string()),
            expected_peer_user_id: Some("owner-1".to_string()),
        };
        let host_bundle = SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem.clone(),
            cert_pem: h_cert.clone(),
            key_pem: h_key.clone(),
            local_user_id: "owner-1".to_string(),
            expected_peer_device_id: Some("100000001".to_string()),
            expected_peer_user_id: Some("owner-1".to_string()),
        };
        let addr_s = addr1.to_string();
        let addr_s2 = addr_s.clone();
        let h_bundle_c = host_bundle.clone();
        let h_jh = thread::spawn(move || {
            E2eeRelayTransport::connect_cancellable(
                &addr_s2,
                RelayRole::Host,
                "e2ee-1",
                Duration::from_secs(3),
                Arc::new(AtomicBool::new(false)),
                Some(&h_bundle_c.to_relay_mtls_config(None)),
                RelayE2eeMode::HostAuto(h_bundle_c),
            )
            .unwrap()
        });
        let mut client_t = E2eeRelayTransport::connect_cancellable(
            &addr_s,
            RelayRole::Client,
            "e2ee-1",
            Duration::from_secs(3),
            Arc::new(AtomicBool::new(false)),
            Some(&client_bundle.to_relay_mtls_config(None)),
            RelayE2eeMode::ClientRequired(client_bundle.clone()),
        )
        .unwrap();
        let mut host_t = h_jh.join().unwrap();
        assert!(client_t.is_e2ee_active());
        assert!(host_t.is_e2ee_active());
        assert_eq!(
            client_t.peer_mtls_identity().unwrap().device_id,
            "200000002"
        );
        assert_eq!(host_t.peer_mtls_identity().unwrap().device_id, "100000001");

        client_t.send_reliable(b"secret-e2ee-cmd").unwrap();
        wait_for_event(
            &mut host_t,
            &NetEvent::Reliable(b"secret-e2ee-cmd".to_vec()),
        );
        host_t.send_datagram(b"secret-e2ee-frame").unwrap();
        wait_for_event(
            &mut client_t,
            &NetEvent::Datagram(b"secret-e2ee-frame".to_vec()),
        );
        drop(client_t);
        drop(host_t);
        let _ = srv_jh1.join();

        // 2) Client Disabled <-> HostAuto (首字节为 1..=3 帧头，回退仅外层 mTLS)
        let (addr2, srv_jh2) = spawn_mock_mtls_relay_server(Arc::clone(&server_cfg));
        let addr_s = addr2.to_string();
        let addr_s2 = addr_s.clone();
        let h_bundle_c = host_bundle.clone();
        let c_relay_mtls = client_bundle.to_relay_mtls_config(None);
        let h_jh2 = thread::spawn(move || {
            E2eeRelayTransport::connect_cancellable(
                &addr_s2,
                RelayRole::Host,
                "e2ee-auto-plain",
                Duration::from_secs(3),
                Arc::new(AtomicBool::new(false)),
                Some(&h_bundle_c.to_relay_mtls_config(None)),
                RelayE2eeMode::HostAuto(h_bundle_c),
            )
            .unwrap()
        });
        let mut client_plain = E2eeRelayTransport::connect_cancellable(
            &addr_s,
            RelayRole::Client,
            "e2ee-auto-plain",
            Duration::from_secs(3),
            Arc::new(AtomicBool::new(false)),
            Some(&c_relay_mtls),
            RelayE2eeMode::Disabled,
        )
        .unwrap();
        client_plain.send_reliable(b"outer-only-msg").unwrap();
        let mut host_plain = h_jh2.join().unwrap();
        assert!(!host_plain.is_e2ee_active());
        wait_for_event(
            &mut host_plain,
            &NetEvent::Reliable(b"outer-only-msg".to_vec()),
        );
        drop(client_plain);
        drop(host_plain);
        let _ = srv_jh2.join();

        // 3) SAN device_id 不匹配必须被 E2EE 校验拒绝
        let (addr3, srv_jh3) = spawn_mock_mtls_relay_server(Arc::clone(&server_cfg));
        let addr_s = addr3.to_string();
        let addr_s2 = addr_s.clone();
        let mut wrong_expect = client_bundle;
        wrong_expect.expected_peer_device_id = Some("999999999".to_string());
        let h_bundle_c = host_bundle;
        let h_jh3 = thread::spawn(move || {
            E2eeRelayTransport::connect_cancellable(
                &addr_s2,
                RelayRole::Host,
                "e2ee-bad-san",
                Duration::from_secs(2),
                Arc::new(AtomicBool::new(false)),
                Some(&h_bundle_c.to_relay_mtls_config(None)),
                RelayE2eeMode::HostRequired(h_bundle_c),
            )
        });
        let c_res = E2eeRelayTransport::connect_cancellable(
            &addr_s,
            RelayRole::Client,
            "e2ee-bad-san",
            Duration::from_secs(2),
            Arc::new(AtomicBool::new(false)),
            Some(&wrong_expect.to_relay_mtls_config(None)),
            RelayE2eeMode::ClientRequired(wrong_expect),
        );
        assert!(c_res.is_err(), "SAN device_id mismatch must reject peer");
        let _ = h_jh3.join();
        let _ = srv_jh3.join();
    }
}
