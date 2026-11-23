//! P2P UDP 打洞与直连 QUIC 建连引擎（`RCP2` 打洞协议 + `QuicEndpoint::from_socket`）。
//!
//! 本模块负责在信令协调（`punch_coord`）下发对端候选地址后，于极短窗口内完成
//! 双向 UDP 打洞并直接在已打通的 `UdpSocket` 上建立 QUIC 连接；若遇到对称 NAT
//! 或 UDP 阻断，则在超时或取消后快速失败，由上层无缝回退至 TCP 中继。
//!
//! # `RCP2` 29 字节打洞包布局
//!
//! ```text
//! [0..4]   magic:        b"RCP2"
//! [4]      version:      u8 = 1
//! [5]      role:         u8 (1 = Host, 2 = Client)
//! [6]      ack:          u8 (0 = Probe, 1 = Ack)
//! [7..23]  session_hash: [u8; 16]（由 session_id 确定性派生）
//! [23..29] nonce:        [u8; 6]（本次打洞随机防重放标记）
//! ```

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::collections::HashSet;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_net::mtls::{MtlsConfig, PeerMtlsIdentity};
use rc_net::quic::{Identity, QuicEndpoint, QuicTransport};
use rc_net::{NetConfig, NetEvent, NetStats, Role, Transport};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

/// UDP 打洞包魔数（`RCP2`）。
pub const PUNCH_MAGIC: &[u8; 4] = b"RCP2";

/// 当前 UDP 打洞协议版本。
pub const PUNCH_VERSION: u8 = 1;

/// UDP 打洞包定长字节数（4 + 1 + 1 + 1 + 16 + 6 = 29）。
pub const PUNCH_PACKET_LEN: usize = 29;

/// 默认 UDP 打洞超时（超时后上层回退 TCP 中继）。
pub const DEFAULT_PUNCH_TIMEOUT: Duration = Duration::from_millis(1_500);

/// 打洞成功后默认的 QUIC 握手超时。
pub const DEFAULT_QUIC_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

/// 探测包周期发送间隔。
const PROBE_INTERVAL: Duration = Duration::from_millis(40);

/// 接收轮询与取消检查步长。
const POLL_STEP: Duration = Duration::from_millis(5);

/// 收到有效包时立即连发 Ack 的次数（对抗弱网单包丢失）。
const IMMEDIATE_ACK_BURST: usize = 3;

/// P2P 打洞角色（与信令协调中的 `host` / `client` 一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum P2pRole {
    /// 被控端：打洞成功后作为 QUIC 服务端调用 `accept`。
    Host = 1,
    /// 控制端：打洞成功后作为 QUIC 客户端调用 `connect`。
    Client = 2,
}

impl P2pRole {
    /// 返回对端应具备的角色（仅接受异侧角色包，防止自环或同侧误匹配）。
    pub const fn opposite(self) -> Self {
        match self {
            Self::Host => Self::Client,
            Self::Client => Self::Host,
        }
    }

    /// 从线缆字节解析角色。
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Host),
            2 => Some(Self::Client),
            _ => None,
        }
    }
}

/// P2P 建连所需的对端候选地址与本地端口提示。
#[derive(Debug, Clone)]
pub struct P2pCandidates {
    /// 信令协调下发的会话标识（用于派生 16 字节 `session_hash`）。
    pub session_id: String,
    /// 信令服务器观测到的对端公网 UDP 地址（如 `"198.51.100.22:60123"`）。
    pub peer_public_addr: Option<String>,
    /// 对端上报的局域网候选地址列表（优先级高于公网地址）。
    pub peer_lan_addrs: Vec<String>,
    /// 本端持久化 UDP `RCSP` 探测套接字绑定的本地端口（优先复用以对齐 NAT 映射）。
    pub local_udp_port: Option<u16>,
}

/// 解码后的 `RCP2` UDP 打洞包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PunchPacket {
    /// 协议版本号（当前固定为 [`PUNCH_VERSION`] = `1`）。
    pub version: u8,
    /// 发送方角色。
    pub role: P2pRole,
    /// 是否为确认包（`false` = Probe，`true` = Ack）。
    pub ack: bool,
    /// 会话 ID 派生的 16 字节哈希。
    pub session_hash: [u8; 16],
    /// 6 字节随机 nonce。
    pub nonce: [u8; 6],
}

static LOCAL_UDP_PORT_LEASES: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();

fn local_udp_port_leases() -> &'static Mutex<HashSet<u16>> {
    LOCAL_UDP_PORT_LEASES.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 进程级本地 UDP 端口租约守卫：确保同一 `local_udp_port` 最多只有一个未 `connect` 的
/// `SO_REUSEPORT` P2P/QUIC 套接字同时存在，避免多个未连接套接字在内核分流导致随机丢包。
#[derive(Debug)]
struct LocalUdpPortLease {
    port: u16,
}

impl LocalUdpPortLease {
    fn try_acquire(port: u16) -> Option<Self> {
        if port == 0 {
            return None;
        }
        let mut set = local_udp_port_leases()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if set.insert(port) {
            Some(Self { port })
        } else {
            None
        }
    }
}

impl Drop for LocalUdpPortLease {
    fn drop(&mut self) {
        let mut set = local_udp_port_leases()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set.remove(&self.port);
    }
}

/// P2P 直连 QUIC 传输封装：对外透明实现 [`rc_net::Transport`]，内部持有
/// [`QuicEndpoint`] 以保证连接存活期间底层 UDP 端点与驱动不提前释放。
pub struct P2pQuicConnection {
    /// 底层 QUIC 传输通道（datagram + reliable + urgent）。
    pub transport: QuicTransport,
    /// 实际建连成功的对端 UDP 地址。
    pub peer_addr: SocketAddr,
    /// 建连阶段测得的 RTT 估计（微秒）。
    pub rtt_us: u32,
    /// 保持端点存活，声明顺序位于 `transport` 之后以确保 `Drop` 时先关连接再释放端点。
    _endpoint: QuicEndpoint,
    /// 本地打洞端口租约，声明顺序位于 `_endpoint` 之后以确保端点关闭后再归还端口租约。
    _port_lease: Option<LocalUdpPortLease>,
}

impl P2pQuicConnection {
    /// 若本次 P2P QUIC 握手经过 mTLS 验证，返回对端证书身份。
    pub fn peer_mtls_identity(&self) -> Option<PeerMtlsIdentity> {
        self.transport.peer_mtls_identity()
    }
}

impl Transport for P2pQuicConnection {
    fn send_datagram(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.transport.send_datagram(data)
    }

    fn send_reliable(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.transport.send_reliable(data)
    }

    fn send_reliable_urgent(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.transport.send_reliable_urgent(data)
    }

    fn poll(&mut self, timeout: Duration) -> rc_types::Result<Vec<NetEvent>> {
        self.transport.poll(timeout)
    }

    fn stats(&self) -> NetStats {
        self.transport.stats()
    }
}

