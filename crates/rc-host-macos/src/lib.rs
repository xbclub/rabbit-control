//! 被控端驻留 UI（M2-7 / T22）：macOS 菜单栏托盘与连接审批弹窗。
//!
//! # 分层
//! - [`state`]：托盘菜单文案与弹窗三要素纯函数（无 AppKit，全平台编译可测）；
//! - [`bridge`]：会话线程 ⇄ 主线程的审批桥（纯标准库同步原语，全平台编译可测）；
//! - `appkit`（仅 `cfg(target_os = "macos")`）：`NSStatusItem` / `NSMenu` / `NSAlert`
//!   与主线程手泵事件循环，本 crate 全部 `unsafe` 集中于此。
//!
//! # 主线程契约
//! `NSStatusItem` 与 `NSAlert` 属于 `MainThreadOnly` 类型，**必须在主线程构造
//! [`TrayHost::new`] 并进入 [`TrayHost::run`]**（内部经 `MainThreadMarker` 断言）。
//! 会话线程只能持有 [`TrayApprover`]（`Send`），不得跨线程触碰任何 AppKit 对象。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
mod appkit;
pub mod bridge;
pub mod state;

use std::fmt;
use std::net::SocketAddr;
#[cfg(target_os = "macos")]
use std::sync::mpsc;

use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};

#[cfg(target_os = "macos")]
use crate::bridge::UiSide;
use crate::bridge::{ApprovalBridge, SessionSide};

#[cfg(all(target_os = "macos", feature = "tray-bench"))]
pub use appkit::{inspect_approval_alert, AlertSnapshot};

/// 托盘初始化配置。
#[derive(Debug, Clone)]
pub struct TrayConfig {
    /// 被控端监听地址（用于状态行展示「RabbitControl · 等待连接 (\<addr\>)」）。
    pub listen: SocketAddr,
}

/// 托盘可见状态（纯数据，驱动 [`state::menu_labels`] 与图标状态标记）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    /// 等待连接（含监听地址）。
    WaitingForConnection(SocketAddr),
    /// 会话中（含对端地址；菜单栏图标附「●」标记区分）。
    InSession(SocketAddr),
}

/// 托盘向宿主进程抛出的用户动作（在主线程 `on_event` 回调中交付）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 用户点击了「断开当前会话」（无会话时菜单项禁用，不会触发）。
    DisconnectRequested,
    /// 用户点击了「退出」（事件循环随即结束，宿主应按停机红线收尾）。
    QuitRequested,
}

/// `rc-host-macos` 的错误类型。
#[derive(Debug)]
pub enum HostUiError {
    /// 无 GUI 登录会话（如 SSH 或无 Aqua 的后台环境）：`NSStatusItem` 创建失败，
    /// 或在非 macOS 平台上尝试启动 macOS 托盘。
    NoGuiSession,
    /// AppKit 调用或主线程契约失败（附可读上下文）。
    AppKit(String),
}

impl fmt::Display for HostUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGuiSession => {
                write!(f, "无可用的 macOS GUI 会话，无法创建菜单栏托盘图标")
            }
            Self::AppKit(msg) => write!(f, "AppKit 错误：{msg}"),
        }
    }
}

impl std::error::Error for HostUiError {}

/// 会话线程侧审批句柄（[`TrayHost::approver`] 的返回类型）。
///
/// 实现 [`ConnectionApprover`]，内部仅持有 [`SessionSide`]（`Send`），
/// 三方法全部非阻塞立即返回，不含任何 AppKit 对象。
#[derive(Debug, Clone)]
pub struct TrayApprover {
    session_side: SessionSide,
}

/// 跨线程托盘状态更新句柄（[`TrayHost::state_handle`] 的返回类型）。
///
/// 实现 `Send + Clone`，内部仅持有标准库通道发送端，不含任何 AppKit 对象（U4）。
/// 会话线程调用 [`TrayStateHandle::set_state`] 将状态更新投递给主线程
/// [`TrayHost::run`] 循环渲染。
#[derive(Debug, Clone)]
pub struct TrayStateHandle {
    #[cfg(target_os = "macos")]
    state_tx: mpsc::Sender<TrayState>,
}

