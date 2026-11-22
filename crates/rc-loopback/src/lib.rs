//! 回环测试框架：在单进程内跑通「采集 → 编码 → 解码 → 呈现」。
//!
//! # 为什么必须先做这个
//! M1 的核心问题不是「能不能编解码」，而是「延迟到底多少」。
//! 没有测量框架，所有优化都只能靠猜。本 crate 的价值不在于它自己多准，
//! 而在于它把**报告 schema、一致性不变式、Gate 判定**先固定下来，
//! 之后接入真实后端（T12/T05）时可以立刻用同一套指标验收。
//!
//! # 与真实链路的区别
//! 本 crate 全部是**假实现**：
//! - 不接 DXGI / ScreenCaptureKit
//! - 不接 NVENC / VideoToolbox
//! - 用逻辑时钟推进，不 `sleep`
//!
//! 这样做的原因是：测量逻辑本身必须先在无 GPU 环境下被验证正确，
//! 否则接入真实后端时，无法区分「延迟高」是后端的锅还是框架的锅。
//!
//! # 六条硬性要求（对应 `docs/reference/tickets/T02-loopback-skeleton.md`）
//! - **L8** `static_content` 时 `encoded_fps == 0`（Q5：静止画面零编码）
//! - **L9** 接收队列深度硬上限 2 帧（Q3：RustDesk 是 120 帧）
//! - **L10** 报告含 `convergence_ms`（Q1/Q4：量化爬坡速度）
//!
//! 其余 L1–L7 见各自模块文档。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::fmt;
use std::time::Duration;

pub mod damage;
pub mod fake;
pub mod report;

pub use damage::DamageInjector;
pub use fake::PipelineRun;
pub use report::{BreakdownMs, GateResult, LoopbackConfig, LoopbackReport};

/// 本 crate 的错误类型。
///
/// # 为什么不引 `thiserror`
/// 本 crate 的错误只有一个变体，为一个 `Display` 实现引入依赖不值得。
/// 手写 [`fmt::Display`] 与 [`std::error::Error`] 的代价远小于多一个传递依赖。
/// 上游若需要统一错误类型，可以照常 `?` 转换（实现了 `Error` 即可装箱/包装）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackError {
    /// 配置非法。字符串是 [`LoopbackConfig::validate`] 给出的具体原因。
    InvalidConfig(String),
    /// 真实管线运行期失败（后端构造 / tick 致命错误 / strict 模式降级）。
    Runtime(String),
}

impl fmt::Display for LoopbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoopbackError::InvalidConfig(reason) => {
                write!(f, "invalid loopback config: {reason}")
            }
            LoopbackError::Runtime(reason) => write!(f, "real pipeline failure: {reason}"),
        }
    }
}

impl std::error::Error for LoopbackError {}

/// 本 crate 的 `Result` 别名。
pub type Result<T> = std::result::Result<T, LoopbackError>;

/// 回环框架入口：持有配置，按指定时长跑一次假管线并产出报告。
///
/// # 不持有运行状态
/// 每次 [`run`](Self::run) 都从零开始构造管线（采集器、损伤注入器、调度器
/// 全部重新创建），因此两次 `run` 之间没有残留状态，`seed` 固定的运行
/// 必然逐字节一致。这一点是可复现性的基础，也是「不要复用 RNG」的原因。
#[derive(Debug, Clone)]
pub struct Loopback {
    /// 已通过 `validate` 的配置快照。
    cfg: LoopbackConfig,
}

impl Loopback {
    /// 按配置创建回环实例。配置非法时返回 [`LoopbackError::InvalidConfig`]。
    ///
    /// 校验放在构造函数里而不是 `run` 里：配置错误是**调用方错误**，
    /// 应该在拿到实例时就知道，而不是跑完一轮才失败。
    pub fn new(cfg: LoopbackConfig) -> Result<Self> {
        cfg.validate().map_err(LoopbackError::InvalidConfig)?;
        Ok(Self { cfg })
    }

    /// 跑指定时长，返回报告。不 panic，错误通过 `Result` 返回。
    ///
    /// # 时长换算
    /// `duration` 换算成逻辑微秒后交给 [`fake::run_pipeline`]。端点事件时刻是
    /// `u32` 微秒（与 `rc_trace::Timestamp` 同宽），超过 `u32::MAX` 微秒
    /// （约 71 分钟）的时长会**饱和**到上限而不是回绕。
    ///
    /// # 报告里的数值怎么来的
    /// - `duration_s`：`run(duration)` 传入的**测量时长**，不是排空后的总时长。
    ///   三个 fps 都用它做分母——若用 `PipelineRun::elapsed_us`（含采集结束后
    ///   排空在途帧的时间），注入 400ms RTT 时 2 秒跑 120 帧会被算成 54.7fps，
    ///   延迟惩罚会被错误地记成掉帧。
    /// - `latency_p*_ms` / `summary`：全量计数与采样分位数（见
    ///   [`fake::PipelineRun::summary`]）；不受 `trace_capacity` 截断影响。
    /// - `breakdown_ms`：只统计**结局为 `Displayed`** 的帧，按
    ///   `FrameTrace::latency_breakdown` 的真实段名逐段求平均（见
    ///   `average_breakdown` 的说明：`total` 段被丢弃，`diff` 与 `packetize`
    ///   在假管线里恒为 0）。
    /// - `convergence_ms` / `degradation_ms`：**整 1 秒**滑动窗口的显示帧率扫描
    ///   （见 `scan_fps_windows`）。
    ///
    /// # 错误
    /// 目前所有运行期错误都在构造期被拦下，因此本方法总是 `Ok`；
    /// 保留 `Result` 返回类型是为了契约稳定（将来真实后端可能返回运行期错误）。
    pub fn run(&mut self, duration: Duration) -> Result<LoopbackReport> {
        // 饱和而非回绕：`Duration::as_micros` 是 u128，直接 `as u32` 会截断。
        let duration_us = u32::try_from(duration.as_micros()).unwrap_or(u32::MAX);
        let run = fake::run_pipeline(&self.cfg, duration_us);

        // 分母用请求的测量时长，而不是含排空的 elapsed_us（见上方说明）。
        let duration_s = f64::from(duration_us) / 1_000_000.0;
        let summary = run.summary;
        let breakdown_ms = average_breakdown(&run);
        let (convergence_ms, degradation_ms) = scan_fps_windows(&run, self.cfg.fps);

        let mut report = LoopbackReport {
            config: self.cfg.clone(),
            duration_s,
            summary,
            sched_stats: run.sched_stats,
            latency_p50_ms: us_to_ms(summary.e2e_p50_us),
            latency_p95_ms: us_to_ms(summary.e2e_p95_us),
            latency_p99_ms: us_to_ms(summary.e2e_p99_us),
            displayed_fps: fps_of(run.displayed, duration_s),
            encoded_fps: fps_of(run.encoded, duration_s),
            captured_fps: fps_of(run.captured, duration_s),
            breakdown_ms,
            convergence_ms,
            degradation_ms,
            max_queue_depth_seen: run.max_queue_depth_seen,
            probe_enabled: self.cfg.probe_enabled,
            // 假管线没有 CPU 计量，如实报 null，而不是编一个 0%。
            static_cpu_pct: None,
            encoder_label: None,
            // 门槛随目标帧率而定（见 `GateResult::for_config`）。
            gate: GateResult::for_config(&self.cfg),
        };
        // 契约要求：不变式必须在 run() 结束时校验一次，而不是只放在测试里。
        report.apply_gate();
        Ok(report)
    }

