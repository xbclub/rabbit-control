//! mock pair 进程内双会话闭环（T14 集成测试）。
//!
//! 经 `rc_session::testkit::pair` 把 HostSession 与 ClientSession 接在
//! 同一对互连 MockTransport 上：握手 → 流媒体 + 输入注入 → 优雅关闭。
//! 全程离线确定性（无网络、无直接 sleep；帧等待经管线 wait_event）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{EncoderConfig, FakeEncoder};
use rc_input::{InputEvent, InputInjector, MouseButton};
use rc_net::quic::DEFAULT_MTU;
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::proto::{
    AuthMethod, ByeReason, ControlMessage, Hello, SessionParams, PROTOCOL_VERSION,
};
use rc_session::testkit::pair;
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
    StopReason,
};
use rc_types::{Resolution, Result};

/// 会话号（固定值保证确定性）。
const SESSION_ID: u64 = 2026_1121;

/// 流参数（假后端能力：64×64@30、1Mbps）。
fn params() -> SessionParams {
    SessionParams {
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 1_000_000,
    }
}

fn pcfg() -> PipelineConfig {
    PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 30,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；真 QUIC 路径由 host 侧 expect_hello 按 N2 覆盖。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

/// 记录型注入器（host 侧）：事件经 Arc 透给测试断言。
struct RecordingInjector {
    events: Arc<Mutex<Vec<InputEvent>>>,
}

impl InputInjector for RecordingInjector {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        self.events.lock().unwrap().extend_from_slice(events);
        Ok(())
    }
    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

/// 脚本型输入捕获（client 侧）：一次性吐一条点击。
struct ScriptedCapture {
    scripted: Vec<InputEvent>,
}

impl rc_input::InputCapture for ScriptedCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(std::mem::take(&mut self.scripted))
    }
}

/// 会话零点槽（照 run_real 的 origin_slot 模式，坑 3：锚首个采集帧）。
type OriginSlot = Arc<Mutex<Option<Instant>>>;

fn origin_clock(origin: OriginSlot) -> SessionClock {
    Arc::new(move || match *origin.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    })
}

/// 组装一对经 pair 互连的会话（握手完成、时钟共享同一零点槽）。
/// 返回 (host, client, host 传输句柄, 注入事件槽, 零点槽)。
fn paired_sessions(
    click: Option<InputEvent>,
) -> (
    HostSession,
    ClientSession,
    rc_session::MockTransport,
    Arc<Mutex<Vec<InputEvent>>>,
    OriginSlot,
) {
    let (ta, tb) = pair();
    let mut ta_handle = ta.clone();
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));

    // 确定性握手顺序（mock poll 忽略超时，不开线程竞速）：
    // ① 预置 Hello 给 host → ② host 真握手（真 Welcome 进链路）→
    // ③ client 消费真 Welcome。
    ta_handle.deliver_reliable(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        session_id: SESSION_ID,
        requested: params(),
        auth_method: AuthMethod::None,
        auth_payload: vec![],
    }));

    let events: Arc<Mutex<Vec<InputEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let inj = RecordingInjector {
        events: Arc::clone(&events),
    };
    let mut host = HostSession::with_parts(
        HostSessionConfig {
            listen_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_millis(500),
            injector: Box::new(inj),
            pace_capture: true, // 假后端必须节流（坑 4）
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        Box::new(ta),
        Box::new(
            FakeBackend::new(FakeBackendConfig {
                display: DisplayInfo {
                    id: rc_types::DisplayId(0),
                    resolution: Resolution::new(64, 64),
                    scale: 1.0,
                    hdr: false,
                },
                fps: 30,
                content: FakeContent::Dynamic,
                dirty_rect: None,
            })
            .expect("fake capture"),
        ),
        Box::new(
            FakeEncoder::with_tuning(
                EncoderConfig {
                    resolution: Resolution::new(64, 64),
                    fps: 30,
                    bitrate_bps: 1_000_000,
                    ..EncoderConfig::default()
                },
                500,
                128,
            )
            .expect("fake encoder"),
        ),
        Arc::clone(&clock),
    )
    .expect("host with_parts");
    host.handshake().expect("host handshake");

    let mut client = ClientSession::with_parts(
        ClientSessionConfig {
            peer_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            requested: params(),
            session_id: SESSION_ID,
            pipeline: pcfg(),
            connect_timeout: Duration::from_secs(1),
            handshake_timeout: Duration::from_millis(500),
            capture: Box::new(ScriptedCapture {
                scripted: click.into_iter().collect(),
            }),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        },
        Box::new(tb),
        Box::new(
            FakeDecoder::new(FakeDecoderConfig {
                resolution: Resolution::new(64, 64),
                ..FakeDecoderConfig::default()
            })
            .expect("fake decoder"),
        ),
        Box::new(
            FakeRenderer::new(FakeRendererConfig {
                size: (64, 64),
                ..FakeRendererConfig::default()
            })
            .expect("fake renderer"),
        ),
        clock,
    )
    .expect("client with_parts");
    client.handshake().expect("client handshake");

    (host, client, ta_handle, events, origin)
}

