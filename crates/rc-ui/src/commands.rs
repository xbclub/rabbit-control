//! Tauri IPC 命令集合（T21 API 契约 §6）。
//!
//! 全模块 `#![deny(unsafe_code)]`：
//! - 所有纯逻辑校验委托 [`rc_ui::model`]；
//! - 所有输入映射委托 [`rc_ui::input_bridge`]；
//! - Retina 物理像素换算在 Rust 侧统一完成（T21 坑 3）；
//! - AppKit / CoreAnimation 层操作统一经 `app.run_on_main_thread` 派发到主线程（T21 坑 2）。

#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use rc_input::{InputEvent, MouseButton};
use rc_session::auth::{OwnerTicketSigner, OwnerTicketVerifier};
use rc_tools::{FileEntry, PortMapRuleSnapshot, TerminalEvent, TransferItem};
use rc_ui::app_prefs::{
    self, AppPrefs, DeviceSnapshotView, SavedPortMapRule, SecurityPrefsPatch, StreamPrefsPatch,
};
use rc_ui::auth_client::{
    AuthClientError, AuthHttpClient, AuthSessionManager, BindDeviceRequest, MyDeviceInfo,
};
use rc_ui::cert_manager::{OwnerGrantPayload, RcOwnerTicketVerifier};
use rc_ui::host_bridge::{self, HostHandle, HostUpdate, SessionMtlsBundle, UiHostConfig};
use rc_ui::input_bridge;
use rc_ui::lan_discovery::{self, LanPeer};
use rc_ui::metal_attach::{self, Frame};
use rc_ui::model::{self, ConnectTarget};
use rc_ui::permissions::{self, PermissionSnapshot};
use rc_ui::server_config::{self, ServerConfig, ServerConnectionState, ServerStatusSnapshot};
use rc_ui::session_bridge::{
    SessionCommand, SessionHandle, SessionUpdate, ToolClientHandle, UiSessionConfig,
};
use rc_ui::signal_client::{self, SignalClientHandle, SignalClientOptions};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::state::{
    self, ActiveSession, ActiveSessionInfo, ActiveSessionMeta, AppGateState, AppState, DeviceEntry,
    GateCertStatus, GateStatusSnapshot, HostRuntime, HostStatusSnapshot,
};
use crate::window;

/// `open_session` 默认请求宽度（像素，实际以 Welcome 握手裁定为准）。
const DEFAULT_SESSION_WIDTH: u32 = 1920;
/// `open_session` 默认请求高度（像素，实际以 Welcome 握手裁定为准）。
const DEFAULT_SESSION_HEIGHT: u32 = 1080;
/// `open_session` 默认优先硬件后端。
fn default_prefer_hardware() -> bool {
    true
}
/// `open_session` 默认开启剪贴板与音频。
fn default_true() -> bool {
    true
}

/// 前端发起远控会话的请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSessionRequest {
    /// 对端地址（M2 直连格式 `ip:port`，如 `"127.0.0.1:9901"`，或 9 位设备码）。
    pub target: String,
    /// 可选访问密码（对端被控端启用密码鉴权时传入）。
    #[serde(default)]
    pub password: Option<String>,
    /// 观看模式（`true` 时只读不注入输入，T21 U9）。
    #[serde(default)]
    pub watch_only: bool,
    /// 请求宽度（像素；未传时使用默认占位，实际以 Welcome 裁定为准）。
    #[serde(default)]
    pub width: Option<u32>,
    /// 请求高度（像素；未传时使用默认占位，实际以 Welcome 裁定为准）。
    #[serde(default)]
    pub height: Option<u32>,
    /// 请求帧率（fps；未传时从持久化的 `AppPrefs.fps` 读取）。
    #[serde(default)]
    pub fps: Option<u32>,
    /// 请求码率（bps；未传时从持久化的 `AppPrefs.bitrate_bps` 读取）。
    #[serde(default)]
    pub bitrate_bps: Option<u32>,
    /// 优先硬件后端（VT 解码 + Metal 呈现）。
    #[serde(default = "default_prefer_hardware")]
    pub prefer_hardware: bool,
    /// 是否启用双向剪贴板同步（默认 `true`）。
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    /// 是否启用远端音频流（默认 `true`）。
    #[serde(default = "default_true")]
    pub audio_enabled: bool,
    /// 对端设备名称（用于 S1 窗标题、主窗控制状态与壁纸快照归档）。
    #[serde(default)]
    pub device_name: Option<String>,
    /// 是否强制通过 TCP 中继连接（忽略 P2P 直连）。
    #[serde(default)]
    pub force_relay: bool,
    /// 本次会话是否开启端到端 mTLS 加密握手（未传时读取 `AppPrefs.default_encrypt_connection`）。
    #[serde(default)]
    pub encrypt_connection: Option<bool>,
}

/// WebView DOM 输入事件负载（经 [`input_bridge`] 映射为 [`InputEvent`]）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebInputEvent {
    /// 指针移动（前端传 stage 内 CSS 坐标与 stage CSS 宽高）。
    MouseMove {
        /// 相对 stage 左上角的 X（CSS 像素）。
        x: f64,
        /// 相对 stage 左上角的 Y（CSS 像素）。
        y: f64,
        /// stage 宽度（CSS 像素）。
        stage_w: f64,
        /// stage 高度（CSS 像素）。
        stage_h: f64,
    },
    /// 鼠标按键按下/抬起（DOM `MouseEvent.button`：0=左 1=中 2=右 3=后退 4=前进）。
    MouseButton {
        /// DOM 按键编号。
        button: u8,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
    },
    /// 鼠标滚轮（DOM `WheelEvent.deltaY` / `deltaX`，CSS 像素）。
    MouseWheel {
        /// 垂直滚动量（CSS 像素，向下为正）。
        delta_y_px: f64,
        /// 前端实测行高（CSS 像素，注入参数）。
        line_height_px: f64,
        /// 可选水平滚动量（CSS 像素，向右为正）。
        #[serde(default)]
        delta_x_px: f64,
    },
    /// 键盘按键按下/抬起（DOM `KeyboardEvent.code` + 修饰键精确快照）。
    Key {
        /// DOM `KeyboardEvent.code`（如 `"KeyA"` / `"ArrowLeft"`）。
        code: String,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
        /// Ctrl 是否处于按下态。
        #[serde(default)]
        ctrl: bool,
        /// Shift 是否处于按下态。
        #[serde(default)]
        shift: bool,
        /// Alt / Option 是否处于按下态。
        #[serde(default)]
        alt: bool,
        /// Meta / Command 是否处于按下态。
        #[serde(default)]
        meta: bool,
    },
}

/// 前端 `ResizeObserver` 上报的 stage 占位矩形（CSS 像素）。
#[derive(Debug, Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageRectMsg {
    /// 左上角 X（CSS 像素）。
    pub x: f64,
    /// 左上角 Y（CSS 像素）。
    pub y: f64,
    /// 宽度（CSS 像素）。
    pub width: f64,
    /// 高度（CSS 像素）。
    pub height: f64,
}

/// 前端「设置 → 网络」保存信令与中继服务器配置的请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveServerConfigRequest {
    /// 是否启用信令与中继服务器。
    pub enabled: bool,
    /// 服务器主机名或 IP（支持可选 `:端口`）。
    pub server_host: String,
    /// 信令端口（1..=65535，默认 `21116`）。
    pub signal_port: u16,
    /// 中继端口（1..=65535，默认 `21117`）。
    pub relay_port: u16,
}

/// W4 添加端口映射表单输入。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapForm {
    /// 映射名称。
    pub name: String,
    /// 本地端口字符串。
    pub local_port: String,
    /// 目标地址字符串。
    pub target_addr: String,
    /// 目标端口字符串。
    pub target_port: String,
}

/// W4 添加端口映射表单校验结果（错误文案原样返给前端红框）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapFormErrors {
    /// 名称错误文案（通过为 `None`）。
    pub name: Option<String>,
    /// 本地端口错误文案（通过为 `None`）。
    pub local_port: Option<String>,
    /// 目标地址错误文案（通过为 `None`）。
    pub target_addr: Option<String>,
    /// 目标端口错误文案（通过为 `None`）。
    pub target_port: Option<String>,
    /// 四个字段全部通过时为 `true`。
    pub valid: bool,
}

/// 将 DOM `MouseEvent.button` 映射为 `rc_input::MouseButton`。
fn map_dom_mouse_button(button: u8) -> Option<MouseButton> {
    match button {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        3 => Some(MouseButton::X2),
        4 => Some(MouseButton::X1),
        _ => None,
    }
}

/// 将 `WebInputEvent` 映射为 0..=2 条 `InputEvent`（纯映射，便于离线单测）。
///
/// 返回 `(events, unmapped_key)`；若遇到未收录的 DOM `KeyboardEvent.code`，
/// `unmapped_key` 为 `true` 且 `events` 为空。
#[cfg_attr(not(test), allow(dead_code))]
fn map_web_input_event(ev: &WebInputEvent) -> (Vec<InputEvent>, bool) {
    map_web_input_event_for_video(ev, None)
}

/// 按当前协商视频分辨率 `video_size` 将 `WebInputEvent` 映射为 `InputEvent`。
///
/// 指针移动采用 [`input_bridge::normalize_pointer_in_video`]，与 `metal_attach` 共享同一套
/// `fit_video_rect` 等比居中视口几何，确保任意窗口缩放下鼠标点击与远端画面像素严格重合。
fn map_web_input_event_for_video(
    ev: &WebInputEvent,
    video_size: Option<(u32, u32)>,
) -> (Vec<InputEvent>, bool) {
    match ev {
        WebInputEvent::MouseMove {
            x,
            y,
            stage_w,
            stage_h,
        } => {
            let (nx, ny) =
                input_bridge::normalize_pointer_in_video(*x, *y, *stage_w, *stage_h, video_size);
            (vec![InputEvent::MouseMove { x: nx, y: ny }], false)
        }
        WebInputEvent::MouseButton { button, pressed } => match map_dom_mouse_button(*button) {
            Some(btn) => (
                vec![InputEvent::MouseButton {
                    button: btn,
                    pressed: *pressed,
                }],
                false,
            ),
            None => (Vec::new(), false),
        },
        WebInputEvent::MouseWheel {
            delta_y_px,
            line_height_px,
            delta_x_px,
        } => {
            let mut out = Vec::new();
            let dy = input_bridge::wheel_lines(*delta_y_px, *line_height_px);
            if dy != 0 {
                out.push(InputEvent::MouseWheel {
                    delta: dy,
                    horizontal: false,
                });
            }
            if delta_x_px.is_finite() && *delta_x_px != 0.0 {
                let dx = input_bridge::wheel_lines(-*delta_x_px, *line_height_px);
                if dx != 0 {
                    out.push(InputEvent::MouseWheel {
                        delta: dx,
                        horizontal: true,
                    });
                }
            }
            (out, false)
        }
        WebInputEvent::Key {
            code,
            pressed,
            ctrl,
            shift,
            alt,
            meta,
        } => match input_bridge::scancode_for(code) {
            Some(scancode) => {
                let modifiers = input_bridge::modifier_bits(*ctrl, *shift, *alt, *meta);
                (
                    vec![InputEvent::Key {
                        scancode,
                        pressed: *pressed,
                        modifiers,
                    }],
                    false,
                )
            }
            None => (Vec::new(), true),
        },
    }
}

/// 将 CSS 像素矩形乘以 `scale_factor` 换算为物理像素 [`Frame`]（T21 坑 3）。
fn css_rect_to_physical_frame(rect: StageRectMsg, scale_factor: f64) -> Frame {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    Frame {
        x: if rect.x.is_finite() {
            rect.x * scale
        } else {
            0.0
        },
        y: if rect.y.is_finite() {
            rect.y * scale
        } else {
            0.0
        },
        width: if rect.width.is_finite() {
            (rect.width * scale).max(0.0)
        } else {
            0.0
        },
        height: if rect.height.is_finite() {
            (rect.height * scale).max(0.0)
        } else {
            0.0
        },
    }
}

/// 安全回收旧会话：若持有待挂载/已挂载的 `CAMetalLayer` 租约，先在主线程 `detach_layer`
/// （期间由 [`rc_ui::session_bridge::LayerLease`] 保活底层 `MetalRenderer`，即使会话线程已
/// 退出或 `"session"` 窗口已销毁也不会 UAF），再在后台短线程 drop `lease` 与 `SessionHandle`
/// （避免主线程阻塞等待 QUIC `Bye` / `join`）。
pub(crate) fn teardown_session(app: &AppHandle, mut old: ActiveSession) {
    if let Some(lease) = old.layer_state.take_lease() {
        let _ = app.run_on_main_thread(move || {
            metal_attach::detach_leased_layer(&lease);
            std::thread::spawn(move || {
                drop(lease);
                drop(old);
            });
        });
    } else {
        std::thread::spawn(move || drop(old));
    }
}

/// 断开并回收当前活动会话；可按需覆写或在未传入时读取 `prefs.lock_on_exit` 在断线前发送锁屏指令。
pub(crate) fn disconnect_active_session_with_lock(app: &AppHandle, lock_override: Option<bool>) {
    let (old, should_lock) = {
        let state = app.state::<AppState>();
        let lock_pref = state
            .prefs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lock_on_exit;
        let should_lock = lock_override.unwrap_or(lock_pref);
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        // H9：递增会话打开代次，使任何在途的 `open_session` (`request_connect`) 醒来后自动作废。
        state.session_open_epoch.fetch_add(1, Ordering::AcqRel);
        state::clear_session_snapshot_locked(&state.last_session_update);
        (slot.take(), should_lock)
    };
    if let Some(session) = old {
        if should_lock {
            session.handle.request_lock_on_disconnect();
        }
        let closed_terms = session.handle.tools().close_all_terminals();
        for term_id in closed_terms {
            let _ = app.emit(
                "terminal://event",
                &TerminalEvent::Exited { term_id, code: -1 },
            );
        }
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        let _ = app.emit(
            "session://update",
            &SessionUpdate::Ended {
                reason: "已主动断开远控会话".to_string(),
            },
        );
        teardown_session(app, session);
    }
    let _ = app.emit("session://active-state", &ActiveSessionInfo::inactive());
}

/// 断开并回收当前活动会话（供 S1 窗口关闭事件兜底调用，T21 U10 + U-H2 + H9）。
pub(crate) fn disconnect_active_session(app: &AppHandle) {
    disconnect_active_session_with_lock(app, None);
}

/// 关闭当前活动会话上的全部远端 PTY 终端（供 W3 `"terminal"` 窗口销毁事件兜底调用，防止远端 PTY 槽位泄漏）。
pub(crate) fn close_active_terminals(app: &AppHandle) {
    let tools = {
        let state = app.state::<AppState>();
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().map(|active| active.handle.tools())
    };
    if let Some(tools) = tools {
        let _ = tools.close_all_terminals();
    }
}

/// IPC 命令：返回 §6.0 mock 设备表（`state.rs` 单一数据源）。
#[tauri::command]
pub fn list_devices(state: State<'_, AppState>) -> Result<Vec<DeviceEntry>, String> {
    state.require_authenticated()?;
    Ok(state.devices())
}

/// IPC 命令：校验连接目标（`model::parse_connect_target` 的 IPC 壳）。
#[tauri::command]
pub fn parse_connect_target(target: String) -> Result<String, String> {
    model::parse_connect_target(&target).map(|addr| addr.to_string())
}

fn resolve_session_target_meta(
    state: &AppState,
    raw_target: &str,
    parsed: &ConnectTarget,
    requested_name: Option<&str>,
) -> (String, String, Option<String>) {
    let trimmed = raw_target.trim();
    let show_lan = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .show_lan_devices;
    let peer = if show_lan {
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().and_then(|lan| {
            let peers = lan.peers();
            match parsed {
                ConnectTarget::Direct(addr) => {
                    let addr_str = addr.to_string();
                    peers.into_iter().find(|p| p.addr == addr_str)
                }
                ConnectTarget::DeviceId(id) => peers
                    .into_iter()
                    .find(|p| p.device_id.as_deref() == Some(id.as_str())),
            }
        })
    } else {
        None
    };
    let relay_target_id = match parsed {
        ConnectTarget::DeviceId(id) => Some(id.clone()),
        ConnectTarget::Direct(_) => peer.as_ref().and_then(|p| p.device_id.clone()),
    };
    let device_key = match (parsed, &peer) {
        (ConnectTarget::DeviceId(id), _) => id.clone(),
        (ConnectTarget::Direct(_), Some(p)) => format!("lan:{}", p.instance_id),
        (ConnectTarget::Direct(addr), None) => addr.to_string(),
    };
    let device_name = requested_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| peer.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| match parsed {
            ConnectTarget::DeviceId(id) => server_config::format_device_id(id),
            ConnectTarget::Direct(addr) => addr.to_string(),
            #[allow(unreachable_patterns)]
            _ => trimmed.to_string(),
        });
    (device_key, device_name, relay_target_id)
}

