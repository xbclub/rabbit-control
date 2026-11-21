//! 输入批处理与合并（任务卡 T10 的 I2 / I3 / I6 / I7 / I8）。
//!
//! 1000Hz 鼠标裸发是 1000 包/秒，网络会被打爆；但按键合并会丢键。
//! [`InputBatcher`] 的取舍：
//! - **移动可丢旧**（I2）：同一批内连续 `MouseMove` 只保留最新坐标；
//! - **按键不可丢**（I3）：`MouseButton` / `MouseWheel` / `Key` 全部按序保留；
//! - **关键事件立即到期**（I7）：批内一旦出现按键/按钮/滚轮，下一次
//!   [`InputBatcher::poll`] 不再等待窗口直接放行；
//! - **窗口 4ms**（I6）：只有移动时最多压 `window` 再发（250Hz 发送率）。
//!
//! 批处理器**不读时钟**：`push` / `poll` 的 `now` 全部由调用方注入，
//! 与 rc-sched 的时间注入约定一致，逻辑可离线确定性单测。

use crate::InputEvent;
use std::time::{Duration, Instant};

/// 建议批窗口：4ms（≈250Hz 发送率，足够跟手）。I6。
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(4);

/// [`InputEvent::Key`] 的 `modifiers` 字段位定义（I8）。
///
/// 每条 Key 事件携带**事件发生瞬间的完整修饰键快照**，接收端不自行
/// 推断（对端焦点切换 / 丢事件不会造成组合键错乱）。
pub mod modifier_bits {
    /// 左 Ctrl。
    pub const LEFT_CTRL: u8 = 0x01;
    /// 左 Shift。
    pub const LEFT_SHIFT: u8 = 0x02;
    /// 左 Alt / Option。
    pub const LEFT_ALT: u8 = 0x04;
    /// 左 GUI（Windows 键 / Cmd）。
    pub const LEFT_GUI: u8 = 0x08;
    /// 右 Ctrl。
    pub const RIGHT_CTRL: u8 = 0x10;
    /// 右 Shift。
    pub const RIGHT_SHIFT: u8 = 0x20;
    /// 右 Alt / AltGr。
    pub const RIGHT_ALT: u8 = 0x40;
    /// 右 GUI（Windows 键 / Cmd）。
    pub const RIGHT_GUI: u8 = 0x80;
}

/// 修饰键的 Set 1 扫描码（I5/I8；E0 前缀编码为 `0xE0xx`）。
///
/// 与平台实现（rc-input-win 的 `MapVirtualKey`、rc-input-macos 的
/// 扫描码表）共用同一约定，桥接层有对照测试锁死。
pub mod modifier_scancodes {
    /// 左 Ctrl = `0x1D`。
    pub const LEFT_CTRL: u16 = 0x1D;
    /// 左 Shift = `0x2A`。
    pub const LEFT_SHIFT: u16 = 0x2A;
    /// 左 Alt = `0x38`。
    pub const LEFT_ALT: u16 = 0x38;
    /// 左 GUI = `E0 5B`。
    pub const LEFT_GUI: u16 = 0xE05B;
    /// 右 Ctrl = `E0 1D`。
    pub const RIGHT_CTRL: u16 = 0xE01D;
    /// 右 Shift = `0x36`。
    pub const RIGHT_SHIFT: u16 = 0x36;
    /// 右 Alt（AltGr）= `E0 38`。
    pub const RIGHT_ALT: u16 = 0xE038;
    /// 右 GUI = `E0 5C`。
    pub const RIGHT_GUI: u16 = 0xE05C;
}

/// 修饰键状态跟踪器（I8）：从 Key 事件流维护精确的按下状态。
///
/// [`InputBatcher`] 内置一个实例：每条进入批的 Key 事件先更新状态，
/// 再把 `modifiers` 字段**重写为当前快照**——调用方无需（也不应）自行
/// 维护修饰键状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState {
    bits: u8,
}

