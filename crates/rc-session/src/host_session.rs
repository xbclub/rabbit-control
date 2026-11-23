//! 被控端会话：QuicEndpoint(accept) + QuicTransport + HostPipeline + 注入。
//!
//! # 职责边界（T14 / T17）
//!
//! 本模块只做**连接生命周期与事件分发**：握手裁定（Hello/Welcome）、
//! 三通道单一分发点（[`HostSession::tick`]）、照 `run_real` 形态的驱动
//! 循环（[`HostSession::run`]）与停机顺序红线（S6）。探测 / 看门狗分别归
//! T15/T16；**T17 增补断线重连**：`Disconnected` 不再等价于会话结束——
//! 保持窗口内进 [`SessionState::Detached`] 保留管线等 `ReconnectRequest`
//! 续传（[`HostSession::tick_detached`]），会话钟到 70 分钟守卫线主动
//! `Bye(Refresh)` 重建（[`HostSession::check_refresh`]）。
//!
//! # 停机顺序红线（S6）
//!
//! host = 发 Bye（尽力、短超时不等待确认）→ `stop_capture()` → tick 排空
//! 到静默（交接通道空 + `has_pending_encodes() == false`，500ms 上限）→
//! `shutdown()` → 末轮 tick 收编码完成并发送。反序（先 shutdown 再 tick）
//! 会把新提交发给已退出的编码线程，授权帧凭空消失（见
//! `rc-pipeline/src/host.rs` 的 `stop_capture` 文档）。T17 的任何拆除路径
//! （TTL 过期 / resume 未命中 / 饱和刷新 / close）都走同一实现
//! （`teardown_pipeline`，R7）。
//!
//! # 时钟纪律（铁律 4 / S9）
//!
//! 会话钟由构造方注入（[`SessionClock`]），握手完成后经 `set_clock` 交给
//! 管线；本模块除 tick 内部把会话钟喂给守卫/管线外**不读任何时钟**
//! （握手等待与排空上限用分片轮数实现，不取墙钟）。T17 的窗口/守卫
//! 判定全部吃 `now_us` 参数（[`HostSession::tick_detached`] 等）。
//! 会话零点锚首个采集帧是构造闭包方的职责（照 `run_real`
//! 的 `origin_slot` 模式，`crates/rc-loopback/src/lib.rs` 的 `run_real`）。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rc_capture::CaptureBackend;
use rc_encode::Encoder;
use rc_net::quic::{QuicEndpoint, DEFAULT_MTU};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_pipeline::{HostPipeline, PipelineConfig};
use rc_types::{Error, Result};

use crate::auth::{
    AutoApprover, ConnectionApprover, HostAuthAction, HostAuthConfig, HostAuthenticator,
    OwnerReplayCache, VerifiedOwnerTicket,
};
use crate::clipboard::{
    ClipboardAction, ClipboardBackend, ClipboardChannel, ClipboardConfig, ClipboardStats,
};
use crate::proto::{
    AudioFramePacket, AuthStatus, ByeReason, ControlMessage, Hello, HostFlags, SessionParams,
    Welcome,
};
use crate::reconnect::{
    ReconnectConfig, ReconnectRequest, ReconnectStats, ResumeWindow, DEFAULT_RESUME_TTL,
};
use crate::{SessionClock, SessionState, StopReason};

/// 握手等待的 poll 分片：真 transport 上单次 poll 最多阻塞这么久，
/// 分片轮转直至累计达到 `handshake_timeout`（不读墙钟的到期实现）。
const HANDSHAKE_POLL_SLICE: Duration = Duration::from_millis(50);

/// 停机排空的静默轮上限：每静默轮 `wait_event(5ms)`，100 轮 = 500ms
/// 上限（S6；产出轮不计入——采集已停，帧数有界）。
const DRAIN_QUIET_ROUNDS: u32 = 100;
/// 停机排空的总轮数硬上限：防异常后端持续产出把排空变成死循环。
const DRAIN_HARD_ROUNDS: u32 = 2_000;
/// 排空时静默轮的等待时长。
const DRAIN_WAIT: Duration = Duration::from_millis(5);

/// Detached 期 accept 的预算。
///
/// 权衡（坑 1 的精确化）：accept 是阻塞式，长超时会把窗口判定与守卫检查
/// 饿死；但 rc-net 的 accept 把「等 Incoming → 完成握手 → 接两条 reliable
/// 流」**整链路放在同一 deadline 内**——预算太短会把半接受的连接丢掉
///（quinn 直接 CONNECTION_CLOSE，client 表现为 connect 成功后写失败，
/// 实测 2ms/50ms 预算下均按概率丢连接）。2s：无连接时到点即返（窗口
/// 判定最多延迟 2s，远小于 10s TTL / 70min 守卫的粒度），有连接时剩余
/// 预算足够走完握手链（本机毫秒级）。
const DETACHED_ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);
/// Detached 期读新连接首条控制消息的 poll 分片（同坑 1）。
const DETACHED_POLL_SLICE: Duration = Duration::from_millis(2);
/// 工具复用接收队列有界容量上限（可靠通道绝不静默丢帧，满时返回协议错误）。
const MAX_PENDING_TOOL_MESSAGES: usize = 2048;
/// 音频帧接收队列有界容量上限（低延迟优先，满时丢弃最旧帧）。
const MAX_PENDING_AUDIO_FRAMES: usize = 32;

/// 被控端会话配置。
pub struct HostSessionConfig {
    /// 监听地址（`0.0.0.0:9901` / `127.0.0.1:0` 均可）。
    pub listen_addr: SocketAddr,
    /// 路径 MTU 预算，默认 `rc_net::quic::DEFAULT_MTU`。
    pub mtu: usize,
    /// host 能力上界：Welcome 裁定时把 client 请求逐字段 min 夹紧到此。
    pub caps: SessionParams,
    /// 管线配置模板；resolution/fps/bitrate 在握手后以裁定值覆盖。
    pub pipeline: PipelineConfig,
    /// accept 后等 Hello 的上限，默认 5s。
    pub handshake_timeout: Duration,
    /// 输入注入器（真机 = rc-input-macos 的 CGEvent 桥；测试 = 记录型替身）。
    pub injector: Box<dyn rc_input::InputInjector>,
    /// 假采集节流：假后端必须 true（坑 4），真采集 false。
    pub pace_capture: bool,
    /// T19：host 侧鉴权配置（默认无密码 / `DEFAULT_AUTH_ITERATIONS` /
    /// `DEFAULT_AUTH_TIMEOUT_US`）。
    pub auth: HostAuthConfig,
    /// T19：连接审批人（全项目唯一审批挂点，默认 [`AutoApprover`]；
    /// T22 托盘弹窗从此处注入）。
    pub approver: Box<dyn ConnectionApprover>,
}

impl HostSessionConfig {
    /// 以监听地址、能力上界与注入器构造（其余字段取默认值）。
    pub fn new(
        listen_addr: SocketAddr,
        caps: SessionParams,
        injector: Box<dyn rc_input::InputInjector>,
    ) -> Self {
        Self {
            listen_addr,
            mtu: DEFAULT_MTU,
            caps,
            pipeline: PipelineConfig::default(),
            handshake_timeout: Duration::from_secs(5),
            injector,
            pace_capture: false,
            auth: HostAuthConfig::default(),
            approver: Box::new(AutoApprover),
        }
    }
}

/// 握手期在途鉴权上下文（`Hello` 已收、尚未终局前暂存，保证 A6：
/// 鉴权未终局前零管线启动）。
struct PendingAuth {
    auth: HostAuthenticator,
    session_id: u64,
    granted: SessionParams,
}

/// 单步鉴权动作执行结果。
enum AuthStepOutcome {
    /// 仍在等待 `AuthResponse`、`OwnerAuthResponse` 或人工审批。
    Pending,
    /// 鉴权终局通过（`Conclude(Ok)`），调用方可落成会话。
    Accepted {
        session_id: u64,
        granted: SessionParams,
        authenticated_owner: Option<VerifiedOwnerTicket>,
    },
    /// 鉴权终局拒绝（`Conclude(Rejected)`），已尽力回发 `Welcome{Rejected}`。
    Rejected,
}

/// 内存测试替身无真实网络地址时的回退对端地址。
const FALLBACK_PEER_ADDR: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    std::net::Ipv4Addr::UNSPECIFIED,
    0,
));

/// 被控端会话。
///
/// 状态机严格四态单向 `Init→Connected→Streaming→Closed`（S1）：
/// 管线在**握手裁定后**才建（参数未裁定前建管线只会建错）。
pub struct HostSession {
    cfg: HostSessionConfig,
    /// 真网络构造持有的端点（accept 后消耗；with_parts 模式恒 None）。
    endpoint: Option<QuicEndpoint>,
    transport: Option<Box<dyn Transport>>,
    /// with_parts 注入的后端；握手建管线时消耗。真网络模式用工厂选择。
    backends: Option<(Box<dyn CaptureBackend>, Box<dyn Encoder>)>,
    pipeline: Option<HostPipeline>,
    clock: SessionClock,
    state: SessionState,
    session_id: Option<u64>,
    negotiated: Option<SessionParams>,
    /// T15：当前生效目标码率（None = 尚无 SetBitrate，取协商值）。
    /// 会话侧自行记账，不查编码线程——编码器在管线线程内。
    current_bitrate: Option<u32>,
    /// 已发过 Bye（停机路径只发一次）。
    bye_sent: bool,
    /// 进入 Closed 的原因（summary 用）。
    closed_reason: Option<StopReason>,
    /// S4：Streaming 之前 / 方向错误到达的媒体事件丢弃计数（只丢不缓存）。
    early_events_dropped: u64,
    /// S8：已打过忽略日志的消息标签（每种一次，限流不刷屏）。
    logged_ignore: Vec<&'static str>,
    /// 累计发出的媒体数据报数（发送成功才算）。
    datagrams_sent_total: usize,
    /// 累计注入的输入事件数。
    events_injected_total: u64,
    /// 停机清点：已发出（Sent 打点）但无结局的帧数（判定权在对端，
    /// 见 `stop_with` 第 ⑤ 步）。
    sent_unresolved: u64,
    // ── T18：观测计数（host 侧摘要 / 报告的数据源）──
    /// 含关键事件的已注入批数（InputAck `key_batch_seq` 的同一口径，
    /// 判定用 [`crate::testkit::batch_is_key`]）。
    key_batches_total: u64,
    /// 收到的 IdrRequest 条数（client 侧请求强度的观测点）。
    idr_requests_received_total: u64,
    /// 调度器在途队列深度峰值（铁律 1：≤2 打 stderr 摘要）。
    in_flight_peak: u32,
    // ── T17：断线重连（Detached / 保持窗口 / 饱和守卫）──
    /// 重连配置（驱动开始前经 [`HostSession::set_reconnect`] 可换）。
    reconnect: ReconnectConfig,
    /// 断线保持窗口（从 Disconnected 起算的拆管线倒计时）。
    resume_window: ResumeWindow,
    /// 断线/重连统计。
    rstats: ReconnectStats,
    /// 管线代数：每建一次管线 +1（窗口内 resume 不变；「管线不拆」的
    /// 可观测判据）。
    pipeline_generation: u32,
    /// 管线存活标记：build 后 true、按红线拆除后 false（管线引用本身
    /// 保留供停机账目查询，不能用 `pipeline.is_some()` 当存活判据）。
    pipeline_alive: bool,
    /// 管线重建工厂（饱和刷新 / 全新 Hello 时调用）；None = 工厂选择
    /// auto 后端（与首次握手同一构造路径）。
    pipeline_factory: Option<Box<dyn FnMut() -> Result<HostPipeline> + Send>>,
    /// T18：带裁定参数的管线工厂（CLI 用；优先于 `pipeline_factory`）。
    pipeline_factory_for: Option<PipelineFactoryFor>,
    /// 下一个全新会话的 Welcome 置 host_flags bit1（饱和刷新后置位）。
    /// 消费点**不是**发出 Welcome——而是「进入 Streaming 后收到对端后续
    /// 证据（该连接首条 Reliable/Urgent 消息）」或 resume 命中（发现 6：
    /// Welcome 随连接丢失时，重试的第二个 Welcome 必须仍带 bit1，否则
    /// 对端 `refresh_pending && !bit1` 强拒死锁）。
    pending_epoch_reset: bool,
    /// 上次刷新基准时刻（u32 会话钟域）。判定用全仓 wrapping 纪律
    /// `now.wrapping_sub(base) >= threshold`（发现 4：绝对加法
    /// `now < base + threshold` 在 base 靠近饱和一侧时和可越过 u32::MAX，
    /// 第二次刷新数学上不可能触发）。None = 从未刷新（视作 base=0，
    /// 首次刷新即绝对线 `now >= threshold`，两者等价）；新会话落成时由
    /// `expect_hello` 重锚到当前钟（第二纪元从小值起算，照常在阈值线
    /// 触发；不归零的驱动器则按阈值周期性触发，wrapping 语义两通）。
    refresh_base_us: Option<u32>,
    /// Detached 期被丢弃的脏候选连接数（发现 1：decode 失败/Hello 违规
    /// 的候选只丢不杀会话；观测点）。
    dirty_candidates_total: u64,
    /// Detached 期 accept 的非超时错误数（发现 2：坏 TLS 握手包等只计数
    /// 不上抛；观测点）。
    accept_errors_total: u32,
    /// 累计发出的带宽探测数据报数（发现 10：与媒体数据报分开记——
    /// `datagrams_sent_total` 按文档只计媒体）。
    probe_datagrams_sent_total: usize,
    // ── T19：密码挑战与人工审批联动 ──
    /// 当前（或最新 accept 到的）对端传输层地址（`with_parts` /
    /// `offer_transport` 内存替身模式回退 `0.0.0.0:0`）。
    peer_addr: SocketAddr,
    /// 在途鉴权上下文（握手未终局前暂存；`Conclude(Ok)` 后清空并落成会话）。
    pending_auth: Option<PendingAuth>,
    /// 本会话已通过自控 PoP 认证的票据元数据（仅 `OwnerTicket` 认证成功时为 `Some`）。
    authenticated_owner: Option<VerifiedOwnerTicket>,
    /// 跨候选连接/重连持久化的已消费 `ticket_id` 防重放缓存。
    owner_replay_cache: Arc<Mutex<OwnerReplayCache>>,
    /// Detached 期当前候选连接的握手/鉴权总截止时刻（微秒，注入时钟口径；H2）。
    detached_candidate_deadline_us: Option<u64>,
    // ── T20：剪贴板 UTF-8 纯文本双向同步（默认禁用，显式 enable_clipboard 开启）──
    /// 剪贴板同步状态：`Some((channel, backend))` 表示已启用，`None` 表示禁用（CB1）。
    clipboard: Option<(ClipboardChannel, Box<dyn ClipboardBackend>)>,
    /// 未启用时收到的 `ClipboardNotify` 计数（CB1：计数、不写本机、不拆会话）。
    clipboard_received_while_disabled: u64,
    // ── 工具复用（0x10）与音频帧（0x11）有界接收队列 ──
    /// 工具复用消息接收队列（FILE/TERMINAL/PORTMAP，上限 [`MAX_PENDING_TOOL_MESSAGES`]）。
    tool_rx: VecDeque<(u8, Vec<u8>)>,
    /// 音频控制子通道（`TOOL_CHANNEL_AUDIO_CTRL`）独立最新值槽位（不被批量数据挤占）。
    audio_ctrl_rx: Option<Vec<u8>>,
    /// 工具复用接收队列满时触发协议错误的溢出计数。
    tool_rx_overflow_dropped: u64,
    /// 音频帧接收队列（上限 [`MAX_PENDING_AUDIO_FRAMES`]，满时丢最旧）。
    audio_rx: VecDeque<AudioFramePacket>,
    /// 握手协商分辨率时是否保持 `cfg.caps` 宽高比（默认 `false` 保持逐字段 `min` 兼容；
    /// 真实显示器场景置 `true`，消除 ScreenCaptureKit 宽高比不匹配导致的黑边与光标偏移）。
    preserve_caps_aspect: bool,
    /// 热切换重建管线时的帧序号偏移量（保证线上 `frame_seq` 跨管线严格单调递增，不被对端重组器丢弃）。
    frame_seq_offset: u64,
    /// 本会话已发出的最高线上 `frame_seq`（用于热切换时推算下一代 `frame_seq_offset`）。
    last_sent_frame_seq: Option<u64>,
}

impl Drop for HostSession {
    fn drop(&mut self) {
        // A8：会话被销毁时撤销任何在途审批（无在途请求时为幂等 no-op）。
        self.cfg.approver.cancel();
    }
}

