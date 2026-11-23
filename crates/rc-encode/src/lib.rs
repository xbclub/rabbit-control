//! 编码抽象层：平台无关的 [`Encoder`] trait、配置/能力类型与假编码器。
//!
//! # 职责边界
//! - 本 crate 只定义「编码侧的形状」：trait、配置、能力描述与输出帧。
//! - 真正的实现（NVENC / VideoToolbox / openh264）在各自 crate 里实现
//!   [`Encoder`]，本 crate **不含任何平台依赖**。
//! - [`fake::FakeEncoder`] 是可配置耗时与输出大小的假编码器，
//!   供回环测试与无 GPU 的 CI 使用。
//!
//! # 延迟红线（契约 E1–E5）
//! - [`EncoderConfig`] 强制 `b_frames == 0`、`low_latency == true`（E1/E2），
//!   违反配置在构造期被拒绝，而不是运行期默默吃掉 1–4 帧延迟；
//! - 实现必须零输出延迟（E3）：`encode` 一进一出，[`Encoder::flush`]
//!   对零缓冲实现恒返回空；
//! - 输出顺序 == 输入顺序（E4），不得乱序交付；
//! - `encode` 的 `deadline_ts` 超出时返回 [`Error::Timeout`] 而不是阻塞（E5），
//!   时间由调用方注入（[`Timestamp`]），实现内部不读系统时钟。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod fake;

pub use fake::FakeEncoder;
pub use rc_types::{Error, Result};

use std::time::Duration;

use rc_capture::CapturedFrame;
use rc_types::{FrameKind, Resolution, Timestamp};

/// 视频编码器。实现必须满足 E1–E5（见 crate 文档）。
///
/// # 时间注入
/// `encode` 不读系统时钟：完成时刻由 `deadline_ts`（调用方注入的截止
/// 时间戳）判定，输出帧的 [`EncodedFrame::encode_done_ts`] 是编码器
/// 估算的完成时刻。
pub trait Encoder: Send {
    /// 对一帧采集数据编码。
    ///
    /// - `force_idr`：本帧强制编成关键帧（丢包恢复 / 新观众加入）；
    /// - `deadline_ts`：截止时间戳。预计完成时刻超过它时返回
    ///   [`Error::Timeout`]，**不得阻塞等待**（E5）；
    /// - 返回值与输入一一对应（一帧进一帧出，E3/E4）。
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame>;

    /// 静态能力描述。调用方据此选择编码器与解码端协商参数。
    fn capabilities(&self) -> EncoderCaps;

    /// 动态调整目标码率（自适应码率的入口）。
    ///
    /// `bps` 为 0 或超出实现范围时返回 [`Error::InvalidConfig`]。
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;

    /// 请求下一帧输出为关键帧（与 [`Encoder::encode`] 的 `force_idr`
    /// 等效，供不持帧引用的调用路径使用）。
    fn request_idr(&mut self);

    /// 冲刷内部缓冲，返回仍在编码器内的帧。
    ///
    /// 零输出延迟（E3）的实现没有任何内部缓冲，恒返回空 `Vec`；
    /// 保留本方法是为了契约稳定（个别硬件编码器在复位时需要冲刷）。
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;

    /// 是否支持 2-deep 异步流水线编码（[`Encoder::submit_encode`] /
    /// [`Encoder::recv_encoded`]）。默认返回 `false`。
    fn supports_pipelined_encode(&self) -> bool {
        false
    }

    /// 异步提交一帧进行编码（仅当 [`Encoder::supports_pipelined_encode`]
    /// 为 `true` 时可用）。调用方以 `token` 关联输入与输出，并需保证
    /// 在途帧数不超过实现上限。
    fn submit_encode(
        &mut self,
        _token: u64,
        _frame: &CapturedFrame,
        _force_idr: bool,
        _deadline_ts: Timestamp,
    ) -> Result<()> {
        Err(Error::InvalidConfig(
            "pipelined encode not supported".into(),
        ))
    }

    /// 接收下一帧已完成的异步编码输出（按 FIFO 提交顺序返回）。
    /// 超时无就绪输出时返回 `Ok(None)`。
    fn recv_encoded(&mut self, _timeout: Duration) -> Result<Option<(u64, EncodedFrame)>> {
        Ok(None)
    }

    /// 当前已提交但尚未被 [`Encoder::recv_encoded`] 取走的在途帧数。
    fn in_flight_count(&self) -> usize {
        0
    }
}

