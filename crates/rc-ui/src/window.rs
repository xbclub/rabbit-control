//! 多窗口创建（设计文档 §8.3）。
//!
//! 主窗口由 `tauri.conf.json` 的 `app.windows[0]` 定义；本模块负责按需创建
//! 独立工具窗口（W1 设置 / W2 文件传输 / W3 终端 / W4 端口映射）与会话窗口（S1）。
//!
//! 窗口已存在时只聚焦、不重建（`WebviewWindowBuilder` 对同 label 重复创建会报错）。

use tauri::{
    AppHandle, LogicalSize, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use crate::state::{self, AppState};

/// S1 会话窗口顶部工具条逻辑高度（与 `tokens.css` 的 `--session-bar-h: 48px` 一致）。
pub(crate) const SESSION_TOOLBAR_HEIGHT: f64 = 48.0;

/// 主窗口标准默认逻辑尺寸（匹配 `tauri.conf.json`）。
const MAIN_DEFAULT_W: f64 = 1000.0;
const MAIN_DEFAULT_H: f64 = 660.0;
/// 主窗口最小逻辑尺寸（匹配 `tauri.conf.json` 与 `main.css`）。
const MAIN_MIN_W: f64 = 820.0;
const MAIN_MIN_H: f64 = 540.0;
/// 2K/4K 大屏主窗口默认逻辑尺寸。
const MAIN_LARGE_W: f64 = 1120.0;
const MAIN_LARGE_H: f64 = 720.0;

/// S1 会话窗口默认与最小逻辑尺寸。
const SESSION_DEFAULT_W: f64 = 1160.0;
const SESSION_DEFAULT_H: f64 = 760.0;
const SESSION_MIN_W: f64 = 640.0;
const SESSION_MIN_H: f64 = 440.0;

/// 无法读取显示器尺寸时的逻辑分辨率兜底（14" MacBook Pro）。
const FALLBACK_MONITOR_W: f64 = 1512.0;
const FALLBACK_MONITOR_H: f64 = 982.0;

/// 工具窗口种类（对应设计文档 §2 窗口清单的 W1–W4 与 S1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolWindow {
    /// W1 设置：660×640，不可调整大小（§2）。
    Settings,
    /// W2 文件传输：1400×860，最小 1120×720（§2）。
    Transfer,
    /// W3 终端：1200×760，最小 720×480（§2）。
    Terminal,
    /// W4 端口映射：1280×780，最小 960×600（§2）。
    PortMap,
    /// S1 远控会话：默认 1160×760，最小 640×440，可全屏（§2）。
    Session,
}

impl ToolWindow {
    /// 从 IPC 传来的字符串解析窗口种类；未知值返回 `None`。
    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "settings" => Some(Self::Settings),
            "transfer" => Some(Self::Transfer),
            "terminal" => Some(Self::Terminal),
            "portmap" => Some(Self::PortMap),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    /// 窗口 label（Tauri 内唯一标识）。
    fn label(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Transfer => "transfer",
            Self::Terminal => "terminal",
            Self::PortMap => "portmap",
            Self::Session => "session",
        }
    }

    /// 窗口标题。
    fn title(self) -> &'static str {
        match self {
            Self::Settings => "设置",
            Self::Transfer => "文件传输",
            Self::Terminal => "终端",
            Self::PortMap => "端口映射",
            Self::Session => "远程桌面",
        }
    }

    /// 页面相对路径（位于 `ui/` 下）。
    fn url(self) -> &'static str {
        match self {
            Self::Settings => "settings.html",
            Self::Transfer => "transfer.html",
            Self::Terminal => "terminal.html",
            Self::PortMap => "portmap.html",
            Self::Session => "session.html",
        }
    }

    /// 窗口尺寸与最小尺寸（§2 窗口清单）。
    fn geometry(self) -> (f64, f64, f64, f64, bool) {
        match self {
            Self::Settings => (660.0, 640.0, 660.0, 640.0, false),
            Self::Transfer => (1400.0, 860.0, 1120.0, 720.0, true),
            Self::Terminal => (1200.0, 760.0, 720.0, 480.0, true),
            Self::PortMap => (1280.0, 780.0, 960.0, 600.0, true),
            Self::Session => (
                SESSION_DEFAULT_W,
                SESSION_DEFAULT_H,
                SESSION_MIN_W,
                SESSION_MIN_H,
                true,
            ),
        }
    }

    /// 是否需要透明 WebView（S1 会话窗口启用 `drawsBackground = NO`，
    /// 使挂在 `WryWebViewParent.layer` 底层 `index 0` / `zPosition = -1` 的
    /// 零拷贝 `CAMetalLayer` 透过透明 `.sn-stage` 呈现实时视频）。
    fn is_transparent(self) -> bool {
        matches!(self, Self::Session)
    }
}

