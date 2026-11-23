//! T20 进程内双会话剪贴板集成测试：双向同步、回声抑制（20 拍零乒乓）、
//! 节流突发合并（窗口内 3 连复制 → 对端恰收 1 条最新值）、未启用时静默计数
//! 不写本机不拆会话（CB1），以及 macOS 真机 pasteboard <200ms 延迟测试（`#[ignore]`）。
//!
//! 自带最小 `LoopTransport`（实现 `rc_net::Transport`：两条方向的 mpsc 对接
//! datagram/reliable/urgent 三通道，照 T15 `tests/probe_session.rs` 形态），
//! 全部假后端、逻辑钟注入、无网络无权限无直接 sleep（仅 `#[ignore]` 真机测试
//! 用墙钟测量 B4 <200ms 耗时）。

use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{Encoder, EncoderConfig, FakeEncoder};
use rc_input::{InputEvent, InputInjector, MouseButton};
use rc_net::quic::DEFAULT_MTU;
use rc_net::{NetEvent, NetStats, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::clipboard::CLIPBOARD_MIN_NOTIFY_INTERVAL_US;
use rc_session::proto::{
    AuthMethod, ControlMessage, Hello, SessionParams, PROTOCOL_VERSION, TYPE_CLIPBOARD_NOTIFY,
};
use rc_session::testkit::SharedFakeClipboard;
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
};
use rc_types::{Error, Result};

// ─────────────────────────────────────────────────────────────────
// LoopTransport：mpsc 三通道成对互联的最小 Transport
// ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct OwnState {
    datagrams_sent: Vec<Vec<u8>>,
    reliable_sent: Vec<Vec<u8>>,
    urgent_sent: Vec<Vec<u8>>,
    deliver: VecDeque<NetEvent>,
}

struct LoopTransport {
    own: Arc<Mutex<OwnState>>,
    dgram_tx: Sender<Vec<u8>>,
    rel_tx: Sender<Vec<u8>>,
    urg_tx: Sender<Vec<u8>>,
    dgram_rx: Receiver<Vec<u8>>,
    rel_rx: Receiver<Vec<u8>>,
    urg_rx: Receiver<Vec<u8>>,
}

#[derive(Clone)]
struct LoopHandle {
    own: Arc<Mutex<OwnState>>,
}

fn loop_pair() -> (LoopTransport, LoopTransport, LoopHandle, LoopHandle) {
    let (ab_d_tx, ab_d_rx) = channel::<Vec<u8>>();
    let (ab_r_tx, ab_r_rx) = channel::<Vec<u8>>();
    let (ab_u_tx, ab_u_rx) = channel::<Vec<u8>>();
    let (ba_d_tx, ba_d_rx) = channel::<Vec<u8>>();
    let (ba_r_tx, ba_r_rx) = channel::<Vec<u8>>();
    let (ba_u_tx, ba_u_rx) = channel::<Vec<u8>>();
    let own_a = Arc::new(Mutex::new(OwnState::default()));
    let own_b = Arc::new(Mutex::new(OwnState::default()));
    let a = LoopTransport {
        own: Arc::clone(&own_a),
        dgram_tx: ab_d_tx,
        rel_tx: ab_r_tx,
        urg_tx: ab_u_tx,
        dgram_rx: ba_d_rx,
        rel_rx: ba_r_rx,
        urg_rx: ba_u_rx,
    };
    let b = LoopTransport {
        own: Arc::clone(&own_b),
        dgram_tx: ba_d_tx,
        rel_tx: ba_r_tx,
        urg_tx: ba_u_tx,
        dgram_rx: ab_d_rx,
        rel_rx: ab_r_rx,
        urg_rx: ab_u_rx,
    };
    (a, b, LoopHandle { own: own_a }, LoopHandle { own: own_b })
}

impl LoopHandle {
    fn deliver(&self, ev: NetEvent) {
        self.own.lock().expect("own lock").deliver.push_back(ev);
    }

    fn deliver_reliable(&self, msg: &ControlMessage) {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.deliver(NetEvent::Reliable(buf));
    }

    fn datagrams_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").datagrams_sent.clone()
    }

    fn reliable_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").reliable_sent.clone()
    }

    fn urgent_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").urgent_sent.clone()
    }
}

