//! 报告 schema 与 Gate 判定。
//!
//! # 这是契约层
//! 字段名**不可改**——验收脚本（`jq`）依赖它们。
//! 权威定义见 `docs/reference/impl/40-diagnostics.md` §4。
//! 改动本文件的字段名，必须同步更新该文档与所有验收命令。

use rc_sched::SchedulerStats;
use rc_trace::TraceSummary;
use rc_types::Resolution;

/// 回环配置。
#[derive(Debug, Clone)]
pub struct LoopbackConfig {
    /// 分辨率
    pub resolution: Resolution,
    /// 目标帧率
    pub fps: u32,
    /// 丢包率（0.0–1.0），如 0.03 = 3%
    pub inject_loss: f32,
    /// 往返延迟（微秒），单程 = 该值 / 2
    pub inject_rtt_us: u32,
    /// 抖动（微秒）
    pub inject_jitter_us: u32,
    /// 带宽上限（bps），0 = 不限
    pub bandwidth_bps: u64,
    /// 编码器减速倍数，1.0 = 正常，2.0 = 慢一倍
    pub encoder_slowdown: f32,
    /// 画面是否静止（用于验证 Q5：静止零编码）
    pub static_content: bool,
    /// 丢包序列随机种子（固定后结果可复现）
    pub seed: u64,
    /// 帧时间线容量
    pub trace_capacity: usize,
    /// **接收队列深度硬上限**。默认 2（Q3），不要调大。
    ///
    /// 同时约束管线里**每一个**有界队列（调度器在途队列、网络发送队列）。
    pub max_queue_depth: usize,
    /// 网络延迟探针开关（Q2 的 `--probe`）。
    ///
    /// 默认 `false`：Q2 要求帧率判定不依赖探针，回环默认就该在「无探针」
    /// 状态下工作。它不是「是否注入了 RTT」的派生值——注入延迟与是否启用
    /// 探针是两件独立的事（`40-diagnostics.md` §4.1）。
    pub probe_enabled: bool,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            resolution: Resolution::new(1920, 1080),
            fps: 60,
            inject_loss: 0.0,
            inject_rtt_us: 0,
            inject_jitter_us: 0,
            bandwidth_bps: 0,
            encoder_slowdown: 1.0,
            static_content: false,
            seed: 42,
            trace_capacity: 4096,
            // Q3：RustDesk 用 120 帧（client.rs:114），我们硬性限制 2 帧。
            max_queue_depth: 2,
            // Q2：默认关闭探针，帧率判定必须照样生效。
            probe_enabled: false,
        }
    }
}

impl LoopbackConfig {
    /// 校验配置。与 [`crate::Loopback::new`] 同样会调用。
    pub fn validate(&self) -> Result<(), String> {
        if self.resolution.width == 0 || self.resolution.height == 0 {
            return Err("resolution must be non-zero".into());
        }
        if self.fps == 0 {
            return Err("fps must be > 0".into());
        }
        if !(0.0..=1.0).contains(&self.inject_loss) {
            return Err(format!("inject_loss {} must be in [0,1]", self.inject_loss));
        }
        // NaN 与任何值比较都是 false，`<= 0.0` 放它过去会让编码耗时被静默归零
        // （`(BASE_COST * NaN).round() as u32` 饱和为 0），整条编码段从报告里消失。
        if !self.encoder_slowdown.is_finite() || self.encoder_slowdown <= 0.0 {
            return Err(format!(
                "encoder_slowdown {} must be a finite value > 0",
                self.encoder_slowdown
            ));
        }
        if self.max_queue_depth == 0 {
            return Err("max_queue_depth must be >= 1".into());
        }
        if self.max_queue_depth > 2 {
            // 刻意限制：Q3 要求任意队列深度 ≤2。允许更大就等于复刻 RustDesk 的问题。
            return Err(format!(
                "max_queue_depth {} exceeds hard limit 2 (see Q3 in docs/reference/design/02-architecture.md)",
                self.max_queue_depth
            ));
        }
        // 解析度没有上限时，`vec![0u8; stride*height]` 会因分配失败 abort，
        // 那不是 `Result` 能表达的失败。按 8K（7680×4320）夹一个上界。
        if self.resolution.width > 7_680 || self.resolution.height > 4_320 {
            return Err(format!(
                "resolution {}x{} exceeds the supported maximum 7680x4320",
                self.resolution.width, self.resolution.height
            ));
        }
        Ok(())
    }

    /// 单帧采集间隔（微秒）。
    pub fn frame_interval_us(&self) -> u32 {
        1_000_000 / self.fps.max(1)
    }
}

