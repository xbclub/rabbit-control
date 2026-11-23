//! macOS CoreAudio 低延迟播放后端（基于 `AudioToolbox.framework` `AudioQueue` C API）。
//!
//! 设计要点：
//! - 使用 4 个 20ms 缓冲块组成固定缓冲池（总排队上限 80ms，确保低延迟）；
//! - 音频播放回调运行在 AudioQueue 内部线程上，通过 `Arc<Mutex<PoolState>>`
//!   将播放完成的缓冲索引归还给空闲池；
//! - 当 4 个缓冲块全部在途时，`push_frame` 直接丢弃当前帧并立即返回，绝不阻塞调用线程；
//! - `stop()` 与 `Drop` 均同步调用 `AudioQueueStop(q, 1)` + `AudioQueueDispose(q, 1)`，
//!   在确保系统不再触发任何回调之后才回收用户数据 `Arc`。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![deny(missing_docs)]

use std::ffi::c_void;
use std::ptr;
use std::sync::{Arc, Mutex};

use crate::codec::FRAME_DURATION_MS;
use crate::{AudioPlayer, PcmFrame};

/// 缓冲池深度：4 × 20ms = 80ms 延迟上限。
const BUFFER_COUNT: usize = 4;

/// 允许的最低音频采样率（8 kHz）。
const MIN_AUDIO_SAMPLE_RATE: u32 = 8_000;

/// 允许的最高音频采样率（192 kHz）。
const MAX_AUDIO_SAMPLE_RATE: u32 = 192_000;

/// CoreAudio `kAudioFormatLinearPCM` (`'lpcm'`)。
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");

/// CoreAudio `kLinearPCMFormatFlagIsSignedInteger`。
const K_LINEAR_PCM_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;

/// CoreAudio `kLinearPCMFormatFlagIsPacked`。
const K_LINEAR_PCM_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;

/// AudioQueue `kAudioQueueParam_Volume`。
const K_AUDIO_QUEUE_PARAM_VOLUME: u32 = 1;

mod raw {
    use std::ffi::c_void;

    pub type OSStatus = i32;
    pub type AudioQueueRef = *mut c_void;
    pub type AudioQueueBufferRef = *mut AudioQueueBuffer;
    pub type AudioQueueParameterID = u32;
    pub type AudioQueueParameterValue = f32;

    pub type AudioQueueOutputCallback = unsafe extern "C" fn(
        in_user_data: *mut c_void,
        in_aq: AudioQueueRef,
        in_buffer: AudioQueueBufferRef,
    );

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
    pub struct AudioQueueBuffer {
        pub m_audio_data_bytes_capacity: u32,
        pub m_audio_data: *mut c_void,
        pub m_audio_data_byte_size: u32,
        pub m_user_data: *mut c_void,
        pub m_packet_description_capacity: u32,
        pub m_packet_descriptions: *mut c_void,
        pub m_packet_description_count: u32,
    }

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        pub fn AudioQueueNewOutput(
            in_format: *const AudioStreamBasicDescription,
            in_callback_proc: AudioQueueOutputCallback,
            in_user_data: *mut c_void,
            in_callback_run_loop: *mut c_void,
            in_callback_run_loop_mode: *const c_void,
            in_flags: u32,
            out_aq: *mut AudioQueueRef,
        ) -> OSStatus;

        pub fn AudioQueueAllocateBuffer(
            in_aq: AudioQueueRef,
            in_buffer_byte_size: u32,
            out_buffer: *mut AudioQueueBufferRef,
        ) -> OSStatus;

        pub fn AudioQueueEnqueueBuffer(
            in_aq: AudioQueueRef,
            in_buffer: AudioQueueBufferRef,
            in_num_packet_descs: u32,
            in_packet_descs: *const c_void,
        ) -> OSStatus;

        pub fn AudioQueueStart(in_aq: AudioQueueRef, in_start_time: *const c_void) -> OSStatus;

        pub fn AudioQueueReset(in_aq: AudioQueueRef) -> OSStatus;

        pub fn AudioQueueStop(in_aq: AudioQueueRef, in_immediate: u8) -> OSStatus;

        pub fn AudioQueueDispose(in_aq: AudioQueueRef, in_immediate: u8) -> OSStatus;