async fn resolve_endpoint_and_grant(
    state: &AppState,
    target: ConnectTarget,
) -> Result<(SocketAddr, Option<rc_ui::signal_client::ConnectGrant>), String> {
    match target {
        ConnectTarget::Direct(addr) => Ok((addr, None)),
        ConnectTarget::DeviceId(target_id) => {
            let client = {
                let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                rt.signal_client.as_ref().cloned()
            };
            let Some(client) = client else {
                return Err("请先在「设置 → 网络」配置并连接服务器".to_string());
            };
            if client.status().state != ServerConnectionState::Connected {
                return Err("请先在「设置 → 网络」配置并连接服务器".to_string());
            }
            let grant = tauri::async_runtime::spawn_blocking(move || {
                client.request_connect(&target_id, Duration::from_secs(10))
            })
            .await
            .map_err(|e| format!("发起中继连接任务异常：{e}"))??;
            Ok((SocketAddr::from(([0, 0, 0, 0], 0)), Some(grant)))
        }
    }
}

fn activate_session_with_cfg(
    app: &AppHandle,
    state: &AppState,
    my_epoch: u64,
    ui_cfg: UiSessionConfig,
    watch_only: bool,
    meta: ActiveSessionMeta,
) -> Result<(), String> {
    state.require_authenticated()?;
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    // C1：仅在输入校验与 `request_connect` 全部成功后才打开 S1 会话窗口。
    window::ensure_session_window(app, state, watch_only)?;
    window::set_session_window_title(app, &meta.device_name);

    // macOS 硬件路径需在主线程预创建 `CAMetalLayer`，避免后台线程创建的 layer 无法合成黑屏。
    let preallocated_layer = if cfg!(target_os = "macos") && ui_cfg.prefer_hardware {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = app.run_on_main_thread(move || {
            let _ = tx.send(metal_attach::new_metal_layer_on_main_thread().ok());
        });
        rx.recv().ok().flatten()
    } else {
        None
    };

    let (previous, active_info) = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        if state.require_authenticated().is_err()
            || state.session_open_epoch.load(Ordering::Acquire) != my_epoch
        {
            return Ok(());
        }
        let old = slot.take();
        // 在 `session` 锁内原子清空旧快照（U-H2 / U-H3），防止新窗口拉到旧会话的 `Ended` / `Negotiated`。
        state::clear_session_snapshot_locked(&state.last_session_update);
        let handle = SessionHandle::spawn_with_metal_layer(ui_cfg, preallocated_layer);
        let sid = state.next_session_id();
        let mut active = ActiveSession::new(sid, handle, watch_only);
        active.set_meta(meta);
        let info = active.info();
        *slot = Some(active);
        (old, info)
    };

    let _ = app.emit("session://active-state", &active_info);

    if let Some(old) = previous {
        // H11：替换旧会话前显式关闭其全部远端终端并清空 W3/W4 前端状态，避免残留死终端或旧端口映射。
        let closed_terms = old.handle.tools().close_all_terminals();
        for term_id in closed_terms {
            let _ = app.emit(
                "terminal://event",
                &TerminalEvent::Exited { term_id, code: -1 },
            );
        }
        let _ = app.emit("portmap://update", &Vec::<PortMapRuleSnapshot>::new());
        teardown_session(app, old);
    }

    Ok(())
}

fn local_device_id_snapshot(state: &AppState) -> Option<String> {
    let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
    rt.current_status()
        .device_id
        .or_else(|| rt.config.cached_device_id.clone())
}

fn build_session_mtls_bundle(
    state: &AppState,
    expected_peer_device_id: Option<String>,
    expected_peer_user_id: Option<String>,
) -> Option<SessionMtlsBundle> {
    let ca_cert_pem = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config
            .ca_cert_pem
            .clone()
            .or_else(|| state.auth.server_config().ca_cert_pem)
    }?;
    let current_uid = state.auth.current_session()?.user_id;
    let local_did = local_device_id_snapshot(state);
    let cert_mgr = state.auth.cert_manager();
    let (cert_pem, meta) = cert_mgr.load_device_cert()?;
    if !meta.user_id.eq_ignore_ascii_case(current_uid.trim()) {
        return None;
    }
    if let Some(ref did) = local_did {
        if meta.device_id != *did {
            return None;
        }
    }
    let key_pem = cert_mgr.tls_key_pem().ok()?;
    Some(SessionMtlsBundle {
        ca_cert_pem,
        cert_pem,
        key_pem,
        local_user_id: meta.user_id,
        expected_peer_device_id,
        expected_peer_user_id,
    })
}

pub(crate) fn build_relay_reject_mtls(
    state: &AppState,
) -> Option<rc_ui::relay_transport::RelayMtlsConfig> {
    build_session_mtls_bundle(state, None, None).map(|b| b.to_relay_mtls_config(None))
}

fn build_owner_verifier(state: &AppState) -> Option<Arc<dyn OwnerTicketVerifier>> {
    let allow_self = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_owner_self_control;
    if !allow_self {
        return None;
    }
    let server_auth_pubkey_hex = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config
            .server_auth_pubkey_hex
            .clone()
            .or_else(|| state.auth.server_config().server_auth_pubkey_hex)
    }?;
    let user_id = state.auth.current_session()?.user_id;
    let local_device_id = local_device_id_snapshot(state)?;
    RcOwnerTicketVerifier::new(&server_auth_pubkey_hex, user_id, local_device_id)
        .ok()
        .map(|v| Arc::new(v) as Arc<dyn OwnerTicketVerifier>)
}

fn build_signal_client_options(state: &AppState) -> SignalClientOptions {
    let auth = Arc::clone(&state.auth);
    let token_provider: rc_ui::signal_client::TokenProvider =
        Arc::new(move || auth.ensure_valid_access_token().map_err(format_auth_error));
    let device_pubkey = Some(state.auth.cert_manager().device_pubkey_hex());
    let allow_remote_assist = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_remote_assist;
    SignalClientOptions {
        token_provider: Some(token_provider),
        device_pubkey,
        allow_remote_assist,
    }
}

/// 将最新的 `allow_remote_assist` / `OwnerTicketVerifier` / `mTLS` 策略热同步至正在运行的被控端监听服务。
pub(crate) fn refresh_host_security_runtime(state: &AppState) {
    let (allow_remote_assist, default_encrypt) = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (prefs.allow_remote_assist, prefs.default_encrypt_connection)
    };
    let owner_verifier = build_owner_verifier(state);
    let mtls_bundle = build_session_mtls_bundle(state, None, None);
    let accept_mtls_lan = default_encrypt;
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    host.allow_remote_assist = allow_remote_assist;
    if let Some(handle) = host.handle.as_ref() {
        handle.set_allow_remote_assist(allow_remote_assist);
        handle.set_owner_verifier(owner_verifier);
        handle.set_mtls_config(mtls_bundle, accept_mtls_lan);
    }
    sync_lan_advertising_locked(state, &host);
}

fn sync_assist_state_to_server(state: &AppState, allow: bool) {
    let signal_client = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.signal_client.as_ref().cloned()
    };
    if let Some(client) = signal_client {
        let _ = client.set_allow_remote_assist(allow);
    }
    if let Some(device_id) = local_device_id_snapshot(state) {
        let auth = Arc::clone(&state.auth);
        let _ = std::thread::Builder::new()
            .name("rc-sync-assist-rest".to_string())
            .spawn(move || {
                let _ = with_valid_token(&auth, |client, token| {
                    client.set_device_allow_remote_assist(token, &device_id, allow)
                });
            });
    }
}

struct PreparedSessionSecurity {
    mtls_config: Option<SessionMtlsBundle>,
    owner_grant: Option<OwnerGrantPayload>,
    owner_signer: Option<Arc<dyn OwnerTicketSigner>>,
    lan_direct_addr: Option<SocketAddr>,
}

fn prepare_session_security(
    state: &AppState,
    relay_target_id: Option<&str>,
    relay_grant: Option<&rc_ui::signal_client::ConnectGrant>,
    force_relay: bool,
    encrypt_connection: bool,
) -> Result<PreparedSessionSecurity, String> {
    let lan_direct_addr = if !force_relay {
        relay_target_id.and_then(|did| {
            let lan = state.lan.lock().unwrap_or_else(|e| e.into_inner());
            lan.as_ref().and_then(|h| h.lookup_lan_addr(did))
        })
    } else {
        None
    };

    let owner_grant = relay_grant.and_then(|g| g.owner_grant.clone());
    let owner_signer = match owner_grant.as_ref() {
        Some(grant) => Some(Arc::new(
            state
                .auth
                .cert_manager()
                .create_owner_ticket_signer(grant.clone())
                .map_err(|e| format!("构造本人自控签名器失败：{e}"))?,
        ) as Arc<dyn OwnerTicketSigner>),
        None => None,
    };

    if encrypt_connection && relay_target_id.is_none() {
        return Err(
            "直连 IP 模式未知对端设备码，无法校验 mTLS 证书身份；请改用 9 位设备码连接，或关闭此会话的加密连接开关".to_string(),
        );
    }

    let expected_peer_user_id = if owner_grant.is_some() {
        state.auth.current_session().map(|s| s.user_id)
    } else {
        None
    };
    let mtls_config = build_session_mtls_bundle(
        state,
        relay_target_id.map(str::to_string),
        expected_peer_user_id,
    );

    if encrypt_connection && mtls_config.is_none() {
        return Err(
            "已开启加密连接 (mTLS)，但本机设备证书或服务器 Root CA 未就绪，请先在「设置 → 安全」续签设备证书".to_string(),
        );
    }

    Ok(PreparedSessionSecurity {
        mtls_config,
        owner_grant,
        owner_signer,
        lan_direct_addr,
    })
}

/// IPC 命令：打开远控会话（支持直连 `ip:port` 与 9 位设备码中继 → 校验/获取中继凭据 → 打开 S1 窗口 → 断开旧会话 → 启动新 `SessionHandle`）。
#[tauri::command]
pub async fn open_session(
    cfg: OpenSessionRequest,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_authenticated()?;

    // H9：在 `request_connect` 之前递增代次；若在途期间出现新的 `open_session` 或 S1 关窗，
    // 返回后自动丢弃过期凭据且不打开窗口 / 不启动会话。
    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;

    let parsed = model::parse_connect_input(&cfg.target)?;
    let (device_key, device_name, relay_target_id) =
        resolve_session_target_meta(&state, &cfg.target, &parsed, cfg.device_name.as_deref());

    let effective_target = if cfg.force_relay {
        match (&parsed, &relay_target_id) {
            (ConnectTarget::DeviceId(_), _) => parsed,
            (ConnectTarget::Direct(_), Some(id)) => ConnectTarget::DeviceId(id.clone()),
            (ConnectTarget::Direct(_), None) => {
                return Err(
                    "直连地址未绑定 9 位设备码，无法强制开启安全中转；请使用对方 9 位设备码连接"
                        .to_string(),
                );
            }
        }
    } else {
        parsed
    };

    let (connect_addr, relay_grant) = resolve_endpoint_and_grant(&state, effective_target).await?;

    // 若等待 `request_connect` 期间已被更新的 `open_session` 或 `disconnect_active_session` 取代，静默丢弃。
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Ok(());
    }

    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let req_width = cfg.width.unwrap_or(DEFAULT_SESSION_WIDTH).max(1);
    let req_height = cfg.height.unwrap_or(DEFAULT_SESSION_HEIGHT).max(1);
    let req_fps = cfg.fps.unwrap_or(prefs_snap.fps).max(1);
    let req_bitrate_bps = cfg.bitrate_bps.unwrap_or(prefs_snap.bitrate_bps).max(1);
    let password = cfg.password.filter(|p| !p.is_empty());
    let encrypt_connection = cfg
        .encrypt_connection
        .unwrap_or(prefs_snap.default_encrypt_connection);
    let security = prepare_session_security(
        &state,
        relay_target_id.as_deref(),
        relay_grant.as_ref(),
        cfg.force_relay,
        encrypt_connection,
    )?;

    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: password.clone(),
        watch_only: cfg.watch_only,
        width: req_width,
        height: req_height,
        fps: req_fps,
        bitrate_bps: req_bitrate_bps,
        prefer_hardware: cfg.prefer_hardware,
        clipboard_enabled: cfg.clipboard_enabled,
        audio_enabled: cfg.audio_enabled,
        relay_grant,
        force_relay: cfg.force_relay,
        encrypt_connection,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
    };

    let meta = ActiveSessionMeta {
        target: cfg.target.trim().to_string(),
        device_key,
        device_name,
        relay_target_id,
        password,
        width: req_width,
        height: req_height,
        fps: req_fps,
        bitrate_bps: req_bitrate_bps,
        prefer_hardware: cfg.prefer_hardware,
        clipboard_enabled: cfg.clipboard_enabled,
        audio_enabled: cfg.audio_enabled,
        force_relay: cfg.force_relay,
        encrypt_connection,
        mtls_active: false,
        peer_cert_fingerprint: None,
        connected: false,
        transport: None,
    };

    activate_session_with_cfg(&app, &state, my_epoch, ui_cfg, cfg.watch_only, meta)
}

/// IPC 命令：在当前远控会话中动态切换「使用安全中转连接」模式，保持窗口与画质/帧率偏好并重建会话。
#[tauri::command]
pub async fn set_session_relay_mode(
    force_relay: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_authenticated()?;
    let (watch_only, mut meta) = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) else {
            return Err("当前无活动远控会话".to_string());
        };
        (active.watch_only, active.meta.clone())
    };
    if meta.target.trim().is_empty() {
        return Err("当前会话缺少连接目标，无法切换传输模式".to_string());
    }

    let parsed = model::parse_connect_input(&meta.target)?;
    if meta.relay_target_id.is_none() {
        let (_, _, found_relay_id) =
            resolve_session_target_meta(&state, &meta.target, &parsed, Some(&meta.device_name));
        meta.relay_target_id = found_relay_id;
    }

    let effective_target = if force_relay {
        match (&parsed, &meta.relay_target_id) {
            (ConnectTarget::DeviceId(_), _) => parsed,
            (ConnectTarget::Direct(_), Some(id)) => ConnectTarget::DeviceId(id.clone()),
            (ConnectTarget::Direct(_), None) => {
                return Err(
                    "直连地址未绑定设备码或对方未注册到服务器，无法切换至安全中转；请先配置信令服务器并确认对端在线".to_string()
                );
            }
        }
    } else {
        parsed
    };

    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let (connect_addr, relay_grant) = resolve_endpoint_and_grant(&state, effective_target).await?;
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Err("会话状态已变更，请重试".to_string());
    }

    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    meta.fps = prefs_snap.fps.max(1);
    meta.bitrate_bps = prefs_snap.bitrate_bps.max(1);
    meta.force_relay = force_relay;
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.connected = false;
    meta.transport = None;

    let security = prepare_session_security(
        &state,
        meta.relay_target_id.as_deref(),
        relay_grant.as_ref(),
        force_relay,
        meta.encrypt_connection,
    )?;

    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: meta.password.clone(),
        watch_only,
        width: meta.width.max(1),
        height: meta.height.max(1),
        fps: meta.fps,
        bitrate_bps: meta.bitrate_bps,
        prefer_hardware: meta.prefer_hardware,
        clipboard_enabled: meta.clipboard_enabled,
        audio_enabled: meta.audio_enabled,
        relay_grant,
        force_relay,
        encrypt_connection: meta.encrypt_connection,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
    };

    activate_session_with_cfg(&app, &state, my_epoch, ui_cfg, watch_only, meta)?;
    Ok(if force_relay {
        "TCP Relay".to_string()
    } else {
        "直连检测中".to_string()
    })
}

