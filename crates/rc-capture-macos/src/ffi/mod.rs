//! ScreenCaptureKit / 系统权限的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - 权限相关的 C 函数（`CGPreflightScreenCaptureAccess` 等）在这里声明与包装；
//! - ScreenCaptureKit 的 ObjC 调用链（`SCShareableContent` 枚举 → `SCContentFilter`
//!   → `SCStream` 构建与启动）在这里完成；
//! - 帧回调类（`RcSckStreamHandler`，实现 `SCStreamOutput` + `SCStreamDelegate`）
//!   在这里定义，回调里只做轻量提取并把事件投递进最新帧优先单槽邮箱（坑 6）。
//!
//! 对外（`sck.rs` / `permission.rs`）只暴露**安全**的包装函数与类型，
//! 每个 `unsafe` 块都带 `// SAFETY:` 注释。
//!
//! 本模块的存在使 crate 其余部分可以 `deny(unsafe_code)`。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFArray, CFBoolean,
    CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use rc_types::Resolution;

// ─────────────────────────────────────────────────────────────────────────────
// C 函数声明（权限三件套，M1/M2/M3）
// ─────────────────────────────────────────────────────────────────────────────

mod raw {
    use super::c_void;
    use objc2_core_foundation::{CFArray, CFDictionary, CFString};

    // CGWindow.h / CGDirectDisplay.h / CGDisplayConfiguration.h
    // 与 core-graphics crate 链接同一个 CoreGraphics 框架。
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub static kCGDisplayShowDuplicateLowResolutionModes: Option<&'static CFString>;

        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGRequestScreenCaptureAccess() -> bool;

        pub fn CGDisplayCopyAllDisplayModes(
            display: u32,
            options: *const CFDictionary,
        ) -> *mut CFArray;
        pub fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        pub fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
        pub fn CGDisplayModeGetRefreshRate(mode: *const c_void) -> f64;
        pub fn CGDisplayModeIsUsableForDesktopGUI(mode: *const c_void) -> bool;

        pub fn CGBeginDisplayConfiguration(config: *mut *mut c_void) -> i32;
        pub fn CGConfigureDisplayWithDisplayMode(
            config: *mut c_void,
            display: u32,
            mode: *const c_void,
            options: *const c_void,
        ) -> i32;
        pub fn CGCompleteDisplayConfiguration(config: *mut c_void, option: u32) -> i32;
        pub fn CGCancelDisplayConfiguration(config: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
        pub fn CFDataCreateMutable(allocator: *const c_void, capacity: isize) -> *mut c_void;
        pub fn CFDataGetLength(the_data: *const c_void) -> isize;
        pub fn CFDataGetBytePtr(the_data: *const c_void) -> *const u8;
    }

    extern "C" {
        pub fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
        pub fn dlopen(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
        pub fn dlclose(handle: *mut c_void) -> std::ffi::c_int;
    }

    #[link(name = "ImageIO", kind = "framework")]
    extern "C" {
        pub static kCGImageSourceCreateThumbnailFromImageAlways: Option<&'static CFString>;
        pub static kCGImageSourceThumbnailMaxPixelSize: Option<&'static CFString>;
        pub static kCGImageSourceCreateThumbnailWithTransform: Option<&'static CFString>;
        pub static kCGImageDestinationLossyCompressionQuality: Option<&'static CFString>;

        pub fn CGImageSourceCreateWithURL(
            url: *const c_void,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageSourceCreateThumbnailAtIndex(
            isrc: *const c_void,
            index: usize,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageDestinationCreateWithData(
            data: *mut c_void,
            image_type: *const CFString,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        pub fn CGImageDestinationAddImage(
            idst: *mut c_void,
            image: *const c_void,
            properties: *const c_void,
        );
        pub fn CGImageDestinationFinalize(idst: *mut c_void) -> bool;
    }

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    // AXUIElement.h：`Boolean AXIsProcessTrusted(void)`（10.4+）。
    // Boolean == unsigned char == u8。注意这个变体**没有 options 参数、绝不弹窗**。
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        pub fn AXIsProcessTrusted() -> u8;
    }

    // IOHIDLib.h（10.15+）：
    //   IOHIDAccessType IOHIDCheckAccess(IOHIDRequestType requestType)
    // IOHIDRequestType: PostEvent=0, ListenEvent=1（输入监控是 ListenEvent）。
    // IOHIDAccessType:  Granted=0, Denied=1, Unknown=2。C enum 按 int 传。
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        pub fn IOHIDCheckAccess(request_type: i32) -> i32;
    }
}

const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: i32 = 1;
const K_IOHID_ACCESS_TYPE_GRANTED: i32 = 0;

/// M1：检测屏幕录制权限，**不弹窗**（`CGPreflightScreenCaptureAccess`）。
pub(crate) fn preflight_screen_capture_access() -> bool {
    // SAFETY: 纯查询函数，无前置条件、无副作用（不会触发 TCC 弹窗）。
    unsafe { raw::CGPreflightScreenCaptureAccess() }
}

/// 请求屏幕录制权限：首次调用会弹系统授权对话框，**之后不再弹**（坑 3）。
pub(crate) fn request_screen_capture_access() -> bool {
    // SAFETY: 纯权限请求；返回值表示当前是否已授权。
    unsafe { raw::CGRequestScreenCaptureAccess() }
}

/// M2：检测辅助功能权限，**不弹窗**（`AXIsProcessTrusted()` 无 options 变体）。
pub(crate) fn ax_is_process_trusted() -> bool {
    // SAFETY: 无参纯查询；Boolean(0/1) 转 bool。
    unsafe { raw::AXIsProcessTrusted() != 0 }
}

/// M3：检测输入监控权限，**不弹窗**（`IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)`）。
pub(crate) fn iohid_check_input_monitoring() -> bool {
    // SAFETY: 纯查询；参数是合法枚举值 ListenEvent。
    unsafe {
        raw::IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) == K_IOHID_ACCESS_TYPE_GRANTED
    }
}

/// 主显示器 CGDirectDisplayID（用于把配置里的 display=0 映射到主屏）。
pub(crate) fn main_display_id() -> u32 {
    // SAFETY: 纯查询，无前置条件。
    unsafe { core_graphics::display::CGMainDisplayID() }
}

/// 查询主显示器逻辑分辨率（单位：点，向下取偶以匹配 YUV 4:2:0 / 编码器要求）。
///
/// 使用 CoreGraphics `CGDisplay::main().bounds()` 同步查询，不触发任何 TCC 权限弹窗，
/// 返回值与 ScreenCaptureKit `display = 0`（主显示器）的逻辑坐标系完全一致。
pub(crate) fn main_display_logical_resolution() -> Option<Resolution> {
    let bounds = core_graphics::display::CGDisplay::main().bounds();
    let w = bounds.size.width;
    let h = bounds.size.height;
    if !w.is_finite() || !h.is_finite() || w < 2.0 || h < 2.0 {
        return None;
    }
    let res = Resolution::new(w.round() as u32, h.round() as u32).to_even();
    (res.width >= 2 && res.height >= 2).then_some(res)
}

/// 查询主显示器物理像素分辨率（backing pixels，向下取偶以匹配 YUV 4:2:0 / 编码器要求）。
///
/// 优先使用 `CGDisplay::main().display_mode()` 的 `pixel_width/pixel_height` 获取真实 Retina
/// 物理像素（例如 14" MBP `3024×1964`），不可用时回退至 `pixels_wide/pixels_high`；
/// 全程同步查询 CoreGraphics，绝不触发任何 TCC 权限弹窗。
pub(crate) fn main_display_backing_resolution() -> Option<Resolution> {
    let display = core_graphics::display::CGDisplay::main();
    let (w, h) = match display.display_mode() {
        Some(mode) if mode.pixel_width() >= 2 && mode.pixel_height() >= 2 => {
            (mode.pixel_width(), mode.pixel_height())
        }
        _ => (display.pixels_wide(), display.pixels_high()),
    };
    let w = u32::try_from(w).ok()?;
    let h = u32::try_from(h).ok()?;
    if w < 2 || h < 2 {
        return None;
    }
    let res = Resolution::new(w, h).to_even();
    (res.width >= 2 && res.height >= 2).then_some(res)
}

// ─────────────────────────────────────────────────────────────────────────────
// 显示模式枚举与动态切换（Item #4）+ 桌面壁纸缩略图（Item #6）
// ─────────────────────────────────────────────────────────────────────────────

/// `kCGConfigureForSession`（CGDisplayConfiguration.h = 1）：仅当前登录会话生效。
const K_CG_CONFIGURE_FOR_SESSION: u32 = 1;

/// 通用 +1 所有权 CF 对象 RAII 包装，确保任何分支退出都释放 CFRelease。
struct CfOwned(NonNull<c_void>);

impl CfOwned {
    fn from_raw(ptr: *mut c_void) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }

    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        // SAFETY: self.0 由 Create/Copy 规则拥有 +1 引用，此处配对 CFRelease。
        unsafe { raw::CFRelease(self.0.as_ptr().cast_const()) };
    }
}

