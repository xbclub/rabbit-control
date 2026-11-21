//! 假管线：采集 → 调度 → 编码 → 网络 → 解码 → 呈现。
//!
//! # 这个模块的职责
//! 把 T02 要求的「全链路」在**一个纯逻辑时钟**里跑完，让 Gate 判定、一致性
//! 不变式与分段延迟统计在无 GPU、无 socket 的环境下就能被验证。
//! 真实后端（T03/T04）接进来之后，只需要替换掉本模块里的四个假组件，
//! 而 [`run_pipeline`] 的调度、埋点与计时结构可以原样复用。
//!
//! # 三条设计约束
//!
//! - **绝不读系统时钟、绝不 sleep**：所有时刻都是调用方/主线推进的 `u32` 微秒。
//!   这样测试的期望值是精确的，也不会因为机器负载变成 flaky test。
//! - **编码器是串行资源**：`encode_cost_us` 不只是「延迟一段时间」，而是一次
//!   真实占用。编码未完成时新帧只能落在调度器的在途槽位里，槽位满了就**丢帧**
//!   ——这正是 Q3/Q6 要验证的行为。若把编码做成瞬时完成，调度器永远不丢帧，
//!   后面的所有验证都会变成「测试没测到东西」。
//! - **静止画面零编码（L8 / Q5）**：重复帧由 [`rc_diff::TileDiff`] 判定，
//!   调度器返回 `Drop(Repeat)` 后**绝不**再调用编码器，也不存在「保活编码」。
//!   首帧会先作为基线喂给 `TileDiff`（不进入统计），因此静止模式下连第一帧
//!   也不会被编码，`encoded` 严格等于 0。
//!
//! # 队列模型：一个 2 帧有界队列 + 一个串行编码器
//! 真实链路是「采集线程 → 有界通道 → 编码线程」：
//!
//! ```text
//! capture ──> [ 有界队列 ≤ 2 ] ──> (编码器正在处理的 1 帧) ──> network
//!             ↑ 调度器在途集合         ↑ 不在队列里
//! ```
//!
//! 两处细节决定这个模型是否真实：
//!
//! - **有界队列就是 `Scheduler::on_captured` 的在途集合**，不另建队列。
//!   若给编码器另建等待队列，2 帧上限就形同虚设（帧被批准后转进去、在途计数
//!   却已释放），队列能无声长到几百帧——这正是 RustDesk 的病症（Q3）。
//! - **编码器取件时释放队列槽位**（在 `EncodeStart` 调用 `on_encoded`）。
//!   调度器「挤出最旧的在途帧」会选中队列里最靠前的那一帧；若让正在编码的帧
//!   继续占位，被挤出的就总是它——编码时间白白浪费，吞吐会塌到
//!   `1 / encode_cost`（实测 2 秒只能上屏 3 帧），这既不真实也测不出东西。
//!   取件即出队才符合「队列深度」的原意：正在被处理的帧不在队列里。
//!
//! 由此得到两条可断言的不变式：
//! - `scheduler.in_flight_len() == pending_encode.len()`；
//! - 管线总深度 ≤ `max_queue_depth + 1`，而 `max_queue_depth_seen`
//!   只统计**队列**，硬上限就是 `cfg.max_queue_depth`（L9）。
//!
//! # 接收/发送侧也必须是有界队列（Q3）
//! 编码之后还有网络发送队列（[`DamageInjector`]），同样是硬上限
//! `max_queue_depth`，满了丢新包。若这里无界，限速链路上延迟会随运行时长
//! 线性增长——实测 15kbps 跑 60 秒能把 p50 推到 30 秒，与 RustDesk 的
//! 120 帧队列是同一个病。报告里的 `max_queue_depth_seen` 取**所有队列**
//! 的最大值，因此任一队列超限都会让 Gate 失败。
//!
//! # 时间线怎么采
//! [`rc_trace::TraceCollector`] 只提供 `begin(seq) -> &mut FrameTrace`，
//! 而同一帧的后续时间点（编码完成、接收、上屏）发生在写之后。因此本模块先为
//! 每帧持有自己的 [`FrameTrace`]，结束时再按序搬进收集器。语义完全一致：
//! 每一帧都会打上 `Captured / Diffed / EncodeStart / EncodeDone / Sent /
//! Received / Decoded / Presented`，并且**恰好一个结局**（`finish`）。
//!
//! # 全量计数 vs 采样
//! `TraceCollector` 是固定容量缓冲，长运行会淘汰最旧的帧。因此**计数**
//! （`PipelineRun::summary`）来自内部的结局计数器与分段累加器，不受容量影响；
//! `trace` 只用于逐帧诊断与分位数。`convergence_ms` 用的上屏时刻表同样
//! 独立保存（`PipelineRun::presented_us`），否则长运行只能看到最后若干帧。

use rc_diff::TileDiff;
use rc_sched::{Decision, DropReason, Scheduler, SchedulerConfig, SchedulerStats};
use rc_trace::{FrameTrace, Mark, Outcome, TraceCollector, TraceSummary};
use rc_types::{CpuFrame, FrameKind, FrameSeq, PixelFormat, Resolution, Timestamp};

use crate::damage::DamageInjector;
use crate::report::LoopbackConfig;

/// 假采集器：按 `fps` 产生帧，`static_content` 时产生内容相同的帧。
///
/// # 内部状态
/// 只维护**一块**像素缓冲并在原地修改，避免每帧分配 MB 级内存。
/// 动态内容每帧改写缓冲开头的 8 字节（内容计数器），保证 `TileDiff` 一定能
/// 检测到变化；静态内容则一次都不改，从第二帧起全部是重复帧。
///
/// # 首帧基线
/// 构造时会把全零画面喂给 `TileDiff` 作为「上一帧」。这样：
/// - 动态内容的首帧与基线不同 → 被判为非重复（会被编码）；
/// - 静态内容的首帧与基线相同 → 从第一帧起就是重复（L8 要求严格零编码）。
///
/// 这次预热调用不产生帧、不进入任何统计。
pub struct FakeCapture {
    /// 复用的像素缓冲（BGRA8）。
    frame: CpuFrame,
    /// 脏区判定器。首帧已在 `new` 中预热。
    diff: TileDiff,
    /// 内容是否静止。
    static_content: bool,
    /// 下一个帧序号。
    next_seq: u64,
    /// 内容版本号：动态模式下每帧自增并写入画面。
    content_counter: u64,
}

