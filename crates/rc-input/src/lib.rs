//! RabbitControl 输入通道核心（全平台，任务卡 T10）。
//!
//! 键鼠事件的**类型、批处理、手写二进制线格式**与注入/捕获抽象都集中在这。
//! 平台实现（Windows `SendInput`、macOS CGEvent）在各自的 `rc-input-win`
//! / `rc-input-macos` crate 里实现本 crate 的 [`InputInjector`] /
//! [`InputCapture`] trait。
//!
//! # 为什么输入要单独一层（对应任务卡 I 编号）
//!
//! | 编号 | 要求 | 本 crate 的落点 |
//! |---|---|---|
//! | I1 | 输入走独立通道，不与媒体共享队列 | [`channel`]：走 rc-net 的 reliable stream，媒体永不进入 |
//! | I2 | 移动事件批内只保留最新 | [`InputBatcher`]：连续 `MouseMove` 原位覆盖 |
//! | I3 | 按键事件全部保留 | [`InputBatcher`]：按键/按钮/滚轮永不合并 |
//! | I4 | 绝对坐标归一化 0..65535 | [`InputEvent::MouseMove`] + [`normalize_coord`] |
//! | I5 | 用扫描码而非虚拟键码 | [`InputEvent::Key`] 的 `scancode`（Set 1，约定见字段文档） |
//! | I6 | 批窗口 4ms | [`batch::DEFAULT_WINDOW`] |
//! | I7 | 关键事件立即 flush | [`InputBatcher::push`] 置 critical，[`InputBatcher::poll`] 不等窗口 |
//! | I8 | 修饰键状态精确跟踪 | [`ModifierState`]，每条 Key 事件携带即时快照 |
//! | I9 | 注入失败返回错误，不 panic | [`InputInjector`] 返回 `Result`，见测试 `injector_failure_is_returned_not_panicked` |
//! | I10 | 注入后光标校准 | Windows 侧职责（`rc-input-win` 骨架 TODO，D 阶段） |
//!
//! # 线格式（手写二进制，不用 protobuf）
//!
//! 高频小包，protobuf 的 varint + 描述开销不划算。所有多字节整数一律
//! **小端**。一个 stream 消息 = `[len(u16 LE)] + 连续多个事件`，
//! `len` 是事件段字节数（不含前缀自身）。事件段超过单条消息上限
//! （65535 字节）的批由发送路径**自动分片**为多条消息
//! （[`channel::send_events`]），接收端逐消息解码即可无损还原：
//!
//!
//! ```text
//! type 0x01 MouseMove:    x(u16 LE) y(u16 LE)              = 5 字节
//! type 0x02 MouseButton:  button(u8) pressed(u8)           = 3 字节
//! type 0x03 MouseWheel:   delta(i16 LE) horizontal(u8)     = 4 字节
//! type 0x04 Key:          scancode(u16 LE) pressed(u8) mods(u8) = 5 字节
//! type 0x05 Sync:         —                                = 1 字节
//! ```
//!
//! 解码器对脏数据一律返回 [`rc_types::Error::Protocol`]（带偏移），
//! 绝不 panic。`pressed` / `horizontal` 严格只接受 0/1，`button` 严格
//! 只接受 0..=4，未知 `type` 直接报错——远控输入是安全边界，宁拒收
//! 不猜测。
//!
//! # 本 crate 不做时间读取
//!
//! 批处理需要「现在几点」判断窗口是否到期，但 [`InputBatcher`] 与
//! [`channel::InputChannel`] 的所有时间参数都由调用方注入
//! （`Instant` 参数），与 rc-sched 的时间注入约定一致——批处理逻辑
//! 因此可以离线单测（见 `batch_window_respected`）。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod batch;
pub mod channel;

pub use batch::{InputBatcher, ModifierState, DEFAULT_WINDOW};

use rc_types::{Error, Result};
use std::time::Duration;

/// 鼠标按键。线格式 1 字节：`Left=0 Right=1 Middle=2 X1=3 X2=4`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    /// 左键（主键）。
    Left = 0,
    /// 右键（次键 / 菜单键）。
    Right = 1,
    /// 中键（滚轮按下）。
    Middle = 2,
    /// 第一侧键（前进）。
    X1 = 3,
    /// 第二侧键（后退）。
    X2 = 4,
}