impl ModifierState {
    /// 全零状态。
    pub fn new() -> Self {
        Self { bits: 0 }
    }

    /// 当前修饰键快照（位定义见 [`modifier_bits`]）。
    pub fn bits(&self) -> u8 {
        self.bits
    }

    /// 用一条事件更新状态并返回新快照。
    ///
    /// 只有修饰键自身的 Key 事件改变状态（按下置位、抬起清位）；
    /// `modifiers` 字段不参与——状态只信事件序列，不信远端快照，
    /// 这样丢一条松键事件也能靠后续事件纠偏。非 Key 事件为 no-op。
    pub fn update(&mut self, ev: &InputEvent) -> u8 {
        if let InputEvent::Key {
            scancode, pressed, ..
        } = ev
        {
            use modifier_bits as b;
            use modifier_scancodes as s;
            let bit = match *scancode {
                s::LEFT_CTRL => b::LEFT_CTRL,
                s::LEFT_SHIFT => b::LEFT_SHIFT,
                s::LEFT_ALT => b::LEFT_ALT,
                s::LEFT_GUI => b::LEFT_GUI,
                s::RIGHT_CTRL => b::RIGHT_CTRL,
                s::RIGHT_SHIFT => b::RIGHT_SHIFT,
                s::RIGHT_ALT => b::RIGHT_ALT,
                s::RIGHT_GUI => b::RIGHT_GUI,
                _ => return self.bits,
            };
            if *pressed {
                self.bits |= bit;
            } else {
                self.bits &= !bit;
            }
        }
        self.bits
    }

    /// 全部抬起（清零）。连接断开 / 焦点丢失时由上层调用兜底，
    /// 防止「松键事件丢失 → 按键卡住」（任务卡坑 7）。
    pub fn release_all(&mut self) {
        self.bits = 0;
    }
}

/// 批处理器：同一时间窗内的移动事件合并、按键全保留、关键事件立即放行。
///
/// 时间全部由调用方注入（`push`/`poll` 的 `now`）。典型控制端循环：
/// `push` 捕获到的每条事件 → 按 tick `poll` 到期批次 → 编码发送。
/// 松键 / 点击等时机可显式 [`flush`](InputBatcher::flush)。
///
/// 合并规则（I2）只作用于**批内连续**的 `MouseMove`：新移动覆盖批
/// 尾部的前一个移动；批尾不是移动（如刚有按键）则追加——保证
/// 「移动 → 点击」的相对顺序不被合并打乱（点击落在正确坐标上）。
#[derive(Debug)]
pub struct InputBatcher {
    window: Duration,
    /// 当前批次第一项事件的时刻（批年龄从它起算，约束移动的最大延迟）。
    window_start: Option<Instant>,
    /// 批内待发事件（保序；连续移动已被合并为最新值）。
    pending: Vec<InputEvent>,
    /// 批内出现过关键事件（I7）：下一次 poll 无视窗口直接放行。
    critical: bool,
    /// 修饰键精确跟踪（I8）。
    modifiers: ModifierState,
}