/// IPC 命令：在当前活动会话中动态切换观看模式（只读禁止本地输入注入），并向主窗推送状态快照。
#[tauri::command]
pub fn set_session_watch_only(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ActiveSessionInfo, String> {
    state.require_authenticated()?;
    let (info, tools_to_close, restore_req) = {
        let mut slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_mut().filter(|a| !a.disconnecting) else {
            return Err("当前无活动远控会话".to_string());
        };
        active.watch_only = enabled;
        active.handle.set_watch_only(enabled);
        let tools_to_close = enabled.then(|| active.handle.tools());
        let restore_req = if !enabled && active.meta.connected && !active.portmaps_restored {
            active.portmaps_restored = true;
            let raw_key = if !active.meta.device_key.trim().is_empty() {
                active.meta.device_key.trim()
            } else {
                active.meta.target.trim()
            };
            (!raw_key.is_empty()).then(|| (active.id, raw_key.to_string()))
        } else {
            None
        };
        (active.info(), tools_to_close, restore_req)
    };
    if let Some(tools) = tools_to_close {
        for term_id in tools.close_all_terminals() {
            let _ = app.emit(
                "terminal://event",
                &TerminalEvent::Exited { term_id, code: -1 },
            );
        }
    }
    if let Some((sid, restore_key)) = restore_req {
        let app_handle = app.clone();
        let _ = std::thread::Builder::new()
            .name("rc-portmap-restore".to_string())
            .spawn(move || {
                let st = app_handle.state::<AppState>();
                restore_saved_portmaps_for_active_session(&st, sid, &restore_key);
            });
    }
    let _ = app.emit("session://active-state", &info);
    Ok(info)
}

/// IPC 命令：在当前活动会话中动态切换「加密连接 (mTLS)」模式并触发安全握手重建会话。
#[tauri::command]
pub async fn set_session_mtls_mode(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ActiveSessionInfo, String> {
    state.require_authenticated()?;
    let (watch_only, mut meta) = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref().filter(|a| !a.disconnecting) else {
            return Err("当前无活动远控会话".to_string());
        };
        (active.watch_only, active.meta.clone())
    };
    if meta.target.trim().is_empty() {
        return Err("当前会话缺少连接目标，无法切换加密连接模式".to_string());
    }

    let parsed = model::parse_connect_input(&meta.target)?;
    if meta.relay_target_id.is_none() {
        let (_, _, found_relay_id) =
            resolve_session_target_meta(&state, &meta.target, &parsed, Some(&meta.device_name));
        meta.relay_target_id = found_relay_id;
    }

    let effective_target = if meta.force_relay {
        match (&parsed, &meta.relay_target_id) {
            (ConnectTarget::DeviceId(_), _) => parsed,
            (ConnectTarget::Direct(_), Some(id)) => ConnectTarget::DeviceId(id.clone()),
            (ConnectTarget::Direct(_), None) => {
                return Err("当前直连会话未绑定 9 位设备码，无法在强制中转模式下重建".to_string());
            }
        }
    } else {
        parsed
    };

    // 若开启 mTLS 但本机证书或 Root CA 缺失，在断开旧会话之前提前拦截（零静默降级）。
    if enabled && build_session_mtls_bundle(&state, meta.relay_target_id.clone(), None).is_none() {
        return Err(
            "已开启加密连接 (mTLS)，但本机设备证书或服务器 Root CA 未就绪，请先在「设置 → 安全」续签设备证书".to_string(),
        );
    }

    let my_epoch = state.session_open_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    let (connect_addr, relay_grant) = resolve_endpoint_and_grant(&state, effective_target).await?;
    if state.session_open_epoch.load(Ordering::Acquire) != my_epoch {
        return Err("会话状态已变更，请重试".to_string());
    }

    let prefs_snap = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    meta.fps = prefs_snap.fps.max(1);
    meta.bitrate_bps = prefs_snap.bitrate_bps.max(1);
    meta.encrypt_connection = enabled;
    meta.mtls_active = false;
    meta.peer_cert_fingerprint = None;
    meta.connected = false;
    meta.transport = None;

    let security = prepare_session_security(
        &state,
        meta.relay_target_id.as_deref(),
        relay_grant.as_ref(),
        meta.force_relay,
        enabled,
    )?;

    let ui_cfg = UiSessionConfig {
        connect_addr,
        password: meta.password.clone(),
        watch_only,
        width: meta.width.max(1),
        height: meta.height.max(1),
        fps: meta.fps,
        bitrate_bps: meta.bitrate_bps,
        prefer_hardware: meta.prefer_hardware,
        clipboard_enabled: meta.clipboard_enabled,
        audio_enabled: meta.audio_enabled,
        relay_grant,
        force_relay: meta.force_relay,
        encrypt_connection: enabled,
        mtls_config: security.mtls_config,
        owner_grant: security.owner_grant,
        owner_signer: security.owner_signer,
        lan_direct_addr: security.lan_direct_addr,
    };

    activate_session_with_cfg(&app, &state, my_epoch, ui_cfg, watch_only, meta)?;
    Ok(state.active_session_info())
}

/// IPC 命令：拉取最近一次会话状态快照（Review M8 + U-H2 / U-H3：
/// 无活动会话时恒返回 `None`，有活动会话时仅返回匹配当前代际号的快照）。
#[tauri::command]
pub fn session_snapshot(state: State<'_, AppState>) -> Option<SessionUpdate> {
    state::current_session_snapshot(&state.session, &state.last_session_update)
}

/// 兼容 `{ "type": "..." }`（规范）与 `{ "kind": "..." }`（历史别名）的会话控制命令反序列化包装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCommandArg(pub SessionCommand);

impl<'de> serde::Deserialize<'de> for SessionCommandArg {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut val = serde_json::Value::deserialize(deserializer)?;
        if let Some(obj) = val.as_object_mut() {
            // L5：若 `"type"` 缺失或为 `null`，回退采用 `"kind"` 作为标签字段。
            if !obj.get("type").is_some_and(|v| !v.is_null()) {
                if let Some(kind) = obj.remove("kind") {
                    obj.insert("type".to_string(), kind);
                }
            }
        }
        serde_json::from_value(val)
            .map(SessionCommandArg)
            .map_err(serde::de::Error::custom)
    }
}

/// IPC 命令：转发低频控制命令（`Disconnect` / `SetBitrate` / `StageRect` /
/// `SetClipboardEnabled` / `SetAudioMuted`）。
///
/// 收到 `Disconnect` 时先在主线程摘除已挂载的 `CAMetalLayer`，再发送 `Disconnect`
/// 让会话线程走 `Bye` 优雅收尾并经轮询器推送 `Ended`（T21 U10 / 坑 2）。
#[tauri::command]
pub fn session_command(
    cmd: SessionCommandArg,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_authenticated()?;
    let cmd = cmd.0;
    match cmd {
        SessionCommand::Disconnect => {
            let lock_on_exit = state
                .prefs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lock_on_exit;
            if let Some((sid, lease)) = state::begin_disconnect(&state.session, |h| {
                if lock_on_exit {
                    h.request_lock_on_disconnect();
                }
                h.send(SessionCommand::Disconnect);
            }) {
                let app_clone = app.clone();
                let _ = app.run_on_main_thread(move || {
                    metal_attach::detach_leased_layer(&lease);
                    let st = app_clone.state::<AppState>();
                    let _ = state::finish_disconnect_on_main_thread(&st.session, sid, |h| {
                        if lock_on_exit {
                            h.request_lock_on_disconnect();
                        }
                        h.send(SessionCommand::Disconnect);
                    });
                    drop(lease);
                });
            }
        }
        other => {
            let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = slot.as_ref() {
                if !active.disconnecting {
                    active.handle.send(other);
                }
            }
        }
    }
    Ok(())
}

/// IPC 命令：WebView DOM 输入事件 → `input_bridge` 映射 → `SessionHandle::send_input`。
///
/// 未收录键码静默丢弃并计数；观看模式（`watch_only`）或无活动会话时静默忽略（T21 U9）。
#[tauri::command]
pub fn send_input_event(ev: WebInputEvent, state: State<'_, AppState>) -> Result<(), String> {
    state.require_authenticated()?;
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = slot.as_ref() else {
        return Ok(());
    };
    if active.watch_only {
        return Ok(());
    }
    let (mapped, unmapped_key) = map_web_input_event_for_video(&ev, active.video_size);
    if unmapped_key {
        drop(slot);
        let _ = state.record_dropped_key();
        return Ok(());
    }
    if mapped.is_empty() {
        return Ok(());
    }
    for input_ev in mapped {
        active.handle.send_input(input_ev);
    }
    Ok(())
}

/// IPC 命令：前端 `ResizeObserver` 上报 stage 矩形（CSS 像素）。
///
/// Rust 侧乘以 `window.scale_factor()` 换算物理像素（T21 坑 3），按协商视频宽高比计算等比居中
/// 视口后转发 `StageRect` 命令，并在 `CAMetalLayer` 已挂载时经主线程调用
/// `metal_attach::sync_layer_frame`（T21 坑 2）。
#[tauri::command]
pub fn stage_rect(
    rect: StageRectMsg,
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AppState>,
) {
    let scale = window.scale_factor().unwrap_or(1.0);
    let frame = css_rect_to_physical_frame(rect, scale);

    let sync_target = state::prepare_stage_rect(&state.session, frame, |handle, effective| {
        let phys_w = effective.width.round() as u32;
        let phys_h = effective.height.round() as u32;
        if phys_w > 0 && phys_h > 0 {
            handle.send(SessionCommand::StageRect {
                width: phys_w,
                height: phys_h,
            });
        }
    });

    if let Some((sid, lease, effective_frame)) = sync_target {
        let app_clone = app.clone();
        let _ = app.run_on_main_thread(move || {
            let st = app_clone.state::<AppState>();
            if state::should_sync_layer_on_main_thread(&st.session, sid, &lease) {
                let _ = metal_attach::sync_leased_layer_frame(&lease, effective_frame);
            }
            drop(lease);
        });
    }
}

/// IPC 命令：W4 添加端口映射表单校验（`model` 三函数的 IPC 壳）。
#[tauri::command]
pub fn validate_map_form(form: MapForm) -> MapFormErrors {
    let name = model::validate_map_name(&form.name).err();
    let local_port = model::validate_port(&form.local_port).err();
    let target_addr = model::validate_target_addr(&form.target_addr).err();
    let target_port = model::validate_port(&form.target_port).err();
    let valid =
        name.is_none() && local_port.is_none() && target_addr.is_none() && target_port.is_none();
    MapFormErrors {
        name,
        local_port,
        target_addr,
        target_port,
        valid,
    }
}

/// IPC 命令：写入系统剪贴板（P2「复制并分享」，经 `tauri-plugin-clipboard-manager`）。
#[tauri::command]
pub fn copy_text(text: String, app: AppHandle) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────
// 被控端服务与局域网发现控制（All-in-One Phase 2）
// ─────────────────────────────────────────────────────────────────

/// 在已持有 `state.host` 锁的前提下同步局域网信标广播状态（U-M6：严格遵守 `host -> lan` 锁序，消除 TOCTOU）。
pub(crate) fn sync_lan_advertising_locked(state: &AppState, host: &HostRuntime) {
    let target = host
        .handle
        .as_ref()
        .map(|h| (h.listen_addr().port(), !host.password.is_empty()));
    if let Some(lan) = state.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        lan.set_advertising(target);
    }
}

/// 启动被控端服务（可指定绑定地址、后端偏好与是否允许他人远程协助）。
pub(crate) fn start_host_service_with_policy(
    state: &AppState,
    listen_addr: SocketAddr,
    prefer_hardware: bool,
    allow_remote_assist: bool,
) -> Result<HostStatusSnapshot, String> {
    let default_encrypt = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .default_encrypt_connection;
    let owner_verifier = build_owner_verifier(state);
    let mtls_config = build_session_mtls_bundle(state, None, None);
    let accept_mtls_lan = default_encrypt;

    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    host.allow_remote_assist = allow_remote_assist;
    if let Some(handle) = host.handle.as_ref() {
        handle.set_allow_remote_assist(allow_remote_assist);
        handle.set_owner_verifier(owner_verifier);
        handle.set_mtls_config(mtls_config, accept_mtls_lan);
    } else {
        let password = (!host.password.is_empty()).then(|| host.password.clone());
        let handle = HostHandle::spawn(UiHostConfig {
            listen_addr,
            password,
            approval_timeout_us: rc_session::auth::DEFAULT_AUTH_TIMEOUT_US,
            prefer_hardware,
            require_approval: true,
            auto_accept_with_password: true,
            auto_rotate_password: !host.custom_password,
            clipboard_enabled: true,
            audio_enabled: true,
            allow_remote_assist,
            owner_verifier,
            mtls_config,
            accept_mtls_lan,
        })?;
        let actual = handle.listen_addr();
        host.listen_addr = Some(actual.to_string());
        host.display_addr = Some(HostRuntime::format_display_addr(actual));
        host.backend = if prefer_hardware {
            "auto".to_string()
        } else {
            "fake".to_string()
        };
        host.connected_peer = None;
        host.pending_approval = None;
        host.handle = Some(handle);
    }
    sync_lan_advertising_locked(state, &host);
    Ok(host.snapshot())
}

/// 启动被控端服务（可指定绑定地址与后端偏好，便于测试使用 `127.0.0.1:0`）。
#[cfg(test)]
pub(crate) fn start_host_service_on(
    state: &AppState,
    listen_addr: SocketAddr,
    prefer_hardware: bool,
) -> Result<HostStatusSnapshot, String> {
    start_host_service_with_policy(state, listen_addr, prefer_hardware, true)
}

/// 停止被控端服务并撤回局域网广播，同时返回被取消的在途审批代次（U-M5 / U-M6）；
/// `blocking = true` 时同步回收工作线程（供退出时调用）。
pub(crate) fn stop_host_service_with_cancelled(
    state: &AppState,
    blocking: bool,
) -> (HostStatusSnapshot, Option<u64>) {
    let (old_handle, snapshot, cancelled_gen) = {
        let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
        let old = host.handle.take();
        let cancelled = host.pending_approval.take().map(|p| p.generation);
        host.listen_addr = None;
        host.display_addr = None;
        host.connected_peer = None;
        host.backend.clear();
        sync_lan_advertising_locked(state, &host);
        (old, host.snapshot(), cancelled)
    };

    if let Some(mut handle) = old_handle {
        if blocking {
            handle.stop();
        } else {
            std::thread::spawn(move || {
                handle.stop();
            });
        }
    }

    (snapshot, cancelled_gen)
}

/// 停止被控端服务并撤回局域网广播；`blocking = true` 时同步回收工作线程（供退出时调用）。
pub(crate) fn stop_host_service(state: &AppState, blocking: bool) -> HostStatusSnapshot {
    stop_host_service_with_cancelled(state, blocking).0
}

/// 应用退出或主窗口销毁时幂等回收认证维护线程、被控端服务、局域网发现线程与信令服务器客户端。
pub(crate) fn shutdown_host_and_lan(app: &AppHandle) {
    disconnect_active_session(app);
    window::close_session_and_tool_windows(app);
    let state = app.state::<AppState>();
    stop_auth_maintainer(&state);
    shutdown_signal_client(&state);
    let _ = stop_host_service(&state, true);
    let old_lan = state.lan.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut lan) = old_lan {
        lan.stop();
    }
}

/// 更新被控端密码的纯状态实现（便于离线单元测试）。
fn set_host_password_inner(
    state: &AppState,
    password: Option<String>,
    regenerate: bool,
) -> Result<HostStatusSnapshot, String> {
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    if regenerate {
        host.password = host_bridge::generate_temp_code();
        host.custom_password = false;
    } else {
        match password.filter(|p| !p.is_empty()) {
            Some(pw) => {
                host.password = pw;
                host.custom_password = true;
            }
            None => {
                host.password.clear();
                host.custom_password = true;
            }
        }
    }
    let opt_pw = (!host.password.is_empty()).then(|| host.password.clone());
    if let Some(handle) = host.handle.as_ref() {
        handle.set_password_mode(opt_pw, !host.custom_password);
    }
    sync_lan_advertising_locked(state, &host);
    Ok(host.snapshot())
}

/// 回写机主审批裁决的纯状态实现。
fn respond_approval_inner(state: &AppState, generation: u64, accepted: bool) -> Result<(), String> {
    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = host.handle.as_ref() else {
        return Err("被控端服务未启动".to_string());
    };
    let applied = handle.respond_approval(generation, accepted);
    if host
        .pending_approval
        .as_ref()
        .is_some_and(|p| p.generation == generation)
    {
        host.pending_approval = None;
    }
    if applied {
        Ok(())
    } else {
        Err("审批请求已过期或不存在".to_string())
    }
}

/// 断开当前控制端连接的纯状态实现。
fn disconnect_host_client_inner(state: &AppState) -> Result<(), String> {
    let host = state.host.lock().unwrap_or_else(|e| e.into_inner());
    let Some(handle) = host.handle.as_ref() else {
        return Err("被控端服务未启动".to_string());
    };
    handle.disconnect_client();
    Ok(())
}

/// IPC 命令：获取当前本机被控端状态快照。
#[tauri::command]
pub fn get_host_status(state: State<'_, AppState>) -> Result<HostStatusSnapshot, String> {
    state.require_authenticated()?;
    Ok(state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot())
}

/// IPC 命令：检测当前系统权限状态（macOS 只查不弹；非 macOS 返回全放行占位快照）。
#[tauri::command]
pub fn check_permissions() -> PermissionSnapshot {
    permissions::check_permissions()
}

/// IPC 命令：请求 macOS 屏幕录制权限并返回最新系统权限快照。
#[tauri::command]
pub fn request_screen_recording_permission() -> PermissionSnapshot {
    permissions::request_screen_recording()
}

/// IPC 命令：打开指定权限对应的系统设置隐私面板（`"screen_recording"` / `"accessibility"` / `"input_monitoring"`）。
#[tauri::command]
pub fn open_permission_settings(kind: String) -> Result<(), String> {
    permissions::open_permission_settings(&kind)
}

fn persist_prefs_mutation(
    state: &AppState,
    path: &Path,
    mutate: impl FnOnce(&mut AppPrefs),
) -> Result<(), String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        mutate(&mut prefs);
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)
}

#[cfg(test)]
fn persist_allow_remote_assist_to_path(
    state: &AppState,
    enabled: bool,
    path: &Path,
) -> Result<(), String> {
    persist_prefs_mutation(state, path, |prefs| {
        prefs.allow_remote_assist = enabled;
    })
}

fn persist_custom_host_password_to_path(
    state: &AppState,
    custom_password: Option<String>,
    path: &Path,
) -> Result<(), String> {
    persist_prefs_mutation(state, path, |prefs| {
        prefs.custom_host_password = custom_password;
    })
}

