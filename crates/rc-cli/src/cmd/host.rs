//! `rc-cli host`：被控端子命令（T18）——accept 一条 QUIC 连接，流媒体 +
//! 注入输入，stderr 摘要 + 可选报告落盘。
//!
//! 流向约定（S1，照抄 loopback）：进度/摘要只进 stderr；JSON 只进
//! `--report` 文件或 stdout。会话逻辑全在 rc-session（`HostSession`），
//! 本文件只做参数 → 会话配置 → 驱动 → 报告的接线（薄壳纪律）。
//! **不做 Gate 判定**（被控端没有 e2e 观测）；停机顺序红线由会话层保证
//! （S8），CLI 不自写停机。

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_pipeline::{DiffMode, PipelineConfig};
use rc_session::proto::SessionParams;
use rc_session::testkit::{RecordingInjector, ScriptedClipboard};
use rc_session::{
    AutoApprover, ClipboardBackend, ConnectionApprover, DenyApprover, HostRunSummary, HostSession,
    HostSessionConfig, SessionClock,
};

use super::{ApprovalMode, BackendMode, Cli, CliResult, ClipboardMode, HostArgs, InjectMode};

/// accept 等待上限（`--duration` 只计接受连接后的会话时长，坑 7）。
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
/// headless 模式下省略 `--duration` 时的默认会话兜底时长（保持 T18 行为，U3）。
const DEFAULT_HOST_DURATION: Duration = Duration::from_secs(30);
/// 托盘模式下单次 accept 分片时长：兼顾 QUIC 握手完成预算与点击「退出」后的快速收尾。
#[cfg(target_os = "macos")]
const TRAY_ACCEPT_SLICE: Duration = Duration::from_secs(1);
/// fake 编码器的单帧编码成本（微秒）。
const FAKE_ENCODE_COST_US: u32 = 500;

/// 会话钟原点：握手/审批期先以构造时刻的墙钟推进（保证 `HostAuthenticator::poll_approval`
/// 的 `--approval-timeout` 计时正常触发 `Welcome{Rejected}`），管线工厂构造后端的那一刻再
/// 重锚（fake = Instant，与 FakeBackend 时间网格零点同刻；real = 主机钟 µs，与 SCK PTS 同域，
/// 照 run_real 的 session_origin_us 模式）。
#[derive(Debug, Clone)]
enum ClockOrigin {
    /// 尚未由管线工厂锚定（握手/审批阶段使用构造时刻的墙钟）。
    Pending(Instant),
    /// fake 后端：锚 Instant（与假栈 WallCapture 帧时间戳**同一个**采样，
    /// 修复卡 C-4 单采样不变式——e2e 的 captured→presented 链路换算依赖
    /// 两轴同零点）。
    Fake(Instant),
    /// 真后端：锚 `host_clock_us()`（SCK PTS 同域）。
    HostClock(u64),
}

/// M6：在 `accept_and_handshake` 等待分片前重锚 `Pending` 会话钟原点，
/// 防止托盘长时空等（>71.6 分钟即 `u32::MAX` µs）后会话钟饱和导致 `approval_timeout_us` 失效。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn reanchor_pending_clock(origin: &Arc<Mutex<ClockOrigin>>) {
    let mut guard = origin.lock().expect("origin lock");
    if matches!(*guard, ClockOrigin::Pending(_)) {
        *guard = ClockOrigin::Pending(Instant::now());
    }
}

/// 根据共享的 [`ClockOrigin`] 构造会话钟闭包（握手期 `Pending` 推进，管线构造后重锚）。
fn make_session_clock(origin: Arc<Mutex<ClockOrigin>>) -> SessionClock {
    Arc::new(move || {
        let guard = origin.lock().expect("origin lock");
        match *guard {
            ClockOrigin::Pending(i) | ClockOrigin::Fake(i) => {
                u32::try_from(i.elapsed().as_micros()).unwrap_or(u32::MAX)
            }
            ClockOrigin::HostClock(o) => {
                u32::try_from(rc_pipeline::host_clock_us().unwrap_or(o).saturating_sub(o))
                    .unwrap_or(u32::MAX)
            }
        }
    })
}

/// 执行 `host` 子命令：参数校验 → 证书加载 → `--tray` 分支或 headless 单会话。
/// 正常收尾 SUCCESS；运行期错误经 CliResult 上抛（main 打印后 exit 1）；
/// `--approval prompt` 缺少 `--tray` 时由 clap 报告参数冲突并 exit 2。**不做 Gate 判定。**
pub fn run(args: &HostArgs) -> CliResult<ExitCode> {
    if args.approval == Some(ApprovalMode::Prompt) && !args.tray {
        use clap::CommandFactory;
        Cli::command()
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--approval prompt 必须与 --tray 同用（无托盘时无法弹出审批窗口）",
            )
            .exit();
    }

    // ── 证书（S3：默认内嵌开发证书，--cert/--key 可覆盖）──
    let cert_pem = match &args.cert {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("无法读证书 {}: {e}", p.display()))?,
        None => rc_session::dev_cert::DEV_CERT_PEM.to_string(),
    };
    let key_pem = match &args.key {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("无法读私钥 {}: {e}", p.display()))?,
        None => rc_session::dev_cert::DEV_KEY_PEM.to_string(),
    };
    // ── 开发证书非环回警告（修复卡D-3）：内嵌开发证书的私钥随源码
    //    公开，不构成安全边界；非环回监听还在用它，任何能路由到本机
    //    的人都能解密/冒充会话。打醒目警告但不阻止（0.0.0.0 快速联调
    //    是合法的开发场景）。──
    if let Some(warn) = dev_cert_warning(&args.listen, args.cert.as_deref(), args.key.as_deref()) {
        eprintln!("{warn}");
    }

    if args.tray {
        return run_tray(args, cert_pem, key_pem);
    }

    let approver: Box<dyn ConnectionApprover> = match args.approval.unwrap_or(ApprovalMode::Auto) {
        ApprovalMode::Auto => Box::new(AutoApprover),
        ApprovalMode::Deny => Box::new(DenyApprover),
        ApprovalMode::Prompt => unreachable!("入口已拦截无 --tray 的 --approval prompt"),
    };
    let BuiltHostSession {
        mut host,
        backend_label,
        mut prebuilt_clipboard,
        ..
    } = build_host_session(args, args.listen, &cert_pem, &key_pem, approver)?;

    let listen = host
        .local_addr()
        .map_err(|e| format!("无法获取监听地址: {e}"))?;
    #[cfg(target_os = "macos")]
    let _activity = if matches!(args.backend, super::BackendMode::Real) {
        rc_capture_macos::promote_current_thread_qos();
        Some(rc_capture_macos::LatencyCriticalActivity::begin(
            "RabbitControl host",
            true,
        ))
    } else {
        None
    };
    eprintln!(
        "host 等待连接：listen={listen}（{}x{} @ {}fps，注入={}）",
        args.resolution.width,
        args.resolution.height,
        args.fps,
        inject_label(args.inject_mode),
    );

    // ── accept + 握手（等待期不消耗 --duration，坑 7）──
    host.accept_and_handshake(ACCEPT_TIMEOUT)
        .map_err(|e| format!("accept/握手失败: {e}"))?;
    let backend = backend_label.lock().expect("label lock").clone();
    let negotiated = host.negotiated().expect("握手完成即有裁定值");
    eprintln!(
        "host 已连接：后端={backend}，裁定 {}x{} @ {}fps @ {}bps，session_id={}",
        negotiated.width,
        negotiated.height,
        negotiated.fps,
        negotiated.bitrate_bps,
        host.session_id().unwrap_or(0),
    );

    enable_host_clipboard(&mut host, args.clipboard, &mut prebuilt_clipboard);

    // ── 驱动到 Bye 或 --duration 兜底（S8：停机顺序由会话层保证）──
    let connected = Instant::now();
    let duration = args.duration.unwrap_or(DEFAULT_HOST_DURATION);
    let summary = host
        .run(&mut || connected.elapsed() >= duration)
        .map_err(|e| format!("host 会话运行失败: {e}"))?;
    let connected_ms = connected.elapsed().as_millis();

    let report = format_and_log_summary(
        args,
        &host,
        &summary,
        &backend,
        listen,
        duration,
        connected_ms,
    );
    write_report(args, &report)?;

    Ok(ExitCode::SUCCESS)
}