impl HostSession {
    /// 真网络构造：绑定 QUIC 端点（Init）。管线**此刻不建**——参数未裁定。
    /// `cert_pem`/`key_pem` 是本端服务证书（内部经 `rc_net::quic::Identity::from_pem`）。
    pub fn new(
        cfg: HostSessionConfig,
        cert_pem: &str,
        key_pem: &str,
        clock: SessionClock,
    ) -> Result<Self> {
        let owner_replay_cache = cfg
            .auth
            .replay_cache
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(OwnerReplayCache::default())));
        let identity = rc_net::quic::Identity::from_pem(cert_pem, key_pem)?;
        let net = NetConfig {
            role: Role::Host,
            local_addr: cfg.listen_addr,
            peer_addr: None,
            mtu: cfg.mtu,
        };
        let endpoint = QuicEndpoint::new(&net, &identity)?;
        Ok(Self {
            cfg,
            endpoint: Some(endpoint),
            transport: None,
            backends: None,
            pipeline: None,
            clock,
            state: SessionState::Init,
            session_id: None,
            negotiated: None,
            current_bitrate: None,
            bye_sent: false,
            closed_reason: None,
            early_events_dropped: 0,
            logged_ignore: Vec::new(),
            datagrams_sent_total: 0,
            events_injected_total: 0,
            sent_unresolved: 0,
            key_batches_total: 0,
            idr_requests_received_total: 0,
            in_flight_peak: 0,
            reconnect: ReconnectConfig::default(),
            resume_window: ResumeWindow::new(DEFAULT_RESUME_TTL),
            rstats: ReconnectStats::default(),
            pipeline_generation: 0,
            pipeline_alive: false,
            pipeline_factory: None,
            pipeline_factory_for: None,
            pending_epoch_reset: false,
            refresh_base_us: None,
            dirty_candidates_total: 0,
            accept_errors_total: 0,
            probe_datagrams_sent_total: 0,
            peer_addr: FALLBACK_PEER_ADDR,
            pending_auth: None,
            authenticated_owner: None,
            owner_replay_cache,
            detached_candidate_deadline_us: None,
            clipboard: None,
            clipboard_received_while_disabled: 0,
            tool_rx: VecDeque::new(),
            audio_ctrl_rx: None,
            tool_rx_overflow_dropped: 0,
            audio_rx: VecDeque::new(),
            preserve_caps_aspect: false,
            frame_seq_offset: 0,
            last_sent_frame_seq: None,
        })
    }

    /// 测试注入：transport 已连通（Connected），媒体后端由调用方给定
    /// （能力需与 `cfg.caps` 一致）。管线同样推迟到握手完成。
    pub fn with_parts(
        cfg: HostSessionConfig,
        transport: Box<dyn Transport>,
        capture: Box<dyn CaptureBackend>,
        encoder: Box<dyn Encoder>,
        clock: SessionClock,
    ) -> Result<Self> {
        let owner_replay_cache = cfg
            .auth
            .replay_cache
            .clone()
            .unwrap_or_else(|| Arc::new(Mutex::new(OwnerReplayCache::default())));
        Ok(Self {
            cfg,
            endpoint: None,
            transport: Some(transport),
            backends: Some((capture, encoder)),
            pipeline: None,
            clock,
            state: SessionState::Connected,
            session_id: None,
            negotiated: None,
            current_bitrate: None,
            bye_sent: false,
            closed_reason: None,
            early_events_dropped: 0,
            logged_ignore: Vec::new(),
            datagrams_sent_total: 0,
            events_injected_total: 0,
            sent_unresolved: 0,
            key_batches_total: 0,
            idr_requests_received_total: 0,
            in_flight_peak: 0,
            reconnect: ReconnectConfig::default(),
            resume_window: ResumeWindow::new(DEFAULT_RESUME_TTL),
            rstats: ReconnectStats::default(),
            pipeline_generation: 0,
            pipeline_alive: false,
            pipeline_factory: None,
            pipeline_factory_for: None,
            pending_epoch_reset: false,
            refresh_base_us: None,
            dirty_candidates_total: 0,
            accept_errors_total: 0,
            probe_datagrams_sent_total: 0,
            peer_addr: FALLBACK_PEER_ADDR,
            pending_auth: None,
            authenticated_owner: None,
            owner_replay_cache,
            detached_candidate_deadline_us: None,
            clipboard: None,
            clipboard_received_while_disabled: 0,
            tool_rx: VecDeque::new(),
            audio_ctrl_rx: None,
            tool_rx_overflow_dropped: 0,
            audio_rx: VecDeque::new(),
            preserve_caps_aspect: false,
            frame_seq_offset: 0,
            last_sent_frame_seq: None,
        })
    }

    /// 启用或关闭按 `cfg.caps` 宽高比等比缩放协商分辨率（默认关闭）。
    ///
    /// 当 `cfg.caps` 对应真实显示器逻辑分辨率时开启，可在满足客户请求分辨率上界的
    /// 同时严格保持主显示器宽高比，避免采集侧填充黑边导致控制端光标坐标偏移。
    pub fn set_preserve_caps_aspect(&mut self, preserve: bool) {
        self.preserve_caps_aspect = preserve;
    }

    /// 实际绑定的监听地址（`listen_addr` 端口填 0 时由此得知；任务卡 API
    /// 表漏了它——真网络 client 必须能拿到 host 的实际端口才能 connect）。
    /// with_parts 模式（无端点）返回配置里的 `listen_addr`。
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        match &self.endpoint {
            Some(ep) => ep.local_addr(),
            None => Ok(self.cfg.listen_addr),
        }
    }

    /// 阻塞 accept 一个连接，随后完成 Hello/Welcome（Init→Connected→Streaming）。
    /// 返回裁定参数。Hello 违规（version≠1 等）返回 Err 且连接关闭。
    ///
    /// T17：端点改为**借用不消耗**——Detached 期 `tick_detached` 还要用
    /// 同一端点 accept 重连/全新连接（T14 实现把它 take 走，是本卡的最小
    /// 内部修正；accept 超时后端点也可重试，行为只会更稳）。
    pub fn accept_and_handshake(&mut self, timeout: Duration) -> Result<SessionParams> {
        match self.state {
            SessionState::Init => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "accept_and_handshake requires Init state, got {other:?}"
                )))
            }
        }
        let transport = self
            .endpoint
            .as_ref()
            .ok_or_else(|| {
                Error::InvalidConfig("accept requires real-network construction (new)".into())
            })?
            .accept(timeout)?;
        self.peer_addr = transport.remote_addr();
        self.transport = Some(Box::new(transport));
        self.state = SessionState::Connected;
        self.handshake()
    }

    /// 已连通 transport 上的握手步骤（with_parts 后由测试直接驱动）。
    /// 内部以 `handshake_timeout` 为预算 poll 等待 Hello，并在 T19 鉴权
    /// 期间每拍非阻塞轮询 `ConnectionApprover`（A6/A7/A8）。
    ///
    /// 分片轮转（每片 [`HANDSHAKE_POLL_SLICE`]）而不是读墙钟算剩余预算：
    /// 本模块不读任何时钟（见模块文档）；mock transport 忽略超时，
    /// 预置事件即刻交付，同样适用。
    pub fn handshake(&mut self) -> Result<SessionParams> {
        match self.state {
            SessionState::Connected => {}
            SessionState::Init => {
                return Err(Error::InvalidConfig(
                    "handshake before transport established (call accept_and_handshake first)"
                        .into(),
                ))
            }
            other => {
                return Err(Error::InvalidConfig(format!(
                    "handshake requires Connected state, got {other:?}"
                )))
            }
        }
        self.pending_auth = None;
        let slice = self.cfg.handshake_timeout.min(HANDSHAKE_POLL_SLICE);
        let slice_ms = slice.as_millis().max(1) as u64;
        let total_ms = self.cfg.handshake_timeout.as_millis() as u64;
        let mut rounds_left = total_ms.div_ceil(slice_ms).max(1);
        let auth_budget_ms = self
            .cfg
            .handshake_timeout
            .as_millis()
            .max(Duration::from_micros(self.cfg.auth.approval_timeout_us).as_millis())
            as u64;
        let auth_budget_rounds = auth_budget_ms.div_ceil(slice_ms).max(1) + 2;

        loop {
            let events = match self
                .transport
                .as_mut()
                .expect("Connected implies transport")
                .poll(slice)
            {
                Ok(ev) => ev,
                Err(e) => return Err(self.handshake_fail(e)),
            };
            let mut polled_approval_this_tick = false;
            for ev in events {
                match ev {
                    NetEvent::Reliable(msg) => {
                        let now_us = u64::from((self.clock)());
                        match self.handle_handshake_reliable(&msg, now_us) {
                            Ok(AuthStepOutcome::Accepted {
                                session_id,
                                granted,
                                authenticated_owner,
                            }) => {
                                self.pending_auth = None;
                                return match self.finalize_session(session_id, granted) {
                                    Ok(()) => {
                                        self.authenticated_owner = authenticated_owner;
                                        Ok(granted)
                                    }
                                    Err(e) => Err(self.handshake_fail(e)),
                                };
                            }
                            Ok(AuthStepOutcome::Rejected) => {
                                // 给真 QUIC 一个短排空窗口（MockTransport 忽略超时立即返回），
                                // 避免 CONNECTION_CLOSE 抢先覆盖在途 Welcome{Rejected}（U11）。
                                if let Some(t) = self.transport.as_mut() {
                                    let _ = t.poll(Duration::from_millis(50));
                                }
                                self.cfg.approver.cancel();
                                self.pending_auth = None;
                                self.state = SessionState::Closed;
                                self.closed_reason = Some(StopReason::Requested);
                                self.transport = None;
                                return Err(Error::Protocol {
                                    offset: 0,
                                    msg: "authentication rejected (replied Welcome{Rejected} and disconnected)"
                                        .into(),
                                });
                            }
                            Ok(AuthStepOutcome::Pending) => {
                                rounds_left = rounds_left.max(auth_budget_rounds);
                                polled_approval_this_tick = true;
                            }
                            Err(e) => return Err(self.handshake_fail(e)),
                        }
                    }
                    // S4：Streaming 之前到达的媒体/输入事件丢弃并计数，不缓存。
                    NetEvent::Datagram(_) | NetEvent::ReliableUrgent(_) => {
                        self.early_events_dropped += 1;
                    }
                    // transport 层事件，会话状态由握手推进（S4）。
                    NetEvent::Connected | NetEvent::PathChanged => {}
                    NetEvent::Disconnected { reason } => {
                        self.cfg.approver.cancel();
                        self.pending_auth = None;
                        self.state = SessionState::Closed;
                        self.closed_reason = Some(StopReason::Disconnected(reason.clone()));
                        self.transport = None;
                        return Err(Error::Network(format!(
                            "disconnected while waiting for Hello/auth: {reason}"
                        )));
                    }
                }
            }
            if !polled_approval_this_tick && self.pending_auth.is_some() {
                let now_us = u64::from((self.clock)());
                match self.poll_pending_auth(now_us) {
                    Ok(AuthStepOutcome::Accepted {
                        session_id,
                        granted,
                        authenticated_owner,
                    }) => {
                        self.pending_auth = None;
                        return match self.finalize_session(session_id, granted) {
                            Ok(()) => {
                                self.authenticated_owner = authenticated_owner;
                                Ok(granted)
                            }
                            Err(e) => Err(self.handshake_fail(e)),
                        };
                    }
                    Ok(AuthStepOutcome::Rejected) => {
                        // 给真 QUIC 一个短排空窗口（MockTransport 忽略超时立即返回），
                        // 避免 CONNECTION_CLOSE 抢先覆盖在途 Welcome{Rejected}（U11）。
                        if let Some(t) = self.transport.as_mut() {
                            let _ = t.poll(Duration::from_millis(50));
                        }
                        self.cfg.approver.cancel();
                        self.pending_auth = None;
                        self.state = SessionState::Closed;
                        self.closed_reason = Some(StopReason::Requested);
                        self.transport = None;
                        return Err(Error::Protocol {
                            offset: 0,
                            msg: "authentication rejected (replied Welcome{Rejected} and disconnected)"
                                .into(),
                        });
                    }
                    Ok(AuthStepOutcome::Pending) => {}
                    Err(e) => return Err(self.handshake_fail(e)),
                }
            }
            if rounds_left <= 1 {
                self.cfg.approver.cancel();
                self.pending_auth = None;
                return Err(Error::Timeout(self.cfg.handshake_timeout));
            }
            rounds_left -= 1;
        }
    }

    /// 当前状态。
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Hello 带来的会话号。
    pub fn session_id(&self) -> Option<u64> {
        self.session_id
    }

    /// 握手裁定的流参数（Streaming 前 None）。
    pub fn negotiated(&self) -> Option<SessionParams> {
        self.negotiated
    }

    /// host 能力上界配置（分辨率/帧率/码率上限）。
    pub fn caps(&self) -> SessionParams {
        self.cfg.caps
    }

    /// 当前（或最新 accept 到的）对端传输层地址（`with_parts` 替身回退 `0.0.0.0:0`）。
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// 本会话已通过自控 PoP 认证的票据元数据（仅 `OwnerTicket` 认证成功时为 `Some`）。
    pub fn authenticated_owner(&self) -> Option<&VerifiedOwnerTicket> {
        self.authenticated_owner.as_ref()
    }

    /// 一拍（run_real 形态的最小单元）：
    /// ① `transport.poll(poll_budget)` 单点分发——`ReliableUrgent`→
    /// `rc_input::channel::inject_message` 解码注入；`Reliable`→控制分发
    /// （`IdrRequest`→`pipeline.request_idr()`，pending_idr 粘住直到真编出
    /// IDR）；`Disconnected`→内联停机序列；
    /// ② `pipeline.tick(clock())` 产出数据报→逐条 `send_datagram`（当拍
    /// 发完，S7）。
    ///
    /// # 错误语义
    ///
    /// 脏控制消息（proto 解码失败）与输入注入失败都会**先执行完整停机
    /// 序列再返回 Err**（宁拒收不猜测，S8；对齐 rc-input 安全边界纪律），
    /// 调用方拿到 Err 时会话已处于 Closed、账目已闭合。
    pub fn tick(&mut self, poll_budget: Duration) -> Result<HostTickReport> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "host tick requires Streaming state, got {other:?} (S1)"
                )))
            }
        }
        // T17：时钟饱和守卫（正常 Streaming 驱动循环每拍调用）——到线发
        // Bye(Refresh) + 红线拆管线 + 回 accept（Detached），本拍即止。
        if self.check_refresh(u64::from((self.clock)()))? {
            return Ok(HostTickReport {
                busy: true,
                ..HostTickReport::default()
            });
        }
        let mut report = HostTickReport::default();
        let events = self
            .transport
            .as_mut()
            .expect("Streaming implies transport")
            .poll(poll_budget)?;

        let mut bye: Option<ByeReason> = None;
        let mut disconnected = false;
        let mut first_err: Option<Error> = None;
        let mut dirty = false;

        for ev in events {
            match ev {
                // transport 层事件，会话状态由握手/停机推进（S4）。
                NetEvent::Connected => {}
                // T17（R5）：路径迁移不算断线——不迁移状态、不断流，
                // 只计数（rebind 已保证连接存活，N9）。
                NetEvent::PathChanged => {
                    self.rstats.path_changes += 1;
                }
                // host 不收媒体数据报（方向错误）：忽略计数，不缓存（S4/S7）。
                NetEvent::Datagram(_) => {
                    self.early_events_dropped += 1;
                }
                NetEvent::ReliableUrgent(msg) => {
                    // 发现 6：该连接首条输入消息 = 对端已收到 Welcome 的
                    // 后续证据，消费饱和刷新 bit1 的待决态（消费点语义见
                    // `pending_epoch_reset` 字段文档）。
                    self.maybe_consume_epoch_reset();
                    // T18：改为 decode + inject 内联（语义与
                    // rc_input::channel::inject_message 等价），以便对**关键批**
                    // （testkit::batch_is_key 共享口径）回发 InputAck——
                    // client 据此换算输入单向延迟。ack 尽力而为：连接将死时
                    // 的发送失败由 poll 的断线信号接管（与 pong 同纪律）。
                    match rc_input::decode_events(&msg).and_then(|events| {
                        self.cfg.injector.inject(&events)?;
                        Ok(events)
                    }) {
                        Ok(events) => {
                            report.events_injected += events.len();
                            report.busy = true;
                            if crate::testkit::batch_is_key(&events) {
                                self.key_batches_total += 1;
                                let ack = ControlMessage::InputAck {
                                    key_batch_seq: self.key_batches_total,
                                    inject_done_us: u64::from((self.clock)()),
                                };
                                let mut buf = Vec::new();
                                ack.encode(&mut buf);
                                if let Some(t) = self.transport.as_mut() {
                                    let _ = t.send_reliable(&buf);
                                }
                            }
                        }
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                NetEvent::Reliable(msg) => {
                    // 发现 6：同上——该连接首条控制消息即对端后续证据
                    //（消费先于分发：脏消息同样证明连接已被对端使用过）。
                    self.maybe_consume_epoch_reset();
                    match self.dispatch_control(&msg) {
                        Ok(ControlFlow::Handled) => {
                            report.control_msgs_handled += 1;
                            report.busy = true;
                        }
                        Ok(ControlFlow::Bye(reason)) => {
                            bye = Some(reason);
                            report.busy = true;
                        }
                        Err(e) => {
                            // 脏控制消息：立即停止分发（S8），批内后续事件不猜。
                            dirty = true;
                            first_err.get_or_insert(e);
                            break;
                        }
                    }
                }
                NetEvent::Disconnected { .. } => {
                    disconnected = true;
                    report.busy = true;
                }
            }
        }

        // 发现 9：先累计当拍已注入的事件数再走停机/早退分支——脏消息
        // 与注入失败路径的 `return Err` 在此之前发生的话，本拍已注入的
        // 计数会被凭空吞掉（summary 的 events_injected 与实际不符）。
        self.events_injected_total += u64::try_from(report.events_injected).unwrap_or(u64::MAX);

        // ② 管线一拍：产出的数据报当拍发完（S7）。
        if !dirty && bye.is_none() && !disconnected {
            let produced = match self.pipeline.as_mut() {
                Some(p) => p.tick((self.clock)())?,
                None => None,
            };
            if let Some(mut datagrams) = produced {
                stamp_and_track_datagrams(
                    &mut datagrams,
                    self.frame_seq_offset,
                    &mut self.last_sent_frame_seq,
                );
                if !datagrams.is_empty() {
                    report.busy = true;
                }
                report.datagrams_sent = self.send_datagrams_best_effort(&datagrams);
                // T16（W5）：IDR 帧编码完成送出数据报的同一拍发 IdrAck
                //（reliable 流，一个跨多片的 IDR 只一条）——client 据此做
                // 去抖与超时复核。连接已死时后续 ack 无处可去，停止。
                for seq in crate::watchdog::idr_frame_seqs(&datagrams) {
                    let mut buf = Vec::new();
                    ControlMessage::IdrAck { frame_seq: seq.0 }.encode(&mut buf);
                    match self.transport.as_mut() {
                        Some(t) => {
                            if t.send_reliable(&buf).is_err() {
                                break; // 连接已死：后续 ack 无处可去
                            }
                        }
                        None => break,
                    }
                }
            }
            // ③ T20 剪贴板泵（仅 Streaming 态；未启用时空操作、零开销）。
            if self.clipboard.is_some() {
                self.pump_clipboard(u64::from((self.clock)()))?;
            }
        }

        // 停机触发（tick 内完成，run 只看 closed）。
        if let Some(reason) = bye {
            let stop = StopReason::ByeReceived(reason);
            self.stop_with(stop.clone(), None);
            report.closed = Some(stop);
        }
        // T17（R1）：Disconnected 不再等价于会话结束——保持窗口内进
        // Detached、管线保留；停机/放弃决策交给窗口判定（tick_detached）。
        if disconnected {
            self.on_disconnected(u64::from((self.clock)()));
            // T18 修复：对端「先 Bye 后关连接」时，同一 poll 批里 Bye 与
            // Disconnected 一起到达——Bye 先走 stop_with 进 Closed，
            // on_disconnected 对非 Streaming 幂等返回，此时 Closed 是合法
            // 终态（断言只认 Detached 会把正常优雅关闭打成 panic）。
            debug_assert!(matches!(
                self.state,
                SessionState::Detached | SessionState::Closed
            ));
            report.busy = true;
        }
        if dirty {
            let err = first_err.expect("dirty implies first_err");
            self.stop_with(
                StopReason::Disconnected(format!("dirty control message: {err}")),
                None,
            );
            return Err(err);
        }
        if let Some(e) = first_err {
            // 输入解码/注入失败：与脏控制消息同一纪律——有序停机后上抛。
            self.stop_with(
                StopReason::Disconnected(format!("input inject failed: {e}")),
                None,
            );
            return Err(e);
        }

        // T18：在途队列深度峰值采样（铁律 1 的观测点；停机排空期不再增长）。
        let cur = self
            .pipeline
            .as_ref()
            .map(|p| p.in_flight_len() as u32)
            .unwrap_or(0);
        self.in_flight_peak = self.in_flight_peak.max(cur);
        Ok(report)
    }

    /// T18：含关键事件的已注入批数（与发出的 InputAck `key_batch_seq`
    /// 同一口径；host 侧摘要 / 报告数据源）。
    pub fn key_batches(&self) -> u64 {
        self.key_batches_total
    }

    /// T18：累计收到的 IdrRequest 条数。
    pub fn idr_requests_received(&self) -> u64 {
        self.idr_requests_received_total
    }

    /// T18：调度器在途队列深度峰值（铁律 1：恒 ≤ 2）。
    pub fn in_flight_peak(&self) -> u32 {
        self.in_flight_peak
    }

    /// 发现 1：Detached 期被丢弃的脏/违规候选连接数（观测点）。
    pub fn dirty_candidates(&self) -> u64 {
        self.dirty_candidates_total
    }

    /// 发现 2：Detached 期 accept 的非超时错误数（观测点）。
    pub fn accept_errors(&self) -> u32 {
        self.accept_errors_total
    }

    // ─────────────────────────────────────────────────────────────
    // T20：剪贴板 UTF-8 纯文本双向同步（与 ClientSession 完全对称）
    // ─────────────────────────────────────────────────────────────

    /// 启用剪贴板同步（默认禁用，CB1：敏感面须显式开启）。
    ///
    /// 替换式：重复调用替换后端并重建 [`ClipboardChannel`]（重置基线与回声状态，
    /// T17 重连恢复后应重新调用，避免复用断线前纪元的 `last_written` / 变更计数基线）。
    pub fn enable_clipboard(&mut self, backend: Box<dyn ClipboardBackend>) {
        self.clipboard = Some((ClipboardChannel::new(ClipboardConfig::default()), backend));
    }

    /// 重置剪贴板通道的会话纪元态（保留后端与累计统计，H6）。
    ///
    /// 在新会话落成、重连恢复或驱动器时钟归零时调用，清除旧纪元的变更计数基线、
    /// 回声基准、挂起槽与节流时间戳。未启用剪贴板时为空操作。
    pub fn reset_clipboard_epoch(&mut self) {
        if let Some((channel, _)) = self.clipboard.as_mut() {
            channel.reset_epoch();
        }
    }

    /// 是否已启用剪贴板同步（供报告 JSON `session.clipboard.enabled` 使用）。
    pub fn clipboard_enabled(&self) -> bool {
        self.clipboard.is_some()
    }

    /// 剪贴板本机侧泵（`now_us` = 本端单调会话钟微秒）：
    ///
    /// 未启用或非 [`SessionState::Streaming`] 时为空操作（零开销）。
    /// 产出 [`ClipboardAction::SendToPeer`] 时经 reliable 控制流（Stream A，CB2）
    /// 发送 `ClipboardNotify`。后端错误记 stderr 继续，不拆会话。
    pub fn pump_clipboard(&mut self, now_us: u64) -> Result<()> {
        if self.state != SessionState::Streaming {
            return Ok(());
        }
        let action = {
            let Some((channel, backend)) = self.clipboard.as_mut() else {
                return Ok(());
            };
            match channel.pump_local(backend.as_mut(), now_us) {
                Ok(act) => act,
                Err(e) => {
                    eprintln!("rc-session host: 剪贴板后端轮询失败（不拆会话）: {e}");
                    return Ok(());
                }
            }
        };
        if let ClipboardAction::SendToPeer(text) = action {
            match ControlMessage::clipboard_notify(text) {
                Ok(msg) => {
                    let mut buf = Vec::new();
                    msg.encode(&mut buf);
                    if let Some(t) = self.transport.as_mut() {
                        if let Err(e) = t.send_reliable(&buf) {
                            eprintln!(
                                "rc-session host: 发送 ClipboardNotify 失败（不拆会话）: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("rc-session host: 构造 ClipboardNotify 失败（不拆会话）: {e}");
                }
            }
        }
        Ok(())
    }

    /// 控制分发臂（0x0F，T20）：处理对端发来的 `ClipboardNotify`。
    ///
    /// 未启用时只递增 `received_while_disabled`、不写本机剪贴板、不拆会话（CB1）；
    /// 启用时交由 [`ClipboardChannel::on_peer_text`] 写入并记录回声基准（后端错误
    /// 仅记 stderr，不拆会话）。
    pub fn on_clipboard_notify(&mut self, text: &str) -> Result<()> {
        let Some((channel, backend)) = self.clipboard.as_mut() else {
            self.clipboard_received_while_disabled += 1;
            return Ok(());
        };
        if let Err(e) = channel.on_peer_text(backend.as_mut(), text) {
            eprintln!("rc-session host: 剪贴板后端写入失败（不拆会话）: {e}");
        }
        Ok(())
    }

    /// 剪贴板同步统计快照（含未启用期间收到的 `received_while_disabled`）。
    pub fn clipboard_stats(&self) -> ClipboardStats {
        let mut stats = match &self.clipboard {
            Some((ch, _)) => ch.stats(),
            None => ClipboardStats::default(),
        };
        stats.received_while_disabled = stats
            .received_while_disabled
            .saturating_add(self.clipboard_received_while_disabled);
        stats
    }

    // ─────────────────────────────────────────────────────────────
    // 工具复用通道（0x10）与音频帧通道（0x11）非阻塞收发接口
    // ─────────────────────────────────────────────────────────────

    /// 发送一条工具复用消息（0x10，经 Stream A reliable 控制流写出）。
    ///
    /// 仅 [`SessionState::Streaming`] 允许调用；校验 `channel` 在 `1..=5` 且
    /// `payload.len() <= MAX_TOOL_PAYLOAD_BYTES`。
    pub fn send_tool_message(&mut self, channel: u8, payload: Vec<u8>) -> Result<()> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "send_tool_message requires Streaming state, got {other:?}"
                )))
            }
        }
        let msg = ControlMessage::tool_channel(channel, payload)?;
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.transport
            .as_mut()
            .ok_or_else(|| {
                Error::InvalidConfig("send_tool_message requires a live transport".into())
            })?
            .send_reliable(&buf)
    }

    /// 排空本端已收到的全部工具复用消息 `(channel, payload)`（可靠子通道按到达顺序，
    /// 独立隔离的 `TOOL_CHANNEL_AUDIO_CTRL` 最新控制指令一并附尾排空）。
    pub fn drain_tool_messages(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut out: Vec<(u8, Vec<u8>)> = self.tool_rx.drain(..).collect();
        if let Some(ctrl) = self.audio_ctrl_rx.take() {
            out.push((crate::proto::TOOL_CHANNEL_AUDIO_CTRL, ctrl));
        }
        out
    }

    /// 工具复用接收队列溢出计数（可靠子通道满触发 `Error::Protocol` 的次数）。
    pub fn tool_rx_overflow_dropped(&self) -> u64 {
        self.tool_rx_overflow_dropped
    }

    /// 发送一帧音频数据包（0x11，经 Stream A reliable 控制流写出）。
    ///
    /// 仅 [`SessionState::Streaming`] 允许调用；校验采样率、声道数、编解码与大小上限。
    pub fn send_audio_frame(&mut self, packet: AudioFramePacket) -> Result<()> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "send_audio_frame requires Streaming state, got {other:?}"
                )))
            }
        }
        let msg = ControlMessage::audio_frame(packet)?;
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.transport
            .as_mut()
            .ok_or_else(|| {
                Error::InvalidConfig("send_audio_frame requires a live transport".into())
            })?
            .send_reliable(&buf)
    }

    /// 排空本端已收到的全部音频帧（按到达顺序；接收队列满时自动丢弃最旧帧）。
    pub fn drain_audio_frames(&mut self) -> Vec<AudioFramePacket> {
        self.audio_rx.drain(..).collect()
    }

    /// 在不断开会话的前提下热切换采集/编码流的分辨率与帧率。
    ///
    /// 仅 [`SessionState::Streaming`] 允许调用：
    /// - 目标 `(width, height)` 夹紧至 `[320..=max_w, 240..=max_h]` 并向下取偶，
    ///   其中 `max_w/max_h` 取 `max_resolution`（默认 `self.cfg.caps`），封顶 `3840×2160`；
    /// - 目标 `fps` 夹紧至 `1..=self.cfg.caps.fps`（`None` 或 `Some(0)` 保持当前帧率）；
    /// - 若与当前 `negotiated` 完全一致，直接返回 `Ok(cur)`（幂等空操作，不重建管线）；
    /// - 否则按 S6 停机红线（`stop_capture` → 排空 → `shutdown`）拆除旧管线、推进线上
    ///   `frame_seq` 偏移以保持跨代单调递增，再用 `pipeline_factory_for`（回退
    ///   `pipeline_factory` / auto 选择）按新参数与当前码率构建全新管线、注入会话钟并
    ///   请求首帧 IDR，递增 `pipeline_generation` 后返回最新 [`SessionParams`]。
    pub fn reconfigure_stream(
        &mut self,
        width: u32,
        height: u32,
        fps: Option<u32>,
        max_resolution: Option<rc_types::Resolution>,
    ) -> Result<SessionParams> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "reconfigure_stream requires Streaming state, got {other:?}"
                )))
            }
        }
        let cur = self.negotiated.ok_or_else(|| {
            Error::InvalidConfig("reconfigure_stream requires negotiated session params".into())
        })?;

        let max_res = max_resolution.unwrap_or_else(|| {
            rc_types::Resolution::new(self.cfg.caps.width, self.cfg.caps.height)
        });
        let max_w = max_res.width.clamp(2, 3840) & !1;
        let max_h = max_res.height.clamp(2, 2160) & !1;
        let min_w = 320u32.min(max_w);
        let min_h = 240u32.min(max_h);

        let clamped =
            rc_types::Resolution::new(width.clamp(min_w, max_w), height.clamp(min_h, max_h))
                .to_even();
        let target_w = clamped.width.max(min_w);
        let target_h = clamped.height.max(min_h);

        let max_fps = self.cfg.caps.fps.max(1);
        let target_fps = match fps {
            Some(f) if f > 0 => f.clamp(1, max_fps),
            _ => cur.fps.clamp(1, max_fps),
        };

        if target_w == cur.width && target_h == cur.height && target_fps == cur.fps {
            return Ok(cur);
        }

        // 先按 S6 红线拆除旧管线并释放句柄（确保旧 SCStream / VTEncoder 完全释放），
        // 但保留 tool_rx / audio_ctrl_rx / audio_rx 以免中断并行的工具与音频流。
        self.teardown_pipeline_only();
        self.pipeline = None;
        self.frame_seq_offset = self
            .last_sent_frame_seq
            .map_or(self.frame_seq_offset, |last| last.saturating_add(1));

        let target_bitrate = self.current_bitrate.unwrap_or(cur.bitrate_bps);
        let mut pcfg = self.cfg.pipeline.clone();
        pcfg.resolution = rc_types::Resolution::new(target_w, target_h);
        pcfg.fps = target_fps;
        pcfg.bitrate_bps = target_bitrate;
        pcfg.max_packet_payload = rc_net::quic::max_media_payload(self.cfg.mtu);
        pcfg.validate()?;

        let mut pipeline = if let Some(f) = self.pipeline_factory_for.as_mut() {
            f(&pcfg)?
        } else if let Some(f) = self.pipeline_factory.as_mut() {
            f()?
        } else {
            let capture = rc_pipeline::select_capture(&pcfg)?;
            let encoder = rc_pipeline::select_encoder(&pcfg)?;
            HostPipeline::with_backends_paced(pcfg, capture, encoder, self.cfg.pace_capture)?
        };
        pipeline.set_clock(Arc::clone(&self.clock));
        pipeline.request_idr();

        // 防御性守卫：若自定义测试工厂在重建时重置了时钟零点，避免 wrapping_sub 误触发刷新。
        let now = (self.clock)();
        if let Some(base) = self.refresh_base_us {
            if now < base {
                self.refresh_base_us = Some(now);
            }
        }

        let new_params = SessionParams {
            width: target_w,
            height: target_h,
            fps: target_fps,
            bitrate_bps: target_bitrate,
        };
        self.pipeline = Some(pipeline);
        self.pipeline_alive = true;
        self.pipeline_generation += 1;
        self.negotiated = Some(new_params);
        Ok(new_params)
    }

    /// 主动优雅关闭（停机红线顺序见模块文档与 S6）。
    ///
    /// T17：Detached 态也可关（上层决定放弃保持窗口）——按红线拆除保留
    /// 中的管线后进入 Closed。
    pub fn close(&mut self, reason: ByeReason) -> Result<HostRunSummary> {
        match self.state {
            SessionState::Closed => {
                // 已停机（如 run 里收到 Bye 后退出）：不重复发 Bye，直接给账目。
                let stop = self.closed_reason.clone().unwrap_or(StopReason::Requested);
                return Ok(self.summary(stop));
            }
            SessionState::Streaming | SessionState::Connected | SessionState::Detached => {}
            SessionState::Init | SessionState::Reconnecting => {
                return Err(Error::InvalidConfig(format!(
                    "close requires an established session, got {:?}",
                    self.state
                )))
            }
        }
        self.stop_with(StopReason::Requested, Some(reason));
        Ok(self.summary(StopReason::Requested))
    }

    /// 驱动循环：跑到 `stop()` 为真、收到 Bye 或重连放弃。退出前若未发过
    /// Bye 则补发 Bye(Normal) 并执行停机序列。
    ///
    /// T17：Detached 态续跑 [`HostSession::tick_detached`]（短超时 accept +
    /// 窗口判定 + 守卫；媒体 tick 暂停——采集线程由容量 1 的交接通道天然
    /// 背压阻塞，坑 3），Resumed/Replaced 后自动回到 Streaming 主循环。
    ///
    /// 空闲挂起：host 用 `pipeline.wait_event(≤10ms)`（事件驱动、零自旋，
    /// 照 `run_real`）。
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<HostRunSummary> {
        while !stop() && self.state != SessionState::Closed {
            match self.state {
                SessionState::Streaming => {
                    let r = self.tick(Duration::from_millis(1))?;
                    if r.closed.is_some() {
                        break; // Bye 已在 tick 内完成停机
                    }
                    if !r.busy && self.pipeline_alive {
                        if let Some(p) = self.pipeline.as_ref() {
                            p.wait_event(Duration::from_millis(10));
                        }
                    }
                }
                // T17：断线保持窗口内的驱动拍（accept 阻塞 ≤ 短超时，不自旋）。
                SessionState::Detached => {
                    self.tick_detached(u64::from((self.clock)()))?;
                }
                SessionState::Init
                | SessionState::Connected
                | SessionState::Reconnecting
                | SessionState::Closed => {
                    break; // 非驱动态（host 不进 Reconnecting）：交给调用方
                }
            }
        }
        if self.state == SessionState::Closed {
            let stop_reason = self.closed_reason.clone().unwrap_or(StopReason::Requested);
            return Ok(self.summary(stop_reason));
        }
        self.close(ByeReason::Normal)
    }

    // ─────────────────────────────────────────────────────────────
    // T17：断线重连（Detached / 保持窗口 / ReconnectRequest 裁定 / 饱和守卫）
    // ─────────────────────────────────────────────────────────────

    /// T17：设置断线重连配置（驱动开始前调用；默认 [`ReconnectConfig::default`]）。
    ///
    /// 返回 Err = 退避参数非法（与 [`crate::reconnect::Backoff::new`] 同一
    /// 校验），此时**保持原配置不变**——非法配置不得半生效。
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig) -> Result<()> {
        // 借 Backoff 的校验器过一遍参数（host 不持 Backoff，但两侧配置
        // 同源，非法值必须在这里拦下）。
        crate::reconnect::Backoff::new(
            cfg.backoff_initial,
            cfg.backoff_max,
            cfg.backoff_max_attempts,
        )?;
        self.resume_window = ResumeWindow::new(cfg.resume_ttl);
        self.reconnect = cfg;
        Ok(())
    }

    /// 当前断线重连配置。
    pub fn reconnect_config(&self) -> &ReconnectConfig {
        &self.reconnect
    }

    /// 断线/重连统计（报告与测试的观测点）。
    pub fn reconnect_stats(&self) -> ReconnectStats {
        self.rstats
    }

    /// 管线代数：每建一次管线 +1。窗口内 resume 不变；过期拆除后的下一个
    /// 全新会话 +1。这是「管线不拆」的可观测判据（报告 / 测试用）。
    pub fn pipeline_generation(&self) -> u32 {
        self.pipeline_generation
    }

    /// T17：管线重建工厂（饱和刷新 / 全新 Hello 时调用）。不注入则按首次
    /// 握手的同一构造路径走 auto 后端（select_*，prefer_hardware=false 时
    /// 全假后端 + `pace_capture` 节流，坑 4）；testkit 注入脚本化工厂做
    /// 确定性离线测试。
    pub fn set_pipeline_factory(&mut self, f: Box<dyn FnMut() -> Result<HostPipeline> + Send>) {
        self.pipeline_factory = Some(f);
    }

    /// T18 增补：带裁定参数的管线工厂——闭包收到按 Welcome 裁定值
    /// （resolution/fps/bitrate 已覆盖）构造好的 [`PipelineConfig`]，
    /// CLI 的 fake 静止栈 / real 严格后端需要按**实际裁定参数**建管线
    /// （T17 的 [`Self::set_pipeline_factory`] 闭包看不到裁定值，只适合
    /// caps 恒等于请求值的脚本化测试）。设置后优先于 `set_pipeline_factory`
    /// 与 auto 选择路径。
    pub fn set_pipeline_factory_for(&mut self, f: PipelineFactoryFor) {
        self.pipeline_factory_for = Some(f);
    }

    /// T17：测试接缝——注入一条「新到达的连接」（仅 Detached 态有效）。
    ///
    /// 真网络模式的新连接由 [`HostSession::tick_detached`] 内部短超时
    /// accept 获得；`with_parts`（MockTransport）模式没有端点，由本方法
    /// 注入等价物，随后照常 `tick_detached` 读首条控制消息。
    pub fn offer_transport(&mut self, transport: Box<dyn Transport>) -> Result<()> {
        match self.state {
            SessionState::Detached => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "offer_transport requires Detached state, got {other:?}"
                )))
            }
        }
        self.cfg.approver.cancel();
        self.pending_auth = None;
        self.detached_candidate_deadline_us = None;
        self.peer_addr = FALLBACK_PEER_ADDR;
        self.transport = Some(transport);
        Ok(())
    }

    /// T17：传输层断开处置。进入 Detached、`ResumeWindow::detach(now_us)`、
    /// **不拆管线**。此后驱动循环只跑「短超时 accept + 窗口判定 + 守卫」，
    /// 媒体 tick 暂停——采集线程由容量 1 的交接通道天然背压阻塞
    /// （不是死锁，是省 CPU，铁律 2 同精神）。
    ///
    /// `tick` 收到 `NetEvent::Disconnected` 时内联调用；测试/驱动也可显式
    /// 调用（把 Box 走的传输 drop 掉即触发对端的真实 close）。
    pub fn on_disconnected(&mut self, now_us: u64) {
        if self.state != SessionState::Streaming {
            return; // 幂等：重复 Disconnected 不续期窗口（R1）
        }
        self.cfg.approver.cancel();
        self.pending_auth = None;
        self.detached_candidate_deadline_us = None;
        self.resume_window.detach(now_us);
        self.rstats.disconnects += 1;
        self.transport = None; // 死连接即弃（Drop 关闭）；重连用新 transport
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.state = SessionState::Detached;
    }

    /// Detached 期单个候选连接的握手/鉴权超时预算（微秒，H2）。
    fn detached_candidate_timeout_us(&self) -> u64 {
        let hs_us = u64::try_from(self.cfg.handshake_timeout.as_micros()).unwrap_or(u64::MAX);
        hs_us.max(self.cfg.auth.approval_timeout_us)
    }

    /// T17：Detached 期驱动拍（短超时，防 accept 饿死，坑 1）。顺序：
    /// ① 守卫（Detached 期同样计时，到线照样刷新——见 `on_disconnected`
    /// 文档的「守卫」项）；② 窗口过期 → 按红线拆管线（一次）回
    /// [`DetachOutcome::TornDown`]；③ 无连接 → 短超时 accept（真网络）；
    /// ④ 有连接 → 读新连接控制消息分发（含 T19 密码挑战与人工审批，A2/A6/A8）：
    /// Hello 鉴权终局通过 → [`DetachOutcome::Replaced`]；
    /// ReconnectRequest → [`HostSession::handle_reconnect`]（命中 →
    /// [`DetachOutcome::Resumed`]；未命中连接保持，等对端补发全新 Hello）。
    pub fn tick_detached(&mut self, now_us: u64) -> Result<DetachOutcome> {
        match self.state {
            SessionState::Detached => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "tick_detached requires Detached state, got {other:?}"
                )))
            }
        }
        // ① 饱和守卫：Detached 期管线仍在保留（或等待重建），到线照样
        //    刷新（无连接时 Bye 发不出去——对端 resume 会 miss，同样收敛
        //    到全新 Hello 路径；刷新在途期间不重复触发，见 check_refresh）。
        self.check_refresh(now_us)?;
        // ② 窗口过期 → 拆管线（幂等：拆过即不重复）。
        if !self.resume_window.is_open(now_us) && self.pipeline_alive {
            self.teardown_pipeline();
            return Ok(DetachOutcome::TornDown);
        }
        // ③ 无连接 → 短超时 accept（真网络模式）；超时即当拍 Waiting。
        if self.transport.is_none() {
            let Some(endpoint) = self.endpoint.as_ref() else {
                return Ok(DetachOutcome::Waiting); // mock 模式：等 offer_transport
            };
            match endpoint.accept(DETACHED_ACCEPT_TIMEOUT) {
                Ok(t) => {
                    self.peer_addr = t.remote_addr();
                    self.pending_auth = None;
                    self.detached_candidate_deadline_us =
                        Some(now_us.saturating_add(self.detached_candidate_timeout_us()));
                    self.transport = Some(Box::new(t));
                }
                Err(Error::Timeout(_)) => return Ok(DetachOutcome::Waiting),
                Err(e) => {
                    // 发现 2：坏 TLS 握手包等 accept 错误只计数不上抛——
                    // 单个坏候选不等于端点死亡，Detached 期继续 Waiting
                    // 等下一个连接（上抛会经 run 的 `?` 杀掉整个 host）。
                    self.accept_errors_total += 1;
                    eprintln!("rc-session host: Detached 期 accept 错误（计数继续等）: {e}");
                    return Ok(DetachOutcome::Waiting);
                }
            }
        }
        // H2：为已接纳候选锚定截止时刻（兼容 offer_transport 注入路径），超期即丢候选，
        // 防止未发送 Hello/AuthResponse 的对端永久霸占 `self.transport`。
        let timeout_us = self.detached_candidate_timeout_us();
        let deadline = *self
            .detached_candidate_deadline_us
            .get_or_insert_with(|| now_us.saturating_add(timeout_us));
        if now_us >= deadline {
            self.drop_detached_candidate("Detached 期候选连接握手超时");
            return Ok(DetachOutcome::Waiting);
        }
        // ④ 读新连接控制消息（支持 T19 多轮 Challenge/AuthResponse 与审批轮询）。
        // M1：poll 错误走坏候选丢弃纪律，不上抛杀 host。
        let events = match self
            .transport
            .as_mut()
            .expect("③ 保证 transport 存在")
            .poll(DETACHED_POLL_SLICE)
        {
            Ok(ev) => ev,
            Err(e) => {
                self.drop_detached_candidate(&format!("Detached 期候选连接 poll 失败: {e}"));
                return Ok(DetachOutcome::Waiting);
            }
        };
        for ev in events {
            match ev {
                NetEvent::Reliable(msg) => {
                    // 发现 1：decode 失败的脏消息消化——丢该候选连接
                    //（transport=None、保持 Detached）、计数后继续服务。
                    // Detached 期 host 是接受者，坏候选不等于坏会话，不得上抛杀 host。
                    let decoded = match ControlMessage::decode(&msg) {
                        Ok(m) => m,
                        Err(e) => {
                            self.drop_detached_candidate(&format!(
                                "Detached 期丢弃脏候选消息: {e}"
                            ));
                            return Ok(DetachOutcome::Waiting);
                        }
                    };
                    if self.pending_auth.is_some() {
                        return match decoded {
                            ControlMessage::AuthResponse(resp) => {
                                match self.handle_auth_response(&resp, now_us) {
                                    Ok(outcome) => Ok(self.finish_detached_auth_step(outcome)),
                                    Err(e) => {
                                        self.drop_detached_candidate(&format!(
                                            "Detached 期候选 AuthResponse 处理失败: {e}"
                                        ));
                                        Ok(DetachOutcome::Waiting)
                                    }
                                }
                            }
                            ControlMessage::OwnerAuthResponse { signature } => {
                                match self.handle_owner_auth_response(&signature, now_us) {
                                    Ok(outcome) => Ok(self.finish_detached_auth_step(outcome)),
                                    Err(e) => {
                                        self.drop_detached_candidate(&format!(
                                            "Detached 期候选 OwnerAuthResponse 处理失败: {e}"
                                        ));
                                        Ok(DetachOutcome::Waiting)
                                    }
                                }
                            }
                            other => {
                                self.drop_detached_candidate(&format!(
                                    "Detached 期鉴权中收到非法控制消息 {other:?}"
                                ));
                                Ok(DetachOutcome::Waiting)
                            }
                        };
                    }
                    return match decoded {
                        ControlMessage::Hello(hello) => {
                            match self.begin_hello_auth(&hello, now_us) {
                                Ok(outcome) => Ok(self.finish_detached_auth_step(outcome)),
                                Err(e) => {
                                    self.drop_detached_candidate(&format!(
                                        "Detached 期丢弃违规 Hello 候选: {e}"
                                    ));
                                    Ok(DetachOutcome::Waiting)
                                }
                            }
                        }
                        ControlMessage::ReconnectRequest { session_id, resume } => {
                            let req = ReconnectRequest { session_id, resume };
                            // 复核遗留（发现 1/2 同类未清尽）：候选连接上
                            // Welcome 发送失败（候选将死）不得上抛杀 host
                            // ——坏候选 ≠ 坏会话，同一吞候选纪律：计数、丢连接、
                            // 保持 Detached 继续服务。
                            match self.handle_reconnect(&req, now_us) {
                                Ok(welcome) if welcome.host_flags.resume_accepted() => {
                                    Ok(DetachOutcome::Resumed)
                                }
                                Ok(_) => {
                                    Ok(DetachOutcome::Waiting) // 未命中：连接保持等全新 Hello
                                }
                                Err(e) => {
                                    self.drop_detached_candidate(&format!(
                                        "Detached 期候选重连 Welcome 发送失败: {e}"
                                    ));
                                    Ok(DetachOutcome::Waiting)
                                }
                            }
                        }
                        other => {
                            self.log_ignore_once(ignore_tag_host(&other));
                            continue;
                        }
                    };
                }
                NetEvent::PathChanged => {
                    self.rstats.path_changes += 1;
                }
                NetEvent::Disconnected { .. } => {
                    // 新连接在等待期断开：撤销在途审批（A8），窗口不续期（R1），回到等 accept。
                    self.cfg.approver.cancel();
                    self.pending_auth = None;
                    self.detached_candidate_deadline_us = None;
                    self.transport = None;
                    return Ok(DetachOutcome::Waiting);
                }
                // Detached 期到达的媒体/输入事件丢弃计数（S4 同纪律）。
                NetEvent::Datagram(_) | NetEvent::ReliableUrgent(_) => {
                    self.early_events_dropped += 1;
                }
                NetEvent::Connected => {}
            }
        }
        // 本拍无控制消息但有在途审批：非阻塞轮询审批人（A7/A8）。
        if self.pending_auth.is_some() {
            match self.poll_pending_auth(now_us) {
                Ok(outcome) => return Ok(self.finish_detached_auth_step(outcome)),
                Err(e) => {
                    self.drop_detached_candidate(&format!("Detached 期审批轮询发送失败: {e}"));
                    return Ok(DetachOutcome::Waiting);
                }
            }
        }
        Ok(DetachOutcome::Waiting)
    }

    /// T17：重连裁定。命中（session_id 匹配 && resume=1 && 窗口开 &&
    /// 管线存活）→ 换绑当前 transport、复用管线（**FrameSeq 延续**）、
    /// `pipeline.request_idr()`（pending_idr 粘住直到真编出 IDR，双保险的
    /// host 侧半边）、回 Welcome{host_flags bit0=1}；未命中 → 旧管线按
    /// 红线拆除、回 Welcome{bit0=0}，**连接保持**等待对端补发全新 Hello。
    ///
    /// 返回发出的 Welcome（测试断言点）。调用前提：`self.transport` 是
    /// 新到达的连接（真网络由 `tick_detached` accept；测试经
    /// [`HostSession::offer_transport`] 注入）。
    pub fn handle_reconnect(&mut self, req: &ReconnectRequest, now_us: u64) -> Result<Welcome> {
        // H1：启用密码鉴权、自控免密会话或禁用远程协助时拒绝未鉴权的快速 resume，
        // 迫使对端降级到全新 Hello 走完整鉴权；仅无密码且允许远程协助的会话保留 T17 快速 resume。
        let hit = self.cfg.auth.allow_remote_assist
            && self.cfg.auth.password.is_none()
            && self.authenticated_owner.is_none()
            && req.resume
            && self.session_id == Some(req.session_id)
            && self.resume_window.is_open(now_us)
            && self.pipeline_alive;
        // 发现 6：resume 命中也是刷新 bit1 的消费点之一（对端
        // ReconnectRequest 到达 = 会话仍被对端持有，且此刻对端可能从未
        // 收到过带 bit1 的 Welcome——若 bit1 仍待决，本条 Welcome 补带）。
        let epoch_bit = if hit && self.pending_epoch_reset {
            HostFlags::CLOCK_EPOCH_RESET
        } else {
            0
        };
        let welcome = Welcome {
            session_id: req.session_id,
            granted: self.negotiated.unwrap_or(self.cfg.caps),
            auth_status: AuthStatus::Ok,
            host_flags: if hit {
                HostFlags(HostFlags::RESUME_ACCEPTED | epoch_bit)
            } else {
                HostFlags(0)
            },
        };
        // 先发后迁移（发现 1/2 同类的复核收尾）：发送失败（候选连接将死，
        // send_reliable 仅在候选已知死亡时 Err）时零副作用——状态不推进
        // Streaming、resumes 不计数、旧管线不拆（未命中臂同理），调用方
        // （tick_detached）按「坏候选 ≠ 坏会话」纪律丢弃候选即可，无需
        // 回滚。pending_epoch_reset 也在发送成功后才消费，Welcome 丢失
        // 时 bit1 待决保留、下一条补带（发现 6 语义不变）。
        let mut buf = Vec::new();
        ControlMessage::Welcome(welcome.clone()).encode(&mut buf);
        self.transport
            .as_mut()
            .ok_or_else(|| {
                Error::InvalidConfig("handle_reconnect requires a live transport".into())
            })?
            .send_reliable(&buf)?;
        if hit {
            // 复用管线 + 主动要 IDR（R4 双保险的 host 侧；client 侧还有
            // IdrRequest{reason=2}，pending_idr 粘住语义幂等）。
            if let Some(p) = self.pipeline.as_mut() {
                p.request_idr();
            }
            self.rstats.resumes += 1;
            self.state = SessionState::Streaming;
            self.detached_candidate_deadline_us = None;
            // 发现 5：resume 命中重开保持窗口——下一轮断线从新时刻起算
            // TTL，否则第二轮沿用首轮锚点，必被旧窗口过期错杀成全量重建。
            self.resume_window.reopen();
            self.pending_epoch_reset = false;
            // H6：resume 命中重置剪贴板纪元态，避免沿用断线前的基线与回声状态。
            self.reset_clipboard_epoch();
            self.tool_rx.clear();
            self.audio_ctrl_rx = None;
            self.audio_rx.clear();
        } else {
            // H1：未命中（含密码启用时的 resume 拒绝）绝不在全新 Hello 通过鉴权
            // 与审批前拆除保留管线——保持管线存活直到保持窗口 TTL 过期或认证通过。
        }
        Ok(welcome)
    }

    /// T17：时钟饱和守卫（正常 Streaming 驱动循环每拍由 `tick` 调用；
    /// Detached 期由 `tick_detached` 调用；也可显式驱动）。
    ///
    /// 到线 → 尽力发 Bye{reason=Refresh}（无连接则跳过）→ 按红线拆管线
    /// → 回 accept（Detached，等 client 全新 Hello）。返回 true 表示已
    /// 触发。下一个全新会话的 Welcome 置 host_flags bit1=1（时钟纪元已
    /// 重置；消费点见 `pending_epoch_reset` 字段文档）。
    pub fn check_refresh(&mut self, now_us: u64) -> Result<bool> {
        // 刷新在途（发现 4/6 的配套抑制）：上一轮 Bye(Refresh) 已发出、
        // 新会话尚未落成期间不再重复触发——驱动器归零后的时钟小值会让
        // wrapping 纪元差立即越线（u32 模语义的预期行为），但此刻并不
        // 需要刷新；下一次刷新由新会话落成时重锚的 base 起算。
        if self.pending_epoch_reset {
            return Ok(false);
        }
        if self.state == SessionState::Closed {
            return Ok(false); // 已停机（幂等）
        }
        // 发现 4：u32 会话钟的 wrapping 纪元差（全仓 wrapping 纪律）。
        // 旧判定 `now < base + threshold` 是绝对加法：base 靠近饱和一侧
        // 时和可达 8.4e9 > u32::MAX，第二次刷新数学上不可能触发。改模
        // 比较后，`now.wrapping_sub(base) >= threshold` 即「距 base 已走
        // 过 threshold（且仍在 u32 圆周的安全弧内）」——第二纪元从小值
        // 起算照常在阈值线触发，不归零钟的驱动器按阈值周期性触发。
        // base=None 视作 0：首次刷新即绝对线 now >= threshold（等价）。
        let threshold = self.reconnect.refresh_guard_us;
        let base = self.refresh_base_us.unwrap_or(0);
        if u64::from((now_us as u32).wrapping_sub(base)) < threshold {
            return Ok(false);
        }
        // 尽力发 Bye(Refresh)：无连接（Detached 无 transport）时跳过——
        // 对端的 resume 会 miss，同样收敛到全新 Hello 路径。
        if let Some(t) = self.transport.as_mut() {
            let mut buf = Vec::new();
            ControlMessage::Bye {
                reason: ByeReason::Refresh,
            }
            .encode(&mut buf);
            let _ = t.send_reliable(&buf);
        }
        if self.pipeline_alive {
            self.teardown_pipeline();
        }
        // **不立即 drop 旧连接**：QUIC 的 CONNECTION_CLOSE 会吞掉尚未 ACK
        // 的流数据，Bye 还在路上就关连接 = client 永远看不到刷新信号（只能
        // 走慢速的 resume→miss→降级路径）。保留它直到对端（收到 Bye 后
        // drop 自己的传输）触发 Disconnected，`tick_detached` 自然回收并
        // 转 accept 新连接。
        self.pending_epoch_reset = true;
        self.detached_candidate_deadline_us = None;
        // 发现 4：基准截断存 u32（会话钟域），配合上面的 wrapping 差。
        self.refresh_base_us = Some(now_us as u32);
        self.rstats.refreshes += 1;
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.state = SessionState::Detached;
        Ok(true)
    }

    /// Detached 期丢弃坏候选连接（发现 1/2/3：坏候选 ≠ 坏会话，旧管线
    /// 原封不动，撤销在途审批并回到等待 accept）。
    fn drop_detached_candidate(&mut self, msg: &str) {
        self.cfg.approver.cancel();
        self.pending_auth = None;
        self.detached_candidate_deadline_us = None;
        self.dirty_candidates_total += 1;
        eprintln!("rc-session host: {msg}（丢候选继续服务）");
        self.transport = None;
    }

    /// Detached 期将单步鉴权结果翻译为 [`DetachOutcome`]：
    /// - `Pending` → 保持 Detached（旧管线不动，A6）继续等待；
    /// - `Rejected` → 计数并丢弃候选连接（已回发 `Welcome{Rejected}`），旧管线不动；
    /// - `Accepted` → 拆旧管线（若存活）并落成新会话（`Replaced`）；建管线/发 Welcome
    ///   失败同样只丢候选并恢复 `session_id`。
    fn finish_detached_auth_step(&mut self, outcome: AuthStepOutcome) -> DetachOutcome {
        match outcome {
            AuthStepOutcome::Pending => DetachOutcome::Waiting,
            AuthStepOutcome::Rejected => {
                // M5：给真 QUIC 一个短排空窗口（MockTransport 忽略超时立即返回），
                // 避免 CONNECTION_CLOSE 抢先覆盖在途 Welcome{Rejected}（U11）。
                if let Some(t) = self.transport.as_mut() {
                    let _ = t.poll(Duration::from_millis(50));
                }
                // H3：拒绝出口同步撤销任何在途审批，防止 PendingApproval 期收到
                // AuthResponse 时泄漏 approver 的 in_flight 状态。
                self.cfg.approver.cancel();
                self.pending_auth = None;
                self.detached_candidate_deadline_us = None;
                self.dirty_candidates_total += 1;
                self.transport = None;
                DetachOutcome::Waiting
            }
            AuthStepOutcome::Accepted {
                session_id,
                granted,
                authenticated_owner,
            } => {
                self.pending_auth = None;
                self.detached_candidate_deadline_us = None;
                // 确认全新会话且鉴权已通过（A6 / 发现 3）：此时才拆旧管线。
                if self.pipeline_alive {
                    self.teardown_pipeline();
                }
                let prev_sid = self.session_id;
                match self.finalize_session(session_id, granted) {
                    Ok(()) => {
                        self.authenticated_owner = authenticated_owner;
                        self.rstats.rebuilds += 1;
                        DetachOutcome::Replaced
                    }
                    Err(e) => {
                        self.dirty_candidates_total += 1;
                        eprintln!(
                            "rc-session host: Detached 期候选 Hello 建会话失败（丢候选继续服务）: {e}"
                        );
                        self.transport = None;
                        self.state = SessionState::Detached;
                        self.closed_reason = None;
                        self.session_id = prev_sid;
                        DetachOutcome::Waiting
                    }
                }
            }
        }
    }

    /// 诊断出口（trace/sched_stats/in_flight 等经它取）。
    ///
    /// # Panics
    /// 握手完成（Streaming）之前没有管线，调用即 panic——诊断出口只在
    /// 会话建立后有意义。Closed 后管线仍保留（停机账目可查）。
    pub fn pipeline(&self) -> &HostPipeline {
        self.pipeline
            .as_ref()
            .expect("pipeline exists after handshake (state == Streaming)")
    }

    /// 管线是否有尚未被 `tick` 消费的挂起事件（采集到达或编码完成）。
    pub fn has_pending_pipeline_work(&self) -> bool {
        self.pipeline
            .as_ref()
            .is_some_and(HostPipeline::has_pending_work)
    }

    /// 动态更新注入器的目标屏幕尺寸（例如被控端分辨率热切换后调用）。
    pub fn set_injector_screen_size(&mut self, w: u32, h: u32) {
        self.cfg.injector.set_screen_size(w, h);
    }

    // ─────────────────────────────────────────────────────────────
    // 私有实现
    // ─────────────────────────────────────────────────────────────

    /// 向当前 transport 发送一条控制消息（经 reliable 流写出）。
    fn send_control(&mut self, msg: &ControlMessage) -> Result<()> {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.transport
            .as_mut()
            .ok_or_else(|| Error::InvalidConfig("send_control requires a live transport".into()))?
            .send_reliable(&buf)
    }

    /// 尽力回发 `Welcome{auth_status: Rejected}`（A5：无原因文本上线，
    /// 区分原因已由 `HostAuthenticator` 写入本端 stderr）。
    fn send_welcome_rejected(&mut self, session_id: u64, granted: SessionParams) {
        let reject = ControlMessage::Welcome(Welcome {
            session_id,
            granted,
            auth_status: AuthStatus::Rejected,
            host_flags: HostFlags(0),
        });
        let _ = self.send_control(&reject);
    }

    /// 发送过程消息 `Welcome{auth_status: ChallengeRequired}`（`granted` 填
    /// 预裁定值，client 必须忽略）。
    fn send_welcome_challenge_required(
        &mut self,
        session_id: u64,
        granted: SessionParams,
    ) -> Result<()> {
        let msg = ControlMessage::Welcome(Welcome {
            session_id,
            granted,
            auth_status: AuthStatus::ChallengeRequired,
            host_flags: HostFlags(0),
        });
        self.send_control(&msg)
    }

    /// 握手期（Connected）收到一条 reliable 控制消息的分发。
    fn handle_handshake_reliable(&mut self, msg: &[u8], now_us: u64) -> Result<AuthStepOutcome> {
        let decoded = ControlMessage::decode(msg)?;
        if self.pending_auth.is_some() {
            match decoded {
                ControlMessage::AuthResponse(resp) => self.handle_auth_response(&resp, now_us),
                ControlMessage::OwnerAuthResponse { signature } => {
                    self.handle_owner_auth_response(&signature, now_us)
                }
                other => Err(Error::Protocol {
                    offset: 0,
                    msg: format!("unexpected control message during auth handshake: {other:?}"),
                }),
            }
        } else {
            match decoded {
                ControlMessage::Hello(hello) => self.begin_hello_auth(&hello, now_us),
                other => Err(Error::Protocol {
                    offset: 0,
                    msg: format!(
                        "expected Hello as first reliable message, got {other:?} (refusing)"
                    ),
                }),
            }
        }
    }

    /// 收到 `Hello`：参数夹紧校验 → 构造全新 [`HostAuthenticator`]（A2：每次
    /// 握手/重连全新随机盐，同时共享 `owner_replay_cache` 防重放）→ 执行决策表；
    /// `Wait` 时同拍立即 `poll_approval` 以保持无密码 + `AutoApprover` 路径仅发一条 `Welcome{Ok}`（T14 兼容）。
    fn begin_hello_auth(&mut self, hello: &Hello, now_us: u64) -> Result<AuthStepOutcome> {
        let granted =
            clamp_params_with_aspect(&hello.requested, &self.cfg.caps, self.preserve_caps_aspect);
        if granted.width == 0 || granted.height == 0 || granted.fps == 0 || granted.bitrate_bps == 0
        {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "hello requested degenerate params {granted:?} \
                     (clamped to caps still zero; refusing)"
                ),
            });
        }
        let mut auth = HostAuthenticator::with_replay_cache(
            self.cfg.auth.clone(),
            Arc::clone(&self.owner_replay_cache),
        );
        let action = auth.on_hello(hello, self.peer_addr);
        match action {
            HostAuthAction::Wait => {
                // 行 1（无密码 + None）：同拍立即轮询一次 approver，使
                // AutoApprover 直接出 Conclude(Ok) 而不发 NotifyPending。
                let poll_action = auth.poll_approval(now_us, self.cfg.approver.as_mut());
                let authenticated_owner = auth.authenticated_owner().cloned();
                let concluded = auth.concluded();
                if !concluded {
                    self.pending_auth = Some(PendingAuth {
                        auth,
                        session_id: hello.session_id,
                        granted,
                    });
                }
                self.apply_auth_action(poll_action, hello.session_id, granted, authenticated_owner)
            }
            other => {
                let authenticated_owner = auth.authenticated_owner().cloned();
                let concluded = auth.concluded();
                if !concluded {
                    self.pending_auth = Some(PendingAuth {
                        auth,
                        session_id: hello.session_id,
                        granted,
                    });
                }
                self.apply_auth_action(other, hello.session_id, granted, authenticated_owner)
            }
        }
    }

    /// 收到 `AuthResponse`：校验 proof；通过后同拍立即 `poll_approval`。
    fn handle_auth_response(
        &mut self,
        resp: &crate::proto::AuthResponse,
        now_us: u64,
    ) -> Result<AuthStepOutcome> {
        let (action, session_id, granted, authenticated_owner) = {
            let Some(pending) = self.pending_auth.as_mut() else {
                return Err(Error::Protocol {
                    offset: 0,
                    msg: "received AuthResponse without active auth handshake".into(),
                });
            };
            let first = pending.auth.on_auth_response(resp);
            let final_action = if first == HostAuthAction::Wait {
                pending
                    .auth
                    .poll_approval(now_us, self.cfg.approver.as_mut())
            } else {
                first
            };
            (
                final_action,
                pending.session_id,
                pending.granted,
                pending.auth.authenticated_owner().cloned(),
            )
        };
        if self
            .pending_auth
            .as_ref()
            .is_some_and(|p| p.auth.concluded())
        {
            self.pending_auth = None;
        }
        self.apply_auth_action(action, session_id, granted, authenticated_owner)
    }

    /// 收到 `OwnerAuthResponse`：校验 PoP 签名；通过即直接 `Conclude(Ok)`，免密码免审批。
    fn handle_owner_auth_response(
        &mut self,
        signature: &[u8],
        _now_us: u64,
    ) -> Result<AuthStepOutcome> {
        let (action, session_id, granted, authenticated_owner) = {
            let Some(pending) = self.pending_auth.as_mut() else {
                return Err(Error::Protocol {
                    offset: 0,
                    msg: "received OwnerAuthResponse without active auth handshake".into(),
                });
            };
            let act = pending.auth.on_owner_auth_response(signature);
            (
                act,
                pending.session_id,
                pending.granted,
                pending.auth.authenticated_owner().cloned(),
            )
        };
        if self
            .pending_auth
            .as_ref()
            .is_some_and(|p| p.auth.concluded())
        {
            self.pending_auth = None;
        }
        self.apply_auth_action(action, session_id, granted, authenticated_owner)
    }

    /// 每拍非阻塞轮询在途人工审批（A7/A8）。
    fn poll_pending_auth(&mut self, now_us: u64) -> Result<AuthStepOutcome> {
        let (action, session_id, granted, authenticated_owner) = {
            let Some(pending) = self.pending_auth.as_mut() else {
                return Ok(AuthStepOutcome::Pending);
            };
            if pending.auth.concluded() {
                return Ok(AuthStepOutcome::Pending);
            }
            let act = pending
                .auth
                .poll_approval(now_us, self.cfg.approver.as_mut());
            (
                act,
                pending.session_id,
                pending.granted,
                pending.auth.authenticated_owner().cloned(),
            )
        };
        if self
            .pending_auth
            .as_ref()
            .is_some_and(|p| p.auth.concluded())
        {
            self.pending_auth = None;
        }
        self.apply_auth_action(action, session_id, granted, authenticated_owner)
    }

    /// 将 [`HostAuthAction`] 翻译为线上消息（不启动管线、不断开连接，由调用方按
    /// [`AuthStepOutcome`] 推进状态）。
    fn apply_auth_action(
        &mut self,
        action: HostAuthAction,
        session_id: u64,
        granted: SessionParams,
        authenticated_owner: Option<VerifiedOwnerTicket>,
    ) -> Result<AuthStepOutcome> {
        match action {
            HostAuthAction::Wait => Ok(AuthStepOutcome::Pending),
            HostAuthAction::NotifyPending => {
                self.send_welcome_challenge_required(session_id, granted)?;
                Ok(AuthStepOutcome::Pending)
            }
            HostAuthAction::Challenge(ch) => {
                // 顺序固定：先过程 Welcome{ChallengeRequired}，再 AuthChallenge。
                self.send_welcome_challenge_required(session_id, granted)?;
                self.send_control(&ControlMessage::AuthChallenge(ch))?;
                Ok(AuthStepOutcome::Pending)
            }
            HostAuthAction::Conclude(AuthStatus::Ok) => Ok(AuthStepOutcome::Accepted {
                session_id,
                granted,
                authenticated_owner,
            }),
            HostAuthAction::Conclude(AuthStatus::Rejected)
            | HostAuthAction::Conclude(AuthStatus::ChallengeRequired) => {
                self.send_welcome_rejected(session_id, granted);
                Ok(AuthStepOutcome::Rejected)
            }
        }
    }

    /// 鉴权终局通过后落成会话（A6：管线只在此处启动）：
    /// 建管线 → 发终局 `Welcome{Ok}` → 状态迁移到 Streaming。
    fn finalize_session(&mut self, session_id: u64, granted: SessionParams) -> Result<()> {
        self.session_id = Some(session_id);

        // 裁定管线配置（模板 + 裁定值覆盖），先建管线再回 Welcome——
        // 管线构造失败时对端拿到的是断开而不是无法兑现的裁定。
        let mut pcfg = self.cfg.pipeline.clone();
        pcfg.resolution = rc_types::Resolution::new(granted.width, granted.height);
        pcfg.fps = granted.fps;
        pcfg.bitrate_bps = granted.bitrate_bps;
        // N2 数据报预算：真 QUIC 的单个数据报（32B 包头 + 媒体片）≤
        // `mtu - QUIC_OVERHEAD`（默认 1152），分片预算必须再扣包头——
        // 模板默认的 1200 会产出 1232B 数据报，被 send_datagram 整批拒收
        // （T18 实测的多包大帧空档）。会话层不启用 FEC，用普通预算
        // `max_media_payload(DEFAULT_SESSION_MTU) = 1120` 即可；M3-1 启用
        // FEC 时应切换 `max_media_payload_with_fec`（1088，冗余包自带的
        // 32B 包头要另留预算，见 rc-net 该函数文档）。
        // 此处覆盖对默认工厂与两个注入工厂（闭包收到的就是这份 pcfg）
        // 一体生效。
        pcfg.max_packet_payload = rc_net::quic::max_media_payload(self.cfg.mtu);
        pcfg.validate()?;

        // T17：重建工厂优先（饱和刷新 / 全新 Hello 的脚本化路径）；否则与
        // 首次握手同一构造路径（注入后端 → auto 工厂选择，坑 4 节流）。
        // T18：带裁定参数的工厂最优先（CLI 的 fake 静止栈 / real 严格后端）。
        let mut pipeline = if let Some(f) = self.pipeline_factory_for.as_mut() {
            f(&pcfg)?
        } else {
            match self.pipeline_factory.as_mut() {
                Some(f) => f()?,
                None => {
                    let (capture, encoder) = match self.backends.take() {
                        Some(b) => b,
                        None => (
                            rc_pipeline::select_capture(&pcfg)?,
                            rc_pipeline::select_encoder(&pcfg)?,
                        ),
                    };
                    HostPipeline::with_backends_paced(
                        pcfg,
                        capture,
                        encoder,
                        self.cfg.pace_capture,
                    )?
                }
            }
        };
        pipeline.set_clock(Arc::clone(&self.clock));

        // T17（R6）/发现 6：饱和刷新后的新会话置 bit1（时钟纪元已重置）；
        // 普通全新连接为 0。**发出 Welcome 不即刻消费**——Welcome 可能随
        // 连接一起丢失，对端重试的第二个 Welcome 缺 bit1 会被强拒死锁
        //(client 侧 `refresh_pending && !bit1` 即 Err)；消费点移到「进入
        // Streaming 后收到对端后续证据」或 resume 命中（见
        // `pending_epoch_reset` 字段文档），期间重试 Hello 持续带 bit1。
        let host_flags = if self.pending_epoch_reset {
            HostFlags(HostFlags::CLOCK_EPOCH_RESET)
        } else {
            HostFlags(0)
        };
        let welcome = ControlMessage::Welcome(Welcome {
            session_id,
            granted,
            auth_status: AuthStatus::Ok,
            host_flags,
        });
        self.send_control(&welcome)?;

        self.pipeline = Some(pipeline);
        self.negotiated = Some(granted);
        self.state = SessionState::Streaming;
        // T17：新管线落成——代数 +1、存活标记置位；新会话允许重新发 Bye。
        self.pipeline_generation += 1;
        self.pipeline_alive = true;
        self.bye_sent = false;
        // 发现 4：新会话落成即重锚刷新基准——第二纪元的会话钟由驱动器
        // 归零/重锚后从小值起算，wrapping 纪元差以本会话为零点起算，
        // 下一轮刷新照常在阈值线触发（也消除 Detached 等待期时钟回跳
        // 造成的纪元差越线误判——配合 check_refresh 的在途抑制）。
        self.refresh_base_us = Some((self.clock)());
        // 发现 5：全新会话建立同样重开保持窗口（下一轮断线重新起算）。
        self.resume_window.reopen();
        // 发现 7：不复用上一会话的 SetBitrate 记账——current_bitrate
        // 归 None（回落到本会话协商值；resume 路径不经此处，计数延续）。
        self.current_bitrate = None;
        // 发现 8：全新会话（非 resume）关键批计数归零——client 侧同一
        // session_id 换新后计数也从 1 起，两侧 InputAck `key_batch_seq`
        // 口径对齐；resume 路径不经此处，计数延续。
        self.key_batches_total = 0;
        self.frame_seq_offset = 0;
        self.last_sent_frame_seq = None;
        // H6：新会话落成重置剪贴板纪元态（避免跨纪元时钟归零卡死节流窗口）。
        self.reset_clipboard_epoch();
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.tool_rx_overflow_dropped = 0;
        self.audio_rx.clear();
        Ok(())
    }

    /// 握手失败的收尾：撤销在途审批、关闭连接并返回原错误（auth 拒绝路径已自关）。
    fn handshake_fail(&mut self, e: Error) -> Error {
        self.cfg.approver.cancel();
        self.pending_auth = None;
        self.detached_candidate_deadline_us = None;
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        if self.state != SessionState::Closed {
            self.state = SessionState::Closed;
            self.closed_reason = Some(StopReason::Disconnected(format!("handshake failed: {e}")));
            self.transport = None;
        }
        e
    }

    /// reliable 控制消息的单点分发（S4/S8）。
    ///
    /// 返回 [`ControlFlow::Bye`] 表示对端请求关闭（调用方走停机序列）；
    /// Err = 脏消息 / 协议违规（含 A11：Streaming 期收到鉴权消息一律致命断开）。
    fn dispatch_control(&mut self, msg: &[u8]) -> Result<ControlFlow> {
        let decoded = ControlMessage::decode(msg)?;
        match decoded {
            // A11：鉴权消息（AuthChallenge/AuthResponse/OwnerAuthResponse/Welcome）仅握手期有效，
            // Streaming 期到达一律致命 Error::Protocol 断开。
            other @ (ControlMessage::AuthChallenge(_)
            | ControlMessage::AuthResponse(_)
            | ControlMessage::OwnerAuthResponse { .. }
            | ControlMessage::Welcome(_)) => Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "auth/handshake message {other:?} is forbidden in Streaming state (A11)"
                ),
            }),
            ControlMessage::IdrRequest { .. } => {
                self.idr_requests_received_total += 1;
                if let Some(p) = self.pipeline.as_mut() {
                    p.request_idr();
                }
                Ok(ControlFlow::Handled)
            }
            // T15（0x05）：ProbePing → 无状态应答 ProbePong（reliable 流）。
            msg @ ControlMessage::ProbePing { .. } => {
                // 应答钟 = 本端会话钟 u32 零扩展（B9：与 client 探测钟同源
                // 同纪元）；pong_for 的 echoed 原样回显 t0、responder 打 t1。
                // T17：应答尽力而为——连接将死时的发送失败由 poll 的断线
                // 信号接管（Detached），不值得为一条 pong 拆会话。
                let responder_now_us = u64::from((self.clock)());
                let pong = crate::probe::pong_for(&msg, responder_now_us)
                    .expect("ProbePing 臂的输入必为 ProbePing 变体");
                let mut buf = Vec::new();
                pong.encode(&mut buf);
                if let Some(t) = self.transport.as_mut() {
                    let _ = t.send_reliable(&buf);
                }
                Ok(ControlFlow::Handled)
            }
            // T15（0x07）：SetBitrate → HostPipeline::set_bitrate 转发。
            // 应用失败只记 stderr 不拆会话（一次码率调整不值得杀连接）。
            ControlMessage::SetBitrate { bps, reason } => {
                if let Err(e) = self.on_set_bitrate(bps, reason as u8) {
                    eprintln!(
                        "rc-session host: SetBitrate({bps}, reason={}) 应用失败（不拆会话）: {e}",
                        reason as u8
                    );
                }
                Ok(ControlFlow::Handled)
            }
            // T15（0x08）：BwProbeRequest → 逐块 send_datagram（datagram
            // 通道，不占控制流——设计师定义）。失败只记 stderr 不拆会话。
            msg @ ControlMessage::BwProbeRequest { .. } => {
                if let Err(e) = self.on_bw_probe_request(&msg) {
                    eprintln!("rc-session host: BwProbeRequest 处理失败（不拆会话）: {e}");
                }
                Ok(ControlFlow::Handled)
            }
            // T20（0x0F）：ClipboardNotify → 剪贴板文本同步（双向；未启用只计数不写本机不拆会话，CB1）。
            ControlMessage::ClipboardNotify { text } => {
                if let Err(e) = self.on_clipboard_notify(&text) {
                    eprintln!("rc-session host: ClipboardNotify 处理失败（不拆会话）: {e}");
                }
                Ok(ControlFlow::Handled)
            }
            // 0x10 ToolChannel → `TOOL_CHANNEL_AUDIO_CTRL` 独立最新值隔离；其余可靠子通道入
            // 有界接收队列（上限 MAX_PENDING_TOOL_MESSAGES），满时绝不静默丢帧，返回协议错误。
            ControlMessage::ToolChannel { channel, payload } => {
                if channel == crate::proto::TOOL_CHANNEL_AUDIO_CTRL {
                    self.audio_ctrl_rx = Some(payload);
                    Ok(ControlFlow::Handled)
                } else if self.tool_rx.len() < MAX_PENDING_TOOL_MESSAGES {
                    self.tool_rx.push_back((channel, payload));
                    Ok(ControlFlow::Handled)
                } else {
                    self.tool_rx_overflow_dropped += 1;
                    Err(Error::Protocol {
                        offset: 0,
                        msg: "tool channel queue overflow".into(),
                    })
                }
            }
            // 0x11 AudioFrame → 入有界音频接收队列（上限 32，满时丢弃最旧帧保低延迟）。
            ControlMessage::AudioFrame {
                seq,
                timestamp_us,
                sample_rate,
                channels,
                codec,
                data,
            } => {
                while self.audio_rx.len() >= MAX_PENDING_AUDIO_FRAMES {
                    self.audio_rx.pop_front();
                }
                self.audio_rx.push_back(AudioFramePacket {
                    seq,
                    timestamp_us,
                    sample_rate,
                    channels,
                    codec,
                    data,
                });
                Ok(ControlFlow::Handled)
            }
            ControlMessage::Bye { reason } => Ok(ControlFlow::Bye(reason)),
            other => {
                self.log_ignore_once(ignore_tag_host(&other));
                Ok(ControlFlow::Handled)
            }
        }
    }

    /// 控制分发臂（0x07，T15）：SetBitrate → [`HostPipeline::set_bitrate`]
    /// 转发（编码线程 ≤5ms 内应用）。
    ///
    /// `bps == 0` 由管线侧拒绝（Err InvalidConfig，不发命令）；reason 仅
    /// 记 stderr 摘要与统计，不影响行为。成功时会话侧记账
    /// （[`HostSession::current_bitrate_bps`]）。
    pub fn on_set_bitrate(&mut self, bps: u32, reason: u8) -> Result<()> {
        let pipeline = self.pipeline.as_mut().ok_or_else(|| {
            Error::InvalidConfig("on_set_bitrate requires an established pipeline".into())
        })?;
        pipeline.set_bitrate(bps)?;
        self.current_bitrate = Some(bps);
        eprintln!("rc-session host: SetBitrate {bps}bps (reason={reason}) 已下发编码线程");
        Ok(())
    }

    /// 控制分发臂（0x08，T15）：BwProbeRequest → [`crate::probe::bw_probe_chunks`]
    /// 生成块序列 → 逐块 `transport.send_datagram`（走 datagram 通道，
    /// 不占控制流——设计师定义）。total_bytes 超限/为零 → Err（管线侧
    /// 拒绝），不发任何块。
    pub fn on_bw_probe_request(&mut self, req: &ControlMessage) -> Result<()> {
        let ControlMessage::BwProbeRequest {
            probe_id,
            total_bytes,
        } = req
        else {
            return Err(Error::InvalidConfig(
                "on_bw_probe_request expects a BwProbeRequest message".into(),
            ));
        };
        let chunks = crate::probe::bw_probe_chunks(*probe_id, *total_bytes)?;
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| Error::InvalidConfig("bw probe requires a live transport".into()))?;
        for chunk in &chunks {
            transport.send_datagram(chunk)?;
            // 发现 10：探测块单独计数——datagrams_sent 的文档语义只计
            // 媒体数据报，混入 BwProbe 块会让摘要的媒体口径失真。
            self.probe_datagrams_sent_total += 1;
        }
        Ok(())
    }

    /// 当前生效目标码率（协商值或最近一次 SetBitrate；诊断与测试断言用。
    /// 会话侧自行记账，不查编码线程——编码器在管线线程内）。
    pub fn current_bitrate_bps(&self) -> u32 {
        self.current_bitrate
            .or_else(|| self.negotiated.map(|n| n.bitrate_bps))
            .unwrap_or(0)
    }

    /// stderr 记一次忽略日志（按标签限流，不刷屏）。
    fn log_ignore_once(&mut self, tag: &'static str) {
        if !self.logged_ignore.contains(&tag) {
            self.logged_ignore.push(tag);
            eprintln!("rc-session host: 忽略消息 {tag}（后续卡接入，S8）");
        }
    }

    /// 发现 6：消费饱和刷新 bit1 的待决态（`pending_epoch_reset`）。
    /// 消费点 = 对端后续证据：Streaming 中该连接首条 Reliable/Urgent
    /// 消息到达（对端显然收到了 Welcome 并继续使用会话），或 resume
    /// 命中（`handle_reconnect`）。此前对同一 client 的重试 Hello 持续
    /// 带 bit1——Welcome 丢失不得让对端死锁在强拒上。
    fn maybe_consume_epoch_reset(&mut self) {
        if self.pending_epoch_reset {
            self.pending_epoch_reset = false;
        }
    }

    /// 逐条发送数据报（尽力）：连接已断时丢弃剩余（网络丢失，如实计数）。
    /// 返回成功发出的条数。
    fn send_datagrams_best_effort(&mut self, datagrams: &[Vec<u8>]) -> usize {
        let mut sent = 0usize;
        let transport = match self.transport.as_mut() {
            Some(t) => t,
            None => return 0,
        };
        for d in datagrams {
            match transport.send_datagram(d) {
                Ok(()) => sent += 1,
                Err(_) => break, // 连接已死：剩余按网络丢失处理，不缓存（S7）
            }
        }
        self.datagrams_sent_total += sent;
        sent
    }

    /// S6 停机序列（幂等：Closed 后再调为空操作）。
    ///
    /// `send_bye` 为 Some 时先尽力发一条 Bye（短路径、不等待确认）。
    fn stop_with(&mut self, reason: StopReason, send_bye: Option<ByeReason>) {
        if self.state == SessionState::Closed {
            return;
        }
        self.cfg.approver.cancel();
        self.pending_auth = None;
        self.detached_candidate_deadline_us = None;
        if let Some(why) = send_bye {
            if !self.bye_sent {
                let mut buf = Vec::new();
                ControlMessage::Bye { reason: why }.encode(&mut buf);
                if let Some(t) = self.transport.as_mut() {
                    if t.send_reliable(&buf).is_ok() {
                        self.bye_sent = true;
                    }
                }
            }
        }
        // T17（R7）：任何路径拆管线都走停机红线（同一实现）。
        self.teardown_pipeline();
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.closed_reason = Some(reason);
        self.state = SessionState::Closed;
        // 断开连接：QuicTransport 的 Drop 关闭连接（尽力 Bye 已在前面发出）。
        self.transport = None;
    }

    /// S6 停机红线的管线拆除段（同时清空工具/音频接收队列；幂等）。
    fn teardown_pipeline(&mut self) {
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.teardown_pipeline_only();
    }

    /// S6 停机红线的纯管线拆除段（幂等：已拆即空操作；管线引用保留供
    /// 停机账目查询，存活标记 [`Self::pipeline_alive`] 置 false；不清空工具/音频队列，
    /// 供 [`Self::reconfigure_stream`] 在不断会热切换时复用）。
    ///
    /// ① `stop_capture` → ② tick 排空到静默（交接通道空 +
    /// `has_pending_encodes()==false`，500ms 上限）→ ③ `shutdown` →
    /// ④ 末轮 tick 收编码完成并发送 → ⑤ 账目清点。反序（先 shutdown
    /// 再 tick）会把新提交发给已退出的编码线程，授权帧凭空消失。
    fn teardown_pipeline_only(&mut self) {
        if !self.pipeline_alive {
            return;
        }
        self.pipeline_alive = false;
        let Some(p) = self.pipeline.as_mut() else {
            return;
        };
        // ① 停采集（先于编码线程，红线见 HostPipeline::stop_capture 文档）。
        p.stop_capture();
        // ② tick 排空到静默：交接通道空 + 无在途编码；500ms 上限。
        // 发送与计数只用 transport / 计数两个字段（与 pipeline
        // 借用分字段共存，不整 self 借用）。
        let mut quiet = 0u32;
        let mut rounds = 0u32;
        while quiet < DRAIN_QUIET_ROUNDS && rounds < DRAIN_HARD_ROUNDS {
            rounds += 1;
            match p.tick((self.clock)()) {
                Ok(Some(mut datagrams)) => {
                    stamp_and_track_datagrams(
                        &mut datagrams,
                        self.frame_seq_offset,
                        &mut self.last_sent_frame_seq,
                    );
                    quiet = 0;
                    // 内联发送：方法调用是整 self 借用，会与 pipeline
                    // 借用冲突；分字段（transport / 计数）直接访问。
                    if let Some(t) = self.transport.as_mut() {
                        for d in &datagrams {
                            if t.send_datagram(d).is_err() {
                                break; // 连接已死：剩余按网络丢失（S7）
                            }
                            self.datagrams_sent_total += 1;
                        }
                    }
                }
                Ok(None) => {
                    if !p.has_pending_encodes() {
                        break;
                    }
                    quiet += 1;
                    p.wait_event(DRAIN_WAIT);
                }
                Err(_) => break, // 排空中管线报错：不强推，账目由结案兜底
            }
        }
        // ③ 停编码线程（排空已提交请求）。
        p.shutdown();
        // ④ 末轮 tick 收编码完成并发送。
        if let Ok(Some(mut datagrams)) = p.tick((self.clock)()) {
            stamp_and_track_datagrams(
                &mut datagrams,
                self.frame_seq_offset,
                &mut self.last_sent_frame_seq,
            );
            if let Some(t) = self.transport.as_mut() {
                for d in &datagrams {
                    if t.send_datagram(d).is_err() {
                        break;
                    }
                    self.datagrams_sent_total += 1;
                }
            }
        }
        // ⑤ 账目闭合（S6）：rc-pipeline 的 HostPipeline 只暴露不可变
        //    trace()，host 侧无法替帧写结局——这是**设计使然**：已编码
        //    发出帧的最终判定权在接收侧（controller trace），双端时间线
        //    由 `rc_pipeline::merge_traces` 收口。因此这里只**清点**：
        //    「已发出（Sent 打点）但无结局」的帧记入 sent_unresolved，
        //    摘要的 unfinished_traces = trace 未结数 − 已发出未判定数
        //    （扣除后非 0 = 真有帧在发出前凭空消失）。
        self.sent_unresolved = p
            .trace()
            .frames()
            .iter()
            .filter(|f| f.outcome().is_none() && f.at(rc_trace::Mark::Sent).is_some())
            .count() as u64;
    }

    /// 组装运行摘要。
    fn summary(&self, stop_reason: StopReason) -> HostRunSummary {
        // 调度器统计一次取齐（SchedulerStats 是 Copy；rc-pipeline 未
        // re-export 该类型，此处按字段内联，不引类型名）。
        let (encoded, unfinished, captured, dropped_stale, dropped_repeat, dropped_budget) =
            match &self.pipeline {
                Some(p) => {
                    let s = p.sched_stats();
                    (
                        s.encoded,
                        p.trace()
                            .summary()
                            .unfinished
                            .saturating_sub(self.sent_unresolved),
                        s.captured,
                        s.dropped_stale,
                        s.dropped_repeat,
                        s.dropped_budget,
                    )
                }
                None => (0, 0, 0, 0, 0, 0),
            };
        HostRunSummary {
            stop_reason,
            datagrams_sent: self.datagrams_sent_total,
            probe_datagrams_sent: self.probe_datagrams_sent_total,
            events_injected: self.events_injected_total,
            frames_encoded: encoded,
            unfinished_traces: unfinished,
            captured,
            dropped_stale,
            dropped_repeat,
            dropped_budget,
            clipboard: crate::report::ClipboardReport::from_stats(
                self.clipboard_enabled(),
                self.clipboard_stats(),
            ),
        }
    }
}

