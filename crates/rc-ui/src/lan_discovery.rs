//! 局域网设备发现（基于 `std::net::UdpSocket` + `socket2` 端口复用多播信标）。
//!
//! 本模块**严禁引入 `tauri` 类型**，全模块 `#![deny(unsafe_code)]`；
//! 状态管理 [`PeerTable`] 遵循铁律 4（所有时间由调用方注入 `now_ms: u64`），
//! 便于离线确定性单测。
//!
//! # 二进制信标格式
//!
//! ```text
//! [0..4]   magic:       b"RCBC"
//! [4]      version:     u8 = 1
//! [5..13]  instance_id: u64 LE
//! [13..15] listen_port: u16 LE
//! [15]     flags:       u8 (bit0 = requires_password)
//! [16]     name_len:    u8 (<= 64)
//! [17..]   name:        UTF-8 字节（精确 name_len 字节）
//! [可选尾部] did_len:   u8 (= 9，仅当存在合法 9 位数字设备码时附带)
//! [可选尾部] device_id: 9 字节 ASCII 数字
//! ```

#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockRef, Socket, Type};

/// 局域网信标魔数。
pub const BEACON_MAGIC: &[u8; 4] = b"RCBC";

/// 当前信标协议版本。
pub const BEACON_VERSION: u8 = 1;

/// 信标固定头部长度（字节）：4 + 1 + 8 + 2 + 1 + 1 = 17。
pub const BEACON_HEADER_LEN: usize = 17;

/// 设备名称编码字节上限（UTF-8 字符边界安全截断）。
pub const MAX_BEACON_NAME_LEN: usize = 64;

/// 标志位 bit 0：被控端是否启用访问密码。
pub const FLAG_REQUIRES_PASSWORD: u8 = 0x01;

/// 局域网多播组地址（本地管理段 `239.255.77.77`）。
pub const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 77);

/// 局域网多播端口。
pub const MULTICAST_PORT: u16 = 19999;

/// 默认信标广播周期。
const BROADCAST_INTERVAL: Duration = Duration::from_millis(1_500);

/// 接收超时分片（兼顾低功耗与 `stop()` 快速回收）。
const RECV_POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// 多播网卡复检周期（检测局域网 IP 变化或补做首次加入失败的重试，U-M4）。
const MULTICAST_IFACE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// 默认局域网节点过期 TTL（毫秒；4 倍广播周期）。
pub const DEFAULT_PEER_TTL_MS: u64 = 6_000;

/// 9 位数字设备码固定长度。
const BEACON_DEVICE_ID_LEN: usize = 9;

/// 校验是否为合法的 9 位纯 ASCII 数字设备码。
fn is_valid_beacon_device_id(id: &str) -> bool {
    id.len() == BEACON_DEVICE_ID_LEN && id.bytes().all(|b| b.is_ascii_digit())
}

/// 解码后的局域网信标载荷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// 发送方随机实例 ID（用于过滤自身回环包与节点去重）。
    pub instance_id: u64,
    /// 被控端监听的 QUIC 端口。
    pub listen_port: u16,
    /// 是否启用了访问密码。
    pub requires_password: bool,
    /// 设备可读名称（编码上限 64 UTF-8 字节）。
    pub name: String,
    /// 可选 9 位纯数字设备码（用于局域网设备无缝回退中继连接）。
    pub device_id: Option<String>,
}

/// 局域网发现的在线被控端节点（经 IPC 推送/拉取至前端列表）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LanPeer {
    /// 对端实例 ID。
    pub instance_id: u64,
    /// 对端设备可读名称。
    pub name: String,
    /// 对端直连地址（`ip:port`）。
    pub addr: String,
    /// 对端是否需要密码。
    pub requires_password: bool,
    /// 对端可选 9 位数字设备码。
    pub device_id: Option<String>,
}

