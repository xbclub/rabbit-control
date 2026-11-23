//! 真 QUIC over UDP 回环集成测试（无需权限，CI 可跑；证书照 rc-net
//! tests/certs 模式复制一份，仅测试用）。
//!
//! 覆盖：真 `QuicEndpoint::accept/connect`（127.0.0.1:0）→ 会话握手 →
//! 假后端流媒体 → client `close(Normal)` 的优雅关闭，以及 client 不发
//! Bye 直接断开时 host 的有序停机；大帧（64KB，多包）在 N2 数据报预算
//! 下的端到端投递（T18 空档关闭）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_capture::{
    CaptureBackend, CapturedFrame, FakeBackend, FakeBackendConfig, FakeContent, FrameTexture,
};
use rc_decode::{DecodeOutput, DecodedFrame, Decoder};
use rc_encode::{EncodedFrame, Encoder, EncoderCaps};
use rc_input::{InputEvent, InputInjector};
use rc_net::quic::{max_media_payload, DEFAULT_MTU};
use rc_pipeline::{ControllerPipeline, DiffMode, HostPipeline, PipelineConfig};
use rc_session::proto::{ByeReason, SessionParams};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, SessionClock, SessionState,
    StopReason,
};
use rc_types::{CpuFrame, FrameKind, PixelFormat, Resolution, Result, Timestamp};

/// localhost 测试自签证书（SAN：rabbitcontrol / localhost / 127.0.0.1，
/// 与 rc-net 的测试证书同模式；**仅测试用**，不构成安全边界）。
const SERVER_CERT_PEM: &str = include_str!("certs/server-cert.pem");
const SERVER_KEY_PEM: &str = include_str!("certs/server-key.pem");

/// 握手 / 收帧的宽松上限：本地回环上真实耗时都在毫秒级，只防挂死。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// 会话号（固定值保证确定性）。
const SESSION_ID: u64 = 424_242;

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
        // CI / 无权限环境：工厂选假后端（不碰 SCK / VT / Metal）。
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；host 侧 expect_hello 握手时按 N2 覆盖为 1120
        //（max_media_payload(DEFAULT_MTU)）——大帧多包分片的数据报必须
        // 装进 rc-net 的 N2 预算，否则 send_datagram 拒收（T18 空档）。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

/// 记录型注入器（host 侧，诊断用）。
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

/// 空捕获（client 侧：本测试不注入输入）。
struct IdleCapture;

impl rc_input::InputCapture for IdleCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

/// 会话零点槽（照 run_real 的 origin_slot 模式，坑 3）。
type OriginSlot = Arc<Mutex<Option<Instant>>>;

fn origin_clock(origin: OriginSlot) -> SessionClock {
    Arc::new(move || match *origin.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    })
}

/// 真网络组建一对会话并完成握手，返回 (host, client, 零点槽)。
fn quic_pair() -> (HostSession, ClientSession, OriginSlot) {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));

    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(RecordingInjector {
                events: Arc::new(Mutex::new(Vec::new())),
            }),
            pace_capture: true, // 工厂假后端必须节流（坑 4）
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        Arc::clone(&clock),
    )
    .expect("host endpoint");
    let host_addr = host.local_addr().expect("host local_addr");

    let accepter = thread::spawn(move || {
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("host accept_and_handshake");
        host
    });
    let mut client = ClientSession::new(
        ClientSessionConfig {
            peer_addr: host_addr,
            mtu: DEFAULT_MTU,
            requested: params(),
            session_id: SESSION_ID,
            pipeline: pcfg(),
            connect_timeout: CONNECT_TIMEOUT,
            handshake_timeout: Duration::from_secs(5),
            capture: Box::new(IdleCapture),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        },
        // 本端端点身份（rc-net 的端点构造对 Controller 角色也要求身份；
        // 对端不校验客户端证书，测试两端共用同一份自签身份）。
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        SERVER_CERT_PEM,
        "rabbitcontrol",
        Arc::clone(&clock),
    )
    .expect("client session");
    client
        .connect_and_handshake()
        .expect("client connect_and_handshake");
    let host = accepter.join().expect("accepter thread");
    assert_eq!(host.state(), SessionState::Streaming);
    assert_eq!(client.state(), SessionState::Streaming);
    // 会话零点锚首个采集帧（坑 3）；尚未采到帧时锚当前时刻（偏晚安全：
    // 只会让 now 落后于帧网格，不会把帧误判超期）。
    {
        let mut slot = origin.lock().expect("origin lock");
        if slot.is_none() {
            *slot = host.pipeline().first_capture();
        }
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }
    (host, client, origin)
}

