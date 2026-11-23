//! UI ↔ 被控端会话桥接层（一体化 All-in-One 模式）。
//!
//! 本模块**严禁引入 `tauri` 类型**，全模块 `#![deny(unsafe_code)]`，
//! 复用 T19 唯一审批 trait [`rc_session::auth::ConnectionApprover`]，
//! 使 `cargo test -p rc-ui` 离线集成测试无需窗口与事件循环。
//!
//! # 设计要点
//! - 启动时在 `HostHandle::spawn` 内即完成首轮 `HostSession` 绑定（默认 `9901` 端口
//!   若被占用自动回退到同 IP 临时端口 `0`），使 [`HostHandle::listen_addr`] 立即可用；
//! - 工作线程单会话串行循环 accept：每轮 accept 分片前重锚 `ClockOrigin::Pending`，
//!   管线工厂按裁定参数构建硬件/假后端并重锚会话钟；
//! - [`UiApprover`] 实现 [`ConnectionApprover`]：代次单调递增防过期裁决串台，
//!   `submit` / `cancel` 直接推送 [`HostUpdate::ApprovalRequested`] /
//!   [`HostUpdate::ApprovalCancelled`]；
//! - [`HostHandle::stop`] 与 `Drop` 自动拒绝在途审批并触发 `HostSession::run` 优雅停机，
//!   回收工作线程不留僵尸线程。

#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_audio::{encode_ima_adpcm, AudioCapture};
use rc_net::quic::QuicEndpoint;
use rc_net::{NetConfig, NetEvent, NetStats, Role, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_session::auth::{
    ApprovalRequest, ApprovalVerdict, ConnectionApprover, OwnerTicketVerifier,
    DEFAULT_AUTH_ITERATIONS,
};
use rc_session::dev_cert::{DEV_CERT_PEM, DEV_KEY_PEM};
use rc_session::proto::{ByeReason, SessionParams};
use rc_session::testkit::RecordingInjector;
use rc_session::{
    AudioFramePacket, AutoApprover, ClipboardBackend, DetachOutcome, HostAuthConfig, HostSession,
    HostSessionConfig, SessionClock, SessionState, StopReason, AUDIO_CODEC_IMA_ADPCM,
    TOOL_CHANNEL_AUDIO_CTRL, TOOL_CHANNEL_DISPLAY_CTRL, TOOL_CHANNEL_FILE, TOOL_CHANNEL_PORTMAP,
    TOOL_CHANNEL_TERMINAL,
};
use rc_tools::{FileHostEngine, PortMapHostEngine, TerminalHostEngine};
use rc_types::Resolution;

use crate::display_ctrl::{self, DisplayControlMsg, DisplayPreset};
use crate::lan_discovery;
use crate::p2p::{self, P2pCandidates};
pub use crate::relay_transport::SessionMtlsBundle;
use crate::relay_transport::{
    E2eeRelayTransport, RelayE2eeMode, RelayRole, RelayShutdownHandle, TcpRelayTransport,
};
use crate::signal_client::RelayIncoming;

/// 默认被控端监听端口（被占用时回退临时端口 `0`）。
pub const DEFAULT_HOST_PORT: u16 = 9901;

/// 被控端更新通道容量上限（有界非阻塞，满载丢最旧）。
pub const HOST_UPDATE_CHANNEL_CAP: usize = 64;

/// 入站中继请求有界队列容量（有界非阻塞，满载丢最旧，符合铁律 1）。
const RELAY_INCOMING_QUEUE_CAP: usize = 8;

/// 入站中继请求的存活有效期（超时未消费的过期令牌自动丢弃）。
const RELAY_INCOMING_TTL: Duration = Duration::from_secs(20);

/// 临时验证码长度（8 位无歧义大写字母数字，排除 `0`/`O`/`1`/`I`）。
pub const TEMP_CODE_LEN: usize = 8;

/// 临时验证码字符表（32 个字符 = 2^5，按低 5 位索引零模偏差）。
const TEMP_CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// 单次 `accept_and_handshake` 分片超时（兼顾 QUIC 握手预算、中继入站响应速度与 `stop()` 回收）。
const HOST_ACCEPT_SLICE: Duration = Duration::from_millis(150);

/// 中继传输层建连与 `RCR1` 握手超时。
const RELAY_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// fake 编码器的单帧编码成本（微秒，对齐 `rc-cli/src/cmd/host.rs`）。
const FAKE_ENCODE_COST_US: u32 = 500;

/// 单拍每个工具引擎最多发送的线帧数量（避免工具吞吐饿死视频与控制面）。
const MAX_TOOL_FRAMES_PER_TICK: usize = 8;

/// Host 侧专用文件传输工作线程的出入站有界通道容量（H12）。
const FILE_HOST_WORKER_CHAN_CAP: usize = 64;

/// 单拍最多泵送的音频帧数量（20ms/帧，4 帧 = 80ms 突发上限）。
const MAX_AUDIO_FRAMES_PER_TICK: usize = 4;

/// 生成 8 位无歧义大写字母数字临时验证码（使用 OS 熵源，排除 `0`/`O`/`1`/`I`）。
pub fn generate_temp_code() -> String {
    let mut bytes = [0u8; TEMP_CODE_LEN];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(TEMP_CODE_LEN);
    for b in bytes {
        out.push(TEMP_CODE_ALPHABET[usize::from(b & 0x1F)] as char);
    }
    out
}

/// UI 被控端启动配置。
#[derive(Clone)]
pub struct UiHostConfig {
    /// 期望监听地址（若端口为 [`DEFAULT_HOST_PORT`] 且被占用，自动回退到同 IP 临时端口 `0`）。
    pub listen_addr: SocketAddr,
    /// 访问密码（`None` = 无密码；`Some` = 启用 PBKDF2 挑战应答）。
    pub password: Option<String>,
    /// 机主人工审批超时（微秒）。
    pub approval_timeout_us: u64,
    /// 优先硬件采集/编码/注入（macOS: SCK + VT + CGEvent），不可用时优雅降级假后端。
    pub prefer_hardware: bool,
    /// 是否需要机主审批弹窗（`false` 时使用 [`AutoApprover`] 自动通过）。
    pub require_approval: bool,
    /// 密码校验通过时是否自动放行而不弹出机主审批弹窗（无密码连接仍需机主审批）。
    pub auto_accept_with_password: bool,
    /// 会话结束后是否自动轮换临时验证码（使用临时验证码时为 `true`，自定义密码为 `false`）。
    pub auto_rotate_password: bool,
    /// 是否启用双向剪贴板同步（仅 `prefer_hardware == true` 时挂接本机真实系统剪贴板）。
    pub clipboard_enabled: bool,
    /// 是否启用系统音频采集与编码串流（仅 `prefer_hardware == true` 时采集真实系统音频）。
    pub audio_enabled: bool,
    /// 是否允许非同账号他人的远程协助（`false` 时仅允许已登录同账号凭 `OwnerTicket` PoP 自控）。
    pub allow_remote_assist: bool,
    /// 同账号本人自控 `OwnerTicket` 公钥验签器（无论 `allow_remote_assist` 均允许本人免密免审批直通）。
    pub owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    /// 双向 mTLS 证书与身份校验束（用于外层 `:21117` 中转、P2P mTLS、内层 E2EE 及可选 LAN mTLS）。
    pub mtls_config: Option<SessionMtlsBundle>,
    /// LAN 直连监听器是否强制启用 mTLS（需配合 `mtls_config: Some(...)`）。
    pub accept_mtls_lan: bool,
}

impl Default for UiHostConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::from(([0, 0, 0, 0], DEFAULT_HOST_PORT)),
            password: None,
            approval_timeout_us: rc_session::auth::DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware: false,
            require_approval: false,
            auto_accept_with_password: false,
            auto_rotate_password: false,
            clipboard_enabled: true,
            audio_enabled: true,
            allow_remote_assist: true,
            owner_verifier: None,
            mtls_config: None,
            accept_mtls_lan: false,
        }
    }
}

impl fmt::Debug for UiHostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiHostConfig")
            .field("listen_addr", &self.listen_addr)
            .field("has_password", &self.password.is_some())
            .field("approval_timeout_us", &self.approval_timeout_us)
            .field("prefer_hardware", &self.prefer_hardware)
            .field("require_approval", &self.require_approval)
            .field("auto_accept_with_password", &self.auto_accept_with_password)
            .field("auto_rotate_password", &self.auto_rotate_password)
            .field("clipboard_enabled", &self.clipboard_enabled)
            .field("audio_enabled", &self.audio_enabled)
            .field("allow_remote_assist", &self.allow_remote_assist)
            .field("has_owner_verifier", &self.owner_verifier.is_some())
            .field("has_mtls_config", &self.mtls_config.is_some())
            .field("accept_mtls_lan", &self.accept_mtls_lan)
            .finish()
    }
}

/// 推送至 UI 的连接审批请求载荷。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HostApprovalPrompt {
    /// 单调递增审批代次（前端回传时用于防过期弹窗裁决串台）。
    pub generation: u64,
    /// 控制端 `Hello.session_id`。
    pub session_id: u64,
    /// 控制端传输层来源地址。
    pub remote_addr: String,
    /// 控制端请求的分辨率（如 `"1920x1080"`）。
    pub requested_res: String,
    /// 控制端请求的帧率（fps）。
    pub requested_fps: u32,
    /// 密码校验是否已通过（未启用密码时为 `false`）。
    pub password_verified: bool,
}

/// 被控端工作线程 → UI 的状态更新（经 Tauri event 转发到前端）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostUpdate {
    /// 已开始监听，等待控制端连接。
    Listening {
        /// 实际绑定的监听地址。
        listen_addr: String,
        /// 供本机展示给对端的连接地址（`0.0.0.0` 会替换为探测到的本机局域网 IPv4）。
        display_addr: String,
        /// 后端模式标签。
        backend: String,
    },
    /// 收到新的连接审批请求。
    ApprovalRequested(HostApprovalPrompt),
    /// 在途连接审批已被取消（对端断开或超时）。
    ApprovalCancelled {
        /// 被取消的审批代次。
        generation: u64,
    },
    /// 控制端已通过鉴权与审批，进入串流。
    ClientConnected {
        /// 控制端来源地址。
        remote_addr: String,
    },
    /// 本人同账号自控会话已通过 `OwnerTicket` PoP 免密直通。
    OwnerConnected {
        /// 发起自控的对端设备码。
        caller_device_id: String,
        /// 控制端来源地址或通道标签。
        remote_addr: String,
    },
    /// 控制端已断开连接（被控端回到等待状态）。
    ClientDisconnected {
        /// 人类可读的断开原因。
        reason: String,
    },
    /// 会话结束后工作线程已原子轮换临时验证码（零窗口防止下一次连入复用旧码）。
    PasswordRotated {
        /// 新生效的 8 位临时验证码。
        password: String,
    },
    /// 被控端服务已停止。
    Stopped,
    /// 被控端运行出错。
    Error {
        /// 错误描述。
        message: String,
    },
}

// ─────────────────────────────────────────────────────────────────
// 有界更新队列
// ─────────────────────────────────────────────────────────────────

struct BoundedHostUpdateQueue {
    cap: usize,
    inner: Mutex<VecDeque<HostUpdate>>,
}

impl BoundedHostUpdateQueue {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    fn push(&self, item: HostUpdate) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back(item);
    }

    fn try_pop(&self) -> Option<HostUpdate> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }
}

/// 入站中继请求有界队列（满载丢最旧，符合铁律 1，且按 TTL 自动过滤过期令牌）。
struct BoundedRelayIncomingQueue {
    cap: usize,
    inner: Mutex<VecDeque<(Instant, RelayIncoming)>>,
}

impl BoundedRelayIncomingQueue {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    fn push(&self, item: RelayIncoming) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back((Instant::now(), item));
    }

    fn pop_ready(&self, ttl: Duration, lan_grace: Duration) -> Option<RelayIncoming> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut deferred = VecDeque::new();
        let mut picked = None;
        while let Some((enqueued_at, item)) = q.pop_front() {
            let elapsed = enqueued_at.elapsed();
            if elapsed > ttl {
                continue;
            }
            if !item.peer_lan_addrs.is_empty() && elapsed < lan_grace {
                deferred.push_back((enqueued_at, item));
                continue;
            }
            picked = Some(item);
            break;
        }
        while let Some(front) = deferred.pop_back() {
            q.push_front(front);
        }
        picked
    }

    fn purge_expired(&self, ttl: Duration) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.retain(|(enqueued_at, _)| enqueued_at.elapsed() <= ttl);
    }

    fn purge_for_peer(&self, peer_device_id: Option<&str>, ttl: Duration) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.retain(|(enqueued_at, item)| {
            enqueued_at.elapsed() <= ttl && peer_device_id != Some(item.peer_id.as_str())
        });
    }
}

struct PasswordState {
    password: Option<String>,
    auto_rotate: bool,
}

#[derive(Clone)]
struct AuthRuntimeState {
    allow_remote_assist: bool,
    owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    owner_replay_cache: Arc<Mutex<rc_session::OwnerReplayCache>>,
    mtls_config: Option<SessionMtlsBundle>,
    accept_mtls_lan: bool,
}

fn emit_peer_connected(host: &HostSession, remote_addr: &str, updates: &BoundedHostUpdateQueue) {
    if let Some(owner) = host.authenticated_owner() {
        updates.push(HostUpdate::OwnerConnected {
            caller_device_id: owner.caller_device_id.clone(),
            remote_addr: remote_addr.to_string(),
        });
    } else {
        updates.push(HostUpdate::ClientConnected {
            remote_addr: remote_addr.to_string(),
        });
    }
}

#[derive(Clone)]
enum ActiveRelayInterrupt {
    Dialing(Arc<AtomicBool>),
    Connected(RelayShutdownHandle),
}

impl ActiveRelayInterrupt {
    fn interrupt(&self) {
        match self {
            Self::Dialing(cancel) => cancel.store(true, Ordering::Release),
            Self::Connected(handle) => handle.shutdown(),
        }
    }
}

struct RelayInterruptSlotGuard<'a> {
    slot: &'a Mutex<Option<ActiveRelayInterrupt>>,
}

impl Drop for RelayInterruptSlotGuard<'_> {
    fn drop(&mut self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// ─────────────────────────────────────────────────────────────────
// UiApprover：实现 T19 唯一 ConnectionApprover trait
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ApproverState {
    in_flight: bool,
    /// 当前在途审批是否实际向 UI 推送了 [`HostUpdate::ApprovalRequested`] 弹窗（避免自动放行后误推取消事件）。
    prompted: bool,
    generation: u64,
    verdict: Option<ApprovalVerdict>,
    /// 粘性停机标记（U-M2）：一旦置位，后续 `submit` / `poll_verdict` 立刻返回拒绝，不再弹窗。
    stopped: bool,
    /// 可选来源地址覆盖（中继会话无直连 peer SocketAddr，用于向机主展示对端设备码）。
    remote_addr_override: Option<String>,
}

/// UI 侧连接审批器（实现 [`ConnectionApprover`]，全方法非阻塞）。
///
/// 当 `auto_accept_with_password == true` 且 `req.password_verified == true` 时，
/// [`ConnectionApprover::submit`] 直接写入 [`ApprovalVerdict::Accepted`] 而不推送
/// [`HostUpdate::ApprovalRequested`]；其余场景（如无密码连接）递增单调代次 `generation`
/// 并发射 [`HostUpdate::ApprovalRequested`]；[`Self::resolve`] 仅当仍在途且代次精确匹配时
/// 生效；[`ConnectionApprover::cancel`] 清除在途状态并在已弹窗时发射
/// [`HostUpdate::ApprovalCancelled`]。
#[derive(Clone)]
pub struct UiApprover {
    state: Arc<Mutex<ApproverState>>,
    updates: Arc<BoundedHostUpdateQueue>,
    auto_accept_with_password: bool,
}

impl UiApprover {
    fn new(updates: Arc<BoundedHostUpdateQueue>, auto_accept_with_password: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(ApproverState::default())),
            updates,
            auto_accept_with_password,
        }
    }

    /// 设置或清除中继会话握手期的来源地址覆盖文案。
    fn set_remote_addr_override(&self, label: Option<String>) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.remote_addr_override = label;
    }

    /// 按代次回写机主审批裁决；仅当未停机、在途且 `generation` 精确匹配时返回 `true`。
    pub fn resolve(&self, generation: u64, accepted: bool) -> bool {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.stopped && guard.in_flight && guard.generation == generation {
            guard.verdict = Some(if accepted {
                ApprovalVerdict::Accepted
            } else {
                ApprovalVerdict::Rejected
            });
            true
        } else {
            false
        }
    }

    /// 停机时置位粘性 `stopped` 标记，并立即唤醒阻塞在审批阶段的 `HostSession::handshake`（U-M2）。
    fn mark_stopped(&self) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.stopped = true;
        if guard.in_flight && guard.verdict.is_none() {
            guard.verdict = Some(ApprovalVerdict::Rejected);
        }
    }
}