/// 创建（或聚焦）一个工具窗口（可选自定义相对 URL，供会话窗口透传 `?watch=1`）。
fn show_tool_window_with_url(app: &AppHandle, kind: ToolWindow, url: &str) -> Result<(), String> {
    if let Some(existing) = app.get_webview_window(kind.label()) {
        // 已存在：聚焦并置前，避免重复创建同名窗口报错。
        existing.show().map_err(|e| e.to_string())?;
        existing.set_focus().map_err(|e| e.to_string())?;
        return Ok(());
    }

    let (width, height, min_width, min_height, resizable) = kind.geometry();
    let mut builder = WebviewWindowBuilder::new(app, kind.label(), WebviewUrl::App(url.into()))
        .title(kind.title())
        .inner_size(width, height)
        .min_inner_size(min_width, min_height)
        .resizable(resizable)
        .center();

    if kind.is_transparent() {
        builder = builder.transparent(true);
    }

    // macOS：工具窗口与主窗口同为 Overlay 标题栏 + 隐藏标题，
    // 让页面内自绘的标题栏与原生交通灯共存（与主窗口一致，见 tauri.conf.json）。
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }

    builder.build().map_err(|e| e.to_string())?;
    Ok(())
}

/// 创建（或聚焦）一个工具窗口。
fn show_tool_window(app: &AppHandle, kind: ToolWindow) -> Result<(), String> {
    show_tool_window_with_url(app, kind, kind.url())
}

/// 根据是否观看模式构造会话窗口页面 URL（U-M7）。
fn session_window_url(watch_only: bool) -> &'static str {
    if watch_only {
        "session.html?watch=1"
    } else {
        "session.html"
    }
}

/// IPC 命令：打开工具窗口（W1–W4 或 S1）。
///
/// 门禁规则：除 `"settings"` 外，其余工具/会话窗口均要求 `gate == Authenticated`。
/// 前端通过 `invoke("open_tool_window", { kind: "settings" })` 调用。
#[tauri::command]
pub fn open_tool_window(
    app: AppHandle,
    state: State<'_, AppState>,
    kind: String,
) -> Result<(), String> {
    let kind = ToolWindow::parse(&kind).ok_or_else(|| format!("未知窗口类型：{kind}"))?;
    if kind != ToolWindow::Settings {
        state.require_authenticated()?;
    }
    show_tool_window(&app, kind)
}

/// 打开（或聚焦）远控会话窗口（S1）：
/// - 若当前无活动会话，先清空残留的 `last_session_update` 快照，防新窗口拉到旧会话 `Ended`（U-H2）；
/// - `watch_only == true` 时通过 `?watch=1` 启动只读观看态（U-M7）。
pub(crate) fn ensure_session_window(
    app: &AppHandle,
    state: &AppState,
    watch_only: bool,
) -> Result<(), String> {
    state::clear_session_snapshot_if_idle(&state.session, &state.last_session_update);
    let url = session_window_url(watch_only);
    show_tool_window_with_url(app, ToolWindow::Session, url)
}

/// IPC 命令：打开远控会话窗口（S1）。
///
/// - 门禁规则：要求 `gate == Authenticated`；
/// - 若当前无活动会话，先清空残留的 `last_session_update` 快照，防新窗口拉到旧会话 `Ended`（U-H2）；
/// - `watch_only == Some(true)` 时通过 `?watch=1` 启动只读观看态（U-M7）。
#[tauri::command]
pub fn open_session_window(
    app: AppHandle,
    state: State<'_, AppState>,
    watch_only: Option<bool>,
) -> Result<(), String> {
    state.require_authenticated()?;
    ensure_session_window(&app, &state, watch_only.unwrap_or(false))
}

/// 读取窗口所在显示器的逻辑尺寸（物理像素除以 `scale_factor`）。
fn monitor_logical_size(win: &WebviewWindow) -> Option<(f64, f64)> {
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let size = monitor.size();
    let w = f64::from(size.width) / scale;
    let h = f64::from(size.height) / scale;
    if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
        Some((w, h))
    } else {
        None
    }
}

