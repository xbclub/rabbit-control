//! 应用状态管理（T21 API 契约 §6 / `08-ui-design.md` §6.0 & §8.1）。
//!
//! 持有当前唯一活动会话（代际号 + `SessionHandle` + `CAMetalLayer` 保活租约状态机 +
//! 最新 stage 物理矩形）以及 §6.0 mock 设备表单一数据源。
//!
//! 全模块 `#![deny(unsafe_code)]`：通过 [`rc_ui::session_bridge::LayerLease`] 持有底层
//! 渲染器强引用，确保任意派发到主线程的 `attach_layer` / `sync_layer_frame` / `detach_layer`
//! 闭包执行时 `CAMetalLayer` 必然存活；通过会话代际号 `id` 防止旧会话闭包误操作新会话。

#![deny(unsafe_code)]

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rc_ui::app_prefs::{self, AppPrefs};
use rc_ui::auth_client::{self, AuthMaintainerHandle, AuthSessionManager};
use rc_ui::cert_manager::{self, CertManager};
use rc_ui::host_bridge::{self, HostApprovalPrompt, HostHandle, HostUpdate};
use rc_ui::lan_discovery::{self, LanDiscoveryHandle};
use rc_ui::metal_attach::Frame;
use rc_ui::server_config::{self, ServerConfig, ServerStatusSnapshot};
use rc_ui::session_bridge::{LayerLease, SessionHandle, SessionUpdate};
use rc_ui::signal_client::SignalClientHandle;

/// §6.0 侧栏设备条目（IPC 序列化字段与 `mock.js` 保持一致）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEntry {
    /// 路由用设备标识（非展示文案）。
    pub id: &'static str,
    /// 展示名称（逐字取自 `08-ui-design.md` §6.0）。
    pub name: &'static str,
    /// 设备类型（`"windows"` / `"mac"`）。
    pub kind: &'static str,
    /// Lucide 图标名（`"app-window"` / `"laptop"`）。
    pub icon: &'static str,
    /// 在线状态。
    pub online: bool,
    /// 是否本机。
    pub is_local: bool,
}

/// `CAMetalLayer` 在 `"session"` 窗口 `NSView` 上的挂载状态机。
#[derive(Debug, Clone, Default)]
pub enum LayerAttachment {
    /// 未挂载（假后端、尚未收到 `Negotiated`、挂载失败或已摘除）。
    #[default]
    None,
    /// 已派发主线程 `attach_layer` 闭包，等待执行结果；持有租约保活渲染器。
    Attaching(LayerLease),
    /// 主线程 `attach_layer` 已成功返回；持有租约保活渲染器。
    Attached(LayerLease),
}

impl LayerAttachment {
    /// 是否处于未挂载状态。
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// 取出当前持有的保活租约（用于摘除），并将状态复位为 [`Self::None`]。
    pub fn take_lease(&mut self) -> Option<LayerLease> {
        match std::mem::replace(self, Self::None) {
            Self::None => None,
            Self::Attaching(lease) | Self::Attached(lease) => Some(lease),
        }
    }

    /// 若已确认挂载，返回租约副本（供 `sync_layer_frame` 保活）。
    pub fn attached_lease(&self) -> Option<LayerLease> {
        match self {
            Self::Attached(lease) => Some(lease.clone()),
            Self::None | Self::Attaching(_) => None,
        }
    }

    /// 当前是否正处于对 `lease` 的待挂载状态。
    pub fn is_attaching(&self, lease: &LayerLease) -> bool {
        matches!(self, Self::Attaching(cur) if cur.same_layer(lease))
    }

    /// 当前是否已确认挂载 `lease`。
    pub fn is_attached(&self, lease: &LayerLease) -> bool {
        matches!(self, Self::Attached(cur) if cur.same_layer(lease))
    }
}

/// 主线程完成 `attach_layer` 尝试后的状态收敛结论。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompleteAttachOutcome {
    /// 挂载成功且会话仍有效；若期间 `stage_rect` 更新过矩形，则携带最新 `Frame` 供主线程补同步。
    Attached {
        /// 若在 `attach_layer` 执行间隙收到新的 stage 矩形，则立即同步该值。
        updated_frame: Option<Frame>,
    },
    /// 窗口缺失 / `ns_view()` 失败 / `attach_layer` 返回错误，已回滚为未挂载。
    Failed,
    /// `attach_layer` 执行期间会话已被断开或替换，主线程须立刻 `detach_layer` 避免残留子层。
    MustDetachNow,
}

/// 当前活动会话状态（泛型 `H` 默认为 [`SessionHandle`]，便于无窗口状态机回归单测）。
/// 当前活动远控会话的控制端元数据（用于主窗状态显示、壁纸归档与中继热切换）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveSessionMeta {
    /// 原始连接目标（`ip:port` 或 9 位设备码）。
    pub target: String,
    /// 用于 `device_snapshots` 与壁纸缓存的规范化设备键。
    pub device_key: String,
    /// 设备显示名称。
    pub device_name: String,
    /// 可用于安全中转重连的 9 位设备码（若已知）。
    pub relay_target_id: Option<String>,
    /// 本次会话重连参数（切换安全中转开关时复用）。
    pub password: Option<String>,
    /// 请求画面宽度。
    pub width: u32,
    /// 请求画面高度。
    pub height: u32,
    /// 请求目标帧率。
    pub fps: u32,
    /// 请求目标码率（bps）。
    pub bitrate_bps: u32,
    /// 是否优先硬件编解码。
    pub prefer_hardware: bool,
    /// 是否启用双向剪贴板同步。
    pub clipboard_enabled: bool,
    /// 是否启用远端音频。
    pub audio_enabled: bool,
    /// 是否强制走中继。
    pub force_relay: bool,
    /// 本次会话是否开启端到端 mTLS 加密握手。
    pub encrypt_connection: bool,
    /// 当前会话实际 negotiated 是否激活了 mTLS/E2EE。
    pub mtls_active: bool,
    /// 对端设备证书 SHA-256 指纹（启用 mTLS 时由握手提取）。
    pub peer_cert_fingerprint: Option<String>,
    /// 握手是否已成功协商。
    pub connected: bool,
    /// 协商后的传输方式标签（`"UDP LAN"` / `"UDP P2P"` / `"Relay"`）。
    pub transport: Option<String>,
}

/// 面向主窗前端展示的当前活动会话只读状态快照（`get_active_session_info` / `session://active-state`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSessionInfo {
    /// 当前是否存在未断开的活动会话。
    pub active: bool,
    /// 原始连接目标。
    pub target: Option<String>,
    /// 规范化设备键。
    pub device_key: Option<String>,
    /// 设备名称。
    pub device_name: Option<String>,
    /// 是否观看模式。
    pub watch_only: bool,
    /// 是否已完成握手并正在远控中。
    pub connected: bool,
    /// 当前传输方式标签。
    pub transport: Option<String>,
    /// 是否已启用强制中继模式。
    pub force_relay: bool,
    /// 本次会话是否启用端到端 mTLS 加密。
    pub encrypt_connection: bool,
    /// 当前握手实际激活的 mTLS 状态。
    pub mtls_active: bool,
    /// 对端设备证书 SHA-256 指纹。
    pub peer_cert_fingerprint: Option<String>,
}

impl ActiveSessionInfo {
    /// 返回空会话（未控制任何设备）快照。
    pub fn inactive() -> Self {
        Self::default()
    }
}

