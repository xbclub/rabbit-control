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

/// 查询当前主显示器的逻辑分辨率（单位：点，已按偶数对齐）。
///
/// 同步查询 `CGDisplay::main().bounds()`，不触发屏幕录制权限弹窗；供被控端在握手阶段
/// 按主显示器真实宽高比设定协商上界，消除 ScreenCaptureKit 黑边与光标坐标偏移。
#[cfg(target_os = "macos")]
pub fn main_display_logical_resolution() -> Option<rc_types::Resolution> {
    ffi::main_display_logical_resolution()
}

/// 查询当前主显示器的物理像素分辨率（backing pixels，已按偶数对齐）。
///
/// 优先同步查询 `CGDisplay::main().display_mode()` 的物理像素尺寸（不可用时回退至
/// `pixels_wide()/pixels_high()`），不触发屏幕录制权限弹窗；供被控端在热切换分辨率时
/// 提供 Retina/超清模式上界。
#[cfg(target_os = "macos")]
pub fn main_display_backing_resolution() -> Option<rc_types::Resolution> {
    ffi::main_display_backing_resolution()
}

/// 主显示器可用桌面显示模式预设（已去重、优先 HiDPI 与高刷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayModePreset {
    /// 逻辑宽度（点）。
    pub width: u32,
    /// 逻辑高度（点）。
    pub height: u32,
    /// 物理像素宽度。
    pub pixel_width: u32,
    /// 物理像素高度。
    pub pixel_height: u32,
    /// 刷新率（Hz，未知时为 `0`）。
    pub refresh_hz: u32,
    /// 是否为 HiDPI 模式（`pixel_width > width`）。
    pub is_hidpi: bool,
    /// 是否为当前激活分辨率。
    pub is_current: bool,
}

/// 枚举主显示器可用桌面显示模式（960..=3840 × 600..=2160，按 `(width, height)` 降序）。
///
/// 非 macOS 平台返回空列表。
pub fn list_main_display_modes() -> Vec<DisplayModePreset> {
    #[cfg(target_os = "macos")]
    {
        ffi::list_main_display_modes()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

/// 主显示器显示模式 RAII 守卫：切换分辨率并在 [`Drop`] 时恢复原始模式。
pub struct DisplayModeGuard {
    #[cfg(target_os = "macos")]
    inner: ffi::DisplayModeGuard,
    #[cfg(not(target_os = "macos"))]
    original: rc_types::Resolution,
}

impl DisplayModeGuard {
    /// 捕获主显示器当前显示模式；失败或非 macOS 平台返回 `None`。
    pub fn capture_current() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            ffi::DisplayModeGuard::capture_current().map(|inner| Self { inner })
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// 捕获时记录的原始逻辑分辨率。
    pub fn original_resolution(&self) -> rc_types::Resolution {
        #[cfg(target_os = "macos")]
        {
            self.inner.original_resolution()
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.original
        }
    }

    /// 切换主显示器到匹配 `(target_w, target_h)` 的最佳可用模式，返回切换后的逻辑分辨率。
    pub fn switch_to(
        &mut self,
        target_w: u32,
        target_h: u32,
    ) -> Result<rc_types::Resolution, String> {
        #[cfg(target_os = "macos")]
        {
            self.inner.switch_to(target_w, target_h)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (target_w, target_h);
            Err("display mode switching is only supported on macOS".to_string())
        }
    }

    /// 恢复为捕获时的原始显示模式（幂等）。
    pub fn restore(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.inner.restore();
        }
    }
}

impl Drop for DisplayModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// 获取主显示器桌面壁纸的低体积 JPEG 缩略图（零 TCC 弹窗）。
///
/// 非 macOS 平台固定返回 `None`。
pub fn main_display_wallpaper_jpeg(max_width: u32, max_height: u32) -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    {
        ffi::main_display_wallpaper_jpeg(max_width, max_height)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (max_width, max_height);
        None
    }
}

/// RAII 实时会话活动声明：持有期间阻止 macOS App Nap 与后台降频节流。
///
/// 非 macOS 平台为零开销空操作。
pub struct LatencyCriticalActivity {
    #[cfg(target_os = "macos")]
    _inner: ffi::LatencyCriticalActivity,
}

#[cfg(not(target_os = "macos"))]
impl Drop for LatencyCriticalActivity {
    fn drop(&mut self) {}
}

impl LatencyCriticalActivity {
    /// 开始一项高优先级实时活动；在返回的 guard 被 Drop 时自动结束。
    pub fn begin(reason: &str, keep_display_awake: bool) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                _inner: ffi::LatencyCriticalActivity::begin(reason, keep_display_awake),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (reason, keep_display_awake);
            Self {}
        }
    }
}

/// 将当前工作线程提升到用户交互实时调度等级（macOS `QOS_CLASS_USER_INTERACTIVE`）。
///
/// 非 macOS 平台为零开销空操作。
pub fn promote_current_thread_qos() {
    #[cfg(target_os = "macos")]
    {
        ffi::promote_current_thread_qos();
    }
}

/// 立即锁定当前 macOS 桌面会话（仅 macOS 有效，非 macOS 平台返回 `false`）。
pub fn lock_screen_now() -> bool {
    #[cfg(target_os = "macos")]
    {
        ffi::lock_screen_now()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn lock_screen_symbol_is_resolvable_without_invoking() {
        assert!(
            ffi::has_lock_screen_symbol(),
            "macOS login.framework 应导出 SACLockScreenImmediate 符号"
        );
    }

    #[test]
    fn latency_activity_and_qos_do_not_panic() {
        promote_current_thread_qos();
        let activity = LatencyCriticalActivity::begin("RabbitControl Unit Test\0Check", true);
        drop(activity);
    }
    use std::collections::HashSet;

    #[test]
    fn display_mode_guard_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<DisplayModeGuard>();
    }

    #[test]
    fn list_main_display_modes_returns_sorted_deduped_presets() {
        let modes = list_main_display_modes();
        let mut seen = HashSet::new();
        for pair in modes.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            assert!(
                (a.width, a.height) > (b.width, b.height),
                "显示模式必须按 (width, height) 严格降序：{a:?} vs {b:?}"
            );
        }
        for m in &modes {
            assert!((960..=3840).contains(&m.width), "width 越界: {m:?}");
            assert!((600..=2160).contains(&m.height), "height 越界: {m:?}");
            assert!(seen.insert((m.width, m.height)), "存在重复分辨率项: {m:?}");
            assert_eq!(m.is_hidpi, m.pixel_width > m.width);
        }
        #[cfg(target_os = "macos")]
        if let Some(guard) = DisplayModeGuard::capture_current() {
            let orig = guard.original_resolution();
            assert!(orig.width >= 2 && orig.height >= 2);
        }
    }

    #[test]
    fn main_display_wallpaper_jpeg_respects_bounds_and_magic() {
        assert!(main_display_wallpaper_jpeg(0, 400).is_none());
        assert!(main_display_wallpaper_jpeg(640, 0).is_none());
        if let Some(jpeg) = main_display_wallpaper_jpeg(640, 400) {
            assert!(
                jpeg.len() <= 60 * 1024,
                "壁纸缩略图超出 60 KiB: {}",
                jpeg.len()
            );
            assert!(
                jpeg.starts_with(&[0xFF, 0xD8]),
                "输出必须是有效 JPEG（以 FF D8 开头）"
            );
        }
    }
}
