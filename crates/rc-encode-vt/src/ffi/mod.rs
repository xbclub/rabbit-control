//! VideoToolbox 的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - `VTCompressionSession` 的 C API（Create / SetProperty / EncodeFrame /
//!   CompleteFrames / Invalidate）在这里声明与包装；
//! - 压缩输出回调（`VTCompressionOutputCallback`）在这里定义：只做
//!   retain + 投递邮箱，不做重活；
//! - `CMSampleBuffer` 的提取（CMBlockBuffer 拷出字节、格式描述里的
//!   SPS/PPS）与 AVCC → Annex-B 的重排在等待线程上完成；
//! - `VTDecompressionSession` 的 C API（E6 启动自检的解码验证，
//!   [`decode_verify_annexb`]）：把自检帧真解码回来，而不是只看结构。
//!
//! 对外（`lib.rs`）只暴露安全的包装类型与函数。
//!
//! # 关键落死项（构造期逐条设置，失败即 Err）
//! | 契约 | 属性 |
//! |---|---|
//! | 硬件路径 | 编码器规格 `kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder = true`（显式要求硬件编码器；null 规格下 VT 只是「优先硬件」，落回软编会在构造期变成失败而不是静默上线） |
//! | 低延迟码控 | 编码器规格 `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true`（与 `RealTime`、`ExpectedFrameRate`、`PrioritizeEncodingSpeedOverQuality` 构成 Apple 官方低延迟 4 件套，强制低延迟码控与零 look-ahead） |
//! | M6 / E2 | `kVTCompressionPropertyKey_RealTime = true`（未设时 VT 按文件转码优化，缓冲 20–40ms；同时关掉 look-ahead 类优化。VT 无独立 2-pass 开关，实时路径不存在 2-pass） |
//! | E1 | `kVTCompressionPropertyKey_AllowFrameReordering = false`（禁 B 帧/重排） |
//! | E3 | `kVTCompressionPropertyKey_MaxFrameDelayCount = 0`（输出零延迟） |
//! | 无限 GOP | `gop_len == 0` 经 [`vt_keyframe_interval_params`] 映射为 `kVTCompressionPropertyKey_MaxKeyFrameInterval = i32::MAX`（VT 中 `0` 语义是「由编码器自决」，而非禁用自动关键帧）+ `MaxKeyFrameIntervalDuration = 0.0`（无时长上限），按需 IDR 走 `kVTEncodeFrameOptionKey_ForceKeyFrame` |
//! | 兼容性 | `kVTCompressionPropertyKey_ProfileLevel = Baseline_AutoLevel`（与 caps 的 profiles=[66] 一致） |
//!
//! # 输出格式说明
//! SDK 头文件（VTCompressionProperties.h 等）**没有** Annex-B 输出开关：
//! VT 的压缩输出是 AVCC 风格（长度前缀 NAL + 参数集在格式描述里），
//! 因此输出侧由本模块重排为 Annex-B（转换逻辑在 [`crate::annexb`]，
//! 纯函数、有单测），并在 `EncoderCaps::bitstream_format` 里声明 Annex-B。
//! 解码验证方向相反：Annex-B 重排回 AVCC + 从参数集建
//! `CMVideoFormatDescription`，交给 `VTDecompressionSession` 真解码。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use std::ffi::{c_int, c_void};
use std::fmt;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFNumber, CFRetained, CFString,
};
use objc2_core_media::{
    kCMBlockBufferAlwaysCopyDataFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVPixelBufferCreate;
use objc2_core_video::{
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
};

use crate::annexb;

/// `OSStatus`（CFBase.h：`typedef int32_t OSStatus`）。core-media 绑定把它
/// 保持为 crate 私有，这里自行定义同名别名。
pub(crate) type OSStatus = i32;

// ─────────────────────────────────────────────────────────────────────────────
// 提交线程 QoS（pthread/qos.h，macOS 10.10+，libSystem）
// ─────────────────────────────────────────────────────────────────────────────

/// `QOS_CLASS_USER_INTERACTIVE`（pthread/qos.h：`0x21`）。
const QOS_CLASS_USER_INTERACTIVE: i32 = 0x21;
/// `QOS_CLASS_UNSPECIFIED`（pthread/qos.h：`0x00`）——只由 getter 返回，
/// 不是 set 的合法入参，恢复时映射为语义等价的 [`QOS_CLASS_DEFAULT`]。
const QOS_CLASS_UNSPECIFIED: i32 = 0x00;
/// `QOS_CLASS_DEFAULT`（pthread/qos.h：`0x15`）。
const QOS_CLASS_DEFAULT: i32 = 0x15;

/// pthread QoS C API 声明（pthread/qos.h）。
mod qos_raw {
    /// `pthread_t`（不透明指针）。
    type PthreadT = *mut super::c_void;
    // SAFETY: 声明与 pthread/qos.h 逐字对应；链接 libSystem（C 运行时，
    // macOS 10.10+ 全系统可用）。
    extern "C" {
        pub fn pthread_self() -> PthreadT;
        pub fn pthread_get_qos_class_np(
            thread: PthreadT,
            qos_class: *mut i32,
            relative_priority: *mut i32,
        ) -> i32;
        pub fn pthread_set_qos_class_self_np(qos_class: i32, relative_priority: i32) -> i32;
    }
}

/// 把当前线程的 QoS 抬到 USER_INTERACTIVE，drop 时恢复原值（RAII）。
///
/// VideoToolbox C API **没有**回调队列 QoS 的设置入口：编码工作与输出回调
/// 的调度优先级继承提交线程的 QoS（GCD 工作传播）。默认 QoS
///（UNSPECIFIED/DEFAULT/UTILITY）在系统负载下让提交与回调排队，直接抬高
/// 输入→输出延迟——而低延迟编码本身就是 USER_INTERACTIVE 级的工作。
/// 抬升只覆盖一次 [`CompressionSession::encode`]（提交 → 等待 → 提取），
/// 任何退出路径都经 Drop 恢复原值，不向调用方泄漏副作用。任何一步失败
/// 都保持现状（QoS 不动，编码照常进行）。
struct QosBump {
    /// 恢复用的 `(class, priority)`；None = 无需恢复（读取/设置失败，或
    /// 本就不低于目标——本就不动）。
    restore: Option<(i32, i32)>,
}

impl QosBump {
    fn new() -> Self {
        let mut class: i32 = QOS_CLASS_UNSPECIFIED;
        let mut priority: i32 = 0;
        // SAFETY: 两个出参指向本栈帧合法内存；pthread_self 返回当前线程
        // 句柄，getter 是同步只读调用。
        let ok = unsafe {
            qos_raw::pthread_get_qos_class_np(qos_raw::pthread_self(), &mut class, &mut priority)
        };
        if ok != 0 || class >= QOS_CLASS_USER_INTERACTIVE {
            return Self { restore: None };
        }
        // SAFETY: 只作用于当前线程；USER_INTERACTIVE 与 priority 0 是合法入参。
        let ok = unsafe { qos_raw::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
        if ok != 0 {
            return Self { restore: None };
        }
        Self {
            restore: Some((class, priority)),
        }
    }
}

impl Drop for QosBump {
    fn drop(&mut self) {
        let Some((class, priority)) = self.restore else {
            return;
        };
        // UNSPECIFIED 是 getter 专属值，不能回设，映射为语义等价的 DEFAULT。
        let class = if class == QOS_CLASS_UNSPECIFIED {
            QOS_CLASS_DEFAULT
        } else {
            class
        };
        // SAFETY: 只作用于当前线程；class 来自本线程 getter 的原值（或
        // DEFAULT），priority 同理，均为合法入参。
        unsafe { qos_raw::pthread_set_qos_class_self_np(class, priority) };
    }
}

/// `kCMVideoCodecType_H264`（'avc1'，CMFormatDescription.h）。FourCharCode = u32。
const K_CM_VIDEO_CODEC_TYPE_H264: u32 = 0x61766331;

/// `kVTEncodeInfo_FrameDropped`（VTErrors.h：`1UL << 1`）。
const K_VTENCODE_INFO_FRAME_DROPPED: u32 = 1 << 1;

/// `kVTPropertyNotSupportedErr`（VTErrors.h）：属性被当前编码器拒绝。
const K_VT_PROPERTY_NOT_SUPPORTED_ERR: OSStatus = -12900;

/// `'420v'`（NV12 video range）—— 与 SCK 采集侧约定一致的输入格式（M5）。
pub(crate) const PIXEL_FORMAT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

/// `kCMTimeInvalid`：flags 为空（不含 Valid）即非数值时间。
/// `VTCompressionSessionCompleteFrames` 对非数值时间会冲刷**全部**在途帧。
const CMTIME_INVALID: CMTime = CMTime {
    epoch: 0,
    value: 0,
    timescale: 0,
    flags: CMTimeFlags::empty(),
};

// ─────────────────────────────────────────────────────────────────────────────
// VideoToolbox C API 声明（VTCompressionSession.h / VTSession.h）
// ─────────────────────────────────────────────────────────────────────────────

/// 压缩输出回调签名（VTCompressionSession.h：`typedef void (*VTCompressionOutputCallback)`）。
type VtOutputCallbackFn = unsafe extern "C" fn(
    output_callback_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: *mut CMSampleBuffer,
);

/// 解码输出回调签名（VTDecompressionSession.h：
/// `typedef void (*VTDecompressionOutputCallback)`）。按 C 惯例
/// `CVImageBufferRef` 重解释为 `CVPixelBuffer`（H.264 解码输出的真身）。
type VtDecodeCallbackFn = unsafe extern "C" fn(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: *mut CVPixelBuffer,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

/// `VTDecompressionOutputCallbackRecord`（按值传给 `VTDecompressionSessionCreate`）。
#[repr(C)]
struct VtDecompressionOutputCallbackRecord {
    /// 回调函数指针（本实现恒为 `Some`）。
    callback: VtDecodeCallbackFn,
    /// 回调 refcon（指向 `Arc<DecodeShared>` 的堆分配）。
    refcon: *mut c_void,
}

/// 属性键与函数的裸声明。会话句柄按 C 惯例用 `*mut c_void`（指向不透明结构）。
mod raw {
    use super::{
        c_void, CFString, CMTime, OSStatus, VtDecompressionOutputCallbackRecord, VtOutputCallbackFn,
    };

    // SAFETY: 声明与 VideoToolbox 头文件逐字对应；链接 VideoToolbox 框架。
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        // —— 属性键（CFStringRef 常量，永不指向无效内存但按 C 头文件是可空指针类型）——
        pub static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:
            Option<&'static CFString>;
        pub static kVTVideoEncoderSpecification_EnableLowLatencyRateControl:
            Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_RealTime: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_ProfileLevel: Option<&'static CFString>;
        pub static kVTProfileLevel_H264_Baseline_AutoLevel: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_AllowFrameReordering: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_MaxKeyFrameInterval: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_MaxFrameDelayCount: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_AverageBitRate: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_ExpectedFrameRate: Option<&'static CFString>;
        pub static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality:
            Option<&'static CFString>;
        pub static kVTEncodeFrameOptionKey_ForceKeyFrame: Option<&'static CFString>;

        // —— 会话函数 ——
        pub fn VTCompressionSessionCreate(
            allocator: *const c_void,
            width: i32,
            height: i32,
            codec_type: u32,
            encoder_specification: *const c_void,
            source_image_buffer_attributes: *const c_void,
            compressed_data_allocator: *const c_void,
            output_callback: Option<VtOutputCallbackFn>,
            output_callback_refcon: *mut c_void,
            compression_session_out: *mut *mut c_void,
        ) -> OSStatus;
        /// 在 VTSession.h；`VTCompressionSessionRef` 可安全重解释为 `VTSessionRef`。
        pub fn VTSessionSetProperty(
            session: *mut c_void,
            property_key: *const CFString,
            property_value: *const c_void,
        ) -> OSStatus;
        pub fn VTCompressionSessionEncodeFrame(
            session: *mut c_void,
            image_buffer: *mut c_void,
            presentation_time_stamp: CMTime,
            duration: CMTime,
            frame_properties: *const c_void,
            source_frame_refcon: *mut c_void,
            info_flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTCompressionSessionCompleteFrames(
            session: *mut c_void,
            complete_until_presentation_time_stamp: CMTime,
        ) -> OSStatus;
        pub fn VTCompressionSessionInvalidate(session: *mut c_void);

        // —— 解码会话（E6 解码验证）——
        pub fn VTDecompressionSessionCreate(
            allocator: *const c_void,
            video_format_description: *const c_void,
            decoder_specification: *const c_void,
            image_buffer_attributes: *const c_void,
            output_callback: *const VtDecompressionOutputCallbackRecord,
            session_out: *mut *mut c_void,
        ) -> OSStatus;
        pub fn VTDecompressionSessionDecodeFrame(
            session: *mut c_void,
            sample_buffer: *mut c_void,
            decode_flags: u32,
            source_frame_refcon: *mut c_void,
            info_flags_out: *mut u32,
        ) -> OSStatus;
        pub fn VTDecompressionSessionWaitForAsynchronousFrames(session: *mut c_void) -> OSStatus;
        pub fn VTDecompressionSessionInvalidate(session: *mut c_void);
    }

    // SAFETY: CoreFoundation 的 CFRelease，同样按 C 头文件声明（CFBase.h）。
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
    }
}

/// 属性键枚举：把 extern static 的 unsafe 读取集中到 [`VtKey::get`] 一处。
#[derive(Debug, Clone, Copy)]
enum VtKey {
    /// RealTime（M6/E2）。
    RealTime,
    /// ProfileLevel。
    ProfileLevel,
    /// H.264 Baseline AutoLevel 档位值。
    ProfileLevelH264BaselineAutoLevel,
    /// AllowFrameReordering（E1）。
    AllowFrameReordering,
    /// MaxKeyFrameInterval（无限 GOP）。
    MaxKeyFrameInterval,
    /// MaxKeyFrameIntervalDuration。
    MaxKeyFrameIntervalDuration,
    /// MaxFrameDelayCount（E3）。
    MaxFrameDelayCount,
    /// AverageBitRate。
    AverageBitRate,
    /// ExpectedFrameRate。
    ExpectedFrameRate,
    /// PrioritizeEncodingSpeedOverQuality（预设微调）。
    PrioritizeEncodingSpeedOverQuality,
    /// 帧选项：ForceKeyFrame（按需 IDR）。
    ForceKeyFrame,
}

impl VtKey {
    /// 键名（错误信息与诊断用）。
    fn name(self) -> &'static str {
        match self {
            VtKey::RealTime => "kVTCompressionPropertyKey_RealTime",
            VtKey::ProfileLevel => "kVTCompressionPropertyKey_ProfileLevel",
            VtKey::ProfileLevelH264BaselineAutoLevel => "kVTProfileLevel_H264_Baseline_AutoLevel",
            VtKey::AllowFrameReordering => "kVTCompressionPropertyKey_AllowFrameReordering",
            VtKey::MaxKeyFrameInterval => "kVTCompressionPropertyKey_MaxKeyFrameInterval",
            VtKey::MaxKeyFrameIntervalDuration => {
                "kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration"
            }
            VtKey::MaxFrameDelayCount => "kVTCompressionPropertyKey_MaxFrameDelayCount",
            VtKey::AverageBitRate => "kVTCompressionPropertyKey_AverageBitRate",
            VtKey::ExpectedFrameRate => "kVTCompressionPropertyKey_ExpectedFrameRate",
            VtKey::PrioritizeEncodingSpeedOverQuality => {
                "kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality"
            }
            VtKey::ForceKeyFrame => "kVTEncodeFrameOptionKey_ForceKeyFrame",
        }
    }

    /// 读取对应的 CFString 常量。缺失视为安装损坏（这些常量自 macOS
    /// 10.8/10.9 起就存在，正常运行不可能拿不到）—— 此时返回 `Err`
    /// （错误经构造/编码路径上报），而不是 panic。
    fn get(self) -> Result<&'static CFString, String> {
        // SAFETY: 只对 extern static 做一次读取（拿 CFString 引用，不写）；
        // 这些 SDK 常量在框架加载时初始化、永不释放，读取无数据竞争。
        let value = unsafe {
            match self {
                VtKey::RealTime => raw::kVTCompressionPropertyKey_RealTime,
                VtKey::ProfileLevel => raw::kVTCompressionPropertyKey_ProfileLevel,
                VtKey::ProfileLevelH264BaselineAutoLevel => {
                    raw::kVTProfileLevel_H264_Baseline_AutoLevel
                }
                VtKey::AllowFrameReordering => raw::kVTCompressionPropertyKey_AllowFrameReordering,
                VtKey::MaxKeyFrameInterval => raw::kVTCompressionPropertyKey_MaxKeyFrameInterval,
                VtKey::MaxKeyFrameIntervalDuration => {
                    raw::kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration
                }
                VtKey::MaxFrameDelayCount => raw::kVTCompressionPropertyKey_MaxFrameDelayCount,
                VtKey::AverageBitRate => raw::kVTCompressionPropertyKey_AverageBitRate,
                VtKey::ExpectedFrameRate => raw::kVTCompressionPropertyKey_ExpectedFrameRate,
                VtKey::PrioritizeEncodingSpeedOverQuality => {
                    raw::kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality
                }
                VtKey::ForceKeyFrame => raw::kVTEncodeFrameOptionKey_ForceKeyFrame,
            }
        };
        value.ok_or_else(|| format!("VideoToolbox SDK 常量缺失: {}", self.name()))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 输出邮箱：回调线程 → 等待线程
// ─────────────────────────────────────────────────────────────────────────────

/// 一次编码的结果状态。回调写入，等待线程消费。
enum CallbackState {
    /// 尚无输出。
    Empty,
    /// 输出就绪（回调里已 retain +1 的 sample buffer）。
    Ready(CFRetained<CMSampleBuffer>),
    /// 编码失败（status != 0，或成功但没有任何输出）。
    Failed(OSStatus),
    /// 编码器主动丢帧（`kVTEncodeInfo_FrameDropped`，RealTime 过载时出现）。
    Dropped,
}

/// 回调与等待线程共享的邮箱。
struct Shared {
    state: Mutex<CallbackState>,
    cond: Condvar,
}

// SAFETY: Shared 的全部可变访问都在 state 这个 Mutex 里；CallbackState 持有
// 的 CF 对象（CMSampleBuffer）本身线程安全（retain/release 线程安全，
// 且我们从不并发读取其内容——只在持有锁的线程上提取）。
unsafe impl Send for Shared {}
// SAFETY: 同上——Sync 的共享前提是所有访问都在 Mutex 内，CF 对象的
// retain/release 本身线程安全。
unsafe impl Sync for Shared {}

/// 锁状态邮箱（容忍中毒：回调线程绝不 panic，中毒只可能来自等待线程的
/// bug，此时继续消费状态比 panic 传播更安全）。压缩与解码两个邮箱共用。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// VT 压缩输出回调。运行在 VideoToolbox 内部队列上：只 retain + 投递邮箱，
/// 绝不做重活、绝不 panic（跨 C 边界 unwind 是未定义行为）。
unsafe extern "C" fn vt_output_callback(
    output_callback_refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: *mut CMSampleBuffer,
) {
    #[cfg(test)]
    latency_diag::mark_callback();
    // SAFETY: refcon 是会话创建时传入的 `Arc::as_ptr(&shared)`，指向
    // CompressionSession.shared 的堆内层分配。存活期由 CompressionSession
    // 持有的 Arc 克隆保证：Drop 先 Invalidate（不再有新回调）再做锁屏障
    //（等在途回调离开临界区），最后才释放 Arc，因此这里的解引用不可能悬垂。
    let shared = unsafe { &*(output_callback_refcon as *const Shared) };
    let state = if status == 0 {
        match NonNull::new(sample_buffer) {
            Some(sb) => {
                // SAFETY: 回调契约保证 sample_buffer 在回调期间有效；
                // retain(+1) 升级为自有引用，随邮箱状态移动/释放。
                CallbackState::Ready(unsafe { CFRetained::retain(sb) })
            }
            None if info_flags & K_VTENCODE_INFO_FRAME_DROPPED != 0 => CallbackState::Dropped,
            None => CallbackState::Failed(0),
        }
    } else {
        CallbackState::Failed(status)
    };
    let mut guard = lock(&shared.state);
    *guard = state;
    drop(guard);
    shared.cond.notify_all();
}

// ─────────────────────────────────────────────────────────────────────────────
// 对 lib.rs 暴露的类型
// ─────────────────────────────────────────────────────────────────────────────

/// 一次编码的输出（Annex-B 码流）。
pub(crate) struct EncodedOutput {
    /// Annex-B 字节流（IDR 帧内联 SPS/PPS）。
    pub data: Vec<u8>,
    /// 是否关键帧（码流里存在 IDR slice，type 5）。
    pub keyframe: bool,
}

/// 编码失败原因（lib.rs 映射到 rc_types::Error）。
pub(crate) enum EncodeError {
    /// 等到截止时间仍无输出（E5）。
    Timeout,
    /// 编码器丢弃了本帧（RealTime 过载）。
    Dropped,
    /// VideoToolbox 报错（会话/提取失败）。
    Failed(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Timeout => write!(f, "等待压缩输出超时"),
            EncodeError::Dropped => write!(f, "编码器丢弃了本帧（RealTime 过载）"),
            EncodeError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// 拥有型 CVPixelBuffer（`CVPixelBufferCreate` +1），供 E6 自检与真机测试。
pub(crate) struct TestPixelBuffer(CFRetained<CVPixelBuffer>);

impl TestPixelBuffer {
    /// 不透明句柄（与 SCK 交付给 [`rc_capture::FrameTexture::CVPixelBuffer`]
    /// 的形态一致）。
    pub(crate) fn as_opaque_ptr(&self) -> *mut c_void {
        CFRetained::as_ptr(&self.0).as_ptr().cast()
    }
}

/// 创建一块内容未定义（无需初始化，编码只验证会话可用性）的 '420v' 像素缓冲。
///
/// 注意：这里**不** LockBaseAddress 写内容——零拷贝路径不碰 CPU 像素。
pub(crate) fn make_pixel_buffer(width: u32, height: u32) -> Result<TestPixelBuffer, String> {
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: out 是合法出参（addr_of_mut 规避 &mut 别名）；分配器与属性
    // 传 None 合法（使用系统默认分配器、无附加属性）。NonNull::from 对
    // 栈上 &mut 是无条件非空的（不可能 panic）。
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            width as usize,
            height as usize,
            PIXEL_FORMAT_420V,
            None,
            // SAFETY: &mut out 是本栈帧上的唯一可变借用，满足 NonNull 出参要求；
            // CVPixelBufferCreate 同步返回后借用即结束。
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        return Err(format!(
            "CVPixelBufferCreate({width}x{height}) 失败: CVReturn {status}"
        ));
    }
    match NonNull::new(out) {
        // SAFETY: Create 成功后 out 是 +1 所有权引用（CM_RETURNS_RETAINED_PARAMETER）。
        Some(ptr) => Ok(TestPixelBuffer(unsafe { CFRetained::from_raw(ptr) })),
        None => Err("CVPixelBufferCreate 成功但返回空缓冲".to_string()),
    }
}

/// `(width, height, pixel_format)`。句柄校验（尺寸核对、M9 防线）用。
pub(crate) fn pixel_buffer_info(ptr: *mut c_void) -> (u32, u32, u32) {
    // SAFETY: 调用方（lib.rs::encode）已校验句柄非空，且处在 FrameTexture
    // 的存活窗口内（下一次采集前有效），这里只做同步读取。
    let buffer = unsafe { &*(ptr as *const CVPixelBuffer) };
    (
        CVPixelBufferGetWidth(buffer) as u32,
        CVPixelBufferGetHeight(buffer) as u32,
        CVPixelBufferGetPixelFormatType(buffer),
    )
}

/// 一个配置好的 `VTCompressionSession`。
///
/// `Send`：随 `VtEncoder`（`Encoder: Send`）跨线程**移交所有权**；所有
/// 访问都发生在持有 `&mut self` 的单一线程上，CF 对象的 retain/release
/// 本身线程安全。
pub(crate) struct CompressionSession {
    /// +1 的会话引用（VTCompressionSessionCreate 返回）。Drop 前必须先
    /// Invalidate（Apple 文档要求），见 [`CompressionSession::drop`]。
    session: NonNull<c_void>,
    /// 回调邮箱。必须排在 session 之后：字段按声明逆序释放，Arc 在
    /// CFRelease(session) 之后才 drop。
    shared: Arc<Shared>,
    /// 会话内单调递增的帧号（PTS 来源，见 [`CompressionSession::next_pts`]）。
    frame_index: u64,
    /// PTS timescale = fps * 1000，帧间隔恰为 1/fps 秒。
    timescale: i32,
}

// SAFETY: 见类型文档：所有权随 VtEncoder 移动，无并发访问；回调线程只经
// Shared（已声明 Send + Sync）触碰数据。
unsafe impl Send for CompressionSession {}

/// VideoToolbox 关键帧间隔属性对。
///
/// 由 [`vt_keyframe_interval_params`] 从配置层的 `gop_len` 纯函数映射得到。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VtKeyframeInterval {
    /// `kVTCompressionPropertyKey_MaxKeyFrameInterval`（帧数上限，恒 > 0）。
    pub max_interval_frames: i32,
    /// `kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration`（秒；`0.0` = 无时长上限）。
    pub max_interval_duration_secs: f64,
}

/// 将配置层的 `gop_len` 映射为 VideoToolbox 关键帧间隔属性。
///
/// - `gop_len == 0` 表示调用方要求无限 GOP / 仅按需 IDR。在 `VTCompressionProperties.h`
///   中，`MaxKeyFrameInterval = 0` 的语义是「由编码器自决关键帧位置」（Apple Silicon
///   真机实测每 30 帧自动插 1 个 IDR），因此必须映射为 `i32::MAX`（60fps 下约 414 天）；
/// - `gop_len > 0` 按帧数直接映射，超过 `i32::MAX` 时饱和钳制到 `i32::MAX`（避免 `as i32`
///   溢出回绕为负数）；
/// - `MaxKeyFrameIntervalDuration` 统一为 `0.0`（Apple 文档定义 `0` 为无时长上限，避免
///   时间间隔抢先触发自动 IDR）。
pub(crate) fn vt_keyframe_interval_params(gop_len: u32) -> VtKeyframeInterval {
    let max_interval_frames = if gop_len == 0 {
        i32::MAX
    } else {
        gop_len.min(i32::MAX as u32) as i32
    };
    VtKeyframeInterval {
        max_interval_frames,
        max_interval_duration_secs: 0.0,
    }
}

/// 构造参数（lib.rs 从 EncoderConfig 映射而来）。
pub(crate) struct SessionParams {
    pub width: u32,
    pub height: u32,
    pub bitrate_bps: u32,
    pub fps: u32,
    /// 0 = 无限 GOP（不自动插关键帧，按需 IDR）。
    pub gop_len: u32,
    /// 预设映射：Preset::Fastest / Fast → true（速度优先）。
    pub speed_over_quality: bool,
}

impl CompressionSession {
    /// 创建会话并把延迟红线属性逐条落死（M6/E1/E2/E3/无限 GOP）。
    ///
    /// 任何红线属性设置失败都返回 Err（构造期暴露，而不是运行期花屏）。
    pub(crate) fn new(params: &SessionParams) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            state: Mutex::new(CallbackState::Empty),
            cond: Condvar::new(),
        });

        let mut session_out: *mut c_void = std::ptr::null_mut();
        // 显式要求硬件编码器 + 启用低延迟码控（VTVideoEncoder.h / VTCompressionProperties.h：
        // kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder = true、
        // kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true）。
        // null 规格下 VT「优先硬件」仍可能在个别系统状态下落回软件编码器，
        // 显式 true 把落回变成构造失败（E6 前就能暴露）；同时开启低延迟码控
        // 完成 Apple 官方低延迟 4 件套（规格 + RealTime + ExpectedFrameRate +
        // PrioritizeEncodingSpeedOverQuality）。dict 在 Create 同步返回前存活。
        let encoder_spec = encoder_specification_dictionary()?;
        // SAFETY: 出参 session_out 合法；encoder_spec 指向栈上存活的
        // CFDictionary（Create 同步读取）；其余可空参数传 null = 系统默认；
        // refcon 指向 shared 的稳定堆分配。
        let status = unsafe {
            raw::VTCompressionSessionCreate(
                std::ptr::null(),
                params.width as i32,
                params.height as i32,
                K_CM_VIDEO_CODEC_TYPE_H264,
                &*encoder_spec as *const CFDictionary as *const c_void,
                std::ptr::null(),
                std::ptr::null(),
                Some(vt_output_callback),
                Arc::as_ptr(&shared) as *mut c_void,
                std::ptr::addr_of_mut!(session_out),
            )
        };
        if status != 0 {
            return Err(format!(
                "VTCompressionSessionCreate({}x{}) 失败: OSStatus {status}",
                params.width, params.height
            ));
        }
        let session = NonNull::new(session_out)
            .ok_or_else(|| "VTCompressionSessionCreate 返回 OSStatus 0 但会话为空".to_string())?;

        let timescale = (params.fps.saturating_mul(1000)).min(i32::MAX as u32) as i32;
        let mut s = CompressionSession {
            session,
            shared,
            frame_index: 0,
            timescale,
        };

        // —— 延迟红线逐条落死（失败即构造失败）——
        // M6/E2：RealTime = true。未设时 VT 按文件转码优化（缓冲 20–40ms），
        // 且会启用 look-ahead 类分析；VT 实时路径不存在 2-pass。
        s.set_boolean(VtKey::RealTime, true, " (M6/E2)")?;
        // E1：禁帧重排（B 帧）。
        s.set_boolean(VtKey::AllowFrameReordering, false, " (E1)")?;
        // E3：输出零延迟。尽力设置 MaxFrameDelayCount = 0（编码器不得压帧
        // 等待重排/优化时机）——注意：实测（Apple Silicon 真机，见
        // real_machine 测试 max_frame_delay_property_status_on_this_device）
        // 硬件 H.264 编码器对该属性一律返回 kVTPropertyNotSupportedErr
        //（0/1/2 值同拒）。这种平台上 E3 由本包装的结构保证兜底：RealTime +
        // 禁重排 + 严格单帧在途（encode 在消费完上一帧输出前绝不提交下一帧，
        // 见 [`CompressionSession::encode`] 开头的邮箱清空），会话内不存在
        // 任何输出缓冲。仅 kVTPropertyNotSupportedErr 被容忍，其它状态码
        // 仍然是构造失败。
        let status = s.set_value_status(VtKey::MaxFrameDelayCount, &*CFNumber::new_i32(0))?;
        if status != 0 && status != K_VT_PROPERTY_NOT_SUPPORTED_ERR {
            return Err(format!(
                "设置 {} (E3) 失败: OSStatus {status}",
                VtKey::MaxFrameDelayCount.name()
            ));
        }
        // 无限 GOP + 按需 IDR：VideoToolbox 中 MaxKeyFrameInterval = 0 表示
        // 「由编码器自决」（Apple Silicon 实测会每 30 帧自动插 IDR），并非
        // 禁用自动关键帧；经 vt_keyframe_interval_params 把 gop_len == 0 映射为
        // i32::MAX，并设 MaxKeyFrameIntervalDuration = 0.0（无时间间隔上限），
        // 使关键帧只由首帧与 kVTEncodeFrameOptionKey_ForceKeyFrame 触发。
        let keyframe_interval = vt_keyframe_interval_params(params.gop_len);
        s.set_number(
            VtKey::MaxKeyFrameInterval,
            CFNumber::new_i32(keyframe_interval.max_interval_frames),
            " (GOP)",
        )?;
        s.set_number(
            VtKey::MaxKeyFrameIntervalDuration,
            CFNumber::new_f64(keyframe_interval.max_interval_duration_secs),
            "",
        )?;
        s.set_number(
            VtKey::AverageBitRate,
            CFNumber::new_i64(params.bitrate_bps as i64),
            "",
        )?;
        s.set_number(
            VtKey::ExpectedFrameRate,
            CFNumber::new_i32(params.fps as i32),
            "",
        )?;
        // Baseline（CAVLC）：与 caps 声明的 profiles=[66] 一致，兼容性最好。
        let baseline = VtKey::ProfileLevelH264BaselineAutoLevel.get()?;
        s.set_value(VtKey::ProfileLevel, baseline, " (Baseline_AutoLevel)")?;

        // 预设微调（速度优先）：不是延迟红线（RealTime 已经保证延迟），
        // 个别编码器可能不支持（头文件标注 Optional），失败容忍。
        if params.speed_over_quality {
            let _ = s.set_boolean(VtKey::PrioritizeEncodingSpeedOverQuality, true, "");
        }
        Ok(s)
    }

    /// 设置布尔属性；红线属性失败返回 Err。
    fn set_boolean(&mut self, key: VtKey, value: bool, label: &str) -> Result<(), String> {
        self.set_value(key, CFBoolean::new(value), label)
    }

    /// 设置数值属性（调用方构造好 CFNumber）。
    fn set_number(
        &mut self,
        key: VtKey,
        value: CFRetained<CFNumber>,
        label: &str,
    ) -> Result<(), String> {
        self.set_value(key, &*value, label)
    }

    /// 设置任意 CF 值属性（布尔/数值共用），返回原始 OSStatus。
    /// 属性键常量缺失返回 `Err`（不 panic）。
    fn set_value_status<T>(&mut self, key: VtKey, value: &T) -> Result<OSStatus, String>
    where
        T: ?Sized,
    {
        // SAFETY: self.session 是本包装拥有的合法会话；键是 SDK 常量 CFString；
        // value 指向调用栈上存活的 CF 对象（VTSessionSetProperty 同步读取）。
        unsafe {
            Ok(raw::VTSessionSetProperty(
                self.session.as_ptr(),
                key.get()?,
                value as *const T as *const c_void,
            ))
        }
    }

    /// 设置任意 CF 值属性（布尔/数值共用）；失败返回 Err。
    fn set_value<T>(&mut self, key: VtKey, value: &T, label: &str) -> Result<(), String>
    where
        T: ?Sized,
    {
        check_status(
            self.set_value_status(key, value)?,
            &format!("{}{}", key.name(), label),
        )
    }

    /// 会话内 PTS：帧号 * 1000 / (fps*1000) —— 严格单调递增，帧间隔 = 1/fps。
    ///
    /// VT 只看 PTS 的单调性；帧的语义时间戳由上层（EncodedFrame / 协议头）
    /// 承载，不写进码流。
    fn next_pts(&self) -> CMTime {
        CMTime {
            epoch: 0,
            flags: CMTimeFlags::Valid,
            value: (self.frame_index as i64) * 1000,
            timescale: self.timescale,
        }
    }

    /// 动态调整目标码率（自适应码率入口）。
    pub(crate) fn set_bitrate(&mut self, bps: u32) -> Result<(), String> {
        self.set_number(
            VtKey::AverageBitRate,
            CFNumber::new_i64(bps as i64),
            " (set_bitrate)",
        )
    }

    /// E3 真机诊断：重试设置 `MaxFrameDelayCount = 0`，返回**原始 OSStatus**。
    ///
    /// 构造路径只容忍「成功(0)」与 `kVTPropertyNotSupportedErr(-12900)` 两种
    /// 结果（见 [`CompressionSession::new`]）；本方法把「本机编码器到底返回
    /// 什么」变成可观测事实，供真机测试实证容忍范围没有写错。重试不改变
    /// 会话语义：成功时值与构造期一致，被拒时属性保持编码器默认（结构
    /// 保证兜底，见 new 的 E3 注释）。
    #[cfg(test)]
    pub(crate) fn probe_max_frame_delay_zero_status(&mut self) -> Result<OSStatus, String> {
        self.set_value_status(VtKey::MaxFrameDelayCount, &*CFNumber::new_i32(0))
    }

    /// 编码一帧：CVPixelBuffer **零拷贝**直通 VT，等待输出最多 `wait`。
    ///
    /// - 不 LockBaseAddress、不读 CPU 像素：GPU 缓冲直接交给编码器；
    /// - `force_idr`：本帧强制关键帧（`kVTEncodeFrameOptionKey_ForceKeyFrame`）；
    /// - E5：等到 `wait` 截止仍无输出返回 [`EncodeError::Timeout`]，绝不无限阻塞。
    pub(crate) fn encode(
        &mut self,
        buffer: *mut c_void,
        force_idr: bool,
        wait: Duration,
    ) -> Result<EncodedOutput, EncodeError> {
        // 编码提交/等待/提取按 USER_INTERACTIVE 调度（回调队列 QoS 继承
        // 提交线程，见 QosBump 文档）；guard 在任何退出路径恢复原值。
        let _qos = QosBump::new();
        // 清空上一帧的迟到输出（超时丢弃策略：迟到帧弃用，调用方已请求 IDR 恢复）。
        {
            let mut guard = lock(&self.shared.state);
            if !matches!(*guard, CallbackState::Empty) {
                *guard = CallbackState::Empty;
            }
        }

        // 按需构造 ForceKeyFrame 帧选项字典（一次性使用，不跨调用复用）。
        let frame_props = if force_idr {
            Some(force_keyframe_dictionary()?)
        } else {
            None
        };
        let props_ptr = frame_props
            .as_ref()
            .map(|dict| &**dict as *const CFDictionary as *const c_void)
            .unwrap_or_else(std::ptr::null);

        let pts = self.next_pts();
        let mut info_flags: u32 = 0;
        // SAFETY: 会话由本包装拥有；buffer 句柄经 lib.rs 校验非空且在
        // FrameTexture 存活窗口内（同步等待输出，不会越窗口使用）；
        // props_ptr 为 null 或指向栈上存活的 CFDictionary；info_flags_out
        // 是合法可写出参。零拷贝：不 LockBaseAddress，直接交 GPU 缓冲。
        let status = unsafe {
            raw::VTCompressionSessionEncodeFrame(
                self.session.as_ptr(),
                buffer,
                pts,
                CMTIME_INVALID,
                props_ptr,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(info_flags),
            )
        };
        if status != 0 {
            return Err(EncodeError::Failed(format!(
                "VTCompressionSessionEncodeFrame 失败: OSStatus {status}"
            )));
        }
        #[cfg(test)]
        latency_diag::mark_submitted();
        // 帧号必须严格单调（E4 的 PTS 前提）。u64 耗尽在物理上不可达
        //（60fps 也要 97 亿年），真发生了宁可报错也绝不回绕/饱和。
        let next_index = self.frame_index.checked_add(1).ok_or_else(|| {
            EncodeError::Failed("会话帧号耗尽 u64，无法继续保证 PTS 单调".to_string())
        })?;
        self.frame_index = next_index;

        // 有界等待输出（E5）。
        let deadline = Instant::now()
            .checked_add(wait)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        let mut guard = lock(&self.shared.state);
        loop {
            match std::mem::replace(&mut *guard, CallbackState::Empty) {
                CallbackState::Empty => {}
                CallbackState::Ready(sample) => {
                    drop(guard);
                    #[cfg(test)]
                    latency_diag::mark_woken();
                    let out = self.extract(sample);
                    #[cfg(test)]
                    latency_diag::mark_returned();
                    return out;
                }
                CallbackState::Failed(status) => {
                    drop(guard);
                    return Err(EncodeError::Failed(format!(
                        "VideoToolbox 输出携带错误状态: OSStatus {status}"
                    )));
                }
                CallbackState::Dropped => {
                    drop(guard);
                    return Err(EncodeError::Dropped);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(EncodeError::Timeout);
            }
            let (next_guard, _) = self
                .shared
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next_guard;
        }
    }

    /// 从 sample buffer 提取 Annex-B 码流：CMBlockBuffer 拷出 → 判定封装 →
    /// 重排（AVCC）→ IDR 帧内联 SPS/PPS。
    fn extract(&self, sample: CFRetained<CMSampleBuffer>) -> Result<EncodedOutput, EncodeError> {
        // SAFETY: sample 是 +1 自有引用；三个 getter 均为同步只读调用。
        let block = unsafe { sample.data_buffer() }.ok_or_else(|| {
            EncodeError::Failed("输出 sample buffer 无 CMBlockBuffer".to_string())
        })?;
        // SAFETY: block 是刚 retain 的自有引用。
        let len = unsafe { block.data_length() };
        if len == 0 {
            return Err(EncodeError::Failed("输出 CMBlockBuffer 为空".to_string()));
        }
        let mut raw = vec![0u8; len];
        // SAFETY: destination 指向 raw 的合法可写内存，长度与 data_length 匹配；
        // len > 0 已在上方确认，NonNull::from 对非空切片恒成功。
        let status =
            unsafe { block.copy_data_bytes(0, len, NonNull::from(raw.as_mut_slice()).cast()) };
        if status != 0 {
            return Err(EncodeError::Failed(format!(
                "CMBlockBufferCopyDataBytes 失败: OSStatus {status}"
            )));
        }

        // 参数集（SPS/PPS）与 AVCC 长度前缀宽度来自格式描述。
        let mut parameter_sets: Vec<Vec<u8>> = Vec::new();
        let mut length_size = 4usize;
        // SAFETY: format_description 对 +0 返回值做 retain，得到自有引用。
        if let Some(desc) = unsafe { sample.format_description() } {
            let mut total = 0usize;
            for index in 0..16usize {
                let mut ptr: *const u8 = std::ptr::null();
                let mut size = 0usize;
                let mut count_out = 0usize;
                let mut nal_header_len: c_int = 0;
                // SAFETY: 全部出参指针指向本栈帧合法内存；desc 在整个循环中
                // 由 desc（自有引用）保活，参数集指针因此有效。
                let status = unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &desc,
                        index,
                        std::ptr::addr_of_mut!(ptr),
                        std::ptr::addr_of_mut!(size),
                        std::ptr::addr_of_mut!(count_out),
                        std::ptr::addr_of_mut!(nal_header_len),
                    )
                };
                if status != 0 {
                    break;
                }
                if index == 0 {
                    total = count_out;
                    if nal_header_len > 0 {
                        length_size = nal_header_len as usize;
                    }
                }
                if size > 0 {
                    // SAFETY: GetH264ParameterSetAtIndex 成功时 [ptr, ptr+size)
                    // 是可读内存（指向 desc 内部），上面已确认 desc 保活。
                    parameter_sets.push(unsafe { std::slice::from_raw_parts(ptr, size).to_vec() });
                }
                if total != 0 && index + 1 >= total {
                    break;
                }
            }
        }

        // 判定封装并规整为 Annex-B；畸形输出绝不外发。
        let (mut data, source) = annexb::normalize_to_annexb(&raw, length_size)
            .ok_or_else(|| EncodeError::Failed("输出码流既非合法 AVCC 也非 Annex-B".to_string()))?;
        let keyframe = annexb::is_idr(&data);
        // 参数集内联只发生在 AVCC 源 + IDR 帧：Annex-B 源自带参数集，P 帧不需要。
        if keyframe && source == annexb::SourceFormat::Avcc && !parameter_sets.is_empty() {
            let refs: Vec<&[u8]> = parameter_sets.iter().map(|v| v.as_slice()).collect();
            annexb::prepend_parameter_sets(&mut data, &refs);
        }
        Ok(EncodedOutput { data, keyframe })
    }
}

