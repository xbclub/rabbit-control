//! client 会话报告 + 会话 Gate（T18）。
//!
//! # 跨钟域换算口径（S4，坑 1）
//!
//! 双机各持一套会话钟。媒体包头里的 `capture_ts` / `encode_done_ts` 是
//! **host 钟域**，client 管线 trace 的 `Received/Decoded/Presented` 是
//! **client 钟域**——两域时间戳绝不能直接相减。换算必须走 T15 探测得到的
//! 时钟偏移（[`SessionObservations::clock_offset_us`]，口径与
//! `probe::ProbeSample::offset_us` 一致 = host − client）：
//!
//! - e2e = Presented − (captured − offset)；
//! - network = Received − (encode_done − offset)；
//! - encode = encode_done − captured（同为 host 域，无需换算）；
//! - decode / present 取 trace 相邻标记差（同为 client 域）。
//!
//! **注意**：`controller.push_datagram` 会把 host 域的 capture_ts 直接写进
//! trace 的 Captured 标记（controller.rs 的 begin/mark 路径）——报告侧
//! 必须自己持包头值并显式做偏移换算，不得拿 trace 的 Captured 直接减
//! Presented（那是把两钟原点差整体吃进 e2e）。偏移缺失（无探测样本）
//! 时：有上屏帧 → Gate 判失败并写明「无时钟偏移样本」，绝不静默跳过
//! 换算（设计师风险 1）。
//!
//! # 与 rc-loopback 报告的差异（键名对齐 40-diagnostics.md §4，只增不改）
//!
//! - `frames` 只含 client 可观测计数：`dropped_*` 是 host 调度器决策，
//!   client 不可知，不编造；
//! - `breakdown_ms` 只含 4 个可观测段：`diff/sched_queue/packetize` 是
//!   host 侧专有段，不出现也不补 0；
//! - `convergence_ms` / `degradation_ms` 记录但**不进 Gate**（首帧门槛已
//!   覆盖爬坡职责；双机含网络抖动，1s 收敛口径易 flaky）；
//! - 新增 `session` 段承载双机会话才有的量（RTT/偏移/输入单向延迟/
//!   IDR 计数/重连），键名是 T21 控制端 UI 的契约，**只增不改名**；
//! - `input_mode == "none"` 时跳过输入门槛判定（session 段如实记录）。
//!
//! 铁律 4：本模块只吃 [`SessionObservations`] 注入数据，不读任何时钟。

use rc_trace::Outcome;
use rc_types::{FrameSeq, Resolution};
use std::net::SocketAddr;

/// 单帧的跨钟域观测（报告计算的最小输入，纯数据）。
///
/// `captured_us` / `encode_done_us` 来自媒体包头（host 会话钟域）；
/// `received_us` / `decoded_us` / `presented_us` 来自 client 管线 trace
/// （client 会话钟域）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameObservation {
    /// 帧序号。
    pub seq: FrameSeq,
    /// 帧结局（None = 未结）。
    pub outcome: Option<Outcome>,
    /// 包头采集时刻（host 域，微秒）。
    pub captured_us: u32,
    /// 包头编码完成时刻（host 域，微秒）。
    pub encode_done_us: u32,
    /// 完整接收时刻（client 域）。
    pub received_us: Option<u32>,
    /// 解码完成时刻（client 域）。
    pub decoded_us: Option<u32>,
    /// 呈现上屏时刻（client 域）。
    pub presented_us: Option<u32>,
}

/// 重连观测（T17 机制的结果，`--reconnect-at` 演练时填充）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconnectStats {
    /// 触发的意外断线重连次数（含主动 `--reconnect-at` 演练与被动断线；
    /// 不含预期的饱和刷新 `Bye(Refresh)`，见 [`Self::refreshes`]）。
    pub attempts: u64,
    /// 末次重连是否以 resume=true 被接受（Welcome host_flags bit0）。
    pub resumed: bool,
    /// 重连成功到首个 IDR 帧上屏的毫秒数。
    pub recovery_ms: Option<u64>,
    /// 预期的时钟饱和刷新重建次数（`Bye(Refresh)`，不参与 `resumed` Gate 失败判定）。
    pub refreshes: u64,
}

/// 剪贴板同步报告段（T20；嵌入两侧报告 `session.clipboard` 字段，键名只增不改名）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardReport {
    /// 本次会话是否启用了剪贴板同步。
    pub enabled: bool,
    /// 实际发出的通知数。
    pub sent: u64,
    /// 收到并接受（写入本机）的通知数。
    pub received: u64,
    /// 本地变更 == 自己刚写入的值（回声抑制）。
    pub echo_suppressed: u64,
    /// 窗口内被合并进挂起槽的变更数。
    pub coalesced: u64,
    /// 超上限被整条丢弃的变更数（绝不截断）。
    pub oversize_dropped: u64,
    /// 非文本 / 非 UTF-8 变更数。
    pub non_text_dropped: u64,
    /// 被对端后到权威作废的本地挂起值次数。
    pub superseded_local: u64,
    /// 未启用时收到的通知数（计数、不写入、不拆会话）。
    pub received_while_disabled: u64,
}

impl ClipboardReport {
    /// 由启用标志与 [`crate::clipboard::ClipboardStats`] 构造报告段。
    pub fn from_stats(enabled: bool, stats: crate::clipboard::ClipboardStats) -> Self {
        Self {
            enabled,
            sent: stats.sent,
            received: stats.received,
            echo_suppressed: stats.echo_suppressed,
            coalesced: stats.coalesced,
            oversize_dropped: stats.oversize_dropped,
            non_text_dropped: stats.non_text_dropped,
            superseded_local: stats.superseded_local,
            received_while_disabled: stats.received_while_disabled,
        }
    }

    /// 序列化为固定 schema 的 JSON 对象（不含外层 `"clipboard":` 键名）。
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{\"enabled\":{en},\"sent\":{sent},\"received\":{recv},",
                "\"echo_suppressed\":{echo},\"coalesced\":{coal},",
                "\"oversize_dropped\":{over},\"non_text_dropped\":{nont},",
                "\"superseded_local\":{sup},\"received_while_disabled\":{dis}}}"
            ),
            en = self.enabled,
            sent = self.sent,
            recv = self.received,
            echo = self.echo_suppressed,
            coal = self.coalesced,
            over = self.oversize_dropped,
            nont = self.non_text_dropped,
            sup = self.superseded_local,
            dis = self.received_while_disabled,
        )
    }
}

