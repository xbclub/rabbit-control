//! macOS 输入捕获器（仅 macOS）：CGEventTap → `rc_input::InputCapture`。
//!
//! 消除「macOS 侧有注入无捕获」的空档——本模块与 [`Injector`]（注入）
//! 成对：控制端用 [`CgCapture`] 把本机键鼠事件转成 rc-input 的归一化
//! 事件流，受控端用 [`InputBridge`](crate::bridge::InputBridge) 还原。
//!
//! # 结构（与注入侧同构：纯函数 + FFI 边界）
//!
//! | 层 | 文件 | 职责 |
//! |---|---|---|
//! | 纯映射 | 本模块 [`raw_to_input`] 等 | RawCgEvent → `rc_input::InputEvent`（归一化 / 键码 / 修饰键 / 滚轮符号），无授权也能单测 |
//! | FFI | [`ffi::tap`](crate::ffi) | CGEventTap 线程 / CFRunLoop / 回调抽取 / 重启用，**全部 unsafe 收口** |
//! | 组装 | 本模块 [`CgCapture`] | 权限预检 + 通道泵（`next_events`） |
//!
//! # 权限（沿用注入器的模式，绝不静默空转）
//!
//! 监听 tap 同样需要授权（辅助功能，或较弱的输入监控）。未授权有两种
//! 暗坑：`CGEventTapCreate` 返回 NULL（可见，映射 [`InputError::TapCreateFailed`]），
//! 或创建成功但收不到任何事件。[`CgCapture::new`] 先 `AXIsProcessTrusted()`
//! 预检（未授权直接 [`InputError::NotTrusted`]，附系统设置路径），再等
//! tap 线程「创建 + 挂源 + enable」握手成功才返回——两条坑都堵死。
//!
//! # 映射约定（与注入侧严格互逆，保证回环闭合）
//!
//! - **坐标**：`CGEventGetLocation` 的全局点坐标，先平移进
//!   [`ScreenBounds`]（全部在线显示器的联合边界——与注入桥
//!   [`InputBridge::map_point`](crate::bridge::InputBridge) 的定义域一致，
//!   单显示器时即主屏），再用 [`rc_input::normalize_coord`] 归一化到
//!   `0..=65535`；
//! - **键**：虚拟键码经 [`key_from_virtual_keycode`](crate::keymap) 还原
//!   逻辑键，再经 [`set1_scancode`](crate::scancode) 转扫描码。表缺的键
//!   （`Fn` / `KeypadEqual` 有键无码；F13–F20、音量/亮度/媒体、JIS 专属、
//!   ISO Section 无 `Key` 变体）**显式丢弃并计数**
//!   （[`CgCapture::dropped_unmapped`]），绝不编造映射；
//! - **修饰键**：快照取 `kCGEventSourceFlagsState(HIDSystemState)`（它不
//!   分左右侧，一律先落在左位；事件本身是左/右修饰键时再精确修正到
//!   对应侧，见 [`refine_modifier_side`]）。macOS 修饰键不产生 keyDown/
//!   keyUp 而是产生 `flagsChanged`，按下与否由事件 flags 里对应位的新
//!   状态判定（[`flags_changed_pressed`]）；
//! - **滚轮**：`Axis1`（垂直，向上为正）直传 `delta`；`Axis2`（水平，
//!   向左为正）取负——rc-input 约定水平 `delta > 0` 为向右。单位是
//!   CG 行单位，即 rc-input 线格式 `MouseWheel.delta` 的规范单位
//!   （见 `rc_input::InputEvent::MouseWheel`；注入侧按同单位
//!   `ScrollEventUnit::LINE` 投递，回环对称）。
//!
//! # 生命周期
//!
//! 构造时采样屏幕边界；显示器热插拔后应**重建**捕获器（同注入器约定）。
//! [`Drop`] 置位 stop 标志，tap 线程在 ≤100ms 内自退（不走 `CFRunLoopStop`，
//! 见 `ffi::tap` 模块文档的竞态说明）。

use crate::error::InputError;
use crate::ffi;
use crate::keymap::{key_from_virtual_keycode, Key};
use crate::plan::ScreenBounds;
use crate::scancode::set1_scancode;
use rc_input::batch::modifier_bits;
use rc_input::{normalize_coord, InputCapture, InputEvent, MouseButton};
use rc_types::{Error, Result};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ─────────────────────────────────────────────────────────────────
// 回调线程送来的纯数据（ffi::tap 抽取）
// ─────────────────────────────────────────────────────────────────

/// 事件的粗分类（从 `CGEventType` 归并而来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawKind {
    /// 指针移动（含三种 Dragged：按住按键拖动也是移动）。
    MouseMove,
    /// 按下（左 / 右 / 中 / 侧键，具体看 `button`）。
    MouseDown,
    /// 抬起。
    MouseUp,
    /// 滚轮（`wheel1` 垂直 / `wheel2` 水平）。
    Scroll,
    /// 普通键按下。
    KeyDown,
    /// 普通键抬起。
    KeyUp,
    /// 修饰键状态变化（macOS 修饰键不产生 keyDown/keyUp）。
    FlagsChanged,
}

/// 回调线程抽取的 CGEvent 纯数据快照（FFI 边界与映射层的交接类型）。
///
/// 只含原始数值，不做任何翻译——翻译全部在 [`raw_to_input`]（纯函数），
/// 因此映射逻辑无需授权、无需 CGEvent 即可单测。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RawCgEvent {
    /// 事件分类。
    pub kind: RawKind,
    /// 全局坐标 x（点，原点 = 主屏左上角，副屏可为负）。仅鼠标类事件有效。
    pub x: f64,
    /// 全局坐标 y。
    pub y: f64,
    /// 按钮号（CG 编号：0=左 1=右 2=中 3=X1 4=X2，与 `MouseButton` 一致）。
    pub button: i64,
    /// 虚拟键码（`kVK_*`）。仅 KeyDown/KeyUp/FlagsChanged 有效。
    pub keycode: u16,
    /// 滚轮 Axis1：垂直滚动量（行单位，向上为正）。
    pub wheel1: i64,
    /// 滚轮 Axis2：水平滚动量（行单位，向左为正）。
    pub wheel2: i64,
    /// 事件自带 flags 位集（`CGEventFlags`，FlagsChanged 判按下用）。
    pub event_flags: u64,
    /// `kCGEventSourceFlagsState(HIDSystemState)` 快照（Key 事件修饰键字段用）。
    pub state_flags: u64,
}

// ─────────────────────────────────────────────────────────────────
// 纯映射函数（全部可离线单测）
// ─────────────────────────────────────────────────────────────────