/// 分段延迟（毫秒）。字段名对应 `FrameTrace::latency_breakdown` 的段落。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BreakdownMs {
    /// 脏区分析
    pub diff: f64,
    /// 调度排队（Q6：必须与 network 分开统计）
    pub sched_queue: f64,
    /// 编码
    pub encode: f64,
    /// 打包分片
    pub packetize: f64,
    /// 网络（Q6：不含排队时间）
    pub network: f64,
    /// 解码
    pub decode: f64,
    /// 呈现
    pub present: f64,
}

/// Gate 判定结果。
#[derive(Debug, Clone, PartialEq)]
pub struct GateResult {
    /// 延迟 P50 上限（毫秒）
    pub p50_ms_max: f64,
    /// Displayed FPS 下限
    pub displayed_fps_min: f64,
    /// 静态画面 CPU 上限（百分比）
    pub static_cpu_pct_max: f64,
    /// 收敛时间上限（毫秒）
    pub convergence_ms_max: u64,
    /// 队列深度上限
    pub max_queue_depth: u32,
    /// 是否全部通过
    pub passed: bool,
    /// 未通过项说明（空 = 全通过）
    pub failures: Vec<String>,
}

impl Default for GateResult {
    fn default() -> Self {
        // 以 60fps 为默认目标；其他目标帧率请用 `for_config`。
        Self::for_config(&LoopbackConfig::default())
    }
}

impl GateResult {
    /// 按目标帧率生成门槛。
    ///
    /// # 为什么帧率门槛不能写死 58
    /// 58 是「60fps 的 96.7%」。写死它会让合法的 30fps 配置永远失败
    /// （30fps 跑满仍判 `30.0 < 58.0`），而按比例算则对任何目标都成立。
    /// 与 `convergence_ms` 的 `target × 0.9` 口径保持一致（同样的相对标准）。
    pub fn for_config(cfg: &LoopbackConfig) -> Self {
        let target = f64::from(cfg.fps);
        Self {
            // 回环不含网络段：22ms 预算 − 5ms 网络 = 17ms，留 1ms 余量
            p50_ms_max: 18.0,
            displayed_fps_min: target * 0.9,
            static_cpu_pct_max: 8.0,
            convergence_ms_max: 1000,
            max_queue_depth: cfg.max_queue_depth.min(2) as u32,
            passed: false,
            failures: Vec::new(),
        }
    }
}

/// 回环测试报告。JSON 字段名与 `40-diagnostics.md` §4 一一对应。
#[derive(Debug, Clone)]
pub struct LoopbackReport {
    /// 本次运行的配置快照
    pub config: LoopbackConfig,
    /// 实际运行时长（秒）
    pub duration_s: f64,
    /// 帧时间线汇总
    pub summary: TraceSummary,
    /// 调度器统计
    pub sched_stats: SchedulerStats,
    /// 端到端延迟分位数（毫秒）
    pub latency_p50_ms: f64,
    /// 端到端延迟 P95（毫秒）
    pub latency_p95_ms: f64,
    /// 端到端延迟 P99（毫秒）
    pub latency_p99_ms: f64,
    /// 实际呈现帧率
    pub displayed_fps: f64,
    /// 实际编码帧率
    pub encoded_fps: f64,
    /// 实际采集帧率
    pub captured_fps: f64,
    /// 分段延迟
    pub breakdown_ms: BreakdownMs,
    /// 收敛时间：从开始到 `displayed_fps ≥ target × 0.9`（毫秒）。None = 未达到
    pub convergence_ms: Option<u64>,
    /// 退化时间：第一个满 1 秒窗口内 `fps < target × 0.5` 的结束时刻（毫秒）。
    ///
    /// 与 `convergence_ms` 用同一套整 1 秒滑动窗口，因此两者可直接比较
    /// （Q4 升/降对称性）。`None` = 全程未低于 50%。
    ///
    /// # 与文档措辞的对应
    /// `40-diagnostics.md` §4.1 写作「帧率从目标降到 50% 所用毫秒数」。本实现
    /// 取的是「第一个低于 50% 的满窗口时刻」：稳态下两者相同；而「从未达标
    /// 就一直在 50% 以下」（编码器持续跟不上）时，本实现给出时刻，文档措辞
    /// 会没有定义。给出时刻更有诊断价值——「从未达标」不应把掉帧藏成 `null`。
    pub degradation_ms: Option<u64>,
    /// 整个运行期间观测到的最大队列深度
    pub max_queue_depth_seen: u32,
    /// 是否启用网络延迟探针（取自配置，默认 `false`）
    pub probe_enabled: bool,
    /// 静态画面 CPU 实测占用（百分比）。
    ///
    /// `None` = 本环境没有 CPU 观测（回环是无 GPU、无系统计量的假管线，
    /// 不编造数字）。真实后端（T03/T04）应当填实测值；
    /// Gate 只在它是 `Some` 时与 `static_cpu_pct_max` 比较。
    ///
    /// **刻意不写进 JSON**：`40-diagnostics.md` §4 的顶层键是固定 schema，
    /// 验收脚本按固定键集合解析，多一个键就会被判为不一致（L1）。
    /// 这个值只参与 Gate 判定，不进契约。
    pub static_cpu_pct: Option<f64>,
    /// 真实模式下的编码器名称覆写（如 `"videotoolbox"`）；`None` 时按假管线配置推导
    /// （`"fake"` / `"software"`）。仅用于写入 JSON `config.encoder` 值，不改变 schema。
    pub encoder_label: Option<&'static str>,
    /// Gate 判定
    pub gate: GateResult,
}

