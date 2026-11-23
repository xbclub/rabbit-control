//! [`CaptureBackend`] 的 ScreenCaptureKit 实现。
//!
//! 流程（对应 T12 的 M 条目）：
//! 1. 枚举 `SCShareableContent` → 选显示器（坑 5：先枚举才能建 filter）；
//! 2. `SCContentFilter`（整屏，坑 10：每显示器一个 stream）；
//! 3. `SCStreamConfiguration`：`'420v'`（M5）、`showsCursor=false`（M7）、
//!    输出逻辑分辨率（M9：Retina 下采样）；
//! 4. `SCStream` + 独立串行 dispatch queue 上的帧回调（坑 6），
//!    回调只做提取，帧经最新帧优先单槽邮箱交给 [`next_frame`](SckBackend::next_frame)；
//! 5. `stream:didStopWithError:`（M10，权限撤销）→ `AccessLost`，可重建。
//!
//! 关键语义：
//! - **C1（静止）**：SCK 只在内容变化时回调，静止时 `next_frame` 阻塞到超时
//!   返回 `Ok(None)`，绝不编造重复帧；
//! - **C2（时间戳）**：取 CMSampleBuffer 的 PTS（内容变化时刻），以首帧为
//!   原点换算微秒（见 [`pts_to_timestamp`]）；
//! - **C5（rebuild 幂等）**：`rebuild` 丢弃旧流、重建新流（M8：SCStream
//!   停止后不可复用），连续调用无叠加副作用；
//! - **M10**：失效通过 [`InvalidationReason`] 上报而不是 panic。
//!
//! 本文件不含 unsafe —— 所有 ObjC/FFI 交互都在 [`crate::ffi`]。

use std::sync::Arc;
use std::time::Duration;

use rc_capture::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_types::{CaptureSeq, DisplayId, Error, Resolution, Result, Timestamp};

use crate::ffi::{self, FrameEvent, FrameMailbox, FramePayload, StreamHandles};

/// SCK 后端配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SckConfig {
    /// 采集的显示器索引：`0` 表示主显示器（`CGMainDisplayID` 匹配，
    /// 找不到时退回列表第一个），`n > 0` 取枚举列表的第 n 个。
    pub display: u32,
    /// 帧率上限。SCK 只在内容变化时出帧，这是上限不是承诺（C1）。
    pub fps: u32,
    /// 可选的输出分辨率覆写。
    ///
    /// - `None`（默认）：沿用 M9 逻辑分辨率下采样（`backing / scale`，向下取偶）；
    /// - `Some(res)`：按指定分辨率向下取偶作为 SCK 输出尺寸（用于与编码器目标
    ///   分辨率对齐，如 1080p60）。宽高必须均大于 0。
    pub resolution: Option<Resolution>,
    /// 可选的会话时钟原点（微秒，需来自 [`crate::host_time_us`]，与 SCK PTS 同域）。
    ///
    /// - `None`（默认）：以本流首帧 PTS 为原点（独立采集 / 单测语义）；
    /// - `Some(origin)`：所有帧的 `timestamp = pts_us.saturating_sub(origin)`，
    ///   使 `Mark::Captured` 与管线会话时钟严格同轴、零启动偏移。
    pub origin_us: Option<u64>,
}

impl Default for SckConfig {
    fn default() -> Self {
        Self {
            display: 0,
            fps: 60,
            resolution: None,
            origin_us: None,
        }
    }
}

