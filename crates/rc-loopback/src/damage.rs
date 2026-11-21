//! 网络损伤注入：丢包、单向延迟、抖动与带宽排队。
//!
//! # 为什么要有这一层
//! 回环测试若只在理想网络下跑，测出来的延迟与帧率无法代表真实链路；
//! 而真实链路一旦变差，出问题的是 FEC 策略、关键帧请求与调度器。
//! 本模块把这些网络条件**变成可复现的逻辑时间**，让上述策略在单测里就能被验证。
//!
//! # 三条实现约束
//! - **可复现**：随机源是 [`StdRng`]，种子固定为 `cfg.seed`。同样的种子与调用序列，
//!   产生逐字节相同的丢包/抖动序列，否则「3% 丢包下 Gate 通过」这种结论就不可信。
//! - **时间注入**：所有时刻由调用方通过 `now_us` 传入，本模块**不读系统时钟、不 sleep**。
//!   这样测试无需等待，也不会因机器负载而 flaky。
//! - **绝不产生负延迟**：延迟只会是 `rtt/2 + [0, jitter] + 排队`，全是无符号加法，
//!   没有任何减法路径，从类型层面排除了负延迟的可能。
//!
//! # 带宽模型
//! `bandwidth_bps > 0` 时按「串行化时间」模拟发送队列：
//! 一个 `n` 字节的包占用链路 `n * 8 / bandwidth_bps` 秒。包只能在前一个包发完后
//! 才开始发送，所以突发大包会把后续包的投递时间逐步推后——这正是真实链路的行为。
//! 被丢弃的包不占用带宽（它根本没上网）。
//!
//! # 队列有界（L9 / Q3）
//! 发送队列（等待占用链路、尚未开始发送的包）深度硬上限为 `max_queue_depth`
//! （默认 2）。满了就**丢新包**并计入 [`DamageInjector::dropped_by_queue`]，
//! 而不是继续排队——后者会让延迟随运行时长线性增长（正是 Q3 要避免的病灶：
//! 15kbps 下跑 60 秒能让 p50 涨到 30 秒）。
//! 已经在链路上传播的包（`send_at` 已过、等待 `deliver_at`）不计入队列深度：
//! 它们受 RTT 约束，不是排队积压。80ms RTT 下 60fps 天然有约 5 个包在途，
//! 若把它们也当队列，长 RTT 链路会无谓丢帧。

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::report::LoopbackConfig;

/// 待投递队列中的条目。
///
/// `serial` 是全局单调递增的入队序号，用于在投递时间相同时保持 FIFO；
/// 仅靠 `Vec` 的稳定排序不够直观，显式序号让「同时刻按入队顺序」成为可断言的性质。
#[derive(Debug)]
struct QueuedPacket {
    /// 应当投递的逻辑时刻（微秒）
    deliver_at_us: u32,
    /// 开始占用链路的时刻（微秒）。`> now` 表示它还在发送队列里排队。
    send_at_us: u32,
    /// 入队序号，越大表示越晚入队
    serial: u64,
    /// 数据报内容
    data: Vec<u8>,
}

/// 网络损伤注入器。
///
/// # 用法
/// ```text
/// let mut net = DamageInjector::new(&cfg);
/// // 发送端每产生一个数据报：
/// if let Some((deliver_at_us, data)) = net.process(now_us, packet) {
///     // 记录「该包应在 deliver_at_us 到达」
/// }
/// // 接收端每次推进时间：
/// for packet in net.poll_ready(now_us) {
///     // 按序交付给解码器
/// }
/// ```
///
/// # 确定性
/// 丢包判定与抖动采样共享同一个 [`StdRng`]，调用顺序固定为「先判丢包，再采抖动」。
/// 改动这个顺序会改变随机序列——已有测试的期望丢包位置会随之失效，属于破坏性变更。
#[derive(Debug)]
pub struct DamageInjector {
    /// 随机源。种子来自配置，保证跨运行可复现。
    rng: StdRng,
    /// 丢包率（0.0–1.0）
    loss: f32,
    /// 单程延迟（微秒），= `inject_rtt_us / 2`
    one_way_us: u32,
    /// 抖动上限（微秒），实际附加 `[0, jitter]`
    jitter_us: u32,
    /// 带宽上限（bps），0 表示不限速
    bandwidth_bps: u64,
    /// 发送队列深度硬上限（L9 / Q3）。来自 `LoopbackConfig::max_queue_depth`。
    max_queue_depth: usize,
    /// 待投递队列（未排序，`poll_ready` 时排序）
    queue: Vec<QueuedPacket>,
    /// 入队序号分配器
    next_serial: u64,
    /// 链路空闲时刻：下一个包最早能在何时开始发送（微秒）
    link_free_at_us: u32,
    /// 被随机丢包丢弃的包数
    dropped: u64,
    /// 交给 `process` 的包总数（含被丢弃的）
    processed: u64,
    /// 因发送队列满而丢弃的包数（拥塞丢弃，与随机丢包分开计数）
    dropped_by_queue: u64,
    /// 发送队列深度历史最大值（L9 的报告口径）
    max_queue_depth_seen: u32,
}