/// 读取窗口当前内部逻辑尺寸（物理像素除以 `scale_factor`）。
fn window_logical_inner_size(win: &WebviewWindow) -> Option<(f64, f64)> {
    let phys = win.inner_size().ok()?;
    let scale = win.scale_factor().unwrap_or(1.0);
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let w = f64::from(phys.width) / scale;
    let h = f64::from(phys.height) / scale;
    if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
        Some((w, h))
    } else {
        None
    }
}

/// 根据显示器逻辑分辨率计算主窗口启动最佳尺寸。
///
/// - 异常/非正数显示器尺寸安全回退到 `1000×660`；
/// - 2K/4K 大屏（`monitor_w >= 2200 && monitor_h >= 1200`）小幅提升至 `1120×720`；
/// - 标准笔记本 / 1080p 屏幕目标 `1000×660`（受限于显示器 `78%×78%`）；
/// - 紧凑屏幕（如 `1280×720` / `1366×768`）等比收窄至显示器 `82%×80%` 以内，
///   保底不低于 `820×540` 且绝不超过显示器本身尺寸。
#[allow(dead_code)]
pub(crate) fn compute_optimal_main_window_size(monitor_w: f64, monitor_h: f64) -> (f64, f64) {
    if !monitor_w.is_finite() || !monitor_h.is_finite() || monitor_w <= 0.0 || monitor_h <= 0.0 {
        return (MAIN_DEFAULT_W, MAIN_DEFAULT_H);
    }

    let (target_w, target_h, ratio_w, ratio_h) = if monitor_w >= 2200.0 && monitor_h >= 1200.0 {
        (MAIN_LARGE_W, MAIN_LARGE_H, 0.78, 0.78)
    } else if monitor_w < 1440.0 || monitor_h < 840.0 {
        (MAIN_DEFAULT_W, MAIN_DEFAULT_H, 0.82, 0.80)
    } else {
        (MAIN_DEFAULT_W, MAIN_DEFAULT_H, 0.78, 0.78)
    };

    let cap_w = (monitor_w * ratio_w).floor().max(1.0);
    let cap_h = (monitor_h * ratio_h).floor().max(1.0);
    let scale = (cap_w / target_w).min(cap_h / target_h).min(1.0);

    let max_mon_w = if monitor_w >= 1.0 {
        monitor_w.floor()
    } else {
        monitor_w
    };
    let max_mon_h = if monitor_h >= 1.0 {
        monitor_h.floor()
    } else {
        monitor_h
    };

    let w = (target_w * scale)
        .round()
        .min(cap_w)
        .max(MAIN_MIN_W)
        .min(max_mon_w);
    let h = (target_h * scale)
        .round()
        .min(cap_h)
        .max(MAIN_MIN_H)
        .min(max_mon_h);
    (w, h)
}

/// 启动时按主窗口所在显示器逻辑分辨率自适应调整主窗口大小并居中。
#[allow(dead_code)]
pub(crate) fn adapt_main_window_on_startup(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let (monitor_w, monitor_h) =
        monitor_logical_size(&win).unwrap_or((FALLBACK_MONITOR_W, FALLBACK_MONITOR_H));
    let (w, h) = compute_optimal_main_window_size(monitor_w, monitor_h);
    let _ = win.set_size(LogicalSize::new(w, h));
    let _ = win.center();
}

