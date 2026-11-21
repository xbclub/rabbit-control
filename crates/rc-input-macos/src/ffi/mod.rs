//! CGEvent 注入与捕获的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**（含子模块 [`tap`]）：
//! - `AXIsProcessTrusted`（ApplicationServices）：注入/捕获前的辅助功能
//!   预检（T12 M2），core-graphics 0.24 不提供，这里自行声明；

//! - [`tap`]（捕获侧）：`CGEventTapEnable` / `CGEventSourceFlagsState`
//!   自行声明；CGEventTap 的创建、CFRunLoop 驱动走 core-graphics /
//!   core-foundation 的安全包装；
//! - `CGEvent` 的构造与投递走 core-graphics crate 的**安全**包装
//!   （`CGEvent::new_mouse_event` / `new_scroll_event` / `new_keyboard_event`
//!   / `post`），不产生裸指针操作。
//!
//! 对外只暴露安全类型 [`CgPoster`]（注入）与 [`TapCtl`] /
//! [`spawn_tap_thread`]（捕获），每个 `unsafe` 块都带 `// SAFETY:`。
//! 本模块的存在使 crate 其余部分可以 `deny(unsafe_code)`。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

pub(crate) mod tap;

pub(crate) use tap::{spawn_tap_thread, TapCtl};

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use crate::error::InputError;
use crate::event::PointerButtonKind;
use crate::plan::{EventPoster, PlannedEvent, PlannedMouseKind, ScreenBounds};

mod raw {
    // AXUIElement.h：`Boolean AXIsProcessTrusted(void)`（10.4+）。
    // Boolean == unsigned char == u8。注意这个变体**没有 options 参数、
    // 绝不弹窗**，适合做只读预检。
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub(super) fn AXIsProcessTrusted() -> u8;
    }

    // CGEventSource.h 的 `CGEventSourceCounterForEventType` 曾在这里自行
    // 声明、供真机测试断言注入进入 HID 系统源。已删除：实测 CGEventPost
    // 合成事件只进 Session 态、不进 HID 态计数（偶尔通过纯属混入真实
    // 硬件移动）。现行断言面是 tap 见证 + Session 态光标位置快照
    //（见 injector.rs 的 `inject_moves_cursor` 文档），不再需要该 extern。
}

/// 权限预检：`AXIsProcessTrusted()`，无 options 变体、绝不弹窗。
pub(crate) fn ax_is_process_trusted() -> bool {
    // SAFETY: 无参纯查询；返回 Boolean(0/1)，只判断非零。
    unsafe { raw::AXIsProcessTrusted() != 0 }
}

/// 当前光标位置（全局坐标）。`#[ignore]` 真机测试用。
///
/// 公开 SDK 没有直接的「查询光标位置」API；`CGEventCreate(NULL)` 快照
/// 事件携带当前光标位置是标准等价物（core-graphics 0.24 已包装）。
/// 测试据此避开「注入零位移移动被系统合并丢弃」的位置依赖缺陷。
#[cfg(test)]
pub(crate) fn current_cursor_location() -> (f64, f64) {
    // core-graphics 0.24 的 CGEvent::new 需要 source（无 None 变体）：创建
    // CombinedSession 态源的快照事件，其 location 即当前光标位置。
    // **必须用 Session 态而不是 HID 态**：CGEventPost 的合成事件只进
    // Session 态（HID 态计数器/位置只反映真实硬件输入——实测合成事件
    // 不涨 HID 计数，早期版本用它断言导致真机测试随机挂）。
    let source = core_graphics::event_source::CGEventSource::new(
        core_graphics::event_source::CGEventSourceStateID::CombinedSessionState,
    )
    .expect("Session 态事件源创建失败");
    let snap = core_graphics::event::CGEvent::new(source).expect("快照事件创建失败");
    let p = snap.location();
    (p.x, p.y)
}

