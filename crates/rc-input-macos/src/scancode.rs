//! 逻辑键 ↔ Windows Set 1 扫描码映射表（T10 桥接层）。
//!
//! rc-input 的线格式用**扫描码**（I5：跨键盘布局不乱），Windows Set 1
//! make code；E0 前缀键（右 Ctrl、方向键、导航键等）编码为
//! `0xE0 << 8 | code`，与 [`rc_input::batch::modifier_scancodes`] 同一
//! 约定（有对照测试锁死）。
//!
//! 枚举、正反映射、全键清单由同一个宏生成——单一事实源：新增 `Key`
//! 变体（在 `keymap.rs` 加了 kVK 码）而忘了配扫描码会**编译失败**
//! （`set1_scancode` 的 match 穷尽性）。
//!
//! # 无 Set 1 扫描码的键
//!
//! - [`Key::Fn`]：PC 键盘上 Fn 不产生 make code（固件层拦截）；
//! - [`Key::KeypadEqual`]：Apple 小键盘专属，Set 1 无对应。
//!
//! 两者映射为 `None`，注入时转为显式错误（见 `bridge`）。

use crate::keymap::Key;

/// 用一张 `(变体 => 扫描码)` 表同时生成正反映射，保证双射。
macro_rules! define_scancode {
    ( $( $variant:ident => $code:expr ),* $(,)? ) => {
        /// 逻辑键 → Windows Set 1 扫描码。E0 前缀键为 `0xE0xx`。
        ///
        /// 无 Set 1 对应的键（`Fn` / `KeypadEqual`）返回 `None`。
        pub fn set1_scancode(key: Key) -> Option<u16> {
            match key {
                $( Key::$variant => Some($code), )*
                Key::Fn | Key::KeypadEqual => None,
            }
        }

        /// Windows Set 1 扫描码 → 逻辑键（[`set1_scancode`] 的逆）。
        ///
        /// 未知 / 保留扫描码返回 `None`。
        pub fn key_from_set1(code: u16) -> Option<Key> {
            match code {
                $( $code => Some(Key::$variant), )*
                _ => None,
            }
        }
    };
}

