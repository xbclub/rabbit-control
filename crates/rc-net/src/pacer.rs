//! 平滑发送（pacing）。
//!
//! # 为什么必须有 pacing（N3）
//! 编码器输出一帧后立刻 for 循环把几十个包全部塞进 socket，会让：
//! - 本地与运营商队列瞬间堆积（bufferbloat），延迟出现秒级尖峰；
//! - 丢包集中在突发尾部，FEC 组内一次丢多个包、恢复失败。
//!
//! pacing 把「一次发完」摊成「按速率逐个放行」。它不限制吞吐，只限制
//! 瞬时突发——队列永远只有一两个包在途。
//!
//! # 时间注入（R7）
//! [`Pacer::poll`] 的时刻由调用方传入（`now: Instant`），因此所有测试
//! 精确、无 sleep、无 flaky。生产路径由 `quic.rs` 的发送线程传入真实时钟。
//!
//! # 放行模型
//! 队列从空闲转忙时，**第一个包立即放行**（交互延迟优先，不引入人为启动延迟）；
//! 之后第 k 个包的放行时刻 = 前一个包的放行时刻 + 该包的串行化时间
//! `ceil(len * 8 * 1e6 / rate)` 微秒。速率含义与线路上的一致：按**整包字节**
//! （含 `rc-proto` 包头）计。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 平滑发送队列。
///
/// # 并发
/// 本类型不内部加锁。`quic.rs` 把它放进 `Mutex` 由发送线程独占驱动；
/// 这里保持纯粹的可测逻辑。
#[derive(Debug)]
pub struct Pacer {
    /// 当前速率（bps）。0 表示不限速（测试/诊断用，生产路径不会设 0）。
    rate_bps: u64,
    /// 待发送队列（FIFO，保证同一帧内分片按序上线）
    queue: VecDeque<Vec<u8>>,
    /// 队列总字节数（增量维护，O(1) 查询）
    queued_bytes: usize,
    /// 下一个包的最早放行时刻。`None` 表示队列空闲过，下一个包可立即发。
    next_release: Option<Instant>,
}

impl Pacer {
    /// 以初始速率构造。
    pub fn new(initial_rate_bps: u64) -> Self {
        Self {
            rate_bps: initial_rate_bps,
            queue: VecDeque::new(),
            queued_bytes: 0,
            next_release: None,
        }
    }

    /// 入队一个待发送的包。
    ///
    /// 入队不判速率——速率只影响 [`Self::poll`] 的放行节奏。
    /// 队列深度由上层（调度器的 ≤2 帧在途预算）间接约束。
    pub fn enqueue(&mut self, data: Vec<u8>) {
        self.queued_bytes += data.len();
        self.queue.push_back(data);
    }

    /// 取出当前时刻允许发送的包（可能 0 个或多个）。
    ///
    /// `now` 由调用方注入：测试用受控时刻，生产用 `Instant::now()`。
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(front) = self.queue.front() {
            let release = match self.next_release {
                Some(t) => t,
                // 队列刚由空闲转忙：首包立即放行，不叠加人为启动延迟。
                None => now,
            };
            if release > now {
                break;
            }
            let interval = send_interval(self.rate_bps, front.len());
            // checked_add 防 Instant 溢出；溢出时退化为「立即放行」，
            // 比 panic 更符合传输层「尽力而为」的定位。
            self.next_release = release.checked_add(interval).or(Some(now));
            let pkt = self.queue.pop_front().expect("front checked above");
            self.queued_bytes -= pkt.len();
            out.push(pkt);
        }
        if self.queue.is_empty() {
            // 空闲复位：下一次突发仍从「首包立即放行」开始，
            // 否则短暂的发送间隙会把后续帧整体推迟。
            self.next_release = None;
        }
        out
    }

    /// 更新速率（由拥塞控制驱动；M3 之前保持初始值）。
    pub fn set_rate(&mut self, bps: u64) {
        self.rate_bps = bps;
    }

    /// 队列积压字节数。
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// 当前速率（bps）。
    pub fn rate_bps(&self) -> u64 {
        self.rate_bps
    }

    /// 下一个包的放行时刻（无积压时 `None`）。发送线程据此决定睡多久。
    pub(crate) fn next_release_at(&self) -> Option<Instant> {
        self.next_release
    }
}