impl Drop for CompressionSession {
    fn drop(&mut self) {
        // SAFETY: self.session 是 Create 返回的 +1 引用；Apple 文档要求
        // Invalidate 必须先于释放。顺序：
        // 1. CompleteFrames(非数值时间) 冲刷在途帧（此时 shared 仍存活，
        //    可能触发的回调安全）；
        // 2. Invalidate：返回后不再有新回调；
        // 3. 锁屏障：等任何正在执行的回调离开临界区，之后不会再有代码
        //    触碰 refcon；
        // 4. CFRelease 会话；随后结构体字段逆序释放（shared 的 Arc 最后）。
        unsafe {
            raw::VTCompressionSessionCompleteFrames(self.session.as_ptr(), CMTIME_INVALID);
            raw::VTCompressionSessionInvalidate(self.session.as_ptr());
            drop(lock(&self.shared.state));
            raw::CFRelease(self.session.as_ptr().cast_const());
        }
    }
}

/// 属性设置结果检查：OSStatus 0 = 成功。
fn check_status(status: OSStatus, label: &str) -> Result<(), String> {
    if status == 0 {
        return Ok(());
    }
    let hint = if status == K_VT_PROPERTY_NOT_SUPPORTED_ERR {
        "（属性不被当前编码器支持）"
    } else {
        ""
    };
    Err(format!("设置 {label} 失败: OSStatus {status}{hint}"))
}