impl DamageInjector {
    /// 从回环配置构造。
    ///
    /// 只读取与网络相关的四个字段（`inject_loss`、`inject_rtt_us`、
    /// `inject_jitter_us`、`bandwidth_bps`）和随机种子 `seed`；
    /// 其余字段（fps、分辨率等）与本模块无关。
    ///
    /// 构造时不做配置校验——`LoopbackConfig::validate` 由上层负责，
    /// 这里对越界值保持「按数学定义执行」，避免出现第二套校验规则。
    pub fn new(cfg: &LoopbackConfig) -> Self {
        Self {
            rng: StdRng::seed_from_u64(cfg.seed),
            loss: cfg.inject_loss,
            // 单程 = RTT/2。整数除法向下取整，对奇数 RTT 最多少 1us，可忽略。
            one_way_us: cfg.inject_rtt_us / 2,
            jitter_us: cfg.inject_jitter_us,
            bandwidth_bps: cfg.bandwidth_bps,
            // 与调度器同一口径的硬上限；0 会被夹到 1，避免「队列永远满」。
            max_queue_depth: cfg.max_queue_depth.max(1),
            queue: Vec::new(),
            next_serial: 0,
            link_free_at_us: 0,
            dropped: 0,
            processed: 0,
            dropped_by_queue: 0,
            max_queue_depth_seen: 0,
        }
    }

    /// 处理一个待发送的数据报。
    ///
    /// 返回 `None` 表示被丢弃；返回 `Some((deliver_at_us, data))` 表示
    /// 该包应于 `deliver_at_us` 时刻投递，且已进入内部队列等待 [`poll_ready`](Self::poll_ready)。
    ///
    /// # 判定顺序
    /// 1. 伯努利丢包：丢则直接返回，不消耗带宽、不采抖动。
    /// 2. 发送队列满（L9）：丢新包并计入拥塞丢弃，不推进链路空闲时刻。
    /// 3. 计算发送时刻：受带宽排队影响（不限速时就是 `now_us`）。
    /// 4. 叠加单程延迟与抖动。
    ///
    /// 先判丢包是有意的：被丢弃的数据报从未上线，不应推迟后续包，
    /// 否则「丢包越严重链路越堵」的连锁反应会让测量结果失真。
    /// 队列拥塞丢弃同理——它也没上线，不该占用带宽。
    pub fn process(&mut self, now_us: u32, data: Vec<u8>) -> Option<(u32, Vec<u8>)> {
        self.processed += 1;

        // 落在 [0, 1) 的均匀采样，与 loss 比较即伯努利试验。
        // loss = 0 时永不命中（下界取不到 1.0 不影响 loss = 1 时必丢）。
        if self.rng.gen::<f32>() < self.loss {
            self.dropped += 1;
            return None;
        }

        // 先看「能否排队」：仍在等待上链路的包数已达上限，就丢这个新包。
        // 丢弃而不排队是 Q3 的核心：排队会让延迟随运行时长无界增长。
        let queued = self.send_queue_depth(now_us);
        if queued >= self.max_queue_depth {
            self.dropped_by_queue += 1;
            return None;
        }

        let (send_at_us, serialization_us) = self.plan_send(now_us, data.len());
        let jitter = if self.jitter_us > 0 {
            // 闭区间 [0, jitter]：抖动为 0 也是合法采样值。
            self.rng.gen_range(0..=self.jitter_us)
        } else {
            0
        };
        let deliver_at_us = send_at_us
            .saturating_add(self.one_way_us)
            .saturating_add(jitter);

        // 接受这个包，链路空闲时刻才前进。
        let finish_us = u64::from(send_at_us).saturating_add(u64::from(serialization_us));
        self.link_free_at_us = u32::try_from(finish_us).unwrap_or(u32::MAX);

        let serial = self.next_serial;
        self.next_serial += 1;
        let out = data.clone();
        self.queue.push(QueuedPacket {
            deliver_at_us,
            send_at_us,
            serial,
            data,
        });
        // 入队后深度 +1（该包此刻 `send_at > now` 时才真正在排队）。
        let depth = self.send_queue_depth(now_us) as u32;
        self.max_queue_depth_seen = self.max_queue_depth_seen.max(depth);
        Some((deliver_at_us, out))
    }

