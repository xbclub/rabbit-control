//! T17 真 QUIC 断线重连集成测试（127.0.0.1 回环，无需权限，CI 可跑；
//! 证书沿用 T14 建的 tests/certs，仅测试用、不构成安全边界）。
//!
//! # harness 形态
//!
//! host 与 client **各一个驱动线程**（照 T18 的双进程形态；不能单线程
//! 交错驱动：`connect`/`accept` 是阻塞式，client 阻塞 connect 期间 host
//! 得不到驱动，QUIC 服务端握手等应用层 accept，会互相饿死）。host 侧
//! Streaming→`tick` / Detached→`tick_detached`；client 侧 Streaming→
//! `tick`+`pump_probes` / Reconnecting→`tick_reconnect`。host 经
//! `Arc<Mutex>` 共享给主线程做观测。
//!
//! 覆盖三个场景（与任务卡时序图一一对应）：
//!
//! 1. 传输层意外断开 → host Detached 保持管线 → client 指数退避自动重连
//!    （缩阈值 50ms）→ resume 复用管线（FrameSeq 延续）→ 恢复后首个到达
//!    client 的数据报是 IDR 且 500ms 内到达；
//! 2. rebind 路径迁移（N9 透传）：client 端点换 socket，会话不断流、状态机
//!    不迁移、host `path_changes ≥ 1`（照 rc-net loopback 的
//!    `connection_survives_rebind_migration` 范式搬到会话层）；
//! 3. 时钟饱和守卫（注入 3s 阈值）：host 发 Bye(Refresh) 拆管线 → client
//!    零退避全新重连 → 新 session_id、Welcome bit1=1、新会话 FrameSeq
//!    从头、双端时钟读数回到小值。

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_net::quic::{Identity, QuicEndpoint, DEFAULT_MTU};
use rc_net::{NetConfig, NetEvent, NetStats, Role, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_session::proto::{ByeReason, SessionParams};
use rc_session::{
    ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, ReconnectConfig,
    SessionClock, SessionState,
};
use rc_types::{Resolution, Result};

/// localhost 测试自签证书（SAN：rabbitcontrol / localhost / 127.0.0.1，
/// 与 rc-net 的测试证书同模式；**仅测试用**，不构成安全边界）。
const SERVER_CERT_PEM: &str = include_str!("certs/server-cert.pem");
const SERVER_KEY_PEM: &str = include_str!("certs/server-key.pem");

/// 握手 / 收帧的宽松上限：本地回环上真实耗时都在毫秒级，只防挂死。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

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
        // 模板默认预算；真 QUIC 路径由 host 侧 expect_hello 按 N2 覆盖。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

/// 记录型注入器（host 侧）。
struct RecordingInjector;

impl rc_input::InputInjector for RecordingInjector {
    fn inject(&mut self, _events: &[rc_input::InputEvent]) -> Result<()> {
        Ok(())
    }
    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

/// 空捕获（client 侧：本测试不注入输入）。
struct IdleCapture;

impl rc_input::InputCapture for IdleCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<rc_input::InputEvent>> {
        Ok(Vec::new())
    }
}

/// 会话零点槽（照 run_real 的 origin_slot 模式；刷新测试还会把槽归零）。
type OriginSlot = Arc<Mutex<Option<Instant>>>;

fn origin_clock(origin: OriginSlot) -> SessionClock {
    Arc::new(move || match *origin.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    })
}

/// 建一个真网络 host（127.0.0.1:0，auto 假后端）。
fn new_host(clock: &SessionClock) -> HostSession {
    HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(RecordingInjector),
            pace_capture: true, // 工厂假后端必须节流（坑 4）
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        Arc::clone(clock),
    )
    .expect("host endpoint")
}

