//! UI ↔ 会话层唯一通道（T21 API 契约 §1）。
//!
//! 本模块**严禁引入 `tauri` 类型**，仅依赖 `std` + `rc-session` + `rc-net` +
//! `rc-pipeline` + `rc-input` + `rc-render` + `rc-decode` + `rc-trace`，
//! 从而使 `cargo test -p rc-ui` 的离线单测与进程内集成测试无需窗口与事件循环。
//!
//! # 硬性纪律
//! - 全模块 `#![deny(unsafe_code)]`（`CAMetalLayer` 借用指针以 `usize` 传递，不解引用）；
//! - 铁律 1（UI 侧延伸 / U6）：命令通道容量 `64`、输入通道容量 `256`、更新通道容量 `64`，
//!   满载一律非阻塞丢最旧；输入通道满载时优先保留关键事件（`Key` / `MouseButton` / `MouseWheel`），
//!   优先淘汰非关键的 `MouseMove` / `Sync`；
//! - U3：会话协议零改动，S3 手动调码率仅发送既有 `SetBitrate { reason: Manual }`；
//! - U5：HUD 数字全部由会话线程调用 [`HudSnapshot::derive`] 差分聚合后推送；
//! - U8：使用 `rc_session::dev_cert` 零配置直连，`ConnectPhase` 起点在 Endpoint 创建之前；
//! - U9：`watch_only == true` 时双层拦截输入（`send_input` + `BridgeInputCapture`）；
//! - U10：`join` 与 `Drop` 均自动发 `Disconnect` 并回收会话线程，不留僵尸线程。

#![deny(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rc_audio::{decode_ima_adpcm, AudioPlayer, NullAudioPlayer, PcmFrame};
use rc_input::{InputCapture, InputEvent};
use rc_net::quic::{Identity, QuicEndpoint};
use rc_net::{NetConfig, NetEvent, NetStats, Role, Transport};
use rc_pipeline::{ControllerPipeline, DiffMode, PipelineConfig};
use rc_render::Renderer;
use rc_session::auth::OwnerTicketSigner;
use rc_session::dev_cert::{dev_server_cert_pem, DEV_CERT_PEM, DEV_KEY_PEM, DEV_SERVER_NAME};
use rc_session::proto::{BitrateReason, ByeReason, ControlMessage, SessionParams};
use rc_session::{
    AudioFramePacket, ClientAuthConfig, ClientSession, ClientSessionConfig, ClipboardBackend,
    ReconnectOutcome, SessionClock, SessionState, AUDIO_CODEC_IMA_ADPCM, AUDIO_CODEC_PCM16,
    TOOL_CHANNEL_AUDIO_CTRL, TOOL_CHANNEL_DISPLAY_CTRL, TOOL_CHANNEL_FILE, TOOL_CHANNEL_PORTMAP,
    TOOL_CHANNEL_TERMINAL,
};
use rc_tools::{
    FileClientEngine, FileEvent, PortMapClientEngine, PortMapRuleSnapshot, TerminalClientEngine,
    TerminalEvent, TransferDirection, TransferItem, TransferStatus,
};
use rc_types::Resolution;

use crate::cert_manager::OwnerGrantPayload;
use crate::display_ctrl::{self, DisplayControlMsg, DisplayPreset};
use crate::hud::{HudSample, HudSnapshot};
use crate::lan_discovery;
use crate::metal_attach::MainThreadMetalLayer;
use crate::p2p::{self, P2pCandidates};
pub use crate::relay_transport::SessionMtlsBundle;
use crate::relay_transport::{
    E2eeRelayTransport, RelayE2eeMode, RelayRole, RelayShutdownHandle, TcpRelayTransport,
};
use crate::signal_client::ConnectGrant;

/// 命令通道容量上限（T21 U6 / 铁律 1 UI 侧延伸）。
pub const COMMAND_CHANNEL_CAP: usize = 64;

/// 输入通道容量上限（T21 U6，对齐 `rc-input-macos` `CgCapture` tap 通道容量）。
pub const INPUT_CHANNEL_CAP: usize = 256;

/// 会话线程 → UI 状态更新通道容量上限（T21 U6）。
pub const UPDATE_CHANNEL_CAP: usize = 64;

/// 常规 poll 预算（对齐 `rc-cli/src/cmd/client.rs`）。
const RUN_POLL_BUDGET: Duration = Duration::from_millis(1);
/// 空闲拍放宽的 poll 预算（4ms 以支撑 120/144fps 高刷唤醒不卡顿）。
const RUN_IDLE_POLL_BUDGET: Duration = Duration::from_millis(4);
/// `Reconnecting` 退避等待的最大睡眠粒度。
const RUN_RECONNECT_SLEEP: Duration = Duration::from_millis(20);
/// HUD 推送节流周期（1 秒）。
const HUD_INTERVAL: Duration = Duration::from_secs(1);
/// 硬件解码器在 `displayed == 0` 期间连续 `Outcome::DecodeError` 的判定阈值
/// （例如远端被控端未授屏幕录制权限而降级到 `FakeEncoder` 发送非 Annex-B 码流）。
const CONSECUTIVE_DECODE_ERROR_LIMIT: usize = 30;
/// 触发持续硬件解码失败终止前的最短串流观察时长（避免首帧短暂丢包误判）。
const PERSISTENT_DECODE_ERROR_MIN_ELAPSED: Duration = Duration::from_secs(3);
/// 最近窗口用于计算 `e2e_p50_ms` 的最大样本数。
const E2E_WINDOW_FRAMES: usize = 60;
/// W2 文件传输事件队列容量上限（有界 drop-oldest）。
const FILE_EVENT_CHANNEL_CAP: usize = 256;
/// W3 远程终端事件队列容量上限（有界 drop-oldest）。
const TERMINAL_EVENT_CHANNEL_CAP: usize = 512;
/// 单拍每个工具引擎最多发送的线帧数（避免工具吞吐饿死视频/输入）。
const MAX_TOOL_FRAMES_PER_TICK: usize = 8;
/// 控制端专用文件工作线程的有界通道容量（H12）。
const FILE_CLIENT_WORKER_CHAN_CAP: usize = 64;
/// 远程终端允许的最大行列尺寸（M10）。
const MAX_TERMINAL_DIM: u16 = 512;
/// 单次远程终端输入字节上限（64 KiB，M10）。
const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
/// 上传进度推送最小间隔（平滑刷新 W2 进度条，不刷屏）。
const UPLOAD_PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(100);
/// W4 端口映射统计快照比对间隔。
const PORTMAP_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);

fn default_true() -> bool {
    true
}

/// UI 会话配置（P2 / S1 连接表单的并集）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct UiSessionConfig {
    /// 对端地址（`ip:port` 直连，M2 无信令；由 `model::parse_connect_target` 校验）。
    pub connect_addr: SocketAddr,
    /// 可选访问密码（`None` = 无密码握手；`Some` = 启用 PBKDF2 挑战应答）。
    #[serde(default)]
    pub password: Option<String>,
    /// 观看模式：不 pump 输入（U9）。
    pub watch_only: bool,
    /// 请求画面宽度（像素；实际以 Welcome 裁定为准）。
    pub width: u32,
    /// 请求画面高度（像素；实际以 Welcome 裁定为准）。
    pub height: u32,
    /// 请求帧率（fps；实际以 Welcome 裁定为准）。
    pub fps: u32,
    /// 请求码率（bps；实际以 Welcome 裁定为准）。
    pub bitrate_bps: u32,
    /// 优先硬件后端（VT 解码 + Metal 呈现），失败降级假后端并如实上报标签。
    pub prefer_hardware: bool,
    /// 是否启用双向剪贴板同步（默认 `true`；仅 `prefer_hardware == true` 时挂接本机真实系统剪贴板）。
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    /// 是否启用远端音频流接收与播放（默认 `true`）。
    #[serde(default = "default_true")]
    pub audio_enabled: bool,
    /// 可选中继/P2P 会话凭据（`Some` 时按 LAN → P2P → 中继三阶路由建连，忽略 `connect_addr`）。
    #[serde(skip)]
    pub relay_grant: Option<ConnectGrant>,
    /// 是否强制跳过 LAN / P2P，直接使用 `TCP Relay` 中继连接（仅在存在 `relay_grant` 时生效）。
    #[serde(default)]
    pub force_relay: bool,
    /// 是否开启端到端双向 mTLS / E2EE 加密连接（默认 `false` 兼容旧离线无证书单测；由上层配置注入）。
    #[serde(default)]
    pub encrypt_connection: bool,
    /// 双向 mTLS 证书与身份校验束（`encrypt_connection == true` 时必填；存在时亦用于 `:21117` 外层 mTLS 中转）。
    #[serde(skip)]
    pub mtls_config: Option<SessionMtlsBundle>,
    /// 可选本人同账号自控票据载荷（随信令 `PunchCoord`/`RelayReady` 下发，配合 `owner_signer` 做 PoP）。
    #[serde(skip)]
    pub owner_grant: Option<OwnerGrantPayload>,
    /// 同账号本人自控 `OwnerTicket` 签名器（存在时向被控端发起免密免审批 PoP 鉴权）。
    #[serde(skip)]
    pub owner_signer: Option<Arc<dyn OwnerTicketSigner>>,
    /// 局域网多播发现命中的同局域网被控端直连地址（三阶路由 Tier 1 `UDP LAN` 优先探测）。
    #[serde(default)]
    pub lan_direct_addr: Option<SocketAddr>,
}

impl Default for UiSessionConfig {
    fn default() -> Self {
        Self {
            connect_addr: SocketAddr::from(([127, 0, 0, 1], 9901)),
            password: None,
            watch_only: false,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 14_000_000,
            prefer_hardware: false,
            clipboard_enabled: true,
            audio_enabled: true,
            relay_grant: None,
            force_relay: false,
            encrypt_connection: false,
            mtls_config: None,
            owner_grant: None,
            owner_signer: None,
            lan_direct_addr: None,
        }
    }
}

impl UiSessionConfig {
    /// 返回切换了 `encrypt_connection` 开关且保留全部其余会话参数的副本（供会话中动态重连切换 mTLS）。
    pub fn with_encrypt_connection(&self, encrypt_connection: bool) -> Self {
        let mut cloned = self.clone();
        cloned.encrypt_connection = encrypt_connection;
        cloned
    }
}

impl fmt::Debug for UiSessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UiSessionConfig")
            .field("connect_addr", &self.connect_addr)
            .field("has_password", &self.password.is_some())
            .field("watch_only", &self.watch_only)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .field("bitrate_bps", &self.bitrate_bps)
            .field("prefer_hardware", &self.prefer_hardware)
            .field("clipboard_enabled", &self.clipboard_enabled)
            .field("audio_enabled", &self.audio_enabled)
            .field("has_relay_grant", &self.relay_grant.is_some())
            .field("force_relay", &self.force_relay)
            .field("encrypt_connection", &self.encrypt_connection)
            .field("has_mtls_config", &self.mtls_config.is_some())
            .field("has_owner_grant", &self.owner_grant.is_some())
            .field("has_owner_signer", &self.owner_signer.is_some())
            .field("lan_direct_addr", &self.lan_direct_addr)
            .finish()
    }
}

/// UI → 会话线程的低频控制命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCommand {
    /// 断开会话：发 `Bye(Normal)` 正常关闭，线程随后退出。
    Disconnect,
    /// 手动设置码率（S3 画质子菜单 → `SetBitrate { reason: Manual }`）。
    SetBitrate {
        /// 目标码率（bps）。
        bps: u32,
    },
    /// 画面容器矩形变化（物理像素），转发到 `Renderer::resize`。
    StageRect {
        /// 容器宽度（物理像素）。
        width: u32,
        /// 容器高度（物理像素）。
        height: u32,
    },
    /// 动态开启或暂停剪贴板双向同步。
    SetClipboardEnabled {
        /// `true` = 开启同步，`false` = 暂停同步。
        enabled: bool,
    },
    /// 动态静音或恢复远端音频播放，并通知主机端暂停/恢复音频编码。
    SetAudioMuted {
        /// `true` = 静音，`false` = 恢复播放。
        muted: bool,
    },
    /// 热切换远端视频流分辨率与帧率（经 `TOOL_CHANNEL_DISPLAY_CTRL` 下发被控端）。
    SetStreamParams {
        /// 目标视频宽度（像素；`0` 表示保持当前分辨率）。
        width: u32,
        /// 目标视频高度（像素；`0` 表示保持当前分辨率）。
        height: u32,
        /// 目标帧率（fps；`0` 表示保持当前帧率）。
        fps: u32,
    },
    /// 请求立即锁定被控端屏幕（经 `TOOL_CHANNEL_DISPLAY_CTRL` 发送）。
    LockRemoteScreen,
}

/// 连接进度（对齐 T18 计时口径：起点 = Endpoint 创建之前）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectPhase {
    /// 正在创建 QUIC Endpoint（计时起点之后的第一步）。
    EndpointCreating,
    /// QUIC 握手中。
    Connecting,
    /// `Hello` 已发，等待 `Welcome` 裁定。
    AwaitingWelcome,
    /// `Welcome` 已收，等待首帧上屏。
    FirstFramePending,
}

/// 会话线程 → UI 的状态推送（经 Tauri event `session://update` 转发到前端）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// 连接进度变更。
    Connecting {
        /// 当前所处连接阶段。
        phase: ConnectPhase,
        /// 是否观看模式（只读不注入输入，U-M7）。
        watch_only: bool,
    },
    /// `Welcome` 裁定参数（S1 屏幕 tab 文案 / HUD 分辨率行以此为准）。
    Negotiated {
        /// 裁定宽度（像素）。
        width: u32,
        /// 裁定高度（像素）。
        height: u32,
        /// 裁定帧率（fps）。
        fps: u32,
        /// 裁定码率（bps）。
        bitrate_bps: u32,
        /// 实际启用的解码 + 呈现后端标签（如 `"videotoolbox+metal"` / `"fake"` / `"fake(降级)"`）。
        backend: String,
        /// 是否观看模式（只读不注入输入，U-M7）。
        watch_only: bool,
        /// 实际传输通道标签（如 `"UDP LAN"` / `"UDP P2P"` / `"TCP Relay"`）。
        transport: String,
        /// 端到端/QUIC mTLS 加密是否已激活。
        mtls_active: bool,
        /// 对端设备证书 SHA-256 指纹（启用 mTLS/E2EE 且握手提取到对端证书时填充）。
        peer_cert_fingerprint: Option<String>,
    },
    /// HUD 快照（会话线程 1s 节流推送；铁律 4：数字在会话侧算好，前端只格式化）。
    Hud(HudSnapshot),
    /// 被控端显示能力与当前生效流参数（首次入流及每次热切换后由被控端权威回传）。
    DisplayConfig {
        /// 当前生效视频宽度（像素）。
        width: u32,
        /// 当前生效视频高度（像素）。
        height: u32,
        /// 当前生效帧率（fps）。
        fps: u32,
        /// 被控端主显示器原生逻辑宽度（像素）。
        native_width: u32,
        /// 被控端主显示器原生逻辑高度（像素）。
        native_height: u32,
        /// 被控端支持的最高帧率（fps）。
        max_fps: u32,
        /// 严格保持原生宽高比、由高到低排列的分辨率预设列表。
        presets: Vec<DisplayPreset>,
    },
    /// 被控端推送的当前桌面壁纸缩略图（`data:image/jpeg;base64,...` / `data:image/png;base64,...`）。
    Wallpaper {
        /// 内联 Data URL。
        data_url: String,
    },
    /// 会话结束（正常 `Bye` / 对端断开 / 驱动错误 / 线程 panic）。
    Ended {
        /// 人类可读的结束原因。
        reason: String,
    },
}

// ─────────────────────────────────────────────────────────────────
// 有界非阻塞队列（铁律 1 UI 侧延伸：命令 64 / 输入 256 / 更新 64）
// ─────────────────────────────────────────────────────────────────

