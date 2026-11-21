//! 假解码器：无 GPU、无系统时钟的 [`Decoder`] 实现。
//!
//! # 用途
//! 把解码抽象层的行为契约（D4 缺参考帧不 panic、D5 进一出一、deadline
//! 语义）固化成无 GPU 的 CI 可执行断言；同时供回环测试填充解码段。
//!
//! # 时间模型
//! 不读系统时钟、不 sleep：维护逻辑时钟，每次**被接受**的 `decode` 调用
//! （无论结果是 Frame / NeedKeyframe / Nothing）推进一个「单帧解码耗时」。
//! 完成时刻 = 调用时刻 + 耗时；预计完成时刻超过 `deadline_ts` 时返回
//! [`Error::Timeout`]，且不改变任何内部状态（帧未消费、时钟不前进）。
//!
//! # 参考链模型
//! - 初始参考链即断裂（模拟「中途入会、从未收到 IDR」），因此
//!   [`Decoder::needs_keyframe`] 起始为 `true`；
//! - 成功解出 IDR 重建链并清除关键帧请求；P 帧仅在链完整时可解，
//!   否则返回 [`DecodeOutput::NeedKeyframe`]（D4）；
//! - [`FakeDecoderConfig::drop_first_n`]：前 N 帧视为从未到达（模拟入会
//!   丢包 / 调度丢弃），返回 [`DecodeOutput::Nothing`]。丢 IDR 不影响
//!   既有链（没收到 ≠ 断链），丢 P 帧会使其后的 P 帧失去参考；
//! - [`FakeDecoderConfig::break_ref_chain_at`]：第 i 帧到达但其参考
//!   （第 i-1 帧）没有——该处与其后的 P 帧都解不出，直到下一个 IDR。
//!
//! # 帧标记通道
//! 输出纹理的前几字节透传输入码流的前几字节（至多 8 字节，不足截断）。
//! 测试用它给帧编号，验证「输出顺序 == 输入顺序」（D5），无需真的解出像素。

use crate::{DecodeOutput, DecodedFrame, Decoder};
use rc_capture::FrameTexture;
use rc_types::{CpuFrame, Error, FrameKind, PixelFormat, Resolution, Result, Timestamp};

/// 默认单帧解码耗时（微秒）：3ms，对应任务卡的解码延迟预算。
pub const DEFAULT_DECODE_COST_US: u32 = 3_000;

/// 帧标记通道宽度（字节）：输出纹理透传输入码流的前 N 字节。
const TAG_LEN: usize = 8;

/// 假解码器配置。
#[derive(Debug, Clone)]
pub struct FakeDecoderConfig {
    /// 输出帧分辨率。假解码器不真解码像素，按它填充输出纹理的尺寸。
    pub resolution: Resolution,
    /// 模拟的单帧解码耗时（微秒）。决定逻辑时钟步长与 deadline 判定。
    pub decode_cost_us: u32,
    /// 丢掉前 N 帧（模拟入会丢包 / 调度丢弃）：这些输入返回
    /// [`DecodeOutput::Nothing`]，不产出帧。
    pub drop_first_n: u32,
    /// 在第几个输入帧（0 起）处打断参考链（模拟其前一帧在网络中丢失）。
    /// `None` = 不断链。该处的 IDR 不受影响（IDR 不需要参考）。
    pub break_ref_chain_at: Option<u64>,
}

impl Default for FakeDecoderConfig {
    /// 1080p、3ms 解码耗时、不丢帧、不断链——与真解码器的默认假设一致。
    fn default() -> Self {
        FakeDecoderConfig {
            resolution: Resolution::new(1920, 1080),
            decode_cost_us: DEFAULT_DECODE_COST_US,
            drop_first_n: 0,
            break_ref_chain_at: None,
        }
    }
}

impl FakeDecoderConfig {
    /// 校验配置。非法项以 [`Error::InvalidConfig`] 返回，信息含字段名。
    pub fn validate(&self) -> Result<()> {
        if self.resolution.width == 0 || self.resolution.height == 0 {
            return Err(Error::InvalidConfig(
                "fake decoder: resolution must be non-zero".to_string(),
            ));
        }
        // CPU 兜底纹理按 BGRA 分配 stride；宽度乘 4 溢出说明分辨率荒谬。
        if self.resolution.width.checked_mul(4).is_none() {
            return Err(Error::InvalidConfig(
                "fake decoder: resolution.width too large".to_string(),
            ));
        }
        Ok(())
    }
}

