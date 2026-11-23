//! `rc-cli client`：控制端子命令（T18）——connect 被控端，跑完整会话，
//! 组装 [`rc_session::report::SessionReport`] 并按 Gate 决定 exit code。
//!
//! 流向约定（S1）：进度/摘要只进 stderr；JSON 只进 `--report` 文件或
//! stdout。驱动循环照 `ClientSession::run` 的形态**手工展开**——run 的
//! 注释明确要求循环外的手工循环照此每拍调用 `pump_probes`；展开是为了
//! 首帧计时（Endpoint 创建之前 → 首帧上屏，坑 3）与 `--reconnect-at`
//! 演练的到点触发。循环内兼负会话钟纪元检测：T17 全新重建（session_id
//! 变更）即重锚归零（修复卡 C-1，会话层契约「驱动器负责把注入钟归零」）。
//! 会话机制全在 rc-session，本文件只接线（薄壳纪律）。

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_pipeline::{DiffMode, PipelineConfig};
use rc_session::proto::{ByeReason, SessionParams};
use rc_session::report::{FrameObservation, SessionConfigSnapshot, SessionReport};
use rc_session::testkit::{ScriptedCapture, ScriptedClipboard};
use rc_session::{
    ClientSession, ClientSessionConfig, ClipboardBackend, ReconnectOutcome, SessionClock,
    SessionState,
};

use super::{BackendMode, CliResult, ClientArgs, ClipboardMode, InputMode};

/// run 循环的常规 poll 预算（照 ClientSession::run 的分档）。
const RUN_POLL_BUDGET: Duration = Duration::from_millis(1);
/// 空闲拍放宽的 poll 预算（client 无 wait_event，靠 poll 预算承担等待）。
const RUN_IDLE_POLL_BUDGET: Duration = Duration::from_millis(10);
/// Reconnecting 退避等待的最大睡眠粒度（照 run 的 RUN_RECONNECT_SLEEP）。
const RUN_RECONNECT_SLEEP: Duration = Duration::from_millis(20);

/// 空输入捕获（`--input-mode none`：不发输入）。
struct IdleCapture;

impl rc_input::InputCapture for IdleCapture {
    fn next_events(&mut self, _timeout: Duration) -> rc_types::Result<Vec<rc_input::InputEvent>> {
        Ok(Vec::new())
    }
}

/// 可重锚的会话钟（修复卡 C-1）：从锚点 `origin` 起算的 µs 读数；重锚 =
/// 往返回的锚点句柄写入新 [`Instant`]（新会话纪元归零，读数从 ≈0 重新
/// 起算）。读数饱和到 u32::MAX 而非回绕（与 host 侧 ClockOrigin 同款式）。
fn reanchorable_clock(origin: Instant) -> (SessionClock, Arc<Mutex<Instant>>) {
    let anchor = Arc::new(Mutex::new(origin));
    let clock: SessionClock = {
        let anchor = Arc::clone(&anchor);
        Arc::new(move || {
            let a = *anchor.lock().expect("会话钟锚点锁");
            u32::try_from(a.elapsed().as_micros()).unwrap_or(u32::MAX)
        })
    };
    (clock, anchor)
}

/// fps 分母（修复卡 C-3）：首帧时刻 → 窗口收口时刻的实际测量窗长（µs）。
/// 无首帧（零上屏）退回请求时长——fps 分子为 0，分母取值不影响判定，
/// 只求报告字段不退化。
fn measurement_duration_us(
    first_frame: Option<Instant>,
    window_close: Instant,
    requested: Duration,
) -> u64 {
    match first_frame {
        Some(f) => u64::try_from(window_close.duration_since(f).as_micros()).unwrap_or(u64::MAX),
        None => u64::try_from(requested.as_micros()).unwrap_or(u64::MAX),
    }
}

/// 剔除排空期上屏帧（修复卡 C-3 分子侧）：`window_end_us` 是驱动循环
/// 收口那一刻的会话钟读数（当前纪元；重建换新 trace，观测帧必同纪元），
/// close() 排空期才呈现的帧 presented_us 严格晚于它。无 presented 打点
/// 的帧（未结/网络丢失）保留——它们计入各自结局桶，本就不进 fps 分子。
/// 返回被剔除的排空期上屏帧数，供调用方同步扣减 `obs.total_displayed`。
fn drop_drain_frames(frames: &mut Vec<FrameObservation>, window_end_us: u32) -> u64 {
    let mut dropped = 0u64;
    frames.retain(|f| {
        let keep = f.presented_us.map_or(true, |p| p <= window_end_us);
        if !keep {
            dropped += 1;
        }
        keep
    });
    dropped
}