/// 真 `QuicEndpoint::accept/connect`（127.0.0.1:0）→ 握手 → 假后端流
/// 1–2 秒 → client `frames_displayed > 0` → client `close(Normal)` →
/// 双端 Closed、`unfinished==0`。
#[test]
fn quic_handshake_streams_and_closes_over_loopback() {
    let (host, mut client, _origin) = quic_pair();

    // host 在子线程 run；主线程驱动 client。
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_host = Arc::clone(&stop);
    let mut host = host;
    let host_thread = thread::spawn(move || {
        let summary = host
            .run(&mut || stop_for_host.load(Ordering::Relaxed))
            .expect("host run");
        (summary, host)
    });

    // client 流到有帧上屏（真 QUIC 回环，毫秒级；上限只防挂死）。
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        client
            .tick(Duration::from_millis(5), Instant::now())
            .expect("client tick");
        if client.pipeline().displayed() > 0 {
            break;
        }
    }
    assert!(
        client.pipeline().displayed() > 0,
        "真 QUIC 回环应有帧上屏（实际 {}）",
        client.pipeline().displayed()
    );

    // client 优雅关闭 → host 观测 Bye 并停机。
    let client_summary = client.close(ByeReason::Normal).expect("client close");
    assert_eq!(client.state(), SessionState::Closed);
    // 给 host 一点时间处理 Bye（回环 µs 级；这里只是让 stop 兜底不抢跑）。
    thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    let (host_summary, host) = host_thread.join().expect("host thread");

    assert_eq!(host.state(), SessionState::Closed);
    assert_eq!(
        host_summary.stop_reason,
        StopReason::ByeReceived(ByeReason::Normal),
        "host 应观测到 client 的 Bye(Normal)"
    );
    assert_eq!(
        host_summary.unfinished_traces, 0,
        "host 停机后未结帧必须为 0"
    );
    assert_eq!(
        client_summary.unfinished_traces, 0,
        "client 停机后未结帧必须为 0"
    );
    assert!(
        client.pipeline().trace().summary().unfinished == 0,
        "client trace 必须全部有结局"
    );
}

/// client 直接 drop transport（不发 Bye）→ host 观测 `Disconnected`。
///
/// T17 语义：断线不再等价于会话结束——host 进 Detached 保留管线（保持
/// 窗口内短超时 accept 等重连），run() 不退出；stop 置位后经 close(Normal)
/// 按红线拆除（stop_reason=Requested，不再是 Disconnected）、`unfinished==0`、
/// 不 panic 不挂死。
#[test]
fn quic_host_survives_client_abrupt_disconnect() {
    let (host, client, _origin) = quic_pair();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_host = Arc::clone(&stop);
    let mut host = host;
    let host_thread = thread::spawn(move || {
        let summary = host
            .run(&mut || stop_for_host.load(Ordering::Relaxed))
            .expect("host run");
        (summary, host)
    });

    // 流一小会儿后 client 整会话直接丢弃（QuicTransport Drop 关闭连接，
    // 不发 Bye）。
    let mut client = client;
    let deadline = Instant::now() + Duration::from_millis(600);
    while Instant::now() < deadline {
        client
            .tick(Duration::from_millis(5), Instant::now())
            .expect("client tick");
    }
    drop(client);

    // host 观测 Disconnected（回环上 CONNECTION_CLOSE 即刻可达）→ Detached
    // 保持窗口；无重连到达，stop 后退出并按红线拆除。
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    let (host_summary, host) = host_thread.join().expect("host 线程必须退出（不挂死）");

    assert_eq!(host.state(), SessionState::Closed);
    assert_eq!(
        host_summary.stop_reason,
        StopReason::Requested,
        "T17：Detached 期退出走 close(Normal)，不再是 Disconnected"
    );
    assert_eq!(
        host.reconnect_stats().disconnects,
        1,
        "真实断线必须被计数（T17 观测点）"
    );
    assert_eq!(
        host_summary.unfinished_traces, 0,
        "断线后的有序停机账目必须闭合"
    );
}

// ════════════════════════════════════════════════════════════════
// 大帧（64KB，多包）在 N2 数据报预算下的端到端投递——T18 已知空档的
// 关闭验证：分片预算经 `expect_hello` 覆盖为 `max_media_payload(mtu)`
// （1120）后，多包大帧的每条数据报都能过真 QUIC `send_datagram`，
// 接收端逐字节重组成功、会话正常收尾。
// ════════════════════════════════════════════════════════════════

/// 大帧载荷字节数（≥50KB，预算 1120 下 = 59 包/帧）。
const BIG_FRAME_BYTES: usize = 65_536;

/// 大帧编码器第 `idx` 帧载荷第 `i`（≥8）字节的确定性合成值。
fn big_pattern_byte(idx: u64, i: usize) -> u8 {
    (idx.wrapping_mul(131).wrapping_add(i as u64) & 0xff) as u8
}

