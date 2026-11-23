//! 进程内双端（真 QUIC over 127.0.0.1）会话集成测试（T18）。
//!
//! 与 `quic_loopback.rs` 的差别：那边只验证「握手 → 流 → 关闭」的连通
//! 账目；这里跑**完整会话形态**（假后端 64×64@60fps、ScriptedCapture
//! 脚本输入、RecordingInjector 记录注入、探测泵每拍驱动），断言
//! [`rc_session::report::SessionReport`] 的 Gate 通过与输入链路的
//! InputAck 采样。免权限、CI 可跑（S7）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_net::quic::{Identity, QuicEndpoint};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_session::auth::{
    ApprovalVerdict, ClientAuthConfig, ConnectionApprover, HostAuthConfig, DEFAULT_AUTH_TIMEOUT_US,
    DEFAULT_CLIENT_AUTH_TIMEOUT_US,
};
use rc_session::proto::{AuthMethod, AuthStatus, ByeReason, ControlMessage, Hello, SessionParams};
use rc_session::report::{SessionConfigSnapshot, SessionReport};
use rc_session::testkit::{MockApprover, RecordingInjector, ScriptedCapture};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
};
use rc_types::{Error, Resolution};

/// localhost 测试用内嵌开发证书（与 rc-net tests/certs 同一对材料）。
const SERVER_CERT_PEM: &str = include_str!("certs/server-cert.pem");
const SERVER_KEY_PEM: &str = include_str!("certs/server-key.pem");

/// 握手 / 收帧的宽松上限：本地回环上真实耗时都在毫秒级，只防挂死。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 流参数（假后端能力：64×64@60、8Mbps）。
fn params() -> SessionParams {
    SessionParams {
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
    }
}

fn pcfg() -> rc_pipeline::PipelineConfig {
    rc_pipeline::PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: rc_pipeline::DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；真 QUIC 路径由 host 侧 expect_hello 按 N2 覆盖。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

/// 会话零点槽（照 run_real 的 origin_slot 模式；host 侧由管线工厂在
/// 构造假后端的那一刻锚定——FakeBackend 的时间网格与墙钟同零点）。
type OriginSlot = Arc<Mutex<Option<Instant>>>;

fn origin_clock(origin: OriginSlot) -> SessionClock {
    Arc::new(move || match *origin.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    })
}

/// host 会话（真网络端点 + 假后端工厂 + 记录注入器）。
/// 返回会话与注入器克隆（断言已注入事件用）。
fn host_session(injector: RecordingInjector) -> (HostSession, OriginSlot) {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));
    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: rc_session::DEFAULT_SESSION_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(injector),
            pace_capture: true,
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        clock,
    )
    .expect("host session");
    // 工厂：锚定会话零点于假后端构造同刻，再建假栈（带节流，坑 4）。
    let factory_origin = Arc::clone(&origin);
    host.set_pipeline_factory_for(Box::new(move |pcfg| {
        *factory_origin.lock().expect("origin lock") = Some(Instant::now());
        let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
            display: rc_capture::DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: pcfg.resolution,
                scale: 1.0,
                hdr: false,
            },
            fps: pcfg.fps,
            content: rc_capture::FakeContent::Dynamic,
            dirty_rect: None,
        })?;
        let encoder = rc_encode::FakeEncoder::with_tuning(
            rc_encode::EncoderConfig {
                resolution: pcfg.resolution,
                fps: pcfg.fps,
                bitrate_bps: pcfg.bitrate_bps,
                ..rc_encode::EncoderConfig::default()
            },
            500,
            512,
        )?;
        rc_pipeline::HostPipeline::with_backends_paced(
            pcfg.clone(),
            Box::new(capture),
            Box::new(encoder),
            true,
        )
    }));
    (host, origin)
}

/// client 会话（真网络端点 + 假解码/呈现工厂 + 脚本输入）。
/// 返回会话与其会话钟句柄（驱动循环的探测泵/重连拍要读同一时钟）。
fn client_session(host_addr: std::net::SocketAddr) -> (ClientSession, SessionClock) {
    let t0 = Instant::now();
    let clock: SessionClock =
        Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX));
    let clock_handle = Arc::clone(&clock);
    let mut client = ClientSession::new(
        ClientSessionConfig {
            peer_addr: host_addr,
            mtu: rc_session::DEFAULT_SESSION_MTU,
            requested: params(),
            session_id: 424_242,
            pipeline: pcfg(),
            connect_timeout: CONNECT_TIMEOUT,
            handshake_timeout: Duration::from_secs(5),
            capture: Box::new(ScriptedCapture::standard()),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        SERVER_CERT_PEM,
        "rabbitcontrol",
        clock,
    )
    .expect("client session");
    client.set_pipeline_factory_for(Box::new(|pcfg| {
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
    }));
    (client, clock_handle)
}