/// ScreenCaptureKit 采集后端（实现 [`CaptureBackend`]）。
///
/// 用 [`SckBackend::new`] 创建；失败（无显示器、无权限等）返回
/// `Error::Capture`。
pub struct SckBackend {
    config: SckConfig,
    /// 当前流的句柄 + 最新帧优先单槽邮箱。`None` = 已失效待重建。
    live: Option<(StreamHandles, FrameMailbox)>,
    /// 缓存的显示器信息（display_info 用，避免每次枚举）。
    display: DisplaySummary,
    /// 本流首帧的 PTS（微秒），作为时间戳原点（C2）。
    origin_pts_us: Option<u64>,
    /// 采集序号（跨 rebuild 连续，会话内唯一）。
    seq: CaptureSeq,
    /// 最近一帧缓冲的元信息 `(width, height, stride, pixel_format)`，供测试核对。
    /// 真实 `PixelBufferHandle` 的所有权已随 [`CapturedFrame::resource_guard`] 交给
    /// 消费方，消费/丢弃后即刻 `CFRelease` 归还 SCK IOSurface 池，零悬垂、零占池。
    #[cfg_attr(not(test), allow(dead_code))]
    last_buffer_info: Option<(u32, u32, u32, u32)>,
    /// 待上报的失效原因（重建前持续上报，C4/C5）。
    invalidation: Option<InvalidationReason>,
    /// 失效原因的可读描述。
    invalidation_msg: String,
    /// 首帧校验是否已完成（格式 / 分辨率核对）。
    first_frame_checked: bool,
}

/// `display_info` 需要的显示器摘要（plain data，可跨线程）。
struct DisplaySummary {
    id: DisplayId,
    /// 实际请求 SCK 输出的分辨率（配置覆写或逻辑分辨率，M9）。
    resolution: Resolution,
    /// 物理分辨率 / 逻辑分辨率。
    scale: f32,
}

impl SckBackend {
    /// 创建并启动采集。
    ///
    /// 前置条件：屏幕录制权限已授予（用 [`crate::permission::PermissionStatus::check`]
    /// 先检查）；未授予时启动失败并返回可读错误，不会 panic。
    pub fn new(config: SckConfig) -> Result<Self> {
        validate_config(&config)?;
        let mut backend = Self {
            config,
            live: None,
            display: DisplaySummary {
                id: DisplayId(0),
                resolution: Resolution::default(),
                scale: 1.0,
            },
            origin_pts_us: None,
            seq: CaptureSeq(0),
            last_buffer_info: None,
            invalidation: None,
            invalidation_msg: String::new(),
            first_frame_checked: false,
        };
        backend.establish()?;
        Ok(backend)
    }

    /// 枚举显示器并构建一条全新的 SCStream（`new` 与 `rebuild` 共用，M8）。
    fn establish(&mut self) -> Result<()> {
        let displays = ffi::enumerate_displays().map_err(unknown_capture_error)?;
        let snapshot = pick_display(&displays, self.config.display).ok_or_else(|| {
            unknown_capture_error(format!(
                "找不到可采集的显示器（index={}, 共 {} 个）",
                self.config.display,
                displays.len()
            ))
        })?;

        // 若配置指定了输出分辨率，取偶后直接请求 SCK 按该尺寸缩放输出；
        // 否则沿用 M9：Retina 下采样到逻辑分辨率（GPU 侧完成，CPU 不过手），
        // 再保证偶数（YUV 4:2:0 / 编码器要求，参考 rc-types 的 to_even）。
        //
        // 注意：这里刻意**不**断言 target == SCDisplay 上报逻辑分辨率取偶
        //——下采样数学的正确性由单测 retina_downsample_computes_correct_target
        // 覆盖，而系统上报数据是否自洽不由我们保证，依赖它的断言放在运行
        // 路径上会在系统数据不一致时把 debug 构建打 panic。真正的防线在
        // 首帧校验：SCK 实际交付的分辨率必须等于我们请求的 target，否则报错。
        let target =
            resolve_target_resolution(self.config.resolution, snapshot.backing, snapshot.scale);

        let (handles, mailbox) =
            ffi::build_stream(snapshot, target, self.config.fps).map_err(unknown_capture_error)?;

        self.display = DisplaySummary {
            id: DisplayId(self.config.display),
            resolution: target,
            scale: snapshot.scale,
        };
        self.live = Some((handles, mailbox));
        self.origin_pts_us = None;
        self.last_buffer_info = None;
        self.first_frame_checked = false;
        Ok(())
    }

    /// 记录失效并拆除当前流（保留原因供 `poll_invalidation` 持续上报）。
    fn invalidate(&mut self, reason: InvalidationReason, msg: impl Into<String>) -> Error {
        self.live = None;
        self.last_buffer_info = None;
        self.invalidation = Some(reason);
        self.invalidation_msg = msg.into();
        reason.to_capture_error(self.invalidation_msg.clone())
    }
}