impl Transport for LoopTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        self.own
            .lock()
            .expect("own lock")
            .datagrams_sent
            .push(data.to_vec());
        self.dgram_tx
            .send(data.to_vec())
            .map_err(|e| Error::Network(format!("loop link broken: {e}")))
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        self.own
            .lock()
            .expect("own lock")
            .reliable_sent
            .push(data.to_vec());
        self.rel_tx
            .send(data.to_vec())
            .map_err(|e| Error::Network(format!("loop link broken: {e}")))
    }

    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        self.own
            .lock()
            .expect("own lock")
            .urgent_sent
            .push(data.to_vec());
        self.urg_tx
            .send(data.to_vec())
            .map_err(|e| Error::Network(format!("loop link broken: {e}")))
    }

    fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
        let mut out: Vec<NetEvent> = {
            let mut own = self.own.lock().expect("own lock");
            own.deliver.drain(..).collect()
        };
        while let Ok(d) = self.dgram_rx.try_recv() {
            out.push(NetEvent::Datagram(d));
        }
        while let Ok(m) = self.rel_rx.try_recv() {
            out.push(NetEvent::Reliable(m));
        }
        while let Ok(m) = self.urg_rx.try_recv() {
            out.push(NetEvent::ReliableUrgent(m));
        }
        Ok(out)
    }

    fn stats(&self) -> NetStats {
        NetStats::default()
    }
}

// ─────────────────────────────────────────────────────────────────
// 会话组装与辅助
// ─────────────────────────────────────────────────────────────────

const SESSION_ID: u64 = 20_261_122;

fn logical_clock() -> (Arc<Mutex<u64>>, SessionClock) {
    let slot = Arc::new(Mutex::new(0u64));
    let s2 = Arc::clone(&slot);
    let clock: SessionClock =
        Arc::new(move || u32::try_from(*s2.lock().expect("clock lock")).unwrap_or(u32::MAX));
    (slot, clock)
}

fn set_clock(slot: &Arc<Mutex<u64>>, us: u64) {
    *slot.lock().expect("clock lock") = us;
}

fn session_params() -> SessionParams {
    SessionParams {
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 1_000_000,
    }
}

fn pcfg() -> PipelineConfig {
    PipelineConfig {
        resolution: rc_types::Resolution::new(64, 64),
        fps: 30,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

fn fake_capture() -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: rc_types::Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 30,
            content: FakeContent::Static,
            dirty_rect: None,
        })
        .expect("fake capture"),
    )
}

fn fake_encoder() -> Box<dyn Encoder> {
    Box::new(
        FakeEncoder::with_tuning(
            EncoderConfig {
                resolution: rc_types::Resolution::new(64, 64),
                fps: 30,
                bitrate_bps: 1_000_000,
                ..EncoderConfig::default()
            },
            500,
            128,
        )
        .expect("fake encoder"),
    )
}

#[derive(Clone, Default)]
struct CountingInjector {
    events: Arc<Mutex<Vec<InputEvent>>>,
}

impl InputInjector for CountingInjector {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        self.events
            .lock()
            .expect("injector lock")
            .extend_from_slice(events);
        Ok(())
    }

    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

#[derive(Clone, Default)]
struct ScriptedInput {
    pending: Arc<Mutex<Vec<InputEvent>>>,
}

impl ScriptedInput {
    fn push(&self, ev: InputEvent) {
        self.pending.lock().expect("input lock").push(ev);
    }
}

impl rc_input::InputCapture for ScriptedInput {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(std::mem::take(
            &mut *self.pending.lock().expect("input lock"),
        ))
    }
}

struct Fixture {
    host: HostSession,
    client: ClientSession,
    host_handle: LoopHandle,
    client_handle: LoopHandle,
    clock_slot: Arc<Mutex<u64>>,
    injector: CountingInjector,
    input: ScriptedInput,
    base_instant: Instant,
}

