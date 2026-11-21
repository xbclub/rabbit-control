//! 假编码器：无 GPU、无系统时钟的 [`Encoder`] 实现。
//!
//! # 用途
//! 把编码抽象层的行为契约（E1–E5、IDR 语义、顺序保持）固化成
//! 无 GPU 的 CI 可执行断言；同时供回环测试填充管线。
//!
//! # 时间模型
//! 不读系统时钟：一帧的编码完成时刻 = 帧的 `timestamp` + 可配置耗时
//! （[`FakeEncoder::with_tuning`]）。`deadline_ts` 早于该完成时刻时
//! 返回 [`Error::Timeout`]（E5），与真编码器的「预计超时即放弃」一致。
//!
//! # 输出模型
//! 输出不是真压缩码流，而是固定大小的合成数据：Annex-B 起始码
//! `00 00 00 01` + 一个 NAL 头（IDR = nal type 5，P = nal type 1）+ 零填充。
//! 这样大小与关键帧标记都可配置、可断言，而无需引入真编解码器。

use crate::{BitstreamFormat, EncodedFrame, Encoder, EncoderCaps, EncoderConfig};
use rc_capture::{CapturedFrame, FrameTexture};
use rc_types::{Error, FrameKind, Resolution, Result, Timestamp};

/// 默认编码耗时（微秒）：5ms，与 rc-loopback 假管线的 encode 段一致，
/// 保证两条假链路的分段延迟可互相对照。
pub const DEFAULT_ENCODE_COST_US: u32 = 5_000;

/// 默认输出字节数：1 KiB，量级接近 1080p 低码率下的真实 P 帧。
pub const DEFAULT_OUTPUT_SIZE: usize = 1_024;

/// IDR 帧输出长度下限：起始码(4) + NAL 头(1) + 至少 1 字节载荷。
const MIN_OUTPUT_SIZE: usize = 6;

/// Annex-B 起始码。合成输出的前 4 字节，解码侧据此识别 NAL 边界。
const ANNEXB_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// 假编码器。
///
/// - 声明为软件编码器（`hardware == false`，Annex-B，baseline profile）；
/// - 只接受 [`FrameTexture::Cpu`] 输入（软件路径不共享 GPU 纹理）；
/// - 零内部缓冲（E3）：一进一出，[`Encoder::flush`] 恒返回空。
#[derive(Debug)]
pub struct FakeEncoder {
    cfg: EncoderConfig,
    /// 模拟的单帧编码耗时（微秒）。
    cost_us: u32,
    /// 每帧输出的字节数。
    out_size: usize,
    /// 当前目标码率（`set_bitrate` 可变）。
    bitrate_bps: u32,
    /// `request_idr` 置位，下一次成功编码消费。
    idr_requested: bool,
    /// 已成功编码的帧数（首帧强制 IDR）。
    encoded: u64,
    /// 距上一个 IDR 的帧数（`gop_len > 0` 时用于自动 IDR）。
    frames_since_idr: u64,
}