/// 将 [`PunchPacket`] 编码为 29 字节固定二进制包。
pub fn encode_punch_packet(pkt: &PunchPacket) -> [u8; PUNCH_PACKET_LEN] {
    let mut out = [0u8; PUNCH_PACKET_LEN];
    out[0..4].copy_from_slice(PUNCH_MAGIC);
    out[4] = pkt.version;
    out[5] = pkt.role as u8;
    out[6] = u8::from(pkt.ack);
    out[7..23].copy_from_slice(&pkt.session_hash);
    out[23..29].copy_from_slice(&pkt.nonce);
    out
}

/// 从 UDP 载荷解码 29 字节 `RCP2` 打洞包；长度、魔数、版本、角色或确认位非法均返回 `None`。
pub fn decode_punch_packet(buf: &[u8]) -> Option<PunchPacket> {
    if buf.len() != PUNCH_PACKET_LEN {
        return None;
    }
    if &buf[0..4] != PUNCH_MAGIC {
        return None;
    }
    let version = buf[4];
    if version != PUNCH_VERSION {
        return None;
    }
    let role = P2pRole::from_u8(buf[5])?;
    let ack = match buf[6] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mut session_hash = [0u8; 16];
    session_hash.copy_from_slice(&buf[7..23]);
    let mut nonce = [0u8; 6];
    nonce.copy_from_slice(&buf[23..29]);
    Some(PunchPacket {
        version,
        role,
        ack,
        session_hash,
        nonce,
    })
}

/// 解析并排序对端候选 UDP 地址：优先保留 `peer_lan_addrs`（同局域网直连优先），
/// 其次追加 `peer_public_addr`，自动去重并过滤非法、端口为 0 或未指定地址（`0.0.0.0` / `::`）。
pub fn parse_candidate_addrs(candidates: &P2pCandidates) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let mut push_raw = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(parsed) = trimmed.parse::<SocketAddr>() else {
            return;
        };
        let normalized = canonicalize_socket_addr(parsed);
        if normalized.port() == 0 || normalized.ip().is_unspecified() {
            return;
        }
        if seen.insert(normalized) {
            out.push(normalized);
        }
    };

    for lan in &candidates.peer_lan_addrs {
        push_raw(lan);
    }
    if let Some(ref public) = candidates.peer_public_addr {
        push_raw(public);
    }
    out
}

/// 由 `session_id` 确定性派生 16 字节会话哈希（零外部依赖，纯 Rust 双路域分离 FNV-1a + 终态雪崩混合）。
pub fn derive_session_hash(session_id: &str) -> [u8; 16] {
    let h0 = fnv1a_64_mixed(b"rc-p2p-session-v1-lo:", session_id.as_bytes());
    let h1 = fnv1a_64_mixed(b"rc-p2p-session-v1-hi:", session_id.as_bytes());
    let mut out = [0u8; 16];
    out[0..8].copy_from_slice(&h0.to_be_bytes());
    out[8..16].copy_from_slice(&h1.to_be_bytes());
    out
}

fn fnv1a_64_mixed(domain: &[u8], data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for &b in domain.iter().chain(data.iter()) {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // 两轮 SplitMix64 终态雪崩混合，确保任意 1 bit 输入差异扩散到全部 64 位。
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94d0_49bb_1331_11eb);
    hash ^ (hash >> 31)
}

fn canonicalize_socket_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => SocketAddr::V6(v6),
        },
        SocketAddr::V4(v4) => SocketAddr::V4(v4),
    }
}

/// 将地址适配为当前套接字的地址族：
/// - `AF_INET6`（含双栈）套接字绝不剥离 `::ffff:` 前缀，且将 `V4` 映射为 IPv4-mapped IPv6；
/// - `AF_INET` 套接字则统一解包为 `V4`。
fn normalize_addr_for_socket(addr: SocketAddr, socket_is_ipv6: bool) -> SocketAddr {
    if socket_is_ipv6 {
        match addr {
            SocketAddr::V6(v6) => SocketAddr::V6(v6),
            SocketAddr::V4(v4) => {
                SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
            }
        }
    } else {
        canonicalize_socket_addr(addr)
    }
}

fn map_targets_for_socket(all_targets: &[SocketAddr], socket_is_ipv6: bool) -> Vec<SocketAddr> {
    let mut out = Vec::with_capacity(all_targets.len());
    for &addr in all_targets {
        if socket_is_ipv6 {
            out.push(normalize_addr_for_socket(addr, true));
        } else if addr.is_ipv4() {
            out.push(addr);
        }
    }
    out
}

fn bind_punch_socket(
    preferred_port: Option<u16>,
    use_ipv6: bool,
    require_dual_stack: bool,
) -> Result<(UdpSocket, Option<LocalUdpPortLease>), String> {
    let try_bind = |port: u16, reuse_port: bool| -> Result<UdpSocket, std::io::Error> {
        let (domain, addr) = if use_ipv6 {
            (Domain::IPV6, SocketAddr::from(([0u16; 8], port)))
        } else {
            (Domain::IPV4, SocketAddr::from(([0u8; 4], port)))
        };
        let raw = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if use_ipv6 {
            if require_dual_stack {
                raw.set_only_v6(false)?;
            } else {
                let _ = raw.set_only_v6(false);
            }
        }
        if reuse_port {
            let _ = raw.set_reuse_address(true);
            #[cfg(unix)]
            let _ = raw.set_reuse_port(true);
        }
        raw.bind(&SockAddr::from(addr))?;
        let sock: UdpSocket = raw.into();
        sock.set_nonblocking(true)?;
        Ok(sock)
    };

    if let Some(port) = preferred_port.filter(|&p| p != 0) {
        if let Some(lease) = LocalUdpPortLease::try_acquire(port) {
            if let Ok(sock) = try_bind(port, true) {
                return Ok((sock, Some(lease)));
            }
            drop(lease);
        }
    }

    // 端口租约已被同进程其他在途/活跃 P2P 连接占用，或优选端口不可用时，
    // 绝不创建第二个未 connect 的 SO_REUSEPORT 套接字：直接以无 SO_REUSEPORT 的临时端口 `0` 绑定。
    let sock = try_bind(0, false).map_err(|e| format!("绑定 P2P UDP 打洞套接字失败：{e}"))?;
    Ok((sock, None))
}

struct PunchSetup {
    socket: UdpSocket,
    port_lease: Option<LocalUdpPortLease>,
    targets: Vec<SocketAddr>,
    socket_is_ipv6: bool,
}

fn prepare_punch_socket_and_targets(
    candidates: &P2pCandidates,
    all_targets: &[SocketAddr],
) -> Result<PunchSetup, String> {
    let has_ipv6 = all_targets.iter().any(SocketAddr::is_ipv6);
    let has_ipv4 = all_targets.iter().any(SocketAddr::is_ipv4);

    if has_ipv6 {
        // 只要存在 IPv6 候选（纯 IPv6 或 IPv4+IPv6 混合），优先创建双栈 AF_INET6 套接字，
        // 保留全部 IPv6 候选并将 IPv4 候选映射为 ::ffff:a.b.c.d，绝不因存在 IPv4 LAN 地址而丢弃 IPv6 公网候选。
        match bind_punch_socket(candidates.local_udp_port, true, has_ipv4) {
            Ok((socket, port_lease)) => {
                let targets = map_targets_for_socket(all_targets, true);
                return Ok(PunchSetup {
                    socket,
                    port_lease,
                    targets,
                    socket_is_ipv6: true,
                });
            }
            Err(v6_err) => {
                if !has_ipv4 {
                    return Err(v6_err);
                }
            }
        }
    }

    let (socket, port_lease) = bind_punch_socket(candidates.local_udp_port, false, false)?;
    let targets = map_targets_for_socket(all_targets, false);
    Ok(PunchSetup {
        socket,
        port_lease,
        targets,
        socket_is_ipv6: false,
    })
}