/// client 会话驱动循环结束时交出的全部原始观测（铁律 4：报告组装
/// 只吃注入数据，不读任何时钟）。
#[derive(Debug, Clone, Default)]
pub struct SessionObservations {
    /// 全部帧的跨钟域观测（含未上屏帧，供 lost/decode_errors 计数）。
    pub frames: Vec<FrameObservation>,
    /// 请求的测量时长（微秒，u64 消除 71.6min 饱和）——fps 分母，不含排空（对齐
    /// rc-loopback「fps_uses_measurement_duration_not_drain_time」口径）。
    pub duration_us: u64,
    /// 跨 `ControllerPipeline` 重建的无界累计上屏帧数（H4）。
    ///
    /// `Some(n)` 时 `frames.displayed` 与 `fps.displayed` 采用该无界累计值（消除
    /// trace 环形缓冲 4096 帧上限与管线重建清零失真），同时保留 `frames` 供分位数与
    /// 分段均值计算；`None` 时回退到 `frames` 统计（兼容手工构造的单元观测）。
    pub total_displayed: Option<u64>,
    /// 从 client 进程 Endpoint 创建**之前**到首帧 Presented 的微秒数。
    /// None = 全程无帧上屏。计时起点定在 Endpoint 创建之前并写死在这里
    /// （真机 LAN 上主要耗时在证书加载与 Endpoint 建立，坑 3），避免
    /// 后续真机口径漂移（设计师风险 7）。
    pub connect_to_first_frame_us: Option<u64>,
    /// 探测得到的平滑 RTT（微秒）。None = 尚无探测样本。
    pub rtt_us: Option<u32>,
    /// 时钟偏移（微秒，host 钟 − client 钟，可为负；口径与
    /// `probe::ProbeSample::offset_us` 一致）。None = 尚无探测样本。
    pub clock_offset_us: Option<i64>,
    /// 输入单向延迟样本（微秒；InputAck 换算：host 注入完成时刻经偏移
    /// 折算到 client 域后 − 发送时刻）。
    pub input_one_way_samples_us: Vec<u32>,
    /// 累计发出的 IdrRequest 条数。
    pub idr_requests: u64,
    /// 有效消费的 IdrAck 条数。
    pub idr_acks: u64,
    /// 停滞看门狗触发次数。
    pub watchdog_fires: u64,
    /// 重组表深度峰值（铁律 1：恒 ≤ 2）。
    pub max_reassembly_depth: u32,
    /// 传输层统计快照（末拍缓存）。
    pub net: rc_net::NetStats,
    /// 重连观测。
    pub reconnect: ReconnectStats,
    /// 剪贴板同步统计快照（T20）。
    pub clipboard: ClipboardReport,
    /// 会话号。
    pub session_id: u64,
}

/// 会话 Gate 门槛（对齐 M2 出口指标；client 口径，host 不判 Gate）。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionGateResult {
    /// 首帧（Endpoint 创建前 → 首帧上屏）上限（毫秒）。
    pub connect_to_first_frame_ms_max: f64, // 800.0
    /// e2e p50 上限（毫秒，M2 出口）。
    pub p50_ms_max: f64, // 25.0
    /// e2e p99 上限（毫秒，M2 出口）。
    pub p99_ms_max: f64, // 40.0
    /// 上屏帧率下限（目标帧率 × 0.9，随目标走，不写死 54）。
    pub displayed_fps_min: f64,
    /// 输入单向延迟余量（毫秒）：input p50 ≤ RTT/2 + 5ms。
    pub input_one_way_margin_ms: f64, // 5.0
    /// 重连恢复时长上限（毫秒），仅发生过重连时判定。
    pub reconnect_recovery_ms_max: f64, // 500.0
    /// 重组表深度上限（铁律 1）。
    pub max_queue_depth: u32, // 2
    /// 是否全部通过。
    pub passed: bool,
    /// 未通过项说明（空 = 全通过）。
    pub failures: Vec<String>,
}

/// 报告 config 段的配置快照。
#[derive(Debug, Clone)]
pub struct SessionConfigSnapshot {
    /// 分辨率。
    pub resolution: Resolution,
    /// 目标帧率。
    pub fps: u32,
    /// 请求的测量时长（秒）。
    pub duration_s: f64,
    /// 实际后端标签（"fake" / "sck+videotoolbox+videotoolbox+metal" 等）。
    pub backend: String,
    /// 对端地址。
    pub connect_addr: SocketAddr,
    /// 输入模式（"scripted" / "none" / "real"）。
    pub input_mode: &'static str,
}

/// `breakdown_ms` 的 4 个 client 可观测段（毫秒）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct BreakdownMs {
    encode: f64,
    network: f64,
    decode: f64,
    present: f64,
}

/// `session` 段（T21 的数据源契约，只增不改名）。
#[derive(Debug, Clone, Default)]
struct SessionSection {
    session_id: u64,
    connect_to_first_frame_ms: Option<f64>,
    rtt_us: Option<u32>,
    clock_offset_us: Option<i64>,
    input_one_way_p50_ms: Option<f64>,
    idr_requests: u64,
    idr_acks: u64,
    watchdog_fires: u64,
    loss_rate: f32,
    bytes_sent: u64,
    bytes_recv: u64,
    reconnect: ReconnectStats,
    clipboard: ClipboardReport,
}

/// client 会话报告。JSON 键名对齐 rc-loopback 报告（40-diagnostics.md §4
/// 口径），新增 `session` 段承载双机会话才有的量；私有字段经
/// [`SessionReport::from_observations`] 构造。
#[derive(Debug, Clone)]
pub struct SessionReport {
    cfg: SessionConfigSnapshot,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    latency_p99_ms: f64,
    displayed_fps: f64,
    frames_displayed: u64,
    frames_lost_in_network: u64,
    frames_decode_errors: u64,
    frames_unfinished: u64,
    breakdown_ms: BreakdownMs,
    convergence_ms: Option<u64>,
    degradation_ms: Option<u64>,
    max_queue_depth_seen: u32,
    session: SessionSection,
    gate: SessionGateResult,
}