/// 大帧编码器：每帧恒 [`BIG_FRAME_BYTES`] 字节；前 8 字节 = 帧号 LE，
/// 其余按 [`big_pattern_byte`] 合成。编码瞬时完成（E5 恒满足）。
struct BigPatternEncoder {
    encoded: u64,
}

impl Encoder for BigPatternEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        _deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        let is_keyframe = force_idr || self.encoded == 0;
        let mut data = vec![0u8; BIG_FRAME_BYTES];
        data[..8].copy_from_slice(&self.encoded.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate().skip(8) {
            *b = big_pattern_byte(self.encoded, i);
        }
        self.encoded += 1;
        Ok(EncodedFrame {
            data,
            kind: if is_keyframe {
                FrameKind::Idr
            } else {
                FrameKind::P
            },
            is_keyframe,
            encode_done_ts: frame.timestamp,
        })
    }
    fn capabilities(&self) -> EncoderCaps {
        EncoderCaps {
            codec: rc_encode::Codec::H264,
            hardware: false,
            max_resolution: Resolution::new(4096, 4096),
            bitstream_format: rc_encode::BitstreamFormat::AnnexB,
            profiles: &[66],
        }
    }
    fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
        Ok(())
    }
    fn request_idr(&mut self) {}
    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// 校验型解码器：收到完整重组帧后逐字节比对合成模式，记录帧号。
/// 任何失真直接 panic（测试失败）——「逐字节重组成功」的强断言。
struct VerifyingDecoder {
    received: Arc<Mutex<Vec<u64>>>,
}

impl Decoder for VerifyingDecoder {
    fn decode(
        &mut self,
        data: &[u8],
        _kind: FrameKind,
        _deadline_ts: Timestamp,
    ) -> Result<DecodeOutput> {
        assert_eq!(
            data.len(),
            BIG_FRAME_BYTES,
            "重组帧长度必须等于 {}（实际 {}）",
            BIG_FRAME_BYTES,
            data.len()
        );
        let idx = u64::from_le_bytes(data[..8].try_into().expect("8 字节帧号"));
        for (i, &b) in data.iter().enumerate().skip(8) {
            assert_eq!(
                b,
                big_pattern_byte(idx, i),
                "帧 {idx} 第 {i} 字节重组失真（期望 {}，实际 {b}）",
                big_pattern_byte(idx, i)
            );
        }
        self.received.lock().expect("received lock").push(idx);
        Ok(DecodeOutput::Frame(DecodedFrame {
            texture: FrameTexture::Cpu(CpuFrame {
                width: 4,
                height: 4,
                format: PixelFormat::Bgra8,
                stride: 16,
                data: vec![0; 64],
            }),
            decode_done_ts: Timestamp(0),
            width: 4,
            height: 4,
        }))
    }
    fn needs_keyframe(&self) -> bool {
        false
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// 大帧工厂用的假采集（64×64@30 动态内容）。
fn big_fake_capture() -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            fps: 30,
            content: FakeContent::Dynamic,
            ..FakeBackendConfig::default()
        })
        .expect("fake capture"),
    )
}

/// 照 [`quic_pair`] 组一对真 QUIC 会话，但两端注入大帧工厂：
/// host 侧 [`BigPatternEncoder`]（64KB/帧），client 侧 [`VerifyingDecoder`]。
fn big_frame_pair(received: Arc<Mutex<Vec<u64>>>) -> (HostSession, ClientSession, OriginSlot) {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));

    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(RecordingInjector {
                events: Arc::new(Mutex::new(Vec::new())),
            }),
            pace_capture: true, // 工厂假后端必须节流（坑 4）
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        Arc::clone(&clock),
    )
    .expect("host endpoint");
    // 大帧工厂：闭包收到的是 expect_hello 按 N2 覆盖后的 pcfg——
    // 此处断言会话层预算确实生效（修复的第 2 环）。
    host.set_pipeline_factory_for(Box::new(|pcfg: &PipelineConfig| {
        assert_eq!(
            pcfg.max_packet_payload,
            max_media_payload(DEFAULT_MTU),
            "会话层必须把分片预算覆盖为 max_media_payload(mtu)"
        );
        HostPipeline::with_backends_paced(
            pcfg.clone(),
            big_fake_capture(),
            Box::new(BigPatternEncoder { encoded: 0 }),
            true,
        )
    }));
    let host_addr = host.local_addr().expect("host local_addr");

    let accepter = thread::spawn(move || {
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("host accept_and_handshake");
        host
    });

    let mut client = ClientSession::new(
        ClientSessionConfig {
            peer_addr: host_addr,
            mtu: DEFAULT_MTU,
            requested: params(),
            session_id: SESSION_ID,
            pipeline: pcfg(),
            connect_timeout: CONNECT_TIMEOUT,
            handshake_timeout: Duration::from_secs(5),
            capture: Box::new(IdleCapture),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: rc_session::ClientAuthConfig::default(),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        SERVER_CERT_PEM,
        "rabbitcontrol",
        Arc::clone(&clock),
    )
    .expect("client session");
    let received_for_factory = Arc::clone(&received);
    client.set_pipeline_factory_for(Box::new(
        move |pcfg: &PipelineConfig| -> Result<ControllerPipeline> {
            ControllerPipeline::with_backends(
                pcfg.clone(),
                Box::new(VerifyingDecoder {
                    received: Arc::clone(&received_for_factory),
                }),
                Box::new(
                    rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
                        size: (64, 64),
                        ..rc_render::FakeRendererConfig::default()
                    })
                    .expect("fake renderer"),
                ),
            )
        },
    ));
    client
        .connect_and_handshake()
        .expect("client connect_and_handshake");
    let host = accepter.join().expect("accepter thread");
    assert_eq!(host.state(), SessionState::Streaming);
    assert_eq!(client.state(), SessionState::Streaming);
    // 会话零点锚首个采集帧（坑 3）。
    {
        let mut slot = origin.lock().expect("origin lock");
        if slot.is_none() {
            *slot = host.pipeline().first_capture();
        }
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }
    (host, client, origin)
}

