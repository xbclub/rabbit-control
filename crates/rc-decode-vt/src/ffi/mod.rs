//! VideoToolbox 解码侧的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - `VTDecompressionSession` 的 C API（Create / SetProperty / DecodeFrame /
//!   WaitForAsynchronousFrames / Invalidate）在这里声明与包装；
//! - 解码输出回调（`VTDecompressionOutputCallback`）在这里定义：只做
//!   retain + 按帧号投递邮箱，不做重活、绝不 panic；
//! - Annex-B → AVCC 之后的装帧（`CMBlockBuffer` → `CMSampleBuffer`）与
//!   SPS/PPS → `CMVideoFormatDescription` 在等待线程上完成；
//! - 测试专用的像素缓冲写入/回读（LockBaseAddress 只出现在测试支撑里，
//!   生产解码路径**零 CPU 回读**，D1）。
//!
//! 对外（`lib.rs`）只暴露安全的包装类型与函数。
//!
//! # 关键落死项
//! | 项 | 处理 |
//! |---|---|
//! | 会话建立 | **惰性**：首个带 SPS/PPS 的 IDR 到达才建会话（之前无从构建格式描述）；SPS 变化（分辨率切换）→ 重建 |
//! | 输入封装 | AVCC：4 字节大端长度前缀，参数集经格式描述传递（转换在 [`crate::h264`]，纯函数） |
//! | D3 低延迟 | `kVTDecompressionPropertyKey_RealTime = true`（提示解码器按实时优化；仅容忍 `kVTPropertyNotSupportedErr`） |
//! | D5 顺序 | 严格单帧在途 + 帧号邮箱匹配（[`crate::mailbox`]），提交线程等到本帧回调才返回 |
//! | 解码器选择 | `decoder_specification = null`（系统默认：优先硬件、软件兜底）。与编码侧「RequireHardwareAccelerated = true」刻意不同：编码落回软编会拖垮整条链路、必须在构造期失败；解码端硬件 H.264 不可用时软件兜底仍可工作，且解码器选择没有可靠的查询接口，硬性要求只会把「能解」变「不能解」 |
//! | 输出缓冲保活 | 输出 `CVPixelBuffer` 在回调里 retain(+1)，由会话持有到**下一次 decode / 会话销毁**——与 `FrameTexture` 文档的采集侧句柄生命周期语义对称（下一次采集前有效），零拷贝交给渲染器 |
//!
//! # 输入格式说明（坑 #7）
//! VideoToolbox 解码输入是 AVCC 风格（长度前缀 NAL + 参数集在格式描述里）。
//! 管线统一消费 Annex-B（`rc-encode-vt` 输出侧已重排并在 caps 里声明），
//! 因此解码端在喂给 VT 之前由 [`crate::h264`] 做一次 Annex-B → AVCC 转换。
//! 方向与编码侧（`rc-encode-vt/src/ffi` 的 E6 解码验证）相反，装帧代码
//! 结构对称，含同一个 `CMBlockBuffer` 分配器的双重释放坑（见
//! [`DecompressionSession::decode_frame`] 内注释）。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFBoolean, CFRetained, CFString};
use objc2_core_media::{
    kCMBlockBufferAlwaysCopyDataFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

use crate::mailbox::FrameMailbox;

/// `OSStatus`（CFBase.h：`typedef int32_t OSStatus`）。core-media 绑定把它
/// 保持为 crate 私有，这里自行定义同名别名（与 rc-encode-vt 的 ffi 一致）。
pub(crate) type OSStatus = i32;

/// `kVTPropertyNotSupportedErr`（VTErrors.h）：属性被当前解码器拒绝。
const K_VT_PROPERTY_NOT_SUPPORTED_ERR: OSStatus = -12900;

/// `kCMTimeInvalid`：flags 为空（不含 Valid）即非数值时间。
const CMTIME_INVALID: CMTime = CMTime {
    epoch: 0,
    value: 0,
    timescale: 0,
    flags: CMTimeFlags::empty(),
};

// ─────────────────────────────────────────────────────────────────────────────
// 提交线程 QoS（pthread/qos.h，macOS 10.10+，libSystem）
// ─────────────────────────────────────────────────────────────────────────────

/// `QOS_CLASS_USER_INTERACTIVE`（pthread/qos.h：`0x21`）。
const QOS_CLASS_USER_INTERACTIVE: i32 = 0x21;
/// `QOS_CLASS_UNSPECIFIED`（pthread/qos.h：`0x00`）——只由 getter 返回，
/// 不是 set 的合法入参，恢复时映射为语义等价的 `QOS_CLASS_DEFAULT`。
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
/// 与编码侧（rc-encode-vt 的 `QosBump`）同理：VideoToolbox C API 没有回调
/// 队列 QoS 的设置入口，回调调度优先级继承提交线程；解码段的预算只有
/// 3ms，默认 QoS 下系统负载会直接把它吃掉。抬升只覆盖一次
/// [`DecompressionSession::decode_frame`]，任何退出路径都经 Drop 恢复。
/// 任何一步失败都保持现状（QoS 不动，解码照常进行）。
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

// ─────────────────────────────────────────────────────────────────────────────
// VideoToolbox C API 声明（VTDecompressionSession.h / VTSession.h）
// ─────────────────────────────────────────────────────────────────────────────

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
    /// 回调 refcon（指向 `Arc<FrameMailbox<CallbackPayload>>` 的堆分配）。
    refcon: *mut c_void,
}

