//! 原生 TCP 定长帧信令客户端与 UDP `RCSP` 公网地址探测。
//!
//! 严格对齐 `server/docs/02-protocol-and-api.md`（**不使用 WebSocket**）：
//! - TCP 信令端口（默认 `:21116`）：`[u32 BE len (1..=65536)][UTF-8 JSON]`；
//! - UDP 探测端口（默认 `:21116`）：22 字节 `RCSP` 请求包 `[b"RCSP"][0x01][device_id 9B][nonce 8B]`；
//! - 断线指数退避重连：`1s -> 2s -> 5s -> 10s` 上限（仅当连接健康持续 `>= 30s` 且非被踢时重置退避）；
//! - 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, SockAddr, SockRef, Socket, TcpKeepalive, Type};

use crate::cert_manager::OwnerGrantPayload;
use crate::server_config::{
    format_device_id, format_host_port, next_status_revision, parse_device_id_input,
    parse_server_input, ServerConfig, ServerConnectionState, ServerStatusSnapshot,
};

/// 单条 TCP 信令 JSON 帧的最大字节数（对齐服务端 `MAX_SIGNAL_FRAME_LEN`）。
pub const MAX_SIGNAL_FRAME_LEN: usize = 65_536;

/// UDP `RCSP` 探测包魔数。
const UDP_PROBE_MAGIC: &[u8; 4] = b"RCSP";
/// UDP `RCSP` 探测协议版本。
const UDP_PROTO_VERSION: u8 = 1;
/// UDP `RCSP` 探测包定长字节数。
pub const UDP_PROBE_LEN: usize = 22;

/// 心跳与 UDP 探测发送周期（30 秒）。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// 服务端失活判定阈值（`2 * 30s + 15s`）：超时无任何入站帧视为半开死链并重连。
const SERVER_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(75);
/// 判定连接健康稳定、可重置退避的最短持续时长（30 秒）。
const STABLE_CONNECTION_THRESHOLD: Duration = Duration::from_secs(30);
/// 建连与首包注册握手超时。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 读循环空闲检查粒度（用于响应 `stop()`、心跳、失活超时并最小化 TLS 写端等待）。
const READ_POLL_TIMEOUT: Duration = Duration::from_millis(50);
/// 已读到半帧后允许补齐整帧的上限时间。
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// 断线重连退避序列（1s -> 2s -> 5s -> 10s 上限）。
const RECONNECT_BACKOFFS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
/// 入站中继队列最大容量（满载丢最旧，防止内存增长）。
const INCOMING_QUEUE_CAP: usize = 64;
/// 状态快照通知队列最大容量（满载丢最旧）。
const STATUS_QUEUE_CAP: usize = 32;
/// 同账号设备状态变更事件队列最大容量（满载丢最旧）。
const DEVICE_STATUS_QUEUE_CAP: usize = 128;
/// 踢线原因通知队列最大容量（满载丢最旧）。
const KICKED_REASON_QUEUE_CAP: usize = 16;
/// 多候选地址拨号时的单地址最小超时预算。
const MIN_ADDR_TIMEOUT: Duration = Duration::from_millis(250);
/// TCP Keepalive 空闲探测时间（30s）。
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
/// TCP Keepalive 探测间隔（10s）。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// 上报给服务端的 `hostname` 最大 UTF-8 字节数。
const MAX_HOSTNAME_BYTES: usize = 128;
/// 上报给服务端的单条 `lan_addr` 最大字节数。
const MAX_LAN_ADDR_BYTES: usize = 64;
/// 上报给服务端的 `lan_addrs` 最大条数。
const MAX_LAN_ADDRS: usize = 16;

/// 协助开关原子编码：未显式设置（沿用服务端已有记录）。
const ASSIST_UNSET: u8 = 0;
/// 协助开关原子编码：关闭他人远程协助。
const ASSIST_FALSE: u8 = 1;
/// 协助开关原子编码：允许他人远程协助。
const ASSIST_TRUE: u8 = 2;

/// 服务端推送的入站中继会话请求（本端作为被控端 `host` 接收）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelayIncoming {
    /// 服务端分配或透传的会话标识。
    pub session_id: String,
    /// 中继服务器 TCP 地址（如 `"172.16.1.2:21117"`）。
    pub relay_addr: String,
    /// 一次性 `RCR1` 中继令牌。
    pub relay_token: String,
    /// 发起连接的对端 9 位设备码。
    pub peer_id: String,
    /// 信令服务器观测到的对端公网 UDP 地址（空串归一化为 `None`）。
    pub peer_public_addr: Option<String>,
    /// 对端上报的局域网候选地址列表。
    pub peer_lan_addrs: Vec<String>,
    /// 本端持久化 UDP `RCSP` 探测套接字绑定的本地端口（供 P2P 同端口复用以对齐 NAT 映射）。
    pub local_udp_port: Option<u16>,
    /// 可选同账号自控一次性免密票据载荷（一般仅控制端下发，被控端通常为 `None`）。
    pub owner_grant: Option<OwnerGrantPayload>,
    /// 本次连接是否为同一登录账号的自控连接。
    pub is_owner_connection: bool,
}

/// 出站连接请求获批后返回的中继/打洞凭据（本端作为控制端 `client` 接收）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectGrant {
    /// 会话标识。
    pub session_id: String,
    /// 中继服务器 TCP 地址（如 `"172.16.1.2:21117"`）。
    pub relay_addr: String,
    /// 一次性 `RCR1` 中继令牌。
    pub relay_token: String,
    /// 目标被控端的 9 位设备码。
    pub target_id: String,
    /// 信令服务器观测到的对端公网 UDP 地址（空串归一化为 `None`）。
    pub peer_public_addr: Option<String>,
    /// 对端上报的局域网候选地址列表。
    pub peer_lan_addrs: Vec<String>,
    /// 本端持久化 UDP `RCSP` 探测套接字绑定的本地端口（供 P2P 同端口复用以对齐 NAT 映射）。
    pub local_udp_port: Option<u16>,
    /// 同账号自控时由服务端签发的一次性 `OwnerGrant` 票据载荷。
    pub owner_grant: Option<OwnerGrantPayload>,
    /// 本次连接是否为同一登录账号的自控连接。
    pub is_owner_connection: bool,
}

/// 同账号其他设备的上下线与远程协助状态变更通知（由信令服务端广播）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatusEvent {
    /// 9 位纯数字设备码。
    pub device_id: String,
    /// 当前是否在线。
    pub online: bool,
    /// 设备主机名。
    pub hostname: String,
    /// 用户自定义设备别名。
    pub alias: String,
    /// 操作系统名称。
    pub os: String,
    /// 是否允许他人远程协助。
    pub allow_remote_assist: bool,
}

/// 动态 Access Token 提供器类型（由上层接入 `AuthSessionManager::ensure_valid_access_token`）。
pub type TokenProvider = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// 信令客户端启动扩展参数（注入动态 Token 提供器、设备公钥与远程协助初始开关）。
#[derive(Clone, Default)]
pub struct SignalClientOptions {
    /// 可选动态 Access Token 提供器；每次建连与断线重连注册时自动拉取最新（必要时已轮换的）令牌。
    pub token_provider: Option<TokenProvider>,
    /// 可选本机设备 Ed25519 公钥（64 位小写十六进制）。
    pub device_pubkey: Option<String>,
    /// 初始是否允许他人远程协助（默认 `false`）。
    pub allow_remote_assist: bool,
}

impl fmt::Debug for SignalClientOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalClientOptions")
            .field("has_token_provider", &self.token_provider.is_some())
            .field("device_pubkey", &self.device_pubkey)
            .field("allow_remote_assist", &self.allow_remote_assist)
            .finish()
    }
}

/// 客户端 -> 服务端信令帧（`type` 标签 `snake_case`）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutgoingSignalMsg {
    Register {
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<String>,
        device_key: String,
        hostname: String,
        os: String,
        device_group: String,
        lan_addrs: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        access_token: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_pubkey: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        allow_remote_assist: Option<bool>,
    },
    Heartbeat {
        lan_addrs: Vec<String>,
    },
    UpdateAssistState {
        allow_remote_assist: bool,
    },
    ConnectRequest {
        target_id: String,
        session_id: String,
        lan_addrs: Vec<String>,
        nonce: String,
    },
}

/// 服务端 -> 客户端信令帧（`type` 标签 `snake_case`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum IncomingSignalMsg {
    Registered {
        device_id: String,
        #[serde(default)]
        formatted_id: Option<String>,
        #[serde(default)]
        udp_probe_addr: Option<String>,
        #[serde(default)]
        relay_addr: Option<String>,
    },
    HeartbeatAck {},
    DeviceStatusChanged {
        device_id: String,
        #[serde(default)]
        online: bool,
        #[serde(default)]
        hostname: String,
        #[serde(default)]
        alias: String,
        #[serde(default)]
        os: String,
        #[serde(default)]
        allow_remote_assist: bool,
    },
    PunchCoord {
        session_id: String,
        #[serde(default)]
        role: String,
        peer_id: String,
        #[serde(default, alias = "peer_public_addr")]
        peer_public_udp_addr: Option<String>,
        #[serde(default)]
        peer_lan_addrs: Vec<String>,
        relay_token: String,
        #[serde(default)]
        relay_addr: String,
        #[serde(default)]
        owner_grant: Option<OwnerGrantPayload>,
        #[serde(default)]
        is_owner_connection: bool,
    },
    RelayReady {
        session_id: String,
        #[serde(default)]
        role: String,
        peer_id: String,
        relay_token: String,
        #[serde(default)]
        relay_addr: String,
        #[serde(default)]
        owner_grant: Option<OwnerGrantPayload>,
        #[serde(default)]
        is_owner_connection: bool,
    },
    PeerOffline {
        target_id: String,
        #[serde(default)]
        session_id: String,
    },
    Kicked {
        #[serde(default)]
        reason: String,
    },
    Error {
        #[serde(default)]
        code: String,
        #[serde(default)]
        message: String,
    },
}

#[derive(Clone, Copy)]
struct SignalTiming {
    heartbeat_interval: Duration,
    activity_timeout: Duration,
    stable_threshold: Duration,
    backoffs: [Duration; 4],
}

impl Default for SignalTiming {
    fn default() -> Self {
        Self {
            heartbeat_interval: HEARTBEAT_INTERVAL,
            activity_timeout: SERVER_ACTIVITY_TIMEOUT,
            stable_threshold: STABLE_CONNECTION_THRESHOLD,
            backoffs: RECONNECT_BACKOFFS,
        }
    }
}

struct PendingConnect {
    target_id: String,
    tx: mpsc::SyncSender<Result<ConnectGrant, String>>,
}

type SharedTlsStream = Arc<Mutex<Option<Box<StreamOwned<ClientConnection, TcpStream>>>>>;

enum SignalWriter {
    Plain(TcpStream),
    Tls(SharedTlsStream),
}

impl SignalWriter {
    fn write_msg(&mut self, msg: &OutgoingSignalMsg) -> Result<(), String> {
        match self {
            Self::Plain(stream) => write_signal_frame(stream, msg),
            Self::Tls(shared_tls) => {
                let mut guard = shared_tls.lock().unwrap_or_else(|e| e.into_inner());
                let Some(tls) = guard.as_mut() else {
                    return Err("信令 TLS 连接已关闭".to_string());
                };
                let res = write_signal_frame_tls(tls, msg);
                if res.is_err() {
                    if let Some(broken) = guard.take() {
                        let _ = broken.sock.shutdown(Shutdown::Both);
                    }
                }
                res
            }
        }
    }

    fn shutdown_and_close(self) {
        match self {
            Self::Plain(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            Self::Tls(shared_tls) => {
                let mut guard = shared_tls.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(tls) = guard.take() {
                    let _ = tls.sock.shutdown(Shutdown::Both);
                }
            }
        }
    }
}

struct SharedState {
    status: Mutex<ServerStatusSnapshot>,
    status_updates: Mutex<VecDeque<ServerStatusSnapshot>>,
    incoming: Mutex<VecDeque<RelayIncoming>>,
    device_status_events: Mutex<VecDeque<DeviceStatusEvent>>,
    kicked_reasons: Mutex<VecDeque<String>>,
    pending_connects: Mutex<HashMap<String, PendingConnect>>,
    active_writer: Mutex<Option<SignalWriter>>,
    /// 独立于 `active_writer` 的关闭句柄：`signal_stop()` 永不等待写锁即可瞬间切断套接字。
    shutdown_stream: Mutex<Option<TcpStream>>,
    stop_flag: AtomicBool,
    stop_lock: Mutex<bool>,
    stop_cv: Condvar,
    /// 持久化 UDP `RCSP` 探测套接字的本地端口（0 表示未绑定）。
    local_udp_port: AtomicU16,
    /// 持久化 UDP `RCSP` 探测套接字是否绑定在 IPv6 地址族（仅在 `local_udp_port != 0` 时有效）。
    local_udp_is_ipv6: AtomicBool,
    lan_addrs: Vec<String>,
    enabled: bool,
    token_provider: Option<TokenProvider>,
    device_pubkey: Option<String>,
    allow_remote_assist: AtomicU8,
}

fn snapshots_same_payload(a: &ServerStatusSnapshot, b: &ServerStatusSnapshot) -> bool {
    a.enabled == b.enabled
        && a.server_host == b.server_host
        && a.signal_port == b.signal_port
        && a.relay_port == b.relay_port
        && a.state == b.state
        && a.device_id == b.device_id
        && a.device_id_formatted == b.device_id_formatted
        && a.error_message == b.error_message
        && a.signal_addr == b.signal_addr
        && a.relay_addr == b.relay_addr
}

impl SharedState {
    fn set_status(
        &self,
        mut snapshot: ServerStatusSnapshot,
        on_status: &mut dyn FnMut(ServerStatusSnapshot),
    ) {
        {
            let mut guard = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if snapshots_same_payload(&guard, &snapshot) {
                return;
            }
            snapshot.revision = next_status_revision();
            *guard = snapshot.clone();
        }
        {
            let mut q = self
                .status_updates
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if q.len() >= STATUS_QUEUE_CAP {
                q.pop_front();
            }
            q.push_back(snapshot.clone());
        }
        on_status(snapshot);
    }

    fn push_incoming(&self, item: RelayIncoming) {
        let mut q = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= INCOMING_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(item);
    }

    fn push_device_status(&self, ev: DeviceStatusEvent) {
        let mut q = self
            .device_status_events
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if q.len() >= DEVICE_STATUS_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(ev);
    }

    fn push_kicked_reason(&self, reason: String) {
        let trimmed = reason.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut q = self
            .kicked_reasons
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if q.len() >= KICKED_REASON_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(trimmed.to_string());
    }

    fn current_allow_assist_opt(&self) -> Option<bool> {
        match self.allow_remote_assist.load(Ordering::Acquire) {
            ASSIST_FALSE => Some(false),
            ASSIST_TRUE => Some(true),
            _ => None,
        }
    }