/// T18：带裁定参数的管线工厂类型（CLI 组合根注入；[`HostSession::
/// set_pipeline_factory_for`]）。
pub type PipelineFactoryFor = Box<dyn FnMut(&PipelineConfig) -> Result<HostPipeline> + Send>;

/// 控制分发结果。
enum ControlFlow {
    /// 已处理（含 S8 的忽略路径）。
    Handled,
    /// 对端 Bye（reason 透传）。
    Bye(ByeReason),
}

/// Detached 期一拍驱动的迁移结果（[`HostSession::tick_detached`] 返回值，
/// 测试断言点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachOutcome {
    /// 窗口内继续等待（本拍 accept 无连接 / 首条消息未到）。
    Waiting,
    /// 窗口过期，旧管线已按停机红线拆除（stop_capture → tick 排空 →
    /// shutdown）；后续拍继续 accept 等全新会话。
    TornDown,
    /// 收到 ReconnectRequest 且命中：管线复用（FrameSeq 延续），回到
    /// Streaming。
    Resumed,
    /// 收到全新 Hello：旧管线拆除、新会话建立（FrameSeq / 时钟归零，
    /// 管线代数 +1）。
    Replaced,
}

/// 忽略标签（S8 限流日志用；host 视角）。ProbePing/SetBitrate/
/// BwProbeRequest（T15）已接入分发臂；IdrAck 由 T16 在发送侧产出，
/// 到达即方向错误。
fn ignore_tag_host(m: &ControlMessage) -> &'static str {
    match m {
        ControlMessage::Hello { .. } => "Hello(streaming 中重复到达)",
        ControlMessage::Welcome { .. } => "Welcome(方向错误)",
        ControlMessage::IdrAck { .. } => "IdrAck(方向错误)",
        ControlMessage::ProbePong { .. } => "ProbePong(T17+ host 主动发起)",
        ControlMessage::BwProbeData { .. } => "BwProbeData(方向错误)",
        // host 是 InputAck（T18）的发送侧；到达即方向错误。
        ControlMessage::InputAck { .. } => "InputAck(方向错误：host 是发送侧)",
        ControlMessage::ReconnectRequest { .. } => {
            "ReconnectRequest(Streaming 中到达；裁定只在 Detached)"
        }
        ControlMessage::AuthChallenge(_) => "AuthChallenge(仅握手期有效)",
        ControlMessage::AuthResponse(_) => "AuthResponse(仅握手期有效)",
        ControlMessage::ClipboardNotify { .. } => "ClipboardNotify(非 Streaming 态到达，忽略)",
        ControlMessage::ToolChannel { .. } => "ToolChannel(非 Streaming 态到达，忽略)",
        ControlMessage::AudioFrame { .. } => "AudioFrame(非 Streaming 态到达，忽略)",
        _ => "未知方向消息",
    }
}