/// 所有在线显示器 `CGDisplayBounds` 的联合边界（全局坐标）。
///
/// 取不到在线列表（极端情况）时退回主显示器边界。
pub(crate) fn screen_bounds() -> ScreenBounds {
    let displays = CGDisplay::active_displays().unwrap_or_default();
    if displays.is_empty() {
        return display_rect_to_bounds(CGDisplay::main().bounds());
    }
    let mut union = CGDisplay::new(displays[0]).bounds();
    for &id in &displays[1..] {
        union = union_rect(union, CGDisplay::new(id).bounds());
    }
    display_rect_to_bounds(union)
}

fn display_rect_to_bounds(r: CGRect) -> ScreenBounds {
    ScreenBounds::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn union_rect(a: CGRect, b: CGRect) -> CGRect {
    let x1 = a.origin.x.min(b.origin.x);
    let y1 = a.origin.y.min(b.origin.y);
    let x2 = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let y2 = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    CGRect {
        origin: CGPoint { x: x1, y: y1 },
        size: CGSize {
            width: x2 - x1,
            height: y2 - y1,
        },
    }
}

/// 构造**行单位**滚轮 CGEvent：wheel1 = `dy`（垂直，向上为正），
/// wheel2 = `dx`（水平，向左为正），wheel_count=2（无第三轴）。
///
/// 单位必须是 [`ScrollEventUnit::LINE`]——与捕获侧 tap 读取的
/// `SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`（行单位）同量纲，macOS↔macOS
/// 回环数值对称；也是 `rc_input::InputEvent::MouseWheel` 的规范单位
/// （Windows 端 WHEEL_DELTA=120/格在 rc-input-win 除以 120 归一为行）。
/// 曾用 [`ScrollEventUnit::PIXEL`]：1..3 行的滚动被注成 1..3 **像素**
/// （系统按 ~10px/行换算后 tap 只能读到 0..1 行），回环严重不对称——
/// 离线测试 `line_scroll_event_roundtrips_tap_fields` 与
/// capture 侧的 `wheel_line_units_survive_capture_to_injection_chain`
/// 锁死不许回归。
pub(crate) fn new_line_scroll_event(
    source: CGEventSource,
    dx: i32,
    dy: i32,
) -> std::result::Result<CGEvent, ()> {
    CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, dy, dx, 0)
}

/// [`EventPoster`] 的 CGEvent 生产实现：持有事件源，把 [`PlannedEvent`]
/// 构造成 `CGEvent` 并 `CGEventPost` 到 HID 层事件 tap。
///
/// **前置**：`CGEventPost` 需要辅助功能授权（`AXIsProcessTrusted`），
/// 由上层 [`Injector`](crate::injector::Injector) 预检；未授权时系统静默
/// 丢弃事件（不报错）。
pub struct CgPoster {
    source: CGEventSource,
}

// SAFETY: core-graphics 0.24 未给 CGEventSource 标 Send，但这里跨线程移动
// 是安全的：CGEventSource 是 Core Foundation 引用计数对象，CFRetain /
// CFRelease 线程安全；Quartz Event Services 的 CGEvent 构造与 CGEventPost
// 按苹果线程安全摘要可从任意线程调用。使用方（Injector / InputBridge）
// 只以 `&mut self` 串行访问，不共享引用（Send 足矣，不需要 Sync）。
// T10 的 `rc_input::InputInjector` trait 以 `Send` 为超特征，没有它生产
// 桥接（`InputBridge<CgPoster>`）无法满足契约。
unsafe impl Send for CgPoster {}

impl CgPoster {
    /// 创建事件源。用 `HIDSystemState`：合成事件从 HID 层进入系统，
    /// 对绝大多数应用可见（含要求硬件事件的游戏类）。
    pub fn new() -> Result<Self, InputError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| InputError::SourceCreateFailed)?;
        Ok(Self { source })
    }
}