pub struct ActiveSession<H = SessionHandle> {
    /// 进程内单调递增会话代际号，防止旧会话的主线程闭包误操作新会话。
    pub id: u64,
    /// 后台会话线程句柄（Drop 时自动发 `Disconnect` 并 join）。
    pub handle: H,
    /// 是否观看模式（只读，不注入输入）。
    pub watch_only: bool,
    /// 是否已发起 `Disconnect`（幂等防重入，并阻止后续挂载/尺寸同步）。
    pub disconnecting: bool,
    /// `CAMetalLayer` 挂载状态与保活租约。
    pub layer_state: LayerAttachment,
    /// 最近一次前端上报并换算为物理像素的 stage 矩形（原始 stage 容器，未裁切黑边）。
    pub last_stage_frame: Option<Frame>,
    /// 握手裁定的远端视频分辨率 `(width, height)`；用于 aspect-fit 视口与指针归一化对齐。
    pub video_size: Option<(u32, u32)>,
    /// 当前活动会话是否已触发过 W4 持久化端口映射自动恢复。
    pub portmaps_restored: bool,
    /// 会话控制端元数据。
    pub meta: ActiveSessionMeta,
}

impl<H> ActiveSession<H> {
    /// 构造新建活动会话（初始未挂载、未断开）。
    pub fn new(id: u64, handle: H, watch_only: bool) -> Self {
        Self {
            id,
            handle,
            watch_only,
            disconnecting: false,
            layer_state: LayerAttachment::None,
            last_stage_frame: None,
            video_size: None,
            portmaps_restored: false,
            meta: ActiveSessionMeta::default(),
        }
    }

    /// 写入会话元数据。
    pub fn set_meta(&mut self, meta: ActiveSessionMeta) {
        self.meta = meta;
    }

    /// 导出前端展示所需的会话状态快照。
    pub fn info(&self) -> ActiveSessionInfo {
        if self.disconnecting {
            return ActiveSessionInfo::inactive();
        }
        ActiveSessionInfo {
            active: true,
            target: (!self.meta.target.is_empty()).then(|| self.meta.target.clone()),
            device_key: (!self.meta.device_key.is_empty()).then(|| self.meta.device_key.clone()),
            device_name: (!self.meta.device_name.is_empty()).then(|| self.meta.device_name.clone()),
            watch_only: self.watch_only,
            connected: self.meta.connected,
            transport: self.meta.transport.clone(),
            force_relay: self.meta.force_relay,
            encrypt_connection: self.meta.encrypt_connection,
            mtls_active: self.meta.mtls_active,
            peer_cert_fingerprint: self.meta.peer_cert_fingerprint.clone(),
        }
    }

    /// 记录握手裁定的远端视频分辨率。
    pub fn set_video_size(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.video_size = Some((width, height));
        }
    }

    /// 计算当前应同步给 `CAMetalLayer` 与 `MetalRenderer` 的等比居中物理矩形。
    pub fn effective_layer_frame(&self, fallback: Frame) -> Frame {
        let stage = self.last_stage_frame.unwrap_or(fallback);
        match self.video_size {
            Some((vw, vh)) if vw > 0 && vh > 0 => stage.fit_aspect(vw, vh),
            _ => stage,
        }
    }

    /// 在收到 `Negotiated` 且持有硬件 `LayerLease` 时进入 `Attaching` 态，返回主线程挂载参数。
    pub fn begin_attach(
        &mut self,
        lease: LayerLease,
        fallback_frame: Frame,
    ) -> Option<(u64, LayerLease, Frame)> {
        if self.disconnecting || !self.layer_state.is_none() {
            return None;
        }
        self.layer_state = LayerAttachment::Attaching(lease.clone());
        let frame = self.effective_layer_frame(fallback_frame);
        Some((self.id, lease, frame))
    }
}

/// 主线程执行 `attach_layer` 前复核：仅当 `sid` 仍为当前未断开会话且处于 `Attaching(lease)`
/// 时返回本次挂载应使用的最新等比物理 `Frame`；否则返回 `None` 跳过挂载。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub fn should_attach_on_main_thread<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    lease: &LayerLease,
    fallback_frame: Frame,
) -> Option<Frame> {
    let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let active = guard.as_ref()?;
    if active.id == sid && !active.disconnecting && active.layer_state.is_attaching(lease) {
        Some(active.effective_layer_frame(fallback_frame))
    } else {
        None
    }
}

/// 主线程完成 `attach_layer` 后收敛状态机：
/// - `attach_ok == false`：若仍为同一会话且处于 `Attaching`，回滚为 `LayerAttachment::None`，
///   确保后续 `teardown_session` 不会对未挂载的 layer 派发 `detach_layer`；
/// - `attach_ok == true` 且会话仍有效：转入 `LayerAttachment::Attached(lease)`；
/// - `attach_ok == true` 但会话在挂载期间已被断开/替换：返回 [`CompleteAttachOutcome::MustDetachNow`]，
///   由主线程在持有 `lease` 期间立刻 `detach_layer`。
pub fn complete_attach_on_main_thread<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    lease: LayerLease,
    attached_frame: Frame,
    attach_ok: bool,
) -> CompleteAttachOutcome {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if !attach_ok {
        if let Some(active) = guard.as_mut() {
            if active.id == sid && active.layer_state.is_attaching(&lease) {
                active.layer_state = LayerAttachment::None;
            }
        }
        return CompleteAttachOutcome::Failed;
    }

    if let Some(active) = guard.as_mut() {
        if active.id == sid && !active.disconnecting && active.layer_state.is_attaching(&lease) {
            active.layer_state = LayerAttachment::Attached(lease);
            let updated_frame = active
                .last_stage_frame
                .map(|_| active.effective_layer_frame(attached_frame))
                .filter(|f| *f != attached_frame);
            return CompleteAttachOutcome::Attached { updated_frame };
        }
    }

    CompleteAttachOutcome::MustDetachNow
}

/// 处理 `stage_rect`：更新 `last_stage_frame`、按视频宽高比算出等比居中视口并转发会话命令，
/// 在已确认挂载时返回 `(sid, LayerLease, effective_frame)` 供主线程安全同步 `CALayer` frame。
pub fn prepare_stage_rect<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    frame: Frame,
    send_stage_rect: impl FnOnce(&H, Frame),
) -> Option<(u64, LayerLease, Frame)> {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let active = guard.as_mut()?;
    if active.disconnecting {
        return None;
    }
    active.last_stage_frame = Some(frame);
    let effective = active.effective_layer_frame(frame);
    send_stage_rect(&active.handle, effective);
    active
        .layer_state
        .attached_lease()
        .map(|lease| (active.id, lease, effective))
}

/// 主线程执行 `sync_layer_frame` 前复核：仅当 `sid` 仍为当前未断开会话且 `lease` 仍已挂载时返回 `true`。
pub fn should_sync_layer_on_main_thread<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    lease: &LayerLease,
) -> bool {
    let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .is_some_and(|a| a.id == sid && !a.disconnecting && a.layer_state.is_attached(lease))
}

/// 发起 `SessionCommand::Disconnect`：
/// - 幂等标记 `disconnecting = true`；
/// - 若存在待挂载/已挂载的 `LayerLease`，将其移交调用方在主线程先 `detach_layer`（期间租约保活
///   `MetalRenderer`，即使并发 `open_session` 替换并 drop 旧会话也不会 UAF），再按 `sid` 核对发送
///   `Disconnect`；
/// - 否则在持锁期间直接发送 `Disconnect`。
pub fn begin_disconnect<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    send_disconnect_now: impl FnOnce(&H),
) -> Option<(u64, LayerLease)> {
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    let active = guard.as_mut()?;
    if active.disconnecting {
        return None;
    }
    active.disconnecting = true;
    if let Some(lease) = active.layer_state.take_lease() {
        Some((active.id, lease))
    } else {
        send_disconnect_now(&active.handle);
        None
    }
}