    fn fail_all_pending(&self, reason: &str) {
        let mut pending = self
            .pending_connects
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (_, waiter) in pending.drain() {
            let _ = waiter.tx.try_send(Err(reason.to_string()));
        }
    }

    /// 取出并关闭独立的 `shutdown_stream`，不触碰 `active_writer` 锁。
    fn shutdown_active_socket(&self) {
        let stream = self
            .shutdown_stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(s) = stream {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn clear_active_writer(&self) {
        self.local_udp_port.store(0, Ordering::Release);
        self.local_udp_is_ipv6.store(false, Ordering::Release);
        self.shutdown_active_socket();
        let writer = self
            .active_writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(w) = writer {
            w.shutdown_and_close();
        }
    }

    fn send_msg(&self, msg: &OutgoingSignalMsg) -> Result<(), String> {
        let mut writer_failed = false;
        let send_res = {
            let mut writer_guard = self.active_writer.lock().unwrap_or_else(|e| e.into_inner());
            match writer_guard.as_mut() {
                Some(writer) => {
                    let res = writer.write_msg(msg);
                    if res.is_err() {
                        writer_failed = true;
                        if let Some(broken) = writer_guard.take() {
                            broken.shutdown_and_close();
                        }
                    }
                    res
                }
                None => Err("信令服务器尚未连接完成".to_string()),
            }
        };
        if writer_failed {
            self.shutdown_active_socket();
        }
        send_res
    }

    fn sleep_or_stop(&self, duration: Duration) -> bool {
        let mut guard = self.stop_lock.lock().unwrap_or_else(|e| e.into_inner());
        if *guard || self.stop_flag.load(Ordering::Acquire) {
            return true;
        }
        let deadline = Instant::now() + duration;
        while !*guard && !self.stop_flag.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (next_guard, wait_res) = self
                .stop_cv
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
            if wait_res.timed_out() {
                break;
            }
        }
        *guard || self.stop_flag.load(Ordering::Acquire)
    }

    fn signal_stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        {
            let mut guard = self.stop_lock.lock().unwrap_or_else(|e| e.into_inner());
            *guard = true;
            self.stop_cv.notify_all();
        }
        // 先立即失败所有在途请求，再通过独立 shutdown_stream 切断套接字（绝不等待 active_writer 锁）。
        self.fail_all_pending("信令客户端已停止");
        self.shutdown_active_socket();
    }
}

/// 持久化信令客户端句柄（后台维护 TCP/TLS 1.3 长连接、注册、心跳、UDP 探测与会话协调）。
///
/// 在 `Drop` 时自动停止后台线程并释放套接字，不留僵尸线程。
pub struct SignalClientHandle {
    shared: Arc<SharedState>,
    worker: Option<JoinHandle<()>>,
}

impl SignalClientHandle {
    /// 启动信令客户端后台线程；若 `cfg.enabled == false` 或 `server_host` 为空则保持 `Disabled` 态且不建连。
    pub fn spawn(cfg: ServerConfig, hostname: String, lan_addrs: Vec<String>) -> Self {
        Self::spawn_with_callback(cfg, hostname, lan_addrs, |_| {})
    }

    /// 启动信令客户端后台线程，并在每次连接状态变化时同步触发 `on_status` 回调。
    pub fn spawn_with_callback<F>(
        cfg: ServerConfig,
        hostname: String,
        lan_addrs: Vec<String>,
        on_status: F,
    ) -> Self
    where
        F: FnMut(ServerStatusSnapshot) + Send + 'static,
    {
        Self::spawn_with_timing(
            cfg,
            hostname,
            lan_addrs,
            None,
            SignalTiming::default(),
            on_status,
        )
    }

    /// 启动携带鉴权扩展选项（动态 Token 提供器、设备公钥与远程协助开关）的信令客户端后台线程。
    pub fn spawn_with_options<F>(
        cfg: ServerConfig,
        hostname: String,
        lan_addrs: Vec<String>,
        options: SignalClientOptions,
        on_status: F,
    ) -> Self
    where
        F: FnMut(ServerStatusSnapshot) + Send + 'static,
    {
        Self::spawn_with_timing(
            cfg,
            hostname,
            lan_addrs,
            Some(options),
            SignalTiming::default(),
            on_status,
        )
    }

    fn spawn_with_timing<F>(
        cfg: ServerConfig,
        hostname: String,
        lan_addrs: Vec<String>,
        options: Option<SignalClientOptions>,
        timing: SignalTiming,
        mut on_status: F,
    ) -> Self
    where
        F: FnMut(ServerStatusSnapshot) + Send + 'static,
    {
        let clean_hostname = sanitize_hostname(&hostname);
        let clean_lan_addrs = sanitize_lan_addrs(&lan_addrs);
        let active = cfg.enabled && !cfg.server_host.trim().is_empty() && cfg.signal_port != 0;
        let initial_snapshot = if active {
            let device_id = cfg.cached_device_id.clone();
            let device_id_formatted = device_id.as_deref().map(format_device_id);
            ServerStatusSnapshot {
                enabled: true,
                server_host: cfg.server_host.clone(),
                signal_port: cfg.signal_port,
                relay_port: cfg.relay_port,
                state: ServerConnectionState::Connecting,
                device_id,
                device_id_formatted,
                error_message: None,
                signal_addr: cfg.signal_addr(),
                relay_addr: cfg.relay_addr(),
                revision: next_status_revision(),
            }
        } else {
            cfg.disabled_snapshot()
        };

        let (token_provider, device_pubkey, initial_assist) = match options {
            Some(opts) => {
                let pubkey = opts
                    .device_pubkey
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty());
                let assist = if opts.allow_remote_assist {
                    ASSIST_TRUE
                } else {
                    ASSIST_FALSE
                };
                (opts.token_provider, pubkey, assist)
            }
            None => (None, None, ASSIST_UNSET),
        };

        let shared = Arc::new(SharedState {
            status: Mutex::new(initial_snapshot.clone()),
            status_updates: Mutex::new(VecDeque::new()),
            incoming: Mutex::new(VecDeque::new()),
            device_status_events: Mutex::new(VecDeque::new()),
            kicked_reasons: Mutex::new(VecDeque::new()),
            pending_connects: Mutex::new(HashMap::new()),
            active_writer: Mutex::new(None),
            shutdown_stream: Mutex::new(None),
            stop_flag: AtomicBool::new(false),
            stop_lock: Mutex::new(false),
            stop_cv: Condvar::new(),
            local_udp_port: AtomicU16::new(0),
            local_udp_is_ipv6: AtomicBool::new(false),
            lan_addrs: clean_lan_addrs.clone(),
            enabled: active,
            token_provider,
            device_pubkey,
            allow_remote_assist: AtomicU8::new(initial_assist),
        });

        if !active {
            return Self {
                shared,
                worker: None,
            };
        }

        let shared_clone = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("rc-signal-client".to_string())
            .spawn(move || {
                on_status(initial_snapshot);
                worker_loop(
                    shared_clone,
                    cfg,
                    clean_hostname,
                    clean_lan_addrs,
                    timing,
                    &mut on_status,
                );
            })
            .ok();

        Self { shared, worker }
    }

