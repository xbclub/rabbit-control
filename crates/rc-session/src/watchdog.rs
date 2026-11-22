//! IDR 停滞恢复闭环（T16）：完整帧跟踪、停滞看门狗与 IdrRequest 去抖。
//!
//! # 为什么存在
//!
//! 媒体走 QUIC datagram 是「不可靠的可靠」：丢包宁可丢帧也不等待（T09）。
//! 丢帧的代价是解码参考链断裂后的画面停滞——P 帧缺一片整帧作废（rc-pipeline
//! controller.rs 的重组语义），要等下一个 IDR 才能恢复；而 IDR 本身也走
//! datagram，同样会被丢。本模块补齐恢复闭环缺的三块：
//!
//! - [`DeliveryTracker`]：client 侧完整帧到达跟踪（只记元数据不存负载）。
//!   `ControllerPipeline::push_datagram` 只返回 `Ok(bool)`，交付帧的
//!   seq / kind 是管线私有状态——看门狗的「最近完整帧时刻」与 IdrAck 的
//!   按 frame_seq 关联确认都拿不到。本类型在会话层顺带解析数据报头部补上
//!   这个可见性，管线 API 零改动（W8）；
//! - [`StallWatchdog`]：超阈值无完整帧 → 触发一次（阈值 500ms 与 rc-net
//!   重组层的残帧放弃线 `FRAME_STALE_US` 刻意对齐：残帧被判死的同一时刻，
//!   看门狗接管恢复）；
//! - [`IdrController`]：IdrRequest 去抖 / IdrAck 按 frame_seq 关联确认 /
//!   超时复核（在途不重发 W3；重发受最小间隔地板约束 W4）。
//!
//! # 时间纪律（铁律 4）
//!
//! 本模块不读任何时钟（无 `Instant::now` / `SystemTime` / `thread::sleep`）：
//! 所有时间参数都是调用方注入的会话钟微秒（`u32`，71.6 分钟回绕）。elapsed
//! 一律 [`u32::wrapping_sub`]（W7，对齐 rc-net 重组层的回绕免疫写法，
//! reassemble.rs 的 gc 先例）。

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};

use rc_types::{FrameSeq, Result};

use crate::proto::IdrReason;

/// 在途残帧表上限（铁律 1：对齐 `ControllerPipeline` 的 `MAX_PARTIALS`，
/// controller.rs）。超出淘汰最旧（与管线重组表同一策略），禁止为减少
/// 丢帧而调大。
const MAX_TRACKED_PARTIALS: usize = 2;

/// 残帧放弃线（微秒）：超时未凑齐的帧被 [`DeliveryTracker::gc`] 移出
/// 在途表。与 rc-net 重组层 `FRAME_STALE_US`（reassemble.rs，500_000）
/// 及 [`DEFAULT_STALL_US`] 是**刻意对齐**的一对（已知坑 5：残帧被判死
/// 的同一时刻看门狗接管恢复，改一个必须同步另一个）。
const PARTIAL_STALE_US: u32 = 500_000;

/// 完成历史容量（有界环形）：只服务「IDR 帧先到、ack 后到」的关联查询
/// （坑 4），不做无限留存。500ms 级窗口内滚出 8 帧历史意味着流已恢复，
/// 闭环无意义。
const COMPLETED_HISTORY: usize = 8;

/// 帧等待超时基数（微秒）：ack 已到、等对应 IDR 帧完整到达的窗口 =
/// `rtt + ack_margin + 本值`。取值对齐看门狗节拍（500ms）：IDR 数据报
/// 被丢时反正 500ms 后看门狗会再触发一轮恢复，比它更短的帧超时只会把
/// 重发频率抬到高于看门狗节拍（持续丢包演练的请求上界约束），比它长
/// 则拖慢恢复（演练断言丢包窗结束后 ≤700ms 恢复上屏）。
const FRAME_WAIT_BASE_US: u32 = 500_000;

// ── 完整帧跟踪 ──────────────────────────────────────────────

/// 最近一次完整到达的帧（或完成历史中的一条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveredFrame {
    /// 帧序号。
    pub seq: FrameSeq,
    /// 是否 IDR（来自包头 flags）。
    pub is_idr: bool,
    /// 凑齐 packet_cnt 的时刻（会话钟，注入值）。
    pub at_us: u32,
}

/// 一个未凑齐帧的元数据状态（不存负载）。
struct Partial {
    /// 总分片数（来自包头，同一帧的所有分片一致）。
    packet_cnt: u16,
    /// 已收到的分片数。
    received: u16,
    /// 是否 IDR。
    is_idr: bool,
    /// 首个分片到达时刻（注入时钟；gc 判超时用）。
    first_seen_us: u32,
    /// 按 packet_idx 标记已到分片（同一 (seq, idx) 重复到达幂等）。
    seen: Vec<bool>,
}

impl Default for DeliveryTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// client 侧完整帧到达跟踪器：只记元数据（frame_seq / packet_idx /
/// 是否 IDR），**不存负载**。
///
/// 铁律 1：在途残帧表上限 2（对齐 `ControllerPipeline` 的 `MAX_PARTIALS`），
/// 超出淘汰最旧；完成历史是有界环形（容量 8）。
pub struct DeliveryTracker {
    /// 在途残帧，key = frame_seq.0（BTreeMap：淘汰最旧 = 最小 key）。
    partials: BTreeMap<u64, Partial>,
    /// 已结案（交付或淘汰/gc）的最大帧序号：更老帧的迟到分片直接忽略，
    /// 不复活已结案的帧（对齐 controller.rs 的水位判定）。
    highest_retired: Option<u64>,
    /// 完成历史（环形，容量 COMPLETED_HISTORY，新的在尾）。
    completed: VecDeque<DeliveredFrame>,
    /// 最近一次完整帧。
    last_complete: Option<DeliveredFrame>,
}