/// 驱动 client 到 `duration` 或 Closed（照 ClientSession::run 手工展开：
/// 每拍 pump_probes + start_bw_probe，首帧计时，返回首帧微秒）。驱动器
/// 是合法时钟源（坑 5 同源豁免）。
fn drive_client(
    client: &mut ClientSession,
    clock: &SessionClock,
    duration: Duration,
) -> Option<u64> {
    let t0 = Instant::now();
    let mut first_frame = None;
    let mut last_busy = true;
    while client.state() != SessionState::Closed && t0.elapsed() < duration {
        match client.state() {
            SessionState::Streaming => {
                let now_us = u64::from((clock)());
                client.pump_probes(now_us).expect("pump_probes");
                client.start_bw_probe(now_us).expect("start_bw_probe");
                let budget = if last_busy {
                    Duration::from_millis(1)
                } else {
                    Duration::from_millis(10)
                };
                let r = client.tick(budget, Instant::now()).expect("client tick");
                if r.closed.is_some() {
                    break;
                }
                last_busy = r.busy;
                if first_frame.is_none() && client.pipeline().displayed() > 0 {
                    first_frame = Some(u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX));
                }
            }
            SessionState::Reconnecting => {
                let _ = client
                    .tick_reconnect(u64::from((clock)()))
                    .expect("tick_reconnect");
                thread::sleep(Duration::from_millis(5));
            }
            _ => break,
        }
    }
    first_frame
}

/// host 会话线程跑完整会话（accept → run 到 duration 兜底），返回摘要
/// 与注入器观测。
struct HostRun {
    summary: rc_session::HostRunSummary,
    in_flight_peak: u32,
}

#[allow(clippy::type_complexity)]
fn spawn_host(host: HostSession, duration: Duration) -> thread::JoinHandle<HostRun> {
    thread::spawn(move || {
        let mut host = host;
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("host accept_and_handshake");
        let connected = Instant::now();
        let summary = host
            .run(&mut || connected.elapsed() >= duration)
            .expect("host run");
        HostRun {
            summary,
            in_flight_peak: host.in_flight_peak(),
        }
    })
}

/// 完整 localhost 假后端会话：SessionReport Gate 通过（M2 出口门槛）。
#[test]
fn localhost_fake_session_meets_gate() {
    let injector = RecordingInjector::new();
    let (host, _origin) = host_session(injector);
    let host_addr = host.local_addr().expect("host local_addr");
    let host_thread = spawn_host(host, Duration::from_secs(3));

    let (mut client, clock) = client_session(host_addr);
    client.connect_and_handshake().expect("client handshake");
    let first_frame = drive_client(&mut client, &clock, Duration::from_millis(3_500));
    if client.state() != SessionState::Closed {
        client
            .close(rc_session::proto::ByeReason::Normal)
            .expect("client close");
    }
    let host_run = host_thread.join().expect("host thread");

    // 报告组装（铁律 4：观测全部来自会话记账）。
    let duration_us = 3_000_000u64;
    let obs = client.observations(duration_us, first_frame);
    let snapshot = SessionConfigSnapshot {
        resolution: Resolution::new(64, 64),
        fps: 60,
        duration_s: 3.0,
        backend: "fake".into(),
        connect_addr: host_addr,
        input_mode: "scripted",
    };
    let report = SessionReport::from_observations(&snapshot, &obs);
    assert!(
        report.gate().passed,
        "localhost 假会话应通过 Gate：{:?}\n{}",
        report.gate().failures,
        report.to_json()
    );
    assert!(
        report.connect_to_first_frame_ms().unwrap_or(f64::INFINITY) <= 800.0,
        "首帧 ≤800ms"
    );
    // 铁律 1：两侧队列深度峰值 ≤ 2。
    assert!(obs.max_reassembly_depth <= 2);
    assert!(host_run.in_flight_peak <= 2, "host in_flight 峰值 ≤ 2");
    // 账目闭合。
    assert_eq!(host_run.summary.unfinished_traces, 0);
    assert_eq!(client.pipeline().trace().summary().unfinished, 0);
}