/// `CGEventFlags` 的设备无关修饰键位（CGEventTypes.h，ABI 稳定值；
/// 离线测试与 core-graphics 的 `CGEventFlags` 常量互锁）。
///
/// NUMERIC_PAD / HELP / NON_COALESCED 生产路径不消费（rc 修饰键位没有
/// 对应），只在与 CGEventFlags 的互锁测试里出现——完整位表保留作对照。
#[cfg_attr(not(test), allow(dead_code))]
mod cg_flags {
    /// CapsLock（AlphaShift）。
    pub const ALPHA_SHIFT: u64 = 0x0001_0000;
    /// Shift（不分左右）。
    pub const SHIFT: u64 = 0x0002_0000;
    /// Control（不分左右）。
    pub const CONTROL: u64 = 0x0004_0000;
    /// Option/Alt（不分左右）。
    pub const ALTERNATE: u64 = 0x0008_0000;
    /// Command（不分左右）。
    pub const COMMAND: u64 = 0x0010_0000;
    /// 小键盘区标志。
    pub const NUMERIC_PAD: u64 = 0x0020_0000;
    /// Help 键。
    pub const HELP: u64 = 0x0040_0000;
    /// Fn 键。
    pub const SECONDARY_FN: u64 = 0x0080_0000;
    /// 事件未被合并标志。
    pub const NON_COALESCED: u64 = 0x0000_0100;
}

/// `kCGEventSourceFlagsState` 快照 → rc-input 修饰键位集。
///
/// CG 的 flags **不分左右侧**，generic 位一律落在左位（约定）；
/// 事件本身是哪侧修饰键由 [`refine_modifier_side`] 修正。
/// AlphaShift / NumericPad / Help / SecondaryFn / NonCoalesced 在
/// rc-input 的 8 个修饰键位里没有对应，丢弃。
pub(crate) fn modifiers_from_state_flags(flags: u64) -> u8 {
    let mut m = 0u8;
    if flags & cg_flags::SHIFT != 0 {
        m |= modifier_bits::LEFT_SHIFT;
    }
    if flags & cg_flags::CONTROL != 0 {
        m |= modifier_bits::LEFT_CTRL;
    }
    if flags & cg_flags::ALTERNATE != 0 {
        m |= modifier_bits::LEFT_ALT;
    }
    if flags & cg_flags::COMMAND != 0 {
        m |= modifier_bits::LEFT_GUI;
    }
    m
}

/// 修饰键事件对快照的左右侧修正。
///
/// generic 位落在左位只是占位；本事件就是某个左/右修饰键时，按实际
/// 侧别重摆该对位（清左/右两位后按 `pressed` 置实际侧）。非修饰键
/// 原样返回。
pub(crate) fn refine_modifier_side(bits: u8, key: Key, pressed: bool) -> u8 {
    use Key::*;
    let pair = match key {
        LeftControl | RightControl => (modifier_bits::LEFT_CTRL, modifier_bits::RIGHT_CTRL),
        LeftShift | RightShift => (modifier_bits::LEFT_SHIFT, modifier_bits::RIGHT_SHIFT),
        LeftOption | RightOption => (modifier_bits::LEFT_ALT, modifier_bits::RIGHT_ALT),
        LeftCommand | RightCommand => (modifier_bits::LEFT_GUI, modifier_bits::RIGHT_GUI),
        _ => return bits,
    };
    let side = matches!(key, RightControl | RightShift | RightOption | RightCommand);
    let mut m = bits & !(pair.0 | pair.1);
    if pressed {
        m |= if side { pair.1 } else { pair.0 };
    }
    m
}

/// 该逻辑键对应的 `CGEventFlags` 位（FlagsChanged 的按下判定依据）。
///
/// 只有会改变 flags 位的键（八个修饰键 + CapsLock + Fn）返回 `Some`；
/// 其余键不会以 FlagsChanged 形式出现，返回 `None`（调用方丢弃）。
pub(crate) fn flags_bit_for_modifier_key(key: Key) -> Option<u64> {
    use Key::*;
    match key {
        LeftShift | RightShift => Some(cg_flags::SHIFT),
        LeftControl | RightControl => Some(cg_flags::CONTROL),
        LeftOption | RightOption => Some(cg_flags::ALTERNATE),
        LeftCommand | RightCommand => Some(cg_flags::COMMAND),
        CapsLock => Some(cg_flags::ALPHA_SHIFT),
        Fn => Some(cg_flags::SECONDARY_FN),
        _ => None,
    }
}

/// flagsChanged 事件的按下状态：事件 flags 是**变化后的新位集**，
/// 对应位亮着 = 该修饰键现在是按下。
pub(crate) fn flags_changed_pressed(key: Key, event_flags: u64) -> Option<bool> {
    let bit = flags_bit_for_modifier_key(key)?;
    Some(event_flags & bit != 0)
}

/// 全局点坐标 → 归一化 `0..=65535`（相对 `bounds` 联合边界）。
///
/// 与注入桥 `InputBridge::map_point` 严格互逆：`0 ↦ bounds 左上`、
/// `extent-1 ↦ 65535`。越界坐标（显示器热插拔间的瞬时窗口）钳进边界。
pub(crate) fn normalize_point(x: f64, y: f64, b: ScreenBounds) -> (u16, u16) {
    let w = b.width.max(1.0).round();
    let h = b.height.max(1.0).round();
    let lx = (x - b.x).round().clamp(0.0, (w - 1.0).max(0.0));
    let ly = (y - b.y).round().clamp(0.0, (h - 1.0).max(0.0));
    (
        normalize_coord(lx as u32, w as u32),
        normalize_coord(ly as u32, h as u32),
    )
}

/// CG 按钮号 → `MouseButton`。未知按钮号（≥5，某些多键鼠标）返回 `None`。
fn cg_button_to_mouse(button: i64) -> Option<MouseButton> {
    match button {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Right),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::X1),
        4 => Some(MouseButton::X2),
        _ => None,
    }
}