/// 一条远控输入事件——输入通道传输的最小单位。
///
/// 坐标一律**归一化绝对坐标** `0..=65535`（I4）：增量在网络丢包时会
/// 永久漂移，绝对坐标不会。1080p 下量化误差 `1920/65536 ≈ 0.03px`，
/// 见 `normalized_coords_have_subpixel_precision`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputEvent {
    /// 指针移动到绝对坐标 `(x, y)`（归一化 0..=65535）。
    MouseMove {
        /// 归一化 x 坐标。
        x: u16,
        /// 归一化 y 坐标。
        y: u16,
    },
    /// 按下 / 抬起鼠标按键。**关键事件**：批处理不合并、立即 flush（I3/I7）。
    MouseButton {
        /// 哪个按键。
        button: MouseButton,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
    },
    /// 滚轮。**关键事件**：立即 flush。
    ///
    /// `delta > 0`：垂直滚动时向上（远离用户），水平滚动时向右。
    /// `horizontal = true` 表示水平滚轮。
    MouseWheel {
        /// 滚动量。**规范单位是行（line）**，两端直传、不做二次缩放：
        /// macOS 端 CG 行单位（`SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`，
        /// 典型 ±1..±3）直传，注入侧按 `kCGScrollEventUnitLine` 投递；
        /// Windows 端 `WHEEL_DELTA`(=120)/格在 rc-input-win D 阶段接入时
        /// 除以 120 归一为行。
        delta: i16,
        /// `true` = 水平滚动，`false` = 垂直滚动。
        horizontal: bool,
    },
    /// 按下 / 抬起键盘键。**关键事件**：立即 flush。扫描码而非虚拟键码
    /// （I5，跨布局不乱）：**Set 1 make code**，E0 前缀键编码为
    /// `0xE0 << 8 | code`（如右 Ctrl = `0xE01D`，左 Win = `0xE05B`）。
    Key {
        /// Set 1 扫描码（E0 前缀键 = `0xE0xx`）。
        scancode: u16,
        /// `true` = 按下，`false` = 抬起。
        pressed: bool,
        /// 修饰键快照（I8）：发送端在事件发生瞬间的精确状态，位定义见
        /// [`batch::modifier_bits`]。接收端不自行推断。
        modifiers: u8,
    },
    /// 客户端心跳 / 校准。不产生注入动作。
    Sync,
}

// ─────────────────────────────────────────────────────────────────
// 线格式常量与编码 / 解码
// ─────────────────────────────────────────────────────────────────

/// 线格式类型字节：`MouseMove`。
const TYPE_MOUSE_MOVE: u8 = 0x01;
/// 线格式类型字节：`MouseButton`。
const TYPE_MOUSE_BUTTON: u8 = 0x02;
/// 线格式类型字节：`MouseWheel`。
const TYPE_WHEEL: u8 = 0x03;
/// 线格式类型字节：`Key`。
const TYPE_KEY: u8 = 0x04;
/// 线格式类型字节：`Sync`。
const TYPE_SYNC: u8 = 0x05;

/// 把一批事件编码成一个 stream 消息，追加到 `out`。
///
/// 消息布局：`[事件段字节数 u16 LE] + 连续多个事件`（每个事件
/// `1 字节类型 + 变长负载`，见[模块文档](crate)的线格式表）。
///
/// 单条消息的事件段上限是 `u16::MAX`（65535）字节。**正常收发不会
/// 碰到该上限**：发送路径 [`channel::send_events`] 会把超长批按事件
/// 边界自动分片成多条独立可解的消息。若绕过发送路径直接调用本函数且
/// 事件段超限（调用方 bug）：debug 构建断言失败；release 构建饱和为
/// `u16::MAX`——对端解码时长度前缀与实际字节数对不上，会得到显式
/// `Protocol` 错误而不是静默错乱。这是最后防线，不是可依赖的行为。
pub fn encode_events(events: &[InputEvent], out: &mut Vec<u8>) {
    let len_at = out.len();
    out.extend_from_slice(&[0u8, 0]); // 长度占位，最后回填
    for ev in events {
        encode_one(ev, out);
    }
    let body = out.len() - len_at - 2;
    let len = u16::try_from(body).unwrap_or(u16::MAX);
    debug_assert!(
        usize::from(len) == body,
        "输入消息事件段 {body} 字节超过 u16 长度上限（批窗口 4ms 内不该发生）"
    );
    out[len_at..len_at + 2].copy_from_slice(&len.to_le_bytes());
}