/// `build_host_session` 的组装产出。
struct BuiltHostSession {
    host: HostSession,
    backend_label: Arc<Mutex<String>>,
    prebuilt_clipboard: Option<Box<dyn ClipboardBackend>>,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    clock_origin: Arc<Mutex<ClockOrigin>>,
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    clock: SessionClock,
}

/// 构造一轮 `HostSession`（注入器、剪贴板预检、鉴权/审批配置、会话钟与管线工厂）。
fn build_host_session(
    args: &HostArgs,
    listen_addr: SocketAddr,
    cert_pem: &str,
    key_pem: &str,
    approver: Box<dyn ConnectionApprover>,
) -> CliResult<BuiltHostSession> {
    // ── 注入器（record 默认免权限；real 仅 macOS）──
    let injector: Box<dyn rc_input::InputInjector> = match args.inject_mode {
        InjectMode::Record => Box::new(RecordingInjector::new()),
        #[cfg(target_os = "macos")]
        InjectMode::Real => Box::new(
            rc_input_macos::InputBridge::new()
                .map_err(|e| format!("CGEvent 注入桥初始化失败（需辅助功能权限）: {e}"))?,
        ),
        #[cfg(not(target_os = "macos"))]
        InjectMode::Real => {
            return Err("--inject-mode real 仅 macOS 支持（其他平台用 record）".into())
        }
    };

    // ── 剪贴板后端前置校验（on 模式启动即校验，非 macOS 不做静默空操作；
    //    scripted 模式待握手后再构造，确保 500ms 延迟从 Streaming 开始起算）──
    let prebuilt_clipboard = match args.clipboard {
        ClipboardMode::Off | ClipboardMode::Scripted => None,
        ClipboardMode::On => Some(build_real_clipboard_backend()?),
    };

    // ── 会话配置（管线模板：握手后以裁定值覆盖，实际后端由工厂选）──
    let pipeline = PipelineConfig {
        resolution: args.resolution,
        fps: args.fps,
        bitrate_bps: args.bitrate_bps,
        prefer_hardware: args.backend != BackendMode::Fake,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；expect_hello 握手时按 N2 覆盖为 1120
        //（max_media_payload(DEFAULT_SESSION_MTU)）。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let approval_timeout_us = u64::try_from(args.approval_timeout.as_micros()).unwrap_or(u64::MAX);
    let cfg = HostSessionConfig {
        listen_addr,
        mtu: rc_session::DEFAULT_SESSION_MTU,
        caps: SessionParams {
            width: args.resolution.width,
            height: args.resolution.height,
            fps: args.fps,
            bitrate_bps: args.bitrate_bps,
        },
        pipeline,
        handshake_timeout: Duration::from_secs(10),
        injector,
        pace_capture: true, // 仅 fake 工厂路径实际生效（真采集不节流）
        auth: rc_session::HostAuthConfig {
            password: args.password.clone(),
            approval_timeout_us,
            ..rc_session::HostAuthConfig::default()
        },
        approver,
    };

    // ── 会话钟（握手/审批期以当前墙钟推进，管线工厂构造后端时重锚，见 ClockOrigin 文档）──
    let origin: Arc<Mutex<ClockOrigin>> =
        Arc::new(Mutex::new(ClockOrigin::Pending(Instant::now())));
    let clock: SessionClock = make_session_clock(Arc::clone(&origin));

    // ── 后端标签（工厂填充，stderr 与报告用）──
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // ── 管线工厂：按裁定参数构造选定后端（fake 含 --static-content 栈）──
    let factory = {
        let origin = Arc::clone(&origin);
        let label = Arc::clone(&backend_label);
        let mode = args.backend;
        let static_content = args.static_content;
        Box::new(
            move |pcfg: &PipelineConfig| -> rc_types::Result<rc_pipeline::HostPipeline> {
                let mut hw_cfg = pcfg.clone();
                hw_cfg.prefer_hardware = true;
                match mode {
                    BackendMode::Fake => {
                        *label.lock().expect("label lock") = "fake".into();
                        let (p, anchor) = fake_stack_anchored(pcfg, static_content)?;
                        *origin.lock().expect("origin lock") = ClockOrigin::Fake(anchor);
                        Ok(p)
                    }
                    BackendMode::Real | BackendMode::Auto => match real_host_stack(&hw_cfg) {
                        Ok((p, l, origin_us)) => {
                            *label.lock().expect("label lock") = l.clone();
                            *origin.lock().expect("origin lock") =
                                ClockOrigin::HostClock(origin_us);
                            if mode == BackendMode::Auto {
                                eprintln!("rc-cli host: 后端 {l}（真后端优先）");
                            }
                            Ok(p)
                        }
                        Err(e) if mode == BackendMode::Auto => {
                            eprintln!("rc-cli host: 真后端不可用（{e}），降级假后端");
                            *label.lock().expect("label lock") = "fake(降级)".into();
                            let (p, anchor) = fake_stack_anchored(pcfg, static_content)?;
                            *origin.lock().expect("origin lock") = ClockOrigin::Fake(anchor);
                            Ok(p)
                        }
                        Err(e) => Err(e),
                    },
                }
            },
        )
    };

    let mut host = HostSession::new(cfg, cert_pem, key_pem, Arc::clone(&clock))
        .map_err(|e| format!("host 会话构造失败: {e}"))?;
    host.set_pipeline_factory_for(factory);
    Ok(BuiltHostSession {
        host,
        backend_label,
        prebuilt_clipboard,
        clock_origin: origin,
        clock,
    })
}

/// 握手后按配置启用剪贴板（CB1：Off 不启用；Scripted 从 Streaming 起表）。
fn enable_host_clipboard(
    host: &mut HostSession,
    mode: ClipboardMode,
    prebuilt: &mut Option<Box<dyn ClipboardBackend>>,
) {
    match mode {
        ClipboardMode::Off => {}
        ClipboardMode::On => {
            if let Some(cb) = prebuilt.take() {
                host.enable_clipboard(cb);
            }
        }
        ClipboardMode::Scripted => {
            host.enable_clipboard(Box::new(ScriptedClipboard::with_text(
                "rabbitcontrol-clipboard-host",
            )));
        }
    }
}

/// 打印 host 完成 stderr 摘要并生成 JSON 报告（无 gate 段）。
fn format_and_log_summary(
    args: &HostArgs,
    host: &HostSession,
    summary: &HostRunSummary,
    backend: &str,
    listen: SocketAddr,
    duration: Duration,
    connected_ms: u128,
) -> String {
    eprintln!(
        "host 完成：{}（{} 数据报 / 探测块 {} / {} 帧编码 / {} 事件注入 / {} 关键批 / IdrRequest×{} / in_flight 峰值 {} / 未结帧 {} / 连接 {connected_ms}ms / 剪贴板 发 {} 收 {} / 调度器：采集 {} → 编码 {}，弃帧 超期 {} + 重复 {} + 预算 {}）",
        match &summary.stop_reason {
            rc_session::StopReason::Requested => "到时收尾".to_string(),
            rc_session::StopReason::ByeReceived(_) => "对端 Bye 优雅收尾".to_string(),
            rc_session::StopReason::Disconnected(r) => format!("断线收尾（{r}）"),
        },
        summary.datagrams_sent,
        summary.probe_datagrams_sent,
        summary.frames_encoded,
        summary.events_injected,
        host.key_batches(),
        host.idr_requests_received(),
        host.in_flight_peak(),
        summary.unfinished_traces,
        summary.clipboard.sent,
        summary.clipboard.received,
        summary.captured,
        summary.frames_encoded,
        summary.dropped_stale,
        summary.dropped_repeat,
        summary.dropped_budget,
    );

    format!(
        concat!(
            "{{\"config\":{{\"resolution\":\"{w}x{h}\",\"fps\":{fps},\"duration_s\":{dur},",
            "\"backend\":\"{be}\",\"listen\":\"{listen}\",\"inject_mode\":\"{im}\"}},",
            "\"session\":{{\"session_id\":{sid},\"in_flight_peak\":{ifp},",
            "\"injected_events\":{iev},\"key_batches\":{kb},",
            "\"idr_requests_received\":{idrr},\"connected_ms\":{cms},",
            "\"captured\":{cap},\"dropped_stale\":{dst},",
            "\"dropped_repeat\":{drep},\"dropped_budget\":{dbud},",
            "\"probe_datagrams_sent\":{pds},\"clipboard\":{clip}}}}}"
        ),
        w = args.resolution.width,
        h = args.resolution.height,
        fps = args.fps,
        dur = duration.as_secs_f64(),
        be = escape(backend),
        listen = escape(&listen.to_string()),
        im = inject_label(args.inject_mode),
        sid = host.session_id().unwrap_or(0),
        ifp = host.in_flight_peak(),
        iev = summary.events_injected,
        kb = host.key_batches(),
        idrr = host.idr_requests_received(),
        cms = connected_ms,
        cap = summary.captured,
        dst = summary.dropped_stale,
        drep = summary.dropped_repeat,
        dbud = summary.dropped_budget,
        pds = summary.probe_datagrams_sent,
        clip = summary.clipboard.to_json(),
    )
}

/// H7：托盘模式显式会话驱动循环——`Streaming` 调 `tick`、`Detached` 调 `tick_detached`；
/// 在 `DetachOutcome::TornDown`、退出/断开信号、时长到期或会话结束时立即跳出并收尾，
/// 让外层托盘循环回到 `WaitingForConnection` 继续接收后续客户端。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn drive_tray_session(
    host: &mut HostSession,
    clock: &SessionClock,
    mut should_stop: impl FnMut() -> bool,
    mut on_peer_changed: impl FnMut(SocketAddr),
) -> rc_types::Result<HostRunSummary> {
    while !should_stop() && host.state() != rc_session::SessionState::Closed {
        match host.state() {
            rc_session::SessionState::Streaming => {
                let r = host.tick(Duration::from_millis(1))?;
                if r.closed.is_some() {
                    break;
                }
                if !r.busy && host.state() == rc_session::SessionState::Streaming {
                    host.pipeline().wait_event(Duration::from_millis(10));
                }
            }
            rc_session::SessionState::Detached => {
                let now_us = u64::from(clock());
                match host.tick_detached(now_us)? {
                    rc_session::DetachOutcome::TornDown => break,
                    rc_session::DetachOutcome::Resumed | rc_session::DetachOutcome::Replaced => {
                        on_peer_changed(host.peer_addr());
                    }
                    rc_session::DetachOutcome::Waiting => {}
                }
            }
            rc_session::SessionState::Init
            | rc_session::SessionState::Connected
            | rc_session::SessionState::Reconnecting
            | rc_session::SessionState::Closed => break,
        }
    }
    host.close(rc_session::proto::ByeReason::Normal)
}