/// 构造编码器规格字典：`RequireHardwareAcceleratedVideoEncoder = true` +
/// `EnableLowLatencyRateControl = true`。
///
/// 显式要求硬件编码器（传 null 规格时 VT 只是「优先」硬件，个别系统状态
/// 下仍可能静默落回软件编码器——落回时延迟特征完全不同），并开启
/// VideoToolbox 低延迟码控模式（Apple `VTCompressionProperties.h` 官方
/// 超低延迟 4 件套的第 1 项，必须在 `VTCompressionSessionCreate` 的
/// `encoderSpecification` 中传入）。常量缺失（安装损坏）返回 `Err`。
fn encoder_specification_dictionary() -> Result<CFRetained<CFDictionary>, String> {
    // SAFETY: 只对 extern static 做一次读取（拿 CFString 引用，不写）；
    // SDK 常量在框架加载时初始化、永不释放，读取无数据竞争。
    let (req_hw_key, low_latency_key) = unsafe {
        (
            raw::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
            raw::kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
        )
    };
    let req_hw_key = req_hw_key.ok_or_else(|| {
        "VideoToolbox SDK 常量缺失: kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder"
            .to_string()
    })?;
    let low_latency_key = low_latency_key.ok_or_else(|| {
        "VideoToolbox SDK 常量缺失: kVTVideoEncoderSpecification_EnableLowLatencyRateControl"
            .to_string()
    })?;
    let value = CFBoolean::new(true);
    let keys = [
        req_hw_key as *const CFString as *const c_void,
        low_latency_key as *const CFString as *const c_void,
    ];
    let values = [
        value as *const CFBoolean as *const c_void,
        value as *const CFBoolean as *const c_void,
    ];
    // SAFETY: keys/values 指向栈上数组的 2 个合法指针；key 为 SDK 常量
    // CFString、value 为不可变单例 CFBoolean，CF 类型回调（retain/release）
    // 对二者均正确，条目数与数组长度一致。
    let dict = unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    dict.ok_or_else(|| {
        "创建编码器规格字典（RequireHardware + EnableLowLatencyRateControl）失败".to_string()
    })
}

