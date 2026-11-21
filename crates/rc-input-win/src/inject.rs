//! `SendInput` 注入骨架（仅 Windows；真机实现属 D 阶段）。
//!
//! 声明 `winuser.h` 的 [`SendInput`] 与 [`GetCursorPos`]（I10 光标
//! 校准）及 `INPUT` 结构布局。注入器 [`SendInputInjector`] 已实现
//! `rc_input::InputInjector` 的**错误路径骨架**：调用即返回显式
//! `Err`（I9：失败返回错误、不 panic），D 阶段填充真实逻辑。

// 本模块是 crate 内唯一触碰 FFI 的地方（模式照 rc-input-macos 的 ffi）。
// extern 声明本身按任务卡注记属「安全 API 声明」（SendInput 是安全 API）。
#![allow(unsafe_code)]
// 骨架：常量与结构供 D 阶段实现使用，当前仅声明未引用。
#![allow(dead_code)]

use rc_input::{InputEvent, InputInjector};
use rc_types::{Error, Result};

// ── winuser.h 常量（D 阶段实现用）──────────────────────────────

/// `INPUT.type`：鼠标事件。
pub const INPUT_MOUSE: u32 = 0;
/// `INPUT.type`：键盘事件。
pub const INPUT_KEYBOARD: u32 = 1;

/// 鼠标：用 dx/dy（配合 `MOUSEEVENTF_ABSOLUTE`）。
pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
/// 鼠标：坐标是绝对归一化 0..65535（与 rc-input 的 I4 坐标系一致，
/// `SetCursorPos` 语义同源，无需换算）。
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
/// 鼠标：左键按下 / 抬起。
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
/// 鼠标：左键抬起。
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
/// 鼠标：右键按下 / 抬起。
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
/// 鼠标：右键抬起。
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
/// 鼠标：中键按下 / 抬起。
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
/// 鼠标：中键抬起。
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
/// 鼠标：X 键（X1/X2 由 `mouseData` 的高位区分：X1 = 0x0001, X2 = 0x0002）。
pub const MOUSEEVENTF_XDOWN: u32 = 0x0080;
/// 鼠标：X 键抬起。
pub const MOUSEEVENTF_XUP: u32 = 0x0100;
/// 鼠标：垂直滚轮（`mouseData` 为 wheel delta，正 = 向上）。
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
/// 鼠标：水平滚轮。
pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;

/// 键盘：`wScan` 是扫描码（I5：不用虚拟键码）。
pub const KEYEVENTF_SCANCODE: u32 = 0x0008;
/// 键盘：E0 前缀键（右 Ctrl / 右 Alt / 方向键等，rc-input 编码 `0xE0xx`）。
pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
/// 键盘：抬起。
pub const KEYEVENTF_KEYUP: u32 = 0x0002;

// ── 结构布局（winuser.h，x64）─────────────────────────────────

/// 屏幕点（`GetCursorPos`，I10 校准用）。
#[derive(Clone, Copy)]
#[repr(C)]
#[allow(non_snake_case)]
pub struct POINT {
    /// x 坐标（像素，虚拟桌面坐标系，可为负——多显示器副屏在负象限）。
    pub x: i32,
    /// y 坐标（像素）。
    pub y: i32,
}

/// `MOUSEINPUT`（`DWORD_PTR` 在 x64 上是 `usize`）。`Copy` 是进入
/// [`INPUT_UNION`] 的前提（Rust union 字段必须 `Copy`）。
#[derive(Clone, Copy)]
#[repr(C)]
#[allow(non_snake_case)]
pub struct MOUSEINPUT {
    /// 归一化 x（`MOUSEEVENTF_ABSOLUTE` 时 0..65535）。
    pub dx: i32,
    /// 归一化 y。
    pub dy: i32,
    /// 滚轮量 / X 键编号。
    pub mouseData: usize,
    /// 事件标志（`MOUSEEVENTF_*`）。
    pub dwFlags: u32,
    /// 时间戳（0 = 系统填充）。
    pub time: u32,
    /// 附加信息（0）。
    pub dwExtraInfo: usize,
}