        pub fn AudioQueueSetParameter(
            in_aq: AudioQueueRef,
            in_param_id: AudioQueueParameterID,
            in_value: AudioQueueParameterValue,
        ) -> OSStatus;
    }
}

/// 回调线程与调用线程共享的缓冲池空闲状态。
struct PoolState {
    /// 可供 `push_frame` 使用的空闲缓冲索引。
    free_indices: Vec<usize>,
    /// 4 个 `AudioQueueBufferRef` 的裸地址（仅用于指针比对以映射回索引）。
    buffer_addrs: [usize; BUFFER_COUNT],
    /// 队列是否正在销毁或已销毁。
    disposed: bool,
}

/// AudioQueue 播放完成回调（运行在 AudioToolbox 内部线程）。
///
/// 绝不 panic，也不获取任何额外所有权：只将已播完的缓冲索引归还到 `free_indices`。
unsafe extern "C" fn audio_queue_output_callback(
    in_user_data: *mut c_void,
    _in_aq: raw::AudioQueueRef,
    in_buffer: raw::AudioQueueBufferRef,
) {
    let _ = std::panic::catch_unwind(|| {
        if in_user_data.is_null() || in_buffer.is_null() {
            return;
        }
        // SAFETY: `in_user_data` 由 `Arc::into_raw` 创建并在 `AudioQueueDispose(q, 1)`
        // 同步返回后才通过 `Arc::from_raw` 释放；AudioQueue 保证 dispose 返回前所有回调已结束，
        // 因此本回调执行期间该 `Mutex<PoolState>` 引用始终有效。
        let pool = unsafe { &*(in_user_data as *const Mutex<PoolState>) };
        let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
        if state.disposed {
            return;
        }
        let target = in_buffer as usize;
        if let Some(idx) = state.buffer_addrs.iter().position(|&addr| addr == target) {
            if !state.free_indices.contains(&idx) {
                state.free_indices.push(idx);
            }
        }
    });
}

/// 基于 macOS `AudioQueue` 的实时 PCM16 音频播放器。
pub(crate) struct MacOsAudioPlayer {
    queue: raw::AudioQueueRef,
    buffers: [raw::AudioQueueBufferRef; BUFFER_COUNT],
    buffer_capacity_bytes: usize,
    sample_rate: u32,
    channels: u8,
    muted: bool,
    started: bool,
    disposed: bool,
    pool: Arc<Mutex<PoolState>>,
    /// 传给 `AudioQueueNewOutput` 的裸 `Arc` 指针，必须在 `AudioQueueDispose` 返回后释放。
    user_data_ptr: *const Mutex<PoolState>,
}

// SAFETY: `MacOsAudioPlayer` 拥有唯一的 `AudioQueueRef` 与缓冲指针所有权；
// 所有外部操作均经由 `&mut self` 串行调用，回调线程仅通过线程安全的 `Arc<Mutex<PoolState>>`
// 同步空闲索引，`Drop` 在最终持有线程同步停止并销毁队列。
unsafe impl Send for MacOsAudioPlayer {}