    /// 向服务器发起连接目标 9 位设备码的请求，阻塞等待 `punch_coord` / `relay_ready`（或立即可返回的 `peer_offline` / `error`）。
    pub fn request_connect(
        &self,
        target_id: &str,
        timeout: Duration,
    ) -> Result<ConnectGrant, String> {
        if !self.shared.enabled {
            return Err("未启用信令服务器，请先在「设置 → 网络」中配置服务器".to_string());
        }
        let normalized = parse_device_id_input(target_id)
            .ok_or_else(|| "目标设备码格式无效，请输入 9 位数字设备码".to_string())?;

        let session_id = random_hex(16);
        let nonce = random_hex(8);
        let (tx, rx) = mpsc::sync_channel(1);

        {
            let mut pending = self
                .shared
                .pending_connects
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if self.shared.stop_flag.load(Ordering::Acquire) {
                return Err("信令客户端已停止".to_string());
            }
            pending.insert(
                session_id.clone(),
                PendingConnect {
                    target_id: normalized.clone(),
                    tx,
                },
            );
        }

        let local_udp_port = self.shared.local_udp_port.load(Ordering::Acquire);
        let local_udp_is_ipv6 = self.shared.local_udp_is_ipv6.load(Ordering::Acquire);
        let msg = OutgoingSignalMsg::ConnectRequest {
            target_id: normalized,
            session_id: session_id.clone(),
            lan_addrs: effective_lan_addrs(
                &self.shared.lan_addrs,
                local_udp_port,
                local_udp_is_ipv6,
            ),
            nonce,
        };

        if let Err(err) = self.shared.send_msg(&msg) {
            let mut pending = self
                .shared
                .pending_connects
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.remove(&session_id);
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut pending = self
                    .shared
                    .pending_connects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pending.remove(&session_id);
                Err("等待服务器下发连接凭据超时".to_string())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let mut pending = self
                    .shared
                    .pending_connects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pending.remove(&session_id);
                Err("信令连接已断开".to_string())
            }
        }
    }

    /// 实时更新本机「允许他人远程协助」状态并同步发送 `UpdateAssistState` 信令帧。
    pub fn set_allow_remote_assist(&self, allow: bool) -> Result<(), String> {
        let code = if allow { ASSIST_TRUE } else { ASSIST_FALSE };
        self.shared
            .allow_remote_assist
            .store(code, Ordering::Release);
        if !self.shared.enabled {
            return Err("未启用信令服务器".to_string());
        }
        self.shared.send_msg(&OutgoingSignalMsg::UpdateAssistState {
            allow_remote_assist: allow,
        })
    }

    /// 返回当前信令连接状态快照。
    pub fn status(&self) -> ServerStatusSnapshot {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 非阻塞拉取一条服务端推送的入站中继会话（本端作为被控端接收）。
    pub fn try_recv_incoming(&self) -> Option<RelayIncoming> {
        self.shared
            .incoming
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// 非阻塞拉取一条状态变化快照（供无回调模式轮询状态更新）。
    pub fn try_recv_status(&self) -> Option<ServerStatusSnapshot> {
        self.shared
            .status_updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// 非阻塞拉取一条同账号设备上下线/协助状态变更通知（有界队列）。
    pub fn try_recv_device_status(&self) -> Option<DeviceStatusEvent> {
        self.shared
            .device_status_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// 非阻塞拉取一条服务端强制踢线原始原因码（如 `account_disabled`、`device_unbound`、`cert_revoked` 等）。
    pub fn try_recv_kicked_reason(&self) -> Option<String> {
        self.shared
            .kicked_reasons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// 触发后台信令线程立即停止并唤醒所有在途连接请求（不阻塞等待线程 `join`）。
    ///
    /// 适用于 `Arc<SignalClientHandle>` 仍存在其他短生命周期引用时，确保旧 TCP 连接
    /// 在启动新客户端前立刻断开，避免触发服务端 `duplicate_login`。
    pub fn request_stop(&self) {
        self.shared.signal_stop();
    }

    /// 优雅停止后台信令线程并等待其退出。
    pub fn stop(&mut self) {
        self.shared.signal_stop();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SignalClientHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 清洗上报的 `hostname`：去除控制字符与 `\0`，裁剪首尾空白并按 UTF-8 字符边界截断至 `<= 128` 字节。
fn sanitize_hostname(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|&ch| !ch.is_control() && ch != '\0')
        .collect();
    let trimmed = cleaned.trim();
    let mut end = 0usize;
    for (idx, ch) in trimmed.char_indices() {
        let next = idx + ch.len_utf8();
        if next > MAX_HOSTNAME_BYTES {
            break;
        }
        end = next;
    }
    trimmed[..end].to_string()
}

/// 清洗上报的 `lan_addrs`：仅保留 `1..=64` 字节且全部为 ASCII 可见图形字符的项，最多取 16 条。
fn sanitize_lan_addrs(addrs: &[String]) -> Vec<String> {
    addrs
        .iter()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= MAX_LAN_ADDR_BYTES
                && s.bytes().all(|b| b.is_ascii_graphic())
        })
        .take(MAX_LAN_ADDRS)
        .cloned()
        .collect()
}

/// 将清洗后的 `lan_addrs` 端口改写为持久化 UDP 探测端口，并按探测套接字地址族过滤：
/// - 当 `local_udp_port == 0`（初始注册尚未绑定探测端口，或探测绑定失败）时返回空列表，
///   绝不泄漏 Host QUIC 直连端口（如 `:9901`）作为 P2P 打洞候选；
/// - 当 `local_udp_port != 0` 时，仅改写并上报与 `local_udp_is_ipv6` 同地址族的局域网地址，
///   跳过地址族不匹配的候选。
fn effective_lan_addrs(
    lan_addrs: &[String],
    local_udp_port: u16,
    local_udp_is_ipv6: bool,
) -> Vec<String> {
    if local_udp_port == 0 {
        return Vec::new();
    }
    let cleaned = sanitize_lan_addrs(lan_addrs);
    let mut out = Vec::with_capacity(cleaned.len());
    for raw in cleaned {
        let Some(ip) = extract_canonical_lan_ip(&raw, local_udp_port) else {
            continue;
        };
        if ip.is_ipv6() != local_udp_is_ipv6 {
            continue;
        }
        let rewritten = format_host_port(&ip.to_string(), local_udp_port);
        if rewritten.len() <= MAX_LAN_ADDR_BYTES && !out.contains(&rewritten) {
            out.push(rewritten);
        }
    }
    out
}

fn extract_canonical_lan_ip(raw: &str, fallback_port: u16) -> Option<IpAddr> {
    if let Ok(sock) = raw.parse::<SocketAddr>() {
        return Some(canonicalize_ip(sock.ip()));
    }
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Some(canonicalize_ip(ip));
    }
    if let Ok((host, _)) = parse_server_input(raw, fallback_port) {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Some(canonicalize_ip(ip));
        }
    }
    None
}

/// 构造 22 字节 UDP `RCSP` 公网端点探测包：
/// `[b"RCSP"][0x01][device_id 9B ASCII][nonce 8B]`。
///
/// 接受匿名探测码 `"000000000"` 或首位为 `'1'..='9'` 的 9 位永久设备码（对齐 `server/docs/02-protocol-and-api.md` §3.1）。
pub fn build_udp_probe_packet(
    device_id: &str,
    nonce: [u8; 8],
) -> Result<[u8; UDP_PROBE_LEN], String> {
    let id_bytes = device_id.as_bytes();
    let valid = id_bytes.len() == 9
        && id_bytes.iter().all(u8::is_ascii_digit)
        && (id_bytes == b"000000000" || matches!(id_bytes[0], b'1'..=b'9'));
    if !valid {
        return Err("UDP 探测包要求 9 位数字设备码".to_string());
    }
    let mut pkt = [0u8; UDP_PROBE_LEN];
    pkt[0..4].copy_from_slice(UDP_PROBE_MAGIC);
    pkt[4] = UDP_PROTO_VERSION;
    pkt[5..14].copy_from_slice(id_bytes);
    pkt[14..22].copy_from_slice(&nonce);
    Ok(pkt)
}

/// 测试信令端口与中继端口的 TCP 连通性，并返回含实测 RTT（毫秒）的可读结果文案。
pub fn probe_server_connectivity(
    host: &str,
    signal_port: u16,
    relay_port: u16,
    timeout: Duration,
) -> Result<String, String> {
    let (clean_host, effective_signal_port) = parse_server_input(host, signal_port)?;
    if effective_signal_port == 0 || relay_port == 0 {
        return Err("信令端口与中继端口必须为 1–65535".to_string());
    }

    let signal_addr = format_host_port(&clean_host, effective_signal_port);
    let relay_addr = format_host_port(&clean_host, relay_port);

    let signal_ms = tcp_probe_once(&signal_addr, timeout)
        .map_err(|e| format!("信令端口 {signal_addr} 不可达：{e}"))?;
    let relay_ms = tcp_probe_once(&relay_addr, timeout).map_err(|e| {
        format!("信令端口可达 ({signal_ms}ms)，但中继端口 {relay_addr} 不可达：{e}")
    })?;

    Ok(format!(
        "连接正常 · 信令 {signal_addr} ({signal_ms}ms) / 中继 {relay_addr} ({relay_ms}ms)"
    ))
}

fn per_addr_timeout(remaining: Duration, remaining_addrs: usize) -> Duration {
    let count = remaining_addrs.max(1) as u32;
    (remaining / count).max(MIN_ADDR_TIMEOUT).min(remaining)
}

fn tcp_probe_once(addr_str: &str, timeout: Duration) -> Result<u128, String> {
    let addrs: Vec<SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("地址解析失败：{e}"))?
        .collect();
    if addrs.is_empty() {
        return Err("未解析到任何可用 IP 地址".to_string());
    }
    let start = Instant::now();
    let deadline = start + timeout;
    let mut last_err: Option<io::Error> = None;
    for (idx, addr) in addrs.iter().enumerate() {
        let now = Instant::now();
        if now >= deadline {
            return Err("连接超时".to_string());
        }
        let budget = per_addr_timeout(deadline - now, addrs.len() - idx);
        match TcpStream::connect_timeout(addr, budget) {
            Ok(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
                return Ok(start.elapsed().as_millis());
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "连接失败".to_string()))
}

fn should_reset_backoff(
    healthy_duration: Duration,
    kicked: bool,
    stable_threshold: Duration,
) -> bool {
    !kicked && healthy_duration >= stable_threshold
}

/// 将服务端下发的踢线/拒绝原因枚举映射为 `(是否终止重连, 本地化中文说明)`。
pub fn classify_kicked_reason(reason: &str) -> (bool, String) {
    let trimmed = reason.trim();
    if trimmed.eq_ignore_ascii_case("replaced_by_new_connection") {
        (
            true,
            "当前设备已在其他位置登录，信令连接已被替换，已停止自动重连".to_string(),
        )
    } else if trimmed.eq_ignore_ascii_case("disabled_by_admin") {
        (true, "当前设备已被管理员禁用，已停止自动重连".to_string())
    } else if trimmed.eq_ignore_ascii_case("account_disabled") {
        (true, "当前账号已被禁用，已停止自动重连".to_string())
    } else if trimmed.eq_ignore_ascii_case("account_deleted")
        || trimmed.eq_ignore_ascii_case("user_deleted")
    {
        (true, "当前账号已被删除，已停止自动重连".to_string())
    } else if trimmed.eq_ignore_ascii_case("password_changed")
        || trimmed.eq_ignore_ascii_case("account_updated")
        || trimmed.eq_ignore_ascii_case("token_revoked")
    {
        (true, "账号凭证或安全状态已变更，请重新登录".to_string())
    } else if trimmed.eq_ignore_ascii_case("device_unbound") {
        (true, "当前设备已被解绑，请重新登录绑定".to_string())
    } else if trimmed.eq_ignore_ascii_case("cert_revoked") {
        (true, "当前设备证书已被吊销，请重新登录".to_string())
    } else if trimmed.eq_ignore_ascii_case("user_logout") {
        (true, "当前会话已退出登录，已停止自动重连".to_string())
    } else if trimmed.is_empty() {
        (false, "连接已被服务器断开".to_string())
    } else {
        (false, format!("被服务器断开：{trimmed}"))
    }
}

/// 若信令踢线/拒绝原因为账号失效或凭证吊销（需强制退登重登），返回对应的中文提示。
///
/// 排除 `server_shutdown`（服务器维护关闭）与 `replaced_by_new_connection`（同设备连接替换）。
pub fn classify_auth_revocation_kick(reason: &str) -> Option<String> {
    let trimmed = reason.trim();
    if trimmed.eq_ignore_ascii_case("server_shutdown")
        || trimmed.eq_ignore_ascii_case("replaced_by_new_connection")
    {
        return None;
    }
    let (is_terminal, msg) = classify_kicked_reason(trimmed);
    if is_terminal {
        Some(msg)
    } else {
        None
    }
}

fn worker_loop(
    shared: Arc<SharedState>,
    cfg: ServerConfig,
    hostname: String,
    lan_addrs: Vec<String>,
    timing: SignalTiming,
    on_status: &mut dyn FnMut(ServerStatusSnapshot),
) {
    let mut backoff_idx = 0usize;
    let mut cached_device_id = cfg.cached_device_id.clone();
    let default_signal_addr = format_host_port(&cfg.server_host, cfg.signal_port);
    let default_relay_addr = format_host_port(&cfg.server_host, cfg.relay_port);

    while !shared.stop_flag.load(Ordering::Acquire) {
        shared.set_status(
            build_snapshot(
                &cfg,
                ServerConnectionState::Connecting,
                cached_device_id.clone(),
                None,
                Some(default_signal_addr.clone()),
                Some(default_relay_addr.clone()),
            ),
            on_status,
        );

        match run_single_connection(
            &shared,
            &cfg,
            &hostname,
            &lan_addrs,
            &mut cached_device_id,
            &default_signal_addr,
            &default_relay_addr,
            timing,
            on_status,
        ) {
            ConnectionOutcome::Stopped => break,
            ConnectionOutcome::TerminalError(reason) => {
                shared.clear_active_writer();
                shared.fail_all_pending(&reason);
                if !shared.stop_flag.load(Ordering::Acquire) {
                    shared.set_status(
                        build_snapshot(
                            &cfg,
                            ServerConnectionState::Error,
                            cached_device_id.clone(),
                            Some(reason),
                            Some(default_signal_addr.clone()),
                            Some(default_relay_addr.clone()),
                        ),
                        on_status,
                    );
                }
                break;
            }
            ConnectionOutcome::ConnectedThenDropped {
                reason,
                healthy_duration,
                kicked,
            } => {
                if should_reset_backoff(healthy_duration, kicked, timing.stable_threshold) {
                    backoff_idx = 0;
                }
                shared.clear_active_writer();
                shared.fail_all_pending(&reason);
                if shared.stop_flag.load(Ordering::Acquire) {
                    break;
                }
                shared.set_status(
                    build_snapshot(
                        &cfg,
                        ServerConnectionState::Error,
                        cached_device_id.clone(),
                        Some(reason),
                        Some(default_signal_addr.clone()),
                        Some(default_relay_addr.clone()),
                    ),
                    on_status,
                );
            }
            ConnectionOutcome::ConnectFailed(reason) => {
                shared.clear_active_writer();
                shared.fail_all_pending(&reason);
                if shared.stop_flag.load(Ordering::Acquire) {
                    break;
                }
                shared.set_status(
                    build_snapshot(
                        &cfg,
                        ServerConnectionState::Error,
                        cached_device_id.clone(),
                        Some(reason),
                        Some(default_signal_addr.clone()),
                        Some(default_relay_addr.clone()),
                    ),
                    on_status,
                );
            }
        }

        let wait = timing.backoffs[backoff_idx];
        backoff_idx = (backoff_idx + 1).min(timing.backoffs.len() - 1);
        if shared.sleep_or_stop(wait) {
            break;
        }
    }

    shared.clear_active_writer();
    shared.fail_all_pending("信令客户端已停止");
}

enum ConnectionOutcome {
    Stopped,
    ConnectFailed(String),
    ConnectedThenDropped {
        reason: String,
        healthy_duration: Duration,
        kicked: bool,
    },
    TerminalError(String),
}

enum IncomingAction {
    Continue,
    Disconnect { reason: String, kicked: bool },
    Terminal(String),
}

fn build_signal_tls_config(ca_cert_pem: &str) -> Result<ClientConfig, String> {
    let mut reader = io::BufReader::new(ca_cert_pem.as_bytes());
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| format!("解析信令 Root CA PEM 失败：{e}"))?;
    if certs.is_empty() {
        return Err("信令 Root CA PEM 不包含任何证书".to_string());
    }
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| format!("添加信令 Root CA 证书失败：{e}"))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("配置信令 TLS 1.3 版本失败：{e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth()
        .pipe_ok()
}

trait SignalPipeOk: Sized {
    fn pipe_ok<E>(self) -> Result<Self, E> {
        Ok(self)
    }
}
impl SignalPipeOk for ClientConfig {}

fn parse_signal_server_name(host: &str) -> Result<ServerName<'static>, String> {
    let clean = host
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host.trim());
    ServerName::try_from(clean.to_string())
        .map_err(|e| format!("无效信令 TLS 主机名 {host:?}：{e}"))
}

enum TlsHandshakeErr {
    Stopped,
    Failed(String),
}

fn complete_tls_handshake(
    conn: &mut ClientConnection,
    tcp: &mut TcpStream,
    deadline: Instant,
    stop_flag: &AtomicBool,
) -> Result<(), TlsHandshakeErr> {
    while conn.is_handshaking() {
        if stop_flag.load(Ordering::Acquire) {
            return Err(TlsHandshakeErr::Stopped);
        }
        if Instant::now() >= deadline {
            return Err(TlsHandshakeErr::Failed("信令 TLS 1.3 握手超时".to_string()));
        }
        match conn.complete_io(tcp) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(e) => {
                return Err(TlsHandshakeErr::Failed(format!(
                    "信令 TLS 1.3 握手失败：{e}"
                )));
            }
        }
    }
    Ok(())
}

enum SignalReader {
    Plain(TcpStream),
    Tls(SharedTlsStream),
}