/// client 发 scripted 输入 → host RecordingInjector 收到事件（归一化坐标
/// 原样），InputAck 样本数 >0。
#[test]
fn localhost_input_flows_to_injector() {
    let injector = RecordingInjector::new();
    let mut observer = injector.clone();
    let (host, _origin) = host_session(injector);
    let host_addr = host.local_addr().expect("host local_addr");
    let host_thread = spawn_host(host, Duration::from_millis(2_500));

    let (mut client, clock) = client_session(host_addr);
    client.connect_and_handshake().expect("client handshake");
    let _ = drive_client(&mut client, &clock, Duration::from_millis(2_200));
    if client.state() != SessionState::Closed {
        client
            .close(rc_session::proto::ByeReason::Normal)
            .expect("client close");
    }
    let _host_run = host_thread.join().expect("host thread");

    // 注入链路：归一化坐标原样到达（I4：绝对坐标不漂移）。
    let events = observer.take_events();
    assert!(
        events.iter().any(|ev| matches!(
            ev,
            rc_input::InputEvent::MouseMove {
                x: 32_768,
                y: 32_768
            }
        )),
        "应有 MouseMove(32768,32768) 原样注入，实际 {events:?}（截断展示前 8 条）",
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, rc_input::InputEvent::MouseButton { .. })),
        "应有 MouseButton 关键事件注入"
    );
    assert!(observer.key_batches() > 0, "关键批计数 >0");
    // InputAck 采样：探测偏移闭环后（~1s），后续关键批的 ack 都能换算出
    // 单向延迟样本。
    assert!(
        !client.input_one_way_samples_us().is_empty(),
        "InputAck 样本数应 >0（rtt={:?} 样本={:?}）",
        client.probe_snapshot().rtt_us_median,
        client.input_one_way_samples_us()
    );
}

// ─────────────────────────────────────────────────────────────────
// T22 审批集成测试（进程内双端、真 QUIC over 127.0.0.1:0、免权限）
// ─────────────────────────────────────────────────────────────────

/// 给假后端 `HostSession` 挂上与 `host_session()` 同款的假采集/编码管线工厂。
fn attach_fake_host_pipeline(host: &mut HostSession) {
    host.set_pipeline_factory_for(Box::new(|pcfg| {
        let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
            display: rc_capture::DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: pcfg.resolution,
                scale: 1.0,
                hdr: false,
            },
            fps: pcfg.fps,
            content: rc_capture::FakeContent::Dynamic,
            dirty_rect: None,
        })?;
        let encoder = rc_encode::FakeEncoder::with_tuning(
            rc_encode::EncoderConfig {
                resolution: pcfg.resolution,
                fps: pcfg.fps,
                bitrate_bps: pcfg.bitrate_bps,
                ..rc_encode::EncoderConfig::default()
            },
            500,
            512,
        )?;
        rc_pipeline::HostPipeline::with_backends_paced(
            pcfg.clone(),
            Box::new(capture),
            Box::new(encoder),
            true,
        )
    }));
}

/// 构造带自定义鉴权配置、审批人与注入时钟的真 QUIC host 会话（临时端口 `127.0.0.1:0`）。
fn host_session_with(
    auth: HostAuthConfig,
    approver: Box<dyn ConnectionApprover>,
    clock: SessionClock,
) -> HostSession {
    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: rc_session::DEFAULT_SESSION_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(RecordingInjector::new()),
            pace_capture: true,
            auth,
            approver,
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        clock,
    )
    .expect("host session");
    attach_fake_host_pipeline(&mut host);
    host
}

/// 构造带自定义 `session_id` 与鉴权配置的真 QUIC client 会话。
fn client_session_with(
    host_addr: SocketAddr,
    session_id: u64,
    auth: ClientAuthConfig,
) -> (ClientSession, SessionClock) {
    let t0 = Instant::now();
    let clock: SessionClock =
        Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX));
    let clock_handle = Arc::clone(&clock);
    let mut cfg = ClientSessionConfig::new(
        host_addr,
        params(),
        session_id,
        Box::new(ScriptedCapture::standard()),
    );
    cfg.mtu = rc_session::DEFAULT_SESSION_MTU;
    cfg.pipeline = pcfg();
    cfg.connect_timeout = CONNECT_TIMEOUT;
    cfg.auth = auth;
    let mut client = ClientSession::new(
        cfg,
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        SERVER_CERT_PEM,
        "rabbitcontrol",
        clock,
    )
    .expect("client session");
    client.set_pipeline_factory_for(Box::new(|pcfg| {
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
    }));
    (client, clock_handle)
}