impl InputBatcher {
    /// 构造。`window` 是纯移动批的最大驻留时间，建议
    /// [`DEFAULT_WINDOW`]（4ms，I6）。
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            window_start: None,
            pending: Vec::new(),
            critical: false,
            modifiers: ModifierState::new(),
        }
    }

    /// 压入一条事件（`now` 为事件时刻，由调用方注入）。
    ///
    /// - `MouseMove`：批尾是移动则**原位覆盖**（I2），否则追加；
    /// - `Key`：先更新修饰键状态，`modifiers` 字段重写为最新快照（I8），
    ///   然后追加；
    /// - `MouseButton` / `MouseWheel` / `Key`：标记关键（I7）；
    /// - `Sync`：普通追加（心跳不需要抢跑）。
    pub fn push(&mut self, ev: InputEvent, now: Instant) {
        match ev {
            InputEvent::MouseMove { x, y } => {
                if let Some(InputEvent::MouseMove { x: lx, y: ly }) = self.pending.last_mut() {
                    *lx = x;
                    *ly = y;
                } else {
                    self.pending.push(InputEvent::MouseMove { x, y });
                }
            }
            InputEvent::Key {
                scancode, pressed, ..
            } => {
                let probe = InputEvent::Key {
                    scancode,
                    pressed,
                    modifiers: 0,
                };
                // I8：以跟踪状态覆盖 modifiers（快照含本事件自身的效应）
                let modifiers = self.modifiers.update(&probe);
                self.pending.push(InputEvent::Key {
                    scancode,
                    pressed,
                    modifiers,
                });
                self.critical = true;
            }
            ev @ InputEvent::MouseButton { .. } | ev @ InputEvent::MouseWheel { .. } => {
                self.pending.push(ev);
                self.critical = true;
            }
            ev @ InputEvent::Sync => {
                self.pending.push(ev);
            }
        }
        if self.window_start.is_none() {
            self.window_start = Some(now);
        }
    }

    /// 取出到期批次；未到期 / 空批返回空表。
    ///
    /// 到期条件（任一）：
    /// 1. 批内有关键事件（I7，不等窗口）；
    /// 2. 距批次首事件 ≥ `window`（I6）。
    pub fn poll(&mut self, now: Instant) -> Vec<InputEvent> {
        let Some(start) = self.window_start else {
            return Vec::new();
        };
        if !self.critical && now.duration_since(start) < self.window {
            return Vec::new();
        }
        self.take()
    }

    /// 无条件冲刷全部待发事件（松键、点击、会话收尾等显式时机）。
    pub fn flush(&mut self) -> Vec<InputEvent> {
        self.take()
    }

    /// 交出批次并复位窗口状态。
    fn take(&mut self) -> Vec<InputEvent> {
        self.window_start = None;
        self.critical = false;
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MouseButton;

    /// I2：批内 10 次移动只出 1 次，且坐标是**最新**值。
    #[test]
    fn move_events_coalesce_to_latest() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        for i in 0..10u16 {
            b.push(
                InputEvent::MouseMove {
                    x: 100 + i,
                    y: 200 + i,
                },
                t0,
            );
        }
        let out = b.poll(t0 + DEFAULT_WINDOW);
        assert_eq!(
            out,
            vec![InputEvent::MouseMove { x: 109, y: 209 }],
            "10 次移动必须合并为最新 1 次"
        );

        // 取走后再 poll 无残余
        assert!(b.poll(t0 + DEFAULT_WINDOW * 2).is_empty());
    }

    /// I3：3 次按键一条不丢、顺序不变，且不等窗口即可取到。
    #[test]
    fn key_events_are_never_coalesced() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        b.push(
            InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: 0,
            },
            t0,
        );
        b.push(
            InputEvent::Key {
                scancode: 0x1E,
                pressed: false,
                modifiers: 0,
            },
            t0,
        );
        b.push(
            InputEvent::Key {
                scancode: 0x30,
                pressed: true,
                modifiers: 0,
            },
            t0,
        );
        // t0 即刻 poll：按键是关键事件，不必等窗口
        let out = b.poll(t0);
        assert_eq!(out.len(), 3, "按键绝不合并");
        assert_eq!(
            out[0],
            InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: 0
            },
            "第一个事件无修饰键"
        );
        assert!(matches!(
            out[1],
            InputEvent::Key {
                scancode: 0x1E,
                pressed: false,
                ..
            }
        ));
        assert!(matches!(
            out[2],
            InputEvent::Key {
                scancode: 0x30,
                pressed: true,
                ..
            }
        ));
    }

    /// I7：移动开窗后窗口未到期，按钮一进来立即放行整批（含最新移动）。
    #[test]
    fn button_events_flush_immediately() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        b.push(InputEvent::MouseMove { x: 10, y: 20 }, t0);
        // 窗口内、无关键事件：不出批
        assert!(b.poll(t0 + Duration::from_millis(1)).is_empty());
        // 关键事件进入
        b.push(
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            t0 + Duration::from_millis(2),
        );
        let out = b.poll(t0 + Duration::from_millis(2));
        assert_eq!(
            out,
            vec![
                InputEvent::MouseMove { x: 10, y: 20 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ],
            "按钮必须不等窗口立即放行，且移动保序在前"
        );
    }

    /// I6：只有移动时，窗口内不发送，窗口到期才发送。
    #[test]
    fn batch_window_respected() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        b.push(InputEvent::MouseMove { x: 1, y: 2 }, t0);
        // 差 1ms 到期：不发
        assert!(b.poll(t0 + Duration::from_millis(3)).is_empty());
        // 到期：发
        let out = b.poll(t0 + DEFAULT_WINDOW);
        assert_eq!(out, vec![InputEvent::MouseMove { x: 1, y: 2 }]);
        // 发完后窗口重置：新移动重新开窗
        b.push(InputEvent::MouseMove { x: 3, y: 4 }, t0 + DEFAULT_WINDOW);
        assert!(b
            .poll(t0 + DEFAULT_WINDOW + Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            b.poll(t0 + DEFAULT_WINDOW * 2),
            vec![InputEvent::MouseMove { x: 3, y: 4 }]
        );
    }

    /// 边界：空批 poll/flush 都是空表（不产生消息，见 channel 测试）。
    #[test]
    fn empty_batch_produces_no_message() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t = Instant::now();
        assert!(b.poll(t).is_empty());
        assert!(b.flush().is_empty());
        // 取空之后再 poll 依旧空
        assert!(b.poll(t + DEFAULT_WINDOW).is_empty());
    }

    /// I8：修饰键状态跨事件精确跟踪，每条 Key 事件携带即时快照；
    /// 松键后快照立即回退。
    #[test]
    fn modifier_state_tracked_across_events() {
        let mut b = InputBatcher::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        let key = |scancode: u16, pressed: bool| InputEvent::Key {
            scancode,
            pressed,
            modifiers: 0, // 调用方故意乱填，批处理器必须以跟踪状态覆盖
        };
        // Shift 按下 → A 按下 → Shift 抬起 → A 抬起
        b.push(key(modifier_scancodes::LEFT_SHIFT, true), t0);
        b.push(key(0x1E, true), t0);
        b.push(key(modifier_scancodes::LEFT_SHIFT, false), t0);
        b.push(key(0x1E, false), t0);
        let out = b.poll(t0);
        assert_eq!(out.len(), 4);
        let mods: Vec<u8> = out
            .iter()
            .map(|ev| match ev {
                InputEvent::Key { modifiers, .. } => *modifiers,
                _ => panic!("unexpected {ev:?}"),
            })
            .collect();
        assert_eq!(
            mods[0],
            modifier_bits::LEFT_SHIFT,
            "Shift 按下事件自身已置位"
        );
        assert_eq!(mods[1], modifier_bits::LEFT_SHIFT, "A 按下时快照含 Shift");
        assert_eq!(mods[2], 0, "Shift 抬起后快照清空");
        assert_eq!(mods[3], 0, "A 抬起时无修饰键");

        // ModifierState 单元语义：右修饰键与 release_all 兜底
        let mut ms = ModifierState::new();
        ms.update(&key(modifier_scancodes::RIGHT_GUI, true));
        ms.update(&key(modifier_scancodes::RIGHT_SHIFT, true));
        assert_eq!(
            ms.bits(),
            modifier_bits::RIGHT_GUI | modifier_bits::RIGHT_SHIFT
        );
        ms.update(&InputEvent::Sync); // 非 Key 事件 no-op
        assert_eq!(
            ms.bits(),
            modifier_bits::RIGHT_GUI | modifier_bits::RIGHT_SHIFT
        );
        ms.release_all();
        assert_eq!(ms.bits(), 0);
    }
}
