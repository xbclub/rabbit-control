//! macOS AppKit 托盘与审批弹窗实现（本 crate 全部 `unsafe` 集中地）。
//!
//! 封装 `NSApplication`（`ActivationPolicy::Accessory`）、`NSStatusBar` /
//! `NSStatusItem`、`NSMenu` / `NSMenuItem`、`NSAlert` 以及主线程手泵事件循环。
//! 所有 AppKit 对象均经 [`MainThreadMarker`] 静态/运行期绑定到主线程（U4），
//! 跨线程只通过标准库 channel 与 [`UiSide`] 交换纯数据。

// 本模块是 crate 内唯一允许 unsafe 的边界（见 crate 级文档）。
#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::sync::mpsc;
use std::time::Duration;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSEventMask, NSImage, NSMenu, NSMenuItem, NSModalResponse, NSModalResponseAbort,
    NSModalResponseContinue, NSModalResponseStop, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSDefaultRunLoopMode, NSObject, NSSize, NSString};
use rc_session::auth::{ApprovalRequest, ApprovalVerdict};

use crate::bridge::UiSide;
use crate::state::{alert_texts, menu_labels};
use crate::{HostUiError, TrayConfig, TrayEvent, TrayState};

/// 内嵌的 32×32 单色模板图标 PNG（`isTemplate = true` 自适应深浅菜单栏）。
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-template.png");

/// 空转退避间隔：手泵事件循环本轮无任何事件/状态/审批时休眠，避免空转吃满 CPU。
const IDLE_SLEEP: Duration = Duration::from_millis(10);

/// `MenuTarget` 的实例变量：仅在主线程向事件循环回投 [`TrayEvent`]。
struct MenuTargetIvars {
    tx: mpsc::Sender<TrayEvent>,
}

define_class!(
    // SAFETY:
    // - 父类 `NSObject` 无额外子类化约束；
    // - `#[thread_kind = MainThreadOnly]` 将该 ObjC 类的分配与方法调用限定在主线程；
    // - `MenuTargetIvars` 仅含 `mpsc::Sender<TrayEvent>`，无跨线程共享可变状态或自定义 Drop 副作用。
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(onDisconnect:))]
        fn on_disconnect(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(TrayEvent::DisconnectRequested);
        }

        #[unsafe(method(onQuit:))]
        fn on_quit(&self, _sender: Option<&AnyObject>) {
            let _ = self.ivars().tx.send(TrayEvent::QuitRequested);
            // M3：若当前正处于审批模态会话中，立即终止模态以便事件循环马上退出。
            let app = NSApplication::sharedApplication(self.mtm());
            if app.modalWindow().is_some() {
                app.stopModal();
            }
        }
    }
);

impl MenuTarget {
    /// 在主线程构造菜单回调 target 对象。
    fn new(mtm: MainThreadMarker, tx: mpsc::Sender<TrayEvent>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuTargetIvars { tx });
        // SAFETY: `this` 是刚完成 `alloc` 且已写入 `ivars` 的 `MenuTarget`，
        // 向父类 `NSObject` 发送 `init` 是标准的 ObjC 指定初始化器调用，返回 +1 强引用。
        unsafe { msg_send![super(this), init] }
    }
}

/// 主线程限定的 AppKit 托盘对象集合（`!Send` + `!Sync`）。
pub(crate) struct AppKitTray {
    mtm: MainThreadMarker,
    app: Retained<NSApplication>,
    status_item: Retained<NSStatusItem>,
    button: Retained<NSStatusBarButton>,
    _menu: Retained<NSMenu>,
    status_row: Retained<NSMenuItem>,
    disconnect_item: Retained<NSMenuItem>,
    _quit_item: Retained<NSMenuItem>,
    _target: Retained<MenuTarget>,
    event_rx: mpsc::Receiver<TrayEvent>,
}