impl Fixture {
    fn new() -> Self {
        let (clock_slot, clock) = logical_clock();
        let (ta, tb, ha, hb) = loop_pair();
        ha.deliver_reliable(&ControlMessage::Hello(Hello {
            version: PROTOCOL_VERSION,
            session_id: SESSION_ID,
            requested: session_params(),
            auth_method: AuthMethod::None,
            auth_payload: vec![],
        }));

        let injector = CountingInjector::default();
        let mut host = HostSession::with_parts(
            HostSessionConfig {
                listen_addr: "127.0.0.1:9901".parse().unwrap(),
                mtu: DEFAULT_MTU,
                caps: session_params(),
                pipeline: pcfg(),
                handshake_timeout: Duration::from_millis(500),
                injector: Box::new(injector.clone()),
                pace_capture: true,
                auth: rc_session::HostAuthConfig::default(),
                approver: Box::new(rc_session::AutoApprover),
            },
            Box::new(ta),
            fake_capture(),
            fake_encoder(),
            Arc::clone(&clock),
        )
        .expect("host with_parts");
        host.handshake().expect("host handshake");

        let input = ScriptedInput::default();
        let mut client = ClientSession::with_parts(
            ClientSessionConfig {
                peer_addr: "127.0.0.1:9901".parse().unwrap(),
                mtu: DEFAULT_MTU,
                requested: session_params(),
                session_id: SESSION_ID,
                pipeline: pcfg(),
                connect_timeout: Duration::from_millis(500),
                handshake_timeout: Duration::from_millis(500),
                capture: Box::new(input.clone()),
                input_window: Duration::from_millis(1),
                auth: rc_session::ClientAuthConfig::default(),
            },
            Box::new(tb),
            Box::new(FakeDecoder::new(FakeDecoderConfig::default()).expect("fake decoder")),
            Box::new(FakeRenderer::new(FakeRendererConfig::default()).expect("fake renderer")),
            clock,
        )
        .expect("client with_parts");
        client.handshake().expect("client handshake");

        // 排空握手期 client 发出的 Hello（host 侧预置 Hello 已消费，client 实发 Hello 留在 host 队列）。
        host.tick(Duration::ZERO)
            .expect("drain post-handshake hello");

        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(client.state(), SessionState::Streaming);

        Self {
            host,
            client,
            host_handle: ha,
            client_handle: hb,
            clock_slot,
            injector,
            input,
            base_instant: Instant::now(),
        }
    }

    /// 推进双端逻辑钟并各驱动一拍（先 client 后 host，再补一拍以交付当拍发送的消息）。
    fn step_at(&mut self, now_us: u64) {
        set_clock(&self.clock_slot, now_us);
        let now = self.base_instant + Duration::from_micros(now_us);
        self.client.tick(Duration::ZERO, now).expect("client tick");
        self.host.tick(Duration::ZERO).expect("host tick");
    }
}

// ─────────────────────────────────────────────────────────────────
// 集成测试
// ─────────────────────────────────────────────────────────────────

/// 双向同步：client 复制 → host 收到同文本；host 反向复制 → client 收到同文本；
/// 且验证 CB2：ClipboardNotify 仅走 reliable 控制流（Stream A），绝不走 datagram / urgent。
#[test]
fn clipboard_syncs_both_directions() {
    let mut fx = Fixture::new();
    let host_cb = SharedFakeClipboard::new();
    let client_cb = SharedFakeClipboard::new();

    fx.host.enable_clipboard(Box::new(host_cb.clone()));
    fx.client.enable_clipboard(Box::new(client_cb.clone()));
    assert!(fx.host.clipboard_enabled());
    assert!(fx.client.clipboard_enabled());

    // 首拍建立基线（CB6）
    fx.step_at(0);
    assert!(host_cb.writes().is_empty());
    assert!(client_cb.writes().is_empty());

    // 1. client → host
    client_cb.simulate_local_copy(Some("hello-from-client-验证码123"));
    fx.step_at(10_000);
    assert_eq!(host_cb.writes(), vec!["hello-from-client-验证码123"]);
    assert_eq!(fx.client.clipboard_stats().sent, 1);
    assert_eq!(fx.host.clipboard_stats().received, 1);

    // 2. host → client
    host_cb.simulate_local_copy(Some("error-stack-from-host"));
    fx.step_at(20_000);
    fx.step_at(30_000);
    assert_eq!(client_cb.writes(), vec!["error-stack-from-host"]);
    assert_eq!(fx.host.clipboard_stats().sent, 1);
    assert_eq!(fx.client.clipboard_stats().received, 1);

    // CB2：所有 ClipboardNotify 均走 reliable 流，datagram / urgent 绝不含 0x0F
    let client_rel_cb = fx
        .client_handle
        .reliable_sent()
        .into_iter()
        .filter(|m| m.first() == Some(&TYPE_CLIPBOARD_NOTIFY))
        .count();
    let host_rel_cb = fx
        .host_handle
        .reliable_sent()
        .into_iter()
        .filter(|m| m.first() == Some(&TYPE_CLIPBOARD_NOTIFY))
        .count();
    assert_eq!(client_rel_cb, 1, "client 应经 reliable 发送恰 1 条 0x0F");
    assert_eq!(host_rel_cb, 1, "host 应经 reliable 发送恰 1 条 0x0F");
    assert!(
        fx.client_handle
            .datagrams_sent()
            .iter()
            .all(|d| d.first() != Some(&TYPE_CLIPBOARD_NOTIFY)),
        "ClipboardNotify 绝不进 datagram"
    );
    assert!(
        fx.client_handle
            .urgent_sent()
            .iter()
            .all(|u| u.first() != Some(&TYPE_CLIPBOARD_NOTIFY)),
        "ClipboardNotify 绝不进 urgent"
    );
    assert!(
        fx.host_handle
            .datagrams_sent()
            .iter()
            .all(|d| d.first() != Some(&TYPE_CLIPBOARD_NOTIFY)),
        "host ClipboardNotify 绝不进 datagram"
    );
    assert!(
        fx.host_handle
            .urgent_sent()
            .iter()
            .all(|u| u.first() != Some(&TYPE_CLIPBOARD_NOTIFY)),
        "host ClipboardNotify 绝不进 urgent"
    );
}