/// 假解码器：按逻辑时钟「进一出一」的 [`Decoder`]。
#[derive(Debug)]
pub struct FakeDecoder {
    cfg: FakeDecoderConfig,
    /// 逻辑时钟（微秒）：下一次 decode 调用的时刻。
    clock_us: u32,
    /// 已接受的输入帧数（含被丢弃与解不出的；超时路径不计入）。
    frames_received: u64,
    /// 成功解出的帧数。
    frames_output: u64,
    /// 参考链是否完整（存在可用的前序参考）。
    chain_ok: bool,
}

impl FakeDecoder {
    /// 按配置创建假解码器。配置非法返回 [`Error::InvalidConfig`]。
    pub fn new(cfg: FakeDecoderConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(FakeDecoder {
            cfg,
            clock_us: 0,
            frames_received: 0,
            frames_output: 0,
            chain_ok: false,
        })
    }

    /// 已接受的输入帧数（含被丢弃与解不出的帧；超时路径不计入）。
    pub fn frames_received(&self) -> u64 {
        self.frames_received
    }

    /// 成功解出（返回 [`DecodeOutput::Frame`]）的帧数。
    pub fn frames_output(&self) -> u64 {
        self.frames_output
    }

    /// 当前逻辑时钟（= 最近一次被接受调用的完成时刻）。
    pub fn clock(&self) -> Timestamp {
        Timestamp(self.clock_us)
    }

    /// 构造输出纹理：全零 BGRA 缓冲 + 前几字节透传输入（帧标记通道）。
    fn build_texture(&self, input: &[u8]) -> FrameTexture {
        let res = self.cfg.resolution;
        let stride = res.width * 4; // validate 已保证不溢出
        let mut data = vec![0u8; stride as usize * res.height as usize];
        let n = input.len().min(TAG_LEN).min(data.len());
        data[..n].copy_from_slice(&input[..n]);
        FrameTexture::Cpu(CpuFrame {
            width: res.width,
            height: res.height,
            format: PixelFormat::Bgra8,
            stride,
            data,
        })
    }

    /// 组装输出帧并计数。
    fn emit(&mut self, input: &[u8], done_ts: Timestamp) -> DecodedFrame {
        let res = self.cfg.resolution;
        self.frames_output += 1;
        DecodedFrame {
            texture: self.build_texture(input),
            decode_done_ts: done_ts,
            width: res.width,
            height: res.height,
        }
    }
}

impl Decoder for FakeDecoder {
    fn decode(
        &mut self,
        data: &[u8],
        kind: FrameKind,
        deadline_ts: Timestamp,
    ) -> Result<DecodeOutput> {
        // deadline：完成时刻 = 调用时刻 + 耗时；超过即超时，不阻塞，
        // 且不改变内部状态（帧未消费、时钟不前进）。
        let done_ts = Timestamp(self.clock_us.saturating_add(self.cfg.decode_cost_us));
        if done_ts > deadline_ts {
            return Err(Error::Timeout(done_ts.saturating_diff(deadline_ts)));
        }
        // 被接受的调用统一推进逻辑时钟（无论结果是哪种输出）。
        self.clock_us = done_ts.0;
        let idx = self.frames_received;
        self.frames_received += 1;

        // 丢弃窗口：前 N 帧视为从未到达 → 无输出（码流不完整）。
        if idx < u64::from(self.cfg.drop_first_n) {
            // 丢 IDR 不影响既有链；丢 P 帧会使其后的 P 帧失去参考。
            if matches!(kind, FrameKind::P) {
                self.chain_ok = false;
            }
            return Ok(DecodeOutput::Nothing);
        }

        // 定点断链：本帧的参考没送到。IDR 不需要参考、仍可解出并重建链。
        if Some(idx) == self.cfg.break_ref_chain_at && matches!(kind, FrameKind::P) {
            self.chain_ok = false;
        }

        match kind {
            FrameKind::Idr => {
                // IDR 自带完整参考：解出、重建链、清除关键帧请求。
                self.chain_ok = true;
                Ok(DecodeOutput::Frame(self.emit(data, done_ts)))
            }
            FrameKind::P => {
                if self.chain_ok {
                    Ok(DecodeOutput::Frame(self.emit(data, done_ts)))
                } else {
                    // D4：缺参考帧不 panic，要求关键帧（链保持断裂）。
                    Ok(DecodeOutput::NeedKeyframe)
                }
            }
        }
    }