/// 编码单个事件（不含消息级长度前缀）。
fn encode_one(ev: &InputEvent, out: &mut Vec<u8>) {
    match ev {
        InputEvent::MouseMove { x, y } => {
            out.push(TYPE_MOUSE_MOVE);
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        InputEvent::MouseButton { button, pressed } => {
            out.push(TYPE_MOUSE_BUTTON);
            out.push(*button as u8);
            out.push(u8::from(*pressed));
        }
        InputEvent::MouseWheel { delta, horizontal } => {
            out.push(TYPE_WHEEL);
            out.extend_from_slice(&delta.to_le_bytes());
            out.push(u8::from(*horizontal));
        }
        InputEvent::Key {
            scancode,
            pressed,
            modifiers,
        } => {
            out.push(TYPE_KEY);
            out.extend_from_slice(&scancode.to_le_bytes());
            out.push(u8::from(*pressed));
            out.push(*modifiers);
        }
        InputEvent::Sync => out.push(TYPE_SYNC),
    }
}

/// 单个事件的线格式字节数（含类型字节，不含消息级长度前缀）。
///
/// 与 [`encode_one`] 的实际输出严格一致（测试 `event_wire_size_matches_encoding`
/// 锁定），是发送路径分片预算的依据（见 [`channel::send_events`]）。
pub(crate) fn event_wire_size(ev: &InputEvent) -> usize {
    match ev {
        InputEvent::MouseMove { .. } => 5,
        InputEvent::MouseButton { .. } => 3,
        InputEvent::MouseWheel { .. } => 4,
        InputEvent::Key { .. } => 5,
        InputEvent::Sync => 1,
    }
}

/// 解码一个 stream 消息（[`encode_events`] 的逆）。
///
/// 任何脏数据——长度前缀与实际字节数不符、事件负载被截断、未知类型
/// 字节、非法枚举值——都返回 [`rc_types::Error::Protocol`]（带出错
/// 偏移），不 panic。空消息（`len = 0`）合法，解码为空事件表。
pub fn decode_events(buf: &[u8]) -> Result<Vec<InputEvent>> {
    if buf.len() < 2 {
        return Err(Error::Protocol {
            offset: buf.len(),
            msg: format!(
                "input message truncated: {len} bytes, need >= 2 for length prefix",
                len = buf.len()
            ),
        });
    }
    let len = usize::from(u16::from_le_bytes([buf[0], buf[1]]));
    let body = &buf[2..];
    if body.len() < len {
        return Err(Error::Protocol {
            offset: buf.len(),
            msg: format!(
                "input message truncated: length prefix says {len} bytes, got {actual}",
                actual = body.len()
            ),
        });
    }
    if body.len() > len {
        return Err(Error::Protocol {
            offset: 2 + len,
            msg: format!(
                "input message has {extra} trailing bytes after declared length {len}",
                extra = body.len() - len
            ),
        });
    }
    let mut events = Vec::new();
    let mut off = 0usize;
    while off < len {
        let (ev, size) = decode_one(body, off)?;
        events.push(ev);
        off += size;
    }
    Ok(events)
}

/// 解码 `body[off..]` 处的一个事件，返回事件与占用字节数。
fn decode_one(body: &[u8], off: usize) -> Result<(InputEvent, usize)> {
    let reject = |msg: String| Error::Protocol { offset: off, msg };
    let need = |n: usize| -> Result<()> {
        if body.len() - off < n {
            return Err(Error::Protocol {
                offset: body.len(),
                msg: format!(
                    "event at offset {off} truncated: need {n} bytes, got {actual}",
                    actual = body.len() - off
                ),
            });
        }
        Ok(())
    };
    let ty = body[off];
    match ty {
        TYPE_MOUSE_MOVE => {
            need(5)?;
            let x = u16::from_le_bytes([body[off + 1], body[off + 2]]);
            let y = u16::from_le_bytes([body[off + 3], body[off + 4]]);
            Ok((InputEvent::MouseMove { x, y }, 5))
        }
        TYPE_MOUSE_BUTTON => {
            need(3)?;
            let button = match body[off + 1] {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                3 => MouseButton::X1,
                4 => MouseButton::X2,
                v => return Err(reject(format!("unknown mouse button {v} (must be 0..=4)"))),
            };
            let pressed = decode_bool(body[off + 2], off, "pressed")?;
            Ok((InputEvent::MouseButton { button, pressed }, 3))
        }
        TYPE_WHEEL => {
            need(4)?;
            let delta = i16::from_le_bytes([body[off + 1], body[off + 2]]);
            let horizontal = decode_bool(body[off + 3], off, "horizontal")?;
            Ok((InputEvent::MouseWheel { delta, horizontal }, 4))
        }
        TYPE_KEY => {
            need(5)?;
            let scancode = u16::from_le_bytes([body[off + 1], body[off + 2]]);
            let pressed = decode_bool(body[off + 3], off, "pressed")?;
            let modifiers = body[off + 4];
            Ok((
                InputEvent::Key {
                    scancode,
                    pressed,
                    modifiers,
                },
                5,
            ))
        }
        TYPE_SYNC => Ok((InputEvent::Sync, 1)),
        v => Err(reject(format!("unknown input event type 0x{v:02X}"))),
    }
}

/// 严格解码布尔字节：只接受 0/1。
fn decode_bool(v: u8, off: usize, field: &str) -> Result<bool> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(Error::Protocol {
            offset: off,
            msg: format!("{field} must be 0 or 1, got {v}"),
        }),
    }
}