/// 主线程 `detach_layer` 完成后按代际号 `sid` 发送 `Disconnect`：
/// 若并发 `open_session` 已将槽位替换为新会话（`active.id != sid`），返回 `false` 且绝不误断新会话。
pub fn finish_disconnect_on_main_thread<H>(
    slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    send_disconnect: impl FnOnce(&H),
) -> bool {
    let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = guard.as_ref() {
        if active.id == sid {
            send_disconnect(&active.handle);
            return true;
        }
    }
    false
}

/// 在已持有 `session` 锁的前提下清空最近一次会话状态快照（U-H2 / U-H3）。
pub fn clear_session_snapshot_locked(snapshot_slot: &Mutex<Option<(u64, SessionUpdate)>>) {
    *snapshot_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 在已持有 `session` 锁的前提下记录当前活动会话的最新状态快照（U-H2 / U-H3）。
pub fn record_session_update_locked(
    snapshot_slot: &Mutex<Option<(u64, SessionUpdate)>>,
    sid: u64,
    upd: SessionUpdate,
) {
    *snapshot_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((sid, upd));
}

/// 若当前无活动会话，原子清空残留的会话快照（供 `open_session_window` 打开新窗口前调用，U-H2）。
pub fn clear_session_snapshot_if_idle<H>(
    session_slot: &Mutex<Option<ActiveSession<H>>>,
    snapshot_slot: &Mutex<Option<(u64, SessionUpdate)>>,
) {
    let guard = session_slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        clear_session_snapshot_locked(snapshot_slot);
    }
}

/// 读取当前活动会话的状态快照（U-H2 / U-H3）：
/// - 若 `session_slot` 为 `None`，始终返回 `None`（绝不向新窗口返回旧会话的 `Ended`）；
/// - 若存在活动会话，仅当快照代际号与当前 `active.id` 匹配时返回。
pub fn current_session_snapshot<H>(
    session_slot: &Mutex<Option<ActiveSession<H>>>,
    snapshot_slot: &Mutex<Option<(u64, SessionUpdate)>>,
) -> Option<SessionUpdate> {
    let guard = session_slot.lock().unwrap_or_else(|e| e.into_inner());
    let active = guard.as_ref()?;
    let snap = snapshot_slot.lock().unwrap_or_else(|e| e.into_inner());
    snap.as_ref()
        .filter(|(snap_sid, _)| *snap_sid == active.id)
        .map(|(_, upd)| upd.clone())
}

/// 轮询器发射 `"session://update"` 与 W2/W3/W4 工具事件前复核代际号（U-H3 + H11）：
/// - 槽位仍为同一会话（`active.id == sid`）：允许发射；
/// - 槽位已空（`None`）：仅当本批次确实由轮询器自身观察到 `SessionUpdate::Ended`（`saw_ended == true`）
///   并原子摘除时才允许发射该终态批次；若 `saw_ended == false`（说明 `disconnect_active_session`
///   已先清理槽位并发射了终端退出与清空映射事件），必须返回 `false` 防止陈旧 `Output` / 非空
///   `portmap://update` 复活已销毁状态；
/// - 槽位已被更新的会话替换（`active.id != sid`）：恒返回 `false`。
#[cfg_attr(not(test), allow(dead_code))]
pub fn should_emit_session_updates<H>(
    session_slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    saw_ended: bool,
) -> bool {
    let guard = session_slot.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(active) => active.id == sid,
        None => saw_ended,
    }
}

/// 在单次 `session` 锁内校验 `sid` 仍归属当前会话并返回待发射的最新 `ActiveSessionInfo`。
///
/// 避免轮询循环先释放 `session` 锁、后发射预快照导致的 `watch_only` 竞态覆盖。
pub fn resolve_session_update_emit<H>(
    session_slot: &Mutex<Option<ActiveSession<H>>>,
    sid: u64,
    saw_ended: bool,
    emit_active_state: bool,
) -> (bool, Option<ActiveSessionInfo>) {
    let guard = session_slot.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(active) if active.id == sid => (true, emit_active_state.then(|| active.info())),
        Some(_) => (false, None),
        None => (
            saw_ended,
            (saw_ended && emit_active_state).then(ActiveSessionInfo::inactive),
        ),
    }
}

/// 被控端实时状态快照（供前端 P2「允许控制本机」卡片与审批弹窗渲染）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatusSnapshot {
    /// 被控端服务是否已启用并处于监听/服务中。
    pub enabled: bool,
    /// 实际绑定的监听地址（如 `"0.0.0.0:9901"`）。
    pub listen_addr: Option<String>,
    /// 展示给对端的连接地址（`0.0.0.0` 已替换为探测到的局域网 IPv4）。
    pub display_addr: Option<String>,
    /// 当前生效的访问密码（临时验证码或自定义密码；空串表示无密码）。
    pub password: String,
    /// 是否为用户自定义密码（`false` 时为自动生成的临时验证码，断开后自动刷新）。
    pub custom_password: bool,
    /// 当前被控端后端标签（如 `"auto"` / `"sck+videotoolbox"` / `"fake"`）。
    pub backend: String,
    /// 当前已连接的控制端地址（未连接时为 `None`）。
    pub connected_peer: Option<String>,
    /// 当前待机主审批的连接请求（无待审批请求时为 `None`）。
    pub pending_approval: Option<HostApprovalPrompt>,
}

/// 被控端运行时状态（持有唯一活动 [`HostHandle`] 与可观测快照字段）。
pub struct HostRuntime {
    /// 当前活动被控端服务句柄（未启用时为 `None`）。
    pub handle: Option<HostHandle>,
    /// 是否允许非同账号他人的远程协助（为 `false` 时，即使仅本人自控监听运行，UI「允许远程协助」仍显示关闭）。
    pub allow_remote_assist: bool,
    /// 实际绑定的监听地址。
    pub listen_addr: Option<String>,
    /// 展示给对端的局域网地址。
    pub display_addr: Option<String>,
    /// 当前生效的访问密码（空串表示无密码）。
    pub password: String,
    /// 是否为用户自定义密码（`false` 时在控制端断开后自动刷新临时验证码）。
    pub custom_password: bool,
    /// 当前被控端后端标签。
    pub backend: String,
    /// 当前已连接的控制端地址。
    pub connected_peer: Option<String>,
    /// 当前待机主审批的连接请求。
    pub pending_approval: Option<HostApprovalPrompt>,
}

impl HostRuntime {
    /// 构造默认被控端运行时（未启动，预生成 8 位临时验证码）。
    pub fn new() -> Self {
        Self {
            handle: None,
            allow_remote_assist: false,
            listen_addr: None,
            display_addr: None,
            password: host_bridge::generate_temp_code(),
            custom_password: false,
            backend: String::new(),
            connected_peer: None,
            pending_approval: None,
        }
    }

    /// 导出当前被控端状态快照。
    pub fn snapshot(&self) -> HostStatusSnapshot {
        HostStatusSnapshot {
            enabled: self.handle.is_some() && self.allow_remote_assist,
            listen_addr: self.listen_addr.clone(),
            display_addr: self.display_addr.clone(),
            password: self.password.clone(),
            custom_password: self.custom_password,
            backend: self.backend.clone(),
            connected_peer: self.connected_peer.clone(),
            pending_approval: self.pending_approval.clone(),
        }
    }