/// 构造 `{ kCGDisplayShowDuplicateLowResolutionModes: kCFBooleanTrue }` 选项字典。
fn duplicate_low_res_modes_options() -> Option<CFRetained<CFDictionary>> {
    // SAFETY: 读取框架导出的只读 extern static CFString 常量指针。
    let key = unsafe { raw::kCGDisplayShowDuplicateLowResolutionModes }?;
    let val = CFBoolean::new(true);
    let keys = [key as *const CFString as *const c_void];
    let values = [val as *const CFBoolean as *const c_void];
    // SAFETY: keys/values 为 1 个元素长的有效指针数组，key/val 均为 CF 对象。
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
}

fn copy_all_display_modes(
    display_id: u32,
    opts: Option<&CFDictionary>,
) -> Option<CFRetained<CFArray>> {
    let opts_ptr = opts
        .map(|d| d as *const CFDictionary)
        .unwrap_or_else(std::ptr::null);
    // SAFETY: opts_ptr 为 null 或有效 CFDictionary；返回值为 +1 retained CFArray（可为 null）。
    let ptr = unsafe { raw::CGDisplayCopyAllDisplayModes(display_id, opts_ptr) };
    // SAFETY: CGDisplayCopyAllDisplayModes 遵循 Copy Rule，非空时返回 +1 所有权引用。
    NonNull::new(ptr).map(|nn| unsafe { CFRetained::from_raw(nn) })
}

fn mode_metrics(mode: *const c_void) -> Option<(u32, u32, u32, u32, u32, bool)> {
    // SAFETY: mode 为有效 CGDisplayModeRef，以下均为纯只读属性查询。
    let usable = unsafe { raw::CGDisplayModeIsUsableForDesktopGUI(mode) };
    if !usable {
        return None;
    }
    // SAFETY: 同上，mode 有效。
    let (w, h, pw, ph, hz_raw) = unsafe {
        (
            raw::CGDisplayModeGetWidth(mode),
            raw::CGDisplayModeGetHeight(mode),
            raw::CGDisplayModeGetPixelWidth(mode),
            raw::CGDisplayModeGetPixelHeight(mode),
            raw::CGDisplayModeGetRefreshRate(mode),
        )
    };
    let w = u32::try_from(w).ok()?;
    let h = u32::try_from(h).ok()?;
    let pw = u32::try_from(pw).ok()?;
    let ph = u32::try_from(ph).ok()?;
    let refresh_hz = if hz_raw.is_finite() && hz_raw > 0.0 {
        hz_raw.round() as u32
    } else {
        0
    };
    let is_hidpi = pw > w;
    Some((w, h, pw, ph, refresh_hz, is_hidpi))
}

/// 枚举主显示器可用桌面显示模式（960..=3840 × 600..=2160，去重偏好 HiDPI 与高刷）。
pub(crate) fn list_main_display_modes() -> Vec<crate::DisplayModePreset> {
    let display_id = main_display_id();
    let opts = duplicate_low_res_modes_options();
    let Some(modes) = copy_all_display_modes(display_id, opts.as_deref()) else {
        return Vec::new();
    };

    // SAFETY: 纯查询当前显示模式，返回值遵循 Copy Rule（+1 或 null）。
    let cur_mode = CfOwned::from_raw(unsafe { raw::CGDisplayCopyDisplayMode(display_id) });
    let cur_dims = cur_mode.as_ref().and_then(|m| {
        // SAFETY: m.as_ptr() 为有效 CGDisplayModeRef。
        let (w, h) = unsafe {
            (
                raw::CGDisplayModeGetWidth(m.as_ptr()),
                raw::CGDisplayModeGetHeight(m.as_ptr()),
            )
        };
        Some((u32::try_from(w).ok()?, u32::try_from(h).ok()?))
    });

    let mut dedup: BTreeMap<(u32, u32), crate::DisplayModePreset> = BTreeMap::new();
    let count = modes.count();
    for i in 0..count {
        // SAFETY: i < count 在合法范围内；modes 在整个循环内保活。
        let raw_mode = unsafe { modes.value_at_index(i) };
        if raw_mode.is_null() {
            continue;
        }
        let Some((w, h, pw, ph, refresh_hz, is_hidpi)) = mode_metrics(raw_mode) else {
            continue;
        };
        if !(960..=3840).contains(&w) || !(600..=2160).contains(&h) {
            continue;
        }
        let candidate = crate::DisplayModePreset {
            width: w,
            height: h,
            pixel_width: pw,
            pixel_height: ph,
            refresh_hz,
            is_hidpi,
            is_current: cur_dims == Some((w, h)),
        };
        match dedup.get(&(w, h)) {
            Some(existing)
                if (existing.is_hidpi, existing.refresh_hz)
                    >= (candidate.is_hidpi, candidate.refresh_hz) => {}
            _ => {
                dedup.insert((w, h), candidate);
            }
        }
    }

    let mut result: Vec<crate::DisplayModePreset> = dedup.into_values().collect();
    result.sort_by(|a, b| b.width.cmp(&a.width).then_with(|| b.height.cmp(&a.height)));
    result
}

/// 主显示器显示模式 RAII 守卫：切换分辨率并在退出/Drop 时自动恢复原始模式。
pub(crate) struct DisplayModeGuard {
    display_id: u32,
    original_mode: NonNull<c_void>,
    original_res: Resolution,
    modified: bool,
}

// SAFETY: CGDisplayModeRef 是 CoreGraphics 不可变 CF 对象（CFRetain/CFRelease 线程安全），
// DisplayModeGuard 独占其 +1 所有权，且所有可变操作仅经 `&mut self` 进行。
unsafe impl Send for DisplayModeGuard {}

impl DisplayModeGuard {
    /// 捕获主显示器当前显示模式（失败返回 `None`）。
    pub(crate) fn capture_current() -> Option<Self> {
        let display_id = main_display_id();
        // SAFETY: CGDisplayCopyDisplayMode 返回 +1 所有权引用或 null。
        let mode_ptr = NonNull::new(unsafe { raw::CGDisplayCopyDisplayMode(display_id) })?;
        // SAFETY: mode_ptr 为有效 CGDisplayModeRef。
        let (w, h) = unsafe {
            (
                raw::CGDisplayModeGetWidth(mode_ptr.as_ptr()),
                raw::CGDisplayModeGetHeight(mode_ptr.as_ptr()),
            )
        };
        let original_res = main_display_logical_resolution().or_else(|| {
            let w = u32::try_from(w).ok()?;
            let h = u32::try_from(h).ok()?;
            let r = Resolution::new(w, h).to_even();
            (r.width >= 2 && r.height >= 2).then_some(r)
        });
        let Some(original_res) = original_res else {
            // SAFETY: mode_ptr 拥有 +1 引用，失败分支立即释放。
            unsafe { raw::CFRelease(mode_ptr.as_ptr().cast_const()) };
            return None;
        };
        Some(Self {
            display_id,
            original_mode: mode_ptr,
            original_res,
            modified: false,
        })
    }

    /// 捕获时记录的主显示器原始逻辑分辨率。
    pub(crate) fn original_resolution(&self) -> Resolution {
        self.original_res
    }

    /// 切换主显示器到匹配 `(target_w, target_h)` 的最佳可用模式（偏好 HiDPI 与高刷）。
    pub(crate) fn switch_to(&mut self, target_w: u32, target_h: u32) -> Result<Resolution, String> {
        let opts = duplicate_low_res_modes_options();
        let modes = copy_all_display_modes(self.display_id, opts.as_deref())
            .ok_or_else(|| "CGDisplayCopyAllDisplayModes 返回空".to_string())?;

        let mut best: Option<(*const c_void, bool, u32)> = None;
        let count = modes.count();
        for i in 0..count {
            // SAFETY: i < count，modes 在整个函数期间保活。
            let raw_mode = unsafe { modes.value_at_index(i) };
            if raw_mode.is_null() {
                continue;
            }
            let Some((w, h, _pw, _ph, refresh_hz, is_hidpi)) = mode_metrics(raw_mode) else {
                continue;
            };
            if w != target_w || h != target_h {
                continue;
            }
            match best {
                Some((_, best_hidpi, best_hz))
                    if (best_hidpi, best_hz) >= (is_hidpi, refresh_hz) => {}
                _ => best = Some((raw_mode, is_hidpi, refresh_hz)),
            }
        }
        let (target_mode, _, _) =
            best.ok_or_else(|| format!("未找到可用的显示模式 {target_w}x{target_h}"))?;

        let mut config: *mut c_void = std::ptr::null_mut();
        // SAFETY: config 是合法出参。
        let begin_status = unsafe { raw::CGBeginDisplayConfiguration(&mut config) };
        if begin_status != 0 || config.is_null() {
            return Err(format!(
                "CGBeginDisplayConfiguration 失败: CGError {begin_status}"
            ));
        }

        // SAFETY: config 为有效事务句柄；target_mode 在 modes CFArray 存活期内有效。
        let cfg_status = unsafe {
            raw::CGConfigureDisplayWithDisplayMode(
                config,
                self.display_id,
                target_mode,
                std::ptr::null(),
            )
        };
        if cfg_status != 0 {
            // SAFETY: 配置失败必须取消事务释放 CGDisplayConfigRef。
            let _ = unsafe { raw::CGCancelDisplayConfiguration(config) };
            return Err(format!(
                "CGConfigureDisplayWithDisplayMode({target_w}x{target_h}) 失败: CGError {cfg_status}"
            ));
        }

        // SAFETY: 事务已配置，1 = kCGConfigureForSession（仅本会话生效）。
        let complete_status =
            unsafe { raw::CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_FOR_SESSION) };
        if complete_status != 0 {
            return Err(format!(
                "CGCompleteDisplayConfiguration 失败: CGError {complete_status}"
            ));
        }
        self.modified = true;