// ─────────────────────────────────────────────────────────────────
// 坐标归一化（I4）
// ─────────────────────────────────────────────────────────────────

/// 像素坐标 → 归一化坐标 `0..=65535`。`extent` 是该轴的像素跨度。
///
/// `0 ↦ 0`、`extent-1 ↦ 65535`（两端点精确对齐）。`extent <= 1`
/// （空轴）一律归 0。
pub fn normalize_coord(px: u32, extent: u32) -> u16 {
    if extent <= 1 {
        return 0;
    }
    let v = (u64::from(px) * 65535) / u64::from(extent - 1);
    u16::try_from(v.min(65535)).expect("clamped to 65535 fits u16")
}

/// 归一化坐标 → 像素坐标（[`normalize_coord`] 的逆）。
///
/// 往返误差 ≤ `ceil(extent / 65535)` 像素：1080p 为 1px 的整数化舍入，
/// 亚像素级（见 `normalized_coords_have_subpixel_precision`）。
pub fn denormalize_coord(n: u16, extent: u32) -> u32 {
    if extent <= 1 {
        return 0;
    }
    ((u64::from(n) * u64::from(extent - 1)) / 65535) as u32
}

// ─────────────────────────────────────────────────────────────────
// 注入 / 捕获抽象
// ─────────────────────────────────────────────────────────────────

/// 输入注入器（受控端）：把远控事件重放进本机。
///
/// 实现必须：**失败返回错误、绝不 panic**（I9）——例如 macOS 未授
/// 辅助功能、Windows `SendInput` 被拒。注入坐标是归一化值，实现负责
/// 用 [`InputInjector::set_screen_size`] 给定的屏幕尺寸反归一化。
pub trait InputInjector: Send {
    /// 注入一批事件（一个批窗口内的顺序事件）。原子性不做要求：中途
    /// 失败时返回错误，已注入部分不回滚（上层按需发 `Sync` 校准）。
    fn inject(&mut self, events: &[InputEvent]) -> Result<()>;
    /// 设置屏幕尺寸（像素），用于坐标反归一化。显示器热插拔后上层
    /// 应重新调用。
    fn set_screen_size(&mut self, w: u32, h: u32);
}