impl AppKitTray {
    /// 在主线程初始化 `NSApplication`（Accessory 策略）、`NSStatusItem` 与三项菜单。
    ///
    /// 若不在主线程返回 [`HostUiError::AppKit`]；若无 GUI 会话（`statusItem.button`
    /// 为空）返回 [`HostUiError::NoGuiSession`]（已知坑 7，绝不静默降级）。
    pub(crate) fn new(cfg: &TrayConfig) -> Result<Self, HostUiError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            HostUiError::AppKit("TrayHost::new 必须在主线程调用（MainThreadOnly）".to_string())
        })?;

        let app = NSApplication::sharedApplication(mtm);
        // 已知坑 2：必须设为 Accessory，不占 Dock 图标、不抢夺前台焦点。
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let status_bar = NSStatusBar::systemStatusBar();
        let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
        let Some(button) = status_item.button(mtm) else {
            status_bar.removeStatusItem(&status_item);
            return Err(HostUiError::NoGuiSession);
        };

        // 加载内嵌 32×32 PNG，缩放到 16×16 pt 并标为模板图（自适应深浅菜单栏）。
        let icon_data = NSData::with_bytes(TRAY_ICON_PNG);
        let image = NSImage::initWithData(NSImage::alloc(), &icon_data)
            .ok_or_else(|| HostUiError::AppKit("无法从内嵌 PNG 解码托盘图标".to_string()))?;
        image.setSize(NSSize::new(16.0, 16.0));
        image.setTemplate(true);
        button.setImage(Some(&image));
        button.setTitle(&NSString::from_str(""));

        let (event_tx, event_rx) = mpsc::channel();
        let target = MenuTarget::new(mtm, event_tx);

        let initial_state = TrayState::WaitingForConnection(cfg.listen);
        let labels = menu_labels(&initial_state);

        let menu = NSMenu::new(mtm);
        // 必须关闭 autoenablesItems，否则 AppKit 会忽略手动的 setEnabled(false)。
        menu.setAutoenablesItems(false);

        let empty_key = NSString::from_str("");

        // SAFETY: 在主线程调用 `NSMenuItem` 指定初始化器，`action` 为 `None`，无 target 悬垂风险。
        let status_row = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&labels.status_line),
                None,
                &empty_key,
            )
        };
        status_row.setEnabled(false);

        // SAFETY: `sel!(onDisconnect:)` 签名与 `MenuTarget::on_disconnect` 严格匹配，
        // 且 `target` 存活于 `AppKitTray` 整个生命周期（与 `menu` 同寿）。
        let disconnect_item = unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&labels.disconnect_title),
                Some(sel!(onDisconnect:)),
                &empty_key,
            );
            let target_obj: &AnyObject = &target;
            item.setTarget(Some(target_obj));
            item
        };
        disconnect_item.setEnabled(labels.disconnect_enabled);

        // SAFETY: `sel!(onQuit:)` 签名与 `MenuTarget::on_quit` 严格匹配，
        // 且 `target` 存活于 `AppKitTray` 整个生命周期（与 `menu` 同寿）。
        let quit_item = unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&labels.quit_title),
                Some(sel!(onQuit:)),
                &empty_key,
            );
            let target_obj: &AnyObject = &target;
            item.setTarget(Some(target_obj));
            item
        };
        quit_item.setEnabled(true);

        menu.addItem(&status_row);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&disconnect_item);
        menu.addItem(&quit_item);
        status_item.setMenu(Some(&menu));

        Ok(Self {
            mtm,
            app,
            status_item,
            button,
            _menu: menu,
            status_row,
            disconnect_item,
            _quit_item: quit_item,
            _target: target,
            event_rx,
        })
    }

    /// 把新的 [`TrayState`] 应用到状态行文案、「断开当前会话」可用性与菜单栏图标标记。
    pub(crate) fn apply_state(&self, state: &TrayState) {
        let labels = menu_labels(state);
        self.status_row
            .setTitle(&NSString::from_str(&labels.status_line));
        self.disconnect_item
            .setTitle(&NSString::from_str(&labels.disconnect_title));
        self.disconnect_item.setEnabled(labels.disconnect_enabled);

        match state {
            TrayState::WaitingForConnection(_) => {
                self.button.setTitle(&NSString::from_str(""));
            }
            TrayState::InSession(_) => {
                // 会话中在图标旁附「●」标记便于机主一眼识别正在被控。
                self.button.setTitle(&NSString::from_str("●"));
            }
        }
    }

    /// 主线程手泵事件循环：排空 AppKit 事件 → 优先派发托盘菜单事件 →
    /// 应用状态更新 → 以非阻塞模态会话弹出审批框，每轮包裹在 `autoreleasepool` 中，
    /// 空转时 10ms 退避，收到 [`TrayEvent::QuitRequested`] 后退出。
    pub(crate) fn run(
        &mut self,
        state_rx: &mpsc::Receiver<TrayState>,
        ui_side: &mut UiSide,
        mut on_event: impl FnMut(TrayEvent),
    ) -> Result<(), HostUiError> {
        // SAFETY: `NSDefaultRunLoopMode` 是 Foundation 导出的静态不可变常量，只读借用安全。
        let default_mode = unsafe { NSDefaultRunLoopMode };

        loop {
            let mut should_quit = false;
            // M2：每拍工作包裹在 autoreleasepool 内，防止长跑中 ObjC autorelease 对象累积泄漏。
            let had_work = autoreleasepool(|_| {
                let mut had_work = false;

                // 1. 排空当前所有待处理的 AppKit 事件（非阻塞，untilDate = None）。
                while let Some(event) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    NSEventMask::Any,
                    None,
                    default_mode,
                    true,
                ) {
                    self.app.sendEvent(&event);
                    had_work = true;
                }

                // 2. M3：在 `take_request()` 之前先排空托盘菜单事件；收到「退出」直接结束循环，
                //    不再为同拍到达的审批请求弹窗。
                let (ev_worked, quit) = drain_tray_events(&self.event_rx, &mut on_event);
                had_work |= ev_worked;
                if quit {
                    should_quit = true;
                    return had_work;
                }

                // 3. 应用会话线程发来的最新托盘状态（合并中间态，只渲染最终态）。
                let mut latest_state = None;
                while let Ok(state) = state_rx.try_recv() {
                    latest_state = Some(state);
                    had_work = true;
                }
                if let Some(state) = latest_state {
                    self.apply_state(&state);
                }

                // 4. 清理在未弹窗阶段到达的过期撤销标记。
                if ui_side.take_cancel() {
                    had_work = true;
                }

                // 5. M3：若有在途审批请求，以非阻塞模态会话驱动 NSAlert，
                //    每 10ms 轮询 `take_cancel()` 与 `event_rx`，客户端断开/超时/点退出立即撤窗。
                if let Some(req) = ui_side.take_request() {
                    had_work = true;
                    if self.run_approval_modal(&req, ui_side, &mut on_event) {
                        should_quit = true;
                        return had_work;
                    }
                }

                had_work
            });

            if should_quit {
                return Ok(());
            }

            if !had_work {
                std::thread::sleep(IDLE_SLEEP);
            }
        }
    }

    /// 以非阻塞 AppKit 模态会话驱动审批 `NSAlert`（M3）。
    ///
    /// 返回 `true` 表示弹窗期间收到了 [`TrayEvent::QuitRequested`]。
    fn run_approval_modal(
        &mut self,
        req: &ApprovalRequest,
        ui_side: &mut UiSide,
        on_event: &mut impl FnMut(TrayEvent),
    ) -> bool {
        // `alert` 必须存活到 `endModalSession` 与 `orderOut` 之后，
        // 因为按钮的 ObjC target 指向 `alert` 本身。
        let alert = build_approval_alert(self.mtm, req);
        alert.layout();
        let window = alert.window();
        self.app.activate();
        let session = self.app.beginModalSessionForWindow(&window);

        let mut quit_requested = false;
        let mut verdict: Option<ApprovalVerdict> = None;

        loop {
            let done = autoreleasepool(|_| {
                match poll_modal_interrupt(ui_side, &self.event_rx, on_event) {
                    ModalInterrupt::Continue => {}
                    ModalInterrupt::Cancelled => {
                        self.app.stopModal();
                        return true;
                    }
                    ModalInterrupt::QuitRequested => {
                        quit_requested = true;
                        self.app.stopModal();
                        return true;
                    }
                }

                // SAFETY: `session` 由配对的 `beginModalSessionForWindow` 创建且尚未 `endModalSession`。
                let response = unsafe { self.app.runModalSession(session) };

                // 本拍 `runModalSession` 可能刚派发了菜单 `onQuit:`，先排空事件通道。
                let (_, quit) = drain_tray_events(&self.event_rx, on_event);
                if quit {
                    quit_requested = true;
                    return true;
                }

                match classify_modal_response(response) {
                    ModalStep::Continue => false,
                    ModalStep::Aborted => true,
                    ModalStep::Verdict(v) => {
                        verdict = Some(v);
                        true
                    }
                }
            });

            if done {
                break;
            }
            std::thread::sleep(IDLE_SLEEP);
        }

        // SAFETY: `session` 由 `beginModalSessionForWindow` 创建，此处唯一一次配对结束并释放。
        unsafe { self.app.endModalSession(session) };
        window.orderOut(None);

        if let Some(v) = verdict {
            ui_side.resolve(v);
        }
        quit_requested
    }
}