/// 一拍的摘要（[`HostSession::tick`] 返回值）。
///
/// 注：任务卡给本结构标了 `Copy`，但 `closed: Option<StopReason>` 含
/// `String`（`StopReason::Disconnected`）不可 `Copy`——按实际字段降为
/// `Clone`（任务卡 API 契约与 StopReason 定义自相矛盾处的最小修正）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostTickReport {
    /// 本拍发出的媒体数据报数。
    pub datagrams_sent: usize,
    /// 本拍注入的输入事件数。
    pub events_injected: usize,
    /// 本拍处理的控制消息数（含 S8 忽略路径）。
    pub control_msgs_handled: usize,
    /// 本拍有任何事件/产出；run 在 !busy 时挂起等待（坑 2）。
    pub busy: bool,
    /// 本拍内进入 Closed 的原因（停机序列已完成）。
    pub closed: Option<StopReason>,
}

/// 会话运行摘要（[`HostSession::close`]/[`HostSession::run`] 返回值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRunSummary {
    /// 结束原因。
    pub stop_reason: StopReason,
    /// 全程发出的媒体数据报数（发送成功计；**不含**带宽探测块——发现 10
    /// 拆分后本字段回到纯媒体口径）。
    pub datagrams_sent: usize,
    /// 全程发出的带宽探测数据报数（BwProbe 块；发现 10 增字段，CLI 摘要
    /// 的打印由并行卡接入）。
    pub probe_datagrams_sent: usize,
    /// 全程注入的输入事件数。
    pub events_injected: u64,
    /// 调度器授权编码数（== 有结局帧数，S6 断言）。
    pub frames_encoded: u64,
    /// 停机后 trace 未结帧数（扣除「已发出、判定权在对端」的帧），
    /// 必须为 0（S5/S6 断言）：非 0 = 有帧在发出前凭空消失。已编码发出
    /// 帧的最终结局由接收侧 trace 经 `rc_pipeline::merge_traces` 收口。
    pub unfinished_traces: u64,
    // ── 调度器帧去向（2026-11-21 localhost 真后端排查增补：30s 会话
    //    0 帧编码时因缺这几项盲飞，现停机时从 `pipeline.sched_stats()`
    //    透传；多代会话只报末代管线，与 frames_encoded 同口径）──
    /// 调度器看到的帧总数（进 `on_captured` 即计）。
    pub captured: u64,
    /// 因超期丢弃数（deadline = capture_ts + target_latency 已过——
    /// 飙升通常意味着会话钟与采集时间戳零点错位，见 rc-cli host 的
    /// real_host_stack 顺序红线）。
    pub dropped_stale: u64,
    /// 因重复帧丢弃数（铁律 2：静止屏零编码属正常）。
    pub dropped_repeat: u64,
    /// 因在途预算满丢弃数（铁律 1：编码器吞吐跟不上时出现）。
    pub dropped_budget: u64,
    /// T20：剪贴板同步报告段（嵌入 host 报告 `session.clipboard`）。
    pub clipboard: crate::report::ClipboardReport,
}

