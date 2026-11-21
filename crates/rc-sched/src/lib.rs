//! deadline-aware 帧调度器。
//!
//! # 这个 crate 解决的问题
//! 「帧率上限高但实际帧率低」的根因不是编不出来，而是**排队的帧在互相挤压**：
//!
//! ```text
//! 采集 → 无界队列 → 编码 → 无界队列 → 发送
//! ```
//!
//! 编码慢一点 → 队列堆积 → 你看到的画面比现实晚 166ms
//! → 网络一卡队列爆掉 → 为追上瞬间猛发 → 把网络再打爆一次。
//!
//! # 本实现的三条硬规则
//! - **R1** 在途帧上限 = `max_in_flight`（默认 2）。超过则丢最旧帧。
//! - **R2** 帧的截止时间 = 采集时刻 + `target_latency`。超期即丢，不编码。
//! - **R3** 重复帧直接丢（脏区为空 / `is_repeat`）。
//!
//! 另有两条由调用方保证的规则：
//! - **R4** IDR 不丢（见 [`FrameKind::is_droppable`]）
//! - **R5** 丢帧必须返回 [`Decision::Drop`] 让调用方通知解码端请求关键帧
//!
//! # 时间注入
//! 所有时间由调用方通过 [`Scheduler::on_captured`] 的 `now` 参数注入，
//! **本 crate 不读系统时钟**。这样全部行为可在单测里精确验证，
//! 不依赖 `sleep`、不产生 flaky test。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use rc_types::{FrameKind, FrameSeq, Timestamp};

/// 调度器配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// 在途帧上限（已送入编码、尚未发送完成的帧）。
    pub max_in_flight: usize,
    /// 目标端到端延迟预算。帧超过 `采集时刻 + 该值` 即视为超期。
    pub target_latency_us: u32,
    /// 丢帧后触发关键帧请求的阈值（连续丢 N 帧）。
    pub idr_request_threshold: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            // Q3（架构文档 §0.2）：在途上限 2 帧。
            // RustDesk 用 120 帧队列（src/client.rs:114），代价是画面晚 2 秒。
            // **不要为了「减少丢帧」而调大这个值**——那正是要避免的问题方向。
            max_in_flight: 2,
            // 25ms 的 SLO 目标，留 3ms 余量给调度器决策本身
            target_latency_us: 22_000,
            idr_request_threshold: 3,
        }
    }
}

impl SchedulerConfig {
    /// 校验配置合法性。构造函数与 `on_captured` 都会调用。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_in_flight == 0 {
            return Err(ConfigError::ZeroMaxInFlight);
        }
        if self.max_in_flight > 8 {
            // 超过 8 帧在途，延迟必然失控；这是刻意的防御性上限。
            // 注意：目标值是 2（见 Default 注释）。8 只是「允许但几乎肯定错」的边界，
            // 用于离线分析等非常规场景。
            return Err(ConfigError::TooManyInFlight(self.max_in_flight));
        }
        if self.target_latency_us == 0 {
            return Err(ConfigError::ZeroLatency);
        }
        Ok(())
    }
}

/// 配置错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `max_in_flight` 不能为 0
    #[error("max_in_flight must be >= 1")]
    ZeroMaxInFlight,
    /// `max_in_flight` 过大（>8）会导致延迟失控
    #[error("max_in_flight {0} exceeds hard limit 8")]
    TooManyInFlight(usize),
    /// 延迟预算不能为 0
    #[error("target_latency_us must be > 0")]
    ZeroLatency,
}

/// 调度决策。调用方必须处理每一个分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 送入编码器。
    Encode {
        /// 帧序号
        seq: FrameSeq,
        /// 该帧的截止时间（编码完成必须在此之前，否则调用方应放弃）
        deadline_us: u32,
    },
    /// 丢弃，并说明原因。
    ///
    /// **调用方必须检查 `needs_keyframe`**：为 true 时应向对端请求关键帧，
    /// 否则画面会因参考链断裂而花屏。
    Drop {
        /// 帧序号
        seq: FrameSeq,
        /// 丢弃原因
        reason: DropReason,
        /// 是否需要请求关键帧
        needs_keyframe: bool,
    },
}