impl CaptureBackend for SckBackend {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        std::thread_local! {
            static QOS_PROMOTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        QOS_PROMOTED.with(|promoted| {
            if !promoted.get() {
                crate::promote_current_thread_qos();
                promoted.set(true);
            }
        });

        // 已失效：持续返回错误，直到上层 rebuild（C4）。
        if let Some(reason) = self.invalidation {
            return Err(reason.to_capture_error(self.invalidation_msg.clone()));
        }
        let Some((_, mailbox)) = &self.live else {
            // 不应出现（无流必有失效记录），防御性兜底。
            return Err(self.invalidation_wrap(InvalidationReason::Unknown, "流不存在且无失效记录"));
        };

        let event = mailbox.recv_timeout(timeout);
        match event {
            Some(FrameEvent::Frame(FramePayload { buffer, pts_us })) => {
                // C2：优先使用调用方注入的会话时钟原点（与管线同域同零点）；
                // 未注入时以本流首帧 PTS 为原点。SCK 的 PTS 单调递增。
                let origin = match self.config.origin_us {
                    Some(o) => o,
                    None => *self.origin_pts_us.get_or_insert(pts_us),
                };
                let timestamp = pts_to_timestamp(pts_us, origin);

                // 首帧校验：SCK 必须按我们的配置交付 '420v' + 请求的分辨率，
                // 否则零拷贝链路（M5/M9）建立在错误的前提上。
                if !self.first_frame_checked {
                    self.first_frame_checked = true;
                    let (w, h, _stride, format) = buffer.info();
                    if format != ffi::PIXEL_FORMAT_420V {
                        return Err(Error::InvalidFrame(format!(
                            "SCK 交付的像素格式 0x{format:08x} 不是 '420v'（0x{:08x}）",
                            ffi::PIXEL_FORMAT_420V
                        )));
                    }
                    if Resolution::new(w, h) != self.display.resolution {
                        return Err(Error::InvalidFrame(format!(
                            "SCK 交付分辨率 {w}x{h} 与请求的逻辑分辨率 {}x{} 不符",
                            self.display.resolution.width, self.display.resolution.height
                        )));
                    }
                }

                // 将 PixelBufferHandle 装入 CapturedFrame::resource_guard：
                // 帧在跨线程编码期间始终保活，Drop 时立即 CFRelease 归还 SCK 池。
                let next_seq = self.seq.0.checked_add(1).ok_or_else(|| {
                    InvalidationReason::Unknown
                        .to_capture_error("采集序号耗尽 u64，无法继续保证单调".to_string())
                })?;
                self.last_buffer_info = Some(buffer.info());
                let texture = FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr());
                self.seq = CaptureSeq(next_seq);
                // macOS SCK 无脏区 API：dirty = None，上层用 TileDiff（见 T12
                // 参考实现表）。SCK 也不会重复交同一帧：is_repeat 恒 false。
                Ok(Some(CapturedFrame {
                    texture,
                    timestamp,
                    dirty: None,
                    seq: self.seq,
                    is_repeat: false,
                    resource_guard: Some(Arc::new(buffer)),
                }))
            }
            Some(FrameEvent::Suspended) => Err(self.invalidation_wrap(
                InvalidationReason::SessionLocked,
                "SCStream 挂起（锁屏 / 会话切换），解锁后请 rebuild",
            )),
            Some(FrameEvent::Stopped { message }) => Err(self.invalidation_wrap(
                InvalidationReason::AccessLost,
                format!("SCStream 已停止: {message}"),
            )),
            None => {
                // C1：超时即无变化，返回 None 而不是错误 / 重复帧。
                Ok(None)
            }
        }
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        self.invalidation
    }

    fn rebuild(&mut self) -> Result<()> {
        // C5 幂等 + M8：SCStream 停止后不可复用 —— 直接丢弃旧流
        // （StreamHandles::drop 会显式 stop），然后构建全新实例。
        // 连续调用只是多建几条新流，无叠加副作用。
        self.live = None;
        self.invalidation = None;
        self.invalidation_msg.clear();
        self.last_buffer_info = None;
        self.establish()
    }

    fn display_info(&self) -> DisplayInfo {
        DisplayInfo {
            id: self.display.id,
            resolution: self.display.resolution,
            scale: self.display.scale,
            // SDR 路径：SCK 的 SCCaptureDynamicRange 默认 SDR。HDR 切换
            // （HdrToggled）在启用 HDR 采集的后续卡里处理。
            hdr: false,
        }
    }
}

