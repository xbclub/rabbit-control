//! CGEventTap 捕获的 FFI 边界（本模块是 crate 内唯一允许 unsafe 的
//! `ffi` 模块的一部分）。
//!
//! 职责：在**专用线程**上创建 `kCGSessionEventTap + kCGHeadInsertEventTap +
//! kCGEventTapListenOnly`（只听不改，绝不拦截系统事件）的事件 tap，把回调
//! 收到的 CGEvent 抽成纯数据 [`RawCgEvent`](crate::capture::RawCgEvent) 送进
//! **有界** mpsc 通道；线程内跑 `CFRunLoop` 驱动 tap。tap 被系统超时禁用时
//! 在回调里就地重新启用。
//!
//! # 通道背压（有界 + 丢最旧，回调绝不阻塞）
//!
//! 回调 → 消费侧的通道是 `sync_channel(TAP_CHANNEL_CAP)`（有界）。回调里
//! **绝不能阻塞**：CGEventTap 回调超时会被系统摘除 tap（整条捕获链路
//! 死亡），所以发送走 [`send_drop_oldest`]——`try_send` 满时丢最旧腾位
//! 再试，全程只用非阻塞原语（`try_lock` / `try_recv` / `try_send`）；
//! 消费侧恰好持锁时降级为丢当前事件。每次丢弃在
//! [`TapCtl::dropped_channel_full`] 计数（与捕获映射层的
//! `dropped_unmapped` 分开的另一个诊断面）。
//!
//! # 线程模型（对应任务卡「回调线程必须有 CFRunLoop、tap 创建后要显式
//! enable」两条坑）
//!
//! ```text
//! 调用线程                             tap 线程（runloop）
//! ────────                             ─────────────────
//! spawn_tap_thread() ──spawn──▶  CGEventTap::new(Session, Head, ListenOnly)
//!                                 create_runloop_source + add_source
//!                                 tap.enable()            ← 显式 enable
//!                                 setup_tx.send(Ok)  ────▶ recv（握手）
//!                                 loop { run_in_mode(100ms) }
//! next_events() ◀─有界 mpsc──  回调：extract_raw + send_drop_oldest
//! Drop: stop=true ──────────▶  loop 退出 → remove_source → drop(tap)
//! ```
//!
//! 停止**不走 `CFRunLoopStop`**：runloop 指针跨线程共享存在「线程退出后
//! runloop 被释放 → 悬垂指针」竞态，而 `run_in_mode` 分片（100ms）让
//! 线程只凭 stop 标志就能在极短时间内自退，简单且无竞态。
//!
//! # 权限（未授权绝不静默空转）
//!
//! 监听 tap 同样需要 TCC 授权（辅助功能，或较弱的输入监控）。未授权时
//! `CGEventTapCreate` 返回 NULL——这是少数**可见**的失败信号，映射为
//! [`InputError::TapCreateFailed`]；上层 [`CgCapture`](crate::capture::CgCapture)
//! 另有 `AXIsProcessTrusted` 预检（沿用注入器的模式）兜住「创建成功但
//! 收不到事件」的暗坑。

// 见 ffi 模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::CFRunLoop;
use core_foundation_sys::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, EventField,
};
use core_graphics::event_source::CGEventSourceStateID;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::capture::{RawCgEvent, RawKind};
use crate::error::InputError;

mod raw {
    use core_foundation::mach_port::CFMachPortRef;
    use core_graphics::event_source::CGEventSourceStateID;

    // CGEvent.h：`void CGEventTapEnable(CFMachPortRef tap, Boolean enable)`（10.4+）。
    // core-graphics 0.24 在 CGEventTap::enable 里内部使用但未导出，这里自行
    // 声明，用于回调内就地恢复被系统超时禁用的 tap。
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub(super) fn CGEventTapEnable(tap: CFMachPortRef, enable: u8);
    }

    // CGEventSource.h：`CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID stateID)`
    // （10.4+）。读取某事件源状态的**当前**修饰键位（AlphaShift/Shift/...），
    // 不区分左右侧。捕获侧用它给每条 Key 事件打修饰键快照。
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub(super) fn CGEventSourceFlagsState(state: CGEventSourceStateID) -> u64;
    }
}