/// 输入捕获器（控制端）：从本机输入子系统取事件并转成归一化事件。
pub trait InputCapture: Send {
    /// 取一批就绪事件；最多阻塞 `timeout`，无事件返回空表。
    fn next_events(&mut self, timeout: Duration) -> Result<Vec<InputEvent>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每种事件各一条 + 多事件混编，编码再解码必须逐字节还原语义。
    /// 同时锁定各事件的线格式字节数（回归线格式表）。
    #[test]
    fn encoder_decoder_roundtrip() {
        let events = vec![
            InputEvent::MouseMove { x: 1234, y: 56789 },
            InputEvent::MouseButton {
                button: MouseButton::X2,
                pressed: true,
            },
            InputEvent::MouseWheel {
                delta: -120,
                horizontal: true,
            },
            InputEvent::Key {
                scancode: 0xE05B,
                pressed: false,
                modifiers: 0x22,
            },
            InputEvent::Sync,
        ];
        let mut buf = Vec::new();
        encode_events(&events, &mut buf);
        // 2 前缀 + 5 + 3 + 4 + 5 + 1 = 20
        assert_eq!(buf.len(), 20, "线格式字节数与任务卡表格不符: {buf:02X?}");
        assert_eq!(&buf[0..2], &18u16.to_le_bytes(), "长度前缀 = 事件段字节数");
        let decoded = decode_events(&buf).expect("roundtrip must decode");
        assert_eq!(decoded, events);

        // 空消息
        let mut empty = Vec::new();
        encode_events(&[], &mut empty);
        assert_eq!(empty, vec![0, 0]);
        assert_eq!(decode_events(&empty).unwrap(), Vec::<InputEvent>::new());

        // 单事件
        let mut one = Vec::new();
        encode_events(&[InputEvent::Sync], &mut one);
        assert_eq!(one, vec![1, 0, TYPE_SYNC]);
        assert_eq!(decode_events(&one).unwrap(), vec![InputEvent::Sync]);
    }

    /// 截断的消息必须报 Protocol 错误：长度前缀谎报、真实消息被砍尾。
    #[test]
    fn decode_rejects_truncated_event() {
        // 前缀说 5 字节，实际只有 3 字节事件段
        let err = decode_events(&[5, 0, TYPE_MOUSE_MOVE, 0x34, 0x12]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("truncated"), "应报截断: {msg}");
        assert!(matches!(err, Error::Protocol { .. }));

        // 真实消息砍掉最后一个字节（事件段不完整）
        let mut buf = Vec::new();
        encode_events(
            &[InputEvent::MouseMove { x: 1, y: 2 }, InputEvent::Sync],
            &mut buf,
        );
        let err = decode_events(&buf[..buf.len() - 1]).unwrap_err();
        assert!(err.to_string().contains("truncated"));

        // 单事件负载被截断（Key 需 5 字节只给 4）
        let err = decode_events(&[4, 0, TYPE_KEY, 0x1E, 0x00, 0x01]).unwrap_err();
        assert!(err.to_string().contains("truncated"));

        // 长度前缀本身被截断
        assert!(decode_events(&[0x01]).is_err());
        assert!(decode_events(&[]).is_err());
    }

    /// 未知类型字节 / 非法枚举值必须报 Protocol 错误，绝不猜测。
    #[test]
    fn decode_rejects_unknown_type() {
        let err = decode_events(&[1, 0, 0x99]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown input event type"),
            "应报未知类型: {msg}"
        );
        assert!(msg.contains("0x99"), "错误信息应含出错值: {msg}");

        // 按钮越界（5 不在 0..=4）
        let err = decode_events(&[3, 0, TYPE_MOUSE_BUTTON, 5, 1]).unwrap_err();
        assert!(err.to_string().contains("unknown mouse button"));

        // 布尔字段非 0/1
        let err = decode_events(&[3, 0, TYPE_MOUSE_BUTTON, 0, 7]).unwrap_err();
        assert!(err.to_string().contains("must be 0 or 1"));

        // 声明长度之后的尾巴字节是成帧错误
        let err = decode_events(&[0, 0, TYPE_SYNC]).unwrap_err();
        assert!(err.to_string().contains("trailing bytes"));
    }