impl SckBackend {
    /// [`invalidate`](SckBackend::invalidate) 的内部变体：返回错误而不是只记录。
    fn invalidation_wrap(&mut self, reason: InvalidationReason, msg: impl Into<String>) -> Error {
        self.invalidate(reason, msg)
    }
}

/// 校验 [`SckConfig`] 参数合法性（在建流之前执行）。
fn validate_config(config: &SckConfig) -> Result<()> {
    if config.fps == 0 {
        return Err(Error::InvalidConfig("fps 必须大于 0".to_string()));
    }
    if let Some(res) = config.resolution {
        if res.width == 0 || res.height == 0 {
            return Err(Error::InvalidConfig(format!(
                "resolution 宽高必须均大于 0，实际 {}x{}",
                res.width, res.height
            )));
        }
    }
    Ok(())
}

/// 决定 SCK 实际请求的输出分辨率：显式配置优先（向下取偶），否则按 M9 逻辑分辨率下采样。
pub(crate) fn resolve_target_resolution(
    configured: Option<Resolution>,
    backing: Resolution,
    scale: f32,
) -> Resolution {
    match configured {
        Some(res) => res.to_even(),
        None => downsample_target(backing, scale),
    }
}

/// 把建流期的未知失败映射为致命采集错误（原因不明时不盲目重试）。
fn unknown_capture_error(msg: impl Into<String>) -> Error {
    InvalidationReason::Unknown.to_capture_error(msg)
}

/// 按配置挑选显示器：index 0 → 主显示器，否则列表第 index 个。
fn pick_display(displays: &[ffi::DisplaySnapshot], index: u32) -> Option<&ffi::DisplaySnapshot> {
    if displays.is_empty() {
        return None;
    }
    if index == 0 {
        let main_id = ffi::main_display_id();
        displays
            .iter()
            .find(|d| d.display_id == main_id)
            .or_else(|| displays.first())
    } else {
        displays.get(index as usize)
    }
}

/// M9：计算 SCK 输出分辨率 —— 物理分辨率按 scale 下采样到逻辑分辨率，
/// 再向下取整到偶数。
///
/// - Retina 2x：`3840x2160 @2.0 → 1920x1080`；
/// - 非 Retina（scale = 1）：原样；
/// - 病态 scale（0 / NaN / Inf）：按 1.0 处理，绝不出 0 尺寸。
pub(crate) fn downsample_target(backing: Resolution, scale: f32) -> Resolution {
    let scale = if scale.is_finite() && scale >= 1.0 {
        scale
    } else {
        1.0
    };
    let width = ((backing.width as f32) / scale).round();
    let height = ((backing.height as f32) / scale).round();
    Resolution::new(width as u32, height as u32).to_even()
}