impl LoopbackReport {
    /// 校验一致性不变式，返回违规项。
    ///
    /// 不变式（`40-diagnostics.md` §4.3）：
    /// - `frames == displayed + dropped_* + lost_in_network + decode_errors + unfinished`
    /// - `sched.encoded == displayed + lost_in_network + decode_errors + unfinished`
    ///
    /// 任何一条不成立，说明埋点缺失——统计失效，报告不可信。
    ///
    /// # 统计必须是全量的
    /// 这里读的 `summary` 来自管线的结局计数器，**不是** `trace.summary()`。
    /// 后者受 `trace_capacity` 截断：跑满 4096 帧后会报 `frames(4096) != ...`，
    /// 把「采样截断」误报成「埋点缺失」，让文档自带的 120 秒验收命令恒定失败。
    pub fn check_invariants(&self) -> Vec<String> {
        let mut out = Vec::new();
        let f = &self.summary;

        let dropped = f.dropped_repeat + f.dropped_stale + f.dropped_budget + f.dropped_superseded;
        let lhs = f.frames;
        if lhs != f.displayed + dropped + f.lost_in_network + f.decode_errors + f.unfinished {
            out.push(format!(
                "frames({}) != displayed({}) + dropped({}) + lost({}) + decode_err({}) + unfinished({})",
                lhs,
                f.displayed,
                dropped,
                f.lost_in_network,
                f.decode_errors,
                f.unfinished
            ));
        }

        let encoded_lhs = self.sched_stats.encoded;
        let encoded_rhs = f.displayed + f.lost_in_network + f.decode_errors + f.unfinished;
        if encoded_lhs != encoded_rhs {
            out.push(format!(
                "sched.encoded({}) != displayed({}) + lost({}) + decode_err({}) + unfinished({})",
                encoded_lhs, f.displayed, f.lost_in_network, f.decode_errors, f.unfinished
            ));
        }
        out
    }

    /// 按 Gate 门槛判定，写入 `self.gate`。
    ///
    /// 判定项：
    /// - P50 ≤ 18ms
    /// - Displayed FPS ≥ 目标帧率 × 0.9（见 [`GateResult::for_config`]）
    /// - `convergence_ms` ≤ 1000 且非 None
    /// - `max_queue_depth_seen` ≤ 2
    /// - 静态画面 CPU ≤ 8%（仅在 `static_cpu_pct` 有实测值时才判定）
    /// - 一致性不变式全部成立
    pub fn apply_gate(&mut self) {
        let mut failures = Vec::new();

        if self.latency_p50_ms > self.gate.p50_ms_max {
            failures.push(format!(
                "latency p50 {:.2}ms > {:.1}ms",
                self.latency_p50_ms, self.gate.p50_ms_max
            ));
        }
        if self.displayed_fps < self.gate.displayed_fps_min {
            failures.push(format!(
                "displayed fps {:.1} < {:.1}",
                self.displayed_fps, self.gate.displayed_fps_min
            ));
        }
        match self.convergence_ms {
            Some(ms) if ms <= self.gate.convergence_ms_max => {}
            Some(ms) => failures.push(format!(
                "convergence {}ms > {}ms",
                ms, self.gate.convergence_ms_max
            )),
            None => failures.push("convergence_ms is null (target fps never reached)".into()),
        }
        if self.max_queue_depth_seen > self.gate.max_queue_depth {
            failures.push(format!(
                "max queue depth {} > {}",
                self.max_queue_depth_seen, self.gate.max_queue_depth
            ));
        }
        // 没有 CPU 观测时不判定——但也不会假装通过，这一点在字段文档里写明。
        if let Some(pct) = self.static_cpu_pct {
            if pct > self.gate.static_cpu_pct_max {
                failures.push(format!(
                    "static cpu {pct:.1}% > {:.1}%",
                    self.gate.static_cpu_pct_max
                ));
            }
        }
        for inv in self.check_invariants() {
            failures.push(format!("invariant: {inv}"));
        }

        self.gate.passed = failures.is_empty();
        self.gate.failures = failures;
    }