        // 给 WindowServer 短暂过渡时间刷新显示边界。
        std::thread::sleep(Duration::from_millis(120));
        main_display_logical_resolution()
            .ok_or_else(|| "切换显示模式后无法读取主显示器逻辑分辨率".to_string())
    }

    /// 恢复为捕获时的原始显示模式（幂等）。
    pub(crate) fn restore(&mut self) {
        if !self.modified {
            return;
        }
        let mut config: *mut c_void = std::ptr::null_mut();
        // SAFETY: config 是合法出参。
        let begin_status = unsafe { raw::CGBeginDisplayConfiguration(&mut config) };
        if begin_status == 0 && !config.is_null() {
            // SAFETY: config 与 self.original_mode 均有效。
            let cfg_status = unsafe {
                raw::CGConfigureDisplayWithDisplayMode(
                    config,
                    self.display_id,
                    self.original_mode.as_ptr(),
                    std::ptr::null(),
                )
            };
            if cfg_status == 0 {
                // SAFETY: 事务已配置，1 = kCGConfigureForSession。
                let _ = unsafe {
                    raw::CGCompleteDisplayConfiguration(config, K_CG_CONFIGURE_FOR_SESSION)
                };
            } else {
                // SAFETY: 配置失败取消事务释放句柄。
                let _ = unsafe { raw::CGCancelDisplayConfiguration(config) };
            }
        }
        self.modified = false;
    }
}

impl Drop for DisplayModeGuard {
    fn drop(&mut self) {
        self.restore();
        // SAFETY: original_mode 由 CGDisplayCopyDisplayMode 返回 +1 所有权，此处配对释放。
        unsafe { raw::CFRelease(self.original_mode.as_ptr().cast_const()) };
    }
}