/// 执行 `client` 子命令：计时起点（Endpoint 创建之前）→ connect →
/// 驱动会话（scripted 输入经会话内输入泵；--reconnect-at 到点触发 T17
/// 重连）→ duration 到时发 Bye 正常关闭 → 组装 SessionReport →
/// stderr 摘要 + 报告落盘。Gate 通过 SUCCESS，未通过 FAILURE
/// （loopback.rs 的模式）。
pub fn run(args: &ClientArgs) -> CliResult<ExitCode> {
    // ── 证书（S3：默认内嵌开发证书，--server-cert/--server-name 可覆盖）──
    let server_cert = match &args.server_cert {
        Some(p) => {
            fs::read_to_string(p).map_err(|e| format!("无法读服务端证书 {}: {e}", p.display()))?
        }
        None => rc_session::dev_cert::dev_server_cert_pem().to_string(),
    };

    // ── 输入捕获（scripted 默认；real 仅 macOS）──
    let capture: Box<dyn rc_input::InputCapture> = match args.input_mode {
        InputMode::None => Box::new(IdleCapture),
        InputMode::Scripted => Box::new(ScriptedCapture::standard()),
        #[cfg(target_os = "macos")]
        InputMode::Real => Box::new(
            rc_input_macos::CgCapture::new()
                .map_err(|e| format!("CG 输入捕获初始化失败（需输入监控权限）: {e}"))?,
        ),
        #[cfg(not(target_os = "macos"))]
        InputMode::Real => {
            return Err("--input-mode real 仅 macOS 支持（其他平台用 scripted/none）".into())
        }
    };

    // ── 剪贴板后端前置校验（on 模式启动即校验，非 macOS 不做静默空操作；
    //    scripted 模式待握手后再构造，确保 500ms 延迟从 Streaming 开始起算）──
    let mut prebuilt_clipboard = match args.clipboard {
        ClipboardMode::Off | ClipboardMode::Scripted => None,
        ClipboardMode::On => Some(build_real_clipboard_backend()?),
    };

    // ── 会话配置（管线模板：握手后以裁定值覆盖，实际后端由工厂选）──
    let pipeline = PipelineConfig {
        resolution: args.resolution,
        fps: args.fps,
        bitrate_bps: 8_000_000,
        prefer_hardware: args.backend != BackendMode::Fake,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；控制端不分片，此字段对 client 侧无效。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let session_id = derive_session_id();
    let cfg = ClientSessionConfig {
        peer_addr: args.connect,
        mtu: rc_session::DEFAULT_SESSION_MTU,
        requested: SessionParams {
            width: args.resolution.width,
            height: args.resolution.height,
            fps: args.fps,
            bitrate_bps: 8_000_000,
        },
        session_id,
        pipeline,
        connect_timeout: Duration::from_secs(10),
        handshake_timeout: Duration::from_secs(10),
        capture,
        input_window: rc_input::DEFAULT_WINDOW,
        auth: rc_session::ClientAuthConfig {
            password: args.password.clone(),
            ..rc_session::ClientAuthConfig::default()
        },
    };

    // ── 计时起点：Endpoint 创建之前（坑 3 写死的口径）──
    let t0 = Instant::now();
    // 会话钟 = 可变锚点（修复卡 C-1，对齐 host 侧 ClockOrigin 模式）：
    // 初始锚 t0（client 域标记与 connect 计时同轴）；T17 全新重建时驱动
    // 循环重锚（见循环内纪元检测）。会话层契约（client_session 的
    // on_bye_refresh 与重建路径注释）写明「驱动器负责把注入钟归零」：
    // Bye(Refresh) 把 next_attempt_at_us 锚到 0，正是依赖重锚后新纪元
    // 读数 ≈0 才能零退避立即到点。旧实现锚死 t0 从不重锚——钟读数单调
    // 爬到 u32::MAX（约 71.6 分钟）后冻结，看门狗/探测/退避全部停摆。
    let (clock, clock_anchor) = reanchorable_clock(t0);
    let clock_reader = Arc::clone(&clock);

    // ── 管线工厂：fake / real（VT 解码 + Metal 呈现）/ auto 降级 ──
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let factory = {
        let label = Arc::clone(&backend_label);
        let mode = args.backend;
        Box::new(
            move |pcfg: &PipelineConfig| -> rc_types::Result<rc_pipeline::ControllerPipeline> {
                let mut hw_cfg = pcfg.clone();
                hw_cfg.prefer_hardware = true;
                match mode {
                    BackendMode::Fake => {
                        *label.lock().expect("label lock") = "fake".into();
                        fake_controller_stack(pcfg)
                    }
                    BackendMode::Real | BackendMode::Auto => match real_controller_stack(&hw_cfg) {
                        Ok((p, l)) => {
                            *label.lock().expect("label lock") = l.clone();
                            if mode == BackendMode::Auto {
                                eprintln!("rc-cli client: 后端 {l}（真后端优先）");
                            }
                            Ok(p)
                        }
                        Err(e) if mode == BackendMode::Auto => {
                            eprintln!("rc-cli client: 真后端不可用（{e}），降级假后端");
                            *label.lock().expect("label lock") = "fake(降级)".into();
                            fake_controller_stack(pcfg)
                        }
                        Err(e) => Err(e),
                    },
                }
            },
        )
    };

    // ── 组装会话（本端端点身份也用开发证书：对端不校验客户端证书）──
    let mut client = ClientSession::new(
        cfg,
        rc_session::dev_cert::DEV_CERT_PEM,
        rc_session::dev_cert::DEV_KEY_PEM,
        &server_cert,
        &args.server_name,
        clock,
    )
    .map_err(|e| format!("client 会话构造失败: {e}"))?;
    client.set_pipeline_factory_for(factory);

    eprintln!(
        "client 连接中：{}（{}x{} @ {}fps，输入={}{}）",
        args.connect,
        args.resolution.width,
        args.resolution.height,
        args.fps,
        input_label(args.input_mode),
        args.reconnect_at
            .map(|d| format!("，{} 后断线演练重连", hum(&d)))
            .unwrap_or_default(),
    );

    client
        .connect_and_handshake()
        .map_err(|e| format!("connect/握手失败: {e}"))?;
    let backend = backend_label.lock().expect("label lock").clone();
    let negotiated = client.negotiated().expect("握手完成即有裁定值");
    eprintln!(
        "client 已连接：后端={backend}，裁定 {}x{} @ {}fps，session_id={session_id}",
        negotiated.width, negotiated.height, negotiated.fps,
    );

    // ── 剪贴板显式启用（CB1：Off 不调用 enable_clipboard）──
    match args.clipboard {
        ClipboardMode::Off => {}
        ClipboardMode::On => {
            if let Some(cb) = prebuilt_clipboard.take() {
                client.enable_clipboard(cb);
            }
        }
        ClipboardMode::Scripted => {
            client.enable_clipboard(Box::new(ScriptedClipboard::standard()));
        }
    }

    // ── 驱动循环（照 ClientSession::run 手工展开；见模块文档）──
    let duration = args.duration;
    let mut last_busy = true;
    let mut first_frame_us: Option<u64> = None;
    // 首帧的墙钟时刻（修复卡 C-3：fps 测量窗从首帧起算，与 first_frame_us
    // 同一拍捕获）。
    let mut first_frame_at: Option<Instant> = None;
    let mut reconnect_triggered = false;
    // 纪元判据（修复卡 C-1）：session_id 只在「全新重建」时变更——
    // Bye(Refresh)（on_bye_refresh 自增）与 resume 被拒降级（先自增再补
    // 全新 Hello）两处，是驱动器可见的最早重建信号。
    let mut epoch_sid = session_id;
    while client.state() != SessionState::Closed && t0.elapsed() < duration {
        match client.state() {
            SessionState::Streaming => {
                let now_us = u64::from((clock_reader)());
                client
                    .pump_probes(now_us)
                    .map_err(|e| format!("client 探测泵失败: {e}"))?;
                client
                    .start_bw_probe(now_us)
                    .map_err(|e| format!("client 带宽粗测发起失败: {e}"))?;
                let budget = if last_busy {
                    RUN_POLL_BUDGET
                } else {
                    RUN_IDLE_POLL_BUDGET
                };
                let r = client
                    .tick(budget, Instant::now())
                    .map_err(|e| format!("client 会话运行失败: {e}"))?;
                if r.closed.is_some() {
                    break; // Bye 已在 tick 内完成停机
                }
                last_busy = r.busy;
                // 首帧计时（口径：t0 = Endpoint 创建之前）。
                if first_frame_us.is_none() && client.pipeline().displayed() > 0 {
                    first_frame_at = Some(Instant::now());
                    first_frame_us =
                        Some(u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
                // --reconnect-at 到点主动断开传输层，触发 T17 退避重连。
                if let Some(at) = args.reconnect_at {
                    if !reconnect_triggered && t0.elapsed() >= at {
                        reconnect_triggered = true;
                        eprintln!(
                            "client 重连演练：{} 到点主动断开（T17 自动退避重连）",
                            hum(&at)
                        );
                        client.on_disconnected(u64::from((clock_reader)()));
                    }
                }
            }
            SessionState::Reconnecting => {
                let outcome = client
                    .tick_reconnect(u64::from((clock_reader)()))
                    .map_err(|e| format!("client 重连驱动失败: {e}"))?;
                match outcome {
                    ReconnectOutcome::Waiting { next_attempt_in_ms } => {
                        let nap =
                            Duration::from_millis(next_attempt_in_ms).min(RUN_RECONNECT_SLEEP);
                        std::thread::sleep(nap);
                    }
                    ReconnectOutcome::Resumed | ReconnectOutcome::Rebuilt => {
                        on_reconnect_clipboard(&mut client, &outcome);
                    }
                    ReconnectOutcome::Connecting | ReconnectOutcome::GaveUp => {}
                }
            }
            // Init/Connected/Detached 不是驱动循环可达态（client 不进 Detached）。
            _ => break,
        }
        // ── 纪元检测（修复卡 C-1）：会话重建即重锚会话钟。────────
        // 覆盖两类拍内迁移：Streaming 拍的 Bye(Refresh)（重锚后下一拍
        // tick_reconnect 的零退避 now>=0 立即成立）与 Reconnecting 拍的
        // resume 被拒降级（重锚发生在 Welcome(Rebuilt) 之前，重建后的
        // 看门狗 arm / 探测样本全部落在新纪元）。resume 命中（管线复用）
        // 不改会话号——不重锚，纪元延续，recovery_ms 才能跨断线测得。
        if client.session_id() != epoch_sid {
            epoch_sid = client.session_id();
            *clock_anchor.lock().expect("会话钟锚点锁") = Instant::now();
            eprintln!("client 会话重建：session_id={epoch_sid}，会话钟纪元归零");
        }
    }
    // ── 测量窗收口（修复卡 C-3，fps 口径两端一致）────────────────
    // 声明口径（report.rs / rc-loopback「fps 分母 = 测量时长、不含排空」）
    // 下，旧实现两头各偏一边：分母是自 t0 起算的 --duration（含建连/握手
    // 死区——首帧前无帧可上屏，只会线性压低 fps），分子却含 close() 排空
    // 期上屏的帧。对齐口径：分母 = 首帧时刻 → 收口时刻的实际窗长；分子 =
    // 窗口内上屏的帧（收口后才呈现的排空帧被剔除，见 drop_drain_frames）。
    // window_end_us 必须在 close() 之前读：排空期呈现的帧 presented_us
    // 严格晚于它（当前纪元单调），而窗口内的帧严格早于它。
    let window_close = Instant::now();
    let window_end_us = (clock_reader)();

    // 正常收尾：到时未收 Bye 则主动 Bye（S6 停机由会话层完成）。
    if client.state() != SessionState::Closed {
        client
            .close(ByeReason::Normal)
            .map_err(|e| format!("client 优雅关闭失败: {e}"))?;
    }

    // ── 报告组装（铁律 4：观测全部来自会话记账，组装不读钟——窗口
    //    收口量是驱动器（组合根）注入的观测，与 connect_to_first_frame
    //    同一模式）──
    let duration_us = measurement_duration_us(first_frame_at, window_close, duration);
    let mut obs = client.observations(duration_us, first_frame_us);
    let dropped_drain = drop_drain_frames(&mut obs.frames, window_end_us);
    if let Some(total) = obs.total_displayed.as_mut() {
        *total = total.saturating_sub(dropped_drain);
    }
    let snapshot = SessionConfigSnapshot {
        resolution: rc_types::Resolution::new(negotiated.width, negotiated.height),
        fps: negotiated.fps,
        duration_s: duration.as_secs_f64(),
        backend,
        connect_addr: args.connect,
        input_mode: input_label(args.input_mode),
    };
    let report = SessionReport::from_observations(&snapshot, &obs);

    // ── stderr 摘要（照 loopback 的完成行）──
    let gate = report.gate();
    eprintln!(
        "client 完成：Gate {}（首帧 {}ms，p50 {:.1}ms，p99 {:.1}ms，displayed {:.1}fps，输入单向 p50 {}ms，队列深度峰值 {}，重连恢复 {:?}）",
        if gate.passed { "通过" } else { "未通过" },
        match report.connect_to_first_frame_ms() {
            Some(ms) => format!("{ms:.0}"),
            None => "null".to_string(),
        },
        report.latency_p50_ms(),
        report.latency_p99_ms(),
        report.displayed_fps(),
        match report.input_one_way_p50_ms() {
            Some(ms) => format!("{ms:.2}"),
            None => "null".to_string(),
        },
        obs.max_reassembly_depth,
        obs.reconnect.recovery_ms,
    );
    for failure in &gate.failures {
        eprintln!("  失败项：{failure}");
    }

    write_report(args, &report.to_json())?;

    if gate.passed {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// fake 控制端栈：FakeDecoder + FakeRenderer（分辨率对齐裁定值）。
fn fake_controller_stack(
    pcfg: &PipelineConfig,
) -> rc_types::Result<rc_pipeline::ControllerPipeline> {
    let decoder = rc_decode::FakeDecoder::new(rc_decode::FakeDecoderConfig {
        resolution: pcfg.resolution,
        ..rc_decode::FakeDecoderConfig::default()
    })?;
    let renderer = rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
        size: (pcfg.resolution.width, pcfg.resolution.height),
        ..rc_render::FakeRendererConfig::default()
    })?;
    rc_pipeline::ControllerPipeline::with_backends(
        pcfg.clone(),
        Box::new(decoder),
        Box::new(renderer),
    )
}

/// 真后端 client 半链：VideoToolbox 解码 + Metal 呈现（会弹真窗口），
/// 任一降级报错。
fn real_controller_stack(
    hw_cfg: &PipelineConfig,
) -> rc_types::Result<(rc_pipeline::ControllerPipeline, String)> {
    let (decoder, dl) = rc_pipeline::select_decoder_labeled(hw_cfg)?;
    let (renderer, rl) = rc_pipeline::select_renderer_labeled(hw_cfg)?;
    if dl != "videotoolbox" || rl != "metal" {
        return Err(rc_types::Error::InvalidConfig(format!(
            "real 后端要求 videotoolbox+metal，实际 {dl}+{rl}"
        )));
    }
    let pipeline =
        rc_pipeline::ControllerPipeline::with_backends(hw_cfg.clone(), decoder, renderer)?;
    Ok((pipeline, format!("{dl}+{rl}")))
}

/// 会话号：pid 与单调计数合成（client 生成，调用方负责——此处即调用方）。
fn derive_session_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    (u64::from(std::process::id()) << 20) | (n & 0xF_FFFF) | 0x100_000
}

/// 构造真实平台剪贴板后端（仅 macOS；非 macOS 返回 InvalidConfig 错误，不做静默空操作）。
fn build_real_clipboard_backend() -> CliResult<Box<dyn ClipboardBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(
            rc_session::NspasteboardBackend::new()
                .map_err(|e| format!("macOS 剪贴板后端初始化失败: {e}"))?,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(rc_types::Error::InvalidConfig(
            "--clipboard on 仅 macOS 支持（其他平台请使用 off / scripted）".into(),
        )
        .to_string())
    }
}

/// L1：重连恢复或重建后仅重置剪贴板纪元态（保留原后端与累计 `ClipboardStats`）。
fn on_reconnect_clipboard(client: &mut ClientSession, outcome: &ReconnectOutcome) {
    if matches!(
        outcome,
        ReconnectOutcome::Resumed | ReconnectOutcome::Rebuilt
    ) {
        client.reset_clipboard_epoch();
    }
}

/// 输入模式的报告标签。
fn input_label(mode: InputMode) -> &'static str {
    match mode {
        InputMode::None => "none",
        InputMode::Scripted => "scripted",
        InputMode::Real => "real",
    }
}