/// 建一个真网络 client（端点由会话内建并留存；rebind 测试经 `endpoint()` 拿）。
fn new_client(host_addr: SocketAddr, session_id: u64, clock: &SessionClock) -> ClientSession {
    ClientSession::new(
        ClientSessionConfig {
            peer_addr: host_addr,
            mtu: DEFAULT_MTU,
            requested: params(),
            session_id,
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
        Arc::clone(clock),
    )
    .expect("client session")
}

/// 共享 host（驱动线程持有 + 主线程观测）。
type SharedHost = Arc<Mutex<HostSession>>;

/// host 驱动线程：Streaming→tick / Detached→tick_detached，直到 stop 置位
/// 或非驱动态。返回共享句柄与线程 JoinHandle。
fn spawn_host_driver(
    host: HostSession,
    clock: SessionClock,
    stop: Arc<AtomicBool>,
) -> (SharedHost, thread::JoinHandle<()>) {
    let shared: SharedHost = Arc::new(Mutex::new(host));
    let shared2 = Arc::clone(&shared);
    let handle = thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let mut h = shared2.lock().expect("host lock");
            match h.state() {
                SessionState::Streaming => {
                    if h.tick(Duration::from_millis(1)).is_err() {
                        break;
                    }
                }
                SessionState::Detached => {
                    if h.tick_detached(u64::from((clock)())).is_err() {
                        break;
                    }
                }
                _ => break,
            }
            drop(h);
            thread::sleep(Duration::from_millis(1));
        }
    });
    (shared, handle)
}

/// 主线程驱动 client 一拍（按状态分发；时间注入自会话钟）。
fn client_step(client: &mut ClientSession, clock: &SessionClock) -> Result<()> {
    let now_us = u64::from((clock)());
    match client.state() {
        SessionState::Streaming => {
            client.pump_probes(now_us)?;
            client.tick(Duration::from_millis(1), Instant::now())?;
        }
        SessionState::Reconnecting => {
            client.tick_reconnect(now_us)?;
            // 退避等待拍不阻塞（Waiting 立即返回）：小睡防自旋。
            thread::sleep(Duration::from_millis(1));
        }
        _ => {}
    }
    Ok(())
}

/// host accept + 双端握手（accept 线程 + 主线程 connect；`host_addr` 与
/// client 配置的 peer_addr 一致由调用方保证）。
fn handshake_pair(host: HostSession, mut client: ClientSession) -> (HostSession, ClientSession) {
    let mut host = host;
    let accepter = thread::spawn(move || {
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("host accept_and_handshake");
        host
    });
    client.connect_and_handshake().expect("client handshake");
    let host = accepter.join().expect("accepter thread");
    (host, client)
}

/// 会话零点锚定（照 quic_loopback 的做法：优先锚首个采集帧）。
fn anchor_origin(origin: &OriginSlot, host: &HostSession) {
    let mut slot = origin.lock().expect("origin lock");
    if slot.is_none() {
        *slot = host.pipeline().first_capture();
    }
    if slot.is_none() {
        *slot = Some(Instant::now());
    }
}

// ─────────────────────────────────────────────────────────────────
// RecordingTransport：记录 client 侧 poll 到达的全部媒体数据报
//（「恢复后首个到达的数据报是 IDR」的观测点——会话内部状态不可透视，
// 包一层 transport 是最小侵入的替代）
// ─────────────────────────────────────────────────────────────────

type Recorder = Arc<Mutex<Vec<Vec<u8>>>>;

/// 记录型 connect 工厂的返回件（工厂 + 记录器 + connect 计数）。
type RecordingFactory = (
    Box<dyn FnMut() -> Result<Box<dyn Transport>> + Send>,
    Recorder,
    Arc<AtomicU32>,
);

struct RecordingTransport {
    inner: Box<dyn Transport>,
    recorder: Recorder,
}

impl Transport for RecordingTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_datagram(data)
    }
    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_reliable(data)
    }
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        self.inner.send_reliable_urgent(data)
    }
    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
        let events = self.inner.poll(timeout)?;
        for ev in &events {
            if let NetEvent::Datagram(d) = ev {
                self.recorder.lock().expect("recorder").push(d.clone());
            }
        }
        Ok(events)
    }
    fn stats(&self) -> NetStats {
        self.inner.stats()
    }
}

