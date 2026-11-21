//! 事件 → CGEvent 的中间表示与纯函数翻译层。
//!
//! [`plan`] 把 [`InputEvent`] 翻译成 [`PlannedEvent`]（“要构造哪种 CGEvent”
//! 的平台中立描述），并在此完成两件防脏数据的事：
//! 1. **坐标钳制**：指针坐标钳制进 [`ScreenBounds`]（全部在线显示器的
//!    联合边界），远控端发来的越界坐标不会被原样透传；
//! 2. **滚动取整**：行单位滚动量四舍五入为 `i32`（CG 的滚轮 API 只吃
//!    整数），`NaN` / 无穷归零。
//!
//! 投递动作抽象为 [`EventPoster`]：生产实现是 CGEvent
//! （[`CgPoster`](crate::ffi::CgPoster)，仅 macOS），单元测试注入假实现，
//! **无需辅助功能授权**即可验证到投递边界的全链路。

use crate::error::InputError;
use crate::event::{InputEvent, PointerButtonKind};
use crate::keymap::virtual_keycode;

/// 全局显示坐标的边界矩形。原点 = 主显示器左上角，y 向下，单位是**点**。
///
/// 多显示器时取所有在线显示器 `CGDisplayBounds` 的联合——左侧副屏在
/// 负象限（如 x ∈ [-1920, 0)），所以原点允许为负。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenBounds {
    /// 左边界 x。
    pub x: f64,
    /// 上边界 y。
    pub y: f64,
    /// 宽（点）。
    pub width: f64,
    /// 高（点）。
    pub height: f64,
}

impl ScreenBounds {
    /// 构造。
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 把坐标 `(x, y)` 钳制进边界（闭区间 `[x, x+width]`）。
    ///
    /// `NaN` 归到左上角：`f64::max/min` 遇 `NaN` 取另一侧（边界值），
    /// 远控端发来 `NaN` 时指针停在确定位置而不是未定义行为。
    pub fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        let (min_x, max_x) = (self.x, self.x + self.width);
        let (min_y, max_y) = (self.y, self.y + self.height);
        (x.max(min_x).min(max_x), y.max(min_y).min(max_y))
    }
}

/// 鼠标事件的按下状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannedMouseKind {
    /// 纯移动。
    Moved,
    /// 按下。
    Down,
    /// 抬起。
    Up,
}

/// 计划注入的 CGEvent（平台中立中间表示）。
///
/// 只描述“构造哪种事件 + 参数”，不触碰 CGEvent 本体——翻译逻辑因此可以
/// 在无授权、非 macOS 的环境单测。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlannedEvent {
    /// 鼠标事件（移动 / 按下 / 抬起，坐标已钳制）。
    Mouse {
        /// 移动、按下还是抬起。
        kind: PlannedMouseKind,
        /// 按键。移动事件中此字段无意义（CG API 强制要求给一个），
        /// 固定填 `Left`。
        button: PointerButtonKind,
        /// 全局坐标 x（已钳制）。
        x: f64,
        /// 全局坐标 y（已钳制）。
        y: f64,
    },
    /// 滚轮（行单位，已取整）。
    Scroll {
        /// 水平滚动量（行，向左为正）。
        dx: i32,
        /// 垂直滚动量（行，向上为正）。
        dy: i32,
    },
    /// 键盘事件（虚拟键码，来自 [`virtual_keycode`]）。
    Key {
        /// macOS 虚拟键码。
        keycode: u16,
        /// `true` = 按下。
        pressed: bool,
    },
}

/// 把 [`PlannedEvent`] 真正投递进系统的注入缝。
///
/// 生产实现 [`CgPoster`](crate::ffi::CgPoster) 构造 `CGEvent` 并
/// `CGEventPost`（需要辅助功能授权）；测试用假实现记录事件即可，
/// 整个翻译与注入器逻辑都不碰系统。
pub trait EventPoster {
    /// 投递一条计划事件。
    fn post(&mut self, planned: &PlannedEvent) -> Result<(), InputError>;
}