impl FakeCapture {
    /// 按配置创建采集器，并完成首帧基线预热。
    pub fn new(cfg: &LoopbackConfig) -> Self {
        let Resolution { width, height } = cfg.resolution;
        let stride = width.saturating_mul(4);
        let frame = CpuFrame {
            width,
            height,
            format: PixelFormat::Bgra8,
            stride,
            // 缓冲一次性分配；尺寸来自已校验的配置。
            data: vec![0u8; (stride as usize).saturating_mul(height as usize)],
        };
        let mut diff = TileDiff::with_defaults();
        // 预热：把全零画面记为「上一帧」。不产生帧、不进入统计。
        diff.update(&frame);
        Self {
            frame,
            diff,
            static_content: cfg.static_content,
            next_seq: 0,
            content_counter: 0,
        }
    }

    /// 推进到下一个采集时刻并产生一帧。
    ///
    /// 返回 `(帧序号, 采集时刻_us, 是否重复帧)`。
    /// 采集时刻就是调用方传入的 `now_us`——本模块不自行计算节拍，
    /// 由 [`run_pipeline`] 的逻辑时钟负责按 `frame_interval_us` 排程。
    pub fn next_frame(&mut self, now_us: u32) -> (FrameSeq, u32, bool) {
        self.content_counter += 1;
        if !self.static_content {
            // 只改开头 8 字节（极小分辨率下可能不足 8 字节，按实际长度截断）。
            // 改动足以让第一个 tile 的哈希变化，从而被判定为「有变化」。
            let bytes = self.content_counter.to_le_bytes();
            let n = self.frame.data.len().min(bytes.len());
            self.frame.data[..n].copy_from_slice(&bytes[..n]);
        }
        let seq = FrameSeq(self.next_seq);
        self.next_seq += 1;
        let is_repeat = self.diff.update(&self.frame).is_repeat;
        (seq, now_us, is_repeat)
    }
}

/// 假编码器：按 `encoder_slowdown` 计算编码耗时。
///
/// # 为什么是「耗时」而不是「速率」
/// 调度器的在途槽位从 `on_captured` 一直占到 `on_encoded`。把耗时做成
/// 可配置值，就能构造「编码器来不及」的场景（耗时 > 帧间隔），
/// 从而验证调度器在预算耗尽时丢帧而不是排队。
pub struct FakeEncoder {
    /// 减速倍数，1.0 = 正常。
    slowdown: f32,
}

impl FakeEncoder {
    /// 基准单帧编码耗时（未减速）。默认 5000us。
    pub const BASE_COST_US: u32 = 5_000;

    /// 按配置创建编码器。
    pub fn new(cfg: &LoopbackConfig) -> Self {
        // 负数/零由 `LoopbackConfig::validate` 拦截；这里再兜底一次，
        // 避免直接构造出「耗时 0」导致编码瞬时完成、调度器永不丢帧。
        Self {
            slowdown: cfg.encoder_slowdown.max(f32::MIN_POSITIVE),
        }
    }

    /// 返回编码耗时（微秒）。`slowdown = 2.0` 时恰好是基准的 2 倍。
    pub fn encode_cost_us(&self) -> u32 {
        let us = (Self::BASE_COST_US as f32 * self.slowdown).round();
        if us < 1.0 {
            1
        } else {
            // 浮点转整数的 `as` 是饱和转换，超大 slowdown 也不会回绕。
            us as u32
        }
    }
}

/// 假解码器。
#[derive(Debug, Default, Clone, Copy)]
pub struct FakeDecoder {}

impl FakeDecoder {
    /// 创建解码器。
    pub fn new() -> Self {
        Self {}
    }

    /// 返回解码耗时（微秒）。固定 3000us。
    pub fn decode_cost_us(&self) -> u32 {
        3_000
    }
}

/// 假呈现器。
#[derive(Debug, Default, Clone, Copy)]
pub struct FakePresenter {}

impl FakePresenter {
    /// 创建呈现器。
    pub fn new() -> Self {
        Self {}
    }

    /// 返回呈现耗时（微秒）。固定 2000us。
    pub fn present_cost_us(&self) -> u32 {
        2_000
    }
}

/// 管线运行结果（供 `report.rs` 组装 [`crate::report::LoopbackReport`] 的原始数据）。
pub struct PipelineRun {
    /// 帧时间线收集器。**受 `trace_capacity` 限制**，长运行只保留最近若干帧，
    /// 因此它只用于诊断与分段延迟，不能用来做全量计数。
    /// 全量计数见 [`Self::summary`]。
    pub trace: TraceCollector,
    /// 全量统计汇总（不受 `trace_capacity` 截断）。
    ///
    /// 计数来自管线内部的结局计数器，每一帧恰好记一次；
    /// 分位数仍取自 `trace`（采样），并在文档中说明。
    pub summary: TraceSummary,
    /// 所有上屏时刻（微秒，采集顺序入栈后已排序）。
    ///
    /// 与 `trace` 分开保存：`convergence_ms` / `degradation_ms` 要在运行开始
    /// 1 秒后立即判定，而长运行的 `trace` 早已被截断到尾部，用它扫描窗口
    /// 会得到完全错误的结论。
    pub presented_us: Vec<u32>,
    /// 调度器统计。
    ///
    /// **`encoded` 字段被校正为「真正完成编码的帧数」**：调度器在把帧收进
    /// 在途队列时就 +1，但被预算挤出的帧并没有编码。若照抄调度器原值，
    /// 报告的一致性不变式 `encoded == displayed + lost + decode_errors +
    /// unfinished` 会把「授权编码」误当成「真实编码」而不成立。
    pub sched_stats: SchedulerStats,
    /// 采集帧数。
    pub captured: u64,
    /// 编码帧数（真正产生码流并送入网络的帧）。
    pub encoded: u64,
    /// 上屏帧数。
    pub displayed: u64,
    /// 最大队列深度。
    ///
    /// 取**管线里所有有界队列**的历史最大值：调度器在途队列与网络发送队列。
    /// 两者各自受 `cfg.max_queue_depth` 约束（L9 / Q3），因此合并后的值
    /// 同样必须 ≤ `cfg.max_queue_depth`。只看其中一个会让另一个的积压隐形。
    pub max_queue_depth_seen: u32,
    /// 网络发送队列因拥塞（队列满）丢弃的包数。
    pub dropped_congestion: u64,
    /// 总运行时长（微秒）。取逻辑时长与最后一个事件的较大者，
    /// 保证跑完所有在途帧以便每帧都有结局。
    ///
    /// **不要**用它做 fps 分母：它含采集结束后的排空时间，会把延迟惩罚成帧率。
    /// fps 分母是请求的测量时长（`duration_us`）。
    pub elapsed_us: u32,
    /// 分段延迟累计（微秒），下标顺序为
    /// `diff, sched_queue, encode, packetize, network, decode, present`。
    ///
    /// 只累计结局为 `Displayed` 的帧。从全量结局时刻累计，而不是遍历
    /// `trace`——后者在长运行里已被截断到尾部，分段均值会失真。
    pub breakdown_sums: [u64; 7],
    /// 分段延迟样本数，下标含义同 [`Self::breakdown_sums`]。
    pub breakdown_counts: [u64; 7],
}