/// `--tray` 常驻托盘模式（T22：仅 macOS；主线程跑 AppKit 事件循环，工作线程一次一条循环 accept）。
#[cfg(target_os = "macos")]
fn run_tray(args: &HostArgs, cert_pem: String, key_pem: String) -> CliResult<ExitCode> {
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut tray = rc_host_macos::TrayHost::new(rc_host_macos::TrayConfig {
        listen: args.listen,
    })
    .map_err(|e| format!("托盘初始化失败: {e}"))?;
    let tray_approver = tray.approver();
    let state_handle = tray.state_handle();
    let disconnect = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));

    let worker_args = args.clone();
    let worker_disconnect = Arc::clone(&disconnect);
    let worker_quit = Arc::clone(&quit);
    let worker = std::thread::spawn(move || {
        let mut listen_addr = worker_args.listen;
        while !worker_quit.load(Ordering::Acquire) {
            let approver: Box<dyn ConnectionApprover> =
                match worker_args.approval.unwrap_or(ApprovalMode::Prompt) {
                    ApprovalMode::Prompt => Box::new(tray_approver.clone()),
                    ApprovalMode::Auto => Box::new(AutoApprover),
                    ApprovalMode::Deny => Box::new(DenyApprover),
                };
            let BuiltHostSession {
                mut host,
                backend_label,
                mut prebuilt_clipboard,
                clock_origin,
                clock,
            } = match build_host_session(&worker_args, listen_addr, &cert_pem, &key_pem, approver) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("host 托盘会话构造失败：{e}");
                    break;
                }
            };
            if let Ok(actual) = host.local_addr() {
                listen_addr = actual;
            }
            let _activity = if matches!(worker_args.backend, super::BackendMode::Real) {
                rc_capture_macos::promote_current_thread_qos();
                Some(rc_capture_macos::LatencyCriticalActivity::begin(
                    "RabbitControl host tray",
                    true,
                ))
            } else {
                None
            };
            state_handle.set_state(rc_host_macos::TrayState::WaitingForConnection(listen_addr));
            eprintln!(
                "host 托盘等待连接：listen={listen_addr}（{}x{} @ {}fps，注入={}）",
                worker_args.resolution.width,
                worker_args.resolution.height,
                worker_args.fps,
                inject_label(worker_args.inject_mode),
            );

            let accepted = loop {
                if worker_quit.load(Ordering::Acquire) {
                    break false;
                }
                // M6：每轮 accept 分片前重锚 Pending 时钟，避免空等 >71.6min 后审批超时饱和。
                reanchor_pending_clock(&clock_origin);
                match host.accept_and_handshake(TRAY_ACCEPT_SLICE) {
                    Ok(_) => break true,
                    Err(rc_types::Error::Timeout(_))
                        if host.state() == rc_session::SessionState::Init =>
                    {
                        continue;
                    }
                    Err(e) => {
                        eprintln!("host 托盘 accept/握手结束：{e}");
                        break false;
                    }
                }
            };
            // 仅在未接受连接时直接丢弃；若已进入 Streaming 且此时收到退出信号，
            // 必须进入驱动循环让 close() 立即触发 U7 优雅停机（Bye(Normal) → 停采 → 排空 → 停编）。
            if !accepted {
                drop(host);
                continue;
            }

            worker_disconnect.store(false, Ordering::Release);
            state_handle.set_state(rc_host_macos::TrayState::InSession(host.peer_addr()));
            enable_host_clipboard(&mut host, worker_args.clipboard, &mut prebuilt_clipboard);

            let connected = Instant::now();
            let duration = worker_args.duration;
            let run_res = drive_tray_session(
                &mut host,
                &clock,
                || {
                    worker_quit.load(Ordering::Acquire)
                        || worker_disconnect.load(Ordering::Acquire)
                        || duration.is_some_and(|d| !d.is_zero() && connected.elapsed() >= d)
                },
                |peer| {
                    state_handle.set_state(rc_host_macos::TrayState::InSession(peer));
                },
            );
            let connected_ms = connected.elapsed().as_millis();
            match run_res {
                Ok(summary) => {
                    let backend = backend_label.lock().expect("label lock").clone();
                    let report = format_and_log_summary(
                        &worker_args,
                        &host,
                        &summary,
                        &backend,
                        listen_addr,
                        duration.unwrap_or(Duration::ZERO),
                        connected_ms,
                    );
                    if worker_args.report.is_some() {
                        if let Err(e) = write_report(&worker_args, &report) {
                            eprintln!("host 托盘报告写入失败：{e}");
                        }
                    }
                }
                Err(e) => eprintln!("host 托盘会话运行失败：{e}"),
            }
            worker_disconnect.store(false, Ordering::Release);
            state_handle.set_state(rc_host_macos::TrayState::WaitingForConnection(listen_addr));
            drop(host);
        }
    });

    let run_res = tray.run(|ev| match ev {
        rc_host_macos::TrayEvent::DisconnectRequested => {
            disconnect.store(true, Ordering::Release);
        }
        rc_host_macos::TrayEvent::QuitRequested => {
            quit.store(true, Ordering::Release);
        }
    });
    quit.store(true, Ordering::Release);
    let _ = worker.join();
    run_res.map_err(|e| format!("托盘事件循环异常: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// 非 macOS 平台拒绝 `--tray`（T22：不静默降级）。
#[cfg(not(target_os = "macos"))]
fn run_tray(_args: &HostArgs, _cert_pem: String, _key_pem: String) -> CliResult<ExitCode> {
    Err("--tray 仅 macOS 支持（其他平台请使用 headless 模式）".into())
}

/// fake 假栈 + 共享锚点（修复卡 C-4 单采样不变式）：一次采样锚点，
/// 既作 [`WallCapture`] 帧时间戳的零点、也返回给调用方锚定
/// `ClockOrigin::Fake`——会话钟与 capture_ts 两轴同零点（顺序红线同
/// real 路径：锚点先确定、栈内全程用它，不二次采样）。
fn fake_stack_anchored(
    pcfg: &PipelineConfig,
    static_content: bool,
) -> rc_types::Result<(rc_pipeline::HostPipeline, Instant)> {
    let anchor = Instant::now();
    Ok((fake_stack(pcfg, static_content, anchor)?, anchor))
}

/// fake 假栈（照 rc-loopback fake_stack 形态）：FakeBackend（静止/动态）
/// + FakeEncoder，带节流（假后端必须 pace，坑 4）。
///
/// `anchor` 是帧时间戳的零点（修复卡 C-4：由 [`fake_stack_anchored`]
/// 单采样传入，与会话钟共用——不要在本函数内再 `Instant::now()`）。
/// 输出尺寸随分辨率缩放，封顶单包帧（64×64 与 1080p 均单包；尺寸预算
/// 见函数体内的注释）。
fn fake_stack(
    pcfg: &PipelineConfig,
    static_content: bool,
    anchor: Instant,
) -> rc_types::Result<rc_pipeline::HostPipeline> {
    let content = if static_content {
        rc_capture::FakeContent::Static
    } else {
        rc_capture::FakeContent::Dynamic
    };
    let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
        display: rc_capture::DisplayInfo {
            id: rc_types::DisplayId(0),
            resolution: pcfg.resolution,
            scale: 1.0,
            hdr: false,
        },
        fps: pcfg.fps,
        content,
        dirty_rect: None,
    })?;
    let pixels = pcfg.resolution.width as usize * pcfg.resolution.height as usize;
    // 单包帧上限：媒体数据报 = 32B 包头 + 载荷 ≤ rc-net 预算
    // （mtu 1200 − QUIC_OVERHEAD 48 = 1152）→ 载荷 ≤ 1120，留余量取 1088。
    // 1080p fake 帧刻意压成单包：rc-proto 的 MAX_PAYLOAD=1200 满尺寸包
    // （1232B > 1152）会被 rc-net send_datagram 拒收——多包帧的预算修正
    // 属 rc-proto/rc-pipeline 的后续卡，本卡以单包 fake 帧实测。
    const SINGLE_PACKET_PAYLOAD: usize = 1_088;
    let output_bytes = (pixels / 8).clamp(128, SINGLE_PACKET_PAYLOAD);
    let encoder = rc_encode::FakeEncoder::with_tuning(
        rc_encode::EncoderConfig {
            resolution: pcfg.resolution,
            fps: pcfg.fps,
            bitrate_bps: pcfg.bitrate_bps,
            ..rc_encode::EncoderConfig::default()
        },
        FAKE_ENCODE_COST_US,
        output_bytes,
    )?;
    rc_pipeline::HostPipeline::with_backends_paced(
        pcfg.clone(),
        Box::new(WallCapture {
            inner: Box::new(capture),
            anchor,
        }),
        Box::new(encoder),
        true,
    )
}