/// 若已开启「允许其他设备远控本机」或「允许本人其他设备免密自控本机」且系统权限均就绪，则在认证放行后自动启用被控端监听。
pub(crate) fn auto_start_host_if_permitted(state: &AppState) {
    let (allow_assist, allow_self) = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        (prefs.allow_remote_assist, prefs.allow_owner_self_control)
    };
    if !allow_assist && !allow_self {
        return;
    }
    let perms = permissions::check_permissions();
    if permissions::ensure_host_ready(&perms).is_ok() {
        let default_addr = SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
        let _ = start_host_service_with_policy(state, default_addr, true, allow_assist);
    }
}

/// IPC 命令：开启或关闭本机「允许他人远程协助」服务（入口强门禁校验 `gate == Authenticated`；
/// 开启前在 macOS 校验必需系统权限；关闭时若仍开启本人免密自控则保留仅本人通道监听并拒绝外来协助；
/// 同步更新局域网信标、信令服务器在线状态与持久化偏好，关闭时取消在途外来审批弹窗 U-M5）。
#[tauri::command]
pub fn set_host_enabled(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<HostStatusSnapshot, String> {
    state.require_authenticated()?;
    if enabled {
        let perms = permissions::check_permissions();
        permissions::ensure_host_ready(&perms)?;

        let prefs_path = app_prefs::default_prefs_path();
        let snapshot = {
            let _io = state
                .prefs_io_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_allow = {
                let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                let prev = prefs.allow_remote_assist;
                prefs.allow_remote_assist = true;
                let snapshot_prefs = prefs.clone();
                drop(prefs);
                app_prefs::save_to_path(&prefs_path, &snapshot_prefs).map_err(|err| {
                    let mut revert = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    revert.allow_remote_assist = prev;
                    err
                })?;
                prev
            };
            let default_addr = SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
            match start_host_service_with_policy(&state, default_addr, true, true) {
                Ok(snap) => snap,
                Err(err) => {
                    let reverted = {
                        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                        prefs.allow_remote_assist = prev_allow;
                        prefs.clone()
                    };
                    let _ = app_prefs::save_to_path(&prefs_path, &reverted);
                    return Err(err);
                }
            }
        };
        sync_assist_state_to_server(&state, true);
        Ok(snapshot)
    } else {
        let prefs_path = app_prefs::default_prefs_path();
        let (snapshot, cancelled_gen, keep_owner_listener) = {
            let _io = state
                .prefs_io_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (prev_allow, keep_owner) = {
                let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                let prev = prefs.allow_remote_assist;
                prefs.allow_remote_assist = false;
                let snapshot_prefs = prefs.clone();
                let keep_owner = prefs.allow_owner_self_control;
                drop(prefs);
                app_prefs::save_to_path(&prefs_path, &snapshot_prefs).map_err(|err| {
                    let mut revert = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                    revert.allow_remote_assist = prev;
                    err
                })?;
                (prev, keep_owner)
            };
            let _ = prev_allow;
            if keep_owner {
                let (snapshot, cancelled_gen) = {
                    let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                    host.allow_remote_assist = false;
                    let cancelled = host.pending_approval.take().map(|p| p.generation);
                    if let Some(handle) = host.handle.as_ref() {
                        if let Some(gen) = cancelled {
                            let _ = handle.respond_approval(gen, false);
                        }
                        handle.set_allow_remote_assist(false);
                    }
                    sync_lan_advertising_locked(&state, &host);
                    (host.snapshot(), cancelled)
                };
                (snapshot, cancelled_gen, true)
            } else {
                let (snapshot, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
                (snapshot, cancelled_gen, false)
            }
        };
        if keep_owner_listener {
            refresh_host_security_runtime(&state);
        }
        sync_assist_state_to_server(&state, false);
        if let Some(generation) = cancelled_gen {
            let _ = app.emit(
                "host://update",
                &HostUpdate::ApprovalCancelled { generation },
            );
        }
        Ok(snapshot)
    }
}

/// IPC 命令：设置自定义访问密码或重新生成临时验证码，并立即同步至活动被控端及持久化偏好。
#[tauri::command]
pub fn set_host_password(
    password: Option<String>,
    regenerate: bool,
    state: State<'_, AppState>,
) -> Result<HostStatusSnapshot, String> {
    state.require_authenticated()?;
    let snap = set_host_password_inner(&state, password, regenerate)?;
    let persisted_pw = if regenerate {
        None
    } else {
        Some(snap.password.clone())
    };
    if let Err(err) =
        persist_custom_host_password_to_path(&state, persisted_pw, &app_prefs::default_prefs_path())
    {
        eprintln!("[rc-ui] 持久化 custom_host_password 失败：{err}");
    }
    Ok(snap)
}

/// IPC 命令：回写机主连接审批弹窗的同意/拒绝裁决。
#[tauri::command]
pub fn respond_approval(
    generation: u64,
    accepted: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.require_authenticated()?;
    respond_approval_inner(&state, generation, accepted)
}

/// IPC 命令：主动断开当前接入本机的控制端会话（被控端继续监听新连接）。
#[tauri::command]
pub fn disconnect_host_client(state: State<'_, AppState>) -> Result<(), String> {
    state.require_authenticated()?;
    disconnect_host_client_inner(&state)
}

/// IPC 命令：返回当前局域网发现的在线被控端设备列表快照（仅当启用 `show_lan_devices` 时返回，默认隐藏不暴露）。
#[tauri::command]
pub fn list_lan_devices(state: State<'_, AppState>) -> Result<Vec<LanPeer>, String> {
    state.require_authenticated()?;
    let show_lan = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .show_lan_devices;
    if !show_lan {
        return Ok(Vec::new());
    }
    Ok(state
        .lan
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|lan| lan.peers())
        .unwrap_or_default())
}

// ─────────────────────────────────────────────────────────────────
// 信令与中继服务器配置与生命周期控制
// ─────────────────────────────────────────────────────────────────

fn collect_local_lan_addrs(state: &AppState) -> Vec<String> {
    let mut out = Vec::new();
    let display = state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .display_addr
        .clone();
    if let Some(addr) = display {
        if !addr.is_empty() {
            out.push(addr);
        }
    } else if let Some(ip) = lan_discovery::detect_local_lan_ipv4() {
        out.push(format!("{ip}:{}", host_bridge::DEFAULT_HOST_PORT));
    }
    out
}

/// 在独立后台短线程中异步停机并回收旧 `SignalClientHandle`，绝不在持锁状态或 UI/轮询线程上 `join`（H2 / H8）。
fn reap_signal_client_async(old: Option<Arc<SignalClientHandle>>) {
    if let Some(arc) = old {
        arc.request_stop();
        std::thread::spawn(move || {
            drop(arc);
        });
    }
}

/// 退出应用时停机并同步回收信令客户端：先置位 `shutting_down = true` 阻止新客户端派生，
/// 再释放 `ServerRuntime` 锁后调用 `request_stop()` 与 `stop()` 同步等待回收（可中断拨号 <= 150ms）。
pub(crate) fn shutdown_signal_client(state: &AppState) {
    let old = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.shutting_down = true;
        rt.signal_client.take()
    };
    if let Some(arc) = old {
        arc.request_stop();
        if let Some(mut handle) = Arc::into_inner(arc) {
            handle.stop();
        }
    }
}

/// 登出时停机并回收信令客户端（不置 `shutting_down = true`，支持同进程重新登录恢复连接）。
fn stop_signal_client_for_logout(state: &AppState) -> ServerStatusSnapshot {
    let (old, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let old = rt.signal_client.take();
        let snap = rt.config.disabled_snapshot();
        rt.last_status = snap.clone();
        (old, snap)
    };
    reap_signal_client_async(old);
    snap
}

/// 根据当前 `state.server.config` 启动或停用信令服务器长连接客户端，并返回最新状态快照。
pub(crate) fn start_signal_client_if_enabled(state: &AppState) -> ServerStatusSnapshot {
    let lan_addrs = collect_local_lan_addrs(state);
    let authenticated = state.gate_state() == AppGateState::Authenticated;
    let options = build_signal_client_options(state);
    let (old, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !rt.shutting_down
            && rt.config.enabled
            && !rt.config.server_host.trim().is_empty()
            && rt.config.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                rt.config.clone(),
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, snap)
    };
    reap_signal_client_async(old);
    snap
}

/// 获取当前服务器状态快照的纯状态实现（优先读取活动 `SignalClientHandle::status()`）。
fn get_server_config_inner(state: &AppState) -> ServerStatusSnapshot {
    state
        .server
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .current_status()
}

/// 保存信令/中继服务器配置到指定路径并重启信令客户端的纯状态实现（H2 + H8，便于离线单元测试）。
fn save_server_config_to_path(
    state: &AppState,
    req: SaveServerConfigRequest,
    path: &Path,
) -> Result<ServerStatusSnapshot, String> {
    if req.signal_port == 0 || req.relay_port == 0 {
        return Err("信令端口与中继端口必须为 1–65535".to_string());
    }

    let (clean_host, effective_signal_port) = if req.enabled {
        server_config::parse_server_input(&req.server_host, req.signal_port)?
    } else {
        let trimmed = req.server_host.trim();
        if trimmed.is_empty() {
            (String::new(), req.signal_port)
        } else {
            server_config::parse_server_input(trimmed, req.signal_port)
                .unwrap_or_else(|_| (trimmed.to_string(), req.signal_port))
        }
    };

    // 锁序恒为 `server_io_lock -> server`，绝不在持有 `server` 锁期间执行磁盘 I/O。
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let (next_cfg, should_wipe_auth) = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let same_endpoint =
            rt.config.server_host == clean_host && rt.config.signal_port == effective_signal_port;
        let next = ServerConfig {
            enabled: req.enabled,
            server_host: clean_host,
            signal_port: effective_signal_port,
            relay_port: req.relay_port,
            api_port: rt.config.api_port,
            use_https: rt.config.use_https,
            ca_fingerprint_sha256: if same_endpoint {
                rt.config.ca_fingerprint_sha256.clone()
            } else {
                None
            },
            ca_cert_pem: if same_endpoint {
                rt.config.ca_cert_pem.clone()
            } else {
                None
            },
            server_auth_pubkey_hex: if same_endpoint {
                rt.config.server_auth_pubkey_hex.clone()
            } else {
                None
            },
            device_key: rt.config.device_key.clone(),
            cached_device_id: if same_endpoint && req.enabled {
                rt.config.cached_device_id.clone()
            } else {
                None
            },
        };
        let should_wipe = !same_endpoint || !next.is_configured();
        (next, should_wipe)
    };

    server_config::save_to_path(path, &next_cfg)?;
    let old_logout_ctx = if should_wipe_auth {
        Some(state.auth.take_logout_context_and_wipe())
    } else {
        None
    };
    state.auth.update_server_config(next_cfg.clone());

    let lan_addrs =
        if next_cfg.enabled && !next_cfg.server_host.trim().is_empty() && next_cfg.signal_port != 0
        {
            collect_local_lan_addrs(state)
        } else {
            Vec::new()
        };

    let authenticated = state
        .auth
        .current_session()
        .is_some_and(|s| !s.must_change_password)
        && next_cfg.is_configured();
    let options = build_signal_client_options(state);
    let (old, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config = next_cfg.clone();
        rt.config_epoch = rt.config_epoch.wrapping_add(1);
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !rt.shutting_down
            && next_cfg.enabled
            && !next_cfg.server_host.trim().is_empty()
            && next_cfg.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                next_cfg,
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, snap)
    };

    drop(_io_guard);
    reap_signal_client_async(old);
    if let Some((old_cfg, prev_sess)) = old_logout_ctx {
        if prev_sess.is_some() {
            let _ = std::thread::Builder::new()
                .name("rc-server-switch-logout".to_string())
                .spawn(move || {
                    let _ = AuthSessionManager::notify_remote_logout(
                        &old_cfg,
                        prev_sess.as_ref(),
                        None,
                    );
                });
        }
    }
    Ok(snap)
}

/// IPC 命令：获取当前信令与中继服务器配置与实时连接状态快照。
#[tauri::command]
pub fn get_server_config(state: State<'_, AppState>) -> ServerStatusSnapshot {
    get_server_config_inner(&state)
}

/// IPC 命令：在后台线程池校验、持久化信令与中继服务器配置，重启信令客户端并广播 `"server://status"`（H2 + H8）。
#[tauri::command]
pub async fn save_server_config(
    cfg: SaveServerConfigRequest,
    app: AppHandle,
) -> Result<ServerStatusSnapshot, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_authenticated()?;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        save_server_config_to_path(&state, cfg, &server_config::default_config_path())
    })
    .await
    .map_err(|e| format!("保存服务器配置任务异常：{e}"))??;
    {
        let state = app.state::<AppState>();
        if state.gate_state() != AppGateState::Authenticated {
            stop_auth_maintainer_async(&state);
            disconnect_active_session(&app);
            window::close_session_and_tool_windows(&app);
            let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
            if let Some(generation) = cancelled_gen {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalCancelled { generation },
                );
            }
            if !host_snap.enabled {
                let _ = app.emit("host://update", &HostUpdate::Stopped);
            }
        }
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lan) = guard.as_ref() {
            lan.set_local_device_id(snap.device_id.clone());
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    let _ = app.emit("server://status", &snap);
    Ok(snap)
}

/// 生成全新 `device_key`、清空 `cached_device_id` 并持久化，重启信令客户端换取新 9 位设备 ID。
fn refresh_device_id_to_path(
    state: &AppState,
    path: &Path,
) -> Result<ServerStatusSnapshot, String> {
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let next_cfg = {
        let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        let mut cloned = rt.config.clone();
        cloned.device_key = server_config::generate_device_key();
        cloned.cached_device_id = None;
        cloned
    };

    server_config::save_to_path(path, &next_cfg)?;
    let _ = state.auth.cert_manager().clear_device_cert();
    state.auth.update_server_config(next_cfg.clone());

    let lan_addrs =
        if next_cfg.enabled && !next_cfg.server_host.trim().is_empty() && next_cfg.signal_port != 0
        {
            collect_local_lan_addrs(state)
        } else {
            Vec::new()
        };

    let authenticated = state
        .auth
        .current_session()
        .is_some_and(|s| !s.must_change_password)
        && next_cfg.is_configured();
    let options = build_signal_client_options(state);
    let (old, snap) = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        rt.config = next_cfg.clone();
        rt.config_epoch = rt.config_epoch.wrapping_add(1);
        let old = rt.signal_client.take();
        let can_start = authenticated
            && !rt.shutting_down
            && next_cfg.enabled
            && !next_cfg.server_host.trim().is_empty()
            && next_cfg.signal_port != 0;
        let snap = if can_start {
            let handle = Arc::new(SignalClientHandle::spawn_with_options(
                next_cfg,
                lan_discovery::local_hostname(),
                lan_addrs,
                options,
                |_| {},
            ));
            let snap = handle.status();
            rt.signal_client = Some(handle);
            snap
        } else {
            rt.config.disabled_snapshot()
        };
        rt.last_status = snap.clone();
        (old, snap)
    };

    drop(_io_guard);
    reap_signal_client_async(old);
    refresh_host_security_runtime(state);
    Ok(snap)
}

/// IPC 命令：更换本机 `device_key` 并重新向信令服务器注册，刷新 9 位设备 ID。
#[tauri::command]
pub async fn refresh_device_id(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ServerStatusSnapshot, String> {
    state.require_authenticated()?;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let mut snap = refresh_device_id_to_path(&state, &server_config::default_config_path())?;
        match bind_device_and_ensure_cert(&state, &server_config::default_config_path()) {
            Ok(new_did) => {
                snap.device_id = Some(new_did.clone());
                snap.device_id_formatted = Some(server_config::format_device_id(&new_did));
                {
                    let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
                    rt.last_status = snap.clone();
                }
                refresh_host_security_runtime(&state);
                Ok::<_, String>(snap)
            }
            Err(err) => {
                {
                    let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(lan) = guard.as_ref() {
                        lan.set_local_device_id(snap.device_id.clone());
                    }
                }
                refresh_host_security_runtime(&state);
                let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
                let _ = app_for_blocking.emit("server://status", &snap);
                Err(err)
            }
        }
    })
    .await
    .map_err(|e| format!("刷新设备 ID 任务异常：{e}"))??;
    {
        let state = app.state::<AppState>();
        let guard = state.lan.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lan) = guard.as_ref() {
            lan.set_local_device_id(snap.device_id.clone());
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    let _ = app.emit("server://status", &snap);
    Ok(snap)
}

/// 校验并持久化远控流与 HUD 偏好的纯状态实现（便于离线单测）。
fn save_stream_prefs_to_path(
    state: &AppState,
    patch: StreamPrefsPatch,
    path: &Path,
) -> Result<AppPrefs, String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        prefs.apply_stream_patch(patch)?;
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)?;
    Ok(next)
}

/// IPC 命令：读取持久化应用偏好（远程协助开关、画质/帧率/分辨率/HUD 状态）。
#[tauri::command]
pub fn get_app_prefs(state: State<'_, AppState>) -> Result<AppPrefs, String> {
    state.require_authenticated()?;
    Ok(state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone())
}

