//! 帧时间线埋点。
//!
//! # 为什么必须有这个 crate
//! 没有它，延迟优化只能靠猜。有了它，任何 agent 都能回答：
//! 「第 1234 帧为什么没到用户屏幕上？」
//!
//! # 设计原则
//! - **零分配热路径**：`mark()` 只是往预分配数组里写一个 `u32`
//! - **无锁**：每帧由单一线程写，不需要同步
//! - **可导出**：会话结束 dump 成 JSON，离线分析
//! - **不在生产路径上做格式化**：只在 dump 时格式化

#![deny(missing_docs)]
#![deny(unsafe_code)]

use rc_types::{FrameSeq, Timestamp};

/// 帧生命周期中的关键时间点。
///
/// **新增枚举值时必须同步更新 [`crate::FrameTrace::latency_breakdown`] 与
/// JSON 导出字段**，否则统计会静默漏项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mark {
    /// 屏幕内容实际变化的时刻（由采集层提供，不是采集返回时刻）
    Captured,
    /// 脏区分析完成
    Diffed,
    /// 送交编码器
    EncodeStart,
    /// 编码器返回码流
    EncodeDone,
    /// 真正写入 socket
    Sent,
    /// 对端 ACK（仅可靠通道有）
    Acked,
    /// 完整接收（对端）
    Received,
    /// 解码完成（对端）
    Decoded,
    /// 呈现上屏（对端）
    Presented,
}

impl Mark {
    /// 全部标记，按时间顺序。
    pub const ALL: [Mark; 9] = [
        Mark::Captured,
        Mark::Diffed,
        Mark::EncodeStart,
        Mark::EncodeDone,
        Mark::Sent,
        Mark::Acked,
        Mark::Received,
        Mark::Decoded,
        Mark::Presented,
    ];

    /// JSON 字段名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Mark::Captured => "captured_us",
            Mark::Diffed => "diffed_us",
            Mark::EncodeStart => "encode_start_us",
            Mark::EncodeDone => "encode_done_us",
            Mark::Sent => "sent_us",
            Mark::Acked => "acked_us",
            Mark::Received => "received_us",
            Mark::Decoded => "decoded_us",
            Mark::Presented => "presented_us",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// 单帧的时间线。
#[derive(Debug, Clone, Default)]
pub struct FrameTrace {
    seq: FrameSeq,
    /// 各标记的时刻，None 表示未到达该阶段
    marks: [Option<u32>; Mark::ALL.len()],
    outcome: Option<Outcome>,
}

/// 帧的最终结局。
///
/// **每一帧必须恰好有一个结局**，否则说明埋点缺失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 成功上屏
    Displayed,
    /// 与上一帧相同，主动跳过
    DroppedRepeat,
    /// 超过 deadline，主动丢弃
    DroppedStale,
    /// 在途预算满，被挤出
    DroppedBudget,
    /// 被更新帧取代
    DroppedSuperseded,
    /// 已发送但在网络丢失
    LostInNetwork,
    /// 解码失败
    DecodeError,
}

impl Outcome {
    /// 是否为成功路径。
    pub const fn is_displayed(self) -> bool {
        matches!(self, Outcome::Displayed)
    }

    /// JSON 字段名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Outcome::Displayed => "displayed",
            Outcome::DroppedRepeat => "dropped_repeat",
            Outcome::DroppedStale => "dropped_stale",
            Outcome::DroppedBudget => "dropped_budget",
            Outcome::DroppedSuperseded => "dropped_superseded",
            Outcome::LostInNetwork => "lost_in_network",
            Outcome::DecodeError => "decode_error",
        }
    }
}

impl FrameTrace {
    /// 创建一帧的时间线。
    pub fn new(seq: FrameSeq) -> Self {
        Self {
            seq,
            marks: [None; Mark::ALL.len()],
            outcome: None,
        }
    }

    /// 帧序号。
    pub fn seq(&self) -> FrameSeq {
        self.seq
    }