/// 构造一次性 `kVTEncodeFrameOptionKey_ForceKeyFrame = true` 帧选项字典。
fn force_keyframe_dictionary() -> Result<CFRetained<CFDictionary>, EncodeError> {
    let key = VtKey::ForceKeyFrame.get().map_err(EncodeError::Failed)?;
    let value = CFBoolean::new(true);
    let keys = [key as *const CFString as *const c_void];
    let values = [value as *const CFBoolean as *const c_void];
    // SAFETY: keys/values 指向栈上数组的 1 个合法指针；key 为 SDK 常量
    // CFString、value 为不可变单例 CFBoolean，CF 类型回调（retain/release）
    // 对二者均正确。
    let dict = unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    dict.ok_or_else(|| EncodeError::Failed("创建 ForceKeyFrame 帧选项字典失败".to_string()))
}
// ─────────────────────────────────────────────────────────────────────────────
// E6 解码验证：VTDecompressionSession 把一帧 Annex-B 码流真解码回来
// ─────────────────────────────────────────────────────────────────────────────

/// 一次解码的结果状态（解码邮箱）。回调线程写入，验证线程消费。
enum DecodeState {
    /// 尚无输出。
    Pending,
    /// 回调已到达：OSStatus +（有输出帧时）在回调内直接读出的尺寸。
    Done(OSStatus, Option<(u32, u32)>),
}