impl ConnectionApprover for UiApprover {
    fn submit(&mut self, req: &ApprovalRequest) {
        let prompt = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.stopped {
                guard.in_flight = false;
                guard.prompted = false;
                guard.verdict = None;
                return;
            }
            guard.generation = guard.generation.wrapping_add(1);
            guard.in_flight = true;
            if self.auto_accept_with_password && req.password_verified {
                guard.prompted = false;
                guard.verdict = Some(ApprovalVerdict::Accepted);
                None
            } else {
                guard.prompted = true;
                guard.verdict = None;
                let remote_addr = guard
                    .remote_addr_override
                    .clone()
                    .unwrap_or_else(|| req.remote_addr.to_string());
                Some(HostApprovalPrompt {
                    generation: guard.generation,
                    session_id: req.session_id,
                    remote_addr,
                    requested_res: format!("{}x{}", req.requested.width, req.requested.height),
                    requested_fps: req.requested.fps,
                    password_verified: req.password_verified,
                })
            }
        };
        if let Some(prompt) = prompt {
            self.updates.push(HostUpdate::ApprovalRequested(prompt));
        }
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.stopped {
            guard.in_flight = false;
            guard.prompted = false;
            guard.verdict = None;
            return Some(ApprovalVerdict::Rejected);
        }
        let verdict = guard.verdict.take()?;
        guard.in_flight = false;
        guard.prompted = false;
        Some(verdict)
    }

    fn cancel(&mut self) {
        let cancelled_gen = {
            let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.in_flight {
                guard.in_flight = false;
                guard.verdict = None;
                let was_prompted = std::mem::replace(&mut guard.prompted, false);
                if was_prompted {
                    Some(guard.generation)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(generation) = cancelled_gen {
            self.updates
                .push(HostUpdate::ApprovalCancelled { generation });
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 会话钟原点与后端管线构造（对齐 rc-cli/src/cmd/host.rs）
// ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum ClockOrigin {
    /// 握手/审批阶段：以每次 accept 分片前的墙钟推进，保证审批超时准确触发。
    Pending(Instant),
    /// 假后端：与 `WallCapture` 共用单次采样锚点。
    Fake(Instant),
    /// 真后端：锚定主机钟 µs（与 SCK PTS 同域同零点）。
    HostClock(u64),
}

fn make_session_clock(origin: Arc<Mutex<ClockOrigin>>) -> SessionClock {
    Arc::new(move || {
        let guard = origin.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            ClockOrigin::Pending(i) | ClockOrigin::Fake(i) => {
                u32::try_from(i.elapsed().as_micros()).unwrap_or(u32::MAX)
            }
            ClockOrigin::HostClock(o) => {
                u32::try_from(rc_pipeline::host_clock_us().unwrap_or(o).saturating_sub(o))
                    .unwrap_or(u32::MAX)
            }
        }
    })
}

struct BuiltUiHost {
    host: HostSession,
    clock: SessionClock,
    clock_origin: Arc<Mutex<ClockOrigin>>,
    backend_label: Arc<Mutex<String>>,
    hardware_video_active: Arc<AtomicBool>,
}

/// 查询硬件主显示器逻辑分辨率（仅 `prefer_hardware == true` 的 macOS 生效）。
fn host_hardware_display_caps(prefer_hardware: bool) -> Option<Resolution> {
    if !prefer_hardware {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        rc_capture_macos::main_display_logical_resolution()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 查询硬件主显示器物理像素分辨率（backing pixels；仅 `prefer_hardware == true` 的 macOS 生效）。
fn host_hardware_display_backing(prefer_hardware: bool) -> Option<Resolution> {
    if !prefer_hardware {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        rc_capture_macos::main_display_backing_resolution()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn default_host_session_config(
    listen_addr: SocketAddr,
    password: Option<String>,
    approval_timeout_us: u64,
    prefer_hardware: bool,
    allow_remote_assist: bool,
    owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    owner_replay_cache: Arc<Mutex<rc_session::OwnerReplayCache>>,
    approver: Box<dyn ConnectionApprover>,
) -> (HostSessionConfig, bool) {
    let injector: Box<dyn rc_input::InputInjector> = build_injector(prefer_hardware);

    let hw_display_res = host_hardware_display_caps(prefer_hardware);
    let (cap_w, cap_h, preserve_aspect) = match hw_display_res {
        Some(res) => (res.width, res.height, true),
        None => (3840, 2160, false),
    };
    let caps = SessionParams {
        width: cap_w,
        height: cap_h,
        fps: 144,
        bitrate_bps: 40_000_000,
    };
    let pipeline = PipelineConfig {
        resolution: hw_display_res.unwrap_or_else(|| Resolution::new(1920, 1080)),
        fps: 144,
        bitrate_bps: 20_000_000,
        prefer_hardware,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let cfg = HostSessionConfig {
        listen_addr,
        mtu: rc_session::DEFAULT_SESSION_MTU,
        caps,
        pipeline,
        handshake_timeout: Duration::from_secs(10),
        injector,
        pace_capture: true,
        auth: HostAuthConfig {
            password,
            iterations: DEFAULT_AUTH_ITERATIONS,
            approval_timeout_us,
            allow_remote_assist,
            owner_verifier,
            replay_cache: Some(owner_replay_cache),
            ..HostAuthConfig::default()
        },
        approver,
    };
    (cfg, preserve_aspect)
}

type HostPipelineFactory =
    Box<dyn FnMut(&PipelineConfig) -> rc_types::Result<rc_pipeline::HostPipeline> + Send>;

fn make_host_pipeline_factory(
    prefer_hardware: bool,
    clock_origin: Arc<Mutex<ClockOrigin>>,
    backend_label: Arc<Mutex<String>>,
    hardware_video_active: Arc<AtomicBool>,
    updates: Arc<BoundedHostUpdateQueue>,
) -> HostPipelineFactory {
    Box::new(
        move |pcfg: &PipelineConfig| -> rc_types::Result<rc_pipeline::HostPipeline> {
            // 若会话已锚定过时钟零点（例如热切换分辨率/帧率重建管线），复用同一零点，
            // 保证会话钟与采集 PTS 严格单调连续、不触发时钟倒退或饱和刷新误判。
            let existing_origin = clock_origin
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if prefer_hardware {
                let mut hw_cfg = pcfg.clone();
                hw_cfg.prefer_hardware = true;
                // 高分辨率（> ~2.6M 像素，如 3K/4K Retina 3024×1964）在 Apple Silicon VideoToolbox
                // RealTime 模式下超过 60fps 会触发硬件过载丢帧；自动将编码器期望帧率收敛到 ≤60fps，
                // 1080p/1512×982/2K 等常规档位仍保持 120–144fps 满帧穿透。
                let pixels =
                    u64::from(hw_cfg.resolution.width) * u64::from(hw_cfg.resolution.height);
                if pixels > 2_600_000 && hw_cfg.fps > 60 {
                    hw_cfg.fps = 60;
                }
                let existing_host_us = match existing_origin {
                    ClockOrigin::HostClock(o) => Some(o),
                    _ => None,
                };
                let first_try = real_host_stack(&hw_cfg, existing_host_us);
                let final_try = match first_try {
                    Ok(ok) => Ok(ok),
                    Err(_) if hw_cfg.fps > 60 => {
                        let mut fallback_60 = hw_cfg.clone();
                        fallback_60.fps = 60;
                        real_host_stack(&fallback_60, existing_host_us)
                    }
                    Err(err) => Err(err),
                };
                match final_try {
                    Ok((p, l, origin_us)) => {
                        *backend_label.lock().unwrap_or_else(|e| e.into_inner()) = l;
                        *clock_origin.lock().unwrap_or_else(|e| e.into_inner()) =
                            ClockOrigin::HostClock(origin_us);
                        hardware_video_active.store(true, Ordering::Release);
                        return Ok(p);
                    }
                    Err(err) => {
                        if hardware_video_active.load(Ordering::Acquire) {
                            // 活跃硬件会话热切换失败时绝不降级到 FakeEncoder（否则对端 VtDecoder
                            // 持续收到非 H.264 伪帧导致黑屏），直接上报错误。
                            let message = format!("硬件分辨率/帧率热切换失败：{err}");
                            eprintln!("[rc-ui-host] {message}");
                            updates.push(HostUpdate::Error { message });
                            return Err(err);
                        }
                        // 注意：在 macOS 上若 SCK / VideoToolbox 初始化失败（常见原因：缺少「屏幕录制」
                        // 权限或授权后尚未重启进程），降级为 FakeEncoder 会发送非 H.264/HEVC 伪帧，
                        // 对端若启用硬件 VtDecoder 将无法解出真实画面（表现为黑屏）。
                        // 因此绝不静默吞错：同时输出 stderr 诊断并向 UI 推送 `HostUpdate::Error`。
                        #[cfg(target_os = "macos")]
                        let message = format!(
                            "硬件屏幕采集/编码初始化失败（请确认已授予屏幕录制权限并在授权后重启应用），已降级为测试后端：{err}"
                        );
                        #[cfg(not(target_os = "macos"))]
                        let message = format!("硬件采集/编码不可用，已降级为测试后端：{err}");
                        eprintln!("[rc-ui-host] {message}");
                        updates.push(HostUpdate::Error { message });

                        hardware_video_active.store(false, Ordering::Release);
                        *backend_label.lock().unwrap_or_else(|e| e.into_inner()) =
                            "fake(降级)".to_string();
                        let existing_fake = match existing_origin {
                            ClockOrigin::Fake(a) => Some(a),
                            _ => None,
                        };
                        let (p, anchor) = fake_stack_anchored(pcfg, existing_fake)?;
                        *clock_origin.lock().unwrap_or_else(|e| e.into_inner()) =
                            ClockOrigin::Fake(anchor);
                        return Ok(p);
                    }
                }
            }
            hardware_video_active.store(false, Ordering::Release);
            *backend_label.lock().unwrap_or_else(|e| e.into_inner()) = "fake".to_string();
            let existing_fake = match existing_origin {
                ClockOrigin::Fake(a) => Some(a),
                _ => None,
            };
            let (p, anchor) = fake_stack_anchored(pcfg, existing_fake)?;
            *clock_origin.lock().unwrap_or_else(|e| e.into_inner()) = ClockOrigin::Fake(anchor);
            Ok(p)
        },
    )
}

/// 按平台与硬件偏好构造剪贴板后端：仅 `prefer_hardware == true` 时挂接本机真实剪贴板，
/// 离线假后端测试绝不触碰开发者真实系统剪贴板。
fn create_clipboard_backend(prefer_hardware: bool) -> Option<Box<dyn ClipboardBackend>> {
    if !prefer_hardware {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        rc_session::NspasteboardBackend::new()
            .ok()
            .map(|b| Box::new(b) as Box<dyn ClipboardBackend>)
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 握手期可中断的传输层包装：在 `streaming_started == false` 期间响应 `stop_flag` / `disconnect_flag`，
/// 使 P2P QUIC 与 TCP 中继会话在握手/审批阶段均可在数毫秒内中止。
struct CancellableHostTransport {
    inner: Box<dyn Transport>,
    stop_flag: Arc<AtomicBool>,
    disconnect_flag: Arc<AtomicBool>,
    streaming_started: Arc<AtomicBool>,
}

impl Transport for CancellableHostTransport {
    fn send_datagram(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.inner.send_datagram(data)
    }

    fn send_reliable(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.inner.send_reliable(data)
    }

    fn send_reliable_urgent(&mut self, data: &[u8]) -> rc_types::Result<()> {
        self.inner.send_reliable_urgent(data)
    }

    fn poll(&mut self, timeout: Duration) -> rc_types::Result<Vec<NetEvent>> {
        if !self.streaming_started.load(Ordering::Acquire)
            && (self.stop_flag.load(Ordering::Acquire)
                || self.disconnect_flag.load(Ordering::Acquire))
        {
            return Ok(vec![NetEvent::Disconnected {
                reason: "本机主动断开".to_string(),
            }]);
        }
        self.inner.poll(timeout)
    }

    fn stats(&self) -> NetStats {
        self.inner.stats()
    }
}

/// 构造一轮直连 QUIC `HostSession`（优先硬件注入/采集/编码，无权限或非 macOS 自动降级假栈）。
#[allow(clippy::too_many_arguments)]
fn build_host_session(
    listen_addr: SocketAddr,
    password: Option<String>,
    approval_timeout_us: u64,
    prefer_hardware: bool,
    clipboard_enabled: bool,
    allow_remote_assist: bool,
    owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    owner_replay_cache: Arc<Mutex<rc_session::OwnerReplayCache>>,
    approver: Box<dyn ConnectionApprover>,
    updates: Arc<BoundedHostUpdateQueue>,
) -> Result<BuiltUiHost, String> {
    let (host_cfg, preserve_aspect) = default_host_session_config(
        listen_addr,
        password,
        approval_timeout_us,
        prefer_hardware,
        allow_remote_assist,
        owner_verifier,
        owner_replay_cache,
        approver,
    );

    let clock_origin: Arc<Mutex<ClockOrigin>> =
        Arc::new(Mutex::new(ClockOrigin::Pending(Instant::now())));
    let clock = make_session_clock(Arc::clone(&clock_origin));
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(if prefer_hardware {
        "auto".to_string()
    } else {
        "fake".to_string()
    }));
    let hardware_video_active = Arc::new(AtomicBool::new(false));

    let factory = make_host_pipeline_factory(
        prefer_hardware,
        Arc::clone(&clock_origin),
        Arc::clone(&backend_label),
        Arc::clone(&hardware_video_active),
        updates,
    );

    let mut host = HostSession::new(host_cfg, DEV_CERT_PEM, DEV_KEY_PEM, Arc::clone(&clock))
        .map_err(|e| format!("host 会话构造失败: {e}"))?;
    host.set_preserve_caps_aspect(preserve_aspect);
    host.set_pipeline_factory_for(factory);
    if clipboard_enabled {
        if let Some(cb) = create_clipboard_backend(prefer_hardware) {
            host.enable_clipboard(cb);
        }
    }

    Ok(BuiltUiHost {
        host,
        clock,
        clock_origin,
        backend_label,
        hardware_video_active,
    })
}

/// 构造一轮基于已建立传输层（LAN mTLS、P2P QUIC 或 TCP `RCR1` 中继）的 `HostSession`。
#[allow(clippy::too_many_arguments)]
fn build_transport_host_session(
    listen_addr: SocketAddr,
    transport: Box<dyn Transport>,
    password: Option<String>,
    approval_timeout_us: u64,
    prefer_hardware: bool,
    clipboard_enabled: bool,
    allow_remote_assist: bool,
    owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    owner_replay_cache: Arc<Mutex<rc_session::OwnerReplayCache>>,
    approver: Box<dyn ConnectionApprover>,
    updates: Arc<BoundedHostUpdateQueue>,
) -> Result<BuiltUiHost, String> {
    let (host_cfg, preserve_aspect) = default_host_session_config(
        listen_addr,
        password,
        approval_timeout_us,
        prefer_hardware,
        allow_remote_assist,
        owner_verifier,
        owner_replay_cache,
        approver,
    );

    let clock_origin: Arc<Mutex<ClockOrigin>> =
        Arc::new(Mutex::new(ClockOrigin::Pending(Instant::now())));
    let clock = make_session_clock(Arc::clone(&clock_origin));
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(if prefer_hardware {
        "auto".to_string()
    } else {
        "fake".to_string()
    }));
    let hardware_video_active = Arc::new(AtomicBool::new(false));

    // 占位后端仅用于满足 `HostSession::with_parts` 签名；握手成功后优先执行
    // `set_pipeline_factory_for`，按裁定参数构建硬件/假栈并锚定会话钟零点。
    let placeholder_res = Resolution::new(64, 64);
    let dummy_capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
        display: rc_capture::DisplayInfo {
            id: rc_types::DisplayId(0),
            resolution: placeholder_res,
            scale: 1.0,
            hdr: false,
        },
        fps: 30,
        content: rc_capture::FakeContent::Static,
        dirty_rect: None,
    })
    .map_err(|e| format!("会话占位采集构造失败: {e}"))?;
    let dummy_encoder = rc_encode::FakeEncoder::new(rc_encode::EncoderConfig {
        resolution: placeholder_res,
        fps: 30,
        bitrate_bps: 2_000_000,
        ..rc_encode::EncoderConfig::default()
    })
    .map_err(|e| format!("会话占位编码构造失败: {e}"))?;

    let mut host = HostSession::with_parts(
        host_cfg,
        transport,
        Box::new(dummy_capture),
        Box::new(dummy_encoder),
        Arc::clone(&clock),
    )
    .map_err(|e| format!("信令 host 会话构造失败: {e}"))?;
    host.set_preserve_caps_aspect(preserve_aspect);

    let factory = make_host_pipeline_factory(
        prefer_hardware,
        Arc::clone(&clock_origin),
        Arc::clone(&backend_label),
        Arc::clone(&hardware_video_active),
        updates,
    );
    host.set_pipeline_factory_for(factory);
    if clipboard_enabled {
        if let Some(cb) = create_clipboard_backend(prefer_hardware) {
            host.enable_clipboard(cb);
        }
    }

    Ok(BuiltUiHost {
        host,
        clock,
        clock_origin,
        backend_label,
        hardware_video_active,
    })
}

fn build_injector(prefer_hardware: bool) -> Box<dyn rc_input::InputInjector> {
    if prefer_hardware {
        #[cfg(target_os = "macos")]
        {
            // 与 SckConfig { display: 0 } 主屏采集严格对齐，避免外接副屏引入坐标偏移。
            if let Ok(bridge) = rc_input_macos::InputBridge::for_main_display() {
                return Box::new(bridge);
            }
        }
    }
    Box::new(RecordingInjector::new())
}

fn real_host_stack(
    hw_cfg: &PipelineConfig,
    existing_origin_us: Option<u64>,
) -> rc_types::Result<(rc_pipeline::HostPipeline, String, u64)> {
    let (encoder, el) = rc_pipeline::select_encoder_labeled(hw_cfg)?;
    let origin_us = match existing_origin_us {
        Some(o) => o,
        None => rc_pipeline::host_clock_us().ok_or_else(|| {
            rc_types::Error::InvalidConfig("平台无主机钟（real 仅 macOS）".into())
        })?,
    };
    // 顺序红线：先锚定原点再建 SCK 流，保证 SCK PTS 与会话钟同域同零点。
    let mut pcfg = hw_cfg.clone();
    pcfg.clock_origin_us = Some(origin_us);
    let (capture, cl) = rc_pipeline::select_capture_labeled(&pcfg)?;
    if el != "videotoolbox" || cl != "sck" {
        return Err(rc_types::Error::InvalidConfig(format!(
            "real 后端要求 sck+videotoolbox，实际 {cl}+{el}"
        )));
    }
    let pipeline = rc_pipeline::HostPipeline::with_backends_paced(pcfg, capture, encoder, false)?;
    Ok((pipeline, format!("{cl}+{el}"), origin_us))
}

fn fake_stack_anchored(
    pcfg: &PipelineConfig,
    existing_anchor: Option<Instant>,
) -> rc_types::Result<(rc_pipeline::HostPipeline, Instant)> {
    let anchor = existing_anchor.unwrap_or_else(Instant::now);
    let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
        display: rc_capture::DisplayInfo {
            id: rc_types::DisplayId(0),
            resolution: pcfg.resolution,
            scale: 1.0,
            hdr: false,
        },
        fps: pcfg.fps,
        content: rc_capture::FakeContent::Dynamic,
        dirty_rect: None,
    })?;
    let pixels = pcfg.resolution.width as usize * pcfg.resolution.height as usize;
    const SINGLE_PACKET_PAYLOAD: usize = 1_088;
    let output_bytes = (pixels / 8).clamp(128, SINGLE_PACKET_PAYLOAD);
    let encoder = rc_encode::FakeEncoder::with_tuning(
        rc_encode::EncoderConfig {
            resolution: pcfg.resolution,
            fps: pcfg.fps,
            bitrate_bps: pcfg.bitrate_bps,
            ..rc_encode::EncoderConfig::default()
        },
        FAKE_ENCODE_COST_US,
        output_bytes,
    )?;
    let pipeline = rc_pipeline::HostPipeline::with_backends_paced(
        pcfg.clone(),
        Box::new(WallCapture {
            inner: Box::new(capture),
            anchor,
        }),
        Box::new(encoder),
        true,
    )?;
    Ok((pipeline, anchor))
}

struct WallCapture {
    inner: Box<dyn rc_capture::CaptureBackend>,
    anchor: Instant,
}

impl rc_capture::CaptureBackend for WallCapture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> rc_types::Result<Option<rc_capture::CapturedFrame>> {
        match self.inner.next_frame(timeout)? {
            Some(mut frame) => {
                frame.timestamp = rc_types::Timestamp(
                    u32::try_from(self.anchor.elapsed().as_micros()).unwrap_or(u32::MAX),
                );
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }

    fn poll_invalidation(&mut self) -> Option<rc_capture::InvalidationReason> {
        self.inner.poll_invalidation()
    }

    fn rebuild(&mut self) -> rc_types::Result<()> {
        self.inner.rebuild()
    }

    fn display_info(&self) -> rc_capture::DisplayInfo {
        self.inner.display_info()
    }
}

fn format_display_addr(actual: SocketAddr) -> String {
    if actual.ip().is_unspecified() {
        let ip = lan_discovery::detect_local_lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
        format!("{ip}:{}", actual.port())
    } else {
        actual.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────
// HostHandle（公开 API）
// ─────────────────────────────────────────────────────────────────

/// UI 侧被控端服务句柄：管理单会话循环工作线程、审批桥与优雅停机。
pub struct HostHandle {
    listen_addr: SocketAddr,
    updates: Arc<BoundedHostUpdateQueue>,
    approver: UiApprover,
    password_state: Arc<Mutex<PasswordState>>,
    password_version: Arc<AtomicU64>,
    auth_state: Arc<Mutex<AuthRuntimeState>>,
    config_version: Arc<AtomicU64>,
    relay_incoming: Arc<BoundedRelayIncomingQueue>,
    active_relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>>,
    disconnect_flag: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    lock_screen_count: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

enum HostListener {
    Plain(Box<BuiltUiHost>),
    MtlsLan { endpoint: QuicEndpoint },
    AwaitingMtlsCert,
}

fn bind_mtls_lan_endpoint(
    listen_addr: SocketAddr,
    bundle: &SessionMtlsBundle,
) -> Result<QuicEndpoint, String> {
    // LAN 直连监听端接受任意同 CA 设备（客户端证书 SAN 验证其身份）；expected_peer_device_id 置 None。
    let mut server_bundle = bundle.clone();
    server_bundle.expected_peer_device_id = None;
    server_bundle.expected_peer_user_id = None;
    let mtls_cfg = server_bundle.to_quic_mtls_config()?;
    QuicEndpoint::new_mtls(&NetConfig::new(Role::Host, listen_addr), &mtls_cfg)
        .map_err(|e| format!("mTLS LAN 监听器绑定失败: {e}"))
}

impl HostHandle {
    /// 启动被控端服务：先同步绑定 QUIC 监听端口（默认 `9901` 冲突时自动回退临时端口），
    /// 随后派发后台单会话循环线程。
    pub fn spawn(cfg: UiHostConfig) -> Result<Self, String> {
        let updates = Arc::new(BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP));
        let approver = UiApprover::new(Arc::clone(&updates), cfg.auto_accept_with_password);
        let password_state = Arc::new(Mutex::new(PasswordState {
            password: cfg.password.clone(),
            auto_rotate: cfg.auto_rotate_password,
        }));
        let password_version = Arc::new(AtomicU64::new(0));
        let shared_owner_replay = Arc::new(Mutex::new(rc_session::OwnerReplayCache::new()));
        let auth_state = Arc::new(Mutex::new(AuthRuntimeState {
            allow_remote_assist: cfg.allow_remote_assist,
            owner_verifier: cfg.owner_verifier.clone(),
            owner_replay_cache: Arc::clone(&shared_owner_replay),
            mtls_config: cfg.mtls_config.clone(),
            accept_mtls_lan: cfg.accept_mtls_lan,
        }));
        let config_version = Arc::new(AtomicU64::new(0));
        let relay_incoming = Arc::new(BoundedRelayIncomingQueue::new(RELAY_INCOMING_QUEUE_CAP));
        let active_relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>> =
            Arc::new(Mutex::new(None));
        let disconnect_flag = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let lock_screen_count = Arc::new(AtomicU64::new(0));

        let make_approver = |require: bool, ui: &UiApprover| -> Box<dyn ConnectionApprover> {
            if require {
                Box::new(ui.clone())
            } else {
                Box::new(AutoApprover)
            }
        };

        // 先在 spawn 内绑定首轮监听器（普通 QUIC HostSession 或 mTLS LAN QuicEndpoint），使 listen_addr() 同步立即可用。
        let (first_listener, actual_addr) = if cfg.accept_mtls_lan {
            match cfg.mtls_config.as_ref() {
                Some(bundle) => {
                    let ep = match bind_mtls_lan_endpoint(cfg.listen_addr, bundle) {
                        Ok(ep) => ep,
                        Err(_) if cfg.listen_addr.port() == DEFAULT_HOST_PORT => {
                            let fallback_addr = SocketAddr::new(cfg.listen_addr.ip(), 0);
                            bind_mtls_lan_endpoint(fallback_addr, bundle)?
                        }
                        Err(e) => return Err(e),
                    };
                    let actual = ep
                        .local_addr()
                        .map_err(|e| format!("无法获取 mTLS LAN 实际监听地址: {e}"))?;
                    (HostListener::MtlsLan { endpoint: ep }, actual)
                }
                None => {
                    let resolved_addr = if cfg.listen_addr.port() == 0 {
                        std::net::UdpSocket::bind(cfg.listen_addr)
                            .and_then(|s| s.local_addr())
                            .unwrap_or(cfg.listen_addr)
                    } else {
                        cfg.listen_addr
                    };
                    (HostListener::AwaitingMtlsCert, resolved_addr)
                }
            }
        } else {
            let first_built = match build_host_session(
                cfg.listen_addr,
                cfg.password.clone(),
                cfg.approval_timeout_us,
                cfg.prefer_hardware,
                cfg.clipboard_enabled,
                cfg.allow_remote_assist,
                cfg.owner_verifier.clone(),
                Arc::clone(&shared_owner_replay),
                make_approver(cfg.require_approval, &approver),
                Arc::clone(&updates),
            ) {
                Ok(built) => built,
                Err(_) if cfg.listen_addr.port() == DEFAULT_HOST_PORT => {
                    let fallback_addr = SocketAddr::new(cfg.listen_addr.ip(), 0);
                    build_host_session(
                        fallback_addr,
                        cfg.password.clone(),
                        cfg.approval_timeout_us,
                        cfg.prefer_hardware,
                        cfg.clipboard_enabled,
                        cfg.allow_remote_assist,
                        cfg.owner_verifier.clone(),
                        Arc::clone(&shared_owner_replay),
                        make_approver(cfg.require_approval, &approver),
                        Arc::clone(&updates),
                    )?
                }
                Err(e) => return Err(e),
            };
            let actual = first_built
                .host
                .local_addr()
                .map_err(|e| format!("无法获取实际监听地址: {e}"))?;
            (HostListener::Plain(Box::new(first_built)), actual)
        };

        let worker_opts = HostWorkerOptions {
            approval_timeout_us: cfg.approval_timeout_us,
            prefer_hardware: cfg.prefer_hardware,
            require_approval: cfg.require_approval,
            clipboard_enabled: cfg.clipboard_enabled,
            audio_enabled: cfg.audio_enabled,
        };

        let t_updates = Arc::clone(&updates);
        let t_approver = approver.clone();
        let t_password_state = Arc::clone(&password_state);
        let t_pw_ver = Arc::clone(&password_version);
        let t_auth_state = Arc::clone(&auth_state);
        let t_cfg_ver = Arc::clone(&config_version);
        let t_relay_incoming = Arc::clone(&relay_incoming);
        let t_active_relay_interrupt = Arc::clone(&active_relay_interrupt);
        let t_disconnect = Arc::clone(&disconnect_flag);
        let t_stop = Arc::clone(&stop_flag);
        let t_running = Arc::clone(&running);
        let t_lock_screen_count = Arc::clone(&lock_screen_count);

        let handle = thread::Builder::new()
            .name("rc-ui-host".to_string())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                rc_capture_macos::promote_current_thread_qos();

                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_host_worker(
                        actual_addr,
                        worker_opts,
                        first_listener,
                        Arc::clone(&t_updates),
                        t_approver,
                        t_password_state,
                        t_pw_ver,
                        t_auth_state,
                        t_cfg_ver,
                        t_relay_incoming,
                        t_active_relay_interrupt,
                        t_disconnect,
                        t_stop,
                        t_lock_screen_count,
                    )
                }));
                if res.is_err() {
                    t_updates.push(HostUpdate::Error {
                        message: "被控端工作线程异常退出".to_string(),
                    });
                }
                // M8：先清除 running 标记再推送 Stopped，消除轮询器读到 Stopped 时 is_running 仍为 true 的竞态。
                t_running.store(false, Ordering::Release);
                t_updates.push(HostUpdate::Stopped);
            })
            .expect("spawn rc-ui-host thread");

        Ok(Self {
            listen_addr: actual_addr,
            updates,
            approver,
            password_state,
            password_version,
            auth_state,
            config_version,
            relay_incoming,
            active_relay_interrupt,
            disconnect_flag,
            stop_flag,
            running,
            lock_screen_count,
            thread: Some(handle),
        })
    }

    /// 统计本 Host 实例收到并执行的远端锁屏请求次数（供集成测试与诊断验证）。
    pub fn lock_screen_count(&self) -> u64 {
        self.lock_screen_count.load(Ordering::Acquire)
    }

    /// 返回实际绑定的监听地址（已解析临时端口）。
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// 工作线程是否仍在运行。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 非阻塞拉取一条被控端状态更新。
    pub fn try_recv_update(&self) -> Option<HostUpdate> {
        self.updates.try_pop()
    }

    /// 向被控端工作线程投递一条信令服务器下发的入站中继会话请求（有界非阻塞，容量 8，满载丢最旧）。
    pub fn offer_relay_incoming(&self, incoming: RelayIncoming) {
        self.relay_incoming.push(incoming);
    }

    /// 回写机主连接审批裁决（仅在途且代次匹配时生效）。
    pub fn respond_approval(&self, generation: u64, accepted: bool) -> bool {
        self.approver.resolve(generation, accepted)
    }

    /// 更新访问密码与会话结束自动轮换策略（对下一次新握手生效）。
    pub fn set_password_mode(&self, password: Option<String>, auto_rotate: bool) {
        {
            let mut st = self
                .password_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            st.password = password;
            st.auto_rotate = auto_rotate;
        }
        self.password_version.fetch_add(1, Ordering::AcqRel);
    }

    /// 更新访问密码（保留既有自动轮换策略，对下一次新握手生效）。
    pub fn set_password(&self, password: Option<String>) {
        {
            let mut st = self
                .password_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            st.password = password;
        }
        self.password_version.fetch_add(1, Ordering::AcqRel);
    }

    /// 运行时切换是否允许他人远程协助（对下一次新握手生效，本人同账号自控不受影响）。
    pub fn set_allow_remote_assist(&self, allow: bool) {
        {
            let mut st = self.auth_state.lock().unwrap_or_else(|e| e.into_inner());
            st.allow_remote_assist = allow;
        }
        self.config_version.fetch_add(1, Ordering::AcqRel);
    }

    /// 运行时注入或清除同账号 `OwnerTicket` 验签器（对下一次新握手生效）。
    pub fn set_owner_verifier(&self, verifier: Option<Arc<dyn OwnerTicketVerifier>>) {
        {
            let mut st = self.auth_state.lock().unwrap_or_else(|e| e.into_inner());
            st.owner_verifier = verifier;
        }
        self.config_version.fetch_add(1, Ordering::AcqRel);
    }

    /// 运行时更新双向 mTLS 证书束与 LAN mTLS 监听开关（对下一次监听与入站会话生效）。
    pub fn set_mtls_config(&self, mtls_config: Option<SessionMtlsBundle>, accept_mtls_lan: bool) {
        {
            let mut st = self.auth_state.lock().unwrap_or_else(|e| e.into_inner());
            st.mtls_config = mtls_config;
            st.accept_mtls_lan = accept_mtls_lan;
        }
        self.config_version.fetch_add(1, Ordering::AcqRel);
    }

    /// 主动断开当前已连接的控制端会话（触发优雅 `Bye` / 中继即时断流，被控端继续等待新连接）。
    pub fn disconnect_client(&self) {
        self.disconnect_flag.store(true, Ordering::Release);
        let interrupt = self
            .active_relay_interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(h) = interrupt {
            h.interrupt();
        }
    }

    /// 停止被控端服务并等待工作线程回收（幂等；可立即中断在途中继拨号/握手）。
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        self.disconnect_flag.store(true, Ordering::Release);
        self.approver.mark_stopped();
        let interrupt = self
            .active_relay_interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(h) = interrupt {
            h.interrupt();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug, Clone, Copy)]
struct HostWorkerOptions {
    approval_timeout_us: u64,
    prefer_hardware: bool,
    require_approval: bool,
    clipboard_enabled: bool,
    audio_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostTransportKind {
    Lan,
    MtlsLan,
    P2p,
    Relay,
}

/// 单通道无丢帧重试冲刷：
/// - 若 `pending` 非空，先冲刷待重试帧且不继续拉取引擎（避免回压期无界堆积）；
/// - 仅当 `pending` 为空时从引擎最多拉取 `max_frames` 帧；
/// - 任意帧发送返回 `Err`（如 TCP 中继背压）时立即停止并保留该帧及后续已拉取帧，
///   下一拍严格按 FIFO 顺序重试，确保可靠工具通道零丢块。
fn flush_tool_channel_with_retry<E>(
    pending: &mut VecDeque<Vec<u8>>,
    max_frames: usize,
    poll_fn: impl FnOnce(usize) -> Vec<Vec<u8>>,
    mut send_fn: impl FnMut(Vec<u8>) -> Result<(), E>,
) -> bool {
    if pending.is_empty() {
        pending.extend(poll_fn(max_frames));
    }
    let mut sent_any = false;
    while let Some(front) = pending.front() {
        if send_fn(front.clone()).is_ok() {
            pending.pop_front();
            sent_any = true;
        } else {
            break;
        }
    }
    sent_any
}

/// Host 侧专用文件传输工作线程（H12）：将 `list_local_dir` / `write_all` / `sync_all` (`F_FULLFSYNC`)
/// 与 `fs::rename` 全部移出 60fps 视频采集/编码主线程，出入站均采用有界通道（容量 64），
/// 会话结束析构时自动关闭通道并回收线程，确保 `FileHostEngine::drop` 及时清理未完成 `.rcpart`。
struct FileHostWorker {
    in_tx: Option<mpsc::SyncSender<Vec<u8>>>,
    out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    thread: Option<JoinHandle<()>>,
}

impl FileHostWorker {
    fn spawn() -> Self {
        let (in_tx, in_rx) = mpsc::sync_channel::<Vec<u8>>(FILE_HOST_WORKER_CHAN_CAP);
        let (out_tx, out_rx) = mpsc::sync_channel::<Vec<u8>>(FILE_HOST_WORKER_CHAN_CAP);
        let handle = thread::Builder::new()
            .name("rc-file-host-worker".to_string())
            .spawn(move || {
                let mut engine = FileHostEngine::new();
                let mut pending_out: VecDeque<Vec<u8>> = VecDeque::new();

                loop {
                    let mut did_work = false;

                    // 1) 每轮最多先处理 MAX_TOOL_FRAMES_PER_TICK 条入站线帧，避免饿死出站 ACK/Cancel。
                    for _ in 0..MAX_TOOL_FRAMES_PER_TICK {
                        match in_rx.try_recv() {
                            Ok(payload) => {
                                engine.handle_incoming(&payload);
                                did_work = true;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }

                    // 2) 待发送缓冲为空时从引擎拉取一批出站线帧。
                    if pending_out.is_empty() {
                        let frames = engine.poll_outgoing(MAX_TOOL_FRAMES_PER_TICK);
                        if !frames.is_empty() {
                            pending_out.extend(frames);
                            did_work = true;
                        }
                    }

                    // 3) 非阻塞投递至有界出站通道；满载时暂停投递并继续监听入站 Cancel。
                    let mut out_full = false;
                    while let Some(front) = pending_out.front() {
                        match out_tx.try_send(front.clone()) {
                            Ok(()) => {
                                pending_out.pop_front();
                                did_work = true;
                            }
                            Err(mpsc::TrySendError::Full(_)) => {
                                out_full = true;
                                break;
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                        }
                    }

                    // 4) 背压或完全空闲时高效泊车，不空转 CPU。
                    let has_more_work =
                        !pending_out.is_empty() || engine.active_download_count() > 0;
                    if out_full {
                        match in_rx.recv_timeout(Duration::from_millis(2)) {
                            Ok(payload) => engine.handle_incoming(&payload),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                    } else if !did_work && !has_more_work {
                        match in_rx.recv() {
                            Ok(payload) => engine.handle_incoming(&payload),
                            Err(_) => return,
                        }
                    }
                }
            })
            .expect("spawn rc-file-host-worker thread");

        Self {
            in_tx: Some(in_tx),
            out_rx: Some(out_rx),
            thread: Some(handle),
        }
    }

    fn try_send_incoming(&self, frame: Vec<u8>) -> Result<(), mpsc::TrySendError<Vec<u8>>> {
        match self.in_tx.as_ref() {
            Some(tx) => tx.try_send(frame),
            None => Err(mpsc::TrySendError::Disconnected(frame)),
        }
    }

    fn try_recv_outgoing(&self, max_frames: usize) -> Vec<Vec<u8>> {
        let Some(rx) = self.out_rx.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(max_frames.min(MAX_TOOL_FRAMES_PER_TICK));
        for _ in 0..max_frames {
            match rx.try_recv() {
                Ok(frame) => out.push(frame),
                Err(_) => break,
            }
        }
        out
    }
}

impl Drop for FileHostWorker {
    fn drop(&mut self) {
        self.in_tx.take();
        self.out_rx.take();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// 解析来自控制端 `TOOL_CHANNEL_AUDIO_CTRL` 的静音控制 JSON（`{"muted": bool}`）。
fn parse_audio_ctrl_muted(payload: &[u8]) -> Option<bool> {
    let val: serde_json::Value = serde_json::from_slice(payload).ok()?;
    val.get("muted").and_then(serde_json::Value::as_bool)
}

/// 计算本会话被控端的原生逻辑分辨率与最高输出分辨率上界（封顶 3840×2160）。
fn compute_host_display_bounds(
    host: &HostSession,
    prefer_hardware: bool,
    hardware_video_active: bool,
) -> (Resolution, Resolution) {
    let neg = host.negotiated().unwrap_or_else(|| host.caps());
    let caps = host.caps();
    if prefer_hardware && hardware_video_active {
        let native = host_hardware_display_caps(true)
            .unwrap_or_else(|| Resolution::new(neg.width, neg.height).to_even());
        let backing = host_hardware_display_backing(true).unwrap_or(native);
        let (mw, mh) = display_ctrl::fit_within_cap(
            backing.width.min(display_ctrl::MAX_STREAM_WIDTH),
            backing.height.min(display_ctrl::MAX_STREAM_HEIGHT),
            native.width,
            native.height,
        );
        let max_res = Resolution::new(mw.max(native.width), mh.max(native.height));
        (native, max_res)
    } else {
        let native = Resolution::new(neg.width, neg.height).to_even();
        let max_res = Resolution::new(
            caps.width.min(display_ctrl::MAX_STREAM_WIDTH),
            caps.height.min(display_ctrl::MAX_STREAM_HEIGHT),
        )
        .to_even();
        (native, max_res)
    }
}

/// 组装本机硬件显示器预设列表，并在原生逻辑分辨率处标记 `is_native = true`。
#[cfg(target_os = "macos")]
fn build_hardware_display_presets(
    prefer_hardware: bool,
    hardware_video_active: bool,
    original_native: Option<Resolution>,
) -> Vec<DisplayPreset> {
    if !(prefer_hardware && hardware_video_active) {
        return Vec::new();
    }
    let modes = rc_capture_macos::list_main_display_modes();
    if modes.is_empty() {
        return Vec::new();
    }
    let native_res = original_native.or_else(rc_capture_macos::main_display_logical_resolution);
    let mut presets: Vec<DisplayPreset> = Vec::with_capacity(modes.len());
    for m in modes {
        let is_native = native_res
            .is_some_and(|n| n.width.abs_diff(m.width) <= 2 && n.height.abs_diff(m.height) <= 2);
        let label = if m.is_hidpi {
            format!("{} × {} (HiDPI)", m.width, m.height)
        } else {
            format!("{} × {}", m.width, m.height)
        };
        presets.push(DisplayPreset {
            width: m.width,
            height: m.height,
            label,
            is_native,
            is_hidpi: m.is_hidpi,
        });
    }
    presets
}

#[cfg(not(target_os = "macos"))]
fn build_hardware_display_presets(
    _prefer_hardware: bool,
    _hardware_video_active: bool,
    _original_native: Option<Resolution>,
) -> Vec<DisplayPreset> {
    Vec::new()
}

/// 编码当前被控端 `DisplayControlMsg::Capabilities` 载荷。
fn encode_host_display_caps(
    host: &HostSession,
    native_res: Resolution,
    max_res: Resolution,
    presets: Vec<DisplayPreset>,
) -> Option<Vec<u8>> {
    let cur = host.negotiated()?;
    let max_fps = host.caps().fps.max(1);
    display_ctrl::encode_display_ctrl(&DisplayControlMsg::Capabilities {
        current_width: cur.width,
        current_height: cur.height,
        current_fps: cur.fps,
        native_width: native_res.width,
        native_height: native_res.height,
        max_width: max_res.width,
        max_height: max_res.height,
        max_fps,
        presets,
    })
}

#[cfg(target_os = "macos")]
struct HostDisplayModeSession {
    guard: Option<rc_capture_macos::DisplayModeGuard>,
}

#[cfg(target_os = "macos")]
impl HostDisplayModeSession {
    fn capture(prefer_hardware: bool, hardware_video_active: bool) -> Self {
        let guard = if prefer_hardware && hardware_video_active {
            rc_capture_macos::DisplayModeGuard::capture_current()
        } else {
            None
        };
        Self { guard }
    }

    fn original_resolution(&self) -> Option<Resolution> {
        self.guard.as_ref().map(|g| g.original_resolution())
    }

    fn switch_to(&mut self, target_w: u32, target_h: u32) -> Result<Resolution, String> {
        match self.guard.as_mut() {
            Some(g) => g.switch_to(target_w, target_h),
            None => Err("显示器模式守卫未激活".to_string()),
        }
    }

    fn restore(&mut self) {
        if let Some(g) = self.guard.as_mut() {
            g.restore();
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct HostDisplayModeSession;

#[cfg(not(target_os = "macos"))]
impl HostDisplayModeSession {
    fn capture(_prefer_hardware: bool, _hardware_video_active: bool) -> Self {
        Self
    }

    fn original_resolution(&self) -> Option<Resolution> {
        None
    }

    fn switch_to(&mut self, _target_w: u32, _target_h: u32) -> Result<Resolution, String> {
        Err("仅 macOS 支持显示器模式切换".to_string())
    }

    fn restore(&mut self) {}
}

#[cfg(target_os = "macos")]
fn spawn_wallpaper_fetcher() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let _ = thread::Builder::new()
        .name("rc-host-wallpaper".to_string())
        .spawn(move || {
            if let Some(jpeg) = rc_capture_macos::main_display_wallpaper_jpeg(640, 400) {
                if !jpeg.is_empty() {
                    let _ = tx.send(jpeg);
                }
            }
        });
    rx
}

#[cfg(not(target_os = "macos"))]
fn spawn_wallpaper_fetcher() -> mpsc::Receiver<Vec<u8>> {
    let (_tx, rx) = mpsc::sync_channel::<Vec<u8>>(1);
    rx
}

/// 单拍泵送主机侧音频采集：`suppress_send == false` 时编码 IMA-ADPCM 并发送；
/// 静音或 Stream A 工具通道背压（`suppress_send == true`）时仅排空陈旧缓冲不发送，避免加剧拥塞（M9）。
fn pump_host_audio(
    host: &mut HostSession,
    capture: &mut dyn AudioCapture,
    suppress_send: bool,
    audio_seq: &mut u32,
) -> bool {
    let mut busy = false;
    for _ in 0..MAX_AUDIO_FRAMES_PER_TICK {
        let Some(frame) = capture.try_recv_frame() else {
            break;
        };
        busy = true;
        if suppress_send || frame.samples.is_empty() {
            continue;
        }
        let data = encode_ima_adpcm(&frame.samples, frame.channels);
        let seq = *audio_seq;
        *audio_seq = audio_seq.wrapping_add(1);
        let _ = host.send_audio_frame(AudioFramePacket {
            seq,
            timestamp_us: frame.timestamp_us,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            codec: AUDIO_CODEC_IMA_ADPCM,
            data,
        });
    }
    busy
}

fn trigger_host_lock_screen(lock_screen_count: &AtomicU64, prefer_hardware: bool) {
    lock_screen_count.fetch_add(1, Ordering::AcqRel);
    #[cfg(target_os = "macos")]
    if prefer_hardware {
        let ok = rc_capture_macos::lock_screen_now();
        if !ok {
            eprintln!("[rc-ui-host] 锁定本机屏幕失败");
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = prefer_hardware;
}

/// U-H1：显式驱动已建立的 `HostSession`（`Streaming` 调 `tick`，`Detached` 调 `tick_detached`）。
///
/// 在以下任一条件满足时立即退出驱动循环并收尾，让外层 accept 循环回到监听态：
/// - `stop_flag` 或 `disconnect_flag` 置位；
/// - `Streaming` 期收到对端 `Bye`（`r.closed.is_some()`）；
/// - `Detached` 期重连窗口到期拆除管线（`DetachOutcome::TornDown`）；
/// - `Detached` 期密码版本发生变更（`password_version != observed_pw_ver`），需重建以使新密码生效。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn drive_ui_host_session(
    host: &mut HostSession,
    clock: &SessionClock,
    updates: &BoundedHostUpdateQueue,
    password_version: &AtomicU64,
    observed_pw_ver: u64,
    disconnect_flag: &AtomicBool,
    stop_flag: &AtomicBool,
) -> String {
    let lock_screen_count = AtomicU64::new(0);
    drive_ui_host_session_inner(
        host,
        clock,
        updates,
        password_version,
        observed_pw_ver,
        disconnect_flag,
        stop_flag,
        HostTransportKind::Lan,
        false,
        false,
        false,
        &lock_screen_count,
    )
}

#[allow(clippy::too_many_arguments)]
fn drive_ui_host_session_inner(
    host: &mut HostSession,
    clock: &SessionClock,
    updates: &BoundedHostUpdateQueue,
    password_version: &AtomicU64,
    observed_pw_ver: u64,
    disconnect_flag: &AtomicBool,
    stop_flag: &AtomicBool,
    transport_kind: HostTransportKind,
    prefer_hardware: bool,
    audio_enabled: bool,
    hardware_video_active: bool,
    lock_screen_count: &AtomicU64,
) -> String {
    #[cfg(target_os = "macos")]
    let _activity =
        rc_capture_macos::LatencyCriticalActivity::begin("RabbitControl 被控会话", true);

    let mut detached_reason: Option<String> = None;
    let mut drive_err: Option<String> = None;
    let mut pending_lock_on_exit = false;

    // 会话作用域工具引擎：文件 I/O 卸载至专用 `rc-file-host-worker` 线程（H12），
    // 会话断开退出本函数时自动析构，清理未完成 `.rcpart`、终止子 Shell PTY 并关闭代理 TCP 套接字。
    let mut file_worker = FileHostWorker::spawn();
    let mut term_host = TerminalHostEngine::new();
    let mut portmap_host = PortMapHostEngine::new();
    let mut pending_file_in: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_file_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_term_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_portmap_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_display_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let (mut session_native_res, mut session_max_res) =
        compute_host_display_bounds(host, prefer_hardware, hardware_video_active);
    let mut display_mode_session =
        HostDisplayModeSession::capture(prefer_hardware, hardware_video_active);
    let original_native_res = display_mode_session
        .original_resolution()
        .or(Some(session_native_res));
    let mut wallpaper_rx = if prefer_hardware && hardware_video_active {
        Some(spawn_wallpaper_fetcher())
    } else {
        None
    };

    if host.state() == SessionState::Streaming {
        let presets = build_hardware_display_presets(
            prefer_hardware,
            hardware_video_active,
            original_native_res,
        );
        if let Some(payload) = encode_host_display_caps(
            host,
            original_native_res.unwrap_or(session_native_res),
            session_max_res,
            presets,
        ) {
            pending_display_tx.push_back(payload);
        }
    }

    // H13：仅当启用音频且硬件视频管线确实为真实 SCK+VT（未降级假栈）时，
    // 在独立后台线程异步初始化系统音频采集，绝不在 `Welcome` 后同步阻塞首帧出图。
    let mut audio_capture: Option<Box<dyn AudioCapture>> = None;
    let mut audio_init_rx: Option<mpsc::Receiver<Box<dyn AudioCapture>>> =
        if audio_enabled && prefer_hardware && hardware_video_active {
            let (tx, rx) = mpsc::sync_channel::<Box<dyn AudioCapture>>(1);
            let _ = thread::Builder::new()
                .name("rc-audio-cap-init".to_string())
                .spawn(move || {
                    if let Ok(cap) = rc_audio::create_system_audio_capture() {
                        if let Err(mpsc::SendError(mut cap)) = tx.send(cap) {
                            cap.stop();
                            drop(cap);
                        }
                    }
                });
            Some(rx)
        } else {
            None
        };
    let mut audio_muted = false;
    let mut audio_seq: u32 = 0;
    let mut last_busy = true;

    while !stop_flag.load(Ordering::Acquire)
        && !disconnect_flag.load(Ordering::Acquire)
        && host.state() != SessionState::Closed
    {
        match host.state() {
            SessionState::Streaming => {
                let poll_budget = if last_busy || host.has_pending_pipeline_work() {
                    Duration::ZERO
                } else {
                    Duration::from_millis(1)
                };
                match host.tick(poll_budget) {
                    Ok(r) => {
                        if r.closed.is_some() {
                            for (channel, payload) in host.drain_tool_messages() {
                                if channel == TOOL_CHANNEL_DISPLAY_CTRL
                                    && matches!(
                                        display_ctrl::decode_display_ctrl(&payload),
                                        Some(DisplayControlMsg::LockScreen)
                                    )
                                {
                                    pending_lock_on_exit = true;
                                }
                            }
                            break;
                        }
                        if host.state() != SessionState::Streaming {
                            continue;
                        }

                        // 0) 非阻塞检查后台音频采集与桌面壁纸初始化是否就绪（H13）。
                        if audio_capture.is_none() {
                            if let Some(ref rx) = audio_init_rx {
                                match rx.try_recv() {
                                    Ok(cap) => {
                                        audio_capture = Some(cap);
                                        audio_init_rx = None;
                                    }
                                    Err(mpsc::TryRecvError::Empty) => {}
                                    Err(mpsc::TryRecvError::Disconnected) => {
                                        audio_init_rx = None;
                                    }
                                }
                            }
                        }
                        if let Some(ref rx) = wallpaper_rx {
                            match rx.try_recv() {
                                Ok(jpeg) => {
                                    wallpaper_rx = None;
                                    let data_base64 = display_ctrl::base64_encode(&jpeg);
                                    if let Some(wp_payload) = display_ctrl::encode_display_ctrl(
                                        &DisplayControlMsg::Wallpaper {
                                            mime: "image/jpeg".to_string(),
                                            data_base64,
                                        },
                                    ) {
                                        pending_display_tx.push_back(wp_payload);
                                    }
                                }
                                Err(mpsc::TryRecvError::Empty) => {}
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    wallpaper_rx = None;
                                }
                            }
                        }

                        // 1) 剪贴板本机侧轮询泵。
                        let _ = host.pump_clipboard(u64::from(clock()));

                        // 2) 工具复用通道入站分发（W2 文件异步交由专用工作线程 / W3 终端 / W4 端口映射 / 音频控制）。
                        let incoming_tools = host.drain_tool_messages();
                        let mut tools_busy = !incoming_tools.is_empty();
                        for (channel, payload) in incoming_tools {
                            match channel {
                                TOOL_CHANNEL_FILE => pending_file_in.push_back(payload),
                                TOOL_CHANNEL_TERMINAL => term_host.handle_incoming(&payload),
                                TOOL_CHANNEL_PORTMAP => portmap_host.handle_incoming(&payload),
                                TOOL_CHANNEL_AUDIO_CTRL => {
                                    if let Some(muted) = parse_audio_ctrl_muted(&payload) {
                                        audio_muted = muted;
                                    }
                                }
                                TOOL_CHANNEL_DISPLAY_CTRL => {
                                    match display_ctrl::decode_display_ctrl(&payload) {
                                        Some(DisplayControlMsg::SetStream {
                                            width,
                                            height,
                                            fps,
                                        }) => {
                                            if let Some(cur) = host.negotiated() {
                                                let target_fps = (fps > 0).then_some(fps);
                                                if width > 0 && height > 0 {
                                                    if let Ok(switched_res) = display_mode_session
                                                        .switch_to(width, height)
                                                    {
                                                        host.set_injector_screen_size(
                                                            switched_res.width,
                                                            switched_res.height,
                                                        );
                                                        session_native_res = switched_res;
                                                        let cur_backing =
                                                            host_hardware_display_backing(true)
                                                                .unwrap_or(switched_res);
                                                        session_max_res = Resolution {
                                                            width: cur_backing
                                                                .width
                                                                .max(switched_res.width),
                                                            height: cur_backing
                                                                .height
                                                                .max(switched_res.height),
                                                        };
                                                        if let Err(err) = host.reconfigure_stream(
                                                            switched_res.width,
                                                            switched_res.height,
                                                            target_fps,
                                                            Some(switched_res),
                                                        ) {
                                                            eprintln!(
                                                        "[rc-ui-host] 热切换流参数 ({}x{}@{fps}fps) 失败: {err}",
                                                        switched_res.width, switched_res.height
                                                    );
                                                        }
                                                    } else {
                                                        let (target_w, target_h) =
                                                            display_ctrl::fit_target_preserving_aspect(
                                                                width,
                                                                height,
                                                                session_native_res.width,
                                                                session_native_res.height,
                                                                session_max_res.width,
                                                                session_max_res.height,
                                                            );
                                                        if let Err(err) = host.reconfigure_stream(
                                                            target_w,
                                                            target_h,
                                                            target_fps,
                                                            Some(session_max_res),
                                                        ) {
                                                            eprintln!(
                                                        "[rc-ui-host] 热切换流参数 ({target_w}x{target_h}@{fps}fps) 失败: {err}"
                                                    );
                                                        }
                                                    }
                                                } else if let Err(err) = host.reconfigure_stream(
                                                    cur.width,
                                                    cur.height,
                                                    target_fps,
                                                    Some(session_max_res),
                                                ) {
                                                    eprintln!(
                                                "[rc-ui-host] 热切换流参数 ({}x{}@{fps}fps) 失败: {err}",
                                                cur.width, cur.height
                                            );
                                                }
                                                let presets = build_hardware_display_presets(
                                                    prefer_hardware,
                                                    hardware_video_active,
                                                    original_native_res,
                                                );
                                                if let Some(caps_payload) = encode_host_display_caps(
                                                    host,
                                                    original_native_res
                                                        .unwrap_or(session_native_res),
                                                    session_max_res,
                                                    presets,
                                                ) {
                                                    pending_display_tx.push_back(caps_payload);
                                                }
                                            }
                                        }
                                        Some(DisplayControlMsg::LockScreen) => {
                                            trigger_host_lock_screen(
                                                lock_screen_count,
                                                prefer_hardware,
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }

                        // H12：非阻塞冲刷入站文件帧至 `rc-file-host-worker` 有界通道（满载时留待下拍重试，零丢帧）。
                        while let Some(front) = pending_file_in.front() {
                            match file_worker.try_send_incoming(front.clone()) {
                                Ok(()) => {
                                    pending_file_in.pop_front();
                                    tools_busy = true;
                                }
                                Err(mpsc::TrySendError::Full(_)) => break,
                                Err(mpsc::TrySendError::Disconnected(_)) => {
                                    pending_file_in.clear();
                                    break;
                                }
                            }
                        }

                        // 3) 工具复用通道出站节流泵（每引擎单拍最多 8 帧 + 背压重试队列，零丢块且不饿死视频/输入）。
                        if flush_tool_channel_with_retry(
                            &mut pending_file_tx,
                            MAX_TOOL_FRAMES_PER_TICK,
                            |n| file_worker.try_recv_outgoing(n),
                            |frame| host.send_tool_message(TOOL_CHANNEL_FILE, frame),
                        ) {
                            tools_busy = true;
                        }
                        if flush_tool_channel_with_retry(
                            &mut pending_term_tx,
                            MAX_TOOL_FRAMES_PER_TICK,
                            |n| term_host.poll_outgoing(n),
                            |frame| host.send_tool_message(TOOL_CHANNEL_TERMINAL, frame),
                        ) {
                            tools_busy = true;
                        }
                        if flush_tool_channel_with_retry(
                            &mut pending_portmap_tx,
                            MAX_TOOL_FRAMES_PER_TICK,
                            |n| portmap_host.poll_outgoing(n),
                            |frame| host.send_tool_message(TOOL_CHANNEL_PORTMAP, frame),
                        ) {
                            tools_busy = true;
                        }
                        if flush_tool_channel_with_retry(
                            &mut pending_display_tx,
                            MAX_TOOL_FRAMES_PER_TICK,
                            |_| Vec::new(),
                            |frame| host.send_tool_message(TOOL_CHANNEL_DISPLAY_CTRL, frame),
                        ) {
                            tools_busy = true;
                        }

                        // 4) 系统音频采集 → IMA-ADPCM 编码 → AudioFramePacket 发送。
                        //    M9：当文件/端口映射通道存在背压待重试帧时，本拍跳过发送音频帧避免加剧 Stream A 拥塞。
                        let stream_a_congested =
                            !pending_file_tx.is_empty() || !pending_portmap_tx.is_empty();
                        let audio_busy = match audio_capture.as_mut() {
                            Some(cap) => pump_host_audio(
                                host,
                                cap.as_mut(),
                                audio_muted || stream_a_congested,
                                &mut audio_seq,
                            ),
                            None => false,
                        };

                        last_busy = r.busy || tools_busy || audio_busy;
                        if !last_busy && host.state() == SessionState::Streaming {
                            let wait_ms =
                                if pending_file_in.is_empty() && pending_display_tx.is_empty() {
                                    2
                                } else {
                                    1
                                };
                            host.pipeline().wait_event(Duration::from_millis(wait_ms));
                        }
                    }
                    Err(e) => {
                        drive_err = Some(format!("会话运行异常: {e}"));
                        break;
                    }
                }
            }
            SessionState::Detached => {
                match transport_kind {
                    HostTransportKind::P2p => {
                        detached_reason = Some("P2P 连接已断开".to_string());
                        break;
                    }
                    HostTransportKind::Relay => {
                        // 一次性 RCR1 中继令牌不可在原连接上续传，断链后直接收尾回到监听态。
                        detached_reason = Some("中继连接已断开".to_string());
                        break;
                    }
                    HostTransportKind::MtlsLan => {
                        detached_reason = Some("mTLS LAN 连接已断开".to_string());
                        break;
                    }
                    HostTransportKind::Lan => {}
                }
                if password_version.load(Ordering::Acquire) != observed_pw_ver {
                    detached_reason = Some("密码已变更，重置监听".to_string());
                    break;
                }
                let now_us = u64::from(clock());
                match host.tick_detached(now_us) {
                    Ok(DetachOutcome::TornDown) => {
                        detached_reason = Some("控制端重连超时已断开".to_string());
                        break;
                    }
                    Ok(DetachOutcome::Resumed) => {
                        host.reset_clipboard_epoch();
                        last_busy = true;
                        pending_display_tx.clear();
                        let presets = build_hardware_display_presets(
                            prefer_hardware,
                            hardware_video_active,
                            original_native_res,
                        );
                        if let Some(payload) = encode_host_display_caps(
                            host,
                            original_native_res.unwrap_or(session_native_res),
                            session_max_res,
                            presets,
                        ) {
                            pending_display_tx.push_back(payload);
                        }
                        emit_peer_connected(host, &host.peer_addr().to_string(), updates);
                    }
                    Ok(DetachOutcome::Replaced) => {
                        // M9：新控制端顶替旧会话时复位音频静音与全部会话级工具引擎/待重试队列，
                        // 避免新客户端继承旧客户端的静音状态、终端 PTY、端口映射或陈旧线帧。
                        display_mode_session.restore();
                        if let Some(orig) = original_native_res {
                            host.set_injector_screen_size(orig.width, orig.height);
                        }
                        last_busy = true;
                        host.reset_clipboard_epoch();
                        audio_muted = false;
                        audio_seq = 0;
                        pending_file_in.clear();
                        pending_file_tx.clear();
                        pending_term_tx.clear();
                        pending_portmap_tx.clear();
                        pending_display_tx.clear();
                        (session_native_res, session_max_res) = compute_host_display_bounds(
                            host,
                            prefer_hardware,
                            hardware_video_active,
                        );
                        let presets = build_hardware_display_presets(
                            prefer_hardware,
                            hardware_video_active,
                            original_native_res,
                        );
                        if let Some(payload) = encode_host_display_caps(
                            host,
                            original_native_res.unwrap_or(session_native_res),
                            session_max_res,
                            presets,
                        ) {
                            pending_display_tx.push_back(payload);
                        }
                        wallpaper_rx = if prefer_hardware && hardware_video_active {
                            Some(spawn_wallpaper_fetcher())
                        } else {
                            None
                        };
                        file_worker = FileHostWorker::spawn();
                        term_host = TerminalHostEngine::new();
                        portmap_host = PortMapHostEngine::new();
                        emit_peer_connected(host, &host.peer_addr().to_string(), updates);
                    }
                    Ok(DetachOutcome::Waiting) => {}
                    Err(e) => {
                        drive_err = Some(format!("会话运行异常: {e}"));
                        break;
                    }
                }
            }
            SessionState::Init
            | SessionState::Connected
            | SessionState::Reconnecting
            | SessionState::Closed => break,
        }
    }

    // 先释放异步初始化接收端，确保尚未完成的 `rc-audio-cap-init` 线程立即感知并停止释放采集器（H13）。
    drop(audio_init_rx);
    if let Some(ref mut cap) = audio_capture {
        cap.stop();
    }
    drop(audio_capture);
    drop(file_worker);
    drop(term_host);
    drop(portmap_host);
    display_mode_session.restore();
    drop(display_mode_session);
    if pending_lock_on_exit {
        trigger_host_lock_screen(lock_screen_count, prefer_hardware);
    }

    let user_disconnected = disconnect_flag.load(Ordering::Acquire);
    let close_res = host.close(ByeReason::Normal);
    if user_disconnected {
        return "本机主动断开".to_string();
    }
    if let Some(err) = drive_err {
        return err;
    }
    if let Some(reason) = detached_reason {
        return reason;
    }
    match close_res {
        Ok(summary) => match summary.stop_reason {
            StopReason::Requested => "本机主动断开".to_string(),
            StopReason::ByeReceived(_) => "控制端已断开".to_string(),
            StopReason::Disconnected(r) => format!("连接中断 ({r})"),
        },
        Err(e) => format!("会话运行异常: {e}"),
    }
}

/// 会话结束后若启用临时验证码自动轮换（且未停机），在工作线程内原子生成新码并推进版本号，
/// 确保紧随其后的下一条入站请求不可能复用已过期的旧临时验证码（M7）。
fn maybe_rotate_password_after_session(
    password_state: &Mutex<PasswordState>,
    password_version: &AtomicU64,
    updates: &BoundedHostUpdateQueue,
    stop_flag: &AtomicBool,
) {
    if stop_flag.load(Ordering::Acquire) {
        return;
    }
    let rotated = {
        let mut st = password_state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.auto_rotate {
            None
        } else {
            let prev = st.password.clone();
            let mut next = generate_temp_code();
            if prev.as_deref() == Some(next.as_str()) {
                next = generate_temp_code();
            }
            st.password = Some(next.clone());
            password_version.fetch_add(1, Ordering::AcqRel);
            Some(next)
        }
    };
    if let Some(password) = rotated {
        updates.push(HostUpdate::PasswordRotated { password });
    }
}

/// 判定 P2P 握手失败错误是否属于「传输层超时/断连」导致的分裂脑场景（H4）：
/// 仅在 `Timeout` / `Network` / `Io` 错误时允许回退至 TCP 中继重试握手；
/// 鉴权拒绝 / 协议错误（`Protocol`）或非法配置（`InvalidConfig`）绝不回退重试。
fn is_p2p_handshake_fallback_eligible(err: &rc_types::Error) -> bool {
    matches!(
        err,
        rc_types::Error::Timeout(_) | rc_types::Error::Network(_) | rc_types::Error::Io(_)
    )
}

struct EstablishedIncomingHost {
    built: Box<BuiltUiHost>,
    streaming_started: Arc<AtomicBool>,
    live_pw_ver: u64,
    transport_kind: HostTransportKind,
    peer_label: String,
}

enum HandshakeTransportOutcome {
    Ready {
        built: Box<BuiltUiHost>,
        streaming_started: Arc<AtomicBool>,
        live_pw_ver: u64,
    },
    Failed(rc_types::Error),
    BuildError,
}

#[allow(clippy::too_many_arguments)]
fn build_and_handshake_transport(
    raw_transport: Box<dyn Transport>,
    peer_label: &str,
    actual_addr: SocketAddr,
    opts: HostWorkerOptions,
    updates: &Arc<BoundedHostUpdateQueue>,
    approver: &UiApprover,
    password_state: &Mutex<PasswordState>,
    password_version: &AtomicU64,
    auth_state: &Mutex<AuthRuntimeState>,
    disconnect_flag: &Arc<AtomicBool>,
    stop_flag: &Arc<AtomicBool>,
) -> HandshakeTransportOutcome {
    let streaming_started = Arc::new(AtomicBool::new(false));
    let transport: Box<dyn Transport> = Box::new(CancellableHostTransport {
        inner: raw_transport,
        stop_flag: Arc::clone(stop_flag),
        disconnect_flag: Arc::clone(disconnect_flag),
        streaming_started: Arc::clone(&streaming_started),
    });

    let live_pw_ver = password_version.load(Ordering::Acquire);
    let live_pw = password_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .password
        .clone();
    let live_auth = auth_state.lock().unwrap_or_else(|e| e.into_inner()).clone();

    let box_approver: Box<dyn ConnectionApprover> = if opts.require_approval {
        approver.set_remote_addr_override(Some(peer_label.to_string()));
        Box::new(approver.clone())
    } else {
        Box::new(AutoApprover)
    };

    let mut built = match build_transport_host_session(
        actual_addr,
        transport,
        live_pw,
        opts.approval_timeout_us,
        opts.prefer_hardware,
        opts.clipboard_enabled,
        live_auth.allow_remote_assist,
        live_auth.owner_verifier,
        live_auth.owner_replay_cache,
        box_approver,
        Arc::clone(updates),
    ) {
        Ok(b) => b,
        Err(e) => {
            approver.set_remote_addr_override(None);
            if !stop_flag.load(Ordering::Acquire) {
                updates.push(HostUpdate::Error { message: e });
            }
            return HandshakeTransportOutcome::BuildError;
        }
    };

    *built.clock_origin.lock().unwrap_or_else(|e| e.into_inner()) =
        ClockOrigin::Pending(Instant::now());
    let handshake_res = built.host.handshake();
    approver.set_remote_addr_override(None);

    match handshake_res {
        Ok(_) => HandshakeTransportOutcome::Ready {
            built: Box::new(built),
            streaming_started,
            live_pw_ver,
        },
        Err(err) => {
            // 显式释放已失效的 HostSession（自动取消在途审批并关闭底层传输）。
            drop(built);
            HandshakeTransportOutcome::Failed(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dial_relay_for_incoming(
    incoming: &RelayIncoming,
    mtls_bundle: Option<&SessionMtlsBundle>,
    require_e2ee: bool,
    updates: &BoundedHostUpdateQueue,
    active_relay_interrupt: &Mutex<Option<ActiveRelayInterrupt>>,
    session_cancel: &Arc<AtomicBool>,
    disconnect_flag: &AtomicBool,
    stop_flag: &AtomicBool,
) -> Option<(Box<dyn Transport>, HostTransportKind, String)> {
    if incoming.relay_addr.is_empty() {
        return None;
    }
    let (transport, shutdown_handle): (Box<dyn Transport>, RelayShutdownHandle) =
        if let Some(bundle) = mtls_bundle {
            let expected_user = incoming
                .is_owner_connection
                .then(|| bundle.local_user_id.clone());
            let peer_bundle = bundle
                .clone()
                .with_expected_peer(Some(incoming.peer_id.clone()), expected_user);
            let relay_mtls = peer_bundle.to_relay_mtls_config(None);
            let e2ee_mode = if require_e2ee {
                RelayE2eeMode::HostRequired(peer_bundle)
            } else {
                RelayE2eeMode::HostAuto(peer_bundle)
            };
            let t = match E2eeRelayTransport::connect_cancellable(
                &incoming.relay_addr,
                RelayRole::Host,
                &incoming.relay_token,
                RELAY_CONNECT_TIMEOUT,
                Arc::clone(session_cancel),
                Some(&relay_mtls),
                e2ee_mode,
            ) {
                Ok(t) => t,
                Err(e) => {
                    disconnect_flag.store(false, Ordering::Release);
                    if !stop_flag.load(Ordering::Acquire) && !session_cancel.load(Ordering::Acquire)
                    {
                        updates.push(HostUpdate::Error {
                            message: format!("mTLS 中继通道连接失败 ({}): {e}", incoming.peer_id),
                        });
                    }
                    return None;
                }
            };
            let h = t.shutdown_handle();
            (Box::new(t), h)
        } else {
            let t = match TcpRelayTransport::connect_cancellable(
                &incoming.relay_addr,
                RelayRole::Host,
                &incoming.relay_token,
                RELAY_CONNECT_TIMEOUT,
                Arc::clone(session_cancel),
            ) {
                Ok(t) => t,
                Err(e) => {
                    disconnect_flag.store(false, Ordering::Release);
                    if !stop_flag.load(Ordering::Acquire) && !session_cancel.load(Ordering::Acquire)
                    {
                        updates.push(HostUpdate::Error {
                            message: format!("中继通道连接失败 ({}): {e}", incoming.peer_id),
                        });
                    }
                    return None;
                }
            };
            let h = t.shutdown_handle();
            (Box::new(t), h)
        };

    *active_relay_interrupt
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        Some(ActiveRelayInterrupt::Connected(shutdown_handle.clone()));

    // 注册中断句柄后立刻复核停机/断连标志，消除建连完成与 stop()/disconnect_client() 之间的竞态（H3）。
    if stop_flag.load(Ordering::Acquire) || disconnect_flag.load(Ordering::Acquire) {
        shutdown_handle.shutdown();
        disconnect_flag.store(false, Ordering::Release);
        return None;
    }

    Some((
        transport,
        HostTransportKind::Relay,
        format!("设备 {} (中继)", incoming.peer_id),
    ))
}

/// 处理一条入站信令会话请求：
/// 1) 若对端提供 UDP 候选地址，优先尝试 P2P UDP 打洞 + QUIC 直连（配置了 mTLS 时自动走双向 mTLS）；
/// 2) 打洞或 QUIC 失败，或 P2P 建连后握手阶段因超时/断链在进入 `Streaming` 前失败（H4 分裂脑恢复），
///    均自动无缝回退至 TCP `RCR1` 中继并重新握手（配置了 mTLS 时强制走外层 mTLS，内层 `HostAuto` 自动兼容客户端开关）；
/// 3) 完成鉴权与机主审批 → 串流驱动（含剪贴板/音频/W2/W3/W4 工具引擎）→ 干净收尾。
#[allow(clippy::too_many_arguments)]
fn serve_relay_incoming(
    incoming: RelayIncoming,
    actual_addr: SocketAddr,
    display_addr: &str,
    opts: HostWorkerOptions,
    updates: &Arc<BoundedHostUpdateQueue>,
    approver: &UiApprover,
    password_state: &Mutex<PasswordState>,
    password_version: &AtomicU64,
    auth_state: &Mutex<AuthRuntimeState>,
    active_relay_interrupt: &Mutex<Option<ActiveRelayInterrupt>>,
    disconnect_flag: &Arc<AtomicBool>,
    stop_flag: &Arc<AtomicBool>,
    lock_screen_count: &AtomicU64,
) {
    if stop_flag.load(Ordering::Acquire) {
        return;
    }
    // 清除此前空闲期残留的 disconnect 标记，确保本次新入站会话能正常建连；
    // 若建连/握手期间再次调用 disconnect_client() 则可即时中断。
    disconnect_flag.store(false, Ordering::Release);

    // 使用每会话独立的取消原子量（绝不直接传入 HostHandle::stop_flag，避免 TcpRelayTransport::drop
    // 置位 cancel 时误杀外层被控端工作线程）。
    let session_cancel = Arc::new(AtomicBool::new(false));
    *active_relay_interrupt
        .lock()
        .unwrap_or_else(|e| e.into_inner()) =
        Some(ActiveRelayInterrupt::Dialing(Arc::clone(&session_cancel)));
    let _interrupt_guard = RelayInterruptSlotGuard {
        slot: active_relay_interrupt,
    };

    if stop_flag.load(Ordering::Acquire) || disconnect_flag.load(Ordering::Acquire) {
        disconnect_flag.store(false, Ordering::Release);
        return;
    }

    let auth_snapshot = auth_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mtls_bundle = auth_snapshot.mtls_config.clone();
    let p2p_mtls = if auth_snapshot.accept_mtls_lan {
        match &mtls_bundle {
            Some(bundle) => {
                let expected_user = incoming
                    .is_owner_connection
                    .then(|| bundle.local_user_id.clone());
                match bundle
                    .clone()
                    .with_expected_peer(Some(incoming.peer_id.clone()), expected_user)
                    .to_quic_mtls_config()
                {
                    Ok(cfg) => Some(cfg),
                    Err(e) => {
                        updates.push(HostUpdate::Error {
                            message: format!("mTLS 配置无效 ({}): {e}", incoming.peer_id),
                        });
                        return;
                    }
                }
            }
            None => {
                updates.push(HostUpdate::Error {
                    message: format!(
                        "已开启加密连接但缺少 mTLS 设备证书，已拒绝入站会话 ({})",
                        incoming.peer_id
                    ),
                });
                return;
            }
        }
    } else {
        None
    };

    // 1) 优先尝试 P2P UDP 打洞 + QUIC 直连；若 QUIC 建连成功但应用层握手前断连/超时（H4 分裂脑：
    //    控制端 P2P 刚好超时已切中继），释放死 P2P 会话并自动回退至 TCP 中继重试握手。
    let candidates = P2pCandidates {
        session_id: incoming.session_id.clone(),
        peer_public_addr: incoming.peer_public_addr.clone(),
        peer_lan_addrs: incoming.peer_lan_addrs.clone(),
        local_udp_port: incoming.local_udp_port,
    };
    let mut established: Option<EstablishedIncomingHost> = None;

    if !p2p::parse_candidate_addrs(&candidates).is_empty()
        && !stop_flag.load(Ordering::Acquire)
        && !disconnect_flag.load(Ordering::Acquire)
    {
        if let Ok(p2p_conn) = p2p::attempt_p2p_quic_host_mtls(
            &candidates,
            p2p::DEFAULT_PUNCH_TIMEOUT,
            p2p::DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
            &session_cancel,
            p2p_mtls.as_ref(),
        ) {
            if stop_flag.load(Ordering::Acquire)
                || disconnect_flag.load(Ordering::Acquire)
                || session_cancel.load(Ordering::Acquire)
            {
                disconnect_flag.store(false, Ordering::Release);
                return;
            }
            let peer_label = format!("设备 {} (P2P 直连)", incoming.peer_id);
            match build_and_handshake_transport(
                Box::new(p2p_conn),
                &peer_label,
                actual_addr,
                opts,
                updates,
                approver,
                password_state,
                password_version,
                auth_state,
                disconnect_flag,
                stop_flag,
            ) {
                HandshakeTransportOutcome::Ready {
                    built,
                    streaming_started,
                    live_pw_ver,
                } => {
                    established = Some(EstablishedIncomingHost {
                        built,
                        streaming_started,
                        live_pw_ver,
                        transport_kind: HostTransportKind::P2p,
                        peer_label,
                    });
                }
                HandshakeTransportOutcome::Failed(err) => {
                    let can_fallback_relay = is_p2p_handshake_fallback_eligible(&err)
                        && !stop_flag.load(Ordering::Acquire)
                        && !disconnect_flag.load(Ordering::Acquire)
                        && !session_cancel.load(Ordering::Acquire)
                        && !incoming.relay_addr.is_empty();
                    if !can_fallback_relay {
                        disconnect_flag.store(false, Ordering::Release);
                        return;
                    }
                }
                HandshakeTransportOutcome::BuildError => {
                    disconnect_flag.store(false, Ordering::Release);
                    return;
                }
            }
        }
    }

    if stop_flag.load(Ordering::Acquire)
        || disconnect_flag.load(Ordering::Acquire)
        || session_cancel.load(Ordering::Acquire)
    {
        disconnect_flag.store(false, Ordering::Release);
        return;
    }

    // 2) 若 P2P 未成功或握手前因分裂脑回退，无缝拨号 TCP RCR1 中继并构造新 HostSession 重试握手。
    let EstablishedIncomingHost {
        built,
        streaming_started,
        live_pw_ver,
        transport_kind,
        peer_label,
    } = match established {
        Some(ready) => ready,
        None => {
            // 中继内层始终采用 `HostAuto`（保留 `:21117` 外层强制 mTLS），以便关闭了 S3 加密开关的客户端也可连通。
            let require_e2ee = false;
            let Some((raw_transport, transport_kind, peer_label)) = dial_relay_for_incoming(
                &incoming,
                mtls_bundle.as_ref(),
                require_e2ee,
                updates,
                active_relay_interrupt,
                &session_cancel,
                disconnect_flag,
                stop_flag,
            ) else {
                disconnect_flag.store(false, Ordering::Release);
                return;
            };
            match build_and_handshake_transport(
                raw_transport,
                &peer_label,
                actual_addr,
                opts,
                updates,
                approver,
                password_state,
                password_version,
                auth_state,
                disconnect_flag,
                stop_flag,
            ) {
                HandshakeTransportOutcome::Ready {
                    built,
                    streaming_started,
                    live_pw_ver,
                } => EstablishedIncomingHost {
                    built,
                    streaming_started,
                    live_pw_ver,
                    transport_kind,
                    peer_label,
                },
                HandshakeTransportOutcome::Failed(_) | HandshakeTransportOutcome::BuildError => {
                    disconnect_flag.store(false, Ordering::Release);
                    return;
                }
            }
        }
    };

    let BuiltUiHost {
        mut host,
        clock,
        clock_origin: _,
        backend_label,
        hardware_video_active,
    } = *built;

    streaming_started.store(true, Ordering::Release);
    emit_peer_connected(&host, &peer_label, updates);

    let reason = drive_ui_host_session_inner(
        &mut host,
        &clock,
        updates,
        password_version,
        live_pw_ver,
        disconnect_flag,
        stop_flag,
        transport_kind,
        opts.prefer_hardware,
        opts.audio_enabled,
        hardware_video_active.load(Ordering::Acquire),
        lock_screen_count,
    );
    let last_backend = backend_label
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    disconnect_flag.store(false, Ordering::Release);
    drop(host);

    // M7：先于 ClientDisconnected 与下一次连接原子轮换临时密码。
    maybe_rotate_password_after_session(password_state, password_version, updates, stop_flag);
    updates.push(HostUpdate::ClientDisconnected { reason });
    if !stop_flag.load(Ordering::Acquire) {
        updates.push(HostUpdate::Listening {
            listen_addr: actual_addr.to_string(),
            display_addr: display_addr.to_string(),
            backend: last_backend,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn run_host_worker(
    actual_addr: SocketAddr,
    opts: HostWorkerOptions,
    first_listener: HostListener,
    updates: Arc<BoundedHostUpdateQueue>,
    approver: UiApprover,
    password_state: Arc<Mutex<PasswordState>>,
    password_version: Arc<AtomicU64>,
    auth_state: Arc<Mutex<AuthRuntimeState>>,
    config_version: Arc<AtomicU64>,
    incoming_relay: Arc<BoundedRelayIncomingQueue>,
    active_relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>>,
    disconnect_flag: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
    lock_screen_count: Arc<AtomicU64>,
) {
    let display_addr = format_display_addr(actual_addr);
    let initial_backend = match &first_listener {
        HostListener::Plain(b) => b
            .backend_label
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        HostListener::MtlsLan { .. } | HostListener::AwaitingMtlsCert => {
            if opts.prefer_hardware {
                "auto".to_string()
            } else {
                "fake".to_string()
            }
        }
    };
    updates.push(HostUpdate::Listening {
        listen_addr: actual_addr.to_string(),
        display_addr: display_addr.clone(),
        backend: initial_backend,
    });

    let mut next_listener = Some(first_listener);
    // U-M1：`first_listener` 在 `HostHandle::spawn` 时以初始版本 0 构造；
    // 若在工作线程进入此处前调用方已调用 `set_password` 或修改鉴权配置，必须立即可见并重建。
    let mut observed_pw_ver = 0u64;
    let mut observed_cfg_ver = 0u64;

    while !stop_flag.load(Ordering::Acquire) {
        let listener = match next_listener.take() {
            Some(l) => l,
            None => {
                observed_pw_ver = password_version.load(Ordering::Acquire);
                observed_cfg_ver = config_version.load(Ordering::Acquire);
                let cur_pw = password_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .password
                    .clone();
                let cur_auth = auth_state.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if cur_auth.accept_mtls_lan && cur_auth.mtls_config.is_none() {
                    HostListener::AwaitingMtlsCert
                } else {
                    // 前一轮 `HostSession` / `QuicEndpoint` 在 tokio 后台异步释放 UDP 端口，
                    // 立即重绑 `actual_addr` 可能短暂遇到 `AddrInUse`；短退避重试至多 10 次（~200ms）。
                    let mut last_err = String::new();
                    let mut rebuilt = None;
                    for attempt in 0..10 {
                        if stop_flag.load(Ordering::Acquire) {
                            break;
                        }
                        let res = if cur_auth.accept_mtls_lan {
                            match cur_auth.mtls_config.as_ref() {
                                Some(bundle) => bind_mtls_lan_endpoint(actual_addr, bundle)
                                    .map(|endpoint| HostListener::MtlsLan { endpoint }),
                                None => {
                                    Err("启用 accept_mtls_lan 时必须提供 mtls_config".to_string())
                                }
                            }
                        } else {
                            let box_approver: Box<dyn ConnectionApprover> = if opts.require_approval
                            {
                                Box::new(approver.clone())
                            } else {
                                Box::new(AutoApprover)
                            };
                            build_host_session(
                                actual_addr,
                                cur_pw.clone(),
                                opts.approval_timeout_us,
                                opts.prefer_hardware,
                                opts.clipboard_enabled,
                                cur_auth.allow_remote_assist,
                                cur_auth.owner_verifier.clone(),
                                Arc::clone(&cur_auth.owner_replay_cache),
                                box_approver,
                                Arc::clone(&updates),
                            )
                            .map(|b| HostListener::Plain(Box::new(b)))
                        };
                        match res {
                            Ok(l) => {
                                rebuilt = Some(l);
                                break;
                            }
                            Err(e) => {
                                last_err = e;
                                if attempt < 9 {
                                    thread::sleep(Duration::from_millis(20));
                                }
                            }
                        }
                    }
                    match rebuilt {
                        Some(l) => l,
                        None => {
                            if !stop_flag.load(Ordering::Acquire) {
                                updates.push(HostUpdate::Error { message: last_err });
                            }
                            break;
                        }
                    }
                }
            }
        };

        match listener {
            HostListener::Plain(built) => {
                let BuiltUiHost {
                    mut host,
                    clock,
                    clock_origin,
                    backend_label,
                    hardware_video_active,
                } = *built;

                // 1) 分片 accept + 握手循环（同时轮询信令下发的入站中继/P2P 会话）。
                let accepted = loop {
                    if stop_flag.load(Ordering::Acquire) {
                        break false;
                    }
                    if let Some(incoming) =
                        incoming_relay.pop_ready(RELAY_INCOMING_TTL, Duration::from_millis(180))
                    {
                        serve_relay_incoming(
                            incoming,
                            actual_addr,
                            &display_addr,
                            opts,
                            &updates,
                            &approver,
                            &password_state,
                            &password_version,
                            &auth_state,
                            &active_relay_interrupt,
                            &disconnect_flag,
                            &stop_flag,
                            &lock_screen_count,
                        );
                        // M6：中继会话结束后清理过期入站令牌，并重建空闲 LAN HostSession 清除期间积压的过期 QUIC 连接。
                        incoming_relay.purge_expired(RELAY_INCOMING_TTL);
                        break false;
                    }
                    if password_version.load(Ordering::Acquire) != observed_pw_ver
                        || config_version.load(Ordering::Acquire) != observed_cfg_ver
                    {
                        // 密码或鉴权配置在等待期间被刷新：重建 HostSession 使新配置对下一次握手生效。
                        break false;
                    }
                    // 每轮 accept 分片前重锚 Pending 墙钟，确保审批超时从连接接入时起算。
                    *clock_origin.lock().unwrap_or_else(|e| e.into_inner()) =
                        ClockOrigin::Pending(Instant::now());

                    match host.accept_and_handshake(HOST_ACCEPT_SLICE) {
                        Ok(_) => break true,
                        Err(rc_types::Error::Timeout(_)) if host.state() == SessionState::Init => {
                            continue;
                        }
                        Err(_) => {
                            // 鉴权失败 / 机主拒绝 / 握手超时 / 对端中途断开：本轮结束，回到监听。
                            break false;
                        }
                    }
                };

                if !accepted {
                    drop(host);
                    continue;
                }

                // 2) 进入串流会话（U-H1：显式驱动循环，覆盖 Streaming 与 Detached 退出）。
                disconnect_flag.store(false, Ordering::Release);
                let remote_addr = host.peer_addr().to_string();
                emit_peer_connected(&host, &remote_addr, &updates);

                let reason = drive_ui_host_session_inner(
                    &mut host,
                    &clock,
                    &updates,
                    &password_version,
                    observed_pw_ver,
                    &disconnect_flag,
                    &stop_flag,
                    HostTransportKind::Lan,
                    opts.prefer_hardware,
                    opts.audio_enabled,
                    hardware_video_active.load(Ordering::Acquire),
                    &lock_screen_count,
                );
                let lan_peer_did = host
                    .authenticated_owner()
                    .map(|o| o.caller_device_id.clone());
                let last_backend = backend_label
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                disconnect_flag.store(false, Ordering::Release);
                drop(host);

                // M7：LAN 会话结束后原子轮换临时密码，再通知 UI 并清理同对端残留及过期入站中继令牌（M6/M12）。
                maybe_rotate_password_after_session(
                    &password_state,
                    &password_version,
                    &updates,
                    &stop_flag,
                );
                updates.push(HostUpdate::ClientDisconnected { reason });
                if !stop_flag.load(Ordering::Acquire) {
                    updates.push(HostUpdate::Listening {
                        listen_addr: actual_addr.to_string(),
                        display_addr: display_addr.clone(),
                        backend: last_backend,
                    });
                }
                incoming_relay.purge_for_peer(lan_peer_did.as_deref(), RELAY_INCOMING_TTL);
            }
            HostListener::AwaitingMtlsCert => {
                while !stop_flag.load(Ordering::Acquire) {
                    if let Some(incoming) =
                        incoming_relay.pop_ready(RELAY_INCOMING_TTL, Duration::from_millis(180))
                    {
                        updates.push(HostUpdate::Error {
                            message: format!(
                                "已开启加密连接但缺少 mTLS 设备证书，已拒绝入站会话 ({})",
                                incoming.peer_id
                            ),
                        });
                        incoming_relay.purge_expired(RELAY_INCOMING_TTL);
                        continue;
                    }
                    if config_version.load(Ordering::Acquire) != observed_cfg_ver {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            HostListener::MtlsLan { endpoint } => {
                // mTLS LAN 监听循环：保持 endpoint 复用，直到需要切回 Plain、config_version 变更或收到信令入站会话。
                let mut need_rebind = false;
                while !stop_flag.load(Ordering::Acquire) {
                    if let Some(incoming) =
                        incoming_relay.pop_ready(RELAY_INCOMING_TTL, Duration::from_millis(180))
                    {
                        serve_relay_incoming(
                            incoming,
                            actual_addr,
                            &display_addr,
                            opts,
                            &updates,
                            &approver,
                            &password_state,
                            &password_version,
                            &auth_state,
                            &active_relay_interrupt,
                            &disconnect_flag,
                            &stop_flag,
                            &lock_screen_count,
                        );
                        incoming_relay.purge_expired(RELAY_INCOMING_TTL);
                        continue;
                    }
                    if config_version.load(Ordering::Acquire) != observed_cfg_ver {
                        need_rebind = true;
                        break;
                    }
                    observed_pw_ver = password_version.load(Ordering::Acquire);
                    match endpoint.accept(HOST_ACCEPT_SLICE) {
                        Ok(quic_transport) => {
                            disconnect_flag.store(false, Ordering::Release);
                            let mtls_peer_did = quic_transport
                                .peer_mtls_identity()
                                .map(|id| id.device_id.clone());
                            let peer_label = match mtls_peer_did.as_deref() {
                                Some(did) => format!(
                                    "设备 {} (LAN mTLS @ {})",
                                    did,
                                    quic_transport.remote_addr()
                                ),
                                None => format!("{} (LAN mTLS)", quic_transport.remote_addr()),
                            };
                            match build_and_handshake_transport(
                                Box::new(quic_transport),
                                &peer_label,
                                actual_addr,
                                opts,
                                &updates,
                                &approver,
                                &password_state,
                                &password_version,
                                &auth_state,
                                &disconnect_flag,
                                &stop_flag,
                            ) {
                                HandshakeTransportOutcome::Ready {
                                    built,
                                    streaming_started,
                                    live_pw_ver,
                                } => {
                                    let BuiltUiHost {
                                        mut host,
                                        clock,
                                        clock_origin: _,
                                        backend_label,
                                        hardware_video_active,
                                    } = *built;
                                    streaming_started.store(true, Ordering::Release);
                                    emit_peer_connected(&host, &peer_label, &updates);
                                    let reason = drive_ui_host_session_inner(
                                        &mut host,
                                        &clock,
                                        &updates,
                                        &password_version,
                                        live_pw_ver,
                                        &disconnect_flag,
                                        &stop_flag,
                                        HostTransportKind::MtlsLan,
                                        opts.prefer_hardware,
                                        opts.audio_enabled,
                                        hardware_video_active.load(Ordering::Acquire),
                                        &lock_screen_count,
                                    );
                                    let last_backend = backend_label
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .clone();
                                    disconnect_flag.store(false, Ordering::Release);
                                    drop(host);
                                    maybe_rotate_password_after_session(
                                        &password_state,
                                        &password_version,
                                        &updates,
                                        &stop_flag,
                                    );
                                    updates.push(HostUpdate::ClientDisconnected { reason });
                                    if !stop_flag.load(Ordering::Acquire) {
                                        updates.push(HostUpdate::Listening {
                                            listen_addr: actual_addr.to_string(),
                                            display_addr: display_addr.clone(),
                                            backend: last_backend,
                                        });
                                    }
                                    incoming_relay.purge_for_peer(
                                        mtls_peer_did.as_deref(),
                                        RELAY_INCOMING_TTL,
                                    );
                                }
                                HandshakeTransportOutcome::Failed(_)
                                | HandshakeTransportOutcome::BuildError => {
                                    disconnect_flag.store(false, Ordering::Release);
                                }
                            }
                        }
                        Err(rc_types::Error::Timeout(_)) => continue,
                        Err(_) => {
                            need_rebind = true;
                            break;
                        }
                    }
                }
                drop(endpoint);
                if !need_rebind && stop_flag.load(Ordering::Acquire) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_code_is_8_unambiguous_alnum() {
        let forbidden = ['0', 'O', '1', 'I'];
        for _ in 0..32 {
            let code = generate_temp_code();
            assert_eq!(code.len(), TEMP_CODE_LEN);
            assert!(code
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
            for bad in forbidden {
                assert!(
                    !code.contains(bad),
                    "验证码不应包含易混淆字符 {bad}: {code}"
                );
            }
        }
    }

    #[test]
    fn ui_approver_enforces_monotonic_generation_and_emits_updates() {
        let updates = Arc::new(BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP));
        let mut approver = UiApprover::new(Arc::clone(&updates), false);
        let req = ApprovalRequest {
            session_id: 77,
            remote_addr: "127.0.0.1:50000".parse().unwrap(),
            requested: SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 12_000_000,
            },
            auth_method: rc_session::proto::AuthMethod::Password,
            password_verified: true,
        };

        // 第 1 次 submit → generation = 1，发射 ApprovalRequested。
        approver.submit(&req);
        match updates.try_pop() {
            Some(HostUpdate::ApprovalRequested(p)) => {
                assert_eq!(p.generation, 1);
                assert_eq!(p.session_id, 77);
                assert_eq!(p.requested_res, "1920x1080");
                assert_eq!(p.requested_fps, 60);
                assert!(p.password_verified);
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }

        // 错误代次被拒绝。
        assert!(!approver.resolve(999, true));
        assert_eq!(approver.poll_verdict(), None);

        // cancel 发射 ApprovalCancelled 并清除在途态。
        approver.cancel();
        assert_eq!(
            updates.try_pop(),
            Some(HostUpdate::ApprovalCancelled { generation: 1 })
        );
        // 过期代次 resolve 无效。
        assert!(!approver.resolve(1, true));

        // 第 2 次 submit → generation = 2，正确代次 resolve 生效。
        approver.submit(&req);
        match updates.try_pop() {
            Some(HostUpdate::ApprovalRequested(p)) => assert_eq!(p.generation, 2),
            other => panic!("expected second ApprovalRequested, got {other:?}"),
        }
        assert!(approver.resolve(2, true));
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Accepted));
    }

    /// 回归测试（U-M2）：`UiApprover::mark_stopped()` 置位粘性停机标记后，
    /// 后续 `submit()` 不再发射 `ApprovalRequested`，`poll_verdict()` 立即返回 `Rejected`，
    /// `resolve()` 恒返回 `false`。
    #[test]
    fn ui_approver_sticky_stop_rejects_future_submits_and_polls() {
        let updates = Arc::new(BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP));
        let mut approver = UiApprover::new(Arc::clone(&updates), false);
        let req = ApprovalRequest {
            session_id: 88,
            remote_addr: "127.0.0.1:50001".parse().unwrap(),
            requested: SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 12_000_000,
            },
            auth_method: rc_session::proto::AuthMethod::None,
            password_verified: false,
        };

        // 停机前先标记 stopped。
        approver.mark_stopped();

        // 停机后再 submit：绝不推送 ApprovalRequested 弹窗。
        approver.submit(&req);
        assert_eq!(updates.try_pop(), None, "停机后不得发射 ApprovalRequested");

        // poll_verdict 立即返回 Rejected，resolve 返回 false。
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Rejected));
        assert!(!approver.resolve(1, true));
    }

    #[test]
    fn ui_approver_auto_accepts_password_verified_without_emitting_prompt() {
        let updates = Arc::new(BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP));
        let mut approver = UiApprover::new(Arc::clone(&updates), true);
        let req_verified = ApprovalRequest {
            session_id: 91,
            remote_addr: "127.0.0.1:50010".parse().unwrap(),
            requested: SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 12_000_000,
            },
            auth_method: rc_session::proto::AuthMethod::Password,
            password_verified: true,
        };

        // 密码已校验且启用 auto_accept_with_password：立即放行且绝不推送审批弹窗。
        approver.submit(&req_verified);
        assert_eq!(
            updates.try_pop(),
            None,
            "密码验证通过自动放行时不得推送 ApprovalRequested"
        );
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Accepted));
        assert_eq!(updates.try_pop(), None);

        // 若在 poll_verdict 之前调用 cancel，因未弹窗也不应推送冗余 ApprovalCancelled。
        approver.submit(&req_verified);
        approver.cancel();
        assert_eq!(updates.try_pop(), None);

        // 无密码连接（password_verified == false）：仍须推送 ApprovalRequested 并等待人工裁决。
        let req_passwordless = ApprovalRequest {
            session_id: 92,
            remote_addr: "127.0.0.1:50011".parse().unwrap(),
            requested: SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 12_000_000,
            },
            auth_method: rc_session::proto::AuthMethod::None,
            password_verified: false,
        };
        approver.submit(&req_passwordless);
        let generation = match updates.try_pop() {
            Some(HostUpdate::ApprovalRequested(p)) => {
                assert_eq!(p.session_id, 92);
                assert!(!p.password_verified);
                p.generation
            }
            other => panic!("无密码连接必须推送 ApprovalRequested，实际: {other:?}"),
        };
        assert_eq!(approver.poll_verdict(), None);
        assert!(approver.resolve(generation, true));
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Accepted));

        // 停机后即使 password_verified == true 也必须拒绝，绝不放行。
        approver.mark_stopped();
        approver.submit(&req_verified);
        assert_eq!(updates.try_pop(), None);
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Rejected));
    }

    /// 回归测试（U-H1）：`drive_ui_host_session` 在 `Detached` 态下：
    /// 1) 重连保持窗口到期（`DetachOutcome::TornDown`）时立刻退出并关闭会话；
    /// 2) 密码版本变更（`password_version != observed_pw_ver`）时立刻退出并关闭会话。
    #[test]
    fn drive_ui_host_session_breaks_on_detached_torn_down_and_password_change() {
        use rc_session::proto::{AuthMethod, ControlMessage, Hello, PROTOCOL_VERSION};
        use rc_session::testkit::MockTransport;
        use rc_session::ReconnectConfig;
        use std::sync::atomic::AtomicU32;

        fn build_handshaken_offline(clock: SessionClock) -> HostSession {
            let requested = SessionParams {
                width: 320,
                height: 240,
                fps: 30,
                bitrate_bps: 2_000_000,
            };
            let hello_msg = ControlMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                session_id: 1,
                requested,
                auth_method: AuthMethod::None,
                auth_payload: Vec::new(),
            });

            let mut mock = MockTransport::new();
            mock.deliver_reliable(&hello_msg);

            let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
                display: rc_capture::DisplayInfo {
                    id: rc_types::DisplayId(0),
                    resolution: Resolution::new(320, 240),
                    scale: 1.0,
                    hdr: false,
                },
                fps: 30,
                content: rc_capture::FakeContent::Static,
                dirty_rect: None,
            })
            .expect("fake capture");
            let encoder = rc_encode::FakeEncoder::new(rc_encode::EncoderConfig {
                resolution: Resolution::new(320, 240),
                fps: 30,
                bitrate_bps: 2_000_000,
                ..rc_encode::EncoderConfig::default()
            })
            .expect("fake encoder");

            let cfg = HostSessionConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                mtu: rc_session::DEFAULT_SESSION_MTU,
                caps: requested,
                pipeline: PipelineConfig {
                    resolution: Resolution::new(320, 240),
                    fps: 30,
                    bitrate_bps: 2_000_000,
                    prefer_hardware: false,
                    target_latency_us: 22_000,
                    trace_capacity: 64,
                    diff: DiffMode::TrustPlatform,
                    clock_origin_us: None,
                    max_packet_payload: rc_pipeline::MAX_PAYLOAD,
                },
                handshake_timeout: Duration::from_secs(5),
                injector: Box::new(RecordingInjector::new()),
                pace_capture: false,
                auth: HostAuthConfig::default(),
                approver: Box::new(AutoApprover),
            };
            let mut host = HostSession::with_parts(
                cfg,
                Box::new(mock),
                Box::new(capture),
                Box::new(encoder),
                clock,
            )
            .expect("with_parts");
            host.set_reconnect(ReconnectConfig {
                resume_ttl: Duration::from_millis(50),
                ..ReconnectConfig::default()
            })
            .expect("set_reconnect");
            host.handshake().expect("handshake");
            assert_eq!(host.state(), SessionState::Streaming);
            host
        }

        // 场景 A：Detached 窗口到期（TornDown）立即跳出驱动循环。
        let now_us = Arc::new(AtomicU32::new(10_000));
        let clk_a: SessionClock = {
            let n = Arc::clone(&now_us);
            Arc::new(move || n.load(Ordering::Acquire))
        };
        let mut host_a = build_handshaken_offline(Arc::clone(&clk_a));
        host_a.on_disconnected(10_000);
        assert_eq!(host_a.state(), SessionState::Detached);
        // 推进注入时钟超过 50ms resume_ttl。
        now_us.store(100_000, Ordering::Release);

        let updates_a = BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP);
        let pw_ver_a = AtomicU64::new(0);
        let disc_a = AtomicBool::new(false);
        let stop_a = AtomicBool::new(false);
        let reason_a = drive_ui_host_session(
            &mut host_a,
            &clk_a,
            &updates_a,
            &pw_ver_a,
            0,
            &disc_a,
            &stop_a,
        );
        assert!(
            reason_a.contains("重连超时"),
            "TornDown 应返回重连超时原因，实际: {reason_a}"
        );
        assert_eq!(host_a.state(), SessionState::Closed);

        // 场景 B：Detached 期间密码版本变更，立即跳出以便重建监听。
        let clk_b: SessionClock = Arc::new(|| 10_000);
        let mut host_b = build_handshaken_offline(Arc::clone(&clk_b));
        host_b.on_disconnected(10_000);
        assert_eq!(host_b.state(), SessionState::Detached);

        let updates_b = BoundedHostUpdateQueue::new(HOST_UPDATE_CHANNEL_CAP);
        let pw_ver_b = AtomicU64::new(1); // 与 observed_pw_ver = 0 不一致
        let disc_b = AtomicBool::new(false);
        let stop_b = AtomicBool::new(false);
        let reason_b = drive_ui_host_session(
            &mut host_b,
            &clk_b,
            &updates_b,
            &pw_ver_b,
            0,
            &disc_b,
            &stop_b,
        );
        assert!(
            reason_b.contains("密码已变更"),
            "Detached 期间密码变更应立即跳出，实际: {reason_b}"
        );
        assert_eq!(host_b.state(), SessionState::Closed);
    }

    /// 回归测试（U-M1）：`HostHandle::spawn` 返回后立即调用 `set_password`，
    /// 工作线程因 `observed_pw_ver = 0` 必然检测到版本变化并使新密码生效。
    #[test]
    fn set_password_immediately_after_spawn_is_observed_by_worker() {
        let mut host = HostHandle::spawn(UiHostConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            password: Some("OLD-PASS-1".to_string()),
            approval_timeout_us: rc_session::auth::DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware: false,
            require_approval: false,
            auto_accept_with_password: false,
            auto_rotate_password: false,
            clipboard_enabled: false,
            audio_enabled: false,
            ..UiHostConfig::default()
        })
        .expect("spawn host");
        // 紧接 spawn 立即改密，覆盖 spawn → worker 启动竞态窗口。
        host.set_password(Some("NEW-PASS-2".to_string()));
        let addr = host.listen_addr();

        // 等待工作线程完成至多一轮 accept 分片（500ms）并重建 HostSession 使新密码生效。
        thread::sleep(Duration::from_millis(650));

        let client =
            crate::session_bridge::SessionHandle::spawn(crate::session_bridge::UiSessionConfig {
                connect_addr: addr,
                password: Some("NEW-PASS-2".to_string()),
                watch_only: true,
                width: 640,
                height: 360,
                fps: 30,
                bitrate_bps: 2_000_000,
                prefer_hardware: false,
                clipboard_enabled: false,
                audio_enabled: false,
                force_relay: false,
                relay_grant: None,
                ..crate::session_bridge::UiSessionConfig::default()
            });
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut negotiated = false;
        while Instant::now() < deadline {
            if let Some(upd) = client.try_recv_update() {
                match upd {
                    crate::session_bridge::SessionUpdate::Negotiated { .. } => {
                        negotiated = true;
                        break;
                    }
                    crate::session_bridge::SessionUpdate::Ended { reason } => {
                        panic!("使用新密码握手不应失败: {reason}");
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        client.send(crate::session_bridge::SessionCommand::Disconnect);
        client.join();
        host.stop();
        assert!(negotiated, "紧接 spawn 调用 set_password 后新密码必须生效");
    }

    #[test]
    fn flush_tool_channel_with_retry_preserves_order_and_drops_nothing_on_backpressure() {
        let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
        let mut source: VecDeque<Vec<u8>> = (1u8..=5).map(|i| vec![i]).collect();
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut poll_calls = 0usize;

        // 第 1 拍：拉出前 3 帧，第 2 帧遇到背压返回 Err -> 第 1 帧发出，第 2、3 帧留在 pending。
        let mut attempt = 0usize;
        let busy1 = flush_tool_channel_with_retry(
            &mut pending,
            3,
            |n| {
                poll_calls += 1;
                (0..n).filter_map(|_| source.pop_front()).collect()
            },
            |frame| {
                attempt += 1;
                if attempt == 2 {
                    Err("backpressure")
                } else {
                    sent.push(frame);
                    Ok(())
                }
            },
        );
        assert!(busy1);
        assert_eq!(poll_calls, 1);
        assert_eq!(sent, vec![vec![1]]);
        assert_eq!(pending.len(), 2);

        // 第 2 拍：继续背压 -> 不得再次 poll 引擎，pending 保持 2 帧。
        let busy2 = flush_tool_channel_with_retry(
            &mut pending,
            3,
            |_| {
                poll_calls += 1;
                Vec::new()
            },
            |_| Err::<(), _>("still busy"),
        );
        assert!(!busy2);
        assert_eq!(poll_calls, 1);
        assert_eq!(pending.len(), 2);

        // 第 3 拍：背压解除 -> 先发出 2、3（仍不 poll 新帧）；第 4 拍再拉出 4、5 全部送达。
        let busy3 = flush_tool_channel_with_retry(
            &mut pending,
            3,
            |_| {
                poll_calls += 1;
                Vec::new()
            },
            |frame| {
                sent.push(frame);
                Ok::<(), ()>(())
            },
        );
        assert!(busy3);
        assert_eq!(poll_calls, 1);
        assert!(pending.is_empty());

        let busy4 = flush_tool_channel_with_retry(
            &mut pending,
            3,
            |n| {
                poll_calls += 1;
                (0..n).filter_map(|_| source.pop_front()).collect()
            },
            |frame| {
                sent.push(frame);
                Ok::<(), ()>(())
            },
        );
        assert!(busy4);
        assert_eq!(poll_calls, 2);
        assert_eq!(sent, vec![vec![1], vec![2], vec![3], vec![4], vec![5]]);
    }

    #[test]
    fn p2p_handshake_fallback_eligibility_matches_transport_errors_only() {
        assert!(is_p2p_handshake_fallback_eligible(
            &rc_types::Error::Timeout(Duration::from_secs(5))
        ));
        assert!(is_p2p_handshake_fallback_eligible(
            &rc_types::Error::Network("peer disconnected".to_string())
        ));
        assert!(is_p2p_handshake_fallback_eligible(&rc_types::Error::Io(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")
        )));
        assert!(!is_p2p_handshake_fallback_eligible(
            &rc_types::Error::Protocol {
                offset: 0,
                msg: "authentication rejected".to_string(),
            }
        ));
        assert!(!is_p2p_handshake_fallback_eligible(
            &rc_types::Error::InvalidConfig("bad cfg".to_string())
        ));
    }

    #[test]
    fn file_host_worker_handles_list_dir_off_thread_and_stops_cleanly() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-file-host-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        std::fs::write(dir.join("probe.txt"), b"hello").expect("write probe file");

        let worker = FileHostWorker::spawn();
        let req = rc_tools::FileWireMsg::ListDir {
            path: Some(dir.to_string_lossy().into_owned()),
        }
        .encode()
        .expect("encode ListDir");
        worker
            .try_send_incoming(req)
            .expect("send incoming ListDir to worker");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut resp_frame = None;
        while Instant::now() < deadline {
            let frames = worker.try_recv_outgoing(4);
            if let Some(f) = frames.into_iter().next() {
                resp_frame = Some(f);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let frame = resp_frame.expect("worker must reply with ListDirResp");
        match rc_tools::FileWireMsg::decode(&frame).expect("decode ListDirResp") {
            rc_tools::FileWireMsg::ListDirResp { entries, error, .. } => {
                assert!(error.is_none(), "unexpected list_dir error: {error:?}");
                assert!(entries.iter().any(|e| e.name == "probe.txt"));
            }
            other => panic!("expected ListDirResp, got {other:?}"),
        }

        drop(worker);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