/// 记录型 connect 工厂：每次都经**同一个测试端点** connect（R3「重连
/// 复用同一 QuicEndpoint」的可观测形态），transport 包一层记录器。
/// 返回 (工厂, 记录器, connect 计数)。
fn recording_connect_factory(host_addr: SocketAddr) -> RecordingFactory {
    let ident = Identity::from_pem(SERVER_CERT_PEM, SERVER_KEY_PEM).expect("identity");
    let net = NetConfig {
        role: Role::Controller,
        local_addr: "0.0.0.0:0".parse().expect("0.0.0.0:0"),
        peer_addr: Some(host_addr),
        mtu: DEFAULT_MTU,
    };
    let endpoint = Arc::new(QuicEndpoint::new(&net, &ident).expect("client endpoint"));
    let certs = Identity::certs_from_pem(SERVER_CERT_PEM).expect("certs");
    let recorder: Recorder = Arc::new(Mutex::new(Vec::new()));
    let connects = Arc::new(AtomicU32::new(0));
    let f = {
        let endpoint = Arc::clone(&endpoint);
        let recorder = Arc::clone(&recorder);
        let connects = Arc::clone(&connects);
        Box::new(move || {
            connects.fetch_add(1, Ordering::SeqCst);
            let t = endpoint.connect(host_addr, &certs, "rabbitcontrol", CONNECT_TIMEOUT)?;
            Ok(Box::new(RecordingTransport {
                inner: Box::new(t),
                recorder: Arc::clone(&recorder),
            }) as Box<dyn Transport>)
        })
    };
    (f, recorder, connects)
}

/// 记录器里的媒体数据报（首字节 = rc-proto MAGIC 的 LE 首字节 0x52）。
fn media_datagrams(recorder: &Recorder) -> Vec<Vec<u8>> {
    recorder
        .lock()
        .expect("recorder")
        .iter()
        .filter(|d| d.first() == Some(&0x52))
        .cloned()
        .collect()
}

// ─────────────────────────────────────────────────────────────────
// 场景 A：断线 → 自动重连 → resume 复用管线 + 强制 IDR
// ─────────────────────────────────────────────────────────────────