/// 解码回调与验证线程共享的邮箱。
struct DecodeShared {
    state: Mutex<DecodeState>,
    cond: Condvar,
}

// SAFETY: 与压缩邮箱（Shared）同理：全部可变访问都在 state 这个 Mutex 里，
// 邮箱载荷是纯数据（OSStatus + 尺寸），无跨线程共享的 CF 对象。
unsafe impl Send for DecodeShared {}
// SAFETY: 同上——共享前提是所有访问都在 Mutex 内。
unsafe impl Sync for DecodeShared {}

/// VT 解码输出回调（VideoToolbox 内部队列上执行）：只读取结果并投递邮箱，
/// 绝不 panic（跨 C 边界 unwind 是未定义行为）。
///
/// 刻意**不 retain** 解码输出缓冲：它属于解码会话的内部对象池，读取尺寸
/// 这类同步只读操作在回调内完成，解码器拥有的对象绝不越过回调边界——
/// 验证语义（解码成功 + 尺寸一致）不受影响。
unsafe extern "C" fn vt_decode_callback(
    decompression_output_refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: *mut CVPixelBuffer,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    // SAFETY: refcon 是 Create 时传入的 `Arc::as_ptr(&shared)`，指向
    // DecodeSessionGuard.shared 的堆内层分配。存活期由 guard 的 Drop 顺序
    // 保证：WaitForAsynchronousFrames + Invalidate（不再有新回调）+ 锁屏障
    // 都先于 Arc 释放，因此这里的解引用不可能悬垂。
    let shared = unsafe { &*(decompression_output_refcon as *const DecodeShared) };
    let state = match NonNull::new(image_buffer) {
        // SAFETY: 回调契约保证 image_buffer 在回调期间有效；as_ref 借用
        // 不延长寿命，两个 getter 都是同步只读调用。
        Some(buf) => {
            // SAFETY: 见上——指针来自回调契约，借用只在本块内存活。
            let buffer = unsafe { buf.as_ref() };
            DecodeState::Done(
                status,
                Some((
                    CVPixelBufferGetWidth(buffer) as u32,
                    CVPixelBufferGetHeight(buffer) as u32,
                )),
            )
        }
        None => DecodeState::Done(status, None),
    };
    let mut guard = lock(&shared.state);
    *guard = state;
    drop(guard);
    shared.cond.notify_all();
}