/// 编码一张 CGImageSource 缩略图到 JPEG 字节流。
fn encode_thumbnail_jpeg(
    image_source: *const c_void,
    max_px: u32,
    quality: f64,
) -> Option<Vec<u8>> {
    // SAFETY: 读取 ImageIO 只读 extern static CFString 键常量。
    let (always_key, max_px_key, transform_key, quality_key) = unsafe {
        (
            raw::kCGImageSourceCreateThumbnailFromImageAlways?,
            raw::kCGImageSourceThumbnailMaxPixelSize?,
            raw::kCGImageSourceCreateThumbnailWithTransform?,
            raw::kCGImageDestinationLossyCompressionQuality?,
        )
    };
    let bool_true = CFBoolean::new(true);
    let max_px_num = CFNumber::new_i32(i32::try_from(max_px.max(1)).unwrap_or(i32::MAX));
    let thumb_keys = [
        always_key as *const CFString as *const c_void,
        max_px_key as *const CFString as *const c_void,
        transform_key as *const CFString as *const c_void,
    ];
    let thumb_vals = [
        bool_true as *const CFBoolean as *const c_void,
        &*max_px_num as *const CFNumber as *const c_void,
        bool_true as *const CFBoolean as *const c_void,
    ];
    // SAFETY: thumb_keys/thumb_vals 为 3 个合法 CF 对象指针。
    let thumb_opts = unsafe {
        CFDictionary::new(
            None,
            thumb_keys.as_ptr() as *mut *const c_void,
            thumb_vals.as_ptr() as *mut *const c_void,
            3,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }?;
    // SAFETY: image_source 与 thumb_opts 均合法，返回 +1 CGImageRef 或 null。
    let thumb_img = CfOwned::from_raw(unsafe {
        raw::CGImageSourceCreateThumbnailAtIndex(
            image_source,
            0,
            &*thumb_opts as *const CFDictionary as *const c_void,
        )
    })?;

    // SAFETY: 创建可变 CFData（容量 0 = 按需增长，返回 +1 所有权）。
    let cf_data = CfOwned::from_raw(unsafe { raw::CFDataCreateMutable(std::ptr::null(), 0) })?;
    let jpeg_uti = CFString::from_str("public.jpeg");
    // SAFETY: cf_data 与 jpeg_uti 均有效，count = 1。
    let dest = CfOwned::from_raw(unsafe {
        raw::CGImageDestinationCreateWithData(cf_data.as_ptr(), &*jpeg_uti, 1, std::ptr::null())
    })?;

    let quality_num = CFNumber::new_f64(quality);
    let dest_keys = [quality_key as *const CFString as *const c_void];
    let dest_vals = [&*quality_num as *const CFNumber as *const c_void];
    // SAFETY: dest_keys/dest_vals 为 1 个合法 CF 对象指针。
    let dest_props = unsafe {
        CFDictionary::new(
            None,
            dest_keys.as_ptr() as *mut *const c_void,
            dest_vals.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }?;

    // SAFETY: dest、thumb_img、dest_props 均有效。
    unsafe {
        raw::CGImageDestinationAddImage(
            dest.as_ptr(),
            thumb_img.as_ptr().cast_const(),
            &*dest_props as *const CFDictionary as *const c_void,
        );
        if !raw::CGImageDestinationFinalize(dest.as_ptr()) {
            return None;
        }
    }

    // SAFETY: finalize 成功后从 cf_data 读取字节切片（存活期覆盖本次拷贝）。
    let len = unsafe { raw::CFDataGetLength(cf_data.as_ptr().cast_const()) };
    if len <= 0 {
        return None;
    }
    // SAFETY: len > 0 时 CFDataGetBytePtr 返回至少 len 字节的可读连续内存。
    let ptr = unsafe { raw::CFDataGetBytePtr(cf_data.as_ptr().cast_const()) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: ptr 非空且长度为 len。
    let slice = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    Some(slice.to_vec())
}

/// 读取当前主显示器桌面壁纸并生成低体积 JPEG 缩略图（零 TCC 弹窗）。
pub(crate) fn main_display_wallpaper_jpeg(max_width: u32, max_height: u32) -> Option<Vec<u8>> {
    if max_width == 0 || max_height == 0 {
        return None;
    }
    let image_source = autoreleasepool(|_| -> Option<CfOwned> {
        let ws_cls = AnyClass::get(c"NSWorkspace")?;
        let screen_cls = AnyClass::get(c"NSScreen")?;
        // SAFETY: NSWorkspace.sharedWorkspace 是标准类方法，返回共享实例。
        let ws: Option<Retained<AnyObject>> = unsafe { msg_send![ws_cls, sharedWorkspace] };
        let ws = ws?;

        // SAFETY: NSScreen.mainScreen 是标准类方法，无参可返回 nil。
        let main_screen: Option<Retained<AnyObject>> = unsafe { msg_send![screen_cls, mainScreen] };
        let screen = match main_screen {
            Some(s) => s,
            None => {
                // SAFETY: NSScreen.screens 是标准类方法，返回 NSArray<NSScreen *>。
                let screens: Option<Retained<AnyObject>> =
                    unsafe { msg_send![screen_cls, screens] };
                let screens = screens?;
                // SAFETY: NSArray.firstObject 返回首个元素或 nil。
                let first: Option<Retained<AnyObject>> =
                    unsafe { msg_send![&*screens, firstObject] };
                first?
            }
        };

        // SAFETY: desktopImageURLForScreen: 接收有效 NSScreen*，返回 NSURL*（toll-free CFURLRef）或 nil。
        let url: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*ws, desktopImageURLForScreen: &*screen] };
        let url = url?;

        // SAFETY: url 为 toll-free bridged CFURLRef，返回 +1 CGImageSourceRef 或 null。
        CfOwned::from_raw(unsafe {
            raw::CGImageSourceCreateWithURL(
                (&*url as *const AnyObject).cast::<c_void>(),
                std::ptr::null(),
            )
        })
    })?;

    let initial_max_px = max_width.max(max_height).max(1);
    let mut jpeg = encode_thumbnail_jpeg(image_source.as_ptr().cast_const(), initial_max_px, 0.72)?;
    if jpeg.len() > 48 * 1024 {
        let retry_max_px = initial_max_px.min(480);
        jpeg = encode_thumbnail_jpeg(image_source.as_ptr().cast_const(), retry_max_px, 0.50)?;
    }
    if jpeg.len() > 60 * 1024 || jpeg.is_empty() {
        return None;
    }
    Some(jpeg)
}

// ─────────────────────────────────────────────────────────────────────────────
// App Nap 抑制与实时线程 QoS 提升（维持 120fps 不被后台节流）
// ─────────────────────────────────────────────────────────────────────────────

const NS_ACTIVITY_IDLE_DISPLAY_SLEEP_DISABLED: u64 = 1 << 40;
const NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED: u64 = 1 << 20;
const NS_ACTIVITY_USER_INITIATED: u64 = 0x00FF_FFFF | NS_ACTIVITY_IDLE_SYSTEM_SLEEP_DISABLED;
const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0x00FF_0000_0000;
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

/// RAII 声明高优先级实时会话活动，防止 macOS App Nap 在长时 120fps 运行期间
/// 降低定时器与采集/编解码线程调度优先级。
pub(crate) struct LatencyCriticalActivity {
    retained: Option<(Retained<AnyObject>, Retained<AnyObject>)>,
}

// SAFETY: `NSProcessInfo` 是线程安全的，`beginActivityWithOptions:reason:` 返回的
// activity token 仅用于最终配对调用 `endActivity:`，可在创建或工作线程安全持有并 Drop。
unsafe impl Send for LatencyCriticalActivity {}
// SAFETY: 仅持有不可变 Foundation 对象句柄且无内部可变共享状态，Drop 需要独占 `&mut self`。
unsafe impl Sync for LatencyCriticalActivity {}

impl LatencyCriticalActivity {
    pub(crate) fn begin(reason: &str, keep_display_awake: bool) -> Self {
        let mut options = NS_ACTIVITY_USER_INITIATED | NS_ACTIVITY_LATENCY_CRITICAL;
        if keep_display_awake {
            options |= NS_ACTIVITY_IDLE_DISPLAY_SLEEP_DISABLED;
        }
        let sanitized = reason.replace('\0', " ");
        let Ok(c_reason) = std::ffi::CString::new(sanitized) else {
            return Self { retained: None };
        };
        let retained = autoreleasepool(|_| -> Option<(Retained<AnyObject>, Retained<AnyObject>)> {
            let pi_cls = AnyClass::get(c"NSProcessInfo")?;
            let ns_cls = AnyClass::get(c"NSString")?;
            // SAFETY: `NSProcessInfo.processInfo` 返回进程级单例。
            let info: Option<Retained<AnyObject>> = unsafe { msg_send![pi_cls, processInfo] };
            let info = info?;
            // SAFETY: `c_reason` 为合法 NUL 结尾 UTF-8 字符串，返回自动释放 NSString。
            let ns_reason: Option<Retained<AnyObject>> =
                unsafe { msg_send![ns_cls, stringWithUTF8String: c_reason.as_ptr()] };
            let ns_reason = ns_reason?;
            // SAFETY: `options` 为 `NSActivityOptions` (u64)，`ns_reason` 为非空 NSString*。
            let token: Option<Retained<AnyObject>> = unsafe {
                msg_send![&*info, beginActivityWithOptions: options, reason: &*ns_reason]
            };
            let token = token?;
            Some((info, token))
        });
        Self { retained }
    }
}

impl Drop for LatencyCriticalActivity {
    fn drop(&mut self) {
        if let Some((info, token)) = self.retained.take() {
            autoreleasepool(|_| {
                // SAFETY: `info` 与 `token` 来自成对的 `beginActivityWithOptions:reason:`。
                unsafe {
                    let _: () = msg_send![&*info, endActivity: &*token];
                }
            });
        }
    }
}

/// 将当前操作系统线程提升至 `QOS_CLASS_USER_INTERACTIVE`（`0x21`）。
pub(crate) fn promote_current_thread_qos() {
    // SAFETY: `pthread_set_qos_class_self_np` 对任意用户态 pthread 均为安全系统调用，
    // 传入合法 `QOS_CLASS_USER_INTERACTIVE`（0x21）与相对优先级 0。
    let _ = unsafe { raw::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
}

const RTLD_LAZY: std::ffi::c_int = 0x1;
const LOGIN_FRAMEWORK_PATH: &std::ffi::CStr =
    c"/System/Library/PrivateFrameworks/login.framework/Versions/Current/login";

/// 检查系统 `login.framework` 是否导出 `SACLockScreenImmediate`（仅解析符号，不锁屏）。
#[cfg(test)]
pub(crate) fn has_lock_screen_symbol() -> bool {
    // SAFETY: `LOGIN_FRAMEWORK_PATH` 与 `c"SACLockScreenImmediate"` 为静态 NUL 结尾 C 字符串；
    // 若 `dlopen` 返回非空句柄则配对调用 `dlclose`，绝不解引用或调用目标函数指针。
    unsafe {
        let handle = raw::dlopen(LOGIN_FRAMEWORK_PATH.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            return false;
        }
        let sym = raw::dlsym(handle, c"SACLockScreenImmediate".as_ptr());
        let ok = !sym.is_null();
        let _ = raw::dlclose(handle);
        ok
    }
}

/// 立即锁定当前 macOS 登录会话屏幕（优先调用 `SACLockScreenImmediate`，失败回退 `pmset displaysleepnow`）。
pub(crate) fn lock_screen_now() -> bool {
    // SAFETY: `dlopen`/`dlsym` 解析 Apple `login.framework` 的零参无返回值函数
    // `SACLockScreenImmediate`（`extern "C" fn()`），调用后配对 `dlclose` 释放句柄。
    let locked = unsafe {
        let handle = raw::dlopen(LOGIN_FRAMEWORK_PATH.as_ptr(), RTLD_LAZY);
        if handle.is_null() {
            false
        } else {
            let sym = raw::dlsym(handle, c"SACLockScreenImmediate".as_ptr());
            let invoked = if !sym.is_null() {
                let lock_fn: extern "C" fn() = std::mem::transmute(sym);
                lock_fn();
                true
            } else {
                false
            };
            let _ = raw::dlclose(handle);
            invoked
        }
    };
    if locked {
        return true;
    }
    std::process::Command::new("/usr/bin/pmset")
        .arg("displaysleepnow")
        .status()
        .is_ok_and(|s| s.success())
}

// ─────────────────────────────────────────────────────────────────────────────
// 最新帧优先单槽邮箱、事件与像素缓冲句柄
// ─────────────────────────────────────────────────────────────────────────────

/// 单帧视频载荷。
///
/// 泛型参数 `B` 默认为 [`PixelBufferHandle`]，仅在纯 Rust 单测中替换为轻量桩类型，
/// 用来验证「丢旧保新」与旧帧立刻 Drop 归还缓冲池的语义。
pub(crate) struct FramePayload<B = PixelBufferHandle> {
    /// 已 +1 retain 的像素缓冲，Drop 时立即归还 SCK IOSurface 池。
    pub(crate) buffer: B,
    /// CMSampleBuffer 的 presentation timestamp，微秒（契约 C2 的原始来源）。
    pub(crate) pts_us: u64,
}

/// 控制事件（与最新视频帧分槽保存，绝不被新视频帧覆写）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlEvent {
    /// SCFrameStatus == Suspended（锁屏 / 会话挂起）。
    Suspended,
    /// `stream:didStopWithError:`（权限被撤销等，M10）。绝不 panic，只上报。
    Stopped {
        /// NSError 的可读描述。
        message: String,
    },
}

/// 帧回调 → `next_frame` 的事件。
pub(crate) enum FrameEvent<B = PixelBufferHandle> {
    /// 一帧最新内容：retain 过的 CVPixelBuffer + 内容变化时刻（PTS，微秒）。
    Frame(FramePayload<B>),
    /// SCFrameStatus == Suspended（锁屏 / 会话挂起）。
    Suspended,
    /// `stream:didStopWithError:`（权限被撤销等，M10）。绝不 panic，只上报。
    Stopped {
        /// NSError 的可读描述。
        message: String,
    },
}

/// [`FrameMailbox`] 的内部双槽状态：控制事件槽与最新视频帧槽完全分离。
struct MailboxInner<B> {
    control: Option<ControlEvent>,
    latest_frame: Option<FramePayload<B>>,
}

/// 线程安全的「最新帧优先单槽邮箱」。
///
/// 替代 FIFO `sync_channel`：
/// - 消费端短暂滞后时，新帧直接覆写 `latest_frame` 并在锁外立刻 Drop 被覆盖的旧
///   `PixelBufferHandle`（即刻 `CFRelease` 归还 SCK IOSurface 池），避免旧帧排队
///   导致的采集交接延迟上升；
/// - 控制事件（`Suspended` / `Stopped`）存入独立控制槽，优先级高于视频帧，绝不被
///   新到达的视频帧覆写；
/// - 双槽均空时消费端通过 `Condvar::wait_timeout` 零自旋等待。
pub(crate) struct FrameMailbox<B = PixelBufferHandle>(Arc<(Mutex<MailboxInner<B>>, Condvar)>);

impl<B> Clone for FrameMailbox<B> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<B> FrameMailbox<B> {
    /// 创建空邮箱。
    pub(crate) fn new() -> Self {
        Self(Arc::new((
            Mutex::new(MailboxInner {
                control: None,
                latest_frame: None,
            }),
            Condvar::new(),
        )))
    }

    /// 投递最新视频帧：直接覆写 `latest_frame` 并立即 Drop 旧帧，随后唤醒等待者。
    pub(crate) fn push_frame(&self, frame: FramePayload<B>) {
        let (lock, cvar) = &*self.0;
        let displaced = {
            let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
            let old = inner.latest_frame.replace(frame);
            cvar.notify_one();
            old
        };
        // 在锁外立刻释放被覆盖的旧帧，第一时间归还 SCK IOSurface 池。
        drop(displaced);
    }

    /// 投递控制事件（`Suspended` / `Stopped`）：写入独立控制槽，绝不被新视频帧覆写，并唤醒等待者。
    ///
    /// 若控制槽已有更致命的 `Stopped`，不会被后续 `Suspended` 降级。
    pub(crate) fn push_control(&self, event: ControlEvent) {
        let (lock, cvar) = &*self.0;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        let should_write = !matches!(
            (&inner.control, &event),
            (Some(ControlEvent::Stopped { .. }), ControlEvent::Suspended)
        );
        if should_write {
            inner.control = Some(event);
        }
        cvar.notify_one();
    }

    /// 取出下一条就绪事件：有控制事件优先返回控制事件，否则 `take()` 最新视频帧。
    fn take_ready(inner: &mut MailboxInner<B>) -> Option<FrameEvent<B>> {
        if let Some(ctrl) = inner.control.take() {
            return Some(match ctrl {
                ControlEvent::Suspended => FrameEvent::Suspended,
                ControlEvent::Stopped { message } => FrameEvent::Stopped { message },
            });
        }
        inner.latest_frame.take().map(FrameEvent::Frame)
    }

    /// 等待下一条事件（控制事件优先，否则取最新视频帧）；超时返回 `None`。
    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Option<FrameEvent<B>> {
        let (lock, cvar) = &*self.0;
        let mut inner = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(event) = Self::take_ready(&mut inner) {
            return Some(event);
        }
        if timeout.is_zero() {
            return None;
        }
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let (guard, timed_out) = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        return None;
                    }
                    let (g, res) = cvar
                        .wait_timeout(inner, dl - now)
                        .unwrap_or_else(|e| e.into_inner());
                    (g, res.timed_out())
                }
                None => (cvar.wait(inner).unwrap_or_else(|e| e.into_inner()), false),
            };
            inner = guard;
            if let Some(event) = Self::take_ready(&mut inner) {
                return Some(event);
            }
            if timed_out {
                return None;
            }
        }
    }
}