    /// 记录一个时间点。重复记录同一标记会被忽略（保留首次）。
    ///
    /// # 为什么保留首次
    /// 热路径可能误调用两次，保留首次更符合「该事件发生时刻」的语义。
    pub fn mark(&mut self, mark: Mark, ts: Timestamp) {
        let slot = &mut self.marks[mark.index()];
        if slot.is_none() {
            *slot = Some(ts.0);
        }
    }

    /// 查询某个标记的时刻。
    pub fn at(&self, mark: Mark) -> Option<Timestamp> {
        self.marks[mark.index()].map(Timestamp)
    }

    /// 设置结局。重复设置会被忽略。
    pub fn finish(&mut self, outcome: Outcome) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }

    /// 查询结局。
    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome
    }

    /// 端到端延迟：`Presented - Captured`。
    ///
    /// 两者缺一返回 `None`（帧未走完流程）。
    pub fn end_to_end_us(&self) -> Option<u32> {
        let cap = self.at(Mark::Captured)?.0;
        let pres = self.at(Mark::Presented)?.0;
        Some(pres.saturating_sub(cap))
    }

    /// 分段延迟。返回 (段名, 微秒)，仅包含两端都有时刻的段。
    ///
    /// # 用途
    /// 定位「哪个环节慢」。这是延迟优化的第一步，必须准确。
    pub fn latency_breakdown(&self) -> Vec<(&'static str, u32)> {
        const PAIRS: [(Mark, Mark, &str); 8] = [
            (Mark::Captured, Mark::Diffed, "diff"),
            (Mark::Diffed, Mark::EncodeStart, "sched_queue"),
            (Mark::EncodeStart, Mark::EncodeDone, "encode"),
            (Mark::EncodeDone, Mark::Sent, "packetize"),
            (Mark::Sent, Mark::Received, "network"),
            (Mark::Received, Mark::Decoded, "decode"),
            (Mark::Decoded, Mark::Presented, "present"),
            (Mark::Captured, Mark::Presented, "total"),
        ];
        let mut out = Vec::with_capacity(PAIRS.len());
        for (from, to, name) in PAIRS {
            if let (Some(a), Some(b)) = (self.at(from), self.at(to)) {
                out.push((name, b.0.saturating_sub(a.0)));
            }
        }
        out
    }

    /// 汇总报告的一行 JSON。字段顺序固定，便于 diff。
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("{\"seq\":");
        s.push_str(&self.seq.0.to_string());
        for m in Mark::ALL {
            if let Some(ts) = self.at(m) {
                s.push_str(",\"");
                s.push_str(m.as_str());
                s.push_str("\":");
                s.push_str(&ts.0.to_string());
            }
        }
        if let Some(o) = self.outcome {
            s.push_str(",\"outcome\":\"");
            s.push_str(o.as_str());
            s.push('"');
        }
        if let Some(e2e) = self.end_to_end_us() {
            s.push_str(",\"e2e_us\":");
            s.push_str(&e2e.to_string());
        }
        s.push('}');
        s
    }
}

/// 时间线收集器（一次会话一个）。
///
/// # 容量
/// 固定容量环形缓冲，避免长会话内存无限增长。
/// 默认 4096 帧（约 68 秒 @60fps），足以诊断大部分问题。
pub struct TraceCollector {
    frames: Vec<FrameTrace>,
    capacity: usize,
    /// 丢弃的最旧帧数（发生回绕时递增）
    evicted: u64,
}

impl TraceCollector {
    /// 创建收集器。
    pub fn new(capacity: usize) -> Self {
        Self {
            frames: Vec::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
            evicted: 0,
        }
    }

    /// 当前保存的帧数。
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 被淘汰的帧数（缓冲回绕）。
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// 开始追踪一帧。返回可变引用供打点。
    ///
    /// # 缓冲满时
    /// 淘汰最旧的一帧。**注意**：被淘汰的帧会丢失其时间线，
    /// 因此长会话诊断应定期 dump 而不是等会话结束。
    pub fn begin(&mut self, seq: FrameSeq) -> &mut FrameTrace {
        if self.frames.len() >= self.capacity {
            self.frames.remove(0);
            self.evicted += 1;
        }
        self.frames.push(FrameTrace::new(seq));
        self.frames.last_mut().unwrap()
    }