/// C2：PTS（微秒）→ 相对会话原点的时间戳。
///
/// - 原点取本流首帧的 PTS，因此首帧时间戳为 0；
/// - 超过 `u32::MAX` 微秒（约 71.6 分钟，`Timestamp` 的表示范围）时饱和到
///   最大值而不是回绕 —— 回绕中的 PTS 序列对延迟归因是灾难。
pub(crate) fn pts_to_timestamp(pts_us: u64, origin_us: u64) -> Timestamp {
    Timestamp(u32::try_from(pts_us.saturating_sub(origin_us)).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn retina_downsample_computes_correct_target() {
        // M9：Retina 2x → 逻辑分辨率（不把 2x 喂给编码器）。
        assert_eq!(
            downsample_target(Resolution::new(3840, 2160), 2.0),
            Resolution::new(1920, 1080)
        );
        assert_eq!(
            downsample_target(Resolution::new(3024, 1964), 2.0),
            Resolution::new(1512, 982)
        );
        assert_eq!(
            downsample_target(Resolution::new(3360, 2100), 2.0),
            Resolution::new(1680, 1050)
        );
        assert_eq!(
            downsample_target(Resolution::new(2560, 1600), 2.0),
            Resolution::new(1280, 800)
        );
        // 非 Retina：原样保留。
        assert_eq!(
            downsample_target(Resolution::new(1920, 1080), 1.0),
            Resolution::new(1920, 1080)
        );
        assert_eq!(
            downsample_target(Resolution::new(1440, 900), 1.0),
            Resolution::new(1440, 900)
        );
        // 逻辑尺寸出现奇数时向下取偶（YUV 4:2:0 的前置条件）。
        assert_eq!(
            downsample_target(Resolution::new(3026, 1964), 2.0),
            Resolution::new(1512, 982)
        );
        assert_eq!(
            downsample_target(Resolution::new(1515, 983), 1.0),
            Resolution::new(1514, 982)
        );
        // 病态 scale：按 1.0 处理，绝不出 0 尺寸。
        for bad in [0.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                downsample_target(Resolution::new(1920, 1080), bad),
                Resolution::new(1920, 1080),
                "scale = {bad} 时应按 1.0 处理"
            );
        }
    }

    #[test]
    fn frame_timestamp_monotonic() {
        // C2：时间戳 = 内容变化时刻（PTS 相对首帧），必须单调递增。
        // 60fps 的典型 PTS 序列（微秒，timescale=600 的等价换算）。
        let pts_series = [0u64, 16_667, 33_334, 50_001, 66_668, 83_335];
        let mut origin: Option<u64> = None;
        let mut stamps = Vec::new();
        for &pts in &pts_series {
            let o = *origin.get_or_insert(pts);
            stamps.push(pts_to_timestamp(pts, o));
        }
        // 首帧即原点：时间戳为 0。
        assert_eq!(stamps[0], Timestamp(0));
        // 严格递增。
        for pair in stamps.windows(2) {
            assert!(
                pair[1] > pair[0],
                "时间戳必须严格递增：{} -> {}",
                pair[0].0,
                pair[1].0
            );
        }
        // 换算正确性：16.667ms 的帧间隔 ≈ 16667us。
        assert_eq!(stamps[1], Timestamp(16_667));
        // 饱和：超过 u32::MAX 微秒（约 71.6 分钟）封顶，绝不回绕。
        // 4e9us（仍在范围内）如实保留；5e9us（超范围）饱和。
        assert_eq!(pts_to_timestamp(u64::MAX, 0), Timestamp(u32::MAX));
        assert_eq!(
            pts_to_timestamp(5_000_000_000, 1_000_000_000),
            Timestamp(4_000_000_000)
        );
        assert_eq!(
            pts_to_timestamp(6_000_000_000, 1_000_000_000),
            Timestamp(u32::MAX)
        );
    }

    #[test]
    fn configured_resolution_validation_and_even_rounding() {
        // 默认配置 resolution 为 None，回退到 M9 逻辑分辨率下采样。
        let default_cfg = SckConfig::default();
        assert_eq!(default_cfg.resolution, None);
        assert!(validate_config(&default_cfg).is_ok());
        assert_eq!(
            resolve_target_resolution(default_cfg.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1512, 982)
        );

        // 显式指定 resolution：合法配置通过校验，并向下取偶以匹配 YUV 4:2:0。
        let cfg_1080p = SckConfig {
            resolution: Some(Resolution::new(1920, 1080)),
            ..SckConfig::default()
        };
        assert!(validate_config(&cfg_1080p).is_ok());
        assert_eq!(
            resolve_target_resolution(cfg_1080p.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1920, 1080)
        );

        let cfg_odd = SckConfig {
            resolution: Some(Resolution::new(1921, 1081)),
            ..SckConfig::default()
        };
        assert!(validate_config(&cfg_odd).is_ok());
        assert_eq!(
            resolve_target_resolution(cfg_odd.resolution, Resolution::new(3024, 1964), 2.0),
            Resolution::new(1920, 1080)
        );

        // 宽或高为 0 时 SckBackend::new 必须返回 Error::InvalidConfig（不触发 SCK 建流）。
        for invalid in [
            Resolution::new(0, 1080),
            Resolution::new(1920, 0),
            Resolution::new(0, 0),
        ] {
            let err = SckBackend::new(SckConfig {
                resolution: Some(invalid),
                ..SckConfig::default()
            })
            .err()
            .expect("零宽高分辨率应拒绝");
            assert!(
                matches!(err, Error::InvalidConfig(_)),
                "期望 InvalidConfig，实际: {err:?}"
            );
        }
    }

    // ── 真机测试（需要 macOS + 三项授权，CI / 沙箱跑不了）──

    /// 真机验收（T12）：1080p 采集 ≥58fps。
    ///
    /// 注意：SCK 只在内容变化时出帧 —— 运行本测试时屏幕上必须有动态内容
    /// （播放视频 / 拖动窗口），屏幕静止时拿不到帧不是实现 bug。
    #[test]
    #[ignore = "requires macOS + permissions granted"]
    fn sck_captures_at_60fps() {
        let mut backend = SckBackend::new(SckConfig::default()).expect("构建 SCK 后端");
        let mut count = 0u64;
        let mut last: Option<Timestamp> = None;
        let mut dts_us: Vec<u32> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(frame) = backend
                .next_frame(Duration::from_millis(100))
                .expect("采集循环不应出错")
            {
                if let Some(prev) = last {
                    assert!(frame.timestamp > prev, "时间戳必须单调递增（C2）");
                    dts_us.push(frame.timestamp.0 - prev.0);
                }
                last = Some(frame.timestamp);
                count += 1;
            }
        }
        let sample_dts: Vec<u32> = dts_us.iter().copied().take(15).collect();
        let mut sorted = dts_us.clone();
        sorted.sort_unstable();
        let p50 = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
        let min_dt = sorted.first().copied().unwrap_or(0);
        let max_dt = sorted.last().copied().unwrap_or(0);
        eprintln!(
            "sck_captures_at_60fps: count={count}, dt_us(min={min_dt}, p50={p50}, max={max_dt}), first15={sample_dts:?}"
        );
        assert!(
            count >= 116,
            "2 秒内应采集 ≥116 帧（≥58fps），实际 {count} 帧 \
             （若屏幕静止请先制造动态内容再跑）"
        );
    }

    /// M8：SCStream 停止后不可复用 —— rebuild 必须创建全新实例并恢复采集。
    #[test]
    #[ignore = "requires macOS + permissions granted"]
    fn sck_stream_rebuilds_after_stop() {
        let mut backend = SckBackend::new(SckConfig::default()).expect("构建 SCK 后端");
        // 先收 1 秒帧（屏幕静止时收不到帧是正常的，不作为失败）。
        let _ = backend
            .next_frame(Duration::from_secs(1))
            .expect("首帧阶段不应出错");
        let seq_before = backend.seq;

        // rebuild = 丢弃旧流 + 建新流（M8 的可观测行为：成功、无失效残留）。
        backend.rebuild().expect("rebuild 必须成功");
        assert_eq!(backend.poll_invalidation(), None, "重建后失效状态应清零");

        // 重建后必须还能出帧；采集序号跨 rebuild 连续。
        if let Some(frame) = backend
            .next_frame(Duration::from_secs(3))
            .expect("重建后不应出错")
        {
            assert!(
                frame.seq.0 > seq_before.0,
                "重建后的帧序号必须继续前进（会话内唯一）"
            );
            // M9 真机核对：SCK 实际交付的是逻辑分辨率（不是 2x 物理分辨率）。
            let (w, h, _stride, format) = backend
                .last_buffer_info
                .expect("出帧后必须记录最近缓冲信息");
            assert_eq!(
                format,
                ffi::PIXEL_FORMAT_420V,
                "交付格式必须是 '420v'（M5）"
            );
            let info = backend.display_info();
            assert_eq!(
                (w, h),
                (info.resolution.width, info.resolution.height),
                "交付分辨率 {w}x{h} 应等于请求的逻辑分辨率（M9）"
            );
        }
        // 连续 rebuild（C5 幂等）：第二次同样成功。
        backend.rebuild().expect("连续 rebuild 必须成功（C5 幂等）");
    }
}
