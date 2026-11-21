//! 控制端管线：重组 → 解码 → 呈现。
//!
//! 红线对应：
//! - P6：按 `frame_seq` 分组、按 `packet_idx` 排序，`packet_cnt` 齐了才交付；
//!   更新的完整帧到达后，序号更小的残帧永远凑不齐，判 `LostInNetwork`；
//! - P3：参考链断裂（`NeedKeyframe`）时置关键帧请求，会话层转发给 host；
//! - P4：残帧与解码失败都有明确结局，`shutdown`/截断时 [`finish_in_flight`]
//!   兜底，`unfinished == 0`。

use crate::{select_all, PipelineConfig};
use rc_decode::{DecodeOutput, DecodedFrame, Decoder};
use rc_proto::{MediaHeader, HEADER_LEN};
use rc_render::Renderer;
use rc_trace::{Mark, Outcome, TraceCollector};
use rc_types::{Error, FrameKind, FrameSeq, Result, Timestamp};
use std::collections::BTreeMap;

/// 重组中的帧。
struct Partial {
    cnt: u16,
    packets: Vec<Option<Vec<u8>>>,
    received: u16,
    capture_ts: Timestamp,
    /// 发送端声明的编码完成时刻（诊断对照用，重组逻辑不依赖）。
    #[allow(dead_code)]
    encode_done_ts: Timestamp,
    kind: FrameKind,
}

/// 控制端管线。
pub struct ControllerPipeline {
    cfg: PipelineConfig,
    decoder: Box<dyn Decoder>,
    renderer: Box<dyn Renderer>,
    trace: TraceCollector,
    /// 重组表（按帧序号有序，便于「新帧到达后清理旧残帧」）。
    partials: BTreeMap<FrameSeq, Partial>,
    /// 已交付的最高帧序号（其后的残帧仍在等待）。
    highest_delivered: Option<FrameSeq>,
    /// 已因过旧或超容量淘汰的最高帧序号（防止迟到分片复活已判 `LostInNetwork` 的帧）。
    highest_evicted: Option<FrameSeq>,
    /// 需要向对端请求关键帧（粘住直到成功解出一个 IDR）。
    need_keyframe: bool,
    /// 已上屏帧数（诊断）。
    displayed: u64,
    /// 会话时钟（微秒）。**管线是计时员**：`Decoded` / `Presented` 在操作
    /// 返回后读本时钟打点，而不是信任后端内部时钟——假后端各持独立
    /// 逻辑时钟，真后端的时间基准也不保证与会话同轴。`None` 时退化为
    /// 调用方传入的 `now_us`（decode/present 段退化为 0，诚实可解释）。
    clock: Option<std::sync::Arc<dyn Fn() -> u32 + Send + Sync>>,
}

/// 重组表的容量上限（对应 TASK.md 铁律 1：任意并发重组帧上限 = 2，与
/// `DEFAULT_MAX_CONCURRENT_FRAMES = 2` 一致）。超过即淘汰最旧残帧并记
/// `LostInNetwork`，禁止为了减少丢帧而调大。
const MAX_PARTIALS: usize = 2;

/// 解码 deadline 余量（微秒）：M1 无真实预算协商，给单帧解码的保守上限。
const DECODE_BUDGET_US: u32 = 3_000;

impl ControllerPipeline {
    /// 用工厂后端创建（auto 模式）。
    pub fn new(cfg: PipelineConfig) -> Result<Self> {
        cfg.validate()?;
        let (_capture, _encoder, decoder, renderer, labels) = select_all(&cfg)?;
        eprintln!(
            "rc-pipeline: controller backends decoder={} renderer={}",
            labels.decoder, labels.renderer
        );
        Self::with_backends(cfg, decoder, renderer)
    }