impl FakeEncoder {
    /// 以默认耗时（5ms）与默认输出大小（1 KiB）创建。
    ///
    /// 配置非法（含 E1/E2 违反）返回 [`Error::InvalidConfig`]。
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        Self::with_tuning(cfg, DEFAULT_ENCODE_COST_US, DEFAULT_OUTPUT_SIZE)
    }

    /// 指定编码耗时（微秒）与输出大小（字节）创建。
    ///
    /// `output_size_bytes < 6` 被拒绝：合成输出需要容纳起始码 + NAL 头 +
    /// 至少 1 字节载荷，更短的输出无法表达一个可解析的 NAL。
    pub fn with_tuning(
        cfg: EncoderConfig,
        encode_cost_us: u32,
        output_size_bytes: usize,
    ) -> Result<Self> {
        cfg.validate()?;
        if output_size_bytes < MIN_OUTPUT_SIZE {
            return Err(Error::InvalidConfig(format!(
                "fake encoder: output size must be >= {MIN_OUTPUT_SIZE} bytes (got {output_size_bytes})"
            )));
        }
        let bitrate_bps = cfg.bitrate_bps;
        Ok(FakeEncoder {
            cfg,
            cost_us: encode_cost_us,
            out_size: output_size_bytes,
            bitrate_bps,
            idr_requested: false,
            encoded: 0,
            frames_since_idr: 0,
        })
    }

    /// 当前目标码率（观测 `set_bitrate` 是否生效）。
    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// 已成功编码的帧数（超时/校验失败的帧不计入）。
    pub fn encoded_frames(&self) -> u64 {
        self.encoded
    }

    /// 合成一帧输出：起始码 + NAL 头（type 5 IDR / type 1 非 IDR）+ 零填充。
    fn synthesize(&self, is_keyframe: bool) -> Vec<u8> {
        let mut data = vec![0u8; self.out_size];
        data[..ANNEXB_START_CODE.len()].copy_from_slice(&ANNEXB_START_CODE);
        // H.264 NAL 头：forbidden_zero(1bit)=0, nal_ref_idc(2bit), type(5bit)。
        // IDR slice = 5；非 IDR slice = 1。
        data[4] = if is_keyframe { 0x65 } else { 0x41 };
        data
    }
}

