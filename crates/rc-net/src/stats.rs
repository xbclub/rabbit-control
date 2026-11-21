//! 统计与拥塞信号。
//!
//! `NetStats` 里的 `rtt_us` / `cwnd_bytes` 直接来自 quinn 的连接统计；
//! `loss_rate` 没有现成来源——QUIC 不会告诉你「应用层丢了多少媒体包」
//! （QUIC 自己的丢包会被重传掩盖，而 datagram 丢了就是丢了，无迹可查
//! 除非应用层自己数）。
//!
//! 本模块的 [`LossEstimator`] 用 **NACK 语义**估计丢包率：接收端知道每帧
//! 应到多少分片（`packet_cnt`）、实到多少；差值就是「缺失包数」。FEC
//! 恢复成功与否都计入——恢复成功说明链路确实丢了，只是被冗余救回。

/// 由「应到 / 实到」差值（NACK）推断的丢包率估计器。
///
/// # 为什么不直接用 QUIC 的丢包事件
/// QUIC 统计的是**它自己的包**（带重传的 stream 流量 + 探测包），与
/// 媒体 datagram 的实际丢失率是两回事。FEC 的冗余率（`rc-fec` 的
/// `ratio_for_loss`）需要的是**媒体通道**的实测丢包，所以必须自己数。
///
/// # 时间注入
/// 本类型只做计数，不涉时间；「哪些帧算丢」由调用方（重组层的 gc 与
/// 完成回调）判定，时刻语义全部留在调用方。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LossEstimator {
    /// 实际到达的分片数（含 FEC 恢复出来的）
    arrivals: u64,
    /// 缺失（NACK）的分片数
    nacks: u64,
}

impl LossEstimator {
    /// 空估计器（无样本时 `loss_rate` 恒为 0）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录 `n` 个分片到达。
    pub fn observe_arrival(&mut self, n: u64) {
        self.arrivals = self.arrivals.saturating_add(n);
    }

    /// 记录 `n` 个分片缺失（NACK）。
    ///
    /// 一帧 10 包只到了 7 个 → `observe_arrival(7)` + `observe_nack(3)`。
    pub fn observe_nack(&mut self, n: u64) {
        self.nacks = self.nacks.saturating_add(n);
    }

    /// 实测丢包率 = `nacks / (arrivals + nacks)`；无样本时返回 0.0
    /// （没有观测就没有结论，不能拿 0 除数制造 NaN）。
    pub fn loss_rate(&self) -> f32 {
        let total = self.arrivals.saturating_add(self.nacks);
        if total == 0 {
            return 0.0;
        }
        self.nacks as f32 / total as f32
    }

    /// 到达分片数（供报告展示）。
    pub fn arrivals(&self) -> u64 {
        self.arrivals
    }

    /// 缺失分片数（供报告展示）。
    pub fn nacks(&self) -> u64 {
        self.nacks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_loss_rate_computed_from_nacks() {
        // N8：丢包率必须由 NACK（缺失）计算，且是媒体通道口径。
        let mut e = LossEstimator::new();
        assert_eq!(e.loss_rate(), 0.0, "无样本时必须返回 0.0，不能是 NaN");

        // 100 个应到分片：97 到达、3 缺失 → 3%。
        e.observe_arrival(97);
        e.observe_nack(3);
        assert!((e.loss_rate() - 0.03).abs() < 1e-6, "got {}", e.loss_rate());
        assert_eq!(e.arrivals(), 97);
        assert_eq!(e.nacks(), 3);

        // 持续累积：再加 100 到达、5 缺失 → 8/205 ≈ 0.03902
        e.observe_arrival(100);
        e.observe_nack(5);
        let expect = 8.0f32 / 205.0;
        assert!((e.loss_rate() - expect).abs() < 1e-6);
    }

    #[test]
    fn loss_estimator_counts_saturate() {
        // 恶意/异常流量不应导致计数溢出翻转成小比率。
        let mut e = LossEstimator::new();
        e.observe_nack(u64::MAX);
        e.observe_nack(1);
        assert_eq!(e.nacks(), u64::MAX);
        e.observe_arrival(u64::MAX);
        e.observe_arrival(1);
        assert_eq!(e.arrivals(), u64::MAX);
        // 总数饱和后比率退化为 0.5 左右，但绝不为 NaN。
        assert!(!e.loss_rate().is_nan());
    }
}