/// tap 回调 → 消费侧事件通道的容量（有界）。
///
/// 消费侧（`CgCapture::next_events`）每个泵周期（~30ms）收干一次，
/// 256 条 ≈ 8500 事件/秒的持续吞吐余量，只有失控风暴才会触顶；触顶
/// 后丢最旧（见 [`send_drop_oldest`]），最新事件始终优先保住。
pub(crate) const TAP_CHANNEL_CAP: usize = 256;

/// tap 线程的控制块：`stop` 置位后线程在一个 100ms 分片内自行退出。
pub(crate) struct TapCtl {
    /// 停止标志（Drop 时由所有者线程置位）。
    pub(crate) stop: AtomicBool,
    /// 本线程创建的 tap 的 `CFMachPortRef`（usize 暂存，0 = 未发布）。
    /// 只在 tap 线程内写入并读取——回调与 tap 同线程，无数据竞争。
    tap_port: AtomicUsize,
    /// 事件通道满而被丢弃的事件累计数（回调线程写，消费线程读；
    /// 与捕获映射层的 `dropped_unmapped` 是两个独立的诊断面）。
    /// `pub(crate)`：本 crate 的捕获器读它做诊断暴露，crate 外不可见。
    pub(crate) dropped_channel_full: AtomicU64,
}

impl TapCtl {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            tap_port: AtomicUsize::new(0),
            dropped_channel_full: AtomicU64::new(0),
        }
    }

    /// 取 tap 的 CFMachPortRef 整数值（仅 tap 线程内调用）。
    fn tap_port_ptr(&self, tap: &CGEventTap<'_>) -> usize {
        tap.mach_port.as_concrete_TypeRef() as usize
    }

    /// 事件通道满被丢弃的事件累计数（诊断：消费侧跟不上时的丢失量）。
    pub(crate) fn dropped_channel_full(&self) -> u64 {
        self.dropped_channel_full.load(Ordering::Relaxed)
    }

    /// 测试构造（不启动线程，仅供 `CgCapture::from_parts` 组装）。
    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::new()
    }
}

/// 事件源 mask 覆盖表（纯函数，离线单测锁死）。
///
/// - 移动：`MouseMoved` + 三个 `*MouseDragged`（按住按键拖动期间系统发
///   的是 Dragged 而不是 Moved，不订阅则拖动全程捕获静默）；
/// - 左键：`LeftMouseDown/Up`；
/// - 右键：`RightMouseDown/Up`——注意右键在 CG 层是**独立事件类型**而非
///   `OtherMouse*`（OtherMouse 只承载 ≥2 号键：中/侧键）；
/// - 中/侧键：`OtherMouseDown/Up`（按钮号在字段里）；
/// - 滚轮：`ScrollWheel`；
/// - 修饰键：`FlagsChanged`（macOS 上修饰键不产生 keyDown/keyUp）；
/// - 普通：`KeyDown/Up`。
///
/// `TapDisabledByTimeout/ByUserInput` 是带外通知，不占 mask 位，回调里
/// 单独处理。
pub(crate) fn interest_types() -> Vec<CGEventType> {
    vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::ScrollWheel,
        CGEventType::FlagsChanged,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
    ]
}

/// `kCGEventSourceFlagsState(HIDSystemState)` 的当前修饰键快照（u64 位集，
/// 位定义即 `CGEventFlags`）。任意线程可调，只读。
pub(crate) fn flags_state_snapshot() -> u64 {
    // SAFETY: 纯查询（HIDSystemState 是合法枚举值），无副作用；返回值按值
    // 传递，不产生任何指针别名。
    unsafe { raw::CGEventSourceFlagsState(CGEventSourceStateID::HIDSystemState) }
}