/// 端到端闭环：host+client 经 pair 互连，假后端，注入式时钟推进若干拍 →
/// client `frames_displayed > 0`、host 注入器收到 client 事件、首帧数据报
/// 为 IDR、双端停机后 `unfinished == 0`。
#[test]
fn mock_pair_session_roundtrip_streams_and_injects() {
    let (mut host, mut client, ta_handle, injected, origin) =
        paired_sessions(Some(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }));
    assert_eq!(host.state(), SessionState::Streaming);
    assert_eq!(client.state(), SessionState::Streaming);

    // 驱动若干拍：client 先收上一轮 host 发出的数据报，再让 host 产新帧。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("client tick");
        host.tick(Duration::from_millis(0)).expect("host tick");
        // 会话零点锚首个采集帧（坑 3：锚在连接建立会把启动延迟吃进零点偏移）。
        {
            let mut slot = origin.lock().expect("origin lock");
            if slot.is_none() {
                *slot = host.pipeline().first_capture();
            }
        }
        if client.pipeline().displayed() > 0 && !injected.lock().unwrap().is_empty() {
            break;
        }
        host.pipeline().wait_event(Duration::from_millis(2));
    }
    assert!(
        client.pipeline().displayed() > 0,
        "端到端应有帧上屏（实际 {}）",
        client.pipeline().displayed()
    );
    assert_eq!(
        injected.lock().unwrap().len(),
        1,
        "host 注入器应收到 client 的点击"
    );
    // 首帧数据报必须是 IDR。
    let datagrams = ta_handle.datagrams_sent();
    assert!(!datagrams.is_empty(), "应有媒体数据报");
    let header = rc_pipeline::host::datagram_header(&datagrams[0]).expect("header");
    assert!(header.is_idr(), "首帧数据报必须是 IDR");

    // 关闭：host 先 close（Bye + 排空数据报进链路），client 的下一拍收
    // Bye 前先把排空数据报全部推入重组（一个 poll 批内完成）。
    let host_summary = host.close(ByeReason::Normal).expect("host close");
    let r = client
        .tick(Duration::from_millis(0), Instant::now())
        .expect("client tick");
    assert_eq!(
        r.closed,
        Some(StopReason::ByeReceived(ByeReason::Normal)),
        "client 应观测到 Bye"
    );
    assert_eq!(client.state(), SessionState::Closed);
    assert_eq!(host.state(), SessionState::Closed);

    // 双端停机后账目闭合。
    assert_eq!(
        host_summary.unfinished_traces, 0,
        "host 未结帧（扣除已发出）必须为 0"
    );
    assert_eq!(
        host_summary.frames_encoded,
        host.pipeline().sched_stats().encoded
    );
    assert!(
        client.pipeline().trace().summary().unfinished == 0,
        "client trace 必须全部有结局"
    );
    // 合并时间线（host 半场 + client 判定）也无未结帧：编码帧的结局由
    // client 侧收口（merge_traces 的契约）。
    let merged =
        rc_pipeline::merge_traces(host.pipeline().trace(), client.pipeline().trace(), 1024);
    assert_eq!(merged.summary().unfinished, 0, "合并时间线不得有未结帧");
}

/// 一侧 `close(Normal)` → 对侧 run/tick 观测 `ByeReceived` 并完成停机。
#[test]
fn mock_pair_close_one_side_closes_the_other() {
    let (mut host, mut client, _ta_handle, _injected, origin) = paired_sessions(None);

    // 流几拍到有帧。
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("client tick");
        host.tick(Duration::from_millis(0)).expect("host tick");
        {
            let mut slot = origin.lock().expect("origin lock");
            if slot.is_none() {
                *slot = host.pipeline().first_capture();
            }
        }
        if client.pipeline().displayed() > 0 {
            break;
        }
        host.pipeline().wait_event(Duration::from_millis(2));
    }

    // client 主动关闭 → host 的下一拍观测 ByeReceived 并完成停机。
    let client_summary = client.close(ByeReason::Normal).expect("client close");
    assert_eq!(client_summary.stop_reason, StopReason::Requested);

    let mut rounds = 0;
    let r = loop {
        rounds += 1;
        assert!(rounds < 100, "host 应在若干拍内观测到 Bye");
        let r = host.tick(Duration::from_millis(0)).expect("host tick");
        if let Some(why) = r.closed {
            break why;
        }
        host.pipeline().wait_event(Duration::from_millis(2));
    };
    assert_eq!(r, StopReason::ByeReceived(ByeReason::Normal));
    assert_eq!(host.state(), SessionState::Closed);

    // 已停机的会话再 close：不重复发 Bye，直接给账目（closed_reason 透传）。
    let summary = host.close(ByeReason::Normal).expect("close after closed");
    assert_eq!(
        summary.stop_reason,
        StopReason::ByeReceived(ByeReason::Normal)
    );
    assert_eq!(summary.unfinished_traces, 0);
    assert!(
        client.pipeline().trace().summary().unfinished == 0,
        "client 停机完整"
    );
}