/// 拥有 `VTDecompressionSession`（RAII：Drop 时等异步帧 → Invalidate →
/// 锁屏障 → CFRelease，与 [`CompressionSession`] 的回收顺序同理）。
struct DecodeSessionGuard {
    /// +1 的会话引用（VTDecompressionSessionCreate 返回）。
    session: NonNull<c_void>,
    /// 回调邮箱（refcon 指向其堆分配）。
    shared: Arc<DecodeShared>,
}

impl Drop for DecodeSessionGuard {
    fn drop(&mut self) {
        // SAFETY: session 是 Create 返回的 +1 引用。顺序：
        // 1. WaitForAsynchronousFrames：等在途回调执行完；
        // 2. Invalidate：返回后不再有新回调；
        // 3. 锁屏障：等正在执行的回调离开临界区；
        // 4. CFRelease 会话，随后字段逆序释放（shared 的 Arc 最后）。
        unsafe {
            raw::VTDecompressionSessionWaitForAsynchronousFrames(self.session.as_ptr());
            raw::VTDecompressionSessionInvalidate(self.session.as_ptr());
            drop(lock(&self.shared.state));
            raw::CFRelease(self.session.as_ptr().cast_const());
        }
    }
}

/// E6 解码验证：把一帧 Annex-B H.264 码流交给 `VTDecompressionSession`
/// **真解码**，验证「解码成功 + 输出尺寸与编码配置一致」。
///
/// 这是契约 §7.2 E6「编一帧假数据并解码验证」的后半句：结构校验
///（Annex-B 可解析、含 SPS/PPS、是 IDR）证明码流形态对，本函数证明
/// VideoToolbox 自己的解码器能把这帧解回来——花屏类故障（参数集与 slice
/// 不匹配、码流损坏）在构造期暴露，而不是上线后由用户眼睛发现。
///
/// 步骤：提取 SPS/PPS 建 `CMVideoFormatDescription` → Annex-B 重排回
/// AVCC → `CMBlockBuffer` → `CMSampleBuffer` → 解码 + 有界等待回调 →
/// 校验输出尺寸。
pub(crate) fn decode_verify_annexb(
    data: &[u8],
    expected_width: u32,
    expected_height: u32,
    wait: Duration,
) -> Result<(), String> {
    // 1. 提取参数集（SPS=7 / PPS=8），建格式描述。
    let parameter_sets: Vec<&[u8]> = annexb::nal_payloads(data)
        .into_iter()
        .filter(|(kind, _)| *kind == 7 || *kind == 8)
        .map(|(_, payload)| payload)
        .collect();
    if parameter_sets.is_empty() {
        return Err("自检码流里找不到 SPS/PPS（nal_unit_type 7/8）".to_string());
    }
    let mut pointers: Vec<NonNull<u8>> = parameter_sets
        .iter()
        .map(|payload| NonNull::from(*payload).cast::<u8>())
        .collect();
    let sizes: Vec<usize> = parameter_sets.iter().map(|payload| payload.len()).collect();
    let mut desc_out: *const CMFormatDescription = std::ptr::null();
    // SAFETY: pointers/sizes 指向栈上 Vec（调用期间存活），参数集切片由
    // parameter_sets 保活；desc_out 是本栈帧的合法出参。nal 头长度 4 =
    // 我们生成的 AVCC 长度前缀宽度，两者必须一致。
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            pointers.len(),
            NonNull::from(pointers.as_mut_slice()).cast(),
            NonNull::from(sizes.as_slice()).cast(),
            4,
            NonNull::from(&mut desc_out),
        )
    };
    if status != 0 {
        return Err(format!(
            "CMVideoFormatDescriptionCreateFromH264ParameterSets 失败: OSStatus {status}"
        ));
    }
    // SAFETY: Create 成功（status==0）时 desc_out 是 +1 所有权引用
    //（CM_RETURNS_RETAINED_PARAMETER）。
    let desc = NonNull::new(desc_out as *mut CMFormatDescription)
        .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
        .ok_or_else(|| {
            "CMVideoFormatDescriptionCreateFromH264ParameterSets 返回 0 但描述为空".to_string()
        })?;

    // 2. Annex-B → AVCC（4 字节长度前缀），包进 CMBlockBuffer。
    // 坑：blockAllocator 传 NULL 是「默认分配器」，CMBlockBuffer 释放时会
    // 用 free() 释放我们传入的内存块（真机上实测为双重释放 → SIGABRT）。
    // 必须传 kCFAllocatorNull（「不释放」语义），数据由 AlwaysCopyDataFlag
    // 拷贝进 CM 自有内存，avcc 的生命周期从此与块缓冲无关。
    let avcc = annexb::to_length_prefixed(data, 4)
        .ok_or_else(|| "自检码流重排回 AVCC 失败".to_string())?;
    // SAFETY: 只读 extern static 常量（框架加载时初始化、永不释放）。
    let null_allocator = unsafe { objc2_core_foundation::kCFAllocatorNull }
        .ok_or_else(|| "kCFAllocatorNull 常量缺失".to_string())?;
    let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
    // SAFETY: memory_block 指向 avcc（同步调用期间存活），Null 分配器承诺
    // CMBlockBuffer 释放时不动它；AlwaysCopyDataFlag 让 CoreMedia 把字节
    // 拷进自有内存；structure_allocator 用系统默认；custom_block_source 为
    // null（无自定义释放回调）；block_out 是本栈帧的合法出参。
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            avcc.as_ptr().cast_mut().cast::<c_void>(),
            avcc.len(),
            Some(null_allocator),
            std::ptr::null(),
            0,
            avcc.len(),
            kCMBlockBufferAlwaysCopyDataFlag,
            NonNull::from(&mut block_out),
        )
    };
    if status != 0 || block_out.is_null() {
        return Err(format!(
            "CMBlockBufferCreateWithMemoryBlock 失败: OSStatus {status}"
        ));
    }
    // SAFETY: 上面已判空；Create 成功时 block_out 是 +1 所有权引用。
    let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };

    // 3. CMSampleBuffer（1 个样本 + 1 条 timing；尺寸表允许缺省）。
    let timing = [CMSampleTimingInfo {
        duration: CMTIME_INVALID,
        presentationTimeStamp: CMTime {
            epoch: 0,
            flags: CMTimeFlags::Valid,
            value: 0,
            timescale: 600,
        },
        decodeTimeStamp: CMTIME_INVALID,
    }];
    let mut sample_out: *mut CMSampleBuffer = std::ptr::null_mut();
    // SAFETY: block/desc 是自有引用（存活于本函数栈）；timing 指向栈数组；
    // sample_size_array 为 null 且对应 entries=0（尺寸未知，对解码合法）；
    // sample_out 是本栈帧的合法出参。
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(&desc),
            1,
            1,
            timing.as_ptr(),
            0,
            std::ptr::null(),
            NonNull::from(&mut sample_out),
        )
    };
    if status != 0 || sample_out.is_null() {
        return Err(format!("CMSampleBufferCreateReady 失败: OSStatus {status}"));
    }
    // SAFETY: 上面已判空；Create 成功时 sample_out 是 +1 所有权引用。
    let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(sample_out)) };

    // 4. 建解码会话（回调邮箱 refcon → Arc 堆地址）。
    let shared = Arc::new(DecodeShared {
        state: Mutex::new(DecodeState::Pending),
        cond: Condvar::new(),
    });
    let callback_record = VtDecompressionOutputCallbackRecord {
        callback: vt_decode_callback,
        refcon: Arc::as_ptr(&shared) as *mut c_void,
    };
    let mut session_out: *mut c_void = std::ptr::null_mut();
    // SAFETY: desc 是刚建的合法视频格式描述（CMFormatDescription 对视频
    // 内容即 CMVideoFormatDescription，指针重解释合法）；decoder_spec/
    // attributes 传 null = 系统默认解码器（硬解优先，软件兜底）；callback
    // record 指向栈上存活的合法结构（Create 按值拷贝其内容）；session_out
    // 是本栈帧合法出参。
    let status = unsafe {
        raw::VTDecompressionSessionCreate(
            std::ptr::null(),
            CFRetained::as_ptr(&desc)
                .as_ptr()
                .cast::<c_void>()
                .cast_const(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::addr_of!(callback_record),
            std::ptr::addr_of_mut!(session_out),
        )
    };
    if status != 0 || session_out.is_null() {
        return Err(format!(
            "VTDecompressionSessionCreate 失败: OSStatus {status}"
        ));
    }
    // SAFETY: 上面已判空；Create 成功时 session_out 是 +1 所有权引用。
    // guard 持有它：本函数任何退出路径（含 Err）都会先等回调、再 Invalidate。
    let guard = DecodeSessionGuard {
        // SAFETY: session_out 判空在先，new_unchecked 的前置条件成立。
        session: unsafe { NonNull::new_unchecked(session_out) },
        shared,
    };

    // 5. 解码并提交（decodeFlags=0：回调恰在本帧到达一次）。
    let mut info_flags: u32 = 0;
    // SAFETY: 会话由 guard 持有；sample 是自有引用；info_flags 是合法
    // 出参；sourceFrameRefcon 传 null（回调里用不上）。
    let status = unsafe {
        raw::VTDecompressionSessionDecodeFrame(
            guard.session.as_ptr(),
            CFRetained::as_ptr(&sample).as_ptr().cast::<c_void>(),
            0,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(info_flags),
        )
    };
    if status != 0 {
        return Err(format!(
            "VTDecompressionSessionDecodeFrame 失败: OSStatus {status}"
        ));
    }

    // 6. 有界等待回调（绝不无限阻塞，与 encode 的等待语义对称）。
    let deadline = Instant::now()
        .checked_add(wait)
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
    let mut state = lock(&guard.shared.state);
    loop {
        match std::mem::replace(&mut *state, DecodeState::Pending) {
            DecodeState::Done(status, Some((width, height))) => {
                drop(state);
                if status != 0 {
                    return Err(format!("解码回调携带错误状态: OSStatus {status}"));
                }
                // 解码成功：输出必须是期望尺寸的视频缓冲（宽度/高度与
                // 编码配置一致——参数集与 slice 不匹配的码流在这里现形）。
                if width != expected_width || height != expected_height {
                    return Err(format!(
                        "解码输出 {width}x{height} != 编码配置 {expected_width}x{expected_height}（码流参数集与配置不符）"
                    ));
                }
                return Ok(());
            }
            DecodeState::Done(status, None) => {
                drop(state);
                return Err(format!(
                    "解码回调没有交出输出帧（成功但不产帧是编码输出损坏的形态）: OSStatus {status}"
                ));
            }
            DecodeState::Pending => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Err("等待解码输出超时（回调未到达）".to_string());
        }
        let (next_state, _) = guard
            .shared
            .cond
            .wait_timeout(state, deadline - now)
            .unwrap_or_else(|e| e.into_inner());
        state = next_state;
    }
}