impl SignalReader {
    fn read_frame(&mut self, stop_flag: &AtomicBool) -> FrameRead {
        match self {
            Self::Plain(stream) => read_signal_frame_from_io(stream, stop_flag),
            Self::Tls(shared_tls) => {
                let mut guard = shared_tls.lock().unwrap_or_else(|e| e.into_inner());
                let Some(tls) = guard.as_mut() else {
                    return FrameRead::Eof;
                };
                read_signal_frame_from_io(&mut **tls, stop_flag)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_single_connection(
    shared: &SharedState,
    cfg: &ServerConfig,
    hostname: &str,
    lan_addrs: &[String],
    cached_device_id: &mut Option<String>,
    default_signal_addr: &str,
    default_relay_addr: &str,
    timing: SignalTiming,
    on_status: &mut dyn FnMut(ServerStatusSnapshot),
) -> ConnectionOutcome {
    let stream = match dial_tcp(default_signal_addr, HANDSHAKE_TIMEOUT, &shared.stop_flag) {
        Ok(s) => s,
        Err(DialError::Stopped) => return ConnectionOutcome::Stopped,
        Err(DialError::Failed(err)) => return ConnectionOutcome::ConnectFailed(err),
    };
    if shared.stop_flag.load(Ordering::Acquire) {
        let _ = stream.shutdown(Shutdown::Both);
        return ConnectionOutcome::Stopped;
    }

    let _ = stream.set_nodelay(true);
    configure_tcp_keepalive(&stream);
    let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_read_timeout(Some(READ_POLL_TIMEOUT));
    let peer_addr = stream.peer_addr().ok();

    // 在握手前即注册独立 shutdown_stream，确保 stop() 可瞬间打断握手读写。
    let shutdown_clone = match stream.try_clone() {
        Ok(s) => s,
        Err(err) => {
            return ConnectionOutcome::ConnectFailed(format!("克隆信令关闭句柄失败：{err}"));
        }
    };
    {
        let mut shutdown_guard = shared
            .shutdown_stream
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if shared.stop_flag.load(Ordering::Acquire) {
            drop(shutdown_guard);
            let _ = stream.shutdown(Shutdown::Both);
            return ConnectionOutcome::Stopped;
        }
        *shutdown_guard = Some(shutdown_clone);
    }

    let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    if cfg.use_https && cfg.ca_cert_pem.is_none() {
        let _ = stream.shutdown(Shutdown::Both);
        return ConnectionOutcome::ConnectFailed(
            "已开启 TLS/HTTPS 安全传输，但缺少服务器 Root CA 证书，拒绝降级明文信令连接"
                .to_string(),
        );
    }
    let use_tls = cfg.use_https && cfg.ca_cert_pem.is_some();
    let (mut reader, writer_template) = if use_tls {
        let ca_pem = cfg.ca_cert_pem.as_deref().expect("checked is_some");
        let tls_cfg = match build_signal_tls_config(ca_pem) {
            Ok(c) => c,
            Err(err) => return ConnectionOutcome::ConnectFailed(err),
        };
        let server_name = match parse_signal_server_name(&cfg.server_host) {
            Ok(s) => s,
            Err(err) => return ConnectionOutcome::ConnectFailed(err),
        };
        let mut conn = match ClientConnection::new(Arc::new(tls_cfg), server_name) {
            Ok(c) => c,
            Err(err) => {
                return ConnectionOutcome::ConnectFailed(format!("初始化信令 TLS 会话失败：{err}"));
            }
        };
        let mut tcp_for_tls = stream;
        if let Err(err) = complete_tls_handshake(
            &mut conn,
            &mut tcp_for_tls,
            handshake_deadline,
            &shared.stop_flag,
        ) {
            let _ = tcp_for_tls.shutdown(Shutdown::Both);
            return match err {
                TlsHandshakeErr::Stopped => ConnectionOutcome::Stopped,
                TlsHandshakeErr::Failed(msg) => ConnectionOutcome::ConnectFailed(msg),
            };
        }
        let shared_tls = Arc::new(Mutex::new(Some(Box::new(StreamOwned::new(
            conn,
            tcp_for_tls,
        )))));
        (
            SignalReader::Tls(Arc::clone(&shared_tls)),
            SignalWriter::Tls(shared_tls),
        )
    } else {
        let writer_clone = match stream.try_clone() {
            Ok(s) => s,
            Err(err) => {
                return ConnectionOutcome::ConnectFailed(format!("克隆信令套接字失败：{err}"));
            }
        };
        (
            SignalReader::Plain(stream),
            SignalWriter::Plain(writer_clone),
        )
    };

    let access_token = match &shared.token_provider {
        Some(provider) => match provider() {
            Ok(token) => Some(token),
            Err(err) => {
                writer_template.shutdown_and_close();
                return ConnectionOutcome::ConnectFailed(format!("获取信令认证令牌失败：{err}"));
            }
        },
        None => None,
    };

    let clean_hostname = sanitize_hostname(hostname);
    let clean_lan_addrs = sanitize_lan_addrs(lan_addrs);
    // 初始注册时尚未绑定持久化 UDP 探测端口（local_udp_port == 0），必须上报空列表，
    // 绝不把未改写的 `:9901` 泄漏为 P2P 打洞目标；待 `Registered` 后由紧随的 Heartbeat 同步。
    let reg_msg = OutgoingSignalMsg::Register {
        device_id: cached_device_id.clone(),
        device_key: cfg.device_key.clone(),
        hostname: clean_hostname,
        os: std::env::consts::OS.to_string(),
        device_group: "default".to_string(),
        lan_addrs: Vec::new(),
        access_token,
        device_pubkey: shared.device_pubkey.clone(),
        allow_remote_assist: shared.current_allow_assist_opt(),
    };
    let send_reg_res = match &reader {
        SignalReader::Plain(_) => match &writer_template {
            SignalWriter::Plain(w) => match w.try_clone() {
                Ok(mut w_clone) => write_signal_frame_plain(&mut w_clone, &reg_msg),
                Err(e) => Err(format!("克隆信令写流失败：{e}")),
            },
            SignalWriter::Tls(_) => unreachable!(),
        },
        SignalReader::Tls(shared_tls) => {
            let mut guard = shared_tls.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut() {
                Some(tls) => write_signal_frame_tls(tls, &reg_msg),
                None => Err("信令 TLS 连接已关闭".to_string()),
            }
        }
    };
    if let Err(err) = send_reg_res {
        if shared.stop_flag.load(Ordering::Acquire) {
            return ConnectionOutcome::Stopped;
        }
        return ConnectionOutcome::ConnectFailed(format!("发送注册请求失败：{err}"));
    }

    // 等待服务端首帧返回 `registered`（或 `error`）。
    let (assigned_id, formatted_id, effective_relay_addr, effective_udp_probe_addr) = loop {
        if shared.stop_flag.load(Ordering::Acquire) {
            return ConnectionOutcome::Stopped;
        }
        if Instant::now() >= handshake_deadline {
            return ConnectionOutcome::ConnectFailed("等待服务器注册响应超时".to_string());
        }
        match reader.read_frame(&shared.stop_flag) {
            FrameRead::IdleTick => continue,
            FrameRead::Stopped => return ConnectionOutcome::Stopped,
            FrameRead::Eof => {
                return ConnectionOutcome::ConnectFailed("服务器在注册阶段关闭了连接".to_string());
            }
            FrameRead::Error(err) => {
                if shared.stop_flag.load(Ordering::Acquire) {
                    return ConnectionOutcome::Stopped;
                }
                return ConnectionOutcome::ConnectFailed(format!("读取注册响应失败：{err}"));
            }
            FrameRead::Message(msg) => match *msg {
                IncomingSignalMsg::Registered {
                    device_id,
                    formatted_id,
                    udp_probe_addr,
                    relay_addr,
                } => {
                    let fmt_id = formatted_id
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| format_device_id(&device_id));
                    let eff_relay = resolve_advertised_addr(
                        relay_addr.as_deref().unwrap_or(""),
                        &cfg.server_host,
                        cfg.relay_port,
                    );
                    let eff_udp = resolve_advertised_addr(
                        udp_probe_addr.as_deref().unwrap_or(""),
                        &cfg.server_host,
                        cfg.signal_port,
                    );
                    break (device_id, fmt_id, eff_relay, eff_udp);
                }
                IncomingSignalMsg::Error { code, message } => {
                    if code == "device_id_mismatch" {
                        // 本地缓存的设备码与服务端记录不一致，清空缓存以便下轮重新签发。
                        *cached_device_id = None;
                    }
                    if matches!(
                        code.as_str(),
                        "account_disabled" | "user_deleted" | "token_revoked"
                    ) {
                        let (_, zh_msg) = classify_kicked_reason(&code);
                        shared.push_kicked_reason(code);
                        return ConnectionOutcome::TerminalError(zh_msg);
                    }
                    return ConnectionOutcome::ConnectFailed(format!(
                        "注册被拒绝 ({code})：{message}"
                    ));
                }
                _ => {
                    return ConnectionOutcome::ConnectFailed(
                        "注册阶段收到非预期信令消息".to_string(),
                    );
                }
            },
        }
    };

    *cached_device_id = Some(assigned_id.clone());

    {
        let mut writer_guard = shared
            .active_writer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if shared.stop_flag.load(Ordering::Acquire) {
            drop(writer_guard);
            shared.clear_active_writer();
            return ConnectionOutcome::Stopped;
        }
        *writer_guard = Some(writer_template);
    }

    if shared.stop_flag.load(Ordering::Acquire) {
        return ConnectionOutcome::Stopped;
    }

    let connected_at = Instant::now();
    let mut last_server_activity = connected_at;
    // 在 Registered 时一次性解析 UDP 探测目标（优先匹配 TCP 对端地址族），避免每 30s 阻塞 DNS。
    let udp_probe_target = resolve_udp_probe_target(&effective_udp_probe_addr, peer_addr);

    // 注册成功后绑定持久化 UDP 探测套接字（SO_REUSEADDR + SO_REUSEPORT 且已 connect 至信令端口），
    // 记录本地端口并立即发送一次 `RCSP` 探测包与携带改写后 `lan_addrs` 的 `Heartbeat`，
    // 确保在对外宣告 `Connected` 前，服务端与本地均已同步正确的 P2P 本地端口。
    let udp_probe = udp_probe_target.and_then(PersistentUdpProbe::bind);
    let bound_udp_port = udp_probe.as_ref().map_or(0, PersistentUdpProbe::local_port);
    let bound_udp_is_ipv6 = udp_probe.as_ref().is_some_and(PersistentUdpProbe::is_ipv6);
    shared
        .local_udp_is_ipv6
        .store(bound_udp_is_ipv6, Ordering::Release);
    shared
        .local_udp_port
        .store(bound_udp_port, Ordering::Release);
    if let Some(ref probe) = udp_probe {
        probe.send_probe(&assigned_id);
        if let Err(err) = send_heartbeat_frame(
            shared,
            effective_lan_addrs(&clean_lan_addrs, bound_udp_port, bound_udp_is_ipv6),
        ) {
            shared.shutdown_active_socket();
            if shared.stop_flag.load(Ordering::Acquire) {
                return ConnectionOutcome::Stopped;
            }
            return ConnectionOutcome::ConnectedThenDropped {
                reason: format!("发送初始心跳失败：{err}"),
                healthy_duration: connected_at.elapsed(),
                kicked: false,
            };
        }
    }

    shared.set_status(
        ServerStatusSnapshot {
            enabled: cfg.enabled,
            server_host: cfg.server_host.clone(),
            signal_port: cfg.signal_port,
            relay_port: cfg.relay_port,
            state: ServerConnectionState::Connected,
            device_id: Some(assigned_id.clone()),
            device_id_formatted: Some(formatted_id),
            error_message: None,
            signal_addr: Some(default_signal_addr.to_string()),
            relay_addr: Some(effective_relay_addr.clone()),
            revision: 0,
        },
        on_status,
    );

    if shared.stop_flag.load(Ordering::Acquire) {
        return ConnectionOutcome::Stopped;
    }

    let mut last_heartbeat = Instant::now();

    loop {
        if shared.stop_flag.load(Ordering::Acquire) {
            return ConnectionOutcome::Stopped;
        }

        if last_server_activity.elapsed() >= timing.activity_timeout {
            return ConnectionOutcome::ConnectedThenDropped {
                reason: "信令服务器心跳响应超时，正在重连".to_string(),
                healthy_duration: connected_at.elapsed(),
                kicked: false,
            };
        }

        if last_heartbeat.elapsed() >= timing.heartbeat_interval {
            let current_udp_port = shared.local_udp_port.load(Ordering::Acquire);
            let current_udp_is_ipv6 = shared.local_udp_is_ipv6.load(Ordering::Acquire);
            if let Err(err) = send_heartbeat_frame(
                shared,
                effective_lan_addrs(&clean_lan_addrs, current_udp_port, current_udp_is_ipv6),
            ) {
                shared.shutdown_active_socket();
                if shared.stop_flag.load(Ordering::Acquire) {
                    return ConnectionOutcome::Stopped;
                }
                return ConnectionOutcome::ConnectedThenDropped {
                    reason: format!("发送心跳失败：{err}"),
                    healthy_duration: connected_at.elapsed(),
                    kicked: false,
                };
            }
            if let Some(ref probe) = udp_probe {
                probe.send_probe(&assigned_id);
            }
            last_heartbeat = Instant::now();
        }

        match reader.read_frame(&shared.stop_flag) {
            FrameRead::IdleTick => continue,
            FrameRead::Stopped => return ConnectionOutcome::Stopped,
            FrameRead::Eof => {
                return ConnectionOutcome::ConnectedThenDropped {
                    reason: "信令服务器断开连接".to_string(),
                    healthy_duration: connected_at.elapsed(),
                    kicked: false,
                };
            }
            FrameRead::Error(err) => {
                if shared.stop_flag.load(Ordering::Acquire) {
                    return ConnectionOutcome::Stopped;
                }
                return ConnectionOutcome::ConnectedThenDropped {
                    reason: format!("读取信令帧失败：{err}"),
                    healthy_duration: connected_at.elapsed(),
                    kicked: false,
                };
            }
            FrameRead::Message(msg) => {
                last_server_activity = Instant::now();
                match handle_incoming_message(
                    shared,
                    *msg,
                    &cfg.server_host,
                    cfg.relay_port,
                    &effective_relay_addr,
                    default_relay_addr,
                ) {
                    IncomingAction::Continue => {}
                    IncomingAction::Disconnect { reason, kicked } => {
                        return ConnectionOutcome::ConnectedThenDropped {
                            reason,
                            healthy_duration: connected_at.elapsed(),
                            kicked,
                        };
                    }
                    IncomingAction::Terminal(reason) => {
                        return ConnectionOutcome::TerminalError(reason);
                    }
                }
            }
        }
    }
}

fn handle_incoming_message(
    shared: &SharedState,
    msg: IncomingSignalMsg,
    server_host: &str,
    relay_port: u16,
    registered_relay_addr: &str,
    default_relay_addr: &str,
) -> IncomingAction {
    match msg {
        IncomingSignalMsg::Registered { .. } | IncomingSignalMsg::HeartbeatAck { .. } => {
            IncomingAction::Continue
        }
        IncomingSignalMsg::DeviceStatusChanged {
            device_id,
            online,
            hostname,
            alias,
            os,
            allow_remote_assist,
        } => {
            shared.push_device_status(DeviceStatusEvent {
                device_id,
                online,
                hostname,
                alias,
                os,
                allow_remote_assist,
            });
            IncomingAction::Continue
        }
        IncomingSignalMsg::PunchCoord {
            session_id,
            role,
            peer_id,
            peer_public_udp_addr,
            peer_lan_addrs,
            relay_token,
            relay_addr,
            owner_grant,
            is_owner_connection,
        } => {
            let eff_relay = pick_relay_addr(
                &relay_addr,
                registered_relay_addr,
                server_host,
                relay_port,
                default_relay_addr,
            );
            dispatch_coord(
                shared,
                CoordPayload {
                    session_id,
                    role,
                    peer_id,
                    relay_addr: eff_relay,
                    relay_token,
                    peer_public_addr: peer_public_udp_addr,
                    peer_lan_addrs,
                    owner_grant,
                    is_owner_connection,
                },
            );
            IncomingAction::Continue
        }
        IncomingSignalMsg::RelayReady {
            session_id,
            role,
            peer_id,
            relay_token,
            relay_addr,
            owner_grant,
            is_owner_connection,
        } => {
            let eff_relay = pick_relay_addr(
                &relay_addr,
                registered_relay_addr,
                server_host,
                relay_port,
                default_relay_addr,
            );
            dispatch_coord(
                shared,
                CoordPayload {
                    session_id,
                    role,
                    peer_id,
                    relay_addr: eff_relay,
                    relay_token,
                    peer_public_addr: None,
                    peer_lan_addrs: Vec::new(),
                    owner_grant,
                    is_owner_connection,
                },
            );
            IncomingAction::Continue
        }
        IncomingSignalMsg::PeerOffline {
            target_id,
            session_id,
        } => {
            let err_msg = format!("目标设备 {} 当前不在线", format_device_id(&target_id));
            let mut pending = shared
                .pending_connects
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !session_id.is_empty() {
                if let Some(waiter) = pending.remove(&session_id) {
                    let _ = waiter.tx.try_send(Err(err_msg));
                }
            } else if let Some(key) = pending
                .iter()
                .find(|(_, v)| v.target_id == target_id)
                .map(|(k, _)| k.clone())
            {
                if let Some(waiter) = pending.remove(&key) {
                    let _ = waiter.tx.try_send(Err(err_msg));
                }
            }
            IncomingAction::Continue
        }
        IncomingSignalMsg::Error { code, message } => {
            let detail = if message.is_empty() {
                format!("服务器返回错误 ({code})")
            } else {
                format!("服务器拒绝请求 ({code})：{message}")
            };
            shared.fail_all_pending(&detail);
            IncomingAction::Continue
        }
        IncomingSignalMsg::Kicked { reason } => {
            shared.push_kicked_reason(reason.clone());
            let (is_terminal, msg) = classify_kicked_reason(&reason);
            if is_terminal {
                IncomingAction::Terminal(msg)
            } else {
                IncomingAction::Disconnect {
                    reason: msg,
                    kicked: true,
                }
            }
        }
    }
}

struct CoordPayload {
    session_id: String,
    role: String,
    peer_id: String,
    relay_addr: String,
    relay_token: String,
    peer_public_addr: Option<String>,
    peer_lan_addrs: Vec<String>,
    owner_grant: Option<OwnerGrantPayload>,
    is_owner_connection: bool,
}

fn normalize_optional_addr(raw: Option<String>) -> Option<String> {
    raw.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn dispatch_coord(shared: &SharedState, coord: CoordPayload) {
    let CoordPayload {
        session_id,
        role,
        peer_id,
        relay_addr,
        relay_token,
        peer_public_addr,
        peer_lan_addrs,
        owner_grant,
        is_owner_connection,
    } = coord;
    let peer_public_addr = normalize_optional_addr(peer_public_addr);
    let local_port = shared.local_udp_port.load(Ordering::Acquire);
    let local_udp_port = (local_port != 0).then_some(local_port);

    let is_host_role = role.eq_ignore_ascii_case("host");
    if is_host_role {
        shared.push_incoming(RelayIncoming {
            session_id,
            relay_addr,
            relay_token,
            peer_id,
            peer_public_addr,
            peer_lan_addrs,
            local_udp_port,
            owner_grant,
            is_owner_connection,
        });
        return;
    }

    if !role.is_empty() && !role.eq_ignore_ascii_case("client") {
        // 未知角色直接丢弃，绝不误入 incoming 队列或污染出站连接。
        return;
    }

    let mut pending = shared
        .pending_connects
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let matched = if !session_id.is_empty() {
        pending.remove(&session_id)
    } else if role.eq_ignore_ascii_case("client") {
        let key = pending
            .iter()
            .find(|(_, v)| v.target_id == peer_id)
            .map(|(k, _)| k.clone());
        key.and_then(|k| pending.remove(&k))
    } else {
        None
    };
    if let Some(waiter) = matched {
        let _ = waiter.tx.try_send(Ok(ConnectGrant {
            session_id,
            relay_addr,
            relay_token,
            target_id: peer_id,
            peer_public_addr,
            peer_lan_addrs,
            local_udp_port,
            owner_grant,
            is_owner_connection,
        }));
    }
    // 未匹配的 client / 空角色协调消息干净丢弃，绝不推入 incoming。
}

fn pick_relay_addr(
    msg_relay_addr: &str,
    registered_relay_addr: &str,
    server_host: &str,
    relay_port: u16,
    default_relay_addr: &str,
) -> String {
    if !msg_relay_addr.trim().is_empty() {
        return resolve_advertised_addr(msg_relay_addr, server_host, relay_port);
    }
    if !registered_relay_addr.trim().is_empty() {
        return registered_relay_addr.to_string();
    }
    default_relay_addr.to_string()
}

fn canonicalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    trimmed
        .parse::<IpAddr>()
        .map(canonicalize_ip)
        .is_ok_and(|ip| ip.is_loopback())
}

fn is_unspecified_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed
        .parse::<IpAddr>()
        .map(canonicalize_ip)
        .is_ok_and(|ip| ip.is_unspecified())
}

fn resolve_advertised_addr(advertised: &str, fallback_host: &str, fallback_port: u16) -> String {
    let trimmed = advertised.trim();
    if trimmed.is_empty() {
        return format_host_port(fallback_host, fallback_port);
    }
    let fallback_is_loopback = is_loopback_host(fallback_host);

    if let Ok(sock) = trimmed.parse::<SocketAddr>() {
        let ip = canonicalize_ip(sock.ip());
        let port = if sock.port() == 0 {
            fallback_port
        } else {
            sock.port()
        };
        if ip.is_unspecified() || (ip.is_loopback() && !fallback_is_loopback) {
            return format_host_port(fallback_host, port);
        }
        return format_host_port(&ip.to_string(), port);
    }

    // 非 SocketAddr 字符串（如 bare hostname/IP："localhost"、"0.0.0.0"、"::1"、"relay.example.com" 或带 `:0`）：
    // 统一通过 `parse_server_input` 规范化并执行同样的回环/未指定地址替换与非零端口保留。
    let parsed = parse_server_input(trimmed, fallback_port).or_else(|_| {
        trimmed
            .strip_suffix(":0")
            .ok_or_else(|| "invalid".to_string())
            .and_then(|without_zero| parse_server_input(without_zero, fallback_port))
    });

    if let Ok((host, port)) = parsed {
        let effective_port = if port == 0 { fallback_port } else { port };
        if is_unspecified_host(&host) || (is_loopback_host(&host) && !fallback_is_loopback) {
            return format_host_port(fallback_host, effective_port);
        }
        let normalized_host = host
            .parse::<IpAddr>()
            .map(|ip| canonicalize_ip(ip).to_string())
            .unwrap_or(host);
        return format_host_port(&normalized_host, effective_port);
    }

    format_host_port(fallback_host, fallback_port)
}

fn resolve_udp_probe_target(
    udp_probe_addr: &str,
    peer_addr: Option<SocketAddr>,
) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = udp_probe_addr.to_socket_addrs().ok()?.collect();
    if addrs.is_empty() {
        return None;
    }
    if let Some(peer) = peer_addr {
        let prefer_v6 = peer.is_ipv6();
        if let Some(&matching) = addrs.iter().find(|a| a.is_ipv6() == prefer_v6) {
            return Some(matching);
        }
    }
    addrs.into_iter().next()
}

fn send_heartbeat_frame(shared: &SharedState, lan_addrs: Vec<String>) -> Result<(), String> {
    let hb = OutgoingSignalMsg::Heartbeat { lan_addrs };
    shared.send_msg(&hb)
}

/// 持久化 UDP `RCSP` 探测套接字：以 `SO_REUSEADDR` + `SO_REUSEPORT` 绑定一次并 `connect` 至信令端口，
/// 使信令服务器学习到的公网 NAT `(ip, port)` 与 P2P 打洞套接字可共享同一本地端口。
///
/// 若 `connect(target)` 失败则立即放弃套接字并返回 `None`，确保绝不保留未连接的
/// `SO_REUSEPORT` 套接字抢走同端口的 P2P/QUIC 数据报。
struct PersistentUdpProbe {
    sock: UdpSocket,
    port: u16,
    is_ipv6: bool,
}

impl PersistentUdpProbe {
    fn bind(target: SocketAddr) -> Option<Self> {
        Self::bind_with(target, |sock, addr| sock.connect(addr))
    }