    /// 将实际监听地址格式化为对端可直连的展示地址（`0.0.0.0` 替换为本机局域网 IPv4）。
    pub fn format_display_addr(actual: SocketAddr) -> String {
        if actual.ip().is_unspecified() {
            let ip = lan_discovery::detect_local_lan_ipv4().unwrap_or(Ipv4Addr::LOCALHOST);
            format!("{ip}:{}", actual.port())
        } else {
            actual.to_string()
        }
    }

    /// 将后台轮询到的 [`HostUpdate`] 折算入运行时状态；返回 `true` 表示收到了 `Stopped`。
    ///
    /// 临时验证码由被控端工作线程在会话结束后原子轮换并通过 [`HostUpdate::PasswordRotated`] 上报（M7）。
    pub fn apply_update(&mut self, upd: &HostUpdate) -> bool {
        match upd {
            HostUpdate::Listening {
                listen_addr,
                display_addr,
                backend,
            } => {
                self.listen_addr = Some(listen_addr.clone());
                self.display_addr = Some(display_addr.clone());
                self.backend = backend.clone();
                self.connected_peer = None;
                self.pending_approval = None;
                false
            }
            HostUpdate::ApprovalRequested(prompt) => {
                self.pending_approval = Some(prompt.clone());
                false
            }
            HostUpdate::ApprovalCancelled { generation } => {
                if self
                    .pending_approval
                    .as_ref()
                    .is_some_and(|p| p.generation == *generation)
                {
                    self.pending_approval = None;
                }
                false
            }
            HostUpdate::ClientConnected { remote_addr } => {
                self.connected_peer = Some(remote_addr.clone());
                self.pending_approval = None;
                false
            }
            HostUpdate::OwnerConnected {
                caller_device_id,
                remote_addr,
            } => {
                self.connected_peer =
                    Some(format!("我的设备 ({caller_device_id}) · {remote_addr}"));
                self.pending_approval = None;
                false
            }
            HostUpdate::ClientDisconnected { .. } => {
                self.connected_peer = None;
                self.pending_approval = None;
                false
            }
            HostUpdate::PasswordRotated { password } => {
                if !self.custom_password {
                    self.password = password.clone();
                    self.custom_password = false;
                }
                false
            }
            HostUpdate::Stopped => {
                self.allow_remote_assist = false;
                self.listen_addr = None;
                self.display_addr = None;
                self.connected_peer = None;
                self.pending_approval = None;
                self.backend.clear();
                true
            }
            HostUpdate::Error { .. } => {
                self.pending_approval = None;
                false
            }
        }
    }
}

impl Default for HostRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// 信令与中继服务器运行时状态（单一锁协调配置、客户端句柄、状态快照、代次与停机标记，H2 + H8）。
pub struct ServerRuntime {
    /// 本地持久化的信令/中继服务器配置。
    pub config: ServerConfig,
    /// 当前活动的信令服务器长连接句柄（以 `Arc` 共享，避免 `request_connect` 阻塞期间持锁）。
    pub signal_client: Option<Arc<SignalClientHandle>>,
    /// 最近一次信令服务器连接状态快照。
    pub last_status: ServerStatusSnapshot,
    /// 配置变更代次（每次保存新配置递增，防止旧代次后台持久化覆盖新配置）。
    pub config_epoch: u64,
    /// 应用停机标记（置位后禁止再派生新的信令客户端线程）。
    pub shutting_down: bool,
}

impl ServerRuntime {
    /// 由初始服务器配置构造运行时状态。
    pub fn new(initial_config: ServerConfig) -> Self {
        let last_status = initial_config.disabled_snapshot();
        Self {
            config: initial_config,
            signal_client: None,
            last_status,
            config_epoch: 0,
            shutting_down: false,
        }
    }

    /// 获取当前最新状态快照（优先读取活动 `SignalClientHandle::status()`）。
    pub fn current_status(&self) -> ServerStatusSnapshot {
        if let Some(client) = self.signal_client.as_ref() {
            return client.status();
        }
        self.last_status.clone()
    }

    /// 仅当快照版本号不低于当前已记录版本号时更新 `last_status`；返回是否接受本次更新。
    pub fn apply_status_snapshot(&mut self, snap: &ServerStatusSnapshot) -> bool {
        if snap.revision < self.last_status.revision {
            return false;
        }
        self.last_status = snap.clone();
        true
    }
}

/// 客户端首次启动双阶强门禁状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppGateState {
    /// 未配置服务器（`!server_config.is_configured()`）。
    Unconfigured,
    /// 已配置服务器但未登录。
    Unauthenticated,
    /// 已登录但必须修改初始密码（`must_change_password == true`）。
    PasswordChangeRequired,
    /// 已登录且已就绪。
    Authenticated,
}

/// 门禁快照中的当前登录用户信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateUserInfo {
    /// 用户 UUID。
    pub id: String,
    /// 登录邮箱。
    pub email: String,
    /// 显示昵称。
    pub display_name: String,
    /// 用户角色（`"admin"` / `"user"`）。
    pub role: String,
    /// 是否必须先修改密码。
    pub must_change_password: bool,
}

/// 门禁快照中的本机设备 mTLS 证书状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateCertStatus {
    /// 本机设备证书是否已签发且本地文件完整可用。
    pub issued: bool,
    /// 证书序列号（小写十六进制）。
    pub serial_hex: Option<String>,
    /// 证书过期时间戳（Unix 秒）。
    pub expires_at_unix_sec: Option<u64>,
}

/// 面向前端 IPC 的首启双阶门禁与认证状态快照（`get_gate_status` / `gate://status`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStatusSnapshot {
    /// 当前门禁阶段。
    pub gate: AppGateState,
    /// 服务器是否已配置并启用。
    pub server_configured: bool,
    /// 服务器主机名或 IP。
    pub server_host: String,
    /// HTTP/HTTPS API 端口。
    pub api_port: u16,
    /// 信令端口。
    pub signal_port: u16,
    /// 中继端口。
    pub relay_port: u16,
    /// API 通信是否启用 HTTPS。
    pub use_https: bool,
    /// 锚定的 Root CA SHA-256 指纹。
    pub ca_fingerprint_sha256: Option<String>,
    /// 服务端是否开放自助注册。
    pub allow_registration: bool,
    /// 当前登录用户信息（未登录时为 `None`）。
    pub user: Option<GateUserInfo>,
    /// 本机设备证书状态。
    pub cert_status: Option<GateCertStatus>,
    /// 本机 9 位设备码。
    pub device_id: Option<String>,
    /// 本机格式化 9 位设备码（`"604 928 235"`）。
    pub device_id_formatted: Option<String>,
}

