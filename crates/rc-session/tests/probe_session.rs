//! T15 进程内双会话探测集成测试：ProbePing/Pong 心跳、BwProbe 数据报
//! 粗测 → SetBitrate 初始码率链、B4 分流不污染管线、探测失败保持协商值。
//!
//! 自带最小 `LoopTransport`（实现 `rc_net::Transport`：两条方向的 mpsc
//! 对接 datagram/reliable/urgent 三通道，仿 rc-input channel.rs 测试里
//! MockTransport 的形态但成对互联），**不依赖 T18 的 testkit**。
//! 全部假后端、逻辑钟注入、无网络无权限无直接 sleep（探测链均确定性；
//! 仅 `setbitrate_reaches_recording_encoder` 对编码线程做 ≤2s 有界轮询，
//! 与 m1.rs 真实线程测试同一形态）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{
    CaptureBackend, CapturedFrame, DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent,
};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{EncodedFrame, Encoder, EncoderCaps, EncoderConfig, FakeEncoder};
use rc_input::{InputEvent, InputInjector};
use rc_net::quic::DEFAULT_MTU;
use rc_net::{NetEvent, NetStats, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::probe::{self, initial_bitrate_from};
use rc_session::proto::{
    AuthMethod, BitrateReason, ControlMessage, Hello, SessionParams, PROTOCOL_VERSION,
    TYPE_BW_PROBE_DATA, TYPE_PROBE_PING, TYPE_PROBE_PONG, TYPE_SET_BITRATE,
};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
};
use rc_types::{Error, Result, Timestamp};

// ─────────────────────────────────────────────────────────────────
// LoopTransport：mpsc 三通道成对互联的最小 Transport
// ─────────────────────────────────────────────────────────────────

/// 链路共享配置（测试经句柄事后调整，模拟链路状况）：
/// - `drop_bw_datagrams`：丢弃探测数据报（探测流在途中全丢，B6 反向）；
/// - `max_datagrams_per_poll`：每次 poll 最多放行的数据报数（0 = 不限；
///   模拟带宽受限下的分批到达，使首末块间隔可注入、估速可确定断言）。
#[derive(Default)]
struct LinkCfg {
    drop_bw_datagrams: AtomicBool,
    max_datagrams_per_poll: AtomicUsize,
}

/// 一端的私有状态：三通道发送记录（通道纪律断言用）+ 脚本化投递队列
/// （握手期预置 Hello/Welcome 用，先于互连事件交付）。
#[derive(Default)]
struct OwnState {
    datagrams_sent: Vec<Vec<u8>>,
    reliable_sent: Vec<Vec<u8>>,
    urgent_sent: Vec<Vec<u8>>,
    deliver: VecDeque<NetEvent>,
}

/// 进程内双工链路的一端（Box 进会话）。
struct LoopTransport {
    own: Arc<Mutex<OwnState>>,
    cfg: Arc<LinkCfg>,
    dgram_tx: Sender<Vec<u8>>,
    rel_tx: Sender<Vec<u8>>,
    urg_tx: Sender<Vec<u8>>,
    dgram_rx: Receiver<Vec<u8>>,
    rel_rx: Receiver<Vec<u8>>,
    urg_rx: Receiver<Vec<u8>>,
}

/// 传输句柄：与被 Box 走的一端共享记录/投递队列与链路配置，测试断言
/// 与链路状况注入的唯一通道。
#[derive(Clone)]
struct LoopHandle {
    own: Arc<Mutex<OwnState>>,
    cfg: Arc<LinkCfg>,
}

/// 一对互连的 LoopTransport + 各自的断言句柄。
fn loop_pair() -> (LoopTransport, LoopTransport, LoopHandle, LoopHandle) {
    let cfg = Arc::new(LinkCfg::default());
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
        cfg: Arc::clone(&cfg),
        dgram_tx: ab_d_tx,
        rel_tx: ab_r_tx,
        urg_tx: ab_u_tx,
        dgram_rx: ba_d_rx,
        rel_rx: ba_r_rx,
        urg_rx: ba_u_rx,
    };
    let b = LoopTransport {
        own: Arc::clone(&own_b),
        cfg: Arc::clone(&cfg),
        dgram_tx: ba_d_tx,
        rel_tx: ba_r_tx,
        urg_tx: ba_u_tx,
        dgram_rx: ab_d_rx,
        rel_rx: ab_r_rx,
        urg_rx: ab_u_rx,
    };
    let ha = LoopHandle {
        own: own_a,
        cfg: Arc::clone(&cfg),
    };
    let hb = LoopHandle { own: own_b, cfg };
    (a, b, ha, hb)
}