    fn needs_keyframe(&self) -> bool {
        // 参考链断裂（含「从未解出过 IDR」）即需要关键帧。
        !self.chain_ok
    }

    fn flush(&mut self) -> Result<()> {
        // 进一出一（D5）的实现没有内部缓冲，无帧可冲刷。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 64×64 的测试配置（小分辨率，测试瞬时）。
    fn cfg() -> FakeDecoderConfig {
        FakeDecoderConfig {
            resolution: Resolution::new(64, 64),
            ..FakeDecoderConfig::default()
        }
    }

    /// 构造带标记的假码流：前 8 字节 = 帧序号（小端），其后零填充。
    fn bitstream(tag: u64) -> Vec<u8> {
        let mut data = tag.to_le_bytes().to_vec();
        data.extend_from_slice(&[0u8; 8]);
        data
    }

    /// 宽裕的截止时间（任何合理配置下都不超时）。
    fn relaxed_deadline() -> Timestamp {
        Timestamp(10_000_000)
    }

    /// 读取解码输出的帧标记（透传通道的前 8 字节）。
    fn tag_of(frame: &DecodedFrame) -> u64 {
        match &frame.texture {
            FrameTexture::Cpu(cpu) => {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&cpu.data[..8]);
                u64::from_le_bytes(buf)
            }
            _ => panic!("假解码器只输出 CPU 纹理"),
        }
    }

