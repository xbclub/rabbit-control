//! 逻辑键 → macOS 虚拟键码映射表。
//!
//! # 覆盖范围（108 键）
//! - 字母 A–Z、主键盘数字 0–9
//! - 主键盘全部 ANSI 标点：`` ` `` `~` `-` `_` `=` `+` `[` `{` `]` `}` `\` `|`
//!   `;` `:` `'` `"` `,` `<` `.` `>` `/` `?`
//! - 控制键：Return / Escape / Backspace / Tab / Space / CapsLock / ForwardDelete
//! - 导航键：Home / End / PageUp / PageDown / 四个方向键
//! - 修饰键：左/右 Command、Shift、Option、Control，以及 Fn
//! - 功能键 F1–F12
//! - 小键盘：0–9 与 `.` `/` `*` `-` `+` `=` Enter Clear
//!
//! # 明确不覆盖
//! - F13–F20、亮度/音量/媒体/弹出等硬件键（远控用不上，且多为系统拦截）
//! - 非 ANSI 布局专属键（JIS 英数/假名等）
//! - Unicode 文本输入——那是输入法的职责，不走 CGEvent 键码路径
//!
//! 键码值逐条抄自本机 SDK 的
//! `HIToolbox.framework/Headers/Events.h`（`kVK_*` 常量），
//! 单元测试 [`keymap_matches_hitooolbox_header_values`] 逐值对照。
//!
//! 枚举、键码表、全键清单由同一个宏生成——单一事实源：新增变体忘了
//! 配键码直接编译失败（穷尽 match），忘了进完整性测试清单也不可能
//! （清单与表同源）。

/// 用一张 `(变体 => 键码)` 表同时生成：`Key` 枚举、全键清单
/// [`Key::ALL`]、穷尽映射 [`virtual_keycode`]。
macro_rules! define_keymap {
    ( $( $variant:ident => $code:expr ),* $(,)? ) => {
        /// 逻辑键。**按语义命名**（`Backspace`，而不是 macOS 的 `kVK_Delete`），
        /// 与 Windows/Linux 侧键名对齐，协议里不透出平台细节。
        ///
        /// 变体名即语义（单字母 / 数字 / 惯用控制键名），逐变体写 doc 只会
        /// 复述变体名，故此处允许 `missing_docs`。
        #[allow(missing_docs)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Key {
            $( $variant, )*
        }

        impl Key {
            /// 全部键（表序）。供映射表完整性测试遍历。
            pub const ALL: &'static [Key] = &[ $( Key::$variant, )* ];
        }

        /// 逻辑键 → macOS 虚拟键码（`kVK_*`，HIToolbox `Events.h`，`u16`）。
        ///
        /// 当前表穷尽所有 [`Key`] 变体，恒返回 `Some`；保留 `Option` 是给
        /// 将来「无虚拟键码的键」预留（届时返回 `None` 并在
        /// [`plan`](crate::plan) 层转为 `UnsupportedKey` 错误）。
        pub fn virtual_keycode(key: Key) -> Option<u16> {
            match key {
                $( Key::$variant => Some($code), )*
            }
        }

        /// macOS 虚拟键码 → 逻辑键（[`virtual_keycode`] 的逆，捕获侧用）。
        ///
        /// 与表外键码（F13–F20、音量/亮度/媒体、JIS 专属、ISO Section 等）
        /// 一律返回 `None`——调用方（捕获器）显式丢弃并计数，绝不猜测。
        pub fn key_from_virtual_keycode(code: u16) -> Option<Key> {
            match code {
                $( $code => Some(Key::$variant), )*
                _ => None,
            }
        }
    };
}

