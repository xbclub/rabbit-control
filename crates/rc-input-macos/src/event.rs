//! 输入事件的最小类型集合。
//!
//! rc-types 目前没有输入事件类型（只有帧/像素相关，见其 `lib.rs`），按
//! 任务卡 T12 在本 crate 定义**最小集合**：指针移动、指针按键、滚轮、
//! 键盘四类，风格照 rc-types（纯数据结构 + 内嵌单元测试）。协议层
//! （rc-proto）将来需要跨平台输入类型时，应把这里上移到 rc-types。

use crate::keymap::Key;

/// 指针（鼠标）按键。`CGMouseButton` 只有左/右/中三个，不做多余设计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButtonKind {
    /// 左键（主键）。
    Left,
    /// 右键（次键 / 菜单键）。
    Right,
    /// 中键（滚轮按下）。
    Middle,
}

/// 一条输入事件——受控端要「重放」的最小单位。
///
/// 坐标一律是**全局显示坐标**：原点在主显示器左上角，x 向右、y 向下，
/// 与 CGEvent 的坐标系一致（左侧副屏可在负象限，单位是点而非物理像素）。
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum InputEvent {
    /// 把指针移动到绝对坐标 `(x, y)`。
    PointerMove {
        /// 全局坐标 x（点）
        x: f64,
        /// 全局坐标 y（点）
        y: f64,
    },
    /// 在坐标 `(x, y)` 按下 / 抬起指针按键。
    PointerButton {
        /// 哪个按键
        button: PointerButtonKind,
        /// `true` = 按下，`false` = 抬起
        pressed: bool,
        /// 事件发生坐标 x（全局）
        x: f64,
        /// 事件发生坐标 y（全局）
        y: f64,
    },
    /// 滚轮。`dx` / `dy` 是**行**级滚动量（rc-input 线格式的规范单位，
    /// 与 CG 行单位一致），符号约定：`dy > 0` 向上滚动，`dx > 0` 向左滚动。
    PointerScroll {
        /// 水平滚动量（行，向左为正）
        dx: f64,
        /// 垂直滚动量（行，向上为正）
        dy: f64,
    },
    /// 按下 / 抬起键盘键。修饰键也是普通按键事件（按下 Shift 表现为
    /// `Key::LeftShift` + `pressed`，本 crate 不合成修饰状态机）。
    Key {
        /// 逻辑键（见 [`keymap`](crate::keymap)）
        key: Key,
        /// `true` = 按下，`false` = 抬起
        pressed: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四类事件都能构造并完整读回载荷（结构体即契约的冒烟测试）。
    #[test]
    fn event_variants_carry_full_payload() {
        let ev = InputEvent::PointerMove {
            x: -1919.5,
            y: 42.0,
        };
        assert_eq!(
            ev,
            InputEvent::PointerMove {
                x: -1919.5,
                y: 42.0
            }
        );

        let ev = InputEvent::PointerButton {
            button: PointerButtonKind::Middle,
            pressed: true,
            x: 1.0,
            y: 2.0,
        };
        let InputEvent::PointerButton {
            button,
            pressed,
            x,
            y,
        } = ev
        else {
            panic!("must destructure PointerButton")
        };
        assert_eq!(button, PointerButtonKind::Middle);
        assert!(pressed);
        assert_eq!((x, y), (1.0, 2.0));

        let InputEvent::PointerScroll { dx, dy } = (InputEvent::PointerScroll {
            dx: -12.5,
            dy: 30.0,
        }) else {
            panic!("must destructure PointerScroll")
        };
        assert_eq!((dx, dy), (-12.5, 30.0));

        let InputEvent::Key { key, pressed } = (InputEvent::Key {
            key: Key::Space,
            pressed: false,
        }) else {
            panic!("must destructure Key")
        };
        assert_eq!(key, Key::Space);
        assert!(!pressed);
    }
}