/// IPC 命令：增量保存远控流画质、帧率、分辨率与 HUD 显隐偏好。
#[tauri::command]
pub async fn save_stream_prefs(
    patch: StreamPrefsPatch,
    app: AppHandle,
) -> Result<AppPrefs, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_authenticated()?;
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        save_stream_prefs_to_path(&state, patch, &app_prefs::default_prefs_path())
    })
    .await
    .map_err(|e| format!("保存偏好设置任务异常：{e}"))?
}

/// IPC 命令：获取已连接设备的历史快照与壁纸 Data URL 视图列表。
#[tauri::command]
pub fn get_device_snapshots(state: State<'_, AppState>) -> Result<Vec<DeviceSnapshotView>, String> {
    state.require_authenticated()?;
    let wallpapers_dir = app_prefs::default_wallpapers_dir();
    let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(prefs.snapshot_views(&wallpapers_dir))
}

/// IPC 命令：获取当前活动远控会话只读状态快照（供主窗显示控制状态）。
#[tauri::command]
pub fn get_active_session_info(state: State<'_, AppState>) -> ActiveSessionInfo {
    state.active_session_info()
}

/// IPC 命令：在后台工作线程探测信令端口与中继端口的 TCP 连通性，返回实测 RTT 文案。
#[tauri::command]
pub async fn test_server_connection(
    server_host: String,
    signal_port: u16,
    relay_port: u16,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (clean_host, effective_signal_port) =
            server_config::parse_server_input(&server_host, signal_port)?;
        signal_client::probe_server_connectivity(
            &clean_host,
            effective_signal_port,
            relay_port,
            Duration::from_secs(5),
        )
    })
    .await
    .map_err(|e| format!("连通性测试任务异常：{e}"))?
}

// ─────────────────────────────────────────────────────────────────
// 首次启动双阶强门禁、账号认证、我的设备与安全偏好 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// 探测并保存服务器配置请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeAndSaveServerRequest {
    /// 服务器域名、IP 或 `host:port`。
    pub server_host: String,
    /// HTTP/HTTPS API 端口（默认 `21118`）。
    #[serde(default)]
    pub api_port: Option<u16>,
    /// 信令端口（默认 `21116`）。
    #[serde(default)]
    pub signal_port: Option<u16>,
    /// 中继端口（默认 `21117`）。
    #[serde(default)]
    pub relay_port: Option<u16>,
    /// API 通信是否使用 HTTPS（默认 `true`）。
    #[serde(default = "default_true")]
    pub use_https: bool,
    /// 可选期望 Root CA SHA-256 指纹（用于 TOFU 二次确认锁定）。
    #[serde(default)]
    pub expected_ca_fingerprint_sha256: Option<String>,
    /// 是否将探测成功的配置与 CA 锚定保存至 `server.json`（默认 `true`）。
    #[serde(default = "default_true")]
    pub save: bool,
}

/// 探测并保存服务器响应负载。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeServerResponse {
    /// 规范化后的服务器主机名或 IP。
    pub server_host: String,
    /// API 端口。
    pub api_port: u16,
    /// 信令端口。
    pub signal_port: u16,
    /// 中继端口。
    pub relay_port: u16,
    /// 是否启用 HTTPS。
    pub use_https: bool,
    /// 服务端健康状态。
    pub status: String,
    /// 服务端版本号。
    pub version: String,
    /// 探测往返延迟（毫秒）。
    pub latency_ms: u64,
    /// 服务端是否启用了 TLS。
    pub tls_enabled: bool,
    /// Root CA 证书 SHA-256 指纹。
    pub ca_fingerprint_sha256: Option<String>,
    /// TLS 叶子证书 SHA-256 指纹。
    pub tls_leaf_fingerprint_sha256: Option<String>,
    /// 服务端是否开放自助注册。
    pub allow_registration: bool,
    /// 本次是否已持久化保存。
    pub saved: bool,
    /// 最新门禁状态快照。
    pub gate_status: GateStatusSnapshot,
}

/// 认证公共配置响应。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfigView {
    /// 是否开放自助注册。
    pub allow_registration: bool,
}

/// 邮箱登录请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthLoginRequest {
    /// 登录邮箱。
    pub email: String,
    /// 登录密码。
    pub password: String,
}

/// 邮箱注册请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRegisterRequest {
    /// 注册邮箱。
    pub email: String,
    /// 登录密码。
    pub password: String,
    /// 可选显示名称。
    #[serde(default)]
    pub display_name: Option<String>,
}

/// 修改密码请求负载。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthChangePasswordRequest {
    /// 当前密码。
    pub current_password: String,
    /// 新密码。
    pub new_password: String,
}

/// 「我的设备」列表条目（面向前端的 `camelCase` DTO）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MyDeviceEntry {
    /// 9 位纯数字设备码。
    pub device_id: String,
    /// 格式化设备码（如 `"604 928 235"`）。
    pub formatted_id: String,
    /// 主机名。
    pub hostname: String,
    /// 操作系统标识（`"macos"` / `"windows"` / `"linux"`）。
    pub os: String,
    /// 用户自定义设备备注。
    pub alias: String,
    /// 展示名称（优先 `alias`，其次 `hostname`，兜底 `formatted_id`）。
    pub display_name: String,
    /// 是否允许他人远程协助。
    pub allow_remote_assist: bool,
    /// 是否已被禁用。
    pub disabled: bool,
    /// 是否在线。
    pub online: bool,
    /// 是否正处于会话中。
    pub in_session: bool,
    /// 是否当前本机。
    pub is_local: bool,
    /// 绑定时间（RFC3339）。
    pub bound_at: Option<String>,
    /// 最近活跃时间（RFC3339）。
    pub last_seen_at: Option<String>,
}

/// 安全设置偏好快照（面向「设置 → 安全」与 S3 控制中心）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityPrefsSnapshot {
    /// 默认开启端到端加密连接 (mTLS)。
    pub default_encrypt_connection: bool,
    /// 允许本人其他设备免密自控本机。
    pub allow_owner_self_control: bool,
    /// 是否开启允许他人远程协助。
    pub allow_remote_assist: bool,
    /// 是否显示局域网设备（默认关闭）。
    pub show_lan_devices: bool,
    /// 本机设备证书状态。
    pub cert_status: GateCertStatus,
}

fn format_auth_error(err: AuthClientError) -> String {
    match err {
        AuthClientError::Api { code, message, .. } => {
            if !message.trim().is_empty() {
                message
            } else {
                format!("请求失败（{code}）")
            }
        }
        AuthClientError::ReloginRequired(msg) => {
            if msg.trim().is_empty() {
                "登录已过期，请重新登录".to_string()
            } else {
                msg
            }
        }
        other => other.to_string(),
    }
}

fn map_my_device_entry(info: MyDeviceInfo, local_device_id: Option<&str>) -> MyDeviceEntry {
    let formatted_id = if info.formatted_id.trim().is_empty() {
        server_config::format_device_id(&info.device_id)
    } else {
        info.formatted_id.clone()
    };
    let alias_trimmed = info.alias.trim();
    let host_trimmed = info.hostname.trim();
    let display_name = if !alias_trimmed.is_empty() {
        alias_trimmed.to_string()
    } else if !host_trimmed.is_empty() {
        host_trimmed.to_string()
    } else {
        formatted_id.clone()
    };
    let is_local = local_device_id.is_some_and(|local| local == info.device_id);
    MyDeviceEntry {
        device_id: info.device_id,
        formatted_id,
        hostname: info.hostname,
        os: info.os,
        alias: info.alias,
        display_name,
        allow_remote_assist: info.allow_remote_assist,
        disabled: info.disabled,
        online: info.online,
        in_session: info.in_session,
        is_local,
        bound_at: info.bound_at,
        last_seen_at: info.last_seen_at,
    }
}

fn with_valid_token<T>(
    auth: &AuthSessionManager,
    f: impl Fn(&AuthHttpClient, &str) -> Result<T, AuthClientError>,
) -> Result<T, String> {
    let cfg = auth.server_config();
    let client = AuthHttpClient::from_server_config(&cfg).map_err(format_auth_error)?;
    let mut token = auth
        .ensure_valid_access_token()
        .map_err(format_auth_error)?;
    match f(&client, &token) {
        Ok(res) => Ok(res),
        Err(err) if err.is_unauthorized() => {
            token = auth
                .refresh_after_unauthorized(&token)
                .map_err(format_auth_error)?;
            f(&client, &token).map_err(format_auth_error)
        }
        Err(err) => Err(format_auth_error(err)),
    }
}

/// 启动后台认证维护线程（周期性轮换临期 Token 与 `< 48h` 设备证书自动续签）。
pub(crate) fn start_auth_maintainer(app: &AppHandle, state: &AppState) {
    let mut slot = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    let app_for_dev = app.clone();
    let app_for_renew = app.clone();
    let app_for_relogin = app.clone();
    let handle = state.auth.spawn_background_maintainer_with_callbacks(
        Duration::from_secs(60),
        move || {
            let st = app_for_dev.state::<AppState>();
            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone())
        },
        move || {
            let st = app_for_renew.state::<AppState>();
            refresh_host_security_runtime(&st);
            let _ = app_for_renew.emit("gate://status", &st.gate_status_snapshot());
        },
        move |reason| {
            let app_kick = app_for_relogin.clone();
            let _ = std::thread::Builder::new()
                .name("rc-auth-maintainer-kick".to_string())
                .spawn(move || {
                    force_logout_from_kick(&app_kick, reason);
                });
        },
    );
    *slot = Some(handle);
}

/// 停止并回收后台认证维护线程（同步等待，供应用退出清理使用）。
pub(crate) fn stop_auth_maintainer(state: &AppState) {
    let old = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(mut handle) = old {
        handle.stop();
    }
}

/// 在后台短线程异步通知并回收认证维护线程，避免同步 HTTP 刷新阻塞登出或被踢清理。
fn stop_auth_maintainer_async(state: &AppState) {
    let old = state
        .auth_maintainer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(handle) = old {
        let _ = std::thread::Builder::new()
            .name("rc-auth-maintainer-reaper".to_string())
            .spawn(move || drop(handle));
    }
}

fn persist_cached_device_id(state: &AppState, path: &Path, device_id: &str) -> Result<(), String> {
    let _io_guard = state
        .server_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next_cfg = {
        let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
        if rt.config.cached_device_id.as_deref() == Some(device_id) {
            return Ok(());
        }
        rt.config.cached_device_id = Some(device_id.to_string());
        rt.config.clone()
    };
    server_config::save_to_path(path, &next_cfg)?;
    state.auth.update_server_config(next_cfg);
    Ok(())
}

fn bind_device_and_ensure_cert(state: &AppState, server_cfg_path: &Path) -> Result<String, String> {
    let cfg = state.auth.server_config();
    let pubkey_hex = state.auth.cert_manager().device_pubkey_hex();
    let allow_remote_assist = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow_remote_assist;
    let hostname = lan_discovery::local_hostname();
    let os_name = std::env::consts::OS.to_string();

    let bound = with_valid_token(&state.auth, |client, token| {
        client.bind_device(
            token,
            &BindDeviceRequest {
                device_key: cfg.device_key.clone(),
                hostname: Some(hostname.clone()),
                os: Some(os_name.clone()),
                alias: None,
                device_pubkey: Some(pubkey_hex.clone()),
                allow_remote_assist: Some(allow_remote_assist),
            },
        )
    })?;

    persist_cached_device_id(state, server_cfg_path, &bound.device_id)?;
    state
        .auth
        .ensure_device_cert(&bound.device_id)
        .map_err(format_auth_error)?;
    Ok(bound.device_id)
}

/// 认证就绪后激活后台维护、信令长连接与本机常驻被控监听（支持仅本人自控通道或完整远程协助），并向前端推送最新门禁/服务器状态。
pub(crate) fn post_auth_ready(app: &AppHandle, state: &AppState) {
    if state.gate_state() != AppGateState::Authenticated {
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
        return;
    }
    start_auth_maintainer(app, state);
    let snap = start_signal_client_if_enabled(state);
    if let Some(lan) = state.lan.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        lan.set_local_device_id(snap.device_id.clone());
    }
    auto_start_host_if_permitted(state);
    refresh_host_security_runtime(state);
    let host_snap = state
        .host
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .snapshot();
    if host_snap.enabled {
        if let (Some(listen_addr), Some(display_addr)) =
            (host_snap.listen_addr, host_snap.display_addr)
        {
            let _ = app.emit(
                "host://update",
                &HostUpdate::Listening {
                    listen_addr,
                    display_addr,
                    backend: host_snap.backend,
                },
            );
        }
    }
    let _ = app.emit("server://status", &snap);
    let _ = app.emit("gate://status", &state.gate_status_snapshot());
}

fn probe_and_save_server_to_path(
    state: &AppState,
    req: ProbeAndSaveServerRequest,
    path: &Path,
) -> Result<ProbeServerResponse, String> {
    let default_api_port = req.api_port.unwrap_or(server_config::DEFAULT_API_PORT);
    let signal_port = req
        .signal_port
        .unwrap_or(server_config::DEFAULT_SIGNAL_PORT);
    let relay_port = req.relay_port.unwrap_or(server_config::DEFAULT_RELAY_PORT);
    if default_api_port == 0 || signal_port == 0 || relay_port == 0 {
        return Err("API 端口、信令端口与中继端口必须为 1–65535".to_string());
    }

    let (clean_host, effective_api_port) =
        server_config::parse_server_input(&req.server_host, default_api_port)?;

    let client = AuthHttpClient::new(clean_host.clone(), effective_api_port, req.use_https, None)
        .map_err(format_auth_error)?;
    let probe = client
        .probe_server(req.expected_ca_fingerprint_sha256.as_deref())
        .map_err(format_auth_error)?;

    *state
        .allow_registration
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = probe.allow_registration;

    let ca_fp = probe
        .ca_bundle
        .as_ref()
        .map(|b| b.ca_fingerprint_sha256.clone())
        .or_else(|| probe.health.ca_fingerprint_sha256.clone());

    let mut endpoint_logout_task: Option<(
        ServerConfig,
        Option<rc_ui::auth_client::StoredAuthSession>,
        Option<String>,
    )> = None;

    if req.save {
        let _io_guard = state
            .server_io_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let (next_cfg, endpoint_changed, old_did) = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            let same_endpoint = rt.config.server_host == clean_host
                && rt.config.api_port == effective_api_port
                && rt.config.use_https == req.use_https
                && rt.config.ca_fingerprint_sha256 == ca_fp;
            let old_did = rt
                .current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone());
            let next = ServerConfig {
                enabled: true,
                server_host: clean_host.clone(),
                signal_port,
                relay_port,
                api_port: effective_api_port,
                use_https: req.use_https,
                ca_fingerprint_sha256: ca_fp.clone(),
                ca_cert_pem: probe.ca_bundle.as_ref().map(|b| b.ca_cert_pem.clone()),
                server_auth_pubkey_hex: probe.ca_bundle.as_ref().map(|b| b.auth_pubkey_hex.clone()),
                device_key: rt.config.device_key.clone(),
                cached_device_id: if same_endpoint {
                    rt.config.cached_device_id.clone()
                } else {
                    None
                },
            };
            (next, !same_endpoint, old_did)
        };

        server_config::save_to_path(path, &next_cfg)?;
        if endpoint_changed {
            let (old_cfg, prev_sess) = state.auth.take_logout_context_and_wipe();
            if prev_sess.is_some() {
                endpoint_logout_task = Some((old_cfg, prev_sess, old_did));
            }
        }
        state.auth.update_server_config(next_cfg.clone());

        let old = {
            let mut rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.config = next_cfg;
            rt.config_epoch = rt.config_epoch.wrapping_add(1);
            rt.signal_client.take()
        };
        drop(_io_guard);
        reap_signal_client_async(old);
        if let Some((old_cfg, prev_sess, old_did)) = endpoint_logout_task {
            let _ = std::thread::Builder::new()
                .name("rc-probe-old-logout".to_string())
                .spawn(move || {
                    let _ = AuthSessionManager::notify_remote_logout(
                        &old_cfg,
                        prev_sess.as_ref(),
                        old_did.as_deref(),
                    );
                });
        }
    }

    Ok(ProbeServerResponse {
        server_host: clean_host,
        api_port: effective_api_port,
        signal_port,
        relay_port,
        use_https: req.use_https,
        status: probe.health.status,
        version: probe.health.version,
        latency_ms: probe.latency_ms,
        tls_enabled: probe.health.tls_enabled,
        ca_fingerprint_sha256: ca_fp,
        tls_leaf_fingerprint_sha256: probe.tls_leaf_fingerprint_sha256,
        allow_registration: probe.allow_registration,
        saved: req.save,
        gate_status: state.gate_status_snapshot(),
    })
}

/// IPC 命令：查询首次启动双阶门禁状态、服务器配置、当前登录用户与设备证书状态。
#[tauri::command]
pub fn get_gate_status(state: State<'_, AppState>) -> GateStatusSnapshot {
    state.gate_status_snapshot()
}