    /// 注入指定后端（测试 / 会话层显式控制）。
    pub fn with_backends(
        cfg: PipelineConfig,
        decoder: Box<dyn Decoder>,
        renderer: Box<dyn Renderer>,
    ) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            trace: TraceCollector::new(cfg.trace_capacity),
            decoder,
            renderer,
            cfg,
            partials: BTreeMap::new(),
            highest_delivered: None,
            highest_evicted: None,
            need_keyframe: false,
            displayed: 0,
            clock: None,
        })
    }

    /// 注入会话时钟（微秒，与帧 `timestamp` 同基准）。见字段文档。
    pub fn set_clock(&mut self, clock: std::sync::Arc<dyn Fn() -> u32 + Send + Sync>) {
        self.clock = Some(clock);
    }

    /// 读会话时钟；未注入时退化为调用方给的 `now_us`。
    fn now(&self, fallback_us: u32) -> Timestamp {
        Timestamp(self.clock.as_ref().map_or(fallback_us, |c| c()))
    }

    /// 投入一个收到的数据报。返回 `Ok(true)` 表示有新帧上屏。
    pub fn push_datagram(&mut self, data: &[u8], now_us: u32) -> Result<bool> {
        let (header, payload) = MediaHeader::parse_datagram(data)?;
        let seq = header.frame_seq;
        let idx = header.packet_idx;

        // 重组表里不该出现已交付或已淘汰序号更小/相等的帧到达（乱序迟到包）：
        // 直接丢弃，不复活已判 LostInNetwork 的帧（若尚未入 trace 则补记 LostInNetwork）。
        if let Some(hi) = self.highest_delivered {
            if seq <= hi {
                return Ok(false);
            }
        }
        if let Some(ev) = self.highest_evicted {
            if seq <= ev {
                self.mark_lost(seq, header.capture_ts);
                return Ok(false);
            }
        }

        let (received, cnt) = {
            let partial = self.partials.entry(seq).or_insert_with(|| Partial {
                cnt: header.packet_cnt,
                packets: vec![None; header.packet_cnt as usize],
                received: 0,
                capture_ts: header.capture_ts,
                encode_done_ts: header.encode_done_ts,
                kind: if header.flags.is_idr() {
                    FrameKind::Idr
                } else {
                    FrameKind::P
                },
            });
            if idx >= partial.cnt {
                return Err(Error::Protocol {
                    offset: 20,
                    msg: format!("packet_idx {idx} >= packet_cnt {}", partial.cnt),
                });
            }
            if partial.packets[idx as usize].is_none() {
                partial.packets[idx as usize] = Some(payload.to_vec());
                partial.received += 1;
            }
            (partial.received, partial.cnt)
        };
        // entry 借用已随块结束释放，evict_stale 才能再借 self。
        self.evict_stale(seq);

        if received < cnt {
            return Ok(false); // P6：缺包不交付
        }

        // 凑齐：按 packet_idx 拼接（坑 4）。remove 拿所有权。
        // 若本帧在容量裁剪中被当作最旧残帧淘汰（例如已有 2 个更新残帧在途时
        // 来了更旧的单包帧），evict_stale 已记 LostInNetwork，直接返回即可。
        let Some(partial) = self.partials.remove(&seq) else {
            return Ok(false);
        };
        let mut payload_all = Vec::with_capacity(
            partial
                .packets
                .iter()
                .map(|p| p.as_ref().map_or(0, |v| v.len()))
                .sum(),
        );
        for bytes in partial.packets.iter().flatten() {
            payload_all.extend_from_slice(bytes);
        }
        let capture_ts = partial.capture_ts;
        let kind = partial.kind;
        self.highest_delivered = Some(seq);

        let slot = self.trace.begin(seq);
        slot.mark(Mark::Captured, capture_ts);
        slot.mark(Mark::Received, Timestamp(now_us));

        // 解码。
        match self.decoder.decode(
            &payload_all,
            kind,
            Timestamp(now_us.saturating_add(DECODE_BUDGET_US)),
        ) {
            Ok(DecodeOutput::NeedKeyframe) => {
                self.need_keyframe = true;
                if let Some(f) = self.trace.get_mut(seq) {
                    f.finish(Outcome::DecodeError);
                }
                Ok(false)
            }
            Ok(DecodeOutput::Nothing) => {
                if let Some(f) = self.trace.get_mut(seq) {
                    f.finish(Outcome::DecodeError);
                }
                Ok(false)
            }
            Ok(DecodeOutput::Frame(decoded)) => self.decode_ok(seq, decoded, now_us),
            Err(e) => {
                if let Some(f) = self.trace.get_mut(seq) {
                    f.finish(Outcome::DecodeError);
                }
                if e.is_fatal() {
                    return Err(e);
                }
                Ok(false) // P7 对称：单帧解码失败不终止
            }
        }
    }

    /// 是否需要向对端请求关键帧（取走即清「请求」标志；参考链是否恢复
    /// 由解码器自己跟踪，解出 IDR 前它会让本标志再次置位）。
    pub fn take_keyframe_request(&mut self) -> bool {
        let kf = self.need_keyframe || self.decoder.needs_keyframe();
        self.need_keyframe = false;
        kf
    }

    /// 帧时间线（本端半场：Received→Presented + 传输/解码结局）。
    pub fn trace(&self) -> &TraceCollector {
        &self.trace
    }

    /// 已上屏帧数。
    pub fn displayed(&self) -> u64 {
        self.displayed
    }

    /// 会话收尾：在途残帧判 `LostInNetwork`，冲刷解码器。
    pub fn shutdown(&mut self) {
        self.finish_in_flight();
        let _ = self.decoder.flush();
    }

    /// 把仍在重组表的残帧全部判 `LostInNetwork`（截断兜底，保证
    /// `unfinished == 0`）。60 秒会话结束时在途最多个位数帧。
    pub fn finish_in_flight(&mut self) {
        let seqs: Vec<FrameSeq> = self.partials.keys().copied().collect();
        for seq in seqs {
            let p = self.partials.remove(&seq).expect("key just collected");
            self.highest_evicted = Some(self.highest_evicted.map_or(seq, |prev| prev.max(seq)));
            let f = self.trace.begin(seq);
            f.mark(Mark::Captured, p.capture_ts);
            // 无真实到达时刻，用采集时刻占位（该帧从未凑齐）。
            f.mark(Mark::Received, Timestamp(p.capture_ts.0));
            f.finish(Outcome::LostInNetwork);
        }
    }

    fn decode_ok(&mut self, seq: FrameSeq, decoded: DecodedFrame, now_us: u32) -> Result<bool> {
        // 解码同步完成：操作返回后读会话时钟打点。
        let decoded_at = self.now(now_us);
        let presented = self.renderer.present(decoded);
        // D8：present 返回后打点。渲染器返回的 Timestamp 只是参考值，
        // 管线统一用会话时钟（见 `clock` 字段文档）。
        let presented_at = self.now(now_us);
        presented?;
        if let Some(f) = self.trace.get_mut(seq) {
            f.mark(Mark::Decoded, decoded_at);
            f.mark(Mark::Presented, presented_at);
            f.finish(Outcome::Displayed);
        }
        self.displayed += 1;
        Ok(true)
    }

    /// 新帧 `upto` 到达后，序号更小的残帧永远凑不齐：判 LostInNetwork。
    /// 同时把重组表容量压回 `MAX_PARTIALS`（= 2）上限内。
    fn evict_stale(&mut self, upto: FrameSeq) {
        let stale: Vec<FrameSeq> = self.partials.range(..upto).map(|(k, _)| *k).collect();
        for seq in stale {
            let p = self.partials.remove(&seq).expect("key from range");
            self.mark_lost(seq, p.capture_ts);
        }
        while self.partials.len() > MAX_PARTIALS {
            let oldest = *self.partials.keys().next().expect("non-empty");
            let p = self.partials.remove(&oldest).expect("key exists");
            self.mark_lost(oldest, p.capture_ts);
        }
        debug_assert!(self.partials.len() <= MAX_PARTIALS);
    }

    /// 淘汰残帧：更新已淘汰水位线，并幂等记录 `LostInNetwork`（同一 `seq` 不重复 `begin`）。
    fn mark_lost(&mut self, seq: FrameSeq, capture_ts: Timestamp) {
        self.highest_evicted = Some(self.highest_evicted.map_or(seq, |prev| prev.max(seq)));
        if let Some(f) = self.trace.get_mut(seq) {
            f.mark(Mark::Captured, capture_ts);
            f.finish(Outcome::LostInNetwork);
        } else {
            let slot = self.trace.begin(seq);
            slot.mark(Mark::Captured, capture_ts);
            slot.finish(Outcome::LostInNetwork);
        }
    }

    /// 配置引用（会话层取分辨率/容量等）。
    pub fn config(&self) -> &PipelineConfig {
        &self.cfg
    }
}

/// HEADER_LEN 再导出（诊断输出用）。
pub const HEADER_BYTES: usize = HEADER_LEN;