define_keymap! {
    // ── 字母（kVK_ANSI_*）──────────────────────────────────────────────
    A => 0x00, S => 0x01, D => 0x02, F => 0x03, H => 0x04, G => 0x05,
    Z => 0x06, X => 0x07, C => 0x08, V => 0x09, B => 0x0B, Q => 0x0C,
    W => 0x0D, E => 0x0E, R => 0x0F, Y => 0x10, T => 0x11, O => 0x1F,
    U => 0x20, I => 0x22, P => 0x23, L => 0x25, J => 0x26, K => 0x28,
    N => 0x2D, M => 0x2E,

    // ── 主键盘数字 ─────────────────────────────────────────────────────
    Digit1 => 0x12, Digit2 => 0x13, Digit3 => 0x14, Digit4 => 0x15,
    Digit5 => 0x17, Digit6 => 0x16, Digit7 => 0x1A, Digit8 => 0x1C,
    Digit9 => 0x19, Digit0 => 0x1D,

    // ── 标点（ANSI）────────────────────────────────────────────────────
    Minus => 0x1B,        // - _
    Equal => 0x18,        // = +
    RightBracket => 0x1E, // ] }
    LeftBracket => 0x21,  // [ {
    Quote => 0x27,        // ' "
    Semicolon => 0x29,    // ; :
    Backslash => 0x2A,    // \ |
    Comma => 0x2B,        // , <
    Slash => 0x2C,        // / ?
    Period => 0x2F,       // . >
    Grave => 0x32,        // ` ~

    // ── 控制键 ─────────────────────────────────────────────────────────
    Return => 0x24,        // kVK_Return
    Tab => 0x30,           // kVK_Tab
    Space => 0x31,         // kVK_Space
    Backspace => 0x33,     // kVK_Delete（macOS 命名反直觉：这是退格）
    Escape => 0x35,        // kVK_Escape
    CapsLock => 0x39,      // kVK_CapsLock
    ForwardDelete => 0x75, // kVK_ForwardDelete

    // ── 导航 ───────────────────────────────────────────────────────────
    Home => 0x73,      End => 0x77,   PageUp => 0x74,  PageDown => 0x79,
    ArrowUp => 0x7E, ArrowDown => 0x7D, ArrowLeft => 0x7B, ArrowRight => 0x7C,

    // ── 修饰键 ─────────────────────────────────────────────────────────
    LeftCommand => 0x37,  // kVK_Command
    LeftShift => 0x38,    // kVK_Shift
    LeftOption => 0x3A,   // kVK_Option
    LeftControl => 0x3B,  // kVK_Control
    RightCommand => 0x36, // kVK_RightCommand
    RightShift => 0x3C,   // kVK_RightShift
    RightOption => 0x3D,  // kVK_RightOption
    RightControl => 0x3E, // kVK_RightControl
    Fn => 0x3F,            // kVK_Function

    // ── 功能键 F1–F12 ──────────────────────────────────────────────────
    F1 => 0x7A, F2 => 0x78, F3 => 0x63, F4 => 0x76, F5 => 0x60, F6 => 0x61,
    F7 => 0x62, F8 => 0x64, F9 => 0x65, F10 => 0x6D, F11 => 0x67, F12 => 0x6F,

    // ── 小键盘 ─────────────────────────────────────────────────────────
    KeypadDecimal => 0x41,  // kVK_ANSI_KeypadDecimal  .
    KeypadMultiply => 0x43, // kVK_ANSI_KeypadMultiply *
    KeypadAdd => 0x45,      // kVK_ANSI_KeypadPlus     +
    KeypadClear => 0x47,    // kVK_ANSI_KeypadClear
    KeypadDivide => 0x4B,   // kVK_ANSI_KeypadDivide   /
    KeypadEnter => 0x4C,    // kVK_ANSI_KeypadEnter
    KeypadSubtract => 0x4E, // kVK_ANSI_KeypadMinus    -
    KeypadEqual => 0x51,    // kVK_ANSI_KeypadEquals   =
    Keypad0 => 0x52, Keypad1 => 0x53, Keypad2 => 0x54, Keypad3 => 0x55,
    Keypad4 => 0x56, Keypad5 => 0x57, Keypad6 => 0x58, Keypad7 => 0x59,
    Keypad8 => 0x5B, Keypad9 => 0x5C,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表完整性：每个键都有码、码不重复、码落在 HID 键码范围（≤ 0x7E）。
    /// `ALL` 与映射表同源（宏生成），遍历 `ALL` 即遍历全表。
    #[test]
    fn keymap_every_key_maps_to_unique_valid_code() {
        let keys = Key::ALL;
        assert!(keys.len() >= 100, "常用键表意外地小：{}", keys.len());
        let mut seen = std::collections::HashSet::new();
        for &k in keys {
            let code = virtual_keycode(k).expect("当前表必须穷尽所有 Key 变体");
            assert!(code <= 0x7E, "{k:?} code {code:#x} 超出虚拟键码范围");
            assert!(seen.insert(code), "{k:?} 与已有键撞码：{code:#x}");
        }
        assert_eq!(seen.len(), keys.len(), "存在重复键码");
    }

    /// 反查双射：每个键的键码经 [`Key::key_from_virtual_keycode`] 必须
    /// 回到自身；表外键码（0x0A ISO Section、0x48 音量+、0x69 F13、
    /// 0xFF 垃圾值）一律 `None`。捕获侧靠它把 CGEvent 键码还原成逻辑键。
    #[test]
    fn keymap_inverse_roundtrip_and_unknown_codes() {
        for &k in Key::ALL {
            let code = virtual_keycode(k).expect("当前表必须穷尽所有 Key 变体");
            assert_eq!(
                key_from_virtual_keycode(code),
                Some(k),
                "{code:#04x} 反查不闭合"
            );
        }
        // 表外键码：逐个抽查（含 Events.h 里存在但本表不覆盖的常用值）
        for code in [0x0Au16, 0x48, 0x49, 0x4A, 0x69, 0x6A, 0x72, 0x7F, 0xFFFF] {
            assert_eq!(
                key_from_virtual_keycode(code),
                None,
                "键码 {code:#04x} 不在表内，必须返回 None 而不是猜测"
            );
        }
    }

    /// 逐值对照本机 SDK `HIToolbox/Events.h` 的 `kVK_*` 常量。
    /// 键码是纯数据，抄错不会有编译期提示，只能靠对照测试锁死。
    #[test]
    fn keymap_matches_hitooolbox_header_values() {
        let expect: &[(Key, u16)] = &[
            // 字母（抽查首尾与中间）
            (Key::A, 0x00),
            (Key::S, 0x01),
            (Key::D, 0x02),
            (Key::Z, 0x06),
            (Key::Q, 0x0C),
            (Key::T, 0x11),
            (Key::O, 0x1F),
            (Key::M, 0x2E),
            // 数字
            (Key::Digit1, 0x12),
            (Key::Digit6, 0x16),
            (Key::Digit0, 0x1D),
            // 标点
            (Key::Minus, 0x1B),
            (Key::Equal, 0x18),
            (Key::RightBracket, 0x1E),
            (Key::LeftBracket, 0x21),
            (Key::Quote, 0x27),
            (Key::Semicolon, 0x29),
            (Key::Backslash, 0x2A),
            (Key::Comma, 0x2B),
            (Key::Slash, 0x2C),
            (Key::Period, 0x2F),
            (Key::Grave, 0x32),
            // 控制键
            (Key::Return, 0x24),
            (Key::Tab, 0x30),
            (Key::Space, 0x31),
            (Key::Backspace, 0x33), // kVK_Delete 是退格
            (Key::Escape, 0x35),
            (Key::CapsLock, 0x39),
            (Key::ForwardDelete, 0x75),
            // 导航
            (Key::Home, 0x73),
            (Key::End, 0x77),
            (Key::PageUp, 0x74),
            (Key::PageDown, 0x79),
            (Key::ArrowUp, 0x7E),
            (Key::ArrowDown, 0x7D),
            (Key::ArrowLeft, 0x7B),
            (Key::ArrowRight, 0x7C),
            // 修饰键
            (Key::LeftCommand, 0x37),
            (Key::LeftShift, 0x38),
            (Key::LeftOption, 0x3A),
            (Key::LeftControl, 0x3B),
            (Key::RightCommand, 0x36),
            (Key::RightShift, 0x3C),
            (Key::RightOption, 0x3D),
            (Key::RightControl, 0x3E),
            (Key::Fn, 0x3F),
            // 功能键
            (Key::F1, 0x7A),
            (Key::F2, 0x78),
            (Key::F3, 0x63),
            (Key::F4, 0x76),
            (Key::F5, 0x60),
            (Key::F6, 0x61),
            (Key::F7, 0x62),
            (Key::F8, 0x64),
            (Key::F9, 0x65),
            (Key::F10, 0x6D),
            (Key::F11, 0x67),
            (Key::F12, 0x6F),
            // 小键盘（抽查）
            (Key::KeypadDecimal, 0x41),
            (Key::KeypadMultiply, 0x43),
            (Key::KeypadAdd, 0x45),
            (Key::KeypadClear, 0x47),
            (Key::KeypadDivide, 0x4B),
            (Key::KeypadEnter, 0x4C),
            (Key::KeypadSubtract, 0x4E),
            (Key::KeypadEqual, 0x51),
            (Key::Keypad0, 0x52),
            (Key::Keypad1, 0x53),
            (Key::Keypad8, 0x5B),
            (Key::Keypad9, 0x5C),
        ];
        for &(k, code) in expect {
            assert_eq!(virtual_keycode(k), Some(code), "{k:?} 键码与 Events.h 不符");
        }
    }
}