/// 墙钟注入闭包（供不测精确注入时钟步进的审批用例用）。
fn wall_clock() -> SessionClock {
    let t0 = Instant::now();
    Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX))
}

/// 等待条件成立（短睡眠轮询，超时返回 `false`）。
fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    pred()
}

/// 构造原始 QUIC client 连接（用于精确控制断线时机或断言首条控制消息形态）。
fn raw_quic_client(host_addr: SocketAddr) -> (QuicEndpoint, rc_net::quic::QuicTransport) {
    let identity = Identity::from_pem(SERVER_CERT_PEM, SERVER_KEY_PEM).expect("identity");
    let certs = Identity::certs_from_pem(SERVER_CERT_PEM).expect("server certs");
    let net = NetConfig {
        role: Role::Controller,
        local_addr: "0.0.0.0:0".parse().unwrap(),
        peer_addr: Some(host_addr),
        mtu: rc_session::DEFAULT_SESSION_MTU,
    };
    let endpoint = QuicEndpoint::new(&net, &identity).expect("client endpoint");
    let transport = endpoint
        .connect(host_addr, &certs, "rabbitcontrol", CONNECT_TIMEOUT)
        .expect("raw quic connect");
    (endpoint, transport)
}

/// T22-1：`MockApprover` 收到的 `ApprovalRequest` 字段完整：传输层 `remote_addr`、
/// `requested` 与 Hello 一致、`auth_method` 一致（U9）。
#[test]
fn approval_request_fields_reach_approver() {
    let mut mock = MockApprover::new();
    mock.preset(ApprovalVerdict::Accepted);
    let mut observer = mock.clone();

    let host = host_session_with(HostAuthConfig::default(), Box::new(mock), wall_clock());
    let host_addr = host.local_addr().expect("host addr");
    let host_thread = spawn_host(host, Duration::from_secs(2));

    let sid = 0xCAFE_BABE_0001;
    let (mut client, _clock) = client_session_with(host_addr, sid, ClientAuthConfig::default());
    client.connect_and_handshake().expect("client handshake");
    client.close(ByeReason::Normal).expect("client close");
    let _host_run = host_thread.join().expect("host thread");

    assert_eq!(observer.submit_count(), 1, "应恰好提交一次审批请求");
    assert_eq!(observer.cancel_count(), 0, "正常接受不得触发 cancel");
    let req = observer
        .take_request()
        .expect("approver 必须收到 ApprovalRequest");
    assert_eq!(req.session_id, sid);
    assert!(
        req.remote_addr.ip().is_loopback() && req.remote_addr.port() != 0,
        "remote_addr 应为 client 真实传输层回环地址，实际 {}",
        req.remote_addr
    );
    assert_ne!(
        req.remote_addr.port(),
        host_addr.port(),
        "remote_addr 应为 client 端口而非 host 监听端口"
    );
    assert_eq!(req.requested, params());
    assert_eq!(req.auth_method, AuthMethod::None);
    assert!(!req.password_verified);
}

/// T22-2：预置 `Accepted` → client 收 `Welcome{Ok}`，进入 Streaming 并跑完短会话。
#[test]
fn approval_accept_sends_welcome_ok() {
    let mut mock = MockApprover::new();
    mock.preset(ApprovalVerdict::Accepted);
    let observer = mock.clone();

    let host = host_session_with(HostAuthConfig::default(), Box::new(mock), wall_clock());
    let host_addr = host.local_addr().expect("host addr");
    let host_thread = spawn_host(host, Duration::from_secs(2));

    let (mut client, clock) = client_session_with(host_addr, 20_002, ClientAuthConfig::default());
    let granted = client.connect_and_handshake().expect("client handshake");
    assert_eq!(granted, params());
    assert_eq!(client.state(), SessionState::Streaming);

    let first_frame = drive_client(&mut client, &clock, Duration::from_millis(800));
    assert!(first_frame.is_some(), "接受后应正常收到首帧");
    let displayed = client.pipeline().displayed();
    client.close(ByeReason::Normal).expect("client close");
    let host_run = host_thread.join().expect("host thread");

    assert!(displayed >= 3, "应上屏至少 3 帧，实际 {displayed}");
    assert!(
        host_run.summary.frames_encoded >= 1,
        "host 应编码至少 1 帧，实际 {}",
        host_run.summary.frames_encoded
    );
    assert_eq!(observer.submit_count(), 1);
}