/// 将热切换后的 `frame_seq_offset` 叠加到线上媒体数据报的 `frame_seq` 字段（偏移 4..12 LE），
/// 并刷新本会话已发出的最高线上 `frame_seq`，确保 `ControllerPipeline` 在热切换后不丢帧。
fn stamp_and_track_datagrams(
    datagrams: &mut [Vec<u8>],
    frame_seq_offset: u64,
    last_sent_frame_seq: &mut Option<u64>,
) {
    for d in datagrams {
        if d.len() < 12 {
            continue;
        }
        let raw_seq = u64::from_le_bytes([d[4], d[5], d[6], d[7], d[8], d[9], d[10], d[11]]);
        let wire_seq = raw_seq.saturating_add(frame_seq_offset);
        if frame_seq_offset > 0 {
            d[4..12].copy_from_slice(&wire_seq.to_le_bytes());
        }
        *last_sent_frame_seq =
            Some(last_sent_frame_seq.map_or(wire_seq, |prev| prev.max(wire_seq)));
    }
}

/// 在最大包络 `(max_w, max_h)` 内按 `(aspect_w, aspect_h)` 等比缩放并偶数对齐，
/// 使输出宽高既不超过包络又保持与源显示器一致的宽高比（避免采集层补黑边）。
fn fit_even_preserving_aspect(
    max_w: u32,
    max_h: u32,
    aspect_w: u32,
    aspect_h: u32,
) -> rc_types::Resolution {
    if max_w < 2 || max_h < 2 || aspect_w == 0 || aspect_h == 0 {
        return rc_types::Resolution::new(max_w, max_h).to_even();
    }
    let max_even_w = max_w & !1;
    let max_even_h = max_h & !1;
    if u64::from(max_w) * u64::from(aspect_h) <= u64::from(max_h) * u64::from(aspect_w) {
        // 宽度受限：以最大偶宽为准，等比求最接近的偶高
        let w = max_even_w;
        let ideal_h = (f64::from(w) * f64::from(aspect_h) / f64::from(aspect_w)).round() as u32;
        let h = ((ideal_h + 1) & !1).min(max_even_h).max(2);
        rc_types::Resolution::new(w, h)
    } else {
        // 高度受限：以最大偶高为准，等比求最接近的偶宽
        let h = max_even_h;
        let ideal_w = (f64::from(h) * f64::from(aspect_w) / f64::from(aspect_h)).round() as u32;
        let w = ((ideal_w + 1) & !1).min(max_even_w).max(2);
        rc_types::Resolution::new(w, h)
    }
}

/// 把 client 请求参数逐字段 min 夹紧到 host 能力，分辨率过 `to_even`（S2）；
/// 当 `preserve_caps_aspect == true` 时在夹紧包络内保持 `caps` 的宽高比。
fn clamp_params_with_aspect(
    requested: &SessionParams,
    caps: &SessionParams,
    preserve_caps_aspect: bool,
) -> SessionParams {
    let max_w = requested.width.min(caps.width);
    let max_h = requested.height.min(caps.height);
    let res = if preserve_caps_aspect {
        fit_even_preserving_aspect(max_w, max_h, caps.width, caps.height)
    } else {
        rc_types::Resolution::new(max_w, max_h).to_even()
    };
    SessionParams {
        width: res.width,
        height: res.height,
        fps: requested.fps.min(caps.fps),
        bitrate_bps: requested.bitrate_bps.min(caps.bitrate_bps),
    }
}