    /// 用**真实后端**跑一次回环（T08 M1 Gate 的入口）。
    ///
    /// 与 [`run`](Self::run)（逻辑时钟假管线）的区别：
    /// - 后端经 [`rc_pipeline`] 工厂选择：`prefer_hardware == true` 时先试
    ///   真实后端（SCK / VideoToolbox / Metal），构造失败降级假后端并打
    ///   标签到 stderr（P8：不静默降级）；`false` 全假后端 + 真实时钟。
    /// - 时钟是**真实墙钟**（`Instant`），会话零点定在**首个采集帧到达时刻**：
    ///   SCK 的时间戳以它自己的首帧为原点，若以循环开始为零点，流启动延迟
    ///   会整段错位成「超期丢弃」。
    /// - 假后端瞬时返回，驱动器按帧间隔节流（`fake_pace`）；真实采集的
    ///   `next_frame` 自带 16ms 阻塞语义，无需节流。
    /// - `encoder_slowdown` 只作用于假编码器（真实硬件没有这个旋钮）。
    ///
    /// # 报告口径与假模式一致
    /// `duration_s` 用请求时长做 fps 分母；计数用调度器全量统计；
    /// 分位数 / 分段延迟 / `presented_us` 取自合并时间线——受
    /// `trace_capacity` 截断，**超过容量（默认 4096 帧 ≈ 60fps × 68s）的运行
    /// 分位数与分段是采样值**，计数仍准确。
    ///
    /// # strict 模式
    /// `strict_real == true` 时任一部件降级到假后端即返回
    /// [`LoopbackError::Runtime`]（对应 CLI `--backend real`）。
    pub fn run_real(
        &mut self,
        duration: Duration,
        prefer_hardware: bool,
        strict_real: bool,
    ) -> Result<LoopbackReport> {
        // 真实模式按实测时长与帧率自动扩足 trace 容量（保留足够余量），
        // 避免 60s+ 运行发生环形截断导致早期时间戳丢失、convergence 虚高或不变式失配。
        let trace_cap = self.cfg.trace_capacity.max(
            (duration.as_secs() as usize)
                .saturating_add(4)
                .saturating_mul(self.cfg.fps as usize)
                .saturating_mul(2),
        );
        let mut pcfg = rc_pipeline::PipelineConfig {
            resolution: self.cfg.resolution,
            fps: self.cfg.fps,
            bitrate_bps: 8_000_000,
            prefer_hardware,
            target_latency_us: 22_000,
            trace_capacity: trace_cap,
            diff: rc_pipeline::DiffMode::TrustPlatform,
            clock_origin_us: None,
            // 回环无数据报预算墙：保持默认 1200（M1 行为逐字节不变）。
            max_packet_payload: rc_pipeline::MAX_PAYLOAD,
        };
        pcfg.validate()
            .map_err(|e| LoopbackError::InvalidConfig(e.to_string()))?;

        // ── 后端选择（带标签，strict 校验）──────────────────────
        // 零拷贝链路要求部件同源：SCK 出 CVPixelBuffer 只有 VT 编码器收，
        // VT 编码器的真码流只有 VT 解码器解。**混搭（假采集+真编码）结构上
        // 不成立**——因此降级是整链的：任一真实部件不可用，四个全退假后端。
        let fake_stack = |pcfg: &rc_pipeline::PipelineConfig| -> Result<(
            Box<dyn rc_capture::CaptureBackend>,
            Box<dyn rc_encode::Encoder>,
        )> {
            let content = if self.cfg.static_content {
                rc_capture::FakeContent::Static
            } else {
                rc_capture::FakeContent::Dynamic
            };
            let cap = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
                fps: self.cfg.fps,
                content,
                ..rc_capture::FakeBackendConfig::default()
            })
            .map_err(|e| LoopbackError::Runtime(e.to_string()))?;
            // encoder_slowdown 只在假编码器上有意义：5ms × slowdown，
            // 经 WallEncoder 真实睡眠生效（见其文档）。
            let cost_us = (5_000.0_f32 * self.cfg.encoder_slowdown).round() as u32;
            let enc = rc_encode::FakeEncoder::with_tuning(
                rc_encode::EncoderConfig {
                    resolution: self.cfg.resolution,
                    fps: self.cfg.fps,
                    bitrate_bps: 8_000_000,
                    ..rc_encode::EncoderConfig::default()
                },
                cost_us.max(1),
                1_024,
            )
            .map_err(|e| LoopbackError::Runtime(e.to_string()))?;
            let wall = WallEncoder {
                inner: Box::new(enc),
                cost: Duration::from_micros(u64::from(cost_us.max(1))),
            };
            let _ = pcfg;
            Ok((Box::new(cap), Box::new(wall)))
        };

        let mut fake_cfg = pcfg.clone();
        fake_cfg.prefer_hardware = false;

        let mut session_origin_us: Option<u64> = None;
        let ((capture, encoder), (decoder, renderer), labels) = if prefer_hardware {
            // 先预热编码/解码/呈现（VT E6 自检 + Metal 着色器编译耗时 ~100ms），
            // 再采样主机时钟原点并最后启动 SCK 采集流：
            // 1. 避免预热期间 SCK 已堆积旧帧；
            // 2. 使 SCK PTS 与会话时钟严格共享同一 CMHostTimeClock 原点，
            //    端到端延迟（Presented - Captured）零偏移、绝不下溢归零。
            let (e, el) = rc_pipeline::select_encoder_labeled(&pcfg)
                .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
            let (d, dl) = rc_pipeline::select_decoder_labeled(&pcfg)
                .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
            let (r, rl) = rc_pipeline::select_renderer_labeled(&pcfg)
                .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
            let hw_downstream_ok = el == "videotoolbox" && dl == "videotoolbox" && rl == "metal";
            if hw_downstream_ok {
                session_origin_us = rc_pipeline::host_clock_us();
                pcfg.clock_origin_us = session_origin_us;
                let (c, cl) = rc_pipeline::select_capture_labeled(&pcfg)
                    .map_err(|err| LoopbackError::Runtime(err.to_string()))?;
                if cl == "sck" {
                    (
                        (c, e),
                        (d, r),
                        "capture=sck encoder=videotoolbox decoder=videotoolbox renderer=metal",
                    )
                } else {
                    drop((c, e, d, r));
                    session_origin_us = None;
                    eprintln!(
                        "loopback: 真实采集不可用，整链退回假后端（零拷贝链路要求采集/编码同源，不混搭）"
                    );
                    let (ce, en) = fake_stack(&pcfg)?;
                    let (d, _) = rc_pipeline::select_decoder_labeled(&fake_cfg)
                        .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
                    let (r, _) = rc_pipeline::select_renderer_labeled(&fake_cfg)
                        .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
                    (
                        (ce, en),
                        (d, r),
                        "capture=fake encoder=fake decoder=fake renderer=fake",
                    )
                }
            } else {
                drop((e, d, r));
                eprintln!(
                    "loopback: 真实编解码/呈现不可用（encoder={el} decoder={dl} renderer={rl}），整链退回假后端"
                );
                let (ce, en) = fake_stack(&pcfg)?;
                let (d, _) = rc_pipeline::select_decoder_labeled(&fake_cfg)
                    .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
                let (r, _) = rc_pipeline::select_renderer_labeled(&fake_cfg)
                    .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
                (
                    (ce, en),
                    (d, r),
                    "capture=fake encoder=fake decoder=fake renderer=fake",
                )
            }
        } else {
            let (ce, en) = fake_stack(&pcfg)?;
            let (d, _) = rc_pipeline::select_decoder_labeled(&fake_cfg)
                .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
            let (r, _) = rc_pipeline::select_renderer_labeled(&fake_cfg)
                .map_err(|er| LoopbackError::Runtime(er.to_string()))?;
            (
                (ce, en),
                (d, r),
                "capture=fake encoder=fake decoder=fake renderer=fake",
            )
        };

        let real_ok = labels.contains("capture=sck");
        if strict_real && !real_ok {
            return Err(LoopbackError::Runtime(format!(
                "strict real mode requested but backends are [{labels}]"
            )));
        }
        eprintln!("loopback 真实模式后端：{labels}");

        let mut host = rc_pipeline::HostPipeline::with_backends_paced(
            pcfg.clone(),
            capture,
            encoder,
            !real_ok, // 假采集节流；真实采集 next_frame 自带阻塞
        )
        .map_err(|e| LoopbackError::Runtime(e.to_string()))?;
        let mut controller =
            rc_pipeline::ControllerPipeline::with_backends(pcfg.clone(), decoder, renderer)
                .map_err(|e| LoopbackError::Runtime(e.to_string()))?;

        // ── 驱动循环 ────────────────────────────────────────────
        let start = std::time::Instant::now();
        // 会话时钟：
        // - 真实 macOS 模式：直接读 `CMClockGetHostTimeClock() - session_origin_us`，
        //   与 SCK CMSampleBuffer PTS 同源同零点；
        // - 假后端模式：以首个产出帧的到达时刻（`host.first_capture()`）为零点。
        let origin_slot = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
        let clock: std::sync::Arc<dyn Fn() -> u32 + Send + Sync> =
            if let Some(orig) = session_origin_us {
                std::sync::Arc::new(move || {
                    let now = rc_pipeline::host_clock_us().unwrap_or(orig);
                    u32::try_from(now.saturating_sub(orig)).unwrap_or(u32::MAX)
                })
            } else {
                let clock_slot = std::sync::Arc::clone(&origin_slot);
                std::sync::Arc::new(move || {
                    let guard = clock_slot.lock().expect("origin lock");
                    match *guard {
                        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
                        None => 0,
                    }
                })
            };
        host.set_clock(std::sync::Arc::clone(&clock));
        controller.set_clock(std::sync::Arc::clone(&clock));

        // ── 控制端独立消费线程（模拟真实双机并发：解码/呈现不阻塞被控端采集与编码）──
        // 通道容量 2 严守铁律 1（任意队列深度 ≤ 2）。
        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::sync_channel::<Vec<Vec<u8>>>(2);
        let ctrl_need_idr = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctrl_idr_flag = std::sync::Arc::clone(&ctrl_need_idr);
        let ctrl_clock = std::sync::Arc::clone(&clock);
        let ctrl_handle = std::thread::Builder::new()
            .name("rc-ctrl-loop".into())
            .spawn(
                move || -> std::result::Result<rc_pipeline::ControllerPipeline, String> {
                    while let Ok(datagrams) = ctrl_rx.recv() {
                        for d in &datagrams {
                            controller
                                .push_datagram(d, ctrl_clock())
                                .map_err(|e| format!("controller push: {e}"))?;
                        }
                        if controller.take_keyframe_request() {
                            ctrl_idr_flag.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                    Ok(controller)
                },
            )
            .map_err(|e| LoopbackError::Runtime(format!("spawn controller thread: {e}")))?;

        let mut max_queue_depth: usize = 0;

        while start.elapsed() < duration {
            let now_us = clock();
            if ctrl_need_idr.swap(false, std::sync::atomic::Ordering::AcqRel) {
                host.request_idr();
            }
            let out = host
                .tick(now_us)
                .map_err(|e| LoopbackError::Runtime(format!("host tick: {e}")))?;
            let busy = out.is_some();
            // 会话零点锚在首次采集（与平台时间戳零点一致），
            // 而不是首个数据报产出（那会把管线延迟吃进零点偏移）。
            {
                let mut origin = origin_slot.lock().expect("origin lock");
                if origin.is_none() {
                    *origin = host.first_capture();
                }
            }
            if let Some(datagrams) = out {
                if !datagrams.is_empty() {
                    ctrl_tx
                        .send(datagrams)
                        .map_err(|_| LoopbackError::Runtime("controller channel closed".into()))?;
                }
            }
            max_queue_depth = max_queue_depth.max(host.in_flight_len());

            // 事件驱动挂起：采集帧到达或编码完成时由 Condvar 微秒级唤醒，
            // 既零自旋（静态 CPU ≈ 0%），又消除 sleep(1ms) 在 macOS 定时器合并下的数毫秒抖动。
            if !busy {
                let remain = duration.saturating_sub(start.elapsed());
                if !remain.is_zero() {
                    host.wait_event(remain.min(Duration::from_millis(10)));
                }
            }
        }
        // 收尾（顺序是红线，见 HostPipeline::stop_capture 文档）：
        // ① 停采集 → ② tick 到静默（本 tick 无产出 + 无在途编码：tick
        //    每轮排空交接通道，None 意味着没有帧可处理；编码线程还活着，
        //    末尾帧能正常提交编码）→ ③ 停编码线程 → ④ 最后一轮收取。
        //    反序（先 shutdown 再 tick）会把新提交发给已退出的编码线程，
        //    授权帧凭空消失。500ms 上限兜底，保证每帧有结局。
        host.stop_capture();
        let drain_deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if std::time::Instant::now() > drain_deadline {
                break;
            }
            let out = host
                .tick(clock())
                .map_err(|e| LoopbackError::Runtime(format!("host drain: {e}")))?;
            match out {
                Some(datagrams) => {
                    if !datagrams.is_empty() {
                        let _ = ctrl_tx.send(datagrams);
                    }
                }
                // 静默：交接通道空（tick 已排空且采集已停）+ 无在途编码。
                None if !host.has_pending_encodes() => break,
                None => host.wait_event(Duration::from_millis(5)),
            }
        }
        host.shutdown();
        // 停机后编码线程已排空剩余请求：最后一轮收取完成。
        if let Some(datagrams) = host
            .tick(clock())
            .map_err(|e| LoopbackError::Runtime(format!("host final: {e}")))?
        {
            if !datagrams.is_empty() {
                let _ = ctrl_tx.send(datagrams);
            }
        }
        drop(ctrl_tx);
        let mut controller = ctrl_handle
            .join()
            .map_err(|_| LoopbackError::Runtime("controller thread panicked".into()))?
            .map_err(LoopbackError::Runtime)?;
        controller.finish_in_flight();
        controller.shutdown();

        // ── 报告组装（口径与假模式一致）─────────────────────────
        let duration_us = u32::try_from(duration.as_micros()).unwrap_or(u32::MAX);
        let duration_s = f64::from(duration_us) / 1_000_000.0;
        let merged = rc_pipeline::merge_traces(host.trace(), controller.trace(), trace_cap);
        let summary = merged.summary();
        let sched_stats = host.sched_stats();
        // 修正 encoded 口径：调度器的 encoded 是「授权编码」，真实完成数
        // = 授权数 − 编码失败数（失败帧在 trace 里是 DecodeError 结局，
        // 不变式 B 仍成立；全量计数不经过截断的 trace）。
        let displayed_full = controller.displayed();
        let _ = displayed_full;

        let mut presented_us: Vec<u32> = merged
            .frames()
            .iter()
            .filter(|f| f.outcome() == Some(rc_trace::Outcome::Displayed))
            .filter_map(|f| f.at(rc_trace::Mark::Presented))
            .map(|ts| ts.0)
            .collect();
        presented_us.sort_unstable();

        let breakdown_ms = average_breakdown_trace(&merged);
        let (convergence_ms, degradation_ms) =
            scan_presented(&presented_us, duration_us, self.cfg.fps);

        let mut report = LoopbackReport {
            config: self.cfg.clone(),
            duration_s,
            summary,
            sched_stats,
            latency_p50_ms: us_to_ms(summary.e2e_p50_us),
            latency_p95_ms: us_to_ms(summary.e2e_p95_us),
            latency_p99_ms: us_to_ms(summary.e2e_p99_us),
            displayed_fps: fps_of(summary.displayed, duration_s),
            encoded_fps: fps_of(sched_stats.encoded, duration_s),
            captured_fps: fps_of(sched_stats.captured, duration_s),
            breakdown_ms,
            convergence_ms,
            degradation_ms,
            max_queue_depth_seen: max_queue_depth as u32,
            probe_enabled: self.cfg.probe_enabled,
            static_cpu_pct: None,
            encoder_label: if real_ok { Some("videotoolbox") } else { None },
            gate: GateResult::for_config(&self.cfg),
        };
        report.apply_gate();
        Ok(report)
    }
}