/// T22-3：预置 `Rejected` → client 收 `Welcome{Rejected}` 后连接被 host 关闭，
/// client 以明确错误终止（U11）。
#[test]
fn approval_reject_sends_welcome_rejected_and_closes() {
    let mut mock = MockApprover::new();
    mock.preset(ApprovalVerdict::Rejected);
    let observer = mock.clone();

    let mut host = host_session_with(HostAuthConfig::default(), Box::new(mock), wall_clock());
    let host_addr = host.local_addr().expect("host addr");
    let host_thread = thread::spawn(move || {
        let res = host.accept_and_handshake(CONNECT_TIMEOUT);
        (res, host.state())
    });

    let (mut client, _clock) = client_session_with(host_addr, 20_003, ClientAuthConfig::default());
    let client_err = client
        .connect_and_handshake()
        .expect_err("审批拒绝时 client 必须返回明确错误");
    assert!(
        matches!(client_err, Error::Protocol { .. } | Error::Network(_)),
        "client 应因 Welcome{{Rejected}} 或 host 立即断连返回明确错误（非 Timeout），实际 {client_err:?}"
    );
    assert_eq!(client.state(), SessionState::Closed);
    assert!(client.negotiated().is_none());

    let (host_res, host_state) = host_thread.join().expect("host thread");
    assert!(
        matches!(host_res, Err(Error::Protocol { .. })),
        "host 应返回拒绝错误，实际 {host_res:?}"
    );
    assert_eq!(host_state, SessionState::Closed);
    assert_eq!(observer.submit_count(), 1);
}

/// T22-4：`MockApprover` 不应答：注入时钟停在 deadline 前 1µs → 仍无 Welcome；
/// 恰好越过 → Rejected 路径触发——证明超时判定只吃注入时钟（U2 / 铁律 4）。
#[test]
fn approval_timeout_rejects_by_injected_clock() {
    let start_us = 1_000_000u32;
    let timeout_us = 10_000_000u64;
    let clock_us = Arc::new(AtomicU32::new(start_us));
    let host_clock: SessionClock = {
        let c = Arc::clone(&clock_us);
        Arc::new(move || c.load(Ordering::SeqCst))
    };

    let mock = MockApprover::new();
    let observer = mock.clone();
    let mut host = host_session_with(
        HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: timeout_us,
            ..HostAuthConfig::default()
        },
        Box::new(mock),
        host_clock,
    );
    let host_addr = host.local_addr().expect("host addr");

    let host_done = Arc::new(AtomicBool::new(false));
    let host_done_flag = Arc::clone(&host_done);
    let host_thread = thread::spawn(move || {
        let res = host.accept_and_handshake(CONNECT_TIMEOUT);
        let st = host.state();
        host_done_flag.store(true, Ordering::SeqCst);
        (res, st)
    });

    let client_done = Arc::new(AtomicBool::new(false));
    let client_done_flag = Arc::clone(&client_done);
    let client_thread = thread::spawn(move || {
        let (mut client, _clock) =
            client_session_with(host_addr, 20_004, ClientAuthConfig::default());
        let res = client.connect_and_handshake();
        let st = client.state();
        client_done_flag.store(true, Ordering::SeqCst);
        (res, st)
    });

    assert!(
        wait_until(Duration::from_secs(5), || observer.submit_count() == 1),
        "host 应在收到 Hello 后立即 submit 审批请求"
    );

    // 注入时钟推进到 deadline 前 1µs，并等待多个 host poll 分片（50ms/片）。
    let deadline_us = start_us + timeout_us as u32;
    clock_us.store(deadline_us - 1, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(200));
    assert!(
        !host_done.load(Ordering::SeqCst),
        "deadline 前 1µs host 不得触发超时"
    );
    assert!(
        !client_done.load(Ordering::SeqCst),
        "deadline 前 1µs client 仍应等待终局 Welcome"
    );
    assert_eq!(observer.cancel_count(), 0);

    // 恰好越过 deadline → 立即触发 Rejected + cancel。
    clock_us.store(deadline_us, Ordering::SeqCst);

    let (host_res, host_state) = host_thread.join().expect("host thread");
    let (client_res, client_state) = client_thread.join().expect("client thread");
    assert!(
        matches!(host_res, Err(Error::Protocol { .. })),
        "越过 deadline 后 host 应走 Rejected，实际 {host_res:?}"
    );
    assert_eq!(host_state, SessionState::Closed);
    assert!(
        matches!(client_res, Err(Error::Protocol { .. } | Error::Network(_))),
        "越过 deadline 后 client 应收到 Welcome{{Rejected}} 或 host 断连，实际 {client_res:?}"
    );
    assert_eq!(client_state, SessionState::Closed);
    assert_eq!(observer.cancel_count(), 1, "超时应撤销在途审批恰好一次");
}