/// `i64` → `i16` 钳制（滚轮量；异常巨大值钳边界而不是截断错乱）。
fn clamp_i16(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

/// RawCgEvent → `rc_input::InputEvent`（纯函数，本模块的核心）。
///
/// 返回 `None` = 无法忠实翻译（表外键码 / 未知按钮号 / 双轴零滚动 /
/// 非修饰键的 flagsChanged），调用方丢弃并计数——绝不编造映射。
///
/// 已知取舍：滚轮事件两轴同亮（触控板对角线滚动）时只取绝对值更大的
/// 主轴——rc-input 的 `MouseWheel` 单事件只装一个轴。
pub(crate) fn raw_to_input(raw: &RawCgEvent, bounds: ScreenBounds) -> Option<InputEvent> {
    match raw.kind {
        RawKind::MouseMove => {
            let (x, y) = normalize_point(raw.x, raw.y, bounds);
            Some(InputEvent::MouseMove { x, y })
        }
        RawKind::MouseDown | RawKind::MouseUp => {
            let button = cg_button_to_mouse(raw.button)?;
            Some(InputEvent::MouseButton {
                button,
                pressed: raw.kind == RawKind::MouseDown,
            })
        }
        RawKind::Scroll => {
            if raw.wheel1 == 0 && raw.wheel2 == 0 {
                return None; // 零滚动是噪声
            }
            if raw.wheel1.abs() >= raw.wheel2.abs() {
                Some(InputEvent::MouseWheel {
                    delta: clamp_i16(raw.wheel1),
                    horizontal: false,
                })
            } else {
                // CG Axis2 向左为正；rc-input 约定向右为正 → 取负。
                Some(InputEvent::MouseWheel {
                    delta: clamp_i16(-raw.wheel2),
                    horizontal: true,
                })
            }
        }
        RawKind::KeyDown | RawKind::KeyUp => {
            let key = key_from_virtual_keycode(raw.keycode)?;
            let pressed = raw.kind == RawKind::KeyDown;
            let mut modifiers = modifiers_from_state_flags(raw.state_flags);
            modifiers = refine_modifier_side(modifiers, key, pressed);
            let scancode = set1_scancode(key)?;
            Some(InputEvent::Key {
                scancode,
                pressed,
                modifiers,
            })
        }
        RawKind::FlagsChanged => {
            let key = key_from_virtual_keycode(raw.keycode)?;
            let pressed = flags_changed_pressed(key, raw.event_flags)?;
            let mut modifiers = modifiers_from_state_flags(raw.state_flags);
            modifiers = refine_modifier_side(modifiers, key, pressed);
            let scancode = set1_scancode(key)?;
            Some(InputEvent::Key {
                scancode,
                pressed,
                modifiers,
            })
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 捕获器本体
// ─────────────────────────────────────────────────────────────────

/// macOS 输入捕获器：`CGEventTap`（Session + HeadInsert + ListenOnly，
/// 专用线程跑 CFRunLoop）→ `rc_input::InputCapture`。
///
/// ListenOnly 只听不改，绝不拦截 / 吞掉本机事件。构造见 [`CgCapture::new`]
/// 的权限说明；用法：
/// ```no_run
/// # fn demo() -> rc_types::Result<()> {
/// use rc_input::InputCapture;
/// let mut cap = rc_input_macos::CgCapture::new()
///     .map_err(|e| rc_types::Error::InvalidConfig(e.to_string()))?;
/// let events = cap.next_events(std::time::Duration::from_millis(4))?;
/// # let _ = events;
/// # Ok(())
/// # }
/// ```
pub struct CgCapture {
    /// tap 线程的事件接收端（有界通道的共享句柄：tap 回调满载时经它
    /// 丢最旧腾位，见 `ffi::tap` 的背压说明）。
    rx: Arc<Mutex<std::sync::mpsc::Receiver<RawCgEvent>>>,
    /// 线程控制块（Drop 置 stop；含通道满丢弃计数）。
    ctl: Arc<ffi::TapCtl>,
    /// 构造时采样的联合边界（归一化定义域）。
    bounds: ScreenBounds,
    /// 无法翻译被丢弃的事件数（表外键码等，诊断用）。
    dropped_unmapped: u64,
}

impl CgCapture {
    /// 生产构造：`AXIsProcessTrusted` 预检 → 屏幕联合边界 → 启动 tap 线程
    /// 并等待「创建 + 挂源 + enable」握手成功。
    ///
    /// 未授权立即返回 [`InputError::NotTrusted`]（附系统设置路径）；
    /// tap 创建失败（`CGEventTapCreate` NULL）返回
    /// [`InputError::TapCreateFailed`]——两种失败都不会带着假捕获器静默
    /// 空转。注意授权后必须**重启进程**才生效（macOS 限制）。
    pub fn new() -> Result<Self, InputError> {
        if !ffi::ax_is_process_trusted() {
            return Err(InputError::NotTrusted);
        }
        let bounds = ffi::screen_bounds();
        let (rx, ctl) = ffi::spawn_tap_thread()?;
        Ok(Self {
            rx,
            ctl,
            bounds,
            dropped_unmapped: 0,
        })
    }

    /// 测试组装（不碰系统）：直接注入接收端与控制块。
    #[cfg(test)]
    fn from_parts(
        rx: Arc<Mutex<std::sync::mpsc::Receiver<RawCgEvent>>>,
        ctl: Arc<ffi::TapCtl>,
        bounds: ScreenBounds,
    ) -> Self {
        Self {
            rx,
            ctl,
            bounds,
            dropped_unmapped: 0,
        }
    }

    /// 当前归一化定义域（构造时采样的联合边界）。
    pub fn screen_bounds(&self) -> ScreenBounds {
        self.bounds
    }

    /// 因「表缺映射」被丢弃的事件累计数（诊断：键表覆盖缺口一目了然）。
    pub fn dropped_unmapped(&self) -> u64 {
        self.dropped_unmapped
    }

    /// 因**事件通道满**被丢弃的事件累计数（诊断：消费侧跟不上输入风暴
    /// 时的丢失量）。与 [`CgCapture::dropped_unmapped`] 是两个独立的
    /// 诊断面——前者是背压丢弃（tap 回调绝不能阻塞，只能丢），后者是
    /// 映射层丢弃（表外键码等）。
    pub fn dropped_channel_full(&self) -> u64 {
        self.ctl.dropped_channel_full()
    }

    /// 泵一批事件：阻塞最多 `timeout` 等第一条，其后无阻塞收干。
    ///
    /// 持接收端锁期间回调侧满载时只能降级丢当前事件（见 `send_drop_oldest`），
    /// 但锁只在无事件时被 `recv_timeout` 长持——那时通道是空的，不存在
    /// 满载丢弃；有事件时 `recv_timeout` 立即返回、收干后尽快放锁。
    fn drain(&mut self, timeout: Duration) -> std::result::Result<Vec<InputEvent>, InputError> {
        let mut out = Vec::new();
        // 持锁线程 panic 只会留下 poison：通道本身无不可恢复状态，
        // 取回内部 Receiver 继续用即可（丢锁即自愈，比 panic 传播更适合
        // 捕获这种长生命周期组件）。
        let rx = self
            .rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match rx.recv_timeout(timeout) {
            Ok(first) => {
                if let Some(ev) = raw_to_input(&first, self.bounds) {
                    out.push(ev);
                } else {
                    self.dropped_unmapped += 1;
                }
                while let Ok(raw) = rx.try_recv() {
                    if let Some(ev) = raw_to_input(&raw, self.bounds) {
                        out.push(ev);
                    } else {
                        self.dropped_unmapped += 1;
                    }
                }
                Ok(out)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(out),
            // 通道断开 = tap 线程已退出。显式报错而不是返回空表——
            // 空表会被上层当成「无输入」，掩盖捕获器死亡。
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(InputError::TapStopped),
        }
    }
}

impl InputCapture for CgCapture {
    fn next_events(&mut self, timeout: Duration) -> Result<Vec<InputEvent>> {
        self.drain(timeout)
            .map_err(|e| Error::InvalidConfig(format!("macOS capture failed: {e}")))
    }
}

impl Drop for CgCapture {
    fn drop(&mut self) {
        // 置位后 tap 线程在一个 ≤100ms 分片内自退（见 ffi::tap 文档）。
        self.ctl
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::batch::modifier_bits as b;

    fn raw(kind: RawKind) -> RawCgEvent {
        RawCgEvent {
            kind,
            x: 0.0,
            y: 0.0,
            button: 0,
            keycode: 0,
            wheel1: 0,
            wheel2: 0,
            event_flags: 0,
            state_flags: 0,
        }
    }

    // ── 掩码（在 ffi::tap 测试）／ 归一化 ──────────────────────────

    /// 归一化端点对齐 + 与注入桥 map_point 的往返误差 ≤ 1px（点坐标）。
    /// 桥的映射：origin + (n/65535)*(size-1)；本函数是其逆。
    #[test]
    fn normalize_point_aligns_endpoints_and_roundtrips_bridge() {
        let bounds = ScreenBounds::new(-1920.0, 0.0, 4480.0, 1080.0);
        // 端点：联合边界左上 ↦ (0,0)，右下角 ↦ (65535,65535)
        assert_eq!(normalize_point(-1920.0, 0.0, bounds), (0, 0));
        let (x, y) = normalize_point(-1920.0 + 4479.0, 1079.0, bounds);
        assert_eq!((x, y), (65535, 65535));
        // 中心往返（桥公式手动复算）
        for n in [0u16, 1, 16384, 32768, 49152, 65535] {
            let px = -1920.0 + (f64::from(n) / 65535.0) * 4479.0;
            let py = (f64::from(n) / 65535.0) * 1079.0;
            let back = normalize_point(px, py, bounds);
            // 每轴误差 ≤ 65535/(4480) ≈ 15 与 65535/1080 ≈ 61 个归一化单位
            //（1px 量化）。放宽到 ±64 覆盖两轴。
            assert!(
                (i32::from(back.0) - i32::from(n)).unsigned_abs() <= 64,
                "x: {n} -> {back:?}"
            );
            assert!(
                (i32::from(back.1) - i32::from(n)).unsigned_abs() <= 64,
                "y: {n} -> {back:?}"
            );
        }
        // 越界钳制（热插拔瞬窗）：负象限外与右下外都钳进边界
        assert_eq!(normalize_point(-99999.0, -99999.0, bounds).0, 0);
        assert_eq!(normalize_point(99999.0, 99999.0, bounds), (65535, 65535));
    }

    // ── 修饰键 ─────────────────────────────────────────────────────

    /// cg_flags 位值与 core-graphics 的 CGEventFlags 常量互锁（抄错立刻爆）。
    #[test]
    fn cg_flag_bits_match_core_graphics_constants() {
        use core_graphics::event::CGEventFlags as F;
        assert_eq!(cg_flags::ALPHA_SHIFT, F::CGEventFlagAlphaShift.bits());
        assert_eq!(cg_flags::SHIFT, F::CGEventFlagShift.bits());
        assert_eq!(cg_flags::CONTROL, F::CGEventFlagControl.bits());
        assert_eq!(cg_flags::ALTERNATE, F::CGEventFlagAlternate.bits());
        assert_eq!(cg_flags::COMMAND, F::CGEventFlagCommand.bits());
        assert_eq!(cg_flags::NUMERIC_PAD, F::CGEventFlagNumericPad.bits());
        assert_eq!(cg_flags::HELP, F::CGEventFlagHelp.bits());
        assert_eq!(cg_flags::SECONDARY_FN, F::CGEventFlagSecondaryFn.bits());
        assert_eq!(cg_flags::NON_COALESCED, F::CGEventFlagNonCoalesced.bits());
    }

    /// 快照映射：四个 generic 位落左位，rc 没有的位（CapsLock/Fn/小键盘…）
    /// 不产生任何 rc 修饰键位。
    #[test]
    fn modifiers_from_state_flags_maps_generic_bits_to_left() {
        assert_eq!(modifiers_from_state_flags(0), 0);
        let all = cg_flags::SHIFT | cg_flags::CONTROL | cg_flags::ALTERNATE | cg_flags::COMMAND;
        assert_eq!(
            modifiers_from_state_flags(all),
            b::LEFT_SHIFT | b::LEFT_CTRL | b::LEFT_ALT | b::LEFT_GUI
        );
        // rc 无对应位：全开也不多出一个位
        assert_eq!(
            modifiers_from_state_flags(
                cg_flags::ALPHA_SHIFT
                    | cg_flags::SECONDARY_FN
                    | cg_flags::NUMERIC_PAD
                    | cg_flags::HELP
                    | cg_flags::NON_COALESCED
            ),
            0
        );
    }

    /// 左右侧修正：右 Shift 按下把 generic 左位挪到右位；抬起清两位；
    /// 非修饰键原样透传。
    #[test]
    fn refine_modifier_side_moves_generic_bit_to_actual_side() {
        let base = b::LEFT_SHIFT | b::LEFT_CTRL; // Shift+Ctrl 同时按下的 generic 快照
                                                 // 本事件是右 Shift 按下：Shift 对修正到右位，Ctrl 留在左位
        assert_eq!(
            refine_modifier_side(base, Key::RightShift, true),
            b::RIGHT_SHIFT | b::LEFT_CTRL
        );
        // 左 Shift 按下：保持左位
        assert_eq!(
            refine_modifier_side(base, Key::LeftShift, true),
            b::LEFT_SHIFT | b::LEFT_CTRL
        );
        // 修饰键抬起：清掉该对两位（Ctrl 不受影响）
        assert_eq!(
            refine_modifier_side(base, Key::RightShift, false),
            b::LEFT_CTRL
        );
        // 非修饰键：不动
        assert_eq!(refine_modifier_side(base, Key::Space, true), base);
    }

    /// flagsChanged 的按下判定：位亮 = 按下；非修饰键键码返回 None。
    #[test]
    fn flags_changed_pressed_reads_new_state() {
        let on = cg_flags::SHIFT;
        assert_eq!(flags_changed_pressed(Key::LeftShift, on), Some(true));
        assert_eq!(flags_changed_pressed(Key::RightShift, 0), Some(false));
        assert_eq!(
            flags_changed_pressed(Key::CapsLock, cg_flags::ALPHA_SHIFT),
            Some(true)
        );
        assert_eq!(flags_changed_pressed(Key::Fn, 0), Some(false));
        // 普通键不会以 flagsChanged 形式出现
        assert_eq!(flags_changed_pressed(Key::Space, u64::MAX), None);
    }

    // ── raw_to_input 全类映射 ──────────────────────────────────────

    /// 每类事件各一条：移动（含 Dragged）、五档按钮按下/抬起、滚轮垂直/
    /// 水平/对角/零、普通键、flagsChanged、表外键码丢弃。
    #[test]
    fn raw_to_input_maps_every_kind() {
        let bounds = ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0);

        // 移动：全局点 → 归一化（959.5 ≈ 中心 → 32767 左右）
        let mut r = raw(RawKind::MouseMove);
        r.x = 959.5;
        r.y = 539.5;
        let InputEvent::MouseMove { x, y } = raw_to_input(&r, bounds).unwrap() else {
            panic!("must be MouseMove");
        };
        assert!((32700..=32800).contains(&x), "{x}");
        assert!((32700..=32800).contains(&y), "{y}");

        // 按钮：CG 编号与 MouseButton 一致（0..=4），按下/抬起正确
        for (cg, want) in [
            (0i64, MouseButton::Left),
            (1, MouseButton::Right),
            (2, MouseButton::Middle),
            (3, MouseButton::X1),
            (4, MouseButton::X2),
        ] {
            let mut down = raw(RawKind::MouseDown);
            down.button = cg;
            assert_eq!(
                raw_to_input(&down, bounds),
                Some(InputEvent::MouseButton {
                    button: want,
                    pressed: true
                })
            );
            let mut up = raw(RawKind::MouseUp);
            up.button = cg;
            assert_eq!(
                raw_to_input(&up, bounds),
                Some(InputEvent::MouseButton {
                    button: want,
                    pressed: false
                })
            );
        }
        // 未知按钮号（多键鼠标 6 号键）：显式丢弃
        let mut weird = raw(RawKind::MouseDown);
        weird.button = 6;
        assert_eq!(raw_to_input(&weird, bounds), None);

        // 滚轮：垂直 up 为正直传；水平 CG 左正 → rc 右正取负
        let mut wheel = raw(RawKind::Scroll);
        wheel.wheel1 = 3;
        assert_eq!(
            raw_to_input(&wheel, bounds),
            Some(InputEvent::MouseWheel {
                delta: 3,
                horizontal: false
            })
        );
        let mut hw = raw(RawKind::Scroll);
        hw.wheel2 = -2; // CG 向右 2 行
        assert_eq!(
            raw_to_input(&hw, bounds),
            Some(InputEvent::MouseWheel {
                delta: 2,
                horizontal: true
            })
        );
        // 对角线：取主轴（|wheel1| > |wheel2| → 垂直）
        let mut diag = raw(RawKind::Scroll);
        diag.wheel1 = -5;
        diag.wheel2 = 2;
        assert_eq!(
            raw_to_input(&diag, bounds),
            Some(InputEvent::MouseWheel {
                delta: -5,
                horizontal: false
            })
        );
        // 双零：噪声丢弃；越界钳制
        assert_eq!(raw_to_input(&raw(RawKind::Scroll), bounds), None);
        let mut huge = raw(RawKind::Scroll);
        huge.wheel1 = 1 << 40;
        assert_eq!(
            raw_to_input(&huge, bounds),
            Some(InputEvent::MouseWheel {
                delta: i16::MAX,
                horizontal: false
            })
        );

        // 普通键：kVK_Escape(0x35) → Set 1 0x01
        let mut kd = raw(RawKind::KeyDown);
        kd.keycode = 0x35;
        kd.state_flags = cg_flags::COMMAND; // Cmd 按住时按 Esc
        assert_eq!(
            raw_to_input(&kd, bounds),
            Some(InputEvent::Key {
                scancode: 0x01,
                pressed: true,
                modifiers: b::LEFT_GUI,
            })
        );
        let mut ku = raw(RawKind::KeyUp);
        ku.keycode = 0x35;
        assert_eq!(
            raw_to_input(&ku, bounds),
            Some(InputEvent::Key {
                scancode: 0x01,
                pressed: false,
                modifiers: 0,
            })
        );

        // flagsChanged：kVK_RightShift(0x3C) 置位 → 右 Shift 按下，
        // 修饰键快照同步修正到右位
        let mut fc = raw(RawKind::FlagsChanged);
        fc.keycode = 0x3C;
        fc.event_flags = cg_flags::SHIFT;
        fc.state_flags = cg_flags::SHIFT;
        assert_eq!(
            raw_to_input(&fc, bounds),
            Some(InputEvent::Key {
                scancode: 0x36, // Set 1 右 Shift
                pressed: true,
                modifiers: b::RIGHT_SHIFT,
            })
        );

        // 表外键码：F13(0x69) 无 Key 变体；kVK_Function(0x3F) 有键无 Set 1 码
        let mut f13 = raw(RawKind::KeyDown);
        f13.keycode = 0x69;
        assert_eq!(raw_to_input(&f13, bounds), None);
        let mut fnkey = raw(RawKind::FlagsChanged);
        fnkey.keycode = 0x3F;
        fnkey.event_flags = cg_flags::SECONDARY_FN;
        assert_eq!(raw_to_input(&fnkey, bounds), None);
    }

    /// 捕获映射与注入桥互逆（回环自洽性）：捕获产生的每类事件，
    /// 经 bridge 注入路径都能翻译回 macOS 侧的对应载荷。
    #[test]
    fn raw_to_input_output_is_bridge_consumable() {
        use crate::bridge::InputBridge;
        use crate::error::InputError;
        use crate::event::PointerButtonKind;
        use crate::injector::Injector;
        use crate::plan::{EventPoster, PlannedEvent, PlannedMouseKind};
        use rc_input::InputInjector;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct FakePoster {
            posted: Arc<Mutex<Vec<PlannedEvent>>>,
        }
        impl EventPoster for FakePoster {
            fn post(&mut self, p: &PlannedEvent) -> std::result::Result<(), InputError> {
                self.posted.lock().unwrap().push(*p);
                Ok(())
            }
        }

        let bounds = ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let injector = Injector::from_parts(
            FakePoster {
                posted: Arc::clone(&recorded),
            },
            bounds,
            true,
        )
        .unwrap();
        let mut bridge = InputBridge::from_injector(injector);

        let mut wheel = raw(RawKind::Scroll);
        wheel.wheel1 = -3; // CG 垂直向下 3 行
        let captured = vec![
            raw_to_input(
                &{
                    let mut m = raw(RawKind::MouseMove);
                    m.x = 960.0;
                    m.y = 540.0;
                    m
                },
                bounds,
            )
            .unwrap(),
            raw_to_input(
                &{
                    let mut d = raw(RawKind::MouseDown);
                    d.button = 1;
                    d
                },
                bounds,
            )
            .unwrap(),
            raw_to_input(&wheel, bounds).unwrap(),
        ];
        bridge.inject(&captured).unwrap();
        let posted = recorded.lock().unwrap();
        // 移动（桥坐标在联合边界内）+ 右键按下 + 垂直滚动 dy=-3。
        // 坐标是「归一化 → 桥反归一化」的往返值，允许 ≤1 点量化误差。
        assert!(matches!(
            posted[0],
            PlannedEvent::Mouse {
                kind: PlannedMouseKind::Moved,
                ..
            }
        ));
        let PlannedEvent::Mouse { kind, button, x, y } = posted[1] else {
            panic!("must be Mouse");
        };
        assert_eq!(kind, PlannedMouseKind::Down);
        assert_eq!(button, PointerButtonKind::Right);
        assert!(
            (x - 960.0).abs() <= 1.0 && (y - 540.0).abs() <= 1.0,
            "({x},{y})"
        );
        assert_eq!(posted[2], PlannedEvent::Scroll { dx: 0, dy: -3 });
    }

    /// drain 泵：通道事件逐条翻译、表外键码计数丢弃、超时返回空表、
    /// 断开报 TapStopped（不静默当无事件）。
    #[test]
    fn drain_translates_drops_counts_and_errors_on_disconnect() {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let ctl = Arc::new(ffi::TapCtl::new_for_test());
        let mut cap = CgCapture::from_parts(
            Arc::new(Mutex::new(rx)),
            Arc::clone(&ctl),
            ScreenBounds::new(0.0, 0.0, 100.0, 100.0),
        );

        // 两事件：一条普通键（kVK_Return 0x24 → 0x1C）、一条表外 F13
        let mut kd = raw(RawKind::KeyDown);
        kd.keycode = 0x24;
        let mut f13 = raw(RawKind::KeyDown);
        f13.keycode = 0x69;
        tx.send(kd).unwrap();
        tx.send(f13).unwrap();

        let evs = cap.drain(Duration::from_millis(50)).unwrap();
        assert_eq!(
            evs,
            vec![InputEvent::Key {
                scancode: 0x1C,
                pressed: true,
                modifiers: 0
            }]
        );
        assert_eq!(cap.dropped_unmapped(), 1, "表外键码必须计数丢弃");

        // 空通道：超时返回空表
        assert!(cap.drain(Duration::from_millis(10)).unwrap().is_empty());

        // 断开：显式错误
        drop(tx);
        assert_eq!(
            cap.drain(Duration::from_millis(10)),
            Err(InputError::TapStopped)
        );
    }

    /// 满载丢弃计数的接线：`dropped_channel_full` 必须读 tap 控制块里的
    /// 原子计数（回调线程写），且与 `dropped_unmapped` 互不影响——
    /// 两个独立诊断面不许串台。
    #[test]
    fn dropped_channel_full_reads_tap_ctl_counter() {
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let ctl = Arc::new(ffi::TapCtl::new_for_test());
        let cap = CgCapture::from_parts(
            Arc::new(Mutex::new(rx)),
            Arc::clone(&ctl),
            ScreenBounds::new(0.0, 0.0, 100.0, 100.0),
        );
        assert_eq!(cap.dropped_channel_full(), 0, "初始必须为 0");
        // 模拟回调侧三次满载丢弃（同一原子字段，tap 线程在生产路径写）
        ctl.dropped_channel_full
            .fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(cap.dropped_channel_full(), 3);
        assert_eq!(cap.dropped_unmapped(), 0, "满载丢弃不得混入映射层丢弃计数");
        drop(tx);
    }

    /// 生产形态满足 trait 契约的 Send 超特征。
    #[test]
    fn production_capture_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CgCapture>();
    }

    /// 滚轮量纲**全链对称**（离线；审计问题 1「滚轮单位不一致」的
    /// 端到端锁定）：构造行单位的 RawCgEvent（tap 抽取层的形态）→
    /// 捕获映射 → `rc_input::InputEvent::MouseWheel` → 注入桥 →
    /// `PlannedEvent::Scroll` → ffi 的行单位 CGEvent 构造 → 用 tap 实际
    /// 读取的字段（`SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`）读回——每一步
    /// 数值逐值相等，单位始终是行。旧的 PIXEL 注入在这最后一步会缩水
    /// 约 10 倍（本机实测 3 行 → tap 读回 1 行）。
    #[test]
    fn wheel_line_units_survive_capture_to_injection_chain() {
        use crate::bridge::InputBridge;
        use crate::error::InputError;
        use crate::injector::Injector;
        use crate::plan::{EventPoster, PlannedEvent};
        use core_graphics::event::EventField;
        use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
        use rc_input::InputInjector;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct FakePoster {
            posted: Arc<Mutex<Vec<PlannedEvent>>>,
        }
        impl EventPoster for FakePoster {
            fn post(&mut self, p: &PlannedEvent) -> std::result::Result<(), InputError> {
                self.posted.lock().unwrap().push(*p);
                Ok(())
            }
        }

        let bounds = ScreenBounds::new(0.0, 0.0, 1920.0, 1080.0);
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let injector = Injector::from_parts(
            FakePoster {
                posted: Arc::clone(&recorded),
            },
            bounds,
            true,
        )
        .unwrap();
        let mut bridge = InputBridge::from_injector(injector);

        // 垂直 3 行（wheel1 是 tap 读 AXIS_1 得到的行数）与水平向右 2 行
        //（CG AXIS_2 向左为正，向右 = -2）。
        let mut v = raw(RawKind::Scroll);
        v.wheel1 = 3;
        let mut h = raw(RawKind::Scroll);
        h.wheel2 = -2;
        let events: Vec<InputEvent> = [v, h]
            .iter()
            .map(|r| raw_to_input(r, bounds).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![
                InputEvent::MouseWheel {
                    delta: 3,
                    horizontal: false
                },
                InputEvent::MouseWheel {
                    delta: 2,
                    horizontal: true
                },
            ],
            "捕获映射必须产出 rc-input 规范的行单位 delta"
        );
        bridge.inject(&events).unwrap();
        let posted = recorded.lock().unwrap();
        assert_eq!(posted[0], PlannedEvent::Scroll { dx: 0, dy: 3 });
        assert_eq!(posted[1], PlannedEvent::Scroll { dx: -2, dy: 0 });

        // 注入终点：同样的计划事件经 ffi 构造 CGEvent（CgPoster::post 的
        // 构造路径），用 tap 抽取读取的同一对字段读回。
        let source =
            CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("构造事件源无需授权");
        let PlannedEvent::Scroll { dx: vdx, dy: vdy } = posted[0] else {
            panic!("must be Scroll");
        };
        let v_ev = crate::ffi::new_line_scroll_event(source.clone(), vdx, vdy).unwrap();
        assert_eq!(
            v_ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            3,
            "垂直 3 行注入，tap 侧 AXIS_1 必须读回同一行数"
        );
        let PlannedEvent::Scroll { dx: hdx, dy: hdy } = posted[1] else {
            panic!("must be Scroll");
        };
        let h_ev = crate::ffi::new_line_scroll_event(source, hdx, hdy).unwrap();
        assert_eq!(
            h_ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
            -2,
            "水平 dx=-2 注入，tap 侧 AXIS_2 必须读回同一行数"
        );
        assert_eq!(
            h_ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
            0
        );
    }

    // ─────────────────────────────────────────────────────────────
    // 真机测试（#[ignore]）：需要辅助功能授权，经
    // ./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture
    // 运行（直接 cargo test 的 TCC 责任进程落在宿主终端，吃不到授权）。
    // ─────────────────────────────────────────────────────────────

    use std::time::Instant;

    /// 期望事件与捕获事件的匹配（移动允许 ±100 归一化单位 ≈ 3px 量化误差，
    /// 其余精确）。Key 的 `modifiers` 参与精确比较——无人操作时应为 0。
    fn real_match(expected: &InputEvent, got: &InputEvent) -> bool {
        match (expected, got) {
            (InputEvent::MouseMove { x: ex, y: ey }, InputEvent::MouseMove { x, y }) => {
                (i32::from(*x) - i32::from(*ex)).unsigned_abs() <= 100
                    && (i32::from(*y) - i32::from(*ey)).unsigned_abs() <= 100
            }
            (a, b) => a == b,
        }
    }

    /// 期望批在捕获流里的有序匹配数（子序列匹配，噪声事件跳过）。
    fn count_matched(batch: &[InputEvent], pulled: &[InputEvent]) -> usize {
        let mut i = 0usize;
        for got in pulled {
            if i < batch.len() && real_match(&batch[i], got) {
                i += 1;
            }
        }
        i
    }

    /// 真机闭环：合成一批事件**真注入**（CGEventPost）→ 真捕获（CGEventTap）
    /// → `InputChannel` 泵 → 回环 transport → 解码 → 注入到记录型注入器。
    ///
    /// 断言：
    /// 1. 捕获侧按序收齐注入的全部 8 条（等量）；
    /// 2. 经通道后 4 次移动按 I2 合并为最新 1 次，按键/按钮全保留（5 条），
    ///    坐标 / 键码 / 按下语义与注入批一致。
    ///
    /// 注意：本测试会真的移动光标、在光标处点击一次并发送 Escape——
    /// 只应在测试机上跑。
    #[test]
    #[ignore = "真机测试：需要辅助功能授权，经 ./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture 运行"]
    fn capture_inject_loopback_real_machine() {
        // 真机测试串行化：并行注入/捕获会互相污染（见 crate::rm_lock 文档）。
        let _rm = crate::rm_lock::REAL_MACHINE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        use crate::bridge::InputBridge;
        use rc_input::channel::InputChannel;
        use rc_input::{InputInjector, DEFAULT_WINDOW};
        use rc_net::{NetEvent, NetStats, Transport};

        /// 回环 transport：发送端记录，poll 侧回放（与 rc-input 通道测试
        /// 的 MockTransport 同构，但那是别 crate 的私有类型，这里自持一份）。
        #[derive(Default)]
        struct LoopbackTransport {
            sent: Vec<Vec<u8>>,
            deliver: Vec<NetEvent>,
        }
        impl Transport for LoopbackTransport {
            fn send_datagram(&mut self, _data: &[u8]) -> Result<()> {
                panic!("输入绝不该走媒体 datagram（I1）");
            }
            fn send_reliable(&mut self, _data: &[u8]) -> Result<()> {
                panic!("输入必须走 urgent 流，不该出现在共享控制流（N6/I1）");
            }
            // 显式覆写（不吃默认委托）：回环链路按真实接线走 urgent 流，
            // 交付侧也以 ReliableUrgent 回放——两条流一旦接错，这里当场爆。
            fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
                self.sent.push(data.to_vec());
                Ok(())
            }
            fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
                Ok(std::mem::take(&mut self.deliver))
            }
            fn stats(&self) -> NetStats {
                NetStats::default()
            }
        }

        /// 记录型注入器：闭环终点，不真的再注入（避免事件自激循环）。
        #[derive(Default)]
        struct RecordingInjector {
            events: Vec<InputEvent>,
        }
        impl InputInjector for RecordingInjector {
            fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
                self.events.extend_from_slice(events);
                Ok(())
            }
            fn set_screen_size(&mut self, _w: u32, _h: u32) {}
        }

        /// 透传捕获器：pump 拉取的真实事件留档（断言捕获侧等量用）。
        struct CountingCapture<'a> {
            inner: &'a mut CgCapture,
            pulled: Vec<InputEvent>,
        }
        impl InputCapture for CountingCapture<'_> {
            fn next_events(&mut self, timeout: Duration) -> Result<Vec<InputEvent>> {
                let evs = self.inner.next_events(timeout)?;
                self.pulled.extend_from_slice(&evs);
                Ok(evs)
            }
        }

        let mut capture =
            CgCapture::new().expect("CgCapture::new 需要辅助功能授权（系统设置 → 隐私与安全性 → 辅助功能），且授权后需重启进程");
        let mut bridge = InputBridge::new().expect("InputBridge::new 需要辅助功能授权");
        // 捕获与注入共享同一联合边界（都取自同一时刻的显示器布局）
        assert_eq!(
            capture.screen_bounds(),
            bridge.screen_bounds(),
            "捕获/注入的归一化定义域不一致（显示器热插拔？）"
        );

        // 静置：清掉测试启动前残留在通道里的事件
        let _ = capture.next_events(Duration::from_millis(200)).unwrap();

        // 合成批：4 次移动（相互间隔 4000 单位，远超 100 的容差）+
        // 左键按下 + Escape 按下/抬起 + 左键抬起。
        let batch: Vec<InputEvent> = vec![
            InputEvent::MouseMove { x: 24000, y: 30000 },
            InputEvent::MouseMove { x: 28000, y: 30000 },
            InputEvent::MouseMove { x: 32000, y: 30000 },
            InputEvent::MouseMove { x: 36000, y: 30000 },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::Key {
                scancode: 0x01, // Set 1 Escape
                pressed: true,
                modifiers: 0,
            },
            InputEvent::Key {
                scancode: 0x01,
                pressed: false,
                modifiers: 0,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            },
        ];

        let mut transport = LoopbackTransport::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);

        let t0 = Instant::now();
        for ev in &batch {
            bridge
                .inject(std::slice::from_ref(ev))
                .expect("真注入失败（辅助功能授权未生效？）");
        }

        // 捕获 → 泵：循环直到批内 8 条按序全部出现在捕获侧（或超时）。
        let deadline = Duration::from_secs(6);
        let pulled = {
            let mut counting = CountingCapture {
                inner: &mut capture,
                pulled: Vec::new(),
            };
            while count_matched(&batch, &counting.pulled) < batch.len() && t0.elapsed() < deadline {
                channel
                    .pump_sender(
                        &mut counting,
                        &mut transport,
                        Instant::now(),
                        Duration::from_millis(30),
                    )
                    .unwrap();
            }
            // 批尾冲刷：最后的纯移动尾批最多压一个 4ms 窗口
            std::thread::sleep(DEFAULT_WINDOW * 2);
            channel
                .pump_sender(
                    &mut counting,
                    &mut transport,
                    Instant::now(),
                    Duration::from_millis(5),
                )
                .unwrap();
            counting.pulled
        };

        let matched = count_matched(&batch, &pulled);
        assert_eq!(
            matched,
            batch.len(),
            "捕获侧只匹配到 {matched}/{} 条注入事件；pulled={pulled:?}（若 Key 差在 \
             modifiers 字段：真机上有修饰键被按住）",
            batch.len()
        );
        if pulled.len() > batch.len() {
            println!(
                "（信息）捕获侧另有 {} 条批外事件（系统噪声 / 点击引发的界面反馈）",
                pulled.len() - batch.len()
            );
        }
        assert_eq!(
            capture.dropped_unmapped(),
            0,
            "本批全部键码在表内，不该有丢弃（丢弃数异常说明键表回归）"
        );

        // 回环 → 解码 → 注入（记录型）
        transport.deliver = transport
            .sent
            .drain(..)
            .map(NetEvent::ReliableUrgent)
            .collect();
        let mut recorder = RecordingInjector::default();
        let injected = channel
            .pump_receiver(&mut transport, &mut recorder, Duration::from_millis(0))
            .unwrap();
        assert_eq!(injected, recorder.events.len());

        // 真机噪声容忍的闭环断言。注入期间系统/物理输入可能夹杂额外事件
        // （实测：注入的点击引发过一次光标重定位 mouseMoved），因此期望的
        // 关键事件按**有序子序列**断言（允许被噪声隔开，顺序与完整性不容
        // 妥协）；移动合并（I2）单独按数量与坐标断言。
        //
        // I3（不可合并的关键事件）：左键按下 → Esc按下 → Esc抬起 → 左键抬起。
        let core = [
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::Key {
                scancode: 0x01,
                pressed: true,
                modifiers: 0,
            },
            InputEvent::Key {
                scancode: 0x01,
                pressed: false,
                modifiers: 0,
            },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            },
        ];
        let mut idx = 0usize;
        for want in &core {
            match recorder.events[idx..].iter().position(|e| *e == *want) {
                Some(p) => idx += p + 1,
                None => panic!(
                    "关键事件序列缺损或乱序：在 recorder.events[{}..] 找不到 {:?}，\
                     实际 {:?}",
                    idx, want, recorder.events
                ),
            }
        }

        // I2（移动合并，按注入坐标族断言）：只统计注入批坐标族（y≈30000、
        // x∈[24000,36000]±100）的移动——族外移动是物理输入/其他来源噪声，
        // 不参与合并语义。真机上 4 次注入移动可能跨批处理窗口到达（合并
        // 发生在同一 4ms 窗内），确定性 I2 证明在 rc-input 离线测试里；
        // 本断言只验真机语义：族内移动 1..=4 条、且**最后一条族内移动**
        // （应恰在左键按下前）等于最新注入坐标（≈36000,30000）——同窗
        // 合并「最新值胜出」在真机上仍然成立。
        let family = |x: u16, y: u16| {
            (i32::from(y) - 30000).unsigned_abs() <= 100 && (24000..=36000).contains(&x)
        };
        let moves: Vec<(u16, u16)> = recorder
            .events
            .iter()
            .filter_map(|e| match e {
                InputEvent::MouseMove { x, y } if family(*x, *y) => Some((*x, *y)),
                _ => None,
            })
            .collect();
        assert!(
            (1..=4).contains(&moves.len()),
            "注入坐标族内应有 1..=4 条移动，实际 {}：{:?}",
            moves.len(),
            recorder.events
        );
        let last = moves[moves.len() - 1];
        assert!(
            (i32::from(last.0) - 36000).unsigned_abs() <= 100
                && (i32::from(last.1) - 30000).unsigned_abs() <= 100,
            "族内最后一条移动应为最新注入坐标 (≈36000,30000)，实际 {last:?}；\
             全部移动 {moves:?}"
        );
        println!(
            "闭环通过：注入 8 → 捕获 8（等量）→ 通道合并后 {} → 解码注入 {}；\
             屏幕边界 {:?}",
            recorder.events.len(),
            recorder.events.len(),
            capture.screen_bounds()
        );
    }

    /// 延迟探针（可选变体）：逐条注入移动并测量「注入返回 → 捕获侧收到
    /// 同坐标事件」的单程耗时，只打点（min/p50/avg/max），**不做门槛断言**
    /// （唯一断言：不能一条都收不到——那是链路断了，不是慢）。
    #[test]
    #[ignore = "真机测试：需要辅助功能授权，经 ./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture 运行"]
    fn capture_inject_latency_probe_real_machine() {
        // 真机测试串行化（见 crate::rm_lock 文档）。
        let _rm = crate::rm_lock::REAL_MACHINE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        use crate::bridge::InputBridge;
        use rc_input::InputInjector;

        let mut capture = CgCapture::new().expect("需要辅助功能授权");
        let mut bridge = InputBridge::new().expect("需要辅助功能授权");
        // 静置
        let _ = capture.next_events(Duration::from_millis(200)).unwrap();

        let mut samples: Vec<Duration> = Vec::new();
        let total = 20usize;
        for i in 0..total {
            // 两个位置交替（相距 1000 单位，远超容差，前一样本的迟到事件
            // 不会误匹配本样本）
            let target = if i % 2 == 0 { 30000 } else { 31000 };
            let t0 = Instant::now();
            bridge
                .inject(&[InputEvent::MouseMove {
                    x: target,
                    y: 32000,
                }])
                .unwrap();
            let mut got: Option<Duration> = None;
            while t0.elapsed() < Duration::from_secs(2) {
                for ev in capture.next_events(Duration::from_millis(2)).unwrap() {
                    if let InputEvent::MouseMove { x, y } = ev {
                        if (i32::from(x) - i32::from(target)).unsigned_abs() <= 100
                            && (i32::from(y) - 32000).unsigned_abs() <= 100
                        {
                            got = Some(t0.elapsed());
                        }
                    }
                }
                if got.is_some() {
                    break;
                }
            }
            match got {
                Some(d) => samples.push(d),
                None => println!("样本 {i}: 2s 内未捕获（注入丢失或链路异常）"),
            }
        }
        assert!(
            !samples.is_empty(),
            "{total} 次注入一条都没捕获——注入或捕获链路断了（不是慢）"
        );
        samples.sort_unstable();
        let min = samples[0];
        let p50 = samples[samples.len() / 2];
        let max = samples[samples.len() - 1];
        let avg = samples.iter().sum::<Duration>() / samples.len() as u32;
        println!(
            "capture←inject 单程延迟打点（{}/{} 样本）：min={min:?} p50={p50:?} \
             avg={avg:?} max={max:?}（探针，无门槛）",
            samples.len(),
            total
        );
    }

    /// 真机滚轮回环（行单位对称性；审计问题 1 的真机验收）：**真注入**
    /// 若干行滚动（LINE 单位 CGEventPost）→ 真 tap 捕获 → 捕获映射还原出
    /// **等值**行滚动——delta 与轴逐一相等（滚轮事件 `real_match` 走精确
    /// 相等，1..3 行的量级下旧 PIXEL 注入会缩水约 10 倍、当场爆本测试）。
    ///
    /// 同时断言满载丢弃计数为 0（3 条事件远不及 256 容量，非 0 即背压
    /// 通路回归）。注意：注入会让前台应用真的滚动——只应在测试机上跑。
    #[test]
    #[ignore = "真机测试：需要辅助功能授权，经 ./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture 运行"]
    fn wheel_line_unit_roundtrip_real_machine() {
        // 真机测试串行化（见 crate::rm_lock 文档）。
        let _rm = crate::rm_lock::REAL_MACHINE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        use crate::bridge::InputBridge;
        use rc_input::InputInjector;

        let mut capture =
            CgCapture::new().expect("CgCapture::new 需要辅助功能授权（系统设置 → 隐私与安全性 → 辅助功能），且授权后需重启进程");
        let mut bridge = InputBridge::new().expect("InputBridge::new 需要辅助功能授权");
        // 静置：清掉测试启动前残留在通道里的事件
        let _ = capture.next_events(Duration::from_millis(200)).unwrap();

        // 审计原文的典型量级（±1..±3 行），垂直正负 + 水平各一条
        let batch: Vec<InputEvent> = vec![
            InputEvent::MouseWheel {
                delta: 3,
                horizontal: false,
            },
            InputEvent::MouseWheel {
                delta: -2,
                horizontal: false,
            },
            InputEvent::MouseWheel {
                delta: 2,
                horizontal: true,
            },
        ];
        let t0 = Instant::now();
        for ev in &batch {
            bridge
                .inject(std::slice::from_ref(ev))
                .expect("真注入失败（辅助功能授权未生效？）");
        }

        // 捕获侧按序子序列匹配（容忍物理输入噪声夹杂）
        let mut pulled = Vec::new();
        while count_matched(&batch, &pulled) < batch.len() && t0.elapsed() < Duration::from_secs(5)
        {
            pulled.extend(capture.next_events(Duration::from_millis(20)).unwrap());
        }
        let matched = count_matched(&batch, &pulled);
        assert_eq!(
            matched,
            batch.len(),
            "滚轮回环只匹配到 {matched}/{} 条等值行滚动；pulled={pulled:?}",
            batch.len()
        );
        assert_eq!(
            capture.dropped_channel_full(),
            0,
            "3 条事件远不及通道容量 256，满载丢弃非 0 即背压通路回归"
        );
        println!(
            "滚轮回环（行单位）通过：注入 {} 条 → 捕获等值匹配 {} 条（delta/轴逐一相等）",
            batch.len(),
            matched
        );
    }
}