/// 通用有界 drop-oldest 队列（永不阻塞生产端或消费端）。
struct BoundedQueue<T> {
    cap: usize,
    inner: Mutex<VecDeque<T>>,
}

impl<T> BoundedQueue<T> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    fn push_drop_oldest(&self, item: T) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while q.len() >= self.cap {
            q.pop_front();
        }
        q.push_back(item);
    }

    fn drain_all(&self) -> Vec<T> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    }

    fn try_pop(&self) -> Option<T> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 状态更新专用有界队列：满载时优先丢弃最旧的 `Hud` / `Connecting`，保护 `Negotiated` / `Ended`。
struct BoundedUpdateQueue {
    cap: usize,
    inner: Mutex<VecDeque<SessionUpdate>>,
}

impl BoundedUpdateQueue {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    fn push(&self, item: SessionUpdate) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= self.cap {
            if let Some(pos) = q
                .iter()
                .position(|u| matches!(u, SessionUpdate::Hud(_) | SessionUpdate::Connecting { .. }))
            {
                q.remove(pos);
            } else {
                q.pop_front();
            }
        }
        q.push_back(item);
    }

    fn try_pop(&self) -> Option<SessionUpdate> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }
}

/// 判定一条输入事件是否为关键事件（不可轻易因移动事件挤占而丢弃）。
fn is_critical_input(ev: &InputEvent) -> bool {
    !matches!(ev, InputEvent::MouseMove { .. } | InputEvent::Sync)
}

/// 输入专用有界队列（容量 256）：满载丢最旧的非关键事件，优先保留关键事件（U6）。
struct BoundedInputQueue {
    cap: usize,
    inner: Mutex<VecDeque<InputEvent>>,
}

impl BoundedInputQueue {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            inner: Mutex::new(VecDeque::with_capacity(cap)),
        }
    }

    fn push(&self, ev: InputEvent) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= self.cap {
            if is_critical_input(&ev) {
                // 关键事件：优先挤掉队列中最旧的非关键事件；若全为关键事件则丢最旧。
                if let Some(pos) = q.iter().position(|e| !is_critical_input(e)) {
                    q.remove(pos);
                } else {
                    q.pop_front();
                }
                q.push_back(ev);
            } else {
                // 非关键事件（MouseMove/Sync）：只淘汰最旧的非关键事件；
                // 若队列已满载关键事件则丢弃本条移动事件，不挤占关键输入。
                if let Some(pos) = q.iter().position(|e| !is_critical_input(e)) {
                    q.remove(pos);
                    q.push_back(ev);
                }
            }
        } else {
            q.push_back(ev);
        }
    }

    fn drain_all(&self) -> Vec<InputEvent> {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 由有界输入队列喂给 `ClientSession` 的自定义 `InputCapture`。
struct BridgeInputCapture {
    queue: Arc<BoundedInputQueue>,
    watch_only: Arc<AtomicBool>,
}

impl InputCapture for BridgeInputCapture {
    fn next_events(&mut self, _timeout: Duration) -> rc_types::Result<Vec<InputEvent>> {
        if self.watch_only.load(Ordering::Acquire) {
            let _ = self.queue.drain_all();
            return Ok(Vec::new());
        }
        Ok(self.queue.drain_all())
    }
}

// ─────────────────────────────────────────────────────────────────
// 渲染器与传输层桥接包装（零改动 rc-pipeline / rc-session）
// ─────────────────────────────────────────────────────────────────

/// 共享渲染器句柄（会话线程用于 `StageRect` resize，不改 `ControllerPipeline` API）。
type RendererHandle = Arc<Mutex<Box<dyn Renderer>>>;

/// 保持离屏 `CAMetalLayer` 存活的渲染器租约（纯安全封装，零 `unsafe`）。
///
/// 内部持有底层 [`Renderer`] 的强引用：只要任意 `LayerLease` 存活，底层 `MetalRenderer`
/// 及其 `CAMetalLayer` 就绝不会被释放。跨线程派发到主线程的 `attach_layer` /
/// `sync_layer_frame` / `detach_layer` 闭包持有本租约的 clone，彻底消除主线程闭包与
/// 会话线程退出（含 `Ended` / 窗口销毁 / 并发重连）之间的 use-after-free 竞态。
#[derive(Clone)]
pub struct LayerLease {
    renderer: RendererHandle,
    layer_addr: usize,
}

impl LayerLease {
    /// 由共享渲染器句柄与借用 `CAMetalLayer` 地址构造保活租约；`layer_addr == 0` 返回 `None`。
    pub fn from_renderer(
        renderer: Arc<Mutex<Box<dyn Renderer>>>,
        layer_addr: usize,
    ) -> Option<Self> {
        if layer_addr == 0 {
            None
        } else {
            Some(Self {
                renderer,
                layer_addr,
            })
        }
    }

    /// 返回借用的 `CAMetalLayer` 原始指针（调用方须在持有本租约存活期间于主线程使用）。
    pub fn as_ptr(&self) -> *mut c_void {
        self.layer_addr as *mut c_void
    }

    /// 返回借用地址（仅用于日志/比较，不解引用）。
    pub fn addr(&self) -> usize {
        self.layer_addr
    }

    /// 判断两份租约是否指向同一渲染器实例的同一 `CAMetalLayer`（防地址复用 ABA）。
    pub fn same_layer(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.renderer, &other.renderer) && self.layer_addr == other.layer_addr
    }
}

impl fmt::Debug for LayerLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerLease")
            .field("layer_addr", &format_args!("{:#x}", self.layer_addr))
            .finish()
    }
}

/// 管线工厂构建产物。
struct BuiltPipeline {
    pipeline: ControllerPipeline,
    backend_label: String,
    renderer: RendererHandle,
    layer_addr: usize,
}

/// 共享渲染器包装：让会话线程在不改 `ControllerPipeline` API 的前提下响应 `StageRect` resize。
struct SharedRenderer {
    inner: RendererHandle,
}

impl Renderer for SharedRenderer {
    fn present(&mut self, frame: rc_decode::DecodedFrame) -> rc_types::Result<rc_types::Timestamp> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .present(frame)
    }

    fn resize(&mut self, size: (u32, u32)) -> rc_types::Result<()> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(size)
    }

    fn set_vsync(&mut self, on: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_vsync(on);
    }

    fn set_cursor(&mut self, pos: (i32, i32), visible: bool) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cursor(pos, visible);
    }
}

/// 按平台与硬件偏好构造剪贴板后端：仅 `prefer_hardware == true` 时挂接本机真实系统剪贴板，
/// 离线假后端测试绝不触碰开发者真实剪贴板。
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

/// 可动态开关的剪贴板后端包装（供 `SessionCommand::SetClipboardEnabled` 运行期切换）。
struct ToggleableClipboardBackend {
    inner: Box<dyn ClipboardBackend>,
    enabled: Arc<AtomicBool>,
    last_count: u64,
}

impl ToggleableClipboardBackend {
    fn new(inner: Box<dyn ClipboardBackend>, enabled: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            enabled,
            last_count: 0,
        }
    }
}

impl ClipboardBackend for ToggleableClipboardBackend {
    fn change_count(&mut self) -> rc_types::Result<u64> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(self.last_count);
        }
        let c = self.inner.change_count()?;
        self.last_count = c;
        Ok(c)
    }

    fn read_text(&mut self) -> rc_types::Result<Option<String>> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.inner.read_text()
    }

    fn write_text(&mut self, text: &str) -> rc_types::Result<()> {
        if !self.enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        self.inner.write_text(text)
    }
}

/// 包装底层 [`Transport`]（`QuicTransport`、`P2pQuicConnection` 或 `TcpRelayTransport`）：
/// - 在 `poll` 入口响应本地断连标记（H3：握手期即时中止 45s 等待并取消远端审批弹窗，中继态即时切断套接字）；
/// - 在 `poll` 入口排空上层排队的手动控制消息（如 `SetBitrate(Manual)`），
///   无需改动 `rc-session` 状态机或协议。
struct BridgeTransport {
    inner: Box<dyn Transport>,
    pending_control: Arc<Mutex<VecDeque<Vec<u8>>>>,
    disconnect_flag: Arc<AtomicBool>,
    relay_shutdown: Option<RelayShutdownHandle>,
    streaming_started: Arc<AtomicBool>,
}

impl BridgeTransport {
    fn flush_pending_control(&mut self) {
        let msgs: Vec<Vec<u8>> = {
            let mut q = self
                .pending_control
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            q.drain(..).collect()
        };
        for msg in msgs {
            let _ = self.inner.send_reliable(&msg);
        }
    }
}

impl Transport for BridgeTransport {
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
        if self.disconnect_flag.load(Ordering::Acquire)
            && (self.relay_shutdown.is_some() || !self.streaming_started.load(Ordering::Acquire))
        {
            if let Some(ref handle) = self.relay_shutdown {
                handle.shutdown();
            }
            return Ok(vec![NetEvent::Disconnected {
                reason: "用户主动断开".to_string(),
            }]);
        }
        self.flush_pending_control();
        self.inner.poll(timeout)
    }

    fn stats(&self) -> NetStats {
        self.inner.stats()
    }
}

/// 会话级非文件工具客户端枢纽（W3 远程终端 + W4 TCP 端口映射）。
///
/// W2 文件传输已拆分至独立专用线程 `rc-file-client-worker`（H12），
/// 保证持有 `tools_hub` 锁时绝不执行任何磁盘 I/O（`canonicalize` / `open` / `write_all` / `sync_all`）。
struct ToolClientHub {
    terminal: TerminalClientEngine,
    portmap: PortMapClientEngine,
    active_terminals: BTreeSet<u32>,
}

impl ToolClientHub {
    fn new() -> Self {
        Self {
            terminal: TerminalClientEngine::new(),
            portmap: PortMapClientEngine::new(),
            active_terminals: BTreeSet::new(),
        }
    }
}

/// 轻量条件变量唤醒器，用于 `rc-file-client-worker` 零空转低延迟响应命令、线帧与停机。
#[derive(Clone)]
struct WorkerWake {
    inner: Arc<(Mutex<u64>, Condvar)>,
}

impl WorkerWake {
    fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    fn seq(&self) -> u64 {
        *self.inner.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        let mut guard = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        *guard = guard.wrapping_add(1);
        self.inner.1.notify_one();
    }

    fn wait_if_unchanged(&self, observed: u64, timeout: Duration) {
        let guard = self.inner.0.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == observed {
            let _ = self.inner.1.wait_timeout(guard, timeout);
        }
    }
}