/// 真实时钟世界的假编码器：按 `cost_us` **真实睡眠**后委托内层。
///
/// `rc_encode::FakeEncoder` 的耗时只写在逻辑时间戳里（逻辑时钟管线的约定，
/// 不允许 sleep）；真实时钟驱动下它瞬时完成，`encoder_slowdown` 造不出
/// 背压。本包装只出现在 [`Loopback::run_real`] 的假后端栈里——那条路
/// 的时钟本来就是墙钟，用睡眠模拟硬件耗时是测量基础设施的本职。
struct WallEncoder {
    inner: Box<dyn rc_encode::Encoder>,
    cost: Duration,
}

impl rc_encode::Encoder for WallEncoder {
    fn encode(
        &mut self,
        frame: &rc_capture::CapturedFrame,
        force_idr: bool,
        deadline_ts: rc_types::Timestamp,
    ) -> rc_types::Result<rc_encode::EncodedFrame> {
        // 混合等待：先睡到剩 2ms，再自旋等准。纯 `sleep` 会被 macOS
        // 定时器合并拉长（实测 20ms 实睡 ~27ms，+35%）：那是**模拟器**
        // 的失真，不是被测管线的延迟，会把慢编码门槛的 p50 顶穿。
        // 真实硬件编码器经回调完成，不吃这份 slack。
        let deadline = std::time::Instant::now() + self.cost;
        const SPIN: Duration = Duration::from_millis(2);
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remain = deadline - now;
            if remain > SPIN {
                std::thread::sleep(remain - SPIN);
            }
            // 剩 ≤2ms：自旋到点（std::hint::spin_loop 让出执行单元）。
            std::hint::spin_loop();
        }
        self.inner.encode(frame, force_idr, deadline_ts)
    }
    fn capabilities(&self) -> rc_encode::EncoderCaps {
        self.inner.capabilities()
    }
    fn set_bitrate(&mut self, bps: u32) -> rc_types::Result<()> {
        self.inner.set_bitrate(bps)
    }
    fn request_idr(&mut self) {
        self.inner.request_idr()
    }
    fn flush(&mut self) -> rc_types::Result<Vec<rc_encode::EncodedFrame>> {
        self.inner.flush()
    }
}