/// 把回调收到的 CGEvent 抽成纯数据（只用 core-graphics 的安全读接口，
/// 本函数无 unsafe）。
fn extract_raw(ty: CGEventType, event: &CGEvent) -> RawCgEvent {
    let mut raw = RawCgEvent {
        kind: RawKind::MouseMove,
        x: 0.0,
        y: 0.0,
        button: 0,
        keycode: 0,
        wheel1: 0,
        wheel2: 0,
        event_flags: 0,
        state_flags: 0,
    };
    match ty {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let p = event.location();
            raw.kind = RawKind::MouseMove;
            raw.x = p.x;
            raw.y = p.y;
        }
        CGEventType::LeftMouseDown
        | CGEventType::LeftMouseUp
        | CGEventType::RightMouseDown
        | CGEventType::RightMouseUp
        | CGEventType::OtherMouseDown
        | CGEventType::OtherMouseUp => {
            let p = event.location();
            raw.kind = if matches!(
                ty,
                CGEventType::LeftMouseDown
                    | CGEventType::RightMouseDown
                    | CGEventType::OtherMouseDown
            ) {
                RawKind::MouseDown
            } else {
                RawKind::MouseUp
            };
            raw.x = p.x;
            raw.y = p.y;
            // 左/右键的类型已自证按钮号；OtherMouse* 从字段取（0=左 1=右
            // 2=中 3=X1 4=X2——CG 与 rc-input 的 MouseButton 编号恰好一致）。
            raw.button = match ty {
                CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => 0,
                CGEventType::RightMouseDown | CGEventType::RightMouseUp => 1,
                _ => event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER),
            };
        }
        CGEventType::ScrollWheel => {
            raw.kind = RawKind::Scroll;
            // Axis1=垂直（行单位，向上为正）、Axis2=水平（行单位）。与注入侧
            // CGEventCreateScrollWheelEvent(wheel1, wheel2) 同一族字段。
            raw.wheel1 = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            raw.wheel2 = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
        }
        CGEventType::FlagsChanged => {
            raw.kind = RawKind::FlagsChanged;
            raw.keycode =
                u16::try_from(event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE))
                    .unwrap_or(0);
            // pressed 判定必须用事件自带 flags（它就是状态变化后的新位集）。
            raw.event_flags = event.get_flags().bits();
            raw.state_flags = flags_state_snapshot();
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            raw.kind = if matches!(ty, CGEventType::KeyDown) {
                RawKind::KeyDown
            } else {
                RawKind::KeyUp
            };
            raw.keycode =
                u16::try_from(event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE))
                    .unwrap_or(0);
            // I8：修饰键快照取事件源当前状态（kCGEventSourceFlagsState）。
            raw.state_flags = flags_state_snapshot();
        }
        // 带外类型不进本函数（回调里单独处理）；Null 等未订阅类型兜底为
        // 移动事件载荷缺省——实际不会发生（mask 未订阅就不会送达）。
        _ => {}
    }
    raw
}

/// 有界通道的回调侧发送：`try_send` 满时**丢最旧**腾位再投（全程只用
/// 非阻塞原语，CGEventTap 回调里绝不能阻塞——阻塞会被系统摘除 tap）。
///
/// - 通道满：`try_lock` 接收端锁（**try**，不等待）→ `try_recv` 丢最旧 →
///   重投。单生产者（回调）下腾出的空位不会被别人抢走，重投必成功；
///   消费侧恰好持锁（正在收干）时降级为丢**当前**事件。
/// - 每次满载事故恰丢 1 条，`dropped` 计数 +1（无论丢的是最旧还是当前）。
/// - 接收端已 drop（所有者先走）：与旧版一样静默忽略，不算丢弃。
///
/// 返回当前事件是否入队（生产路径忽略；离线测试用）。
pub(crate) fn send_drop_oldest(
    tx: &SyncSender<RawCgEvent>,
    rx: &Mutex<Receiver<RawCgEvent>>,
    dropped: &AtomicU64,
    ev: RawCgEvent,
) -> bool {
    match tx.try_send(ev) {
        Ok(()) => true,
        // 接收端已 drop（所有者先走）：不算「满」丢弃。
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(ev)) => {
            let freed = match rx.try_lock() {
                Ok(guard) => guard.try_recv().is_ok(),
                Err(_) => false, // 消费侧持锁：不等待，直接丢当前事件
            };
            // 理论上 freed 后重投不会失败（单生产者）；仍失败按丢当前计。
            let sent = freed && tx.try_send(ev).is_ok();
            dropped.fetch_add(1, Ordering::Relaxed);
            sent
        }
    }
}