/// 跑一次完整的假管线。
///
/// `duration_us` 是逻辑时长（不是真实时间）。采集帧数 = `duration_us /
/// frame_interval_us`，首帧在 t=0，随后每 `frame_interval_us` 采集一帧。
///
/// # 事件顺序（同一时刻）
/// 1. 编码完成（释放调度器在途槽位，并立刻启动下一帧）；
/// 2. 网络投递（`DamageInjector::poll_ready`）；
/// 3. 解码完成、呈现完成；
/// 4. 采集。
///
/// 完成先于采集是刻意的：恰好落在采集时刻完成的编码应当能被新帧立刻复用槽位。
///
/// # 为什么编码器/解码器/呈现器都是串行资源
/// 真实链路里它们是单实例硬件/线程。串行化让「编码慢于采集」表现为
/// 排队时间（记录在 `sched_queue` 段）与丢帧，而不是让多个编码凭空并行。
pub fn run_pipeline(cfg: &LoopbackConfig, duration_us: u32) -> PipelineRun {
    match Sim::new(cfg, duration_us) {
        Some(mut sim) => {
            sim.run();
            sim.into_run(duration_us)
        }
        // 调度器构造失败只可能发生在配置被改坏时（见 `Sim::new` 的夹取逻辑）。
        // 不 panic：交出一份零帧的诚实报告，Gate 会因为「没有上屏帧」判失败，
        // 问题因此可见，而不是让调用方在跑一次仿真时崩掉。
        None => PipelineRun {
            trace: TraceCollector::new(cfg.trace_capacity),
            summary: TraceSummary::default(),
            presented_us: Vec::new(),
            sched_stats: SchedulerStats::default(),
            captured: 0,
            encoded: 0,
            displayed: 0,
            max_queue_depth_seen: 0,
            dropped_congestion: 0,
            elapsed_us: duration_us,
            breakdown_sums: [0; 7],
            breakdown_counts: [0; 7],
        },
    }
}

/// 事件记录：`(逻辑时刻_us, 帧序号)`。
type Event = (u32, FrameSeq);

/// 一次回环的完整模拟状态。
///
/// 所有字段都是逻辑时钟驱动的。事件表用 `Vec` 线性扫描：同时在途/在网
/// 帧数受限于 `max_in_flight` 与网络件数，规模是个位数，无需堆。
struct Sim<'a> {
    cfg: &'a LoopbackConfig,
    capture: FakeCapture,
    encoder: FakeEncoder,
    decoder: FakeDecoder,
    presenter: FakePresenter,
    net: DamageInjector,
    sched: Scheduler,
    /// 每帧一个时间线，下标 == `seq.0`。
    frames: Vec<FrameTrace>,
    /// 当前逻辑时刻。
    now: u32,
    /// 下一次采集时刻。
    next_capture_at: u32,
    /// 已采集帧数。
    captures_done: u64,
    /// 本次运行应采集的总帧数。
    total_captures: u64,
    /// 已获调度器批准、等待编码器空闲的帧（FIFO）。
    ///
    /// 这个队列**就是**调度器的在途集合：每次入队/出队都同步调用
    /// `on_captured` / `on_encoded`，因此 `pending_encode.len()` 恒等于
    /// `scheduler.in_flight_len()`，长度不会超过 `max_in_flight`。
    /// 正在编码的那一帧已出队，不在计数内（见模块文档的队列模型）。
    pending_encode: Vec<FrameSeq>,
    /// 编码器正在处理的帧（串行资源，最多一帧）。
    encoding: Option<FrameSeq>,
    /// `encoding` 的完成时刻。
    encode_done_at: Option<u32>,
    /// 已交给网络的帧的预计投递时刻。
    ///
    /// 只用于决定「下一次 `poll_ready` 应在何时调用」；投递本身仍由
    /// [`DamageInjector::poll_ready`] 完成（契约要求走这条路径）。
    delivery_ats: Vec<Event>,
    /// 待记录 `Decoded` 的事件。
    decode_dones: Vec<Event>,
    /// 待记录 `Presented` 的事件。
    present_dones: Vec<Event>,
    /// 解码器空闲时刻（串行资源）。
    decoder_free_at: u32,
    /// 呈现器空闲时刻（串行资源）。
    presenter_free_at: u32,
    /// 采集帧数。
    captured: u64,
    /// 真正完成编码的帧数。
    encoded: u64,
    /// 上屏帧数。
    displayed: u64,
    /// 在途队列历史最大深度（调度器队列与网络发送队列的较大者）。
    max_queue_depth_seen: u32,
    /// 网络发送队列因拥塞丢弃的包数。
    dropped_congestion: u64,
    /// 全量结局计数（不受 `trace_capacity` 影响）。
    ///
    /// 每帧 [`finish`](Self::finish) 时恰好 +1，因此各字段之和恒等于 `captured`。
    /// `trace.summary()` 只能数到缓冲里剩下的帧，长运行会少算，
    /// 不能用来做一致性校验。
    outcomes: TraceSummary,
    /// 所有上屏时刻（微秒）。窗口扫描需要全量数据，见 `PipelineRun::presented_us`。
    presented_us: Vec<u32>,
    /// 分段延迟累计（只有 `Displayed` 帧），下标含义见 `PipelineRun::breakdown_sums`。
    breakdown_sums: [u64; 7],
    /// 分段延迟样本数，下标含义见 `PipelineRun::breakdown_sums`。
    breakdown_counts: [u64; 7],
}