/// 真机延迟诊断（**仅测试构建**，不进生产）：把一次 [`CompressionSession::encode`]
/// 按时刻戳分解为三段——submit→callback（VideoToolbox 内部）、callback→
/// waiter 唤醒（线程切换/锁传递）、唤醒→返回（提取 + Annex-B 重排）。
///
/// 单帧在途 + 一进一出（E3/E4）保证四个数组按帧号对齐；E6 自检帧是第 0
/// 条，消费侧跳过。回调线程只 push（锁中毒容忍，绝不 panic）。
#[cfg(test)]
pub(crate) mod latency_diag {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    /// 一段诊断记录（四个同长数组，下标即帧号）。
    #[derive(Default)]
    pub(crate) struct Marks {
        /// `VTCompressionSessionEncodeFrame` 返回（提交完成）时刻。
        pub submitted: Vec<Instant>,
        /// 压缩输出回调入口时刻。
        pub callback: Vec<Instant>,
        /// 等待线程在邮箱里看到 Ready（唤醒）时刻。
        pub woken: Vec<Instant>,
        /// extract 完成、encode 即将返回时刻。
        pub returned: Vec<Instant>,
    }

    /// 全局记录表（一次测试进程一份）。
    fn marks() -> &'static Mutex<Marks> {
        static MARKS: OnceLock<Mutex<Marks>> = OnceLock::new();
        MARKS.get_or_init(Mutex::default)
    }

    fn push(slot: fn(&mut Marks) -> &mut Vec<Instant>) {
        // 回调线程绝不能 panic：中毒取内部数据继续。
        let mut guard = marks().lock().unwrap_or_else(|e| e.into_inner());
        slot(&mut guard).push(Instant::now());
    }

    /// 记录提交完成（EncodeFrame 返回）。
    pub(crate) fn mark_submitted() {
        push(|m| &mut m.submitted);
    }

    /// 记录压缩输出回调入口。
    pub(crate) fn mark_callback() {
        push(|m| &mut m.callback);
    }

    /// 记录等待线程唤醒（看到 Ready）。
    pub(crate) fn mark_woken() {
        push(|m| &mut m.woken);
    }

    /// 记录提取完成（encode 即将返回）。
    pub(crate) fn mark_returned() {
        push(|m| &mut m.returned);
    }

    /// 取走全部记录（测试侧消费一次；之后记录表为空）。
    pub(crate) fn take() -> Marks {
        std::mem::take(&mut *marks().lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gop_len_zero_maps_to_i32_max_and_unlimited_duration() {
        assert_eq!(
            vt_keyframe_interval_params(0),
            VtKeyframeInterval {
                max_interval_frames: i32::MAX,
                max_interval_duration_secs: 0.0,
            }
        );
    }

    #[test]
    fn gop_len_regular_values_pass_through() {
        for gop in [1u32, 30, 60, 120, 300, i32::MAX as u32] {
            assert_eq!(
                vt_keyframe_interval_params(gop),
                VtKeyframeInterval {
                    max_interval_frames: gop as i32,
                    max_interval_duration_secs: 0.0,
                },
                "常规 gop_len={gop} 应保持原值"
            );
        }
    }

    #[test]
    fn gop_len_above_i32_max_clamps_without_wrapping() {
        let just_over = (i32::MAX as u32) + 1;
        for gop in [just_over, u32::MAX] {
            let mapped = vt_keyframe_interval_params(gop);
            assert_eq!(
                mapped,
                VtKeyframeInterval {
                    max_interval_frames: i32::MAX,
                    max_interval_duration_secs: 0.0,
                },
                "超出 i32::MAX 的 gop_len={gop} 必须饱和钳制到 i32::MAX，不能回绕成负数"
            );
            assert!(mapped.max_interval_frames > 0);
        }
    }
}