/// 将 `s` 按 UTF-8 字符边界安全截断至不超过 `max_bytes` 字节。
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 编码局域网信标为纯二进制报文。
///
/// 设备名超过 [`MAX_BEACON_NAME_LEN`] 字节时自动在 UTF-8 字符边界截断；
/// 仅当 `device_id` 为合法 9 位数字时附带向后兼容尾部段。
pub fn encode_beacon(beacon: &Beacon) -> Vec<u8> {
    let name = truncate_utf8(&beacon.name, MAX_BEACON_NAME_LEN);
    let name_bytes = name.as_bytes();
    let flags = if beacon.requires_password {
        FLAG_REQUIRES_PASSWORD
    } else {
        0
    };
    let valid_device_id = beacon
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|s| is_valid_beacon_device_id(s));
    let tail_len = if valid_device_id.is_some() {
        1 + BEACON_DEVICE_ID_LEN
    } else {
        0
    };
    let mut out = Vec::with_capacity(BEACON_HEADER_LEN + name_bytes.len() + tail_len);
    out.extend_from_slice(BEACON_MAGIC);
    out.push(BEACON_VERSION);
    out.extend_from_slice(&beacon.instance_id.to_le_bytes());
    out.extend_from_slice(&beacon.listen_port.to_le_bytes());
    out.push(flags);
    out.push(name_bytes.len() as u8);
    out.extend_from_slice(name_bytes);
    if let Some(did) = valid_device_id {
        out.push(BEACON_DEVICE_ID_LEN as u8);
        out.extend_from_slice(did.as_bytes());
    }
    out
}

/// 解码局域网信标（严格校验魔数、版本、精确长度、名称上限与合法 UTF-8；兼容无尾部设备码的旧版 v1 报文）。
pub fn decode_beacon(buf: &[u8]) -> Option<Beacon> {
    if buf.len() < BEACON_HEADER_LEN {
        return None;
    }
    if &buf[0..4] != BEACON_MAGIC {
        return None;
    }
    if buf[4] != BEACON_VERSION {
        return None;
    }
    let instance_id = u64::from_le_bytes(buf[5..13].try_into().ok()?);
    let listen_port = u16::from_le_bytes(buf[13..15].try_into().ok()?);
    let flags = buf[15];
    let name_len = usize::from(buf[16]);
    if name_len > MAX_BEACON_NAME_LEN {
        return None;
    }
    let name_end = BEACON_HEADER_LEN + name_len;
    if buf.len() < name_end {
        return None;
    }
    let name = std::str::from_utf8(&buf[BEACON_HEADER_LEN..name_end])
        .ok()?
        .to_string();
    let device_id = if buf.len() == name_end {
        None
    } else if buf.len() == name_end + 1 + BEACON_DEVICE_ID_LEN
        && usize::from(buf[name_end]) == BEACON_DEVICE_ID_LEN
    {
        let did = std::str::from_utf8(&buf[name_end + 1..]).ok()?;
        if !is_valid_beacon_device_id(did) {
            return None;
        }
        Some(did.to_string())
    } else {
        return None;
    };
    Some(Beacon {
        instance_id,
        listen_port,
        requires_password: (flags & FLAG_REQUIRES_PASSWORD) != 0,
        name,
        device_id,
    })
}

/// 节点表内部条目。
#[derive(Debug, Clone)]
struct PeerEntry {
    peer: LanPeer,
    last_seen_ms: u64,
}

/// 局域网在线节点表（纯确定性状态机，所有时间戳由调用方注入 `now_ms`，铁律 4）。
#[derive(Debug, Clone)]
pub struct PeerTable {
    self_instance_id: u64,
    entries: BTreeMap<u64, PeerEntry>,
}

impl PeerTable {
    /// 构造空节点表，指定本机 `self_instance_id` 以过滤自身回环广播。
    pub fn new(self_instance_id: u64) -> Self {
        Self {
            self_instance_id,
            entries: BTreeMap::new(),
        }
    }