/// 翻译一条输入事件（纯函数）。
///
/// 返回的 [`PlannedEvent`] 已完成坐标钳制与滚动取整；
/// 键映射失败（未来扩展位）返回 [`InputError::UnsupportedKey`]。
pub fn plan(event: &InputEvent, screen: ScreenBounds) -> Result<PlannedEvent, InputError> {
    match event {
        InputEvent::PointerMove { x, y } => {
            let (x, y) = screen.clamp(*x, *y);
            Ok(PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x,
                y,
            })
        }
        InputEvent::PointerButton {
            button,
            pressed,
            x,
            y,
        } => {
            let (x, y) = screen.clamp(*x, *y);
            Ok(PlannedEvent::Mouse {
                kind: if *pressed {
                    PlannedMouseKind::Down
                } else {
                    PlannedMouseKind::Up
                },
                button: *button,
                x,
                y,
            })
        }
        InputEvent::PointerScroll { dx, dy } => Ok(PlannedEvent::Scroll {
            dx: scroll_to_i32(*dx),
            dy: scroll_to_i32(*dy),
        }),
        InputEvent::Key { key, pressed } => match virtual_keycode(*key) {
            Some(keycode) => Ok(PlannedEvent::Key {
                keycode,
                pressed: *pressed,
            }),
            None => Err(InputError::UnsupportedKey(*key)),
        },
    }
}

/// 滚动量取整（行单位）：`NaN` / 无穷 → 0（远控端发脏数据时不如
/// 不动），其余四舍五入；超出 `i32` 的极端值钳到边界。
fn scroll_to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    v.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Key;

    /// 坐标钳制：负向 / 正向越界、NaN、界内不动，以及 `plan` 层确实生效。
    /// 用带左侧副屏的联合边界（负原点）覆盖多显示器场景。
    #[test]
    fn plan_clamps_pointer_into_screen_bounds() {
        let screen = ScreenBounds::new(-1920.0, 0.0, 1920.0 + 2560.0, 1080.0);

        // 负向越界 → 左边界
        assert_eq!(screen.clamp(-5000.0, 100.0), (-1920.0, 100.0));
        // 正向越界 → 右/下边界
        assert_eq!(screen.clamp(4000.0, 5000.0), (2560.0, 1080.0));
        // NaN → 左上角
        assert_eq!(screen.clamp(f64::NAN, f64::NAN), (-1920.0, 0.0));
        // 界内不动
        assert_eq!(screen.clamp(100.0, 100.0), (100.0, 100.0));

        // plan 真的用到了钳制
        let ev = plan(
            &InputEvent::PointerMove {
                x: -5000.0,
                y: 5432.0,
            },
            screen,
        )
        .unwrap();
        match ev {
            PlannedEvent::Mouse { kind, x, y, .. } => {
                assert_eq!(kind, PlannedMouseKind::Moved);
                assert_eq!((x, y), (-1920.0, 1080.0));
            }
            other => panic!("expected Mouse, got {other:?}"),
        }
    }

    /// 四类事件的翻译正确性：按钮/状态映射、滚动取整与脏数据、键码。
    #[test]
    fn plan_translates_each_event_kind() {
        let screen = ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0);

        // 移动
        assert_eq!(
            plan(&InputEvent::PointerMove { x: 10.0, y: 20.0 }, screen).unwrap(),
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                button: PointerButtonKind::Left,
                x: 10.0,
                y: 20.0,
            }
        );

        // 右键按下
        assert_eq!(
            plan(
                &InputEvent::PointerButton {
                    button: PointerButtonKind::Right,
                    pressed: true,
                    x: 300.0,
                    y: 400.0,
                },
                screen
            )
            .unwrap(),
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Down,
                button: PointerButtonKind::Right,
                x: 300.0,
                y: 400.0,
            }
        );

        // 中键抬起
        assert_eq!(
            plan(
                &InputEvent::PointerButton {
                    button: PointerButtonKind::Middle,
                    pressed: false,
                    x: 0.0,
                    y: 0.0,
                },
                screen
            )
            .unwrap(),
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Up,
                button: PointerButtonKind::Middle,
                x: 0.0,
                y: 0.0,
            }
        );

        // 滚轮：四舍五入（0.5 远离零）；NaN/无穷归 0
        assert_eq!(
            plan(&InputEvent::PointerScroll { dx: 3.6, dy: -2.6 }, screen).unwrap(),
            PlannedEvent::Scroll { dx: 4, dy: -3 }
        );
        assert_eq!(
            plan(
                &InputEvent::PointerScroll {
                    dx: f64::NAN,
                    dy: f64::INFINITY,
                },
                screen
            )
            .unwrap(),
            PlannedEvent::Scroll { dx: 0, dy: 0 }
        );

        // 键盘：Key::Return = kVK_Return = 0x24
        assert_eq!(
            plan(
                &InputEvent::Key {
                    key: Key::Return,
                    pressed: true,
                },
                screen
            )
            .unwrap(),
            PlannedEvent::Key {
                keycode: 0x24,
                pressed: true,
            }
        );
    }
}
