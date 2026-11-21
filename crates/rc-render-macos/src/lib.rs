//! RabbitControl 的 macOS 呈现后端（任务卡 T05 的呈现项）。
//!
//! [`MetalRenderer`]（实现 [`rc_render::Renderer`]）把解码输出的
//! [`CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer) 画到
//! `CAMetalLayer` 上。
//!
//! # 延迟红线（任务卡 T05）
//! - **D1 零拷贝**：`CVPixelBuffer` 经 `CVMetalTextureCache` 包装成
//!   `MTLTexture` 直接交给渲染管线采样，全程 GPU 内存，**禁止 CPU 回读**；
//!   不支持的纹理变体（CPU 帧）以 [`Error::InvalidFrame`](rc_types::Error::InvalidFrame)
//!   拒绝，而不是偷偷走 CPU 路径；
//! - **D2 关 vsync**：`CAMetalLayer.displaySyncEnabled = false`（macOS 等价于
//!   Windows 的 `Present(0)`），初始必须为关；
//! - **D6 呈现失败可恢复**：`nextDrawable` 为空 / 命令缓冲失败等
//!   device-lost 类失败返回 `Capture { recoverable: true }`（ Metal 没有
//!   D3D 式显式 device lost；这些失败重建渲染器后即可继续），绝不 panic；
//! - **D7 光标本地渲染**：光标**不进视频管线**（否则位置滞后一帧）——
//!   位置 / 可见性经
//!   [`Renderer::set_cursor`](rc_render::Renderer::set_cursor)
//!   独立低延迟更新，呈现时按「当下」位置在视频 quad 之后本地叠加
//!   （GPU 里 alpha 混合，延迟感知为 0）；形状
//!   （[`CursorShape`](rc_render::CursorShape)）按 id **缓存**，
//!   只在换光标样式时上传一次 GPU 纹理（同 id 不重传）；
//! - **D8 呈现打点在返回后**：`present` 在 commit 返回**之后**按会话时钟
//!   打点，在入口打点会让延迟测量偏小。
//!
//! # 尺寸语义
//! `resize` / [`MetalRendererConfig::size`] 一律是**物理像素**（即
//! `CAMetalLayer.drawableSize` 的单位，backing store 尺寸）。NSView 路径的
//! 调用方负责 `view.bounds × backingScaleFactor` 的换算（高 DPI 坑 6）。
//!
//! # 测试策略
//! **无窗口路径优先**：`CAMetalLayer` 不挂 `NSWindow` 也能创建 drawable，
//! 单测全部走离屏 layer（含 D7 光标叠加的 GPU 像素级回读验证）；需要
//! 真机跑的吞吐测试（300 帧 P50）标了 `#[ignore = "requires macOS + GPU
//! session"]`。**真实窗口**的完整 present 路径（含 `nextDrawable`）与
//! 60fps×60s soak 在 `tests/window_present.rs`（`window-bench` feature，
//! harness = false——NSWindow 是 MainThreadOnly，只有 `fn main` 在主线程；
//! 运行方式见该文件文档）。NSView / 窗口路径经
//! [`MetalRenderer::from_raw_layer`] 做成可选构造，不进单测。
//!
//! # 结构
//! - [`renderer`]：纯 Rust 的状态机 + [`Renderer`](rc_render::Renderer) 实现；
//! - [`ffi`]：**全部 `unsafe` 集中于此**（Metal / CoreVideo / QuartzCore 调用链），
//!   每个 `unsafe` 块都有 `// SAFETY:` 注释，其余模块 `deny(unsafe_code)`。
//!
//! 非 macOS 目标上本 crate 编译为空壳（依赖与模块全部 cfg 门控），
//! 保证 workspace 在任何平台都能构建。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub(crate) mod ffi;
#[cfg(target_os = "macos")]
mod renderer;
#[cfg(target_os = "macos")]
mod shader;

#[cfg(target_os = "macos")]
pub use renderer::{MetalRenderer, MetalRendererConfig};

/// 真实窗口基准的安全封装（`window-bench` feature，非默认开启）。
///
/// 供 `tests/window_present.rs`（harness = false，`fn main` 在主线程——
/// NSWindow 是 MainThreadOnly 类型，libtest 的测试体永远不在主线程）
/// 使用；unsafe 全部留在 ffi 边界（本 crate 的约定：unsafe 只在
/// `ffi` 模块）。运行方式见该测试文件。
#[cfg(all(target_os = "macos", feature = "window-bench"))]
pub use ffi::window_bench;

/// 光标状态（D7）：位置与可见性的缓存快照。
///
/// 复用抽象层的类型（单一事实来源），含义见
/// [`rc_render::CursorState`]。
#[cfg(target_os = "macos")]
pub use rc_render::CursorState;