    /// 输出固定 schema 的 JSON。字段名与 `40-diagnostics.md` §4 一致。
    pub fn to_json(&self) -> String {
        let cfg = &self.config;
        let s = &self.summary;
        let g = &self.gate;
        let opt_u64 = |v: Option<u64>| match v {
            Some(x) => x.to_string(),
            None => "null".to_string(),
        };
        let json_str = |v: &str| format!("\"{}\"", escape(v));
        let failures = if g.failures.is_empty() {
            "[]".to_string()
        } else {
            let items: Vec<String> = g.failures.iter().map(|f| json_str(f)).collect();
            format!("[{}]", items.join(","))
        };
        let encoder = self.encoder_label.unwrap_or(if cfg.encoder_slowdown > 1.0 {
            "software"
        } else {
            "fake"
        });

        format!(
            concat!(
                "{{\"config\":{{\"resolution\":{res},\"fps\":{fps},\"duration_s\":{dur},",
                "\"inject_loss\":{loss},\"inject_rtt_us\":{rtt},\"bandwidth_bps\":{bw},",
                "\"encoder\":{enc}}},",
                "\"latency_ms\":{{\"p50\":{p50},\"p95\":{p95},\"p99\":{p99}}},",
                "\"fps\":{{\"displayed\":{fdisp},\"encoded\":{fenc},\"captured\":{fcap}}},",
                "\"frames\":{{\"captured\":{fcapn},\"encoded\":{fencn},\"displayed\":{fdispn},",
                "\"dropped_repeat\":{dr},\"dropped_stale\":{ds},\"dropped_budget\":{db},",
                "\"dropped_superseded\":{dsup},\"lost_in_network\":{lost},",
                "\"decode_errors\":{derr},\"unfinished\":{unf}}},",
                "\"breakdown_ms\":{{\"diff\":{bd},\"sched_queue\":{bsq},\"encode\":{ben},",
                "\"packetize\":{bpk},\"network\":{bnet},\"decode\":{bdec},\"present\":{bpre}}},",
                "\"convergence_ms\":{conv},\"degradation_ms\":{degr},",
                "\"max_queue_depth_seen\":{mqd},\"probe_enabled\":{probe},",
                "\"gate\":{{\"p50_ms_max\":{gp50},\"displayed_fps_min\":{gfps},",
                "\"static_cpu_pct_max\":{gcpu},\"convergence_ms_max\":{gconv},",
                "\"max_queue_depth\":{gmqd},\"passed\":{passed},\"failures\":{fails}}}}}"
            ),
            res = json_str(&format!(
                "{}x{}",
                cfg.resolution.width, cfg.resolution.height
            )),
            fps = cfg.fps,
            dur = self.duration_s,
            loss = cfg.inject_loss,
            rtt = cfg.inject_rtt_us,
            bw = cfg.bandwidth_bps,
            enc = json_str(encoder),
            p50 = self.latency_p50_ms,
            p95 = self.latency_p95_ms,
            p99 = self.latency_p99_ms,
            fdisp = self.displayed_fps,
            fenc = self.encoded_fps,
            fcap = self.captured_fps,
            fcapn = s.frames,
            fencn = self.sched_stats.encoded,
            fdispn = s.displayed,
            dr = s.dropped_repeat,
            ds = s.dropped_stale,
            db = s.dropped_budget,
            dsup = s.dropped_superseded,
            lost = s.lost_in_network,
            derr = s.decode_errors,
            unf = s.unfinished,
            bd = self.breakdown_ms.diff,
            bsq = self.breakdown_ms.sched_queue,
            ben = self.breakdown_ms.encode,
            bpk = self.breakdown_ms.packetize,
            bnet = self.breakdown_ms.network,
            bdec = self.breakdown_ms.decode,
            bpre = self.breakdown_ms.present,
            conv = opt_u64(self.convergence_ms),
            degr = opt_u64(self.degradation_ms),
            mqd = self.max_queue_depth_seen,
            probe = self.probe_enabled,
            gp50 = g.p50_ms_max,
            gfps = g.displayed_fps_min,
            gcpu = g.static_cpu_pct_max,
            gconv = g.convergence_ms_max,
            gmqd = g.max_queue_depth,
            passed = g.passed,
            fails = failures,
        )
    }
}

/// 最小 JSON 字符串转义（只处理必要字符）。
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