/// CB5 回声抑制：双向同步后连续驱动 20 拍（逻辑钟），两侧 reliable 通道零新增消息、
/// `echo_suppressed` 恰各 1。
#[test]
fn no_ping_pong_after_sync() {
    let mut fx = Fixture::new();
    let host_cb = SharedFakeClipboard::new();
    let client_cb = SharedFakeClipboard::new();

    fx.host.enable_clipboard(Box::new(host_cb.clone()));
    fx.client.enable_clipboard(Box::new(client_cb.clone()));

    // 基线拍
    fx.step_at(0);

    // client → host 同步一次（host 写入并在同拍 pump_clipboard 抑制回声）
    client_cb.simulate_local_copy(Some("sync-a"));
    fx.step_at(10_000);
    assert_eq!(host_cb.writes(), vec!["sync-a"]);

    // host → client 同步一次（client 写入并在同拍 pump_clipboard 抑制回声）
    host_cb.simulate_local_copy(Some("sync-b"));
    fx.step_at(20_000);
    fx.step_at(30_000);
    assert_eq!(client_cb.writes(), vec!["sync-b"]);

    let count_cb_reliable = |h: &LoopHandle| {
        h.reliable_sent()
            .into_iter()
            .filter(|m| m.first() == Some(&TYPE_CLIPBOARD_NOTIFY))
            .count()
    };
    let host_cb_before = count_cb_reliable(&fx.host_handle);
    let client_cb_before = count_cb_reliable(&fx.client_handle);
    assert_eq!(host_cb_before, 1);
    assert_eq!(client_cb_before, 1);

    // 连续驱动 20 拍（每拍推进 50ms，累计 1s = 10× 节流窗口，确保任何潜在乒乓或挂起冲刷都会暴露）。
    for i in 1..=20u64 {
        fx.step_at(30_000 + i * 50_000);
    }

    assert_eq!(
        count_cb_reliable(&fx.host_handle),
        host_cb_before,
        "同步后 20 拍 host reliable 通道必须零新增 ClipboardNotify"
    );
    assert_eq!(
        count_cb_reliable(&fx.client_handle),
        client_cb_before,
        "同步后 20 拍 client reliable 通道必须零新增 ClipboardNotify"
    );
    assert_eq!(fx.host.clipboard_stats().sent, 1);
    assert_eq!(fx.client.clipboard_stats().sent, 1);
    assert_eq!(
        fx.host.clipboard_stats().echo_suppressed,
        1,
        "host echo_suppressed 应恰为 1"
    );
    assert_eq!(
        fx.client.clipboard_stats().echo_suppressed,
        1,
        "client echo_suppressed 应恰为 1"
    );
}