/// 拥有型 `CVPixelBuffer` 引用（CFRetained，+1）。
///
/// 这是 [`rc_capture::FrameTexture::CVPixelBuffer`] 背后的真身：
/// 抽象层拿到的 `*mut c_void` 指向这里持有的对象。
pub(crate) struct PixelBufferHandle(CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer 是 CoreVideo 的线程安全对象（文档只要求访问 base
// address 时加锁，我们从不锁着读 CPU 数据——零拷贝路径不碰像素）。
// 这里持有的是 +1 retain，跨线程移动、在任意线程 Drop 释放都是安全的。
unsafe impl Send for PixelBufferHandle {}

// SAFETY: 本包装仅在 Drop 时调用原子引用计数 CFRelease，且不提供可变内部状态，
// 多线程通过 Arc 共享保活守卫并在最后一个引用 Drop 时释放完全线程安全。
unsafe impl Sync for PixelBufferHandle {}

impl PixelBufferHandle {
    /// 暴露给抽象层的**不透明**句柄（不转移所有权；存活期由本 wrapper 决定）。
    pub(crate) fn as_opaque_ptr(&self) -> *mut c_void {
        CFRetained::as_ptr(&self.0).as_ptr().cast()
    }

    /// `(width, height, bytes_per_row, pixel_format)`。
    ///
    /// 供首帧校验（格式必须是 '420v'）与真机测试核对逻辑分辨率（M9）。
    pub(crate) fn info(&self) -> (u32, u32, u32, u32) {
        // 这些是 objc2-core-video 生成的安全包装（内部已完成 unsafe 调用），
        // 传入的引用由 CFRetained 保证有效存活。
        (
            CVPixelBufferGetWidth(&self.0) as u32,
            CVPixelBufferGetHeight(&self.0) as u32,
            CVPixelBufferGetBytesPerRow(&self.0) as u32,
            CVPixelBufferGetPixelFormatType(&self.0),
        )
    }
}

/// SCK 内部 IOSurface 缓冲池深度（`SCStreamConfiguration::queueDepth`，Apple 文档上限为 8）。
///
/// 注意：它不是会增加排队延迟的发送缓冲（实际排队深度由最新帧优先单槽邮箱
/// [`FrameMailbox`]（深度 1，丢旧保新）与 `HostPipeline` 的 `cap_tx(1)` +
/// `max_in_flight(1)` 严格封顶），而是 SCK 分配 `CVPixelBuffer` 的底层对象池容量。
/// 必须严格大于「调用方保活环 + 邮箱单槽」之和（当前 `3 + 1 = 4 < 8`，至少留 4 块
/// 空闲表面供 SCK 持续产出），否则池耗尽会卡死出帧。
pub(crate) const SCK_SURFACE_POOL_DEPTH: isize = 8;

/// `'420v'` 四字符码（`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`，
/// 即 NV12 video range）。M5：SCK 配置与首帧校验共用的期望值。
pub(crate) const PIXEL_FORMAT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

// ─────────────────────────────────────────────────────────────────────────────
// CMSampleBuffer 提取（帧回调里的轻量路径）
// ─────────────────────────────────────────────────────────────────────────────

/// 读取 SCK 附加在 sample buffer 上的帧状态（`SCStreamFrameInfoStatus`）。
///
/// 在 ScreenCaptureKit 中，`SCStreamFrameInfoStatus` 存放在
/// `CMSampleBufferGetSampleAttachmentsArray(sb, false)` 返回数组的第 0 个
/// `CFDictionary` 里，而不是 sample buffer 顶层附件。
fn frame_status(sb: &CMSampleBuffer) -> Option<SCFrameStatus> {
    // SAFETY: sb 是有效的 CMSampleBuffer；create_if_necessary = false 只查询
    // 已有附件数组而不修改 sample buffer，返回值已被绑定层 retain。
    let attachments = unsafe { sb.sample_attachments_array(false) }?;
    if attachments.count() <= 0 {
        return None;
    }
    // SAFETY: 已确认 count > 0，索引 0 合法；取出的指针在 attachments 存活期内有效。
    let raw_dict = unsafe { attachments.value_at_index(0) };
    let dict_ptr = NonNull::new(raw_dict.cast_mut())?;
    // SAFETY: sample attachments array 的非空元素为 CFType 对象，存活期受
    // attachments 保护；随后 downcast_ref 通过 CFGetTypeID 在运行时校验其为 CFDictionary。
    let dict: &CFDictionary = unsafe { dict_ptr.cast::<CFType>().as_ref() }.downcast_ref()?;

    // SAFETY: SCStreamFrameInfoStatus 是 ScreenCaptureKit 导出的 &'static NSString，
    // NSString 与 CFString toll-free bridged，指针可直接作为 CFDictionary 的键。
    let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoStatus) }.cast();
    // SAFETY: dict 是有效的 CFDictionary，key 为有效的 CFString/NSString 键；
    // 返回值由 dict 持有，在 attachments 存活期内有效。
    let raw_value = unsafe { dict.value(key) };
    let value_ptr = NonNull::new(raw_value.cast_mut())?;
    // SAFETY: 字典内非空附件值是 CFType 对象，存活期受 dict 保护；
    // 随后 downcast_ref 通过 CFGetTypeID 在运行时校验其为 CFNumber。
    let number: &CFNumber = unsafe { value_ptr.cast::<CFType>().as_ref() }.downcast_ref()?;

    // SCFrameStatus 的底层就是 NSInteger 枚举值（Complete=0, Idle=1, Blank=2,
    // Suspended=3, Started=4, Stopped=5）。
    let status = number.as_isize()?;
    Some(SCFrameStatus(status))
}

/// `CMTime` 换算成微秒。无效时间（flags 无 Valid 或 timescale 为 0）返回 None。
fn cmtime_to_us(t: CMTime) -> Option<u64> {
    if !t.flags.contains(CMTimeFlags::Valid) || t.timescale == 0 {
        return None;
    }
    // i128 中间量避免 value * 1e6 溢出（mach 时间戳的 value 可以很大）。
    let us = (t.value as i128) * 1_000_000 / (t.timescale as i128);
    u64::try_from(us).ok()
}