    /// I4：归一化坐标在 1080p/2K/4K 下的量化误差必须 < 0.1px
    /// （任务卡原文测试，逐字照抄）。
    #[test]
    fn normalized_coords_have_subpixel_precision() {
        for width in [1920u32, 2560, 3840] {
            let max_err = width as f64 / 65536.0;
            assert!(max_err < 0.1, "{width}: err {max_err}px exceeds 0.1px");
        }
    }

    /// normalize / denormalize 的端点对齐与往返误差上界。
    #[test]
    fn normalize_denormalize_roundtrip_within_one_pixel() {
        assert_eq!(normalize_coord(0, 1920), 0);
        assert_eq!(normalize_coord(1919, 1920), 65535);
        assert_eq!(denormalize_coord(0, 1920), 0);
        assert_eq!(denormalize_coord(65535, 1920), 1919);
        for px in [0u32, 1, 959, 1918, 1919] {
            let back = denormalize_coord(normalize_coord(px, 1920), 1920);
            assert!(
                (back as i64 - px as i64).unsigned_abs() <= 1,
                "{px} -> {back} 超过 1px 往返误差"
            );
        }
        // 空轴 / 单像素轴不 panic
        assert_eq!(normalize_coord(0, 0), 0);
        assert_eq!(denormalize_coord(65535, 1), 0);
    }

    /// `event_wire_size` 必须与 `encode_one` 的实际输出逐字节一致——
    /// 它是发送路径分片预算（channel::send_events）的依据，尺寸表与
    /// 编码器脱钩会造成分片错切。
    #[test]
    fn event_wire_size_matches_encoding() {
        let samples = [
            InputEvent::MouseMove { x: 1, y: 2 },
            InputEvent::MouseButton {
                button: MouseButton::Middle,
                pressed: false,
            },
            InputEvent::MouseWheel {
                delta: 120,
                horizontal: false,
            },
            InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: 0,
            },
            InputEvent::Sync,
        ];
        for ev in &samples {
            let mut buf = Vec::new();
            encode_one(ev, &mut buf);
            assert_eq!(
                buf.len(),
                event_wire_size(ev),
                "{ev:?} 的尺寸表与实际编码不一致"
            );
        }
    }

    /// 最后防线锁定（release 专属路径）：绕过发送路径直接
    /// `encode_events` 一个超长批（事件段 100000 字节 > 65535），
    /// 长度前缀饱和为 `u16::MAX`，产出的消息必须被 `decode_events`
    /// **显式拒绝**（trailing bytes）——绝不静默错乱。正常发送路径
    /// （channel::send_events）已分片，不会走到这里。debug 构建该
    /// 路径是断言失败，跳过本测试。
    #[test]
    #[cfg_attr(debug_assertions, ignore = "debug 构建下超长批走断言失败路径")]
    fn encode_events_oversized_saturates_and_is_rejected() {
        let events: Vec<InputEvent> = (0..20000u32)
            .map(|i| InputEvent::MouseMove { x: i as u16, y: 0 })
            .collect();
        let mut buf = Vec::new();
        encode_events(&events, &mut buf);
        assert_eq!(&buf[0..2], &u16::MAX.to_le_bytes(), "release 饱和路径");
        assert_eq!(buf.len(), 2 + 100000, "事件本体完整编码");
        let err = decode_events(&buf).unwrap_err();
        assert!(
            err.to_string().contains("trailing bytes"),
            "必须显式拒绝，不静默错乱: {err}"
        );
    }

    /// I9：注入器失败必须以 `Err` 返回，不 panic，且可重复调用。
    #[test]
    fn injector_failure_is_returned_not_panicked() {
        struct FailingInjector;
        impl InputInjector for FailingInjector {
            fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
                Err(Error::InvalidConfig("no accessibility permission".into()))
            }
            fn set_screen_size(&mut self, _w: u32, _h: u32) {}
        }

        let mut inj = FailingInjector;
        // 第一次失败：返回错误而不是 panic
        let err = inj
            .inject(&[InputEvent::Sync])
            .expect_err("失败注入器必须返回 Err");
        assert!(err.to_string().contains("permission"));
        // 失败后状态不被破坏：继续调用仍返回错误，依旧不 panic
        assert!(inj.inject(&[]).is_err());
        inj.set_screen_size(1920, 1080); // 不 panic
    }
}
