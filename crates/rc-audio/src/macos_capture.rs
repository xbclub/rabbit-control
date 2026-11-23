//! macOS 系统音频独立采集后端（基于 ScreenCaptureKit）。
//!
//! 设计要点：
//! - 独立构建 `SCStream` 并配置 `capturesAudio = true`、`excludesCurrentProcessAudio = true`、
//!   48 kHz / 2 声道；视频配置压到最小（2×2、1 fps、无光标）以避免浪费 GPU；
//! - 在 `SCStreamOutput` 回调中仅处理 `SCStreamOutputType::Audio`，通过 CoreMedia
//!   `CMAudioFormatDescriptionGetStreamBasicDescription` 与
//!   `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` 提取 PCM（同时支持
//!   ScreenCaptureKit 默认的非交错 Float32，以及交错 Float32 / Int16）；
//! - 统一转换为交错立体声 `i16`，按严格 20 ms（`960 * 2 = 1920` 采样）切块后写入
//!   有界丢旧队列（上限 8 帧 = 160 ms），保证采集端延迟有上界且永不阻塞回调队列；
//! - `stop()` 与 `Drop` 均干净停止 `SCStream` 并按安全顺序释放 ObjC 对象。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::codec::{DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, FRAME_DURATION_MS};
use crate::{AudioCapture, PcmFrame};

/// 采集帧缓冲队列上限：8 × 20ms = 160ms，超限时丢弃最旧帧。
const MAX_QUEUED_FRAMES: usize = 8;

/// 允许的最低音频采样率（8 kHz）。
const MIN_SAMPLE_RATE: u32 = 8_000;

/// 允许的最高音频采样率（192 kHz）。
const MAX_SAMPLE_RATE: u32 = 192_000;

/// CoreAudio `kAudioFormatLinearPCM` (`'lpcm'`)。
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

/// `kLinearPCMFormatFlagIsFloat`。
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;

/// `kAudioFormatFlagIsBigEndian`。
const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;

/// `kLinearPCMFormatFlagIsSignedInteger`。
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;

/// `kLinearPCMFormatFlagIsNonInterleaved`。
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// `kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment`。
const K_CM_SAMPLE_BUFFER_FLAG_ASSURE_16_BYTE_ALIGNMENT: u32 = 1 << 0;

mod raw {
    use std::ffi::c_void;

    pub type OSStatus = i32;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct AudioStreamBasicDescription {
        pub m_sample_rate: f64,
        pub m_format_id: u32,
        pub m_format_flags: u32,
        pub m_bytes_per_packet: u32,
        pub m_frames_per_packet: u32,
        pub m_bytes_per_frame: u32,
        pub m_channels_per_frame: u32,
        pub m_bits_per_channel: u32,
        pub m_reserved: u32,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct AudioBuffer {
        pub m_number_channels: u32,
        pub m_data_byte_size: u32,
        pub m_data: *mut c_void,
    }

    #[repr(C)]
    pub struct AudioBufferList {
        pub m_number_buffers: u32,
        pub m_buffers: [AudioBuffer; 1],
    }

    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        pub fn CMSampleBufferGetFormatDescription(sbuf: *const c_void) -> *const c_void;

        pub fn CMAudioFormatDescriptionGetStreamBasicDescription(
            desc: *const c_void,
        ) -> *const AudioStreamBasicDescription;

        pub fn CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sbuf: *const c_void,
            buffer_list_size_needed_out: *mut usize,
            buffer_list_out: *mut AudioBufferList,
            buffer_list_size: usize,
            block_buffer_structure_allocator: *const c_void,
            block_buffer_block_allocator: *const c_void,
            flags: u32,
            block_buffer_out: *mut *const c_void,
        ) -> OSStatus;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        pub fn CFRelease(cf: *const c_void);
    }
}

/// CoreFoundation +1 拥有型对象守卫，离开作用域自动 `CFRelease`。
struct RetainedCf(*const c_void);