/// 时长的人读形式（stderr 摘要用）。
fn hum(d: &Duration) -> String {
    format!("{}ms", d.as_millis())
}

/// 报告输出：`--report <path>` 写文件，`--report -` 或省略写 stdout（S1）。
fn write_report(args: &ClientArgs, json: &str) -> CliResult<()> {
    match args.report.as_deref() {
        Some(path) if path != Path::new("-") => fs::write(path, format!("{json}\n"))
            .map_err(|e| format!("无法写报告文件 {}: {e}", path.display())),
        _ => {
            println!("{json}");
            Ok(())
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// 测试：驱动器侧纪元与 fps 口径机制的单元锁（离线，无网络、无会话）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 修复卡 C-1：重锚信号后钟读数归零附近（新纪元从 ≈0 重新起算），
    /// 且小于重锚前的旧纪元读数——证明重锚真的发生（旧实现锚死进程启动
    /// t0 从不重锚，读数只增不减，约 71.6 分钟后冻结 u32::MAX，看门狗/
    /// 探测/退避全部停摆）。
    #[test]
    fn clock_reanchor_resets_epoch_near_zero() {
        let (clock, anchor) = reanchorable_clock(Instant::now());
        std::thread::sleep(Duration::from_millis(50));
        let before = clock();
        assert!(
            before >= 40_000,
            "重锚前旧纪元读数应 ≈50ms，实际 {before}µs"
        );
        *anchor.lock().expect("会话钟锚点锁") = Instant::now();
        let after = clock();
        assert!(
            after < before,
            "重锚后读数必须回到旧纪元读数之前（{after} ≥ {before}）"
        );
        assert!(
            after < 50_000,
            "重锚后读数应归零附近（<50ms），实际 {after}µs"
        );
    }

    /// 修复卡 C-3：fps 分母 = 首帧时刻 → 收口时刻的窗长（不含建连死区）；
    /// 无首帧退回请求时长（零上屏时报告字段不退化，含饱和语义）。
    #[test]
    fn measurement_duration_starts_at_first_frame() {
        // 首帧在 2s 前、刚刚收口 → 窗长 ≈2s（容忍 ±100ms 调度抖动）。
        let first = Instant::now() - Duration::from_secs(2);
        let close = Instant::now();
        let d = measurement_duration_us(Some(first), close, Duration::from_secs(8));
        assert!(
            (1_900_000..=2_100_000).contains(&d),
            "首帧起算的窗长应 ≈2s，实际 {d}µs"
        );
        // 无首帧 → 请求时长原样透传；超 u32 范围时 u64 精确保留不饱和。
        let close = Instant::now();
        assert_eq!(
            measurement_duration_us(None, close, Duration::from_secs(8)),
            8_000_000
        );
        assert_eq!(
            measurement_duration_us(None, close, Duration::from_secs(6_000)),
            6_000_000_000u64,
            "超 71.6min 的请求时长 u64 精确保留不饱和"
        );
    }

    /// 修复卡 C-3：分子侧剔除排空期上屏帧——presented 晚于窗口收口读数
    /// 的帧被剔除；窗口内的帧与无 presented 打点的帧（未结/网络丢失，
    /// 本就不进 fps 分子）保留。
    #[test]
    fn drain_frames_are_dropped_from_observations() {
        let frame = |presented: Option<u32>| FrameObservation {
            seq: rc_types::FrameSeq(0),
            outcome: None,
            captured_us: 0,
            encode_done_us: 0,
            received_us: None,
            decoded_us: None,
            presented_us: presented,
        };
        let mut frames = vec![
            frame(Some(100)),   // 窗口内上屏帧
            frame(Some(5_000)), // 恰在收口读数上（边界含）
            frame(Some(5_001)), // 排空期上屏帧：剔除
            frame(None),        // 未结/丢失帧：保留
        ];
        drop_drain_frames(&mut frames, 5_000);
        assert_eq!(frames.len(), 3, "只剔除排空期上屏帧");
        assert_eq!(frames[0].presented_us, Some(100));
        assert_eq!(frames[1].presented_us, Some(5_000));
        assert_eq!(frames[2].presented_us, None);
    }

    /// L1 回归：
    /// 1) `ClientSession::tick` 内部已泵剪贴板（驱动循环不再冗余调用 `pump_clipboard`）；
    /// 2) `on_reconnect_clipboard` 在 `Resumed`/`Rebuilt` 时调用 `reset_clipboard_epoch()`，
    ///    保留累计 `ClipboardStats` 且时钟归零后可继续发送新剪贴板内容。
    #[test]
    fn reconnect_clipboard_resets_epoch_and_preserves_stats() {
        use rc_session::proto::{AuthStatus, ControlMessage, HostFlags, Welcome};
        use rc_session::testkit::{MockTransport, SharedFakeClipboard};
        use std::sync::atomic::{AtomicU32, Ordering};

        let pcfg = PipelineConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 60,
            bitrate_bps: 8_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 64,
            diff: DiffMode::TrustPlatform,
            clock_origin_us: None,
            max_packet_payload: rc_pipeline::MAX_PAYLOAD,
        };
        let params = SessionParams {
            width: 64,
            height: 64,
            fps: 60,
            bitrate_bps: 8_000_000,
        };

        let mut transport = MockTransport::new();
        transport.deliver_reliable(&ControlMessage::Welcome(Welcome {
            session_id: 77,
            granted: params,
            auth_status: AuthStatus::Ok,
            host_flags: HostFlags(0),
        }));

        let client_dec = rc_decode::FakeDecoder::new(rc_decode::FakeDecoderConfig {
            resolution: pcfg.resolution,
            ..rc_decode::FakeDecoderConfig::default()
        })
        .expect("fake decoder");
        let client_ren = rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
            size: (pcfg.resolution.width, pcfg.resolution.height),
            ..rc_render::FakeRendererConfig::default()
        })
        .expect("fake renderer");
        let now_us = Arc::new(AtomicU32::new(4_000_000_000));
        let client_clock: SessionClock = {
            let now_us = Arc::clone(&now_us);
            Arc::new(move || now_us.load(Ordering::Relaxed))
        };
        let mut client = ClientSession::with_parts(
            ClientSessionConfig {
                peer_addr: "127.0.0.1:0".parse().unwrap(),
                mtu: rc_session::DEFAULT_SESSION_MTU,
                requested: params,
                session_id: 77,
                pipeline: pcfg,
                connect_timeout: Duration::from_secs(5),
                handshake_timeout: Duration::from_secs(5),
                capture: Box::new(IdleCapture),
                input_window: rc_input::DEFAULT_WINDOW,
                auth: rc_session::ClientAuthConfig::default(),
            },
            Box::new(transport),
            Box::new(client_dec),
            Box::new(client_ren),
            client_clock,
        )
        .expect("client with_parts");
        client.handshake().expect("client 握手");

        let cb = SharedFakeClipboard::new();
        client.enable_clipboard(Box::new(cb.clone()));

        // 首拍建立基线，随后模拟本机复制，由 tick() 内部泵出（无外部冗余 pump_clipboard）。
        let t = Instant::now();
        client
            .tick(Duration::from_millis(1), t)
            .expect("baseline tick");
        cb.simulate_local_copy(Some("before-reconnect"));
        now_us.store(4_000_100_000, Ordering::Relaxed);
        client
            .tick(Duration::from_millis(1), t)
            .expect("send tick 1");
        assert_eq!(client.clipboard_stats().sent, 1);

        // 模拟重连 Resumed + 时钟纪元归零：统计必须保留且新纪元能继续发送。
        on_reconnect_clipboard(&mut client, &ReconnectOutcome::Resumed);
        assert_eq!(
            client.clipboard_stats().sent,
            1,
            "Resumed 重置纪元不得清零累计 ClipboardStats"
        );
        now_us.store(0, Ordering::Relaxed);
        client
            .tick(Duration::from_millis(1), t)
            .expect("new epoch baseline");
        cb.simulate_local_copy(Some("after-resumed"));
        now_us.store(100_000, Ordering::Relaxed);
        client
            .tick(Duration::from_millis(1), t)
            .expect("send tick 2");
        assert_eq!(client.clipboard_stats().sent, 2);

        // 模拟 Rebuilt：同样保留累计统计。
        on_reconnect_clipboard(&mut client, &ReconnectOutcome::Rebuilt);
        assert_eq!(
            client.clipboard_stats().sent,
            2,
            "Rebuilt 重置纪元不得清零累计 ClipboardStats"
        );
    }
}