/// 假采集的墙钟对齐包装（定位对齐 rc-loopback 的 `WallEncoder`：测量
/// 基础设施在 IO 边界读墙钟，真实睡眠模拟硬件）。
///
/// 为什么必须包：FakeBackend 的 Dynamic 时间戳是**按调用次数推进**的
/// 逻辑网格（`next_content_ts += interval`），节流包装让网格贴着墙钟走；
/// 但 T17 的 Detached 暂停会背压阻塞采集线程，暂停期间网格停摆、墙钟
/// 照走——恢复后两者出现不可恢复的缺口，每帧的 `capture_ts + 22ms`
/// deadline 都在过去，调度器把此后**所有**帧判 Stale 丢弃（重连演练
/// 实测复现：resume 后 8 秒零编码、IdrRequest 重发 68 次）。把时间戳
/// 改写为「锚定构造时刻的墙钟读数」后，deadline 永远新鲜，语义也诚实
/// （合成内容确实是在该时刻生成的）。
struct WallCapture {
    inner: Box<dyn rc_capture::CaptureBackend>,
    anchor: Instant,
}

impl rc_capture::CaptureBackend for WallCapture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> rc_types::Result<Option<rc_capture::CapturedFrame>> {
        match self.inner.next_frame(timeout)? {
            Some(mut frame) => {
                frame.timestamp = rc_types::Timestamp(
                    u32::try_from(self.anchor.elapsed().as_micros()).unwrap_or(u32::MAX),
                );
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }
    fn poll_invalidation(&mut self) -> Option<rc_capture::InvalidationReason> {
        self.inner.poll_invalidation()
    }
    fn rebuild(&mut self) -> rc_types::Result<()> {
        self.inner.rebuild()
    }
    fn display_info(&self) -> rc_capture::DisplayInfo {
        self.inner.display_info()
    }
}

/// 真后端 host 半链：SCK + VideoToolbox（任一降级报错）。返回管线、
/// 标签与主机钟原点（µs；调用方锚进会话钟，保证 SCK PTS 同域——照
/// run_real 的 session_origin_us 模式）。
///
/// 顺序红线：必须**先**采样原点写进 `clock_origin_us`，**再**用该配置
/// 调 `select_capture_labeled`——SckConfig.origin_us 取自它，SCK 帧时间
/// 戳 = pts − origin，与会话钟（host_clock_us() − origin）同域同零点
///（照 rc-loopback run_real 的顺序）。2026-11-21 localhost 真后端实测
/// 教训：顺序颠倒（先建 SCK 流、后设零点）时 SCK 以首帧 PTS 为零点，
/// 首帧晚到 Δ（流启动 + 等屏幕变化）后每帧 capture_ts 都比会话钟旧
/// Δ，rc-sched 的 deadline = capture_ts + target_latency(22ms) 恒已过
/// → 每帧即到即 Drop(Stale)，30s 会话 0 帧编码、in_flight_peak=0、
/// IdrRequest 全饿死（同 app 内 SCK 直测与 M1 loopback 均正常，唯
/// host 双进程路径中招——差的就是这一处顺序）。
fn real_host_stack(
    hw_cfg: &PipelineConfig,
) -> rc_types::Result<(rc_pipeline::HostPipeline, String, u64)> {
    // 编码器先行自检（VT E6），再采样原点，最后启动 SCK 采集流。
    let (encoder, el) = rc_pipeline::select_encoder_labeled(hw_cfg)?;
    let origin_us = rc_pipeline::host_clock_us()
        .ok_or_else(|| rc_types::Error::InvalidConfig("平台无主机钟（real 仅 macOS）".into()))?;
    // 先锚零点再建 SCK 流（顺序红线见函数文档：SCK 与会话钟必须同零点）。
    let mut pcfg = hw_cfg.clone();
    pcfg.clock_origin_us = Some(origin_us);
    let (capture, cl) = rc_pipeline::select_capture_labeled(&pcfg)?;
    if el != "videotoolbox" || cl != "sck" {
        return Err(rc_types::Error::InvalidConfig(format!(
            "real 后端要求 sck+videotoolbox，实际 {cl}+{el}"
        )));
    }
    let pipeline = rc_pipeline::HostPipeline::with_backends_paced(
        pcfg, capture, encoder, false, // 真采集不节流
    )?;
    Ok((pipeline, format!("{cl}+{el}"), origin_us))
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

/// 注入模式的报告标签。
fn inject_label(mode: InjectMode) -> &'static str {
    match mode {
        InjectMode::Record => "record",
        InjectMode::Real => "real",
    }
}

/// 报告输出：`--report <path>` 写文件，`--report -` 或省略写 stdout（S1）。
fn write_report(args: &HostArgs, json: &str) -> CliResult<()> {
    match args.report.as_deref() {
        Some(path) if path != Path::new("-") => fs::write(path, format!("{json}\n"))
            .map_err(|e| format!("无法写报告文件 {}: {e}", path.display())),
        _ => {
            println!("{json}");
            Ok(())
        }
    }
}

/// 最小 JSON 字符串转义（对齐 rc-loopback report.rs）。
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

/// 开发证书 + 非环回监听的警告文案（修复卡D-3）；`None` = 无需警告。
///
/// 拆成纯函数锁定判定口径，供单测断言：仅当「`--cert` / `--key` 未成对
/// 提供（仍在用内嵌开发证书——只给一半时另一半也回退开发证书，私钥
/// 同样公开）」且「监听地址非环回」时警告。
fn dev_cert_warning(
    listen: &std::net::SocketAddr,
    cert: Option<&Path>,
    key: Option<&Path>,
) -> Option<String> {
    let using_dev_cert = !(cert.is_some() && key.is_some());
    (using_dev_cert && !listen.ip().is_loopback()).then(|| {
        format!(
            "⚠️  警告：--listen {listen} 非环回地址，且未成对提供 --cert/--key：\
             当前使用内嵌开发证书，其私钥随源码公开，不构成安全边界，\
             任何拿到该证书的人都能解密或冒充本会话。生产环境请提供自有证书。"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_host_session, dev_cert_warning, drive_tray_session, fake_stack, make_session_clock,
        reanchor_pending_clock, ApprovalMode, BackendMode, ClipboardMode, ClockOrigin, HostArgs,
        InjectMode, WallCapture,
    };
    use rc_capture::CaptureBackend as _;
    use rc_pipeline::host::datagram_header;
    use rc_pipeline::{DiffMode, PipelineConfig};
    use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};
    use rc_session::proto::{ByeReason, SessionParams};
    use rc_session::testkit::ScriptedCapture;
    use rc_session::{AutoApprover, ClientSession, ClientSessionConfig, StopReason};
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("合法 SocketAddr")
    }

    /// 永不表决的审批器（用于回归测试 `--approval-timeout` 超时拒连路径）。
    struct NeverApprover;

    impl ConnectionApprover for NeverApprover {
        fn submit(&mut self, _req: &ApprovalRequest) {}
        fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
            None
        }
        fn cancel(&mut self) {}
    }