/// 属性键与函数的裸声明。会话句柄按 C 惯例用 `*mut c_void`（指向不透明结构）。
mod raw {
    use super::{c_void, CFString, OSStatus, VtDecompressionOutputCallbackRecord};

    // SAFETY: 声明与 VideoToolbox 头文件逐字对应；链接 VideoToolbox 框架。
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        /// 属性键（CFStringRef 常量，永不指向无效内存但按 C 头文件是可空指针类型）。
        pub static kVTDecompressionPropertyKey_RealTime: Option<&'static CFString>;

        pub fn VTDecompressionSessionCreate(
            allocator: *const c_void,
            video_format_description: *const c_void,
            decoder_specification: *const c_void,
            image_buffer_attributes: *const c_void,
            output_callback: *const VtDecompressionOutputCallbackRecord,
            session_out: *mut *mut c_void,
        ) -> OSStatus;
        /// 在 VTSession.h；`VTDecompressionSessionRef` 可安全重解释为 `VTSessionRef`。
        pub fn VTSessionSetProperty(
            session: *mut c_void,
            property_key: *const CFString,
            property_value: *const c_void,
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

// ─────────────────────────────────────────────────────────────────────────────
// 解码输出邮箱（回调线程 → 等待线程）
// ─────────────────────────────────────────────────────────────────────────────

/// 一次解码的回调载荷。
struct CallbackPayload {
    /// 回调携带的 OSStatus（0 = 成功）。
    status: OSStatus,
    /// 输出帧（回调里 retain +1 的自有引用）；成功但无输出帧时为 None。
    buffer: Option<CFRetained<CVPixelBuffer>>,
}

// SAFETY: 载荷只在邮箱 Mutex 内移动（回调线程构造 → 等待线程消费，互不
// 并发触碰内容）；CFRetained 的 retain/release 是线程安全的（CF 引用计数
// 全局可用），CVPixelBuffer 本身在解出后不做任何并发可变访问。
unsafe impl Send for CallbackPayload {}

/// VT 解码输出回调。运行在 VideoToolbox 内部队列上：只 retain + 按帧号
/// 投递邮箱，绝不做重活、绝不 panic（跨 C 边界 unwind 是未定义行为）。
unsafe extern "C" fn vt_decode_callback(
    decompression_output_refcon: *mut c_void,
    source_frame_refcon: *mut c_void,
    status: OSStatus,
    _info_flags: u32,
    image_buffer: *mut CVPixelBuffer,
    _presentation_time_stamp: CMTime,
    _presentation_duration: CMTime,
) {
    // SAFETY: refcon 是 Create 时传入的 `Arc::as_ptr(&mailbox)`，指向
    // DecompressionSession.mailbox 的堆内层分配。存活期由 Drop 顺序保证：
    // WaitForAsynchronousFrames（等在途回调）+ Invalidate（不再有新回调）
    // + 邮箱锁屏障（等正在执行的回调离开临界区）都先于 Arc 释放，
    // 因此这里的解引用不可能悬垂。
    let mailbox =
        unsafe { &*(decompression_output_refcon as *const FrameMailbox<CallbackPayload>) };
    // sourceFrameRefcon 原样带回提交时的帧号（lib.rs 保证单调唯一）。
    let frame_id = source_frame_refcon as usize as u64;
    // SAFETY: NonNull::new 只做空指针检查；闭包里的 retain 依据回调契约——
    // image_buffer 在回调期间有效，retain(+1) 升级为自有引用，随邮箱状态
    // 移动/释放。
    let buffer = NonNull::new(image_buffer).map(|buf| unsafe { CFRetained::retain(buf) });
    mailbox.post(frame_id, CallbackPayload { status, buffer });
}

// ─────────────────────────────────────────────────────────────────────────────
// 对 lib.rs 暴露的类型
// ─────────────────────────────────────────────────────────────────────────────

/// 一次成功解码的输出（零拷贝句柄，D1）。
pub(crate) struct DecodeOutput {
    /// 输出像素缓冲的不透明句柄（与 `FrameTexture::CVPixelBuffer` 同形态）。
    /// 存活期见 [`DecompressionSession`] 的 last_output 注释：下一次
    /// decode_frame / 会话销毁之前有效。
    pub ptr: *mut c_void,
    /// 输出宽度（像素，取自 CVPixelBuffer——VideoToolbox 是权威）。
    pub width: u32,
    /// 输出高度（像素）。
    pub height: u32,
}

/// 解码失败原因（lib.rs 映射到 rc_types::Error）。
pub(crate) enum DecodeFailure {
    /// 提交后等到预算尽头回调仍未到达（E 系超时语义的解码侧对应）。
    Timeout,
    /// 回调携带错误状态，或成功但没有交出输出帧（参考链/码流问题）。
    Callback(String),
    /// 提交或装帧失败（会话/样本层面）。
    Failed(String),
}

impl fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeFailure::Timeout => write!(f, "等待解码输出超时（回调未到达）"),
            DecodeFailure::Callback(msg) => write!(f, "{msg}"),
            DecodeFailure::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

/// 一个配置好的 `VTDecompressionSession`。
///
/// `Send`：随 `VtDecoder`（`Decoder: Send`）跨线程**移交所有权**；所有
/// 访问都发生在持有 `&mut self` 的单一线程上，回调线程只经邮箱
/// （已声明 Send + Sync）触碰数据。
pub(crate) struct DecompressionSession {
    /// +1 的会话引用（VTDecompressionSessionCreate 返回）。Drop 前必须先
    /// Invalidate（Apple 文档要求），见 [`DecompressionSession::drop`]。
    session: NonNull<c_void>,
    /// 会话的格式描述（SPS/PPS 构建时的 +1 引用）。每个样本都引用它；
    /// CMSampleBuffer 会自行 retain，这里是会话级的兜底所有权。
    _format_description: CFRetained<CMFormatDescription>,
    /// 回调邮箱（refcon 指向其堆内层分配）。必须排在 session 之后：
    /// 字段按声明顺序释放，Arc 在 Drop 手动 CFRelease(session) 与锁屏障
    /// 之后才 drop。
    mailbox: Arc<FrameMailbox<CallbackPayload>>,
    /// 上一帧输出缓冲的保活引用：零拷贝句柄（[`DecodeOutput::ptr`]）交出去
    /// 之后由这里 hold 引用计数，直到**下一次 decode_frame 替换或会话销毁**
    /// ——与 `FrameTexture` 文档「GPU 句柄在下一次采集前有效」的语义对称。
    /// 调用方（渲染器）要跨帧持有必须自行 retain。
    last_output: Option<CFRetained<CVPixelBuffer>>,
}

// SAFETY: 见类型文档：所有权随 VtDecoder 移动，无并发访问；回调线程只经
// FrameMailbox（已声明 Send + Sync）触碰数据；CF 对象的 retain/release
// 本身线程安全。
unsafe impl Send for DecompressionSession {}

impl DecompressionSession {
    /// 从参数集（SPS/PPS 的 NAL 载荷，含 NAL 头字节）建格式描述并创建
    /// 解码会话，设置 RealTime（D3）。
    ///
    /// 任何一步失败都返回 `Err`（错误经构造路径上报为 `Error::Decoder`）。
    pub(crate) fn new(sps: &[u8], pps: &[u8]) -> Result<Self, String> {
        // 1. SPS/PPS → CMVideoFormatDescription（AVCC 长度前缀宽度 = 4，
        //    与装帧侧 to_avcc_without_parameter_sets 的 4 字节前缀一致）。
        let pointers = [
            NonNull::from(sps).cast::<u8>(),
            NonNull::from(pps).cast::<u8>(),
        ];
        let sizes = [sps.len(), pps.len()];
        let mut desc_out: *const CMFormatDescription = std::ptr::null();
        // SAFETY: pointers/sizes 指向栈上数组（调用期间存活），参数集切片由
        // 调用方保活到 Create 同步返回；desc_out 是本栈帧的合法出参。
        // nal 头长度 4 必须与装帧侧的 AVCC 前缀宽度一致（坑 #7）。
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                2,
                NonNull::from(pointers.as_slice()).cast(),
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

        // 2. 建解码会话。decoder_specification / image_buffer_attributes 传
        //    null：系统默认解码器（优先硬件、软件兜底，见模块文档的选择理由）；
        //    输出格式由解码器原生决定（NV12），对零拷贝渲染最友好。
        let mailbox = Arc::new(FrameMailbox::new());
        let callback_record = VtDecompressionOutputCallbackRecord {
            callback: vt_decode_callback,
            refcon: Arc::as_ptr(&mailbox) as *mut c_void,
        };
        let mut session_out: *mut c_void = std::ptr::null_mut();
        // SAFETY: desc 是刚建的合法视频格式描述；两个可空参数传 null = 系统
        // 默认；callback record 指向栈上存活的合法结构（Create 按值拷贝其
        // 内容）；session_out 是本栈帧合法出参；refcon 指向 mailbox 的稳定
        // 堆分配（由会话持有 Arc 克隆保活，见 Drop 顺序）。
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
        let session = unsafe { NonNull::new_unchecked(session_out) };

        let created = Self {
            session,
            _format_description: desc,
            mailbox,
            last_output: None,
        };

        // 3. D3 低延迟：RealTime = true（提示解码器为实时优化，可牺牲质量
        //    保速度）。个别解码器不支持该属性（-12900）——结构上本项目解码
        //    恒为单帧在途同步等待，不依赖该属性兜底，因此仅容忍这一种状态码。
        // SAFETY: 只对 extern static 做一次读取（拿 CFString 引用，不写）；
        // SDK 常量在框架加载时初始化、永不释放，读取无数据竞争。
        let realtime_key =
            unsafe { raw::kVTDecompressionPropertyKey_RealTime }.ok_or_else(|| {
                "VideoToolbox SDK 常量缺失: kVTDecompressionPropertyKey_RealTime".to_string()
            })?;
        // CFBoolean::new 返回的是不可变单例（kCFBooleanTrue）的静态引用。
        let value = CFBoolean::new(true);
        // SAFETY: session 是本包装拥有的合法会话；键是 SDK 常量 CFString；
        // value 是不可变全局单例（'static，永不释放）；VTSessionSetProperty
        // 同步读取。
        let status = unsafe {
            raw::VTSessionSetProperty(
                created.session.as_ptr(),
                realtime_key,
                value as *const CFBoolean as *const c_void,
            )
        };
        if status != 0 && status != K_VT_PROPERTY_NOT_SUPPORTED_ERR {
            return Err(format!(
                "设置 kVTDecompressionPropertyKey_RealTime (D3) 失败: OSStatus {status}"
            ));
        }
        Ok(created)
    }

    /// 解码一帧：AVCC 装帧 → 提交（带帧号）→ 邮箱上等到**该帧**的回调。
    ///
    /// - 输入是 4 字节长度前缀的 AVCC（参数集已剥离，转换在纯逻辑层完成）；
    /// - `frame_id`：单调帧号，经 `sourceFrameRefcon` 原样带回回调，
    ///   邮箱按号取件（D5：严格单帧在途 + 号匹配 → 输出顺序 == 输入顺序）；
    /// - `wait`：墙钟等待上限（由注入时间域的 deadline 换算，E 系超时语义），
    ///   到点回调未到达返回 [`DecodeFailure::Timeout`]（迟到回调的输出留在
    ///   邮箱里，下一次调用开头被 clear 丢弃——与编码侧「迟到输出丢弃」对称）；
    /// - 输出零拷贝：不 LockBaseAddress、不做任何 CPU 回读（D1）。
    pub(crate) fn decode_frame(
        &mut self,
        avcc: &[u8],
        frame_id: u64,
        wait: Duration,
    ) -> Result<DecodeOutput, DecodeFailure> {
        // 解码提交/等待按 USER_INTERACTIVE 调度（回调队列 QoS 继承提交线程，
        // 见 QosBump 文档）；guard 在任何退出路径恢复原值。
        let _qos = QosBump::new();
        // 清掉上一帧的迟到残留（超时被弃的帧不复活）。
        self.mailbox.clear();

        // 1. AVCC → CMBlockBuffer。
        // 坑（与编码侧 E6 相同）：blockAllocator 传 NULL 是「默认分配器」，
        // CMBlockBuffer 释放时会用 free() 释放我们传入的内存块（真机上实测
        // 为双重释放 → SIGABRT）。必须传 kCFAllocatorNull（「不释放」语义），
        // 数据由 AlwaysCopyDataFlag 拷贝进 CM 自有内存，avcc 的生命周期从此
        // 与块缓冲无关。
        // SAFETY: 只读 extern static 常量（框架加载时初始化、永不释放）。
        let null_allocator = unsafe { objc2_core_foundation::kCFAllocatorNull }
            .ok_or_else(|| DecodeFailure::Failed("kCFAllocatorNull 常量缺失".to_string()))?;
        let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: memory_block 指向 avcc（同步调用期间存活），Null 分配器承诺
        // CMBlockBuffer 释放时不动它；AlwaysCopyDataFlag 让 CoreMedia 把字节
        // 拷进自有内存；structure_allocator 用系统默认；custom_block_source
        // 为 null（无自定义释放回调）；block_out 是本栈帧的合法出参。
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
            return Err(DecodeFailure::Failed(format!(
                "CMBlockBufferCreateWithMemoryBlock 失败: OSStatus {status}"
            )));
        }
        // SAFETY: 上面已判空；Create 成功时 block_out 是 +1 所有权引用。
        let block = unsafe { CFRetained::from_raw(NonNull::new_unchecked(block_out)) };

        // 2. CMSampleBuffer（1 个样本 + 1 条 timing；尺寸表允许缺省）。
        //    PTS 严格单调（帧号 × 10 / 600），DTS 无效——无 B 帧流不需要。
        let timing = [CMSampleTimingInfo {
            duration: CMTIME_INVALID,
            presentationTimeStamp: CMTime {
                epoch: 0,
                flags: CMTimeFlags::Valid,
                value: (frame_id as i64).saturating_mul(10),
                timescale: 600,
            },
            decodeTimeStamp: CMTIME_INVALID,
        }];
        let mut sample_out: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: block 是自有引用；desc 经格式描述在会话内保活（CMSampleBuffer
        // 亦自行 retain）；timing 指向栈数组；sample_size_array 为 null 且对应
        // entries=0（尺寸未知，对解码合法）；sample_out 是本栈帧的合法出参。
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self._format_description),
                1,
                1,
                timing.as_ptr(),
                0,
                std::ptr::null(),
                NonNull::from(&mut sample_out),
            )
        };
        if status != 0 || sample_out.is_null() {
            return Err(DecodeFailure::Failed(format!(
                "CMSampleBufferCreateReady 失败: OSStatus {status}"
            )));
        }
        // SAFETY: 上面已判空；Create 成功时 sample_out 是 +1 所有权引用。
        let sample = unsafe { CFRetained::from_raw(NonNull::new_unchecked(sample_out)) };

        // 3. 提交（decodeFlags = 0：回调恰在本帧到达一次；refcon = 帧号）。
        let mut info_flags: u32 = 0;
        // SAFETY: 会话由本包装拥有；sample 是自有引用；info_flags 是合法出参；
        // refcon 只是一个透传的整数帧号（回调里重解释回 u64）。
        let status = unsafe {
            raw::VTDecompressionSessionDecodeFrame(
                self.session.as_ptr(),
                CFRetained::as_ptr(&sample).as_ptr().cast::<c_void>(),
                0,
                frame_id as usize as *mut c_void,
                std::ptr::addr_of_mut!(info_flags),
            )
        };
        if status != 0 {
            return Err(DecodeFailure::Failed(format!(
                "VTDecompressionSessionDecodeFrame 失败: OSStatus {status}"
            )));
        }

        // 4. 邮箱上等本帧回调（有界等待，绝不无限阻塞）。
        let deadline = Instant::now()
            .checked_add(wait)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(1));
        let Some(payload) = self.mailbox.take(frame_id, deadline) else {
            return Err(DecodeFailure::Timeout);
        };
        if payload.status != 0 {
            return Err(DecodeFailure::Callback(format!(
                "解码回调携带错误状态: OSStatus {}（帧 {}）",
                payload.status, frame_id
            )));
        }
        // SAFETY: 回调里 retain 的 +1 自有引用，随邮箱移动到这里； getWidth/
        // getHeight 是同步只读的安全包装（直接调用，无需 unsafe）。
        let Some(buffer) = payload.buffer else {
            return Err(DecodeFailure::Callback(format!(
                "解码回调成功但没有交出输出帧（帧 {frame_id}，码流可能损坏）"
            )));
        };
        let width = CVPixelBufferGetWidth(&buffer) as u32;
        let height = CVPixelBufferGetHeight(&buffer) as u32;
        // SAFETY: as_ptr 借用不转移所有权；句柄交给调用方，存活期由
        // last_output 的引用计数保证（见字段注释）。
        let ptr = CFRetained::as_ptr(&buffer).as_ptr().cast::<c_void>();
        self.last_output = Some(buffer);
        Ok(DecodeOutput { ptr, width, height })
    }
}