/// 对合并时间线里「结局为 `Displayed` 的帧」逐段求平均（真实模式用）。
///
/// 与 [`average_breakdown`](假模式) 同一口径：`total` 段丢弃、段名对齐
/// [`BreakdownMs`] 的 7 个字段；某段缺席记 0。
fn average_breakdown_trace(trace: &rc_trace::TraceCollector) -> BreakdownMs {
    let mut sums = [0u64; 7];
    let mut counts = [0u64; 7];
    for f in trace.frames() {
        if f.outcome() != Some(rc_trace::Outcome::Displayed) {
            continue;
        }
        for (name, us) in f.latency_breakdown() {
            let idx = match name {
                "diff" => Some(0),
                "sched_queue" => Some(1),
                "encode" => Some(2),
                "packetize" => Some(3),
                "network" => Some(4),
                "decode" => Some(5),
                "present" => Some(6),
                _ => None, // total：有意丢弃
            };
            if let Some(i) = idx {
                sums[i] += u64::from(us);
                counts[i] += 1;
            }
        }
    }
    let avg = |i: usize| {
        if counts[i] == 0 {
            0.0
        } else {
            sums[i] as f64 / counts[i] as f64 / 1_000.0
        }
    };
    BreakdownMs {
        diff: avg(0),
        sched_queue: avg(1),
        encode: avg(2),
        packetize: avg(3),
        network: avg(4),
        decode: avg(5),
        present: avg(6),
    }
}

/// 微秒分位数换算毫秒；缺席（没有上屏帧）记 0.0。
fn us_to_ms(us: Option<u32>) -> f64 {
    us.map_or(0.0, |v| f64::from(v) / 1_000.0)
}

/// 计数换算成每秒帧率。时长为 0（空运行）时返回 0.0，不做除零。
fn fps_of(count: u64, duration_s: f64) -> f64 {
    if duration_s > 0.0 {
        count as f64 / duration_s
    } else {
        0.0
    }
}

/// 对「结局为 `Displayed` 的帧」逐段求平均，微秒转毫秒。
///
/// # 数据来源
/// 用 [`PipelineRun::breakdown_sums`] / `breakdown_counts`——管线在每帧上屏时
/// 就地累计，不受 `trace_capacity` 截断影响。若遍历 `run.trace.frames()`，
/// 长运行（> 4096 帧）的分段均值只反映最后若干帧。
///
/// # 段名怎么对齐
/// [`rc_trace::FrameTrace::latency_breakdown`] 实际返回 8 段：
/// `diff`、`sched_queue`、`encode`、`packetize`、`network`、`decode`、
/// `present`、`total`。前 7 段与 [`BreakdownMs`] 的 7 个字段一一对应；
/// **`total` 有意丢弃**——它等于 `Captured → Presented`，与
/// `latency_p50/p95/p99_ms` 是同一个量的逐帧展开，混进来只会重复统计。
///
/// # 假管线里恒为 0 的两段
/// - `diff`：假采集器在采集时刻同时打 `Captured` 与 `Diffed`（脏区判定
///   [`rc_diff::TileDiff`] 在采集器内部完成，不单独计时），
///   因此该段恒为 0。真实后端的采集/比对耗时会在接入后显现。
/// - `packetize`：打包是假实现，`Sent` 与 `EncodeDone` 同刻打点，恒为 0。
///
/// 某段在某一帧上缺席（帧没走完流程）时该帧不计入该段分母；完全没有
/// `Displayed` 帧时返回全 0。
fn average_breakdown(run: &PipelineRun) -> BreakdownMs {
    let avg_ms = |i: usize| {
        let counts = run.breakdown_counts[i];
        if counts == 0 {
            0.0
        } else {
            run.breakdown_sums[i] as f64 / counts as f64 / 1_000.0
        }
    };

    BreakdownMs {
        diff: avg_ms(0),
        sched_queue: avg_ms(1),
        encode: avg_ms(2),
        packetize: avg_ms(3),
        network: avg_ms(4),
        decode: avg_ms(5),
        present: avg_ms(6),
    }
}