    /// 插入或刷新一条局域网节点记录；自动忽略自身回环包与端口为 0 的无效包。
    pub fn upsert(&mut self, peer_ip: IpAddr, beacon: Beacon, now_ms: u64) {
        if beacon.instance_id == self.self_instance_id || beacon.listen_port == 0 {
            return;
        }
        let addr = SocketAddr::new(peer_ip, beacon.listen_port).to_string();
        let peer = LanPeer {
            instance_id: beacon.instance_id,
            name: beacon.name,
            addr,
            requires_password: beacon.requires_password,
            device_id: beacon.device_id,
        };
        self.entries.insert(
            beacon.instance_id,
            PeerEntry {
                peer,
                last_seen_ms: now_ms,
            },
        );
    }

    /// 按注入时间 `now_ms` 淘汰超时未刷新（`now_ms - last_seen_ms >= ttl_ms`）的节点。
    pub fn prune(&mut self, now_ms: u64, ttl_ms: u64) {
        self.entries
            .retain(|_, entry| now_ms.saturating_sub(entry.last_seen_ms) < ttl_ms);
    }

    /// 导出当前在线节点快照（按名称、地址、实例 ID 稳定排序）。
    pub fn snapshot(&self) -> Vec<LanPeer> {
        let mut peers: Vec<LanPeer> = self.entries.values().map(|e| e.peer.clone()).collect();
        peers.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.addr.cmp(&b.addr))
                .then_with(|| a.instance_id.cmp(&b.instance_id))
        });
        peers
    }

    /// 按 9 位设备码（允许空格/短横线）查询最近活跃的局域网直连地址，供三阶路由优先尝试 `UDP LAN`。
    pub fn lookup_addr(&self, device_id: &str) -> Option<SocketAddr> {
        let normalized = crate::server_config::parse_device_id_input(device_id)?;
        self.entries
            .values()
            .filter(|entry| entry.peer.device_id.as_deref() == Some(normalized.as_str()))
            .max_by_key(|entry| (entry.last_seen_ms, entry.peer.instance_id))
            .and_then(|entry| entry.peer.addr.parse::<SocketAddr>().ok())
    }
}