impl<'a> Sim<'a> {
    /// 构造模拟状态。调度器配置非法时返回 `None`（由 [`run_pipeline`] 兜底），
    /// 本函数不 panic。
    fn new(cfg: &'a LoopbackConfig, duration_us: u32) -> Option<Self> {
        let interval = cfg.frame_interval_us().max(1);
        // 配置本应已通过 `LoopbackConfig::validate`。这里把上限夹到调度器
        // 允许的区间，保证 `run_pipeline` 不会因为非法配置 panic。
        let max_in_flight = cfg.max_queue_depth.clamp(1, 8);
        let sched_cfg = SchedulerConfig {
            max_in_flight,
            ..Default::default()
        };
        // 夹取后 `max_in_flight ∈ [1, 8]`，默认的 `target_latency_us` 也合法，
        // 因此正常路径不会走到 `None`；真走到了说明夹取逻辑坏了，交回零帧报告。
        let sched = Scheduler::new(sched_cfg).ok()?;
        Some(Self {
            cfg,
            capture: FakeCapture::new(cfg),
            encoder: FakeEncoder::new(cfg),
            decoder: FakeDecoder::new(),
            presenter: FakePresenter::new(),
            net: DamageInjector::new(cfg),
            sched,
            frames: Vec::new(),
            now: 0,
            next_capture_at: 0,
            captures_done: 0,
            total_captures: u64::from(duration_us / interval),
            pending_encode: Vec::new(),
            encoding: None,
            encode_done_at: None,
            delivery_ats: Vec::new(),
            decode_dones: Vec::new(),
            present_dones: Vec::new(),
            decoder_free_at: 0,
            presenter_free_at: 0,
            captured: 0,
            encoded: 0,
            displayed: 0,
            max_queue_depth_seen: 0,
            dropped_congestion: 0,
            outcomes: TraceSummary::default(),
            presented_us: Vec::new(),
            breakdown_sums: [0; 7],
            breakdown_counts: [0; 7],
        })
    }

    /// 主循环：推进到最近的事件时刻，处理该时刻的全部事件，直到没有事件。
    ///
    /// 采集结束后仍会继续排空编码/网络/解码/呈现事件，
    /// 这是保证 `unfinished == 0` 的关键。
    fn run(&mut self) {
        while let Some(t) = self.next_event_time() {
            self.now = t;
            // 同一时刻可能连锁产生新事件（例如 t 时刻的投递触发 t 时刻的解码完成）。
            // 耗时均为正时不会发生，但下面的循环让这种可能性被安全处理。
            loop {
                let mut progressed = false;

                if self.encode_done_at.is_some_and(|at| at <= self.now) {
                    self.on_encode_done();
                    progressed = true;
                }
                if self.delivery_ats.iter().any(|(at, _)| *at <= self.now) {
                    self.on_deliveries();
                    progressed = true;
                }
                for seq in drain_due(&mut self.decode_dones, self.now) {
                    self.mark(seq, Mark::Decoded);
                    progressed = true;
                }
                for seq in drain_due(&mut self.present_dones, self.now) {
                    let presented_at = self.now;
                    self.mark(seq, Mark::Presented);
                    self.finish(seq, Outcome::Displayed);
                    self.displayed += 1;
                    self.presented_us.push(presented_at);
                    // 此刻该帧所有标记都已打点，可以就地累计分段延迟。
                    self.accumulate_breakdown(seq);
                    progressed = true;
                }

                if !progressed {
                    break;
                }
            }

            // 采集放在事件之后：同刻完成的编码可立即复用槽位。
            // 帧间隔 ≥ 1us，因此同一时刻最多采集一帧。
            if self.captures_done < self.total_captures && self.next_capture_at <= self.now {
                self.on_capture();
            }
        }
    }

    /// 最近一个待处理事件的时刻。没有事件返回 `None`。
    fn next_event_time(&self) -> Option<u32> {
        let mut next: Option<u32> = None;
        let mut consider = |at: u32| {
            next = Some(match next {
                Some(cur) => cur.min(at),
                None => at,
            });
        };

        if self.captures_done < self.total_captures {
            consider(self.next_capture_at);
        }
        if let Some(at) = self.encode_done_at {
            consider(at);
        }
        for events in [&self.delivery_ats, &self.decode_dones, &self.present_dones] {
            if let Some(&(at, _)) = events.iter().min_by_key(|(at, _)| *at) {
                consider(at);
            }
        }
        next
    }

    /// 采集一帧：打点 → 脏区判定（已在采集器内完成）→ 调度决策。
    fn on_capture(&mut self) {
        let (seq, capture_ts, is_repeat) = self.capture.next_frame(self.now);
        self.captured += 1;

        let mut trace = FrameTrace::new(seq);
        trace.mark(Mark::Captured, Timestamp(capture_ts));
        trace.mark(Mark::Diffed, Timestamp(capture_ts));
        // 帧序号由 `FakeCapture::next_seq` 每次 +1 生成，且 `frames` 只在这里 push，
        // 因此 `seq.0 == frames.len()` 由构造保证。这里**不做断言**：
        // `run()` 承诺不 panic（L7），而这条性质不可能被外部输入破坏。
        self.frames.push(trace);

        // 首帧按关键帧处理；后续 P 帧可被调度器丢弃。
        let kind = if seq.0 == 0 {
            FrameKind::Idr
        } else {
            FrameKind::P
        };

        let before = self.sched.in_flight_seqs();
        let dropped_before = self.sched.stats().dropped_budget;
        let decision =
            self.sched
                .on_captured(seq, kind, is_repeat, Timestamp(capture_ts), self.now);

        let depth = self.sched.in_flight_len() as u32;
        self.max_queue_depth_seen = self.max_queue_depth_seen.max(depth);

        // R1 的「挤出最旧帧」是静默发生的：调度器只 +1 `dropped_budget`，
        // 不告诉调用方被挤掉的是谁。用前后在途集合求差找出它，取消其编码
        // 计划并补上结局，否则这一帧会永远 unfinished。
        if self.sched.stats().dropped_budget > dropped_before {
            let after = self.sched.in_flight_seqs();
            if let Some(evicted) = before.into_iter().find(|s| !after.contains(s)) {
                self.evict(evicted);
            }
        }

        match decision {
            Decision::Encode { seq, .. } => {
                // 这里只是「批准」。真正开始时刻由编码器何时空闲决定；
                // 被批准但还没开始的帧照样占用调度器槽位（就是 pending_encode）。
                self.pending_encode.push(seq);
                self.pump_encoder();
            }
            Decision::Drop { seq, reason, .. } => {
                self.finish(seq, outcome_of(reason));
            }
        }

        self.captures_done += 1;
        self.next_capture_at = self
            .next_capture_at
            .saturating_add(self.cfg.frame_interval_us().max(1));

        // 假管线不建模 IDR 刷新（画面内容由假采集器决定），但要消费请求
        // 以免计数一直累积。真实后端应在这里触发关键帧。
        let _ = self.sched.take_keyframe_request();
    }