/// T18 空档关闭：64KB 多包大帧经真 QUIC 回环端到端——全部数据报被
/// `send_datagram` 接受（发出数 == 每帧包数 × 编码帧数）、client 逐字节
/// 重组校验通过、双端优雅收尾（Closed / unfinished==0）。
#[test]
fn quic_multi_packet_large_frames_survive_n2_budget() {
    // 前置：预算几何。默认 MTU 下媒体片预算 1120，64KB 帧 = 59 包。
    let budget = max_media_payload(DEFAULT_MTU);
    assert_eq!(budget, 1120);
    let packets_per_frame = BIG_FRAME_BYTES.div_ceil(budget);
    assert_eq!(packets_per_frame, 59, "64KB/1120 必须是 59 包的多包帧");

    let received: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let (host, mut client, _origin) = big_frame_pair(Arc::clone(&received));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_host = Arc::clone(&stop);
    let mut host = host;
    let host_thread = thread::spawn(move || {
        let summary = host
            .run(&mut || stop_for_host.load(Ordering::Relaxed))
            .expect("host run");
        (summary, host)
    });

    // client 流到足够多大帧完整上屏（回环毫秒级；上限只防挂死）。
    let want_frames: u64 = 8;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        client
            .tick(Duration::from_millis(5), Instant::now())
            .expect("client tick");
        if received.lock().expect("received lock").len() as u64 >= want_frames {
            break;
        }
    }
    let mut frames = received.lock().expect("received lock").clone();
    assert!(
        frames.len() as u64 >= want_frames,
        "真 QUIC 回环应投递 ≥{want_frames} 个完整 64KB 帧（实际 {}）——\
         多包数据报疑似被 N2 预算拒收",
        frames.len()
    );

    // 优雅收尾：client Bye(Normal) → host 观测并有序停机。
    let client_summary = client.close(ByeReason::Normal).expect("client close");
    assert_eq!(client.state(), SessionState::Closed);
    thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::Relaxed);
    let (host_summary, host_after) = host_thread.join().expect("host thread");

    assert_eq!(host_after.state(), SessionState::Closed, "host 必须 Closed");
    assert_eq!(
        host_summary.stop_reason,
        StopReason::ByeReceived(ByeReason::Normal)
    );
    assert_eq!(host_summary.unfinished_traces, 0, "host 停机账目闭合");
    assert_eq!(client_summary.unfinished_traces, 0, "client 停机账目闭合");

    // 全部数据报被接受：发出数 == 每帧包数 × 编码帧数。旧缺陷下
    // （预算 1200 → 数据报 1232B > N2 的 1152B）首条媒体数据报即被
    // send_datagram 拒收，发出数恒为 0——本断言即回归杀手。
    assert_eq!(
        host_summary.datagrams_sent as u64,
        packets_per_frame as u64 * host_summary.frames_encoded,
        "每个编码帧的 {} 条数据报必须全部被 send_datagram 接受",
        packets_per_frame
    );

    // 逐字节重组已在 VerifyingDecoder 内强校验；帧号必须无缺失、无重复
    //（回环无损：收到的帧号是 0..n 的连续前缀）。
    frames.sort_unstable();
    frames.dedup();
    for (i, &idx) in frames.iter().enumerate() {
        assert_eq!(idx, i as u64, "帧号必须从 0 起连续（缺失/乱序即丢包）");
    }
    assert!(
        frames.len() as u64 >= want_frames,
        "连续完整帧数必须 ≥{want_frames}"
    );
}