impl LoopHandle {
    /// 预置一条下一拍 poll 要交付的事件（脚本 = 「更早到达」）。
    fn deliver(&self, ev: NetEvent) {
        self.own.lock().expect("own lock").deliver.push_back(ev);
    }

    /// 编码一条控制消息并入箱。
    fn deliver_reliable(&self, msg: &ControlMessage) {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.deliver(NetEvent::Reliable(buf));
    }

    /// 本端发出的数据报（按发送序）。
    fn datagrams_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").datagrams_sent.clone()
    }

    /// 本端经 reliable 流发出的原始消息（按发送序）。
    fn reliable_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").reliable_sent.clone()
    }

    /// 本端经 urgent 流发出的原始消息（按发送序）。
    fn urgent_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("own lock").urgent_sent.clone()
    }

    /// 链路状况注入：是否丢弃探测数据报。
    fn set_drop_bw_datagrams(&self, on: bool) {
        self.cfg.drop_bw_datagrams.store(on, Ordering::Relaxed);
    }

    /// 链路状况注入：每次 poll 最多放行的数据报数（0 = 不限）。
    fn set_max_datagrams_per_poll(&self, n: usize) {
        self.cfg.max_datagrams_per_poll.store(n, Ordering::Relaxed);
    }
}

impl Transport for LoopTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        self.own
            .lock()
            .expect("own lock")
            .datagrams_sent
            .push(data.to_vec());
        if self.cfg.drop_bw_datagrams.load(Ordering::Relaxed)
            && data.first() == Some(&TYPE_BW_PROBE_DATA)
        {
            return Ok(()); // 模拟探测流在途中全丢
        }
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

    /// 忽略 `timeout`（确定性优先，与 testkit::MockTransport 同纪律）：
    /// 脚本队列 → 数据报（受 pacing 上限）→ reliable → urgent，按序交付。
    fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
        let mut out: Vec<NetEvent> = {
            let mut own = self.own.lock().expect("own lock");
            own.deliver.drain(..).collect()
        };
        let cap = self.cfg.max_datagrams_per_poll.load(Ordering::Relaxed);
        let mut got = 0usize;
        while cap == 0 || got < cap {
            match self.dgram_rx.try_recv() {
                Ok(d) => {
                    out.push(NetEvent::Datagram(d));
                    got += 1;
                }
                Err(_) => break,
            }
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
// 会话组装与驱动
// ─────────────────────────────────────────────────────────────────

/// 会话号（固定值保证确定性）。
const SESSION_ID: u64 = 20_261_121;

/// 逻辑钟（µs）+ 由它派生的会话钟（u32 饱和视图）：探测泵拿 u64 本值、
/// 两侧管线拿 u32 视图——同源同纪元（B9），照 rc-loopback 单闭包模式。
fn logical_clock() -> (Arc<Mutex<u64>>, SessionClock) {
    let slot = Arc::new(Mutex::new(0u64));
    let s2 = Arc::clone(&slot);
    let clock: SessionClock =
        Arc::new(move || u32::try_from(*s2.lock().expect("clock lock")).unwrap_or(u32::MAX));
    (slot, clock)
}

/// 组装对接会话的公共配置。
fn session_params(bitrate_bps: u32) -> SessionParams {
    SessionParams {
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps,
    }
}

fn pcfg(bitrate_bps: u32) -> PipelineConfig {
    PipelineConfig {
        resolution: rc_types::Resolution::new(64, 64),
        fps: 30,
        bitrate_bps,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；真 QUIC 路径由 host 侧 expect_hello 按 N2 覆盖。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

fn fake_capture(content: FakeContent) -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: rc_types::Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 30,
            content,
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

/// 记录型编码器：set_bitrate 成功后写入共享记录（m1.rs 包装先例）。
struct RecordingEncoder {
    inner: Box<dyn Encoder>,
    bitrates: Arc<Mutex<Vec<u32>>>,
}

impl Encoder for RecordingEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        self.inner.encode(frame, force_idr, deadline_ts)
    }
    fn capabilities(&self) -> EncoderCaps {
        self.inner.capabilities()
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.inner.set_bitrate(bps)?;
        self.bitrates.lock().expect("record lock").push(bps);
        Ok(())
    }
    fn request_idr(&mut self) {
        self.inner.request_idr()
    }
    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        self.inner.flush()
    }
}