impl MacOsAudioPlayer {
    /// 创建并初始化 macOS `AudioQueue` 输出队列与 4 × 20ms 缓冲池。
    pub(crate) fn new(sample_rate: u32, channels: u8) -> Result<Self, String> {
        if !(MIN_AUDIO_SAMPLE_RATE..=MAX_AUDIO_SAMPLE_RATE).contains(&sample_rate) {
            return Err(format!(
                "invalid audio sample_rate: {sample_rate} (expected {MIN_AUDIO_SAMPLE_RATE}..={MAX_AUDIO_SAMPLE_RATE})"
            ));
        }
        if channels != 1 && channels != 2 {
            return Err(format!(
                "unsupported audio channels: {channels} (expected 1 or 2)"
            ));
        }

        let bytes_per_sample = std::mem::size_of::<i16>() as u32;
        let bytes_per_frame = bytes_per_sample * u32::from(channels);
        let asbd = raw::AudioStreamBasicDescription {
            m_sample_rate: f64::from(sample_rate),
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_LINEAR_PCM_FORMAT_FLAG_IS_SIGNED_INTEGER
                | K_LINEAR_PCM_FORMAT_FLAG_IS_PACKED,
            m_bytes_per_packet: bytes_per_frame,
            m_frames_per_packet: 1,
            m_bytes_per_frame: bytes_per_frame,
            m_channels_per_frame: u32::from(channels),
            m_bits_per_channel: 16,
            m_reserved: 0,
        };

        let frames_20ms =
            ((u64::from(sample_rate) * u64::from(FRAME_DURATION_MS)) / 1000).max(1) as usize;
        let buffer_capacity_bytes = frames_20ms * bytes_per_frame as usize;
        let buffer_capacity_u32 = u32::try_from(buffer_capacity_bytes)
            .map_err(|_| "audio buffer capacity exceeds u32".to_string())?;

        let pool = Arc::new(Mutex::new(PoolState {
            free_indices: (0..BUFFER_COUNT).collect(),
            buffer_addrs: [0; BUFFER_COUNT],
            disposed: false,
        }));
        let user_data_ptr = Arc::into_raw(Arc::clone(&pool));

        let mut queue: raw::AudioQueueRef = ptr::null_mut();
        // SAFETY: `asbd` 指向有效的 `AudioStreamBasicDescription`；`user_data_ptr` 是有效
        // 存活的 `Arc` 裸指针；`in_callback_run_loop` 传 null 使 AudioQueue 使用内部回调线程；
        // `out_aq` 指向有效的栈输出变量。
        let status = unsafe {
            raw::AudioQueueNewOutput(
                &asbd,
                audio_queue_output_callback,
                user_data_ptr as *mut c_void,
                ptr::null_mut(),
                ptr::null(),
                0,
                &mut queue,
            )
        };
        if status != 0 || queue.is_null() {
            // SAFETY: `AudioQueueNewOutput` 失败，不会有回调触发，立刻回收 `Arc` 裸指针。
            unsafe {
                drop(Arc::from_raw(user_data_ptr));
            }
            return Err(format!("AudioQueueNewOutput failed: OSStatus={status}"));
        }

        let mut buffers: [raw::AudioQueueBufferRef; BUFFER_COUNT] = [ptr::null_mut(); BUFFER_COUNT];
        for slot in &mut buffers {
            let mut buf: raw::AudioQueueBufferRef = ptr::null_mut();
            // SAFETY: `queue` 是刚创建的有效 AudioQueueRef，`out_buffer` 指向栈变量。
            let alloc_status =
                unsafe { raw::AudioQueueAllocateBuffer(queue, buffer_capacity_u32, &mut buf) };
            if alloc_status != 0 || buf.is_null() {
                // SAFETY: 销毁队列并释放其名下所有已分配缓冲；`in_immediate = 1` 同步完成，
                // 随后安全回收 `user_data_ptr`。
                unsafe {
                    let _ = raw::AudioQueueDispose(queue, 1);
                    drop(Arc::from_raw(user_data_ptr));
                }
                return Err(format!(
                    "AudioQueueAllocateBuffer failed: OSStatus={alloc_status}"
                ));
            }
            *slot = buf;
        }

        {
            let mut state = pool.lock().unwrap_or_else(|e| e.into_inner());
            for (idx, &buf) in buffers.iter().enumerate() {
                state.buffer_addrs[idx] = buf as usize;
            }
        }

        Ok(Self {
            queue,
            buffers,
            buffer_capacity_bytes,
            sample_rate,
            channels,
            muted: false,
            started: false,
            disposed: false,
            pool,
            user_data_ptr,
        })
    }

    /// 同步停止并释放底层 AudioQueue（幂等，可由 `stop()` 或 `Drop` 调用）。
    fn dispose_inner(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        {
            let mut state = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            state.disposed = true;
            state.free_indices.clear();
        }
        if !self.queue.is_null() {
            // SAFETY: `self.queue` 有效且只释放一次。`in_immediate = 1` 同步停止并销毁
            // 队列及其所有缓冲，AudioToolbox 保证 `AudioQueueDispose` 返回后不再调用任何回调。
            unsafe {
                let _ = raw::AudioQueueStop(self.queue, 1);
                let _ = raw::AudioQueueDispose(self.queue, 1);
            }
            self.queue = ptr::null_mut();
        }
        if !self.user_data_ptr.is_null() {
            // SAFETY: `AudioQueueDispose(q, 1)` 已同步返回，不会再有回调访问 `user_data_ptr`，
            // 因此可安全回收构造时 `Arc::into_raw` 产生的强引用。
            unsafe {
                drop(Arc::from_raw(self.user_data_ptr));
            }
            self.user_data_ptr = ptr::null();
        }
        self.buffers = [ptr::null_mut(); BUFFER_COUNT];
    }