impl Drop for RetainedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` 是由 CoreMedia Create/Retain 规则返回的非空 CF 对象，
            // 且只在本守卫 Drop 时释放一次。
            unsafe { raw::CFRelease(self.0) };
        }
    }
}

/// PCM 缓冲区格式描述符（用于纯 Rust 转换与单元测试）。
#[derive(Debug, Clone, Copy)]
struct PcmFormatSpec {
    sample_rate: u32,
    is_float: bool,
    is_signed_int: bool,
    is_non_interleaved: bool,
    is_big_endian: bool,
    channels: u32,
    bits_per_channel: u32,
}

impl PcmFormatSpec {
    fn from_asbd(asbd: &raw::AudioStreamBasicDescription) -> Option<Self> {
        if asbd.m_format_id != K_AUDIO_FORMAT_LINEAR_PCM || asbd.m_channels_per_frame == 0 {
            return None;
        }
        let rounded_rate = asbd.m_sample_rate.round();
        let sample_rate = if rounded_rate.is_finite()
            && rounded_rate >= f64::from(MIN_SAMPLE_RATE)
            && rounded_rate <= f64::from(MAX_SAMPLE_RATE)
        {
            rounded_rate as u32
        } else {
            DEFAULT_SAMPLE_RATE
        };
        Some(Self {
            sample_rate,
            is_float: (asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_FLOAT) != 0,
            is_signed_int: (asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER) != 0,
            is_non_interleaved: (asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED) != 0,
            is_big_endian: (asbd.m_format_flags & K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN) != 0,
            channels: asbd.m_channels_per_frame,
            bits_per_channel: asbd.m_bits_per_channel,
        })
    }
}

#[inline]
fn f32_to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped >= 1.0 {
        i16::MAX
    } else if clamped <= -1.0 {
        i16::MIN
    } else {
        (clamped * 32_767.0).round() as i16
    }
}

#[inline]
fn read_f32(bytes: [u8; 4], big_endian: bool) -> f32 {
    if big_endian {
        f32::from_be_bytes(bytes)
    } else {
        f32::from_ne_bytes(bytes)
    }
}

#[inline]
fn read_i16(bytes: [u8; 2], big_endian: bool) -> i16 {
    if big_endian {
        i16::from_be_bytes(bytes)
    } else {
        i16::from_ne_bytes(bytes)
    }
}

/// 将 CoreAudio `AudioBufferList` 的字节切片转换为交错立体声 `i16`。
///
/// 支持：
/// - 非交错 Float32（ScreenCaptureKit 默认输出格式）
/// - 交错 Float32（单声道或双声道/多声道取前 2 声道）
/// - 非交错 / 交错 16-bit 有符号整型 PCM
fn extract_stereo_i16_from_buffers(buffers: &[&[u8]], spec: PcmFormatSpec, out: &mut Vec<i16>) {
    if buffers.is_empty() {
        return;
    }

    if spec.is_float && spec.bits_per_channel == 32 {
        if spec.is_non_interleaved {
            if buffers.len() >= 2 {
                let left = buffers[0];
                let right = buffers[1];
                let frames = (left.len() / 4).min(right.len() / 4);
                out.reserve(frames * 2);
                for (l_bytes, r_bytes) in
                    left.chunks_exact(4).zip(right.chunks_exact(4)).take(frames)
                {
                    let l = f32_to_i16(read_f32(
                        [l_bytes[0], l_bytes[1], l_bytes[2], l_bytes[3]],
                        spec.is_big_endian,
                    ));
                    let r = f32_to_i16(read_f32(
                        [r_bytes[0], r_bytes[1], r_bytes[2], r_bytes[3]],
                        spec.is_big_endian,
                    ));
                    out.push(l);
                    out.push(r);
                }
            } else {
                // 单声道非交错 Float32：复制到双声道。
                let mono = buffers[0];
                out.reserve((mono.len() / 4) * 2);
                for b in mono.chunks_exact(4) {
                    let s = f32_to_i16(read_f32([b[0], b[1], b[2], b[3]], spec.is_big_endian));
                    out.push(s);
                    out.push(s);
                }
            }
        } else {
            let data = buffers[0];
            let ch = spec.channels as usize;
            let frame_bytes = ch * 4;
            if frame_bytes == 0 {
                return;
            }
            let frames = data.len() / frame_bytes;
            out.reserve(frames * 2);
            for frame in data.chunks_exact(frame_bytes) {
                let l = f32_to_i16(read_f32(
                    [frame[0], frame[1], frame[2], frame[3]],
                    spec.is_big_endian,
                ));
                let r = if ch >= 2 {
                    f32_to_i16(read_f32(
                        [frame[4], frame[5], frame[6], frame[7]],
                        spec.is_big_endian,
                    ))
                } else {
                    l
                };
                out.push(l);
                out.push(r);
            }
        }
    } else if !spec.is_float && spec.is_signed_int && spec.bits_per_channel == 16 {
        if spec.is_non_interleaved {
            if buffers.len() >= 2 {
                let left = buffers[0];
                let right = buffers[1];
                let frames = (left.len() / 2).min(right.len() / 2);
                out.reserve(frames * 2);
                for (l_bytes, r_bytes) in
                    left.chunks_exact(2).zip(right.chunks_exact(2)).take(frames)
                {
                    out.push(read_i16([l_bytes[0], l_bytes[1]], spec.is_big_endian));
                    out.push(read_i16([r_bytes[0], r_bytes[1]], spec.is_big_endian));
                }
            } else {
                let mono = buffers[0];
                out.reserve(mono.len());
                for b in mono.chunks_exact(2) {
                    let s = read_i16([b[0], b[1]], spec.is_big_endian);
                    out.push(s);
                    out.push(s);
                }
            }
        } else {
            let data = buffers[0];
            let ch = spec.channels as usize;
            let frame_bytes = ch * 2;
            if frame_bytes == 0 {
                return;
            }
            let frames = data.len() / frame_bytes;
            out.reserve(frames * 2);
            for frame in data.chunks_exact(frame_bytes) {
                let l = read_i16([frame[0], frame[1]], spec.is_big_endian);
                let r = if ch >= 2 {
                    read_i16([frame[2], frame[3]], spec.is_big_endian)
                } else {
                    l
                };
                out.push(l);
                out.push(r);
            }
        }
    }
}

/// 20ms 切块器 + 有界丢旧帧队列（上限 `MAX_QUEUED_FRAMES` = 8 帧 = 160ms）。
struct AudioChunker {
    pending: Vec<i16>,
    frames: VecDeque<PcmFrame>,
    sample_rate: u32,
    channels: u8,
    emitted_frames: u64,
    stopped: bool,
}

impl AudioChunker {
    fn new(sample_rate: u32, channels: u8) -> Self {
        let safe_rate = if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            sample_rate
        } else {
            DEFAULT_SAMPLE_RATE
        };
        let safe_channels = if channels == 1 { 1 } else { 2 };
        let chunk_len = ((safe_rate / 50).max(1) as usize) * usize::from(safe_channels);
        Self {
            pending: Vec::with_capacity(chunk_len * 2),
            frames: VecDeque::with_capacity(MAX_QUEUED_FRAMES),
            sample_rate: safe_rate,
            channels: safe_channels,
            emitted_frames: 0,
            stopped: false,
        }
    }

    fn chunk_samples(&self) -> usize {
        let per_channel = (self.sample_rate / 50).max(1) as usize;
        per_channel * usize::from(self.channels)
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        let valid_rate = if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate) {
            sample_rate
        } else {
            DEFAULT_SAMPLE_RATE
        };
        if self.sample_rate != valid_rate {
            self.sample_rate = valid_rate;
            // 采样率切换时清空尚未凑齐 20ms 的旧采样率残片，避免混合两种采样率。
            self.pending.clear();
        }
    }

    fn push_samples_with_rate(&mut self, sample_rate: u32, samples: &[i16]) {
        self.set_sample_rate(sample_rate);
        self.push_samples(samples);
    }

    fn push_samples(&mut self, samples: &[i16]) {
        if self.stopped || samples.is_empty() {
            return;
        }
        let chunk_samples = self.chunk_samples();
        self.pending.extend_from_slice(samples);

        while self.pending.len() >= chunk_samples {
            let chunk: Vec<i16> = self.pending.drain(..chunk_samples).collect();
            let timestamp_us =
                self.emitted_frames
                    .wrapping_mul(u64::from(FRAME_DURATION_MS) * 1_000) as u32;
            self.emitted_frames = self.emitted_frames.wrapping_add(1);

            if self.frames.len() >= MAX_QUEUED_FRAMES {
                self.frames.pop_front();
            }
            self.frames.push_back(PcmFrame {
                sample_rate: self.sample_rate,
                channels: self.channels,
                timestamp_us,
                samples: chunk,
            });
        }
    }

    fn pop_frame(&mut self) -> Option<PcmFrame> {
        self.frames.pop_front()
    }

    fn mark_stopped(&mut self) {
        self.stopped = true;
        self.pending.clear();
        self.frames.clear();
    }
}

/// 从包含有效 `AudioBufferList` 的对齐存储切片中安全提取 `&[raw::AudioBuffer]`。
///
/// 通过字节偏移从整块 `abl_storage` 派生指针，保留完整切片的 Stacked Borrows provenance，
/// 避免直接从 `addr_of!((*abl_ptr).m_buffers)` 构造切片时将 provenance 缩窄为 `[AudioBuffer; 1]`。
///
/// # Safety
/// `abl_storage` 前部必须已完整初始化为一个合法的 `raw::AudioBufferList` 及紧随其后的
/// `m_number_buffers` 个 `raw::AudioBuffer`。
unsafe fn audio_buffers_from_storage(abl_storage: &[usize]) -> Option<&[raw::AudioBuffer]> {
    let storage_bytes = abl_storage
        .len()
        .checked_mul(std::mem::size_of::<usize>())?;
    if storage_bytes < std::mem::size_of::<raw::AudioBufferList>() {
        return None;
    }
    let base_ptr = abl_storage.as_ptr().cast::<u8>();
    let abl_ptr = base_ptr.cast::<raw::AudioBufferList>();
    // SAFETY: `storage_bytes >= size_of::<AudioBufferList>()` 且 `usize` 切片满足
    // `align_of::<AudioBufferList>()`，调用方保证头字段已初始化。
    let num_buffers = unsafe { ptr::addr_of!((*abl_ptr).m_number_buffers).read() } as usize;
    // 仅以整数差值计算 `m_buffers` 相对字节偏移，不沿窄字段 provenance 访问后续元素（MSRV 1.75 兼容）。
    // SAFETY: `abl_ptr` 指向合法的 `AudioBufferList` 布局，`m_buffers` 位于同一分配内。
    let buffers_offset =
        unsafe { (ptr::addr_of!((*abl_ptr).m_buffers) as usize).checked_sub(abl_ptr as usize)? };
    let elem_bytes = num_buffers.checked_mul(std::mem::size_of::<raw::AudioBuffer>())?;
    let required_bytes = buffers_offset.checked_add(elem_bytes)?;
    if required_bytes > storage_bytes {
        return None;
    }
    // SAFETY: `buffers_offset <= storage_bytes` 且满足 `AudioBuffer` 对齐，
    // 由完整 `abl_storage` provenance 的 `base_ptr` 按字节偏移派生。
    let first_buf_ptr = unsafe { base_ptr.add(buffers_offset).cast::<raw::AudioBuffer>() };
    // SAFETY: 已校验 `buffers_offset + num_buffers * size_of::<AudioBuffer>() <= storage_bytes`，
    // 且 `abl_storage` 在返回切片生命周期内保持有效与已初始化。
    Some(unsafe { std::slice::from_raw_parts(first_buf_ptr, num_buffers) })
}

/// 从 `CMSampleBuffer` 提取实际采样率与交错立体声 `i16` 采样。
fn decode_sample_buffer_to_stereo_i16(sample_buffer: &CMSampleBuffer) -> (u32, Vec<i16>) {
    let sbuf_ptr = ptr::addr_of!(*sample_buffer).cast::<c_void>();

    // SAFETY: `sbuf_ptr` 指向回调期间有效的 `CMSampleBuffer`；返回值为借用引用，无需 CFRelease。
    let fmt_desc = unsafe { raw::CMSampleBufferGetFormatDescription(sbuf_ptr) };
    if fmt_desc.is_null() {
        return (DEFAULT_SAMPLE_RATE, Vec::new());
    }
    // SAFETY: `fmt_desc` 非空且在 `sample_buffer` 存活期内有效。
    let asbd_ptr = unsafe { raw::CMAudioFormatDescriptionGetStreamBasicDescription(fmt_desc) };
    if asbd_ptr.is_null() {
        return (DEFAULT_SAMPLE_RATE, Vec::new());
    }
    // SAFETY: `asbd_ptr` 指向 CoreMedia 管理的只读 `AudioStreamBasicDescription`。
    let asbd = unsafe { *asbd_ptr };
    let Some(spec) = PcmFormatSpec::from_asbd(&asbd) else {
        return (DEFAULT_SAMPLE_RATE, Vec::new());
    };

    let mut size_needed: usize = 0;
    // SAFETY: 首次调用传 null 查询 `AudioBufferList` 所需字节数。
    let size_status = unsafe {
        raw::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sbuf_ptr,
            &mut size_needed,
            ptr::null_mut(),
            0,
            ptr::null(),
            ptr::null(),
            0,
            ptr::null_mut(),
        )
    };
    if size_status != 0 || size_needed < std::mem::size_of::<raw::AudioBufferList>() {
        return (spec.sample_rate, Vec::new());
    }

    // 用 `Vec<usize>` 保证至少指针自然对齐（>= align_of::<AudioBufferList>()）。
    let word_size = std::mem::size_of::<usize>();
    let word_count = size_needed.div_ceil(word_size);
    let mut abl_storage = vec![0usize; word_count];
    let alloc_bytes = word_count * word_size;
    let abl_ptr = abl_storage.as_mut_ptr().cast::<raw::AudioBufferList>();

    let mut block_buffer_raw: *const c_void = ptr::null();
    // SAFETY: `abl_ptr` 指向已分配且按 `usize` 对齐的 `alloc_bytes >= size_needed` 字节缓冲区；
    // `block_buffer_raw` 接收 +1 retain 的 `CMBlockBufferRef`，随后立刻交由 `RetainedCf` 管理。
    let get_status = unsafe {
        raw::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sbuf_ptr,
            ptr::null_mut(),
            abl_ptr,
            alloc_bytes,
            ptr::null(),
            ptr::null(),
            K_CM_SAMPLE_BUFFER_FLAG_ASSURE_16_BYTE_ALIGNMENT,
            &mut block_buffer_raw,
        )
    };
    let _block_guard = RetainedCf(block_buffer_raw);
    if get_status != 0 {
        return (spec.sample_rate, Vec::new());
    }

    // SAFETY: `CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer` 返回 0 表示
    // `abl_storage` 已被有效填充为 `AudioBufferList` 及其全部 `AudioBuffer` 条目。
    let Some(audio_buffers) = (unsafe { audio_buffers_from_storage(&abl_storage) }) else {
        return (spec.sample_rate, Vec::new());
    };

    let mut slices: Vec<&[u8]> = Vec::with_capacity(audio_buffers.len());
    for ab in audio_buffers {
        if !ab.m_data.is_null() && ab.m_data_byte_size > 0 {
            // SAFETY: `ab.m_data` 指向由 `_block_guard` 持有保活的 `CMBlockBuffer` 内部只读内存，
            // 长度为 `ab.m_data_byte_size` 字节，在本作用域结束前有效。
            let slice = unsafe {
                std::slice::from_raw_parts(ab.m_data.cast::<u8>(), ab.m_data_byte_size as usize)
            };
            slices.push(slice);
        }
    }

    let mut stereo_i16 = Vec::new();
    extract_stereo_i16_from_buffers(&slices, spec, &mut stereo_i16);
    (spec.sample_rate, stereo_i16)
}

struct AudioHandlerIvars {
    chunker: Arc<Mutex<AudioChunker>>,
}

define_class!(
    // SAFETY:
    // - 超类 NSObject 无额外子类化约束；
    // - 本类不实现自定义 Drop；
    // - ivars 为 `Arc<Mutex<AudioChunker>>`，线程安全，在串行 audio sample queue
    //   与 delegate queue 上仅经由 Mutex 访问。
    #[unsafe(super(NSObject))]
    #[name = "RcAudioSckStreamHandler"]
    #[ivars = AudioHandlerIvars]
    struct RcAudioSckStreamHandler;

    unsafe impl NSObjectProtocol for RcAudioSckStreamHandler {}

    unsafe impl SCStreamOutput for RcAudioSckStreamHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_didOutputSampleBuffer_ofType(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Audio {
                return;
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let (sample_rate, pcm) = decode_sample_buffer_to_stereo_i16(sample_buffer);
                if !pcm.is_empty() {
                    let mut guard = self
                        .ivars()
                        .chunker
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    guard.push_samples_with_rate(sample_rate, &pcm);
                }
            }));
        }
    }

    unsafe impl SCStreamDelegate for RcAudioSckStreamHandler {
        #[allow(non_snake_case)]
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_didStopWithError(&self, _stream: &SCStream, _error: &NSError) {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut guard = self
                    .ivars()
                    .chunker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.mark_stopped();
            }));
        }
    }
);

impl RcAudioSckStreamHandler {
    fn new(chunker: Arc<Mutex<AudioChunker>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioHandlerIvars { chunker });
        // SAFETY: `this` 是新分配的本类实例，调用超类 `init` 完成初始化并返回 +1 `Retained`。
        unsafe { msg_send![super(this), init] }
    }
}

fn nserror_message(err: &NSError) -> String {
    err.localizedDescription().to_string()
}

fn fetch_first_display(timeout: Duration) -> Result<Retained<SCDisplay>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let block = block2::RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = if !error.is_null() {
                // SAFETY: 回调期间 `error` 有效，只做同步只读访问。
                Err(nserror_message(unsafe { &*error }))
            } else {
                match NonNull::new(content) {
                    Some(ptr) => {
                        // SAFETY: 回调期间指针有效；`Retained::retain` 升级为 +1 强引用。
                        unsafe { Retained::retain(ptr.as_ptr()) }
                            .ok_or_else(|| "retain SCShareableContent failed".to_string())
                    }
                    None => Err("SCShareableContent returned null".to_string()),
                }
            };
            let _ = tx.send(result);
        },
    );

    // SAFETY: completion handler block 签名与 ObjC 方法声明匹配。
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true,
            false,
            &block,
        );
    }

    let content = rx
        .recv_timeout(timeout)
        .map_err(|_| "timed out fetching SCShareableContent for audio capture".to_string())??;

    // SAFETY: `displays` 为纯属性 getter。
    let displays = unsafe { content.displays() };
    displays
        .to_vec()
        .into_iter()
        .next()
        .ok_or_else(|| "no shareable display found for ScreenCaptureKit audio capture".to_string())
}

/// 持有 `SCStream` 及其回调 handler / dispatch queue（字段顺序保证 stream 先于 handler 释放）。
struct AudioStreamHandles {
    stream: Retained<SCStream>,
    _handler: Retained<RcAudioSckStreamHandler>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
    stopped: bool,
}

// SAFETY: `AudioStreamHandles` 随 `MacOsAudioCapture` 唯一所有权跨线程移动；
// 对外方法均经 `&mut self` 串行调用，ObjC release 线程安全。
unsafe impl Send for AudioStreamHandles {}

impl AudioStreamHandles {
    fn stop_stream(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // SAFETY: `stopCaptureWithCompletionHandler(None)` 为官方合法的无回调停止调用。
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

impl Drop for AudioStreamHandles {
    fn drop(&mut self) {
        self.stop_stream();
    }
}

/// 基于 macOS ScreenCaptureKit 的系统音频采集器（立体声，20 ms 帧）。
pub(crate) struct MacOsAudioCapture {
    handles: AudioStreamHandles,
    chunker: Arc<Mutex<AudioChunker>>,
    channels: u8,
}

impl MacOsAudioCapture {
    /// 启动一条独立的 ScreenCaptureKit 系统音频采集流。
    pub(crate) fn new() -> Result<Self, String> {
        let display = fetch_first_display(Duration::from_secs(3))?;
        let empty_windows: Retained<NSArray<SCWindow>> = NSArray::new();

        // SAFETY: 标准 `alloc` + `initWithDisplay:excludingWindows:` 构造，`display` 与空数组均有效。
        let filter = unsafe {
            let allocated = SCContentFilter::alloc();
            SCContentFilter::initWithDisplay_excludingWindows(allocated, &display, &empty_windows)
        };

        // SAFETY: `SCStreamConfiguration::new` 为无参构造，随后均为纯属性 setter。
        let config = unsafe {
            let cfg = SCStreamConfiguration::new();
            cfg.setCapturesAudio(true);
            cfg.setExcludesCurrentProcessAudio(true);
            cfg.setSampleRate(DEFAULT_SAMPLE_RATE as isize);
            cfg.setChannelCount(isize::from(DEFAULT_CHANNELS));
            // 将不需要的视频面压到最低规格（2x2 @ 1fps），避免消耗 GPU。
            cfg.setWidth(2);
            cfg.setHeight(2);
            cfg.setShowsCursor(false);
            cfg.setQueueDepth(3);
            cfg.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 1,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            cfg
        };

        let chunker = Arc::new(Mutex::new(AudioChunker::new(
            DEFAULT_SAMPLE_RATE,
            DEFAULT_CHANNELS,
        )));
        let handler = RcAudioSckStreamHandler::new(Arc::clone(&chunker));

        // SAFETY: `filter`、`config`、`handler` 有效；`AudioStreamHandles` 持有 `handler`
        // 强引用且在 `stream` 之后释放，满足 delegate 生命周期要求。
        let stream = unsafe {
            let allocated = SCStream::alloc();
            SCStream::initWithFilter_configuration_delegate(
                allocated,
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*handler)),
            )
        };

        let attr = dispatch2::DispatchQueueAttr::with_qos_class(
            dispatch2::DispatchQueueAttr::SERIAL,
            dispatch2::DispatchQoS::UserInteractive,
            0,
        );
        let queue = dispatch2::DispatchQueue::new("rc.audio.sck.capture", Some(&attr));

        // SAFETY: `handler` 实现了 `SCStreamOutput`，仅注册 `SCStreamOutputType::Audio`
        // 到独立的串行高优先级队列。
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*handler),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| format!("addStreamOutput(Audio) failed: {}", nserror_message(&e)))?;
        }

        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let start_block = block2::RcBlock::new(move |error: *mut NSError| {
            let err_msg = if error.is_null() {
                None
            } else {
                // SAFETY: 回调期间 `error` 有效，只做同步只读访问。
                Some(nserror_message(unsafe { &*error }))
            };
            let _ = start_tx.send(err_msg);
        });
        // SAFETY: block 签名与 `startCaptureWithCompletionHandler:` 匹配。
        unsafe { stream.startCaptureWithCompletionHandler(Some(&start_block)) };

        match start_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => {}
            Ok(Some(msg)) => {
                return Err(format!("SCStream audio startCapture failed: {msg}"));
            }
            Err(_) => {
                // SAFETY: 启动等待超时后流可能仍在后台启动/运行，释放前必须显式停止以避免泄漏。
                unsafe { stream.stopCaptureWithCompletionHandler(None) };
                return Err("timed out waiting for SCStream audio startCapture".to_string());
            }
        }

        Ok(Self {
            handles: AudioStreamHandles {
                stream,
                _handler: handler,
                _queue: queue,
                stopped: false,
            },
            chunker,
            channels: DEFAULT_CHANNELS,
        })
    }
}

impl AudioCapture for MacOsAudioCapture {
    fn try_recv_frame(&mut self) -> Option<PcmFrame> {
        let mut guard = self.chunker.lock().unwrap_or_else(|e| e.into_inner());
        guard.pop_frame()
    }

    fn sample_rate(&self) -> u32 {
        self.chunker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn stop(&mut self) {
        self.handles.stop_stream();
        let mut guard = self.chunker.lock().unwrap_or_else(|e| e.into_inner());
        guard.mark_stopped();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::SAMPLES_PER_CHANNEL_20MS;

    const STEREO_CHUNK_SAMPLES_48K: usize = SAMPLES_PER_CHANNEL_20MS * (DEFAULT_CHANNELS as usize);

    #[test]
    fn extracts_non_interleaved_float32_stereo() {
        let left_samples: [f32; 4] = [0.0, 0.5, -0.5, 1.0];
        let right_samples: [f32; 4] = [-1.0, 0.25, f32::NAN, 1.5];
        let mut left_bytes = Vec::new();
        let mut right_bytes = Vec::new();
        for s in left_samples {
            left_bytes.extend_from_slice(&s.to_ne_bytes());
        }
        for s in right_samples {
            right_bytes.extend_from_slice(&s.to_ne_bytes());
        }

        let spec = PcmFormatSpec {
            sample_rate: DEFAULT_SAMPLE_RATE,
            is_float: true,
            is_signed_int: false,
            is_non_interleaved: true,
            is_big_endian: false,
            channels: 2,
            bits_per_channel: 32,
        };
        let mut out = Vec::new();
        extract_stereo_i16_from_buffers(&[&left_bytes, &right_bytes], spec, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], i16::MIN);
        assert!((out[2] - 16_384).abs() <= 1);
        assert!((out[3] - 8_192).abs() <= 1);
        assert!((out[4] + 16_384).abs() <= 1);
        assert_eq!(out[5], 0); // NaN clamped safely to 0
        assert_eq!(out[6], i16::MAX);
        assert_eq!(out[7], i16::MAX); // +1.5 clamped to +1.0 -> i16::MAX
    }

    #[test]
    fn extracts_interleaved_int16_and_mono_float32() {
        let interleaved_i16: [i16; 4] = [100, -200, 300, -400];
        let mut raw_i16 = Vec::new();
        for s in interleaved_i16 {
            raw_i16.extend_from_slice(&s.to_ne_bytes());
        }
        let spec_i16 = PcmFormatSpec {
            sample_rate: DEFAULT_SAMPLE_RATE,
            is_float: false,
            is_signed_int: true,
            is_non_interleaved: false,
            is_big_endian: false,
            channels: 2,
            bits_per_channel: 16,
        };
        let mut out_i16 = Vec::new();
        extract_stereo_i16_from_buffers(&[&raw_i16], spec_i16, &mut out_i16);
        assert_eq!(out_i16, vec![100, -200, 300, -400]);

        // Mono interleaved Float32 upmixes cleanly to stereo.
        let mono_f32: [f32; 2] = [0.5, -0.5];
        let mut raw_f32 = Vec::new();
        for s in mono_f32 {
            raw_f32.extend_from_slice(&s.to_ne_bytes());
        }
        let spec_mono = PcmFormatSpec {
            sample_rate: DEFAULT_SAMPLE_RATE,
            is_float: true,
            is_signed_int: false,
            is_non_interleaved: false,
            is_big_endian: false,
            channels: 1,
            bits_per_channel: 32,
        };
        let mut out_mono = Vec::new();
        extract_stereo_i16_from_buffers(&[&raw_f32], spec_mono, &mut out_mono);
        assert_eq!(out_mono.len(), 4);
        assert_eq!(out_mono[0], out_mono[1]);
        assert_eq!(out_mono[2], out_mono[3]);
        assert!((out_mono[0] - 16_384).abs() <= 1);
        assert!((out_mono[2] + 16_384).abs() <= 1);
    }

    #[test]
    fn chunker_emits_20ms_frames_and_drops_oldest_over_cap() {
        let mut chunker = AudioChunker::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS);
        // 推送 10 帧数据（超出 8 帧上限），前 2 帧应被丢弃，保留最后 8 帧。
        for frame_idx in 0..10i16 {
            let chunk = vec![frame_idx + 1; STEREO_CHUNK_SAMPLES_48K];
            chunker.push_samples(&chunk);
        }

        let mut received = Vec::new();
        while let Some(f) = chunker.pop_frame() {
            received.push(f);
        }
        assert_eq!(received.len(), MAX_QUEUED_FRAMES);
        // 第 0、1 帧被挤出，最早剩下的应该是 frame_idx == 2（采样值 3，时间戳 40_000µs）。
        assert_eq!(received[0].samples.len(), STEREO_CHUNK_SAMPLES_48K);
        assert_eq!(received[0].samples[0], 3);
        assert_eq!(received[0].timestamp_us, 40_000);
        assert_eq!(received[7].samples[0], 10);
        assert_eq!(received[7].timestamp_us, 180_000);
    }

    #[test]
    fn audio_buffer_list_multi_buffer_offset_extraction_with_two_buffers() {
        let mut left_data = [0x11u8, 0x22, 0x33, 0x44];
        let mut right_data = [0x55u8, 0x66, 0x77, 0x88];

        let mut probe_abl = raw::AudioBufferList {
            m_number_buffers: 2,
            m_buffers: [raw::AudioBuffer {
                m_number_channels: 1,
                m_data_byte_size: 4,
                m_data: left_data.as_mut_ptr().cast::<c_void>(),
            }],
        };
        let base_addr = ptr::addr_of!(probe_abl) as usize;
        let buffers_offset = (ptr::addr_of!(probe_abl.m_buffers) as usize) - base_addr;
        let total_bytes = buffers_offset + 2 * std::mem::size_of::<raw::AudioBuffer>();
        let word_size = std::mem::size_of::<usize>();
        let mut storage = vec![0usize; total_bytes.div_ceil(word_size)];

        // SAFETY: `storage` 满足 `AudioBufferList` 与后续 2 个 `AudioBuffer` 的对齐与长度要求，
        // 仅在本测试内按字节偏移写入两个合法条目。
        unsafe {
            let base = storage.as_mut_ptr().cast::<u8>();
            ptr::write(base.cast::<u32>(), 2u32);
            let bufs = base.add(buffers_offset).cast::<raw::AudioBuffer>();
            ptr::write(
                bufs,
                raw::AudioBuffer {
                    m_number_channels: 1,
                    m_data_byte_size: left_data.len() as u32,
                    m_data: left_data.as_mut_ptr().cast::<c_void>(),
                },
            );
            ptr::write(
                bufs.add(1),
                raw::AudioBuffer {
                    m_number_channels: 1,
                    m_data_byte_size: right_data.len() as u32,
                    m_data: right_data.as_mut_ptr().cast::<c_void>(),
                },
            );
        }
        probe_abl.m_number_buffers = 0; // silence unused mut warning if any
        let _ = probe_abl;

        // SAFETY: `storage` 已按 C `AudioBufferList` 布局完整写入 2 个 `AudioBuffer`。
        let extracted =
            unsafe { audio_buffers_from_storage(&storage) }.expect("num_buffers=2 must succeed");
        assert_eq!(extracted.len(), 2);
        assert_eq!(extracted[0].m_number_channels, 1);
        assert_eq!(extracted[0].m_data_byte_size, 4);
        assert_eq!(extracted[0].m_data, left_data.as_mut_ptr().cast::<c_void>());
        assert_eq!(extracted[1].m_number_channels, 1);
        assert_eq!(extracted[1].m_data_byte_size, 4);
        assert_eq!(
            extracted[1].m_data,
            right_data.as_mut_ptr().cast::<c_void>()
        );

        // Out-of-bounds `m_number_buffers` must return None safely.
        // SAFETY: `storage` 头部足够容纳 `u32` 写入，此处故意写入越界计数验证边界检查。
        unsafe {
            ptr::write(storage.as_mut_ptr().cast::<u32>(), 3u32);
        }
        // SAFETY: 头部已初始化为越界计数，helper 须拒绝越界访问并返回 None。
        assert!(unsafe { audio_buffers_from_storage(&storage) }.is_none());
    }

    #[test]
    fn pcm_format_spec_and_chunker_propagate_44100hz_and_fallback_invalid_rates() {
        let make_asbd = |rate: f64| raw::AudioStreamBasicDescription {
            m_sample_rate: rate,
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
            m_bytes_per_packet: 4,
            m_frames_per_packet: 1,
            m_bytes_per_frame: 4,
            m_channels_per_frame: 2,
            m_bits_per_channel: 32,
            m_reserved: 0,
        };

        let spec_44k = PcmFormatSpec::from_asbd(&make_asbd(44_100.0)).expect("valid 44.1 kHz ASBD");
        assert_eq!(spec_44k.sample_rate, 44_100);

        // Non-finite or out-of-range sample rates fall back to DEFAULT_SAMPLE_RATE.
        for bad_rate in [0.0, 4_000.0, 7_999.0, 192_001.0, f64::NAN, f64::INFINITY] {
            let spec = PcmFormatSpec::from_asbd(&make_asbd(bad_rate)).expect("fallback ASBD");
            assert_eq!(
                spec.sample_rate, DEFAULT_SAMPLE_RATE,
                "bad rate {bad_rate:?} must fall back to DEFAULT_SAMPLE_RATE"
            );
        }

        // Propagate 44.1 kHz into AudioChunker: 20ms stereo = 882 samples/ch = 1764 samples total.
        let mut chunker = AudioChunker::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS);
        let samples_44k_20ms = (44_100 / 50) as usize * 2;
        assert_eq!(samples_44k_20ms, 882 * 2);

        // Push 2 full 20ms stereo frames + 100 trailing samples at 44.1 kHz.
        let input = vec![1234i16; samples_44k_20ms * 2 + 100];
        chunker.push_samples_with_rate(spec_44k.sample_rate, &input);

        let f0 = chunker.pop_frame().expect("first 44.1 kHz 20ms frame");
        let f1 = chunker.pop_frame().expect("second 44.1 kHz 20ms frame");
        assert!(chunker.pop_frame().is_none());

        assert_eq!(f0.sample_rate, 44_100);
        assert_eq!(f0.channels, 2);
        assert_eq!(f0.samples.len(), 882 * 2);
        assert_eq!(f0.timestamp_us, 0);

        assert_eq!(f1.sample_rate, 44_100);
        assert_eq!(f1.channels, 2);
        assert_eq!(f1.samples.len(), 882 * 2);
        assert_eq!(f1.timestamp_us, 20_000);
    }
}