/// PTS 换算成微秒。无效时间（flags 无 Valid 或 timescale 为 0）返回 None。
fn pts_us(sb: &CMSampleBuffer) -> Option<u64> {
    // SAFETY: 纯 getter，无副作用。
    cmtime_to_us(unsafe { sb.presentation_time_stamp() })
}

/// 读取 `CMClockGetHostTimeClock()` 当前时刻（微秒），与 ScreenCaptureKit
/// `CMSampleBuffer` 的 PTS 处于**完全相同的单调时钟域**。
pub(crate) fn host_time_us() -> u64 {
    // SAFETY: CMClockGetHostTimeClock 返回系统全局单调主机时钟；time() 为纯只读查询。
    let t = unsafe {
        let clock = CMClock::host_time_clock();
        clock.time()
    };
    cmtime_to_us(t).unwrap_or(0)
}

/// 计算传给 `SCStreamConfiguration::setMinimumFrameInterval` 的帧间隔（微秒）。
///
/// 在 macOS ProMotion（120Hz，8.33ms 节拍）与 60Hz 显示器上，若严格传 `1/fps`
/// （如 60fps = 16_667µs），轻微 vsync 抖动就会使 SCK 判定间隔不足而跳过当前刷新
/// 并顺延到下一节拍（25ms = 40fps），实测帧率跌至 ~47.5fps。
/// 取标称间隔的 75%（`750_000 / fps`）：
/// - 60fps → `12_500µs`（严格落在 120Hz 的 8.33ms 与 60Hz 的 16.67ms 中点，既不放行
///   120Hz 倍频帧，又为 60Hz 刷新留足 4.17ms 抖动容差）；
/// - 120fps → `6_250µs`；30fps → `25_000µs`。
pub(crate) fn minimum_frame_interval_us(fps: u32) -> i64 {
    let fps = i64::from(fps.max(1));
    (750_000 / fps).max(1)
}

/// 取出 sample buffer 里的像素缓冲（retain +1，交给 [`PixelBufferHandle`]）。
fn image_buffer(sb: &CMSampleBuffer) -> Option<PixelBufferHandle> {
    // SAFETY: 纯 getter；返回的 CVImageBuffer 已被绑定层 retain。
    let buffer = unsafe { sb.image_buffer() }?;
    Some(PixelBufferHandle(buffer))
}

/// NSError → 可读字符串。
fn nserror_message(err: &NSError) -> String {
    // localizedDescription 在绑定层是安全方法（纯属性 getter）。
    // NSString 实现了 Display，to_string() 是安全方法。
    err.localizedDescription().to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// 帧回调类：实现 SCStreamOutput（帧）+ SCStreamDelegate（M10 错误停止）
// ─────────────────────────────────────────────────────────────────────────────

// 回调侧不做 PTS 节拍过滤。
//
// 曾按「距上一放行帧 ≥ 标称间隔 87.5%」过滤 ProMotion 中间拍，真机实测
// 反而把采集打到 48fps：SCK 在 60fps 内容下的实际到达节拍是 8.33/16.67ms
// **交替**，两个 8.33ms 连拍后再来一帧时，距上一放行帧已累计 25ms 的
// 「假间隔」——过滤器把它当 60Hz 双拍放行，节拍被拉长到 25ms 周期。
// 多余供给（~70fps）交给调度器的预算门（R1 丢到达帧）在真机全链里
// 实测更优：0 预算丢帧时 displayed 72fps、P50 14.3ms，双达标。
// （过滤函数 `should_emit_pts` / `callback_min_spacing_us` 已删除；
// 如需重加，必须先修交替拍累计间隔的判定基准。）

struct HandlerIvars {
    mailbox: FrameMailbox,
}

define_class!(
    // SAFETY:
    // - 超类 NSObject 没有子类化要求。
    // - 本类不实现 Drop。
    // - ivars（FrameMailbox）只在构造线程写入，在 SCStream 的串行 sample handler
    //   queue 与 delegate queue 上通过内部 Mutex + Condvar 并发访问；
    //   FrameMailbox 本身是 Send + Sync。
    #[unsafe(super(NSObject))]
    #[name = "RcSckStreamHandler"]
    #[ivars = HandlerIvars]
    struct RcSckStreamHandler;

    unsafe impl NSObjectProtocol for RcSckStreamHandler {}

    unsafe impl SCStreamOutput for RcSckStreamHandler {
        // 方法名必须与协议声明的选择器部分一致（non_snake_case 是刻意的）。
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            // 坑 6：本回调运行在 SCStream 的独立串行 dispatch queue 上。
            // 只做指针/整数级提取并投递单槽邮箱，绝不做重活。
            if r#type != SCStreamOutputType::Screen {
                return;
            }
            let Some(status) = frame_status(sample_buffer) else {
                return;
            };
            match status {
                SCFrameStatus::Complete => {}
                SCFrameStatus::Suspended => {
                    // 锁屏/挂起：上报 SessionLocked 语义，入独立控制槽，由上层决定重建。
                    self.ivars().mailbox.push_control(ControlEvent::Suspended);
                    return;
                }
                // Idle/Blank/Started/Stopped：没有新内容 —— 不产帧（契约 C1，
                // 静止画面下采集侧 CPU 占用必须接近 0）。
                _ => return,
            }
            let pts = pts_us(sample_buffer).unwrap_or(0);
            let Some(buffer) = image_buffer(sample_buffer) else {
                return;
            };
            // 最新帧优先：直接覆写单槽，被覆盖的旧 CVPixelBuffer 立刻 Drop 归还 SCK 池，
            // 绝不阻塞回调队列。
            self.ivars().mailbox.push_frame(FramePayload {
                buffer,
                pts_us: pts,
            });
        }
    }

    unsafe impl SCStreamDelegate for RcSckStreamHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, error: &NSError) {
            // M10：权限被撤销等致命停止在这里上报（AccessLost 语义），
            // 绝不 panic —— 回调线程 panic 会直接杀掉进程。
            let message = nserror_message(error);
            self.ivars()
                .mailbox
                .push_control(ControlEvent::Stopped { message });
        }
    }
);

// 构造方法放在 define_class! 之外：纯 Rust 辅助函数，不暴露给 ObjC。
impl RcSckStreamHandler {
    fn new(mailbox: FrameMailbox) -> Retained<Self> {
        let this = Self::alloc().set_ivars(HandlerIvars { mailbox });
        // SAFETY: this 是 alloc 出来的本类实例，调用超类 init 完成 NSObject
        // 初始化并移交所有权（返回 +1 的 Retained）。
        unsafe { msg_send![super(this), init] }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 显示器枚举
// ─────────────────────────────────────────────────────────────────────────────

/// 一个可采集显示器的快照（plain 数据 + 建 filter 用的 SCDisplay 强引用）。
///
/// 只在建流调用栈上存活，不进入 [`SckBackend`](crate::sck::SckBackend)，
/// 因此无需 Send。
pub(crate) struct DisplaySnapshot {
    /// CGDirectDisplayID。
    pub display_id: u32,
    /// 物理分辨率（像素）。
    pub backing: Resolution,
    /// 物理分辨率 / 逻辑分辨率（Retina 为 2.0）。
    pub scale: f32,
    /// 建 SCContentFilter 用的强引用。
    display: Retained<SCDisplay>,
}

/// 枚举所有可采集显示器（异步获取 `SCShareableContent`，坑 5）。
pub(crate) fn enumerate_displays() -> Result<Vec<DisplaySnapshot>, String> {
    let content = fetch_shareable_content(Duration::from_secs(3))?;
    // SAFETY: displays 是纯属性 getter。
    let displays = unsafe { content.displays() };
    // to_vec 是 objc2-foundation 的安全方法（逐个 retain 后返回 Vec）。
    Ok(displays
        .to_vec()
        .into_iter()
        .map(|display| {
            // SAFETY: width/height/frame/displayID 均为纯 getter。
            unsafe {
                let backing_w = display.width() as u32;
                let backing_h = display.height() as u32;
                let frame = display.frame();
                // 逻辑宽度只用来推 scale（Retina 为 2.0）；请求的输出分辨率
                // 由 sck.rs 按 backing/scale 下采样计算（M9）。
                let logical_w = frame.size.width.round().max(1.0) as u32;
                let scale = if logical_w > 0 {
                    backing_w as f32 / logical_w as f32
                } else {
                    1.0
                };
                DisplaySnapshot {
                    display_id: display.displayID(),
                    backing: Resolution::new(backing_w, backing_h),
                    scale,
                    display,
                }
            }
        })
        .collect())
}

/// 异步获取 `SCShareableContent`，转成同步等待（带超时）。
fn fetch_shareable_content(timeout: Duration) -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: 完成回调保证 error 在回调期间有效；只做同步读取。
                Err(nserror_message(unsafe { &*error }))
            } else {
                match NonNull::new(content) {
                    Some(ptr) => {
                        // SAFETY: 回调期间指针有效；retain 把 +0 借用升级为 +1
                        // 强引用（无论调用方是否移交所有权，这都是正确语义）。
                        // 返回 None 仅当指针为 null（上面已排除）。
                        unsafe { Retained::retain(ptr.as_ptr()) }
                            .ok_or_else(|| "retain SCShareableContent 失败".to_string())
                    }
                    None => Err("SCShareableContent 为空".to_string()),
                }
            };
            // 捕获的 Sender 是 Send + Sync：block 被 ObjC 拷贝后在任意系统队列
            // 上恰好调用一次，线程上是安全的。
            let _ = tx.send(result);
        },
    );
    // SAFETY: block 签名与 completionHandler:（content, error）匹配。
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &block,
        );
    }
    rx.recv_timeout(timeout)
        .map_err(|_| "获取 SCShareableContent 超时".to_string())?
}