    /// 若编码器空闲则取出队首帧开始编码。
    ///
    /// **取件即出队**：`EncodeStart` 的同时调用 `on_encoded` 释放调度器槽位。
    /// 正在编码的帧不在「队列」里，因此队列深度就是 `pending_encode.len()`，
    /// 上限严格等于 `max_in_flight`（L9）。若不放行，调度器在队列满时挤出的
    /// 永远是最前面、正在编码的那一帧，编码时间全部白费。
    ///
    /// `EncodeStart` 记在真正开始的时刻。从 `Diffed` 到这里之间的等待
    /// 被统计为 `sched_queue`，绝不会混进 `network`（Q6）。
    fn pump_encoder(&mut self) {
        if self.encoding.is_some() {
            return;
        }
        if self.pending_encode.is_empty() {
            return;
        }
        let seq = self.pending_encode.remove(0);
        self.mark(seq, Mark::EncodeStart);
        // 出队 = 编码器已认领，调度器不必再为它保留在途额度。
        self.sched.on_encoded(seq, self.now);
        self.encoding = Some(seq);
        self.encode_done_at = Some(self.now.saturating_add(self.encoder.encode_cost_us()));
    }

    /// 编码完成：记录时间点、交给网络，然后立刻启动下一帧。
    fn on_encode_done(&mut self) {
        let Some(seq) = self.encoding.take() else {
            self.encode_done_at = None;
            return;
        };
        self.encode_done_at = None;
        self.mark(seq, Mark::EncodeDone);

        // 打包是假实现，耗时记为 0，因此 Sent 与 EncodeDone 同刻。
        self.mark(seq, Mark::Sent);
        self.encoded += 1;

        // 码流里带帧序号，供 `poll_ready` 取回时还原。
        let mut payload = vec![0u8; 64];
        payload[..8].copy_from_slice(&seq.0.to_le_bytes());

        let congestion_before = self.net.queue_stats().0;
        match self.net.process(self.now, payload) {
            Some((deliver_at, _)) => self.delivery_ats.push((deliver_at, seq)),
            None => {
                // 丢包必须立即定案，否则这帧没有结局。
                // 无论是随机丢包还是发送队列拥塞，都如实记为 LostInNetwork：
                // 对解码端而言两者都是「这帧没到」。拥塞单独计数供诊断用，
                // 不另立结局，否则一致性不变式里 `encoded == displayed + lost + ...`
                // 会对不上（拥塞丢的包确实编码过）。
                if self.net.queue_stats().0 > congestion_before {
                    self.dropped_congestion += 1;
                }
                self.finish(seq, Outcome::LostInNetwork);
            }
        }

        // 网络发送队列也是 Q3 的队列之一：把它并入报告的队列深度。
        self.max_queue_depth_seen = self.max_queue_depth_seen.max(self.net.queue_stats().1);

        self.pump_encoder();
    }