enum FileWorkerCmd {
    ListRemote {
        path: Option<String>,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    MkdirRemote {
        parent: String,
        name: String,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    StartUpload {
        local_path: String,
        remote_dir: String,
        reply: mpsc::SyncSender<Result<u64, String>>,
    },
    StartDownload {
        remote_path: String,
        local_dir: String,
        reply: mpsc::SyncSender<Result<u64, String>>,
    },
    Cancel {
        id: u64,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    ClearFinished {
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    Remove {
        id: u64,
        reply: mpsc::SyncSender<Result<(), String>>,
    },
    Shutdown,
}

#[derive(Clone)]
struct FileClientHandle {
    cmd_tx: mpsc::SyncSender<FileWorkerCmd>,
    cached_transfers: Arc<Mutex<Vec<TransferItem>>>,
    wake: WorkerWake,
}

impl FileClientHandle {
    fn send_cmd<R>(
        &self,
        make_cmd: impl FnOnce(mpsc::SyncSender<Result<R, String>>) -> FileWorkerCmd,
    ) -> Result<R, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(make_cmd(reply_tx))
            .map_err(|_| "请先建立远控会话".to_string())?;
        self.wake.notify();
        reply_rx
            .recv()
            .map_err(|_| "请先建立远控会话".to_string())?
    }
}

/// 控制端专用文件传输工作线程（H12）：独立拥有 [`FileClientEngine`]，
/// 将本地文件打开/校验、分块读写、落盘 `sync_all()` 与重命名全部移出 60fps 视频/输入线程与 UI IPC 线程。
struct FileClientWorker {
    wire_in_tx: Option<mpsc::SyncSender<Vec<u8>>>,
    wire_out_rx: Option<mpsc::Receiver<Vec<u8>>>,
    cmd_tx: mpsc::SyncSender<FileWorkerCmd>,
    stop: Arc<AtomicBool>,
    wake: WorkerWake,
    cached_transfers: Arc<Mutex<Vec<TransferItem>>>,
    thread: Option<JoinHandle<()>>,
}

impl FileClientWorker {
    fn spawn(file_events: Arc<BoundedQueue<FileEvent>>) -> (Self, FileClientHandle) {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<FileWorkerCmd>(FILE_CLIENT_WORKER_CHAN_CAP);
        let (wire_in_tx, wire_in_rx) = mpsc::sync_channel::<Vec<u8>>(FILE_CLIENT_WORKER_CHAN_CAP);
        let (wire_out_tx, wire_out_rx) = mpsc::sync_channel::<Vec<u8>>(FILE_CLIENT_WORKER_CHAN_CAP);
        let cached_transfers = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = WorkerWake::new();

        let t_cached = Arc::clone(&cached_transfers);
        let t_stop = Arc::clone(&stop);
        let t_wake = wake.clone();

        let handle = thread::Builder::new()
            .name("rc-file-client-worker".to_string())
            .spawn(move || {
                let mut engine = FileClientEngine::new();
                let mut pending_out: VecDeque<Vec<u8>> = VecDeque::new();
                let mut last_upload_progress_at = Instant::now();

                loop {
                    if t_stop.load(Ordering::Acquire) {
                        return;
                    }
                    let wake_seq = t_wake.seq();
                    let mut did_work = false;
                    let mut transfers_dirty = false;

                    // 1) 优先排空所有就绪的控制命令（不被下载数据块饿死）。
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(FileWorkerCmd::ListRemote { path, reply }) => {
                                engine.request_list_dir(path);
                                did_work = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(FileWorkerCmd::MkdirRemote {
                                parent,
                                name,
                                reply,
                            }) => {
                                engine.request_mkdir(parent, name);
                                did_work = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(FileWorkerCmd::StartUpload {
                                local_path,
                                remote_dir,
                                reply,
                            }) => {
                                let res = engine.start_upload(&local_path, &remote_dir);
                                if let Ok(id) = res {
                                    let transfers = engine.transfers();
                                    if let Some(it) = transfers.iter().find(|t| t.id == id).cloned()
                                    {
                                        file_events
                                            .push_drop_oldest(FileEvent::TransferUpdated(it));
                                    }
                                    *t_cached.lock().unwrap_or_else(|e| e.into_inner()) = transfers;
                                    did_work = true;
                                }
                                let _ = reply.send(res);
                            }
                            Ok(FileWorkerCmd::StartDownload {
                                remote_path,
                                local_dir,
                                reply,
                            }) => {
                                let res = engine.start_download(&remote_path, &local_dir);
                                if let Ok(id) = res {
                                    let transfers = engine.transfers();
                                    if let Some(it) = transfers.iter().find(|t| t.id == id).cloned()
                                    {
                                        file_events
                                            .push_drop_oldest(FileEvent::TransferUpdated(it));
                                    }
                                    *t_cached.lock().unwrap_or_else(|e| e.into_inner()) = transfers;
                                    did_work = true;
                                }
                                let _ = reply.send(res);
                            }
                            Ok(FileWorkerCmd::Cancel { id, reply }) => {
                                engine.cancel_transfer(id);
                                let transfers = engine.transfers();
                                if let Some(it) = transfers.iter().find(|t| t.id == id).cloned() {
                                    file_events.push_drop_oldest(FileEvent::TransferUpdated(it));
                                }
                                *t_cached.lock().unwrap_or_else(|e| e.into_inner()) = transfers;
                                did_work = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(FileWorkerCmd::ClearFinished { reply }) => {
                                engine.clear_finished();
                                *t_cached.lock().unwrap_or_else(|e| e.into_inner()) =
                                    engine.transfers();
                                did_work = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(FileWorkerCmd::Remove { id, reply }) => {
                                engine.remove_transfer(id);
                                *t_cached.lock().unwrap_or_else(|e| e.into_inner()) =
                                    engine.transfers();
                                did_work = true;
                                let _ = reply.send(Ok(()));
                            }
                            Ok(FileWorkerCmd::Shutdown) => return,
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }

                    // 2) 每轮最多处理 MAX_TOOL_FRAMES_PER_TICK 条入站线帧。
                    for _ in 0..MAX_TOOL_FRAMES_PER_TICK {
                        match wire_in_rx.try_recv() {
                            Ok(payload) => {
                                for ev in engine.handle_incoming(&payload) {
                                    file_events.push_drop_oldest(ev);
                                }
                                did_work = true;
                                transfers_dirty = true;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => return,
                        }
                    }

                    // 3) 待发送缓冲为空时从引擎拉取一批出站线帧，并收集本地事件与上传进度。
                    if pending_out.is_empty() {
                        let frames = engine.poll_outgoing(MAX_TOOL_FRAMES_PER_TICK);
                        let local_evs = engine.drain_local_events();
                        if !local_evs.is_empty() {
                            for ev in local_evs {
                                file_events.push_drop_oldest(ev);
                            }
                            transfers_dirty = true;
                            did_work = true;
                        }
                        if !frames.is_empty() {
                            pending_out.extend(frames);
                            did_work = true;
                            transfers_dirty = true;
                            if last_upload_progress_at.elapsed() >= UPLOAD_PROGRESS_EMIT_INTERVAL {
                                last_upload_progress_at = Instant::now();
                                for item in engine.transfers() {
                                    if item.direction == TransferDirection::Upload
                                        && item.status == TransferStatus::Active
                                    {
                                        file_events
                                            .push_drop_oldest(FileEvent::TransferUpdated(item));
                                    }
                                }
                            }
                        }
                    }

                    if transfers_dirty {
                        *t_cached.lock().unwrap_or_else(|e| e.into_inner()) = engine.transfers();
                    }

                    // 4) 非阻塞冲刷出站线帧到有界通道。
                    let mut out_full = false;
                    while let Some(front) = pending_out.front() {
                        match wire_out_tx.try_send(front.clone()) {
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

                    // 5) 输出满载或空闲时条件变量泊车，零空转且可被命令/线帧/排空/停机即时唤醒。
                    if out_full {
                        t_wake.wait_if_unchanged(wake_seq, Duration::from_millis(2));
                    } else if !did_work && pending_out.is_empty() {
                        t_wake.wait_if_unchanged(wake_seq, Duration::from_millis(20));
                    }
                }
            })
            .expect("spawn rc-file-client-worker thread");

        let client_handle = FileClientHandle {
            cmd_tx: cmd_tx.clone(),
            cached_transfers: Arc::clone(&cached_transfers),
            wake: wake.clone(),
        };
        let worker = Self {
            wire_in_tx: Some(wire_in_tx),
            wire_out_rx: Some(wire_out_rx),
            cmd_tx,
            stop,
            wake,
            cached_transfers,
            thread: Some(handle),
        };
        (worker, client_handle)
    }

    fn try_send_incoming(&self, frame: Vec<u8>) -> Result<(), mpsc::TrySendError<Vec<u8>>> {
        let Some(tx) = self.wire_in_tx.as_ref() else {
            return Err(mpsc::TrySendError::Disconnected(frame));
        };
        let res = tx.try_send(frame);
        if res.is_ok() {
            self.wake.notify();
        }
        res
    }

    fn try_recv_outgoing(&self, max_frames: usize) -> Vec<Vec<u8>> {
        let Some(rx) = self.wire_out_rx.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(max_frames.min(MAX_TOOL_FRAMES_PER_TICK));
        for _ in 0..max_frames {
            match rx.try_recv() {
                Ok(frame) => out.push(frame),
                Err(_) => break,
            }
        }
        if !out.is_empty() {
            self.wake.notify();
        }
        out
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.wire_in_tx.take();
        self.wire_out_rx.take();
        let _ = self.cmd_tx.try_send(FileWorkerCmd::Shutdown);
        self.wake.notify();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.cached_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Drop for FileClientWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 线程安全的控制端工具调用句柄（供 Tauri IPC 命令直接调用，不在 `AppState::session` 锁内阻塞）。
#[derive(Clone)]
pub struct ToolClientHandle {
    hub: Arc<Mutex<ToolClientHub>>,
    file: FileClientHandle,
    portmap_latest: Arc<Mutex<Option<Vec<PortMapRuleSnapshot>>>>,
    running: Arc<AtomicBool>,
}

impl ToolClientHandle {
    fn ensure_running(&self) -> Result<(), String> {
        if self.running.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err("请先建立远控会话".to_string())
        }
    }

    /// 请求列举远端目录（结果通过 `file://event` 推送，在专用文件工作线程执行）。
    pub fn file_list_remote(&self, path: Option<String>) -> Result<(), String> {
        self.ensure_running()?;
        self.file
            .send_cmd(|reply| FileWorkerCmd::ListRemote { path, reply })
    }

    /// 请求在远端创建目录（结果通过 `file://event` 推送，在专用文件工作线程执行）。
    pub fn file_mkdir_remote(&self, parent: String, name: String) -> Result<(), String> {
        self.ensure_running()?;
        self.file.send_cmd(|reply| FileWorkerCmd::MkdirRemote {
            parent,
            name,
            reply,
        })
    }

    /// 发起本地文件上传到远端目录，返回传输任务 ID（磁盘 I/O 在专用文件工作线程执行，不触碰 `tools_hub` 锁）。
    pub fn file_upload(&self, local_path: &str, remote_dir: &str) -> Result<u64, String> {
        self.ensure_running()?;
        let local_path = local_path.to_string();
        let remote_dir = remote_dir.to_string();
        self.file.send_cmd(|reply| FileWorkerCmd::StartUpload {
            local_path,
            remote_dir,
            reply,
        })
    }

    /// 发起远端文件下载到本地目录，返回传输任务 ID（磁盘 I/O 在专用文件工作线程执行，不触碰 `tools_hub` 锁）。
    pub fn file_download(&self, remote_path: &str, local_dir: &str) -> Result<u64, String> {
        self.ensure_running()?;
        let remote_path = remote_path.to_string();
        let local_dir = local_dir.to_string();
        self.file.send_cmd(|reply| FileWorkerCmd::StartDownload {
            remote_path,
            local_dir,
            reply,
        })
    }

    /// 取消进行中的传输任务。
    pub fn file_cancel(&self, id: u64) -> Result<(), String> {
        self.ensure_running()?;
        self.file
            .send_cmd(|reply| FileWorkerCmd::Cancel { id, reply })
    }

    /// 返回当前全部文件传输任务快照（读取轻量缓存副本，永不阻塞文件磁盘 I/O 或视频线程）。
    pub fn file_list_transfers(&self) -> Vec<TransferItem> {
        if !self.running.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.file
            .cached_transfers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 清除已终态的传输任务记录。
    pub fn file_clear_finished(&self) -> Result<(), String> {
        self.ensure_running()?;
        self.file
            .send_cmd(|reply| FileWorkerCmd::ClearFinished { reply })
    }

    /// 移除指定传输任务记录（若仍活跃会先取消）。
    pub fn file_remove_transfer(&self, id: u64) -> Result<(), String> {
        self.ensure_running()?;
        self.file
            .send_cmd(|reply| FileWorkerCmd::Remove { id, reply })
    }

    /// 请求远端开启一个新的 PTY 终端会话，返回分配的 `term_id`。
    pub fn terminal_open(&self, cols: u16, rows: u16) -> Result<u32, String> {
        if cols == 0 || rows == 0 || cols > MAX_TERMINAL_DIM || rows > MAX_TERMINAL_DIM {
            return Err("终端行列数必须在 1–512 之间".to_string());
        }
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        let term_id = hub.terminal.open(cols, rows);
        hub.active_terminals.insert(term_id);
        Ok(term_id)
    }

    /// 查询指定 `term_id` 是否仍属于当前会话的活跃终端。
    pub fn has_active_terminal(&self, term_id: u32) -> bool {
        if !self.running.load(Ordering::Acquire) {
            return false;
        }
        self.hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_terminals
            .contains(&term_id)
    }

    /// 向远端指定 PTY 终端发送键盘/粘贴输入字节。
    pub fn terminal_input(&self, term_id: u32, data: &[u8]) -> Result<(), String> {
        if data.len() > MAX_TERMINAL_INPUT_BYTES {
            return Err("单次终端输入不能超过 64 KiB".to_string());
        }
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        if !hub.active_terminals.contains(&term_id) {
            return Err("目标终端会话不存在或已关闭".to_string());
        }
        hub.terminal.send_input(term_id, data);
        Ok(())
    }

    /// 同步调整远端指定 PTY 终端的行列尺寸。
    pub fn terminal_resize(&self, term_id: u32, cols: u16, rows: u16) -> Result<(), String> {
        if cols == 0 || rows == 0 || cols > MAX_TERMINAL_DIM || rows > MAX_TERMINAL_DIM {
            return Err("终端行列数必须在 1–512 之间".to_string());
        }
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        hub.terminal.resize(term_id, cols, rows);
        Ok(())
    }

    /// 请求关闭远端指定 PTY 终端会话。
    pub fn terminal_close(&self, term_id: u32) -> Result<(), String> {
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        hub.active_terminals.remove(&term_id);
        hub.terminal.close(term_id);
        Ok(())
    }

    /// 返回当前仍处于开启状态的全部远端终端 ID 快照（升序）。
    pub fn active_terminal_ids(&self) -> Vec<u32> {
        self.hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_terminals
            .iter()
            .copied()
            .collect()
    }

    /// 关闭当前全部活跃的远端 PTY 终端会话，并返回被关闭的 `term_id` 列表。
    pub fn close_all_terminals(&self) -> Vec<u32> {
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<u32> = std::mem::take(&mut hub.active_terminals)
            .into_iter()
            .collect();
        if self.running.load(Ordering::Acquire) {
            for &id in &ids {
                hub.terminal.close(id);
            }
        }
        ids
    }

    /// 列出全部 TCP 端口映射规则快照。
    pub fn portmap_list(&self) -> Vec<PortMapRuleSnapshot> {
        if !self.running.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .portmap
            .list_rules()
    }

    /// 添加一条 TCP 端口映射规则，返回新规则与最新完整规则列表（`hub -> portmap_latest` 严格串行化）。
    pub fn portmap_add(
        &self,
        name: String,
        local_port: u16,
        target_addr: String,
        target_port: u16,
    ) -> Result<(PortMapRuleSnapshot, Vec<PortMapRuleSnapshot>), String> {
        self.portmap_add_with_enabled(name, local_port, target_addr, target_port, true)
    }

    /// 添加一条 TCP 端口映射规则并指定初始启用状态（供禁用规则恢复免短暂绑定端口使用）。
    pub fn portmap_add_with_enabled(
        &self,
        name: String,
        local_port: u16,
        target_addr: String,
        target_port: u16,
        enabled: bool,
    ) -> Result<(PortMapRuleSnapshot, Vec<PortMapRuleSnapshot>), String> {
        let name = name.trim().to_string();
        let target_addr = target_addr.trim().to_string();
        crate::model::validate_map_name(&name)?;
        if local_port == 0 || target_port == 0 {
            return Err("端口不能为 0，范围必须为 1–65535".to_string());
        }
        crate::model::validate_target_addr(&target_addr)?;
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        let rule = hub.portmap.add_rule_with_enabled(
            name,
            local_port,
            target_addr,
            target_port,
            enabled,
        )?;
        let all = hub.portmap.list_rules();
        *self
            .portmap_latest
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(all.clone());
        Ok((rule, all))
    }

    /// 启用或暂停指定 TCP 端口映射规则，返回更新后的规则与最新完整规则列表（`hub -> portmap_latest` 严格串行化）。
    pub fn portmap_toggle(
        &self,
        id: u64,
        enabled: bool,
    ) -> Result<(PortMapRuleSnapshot, Vec<PortMapRuleSnapshot>), String> {
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        let rule = hub.portmap.set_rule_enabled(id, enabled)?;
        let all = hub.portmap.list_rules();
        *self
            .portmap_latest
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(all.clone());
        Ok((rule, all))
    }

    /// 删除指定 TCP 端口映射规则，返回最新完整规则列表（`hub -> portmap_latest` 严格串行化）。
    pub fn portmap_delete(&self, id: u64) -> Result<Vec<PortMapRuleSnapshot>, String> {
        self.ensure_running()?;
        let mut hub = self.hub.lock().unwrap_or_else(|e| e.into_inner());
        hub.portmap.delete_rule(id)?;
        let all = hub.portmap.list_rules();
        *self
            .portmap_latest
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(all.clone());
        Ok(all)
    }
}

/// 按收到音频包的 `(sample_rate, channels)` 校验/重建播放器（M9）：
/// 若格式与当前播放器不同，先调用旧播放器 `stop()` 再按新格式重建并同步静音态。
fn ensure_audio_player_for_format(
    audio_player: &mut Option<Box<dyn AudioPlayer>>,
    audio_player_format: &mut Option<(u32, u8)>,
    sample_rate: u32,
    channels: u8,
    audio_muted: bool,
    make_player: impl FnOnce(u32, u8) -> Box<dyn AudioPlayer>,
) {
    let target_format = (sample_rate, channels);
    if *audio_player_format != Some(target_format) {
        if let Some(mut old_player) = audio_player.take() {
            old_player.stop();
        }
        let mut player = make_player(sample_rate, channels);
        player.set_muted(audio_muted);
        *audio_player = Some(player);
        *audio_player_format = Some(target_format);
    }
}

/// 将收到的音频帧包解码为线性 PCM16 采样；不支持的编码或非法包返回 `None`。
fn decode_audio_packet_samples(pkt: &AudioFramePacket) -> Option<Vec<i16>> {
    match pkt.codec {
        AUDIO_CODEC_IMA_ADPCM => decode_ima_adpcm(&pkt.data, pkt.channels).ok(),
        AUDIO_CODEC_PCM16 => {
            if pkt.data.len() % 2 != 0 {
                return None;
            }
            let mut samples = Vec::with_capacity(pkt.data.len() / 2);
            for chunk in pkt.data.chunks_exact(2) {
                samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            Some(samples)
        }
        _ => None,
    }
}

/// 构造音频控制 JSON 载荷（`{"muted": bool}`）。
fn encode_audio_ctrl_muted(muted: bool) -> Vec<u8> {
    serde_json::json!({ "muted": muted })
        .to_string()
        .into_bytes()
}

/// 冲刷单个工具通道的待发送队列：遇到发送 `Err`（如 TCP 中继背压）时立即停止并保留队首，
/// 下一拍严格按 FIFO 顺序重试，绝不丢弃任何可靠工具帧。
fn flush_pending_tool_queue<E>(
    pending: &mut VecDeque<Vec<u8>>,
    mut send_fn: impl FnMut(Vec<u8>) -> Result<(), E>,
) -> bool {
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
// SessionHandle（公开 API）
// ─────────────────────────────────────────────────────────────────

/// UI 侧会话句柄：持有有界通道与后台会话线程。
///
/// 通道容量（铁律 1 UI 侧延伸 / U6）：
/// - 命令通道 [`COMMAND_CHANNEL_CAP`] = 64，满载丢最旧；
/// - 输入通道 [`INPUT_CHANNEL_CAP`] = 256，满载优先保留关键事件、丢最旧移动事件；
/// - 更新通道 [`UPDATE_CHANNEL_CAP`] = 64，满载优先丢最旧 `Hud`/`Connecting`。
///
/// 调用 [`SessionHandle::join`] 或直接 `Drop` 均会发送 `Disconnect` 并等待线程干净退出，
/// 绝不遗留僵尸线程（U10）。
pub struct SessionHandle {
    cmd_tx: Arc<BoundedQueue<SessionCommand>>,
    input_tx: Arc<BoundedInputQueue>,
    update_rx: Arc<BoundedUpdateQueue>,
    file_event_rx: Arc<BoundedQueue<FileEvent>>,
    terminal_event_rx: Arc<BoundedQueue<TerminalEvent>>,
    portmap_latest: Arc<Mutex<Option<Vec<PortMapRuleSnapshot>>>>,
    tools_hub: Arc<Mutex<ToolClientHub>>,
    file_handle: FileClientHandle,
    disconnect_flag: Arc<AtomicBool>,
    lock_on_disconnect: Arc<AtomicBool>,
    relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>>,
    running: Arc<AtomicBool>,
    watch_only: Arc<AtomicBool>,
    encrypt_connection: bool,
    base_config: UiSessionConfig,
    layer_lease: Arc<Mutex<Option<LayerLease>>>,
    thread: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// 启动会话线程（构造 `ClientSession` → 握手 → 驱动循环 → `Ended` 后自然退出）。
    ///
    /// 返回的句柄立即可用；连接进度与 HUD 更新经 [`Self::try_recv_update`] 非阻塞拉取。
    pub fn spawn(cfg: UiSessionConfig) -> Self {
        Self::spawn_with_metal_layer(cfg, None)
    }

    /// 启动会话线程并可选传入在 macOS 主线程预分配的 `CAMetalLayer`（避免后台创建
    /// `CAMetalLayer` 无法绑定 WindowServer 合成树导致黑屏）。
    pub fn spawn_with_metal_layer(
        cfg: UiSessionConfig,
        preallocated_layer: Option<MainThreadMetalLayer>,
    ) -> Self {
        let cmd_tx = Arc::new(BoundedQueue::new(COMMAND_CHANNEL_CAP));
        let input_tx = Arc::new(BoundedInputQueue::new(INPUT_CHANNEL_CAP));
        let update_rx = Arc::new(BoundedUpdateQueue::new(UPDATE_CHANNEL_CAP));
        let file_event_rx = Arc::new(BoundedQueue::new(FILE_EVENT_CHANNEL_CAP));
        let terminal_event_rx = Arc::new(BoundedQueue::new(TERMINAL_EVENT_CHANNEL_CAP));
        let portmap_latest: Arc<Mutex<Option<Vec<PortMapRuleSnapshot>>>> =
            Arc::new(Mutex::new(None));
        let tools_hub = Arc::new(Mutex::new(ToolClientHub::new()));
        let (file_worker, file_handle) = FileClientWorker::spawn(Arc::clone(&file_event_rx));
        let disconnect_flag = Arc::new(AtomicBool::new(false));
        let lock_on_disconnect = Arc::new(AtomicBool::new(false));
        let relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>> = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(true));
        let layer_lease = Arc::new(Mutex::new(None));
        let watch_only = Arc::new(AtomicBool::new(cfg.watch_only));
        let encrypt_connection = cfg.encrypt_connection;
        let base_config = cfg.clone();

        let t_cmd = Arc::clone(&cmd_tx);
        let t_input = Arc::clone(&input_tx);
        let t_update = Arc::clone(&update_rx);
        let t_terminal_events = Arc::clone(&terminal_event_rx);
        let t_portmap_latest = Arc::clone(&portmap_latest);
        let t_tools_hub = Arc::clone(&tools_hub);
        let t_disconnect = Arc::clone(&disconnect_flag);
        let t_lock_on_disconnect = Arc::clone(&lock_on_disconnect);
        let t_relay_interrupt = Arc::clone(&relay_interrupt);
        let t_running = Arc::clone(&running);
        let t_watch_only = Arc::clone(&watch_only);
        let t_layer = Arc::clone(&layer_lease);

        let handle = thread::Builder::new()
            .name("rc-ui-session".to_string())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                rc_capture_macos::promote_current_thread_qos();

                let mut file_worker = file_worker;
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_session_thread(
                        cfg,
                        preallocated_layer,
                        t_cmd,
                        t_input,
                        Arc::clone(&t_update),
                        t_layer,
                        t_disconnect,
                        t_lock_on_disconnect,
                        t_relay_interrupt,
                        Arc::clone(&t_tools_hub),
                        &file_worker,
                        Arc::clone(&t_terminal_events),
                        t_portmap_latest,
                        t_watch_only,
                    )
                }));
                let reason = match res {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => e,
                    Err(_) => "会话线程异常退出".to_string(),
                };
                // 先标记非运行并回收专用文件工作线程（确保 `.rcpart` 清理完毕），
                // 向仍打开的终端推送 Exited(code=-1)，再重置端口映射与终端引擎。
                t_running.store(false, Ordering::Release);
                file_worker.shutdown();
                let remaining_terms: Vec<u32> = {
                    let mut hub = t_tools_hub.lock().unwrap_or_else(|e| e.into_inner());
                    let ids: Vec<u32> = std::mem::take(&mut hub.active_terminals)
                        .into_iter()
                        .collect();
                    *hub = ToolClientHub::new();
                    ids
                };
                for term_id in remaining_terms {
                    t_terminal_events.push_drop_oldest(TerminalEvent::Exited { term_id, code: -1 });
                }
                t_update.push(SessionUpdate::Ended { reason });
            })
            .expect("spawn rc-ui-session thread");

        Self {
            cmd_tx,
            input_tx,
            update_rx,
            file_event_rx,
            terminal_event_rx,
            portmap_latest,
            tools_hub,
            file_handle,
            disconnect_flag,
            lock_on_disconnect,
            relay_interrupt,
            running,
            watch_only,
            encrypt_connection,
            base_config,
            layer_lease,
            thread: Some(handle),
        }
    }

    /// 标记本次会话在断开前向被控端发送锁屏指令（供「退出时锁定被控端」使用）。
    pub fn request_lock_on_disconnect(&self) {
        self.lock_on_disconnect.store(true, Ordering::Release);
    }

    fn trigger_disconnect(&self) {
        self.disconnect_flag.store(true, Ordering::Release);
        let interrupt = self
            .relay_interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(h) = interrupt {
            h.interrupt();
        }
    }

    /// 发送控制命令；会话已结束时静默忽略（幂等，U10）。
    pub fn send(&self, cmd: SessionCommand) {
        if matches!(cmd, SessionCommand::Disconnect) {
            self.trigger_disconnect();
        }
        if !self.is_running() {
            return;
        }
        self.cmd_tx.push_drop_oldest(cmd);
    }

    /// 动态切换当前会话的只读观看模式标记（供 `set_session_watch_only` 同步控制输入通道与采集器）。
    pub fn set_watch_only(&self, enabled: bool) {
        self.watch_only.store(enabled, Ordering::Release);
    }

    /// 发送一条输入事件；观看模式（`watch_only`）或会话已结束时静默丢弃（U9 / U10）。
    pub fn send_input(&self, ev: InputEvent) {
        if self.watch_only.load(Ordering::Acquire) || !self.is_running() {
            return;
        }
        self.input_tx.push(ev);
    }

    /// 非阻塞拉取一条状态更新（`commands.rs` 轮询后 emit 给前端）。
    pub fn try_recv_update(&self) -> Option<SessionUpdate> {
        self.update_rx.try_pop()
    }

    /// 获取轻量可克隆的工具操作句柄（供 W2/W3/W4 IPC 命令在释放 `AppState::session` 锁后调用）。
    pub fn tools(&self) -> ToolClientHandle {
        ToolClientHandle {
            hub: Arc::clone(&self.tools_hub),
            file: self.file_handle.clone(),
            portmap_latest: Arc::clone(&self.portmap_latest),
            running: Arc::clone(&self.running),
        }
    }

    /// 非阻塞拉取一条 W2 文件传输 UI 事件（`main.rs` 轮询后 emit `"file://event"`）。
    pub fn try_recv_file_event(&self) -> Option<FileEvent> {
        self.file_event_rx.try_pop()
    }

    /// 非阻塞拉取一条 W3 远程终端 UI 事件（`main.rs` 轮询后 emit `"terminal://event"`）。
    pub fn try_recv_terminal_event(&self) -> Option<TerminalEvent> {
        self.terminal_event_rx.try_pop()
    }

    /// 非阻塞取出最新 W4 端口映射规则列表变更（`main.rs` 轮询后 emit `"portmap://update"`）。
    pub fn take_portmap_update(&self) -> Option<Vec<PortMapRuleSnapshot>> {
        self.portmap_latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// 会话线程是否仍在运行。
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 当前会话是否开启了双向 mTLS / E2EE 加密连接模式。
    pub fn encrypt_connection(&self) -> bool {
        self.encrypt_connection
    }

    /// 基于当前会话配置生成切换 `encrypt_connection` 后的重连配置（保持所有画面、音频、权限与地址参数不变）。
    pub fn config_with_encrypt_connection(&self, encrypt_connection: bool) -> UiSessionConfig {
        self.base_config.with_encrypt_connection(encrypt_connection)
    }

    /// 获取离屏 `MetalRenderer` 的保活租约（仅 macOS 硬件后端就绪后返回 `Some`）。
    ///
    /// 持有返回的 [`LayerLease`] 期间，即使会话线程已退出并释放自身管线，底层
    /// `MetalRenderer` 及其 `CAMetalLayer` 仍保持存活，供主线程安全完成挂载/同步/摘除。
    pub fn layer_lease(&self) -> Option<LayerLease> {
        self.layer_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 获取离屏 `MetalRenderer::layer_ptr()` 借用指针（仅 macOS 硬件后端就绪后返回 `Some`）。
    ///
    /// 跨线程派发到主线程时请优先使用 [`Self::layer_lease`] 保活渲染器。
    pub fn layer_ptr(&self) -> Option<*mut c_void> {
        self.layer_lease().map(|lease| lease.as_ptr())
    }

    /// 主动断开并等待会话线程干净退出（消费句柄，U10）。
    pub fn join(mut self) {
        self.trigger_disconnect();
        if self.is_running() {
            self.cmd_tx.push_drop_oldest(SessionCommand::Disconnect);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.trigger_disconnect();
        if self.is_running() {
            self.cmd_tx.push_drop_oldest(SessionCommand::Disconnect);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 会话线程主循环（照 rc-cli/src/cmd/client.rs 形态）
// ─────────────────────────────────────────────────────────────────

/// 可重锚的会话钟（同 `rc-cli/src/cmd/client.rs` 修复卡 C-1）。
fn reanchorable_clock(origin: Instant) -> (SessionClock, Arc<Mutex<Instant>>) {
    let anchor = Arc::new(Mutex::new(origin));
    let clock: SessionClock = {
        let anchor = Arc::clone(&anchor);
        Arc::new(move || {
            let a = *anchor.lock().unwrap_or_else(|e| e.into_inner());
            u32::try_from(a.elapsed().as_micros()).unwrap_or(u32::MAX)
        })
    };
    (clock, anchor)
}

/// 生成进程内唯一会话号（同 `rc-cli/src/cmd/client.rs`）。
fn derive_session_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    (u64::from(std::process::id()) << 20) | (n & 0xF_FFFF) | 0x200_000
}

const LAN_DIRECT_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

struct LanQuicTransport {
    transport: rc_net::quic::QuicTransport,
    _endpoint: QuicEndpoint,
}

impl Transport for LanQuicTransport {
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

fn is_mtls_cert_or_config_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") || msg.contains("超时") {
        return false;
    }
    lower.contains("invalid peer certificate")
        || lower.contains("certificate")
        || lower.contains("unknownissuer")
        || lower.contains("badsignature")
        || lower.contains("san uri")
        || lower.contains("urn:rabbitcontrol")
        || lower.contains("invalidconfig")
        || lower.contains("tls alert")
        || lower.contains("unexpected eof")
        || lower.contains("handshakefailure")
        || lower.contains("badcertificate")
        || lower.contains("cert")
}

fn prepare_peer_mtls_bundle(
    cfg: &UiSessionConfig,
    grant: &ConnectGrant,
) -> Option<SessionMtlsBundle> {
    let mut bundle = cfg.mtls_config.clone()?;
    if bundle.expected_peer_device_id.is_none() && !grant.target_id.is_empty() {
        bundle.expected_peer_device_id = Some(grant.target_id.clone());
    }
    if bundle.expected_peer_user_id.is_none() && grant.is_owner_connection {
        bundle.expected_peer_user_id = Some(bundle.local_user_id.clone());
    }
    Some(bundle)
}

fn collect_lan_candidate_addrs(cfg: &UiSessionConfig, grant: &ConnectGrant) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Some(addr) = cfg.lan_direct_addr {
        out.push(addr);
    }
    let local_v4 = lan_discovery::detect_local_lan_ipv4();
    for raw in &grant.peer_lan_addrs {
        let Ok(addr) = raw.parse::<SocketAddr>() else {
            continue;
        };
        if addr.ip().is_loopback() {
            continue;
        }
        let same_subnet = match (addr.ip(), local_v4) {
            (std::net::IpAddr::V4(peer_ip), Some(local_ip)) => {
                peer_ip.octets()[..3] == local_ip.octets()[..3]
            }
            _ => false,
        };
        if same_subnet {
            let candidate = SocketAddr::new(addr.ip(), crate::host_bridge::DEFAULT_HOST_PORT);
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

fn dial_lan_direct_quic(
    lan_addr: SocketAddr,
    mtls: Option<&rc_net::mtls::MtlsConfig>,
    cancel: &AtomicBool,
) -> Result<(Box<dyn Transport>, Option<String>), String> {
    let identity = Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM)
        .map_err(|e| format!("LAN QUIC 端点证书加载失败: {e}"))?;
    let net = NetConfig {
        role: Role::Controller,
        local_addr: "0.0.0.0:0"
            .parse()
            .expect("0.0.0.0:0 is a valid socket addr"),
        peer_addr: Some(lan_addr),
        mtu: rc_session::DEFAULT_SESSION_MTU,
    };
    let endpoint =
        QuicEndpoint::new(&net, &identity).map_err(|e| format!("LAN QUIC 端点创建失败: {e}"))?;
    if let Some(mtls_cfg) = mtls {
        let transport = endpoint
            .connect_mtls_cancelable(lan_addr, mtls_cfg, "", LAN_DIRECT_PROBE_TIMEOUT, cancel)
            .map_err(|e| format!("LAN mTLS 直连失败: {e}"))?;
        let fp = transport
            .peer_mtls_identity()
            .map(|id| id.cert_fingerprint_sha256);
        Ok((
            Box::new(LanQuicTransport {
                transport,
                _endpoint: endpoint,
            }),
            fp,
        ))
    } else {
        let certs = Identity::certs_from_pem(dev_server_cert_pem())
            .map_err(|e| format!("LAN 受信任证书解析失败: {e}"))?;
        let transport = endpoint
            .connect_cancelable(
                lan_addr,
                &certs,
                DEV_SERVER_NAME,
                LAN_DIRECT_PROBE_TIMEOUT,
                cancel,
            )
            .map_err(|e| format!("LAN QUIC 直连失败: {e}"))?;
        Ok((
            Box::new(LanQuicTransport {
                transport,
                _endpoint: endpoint,
            }),
            None,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session_thread(
    cfg: UiSessionConfig,
    preallocated_layer: Option<MainThreadMetalLayer>,
    cmd_rx: Arc<BoundedQueue<SessionCommand>>,
    input_rx: Arc<BoundedInputQueue>,
    update_tx: Arc<BoundedUpdateQueue>,
    layer_lease: Arc<Mutex<Option<LayerLease>>>,
    disconnect_flag: Arc<AtomicBool>,
    lock_on_disconnect: Arc<AtomicBool>,
    relay_interrupt: Arc<Mutex<Option<ActiveRelayInterrupt>>>,
    tools_hub: Arc<Mutex<ToolClientHub>>,
    file_worker: &FileClientWorker,
    terminal_events: Arc<BoundedQueue<TerminalEvent>>,
    portmap_latest: Arc<Mutex<Option<Vec<PortMapRuleSnapshot>>>>,
    watch_only: Arc<AtomicBool>,
) -> Result<String, String> {
    let _relay_guard = RelayInterruptSlotGuard {
        slot: &relay_interrupt,
    };

    if disconnect_flag.load(Ordering::Acquire) {
        return Ok("用户主动断开".to_string());
    }

    // 严禁静默降级：开启加密连接但缺少 mTLS 证书束时立即拒绝建连。
    if cfg.encrypt_connection && cfg.mtls_config.is_none() {
        return Err("已开启「加密连接 (mTLS)」，但缺少设备证书配置，已拒绝降级连接".to_string());
    }

    // ── 计时起点：Endpoint 创建之前（T21 U8 / 坑 11，对齐 T18 口径）──
    let t0 = Instant::now();
    let (clock, clock_anchor) = reanchorable_clock(t0);
    let clock_reader = Arc::clone(&clock);

    let live_watch_only = Arc::clone(&watch_only);
    let capture = Box::new(BridgeInputCapture {
        queue: input_rx,
        watch_only,
    });

    let resolution = Resolution::new(cfg.width.max(1), cfg.height.max(1));
    let pipeline = PipelineConfig {
        resolution,
        fps: cfg.fps.max(1),
        bitrate_bps: cfg.bitrate_bps.max(1),
        prefer_hardware: cfg.prefer_hardware,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let session_id = derive_session_id();
    let mut client_cfg = ClientSessionConfig {
        peer_addr: cfg.connect_addr,
        mtu: rc_session::DEFAULT_SESSION_MTU,
        requested: SessionParams {
            width: resolution.width,
            height: resolution.height,
            fps: cfg.fps.max(1),
            bitrate_bps: cfg.bitrate_bps.max(1),
        },
        session_id,
        pipeline,
        connect_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
        capture,
        input_window: rc_input::DEFAULT_WINDOW,
        auth: ClientAuthConfig {
            password: cfg.password.clone(),
            owner_signer: cfg.owner_signer.clone(),
            ..ClientAuthConfig::default()
        },
    };

    let pending_control: Arc<Mutex<VecDeque<Vec<u8>>>> = Arc::new(Mutex::new(VecDeque::new()));
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let renderer_handle: Arc<Mutex<Option<RendererHandle>>> = Arc::new(Mutex::new(None));
    let is_signaling_session = cfg.relay_grant.is_some();
    let streaming_started = Arc::new(AtomicBool::new(false));
    let clipboard_enabled_flag = Arc::new(AtomicBool::new(cfg.clipboard_enabled));

    let make_pipeline_factory = |backend_suffix: Option<&'static str>| {
        let label = Arc::clone(&backend_label);
        let rend_slot = Arc::clone(&renderer_handle);
        let layer_slot = Arc::clone(&layer_lease);
        let prefer_hw = cfg.prefer_hardware;
        let pre_layer = preallocated_layer.clone();
        Box::new(move |pcfg: &PipelineConfig| {
            let built = build_controller_pipeline(pcfg, prefer_hw, t0, pre_layer.as_ref())?;
            let final_label = match backend_suffix {
                Some(suffix) => format!("{} {suffix}", built.backend_label),
                None => built.backend_label,
            };
            *label.lock().unwrap_or_else(|e| e.into_inner()) = final_label;
            *rend_slot.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Arc::clone(&built.renderer));
            *layer_slot.lock().unwrap_or_else(|e| e.into_inner()) =
                LayerLease::from_renderer(built.renderer, built.layer_addr);
            Ok(built.pipeline)
        })
    };

    let (mut client, negotiated, transport_label, mtls_active, peer_cert_fingerprint) =
        if let Some(grant) = cfg.relay_grant.as_ref() {
            update_tx.push(SessionUpdate::Connecting {
                phase: ConnectPhase::Connecting,
                watch_only: live_watch_only.load(Ordering::Acquire),
            });

            let dial_cancel = Arc::new(AtomicBool::new(false));
            *relay_interrupt.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(ActiveRelayInterrupt::Dialing(Arc::clone(&dial_cancel)));
            if disconnect_flag.load(Ordering::Acquire) {
                return Ok("用户主动断开".to_string());
            }

            let peer_bundle = prepare_peer_mtls_bundle(&cfg, grant);
            let quic_mtls = if cfg.encrypt_connection {
                let bundle = peer_bundle
                    .as_ref()
                    .ok_or_else(|| "已开启「加密连接 (mTLS)」，但缺少设备证书配置".to_string())?;
                Some(bundle.to_quic_mtls_config()?)
            } else {
                None
            };

            // Tier 1：若未强制中继且命中 LAN 直连候选（lan_direct_addr 或同网段地址），300ms 快速探测 UDP LAN。
            struct SelectedRelayPath {
                transport: Box<dyn Transport>,
                relay_shutdown: Option<RelayShutdownHandle>,
                transport_label: &'static str,
                backend_suffix: Option<&'static str>,
                mtls_active: bool,
                peer_cert_fingerprint: Option<String>,
            }
            let mut chosen: Option<SelectedRelayPath> = None;
            if !cfg.force_relay && !disconnect_flag.load(Ordering::Acquire) {
                for lan_addr in collect_lan_candidate_addrs(&cfg, grant) {
                    if disconnect_flag.load(Ordering::Acquire) {
                        return Ok("用户主动断开".to_string());
                    }
                    match dial_lan_direct_quic(lan_addr, quic_mtls.as_ref(), &disconnect_flag) {
                        Ok((t, fp)) => {
                            client_cfg.peer_addr = lan_addr;
                            chosen = Some(SelectedRelayPath {
                                transport: t,
                                relay_shutdown: None,
                                transport_label: "UDP LAN",
                                backend_suffix: None,
                                mtls_active: cfg.encrypt_connection,
                                peer_cert_fingerprint: fp,
                            });
                            break;
                        }
                        Err(err) => {
                            if cfg.encrypt_connection && is_mtls_cert_or_config_error(&err) {
                                return Err(format!(
                                    "LAN mTLS 证书校验失败（严禁静默降级）: {err}"
                                ));
                            }
                        }
                    }
                }
            }

            // Tier 2：LAN 未命中或失败且未强制中继时，尝试 UDP P2P 打洞 + QUIC 直连。
            if chosen.is_none() && !cfg.force_relay && !disconnect_flag.load(Ordering::Acquire) {
                let candidates = P2pCandidates {
                    session_id: grant.session_id.clone(),
                    peer_public_addr: grant.peer_public_addr.clone(),
                    peer_lan_addrs: grant.peer_lan_addrs.clone(),
                    local_udp_port: grant.local_udp_port,
                };
                if !p2p::parse_candidate_addrs(&candidates).is_empty() {
                    match p2p::attempt_p2p_quic_client_mtls(
                        &candidates,
                        p2p::DEFAULT_PUNCH_TIMEOUT,
                        p2p::DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
                        &disconnect_flag,
                        quic_mtls.as_ref(),
                    ) {
                        Ok(p2p_conn) => {
                            let fp = p2p_conn
                                .peer_mtls_identity()
                                .map(|id| id.cert_fingerprint_sha256);
                            chosen = Some(SelectedRelayPath {
                                transport: Box::new(p2p_conn),
                                relay_shutdown: None,
                                transport_label: "UDP P2P",
                                backend_suffix: Some("(P2P 直连)"),
                                mtls_active: cfg.encrypt_connection,
                                peer_cert_fingerprint: fp,
                            });
                        }
                        Err(err) => {
                            if cfg.encrypt_connection && is_mtls_cert_or_config_error(&err) {
                                return Err(format!(
                                    "P2P mTLS 证书校验失败（严禁静默降级）: {err}"
                                ));
                            }
                        }
                    }
                }
            }

            if disconnect_flag.load(Ordering::Acquire) {
                return Ok("用户主动断开".to_string());
            }

            // Tier 3：P2P 失败/未命中或强制中继 → 回退 `:21117` 中继（有 mtls_config 强制外层 mTLS；encrypt_connection 再叠加内层 E2EE mTLS）。
            let SelectedRelayPath {
                transport: raw_transport,
                relay_shutdown,
                transport_label: chosen_transport_label,
                backend_suffix,
                mtls_active,
                peer_cert_fingerprint: peer_fp,
            } = match chosen {
                Some(ready) => ready,
                None => {
                    let (raw_transport, shutdown_handle, active, fp): (
                        Box<dyn Transport>,
                        RelayShutdownHandle,
                        bool,
                        Option<String>,
                    ) = if cfg.encrypt_connection {
                        let bundle = peer_bundle.ok_or_else(|| {
                            "已开启「加密连接 (mTLS)」，但缺少设备证书配置".to_string()
                        })?;
                        let relay_mtls = bundle.to_relay_mtls_config(None);
                        let t = match E2eeRelayTransport::connect_cancellable(
                            &grant.relay_addr,
                            RelayRole::Client,
                            &grant.relay_token,
                            Duration::from_secs(10),
                            Arc::clone(&dial_cancel),
                            Some(&relay_mtls),
                            RelayE2eeMode::ClientRequired(bundle),
                        ) {
                            Ok(t) => t,
                            Err(e) => {
                                if disconnect_flag.load(Ordering::Acquire) {
                                    return Ok("用户主动断开".to_string());
                                }
                                return Err(format!("mTLS 中继连接失败: {e}"));
                            }
                        };
                        let h = t.shutdown_handle();
                        let fp = t
                            .peer_mtls_identity()
                            .map(|id| id.cert_fingerprint_sha256.clone());
                        (Box::new(t), h, true, fp)
                    } else if let Some(bundle) = peer_bundle {
                        let relay_mtls = bundle.to_relay_mtls_config(None);
                        let t = match TcpRelayTransport::connect_mtls_cancellable(
                            &grant.relay_addr,
                            RelayRole::Client,
                            &grant.relay_token,
                            Duration::from_secs(10),
                            Arc::clone(&dial_cancel),
                            &relay_mtls,
                        ) {
                            Ok(t) => t,
                            Err(e) => {
                                if disconnect_flag.load(Ordering::Acquire) {
                                    return Ok("用户主动断开".to_string());
                                }
                                return Err(format!("外层 mTLS 中继连接失败: {e}"));
                            }
                        };
                        let h = t.shutdown_handle();
                        (Box::new(t), h, false, None)
                    } else {
                        let t = match TcpRelayTransport::connect_cancellable(
                            &grant.relay_addr,
                            RelayRole::Client,
                            &grant.relay_token,
                            Duration::from_secs(10),
                            Arc::clone(&dial_cancel),
                        ) {
                            Ok(t) => t,
                            Err(e) => {
                                if disconnect_flag.load(Ordering::Acquire) {
                                    return Ok("用户主动断开".to_string());
                                }
                                return Err(format!("中继连接失败: {e}"));
                            }
                        };
                        let h = t.shutdown_handle();
                        (Box::new(t), h, false, None)
                    };

                    *relay_interrupt.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(ActiveRelayInterrupt::Connected(shutdown_handle.clone()));
                    if disconnect_flag.load(Ordering::Acquire) {
                        shutdown_handle.shutdown();
                        return Ok("用户主动断开".to_string());
                    }

                    SelectedRelayPath {
                        transport: raw_transport,
                        relay_shutdown: Some(shutdown_handle),
                        transport_label: "TCP Relay",
                        backend_suffix: Some("(中继)"),
                        mtls_active: active,
                        peer_cert_fingerprint: fp,
                    }
                }
            };

            update_tx.push(SessionUpdate::Connecting {
                phase: ConnectPhase::AwaitingWelcome,
                watch_only: live_watch_only.load(Ordering::Acquire),
            });

            let bridge_transport = BridgeTransport {
                inner: raw_transport,
                pending_control: Arc::clone(&pending_control),
                disconnect_flag: Arc::clone(&disconnect_flag),
                relay_shutdown,
                streaming_started: Arc::clone(&streaming_started),
            };
            // 占位解码/渲染器仅用于满足 `ClientSession::with_parts` 签名；握手成功后
            // `set_pipeline_factory_for` 会按裁定参数构造实际硬件/假栈并挂载 `LayerLease`。
            let placeholder_res = Resolution::new(64, 64);
            let dummy_decoder = rc_decode::FakeDecoder::new(rc_decode::FakeDecoderConfig {
                resolution: placeholder_res,
                ..rc_decode::FakeDecoderConfig::default()
            })
            .map_err(|e| format!("会话占位解码器构造失败: {e}"))?;
            let dummy_renderer = rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
                size: (placeholder_res.width, placeholder_res.height),
                ..rc_render::FakeRendererConfig::default()
            })
            .map_err(|e| format!("会话占位渲染器构造失败: {e}"))?;

            let mut client = ClientSession::with_parts(
                client_cfg,
                Box::new(bridge_transport),
                Box::new(dummy_decoder),
                Box::new(dummy_renderer),
                clock,
            )
            .map_err(|e| format!("信令会话构造失败: {e}"))?;
            client.set_pipeline_factory_for(make_pipeline_factory(backend_suffix));
            if let Some(cb) = create_clipboard_backend(cfg.prefer_hardware) {
                client.enable_clipboard(Box::new(ToggleableClipboardBackend::new(
                    cb,
                    Arc::clone(&clipboard_enabled_flag),
                )));
            }

            let negotiated = match client.handshake() {
                Ok(n) => n,
                Err(e) => {
                    if disconnect_flag.load(Ordering::Acquire) {
                        return Ok("用户主动断开".to_string());
                    }
                    return Err(format!("连接/握手失败: {e}"));
                }
            };
            (
                client,
                negotiated,
                chosen_transport_label,
                mtls_active,
                peer_fp,
            )
        } else {
            let direct_mtls = if cfg.encrypt_connection {
                let bundle = cfg
                    .mtls_config
                    .as_ref()
                    .ok_or_else(|| "已开启「加密连接 (mTLS)」，但缺少设备证书配置".to_string())?;
                Some(bundle.to_quic_mtls_config()?)
            } else {
                None
            };
            let direct_peer_fp: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

            let server_cert = dev_server_cert_pem();
            let mut client = ClientSession::new(
                client_cfg,
                DEV_CERT_PEM,
                DEV_KEY_PEM,
                server_cert,
                DEV_SERVER_NAME,
                clock,
            )
            .map_err(|e| format!("会话构造失败: {e}"))?;
            if let Some(cb) = create_clipboard_backend(cfg.prefer_hardware) {
                client.enable_clipboard(Box::new(ToggleableClipboardBackend::new(
                    cb,
                    Arc::clone(&clipboard_enabled_flag),
                )));
            }

            // ── 自定义 connect 工厂：精确发射 ConnectPhase + 挂载 BridgeTransport（支持可选 mTLS） ──
            {
                let peer_addr = cfg.connect_addr;
                let pending = Arc::clone(&pending_control);
                let upd = Arc::clone(&update_tx);
                let disc = Arc::clone(&disconnect_flag);
                let started = Arc::clone(&streaming_started);
                let fp_slot = Arc::clone(&direct_peer_fp);
                let watch_only_flag = Arc::clone(&live_watch_only);
                let mut retained_endpoint: Option<QuicEndpoint> = None;
                let mut first_connect = true;
                client.set_connect_factory(Box::new(move || {
                    if disc.load(Ordering::Acquire) {
                        return Err(rc_types::Error::Network("用户主动断开".to_string()));
                    }
                    if retained_endpoint.is_none() {
                        if first_connect {
                            upd.push(SessionUpdate::Connecting {
                                phase: ConnectPhase::EndpointCreating,
                                watch_only: watch_only_flag.load(Ordering::Acquire),
                            });
                        }
                        let identity = Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM)?;
                        let net = NetConfig {
                            role: Role::Controller,
                            local_addr: "0.0.0.0:0"
                                .parse()
                                .expect("0.0.0.0:0 is a valid socket addr"),
                            peer_addr: Some(peer_addr),
                            mtu: rc_session::DEFAULT_SESSION_MTU,
                        };
                        retained_endpoint = Some(QuicEndpoint::new(&net, &identity)?);
                    }
                    if first_connect {
                        upd.push(SessionUpdate::Connecting {
                            phase: ConnectPhase::Connecting,
                            watch_only: watch_only_flag.load(Ordering::Acquire),
                        });
                    }
                    let endpoint = retained_endpoint.as_ref().expect("endpoint initialized");
                    let transport = if let Some(ref mtls_cfg) = direct_mtls {
                        let t = endpoint.connect_mtls_cancelable(
                            peer_addr,
                            mtls_cfg,
                            "",
                            Duration::from_secs(10),
                            &disc,
                        )?;
                        *fp_slot.lock().unwrap_or_else(|e| e.into_inner()) =
                            t.peer_mtls_identity().map(|id| id.cert_fingerprint_sha256);
                        t
                    } else {
                        let certs = Identity::certs_from_pem(dev_server_cert_pem())?;
                        endpoint.connect(
                            peer_addr,
                            &certs,
                            DEV_SERVER_NAME,
                            Duration::from_secs(10),
                        )?
                    };
                    if first_connect {
                        first_connect = false;
                        upd.push(SessionUpdate::Connecting {
                            phase: ConnectPhase::AwaitingWelcome,
                            watch_only: watch_only_flag.load(Ordering::Acquire),
                        });
                    }
                    Ok(Box::new(BridgeTransport {
                        inner: Box::new(transport),
                        pending_control: Arc::clone(&pending),
                        disconnect_flag: Arc::clone(&disc),
                        relay_shutdown: None,
                        streaming_started: Arc::clone(&started),
                    }))
                }));
            }

            // ── 管线工厂：prefer_hardware 时优先 VT + 离屏 MetalRenderer，失败降级假后端 ──
            client.set_pipeline_factory_for(make_pipeline_factory(None));

            // ── 连接 + 握手 ──
            let negotiated = match client.connect_and_handshake() {
                Ok(n) => n,
                Err(e) => {
                    if disconnect_flag.load(Ordering::Acquire) {
                        return Ok("用户主动断开".to_string());
                    }
                    return Err(format!("连接/握手失败: {e}"));
                }
            };
            let fp = direct_peer_fp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            (client, negotiated, "UDP LAN", cfg.encrypt_connection, fp)
        };
    streaming_started.store(true, Ordering::Release);
    #[cfg(target_os = "macos")]
    let _activity =
        rc_capture_macos::LatencyCriticalActivity::begin("RabbitControl 远控会话", true);
    let backend = backend_label
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let is_hardware_decoder = backend.starts_with("videotoolbox");

    let live_wo = live_watch_only.load(Ordering::Acquire);
    update_tx.push(SessionUpdate::Connecting {
        phase: ConnectPhase::FirstFramePending,
        watch_only: live_wo,
    });
    update_tx.push(SessionUpdate::Negotiated {
        width: negotiated.width,
        height: negotiated.height,
        fps: negotiated.fps,
        bitrate_bps: negotiated.bitrate_bps,
        backend: backend.clone(),
        watch_only: live_wo,
        transport: transport_label.to_string(),
        mtls_active,
        peer_cert_fingerprint,
    });

    // 音频播放状态：按需懒初始化并跟踪采样率/声道数（M9），若初始关闭音频则立即通知主机端静音以节省带宽。
    let mut audio_player: Option<Box<dyn AudioPlayer>> = None;
    let mut audio_player_format: Option<(u32, u8)> = None;
    let mut audio_muted = !cfg.audio_enabled;
    let mut pending_audio_ctrl: Option<Vec<u8>> = if audio_muted {
        Some(encode_audio_ctrl_muted(true))
    } else {
        None
    };

    // 初始 HUD 快照（prev = None → fps/bitrate 均为 0.0）。
    let initial_sample = sample_hud(&client, &clock_reader, negotiated);
    update_tx.push(SessionUpdate::Hud(HudSnapshot::derive_with_transport(
        None,
        &initial_sample,
        transport_label,
    )));
    let mut prev_hud = initial_sample;
    let mut last_hud_at = Instant::now();
    let stream_started_at = Instant::now();
    let mut first_frame_logged = false;
    let mut decode_error_logged = false;
    let mut last_portmap_poll_at = Instant::now();
    let mut last_portmap_snapshot: Vec<PortMapRuleSnapshot> = Vec::new();
    let mut pending_file_in: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_file_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_term_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_portmap_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut pending_display_tx: VecDeque<Vec<u8>> = VecDeque::new();
    let mut hud_width = negotiated.width;
    let mut hud_height = negotiated.height;

    // ── 驱动循环（无 duration 上限，直到 Disconnect 或对端 Bye/错误）──
    let mut last_busy = true;
    let mut epoch_sid = session_id;
    let mut disconnect_requested = false;
    let mut end_reason = "会话已结束".to_string();

    while client.state() != SessionState::Closed {
        // 1) 处理来自 UI 的控制命令。
        for cmd in cmd_rx.drain_all() {
            match cmd {
                SessionCommand::Disconnect => {
                    disconnect_requested = true;
                }
                SessionCommand::SetBitrate { bps } => {
                    if bps > 0 && client.state() == SessionState::Streaming {
                        let mut buf = Vec::new();
                        ControlMessage::SetBitrate {
                            bps,
                            reason: BitrateReason::Manual,
                        }
                        .encode(&mut buf);
                        pending_control
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push_back(buf);
                    }
                }
                SessionCommand::StageRect { width, height } => {
                    if width > 0 && height > 0 {
                        if let Some(rend) = renderer_handle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                        {
                            let _ = rend
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .resize((width, height));
                        }
                    }
                }
                SessionCommand::SetClipboardEnabled { enabled } => {
                    let prev = clipboard_enabled_flag.swap(enabled, Ordering::AcqRel);
                    if enabled && !prev {
                        // 重新开启时重置基线，避免把暂停期间复制的内容突发外发。
                        client.reset_clipboard_epoch();
                    }
                }
                SessionCommand::SetAudioMuted { muted } => {
                    audio_muted = muted;
                    if let Some(ref mut player) = audio_player {
                        player.set_muted(muted);
                    }
                    pending_audio_ctrl = Some(encode_audio_ctrl_muted(muted));
                }
                SessionCommand::SetStreamParams { width, height, fps } => {
                    if let Some(payload) =
                        display_ctrl::encode_display_ctrl(&DisplayControlMsg::SetStream {
                            width,
                            height,
                            fps,
                        })
                    {
                        pending_display_tx.push_back(payload);
                    }
                }
                SessionCommand::LockRemoteScreen => {
                    if let Some(payload) =
                        display_ctrl::encode_display_ctrl(&DisplayControlMsg::LockScreen)
                    {
                        pending_display_tx.push_back(payload);
                    }
                }
            }
        }
        if disconnect_requested || disconnect_flag.load(Ordering::Acquire) {
            end_reason = "用户主动断开".to_string();
            break;
        }

        // 2) 驱动会话状态机。
        match client.state() {
            SessionState::Streaming => {
                let now_us = u64::from((clock_reader)());
                client
                    .pump_probes(now_us)
                    .map_err(|e| format!("探测泵失败: {e}"))?;
                client
                    .start_bw_probe(now_us)
                    .map_err(|e| format!("带宽粗测失败: {e}"))?;
                if clipboard_enabled_flag.load(Ordering::Acquire) {
                    let _ = client.pump_clipboard(now_us);
                }
                let budget = if last_busy {
                    RUN_POLL_BUDGET
                } else {
                    RUN_IDLE_POLL_BUDGET
                };
                let tick_report = client
                    .tick(budget, Instant::now())
                    .map_err(|e| format!("会话运行失败: {e}"))?;
                if let Some(stop) = tick_report.closed {
                    end_reason = format!("对端已关闭 ({stop:?})");
                    break;
                }

                // 2a) 音频帧解码与播放（M9：采样率/声道数变更时先 stop 旧播放器再按新格式重建）。
                let audio_packets = client.drain_audio_frames();
                let audio_busy = !audio_packets.is_empty();
                for pkt in audio_packets {
                    if audio_muted {
                        continue;
                    }
                    let Some(samples) = decode_audio_packet_samples(&pkt) else {
                        continue;
                    };
                    if samples.is_empty() {
                        continue;
                    }
                    ensure_audio_player_for_format(
                        &mut audio_player,
                        &mut audio_player_format,
                        pkt.sample_rate,
                        pkt.channels,
                        audio_muted,
                        |sr, ch| {
                            if cfg.prefer_hardware {
                                rc_audio::create_audio_player(sr, ch)
                                    .unwrap_or_else(|_| Box::new(NullAudioPlayer::new()))
                            } else {
                                Box::new(NullAudioPlayer::new())
                            }
                        },
                    );
                    if let Some(player) = audio_player.as_mut() {
                        let frame = PcmFrame {
                            sample_rate: pkt.sample_rate,
                            channels: pkt.channels,
                            timestamp_us: pkt.timestamp_us,
                            samples,
                        };
                        let _ = player.push_frame(&frame);
                    }
                }

                // 2b) W2/W3/W4 工具复用通道收发：
                //     - W2 文件通道完全不进入 `tools_hub.lock()`，仅通过非阻塞有界通道与 `rc-file-client-worker` 交换线帧（H12）；
                //     - 严格不在持有 `tools_hub` 锁期间做网络 I/O 或文件磁盘 I/O；
                //     - 各通道背压时保留待重试队列且停止拉取对应引擎，零丢块。
                let incoming_tools = client.drain_tool_messages();
                let mut tools_busy = !incoming_tools.is_empty();
                let mut new_term_events: Vec<TerminalEvent> = Vec::new();
                let poll_file = pending_file_tx.is_empty();
                let poll_term = pending_term_tx.is_empty();
                let poll_portmap = pending_portmap_tx.is_empty();

                let mut non_file_incoming: Vec<(u8, Vec<u8>)> = Vec::new();
                for (channel, payload) in incoming_tools {
                    if channel == TOOL_CHANNEL_FILE {
                        pending_file_in.push_back(payload);
                    } else if channel == TOOL_CHANNEL_DISPLAY_CTRL {
                        if let Some(msg) = display_ctrl::decode_display_ctrl(&payload) {
                            if let Some(upd) =
                                handle_display_ctrl_msg(msg, &mut hud_width, &mut hud_height)
                            {
                                update_tx.push(upd);
                            }
                        }
                    } else {
                        non_file_incoming.push((channel, payload));
                    }
                }

                // H12：非阻塞冲刷入站文件帧至专用文件工作线程（满载时留待下拍重试，零丢帧）。
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

                // H12：在 `tools_hub.lock()` 之外非阻塞拉取文件工作线程产出的出站线帧。
                let file_out = if poll_file {
                    file_worker.try_recv_outgoing(MAX_TOOL_FRAMES_PER_TICK)
                } else {
                    Vec::new()
                };

                let (term_out, pm_out) = {
                    let mut hub = tools_hub.lock().unwrap_or_else(|e| e.into_inner());
                    let mut pm_dirty = false;
                    for (channel, payload) in non_file_incoming {
                        match channel {
                            TOOL_CHANNEL_TERMINAL => {
                                let evs = hub.terminal.handle_incoming(&payload);
                                for ev in &evs {
                                    match ev {
                                        TerminalEvent::Exited { term_id, .. }
                                        | TerminalEvent::Opened {
                                            term_id, ok: false, ..
                                        } => {
                                            hub.active_terminals.remove(term_id);
                                        }
                                        _ => {}
                                    }
                                }
                                new_term_events.extend(evs);
                            }
                            TOOL_CHANNEL_PORTMAP if hub.portmap.handle_incoming(&payload) => {
                                pm_dirty = true;
                            }
                            _ => {}
                        }
                    }

                    let t_out = if poll_term {
                        hub.terminal.poll_outgoing(MAX_TOOL_FRAMES_PER_TICK)
                    } else {
                        Vec::new()
                    };
                    let p_out = if poll_portmap {
                        hub.portmap.poll_outgoing(MAX_TOOL_FRAMES_PER_TICK)
                    } else {
                        Vec::new()
                    };

                    if pm_dirty
                        || !p_out.is_empty()
                        || last_portmap_poll_at.elapsed() >= PORTMAP_SNAPSHOT_INTERVAL
                    {
                        last_portmap_poll_at = Instant::now();
                        let cur_rules = hub.portmap.list_rules();
                        if cur_rules != last_portmap_snapshot {
                            last_portmap_snapshot = cur_rules.clone();
                            // 与 ToolClientHandle 共享严格 `hub -> portmap_latest` 锁序，防并发乱序覆盖。
                            *portmap_latest.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(cur_rules);
                        }
                    }

                    (t_out, p_out)
                };

                pending_file_tx.extend(file_out);
                pending_term_tx.extend(term_out);
                pending_portmap_tx.extend(pm_out);

                if client.state() == SessionState::Streaming {
                    if let Some(ctrl) = pending_audio_ctrl.as_ref() {
                        if client
                            .send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, ctrl.clone())
                            .is_ok()
                        {
                            pending_audio_ctrl = None;
                            tools_busy = true;
                        }
                    }
                    if flush_pending_tool_queue(&mut pending_file_tx, |frame| {
                        client.send_tool_message(TOOL_CHANNEL_FILE, frame)
                    }) {
                        tools_busy = true;
                    }
                    if flush_pending_tool_queue(&mut pending_term_tx, |frame| {
                        client.send_tool_message(TOOL_CHANNEL_TERMINAL, frame)
                    }) {
                        tools_busy = true;
                    }
                    if flush_pending_tool_queue(&mut pending_portmap_tx, |frame| {
                        client.send_tool_message(TOOL_CHANNEL_PORTMAP, frame)
                    }) {
                        tools_busy = true;
                    }
                    if flush_pending_tool_queue(&mut pending_display_tx, |frame| {
                        client.send_tool_message(TOOL_CHANNEL_DISPLAY_CTRL, frame)
                    }) {
                        tools_busy = true;
                    }
                }
                for ev in new_term_events {
                    terminal_events.push_drop_oldest(ev);
                }

                last_busy = tick_report.busy || tools_busy || audio_busy;

                // 3) 首帧诊断与 1s 节流推送 HUD 快照。
                let displayed_now = client.pipeline().displayed();
                if !first_frame_logged && displayed_now > 0 {
                    first_frame_logged = true;
                    eprintln!(
                        "rc-ui: first video frame presented (backend={backend}, displayed={displayed_now})"
                    );
                }

                if last_hud_at.elapsed() >= HUD_INTERVAL {
                    let mut cur_negotiated = client.negotiated().unwrap_or(negotiated);
                    cur_negotiated.width = hud_width;
                    cur_negotiated.height = hud_height;
                    let cur = sample_hud(&client, &clock_reader, cur_negotiated);
                    let snap =
                        HudSnapshot::derive_with_transport(Some(&prev_hud), &cur, transport_label);
                    update_tx.push(SessionUpdate::Hud(snap));
                    prev_hud = cur;
                    last_hud_at = Instant::now();

                    // 当本地启用 VideoToolbox 硬件解码但远端持续产出非 H.264 码流（例如被控端
                    // 未授屏幕录制权限而降级到 FakeEncoder）时，`displayed` 恒为 0 且持续出现
                    // `Outcome::DecodeError`。达到阈值时显式报错并结束会话，避免无限静默黑屏。
                    if is_hardware_decoder && displayed_now == 0 {
                        let decode_errors = count_decode_errors(&client);
                        if decode_errors > 0 && !decode_error_logged {
                            decode_error_logged = true;
                            eprintln!(
                                "rc-ui: hardware decoder reporting decode errors before first frame \
                                 (backend={backend}, decode_errors={decode_errors})"
                            );
                        }
                        if is_persistent_decode_failure(
                            is_hardware_decoder,
                            displayed_now,
                            decode_errors,
                            stream_started_at.elapsed(),
                        ) {
                            eprintln!(
                                "rc-ui: persistent hardware decode failure \
                                 (backend={backend}, displayed=0, decode_errors={decode_errors}); \
                                 ending session"
                            );
                            end_reason =
                                "远端画面解码失败（请确认被控端已授予屏幕录制权限）".to_string();
                            break;
                        }
                    }
                }
            }
            SessionState::Reconnecting => {
                if is_signaling_session {
                    end_reason = if transport_label == "UDP P2P" {
                        "P2P 连接已断开".to_string()
                    } else {
                        "中继连接已断开".to_string()
                    };
                    break;
                }
                let outcome = client
                    .tick_reconnect(u64::from((clock_reader)()))
                    .map_err(|e| format!("重连驱动失败: {e}"))?;
                match outcome {
                    ReconnectOutcome::Waiting { next_attempt_in_ms } => {
                        let nap =
                            Duration::from_millis(next_attempt_in_ms).min(RUN_RECONNECT_SLEEP);
                        thread::sleep(nap);
                    }
                    ReconnectOutcome::GaveUp => {
                        end_reason = "重连次数已耗尽".to_string();
                        break;
                    }
                    ReconnectOutcome::Resumed | ReconnectOutcome::Rebuilt => {
                        client.reset_clipboard_epoch();
                        if let Some(n) = client.negotiated() {
                            hud_width = n.width;
                            hud_height = n.height;
                        }
                        // M9：重连恢复或重建后若本地处于静音态，重新向主机同步静音控制消息。
                        if audio_muted {
                            pending_audio_ctrl = Some(encode_audio_ctrl_muted(true));
                        }
                    }
                    ReconnectOutcome::Connecting => {}
                }
            }
            _ => break,
        }

        // 4) 纪元检测：T17 全新重建时重锚会话钟（同 rc-cli/src/cmd/client.rs）。
        if client.session_id() != epoch_sid {
            epoch_sid = client.session_id();
            *clock_anchor.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            client.reset_clipboard_epoch();
        }
    }

    if let Some(ref mut player) = audio_player {
        player.stop();
    }
    drop(audio_player);

    if lock_on_disconnect.load(Ordering::Acquire) && client.state() == SessionState::Streaming {
        if let Some(payload) = display_ctrl::encode_display_ctrl(&DisplayControlMsg::LockScreen) {
            if client
                .send_tool_message(TOOL_CHANNEL_DISPLAY_CTRL, payload)
                .is_ok()
            {
                let flush_until = Instant::now() + Duration::from_millis(150);
                while Instant::now() < flush_until && client.state() == SessionState::Streaming {
                    let _ = client.tick(Duration::from_millis(5), Instant::now());
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    // 优雅关闭（若尚未 Closed，发 Bye(Normal) 并执行 S6 停机序列）。
    if client.state() != SessionState::Closed {
        let _ = client.close(ByeReason::Normal);
    }

    Ok(end_reason)
}

/// 构造控制端管线（硬件优先 + 假后端降级）。
fn build_controller_pipeline(
    pcfg: &PipelineConfig,
    prefer_hardware: bool,
    session_start: Instant,
    preallocated_layer: Option<&MainThreadMetalLayer>,
) -> rc_types::Result<BuiltPipeline> {
    if prefer_hardware {
        #[cfg(target_os = "macos")]
        {
            match try_hardware_controller_stack(pcfg, session_start, preallocated_layer) {
                Ok(hw) => return Ok(hw),
                Err(e) => {
                    eprintln!(
                        "rc-ui: hardware controller stack unavailable, falling back to fake: {e}"
                    );
                }
            }
            let (pipeline, renderer) = fake_controller_stack(pcfg)?;
            return Ok(BuiltPipeline {
                pipeline,
                backend_label: "fake(降级)".to_string(),
                renderer,
                layer_addr: 0,
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (session_start, preallocated_layer);
            let (pipeline, renderer) = fake_controller_stack(pcfg)?;
            return Ok(BuiltPipeline {
                pipeline,
                backend_label: "fake(降级)".to_string(),
                renderer,
                layer_addr: 0,
            });
        }
    }
    let _ = session_start;
    let (pipeline, renderer) = fake_controller_stack(pcfg)?;
    Ok(BuiltPipeline {
        pipeline,
        backend_label: "fake".to_string(),
        renderer,
        layer_addr: 0,
    })
}

/// macOS 硬件栈：VideoToolbox 解码 + 离屏 `MetalRenderer`（路径 A，暴露 `layer_ptr`）。
#[cfg(target_os = "macos")]
fn try_hardware_controller_stack(
    pcfg: &PipelineConfig,
    session_start: Instant,
    preallocated_layer: Option<&MainThreadMetalLayer>,
) -> rc_types::Result<BuiltPipeline> {
    let mut hw_cfg = pcfg.clone();
    hw_cfg.prefer_hardware = true;
    let (decoder, dl) = rc_pipeline::select_decoder_labeled(&hw_cfg)?;
    if dl != "videotoolbox" {
        return Err(rc_types::Error::InvalidConfig(format!(
            "hardware decoder unavailable, got {dl}"
        )));
    }
    let metal_cfg = rc_render_macos::MetalRendererConfig {
        size: (pcfg.resolution.width, pcfg.resolution.height),
        vsync: false,
        session_start: Some(session_start),
        cursor_compositing: true,
    };
    let metal = match preallocated_layer {
        Some(layer) => rc_render_macos::MetalRenderer::from_raw_layer(layer.as_ptr(), metal_cfg)?,
        None => rc_render_macos::MetalRenderer::new(metal_cfg)?,
    };
    let layer_addr = metal.layer_ptr() as usize;
    let shared: RendererHandle = Arc::new(Mutex::new(Box::new(metal)));
    let wrapper = SharedRenderer {
        inner: Arc::clone(&shared),
    };
    let pipeline = ControllerPipeline::with_backends(hw_cfg, decoder, Box::new(wrapper))?;
    Ok(BuiltPipeline {
        pipeline,
        backend_label: format!("{dl}+metal"),
        renderer: shared,
        layer_addr,
    })
}

/// 假后端控制端栈：`FakeDecoder` + `FakeRenderer`。
fn fake_controller_stack(
    pcfg: &PipelineConfig,
) -> rc_types::Result<(ControllerPipeline, RendererHandle)> {
    let decoder = rc_decode::FakeDecoder::new(rc_decode::FakeDecoderConfig {
        resolution: pcfg.resolution,
        ..rc_decode::FakeDecoderConfig::default()
    })?;
    let renderer = rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
        size: (pcfg.resolution.width, pcfg.resolution.height),
        ..rc_render::FakeRendererConfig::default()
    })?;
    let shared: RendererHandle = Arc::new(Mutex::new(Box::new(renderer)));
    let wrapper = SharedRenderer {
        inner: Arc::clone(&shared),
    };
    let pipeline =
        ControllerPipeline::with_backends(pcfg.clone(), Box::new(decoder), Box::new(wrapper))?;
    Ok((pipeline, shared))
}

/// 统计控制端 trace 环形缓冲中 `Outcome::DecodeError` 的帧数。
fn count_decode_errors(client: &ClientSession) -> usize {
    client
        .pipeline()
        .trace()
        .frames()
        .iter()
        .filter(|f| f.outcome() == Some(rc_trace::Outcome::DecodeError))
        .count()
}

/// 判定硬件解码器是否处于持续不可恢复的码流不兼容/解码失败状态（纯逻辑，便于单测）。
///
/// 仅在本地启用 VideoToolbox 硬件解码、从未成功上屏（`displayed == 0`）、累计
/// `Outcome::DecodeError` 达到阈值且串流已持续足够时长时返回 `true`：
/// - 静止桌面（0 帧，0 解码错误）返回 `false`；
/// - 正常已出画（`displayed > 0`）返回 `false`；
/// - 假后端测试（`is_hardware_decoder == false`）返回 `false`。
fn is_persistent_decode_failure(
    is_hardware_decoder: bool,
    displayed: u64,
    decode_errors: usize,
    stream_elapsed: Duration,
) -> bool {
    is_hardware_decoder
        && displayed == 0
        && decode_errors >= CONSECUTIVE_DECODE_ERROR_LIMIT
        && stream_elapsed >= PERSISTENT_DECODE_ERROR_MIN_ELAPSED
}

/// 从 `ClientSession` 提取当前 [`HudSample`]（全注入数据，铁律 4）。
fn sample_hud(
    client: &ClientSession,
    clock: &SessionClock,
    negotiated: SessionParams,
) -> HudSample {
    let at_us = u64::from((clock)());
    let displayed = client.pipeline().displayed();
    let net = client.net_stats();
    let probe = client.probe_snapshot();
    let rtt_us = probe.rtt_us_median.and_then(|v| u32::try_from(v).ok());
    let e2e_p50_ms = compute_recent_e2e_p50_ms(client);
    HudSample {
        at_us,
        displayed,
        bytes_recv: net.bytes_recv,
        rtt_us,
        loss_rate: net.loss_rate,
        e2e_p50_ms,
        width: negotiated.width,
        height: negotiated.height,
    }
}

/// 从最近若干已上屏帧计算偏移折算后的端到端帧时间 p50（毫秒）。
fn compute_recent_e2e_p50_ms(client: &ClientSession) -> Option<f64> {
    let mut samples_ms: Vec<f64> = Vec::with_capacity(E2E_WINDOW_FRAMES);
    for frame in client.pipeline().trace().frames().iter().rev() {
        if samples_ms.len() >= E2E_WINDOW_FRAMES {
            break;
        }
        if frame.outcome() != Some(rc_trace::Outcome::Displayed) {
            continue;
        }
        let Some(captured_host) = frame.at(rc_trace::Mark::Captured) else {
            continue;
        };
        let Some(presented_local) = frame.at(rc_trace::Mark::Presented) else {
            continue;
        };
        let Some(captured_local) = client.host_to_local_us(captured_host.0) else {
            continue;
        };
        let presented_u64 = u64::from(presented_local.0);
        if presented_u64 >= captured_local {
            samples_ms.push((presented_u64 - captured_local) as f64 / 1000.0);
        }
    }
    if samples_ms.is_empty() {
        return None;
    }
    samples_ms.sort_by(|a, b| a.total_cmp(b));
    Some(samples_ms[samples_ms.len() / 2])
}

/// 解析控制端收到的 [`DisplayControlMsg`]，同步更新 HUD 尺寸并产出对应的 [`SessionUpdate`]。
fn handle_display_ctrl_msg(
    msg: DisplayControlMsg,
    hud_width: &mut u32,
    hud_height: &mut u32,
) -> Option<SessionUpdate> {
    match msg {
        DisplayControlMsg::Capabilities {
            current_width,
            current_height,
            current_fps,
            native_width,
            native_height,
            max_width,
            max_height,
            max_fps,
            presets,
        } => {
            if current_width > 0 && current_height > 0 {
                *hud_width = current_width;
                *hud_height = current_height;
            }
            let resolved_presets = if presets.is_empty() {
                display_ctrl::build_resolution_presets(
                    native_width,
                    native_height,
                    max_width,
                    max_height,
                )
            } else {
                presets
            };
            Some(SessionUpdate::DisplayConfig {
                width: current_width,
                height: current_height,
                fps: current_fps,
                native_width,
                native_height,
                max_fps,
                presets: resolved_presets,
            })
        }
        DisplayControlMsg::Wallpaper { mime, data_base64 } => {
            if matches!(mime.as_str(), "image/jpeg" | "image/png")
                && !data_base64.is_empty()
                && data_base64.len() <= 90_000
                && display_ctrl::base64_decode(&data_base64).is_some()
            {
                Some(SessionUpdate::Wallpaper {
                    data_url: format!("data:{mime};base64,{data_base64}"),
                })
            } else {
                None
            }
        }
        DisplayControlMsg::SetStream { .. } | DisplayControlMsg::LockScreen => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::MouseButton;

    #[test]
    fn session_channels_are_bounded() {
        // 铁律 1 UI 侧延伸：常量钉死 64 / 256。
        assert_eq!(COMMAND_CHANNEL_CAP, 64);
        assert_eq!(INPUT_CHANNEL_CAP, 256);

        // 命令通道满载 64：再推不阻塞，丢最旧保留最新 64 条。
        let cmd_q: BoundedQueue<SessionCommand> = BoundedQueue::new(COMMAND_CHANNEL_CAP);
        for bps in 1..=100u32 {
            cmd_q.push_drop_oldest(SessionCommand::SetBitrate { bps });
        }
        assert_eq!(cmd_q.len(), COMMAND_CHANNEL_CAP);
        let drained = cmd_q.drain_all();
        assert_eq!(drained.len(), 64);
        assert_eq!(drained[0], SessionCommand::SetBitrate { bps: 37 });
        assert_eq!(drained[63], SessionCommand::SetBitrate { bps: 100 });

        // 输入通道满载 256：纯移动事件满载丢最旧、不阻塞。
        let input_q = BoundedInputQueue::new(INPUT_CHANNEL_CAP);
        for i in 0..300u16 {
            input_q.push(InputEvent::MouseMove { x: i, y: i });
        }
        assert_eq!(input_q.len(), INPUT_CHANNEL_CAP);
        let moves = input_q.drain_all();
        assert_eq!(moves.len(), 256);
        assert_eq!(moves[0], InputEvent::MouseMove { x: 44, y: 44 });
        assert_eq!(moves[255], InputEvent::MouseMove { x: 299, y: 299 });

        // 满载优先级：关键事件不被后续 MouseMove 挤掉。
        let key_ev = InputEvent::Key {
            scancode: 0x1E,
            pressed: true,
            modifiers: 0,
        };
        let click_ev = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        };
        input_q.push(key_ev);
        input_q.push(click_ev);
        for i in 0..400u16 {
            input_q.push(InputEvent::MouseMove { x: i, y: 0 });
        }
        assert_eq!(input_q.len(), INPUT_CHANNEL_CAP);
        let mixed = input_q.drain_all();
        assert!(
            mixed.contains(&key_ev),
            "关键键盘事件不得被 MouseMove 挤占丢弃"
        );
        assert!(
            mixed.contains(&click_ev),
            "关键鼠标按键事件不得被 MouseMove 挤占丢弃"
        );
    }

    struct DropSpyRenderer {
        dropped: Arc<AtomicBool>,
    }

    impl Renderer for DropSpyRenderer {
        fn present(
            &mut self,
            _frame: rc_decode::DecodedFrame,
        ) -> rc_types::Result<rc_types::Timestamp> {
            Ok(rc_types::Timestamp(0))
        }

        fn resize(&mut self, _size: (u32, u32)) -> rc_types::Result<()> {
            Ok(())
        }

        fn set_vsync(&mut self, _on: bool) {}

        fn set_cursor(&mut self, _pos: (i32, i32), _visible: bool) {}
    }

    impl Drop for DropSpyRenderer {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn layer_lease_keeps_renderer_alive_until_last_clone_drops() {
        let dropped = Arc::new(AtomicBool::new(false));
        let renderer: RendererHandle = Arc::new(Mutex::new(Box::new(DropSpyRenderer {
            dropped: Arc::clone(&dropped),
        })));

        // 地址为 0 时不得构造租约（假后端路径）。
        assert!(LayerLease::from_renderer(Arc::clone(&renderer), 0).is_none());

        let lease = LayerLease::from_renderer(Arc::clone(&renderer), 0x1234_5678)
            .expect("non-zero layer addr should construct lease");
        assert_eq!(lease.addr(), 0x1234_5678);
        assert_eq!(lease.as_ptr() as usize, 0x1234_5678);

        // 模拟会话线程退出、释放管线与本地 RendererHandle。
        drop(renderer);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "会话线程释放渲染器后，LayerLease 必须继续保活底层 MetalRenderer"
        );

        // 模拟主线程闭包 clone 租约后原持有者释放。
        let cloned = lease.clone();
        assert!(lease.same_layer(&cloned));
        drop(lease);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "主线程闭包仍持有 clone 租约期间不得释放渲染器"
        );

        // 相同地址但不同渲染器实例必须判定为不同层（防 ABA）。
        let other_dropped = Arc::new(AtomicBool::new(false));
        let other_renderer: RendererHandle = Arc::new(Mutex::new(Box::new(DropSpyRenderer {
            dropped: Arc::clone(&other_dropped),
        })));
        let other_lease = LayerLease::from_renderer(other_renderer, 0x1234_5678).unwrap();
        assert!(!cloned.same_layer(&other_lease));

        drop(cloned);
        assert!(
            dropped.load(Ordering::SeqCst),
            "最后一份 LayerLease 释放后底层渲染器应立即析构"
        );
        drop(other_lease);
        assert!(other_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn toggleable_clipboard_backend_respects_enabled_flag() {
        let fake = rc_session::testkit::SharedFakeClipboard::new();
        fake.simulate_local_copy(Some("initial"));
        let flag = Arc::new(AtomicBool::new(false));
        let mut backend =
            ToggleableClipboardBackend::new(Box::new(fake.clone()), Arc::clone(&flag));

        // 禁用态：不读取真实内容，也不写回本地剪贴板。
        assert_eq!(backend.read_text().unwrap(), None);
        backend.write_text("from-peer-while-disabled").unwrap();
        assert!(fake.writes().is_empty());
        let frozen_count = backend.change_count().unwrap();
        fake.simulate_local_copy(Some("changed-while-disabled"));
        assert_eq!(backend.change_count().unwrap(), frozen_count);

        // 启用态：正常读写并推进变更计数。
        flag.store(true, Ordering::Release);
        assert!(backend.change_count().unwrap() > frozen_count);
        assert_eq!(
            backend.read_text().unwrap(),
            Some("changed-while-disabled".to_string())
        );
        backend.write_text("from-peer-enabled").unwrap();
        assert_eq!(fake.writes(), vec!["from-peer-enabled".to_string()]);
    }

    #[test]
    fn audio_packet_decoding_supports_ima_adpcm_and_pcm16() {
        use rc_audio::AudioCapture;

        let pcm = vec![1000i16, -1000, 2000, -2000, 3000, -3000];
        let mut pcm_bytes = Vec::with_capacity(pcm.len() * 2);
        for &s in &pcm {
            pcm_bytes.extend_from_slice(&s.to_le_bytes());
        }
        let pcm_pkt = AudioFramePacket {
            seq: 1,
            timestamp_us: 0,
            sample_rate: 48_000,
            channels: 2,
            codec: AUDIO_CODEC_PCM16,
            data: pcm_bytes,
        };
        assert_eq!(decode_audio_packet_samples(&pcm_pkt), Some(pcm));

        let mut cap = rc_audio::FakeAudioCapture::new();
        let frame = cap.try_recv_frame().expect("fake audio frame");
        let encoded = rc_audio::encode_ima_adpcm(&frame.samples, frame.channels);
        let adpcm_pkt = AudioFramePacket {
            seq: 2,
            timestamp_us: frame.timestamp_us,
            sample_rate: frame.sample_rate,
            channels: frame.channels,
            codec: AUDIO_CODEC_IMA_ADPCM,
            data: encoded,
        };
        let decoded = decode_audio_packet_samples(&adpcm_pkt).expect("decode IMA-ADPCM");
        assert_eq!(decoded.len(), frame.samples.len());

        let bad_pkt = AudioFramePacket {
            seq: 3,
            timestamp_us: 0,
            sample_rate: 48_000,
            channels: 2,
            codec: 99,
            data: vec![1, 2, 3, 4],
        };
        assert_eq!(decode_audio_packet_samples(&bad_pkt), None);
    }

    #[test]
    fn tool_client_handle_tracks_and_closes_active_terminals() {
        let hub = Arc::new(Mutex::new(ToolClientHub::new()));
        let file_events = Arc::new(BoundedQueue::new(16));
        let (mut worker, file_handle) = FileClientWorker::spawn(Arc::clone(&file_events));
        let handle = ToolClientHandle {
            hub: Arc::clone(&hub),
            file: file_handle,
            portmap_latest: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
        };

        assert!(handle.active_terminal_ids().is_empty());
        assert!(handle.terminal_open(0, 24).is_err());
        assert!(handle.terminal_open(513, 24).is_err());
        let id1 = handle.terminal_open(80, 24).expect("open 1");
        let id2 = handle.terminal_open(100, 30).expect("open 2");
        assert!(handle.has_active_terminal(id1));
        assert!(handle.has_active_terminal(id2));
        assert!(!handle.has_active_terminal(999_999));
        assert_eq!(handle.active_terminal_ids(), vec![id1, id2]);

        assert!(handle.terminal_resize(id1, 0, 24).is_err());
        assert!(handle.terminal_resize(id1, 80, 513).is_err());
        assert!(handle.terminal_resize(id1, 120, 40).is_ok());

        assert!(handle.terminal_input(id1, b"ls\n").is_ok());
        assert!(handle.terminal_input(999_999, b"ls\n").is_err());
        let oversized = vec![b'x'; MAX_TERMINAL_INPUT_BYTES + 1];
        assert!(handle.terminal_input(id1, &oversized).is_err());

        handle.terminal_close(id1).expect("close 1");
        assert!(!handle.has_active_terminal(id1));
        assert!(handle.terminal_input(id1, b"after-close").is_err());
        assert_eq!(handle.active_terminal_ids(), vec![id2]);

        let closed = handle.close_all_terminals();
        assert_eq!(closed, vec![id2]);
        assert!(handle.active_terminal_ids().is_empty());

        worker.shutdown();
    }

    #[test]
    fn ensure_audio_player_recreates_and_stops_old_player_on_format_change() {
        struct SpyAudioPlayer {
            stopped: Arc<AtomicBool>,
            muted: Arc<AtomicBool>,
        }

        impl AudioPlayer for SpyAudioPlayer {
            fn push_frame(&mut self, _frame: &PcmFrame) -> Result<(), String> {
                Ok(())
            }
            fn set_muted(&mut self, muted: bool) {
                self.muted.store(muted, Ordering::SeqCst);
            }
            fn is_muted(&self) -> bool {
                self.muted.load(Ordering::SeqCst)
            }
            fn stop(&mut self) {
                self.stopped.store(true, Ordering::SeqCst);
            }
        }

        let mut player: Option<Box<dyn AudioPlayer>> = None;
        let mut format: Option<(u32, u8)> = None;
        let stopped_1 = Arc::new(AtomicBool::new(false));
        let muted_1 = Arc::new(AtomicBool::new(false));
        let stopped_2 = Arc::new(AtomicBool::new(false));
        let muted_2 = Arc::new(AtomicBool::new(false));
        let mut created_formats: Vec<(u32, u8)> = Vec::new();

        ensure_audio_player_for_format(&mut player, &mut format, 48_000, 2, false, |sr, ch| {
            created_formats.push((sr, ch));
            Box::new(SpyAudioPlayer {
                stopped: Arc::clone(&stopped_1),
                muted: Arc::clone(&muted_1),
            })
        });
        assert_eq!(format, Some((48_000, 2)));
        assert_eq!(created_formats, vec![(48_000, 2)]);
        assert!(!stopped_1.load(Ordering::SeqCst));

        // 相同格式不重建播放器。
        ensure_audio_player_for_format(&mut player, &mut format, 48_000, 2, false, |sr, ch| {
            created_formats.push((sr, ch));
            Box::new(NullAudioPlayer::new())
        });
        assert_eq!(created_formats, vec![(48_000, 2)]);
        assert!(!stopped_1.load(Ordering::SeqCst));

        // 格式切换到 44.1kHz 单声道且静音态为 true：旧播放器必须被 stop()，新播放器继承 muted=true。
        ensure_audio_player_for_format(&mut player, &mut format, 44_100, 1, true, |sr, ch| {
            created_formats.push((sr, ch));
            Box::new(SpyAudioPlayer {
                stopped: Arc::clone(&stopped_2),
                muted: Arc::clone(&muted_2),
            })
        });
        assert!(
            stopped_1.load(Ordering::SeqCst),
            "格式变更时必须先 stop() 旧 AudioPlayer"
        );
        assert_eq!(format, Some((44_100, 1)));
        assert_eq!(created_formats, vec![(48_000, 2), (44_100, 1)]);
        assert!(muted_2.load(Ordering::SeqCst));
        assert!(!stopped_2.load(Ordering::SeqCst));
    }

    #[test]
    fn file_client_worker_offloads_commands_and_updates_cached_transfers_without_hub_lock() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-file-client-worker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        let sample = dir.join("upload_probe.bin");
        std::fs::write(&sample, b"0123456789abcdef").expect("write probe file");

        let hub = Arc::new(Mutex::new(ToolClientHub::new()));
        let file_events = Arc::new(BoundedQueue::new(32));
        let (mut worker, file_handle) = FileClientWorker::spawn(Arc::clone(&file_events));
        let tools = ToolClientHandle {
            hub: Arc::clone(&hub),
            file: file_handle,
            portmap_latest: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
        };

        // 即使外部正持有 tools_hub 锁，文件上传与传输列表查询也绝不阻塞（证明文件引擎完全脱离 tools_hub）。
        let _hold_hub = hub.lock().unwrap();
        let up_id = tools
            .file_upload(sample.to_string_lossy().as_ref(), "/tmp")
            .expect("file_upload must succeed while tools_hub is locked");
        let cached = tools.file_list_transfers();
        assert!(
            cached
                .iter()
                .any(|t| t.id == up_id && t.name == "upload_probe.bin"),
            "cached_transfers must reflect upload immediately, got: {cached:?}"
        );
        drop(_hold_hub);

        // 工作线程应产出 UploadStart 线帧。
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got_frame = false;
        while Instant::now() < deadline {
            if !worker.try_recv_outgoing(4).is_empty() {
                got_frame = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            got_frame,
            "rc-file-client-worker must emit UploadStart frame"
        );

        tools.file_cancel(up_id).expect("file_cancel");
        worker.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_pending_tool_queue_retains_unsent_frames_on_backpressure() {
        let mut q: VecDeque<Vec<u8>> = vec![vec![1], vec![2], vec![3]].into();
        let mut delivered = Vec::new();
        let mut calls = 0usize;

        let sent_any = flush_pending_tool_queue(&mut q, |frame| {
            calls += 1;
            if calls == 2 {
                Err("full")
            } else {
                delivered.push(frame);
                Ok(())
            }
        });
        assert!(sent_any);
        assert_eq!(delivered, vec![vec![1]]);
        assert_eq!(q.len(), 2);

        let sent_rest = flush_pending_tool_queue(&mut q, |frame| {
            delivered.push(frame);
            Ok::<(), ()>(())
        });
        assert!(sent_rest);
        assert!(q.is_empty());
        assert_eq!(delivered, vec![vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn persistent_decode_failure_triggers_only_for_hw_zero_displayed_over_threshold() {
        let min = PERSISTENT_DECODE_ERROR_MIN_ELAPSED;
        let limit = CONSECUTIVE_DECODE_ERROR_LIMIT;

        // 达标场景：硬件解码器 + displayed==0 + 达到连续错误阈值与最小时长 → true。
        assert!(is_persistent_decode_failure(true, 0, limit, min));
        assert!(is_persistent_decode_failure(
            true,
            0,
            limit + 50,
            min + Duration::from_secs(2)
        ));

        // 假后端测试 / 非硬件解码器 → false。
        assert!(!is_persistent_decode_failure(false, 0, limit + 100, min));
        // 已有成功上屏帧（后续偶发解码错）→ false。
        assert!(!is_persistent_decode_failure(true, 1, limit + 100, min));
        // 静止屏幕（0 帧，0 错误）→ false。
        assert!(!is_persistent_decode_failure(
            true,
            0,
            0,
            Duration::from_secs(60)
        ));
        // 尚未达到错误次数或观察时长 → false。
        assert!(!is_persistent_decode_failure(true, 0, limit - 1, min));
        assert!(!is_persistent_decode_failure(
            true,
            0,
            limit,
            min - Duration::from_millis(1)
        ));
    }

    #[test]
    fn handle_display_ctrl_msg_supports_custom_presets_and_wallpaper_validation() {
        let mut hud_w = 1280;
        let mut hud_h = 720;

        // 1) 非空 presets 直接沿用；为空时回退 build_resolution_presets。
        let custom_presets = vec![DisplayPreset {
            width: 2560,
            height: 1600,
            label: "2560 × 1600 (HiDPI)".to_string(),
            is_native: true,
            is_hidpi: true,
        }];
        let upd = handle_display_ctrl_msg(
            DisplayControlMsg::Capabilities {
                current_width: 2560,
                current_height: 1600,
                current_fps: 120,
                native_width: 2560,
                native_height: 1600,
                max_width: 2560,
                max_height: 1600,
                max_fps: 120,
                presets: custom_presets.clone(),
            },
            &mut hud_w,
            &mut hud_h,
        )
        .expect("capabilities update");
        assert_eq!((hud_w, hud_h), (2560, 1600));
        match upd {
            SessionUpdate::DisplayConfig { presets, .. } => assert_eq!(presets, custom_presets),
            other => panic!("expected DisplayConfig, got {other:?}"),
        }

        let fallback_upd = handle_display_ctrl_msg(
            DisplayControlMsg::Capabilities {
                current_width: 1920,
                current_height: 1080,
                current_fps: 60,
                native_width: 1920,
                native_height: 1080,
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                presets: Vec::new(),
            },
            &mut hud_w,
            &mut hud_h,
        )
        .expect("fallback capabilities update");
        match fallback_upd {
            SessionUpdate::DisplayConfig { presets, .. } => {
                assert!(!presets.is_empty());
                assert!(presets.iter().any(|p| p.is_native && p.width == 1920));
            }
            other => panic!("expected DisplayConfig, got {other:?}"),
        }

        // 2) Wallpaper 支持 image/jpeg 与 image/png，拒绝非法 mime、空/超长/非法 base64。
        let b64 = display_ctrl::base64_encode(b"\xff\xd8\xff\xe0");
        let wp = handle_display_ctrl_msg(
            DisplayControlMsg::Wallpaper {
                mime: "image/jpeg".to_string(),
                data_base64: b64.clone(),
            },
            &mut hud_w,
            &mut hud_h,
        );
        assert_eq!(
            wp,
            Some(SessionUpdate::Wallpaper {
                data_url: format!("data:image/jpeg;base64,{b64}"),
            })
        );
        assert_eq!(
            handle_display_ctrl_msg(
                DisplayControlMsg::Wallpaper {
                    mime: "image/svg+xml".to_string(),
                    data_base64: b64.clone(),
                },
                &mut hud_w,
                &mut hud_h,
            ),
            None
        );
        assert_eq!(
            handle_display_ctrl_msg(
                DisplayControlMsg::Wallpaper {
                    mime: "image/png".to_string(),
                    data_base64: "not!base64".to_string(),
                },
                &mut hud_w,
                &mut hud_h,
            ),
            None
        );
        assert_eq!(
            handle_display_ctrl_msg(
                DisplayControlMsg::Wallpaper {
                    mime: "image/png".to_string(),
                    data_base64: "AAAA".repeat(22_501),
                },
                &mut hud_w,
                &mut hud_h,
            ),
            None
        );
    }

    #[test]
    fn ui_session_config_defaults_force_relay_to_false() {
        let json = serde_json::json!({
            "connect_addr": "127.0.0.1:9901",
            "watch_only": false,
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate_bps": 14000000,
            "prefer_hardware": true
        });
        let cfg: UiSessionConfig = serde_json::from_value(json).expect("deserialize config");
        assert!(!cfg.force_relay);
        assert!(cfg.clipboard_enabled);
        assert!(cfg.audio_enabled);
    }
}