// 滑动窗口常量：与 rc-loopback `scan_presented` 同值（复制语义见
// `scan_presented_windows` 文档）。
/// 窗口长度：1 秒。
const WINDOW_LEN_US: u32 = 1_000_000;
/// 窗口结束时刻的评估步长：10ms。
const WINDOW_STEP_US: u32 = 10_000;
/// 窗口最小帧数（不足则该窗口帧率不被采信）。
const WINDOW_MIN_FRAMES: usize = 5;
/// 收敛判定上限：30 秒（超过才达标报 null）。
const CONVERGENCE_TIMEOUT_US: u32 = 30_000_000;

impl SessionReport {
    /// 由原始观测组装报告并跑 Gate。所有跨钟域换算在此完成（见模块文档）。
    pub fn from_observations(cfg: &SessionConfigSnapshot, obs: &SessionObservations) -> Self {
        let offset = obs.clock_offset_us;

        // 上屏帧（Displayed 且有 Presented 打点）。
        let displayed: Vec<&FrameObservation> = obs
            .frames
            .iter()
            .filter(|f| f.outcome == Some(Outcome::Displayed) && f.presented_us.is_some())
            .collect();

        // e2e 样本（毫秒，可为负——偏移估计噪声，如实保留）：只在有偏移
        // 样本时计算（无偏移时数字是两钟原点差，垃圾数据不进报告）。
        let mut e2e_ms: Vec<f64> = Vec::new();
        if let Some(off) = offset {
            for f in &displayed {
                let presented = i64::from(f.presented_us.unwrap_or(0));
                let captured_local = i64::from(f.captured_us) - off;
                e2e_ms.push((presented - captured_local) as f64 / 1_000.0);
            }
        }
        let latency_p50_ms = percentile(&mut e2e_ms, 50.0);
        let latency_p95_ms = percentile(&mut e2e_ms, 95.0);
        let latency_p99_ms = percentile(&mut e2e_ms, 99.0);

        // 帧计数（client 可观测结局）。
        let mut frames_displayed_from_trace = 0u64;
        let mut frames_lost_in_network = 0u64;
        let mut frames_decode_errors = 0u64;
        let mut frames_unfinished = 0u64;
        for f in &obs.frames {
            match f.outcome {
                Some(Outcome::Displayed) => frames_displayed_from_trace += 1,
                Some(Outcome::LostInNetwork) => frames_lost_in_network += 1,
                Some(Outcome::DecodeError) => frames_decode_errors += 1,
                None => frames_unfinished += 1,
                // client 管线不会产出 host 侧的 dropped_* 结局，防御性忽略。
                _ => {}
            }
        }
        // H4：优先使用跨管线重建的无界累计上屏帧数；未提供时回退到 trace 统计。
        let frames_displayed = obs.total_displayed.unwrap_or(frames_displayed_from_trace);

        // fps：分母是请求的测量时长（不含排空，u64 消除 71.6min 饱和）。
        let duration_s = obs.duration_us as f64 / 1_000_000.0;
        let displayed_fps = if duration_s > 0.0 {
            let fps_numerator = obs.total_displayed.unwrap_or(displayed.len() as u64);
            fps_numerator as f64 / duration_s
        } else {
            0.0
        };

        // 分段均值（对上屏帧；段缺席的帧不计入该段分母）。
        let mean = |vals: &[f64]| {
            if vals.is_empty() {
                0.0
            } else {
                vals.iter().sum::<f64>() / vals.len() as f64
            }
        };
        let enc: Vec<f64> = displayed
            .iter()
            .map(|f| (f.encode_done_us.saturating_sub(f.captured_us)) as f64 / 1_000.0)
            .collect();
        let net: Vec<f64> = match offset {
            Some(off) => displayed
                .iter()
                .filter_map(|f| {
                    f.received_us.map(|r| {
                        (i64::from(r) - (i64::from(f.encode_done_us) - off)) as f64 / 1_000.0
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        let dec: Vec<f64> = displayed
            .iter()
            .filter_map(|f| Some(f.decoded_us?.saturating_sub(f.received_us?)))
            .map(|us| us as f64 / 1_000.0)
            .collect();
        let pre: Vec<f64> = displayed
            .iter()
            .filter_map(|f| Some(f.presented_us?.saturating_sub(f.decoded_us?)))
            .map(|us| us as f64 / 1_000.0)
            .collect();
        let breakdown_ms = BreakdownMs {
            encode: mean(&enc),
            network: mean(&net),
            decode: mean(&dec),
            present: mean(&pre),
        };

        // 收敛 / 退化（记录但不进 Gate）。
        let mut presented: Vec<u32> = displayed.iter().filter_map(|f| f.presented_us).collect();
        presented.sort_unstable();
        let (convergence_ms, degradation_ms) = scan_presented_windows(
            &presented,
            u32::try_from(obs.duration_us).unwrap_or(u32::MAX),
            cfg.fps,
        );

        // 输入单向延迟 p50。
        let mut input_samples_ms: Vec<f64> = obs
            .input_one_way_samples_us
            .iter()
            .map(|us| f64::from(*us) / 1_000.0)
            .collect();
        let input_one_way_p50_ms = if input_samples_ms.is_empty() {
            None
        } else {
            Some(percentile(&mut input_samples_ms, 50.0))
        };

        let session = SessionSection {
            session_id: obs.session_id,
            connect_to_first_frame_ms: connect_to_first_frame_us_to_ms(obs),
            rtt_us: obs.rtt_us,
            clock_offset_us: obs.clock_offset_us,
            input_one_way_p50_ms,
            idr_requests: obs.idr_requests,
            idr_acks: obs.idr_acks,
            watchdog_fires: obs.watchdog_fires,
            loss_rate: obs.net.loss_rate,
            bytes_sent: obs.net.bytes_sent,
            bytes_recv: obs.net.bytes_recv,
            reconnect: obs.reconnect,
            clipboard: obs.clipboard,
        };

        let gate = SessionGateResult {
            connect_to_first_frame_ms_max: 800.0,
            p50_ms_max: 25.0,
            p99_ms_max: 40.0,
            displayed_fps_min: f64::from(cfg.fps) * 0.9,
            input_one_way_margin_ms: 5.0,
            reconnect_recovery_ms_max: 500.0,
            max_queue_depth: 2,
            passed: false,
            failures: Vec::new(),
        };
        let mut report = Self {
            cfg: cfg.clone(),
            latency_p50_ms,
            latency_p95_ms,
            latency_p99_ms,
            displayed_fps,
            frames_displayed,
            frames_lost_in_network,
            frames_decode_errors,
            frames_unfinished,
            breakdown_ms,
            convergence_ms,
            degradation_ms,
            max_queue_depth_seen: obs.max_reassembly_depth,
            session,
            gate,
        };
        report.apply_gate();
        report
    }

    /// Gate 判定（构造时已跑一次；此方法供测试注入超标值复核）。
    pub fn apply_gate(&mut self) {
        let g = &self.gate;
        let mut failures = Vec::new();

        // 首帧门槛（None = 全程无帧上屏）。
        match self.session.connect_to_first_frame_ms {
            None => failures.push("无上屏帧（connect_to_first_frame_ms 为 null）".to_string()),
            Some(ms) if ms > g.connect_to_first_frame_ms_max => failures.push(format!(
                "connect_to_first_frame {:.1}ms > {:.0}ms",
                ms, g.connect_to_first_frame_ms_max
            )),
            Some(_) => {}
        }
        // 跨钟域换算的前提：有探测偏移样本（有上屏帧时才是失败——
        // 无上屏帧已由上一项覆盖）。
        let has_offset = self.session.clock_offset_us.is_some();
        if !has_offset && self.frames_displayed > 0 {
            failures.push("无时钟偏移样本：跨钟域换算未发生，e2e/network 未统计".to_string());
        }
        // e2e 分位数（有上屏帧且有偏移才判定；零帧场景由首帧/帧率项覆盖）。
        if self.frames_displayed > 0 && has_offset {
            if self.latency_p50_ms > g.p50_ms_max {
                failures.push(format!(
                    "latency p50 {:.2}ms > {:.1}ms",
                    self.latency_p50_ms, g.p50_ms_max
                ));
            }
            if self.latency_p99_ms > g.p99_ms_max {
                failures.push(format!(
                    "latency p99 {:.2}ms > {:.1}ms",
                    self.latency_p99_ms, g.p99_ms_max
                ));
            }
        }
        // 帧率门槛（随目标帧率）。
        if self.displayed_fps < g.displayed_fps_min {
            failures.push(format!(
                "displayed fps {:.1} < {:.1}",
                self.displayed_fps, g.displayed_fps_min
            ));
        }
        // 输入单向延迟：p50 ≤ RTT/2 + margin（无 RTT 样本按 0 折算）；
        // input_mode == "none" 时跳过判定（session 段如实记录）。
        if self.cfg.input_mode != "none" {
            match self.session.input_one_way_p50_ms {
                Some(p50) => {
                    let rtt_half_ms = f64::from(self.session.rtt_us.unwrap_or(0)) / 2_000.0;
                    let limit = rtt_half_ms + g.input_one_way_margin_ms;
                    if p50 > limit {
                        failures.push(format!(
                            "input one-way p50 {:.2}ms > rtt/2+{:.0}ms ({:.2}ms)",
                            p50, g.input_one_way_margin_ms, limit
                        ));
                    }
                }
                None => {
                    failures.push("无输入单向延迟样本（无探测偏移或无关键批输入流量）".to_string())
                }
            }
        }
        // 重连门槛：仅发生过重连时判定。
        if self.session.reconnect.attempts > 0 {
            if !self.session.reconnect.resumed {
                failures.push("重连 resume 被拒（resumed=false）".to_string());
            }
            match self.session.reconnect.recovery_ms {
                Some(ms) if ms as f64 > g.reconnect_recovery_ms_max => failures.push(format!(
                    "reconnect recovery {ms}ms > {:.0}ms",
                    g.reconnect_recovery_ms_max
                )),
                Some(_) => {}
                None if self.session.reconnect.resumed => {
                    failures.push("重连恢复时长未测得（recovery_ms 为 null）".to_string())
                }
                None => {}
            }
        }
        // 队列深度（铁律 1）。
        if self.max_queue_depth_seen > g.max_queue_depth {
            failures.push(format!(
                "max queue depth {} > {}",
                self.max_queue_depth_seen, g.max_queue_depth
            ));
        }

        self.gate.passed = failures.is_empty();
        self.gate.failures = failures;
    }

    /// Gate 结果（测试与 CLI 摘要用）。
    pub fn gate(&self) -> &SessionGateResult {
        &self.gate
    }

    /// e2e p50（毫秒；CLI stderr 摘要用）。
    pub fn latency_p50_ms(&self) -> f64 {
        self.latency_p50_ms
    }

    /// e2e p99（毫秒）。
    pub fn latency_p99_ms(&self) -> f64 {
        self.latency_p99_ms
    }

    /// 上屏帧率（fps；分母 = 请求的测量时长）。
    pub fn displayed_fps(&self) -> f64 {
        self.displayed_fps
    }

    /// 首帧毫秒（None = 全程无帧上屏）。
    pub fn connect_to_first_frame_ms(&self) -> Option<f64> {
        self.session.connect_to_first_frame_ms
    }

    /// 输入单向延迟 p50（毫秒；None = 无样本）。
    pub fn input_one_way_p50_ms(&self) -> Option<f64> {
        self.session.input_one_way_p50_ms
    }

    /// 剪贴板同步报告段（T20）。
    pub fn clipboard(&self) -> ClipboardReport {
        self.session.clipboard
    }

    /// 输出固定 schema 的 JSON（手写，不引 serde；escape 对齐
    /// rc-loopback/src/report.rs 的实现）。
    pub fn to_json(&self) -> String {
        let cfg = &self.cfg;
        let s = &self.session;
        let g = &self.gate;
        let opt_f64 = |v: Option<f64>| match v {
            Some(x) => format_f64(x),
            None => "null".to_string(),
        };
        let opt_u64 = |v: Option<u64>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
        let opt_u32 = |v: Option<u32>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
        let opt_i64 = |v: Option<i64>| v.map(|x| x.to_string()).unwrap_or_else(|| "null".into());
        let json_str = |v: &str| format!("\"{}\"", escape(v));
        let failures = if g.failures.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                g.failures
                    .iter()
                    .map(|f| json_str(f))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };

        format!(
            concat!(
                "{{\"config\":{{\"resolution\":{res},\"fps\":{fps},\"duration_s\":{dur},",
                "\"backend\":{be},\"connect\":{conn},\"input_mode\":{im}}},",
                "\"latency_ms\":{{\"p50\":{p50},\"p95\":{p95},\"p99\":{p99}}},",
                "\"fps\":{{\"displayed\":{fdisp}}},",
                "\"frames\":{{\"displayed\":{fdispn},\"lost_in_network\":{lost},",
                "\"decode_errors\":{derr},\"unfinished\":{unf}}},",
                "\"breakdown_ms\":{{\"encode\":{ben},\"network\":{bnet},",
                "\"decode\":{bdec},\"present\":{bpre}}},",
                "\"convergence_ms\":{conv},\"degradation_ms\":{degr},",
                "\"max_queue_depth_seen\":{mqd},",
                "\"session\":{{\"session_id\":{sid},\"connect_to_first_frame_ms\":{c2ff},",
                "\"rtt_us\":{rtt},\"clock_offset_us\":{coff},",
                "\"input_one_way_p50_ms\":{iow},",
                "\"idr_requests\":{idrr},\"idr_acks\":{idra},\"watchdog_fires\":{wd},",
                "\"loss_rate\":{loss},\"bytes_sent\":{bsent},\"bytes_recv\":{brecv},",
                "\"reconnect\":{{\"attempts\":{rat},\"resumed\":{rres},\"recovery_ms\":{rrec},\"refreshes\":{rref}}},",
                "\"clipboard\":{clip}}},",
                "\"gate\":{{\"connect_to_first_frame_ms_max\":{gc2f},\"p50_ms_max\":{gp50},",
                "\"p99_ms_max\":{gp99},\"displayed_fps_min\":{gfps},",
                "\"input_one_way_margin_ms\":{gmar},\"reconnect_recovery_ms_max\":{grec},",
                "\"max_queue_depth\":{gmqd},\"passed\":{passed},\"failures\":{fails}}}}}"
            ),
            res = json_str(&format!(
                "{}x{}",
                cfg.resolution.width, cfg.resolution.height
            )),
            fps = cfg.fps,
            dur = format_f64(cfg.duration_s),
            be = json_str(&cfg.backend),
            conn = json_str(&cfg.connect_addr.to_string()),
            im = json_str(cfg.input_mode),
            p50 = format_f64(self.latency_p50_ms),
            p95 = format_f64(self.latency_p95_ms),
            p99 = format_f64(self.latency_p99_ms),
            fdisp = format_f64(self.displayed_fps),
            fdispn = self.frames_displayed,
            lost = self.frames_lost_in_network,
            derr = self.frames_decode_errors,
            unf = self.frames_unfinished,
            ben = format_f64(self.breakdown_ms.encode),
            bnet = format_f64(self.breakdown_ms.network),
            bdec = format_f64(self.breakdown_ms.decode),
            bpre = format_f64(self.breakdown_ms.present),
            conv = opt_u64(self.convergence_ms),
            degr = opt_u64(self.degradation_ms),
            mqd = self.max_queue_depth_seen,
            sid = s.session_id,
            c2ff = opt_f64(s.connect_to_first_frame_ms),
            rtt = opt_u32(s.rtt_us),
            coff = opt_i64(s.clock_offset_us),
            iow = opt_f64(s.input_one_way_p50_ms),
            idrr = s.idr_requests,
            idra = s.idr_acks,
            wd = s.watchdog_fires,
            loss = format_f32(s.loss_rate),
            bsent = s.bytes_sent,
            brecv = s.bytes_recv,
            rat = s.reconnect.attempts,
            rres = s.reconnect.resumed,
            rrec = opt_u64(s.reconnect.recovery_ms),
            rref = s.reconnect.refreshes,
            clip = s.clipboard.to_json(),
            gc2f = format_f64(g.connect_to_first_frame_ms_max),
            gp50 = format_f64(g.p50_ms_max),
            gp99 = format_f64(g.p99_ms_max),
            gfps = format_f64(g.displayed_fps_min),
            gmar = format_f64(g.input_one_way_margin_ms),
            grec = format_f64(g.reconnect_recovery_ms_max),
            gmqd = g.max_queue_depth,
            passed = g.passed,
            fails = failures,
        )
    }
}

/// `connect_to_first_frame_us` → 毫秒（None 透传）。
fn connect_to_first_frame_us_to_ms(obs: &SessionObservations) -> Option<f64> {
    obs.connect_to_first_frame_us.map(|us| us as f64 / 1_000.0)
}

/// 最近邻秩分位数（复制 rc-trace `percentile` 的语义：rank = ceil(p%·n)，
/// 取第 rank 小的样本）。空输入返回 0.0（零帧报告不产 NaN）。
fn percentile(sorted_vals: &mut [f64], p: f64) -> f64 {
    if sorted_vals.is_empty() {
        return 0.0;
    }
    sorted_vals.sort_by(|a, b| {
        a.partial_cmp(b)
            .expect("样本不含 NaN（来源都是有限整数换算）")
    });
    let rank = (p / 100.0 * sorted_vals.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_vals.len() - 1);
    sorted_vals[idx]
}

/// f64 的 JSON 输出：有限值原样（Rust 的 `{}` 对 0.0 打 `0`，与 rc-loopback
/// 报告一致）；NaN/Infinity 防御性打 0（零帧报告不产非法 JSON）。
fn format_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

/// f32 同 [`format_f64`。
fn format_f32(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

/// 整 1 秒滑动窗口扫描收敛 / 退化时刻。**同语义重实现**：复制
/// `rc-loopback/src/lib.rs` 的 `scan_presented`（窗口最小
/// [`WINDOW_MIN_FRAMES`] 帧、收敛 [`CONVERGENCE_TIMEOUT_US`] 上限、退化扫
/// 全程）——依赖方向不允许 rc-session → rc-loopback，故在此复刻并在注释
/// 注明来源；两处语义必须同步演进。
fn scan_presented_windows(
    presented: &[u32],
    elapsed_us: u32,
    target_fps: u32,
) -> (Option<u64>, Option<u64>) {
    if presented.is_empty() || elapsed_us < WINDOW_LEN_US {
        return (None, None);
    }
    let converge_at = f64::from(target_fps) * 0.9;
    let degrade_at = f64::from(target_fps) * 0.5;
    let converge_limit = elapsed_us.min(CONVERGENCE_TIMEOUT_US);
    let mut convergence = None;
    let mut degradation = None;
    let mut end = WINDOW_LEN_US;
    loop {
        let start = end - WINDOW_LEN_US;
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
        end = end.saturating_add(WINDOW_STEP_US).min(elapsed_us);
    }
    (convergence, degradation)
}

/// 最小 JSON 字符串转义（对齐 rc-loopback/src/report.rs 的 `escape`）。
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

// ─────────────────────────────────────────────────────────────────
// 测试：全部离线确定性（合成观测，无网络、无时钟）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_snapshot() -> SessionConfigSnapshot {
        SessionConfigSnapshot {
            resolution: Resolution::new(64, 64),
            fps: 60,
            duration_s: 3.0,
            backend: "fake".into(),
            connect_addr: "127.0.0.1:9901".parse().unwrap(),
            input_mode: "scripted",
        }
    }

    /// 造一台上屏帧：host 域 captured/encode_done、client 域
    /// received/decoded/presented（caller 给出跨域偏移量后的真实关系）。
    fn displayed_frame(
        seq: u64,
        captured: u32,
        encode_done: u32,
        received: u32,
        decoded: u32,
        presented: u32,
    ) -> FrameObservation {
        FrameObservation {
            seq: FrameSeq(seq),
            outcome: Some(Outcome::Displayed),
            captured_us: captured,
            encode_done_us: encode_done,
            received_us: Some(received),
            decoded_us: Some(decoded),
            presented_us: Some(presented),
        }
    }

    /// 一份健康观测：60 帧、恒定分段、RTT 480µs、偏移 −35µs。
    ///
    /// 时间线（client 域 t = i×16_667；host 域 = client 域 + 35）：
    /// captured(host) = t+35 → client 域 t+70；encode_done = captured+5ms；
    /// received = encode_done(client 域)+0.5ms；decoded = +3ms；
    /// presented = +2ms → e2e = 10_570 − 70 = 10.5ms。
    fn healthy_obs() -> SessionObservations {
        let mut frames = Vec::new();
        for i in 0..60u32 {
            let t = i * 16_667; // client 域时间轴
            let captured_host = t + 35; // host 域 = client 域 + 35（offset = −35）
            frames.push(displayed_frame(
                u64::from(i),
                captured_host,
                captured_host + 5_000, // encode 5ms（host 域差，无需换算）
                t + 5_570,             // encode_done(client 域 t+5_070) + network 0.5ms
                t + 8_570,             // + decode 3ms
                t + 10_570,            // + present 2ms；e2e = 10_570 − 70 = 10.5ms
            ));
        }
        SessionObservations {
            frames,
            duration_us: 1_000_000,
            total_displayed: None,
            connect_to_first_frame_us: Some(62_500),
            rtt_us: Some(480),
            clock_offset_us: Some(-35),
            input_one_way_samples_us: vec![310; 10],
            idr_requests: 1,
            idr_acks: 1,
            watchdog_fires: 0,
            max_reassembly_depth: 2,
            net: rc_net::NetStats::default(),
            reconnect: ReconnectStats::default(),
            clipboard: ClipboardReport::default(),
            session_id: 1234567890,
        }
    }

    /// S4：合成 trace（captured=host 域、presented=client 域、已知 offset）
    /// → e2e/network 精确等于手算值；offset 置 0 时数字必须不同（证明
    /// 换算真的发生了）。
    #[test]
    fn offset_conversion_is_applied_to_e2e_and_network() {
        let mut obs = healthy_obs();
        // 手算（offset = −35，即 host = client + 35）：
        // 帧 0：captured(host) = 35 → client 域 = 35 − (−35) = 70；
        // presented = 10_570 → e2e = 10_570 − 70 = 10_500µs = 10.5ms。
        let report = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            (report.latency_p50_ms - 10.5).abs() < 0.01,
            "e2e 应等于手算 10.5ms，实际 {}",
            report.latency_p50_ms
        );
        assert!(
            (report.breakdown_ms.network - 0.5).abs() < 0.001,
            "network = received − (encode_done − offset) = 0.5ms，实际 {}",
            report.breakdown_ms.network
        );

        // 反向证明：offset 置 0，数字必须改变（不再做换算补偿）。
        obs.clock_offset_us = Some(0);
        let report0 = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            (report0.latency_p50_ms - 10.5).abs() >= 0.005,
            "offset 置 0 后 e2e 必须不同于换算值（{} vs 10.5）",
            report0.latency_p50_ms
        );
        // offset = 0：captured 直接按 client 域用，e2e = 10_570 − 35 = 10.535。
        assert!(
            (report0.latency_p50_ms - 10.535).abs() < 0.001,
            "offset=0 时 e2e = 10.535ms，实际 {}",
            report0.latency_p50_ms
        );
    }

    /// S4：有上屏帧但无偏移样本 → Gate 失败且 failures 含「时钟偏移」；
    /// 不静默给 0。
    #[test]
    fn missing_offset_fails_gate_loudly() {
        let mut obs = healthy_obs();
        obs.clock_offset_us = None;
        obs.rtt_us = None;
        let report = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(!report.gate.passed, "无偏移样本必须判失败");
        assert!(
            report.gate.failures.iter().any(|f| f.contains("时钟偏移")),
            "失败项必须写明「时钟偏移」：{:?}",
            report.gate.failures
        );
    }

    /// S9：顶层键齐全；frames 不含 dropped_*；breakdown_ms 只含 4 段。
    #[test]
    fn session_report_json_has_schema_keys() {
        let json = SessionReport::from_observations(&cfg_snapshot(), &healthy_obs()).to_json();
        for key in [
            "\"config\"",
            "\"latency_ms\"",
            "\"fps\"",
            "\"frames\"",
            "\"breakdown_ms\"",
            "\"convergence_ms\"",
            "\"degradation_ms\"",
            "\"max_queue_depth_seen\"",
            "\"session\"",
            "\"gate\"",
        ] {
            assert!(json.contains(key), "报告缺少顶层键 {key}：{json}");
        }
        // frames 只含 client 可观测计数。
        let frames_sec = json
            .split("\"frames\":")
            .nth(1)
            .unwrap()
            .split("},")
            .next()
            .unwrap();
        for banned in [
            "dropped_repeat",
            "dropped_stale",
            "dropped_budget",
            "dropped_superseded",
        ] {
            assert!(
                !json.contains(banned),
                "frames 不得编造 host 侧 dropped_* 计数（{banned}）：{json}"
            );
        }
        let _ = frames_sec;
        // breakdown_ms 只含 4 段。
        let bd = json
            .split("\"breakdown_ms\":")
            .nth(1)
            .unwrap()
            .split("}},")
            .next()
            .unwrap();
        for banned in ["diff", "sched_queue", "packetize"] {
            assert!(
                !bd.contains(banned),
                "breakdown_ms 不得含 host 专有段 {banned}"
            );
        }
        for needed in ["encode", "network", "decode", "present"] {
            assert!(bd.contains(needed), "breakdown_ms 缺 {needed} 段");
        }
        // session 段键齐全（T21 契约）。
        for key in [
            "\"session_id\"",
            "\"connect_to_first_frame_ms\"",
            "\"rtt_us\"",
            "\"clock_offset_us\"",
            "\"input_one_way_p50_ms\"",
            "\"idr_requests\"",
            "\"idr_acks\"",
            "\"watchdog_fires\"",
            "\"loss_rate\"",
            "\"bytes_sent\"",
            "\"bytes_recv\"",
            "\"reconnect\"",
            "\"clipboard\"",
            "\"enabled\"",
            "\"sent\"",
            "\"received\"",
            "\"echo_suppressed\"",
            "\"coalesced\"",
            "\"oversize_dropped\"",
            "\"non_text_dropped\"",
            "\"superseded_local\"",
            "\"received_while_disabled\"",
        ] {
            assert!(json.contains(key), "session 段缺少 {key}：{json}");
        }
    }

    /// 逐项注入超标值：首帧 >800ms / p50 >25 / p99 >40 / fps <0.9× /
    /// 队列深度 >2，各产生对应失败项。
    #[test]
    fn session_gate_fails_each_threshold() {
        // 基线全过。
        let base = SessionReport::from_observations(&cfg_snapshot(), &healthy_obs());
        assert!(base.gate.passed, "基线必须通过：{:?}", base.gate.failures);

        // 首帧超时。
        let mut obs = healthy_obs();
        obs.connect_to_first_frame_us = Some(900_000);
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(!r.gate.passed);
        assert!(r
            .gate
            .failures
            .iter()
            .any(|f| f.contains("connect_to_first_frame")));

        // p50 > 25ms（把所有帧的 presented 推迟 20ms → e2e ≈ 30.4ms）。
        let mut obs = healthy_obs();
        for f in obs.frames.iter_mut() {
            f.presented_us = f.presented_us.map(|p| p + 20_000);
        }
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            r.gate.failures.iter().any(|f| f.contains("p50")),
            "{:?}",
            r.gate.failures
        );

        // p99 > 40ms：只把最后 4 帧（>5%）推迟 35ms。
        let mut obs = healthy_obs();
        let n = obs.frames.len();
        for f in obs.frames.iter_mut().skip(n - 4) {
            f.presented_us = f.presented_us.map(|p| p + 35_000);
        }
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            r.gate.failures.iter().any(|f| f.contains("p99")),
            "{:?}",
            r.gate.failures
        );