/// 计算 S1 会话窗口贴合远端视频分辨率的零黑边逻辑尺寸。
///
/// 保证窗口内部视频舞台 `(win_w, win_h - toolbar_h)` 与 `video_w / video_h` 宽高比偏差
/// 小于 `1.5 px`（在正常宽高比区间内实测 `<= 0.5 px`），同时满足最小窗口限制
/// `[640×440]` 与显示器安全工作区上限 `monitor_w * 0.88` × `monitor_h * 0.86`。
///
/// - `current_size == None`：首次进入远控自动贴合，舞台尺寸目标不超过
///   `min(video_w, monitor_w * 0.82)` 与 `min(video_h, monitor_h * 0.82 - toolbar_h)`；
/// - `current_size == Some((cur_w, cur_h))`：用户点击「一键消除黑边」，基于当前窗口
///   收窄存在黑边的一侧（若触发最小尺寸则改为等比放大另一侧），并等比收敛到显示器上限内。
#[allow(dead_code)]
pub(crate) fn compute_fit_session_window_size(
    video_w: u32,
    video_h: u32,
    monitor_w: f64,
    monitor_h: f64,
    current_size: Option<(f64, f64)>,
    toolbar_h: f64,
) -> (f64, f64) {
    let toolbar_h = if toolbar_h.is_finite() && toolbar_h >= 0.0 {
        toolbar_h
    } else {
        SESSION_TOOLBAR_HEIGHT
    };
    let (mon_w, mon_h) =
        if monitor_w.is_finite() && monitor_h.is_finite() && monitor_w > 0.0 && monitor_h > 0.0 {
            (monitor_w, monitor_h)
        } else {
            (FALLBACK_MONITOR_W, FALLBACK_MONITOR_H)
        };

    let max_w = (mon_w * 0.88).floor().max(SESSION_MIN_W.min(mon_w));
    let max_h = (mon_h * 0.86).floor().max(SESSION_MIN_H.min(mon_h));
    let min_w = SESSION_MIN_W.min(max_w);
    let min_h = SESSION_MIN_H.min(max_h);

    if video_w == 0 || video_h == 0 {
        return (
            SESSION_DEFAULT_W.clamp(min_w, max_w),
            SESSION_DEFAULT_H.clamp(min_h, max_h),
        );
    }

    let max_stage_w = max_w;
    let max_stage_h = (max_h - toolbar_h).max(1.0);
    let min_stage_h = (min_h - toolbar_h).max(1.0);
    let aspect = f64::from(video_w) / f64::from(video_h);

    let valid_current =
        current_size.filter(|(w, h)| w.is_finite() && h.is_finite() && *w > 0.0 && *h > 0.0);

    let (mut stage_w, mut stage_h) = match valid_current {
        None => {
            let init_max_w = f64::from(video_w).min(mon_w * 0.82).min(max_stage_w);
            let init_max_h = f64::from(video_h)
                .min((mon_h * 0.82 - toolbar_h).max(1.0))
                .min(max_stage_h);
            if init_max_w / init_max_h > aspect {
                (init_max_h * aspect, init_max_h)
            } else {
                (init_max_w, init_max_w / aspect)
            }
        }
        Some((cur_w, cur_h)) => {
            let cur_stage_w = cur_w.max(1.0);
            let cur_stage_h = (cur_h - toolbar_h).max(1.0);
            if cur_stage_w / cur_stage_h > aspect {
                // 左右黑边（pillarbox）：优先收窄宽度；若低于最小宽度则改为增高。
                let target_w = cur_stage_h * aspect;
                if target_w < min_w {
                    (min_w, min_w / aspect)
                } else {
                    (target_w, cur_stage_h)
                }
            } else {
                // 上下黑边（letterbox）：优先收窄高度；若低于最小高度则改为增宽。
                let target_h = cur_stage_w / aspect;
                if target_h + toolbar_h < min_h {
                    (min_stage_h * aspect, min_stage_h)
                } else {
                    (cur_stage_w, target_h)
                }
            }
        }
    };

    // 若低于最小边界且放大后仍在显示器上限内，则等比放大以满足最小窗口尺寸。
    if stage_w < min_w {
        let scale = min_w / stage_w;
        if stage_h * scale <= max_stage_h {
            stage_w = min_w;
            stage_h *= scale;
        }
    }
    if stage_h < min_stage_h {
        let scale = min_stage_h / stage_h;
        if stage_w * scale <= max_stage_w {
            stage_h = min_stage_h;
            stage_w *= scale;
        }
    }

    // 等比收敛到显示器工作区上限内。
    if stage_w > max_stage_w {
        let scale = max_stage_w / stage_w;
        stage_w *= scale;
        stage_h *= scale;
    }
    if stage_h > max_stage_h {
        let scale = max_stage_h / stage_h;
        stage_w *= scale;
        stage_h *= scale;
    }

    // 先对短边（约束边）取整，再由精确宽高比推导长边并取整，保证双轴误差 <= 0.5 px。
    if aspect >= 1.0 {
        let mut win_h = (stage_h + toolbar_h).round().clamp(min_h, max_h);
        let mut win_w = ((win_h - toolbar_h).max(1.0) * aspect).round();
        if win_w > max_w {
            win_h = ((max_w / aspect).floor() + toolbar_h).clamp(min_h, max_h);
            win_w = ((win_h - toolbar_h).max(1.0) * aspect).round().min(max_w);
        } else if win_w < min_w {
            win_h = ((min_w / aspect).ceil() + toolbar_h).clamp(min_h, max_h);
            win_w = ((win_h - toolbar_h).max(1.0) * aspect)
                .round()
                .clamp(min_w, max_w);
        }
        (win_w.clamp(min_w, max_w), win_h)
    } else {
        let mut win_w = stage_w.round().clamp(min_w, max_w);
        let mut win_h = (win_w / aspect + toolbar_h).round();
        if win_h > max_h {
            win_w = ((max_h - toolbar_h).max(1.0) * aspect)
                .floor()
                .clamp(min_w, max_w);
            win_h = (win_w / aspect + toolbar_h).round().min(max_h);
        } else if win_h < min_h {
            win_w = ((min_h - toolbar_h).max(1.0) * aspect)
                .ceil()
                .clamp(min_w, max_w);
            win_h = (win_w / aspect + toolbar_h).round().clamp(min_h, max_h);
        }
        (win_w, win_h.clamp(min_h, max_h))
    }
}