    fn bind_with<F>(target: SocketAddr, connect_fn: F) -> Option<Self>
    where
        F: FnOnce(&UdpSocket, SocketAddr) -> io::Result<()>,
    {
        if target.port() == 0 || target.ip().is_unspecified() {
            return None;
        }
        let is_ipv6 = target.is_ipv6();
        let (domain, bind_addr) = if is_ipv6 {
            (Domain::IPV6, SocketAddr::from(([0u16; 8], 0)))
        } else {
            (Domain::IPV4, SocketAddr::from(([0u8; 4], 0)))
        };
        let raw = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).ok()?;
        let _ = raw.set_reuse_address(true);
        #[cfg(unix)]
        let _ = raw.set_reuse_port(true);
        raw.bind(&SockAddr::from(bind_addr)).ok()?;
        let sock: UdpSocket = raw.into();
        let port = sock.local_addr().ok()?.port();
        // 必须成功 connect 到信令服务器的 UDP 探测端点；失败则直接丢弃套接字，
        // 不暴露 `local_udp_port`，防止未 connect 的 SO_REUSEPORT 套接字窃取 P2P/QUIC 包。
        if connect_fn(&sock, target).is_err() {
            return None;
        }
        Some(Self {
            sock,
            port,
            is_ipv6,
        })
    }

    fn local_port(&self) -> u16 {
        self.port
    }

    fn is_ipv6(&self) -> bool {
        self.is_ipv6
    }

    fn send_probe(&self, device_id: &str) {
        let mut nonce = [0u8; 8];
        OsRng.fill_bytes(&mut nonce);
        let Ok(pkt) = build_udp_probe_packet(device_id, nonce) else {
            return;
        };
        let _ = self.sock.send(&pkt);
    }
}

fn build_snapshot(
    cfg: &ServerConfig,
    state: ServerConnectionState,
    device_id: Option<String>,
    error_message: Option<String>,
    signal_addr: Option<String>,
    relay_addr: Option<String>,
) -> ServerStatusSnapshot {
    let device_id_formatted = device_id.as_deref().map(format_device_id);
    ServerStatusSnapshot {
        enabled: cfg.enabled,
        server_host: cfg.server_host.clone(),
        signal_port: cfg.signal_port,
        relay_port: cfg.relay_port,
        state,
        device_id,
        device_id_formatted,
        error_message,
        signal_addr,
        relay_addr,
        revision: 0,
    }
}

fn configure_tcp_keepalive(stream: &TcpStream) {
    let sock_ref = SockRef::from(stream);
    let _ = sock_ref.set_keepalive(true);
    let ka = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    let _ = sock_ref.set_tcp_keepalive(&ka);
}

#[derive(Debug)]
enum DialError {
    Stopped,
    Failed(String),
}

