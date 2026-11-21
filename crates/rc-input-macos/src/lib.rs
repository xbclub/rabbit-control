//! RabbitControl 的 macOS 输入：CGEvent 注入与捕获（任务卡 T12 的 CGEvent 部分）。
//!
//! # 权限前置（必读）
//!
//! 通过 `CGEventPost` 注入合成事件，系统要求本进程持有**辅助功能授权**
//! （Accessibility：系统设置 → 隐私与安全性 → 辅助功能）。未授权时
//! `CGEventPost` **不返回错误、也不产生任何事件**——静默失败，这正是
//! T12 的 M2 陷阱。因此 [`Injector::new`] 在构造时用 `AXIsProcessTrusted()`
//! 预检，未授权直接返回 [`InputError::NotTrusted`]（附系统设置路径），
//! 绝不静默失败。捕获侧的 `CGEventTap`（ListenOnly）同样需要授权，
//! [`CgCapture::new`] 沿用同一预检模式。注意授权在系统设置里打开后
//! **必须重启进程**才生效（macOS 限制，T12 M4）。
//!
//! # 分层
//!
//! | 模块 | 平台 | 职责 |
//! |---|---|---|
//! | [`event`] | 全平台 | 输入事件最小类型集合（`PointerMove` / `PointerButton` / `PointerScroll` / `Key`） |
//! | [`keymap`] | 全平台 | 逻辑键 ↔ macOS 虚拟键码（`kVK_*`，逐值对照本机 SDK 的 Events.h） |
//! | [`scancode`] | 全平台 | 逻辑键 ↔ Windows Set 1 扫描码（T10 线格式的键编码） |
//! | [`plan`] | 全平台 | 事件 → 待注入 CGEvent 的中间表示（坐标钳制、滚动取整），[`plan::EventPoster`] 注入缝 |
//! | [`injector`] | 仅 macOS | [`Injector`]：权限预检 + 钳制 + 投递 |
//! | [`capture`] | 仅 macOS | [`CgCapture`]：权限预检 + CGEventTap（ListenOnly 专用线程）→ `rc_input::InputCapture` |
//! | [`bridge`] | 仅 macOS | [`InputBridge`]：实现 `rc_input::InputInjector`，把远控归一化事件接进本 crate |
//! | [`ffi`] | 仅 macOS | 本 crate 全部 `unsafe` 与 CGEvent 调用集中地（注入 `CgPoster` + 捕获 `tap`） |
//!
//! [`event`] / [`keymap`] / [`scancode`] / [`plan`] 是纯数据与纯函数，**无授权也能单元测试**；
//! 投递动作用 [`plan::EventPoster`] 抽象，测试注入假实现；捕获映射是纯函数
//! （[`capture::CgCapture`] 内部）。只有真投递/真捕获（`#[ignore]` 的
//! `inject_moves_cursor` / `capture_inject_loopback_real_machine`）需要辅助功能授权。
//! 非 macOS 目标上注入相关模块不编译，本 crate 主体仍可构建测试。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod error;
pub mod event;
pub mod keymap;
pub mod plan;
pub mod scancode;

#[cfg(target_os = "macos")]
pub mod bridge;
#[cfg(target_os = "macos")]
pub mod capture;

/// 真机测试串行化锁（仅测试构建可见）。
///
/// libtest 默认**并行**跑测试；真机 `#[ignore]` 测试会真的注入/捕获
/// 系统输入事件，并行时互相污染捕获窗（实测：滚轮测试注入的移动与
/// 滚动事件泄进回环测试的断言窗）。所有真机测试必须在入口先拿本锁。
#[cfg(test)]
pub(crate) mod rm_lock {
    use std::sync::Mutex;
    pub static REAL_MACHINE: Mutex<()> = Mutex::new(());
}

#[cfg(target_os = "macos")]
pub mod ffi;
#[cfg(target_os = "macos")]
pub mod injector;

pub use error::InputError;
pub use event::{InputEvent, PointerButtonKind};
pub use keymap::Key;
pub use plan::{EventPoster, PlannedEvent, PlannedMouseKind, ScreenBounds};
pub use scancode::{key_from_set1, set1_scancode};

#[cfg(target_os = "macos")]
pub use bridge::InputBridge;
#[cfg(target_os = "macos")]
pub use capture::CgCapture;
#[cfg(target_os = "macos")]
pub use ffi::CgPoster;
#[cfg(target_os = "macos")]
pub use injector::Injector;