/// 将 `"session"` 窗口尺寸调整为与远端视频分辨率严格等比贴合（零黑边）。
///
/// - 全屏态直接返回 `Ok` 空操作，不与系统全屏争抢尺寸；
/// - `preserve_current_scale == false`（首次 `Negotiated` 自动贴合）完成后居中窗口；
/// - `preserve_current_scale == true`（一键消除黑边）基于当前窗口大小收窄黑边侧，不强制移动位置。
#[allow(dead_code)]
pub(crate) fn fit_session_window_to_video(
    app: &AppHandle,
    video_w: u32,
    video_h: u32,
    preserve_current_scale: bool,
) -> Result<(f64, f64), String> {
    let Some(win) = app.get_webview_window(ToolWindow::Session.label()) else {
        return Err("会话窗口未打开".to_string());
    };
    if win.is_fullscreen().unwrap_or(false) {
        return Ok(window_logical_inner_size(&win).unwrap_or((0.0, 0.0)));
    }

    let (mon_w, mon_h) =
        monitor_logical_size(&win).unwrap_or((FALLBACK_MONITOR_W, FALLBACK_MONITOR_H));
    let current_size = if preserve_current_scale {
        window_logical_inner_size(&win)
    } else {
        None
    };

    let (w, h) = compute_fit_session_window_size(
        video_w,
        video_h,
        mon_w,
        mon_h,
        current_size,
        SESSION_TOOLBAR_HEIGHT,
    );
    win.set_size(LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    if !preserve_current_scale {
        let _ = win.center();
    }
    Ok((w, h))
}

/// 格式化 S1 会话窗口标题（如 `"Studio Mac — 显示屏1"`；空名称回退 `"远程桌面"`）。
fn format_session_window_title(device_name: &str) -> String {
    let trimmed = device_name.trim();
    if trimmed.is_empty() {
        ToolWindow::Session.title().to_string()
    } else {
        format!("{trimmed} — 显示屏1")
    }
}

/// 更新 `"session"` 窗口标题为 `<device_name> — 显示屏1`（窗口不存在时静默忽略）。
#[allow(dead_code)]
pub(crate) fn set_session_window_title(app: &AppHandle, device_name: &str) {
    if let Some(win) = app.get_webview_window(ToolWindow::Session.label()) {
        let _ = win.set_title(&format_session_window_title(device_name));
    }
}

/// IPC 命令：将已打开的 S1 会话窗口恢复最小化并显示聚焦到前台。
#[allow(dead_code)]
#[tauri::command]
pub fn focus_session_window(app: AppHandle) -> Result<(), String> {
    app.state::<AppState>().require_authenticated()?;
    let Some(win) = app.get_webview_window(ToolWindow::Session.label()) else {
        return Err("会话窗口未打开".to_string());
    };
    let _ = win.unminimize();
    win.show().map_err(|e| e.to_string())?;
    win.set_focus().map_err(|e| e.to_string())?;
    Ok(())
}

/// 关闭所有已打开的 S1 远控与 W2–W4 工具窗口（登出、切换服务器、踢线或主窗口退出时调用）。
pub(crate) fn close_session_and_tool_windows(app: &AppHandle) {
    let app_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        for kind in [
            ToolWindow::Session,
            ToolWindow::Transfer,
            ToolWindow::Terminal,
            ToolWindow::PortMap,
        ] {
            if let Some(win) = app_main.get_webview_window(kind.label()) {
                let _ = win.destroy();
            }
        }
    });
}