/// 空注入器（探测测试不关心输入）。
struct NoopInjector;
impl InputInjector for NoopInjector {
    fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
        Ok(())
    }
    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

/// 脚本型输入捕获（恒空：探测测试不发输入）。
struct EmptyCapture;
impl rc_input::InputCapture for EmptyCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

/// 一对经 LoopTransport 对接、握手完成的会话 + 断言句柄 + 会话钟。
struct Fixture {
    host: HostSession,
    client: ClientSession,
    host_handle: LoopHandle,
    client_handle: LoopHandle,
    clock: SessionClock,
}

/// 组装双会话（确定性握手顺序：预置 Hello 给 host → host 真握手 →
/// client 消费真 Welcome；与 session_roundtrip 同模式）。
fn paired_sessions(
    content: FakeContent,
    encoder: Box<dyn Encoder>,
    bitrate_bps: u32,
    clock: SessionClock,
) -> Fixture {
    let (ta, tb, ha, hb) = loop_pair();
    ha.deliver_reliable(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        session_id: SESSION_ID,
        requested: session_params(bitrate_bps),
        auth_method: AuthMethod::None,
        auth_payload: vec![],
    }));

    let mut host = HostSession::with_parts(
        HostSessionConfig {
            listen_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: session_params(bitrate_bps),
            pipeline: pcfg(bitrate_bps),
            handshake_timeout: Duration::from_millis(500),
            injector: Box::new(NoopInjector),
            pace_capture: true, // 假后端必须节流（坑 4）
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        Box::new(ta),
        fake_capture(content),
        encoder,
        Arc::clone(&clock),
    )
    .expect("host with_parts");
    host.handshake().expect("host handshake");

    let kept_clock = Arc::clone(&clock);
    let mut client = ClientSession::with_parts(
        ClientSessionConfig {
            peer_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            requested: session_params(bitrate_bps),
            session_id: SESSION_ID,
            pipeline: pcfg(bitrate_bps),
            connect_timeout: Duration::from_secs(1),
            handshake_timeout: Duration::from_millis(500),
            capture: Box::new(EmptyCapture),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        },
        Box::new(tb),
        Box::new(
            FakeDecoder::new(FakeDecoderConfig {
                resolution: rc_types::Resolution::new(64, 64),
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

    // clock 与两侧会话共享同一 Arc 闭包（B9 同源同纪元）；测试侧留一份
    // 供读钟驱动探测泵。
    Fixture {
        host,
        client,
        host_handle: ha,
        client_handle: hb,
        clock: kept_clock,
    }
}

/// 逻辑钟驱动一轮：推进 step µs → client 探测泵 → client 一拍 → host 一拍
/// （ping 先于 host 到达、pong 于下一轮被 client 收到——RTT = step，确定）。
fn drive_round(f: &mut Fixture, now_slot: &Arc<Mutex<u64>>, step_us: u64) -> u64 {
    let now = {
        let mut slot = now_slot.lock().expect("clock lock");
        *slot += step_us;
        *slot
    };
    f.client.pump_probes(now).expect("client pump_probes");
    f.client
        .tick(Duration::from_millis(0), Instant::now())
        .expect("client tick");
    f.host.tick(Duration::from_millis(0)).expect("host tick");
    now
}

/// 统计缓冲里以指定类型号开头的消息数。
fn count_type(bufs: &[Vec<u8>], ty: u8) -> usize {
    bufs.iter().filter(|b| b.first() == Some(&ty)).count()
}

/// 断言探测类消息（ProbePing 0x05 / ProbePong 0x06 / BwProbeRequest 0x08 /
/// BwProbeData 0x09 首字节）绝不出现在给定的（datagram/urgent）缓冲里。
fn assert_no_probe_messages(bufs: &[Vec<u8>], what: &str) {
    for b in bufs {
        let bad = matches!(b.first(), Some(0x05 | 0x06 | 0x08 | 0x09));
        assert!(
            !bad,
            "{what} 出现探测消息首字节 0x{:02X?}",
            &b[..b.len().min(4)]
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// 集成测试
// ─────────────────────────────────────────────────────────────────

/// B1/B2/B3：双会话对接，逻辑钟驱动 6s → host 收 ≥4 条 ProbePing 且各回
/// Pong，client 窗口 ≥4 样本；RTT/偏移公式与 `host_to_local_us` 换算被
/// 钉死（换算误差 0）；探测消息只出现在 reliable 通道。
#[test]
fn heartbeat_ping_pong_roundtrip_over_transport() {
    let (now_slot, clock) = logical_clock();
    let mut f = paired_sessions(FakeContent::Static, fake_encoder(), 8_000_000, clock);
    assert_eq!(f.host.state(), SessionState::Streaming);
    assert_eq!(f.client.state(), SessionState::Streaming);

    // 6s 逻辑钟，100ms 一拍：ping 于锚点后每整 1s 发出（1.1s..5.1s 共 5 条）。
    for _ in 0..60 {
        drive_round(&mut f, &now_slot, 100_000);
    }

    let snap = f.client.probe_snapshot();
    assert!(snap.pings_sent >= 4, "5s+ 逻辑时钟应产出 ≥4 条 ping");
    assert!(snap.pongs_received >= 4, "每条 ping 都应收到 pong");
    // 确定性链路时序：ping 在第 k 拍发出（t0）、host 同拍应答（t1 = t0）、
    // client 第 k+1 拍收到（t2 = t0 + 100ms）→ rtt = 100ms、
    // offset = t1 − (t0+t2)/2 = −50ms（NTP 中点法，误差 0）。
    assert_eq!(snap.rtt_us_median, Some(100_000), "rtt = t2 − t0");
    assert_eq!(snap.offset_us_median, Some(-50_000), "offset 中点法");
    // B3：host 侧时间戳换算（合成固定偏移，误差 0）。
    assert_eq!(
        f.client.host_to_local_us(1_000_000),
        Some(1_050_000),
        "local = remote − offset = remote + 50ms"
    );

    // 通道纪律（B1）：ping/pong 只走 reliable。
    assert!(
        count_type(&f.client_handle.reliable_sent(), TYPE_PROBE_PING) >= 4,
        "client 应经 reliable 发 ≥4 条 ProbePing"
    );
    assert!(
        count_type(&f.host_handle.reliable_sent(), TYPE_PROBE_PONG) >= 4,
        "host 应经 reliable 回 ≥4 条 ProbePong"
    );
    for (bufs, what) in [
        (&f.host_handle.urgent_sent(), "host urgent"),
        (&f.client_handle.urgent_sent(), "client urgent"),
        (&f.host_handle.datagrams_sent(), "host datagram"),
        (&f.client_handle.datagrams_sent(), "client datagram"),
    ] {
        assert!(
            bufs.is_empty() || what.ends_with("datagram"),
            "{what} 应为空（静屏 + 无输入）：{:?}",
            bufs.len()
        );
        assert_no_probe_messages(bufs, what);
    }
}

/// B5/B6：client `start_bw_probe` → host 经 datagram 逐块发出（pacing 16/
/// 拍模拟分批到达）→ client 结算（首末块间隔 150ms 注入）→ 发出
/// `SetBitrate{reason:1}` → `host.current_bitrate_bps()` == clamp 值。
#[test]
fn bw_probe_and_setbitrate_end_to_end() {
    let (now_slot, clock) = logical_clock();
    let mut f = paired_sessions(FakeContent::Static, fake_encoder(), 50_000_000, clock);
    // 链路带宽受限：每拍只放行 16 个数据报（256 块 = 16 拍）。
    f.client_handle.set_max_datagrams_per_poll(16);

    // Welcome 后发起一次；重复调用为空操作（只发一条请求）。
    f.client.start_bw_probe(0).expect("start_bw_probe");
    f.client.start_bw_probe(0).expect("重复调用为空操作");
    assert_eq!(
        count_type(&f.client_handle.reliable_sent(), 0x08),
        1,
        "BwProbeRequest 只发一条"
    );

    // 40 拍 × 10ms：host 第 1 拍收请求并发出 256 块；client 第 2..17 拍
    // 每拍收 16 块（首块 t=20ms、末块 t=170ms → span=150ms ≥ 1ms 不退化）。
    let mut applied = false;
    for _ in 0..40 {
        drive_round(&mut f, &now_slot, 10_000);
        if f.host.current_bitrate_bps() != 50_000_000 {
            applied = true;
            break;
        }
    }
    assert!(applied, "链路应在探测结算后收到 SetBitrate");

    // 估速（整数确定性）：256×1024 padding × 8 bit / 150_000µs。
    let est = probe::BW_PROBE_MAX_TOTAL_BYTES as u64 * 8 * 1_000_000 / 150_000;
    let want = initial_bitrate_from(Some(est), 50_000_000).expect("非退化估速必产出初始码率");
    assert_eq!(est, 13_981_013, "估速整数口径自检");
    assert_eq!(want, 9_786_709, "clamp(13_981_013×0.7, 2M, 50M)");
    assert!(want > 2_000_000 && want < 50_000_000, "不打到夹紧边界");

    assert_eq!(f.host.current_bitrate_bps(), want, "host 记账 = clamp 值");
    let snap = f.client.probe_snapshot();
    assert_eq!(snap.bw_estimate_bps, Some(est));
    assert_eq!(snap.initial_bitrate_bps, Some(want));
    assert_eq!(snap.negotiated_bitrate_bps, 50_000_000);

    // SetBitrate 恰一条、reason=Probe（0x07 解码断言）。
    let setbitrates: Vec<ControlMessage> = f
        .client_handle
        .reliable_sent()
        .iter()
        .filter(|b| b.first() == Some(&TYPE_SET_BITRATE))
        .map(|b| ControlMessage::decode(b).expect("client 发出的必可解"))
        .collect();
    assert_eq!(setbitrates.len(), 1, "SetBitrate 只发一条：{setbitrates:?}");
    assert_eq!(
        setbitrates[0],
        ControlMessage::SetBitrate {
            bps: want,
            reason: BitrateReason::Probe,
        }
    );
    // host 确实经 datagram 逐块发出（不占控制流）。
    assert_eq!(f.host_handle.datagrams_sent().len(), 256);
    assert_no_probe_messages(&f.host_handle.reliable_sent(), "host reliable");
}

/// host 侧管线用记录型编码器 → SetBitrate 分发臂 → HostPipeline::
/// set_bitrate 命令通道 → 编码线程实际收到下发值（≤2s 有界等待，真实
/// 线程，m1.rs 同形态）；bps=0 只记错不拆会话、记账不变。
#[test]
fn setbitrate_reaches_recording_encoder() {
    let (_now_slot, clock) = logical_clock();
    let bitrates = Arc::new(Mutex::new(Vec::new()));
    let encoder: Box<dyn Encoder> = Box::new(RecordingEncoder {
        inner: fake_encoder(),
        bitrates: Arc::clone(&bitrates),
    });
    let mut f = paired_sessions(FakeContent::Static, encoder, 8_000_000, clock);
    assert_eq!(f.host.current_bitrate_bps(), 8_000_000, "初始 = Welcome 值");

    // 正常路径：3.5Mbps 经分发臂下发。
    f.host_handle.deliver_reliable(&ControlMessage::SetBitrate {
        bps: 3_500_000,
        reason: BitrateReason::Probe,
    });
    f.host.tick(Duration::from_millis(0)).expect("tick");
    assert_eq!(f.host.current_bitrate_bps(), 3_500_000, "会话侧记账即时");

    // 有界等待编码线程应用（命令通道 ≤5ms 量级，2s 上限防挂死）。
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if bitrates.lock().expect("record lock").contains(&3_500_000) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "编码线程 2s 内未应用 SetBitrate：{:?}",
            bitrates.lock().expect("record lock")
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 非法值路径：bps=0 → 管线本地拒绝、只记 stderr、不拆会话、记账不变。
    f.host_handle.deliver_reliable(&ControlMessage::SetBitrate {
        bps: 0,
        reason: BitrateReason::Manual,
    });
    f.host
        .tick(Duration::from_millis(0))
        .expect("bps=0 记错不拆会话");
    assert_eq!(f.host.current_bitrate_bps(), 3_500_000, "记账不变");
    assert_eq!(f.host.state(), SessionState::Streaming);
    std::thread::sleep(Duration::from_millis(20)); // 反证窗口
    assert_eq!(
        *bitrates.lock().expect("record lock"),
        vec![3_500_000],
        "编码器只收到合法值，0 被拒于命令入队前"
    );
}

/// B4：同通道混合媒体帧与探测块 → ControllerPipeline 照常收帧上屏，
/// 探测块不触发 push_datagram 的 Protocol 错（会话不断、驱动循环不中断）。
#[test]
fn bw_datagram_does_not_leak_into_pipeline() {
    // 媒体链路用墙钟会话零点（照 session_roundtrip 的 origin_slot 模式）。
    let origin: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&origin);
    let clock: SessionClock = Arc::new(move || match *slot.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    });
    let mut f = paired_sessions(FakeContent::Dynamic, fake_encoder(), 8_000_000, clock);

    // Welcome 后立刻发起粗测：host 的 256 个探测块与媒体数据报同通道混合。
    f.client
        .start_bw_probe(u64::from((f.clock)()))
        .expect("start_bw_probe");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        f.client
            .pump_probes(u64::from((f.clock)()))
            .expect("pump_probes");
        f.client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("client tick：探测块绝不触发 Protocol 错（B4）");
        f.host.tick(Duration::from_millis(0)).expect("host tick");
        {
            let mut o = origin.lock().expect("origin lock");
            if o.is_none() {
                *o = f.host.pipeline().first_capture();
            }
        }
        if f.client.pipeline().displayed() > 0 {
            break;
        }
        f.host.pipeline().wait_event(Duration::from_millis(2));
    }
    assert!(
        f.client.pipeline().displayed() > 0,
        "混合探测块与媒体帧的通道下 ControllerPipeline 照常收帧上屏"
    );
    assert_eq!(f.client.state(), SessionState::Streaming, "会话不断");
    assert_eq!(f.host.state(), SessionState::Streaming);

    // 前提自证：host 的 datagram 通道确实同时承载了媒体与探测块。
    let sent = f.host_handle.datagrams_sent();
    let probe_chunks = count_type(&sent, TYPE_BW_PROBE_DATA);
    let media = sent
        .iter()
        .filter(|b| b.first() == Some(&0x52)) // rc-proto MAGIC LE 首字节
        .count();
    assert_eq!(probe_chunks, 256, "探测块经 datagram 通道逐块发出");
    assert!(media > 0, "媒体帧与探测块同通道混合（B4 前提）");
}

/// B6 反向：LoopTransport 丢弃探测数据报 → 超时结算 est=None → 不发
/// SetBitrate、host 码率保持 Welcome 值。
#[test]
fn probe_failure_keeps_negotiated_bitrate() {
    let (now_slot, clock) = logical_clock();
    let mut f = paired_sessions(FakeContent::Static, fake_encoder(), 8_000_000, clock);
    f.host_handle.set_drop_bw_datagrams(true);

    f.client.start_bw_probe(0).expect("start_bw_probe");
    // 3s 逻辑钟（> 2s 结算超时）：零实收 → 超时结算 est=None。
    for _ in 0..30 {
        drive_round(&mut f, &now_slot, 100_000);
    }

    let snap = f.client.probe_snapshot();
    assert_eq!(snap.bw_estimate_bps, None, "零实收 → est=None");
    assert_eq!(snap.initial_bitrate_bps, None, "未下发初始码率");
    assert_eq!(snap.negotiated_bitrate_bps, 8_000_000);
    assert_eq!(
        count_type(&f.client_handle.reliable_sent(), TYPE_SET_BITRATE),
        0,
        "探测失败绝不发 SetBitrate（B6）"
    );
    assert_eq!(
        f.host.current_bitrate_bps(),
        8_000_000,
        "host 码率保持 Welcome 协商值"
    );
    // 前提自证：host 确实发出了 256 块（链路丢的是传输，不是没发）。
    assert_eq!(f.host_handle.datagrams_sent().len(), 256);
    assert_eq!(f.client.state(), SessionState::Streaming);
}