/// 建立会话流数帧 → drop client 传输（Drop 即 close(0)，host 立刻收到
/// Disconnected）→ host Detached → client 以缩阈值退避（50ms）自动重连 →
/// `resumes ≥ 1`、host generation 不变、恢复后首个到达 client 的数据报是
/// IDR 且 `frame_seq` 延续、重连尝试到 IDR 到达 ≤ 500ms。
#[test]
fn reconnect_over_real_quic_resumes_with_idr() {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));
    let host = new_host(&clock);
    let host_addr = host.local_addr().expect("host addr");
    let mut client = new_client(host_addr, 42, &clock);
    // 连接来源走记录工厂（同端点复用 + 到达数据报可观测）。
    let (factory, recorder, connects) = recording_connect_factory(host_addr);
    client.set_connect_factory(factory);
    // 缩阈值退避：50ms 起步（验收线「恢复 500ms 内到达」的前提）。
    let rcfg = ReconnectConfig {
        backoff_initial: Duration::from_millis(50),
        ..ReconnectConfig::default()
    };
    client.set_reconnect(rcfg).expect("set_reconnect");

    let (host, mut client) = handshake_pair(host, client);
    anchor_origin(&origin, &host);
    assert_eq!(host.state(), SessionState::Streaming);
    assert_eq!(client.state(), SessionState::Streaming);
    assert_eq!(connects.load(Ordering::SeqCst), 1, "首次连接经工厂");

    let stop = Arc::new(AtomicBool::new(false));
    let (shared_host, host_thread) = spawn_host_driver(host, Arc::clone(&clock), Arc::clone(&stop));

    // 流到至少 3 帧上屏（前提：有 P 帧在 IDR 之后）。
    let deadline = Instant::now() + Duration::from_secs(8);
    while client.pipeline().displayed() < 3 {
        assert!(Instant::now() < deadline, "预热超时（管线饿死）");
        client_step(&mut client, &clock).expect("client step");
    }
    let displayed_at_drop = client.pipeline().displayed();

    // drop client 传输：on_disconnected 会 drop Box 走的 QuicTransport →
    // Drop 触发 close(0) → host 立刻收到真实 Disconnected（quic.rs:894-907）。
    let drop_at = Instant::now();
    client.on_disconnected(u64::from((clock)()));
    assert_eq!(client.state(), SessionState::Reconnecting);
    let pre_max_seq = media_datagrams(&recorder)
        .iter()
        .filter_map(|d| rc_pipeline::host::datagram_header(d).ok())
        .map(|h| h.frame_seq.0)
        .max()
        .expect("断开前必有媒体数据报");
    let mark = media_datagrams(&recorder).len();

    // 驱动到恢复完成（host Resumed + client 回 Streaming + 新数据报到达）。
    let mut attempt_at: Option<Instant> = None;
    let mut idr_at: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        client_step(&mut client, &clock).expect("client step");
        if attempt_at.is_none() && client.reconnect_stats().reconnect_attempts >= 1 {
            attempt_at = Some(Instant::now());
        }
        let media = media_datagrams(&recorder);
        if idr_at.is_none() && media.len() > mark {
            idr_at = Some(Instant::now());
            break;
        }
    }
    let idr_at = idr_at.expect("重连后必须有媒体数据报到达");
    let attempt_at = attempt_at.expect("必须发起过重连尝试");
    assert_eq!(
        client.state(),
        SessionState::Streaming,
        "client 应已 resume 回 Streaming"
    );

    // 断线/恢复计数与管线复用判据（host 观测经共享句柄）。
    {
        let h = shared_host.lock().expect("host lock");
        assert_eq!(h.reconnect_stats().disconnects, 1, "host 观测到一次断线");
        assert!(h.reconnect_stats().resumes >= 1, "resume 被接受");
        assert_eq!(h.pipeline_generation(), 1, "管线复用：代数不变");
    }
    assert_eq!(client.reconnect_stats().resumes, 1);
    assert_eq!(
        connects.load(Ordering::SeqCst),
        2,
        "重连续用同一工厂端点（R3：同一 QuicEndpoint）"
    );

    // 恢复后首个到达 client 的数据报：IDR 且 frame_seq 延续。
    let media = media_datagrams(&recorder);
    let first = &media[mark];
    let h = rc_pipeline::host::datagram_header(first).expect("恢复首报头部");
    assert!(h.is_idr(), "恢复后首个到达的数据报必须是 IDR（R4）");
    assert!(
        h.frame_seq.0 > pre_max_seq,
        "frame_seq 延续：恢复首帧 {} 应大于断线前最大 {}",
        h.frame_seq.0,
        pre_max_seq
    );
    // 恢复时延：重连尝试 → 首个 IDR 数据报到达 ≤ 500ms（缩阈值退避配置）。
    let recovery = idr_at.duration_since(attempt_at);
    assert!(
        recovery <= Duration::from_millis(500),
        "重连尝试到 IDR 到达应 ≤500ms，实际 {recovery:?}"
    );
    let total = idr_at.duration_since(drop_at);
    assert!(
        total <= Duration::from_millis(800),
        "断线到恢复总时延应 ≤800ms（50ms 退避 + 500ms 恢复 + 余量），实际 {total:?}"
    );
    eprintln!(
        "reconnect#1: 退避后恢复={recovery:?} 总时延={total:?} stats={:?}",
        client.reconnect_stats()
    );

    // 恢复后继续上屏（管线复用，displayed 计数延续）。
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.pipeline().displayed() <= displayed_at_drop {
        assert!(Instant::now() < deadline, "恢复后应继续上屏");
        client_step(&mut client, &clock).expect("client step");
    }
    client.close(ByeReason::Normal).expect("close");
    stop.store(true, Ordering::Relaxed);
    host_thread.join().expect("host driver 线程退出");
}

// ─────────────────────────────────────────────────────────────────
// 场景 N9 透传：rebind 迁移不断流
// ─────────────────────────────────────────────────────────────────