/// Tauri 全局托管应用状态。
pub struct AppState {
    /// 当前唯一活动会话。
    pub session: Mutex<Option<ActiveSession>>,
    /// 最近一次会话状态更新快照，附带会话代际号 `sid`（Review M8 + U-H2 / U-H3：
    /// 锁序恒为 `session -> last_session_update`，防旧会话快照串台或泄露过期 `Ended`）。
    pub last_session_update: Mutex<Option<(u64, SessionUpdate)>>,
    /// 会话打开代次计数器（H9：`open_session` 与 `disconnect_active_session` 递增，
    /// 使在途 `request_connect` 在被更新请求或关窗取消后自动作废）。
    pub session_open_epoch: AtomicU64,
    /// 本机被控端运行时状态（一体化 All-in-One 模式）。
    pub host: Mutex<HostRuntime>,
    /// 局域网多播发现后台线程句柄。
    pub lan: Mutex<Option<LanDiscoveryHandle>>,
    /// 信令与中继服务器运行时状态（单一互斥锁协调配置/客户端/快照/代次/停机标记）。
    pub server: Mutex<ServerRuntime>,
    /// 串行化 `server.json` 磁盘写入的专用锁（锁序恒为 `server_io_lock -> server`，不持 `server` 锁做 I/O）。
    pub server_io_lock: Mutex<()>,
    /// 应用级偏好设置（远程协助持久化开关、画质/帧率/分辨率记忆、设备壁纸快照）。
    pub prefs: Mutex<AppPrefs>,
    /// 串行化 `prefs.json` 磁盘写入的专用锁（锁序恒为 `prefs_io_lock -> prefs`）。
    pub prefs_io_lock: Mutex<()>,
    /// 认证会话、Refresh Token 自动轮换与设备证书生命周期管理器。
    pub auth: Arc<AuthSessionManager>,
    /// 后台令牌轮换与证书续签维护线程句柄。
    pub auth_maintainer: Mutex<Option<AuthMaintainerHandle>>,
    /// 服务端是否开放自助注册（默认 `true`，由探测与拉取认证配置更新）。
    pub allow_registration: Mutex<bool>,
    /// 单调递增会话代际号分配器（从 1 开始）。
    next_session_id: AtomicU64,
    /// §6.0 mock 设备表（侧栏 3 台）。
    devices: Vec<DeviceEntry>,
    /// 未收录 DOM `KeyboardEvent.code` 的静默丢弃计数（T21 §6）。
    dropped_unmapped_keys: AtomicU64,
    /// 测试隔离临时目录（Drop 时清理）。
    test_temp_dir: Option<PathBuf>,
}

impl Drop for AppState {
    fn drop(&mut self) {
        if let Some(mut handle) = self
            .auth_maintainer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.stop();
        }
        if let Some(ref dir) = self.test_temp_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl AppState {
    /// 构造默认应用状态（空会话 + 预生成临时验证码的被控端运行时 + 加载本地服务器配置 + 应用偏好 + 认证与证书管理器）。
    pub fn new() -> Self {
        let initial_server_cfg =
            server_config::load_from_path(&server_config::default_config_path());
        let initial_prefs = app_prefs::load_from_path(&app_prefs::default_prefs_path());
        Self::with_initial_configs_and_paths(
            initial_server_cfg,
            initial_prefs,
            auth_client::default_auth_path(),
            cert_manager::default_certs_dir(),
            None,
        )
    }

    /// 以显式传入的初始配置与偏好构造 `AppState`（供生产入口与确定性单测复用）。
    #[allow(dead_code)]
    pub fn with_initial_configs(initial_server_cfg: ServerConfig, initial_prefs: AppPrefs) -> Self {
        Self::with_initial_configs_and_paths(
            initial_server_cfg,
            initial_prefs,
            auth_client::default_auth_path(),
            cert_manager::default_certs_dir(),
            None,
        )
    }

    /// 以显式传入的初始配置、偏好、认证凭据路径及证书目录构造 `AppState`。
    pub fn with_initial_configs_and_paths(
        initial_server_cfg: ServerConfig,
        initial_prefs: AppPrefs,
        auth_path: PathBuf,
        certs_dir: PathBuf,
        test_temp_dir: Option<PathBuf>,
    ) -> Self {
        let mut host_rt = HostRuntime::new();
        if let Some(ref pw) = initial_prefs.custom_host_password {
            host_rt.password = pw.clone();
            host_rt.custom_password = true;
        }
        let cert_mgr = Arc::new(CertManager::load_or_init(certs_dir).unwrap_or_else(|err| {
            eprintln!("[rc-ui] 初始化设备证书目录失败，将回退临时目录：{err}");
            let fallback =
                std::env::temp_dir().join(format!("rc-certs-fallback-{}", std::process::id()));
            CertManager::load_or_init(fallback).expect("init fallback CertManager")
        }));
        let auth = Arc::new(AuthSessionManager::new(
            initial_server_cfg.clone(),
            auth_path,
            cert_mgr,
        ));
        Self {
            session: Mutex::new(None),
            last_session_update: Mutex::new(None),
            session_open_epoch: AtomicU64::new(0),
            host: Mutex::new(host_rt),
            lan: Mutex::new(None),
            server: Mutex::new(ServerRuntime::new(initial_server_cfg)),
            server_io_lock: Mutex::new(()),
            prefs: Mutex::new(initial_prefs),
            prefs_io_lock: Mutex::new(()),
            auth,
            auth_maintainer: Mutex::new(None),
            allow_registration: Mutex::new(true),
            next_session_id: AtomicU64::new(1),
            devices: default_mock_devices(),
            dropped_unmapped_keys: AtomicU64::new(0),
            test_temp_dir,
        }
    }

    /// 供单元测试使用的隔离构造器（不读取 `$HOME/.config/rabbitcontrol/` 中的用户配置）。
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        use rand::rngs::OsRng;
        use rand::RngCore;
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let temp_dir = std::env::temp_dir().join(format!("rc-ui-appstate-test-{hex}"));
        let auth_path = temp_dir.join("auth.json");
        let certs_dir = temp_dir.join("certs");
        Self::with_initial_configs_and_paths(
            ServerConfig::default(),
            AppPrefs::default(),
            auth_path,
            certs_dir,
            Some(temp_dir),
        )
    }

    /// 计算当前门禁阶段。
    pub fn gate_state(&self) -> AppGateState {
        let configured = self
            .server
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .is_configured();
        if !configured {
            return AppGateState::Unconfigured;
        }
        match self.auth.current_session() {
            None => AppGateState::Unauthenticated,
            Some(sess) if sess.must_change_password => AppGateState::PasswordChangeRequired,
            Some(_) => AppGateState::Authenticated,
        }
    }

    /// 严格校验当前门禁必须处于 [`AppGateState::Authenticated`]，否则返回明确中文拒绝提示。
    pub fn require_authenticated(&self) -> Result<(), String> {
        match self.gate_state() {
            AppGateState::Authenticated => Ok(()),
            AppGateState::Unconfigured => Err("请先配置并连接 RabbitControl 服务器".to_string()),
            AppGateState::Unauthenticated => Err("请先登录 RabbitControl 账号".to_string()),
            AppGateState::PasswordChangeRequired => {
                Err("首次登录必须修改密码后才能继续使用".to_string())
            }
        }
    }

    /// 导出当前本机设备证书状态快照。
    pub fn cert_status(&self) -> GateCertStatus {
        match self.auth.cert_manager().load_device_cert_metadata() {
            Some(meta) => GateCertStatus {
                issued: true,
                serial_hex: Some(meta.serial_hex),
                expires_at_unix_sec: Some(meta.expires_at_unix_sec),
            },
            None => GateCertStatus {
                issued: false,
                serial_hex: None,
                expires_at_unix_sec: None,
            },
        }
    }

    /// 导出前端展示所需的首启双阶门禁状态快照。
    pub fn gate_status_snapshot(&self) -> GateStatusSnapshot {
        let (cfg, srv_snap) = {
            let rt = self.server.lock().unwrap_or_else(|e| e.into_inner());
            (rt.config.clone(), rt.current_status())
        };
        let gate = if !cfg.is_configured() {
            AppGateState::Unconfigured
        } else {
            match self.auth.current_session() {
                None => AppGateState::Unauthenticated,
                Some(sess) if sess.must_change_password => AppGateState::PasswordChangeRequired,
                Some(_) => AppGateState::Authenticated,
            }
        };
        let user = self.auth.current_session().map(|sess| GateUserInfo {
            id: sess.user_id,
            email: sess.email,
            display_name: sess.display_name,
            role: sess.role,
            must_change_password: sess.must_change_password,
        });
        let allow_registration = *self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let device_id = srv_snap.device_id.or(cfg.cached_device_id.clone());
        let device_id_formatted = device_id.as_deref().map(server_config::format_device_id);
        GateStatusSnapshot {
            gate,
            server_configured: cfg.is_configured(),
            server_host: cfg.server_host,
            api_port: cfg.api_port,
            signal_port: cfg.signal_port,
            relay_port: cfg.relay_port,
            use_https: cfg.use_https,
            ca_fingerprint_sha256: cfg.ca_fingerprint_sha256,
            allow_registration,
            user,
            cert_status: Some(self.cert_status()),
            device_id,
            device_id_formatted,
        }
    }