/// `KEYBDINPUT`。`Copy` 是进入 [`INPUT_UNION`] 的前提。
#[derive(Clone, Copy)]
#[repr(C)]
#[allow(non_snake_case)]
pub struct KEYBDINPUT {
    /// 虚拟键码（用 `KEYEVENTF_SCANCODE` 时填 0）。
    pub wVk: u16,
    /// 扫描码（Set 1；E0 键只放低 8 位 + `KEYEVENTF_EXTENDEDKEY`）。
    pub wScan: u16,
    /// 事件标志（`KEYEVENTF_*`）。
    pub dwFlags: u32,
    /// 时间戳（0 = 系统填充）。
    pub time: u32,
    /// 附加信息（0）。
    pub dwExtraInfo: usize,
}

/// `INPUT` 的联合体部分。
#[repr(C)]
#[allow(non_snake_case)]
pub union INPUT_UNION {
    /// 鼠标事件。
    pub mi: MOUSEINPUT,
    /// 键盘事件。
    pub ki: KEYBDINPUT,
}

/// `SendInput` 的输入单元（winuser.h `INPUT`）。
#[repr(C)]
#[allow(non_snake_case)]
pub struct INPUT {
    /// `INPUT_MOUSE` / `INPUT_KEYBOARD`。
    pub r#type: u32,
    /// 事件载荷（联合体）。
    pub U: INPUT_UNION,
}

// ── FFI 声明（user32）─────────────────────────────────────────

#[link(name = "user32")]
extern "system" {
    /// `UINT SendInput(UINT cInputs, LPINPUT pInputs, int cbSize)`：
    /// 注入一组输入事件。返回**成功注入的条数**——不满 `cInputs` 即
    /// 部分失败，必须当错误处理（I9），不能静默忽略。
    pub fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: i32) -> u32;
    /// `BOOL GetCursorPos(LPPOINT)`：读回光标位置。I10：注入移动后
    /// 读取并校准，防系统侧（加速曲线 / 游戏重设）造成的光标漂移。
    pub fn GetCursorPos(lppoint: *mut POINT) -> i32;
}

// ── 注入器骨架 ────────────────────────────────────────────────

/// `SendInput` 注入器骨架（D 阶段实现）。
///
/// 当前 [`InputInjector::inject`] 恒返回 `Err`——骨架不假装能注入。
pub struct SendInputInjector {
    screen_w: u32,
    screen_h: u32,
}

impl SendInputInjector {
    /// 构造骨架。
    pub fn new() -> Self {
        Self {
            screen_w: 0,
            screen_h: 0,
        }
    }
}

impl Default for SendInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
        // TODO(D 阶段)：
        // 1. 事件 → INPUT[]：MouseMove 用 MOUSEEVENTF_ABSOLUTE（坐标即
        //    rc-input 的归一化值，直传）；Key 拆 scancode 低 8 位 +
        //    KEYEVENTF_EXTENDEDKEY（0xE0xx 高字节）；滚轮换算 120/格。
        // 2. SendInput(...)，返回值 < 条数 → Err（I9）。
        // 3. 注入移动后 GetCursorPos 校准（I10，任务卡坑 3：虚拟桌面
        //    坐标系可为负）。
        // 4. UAC 提权窗口需 SYSTEM 权限（任务卡坑 2）——注入被挡时
        //    返回显式错误并上报 UI。
        // 5. 已知事实：注入事件带 LLMHF_INJECTED 标记，不做反检测（红线）。
        Err(Error::InvalidConfig(format!(
            "SendInput injection not implemented (D-phase); screen {}x{}",
            self.screen_w, self.screen_h
        )))
    }

    fn set_screen_size(&mut self, w: u32, h: u32) {
        self.screen_w = w;
        self.screen_h = h;
    }
}