/// 编码输出的一帧。
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// 压缩码流。Annex-B 或 AVCC 由 [`EncoderCaps::bitstream_format`] 声明，
    /// 解码端据此选择解析方式，两者**不能**混用。
    pub data: Vec<u8>,
    /// 帧语义类型（IDR / P）。调度器据此决定可丢性。
    pub kind: FrameKind,
    /// 是否为关键帧。与 `kind == FrameKind::Idr` 恒等价，
    /// 冗余存放是为了让调用方少一次模式匹配。
    pub is_keyframe: bool,
    /// 编码完成时刻（时间戳，非墙钟）。延迟测量的 encode 段终点。
    pub encode_done_ts: Timestamp,
}

/// 编码器的静态能力。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderCaps {
    /// 实际使用的编解码器。
    pub codec: Codec,
    /// 是否硬件编码（NVENC / VideoToolbox / VAAPI）。
    pub hardware: bool,
    /// 支持的最大分辨率。超过它应降级（降分辨率或换编码器）。
    pub max_resolution: Resolution,
    /// 输出码流格式（Annex-B / AVCC）。
    pub bitstream_format: BitstreamFormat,
    /// 支持的 profile idc 列表（H.264: 66=baseline, 77=main, 100=high）。
    pub profiles: &'static [u8],
}

/// 编码器配置。
///
/// 构造编码器前应调用 [`EncoderConfig::validate`]；违反延迟红线的配置
/// （`b_frames != 0`、`low_latency == false`）在构造期被拒绝（E1/E2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 使用的编解码器。
    pub codec: Codec,
    /// 编码分辨率（应与采集分辨率一致，必要时由调用方先
    /// [`Resolution::to_even`]）。
    pub resolution: Resolution,
    /// 目标帧率。
    pub fps: u32,
    /// 初始目标码率（bps）。
    pub bitrate_bps: u32,
    /// 质量/速度档位（后端各自的原始档位映射到该抽象档位）。
    pub preset: Preset,
    /// 低延迟模式。**必须为 true**（E2：关闭 look-ahead / 2-pass）。
    pub low_latency: bool,
    /// GOP 长度。建议 0 = 无限 GOP，按需 IDR（`force_idr` / `request_idr`）。
    pub gop_len: u32,
    /// B 帧数。**必须为 0**（E1：B 帧引入 1–4 帧延迟）。
    pub b_frames: u32,
}

impl Default for EncoderConfig {
    /// 低延迟 H.264 1080p60、8 Mbps、无限 GOP——本项目管线的中位配置。
    fn default() -> Self {
        EncoderConfig {
            codec: Codec::H264,
            resolution: Resolution::new(1920, 1080),
            fps: 60,
            bitrate_bps: 8_000_000,
            preset: Preset::Balanced,
            low_latency: true,
            gop_len: 0,
            b_frames: 0,
        }
    }
}

impl EncoderConfig {
    /// 校验配置，违反项以 [`Error::InvalidConfig`] 返回，信息含字段名。
    ///
    /// 强制项：
    /// - `fps > 0`、`bitrate_bps > 0`、分辨率非零；
    /// - `b_frames == 0`（E1）；
    /// - `low_latency == true`（E2）。
    pub fn validate(&self) -> Result<()> {
        if self.fps == 0 {
            return Err(Error::InvalidConfig(
                "encoder config: fps must be > 0".to_string(),
            ));
        }
        if self.resolution.width == 0 || self.resolution.height == 0 {
            return Err(Error::InvalidConfig(
                "encoder config: resolution must be non-zero".to_string(),
            ));
        }
        if self.bitrate_bps == 0 {
            return Err(Error::InvalidConfig(
                "encoder config: bitrate_bps must be > 0".to_string(),
            ));
        }
        if self.b_frames != 0 {
            return Err(Error::InvalidConfig(format!(
                "encoder config: b_frames must be 0 (E1, got {})",
                self.b_frames
            )));
        }
        if !self.low_latency {
            return Err(Error::InvalidConfig(
                "encoder config: low_latency must be true (E2)".to_string(),
            ));
        }
        Ok(())
    }
}

/// 编解码器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// H.264 / AVC。兼容性与解码端覆盖面最好，实时管线的默认选择。
    H264,
    /// H.265 / HEVC。同画质省约 30% 码率，老控制端硬解覆盖不如 H.264。
    Hevc,
    /// AV1。码率最优但实时硬编目前只在少数平台可用，作为前瞻选项。
    Av1,
}

impl Codec {
    /// 规范小写名（诊断与日志用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }
}