impl DeliveryTracker {
    /// 构造（空表）。
    pub fn new() -> Self {
        Self {
            partials: BTreeMap::new(),
            highest_retired: None,
            completed: VecDeque::new(),
            last_complete: None,
        }
    }

    /// 观察一个到达的数据报（会话层在 `push_datagram` 之前对同一字节
    /// 调用；内部经 `rc_pipeline::host::datagram_header` 解析，不引入
    /// rc-proto 直接依赖）。同一 (seq, idx) 重复到达幂等。凑齐
    /// packet_cnt 时记入完成历史并推进 [`Self::last_complete`]。
    ///
    /// 头部非法返回 `Err`（与管线自己的 `push_datagram` 判定一致，交给
    /// 会话层的错误策略处理）。
    pub fn observe(&mut self, datagram: &[u8], now_us: u32) -> Result<()> {
        let h = rc_pipeline::host::datagram_header(datagram)?;
        let seq = h.frame_seq.0;
        // 已结案帧的迟到分片：忽略（不复活，不占在途名额）。
        if self.highest_retired.is_some_and(|r| seq <= r) {
            return Ok(());
        }
        let idx = usize::from(h.packet_idx);
        if let Entry::Vacant(slot) = self.partials.entry(seq) {
            let mut partial = Partial {
                packet_cnt: h.packet_cnt,
                received: 0,
                is_idr: h.is_idr(),
                first_seen_us: now_us,
                seen: vec![false; usize::from(h.packet_cnt)],
            };
            Self::mark_seen(&mut partial, idx)?;
            slot.insert(partial);
            self.evict_overflow();
        }
        let complete = {
            let Some(partial) = self.partials.get_mut(&seq) else {
                // 刚插入就被容量淘汰（更旧的单包帧撞满上限，对齐
                // controller.rs 场景 B 的语义）：按「未到达」处理。
                return Ok(());
            };
            Self::mark_seen(partial, idx)?;
            partial.received >= partial.packet_cnt
        };
        if complete {
            let partial = self
                .partials
                .remove(&seq)
                .expect("complete implies present");
            let frame = DeliveredFrame {
                seq: FrameSeq(seq),
                is_idr: partial.is_idr,
                at_us: now_us,
            };
            self.completed.push_back(frame);
            if self.completed.len() > COMPLETED_HISTORY {
                self.completed.pop_front();
            }
            self.last_complete = Some(frame);
            self.retire(seq);
        }
        Ok(())
    }

    /// 标记一个分片已到；idx 越界（后续分片声明的 cnt 与首片不一致）
    /// 返回 `Err(Protocol)`（对齐 controller.rs 的 push_datagram 判定）。
    fn mark_seen(partial: &mut Partial, idx: usize) -> Result<()> {
        if idx >= partial.seen.len() {
            return Err(rc_types::Error::Protocol {
                offset: 20,
                msg: format!("packet_idx {idx} >= packet_cnt {}", partial.packet_cnt),
            });
        }
        if !partial.seen[idx] {
            partial.seen[idx] = true;
            partial.received += 1;
        }
        Ok(())
    }

    /// 在途表超容量：淘汰最旧（最小 key），对齐 controller.rs 的
    /// evict_stale 语义。
    fn evict_overflow(&mut self) {
        while self.partials.len() > MAX_TRACKED_PARTIALS {
            let oldest = *self
                .partials
                .keys()
                .next()
                .expect("len > limit implies key");
            self.partials.remove(&oldest);
            self.retire(oldest);
        }
    }

    /// 把 seq 记入已结案水位。
    fn retire(&mut self, seq: u64) {
        if self.highest_retired.map_or(true, |r| seq > r) {
            self.highest_retired = Some(seq);
        }
    }

    /// 最近一次完整帧（任意类型）。看门狗的数据源。
    pub fn last_complete(&self) -> Option<DeliveredFrame> {
        self.last_complete
    }

    /// 查询指定 seq 是否已完整到达（ack 后到时的关联确认）。已滚出环形
    /// 历史的旧帧返回 `None`（调用方按「未到达」处理即可——500ms 级窗口
    /// 内滚出 8 帧历史意味着流已恢复，闭环无意义）。
    pub fn completed(&self, seq: FrameSeq) -> Option<DeliveredFrame> {
        self.completed.iter().rev().find(|f| f.seq == seq).copied()
    }

    /// `since_us` 之后（含）完整到达的首个 IDR 帧（T18 恢复计时的数据
    /// 源）。不能只看 [`Self::last_complete`]：负载下一拍可能批量凑齐
    /// 多帧，强制的 IDR 被同拍更晚的 delta 覆盖，恢复计时会竞态性永远
    /// 等不到。时刻判定用 [`u32::wrapping_sub`]（W7 回绕免疫）：早于
    /// since 的旧帧 elapsed 为回绕大值，被半程窗口排除。
    pub fn first_idr_since(&self, since_us: u32) -> Option<DeliveredFrame> {
        self.completed
            .iter()
            .find(|f| f.is_idr && f.at_us.wrapping_sub(since_us) < 0x8000_0000)
            .copied()
    }

    /// 残帧清理：超时（[`PARTIAL_STALE_US`]，严格大于）未凑齐的帧移出
    /// 在途表并结案。时间比较用 [`u32::wrapping_sub`]（会话钟 71.6 分钟
    /// 回绕免疫，W7）。
    pub fn gc(&mut self, now_us: u32) {
        let stale: Vec<u64> = self
            .partials
            .iter()
            .filter(|(_, p)| now_us.wrapping_sub(p.first_seen_us) > PARTIAL_STALE_US)
            .map(|(&seq, _)| seq)
            .collect();
        for seq in stale {
            self.partials.remove(&seq);
            self.retire(seq);
        }
    }