    /// 任务卡 T05（D4）：缺参考帧时返回 NeedKeyframe 并置位标志，不 panic。
    #[test]
    fn fake_decoder_returns_need_keyframe_on_missing_ref() {
        // 场景一：入会时首帧 IDR 被丢（drop_first_n = 1）。
        let mut dec = FakeDecoder::new(FakeDecoderConfig {
            drop_first_n: 1,
            ..cfg()
        })
        .expect("配置合法");
        assert!(dec.needs_keyframe(), "从未解出 IDR：初始即缺关键帧");
        match dec
            .decode(&bitstream(0), FrameKind::Idr, relaxed_deadline())
            .expect("丢弃不是错误")
        {
            DecodeOutput::Nothing => {}
            other => panic!("被丢弃的帧应返回 Nothing，实际 {other:?}"),
        }
        assert!(dec.needs_keyframe(), "IDR 被丢后仍缺关键帧");
        // 后续 P 帧缺参考：NeedKeyframe，不 panic（D4）。
        assert!(matches!(
            dec.decode(&bitstream(1), FrameKind::P, relaxed_deadline()),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        assert!(dec.needs_keyframe());
        // 断链是粘性的：下一个 P 帧同样解不出。
        assert!(matches!(
            dec.decode(&bitstream(2), FrameKind::P, relaxed_deadline()),
            Ok(DecodeOutput::NeedKeyframe)
        ));
        // IDR 到达：链重建，正常出帧，标志清除。
        match dec
            .decode(&bitstream(3), FrameKind::Idr, relaxed_deadline())
            .expect("IDR 必须解出")
        {
            DecodeOutput::Frame(f) => assert_eq!((f.width, f.height), (64, 64)),
            other => panic!("IDR 应解出帧，实际 {other:?}"),
        }
        assert!(!dec.needs_keyframe(), "解出 IDR 后关键帧请求必须清除");
        assert!(matches!(
            dec.decode(&bitstream(4), FrameKind::P, relaxed_deadline()),
            Ok(DecodeOutput::Frame(_))
        ));

        // 场景二：中途丢一个 P 帧（第 1 帧的参考没送到）。
        let mut dec = FakeDecoder::new(FakeDecoderConfig {
            break_ref_chain_at: Some(1),
            ..cfg()
        })
        .expect("配置合法");
        assert!(matches!(
            dec.decode(&bitstream(0), FrameKind::Idr, relaxed_deadline()),
            Ok(DecodeOutput::Frame(_))
        ));
        assert!(!dec.needs_keyframe());
        assert!(
            matches!(
                dec.decode(&bitstream(1), FrameKind::P, relaxed_deadline()),
                Ok(DecodeOutput::NeedKeyframe)
            ),
            "参考丢失的 P 帧必须返回 NeedKeyframe，不 panic"
        );
        assert!(dec.needs_keyframe());
        assert!(
            matches!(
                dec.decode(&bitstream(2), FrameKind::P, relaxed_deadline()),
                Ok(DecodeOutput::NeedKeyframe)
            ),
            "断链持续到下一个 IDR"
        );
        assert!(matches!(
            dec.decode(&bitstream(3), FrameKind::Idr, relaxed_deadline()),
            Ok(DecodeOutput::Frame(_))
        ));
        assert!(!dec.needs_keyframe());
    }

    /// 任务卡 T05（D5）：进一出一，输出顺序 == 输入顺序。
    #[test]
    fn fake_decoder_outputs_in_order() {
        let cost = 3_000u32;
        let mut dec = FakeDecoder::new(FakeDecoderConfig {
            decode_cost_us: cost,
            ..cfg()
        })
        .expect("配置合法");
        let kinds = [FrameKind::Idr, FrameKind::P, FrameKind::P, FrameKind::P];
        let mut prev_done = None;
        for (i, &kind) in kinds.iter().enumerate() {
            let out = dec
                .decode(&bitstream(i as u64), kind, relaxed_deadline())
                .expect("链完整时必须解出");
            let frame = match out {
                DecodeOutput::Frame(f) => f,
                other => panic!("第 {i} 帧应进一出一，实际 {other:?}"),
            };
            // 标记透传：输出携带输入的帧序号 → 输出顺序 == 输入顺序。
            assert_eq!(
                tag_of(&frame),
                i as u64,
                "输出第 {i} 帧的标记必须与输入一致（不得乱序交付）"
            );
            let done = frame.decode_done_ts;
            assert_eq!(done, Timestamp(cost * (i as u32 + 1)), "完成时刻逐帧推进");
            if let Some(p) = prev_done {
                assert!(done > p, "完成时刻必须严格递增：{p:?} -> {done:?}");
            }
            prev_done = Some(done);
        }
        assert_eq!(dec.frames_output(), 4, "4 帧输入恰好 4 帧输出（进一出一）");
        assert_eq!(dec.frames_received(), 4);
    }

    /// 任务卡 T05：deadline 语义——预计完成超过截止返回 Timeout、不阻塞、
    /// 不改变内部状态；恰好赶上不算超时。
    #[test]
    fn decoder_deadline_timeout() {
        let cost = 5_000u32;
        let mut dec = FakeDecoder::new(FakeDecoderConfig {
            decode_cost_us: cost,
            ..cfg()
        })
        .expect("配置合法");
        let data = bitstream(0);

        // 完成时刻 5000 > deadline 3000 → 超时，携带超出的 2000µs。
        let err = dec
            .decode(&data, FrameKind::Idr, Timestamp(3_000))
            .expect_err("超过 deadline 必须报 Timeout");
        assert!(
            matches!(err, Error::Timeout(d) if d == Duration::from_micros(2_000)),
            "应返回携带超出时长的 Timeout，实际 {err:?}"
        );
        // 超时路径不消费帧、不推进时钟。
        assert_eq!(dec.frames_received(), 0);
        assert_eq!(dec.clock(), Timestamp(0));

        // 恰好赶上（完成时刻 == deadline）不算超时。
        let ok = dec
            .decode(&data, FrameKind::Idr, Timestamp(cost))
            .expect("完成时刻等于 deadline 必须成功");
        match ok {
            DecodeOutput::Frame(f) => assert_eq!(f.decode_done_ts, Timestamp(cost)),
            other => panic!("IDR 应解出帧，实际 {other:?}"),
        }
        assert_eq!(dec.clock(), Timestamp(cost));

        // 下一帧的完成时刻在上一帧基础上继续推进。
        let next = dec
            .decode(&bitstream(1), FrameKind::P, Timestamp(3 * cost))
            .expect("宽裕 deadline 必须成功");
        match next {
            DecodeOutput::Frame(f) => assert_eq!(f.decode_done_ts, Timestamp(2 * cost)),
            other => panic!("P 帧应解出，实际 {other:?}"),
        }
    }
}