/// 质量/速度档位。
///
/// 各后端的原始档位（NVENC 的 P1–P7、VideoToolbox 的 Speed–Quality）
/// **不进入抽象层**：后端实现负责把这里的抽象档位映射到各自最接近的
/// 原始档位。低延迟实时管线通常只用 [`Preset::Fastest`] 到
/// [`Preset::Balanced`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Preset {
    /// 最快、质量最低。NVENC ≈ P1–P2；VideoToolbox ≈ 实时优先。
    Fastest,
    /// 偏速度。NVENC ≈ P3。
    Fast,
    /// 均衡。NVENC ≈ P4；VideoToolbox ≈ 默认。
    Balanced,
    /// 偏质量。NVENC ≈ P5–P6。实时管线慎用（可能赶不上帧间隔）。
    HighQuality,
    /// 最慢、质量最高。NVENC ≈ P7；VideoToolbox ≈ 质量优先。
    /// 实时低延迟管线不应使用。
    Slowest,
}

/// 压缩码流的 NAL 封装格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitstreamFormat {
    /// Annex-B：每个 NAL 前有 4 字节起始码 `00 00 00 01`。
    /// openh264 与多数硬件编码器的原生输出。
    AnnexB,
    /// AVCC：每个 NAL 前有长度前缀（大端）。MP4 风格封装。
    Avcc,
}

impl BitstreamFormat {
    /// 规范小写名（诊断与日志用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            BitstreamFormat::AnnexB => "annexb",
            BitstreamFormat::Avcc => "avcc",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_capture::FrameTexture;

    /// 合法配置必须通过校验。
    #[test]
    fn default_config_passes_validation() {
        assert!(EncoderConfig::default().validate().is_ok());
    }

    /// E1/E2：违反延迟红线的配置必须被拒绝，且错误信息指明字段。
    #[test]
    fn validate_rejects_latency_violations() {
        let b_frames = EncoderConfig {
            b_frames: 2,
            ..EncoderConfig::default()
        };
        let err = b_frames.validate().unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(ref msg) if msg.contains("b_frames")));

        let lookahead = EncoderConfig {
            low_latency: false,
            ..EncoderConfig::default()
        };
        let err = lookahead.validate().unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig(ref msg) if msg.contains("low_latency")),
            "E2：low_latency = false 必须被拒绝，实际 {err:?}"
        );
    }

    /// 零值参数必须被拒绝（fps / 分辨率 / 码率）。
    #[test]
    fn validate_rejects_zero_values() {
        assert!(matches!(
            EncoderConfig {
                fps: 0,
                ..EncoderConfig::default()
            }
            .validate(),
            Err(Error::InvalidConfig(ref msg)) if msg.contains("fps")
        ));
        assert!(matches!(
            EncoderConfig {
                resolution: Resolution::new(0, 1080),
                ..EncoderConfig::default()
            }
            .validate(),
            Err(Error::InvalidConfig(ref msg)) if msg.contains("resolution")
        ));
        assert!(matches!(
            EncoderConfig {
                bitrate_bps: 0,
                ..EncoderConfig::default()
            }
            .validate(),
            Err(Error::InvalidConfig(ref msg)) if msg.contains("bitrate_bps")
        ));
    }

    /// 枚举的规范名固定，诊断脚本按名字解析，不能随意改。
    #[test]
    fn enum_names_are_stable() {
        assert_eq!(Codec::H264.as_str(), "h264");
        assert_eq!(Codec::Hevc.as_str(), "hevc");
        assert_eq!(Codec::Av1.as_str(), "av1");
        assert_eq!(BitstreamFormat::AnnexB.as_str(), "annexb");
        assert_eq!(BitstreamFormat::Avcc.as_str(), "avcc");
    }

    /// 默认流水线编码方法对非流水线编码器保持安全退化语义。
    #[test]
    fn default_pipelined_encoder_methods_reject_submit_and_return_none() {
        let mut enc = FakeEncoder::new(EncoderConfig::default()).expect("假编码器配置合法");
        assert!(!enc.supports_pipelined_encode());
        assert_eq!(enc.in_flight_count(), 0);
        assert!(enc
            .recv_encoded(Duration::from_millis(1))
            .expect("recv_encoded 默认应返回 Ok(None)")
            .is_none());

        let frame = CapturedFrame {
            texture: FrameTexture::Cpu(rc_types::CpuFrame {
                width: 4,
                height: 4,
                format: rc_types::PixelFormat::Bgra8,
                stride: 16,
                data: vec![0; 64],
            }),
            timestamp: Timestamp(0),
            dirty: None,
            seq: rc_types::CaptureSeq(0),
            is_repeat: false,
            resource_guard: None,
        };
        let err = enc
            .submit_encode(1, &frame, false, Timestamp(50_000))
            .expect_err("不支持流水线的编码器调用 submit_encode 应返回 Err");
        assert!(matches!(err, Error::InvalidConfig(_)));
    }
}