/// 探测本机出向局域网 IPv4 地址（纯标准库 UDP `connect` 路由表查询，发送 0 个报文）。
pub fn detect_local_lan_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// 获取本机可读设备名（优先 macOS `scutil`，次选 `HOSTNAME` 环境变量，兜底 `"本机 Mac"`）。
pub fn local_hostname() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
        {
            if output.status.success() {
                if let Ok(name) = String::from_utf8(output.stdout) {
                    let trimmed = name.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "本机 Mac".to_string()
}

/// 局域网发现后台线程句柄。
///
/// 同时负责接收同网段其他 RabbitControl 实例的信标与按需广播本机被控端信息；
/// 若多播端口 `19999` 被占用或多播不可用，自动静默降级，绝不 panic 或阻塞应用启动。
pub struct LanDiscoveryHandle {
    advertising: Arc<Mutex<Option<(u16, bool)>>>,
    local_device_id: Arc<Mutex<Option<String>>>,
    advertising_dirty: Arc<AtomicBool>,
    peers: Arc<Mutex<PeerTable>>,
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LanDiscoveryHandle {
    /// 启动局域网发现后台线程（默认不广播，仅监听；调用 [`Self::set_advertising`] 开启广播）。
    pub fn spawn(instance_id: u64, device_name: String) -> Self {
        let advertising = Arc::new(Mutex::new(None));
        let local_device_id = Arc::new(Mutex::new(None));
        let advertising_dirty = Arc::new(AtomicBool::new(false));
        let peers = Arc::new(Mutex::new(PeerTable::new(instance_id)));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let t_adv = Arc::clone(&advertising);
        let t_did = Arc::clone(&local_device_id);
        let t_dirty = Arc::clone(&advertising_dirty);
        let t_peers = Arc::clone(&peers);
        let t_stop = Arc::clone(&stop_flag);

        let handle = thread::Builder::new()
            .name("rc-ui-lan-discovery".to_string())
            .spawn(move || {
                run_discovery_loop(
                    instance_id,
                    device_name,
                    t_adv,
                    t_did,
                    t_dirty,
                    t_peers,
                    t_stop,
                );
            })
            .expect("spawn rc-ui-lan-discovery thread");

        Self {
            advertising,
            local_device_id,
            advertising_dirty,
            peers,
            stop_flag,
            thread: Some(handle),
        }
    }

    /// 设置本机广播状态：`Some((listen_port, requires_password))` 开启广播，`None` 停止广播。
    pub fn set_advertising(&self, target: Option<(u16, bool)>) {
        *self.advertising.lock().unwrap_or_else(|e| e.into_inner()) = target;
        self.advertising_dirty.store(true, Ordering::Release);
    }

    /// 设置本机关联的 9 位数字设备码（仅合法 9 位数字会随局域网信标广播）。
    pub fn set_local_device_id(&self, device_id: Option<String>) {
        let normalized = device_id
            .map(|s| s.trim().to_string())
            .filter(|s| is_valid_beacon_device_id(s));
        *self
            .local_device_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = normalized;
        self.advertising_dirty.store(true, Ordering::Release);
    }

    /// 获取当前在线局域网节点快照。
    pub fn peers(&self) -> Vec<LanPeer> {
        self.peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    /// 查询指定 9 位设备码最近活跃的局域网直连地址（`UDP LAN` 路由快查）。
    pub fn lookup_lan_addr(&self, device_id: &str) -> Option<SocketAddr> {
        self.peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lookup_addr(device_id)
    }

    /// 停止发现线程并等待回收（幂等）。
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LanDiscoveryHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 多播套接字与网卡加入状态（用于 U-M4 周期复检与重加组）。
struct DiscoverySocket {
    sock: UdpSocket,
    can_recv: bool,
    lan_iface: Option<Ipv4Addr>,
    joined_ok: bool,
}

impl DiscoverySocket {
    /// 绑定多播套接字：通过 `socket2` 开启 `SO_REUSEADDR`（Unix 下同时开启 `SO_REUSEPORT`）
    /// 后绑定 `0.0.0.0:19999` 收发双向；绑定失败时降级为临时端口只发送（U-M3 / U-M4）。
    fn bind() -> Option<Self> {
        let lan_iface = detect_local_lan_ipv4();
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MULTICAST_PORT);

        if let Ok(raw) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
            let _ = raw.set_reuse_address(true);
            #[cfg(unix)]
            let _ = raw.set_reuse_port(true);
            if raw.bind(&bind_addr.into()).is_ok() {
                let sock: UdpSocket = raw.into();
                let _ = sock.set_read_timeout(Some(RECV_POLL_TIMEOUT));
                // 开启多播回环，依靠 `PeerTable::self_instance_id` 过滤自身回环包，
                // 从而支持同机多实例互发现（U-M3）。
                let _ = sock.set_multicast_loop_v4(true);
                if let Some(ip) = lan_iface {
                    let _ = SockRef::from(&sock).set_multicast_if_v4(&ip);
                }
                let join_iface = lan_iface.unwrap_or(Ipv4Addr::UNSPECIFIED);
                let joined_ok = sock.join_multicast_v4(&MULTICAST_ADDR, &join_iface).is_ok();
                return Some(Self {
                    sock,
                    can_recv: true,
                    lan_iface,
                    joined_ok,
                });
            }
        }

        if let Ok(sock) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) {
            let _ = sock.set_read_timeout(Some(RECV_POLL_TIMEOUT));
            let _ = sock.set_multicast_loop_v4(true);
            if let Some(ip) = lan_iface {
                let _ = SockRef::from(&sock).set_multicast_if_v4(&ip);
            }
            return Some(Self {
                sock,
                can_recv: false,
                lan_iface,
                joined_ok: false,
            });
        }
        None
    }

    /// 检查本机局域网 IPv4 是否变化或首次入组失败；必要时重新设置出向网卡并重加多播组（U-M4）。
    fn refresh_multicast_iface(&mut self) {
        let current_lan = detect_local_lan_ipv4();
        let iface_changed = current_lan != self.lan_iface;
        if !iface_changed && (!self.can_recv || self.joined_ok) {
            return;
        }
        if self.can_recv && self.joined_ok && iface_changed {
            let old_iface = self.lan_iface.unwrap_or(Ipv4Addr::UNSPECIFIED);
            let _ = self.sock.leave_multicast_v4(&MULTICAST_ADDR, &old_iface);
            self.joined_ok = false;
        }
        self.lan_iface = current_lan;
        if let Some(ip) = current_lan {
            let _ = SockRef::from(&self.sock).set_multicast_if_v4(&ip);
        }
        if self.can_recv {
            let join_iface = current_lan.unwrap_or(Ipv4Addr::UNSPECIFIED);
            self.joined_ok = self
                .sock
                .join_multicast_v4(&MULTICAST_ADDR, &join_iface)
                .is_ok();
        }
    }
}

fn run_discovery_loop(
    instance_id: u64,
    device_name: String,
    advertising: Arc<Mutex<Option<(u16, bool)>>>,
    local_device_id: Arc<Mutex<Option<String>>>,
    advertising_dirty: Arc<AtomicBool>,
    peers: Arc<Mutex<PeerTable>>,
    stop_flag: Arc<AtomicBool>,
) {
    let origin = Instant::now();
    let target = SocketAddrV4::new(MULTICAST_ADDR, MULTICAST_PORT);
    let mut socket_opt = DiscoverySocket::bind();
    let mut last_broadcast: Option<Instant> = None;
    let mut last_iface_check = Instant::now();
    let mut recv_buf = [0u8; 256];

    while !stop_flag.load(Ordering::Acquire) {
        let now_ms = u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX);

        if let Some(ref mut ds) = socket_opt {
            if last_iface_check.elapsed() >= MULTICAST_IFACE_CHECK_INTERVAL {
                last_iface_check = Instant::now();
                ds.refresh_multicast_iface();
            }

            // 1) 定期或在状态变更时发送本机信标。
            let adv = *advertising.lock().unwrap_or_else(|e| e.into_inner());
            let dirty = advertising_dirty.swap(false, Ordering::AcqRel);
            let should_send = dirty
                || last_broadcast
                    .map(|t| t.elapsed() >= BROADCAST_INTERVAL)
                    .unwrap_or(true);
            if should_send {
                if let Some((listen_port, requires_password)) = adv {
                    let device_id = local_device_id
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let pkt = encode_beacon(&Beacon {
                        instance_id,
                        listen_port,
                        requires_password,
                        name: device_name.clone(),
                        device_id,
                    });
                    let _ = ds.sock.send_to(&pkt, target);
                    last_broadcast = Some(Instant::now());
                } else {
                    last_broadcast = None;
                }
            }

            // 2) 接收同网段信标（200ms 超时分片，不阻塞停机）。
            if ds.can_recv {
                match ds.sock.recv_from(&mut recv_buf) {
                    Ok((len, src)) => {
                        if let Some(beacon) = decode_beacon(&recv_buf[..len]) {
                            let rx_ms =
                                u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX);
                            peers.lock().unwrap_or_else(|e| e.into_inner()).upsert(
                                src.ip(),
                                beacon,
                                rx_ms,
                            );
                        }
                    }
                    Err(_) => {
                        // 超时或临时网络错误静默忽略。
                    }
                }
            } else {
                thread::sleep(RECV_POLL_TIMEOUT);
            }
        } else {
            // 极端受限环境无法绑定任何 UDP 套接字：降级空转等待退出。
            thread::sleep(RECV_POLL_TIMEOUT);
        }

        // 3) 清理过期节点。
        peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prune(now_ms, DEFAULT_PEER_TTL_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacon_roundtrip_and_truncation() {
        let normal = Beacon {
            instance_id: 0x0123_4567_89AB_CDEF,
            listen_port: 9901,
            requires_password: true,
            name: "Studio Mac".to_string(),
            device_id: None,
        };
        let encoded = encode_beacon(&normal);
        assert_eq!(&encoded[0..4], b"RCBC");
        assert_eq!(encoded[4], 1);
        assert_eq!(decode_beacon(&encoded), Some(normal));

        // 带合法 9 位设备码的编码/解码往返
        let with_id = Beacon {
            instance_id: 0x1122_3344_5566_7788,
            listen_port: 9901,
            requires_password: false,
            name: "Studio Mac".to_string(),
            device_id: Some("604928235".to_string()),
        };
        let encoded_with_id = encode_beacon(&with_id);
        assert_eq!(encoded_with_id.len(), encoded.len() + 10);
        assert_eq!(decode_beacon(&encoded_with_id), Some(with_id));

        // 旧版 v1 载荷（无尾部段）仍可解码且 device_id == None
        assert_eq!(decode_beacon(&encoded).and_then(|b| b.device_id), None);

        // 非法 device_id 在编码时被忽略（不附带尾部段，与 v1 字节数完全一致）
        for bad_id in ["12345678", "1234567890", "604 928 235", "60492823a", ""] {
            let invalid = Beacon {
                instance_id: 42,
                listen_port: 9901,
                requires_password: false,
                name: "BadId".to_string(),
                device_id: Some(bad_id.to_string()),
            };
            let pkt = encode_beacon(&invalid);
            assert_eq!(pkt.len(), BEACON_HEADER_LEN + "BadId".len());
            assert_eq!(decode_beacon(&pkt).and_then(|b| b.device_id), None);
        }

        // 畸形尾部设备码段必须拒绝
        let mut bad_tail_len = encoded_with_id.clone();
        bad_tail_len[encoded.len()] = 8;
        assert_eq!(decode_beacon(&bad_tail_len), None);

        let mut bad_tail_digit = encoded_with_id;
        let tail_last = bad_tail_digit.len() - 1;
        bad_tail_digit[tail_last] = b'x';
        assert_eq!(decode_beacon(&bad_tail_digit), None);

        // 多字节 UTF-8 超长名称（每个中文字符 3 字节，25 个 = 75 字节 > 64）须按字符边界截断至 63 字节。
        let long_name = "兔".repeat(25);
        assert_eq!(long_name.len(), 75);
        let long_beacon = Beacon {
            instance_id: 42,
            listen_port: 19901,
            requires_password: false,
            name: long_name,
            device_id: Some("100200300".to_string()),
        };
        let encoded_long = encode_beacon(&long_beacon);
        let decoded_long = decode_beacon(&encoded_long).expect("truncated beacon should decode");
        assert!(decoded_long.name.len() <= MAX_BEACON_NAME_LEN);
        assert_eq!(decoded_long.name.len(), 63);
        assert_eq!(decoded_long.name, "兔".repeat(21));
        assert!(!decoded_long.requires_password);
        assert_eq!(decoded_long.device_id.as_deref(), Some("100200300"));

        // 非法魔数、错误版本、尾部截断、额外尾字节、非法 UTF-8 均应拒绝。
        let mut bad_magic = encoded.clone();
        bad_magic[0] = b'X';
        assert_eq!(decode_beacon(&bad_magic), None);

        let mut bad_ver = encoded.clone();
        bad_ver[4] = 2;
        assert_eq!(decode_beacon(&bad_ver), None);

        assert_eq!(decode_beacon(&encoded[..encoded.len() - 1]), None);

        let mut extra_tail = encoded.clone();
        extra_tail.push(0);
        assert_eq!(decode_beacon(&extra_tail), None);

        let mut bad_utf8 = encoded;
        let last = bad_utf8.len() - 1;
        bad_utf8[last] = 0xFF;
        assert_eq!(decode_beacon(&bad_utf8), None);
    }

    #[test]
    fn peer_table_ignores_self_and_expires_by_injected_clock() {
        let mut table = PeerTable::new(100);
        let ip_a = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let ip_b = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));

        // 自身回环信标必须忽略。
        table.upsert(
            ip_a,
            Beacon {
                instance_id: 100,
                listen_port: 9901,
                requires_password: true,
                name: "Self".to_string(),
                device_id: Some("111222333".to_string()),
            },
            1_000,
        );
        assert!(table.snapshot().is_empty());

        // 端口 0 的无效信标必须忽略。
        table.upsert(
            ip_a,
            Beacon {
                instance_id: 200,
                listen_port: 0,
                requires_password: false,
                name: "ZeroPort".to_string(),
                device_id: None,
            },
            1_000,
        );
        assert!(table.snapshot().is_empty());

        // 正常插入两个远端节点。
        table.upsert(
            ip_b,
            Beacon {
                instance_id: 202,
                listen_port: 9902,
                requires_password: false,
                name: "Beta Mac".to_string(),
                device_id: None,
            },
            1_000,
        );
        table.upsert(
            ip_a,
            Beacon {
                instance_id: 201,
                listen_port: 9901,
                requires_password: true,
                name: "Alpha Mac".to_string(),
                device_id: Some("604928235".to_string()),
            },
            2_500,
        );

        let snap = table.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].name, "Alpha Mac");
        assert_eq!(snap[0].addr, "192.168.1.10:9901");
        assert!(snap[0].requires_password);
        assert_eq!(snap[0].device_id.as_deref(), Some("604928235"));
        assert_eq!(snap[1].name, "Beta Mac");
        assert_eq!(snap[1].addr, "192.168.1.20:9902");
        assert!(!snap[1].requires_password);
        assert_eq!(snap[1].device_id, None);

        // 注入时钟推进到 7_000ms（TTL=6_000ms）：Beta(1_000) 刚好到期被清除，Alpha(2_500) 保留。
        table.prune(7_000, DEFAULT_PEER_TTL_MS);
        let snap2 = table.snapshot();
        assert_eq!(snap2.len(), 1);
        assert_eq!(snap2[0].instance_id, 201);

        // 注入时钟推进到 8_500ms：Alpha(2_500) 也到期清空。
        table.prune(8_500, DEFAULT_PEER_TTL_MS);
        assert!(table.snapshot().is_empty());
    }

    /// 回归测试（U-M3）：开启 `SO_REUSEADDR`/`SO_REUSEPORT` 与多播回环后，
    /// 同一主机上两个发现实例可同时绑定端口 19999 并启用回环接收。
    #[test]
    fn discovery_socket_supports_multi_instance_reuse_and_loopback() {
        let a = DiscoverySocket::bind().expect("first discovery socket");
        let b = DiscoverySocket::bind().expect("second discovery socket");
        assert!(a.can_recv, "首个实例应成功绑定多播端口");
        #[cfg(unix)]
        assert!(
            b.can_recv,
            "Unix 下开启 SO_REUSEPORT 后第二实例也应绑定成功"
        );
        assert!(a.sock.multicast_loop_v4().unwrap_or(false));
        assert!(b.sock.multicast_loop_v4().unwrap_or(false));
    }

    #[test]
    fn peer_table_lookup_addr_normalizes_and_picks_freshest() {
        let mut table = PeerTable::new(100);
        table.upsert(
            IpAddr::V4(Ipv4Addr::new(192, 168, 10, 5)),
            Beacon {
                instance_id: 1,
                listen_port: 9901,
                requires_password: false,
                name: "Old".to_string(),
                device_id: Some("604928235".to_string()),
            },
            1_000,
        );
        table.upsert(
            IpAddr::V4(Ipv4Addr::new(192, 168, 10, 6)),
            Beacon {
                instance_id: 2,
                listen_port: 9902,
                requires_password: true,
                name: "New".to_string(),
                device_id: Some("604928235".to_string()),
            },
            2_000,
        );

        assert_eq!(
            table.lookup_addr("604 928-235"),
            Some("192.168.10.6:9902".parse().unwrap())
        );
        assert_eq!(table.lookup_addr("999999999"), None);
        assert_eq!(table.lookup_addr("bad"), None);
    }
}