    /// 获取当前活动会话状态只读快照（未连接或已发起断开时返回 `active: false`）。
    pub fn active_session_info(&self) -> ActiveSessionInfo {
        let slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref()
            .map(ActiveSession::info)
            .unwrap_or_else(ActiveSessionInfo::inactive)
    }

    /// 分配下一个唯一的会话代际号。
    pub fn next_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 返回 §6.0 mock 设备列表副本。
    pub fn devices(&self) -> Vec<DeviceEntry> {
        self.devices.clone()
    }

    /// 记录一次未收录键码静默丢弃并返回累计值。
    pub fn record_dropped_key(&self) -> u64 {
        self.dropped_unmapped_keys.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 读取累计丢弃的未收录键码次数。
    #[cfg(test)]
    pub fn dropped_keys_count(&self) -> u64 {
        self.dropped_unmapped_keys.load(Ordering::Relaxed)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// `08-ui-design.md` §6.0 侧栏 3 台 mock 设备（逐字对齐 `mock.js`）。
fn default_mock_devices() -> Vec<DeviceEntry> {
    vec![
        DeviceEntry {
            id: "desktop-q5q8e8r",
            name: "DESKTOP-Q5Q8E8R",
            kind: "windows",
            icon: "app-window",
            online: true,
            is_local: false,
        },
        DeviceEntry {
            id: "yii-mbp",
            name: "yii's MacBook Pro",
            kind: "mac",
            icon: "laptop",
            online: true,
            is_local: false,
        },
        DeviceEntry {
            id: "yii-mac-mini",
            name: "Y 的 Mac mini",
            kind: "mac",
            icon: "laptop",
            online: true,
            is_local: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use rc_render::Renderer;
    use rc_ui::session_bridge::SessionCommand;

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

    #[derive(Default)]
    struct FakeSessionHandle {
        commands: Mutex<Vec<SessionCommand>>,
    }

    impl FakeSessionHandle {
        fn send(&self, cmd: SessionCommand) {
            self.commands.lock().unwrap().push(cmd);
        }

        fn commands(&self) -> Vec<SessionCommand> {
            self.commands.lock().unwrap().clone()
        }
    }

    fn make_lease(addr: usize) -> (LayerLease, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let renderer: Arc<Mutex<Box<dyn Renderer>>> =
            Arc::new(Mutex::new(Box::new(DropSpyRenderer {
                dropped: Arc::clone(&dropped),
            })));
        let lease = LayerLease::from_renderer(renderer, addr).expect("non-zero addr");
        (lease, dropped)
    }

    fn sample_frame(w: f64, h: f64) -> Frame {
        Frame {
            x: 0.0,
            y: 0.0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn default_devices_match_spec_6_0() {
        let state = AppState::new_for_test();
        let devices = state.devices();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].name, "DESKTOP-Q5Q8E8R");
        assert_eq!(devices[1].name, "yii's MacBook Pro");
        assert_eq!(devices[2].name, "Y 的 Mac mini");
        assert!(devices.iter().all(|d| d.online && !d.is_local));
    }

    #[test]
    fn dropped_key_counter_increments() {
        let state = AppState::new_for_test();
        assert_eq!(state.dropped_keys_count(), 0);
        assert_eq!(state.record_dropped_key(), 1);
        assert_eq!(state.record_dropped_key(), 2);
        assert_eq!(state.dropped_keys_count(), 2);
    }

    #[test]
    fn with_initial_configs_restores_saved_custom_host_password() {
        let state = AppState::with_initial_configs(
            ServerConfig::default(),
            AppPrefs {
                custom_host_password: Some("SAVED42".to_string()),
                ..Default::default()
            },
        );
        let snap = state.host.lock().unwrap().snapshot();
        assert!(snap.custom_password);
        assert_eq!(snap.password, "SAVED42");
    }

    /// 回归测试（Issue 1a）：主线程 `attach_layer` 失败（如窗口不存在 / `ns_view()` 失败 /
    /// `attach_layer` 返回 `Err`）后，状态必须回滚为 `LayerAttachment::None`，
    /// 后续 teardown 不得对未挂载的 layer 派发 `detach_layer`。
    #[test]
    fn failed_attach_resets_layer_state_so_teardown_skips_detach() {
        let slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(Some(
            ActiveSession::new(1, FakeSessionHandle::default(), false),
        ));
        let (lease, dropped) = make_lease(0x1001);
        let fallback = sample_frame(1920.0, 1080.0);

        let (sid, attach_lease, frame) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease, fallback)
            .expect("begin_attach should succeed");
        assert_eq!(sid, 1);

        let effective = should_attach_on_main_thread(&slot, sid, &attach_lease, frame)
            .expect("active session should proceed to attach");
        assert_eq!(effective, fallback);

        let outcome = complete_attach_on_main_thread(&slot, sid, attach_lease, effective, false);
        assert_eq!(outcome, CompleteAttachOutcome::Failed);

        // teardown 取出 old session：layer_state 应为空，不需要派发 detach_layer。
        let mut old = slot.lock().unwrap().take().unwrap();
        assert!(old.layer_state.take_lease().is_none());
        drop(old);
        assert!(dropped.load(Ordering::SeqCst));
    }

    /// 回归测试（Issue 1c）：主线程 `attach_layer` 执行间隙若会话被并发替换或关闭，
    /// `complete_attach_on_main_thread` 必须返回 `MustDetachNow`，同时租约保活渲染器直到摘除结束。
    #[test]
    fn concurrent_replacement_during_attach_requests_immediate_detach_with_live_lease() {
        let slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(Some(
            ActiveSession::new(1, FakeSessionHandle::default(), false),
        ));
        let (lease, dropped) = make_lease(0x1002);
        let fallback = sample_frame(1280.0, 720.0);

        let (sid, attach_lease, frame) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease, fallback)
            .unwrap();
        let effective = should_attach_on_main_thread(&slot, sid, &attach_lease, frame).unwrap();

        // 模拟主线程正在执行 attach_layer 时，并发 open_session 替换并 drop 了旧会话。
        let old = slot
            .lock()
            .unwrap()
            .replace(ActiveSession::new(2, FakeSessionHandle::default(), false))
            .unwrap();
        drop(old);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "主线程闭包持有 attach_lease 期间底层渲染器必须保持存活"
        );

        let outcome =
            complete_attach_on_main_thread(&slot, sid, attach_lease.clone(), effective, true);
        assert_eq!(outcome, CompleteAttachOutcome::MustDetachNow);
        assert!(
            !dropped.load(Ordering::SeqCst),
            "立刻执行 detach_layer 前底层渲染器仍须存活"
        );

        // 主线程立刻 detach_layer 完成后释放租约。
        drop(attach_lease);
        assert!(dropped.load(Ordering::SeqCst));
    }

    /// 回归测试（Issue 2）：`stage_rect` 与 `attach_layer_on_main_thread` 排队后若并发发生
    /// `teardown_session` 并释放旧 `ActiveSession`，排队闭包仍通过 `LayerLease` 保活底层渲染器，
    /// 且在主线程复核 `sid` 失败后跳过 `sync_layer_frame` / `attach_layer`。
    #[test]
    fn queued_main_thread_ops_keep_layer_alive_and_skip_after_teardown() {
        // 场景 A：已挂载后排队 stage_rect，随后 teardown。
        let slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(Some(
            ActiveSession::new(1, FakeSessionHandle::default(), false),
        ));
        let (lease_a, dropped_a) = make_lease(0x2001);
        let frame = sample_frame(1920.0, 1080.0);
        let (sid_a, attach_lease_a, _) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease_a, frame)
            .unwrap();
        assert_eq!(
            complete_attach_on_main_thread(&slot, sid_a, attach_lease_a, frame, true),
            CompleteAttachOutcome::Attached {
                updated_frame: None
            }
        );

        let (sync_sid, sync_lease, _) =
            prepare_stage_rect(&slot, sample_frame(800.0, 600.0), |_, _| {})
                .expect("attached session should schedule sync");

        // 并发 teardown 移出并 drop 旧会话。
        let old = slot.lock().unwrap().take().unwrap();
        drop(old);
        assert!(
            !dropped_a.load(Ordering::SeqCst),
            "排队中的 sync_layer_frame 闭包持有 LayerLease，渲染器不得提前释放"
        );
        assert!(
            !should_sync_layer_on_main_thread(&slot, sync_sid, &sync_lease),
            "旧会话已摘除，主线程必须跳过 sync_layer_frame"
        );
        drop(sync_lease);
        assert!(dropped_a.load(Ordering::SeqCst));

        // 场景 B：begin_attach 排队后尚未执行即发生 teardown。
        let (lease_b, dropped_b) = make_lease(0x2002);
        *slot.lock().unwrap() = Some(ActiveSession::new(2, FakeSessionHandle::default(), false));
        let (sid_b, queued_attach_lease, queued_frame) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease_b, frame)
            .unwrap();
        let old_b = slot.lock().unwrap().take().unwrap();
        drop(old_b);
        assert!(
            !dropped_b.load(Ordering::SeqCst),
            "排队中的 attach_layer 闭包持有 LayerLease，渲染器不得提前释放"
        );
        assert!(
            should_attach_on_main_thread(&slot, sid_b, &queued_attach_lease, queued_frame)
                .is_none(),
            "旧会话已摘除，主线程必须跳过 attach_layer"
        );
        drop(queued_attach_lease);
        assert!(dropped_b.load(Ordering::SeqCst));
    }