/// `spawn_tap_thread` 的产物：有界事件通道的共享接收端（回调侧满载
/// 丢最旧要经它腾位）+ 线程控制块。
pub(crate) type TapThread = (Arc<Mutex<Receiver<RawCgEvent>>>, Arc<TapCtl>);

/// 启动 tap 线程：setup 握手成功后返回事件接收端与控制块。
///
/// 握手保证**返回 `Ok` 时 tap 已创建、已挂进 runloop、已 `enable`**——
/// 排除「创建成功但收不到事件」里「其实没建起来」的那一半；剩下的一半
/// （未授权时静默无事件）由上层 `AXIsProcessTrusted` 预检兜住。
pub(crate) fn spawn_tap_thread() -> Result<TapThread, InputError> {
    let (tx, rx) = std::sync::mpsc::sync_channel(TAP_CHANNEL_CAP);
    let rx_shared = Arc::new(Mutex::new(rx));
    let (setup_tx, setup_rx) = std::sync::mpsc::channel();
    let ctl = Arc::new(TapCtl::new());

    let ctl_thread = Arc::clone(&ctl);
    let rx_for_thread = Arc::clone(&rx_shared);
    thread::Builder::new()
        .name("rc-input-capture-tap".into())
        .spawn(move || run_tap_loop(ctl_thread, tx, rx_for_thread, setup_tx))
        .map_err(|_| InputError::TapCreateFailed("tap thread spawn failed"))?;

    match setup_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok((rx_shared, ctl)),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // 握手超时：命令线程停机（分片 100ms，很快自退），不让它悬空。
            ctl.stop.store(true, Ordering::Release);
            Err(InputError::TapCreateFailed("tap setup handshake timeout"))
        }
    }
}