/// 模态等待期间每拍轮询到的中断信号（纯逻辑，供单元测试覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalInterrupt {
    /// 无中断，继续泵模态会话。
    Continue,
    /// 会话侧已撤销该审批请求（对端断开或审批超时）。
    Cancelled,
    /// 用户触发了「退出」。
    QuitRequested,
}

/// 单拍 `runModalSession` 返回值的分类结果（纯逻辑，供单元测试覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalStep {
    /// 模态会话仍在进行中（`NSModalResponseContinue`）。
    Continue,
    /// 模态会话被程序终止/取消（`NSModalResponseStop` / `NSModalResponseAbort`）。
    Aborted,
    /// 用户点击按钮产生终局裁决。
    Verdict(ApprovalVerdict),
}

/// 排空托盘菜单事件通道并回调 `on_event`；返回 `(是否处理过事件, 是否收到 QuitRequested)`。
fn drain_tray_events(
    event_rx: &mpsc::Receiver<TrayEvent>,
    on_event: &mut impl FnMut(TrayEvent),
) -> (bool, bool) {
    let mut had_work = false;
    while let Ok(ev) = event_rx.try_recv() {
        had_work = true;
        let is_quit = ev == TrayEvent::QuitRequested;
        on_event(ev);
        if is_quit {
            return (had_work, true);
        }
    }
    (had_work, false)
}