/// CB4 节流合并：100ms 窗口内 3 连复制 → 对端恰收 1 条且为最新值。
#[test]
fn throttle_burst_sends_single_latest_over_transport() {
    let mut fx = Fixture::new();
    let host_cb = SharedFakeClipboard::new();
    let client_cb = SharedFakeClipboard::new();

    fx.host.enable_clipboard(Box::new(host_cb.clone()));
    fx.client.enable_clipboard(Box::new(client_cb.clone()));

    // 基线拍 + 启动窗口的首发（leading edge）
    fx.step_at(0);
    client_cb.simulate_local_copy(Some("init"));
    fx.step_at(10_000);
    assert_eq!(host_cb.writes(), vec!["init"]);
    assert_eq!(fx.client.clipboard_stats().sent, 1);

    // 100ms 窗口内（10_000 .. 110_000）连续 3 次本地复制
    client_cb.simulate_local_copy(Some("burst-1"));
    fx.step_at(20_000);
    client_cb.simulate_local_copy(Some("burst-2"));
    fx.step_at(50_000);
    client_cb.simulate_local_copy(Some("burst-3-latest"));
    fx.step_at(80_000);

    // 窗口未到期前对端不得收到任何突发条
    assert_eq!(host_cb.writes(), vec!["init"]);
    assert_eq!(fx.client.clipboard_stats().sent, 1);
    assert_eq!(fx.client.clipboard_stats().coalesced, 2);

    // 窗口到期后驱动一拍：冲刷恰 1 条最新值
    fx.step_at(10_000 + CLIPBOARD_MIN_NOTIFY_INTERVAL_US);
    assert_eq!(host_cb.writes(), vec!["init", "burst-3-latest"]);
    assert_eq!(fx.client.clipboard_stats().sent, 2);
    assert_eq!(fx.host.clipboard_stats().received, 2);

    // 后续多拍不再补发旧值
    fx.step_at(300_000);
    fx.step_at(500_000);
    assert_eq!(host_cb.writes(), vec!["init", "burst-3-latest"]);
}

/// CB1 默认关闭：未 `enable_clipboard` 的会话收到 0x0F → `received_while_disabled +1`、
/// 不写本机、不拆会话（后续输入/媒体照常流动）。
#[test]
fn notify_while_disabled_is_dropped_not_written() {
    let mut fx = Fixture::new();
    let client_cb = SharedFakeClipboard::new();

    // 仅启用 client，host 保持默认禁用
    fx.client.enable_clipboard(Box::new(client_cb.clone()));
    assert!(!fx.host.clipboard_enabled());
    assert!(fx.client.clipboard_enabled());

    fx.step_at(0);
    client_cb.simulate_local_copy(Some("sensitive-secret"));
    fx.step_at(10_000);

    assert_eq!(
        fx.host.state(),
        SessionState::Streaming,
        "未启用收到 0x0F 绝不拆会话"
    );
    assert_eq!(fx.host.clipboard_stats().received_while_disabled, 1);
    assert_eq!(fx.host.clipboard_stats().received, 0);

    // 验证会话未受损：后续输入照常流向 host 注入器
    fx.input.push(InputEvent::MouseButton {
        button: MouseButton::Left,
        pressed: true,
    });
    fx.step_at(25_000);
    assert_eq!(fx.host.state(), SessionState::Streaming);
    assert_eq!(
        fx.injector.events.lock().expect("injector lock").len(),
        1,
        "收到 0x0F 后后续输入应照常送达"
    );
}

/// B4 真机剪贴板延迟测试（`#[ignore]`；无需 TCC，但会改写当前登录会话的真实剪贴板）：
/// 一端使用真实 `NspasteboardBackend`、另一端使用 `SharedFakeClipboard`，
/// 测量外部写入系统剪贴板 → 对端 `writes()` 出现同文本的墙钟耗时 < 200ms。
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn real_pasteboard_sync_under_200ms() {
    use rc_session::clipboard::ClipboardBackend;
    use rc_session::NspasteboardBackend;

    let mut fx = Fixture::new();
    let real_host_cb = NspasteboardBackend::new().expect("macOS pasteboard");
    let client_cb = SharedFakeClipboard::new();

    fx.host.enable_clipboard(Box::new(real_host_cb));
    fx.client.enable_clipboard(Box::new(client_cb.clone()));

    // 建立基线
    fx.step_at(0);

    let payload = format!("rabbitcontrol-t20-real-sync-{}", std::process::id());
    let mut writer = NspasteboardBackend::new().expect("external pasteboard handle");

    let start = Instant::now();
    writer
        .write_text(&payload)
        .expect("write system pasteboard");

    let mut tick_idx = 1u64;
    let deadline = Duration::from_secs(2);
    while start.elapsed() < deadline {
        fx.step_at(tick_idx * 5_000);
        tick_idx += 1;
        if client_cb.writes().iter().any(|w| w == &payload) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let elapsed = start.elapsed();
    assert!(
        client_cb.writes().iter().any(|w| w == &payload),
        "对端未在 {deadline:?} 内收到真实剪贴板文本"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "B4 剪贴板同步墙钟耗时 {elapsed:?} 超过 200ms 门槛"
    );
}