/// client 端点 `rebind` 新 socket（模拟 NAT rebinding 路径迁移）→ 会话仍
/// Streaming（状态机从未进 Detached/Reconnecting）、host `path_changes≥1`、
/// 媒体数据报持续到达（displayed 继续增长）。照 rc-net loopback 的
/// `connection_survives_rebind_migration` 范式。
#[test]
fn rebind_migration_does_not_interrupt_session() {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));
    let host = new_host(&clock);
    let host_addr = host.local_addr().expect("host addr");
    let client = new_client(host_addr, 7, &clock);
    let (host, mut client) = handshake_pair(host, client);
    anchor_origin(&origin, &host);

    let stop = Arc::new(AtomicBool::new(false));
    let (shared_host, host_thread) = spawn_host_driver(host, Arc::clone(&clock), Arc::clone(&stop));

    // 迁移前基线：至少 3 帧上屏。
    let deadline = Instant::now() + Duration::from_secs(8);
    while client.pipeline().displayed() < 3 {
        assert!(Instant::now() < deadline, "基线预热超时");
        client_step(&mut client, &clock).expect("client step");
    }
    let displayed_before = client.pipeline().displayed();

    // 迁移：client 端点换底层 socket（新端口），连接 ID 不变。
    let new_sock = UdpSocket::bind("127.0.0.1:0").expect("bind new socket");
    client
        .endpoint()
        .expect("client 端点必须留存（坑 7）")
        .rebind(new_sock)
        .expect("rebind");

    // 迁移后：会话不断流、状态机从未离开 Streaming、PathChanged 上报。
    let mut saw_path_change = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        client_step(&mut client, &clock).expect("client step");
        assert_eq!(
            client.state(),
            SessionState::Streaming,
            "迁移后 client 不得进 Reconnecting"
        );
        let h = shared_host.lock().expect("host lock");
        assert_eq!(
            h.state(),
            SessionState::Streaming,
            "迁移后 host 不得进 Detached"
        );
        if h.reconnect_stats().path_changes >= 1 {
            saw_path_change = true;
        }
        drop(h);
        if saw_path_change && client.pipeline().displayed() > displayed_before + 1 {
            break;
        }
    }
    let (disconnects, resumes, path_changes) = {
        let h = shared_host.lock().expect("host lock");
        let s = h.reconnect_stats();
        (s.disconnects, s.resumes, s.path_changes)
    };
    assert!(
        saw_path_change,
        "host 必须观测到 PathChanged（迁移后 path_changes={path_changes}）"
    );
    assert!(
        client.pipeline().displayed() > displayed_before + 1,
        "迁移后媒体数据报持续到达（displayed {} 应继续增长）",
        client.pipeline().displayed()
    );
    assert_eq!(disconnects, 0, "迁移不算断线（R5）");
    assert_eq!(resumes, 0, "无需任何重连");
    eprintln!(
        "rebind#2: path_changes={path_changes} displayed={}",
        client.pipeline().displayed()
    );
    client.close(ByeReason::Normal).expect("close");
    stop.store(true, Ordering::Relaxed);
    host_thread.join().expect("host driver 线程退出");
}

// ─────────────────────────────────────────────────────────────────
// 场景 B：时钟饱和守卫端到端（注入 3s 阈值）
// ─────────────────────────────────────────────────────────────────

