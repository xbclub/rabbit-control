//! 进程内双端（真 QUIC over 127.0.0.1）会话集成测试（T18）。
//!
//! 与 `quic_loopback.rs` 的差别：那边只验证「握手 → 流 → 关闭」的连通
//! 账目；这里跑**完整会话形态**（假后端 64×64@60fps、ScriptedCapture
//! 脚本输入、RecordingInjector 记录注入、探测泵每拍驱动），断言
//! [`rc_session::report::SessionReport`] 的 Gate 通过与输入链路的
//! InputAck 采样。免权限、CI 可跑（S7）。

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_session::proto::SessionParams;
use rc_session::report::{SessionConfigSnapshot, SessionReport};
use rc_session::testkit::{RecordingInjector, ScriptedCapture};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
};
use rc_types::Resolution;

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
    let duration_us = 3_000_000u32;
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