    /// 弹出一个空闲缓冲索引，若 4 个缓冲全部在途则返回 `None`。
    fn pop_free_buffer(&self) -> Option<usize> {
        let mut state = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        state.free_indices.pop()
    }

    /// 入队失败时归还缓冲索引，避免缓冲池泄漏。
    fn return_free_buffer(&self, idx: usize) {
        let mut state = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if !state.disposed && !state.free_indices.contains(&idx) {
            state.free_indices.push(idx);
        }
    }
}

impl AudioPlayer for MacOsAudioPlayer {
    fn push_frame(&mut self, frame: &PcmFrame) -> Result<(), String> {
        if self.disposed || self.queue.is_null() {
            return Err("audio player has been stopped".to_string());
        }
        if frame.sample_rate != self.sample_rate || frame.channels != self.channels {
            return Err(format!(
                "audio frame format mismatch: expected {}Hz {}ch, got {}Hz {}ch",
                self.sample_rate, self.channels, frame.sample_rate, frame.channels
            ));
        }
        if frame.samples.is_empty() {
            return Ok(());
        }

        let bytes_per_frame = std::mem::size_of::<i16>() * usize::from(self.channels);
        let max_samples_per_buf = self.buffer_capacity_bytes / std::mem::size_of::<i16>();
        let aligned_chunk_samples =
            (max_samples_per_buf / usize::from(self.channels)).max(1) * usize::from(self.channels);

        for chunk in frame.samples.chunks(aligned_chunk_samples) {
            let buf_idx = match self.pop_free_buffer() {
                Some(idx) => idx,
                None => {
                    if !self.started {
                        // SAFETY: `self.queue` 有效，`in_start_time = null` 表示立即开始播放。
                        let start_status = unsafe { raw::AudioQueueStart(self.queue, ptr::null()) };
                        if start_status == 0 {
                            self.started = true;
                            return Ok(());
                        }
                        // 启动失败：立即重置队列，使已入队缓冲归还 `free_indices`，避免永久卡死。
                        // SAFETY: `self.queue` 有效，`AudioQueueReset` 冲刷在途缓冲并经回调归还。
                        unsafe {
                            let _ = raw::AudioQueueReset(self.queue);
                        }
                        return Err(format!("AudioQueueStart failed: OSStatus={start_status}"));
                    }
                    // 全部 4 个缓冲都在播放中：直接丢弃，绝不阻塞调用线程。
                    return Ok(());
                }
            };
            let buf_ptr = self.buffers[buf_idx];
            let byte_len = std::mem::size_of_val(chunk);

            // SAFETY: `buf_idx` 已从 `free_indices` 独占弹出，当前不在 AudioQueue 队列中；
            // `buf_ptr` 有效，检查 `m_audio_data` 非空并按 `bytes_per_frame` 向下对齐后再拷贝，
            // 且 `chunk` 指向有效的本地 `i16` 切片。
            let copy_result: Result<usize, String> = unsafe {
                let buf = &mut *buf_ptr;
                if buf.m_audio_data.is_null() {
                    Err("AudioQueueBuffer m_audio_data is null".to_string())
                } else {
                    let cap = buf.m_audio_data_bytes_capacity as usize;
                    let copy_bytes = (byte_len.min(cap) / bytes_per_frame) * bytes_per_frame;
                    if copy_bytes > 0 {
                        if self.muted {
                            ptr::write_bytes(buf.m_audio_data.cast::<u8>(), 0, copy_bytes);
                        } else {
                            ptr::copy_nonoverlapping(
                                chunk.as_ptr().cast::<u8>(),
                                buf.m_audio_data.cast::<u8>(),
                                copy_bytes,
                            );
                        }
                        buf.m_audio_data_byte_size = copy_bytes as u32;
                    }
                    Ok(copy_bytes)
                }
            };
            match copy_result {
                Err(err) => {
                    self.return_free_buffer(buf_idx);
                    return Err(err);
                }
                Ok(0) => {
                    self.return_free_buffer(buf_idx);
                    return Ok(());
                }
                Ok(_) => {}
            }

            // SAFETY: `self.queue` 与 `buf_ptr` 均有效，线性 PCM 无可变包描述（传 0 / null）。
            let enqueue_status =
                unsafe { raw::AudioQueueEnqueueBuffer(self.queue, buf_ptr, 0, ptr::null()) };
            if enqueue_status != 0 {
                self.return_free_buffer(buf_idx);
                return Err(format!(
                    "AudioQueueEnqueueBuffer failed: OSStatus={enqueue_status}"
                ));
            }

            if !self.started {
                // SAFETY: `self.queue` 有效，`in_start_time = null` 表示立即开始播放。
                let start_status = unsafe { raw::AudioQueueStart(self.queue, ptr::null()) };
                if start_status != 0 {
                    // 启动失败：立即重置队列，将已入队缓冲归还 `free_indices`，避免后续陷入静默 `Ok(())`。
                    // SAFETY: `self.queue` 有效，`AudioQueueReset` 冲刷在途缓冲并经回调归还。
                    unsafe {
                        let _ = raw::AudioQueueReset(self.queue);
                    }
                    return Err(format!("AudioQueueStart failed: OSStatus={start_status}"));
                }
                self.started = true;
            }
        }

        Ok(())
    }

    fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        if !self.disposed && !self.queue.is_null() {
            let volume = if muted { 0.0_f32 } else { 1.0_f32 };
            // SAFETY: `self.queue` 有效，`K_AUDIO_QUEUE_PARAM_VOLUME` 为标准音量参数（0.0..=1.0）。
            unsafe {
                let _ = raw::AudioQueueSetParameter(self.queue, K_AUDIO_QUEUE_PARAM_VOLUME, volume);
            }
        }
    }

    fn is_muted(&self) -> bool {
        self.muted
    }

    fn stop(&mut self) {
        self.dispose_inner();
    }
}

impl Drop for MacOsAudioPlayer {
    fn drop(&mut self) {
        self.dispose_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, SAMPLES_PER_CHANNEL_20MS};

    #[test]
    fn rejects_invalid_player_parameters() {
        assert!(MacOsAudioPlayer::new(0, 2).is_err());
        assert!(MacOsAudioPlayer::new(4_000, 2).is_err());
        assert!(MacOsAudioPlayer::new(MIN_AUDIO_SAMPLE_RATE - 1, 2).is_err());
        assert!(MacOsAudioPlayer::new(MAX_AUDIO_SAMPLE_RATE + 1, 2).is_err());
        assert!(MacOsAudioPlayer::new(u32::MAX, 2).is_err());
        assert!(MacOsAudioPlayer::new(48_000, 0).is_err());
        assert!(MacOsAudioPlayer::new(48_000, 3).is_err());
    }

    #[test]
    fn audio_queue_player_push_silence_mute_and_clean_drop() {
        // 在无音频硬件的极简 CI 环境下，AudioQueueNewOutput / AudioQueueStart 可能返回错误；
        // 有音频输出设备的 macOS 主机上会完整验证建队、推送静音帧、静音切换、满池不阻塞与干净 Drop。
        let Ok(mut player) = MacOsAudioPlayer::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS) else {
            return;
        };
        assert!(!player.is_muted());
        player.set_muted(true);
        assert!(player.is_muted());

        let silence = PcmFrame {
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            timestamp_us: 0,
            samples: vec![0i16; SAMPLES_PER_CHANNEL_20MS * usize::from(DEFAULT_CHANNELS)],
        };

        // 连续推送 6 帧（超过 4 缓冲池深度），验证满池丢弃不阻塞。
        for i in 0..6u32 {
            let mut f = silence.clone();
            f.timestamp_us = i * 20_000;
            if let Err(err) = player.push_frame(&f) {
                // 无物理/虚拟音频设备的无头环境在 AudioQueueStart 时可能报错，跳过硬件启动断言。
                if err.contains("AudioQueueStart") {
                    return;
                }
                panic!("unexpected AudioQueue push_frame error: {err}");
            }
        }

        player.set_muted(false);
        assert!(!player.is_muted());
        player.stop();
        assert!(player.push_frame(&silence).is_err());
    }
}