/// 丢弃原因。用于统计与诊断（FrameTrace 会记录它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// 与上一帧内容完全相同（静态画面）
    Repeat,
    /// 已超过 deadline（编码也来不及了）
    Stale,
    /// 在途预算已满，到达帧被拒（不逐出在途帧，见 `on_captured` 的 R1 说明）
    Budget,
    /// 该帧被更新的一帧取代（旧帧已无价值）
    Superseded,
}

/// 在途帧记录。
///
/// 只留完成配对必需的字段：`seq` 用于 [`Self::on_encoded`] 配对释放，
/// `deadline_us` 用于超期监控。帧类型不放进来——丢弃到达帧（R1）的
/// 裁决发生在入表之前，在途帧本身无差别（旧「逐出 P 帧」逻辑随
/// 逐出策略一起删除了）。
#[derive(Debug, Clone, Copy)]
struct InFlight {
    seq: FrameSeq,
    deadline_us: u32,
}

/// 调度器统计。
///
/// 每次会话结束应打印/上报，这是判断调度器是否生效的唯一依据。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// 收到（采集到）的帧数
    pub captured: u64,
    /// 决定编码的帧数
    pub encoded: u64,
    /// 因重复丢弃
    pub dropped_repeat: u64,
    /// 因超期丢弃
    pub dropped_stale: u64,
    /// 因预算丢弃
    pub dropped_budget: u64,
    /// 因被取代丢弃
    pub dropped_superseded: u64,
    /// 发出的关键帧请求次数
    pub keyframe_requests: u64,
}

impl SchedulerStats {
    /// 丢弃总数。
    pub fn dropped_total(&self) -> u64 {
        self.dropped_repeat + self.dropped_stale + self.dropped_budget + self.dropped_superseded
    }

    /// 实际编码率（相对采集数）。
    ///
    /// 静态画面下应显著 < 1.0，否则说明重复帧没被过滤。
    pub fn encode_ratio(&self) -> f64 {
        if self.captured == 0 {
            return 0.0;
        }
        self.encoded as f64 / self.captured as f64
    }
}

/// 帧调度器。
///
/// # 生命周期
/// ```text
/// let mut sched = Scheduler::new(SchedulerConfig::default())?;
/// // 采集循环
/// match sched.on_captured(seq, kind, is_repeat, capture_ts, now_us) {
///     Decision::Encode { seq, deadline_us } => { /* 送编码 */ }
///     Decision::Drop { reason, needs_keyframe, .. } => {
///         if needs_keyframe { /* 请求关键帧 */ }
///     }
/// }
/// // 编码完成后
/// sched.on_encoded(seq, now_us);
/// ```
#[derive(Debug)]
pub struct Scheduler {
    cfg: SchedulerConfig,
    in_flight: Vec<InFlight>,
    stats: SchedulerStats,
    /// 连续丢帧计数（用于 IDR 请求）
    consecutive_drops: u32,
    next_seq: u64,
}