// ─────────────────────────────────────────────────────────────────────────────
// 建流：filter + configuration + stream + start
// ─────────────────────────────────────────────────────────────────────────────

/// 拥有 SCStream 全套对象（stream、handler、queue）。
///
/// 字段顺序即 Drop 顺序：stream 先释放（释放过程中回调仍可能进来，此时
/// handler 还活着），handler 与 queue 随后释放。
pub(crate) struct StreamHandles {
    stream: Retained<SCStream>,
    /// SCStream 的 delegate 是 unretained 的 —— 这个强引用让它活到 stream 之后。
    _handler: Retained<RcSckStreamHandler>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

// SAFETY: 这组对象随 SckBackend 一起被移动（Send = 跨线程移交所有权）。
// 访问始终发生在持有 &mut self 的单一线程上（CaptureBackend: Send 的使用
// 约定），不存在并发访问；释放（Drop）发生在最终 owning 线程，ObjC 的
// release 是线程安全的。
unsafe impl Send for StreamHandles {}

impl Drop for StreamHandles {
    fn drop(&mut self) {
        // M8：SCStream 停止后不可复用 —— 丢弃前显式 stop（异步，不等待），
        // 随后字段按声明顺序释放 stream 本体。
        // SAFETY: completion 传 None 合法（只是不要回调）。
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

/// 组装 `SCStreamConfiguration`（M5/M7/M9 全部在这里落实）。
fn make_configuration(target: Resolution, fps: u32) -> Retained<SCStreamConfiguration> {
    // SAFETY: new 是无参构造。
    let config = unsafe { SCStreamConfiguration::new() };
    // M5：'420v'（NV12 video range）—— VideoToolbox 的原生输入，零拷贝直达
    // 编码器，无需 CPU 色彩转换。
    // SAFETY: 纯属性 setter。
    unsafe { config.setPixelFormat(PIXEL_FORMAT_420V) };
    // M7：光标不进视频帧（远端由输入层单独绘制光标，避免双光标/滞后）。
    // SAFETY: 纯属性 setter。
    unsafe { config.setShowsCursor(false) };
    // 显式关闭音频采集，避免 SCK 维护额外音频管线与回调开销。
    // SAFETY: 纯属性 setter。
    unsafe { config.setCapturesAudio(false) };
    // M9：输出逻辑分辨率 —— Retina 2x 屏上等价于把 2x 帧下采样到 1x，
    // 不把 2x 分辨率喂给编码器（带宽翻倍）；显式启用 scalesToFit 确保
    // 目标分辨率小于物理分辨率时由 GPU 等比缩放填满画幅而非左上角 1:1 裁切。
    // SAFETY: 纯属性 setter。
    unsafe {
        config.setWidth(target.width as usize);
        config.setHeight(target.height as usize);
        config.setScalesToFit(true);
    }
    // minimumFrameInterval 带 10% vsync 抖动容差（见 minimum_frame_interval_us），
    // 消除 ProMotion 120Hz 节拍拍频掉帧；静止画面下回调仍不触发（契约 C1）。
    // SAFETY: 纯属性 setter。
    unsafe {
        config.setMinimumFrameInterval(CMTime {
            epoch: 0,
            flags: CMTimeFlags::Valid,
            value: minimum_frame_interval_us(fps),
            timescale: 1_000_000,
        });
    }
    // SCK 底层 IOSurface 池容量（见 SCK_SURFACE_POOL_DEPTH 注释，必须严格大于
    // 消费侧保活环 + 回调通道之和，避免池耗尽卡死出帧）。
    // SAFETY: 纯属性 setter。
    unsafe { config.setQueueDepth(SCK_SURFACE_POOL_DEPTH) };
    config
}

/// 组装 `SCContentFilter`（捕获整块显示器，含桌面与 Dock，坑 10：每显示器一个 filter）。
fn make_filter(display: &DisplaySnapshot) -> Retained<SCContentFilter> {
    // NSArray::new 产生空数组（合法的 excluded：什么都不排除）；绑定层安全方法。
    let empty: Retained<NSArray<SCWindow>> = NSArray::new();
    // SAFETY: alloc + initWithDisplay:excludingWindows: 标准构造；
    // display 强引用有效，excluded 为空数组。
    unsafe {
        let allocated = SCContentFilter::alloc();
        SCContentFilter::initWithDisplay_excludingWindows(allocated, &display.display, &empty)
    }
}

/// 构建、启动一条全新的 SCStream，返回句柄与最新帧优先单槽邮箱。
///
/// 调用方（`sck.rs`）保证 `display` 是本次调用栈上刚枚举出来的快照。
pub(crate) fn build_stream(
    display: &DisplaySnapshot,
    target: Resolution,
    fps: u32,
) -> Result<(StreamHandles, FrameMailbox), String> {
    let filter = make_filter(display);
    let config = make_configuration(target, fps);
    let mailbox = FrameMailbox::new();
    let handler = RcSckStreamHandler::new(mailbox.clone());

    // SAFETY: initWithFilter:configuration:delegate: 标准构造。delegate 参数是
    // unretained 的 —— StreamHandles 持有 handler 强引用且比 stream 活得久
    // （见字段顺序注释），生命周期安全。
    let stream = unsafe {
        let allocated = SCStream::alloc();
        SCStream::initWithFilter_configuration_delegate(
            allocated,
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*handler)),
        )
    };