    /// 当前在途残帧数（诊断；铁律 1 断言 ≤2 用）。
    pub fn in_flight(&self) -> usize {
        self.partials.len()
    }
}

// ── 停滞看门狗 ──────────────────────────────────────────────

/// 看门狗阈值（微秒）：与 rc-net 重组层的残帧放弃线 `FRAME_STALE_US`
/// （reassemble.rs，500_000）**刻意对齐**——残帧被判死的同一时刻，看门狗
/// 接管恢复。改一个必须同步另一个（见已知坑 5）。
pub const DEFAULT_STALL_US: u32 = 500_000;

/// client 停滞看门狗：超阈值无完整帧 → 触发一次（会话层把它转成
/// [`IdrController::request`](IdrReason::WatchdogStall)，是否真发请求由
/// 后者裁决）。
pub struct StallWatchdog {
    /// 触发阈值（微秒）。
    stall_after_us: u32,
    /// 基线：最近一次完整帧时刻（或 arm 基线）。
    baseline_us: u32,
    /// 是否已 arm（Streaming 进入前不触发）。
    armed: bool,
    /// 累计触发次数。
    fires: u64,
}

impl StallWatchdog {
    /// 构造。`stall_after_us` 传 [`DEFAULT_STALL_US`]。
    pub fn new(stall_after_us: u32) -> Self {
        Self {
            stall_after_us,
            baseline_us: 0,
            armed: false,
            fires: 0,
        }
    }

    /// 进入 Streaming 状态时锚定基线（状态机转入时调用一次）。首帧迟到
    /// （如首个 IDR 被丢）同样能触发恢复——不要等首帧到达才武装。
    pub fn arm(&mut self, now_us: u32) {
        self.baseline_us = now_us;
        self.armed = true;
    }

    /// 完整帧到达（[`DeliveryTracker`] 凑齐时转发）。
    pub fn on_frame_complete(&mut self, now_us: u32) {
        self.baseline_us = now_us;
    }

    /// 每拍驱动：距最近完整帧（或 arm 基线）超过阈值（严格大于，对齐
    /// rc-net 重组层的 gc 语义）返回 true。触发后**自锚定**（基线推进到
    /// 触发时刻）：同一段停滞每 500ms 至多报一次，直到下一帧到达重新武装。
    pub fn poll(&mut self, now_us: u32) -> bool {
        if !self.armed {
            return false;
        }
        if now_us.wrapping_sub(self.baseline_us) > self.stall_after_us {
            self.baseline_us = now_us;
            self.fires += 1;
            true
        } else {
            false
        }
    }

    /// 累计触发次数（报告 `watchdog_fires` 数据源，JSON 组装归 T18）。
    pub fn fires(&self) -> u64 {
        self.fires
    }
}

// ── IdrRequest 去抖 / IdrAck 关联确认 ───────────────────────

/// 超时复核余量（微秒）：timeout = rtt_us + margin。覆盖单帧周期
/// （60fps ≈ 16.7ms）+ 编码（~10ms 量级）+ 调度余量。
pub const DEFAULT_ACK_MARGIN_US: u32 = 50_000;

/// 重发最小间隔地板（微秒）：静态画面下（铁律 2 合法零帧）请求-超时-
/// 重发循环必然存在，此地板把它的频率压到 ≤10 次/秒（每条 2 字节
/// reliable 消息）。
pub const DEFAULT_MIN_RETRY_INTERVAL_US: u32 = 100_000;

/// IdrController 配置。
#[derive(Debug, Clone, Copy)]
pub struct IdrControllerConfig {
    /// 等待 IdrAck 的超时余量（timeout = rtt_us + 本值）。默认
    /// [`DEFAULT_ACK_MARGIN_US`]。
    pub ack_margin_us: u32,
    /// 重发最小间隔地板。默认 [`DEFAULT_MIN_RETRY_INTERVAL_US`]。
    pub min_retry_interval_us: u32,
}

impl Default for IdrControllerConfig {
    fn default() -> Self {
        Self {
            ack_margin_us: DEFAULT_ACK_MARGIN_US,
            min_retry_interval_us: DEFAULT_MIN_RETRY_INTERVAL_US,
        }
    }
}

/// [`IdrController::poll`] 的产出：会话层应发出一条
/// `ControlMessage::IdrRequest { reason }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendIdrRequest {
    /// 请求原因：0=DecoderNeedKeyframe（解码器）、1=WatchdogStall（看门狗）；
    /// 2=ReconnectRecovery 属 T17，本卡不发送。
    pub reason: IdrReason,
    /// 第几次尝试（1 起）。测试与报告用。
    pub attempt: u8,
}

/// 本卡计数（client 报告 idr_requests / idr_acks / watchdog_fires 的数据源）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdrStats {
    /// 累计发出的 IdrRequest 条数（首次 + 重发）。
    pub requests_sent: u64,
    /// 按原因分计数，下标 = IdrReason 线值（0/1/2）。
    pub requests_by_reason: [u64; 3],
    /// 超时重发次数。
    pub retries: u64,
    /// 有效消费的 IdrAck 条数（重复/过期 ack 幂等不计）。
    pub acks_received: u64,
    /// 「等待中的 IDR 帧完整到达」而闭环的次数。
    pub resolved_by_idr: u64,
}

/// 去抖状态机的私有状态。
enum IdrState {
    /// 无在途请求。
    Idle,
    /// 已发请求等 ack。
    InFlight {
        /// 本次（重）发时刻。
        sent_at: u32,
        /// 第几次尝试。
        attempt: u8,
    },
    /// ack 已到，等对应 IDR 帧完整到达。
    AwaitFrame {
        /// host 确认编出的 IDR 帧序号。
        seq: FrameSeq,
        /// ack 到达时刻（帧等待超时起点）。
        since: u32,
        /// 本次尝试的发出时刻（重发地板起点）。
        sent_at: u32,
        /// 第几次尝试。
        attempt: u8,
    },
}