impl Encoder for FakeEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        // 软件假编码器只吃 CPU 帧；GPU 纹理应交给硬件编码器后端。
        let cpu = match &frame.texture {
            FrameTexture::Cpu(cpu) => cpu,
            _ => {
                return Err(Error::InvalidFrame(
                    "fake encoder only accepts CPU frames".to_string(),
                ))
            }
        };
        cpu.validate()?;

        // E5：完成时刻 = 帧时间戳 + 耗时；超过截止即超时，不阻塞。
        let done_ts = Timestamp(frame.timestamp.0.saturating_add(self.cost_us));
        if done_ts > deadline_ts {
            // 错误路径不改变内部状态（帧未入列，IDR 请求保留）。
            return Err(Error::Timeout(done_ts.saturating_diff(deadline_ts)));
        }

        // 首帧必须 IDR（解码端无从解 P 帧）；之后按 force_idr / 请求 /
        // GOP 长度决定。gop_len == 0 表示无限 GOP，永不自动 IDR。
        let is_keyframe = force_idr
            || self.idr_requested
            || self.encoded == 0
            || (self.cfg.gop_len > 0 && self.frames_since_idr >= u64::from(self.cfg.gop_len));

        let out = EncodedFrame {
            data: self.synthesize(is_keyframe),
            kind: if is_keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe,
            encode_done_ts: done_ts,
        };
        self.encoded += 1;
        self.frames_since_idr = if is_keyframe {
            0
        } else {
            self.frames_since_idr + 1
        };
        self.idr_requested = false;
        Ok(out)
    }

    fn capabilities(&self) -> EncoderCaps {
        EncoderCaps {
            codec: self.cfg.codec,
            hardware: false,
            max_resolution: Resolution::new(4096, 4096),
            bitstream_format: BitstreamFormat::AnnexB,
            profiles: &[66], // baseline
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(Error::InvalidConfig(
                "fake encoder: bitrate must be > 0".to_string(),
            ));
        }
        self.bitrate_bps = bps;
        Ok(())
    }

    fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // E3：零输出延迟，没有任何内部缓冲。
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_types::{CaptureSeq, CpuFrame, PixelFormat, Rect};

    /// 构造 4×4 BGRA 的 CPU 采集帧（stride = width * 4，数据合法）。
    fn cpu_frame(ts_us: u32, seq: u64) -> CapturedFrame {
        CapturedFrame {
            texture: FrameTexture::Cpu(CpuFrame {
                width: 4,
                height: 4,
                format: PixelFormat::Bgra8,
                stride: 16,
                data: vec![0; 64],
            }),
            timestamp: Timestamp(ts_us),
            dirty: Some(Rect::new(0, 0, 4, 4)),
            seq: CaptureSeq(seq),
            is_repeat: false,
            resource_guard: None,
        }
    }

    /// 宽裕的截止时间（帧时间戳 + 100ms），让 encode 不因超时失败。
    fn relaxed_deadline(ts_us: u32) -> Timestamp {
        Timestamp(ts_us.saturating_add(100_000))
    }

    /// IDR 语义：首帧强制 IDR；request_idr 让下一帧重新成为关键帧。
    #[test]
    fn fake_encoder_emits_idr_on_first_frame_and_on_request() {
        let mut enc = FakeEncoder::new(EncoderConfig::default()).expect("配置合法");
        assert!(!enc.capabilities().hardware, "假编码器必须声明为软件编码器");

        // 首帧：即使不请求也必须是 IDR（解码端依赖首帧建参考）。
        let first = enc
            .encode(&cpu_frame(0, 0), false, relaxed_deadline(0))
            .expect("宽裕 deadline 下首帧编码成功");
        assert!(first.is_keyframe);
        assert_eq!(first.kind, FrameKind::Idr);
        assert_eq!(first.data[4] & 0x1f, 5, "IDR 的 NAL type 必须是 5");

        // 第二帧：普通 P 帧。
        let second = enc
            .encode(&cpu_frame(16_667, 1), false, relaxed_deadline(16_667))
            .expect("P 帧编码成功");
        assert!(!second.is_keyframe);
        assert_eq!(second.kind, FrameKind::P);
        assert_eq!(second.data[4] & 0x1f, 1, "非 IDR 的 NAL type 必须是 1");

        // request_idr：下一帧（force_idr = false）重新成为关键帧。
        enc.request_idr();
        let third = enc
            .encode(&cpu_frame(33_334, 2), false, relaxed_deadline(33_334))
            .expect("IDR 请求后的帧编码成功");
        assert!(third.is_keyframe, "request_idr 后下一帧必须是关键帧");
        assert_eq!(third.kind, FrameKind::Idr);

        // 请求已消费：再下一帧回到 P。
        let fourth = enc
            .encode(&cpu_frame(50_001, 3), false, relaxed_deadline(50_001))
            .expect("P 帧编码成功");
        assert!(!fourth.is_keyframe, "IDR 请求不应泄漏到后续帧");
    }

    /// E4：输出顺序 == 输入顺序，完成时刻与输入时间戳同向前进。
    #[test]
    fn fake_encoder_preserves_input_order() {
        let cost = 3_000u32;
        let mut enc =
            FakeEncoder::with_tuning(EncoderConfig::default(), cost, 32).expect("配置合法");
        let timestamps = [0u32, 16_667, 33_334, 50_001];
        let mut prev_done = None;
        for (i, &ts) in timestamps.iter().enumerate() {
            let out = enc
                .encode(&cpu_frame(ts, i as u64), false, relaxed_deadline(ts))
                .expect("宽裕 deadline 下编码成功");
            // 一进一出：每帧恰产出一个输出，完成时刻 = ts + cost。
            assert_eq!(out.encode_done_ts, Timestamp(ts + cost));
            if let Some(p) = prev_done {
                assert!(
                    out.encode_done_ts > p,
                    "输出完成时刻应随输入前进：{p:?} -> {:?}",
                    out.encode_done_ts
                );
            }
            prev_done = Some(out.encode_done_ts);
        }
        assert_eq!(enc.encoded_frames(), 4, "4 帧输入必须恰好 4 次成功编码");
    }

    /// E3：零内部缓冲，flush 恒返回空。
    #[test]
    fn fake_encoder_flush_returns_empty() {
        let mut enc = FakeEncoder::new(EncoderConfig::default()).expect("配置合法");
        for i in 0..3 {
            let ts = i * 16_667;
            enc.encode(&cpu_frame(ts, i as u64), false, relaxed_deadline(ts))
                .expect("编码成功");
        }
        assert!(
            enc.flush().expect("flush 不应失败").is_empty(),
            "零输出延迟（E3）下 flush 必须为空——编码器内不应滞留帧"
        );
        // flush 可重复调用，且不影响后续编码。
        assert!(enc.flush().expect("flush 不应失败").is_empty());
        let out = enc
            .encode(&cpu_frame(100_000, 3), false, relaxed_deadline(100_000))
            .expect("flush 后编码继续工作");
        assert!(!out.is_keyframe);
    }

    /// E5：预计完成时刻超过 deadline_ts 返回 Timeout，不阻塞。
    #[test]
    fn fake_encoder_times_out_past_deadline() {
        let cost = 5_000u32;
        let mut enc =
            FakeEncoder::with_tuning(EncoderConfig::default(), cost, 32).expect("配置合法");
        let frame_ts = 100_000u32;

        // 完成时刻 105_000 > deadline 101_000 → 超时，错误携带超出的时长。
        let err = enc
            .encode(&cpu_frame(frame_ts, 0), false, Timestamp(frame_ts + 1_000))
            .expect_err("超过 deadline 必须报 Timeout");
        assert!(
            matches!(err, Error::Timeout(d) if d == Timestamp(frame_ts + cost).saturating_diff(Timestamp(frame_ts + 1_000))),
            "应返回携带超出时长的 Timeout，实际 {err:?}"
        );

        // 恰好赶上（完成时刻 == deadline）不算超时。
        let ok = enc
            .encode(&cpu_frame(frame_ts, 0), false, Timestamp(frame_ts + cost))
            .expect("完成时刻等于 deadline 不算超时");
        assert_eq!(ok.encode_done_ts, Timestamp(frame_ts + cost));

        // 超时的帧没有计入成功数。
        assert_eq!(enc.encoded_frames(), 1);
    }

    /// 配置校验（E1/E2）与输入校验：非法输入必须在入口被拒绝。
    #[test]
    fn fake_encoder_rejects_invalid_config_and_frames() {
        // E1：b_frames != 0 在构造期被拒绝。
        let err = FakeEncoder::new(EncoderConfig {
            b_frames: 1,
            ..EncoderConfig::default()
        })
        .unwrap_err();
        assert!(matches!(&err, Error::InvalidConfig(msg) if msg.contains("b_frames")));

        // 输出太小放不下一个 NAL。
        let err = FakeEncoder::with_tuning(EncoderConfig::default(), 1_000, 5).unwrap_err();
        assert!(
            matches!(&err, Error::InvalidConfig(msg) if msg.contains("output size")),
            "过小的输出应被拒绝，实际 {err:?}"
        );

        let mut enc = FakeEncoder::new(EncoderConfig::default()).expect("配置合法");

        // 非 CPU 纹理（macOS 上可构造 CVPixelBuffer 变体）→ InvalidFrame。
        #[cfg(target_os = "macos")]
        {
            let gpu_frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(std::ptr::null_mut()),
                timestamp: Timestamp(0),
                dirty: None,
                seq: CaptureSeq(0),
                is_repeat: false,
                resource_guard: None,
            };
            let err = enc
                .encode(&gpu_frame, false, relaxed_deadline(0))
                .expect_err("软件假编码器必须拒绝 GPU 纹理");
            assert!(matches!(err, Error::InvalidFrame(_)));
        }

        // stride/数据长度不匹配的 CPU 帧 → InvalidFrame（透传 CpuFrame::validate）。
        let bad = CapturedFrame {
            texture: FrameTexture::Cpu(CpuFrame {
                width: 4,
                height: 4,
                format: PixelFormat::Bgra8,
                stride: 16,
                data: vec![0; 63], // 少 1 字节
            }),
            timestamp: Timestamp(0),
            dirty: None,
            seq: CaptureSeq(0),
            is_repeat: false,
            resource_guard: None,
        };
        assert!(matches!(
            enc.encode(&bad, false, relaxed_deadline(0)),
            Err(Error::InvalidFrame(_))
        ));

        // set_bitrate：0 被拒绝，合法值生效。
        assert!(matches!(enc.set_bitrate(0), Err(Error::InvalidConfig(_))));
        enc.set_bitrate(1_000_000).expect("合法码率必须被接受");
        assert_eq!(enc.bitrate_bps(), 1_000_000);
    }
}