enum SingleConnectErr {
    Stopped,
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

fn connect_single_interruptible(
    addr: SocketAddr,
    timeout: Duration,
    stop_flag: &AtomicBool,
) -> Result<TcpStream, SingleConnectErr> {
    if stop_flag.load(Ordering::Acquire) {
        return Err(SingleConnectErr::Stopped);
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
                if stop_flag.load(Ordering::Acquire) {
                    return Err(SingleConnectErr::Stopped);
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

fn dial_tcp(
    addr_str: &str,
    timeout: Duration,
    stop_flag: &AtomicBool,
) -> Result<TcpStream, DialError> {
    if stop_flag.load(Ordering::Acquire) {
        return Err(DialError::Stopped);
    }
    let addrs: Vec<SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| DialError::Failed(format!("解析信令地址 {addr_str} 失败：{e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(DialError::Failed(format!(
            "信令地址 {addr_str} 未解析到可用 IP"
        )));
    }
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<io::Error> = None;
    let mut timed_out_any = false;
    for (idx, &addr) in addrs.iter().enumerate() {
        if stop_flag.load(Ordering::Acquire) {
            return Err(DialError::Stopped);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(DialError::Failed(format!("连接信令服务器 {addr_str} 超时")));
        }
        let budget = per_addr_timeout(deadline - now, addrs.len() - idx);
        match connect_single_interruptible(addr, budget, stop_flag) {
            Ok(stream) => return Ok(stream),
            Err(SingleConnectErr::Stopped) => return Err(DialError::Stopped),
            Err(SingleConnectErr::TimedOut) => timed_out_any = true,
            Err(SingleConnectErr::Io(err)) => last_err = Some(err),
        }
    }
    if last_err.is_none() && timed_out_any {
        return Err(DialError::Failed(format!("连接信令服务器 {addr_str} 超时")));
    }
    Err(DialError::Failed(format!(
        "连接信令服务器 {addr_str} 失败：{}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    )))
}

fn encode_outgoing_signal_frame(msg: &OutgoingSignalMsg) -> Result<Vec<u8>, String> {
    let payload = serde_json::to_vec(msg).map_err(|e| format!("编码信令 JSON 失败：{e}"))?;
    if payload.is_empty() || payload.len() > MAX_SIGNAL_FRAME_LEN {
        return Err(format!(
            "信令帧长度 {} 越界（要求 1..={MAX_SIGNAL_FRAME_LEN}）",
            payload.len()
        ));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn write_signal_frame_plain(stream: &mut TcpStream, msg: &OutgoingSignalMsg) -> Result<(), String> {
    let frame = encode_outgoing_signal_frame(msg)?;
    write_all_retry(stream, &frame).map_err(|e| format!("写入信令帧失败：{e}"))?;
    stream.flush().map_err(|e| format!("刷新信令缓冲失败：{e}"))
}

fn write_signal_frame(stream: &mut TcpStream, msg: &OutgoingSignalMsg) -> Result<(), String> {
    write_signal_frame_plain(stream, msg)
}

fn write_signal_frame_tls(
    tls: &mut StreamOwned<ClientConnection, TcpStream>,
    msg: &OutgoingSignalMsg,
) -> Result<(), String> {
    let frame = encode_outgoing_signal_frame(msg)?;
    let mut pos = 0usize;
    while pos < frame.len() || tls.conn.wants_write() {
        if pos < frame.len() {
            let written = tls
                .conn
                .writer()
                .write(&frame[pos..])
                .map_err(|e| format!("加密信令帧失败：{e}"))?;
            if written == 0 {
                return Err("加密信令帧返回 0 字节".to_string());
            }
            pos += written;
        }
        while tls.conn.wants_write() {
            match tls.conn.write_tls(&mut tls.sock) {
                Ok(0) => {
                    return Err("写入信令 TLS 数据返回 0 字节".to_string());
                }
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("写入信令 TLS 帧失败：{e}")),
            }
        }
    }
    loop {
        match tls.sock.flush() {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("刷新信令 TLS 缓冲失败：{e}")),
        }
    }
}

enum FrameRead {
    Message(Box<IncomingSignalMsg>),
    IdleTick,
    Eof,
    Stopped,
    Error(String),
}

fn read_signal_frame_from_io<S: Read>(stream: &mut S, stop_flag: &AtomicBool) -> FrameRead {
    let mut len_buf = [0u8; 4];
    let mut filled = 0usize;
    let mut partial_deadline: Option<Instant> = None;

    while filled < 4 {
        if stop_flag.load(Ordering::Acquire) {
            return FrameRead::Stopped;
        }
        match stream.read(&mut len_buf[filled..]) {
            Ok(0) => return FrameRead::Eof,
            Ok(n) => {
                filled += n;
                if filled < 4 && partial_deadline.is_none() {
                    partial_deadline = Some(Instant::now() + PARTIAL_FRAME_TIMEOUT);
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if filled == 0 {
                    return FrameRead::IdleTick;
                }
                if partial_deadline.is_some_and(|d| Instant::now() >= d) {
                    return FrameRead::Error("读取信令帧长度头超时".to_string());
                }
            }
            Err(e) if filled == 0 && e.kind() == ErrorKind::UnexpectedEof => {
                return FrameRead::Eof;
            }
            Err(e) => return FrameRead::Error(e.to_string()),
        }
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_SIGNAL_FRAME_LEN {
        return FrameRead::Error(format!(
            "非法信令帧长度 {len}（允许 1..={MAX_SIGNAL_FRAME_LEN}）"
        ));
    }

    let mut payload = vec![0u8; len];
    let mut pos = 0usize;
    let payload_deadline = Instant::now() + PARTIAL_FRAME_TIMEOUT;
    while pos < len {
        if stop_flag.load(Ordering::Acquire) {
            return FrameRead::Stopped;
        }
        match stream.read(&mut payload[pos..]) {
            Ok(0) => return FrameRead::Eof,
            Ok(n) => pos += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if Instant::now() >= payload_deadline {
                    return FrameRead::Error("读取信令帧负载超时".to_string());
                }
            }
            Err(e) => return FrameRead::Error(e.to_string()),
        }
    }

    match serde_json::from_slice::<IncomingSignalMsg>(&payload) {
        Ok(msg) => FrameRead::Message(Box::new(msg)),
        Err(err) => FrameRead::Error(format!("解析信令 JSON 失败：{err}")),
    }
}

fn write_all_retry(stream: &mut TcpStream, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match stream.write(buf) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "write zero on signal tcp stream",
                ));
            }
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn random_hex(num_bytes: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = vec![0u8; num_bytes];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(num_bytes * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn read_test_frame(stream: &mut TcpStream) -> serde_json::Value {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn write_test_frame(stream: &mut TcpStream, val: &serde_json::Value) {
        let body = serde_json::to_vec(val).unwrap();
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body);
        stream.write_all(&frame).unwrap();
        stream.flush().unwrap();
    }

    fn wait_until_connected(
        handle: &SignalClientHandle,
        timeout: Duration,
    ) -> ServerStatusSnapshot {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let snap = handle.status();
            if snap.state == ServerConnectionState::Connected {
                return snap;
            }
            thread::sleep(Duration::from_millis(15));
        }
        panic!(
            "timed out waiting for Connected; last={:?}",
            handle.status()
        );
    }

    #[test]
    fn mock_signal_server_register_connect_request_and_incoming_flow() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let signal_addr = listener.local_addr().unwrap();
        let udp_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp_addr = udp_sock.local_addr().unwrap();
        udp_sock
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let fake_relay_addr = "127.0.0.1:21117".to_string();
        let relay_for_server = fake_relay_addr.clone();
        let udp_for_server = udp_addr.to_string();

        let server_thread = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = conn.set_nodelay(true);

            let reg = read_test_frame(&mut conn);
            assert_eq!(reg["type"], "register");
            assert_eq!(reg["hostname"], "macbook-test");
            assert_eq!(reg["device_key"].as_str().unwrap().len(), 64);
            assert_eq!(
                reg["lan_addrs"],
                serde_json::json!([]),
                "initial Register must not leak :9901 before PersistentUdpProbe binds"
            );

            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "registered",
                    "device_id": "604928235",
                    "formatted_id": "604 928 235",
                    "server_time": "2026-11-22T12:00:00Z",
                    "udp_probe_addr": udp_for_server,
                    "relay_addr": relay_for_server,
                }),
            );

            // 注册成功后客户端立即发送携带持久化 local_udp_port 的 Heartbeat 同步局域网打洞地址
            let hb0 = read_test_frame(&mut conn);
            assert_eq!(hb0["type"], "heartbeat");
            let hb0_lan = hb0["lan_addrs"].as_array().unwrap();
            assert_eq!(hb0_lan.len(), 1);
            let hb0_addr = hb0_lan[0].as_str().unwrap().to_string();
            assert!(
                hb0_addr.starts_with("192.168.1.10:") && !hb0_addr.ends_with(":9901"),
                "immediate heartbeat must rewrite lan_addr port to local_udp_port: {hb0_addr}"
            );

            // 第 1 次 connect_request：在线对端 -> 返回 punch_coord（同样携带改写后的 local_udp_port）
            let req1 = read_test_frame(&mut conn);
            assert_eq!(req1["type"], "connect_request");
            assert_eq!(req1["target_id"], "987654321");
            assert_eq!(req1["lan_addrs"], serde_json::json!([hb0_addr]));
            let sid1 = req1["session_id"].as_str().unwrap().to_string();

            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "punch_coord",
                    "session_id": sid1,
                    "role": "client",
                    "peer_id": "987654321",
                    "peer_public_udp_addr": "198.51.100.22:60123",
                    "peer_lan_addrs": ["192.168.1.20:9901"],
                    "nonce": "deadbeef",
                    "relay_token": "tok-outbound-001",
                    "relay_addr": relay_for_server,
                }),
            );

            // 推送一条入站 host 通知（另一台设备来连我们）
            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "punch_coord",
                    "session_id": "incoming-sess-42",
                    "role": "host",
                    "peer_id": "111222333",
                    "peer_public_udp_addr": "203.0.113.9:51234",
                    "peer_lan_addrs": ["192.168.1.30:9901"],
                    "nonce": "cafebabe",
                    "relay_token": "tok-inbound-002",
                    "relay_addr": relay_for_server,
                }),
            );

            // 第 2 次 connect_request：离线对端 -> 返回 peer_offline
            let req2 = read_test_frame(&mut conn);
            assert_eq!(req2["type"], "connect_request");
            assert_eq!(req2["target_id"], "999888777");
            let sid2 = req2["session_id"].as_str().unwrap().to_string();

            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "peer_offline",
                    "target_id": "999888777",
                    "session_id": sid2,
                }),
            );

            // 等待客户端 stop() 关闭连接
            let mut sink = [0u8; 16];
            let _ = conn.read(&mut sink);
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: signal_addr.port(),
            relay_port: 21117,
            use_https: false,
            device_key: crate::server_config::generate_device_key(),
            cached_device_id: None,
            ..ServerConfig::default()
        };

        let mut client = SignalClientHandle::spawn(
            cfg,
            "macbook-test".to_string(),
            vec!["192.168.1.10:9901".to_string()],
        );

        let snap = wait_until_connected(&client, Duration::from_secs(3));
        assert_eq!(snap.device_id.as_deref(), Some("604928235"));
        assert_eq!(snap.device_id_formatted.as_deref(), Some("604 928 235"));
        assert_eq!(snap.relay_addr.as_deref(), Some("127.0.0.1:21117"));
        assert!(snap.revision > 0);

        // 验证注册后自动发出合法 22 字节 UDP RCSP 探测包，并记住其持久化本地端口
        let mut udp_buf = [0u8; 64];
        let (n, probe_from) = udp_sock
            .recv_from(&mut udp_buf)
            .expect("expected UDP RCSP probe after register");
        assert_eq!(n, UDP_PROBE_LEN);
        assert_eq!(&udp_buf[0..4], b"RCSP");
        assert_eq!(udp_buf[4], 1);
        assert_eq!(&udp_buf[5..14], b"604928235");
        let probe_port = probe_from.port();
        assert_ne!(probe_port, 0);

        // 发起出站连接请求（带空格格式化的 9 位设备码也能被规范化）
        let grant = client
            .request_connect("987 654 321", Duration::from_secs(2))
            .expect("connect_request must resolve with ConnectGrant");
        assert_eq!(grant.target_id, "987654321");
        assert_eq!(grant.relay_token, "tok-outbound-001");
        assert_eq!(grant.relay_addr, "127.0.0.1:21117");
        assert_eq!(
            grant.peer_public_addr.as_deref(),
            Some("198.51.100.22:60123")
        );
        assert_eq!(grant.peer_lan_addrs, vec!["192.168.1.20:9901".to_string()]);
        assert_eq!(grant.local_udp_port, Some(probe_port));

        // 验证入站 host 通知被放入 try_recv_incoming() 队列
        let incoming = {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(inc) = client.try_recv_incoming() {
                    break inc;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for RelayIncoming"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(
            incoming,
            RelayIncoming {
                session_id: "incoming-sess-42".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok-inbound-002".to_string(),
                peer_id: "111222333".to_string(),
                peer_public_addr: Some("203.0.113.9:51234".to_string()),
                peer_lan_addrs: vec!["192.168.1.30:9901".to_string()],
                local_udp_port: Some(probe_port),
                owner_grant: None,
                is_owner_connection: false,
            }
        );

        // 验证 peer_offline 立即以可读错误返回
        let offline_err = client
            .request_connect("999 888 777", Duration::from_secs(2))
            .unwrap_err();
        assert!(
            offline_err.contains("999 888 777") && offline_err.contains("不在线"),
            "unexpected offline error: {offline_err}"
        );

        client.stop();
        server_thread.join().unwrap();
    }

    #[test]
    fn disabled_config_stays_disabled_and_rejects_connect() {
        let cfg = ServerConfig::default();
        let mut handle = SignalClientHandle::spawn(cfg, "host".to_string(), Vec::new());
        assert_eq!(handle.status().state, ServerConnectionState::Disabled);
        assert!(handle.status().revision > 0);
        assert!(handle
            .request_connect("604928235", Duration::from_millis(200))
            .is_err());
        handle.stop();
    }

    #[test]
    fn probe_server_connectivity_checks_both_ports_and_honors_inline_port() {
        let sig_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let rel_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sig_port = sig_listener.local_addr().unwrap().port();
        let rel_port = rel_listener.local_addr().unwrap().port();

        let msg =
            probe_server_connectivity("127.0.0.1", sig_port, rel_port, Duration::from_secs(2))
                .expect("both listening ports must probe ok");
        assert!(msg.contains("ms"), "expected latency in probe msg: {msg}");

        // L1: host 中携带显式端口时应优先使用解析出的端口（即使传入的 signal_port 不同）
        let inline_host = format!("127.0.0.1:{sig_port}");
        let msg_inline =
            probe_server_connectivity(&inline_host, 1, rel_port, Duration::from_secs(2))
                .expect("inline signal port in host must be honored");
        assert!(msg_inline.contains(&format!("127.0.0.1:{sig_port}")));

        // 关闭中继监听后应报错
        drop(rel_listener);
        let err =
            probe_server_connectivity("127.0.0.1", sig_port, rel_port, Duration::from_secs(1))
                .unwrap_err();
        assert!(err.contains("中继端口"), "unexpected error: {err}");
        drop(sig_listener);
    }

    #[test]
    fn build_udp_probe_packet_layout_and_device_id_validation() {
        let nonce = [1, 2, 3, 4, 5, 6, 7, 8];
        let pkt = build_udp_probe_packet("604928235", nonce).unwrap();
        assert_eq!(&pkt[0..4], b"RCSP");
        assert_eq!(pkt[4], 1);
        assert_eq!(&pkt[5..14], b"604928235");
        assert_eq!(&pkt[14..22], &nonce);

        // 协议允许匿名探测码 "000000000"
        let anon = build_udp_probe_packet("000000000", nonce).unwrap();
        assert_eq!(&anon[5..14], b"000000000");

        // 拒绝其余以 '0' 开头的非法 9 位码与格式错误输入
        assert!(build_udp_probe_packet("012345678", nonce).is_err());
        assert!(build_udp_probe_packet("000000001", nonce).is_err());
        assert!(build_udp_probe_packet("short", nonce).is_err());
        assert!(build_udp_probe_packet("60492823a", nonce).is_err());
    }

    #[test]
    fn relay_ready_and_status_callback_monotonic_revision() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let signal_addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = read_test_frame(&mut conn);
            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "registered",
                    "device_id": "222333444",
                    "formatted_id": "222 333 444",
                    "server_time": "2026-11-22T12:00:00Z",
                    "udp_probe_addr": "0.0.0.0:21116",
                    "relay_addr": "0.0.0.0:21117",
                }),
            );

            let hb = read_test_frame(&mut conn);
            assert_eq!(hb["type"], "heartbeat");

            let req = read_test_frame(&mut conn);
            assert_eq!(req["type"], "connect_request");
            let sid = req["session_id"].as_str().unwrap().to_string();
            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "relay_ready",
                    "session_id": sid,
                    "role": "client",
                    "peer_id": "555666777",
                    "relay_token": "tok-relay-ready",
                    "relay_addr": "0.0.0.0:21117",
                    "expires_in_sec": 60,
                }),
            );

            let mut sink = [0u8; 8];
            let _ = conn.read(&mut sink);
        });

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = Arc::clone(&seen);

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: signal_addr.port(),
            relay_port: 21117,
            use_https: false,
            device_key: crate::server_config::generate_device_key(),
            cached_device_id: None,
            ..ServerConfig::default()
        };

        let mut client = SignalClientHandle::spawn_with_callback(
            cfg.clone(),
            "cb-test".to_string(),
            Vec::new(),
            move |snap| {
                seen_clone.lock().unwrap().push((snap.state, snap.revision));
            },
        );

        let snap = wait_until_connected(&client, Duration::from_secs(3));
        assert_eq!(snap.relay_addr.as_deref(), Some("127.0.0.1:21117"));

        let grant = client
            .request_connect("555666777", Duration::from_secs(2))
            .expect("relay_ready must resolve pending request_connect");
        assert_eq!(grant.relay_token, "tok-relay-ready");
        assert_eq!(grant.relay_addr, "127.0.0.1:21117");
        assert_eq!(grant.target_id, "555666777");

        let pairs = seen.lock().unwrap().clone();
        assert!(pairs.len() >= 2);
        assert_eq!(pairs[0].0, ServerConnectionState::Connecting);
        assert_eq!(pairs[1].0, ServerConnectionState::Connected);
        // M9: revision 严格单调递增
        for w in pairs.windows(2) {
            assert!(
                w[1].1 > w[0].1,
                "revision must strictly increase: {pairs:?}"
            );
        }

        // 重启/禁用快照的 revision 仍严格大于旧句柄
        let next_disabled = cfg.disabled_snapshot();
        assert!(next_disabled.revision > pairs.last().unwrap().1);

        client.stop();
        server.join().unwrap();
    }

    #[test]
    fn resolve_advertised_addr_handles_loopback_unspecified_ipv4_mapped_and_bare_hosts() {
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:21117", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:31117", "172.16.1.2", 21117),
            "172.16.1.2:31117"
        );
        assert_eq!(
            resolve_advertised_addr("[::1]:21116", "172.16.1.2", 21116),
            "172.16.1.2:21116"
        );
        assert_eq!(
            resolve_advertised_addr("localhost:21117", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("0.0.0.0:21117", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:21117", "2001:db8::1", 21117),
            "[2001:db8::1]:21117"
        );
        // When fallback itself is loopback, keep advertised loopback addr as-is.
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:54321", "127.0.0.1", 21117),
            "127.0.0.1:54321"
        );
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:54321", "localhost", 21117),
            "127.0.0.1:54321"
        );
        // Explicit public IP from server is preserved.
        assert_eq!(
            resolve_advertised_addr("198.51.100.5:21117", "172.16.1.2", 21117),
            "198.51.100.5:21117"
        );

        // M3: IPv4-mapped IPv6 规范化后识别回环与未指定地址
        assert_eq!(
            resolve_advertised_addr("[::ffff:127.0.0.1]:21117", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("[::ffff:0.0.0.0]:31117", "172.16.1.2", 21117),
            "172.16.1.2:31117"
        );
        assert_eq!(
            resolve_advertised_addr("[::ffff:198.51.100.5]:21117", "172.16.1.2", 21117),
            "198.51.100.5:21117"
        );

        // M3: port == 0 在所有路径（含非回环与回环对回环）下均替换为 fallback_port
        assert_eq!(
            resolve_advertised_addr("198.51.100.5:0", "172.16.1.2", 21117),
            "198.51.100.5:21117"
        );
        assert_eq!(
            resolve_advertised_addr("127.0.0.1:0", "127.0.0.1", 21117),
            "127.0.0.1:21117"
        );
        assert_eq!(
            resolve_advertised_addr("relay.example.com:0", "172.16.1.2", 21117),
            "relay.example.com:21117"
        );

        // M3: 裸主机名 / 裸 IP 补全端口并执行回环/未指定重写
        assert_eq!(
            resolve_advertised_addr("localhost", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("0.0.0.0", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("::1", "172.16.1.2", 21117),
            "172.16.1.2:21117"
        );
        assert_eq!(
            resolve_advertised_addr("relay.example.com", "172.16.1.2", 21117),
            "relay.example.com:21117"
        );
    }

    #[test]
    fn sanitize_hostname_and_lan_addrs_enforce_bounds_and_strip_control_chars() {
        assert_eq!(sanitize_hostname("  my\nhost\0\x1bname  "), "myhostname");

        // 多字节 UTF-8 截断不得切在字符中间，且字节数 <= 128
        let long_cjk = "中文主机名".repeat(20); // 100 字符 = 300 字节
        let truncated = sanitize_hostname(&long_cjk);
        assert!(truncated.len() <= MAX_HOSTNAME_BYTES);
        assert!(truncated.len() >= MAX_HOSTNAME_BYTES - 3);
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());

        // lan_addrs 过滤空串、超 64B、含空白/控制/非 ASCII 字符，最多保留 16 条
        let mut raw = vec![
            "".to_string(),
            "192.168.1.1:9901\n".to_string(),
            "192.168.1.1 9901".to_string(),
            "a".repeat(65),
            "中文:9901".to_string(),
        ];
        for i in 1..=20 {
            raw.push(format!("192.168.1.{i}:9901"));
        }
        let cleaned = sanitize_lan_addrs(&raw);
        assert_eq!(cleaned.len(), MAX_LAN_ADDRS);
        assert_eq!(cleaned[0], "192.168.1.1:9901");
        assert_eq!(cleaned[15], "192.168.1.16:9901");
    }

    #[test]
    fn dispatch_coord_and_peer_offline_enforce_strict_role_and_session_matching() {
        let cfg = ServerConfig::default();
        let shared = SharedState {
            status: Mutex::new(cfg.disabled_snapshot()),
            status_updates: Mutex::new(VecDeque::new()),
            incoming: Mutex::new(VecDeque::new()),
            device_status_events: Mutex::new(VecDeque::new()),
            kicked_reasons: Mutex::new(VecDeque::new()),
            pending_connects: Mutex::new(HashMap::new()),
            active_writer: Mutex::new(None),
            shutdown_stream: Mutex::new(None),
            stop_flag: AtomicBool::new(false),
            stop_lock: Mutex::new(false),
            stop_cv: Condvar::new(),
            local_udp_port: AtomicU16::new(45678),
            local_udp_is_ipv6: AtomicBool::new(false),
            lan_addrs: Vec::new(),
            enabled: true,
            token_provider: None,
            device_pubkey: None,
            allow_remote_assist: AtomicU8::new(ASSIST_UNSET),
        };

        let (tx1, rx1) = mpsc::sync_channel(1);
        shared.pending_connects.lock().unwrap().insert(
            "sess-A".to_string(),
            PendingConnect {
                target_id: "987654321".to_string(),
                tx: tx1,
            },
        );

        // H5: 非空但未匹配的 session_id 不得误匹配同一 target_id 的其他 pending 会话，也不得推入 incoming
        dispatch_coord(
            &shared,
            CoordPayload {
                session_id: "sess-OTHER".to_string(),
                role: "client".to_string(),
                peer_id: "987654321".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok-wrong".to_string(),
                peer_public_addr: None,
                peer_lan_addrs: Vec::new(),
                owner_grant: None,
                is_owner_connection: false,
            },
        );
        assert!(rx1.try_recv().is_err());
        assert!(shared.incoming.lock().unwrap().is_empty());

        // H5: 未知角色不得推入 incoming 也不得匹配 pending
        dispatch_coord(
            &shared,
            CoordPayload {
                session_id: "sess-A".to_string(),
                role: "observer".to_string(),
                peer_id: "987654321".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok-bogus".to_string(),
                peer_public_addr: None,
                peer_lan_addrs: Vec::new(),
                owner_grant: None,
                is_owner_connection: false,
            },
        );
        assert!(rx1.try_recv().is_err());
        assert!(shared.incoming.lock().unwrap().is_empty());

        // H5: PeerOffline 携带不匹配的 session_id 不得误杀 sess-A
        handle_incoming_message(
            &shared,
            IncomingSignalMsg::PeerOffline {
                target_id: "987654321".to_string(),
                session_id: "sess-OTHER".to_string(),
            },
            "127.0.0.1",
            21117,
            "127.0.0.1:21117",
            "127.0.0.1:21117",
        );
        assert!(rx1.try_recv().is_err());

        // 仅当 session_id 为空时才按 target_id 兜底匹配；空串 peer_public_addr 清洗为 None
        dispatch_coord(
            &shared,
            CoordPayload {
                session_id: "".to_string(),
                role: "client".to_string(),
                peer_id: "987654321".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok-ok".to_string(),
                peer_public_addr: Some("   ".to_string()),
                peer_lan_addrs: vec!["10.0.0.2:50000".to_string()],
                owner_grant: None,
                is_owner_connection: false,
            },
        );
        let grant = rx1.try_recv().unwrap().unwrap();
        assert_eq!(grant.relay_token, "tok-ok");
        assert_eq!(grant.peer_public_addr, None);
        assert_eq!(grant.peer_lan_addrs, vec!["10.0.0.2:50000".to_string()]);
        assert_eq!(grant.local_udp_port, Some(45678));
        assert!(shared.incoming.lock().unwrap().is_empty());

        // 只有 role == "host" 才进入 incoming
        dispatch_coord(
            &shared,
            CoordPayload {
                session_id: "in-1".to_string(),
                role: "host".to_string(),
                peer_id: "111222333".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok-in".to_string(),
                peer_public_addr: Some("198.51.100.7:44444".to_string()),
                peer_lan_addrs: vec!["192.168.1.9:9901".to_string()],
                owner_grant: None,
                is_owner_connection: false,
            },
        );
        let inc = shared.incoming.lock().unwrap().pop_front().unwrap();
        assert_eq!(inc.peer_public_addr.as_deref(), Some("198.51.100.7:44444"));
        assert_eq!(inc.peer_lan_addrs, vec!["192.168.1.9:9901".to_string()]);
        assert_eq!(inc.local_udp_port, Some(45678));
    }

    #[test]
    fn kicked_terminal_reasons_stop_reconnecting_and_backoff_policy_is_strict() {
        assert!(!should_reset_backoff(
            Duration::from_secs(10),
            false,
            STABLE_CONNECTION_THRESHOLD
        ));
        assert!(should_reset_backoff(
            Duration::from_secs(30),
            false,
            STABLE_CONNECTION_THRESHOLD
        ));
        assert!(!should_reset_backoff(
            Duration::from_secs(300),
            true,
            STABLE_CONNECTION_THRESHOLD
        ));
        assert!(classify_kicked_reason("replaced_by_new_connection").0);
        assert!(classify_kicked_reason("disabled_by_admin").0);
        assert!(!classify_kicked_reason("server_shutdown").0);

        // 端到端验证：收到 Kicked(replaced_by_new_connection) 后发出 Error 且不再重连
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let signal_addr = listener.local_addr().unwrap();
        let accept_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_count_srv = Arc::clone(&accept_count);
        listener.set_nonblocking(false).unwrap();

        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            accept_count_srv.fetch_add(1, Ordering::SeqCst);
            let _ = read_test_frame(&mut conn);
            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "registered",
                    "device_id": "333444555",
                    "formatted_id": "333 444 555",
                }),
            );
            write_test_frame(
                &mut conn,
                &serde_json::json!({
                    "type": "kicked",
                    "reason": "replaced_by_new_connection",
                }),
            );
            // 若客户端错误地自动重连，这里会接到第 2 次连接
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(350);
            while Instant::now() < deadline {
                if listener.accept().is_ok() {
                    accept_count_srv.fetch_add(1, Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: signal_addr.port(),
            relay_port: 21117,
            use_https: false,
            device_key: crate::server_config::generate_device_key(),
            cached_device_id: None,
            ..ServerConfig::default()
        };
        let fast_timing = SignalTiming {
            heartbeat_interval: Duration::from_secs(30),
            activity_timeout: Duration::from_secs(75),
            stable_threshold: Duration::from_secs(30),
            backoffs: [
                Duration::from_millis(30),
                Duration::from_millis(60),
                Duration::from_millis(120),
                Duration::from_millis(240),
            ],
        };
        let mut client = SignalClientHandle::spawn_with_timing(
            cfg,
            "kick-test".to_string(),
            Vec::new(),
            None,
            fast_timing,
            |_| {},
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if client.status().state == ServerConnectionState::Error {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        let snap = client.status();
        assert_eq!(snap.state, ServerConnectionState::Error);
        assert!(
            snap.error_message
                .as_deref()
                .unwrap_or("")
                .contains("已停止自动重连"),
            "unexpected kicked error msg: {:?}",
            snap.error_message
        );

        server.join().unwrap();
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "terminal Kicked must not auto-reconnect"
        );
        client.stop();
    }

    #[test]
    fn dead_connection_without_server_activity_triggers_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let signal_addr = listener.local_addr().unwrap();
        let accept_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_count_srv = Arc::clone(&accept_count);

        let server = thread::spawn(move || {
            for round in 0..2 {
                let (mut conn, _) = listener.accept().unwrap();
                let _ = read_test_frame(&mut conn);
                write_test_frame(
                    &mut conn,
                    &serde_json::json!({
                        "type": "registered",
                        "device_id": "444555666",
                        "formatted_id": "444 555 666",
                    }),
                );
                accept_count_srv.fetch_add(1, Ordering::SeqCst);
                // 第 1 轮故意不回 HeartbeatAck（模拟半开死链），等待客户端超时重连
                if round == 0 {
                    let mut sink = [0u8; 64];
                    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                    while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
                } else {
                    let mut sink = [0u8; 16];
                    let _ = conn.read(&mut sink);
                }
            }
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: signal_addr.port(),
            relay_port: 21117,
            use_https: false,
            device_key: crate::server_config::generate_device_key(),
            cached_device_id: None,
            ..ServerConfig::default()
        };
        let fast_timing = SignalTiming {
            heartbeat_interval: Duration::from_millis(80),
            activity_timeout: Duration::from_millis(250),
            stable_threshold: Duration::from_secs(30),
            backoffs: [
                Duration::from_millis(40),
                Duration::from_millis(80),
                Duration::from_millis(120),
                Duration::from_millis(200),
            ],
        };
        let mut client = SignalClientHandle::spawn_with_timing(
            cfg,
            "m4-dead-conn".to_string(),
            Vec::new(),
            None,
            fast_timing,
            |_| {},
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        while accept_count.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "client should detect dead connection via activity timeout and reconnect"
        );

        client.stop();
        server.join().unwrap();
    }

    #[test]
    fn dial_tcp_and_signal_stop_interrupt_promptly_without_waiting_on_writer_lock() {
        // H2: 预先置位 stop_flag 时 dial_tcp 立即返回 Stopped
        let stop = AtomicBool::new(true);
        assert!(matches!(
            dial_tcp("127.0.0.1:1", Duration::from_secs(5), &stop),
            Err(DialError::Stopped)
        ));

        // H2: 对已关闭端口的非阻塞拨号必须快速返回 Failed（而非耗尽整个 5s 超时）
        let temp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_addr = temp_listener.local_addr().unwrap();
        drop(temp_listener);
        let no_stop = AtomicBool::new(false);
        let t0 = Instant::now();
        assert!(matches!(
            dial_tcp(&closed_addr.to_string(), Duration::from_secs(5), &no_stop),
            Err(DialError::Failed(_))
        ));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "closed port dial should fail fast, took {:?}",
            t0.elapsed()
        );

        // H2: 拨号过程中外部置位 stop_flag 应迅速中断（测试黑洞地址 RFC 5737 TEST-NET-1）
        let cancel_mid = Arc::new(AtomicBool::new(false));
        let cancel_trigger = Arc::clone(&cancel_mid);
        let trigger_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel_trigger.store(true, Ordering::Release);
        });
        let t1 = Instant::now();
        let mid_res = dial_tcp("192.0.2.1:21116", Duration::from_secs(5), &cancel_mid);
        assert!(mid_res.is_err());
        assert!(
            t1.elapsed() < Duration::from_secs(2),
            "in-flight dial must abort promptly on stop_flag, took {:?}",
            t1.elapsed()
        );
        let _ = trigger_thread.join();

        // H7/M2: 即使 active_writer 锁被占用，signal_stop() 仍能立刻失败 pending 并关闭 shutdown_stream
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8];
            let _ = s.read(&mut buf);
        });
        let stream = TcpStream::connect(addr).unwrap();
        let shutdown_clone = stream.try_clone().unwrap();

        let cfg = ServerConfig::default();
        let shared = Arc::new(SharedState {
            status: Mutex::new(cfg.disabled_snapshot()),
            status_updates: Mutex::new(VecDeque::new()),
            incoming: Mutex::new(VecDeque::new()),
            device_status_events: Mutex::new(VecDeque::new()),
            kicked_reasons: Mutex::new(VecDeque::new()),
            pending_connects: Mutex::new(HashMap::new()),
            active_writer: Mutex::new(Some(SignalWriter::Plain(stream))),
            shutdown_stream: Mutex::new(Some(shutdown_clone)),
            stop_flag: AtomicBool::new(false),
            stop_lock: Mutex::new(false),
            stop_cv: Condvar::new(),
            local_udp_port: AtomicU16::new(0),
            local_udp_is_ipv6: AtomicBool::new(false),
            lan_addrs: Vec::new(),
            enabled: true,
            token_provider: None,
            device_pubkey: None,
            allow_remote_assist: AtomicU8::new(ASSIST_UNSET),
        });

        let (tx, rx) = mpsc::sync_channel(1);
        shared.pending_connects.lock().unwrap().insert(
            "s1".to_string(),
            PendingConnect {
                target_id: "604928235".to_string(),
                tx,
            },
        );

        // 故意持有 active_writer 锁，验证另一线程调用 signal_stop() 不会死锁/阻塞
        let _held_writer = shared.active_writer.lock().unwrap();
        let shared_for_stop = Arc::clone(&shared);
        let stopper = thread::spawn(move || {
            shared_for_stop.signal_stop();
        });
        stopper.join().unwrap();

        let pending_res = rx.recv_timeout(Duration::from_millis(500)).unwrap();
        assert!(pending_res.is_err());
        assert!(shared.shutdown_stream.lock().unwrap().is_none());
        drop(_held_writer);
        srv.join().unwrap();
    }

    #[test]
    fn resolve_udp_probe_target_prefers_matching_ip_family() {
        let v4_peer: SocketAddr = "127.0.0.1:21116".parse().unwrap();
        let target = resolve_udp_probe_target("127.0.0.1:21116", Some(v4_peer)).unwrap();
        assert!(target.is_ipv4());
        assert_eq!(target, v4_peer);
    }

    #[cfg(unix)]
    #[test]
    fn persistent_udp_probe_keeps_stable_port_and_coexists_with_reuseport_p2p_socket() {
        let sig_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sig_addr = sig_udp.local_addr().unwrap();
        sig_udp
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let probe = PersistentUdpProbe::bind(sig_addr).expect("bind persistent probe");
        let port = probe.local_port();
        assert_ne!(port, 0);

        // 连续两次探测均从同一本地端口发出
        let mut buf = [0u8; 64];
        probe.send_probe("604928235");
        let (n1, from1) = sig_udp.recv_from(&mut buf).unwrap();
        assert_eq!(n1, UDP_PROBE_LEN);
        assert_eq!(from1.port(), port);

        probe.send_probe("604928235");
        let (n2, from2) = sig_udp.recv_from(&mut buf).unwrap();
        assert_eq!(n2, UDP_PROBE_LEN);
        assert_eq!(from2.port(), port);

        // 在 probe 仍存活时，P2P 打洞套接字以 SO_REUSEADDR + SO_REUSEPORT 绑定同一端口，
        // 来自外部 peer（非信令服务器）的 UDP 数据报必须全部落到 P2P 套接字上。
        let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        raw.set_reuse_address(true).unwrap();
        #[cfg(unix)]
        raw.set_reuse_port(true).unwrap();
        raw.bind(&SockAddr::from(SocketAddr::from(([0, 0, 0, 0], port))))
            .expect("P2P socket must share local_udp_port");
        let p2p_sock: UdpSocket = raw.into();
        p2p_sock
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        for i in 0u8..5 {
            let payload = [b'P', b'2', b'P', i];
            peer.send_to(&payload, SocketAddr::from(([127, 0, 0, 1], port)))
                .unwrap();
            let (rn, rfrom) = p2p_sock
                .recv_from(&mut buf)
                .expect("p2p must recv peer pkt");
            assert_eq!(&buf[..rn], &payload);
            assert_eq!(rfrom, peer.local_addr().unwrap());
        }
    }

    #[test]
    fn effective_lan_addrs_rewrites_ports_to_local_udp_port() {
        let raw = vec![
            "192.168.1.10:9901".to_string(),
            "[::ffff:192.168.1.10]:9901".to_string(), // IPv4-mapped 去重
            "10.0.0.5".to_string(),                   // 裸 IPv4 补全端口
            "[fd00::1]:9901".to_string(),             // IPv6 改写端口
        ];
        // H4: local_udp_port == 0 时必须返回空列表，绝不泄漏 :9901 作为打洞候选
        assert!(
            effective_lan_addrs(&raw, 0, false).is_empty(),
            "local_udp_port == 0 must return empty lan_addrs (IPv4)"
        );
        assert!(
            effective_lan_addrs(&raw, 0, true).is_empty(),
            "local_udp_port == 0 must return empty lan_addrs (IPv6)"
        );
        // H3: IPv4 探测套接字仅改写并上报 IPv4 LAN 地址（自动去重 ::ffff: 并跳过 IPv6）
        assert_eq!(
            effective_lan_addrs(&raw, 54321, false),
            vec![
                "192.168.1.10:54321".to_string(),
                "10.0.0.5:54321".to_string(),
            ]
        );
        // H3: IPv6 探测套接字仅改写并上报 IPv6 LAN 地址（跳过 IPv4）
        assert_eq!(
            effective_lan_addrs(&raw, 54321, true),
            vec!["[fd00::1]:54321".to_string()]
        );
    }

    #[test]
    fn persistent_udp_probe_connect_failure_drops_socket_and_clears_local_udp_port() {
        let target: SocketAddr = "127.0.0.1:21116".parse().unwrap();
        let failed = PersistentUdpProbe::bind_with(target, |_sock, _addr| {
            Err(io::Error::new(
                ErrorKind::NetworkUnreachable,
                "simulated connect failure",
            ))
        });
        assert!(
            failed.is_none(),
            "failed connect must return None and never keep an unconnected SO_REUSEPORT socket"
        );
        // 端口为 0 或未指定地址也直接返回 None
        assert!(PersistentUdpProbe::bind("0.0.0.0:0".parse().unwrap()).is_none());
        assert!(PersistentUdpProbe::bind("127.0.0.1:0".parse().unwrap()).is_none());

        // 验证当探测绑定失败时，SharedState::local_udp_port 被置为 0，下发的凭据为 None
        let cfg = ServerConfig::default();
        let shared = SharedState {
            status: Mutex::new(cfg.disabled_snapshot()),
            status_updates: Mutex::new(VecDeque::new()),
            incoming: Mutex::new(VecDeque::new()),
            device_status_events: Mutex::new(VecDeque::new()),
            kicked_reasons: Mutex::new(VecDeque::new()),
            pending_connects: Mutex::new(HashMap::new()),
            active_writer: Mutex::new(None),
            shutdown_stream: Mutex::new(None),
            stop_flag: AtomicBool::new(false),
            stop_lock: Mutex::new(false),
            stop_cv: Condvar::new(),
            local_udp_port: AtomicU16::new(45678),
            local_udp_is_ipv6: AtomicBool::new(false),
            lan_addrs: Vec::new(),
            enabled: true,
            token_provider: None,
            device_pubkey: None,
            allow_remote_assist: AtomicU8::new(ASSIST_UNSET),
        };
        shared.local_udp_port.store(
            failed.as_ref().map_or(0, PersistentUdpProbe::local_port),
            Ordering::Release,
        );
        assert_eq!(shared.local_udp_port.load(Ordering::Acquire), 0);

        dispatch_coord(
            &shared,
            CoordPayload {
                session_id: "sess-no-probe".to_string(),
                role: "host".to_string(),
                peer_id: "111222333".to_string(),
                relay_addr: "127.0.0.1:21117".to_string(),
                relay_token: "tok".to_string(),
                peer_public_addr: None,
                peer_lan_addrs: Vec::new(),
                owner_grant: None,
                is_owner_connection: false,
            },
        );
        let inc = shared.incoming.lock().unwrap().pop_front().unwrap();
        assert_eq!(inc.local_udp_port, None);
    }

    #[test]
    fn tls13_signal_reconnect_fetches_rotated_token_and_handles_assist_status_owner_grant_and_kick()
    {
        use std::sync::atomic::AtomicUsize;

        let server_cert_pem = rc_session::dev_cert::dev_server_cert_pem();
        let server_key_pem = rc_session::dev_cert::DEV_KEY_PEM;
        let certs: Vec<_> = rustls_pemfile::certs(&mut server_cert_pem.as_bytes())
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(&mut server_key_pem.as_bytes())
            .unwrap()
            .unwrap();
        let tls_server_cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let signal_addr = listener.local_addr().unwrap();

        let server_thread = thread::spawn(move || {
            // Round 1: initial connection -> accept TLS, verify Register fields, drop with server_shutdown kick
            {
                let (tcp, _) = listener.accept().unwrap();
                let conn = rustls::ServerConnection::new(Arc::clone(&tls_server_cfg)).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, tcp);

                let mut len_buf = [0u8; 4];
                tls.read_exact(&mut len_buf).unwrap();
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut body = vec![0u8; len];
                tls.read_exact(&mut body).unwrap();
                let reg: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(reg["type"], "register");
                assert_eq!(reg["access_token"], "tok-round-1");
                assert_eq!(reg["device_pubkey"], "a".repeat(64));
                assert_eq!(reg["allow_remote_assist"], false);

                let write_tls_json =
                    |s: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
                     val: &serde_json::Value| {
                        let b = serde_json::to_vec(val).unwrap();
                        let mut frame = Vec::with_capacity(4 + b.len());
                        frame.extend_from_slice(&(b.len() as u32).to_be_bytes());
                        frame.extend_from_slice(&b);
                        s.write_all(&frame).unwrap();
                        s.flush().unwrap();
                    };

                write_tls_json(
                    &mut tls,
                    &serde_json::json!({
                        "type": "registered",
                        "device_id": "604928235",
                        "formatted_id": "604 928 235",
                        "udp_probe_addr": "127.0.0.1:21116",
                        "relay_addr": "127.0.0.1:21117",
                    }),
                );

                // Expect server_shutdown kick to trigger reconnect with rotated token
                write_tls_json(
                    &mut tls,
                    &serde_json::json!({
                        "type": "kicked",
                        "reason": "server_shutdown",
                    }),
                );
            }

            // Round 2: reconnect -> must fetch rotated token "tok-round-2", handle UpdateAssistState,
            // push DeviceStatusChanged, PunchCoord with owner_grant, and terminal kick "account_disabled".
            let (tcp, _) = listener.accept().unwrap();
            let conn = rustls::ServerConnection::new(tls_server_cfg).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, tcp);

            let read_tls_json = |s: &mut rustls::StreamOwned<
                rustls::ServerConnection,
                TcpStream,
            >|
             -> serde_json::Value {
                let mut len_buf = [0u8; 4];
                s.read_exact(&mut len_buf).unwrap();
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut body = vec![0u8; len];
                s.read_exact(&mut body).unwrap();
                serde_json::from_slice(&body).unwrap()
            };
            let write_tls_json =
                |s: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>,
                 val: &serde_json::Value| {
                    let b = serde_json::to_vec(val).unwrap();
                    let mut frame = Vec::with_capacity(4 + b.len());
                    frame.extend_from_slice(&(b.len() as u32).to_be_bytes());
                    frame.extend_from_slice(&b);
                    s.write_all(&frame).unwrap();
                    s.flush().unwrap();
                };

            let reg2 = read_tls_json(&mut tls);
            assert_eq!(reg2["type"], "register");
            assert_eq!(reg2["access_token"], "tok-round-2");

            write_tls_json(
                &mut tls,
                &serde_json::json!({
                    "type": "registered",
                    "device_id": "604928235",
                    "formatted_id": "604 928 235",
                    "udp_probe_addr": "127.0.0.1:21116",
                    "relay_addr": "127.0.0.1:21117",
                }),
            );

            // Read initial heartbeat
            let hb = read_tls_json(&mut tls);
            assert_eq!(hb["type"], "heartbeat");

            // Push DeviceStatusChanged
            write_tls_json(
                &mut tls,
                &serde_json::json!({
                    "type": "device_status_changed",
                    "device_id": "777888999",
                    "online": true,
                    "hostname": "Office Mini",
                    "alias": "主力机",
                    "os": "macos",
                    "allow_remote_assist": false,
                }),
            );

            // Read UpdateAssistState + ConnectRequest (order: UpdateAssistState then ConnectRequest)
            let upd = read_tls_json(&mut tls);
            assert_eq!(upd["type"], "update_assist_state");
            assert_eq!(upd["allow_remote_assist"], true);

            let req = read_tls_json(&mut tls);
            assert_eq!(req["type"], "connect_request");
            let sid = req["session_id"].as_str().unwrap().to_string();
            write_tls_json(
                &mut tls,
                &serde_json::json!({
                    "type": "punch_coord",
                    "session_id": sid,
                    "role": "client",
                    "peer_id": "777888999",
                    "peer_lan_addrs": [],
                    "relay_token": "tok-owner-01",
                    "relay_addr": "127.0.0.1:21117",
                    "is_owner_connection": true,
                    "owner_grant": {
                        "ticket_id": "0123456789abcdef0123456789abcdef",
                        "user_id": "user-uuid-1",
                        "caller_device_id": "604928235",
                        "caller_pubkey_hex": "a".repeat(64),
                        "target_device_id": "777888999",
                        "session_id": sid,
                        "expires_at_unix_sec": 2_000_000_000u64,
                        "signature_hex": "b".repeat(128),
                    }
                }),
            );

            // Send terminal kick reason "account_disabled"
            write_tls_json(
                &mut tls,
                &serde_json::json!({
                    "type": "kicked",
                    "reason": "account_disabled",
                }),
            );
            let mut sink = [0u8; 8];
            let _ = tls.read(&mut sink);
        });

        let token_calls = Arc::new(AtomicUsize::new(0));
        let token_calls_cb = Arc::clone(&token_calls);
        let cfg = ServerConfig {
            enabled: true,
            server_host: "localhost".to_string(),
            signal_port: signal_addr.port(),
            relay_port: 21117,
            use_https: true,
            ca_cert_pem: Some(server_cert_pem.to_string()),
            ..ServerConfig::default()
        };
        let opts = SignalClientOptions {
            token_provider: Some(Arc::new(move || {
                let n = token_calls_cb.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(format!("tok-round-{n}"))
            })),
            device_pubkey: Some("A".repeat(64)),
            allow_remote_assist: false,
        };
        let fast_timing = SignalTiming {
            heartbeat_interval: Duration::from_secs(30),
            activity_timeout: Duration::from_secs(75),
            stable_threshold: Duration::from_secs(30),
            backoffs: [
                Duration::from_millis(30),
                Duration::from_millis(60),
                Duration::from_millis(120),
                Duration::from_millis(240),
            ],
        };
        let mut client = SignalClientHandle::spawn_with_timing(
            cfg,
            "tls-test".to_string(),
            Vec::new(),
            Some(opts),
            fast_timing,
            |_| {},
        );

        // Wait until reconnect round 2 reaches Connected (token_calls >= 2)
        let deadline = Instant::now() + Duration::from_secs(5);
        while (token_calls.load(Ordering::SeqCst) < 2
            || client.status().state != ServerConnectionState::Connected)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(token_calls.load(Ordering::SeqCst) >= 2);

        // Wait for DeviceStatusChanged
        let dev_ev = {
            let d = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(ev) = client.try_recv_device_status() {
                    break ev;
                }
                assert!(
                    Instant::now() < d,
                    "timed out waiting for DeviceStatusEvent"
                );
                thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(dev_ev.device_id, "777888999");
        assert!(dev_ev.online);
        assert_eq!(dev_ev.alias, "主力机");

        // Send UpdateAssistState + ConnectRequest
        client.set_allow_remote_assist(true).unwrap();
        let grant = client
            .request_connect("777888999", Duration::from_secs(3))
            .expect("must receive ConnectGrant with owner_grant over TLS 1.3");
        assert!(grant.is_owner_connection);
        assert_eq!(
            grant.owner_grant.as_ref().unwrap().ticket_id,
            "0123456789abcdef0123456789abcdef"
        );

        // Collect kicked reasons ("server_shutdown", then "account_disabled")
        let mut kicked = Vec::new();
        let d2 = Instant::now() + Duration::from_secs(3);
        while Instant::now() < d2 {
            while let Some(r) = client.try_recv_kicked_reason() {
                kicked.push(r);
            }
            if kicked.iter().any(|r| r == "account_disabled")
                && client.status().state == ServerConnectionState::Error
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(kicked.contains(&"server_shutdown".to_string()));
        assert!(kicked.contains(&"account_disabled".to_string()));
        assert_eq!(client.status().state, ServerConnectionState::Error);

        client.stop();
        server_thread.join().unwrap();
    }

    #[test]
    fn refuses_plaintext_signal_when_use_https_enabled_without_ca_cert() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accept_jh = thread::spawn(move || {
            let _ = listener.accept();
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: addr.port(),
            relay_port: 21117,
            use_https: true,
            ca_cert_pem: None,
            device_key: crate::server_config::generate_device_key(),
            ..ServerConfig::default()
        };

        let mut client = SignalClientHandle::spawn(cfg, "tls-guard-test".into(), vec![]);
        let deadline = Instant::now() + Duration::from_secs(2);
        let snap = loop {
            let st = client.status();
            if st.state == ServerConnectionState::Error {
                break st;
            }
            assert!(Instant::now() < deadline, "expected TLS-no-CA error state");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(
            snap.error_message
                .as_deref()
                .is_some_and(|m| m.contains("Root CA") || m.contains("拒绝降级")),
            "unexpected error_message: {:?}",
            snap.error_message
        );
        client.stop();
        let _ = accept_jh.join();
    }
}