    /// 回归测试（Issue 3）：`session_command(Disconnect)` 在主线程 `detach_layer` 排队期间
    /// 遭遇并发 `open_session`：
    /// 1) 旧会话的 `MetalRenderer` 在 `detach_layer` 执行前必须保持存活（无 UAF）；
    /// 2) 主线程闭包随后核对 `sid` 发现已切换为新会话，绝不向新会话误发 `Disconnect`。
    #[test]
    fn disconnect_race_with_open_session_prevents_uaf_and_wrong_session_disconnect() {
        let slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(Some(
            ActiveSession::new(1, FakeSessionHandle::default(), false),
        ));
        let (lease_1, dropped_1) = make_lease(0x3001);
        let frame = sample_frame(1920.0, 1080.0);

        let (sid_1, attach_lease_1, _) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease_1, frame)
            .unwrap();
        assert_eq!(
            complete_attach_on_main_thread(&slot, sid_1, attach_lease_1, frame, true),
            CompleteAttachOutcome::Attached {
                updated_frame: None
            }
        );

        // 1) UI 触发 Disconnect：取出租约并排队主线程 detach。
        let (disc_sid, disc_lease) =
            begin_disconnect(&slot, |h| h.send(SessionCommand::Disconnect))
                .expect("attached session should defer disconnect until main-thread detach");
        assert_eq!(disc_sid, 1);

        // 2) 在主线程 detach 闭包执行之前，并发 open_session 替换槽位并 drop 旧会话。
        let old_session = slot
            .lock()
            .unwrap()
            .replace(ActiveSession::new(2, FakeSessionHandle::default(), false))
            .unwrap();
        assert!(old_session.layer_state.is_none());
        drop(old_session);

        // 关键断言 1：旧会话被 drop 后，由于主线程闭包仍持有 disc_lease，旧渲染器必须存活（无 UAF）。
        assert!(
            !dropped_1.load(Ordering::SeqCst),
            "detach_layer 执行前旧渲染器必须由 disc_lease 保活"
        );

        // 3) 主线程闭包执行 detach_layer 后调用 finish_disconnect_on_main_thread(sid = 1)。
        let sent = finish_disconnect_on_main_thread(&slot, disc_sid, |h| {
            h.send(SessionCommand::Disconnect)
        });
        assert!(
            !sent,
            "槽位已切换为新会话（id=2），旧 Disconnect 闭包不得向新会话发 Disconnect"
        );

        // 关键断言 2：新会话（id=2）未收到任何 Disconnect 命令。
        let guard = slot.lock().unwrap();
        let new_session = guard.as_ref().unwrap();
        assert_eq!(new_session.id, 2);
        assert!(new_session.handle.commands().is_empty());
        drop(guard);