/// tap 线程主体：创建 → 挂 runloop → enable → 握手 → 分片运行直至 stop。
fn run_tap_loop(
    ctl: Arc<TapCtl>,
    tx: SyncSender<RawCgEvent>,
    rx_shared: Arc<Mutex<Receiver<RawCgEvent>>>,
    setup_tx: Sender<Result<(), InputError>>,
) {
    let tap_port_for_cb = Arc::clone(&ctl);
    let dropped_for_cb = Arc::clone(&ctl);

    // ListenOnly 回调：只抽取数据并转发，返回 None（不替换事件）。
    let callback =
        move |_proxy: CGEventTapProxy, ty: CGEventType, event: &CGEvent| -> Option<CGEvent> {
            match ty {
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    // 被系统禁用（超时是常见原因；ListenOnly 不改事件，ByUserInput
                    // 理论上不会发生，同样兜底）。就地重新启用。
                    let port = tap_port_for_cb.tap_port.load(Ordering::Acquire);
                    if port != 0 {
                        // SAFETY: port 是本线程创建并暂存的 CFMachPortRef；回调
                        // 与 tap 同线程，此刻 tap 尚未 drop（runloop 还在跑回调），
                        // 指针必然有效。CGEventTapEnable 对已启用 tap 再启用是无害
                        // 幂等操作。
                        unsafe { raw::CGEventTapEnable(port as CFMachPortRef, 1) };
                    }
                }
                _ => {
                    // 有界通道的非阻塞发送：满时丢最旧（见 send_drop_oldest，
                    // 回调绝不能阻塞）。接收端已 drop 时静默忽略。
                    let _ = send_drop_oldest(
                        &tx,
                        &rx_shared,
                        &dropped_for_cb.dropped_channel_full,
                        extract_raw(ty, event),
                    );
                }
            }
            None
        };

    let tap = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        interest_types(),
        callback,
    ) {
        Ok(tap) => tap,
        Err(()) => {
            let _ = setup_tx.send(Err(InputError::TapCreateFailed(
                "CGEventTapCreate returned NULL",
            )));
            return;
        }
    };
    let source = match tap.mach_port.create_runloop_source(0) {
        Ok(s) => s,
        Err(()) => {
            let _ = setup_tx.send(Err(InputError::TapCreateFailed(
                "CFMachPortCreateRunLoopSource failed",
            )));
            return;
        }
    };

    // 先暂存端口再挂源再 enable：回调只可能在这些步骤之后触发，
    // 读取到的 tap_port 必非 0。
    ctl.tap_port
        .store(ctl.tap_port_ptr(&tap), Ordering::Release);

    let runloop = CFRunLoop::get_current();
    // SAFETY: kCFRunLoopCommonModes 是 CF 全局常量 CFStringRef（进程存活期
    // 有效）；add_source 是 CFRunLoop 的常规注册调用，runloop 与 source 均
    // 为本线程创建的对象。
    unsafe {
        runloop.add_source(&source, kCFRunLoopCommonModes);
    }
    // 显式 enable：tap 创建后默认禁用，不 enable 收不到任何事件（任务卡坑）。
    tap.enable();

    let _ = setup_tx.send(Ok(()));

    while !ctl.stop.load(Ordering::Acquire) {
        // 分片运行：源有事件随时唤醒执行回调；100ms 是无事件时的最长空转，
        // 也是 stop 置位后的最长退出延迟。
        // SAFETY: kCFRunLoopDefaultMode 是 CF 全局常量；run_in_mode 是安全包装。
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(100),
            false,
        );
    }

    // 清理：摘源、drop tap（CFMachPort 释放即销毁 tap）。runloop 随线程
    // 结束由系统回收，无需（也不能）显式释放。
    // SAFETY: 同上的常量与注册调用；remove 与 add 严格配对。
    unsafe {
        runloop.remove_source(&source, kCFRunLoopCommonModes);
    }
    drop(tap);
    drop(source);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 掩码覆盖表（离线，纯函数）：14 种类型一个不多一个不少。
    /// 逐个按 CGEventType 判别值断言，防止表被误删/误改。
    #[test]
    fn interest_types_cover_exactly_the_contract_set() {
        let types = interest_types();
        let discriminants: Vec<u32> = types.iter().map(|t| *t as u32).collect();
        assert_eq!(
            discriminants,
            vec![
                5,  // MouseMoved
                6,  // LeftMouseDragged
                7,  // RightMouseDragged
                27, // OtherMouseDragged
                1,  // LeftMouseDown
                2,  // LeftMouseUp
                3,  // RightMouseDown
                4,  // RightMouseUp
                25, // OtherMouseDown
                26, // OtherMouseUp
                22, // ScrollWheel
                12, // FlagsChanged
                10, // KeyDown
                11, // KeyUp
            ],
            "事件源 mask 必须与文档列出的类型一一对应"
        );
        // 重复订阅同一类型是常见笔误，显式排除。
        let mut seen = std::collections::HashSet::new();
        for t in &types {
            assert!(seen.insert(*t as u32), "重复订阅 {t:?}");
        }
    }

    /// flags_state_snapshot 是纯查询：调用不 panic，且值能安全穿过
    /// `modifiers_from_state_flags` 的位过滤——真实机器上快照可能带
    /// 设备相关位（本机实测见过 0x2000_0000，随输入设备而变），映射
    /// 必须只认四个设备无关修饰键位、其余一律忽略。
    #[test]
    fn flags_state_snapshot_is_queriable() {
        use rc_input::batch::modifier_bits as b;
        let snap = flags_state_snapshot();
        let mapped = crate::capture::modifiers_from_state_flags(snap);
        assert_eq!(
            mapped & !(b::LEFT_CTRL | b::LEFT_SHIFT | b::LEFT_ALT | b::LEFT_GUI),
            0,
            "快照 {snap:#x} 里设备相关位必须被映射过滤，不得泄漏进 rc 修饰键位"
        );
    }

    /// tap 线程生命周期（真机，**无需授权**）：未授权机器上
    /// `CGEventTapCreate` 返回 NULL → 立即 [`InputError::TapCreateFailed`]
    /// （显式失败而非静默空转）；授权机器上握手 `Ok` 且 stop 后线程在
    /// 一个 100ms 分片内干净退出（发送端 drop → 接收端 Disconnected）。
    /// 两种机器形态都不得悬挂。
    #[test]
    fn tap_thread_lifecycle_never_hangs() {
        match spawn_tap_thread() {
            Ok((rx, ctl)) => {
                ctl.stop.store(true, Ordering::Release);
                // 线程退出时会 drop 发送端：500ms 余量（分片 100ms）后
                // 通道必然断开。
                std::thread::sleep(Duration::from_millis(500));
                let rx = rx.lock().expect("tap channel lock");
                assert!(
                    matches!(
                        rx.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Disconnected)
                    ),
                    "stop 置位后 tap 线程必须退出（通道应已断开）"
                );
            }
            Err(e @ InputError::TapCreateFailed(_)) => {
                // 未授权机器（无辅助功能/输入监控）：tap 创建被系统拒绝，
                // 这是设计内的显式失败路径。
                let msg = e.to_string();
                assert!(msg.contains("辅助功能"), "错误信息应指向授权路径：{msg}");
            }
            Err(other) => panic!("意外错误（既不是成功也不是 TapCreateFailed）：{other:?}"),
        }
    }

    /// 有界通道容量常量回归锁：改小会放大正常突发下的丢弃，改大失去
    /// 背压意义——审计修复（无界 → 有界 256）的数值不许悄悄漂移。
    #[test]
    fn tap_channel_capacity_is_256() {
        assert_eq!(TAP_CHANNEL_CAP, 256);
    }

    /// 丢最旧语义（离线，纯逻辑）：容量 2 填满后投第 3 条 → 最旧被丢、
    /// 最新两按序保留、丢弃计数 +1；消费侧持锁时降级丢当前并计数。
    ///
    /// （Disconnected 分支在当前接线里不可达——回调闭包自身持有接收端
    /// 的 Arc 强引用，接收端不可能在回调运行中被释放——该分支是防御
    /// 性保留，行为与旧版 `let _ = tx.send(..)` 一致：静默忽略。）
    #[test]
    fn send_drop_oldest_drops_oldest_and_counts() {
        use crate::capture::RawKind;

        fn scroll(wheel1: i64) -> RawCgEvent {
            RawCgEvent {
                kind: RawKind::Scroll,
                x: 0.0,
                y: 0.0,
                button: 0,
                keycode: 0,
                wheel1,
                wheel2: 0,
                event_flags: 0,
                state_flags: 0,
            }
        }

        // 常规路径：满 → 丢最旧 → 新事件入队
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let rx = Mutex::new(rx);
        let dropped = AtomicU64::new(0);
        tx.send(scroll(1)).unwrap();
        tx.send(scroll(2)).unwrap();
        assert!(
            send_drop_oldest(&tx, &rx, &dropped, scroll(3)),
            "腾位后当前事件必须入队"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1, "恰丢 1 条（最旧）");
        {
            let guard = rx.lock().unwrap();
            assert_eq!(guard.try_recv().unwrap().wheel1, 2, "最旧（1）被丢");
            assert_eq!(guard.try_recv().unwrap().wheel1, 3, "最新（3）在队");
            assert!(guard.try_recv().is_err(), "通道里只剩 2 条");
        }

        // 消费侧持锁（模拟正在收干）：try_lock 失败 → 丢当前、计数，
        // 队列原样保留、绝不阻塞。
        tx.send(scroll(4)).unwrap();
        tx.send(scroll(5)).unwrap(); // 重新填满
        {
            let _guard = rx.lock().unwrap();
            assert!(
                !send_drop_oldest(&tx, &rx, &dropped, scroll(6)),
                "持锁时当前事件只能丢弃（降级路径，不等待锁）"
            );
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 2, "丢当前也计 1");
        {
            let guard = rx.lock().unwrap();
            assert_eq!(guard.try_recv().unwrap().wheel1, 4, "队头未被触碰");
            assert_eq!(guard.try_recv().unwrap().wheel1, 5, "队尾未被触碰");
            assert!(guard.try_recv().is_err(), "当前事件（6）被丢");
        }
    }
}