    /// 按序号查找。
    pub fn get(&self, seq: FrameSeq) -> Option<&FrameTrace> {
        self.frames.iter().find(|f| f.seq == seq)
    }

    /// 按序号查找（可变）。管线层在 `begin` 之后的各阶段继续打点用；
    /// **不要**对同一序号调用两次 [`Self::begin`]（那会压入重复条目）。
    pub fn get_mut(&mut self, seq: FrameSeq) -> Option<&mut FrameTrace> {
        self.frames.iter_mut().find(|f| f.seq == seq)
    }

    /// 全部帧（按进入顺序）。
    pub fn frames(&self) -> &[FrameTrace] {
        &self.frames
    }

    /// 统计汇总。
    pub fn summary(&self) -> TraceSummary {
        let mut s = TraceSummary {
            frames: self.frames.len() as u64,
            evicted: self.evicted,
            ..Default::default()
        };
        let mut e2e: Vec<u32> = Vec::new();
        for f in &self.frames {
            match f.outcome {
                Some(Outcome::Displayed) => s.displayed += 1,
                Some(Outcome::DroppedRepeat) => s.dropped_repeat += 1,
                Some(Outcome::DroppedStale) => s.dropped_stale += 1,
                Some(Outcome::DroppedBudget) => s.dropped_budget += 1,
                Some(Outcome::DroppedSuperseded) => s.dropped_superseded += 1,
                Some(Outcome::LostInNetwork) => s.lost_in_network += 1,
                Some(Outcome::DecodeError) => s.decode_errors += 1,
                None => s.unfinished += 1,
            }
            if let Some(v) = f.end_to_end_us() {
                if f.outcome == Some(Outcome::Displayed) {
                    e2e.push(v);
                }
            }
        }
        if !e2e.is_empty() {
            e2e.sort_unstable();
            s.e2e_p50_us = Some(percentile(&e2e, 50.0));
            s.e2e_p95_us = Some(percentile(&e2e, 95.0));
            s.e2e_p99_us = Some(percentile(&e2e, 99.0));
        }
        s
    }

    /// 导出为 JSON 数组（每行一帧，便于 `jq` 处理）。
    pub fn export_jsonl(&self) -> String {
        let mut s = String::with_capacity(self.frames.len() * 160);
        for f in &self.frames {
            s.push_str(&f.to_json());
            s.push('\n');
        }
        s
    }
}

/// 统计汇总。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceSummary {
    /// 总帧数
    pub frames: u64,
    /// 成功上屏
    pub displayed: u64,
    /// 重复丢弃
    pub dropped_repeat: u64,
    /// 超期丢弃
    pub dropped_stale: u64,
    /// 预算丢弃
    pub dropped_budget: u64,
    /// 被取代丢弃
    pub dropped_superseded: u64,
    /// 网络丢失
    pub lost_in_network: u64,
    /// 解码错误
    pub decode_errors: u64,
    /// 未完成（未设置结局）
    pub unfinished: u64,
    /// 被淘汰（缓冲回绕）
    pub evicted: u64,
    /// 端到端延迟 P50（微秒）
    pub e2e_p50_us: Option<u32>,
    /// 端到端延迟 P95
    pub e2e_p95_us: Option<u32>,
    /// 端到端延迟 P99
    pub e2e_p99_us: Option<u32>,
}

impl TraceSummary {
    /// 丢帧总数。
    pub fn dropped_total(&self) -> u64 {
        self.dropped_repeat + self.dropped_stale + self.dropped_budget + self.dropped_superseded
    }

    /// 上屏率（displayed / frames）。
    pub fn display_ratio(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        self.displayed as f64 / self.frames as f64
    }