/// IPC 命令：探测服务器 `/api/v1/health` 与 `/api/v1/pki/ca`，校验并保存 CA 指纹与服务器配置。
#[tauri::command]
pub async fn probe_and_save_server(
    req: ProbeAndSaveServerRequest,
    app: AppHandle,
) -> Result<ProbeServerResponse, String> {
    let app_for_blocking = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        probe_and_save_server_to_path(&state, req, &server_config::default_config_path())
    })
    .await
    .map_err(|e| format!("探测服务器任务异常：{e}"))??;
    if res.saved {
        let state = app.state::<AppState>();
        if state.gate_state() != AppGateState::Authenticated {
            stop_auth_maintainer_async(&state);
            disconnect_active_session(&app);
            window::close_session_and_tool_windows(&app);
            let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
            if let Some(generation) = cancelled_gen {
                let _ = app.emit(
                    "host://update",
                    &HostUpdate::ApprovalCancelled { generation },
                );
            }
            if !host_snap.enabled {
                let _ = app.emit("host://update", &HostUpdate::Stopped);
            }
        }
        let _ = app.emit("gate://status", &state.gate_status_snapshot());
    }
    Ok(res)
}

/// IPC 命令：刷新并返回服务端 `allowRegistration` 公共配置。
#[tauri::command]
pub async fn fetch_auth_config(app: AppHandle) -> Result<AuthConfigView, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let cfg = state.auth.server_config();
        let client = AuthHttpClient::from_server_config(&cfg).map_err(format_auth_error)?;
        let auth_cfg = client.fetch_auth_config().map_err(format_auth_error)?;
        *state
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = auth_cfg.allow_registration;
        Ok(AuthConfigView {
            allow_registration: auth_cfg.allow_registration,
        })
    })
    .await
    .map_err(|e| format!("拉取认证配置任务异常：{e}"))?
}

/// IPC 命令：注册新账号并在成功后签发设备证书、绑定设备及启动后台服务。
#[tauri::command]
pub async fn auth_register(
    req: AuthRegisterRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, String> {
    if req.email.trim().is_empty() {
        return Err("请输入邮箱地址".to_string());
    }
    if req.password.is_empty() {
        return Err("请输入密码".to_string());
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.gate_state() == AppGateState::Unconfigured {
            return Err("请先配置并连接 RabbitControl 服务器".to_string());
        }
        let hostname = lan_discovery::local_hostname();
        let os_name = std::env::consts::OS;
        let sess = state
            .auth
            .register(
                &req.email,
                &req.password,
                req.display_name.as_deref(),
                Some(&hostname),
                Some(os_name),
            )
            .map_err(format_auth_error)?;
        if !sess.must_change_password {
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path())
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(err);
            }
            post_auth_ready(&app_for_blocking, &state);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| format!("注册任务异常：{e}"))?
}

/// IPC 命令：邮箱密码登录并在成功后签发设备证书、绑定设备及启动后台服务。
#[tauri::command]
pub async fn auth_login(
    req: AuthLoginRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, String> {
    if req.email.trim().is_empty() {
        return Err("请输入邮箱地址".to_string());
    }
    if req.password.is_empty() {
        return Err("请输入密码".to_string());
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.gate_state() == AppGateState::Unconfigured {
            return Err("请先配置并连接 RabbitControl 服务器".to_string());
        }
        let hostname = lan_discovery::local_hostname();
        let os_name = std::env::consts::OS;
        let sess = state
            .auth
            .login(&req.email, &req.password, Some(&hostname), Some(os_name))
            .map_err(format_auth_error)?;
        if !sess.must_change_password {
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path())
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(err);
            }
            post_auth_ready(&app_for_blocking, &state);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| format!("登录任务异常：{e}"))?
}

/// IPC 命令：修改当前登录账号密码；若由首次强制改密解锁则立即完成设备绑定与证书签发。
#[tauri::command]
pub async fn auth_change_password(
    req: AuthChangePasswordRequest,
    app: AppHandle,
) -> Result<GateStatusSnapshot, String> {
    if req.current_password.is_empty() || req.new_password.is_empty() {
        return Err("请填写当前密码与新密码".to_string());
    }
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        if state.auth.current_session().is_none() {
            return Err("当前未登录".to_string());
        }
        let prev_must_change = state
            .auth
            .current_session()
            .is_some_and(|s| s.must_change_password);
        let updated = state
            .auth
            .change_password(&req.current_password, &req.new_password)
            .map_err(format_auth_error)?;
        if !updated.must_change_password {
            if !prev_must_change {
                let _ = state.auth.cert_manager().clear_device_cert();
            }
            if let Err(err) =
                bind_device_and_ensure_cert(&state, &server_config::default_config_path())
            {
                let _ = perform_logout_cleanup(&app_for_blocking, false);
                return Err(err);
            }
            post_auth_ready(&app_for_blocking, &state);
        } else {
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(state.gate_status_snapshot())
    })
    .await
    .map_err(|e| format!("修改密码任务异常：{e}"))?
}

fn perform_logout_cleanup(app: &AppHandle, notify_remote: bool) -> GateStatusSnapshot {
    let state = app.state::<AppState>();
    let device_id = local_device_id_snapshot(&state);
    let (old_cfg, prev_sess) = state.auth.take_logout_context_and_wipe();

    stop_auth_maintainer_async(&state);
    disconnect_active_session(app);
    window::close_session_and_tool_windows(app);
    let (host_snap, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
    if let Some(generation) = cancelled_gen {
        let _ = app.emit(
            "host://update",
            &HostUpdate::ApprovalCancelled { generation },
        );
    }
    if !host_snap.enabled {
        let _ = app.emit("host://update", &HostUpdate::Stopped);
    }
    let srv_snap = stop_signal_client_for_logout(&state);
    if notify_remote && prev_sess.is_some() {
        let _ = AuthSessionManager::notify_remote_logout(
            &old_cfg,
            prev_sess.as_ref(),
            device_id.as_deref(),
        );
    }

    let gate_snap = state.gate_status_snapshot();
    let _ = app.emit("server://status", &srv_snap);
    let _ = app.emit("gate://status", &gate_snap);
    gate_snap
}

/// 当账号被管理员禁用/删除/踢线时，强制清理本地凭据与证书并回退门禁至 `Unauthenticated`。
pub(crate) fn force_logout_from_kick(app: &AppHandle, reason: String) {
    let _ = perform_logout_cleanup(app, false);
    let _ = app.emit("auth://kicked", serde_json::json!({ "reason": reason }));
}

/// IPC 命令：退出当前账号登录，清空凭据与设备证书、停信令与被控端并回退门禁至 `Unauthenticated`。
#[tauri::command]
pub async fn auth_logout(app: AppHandle) -> Result<GateStatusSnapshot, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || perform_logout_cleanup(&app_for_blocking, true))
        .await
        .map_err(|e| format!("退出登录任务异常：{e}"))
}

/// IPC 命令：列出当前账号下绑定的所有设备（`/api/v1/me/devices`）。
#[tauri::command]
pub async fn list_my_devices(app: AppHandle) -> Result<Vec<MyDeviceEntry>, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_authenticated()?;
        let local_did = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone())
        };
        let list = with_valid_token(&state.auth, |client, token| client.list_my_devices(token))?;
        Ok(list
            .into_iter()
            .map(|d| map_my_device_entry(d, local_did.as_deref()))
            .collect())
    })
    .await
    .map_err(|e| format!("拉取我的设备列表任务异常：{e}"))?
}

/// IPC 命令：修改「我的设备」中指定设备的备注名称。
#[tauri::command]
pub async fn rename_my_device(
    device_id: String,
    alias: String,
    app: AppHandle,
) -> Result<MyDeviceEntry, String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_authenticated()?;
        let local_did = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone())
        };
        let updated = with_valid_token(&state.auth, |client, token| {
            client.rename_device(token, &device_id, &alias)
        })?;
        Ok(map_my_device_entry(updated, local_did.as_deref()))
    })
    .await
    .map_err(|e| format!("修改设备备注任务异常：{e}"))?
}

/// IPC 命令：从「我的设备」解绑并移除指定设备。
#[tauri::command]
pub async fn remove_my_device(device_id: String, app: AppHandle) -> Result<(), String> {
    let app_for_blocking = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_authenticated()?;
        let norm_target = server_config::parse_device_id_input(&device_id)
            .ok_or_else(|| format!("设备码格式无效：{device_id:?}"))?;
        let local_did = {
            let rt = state.server.lock().unwrap_or_else(|e| e.into_inner());
            rt.current_status()
                .device_id
                .or_else(|| rt.config.cached_device_id.clone())
        };
        with_valid_token(&state.auth, |client, token| {
            client.remove_device(token, &norm_target)
        })?;
        if local_did.as_deref() == Some(norm_target.as_str()) {
            let _ = state.auth.cert_manager().clear_device_cert();
            refresh_host_security_runtime(&state);
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("移除设备任务异常：{e}"))?
}

fn security_prefs_snapshot(state: &AppState) -> SecurityPrefsSnapshot {
    let prefs = state
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    SecurityPrefsSnapshot {
        default_encrypt_connection: prefs.default_encrypt_connection,
        allow_owner_self_control: prefs.allow_owner_self_control,
        allow_remote_assist: prefs.allow_remote_assist,
        show_lan_devices: prefs.show_lan_devices,
        cert_status: state.cert_status(),
    }
}

#[cfg(test)]
fn save_security_prefs_to_path(
    state: &AppState,
    patch: SecurityPrefsPatch,
    path: &Path,
) -> Result<SecurityPrefsSnapshot, String> {
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        prefs.apply_security_patch(patch)?;
        prefs.clone()
    };
    app_prefs::save_to_path(path, &next)?;
    Ok(SecurityPrefsSnapshot {
        default_encrypt_connection: next.default_encrypt_connection,
        allow_owner_self_control: next.allow_owner_self_control,
        allow_remote_assist: next.allow_remote_assist,
        show_lan_devices: next.show_lan_devices,
        cert_status: state.cert_status(),
    })
}

/// IPC 命令：读取安全与 mTLS 加密偏好设置。
#[tauri::command]
pub fn get_security_prefs(state: State<'_, AppState>) -> Result<SecurityPrefsSnapshot, String> {
    state.require_authenticated()?;
    Ok(security_prefs_snapshot(&state))
}

/// IPC 命令：保存安全与 mTLS 加密偏好设置，并实时同步被控端策略与按需启停本人自控监听。
#[tauri::command]
pub async fn save_security_prefs(
    patch: SecurityPrefsPatch,
    app: AppHandle,
) -> Result<SecurityPrefsSnapshot, String> {
    let state_gate = app.state::<AppState>();
    state_gate.require_authenticated()?;
    let assist_changed = patch.allow_remote_assist;
    let app_for_blocking = app.clone();
    let snap = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        let snap = {
            let _io = state
                .prefs_io_lock
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let (prev, next) = {
                let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
                let prev = prefs.clone();
                prefs.apply_security_patch(patch)?;
                (prev, prefs.clone())
            };
            if let Err(err) = app_prefs::save_to_path(&app_prefs::default_prefs_path(), &next) {
                *state.prefs.lock().unwrap_or_else(|e| e.into_inner()) = prev;
                return Err(err);
            }
            if state.gate_state() == AppGateState::Authenticated {
                if next.allow_owner_self_control || next.allow_remote_assist {
                    let host_missing = state
                        .host
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .handle
                        .is_none();
                    if host_missing {
                        let perms = permissions::check_permissions();
                        if permissions::ensure_host_ready(&perms).is_ok() {
                            let default_addr =
                                SocketAddr::from(([0, 0, 0, 0], host_bridge::DEFAULT_HOST_PORT));
                            let _ = start_host_service_with_policy(
                                &state,
                                default_addr,
                                true,
                                next.allow_remote_assist,
                            );
                        }
                    } else {
                        if !next.allow_remote_assist {
                            let cancelled_gen = {
                                let mut host = state.host.lock().unwrap_or_else(|e| e.into_inner());
                                host.allow_remote_assist = false;
                                let cancelled = host.pending_approval.take().map(|p| p.generation);
                                if let Some(handle) = host.handle.as_ref() {
                                    if let Some(gen) = cancelled {
                                        let _ = handle.respond_approval(gen, false);
                                    }
                                    handle.set_allow_remote_assist(false);
                                }
                                cancelled
                            };
                            if let Some(generation) = cancelled_gen {
                                let _ = app_for_blocking.emit(
                                    "host://update",
                                    &HostUpdate::ApprovalCancelled { generation },
                                );
                            }
                        }
                        refresh_host_security_runtime(&state);
                    }
                } else {
                    let has_host = state
                        .host
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .handle
                        .is_some();
                    if has_host {
                        let (_, cancelled_gen) = stop_host_service_with_cancelled(&state, false);
                        if let Some(generation) = cancelled_gen {
                            let _ = app_for_blocking.emit(
                                "host://update",
                                &HostUpdate::ApprovalCancelled { generation },
                            );
                        }
                        let _ = app_for_blocking.emit("host://update", &HostUpdate::Stopped);
                    }
                }
            }
            SecurityPrefsSnapshot {
                default_encrypt_connection: next.default_encrypt_connection,
                allow_owner_self_control: next.allow_owner_self_control,
                allow_remote_assist: next.allow_remote_assist,
                show_lan_devices: next.show_lan_devices,
                cert_status: state.cert_status(),
            }
        };
        if let Some(allow) = assist_changed {
            sync_assist_state_to_server(&state, allow);
        }
        Ok::<_, String>(snap)
    })
    .await
    .map_err(|e| format!("保存安全偏好任务异常：{e}"))??;
    Ok(snap)
}

/// IPC 命令：手动立即续签本机 mTLS 设备证书。
#[tauri::command]
pub async fn renew_device_cert(app: AppHandle) -> Result<GateCertStatus, String> {
    let app_for_blocking = app.clone();
    let status = tauri::async_runtime::spawn_blocking(move || {
        let state = app_for_blocking.state::<AppState>();
        state.require_authenticated()?;
        let _ = state.auth.cert_manager().clear_device_cert();
        if let Err(err) = bind_device_and_ensure_cert(&state, &server_config::default_config_path())
        {
            refresh_host_security_runtime(&state);
            let _ = app_for_blocking.emit("gate://status", &state.gate_status_snapshot());
            return Err(err);
        }
        refresh_host_security_runtime(&state);
        Ok::<_, String>(state.cert_status())
    })
    .await
    .map_err(|e| format!("续签设备证书任务异常：{e}"))??;
    let state = app.state::<AppState>();
    let _ = app.emit("gate://status", &state.gate_status_snapshot());
    Ok(status)
}

// ─────────────────────────────────────────────────────────────────
// W2 文件传输 / W3 远程终端 / W4 TCP 端口映射 IPC 命令
// ─────────────────────────────────────────────────────────────────

/// W2 本地目录浏览快照（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDirSnapshot {
    /// 规范化后的目录路径。
    pub path: String,
    /// 目录下的条目列表。
    pub entries: Vec<FileEntry>,
}

/// 在极短单次持锁窗口内取出当前活动会话的工具操作句柄及其归一化设备持久化键。
fn active_session_tools_with_portmap_key(
    state: &AppState,
) -> Result<(ToolClientHandle, Option<String>), String> {
    state.require_authenticated()?;
    let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = slot.as_ref() else {
        return Err("请先建立远控会话".to_string());
    };
    if active.disconnecting || !active.handle.is_running() {
        return Err("请先建立远控会话".to_string());
    }
    if active.watch_only {
        return Err("当前为只读观看模式，无法使用文件传输、终端或端口映射".to_string());
    }
    let raw_key = if !active.meta.device_key.trim().is_empty() {
        active.meta.device_key.trim()
    } else {
        active.meta.target.trim()
    };
    let key = (!raw_key.is_empty()).then(|| raw_key.to_string());
    Ok((active.handle.tools(), key))
}

/// 在极短持锁窗口内取出当前活动会话的工具操作句柄；无活动会话、未通过门禁、正在断开或处于只读观看模式时返回可读错误。
fn active_session_tools(state: &AppState) -> Result<ToolClientHandle, String> {
    active_session_tools_with_portmap_key(state).map(|(tools, _)| tools)
}

/// 列举本机目录的纯同步实现（在 `spawn_blocking` 中执行，避免阻塞 UI 主线程）。
fn file_list_local_inner(path: Option<String>) -> Result<LocalDirSnapshot, String> {
    let target = match path {
        Some(p) if !p.trim().is_empty() => p,
        _ => rc_tools::default_home_dir().to_string_lossy().into_owned(),
    };
    let (resolved, entries) = rc_tools::list_local_dir(&target)?;
    Ok(LocalDirSnapshot {
        path: resolved,
        entries,
    })
}

/// 在本机创建目录的纯同步实现（在 `spawn_blocking` 中执行，避免阻塞 UI 主线程）。
fn file_mkdir_local_inner(parent: String, name: String) -> Result<String, String> {
    rc_tools::create_local_dir(&parent, &name)
}