        // 4) 主线程闭包释放 disc_lease，旧渲染器干净析构。
        drop(disc_lease);
        assert!(dropped_1.load(Ordering::SeqCst));
    }

    /// 正常路径（无并发替换）：`Disconnect` 在主线程完成 `detach_layer` 后正确发给原会话。
    #[test]
    fn disconnect_without_replacement_sends_to_matching_session() {
        let slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(Some(
            ActiveSession::new(7, FakeSessionHandle::default(), false),
        ));
        let (lease, dropped) = make_lease(0x3002);
        let frame = sample_frame(1920.0, 1080.0);

        let (sid, attach_lease, _) = slot
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .begin_attach(lease, frame)
            .unwrap();
        assert_eq!(
            complete_attach_on_main_thread(&slot, sid, attach_lease, frame, true),
            CompleteAttachOutcome::Attached {
                updated_frame: None
            }
        );

        let (disc_sid, disc_lease) =
            begin_disconnect(&slot, |h| h.send(SessionCommand::Disconnect)).unwrap();
        assert_eq!(disc_sid, 7);

        // 重复调用 Disconnect 必须幂等忽略。
        assert!(begin_disconnect(&slot, |h| h.send(SessionCommand::Disconnect)).is_none());

        let sent = finish_disconnect_on_main_thread(&slot, disc_sid, |h| {
            h.send(SessionCommand::Disconnect)
        });
        assert!(sent);
        assert_eq!(
            slot.lock().unwrap().as_ref().unwrap().handle.commands(),
            vec![SessionCommand::Disconnect]
        );

        drop(disc_lease);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn host_runtime_tracks_updates_and_rotates_temp_code_on_disconnect() {
        let mut rt = HostRuntime::new();
        rt.allow_remote_assist = true;
        assert!(!rt.custom_password);
        assert_eq!(rt.password.len(), host_bridge::TEMP_CODE_LEN);
        let initial_code = rt.password.clone();

        assert!(!rt.apply_update(&HostUpdate::Listening {
            listen_addr: "0.0.0.0:9901".to_string(),
            display_addr: "192.168.1.10:9901".to_string(),
            backend: "auto".to_string(),
        }));
        let snap = rt.snapshot();
        assert_eq!(snap.listen_addr.as_deref(), Some("0.0.0.0:9901"));
        assert_eq!(snap.display_addr.as_deref(), Some("192.168.1.10:9901"));
        assert_eq!(snap.backend, "auto");

        let prompt = HostApprovalPrompt {
            generation: 3,
            session_id: 99,
            remote_addr: "192.168.1.20:51234".to_string(),
            requested_res: "1920x1080".to_string(),
            requested_fps: 60,
            password_verified: true,
        };
        assert!(!rt.apply_update(&HostUpdate::ApprovalRequested(prompt.clone())));
        assert_eq!(rt.pending_approval, Some(prompt.clone()));

        // 不匹配的代次取消不应清除当前待审批请求。
        assert!(!rt.apply_update(&HostUpdate::ApprovalCancelled { generation: 2 }));
        assert_eq!(rt.pending_approval, Some(prompt));

        // 匹配代次取消应清除。
        assert!(!rt.apply_update(&HostUpdate::ApprovalCancelled { generation: 3 }));
        assert!(rt.pending_approval.is_none());

        // 连接建立后记录 peer，工作线程推送 PasswordRotated 更新临时验证码；ClientDisconnected 不再在轮询侧重复轮换。
        assert!(!rt.apply_update(&HostUpdate::ClientConnected {
            remote_addr: "192.168.1.20:51234".to_string(),
        }));
        assert_eq!(rt.connected_peer.as_deref(), Some("192.168.1.20:51234"));

        let rotated_code = "ABCD2345".to_string();
        assert_ne!(rotated_code, initial_code);
        assert!(!rt.apply_update(&HostUpdate::PasswordRotated {
            password: rotated_code.clone(),
        }));
        assert_eq!(rt.password, rotated_code);
        assert!(!rt.custom_password);

        assert!(!rt.apply_update(&HostUpdate::ClientDisconnected {
            reason: "控制端已断开".to_string(),
        }));
        assert!(rt.connected_peer.is_none());
        assert_eq!(rt.password, rotated_code);

        // 设置自定义密码后，断开或迟到的 PasswordRotated 均不得覆盖自定义密码。
        rt.password = "MyCustomPass!9".to_string();
        rt.custom_password = true;
        assert!(!rt.apply_update(&HostUpdate::PasswordRotated {
            password: "STALE999".to_string(),
        }));
        assert!(!rt.apply_update(&HostUpdate::ClientDisconnected {
            reason: "控制端已断开".to_string(),
        }));
        assert_eq!(rt.password, "MyCustomPass!9");
        assert!(rt.custom_password);

        // Stopped 应清空监听与连接字段并返回 true。
        assert!(rt.apply_update(&HostUpdate::Stopped));
        assert!(rt.listen_addr.is_none());
        assert!(rt.display_addr.is_none());
        assert!(rt.backend.is_empty());
    }

    #[test]
    fn server_runtime_rejects_stale_status_revisions() {
        let cfg = ServerConfig::default();
        let mut rt = ServerRuntime::new(cfg.clone());
        let initial_rev = rt.last_status.revision;
        assert!(initial_rev > 0);

        let mut stale = cfg.disabled_snapshot();
        stale.revision = initial_rev.saturating_sub(1);
        stale.server_host = "stale.example.com".to_string();
        assert!(!rt.apply_status_snapshot(&stale));
        assert_eq!(rt.last_status.revision, initial_rev);
        assert_ne!(rt.last_status.server_host, "stale.example.com");

        let mut fresh = cfg.disabled_snapshot();
        fresh.server_host = "fresh.example.com".to_string();
        assert!(fresh.revision > initial_rev);
        assert!(rt.apply_status_snapshot(&fresh));
        assert_eq!(rt.last_status.server_host, "fresh.example.com");
    }

    /// 回归测试（U-H2 / U-H3）：
    /// 1) 无活动会话（`session == None`）时，`current_session_snapshot` 绝不返回残留的 `Ended`；
    /// 2) 活动会话切为新代际号 `sid = 2` 后，旧代际号 `sid = 1` 的快照不被返回，且
    ///    `should_emit_session_updates(..., 1)` 返回 `false` 阻止旧事件污染新会话；
    /// 3) `clear_session_snapshot_if_idle` 仅在无活动会话时清空快照，不影响活动会话。
    #[test]
    fn session_snapshot_guards_stale_ended_and_cross_generation_updates() {
        let session_slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(None);
        let snap_slot: Mutex<Option<(u64, SessionUpdate)>> = Mutex::new(None);

        // 即使快照槽残留旧会话的 Ended，无活动会话时也必须返回 None（U-H2）。
        record_session_update_locked(
            &snap_slot,
            1,
            SessionUpdate::Ended {
                reason: "旧会话已结束".to_string(),
            },
        );
        assert_eq!(current_session_snapshot(&session_slot, &snap_slot), None);

        // open_session_window 在无活动会话时调用 clear_session_snapshot_if_idle 应彻底清空。
        clear_session_snapshot_if_idle(&session_slot, &snap_slot);
        assert!(snap_slot.lock().unwrap().is_none());

        // 启动新会话 sid = 2，但假设快照里仍存在 sid = 1 的更新：代际号不匹配必须过滤。
        *session_slot.lock().unwrap() =
            Some(ActiveSession::new(2, FakeSessionHandle::default(), false));
        record_session_update_locked(
            &snap_slot,
            1,
            SessionUpdate::Ended {
                reason: "过期事件".to_string(),
            },
        );
        assert_eq!(current_session_snapshot(&session_slot, &snap_slot), None);
        assert!(
            !should_emit_session_updates(&session_slot, 1, false),
            "旧会话 sid=1 不得向新会话 sid=2 发射事件"
        );
        assert!(
            !should_emit_session_updates(&session_slot, 1, true),
            "旧会话 sid=1 即便携带 Ended 也不得污染新会话 sid=2"
        );
        assert!(
            should_emit_session_updates(&session_slot, 2, false),
            "当前活动会话 sid=2 应允许发射事件"
        );

        // 记录 sid = 2 的更新后，current_session_snapshot 正常返回，且 clear_if_idle 不误清。
        let connecting = SessionUpdate::Connecting {
            phase: rc_ui::session_bridge::ConnectPhase::Connecting,
            watch_only: true,
        };
        record_session_update_locked(&snap_slot, 2, connecting.clone());
        assert_eq!(
            current_session_snapshot(&session_slot, &snap_slot),
            Some(connecting.clone())
        );
        clear_session_snapshot_if_idle(&session_slot, &snap_slot);
        assert_eq!(
            current_session_snapshot(&session_slot, &snap_slot),
            Some(connecting)
        );
    }

    /// 回归测试（H11）：当 `disconnect_active_session` 已清空会话槽（`slot == None`）时，
    /// 后台轮询器在 `saw_ended == false` 的陈旧批次必须被阻止发射（避免复活死终端或旧端口映射），
    /// 而轮询器自身观察到 `SessionUpdate::Ended` 并摘除会话的终态批次（`saw_ended == true`）允许发射。
    #[test]
    fn should_emit_session_updates_blocks_stale_non_ended_when_slot_cleared() {
        let session_slot: Mutex<Option<ActiveSession<FakeSessionHandle>>> = Mutex::new(None);
        assert!(
            !should_emit_session_updates(&session_slot, 42, false),
            "槽位已清空且 saw_ended=false 时不得发射陈旧非终态事件"
        );
        assert!(
            should_emit_session_updates(&session_slot, 42, true),
            "槽位已清空且 saw_ended=true 时应允许发射本批次终态事件"
        );
    }
}