/// 滑动窗口长度：1 秒。
const WINDOW_LEN_US: u32 = 1_000_000;
/// 窗口结束时刻的评估步长：10ms（足够细，`convergence_ms` 不会被步长粗化）。
const WINDOW_STEP_US: u32 = 10_000;
/// 一个窗口至少要有这么多上屏帧，其帧率才被采信。
///
/// 没有这个下限时，满 1 秒窗口内只有 1–2 帧也算出「低帧率」，但那更可能是
/// 「这段确实没出帧」的噪声而非稳定状态。
const WINDOW_MIN_FRAMES: usize = 5;
/// 超过这个时长仍未达标，`convergence_ms` 就是 `null`（任务卡 30 秒规则）。
const CONVERGENCE_TIMEOUT_US: u32 = 30_000_000;
/// 扫描显示帧率窗口，返回 `(convergence_ms, degradation_ms)`。
///
/// # 窗口定义（严格按任务卡 T02:141）
/// 窗口长度**恒为 1 秒**，结束时刻从 1 秒开始、每 10ms 评估一次，直到
/// `min(运行时长, 30s)`。窗口是左开右闭区间 `(end - 1s, end]`，
/// `fps = 窗口内帧数 / 1s`。
///
/// # 为什么不能拿短的「从 0 到当前」当窗口
/// 那等于用 5 帧的突发速率冒充 1 秒帧率：60fps 运行会在 80ms 就报「收敛」，
/// 而 200ms RTT 的健康运行（每帧都上屏）会在 180ms 因 5 帧 / 180ms = 27.8fps
/// 被误判为「已退化」。两个指标都会失去意义。窗口不满 1 秒就不参与判定，
/// 因此 `convergence_ms`/`degradation_ms` 的**最小值是 1000ms**。
///
/// # 两个指标
/// - `convergence`：第一个 `fps ≥ target × 0.9` 的满窗口结束时刻（毫秒）。
/// - `degradation`：第一个 `fps < target × 0.5` 的满窗口结束时刻（毫秒）。
///
/// `degradation` 不要求先达到过目标帧率：编码器一直跟不上时，它应在第一个
/// 满窗口就给出时刻，而不是因为「从未达标」就报 `null`，把「掉帧」藏起来。
/// 这与 `40-diagnostics.md` §4.1 的「帧率从目标降到 50%」在稳态运行里等价，
/// 并且在「从未达标」时给出更有诊断价值的信息。
///
/// 超过 `CONVERGENCE_TIMEOUT_US`（30 秒）仍未达标则 `convergence` 为 `None`
/// （报告 JSON 里是 `null`，Gate 判失败）。运行不足 1 秒时两者都是 `None`：
/// 没有满窗口可以判定，诚实地报「测不出来」，而不是编一个数。
fn scan_fps_windows(run: &PipelineRun, target_fps: u32) -> (Option<u64>, Option<u64>) {
    scan_presented(&run.presented_us, run.elapsed_us, target_fps)
}