/// IPC 命令：异步列举本机目录（`spawn_blocking`，无需活动远控会话即可工作）。
#[tauri::command]
pub async fn file_list_local(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<LocalDirSnapshot, String> {
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || file_list_local_inner(path))
        .await
        .map_err(|e| format!("列举本地目录任务异常：{e}"))?
}

/// IPC 命令：异步在本机指定父目录下创建新目录（`spawn_blocking`，无需活动远控会话即可工作）。
#[tauri::command]
pub async fn file_mkdir_local(
    parent: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<String, String> {
    state.require_authenticated()?;
    tauri::async_runtime::spawn_blocking(move || file_mkdir_local_inner(parent, name))
        .await
        .map_err(|e| format!("创建本地目录任务异常：{e}"))?
}

/// 校验 W3 终端行列尺寸（M10：1–512）。
fn validate_terminal_size(cols: u16, rows: u16) -> Result<(), String> {
    if cols == 0 || rows == 0 || cols > 512 || rows > 512 {
        return Err("终端行列数必须在 1–512 之间".to_string());
    }
    Ok(())
}

/// 校验 W3 单次终端输入字节上限（M10：<= 64 KiB）。
fn validate_terminal_input_len(len: usize) -> Result<(), String> {
    if len > 64 * 1024 {
        return Err("单次终端输入不能超过 64 KiB".to_string());
    }
    Ok(())
}

/// 校验 W4 添加端口映射参数（M10：名称、非零端口与目标地址）。
fn validate_portmap_add(
    name: &str,
    local_port: u16,
    target_addr: &str,
    target_port: u16,
) -> Result<(), String> {
    model::validate_map_name(name)?;
    if local_port == 0 || target_port == 0 {
        return Err("端口不能为 0，范围必须为 1–65535".to_string());
    }
    model::validate_target_addr(target_addr)?;
    Ok(())
}

/// IPC 命令：请求列举远端目录（结果通过 `"file://event"` 推送，H12 经 `spawn_blocking` 投递专用文件工作线程）。
#[tauri::command]
pub async fn file_list_remote(
    path: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_list_remote(path))
        .await
        .map_err(|e| format!("列举远端目录任务异常：{e}"))?
}

/// IPC 命令：请求在远端创建目录（结果通过 `"file://event"` 推送，H12 经 `spawn_blocking` 投递专用文件工作线程）。
#[tauri::command]
pub async fn file_mkdir_remote(
    parent: String,
    name: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_mkdir_remote(parent, name))
        .await
        .map_err(|e| format!("创建远端目录任务异常：{e}"))?
}

/// IPC 命令：上传本地文件至远端目录，返回传输任务 ID（H12：不阻塞 UI IPC 线程与 60fps 视频线程）。
#[tauri::command]
pub async fn file_upload(
    local_path: String,
    remote_dir: String,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_upload(&local_path, &remote_dir))
        .await
        .map_err(|e| format!("发起文件上传任务异常：{e}"))?
}

/// IPC 命令：从远端下载文件至本地目录，返回传输任务 ID（H12：不阻塞 UI IPC 线程与 60fps 视频线程）。
#[tauri::command]
pub async fn file_download(
    remote_path: String,
    local_dir: String,
    state: State<'_, AppState>,
) -> Result<u64, String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_download(&remote_path, &local_dir))
        .await
        .map_err(|e| format!("发起文件下载任务异常：{e}"))?
}

/// IPC 命令：取消进行中的文件传输任务（H12）。
#[tauri::command]
pub async fn file_cancel(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_cancel(id))
        .await
        .map_err(|e| format!("取消文件传输任务异常：{e}"))?
}

/// IPC 命令：获取当前文件传输队列快照（无活动会话时返回空表；读取轻量缓存副本，永不阻塞）。
#[tauri::command]
pub fn file_list_transfers(state: State<'_, AppState>) -> Vec<TransferItem> {
    match active_session_tools(&state) {
        Ok(tools) => tools.file_list_transfers(),
        Err(_) => Vec::new(),
    }
}

/// IPC 命令：清除已终态的文件传输任务记录（H12）。
#[tauri::command]
pub async fn file_clear_finished(state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_clear_finished())
        .await
        .map_err(|e| format!("清理已完成传输记录任务异常：{e}"))?
}

/// IPC 命令：移除单条文件传输任务记录（H12）。
#[tauri::command]
pub async fn file_remove_transfer(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let tools = active_session_tools(&state)?;
    tauri::async_runtime::spawn_blocking(move || tools.file_remove_transfer(id))
        .await
        .map_err(|e| format!("移除传输记录任务异常：{e}"))?
}

/// IPC 命令：在远端开启新的 PTY 终端会话，返回分配的 `termId`（M10：校验行列范围 1–512）。
#[tauri::command]
pub fn terminal_open(cols: u16, rows: u16, state: State<'_, AppState>) -> Result<u32, String> {
    validate_terminal_size(cols, rows)?;
    active_session_tools(&state)?.terminal_open(cols, rows)
}

/// IPC 命令：向远端指定 PTY 终端发送键盘/粘贴文本输入（M10：限制单次 64 KiB 并核验终端活跃）。
#[tauri::command]
pub fn terminal_input(
    term_id: u32,
    data: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_terminal_input_len(data.len())?;
    let tools = active_session_tools(&state)?;
    if !tools.has_active_terminal(term_id) {
        return Err("目标终端会话不存在或已关闭".to_string());
    }
    tools.terminal_input(term_id, data.as_bytes())
}

/// IPC 命令：调整远端指定 PTY 终端尺寸（M10：校验行列范围 1–512）。
#[tauri::command]
pub fn terminal_resize(
    term_id: u32,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    validate_terminal_size(cols, rows)?;
    active_session_tools(&state)?.terminal_resize(term_id, cols, rows)
}

/// IPC 命令：关闭远端指定 PTY 终端会话。
#[tauri::command]
pub fn terminal_close(term_id: u32, state: State<'_, AppState>) -> Result<(), String> {
    active_session_tools(&state)?.terminal_close(term_id)
}

fn persist_portmaps_locked(
    state: &AppState,
    _io_guard: &std::sync::MutexGuard<'_, ()>,
    device_key: &str,
    rules: &[PortMapRuleSnapshot],
    path: &Path,
) {
    let clean_key = device_key.trim();
    if clean_key.is_empty() {
        return;
    }
    let saved: Vec<SavedPortMapRule> = rules
        .iter()
        .map(|r| SavedPortMapRule {
            name: r.name.clone(),
            local_port: r.local_port,
            target_addr: r.target_addr.clone(),
            target_port: r.target_port,
            enabled: r.enabled,
        })
        .collect();
    let next = {
        let mut prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        prefs.set_device_portmaps(clean_key, saved);
        prefs.clone()
    };
    let _ = app_prefs::save_to_path(path, &next);
}

#[cfg_attr(not(test), allow(dead_code))]
fn persist_portmaps_to_path(
    state: &AppState,
    device_key: &str,
    rules: &[PortMapRuleSnapshot],
    path: &Path,
) {
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    persist_portmaps_locked(state, &io_guard, device_key, rules, path);
}

/// 在远控会话首次协商完成后自动恢复该设备持久化的 W4 TCP 端口映射规则。
pub(crate) fn restore_saved_portmaps_for_active_session(
    state: &AppState,
    sid: u64,
    device_key: &str,
) {
    if state.require_authenticated().is_err() {
        return;
    }
    let clean_key = device_key.trim();
    if clean_key.is_empty() {
        return;
    }
    let saved = {
        let prefs = state.prefs.lock().unwrap_or_else(|e| e.into_inner());
        prefs.saved_portmaps.get(clean_key).cloned()
    };
    let Some(saved) = saved else {
        return;
    };
    if saved.is_empty() {
        return;
    }
    let tools = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        let Some(active) = slot.as_ref() else {
            return;
        };
        if active.id != sid
            || active.watch_only
            || active.disconnecting
            || !active.handle.is_running()
        {
            return;
        }
        active.handle.tools()
    };
    let _io = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let existing = tools.portmap_list();
    for rule in saved {
        let dup = existing.iter().any(|cur| {
            cur.name == rule.name
                && cur.local_port == rule.local_port
                && cur.target_addr == rule.target_addr
                && cur.target_port == rule.target_port
        });
        if dup {
            continue;
        }
        let _ = tools.portmap_add_with_enabled(
            rule.name,
            rule.local_port,
            rule.target_addr,
            rule.target_port,
            rule.enabled,
        );
    }
}

/// IPC 命令：列出当前全部 TCP 端口映射规则（无活动会话或处于观看模式时返回空表）。
#[tauri::command]
pub fn portmap_list(state: State<'_, AppState>) -> Vec<PortMapRuleSnapshot> {
    match active_session_tools(&state) {
        Ok(tools) => tools.portmap_list(),
        Err(_) => Vec::new(),
    }
}

/// IPC 命令：添加一条 TCP 端口映射规则并持久化到设备偏好（M10：校验名称/端口，快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_add(
    name: String,
    local_port: u16,
    target_addr: String,
    target_port: u16,
    state: State<'_, AppState>,
) -> Result<PortMapRuleSnapshot, String> {
    let name = name.trim().to_string();
    let target_addr = target_addr.trim().to_string();
    validate_portmap_add(&name, local_port, &target_addr, target_port)?;
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (rule, all) = tools.portmap_add(name, local_port, target_addr, target_port)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(rule)
}

/// IPC 命令：启用或暂停指定 TCP 端口映射规则并同步持久化（快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_toggle(
    id: u64,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<PortMapRuleSnapshot, String> {
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (rule, all) = tools.portmap_toggle(id, enabled)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(rule)
}