/// IdrRequest 去抖 / IdrAck 关联确认状态机（client 侧）。
///
/// 状态：`Idle` →（触发）→ `InFlight{sent_at, attempt}`（已发请求等 ack）
/// →（on_ack(seq)）→ `AwaitFrame{seq, since, attempt}`（等该 IDR 帧完整
/// 到达）→（on_frame_complete 且 seq 匹配且 is_idr）→ `Idle`。
///
/// - 两条超时路径都回到 `InFlight` 重发（attempt+1）：等 ack超
///   `rtt + ack_margin`；ack 到了但对应 IDR 帧超时未到（IDR 数据报被丢，
///   窗口 = `rtt + ack_margin + FRAME_WAIT_BASE_US`）。重发额外受
///   `min_retry_interval_us` 地板约束（W4）。
/// - **在途（InFlight/AwaitFrame）期间一切新触发被吞**（去抖红线 W3）；
///   闭环时清空被吞记录——刚到的 IDR 已覆盖所有原因，持续性需求（解码器
///   仍请求 DecoderNeedKeyframe）下一拍会经 `take_keyframe_request` 自然
///   重触发。
/// - IDR 走 datagram、ack 走 reliable，**两通道无序**：ack 晚于帧到达是
///   常态。本类型在 on_frame_complete 里顺带记住最近完整到达的 IDR 帧，
///   on_ack 命中即立即闭环（等价于契约所述「先查 completed」，W5）。
pub struct IdrController {
    /// 配置。
    cfg: IdrControllerConfig,
    /// 去抖状态机。
    state: IdrState,
    /// Idle 时待发送的触发（request 只记意图，发送全部经 poll）。
    pending: Option<IdrReason>,
    /// 在途期间被吞的触发（闭环时清空，不补发）。
    swallowed: Option<IdrReason>,
    /// 本轮请求的原因（重发沿用）。
    reason: Option<IdrReason>,
    /// 最近完整到达的 IDR 帧（帧先到、ack 后到的关联查询用）。
    last_completed_idr: Option<DeliveredFrame>,
    /// 计数。
    stats: IdrStats,
}

impl IdrController {
    /// 构造。
    pub fn new(cfg: IdrControllerConfig) -> Self {
        Self {
            cfg,
            state: IdrState::Idle,
            pending: None,
            swallowed: None,
            reason: None,
            last_completed_idr: None,
            stats: IdrStats::default(),
        }
    }

    /// 外部触发（解码器 DecoderNeedKeyframe；看门狗 WatchdogStall）。
    /// 在途时记入待清空记录（吞掉，不产生发送）。
    pub fn request(&mut self, reason: IdrReason, now_us: u32) {
        // 时间不参与判定（只记意图）；参数保留以对齐契约签名。
        let _ = now_us;
        match self.state {
            IdrState::Idle => self.pending = Some(reason),
            IdrState::InFlight { .. } | IdrState::AwaitFrame { .. } => {
                self.swallowed = Some(reason);
            }
        }
    }

    /// 收到 `ControlMessage::IdrAck { frame_seq }`。重复/过期 ack 幂等
    /// 无副作用。InFlight 时若该帧已完整到达（两通道无序，帧先到）则
    /// 立即闭环，否则转入 AwaitFrame 等帧。
    pub fn on_ack(&mut self, frame_seq: FrameSeq, now_us: u32) {
        match self.state {
            IdrState::InFlight { attempt, sent_at } => {
                self.stats.acks_received += 1;
                if self.last_completed_idr.is_some_and(|f| f.seq == frame_seq) {
                    // 帧先到（IDR 数据报早于 ack 抵达）：立即闭环。
                    self.close_loop();
                } else {
                    self.state = IdrState::AwaitFrame {
                        seq: frame_seq,
                        since: now_us,
                        sent_at,
                        attempt,
                    };
                }
            }
            // 重复 ack（同 seq）或过期 ack（别的轮次）：幂等忽略。
            IdrState::AwaitFrame { .. } | IdrState::Idle => {}
        }
    }

    /// 完整帧到达（[`DeliveryTracker`] 凑齐时转发）。AwaitFrame 且 seq
    /// 匹配且 is_idr 时闭环；同时记住最近完整到达的 IDR 帧供 on_ack 查询。
    pub fn on_frame_complete(&mut self, frame: DeliveredFrame) {
        if frame.is_idr {
            self.last_completed_idr = Some(frame);
        }
        if let IdrState::AwaitFrame { seq, .. } = self.state {
            if seq == frame.seq && frame.is_idr {
                self.close_loop();
            }
        }
    }

    /// 每拍超时复核与**唯一发送出口**（`request` 只记意图，发送全部经此）。
    /// `rtt_us` 注入自 `NetStats::rtt_us`（不依赖 T15 的 ProbePing）。
    pub fn poll(&mut self, now_us: u32, rtt_us: u32) -> Option<SendIdrRequest> {
        match self.state {
            IdrState::Idle => {
                let reason = self.pending.take()?;
                self.send(reason, now_us, 1)
            }
            IdrState::InFlight { sent_at, attempt } => {
                // 等 ack 超时 = rtt + margin（严格大于）；重发再受地板约束。
                let elapsed = now_us.wrapping_sub(sent_at);
                let ack_timeout = rtt_us.saturating_add(self.cfg.ack_margin_us);
                if elapsed > ack_timeout && elapsed >= self.cfg.min_retry_interval_us {
                    let reason = self.reason.expect("in-flight implies reason");
                    self.stats.retries += 1;
                    self.send(reason, now_us, attempt.saturating_add(1))
                } else {
                    None
                }
            }
            IdrState::AwaitFrame {
                seq: _,
                since,
                sent_at,
                attempt,
            } => {
                // ack 到了但对应 IDR 帧超时未到（IDR 数据报被丢）：
                // 窗口 = rtt + margin + 帧等待基数；地板以距上次发送计。
                let frame_timeout = rtt_us
                    .saturating_add(self.cfg.ack_margin_us)
                    .saturating_add(FRAME_WAIT_BASE_US);
                let since_elapsed = now_us.wrapping_sub(since);
                let sent_elapsed = now_us.wrapping_sub(sent_at);
                if since_elapsed > frame_timeout && sent_elapsed >= self.cfg.min_retry_interval_us {
                    let reason = self.reason.expect("in-flight implies reason");
                    self.stats.retries += 1;
                    self.send(reason, now_us, attempt.saturating_add(1))
                } else {
                    None
                }
            }
        }
    }