    fn tiny_host_args(approval_timeout: Duration) -> HostArgs {
        HostArgs {
            listen: addr("127.0.0.1:0"),
            backend: BackendMode::Fake,
            resolution: rc_types::Resolution::new(64, 64),
            fps: 60,
            bitrate_bps: 8_000_000,
            duration: Some(Duration::from_secs(5)),
            static_content: true,
            inject_mode: InjectMode::Record,
            cert: None,
            key: None,
            password: None,
            clipboard: ClipboardMode::Off,
            tray: true,
            approval: Some(ApprovalMode::Prompt),
            approval_timeout,
            report: None,
        }
    }

    fn build_test_client(peer_addr: SocketAddr) -> ClientSession {
        let pcfg = tiny_pcfg();
        let cfg = ClientSessionConfig {
            peer_addr,
            mtu: rc_session::DEFAULT_SESSION_MTU,
            requested: SessionParams {
                width: pcfg.resolution.width,
                height: pcfg.resolution.height,
                fps: pcfg.fps,
                bitrate_bps: pcfg.bitrate_bps,
            },
            session_id: 99,
            pipeline: pcfg,
            connect_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_secs(5),
            capture: Box::new(ScriptedCapture::from_timeline(Vec::new()).expect("空时间线")),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        };
        let t0 = Instant::now();
        let clock: rc_session::SessionClock =
            Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX));
        ClientSession::new(
            cfg,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_KEY_PEM,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_SERVER_NAME,
            clock,
        )
        .expect("测试 ClientSession 构造")
    }

    /// fake 栈的最小配置（64×64 @ 60fps 单包帧；字段照 run() 的模板）。
    fn tiny_pcfg() -> PipelineConfig {
        PipelineConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 60,
            bitrate_bps: 8_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 64,
            diff: DiffMode::TrustPlatform,
            clock_origin_us: None,
            max_packet_payload: rc_pipeline::MAX_PAYLOAD,
        }
    }

    /// 修复卡D-3：内嵌开发证书（--cert/--key 未成对提供）+ 非环回监听
    /// 必须给出醒目警告（点明「不构成安全边界」）；环回地址（v4/v6）
    /// 或成对提供了自有证书时不警告；只提供一半仍警告（另一半回退
    /// 开发证书）。
    #[test]
    fn dev_cert_warning_only_for_non_loopback_without_own_cert() {
        // 环回（v4 / v6）+ 开发证书：本机调试场景，不警告。
        for a in ["127.0.0.1:9901", "127.0.0.1:0", "[::1]:9901"] {
            assert!(
                dev_cert_warning(&addr(a), None, None).is_none(),
                "环回 {a} 不应警告"
            );
        }
        // 非环回（任意地址 / 局域网 / v6 任意）+ 开发证书：必须警告。
        for a in ["0.0.0.0:9901", "192.168.1.10:9901", "[::]:9901"] {
            let msg = dev_cert_warning(&addr(a), None, None)
                .unwrap_or_else(|| panic!("非环回 {a} 必须警告"));
            assert!(msg.contains("不构成安全边界"), "文案要点明风险：{msg}");
            assert!(msg.contains(a), "文案应带上监听地址 {a}：{msg}");
        }
        // 成对提供了 --cert/--key：自有证书，不警告。
        let p = Path::new("/tmp/rc-cert.pem");
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), Some(p), Some(p)).is_none());
        // 只提供一半（另一半回退开发证书）：仍警告。
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), Some(p), None).is_some());
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), None, Some(p)).is_some());
    }

    /// 修复卡 C-4：WallCapture 的帧时间戳严格跟随**传入锚点**的轴
    ///（next_frame 调用前后夹逼，误差 < 1ms 级；注入假构造验证换轴
    /// 行为）。锚点共享的另一侧（ClockOrigin::Fake）由 fake_stack_anchored
    /// 的单变量传递保证——同一次采样既进本包装也返回给调用方。
    #[test]
    fn wall_capture_timestamps_follow_shared_anchor() {
        let inner = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
            display: rc_capture::DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: rc_types::Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 60,
            content: rc_capture::FakeContent::Dynamic,
            dirty_rect: None,
        })
        .expect("假采集后端构造");
        let anchor = Instant::now();
        let mut cap = WallCapture {
            inner: Box::new(inner),
            anchor,
        };
        let lo = anchor.elapsed().as_micros() as u64;
        let frame = cap
            .next_frame(Duration::from_millis(100))
            .expect("采集不失败")
            .expect("动态内容立即出帧");
        let hi = anchor.elapsed().as_micros() as u64;
        let ts = u64::from(frame.timestamp.0);
        assert!(
            ts + 1_000 >= lo && ts <= hi + 1_000,
            "帧时间戳 {ts}µs 应落在传入锚点轴的 [{}, {}]（±1ms）内",
            lo.saturating_sub(1_000),
            hi + 1_000
        );
    }

    /// 修复卡 C-4：fake_stack 必须用**传入的**锚点换算帧时间戳，不得在
    /// 栈内二次采样 `Instant::now()`——传入 3 秒前的旧锚点，首帧数据报
    /// 头的 capture_ts 必须落在旧锚点的轴上（≈3s；二次采样会得到 ≈0）。
    #[test]
    fn fake_stack_capture_axis_uses_passed_anchor() {
        let anchor = Instant::now() - Duration::from_secs(3);
        let mut stack = fake_stack(&tiny_pcfg(), false, anchor).expect("fake 栈构造");
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let mut capture_ts = None;
        while Instant::now() < deadline {
            stack.wait_event(Duration::from_millis(50));
            if let Some(datagrams) = stack.tick(0).expect("tick") {
                if let Some(first) = datagrams.first() {
                    let h = datagram_header(first).expect("帧头解析");
                    capture_ts = Some(h.capture_ts.0);
                    break;
                }
            }
        }
        stack.stop_capture();
        let ts = u64::from(capture_ts.expect("2s 内应产出首帧数据报"));
        assert!(
            (2_500_000..=3_600_000).contains(&ts),
            "capture_ts {ts}µs 应落在传入旧锚点的轴上（≈3s）——栈内二次采样会得到 ≈0"
        );
    }

    /// T22 评审回归 1a：握手/审批阶段（`ClockOrigin::Pending`）会话钟必须持续推进，
    /// 不得冻结在 0；管线工厂重锚为 `Fake` 后读数归零重算。
    #[test]
    fn pending_session_clock_advances_and_reanchors() {
        let origin = Arc::new(Mutex::new(ClockOrigin::Pending(
            Instant::now() - Duration::from_millis(60),
        )));
        let clock = make_session_clock(Arc::clone(&origin));
        let t1 = clock();
        assert!(
            t1 >= 50_000,
            "Pending 阶段会话钟应随墙钟推进（实测 {t1}µs）"
        );
        std::thread::sleep(Duration::from_millis(15));
        let t2 = clock();
        assert!(
            t2 > t1,
            "Pending 阶段会话钟应单调递增（t1={t1}µs, t2={t2}µs）"
        );

        // 模拟管线工厂重锚到新 Instant。
        *origin.lock().expect("origin lock") = ClockOrigin::Fake(Instant::now());
        let t3 = clock();
        assert!(
            t3 < t2,
            "管线工厂重锚后会话钟应从新锚点起算（t3={t3}µs, t2={t2}µs）"
        );
    }

    /// T22 评审回归 1b：`build_host_session` 在审批超时（250ms < handshake_timeout 10s）
    /// 时必须经会话钟触发 `Welcome{Rejected}`（而非卡满 10s 后落入 Timeout 兜底）。
    #[test]
    fn build_host_session_approval_timeout_sends_welcome_rejected() {
        let args = tiny_host_args(Duration::from_millis(250));
        let mut built = build_host_session(
            &args,
            args.listen,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_KEY_PEM,
            Box::new(NeverApprover),
        )
        .expect("host 会话构造");
        let listen = built.host.local_addr().expect("local_addr");

        let client_handle = std::thread::spawn(move || {
            let mut client = build_test_client(listen);
            client.connect_and_handshake()
        });

        let started = Instant::now();
        let host_res = built.host.accept_and_handshake(Duration::from_secs(5));
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "250ms 审批超时应在远小于 10s 握手超时内结束（实测 {elapsed:?}）"
        );
        match host_res {
            Err(rc_types::Error::Protocol { msg, .. }) => {
                assert!(
                    msg.contains("authentication rejected"),
                    "应返回审批拒绝错误（含 Welcome{{Rejected}}），实际：{msg}"
                );
            }
            other => panic!("审批超时应触发 Welcome{{Rejected}} 协议拒绝，实际：{other:?}"),
        }

        let client_res = client_handle.join().expect("client 线程不 panic");
        assert!(client_res.is_err(), "client 应收到审批拒绝结果而非握手成功");
    }

    /// T22 评审回归 3：握手刚成功进入 `Streaming` 时若退出信号已置位，
    /// 进入 `host.run(&mut || true)` 必须立即触发 U7 优雅停机并向 client 发送 `Bye(Normal)`。
    #[test]
    fn accepted_session_with_immediate_stop_sends_bye_normal() {
        let args = tiny_host_args(Duration::from_secs(5));
        let mut built = build_host_session(
            &args,
            args.listen,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_KEY_PEM,
            Box::new(AutoApprover),
        )
        .expect("host 会话构造");
        let listen = built.host.local_addr().expect("local_addr");

        let (client_ready_tx, client_ready_rx) = std::sync::mpsc::channel();
        let client_handle = std::thread::spawn(move || {
            let mut client = build_test_client(listen);
            client.connect_and_handshake().expect("client 握手应成功");
            let _ = client_ready_tx.send(());
            let deadline = Instant::now() + Duration::from_secs(3);
            client
                .run(&mut || Instant::now() >= deadline)
                .expect("client run 应正常收尾")
        });

        built
            .host
            .accept_and_handshake(Duration::from_secs(5))
            .expect("host 握手应成功");
        client_ready_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("client 应完成握手进入 Streaming");

        // 模拟 accepted == true 且 worker_quit 同时变为 true：进入 host.run 立即触发 stop()。
        let host_summary = built
            .host
            .run(&mut || true)
            .expect("host 立即停机应返回 Ok");
        assert_eq!(host_summary.stop_reason, StopReason::Requested);
        assert_eq!(built.host.state(), rc_session::SessionState::Closed);

        let client_summary = client_handle.join().expect("client 线程不 panic");
        assert_eq!(
            client_summary.stop_reason,
            StopReason::ByeReceived(ByeReason::Normal),
            "client 应收到 U7 优雅停机发出的 Bye(Normal)"
        );
    }

    /// M6 回归：托盘空等超过 71.6 分钟（`u32::MAX` µs）时 `Pending` 会话钟会饱和到 `u32::MAX`；
    /// 每轮 accept 分片前调用 `reanchor_pending_clock` 必须将其重锚回 ≈0，且不覆盖已锚定的 `Fake`/`HostClock`。
    #[test]
    fn reanchor_pending_clock_clears_saturated_idle_clock() {
        let origin = Arc::new(Mutex::new(ClockOrigin::Pending(
            Instant::now() - Duration::from_secs(5_000),
        )));
        let clock = make_session_clock(Arc::clone(&origin));
        assert_eq!(
            clock(),
            u32::MAX,
            "空等 5000s (>71.6min) 的 Pending 时钟应饱和到 u32::MAX"
        );

        reanchor_pending_clock(&origin);
        let after = clock();
        assert!(
            after < 100_000,
            "reanchor_pending_clock 后时钟应回到 ≈0（实测 {after}µs）"
        );

        // 已进入 Fake 态时重锚为空操作，不得破坏帧时间戳同轴零点。
        let old_fake = Instant::now() - Duration::from_secs(2);
        *origin.lock().expect("origin lock") = ClockOrigin::Fake(old_fake);
        reanchor_pending_clock(&origin);
        assert!(
            clock() >= 1_500_000,
            "Fake 态不应被 reanchor_pending_clock 重置"
        );
    }

    /// H7 回归：客户端非优雅断开进入 `Detached` 且保持窗口到期（`DetachOutcome::TornDown`）时，
    /// `drive_tray_session` 必须立刻退出并收尾到 `Closed`（而非像 `host.run` 那样无限滞留在 Detached），
    /// 释放监听地址后外层托盘循环可顺利接收下一位客户端。
    #[test]
    fn drive_tray_session_breaks_on_torn_down_and_accepts_next_client() {
        let args = tiny_host_args(Duration::from_secs(5));
        let mut built1 = build_host_session(
            &args,
            args.listen,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_KEY_PEM,
            Box::new(AutoApprover),
        )
        .expect("首轮 host 会话构造");
        built1
            .host
            .set_reconnect(rc_session::ReconnectConfig {
                resume_ttl: Duration::from_millis(80),
                ..rc_session::ReconnectConfig::default()
            })
            .expect("设置短 resume_ttl");
        let listen = built1.host.local_addr().expect("local_addr");

        let client1 = std::thread::spawn(move || {
            let mut c = build_test_client(listen);
            c.connect_and_handshake().expect("client1 握手应成功");
            // 不发 Bye 直接丢弃连接，模拟客户端异常断线。
            drop(c);
        });

        built1
            .host
            .accept_and_handshake(Duration::from_secs(5))
            .expect("host1 握手应成功");
        client1.join().expect("client1 线程不 panic");

        // 触发 Detached，验证 drive_tray_session 在 resume_ttl 过期后因 TornDown 立刻返回。
        built1.host.on_disconnected(u64::from((built1.clock)()));
        assert_eq!(built1.host.state(), rc_session::SessionState::Detached);

        let started = Instant::now();
        let summary1 = drive_tray_session(&mut built1.host, &built1.clock, || false, |_| {})
            .expect("drive_tray_session 应在 TornDown 后正常收尾");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "TornDown 后应迅速退出（实测 {:?}）",
            started.elapsed()
        );
        assert_eq!(built1.host.state(), rc_session::SessionState::Closed);
        assert_eq!(summary1.stop_reason, StopReason::Requested);
        drop(built1);

        // 模拟外层托盘循环重新构造会话并接受下一位客户端（使用临时端口避免 OS 端口释放抖动）。
        let mut built2 = build_host_session(
            &args,
            args.listen,
            rc_session::dev_cert::DEV_CERT_PEM,
            rc_session::dev_cert::DEV_KEY_PEM,
            Box::new(AutoApprover),
        )
        .expect("次轮 host 会话构造");
        let listen2 = built2.host.local_addr().expect("local_addr2");
        let client2 = std::thread::spawn(move || {
            let mut c = build_test_client(listen2);
            c.connect_and_handshake().expect("client2 握手应成功");
            c.close(ByeReason::Normal).expect("client2 优雅关闭");
        });
        built2
            .host
            .accept_and_handshake(Duration::from_secs(5))
            .expect("host2 应正常接收下一位客户端");
        let summary2 = drive_tray_session(&mut built2.host, &built2.clock, || false, |_| {})
            .expect("host2 收到 Bye 后正常退出");
        assert_eq!(
            summary2.stop_reason,
            StopReason::ByeReceived(ByeReason::Normal)
        );
        client2.join().expect("client2 线程不 panic");
    }
}