impl Scheduler {
    /// 创建调度器。
    ///
    /// # 错误
    /// 配置非法时返回 [`ConfigError`]。
    pub fn new(cfg: SchedulerConfig) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            in_flight: Vec::with_capacity(cfg.max_in_flight),
            cfg,
            stats: SchedulerStats::default(),
            consecutive_drops: 0,
            next_seq: 0,
        })
    }

    /// 分配下一个帧序号。
    ///
    /// 采集层在产生新帧时调用，保证 seq 单调递增且不与重复帧混淆。
    pub fn alloc_seq(&mut self) -> FrameSeq {
        let s = FrameSeq(self.next_seq);
        self.next_seq += 1;
        s
    }

    /// 采集到一帧时的决策。
    ///
    /// # 参数
    /// - `seq`: 帧序号（由 [`alloc_seq`](Self::alloc_seq) 分配）
    /// - `kind`: IDR 或 P
    /// - `is_repeat`: 与上一帧完全相同（由 `rc-diff` 判定）
    /// - `capture_ts`: **内容变化时刻**（不是采集完成时刻）
    /// - `now_us`: 当前时刻（同一时间基准）
    ///
    /// # 决策顺序（顺序不可调换）
    /// 1. R3 重复帧 → Drop(Repeat)
    /// 2. R2 超期 → Drop(Stale)
    /// 3. R1 预算满 → 丢弃**当前到达帧** Drop(Budget)
    /// 4. 接受 → Encode
    ///
    /// # R1 为什么丢新帧而不是挤出最旧在途帧（2026-11-21 M1 实测修正）
    /// 硬件/串行编码器**无法中途取消**在编帧：挤出在途帧 ≠ 省下它的编码
    /// 工作，只是把已完成的产出扔掉。实测（20ms 编码器 + 16.7ms 到达率）
    /// 逐出策略进入活锁——每个新到帧驱逐的总是尚未完成的更旧帧，全部
    /// 在制工作被反复作废，吞吐塌缩、几乎无帧上屏。满员丢新帧则保住
    /// 全部在制工作：显示率 = 编码器吞吐，延迟有界。在途 IDR 因此天然
    /// 受保护（永远不会被移出在途表）。
    pub fn on_captured(
        &mut self,
        seq: FrameSeq,
        kind: FrameKind,
        is_repeat: bool,
        capture_ts: Timestamp,
        now_us: u32,
    ) -> Decision {
        self.stats.captured += 1;

        // R3: 重复帧
        if is_repeat {
            self.stats.dropped_repeat += 1;
            self.consecutive_drops += 1;
            return Decision::Drop {
                seq,
                reason: DropReason::Repeat,
                needs_keyframe: false, // 重复帧不影响参考链
            };
        }

        // R2: 超期
        let deadline_us = capture_ts.0.saturating_add(self.cfg.target_latency_us);
        if now_us >= deadline_us {
            self.stats.dropped_stale += 1;
            self.consecutive_drops += 1;
            let needs_keyframe = kind.is_droppable();
            return Decision::Drop {
                seq,
                reason: DropReason::Stale,
                needs_keyframe,
            };
        }

        // R1: 预算满 → 丢弃当前到达帧（见 on_captured 文档的实测修正说明：
        // 硬件编码无法中途取消，逐出在途帧 = 反复作废在制工作的活锁）。
        if self.in_flight.len() >= self.cfg.max_in_flight {
            self.stats.dropped_budget += 1;
            self.consecutive_drops += 1;
            return Decision::Drop {
                seq,
                reason: DropReason::Budget,
                needs_keyframe: !kind.is_droppable(),
            };
        }

        self.in_flight.push(InFlight { seq, deadline_us });
        self.stats.encoded += 1;
        self.consecutive_drops = 0;

        Decision::Encode { seq, deadline_us }
    }

    /// 编码完成回调。
    ///
    /// # 必须调用
    /// 不调用会导致在途队列永远不释放，调度器在 `max_in_flight` 帧后
    /// 开始无脑丢帧 —— 表现为「画面卡死但 CPU 很低」。
    ///
    /// # 未知序号
    /// 传入未注册的 seq 是调用方 bug。本方法不 panic（生产环境不应因
    /// 统计错误而崩溃），而是静默忽略并返回 `false`。
    pub fn on_encoded(&mut self, seq: FrameSeq, _now_us: u32) -> bool {
        match self.in_flight.iter().position(|f| f.seq == seq) {
            Some(idx) => {
                self.in_flight.remove(idx);
                true
            }
            None => false,
        }
    }

    /// 是否需要请求关键帧（由连续丢帧触发）。
    ///
    /// 调用方应在每次 [`on_captured`](Self::on_captured) 后检查。
    /// 返回 `true` 时同时清零计数，避免重复请求。
    pub fn take_keyframe_request(&mut self) -> bool {
        if self.consecutive_drops >= self.cfg.idr_request_threshold {
            self.consecutive_drops = 0;
            self.stats.keyframe_requests += 1;
            true
        } else {
            false
        }
    }

    /// 当前在途帧数。
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// 在途帧的序号（按进入顺序）。用于测试与诊断。
    pub fn in_flight_seqs(&self) -> Vec<FrameSeq> {
        self.in_flight.iter().map(|f| f.seq).collect()
    }

    /// 在途帧的 `(序号, 截止时间)`。用于诊断「还有多久必须编完」。
    pub fn in_flight_deadlines(&self) -> Vec<(FrameSeq, u32)> {
        self.in_flight
            .iter()
            .map(|f| (f.seq, f.deadline_us))
            .collect()
    }

    /// 统计快照。
    pub fn stats(&self) -> SchedulerStats {
        self.stats
    }

    /// 配置快照。
    pub fn config(&self) -> SchedulerConfig {
        self.cfg
    }

    /// 会话重置：清空在途与统计，保留配置。
    pub fn reset(&mut self) {
        self.in_flight.clear();
        self.stats = SchedulerStats::default();
        self.consecutive_drops = 0;
        self.next_seq = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS0: u32 = 1_000_000; // 会话开始后 1 秒

    fn sched() -> Scheduler {
        Scheduler::new(SchedulerConfig::default()).unwrap()
    }

    #[test]
    fn config_rejects_zero_in_flight() {
        let cfg = SchedulerConfig {
            max_in_flight: 0,
            ..Default::default()
        };
        assert_eq!(cfg.validate(), Err(ConfigError::ZeroMaxInFlight));
    }

    #[test]
    fn config_rejects_excessive_in_flight() {
        let cfg = SchedulerConfig {
            max_in_flight: 16,
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(ConfigError::TooManyInFlight(16))
        ));
    }

    /// R3: 重复帧必须被丢弃且不请求关键帧
    #[test]
    fn repeat_frame_is_dropped_without_keyframe() {
        let mut s = sched();
        let seq = s.alloc_seq();
        let d = s.on_captured(seq, FrameKind::P, true, Timestamp(TS0), TS0 + 100);
        assert_eq!(
            d,
            Decision::Drop {
                seq,
                reason: DropReason::Repeat,
                needs_keyframe: false
            }
        );
        assert_eq!(s.stats().dropped_repeat, 1);
        assert_eq!(s.in_flight_len(), 0);
    }

    /// R2: 超期帧必须被丢弃
    #[test]
    fn stale_frame_is_dropped() {
        let mut s = sched();
        let seq = s.alloc_seq();
        // 采集时刻 + 22ms < now
        let d = s.on_captured(seq, FrameKind::P, false, Timestamp(TS0), TS0 + 30_000);
        assert_eq!(
            d,
            Decision::Drop {
                seq,
                reason: DropReason::Stale,
                needs_keyframe: true
            }
        );
        assert_eq!(s.stats().dropped_stale, 1);
    }

    /// 边界：恰好等于 deadline 视为超期（不编码）
    #[test]
    fn deadline_boundary_is_exclusive() {
        let mut s = sched();
        let seq = s.alloc_seq();
        let cfg = s.config();
        let d = s.on_captured(
            seq,
            FrameKind::P,
            false,
            Timestamp(TS0),
            TS0 + cfg.target_latency_us,
        );
        assert!(matches!(d, Decision::Drop { .. }));
    }

    /// 边界：deadline 前 1 微秒仍可编码
    #[test]
    fn just_before_deadline_is_encoded() {
        let mut s = sched();
        let seq = s.alloc_seq();
        let cfg = s.config();
        let d = s.on_captured(
            seq,
            FrameKind::P,
            false,
            Timestamp(TS0),
            TS0 + cfg.target_latency_us - 1,
        );
        assert!(matches!(d, Decision::Encode { .. }));
    }

    #[test]
    fn normal_frame_is_encoded() {
        let mut s = sched();
        let seq = s.alloc_seq();
        let d = s.on_captured(seq, FrameKind::P, false, Timestamp(TS0), TS0 + 1_000);
        match d {
            Decision::Encode {
                seq: got,
                deadline_us,
            } => {
                assert_eq!(got, seq);
                assert_eq!(deadline_us, TS0 + 22_000);
            }
            other => panic!("expected Encode, got {other:?}"),
        }
        assert_eq!(s.in_flight_len(), 1);
    }

    /// R1: 在途满时，丢弃**到达帧**而不是逐出在途帧（2026-11-21 修正，
    /// 见 on_captured 文档：逐出无法取消的物理编码 = 活锁）。
    #[test]
    fn budget_full_drops_arrival() {
        let mut s = sched();
        let a = s.alloc_seq();
        let b = s.alloc_seq();
        let c = s.alloc_seq();

        s.on_captured(a, FrameKind::P, false, Timestamp(TS0), TS0 + 100);
        s.on_captured(b, FrameKind::P, false, Timestamp(TS0 + 1000), TS0 + 1100);
        assert_eq!(s.in_flight_len(), 2);

        let d = s.on_captured(c, FrameKind::P, false, Timestamp(TS0 + 2000), TS0 + 2100);
        // c 应被拒，在途保持 [a, b]
        assert!(matches!(d, Decision::Drop { seq, reason: DropReason::Budget, .. } if seq == c));
        assert_eq!(s.in_flight_seqs(), vec![a, b]);
        assert_eq!(s.stats().dropped_budget, 1);
    }

    /// 关键用例：在途含 IDR 时，IDR 天然受保护（新语义不逐出任何在途帧）
    #[test]
    fn idr_in_flight_is_never_evicted() {
        let mut s = sched();
        let idr = s.alloc_seq();
        let p = s.alloc_seq();
        let newp = s.alloc_seq();

        s.on_captured(idr, FrameKind::Idr, false, Timestamp(TS0), TS0 + 100);
        s.on_captured(p, FrameKind::P, false, Timestamp(TS0 + 500), TS0 + 600);

        let d = s.on_captured(newp, FrameKind::P, false, Timestamp(TS0 + 1000), TS0 + 1100);
        // 预算满：新帧被拒，idr 与 p 都保留
        assert!(matches!(d, Decision::Drop { seq, reason: DropReason::Budget, .. } if seq == newp));
        assert_eq!(s.in_flight_seqs(), vec![idr, p]);
    }

    /// 在途全是 IDR 时，新帧必须被丢弃
    #[test]
    fn all_idr_in_flight_rejects_new_frame() {
        let mut s = sched();
        let a = s.alloc_seq();
        let b = s.alloc_seq();
        let c = s.alloc_seq();

        s.on_captured(a, FrameKind::Idr, false, Timestamp(TS0), TS0 + 100);
        s.on_captured(b, FrameKind::Idr, false, Timestamp(TS0 + 200), TS0 + 300);

        let d = s.on_captured(c, FrameKind::P, false, Timestamp(TS0 + 400), TS0 + 500);
        // 新语义：预算满一律丢到达帧（Budget），在途 IDR 无需特判即受保护。
        assert!(matches!(
            d,
            Decision::Drop {
                reason: DropReason::Budget,
                ..
            }
        ));
        assert_eq!(s.in_flight_seqs(), vec![a, b]);
    }

    #[test]
    fn on_encoded_releases_slot() {
        let mut s = sched();
        let a = s.alloc_seq();
        s.on_captured(a, FrameKind::P, false, Timestamp(TS0), TS0 + 100);
        assert_eq!(s.in_flight_len(), 1);
        assert!(s.on_encoded(a, TS0 + 200));
        assert_eq!(s.in_flight_len(), 0);
    }

    #[test]
    fn on_encoded_unknown_seq_is_ignored() {
        let mut s = sched();
        assert!(!s.on_encoded(FrameSeq(999), TS0));
    }

    /// 连续丢帧达阈值后必须请求关键帧，且只请求一次
    #[test]
    fn consecutive_drops_trigger_single_keyframe_request() {
        let mut s = sched();
        for i in 0..3 {
            let seq = s.alloc_seq();
            s.on_captured(seq, FrameKind::P, true, Timestamp(TS0 + i), TS0 + i);
        }
        assert!(
            s.take_keyframe_request(),
            "3 consecutive drops must request keyframe"
        );
        assert!(
            !s.take_keyframe_request(),
            "request must be consumed exactly once"
        );
        assert_eq!(s.stats().keyframe_requests, 1);
    }

    #[test]
    fn successful_encode_resets_drop_counter() {
        let mut s = sched();
        let a = s.alloc_seq();
        s.on_captured(a, FrameKind::P, true, Timestamp(TS0), TS0);
        let b = s.alloc_seq();
        s.on_captured(b, FrameKind::P, false, Timestamp(TS0 + 10), TS0 + 20);
        // 计数已重置，不应请求关键帧
        assert!(!s.take_keyframe_request());
    }

    /// 这是调度器的核心价值验证：编码器变慢时延迟不增长
    #[test]
    fn latency_does_not_grow_when_encoder_is_slow() {
        let mut s = sched();
        // 模拟：每帧采集间隔 16ms，但编码耗时 50ms（只有第 1 帧能完成）
        let mut confirmed: Vec<FrameSeq> = Vec::new();
        for i in 0..10u32 {
            let now = TS0 + i * 16_000;
            let seq = s.alloc_seq();
            match s.on_captured(seq, FrameKind::P, false, Timestamp(now), now + 100) {
                Decision::Encode { seq, .. } => confirmed.push(seq),
                Decision::Drop { .. } => {}
            }
            // 编码器极慢：只在第 5 帧时完成第 0 帧
            if i == 5 {
                s.on_encoded(confirmed[0], now);
            }
        }
        // 在途永远不会超过 max_in_flight，证明延迟不累积
        assert!(s.in_flight_len() <= s.config().max_in_flight);
        assert!(
            s.stats().dropped_budget > 0,
            "slow encoder must cause drops"
        );
    }

    #[test]
    fn stats_encode_ratio_for_static_screen() {
        let mut s = sched();
        let first = s.alloc_seq();
        s.on_captured(first, FrameKind::Idr, false, Timestamp(TS0), TS0 + 100);
        for i in 1..60u32 {
            let seq = s.alloc_seq();
            s.on_captured(
                seq,
                FrameKind::P,
                true,
                Timestamp(TS0 + i * 1000),
                TS0 + i * 1000,
            );
        }
        let st = s.stats();
        assert_eq!(st.captured, 60);
        assert_eq!(st.encoded, 1);
        assert!(
            st.encode_ratio() < 0.02,
            "static screen must encode ~0% of frames"
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = sched();
        let a = s.alloc_seq();
        s.on_captured(a, FrameKind::P, false, Timestamp(TS0), TS0 + 100);
        s.reset();
        assert_eq!(s.in_flight_len(), 0);
        assert_eq!(s.stats(), SchedulerStats::default());
        assert_eq!(s.alloc_seq(), FrameSeq(0));
    }

    #[test]
    fn stale_idr_requests_keyframe_marker() {
        // IDR 超期被丢弃时 needs_keyframe=false（它本身就是关键帧，丢了要重新要）
        let mut s = sched();
        let seq = s.alloc_seq();
        let d = s.on_captured(seq, FrameKind::Idr, false, Timestamp(TS0), TS0 + 99_999);
        match d {
            Decision::Drop {
                needs_keyframe,
                reason,
                ..
            } => {
                assert_eq!(reason, DropReason::Stale);
                // IDR 不可丢的语义：需要请求新的关键帧
                assert!(
                    !needs_keyframe,
                    "IDR itself is a keyframe; no re-request needed"
                );
            }
            other => panic!("expected Drop, got {other:?}"),
        }
    }
}
