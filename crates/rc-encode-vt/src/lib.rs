//! RabbitControl 的 VideoToolbox 硬件编码后端（任务卡 T12）。
//!
//! # 职责
//! 用 `VTCompressionSession` 实现 [`rc_encode::Encoder`]（H.264），
//! 吃 ScreenCaptureKit 交付的 [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)，
//! 产出 Annex-B 码流。
//!
//! # 零拷贝
//! 输入的 `CVPixelBuffer` **句柄直通** VideoToolbox：不 LockBaseAddress、
//! 不读 CPU 像素（坑 7：锁了才有 CPU 视图，零拷贝的前提就是不锁）。
//! 与 `rc-capture-macos` 同卡交付正是因为共享这条 GPU 路径。
//!
//! # 延迟红线（契约 E1–E6 的落点）
//! | 契约 | 落点 |
//! |---|---|
//! | E1（b_frames = 0） | 构造期 `EncoderConfig::validate` 拒绝 + `AllowFrameReordering = false` |
//! | E2（关 look-ahead / 2-pass） | `low_latency` 构造期强制 + `RealTime = true`（M6，未设时 VT 按文件转码优化，延迟 +20–40ms；VT 实时路径不存在 2-pass） |
//! | E3（输出零延迟） | `MaxFrameDelayCount = 0`；`flush` 恒返回空 |
//! | E4（输出序 == 输入序） | 会话内帧号单调，同步等待本帧输出，一进一出 |
//! | E5（超时不阻塞） | 预算 = `deadline_ts - 帧时间戳`（注入时间域），到点返回 `Error::Timeout`；迟到输出丢弃 + 下一帧强制 IDR 恢复（低延迟优先于完整） |
//! | E6（启动自检） | `new` 时编一帧假数据（内容未定义的 '420v' 缓冲）：结构校验（Annex-B / SPS+PPS / IDR）**并用 `VTDecompressionSession` 解码验证**，失败返回 `Err` |
//!
//! # 输出格式
//! VideoToolbox 的输出是 AVCC 风格（长度前缀 NAL、参数集在格式描述里，
//! SDK 无 Annex-B 开关），本 crate 在输出侧重排为 Annex-B（纯逻辑在
//! [`annexb`]，含 3/4 字节起始码直通防御），并在
//! `capabilities().bitstream_format` 里如实声明。
//!
//! 非 macOS 目标上本 crate 编译为空壳（仅保留纯逻辑模块），保证 workspace
//! 在任何平台都能构建与测试。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

/// AVCC → Annex-B 重排与 NAL 结构解析（纯函数，跨平台可测）。
///
/// 调用方只有 macOS 的 ffi 模块；非 macOS 空壳构建里它没有调用者但仍然
/// 编译 + 跑单测（保持纯逻辑在任何目标上可验证），因此容忍空壳的
/// dead_code 提示。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod annexb;

#[cfg(target_os = "macos")]
pub(crate) mod ffi;

use rc_encode::{BitstreamFormat, EncoderCaps, EncoderConfig};
use rc_types::Resolution;

#[cfg(target_os = "macos")]
use std::fmt;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use rc_capture::{CapturedFrame, FrameTexture};
#[cfg(target_os = "macos")]
use rc_encode::{EncodedFrame, Encoder, Preset};
#[cfg(target_os = "macos")]
use rc_types::{Error, FrameKind, Result, Timestamp};

/// 能力声明是配置的纯函数：任何平台（含无 GPU 的 CI）都可校验。
/// 调用方在 macOS 的 `VtEncoder` 里；空壳构建同样容忍无调用者。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn caps_for(cfg: &EncoderConfig) -> EncoderCaps {
    EncoderCaps {
        codec: cfg.codec,
        // 会话创建时显式要求硬件编码器（RequireHardwareAccelerated = true，
        // 见 ffi 模块），无硬件可用会在构造期失败而非静默落回软编。
        hardware: true,
        // 保守声明 4K：超过它应由上层降分辨率（M9 下采样在采集侧完成）。
        max_resolution: Resolution::new(4096, 4096),
        // ffi 侧重排为 Annex-B（见模块文档「输出格式」）。
        bitstream_format: BitstreamFormat::AnnexB,
        // 与会话属性 ProfileLevel = Baseline_AutoLevel 一致（profile idc 66）。
        profiles: &[66],
    }
}