    // 坑 6：帧回调走独立串行高优先级（UserInteractive）queue，不占用主队列 / 全局队列。
    let attr = dispatch2::DispatchQueueAttr::with_qos_class(
        dispatch2::DispatchQueueAttr::SERIAL,
        dispatch2::DispatchQoS::UserInteractive,
        0,
    );
    let queue = dispatch2::DispatchQueue::new("rc.capture.sck.frames", Some(&attr));
    // SAFETY: handler 实现了 SCStreamOutput；sample handler queue 为我们创建的
    // 串行队列，回调串行到达，满足协议的线程要求。
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*handler),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| nserror_message(&e))?;
    }

    // 启动并等待完成回调（拿 NSError 描述，例如权限被拒）。
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let start_block = block2::RcBlock::new(move |error: *mut NSError| {
        let message = if error.is_null() {
            None
        } else {
            // SAFETY: 完成回调保证 error 在回调期间有效；只做同步读取。
            Some(nserror_message(unsafe { &*error }))
        };
        // 捕获的 Sender 是 Send + Sync，回调恰好在某个系统队列上调用一次。
        let _ = start_tx.send(message);
    });
    // SAFETY: block 签名与 startCaptureWithCompletionHandler:（*mut NSError）匹配。
    unsafe { stream.startCaptureWithCompletionHandler(Some(&start_block)) };
    match start_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(None) => {}
        Ok(Some(message)) => {
            return Err(format!("startCapture 失败: {message}"));
        }
        Err(_) => return Err("等待 startCapture 完成回调超时".to_string()),
    }

    Ok((
        StreamHandles {
            stream,
            _handler: handler,
            _queue: queue,
        },
        mailbox,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_foundation::CFMutableDictionary;

    fn make_empty_sample_buffer() -> CFRetained<CMSampleBuffer> {
        let mut raw_sb: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: sample_buffer_out 指向有效的栈变量；创建 1 个无负载的
        // sample buffer 仅用于测试 sample attachments array 的读写。
        let status = unsafe {
            CMSampleBuffer::create(
                None,
                None,
                true,
                None,
                std::ptr::null_mut(),
                None,
                1,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                NonNull::from(&mut raw_sb),
            )
        };
        assert_eq!(status, 0, "CMSampleBufferCreate 失败: OSStatus={status}");
        let ptr = NonNull::new(raw_sb).expect("CMSampleBufferCreate 返回空指针");
        // SAFETY: CMSampleBufferCreate 遵循 Create Rule，返回 +1 所有权。
        unsafe { CFRetained::from_raw(ptr) }
    }

    #[test]
    fn frame_status_reads_sample_attachments_array() {
        let sb = make_empty_sample_buffer();
        // 1. 尚未创建 sample attachments 时返回 None。
        assert_eq!(frame_status(&sb), None);

        // 2. 创建空的 sample attachments 字典后仍应返回 None。
        // SAFETY: sb 有效，create_if_necessary = true 创建并保留附件数组。
        let attachments = unsafe { sb.sample_attachments_array(true) }
            .expect("create_if_necessary=true 必须返回 CFArray");
        assert_eq!(attachments.count(), 1);
        assert_eq!(frame_status(&sb), None);

        // SAFETY: count == 1，索引 0 为 CMSampleBuffer 提供的可变 CFDictionary。
        let raw_dict = unsafe { attachments.value_at_index(0) };
        let dict_ptr = NonNull::new(raw_dict.cast_mut()).expect("附件字典非空");
        // SAFETY: CMSampleBufferGetSampleAttachmentsArray 的元素保证是 CFMutableDictionary，
        // 存活期受 attachments 保护。
        let dict: &CFMutableDictionary = unsafe { dict_ptr.cast::<CFMutableDictionary>().as_ref() };
        // SAFETY: SCStreamFrameInfoStatus 是系统框架导出的 &'static NSString，
        // 与 CFString toll-free bridged。
        let key: *const c_void = unsafe { std::ptr::addr_of!(*SCStreamFrameInfoStatus) }.cast();

        // 3. 写入各类 SCFrameStatus 枚举值，frame_status 必须准确还原。
        for expected in [
            SCFrameStatus::Complete,
            SCFrameStatus::Idle,
            SCFrameStatus::Blank,
            SCFrameStatus::Suspended,
            SCFrameStatus::Started,
            SCFrameStatus::Stopped,
        ] {
            let num = CFNumber::new_isize(expected.0);
            let val_ptr: *const c_void = CFRetained::as_ptr(&num).as_ptr().cast();
            // SAFETY: dict 是可变附件字典，key/val_ptr 均为有效 CFType 指针，
            // CFDictionarySetValue 会 retain 它们。
            unsafe { CFMutableDictionary::set_value(Some(dict), key, val_ptr) };
            assert_eq!(
                frame_status(&sb),
                Some(expected),
                "应解析出 SCFrameStatus({})",
                expected.0
            );
        }

        // 4. 值为非 CFNumber（例如 CFString）时，运行时 CFGetTypeID 校验应返回 None。
        // SAFETY: key 本身是有效的 CFString 指针，写入后验证类型不匹配会安全返回 None。
        unsafe { CFMutableDictionary::set_value(Some(dict), key, key) };
        assert_eq!(frame_status(&sb), None);
    }

    #[test]
    fn minimum_frame_interval_leaves_vsync_jitter_margin() {
        // 60fps: 12.5ms 严格位于 120Hz (8.33ms) 与 60Hz (16.67ms) 中点。
        assert_eq!(minimum_frame_interval_us(60), 12_500);
        assert_eq!(minimum_frame_interval_us(120), 6_250);
        assert_eq!(minimum_frame_interval_us(30), 25_000);
        // host_time_us 返回单调非零微秒。
        let t1 = host_time_us();
        let t2 = host_time_us();
        assert!(t1 > 0 && t2 >= t1);
    }

    #[test]
    fn main_display_backing_resolution_is_even_and_at_least_logical() {
        if let (Some(logical), Some(backing)) = (
            main_display_logical_resolution(),
            main_display_backing_resolution(),
        ) {
            assert_eq!(backing.width % 2, 0);
            assert_eq!(backing.height % 2, 0);
            assert!(
                backing.width >= logical.width && backing.height >= logical.height,
                "backing {backing:?} 应不小于逻辑分辨率 {logical:?}"
            );
        }
    }

    #[test]
    fn frame_mailbox_overwrites_old_frames_and_times_out_when_empty() {
        struct TrackedBuffer {
            id: u64,
            dropped: Arc<Mutex<Vec<u64>>>,
        }
        impl Drop for TrackedBuffer {
            fn drop(&mut self) {
                self.dropped.lock().unwrap().push(self.id);
            }
        }

        let dropped = Arc::new(Mutex::new(Vec::new()));
        let mailbox = FrameMailbox::<TrackedBuffer>::new();

        // 1. 连续写入 3 帧不消费：旧帧 1、2 必须被立即覆写并 Drop，仅保留最新帧 3。
        for (id, pts_us) in [(1, 100), (2, 200), (3, 300)] {
            mailbox.push_frame(FramePayload {
                buffer: TrackedBuffer {
                    id,
                    dropped: Arc::clone(&dropped),
                },
                pts_us,
            });
        }
        assert_eq!(
            *dropped.lock().unwrap(),
            vec![1, 2],
            "被新帧覆写的旧帧必须立刻 Drop 以归还缓冲池"
        );

        let received = mailbox
            .recv_timeout(Duration::from_millis(50))
            .expect("应立即取到最新帧");
        match received {
            FrameEvent::Frame(payload) => {
                assert_eq!(payload.buffer.id, 3, "必须丢旧保新，返回最新帧");
                assert_eq!(payload.pts_us, 300);
                // 消费端持有期间帧 3 仍存活，Drop 后归还。
                assert_eq!(*dropped.lock().unwrap(), vec![1, 2]);
            }
            _ => panic!("期望收到 FrameEvent::Frame"),
        }
        assert_eq!(*dropped.lock().unwrap(), vec![1, 2, 3]);

        // 2. 邮箱为空时，零超时与有限超时均返回 None。
        assert!(mailbox.recv_timeout(Duration::ZERO).is_none());
        let start = Instant::now();
        assert!(mailbox.recv_timeout(Duration::from_millis(15)).is_none());
        assert!(
            start.elapsed() >= Duration::from_millis(10),
            "空邮箱应通过 Condvar 等待至超时而非忙等立即返回"
        );

        // 3. 跨线程唤醒：生产者推入新帧后应立刻唤醒阻塞等待中的消费端。
        let producer = mailbox.clone();
        let dropped_clone = Arc::clone(&dropped);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            producer.push_frame(FramePayload {
                buffer: TrackedBuffer {
                    id: 4,
                    dropped: dropped_clone,
                },
                pts_us: 400,
            });
        });
        let wake_start = Instant::now();
        let woke = mailbox
            .recv_timeout(Duration::from_secs(1))
            .expect("生产者唤醒后应收到帧 4");
        assert!(
            wake_start.elapsed() < Duration::from_millis(500),
            "应被 notify_one 及时唤醒而非等到超时"
        );
        match woke {
            FrameEvent::Frame(payload) => {
                assert_eq!(payload.buffer.id, 4);
                assert_eq!(payload.pts_us, 400);
            }
            _ => panic!("期望收到 FrameEvent::Frame(4)"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn frame_mailbox_preserves_control_events_over_new_frames() {
        let mailbox = FrameMailbox::<u64>::new();

        // 1. 先来视频帧、再进控制事件、再覆盖多帧视频：控制事件绝不被覆写且优先返回。
        mailbox.push_frame(FramePayload {
            buffer: 1,
            pts_us: 100,
        });
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_frame(FramePayload {
            buffer: 2,
            pts_us: 200,
        });
        mailbox.push_frame(FramePayload {
            buffer: 3,
            pts_us: 300,
        });

        assert!(
            matches!(
                mailbox.recv_timeout(Duration::from_millis(50)),
                Some(FrameEvent::Suspended)
            ),
            "控制事件 Suspended 必须优先于视频帧且不被后续帧覆写"
        );

        // 取出控制事件后，帧槽中保留的仍是最新帧 3。
        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Frame(FramePayload {
                buffer: 3,
                pts_us: 300,
            })) => {}
            _ => panic!("控制事件取出后，帧槽应保留最新视频帧 3"),
        }

        // 2. 致命 Stopped 优先级高于 Suspended，不会被后续 Suspended 降级，也不被新帧覆写。
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_control(ControlEvent::Stopped {
            message: "permission revoked".to_string(),
        });
        mailbox.push_control(ControlEvent::Suspended);
        mailbox.push_frame(FramePayload {
            buffer: 4,
            pts_us: 400,
        });

        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Stopped { message }) => assert_eq!(message, "permission revoked"),
            _ => panic!("致命 Stopped 不应被后续 Suspended 或视频帧覆写"),
        }
        match mailbox.recv_timeout(Duration::from_millis(50)) {
            Some(FrameEvent::Frame(FramePayload {
                buffer: 4,
                pts_us: 400,
            })) => {}
            _ => panic!("应取出剩余最新视频帧 4"),
        }
        assert!(mailbox.recv_timeout(Duration::from_millis(10)).is_none());
    }

    // PTS 节拍过滤（should_emit_pts）已被整段删除：真机实测 SCK 60fps 内容的
    // 到达节拍是 8.33/16.67ms 交替，按「距上一放行帧」累计间隔判定会把节拍
    // 拉长到 25ms 周期（采集 48fps）。多余供给交给调度器预算门处理，见
    // 上方 `HandlerIvars` 的文档注释。
}