    /// 发送队列深度：仍在等待占用链路的包数（`send_at_us > now_us`）。
    ///
    /// 已经上链路传播的包不计入：它们受 RTT 约束，不是排队积压。
    fn send_queue_depth(&self, now_us: u32) -> usize {
        self.queue.iter().filter(|q| q.send_at_us > now_us).count()
    }

    /// 取出所有 `deliver_at_us <= now_us` 的包，按投递时间升序。
    ///
    /// 投递时间相同的包保持入队顺序（由入队序号保证），
    /// 因为同一帧的分片必须按原序交给解码器。
    ///
    /// 已过期的包（`deliver_at_us` 远小于 `now_us`）同样会被返回——
    /// 本模块不隐藏「迟到」的事实，迟到多少由调用方用 `now_us - deliver_at_us` 自行判断。
    pub fn poll_ready(&mut self, now_us: u32) -> Vec<Vec<u8>> {
        // 按 (投递时刻, 入队序号) 排序：稳定且与插入顺序无关。
        self.queue.sort_by_key(|q| (q.deliver_at_us, q.serial));
        let ready_count = self.queue.partition_point(|q| q.deliver_at_us <= now_us);
        self.queue.drain(..ready_count).map(|q| q.data).collect()
    }

    /// 观测到的丢包率（用于 FEC 自适应）。
    ///
    /// 定义是「已丢弃 / 已处理」，而不是配置里的 `inject_loss`：
    /// FEC 需要根据**实测**损失调整冗余度，配置值只是目标，不是事实。
    /// 尚未处理任何包时返回 0.0（没有观测就没有结论）。
    pub fn observed_loss(&self) -> f32 {
        if self.processed == 0 {
            return 0.0;
        }
        self.dropped as f32 / self.processed as f32
    }

    /// 已丢弃包数 / 已处理包数。
    ///
    /// 拆成两个绝对值而不是给一个比率，是为了让测试与报告能自行计算并做精确断言，
    /// 不受浮点舍入影响。
    pub fn stats(&self) -> (u64, u64) {
        (self.dropped, self.processed)
    }

    /// 因发送队列满而丢弃的包数 / 发送队列深度历史最大值。
    ///
    /// 与 [`stats`](Self::stats) 的随机丢包分开：拥塞丢包是**主动**行为
    /// （为了不让延迟无界增长），不应污染 FEC 观测到的链路丢包率。
    pub fn queue_stats(&self) -> (u64, u32) {
        (self.dropped_by_queue, self.max_queue_depth_seen)
    }

