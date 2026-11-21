//! 呈现抽象层：平台无关的 [`Renderer`] trait 与假渲染器。
//!
//! # 职责边界
//! - 本 crate 只定义「呈现侧的形状」：trait 与光标状态类型。
//! - 真正的实现（D3D11 swapchain / Metal layer）在各自的平台 crate 里
//!   实现 [`Renderer`]，本 crate **不含任何平台依赖**。
//! - [`fake::FakeRenderer`] 是记录呈现时刻 / 尺寸 / 光标状态的假渲染器，
//!   供回环测试与无 GPU 的 CI 使用。
//!
//! # 延迟红线（任务卡 T05）
//! - **关闭 vsync（D2）**：`present` 实现必须以 `Present(0)` 一类的
//!   不等待方式呈现，强制 vsync 每帧 +16.7ms；初始状态必须为 vsync 关；
//! - **呈现失败可恢复（D6）**：device lost 等失败返回 [`Error`]（不
//!   panic），由上层重建渲染器后继续；
//! - **光标本地渲染（D7）**：光标不进视频管线（否则位置滞后一帧），
//!   经 [`Renderer::set_cursor`] 单独更新，延迟感知为 0；形状
//!   （[`cursor::CursorShape`]）低频更新、按 id 缓存，呈现时按当下
//!   位置本地叠加（几何见 [`cursor::cursor_rect`]）；
//! - **呈现打点在返回后（D8）**：[`Renderer::present`] 返回的时间戳在
//!   呈现调用**返回之后**打——在入口打点会让延迟测量偏小。
//!
//! # 时间注入
//! 实现内部**不读系统时钟**：呈现时刻由实现注入（平台层在 `Present`
//! 返回后取 `LastPresentTime` 一类的时间戳；假渲染器按逻辑时钟推进）。
//!
//! # 不做什么（任务卡「不要做什么」）
//! 不做窗口管理 / UI、不在呈现路径做色彩空间转换、不实现缩放算法
//! （双线性由平台层）。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod cursor;
pub mod fake;

pub use cursor::{CursorRect, CursorShape};
pub use fake::{CursorState, FakeRenderer, FakeRendererConfig};
pub use rc_types::{Error, Result};

use rc_decode::DecodedFrame;
use rc_types::Timestamp;

/// 帧呈现器。实现必须满足 D2/D6/D7/D8（见 crate 文档）。
pub trait Renderer: Send {
    /// 呈现一帧。必须关闭 vsync。
    ///
    /// 返回的 [`Timestamp`] 是呈现完成时刻：**在呈现调用返回之后打点**
    /// （D8），在入口打点会让延迟测量偏小。呈现失败（device lost 等）
    /// 返回 [`Error`] 而不是 panic（D6），上层据此重建渲染器。
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp>;

    /// 重建 / 调整呈现目标尺寸（窗口 resize、分辨率切换）。
    ///
    /// 非法尺寸（任一边为 0）以 [`Error::InvalidConfig`] 拒绝，且不得
    /// 改变当前状态。
    fn resize(&mut self, size: (u32, u32)) -> Result<()>;

    /// 开关 vsync。初始必须为关（D2），仅诊断 / 对比测试才打开。
    fn set_vsync(&mut self, on: bool);

    /// 光标单独渲染（延迟感知为 0）
    ///
    /// 光标不进视频管线（D7：否则位置滞后一帧）；实现应缓存位置与
    /// 可见性，在呈现层本地叠加。
    fn set_cursor(&mut self, pos: (i32, i32), visible: bool);
}