// ─────────────────────────────────────────────────────────────────
// 测试：全部离线确定性（mock transport + 假后端，无网络、无直接 sleep；
// 帧等待经管线自身的 wait_event 挂起，与 rc-pipeline 测试同一手法）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{self, Hello, PROTOCOL_VERSION};
    use crate::testkit::MockTransport;
    use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
    use rc_encode::{EncoderConfig, FakeEncoder};
    use rc_input::{encode_events, InputEvent, InputInjector, MouseButton};
    use rc_net::NetEvent;
    use rc_trace::Mark;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    /// 假采集的帧间隔（30fps → 33334µs；FakeBackend 向上取整）。
    const FRAME_INTERVAL_US: u32 = 1_000_000u32.div_ceil(30);

    /// 记录型注入器：事件经 Arc 共享给测试断言（会话把它 Box 走了）。
    struct RecordingInjector {
        events: std::sync::Arc<Mutex<Vec<InputEvent>>>,
    }

    impl RecordingInjector {
        fn new() -> (Self, std::sync::Arc<Mutex<Vec<InputEvent>>>) {
            let events = std::sync::Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    impl InputInjector for RecordingInjector {
        fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
            self.events
                .lock()
                .expect("injector lock")
                .extend_from_slice(events);
            Ok(())
        }
        fn set_screen_size(&mut self, _w: u32, _h: u32) {}
    }

    /// 计数编码器：包一层 FakeEncoder，数「实际完成」的编码次数
    /// （S6 断言 sched_stats().encoded 与之相等 = 无授权帧凭空消失）。
    struct CountingEncoder {
        inner: Box<dyn Encoder>,
        completed: Arc<AtomicU64>,
    }

    impl Encoder for CountingEncoder {
        fn encode(
            &mut self,
            frame: &rc_capture::CapturedFrame,
            force_idr: bool,
            deadline_ts: rc_types::Timestamp,
        ) -> Result<rc_encode::EncodedFrame> {
            let out = self.inner.encode(frame, force_idr, deadline_ts)?;
            self.completed.fetch_add(1, Ordering::Relaxed);
            Ok(out)
        }
        fn capabilities(&self) -> rc_encode::EncoderCaps {
            self.inner.capabilities()
        }
        fn set_bitrate(&mut self, bps: u32) -> Result<()> {
            self.inner.set_bitrate(bps)
        }
        fn request_idr(&mut self) {
            self.inner.request_idr()
        }
        fn flush(&mut self) -> Result<Vec<rc_encode::EncodedFrame>> {
            self.inner.flush()
        }
    }

    /// host 能力上界（64×64@30、1Mbps——小分辨率测试瞬时）。
    fn caps() -> SessionParams {
        SessionParams {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 1_000_000,
        }
    }

    /// 管线模板（与 caps 对齐的 64×64@30）。
    fn pcfg() -> PipelineConfig {
        PipelineConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 30,
            bitrate_bps: 1_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 1024,
            diff: rc_pipeline::DiffMode::TrustPlatform,
            clock_origin_us: None,
            // 模板取默认预算；真 QUIC 路径由 expect_hello 按 N2 覆盖为 1120。
            max_packet_payload: rc_proto::MAX_PAYLOAD,
        }
    }

    fn host_cfg(injector: Box<dyn InputInjector>) -> HostSessionConfig {
        HostSessionConfig {
            listen_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            caps: caps(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_millis(500),
            injector,
            pace_capture: true, // 假后端必须节流（坑 4）
            auth: HostAuthConfig::default(),
            approver: Box::new(AutoApprover),
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

    /// 恒 0 会话钟：调度器 now=0 恒小于帧 deadline，帧不会被误判超期。
    fn zero_clock() -> SessionClock {
        Arc::new(|| 0)
    }

    /// 一条合法 Hello（可指定请求参数与鉴权方式）。
    fn hello_msg(requested: SessionParams, auth: proto::AuthMethod) -> ControlMessage {
        ControlMessage::Hello(Hello {
            version: PROTOCOL_VERSION,
            session_id: 4242,
            requested,
            auth_method: auth,
            auth_payload: vec![],
        })
    }

    /// 预置 Hello 并完成握手的会话（返回会话与传输句柄）。
    fn handshaken(
        injector: Box<dyn InputInjector>,
        requested: SessionParams,
    ) -> (HostSession, MockTransport) {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(requested, proto::AuthMethod::None));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(injector),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        host.handshake().expect("handshake");
        (host, handle)
    }

    /// 可变注入钟（铁律 4：仍由测试注入，不读墙钟）——发现 4/5 的双纪元
    /// 与双轮断线场景需要测试推进会话钟。槽存 u64，闭包内饱和到 u32 域。
    fn mutable_clock() -> (Arc<Mutex<u64>>, SessionClock) {
        let slot = Arc::new(Mutex::new(0u64));
        let clock: SessionClock = {
            let slot = Arc::clone(&slot);
            Arc::new(move || u32::try_from(*slot.lock().expect("clock lock")).unwrap_or(u32::MAX))
        };
        (slot, clock)
    }

    /// 指定注入钟的握手会话（发现 4/5 用；其余与 [`handshaken`] 同）。
    fn handshaken_at(
        injector: Box<dyn InputInjector>,
        clock: SessionClock,
    ) -> (HostSession, MockTransport) {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(injector),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            clock,
        )
        .expect("with_parts");
        host.handshake().expect("handshake");
        (host, handle)
    }

    /// 驱动会话直到新数据报产出（或超轮数 panic）。返回产出前后的总量。
    fn drive_until_datagram(host: &mut HostSession, handle: &MockTransport, max_rounds: usize) {
        let prev = handle.datagrams_sent().len();
        for _ in 0..max_rounds {
            host.tick(Duration::from_millis(0)).expect("tick");
            if handle.datagrams_sent().len() > prev {
                return;
            }
            host.pipeline().wait_event(Duration::from_millis(2));
        }
        panic!("drive_until_datagram：{max_rounds} 轮内没有新数据报（管线饿死）");
    }

    /// S2/S3：Hello 请求超过 caps → Welcome 回夹紧值；session_id 回显一致；
    /// state==Streaming。
    #[test]
    fn host_handshake_replies_welcome_with_clamped_params() {
        let (inj, _events) = RecordingInjector::new();
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(
            SessionParams {
                width: 4000,
                height: 2000,
                fps: 144,
                bitrate_bps: 50_000_000,
            },
            proto::AuthMethod::None,
        ));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");

        let granted = host.handshake().expect("handshake ok");
        assert_eq!(granted, caps(), "请求逐字段 min 夹紧到 caps");
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.session_id(), Some(4242));
        assert_eq!(host.negotiated(), Some(caps()));

        let msgs = handle.reliable_decoded();
        assert_eq!(msgs.len(), 1, "握手只发一条 Welcome");
        match &msgs[0] {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.session_id, 4242, "session_id 必须回显");
                assert_eq!(w.granted, caps());
                assert_eq!(w.auth_status, AuthStatus::Ok);
                assert_eq!(w.host_flags, HostFlags(0));
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }
    }

    /// 分辨率奇数值过 to_even 夹紧（1921×1081 → 1920×1080 再 min caps）。
    #[test]
    fn host_clamps_odd_resolution_to_even() {
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.caps = SessionParams {
            width: 1921,
            height: 1081,
            fps: 60,
            bitrate_bps: 8_000_000,
        };
        cfg.pipeline = PipelineConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 30,
            bitrate_bps: 1_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 256,
            diff: rc_pipeline::DiffMode::TrustPlatform,
            clock_origin_us: None,
            // 模板取默认预算；握手时由 expect_hello 按 N2 覆盖。
            max_packet_payload: rc_proto::MAX_PAYLOAD,
        };
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(cfg.caps, proto::AuthMethod::None));
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        let granted = host.handshake().expect("handshake");
        assert_eq!(
            (granted.width, granted.height),
            (1920, 1080),
            "to_even 向下取整"
        );
    }

    /// 启用 preserve_caps_aspect 后，协商分辨率在请求与 caps 包络内严格保持显示器宽高比，
    /// 避免 16:10 / 21:9 屏幕被按 16:9 采集导致黑边与光标坐标偏移。
    #[test]
    fn host_preserves_display_aspect_ratio_when_enabled() {
        let cases = [
            // (requested, display_caps, expected)
            ((1920, 1080), (1512, 982), (1512, 982)), // 14" MBP 原生 1:1
            ((1920, 1080), (1728, 1116), (1672, 1080)), // 16" MBP 高度受限等比缩宽
            ((1280, 720), (1512, 982), (1110, 720)),  // 客户请求 720p，保持 14" MBP 宽高比
            ((1920, 1080), (2560, 1080), (1920, 810)), // 21:9 带鱼屏宽度受限等比缩高
            ((1920, 1080), (2560, 1440), (1920, 1080)), // 5K 16:9 原样保持 1080p
        ];
        for ((rw, rh), (cw, ch), (ew, eh)) in cases {
            let (inj, _) = RecordingInjector::new();
            let mut cfg = host_cfg(Box::new(inj));
            cfg.caps = SessionParams {
                width: cw,
                height: ch,
                fps: 60,
                bitrate_bps: 40_000_000,
            };
            let mut mock = MockTransport::new();
            mock.deliver_reliable(&hello_msg(
                SessionParams {
                    width: rw,
                    height: rh,
                    fps: 60,
                    bitrate_bps: 14_000_000,
                },
                proto::AuthMethod::None,
            ));
            let mut host = HostSession::with_parts(
                cfg,
                Box::new(mock),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                zero_clock(),
            )
            .expect("with_parts");
            host.set_preserve_caps_aspect(true);
            let granted = host.handshake().expect("handshake");
            assert_eq!(
                (granted.width, granted.height),
                (ew, eh),
                "req={rw}x{rh}, caps={cw}x{ch}"
            );
            assert_eq!(granted.width % 2, 0);
            assert_eq!(granted.height % 2, 0);
        }
    }

    /// S2：version=2 → Err(Protocol)、不发 Welcome、连接关闭。
    #[test]
    fn host_rejects_hello_with_wrong_version() {
        let (inj, _) = RecordingInjector::new();
        let mut mock = MockTransport::new();
        let mut wire = Vec::new();
        hello_msg(caps(), proto::AuthMethod::None).encode(&mut wire);
        wire[1] = 2; // version 字段
        mock.deliver(NetEvent::Reliable(wire));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");

        let err = host.handshake().expect_err("version≠1 必须拒收");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert!(
            handle.reliable_sent().is_empty(),
            "不发 Welcome（S2）：{:?}",
            handle.reliable_sent()
        );
        assert_eq!(host.state(), SessionState::Closed, "连接关闭");
    }

    /// S2：auth_method=1 → 回 Welcome{auth_status: 拒绝} 后断开，不进 Streaming。
    #[test]
    fn host_rejects_hello_with_auth_method_nonzero() {
        let (inj, _) = RecordingInjector::new();
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");

        let err = host.handshake().expect_err("auth_method≠0 本轮必须拒绝");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        let msgs = handle.reliable_decoded();
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.auth_status, AuthStatus::Rejected, "必须回拒绝状态");
                assert_eq!(w.session_id, 4242);
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }
        assert_ne!(host.state(), SessionState::Streaming, "不进 Streaming");
    }

    /// S4/S5：投递 IdrRequest → request_idr() 生效，随后 tick 产出的下一
    /// 个数据报包头 is_idr()。
    #[test]
    fn host_applies_idr_request_and_emits_idr_datagram() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());

        // 首帧天然 IDR；等至少两帧数据报（第 2 帧起是 P 帧）。
        drive_until_datagram(&mut host, &handle, 400);
        drive_until_datagram(&mut host, &handle, 400);
        let before = handle.datagrams_sent().len();
        // 确认最近一帧确实是 P（否则测试前提不成立）。
        let last = handle.datagrams_sent()[before - 1].clone();
        let h = rc_pipeline::host::datagram_header(&last).expect("header");
        assert!(!h.is_idr(), "前提：IDR 之后的帧应为 P 帧");

        // 投递 IdrRequest → 下一帧必须编成 IDR。
        handle.deliver_reliable(&ControlMessage::IdrRequest {
            reason: proto::IdrReason::DecoderNeedKeyframe,
        });
        drive_until_datagram(&mut host, &handle, 400);
        let idx = handle.datagrams_sent().len();
        let dg = handle.datagrams_sent()[idx - 1].clone();
        let h = rc_pipeline::host::datagram_header(&dg).expect("header");
        assert!(
            h.is_idr(),
            "IdrRequest 后下一帧必须是 IDR（pending_idr 粘住）"
        );
    }

    /// S4：投递 ReliableUrgent（rc-input 编码的点击）→ 注入器收到且内容
    /// 一致；**同一事件若走 Reliable 则不得注入**。
    #[test]
    fn host_injects_urgent_input_events() {
        let click = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        };
        let mut wire = Vec::new();
        encode_events(&[click], &mut wire);

        // urgent 路径：注入成功。
        let (inj, events) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        handle.deliver(NetEvent::ReliableUrgent(wire.clone()));
        let r = host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(r.events_injected, 1);
        assert!(r.busy);
        assert_eq!(*events.lock().unwrap(), vec![click], "内容一致");

        // reliable 路径：同字节绝不当输入注入（会成为脏控制消息 → 停机）。
        let (inj2, events2) = RecordingInjector::new();
        let (mut host2, mut handle2) = handshaken(Box::new(inj2), caps());
        handle2.deliver(NetEvent::Reliable(wire));
        let res = host2.tick(Duration::from_millis(0));
        assert!(res.is_err(), "事件段不是合法控制消息，必须按脏消息处理");
        assert!(
            events2.lock().unwrap().is_empty(),
            "走 Reliable 的事件绝不被注入（S4 通道纪律）"
        );
    }

    /// S4 接缝钉死：IdrRequest 与 urgent 输入**同批**投递 → 两者都被处理
    /// （pump_receiver 路径会吞掉前者，此测试防回归）。
    #[test]
    fn host_processes_control_and_input_from_same_poll_batch() {
        let (inj, events) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        handle.deliver_reliable(&ControlMessage::IdrRequest {
            reason: proto::IdrReason::WatchdogStall,
        });
        let click = InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: true,
        };
        let mut wire = Vec::new();
        encode_events(&[click], &mut wire);
        handle.deliver(NetEvent::ReliableUrgent(wire));

        let r = host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(r.events_injected, 1, "urgent 输入必须被注入");
        assert_eq!(r.control_msgs_handled, 1, "同批控制消息必须被处理");
        assert_eq!(*events.lock().unwrap(), vec![click]);
        assert_eq!(host.state(), SessionState::Streaming, "不断连");
    }

    /// S6：close 中途有在途编码 → Bye 先发出；停机后无授权帧凭空消失
    /// （sched_stats().encoded == 编码器实际完成数），且每帧要么有结局、
    /// 要么已发出（Sent 打点）——采集线程先于编码线程停止的行为断言：
    /// 反序会让交接通道里的帧提交给已退出的编码线程，encoded 与完成数
    /// 失配、未发出也无结局的帧出现。
    #[test]
    fn host_close_sends_bye_then_drains_in_order() {
        let completed = Arc::new(AtomicU64::new(0));
        let counting = CountingEncoder {
            inner: fake_encoder(),
            completed: completed.clone(),
        };
        let (inj, _) = RecordingInjector::new();

        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            Box::new(counting),
            zero_clock(),
        )
        .expect("with_parts");
        host.handshake().expect("handshake");

        // 流若干拍，保证 close 时有已授权/在途编码。
        drive_until_datagram(&mut host, &handle, 400);
        for _ in 0..8 {
            host.tick(Duration::from_millis(0)).expect("tick");
            host.pipeline().wait_event(Duration::from_millis(4));
        }
        let datagrams_before = handle.datagrams_sent().len();
        assert!(datagrams_before > 0, "前提：已有数据报发出");

        let summary = host.close(ByeReason::Normal).expect("close");
        assert_eq!(host.state(), SessionState::Closed);

        // Bye 已发出（Welcome 之后的最后一条可靠消息）。
        let msgs = handle.reliable_decoded();
        assert!(
            matches!(
                msgs.last(),
                Some(ControlMessage::Bye {
                    reason: ByeReason::Normal
                })
            ),
            "close 必须发 Bye(Normal)：{msgs:?}"
        );

        // 排空继续发出（在途编码完成并上路）。
        assert!(handle.datagrams_sent().len() >= datagrams_before);

        // 无凭空消失：授权编码数 == 编码器实际完成数；每帧有结局或已发出。
        let encoded = host.pipeline().sched_stats().encoded;
        assert_eq!(
            encoded,
            completed.load(Ordering::Relaxed),
            "授权 {encoded} == 实际完成 {}",
            completed.load(Ordering::Relaxed)
        );
        for f in host.pipeline().trace().frames() {
            assert!(
                f.outcome().is_some() || f.at(Mark::Sent).is_some(),
                "帧 {} 无结局且未发出（授权帧凭空消失，S6）",
                f.seq().0
            );
        }
        assert_eq!(
            summary.unfinished_traces, 0,
            "停机后未结帧数（扣除已发出）必须为 0"
        );
        assert_eq!(summary.stop_reason, StopReason::Requested);
    }

    /// S8：Reliable 投递 proto 解码必败的字节 → Err(Protocol)、有序停机、
    /// unfinished==0。
    #[test]
    fn host_dirty_control_message_closes_session() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400); // 先流起来

        handle.deliver(NetEvent::Reliable(vec![0x99])); // 未知类型，decode 必败
        let err = host
            .tick(Duration::from_millis(0))
            .expect_err("脏消息必须报错");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(host.state(), SessionState::Closed, "有序停机");
        assert_eq!(
            host.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host.pipeline()),
            "未结帧必须全部是「已发出待对端判定」的帧"
        );
    }

    /// S8：投递 ProbePing/SetBitrate/预留号 → 状态不变、不断连、stderr 记录
    /// 一次；同种消息再来一条不再刷屏（限流由实现保证，这里断言行为面）。
    #[test]
    fn host_ignores_messages_reserved_for_later_cards() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        handle.deliver_reliable(&ControlMessage::ProbePing {
            probe_id: 1,
            sender_clock_us: 123,
        });
        handle.deliver_reliable(&ControlMessage::SetBitrate {
            bps: 4_000_000,
            reason: proto::BitrateReason::Probe,
        });
        // T20 已实装 0x0F：未启用剪贴板时收到合法 ClipboardNotify 仅计数不写本机、不拆会话（CB1）。
        handle.deliver_reliable(
            &ControlMessage::clipboard_notify("disabled-clipboard-test".into())
                .expect("valid clipboard notify"),
        );

        let r = host.tick(Duration::from_millis(0)).expect("忽略不报错");
        assert_eq!(host.state(), SessionState::Streaming, "状态不变");
        assert!(r.closed.is_none(), "不断连");
        assert_eq!(r.control_msgs_handled, 3, "三条都按处理/忽略路径完成");
        assert_eq!(
            host.clipboard_stats().received_while_disabled,
            1,
            "未启用剪贴板时应计入 received_while_disabled"
        );

        // 再来一轮同类消息：会话继续活着。
        handle.deliver_reliable(&ControlMessage::ProbePing {
            probe_id: 2,
            sender_clock_us: 456,
        });
        let r2 = host.tick(Duration::from_millis(0)).expect("忽略不报错");
        assert_eq!(host2_state(&host), SessionState::Streaming);
        assert!(r2.closed.is_none());
    }

    /// S6：流中投递 Bye(Normal) / Disconnected → tick 报告 closed，停机后
    /// 未结帧全部是已发出待判定帧。
    #[test]
    fn host_stops_on_bye_or_disconnect_mid_stream() {
        // 情形 A：Bye(Normal)。
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Normal,
        });
        let r = host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(r.closed, Some(StopReason::ByeReceived(ByeReason::Normal)));
        assert_eq!(host.state(), SessionState::Closed);
        assert_eq!(
            host.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host.pipeline())
        );

        // 情形 B：Disconnected（T17 语义：不再等价于会话结束——进 Detached
        // 保留管线，窗口 TTL 内等 ReconnectRequest；过期才按红线拆除）。
        let (inj2, _) = RecordingInjector::new();
        let (mut host2, mut handle2) = handshaken(Box::new(inj2), caps());
        drive_until_datagram(&mut host2, &handle2, 400);
        let gen_before = host2.pipeline_generation();
        handle2.deliver(NetEvent::Disconnected {
            reason: "peer dropped".into(),
        });
        let r2 = host2.tick(Duration::from_millis(0)).expect("tick");
        assert!(r2.closed.is_none(), "Disconnected 不再关闭会话（T17 R1）");
        assert_eq!(host2.state(), SessionState::Detached);
        assert_eq!(
            host2.pipeline_generation(),
            gen_before,
            "管线未拆，代数不变"
        );
        // 窗口内驱动拍：无新连接 → Waiting（管线仍保留）。
        assert_eq!(
            host2.tick_detached(0).expect("tick_detached"),
            DetachOutcome::Waiting
        );
        // 注入时钟推过默认 TTL（10s）→ 一拍 TornDown（红线拆除）。
        assert_eq!(
            host2
                .tick_detached(crate::reconnect::DEFAULT_RESUME_TTL.as_micros() as u64)
                .expect("tick_detached"),
            DetachOutcome::TornDown
        );
        // 拆除后账目闭合：未结帧全部是「已发出待对端判定」的帧。
        assert_eq!(
            host2.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host2.pipeline())
        );
        let summary2 = host2.close(ByeReason::Normal).expect("close");
        assert_eq!(host2.state(), SessionState::Closed);
        assert_eq!(summary2.stop_reason, StopReason::Requested);
    }

    /// 象限 A（坑 2）：无任何网络事件、假采集持续供帧 → run 若干拍数据报
    /// 持续发出、不饿死。
    ///
    /// 2026-11-21 增补断言（调度器帧去向贯通）：mock 会话（恒 0 会话钟）
    /// 下帧必须正常进调度器、**不被 Stale 全弃**——captured>0、有帧被
    /// 授权编码、超期弃帧吞不掉全部帧。这是 localhost 真后端「0 帧编码 +
    /// IdrRequest 全饿死」事故（SCK 零点错位 → 全帧即到即 Stale）的
    /// 反向保证。
    #[test]
    fn host_streaming_continues_when_network_is_silent() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, _handle) = handshaken(Box::new(inj), caps());
        let mut calls = 0u32;
        let summary = host
            .run(&mut || {
                calls += 1;
                calls >= 60
            })
            .expect("run");
        assert!(
            summary.datagrams_sent > 0,
            "网络静默时采集照常供帧、数据报持续发出"
        );
        assert_eq!(summary.stop_reason, StopReason::Requested);
        assert_eq!(summary.unfinished_traces, 0);
        assert!(summary.captured > 0, "假采集动态内容应持续供帧");
        assert!(
            summary.frames_encoded > 0,
            "帧应正常通过调度器被授权编码（不被全弃）"
        );
        assert!(
            summary.dropped_stale < summary.captured,
            "mock 会话不应全帧 Stale（captured={} dropped_stale={}）",
            summary.captured,
            summary.dropped_stale
        );
    }

    /// 象限 B（坑 7）：FakeContent::Static（零编码属正常）+ urgent 事件持续
    /// 到达 → 注入照常、循环不空转不卡死。
    #[test]
    fn host_handles_input_when_capture_is_static() {
        let (inj, events) = RecordingInjector::new();
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let mut handle = mock.clone();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(mock),
            fake_capture(FakeContent::Static),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        host.handshake().expect("handshake");

        for i in 0..8u16 {
            let ev = InputEvent::MouseMove {
                x: 100 + i,
                y: 200 + i,
            };
            let mut wire = Vec::new();
            encode_events(&[ev], &mut wire);
            handle.deliver(NetEvent::ReliableUrgent(wire));
            let r = host
                .tick(Duration::from_millis(0))
                .expect("tick 不因静屏卡死");
            assert_eq!(r.events_injected, 1, "静屏下注入照常");
        }
        assert_eq!(events.lock().unwrap().len(), 8);
        assert_eq!(host.state(), SessionState::Streaming);
        host.close(ByeReason::Normal).expect("close");
    }

    /// S9（铁律 4）：注入恒返 42 的会话钟 → 管线打点（除 Captured——它
    /// 来自采集帧自带时间戳）全为 42，证明未读系统钟。Captured 必须落在
    /// 假采集网格（帧间隔整数倍），也不是墙钟。经 testkit::pair 双端闭环。
    #[test]
    fn constant_session_clock_pins_all_trace_marks() {
        use crate::client_session::{ClientSession, ClientSessionConfig};
        use crate::testkit::pair;
        use rc_decode::{FakeDecoder, FakeDecoderConfig};
        use rc_render::{FakeRenderer, FakeRendererConfig};

        /// 脚本型输入捕获：一次性吐一条点击。
        struct ScriptedCapture {
            scripted: Vec<InputEvent>,
        }
        impl rc_input::InputCapture for ScriptedCapture {
            fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
                Ok(std::mem::take(&mut self.scripted))
            }
        }

        let (ta, tb) = pair();
        let mut ta_handle = ta.clone();
        let clock: SessionClock = Arc::new(|| 42);

        let (inj, injected) = RecordingInjector::new();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(ta),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            clock.clone(),
        )
        .expect("host");
        let mut client = ClientSession::with_parts(
            ClientSessionConfig {
                peer_addr: "127.0.0.1:9901".parse().unwrap(),
                mtu: DEFAULT_MTU,
                requested: caps(),
                session_id: 7,
                pipeline: pcfg(),
                connect_timeout: Duration::from_secs(1),
                handshake_timeout: Duration::from_millis(500),
                capture: Box::new(ScriptedCapture {
                    scripted: vec![InputEvent::MouseButton {
                        button: MouseButton::Left,
                        pressed: true,
                    }],
                }),
                input_window: rc_input::DEFAULT_WINDOW,
                auth: crate::auth::ClientAuthConfig::default(),
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
            clock.clone(),
        )
        .expect("client");

        // 握手（确定性顺序，不开线程竞速——mock poll 忽略超时，分片轮转
        // 会即刻耗尽）：① 预置 Hello 给 host；② host 真握手，把真 Welcome
        // 发进链路；③ client 握手消费真 Welcome（client 自己发的 Hello
        // 由 host 在 Streaming 后按「重复到达」忽略，无害）。
        let mut hello7 = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut hello7 {
            h.session_id = 7; // 与 client 配置一致，Welcome 回显才能对上
        }
        ta_handle.deliver_reliable(&hello7);
        host.handshake().expect("host handshake");
        client.handshake().expect("client handshake");

        // 驱动若干拍：client 先收上一轮 host 发出的数据报，再让 host 产新帧。
        let t0 = Instant::now();
        for i in 0..400u32 {
            let now = t0 + Duration::from_millis(u64::from(i));
            client
                .tick(Duration::from_millis(0), now)
                .expect("client tick");
            host.tick(Duration::from_millis(0)).expect("host tick");
            if client.pipeline().displayed() > 0 && !injected.lock().unwrap().is_empty() {
                break;
            }
            host.pipeline().wait_event(Duration::from_millis(2));
        }
        assert!(client.pipeline().displayed() > 0, "应有帧上屏");
        assert!(
            !injected.lock().unwrap().is_empty(),
            "host 应注入 client 的输入"
        );

        // 关闭：host 先 close（Bye + 排空），client 的 tick 收尾。
        host.close(ByeReason::Normal).expect("host close");
        let r = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("client tick");
        assert_eq!(r.closed, Some(StopReason::ByeReceived(ByeReason::Normal)));
        if client.state() != SessionState::Closed {
            client.close(ByeReason::Normal).expect("client close");
        }

        // 打点断言：非 Captured 全为 42；Captured 落在假采集网格上。
        for trace in [host.pipeline().trace(), client.pipeline().trace()] {
            for f in trace.frames() {
                for m in rc_trace::Mark::ALL {
                    if let Some(ts) = f.at(m) {
                        if m == Mark::Captured {
                            assert_eq!(
                                ts.0 % FRAME_INTERVAL_US,
                                0,
                                "Captured 应来自假采集网格（{}µs 的倍数），不是墙钟",
                                FRAME_INTERVAL_US
                            );
                        } else {
                            assert_eq!(ts.0, 42, "{m:?} 打点必须读会话钟（恒 42）");
                        }
                    }
                }
            }
        }
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);
        assert_eq!(
            host.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host.pipeline())
        );
    }

    /// 「已发出但无结局」的帧数（与 stop_with 第 ⑤ 步同一判据）。
    fn count_sent_unresolved(p: &HostPipeline) -> u64 {
        p.trace()
            .frames()
            .iter()
            .filter(|f| f.outcome().is_none() && f.at(Mark::Sent).is_some())
            .count() as u64
    }

    // ─────────────────────────────────────────────────────────────
    // T17：断线重连（Detached / 保持窗口 / 裁定 / 饱和守卫 / PathChanged）
    // ─────────────────────────────────────────────────────────────

    /// 脚本化管线工厂：每次建一个全假后端的新 HostPipeline（Replaced /
    /// 刷新路径的确定性重建；时钟由 expect_hello 统一 set_clock）。
    fn scripted_pipeline_factory() -> Box<dyn FnMut() -> Result<HostPipeline> + Send> {
        Box::new(|| {
            HostPipeline::with_backends_paced(
                pcfg(),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                true, // 假后端必须节流（坑 4）
            )
        })
    }

    /// 一个已发出的媒体数据报的最大帧序号（无数据报时 None）。
    fn max_sent_seq(handle: &MockTransport) -> Option<u64> {
        handle
            .datagrams_sent()
            .iter()
            .filter_map(|d| rc_pipeline::host::datagram_header(d).ok())
            .map(|h| h.frame_seq.0)
            .max()
    }

    /// R1：Disconnected → Detached、管线保留（generation 不变）；窗口内
    /// 驱动拍恒 Waiting；注入时钟推过默认 TTL 后一拍 TornDown。
    #[test]
    fn host_detached_keeps_pipeline_until_ttl() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        let gen = host.pipeline_generation();

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        let r = host.tick(Duration::from_millis(0)).expect("tick");
        assert!(r.closed.is_none(), "断线不再关闭会话（R1）");
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(host.pipeline_generation(), gen, "管线保留，代数不变");
        assert_eq!(host.reconnect_stats().disconnects, 1);

        // 窗口内（默认 TTL 10s）：无新连接 → Waiting，管线仍在。
        for now_us in [0u64, 1_000, 5_000_000, 9_999_999] {
            assert_eq!(
                host.tick_detached(now_us).expect("tick_detached"),
                DetachOutcome::Waiting,
                "now={now_us} 应仍在窗口内等待"
            );
            assert_eq!(host.pipeline_generation(), gen);
        }
        // 到 TTL：一拍 TornDown；之后再拍不再重复拆除（幂等 Waiting）。
        assert_eq!(
            host.tick_detached(10_000_000).expect("tick_detached"),
            DetachOutcome::TornDown
        );
        assert_eq!(
            host.tick_detached(10_000_000).expect("tick_detached"),
            DetachOutcome::Waiting
        );
        // 拆除后账目闭合（R7）。
        assert_eq!(
            host.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host.pipeline())
        );
    }

    /// R2/R4：窗口内 ReconnectRequest 命中 → Welcome bit0=1、管线复用
    ///（generation 不变）、恢复后首个产出的媒体数据报是 IDR 且 frame_seq
    /// 延续（不归零）。
    #[test]
    fn host_resume_within_window_reuses_pipeline_with_idr() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        drive_until_datagram(&mut host, &handle, 400);
        let last_seq = max_sent_seq(&handle).expect("断开前应有数据报");
        let gen = host.pipeline_generation();

        // 断线 → Detached；新连接带 ReconnectRequest{sid=4242, resume=1}。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let mut incoming = MockTransport::new();
        incoming.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let incoming_handle = incoming.clone();
        host.offer_transport(Box::new(incoming)).expect("offer");

        let outcome = host.tick_detached(1_000).expect("tick_detached");
        assert_eq!(outcome, DetachOutcome::Resumed);
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen, "复用管线，代数不变");
        assert_eq!(host.reconnect_stats().resumes, 1);

        // 裁定回执：Welcome{bit0=1, 回显 sid}。
        let msgs = incoming_handle.reliable_decoded();
        assert_eq!(msgs.len(), 1, "只发一条 Welcome：{msgs:?}");
        match &msgs[0] {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.session_id, 4242);
                assert!(w.host_flags.resume_accepted(), "bit0=1：{w:?}");
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }

        // 恢复后首个产出的数据报：IDR 且 frame_seq 延续。
        drive_until_datagram(&mut host, &incoming_handle, 400);
        let dg = incoming_handle.datagrams_sent();
        let h = rc_pipeline::host::datagram_header(&dg[dg.len() - 1]).expect("header");
        assert!(h.is_idr(), "恢复后首个数据报必须是 IDR（request_idr 粘住）");
        assert!(
            h.frame_seq.0 > last_seq,
            "frame_seq 延续：恢复首帧 {} 应大于断线前 {}",
            h.frame_seq.0,
            last_seq
        );
        host.close(ByeReason::Normal).expect("close");
    }

    /// 复核遗留测试用（发现 1/2 同类）：投递一条预置消息后所有发送一律
    /// 失败的死候选连接（模拟对端发完 ReconnectRequest 即死——QUIC 层
    /// 感知连接死亡，send_reliable Err）。
    struct DeadCandidate {
        events: Vec<NetEvent>,
    }

    impl Transport for DeadCandidate {
        fn send_datagram(&mut self, _data: &[u8]) -> Result<()> {
            Err(Error::Network("dead candidate".into()))
        }
        fn send_reliable(&mut self, _data: &[u8]) -> Result<()> {
            Err(Error::Network("dead candidate".into()))
        }
        fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
            Ok(std::mem::take(&mut self.events))
        }
        fn stats(&self) -> rc_net::NetStats {
            rc_net::NetStats::default()
        }
    }

    /// 复核遗留（发现 1/2 同类未清尽）：Detached 期候选已死——
    /// ReconnectRequest 的 Welcome send_reliable 失败不得上抛杀 host
    /// （修复前经 run 的 `?` 整个 host 停机）。吞候选纪律：计数、丢连接、
    /// 保持 Detached；且 handle_reconnect 先发后迁移零副作用，旧管线
    /// 原封保留——同窗口内后续合法候选仍可命中 resume。
    #[test]
    fn host_survives_dead_candidate_reconnect_and_still_resumes() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        let gen = host.pipeline_generation();

        // 断线 → Detached；死候选带 ReconnectRequest{sid=4242, resume=1}。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let mut req = Vec::new();
        ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        }
        .encode(&mut req);
        host.offer_transport(Box::new(DeadCandidate {
            events: vec![NetEvent::Reliable(req)],
        }))
        .expect("offer");

        // 修复前：Err 上抛（run 的 `?` 杀 host）；修复后：Ok(Waiting)。
        assert_eq!(
            host.tick_detached(1_000)
                .expect("死候选必须吞掉不上抛（坏候选 ≠ 坏会话）"),
            DetachOutcome::Waiting
        );
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(host.dirty_candidates(), 1, "死候选计入丢弃");
        assert_eq!(host.pipeline_generation(), gen, "零副作用：管线未被拆");

        // 同窗口内合法候选仍可命中 resume（管线保留的收益断言）。
        let mut incoming = MockTransport::new();
        incoming.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let incoming_handle = incoming.clone();
        host.offer_transport(Box::new(incoming)).expect("offer");
        assert_eq!(
            host.tick_detached(2_000).expect("tick_detached"),
            DetachOutcome::Resumed
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert!(
            incoming_handle
                .reliable_decoded()
                .iter()
                .any(|m| matches!(m, ControlMessage::Welcome(w) if w.host_flags.resume_accepted())),
            "后续合法候选照常拿到 Welcome{{bit0=1}}"
        );
        host.close(ByeReason::Normal).expect("close");
    }

    /// R2：过期后 resume=1 → bit0=0（连接保持）；同连接补发全新 Hello →
    /// 新会话建立（Replaced）、generation+1、FrameSeq 归零。
    #[test]
    fn host_rejects_resume_after_expiry_via_fresh_hello() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        host.set_pipeline_factory(scripted_pipeline_factory());
        drive_until_datagram(&mut host, &handle, 400);
        drive_until_datagram(&mut host, &handle, 400);
        let last_seq = max_sent_seq(&handle).expect("断开前应有数据报");
        let gen = host.pipeline_generation();

        // 断线 → 推过 TTL → 拆除。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(
            host.tick_detached(10_000_000).expect("tick_detached"),
            DetachOutcome::TornDown
        );

        // 过期后的 resume=1：未命中 → bit0=0，连接保持。
        let mut incoming = MockTransport::new();
        incoming.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let mut incoming_handle = incoming.clone();
        host.offer_transport(Box::new(incoming)).expect("offer");
        assert_eq!(
            host.tick_detached(10_000_000).expect("tick_detached"),
            DetachOutcome::Waiting,
            "未命中不是终局：连接保持等全新 Hello"
        );
        assert_eq!(host.state(), SessionState::Detached);
        match &incoming_handle.reliable_decoded()[0] {
            ControlMessage::Welcome(w) => {
                assert!(!w.host_flags.resume_accepted(), "过期后 bit0=0：{w:?}");
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }

        // 同连接补发全新 Hello（新 sid）→ Replaced：generation+1、FrameSeq 归零。
        let mut fresh = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh {
            h.session_id = 5000;
        }
        incoming_handle.deliver_reliable(&fresh);
        assert_eq!(
            host.tick_detached(10_000_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.session_id(), Some(5000));
        assert_eq!(host.pipeline_generation(), gen + 1);
        assert_eq!(host.reconnect_stats().rebuilds, 1);

        // 新会话 FrameSeq 从头：首个数据报序号远小于旧纪元。
        drive_until_datagram(&mut host, &incoming_handle, 400);
        let dg = incoming_handle.datagrams_sent();
        let h = rc_pipeline::host::datagram_header(&dg[dg.len() - 1]).expect("header");
        assert!(
            h.frame_seq.0 < last_seq,
            "新会话 FrameSeq 归零：{} 应小于旧纪元 {last_seq}",
            h.frame_seq.0
        );
        host.close(ByeReason::Normal).expect("close");
    }

    /// R6/R7：注入时钟到守卫线 → reliable 流出现 Bye{reason=Refresh}、
    /// 管线按红线拆除；随后全新 Hello → Welcome{bit0=0, bit1=1}。
    #[test]
    fn host_refresh_sends_bye_and_sets_epoch_bit() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, handle) = handshaken(Box::new(inj), caps());
        host.set_pipeline_factory(scripted_pipeline_factory());
        drive_until_datagram(&mut host, &handle, 400);

        // 缩短守卫线（1s）：到线前不触发，到线即触发。
        let cfg = ReconnectConfig {
            refresh_guard_us: 1_000_000,
            ..ReconnectConfig::default()
        };
        host.set_reconnect(cfg).expect("set_reconnect");
        assert!(!host.check_refresh(999_999).expect("check"));
        assert!(host.check_refresh(1_000_000).expect("check"));

        // Bye{Refresh} 已上路；管线已拆、回 accept（Detached）。
        let msgs = handle.reliable_decoded();
        assert!(
            matches!(
                msgs.last(),
                Some(ControlMessage::Bye {
                    reason: ByeReason::Refresh
                })
            ),
            "守卫触发必须发 Bye(Refresh)：{msgs:?}"
        );
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(host.reconnect_stats().refreshes, 1);
        assert_eq!(
            host.pipeline().trace().summary().unfinished,
            count_sent_unresolved(host.pipeline()),
            "刷新拆除同样走红线，账目闭合"
        );

        // 全新 Hello → 新会话，Welcome 带 bit1=1（时钟纪元已重置）。
        let mut incoming = MockTransport::new();
        let mut fresh = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh {
            h.session_id = 6000;
        }
        incoming.deliver_reliable(&fresh);
        let incoming_handle = incoming.clone();
        host.offer_transport(Box::new(incoming)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        let msgs = incoming_handle.reliable_decoded();
        match &msgs[0] {
            ControlMessage::Welcome(w) => {
                assert!(!w.host_flags.resume_accepted(), "bit0=0");
                assert!(w.host_flags.clock_epoch_reset(), "bit1=1：{w:?}");
                assert_eq!(w.session_id, 6000);
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }
        assert_eq!(host.reconnect_stats().rebuilds, 1);
        assert_eq!(host.pipeline_generation(), 2);
        host.close(ByeReason::Normal).expect("close");
    }

    /// R5：PathChanged 不算断线——状态仍 Streaming、path_changes 计数、
    /// 数据报继续产出不断流。
    #[test]
    fn host_path_changed_keeps_streaming() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        let before = handle.datagrams_sent().len();

        handle.deliver(NetEvent::PathChanged);
        handle.deliver(NetEvent::PathChanged);
        let r = host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Streaming, "不迁移状态");
        assert!(r.closed.is_none(), "不断流不关闭");
        assert_eq!(host.reconnect_stats().path_changes, 2);

        // 迁移后媒体继续产出。
        drive_until_datagram(&mut host, &handle, 400);
        assert!(handle.datagrams_sent().len() > before, "数据报继续产出");
    }

    // ─────────────────────────────────────────────────────────────
    // 评审修复回归锁（修复卡 A：发现 1–10 各一项）
    // ─────────────────────────────────────────────────────────────

    /// 发现 1：Detached 期一条脏控制消息（decode 失败）不得杀 host——
    /// 丢该候选连接、计数、保持 Detached；随后合法重连仍被接受。
    /// 旧行为：`ControlMessage::decode(&msg)?` 上抛，经 run 的 `?` 杀掉
    /// 整个 host 进程。
    #[test]
    fn detached_dirty_message_drops_candidate_not_session() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);

        // 脏候选：新连接首条 Reliable 是不可解码字节。
        let mut bad = MockTransport::new();
        bad.deliver(NetEvent::Reliable(vec![0x99]));
        let bad_handle = bad.clone();
        host.offer_transport(Box::new(bad)).expect("offer");
        let out = host
            .tick_detached(1_000)
            .expect("脏消息必须消化，不得上抛（发现 1）");
        assert_eq!(out, DetachOutcome::Waiting);
        assert_eq!(host.state(), SessionState::Detached, "坏候选≠坏会话");
        assert_eq!(host.dirty_candidates(), 1);
        assert!(bad_handle.reliable_sent().is_empty(), "不回任何消息");

        // 脏消息之后，合法重连仍被接受。
        let mut good = MockTransport::new();
        good.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        host.offer_transport(Box::new(good)).expect("offer");
        assert_eq!(
            host.tick_detached(2_000).expect("tick_detached"),
            DetachOutcome::Resumed,
            "脏消息后 host 仍能接受合法重连（发现 1）"
        );
        assert_eq!(host.state(), SessionState::Streaming);
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 2：Detached 期 accept 的非 Timeout 错误（坏 TLS 握手包）只
    /// 计数不杀 host。真 QUIC 注入：client 信任 dev 证书但 server_name
    /// SAN 失配 → client 侧 TLS 拒绝 → host 侧 accept 报 Network 错。
    /// 连续两次注入，host 必须计数后继续 Waiting 存活。
    #[test]
    fn detached_accept_errors_are_counted_not_fatal() {
        use rc_net::quic::{Identity, QuicEndpoint};
        use rc_net::{NetConfig, Role};

        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.listen_addr = "127.0.0.1:0".parse().unwrap();
        let mut host = HostSession::new(
            cfg,
            crate::dev_cert::DEV_CERT_PEM,
            crate::dev_cert::DEV_KEY_PEM,
            zero_clock(),
        )
        .expect("host endpoint");
        // 直达 Detached：显式驱动守卫到线（文档允许的显式驱动入口；
        // Init 态无连接/管线，此处只做状态迁移以进入 accept 路径）。
        assert!(host
            .check_refresh(crate::reconnect::REFRESH_GUARD_US)
            .expect("check"));
        assert_eq!(host.state(), SessionState::Detached);
        let host_addr = host.local_addr().expect("host addr");

        let ident = Identity::from_pem(crate::dev_cert::DEV_CERT_PEM, crate::dev_cert::DEV_KEY_PEM)
            .expect("identity");
        let certs = Identity::certs_from_pem(crate::dev_cert::DEV_CERT_PEM).expect("certs");
        // 每轮 spawn 一个坏握手 client 再驱动一拍（accept ≤2s，坏握手在
        // 回环上毫秒级失败）；至多 6 轮容错极慢的握手失败传播。
        let mut rounds = 0u32;
        while host.accept_errors() < 2 && rounds < 6 {
            let net = NetConfig {
                role: Role::Controller,
                local_addr: "127.0.0.1:0".parse().unwrap(),
                peer_addr: Some(host_addr),
                mtu: DEFAULT_MTU,
            };
            let ep = QuicEndpoint::new(&net, &ident).expect("client endpoint");
            let certs = certs.clone();
            std::thread::spawn(move || {
                // SAN 失配 → client 拒绝服务端证书 → host 侧 accept Err。
                let _ = ep.connect(
                    host_addr,
                    &certs,
                    "san-mismatch.invalid",
                    Duration::from_secs(3),
                );
            });
            let out = host
                .tick_detached(0)
                .expect("accept 错误必须消化，不得上抛（发现 2）");
            assert_eq!(out, DetachOutcome::Waiting, "坏握手后继续 Waiting");
            rounds += 1;
        }
        assert_eq!(
            host.accept_errors(),
            2,
            "连续两次坏握手各计一次（rounds={rounds}）"
        );
        assert_eq!(host.state(), SessionState::Detached, "host 仍存活");
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 3：Detached 期垃圾 Hello 先检后拆——违规候选不得先拆旧管线，
    /// 同窗口内后续合法 ReconnectRequest 仍能 resume 命中。旧行为：先拆
    /// 后校验，垃圾 Hello 把保留中的管线拆掉且 Err 上抛，resume 必 miss。
    #[test]
    fn detached_garbage_hello_keeps_pipeline_for_resume() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400);
        drive_until_datagram(&mut host, &handle, 400);
        let gen = host.pipeline_generation();

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);

        // 候选 1：auth_method≠0 的违规 Hello → 只丢候选，旧管线不动。
        let mut bad = MockTransport::new();
        bad.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        let bad_handle = bad.clone();
        host.offer_transport(Box::new(bad)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000)
                .expect("违规候选必须消化（发现 1/3）"),
            DetachOutcome::Waiting
        );
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(
            host.pipeline_generation(),
            gen,
            "违规候选不得先拆旧管线（发现 3：先检后拆）"
        );
        assert_eq!(host.dirty_candidates(), 1);
        // T19 行 2（无密码 host 收到 AuthMethod::Password）：按 A5 回
        // Welcome{Rejected} 后立即断开候选连接，但绝不拆旧管线。
        let bad_msgs = bad_handle.reliable_sent();
        assert_eq!(bad_msgs.len(), 1, "行 2 拒绝应回一条 Welcome{{Rejected}}");
        match ControlMessage::decode(&bad_msgs[0]).expect("decode Welcome") {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.auth_status, AuthStatus::Rejected);
            }
            other => panic!("应回 Welcome{{Rejected}}，实际 {other:?}"),
        }

        // 候选 2：合法 ReconnectRequest → 仍命中 resume。
        let mut good = MockTransport::new();
        good.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        host.offer_transport(Box::new(good)).expect("offer");
        assert_eq!(
            host.tick_detached(2_000).expect("tick_detached"),
            DetachOutcome::Resumed,
            "垃圾 Hello 后合法 ReconnectRequest 仍能 resume（发现 3）"
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen);
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 4：双纪元连刷（小阈值注入钟）——第一纪元到线触发刷新后，
    /// 驱动器把注入钟归零、全新 Hello 重建会话（落成即重锚基准），第二
    /// 纪元时钟再到同一阈值线时必须**再次**触发。旧行为：绝对加法
    /// `now < base + threshold` 在第二纪元（base+threshold=2×阈值处）
    /// 永不满足，第二次刷新数学上不可能。
    #[test]
    fn refresh_guard_fires_in_each_epoch() {
        let (now, clock) = mutable_clock();
        let (inj, _) = RecordingInjector::new();
        let (mut host, handle) = handshaken_at(Box::new(inj), clock);
        host.set_pipeline_factory(scripted_pipeline_factory());
        drive_until_datagram(&mut host, &handle, 400);

        // 纪元 1：阈值 1s，到线触发刷新 #1。
        host.set_reconnect(ReconnectConfig {
            refresh_guard_us: 1_000_000,
            ..ReconnectConfig::default()
        })
        .expect("set_reconnect");
        *now.lock().unwrap() = 1_000_000;
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.reconnect_stats().refreshes, 1, "纪元 1 到线触发");
        assert_eq!(host.state(), SessionState::Detached);

        // 驱动器归零（第二纪元从小值起算）+ 全新 Hello 重建会话。
        *now.lock().unwrap() = 0;
        let mut incoming = MockTransport::new();
        let mut fresh = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh {
            h.session_id = 7000;
        }
        incoming.deliver_reliable(&fresh);
        let mut incoming_handle = incoming.clone();
        host.offer_transport(Box::new(incoming)).expect("offer");
        assert_eq!(
            host.tick_detached(0).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.state(), SessionState::Streaming);
        // 对端后续证据（首条控制消息）消费 bit1 待决态——纪元 2 的守卫
        // 计时不再被「刷新在途」抑制。
        incoming_handle.deliver_reliable(&ControlMessage::IdrRequest {
            reason: proto::IdrReason::DecoderNeedKeyframe,
        });
        host.tick(Duration::from_millis(0)).expect("tick");

        // 纪元 2：同一阈值线必须再次触发刷新 #2（旧行为：永不触发）。
        *now.lock().unwrap() = 1_000_000;
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(
            host.reconnect_stats().refreshes,
            2,
            "第二纪元照常在阈值线触发（发现 4：wrapping 纪元差）"
        );
        assert_eq!(host.state(), SessionState::Detached);
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 5：两轮断线两轮 resume 都命中——resume 命中后保持窗口必须
    /// 重开，第二轮断线按新时刻起算 TTL。旧行为：窗口沿用首轮锚点
    ///（detach 幂等不续期），第二轮在旧窗口过期后必被错杀成全量重建。
    #[test]
    fn resume_window_reopens_between_disconnect_rounds() {
        let (now, clock) = mutable_clock();
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken_at(Box::new(inj), clock);
        drive_until_datagram(&mut host, &handle, 400);

        // 第一轮断线（t=1s）→ 窗口内 resume 命中（t=1.5s）。
        *now.lock().unwrap() = 1_000_000;
        handle.deliver(NetEvent::Disconnected {
            reason: "drop#1".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);
        *now.lock().unwrap() = 1_500_000;
        let mut second = MockTransport::new();
        second.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let mut second_handle = second.clone();
        host.offer_transport(Box::new(second)).expect("offer");
        assert_eq!(
            host.tick_detached(1_500_000).expect("tick_detached"),
            DetachOutcome::Resumed
        );
        assert_eq!(host.state(), SessionState::Streaming);

        // 第二轮断线（t=2s）：修复后窗口从 2s 重新起算。
        *now.lock().unwrap() = 2_000_000;
        second_handle.deliver(NetEvent::Disconnected {
            reason: "drop#2".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);

        // t=11.5s：旧锚点(1s)+TTL(10s) 已过、新锚点(2s)+TTL 未到——
        // 修复后仍 resume 命中（旧行为：窗口过期 → miss → 全量重建）。
        *now.lock().unwrap() = 11_500_000;
        let mut third = MockTransport::new();
        third.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        host.offer_transport(Box::new(third)).expect("offer");
        assert_eq!(
            host.tick_detached(11_500_000).expect("tick_detached"),
            DetachOutcome::Resumed,
            "第二轮断线也应 resume 命中（发现 5：命中后重开窗口）"
        );
        assert_eq!(host.reconnect_stats().resumes, 2);
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 6：饱和刷新后的 Welcome 可能随连接丢失——对端重试时第二个
    /// Welcome 必须仍带 bit1（旧行为：发出 Welcome 即消费，重试的
    /// Welcome 缺 bit1 被 client `refresh_pending && !bit1` 强拒死锁）；
    /// 对端后续证据（该连接首条控制/输入消息）到达后才消费。
    #[test]
    fn epoch_reset_bit_survives_welcome_loss_retry() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, handle) = handshaken(Box::new(inj), caps());
        host.set_pipeline_factory(scripted_pipeline_factory());
        drive_until_datagram(&mut host, &handle, 400);

        // 直接触发守卫到线 → Detached、bit1 待决。
        assert!(host
            .check_refresh(crate::reconnect::REFRESH_GUARD_US)
            .expect("check"));
        assert_eq!(host.state(), SessionState::Detached);

        // 全新 Hello #1 → Welcome 带 bit1（此刻不得消费）。
        let mut t1 = MockTransport::new();
        let mut fresh1 = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh1 {
            h.session_id = 6000;
        }
        t1.deliver_reliable(&fresh1);
        let mut t1_handle = t1.clone();
        host.offer_transport(Box::new(t1)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        match &t1_handle.reliable_decoded()[0] {
            ControlMessage::Welcome(w) => {
                assert!(w.host_flags.clock_epoch_reset(), "首个 Welcome 带 bit1")
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }

        // 模拟 Welcome 丢失：连接死亡（对端从未收到 Welcome）→ 回 Detached。
        t1_handle.deliver(NetEvent::Disconnected {
            reason: "welcome lost".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);

        // 对端重试全新 Hello #2：第二个 Welcome 必须仍带 bit1（发现 6）。
        let mut t2 = MockTransport::new();
        let mut fresh2 = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh2 {
            h.session_id = 6000;
        }
        t2.deliver_reliable(&fresh2);
        let mut t2_handle = t2.clone();
        host.offer_transport(Box::new(t2)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        match &t2_handle.reliable_decoded()[0] {
            ControlMessage::Welcome(w) => {
                assert!(
                    w.host_flags.clock_epoch_reset(),
                    "重试的第二个 Welcome 仍须带 bit1：{w:?}"
                );
                assert!(!w.host_flags.resume_accepted());
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }

        // 对端后续证据（该连接首条控制消息）→ 消费；此后的全新 Welcome
        // 不再带 bit1（消费点语义）。
        t2_handle.deliver_reliable(&ControlMessage::IdrRequest {
            reason: proto::IdrReason::DecoderNeedKeyframe,
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        t2_handle.deliver(NetEvent::Disconnected {
            reason: "done".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);
        let mut t3 = MockTransport::new();
        let mut fresh3 = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh3 {
            h.session_id = 6100;
        }
        t3.deliver_reliable(&fresh3);
        let t3_handle = t3.clone();
        host.offer_transport(Box::new(t3)).expect("offer");
        assert_eq!(
            host.tick_detached(2_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        match &t3_handle.reliable_decoded()[0] {
            ControlMessage::Welcome(w) => {
                assert!(!w.host_flags.clock_epoch_reset(), "证据到达后已消费")
            }
            other => panic!("应回 Welcome，实际 {other:?}"),
        }
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 7：全新会话（非 resume）不复用上一会话的 SetBitrate 记账——
    /// 落成段复位 None，回落本会话协商值。
    #[test]
    fn fresh_session_resets_bitrate_accounting() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        // 上一会话调过 SetBitrate(400kbps)。
        handle.deliver_reliable(&ControlMessage::SetBitrate {
            bps: 400_000,
            reason: proto::BitrateReason::Probe,
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.current_bitrate_bps(), 400_000);

        // 断线 → 全新 Hello（新 sid）建新会话。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let mut incoming = MockTransport::new();
        let mut fresh = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh {
            h.session_id = 8000;
        }
        incoming.deliver_reliable(&fresh);
        host.offer_transport(Box::new(incoming)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        assert_eq!(
            host.current_bitrate_bps(),
            caps().bitrate_bps,
            "全新会话不复用上一会话的 SetBitrate 记账（发现 7）"
        );
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 8：key_batches_total 只在全新会话（非 resume）复位——resume
    /// 命中计数延续，两侧 InputAck `key_batch_seq` 口径对齐。
    #[test]
    fn key_batch_counter_reset_on_fresh_session_only() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        let click_wire = |btn| {
            let mut wire = Vec::new();
            encode_events(
                &[InputEvent::MouseButton {
                    button: btn,
                    pressed: true,
                }],
                &mut wire,
            );
            wire
        };

        // 会话 1：一批关键事件（点击）→ key_batches=1。
        handle.deliver(NetEvent::ReliableUrgent(click_wire(MouseButton::Left)));
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.key_batches(), 1);

        // resume 路径：断线 → 重连命中 → 计数延续。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let mut c2 = MockTransport::new();
        c2.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let mut c2_handle = c2.clone();
        host.offer_transport(Box::new(c2)).expect("offer");
        assert_eq!(
            host.tick_detached(1_000).expect("tick_detached"),
            DetachOutcome::Resumed
        );
        c2_handle.deliver(NetEvent::ReliableUrgent(click_wire(MouseButton::Right)));
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.key_batches(), 2, "resume 命中后计数延续（发现 8）");
        let acks: Vec<u64> = c2_handle
            .reliable_decoded()
            .iter()
            .filter_map(|m| match m {
                ControlMessage::InputAck { key_batch_seq, .. } => Some(*key_batch_seq),
                _ => None,
            })
            .collect();
        assert_eq!(acks, vec![2], "InputAck key_batch_seq 延续：{acks:?}");

        // 全新会话：断线 → 全新 Hello（新 sid）→ 计数归零并从 1 起算。
        c2_handle.deliver(NetEvent::Disconnected {
            reason: "drop#2".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let mut c3 = MockTransport::new();
        let mut fresh = hello_msg(caps(), proto::AuthMethod::None);
        if let ControlMessage::Hello(h) = &mut fresh {
            h.session_id = 9000;
        }
        c3.deliver_reliable(&fresh);
        let mut c3_handle = c3.clone();
        host.offer_transport(Box::new(c3)).expect("offer");
        assert_eq!(
            host.tick_detached(2_000).expect("tick_detached"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.key_batches(), 0, "全新会话关键批计数归零（发现 8）");
        c3_handle.deliver(NetEvent::ReliableUrgent(click_wire(MouseButton::Right)));
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.key_batches(), 1);
        let acks3: Vec<u64> = c3_handle
            .reliable_decoded()
            .iter()
            .filter_map(|m| match m {
                ControlMessage::InputAck { key_batch_seq, .. } => Some(*key_batch_seq),
                _ => None,
            })
            .collect();
        assert_eq!(acks3, vec![1], "新会话 InputAck 从 1 重新起算：{acks3:?}");
        host.close(ByeReason::Normal).expect("close");
    }

    /// 发现 9：脏消息停机的早退路径不得吞掉本拍已注入的事件计数——
    /// 同拍「先注入 1 条输入、后到脏控制消息」→ summary.events_injected
    /// 必须计 1（旧行为：早退在累计之前 return，计数凭空消失）。
    #[test]
    fn events_counted_even_when_dirty_message_stops_tick() {
        let (inj, events) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        let click = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        };
        let mut wire = Vec::new();
        encode_events(&[click], &mut wire);
        handle.deliver(NetEvent::ReliableUrgent(wire));
        handle.deliver(NetEvent::Reliable(vec![0x99])); // 同批脏控制消息
        let err = host
            .tick(Duration::from_millis(0))
            .expect_err("脏消息仍按 S8 停机上抛");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(
            *events.lock().unwrap(),
            vec![click],
            "同批在脏消息之前的输入已被注入"
        );
        let summary = host.close(ByeReason::Normal).expect("close");
        assert_eq!(
            summary.events_injected, 1,
            "早退路径不得丢当拍已注入计数（发现 9）"
        );
    }

    /// 发现 10：BwProbe 探测块不再混入媒体数据报计数——
    /// summary.datagrams_sent 只计媒体，probe_datagrams_sent 单独计，
    /// 两者之和恰等于传输层实际发出的全部数据报。
    #[test]
    fn bw_probe_datagrams_counted_separately_from_media() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        drive_until_datagram(&mut host, &handle, 400); // 至少一个媒体数据报

        let total_bytes = 100_000u32;
        let expected_chunks = crate::probe::bw_probe_chunks(7, total_bytes)
            .expect("chunks")
            .len();
        assert!(expected_chunks > 0);
        handle.deliver_reliable(&ControlMessage::BwProbeRequest {
            probe_id: 7,
            total_bytes,
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        let summary = host.close(ByeReason::Normal).expect("close");

        assert_eq!(
            summary.probe_datagrams_sent, expected_chunks,
            "探测块单独计数（发现 10）"
        );
        // mock 传输记录全部数据报（媒体 + 探测）：两种口径之和恰等于总量。
        assert_eq!(
            summary.datagrams_sent + summary.probe_datagrams_sent,
            handle.datagrams_sent().len(),
            "datagrams_sent 只计媒体（发现 10）"
        );
        assert!(summary.datagrams_sent > 0, "媒体数据报照常计数");
    }

    /// 测试辅助：忽略测试里的第二会话状态读取（避免 unused 警告的直读）。
    fn host2_state(host: &HostSession) -> SessionState {
        host.state()
    }

    /// 线程并发测试用包装：空 poll 时让步 1ms，避免 MockTransport 0ms 空转
    /// 瞬间耗尽 `handshake()` 的分片轮数。
    struct YieldingTransport(MockTransport);
    impl Transport for YieldingTransport {
        fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
            self.0.send_datagram(data)
        }
        fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
            self.0.send_reliable(data)
        }
        fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
            let evs = self.0.poll(timeout)?;
            if evs.is_empty() && !timeout.is_zero() {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(evs)
        }
        fn stats(&self) -> rc_net::NetStats {
            self.0.stats()
        }
    }

    /// T19：密码挑战握手成功路径——线上消息顺序严格为
    /// `Welcome{ChallengeRequired}` → `AuthChallenge` →（收到合法 `AuthResponse`）→
    /// `Welcome{Ok}`，且鉴权终局前零数据报（A6）。
    #[test]
    fn host_password_handshake_succeeds_and_emits_expected_wire_order() {
        let (ta, mut tb) = crate::testkit::pair();
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.auth = HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        };
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(ta)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");

        let mut hello_buf = Vec::new();
        hello_msg(caps(), proto::AuthMethod::Password).encode(&mut hello_buf);
        tb.send_reliable(&hello_buf).expect("send Hello");

        let host_thread = std::thread::spawn(move || {
            let granted = host.handshake().expect("password handshake must succeed");
            (host, granted)
        });

        // 收集过程 Welcome + AuthChallenge（最多轮询若干次等 host 线程产出）。
        let mut received = Vec::new();
        for _ in 0..100 {
            for ev in tb.poll(Duration::from_millis(5)).expect("poll") {
                if let NetEvent::Reliable(m) = ev {
                    received.push(ControlMessage::decode(&m).expect("decode"));
                } else if let NetEvent::Datagram(_) = ev {
                    panic!("A6：鉴权未终局前不得产生任何媒体数据报");
                }
            }
            if received.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(received.len(), 2, "应收到过程 Welcome + AuthChallenge");
        match &received[0] {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.auth_status, AuthStatus::ChallengeRequired);
            }
            other => panic!("首条应为过程 Welcome{{ChallengeRequired}}，实际 {other:?}"),
        }
        let ch = match &received[1] {
            ControlMessage::AuthChallenge(c) => c.clone(),
            other => panic!("次条应为 AuthChallenge，实际 {other:?}"),
        };
        assert_eq!(ch.iterations, 64);

        let proof = crate::auth::compute_proof(b"hunter2", &ch.salt, ch.iterations);
        let mut resp_buf = Vec::new();
        ControlMessage::AuthResponse(proto::AuthResponse { proof }).encode(&mut resp_buf);
        tb.send_reliable(&resp_buf).expect("send AuthResponse");

        let (mut host, granted) = host_thread.join().expect("host thread");
        assert_eq!(granted, caps());
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), 1);

        // 客户端应收到终局 Welcome{Ok}。
        let mut final_welcome = None;
        for ev in tb.poll(Duration::from_millis(5)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                final_welcome = Some(ControlMessage::decode(&m).expect("decode"));
            }
        }
        match final_welcome {
            Some(ControlMessage::Welcome(w)) => {
                assert_eq!(w.auth_status, AuthStatus::Ok);
                assert_eq!(w.granted, caps());
            }
            other => panic!("终局应为 Welcome{{Ok}}，实际 {other:?}"),
        }
        host.close(ByeReason::Normal).expect("close");
    }

    /// T19（A5/A6）：错误 proof → 回 `Welcome{Rejected}` 并立即断开，管线零启动。
    #[test]
    fn host_password_wrong_proof_rejected_without_starting_pipeline() {
        let (ta, mut tb) = crate::testkit::pair();
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.auth = HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        };
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(ta)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");

        let mut hello_buf = Vec::new();
        hello_msg(caps(), proto::AuthMethod::Password).encode(&mut hello_buf);
        tb.send_reliable(&hello_buf).expect("send Hello");

        let host_thread = std::thread::spawn(move || {
            let res = host.handshake();
            (host, res)
        });

        let mut received = Vec::new();
        for _ in 0..100 {
            for ev in tb.poll(Duration::from_millis(5)).expect("poll") {
                if let NetEvent::Reliable(m) = ev {
                    received.push(ControlMessage::decode(&m).expect("decode"));
                }
            }
            if received.len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(received.len(), 2);

        let mut resp_buf = Vec::new();
        ControlMessage::AuthResponse(proto::AuthResponse { proof: [0xFF; 32] })
            .encode(&mut resp_buf);
        tb.send_reliable(&resp_buf)
            .expect("send wrong AuthResponse");

        let (host, res) = host_thread.join().expect("host thread");
        let err = res.expect_err("错误密码必须拒绝");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(host.state(), SessionState::Closed);
        assert_eq!(host.pipeline_generation(), 0, "A6：被拒不得启动管线");

        let mut final_msgs = Vec::new();
        for ev in tb.poll(Duration::from_millis(5)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                final_msgs.push(ControlMessage::decode(&m).expect("decode"));
            }
        }
        assert!(
            final_msgs.iter().any(|m| matches!(
                m,
                ControlMessage::Welcome(w) if w.auth_status == AuthStatus::Rejected
            )),
            "A5：必须回发 Welcome{{Rejected}}，实际 {final_msgs:?}"
        );
    }

    /// T19（A8）：无密码 + 挂起型审批人（`ScriptedApprover([None, None, Accepted])`）→
    /// 首拍发 `Welcome{ChallengeRequired}`（NotifyPending），后续拍非阻塞轮询直至
    /// `Welcome{Ok}` 进入 Streaming。
    #[test]
    fn host_pending_approval_notifies_then_accepts() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let handle = mock.clone();
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.approver = Box::new(crate::auth::ScriptedApprover::new(vec![
            None,
            None,
            Some(crate::auth::ApprovalVerdict::Accepted),
        ]));
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");

        let granted = host.handshake().expect("审批通过后握手应成功");
        assert_eq!(granted, caps());
        assert_eq!(host.state(), SessionState::Streaming);

        let sent = handle.reliable_sent();
        assert_eq!(
            sent.len(),
            2,
            "应先发一条 NotifyPending 过程 Welcome，再发终局 Welcome{{Ok}}"
        );
        match ControlMessage::decode(&sent[0]).expect("decode 0") {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.auth_status, AuthStatus::ChallengeRequired);
            }
            other => panic!("首条应为过程 Welcome{{ChallengeRequired}}，实际 {other:?}"),
        }
        match ControlMessage::decode(&sent[1]).expect("decode 1") {
            ControlMessage::Welcome(w) => {
                assert_eq!(w.auth_status, AuthStatus::Ok);
            }
            other => panic!("次条应为终局 Welcome{{Ok}}，实际 {other:?}"),
        }
        host.close(ByeReason::Normal).expect("close");
    }

    /// T19（A5/A6/A7）：审批超时由注入时钟判定 → 回 `Welcome{Rejected}` 并关闭，
    /// 管线零启动。
    #[test]
    fn host_approval_timeout_rejects_with_injected_clock() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let handle = mock.clone();
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.auth.approval_timeout_us = 1_000;
        cfg.approver = Box::new(crate::auth::ScriptedApprover::new(vec![None]));
        let tick_us = Arc::new(AtomicU64::new(0));
        let clock_ref = Arc::clone(&tick_us);
        let stepping_clock: SessionClock =
            Arc::new(move || clock_ref.fetch_add(600, Ordering::Relaxed) as u32);
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            stepping_clock,
        )
        .expect("host with_parts");

        let err = host.handshake().expect_err("审批超时必须拒绝");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(host.state(), SessionState::Closed);
        assert_eq!(host.pipeline_generation(), 0, "A6：超时拒绝不得启动管线");

        let sent = handle.reliable_sent();
        assert_eq!(sent.len(), 2, "过程 Welcome + 终局 Rejected");
        match ControlMessage::decode(&sent[1]).expect("decode 1") {
            ControlMessage::Welcome(w) => assert_eq!(w.auth_status, AuthStatus::Rejected),
            other => panic!("末条应为 Welcome{{Rejected}}，实际 {other:?}"),
        }
    }

    /// T19（A8）：等待人工审批期间 client 断开 → 必须调用 `approver.cancel()`，
    /// 管线零启动。
    #[test]
    fn host_disconnect_during_pending_approval_cancels_approver() {
        use std::sync::atomic::AtomicBool;

        struct SpyApprover {
            cancelled: Arc<AtomicBool>,
        }
        impl ConnectionApprover for SpyApprover {
            fn submit(&mut self, _req: &crate::auth::ApprovalRequest) {}
            fn poll_verdict(&mut self) -> Option<crate::auth::ApprovalVerdict> {
                None
            }
            fn cancel(&mut self) {
                self.cancelled.store(true, Ordering::SeqCst);
            }
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        mock.deliver(NetEvent::Disconnected {
            reason: "client gave up".into(),
        });
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.approver = Box::new(SpyApprover {
            cancelled: Arc::clone(&cancelled),
        });
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(mock),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");

        let err = host.handshake().expect_err("等待期断开应返回 Err");
        assert!(matches!(err, Error::Network(_)), "{err}");
        assert!(
            cancelled.load(Ordering::SeqCst),
            "A8：client 断开时必须调用 approver.cancel()"
        );
        assert_eq!(host.pipeline_generation(), 0);
    }

    /// T19（A11）：Streaming 期收到 `AuthChallenge` / `AuthResponse` / `Welcome`
    /// 一律按致命 `Error::Protocol` 断开。
    #[test]
    fn host_streaming_rejects_midstream_auth_messages_fatally() {
        let cases = [
            ControlMessage::AuthChallenge(proto::AuthChallenge {
                salt: [1u8; proto::AUTH_SALT_LEN],
                iterations: 64,
            }),
            ControlMessage::AuthResponse(proto::AuthResponse {
                proof: [2u8; proto::AUTH_PROOF_LEN],
            }),
            ControlMessage::Welcome(Welcome {
                session_id: 1,
                granted: caps(),
                auth_status: AuthStatus::Ok,
                host_flags: HostFlags(0),
            }),
        ];
        for msg in cases {
            let (inj, _) = RecordingInjector::new();
            let (mut host, mut handle) = handshaken(Box::new(inj), caps());
            handle.deliver_reliable(&msg);
            let err = host
                .tick(Duration::from_millis(0))
                .expect_err("A11：Streaming 期鉴权消息必须致命拒绝");
            assert!(matches!(err, Error::Protocol { .. }), "{err}");
            assert!(err.is_fatal(), "A11：必须为致命错误");
            assert_eq!(host.state(), SessionState::Closed, "A11：必须立即断开");
        }
    }

    /// T19（A2）：Detached 期重新握手（新连接发 `Hello{Password}`）必须生成全新
    /// 随机盐，重放上一连接的旧 proof 必被拒且不拆旧管线；用新盐算出的 proof 通过。
    #[test]
    fn host_detached_reconnect_uses_fresh_salt() {
        let (ta, mut tb) = crate::testkit::pair();
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.auth = HostAuthConfig {
            password: Some("secret-pass".into()),
            iterations: 64,
            approval_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        };
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(ta)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");

        let mut hello_buf = Vec::new();
        hello_msg(caps(), proto::AuthMethod::Password).encode(&mut hello_buf);
        tb.send_reliable(&hello_buf).expect("send Hello #1");

        let host_thread = std::thread::spawn(move || {
            host.handshake().expect("first handshake");
            host
        });

        let mut salt1 = None;
        for _ in 0..100 {
            for ev in tb.poll(Duration::from_millis(5)).expect("poll") {
                if let NetEvent::Reliable(m) = ev {
                    if let ControlMessage::AuthChallenge(ch) =
                        ControlMessage::decode(&m).expect("decode")
                    {
                        salt1 = Some(ch.salt);
                    }
                }
            }
            if salt1.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let salt1 = salt1.expect("first handshake must emit AuthChallenge");
        let proof1 = crate::auth::compute_proof(b"secret-pass", &salt1, 64);
        let mut resp1 = Vec::new();
        ControlMessage::AuthResponse(proto::AuthResponse { proof: proof1 }).encode(&mut resp1);
        tb.send_reliable(&resp1).expect("send AuthResponse #1");

        let mut host = host_thread.join().expect("host thread");
        let gen1 = host.pipeline_generation();
        assert_eq!(gen1, 1);

        // 注入新建管线工厂，随后断线进 Detached。
        host.set_pipeline_factory(Box::new(|| {
            HostPipeline::with_backends_paced(
                pcfg(),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                true,
            )
        }));
        host.on_disconnected(1_000);
        assert_eq!(host.state(), SessionState::Detached);

        // 候选 1：发 Hello{Password} → 拿到全新盐 salt2（!= salt1）→ 重放 proof1 必被拒。
        let mut c1 = MockTransport::new();
        c1.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        let mut c1_handle = c1.clone();
        host.offer_transport(Box::new(c1)).expect("offer c1");
        assert_eq!(
            host.tick_detached(2_000).expect("tick c1 hello"),
            DetachOutcome::Waiting
        );
        let c1_sent = c1_handle.reliable_sent();
        assert_eq!(
            c1_sent.len(),
            2,
            "Welcome{{ChallengeRequired}} + AuthChallenge"
        );
        let salt2 = match ControlMessage::decode(&c1_sent[1]).expect("decode ch2") {
            ControlMessage::AuthChallenge(ch) => ch.salt,
            other => panic!("应为 AuthChallenge，实际 {other:?}"),
        };
        assert_ne!(salt2, salt1, "A2：重连重新握手必须生成全新随机盐");

        // 重放旧 proof1 → 被拒（Welcome{Rejected}），旧管线不动。
        c1_handle.deliver_reliable(&ControlMessage::AuthResponse(proto::AuthResponse {
            proof: proof1,
        }));
        assert_eq!(
            host.tick_detached(3_000).expect("tick c1 replay"),
            DetachOutcome::Waiting
        );
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(
            host.pipeline_generation(),
            gen1,
            "A6：重放被拒不得拆/建管线"
        );
        let c1_sent_after = c1_handle.reliable_sent();
        match ControlMessage::decode(c1_sent_after.last().unwrap()).expect("decode rej") {
            ControlMessage::Welcome(w) => assert_eq!(w.auth_status, AuthStatus::Rejected),
            other => panic!("重放应回 Welcome{{Rejected}}，实际 {other:?}"),
        }

        // 候选 2：正确响应新盐 → Replaced，进入 Streaming。
        let mut c2 = MockTransport::new();
        c2.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        let mut c2_handle = c2.clone();
        host.offer_transport(Box::new(c2)).expect("offer c2");
        assert_eq!(
            host.tick_detached(4_000).expect("tick c2 hello"),
            DetachOutcome::Waiting
        );
        let c2_sent = c2_handle.reliable_sent();
        let salt3 = match ControlMessage::decode(&c2_sent[1]).expect("decode ch3") {
            ControlMessage::AuthChallenge(ch) => ch.salt,
            other => panic!("应为 AuthChallenge，实际 {other:?}"),
        };
        assert_ne!(salt3, salt2, "A2：每次握手独立随机盐");
        let proof3 = crate::auth::compute_proof(b"secret-pass", &salt3, 64);
        c2_handle.deliver_reliable(&ControlMessage::AuthResponse(proto::AuthResponse {
            proof: proof3,
        }));
        assert_eq!(
            host.tick_detached(5_000).expect("tick c2 resp"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen1 + 1);
        host.close(ByeReason::Normal).expect("close");
    }

    /// T19 回归：Detached 期候选 1 被 `ScriptedApprover` 终局拒绝（不触发 `cancel()`）后，
    /// 候选 2 再次 `submit()` 不得因残留 `in_flight` 触发 `debug_assert!` panic。
    #[test]
    fn host_detached_scripted_approver_reject_then_accept_next_candidate() {
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.approver = Box::new(crate::auth::ScriptedApprover::new(vec![
            Some(crate::auth::ApprovalVerdict::Accepted),
            None,
            Some(crate::auth::ApprovalVerdict::Rejected),
            Some(crate::auth::ApprovalVerdict::Accepted),
        ]));

        let mut t0 = MockTransport::new();
        t0.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(t0),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("host with_parts");
        host.handshake().expect("initial handshake accepted");
        let gen1 = host.pipeline_generation();

        host.set_pipeline_factory(Box::new(|| {
            HostPipeline::with_backends_paced(
                pcfg(),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                true,
            )
        }));
        host.on_disconnected(1_000);
        assert_eq!(host.state(), SessionState::Detached);

        // 候选 1：首次轮询返回 None（Waiting），第二次返回 Rejected（终局拒绝，不调 cancel）。
        let mut c1 = MockTransport::new();
        c1.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let c1_handle = c1.clone();
        host.offer_transport(Box::new(c1)).expect("offer c1");
        assert_eq!(
            host.tick_detached(2_000).expect("tick c1 pending"),
            DetachOutcome::Waiting
        );
        assert_eq!(
            host.tick_detached(3_000).expect("tick c1 rejected"),
            DetachOutcome::Waiting
        );
        assert_eq!(host.state(), SessionState::Detached);
        assert_eq!(host.pipeline_generation(), gen1);
        match ControlMessage::decode(c1_handle.reliable_sent().last().unwrap()).expect("decode rej")
        {
            ControlMessage::Welcome(w) => assert_eq!(w.auth_status, AuthStatus::Rejected),
            other => panic!("候选 1 应回 Welcome{{Rejected}}，实际 {other:?}"),
        }

        // 候选 2：再次 submit 不得触发 debug_assert!(!self.in_flight)，并成功被接受。
        let mut c2 = MockTransport::new();
        c2.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        host.offer_transport(Box::new(c2)).expect("offer c2");
        assert_eq!(
            host.tick_detached(4_000).expect("tick c2 accepted"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen1 + 1);
        host.close(ByeReason::Normal).expect("close");
    }

    /// H1 回归：
    /// 1) 无密码模式下 `ReconnectRequest` miss（错 sid）绝不拆保留管线，后续合法
    ///    `ReconnectRequest` 仍可命中 resume（`pipeline_generation` 不变）；
    /// 2) 密码启用模式下，即便 `session_id` 匹配且在 TTL 内，`ReconnectRequest` 也必须
    ///    被拒绝（`!resume_accepted()`）且不拆旧管线，迫使对端同连接降级为全新 Hello →
    ///    PBKDF2 新鲜盐挑战 → `Replaced`。
    #[test]
    fn host_reconnect_miss_preserves_pipeline_and_password_rejects_fast_resume() {
        // ── 子用例 1：无密码 miss 不拆管线 ──
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        let gen1 = host.pipeline_generation();
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host.state(), SessionState::Detached);

        // 陌生候选发错 sid 的 ReconnectRequest → miss，但旧管线保持存活！
        let mut wrong = MockTransport::new();
        wrong.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 9999,
            resume: true,
        });
        let wrong_handle = wrong.clone();
        host.offer_transport(Box::new(wrong)).expect("offer wrong");
        assert_eq!(
            host.tick_detached(2_000).expect("tick wrong"),
            DetachOutcome::Waiting
        );
        assert!(host.pipeline_alive, "H1：miss 不得拆除保留管线");
        match ControlMessage::decode(wrong_handle.reliable_sent().last().unwrap()).unwrap() {
            ControlMessage::Welcome(w) => assert!(!w.host_flags.resume_accepted()),
            other => panic!("应回 Welcome{{bit0=0}}，实际 {other:?}"),
        }

        // 合法原 client 随后发正确 sid 的 ReconnectRequest → 仍能命中 resume（gen 不变）
        let mut legit = MockTransport::new();
        legit.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        host.offer_transport(Box::new(legit)).expect("offer legit");
        assert_eq!(
            host.tick_detached(3_000).expect("tick legit"),
            DetachOutcome::Resumed
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen1);
        host.close(ByeReason::Normal).expect("close");

        // ── 子用例 2：启用密码时拒绝未鉴权快速 resume，且不拆旧管线 ──
        let (inj2, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj2));
        cfg.auth = crate::auth::HostAuthConfig {
            password: Some("pw123".into()),
            iterations: 64,
            approval_timeout_us: 10_000_000,
            ..crate::auth::HostAuthConfig::default()
        };
        let mut t0 = MockTransport::new();
        t0.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        let t0_handle = t0.clone();
        let mut t0_clone = t0.clone();
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(t0)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        // 用 YieldingTransport 或分步不够方便，这里用后台线程完成初次密码握手：
        let mut th_handle = t0_clone.clone();
        let responder = std::thread::spawn(move || {
            for _ in 0..500 {
                let sent = th_handle.reliable_sent();
                if sent.len() >= 2 {
                    if let Ok(ControlMessage::AuthChallenge(ch)) = ControlMessage::decode(&sent[1])
                    {
                        let proof = crate::auth::compute_proof(b"pw123", &ch.salt, ch.iterations);
                        th_handle.deliver_reliable(&ControlMessage::AuthResponse(
                            proto::AuthResponse { proof },
                        ));
                        return ch.salt;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("timeout waiting for AuthChallenge");
        });
        host.handshake().expect("password handshake");
        let salt1 = responder.join().expect("responder join");
        let _ = t0_handle;
        let _ = &mut t0_clone;
        let gen_pw = host.pipeline_generation();
        host.set_pipeline_factory(Box::new(|| {
            HostPipeline::with_backends_paced(
                pcfg(),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                true,
            )
        }));
        host.on_disconnected(1_000);

        // 对端发 ReconnectRequest{sid=4242, resume=true} → 密码模式下必须拒绝（bit0=0）且旧管线存活
        let mut cand = MockTransport::new();
        cand.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        let mut cand_handle = cand.clone();
        host.offer_transport(Box::new(cand)).expect("offer cand");
        assert_eq!(
            host.tick_detached(2_000).expect("tick reconnect req"),
            DetachOutcome::Waiting
        );
        assert!(
            host.pipeline_alive,
            "H1：密码拒绝快速 resume 时不得拆旧管线"
        );
        assert_eq!(host.pipeline_generation(), gen_pw);
        match ControlMessage::decode(&cand_handle.reliable_sent()[0]).unwrap() {
            ControlMessage::Welcome(w) => assert!(!w.host_flags.resume_accepted()),
            other => panic!("应回 Welcome{{bit0=0}}，实际 {other:?}"),
        }

        // 同连接降级全新 Hello → 收到新鲜盐 AuthChallenge → AuthResponse → Replaced
        cand_handle.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::Password));
        assert_eq!(
            host.tick_detached(3_000).expect("tick fresh hello"),
            DetachOutcome::Waiting
        );
        let sent = cand_handle.reliable_sent();
        let ch2 = match ControlMessage::decode(sent.last().unwrap()).unwrap() {
            ControlMessage::AuthChallenge(ch) => ch,
            other => panic!("应发 AuthChallenge，实际 {other:?}"),
        };
        assert_ne!(ch2.salt, salt1, "A2：降级 Hello 必须使用新鲜盐");
        let proof2 = crate::auth::compute_proof(b"pw123", &ch2.salt, ch2.iterations);
        cand_handle.deliver_reliable(&ControlMessage::AuthResponse(proto::AuthResponse {
            proof: proof2,
        }));
        assert_eq!(
            host.tick_detached(4_000).expect("tick auth resp"),
            DetachOutcome::Replaced
        );
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(host.pipeline_generation(), gen_pw + 1);
        host.close(ByeReason::Normal).expect("close");
    }

    /// H2 回归：Detached 期静默候选（不发 Hello/ReconnectRequest）与挑战停滞候选
    /// （收到 AuthChallenge 后不回 AuthResponse）都会在超时后被 `drop_detached_candidate`
    /// 踢出，解除对 `self.transport` 的霸占。
    #[test]
    fn host_detached_candidate_timeout_drops_silent_and_stalled_challenge_peers() {
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.handshake_timeout = Duration::from_millis(50); // 50_000 us
        cfg.auth = crate::auth::HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: 20_000,
            ..crate::auth::HostAuthConfig::default()
        };
        let mut t0 = MockTransport::new();
        t0.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(t0)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        host.handshake().expect("handshake");
        host.on_disconnected(1_000);

        // 静默候选：从不发任何消息
        let silent = MockTransport::new();
        host.offer_transport(Box::new(silent))
            .expect("offer silent");
        // 首拍锚定截止时间 = 2_000 + max(50_000, 20_000) = 52_000
        assert_eq!(
            host.tick_detached(2_000).expect("tick anchor"),
            DetachOutcome::Waiting
        );
        assert!(host.transport.is_some());
        assert_eq!(host.dirty_candidates(), 0);
        // 到期前仍保留
        assert_eq!(
            host.tick_detached(51_999).expect("tick before deadline"),
            DetachOutcome::Waiting
        );
        assert!(host.transport.is_some());
        // 到期踢出候选，释放 transport
        assert_eq!(
            host.tick_detached(52_000).expect("tick at deadline"),
            DetachOutcome::Waiting
        );
        assert!(host.transport.is_none(), "H2：超时候选必须释放 transport");
        assert_eq!(host.dirty_candidates(), 1);
        assert_eq!(host.state(), SessionState::Detached);
        host.close(ByeReason::Normal).expect("close");
    }

    /// H3 回归：`PendingApproval` 期收到意外 `AuthResponse` 触发 `AuthStepOutcome::Rejected`
    /// 时，无论在 `handshake()` 还是 `finish_detached_auth_step()`，都必须调用
    /// `approver.cancel()` 避免 `ScriptedApprover.in_flight` 泄漏。
    #[test]
    fn host_unexpected_auth_response_during_pending_approval_cancels_approver() {
        // ── 子用例 1：Detached 期 PendingApproval 收到意外 AuthResponse ──
        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.approver = Box::new(crate::auth::ScriptedApprover::new(vec![
            Some(crate::auth::ApprovalVerdict::Accepted), // 初次握手
            None,                                         // 候选 1 进入 PendingApproval
            Some(crate::auth::ApprovalVerdict::Accepted), // 候选 2 再次 submit（未 cancel 会 panic）
        ]));
        let mut t0 = MockTransport::new();
        t0.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let mut host = HostSession::with_parts(
            cfg,
            Box::new(YieldingTransport(t0)),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        host.handshake().expect("initial handshake");
        host.set_pipeline_factory(Box::new(|| {
            HostPipeline::with_backends_paced(
                pcfg(),
                fake_capture(FakeContent::Dynamic),
                fake_encoder(),
                true,
            )
        }));
        host.on_disconnected(1_000);

        // 候选 1 发 Hello 进入 PendingApproval（approver.in_flight = true）
        let mut c1 = MockTransport::new();
        c1.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        let mut c1_handle = c1.clone();
        host.offer_transport(Box::new(c1)).expect("offer c1");
        assert_eq!(
            host.tick_detached(2_000).expect("c1 pending"),
            DetachOutcome::Waiting
        );
        // 在 PendingApproval 期突兀发 AuthResponse → AuthStepOutcome::Rejected，必须 cancel approver
        c1_handle.deliver_reliable(&ControlMessage::AuthResponse(proto::AuthResponse {
            proof: [0u8; 32],
        }));
        assert_eq!(
            host.tick_detached(3_000).expect("c1 rejected"),
            DetachOutcome::Waiting
        );
        assert!(host.transport.is_none());

        // 候选 2 再次 submit 不触发 debug_assert!(!in_flight) 并成功 Replaced
        let mut c2 = MockTransport::new();
        c2.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        host.offer_transport(Box::new(c2)).expect("offer c2");
        assert_eq!(
            host.tick_detached(4_000).expect("c2 accepted"),
            DetachOutcome::Replaced
        );
        host.close(ByeReason::Normal).expect("close");

        // ── 子用例 2：handshake() 期 PendingApproval 收到意外 AuthResponse ──
        struct CancelCounter {
            cancelled: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl crate::auth::ConnectionApprover for CancelCounter {
            fn submit(&mut self, _req: &crate::auth::ApprovalRequest) {}
            fn poll_verdict(&mut self) -> Option<crate::auth::ApprovalVerdict> {
                None
            }
            fn cancel(&mut self) {
                self.cancelled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (inj2, _) = RecordingInjector::new();
        let mut cfg2 = host_cfg(Box::new(inj2));
        cfg2.approver = Box::new(CancelCounter {
            cancelled: Arc::clone(&cancelled),
        });
        let mut t1 = MockTransport::new();
        t1.deliver_reliable(&hello_msg(caps(), proto::AuthMethod::None));
        t1.deliver_reliable(&ControlMessage::AuthResponse(proto::AuthResponse {
            proof: [0u8; 32],
        }));
        let mut host2 = HostSession::with_parts(
            cfg2,
            Box::new(t1),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        assert!(host2.handshake().is_err());
        assert!(
            cancelled.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "H3：handshake 拒绝分支必须调用 approver.cancel()"
        );
    }

    /// M1 回归：`transport.poll(...)` 返回 `Err` 时，
    /// `handshake()` 走 `handshake_fail`（cancel approver + Closed + Err），
    /// `tick_detached()` 走 `drop_detached_candidate`（计数 + 丢连接 + 保持 Detached + Ok(Waiting)）。
    #[test]
    fn host_transport_poll_error_cleans_up_in_handshake_and_detached() {
        struct PollErrTransport;
        impl Transport for PollErrTransport {
            fn send_datagram(&mut self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            fn send_reliable(&mut self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
                Err(Error::Network("simulated poll failure".into()))
            }
            fn stats(&self) -> rc_net::NetStats {
                rc_net::NetStats::default()
            }
        }

        // handshake() 路径
        let (inj, _) = RecordingInjector::new();
        let mut host = HostSession::with_parts(
            host_cfg(Box::new(inj)),
            Box::new(PollErrTransport),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");
        let err = host
            .handshake()
            .expect_err("poll err should fail handshake");
        assert!(matches!(err, Error::Network(_)));
        assert_eq!(host.state(), SessionState::Closed);

        // tick_detached() 路径
        let (inj2, _) = RecordingInjector::new();
        let (mut host2, mut handle2) = handshaken(Box::new(inj2), caps());
        handle2.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host2.tick(Duration::from_millis(0)).expect("tick");
        assert_eq!(host2.state(), SessionState::Detached);
        host2
            .offer_transport(Box::new(PollErrTransport))
            .expect("offer");
        assert_eq!(
            host2
                .tick_detached(2_000)
                .expect("M1：poll 错不得上抛杀 host"),
            DetachOutcome::Waiting
        );
        assert_eq!(host2.state(), SessionState::Detached);
        assert!(host2.transport.is_none());
        assert_eq!(host2.dirty_candidates(), 1);
        host2.close(ByeReason::Normal).expect("close");
    }

    /// M5 回归：`finish_detached_auth_step` 的 `Rejected` 分支在丢弃 transport 前执行
    /// 50ms `poll` 排空窗口（与 `handshake()` 一致）。
    #[test]
    fn host_detached_auth_rejected_flushes_transport_before_drop() {
        struct SpyPollTransport {
            events: Vec<NetEvent>,
            poll_timeouts: Arc<std::sync::Mutex<Vec<Duration>>>,
        }
        impl Transport for SpyPollTransport {
            fn send_datagram(&mut self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            fn send_reliable(&mut self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
                self.poll_timeouts.lock().unwrap().push(timeout);
                Ok(std::mem::take(&mut self.events))
            }
            fn stats(&self) -> rc_net::NetStats {
                rc_net::NetStats::default()
            }
        }

        let (inj, _) = RecordingInjector::new();
        let mut cfg = host_cfg(Box::new(inj));
        cfg.auth.password = Some("secret".into());
        let (mut host, mut handle) = handshaken(Box::new(RecordingInjector::new().0), caps());
        host.cfg.auth.password = Some("secret".into());
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        host.tick(Duration::from_millis(0)).expect("tick");

        // 发 AuthMethod::None（密码已启用 → 立即 Conclude(Rejected)）
        let mut hello_bytes = Vec::new();
        hello_msg(caps(), proto::AuthMethod::None).encode(&mut hello_bytes);
        let timeouts = Arc::new(std::sync::Mutex::new(Vec::new()));
        host.offer_transport(Box::new(SpyPollTransport {
            events: vec![NetEvent::Reliable(hello_bytes)],
            poll_timeouts: Arc::clone(&timeouts),
        }))
        .expect("offer spy");
        assert_eq!(
            host.tick_detached(2_000).expect("tick rejected"),
            DetachOutcome::Waiting
        );
        let recorded = timeouts.lock().unwrap().clone();
        assert!(
            recorded.contains(&Duration::from_millis(50)),
            "M5：Rejected 路径丢连接前应执行 50ms poll 排空，实际 timeouts={recorded:?}"
        );
        host.close(ByeReason::Normal).expect("close");
    }

    /// H6 回归：HostSession 在 `handle_reconnect` 命中后自动重置剪贴板纪元态，
    /// 时钟归零到 0 后本地剪贴板变更仍可正常同步且保留统计。
    #[test]
    fn host_reset_clipboard_epoch_on_reconnect_allows_sync_after_clock_reset() {
        let (inj, _) = RecordingInjector::new();
        let (mut host, mut handle) = handshaken(Box::new(inj), caps());
        let cb = crate::testkit::SharedFakeClipboard::new();
        let cb_ctrl = cb.clone();
        host.enable_clipboard(Box::new(cb));

        // 旧纪元高位时刻基线 + 发送 1 条
        host.pump_clipboard(4_000_000_000).expect("baseline");
        cb_ctrl.simulate_local_copy(Some("before-reconnect"));
        host.pump_clipboard(4_000_010_000).expect("send 1");
        assert_eq!(host.clipboard_stats().sent, 1);

        // 断线 + resume 命中（自动 reset_clipboard_epoch）
        let _ = &mut handle;
        host.on_disconnected(4_000_020_000);
        let mut t2 = MockTransport::new();
        t2.deliver_reliable(&ControlMessage::ReconnectRequest {
            session_id: 4242,
            resume: true,
        });
        host.offer_transport(Box::new(t2)).expect("offer t2");
        assert_eq!(
            host.tick_detached(4_000_050_000).expect("resume"),
            DetachOutcome::Resumed
        );

        // 新纪元时钟归零到 0 → 首拍基线，随后变更立发（不被旧 last_sent_us 卡死）
        host.pump_clipboard(0).expect("new baseline");
        cb_ctrl.simulate_local_copy(Some("after-reset"));
        host.pump_clipboard(1_000).expect("send 2");
        assert_eq!(host.clipboard_stats().sent, 2);
        host.close(ByeReason::Normal).expect("close");
    }

    #[test]
    fn host_reconfigure_stream_rebuilds_pipeline_and_continues_streaming() {
        use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
        use rc_encode::{EncoderConfig, FakeEncoder};
        use rc_types::{DisplayId, Resolution};

        let (inj, _) = RecordingInjector::new();
        let initial_caps = SessionParams {
            width: 1920,
            height: 1080,
            fps: 144,
            bitrate_bps: 20_000_000,
        };
        let mut cfg = host_cfg(Box::new(inj));
        cfg.caps = initial_caps;
        cfg.pipeline.resolution = Resolution::new(1920, 1080);
        cfg.pipeline.fps = 60;
        cfg.pipeline.bitrate_bps = 12_000_000;

        let mut transport = MockTransport::new();
        let handle = transport.clone();
        transport.deliver_reliable(&hello_msg(
            SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 12_000_000,
            },
            proto::AuthMethod::None,
        ));

        let mut host = HostSession::with_parts(
            cfg,
            Box::new(transport),
            fake_capture(FakeContent::Dynamic),
            fake_encoder(),
            zero_clock(),
        )
        .expect("with_parts");

        let built_configs = Arc::new(std::sync::Mutex::new(Vec::<(u32, u32, u32, u32)>::new()));
        let built_configs_cb = Arc::clone(&built_configs);
        host.set_pipeline_factory_for(Box::new(move |pcfg| {
            built_configs_cb.lock().unwrap().push((
                pcfg.resolution.width,
                pcfg.resolution.height,
                pcfg.fps,
                pcfg.bitrate_bps,
            ));
            let cap = FakeBackend::new(FakeBackendConfig {
                display: DisplayInfo {
                    id: DisplayId(0),
                    resolution: pcfg.resolution,
                    scale: 1.0,
                    hdr: false,
                },
                fps: pcfg.fps,
                content: FakeContent::Dynamic,
                dirty_rect: None,
            })?;
            let enc = FakeEncoder::with_tuning(
                EncoderConfig {
                    resolution: pcfg.resolution,
                    fps: pcfg.fps,
                    bitrate_bps: pcfg.bitrate_bps,
                    ..EncoderConfig::default()
                },
                500,
                128,
            )?;
            HostPipeline::with_backends_paced(pcfg.clone(), Box::new(cap), Box::new(enc), true)
        }));

        let granted = host.handshake().expect("handshake");
        assert_eq!(granted.width, 1920);
        assert_eq!(granted.height, 1080);
        assert_eq!(granted.fps, 60);
        assert_eq!(host.pipeline_generation(), 1);

        // 先驱动出至少 1 帧数据报
        for _ in 0..200 {
            let r = host.tick(Duration::ZERO).expect("tick before reconfig");
            if r.datagrams_sent > 0 {
                break;
            }
            host.pipeline().wait_event(Duration::from_millis(5));
        }
        let before_dgs = handle.datagrams_sent();
        assert!(!before_dgs.is_empty(), "切换前应已产出媒体数据报");
        let max_seq_before = before_dgs
            .iter()
            .filter_map(|d| {
                rc_proto::MediaHeader::parse_datagram(d)
                    .ok()
                    .map(|(h, _)| h.frame_seq.0)
            })
            .max()
            .expect("header seq");

        // 幂等空操作：与当前参数一致不重建管线
        let noop = host
            .reconfigure_stream(1920, 1080, Some(60), None)
            .expect("noop reconfigure");
        assert_eq!(noop, granted);
        assert_eq!(host.pipeline_generation(), 1);

        // 热切换到 1280x720@120fps（允许 max_resolution 覆盖，奇数自动向下取偶）
        let updated = host
            .reconfigure_stream(1281, 721, Some(120), Some(Resolution::new(3024, 1964)))
            .expect("live reconfigure");
        assert_eq!(updated.width, 1280);
        assert_eq!(updated.height, 720);
        assert_eq!(updated.fps, 120);
        assert_eq!(host.negotiated(), Some(updated));
        assert_eq!(host.pipeline_generation(), 2);

        // 切换后继续驱动，应持续产出新数据报且线上 frame_seq 严格大于切换前
        let count_after_reconfig = handle.datagrams_sent().len();
        let mut got_new_frame = false;
        for _ in 0..200 {
            let _ = host.tick(Duration::ZERO).expect("tick after reconfig");
            let cur_dgs = handle.datagrams_sent();
            if cur_dgs.len() > count_after_reconfig {
                let max_seq_after = cur_dgs[count_after_reconfig..]
                    .iter()
                    .filter_map(|d| {
                        rc_proto::MediaHeader::parse_datagram(d)
                            .ok()
                            .map(|(h, _)| h.frame_seq.0)
                    })
                    .max()
                    .expect("new header seq");
                assert!(
                    max_seq_after > max_seq_before,
                    "热切换后线上 frame_seq({max_seq_after}) 必须严格大于切换前({max_seq_before})"
                );
                got_new_frame = true;
                break;
            }
            host.pipeline().wait_event(Duration::from_millis(5));
        }
        assert!(got_new_frame, "reconfigure_stream 后管线必须继续持续出帧");

        // 验证工厂确实收到 (1920,1080,60) 和 (1280,720,120) 两次构造
        let recorded = built_configs.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![(1920, 1080, 60, 12_000_000), (1280, 720, 120, 12_000_000)]
        );

        host.close(ByeReason::Normal).expect("close");
    }

    #[test]
    fn has_pending_pipeline_work_and_set_injector_screen_size_work() {
        struct SizeTrackInjector {
            sizes: Arc<Mutex<Vec<(u32, u32)>>>,
        }
        impl InputInjector for SizeTrackInjector {
            fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
                Ok(())
            }
            fn set_screen_size(&mut self, w: u32, h: u32) {
                self.sizes.lock().unwrap().push((w, h));
            }
        }

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let transport = MockTransport::new();
        let cfg = HostSessionConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            SessionParams {
                width: 64,
                height: 64,
                fps: 30,
                bitrate_bps: 1_000_000,
            },
            Box::new(SizeTrackInjector {
                sizes: Arc::clone(&sizes),
            }),
        );
        let cap = FakeBackend::new(FakeBackendConfig {
            fps: 30,
            content: FakeContent::Static,
            ..FakeBackendConfig::default()
        })
        .unwrap();
        let enc = FakeEncoder::new(EncoderConfig {
            resolution: rc_types::Resolution::new(1920, 1080),
            fps: 30,
            ..EncoderConfig::default()
        })
        .unwrap();
        let mut session = HostSession::with_parts(
            cfg,
            Box::new(transport),
            Box::new(cap),
            Box::new(enc),
            Arc::new(|| 0),
        )
        .unwrap();

        assert!(!session.has_pending_pipeline_work());
        session.set_injector_screen_size(1800, 1169);
        assert_eq!(*sizes.lock().unwrap(), vec![(1800, 1169)]);
    }
}