/// 一个 `len` 字节的包在 `rate_bps` 速率下占用链路多久。
///
/// `ceil` 取整保证任何包至少占 1µs（速率为 0 视为不限速，返回 0）。
fn send_interval(rate_bps: u64, len: usize) -> Duration {
    if rate_bps == 0 {
        return Duration::ZERO;
    }
    let bits = (len as u64).saturating_mul(8).saturating_mul(1_000_000);
    let us = bits.div_ceil(rate_bps).max(1);
    Duration::from_micros(us)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1250 字节 @ 10 Mbps 恰好 1ms，取整不引入误差，断言可以做成精确值。
    const PKT: usize = 1250;
    const RATE_10M: u64 = 10_000_000;
    const ONE_MS: Duration = Duration::from_millis(1);

    #[test]
    fn pacer_smooths_burst_sends() {
        // N3：100 个包的突发不能一次发出；总耗时 ≥ 计算值的一半（确实在平滑）。
        let mut p = Pacer::new(RATE_10M);
        for _ in 0..100 {
            p.enqueue(vec![0u8; PKT]);
        }
        assert_eq!(p.queued_bytes(), 100 * PKT);

        let t0 = Instant::now();
        let first = p.poll(t0);
        // 首包立即放行，且只放行一个：poll 的语义是「当前时刻允许发送的」，
        // 第二个包的放行时刻是 t0 + 1ms。
        assert_eq!(first.len(), 1, "100 包突发不能一次发出（N3）");

        // 计算值：100 包 × 1ms = 100ms；验收线 = 一半。
        let expected_total = 100 * ONE_MS;
        let mut got = first.len();
        let mut t = t0;
        while got < 100 {
            t += Duration::from_millis(15);
            got += p.poll(t).len();
        }
        assert_eq!(got, 100, "最终必须全部放行");
        let span = t.duration_since(t0);
        assert!(
            span >= expected_total / 2,
            "100 包突发耗时 {span:?} < 计算值的一半 {:?}（没有在平滑）",
            expected_total / 2
        );
        // 上界也校验：不应比计算值多出一个数量级（那说明把间隔算错了）。
        assert!(
            span <= expected_total * 2,
            "耗时 {span:?} 明显超过计算值 {expected_total:?}，节奏算错"
        );
        assert_eq!(p.queued_bytes(), 0);
    }

    #[test]
    fn pacer_respects_rate_change() {
        let mut p = Pacer::new(RATE_10M);
        for _ in 0..100 {
            p.enqueue(vec![0u8; PKT]);
        }
        let t0 = Instant::now();
        assert_eq!(p.poll(t0).len(), 1);

        // 10 Mbps 下，[t0, t0+5ms] 内放行 5 个（release 1ms..5ms）。
        let w1 = p.poll(t0 + Duration::from_millis(5));
        assert_eq!(w1.len(), 5, "10Mbps → 1ms/包，5ms 窗口应放行 5 个");

        // 提速到 100 Mbps → 100µs/包。
        p.set_rate(100_000_000);
        assert_eq!(p.rate_bps(), 100_000_000);
        // 之后的 10ms 窗口应放行大部分剩余包。
        let w2 = p.poll(t0 + Duration::from_millis(15));
        assert!(
            w2.len() > w1.len() * 4,
            "提速后节奏必须明显变快：w1={} w2={}",
            w1.len(),
            w2.len()
        );
        let drained = 1 + w1.len() + w2.len() + p.poll(t0 + Duration::from_secs(1)).len();
        assert_eq!(drained, 100, "所有包最终都被放行且不重复");
    }

    #[test]
    fn pacer_queued_bytes_tracks_backlog() {
        let mut p = Pacer::new(RATE_10M);
        p.enqueue(vec![1u8; 500]);
        p.enqueue(vec![2u8; 300]);
        p.enqueue(vec![3u8; 200]);
        assert_eq!(p.queued_bytes(), 1000, "积压 = 全部队列字节");

        let t0 = Instant::now();
        let out = p.poll(t0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vec![1u8; 500], "FIFO：先入先出");
        assert_eq!(p.queued_bytes(), 500, "放行一个 500B 包后剩 500");

        // 清空队列后积压归零，且空闲复位（下次首包立即放行）。
        let rest = p.poll(t0 + Duration::from_millis(50));
        assert_eq!(rest.len(), 2);
        assert_eq!(p.queued_bytes(), 0);
        assert_eq!(p.poll(t0 + Duration::from_millis(60)).len(), 0);
        p.enqueue(vec![9u8; 10]);
        assert_eq!(p.poll(t0 + Duration::from_millis(61)), vec![vec![9u8; 10]]);
    }

    #[test]
    fn send_interval_math() {
        // 1250B @10Mbps = 1ms 整
        assert_eq!(send_interval(RATE_10M, 1250), ONE_MS);
        // 1250B @100Mbps = 100µs 整
        assert_eq!(send_interval(100_000_000, 1250), Duration::from_micros(100));
        // 向上取整：1B @16Mbps = 0.5µs → 至少 1µs
        assert_eq!(send_interval(16_000_000, 1), Duration::from_micros(1));
        // 1B @8Mbps 恰好 1µs
        assert_eq!(send_interval(8_000_000, 1), Duration::from_micros(1));
        // 速率 0 = 不限速
        assert_eq!(send_interval(0, 9999), Duration::ZERO);
    }
}