/// [`scan_fps_windows`] 的纯函数主体：只依赖上屏时刻表与运行时长。
///
/// 拆出来是为了能用**合成时间线**验证 30 秒规则——真实回环里假管线的
/// 帧率是确定的，构造不出「30 秒后才达标」的场景。
fn scan_presented(
    presented: &[u32],
    elapsed_us: u32,
    target_fps: u32,
) -> (Option<u64>, Option<u64>) {
    if presented.is_empty() || elapsed_us < WINDOW_LEN_US {
        return (None, None);
    }

    let converge_at = f64::from(target_fps) * 0.9;
    let degrade_at = f64::from(target_fps) * 0.5;

    // 收敛只在**前 30 秒**内判定（任务卡：30 秒内未达到即 null）。
    // 退化则扫完整段运行：达标后再退化的时刻可能远晚于 30 秒，
    // 用收敛的窗口把它截掉会让退化永远报不出来。
    let converge_limit = elapsed_us.min(CONVERGENCE_TIMEOUT_US);

    let mut convergence = None;
    let mut degradation = None;
    // 第一个满 1 秒窗口的结束时刻就是 1s。
    let mut end = WINDOW_LEN_US;

    loop {
        let start = end - WINDOW_LEN_US;
        // 窗口内帧数 = 呈现时刻 ≤ end 的帧数 − 呈现时刻 ≤ start 的帧数。
        let count =
            presented.partition_point(|&t| t <= end) - presented.partition_point(|&t| t <= start);

        if count >= WINDOW_MIN_FRAMES {
            let fps = count as f64 * 1_000_000.0 / f64::from(WINDOW_LEN_US);
            if convergence.is_none() && fps >= converge_at && end <= converge_limit {
                convergence = Some(u64::from(end) / 1_000);
            }
            if degradation.is_none() && fps < degrade_at {
                degradation = Some(u64::from(end) / 1_000);
            }
        }

        if end >= elapsed_us
            || (convergence.is_some() && (degradation.is_some() || end >= converge_limit))
        {
            break;
        }
        end = (end + WINDOW_STEP_US).min(elapsed_us);
    }

    (convergence, degradation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_types::Resolution;

    /// 小分辨率配置：脏区比对与缓冲分配都便宜，测试跑得快。
    fn small_cfg(fps: u32) -> LoopbackConfig {
        LoopbackConfig {
            resolution: Resolution::new(64, 64),
            fps,
            ..Default::default()
        }
    }

    #[test]
    fn new_rejects_invalid_config() {
        let bad = LoopbackConfig {
            fps: 0,
            ..small_cfg(60)
        };
        match Loopback::new(bad) {
            Err(LoopbackError::InvalidConfig(msg)) => {
                assert!(msg.contains("fps"), "错误信息应指明 fps，实际 {msg}");
            }
            other => panic!("fps = 0 必须被拒绝，实际 {other:?}"),
        }

        // Q3：队列上限是硬约束，3 帧必须被拒绝。
        let deep = LoopbackConfig {
            max_queue_depth: 3,
            ..small_cfg(60)
        };
        assert!(
            matches!(Loopback::new(deep), Err(LoopbackError::InvalidConfig(_))),
            "max_queue_depth = 3 必须被拒绝"
        );

        assert!(Loopback::new(small_cfg(60)).is_ok(), "默认配置必须合法");
    }

    /// NaN 的 `encoder_slowdown` 必须被拒绝。
    ///
    /// `NaN <= 0.0` 恒为 false，若校验写成 `slowdown <= 0.0` 它就会漏过去，
    /// 随后 `(BASE * NaN).round() as u32` 饱和为 0 —— 编码耗时凭空消失，
    /// 报告里 encode 段变 0 而 Gate 照样通过。这类「合法配置静默失效」
    /// 比直接报错危险得多。
    #[test]
    fn new_rejects_non_finite_slowdown() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let cfg = LoopbackConfig {
                encoder_slowdown: bad,
                ..small_cfg(60)
            };
            assert!(
                matches!(Loopback::new(cfg), Err(LoopbackError::InvalidConfig(_))),
                "encoder_slowdown = {bad} 必须被拒绝"
            );
        }
    }

    /// 超大分辨率必须被拒绝，而不是在 `vec![0u8; ...]` 时分配失败 abort。
    #[test]
    fn new_rejects_oversized_resolution() {
        let cfg = LoopbackConfig {
            resolution: Resolution::new(100_000, 100_000),
            ..small_cfg(60)
        };
        match Loopback::new(cfg) {
            Err(LoopbackError::InvalidConfig(msg)) => assert!(
                msg.contains("resolution"),
                "错误信息应指明 resolution，实际 {msg}"
            ),
            other => panic!("超大分辨率必须被拒绝，实际 {other:?}"),
        }
    }

    /// L8 / Q5：静止画面的编码帧率严格为 0，但采集帧率是满的。
    #[test]
    fn static_content_reports_zero_encoded_fps() {
        let cfg = LoopbackConfig {
            static_content: true,
            ..small_cfg(60)
        };
        let report = Loopback::new(cfg)
            .expect("配置合法")
            .run(Duration::from_secs(1))
            .expect("run 不应失败");

        assert_eq!(
            report.encoded_fps, 0.0,
            "L8/Q5 红线：静止画面 encoded_fps 必须严格为 0，实际 {}",
            report.encoded_fps
        );
        assert!(
            report.captured_fps > 55.0,
            "采集帧率应接近 60，实际 {}",
            report.captured_fps
        );
        assert_eq!(
            report.summary.dropped_repeat, report.summary.frames,
            "静止模式下每一帧都应是 DroppedRepeat"
        );
        assert!(
            report.check_invariants().is_empty(),
            "静止模式也必须满足一致性不变式：{:?}",
            report.check_invariants()
        );
    }

    /// L9 / L10：60fps 动态内容应在 1 秒内收敛，队列深度不超过 2。
    #[test]
    fn dynamic_run_converges_and_caps_queue() {
        let report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");

        let convergence = report.convergence_ms.expect("动态内容必须能收敛");
        assert!(
            convergence <= 1_000,
            "L10：convergence_ms 必须 ≤ 1000，实际 {convergence}"
        );
        assert!(
            report.max_queue_depth_seen <= 2,
            "L9/Q3：队列深度 {} 超过上限 2",
            report.max_queue_depth_seen
        );
        assert!(
            report.check_invariants().is_empty(),
            "一致性不变式不成立：{:?}",
            report.check_invariants()
        );
        assert!(
            report.displayed_fps > 55.0,
            "无损伤 60fps 运行应接近满帧率，实际 {}",
            report.displayed_fps
        );
    }

    /// 空运行不是除零：所有 fps 归零，不产生 NaN/Infinity。
    #[test]
    fn zero_duration_is_not_a_division_by_zero() {
        let report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::ZERO)
            .expect("run 不应失败");

        assert_eq!(report.duration_s, 0.0);
        assert_eq!(report.captured_fps, 0.0);
        assert_eq!(report.encoded_fps, 0.0);
        assert_eq!(report.displayed_fps, 0.0);
        assert!(report.convergence_ms.is_none());
        assert!(report.degradation_ms.is_none());
        assert!(report.latency_p50_ms.is_finite());
        assert!(report.to_json().contains("\"convergence_ms\":null"));
    }

    /// L1：报告 JSON 必须带齐 `40-diagnostics.md` §4 的关键字段。
    #[test]
    fn report_json_has_schema_keys() {
        let report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_secs(1))
            .expect("run 不应失败");
        let json = report.to_json();

        for key in [
            "\"config\"",
            "\"latency_ms\"",
            "\"p50\"",
            "\"p95\"",
            "\"p99\"",
            "\"fps\"",
            // §4 里帧率是 `fps.displayed`，不是 `displayed_fps`。
            "\"displayed\"",
            "\"frames\"",
            "\"breakdown_ms\"",
            "\"sched_queue\"",
            "\"network\"",
            "\"convergence_ms\"",
            "\"degradation_ms\"",
            "\"max_queue_depth_seen\"",
            "\"probe_enabled\"",
            "\"gate\"",
        ] {
            assert!(json.contains(key), "报告 JSON 缺少字段 {key}");
        }
        // 顶层键必须与 §4 完全一致：多一个键同样会让验收脚本（按固定键集合
        // 解析）失败。这里钉死「不得出现」的键。
        assert!(
            !json.contains("\"static_cpu_pct\":"),
            "static_cpu_pct 只参与 Gate 判定，不属于 §4 的顶层 schema：{json}"
        );
    }

    /// 静态画面 CPU 有实测值时参与 Gate 判定（不是「列了但不校验」）。
    #[test]
    fn gate_checks_static_cpu_when_measured() {
        // 用满速运行动态内容：其余门槛都达标，失败项只可能来自 CPU。
        let mut report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");
        assert!(
            report.gate.passed,
            "基线必须通过 Gate，实际 failures = {:?}",
            report.gate.failures
        );

        // 假管线没有 CPU 计量：如实为 None，且不判定。
        assert!(
            report.static_cpu_pct.is_none(),
            "回环没有 CPU 观测，必须为 None，实际 {:?}",
            report.static_cpu_pct
        );

        // 注入一个超标实测值：Gate 必须失败并指出 CPU。
        report.static_cpu_pct = Some(report.gate.static_cpu_pct_max + 1.0);
        report.apply_gate();
        assert!(
            !report.gate.passed,
            "CPU 超限时 Gate 必须失败，实际 passed = true"
        );
        assert_eq!(
            report.gate.failures.len(),
            1,
            "基线达标时，CPU 超限应是唯一失败项，实际 {:?}",
            report.gate.failures
        );
        assert!(
            report
                .gate
                .failures
                .iter()
                .any(|f| f.contains("static cpu")),
            "failures 必须包含 CPU 项，实际 {:?}",
            report.gate.failures
        );

        // 达标值不产生失败项。
        report.static_cpu_pct = Some(report.gate.static_cpu_pct_max - 1.0);
        report.apply_gate();
        assert!(
            report.gate.passed,
            "CPU 达标时 Gate 必须通过，实际 failures = {:?}",
            report.gate.failures
        );
    }

    /// Q6：无网络损伤时 `network` 为 0，`sched_queue` 单独成段，两者不合并。
    #[test]
    fn breakdown_separates_queue_from_network() {
        let report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");

        let bd = report.breakdown_ms;
        // 假编码器 5ms、假解码 3ms、假呈现 2ms，全是确定值。
        assert!((bd.encode - 5.0).abs() < 0.01, "encode = {}", bd.encode);
        assert!((bd.decode - 3.0).abs() < 0.01, "decode = {}", bd.decode);
        assert!((bd.present - 2.0).abs() < 0.01, "present = {}", bd.present);
        assert_eq!(bd.network, 0.0, "未注入 RTT 时 network 必须为 0");
        // 假管线里脏区判定在采集时刻同刻完成、打包瞬时完成，两段恒为 0。
        assert_eq!(bd.diff, 0.0, "假采集器不单独计时 diff");
        assert_eq!(bd.packetize, 0.0, "假打包瞬时完成");
        // 无网络时总延迟应等于各段之和；`sched_queue` 若混进了网络时间，
        // 这个等式在注入 RTT 的用例里就会失衡。
        let sum = bd.diff
            + bd.sched_queue
            + bd.encode
            + bd.packetize
            + bd.network
            + bd.decode
            + bd.present;
        assert!(
            (sum - report.latency_p50_ms).abs() < 0.01,
            "分段之和 {sum:.2}ms 应等于 p50 {:.2}ms",
            report.latency_p50_ms
        );

        // 注入 80ms RTT：network 段只承担单程 40ms，排队时间不被吸收。
        let rtt_cfg = LoopbackConfig {
            fps: 30,
            inject_rtt_us: 80_000,
            ..small_cfg(30)
        };
        let rtt = Loopback::new(rtt_cfg)
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");
        assert!(
            (rtt.breakdown_ms.network - 40.0).abs() < 1.0,
            "network 段应为单程 40ms，实际 {:.2}ms",
            rtt.breakdown_ms.network
        );
        assert!(
            rtt.breakdown_ms.sched_queue < 5.0,
            "30fps 下编码器不排队，sched_queue 不应吸收网络延迟，实际 {:.2}ms",
            rtt.breakdown_ms.sched_queue
        );
        assert!(
            !rtt.probe_enabled,
            "探针是独立的 --probe 开关（默认关），不应由「注入了 RTT」派生"
        );
        // Q2：探针关闭时帧率判定照样生效——本用例里 RTT 抬高了延迟但没掉帧，
        // 说明调度不依赖探针回路。
        let local = rtt.breakdown_ms.encode
            + rtt.breakdown_ms.decode
            + rtt.breakdown_ms.present
            + rtt.breakdown_ms.sched_queue
            + rtt.breakdown_ms.diff
            + rtt.breakdown_ms.packetize;
        let without_network = rtt.latency_p50_ms - rtt.breakdown_ms.network;
        assert!(
            (without_network - local).abs() < 0.01,
            "去掉 network 段后剩余延迟 {without_network:.2}ms 应等于本地各段之和 {local:.2}ms"
        );
    }

    /// Q4：编码器跟不上时 `degradation_ms` 必须给出退化时刻（而不是 null）。
    #[test]
    fn degradation_reported_when_encoder_falls_behind() {
        let cfg = LoopbackConfig {
            fps: 60,
            encoder_slowdown: 8.0,
            ..small_cfg(60)
        };
        let report = Loopback::new(cfg)
            .expect("配置合法")
            .run(Duration::from_secs(5))
            .expect("run 不应失败");

        // 编码 40ms > 帧间隔 16.7ms → 显示帧率约 25fps < 30（目标一半）。
        assert!(
            report.displayed_fps < 30.0,
            "8× 慢编码下显示帧率应低于目标一半，实际 {}",
            report.displayed_fps
        );
        let degradation = report.degradation_ms.expect("必然发生退化，不应为 null");
        assert!(
            degradation <= 1_000,
            "退化应在 1 秒内被观测到，实际 {degradation}ms"
        );
        assert!(
            report.convergence_ms.is_none(),
            "从未达标时收敛时间必须是 null"
        );
        assert!(!report.gate.passed, "未达标必须让 Gate 失败");
    }

    /// 固定 seed + 丢包：两次运行的报告 JSON 必须逐字节一致（L3）。
    #[test]
    fn same_seed_same_report() {
        let cfg = LoopbackConfig {
            inject_loss: 0.03,
            inject_jitter_us: 2_000,
            inject_rtt_us: 10_000,
            seed: 20_260_101,
            ..small_cfg(60)
        };
        let run = || {
            Loopback::new(cfg.clone())
                .expect("配置合法")
                .run(Duration::from_secs(2))
                .expect("run 不应失败")
                .to_json()
        };
        assert_eq!(run(), run(), "固定 seed 的报告必须逐字节一致");
    }

    /// 回归：`convergence_ms` / `degradation_ms` 必须用**整 1 秒**窗口。
    ///
    /// 旧实现把 `end - start`（`start = end.saturating_sub(1s)`）当跨度，
    /// 首个评估点 `end = 10ms` 时跨度只有 10ms，于是 5 帧 / 80ms 被算成
    /// 62.5fps——满速运行在 80ms 就「收敛」。同一个缺陷让 200ms RTT 的
    /// 健康运行在 180ms 因 5 帧 / 180ms = 27.8fps 被误判为「已退化」。
    /// 任务卡 T02:141 要求 1 秒滑动窗口，不是瞬时速率。
    #[test]
    fn fps_scan_uses_full_one_second_window() {
        // 满速：第一个可观窗口结束于 1000ms，收敛值必须恰好是 1000。
        let fast = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");
        assert_eq!(
            fast.convergence_ms,
            Some(1_000),
            "满速运行必须先攒满 1 秒窗口才判定收敛，实际 {:?}",
            fast.convergence_ms
        );

        // 健康但高延迟：每帧都上屏（60fps），不得被判为退化。
        let mut high_rtt_cfg = small_cfg(60);
        high_rtt_cfg.inject_rtt_us = 200_000;
        let high_rtt = Loopback::new(high_rtt_cfg)
            .expect("配置合法")
            .run(Duration::from_secs(3))
            .expect("run 不应失败");
        assert!(
            high_rtt.displayed_fps >= 55.0,
            "测试前提不成立：200ms RTT 下仍应满帧上屏，实际 {:.1}fps",
            high_rtt.displayed_fps
        );
        assert!(
            high_rtt.degradation_ms.is_none(),
            "从未退化的健康运行不得报 degradation_ms，实际 {:?}",
            high_rtt.degradation_ms
        );

        // 真的退化：8× 慢编码器 → 帧率约目标 40%，必须在满窗口上被观测到。
        let slow = Loopback::new(LoopbackConfig {
            encoder_slowdown: 8.0,
            ..small_cfg(60)
        })
        .expect("配置合法")
        .run(Duration::from_secs(3))
        .expect("run 不应失败");
        assert_eq!(
            slow.degradation_ms,
            Some(1_000),
            "退化的最早可判定时刻是第一个满窗口（1000ms），实际 {:?}",
            slow.degradation_ms
        );
        assert!(
            slow.convergence_ms.is_none(),
            "从未达标的运行必须报 null，实际 {:?}",
            slow.convergence_ms
        );
    }

    /// 30 秒规则：超过 30 秒才达标，`convergence_ms` 必须是 `null`。
    ///
    /// 用合成时间线验证：假管线的帧率是确定的，构造不出「30 秒后才达标」。
    /// 30 秒内达标必须给出准确时刻；31 秒才达标必须报 null，
    /// 而不是照实给出 31000ms（那样 Gate 会报「超限」而不是「未达标」，
    /// 且违反任务卡 T02:142 的明确要求）。
    #[test]
    fn convergence_times_out_after_thirty_seconds() {
        // 20 秒空白 + 之后满速 60fps 的时间线构造器。
        let timeline = |start_us: u32, end_us: u32| -> Vec<u32> {
            let mut out = Vec::new();
            let mut t = start_us;
            while t <= end_us {
                out.push(t);
                t += 16_667;
            }
            out
        };

        // 情形 A：第 20 秒才开始出帧 → 收敛时刻应约为 21 秒（仍 ≤ 30 秒）。
        let mut a = timeline(20_000_000, 40_000_000);
        a.sort_unstable();
        let (conv_a, _) = scan_presented(&a, 40_000_000, 60);
        let conv_a = conv_a.expect("20 秒起出满帧，应在 30 秒内收敛");
        assert!(
            (20_000..=21_000).contains(&conv_a),
            "第 20 秒开始满帧，convergence_ms 应 ≈ 21000，实际 {conv_a}"
        );

        // 情形 B：第 31 秒才开始出帧 → 超过 30 秒上限，必须 null。
        let mut b = timeline(31_000_000, 45_000_000);
        b.sort_unstable();
        let (conv_b, _) = scan_presented(&b, 45_000_000, 60);
        assert!(
            conv_b.is_none(),
            "31 秒才达标已超过 30 秒上限，convergence_ms 必须是 null，实际 {:?}",
            conv_b
        );
    }

    /// 30 秒上限只约束收敛，**不能**把后期的退化也截掉。
    ///
    /// 若共用同一个 30 秒扫描窗口，一段「前 40 秒满速、之后掉到一半」的运行
    /// 会永远报不出 `degradation_ms` —— 恰恰是最该被发现的场景。
    #[test]
    fn degradation_is_detected_beyond_thirty_seconds() {
        let mut presented = Vec::new();
        // 0–20 秒：60fps 满速。
        let mut t = 0u32;
        while t <= 20_000_000 {
            presented.push(t);
            t += 16_667;
        }
        // 20 秒之后：帧率掉到约 20fps（编码器被拖慢），持续到 45 秒。
        t = 20_000_000;
        while t <= 45_000_000 {
            presented.push(t);
            t += 50_000;
        }
        presented.sort_unstable();

        let (convergence, degradation) = scan_presented(&presented, 45_000_000, 60);
        assert_eq!(
            convergence,
            Some(1_000),
            "前 20 秒满速，收敛应报 1000ms，实际 {:?}",
            convergence
        );
        let degradation = degradation.expect("20 秒后的掉帧必须被观测到");
        assert!(
            (20_000..=22_000).contains(&degradation),
            "掉帧发生在 20 秒后，degradation_ms 应 ≈ 21000，实际 {degradation}"
        );
    }

    /// 回归：运行不足 1 秒时收敛/退化必须是 `null`，不能编造数值。
    #[test]
    fn short_run_cannot_judge_convergence() {
        let report = Loopback::new(small_cfg(60))
            .expect("配置合法")
            .run(Duration::from_millis(500))
            .expect("run 不应失败");
        assert!(report.summary.displayed > 0, "500ms 内应有帧上屏");
        assert!(
            report.convergence_ms.is_none(),
            "不足 1 秒无法用 1 秒窗口判定，convergence_ms 必须为 null，实际 {:?}",
            report.convergence_ms
        );
        assert!(
            report.degradation_ms.is_none(),
            "不足 1 秒无法判定退化，degradation_ms 必须为 null，实际 {:?}",
            report.degradation_ms
        );
    }

    /// 回归：fps 的分母是**请求的测量时长**，不含采集结束后的排空时间。
    ///
    /// 旧实现用 `elapsed_us`（= `max(最后一个事件, 请求时长)`）做分母。
    /// 注入 400ms RTT 时，2 秒跑完 120 帧要再排空约 0.2 秒，120 / 2.19s
    /// 被算成 54.7fps —— 一个 100% 上屏的健康运行被判「掉帧」而 Gate 失败。
    /// 延迟与帧率是两个独立指标，延迟不能惩罚帧率。
    #[test]
    fn fps_uses_measurement_duration_not_drain_time() {
        let mut cfg = small_cfg(60);
        cfg.inject_rtt_us = 400_000;
        let report = Loopback::new(cfg)
            .expect("配置合法")
            .run(Duration::from_secs(2))
            .expect("run 不应失败");

        assert_eq!(
            report.summary.displayed, report.summary.frames,
            "高延迟但不丢帧：期望全部 {} 帧上屏，实际 {}",
            report.summary.frames, report.summary.displayed
        );
        assert!(
            (report.displayed_fps - 60.0).abs() < 0.5,
            "分母必须是 2 秒测量时长（而非含排空的 elapsed），displayed_fps 应为 60，实际 {:.2}",
            report.displayed_fps
        );
        assert!(
            report.duration_s == 2.0,
            "duration_s 必须是请求的 2.0 秒，实际 {}",
            report.duration_s
        );
        // 延迟自然会超限——那是正确的结果，与帧率判定互不影响。
        assert!(
            report
                .gate
                .failures
                .iter()
                .all(|f| !f.contains("displayed fps")),
            "延迟超限不得被表述为帧率不达标，实际 failures = {:?}",
            report.gate.failures
        );
    }

    /// 回归：长运行（超过 `trace_capacity`）的一致性不变式与 JSON 语义必须成立。
    ///
    /// 旧实现拿**被截断**的 `trace.summary()` 去比**全量**的 `sched_stats.encoded`，
    /// 跑满 4096 帧后 `summary.frames` 停在容量上而 encoded 继续增长，
    /// `check_invariants` 恒定报错——文档自带的 120 秒验收命令必然失败。
    /// 同时 `to_json` 会出现 `frames.encoded > frames.captured` 这种自相矛盾。
    #[test]
    fn long_run_survives_trace_capacity() {
        // 30 秒 = 1800 帧（仍在默认 4096 内）；用 1 秒窗口的 trace 逼出截断。
        let cfg = LoopbackConfig {
            trace_capacity: 100,
            ..small_cfg(60)
        };
        let report = Loopback::new(cfg)
            .expect("配置合法")
            .run(Duration::from_secs(10))
            .expect("run 不应失败");

        assert!(
            report.summary.frames > 500,
            "10 秒 60fps 应采集 > 500 帧，实际 {}",
            report.summary.frames
        );
        assert_eq!(
            report.summary.frames,
            report.config.fps as u64 * 10,
            "全量计数必须精确（不受 trace_capacity 影响），实际 {}",
            report.summary.frames
        );
        assert!(
            report.check_invariants().is_empty(),
            "截断不能造成不变式误报，实际 {:?}",
            report.check_invariants()
        );
        assert!(
            report.summary.frames >= report.sched_stats.encoded,
            "编码数不能超过采集数：captured {} < encoded {}",
            report.summary.frames,
            report.sched_stats.encoded
        );
        assert!(
            report.gate.passed,
            "健康长运行的 Gate 必须通过，实际 failures = {:?}",
            report.gate.failures
        );
        // 窗口扫描同样必须基于全量上屏时刻，而不是被截断的尾部。
        assert_eq!(
            report.convergence_ms,
            Some(1_000),
            "长运行也应在第一个满窗口收敛，实际 {:?}",
            report.convergence_ms
        );
    }

    /// 回归：Gate 的帧率门槛随目标帧率而定，不写死 58。
    ///
    /// 写死 58 会让合法的 30fps 配置永远失败（跑满 30.0 仍判 `30.0 < 58.0`），
    /// 而 convergence 用的是 `target × 0.9`——两套口径不一致。
    #[test]
    fn gate_fps_threshold_follows_target() {
        for fps in [30u32, 60, 120] {
            let report = Loopback::new(small_cfg(fps))
                .expect("配置合法")
                .run(Duration::from_secs(2))
                .expect("run 不应失败");
            let expected = f64::from(fps) * 0.9;
            assert!(
                (report.gate.displayed_fps_min - expected).abs() < 1e-9,
                "{fps}fps 的门槛应为 {expected}，实际 {}",
                report.gate.displayed_fps_min
            );
            assert!(
                report.gate.passed,
                "满速 {fps}fps 运行必须通过 Gate（门槛 {expected}，实际 {:.1}fps），failures = {:?}",
                report.displayed_fps, report.gate.failures
            );
        }
    }

    /// Q2：`probe_enabled` 反映 `--probe` 开关，不由「是否注入 RTT」派生。
    #[test]
    fn probe_flag_is_independent_of_rtt() {
        let with_rtt = Loopback::new(LoopbackConfig {
            inject_rtt_us: 80_000,
            ..small_cfg(60)
        })
        .expect("配置合法")
        .run(Duration::from_secs(1))
        .expect("run 不应失败");
        assert!(
            !with_rtt.probe_enabled,
            "默认未开 --probe，即使注入了 RTT 也必须是 false"
        );

        let probe_on = Loopback::new(LoopbackConfig {
            probe_enabled: true,
            ..small_cfg(60)
        })
        .expect("配置合法")
        .run(Duration::from_secs(1))
        .expect("run 不应失败");
        assert!(probe_on.probe_enabled, "配置开启探针时报告必须为 true");
        // Q2 的要求：探针开/关不改变调度判定结果（两边都满帧）。
        assert_eq!(
            probe_on.displayed_fps, with_rtt.displayed_fps,
            "探针开关不应影响帧率判定：关闭时 {:.2}，开启时 {:.2}",
            with_rtt.displayed_fps, probe_on.displayed_fps
        );
    }
}