/// IPC 命令：断开当前远控会话、销毁 S1 窗口并将焦点交回主窗口（解决 WKWebView `window.close()` 不生效问题）。
#[allow(dead_code)]
#[tauri::command]
pub fn close_session_window(lock_remote: Option<bool>, app: AppHandle) -> Result<(), String> {
    app.state::<AppState>().require_authenticated()?;
    crate::commands::disconnect_active_session_with_lock(&app, lock_remote);
    if let Some(win) = app.get_webview_window(ToolWindow::Session.label()) {
        let _ = win.destroy();
    }
    if let Some(main_win) = app.get_webview_window("main") {
        let _ = main_win.unminimize();
        let _ = main_win.show();
        let _ = main_win.set_focus();
    }
    Ok(())
}

/// IPC 命令：一键消除 S1 会话窗口黑边（按当前已协商视频分辨率贴合窗口比例）。
#[allow(dead_code)]
#[tauri::command]
pub fn fit_session_window(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.require_authenticated()?;
    let video_size = {
        let slot = state.session.lock().unwrap_or_else(|e| e.into_inner());
        slot.as_ref().and_then(|active| active.video_size)
    };
    let Some((vw, vh)) = video_size else {
        return Err("当前会话尚未协商画面分辨率".to_string());
    };
    fit_session_window_to_video(&app, vw, vh, true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        compute_fit_session_window_size, compute_optimal_main_window_size, ToolWindow,
        SESSION_TOOLBAR_HEIGHT,
    };

    fn assert_stage_aspect_matches(win_w: f64, win_h: f64, video_w: u32, video_h: u32) {
        let stage_w = win_w;
        let stage_h = win_h - SESSION_TOOLBAR_HEIGHT;
        assert!(stage_h > 0.0, "stage_h 应大于 0: win=({win_w}, {win_h})");
        let aspect = f64::from(video_w) / f64::from(video_h);
        let err_w = (stage_w - stage_h * aspect).abs();
        let err_h = (stage_h - stage_w / aspect).abs();
        assert!(
            err_w < 1.5 && err_h < 1.5,
            "黑边误差过大: win=({win_w}, {win_h}), stage=({stage_w}, {stage_h}), video={video_w}x{video_h}, err_w={err_w:.3}, err_h={err_h:.3}"
        );
    }

    #[test]
    fn parse_accepts_all_known_kinds() {
        for (raw, expected) in [
            ("settings", ToolWindow::Settings),
            ("transfer", ToolWindow::Transfer),
            ("terminal", ToolWindow::Terminal),
            ("portmap", ToolWindow::PortMap),
            ("session", ToolWindow::Session),
        ] {
            assert_eq!(ToolWindow::parse(raw), Some(expected), "kind={raw}");
        }
    }

    #[test]
    fn parse_rejects_unknown_kind() {
        assert_eq!(ToolWindow::parse(""), None);
        assert_eq!(ToolWindow::parse("settings "), None);
        assert_eq!(ToolWindow::parse("unknown"), None);
    }

    #[test]
    fn session_window_title_and_format_match_spec() {
        assert_eq!(ToolWindow::Session.title(), "远程桌面");
        assert_eq!(
            super::format_session_window_title("Studio Mac"),
            "Studio Mac — 显示屏1"
        );
        assert_eq!(
            super::format_session_window_title("  MacBook Pro  "),
            "MacBook Pro — 显示屏1"
        );
        assert_eq!(super::format_session_window_title("   "), "远程桌面");
    }

    #[test]
    fn window_labels_and_urls_are_unique() {
        let all = [
            ToolWindow::Settings,
            ToolWindow::Transfer,
            ToolWindow::Terminal,
            ToolWindow::PortMap,
            ToolWindow::Session,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i].label(), all[j].label(), "label 重复");
                assert_ne!(all[i].url(), all[j].url(), "url 重复");
            }
        }
    }

    #[test]
    fn settings_window_is_fixed_size() {
        let (w, h, min_w, min_h, resizable) = ToolWindow::Settings.geometry();
        assert!(!resizable, "设置窗口不可调整大小");
        assert_eq!(
            (w, h),
            (min_w, min_h),
            "固定尺寸窗口的默认尺寸应等于最小尺寸"
        );
        assert_eq!((w, h), (660.0, 640.0));
    }

    #[test]
    fn resizable_windows_have_min_not_exceeding_default() {
        for kind in [
            ToolWindow::Transfer,
            ToolWindow::Terminal,
            ToolWindow::PortMap,
            ToolWindow::Session,
        ] {
            let (w, h, min_w, min_h, resizable) = kind.geometry();
            assert!(resizable, "{kind:?} 应可调整大小");
            assert!(
                min_w <= w && min_h <= h,
                "{kind:?} 最小尺寸不得超过默认尺寸"
            );
        }
    }

    #[test]
    fn session_window_url_reflects_watch_only_flag() {
        assert_eq!(super::session_window_url(false), "session.html");
        assert_eq!(super::session_window_url(true), "session.html?watch=1");
    }

    #[test]
    fn session_window_enables_transparency_for_metal_underlay() {
        assert!(ToolWindow::Session.is_transparent());
        for kind in [
            ToolWindow::Settings,
            ToolWindow::Transfer,
            ToolWindow::Terminal,
            ToolWindow::PortMap,
        ] {
            assert!(!kind.is_transparent(), "{kind:?} 不应开启透明窗口");
        }
    }

    #[test]
    fn session_window_geometry_matches_adaptive_defaults() {
        let (w, h, min_w, min_h, resizable) = ToolWindow::Session.geometry();
        assert_eq!(
            (w, h, min_w, min_h, resizable),
            (1160.0, 760.0, 640.0, 440.0, true)
        );
    }

    #[test]
    fn optimal_main_window_size_adapts_across_monitors() {
        // 标准笔记本 / 1080p：目标 1000×660 且不超过 78%
        for (mw, mh) in [(1440.0, 900.0), (1512.0, 982.0), (1920.0, 1080.0)] {
            let (w, h) = compute_optimal_main_window_size(mw, mh);
            assert_eq!((w, h), (1000.0, 660.0), "monitor={mw}x{mh}");
            assert!(w <= mw * 0.78 && h <= mh * 0.78);
        }

        // 2K / 4K 大屏：小幅提升至 1120×720，克制不过大
        for (mw, mh) in [(2560.0, 1440.0), (3840.0, 2160.0)] {
            let (w, h) = compute_optimal_main_window_size(mw, mh);
            assert_eq!((w, h), (1120.0, 720.0), "monitor={mw}x{mh}");
        }

        // 紧凑屏（1280×720 / 1366×768）：<= 82%×80%，保底 >= 820×540
        for (mw, mh) in [(1280.0, 720.0), (1366.0, 768.0)] {
            let (w, h) = compute_optimal_main_window_size(mw, mh);
            assert!(w >= 820.0 && w <= mw * 0.82, "monitor={mw}x{mh}, w={w}");
            assert!(h >= 540.0 && h <= mh * 0.80, "monitor={mw}x{mh}, h={h}");
        }

        // 超小显示器：绝不超过显示器自身尺寸
        let (w, h) = compute_optimal_main_window_size(700.0, 500.0);
        assert!(w <= 700.0 && h <= 500.0, "tiny monitor got ({w}, {h})");

        // 异常值安全回退 1000×660
        for (mw, mh) in [
            (0.0, 900.0),
            (1512.0, -10.0),
            (f64::NAN, 982.0),
            (f64::INFINITY, 982.0),
        ] {
            assert_eq!(
                compute_optimal_main_window_size(mw, mh),
                (1000.0, 660.0),
                "invalid monitor={mw}x{mh}"
            );
        }
    }

    #[test]
    fn fit_session_initial_mbp14_exact_zero_black_bars() {
        let (w, h) =
            compute_fit_session_window_size(1512, 982, 1512.0, 982.0, None, SESSION_TOOLBAR_HEIGHT);
        assert_eq!((w, h), (1166.0, 805.0));
        assert_stage_aspect_matches(w, h, 1512, 982);
    }

    #[test]
    fn fit_session_initial_across_aspects_and_monitors() {
        let cases = [
            // 16:9 跨不同显示器
            (1920, 1080, 1512.0, 982.0),
            (1920, 1080, 1920.0, 1080.0),
            (1920, 1080, 1280.0, 720.0),
            // 21:9 超宽屏
            (2560, 1080, 1512.0, 982.0),
            (2560, 1080, 1280.0, 720.0),
            (2560, 1080, 2560.0, 1440.0),
            // 4K 视频在 4K 与笔记本屏幕
            (3840, 2160, 3840.0, 2160.0),
            (3840, 2160, 1512.0, 982.0),
            // 小分辨率视频（需放大以满足 min_h=440）
            (640, 360, 1512.0, 982.0),
        ];

        for (vw, vh, mw, mh) in cases {
            let (w, h) =
                compute_fit_session_window_size(vw, vh, mw, mh, None, SESSION_TOOLBAR_HEIGHT);
            assert!(
                w >= 640.0 && w <= mw * 0.88,
                "case={vw}x{vh} on {mw}x{mh}, w={w}"
            );
            assert!(
                h >= 440.0 && h <= mh * 0.86,
                "case={vw}x{vh} on {mw}x{mh}, h={h}"
            );
            assert_stage_aspect_matches(w, h, vw, vh);

            // 幂等性：再次执行「一键消除黑边」不应漂移
            let (w2, h2) = compute_fit_session_window_size(
                vw,
                vh,
                mw,
                mh,
                Some((w, h)),
                SESSION_TOOLBAR_HEIGHT,
            );
            assert_eq!(
                (w2, h2),
                (w, h),
                "repeat fit drifted for {vw}x{vh} on {mw}x{mh}"
            );
        }
    }

    #[test]
    fn fit_session_eliminates_pillarbox_black_bars() {
        // 1512×982 视频放在过宽的 1280×748 窗口（stage=1280×700，左右有黑边）
        let (w, h) = compute_fit_session_window_size(
            1512,
            982,
            1512.0,
            982.0,
            Some((1280.0, 748.0)),
            SESSION_TOOLBAR_HEIGHT,
        );
        // 应保留高度 748，收窄宽度至 ~1078
        assert_eq!(h, 748.0);
        assert!((w - 1078.0).abs() <= 1.0, "got w={w}");
        assert_stage_aspect_matches(w, h, 1512, 982);
    }

    #[test]
    fn fit_session_pillarbox_below_min_width_grows_height() {
        // 4:3 视频在矮宽窗口 900×448（stage_h=400，400*4/3=533 < min_w=640）
        let (w, h) = compute_fit_session_window_size(
            1600,
            1200,
            1920.0,
            1080.0,
            Some((900.0, 448.0)),
            SESSION_TOOLBAR_HEIGHT,
        );
        assert!(w >= 640.0, "w={w} 低于最小宽度");
        assert!(h > 448.0 && h <= 1080.0 * 0.86, "h={h} 应增大以保最小宽度");
        assert_stage_aspect_matches(w, h, 1600, 1200);
    }

    #[test]
    fn fit_session_eliminates_letterbox_black_bars() {
        // 16:9 视频放在过高的 960×780 窗口（stage=960×732，上下有黑边）
        let (w, h) = compute_fit_session_window_size(
            1920,
            1080,
            1920.0,
            1080.0,
            Some((960.0, 780.0)),
            SESSION_TOOLBAR_HEIGHT,
        );
        // 应保持宽 ~960，收窄高度至 960/(16/9)+48 = 588
        assert!((w - 960.0).abs() <= 1.0, "got w={w}");
        assert_eq!(h, 588.0);
        assert_stage_aspect_matches(w, h, 1920, 1080);
    }

    #[test]
    fn fit_session_letterbox_below_min_height_grows_width() {
        // 21:9 视频在窄高窗口 700×640（700/2.37+48 = 343 < min_h=440）
        let (w, h) = compute_fit_session_window_size(
            2560,
            1080,
            1920.0,
            1080.0,
            Some((700.0, 640.0)),
            SESSION_TOOLBAR_HEIGHT,
        );
        assert!(h >= 440.0, "h={h} 低于最小高度");
        assert!(w > 700.0 && w <= 1920.0 * 0.88, "w={w} 应增大以保最小高度");
        assert_stage_aspect_matches(w, h, 2560, 1080);
    }

    #[test]
    fn fit_session_reclamps_oversized_current_window_preserving_aspect() {
        let (w, h) = compute_fit_session_window_size(
            1920,
            1080,
            1280.0,
            720.0,
            Some((2400.0, 1600.0)),
            SESSION_TOOLBAR_HEIGHT,
        );
        assert!(w <= 1280.0 * 0.88 && h <= 720.0 * 0.86, "({w}, {h})");
        assert_stage_aspect_matches(w, h, 1920, 1080);
    }

    #[test]
    fn fit_session_handles_invalid_inputs_safely() {
        let (w, h) =
            compute_fit_session_window_size(0, 0, 1512.0, 982.0, None, SESSION_TOOLBAR_HEIGHT);
        assert!(w.is_finite() && h.is_finite() && w >= 640.0 && h >= 440.0);

        let (w2, h2) = compute_fit_session_window_size(
            1920,
            1080,
            f64::NAN,
            -1.0,
            Some((f64::INFINITY, 0.0)),
            f64::NAN,
        );
        assert!(w2.is_finite() && h2.is_finite() && w2 >= 640.0 && h2 >= 440.0);
        assert_stage_aspect_matches(w2, h2, 1920, 1080);
    }
}