        // fps < 0.9×：60 帧摊在 1.2s 上（50fps < 54）。注意保持每帧 e2e 不变
        // （时间轴整体放大即可：帧间隔 16_667 → 20_000，时长 1s → 1.2s）。
        let mut obs = healthy_obs();
        for (i, f) in obs.frames.iter_mut().enumerate() {
            let t = i as u32 * 20_000;
            f.presented_us = Some(t + 10_500);
            f.captured_us = t + 35;
            f.encode_done_us = t + 35 + 5_000;
            f.received_us = Some(t + 500);
            f.decoded_us = Some(t + 3_500);
        }
        obs.duration_us = 1_200_000;
        let mut cfg = cfg_snapshot();
        cfg.duration_s = 1.2;
        let r = SessionReport::from_observations(&cfg, &obs);
        assert!(
            r.gate.failures.iter().any(|f| f.contains("displayed fps")),
            "{:?}",
            r.gate.failures
        );

        // 队列深度 > 2。
        let mut obs = healthy_obs();
        obs.max_reassembly_depth = 3;
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(r.gate.failures.iter().any(|f| f.contains("queue depth")));
    }

    /// 输入门槛：样本 p50 = rtt/2+4.9ms 通过、+5.1ms 失败；
    /// input_mode="none" 跳过判定。
    #[test]
    fn input_one_way_gate_uses_rtt_half_plus_margin() {
        // rtt = 1ms → 门槛 0.5 + 5 = 5.5ms。
        let mut obs = healthy_obs();
        obs.rtt_us = Some(1_000);
        obs.input_one_way_samples_us = vec![5_400; 10]; // 5.4ms：通过
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(r.gate.passed, "5.4ms ≤ 5.5ms 应通过：{:?}", r.gate.failures);

        obs.input_one_way_samples_us = vec![5_600; 10]; // 5.6ms：失败
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            r.gate.failures.iter().any(|f| f.contains("input one-way")),
            "5.6ms > 5.5ms 应失败：{:?}",
            r.gate.failures
        );

        // input_mode = "none"：样本照记录、门槛跳过（空样本也不失败）。
        let mut cfg = cfg_snapshot();
        cfg.input_mode = "none";
        let mut obs = healthy_obs();
        obs.input_one_way_samples_us = Vec::new();
        let r = SessionReport::from_observations(&cfg, &obs);
        assert!(
            r.gate.passed,
            "input_mode=none 必须跳过输入门槛：{:?}",
            r.gate.failures
        );
        assert!(
            r.to_json().contains("\"input_one_way_p50_ms\":null"),
            "session 段如实记录空样本"
        );
    }

    /// attempts=0 不判 recovery；attempts>0 且 resumed=false 或 recovery>500
    /// 失败。
    #[test]
    fn reconnect_gate_only_when_attempted() {
        // attempts=0 + recovery=None：不判定（默认观测即此态，基线通过）。
        let r = SessionReport::from_observations(&cfg_snapshot(), &healthy_obs());
        assert!(r.gate.passed);

        // attempts>0 + resumed=true + recovery=300ms：通过。
        let mut obs = healthy_obs();
        obs.reconnect = ReconnectStats {
            attempts: 1,
            resumed: true,
            recovery_ms: Some(300),
            refreshes: 0,
        };
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(r.gate.passed, "{:?}", r.gate.failures);

        // attempts>0 + resumed=false：失败。
        obs.reconnect.resumed = false;
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(r.gate.failures.iter().any(|f| f.contains("resume 被拒")));

        // recovery > 500ms：失败。
        obs.reconnect = ReconnectStats {
            attempts: 1,
            resumed: true,
            recovery_ms: Some(600),
            refreshes: 0,
        };
        let r = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(r.gate.failures.iter().any(|f| f.contains("recovery")));
    }

    /// 收敛扫描的整 1 秒窗口语义（复制 rc-loopback
    /// `convergence_times_out_after_thirty_seconds` 同名语义）：满速 1000ms
    /// 收敛、31s 才达标报 null。
    #[test]
    fn convergence_scan_uses_full_one_second_window() {
        // 20 秒空白 + 之后满速 60fps：收敛时刻 ≈ 21s（≤30s）。
        let timeline = |start_us: u32, end_us: u32| -> Vec<u32> {
            let mut out = Vec::new();
            let mut t = start_us;
            while t <= end_us {
                out.push(t);
                t += 16_667;
            }
            out.sort_unstable();
            out
        };
        let a = timeline(20_000_000, 40_000_000);
        let (conv_a, _) = scan_presented_windows(&a, 40_000_000, 60);
        let conv_a = conv_a.expect("20 秒起满帧，应在 30 秒内收敛");
        assert!(
            (20_000..=21_000).contains(&conv_a),
            "convergence_ms 应 ≈ 21000，实际 {conv_a}"
        );

        // 31 秒才出帧：超过 30 秒上限 → null。
        let b = timeline(31_000_000, 45_000_000);
        let (conv_b, _) = scan_presented_windows(&b, 45_000_000, 60);
        assert!(conv_b.is_none(), "31s 才达标必须报 null，实际 {conv_b:?}");

        // 满速时间线（健康观测）：1 秒整窗口即收敛（1000ms）。
        let obs = healthy_obs();
        let mut presented: Vec<u32> = obs.frames.iter().filter_map(|f| f.presented_us).collect();
        presented.sort_unstable();
        let (conv, degr) = scan_presented_windows(
            &presented,
            u32::try_from(obs.duration_us).unwrap_or(u32::MAX),
            60,
        );
        // 60 帧均匀 16.67ms：任意满 1s 窗口都有 ~60 帧 ≥ 54 → 1000ms 收敛。
        assert_eq!(conv, Some(1000), "满速时间线应 1000ms 收敛");
        assert_eq!(degr, None, "满速不退化");
    }

    /// 空观测：全 0 / null、无 NaN/Infinity、Gate 失败于「无上屏帧」。
    #[test]
    fn zero_frame_report_is_not_nan() {
        let obs = SessionObservations::default();
        let report = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(!report.gate.passed);
        assert!(
            report.gate.failures.iter().any(|f| f.contains("无上屏帧")),
            "{:?}",
            report.gate.failures
        );
        let json = report.to_json();
        assert!(!json.contains("NaN") && !json.contains("inf"), "{json}");
        assert!(json.contains("\"connect_to_first_frame_ms\":null"));
        assert!(json.contains("\"frames\":{\"displayed\":0"));
        assert!(json.contains("\"input_one_way_p50_ms\":null"));
        // 空观测（scripted 模式）同时报无输入样本——如实。
        assert!(
            report
                .gate
                .failures
                .iter()
                .any(|f| f.contains("输入单向延迟样本")),
            "{:?}",
            report.gate.failures
        );
    }

    /// H4 回归：超 71.6min 的 u64 时长（如 2h = 7_200_000_000µs）+ 超 4096 帧
    /// trace 上限的 `total_displayed` 准确计算 `frames.displayed` 与 `fps.displayed`，
    /// 分位数与分段均值仍来自 `obs.frames`。
    #[test]
    fn report_uses_u64_duration_and_unbounded_total_displayed() {
        let mut obs = healthy_obs();
        let two_hours_us: u64 = 7_200_000_000; // > u32::MAX (4_294_967_295)
        let two_hours_frames: u64 = 7_200 * 60; // 432_000 >> 4096
        obs.duration_us = two_hours_us;
        obs.total_displayed = Some(two_hours_frames);

        let mut cfg = cfg_snapshot();
        cfg.duration_s = 7_200.0;
        let report = SessionReport::from_observations(&cfg, &obs);
        assert_eq!(report.frames_displayed, two_hours_frames);
        assert!(
            (report.displayed_fps - 60.0).abs() < 0.01,
            "2h 无界 fps 应为 60.0，实际 {}",
            report.displayed_fps
        );
        assert!(
            (report.latency_p50_ms - 10.5).abs() < 0.01,
            "分位数仍由 obs.frames 算出"
        );
        assert!(
            report.gate.passed,
            "2h 健康 soak 应过 Gate: {:?}",
            report.gate.failures
        );
    }

    /// H5 回归：仅有预期的 `Bye(Refresh)` 纪元重建（`attempts=0, refreshes=1, resumed=false`）
    /// 不得误触发「重连 resume 被拒（resumed=false）」导致 Gate 失败。
    #[test]
    fn refresh_only_reconnect_stats_passes_gate() {
        let mut obs = healthy_obs();
        obs.reconnect = ReconnectStats {
            attempts: 0,
            resumed: false,
            recovery_ms: None,
            refreshes: 1,
        };
        let report = SessionReport::from_observations(&cfg_snapshot(), &obs);
        assert!(
            report.gate.passed,
            "纯饱和刷新不应被误判为 resume 被拒：{:?}",
            report.gate.failures
        );
        let json = report.to_json();
        assert!(
            json.contains("\"refreshes\":1"),
            "JSON 应包含 refreshes 字段：{json}"
        );
    }
}