/// T22-5：审批超时可配置：timeout=2s 时注入时钟推进 2s 走 Rejected；
/// 30s 配置下推进 2s 不触发。
#[test]
fn approval_timeout_is_configurable() {
    // 子场景 A：timeout = 2s → 推进 2s 触发 Rejected。
    {
        let clock_us = Arc::new(AtomicU32::new(0));
        let host_clock: SessionClock = {
            let c = Arc::clone(&clock_us);
            Arc::new(move || c.load(Ordering::SeqCst))
        };
        let mock = MockApprover::new();
        let observer = mock.clone();
        let mut host = host_session_with(
            HostAuthConfig {
                password: None,
                iterations: 64,
                approval_timeout_us: 2_000_000,
                ..HostAuthConfig::default()
            },
            Box::new(mock),
            host_clock,
        );
        let host_addr = host.local_addr().expect("host addr");
        let host_thread = thread::spawn(move || {
            let res = host.accept_and_handshake(CONNECT_TIMEOUT);
            (res, host.state())
        });
        let client_thread = thread::spawn(move || {
            let (mut client, _clock) =
                client_session_with(host_addr, 20_005, ClientAuthConfig::default());
            let res = client.connect_and_handshake();
            (res, client.state())
        });

        assert!(wait_until(Duration::from_secs(5), || {
            observer.submit_count() == 1
        }));
        clock_us.store(2_000_000, Ordering::SeqCst);

        let (host_res, host_state) = host_thread.join().expect("host thread");
        let (client_res, client_state) = client_thread.join().expect("client thread");
        assert!(matches!(host_res, Err(Error::Protocol { .. })));
        assert_eq!(host_state, SessionState::Closed);
        assert!(
            matches!(client_res, Err(Error::Protocol { .. } | Error::Network(_))),
            "2s 超时拒绝后 client 应返回明确错误，实际 {client_res:?}"
        );
        assert_eq!(client_state, SessionState::Closed);
        assert_eq!(observer.cancel_count(), 1);
    }

    // 子场景 B：timeout = 30s → 推进 2s 不触发，随后人工接受仍可正常完成握手。
    {
        let clock_us = Arc::new(AtomicU32::new(0));
        let host_clock: SessionClock = {
            let c = Arc::clone(&clock_us);
            Arc::new(move || c.load(Ordering::SeqCst))
        };
        let mock = MockApprover::new();
        let mut observer = mock.clone();
        let host = host_session_with(
            HostAuthConfig {
                password: None,
                iterations: 64,
                approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..HostAuthConfig::default()
            },
            Box::new(mock),
            host_clock,
        );
        let host_addr = host.local_addr().expect("host addr");
        let host_done = Arc::new(AtomicBool::new(false));
        let host_done_flag = Arc::clone(&host_done);
        let host_thread = thread::spawn(move || {
            let mut host = host;
            let res = host.accept_and_handshake(CONNECT_TIMEOUT);
            host_done_flag.store(true, Ordering::SeqCst);
            if res.is_ok() {
                let started = Instant::now();
                let _ = host.run(&mut || started.elapsed() >= Duration::from_secs(2));
            }
            res
        });
        let client_thread = thread::spawn(move || {
            let (mut client, _clock) =
                client_session_with(host_addr, 20_006, ClientAuthConfig::default());
            let res = client.connect_and_handshake();
            if res.is_ok() {
                let _ = client.close(ByeReason::Normal);
            }
            res
        });

        assert!(wait_until(Duration::from_secs(5), || {
            observer.submit_count() == 1
        }));
        clock_us.store(2_000_000, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(200));
        assert!(
            !host_done.load(Ordering::SeqCst),
            "30s 超时配置下推进 2s 不得触发超时"
        );
        assert_eq!(observer.cancel_count(), 0);

        // 放行并干净收尾。
        observer.preset(ApprovalVerdict::Accepted);
        assert!(host_thread.join().expect("host thread").is_ok());
        assert!(client_thread.join().expect("client thread").is_ok());
    }
}