/// 工厂：按配置创建 VideoToolbox 编码器（[`VtEncoder::new`] 的别名入口）。
///
/// ```no_run
/// # #[cfg(target_os = "macos")]
/// # fn main() -> rc_types::Result<()> {
/// use rc_encode::Encoder as _;
/// let cfg = rc_encode::EncoderConfig::default(); // 1080p60 低延迟 H.264
/// let mut enc = rc_encode_vt::new_encoder(cfg)?; // 含 E6 启动自检
/// assert!(enc.capabilities().hardware);
/// # Ok(())
/// # }
/// # #[cfg(not(target_os = "macos"))]
/// # fn main() {}
/// ```
#[cfg(target_os = "macos")]
pub fn new_encoder(config: EncoderConfig) -> Result<VtEncoder> {
    VtEncoder::new(config)
}

/// VideoToolbox 硬件 H.264 编码器（实现 [`Encoder`](rc_encode::Encoder)）。
///
/// - 只接受 [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)
///   输入（SCK 零拷贝路径）；CPU 帧请交给软件编码器（对称于
///   `FakeEncoder` 只接受 CPU 帧）；
/// - 会话属性在构造期逐条落死（M6/E1/E2/E3/无限 GOP，见 crate 文档），
///   并通过 E6 启动自检；
/// - 关键帧按需产生：`force_idr` / `request_idr` / 首个正式帧；
///   无限 GOP 下不自动插关键帧。
#[cfg(target_os = "macos")]
pub struct VtEncoder {
    cfg: EncoderConfig,
    session: ffi::CompressionSession,
    /// `request_idr` 置位，下一次成功编码消费。
    idr_requested: bool,
    /// 首个正式帧强制 IDR：E6 自检帧的输出不进管线（被丢弃），解码端
    /// 仍需要一个带 SPS/PPS 的 IDR 开场。
    first_frame_pending: bool,
}

#[cfg(target_os = "macos")]
impl fmt::Debug for VtEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // session 是不透明 FFI 句柄，不展开（打印指针地址无诊断价值）。
        f.debug_struct("VtEncoder")
            .field("cfg", &self.cfg)
            .field("idr_requested", &self.idr_requested)
            .field("first_frame_pending", &self.first_frame_pending)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "macos")]
impl VtEncoder {
    /// 创建编码器：配置校验 → 建会话（属性逐条落死）→ E6 启动自检。
    ///
    /// 任何一步失败都返回 `Err`（花屏问题在构造期暴露，而不是运行期）。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        validate_supported(&cfg)?;
        let session = ffi::CompressionSession::new(&ffi::SessionParams {
            width: cfg.resolution.width,
            height: cfg.resolution.height,
            bitrate_bps: cfg.bitrate_bps,
            fps: cfg.fps,
            gop_len: cfg.gop_len,
            // 抽象档位映射：Fastest/Fast → 速度优先；Balanced 及以上用
            // 编码器默认（RealTime 已保证延迟红线，档位只影响质量）。
            speed_over_quality: matches!(cfg.preset, Preset::Fastest | Preset::Fast),
        })
        .map_err(Error::Encoder)?;
        let mut encoder = Self {
            cfg,
            session,
            idr_requested: false,
            first_frame_pending: true,
        };
        encoder.self_check()?;
        Ok(encoder)
    }

    /// E6 启动自检：编一帧假数据（内容未定义的 '420v' 缓冲，无需初始化
    /// 像素——会话可用性不依赖画面内容），先验证输出结构，再用
    /// `VTDecompressionSession` 把这帧**真解码回来**（契约 §7.2 E6 的
    /// 原文：「编一帧假数据并解码验证」）。
    ///
    /// 两层校验各挡一类故障：结构校验（Annex-B 可解析、含 SPS/PPS、是
    /// IDR）暴露「会话建立但产出不可解析码流」；解码验证暴露「码流形态
    /// 对但内容解不回来」（参数集与 slice 不匹配、码流损坏）——花屏类
    /// 问题因此在构造期报 `Err`，而不是上线后由远端用户看到。
    fn self_check(&mut self) -> Result<()> {
        let buffer = ffi::make_pixel_buffer(self.cfg.resolution.width, self.cfg.resolution.height)
            .map_err(Error::Encoder)?;
        let output = self
            .session
            .encode(buffer.as_opaque_ptr(), true, Duration::from_millis(500))
            .map_err(|e| Error::Encoder(format!("E6 启动自检（编码一帧假数据）失败: {e}")))?;
        if !output.keyframe
            || !annexb::has_parameter_sets(&output.data)
            || annexb::nal_unit_types(&output.data).is_empty()
        {
            return Err(Error::Encoder(format!(
                "E6 启动自检失败：输出（{} 字节）不是带 SPS/PPS 的 Annex-B IDR",
                output.data.len()
            )));
        }
        ffi::decode_verify_annexb(
            &output.data,
            self.cfg.resolution.width,
            self.cfg.resolution.height,
            Duration::from_secs(2),
        )
        .map_err(|e| Error::Encoder(format!("E6 启动自检（解码验证）失败: {e}")))?;
        Ok(())
    }
}