impl TrayStateHandle {
    /// 向主线程投递新的托盘可见状态（非阻塞、幂等）。
    pub fn set_state(&self, state: TrayState) {
        #[cfg(target_os = "macos")]
        {
            let _ = self.state_tx.send(state);
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = state;
        }
    }
}

impl ConnectionApprover for TrayApprover {
    fn submit(&mut self, req: &ApprovalRequest) {
        self.session_side.submit(req);
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        self.session_side.poll_verdict()
    }

    fn cancel(&mut self) {
        self.session_side.cancel();
    }
}

/// macOS 菜单栏托盘宿主。
///
/// 典型生命周期：主线程 [`TrayHost::new`] → [`TrayHost::approver`] 交给会话线程 →
/// spawn 会话线程 → 主线程阻塞在 [`TrayHost::run`] 直到用户点击「退出」。
pub struct TrayHost {
    bridge: ApprovalBridge,
    #[cfg(target_os = "macos")]
    ui_side: UiSide,
    #[cfg(target_os = "macos")]
    state_tx: mpsc::Sender<TrayState>,
    #[cfg(target_os = "macos")]
    state_rx: mpsc::Receiver<TrayState>,
    #[cfg(target_os = "macos")]
    inner: appkit::AppKitTray,
}

impl TrayHost {
    /// 初始化 AppKit（`ActivationPolicy::Accessory`）并创建菜单栏状态项与三项菜单。
    ///
    /// **主线程限定**：内部断言 `MainThreadMarker`。无 GUI 会话时返回
    /// [`HostUiError::NoGuiSession`]；非 macOS 平台一律返回 [`HostUiError::NoGuiSession`]。
    pub fn new(cfg: TrayConfig) -> Result<Self, HostUiError> {
        #[cfg(target_os = "macos")]
        {
            let inner = appkit::AppKitTray::new(&cfg)?;
            let bridge = ApprovalBridge::new();
            let ui_side = bridge.ui_side();
            let (state_tx, state_rx) = mpsc::channel();
            Ok(Self {
                bridge,
                ui_side,
                state_tx,
                state_rx,
                inner,
            })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = cfg;
            Err(HostUiError::NoGuiSession)
        }
    }

    /// 取得会话线程侧审批句柄（实现 [`ConnectionApprover`]，`Send`）。
    ///
    /// 应在 spawn 会话线程之前调用，并装入 `HostSessionConfig::approver`。
    pub fn approver(&self) -> TrayApprover {
        TrayApprover {
            session_side: self.bridge.clone().session_side(),
        }
    }

    /// 取得跨线程托盘状态更新句柄（`Send + Clone`），供会话线程切换
    /// `WaitingForConnection` / `InSession` 状态。
    pub fn state_handle(&self) -> TrayStateHandle {
        TrayStateHandle {
            #[cfg(target_os = "macos")]
            state_tx: self.state_tx.clone(),
        }
    }

    /// 更新托盘可见状态（幂等）。
    ///
    /// 内部经 channel 入队并在主线程更新菜单项文案与图标标记。
    pub fn set_state(&self, state: TrayState) {
        #[cfg(target_os = "macos")]
        {
            let _ = self.state_tx.send(state.clone());
            // `TrayHost` 为 `!Send`（持有 `MainThreadMarker`），调用者必在主线程；
            // 同步刷新可保证未进入 `run()` 的基准测试（如 `tray_bench`）也即时生效。
            self.inner.apply_state(&state);
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = state;
        }
    }

    /// 进入主线程手泵事件循环，阻塞直到用户点击「退出」。
    ///
    /// 循环内依次排空 AppKit 事件、应用待处理状态、弹出在途审批 `NSAlert`
    /// （模态期间由 AppKit 模态循环接管，见 T22 已知坑 4）并回调 `on_event`。
    pub fn run(&mut self, on_event: impl FnMut(TrayEvent)) -> Result<(), HostUiError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.run(&self.state_rx, &mut self.ui_side, on_event)
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = on_event;
            Err(HostUiError::NoGuiSession)
        }
    }
}