/// 在审批模态会话每拍中轮询取消标记与菜单事件。
fn poll_modal_interrupt(
    ui_side: &mut UiSide,
    event_rx: &mpsc::Receiver<TrayEvent>,
    on_event: &mut impl FnMut(TrayEvent),
) -> ModalInterrupt {
    let (_, quit) = drain_tray_events(event_rx, on_event);
    if quit {
        return ModalInterrupt::QuitRequested;
    }
    if ui_side.take_cancel() {
        return ModalInterrupt::Cancelled;
    }
    ModalInterrupt::Continue
}

/// 将 `NSApplication::runModalSession` 的返回值分类为继续/中止/裁决。
fn classify_modal_response(response: NSModalResponse) -> ModalStep {
    if response == NSModalResponseContinue {
        ModalStep::Continue
    } else if response == NSModalResponseStop || response == NSModalResponseAbort {
        ModalStep::Aborted
    } else if response == NSAlertFirstButtonReturn {
        ModalStep::Verdict(ApprovalVerdict::Accepted)
    } else {
        ModalStep::Verdict(ApprovalVerdict::Rejected)
    }
}

impl Drop for AppKitTray {
    fn drop(&mut self) {
        // `AppKitTray` 持有 `MainThreadMarker`（`!Send`），Drop 必定发生在主线程。
        NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
    }
}

/// 根据 [`ApprovalRequest`] 构造审批 `NSAlert`（含「接受」/「拒绝」两按钮）。
///
/// 拆出纯构造 helper 供 [`AppKitTray::run`] 与 `tray_bench` 共用，使基准测试可在
/// 不调用 `runModal` 的前提下直接读取并断言 `NSAlert` 的文案与按钮标题。
pub(crate) fn build_approval_alert(
    mtm: MainThreadMarker,
    req: &ApprovalRequest,
) -> Retained<NSAlert> {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);

    let (message, informative) = alert_texts(req);
    alert.setMessageText(&NSString::from_str(&message));
    alert.setInformativeText(&NSString::from_str(&informative));

    // 第一个按钮对应 NSAlertFirstButtonReturn（接受），第二个对应拒绝。
    alert.addButtonWithTitle(&NSString::from_str("接受"));
    alert.addButtonWithTitle(&NSString::from_str("拒绝"));
    alert
}

/// 供 `tray-bench` 真机基准检查 `NSAlert` 构造结果的快照（不触发 `runModal`）。
#[cfg(feature = "tray-bench")]
pub struct AlertSnapshot {
    /// `NSAlert.messageText` 读取值。
    pub message_text: String,
    /// `NSAlert.informativeText` 读取值。
    pub informative_text: String,
    /// `NSAlert.buttons` 按添加顺序读取的按钮标题列表。
    pub button_titles: Vec<String>,
}