define_scancode! {
    // ── 字母（Set 1 make code）────────────────────────────────────────
    A => 0x1E, S => 0x1F, D => 0x20, F => 0x21, H => 0x23, G => 0x22,
    Z => 0x2C, X => 0x2D, C => 0x2E, V => 0x2F, B => 0x30, Q => 0x10,
    W => 0x11, E => 0x12, R => 0x13, Y => 0x15, T => 0x14, O => 0x18,
    U => 0x16, I => 0x17, P => 0x19, L => 0x26, J => 0x24, K => 0x25,
    N => 0x31, M => 0x32,

    // ── 主键盘数字 ─────────────────────────────────────────────────────
    Digit1 => 0x02, Digit2 => 0x03, Digit3 => 0x04, Digit4 => 0x05,
    Digit5 => 0x06, Digit6 => 0x07, Digit7 => 0x08, Digit8 => 0x09,
    Digit9 => 0x0A, Digit0 => 0x0B,

    // ── 标点（ANSI）────────────────────────────────────────────────────
    Minus => 0x0C,        // - _
    Equal => 0x0D,        // = +
    RightBracket => 0x1B, // ] }
    LeftBracket => 0x1A,  // [ {
    Quote => 0x28,        // ' "
    Semicolon => 0x27,    // ; :
    Backslash => 0x2B,    // \ |
    Comma => 0x33,        // , <
    Slash => 0x35,        // / ?
    Period => 0x34,       // . >
    Grave => 0x29,        // ` ~

    // ── 控制键 ─────────────────────────────────────────────────────────
    Return => 0x1C,        // Enter
    Tab => 0x0F,
    Space => 0x39,
    Backspace => 0x0E,
    Escape => 0x01,
    CapsLock => 0x3A,
    ForwardDelete => 0xE053, // E0 53

    // ── 导航（全部 E0 前缀）───────────────────────────────────────────
    Home => 0xE047, End => 0xE04F, PageUp => 0xE049, PageDown => 0xE051,
    ArrowUp => 0xE048, ArrowDown => 0xE050, ArrowLeft => 0xE04B, ArrowRight => 0xE04D,

    // ── 修饰键（与 rc_input::batch::modifier_scancodes 一致，有对照测试）──
    LeftCommand => 0xE05B, // 左 Win（在 mac 键盘上是左 Cmd）
    LeftShift => 0x2A,
    LeftOption => 0x38,    // 左 Alt；右 Alt(AltGr) 才是 E0 38
    LeftControl => 0x1D,
    RightCommand => 0xE05C,
    RightShift => 0x36,
    RightOption => 0xE038,
    RightControl => 0xE01D,

    // ── 功能键 F1–F12 ──────────────────────────────────────────────────
    F1 => 0x3B, F2 => 0x3C, F3 => 0x3D, F4 => 0x3E, F5 => 0x3F, F6 => 0x40,
    F7 => 0x41, F8 => 0x42, F9 => 0x43, F10 => 0x44, F11 => 0x57, F12 => 0x58,

    // ── 小键盘 ─────────────────────────────────────────────────────────
    KeypadDecimal => 0x53,  // .
    KeypadMultiply => 0x37, // *
    KeypadAdd => 0x4E,      // +
    KeypadClear => 0x45,    // PC 上是 NumLock，Apple 键盘是 Clear
    KeypadDivide => 0xE035, // E0 35
    KeypadEnter => 0xE01C,  // E0 1C
    KeypadSubtract => 0x4A, // -
    Keypad0 => 0x52, Keypad1 => 0x4F, Keypad2 => 0x50, Keypad3 => 0x51,
    Keypad4 => 0x4B, Keypad5 => 0x4C, Keypad6 => 0x4D, Keypad7 => 0x47,
    Keypad8 => 0x48, Keypad9 => 0x49,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 双射完整性：每个有扫描码的键正反映射闭合，且无两键撞码。
    /// `Fn` / `KeypadEqual` 显式无码。
    #[test]
    fn scancode_table_is_bijective() {
        let mut seen = HashSet::new();
        let mut mapped = 0usize;
        for &k in Key::ALL {
            match set1_scancode(k) {
                Some(code) => {
                    mapped += 1;
                    assert!(seen.insert(code), "{k:?} 与已有键撞扫描码：{code:#06X}");
                    assert_eq!(key_from_set1(code), Some(k), "{code:#06X} 反查不闭合");
                }
                None => assert!(
                    matches!(k, Key::Fn | Key::KeypadEqual),
                    "{k:?} 意外无扫描码（新键忘了进表？）"
                ),
            }
        }
        assert_eq!(seen.len(), mapped);
        assert_eq!(key_from_set1(0xFFFF), None, "未知扫描码必须返回 None");
    }

    /// 逐值对照公开的 Windows Set 1 扫描码表（抽查易错点：E0 前缀键、
    /// 左右修饰键、字母表乱序区、F11/F12 跳位）。
    #[test]
    fn scancode_matches_windows_set1_reference() {
        let expect: &[(Key, u16)] = &[
            (Key::Escape, 0x01),
            (Key::Digit1, 0x02),
            (Key::Digit0, 0x0B),
            (Key::Backspace, 0x0E),
            (Key::Tab, 0x0F),
            (Key::Q, 0x10),
            (Key::A, 0x1E),
            (Key::Return, 0x1C),
            (Key::LeftBracket, 0x1A),
            (Key::Backslash, 0x2B),
            (Key::Grave, 0x29),
            (Key::Z, 0x2C),
            (Key::M, 0x32),
            (Key::Space, 0x39),
            (Key::CapsLock, 0x3A),
            (Key::F1, 0x3B),
            (Key::F10, 0x44),
            (Key::F11, 0x57),
            (Key::F12, 0x58),
            // 修饰键（左右区分 + E0 前缀）
            (Key::LeftControl, 0x1D),
            (Key::LeftShift, 0x2A),
            (Key::LeftOption, 0x38),
            (Key::LeftCommand, 0xE05B),
            (Key::RightControl, 0xE01D),
            (Key::RightShift, 0x36),
            (Key::RightOption, 0xE038),
            (Key::RightCommand, 0xE05C),
            // E0 前缀导航 / 删除
            (Key::ForwardDelete, 0xE053),
            (Key::Home, 0xE047),
            (Key::End, 0xE04F),
            (Key::PageUp, 0xE049),
            (Key::PageDown, 0xE051),
            (Key::ArrowUp, 0xE048),
            (Key::ArrowDown, 0xE050),
            (Key::ArrowLeft, 0xE04B),
            (Key::ArrowRight, 0xE04D),
            // 小键盘
            (Key::KeypadDivide, 0xE035),
            (Key::KeypadEnter, 0xE01C),
            (Key::KeypadAdd, 0x4E),
            (Key::KeypadSubtract, 0x4A),
            (Key::Keypad0, 0x52),
            (Key::Keypad7, 0x47),
            (Key::KeypadClear, 0x45),
        ];
        for &(k, code) in expect {
            assert_eq!(set1_scancode(k), Some(code), "{k:?} 扫描码与 Set 1 不符");
        }
    }

    /// 与 rc-input 的修饰键扫描码常量互锁：两端约定一旦漂移立即失败。
    #[test]
    fn modifier_scancodes_match_rc_input_contract() {
        use rc_input::batch::modifier_scancodes as rc;
        assert_eq!(set1_scancode(Key::LeftControl), Some(rc::LEFT_CTRL));
        assert_eq!(set1_scancode(Key::LeftShift), Some(rc::LEFT_SHIFT));
        assert_eq!(set1_scancode(Key::LeftOption), Some(rc::LEFT_ALT));
        assert_eq!(set1_scancode(Key::LeftCommand), Some(rc::LEFT_GUI));
        assert_eq!(set1_scancode(Key::RightControl), Some(rc::RIGHT_CTRL));
        assert_eq!(set1_scancode(Key::RightShift), Some(rc::RIGHT_SHIFT));
        assert_eq!(set1_scancode(Key::RightOption), Some(rc::RIGHT_ALT));
        assert_eq!(set1_scancode(Key::RightCommand), Some(rc::RIGHT_GUI));
    }
}