struct PunchedSocket {
    socket: UdpSocket,
    peer_addr: SocketAddr,
    elapsed: Duration,
    port_lease: Option<LocalUdpPortLease>,
}

/// 执行双向 UDP 打洞，成功后返回已打通的非阻塞 `UdpSocket`、实测对端地址及打洞耗时。
///
/// - 优先在进程级端口租约保护下复用 `candidates.local_udp_port`（`SO_REUSEADDR` + `SO_REUSEPORT`），
///   若已被同进程其他在途/活跃 P2P 连接占用或绑定失败则自动降级为无 `SO_REUSEPORT` 的临时端口 `0`；
/// - 存在 IPv6 候选（含 IPv4+IPv6 混合）时自动使用双栈 `AF_INET6` 套接字，保留全部候选地址；
/// - 每 40ms 向候选地址发送 29 字节 `RCP2` 探测/确认包，使用 `peek_from` 预检入站包以完好保留
///   对端先发出的 QUIC `Initial` 报文；
/// - 严格要求双向连通确认（收到异侧 `ack = 1` 或已确认对端/目标发来的非 `RCP2` 报文），
///   杜绝单向 Probe 误判；
/// - `cancel` 置位时在 ~5ms 内快速中止，便于上层立刻回退 TCP 中继。
pub fn punch_udp_pair(
    role: P2pRole,
    candidates: &P2pCandidates,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<(UdpSocket, SocketAddr, Duration), String> {
    let punched = punch_udp_pair_with_lease(role, candidates, timeout, cancel)?;
    Ok((punched.socket, punched.peer_addr, punched.elapsed))
}

fn punch_udp_pair_with_lease(
    role: P2pRole,
    candidates: &P2pCandidates,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<PunchedSocket, String> {
    if cancel.load(Ordering::Acquire) {
        return Err("P2P 打洞已取消".to_string());
    }

    let all_targets = parse_candidate_addrs(candidates);
    if all_targets.is_empty() {
        return Err("没有可用的对端 UDP 候选地址".to_string());
    }

    let PunchSetup {
        socket,
        port_lease,
        targets,
        socket_is_ipv6,
    } = prepare_punch_socket_and_targets(candidates, &all_targets)?;

    let target_canons: HashSet<SocketAddr> = targets
        .iter()
        .copied()
        .map(canonicalize_socket_addr)
        .collect();
    let session_hash = derive_session_hash(&candidates.session_id);
    let expected_role = role.opposite();

    let mut nonce = [0u8; 6];
    OsRng.fill_bytes(&mut nonce);

    let probe_pkt = encode_punch_packet(&PunchPacket {
        version: PUNCH_VERSION,
        role,
        ack: false,
        session_hash,
        nonce,
    });
    let ack_pkt = encode_punch_packet(&PunchPacket {
        version: PUNCH_VERSION,
        role,
        ack: true,
        session_hash,
        nonce,
    });

    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_send: Option<Instant> = None;
    let mut confirmed_peer: Option<SocketAddr> = None;
    let mut recv_buf = [0u8; 2048];

    loop {
        if cancel.load(Ordering::Acquire) {
            return Err("P2P 打洞已取消".to_string());
        }

        if last_send.map_or(true, |t| t.elapsed() >= PROBE_INTERVAL) {
            if let Some(peer_addr) = confirmed_peer {
                let _ = socket.send_to(&ack_pkt, peer_addr);
                let peer_canon = canonicalize_socket_addr(peer_addr);
                for &target in &targets {
                    if canonicalize_socket_addr(target) != peer_canon {
                        let _ = socket.send_to(&probe_pkt, target);
                    }
                }
            } else {
                for &target in &targets {
                    let _ = socket.send_to(&probe_pkt, target);
                }
            }
            last_send = Some(Instant::now());
        }

        loop {
            match socket.peek_from(&mut recv_buf) {
                Ok((n, raw_from)) => {
                    let from_addr = normalize_addr_for_socket(raw_from, socket_is_ipv6);
                    let from_canon = canonicalize_socket_addr(from_addr);
                    if let Some(pkt) = decode_punch_packet(&recv_buf[..n]) {
                        // 合法 RCP2 打洞包：先从内核接收队列消费掉再处理。
                        let _ = socket.recv_from(&mut recv_buf);
                        if pkt.session_hash != session_hash || pkt.role != expected_role {
                            continue;
                        }
                        for _ in 0..IMMEDIATE_ACK_BURST {
                            let _ = socket.send_to(&ack_pkt, from_addr);
                        }
                        if pkt.ack {
                            return Ok(PunchedSocket {
                                socket,
                                peer_addr: from_addr,
                                elapsed: start.elapsed(),
                                port_lease,
                            });
                        }
                        if confirmed_peer.is_none() {
                            confirmed_peer = Some(from_addr);
                        }
                    } else {
                        let from_confirmed = confirmed_peer
                            .is_some_and(|p| canonicalize_socket_addr(p) == from_canon);
                        let from_target = target_canons.contains(&from_canon);
                        if from_confirmed
                            || (from_target && (role == P2pRole::Host || confirmed_peer.is_some()))
                        {
                            // 对端已切入 QUIC 并发出 Initial 等非 RCP2 报文：绝不消费该包，
                            // 直接保留在内核接收缓冲中移交给 QuicEndpoint::from_socket。
                            return Ok(PunchedSocket {
                                socket,
                                peer_addr: from_addr,
                                elapsed: start.elapsed(),
                                port_lease,
                            });
                        }
                        // 未知来源的杂散非 RCP2 报文：消费丢弃，防止 peek_from 空转。
                        let _ = socket.recv_from(&mut recv_buf);
                    }
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::WouldBlock
                            | ErrorKind::Interrupted
                            | ErrorKind::ConnectionRefused
                            | ErrorKind::ConnectionReset
                    ) =>
                {
                    break;
                }
                Err(err) => {
                    return Err(format!("P2P UDP 打洞读取失败：{err}"));
                }
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(format!("P2P UDP 打洞超时（{timeout:?}）"));
        }

        let sleep_budget = POLL_STEP.min(deadline - now);
        thread::sleep(sleep_budget);
    }
}

/// 作为被控端（Host）尝试 UDP 打洞并在打通的 socket 上完成 QUIC 服务端握手。
///
/// 任意阶段失败或被 `cancel` 打断均返回可读错误字符串，供上层回退 TCP 中继。
pub fn attempt_p2p_quic_host(
    candidates: &P2pCandidates,
    punch_timeout: Duration,
    quic_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<P2pQuicConnection, String> {
    attempt_p2p_quic_host_mtls(candidates, punch_timeout, quic_timeout, cancel, None)
}

/// 作为被控端（Host）尝试 UDP 打洞并在打通的 socket 上完成 QUIC 服务端握手（支持可选双向 mTLS）。
pub fn attempt_p2p_quic_host_mtls(
    candidates: &P2pCandidates,
    punch_timeout: Duration,
    quic_timeout: Duration,
    cancel: &AtomicBool,
    mtls: Option<&MtlsConfig>,
) -> Result<P2pQuicConnection, String> {
    let PunchedSocket {
        socket,
        peer_addr: punched_peer,
        elapsed: punch_elapsed,
        port_lease,
    } = punch_udp_pair_with_lease(P2pRole::Host, candidates, punch_timeout, cancel)?;
    if cancel.load(Ordering::Acquire) {
        return Err("P2P QUIC Host 握手已取消".to_string());
    }

    let local_addr = socket
        .local_addr()
        .map_err(|e| format!("获取 P2P Host 本地地址失败：{e}"))?;
    let endpoint = if let Some(mtls_cfg) = mtls {
        QuicEndpoint::from_socket_mtls(
            socket,
            NetConfig::new(Role::Host, local_addr),
            Some(mtls_cfg),
        )
        .map_err(|e| format!("创建 P2P QUIC Host mTLS 端点失败：{e}"))?
    } else {
        let identity = rc_session::dev_cert::dev_identity()
            .map_err(|e| format!("加载 P2P Host 开发证书失败：{e}"))?;
        QuicEndpoint::from_socket(
            socket,
            NetConfig::new(Role::Host, local_addr),
            Some(identity),
        )
        .map_err(|e| format!("创建 P2P QUIC Host 端点失败：{e}"))?
    };

    let transport = endpoint
        .accept_from_cancelable(Some(punched_peer), quic_timeout, cancel)
        .map_err(|e| format!("P2P QUIC Host 握手失败：{e}"))?;

    Ok(finalize_p2p_connection(
        endpoint,
        transport,
        punch_elapsed,
        port_lease,
    ))
}

/// 作为控制端（Client）尝试 UDP 打洞并在打通的 socket 上发起 QUIC 客户端握手。
///
/// 任意阶段失败或被 `cancel` 打断均返回可读错误字符串，供上层回退 TCP 中继。
pub fn attempt_p2p_quic_client(
    candidates: &P2pCandidates,
    punch_timeout: Duration,
    quic_timeout: Duration,
    cancel: &AtomicBool,
) -> Result<P2pQuicConnection, String> {
    attempt_p2p_quic_client_mtls(candidates, punch_timeout, quic_timeout, cancel, None)
}

/// 作为控制端（Client）尝试 UDP 打洞并在打通的 socket 上发起 QUIC 客户端握手（支持可选双向 mTLS）。
pub fn attempt_p2p_quic_client_mtls(
    candidates: &P2pCandidates,
    punch_timeout: Duration,
    quic_timeout: Duration,
    cancel: &AtomicBool,
    mtls: Option<&MtlsConfig>,
) -> Result<P2pQuicConnection, String> {
    let PunchedSocket {
        socket,
        peer_addr,
        elapsed: punch_elapsed,
        port_lease,
    } = punch_udp_pair_with_lease(P2pRole::Client, candidates, punch_timeout, cancel)?;
    if cancel.load(Ordering::Acquire) {
        return Err("P2P QUIC Client 握手已取消".to_string());
    }

    let local_addr = socket
        .local_addr()
        .map_err(|e| format!("获取 P2P Client 本地地址失败：{e}"))?;
    if let Some(mtls_cfg) = mtls {
        let endpoint = QuicEndpoint::from_socket_mtls(
            socket,
            NetConfig::new(Role::Controller, local_addr),
            None,
        )
        .map_err(|e| format!("创建 P2P QUIC Client mTLS 端点失败：{e}"))?;
        let transport = endpoint
            .connect_mtls_cancelable(peer_addr, mtls_cfg, "", quic_timeout, cancel)
            .map_err(|e| format!("P2P QUIC Client mTLS 握手失败：{e}"))?;
        Ok(finalize_p2p_connection(
            endpoint,
            transport,
            punch_elapsed,
            port_lease,
        ))
    } else {
        let endpoint =
            QuicEndpoint::from_socket(socket, NetConfig::new(Role::Controller, local_addr), None)
                .map_err(|e| format!("创建 P2P QUIC Client 端点失败：{e}"))?;
        let certs = Identity::certs_from_pem(rc_session::dev_cert::dev_server_cert_pem())
            .map_err(|e| format!("解析 P2P 受信任证书失败：{e}"))?;
        let transport = endpoint
            .connect_cancelable(
                peer_addr,
                &certs,
                rc_session::dev_cert::DEV_SERVER_NAME,
                quic_timeout,
                cancel,
            )
            .map_err(|e| format!("P2P QUIC Client 握手失败：{e}"))?;
        Ok(finalize_p2p_connection(
            endpoint,
            transport,
            punch_elapsed,
            port_lease,
        ))
    }
}

fn finalize_p2p_connection(
    endpoint: QuicEndpoint,
    transport: QuicTransport,
    punch_elapsed: Duration,
    port_lease: Option<LocalUdpPortLease>,
) -> P2pQuicConnection {
    let peer_addr = transport.remote_addr();
    let stats_rtt = transport.stats().rtt_us;
    let rtt_us = if stats_rtt > 0 {
        stats_rtt
    } else {
        u32::try_from(punch_elapsed.as_micros()).unwrap_or(u32::MAX)
    };
    P2pQuicConnection {
        transport,
        peer_addr,
        rtt_us,
        _endpoint: endpoint,
        _port_lease: port_lease,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn punch_packet_encode_decode_and_reject_invalid() {
        let hash = derive_session_hash("sess-p2p-001");
        let pkt = PunchPacket {
            version: PUNCH_VERSION,
            role: P2pRole::Host,
            ack: true,
            session_hash: hash,
            nonce: [1, 2, 3, 4, 5, 6],
        };
        let wire = encode_punch_packet(&pkt);
        assert_eq!(wire.len(), PUNCH_PACKET_LEN);
        assert_eq!(&wire[0..4], PUNCH_MAGIC);
        assert_eq!(decode_punch_packet(&wire), Some(pkt));

        // 截断或超长拒绝
        assert!(decode_punch_packet(&wire[..PUNCH_PACKET_LEN - 1]).is_none());
        let mut oversized = wire.to_vec();
        oversized.push(0);
        assert!(decode_punch_packet(&oversized).is_none());

        // 错误魔数拒绝
        let mut bad_magic = wire;
        bad_magic[0] = b'X';
        assert!(decode_punch_packet(&bad_magic).is_none());

        // 错误版本拒绝
        let mut bad_ver = wire;
        bad_ver[4] = 2;
        assert!(decode_punch_packet(&bad_ver).is_none());

        // 非法角色字节拒绝
        let mut bad_role = wire;
        bad_role[5] = 9;
        assert!(decode_punch_packet(&bad_role).is_none());

        // 非法 ack 字节拒绝
        let mut bad_ack = wire;
        bad_ack[6] = 2;
        assert!(decode_punch_packet(&bad_ack).is_none());

        // session_hash 确定性且区分不同 session_id
        assert_eq!(derive_session_hash("sess-p2p-001"), hash);
        assert_ne!(derive_session_hash("sess-p2p-002"), hash);
    }

    #[test]
    fn parse_candidate_addrs_prefers_lan_dedups_and_filters_invalid() {
        let candidates = P2pCandidates {
            session_id: "s1".to_string(),
            peer_public_addr: Some("198.51.100.22:60123".to_string()),
            peer_lan_addrs: vec![
                "  ".to_string(),
                "not-an-addr".to_string(),
                "0.0.0.0:9901".to_string(),
                "192.168.1.20:0".to_string(),
                "[::]:9901".to_string(),
                "192.168.1.20:50000".to_string(),
                "192.168.1.20:50000".to_string(),          // 重复
                "[::ffff:192.168.1.20]:50000".to_string(), // IPv4-mapped 同地址去重
                "10.0.0.8:50001".to_string(),
            ],
            local_udp_port: None,
        };
        let parsed = parse_candidate_addrs(&candidates);
        assert_eq!(
            parsed,
            vec![
                "192.168.1.20:50000".parse::<SocketAddr>().unwrap(),
                "10.0.0.8:50001".parse::<SocketAddr>().unwrap(),
                "198.51.100.22:60123".parse::<SocketAddr>().unwrap(),
            ]
        );

        // 若 public 与某个 LAN 重复，仍只保留首次（LAN 优先位）
        let dup_pub = P2pCandidates {
            session_id: "s2".to_string(),
            peer_public_addr: Some("192.168.1.20:50000".to_string()),
            peer_lan_addrs: vec!["192.168.1.20:50000".to_string()],
            local_udp_port: None,
        };
        assert_eq!(
            parse_candidate_addrs(&dup_pub),
            vec!["192.168.1.20:50000".parse::<SocketAddr>().unwrap()]
        );
    }

    fn reserve_ephemeral_udp_port() -> u16 {
        let s = UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
        s.local_addr().expect("local addr").port()
    }

    #[test]
    fn punch_udp_pair_localhost_succeeds_fast_and_rejects_wrong_session_or_role() {
        let host_port = reserve_ephemeral_udp_port();
        let client_port = reserve_ephemeral_udp_port();
        assert_ne!(host_port, client_port);

        // 注入一条错误会话哈希干扰源与同侧角色干扰源，验证打洞仅认匹配的异侧对端
        let noise_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let wrong_hash_pkt = encode_punch_packet(&PunchPacket {
            version: PUNCH_VERSION,
            role: P2pRole::Client,
            ack: true,
            session_hash: derive_session_hash("wrong-session"),
            nonce: [9; 6],
        });
        let same_role_pkt = encode_punch_packet(&PunchPacket {
            version: PUNCH_VERSION,
            role: P2pRole::Host,
            ack: true,
            session_hash: derive_session_hash("punch-localhost-ok"),
            nonce: [8; 6],
        });
        let _ = noise_sock.send_to(
            &wrong_hash_pkt,
            SocketAddr::from(([127, 0, 0, 1], host_port)),
        );
        let _ = noise_sock.send_to(
            &same_role_pkt,
            SocketAddr::from(([127, 0, 0, 1], host_port)),
        );

        let host_cand = P2pCandidates {
            session_id: "punch-localhost-ok".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let client_cand = P2pCandidates {
            session_id: "punch-localhost-ok".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_port),
        };

        let cancel_h = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel_h);

        let t0 = Instant::now();
        let host_thread = thread::spawn(move || {
            punch_udp_pair(P2pRole::Host, &host_cand, DEFAULT_PUNCH_TIMEOUT, &cancel_h)
        });
        let (client_sock, client_seen_peer, client_elapsed) = punch_udp_pair(
            P2pRole::Client,
            &client_cand,
            DEFAULT_PUNCH_TIMEOUT,
            &cancel_c,
        )
        .expect("client punch must succeed");
        let (host_sock, host_seen_peer, host_elapsed) = host_thread
            .join()
            .expect("host thread join")
            .expect("host punch must succeed");

        let total = t0.elapsed();
        assert!(
            total < Duration::from_millis(200),
            "localhost UDP punch should succeed in < 200ms, took {total:?} (host={host_elapsed:?}, client={client_elapsed:?})"
        );
        assert_eq!(
            client_seen_peer.port(),
            host_sock.local_addr().unwrap().port()
        );
        assert_eq!(
            host_seen_peer.port(),
            client_sock.local_addr().unwrap().port()
        );
    }

    #[test]
    fn attempt_p2p_quic_localhost_roundtrip_datagram_reliable_and_urgent() {
        let host_port = reserve_ephemeral_udp_port();
        let client_port = reserve_ephemeral_udp_port();
        assert_ne!(host_port, client_port);

        let host_cand = P2pCandidates {
            session_id: "p2p-quic-e2e-001".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let client_cand = P2pCandidates {
            session_id: "p2p-quic-e2e-001".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_port),
        };

        let cancel_h = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel_h);

        let host_thread = thread::spawn(move || {
            attempt_p2p_quic_host(
                &host_cand,
                DEFAULT_PUNCH_TIMEOUT,
                DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
                &cancel_h,
            )
        });
        let client_conn = attempt_p2p_quic_client(
            &client_cand,
            DEFAULT_PUNCH_TIMEOUT,
            DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
            &cancel_c,
        )
        .expect("P2P QUIC client must connect");
        let host_conn = host_thread
            .join()
            .expect("host thread join")
            .expect("P2P QUIC host must accept");

        assert_eq!(client_conn.peer_addr.port(), host_port);
        assert_eq!(host_conn.peer_addr.port(), client_port);

        // 通过 trait object (`Box<dyn Transport>`) 验证三条通道收发与端点保活
        let mut client: Box<dyn Transport> = Box::new(client_conn);
        let mut host: Box<dyn Transport> = Box::new(host_conn);

        let dgram = b"p2p-video-slice-01".to_vec();
        let rel_h2c = b"p2p-control-host-to-client".to_vec();
        let rel_c2h = b"p2p-control-client-to-host".to_vec();
        let urg_c2h = b"p2p-input-urgent-key-a".to_vec();

        host.send_datagram(&dgram).unwrap();
        host.send_reliable(&rel_h2c).unwrap();
        client.send_reliable(&rel_c2h).unwrap();
        client.send_reliable_urgent(&urg_c2h).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_dgram = false;
        let mut got_rel_h2c = false;
        let mut got_rel_c2h = false;
        let mut got_urg_c2h = false;

        while Instant::now() < deadline && !(got_dgram && got_rel_h2c && got_rel_c2h && got_urg_c2h)
        {
            for ev in client.poll(Duration::from_millis(20)).unwrap() {
                match ev {
                    NetEvent::Datagram(b) if b == dgram => got_dgram = true,
                    NetEvent::Reliable(b) if b == rel_h2c => got_rel_h2c = true,
                    _ => {}
                }
            }
            for ev in host.poll(Duration::from_millis(20)).unwrap() {
                match ev {
                    NetEvent::Reliable(b) if b == rel_c2h => got_rel_c2h = true,
                    NetEvent::ReliableUrgent(b) if b == urg_c2h => got_urg_c2h = true,
                    _ => {}
                }
            }
        }

        assert!(got_dgram, "client must receive datagram over P2P QUIC");
        assert!(got_rel_h2c, "client must receive reliable over P2P QUIC");
        assert!(got_rel_c2h, "host must receive reliable over P2P QUIC");
        assert!(got_urg_c2h, "host must receive urgent over P2P QUIC");
    }

    #[cfg(unix)]
    #[test]
    fn p2p_quic_succeeds_while_connected_signal_probe_sockets_hold_same_local_ports() {
        // 模拟真实运行时：两端 SignalClientHandle 各自持有已 connect 到信令 UDP 端口的
        // SO_REUSEADDR + SO_REUSEPORT 探测套接字，同时 P2P 打洞 + QUIC 复用完全相同的本地端口。
        let sig_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sig_addr = sig_udp.local_addr().unwrap();

        let make_probe_sock = || -> (UdpSocket, u16) {
            let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
            raw.set_reuse_address(true).unwrap();
            #[cfg(unix)]
            raw.set_reuse_port(true).unwrap();
            raw.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], 0))))
                .unwrap();
            let sock: UdpSocket = raw.into();
            let port = sock.local_addr().unwrap().port();
            sock.connect(sig_addr).unwrap();
            (sock, port)
        };

        let (_host_probe, host_port) = make_probe_sock();
        let (_client_probe, client_port) = make_probe_sock();
        assert_ne!(host_port, client_port);

        let host_cand = P2pCandidates {
            session_id: "p2p-coexist-probe-001".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let client_cand = P2pCandidates {
            session_id: "p2p-coexist-probe-001".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_port),
        };

        let cancel_h = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel_h);

        let host_thread = thread::spawn(move || {
            attempt_p2p_quic_host(
                &host_cand,
                DEFAULT_PUNCH_TIMEOUT,
                DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
                &cancel_h,
            )
        });
        let mut client = attempt_p2p_quic_client(
            &client_cand,
            DEFAULT_PUNCH_TIMEOUT,
            DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
            &cancel_c,
        )
        .expect("client P2P QUIC must coexist with persistent probe socket");
        let mut host = host_thread
            .join()
            .expect("host join")
            .expect("host P2P QUIC must coexist with persistent probe socket");

        assert_eq!(client.peer_addr.port(), host_port);
        assert_eq!(host.peer_addr.port(), client_port);

        host.send_datagram(b"coexist-dgram").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = false;
        while Instant::now() < deadline && !received {
            for ev in client.poll(Duration::from_millis(20)).unwrap() {
                if matches!(ev, NetEvent::Datagram(ref b) if b == b"coexist-dgram") {
                    received = true;
                }
            }
        }
        assert!(
            received,
            "datagram must flow while probe sockets remain open"
        );
    }

    #[test]
    fn punch_cancel_aborts_fast_and_unreachable_times_out_cleanly() {
        // 1) 取消信号必须在 < 300ms 内快速返回错误
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = Arc::clone(&cancel);
        let killer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            trigger.store(true, Ordering::Release);
        });
        let dead_port = reserve_ephemeral_udp_port();
        let cand_cancel = P2pCandidates {
            session_id: "cancel-test".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{dead_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
        };
        let t0 = Instant::now();
        let res = punch_udp_pair(
            P2pRole::Client,
            &cand_cancel,
            Duration::from_secs(5),
            &cancel,
        );
        let elapsed = t0.elapsed();
        killer.join().unwrap();
        assert!(res.is_err());
        assert!(
            elapsed < Duration::from_millis(300),
            "cancel must abort in < 300ms, took {elapsed:?}"
        );

        // 2) 无响应对端在超时后干净返回 Err，供上层回退 TCP 中继
        let no_cancel = AtomicBool::new(false);
        let cand_timeout = P2pCandidates {
            session_id: "timeout-test".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{dead_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
        };
        let t1 = Instant::now();
        let err = punch_udp_pair(
            P2pRole::Host,
            &cand_timeout,
            Duration::from_millis(120),
            &no_cancel,
        )
        .unwrap_err();
        assert!(
            err.contains("超时"),
            "timeout error should explain timeout: {err}"
        );
        assert!(
            t1.elapsed() >= Duration::from_millis(100) && t1.elapsed() < Duration::from_secs(1),
            "timeout duration unexpected: {:?}",
            t1.elapsed()
        );

        // 3) 空候选列表立刻失败，不白等超时
        let empty_cand = P2pCandidates {
            session_id: "empty-test".to_string(),
            peer_public_addr: None,
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
        };
        let t2 = Instant::now();
        assert!(attempt_p2p_quic_client(
            &empty_cand,
            Duration::from_secs(5),
            Duration::from_secs(5),
            &no_cancel,
        )
        .is_err());
        assert!(t2.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn local_udp_port_lease_prevents_double_bind_and_falls_back_to_ephemeral() {
        let preferred = reserve_ephemeral_udp_port();
        let (sock1, lease1) =
            bind_punch_socket(Some(preferred), false, false).expect("first bind must succeed");
        assert_eq!(sock1.local_addr().unwrap().port(), preferred);
        assert!(lease1.is_some(), "first bind must acquire port lease");

        // 租约仍被持有时，第二次请求同一 local_udp_port 不得双绑 SO_REUSEPORT，必须降级临时端口。
        let (sock2, lease2) =
            bind_punch_socket(Some(preferred), false, false).expect("second bind must fallback");
        assert_ne!(
            sock2.local_addr().unwrap().port(),
            preferred,
            "second bind must fall back to ephemeral port while lease is held"
        );
        assert!(lease2.is_none(), "ephemeral fallback must not hold a lease");

        // 释放第一个套接字与租约后，同一端口可重新获取租约并绑定。
        drop(sock1);
        drop(lease1);
        let (sock3, lease3) =
            bind_punch_socket(Some(preferred), false, false).expect("rebind after drop");
        assert_eq!(sock3.local_addr().unwrap().port(), preferred);
        assert!(lease3.is_some());
    }

    #[test]
    fn mixed_ipv4_and_ipv6_candidates_retain_ipv6_public_addr_and_punch_dual_stack() {
        // 混合候选同时包含 IPv4 LAN 与 IPv6 公网地址：绝不得因存在 IPv4 LAN 而丢弃 IPv6 公网候选。
        let dead_v4_port = reserve_ephemeral_udp_port();
        let host_v6_probe = UdpSocket::bind("[::1]:0").expect("bind ipv6 loopback");
        let host_v6_port = host_v6_probe.local_addr().unwrap().port();
        drop(host_v6_probe);

        let mixed_client_cand = P2pCandidates {
            session_id: "mixed-v4-v6-001".to_string(),
            peer_public_addr: Some(format!("[::1]:{host_v6_port}")),
            peer_lan_addrs: vec![format!("127.0.0.1:{dead_v4_port}")],
            local_udp_port: None,
        };
        let all = parse_candidate_addrs(&mixed_client_cand);
        let setup = prepare_punch_socket_and_targets(&mixed_client_cand, &all)
            .expect("prepare dual-stack socket");
        assert!(
            setup.socket_is_ipv6,
            "mixed IPv4+IPv6 candidates must create AF_INET6 dual-stack socket"
        );
        assert_eq!(
            setup.targets.len(),
            2,
            "must retain both IPv4-mapped and IPv6 targets"
        );
        assert!(
            setup.targets.iter().any(|a| *a
                == format!("[::1]:{host_v6_port}")
                    .parse::<SocketAddr>()
                    .unwrap()),
            "IPv6 peer_public_addr must be retained: {:?}",
            setup.targets
        );
        assert!(
            setup.targets.iter().any(|a| {
                *a == format!("[::ffff:127.0.0.1]:{dead_v4_port}")
                    .parse::<SocketAddr>()
                    .unwrap()
            }),
            "IPv4 LAN candidate must be mapped to ::ffff: on AF_INET6 socket: {:?}",
            setup.targets
        );
        drop(setup);

        // 1) 端到端验证：混合候选 Client 通过双栈 AF_INET6 套接字打通纯 IPv6 Host（IPv4 LAN 不可达）。
        let client_v6_probe = UdpSocket::bind("[::1]:0").expect("bind ipv6 client port");
        let client_v6_port = client_v6_probe.local_addr().unwrap().port();
        drop(client_v6_probe);

        let host_v6_cand = P2pCandidates {
            session_id: "mixed-v4-v6-e2e".to_string(),
            peer_public_addr: Some(format!("[::1]:{client_v6_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_v6_port),
        };
        let client_mixed_cand = P2pCandidates {
            session_id: "mixed-v4-v6-e2e".to_string(),
            peer_public_addr: Some(format!("[::1]:{host_v6_port}")),
            peer_lan_addrs: vec![format!("127.0.0.1:{dead_v4_port}")],
            local_udp_port: Some(client_v6_port),
        };

        let cancel_h = Arc::new(AtomicBool::new(false));
        let cancel_c = Arc::clone(&cancel_h);
        let host_thread = thread::spawn(move || {
            punch_udp_pair(
                P2pRole::Host,
                &host_v6_cand,
                DEFAULT_PUNCH_TIMEOUT,
                &cancel_h,
            )
        });
        let (_c_sock, c_peer, _) = punch_udp_pair(
            P2pRole::Client,
            &client_mixed_cand,
            DEFAULT_PUNCH_TIMEOUT,
            &cancel_c,
        )
        .expect("mixed IPv4+IPv6 client must punch IPv6 host");
        let (_h_sock, h_peer, _) = host_thread
            .join()
            .unwrap()
            .expect("IPv6 host must punch mixed client");
        assert_eq!(c_peer.port(), host_v6_port);
        assert_eq!(h_peer.port(), client_v6_port);

        // 2) 反向验证：双栈 AF_INET6 套接字也能向 IPv4-mapped (::ffff:127.0.0.1) 对端收发而不剥离前缀。
        let host_v4_port = reserve_ephemeral_udp_port();
        let client_dual_port = reserve_ephemeral_udp_port();
        let dead_v6_probe = UdpSocket::bind("[::1]:0").expect("bind dead v6");
        let dead_v6_port = dead_v6_probe.local_addr().unwrap().port();
        drop(dead_v6_probe);

        let host_v4_cand = P2pCandidates {
            session_id: "dual-to-v4-e2e".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_dual_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_v4_port),
        };
        let client_dual_cand = P2pCandidates {
            session_id: "dual-to-v4-e2e".to_string(),
            peer_public_addr: Some(format!("[::1]:{dead_v6_port}")),
            peer_lan_addrs: vec![format!("127.0.0.1:{host_v4_port}")],
            local_udp_port: Some(client_dual_port),
        };
        let cancel_h2 = Arc::new(AtomicBool::new(false));
        let cancel_c2 = Arc::clone(&cancel_h2);
        let host_v4_thread = thread::spawn(move || {
            punch_udp_pair(
                P2pRole::Host,
                &host_v4_cand,
                DEFAULT_PUNCH_TIMEOUT,
                &cancel_h2,
            )
        });
        let (_c2_sock, c2_peer, _) = punch_udp_pair(
            P2pRole::Client,
            &client_dual_cand,
            DEFAULT_PUNCH_TIMEOUT,
            &cancel_c2,
        )
        .expect("dual-stack AF_INET6 socket must punch IPv4 host via ::ffff:");
        let (_h2_sock, _h2_peer, _) = host_v4_thread
            .join()
            .unwrap()
            .expect("IPv4 host must punch dual-stack client");
        assert!(
            c2_peer.is_ipv6(),
            "AF_INET6 socket must preserve ::ffff: address family instead of stripping to V4: {c2_peer}"
        );
        assert_eq!(c2_peer.port(), host_v4_port);
    }

    #[test]
    fn peek_from_preserves_peer_quic_initial_packet_in_kernel_buffer() {
        let host_port = reserve_ephemeral_udp_port();
        let peer_sock = UdpSocket::bind("127.0.0.1:0").expect("bind fake peer");
        let peer_addr = peer_sock.local_addr().unwrap();
        let junk_sock = UdpSocket::bind("127.0.0.1:0").expect("bind unknown junk");

        let session_id = "peek-preserve-quic-initial";
        let session_hash = derive_session_hash(session_id);
        let host_addr = SocketAddr::from(([127, 0, 0, 1], host_port));

        // 构造合成 QUIC Initial 长包头载荷（非 RCP2 魔数）。
        let mut fake_quic_initial = vec![0xc0, 0x00, 0x00, 0x00, 0x01];
        fake_quic_initial.extend_from_slice(b"synthetic-quic-initial-payload-must-stay-intact");
        let expected_quic = fake_quic_initial.clone();

        let peer_thread = thread::spawn(move || {
            peer_sock
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // 先从未知来源发一条非 RCP2 杂散包，验证不会误触发或卡死。
            let _ = junk_sock.send_to(b"random-noise-from-stranger", host_addr);

            // 等待 Host 绑定并开始发送 Probe 后，发一条 Client Probe 触发 Host 确认对端。
            let mut buf = [0u8; 256];
            let _ = peer_sock.recv_from(&mut buf).expect("recv host probe");
            let probe = encode_punch_packet(&PunchPacket {
                version: PUNCH_VERSION,
                role: P2pRole::Client,
                ack: false,
                session_hash,
                nonce: [7; 6],
            });
            peer_sock.send_to(&probe, host_addr).unwrap();

            // 收到 Host 回复的 Ack 后，模拟对端 Ack 丢失但直接切入 QUIC 发出 Initial。
            let (n, _) = peer_sock.recv_from(&mut buf).expect("recv host ack");
            let pkt = decode_punch_packet(&buf[..n]).expect("valid host ack");
            assert!(pkt.ack, "host must reply with ack=true");
            peer_sock.send_to(&fake_quic_initial, host_addr).unwrap();
        });

        let host_cand = P2pCandidates {
            session_id: session_id.to_string(),
            peer_public_addr: Some(peer_addr.to_string()),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let cancel = AtomicBool::new(false);
        let (host_sock, punched_peer, _) =
            punch_udp_pair(P2pRole::Host, &host_cand, DEFAULT_PUNCH_TIMEOUT, &cancel)
                .expect("host punch must exit immediately when peeking peer QUIC Initial");
        peer_thread.join().unwrap();
        assert_eq!(punched_peer, peer_addr);

        // 关键断言：对端的 QUIC Initial 必须完好留在内核接收缓冲中未被 punch_udp_pair 吞掉！
        host_sock
            .set_nonblocking(false)
            .expect("set blocking for verify");
        host_sock
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut verify_buf = [0u8; 256];
        let (n, from) = host_sock
            .recv_from(&mut verify_buf)
            .expect("QUIC Initial packet must still be unread in kernel buffer");
        assert_eq!(from, peer_addr);
        assert_eq!(&verify_buf[..n], expected_quic.as_slice());
    }

    #[test]
    fn one_way_probe_without_ack_does_not_false_positive_confirm() {
        let host_port = reserve_ephemeral_udp_port();
        let peer_sock = UdpSocket::bind("127.0.0.1:0").expect("bind one-way peer");
        let peer_addr = peer_sock.local_addr().unwrap();
        let host_addr = SocketAddr::from(([127, 0, 0, 1], host_port));

        let session_id = "one-way-probe-no-ack";
        let session_hash = derive_session_hash(session_id);
        let probe = encode_punch_packet(&PunchPacket {
            version: PUNCH_VERSION,
            role: P2pRole::Client,
            ack: false,
            session_hash,
            nonce: [3; 6],
        });

        // 模拟单向链路：对端不断发送 Probe（ack=false），永远不发 Ack 也不发 QUIC Initial。
        let stop_sender = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop_sender);
        let sender = thread::spawn(move || {
            while !stop_flag.load(Ordering::Acquire) {
                let _ = peer_sock.send_to(&probe, host_addr);
                thread::sleep(Duration::from_millis(20));
            }
        });

        let host_cand = P2pCandidates {
            session_id: session_id.to_string(),
            peer_public_addr: Some(peer_addr.to_string()),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let cancel = AtomicBool::new(false);
        let res = punch_udp_pair(
            P2pRole::Host,
            &host_cand,
            Duration::from_millis(220),
            &cancel,
        );
        stop_sender.store(true, Ordering::Release);
        sender.join().unwrap();

        let err = res.expect_err("one-way Probe without Ack must NOT false-positive confirm");
        assert!(
            err.contains("超时"),
            "expected timeout error on one-way probe, got: {err}"
        );
    }

    #[test]
    fn p2p_quic_mtls_handshake_verifies_identity_and_rejects_mismatch() {
        use crate::relay_transport::tests::mtls_test_certs::TestCa;
        use crate::relay_transport::SessionMtlsBundle;

        let mut ca = TestCa::new("P2pMtlsCA");
        let (c_cert, c_key) = ca.issue_device("u-p2p", "111222333");
        let (h_cert, h_key) = ca.issue_device("u-p2p", "444555666");

        let client_mtls = SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem.clone(),
            cert_pem: c_cert,
            key_pem: c_key,
            local_user_id: "u-p2p".to_string(),
            expected_peer_device_id: Some("444555666".to_string()),
            expected_peer_user_id: Some("u-p2p".to_string()),
        }
        .to_quic_mtls_config()
        .unwrap();
        let host_bundle = SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem,
            cert_pem: h_cert,
            key_pem: h_key,
            local_user_id: "u-p2p".to_string(),
            expected_peer_device_id: Some("111222333".to_string()),
            expected_peer_user_id: Some("u-p2p".to_string()),
        };
        let host_mtls = host_bundle.to_quic_mtls_config().unwrap();

        // 1) 成功握手并验证对端身份
        let host_port = reserve_ephemeral_udp_port();
        let client_port = reserve_ephemeral_udp_port();
        let host_cand = P2pCandidates {
            session_id: "p2p-mtls-ok".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port),
        };
        let client_cand = P2pCandidates {
            session_id: "p2p-mtls-ok".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_port),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_h = Arc::clone(&cancel);
        let h_jh = thread::spawn(move || {
            attempt_p2p_quic_host_mtls(
                &host_cand,
                Duration::from_secs(2),
                Duration::from_secs(2),
                &cancel_h,
                Some(&host_mtls),
            )
            .expect("host p2p mtls must succeed")
        });
        let mut client_conn = attempt_p2p_quic_client_mtls(
            &client_cand,
            Duration::from_secs(2),
            Duration::from_secs(2),
            &cancel,
            Some(&client_mtls),
        )
        .expect("client p2p mtls must succeed");
        let mut host_conn = h_jh.join().unwrap();

        assert_eq!(
            client_conn.peer_mtls_identity().unwrap().device_id,
            "444555666"
        );
        assert_eq!(
            host_conn.peer_mtls_identity().unwrap().device_id,
            "111222333"
        );

        client_conn.transport.send_reliable(b"p2p-mtls-ok").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got_msg = false;
        while Instant::now() < deadline && !got_msg {
            for ev in host_conn.transport.poll(Duration::from_millis(50)).unwrap() {
                if ev == NetEvent::Reliable(b"p2p-mtls-ok".to_vec()) {
                    got_msg = true;
                }
            }
        }
        assert!(got_msg, "host must receive reliable message over P2P mTLS");
        drop(client_conn);
        drop(host_conn);

        // 2) expected_peer_device_id 不匹配必须拒绝
        let host_port2 = reserve_ephemeral_udp_port();
        let client_port2 = reserve_ephemeral_udp_port();
        let host_cand2 = P2pCandidates {
            session_id: "p2p-mtls-mismatch".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{client_port2}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(host_port2),
        };
        let client_cand2 = P2pCandidates {
            session_id: "p2p-mtls-mismatch".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_port2}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_port2),
        };
        let mut wrong_client_mtls = client_mtls;
        wrong_client_mtls.expected_peer_device_id = Some("999999999".to_string());
        let cancel2 = Arc::new(AtomicBool::new(false));
        let cancel2_h = Arc::clone(&cancel2);
        let host_mtls2 = host_bundle.to_quic_mtls_config().unwrap();
        let h_jh2 = thread::spawn(move || {
            attempt_p2p_quic_host_mtls(
                &host_cand2,
                Duration::from_secs(2),
                Duration::from_millis(800),
                &cancel2_h,
                Some(&host_mtls2),
            )
        });
        let c_res = attempt_p2p_quic_client_mtls(
            &client_cand2,
            Duration::from_secs(2),
            Duration::from_millis(800),
            &cancel2,
            Some(&wrong_client_mtls),
        );
        assert!(c_res.is_err(), "wrong expected_peer_device_id must fail");
        let _ = h_jh2.join();
    }
}