    /// 取出所有到期数据报并推进解码/呈现。
    fn on_deliveries(&mut self) {
        let ready = self.net.poll_ready(self.now);
        for data in ready {
            // 载荷前 8 字节是帧序号；长度异常时跳过（不应发生）。
            if data.len() < 8 {
                continue;
            }
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&data[..8]);
            let seq = FrameSeq(u64::from_le_bytes(raw));

            // 投递时刻就是本事件时刻：一个数据报一帧，因此 Received 精确。
            self.mark(seq, Mark::Received);

            let decode_start = self.now.max(self.decoder_free_at);
            let decoded_at = decode_start.saturating_add(self.decoder.decode_cost_us());
            self.decoder_free_at = decoded_at;
            self.decode_dones.push((decoded_at, seq));

            let present_start = decoded_at.max(self.presenter_free_at);
            let presented_at = present_start.saturating_add(self.presenter.present_cost_us());
            self.presenter_free_at = presented_at;
            self.present_dones.push((presented_at, seq));
        }
        // 未到期（或载荷异常的）记录保留；已处理的条目按时刻清掉。
        self.delivery_ats.retain(|(at, _)| *at > self.now);
    }

    /// 处理被调度器挤出的在途帧。
    ///
    /// 正在编码的帧已经出队、不占用调度器额度，因此被挤出的必然是
    /// `pending_encode` 里的一帧：直接移除，它永远不会开始编码。
    fn evict(&mut self, seq: FrameSeq) {
        if let Some(pos) = self.pending_encode.iter().position(|s| *s == seq) {
            self.pending_encode.remove(pos);
        }
        self.finish(seq, Outcome::DroppedBudget);
    }

    /// 在当前逻辑时刻记录一个标记。序号越界说明存在管线 bug，静默忽略。
    fn mark(&mut self, seq: FrameSeq, mark: Mark) {
        if let Some(frame) = self.frames.get_mut(seq.0 as usize) {
            frame.mark(mark, Timestamp(self.now));
        }
    }

    /// 累计一个已上屏帧的分段延迟。
    ///
    /// 段名与 [`rc_trace::FrameTrace::latency_breakdown`] 对齐；
    /// `total` 有意丢弃——它与端到端百分位是同一个量，混进来只会重复统计。
    fn accumulate_breakdown(&mut self, seq: FrameSeq) {
        let Some(frame) = self.frames.get(seq.0 as usize) else {
            return;
        };
        for (seg, us) in frame.latency_breakdown() {
            let idx = match seg {
                "diff" => 0,
                "sched_queue" => 1,
                "encode" => 2,
                "packetize" => 3,
                "network" => 4,
                "decode" => 5,
                "present" => 6,
                _ => continue,
            };
            self.breakdown_sums[idx] += u64::from(us);
            self.breakdown_counts[idx] += 1;
        }
    }

    /// 写入帧结局（`FrameTrace` 内部只接受第一次写入）。
    ///
    /// 全量计数在这里累加：无论 `trace` 缓冲是否已经把这一帧挤掉，
    /// 报告的一致性校验都要能数到它。`FrameTrace::finish` 拒绝第二次写入，
    /// 所以这里用返回值不可见，改用「帧是否已有结局」先行判断，保证只计一次。
    fn finish(&mut self, seq: FrameSeq, outcome: Outcome) {
        let Some(frame) = self.frames.get_mut(seq.0 as usize) else {
            return;
        };
        if frame.outcome().is_some() {
            return;
        }
        frame.finish(outcome);
        match outcome {
            Outcome::Displayed => self.outcomes.displayed += 1,
            Outcome::DroppedRepeat => self.outcomes.dropped_repeat += 1,
            Outcome::DroppedStale => self.outcomes.dropped_stale += 1,
            Outcome::DroppedBudget => self.outcomes.dropped_budget += 1,
            Outcome::DroppedSuperseded => self.outcomes.dropped_superseded += 1,
            Outcome::LostInNetwork => self.outcomes.lost_in_network += 1,
            Outcome::DecodeError => self.outcomes.decode_errors += 1,
        }
    }

    /// 把内部帧时间线搬进 [`TraceCollector`]，组装最终结果。
    fn into_run(self, duration_us: u32) -> PipelineRun {
        let capacity = self.cfg.trace_capacity;
        let mut trace = TraceCollector::new(capacity);
        for frame in self.frames {
            let slot = trace.begin(frame.seq());
            *slot = frame;
        }

        // 调度器统计的 `encoded` 含「被预算挤出、从未编码」的帧，
        // 校正为实际编码数，才能与报告的一致性不变式对齐。
        let mut sched_stats = self.sched.stats();
        sched_stats.encoded = self.encoded;

        // 全量计数 + 采样得到的端到端分位数。
        //
        // 计数不能从 `trace.summary()` 取：`trace` 受容量限制，长运行会丢最旧的帧，
        // 于是 `frames` 停在容量上而 `sched.encoded` 是全量，不变式必然误报。
        // 分位数只能从采样里算（全量保存所有 e2e 太贵），因此用 `trace` 的
        // 分位数补上，其余字段用全量计数。
        let sampled = trace.summary();
        let mut counts = self.outcomes;
        let finished = counts.displayed
            + counts.dropped_repeat
            + counts.dropped_stale
            + counts.dropped_budget
            + counts.dropped_superseded
            + counts.lost_in_network
            + counts.decode_errors;
        counts.frames = self.captured;
        // 未拿到结局的帧（正常路径为 0）。用饱和减法，避免任何计数异常把
        // `unfinished` 变成巨大的回绕值。
        counts.unfinished = self.captured.saturating_sub(finished);
        let summary = TraceSummary {
            e2e_p50_us: sampled.e2e_p50_us,
            e2e_p95_us: sampled.e2e_p95_us,
            e2e_p99_us: sampled.e2e_p99_us,
            evicted: sampled.evicted,
            ..counts
        };

        let mut presented_us = self.presented_us;
        // 网络抖动可以让后采集的帧先上屏，窗口扫描必须先按时刻排序。
        presented_us.sort_unstable();

        PipelineRun {
            trace,
            summary,
            presented_us,
            sched_stats,
            captured: self.captured,
            encoded: self.encoded,
            displayed: self.displayed,
            max_queue_depth_seen: self.max_queue_depth_seen,
            dropped_congestion: self.dropped_congestion,
            elapsed_us: self.now.max(duration_us),
            breakdown_sums: self.breakdown_sums,
            breakdown_counts: self.breakdown_counts,
        }
    }
}

/// 取出 `at <= now` 的事件并返回其帧序号。
fn drain_due(events: &mut Vec<Event>, now: u32) -> Vec<FrameSeq> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < events.len() {
        if events[i].0 <= now {
            out.push(events.remove(i).1);
        } else {
            i += 1;
        }
    }
    out
}