    /// 某长度的包最早能在何时开始发送，以及它占用链路多久（微秒）。
    ///
    /// 不限速（`bandwidth_bps == 0`）时立即发送，占用 0us。
    /// 否则 `send_at = max(now, link_free_at)`，
    /// 占用 `ceil(len * 8 * 1e6 / bandwidth_bps)` 微秒。
    ///
    /// **本函数不修改任何状态**：是否真的占用链路，由调用方在确认包被接受后
    /// 再用返回的占用时长推进 `link_free_at_us`。否则「算好了才发现队列满、
    /// 包被丢掉」会把链路白占一段时间，后续包被无谓推迟。
    fn plan_send(&self, now_us: u32, len: usize) -> (u32, u32) {
        if self.bandwidth_bps == 0 {
            return (now_us, 0);
        }
        let send_at_us = now_us.max(self.link_free_at_us);
        // 比特数 × 1e6 换算成微秒。先乘后除，减少整数除法带来的截断误差。
        let bits = (len as u64).saturating_mul(8).saturating_mul(1_000_000);
        let serialization_us = bits.div_ceil(self.bandwidth_bps);
        // 再快的链路上一个包也至少占用 1us：否则时间戳无法区分前后包，
        // 高带宽下的排队行为会退化成「全部同时到达」，与物理事实不符。
        (
            send_at_us,
            u32::try_from(serialization_us.max(1)).unwrap_or(u32::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个「理想网络」配置，测试按需覆盖字段。
    fn ideal_cfg() -> LoopbackConfig {
        LoopbackConfig {
            inject_loss: 0.0,
            inject_rtt_us: 0,
            inject_jitter_us: 0,
            bandwidth_bps: 0,
            seed: 42,
            ..Default::default()
        }
    }

    /// 跑 `n` 个包，返回每包是否送达（`true` = Some）。
    fn drop_pattern(mut net: DamageInjector, n: usize) -> Vec<bool> {
        (0..n)
            .map(|i| net.process(1_000, vec![i as u8]).is_some())
            .collect()
    }

    #[test]
    fn zero_loss_delivers_everything() {
        let cfg = ideal_cfg();
        let mut net = DamageInjector::new(&cfg);
        for i in 0..1_000u32 {
            assert!(
                net.process(i * 100, vec![1, 2, 3]).is_some(),
                "packet {i} must be delivered when inject_loss = 0"
            );
        }
        assert_eq!(net.stats(), (0, 1_000));
        assert_eq!(
            net.observed_loss(),
            0.0,
            "no drops observed means loss must be exactly 0"
        );
    }

    #[test]
    fn same_seed_produces_identical_drop_pattern() {
        let cfg = LoopbackConfig {
            inject_loss: 0.03,
            inject_rtt_us: 40_000,
            inject_jitter_us: 2_000,
            seed: 12_345,
            ..ideal_cfg()
        };
        let a = drop_pattern(DamageInjector::new(&cfg), 1_000);
        let b = drop_pattern(DamageInjector::new(&cfg), 1_000);
        assert_eq!(a, b, "same seed must produce byte-identical drop patterns");
    }

    #[test]
    fn different_seed_differs() {
        let cfg_a = LoopbackConfig {
            inject_loss: 0.1,
            seed: 1,
            ..ideal_cfg()
        };
        let cfg_b = LoopbackConfig {
            seed: 2,
            ..cfg_a.clone()
        };
        let a = drop_pattern(DamageInjector::new(&cfg_a), 200);
        let b = drop_pattern(DamageInjector::new(&cfg_b), 200);
        assert_ne!(
            a, b,
            "different seeds must not produce the same drop pattern (fake RNG?)"
        );
    }

    #[test]
    fn loss_rate_matches_config() {
        let cfg = LoopbackConfig {
            inject_loss: 0.03,
            seed: 7,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let n = 10_000u32;
        for i in 0..n {
            net.process(i, vec![0u8; 16]);
        }
        let (dropped, processed) = net.stats();
        assert_eq!(processed, u64::from(n));
        let rate = dropped as f64 / f64::from(n);
        assert!(
            (0.02..0.04).contains(&rate),
            "loss rate {rate:.4} outside expected 2%-4% for 3% config (dropped {dropped}/{n})"
        );
        assert!(
            (net.observed_loss() - rate as f32).abs() < 1e-6,
            "observed_loss {} must match stats ratio {rate}",
            net.observed_loss()
        );
    }

    #[test]
    fn rtt_injection_delays_by_half_rtt() {
        let cfg = LoopbackConfig {
            inject_rtt_us: 80_000,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let now = 5_000u32;
        let (deliver_at, data) = net
            .process(now, vec![9u8; 4])
            .expect("zero loss config must deliver");
        assert_eq!(
            deliver_at,
            now + 40_000,
            "one-way delay must be exactly rtt/2 = 40000us"
        );
        assert_eq!(data, vec![9u8; 4], "payload must pass through unchanged");
    }

    #[test]
    fn jitter_never_negative() {
        let cfg = LoopbackConfig {
            inject_rtt_us: 80_000,
            inject_jitter_us: 5_000,
            seed: 99,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let base = 1_000_000u32;
        for i in 0..1_000u32 {
            let now = base + i;
            let (deliver_at, _) = net.process(now, vec![0u8; 8]).unwrap();
            assert!(
                deliver_at >= now + 40_000,
                "packet {i}: deliver_at {deliver_at} < now {now} + rtt/2 40000 (negative-ish jitter)"
            );
        }
    }

    #[test]
    fn jitter_within_bounds() {
        let cfg = LoopbackConfig {
            inject_rtt_us: 80_000,
            inject_jitter_us: 5_000,
            seed: 99,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let base = 1_000_000u32;
        for i in 0..1_000u32 {
            let now = base + i;
            let (deliver_at, _) = net.process(now, vec![0u8; 8]).unwrap();
            let delay = deliver_at - now;
            assert!(
                (40_000..=45_000).contains(&delay),
                "packet {i}: delay {delay} outside [rtt/2, rtt/2 + jitter] = [40000, 45000]"
            );
        }

        // 端点可达性用 jitter = 1 验证：采样只可能落在 {40000, 40001}，
        // 1000 次下两端必然都出现（漏掉任一侧的概率约为 2^-1000）。
        // 换成 jitter = 5000 时，恰好采到 0 或上限的概率各约 0.02%，无法写进稳定断言。
        let edge_cfg = LoopbackConfig {
            inject_jitter_us: 1,
            seed: 99,
            ..cfg
        };
        let mut edge = DamageInjector::new(&edge_cfg);
        let mut saw_base = false;
        let mut saw_max = false;
        for i in 0..1_000u32 {
            let (deliver_at, _) = edge.process(base + i, vec![0u8; 8]).unwrap();
            let delay = deliver_at - (base + i);
            assert!(
                delay == 40_000 || delay == 40_001,
                "packet {i}: delay {delay} must be rtt/2 or rtt/2 + 1"
            );
            saw_base |= delay == 40_000;
            saw_max |= delay == 40_001;
        }
        assert!(
            saw_base && saw_max,
            "jitter must span its whole [0, jitter] range (base {saw_base}, max {saw_max})"
        );
    }

    #[test]
    fn bandwidth_limit_delays_packets() {
        // 1 Mbps 链路上发 12500 字节：每包串行化 100ms。
        let cfg = LoopbackConfig {
            bandwidth_bps: 1_000_000,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let payload = vec![0u8; 12_500];
        let times: Vec<u32> = (0..3)
            .map(|_| net.process(0, payload.clone()).unwrap().0)
            .collect();
        assert_eq!(
            times,
            vec![0, 100_000, 200_000],
            "queued packets must be spaced by serialization time 100000us, got {times:?}"
        );
        assert!(
            times.windows(2).all(|w| w[1] > w[0]),
            "delivery times must strictly increase under bandwidth limit, got {times:?}"
        );
    }

    /// L9 / Q3：发送队列有硬上限，满了丢新包，延迟不随运行时长增长。
    ///
    /// 旧实现只 push 不设限：限速链路上每帧的串行化时间大于帧间隔时，
    /// 队列无界增长。实测 15kbps / 60fps：5 秒 p50 = 2.6s、60 秒 p50 = 31.4s，
    /// 线性增长——正是 RustDesk 120 帧队列的病灶（Q3）。
    #[test]
    fn send_queue_is_bounded() {
        let cfg = LoopbackConfig {
            bandwidth_bps: 30_000,
            max_queue_depth: 2,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);

        // 64 字节 @30kbps = 17.1ms 串行化，远大于下面 1ms 的发送节奏。
        let payload = vec![0u8; 64];
        let mut accepted = 0u64;
        let mut rejected = 0u64;
        for i in 0..200u32 {
            if net.process(i * 1_000, payload.clone()).is_some() {
                accepted += 1;
            } else {
                rejected += 1;
            }
        }
        assert!(
            rejected > 0,
            "链路吞吐远低于发送速率时必须发生拥塞丢弃，实际 rejected = 0（accepted {accepted}）"
        );

        let (dropped_by_queue, max_depth) = net.queue_stats();
        assert_eq!(
            dropped_by_queue, rejected,
            "拥塞丢弃计数必须与 process 返回 None 的次数一致"
        );
        assert!(
            max_depth <= cfg.max_queue_depth as u32,
            "L9/Q3：发送队列深度 {max_depth} 超过上限 {}",
            cfg.max_queue_depth
        );
        // 随机丢包为 0：所有 None 都必须来自拥塞，不能混为一谈。
        assert_eq!(
            net.stats(),
            (0, 200),
            "本用例未配置随机丢包，stats 的 dropped 必须为 0"
        );
    }

    /// 队列深度只算「还在等上链路」的包，已在传播中的不计入。
    ///
    /// 否则长 RTT 链路会无谓丢帧：80ms RTT / 60fps 天然有约 5 个包在途。
    #[test]
    fn in_flight_packets_do_not_count_as_queue() {
        let cfg = LoopbackConfig {
            inject_rtt_us: 80_000,
            max_queue_depth: 2,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);

        // 不限速：每包立即上链路，全部处于「传播中」。
        for _ in 0..20 {
            assert!(
                net.process(0, vec![0u8; 64]).is_some(),
                "不限速时包立即上链路，不应被判为队列拥塞"
            );
        }
        let (dropped_by_queue, max_depth) = net.queue_stats();
        assert_eq!(dropped_by_queue, 0, "传播中的包不计入队列，不应丢包");
        assert_eq!(max_depth, 0, "无包在等待链路，队列深度必须为 0");

        // 20 个包都应在 40ms 后到期。
        assert!(net.poll_ready(39_999).is_empty(), "单程 40ms 前不应投递");
        assert_eq!(net.poll_ready(40_000).len(), 20, "单程 40ms 到时全部投递");
    }

    #[test]
    fn poll_ready_returns_in_order() {
        // 抖动会让入队顺序与投递顺序不一致，正好检验排序。
        let cfg = LoopbackConfig {
            inject_jitter_us: 10_000,
            seed: 3,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let mut inserted = Vec::new();
        for i in 0..200u32 {
            let (deliver_at, _) = net.process(0, i.to_be_bytes().to_vec()).unwrap();
            inserted.push(deliver_at);
        }
        assert!(
            inserted.windows(2).any(|w| w[1] < w[0]),
            "test setup ineffective: insertion order was already sorted"
        );

        let ready = net.poll_ready(u32::MAX);
        assert_eq!(ready.len(), 200, "all packets must be ready");
        let delivered: Vec<u32> = ready
            .iter()
            .map(|b| u32::from_be_bytes(b[..4].try_into().unwrap()))
            .collect();

        // 严格升序不足以判定「乱序」；这里检查输出顺序既非入队顺序，
        // 又能与「按投递时间排序」的结果一致。
        let mut expected = (0..200u32).collect::<Vec<_>>();
        expected.sort_by_key(|&i| inserted[i as usize]);
        assert_eq!(
            delivered, expected,
            "poll_ready must order by deliver_at, ties by insertion order"
        );
    }

    #[test]
    fn poll_ready_respects_time() {
        let cfg = LoopbackConfig {
            inject_rtt_us: 1_000,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        let (deliver_at, _) = net.process(500, vec![1]).unwrap();
        assert_eq!(deliver_at, 1_000);

        assert!(
            net.poll_ready(999).is_empty(),
            "packet must not be ready 1us before deliver_at"
        );
        let ready = net.poll_ready(1_000);
        assert_eq!(
            ready,
            vec![vec![1]],
            "packet must be ready exactly at deliver_at"
        );
        assert!(
            net.poll_ready(1_000).is_empty(),
            "packet must be removed after delivery"
        );
    }

    #[test]
    fn observed_loss_tracks_actual() {
        let cfg = LoopbackConfig {
            inject_loss: 0.05,
            seed: 2024,
            ..ideal_cfg()
        };
        let mut net = DamageInjector::new(&cfg);
        assert_eq!(
            net.observed_loss(),
            0.0,
            "no samples means loss is undefined-but-reported as 0.0"
        );
        for i in 0..2_000u32 {
            net.process(i, vec![0u8; 32]);
        }
        let (dropped, processed) = net.stats();
        let expected = dropped as f32 / processed as f32;
        assert_eq!(
            net.observed_loss(),
            expected,
            "observed_loss must equal dropped/processed ({dropped}/{processed})"
        );
        assert!(
            net.observed_loss() > 0.0,
            "5% loss over 2000 packets must drop something (got {dropped})"
        );
    }
}