    /// 汇总的 JSON 表示。
    pub fn to_json(&self) -> String {
        let opt = |v: Option<u32>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
        format!(
            concat!(
                "{{\"frames\":{},\"displayed\":{},\"dropped_repeat\":{},",
                "\"dropped_stale\":{},\"dropped_budget\":{},\"dropped_superseded\":{},",
                "\"lost_in_network\":{},\"decode_errors\":{},\"unfinished\":{},",
                "\"evicted\":{},\"e2e_p50_us\":{},\"e2e_p95_us\":{},\"e2e_p99_us\":{}}}"
            ),
            self.frames,
            self.displayed,
            self.dropped_repeat,
            self.dropped_stale,
            self.dropped_budget,
            self.dropped_superseded,
            self.lost_in_network,
            self.decode_errors,
            self.unfinished,
            self.evicted,
            opt(self.e2e_p50_us),
            opt(self.e2e_p95_us),
            opt(self.e2e_p99_us),
        )
    }
}

/// 最近秩百分位数。`sorted` 必须已升序。
fn percentile(sorted: &[u32], p: f64) -> u32 {
    debug_assert!(!sorted.is_empty());
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(seq: u64, e2e_us: u32, outcome: Outcome) -> FrameTrace {
        let mut t = FrameTrace::new(FrameSeq(seq));
        t.mark(Mark::Captured, Timestamp(0));
        t.mark(Mark::Diffed, Timestamp(1000));
        t.mark(Mark::EncodeStart, Timestamp(1500));
        t.mark(Mark::EncodeDone, Timestamp(6000));
        t.mark(Mark::Sent, Timestamp(7000));
        t.mark(Mark::Received, Timestamp(12000));
        t.mark(Mark::Decoded, Timestamp(15000));
        t.mark(Mark::Presented, Timestamp(1000 + e2e_us));
        t.finish(outcome);
        t
    }

    #[test]
    fn marks_are_recorded() {
        let mut t = FrameTrace::new(FrameSeq(1));
        t.mark(Mark::Captured, Timestamp(100));
        assert_eq!(t.at(Mark::Captured), Some(Timestamp(100)));
        assert_eq!(t.at(Mark::Sent), None);
    }

    #[test]
    fn duplicate_mark_keeps_first() {
        let mut t = FrameTrace::new(FrameSeq(1));
        t.mark(Mark::Captured, Timestamp(100));
        t.mark(Mark::Captured, Timestamp(500));
        assert_eq!(t.at(Mark::Captured), Some(Timestamp(100)));
    }

    #[test]
    fn outcome_set_once() {
        let mut t = FrameTrace::new(FrameSeq(1));
        t.finish(Outcome::Displayed);
        t.finish(Outcome::DroppedStale);
        assert_eq!(t.outcome(), Some(Outcome::Displayed));
    }

    #[test]
    fn end_to_end_is_presented_minus_captured() {
        let t = filled(1, 20_000, Outcome::Displayed);
        // Presented = 1000 + 20000 = 21000, Captured = 0
        assert_eq!(t.end_to_end_us(), Some(21_000));
    }

    #[test]
    fn end_to_end_none_without_presented() {
        let mut t = FrameTrace::new(FrameSeq(1));
        t.mark(Mark::Captured, Timestamp(0));
        assert_eq!(t.end_to_end_us(), None);
    }

    #[test]
    fn latency_breakdown_has_expected_segments() {
        let t = filled(1, 20_000, Outcome::Displayed);
        let b: std::collections::HashMap<_, _> = t.latency_breakdown().into_iter().collect();
        assert_eq!(b["diff"], 1000);
        assert_eq!(b["sched_queue"], 500);
        assert_eq!(b["encode"], 4500);
        assert_eq!(b["packetize"], 1000);
        assert_eq!(b["network"], 5000);
        assert_eq!(b["decode"], 3000);
        assert_eq!(b["total"], 21_000);
    }

    /// 乱序时间戳不得下溢 panic
    #[test]
    fn breakdown_handles_out_of_order() {
        let mut t = FrameTrace::new(FrameSeq(1));
        t.mark(Mark::Captured, Timestamp(5000));
        t.mark(Mark::Diffed, Timestamp(1000));
        let b = t.latency_breakdown();
        assert_eq!(b[0].1, 0, "out-of-order must saturate to 0, not panic");
    }

    #[test]
    fn json_contains_all_marks() {
        let t = filled(7, 20_000, Outcome::Displayed);
        let j = t.to_json();
        assert!(j.contains("\"seq\":7"), "{j}");
        assert!(j.contains("\"captured_us\":0"), "{j}");
        assert!(j.contains("\"presented_us\":"), "{j}");
        assert!(j.contains("\"outcome\":\"displayed\""), "{j}");
        assert!(j.contains("\"e2e_us\":"), "{j}");
    }

    #[test]
    fn collector_evicts_oldest_when_full() {
        let mut c = TraceCollector::new(2);
        c.begin(FrameSeq(0));
        c.begin(FrameSeq(1));
        c.begin(FrameSeq(2));
        assert_eq!(c.len(), 2);
        assert_eq!(c.evicted(), 1);
        assert!(c.get(FrameSeq(0)).is_none());
        assert!(c.get(FrameSeq(2)).is_some());
    }

    #[test]
    fn collector_summary_counts_outcomes() {
        let mut c = TraceCollector::new(16);
        c.begin(FrameSeq(0)).finish(Outcome::Displayed);
        c.begin(FrameSeq(1)).finish(Outcome::DroppedRepeat);
        c.begin(FrameSeq(2)).finish(Outcome::DroppedStale);
        c.begin(FrameSeq(3));
        let s = c.summary();
        assert_eq!(s.frames, 4);
        assert_eq!(s.displayed, 1);
        assert_eq!(s.dropped_repeat, 1);
        assert_eq!(s.dropped_stale, 1);
        assert_eq!(s.unfinished, 1);
        assert_eq!(s.dropped_total(), 2);
        assert!((s.display_ratio() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn summary_percentiles_use_only_displayed_frames() {
        let mut c = TraceCollector::new(16);
        // 3 帧上屏，延迟分别 10/20/30ms
        for (i, e2e) in [10_000, 20_000, 30_000].iter().enumerate() {
            let mut t = FrameTrace::new(FrameSeq(i as u64));
            t.mark(Mark::Captured, Timestamp(0));
            t.mark(Mark::Presented, Timestamp(*e2e));
            t.finish(Outcome::Displayed);
            // 直接塞入内部（通过 begin + 复制）
            let slot = c.begin(FrameSeq(i as u64));
            *slot = t;
        }
        // 加一帧丢帧，其 e2e 不应参与统计
        let mut dropped = FrameTrace::new(FrameSeq(99));
        dropped.mark(Mark::Captured, Timestamp(0));
        dropped.mark(Mark::Presented, Timestamp(999_000));
        dropped.finish(Outcome::DroppedStale);
        *c.begin(FrameSeq(99)) = dropped;

        let s = c.summary();
        assert_eq!(s.e2e_p50_us, Some(20_000));
        assert_eq!(s.e2e_p99_us, Some(30_000));
    }

    #[test]
    fn summary_with_no_displayed_has_no_percentiles() {
        let mut c = TraceCollector::new(4);
        c.begin(FrameSeq(0)).finish(Outcome::DroppedRepeat);
        let s = c.summary();
        assert_eq!(s.e2e_p50_us, None);
    }

    #[test]
    fn summary_json_is_valid_shape() {
        let mut c = TraceCollector::new(4);
        c.begin(FrameSeq(0)).finish(Outcome::Displayed);
        let j = c.summary().to_json();
        assert!(j.starts_with('{') && j.ends_with('}'));
        assert!(j.contains("\"frames\":1"));
        assert!(j.contains("\"e2e_p50_us\":null"));
    }

    #[test]
    fn export_jsonl_one_line_per_frame() {
        let mut c = TraceCollector::new(4);
        c.begin(FrameSeq(0)).finish(Outcome::Displayed);
        c.begin(FrameSeq(1)).finish(Outcome::DroppedRepeat);
        let out = c.export_jsonl();
        assert_eq!(out.lines().count(), 2);
        for line in out.lines() {
            assert!(line.starts_with('{') && line.ends_with('}'));
        }
    }

    #[test]
    fn percentile_helper_edge_cases() {
        let v = [1u32, 2, 3, 4, 5];
        assert_eq!(percentile(&v, 0.0), 1);
        assert_eq!(percentile(&v, 50.0), 3);
        assert_eq!(percentile(&v, 100.0), 5);
    }
}