impl EventPoster for CgPoster {
    fn post(&mut self, planned: &PlannedEvent) -> Result<(), InputError> {
        match planned {
            PlannedEvent::Mouse { kind, button, x, y } => {
                let (event_type, cg_button) = match (kind, button) {
                    // 移动事件的按钮字段被 CG 忽略，但 API 强制要给。
                    (PlannedMouseKind::Moved, _) => (CGEventType::MouseMoved, CGMouseButton::Left),
                    (PlannedMouseKind::Down, PointerButtonKind::Left) => {
                        (CGEventType::LeftMouseDown, CGMouseButton::Left)
                    }
                    (PlannedMouseKind::Up, PointerButtonKind::Left) => {
                        (CGEventType::LeftMouseUp, CGMouseButton::Left)
                    }
                    (PlannedMouseKind::Down, PointerButtonKind::Right) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right)
                    }
                    (PlannedMouseKind::Up, PointerButtonKind::Right) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right)
                    }
                    (PlannedMouseKind::Down, PointerButtonKind::Middle) => {
                        (CGEventType::OtherMouseDown, CGMouseButton::Center)
                    }
                    (PlannedMouseKind::Up, PointerButtonKind::Middle) => {
                        (CGEventType::OtherMouseUp, CGMouseButton::Center)
                    }
                };
                let event = CGEvent::new_mouse_event(
                    self.source.clone(),
                    event_type,
                    CGPoint { x: *x, y: *y },
                    cg_button,
                )
                .map_err(|_| InputError::EventCreateFailed("mouse"))?;
                // 明确置 clickState=1（单击）：CGEventCreateMouseEvent 不保证
                // 该字段默认值，部分应用依赖它判定点击次数。
                event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
            PlannedEvent::Scroll { dx, dy } => {
                let event = new_line_scroll_event(self.source.clone(), *dx, *dy)
                    .map_err(|_| InputError::EventCreateFailed("scroll"))?;
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
            PlannedEvent::Key { keycode, pressed } => {
                let event = CGEvent::new_keyboard_event(self.source.clone(), *keycode, *pressed)
                    .map_err(|_| InputError::EventCreateFailed("keyboard"))?;
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行单位滚轮事件的**tap 侧读回对称性**（离线，无需授权——构造
    /// CGEvent 不需要辅助功能，只有 post 需要）：
    ///
    /// 注入侧 `new_line_scroll_event` 构造的事件，用捕获侧 tap 实际读取
    /// 的同一对字段（`SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`，见 `ffi::tap`
    /// 的 `extract_raw`）读回，数值必须**逐值相等**——这是 macOS↔macOS
    /// 滚轮回环对称的机制级锁定。
    ///
    /// 选 delta=3（垂直）/-2 与 2（水平）不是随意值：本机实测 PIXEL 单位
    /// 构造同值事件时系统按 ~10px/行换算，AXIS_1 只能读回 1（10× 缩水），
    /// 因此若有人把单位改回 PIXEL，本测试当场爆。3/±2 的典型值域正来自
    /// 审计问题原文（「典型 ±1..±3」）。
    #[test]
    fn line_scroll_event_roundtrips_tap_fields() {
        let source =
            CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("构造事件源无需授权");

        // 垂直：wheel1 = dy（向上为正，与 tap 读回的 AXIS_1 同符号同值）
        let v = new_line_scroll_event(source.clone(), 0, 3).expect("line scroll event");
        assert_eq!(
            v.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            3,
            "垂直 3 行注入，tap 侧 AXIS_1 必须读回 3"
        );
        assert_eq!(
            v.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            0
        );

        // 垂直向下 -2 行
        let vd = new_line_scroll_event(source.clone(), 0, -2).expect("line scroll event");
        assert_eq!(
            vd.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            -2
        );

        // 水平：wheel2 = dx（向左为正；桥接层已把 rc 的向右为正翻成 dx 负）
        let h = new_line_scroll_event(source, -2, 0).expect("line scroll event");
        assert_eq!(
            h.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            -2,
            "水平 dx=-2（rc 向右 2 行）注入，tap 侧 AXIS_2 必须读回 -2"
        );
        assert_eq!(
            h.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            0
        );
    }
}