/// IPC 命令：删除指定 TCP 端口映射规则并同步持久化（快照由 `portmap_latest` 经轮询器单点串行发射）。
#[tauri::command]
pub fn portmap_delete(id: u64, state: State<'_, AppState>) -> Result<(), String> {
    let (tools, device_key) = active_session_tools_with_portmap_key(&state)?;
    let io_guard = state
        .prefs_io_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let all = tools.portmap_delete(id)?;
    if let Some(ref key) = device_key {
        persist_portmaps_locked(
            &state,
            &io_guard,
            key,
            &all,
            &app_prefs::default_prefs_path(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::batch::modifier_bits as rc_mod;

    #[test]
    fn parse_connect_target_command_delegates_to_model() {
        assert_eq!(
            parse_connect_target("127.0.0.1:9901".to_string()).unwrap(),
            "127.0.0.1:9901"
        );
        let err = parse_connect_target("example.com:9901".to_string()).unwrap_err();
        assert!(err.contains("暂仅支持 IP:端口直连"));
    }

    #[test]
    fn validate_map_form_command_reports_field_errors() {
        let bad = validate_map_form(MapForm {
            name: String::new(),
            local_port: "0".to_string(),
            target_addr: "  ".to_string(),
            target_port: "70000".to_string(),
        });
        assert!(!bad.valid);
        assert!(bad.name.is_some());
        assert!(bad.local_port.is_some());
        assert!(bad.target_addr.is_some());
        assert!(bad.target_port.is_some());

        let good = validate_map_form(MapForm {
            name: "ssh-tunnel".to_string(),
            local_port: "2222".to_string(),
            target_addr: "127.0.0.1".to_string(),
            target_port: "22".to_string(),
        });
        assert!(good.valid);
        assert_eq!(good.name, None);
        assert_eq!(good.local_port, None);
        assert_eq!(good.target_addr, None);
        assert_eq!(good.target_port, None);
    }

    #[test]
    fn map_web_input_event_handles_all_kinds_and_unmapped_keys() {
        let (evs, unmapped) = map_web_input_event(&WebInputEvent::MouseMove {
            x: 500.0,
            y: 250.0,
            stage_w: 1000.0,
            stage_h: 500.0,
        });
        assert!(!unmapped);
        assert_eq!(evs, vec![InputEvent::MouseMove { x: 32768, y: 32768 }]);

        // 等比居中视频区对齐：14" MBP (1512x982) 渲染进 1280x752 stage 时，
        // metal_attach 的 Frame::fit_aspect 左右边缘精确对应 0 与 65535，无偏移。
        let stage_frame = Frame {
            x: 0.0,
            y: 48.0,
            width: 1280.0,
            height: 752.0,
        };
        let fitted = stage_frame.fit_aspect(1512, 982);
        let (left_evs, _) = map_web_input_event_for_video(
            &WebInputEvent::MouseMove {
                x: fitted.x,
                y: 0.0,
                stage_w: 1280.0,
                stage_h: 752.0,
            },
            Some((1512, 982)),
        );
        assert_eq!(left_evs, vec![InputEvent::MouseMove { x: 0, y: 0 }]);
        let (right_evs, _) = map_web_input_event_for_video(
            &WebInputEvent::MouseMove {
                x: fitted.x + fitted.width,
                y: fitted.height,
                stage_w: 1280.0,
                stage_h: 752.0,
            },
            Some((1512, 982)),
        );
        assert_eq!(
            right_evs,
            vec![InputEvent::MouseMove { x: 65535, y: 65535 }]
        );

        // DOM button: 0=Left, 1=Middle, 2=Right
        let (evs, _) = map_web_input_event(&WebInputEvent::MouseButton {
            button: 1,
            pressed: true,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseButton {
                button: MouseButton::Middle,
                pressed: true
            }]
        );
        let (evs, _) = map_web_input_event(&WebInputEvent::MouseButton {
            button: 2,
            pressed: false,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: false
            }]
        );

        let (evs, _) = map_web_input_event(&WebInputEvent::MouseWheel {
            delta_y_px: 32.0,
            line_height_px: 16.0,
            delta_x_px: 0.0,
        });
        assert_eq!(
            evs,
            vec![InputEvent::MouseWheel {
                delta: -2,
                horizontal: false
            }]
        );

        let (evs, unmapped) = map_web_input_event(&WebInputEvent::Key {
            code: "KeyA".to_string(),
            pressed: true,
            ctrl: true,
            shift: false,
            alt: false,
            meta: false,
        });
        assert!(!unmapped);
        assert_eq!(
            evs,
            vec![InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: rc_mod::LEFT_CTRL,
            }]
        );

        // 未收录键码：静默丢弃并标记 unmapped=true
        let (evs, unmapped) = map_web_input_event(&WebInputEvent::Key {
            code: "UnknownVendorKey".to_string(),
            pressed: true,
            ctrl: false,
            shift: false,
            alt: false,
            meta: false,
        });
        assert!(unmapped);
        assert!(evs.is_empty());
    }

    #[test]
    fn css_rect_scales_to_physical_pixels() {
        let frame = css_rect_to_physical_frame(
            StageRectMsg {
                x: 10.0,
                y: 48.0,
                width: 1280.0,
                height: 752.0,
            },
            2.0,
        );
        assert_eq!(
            frame,
            Frame {
                x: 20.0,
                y: 96.0,
                width: 2560.0,
                height: 1504.0,
            }
        );
    }

    #[test]
    fn host_service_lifecycle_and_password_updates_use_ephemeral_port() {
        let state = AppState::new_for_test();
        assert!(!state.host.lock().unwrap().snapshot().enabled);
        assert!(disconnect_host_client_inner(&state).is_err());
        assert!(respond_approval_inner(&state, 1, true).is_err());

        let started = start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();
        assert!(started.enabled);
        assert!(started.listen_addr.is_some());
        assert!(started.display_addr.is_some());
        assert!(!started.custom_password);
        assert_eq!(started.password.len(), host_bridge::TEMP_CODE_LEN);

        // 设置自定义密码
        let custom =
            set_host_password_inner(&state, Some("Secret#2026".to_string()), false).unwrap();
        assert!(custom.custom_password);
        assert_eq!(custom.password, "Secret#2026");

        // 清空密码（无密码模式）
        let cleared = set_host_password_inner(&state, Some(String::new()), false).unwrap();
        assert!(cleared.custom_password);
        assert!(cleared.password.is_empty());

        // 重新生成临时验证码
        let regen = set_host_password_inner(&state, None, true).unwrap();
        assert!(!regen.custom_password);
        assert_eq!(regen.password.len(), host_bridge::TEMP_CODE_LEN);

        // 运行期间 disconnect_host_client 返回 Ok；无待审批时 respond_approval 返回 Err。
        assert!(disconnect_host_client_inner(&state).is_ok());
        assert!(respond_approval_inner(&state, 42, true).is_err());

        // 停止服务并回收线程
        let stopped = stop_host_service(&state, true);
        assert!(!stopped.enabled);
        assert!(stopped.listen_addr.is_none());
        assert!(stopped.display_addr.is_none());
        assert!(disconnect_host_client_inner(&state).is_err());
    }

    /// 回归测试（U-M5）：关闭被控端服务时若有在途审批请求，`stop_host_service_with_cancelled`
    /// 必须取出并返回该审批代次供 `set_host_enabled(false)` 发射 `ApprovalCancelled`，且清空快照中的 `pending_approval`。
    #[test]
    fn stop_host_service_captures_pending_approval_generation_for_cancellation() {
        let state = AppState::new_for_test();
        let _ = start_host_service_on(&state, "127.0.0.1:0".parse().unwrap(), false).unwrap();

        {
            let mut host = state.host.lock().unwrap();
            host.pending_approval = Some(rc_ui::host_bridge::HostApprovalPrompt {
                generation: 17,
                session_id: 42,
                remote_addr: "192.168.1.99:55555".to_string(),
                requested_res: "1920x1080".to_string(),
                requested_fps: 60,
                password_verified: true,
            });
        }

        let (stopped, cancelled_gen) = stop_host_service_with_cancelled(&state, true);
        assert!(!stopped.enabled);
        assert!(stopped.pending_approval.is_none());
        assert_eq!(cancelled_gen, Some(17));

        // 再次停止（无在途审批）应返回 None。
        let (_, second_cancelled) = stop_host_service_with_cancelled(&state, true);
        assert_eq!(second_cancelled, None);
    }

    #[test]
    fn save_server_config_validates_preserves_device_key_and_persists() {
        let state = AppState::new_for_test();
        let original_key = state.server.lock().unwrap().config.device_key.clone();
        let dir =
            std::env::temp_dir().join(format!("rc-ui-cmd-server-cfg-test-{}", std::process::id()));
        let path = dir.join("server.json");

        // 启用但地址非法：必须拒绝，不落盘。
        let bad = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: true,
                server_host: "   ".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        );
        assert!(bad.is_err());

        // 保存禁用态合法配置：保留 device_key 并写入磁盘，递增 config_epoch。
        let disabled = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: false,
                server_host: "http://172.16.1.2:21116/".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        )
        .expect("save disabled config");
        assert!(!disabled.enabled);
        assert_eq!(disabled.server_host, "172.16.1.2");
        assert_eq!(disabled.signal_port, 21116);
        assert_eq!(disabled.relay_port, 21117);
        assert_eq!(disabled.state, ServerConnectionState::Disabled);
        assert_eq!(state.server.lock().unwrap().config_epoch, 1);

        let loaded = server_config::load_from_path(&path);
        assert_eq!(loaded.device_key, original_key);
        assert_eq!(loaded.server_host, "172.16.1.2");

        // 读取快照与当前状态一致。
        assert_eq!(get_server_config_inner(&state), disabled);

        // H8：停机后调用 save_server_config 不得派生孤儿 SignalClientHandle。
        shutdown_signal_client(&state);
        let after_shutdown = save_server_config_to_path(
            &state,
            SaveServerConfigRequest {
                enabled: true,
                server_host: "172.16.1.2".to_string(),
                signal_port: 21116,
                relay_port: 21117,
            },
            &path,
        )
        .expect("save during shutdown succeeds without spawning client");
        assert_eq!(after_shutdown.state, ServerConnectionState::Disabled);
        assert!(state.server.lock().unwrap().signal_client.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_command_arg_accepts_type_and_kind_tags() {
        let a: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_audio_muted","muted":true}"#).unwrap();
        assert_eq!(a.0, SessionCommand::SetAudioMuted { muted: true });

        let b: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_audio_muted","muted":false}"#).unwrap();
        assert_eq!(b.0, SessionCommand::SetAudioMuted { muted: false });

        let c: SessionCommandArg =
            serde_json::from_str(r#"{"type":"set_clipboard_enabled","enabled":false}"#).unwrap();
        assert_eq!(c.0, SessionCommand::SetClipboardEnabled { enabled: false });

        let d: SessionCommandArg =
            serde_json::from_str(r#"{"kind":"set_clipboard_enabled","enabled":true}"#).unwrap();
        assert_eq!(d.0, SessionCommand::SetClipboardEnabled { enabled: true });

        let e: SessionCommandArg = serde_json::from_str(r#"{"type":"disconnect"}"#).unwrap();
        assert_eq!(e.0, SessionCommand::Disconnect);

        // L5：`type: null` 时回退使用 `kind`。
        let f: SessionCommandArg =
            serde_json::from_str(r#"{"type":null,"kind":"disconnect"}"#).unwrap();
        assert_eq!(f.0, SessionCommand::Disconnect);

        let g: SessionCommandArg =
            serde_json::from_str(r#"{"type":null,"kind":"set_audio_muted","muted":true}"#).unwrap();
        assert_eq!(g.0, SessionCommand::SetAudioMuted { muted: true });
    }

    #[test]
    fn terminal_and_portmap_command_validators_enforce_bounds() {
        assert_eq!(
            validate_terminal_size(0, 24).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(80, 0).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(513, 24).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert_eq!(
            validate_terminal_size(80, 513).unwrap_err(),
            "终端行列数必须在 1–512 之间"
        );
        assert!(validate_terminal_size(1, 1).is_ok());
        assert!(validate_terminal_size(512, 512).is_ok());

        assert!(validate_terminal_input_len(64 * 1024).is_ok());
        assert_eq!(
            validate_terminal_input_len(64 * 1024 + 1).unwrap_err(),
            "单次终端输入不能超过 64 KiB"
        );

        assert!(validate_portmap_add("", 8080, "127.0.0.1", 80).is_err());
        assert!(validate_portmap_add("web", 0, "127.0.0.1", 80).is_err());
        assert!(validate_portmap_add("web", 8080, "127.0.0.1", 0).is_err());
        assert!(validate_portmap_add("web", 8080, "   ", 80).is_err());
        assert!(validate_portmap_add("web", 8080, "127.0.0.1", 80).is_ok());
    }

    #[test]
    fn local_file_commands_work_without_session_and_remote_tools_require_session() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-local-file-cmd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        let parent = dir.to_string_lossy().into_owned();

        let created = file_mkdir_local_inner(parent.clone(), "sub_folder".to_string())
            .expect("file_mkdir_local should succeed without active session");
        assert!(created.ends_with("sub_folder"));

        let listed = file_list_local_inner(Some(parent))
            .expect("file_list_local should succeed without active session");
        assert!(listed
            .entries
            .iter()
            .any(|e| e.name == "sub_folder" && e.is_dir));

        let state = AppState::new_for_test();
        let err = active_session_tools(&state)
            .err()
            .expect("remote tools must fail when unauthenticated or no session is active");
        assert_eq!(err, "请先配置并连接 RabbitControl 服务器");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_state_and_host_runtime_default_disabled_on_init() {
        let rt = HostRuntime::new();
        assert!(rt.handle.is_none());
        let rt_snap = rt.snapshot();
        assert!(!rt_snap.enabled);
        assert!(rt_snap.listen_addr.is_none());
        assert!(rt_snap.display_addr.is_none());
        assert!(rt_snap.connected_peer.is_none());
        assert!(rt_snap.pending_approval.is_none());
        assert!(!rt_snap.custom_password);
        assert_eq!(rt_snap.password.len(), host_bridge::TEMP_CODE_LEN);

        let state = AppState::new_for_test();
        let snap = state.host.lock().unwrap().snapshot();
        assert!(!snap.enabled, "应用初始化时默认不得启用被控端监听");
        assert!(snap.listen_addr.is_none());
        assert!(snap.display_addr.is_none());
    }

    #[test]
    fn open_permission_settings_command_rejects_invalid_kind() {
        let err = open_permission_settings("unknown_perm".to_string()).unwrap_err();
        assert!(err.contains("unknown_perm"));
    }

    #[test]
    fn open_session_request_allows_omitted_optional_stream_fields() {
        let raw = r#"{"target":"192.168.1.10:9901","watchOnly":true,"deviceName":"Studio"}"#;
        let req: OpenSessionRequest =
            serde_json::from_str(raw).expect("OpenSessionRequest 应支持省略可选宽高帧率码率字段");
        assert_eq!(req.target, "192.168.1.10:9901");
        assert!(req.watch_only);
        assert_eq!(req.device_name.as_deref(), Some("Studio"));
        assert!(!req.force_relay);
        assert_eq!(req.width, None);
        assert_eq!(req.height, None);
        assert_eq!(req.fps, None);
        assert_eq!(req.bitrate_bps, None);
    }

    #[test]
    fn persist_allow_remote_assist_and_stream_prefs_roundtrip_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-prefs-cmd-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        persist_allow_remote_assist_to_path(&state, true, &prefs_path)
            .expect("持久化 allow_remote_assist=true 应成功");
        let loaded = app_prefs::load_from_path(&prefs_path);
        assert!(loaded.allow_remote_assist);

        let updated = save_stream_prefs_to_path(
            &state,
            StreamPrefsPatch {
                bitrate_preset: Some("30m".to_string()),
                bitrate_bps: Some(30_000_000),
                fps: Some(120),
                resolution_width: Some(1280),
                resolution_height: Some(800),
                clear_resolution: None,
                show_hud: Some(false),
                lock_on_exit: Some(true),
            },
            &prefs_path,
        )
        .expect("增量保存流偏好应成功");
        assert_eq!(updated.bitrate_preset, "30m");
        assert_eq!(updated.bitrate_bps, 30_000_000);
        assert_eq!(updated.fps, 120);
        assert_eq!(updated.resolution_width, Some(1280));
        assert_eq!(updated.resolution_height, Some(800));
        assert!(!updated.show_hud);
        assert!(updated.lock_on_exit);

        let reloaded = app_prefs::load_from_path(&prefs_path);
        assert_eq!(reloaded, updated);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_device_id_to_path_rotates_device_key_and_clears_cached_id() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-refresh-id-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cfg_path = dir.join("server.json");

        let state = AppState::new_for_test();
        let old_key = {
            let mut rt = state.server.lock().unwrap();
            rt.config.enabled = false;
            rt.config.cached_device_id = Some("604928235".to_string());
            rt.config.device_key.clone()
        };

        let snap = refresh_device_id_to_path(&state, &cfg_path)
            .expect("refresh_device_id_to_path 应成功持久化");
        assert!(snap.device_id.is_none());

        let persisted = server_config::load_from_path(&cfg_path);
        assert_ne!(persisted.device_key, old_key);
        assert!(persisted.cached_device_id.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_custom_host_password_roundtrips_and_clears_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-custom-pw-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        persist_custom_host_password_to_path(&state, Some("ABC123".to_string()), &prefs_path)
            .expect("持久化自定义密码应成功");
        let loaded = app_prefs::load_from_path(&prefs_path);
        assert_eq!(loaded.custom_host_password.as_deref(), Some("ABC123"));

        persist_custom_host_password_to_path(&state, Some(String::new()), &prefs_path)
            .expect("持久化无密码自定义模式应成功");
        let loaded_empty = app_prefs::load_from_path(&prefs_path);
        assert_eq!(loaded_empty.custom_host_password.as_deref(), Some(""));

        persist_custom_host_password_to_path(&state, None, &prefs_path)
            .expect("清空自定义密码回退临时码应成功");
        let cleared = app_prefs::load_from_path(&prefs_path);
        assert_eq!(cleared.custom_host_password, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_state_transitions_and_require_authenticated_enforces_stages() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_path = dir.join("auth.json");
        let certs_dir = dir.join("certs");

        // 1) Unconfigured
        let state = AppState::with_initial_configs_and_paths(
            ServerConfig::default(),
            AppPrefs::default(),
            auth_path.clone(),
            certs_dir.clone(),
            None,
        );
        assert_eq!(state.gate_state(), AppGateState::Unconfigured);
        assert!(state
            .require_authenticated()
            .unwrap_err()
            .contains("请先配置"));

        // 2) Configured -> Unauthenticated
        let configured = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            api_port: 21118,
            use_https: false,
            ..ServerConfig::default()
        };
        {
            let mut rt = state.server.lock().unwrap();
            rt.config = configured.clone();
        }
        state.auth.update_server_config(configured.clone());
        assert_eq!(state.gate_state(), AppGateState::Unauthenticated);
        assert!(state
            .require_authenticated()
            .unwrap_err()
            .contains("请先登录"));

        // 3) PasswordChangeRequired
        let future_exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let must_change_sess = rc_ui::auth_client::StoredAuthSession {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            email: "admin@example.com".to_string(),
            display_name: "Admin".to_string(),
            role: "admin".to_string(),
            must_change_password: true,
            access_token: "tok-1".to_string(),
            refresh_token: "ref-1".to_string(),
            access_expires_at_unix_sec: future_exp,
            server_host: "127.0.0.1".to_string(),
            api_port: 21118,
        };
        rc_ui::auth_client::save_auth_session(&auth_path, &must_change_sess).unwrap();
        let state2 = AppState::with_initial_configs_and_paths(
            configured.clone(),
            AppPrefs::default(),
            auth_path.clone(),
            certs_dir.clone(),
            None,
        );
        let _ = state2.auth.restore_on_startup();
        assert_eq!(state2.gate_state(), AppGateState::PasswordChangeRequired);
        assert!(state2
            .require_authenticated()
            .unwrap_err()
            .contains("修改密码"));

        // 4) Authenticated
        let ready_sess = rc_ui::auth_client::StoredAuthSession {
            must_change_password: false,
            ..must_change_sess
        };
        rc_ui::auth_client::save_auth_session(&auth_path, &ready_sess).unwrap();
        let state3 = AppState::with_initial_configs_and_paths(
            configured,
            AppPrefs::default(),
            auth_path,
            certs_dir,
            None,
        );
        let _ = state3.auth.restore_on_startup();
        assert_eq!(state3.gate_state(), AppGateState::Authenticated);
        assert!(state3.require_authenticated().is_ok());

        let snap = state3.gate_status_snapshot();
        assert_eq!(snap.gate, AppGateState::Authenticated);
        assert!(snap.server_configured);
        assert_eq!(
            snap.user.as_ref().map(|u| u.email.as_str()),
            Some("admin@example.com")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_and_save_server_to_path_probes_mock_http_and_advances_gate() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock http");
        let port = listener.local_addr().unwrap().port();
        let server_thread = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if req.starts_with("GET /api/v1/pki/ca") {
                    (
                        "404 Not Found",
                        r#"{"error":"not_found","message":"no ca"}"#,
                    )
                } else if req.starts_with("GET /api/v1/health") {
                    (
                        "200 OK",
                        r#"{"status":"ok","version":"0.1.0","tls_enabled":false,"allow_registration":false}"#,
                    )
                } else {
                    ("200 OK", r#"{"allow_registration":false}"#)
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "rc-ui-probe-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let cfg_path = dir.join("server.json");

        let state = AppState::new_for_test();
        assert_eq!(state.gate_state(), AppGateState::Unconfigured);

        let res = probe_and_save_server_to_path(
            &state,
            ProbeAndSaveServerRequest {
                server_host: "127.0.0.1".to_string(),
                api_port: Some(port),
                signal_port: Some(21116),
                relay_port: Some(21117),
                use_https: false,
                expected_ca_fingerprint_sha256: None,
                save: true,
            },
            &cfg_path,
        )
        .expect("probe_and_save_server_to_path must succeed");

        assert!(res.saved);
        assert!(!res.allow_registration);
        assert_eq!(res.gate_status.gate, AppGateState::Unauthenticated);
        assert_eq!(state.gate_state(), AppGateState::Unauthenticated);
        assert!(server_config::load_from_path(&cfg_path).is_configured());

        let _ = server_thread.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_security_prefs_and_my_device_entry_mapping_work() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-sec-prefs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        let initial = security_prefs_snapshot(&state);
        assert!(initial.default_encrypt_connection);
        assert!(initial.allow_owner_self_control);
        assert!(!initial.show_lan_devices);

        let updated = save_security_prefs_to_path(
            &state,
            SecurityPrefsPatch {
                default_encrypt_connection: Some(false),
                allow_owner_self_control: Some(false),
                ..Default::default()
            },
            &prefs_path,
        )
        .expect("save_security_prefs_to_path must succeed");
        assert!(!updated.default_encrypt_connection);
        assert!(!updated.allow_owner_self_control);

        let entry = map_my_device_entry(
            MyDeviceInfo {
                device_id: "604928235".to_string(),
                formatted_id: String::new(),
                hostname: "Studio-Mac".to_string(),
                os: "macos".to_string(),
                alias: "办公室 Mac".to_string(),
                device_pubkey: String::new(),
                allow_remote_assist: true,
                disabled: false,
                online: true,
                in_session: false,
                bound_at: None,
                last_seen_at: None,
            },
            Some("604928235"),
        );
        assert_eq!(entry.formatted_id, "604 928 235");
        assert_eq!(entry.display_name, "办公室 Mac");
        assert!(entry.is_local);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_session_tools_requires_auth_and_persists_portmaps() {
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-tools-persist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let prefs_path = dir.join("prefs.json");

        let state = AppState::new_for_test();
        let err = active_session_tools(&state)
            .err()
            .expect("unauthenticated must fail");
        assert!(err.contains("配置并连接") || err.contains("登录"));

        let rules = vec![PortMapRuleSnapshot {
            id: 1,
            name: "Web".to_string(),
            local_port: 18080,
            target_addr: "127.0.0.1".to_string(),
            target_port: 80,
            enabled: true,
            status: rc_tools::PortMapStatus::Ok,
            active_conns: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            error: None,
        }];
        persist_portmaps_to_path(&state, "dev-key-1", &rules, &prefs_path);
        let reloaded = app_prefs::load_from_path(&prefs_path);
        let saved = reloaded.saved_portmaps.get("dev-key-1").unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Web");
        assert_eq!(saved[0].local_port, 18080);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