/// 构造真实 `NSAlert` 并读回文案与按钮标题（仅 `tray-bench` feature 可见，不弹模态框）。
#[cfg(feature = "tray-bench")]
pub fn inspect_approval_alert(mtm: MainThreadMarker, req: &ApprovalRequest) -> AlertSnapshot {
    let alert = build_approval_alert(mtm, req);
    let message_text = alert.messageText().to_string();
    let informative_text = alert.informativeText().to_string();
    let buttons = alert.buttons();
    let mut button_titles = Vec::with_capacity(buttons.len());
    for btn in buttons.iter() {
        button_titles.push(btn.title().to_string());
    }
    AlertSnapshot {
        message_text,
        informative_text,
        button_titles,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::mpsc;

    use objc2_app_kit::{
        NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSModalResponseAbort,
        NSModalResponseContinue, NSModalResponseStop,
    };
    use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};
    use rc_session::proto::{AuthMethod, SessionParams};

    use crate::bridge::ApprovalBridge;
    use crate::TrayEvent;

    use super::{
        classify_modal_response, drain_tray_events, poll_modal_interrupt, ModalInterrupt, ModalStep,
    };

    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            session_id: 7,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50000),
            requested: SessionParams {
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_bps: 4_000_000,
            },
            auth_method: AuthMethod::None,
            password_verified: false,
        }
    }

    /// M3 回归：当 `QuitRequested` 与待审批请求同拍到达时，先排空 `event_rx`
    /// 必须立即返回 quit，不消费 `ui_side.take_request()`。
    #[test]
    fn drain_tray_events_preempts_pending_approval_on_quit() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();
        session.submit(&sample_request());

        let (tx, rx) = mpsc::channel();
        tx.send(TrayEvent::DisconnectRequested).unwrap();
        tx.send(TrayEvent::QuitRequested).unwrap();

        let mut seen = Vec::new();
        let (worked, quit) = drain_tray_events(&rx, &mut |ev| seen.push(ev));
        assert!(worked);
        assert!(quit);
        assert_eq!(
            seen,
            vec![TrayEvent::DisconnectRequested, TrayEvent::QuitRequested]
        );
        // 因已退出，审批请求未被 take_request 消费。
        assert!(ui.take_request().is_some());
    }

    /// M3 回归：非阻塞模态轮询在会话侧 cancel 或菜单 Quit 到达时必须立刻中断，
    /// 不会继续等待用户点击。
    #[test]
    fn poll_modal_interrupt_detects_cancel_and_quit() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();
        let (tx, rx) = mpsc::channel();

        session.submit(&sample_request());
        let _req = ui.take_request().expect("取走审批请求");

        // 无中断时继续模态。
        let mut seen = Vec::new();
        assert_eq!(
            poll_modal_interrupt(&mut ui, &rx, &mut |ev| seen.push(ev)),
            ModalInterrupt::Continue
        );

        // 会话侧因对端断开/超时取消审批 → 立即返回 Cancelled。
        session.cancel();
        assert_eq!(
            poll_modal_interrupt(&mut ui, &rx, &mut |ev| seen.push(ev)),
            ModalInterrupt::Cancelled
        );
        // 复位后恢复 Continue。
        assert_eq!(
            poll_modal_interrupt(&mut ui, &rx, &mut |ev| seen.push(ev)),
            ModalInterrupt::Continue
        );

        // 模态期间点击「退出」→ 立即返回 QuitRequested 并派发回调。
        tx.send(TrayEvent::QuitRequested).unwrap();
        assert_eq!(
            poll_modal_interrupt(&mut ui, &rx, &mut |ev| seen.push(ev)),
            ModalInterrupt::QuitRequested
        );
        assert_eq!(seen, vec![TrayEvent::QuitRequested]);
    }

    /// M3 回归：`runModalSession` 返回值分类必须精确区分 Continue / Stop / Abort / 接受 / 拒绝。
    #[test]
    fn classify_modal_response_maps_all_outcomes() {
        assert_eq!(
            classify_modal_response(NSModalResponseContinue),
            ModalStep::Continue
        );
        assert_eq!(
            classify_modal_response(NSModalResponseStop),
            ModalStep::Aborted
        );
        assert_eq!(
            classify_modal_response(NSModalResponseAbort),
            ModalStep::Aborted
        );
        assert_eq!(
            classify_modal_response(NSAlertFirstButtonReturn),
            ModalStep::Verdict(ApprovalVerdict::Accepted)
        );
        assert_eq!(
            classify_modal_response(NSAlertSecondButtonReturn),
            ModalStep::Verdict(ApprovalVerdict::Rejected)
        );
    }
}
