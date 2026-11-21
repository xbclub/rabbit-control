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

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFType};
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
    // CGWindow.h：`bool CGPreflightScreenCaptureAccess(void)`（10.15+）。
    // 与 core-graphics crate 链接同一个 CoreGraphics 框架。
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        pub fn CGPreflightScreenCaptureAccess() -> bool;
        pub fn CGRequestScreenCaptureAccess() -> bool;
    }

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
    // 不把 2x 分辨率喂给编码器（带宽翻倍）。
    // SAFETY: 纯属性 setter。
    unsafe {
        config.setWidth(target.width as usize);
        config.setHeight(target.height as usize);
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