    /// 计数快照。
    pub fn stats(&self) -> IdrStats {
        self.stats
    }

    /// 唯一发送路径：转入 InFlight 并计数。
    fn send(&mut self, reason: IdrReason, now_us: u32, attempt: u8) -> Option<SendIdrRequest> {
        self.stats.requests_sent += 1;
        self.stats.requests_by_reason[reason as usize] += 1;
        self.reason = Some(reason);
        self.state = IdrState::InFlight {
            sent_at: now_us,
            attempt,
        };
        Some(SendIdrRequest { reason, attempt })
    }

    /// 闭环：回 Idle、清空被吞记录与本轮原因。
    fn close_loop(&mut self) {
        self.state = IdrState::Idle;
        self.swallowed = None;
        self.reason = None;
        self.stats.resolved_by_idr += 1;
    }
}

// ── host 侧 ─────────────────────────────────────────────────

/// 从 `HostPipeline::tick` 一拍产出的数据报里扫出 IDR 帧序号（保序去重：
/// 一个跨多片的 IDR 只出一个 seq）。host 会话在把这批数据报
/// `send_datagram` 完之后、**同一循环体内**对每个 seq 发一条
/// `ControlMessage::IdrAck { frame_seq }`（经 `send_reliable`）——
/// 「IDR 帧编码完成送出数据报的同一 tick 发出」的落地（W5）。
/// 纯函数，独立可测。解析失败的数据报跳过（host 产出的必合法，防御）。
pub fn idr_frame_seqs(datagrams: &[Vec<u8>]) -> Vec<FrameSeq> {
    let mut out = Vec::new();
    for d in datagrams {
        if let Ok(h) = rc_pipeline::host::datagram_header(d) {
            if h.is_idr() && !out.contains(&h.frame_seq) {
                out.push(h.frame_seq);
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────
// 测试：全部注入时间、离线确定性（无网络、无墙钟、无 sleep）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::{split_frame, MAX_PAYLOAD};
    use rc_types::{FrameKind, Timestamp};

    /// 造一帧的分片：payload 切成 `chunk` 大小的片。
    fn frame_packets(seq: u64, kind: FrameKind, payload: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        split_frame(
            FrameSeq(seq),
            kind,
            Timestamp(1_000),
            Timestamp(2_000),
            0,
            payload,
            chunk,
        )
    }

    // ── DeliveryTracker ─────────────────────────────────────

    /// 多片乱序 + 重复投递幂等；凑齐才进完成历史，`last_complete` 推进。
    #[test]
    fn tracker_completes_frame_only_when_all_packets_arrive() {
        let mut t = DeliveryTracker::new();
        let pkts = frame_packets(7, FrameKind::Idr, &[0xAB; 300], 128);
        assert_eq!(pkts.len(), 3, "300B 按 128 分片为 3 包");

        // 乱序：先到中间片。
        t.observe(&pkts[1], 100).expect("observe");
        assert!(t.last_complete().is_none(), "1/3 片不能算完整");
        assert!(t.completed(FrameSeq(7)).is_none());
        assert_eq!(t.in_flight(), 1);

        // 重复投递同一 (seq, idx)：幂等。
        t.observe(&pkts[1], 110).expect("observe");
        assert_eq!(t.in_flight(), 1);

        t.observe(&pkts[0], 120).expect("observe");
        assert!(t.last_complete().is_none(), "2/3 片仍不完整");

        // 补齐第 3 片：帧 7 完整。
        t.observe(&pkts[2], 130).expect("observe");
        let f = t.last_complete().expect("凑齐必须完整");
        assert_eq!(f.seq, FrameSeq(7));
        assert!(f.is_idr, "包头 flags 带 IDR 位");
        assert_eq!(f.at_us, 130);
        assert_eq!(t.completed(FrameSeq(7)).unwrap().seq, FrameSeq(7));
        assert_eq!(t.in_flight(), 0, "完成后移出在途表");

        // 已完成帧的重复分片：幂等，不再推进。
        t.observe(&pkts[0], 140).expect("observe");
        assert_eq!(t.last_complete().unwrap().at_us, 130);
        assert_eq!(t.in_flight(), 0);

        // 单包 P 帧对照：一次到达即完整，last_complete 推进。
        let single = frame_packets(8, FrameKind::P, &[0xCD; 64], MAX_PAYLOAD);
        t.observe(&single[0], 150).expect("observe");
        let f = t.last_complete().expect("单包帧立即完整");
        assert_eq!(f.seq, FrameSeq(8));
        assert!(!f.is_idr, "P 帧不是 IDR");
        assert_eq!(f.at_us, 150);
        // 帧 7 仍在完成历史（ack 后到时的关联查询）。
        assert!(t.completed(FrameSeq(7)).is_some());

        // 被新帧超越的旧帧分片（帧 7 已结案、水位 8）：直接忽略，
        // 不复活（对齐 controller.rs 的 highest_delivered 判定）。
        t.observe(&pkts[2], 160).expect("observe");
        assert_eq!(t.in_flight(), 0);
    }

    /// 门禁第 2 轮全仓测试偶发失败根因（T18 恢复计时竞态）：同一拍批量
    /// 凑齐「IDR + 更晚的 delta」时 `last_complete` 被 delta 覆盖，
    /// `first_idr_since` 必须仍能从完成历史找到 since 之后的首个 IDR；
    /// 早于 since 的旧 IDR（断线前残留）必须被排除；时刻判定跨 u32
    /// 回绕不失效（W7）。
    #[test]
    fn tracker_first_idr_since_finds_idr_under_same_tick_burst() {
        let mut t = DeliveryTracker::new();
        // 断线前残留的旧 IDR（seq 7，早于 since）——不得被当作恢复帧。
        let old_idr = frame_packets(7, FrameKind::Idr, &[0xAB; 64], MAX_PAYLOAD);
        t.observe(&old_idr[0], 1_000_000).expect("observe");
        // since = 2_000_000（resume 时刻）。
        // 同拍突发：强制 IDR（seq 8）+ 更晚的 delta（seq 9）。
        let idr = frame_packets(8, FrameKind::Idr, &[0xCD; 64], MAX_PAYLOAD);
        let delta = frame_packets(9, FrameKind::P, &[0xEE; 64], MAX_PAYLOAD);
        t.observe(&idr[0], 2_050_000).expect("observe");
        t.observe(&delta[0], 2_050_000).expect("observe");
        // last_complete 已是 delta：只看最新一帧的旧实现在此永远等不到
        // IDR（恢复计时挂死 → e2e 报 recovery_ms 为 null）。
        assert!(!t.last_complete().expect("有完整帧").is_idr);
        let f = t
            .first_idr_since(2_000_000)
            .expect("突发覆盖下仍须找到恢复 IDR");
        assert_eq!(f.seq, FrameSeq(8), "取 since 之后的首个 IDR，不是旧 IDR");
        assert_eq!(f.at_us, 2_050_000);

        // 全部早于 since：None（旧 IDR 不复活）。
        assert!(t.first_idr_since(3_000_000).is_none());

        // 回绕：since 在 u32::MAX 附近，IDR 完成于回绕之后——wrapping
        // 判定仍算「之后」。
        let mut w = DeliveryTracker::new();
        let near_wrap = frame_packets(20, FrameKind::Idr, &[1u8; 64], MAX_PAYLOAD);
        w.observe(&near_wrap[0], 100).expect("observe");
        let f = w
            .first_idr_since(u32::MAX - 10_000)
            .expect("回绕后到达仍是 since 之后");
        assert_eq!(f.seq, FrameSeq(20));
    }

    /// 3 个并发残帧 → 最旧被逐，在途表 ≤2（W6/铁律 1）。
    #[test]
    fn tracker_bounded_inflight_drops_oldest() {
        let mut t = DeliveryTracker::new();
        let f12 = frame_packets(12, FrameKind::P, &[1u8; 200], 100);
        let f11 = frame_packets(11, FrameKind::P, &[2u8; 200], 100);
        let f10 = frame_packets(10, FrameKind::P, &[3u8; 200], 100);
        assert_eq!(f12.len(), 2);

        t.observe(&f12[0], 12_500).expect("observe");
        t.observe(&f11[0], 13_000).expect("observe");
        assert_eq!(t.in_flight(), 2, "2 个并发残帧在上限内");
        assert!(t.last_complete().is_none());

        // 第 3 个并发残帧（最旧的 10 后到）→ 10 插入即被容量淘汰。
        t.observe(&f10[0], 13_500).expect("observe");
        assert_eq!(t.in_flight(), 2, "在途表必须 ≤2");
        // 被逐帧的迟到分片不得复活。
        t.observe(&f10[1], 14_000).expect("observe");
        assert_eq!(t.in_flight(), 2, "被淘汰帧的分片不得复活占位");
        assert!(t.completed(FrameSeq(10)).is_none(), "被逐帧永不完整");

        // 表内 11、12 凑齐后正常完成。
        t.observe(&f11[1], 20_000).expect("observe");
        t.observe(&f12[1], 21_000).expect("observe");
        assert_eq!(t.last_complete().unwrap().seq, FrameSeq(12));
        assert_eq!(t.in_flight(), 0);
    }

    /// 构造跨 u32::MAX 的时刻，`gc` 用 wrapping 比较不误杀/不漏杀（W7）。
    #[test]
    fn tracker_gc_evicts_stale_partials_across_wrap() {
        let mut t = DeliveryTracker::new();
        let t0 = u32::MAX - 10_000; // 回绕前 10ms
        let pkts = frame_packets(77, FrameKind::Idr, &[0xAB; 200], 100);
        assert_eq!(pkts.len(), 2);

        t.observe(&pkts[0], t0).expect("observe");
        assert_eq!(t.in_flight(), 1);

        // 跨回绕后恰好 500ms：不逐（严格大于才逐），回绕不得影响判定。
        t.gc(t0.wrapping_add(PARTIAL_STALE_US));
        assert_eq!(t.in_flight(), 1, "恰好 500ms 不逐");
        // 501ms：必须逐（saturating_sub 版本在回绕点会把经过时间算成 0，
        // 永远不逐——这正是要防的回绕 bug）。
        t.gc(t0.wrapping_add(PARTIAL_STALE_US).wrapping_add(1));
        assert_eq!(t.in_flight(), 0, "跨回绕的超时判定仍正确");
        // 被逐帧的迟到分片不复活。
        t.observe(&pkts[1], t0.wrapping_add(600_000))
            .expect("observe");
        assert_eq!(t.in_flight(), 0);
        // gc 只清残帧，不冻结状态机：更新的帧照常接受。
        let next = frame_packets(78, FrameKind::P, &[4u8; 200], 100);
        t.observe(&next[0], t0.wrapping_add(600_000))
            .expect("observe");
        assert_eq!(t.in_flight(), 1);
    }

    // ── StallWatchdog ───────────────────────────────────────

    /// 499ms 不触发、501ms 触发（阈值边界，严格大于）。
    #[test]
    fn watchdog_fires_after_500ms_without_complete_frame() {
        let mut w = StallWatchdog::new(DEFAULT_STALL_US);
        w.arm(0);
        assert!(!w.poll(499_000), "499ms 不触发");
        assert!(!w.poll(500_000), "恰好 500ms 不触发（严格大于）");
        assert!(w.poll(500_001), "501ms 触发");
        assert_eq!(w.fires(), 1);
    }

    /// 同一段停滞 500ms 一次；持续停滞按 500ms 节拍重复（自锚定）；
    /// 帧到达重新武装。
    #[test]
    fn watchdog_refires_only_after_next_window() {
        let mut w = StallWatchdog::new(DEFAULT_STALL_US);
        w.arm(0);
        assert!(w.poll(600_000), "第一次触发");
        // 自锚定：基线推进到 600_000，紧接着拍不重复触发。
        assert!(!w.poll(600_001), "触发后立即不得重复");
        assert!(!w.poll(1_099_000), "下一窗口前不触发");
        assert!(w.poll(1_100_001), "持续停滞按 500ms 节拍重复");
        assert_eq!(w.fires(), 2);

        // 帧到达重新武装（基线推进）。
        w.on_frame_complete(1_200_000);
        assert!(!w.poll(1_699_999), "帧到达后 500ms 内不触发");
        assert!(w.poll(1_700_001));
        assert_eq!(w.fires(), 3);
    }

    /// arm 后首帧 >500ms 才到也触发（首 IDR 丢失可恢复，W1）；
    /// 未 arm 的看门狗不触发。
    #[test]
    fn watchdog_arms_at_streaming_start() {
        let mut w = StallWatchdog::new(DEFAULT_STALL_US);
        w.arm(1_000);
        assert!(!w.poll(500_999), "arm 基线起 500ms 内不触发");
        assert!(w.poll(501_001), "首帧迟到超过阈值必须触发（不等首帧武装）");

        // arm 之前（Init/Connected）：永不触发（会话只在 Streaming 拍 poll，
        // 这里钉死防御语义）。
        let mut w2 = StallWatchdog::new(DEFAULT_STALL_US);
        assert!(!w2.poll(10_000_000));
        assert_eq!(w2.fires(), 0);
    }

    // ── IdrController ───────────────────────────────────────

    /// 在途时第二个触发（不同 reason）不产生发送；闭环后清空记录（W3）。
    #[test]
    fn idr_controller_dedups_inflight_request() {
        let mut c = IdrController::new(IdrControllerConfig::default());
        c.request(IdrReason::WatchdogStall, 0);
        let r1 = c.poll(0, 0).expect("Idle 的触发必须发出");
        assert_eq!(r1.reason, IdrReason::WatchdogStall);
        assert_eq!(r1.attempt, 1);

        // 在途期间新触发（不同 reason）被吞：不产生发送。
        c.request(IdrReason::DecoderNeedKeyframe, 10);
        assert!(c.poll(10, 0).is_none(), "在途不得重复发送");
        assert!(c.poll(60_000, 0).is_none(), "超 ack 窗口但未到重发地板");
        // 地板到：重发仍是**原** reason（被吞的不改本轮语义）。
        let r2 = c.poll(100_000, 0).expect("超时+地板满足必须重发");
        assert_eq!(r2.reason, IdrReason::WatchdogStall);
        assert_eq!(r2.attempt, 2);
        assert_eq!(c.stats().retries, 1);
        assert_eq!(c.stats().requests_sent, 2);
        assert_eq!(c.stats().requests_by_reason, [0, 2, 0]);

        // 闭环（ack → 帧到达）：被吞记录清空，之后不补发。
        c.on_ack(FrameSeq(5), 150_000);
        c.on_frame_complete(DeliveredFrame {
            seq: FrameSeq(5),
            is_idr: true,
            at_us: 160_000,
        });
        assert_eq!(c.stats().resolved_by_idr, 1);
        assert!(
            c.poll(161_000, 0).is_none(),
            "闭环后被吞的触发不得补发（持续性需求下一拍自然重触发）"
        );
        // stats 无「被吞」计数：request 只记意图。
        assert_eq!(c.stats().requests_sent, 2);
    }

    /// ack 先到→帧到达闭环；**帧先到→ack 到达立即闭环**（两通道无序，W5）。
    #[test]
    fn idr_controller_resolves_on_acked_frame_arrival() {
        // 方向一：ack 先到（→ AwaitFrame），帧到达闭环。
        let mut c = IdrController::new(IdrControllerConfig::default());
        c.request(IdrReason::WatchdogStall, 0);
        assert!(c.poll(0, 0).is_some());
        c.on_ack(FrameSeq(9), 20_000);
        assert_eq!(c.stats().acks_received, 1);
        assert_eq!(c.stats().resolved_by_idr, 0, "ack 到达不闭环（还要等帧）");
        c.on_frame_complete(DeliveredFrame {
            seq: FrameSeq(9),
            is_idr: true,
            at_us: 30_000,
        });
        assert_eq!(c.stats().resolved_by_idr, 1, "帧到达且 seq 匹配闭环");
        assert!(c.poll(31_000, 0).is_none(), "闭环后不再发送");

        // 方向二：帧先到（InFlight 期间 IDR 完整到达），ack 到达立即闭环。
        let mut c2 = IdrController::new(IdrControllerConfig::default());
        c2.request(IdrReason::DecoderNeedKeyframe, 0);
        assert!(c2.poll(0, 0).is_some());
        c2.on_frame_complete(DeliveredFrame {
            seq: FrameSeq(11),
            is_idr: true,
            at_us: 10_000,
        });
        assert_eq!(c2.stats().resolved_by_idr, 0, "帧先到不闭环（等 ack 确认）");
        c2.on_ack(FrameSeq(11), 15_000);
        assert_eq!(c2.stats().resolved_by_idr, 1, "ack 命中已到达的帧立即闭环");
        // 不挂死在 AwaitFrame：远超超时窗口也不重发。
        assert!(c2.poll(5_000_000, 0).is_none());
    }

    /// rtt+margin 无 ack → 恰好重发一次，attempt 递增、计入 `retries`；
    /// rtt 注入生效（窗口随 rtt 拓宽）。
    #[test]
    fn idr_controller_retries_when_ack_times_out() {
        let mut c = IdrController::new(IdrControllerConfig::default());
        c.request(IdrReason::WatchdogStall, 0);
        let r1 = c.poll(0, 0).expect("首次发送");
        assert_eq!(r1.attempt, 1);

        // rtt=0 → 窗口 = 50ms。恰好 50ms 不重发（严格大于）。
        assert!(c.poll(50_000, 0).is_none(), "恰好 rtt+margin 不重发");
        // 50ms 超时但未到 100ms 地板。
        assert!(c.poll(60_000, 0).is_none(), "重发受 min_retry 地板约束");
        // 地板满足：恰好重发一次。
        let r2 = c.poll(100_000, 0).expect("超时+地板满足必须重发");
        assert_eq!(r2.attempt, 2);
        assert_eq!(c.stats().retries, 1);
        assert_eq!(c.stats().requests_sent, 2);
        // 重发后重新计时：马上不重复。
        assert!(c.poll(100_001, 0).is_none());

        // rtt 注入生效：rtt=200ms 时窗口 = 250ms。
        let mut c2 = IdrController::new(IdrControllerConfig::default());
        c2.request(IdrReason::DecoderNeedKeyframe, 0);
        assert!(c2.poll(0, 0).is_some());
        assert!(
            c2.poll(250_000, 200_000).is_none(),
            "250ms 恰好 rtt+margin 不重发"
        );
        assert!(c2.poll(250_001, 200_000).is_some(), "250ms+1 重发");
    }

    /// ack 到了但对应帧超时未到（IDR 被丢）→ 重发（W5 关键路径）。
    #[test]
    fn idr_controller_retries_when_idr_datagram_lost() {
        let mut c = IdrController::new(IdrControllerConfig::default());
        c.request(IdrReason::WatchdogStall, 0);
        assert!(c.poll(0, 0).is_some());
        c.on_ack(FrameSeq(3), 20_000); // → AwaitFrame(3)
        assert_eq!(c.stats().acks_received, 1);

        // 帧等待窗口 = rtt(0) + margin(50ms) + 500ms = 550ms（自 since）。
        assert!(
            c.poll(569_000, 0).is_none(),
            "帧等待窗口内不重发（ack 已确认请求到达）"
        );
        let r2 = c.poll(570_001, 0).expect("帧超时必须重发");
        assert_eq!(r2.attempt, 2);
        assert_eq!(c.stats().retries, 1);

        // 重发后正常闭环：ack(4) → 帧 4 到达。
        c.on_ack(FrameSeq(4), 600_000);
        c.on_frame_complete(DeliveredFrame {
            seq: FrameSeq(4),
            is_idr: true,
            at_us: 610_000,
        });
        assert_eq!(c.stats().resolved_by_idr, 1);
        assert_eq!(c.stats().acks_received, 2);
    }

    /// 持续停滞 2s（模拟静态画面：无 ack 无帧）：`requests_sent` 受地板
    /// 约束 ≤ 2s/100ms + 1（W4）。
    #[test]
    fn idr_controller_retry_floor_bounds_static_storm() {
        let mut c = IdrController::new(IdrControllerConfig::default());
        c.request(IdrReason::WatchdogStall, 0);
        let mut sent = 0u64;
        let mut now = 0u32;
        while now <= 2_000_000 {
            if c.poll(now, 0).is_some() {
                sent += 1;
            }
            now = now.saturating_add(1_000); // 1ms 一拍（模拟驱动循环）
        }
        assert_eq!(c.stats().requests_sent, sent);
        assert_eq!(sent, 21, "0,100ms,...,2s 共 21 条");
        assert!(
            sent <= 2_000_000 / DEFAULT_MIN_RETRY_INTERVAL_US as u64 + 1,
            "地板必须把重发压到 ≤10 条/秒：{sent}"
        );
    }

    // ── idr_frame_seqs ──────────────────────────────────────

    /// `split_frame` 多片 IDR → 单 seq；P 帧不产出；多帧混排保序去重。
    #[test]
    fn idr_frame_seqs_scans_split_datagrams() {
        let idr5 = frame_packets(5, FrameKind::Idr, &[0xAB; 3000], MAX_PAYLOAD);
        let p6 = frame_packets(6, FrameKind::P, &[0xCD; 64], MAX_PAYLOAD);
        let idr7 = frame_packets(7, FrameKind::Idr, &[0xEF; 64], MAX_PAYLOAD);
        assert_eq!(idr5.len(), 3, "3000B 按 1200 分片为 3 包");

        // 混排：IDR5(3 片) + P6 + IDR7 + IDR5 的重复分片。
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        datagrams.extend(idr5.iter().cloned());
        datagrams.extend(p6.iter().cloned());
        datagrams.extend(idr7.iter().cloned());
        datagrams.push(idr5[0].clone());
        assert_eq!(
            idr_frame_seqs(&datagrams),
            vec![FrameSeq(5), FrameSeq(7)],
            "多片 IDR 只出一个 seq（坑 8），P 帧不产出，保序去重"
        );

        // 全 P / 空。
        assert!(idr_frame_seqs(&p6).is_empty());
        assert!(idr_frame_seqs(&[]).is_empty());
    }
}