impl Drop for DecompressionSession {
    fn drop(&mut self) {
        // SAFETY: session 是 Create 返回的 +1 引用。顺序：
        // 1. WaitForAsynchronousFrames：等在途回调执行完；
        // 2. Invalidate：返回后不再有新回调；
        // 3. 邮箱锁屏障：等正在执行的回调离开临界区，之后不会再有代码
        //    触碰 refcon；
        // 4. CFRelease 会话；随后字段按声明顺序释放（mailbox 的 Arc 最后）。
        unsafe {
            raw::VTDecompressionSessionWaitForAsynchronousFrames(self.session.as_ptr());
            raw::VTDecompressionSessionInvalidate(self.session.as_ptr());
            self.mailbox.clear();
            raw::CFRelease(self.session.as_ptr().cast_const());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试支撑（仅测试构建）：像素缓冲创建 / 写图案 / 一次性 CPU 回读。
// 生产解码路径绝不 LockBaseAddress（D1 零拷贝）；CPU 访问只发生在测试里
//（任务卡允许「测试内 CPU 回读一次」）。
// ─────────────────────────────────────────────────────────────────────────────

/// 测试支撑子模块：真机测试用它创建像素缓冲、写入 PRNG 图案、做一次性
/// CPU 回读。整个模块只在 `cfg(test)` 下存在，不进生产二进制。
#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::c_void;
    use std::ptr::NonNull;

    use objc2_core_foundation::CFRetained;
    use objc2_core_video::{
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferCreate,
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
        CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };

    /// `'420v'`（NV12 video range）——与采集/编码侧约定一致的输入格式（M5）。
    pub(crate) const PIXEL_FORMAT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

    /// 拥有型 CVPixelBuffer（`CVPixelBufferCreate` +1），供真机测试。
    pub(crate) struct TestPixelBuffer(CFRetained<CVPixelBuffer>);

    impl TestPixelBuffer {
        /// 不透明句柄（与 SCK 交付给 `FrameTexture::CVPixelBuffer` 的形态一致）。
        pub(crate) fn as_opaque_ptr(&self) -> *mut c_void {
            CFRetained::as_ptr(&self.0).as_ptr().cast()
        }
    }

    /// 创建一块 '420v' 像素缓冲（内容未定义，写图案前不要喂给编码器）。
    pub(crate) fn make_pixel_buffer(width: u32, height: u32) -> Result<TestPixelBuffer, String> {
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: out 是合法出参（addr_of_mut 规避 &mut 别名）；分配器与属性
        // 传 None 合法（系统默认分配器、无附加属性）。
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

    /// 把测试图案写入 '420v' 缓冲的 Y/C 平面，返回写入的 Y 平面**密集副本**
    /// （无行距填充，供 PSNR 对比）。
    ///
    /// 图案 = 平滑双梯度 + PRNG 生成的实心矩形（逐帧平移）——结构化、可压缩、
    /// 对预测编码友好，同时把参考链真正用起来。亮度值域夹在 [64, 200]，
    /// 避开 video/full range 两端的重映射区。
    pub(crate) fn write_nv12_pattern(
        buffer: &TestPixelBuffer,
        frame: u32,
        seed: u64,
    ) -> Result<Vec<u8>, String> {
        let width = CVPixelBufferGetWidth(&buffer.0) as u32;
        let height = CVPixelBufferGetHeight(&buffer.0) as u32;
        let rects = pattern_rects(seed, width, height);

        // SAFETY: Lock(读写) 与 Unlock 配对；两个 getter 是同步只读安全包装。
        let lock =
            unsafe { CVPixelBufferLockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        if lock != 0 {
            return Err(format!(
                "CVPixelBufferLockBaseAddress 失败: CVReturn {lock}"
            ));
        }
        let result = (|| {
            let base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 0);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 0);
            if base.is_null() || stride < width as usize {
                return Err("Y 平面基址为空或行距小于宽度".to_string());
            }
            let mut dense = vec![0u8; (width as usize) * (height as usize)];
            // SAFETY: 锁存期间 base 指向 Y 平面可写内存（stride * height 字节），
            // 逐行写入宽度个字节，不越界。
            unsafe {
                let plane =
                    std::slice::from_raw_parts_mut(base as *mut u8, stride * height as usize);
                for y in 0..height as usize {
                    for x in 0..width as usize {
                        let v = pattern_luma(x as u32, y as u32, frame, &rects);
                        plane[y * stride + x] = v;
                        dense[y * width as usize + x] = v;
                    }
                }
            }
            // C 平面（NV12 交织 CbCr）填中性值 128/128。
            let chroma_base = CVPixelBufferGetBaseAddressOfPlane(&buffer.0, 1);
            let chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer.0, 1);
            if !chroma_base.is_null() && chroma_stride > 0 {
                // SAFETY: 同上——锁存期间 CbCr 平面可写；高度为 Y 的一半。
                unsafe {
                    let plane = std::slice::from_raw_parts_mut(
                        chroma_base as *mut u8,
                        chroma_stride * (height as usize / 2).max(1),
                    );
                    for row in plane.chunks_mut(chroma_stride) {
                        row.fill(128);
                    }
                }
            }
            Ok(dense)
        })();
        // SAFETY: 与上面的 Lock 配对（任何退出路径都解锁）。
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer.0, CVPixelBufferLockFlags::empty()) };
        result
    }

    /// 一次性 CPU 回读指定像素缓冲的 Y 平面（**仅测试**，D1 红线不适用于此）。
    /// 返回 `(width, height, 密集 Y 平面)`。
    pub(crate) fn read_luma_plane(ptr: *mut c_void) -> Result<(u32, u32, Vec<u8>), String> {
        if ptr.is_null() {
            return Err("像素缓冲句柄为空".to_string());
        }
        // SAFETY: 调用方（真机测试）在解码器的保活窗口内（下一次 decode 前）
        // 立即回读，句柄有效；借用只在本函数内存活。
        let buffer = unsafe { &*(ptr as *const CVPixelBuffer) };
        let width = CVPixelBufferGetWidth(buffer) as u32;
        let height = CVPixelBufferGetHeight(buffer) as u32;
        // SAFETY: 只读 Lock 与 Unlock 配对。
        let lock = unsafe { CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
        if lock != 0 {
            return Err(format!(
                "CVPixelBufferLockBaseAddress(读) 失败: CVReturn {lock}"
            ));
        }
        let result = (|| {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, 0);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            if base.is_null() || stride < width as usize {
                return Err("Y 平面基址为空或行距小于宽度".to_string());
            }
            let mut dense = vec![0u8; (width as usize) * (height as usize)];
            // SAFETY: 锁存期间 base 指向 Y 平面可读内存；逐行拷贝宽度个字节。
            unsafe {
                let plane = std::slice::from_raw_parts(base as *const u8, stride * height as usize);
                for y in 0..height as usize {
                    dense[y * width as usize..(y + 1) * width as usize]
                        .copy_from_slice(&plane[y * stride..y * stride + width as usize]);
                }
            }
            Ok((width, height, dense))
        })();
        // SAFETY: 与上面的 Lock 配对。
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::empty()) };
        result
    }

    // —— 图案生成（确定性 PRNG，与像素格式无关的纯函数）——

    /// xorshift64 PRNG（种子非零）。
    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    /// 一个实心矩形（左上角 + 宽高 + 亮度）。
    struct PatternRect {
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        shade: u8,
    }

    /// 从种子生成 6 个互不重叠区间的矩形（位置留在边界内，留出逐帧平移余量）。
    fn pattern_rects(seed: u64, width: u32, height: u32) -> Vec<PatternRect> {
        let mut rng = XorShift64(seed | 1);
        let mut rects = Vec::with_capacity(6);
        for _ in 0..6 {
            let w = 160 + rng.below(160) as u32;
            let h = 120 + rng.below(120) as u32;
            // 位置留 64px 边距（逐帧平移 ±52px 不越界）。
            let x = 64 + rng.below((width.saturating_sub(w + 128)) as u64) as u32;
            let y = 64 + rng.below((height.saturating_sub(h + 128)) as u64) as u32;
            let shade = (90 + rng.below(100)) as u8;
            rects.push(PatternRect { x, y, w, h, shade });
        }
        rects
    }

    /// 图案亮度：平滑梯度打底 + 命中矩形取矩形亮度；逐帧平移（+13, +7 px）。
    fn pattern_luma(x: u32, y: u32, frame: u32, rects: &[PatternRect]) -> u8 {
        let mut v = 64u32 + (x * 96) / 1920 + (y * 64) / 1080; // 梯度 64..224
        for r in rects {
            let rx = r.x + (frame * 13) % 52;
            let ry = r.y + (frame * 7) % 52;
            if x >= rx && x < rx + r.w && y >= ry && y < ry + r.h {
                v = u32::from(r.shade);
            }
        }
        v.min(200) as u8
    }
} // mod test_support