/// 构造期配置校验：先跑抽象层校验（E1/E2 与基本项），再补本后端的
/// 支持范围。全部在触碰任何 FFI 之前完成。
#[cfg(target_os = "macos")]
fn validate_supported(cfg: &EncoderConfig) -> Result<()> {
    cfg.validate()?;
    if cfg.codec != rc_encode::Codec::H264 {
        return Err(Error::InvalidConfig(format!(
            "rc-encode-vt 目前只实现 H.264（kCMVideoCodecType_H264），配置的是 {}",
            cfg.codec.as_str()
        )));
    }
    if !cfg.resolution.is_even() {
        return Err(Error::InvalidConfig(format!(
            "rc-encode-vt: 分辨率必须为偶数（YUV 4:2:0），当前 {}x{}；请先 Resolution::to_even",
            cfg.resolution.width, cfg.resolution.height
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
impl Encoder for VtEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        // 零拷贝路径：只接受 CVPixelBuffer 句柄（SCK 交付形态）。
        let buffer = match &frame.texture {
            FrameTexture::CVPixelBuffer(ptr) => *ptr,
            _ => {
                return Err(Error::InvalidFrame(
                    "rc-encode-vt 只接受 FrameTexture::CVPixelBuffer（SCK → VT 零拷贝路径）；CPU 帧请交给软件编码器"
                        .to_string(),
                ))
            }
        };
        if buffer.is_null() {
            return Err(Error::InvalidFrame("CVPixelBuffer 句柄为空".to_string()));
        }
        // 尺寸核对：分辨率漂移（含 Retina 2x 直灌，M9）在入口拒绝，绝不带病编码。
        let (width, height, _format) = ffi::pixel_buffer_info(buffer);
        if width != self.cfg.resolution.width || height != self.cfg.resolution.height {
            return Err(Error::InvalidFrame(format!(
                "CVPixelBuffer {}x{} != 编码配置 {}x{}（M9：Retina 应先下采样到逻辑分辨率）",
                width, height, self.cfg.resolution.width, self.cfg.resolution.height
            )));
        }

        // E5：预算 = deadline_ts - 帧时间戳（同一注入时间域），从进入本函数
        // 起算。帧时间戳是内容变化时刻，早于调用时刻的部分计入预算，因此
        // 实际等待可能略长于调用方预期的剩余时间——完成时刻按注入时间域
        // 估算（encode_done_ts），这是契约允许的模型（见 rc-encode 文档）。
        let budget_us = deadline_ts.0.saturating_sub(frame.timestamp.0);
        if budget_us == 0 {
            // 已到截止：立即超时，不进 FFI。
            return Err(Error::Timeout(Duration::ZERO));
        }

        let force = force_idr || self.idr_requested || self.first_frame_pending;
        let started = Instant::now();
        let output =
            match self
                .session
                .encode(buffer, force, Duration::from_micros(u64::from(budget_us)))
            {
                Ok(output) => output,
                Err(ffi::EncodeError::Timeout) => {
                    // 丢帧恢复：迟到输出即弃（不缓冲，E3），请求下一帧强制 IDR
                    // 重建参考链——低延迟优先于完整。
                    self.idr_requested = true;
                    return Err(Error::Timeout(started.elapsed()));
                }
                Err(ffi::EncodeError::Dropped) => {
                    // 参考链语义（与 Timeout 分支的刻意对比）：
                    // - Dropped：编码器**没有编这帧**——输入被丢，编码器内部
                    //   与解码端的参考链均无损，下一帧照常 P 帧即可，因此
                    //   **不**置 idr_requested（置了会让下一帧无谓地变 IDR）。
                    // - Timeout：输出已产生但迟到被弃 → 解码端缺这一帧、
                    //   参考链真断 → 必须置 idr_requested 重建参考（现状不动）。
                    // RealTime 过载丢帧在高帧率逼近硬编吞吐上限时是预期瞬态
                    // （1080p×120 实测 VT 吞吐 ≈105–108fps < 120fps），
                    // 用非致命变体上报：管线丢帧继续（P7），不拆会话。
                    return Err(Error::EncoderDroppedFrame {
                        msg: "VideoToolbox 丢弃了本帧（RealTime 过载）".to_string(),
                    });
                }
                Err(ffi::EncodeError::Failed(msg)) => return Err(Error::Encoder(msg)),
            };
        self.first_frame_pending = false;
        // force 已随本帧提交，IDR 请求已消费。
        self.idr_requested = false;
        let elapsed_us = u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX);
        Ok(EncodedFrame {
            data: output.data,
            kind: if output.keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe: output.keyframe,
            encode_done_ts: Timestamp(frame.timestamp.0.saturating_add(elapsed_us)),
        })
    }

    fn capabilities(&self) -> EncoderCaps {
        caps_for(&self.cfg)
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(Error::InvalidConfig(
                "rc-encode-vt: bitrate must be > 0".to_string(),
            ));
        }
        self.session.set_bitrate(bps).map_err(Error::Encoder)
    }

    fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // E3：零输出延迟——会话内没有待取的帧；超时被丢弃的帧不在此复活
        //（丢弃帧的恢复手段是下一帧强制 IDR，不是补发）。
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_encode::{Codec, FakeEncoder};

    /// E1/E2：违反延迟红线的配置在构造期被拒，错误信息指明字段——
    /// 校验发生在触碰任何 FFI 之前，无 GPU 也能跑。
    #[cfg(target_os = "macos")]
    #[test]
    fn new_rejects_latency_violations_e1_e2() {
        let b_frames = VtEncoder::new(EncoderConfig {
            b_frames: 2,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&b_frames, Error::InvalidConfig(msg) if msg.contains("b_frames")),
            "E1：b_frames != 0 必须被拒，实际 {b_frames:?}"
        );

        let low_latency = VtEncoder::new(EncoderConfig {
            low_latency: false,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&low_latency, Error::InvalidConfig(msg) if msg.contains("low_latency")),
            "E2：low_latency = false 必须被拒，实际 {low_latency:?}"
        );

        let zero_fps = VtEncoder::new(EncoderConfig {
            fps: 0,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&zero_fps, Error::InvalidConfig(msg) if msg.contains("fps")),
            "fps = 0 必须被拒，实际 {zero_fps:?}"
        );
    }

    /// 本后端的支持范围：非 H.264 与奇数分辨率在构造期被拒（同样不碰 FFI）。
    #[cfg(target_os = "macos")]
    #[test]
    fn new_rejects_unsupported_codec_and_odd_resolution() {
        let hevc = VtEncoder::new(EncoderConfig {
            codec: Codec::Hevc,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&hevc, Error::InvalidConfig(msg) if msg.contains("H.264")),
            "HEVC 超出本后端支持范围，实际 {hevc:?}"
        );

        let odd = VtEncoder::new(EncoderConfig {
            resolution: rc_types::Resolution::new(1921, 1081),
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(
            matches!(&odd, Error::InvalidConfig(msg) if msg.contains("偶数")),
            "奇数分辨率（YUV 4:2:0 不支持）必须被拒，实际 {odd:?}"
        );
    }

    /// caps 声明与实现严格一致：硬件 H.264、Annex-B（输出侧确实做了
    /// AVCC → Annex-B 重排）、baseline（与会话 ProfileLevel 一致）。
    #[test]
    fn caps_declare_hardware_h264_annexb_baseline() {
        let caps = caps_for(&EncoderConfig::default());
        assert_eq!(caps.codec, Codec::H264);
        assert!(caps.hardware, "VideoToolbox 是硬件编码器");
        assert_eq!(
            caps.bitstream_format,
            BitstreamFormat::AnnexB,
            "caps 声明什么，解码端就按什么解析——输出侧重排后必须声明 Annex-B"
        );
        assert_eq!(caps.profiles, &[66], "profile idc 66 = baseline");
    }

    /// 与 FakeEncoder 的 trait 一致性对照：两条管线的码流形态一致
    /// （Annex-B / H.264 / baseline），调用方可以无差别消费；
    /// 唯一刻意的差别是硬件标志（软件假路径 vs VT 硬件路径）。
    #[test]
    fn caps_shape_matches_fake_encoder() {
        // 显式经 trait 调用：不依赖父模块的（平台门控）导入。
        let fake = rc_encode::Encoder::capabilities(
            &FakeEncoder::new(EncoderConfig::default()).expect("假编码器配置合法"),
        );
        let vt = caps_for(&EncoderConfig::default());
        assert_eq!(
            vt.bitstream_format, fake.bitstream_format,
            "两条管线必须同为 Annex-B（Annex-B 与 AVCC 不能混用）"
        );
        assert_eq!(vt.codec, fake.codec);
        assert!(fake.profiles.contains(&66));
        assert!(!fake.hardware, "假编码器是软件路径");
        assert!(vt.hardware, "VT 是硬件路径");
    }
}

/// 真机测试：需要 macOS + 可用的 VideoToolbox 编码器（不依赖屏幕采集
/// 权限——输入是自建的像素缓冲）。
#[cfg(all(test, target_os = "macos"))]
mod real_machine {
    use super::*;
    use rc_capture::{CapturedFrame, FrameTexture};
    use rc_types::{CaptureSeq, Timestamp};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// 串行化真机测试：避免多个硬件编码会话并发争用媒体引擎、污染
    /// 全局 `latency_diag` 与延迟基线。
    static REAL_MACHINE_LOCK: Mutex<()> = Mutex::new(());

    /// T12 真机验收：验证 `kVTCompressionPropertyKey_RealTime` 确实生效。
    ///
    /// 方法（按任务卡注释）：编码 300 帧，统计从输入到输出的延迟分布；
    /// 未设标志时 VT 按文件转码优化，P50 会 > 30ms；设了应 < 12ms。
    /// 顺带抽检动态行为：首帧 IDR、`request_idr` 下一帧生效、码率可调。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（真机延迟验收，T12）"]
    fn videotoolbox_realtime_flag_is_set() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = crate::ffi::latency_diag::take();
        let mut encoder =
            VtEncoder::new(EncoderConfig::default()).expect("构造（含 E6 启动自检）必须成功");
        let buffer = crate::ffi::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");

        let mut samples_ms: Vec<f64> = Vec::with_capacity(300);
        let mut keyframe_count = 0usize;
        // 按 60fps 节奏提交：与帧时间戳（16_667µs/帧）和会话
        // ExpectedFrameRate=60 一致——真实管线（SCK 采集 → 编码）就是
        // 这条节奏。背靠背 ~107fps 提交会让 PTS 谎报节奏、码控按 60fps
        // 误判，测得的是突发吞吐延迟而非稳态单帧延迟。
        let frame_period = Duration::from_nanos(16_666_667);
        let mut next_submit = Instant::now() + frame_period;
        for i in 0u32..300 {
            let ts = Timestamp(i * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let started = Instant::now();
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("第 {i} 帧编码失败（预算 50ms）: {e}"));
            samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            if out.is_keyframe {
                keyframe_count += 1;
            }
            // 睡到下一个 60fps 提交点（睡眠在采样之后，不污染延迟测量；
            // 偶发超睡由 next_submit 累计对齐吸收，超时则立即提交）。
            if let Some(sleep) = next_submit.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            }
            next_submit += frame_period;

            if i == 0 {
                assert!(
                    out.is_keyframe,
                    "首个正式帧必须是 IDR（解码端依赖它建参考）"
                );
            }
            if i == 150 {
                encoder.request_idr();
                encoder
                    .set_bitrate(6_000_000)
                    .expect("动态码率调整必须成功");
            }
            if i == 151 {
                assert!(out.is_keyframe, "request_idr 后下一帧必须是关键帧");
            }
        }

        samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
        let p50 = samples_ms[samples_ms.len() / 2];
        let p95 = samples_ms[(samples_ms.len() * 95) / 100];
        // 真机诊断输出：验收数字直接可见（--ignored 运行时才打印）。
        eprintln!(
            "VideoToolbox 1080p 输入→输出延迟（300 帧）：P50 = {p50:.2}ms, P95 = {p95:.2}ms, min = {:.2}ms, max = {:.2}ms",
            samples_ms[0],
            samples_ms[samples_ms.len() - 1]
        );

        // —— 延迟分解（cfg(test) 探针，ffi::latency_diag）——
        // 第 0 条记录是 E6 自检帧，跳过；四段长度可能不齐时按最短对齐。
        let marks = crate::ffi::latency_diag::take();
        let n = marks
            .submitted
            .len()
            .min(marks.callback.len())
            .min(marks.woken.len())
            .min(marks.returned.len())
            .saturating_sub(1);
        let seg_ms = |a: std::time::Instant, b: std::time::Instant| {
            b.checked_duration_since(a)
                .unwrap_or_default()
                .as_secs_f64()
                * 1000.0
        };
        let mut vt_ms: Vec<f64> = Vec::with_capacity(n);
        let mut wake_ms: Vec<f64> = Vec::with_capacity(n);
        let mut extract_ms: Vec<f64> = Vec::with_capacity(n);
        for r in 1..=n {
            vt_ms.push(seg_ms(marks.submitted[r], marks.callback[r]));
            wake_ms.push(seg_ms(marks.callback[r], marks.woken[r]));
            extract_ms.push(seg_ms(marks.woken[r], marks.returned[r]));
        }
        let report = |name: &str, xs: &mut Vec<f64>| {
            if xs.is_empty() {
                return;
            }
            xs.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
            eprintln!(
                "  {name}: P50 = {:.2}ms, P95 = {:.2}ms, min = {:.2}ms, max = {:.2}ms",
                xs[xs.len() / 2],
                xs[xs.len() * 95 / 100],
                xs[0],
                xs[xs.len() - 1]
            );
        };
        eprintln!(
            "  300 帧中关键帧 = {keyframe_count}（首帧 + request_idr@150 各 1，超出即语义异常）"
        );
        report("submit→callback（VT 内部）", &mut vt_ms);
        report("callback→唤醒（线程切换）", &mut wake_ms);
        report("唤醒→返回（提取+重排）", &mut extract_ms);
        // 相邻回调到达间隔：若 VT 以固定节拍投递输出，会在这里显形。
        let mut cadence_ms: Vec<f64> = Vec::with_capacity(n.saturating_sub(1));
        for r in 2..=n {
            cadence_ms.push(seg_ms(marks.callback[r - 1], marks.callback[r]));
        }
        report("相邻 callback 间隔（投递节拍）", &mut cadence_ms);
        // 总延迟直方图（1ms 桶，>20ms 归并）：分布形态（单峰/双峰）可见。
        let mut hist: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
        for s in &samples_ms {
            *hist.entry((*s as u64).min(20)).or_default() += 1;
        }
        eprintln!("  总延迟直方图（1ms 桶，20 = ≥20ms 归并）：{hist:?}");
        assert!(
            p50 < 12.0,
            "输入→输出延迟 P50 = {p50:.2}ms ≥ 12ms：kVTCompressionPropertyKey_RealTime 可能未生效（未设时 P50 通常 > 30ms；本次 P95 = {p95:.2}ms）"
        );
    }

    /// E3 实证：`kVTCompressionPropertyKey_MaxFrameDelayCount = 0` 在本机
    /// 编码器上的原始设置结果。
    ///
    /// 构造路径只容忍「成功(0)」与 `kVTPropertyNotSupportedErr(-12900)`两种
    /// 结果（其余状态码 = 构造失败）。Apple Silicon 硬件 H.264 编码器对该
    /// 属性一律不支持的声明过去只有代码注释佐证——本测试把它变成真机上的
    /// 可观测事实：跑出任何第三种状态码都说明容忍范围写错了。零输出延迟
    /// 本身不依赖该属性设置成功：由结构保证（严格单帧在途 + 每次提交前
    /// 清空邮箱，见 ffi 模块 E3 注释），300 帧逐帧同步等待的延迟测试
    ///（`videotoolbox_realtime_flag_is_set`）隐式验证了这一点。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（E3 属性实证，T12）"]
    fn max_frame_delay_property_status_on_this_device() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut encoder = VtEncoder::new(EncoderConfig::default())
            .expect("构造（含 E6 自检：编码 + 解码验证）必须成功");
        let status = encoder
            .session
            .probe_max_frame_delay_zero_status()
            .expect("属性键常量自 macOS 10.8 起存在，不可能缺失");
        // 真机诊断输出：本机编码器对该属性的态度直接可见（--ignored 运行时打印）。
        eprintln!("MaxFrameDelayCount=0 在本机的原始 OSStatus = {status}");
        assert!(
            status == 0 || status == -12900,
            "E3 容忍范围外的结果：OSStatus {status}（0 = 设置成功，-12900 = \
             kVTPropertyNotSupportedErr；其它状态码必须让构造失败，不该被容忍）"
        );
    }

    /// 无限 GOP 真机回归：`gop_len = 0` 下连续 300 帧（首帧后全部
    /// `force_idr = false`）必须只产生 1 个 IDR（仅首帧），不自动周期性插
    /// 关键帧；随后显式 `force_idr = true` 仍可立即触发 IDR。
    #[test]
    #[ignore = "requires macOS + 真实 VideoToolbox 编码器（无限 GOP 真机回归）"]
    fn infinite_gop_emits_only_initial_and_forced_idr() {
        let _serial = REAL_MACHINE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = EncoderConfig {
            gop_len: 0,
            ..EncoderConfig::default()
        };
        let mut encoder = VtEncoder::new(cfg).expect("构造（含 E6 启动自检）必须成功");
        let buffer = crate::ffi::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");

        let mut idr_indices: Vec<u32> = Vec::new();
        for i in 0u32..300 {
            let ts = Timestamp(i * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("第 {i} 帧编码失败: {e}"));
            if out.is_keyframe {
                idr_indices.push(i);
            }
        }

        let ts_forced = Timestamp(300 * 16_667);
        let forced_frame = CapturedFrame {
            texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
            timestamp: ts_forced,
            dirty: None,
            seq: CaptureSeq(300),
            is_repeat: false,
            resource_guard: None,
        };
        let forced_out = encoder
            .encode(
                &forced_frame,
                true,
                Timestamp(ts_forced.0.saturating_add(50_000)),
            )
            .expect("第 300 帧显式 force_idr 编码必须成功");

        eprintln!(
            "无限 GOP 300 帧真机回归：IDR 数量 = {}（帧号 {idr_indices:?}），第 300 帧 force_idr = {}",
            idr_indices.len(),
            forced_out.is_keyframe
        );
        assert_eq!(
            idr_indices,
            vec![0],
            "gop_len=0（无限 GOP）300 帧内不应自动插入关键帧，仅首帧应为 IDR"
        );
        assert!(
            forced_out.is_keyframe,
            "显式 force_idr=true 必须立即触发 IDR"
        );
    }
}