/// 调度器的丢弃原因 → 时间线结局。
fn outcome_of(reason: DropReason) -> Outcome {
    match reason {
        DropReason::Repeat => Outcome::DroppedRepeat,
        DropReason::Stale => Outcome::DroppedStale,
        DropReason::Budget => Outcome::DroppedBudget,
        DropReason::Superseded => Outcome::DroppedSuperseded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::LoopbackConfig;

    /// 小分辨率配置：tile 比对与缓冲分配都很便宜，测试跑得快。
    fn pipeline_cfg() -> LoopbackConfig {
        LoopbackConfig {
            resolution: Resolution::new(64, 64),
            fps: 60,
            ..Default::default()
        }
    }

    /// 从所有成功上屏的帧里取某个分段延迟的平均值（微秒）。
    fn avg_segment(run: &PipelineRun, name: &str) -> f64 {
        let mut sum = 0u64;
        let mut count = 0u64;
        for frame in run.trace.frames() {
            if frame.outcome() != Some(Outcome::Displayed) {
                continue;
            }
            for (seg, us) in frame.latency_breakdown() {
                if seg == name {
                    sum += u64::from(us);
                    count += 1;
                }
            }
        }
        assert!(count > 0, "no displayed frame carried segment {name}");
        sum as f64 / count as f64
    }

    /// L8 / Q5：静止画面下编码帧数必须严格为 0。
    #[test]
    fn static_content_encodes_zero_frames() {
        let mut cfg = pipeline_cfg();
        cfg.static_content = true;

        let run = run_pipeline(&cfg, 1_000_000);

        assert!(run.captured > 10, "测试应覆盖足够多帧");
        assert_eq!(
            run.encoded, 0,
            "L8/Q5 红线：静止画面的编码帧数必须严格为 0，实际 {}",
            run.encoded
        );
        assert_eq!(run.sched_stats.encoded, 0, "报告里的 encoded 也必须是 0");
        assert_eq!(
            run.trace.summary().dropped_repeat,
            run.captured,
            "静止模式下每一帧都应是 DroppedRepeat"
        );
    }

    /// L9 / Q3：任何时刻在途队列深度不得超过 `max_queue_depth`（默认 2）。
    #[test]
    fn queue_depth_capped_at_two() {
        let cfg = pipeline_cfg();
        let run = run_pipeline(&cfg, 2_000_000);

        assert!(run.captured > 0);
        assert!(
            run.max_queue_depth_seen <= cfg.max_queue_depth as u32,
            "L9/Q3：队列深度 {} 超过上限 {}",
            run.max_queue_depth_seen,
            cfg.max_queue_depth
        );
        // 默认配置下仍必须有帧上屏，避免「零队列 = 零工作」的假通过。
        assert!(run.displayed > 0);
    }

    /// L9 / Q3：**所有**队列都受同一上限约束——不只是调度器在途队列。
    ///
    /// 旧实现只统计 `sched.in_flight_len()`，网络发送队列完全不受约束：
    /// 限制到 15kbps 后，跑 60 秒 p50 涨到 31 秒（接收侧积压约 1886 个包），
    /// 而 `max_queue_depth_seen` 全程恒为 1、Gate 无任何 queue 项。
    /// 这条用例让发送队列成为瓶颈，断言延迟不随时长增长。
    #[test]
    fn all_queues_are_bounded_including_network() {
        let mut cfg = pipeline_cfg();
        cfg.bandwidth_bps = 15_000;

        let short = run_pipeline(&cfg, 2_000_000);
        let long = run_pipeline(&cfg, 20_000_000);

        for (label, run) in [("2s", &short), ("20s", &long)] {
            assert!(
                run.max_queue_depth_seen <= cfg.max_queue_depth as u32,
                "{label}：L9/Q3 队列深度 {} 超过上限 {}",
                run.max_queue_depth_seen,
                cfg.max_queue_depth
            );
            assert_eq!(
                run.summary.unfinished, 0,
                "{label}：每帧必须有结局，实际 unfinished = {}",
                run.summary.unfinished
            );
        }

        // 关键：延迟不得随运行时长增长（无界排队的特征）。
        let p50_of = |run: &PipelineRun| run.summary.e2e_p50_us.unwrap_or(0);
        let growth = p50_of(&long) as i64 - p50_of(&short) as i64;
        assert!(
            growth < 10_000,
            "限速下延迟不得随时长增长：2 秒 {}us → 20 秒 {}us（增长 {growth}us），增长说明发送队列无界",
            p50_of(&short),
            p50_of(&long)
        );

        // 拥塞确实发生了（否则测的是「链路够快」而不是「队列有界」）。
        assert!(
            long.dropped_congestion > 0,
            "15kbps 下 20 秒必须有拥塞丢弃，实际 {}",
            long.dropped_congestion
        );
        assert!(
            long.summary.lost_in_network > 0,
            "拥塞丢弃的包在时间线上必须记为 lost_in_network，实际 0"
        );
    }

    /// 动态内容必须真的产生码流（防止假实现把一切都丢掉）。
    #[test]
    fn dynamic_content_produces_frames() {
        let cfg = pipeline_cfg();
        let run = run_pipeline(&cfg, 1_000_000);

        assert!(run.captured > 0);
        assert!(
            run.encoded > 0,
            "动态内容必须编码，实际 encoded = {}",
            run.encoded
        );
        assert!(run.displayed > 0);
        assert!(run.displayed <= run.encoded);
        assert!(run.encoded <= run.captured);
    }

    /// 编码器慢一倍 → 调度器必须靠丢帧维持延迟，而不是无限排队。
    ///
    /// 60fps 时帧间隔 16.6ms，慢一倍也只有 10ms，来不及堆积；
    /// 因此这里用 120fps（间隔 8.3ms），让 10ms 的编码真实地落后于采集。
    #[test]
    fn encoder_slowdown_causes_budget_drops() {
        let mut fast = pipeline_cfg();
        fast.fps = 120;
        let baseline = run_pipeline(&fast, 2_000_000);
        assert_eq!(
            baseline.sched_stats.dropped_budget, 0,
            "正常编码器不应触发预算丢帧"
        );

        let mut slow = fast;
        slow.encoder_slowdown = 2.0;
        let run = run_pipeline(&slow, 2_000_000);
        assert!(
            run.sched_stats.dropped_budget > 0,
            "编码慢一倍时必须丢帧，实际 dropped_budget = {}",
            run.sched_stats.dropped_budget
        );
        // 丢帧是主动行为：必须仍有帧成功上屏。
        assert!(run.displayed > 0);
    }

    /// 减速只增加丢帧，不增加在途帧数，也不让端到端延迟随时间增长。
    ///
    /// # 判据为什么按「采集时刻」分段
    /// 旧版本按帧数中位切分再比较两段的**最大值**。这有两个统计缺陷：
    ///
    /// 1. 运行末尾还在管线里的帧要等排空（`run()` 会继续推进事件直到每帧
    ///    都有结局），它们的延迟天然偏高；按帧数切分会把这几帧全推进后半段，
    ///    于是「后半段最大值更大」恒成立，测的是排空效应而不是漂移。
    /// 2. 最大值对单个离群点极其敏感，20 万微秒的容差又松到能掩盖真实增长。
    ///
    /// 改为按**逻辑采集时刻**在中点切分，比较两段的 P50/P95（对离群点稳健、
    /// 能反映整段分布），并要求每段有足够样本，否则「比较」没有统计意义。
    /// 若管线真的让帧在队列里越积越久，后半段 P50 会随运行时长抬升，必然触发断言。
    #[test]
    fn slowdown_does_not_grow_latency() {
        let cfg = LoopbackConfig {
            fps: 120,
            encoder_slowdown: 2.0,
            ..pipeline_cfg()
        };
        let duration_us = 2_000_000u32;
        let run = run_pipeline(&cfg, duration_us);

        assert!(
            run.max_queue_depth_seen <= cfg.max_queue_depth as u32,
            "慢编码器下在途帧数 {} 仍必须 ≤ {}",
            run.max_queue_depth_seen,
            cfg.max_queue_depth
        );
        let summary = run.trace.summary();
        assert_eq!(summary.unfinished, 0, "所有帧（含被挤出的）都必须有结局");

        // 关键：队列上限 2 + 编码 10ms + 解码/呈现 5ms，
        // 端到端延迟应稳定在几十毫秒，而不是随运行时长线性增长。
        let p95 = summary.e2e_p95_us.expect("必须有帧上屏才能统计端到端延迟");
        assert!(
            p95 < 100_000,
            "慢编码器不得让延迟积累到 100ms 以上，实测 p95 = {p95}us"
        );

        // 按采集时刻切分前/后半段；只统计上屏帧（没走完流程的帧没有 e2e）。
        let mut head: Vec<u32> = Vec::new();
        let mut tail: Vec<u32> = Vec::new();
        for frame in run.trace.frames() {
            if frame.outcome() != Some(Outcome::Displayed) {
                continue;
            }
            let (Some(captured), Some(e2e)) = (frame.at(Mark::Captured), frame.end_to_end_us())
            else {
                continue;
            };
            if u64::from(captured.0) < u64::from(duration_us) / 2 {
                head.push(e2e);
            } else {
                tail.push(e2e);
            }
        }

        // 编码吞吐 = 1 / 10ms = 100fps，2 秒每段应各有约 100 帧上屏。
        // 这个下限同时是「管线还能出货」的体检：若编码器被在途槽位卡住
        // （Q3 的旧病灶），上屏帧数会塌到个位数，这里会立刻失败。
        let min_samples = 50;
        assert!(
            head.len() >= min_samples && tail.len() >= min_samples,
            "样本不足以判断漂移：前半段 {} 帧、后半段 {} 帧，各需 ≥ {min_samples}",
            head.len(),
            tail.len()
        );

        // 允许的抬升：相对 10% + 2ms 绝对余量。实测无漂移时两段 P50 相差 <0.2ms。
        let head_p50 = percentile_us(&head, 50.0);
        let tail_p50 = percentile_us(&tail, 50.0);
        assert!(
            tail_p50 <= head_p50 * 1.1 + 2_000.0,
            "端到端延迟中位数随运行时长增长：前半段 {head_p50:.0}us，后半段 {tail_p50:.0}us"
        );

        let head_p95 = percentile_us(&head, 95.0);
        let tail_p95 = percentile_us(&tail, 95.0);
        assert!(
            tail_p95 <= head_p95 * 1.1 + 2_000.0,
            "端到端延迟 P95 随运行时长增长：前半段 {head_p95:.0}us，后半段 {tail_p95:.0}us"
        );
    }

    /// 从样本中取百分位（微秒，最近秩）。
    ///
    /// 测试专用：与 `rc_trace` 内部实现同一定义，避免为一个断言把 crate 私有
    /// 工具函数变成公共 API。
    fn percentile_us(samples: &[u32], p: f64) -> f64 {
        assert!(!samples.is_empty(), "百分位需要非空样本");
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let rank = (p / 100.0 * sorted.len() as f64).ceil();
        let idx = (rank as usize).saturating_sub(1).min(sorted.len() - 1);
        f64::from(sorted[idx])
    }

    /// 埋点完整性：即使有丢包，每一帧也必须恰好有一个结局。
    #[test]
    fn every_frame_has_outcome() {
        let cfg = LoopbackConfig {
            inject_loss: 0.2,
            seed: 7,
            ..pipeline_cfg()
        };
        let run = run_pipeline(&cfg, 1_000_000);
        let summary = run.trace.summary();

        assert_eq!(
            summary.unfinished, 0,
            "L10：unfinished 必须为 0，实际 {}",
            summary.unfinished
        );
        assert_eq!(summary.frames, run.captured);
        assert!(summary.lost_in_network > 0, "20% 丢包下应有帧被网络丢弃");
    }

    /// Q6：network 段反映注入的 RTT，且排队时间不会混进来。
    #[test]
    fn rtt_injection_shows_in_breakdown() {
        let cfg = LoopbackConfig {
            fps: 30,
            inject_rtt_us: 80_000,
            ..pipeline_cfg()
        };
        let run = run_pipeline(&cfg, 1_000_000);

        // 单程 = 80ms / 2 = 40ms，±1ms 容差。
        let network = avg_segment(&run, "network");
        assert!(
            (39_000.0..=41_000.0).contains(&network),
            "network 段应 ≈ 40000us，实际 {network:.0}us"
        );
        // sched_queue 是独立分段；正常编码器下应远小于网络段。
        let sched_queue = avg_segment(&run, "sched_queue");
        assert!(
            sched_queue < 5_000.0,
            "sched_queue 不应吸收网络延迟，实际 {sched_queue:.0}us"
        );
    }

    /// 没有隐藏的系统时钟依赖：同样输入两次运行必须逐字节一致。
    #[test]
    fn no_system_clock_used() {
        let cfg = pipeline_cfg();
        let a = run_pipeline(&cfg, 1_000_000);
        let b = run_pipeline(&cfg, 1_000_000);

        assert_eq!(a.captured, b.captured);
        assert_eq!(a.encoded, b.encoded);
        assert_eq!(a.displayed, b.displayed);
        assert_eq!(a.max_queue_depth_seen, b.max_queue_depth_seen);
        assert_eq!(a.elapsed_us, b.elapsed_us);
        assert_eq!(a.sched_stats, b.sched_stats);
        assert_eq!(a.trace.summary(), b.trace.summary());
        assert_eq!(
            a.trace.export_jsonl(),
            b.trace.export_jsonl(),
            "两次运行的时间线必须完全一致（存在时间/随机依赖就会不同）"
        );
    }

    /// 帧数由 duration 与 fps 决定，误差不超过 1。
    #[test]
    fn capture_produces_expected_frame_count() {
        let cfg = pipeline_cfg();
        let run = run_pipeline(&cfg, 1_000_000);
        let expected = 1_000_000 / cfg.frame_interval_us();

        assert!(
            (run.captured as i64 - expected as i64).abs() <= 1,
            "1s @60fps 应约 {expected} 帧，实际 {}",
            run.captured
        );
    }
}
