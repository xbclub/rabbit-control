//! RabbitControl 的 macOS 采集后端（任务卡 T12）。
//!
//! # 组成
//! - [`sck::SckBackend`]：ScreenCaptureKit 实现的
//!   [`CaptureBackend`](rc_capture::CaptureBackend)，工厂入口
//!   [`new_backend`]；
//! - [`permission`]：三重授权（屏幕录制 / 辅助功能 / 输入监控）的检测与
//!   引导，不弹窗、变更后提示重启进程；
//! - [`ffi`]：全部 `unsafe` 集中于此（ObjC 调用链与 C 权限函数），
//!   其余模块 `deny(unsafe_code)`。
//!
//! # 设计要点（对应 T12 的 M 条目）
//! - M5：`pixelFormat = '420v'`（NV12）—— SCK 交付的 `CVPixelBuffer` 与
//!   VideoToolbox 之间零拷贝（`rc-encode-vt` 直接吃句柄）；
//! - M7：`showsCursor = false`，光标由输入层单独绘制；
//! - M8：`SCStream` 停止后不可复用，`rebuild` 一律新建实例；
//! - M9：Retina 2x 输出逻辑分辨率（GPU 下采样，见 [`sck::SckConfig`]）；
//! - M10：权限撤销经 `stream:didStopWithError:` 上报为
//!   [`InvalidationReason::AccessLost`](rc_capture::InvalidationReason)，
//!   绝不 panic。
//!
//! # 契约
//! - C1（静止）：SCK 只在内容变化时回调，静止时 `next_frame` 超时返回
//!   `Ok(None)`；
//! - C2（时间戳）：帧时间戳取 CMSampleBuffer 的 PTS（内容变化时刻），
//!   首帧为原点；
//! - C4/C5：失效原因映射 `recoverable` 标志，`rebuild` 幂等。
//!
//! 非 macOS 目标上本 crate 编译为空壳（依赖与模块全部 cfg 门控），
//! 保证 workspace 在任何平台都能构建。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
pub mod ffi;
#[cfg(target_os = "macos")]
pub mod permission;
#[cfg(target_os = "macos")]
pub mod sck;

#[cfg(target_os = "macos")]
pub use permission::{PermissionKind, PermissionStatus};
#[cfg(target_os = "macos")]
pub use sck::{SckBackend, SckConfig};

/// 工厂：按配置创建 ScreenCaptureKit 采集后端。
///
/// 典型用法：
///
/// ```no_run
/// # #[cfg(target_os = "macos")]
/// # fn main() -> rc_types::Result<()> {
/// // 1. 先查三项授权（不弹窗）；缺则引导，全部具备才建流。
/// let status = rc_capture_macos::PermissionStatus::check();
/// if !status.all_granted() {
///     rc_capture_macos::PermissionStatus::open_settings(
///         &status,
///         rc_capture_macos::PermissionKind::ScreenRecording,
///     )?;
/// }
/// // 2. 建流（等用户重启进程授权生效后）。
/// let mut backend = rc_capture_macos::new_backend(rc_capture_macos::SckConfig::default())?;
/// # Ok(())
/// # }
/// # #[cfg(not(target_os = "macos"))]
/// # fn main() {}
/// ```
#[cfg(target_os = "macos")]
pub fn new_backend(config: SckConfig) -> rc_types::Result<SckBackend> {
    SckBackend::new(config)
}

/// 读取与 ScreenCaptureKit `CMSampleBuffer` PTS 同域的系统主机时钟（微秒）。
///
/// 供管线层在启动采集前采样会话零点并传入 [`SckConfig::origin_us`]，确保
/// `Captured` 与下游 `Diffed` / `EncodeDone` / `Presented` 处于完全一致的单调时间轴。
#[cfg(target_os = "macos")]
pub fn host_time_us() -> u64 {
    ffi::host_time_us()
}