/// 注入 `refresh_guard_us = 3s` → host 会话钟到线发 `Bye{Refresh}` 并按
/// 红线拆管线 → client 零退避全新重连（内部留存端点路径）→ 新
/// session_id ≠ 旧、`Welcome` bit1=1（client 校验通过才会 Rebuilt）、
/// 新会话 FrameSeq 从头、驱动器归零后双端时钟读数回到小值。
#[test]
fn refresh_guard_rebuilds_session_end_to_end() {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));
    let mut host = new_host(&clock);
    let host_addr = host.local_addr().expect("host addr");
    // 注入 3s 守卫线（不真等 70 分钟，坑 5）。
    let rcfg = ReconnectConfig {
        refresh_guard_us: 3_000_000,
        ..ReconnectConfig::default()
    };
    host.set_reconnect(rcfg).expect("set_reconnect");
    let client = new_client(host_addr, 4242, &clock);
    let (host, mut client) = handshake_pair(host, client);
    anchor_origin(&origin, &host);
    let old_sid = client.session_id();
    let mut old_max_seq: Option<u64> = None;

    let stop = Arc::new(AtomicBool::new(false));
    let (shared_host, host_thread) = spawn_host_driver(host, Arc::clone(&clock), Arc::clone(&stop));

    // 阶段 1：流 ~3.2s（守卫线 3s）。host 到线即刷新（tick 内自动触发）；
    // 观测到刷新后驱动器把会话钟归零（T17 会话侧清的是纪元估计；注入钟
    // 的重新锚定是驱动器的事——T18 在 CLI 里做同一件事）。
    let mut refreshed = false;
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        client_step(&mut client, &clock).expect("client step");
        let refresh_fired = {
            let h = shared_host.lock().expect("host lock");
            h.reconnect_stats().refreshes >= 1
        };
        if !refreshed && refresh_fired {
            refreshed = true;
            // 记录旧纪元最大帧序号（新会话 FrameSeq 必须小于它）。
            old_max_seq = shared_host
                .lock()
                .expect("host lock")
                .pipeline()
                .trace()
                .frames()
                .iter()
                .map(|f| f.seq().0)
                .max();
            *origin.lock().expect("origin lock") = Some(Instant::now());
        }
        // 等待「真实重建完成」：client 的 rebuilds 计数（而不是 Streaming
        // 态——刷新瞬间 client 还没离开 Streaming，会误判）。
        if refreshed && client.reconnect_stats().rebuilds >= 1 {
            break;
        }
    }
    assert!(refreshed, "3s 守卫线必须触发刷新");
    assert_eq!(
        client.reconnect_stats().rebuilds,
        1,
        "client 应完成零退避全新重建"
    );
    assert_eq!(client.state(), SessionState::Streaming);
    {
        let h = shared_host.lock().expect("host lock");
        assert_eq!(h.state(), SessionState::Streaming, "新会话已建立");
        assert_eq!(
            h.reconnect_stats().disconnects,
            0,
            "刷新不是断线：不经 Detached 的断线路径"
        );
    }

    // 阶段 2：全新会话断言。
    let new_sid = client.session_id();
    assert_ne!(new_sid, old_sid, "刷新重建必须换新 session_id");
    assert_eq!(
        client.reconnect_stats().refreshes,
        1,
        "client 收到一次 Bye(Refresh)（保留旧连接送达，不走降级慢路径）"
    );
    // Welcome bit1=1 由 client 强校验（缺 bit1 会 Err 而不是 Rebuilt）——
    // 走到这里即证明；双端时钟读数回到小值（驱动器已归零）。
    let now_small = (clock)();
    assert!(
        now_small < 1_000_000,
        "归零后时钟读数应回到小值，实际 {now_small}µs"
    );

    // 阶段 3：新会话继续出帧；FrameSeq 从头（< 旧纪元最大值）。
    let displayed_base = client.pipeline().displayed();
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        client_step(&mut client, &clock).expect("client step");
        if client.pipeline().displayed() > displayed_base + 1 {
            break;
        }
    }
    assert!(
        client.pipeline().displayed() > displayed_base + 1,
        "刷新后新会话必须继续出帧上屏"
    );
    let new_max_seq = shared_host
        .lock()
        .expect("host lock")
        .pipeline()
        .trace()
        .frames()
        .iter()
        .map(|f| f.seq().0)
        .max()
        .expect("新会话必须有帧");
    let old_max = old_max_seq.expect("旧纪元必有帧");
    assert!(
        new_max_seq < old_max,
        "新会话 FrameSeq 从头：{new_max_seq} 应远小于旧纪元 {old_max}"
    );
    eprintln!(
        "refresh#3: old_sid={old_sid} new_sid={new_sid} old_max_seq={old_max} new_max_seq={new_max_seq}"
    );
    client.close(ByeReason::Normal).expect("close");
    stop.store(true, Ordering::Relaxed);
    host_thread.join().expect("host driver 线程退出");
}