/// T22-6：T19 联动顺序：密码校验失败 → `submit_count() == 0`（不打扰机主）+ `Welcome(Rejected)`（U1）。
#[test]
fn approval_skipped_when_password_fails() {
    let mut mock = MockApprover::new();
    mock.preset(ApprovalVerdict::Accepted);
    let mut observer = mock.clone();

    let mut host = host_session_with(
        HostAuthConfig {
            password: Some("correct-secret".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        },
        Box::new(mock),
        wall_clock(),
    );
    let host_addr = host.local_addr().expect("host addr");
    let host_thread = thread::spawn(move || {
        let res = host.accept_and_handshake(CONNECT_TIMEOUT);
        (res, host.state())
    });

    let (mut client, _clock) = client_session_with(
        host_addr,
        20_007,
        ClientAuthConfig {
            password: Some("wrong-secret".into()),
            ..ClientAuthConfig::default()
        },
    );
    let client_err = client
        .connect_and_handshake()
        .expect_err("错误密码必须被拒绝");
    assert!(
        matches!(client_err, Error::Protocol { .. } | Error::Network(_)),
        "错误密码时 client 应返回明确错误，实际 {client_err:?}"
    );
    assert_eq!(client.state(), SessionState::Closed);

    let (host_res, host_state) = host_thread.join().expect("host thread");
    assert!(matches!(host_res, Err(Error::Protocol { .. })));
    assert_eq!(host_state, SessionState::Closed);

    assert_eq!(
        observer.submit_count(),
        0,
        "密码失败不得调用 approver.submit"
    );
    assert!(observer.take_request().is_none());
    assert_eq!(observer.cancel_count(), 0);
}

/// T22-7：审批在途时 drop client 传输 → `cancel_count() == 1` 恰好一次，
/// host 会话按 T14 断线路径清理，无死锁无泄漏。
#[test]
fn approver_cancel_on_client_disconnect_during_approval() {
    let mock = MockApprover::new();
    let observer = mock.clone();

    let mut host = host_session_with(HostAuthConfig::default(), Box::new(mock), wall_clock());
    let host_addr = host.local_addr().expect("host addr");
    let host_thread = thread::spawn(move || {
        let res = host.accept_and_handshake(CONNECT_TIMEOUT);
        (res, host.state())
    });

    let (endpoint, mut transport) = raw_quic_client(host_addr);
    let hello = ControlMessage::Hello(Hello {
        version: rc_session::proto::PROTOCOL_VERSION,
        session_id: 20_008,
        requested: params(),
        auth_method: AuthMethod::None,
        auth_payload: Vec::new(),
    });
    let mut buf = Vec::new();
    hello.encode(&mut buf);
    transport.send_reliable(&buf).expect("send Hello");

    assert!(
        wait_until(Duration::from_secs(5), || observer.submit_count() == 1),
        "host 应在收到 Hello 后进入审批等待"
    );

    // 审批未决期间主动 drop client 传输（触发 QUIC CONNECTION_CLOSE）。
    drop(transport);
    drop(endpoint);

    let (host_res, host_state) = host_thread.join().expect("host thread");
    assert!(
        matches!(host_res, Err(Error::Network(_))),
        "client 断连后 host 应按断线路径返回 Network 错误，实际 {host_res:?}"
    );
    assert_eq!(host_state, SessionState::Closed);
    assert_eq!(observer.submit_count(), 1);
    assert_eq!(
        observer.cancel_count(),
        1,
        "client 断开时 approver.cancel 应恰好生效一次（含 HostSession Drop 幂等）"
    );
}

/// T22-8：默认（未显式配置审批）host 在 Hello 后同一拍内出 `Welcome{Ok}`（AutoApprover
/// 零拍延迟，无过程 `Welcome{ChallengeRequired}`），首帧时序与 T14/T18 基线一致（U3）；
/// 同时校验 client 默认超时已放宽至 ≥ 45s（U6）。
#[test]
fn headless_default_keeps_t18_handshake_shape() {
    // U6：默认 client 配置的 Hello→Welcome 与 auth 超时均 ≥ 45s（30s 审批 + 15s 余量）。
    let default_client_cfg = ClientSessionConfig::new(
        "127.0.0.1:1".parse().unwrap(),
        params(),
        1,
        Box::new(ScriptedCapture::standard()),
    );
    assert!(
        default_client_cfg.handshake_timeout >= Duration::from_secs(45),
        "默认 handshake_timeout 应 ≥ 45s，实际 {:?}",
        default_client_cfg.handshake_timeout
    );
    assert_eq!(
        default_client_cfg.auth.auth_timeout_us, DEFAULT_CLIENT_AUTH_TIMEOUT_US,
        "默认 auth_timeout_us 应为 45s"
    );

    // 线形检查：默认 HostSessionConfig::new() 收到 Hello 后首条可靠消息即为终局 Welcome{Ok}。
    {
        let mut hcfg = HostSessionConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            params(),
            Box::new(RecordingInjector::new()),
        );
        hcfg.mtu = rc_session::DEFAULT_SESSION_MTU;
        hcfg.pipeline = pcfg();
        hcfg.pace_capture = true;
        let mut host =
            HostSession::new(hcfg, SERVER_CERT_PEM, SERVER_KEY_PEM, wall_clock()).expect("host");
        attach_fake_host_pipeline(&mut host);
        let host_addr = host.local_addr().expect("host addr");
        let host_thread = thread::spawn(move || {
            host.accept_and_handshake(CONNECT_TIMEOUT)
                .expect("default host handshake");
            let started = Instant::now();
            let _ = host.run(&mut || started.elapsed() >= Duration::from_millis(400));
        });

        let (_endpoint, mut transport) = raw_quic_client(host_addr);
        let hello = ControlMessage::Hello(Hello {
            version: rc_session::proto::PROTOCOL_VERSION,
            session_id: 20_009,
            requested: params(),
            auth_method: AuthMethod::None,
            auth_payload: Vec::new(),
        });
        let mut buf = Vec::new();
        hello.encode(&mut buf);
        transport.send_reliable(&buf).expect("send Hello");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut first_ctrl = None;
        while Instant::now() < deadline && first_ctrl.is_none() {
            for ev in transport.poll(Duration::from_millis(20)).expect("poll") {
                if let NetEvent::Reliable(raw) = ev {
                    first_ctrl = Some(ControlMessage::decode(&raw).expect("decode ctrl"));
                    break;
                }
            }
        }
        match first_ctrl {
            Some(ControlMessage::Welcome(w)) => {
                assert_eq!(
                    w.auth_status,
                    AuthStatus::Ok,
                    "默认 AutoApprover 应零拍直出 Welcome{{Ok}}，不得插入 ChallengeRequired"
                );
                assert_eq!(w.granted, params());
            }
            other => panic!("首条控制消息应为 Welcome{{Ok}}，实际 {other:?}"),
        }
        drop(transport);
        host_thread.join().expect("host thread");
    }

    // 端到端时序检查：默认 host + client 首帧时序满足 T18 基线（≤ 800ms）。
    {
        let mut hcfg = HostSessionConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            params(),
            Box::new(RecordingInjector::new()),
        );
        hcfg.mtu = rc_session::DEFAULT_SESSION_MTU;
        hcfg.pipeline = pcfg();
        hcfg.pace_capture = true;
        let mut host =
            HostSession::new(hcfg, SERVER_CERT_PEM, SERVER_KEY_PEM, wall_clock()).expect("host");
        attach_fake_host_pipeline(&mut host);
        let host_addr = host.local_addr().expect("host addr");
        let host_thread = spawn_host(host, Duration::from_secs(2));

        let (mut client, clock) =
            client_session_with(host_addr, 20_010, ClientAuthConfig::default());
        client.connect_and_handshake().expect("client handshake");
        let first_frame = drive_client(&mut client, &clock, Duration::from_millis(1_000));
        client.close(ByeReason::Normal).expect("client close");
        let _host_run = host_thread.join().expect("host thread");

        let ff_us = first_frame.expect("默认 headless 会话必须产出首帧");
        assert!(
            ff_us <= 800_000,
            "默认 headless 会话首帧时序应 ≤ 800ms（T18 基线），实际 {}ms",
            ff_us as f64 / 1_000.0
        );
    }
}
