//! 控制端会话：QuicEndpoint(connect) + QuicTransport + ControllerPipeline + 输入泵。
//!
//! 与 [`crate::host_session`] 对称（方向相反）：connect + Hello/Welcome
//! 握手、三通道分发（媒体 datagram / 控制 reliable / 输入 urgent）、
//! 照 `run_real` 形态的驱动循环与 client 侧停机红线（S6）。
//!
//! **T17 增补断线重连**：`Disconnected` 不再等价于会话结束——保留
//! ControllerPipeline / session_id / QuicEndpoint 进
//! [`SessionState::Reconnecting`]，按指数退避自动重连（
//! [`ClientSession::tick_reconnect`]），resume 被接受则复用管线并强制
//! IDR，被拒则同连接降级全新 Hello；收到 `Bye(Refresh)` 零退避全新重建
//! （[`ClientSession::on_bye_refresh`]）。非 Streaming 态看门狗不评估、
//! 探测与输入泵暂停（R8：tick/pump_probes 均要求 Streaming，天然门禁）。
//!
//! # 双时间轴（坑 5）
//!
//! 管线打点用 u32 会话钟（[`SessionClock`] 注入闭包）；输入批窗口用
//! `Instant`——经 [`ClientSession::tick`] 的 `now` 参数传入，文档要求与
//! 会话钟同轴。[`ClientSession::run`] 内取 `Instant::now()` 是**驱动器的
//! 合法时钟源**（与 `run_real` 的 `start.elapsed()` 同一纪律）；后续卡
//! （T15/T20 等）的铁律 4 grep 自检必须豁免本文件。
//!
//! # 停机顺序红线（S6，client 侧）
//!
//! 发 Bye → 停 pump（无独立线程，天然停）→ 继续收/推数据报到静默或
//! 500ms 上限 → `finish_in_flight()` → `controller.shutdown()`。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_decode::Decoder;
use rc_net::quic::{QuicEndpoint, DEFAULT_MTU};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_pipeline::{ControllerPipeline, PipelineConfig};
use rc_render::Renderer;
use rc_types::{Error, FrameSeq, Result};

use crate::auth::{ClientAuthAction, ClientAuthConfig, ClientAuthenticator};
use crate::clipboard::{
    ClipboardAction, ClipboardBackend, ClipboardChannel, ClipboardConfig, ClipboardStats,
};
use crate::proto::{
    self, AudioFramePacket, AuthStatus, ByeReason, ControlMessage, Hello, IdrReason, SessionParams,
};
use crate::reconnect::{Backoff, ReconnectConfig, ReconnectStats};
use crate::watchdog::{
    DeliveryTracker, IdrController, IdrControllerConfig, StallWatchdog, DEFAULT_STALL_US,
};
use crate::{SessionClock, SessionState, StopReason};

/// 握手等待的 poll 分片（与 host 侧同一手法：不读墙钟的到期实现）。
const HANDSHAKE_POLL_SLICE: Duration = Duration::from_millis(50);
/// run 循环的常规 poll 预算（坑 2：网络等待不饿死管线）。
const RUN_POLL_BUDGET: Duration = Duration::from_millis(1);
/// run 循环空闲拍放宽的 poll 预算（N4：client 无 wait_event，靠 poll
/// 预算承担等待）。
const RUN_IDLE_POLL_BUDGET: Duration = Duration::from_millis(10);
/// 停机排空：poll 分片与轮数上限（100 × 5ms = 500ms，S6）。
const DRAIN_POLL_SLICE: Duration = Duration::from_millis(5);
const DRAIN_ROUNDS: u32 = 100;
/// Reconnecting 期 poll 等待重连裁定（Welcome）的分片（防驱动循环自旋；
/// mock transport 忽略超时不受影响）。
const RECONNECT_POLL_SLICE: Duration = Duration::from_millis(5);
/// run 循环在 Reconnecting 退避等待时的最大睡眠粒度（等待 Welcome 或
/// 下一尝试点；驱动器的合法时钟源，坑 5 同源豁免）。
const RUN_RECONNECT_SLEEP: Duration = Duration::from_millis(20);

/// 媒体数据报首字节：rc-proto `MAGIC` = 0x3156_4352 的 LE 编码首字节
/// （`RCV1` 的 `R`）。rc-proto 不是 rc-session 的正式依赖（仅 dev），完整
/// MAGIC 校验由管线 `MediaHeader::parse_datagram` 把关——这里只做 B4 的
/// 分流前置判定。
const MEDIA_DATAGRAM_FIRST_BYTE: u8 = 0x52;

/// T18：在途关键批发送时刻表的容量上限（防 ack 永不到达时无限增长；
/// 30s 级会话的关键批 ≤ 数百，8192 绰绰有余）。
const PENDING_ACK_CAP: usize = 8192;

/// T18：每帧包头时间戳表的容量上限（超出淘汰最旧；报告只消费 trace
/// 内的帧，trace 容量同级即可）。
const FRAME_HEADER_CAP: usize = 8192;

/// 工具复用接收队列有界容量上限（可靠通道绝不静默丢帧，满时返回协议错误）。
const MAX_PENDING_TOOL_MESSAGES: usize = 2048;
/// 音频帧接收队列有界容量上限（低延迟优先，满时丢弃最旧帧）。
const MAX_PENDING_AUDIO_FRAMES: usize = 32;

/// 控制端会话配置。
pub struct ClientSessionConfig {
    /// 对端（被控端）地址。
    pub peer_addr: SocketAddr,
    /// 路径 MTU 预算，默认 `rc_net::quic::DEFAULT_MTU`。
    pub mtu: usize,
    /// Hello 请求参数（裁定后以 Welcome 回值为准，此值不生效）。
    pub requested: SessionParams,
    /// 会话号（client 生成；调用方负责，测试用固定值保证确定性）。
    pub session_id: u64,
    /// 管线配置模板；resolution/fps/bitrate 在握手后以裁定值覆盖。
    pub pipeline: PipelineConfig,
    /// connect 超时，默认 10s。
    pub connect_timeout: Duration,
    /// 发 Hello 后等 Welcome 的上限，默认 45s（T22 U6：`approval_timeout + 15s`，
    /// 避免 host 30s 审批弹窗等待期间 client 先于 host 超时）。
    pub handshake_timeout: Duration,
    /// 输入捕获器（真机 = 平台捕获桥；测试 = 脚本型替身）。
    pub capture: Box<dyn rc_input::InputCapture>,
    /// InputChannel 批窗口，默认 `rc_input::DEFAULT_WINDOW`。
    pub input_window: Duration,
    /// T19/T22：鉴权配置（默认无密码 + `DEFAULT_CLIENT_AUTH_TIMEOUT_US` = 45s）。
    pub auth: ClientAuthConfig,
}

impl ClientSessionConfig {
    /// 以对端地址、请求参数、会话号与输入捕获器构造（其余字段取默认值）。
    pub fn new(
        peer_addr: SocketAddr,
        requested: SessionParams,
        session_id: u64,
        capture: Box<dyn rc_input::InputCapture>,
    ) -> Self {
        Self {
            peer_addr,
            mtu: DEFAULT_MTU,
            requested,
            session_id,
            pipeline: PipelineConfig::default(),
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_micros(crate::auth::DEFAULT_CLIENT_AUTH_TIMEOUT_US),
            capture,
            input_window: rc_input::DEFAULT_WINDOW,
            auth: ClientAuthConfig::default(),
        }
    }
}

/// 控制端会话。
///
/// 状态机与 host 侧同一纪律：严格四态单向 `Init→Connected→Streaming→Closed`
/// （S1），管线推迟到 Welcome 裁定后建。
pub struct ClientSession {
    cfg: ClientSessionConfig,
    /// 本端端点身份的 PEM（rc-net 的 QuicEndpoint 即使 Controller 角色也
    /// 要求一份服务身份；见 `new` 的说明）。
    cert_pem: String,
    key_pem: String,
    /// 固定信任的对端证书 PEM（connect 时经 `Identity::certs_from_pem` 解析）。
    server_cert_pem: String,
    server_name: String,
    transport: Option<Box<dyn Transport>>,
    /// with_parts 注入的后端；握手建管线时消耗。真网络模式用工厂选择。
    backends: Option<(Box<dyn Decoder>, Box<dyn Renderer>)>,
    pipeline: Option<ControllerPipeline>,
    input: rc_input::InputBatcher,
    clock: SessionClock,
    state: SessionState,
    negotiated: Option<SessionParams>,
    bye_sent: bool,
    closed_reason: Option<StopReason>,
    /// S4：Streaming 之前 / 方向错误到达的事件丢弃计数。
    early_events_dropped: u64,
    /// S8：已打过忽略日志的消息标签（每种一次，限流不刷屏）。
    logged_ignore: Vec<&'static str>,
    input_bytes_total: u64,
    /// 修复卡 B-3（I9 可诊断性）：输入捕获器 next_events 的错误累计
    ///（原先 unwrap_or_default 静默吞；持续失败可观测）。
    input_capture_errors_total: u64,
    /// 修复卡 B-4：同批多条 Bye 且类别冲突（Normal vs Refresh）时被
    /// 忽略的条数（先到的生效；对端行为异常可观测）。
    bye_conflicts_dropped: u64,
    idr_requests_total: u64,
    datagrams_pushed_total: usize,
    // ── T18：观测（报告 SessionObservations 的数据源）──
    /// 已发出的关键批序号（下一批的号；与 host 的 InputAck 计数同口径，
    /// 判定用 [`crate::testkit::batch_is_key`]）。
    key_batch_seq_next: u64,
    /// 在途关键批的发送时刻表 `(seq, client 会话钟 µs)`（等 host 回执
    /// 换算单向延迟；容量上限 [`PENDING_ACK_CAP`]）。
    pending_input_acks: std::collections::VecDeque<(u64, u64)>,
    /// 输入单向延迟样本（µs；InputAck 换算：inject_done(host) 经偏移折算
    /// 到 client 域后 − 发送时刻）。
    input_one_way_samples: Vec<u32>,
    /// 重组表深度峰值（铁律 1：恒 ≤ 2；每拍采样 tracker.in_flight()）。
    max_reassembly_depth: u32,
    /// 最近一拍的 NetStats 快照（停机后 transport 丢弃，报告取缓存）。
    net_stats_cache: rc_net::NetStats,
    /// 每帧媒体包头 `(capture_ts, encode_done_ts)`（host 域；报告换算用，
    /// 容量上限 [`FRAME_HEADER_CAP`]，超出淘汰最旧）。
    frame_headers: std::collections::BTreeMap<u64, (u32, u32)>,
    /// 重连恢复计时起点（resume 被接受的时刻；等首个 IDR 帧完整到达）。
    recovery_pending_since_us: Option<u64>,
    /// 重连成功到首个 IDR 帧上屏的毫秒数。
    recovery_ms: Option<u64>,
    /// 上一拍末的 displayed 计数（算每拍增量）。
    last_displayed: u64,
    /// 历次已替换旧 `ControllerPipeline` 累计上屏帧数（H4：跨重建无界累计，
    /// 消除 trace 环形缓冲 4096 上限与管线重建清零失真）。
    prev_pipelines_displayed: u64,
    /// T15：RTT/偏移探测引擎（client 发起，兼 1s 心跳）。
    probe: crate::probe::ProbeEngine,
    /// T15：带宽粗测估计器（每次会话一次，v0 粗 heuristic）。
    bw: crate::probe::BwEstimator,
    /// T15：带宽粗测已发起（重复 start_bw_probe 为空操作）。
    bw_probe_started: bool,
    /// T15：带宽粗测的下一个探测号（独立于 ProbePing 的 id 空间）。
    bw_probe_next_id: u32,
    /// T15：粗测估速（None = 未发起 / 退化 / 未结算）。
    bw_estimate_bps: Option<u64>,
    /// T15：探测后实际下发的初始码率（None = 保持协商值）。
    initial_bitrate_bps: Option<u32>,
    /// T15（B4）：首字节既非媒体 MAGIC 也非探测块的数据报丢弃计数。
    unknown_datagrams_dropped: u64,
    /// T16：完整帧到达跟踪（看门狗与 IdrAck 关联确认的数据源）。
    tracker: DeliveryTracker,
    /// T16：停滞看门狗（>500ms 无完整帧触发恢复）。
    watchdog: StallWatchdog,
    /// T16：IdrRequest 去抖 / IdrAck 关联确认 / 超时复核。
    idr: IdrController,
    // ── T17：断线重连（Reconnecting / 指数退避 / 强制 IDR / 饱和刷新重建）──
    /// 重连要复用的端点（真网络构造时留存，坑 7：endpoint 被 drop 则
    /// 连接上下文全失；`with_parts` 模式为 None，测试经 connect 工厂注入）。
    retained_endpoint: Option<QuicEndpoint>,
    /// 重连配置（驱动开始前经 [`ClientSession::set_reconnect`] 可换）。
    reconnect: ReconnectConfig,
    /// 指数退避（纯函数 + 计数，不持时钟）。
    backoff: Backoff,
    /// 下一次重连尝试时刻（微秒，注入时钟口径；到点判定全凭参数）。
    next_attempt_at_us: u64,
    /// 断线/重连统计。
    rstats: ReconnectStats,
    /// 管线代数：每建一次管线 +1（「管线不拆」的可观测判据）。
    pipeline_generation: u32,
    /// 当前生效会话号（初始 = cfg.session_id；全新重建时自增换新）。
    session_id_current: u64,
    /// 管线重建工厂（resume 被拒 / Bye(Refresh) 时调用）；None = 与首次
    /// 握手同一构造路径（select_* auto 后端）。
    pipeline_factory: Option<Box<dyn FnMut() -> Result<ControllerPipeline> + Send>>,
    /// T18：带裁定参数的管线工厂（CLI 用；优先于 `pipeline_factory`）。
    pipeline_factory_for: Option<ControllerFactoryFor>,
    /// connect 工厂（测试接缝：MockTransport 模式重连的连接来源；None =
    /// 真网络走 retained_endpoint.connect）。
    connect_factory: Option<ConnectFactory>,
    /// Reconnecting 期的子阶段（连接发起前/裁定前）。
    reconnect_phase: ReconnectPhase,
    /// 饱和刷新重建进行中（下一个 Welcome 必须带 host_flags bit1=1）。
    refresh_pending: bool,
    /// T19：握手期鉴权子态（Init/Connected ⇄ 终局 Welcome/超时；A6：未终局前零启动管线）。
    auth_phase: AuthPhase,
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
}

/// 握手期鉴权子态（私有状态机，不改动公开的 [`SessionState`]）。
enum AuthPhase {
    /// 非鉴权中（Init / Streaming / Closed / 退避等待）。
    Idle,
    /// 已发 Hello，鉴权进行中（等终局 Welcome 或 AuthChallenge）。
    Authenticating(ClientAuthenticator),
}

/// 单步握手消息处理结果。
enum ClientHandshakeStep {
    /// 鉴权进行中（已收到过程 Welcome 或已发出 AuthResponse，继续等待终局）。
    Authenticating,
    /// 终局成功（收到 `Welcome{Ok}`，携带裁定参数）。
    Done(SessionParams),
}

/// connect 工厂类型（[`ClientSession::set_connect_factory`] 的注入件）。
type ConnectFactory = Box<dyn FnMut() -> Result<Box<dyn Transport>> + Send>;

/// T18：带裁定参数的管线工厂类型（CLI 组合根注入；[`
/// ClientSession::set_pipeline_factory_for`]）。
pub type ControllerFactoryFor =
    Box<dyn FnMut(&PipelineConfig) -> Result<ControllerPipeline> + Send>;

/// Reconnecting 期的子阶段（私有状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectPhase {
    /// 无重连在途（非 Reconnecting 态的占位）。
    Idle,
    /// 断线后等下一尝试点；到点 connect 并发 ReconnectRequest{旧 sid,
    /// resume=1}（正常断线路径）。
    ResumePending,
    /// 全新重建等下一尝试点（Bye(Refresh) / resume 被拒已裁定）；到点
    /// connect 并发全新 Hello（新 sid）。
    FreshPending,
    /// ReconnectRequest 已发，等 Welcome 裁定（bit0）。
    AwaitingVerdict,
    /// 全新 Hello 已发，等 Welcome（刷新路径要求 bit1=1）。
    AwaitingFresh,
}

impl ClientSession {
    /// 真网络构造（Init）。
    ///
    /// `server_cert_pem` 为固定信任的对端证书（内部经
    /// `Identity::certs_from_pem`，不直接依赖 rustls）；`server_name` 须与
    /// 证书 SAN 匹配。
    ///
    /// # 与任务卡签名的出入
    ///
    /// rc-net 的 `QuicEndpoint::new` 对**任何角色**都要求一份服务身份
    /// （证书 + 私钥，用于端点的 server_config）——控制端端点也不例外
    /// （rc-net 自己的回环测试两端传同一 identity）。因此本构造比任务卡
    /// 的 `new(cfg, server_cert_pem, server_name, clock)` 多出
    /// `cert_pem`/`key_pem` 两个参数：本端自签身份。对端不做客户端证书
    /// 校验（`with_no_client_auth`），这份身份只服务于端点构造。
    pub fn new(
        cfg: ClientSessionConfig,
        cert_pem: &str,
        key_pem: &str,
        server_cert_pem: &str,
        server_name: &str,
        clock: SessionClock,
    ) -> Result<Self> {
        // 提前校验三份 PEM，把配置错误拦在构造期（对齐 rc-pipeline 的纪律）。
        rc_net::quic::Identity::from_pem(cert_pem, key_pem)?;
        rc_net::quic::Identity::certs_from_pem(server_cert_pem)?;
        // T18：直接持 InputBatcher（而非 InputChannel）——关键批的发送
        // 记账（seq / 发送时刻）需要看到批内容，pump_sender 只返回字节数。
        let input = rc_input::InputBatcher::new(cfg.input_window);
        let sid = cfg.session_id;
        Ok(Self {
            cfg,
            cert_pem: cert_pem.into(),
            key_pem: key_pem.into(),
            server_cert_pem: server_cert_pem.into(),
            server_name: server_name.into(),
            transport: None,
            backends: None,
            pipeline: None,
            input,
            clock,
            state: SessionState::Init,
            negotiated: None,
            bye_sent: false,
            closed_reason: None,
            early_events_dropped: 0,
            logged_ignore: Vec::new(),
            input_bytes_total: 0,
            input_capture_errors_total: 0,
            bye_conflicts_dropped: 0,
            idr_requests_total: 0,
            key_batch_seq_next: 0,
            pending_input_acks: std::collections::VecDeque::new(),
            input_one_way_samples: Vec::new(),
            max_reassembly_depth: 0,
            net_stats_cache: rc_net::NetStats::default(),
            frame_headers: std::collections::BTreeMap::new(),
            recovery_pending_since_us: None,
            recovery_ms: None,
            datagrams_pushed_total: 0,
            last_displayed: 0,
            prev_pipelines_displayed: 0,
            probe: crate::probe::ProbeEngine::new(),
            bw: crate::probe::BwEstimator::new(),
            bw_probe_started: false,
            bw_probe_next_id: 1,
            bw_estimate_bps: None,
            initial_bitrate_bps: None,
            unknown_datagrams_dropped: 0,
            tracker: DeliveryTracker::new(),
            watchdog: StallWatchdog::new(DEFAULT_STALL_US),
            idr: IdrController::new(IdrControllerConfig::default()),
            retained_endpoint: None,
            reconnect: ReconnectConfig::default(),
            backoff: Backoff::new(
                crate::reconnect::DEFAULT_BACKOFF_INITIAL,
                crate::reconnect::DEFAULT_BACKOFF_MAX,
                crate::reconnect::DEFAULT_BACKOFF_MAX_ATTEMPTS,
            )
            .expect("默认退避参数合法（构造期常量）"),
            next_attempt_at_us: 0,
            rstats: ReconnectStats::default(),
            pipeline_generation: 0,
            session_id_current: sid,
            pipeline_factory: None,
            pipeline_factory_for: None,
            connect_factory: None,
            reconnect_phase: ReconnectPhase::Idle,
            refresh_pending: false,
            auth_phase: AuthPhase::Idle,
            clipboard: None,
            clipboard_received_while_disabled: 0,
            tool_rx: VecDeque::new(),
            audio_ctrl_rx: None,
            tool_rx_overflow_dropped: 0,
            audio_rx: VecDeque::new(),
        })
    }

    /// 测试注入（Connected，等握手）。
    pub fn with_parts(
        cfg: ClientSessionConfig,
        transport: Box<dyn Transport>,
        decoder: Box<dyn Decoder>,
        renderer: Box<dyn Renderer>,
        clock: SessionClock,
    ) -> Result<Self> {
        // T18：直接持 InputBatcher（而非 InputChannel）——关键批的发送
        // 记账（seq / 发送时刻）需要看到批内容，pump_sender 只返回字节数。
        let input = rc_input::InputBatcher::new(cfg.input_window);
        let sid = cfg.session_id;
        Ok(Self {
            cfg,
            cert_pem: String::new(),
            key_pem: String::new(),
            server_cert_pem: String::new(),
            server_name: String::new(),
            transport: Some(transport),
            backends: Some((decoder, renderer)),
            pipeline: None,
            input,
            clock,
            state: SessionState::Connected,
            negotiated: None,
            bye_sent: false,
            closed_reason: None,
            early_events_dropped: 0,
            logged_ignore: Vec::new(),
            input_bytes_total: 0,
            input_capture_errors_total: 0,
            bye_conflicts_dropped: 0,
            idr_requests_total: 0,
            key_batch_seq_next: 0,
            pending_input_acks: std::collections::VecDeque::new(),
            input_one_way_samples: Vec::new(),
            max_reassembly_depth: 0,
            net_stats_cache: rc_net::NetStats::default(),
            frame_headers: std::collections::BTreeMap::new(),
            recovery_pending_since_us: None,
            recovery_ms: None,
            datagrams_pushed_total: 0,
            last_displayed: 0,
            prev_pipelines_displayed: 0,
            probe: crate::probe::ProbeEngine::new(),
            bw: crate::probe::BwEstimator::new(),
            bw_probe_started: false,
            bw_probe_next_id: 1,
            bw_estimate_bps: None,
            initial_bitrate_bps: None,
            unknown_datagrams_dropped: 0,
            tracker: DeliveryTracker::new(),
            watchdog: StallWatchdog::new(DEFAULT_STALL_US),
            idr: IdrController::new(IdrControllerConfig::default()),
            retained_endpoint: None,
            reconnect: ReconnectConfig::default(),
            backoff: Backoff::new(
                crate::reconnect::DEFAULT_BACKOFF_INITIAL,
                crate::reconnect::DEFAULT_BACKOFF_MAX,
                crate::reconnect::DEFAULT_BACKOFF_MAX_ATTEMPTS,
            )
            .expect("默认退避参数合法（构造期常量）"),
            next_attempt_at_us: 0,
            rstats: ReconnectStats::default(),
            pipeline_generation: 0,
            session_id_current: sid,
            pipeline_factory: None,
            pipeline_factory_for: None,
            connect_factory: None,
            reconnect_phase: ReconnectPhase::Idle,
            refresh_pending: false,
            auth_phase: AuthPhase::Idle,
            clipboard: None,
            clipboard_received_while_disabled: 0,
            tool_rx: VecDeque::new(),
            audio_ctrl_rx: None,
            tool_rx_overflow_dropped: 0,
            audio_rx: VecDeque::new(),
        })
    }

    /// connect + Hello/Welcome（Init→Connected→Streaming），返回裁定参数。
    ///
    /// T17：真网络构造的端点**留存复用**（坑 7：重连需要同一
    /// QuicEndpoint 的连接上下文；T14 实现把它当局部变量丢掉，是本卡的
    /// 最小内部修正）。注入了 connect 工厂（测试接缝）则连接来自工厂。
    pub fn connect_and_handshake(&mut self) -> Result<SessionParams> {
        match self.state {
            SessionState::Init => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "connect_and_handshake requires Init state, got {other:?}"
                )))
            }
        }
        let transport = self.open_connection()?;
        self.transport = Some(transport);
        self.state = SessionState::Connected;
        self.handshake()
    }

    /// 建立一条到对端的连接（connect_and_handshake 与重连共用）：
    /// connect 工厂优先（测试接缝）；否则经留存的端点 connect（首次
    /// 调用时惰性建端点并留存）。
    fn open_connection(&mut self) -> Result<Box<dyn Transport>> {
        if let Some(f) = self.connect_factory.as_mut() {
            return f();
        }
        if self.retained_endpoint.is_none() {
            let identity = rc_net::quic::Identity::from_pem(&self.cert_pem, &self.key_pem)?;
            let net = NetConfig {
                role: Role::Controller,
                local_addr: "0.0.0.0:0"
                    .parse()
                    .expect("0.0.0.0:0 is a valid socket addr"),
                peer_addr: Some(self.cfg.peer_addr),
                mtu: self.cfg.mtu,
            };
            self.retained_endpoint = Some(QuicEndpoint::new(&net, &identity)?);
        }
        let endpoint = self.retained_endpoint.as_ref().expect("上面刚建/已留存");
        let certs = rc_net::quic::Identity::certs_from_pem(&self.server_cert_pem)?;
        let transport = endpoint.connect(
            self.cfg.peer_addr,
            &certs,
            &self.server_name,
            self.cfg.connect_timeout,
        )?;
        Ok(Box::new(transport))
    }

    fn drain_post_welcome_events(&mut self, ev_iter: impl Iterator<Item = NetEvent>) -> Result<()> {
        for trailing in ev_iter {
            match trailing {
                NetEvent::Reliable(extra) => match self.dispatch_control(&extra) {
                    Ok(ControlFlow::Handled) => {}
                    Ok(ControlFlow::Bye(reason)) => {
                        self.stop_with(StopReason::ByeReceived(reason), None);
                        return Err(Error::Network(format!(
                            "peer closed right after Welcome: {reason:?}"
                        )));
                    }
                    Err(e) => {
                        self.stop_with(StopReason::Disconnected(e.to_string()), None);
                        return Err(e);
                    }
                },
                NetEvent::Disconnected { reason } => {
                    self.stop_with(StopReason::Disconnected(reason.clone()), None);
                    return Err(Error::Network(reason));
                }
                NetEvent::PathChanged => {
                    self.rstats.path_changes += 1;
                }
                NetEvent::Connected => {}
                NetEvent::Datagram(_) | NetEvent::ReliableUrgent(_) => {
                    self.early_events_dropped += 1;
                }
            }
        }
        Ok(())
    }

    /// 已连通 transport 上的握手步骤：发 Hello（鉴权位来自 [`ClientAuthenticator`]，
    /// A1：密码绝不进 Hello）→ 进内部 `Authenticating` 子态 → poll 等 `Welcome` /
    /// `AuthChallenge`（A5/A6/A7）→ 终局 `Welcome{Ok}` 后才以裁定参数建管线
    /// （`ControllerPipeline::with_backends`）。
    pub fn handshake(&mut self) -> Result<SessionParams> {
        match self.state {
            SessionState::Connected => {}
            SessionState::Init => {
                return Err(Error::InvalidConfig(
                    "handshake before transport established (call connect_and_handshake first)"
                        .into(),
                ))
            }
            other => {
                return Err(Error::InvalidConfig(format!(
                    "handshake requires Connected state, got {other:?}"
                )))
            }
        }
        // T19（A1/A2）：构造全新 ClientAuthenticator，鉴权位来自 hello_auth()（密码绝不进 Hello）。
        let mut auth =
            ClientAuthenticator::with_session_id(self.cfg.auth.clone(), self.session_id_current);
        let (auth_method, auth_payload) = auth.hello_auth();
        // 发 Hello（S3：请求参数原样上送，裁定以 Welcome 回值为准）。
        let hello = ControlMessage::Hello(Hello {
            version: proto::PROTOCOL_VERSION,
            session_id: self.session_id_current,
            requested: self.cfg.requested,
            auth_method,
            auth_payload,
        });
        let mut buf = Vec::new();
        hello.encode(&mut buf);
        if let Err(e) = self
            .transport
            .as_mut()
            .expect("Connected implies transport")
            .send_reliable(&buf)
        {
            return Err(self.handshake_fail(e));
        }
        auth.on_hello_sent(u64::from((self.clock)()));
        self.auth_phase = AuthPhase::Authenticating(auth);

        let slice = self.cfg.handshake_timeout.min(HANDSHAKE_POLL_SLICE);
        let slice_ms = slice.as_millis().max(1) as u64;
        let total_ms = self.cfg.handshake_timeout.as_millis() as u64;
        let mut rounds_left = total_ms.div_ceil(slice_ms).max(1);
        let auth_budget_ms = self
            .cfg
            .handshake_timeout
            .as_millis()
            .max(Duration::from_micros(self.cfg.auth.auth_timeout_us).as_millis())
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
            let mut ev_iter = events.into_iter();
            while let Some(ev) = ev_iter.next() {
                match ev {
                    NetEvent::Reliable(msg) => {
                        match self.handle_handshake_reliable(&msg) {
                            Ok(ClientHandshakeStep::Done(granted)) => {
                                self.auth_phase = AuthPhase::Idle;
                                if let Err(e) = self.finalize_handshake(granted) {
                                    return Err(self.handshake_fail(e));
                                }
                                // 同批中 Welcome 之后紧随到达的控制帧（例如初始 DisplayConfig ToolChannel）
                                // 必须立即交付到 Streaming 控制分发，防止被提前 return 丢弃。
                                self.drain_post_welcome_events(ev_iter)?;
                                return Ok(granted);
                            }
                            Ok(ClientHandshakeStep::Authenticating) => {
                                // 进入挑战/等待人工确认窗口：放宽轮询预算以覆盖 auth_timeout_us。
                                rounds_left = rounds_left.max(auth_budget_rounds);
                            }
                            Err(e) => return Err(self.handshake_fail(e)),
                        }
                    }
                    // S4：Streaming 之前到达的媒体/输入事件丢弃并计数，不缓存。
                    NetEvent::Datagram(_) | NetEvent::ReliableUrgent(_) => {
                        self.early_events_dropped += 1;
                    }
                    NetEvent::Connected | NetEvent::PathChanged => {}
                    NetEvent::Disconnected { reason } => {
                        self.auth_phase = AuthPhase::Idle;
                        self.state = SessionState::Closed;
                        self.closed_reason = Some(StopReason::Disconnected(reason.clone()));
                        self.transport = None;
                        return Err(Error::Network(format!(
                            "disconnected while waiting for Welcome: {reason}"
                        )));
                    }
                }
            }
            // A7：每拍复核注入时钟口径的鉴权超时窗口。
            let now_us = u64::from((self.clock)());
            if let AuthPhase::Authenticating(ref mut auth) = self.auth_phase {
                if auth.poll_timeout(now_us) {
                    let timeout = Duration::from_micros(self.cfg.auth.auth_timeout_us);
                    eprintln!(
                        "[rc-session/auth] client authentication timed out after {timeout:?}"
                    );
                    return Err(self.handshake_fail(Error::Timeout(timeout)));
                }
            }
            if rounds_left <= 1 {
                return Err(self.handshake_fail(Error::Timeout(self.cfg.handshake_timeout)));
            }
            rounds_left -= 1;
        }
    }

    /// 当前状态。
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// 本端会话号（初始 = 构造时给定；T17 全新重建后为自增的新号）。
    pub fn session_id(&self) -> u64 {
        self.session_id_current
    }

    /// 握手裁定的流参数（Streaming 前 None）。
    pub fn negotiated(&self) -> Option<SessionParams> {
        self.negotiated
    }

    /// 一拍：
    /// ① `input.pump_sender(capture, transport, now, ZERO)`（输入走 urgent 流）；
    /// ② `poll(poll_budget)` 分发——`Datagram`→`tracker.observe` **再**
    /// `pipeline.push_datagram`（含重组+解码+呈现；凑齐完整帧喂看门狗与
    /// IdrController，T16）；`Reliable`→控制分发（Welcome/Bye/IdrAck/
    /// ProbePong 各归其臂）；`Disconnected`→内联停机；
    /// ③ 拍末 T16 接线：`take_keyframe_request`/看门狗触发 →
    /// `IdrController`（去抖），`idr.poll` 是唯一发送出口，最后 `tracker.gc`。
    ///
    /// `now` 供批窗口判断（`Instant`），须与会话钟同轴（坑 5）。
    ///
    /// # 错误语义
    ///
    /// 与 host 侧同一纪律：脏控制消息 / 媒体数据报协议错误先执行完整
    /// 停机序列再返回 Err（S8）。
    pub fn tick(&mut self, poll_budget: Duration, now: Instant) -> Result<ClientTickReport> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "client tick requires Streaming state, got {other:?} (S1)"
                )))
            }
        }
        let mut report = ClientTickReport::default();

        // ① 输入泵（urgent 流；超时 0：事件到 capture 缓冲即刻取走）。
        // T17：发送失败（连接将死）不当错误——尽力而为；权威的断线/
        // Bye(Refresh) 信号由紧随的 poll 给出，届时走重连而不是停机。
        // T18：pump_sender 展开为内联（capture → batcher → send_events），
        // 以便对**关键批**记账（seq 与发送时刻）——host 的 InputAck 按
        // 同一口径（testkit::batch_is_key）计数，两侧对齐。
        let sent = {
            let capture = self.cfg.capture.as_mut();
            let transport = &mut **self
                .transport
                .as_mut()
                .expect("Streaming implies transport");
            let mut sent = 0usize;
            // 修复卡 B-3（I9 可诊断性）：捕获器错误不再 unwrap_or_default
            // 静默吞——计数 + 首次限流 stderr（log_ignore_once 同款每种一次
            // 的机制，此处内联以带上错误详情），持续失败可诊断；错误不中断
            // tick（捕获故障不等价于会话死亡，与输入发送失败同一尽力而为
            // 语义）。
            match capture.next_events(Duration::ZERO) {
                Ok(events) => {
                    for ev in events {
                        self.input.push(ev, now);
                    }
                }
                Err(e) => {
                    self.input_capture_errors_total += 1;
                    const TAG: &str = "input capture error";
                    if !self.logged_ignore.contains(&TAG) {
                        self.logged_ignore.push(TAG);
                        eprintln!("rc-session client: 输入捕获器错误（此后只计数不刷屏）: {e}");
                    }
                }
            }
            let batch = self.input.poll(now);
            if !batch.is_empty() {
                // 连接将死的发送失败：尽力而为（T17 同纪律），批记账跳过。
                // 返回值口径与 pump_sender 一致：发出的全部消息字节数。
                if let Ok(n) = rc_input::channel::send_events(transport, &batch) {
                    sent = n;
                    if crate::testkit::batch_is_key(&batch) {
                        self.key_batch_seq_next += 1;
                        self.pending_input_acks
                            .push_back((self.key_batch_seq_next, u64::from((self.clock)())));
                        while self.pending_input_acks.len() > PENDING_ACK_CAP {
                            self.pending_input_acks.pop_front();
                        }
                    }
                }
            }
            sent
        };
        report.input_bytes_sent = sent;
        self.input_bytes_total += u64::try_from(sent).unwrap_or(u64::MAX);
        report.busy |= sent > 0;

        // ② poll 分发。修复卡 B-1：poll 错误不再 `?` 裸上抛（会卡死在
        // Streaming、绕过 S6/S8「先完整停机再返回 Err」的纪律）——置脏
        // 跳出分发，尾部 stop_with 完整停机序列后才返回 Err。
        let mut bye: Option<ByeReason> = None;
        let mut disconnected = false;
        let mut first_err: Option<Error> = None;
        let mut dirty = false;
        let events = match self
            .transport
            .as_mut()
            .expect("Streaming implies transport")
            .poll(poll_budget)
        {
            Ok(events) => events,
            Err(e) => {
                dirty = true;
                first_err = Some(e);
                Vec::new()
            }
        };

        for ev in events {
            match ev {
                NetEvent::Connected => {}
                // T17（R5）：路径迁移不算断线——不迁移状态、不断流，只计数。
                NetEvent::PathChanged => {
                    self.rstats.path_changes += 1;
                }
                NetEvent::ReliableUrgent(_) => {
                    // client 不收输入（方向错误）：忽略计数（S4）。
                    self.early_events_dropped += 1;
                }
                NetEvent::Datagram(data) => {
                    // T15（B4 分流）：媒体（rc-proto MAGIC 0x31564352 的 LE
                    // 首字节 0x52）→ 管线；探测块（首字节 0x09）→ 带宽
                    // 估计器；其余记错丢弃（完整 MAGIC 校验仍由
                    // push_datagram 的 MediaHeader::parse_datagram 把关，
                    // 探测块绝不进 push_datagram——坑 3）。
                    if data.first() == Some(&proto::TYPE_BW_PROBE_DATA) {
                        // 修复卡 B-1：探测块的线格式错误（对端可触发——
                        // 1 字节 0x09 数据报即触发 parse 失败）不再 `?` 裸
                        // 上抛：与脏媒体数据报同一纪律（S8 宁拒收），置脏
                        // 跳出，尾部完整停机序列后才返回 Err。
                        if let Err(e) = self.on_bw_datagram(&data, u64::from((self.clock)())) {
                            dirty = true;
                            first_err.get_or_insert(e);
                            break;
                        }
                        report.busy = true;
                    } else if data.first() == Some(&MEDIA_DATAGRAM_FIRST_BYTE) {
                        let now = (self.clock)();
                        // T18：记录包头时间戳（host 域 capture/encode_done）——
                        // 报告侧持包头值自行做偏移折算，不拿 trace 的 Captured
                        // 标记直接减 Presented（坑 1；trace 的 Captured 写入的
                        // 也是 host 域值，两者同源，但换算必须显式走偏移）。
                        if let Ok(h) = rc_pipeline::host::datagram_header(&data) {
                            self.frame_headers
                                .insert(h.frame_seq.0, (h.capture_ts.0, h.encode_done_ts.0));
                            while self.frame_headers.len() > FRAME_HEADER_CAP {
                                let oldest = *self.frame_headers.keys().next().expect("非空");
                                self.frame_headers.remove(&oldest);
                            }
                        }
                        // T16：先观察（tracker 顺带解析头部补可见性，管线 API
                        // 零改动）再推管线；观察失败与 push_datagram 的头部
                        // 非法同源，同一停机纪律（S8 宁拒收）。
                        let before = self.tracker.last_complete().map(|f| (f.seq, f.at_us));
                        if let Err(e) = self.tracker.observe(&data, now) {
                            dirty = true;
                            first_err.get_or_insert(e);
                            break;
                        }
                        match self
                            .pipeline
                            .as_mut()
                            .expect("Streaming implies pipeline")
                            .push_datagram(&data, now)
                        {
                            Ok(_) => {
                                report.datagrams_pushed += 1;
                                report.busy = true;
                                // T16：凑齐完整帧 → 喂看门狗与 IdrController
                                // （ack 关联确认，两通道无序的兜底数据源）。
                                let after = self.tracker.last_complete();
                                if after.map(|f| (f.seq, f.at_us)) != before {
                                    let frame = after.expect("changed implies some");
                                    self.watchdog.on_frame_complete(now);
                                    self.idr.on_frame_complete(frame);
                                }
                            }
                            Err(e) => {
                                // 脏媒体数据报：与脏控制消息同一纪律（S8 宁拒收）。
                                dirty = true;
                                first_err.get_or_insert(e);
                                break;
                            }
                        }
                    } else {
                        self.unknown_datagrams_dropped += 1;
                        self.log_ignore_once("unknown datagram（首字节既非 MAGIC 也非探测块）");
                    }
                }
                NetEvent::Reliable(msg) => match self.dispatch_control(&msg) {
                    Ok(ControlFlow::Handled) => {
                        report.busy = true;
                    }
                    Ok(ControlFlow::Bye(reason)) => {
                        // 修复卡 B-4：同批多条 Bye 只认最先到达的那条
                        //（先处理的生效）；后到的**另一类** Bye（Normal 与
                        // Refresh 的处置不同：停机 vs 全新重建）忽略并计数
                        // ——对端行为异常可观测，不再出现「干净停机后又
                        // 返回 InvalidConfig」。同类重复 Bye 幂等（首条已
                        // 记录处置，后续为空操作）。
                        let incoming_refresh = reason == ByeReason::Refresh;
                        match bye {
                            None => bye = Some(reason),
                            Some(prev) => {
                                if (prev == ByeReason::Refresh) != incoming_refresh {
                                    self.bye_conflicts_dropped += 1;
                                    self.log_ignore_once("Bye(同批类别冲突，先到的生效)");
                                }
                            }
                        }
                        report.busy = true;
                    }
                    Err(e) => {
                        dirty = true;
                        first_err.get_or_insert(e);
                        break;
                    }
                },
                // T17（R3）：Disconnected 不再等价于会话结束——保留管线/
                // session_id/端点，进 Reconnecting 按退避自动重连。
                NetEvent::Disconnected { .. } => {
                    disconnected = true;
                    report.busy = true;
                }
            }
        }

        // T18：重组表深度峰值采样（gc 之前的瞬时在途数才是真实峰值）与
        // NetStats 快照（停机后 transport 丢弃，报告读缓存）。
        self.max_reassembly_depth = self
            .max_reassembly_depth
            .max(self.tracker.in_flight() as u32);
        if let Some(t) = self.transport.as_ref() {
            self.net_stats_cache = t.stats();
        }

        // ③ 拍末（T16 接线，顺序按任务卡 W 节）：解码器/看门狗触发 →
        // IdrController（去抖状态机；poll 是唯一发送出口），再超时复核、
        // 发送与残帧 gc。
        if !dirty && bye.is_none() && !disconnected {
            let now = (self.clock)();
            if self
                .pipeline
                .as_mut()
                .expect("Streaming implies pipeline")
                .take_keyframe_request()
            {
                self.idr.request(IdrReason::DecoderNeedKeyframe, now);
            }
            if self.watchdog.poll(now) {
                self.idr.request(IdrReason::WatchdogStall, now);
            }
            // rtt 注入自 NetStats（握手初期可能为 0 → 超时退化为纯 margin，
            // localhost/内网足够；不依赖 T15 的 ProbePing）。
            let rtt_us = self
                .transport
                .as_ref()
                .expect("Streaming implies transport")
                .stats()
                .rtt_us;
            if let Some(req) = self.idr.poll(now, rtt_us) {
                // T17：尽力而为——连接将死时的发送失败由 poll 的断线信号
                // 接管（重连成功后去抖已清，恢复路径会重新请求）。
                if self.send_idr_request(req.reason).is_ok() {
                    report.idr_requests_sent = 1;
                    report.busy = true;
                }
            }
            self.tracker.gc(now);
            // T18：重连恢复计时——resume 被接受后等首个 IDR 帧完整到达，
            // 差值即 recovery_ms（注入时钟口径，铁律 4）。since 守卫排除
            // 断线前就在 tracker 历史里的旧 IDR。不能只看 last_complete
            // （门禁第 2 轮全仓测试偶发失败根因）：负载下一拍可能批量凑齐
            // 多帧，强制的 IDR 被同拍更晚的 delta 覆盖，恢复计时会竞态性
            // 永远等不到——须扫完成历史（first_idr_since），时刻差一律
            // wrapping（W7）。
            if let Some(since) = self.recovery_pending_since_us {
                let since = u32::try_from(since).unwrap_or(u32::MAX);
                if let Some(f) = self.tracker.first_idr_since(since) {
                    self.recovery_ms = Some(u64::from(f.at_us.wrapping_sub(since)) / 1_000);
                    self.recovery_pending_since_us = None;
                }
            }
            // T20 剪贴板泵（仅 Streaming 态；未启用时空操作、零开销）。
            if self.clipboard.is_some() {
                self.pump_clipboard(u64::from(now))?;
            }
        }

        // 上屏增量（本拍新上屏帧数）。
        let displayed = self
            .pipeline
            .as_ref()
            .expect("Streaming implies pipeline")
            .displayed();
        report.frames_displayed_delta = displayed - self.last_displayed;
        self.last_displayed = displayed;
        report.busy |= report.frames_displayed_delta > 0;
        self.datagrams_pushed_total += report.datagrams_pushed;

        // 停机/重连触发（内联完成）。修复卡 B-4：bye 单变量承载两类处置
        //——先到的那条生效（分发臂已把后到的冲突条目忽略计数），不会再
        // 出现「Normal 先干净停机、Refresh 又对 Closed 态返回 InvalidConfig」。
        if let Some(reason) = bye {
            if reason == ByeReason::Refresh {
                // T17（R6）：Bye(Refresh) = host 时钟饱和刷新——零退避
                // 全新重建（on_bye_refresh），不是会话结束。到达此处时
                // 状态必为 Streaming（事件循环只记标志不停机）。
                self.on_bye_refresh(u64::from((self.clock)()))?;
                debug_assert_eq!(self.state, SessionState::Reconnecting);
                report.busy = true;
            } else {
                let stop = StopReason::ByeReceived(reason);
                self.stop_with(stop.clone(), None);
                report.closed = Some(stop);
            }
        }
        // T17：Disconnected → 保留管线自动重连（会话不结束）。
        if disconnected {
            self.on_disconnected(u64::from((self.clock)()));
            // 对端「先 Bye 后关连接」竞态（与 host 侧 tick 同型，
            // host_session.rs 已注明实测竞态）：同一 poll 批里 Bye(Normal)
            // 与 Disconnected 一起到达——Bye 先走 stop_with 进 Closed，
            // on_disconnected 对非 Streaming 幂等返回，此时 Closed 是
            // 合法终态（断言只认 Reconnecting 会把正常优雅关闭在 debug
            // 构建打成 panic；release 行为本就正确）。
            debug_assert!(matches!(
                self.state,
                SessionState::Reconnecting | SessionState::Closed
            ));
            report.busy = true;
        }
        if dirty {
            let err = first_err.expect("dirty implies first_err");
            self.stop_with(
                StopReason::Disconnected(format!("dirty message: {err}")),
                None,
            );
            return Err(err);
        }
        Ok(report)
    }

    /// 手动请求关键帧（T16：经 [`IdrController`] 去抖——`request` 只记意图，
    /// 实际发送在下一拍 tick 的唯一发送出口，在途期间新触发被吞）。
    pub fn request_idr(&mut self, reason: IdrReason) -> Result<()> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "request_idr requires Streaming state, got {other:?}"
                )))
            }
        }
        self.idr.request(reason, (self.clock)());
        Ok(())
    }

    /// 主动优雅关闭（S6 client 侧停机顺序见模块文档）。
    ///
    /// T17：Reconnecting 态也可关（上层决定放弃重连）——按红线拆除保留
    /// 中的管线后进入 Closed。
    pub fn close(&mut self, reason: ByeReason) -> Result<ClientRunSummary> {
        match self.state {
            SessionState::Closed => {
                let stop = self.closed_reason.clone().unwrap_or(StopReason::Requested);
                return Ok(self.summary(stop));
            }
            SessionState::Streaming | SessionState::Connected | SessionState::Reconnecting => {}
            SessionState::Init | SessionState::Detached => {
                return Err(Error::InvalidConfig(format!(
                    "close requires an established session, got {:?}",
                    self.state
                )))
            }
        }
        self.stop_with(StopReason::Requested, Some(reason));
        Ok(self.summary(StopReason::Requested))
    }

    /// 驱动循环：跑到 `stop()` 为真、收到 Bye 或断连。退出前若未发过
    /// Bye 则补发 Bye(Normal) 并执行停机序列。
    ///
    /// client 无 `wait_event`：空闲拍靠下一拍 poll 的（放宽）预算承担
    /// 等待（N4）。`Instant::now()` 是驱动器的合法时钟源（坑 5，模块文档）。
    ///
    /// T15：循环每拍驱动探测泵（ProbePing 心跳 + Bw 超时结算），进入循环
    /// 时发起一次带宽粗测（Welcome 之后、仅一次；协商值已先生效，探测是
    /// 覆盖）。探测钟 = 会话钟闭包 u32 零扩展（B9 同源同纪元）；**驱动循环
    /// 之外的手工循环（T18 CLI 等）必须照此每拍调用 [`Self::pump_probes`]**。
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<ClientRunSummary> {
        let mut last_busy = true;
        while !stop() && self.state != SessionState::Closed {
            match self.state {
                SessionState::Streaming => {
                    let now_us = u64::from((self.clock)());
                    self.pump_probes(now_us)?;
                    self.start_bw_probe(now_us)?;
                    let budget = if last_busy {
                        RUN_POLL_BUDGET
                    } else {
                        RUN_IDLE_POLL_BUDGET
                    };
                    let r = self.tick(budget, Instant::now())?;
                    if r.closed.is_some() {
                        break;
                    }
                    last_busy = r.busy;
                }
                // T17：重连驱动拍（时间注入；退避等待靠短睡眠防自旋——
                // 驱动器的合法时钟源，坑 5 同源豁免）。
                SessionState::Reconnecting => {
                    let outcome = self.tick_reconnect(u64::from((self.clock)()))?;
                    if let ReconnectOutcome::Waiting { next_attempt_in_ms } = outcome {
                        let nap =
                            Duration::from_millis(next_attempt_in_ms).min(RUN_RECONNECT_SLEEP);
                        std::thread::sleep(nap);
                    }
                }
                SessionState::Init
                | SessionState::Connected
                | SessionState::Detached
                | SessionState::Closed => {
                    break; // 非驱动态（client 不进 Detached）：交给调用方
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
    // T15：探测与初始码率（纯增量于 T14 之上）
    // ─────────────────────────────────────────────────────────────

    /// 探测泵（驱动循环每拍调用，now_us = client 会话钟 u64）：
    /// [`crate::probe::ProbeEngine::pump`] 到点发 ProbePing（reliable 控制流，
    /// 兼 1s 心跳，B1）+ [`crate::probe::BwEstimator::pump`] 超时结算；结算
    /// 出估速后按 `clamp(估速×0.7, 2Mbps, Welcome 上限)` 自动发
    /// `SetBitrate { reason: Probe }`（B6）。
    pub fn pump_probes(&mut self, now_us: u64) -> Result<()> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "pump_probes requires Streaming state, got {other:?} (S1)"
                )))
            }
        }
        if let Some(ping) = self.probe.pump(now_us) {
            let mut buf = Vec::new();
            ping.encode(&mut buf);
            // T17：心跳尽力而为——连接将死时丢一条 ping 无害（探测超时
            // 自会结算），断线/Bye(Refresh) 由 poll 的权威信号接管。
            if let Some(t) = self.transport.as_mut() {
                let _ = t.send_reliable(&buf);
            }
        }
        if let Some(outcome) = self.bw.pump(now_us) {
            self.settle_bw(outcome)?;
        }
        Ok(())
    }

    /// 控制分发臂（0x06，T15）：ProbePong → [`crate::probe::ProbeEngine::on_pong`]
    /// （now_us = 本端收到时刻，会话钟零扩展）。非 Pong 输入忽略。
    pub fn on_pong(&mut self, pong: &ControlMessage, now_us: u64) {
        self.probe.on_pong(pong, now_us);
    }

    /// Welcome 之后发起一次带宽粗测（`BwProbeRequest` 走 reliable 控制流；
    /// 每次会话仅一次，重复调用为空操作）。不必等探测完成再等首帧——
    /// 协商值已生效，探测是覆盖（B6）。v0 恒请求
    /// [`crate::probe::BW_PROBE_MAX_TOTAL_BYTES`]（256KB）一次突发。
    pub fn start_bw_probe(&mut self, now_us: u64) -> Result<()> {
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "start_bw_probe requires Streaming state, got {other:?} (S1)"
                )))
            }
        }
        if self.bw_probe_started {
            return Ok(()); // 每会话仅一次
        }
        let probe_id = self.bw_probe_next_id;
        self.bw_probe_next_id = self.bw_probe_next_id.wrapping_add(1);
        let total_bytes = crate::probe::BW_PROBE_MAX_TOTAL_BYTES;
        let mut buf = Vec::new();
        ControlMessage::BwProbeRequest {
            probe_id,
            total_bytes,
        }
        .encode(&mut buf);
        // T17：尽力而为——连接将死时探测流发不出去，超时结算自会退化为
        //「保持协商值」，断线信号由 poll 接管。
        if let Some(t) = self.transport.as_mut() {
            let _ = t.send_reliable(&buf);
        }
        self.bw.start(probe_id, total_bytes, now_us);
        self.bw_probe_started = true;
        Ok(())
    }

    /// Datagram 分发臂（T15，B4）：首字节 0x09 的探测块 →
    /// [`crate::probe::BwEstimator::on_chunk`]。探测号不符 / 已结算的块
    /// 静默忽略；截断的探测块（线格式错误）返回 Err(Protocol)。
    pub fn on_bw_datagram(&mut self, buf: &[u8], now_us: u64) -> Result<()> {
        let data = crate::probe::parse_bw_datagram(buf)?;
        if let Some(outcome) = self.bw.on_chunk(&data, now_us) {
            self.settle_bw(outcome)?;
        }
        Ok(())
    }

    /// host 侧 u32 时间戳 → client 本地时间轴（微秒；无偏移样本返回
    /// None——报告侧必须当「无数据」，不是当 0 偏移出数，B3）。T18 报告
    /// 的 e2e 与 breakdown.network 的一切 host 时间戳换算必须经此。
    /// u32 零扩展无损（B9：70 分钟守卫内两钟同源同纪元）。
    pub fn host_to_local_us(&self, host_us: u32) -> Option<u64> {
        self.probe.to_local_us(u64::from(host_us))
    }

    /// 探测快照（T18 嵌入 client 会话报告 JSON 的 session 段）。
    pub fn probe_snapshot(&self) -> ProbeSnapshot {
        let s = self.probe.stats();
        ProbeSnapshot {
            rtt_us_median: s.rtt_us_median,
            offset_us_median: s.offset_us_median,
            bw_estimate_bps: self.bw_estimate_bps,
            initial_bitrate_bps: self.initial_bitrate_bps,
            negotiated_bitrate_bps: self.negotiated.map(|n| n.bitrate_bps).unwrap_or(0),
            pings_sent: s.pings_sent,
            pongs_received: s.pongs_received,
            probes_lost: s.probes_lost,
        }
    }

    /// 带宽粗测结算（即时最高块或超时两路共用）：记录估速并按 B6 下发
    /// SetBitrate——退化/失败/未结算（None）不发、保持协商值。
    fn settle_bw(&mut self, outcome: crate::probe::BwOutcome) -> Result<()> {
        self.bw_estimate_bps = outcome.estimate_bps;
        let cap = self
            .negotiated
            .expect("Streaming implies negotiated")
            .bitrate_bps;
        match crate::probe::initial_bitrate_from(outcome.estimate_bps, cap) {
            Some(bps) if bps > 0 => {
                let mut buf = Vec::new();
                ControlMessage::SetBitrate {
                    bps,
                    reason: proto::BitrateReason::Probe,
                }
                .encode(&mut buf);
                // T17：尽力而为——连接将死时发不出去只影响这一次调整
                //（host 保持原值），断线信号由 poll 接管。
                if let Some(t) = self.transport.as_mut() {
                    let _ = t.send_reliable(&buf);
                }
                self.initial_bitrate_bps = Some(bps);
            }
            // None = 退化/失败 → 不发（B6）；bps==0 只来自病态协商 cap==0
            // （握手已拒 0 值裁定），同样不发——管线侧也会拒绝。
            _ => {}
        }
        Ok(())
    }

    // ─────────────────────────────────────────────────────────────
    // T17：断线重连（Reconnecting / 指数退避 / 强制 IDR / 饱和刷新重建）
    // ─────────────────────────────────────────────────────────────

    /// T17：设置断线重连配置（驱动开始前调用；默认 [`ReconnectConfig::default`]）。
    ///
    /// 返回 Err = 退避参数非法（与 [`Backoff::new`] 同一校验），此时**保持
    /// 原配置不变**——非法配置不得半生效。
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig) -> Result<()> {
        self.backoff = Backoff::new(
            cfg.backoff_initial,
            cfg.backoff_max,
            cfg.backoff_max_attempts,
        )?;
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

    /// 管线代数：每建一次管线 +1（窗口内 resume 不变；「管线不拆」的
    /// 可观测判据）。
    pub fn pipeline_generation(&self) -> u32 {
        self.pipeline_generation
    }

    /// T17：管线重建工厂（resume 被拒 / Bye(Refresh) 时调用）。不注入则与
    /// 首次握手同一构造路径（select_* auto 后端）；testkit 注入脚本化
    /// 工厂做确定性离线测试。
    pub fn set_pipeline_factory(
        &mut self,
        f: Box<dyn FnMut() -> Result<ControllerPipeline> + Send>,
    ) {
        self.pipeline_factory = Some(f);
    }

    /// T18 增补：带裁定参数的管线工厂——闭包收到按 Welcome 裁定值
    /// （resolution/fps/bitrate 已覆盖）构造好的 [`PipelineConfig`]。
    /// CLI 的 fake / real 后端选择需要按**实际裁定参数**建管线；设置后
    /// 优先于 [`Self::set_pipeline_factory`] 与 auto 选择路径。
    pub fn set_pipeline_factory_for(&mut self, f: ControllerFactoryFor) {
        self.pipeline_factory_for = Some(f);
    }

    /// T17 测试接缝：connect 工厂——重连（含首次 connect）的连接来源。
    /// 不注入则真网络路径：留存的 QuicEndpoint::connect（坑 7：重连必须
    /// 复用同一端点）。MockTransport 模式没有端点，注入工厂返回脚本化
    /// transport（成功预置 Welcome / 失败模拟不可达）。
    pub fn set_connect_factory(&mut self, f: ConnectFactory) {
        self.connect_factory = Some(f);
    }

    /// T17：留存的 QUIC 端点（真网络构造才有）。重连复用同一端点（R3）；
    /// rebind 路径迁移（N9）也经它执行。
    pub fn endpoint(&self) -> Option<&QuicEndpoint> {
        self.retained_endpoint.as_ref()
    }

    // ─────────────────────────────────────────────────────────────
    // T18：观测访问器（client 会话报告 SessionObservations 的数据源）
    // ─────────────────────────────────────────────────────────────

    /// 累计发出的 IdrRequest 条数（含重发）。
    pub fn idr_requests_sent(&self) -> u64 {
        self.idr_requests_total
    }

    /// 有效消费的 IdrAck 条数（重复/过期 ack 不计；IdrController 同口径）。
    pub fn idr_acks_received(&self) -> u64 {
        self.idr.stats().acks_received
    }

    /// 停滞看门狗累计触发次数。
    pub fn watchdog_fires(&self) -> u64 {
        self.watchdog.fires()
    }

    /// 重组表深度峰值（铁律 1：恒 ≤ 2；每拍采样 tracker 在途数）。
    pub fn max_reassembly_depth(&self) -> u32 {
        self.max_reassembly_depth
    }

    /// 最近一拍的 NetStats 快照（停机后 transport 丢弃，取缓存）。
    pub fn net_stats(&self) -> rc_net::NetStats {
        self.net_stats_cache
    }

    /// 输入单向延迟样本（微秒；InputAck 换算）。
    pub fn input_one_way_samples_us(&self) -> &[u32] {
        &self.input_one_way_samples
    }

    /// 修复卡 B-3（I9 可诊断性）：输入捕获器错误累计（每次 next_events
    /// 返回 Err +1；持续增长 = 权限/驱动侧故障可观测）。
    pub fn input_capture_errors(&self) -> u64 {
        self.input_capture_errors_total
    }

    /// 修复卡 B-4：同批 Bye 类别冲突（Normal vs Refresh）被忽略的条数
    ///（先到的生效；非 0 = 对端行为异常）。
    pub fn bye_conflicts_dropped(&self) -> u64 {
        self.bye_conflicts_dropped
    }

    /// 重连成功到首个 IDR 帧上屏的毫秒数（未发生 resume 或未测得为 None）。
    pub fn reconnect_recovery_ms(&self) -> Option<u64> {
        self.recovery_ms
    }

    // ─────────────────────────────────────────────────────────────
    // T20：剪贴板 UTF-8 纯文本双向同步（与 HostSession 完全对称）
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
    /// 在重连恢复（`Resumed`）、全新重建（`Rebuilt`）或驱动器时钟归零时调用，
    /// 清除旧纪元的变更计数基线、回声基准、挂起槽与节流时间戳。未启用剪贴板时为空操作。
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
                    eprintln!("rc-session client: 剪贴板后端轮询失败（不拆会话）: {e}");
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
                                "rc-session client: 发送 ClipboardNotify 失败（不拆会话）: {e}"
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!("rc-session client: 构造 ClipboardNotify 失败（不拆会话）: {e}");
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
            eprintln!("rc-session client: 剪贴板后端写入失败（不拆会话）: {e}");
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
    /// 仅 [`SessionState::Streaming`] 允许调用；校验 `channel` 在 `1..=4` 且
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

    /// 组装 client 会话报告的全部原始观测（铁律 4：只吃会话内已记账的
    /// 注入数据，不读任何时钟；`duration_us` 是请求的测量时长——fps
    /// 分母，不含排空；`connect_to_first_frame_us` 由驱动器（CLI 组合根）
    /// 以 Endpoint 创建之前的时刻起算传入）。
    ///
    /// 停机后调用（Closed 状态管线仍在，账目可查）。
    pub fn observations(
        &self,
        duration_us: u64,
        connect_to_first_frame_us: Option<u64>,
    ) -> crate::report::SessionObservations {
        let snapshot = self.probe_snapshot();
        let frames = self
            .pipeline
            .as_ref()
            .map(|p| {
                p.trace()
                    .frames()
                    .iter()
                    .map(|f| {
                        let seq = f.seq().0;
                        // Captured 标记写入的就是包头 host 域 capture_ts（坑 1）；
                        // encode_done 取自包头记录表元组第 2 项（H8：.1 是 encode_done_ts，
                        // 被容量淘汰的旧帧退化为 captured——encode 段记 0，诚实可解释）。
                        let captured = f.at(rc_trace::Mark::Captured).map(|ts| ts.0).unwrap_or(0);
                        let encode_done = self
                            .frame_headers
                            .get(&seq)
                            .map(|(_, e)| *e)
                            .unwrap_or(captured);
                        crate::report::FrameObservation {
                            seq: f.seq(),
                            outcome: f.outcome(),
                            captured_us: captured,
                            encode_done_us: encode_done,
                            received_us: f.at(rc_trace::Mark::Received).map(|ts| ts.0),
                            decoded_us: f.at(rc_trace::Mark::Decoded).map(|ts| ts.0),
                            presented_us: f.at(rc_trace::Mark::Presented).map(|ts| ts.0),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let current_displayed = self.pipeline.as_ref().map(|p| p.displayed()).unwrap_or(0);
        let total_displayed = self
            .prev_pipelines_displayed
            .saturating_add(current_displayed);
        let rs = self.reconnect_stats();
        crate::report::SessionObservations {
            frames,
            duration_us,
            total_displayed: Some(total_displayed),
            connect_to_first_frame_us,
            rtt_us: snapshot.rtt_us_median.and_then(|v| u32::try_from(v).ok()),
            clock_offset_us: snapshot.offset_us_median,
            input_one_way_samples_us: self.input_one_way_samples.clone(),
            idr_requests: self.idr_requests_total,
            idr_acks: self.idr.stats().acks_received,
            watchdog_fires: self.watchdog.fires(),
            max_reassembly_depth: self.max_reassembly_depth,
            net: self.net_stats_cache,
            reconnect: crate::report::ReconnectStats {
                // H5：attempts 仅计意外断线重连（含主动演练）；饱和刷新 Bye(Refresh)
                // 属预期纪元重建，单列 refreshes，不触发 resumed=false 误判。
                attempts: u64::from(rs.disconnects),
                resumed: rs.resumes > 0,
                recovery_ms: self.recovery_ms,
                refreshes: u64::from(rs.refreshes),
            },
            clipboard: crate::report::ClipboardReport::from_stats(
                self.clipboard_enabled(),
                self.clipboard_stats(),
            ),
            session_id: self.session_id_current,
        }
    }

    /// T17：断开处置。保留 ControllerPipeline、session_id 与 QuicEndpoint
    ///（重连复用**同一 endpoint**：建连时留存的 peer_addr / 证书 /
    /// server_name 直接再用；endpoint 被 drop 则一切重来，坑 7）。进入
    /// Reconnecting，看门狗（T16）与探测（T15）、输入泵暂停（R8：tick /
    /// pump_probes 均要求 Streaming，天然状态门禁）。
    ///
    /// `tick` 收到 `NetEvent::Disconnected` 时内联调用；测试/驱动也可
    /// 显式调用（Box 走的传输被 drop 即触发对端的真实 close）。
    pub fn on_disconnected(&mut self, now_us: u64) {
        if self.state != SessionState::Streaming {
            return; // 幂等：重复 Disconnected 不重置退避
        }
        self.rstats.disconnects += 1;
        self.transport = None; // 死连接即弃；重连开新连接
        self.auth_phase = AuthPhase::Idle;
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.state = SessionState::Reconnecting;
        self.refresh_pending = false;
        self.reconnect_phase = ReconnectPhase::ResumePending;
        // 首次尝试时刻 = 断开时刻 + initial（退避序列 0.5/1.5/3.5/7.5/15.5s）。
        self.next_attempt_at_us = now_us + self.backoff.delay_before(1).as_micros() as u64;
    }

    /// T17：重连驱动拍（时间注入；到点判定全凭 `now_us`，不读墙钟，R3）。
    ///
    /// 顺序：① 已有在途连接 → poll 等裁定（Welcome bit0 → Resumed /
    /// 降级；bit1 校验）；② 无连接且到点 → connect（工厂或留存端点）→
    /// 发 ReconnectRequest{sid=旧, resume=1}（或刷新路径的全新 Hello）→
    /// [`ReconnectOutcome::Connecting`]；③ 未到点 → [`ReconnectOutcome::Waiting`]。
    /// connect / 握手失败 → [`Backoff::on_failure`] 退避再试；次数用尽 →
    /// [`ReconnectOutcome::GaveUp`]（会话按红线停机）。
    pub fn tick_reconnect(&mut self, now_us: u64) -> Result<ReconnectOutcome> {
        match self.state {
            SessionState::Reconnecting => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "tick_reconnect requires Reconnecting state, got {other:?}"
                )))
            }
        }
        // ① 在途连接：poll 等裁定（Welcome）或断开。修复卡 B-1：poll
        // 错误不再 `?` 裸上抛（绕过 S6/S8「先完整停机再返回 Err」）——
        // 按红线完整停机后才返回。
        if self.transport.is_some() {
            let events = match self
                .transport
                .as_mut()
                .expect("上面判过 Some")
                .poll(RECONNECT_POLL_SLICE)
            {
                Ok(events) => events,
                Err(e) => {
                    let stop = StopReason::Disconnected(format!(
                        "transport poll failed while reconnecting: {e}"
                    ));
                    self.stop_with(stop, None);
                    return Err(e);
                }
            };
            let mut ev_iter = events.into_iter();
            while let Some(ev) = ev_iter.next() {
                match ev {
                    NetEvent::Reliable(msg) => {
                        if let Some(outcome) = self.on_reconnect_message(&msg, now_us)? {
                            if matches!(
                                outcome,
                                ReconnectOutcome::Resumed | ReconnectOutcome::Rebuilt
                            ) {
                                self.drain_post_welcome_events(ev_iter)?;
                            }
                            return Ok(outcome);
                        }
                        // 其余消息：忽略（等 Welcome），继续 poll 本批。
                    }
                    // 路径迁移不算断线（R5）。
                    NetEvent::PathChanged => {
                        self.rstats.path_changes += 1;
                    }
                    // 连接又断：按一次失败退避（阶段回到 Pending，清鉴权子态）。
                    NetEvent::Disconnected { .. } => {
                        self.transport = None;
                        self.auth_phase = AuthPhase::Idle;
                        self.reconnect_phase = if self.refresh_pending {
                            ReconnectPhase::FreshPending
                        } else {
                            ReconnectPhase::ResumePending
                        };
                        return Ok(self.on_attempt_failure(now_us));
                    }
                    // client 不收媒体/输入（方向错误，S4）：丢弃计数。
                    NetEvent::Datagram(_) | NetEvent::ReliableUrgent(_) => {
                        self.early_events_dropped += 1;
                    }
                    NetEvent::Connected => {}
                }
            }
            // A7：重连全新握手鉴权期间每拍复核超时（注入时钟口径）。
            if let AuthPhase::Authenticating(ref mut auth) = self.auth_phase {
                if auth.poll_timeout(now_us) {
                    let timeout = Duration::from_micros(self.cfg.auth.auth_timeout_us);
                    eprintln!(
                        "[rc-session/auth] client authentication timed out during reconnect after {timeout:?}"
                    );
                    self.auth_phase = AuthPhase::Idle;
                    self.stop_with(
                        StopReason::Disconnected(format!(
                            "auth timed out while reconnecting after {timeout:?}"
                        )),
                        None,
                    );
                    return Err(Error::Timeout(timeout));
                }
            }
            // 本批无裁定：继续等（驱动循环靠 poll 分片防自旋）。
            return Ok(ReconnectOutcome::Waiting {
                next_attempt_in_ms: 0,
            });
        }
        // ② 无连接且到点 → 发起一次 connect。
        if now_us >= self.next_attempt_at_us {
            self.rstats.reconnect_attempts += 1;
            // connect 与首条消息的发送都算「一次尝试」：连接刚建好就死
            //（如对端半接受被丢）按失败退避重试，不向驱动器上抛——重连
            // 状态机的自愈轮次（默认 5 次）就是为这种瞬态准备的。
            let attempt = self.open_connection().and_then(|t| {
                self.transport = Some(t);
                match self.reconnect_phase {
                    ReconnectPhase::FreshPending => self.send_fresh_hello(now_us),
                    _ => self.send_reconnect_request(),
                }
            });
            match attempt {
                Ok(()) => {
                    self.reconnect_phase = match self.reconnect_phase {
                        ReconnectPhase::FreshPending => ReconnectPhase::AwaitingFresh,
                        _ => ReconnectPhase::AwaitingVerdict,
                    };
                    return Ok(ReconnectOutcome::Connecting);
                }
                Err(_) => {
                    self.transport = None;
                    self.auth_phase = AuthPhase::Idle;
                    return Ok(self.on_attempt_failure(now_us));
                }
            }
        }
        // ③ 未到点：退避等待。
        let next_in_ms = self
            .next_attempt_at_us
            .saturating_sub(now_us)
            .saturating_div(1_000);
        Ok(ReconnectOutcome::Waiting {
            next_attempt_in_ms: next_in_ms,
        })
    }

    /// T17：收到 Bye{reason=Refresh}（Streaming 态，由 `tick` 的事件分发
    /// 内联调用）。**零退避**立即全新重连（host 在线等着，退避只会白等）：
    /// 换新 session_id、下一拍 connect 后发全新 Hello、经工厂重建管线；
    /// 后续 Welcome 必须带 bit1=1（`on_reconnect_message` 校验之），据此
    /// 清零 T15 时钟偏移估计、重置本地会话钟纪元（驱动器负责把注入钟
    /// 归零——会话侧清的是纪元相关的估计与状态）。
    pub fn on_bye_refresh(&mut self, now_us: u64) -> Result<()> {
        // `now_us` 保留在签名里（任务卡契约）：零退避的到点锚定刻意用 0
        //（见下方注释），不消费本参数。
        let _ = now_us;
        match self.state {
            SessionState::Streaming => {}
            other => {
                return Err(Error::InvalidConfig(format!(
                    "on_bye_refresh requires Streaming state, got {other:?}"
                )))
            }
        }
        self.rstats.refreshes += 1;
        // 旧管线收口：对端已按红线拆除（先发 Bye 再拆），无在途帧可等，
        // 直接 finish + shutdown 保持账目闭合；引用保留至工厂重建覆盖。
        if let Some(p) = self.pipeline.as_mut() {
            p.finish_in_flight();
            p.shutdown();
        }
        self.transport = None; // host 已关：丢弃残余连接
        self.auth_phase = AuthPhase::Idle;
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.session_id_current = self.session_id_current.wrapping_add(1);
        self.refresh_pending = true;
        self.reconnect_phase = ReconnectPhase::FreshPending;
        self.backoff.reset();
        // 零退避不能写 `now_us`：驱动器紧接着会把注入钟归零（刷新语义），
        // 新纪元读数（≈0）会比旧纪元锚定的到点时刻小，零退避反而变成
        // 「等时钟爬回旧值」的 70 分钟级延迟。0 在任何纪元读数下都立即
        // 到点。
        self.next_attempt_at_us = 0;
        self.state = SessionState::Reconnecting;
        Ok(())
    }

    /// Reconnecting 期一条可靠消息的裁定。返回 Some(outcome) 表示重连
    /// 状态机有了终局/迁移（Resumed / Rebuilt / Waiting）；None = 非裁定
    /// 消息（忽略，继续等）。
    fn on_reconnect_message(
        &mut self,
        msg: &[u8],
        now_us: u64,
    ) -> Result<Option<ReconnectOutcome>> {
        let decoded = match ControlMessage::decode(msg) {
            Ok(d) => d,
            // 脏消息：与 Streaming 期同一纪律（S8 宁拒收）——停机后上抛。
            Err(e) => {
                let stop =
                    StopReason::Disconnected(format!("dirty message while reconnecting: {e}"));
                self.stop_with(stop, None);
                return Err(e);
            }
        };
        match (decoded, self.reconnect_phase) {
            (ControlMessage::Welcome(w), ReconnectPhase::AwaitingVerdict) => {
                // 修复卡 B-1：Welcome 校验失败（对端可触发——session_id 不
                // 回显 / auth 拒绝）不再 `?` 裸上抛——先按红线完整停机
                //（S6/S8）再返回 Err，不得把会话晾在 Reconnecting。
                if let Err(e) = self.validate_welcome(&w) {
                    let stop = StopReason::Disconnected(format!(
                        "welcome rejected while reconnecting: {e}"
                    ));
                    self.stop_with(stop, None);
                    return Err(e);
                }
                if w.host_flags.resume_accepted() {
                    // resume 命中：管线复用（FrameSeq 延续），立即强制 IDR
                    //（R4 client 侧半边），清 T16 在途去抖（坑 6：旧连接的
                    // IdrAck 永远不会来）、重置看门狗基准；时钟/RTT/偏移
                    // 延续（纪元未变）；T15 带宽粗测允许重新发起。
                    // 修复卡 B-1：IDrRequest 发送失败（连接将死）不再裸
                    // 上抛——完整停机后返回 Err（此时尚未迁移 Streaming，
                    // 停机序列经保留管线照常收口）。
                    if let Err(e) = self.send_idr_request(IdrReason::ReconnectRecovery) {
                        let stop = StopReason::Disconnected(format!(
                            "send IdrRequest after resume failed: {e}"
                        ));
                        self.stop_with(stop, None);
                        return Err(e);
                    }
                    self.idr = IdrController::new(IdrControllerConfig::default());
                    self.watchdog.arm(u32::try_from(now_us).unwrap_or(u32::MAX));
                    self.state = SessionState::Streaming;
                    self.reconnect_phase = ReconnectPhase::Idle;
                    self.backoff.reset();
                    self.bw_probe_started = false;
                    self.rstats.resumes += 1;
                    // 修复卡 B-2：断线期间的关键批在途回执表清空——旧连接
                    // 的 InputAck 随连接消亡永远不会来，残留只会在序号错位
                    //（如断线前有批未被 host 收到）时把回执错配到断线前的
                    // 发送时刻，产出跨断线的伪单向延迟样本。
                    // key_batch_seq_next 对齐策略：host 命中 resume 复用会话、
                    // key_batches_total **延续**计数（host_session.rs 的
                    // handle_reconnect 不复位），client 同样延续计数。
                    self.pending_input_acks.clear();
                    // T18：恢复计时起点——首个完整到达的 IDR 帧结出
                    // recovery_ms（上面强制的 IdrRequest 的响应）。
                    self.recovery_pending_since_us = Some(now_us);
                    // H6：resume 恢复重置剪贴板纪元态（避免复用断线前的基线与回声状态）。
                    self.reset_clipboard_epoch();
                    self.tool_rx.clear();
                    self.audio_ctrl_rx = None;
                    self.audio_rx.clear();
                    Ok(Some(ReconnectOutcome::Resumed))
                } else {
                    // bit0=0 → 同连接降级：换新 session_id 补发全新 Hello
                    //（T19 A2：构造全新 ClientAuthenticator 走全新挑战应答；
                    // 管线重建等终局 Welcome{Ok}，届时首帧天然 IDR）。
                    self.session_id_current = self.session_id_current.wrapping_add(1);
                    // 修复卡 B-1：降级 Hello 发送失败不再裸上抛——完整停机
                    // 后返回 Err（sid 已自增无所谓：会话按红线终结）。
                    if let Err(e) = self.send_fresh_hello(now_us) {
                        let stop = StopReason::Disconnected(format!(
                            "send fresh Hello after resume rejected failed: {e}"
                        ));
                        self.stop_with(stop, None);
                        return Err(e);
                    }
                    self.reconnect_phase = ReconnectPhase::AwaitingFresh;
                    Ok(Some(ReconnectOutcome::Waiting {
                        next_attempt_in_ms: 0,
                    }))
                }
            }
            (ControlMessage::Welcome(w), ReconnectPhase::AwaitingFresh) => {
                // T19：session_id 回显校验（S3）+ ClientAuthenticator 状态机裁定。
                if w.session_id != self.session_id_current {
                    let e = Error::Protocol {
                        offset: 0,
                        msg: format!(
                            "welcome session_id {} does not echo local {} (refusing)",
                            w.session_id, self.session_id_current
                        ),
                    };
                    let stop =
                        StopReason::Disconnected(format!("welcome rejected while rebuilding: {e}"));
                    self.stop_with(stop, None);
                    return Err(e);
                }
                let action = match &mut self.auth_phase {
                    AuthPhase::Authenticating(auth) => auth.on_welcome(&w),
                    AuthPhase::Idle => ClientAuthAction::Failed(
                        "protocol violation: received Welcome without active auth state".into(),
                    ),
                };
                match action {
                    ClientAuthAction::Waiting => {
                        // 过程 Welcome{ChallengeRequired}：显式忽略 granted（坑 5），
                        // 不建管线（A6），返回 Ok(None) 以继续消费同批后续 AuthChallenge。
                        Ok(None)
                    }
                    ClientAuthAction::Failed(reason) => {
                        eprintln!(
                            "[rc-session/auth] client authentication failed during reconnect: {reason}"
                        );
                        let e = Error::Protocol {
                            offset: 0,
                            msg: reason,
                        };
                        let stop =
                            StopReason::Disconnected(format!("auth failed while rebuilding: {e}"));
                        self.stop_with(stop, None);
                        Err(e)
                    }
                    ClientAuthAction::Respond(_) | ClientAuthAction::RespondOwner(_) => {
                        let e = Error::Protocol {
                            offset: 0,
                            msg: "unexpected Respond action from on_welcome".into(),
                        };
                        let stop = StopReason::Disconnected(format!("{e}"));
                        self.stop_with(stop, None);
                        Err(e)
                    }
                    ClientAuthAction::Done => {
                        self.auth_phase = AuthPhase::Idle;
                        let granted = w.granted;
                        if self.refresh_pending && !w.host_flags.clock_epoch_reset() {
                            let e = Error::Protocol {
                                offset: 0,
                                msg:
                                    "refresh rebuild requires Welcome host_flags bit1 (clock epoch \
                                      reset) to be set, refusing to mix epochs"
                                        .into(),
                            };
                            let stop = StopReason::Disconnected(format!("{e}"));
                            self.stop_with(stop, None);
                            return Err(e);
                        }
                        // 全新重建：经工厂建管线（FrameSeq / 双端时钟归零由新会话
                        // 语义保证——驱动器随后把注入钟归零），纪元相关的估计全部
                        // 清零重建（坑 10：两段不可混进同一分位数）。
                        // 修复卡 B-1：管线构建失败不再裸上抛——完整停机后返回 Err。
                        let mut pipeline = match self.build_controller_pipeline(granted) {
                            Ok(p) => p,
                            Err(e) => {
                                let stop = StopReason::Disconnected(format!(
                                    "rebuild controller pipeline failed: {e}"
                                ));
                                self.stop_with(stop, None);
                                return Err(e);
                            }
                        };
                        pipeline.set_clock(Arc::clone(&self.clock));
                        // H4：替换管线前累加旧管线的无界上屏帧数。
                        if let Some(old_p) = self.pipeline.as_ref() {
                            self.prev_pipelines_displayed = self
                                .prev_pipelines_displayed
                                .saturating_add(old_p.displayed());
                        }
                        self.pipeline = Some(pipeline);
                        self.negotiated = Some(granted);
                        self.state = SessionState::Streaming;
                        self.reconnect_phase = ReconnectPhase::Idle;
                        self.pipeline_generation += 1;
                        self.last_displayed = 0; // 新管线从 0 计
                        self.probe = crate::probe::ProbeEngine::new(); // 偏移估计清零
                        self.bw = crate::probe::BwEstimator::new();
                        self.bw_probe_started = false;
                        self.tracker = DeliveryTracker::new(); // FrameSeq 重启，旧水位作废
                        self.idr = IdrController::new(IdrControllerConfig::default());
                        self.watchdog.arm(u32::try_from(now_us).unwrap_or(u32::MAX));
                        self.refresh_pending = false;
                        self.backoff.reset();
                        self.rstats.rebuilds += 1;
                        // 修复卡 B-2：全新会话的关键批记账归零——在途回执表清空
                        // + 序号重计（下一批 = 1）。对齐策略：host 侧全新会话把
                        // key_batches_total 复位 0（发现 8 已落地，host_session.rs
                        // 与 client_rebuild_resets_key_batch_accounting /
                        // key_batch_counter_reset_on_fresh_session_only 两侧测试
                        // 互证），client 从 1 重新计数，两侧序号对齐；旧表跨会话
                        // 无效（seq 空间已重启，残留只会错配出伪样本）。
                        self.pending_input_acks.clear();
                        self.key_batch_seq_next = 0;
                        // H6：全新重建重置剪贴板纪元态（时钟归零后不卡死节流窗口）。
                        self.reset_clipboard_epoch();
                        self.tool_rx.clear();
                        self.audio_ctrl_rx = None;
                        self.tool_rx_overflow_dropped = 0;
                        self.audio_rx.clear();
                        Ok(Some(ReconnectOutcome::Rebuilt))
                    }
                }
            }
            (ControlMessage::AuthChallenge(ch), ReconnectPhase::AwaitingFresh) => {
                let action = match &mut self.auth_phase {
                    AuthPhase::Authenticating(auth) => auth.on_challenge(&ch),
                    AuthPhase::Idle => ClientAuthAction::Failed(
                        "protocol violation: received AuthChallenge without active auth state"
                            .into(),
                    ),
                };
                match action {
                    ClientAuthAction::Respond(resp) => {
                        let mut buf = Vec::new();
                        ControlMessage::AuthResponse(resp).encode(&mut buf);
                        if let Err(e) = self
                            .transport
                            .as_mut()
                            .expect("Reconnecting 期发送前必有连接")
                            .send_reliable(&buf)
                        {
                            let stop = StopReason::Disconnected(format!(
                                "send AuthResponse while rebuilding failed: {e}"
                            ));
                            self.stop_with(stop, None);
                            return Err(e);
                        }
                        Ok(None)
                    }
                    ClientAuthAction::RespondOwner(signature) => {
                        let mut buf = Vec::new();
                        ControlMessage::OwnerAuthResponse { signature }.encode(&mut buf);
                        if let Err(e) = self
                            .transport
                            .as_mut()
                            .expect("Reconnecting 期发送前必有连接")
                            .send_reliable(&buf)
                        {
                            let stop = StopReason::Disconnected(format!(
                                "send OwnerAuthResponse while rebuilding failed: {e}"
                            ));
                            self.stop_with(stop, None);
                            return Err(e);
                        }
                        Ok(None)
                    }
                    ClientAuthAction::Failed(reason) => {
                        eprintln!(
                            "[rc-session/auth] client authentication failed during reconnect: {reason}"
                        );
                        let e = Error::Protocol {
                            offset: 0,
                            msg: reason,
                        };
                        let stop = StopReason::Disconnected(format!(
                            "auth challenge failed while rebuilding: {e}"
                        ));
                        self.stop_with(stop, None);
                        Err(e)
                    }
                    ClientAuthAction::Done | ClientAuthAction::Waiting => {
                        let e = Error::Protocol {
                            offset: 0,
                            msg: "unexpected action from on_challenge".into(),
                        };
                        let stop = StopReason::Disconnected(format!("{e}"));
                        self.stop_with(stop, None);
                        Err(e)
                    }
                }
            }
            (
                ControlMessage::Bye {
                    reason: ByeReason::Normal,
                },
                _,
            ) => {
                // 重连等待期对端要求关闭：如实停机。
                let stop = StopReason::ByeReceived(ByeReason::Normal);
                self.stop_with(stop.clone(), None);
                Ok(Some(ReconnectOutcome::GaveUp))
            }
            (other, _) => {
                self.log_ignore_once(ignore_tag_client(&other));
                Ok(None)
            }
        }
    }

    /// 一次 connect 失败的退避处理（返回该拍的驱动结果）。
    fn on_attempt_failure(&mut self, now_us: u64) -> ReconnectOutcome {
        match self.backoff.on_failure() {
            Some(delay) => {
                self.next_attempt_at_us = now_us + delay.as_micros() as u64;
                ReconnectOutcome::Waiting {
                    next_attempt_in_ms: delay.as_millis() as u64,
                }
            }
            None => {
                // 次数用尽：按红线停机（rc-cli 按退出码处理，属 T18）。
                let stop = StopReason::Disconnected(format!(
                    "reconnect attempts exhausted after {} (backoff gave up)",
                    self.backoff.attempts_used()
                ));
                self.stop_with(stop, None);
                ReconnectOutcome::GaveUp
            }
        }
    }

    /// 发送 ReconnectRequest{当前会话号, resume=1}（reliable 流）。
    fn send_reconnect_request(&mut self) -> Result<()> {
        let msg = ControlMessage::ReconnectRequest {
            session_id: self.session_id_current,
            resume: true,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.transport
            .as_mut()
            .expect("Reconnecting 期发送前必有连接")
            .send_reliable(&buf)?;
        Ok(())
    }

    /// 发送全新 Hello（当前会话号——重建路径已先自增；reliable 流）。
    ///
    /// T19（A1/A2）：构造全新 [`ClientAuthenticator`]，鉴权位来自 `hello_auth()`
    /// （密码绝不进 Hello），并记录 `on_hello_sent(now_us)` 启动超时窗口。
    fn send_fresh_hello(&mut self, now_us: u64) -> Result<()> {
        let mut auth =
            ClientAuthenticator::with_session_id(self.cfg.auth.clone(), self.session_id_current);
        let (auth_method, auth_payload) = auth.hello_auth();
        let hello = ControlMessage::Hello(Hello {
            version: proto::PROTOCOL_VERSION,
            session_id: self.session_id_current,
            requested: self.cfg.requested,
            auth_method,
            auth_payload,
        });
        let mut buf = Vec::new();
        hello.encode(&mut buf);
        self.transport
            .as_mut()
            .expect("Reconnecting 期发送前必有连接")
            .send_reliable(&buf)?;
        auth.on_hello_sent(now_us);
        self.auth_phase = AuthPhase::Authenticating(auth);
        Ok(())
    }

    /// 诊断出口（trace/displayed/config 等经它取）。
    ///
    /// # Panics
    /// 握手完成（Streaming）之前没有管线，调用即 panic。Closed 后管线
    /// 仍保留（停机账目可查）。
    pub fn pipeline(&self) -> &ControllerPipeline {
        self.pipeline
            .as_ref()
            .expect("pipeline exists after handshake (state == Streaming)")
    }

    // ─────────────────────────────────────────────────────────────
    // 私有实现
    // ─────────────────────────────────────────────────────────────

    /// T18：消费一条 InputAck。`key_batch_seq` 命在途关键批表 → 用探测
    /// 偏移把 host 注入完成时刻折算到 client 域，减发送时刻得单向样本；
    /// 无偏移样本（探测未闭环）或 seq 未知（断线丢批后的错位）时丢弃
    /// 该样本，并把表里 ≤ seq 的过期项清掉（reliable 流保证 ack 按序，
    /// 留着只会占容量）。
    fn on_input_ack(&mut self, key_batch_seq: u64, inject_done_us: u64) {
        while self
            .pending_input_acks
            .front()
            .is_some_and(|(seq, _)| *seq < key_batch_seq)
        {
            self.pending_input_acks.pop_front();
        }
        let Some(pos) = self
            .pending_input_acks
            .iter()
            .position(|(seq, _)| *seq == key_batch_seq)
        else {
            return; // 未知 seq：错位样本，宁缺毋滥
        };
        let (_, send_at_us) = self
            .pending_input_acks
            .remove(pos)
            .expect("position 刚找到");
        if let Some(inject_local_us) = self.probe.to_local_us(inject_done_us) {
            let sample = inject_local_us.saturating_sub(send_at_us);
            self.input_one_way_samples
                .push(u32::try_from(sample).unwrap_or(u32::MAX));
        }
    }

    /// 发送一条 IdrRequest（reliable 流）。
    fn send_idr_request(&mut self, reason: IdrReason) -> Result<()> {
        let mut buf = Vec::new();
        ControlMessage::IdrRequest { reason }.encode(&mut buf);
        self.transport
            .as_mut()
            .expect("Streaming implies transport")
            .send_reliable(&buf)?;
        self.idr_requests_total += 1;
        Ok(())
    }

    /// 握手期（Authenticating 子态）收到一条 reliable 控制消息的处理（T19）。
    fn handle_handshake_reliable(&mut self, msg: &[u8]) -> Result<ClientHandshakeStep> {
        let decoded = ControlMessage::decode(msg)?;
        match decoded {
            ControlMessage::Welcome(welcome) => {
                // S3：session_id 必须回显当前会话号（含过程 Welcome）。
                if welcome.session_id != self.session_id_current {
                    return Err(Error::Protocol {
                        offset: 0,
                        msg: format!(
                            "welcome session_id {} does not echo local {} (refusing)",
                            welcome.session_id, self.session_id_current
                        ),
                    });
                }
                let action = match &mut self.auth_phase {
                    AuthPhase::Authenticating(auth) => auth.on_welcome(&welcome),
                    AuthPhase::Idle => ClientAuthAction::Failed(
                        "protocol violation: received Welcome without active auth state".into(),
                    ),
                };
                match action {
                    ClientAuthAction::Done => Ok(ClientHandshakeStep::Done(welcome.granted)),
                    ClientAuthAction::Waiting => {
                        // 过程 Welcome{ChallengeRequired}：显式忽略 granted（坑 5），
                        // 不建管线（A6），保持 Authenticating 子态。
                        Ok(ClientHandshakeStep::Authenticating)
                    }
                    ClientAuthAction::Failed(reason) => {
                        eprintln!("[rc-session/auth] client authentication failed: {reason}");
                        Err(Error::Protocol {
                            offset: 0,
                            msg: reason,
                        })
                    }
                    ClientAuthAction::Respond(_) | ClientAuthAction::RespondOwner(_) => {
                        Err(Error::Protocol {
                            offset: 0,
                            msg: "unexpected Respond action from on_welcome".into(),
                        })
                    }
                }
            }
            ControlMessage::AuthChallenge(ch) => {
                let action = match &mut self.auth_phase {
                    AuthPhase::Authenticating(auth) => auth.on_challenge(&ch),
                    AuthPhase::Idle => ClientAuthAction::Failed(
                        "protocol violation: received AuthChallenge without active auth state"
                            .into(),
                    ),
                };
                match action {
                    ClientAuthAction::Respond(resp) => {
                        let mut buf = Vec::new();
                        ControlMessage::AuthResponse(resp).encode(&mut buf);
                        self.transport
                            .as_mut()
                            .expect("Connected implies transport")
                            .send_reliable(&buf)?;
                        Ok(ClientHandshakeStep::Authenticating)
                    }
                    ClientAuthAction::RespondOwner(signature) => {
                        let mut buf = Vec::new();
                        ControlMessage::OwnerAuthResponse { signature }.encode(&mut buf);
                        self.transport
                            .as_mut()
                            .expect("Connected implies transport")
                            .send_reliable(&buf)?;
                        Ok(ClientHandshakeStep::Authenticating)
                    }
                    ClientAuthAction::Failed(reason) => {
                        eprintln!("[rc-session/auth] client authentication failed: {reason}");
                        Err(Error::Protocol {
                            offset: 0,
                            msg: reason,
                        })
                    }
                    ClientAuthAction::Done | ClientAuthAction::Waiting => Err(Error::Protocol {
                        offset: 0,
                        msg: "unexpected action from on_challenge".into(),
                    }),
                }
            }
            other => Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "expected Welcome or AuthChallenge during handshake, got {other:?} (refusing)"
                ),
            }),
        }
    }

    /// 鉴权终局通过后建管线并迁移到 Streaming（A6：管线只在此处启动）。
    fn finalize_handshake(&mut self, granted: SessionParams) -> Result<()> {
        let mut pipeline = self.build_controller_pipeline(granted)?;
        pipeline.set_clock(Arc::clone(&self.clock));

        self.pipeline = Some(pipeline);
        self.negotiated = Some(granted);
        self.state = SessionState::Streaming;
        // T17：首建管线代数 +1（与重建同口径）。
        self.pipeline_generation += 1;
        // T16（W1）：进入 Streaming 即锚定看门狗基线——首帧迟到（如首个
        // IDR 被丢）同样能触发恢复，不等首帧到达才武装。
        self.watchdog.arm((self.clock)());
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.tool_rx_overflow_dropped = 0;
        self.audio_rx.clear();
        Ok(())
    }

    /// Welcome 校验（S3）：session_id 必须回显当前会话号、auth_status
    /// 必须为 Ok。返回裁定参数。T17 重连路径共用（重建后回显新号）。
    fn validate_welcome(&self, welcome: &proto::Welcome) -> Result<SessionParams> {
        // S3：session_id 不回显即断。
        if welcome.session_id != self.session_id_current {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "welcome session_id {} does not echo local {} (refusing)",
                    welcome.session_id, self.session_id_current
                ),
            });
        }
        // S3：auth_status ≠ 0 即断（本卡只认通过；挑战路径是 T19）。
        if welcome.auth_status != AuthStatus::Ok {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!(
                    "welcome auth_status {:?} != Ok (this round only accepts Ok; \
                     challenge/denial is T19)",
                    welcome.auth_status
                ),
            });
        }
        Ok(welcome.granted)
    }

    /// 以裁定参数建 ControllerPipeline：重建工厂优先（T17 脚本化路径）；
    /// 否则注入后端 → auto 工厂选择（与首次握手同一构造路径）。
    /// 不做 set_clock / 赋值（调用方负责）。
    fn build_controller_pipeline(&mut self, granted: SessionParams) -> Result<ControllerPipeline> {
        // T18：带裁定参数的工厂最优先（CLI 的 fake/real 严格选择）。
        let pcfg = {
            let mut c = self.cfg.pipeline.clone();
            c.resolution = rc_types::Resolution::new(granted.width, granted.height);
            c.fps = granted.fps;
            c.bitrate_bps = granted.bitrate_bps;
            c.validate()?;
            c
        };
        if let Some(f) = self.pipeline_factory_for.as_mut() {
            return f(&pcfg);
        }
        if let Some(f) = self.pipeline_factory.as_mut() {
            return f();
        }
        // 以裁定值覆盖模板的 pcfg 已在上方算好（requested 不生效，S3）。
        let (decoder, renderer) = match self.backends.take() {
            Some(b) => b,
            None => (
                rc_pipeline::select_decoder(&pcfg)?,
                rc_pipeline::select_renderer(&pcfg)?,
            ),
        };
        ControllerPipeline::with_backends(pcfg, decoder, renderer)
    }

    /// 握手失败的收尾：清空鉴权子态、关闭连接并返回原错误。
    fn handshake_fail(&mut self, e: Error) -> Error {
        self.auth_phase = AuthPhase::Idle;
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

    /// reliable 控制消息的单点分发（S4/S8；client 视角）。
    ///
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
            ControlMessage::Bye { reason } => Ok(ControlFlow::Bye(reason)),
            // T15（0x06）：ProbePong → 探测引擎（t2 = 会话钟零扩展，B9）。
            msg @ ControlMessage::ProbePong { .. } => {
                self.on_pong(&msg, u64::from((self.clock)()));
                Ok(ControlFlow::Handled)
            }
            // T16（0x04）：IdrAck → IdrController 关联确认（帧先到时立即
            // 闭环；否则转等该 IDR 帧完整到达）。
            ControlMessage::IdrAck { frame_seq } => {
                self.idr.on_ack(FrameSeq(frame_seq), (self.clock)());
                Ok(ControlFlow::Handled)
            }
            // T18（0x0A）：InputAck → 输入单向延迟采样。换算：host 注入完成
            // 时刻经探测偏移折算到 client 域，减去本端记录的关键批发送时刻。
            // 无偏移样本（探测尚未闭环）时丢弃该样本——「无数据」不是 0。
            ControlMessage::InputAck {
                key_batch_seq,
                inject_done_us,
            } => {
                self.on_input_ack(key_batch_seq, inject_done_us);
                Ok(ControlFlow::Handled)
            }
            // T20（0x0F）：ClipboardNotify → 剪贴板文本同步（双向；未启用只计数不写本机不拆会话，CB1）。
            ControlMessage::ClipboardNotify { text } => {
                if let Err(e) = self.on_clipboard_notify(&text) {
                    eprintln!("rc-session client: ClipboardNotify 处理失败（不拆会话）: {e}");
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
            other => {
                self.log_ignore_once(ignore_tag_client(&other));
                Ok(ControlFlow::Handled)
            }
        }
    }

    /// stderr 记一次忽略日志（按标签限流，不刷屏）。
    fn log_ignore_once(&mut self, tag: &'static str) {
        if !self.logged_ignore.contains(&tag) {
            self.logged_ignore.push(tag);
            eprintln!("rc-session client: 忽略消息 {tag}（后续卡接入，S8）");
        }
    }

    /// S6 停机序列（幂等）：发 Bye → 停 pump（无独立线程）→ 收/推数据报
    /// 到静默或 500ms 上限 → `finish_in_flight()` → `shutdown()`。
    fn stop_with(&mut self, reason: StopReason, send_bye: Option<ByeReason>) {
        if self.state == SessionState::Closed {
            return;
        }
        self.auth_phase = AuthPhase::Idle;
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
        // 继续收/推数据报到静默或 500ms 上限：给 host 的末批帧一个到达
        // 与上屏的机会（排空 host 侧排空时发出的数据报）。
        //
        // 「静默」按**链路无任何事件**判定：poll 返回了 Reliable/urgent
        // 消息（如 host 的 IdrAck/ProbePong）说明对端还在说话，立即断开
        // 会与在途消息抢线——quinn 的 send_stream 是异步刷出的，Bye 尚未
        // 上路时 Drop 的 CONNECTION_CLOSE 先到，对端就观测不到 Bye
        //（实测竞态：T16 的 IdrAck 流量放大了该窗口）。
        if let (Some(p), Some(t)) = (self.pipeline.as_mut(), self.transport.as_mut()) {
            for _ in 0..DRAIN_ROUNDS {
                let events = match t.poll(DRAIN_POLL_SLICE) {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                let mut link_active = false;
                for ev in events {
                    if let NetEvent::Datagram(data) = ev {
                        // T15（B4）：停机排空期到达的探测块（首字节 0x09）
                        // 直接丢弃——不进 push_datagram（会 Protocol 错误
                        // 中断排空）；会话已 Closing，晚到的结算无消费者。
                        if data.first() == Some(&proto::TYPE_BW_PROBE_DATA) {
                            link_active = true;
                            continue;
                        }
                        link_active = true;
                        if p.push_datagram(&data, (self.clock)()).is_err() {
                            // 脏数据报：排空期不再上抛，账目由 finish 兜底。
                            break;
                        }
                    } else {
                        // 任何控制/输入/传输事件都算链路活跃（见上）。
                        link_active = true;
                    }
                }
                if !link_active {
                    break; // 静默：poll 超时返回空
                }
            }
        }
        if let Some(p) = self.pipeline.as_mut() {
            p.finish_in_flight();
            p.shutdown();
        }
        self.tool_rx.clear();
        self.audio_ctrl_rx = None;
        self.audio_rx.clear();
        self.closed_reason = Some(reason);
        self.state = SessionState::Closed;
        self.transport = None;
    }

    /// 组装运行摘要。
    fn summary(&self, stop_reason: StopReason) -> ClientRunSummary {
        let (displayed, unfinished) = match &self.pipeline {
            Some(p) => (
                self.prev_pipelines_displayed.saturating_add(p.displayed()),
                p.trace().summary().unfinished,
            ),
            None => (self.prev_pipelines_displayed, 0),
        };
        ClientRunSummary {
            stop_reason,
            frames_displayed: displayed,
            input_bytes_sent: self.input_bytes_total,
            idr_requests_sent: self.idr_requests_total,
            unfinished_traces: unfinished,
        }
    }
}

/// 控制分发结果。
enum ControlFlow {
    /// 已处理（含 S8 的忽略路径）。
    Handled,
    /// 对端 Bye（reason 透传）。
    Bye(ByeReason),
}

/// Reconnecting 期一拍驱动的迁移结果（[`ClientSession::tick_reconnect`]
/// 返回值，测试断言点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectOutcome {
    /// 退避等待中（`next_attempt_in_ms` 按注入时钟折算，诊断 / 测试用；
    /// 在途连接上等裁定时为 0——等待由 poll 分片承担）。
    Waiting {
        /// 距下一次尝试的毫秒数。
        next_attempt_in_ms: u64,
    },
    /// 本拍发起了一次 connect（结果在后续拍 settle）。
    Connecting,
    /// resume 成功：管线复用（FrameSeq 延续），已发 IdrRequest{reason=
    /// ReconnectRecovery}，回 Streaming。
    Resumed,
    /// resume 被拒 / Bye(Refresh)：全新 Hello 已裁定，管线已重建、双端
    /// 时钟纪元已归零（新 Welcome bit1=1 已校验）。
    Rebuilt,
    /// 退避次数用尽（或重连等待期收到 Bye），会话已按红线关闭
    ///（rc-cli 按退出码处理，属 T18）。
    GaveUp,
}

/// 忽略标签（S8 限流日志用；client 视角）。ProbePong（T15）与 IdrAck
/// （T16）已接入分发臂，不再走忽略路径。
fn ignore_tag_client(m: &ControlMessage) -> &'static str {
    match m {
        ControlMessage::Hello { .. } => "Hello(方向错误)",
        ControlMessage::Welcome { .. } => "Welcome(streaming 中重复到达)",
        ControlMessage::IdrRequest { .. } => "IdrRequest(方向错误)",
        ControlMessage::ProbePing { .. } => "ProbePing(T17+ host 主动发起)",
        ControlMessage::SetBitrate { .. } => "SetBitrate(方向错误)",
        ControlMessage::BwProbeRequest { .. } => "BwProbeRequest(方向错误)",
        ControlMessage::BwProbeData { .. } => "BwProbeData(走 datagram 通道)",
        // InputAck（T18）已接入分发臂；此分支不再可达。
        ControlMessage::InputAck { .. } => "InputAck(已接入，此分支不可达)",
        ControlMessage::ReconnectRequest { .. } => "ReconnectRequest(方向错误)",
        ControlMessage::ClipboardNotify { .. } => "ClipboardNotify(非 Streaming 态到达，忽略)",
        ControlMessage::ToolChannel { .. } => "ToolChannel(非 Streaming 态到达，忽略)",
        ControlMessage::AudioFrame { .. } => "AudioFrame(非 Streaming 态到达，忽略)",
        _ => "未知方向消息",
    }
}

/// 一次会话的探测结论（纯数据，报告/诊断用；T18 嵌入 client 会话报告
/// JSON 的 session 段）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProbeSnapshot {
    /// RTT 中位数（微秒；无样本 = None = 「无数据」）。
    pub rtt_us_median: Option<u64>,
    /// 时钟偏移中位数（微秒；对端 − 本端；无样本 = None）。
    pub offset_us_median: Option<i64>,
    /// v0 粗测估速（None = 未发起 / 退化 / 未结算；B10：粗 heuristic，
    /// M3-3 拥塞控制接管后废弃）。
    pub bw_estimate_bps: Option<u64>,
    /// 探测后实际下发的初始码率（None = 保持协商值）。
    pub initial_bitrate_bps: Option<u32>,
    /// Welcome 裁定的协商码率（对照用）。
    pub negotiated_bitrate_bps: u32,
    /// 累计发出的 ProbePing 数。
    pub pings_sent: u64,
    /// 累计收到的 ProbePong 数。
    pub pongs_received: u64,
    /// 超时判失的探测数。
    pub probes_lost: u64,
}

/// 一拍的摘要（[`ClientSession::tick`] 返回值）。
///
/// 注：任务卡给本结构标了 `Copy`，但 `closed: Option<StopReason>` 含
/// `String`（`StopReason::Disconnected`）不可 `Copy`——按实际字段降为
/// `Clone`（任务卡 API 契约与 StopReason 定义自相矛盾处的最小修正）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClientTickReport {
    /// 本拍推入重组的媒体数据报数。
    pub datagrams_pushed: usize,
    /// 本拍新上屏帧数。
    pub frames_displayed_delta: u64,
    /// 本拍发出的输入消息字节数。
    pub input_bytes_sent: usize,
    /// 本拍发出的 IdrRequest 条数（0/1）。
    pub idr_requests_sent: usize,
    /// 本拍有任何事件/产出。
    pub busy: bool,
    /// 本拍内进入 Closed 的原因（停机序列已完成）。
    pub closed: Option<StopReason>,
}

/// 会话运行摘要（[`ClientSession::close`]/[`ClientSession::run`] 返回值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRunSummary {
    /// 结束原因。
    pub stop_reason: StopReason,
    /// 全程上屏帧数。
    pub frames_displayed: u64,
    /// 全程发出的输入消息字节数。
    pub input_bytes_sent: u64,
    /// 全程发出的 IdrRequest 条数。
    pub idr_requests_sent: u64,
    /// 停机后 trace 未结帧数，必须为 0（S5/S6 断言）。
    pub unfinished_traces: u64,
}

// ─────────────────────────────────────────────────────────────────
// 测试：全部离线确定性（mock transport + 假后端，无网络、无 sleep）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{HostFlags, PROTOCOL_VERSION};
    use crate::testkit::MockTransport;
    use rc_decode::{FakeDecoder, FakeDecoderConfig};
    use rc_input::{InputEvent, MouseButton};
    use rc_net::NetEvent;
    use rc_proto::{split_frame, MAX_PAYLOAD};
    use rc_render::{FakeRenderer, FakeRendererConfig};
    use rc_types::Timestamp;
    use rc_types::{FrameKind, FrameSeq};

    /// 请求参数（host caps 对齐 64×64@30、1Mbps）。
    fn req() -> SessionParams {
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
            diff: rc_pipeline::DiffMode::TrustPlatform,
            clock_origin_us: None,
            // 控制端不分片（split_frame 只在被控端），模板默认值即可。
            max_packet_payload: rc_proto::MAX_PAYLOAD,
        }
    }

    /// 脚本型输入捕获：按队列一次性吐事件。
    struct ScriptedCapture {
        scripted: Vec<InputEvent>,
    }

    impl rc_input::InputCapture for ScriptedCapture {
        fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
            Ok(std::mem::take(&mut self.scripted))
        }
    }

    fn cfg(scripted: Vec<InputEvent>) -> ClientSessionConfig {
        ClientSessionConfig {
            peer_addr: "127.0.0.1:9901".parse().unwrap(),
            mtu: DEFAULT_MTU,
            requested: req(),
            session_id: 7,
            pipeline: pcfg(),
            connect_timeout: Duration::from_secs(1),
            handshake_timeout: Duration::from_millis(500),
            capture: Box::new(ScriptedCapture { scripted }),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: ClientAuthConfig::default(),
        }
    }

    fn zero_clock() -> SessionClock {
        Arc::new(|| 0)
    }

    fn fake_decoder(drop_first_n: u32) -> Box<dyn Decoder> {
        Box::new(
            FakeDecoder::new(FakeDecoderConfig {
                resolution: rc_types::Resolution::new(64, 64),
                drop_first_n,
                ..FakeDecoderConfig::default()
            })
            .expect("fake decoder"),
        )
    }

    fn fake_renderer() -> Box<dyn Renderer> {
        Box::new(
            FakeRenderer::new(FakeRendererConfig {
                size: (64, 64),
                ..FakeRendererConfig::default()
            })
            .expect("fake renderer"),
        )
    }

    /// 一条合法 Welcome（可定制 granted/session_id/auth）。
    fn welcome_msg(
        session_id: u64,
        granted: SessionParams,
        auth: proto::AuthStatus,
    ) -> ControlMessage {
        ControlMessage::Welcome(proto::Welcome {
            session_id,
            granted,
            auth_status: auth,
            host_flags: HostFlags(0),
        })
    }

    /// 预置 Welcome 并完成握手的会话（返回会话与传输句柄）。
    fn handshaken(scripted: Vec<InputEvent>) -> (ClientSession, MockTransport) {
        handshaken_with(welcome_msg(7, req(), proto::AuthStatus::Ok), scripted)
    }

    fn handshaken_with(
        welcome: ControlMessage,
        scripted: Vec<InputEvent>,
    ) -> (ClientSession, MockTransport) {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome);
        let mut handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg(scripted),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        client.handshake().expect("handshake");
        let _ = &mut handle;
        (client, handle)
    }

    /// 手工造一个媒体数据报（单包帧；rc-proto 的 split_frame）。
    fn media_datagram(seq: u64, kind: FrameKind, ts: u32) -> Vec<u8> {
        let payload = vec![7u8; 64];
        split_frame(
            FrameSeq(seq),
            kind,
            Timestamp(ts),
            Timestamp(ts + 500),
            0,
            &payload,
            MAX_PAYLOAD,
        )
        .remove(0)
    }

    /// S3：Hello 字段正确（version/session_id/请求参数）；Welcome 裁定值
    /// 覆盖 requested；state==Streaming。
    #[test]
    fn client_handshake_sends_hello_and_adopts_welcome_params() {
        let granted = SessionParams {
            width: 32,
            height: 32,
            fps: 24,
            bitrate_bps: 500_000,
        };
        let (client, handle) =
            handshaken_with(welcome_msg(7, granted, proto::AuthStatus::Ok), Vec::new());
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.negotiated(), Some(granted), "以 Welcome 裁定值为准");
        assert_ne!(client.negotiated(), Some(req()), "requested 不生效");

        let msgs = handle.reliable_decoded();
        assert_eq!(msgs.len(), 1, "握手只发一条 Hello");
        match &msgs[0] {
            ControlMessage::Hello(h) => {
                assert_eq!(h.version, PROTOCOL_VERSION);
                assert_eq!(h.session_id, 7);
                assert_eq!(h.requested, req());
                assert_eq!(h.auth_method, proto::AuthMethod::None);
                assert!(h.auth_payload.is_empty());
            }
            other => panic!("应发 Hello，实际 {other:?}"),
        }
    }

    /// S3：回显不符 → Err、断开。
    #[test]
    fn client_rejects_welcome_with_wrong_session_id() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(999, req(), proto::AuthStatus::Ok));
        let mut client = ClientSession::with_parts(
            cfg(Vec::new()),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        let err = client.handshake().expect_err("回显不符必须断");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert!(err.to_string().contains("999"), "{err}");
        assert_eq!(client.state(), SessionState::Closed, "断开");
    }

    /// S3：auth_status=2 → Err、不建管线。
    #[test]
    fn client_rejects_welcome_with_auth_denied() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Rejected));
        let mut client = ClientSession::with_parts(
            cfg(Vec::new()),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        let err = client.handshake().expect_err("auth 拒绝必须断");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(client.state(), SessionState::Closed);
        assert!(client.negotiated().is_none(), "不建管线、无裁定值");
    }

    /// S4/S5：脚本捕获点击（关键事件立即成批）→ urgent_sent 非空、
    /// reliable_sent 无输入消息。
    #[test]
    fn client_sends_input_over_urgent_stream_only() {
        let (mut client, handle) = handshaken(vec![InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }]);
        let now = Instant::now();
        let r = client.tick(Duration::from_millis(0), now).expect("tick");
        assert!(r.input_bytes_sent > 0, "点击应即刻成批发出");
        assert!(!handle.urgent_sent().is_empty(), "输入必须走 urgent 流");
        // reliable 流上只允许控制消息（握手期的 Hello；FakeDecoder 在解出
        // 首个 IDR 前 needs_keyframe 恒真，首拍直发一条 IdrRequest 也是
        // 控制消息——controller.rs take_keyframe_request 的正确行为），
        // 绝不出现输入字节（S4 通道纪律）。
        for m in &handle.reliable_decoded() {
            assert!(
                matches!(
                    m,
                    ControlMessage::Hello(_) | ControlMessage::IdrRequest { .. }
                ),
                "reliable 流只允许控制消息，发现 {m:?}（输入退回共享流 = 队头阻塞回归）"
            );
        }
        assert!(
            handle.reliable_sent().iter().all(|m| m.len() <= 32),
            "输入事件段（含点击）绝不在 reliable 流上出现"
        );
    }

    /// S5 基本接线：FakeDecoderConfig{drop_first_n: 1} 丢首 IDR → P 帧触发
    /// DecoderNeedKeyframe → IdrRequest{DecoderNeedKeyframe} 发往 reliable 流。
    #[test]
    fn client_requests_idr_when_decoder_needs_keyframe() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg(Vec::new()),
            Box::new(mock),
            fake_decoder(1), // 定点断链：丢首帧 IDR
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        client.handshake().expect("handshake");

        // 帧 1 = IDR（被 drop_first_n 吞）、帧 2 = P（缺参考 → NeedKeyframe）。
        let mut handle = handle;
        handle.deliver(NetEvent::Datagram(media_datagram(1, FrameKind::Idr, 0)));
        handle.deliver(NetEvent::Datagram(media_datagram(2, FrameKind::P, 33_334)));
        let now = Instant::now();
        let r = client.tick(Duration::from_millis(0), now).expect("tick");
        assert_eq!(r.datagrams_pushed, 2);
        assert_eq!(r.frames_displayed_delta, 0, "断链下无上屏");

        // 拍末 take_keyframe_request 为真 → IdrRequest 已发 reliable 流。
        assert_eq!(r.idr_requests_sent, 1);
        let msgs = handle.reliable_decoded();
        assert!(
            matches!(
                msgs.last(),
                Some(ControlMessage::IdrRequest {
                    reason: proto::IdrReason::DecoderNeedKeyframe
                })
            ),
            "应发 IdrRequest{{DecoderNeedKeyframe}}，实际 {msgs:?}"
        );
    }

    /// S6：Bye 发出、残帧判 LostInNetwork、unfinished==0、state==Closed。
    #[test]
    fn client_close_sends_bye_and_finishes_in_flight() {
        let (mut client, mut handle) = handshaken(Vec::new());
        // 半帧：两包帧只投第 0 包 → 重组表残留 partial。
        let mut payload = vec![0u8; 200];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = i as u8;
        }
        let pkts = split_frame(
            FrameSeq(3),
            FrameKind::Idr,
            Timestamp(0),
            Timestamp(500),
            0,
            &payload,
            128,
        );
        assert_eq!(pkts.len(), 2, "前提：200 字节按 128 分片为 2 包");
        handle.deliver(NetEvent::Datagram(pkts[0].clone()));
        let now = Instant::now();
        client.tick(Duration::from_millis(0), now).expect("tick");

        let summary = client.close(ByeReason::Normal).expect("close");
        assert_eq!(client.state(), SessionState::Closed);
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
        let s = client.pipeline().trace().summary();
        assert_eq!(s.lost_in_network, 1, "残帧判 LostInNetwork");
        assert_eq!(s.unfinished, 0, "finish_in_flight 后无未结帧");
        assert_eq!(summary.unfinished_traces, 0);
        assert_eq!(summary.stop_reason, StopReason::Requested);
    }

    /// 收 Bye → tick 报告 closed(StopReason::ByeReceived(Normal))，停机完整。
    #[test]
    fn client_stops_on_bye_from_host() {
        let (mut client, mut handle) = handshaken(Vec::new());
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Normal,
        });
        let now = Instant::now();
        let r = client.tick(Duration::from_millis(0), now).expect("tick");
        assert_eq!(
            r.closed,
            Some(StopReason::ByeReceived(ByeReason::Normal)),
            "应报告 ByeReceived"
        );
        assert_eq!(client.state(), SessionState::Closed);
        assert_eq!(
            client.pipeline().trace().summary().unfinished,
            0,
            "停机完整"
        );
    }

    /// S1：Closed 后再 tick 返回 Err（非法迁移）。
    #[test]
    fn client_tick_after_close_is_rejected() {
        let (mut client, _) = handshaken(Vec::new());
        client.close(ByeReason::Normal).expect("close");
        let err = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect_err("Closed 后 tick 必须报错（S1）");
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
    }

    // ─────────────────────────────────────────────────────────────
    // T17：断线重连（Reconnecting / 指数退避 / 强制 IDR / 刷新重建）
    // ─────────────────────────────────────────────────────────────

    /// 指定 host_flags 的合法 Welcome 线编码（重连裁定断言用）。
    fn welcome_with_flags(session_id: u64, flags: u8) -> ControlMessage {
        ControlMessage::Welcome(proto::Welcome {
            session_id,
            granted: req(),
            auth_status: proto::AuthStatus::Ok,
            host_flags: proto::HostFlags(flags),
        })
    }

    /// 脚本化管线工厂：每次建一个全假后端的新 ControllerPipeline
    ///（Rebuilt 路径的确定性重建；时钟由会话统一 set_clock）。
    fn controller_factory() -> Box<dyn FnMut() -> Result<ControllerPipeline> + Send> {
        Box::new(|| {
            let p = ControllerPipeline::with_backends(pcfg(), fake_decoder(0), fake_renderer())?;
            Ok(p)
        })
    }

    /// 计数 connect 工厂：前 `fail_first_n` 次返回 Err（模拟对端不可达）；
    /// 之后每次返回一个预置 `preload` 消息的 MockTransport（句柄留存供
    /// 断言）。返回 (工厂, 创建的 transport 句柄表, 调用计数)。
    fn scripted_connect_factory(
        fail_first_n: u32,
        preload: Vec<ControlMessage>,
    ) -> (
        ConnectFactory,
        std::sync::Arc<std::sync::Mutex<Vec<MockTransport>>>,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = std::sync::Arc::new(AtomicU32::new(0));
        let handles: std::sync::Arc<std::sync::Mutex<Vec<MockTransport>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls2 = std::sync::Arc::clone(&calls);
        let handles2 = std::sync::Arc::clone(&handles);
        let f = Box::new(move || {
            let n = calls2.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= fail_first_n {
                return Err(Error::Network(format!(
                    "attempt {n}: unreachable (scripted)"
                )));
            }
            let mut mock = MockTransport::new();
            for m in &preload {
                mock.deliver_reliable(m);
            }
            handles2.lock().expect("factory handles").push(mock.clone());
            Ok(Box::new(mock) as Box<dyn Transport>)
        });
        (f, handles, calls)
    }

    /// R3/R4：Disconnected → Reconnecting；注入推进 0.5/1.5/3.5s 各恰有
    /// 一次 connect 尝试（不多不少）；ReconnectRequest 带原 session_id；
    /// Welcome bit0=1 → Resumed 且**立即**发出 IdrRequest{reason=2}。
    #[test]
    fn client_disconnect_reconnects_with_backoff_and_forces_idr() {
        use std::sync::atomic::Ordering;
        let (mut client, mut handle) = handshaken(Vec::new());
        // 工厂：前两次失败，第三次成功并预置 Welcome{bit0=1, 回显 sid=7}。
        let (factory, handles, calls) =
            scripted_connect_factory(2, vec![welcome_with_flags(7, 0b01)]);
        client.set_connect_factory(factory);
        client
            .set_reconnect(crate::reconnect::ReconnectConfig::default())
            .expect("默认配置合法");

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.state(), SessionState::Reconnecting);
        assert_eq!(client.reconnect_stats().disconnects, 1);

        // 0.5s 前：无尝试（退避等待；400ms 处剩余 100ms）。
        assert!(matches!(
            client.tick_reconnect(400_000).expect("tick_reconnect"),
            ReconnectOutcome::Waiting {
                next_attempt_in_ms: 100
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // 0.5s：第 1 次尝试（失败 → 退避 1s）。
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Waiting {
                next_attempt_in_ms: 1_000
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // 0.5s+1ms：未到 1.5s，不再尝试（不多不少）。
        assert!(matches!(
            client.tick_reconnect(501_000).expect("tick_reconnect"),
            ReconnectOutcome::Waiting { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // 1.5s：第 2 次尝试（失败 → 退避 2s）；到 3.5s 前不再尝试。
        assert!(matches!(
            client.tick_reconnect(1_500_000).expect("tick_reconnect"),
            ReconnectOutcome::Waiting {
                next_attempt_in_ms: 2_000
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            client.tick_reconnect(3_499_999).expect("tick_reconnect"),
            ReconnectOutcome::Waiting { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // 3.5s：第 3 次尝试（成功）→ Connecting；ReconnectRequest 带原 sid。
        assert!(matches!(
            client.tick_reconnect(3_500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let t = handles.lock().expect("handles")[0].clone();
        assert_eq!(
            t.reliable_decoded(),
            vec![ControlMessage::ReconnectRequest {
                session_id: 7,
                resume: true
            }],
            "重连请求必须带原 session_id 且 resume=1"
        );

        // 下一拍收到 Welcome{bit0=1} → Resumed 且立即发 IdrRequest{reason=2}。
        assert!(matches!(
            client.tick_reconnect(3_500_000).expect("tick_reconnect"),
            ReconnectOutcome::Resumed
        ));
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.reconnect_stats().resumes, 1);
        assert_eq!(client.reconnect_stats().reconnect_attempts, 3);
        assert_eq!(
            t.reliable_decoded(),
            vec![
                ControlMessage::ReconnectRequest {
                    session_id: 7,
                    resume: true
                },
                ControlMessage::IdrRequest {
                    reason: IdrReason::ReconnectRecovery
                },
            ],
            "resume 命中后必须立即发 IdrRequest{{reason=2}}（R4）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// T18 恢复计时的同拍突发竞态（门禁第 2 轮全仓测试偶发失败根因）：
    /// resume 后强制的 IDR 与更晚的 delta 在同一拍批量凑齐时，
    /// `last_complete` 被 delta 覆盖——恢复判定必须扫完成历史取 since
    /// 之后的首个完整 IDR，否则 recovery_ms 竞态性挂死（e2e 表现：重连
    /// 演练 Gate 报「重连恢复时长未测得（recovery_ms 为 null）」）。
    #[test]
    fn client_recovery_measured_when_burst_overwrites_idr_same_tick() {
        let (clock, clock_v) = mutable_clock();
        let (mut client, mut handle) = handshaken_with_clock(
            clock,
            Box::new(ScriptedCapture {
                scripted: Vec::new(),
            }),
        );
        let (factory, handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(7, 0b01)]);
        client.set_connect_factory(factory);
        client
            .set_reconnect(crate::reconnect::ReconnectConfig::default())
            .expect("默认配置合法");

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.state(), SessionState::Reconnecting);

        // 0.5s：第 1 次尝试即成功 → Connecting；下一拍 Welcome{bit0=1}
        // → Resumed，恢复计时起点 since = 500_000µs。
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Resumed
        ));
        assert!(
            client.reconnect_recovery_ms().is_none(),
            "IDR 未到不得提前结"
        );

        // 同拍突发：IDR（seq 1）与更晚的 delta（seq 2）在同一拍到达凑齐
        //（负载下的真实形态：client 被调度延迟一拍排空多帧）。
        let mut t = handles.lock().expect("handles")[0].clone();
        t.deliver(NetEvent::Datagram(media_datagram(
            1,
            FrameKind::Idr,
            500_000,
        )));
        t.deliver(NetEvent::Datagram(media_datagram(2, FrameKind::P, 516_667)));
        *clock_v.lock().expect("clock") = 550_000;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        // 此刻 last_complete 已是 delta（tracker 层测试
        // tracker_first_idr_since_finds_idr_under_same_tick_burst 锁死该
        // 形态）：只看最新一帧的旧实现在此永远等不到 IDR。
        assert_eq!(
            client.reconnect_recovery_ms(),
            Some(50),
            "突发覆盖下仍须测得恢复时长（IDR 完成于 since 后 50ms）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// R2：Welcome bit0=0 → 同连接补发带**新** session_id 的 Hello、
    /// 经工厂重建管线（generation+1）、T15 偏移估计清零（纪元重建）。
    #[test]
    fn client_resume_rejected_falls_back_to_fresh_hello() {
        let (mut client, mut handle) = handshaken(Vec::new());
        client.set_pipeline_factory(controller_factory());
        // 先攒一个偏移样本（重连后必须被清零）。pump 首拍只锚定起点，
        // 第二拍（+1s）才发出第一条 ProbePing。
        client.pump_probes(0).expect("pump_probes");
        client.pump_probes(1_000_000).expect("pump_probes");
        handle.deliver_reliable(&ControlMessage::ProbePong {
            probe_id: 1,
            echoed_clock_us: 0,
            responder_clock_us: 100,
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(
            client.probe_snapshot().offset_us_median.is_some(),
            "前提：重建前已有偏移样本（值无关，只要非 None）"
        );

        // 工厂：一次成功，预置 Welcome{bit0=0, 回显旧 sid}。
        let (factory, handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(7, 0)]);
        client.set_connect_factory(factory);
        let gen = client.pipeline_generation();

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        // Welcome bit0=0 → 同连接降级：补发全新 Hello（新 sid=8）。
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Waiting {
                next_attempt_in_ms: 0
            }
        ));
        let mut t = handles.lock().expect("handles")[0].clone();
        assert_eq!(
            t.reliable_decoded(),
            vec![
                ControlMessage::ReconnectRequest {
                    session_id: 7,
                    resume: true
                },
                ControlMessage::Hello(proto::Hello {
                    version: proto::PROTOCOL_VERSION,
                    session_id: 8,
                    requested: req(),
                    auth_method: proto::AuthMethod::None,
                    auth_payload: vec![],
                }),
            ],
            "bit0=0 后必须同连接补发带新 session_id 的全新 Hello"
        );
        assert_eq!(
            client.state(),
            SessionState::Reconnecting,
            "等第二条 Welcome"
        );

        // 第二条 Welcome（回显新 sid）→ 工厂重建管线 → Rebuilt。
        t.deliver_reliable(&welcome_with_flags(8, 0));
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Rebuilt
        ));
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.session_id(), 8);
        assert_eq!(client.pipeline_generation(), gen + 1);
        assert_eq!(client.reconnect_stats().rebuilds, 1);
        assert_eq!(
            client.probe_snapshot().offset_us_median,
            None,
            "纪元重建后偏移估计必须清零（坑 10）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// R3：connect 恒失败 → 第 5 次后 GaveUp、状态 Closed、
    /// reconnect_attempts == 5。
    #[test]
    fn client_gives_up_after_max_attempts() {
        use std::sync::atomic::Ordering;
        let (mut client, mut handle) = handshaken(Vec::new());
        let (factory, _handles, calls) = scripted_connect_factory(u32::MAX, Vec::new());
        client.set_connect_factory(factory);

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");

        // 尝试时刻序列（0.5/1.5/3.5/7.5/15.5s）：每次到点恰一次尝试。
        let due = [500_000u64, 1_500_000, 3_500_000, 7_500_000, 15_500_000];
        let mut gave_up_at = None;
        for (i, &t) in due.iter().enumerate() {
            let outcome = client.tick_reconnect(t).expect("tick_reconnect");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                i as u32 + 1,
                "第 {} 个到点时刻恰一次尝试",
                i + 1
            );
            if matches!(outcome, ReconnectOutcome::GaveUp) {
                gave_up_at = Some(i);
                break;
            }
        }
        assert_eq!(gave_up_at, Some(4), "第 5 次尝试失败即放弃");
        assert_eq!(client.state(), SessionState::Closed);
        assert_eq!(client.reconnect_stats().reconnect_attempts, 5);
    }

    /// R6：Streaming 中收到 Bye{Refresh} → 下一拍即发全新 Hello（零退避，
    /// 断言与 Bye 同时刻就 connect）；Welcome bit1=1 后偏移估计已清零、
    /// 管线经工厂重建。
    #[test]
    fn client_bye_refresh_rebuilds_without_backoff() {
        let (mut client, mut handle) = handshaken(Vec::new());
        client.set_pipeline_factory(controller_factory());
        client.pump_probes(0).expect("pump_probes");
        client.pump_probes(1_000_000).expect("pump_probes");
        handle.deliver_reliable(&ControlMessage::ProbePong {
            probe_id: 1,
            echoed_clock_us: 0,
            responder_clock_us: 100,
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(client.probe_snapshot().offset_us_median.is_some());

        // 工厂预置 Welcome{bit0=0, bit1=1, 回显新 sid=8}。
        let (factory, handles, calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(8, 0b10)]);
        client.set_connect_factory(factory);
        let gen = client.pipeline_generation();
        let old_sid = client.session_id();

        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Refresh,
        });
        let r = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(r.closed.is_none(), "Bye(Refresh) 不关闭会话（T17 接管）");
        assert_eq!(client.state(), SessionState::Reconnecting);
        assert_eq!(client.reconnect_stats().refreshes, 1);

        // 零退避：同一时刻（now=0）下一拍即 connect + 全新 Hello。
        use std::sync::atomic::Ordering;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "前提：尚未 connect");
        assert!(matches!(
            client.tick_reconnect(0).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "零退避：t=0 即尝试");
        let t = handles.lock().expect("handles")[0].clone();
        match &t.reliable_decoded()[0] {
            ControlMessage::Hello(h) => {
                assert_eq!(h.session_id, 8, "全新 Hello 带新 session_id");
                assert_ne!(h.session_id, old_sid);
            }
            other => panic!("刷新路径必须发全新 Hello，实际 {other:?}"),
        }

        // Welcome{bit1=1} → Rebuilt：偏移清零、管线代数 +1。
        assert!(matches!(
            client.tick_reconnect(0).expect("tick_reconnect"),
            ReconnectOutcome::Rebuilt
        ));
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.session_id(), 8);
        assert_eq!(client.pipeline_generation(), gen + 1);
        assert_eq!(client.probe_snapshot().offset_us_median, None);
        assert_eq!(client.reconnect_stats().rebuilds, 1);
        client.close(ByeReason::Normal).expect("close");
    }

    /// R8：Reconnecting 期间注入时钟推进 10s，无任何 IdrRequest 发出
    ///（看门狗被状态门禁；tick/pump_probes 均要求 Streaming）。
    #[test]
    fn watchdog_silent_while_reconnecting() {
        use std::sync::atomic::Ordering;
        let (mut client, mut handle) = handshaken(Vec::new());
        let (factory, _handles, calls) = scripted_connect_factory(u32::MAX, Vec::new());
        client.set_connect_factory(factory);

        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.state(), SessionState::Reconnecting);

        // 断线时刻起的所有发出记录为基线（含 Streaming 期的合法消息）。
        let baseline = handle.reliable_sent().len();
        for step in 0..100u64 {
            let _ = client
                .tick_reconnect(step * 100_000)
                .expect("tick_reconnect");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "10s 内恰 4 次尝试（0.5/1.5/3.5/7.5s），第 5 次在 15.5s"
        );
        assert_eq!(client.state(), SessionState::Reconnecting, "仍在重连");
        assert_eq!(
            handle.reliable_sent().len(),
            baseline,
            "重连期不得发出任何消息（含 IdrRequest，R8）"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // 修复卡 B：评审发现修复的锁定测试（发现编号在各测试文档注释）
    // ─────────────────────────────────────────────────────────────

    /// 可中途装填的队列捕获器（修复卡 B-2 测试用：不同时刻发不同批）。
    struct QueueCapture {
        q: std::sync::Arc<std::sync::Mutex<Vec<InputEvent>>>,
    }

    impl rc_input::InputCapture for QueueCapture {
        fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
            Ok(std::mem::take(&mut *self.q.lock().expect("queue capture")))
        }
    }

    /// 恒 Err 的捕获器（修复卡 B-3 测试用）。
    struct FailingCapture;

    impl rc_input::InputCapture for FailingCapture {
        fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
            Err(Error::Network("capture boom (scripted)".into()))
        }
    }

    /// 前 `ok_polls_left` 次 poll 正常、之后恒 Err 的传输（修复卡 B-1
    /// poll 错误路径的确定性触发器）。
    struct PollFailTransport {
        inner: MockTransport,
        ok_polls_left: u32,
    }

    impl Transport for PollFailTransport {
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
            if self.ok_polls_left == 0 {
                return Err(Error::Network("poll boom (scripted)".into()));
            }
            self.ok_polls_left -= 1;
            self.inner.poll(timeout)
        }
        fn stats(&self) -> rc_net::NetStats {
            self.inner.stats()
        }
    }

    /// 以指定捕获器构造 cfg（其余与 [`cfg`] 一致）。
    fn cfg_with_capture(capture: Box<dyn rc_input::InputCapture>) -> ClientSessionConfig {
        ClientSessionConfig {
            capture,
            ..cfg(Vec::new())
        }
    }

    /// 可变注入钟（µs，u32 会话钟口径；修复卡 B-2 测试用）。
    fn mutable_clock() -> (SessionClock, std::sync::Arc<std::sync::Mutex<u32>>) {
        let v = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let v2 = std::sync::Arc::clone(&v);
        (Arc::new(move || *v2.lock().expect("clock")), v)
    }

    /// 以自定义钟 + 捕获器完成握手（修复卡 B-2 测试底座）。
    fn handshaken_with_clock(
        clock: SessionClock,
        capture: Box<dyn rc_input::InputCapture>,
    ) -> (ClientSession, MockTransport) {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg_with_capture(capture),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            clock,
        )
        .expect("with_parts");
        client.handshake().expect("handshake");
        (client, handle)
    }

    /// 建立偏移为 0 的探测基准（to_local_us(remote) = remote 恒等；修复卡
    /// B-2 测试的 ack 换算前提）。`ping_at_us` ≥ 1s（探测间隔）。
    fn arm_zero_offset(
        client: &mut ClientSession,
        handle: &mut MockTransport,
        clock: &std::sync::Arc<std::sync::Mutex<u32>>,
        ping_at_us: u32,
    ) {
        client.pump_probes(0).expect("pump_probes 锚定");
        *clock.lock().expect("clock") = ping_at_us;
        client
            .pump_probes(u64::from(ping_at_us))
            .expect("pump_probes 发 ping");
        // t2 = ping_at+2、responder = ping_at+1 → NTP 中点 offset = 0。
        handle.deliver_reliable(&ControlMessage::ProbePong {
            probe_id: 1,
            echoed_clock_us: u64::from(ping_at_us),
            responder_clock_us: u64::from(ping_at_us) + 1,
        });
        *clock.lock().expect("clock") = ping_at_us + 2;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick 消费 pong");
        assert_eq!(
            client.probe_snapshot().offset_us_median,
            Some(0),
            "前提：偏移基准为 0（to_local_us = 恒等）"
        );
    }

    /// 一次左键点击（关键事件，必成关键批）。
    fn click() -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }
    }

    /// 修复卡 B-1（发现 1，med）：1 字节 0x09 数据报（探测块线格式截断，
    /// 对端可触发）→ 完整停机后再返回 Err——不再 `?` 裸上抛卡 Streaming。
    #[test]
    fn client_malformed_bw_datagram_shuts_down_before_err() {
        let (mut client, mut handle) = handshaken(Vec::new());
        handle.deliver(NetEvent::Datagram(vec![proto::TYPE_BW_PROBE_DATA]));
        let err = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect_err("脏探测块必须 Err");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(
            client.state(),
            SessionState::Closed,
            "完整停机（尾部 stop_with）而非裸抛卡 Streaming"
        );
        assert_eq!(
            client.pipeline().trace().summary().unfinished,
            0,
            "管线收口（finish_in_flight 后无未结帧）"
        );
        // 停机后按非法迁移拒绝（S1），不会继续消费任何事件。
        assert!(client
            .tick(Duration::from_millis(0), Instant::now())
            .is_err());
    }

    /// 修复卡 B-1（发现 1，med）：重连裁定期的 Welcome session_id 不符 →
    /// 完整停机后 Err（原先 validate_welcome 的 `?` 裸上抛，状态晾在
    /// Reconnecting）。
    #[test]
    fn client_reconnect_welcome_mismatch_shuts_down_before_err() {
        let (mut client, mut handle) = handshaken(Vec::new());
        // 工厂一次成功，预置回显错误 session_id 的 Welcome{bit0=1}。
        let (factory, _handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(999, 0b01)]);
        client.set_connect_factory(factory);
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        let err = client.tick_reconnect(500_000).expect_err("回显不符必须断");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert!(err.to_string().contains("999"), "{err}");
        assert_eq!(client.state(), SessionState::Closed, "完整停机");
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);
    }

    /// 修复卡 B-1（发现 1，补）：全新重建等待期（AwaitingFresh）的 Welcome
    /// 回显不符 → 同一完整停机纪律（该臂的 validate_welcome 原先同样裸上抛）。
    #[test]
    fn client_rebuild_welcome_mismatch_shuts_down_before_err() {
        let (mut client, mut handle) = handshaken(Vec::new());
        client.set_pipeline_factory(controller_factory());
        let (factory, _handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(999, 0b10)]);
        client.set_connect_factory(factory);
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Refresh,
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(matches!(
            client.tick_reconnect(0).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        let err = client.tick_reconnect(0).expect_err("回显不符必须断");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(client.state(), SessionState::Closed, "完整停机");
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);
    }

    /// 修复卡 B-1（发现 1，med）：Streaming 期 transport.poll 错误 →
    /// 完整停机后返回 Err（原先 `?` 裸上抛，状态卡 Streaming）。
    #[test]
    fn client_tick_poll_error_shuts_down_before_err() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let _handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg(Vec::new()),
            Box::new(PollFailTransport {
                inner: mock,
                ok_polls_left: 1, // 握手耗掉唯一一次正常 poll
            }),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        client.handshake().expect("handshake（仅耗 1 次 poll）");
        let err = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect_err("poll 错误必须 Err");
        assert!(matches!(err, Error::Network { .. }), "{err}");
        assert_eq!(
            client.state(),
            SessionState::Closed,
            "完整停机（尾部 stop_with）而非裸抛卡 Streaming"
        );
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);
    }

    /// 修复卡 B-1（发现 1，med）：Reconnecting 在途连接的 poll 错误 →
    /// 完整停机后返回 Err（原先 `?` 裸上抛）。
    #[test]
    fn client_reconnect_poll_error_shuts_down_before_err() {
        let (mut client, mut handle) = handshaken(Vec::new());
        let factory: ConnectFactory = Box::new(|| {
            Ok(Box::new(PollFailTransport {
                inner: MockTransport::new(),
                ok_polls_left: 0,
            }) as Box<dyn Transport>)
        });
        client.set_connect_factory(factory);
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(matches!(
            client.tick_reconnect(500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        let err = client
            .tick_reconnect(500_000)
            .expect_err("poll 错误必须 Err");
        assert!(matches!(err, Error::Network { .. }), "{err}");
        assert_eq!(client.state(), SessionState::Closed, "完整停机");
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);
    }

    /// 修复卡 B-2（发现 2，med）：resume 命中时清空 pending_input_acks。
    /// 构造「断线前批 1 未被 host 收到」的序号错位：resume 后 host 按自己
    /// 的计数对新批复 seq=1——若旧表残留（修复前），该回执会错配到断线前
    /// 的发送时刻，产出跨断线的伪单向延迟样本；清空后错位回执无匹配，
    /// 宁缺毋滥（无样本）。
    #[test]
    fn client_resume_clears_pending_input_acks_no_cross_disconnect_sample() {
        let (clock, v) = mutable_clock();
        let q = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = QueueCapture {
            q: std::sync::Arc::clone(&q),
        };
        let (mut client, mut handle) = handshaken_with_clock(clock, Box::new(capture));
        arm_zero_offset(&mut client, &mut handle, &v, 1_000_000);

        // 断线前：t=2_000_000 发关键批（seq=1）——host 从未收到（无回执）。
        *v.lock().expect("clock") = 2_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.pending_input_acks.len(), 1, "前提：批 1 在途");
        assert_eq!(client.key_batch_seq_next, 1);

        // 断线 → resume（Welcome bit0=1，回显 sid=7）。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        let (factory, handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(7, 0b01)]);
        client.set_connect_factory(factory);
        *v.lock().expect("clock") = 2_500_000;
        assert!(matches!(
            client.tick_reconnect(2_500_000).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        *v.lock().expect("clock") = 2_500_001;
        assert!(matches!(
            client.tick_reconnect(2_500_001).expect("tick_reconnect"),
            ReconnectOutcome::Resumed
        ));
        assert!(
            client.pending_input_acks.is_empty(),
            "修复点：resume 命中即清空跨断线在途回执表"
        );

        // resume 后：t=8_000_000 发新关键批（client 序号延续 → seq=2，
        // 与 host 的 resume 延续计数同口径）。
        let mut t2 = handles.lock().expect("handles")[0].clone();
        *v.lock().expect("clock") = 8_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.key_batch_seq_next, 2, "resume 延续计数");

        // host 未见过批 1 → 新批按 host 计数是其第 1 个关键批 → 回 seq=1
        //（偏移 0 ⇒ inject_done 折算后 = 本地 8_100_000）。修复前旧表残留
        // 的 (1, 2_000_000) 会吞掉该回执，产出 6_100_000µs 的跨断线伪样本；
        // 修复后无匹配 → 宁缺毋滥。
        t2.deliver_reliable(&ControlMessage::InputAck {
            key_batch_seq: 1,
            inject_done_us: 8_100_000,
        });
        *v.lock().expect("clock") = 8_100_001;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(
            client.input_one_way_samples_us().is_empty(),
            "修复点：错位回执不得产出跨断线样本（修复前会得到 6_100_000µs）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// 修复卡 B-2（发现 2，med）：resume 后序号对齐（两侧延续计数）的正路
    /// 样本——断线前的批已回执清账，resume 后新批的回执样本只含断线后
    /// 间隔（对新账本，不跨断线）。
    #[test]
    fn client_resume_aligned_ack_yields_fresh_sample() {
        let (clock, v) = mutable_clock();
        let q = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = QueueCapture {
            q: std::sync::Arc::clone(&q),
        };
        let (mut client, mut handle) = handshaken_with_clock(clock, Box::new(capture));
        arm_zero_offset(&mut client, &mut handle, &v, 1_000_000);

        // 断线前批 1 已被 host 回执（清账，不留 pending）。
        *v.lock().expect("clock") = 2_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        handle.deliver_reliable(&ControlMessage::InputAck {
            key_batch_seq: 1,
            inject_done_us: 2_050_000,
        });
        *v.lock().expect("clock") = 2_050_001;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.input_one_way_samples_us(), &[50_000]);

        // 断线 → resume → 新批（seq=2）→ host 延续计数回 seq=2。
        handle.deliver(NetEvent::Disconnected {
            reason: "drop".into(),
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        let (factory, handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(7, 0b01)]);
        client.set_connect_factory(factory);
        // 断线时刻 ≈ 2_050_001，退避 0.5s → 到点时刻推进注入钟再驱动。
        *v.lock().expect("clock") = 2_550_001;
        assert!(matches!(
            client.tick_reconnect(2_550_001).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        assert!(matches!(
            client.tick_reconnect(2_550_001).expect("tick_reconnect"),
            ReconnectOutcome::Resumed
        ));
        let mut t2 = handles.lock().expect("handles")[0].clone();
        *v.lock().expect("clock") = 8_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        t2.deliver_reliable(&ControlMessage::InputAck {
            key_batch_seq: 2,
            inject_done_us: 8_100_000,
        });
        *v.lock().expect("clock") = 8_100_001;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        // 样本 = 新批发送后的 100ms 间隔，绝无跨断线的 6s+ 量级。
        assert_eq!(
            client.input_one_way_samples_us(),
            &[50_000, 100_000],
            "resume 后的新样本只含断线后间隔（两侧延续计数对齐）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// 修复卡 B-2（发现 2，med）：全新重建（Bye(Refresh) 路径）清空
    /// pending_input_acks 且 key_batch_seq_next 归零重计（下一批 = 1），
    /// 对齐 host「全新会话复位 0」的约定口径；旧回执不得错配到新会话
    ///（修复前 seq=1 会命中断线前的旧条目，产出跨重建伪样本）。
    #[test]
    fn client_rebuild_resets_key_batch_accounting() {
        let (clock, v) = mutable_clock();
        let q = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = QueueCapture {
            q: std::sync::Arc::clone(&q),
        };
        let (mut client, mut handle) = handshaken_with_clock(clock, Box::new(capture));
        arm_zero_offset(&mut client, &mut handle, &v, 1_000_000);

        // 重建前：t=2_000_000 发关键批（seq=1，无回执、随连接消亡）。
        *v.lock().expect("clock") = 2_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.pending_input_acks.len(), 1, "前提：旧账在途");

        // Bye(Refresh) → 零退避全新重建（新 sid=8；Welcome 带 bit1=1）。
        client.set_pipeline_factory(controller_factory());
        let (factory, handles, _calls) =
            scripted_connect_factory(0, vec![welcome_with_flags(8, 0b10)]);
        client.set_connect_factory(factory);
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Refresh,
        });
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert!(matches!(
            client.tick_reconnect(0).expect("tick_reconnect"),
            ReconnectOutcome::Connecting
        ));
        assert!(matches!(
            client.tick_reconnect(0).expect("tick_reconnect"),
            ReconnectOutcome::Rebuilt
        ));
        assert!(
            client.pending_input_acks.is_empty(),
            "修复点：全新会话的回执表清空"
        );
        assert_eq!(
            client.key_batch_seq_next, 0,
            "修复点：序号归零重计（下一批 = 1，对齐 host 全新会话复位 0）"
        );

        // 重建会话的偏移估计已清零（纪元重建）——重新建立 0 偏移基准。
        let mut t2 = handles.lock().expect("handles")[0].clone();
        *v.lock().expect("clock") = 9_000_000;
        client.pump_probes(9_000_000).expect("pump_probes 锚定");
        *v.lock().expect("clock") = 10_000_000;
        client.pump_probes(10_000_000).expect("pump_probes 发 ping");
        t2.deliver_reliable(&ControlMessage::ProbePong {
            probe_id: 1,
            echoed_clock_us: 10_000_000,
            responder_clock_us: 10_000_001,
        });
        *v.lock().expect("clock") = 10_000_002;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.probe_snapshot().offset_us_median, Some(0));

        // 新会话第 1 个关键批（client seq=1）→ host（全新会话计数 0→1）
        // 回 seq=1 → 样本 = 新批发送后的短间隔；修复前会错配断线前旧条目
        // 得到 9_100_000µs 的跨重建伪样本。
        *v.lock().expect("clock") = 11_000_000;
        q.lock().expect("queue").push(click());
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(client.key_batch_seq_next, 1, "新会话从 1 重新计数");
        t2.deliver_reliable(&ControlMessage::InputAck {
            key_batch_seq: 1,
            inject_done_us: 11_100_000,
        });
        *v.lock().expect("clock") = 11_100_001;
        client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("tick");
        assert_eq!(
            client.input_one_way_samples_us(),
            &[100_000],
            "样本只含新会话内间隔（修复前会得到跨重建的 9_100_000µs 伪样本）"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// 修复卡 B-3（发现 3，med）：捕获器持续 Err → 计数增长、无 panic、
    /// tick 照常 Ok（原先 unwrap_or_default 静默吞，I9 不可诊断）。
    #[test]
    fn client_counts_capture_errors_instead_of_swallowing() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let _handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg_with_capture(Box::new(FailingCapture)),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        client.handshake().expect("handshake");
        for _ in 0..3 {
            let r = client
                .tick(Duration::from_millis(0), Instant::now())
                .expect("tick 不因捕获错误中断（尽力而为语义）");
            assert_eq!(r.input_bytes_sent, 0);
        }
        assert_eq!(
            client.input_capture_errors(),
            3,
            "每次捕获错误都计数（不再静默吞）"
        );
        assert_eq!(client.state(), SessionState::Streaming, "会话不受影响");
    }

    /// 修复卡 B-4（发现 4，low）：同批 Bye(Normal)+Bye(Refresh) → 先到的
    /// 生效、另一条忽略并计数；tick Ok 收尾（修复前：Normal 先干净停机，
    /// Refresh 又对 Closed 态返回 InvalidConfig）。
    #[test]
    fn client_duplicate_bye_in_one_batch_first_wins() {
        // 正序：Normal 在前 → 干净停机收尾，后到的 Refresh 忽略计数。
        let (mut client, mut handle) = handshaken(Vec::new());
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Normal,
        });
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Refresh,
        });
        let r = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("同批双 Bye 必须 Ok 收尾（修复前这里返回 InvalidConfig）");
        assert_eq!(
            r.closed,
            Some(StopReason::ByeReceived(ByeReason::Normal)),
            "先到的 Normal 生效"
        );
        assert_eq!(client.state(), SessionState::Closed);
        assert_eq!(
            client.bye_conflicts_dropped(),
            1,
            "后到的 Refresh 忽略并计数"
        );
        assert_eq!(client.pipeline().trace().summary().unfinished, 0);

        // 反序：Refresh 在前 → 全新重建生效（会话不结束），后到的 Normal
        // 忽略计数。
        let (mut client2, mut handle2) = handshaken(Vec::new());
        handle2.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Refresh,
        });
        handle2.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Normal,
        });
        let r2 = client2
            .tick(Duration::from_millis(0), Instant::now())
            .expect("同批双 Bye 必须 Ok 收尾");
        assert!(r2.closed.is_none(), "先到的 Refresh 生效：会话不结束");
        assert_eq!(client2.state(), SessionState::Reconnecting);
        assert_eq!(
            client2.bye_conflicts_dropped(),
            1,
            "后到的 Normal 忽略并计数"
        );
    }

    /// 复核遗留（B-4 同型竞态）：同批 Bye(Normal)+Disconnected（对端
    /// 「先 Bye 后关连接」的真实形态，host 侧同竞态已修）——Bye 先走
    /// stop_with 进 Closed，on_disconnected 幂等返回，tick Ok 收尾且
    /// 状态保留 Closed；修复前 debug 构建在 tick 尾部的
    /// `debug_assert_eq!(state, Reconnecting)` panic（release 行为正确）。
    #[test]
    fn client_bye_then_disconnected_same_batch_keeps_closed() {
        let (mut client, mut handle) = handshaken(Vec::new());
        handle.deliver_reliable(&ControlMessage::Bye {
            reason: ByeReason::Normal,
        });
        handle.deliver(NetEvent::Disconnected {
            reason: "closed after bye".into(),
        });
        let r = client
            .tick(Duration::from_millis(0), Instant::now())
            .expect("Bye 先到 + Disconnected 同批必须 Ok 收尾（debug 不 panic）");
        assert_eq!(
            r.closed,
            Some(StopReason::ByeReceived(ByeReason::Normal)),
            "先到的 Bye(Normal) 生效"
        );
        assert_eq!(
            client.state(),
            SessionState::Closed,
            "优雅关闭终态保留，不被 Disconnected 掩盖"
        );
        assert_eq!(
            client.pipeline().trace().summary().unfinished,
            0,
            "停机序列完整收口"
        );
    }

    // ─────────────────────────────────────────────────────────────
    // T19 Phase 4：ClientAuthenticator 集成测试（A1/A2/A5/A6/A7/A11/坑 5）
    // ─────────────────────────────────────────────────────────────

    /// T19（A1 + 坑 5 + A6）：密码握手 Hello 载荷恒空、过程 Welcome.granted 被忽略、
    /// 收到 AuthChallenge 后回发正确 PBKDF2 proof，仅终局 Welcome{Ok} 后建管线。
    #[test]
    fn client_password_handshake_sends_empty_hello_payload_and_responds_to_challenge() {
        let salt = [0x42u8; proto::AUTH_SALT_LEN];
        let iterations = 64u32;
        let process_granted = SessionParams {
            width: 16,
            height: 16,
            fps: 10,
            bitrate_bps: 100_000,
        };
        let final_granted = SessionParams {
            width: 32,
            height: 32,
            fps: 24,
            bitrate_bps: 500_000,
        };

        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(
            7,
            process_granted,
            proto::AuthStatus::ChallengeRequired,
        ));
        mock.deliver_reliable(&ControlMessage::AuthChallenge(proto::AuthChallenge {
            salt,
            iterations,
        }));
        mock.deliver_reliable(&welcome_msg(7, final_granted, proto::AuthStatus::Ok));
        let handle = mock.clone();

        let mut c = cfg(Vec::new());
        c.auth = ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        };
        let mut client = ClientSession::with_parts(
            c,
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        let granted = client.handshake().expect("password handshake succeeds");
        assert_eq!(
            granted, final_granted,
            "坑 5：必须采用终局 Welcome.granted，忽略过程 Welcome"
        );
        assert_eq!(client.negotiated(), Some(final_granted));
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.pipeline_generation(), 1);

        let msgs = handle.reliable_decoded();
        assert_eq!(msgs.len(), 2, "client 应发 Hello + AuthResponse 两条消息");
        match &msgs[0] {
            ControlMessage::Hello(h) => {
                assert_eq!(h.auth_method, proto::AuthMethod::Password);
                assert!(h.auth_payload.is_empty(), "A1：Hello 绝不携带密码");
            }
            other => panic!("首条应为 Hello，实际 {other:?}"),
        }
        let expected_proof = crate::auth::compute_proof(b"hunter2", &salt, iterations);
        match &msgs[1] {
            ControlMessage::AuthResponse(resp) => {
                assert_eq!(
                    resp.proof, expected_proof,
                    "proof 须与 compute_proof 逐字节一致"
                );
            }
            other => panic!("第二条应为 AuthResponse，实际 {other:?}"),
        }
    }

    /// T19（A6）：未收过程 Welcome{ChallengeRequired} 先收 AuthChallenge → 协议违规断开、零管线启动。
    #[test]
    fn client_auth_challenge_before_process_welcome_fails_protocol() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&ControlMessage::AuthChallenge(proto::AuthChallenge {
            salt: [0x11u8; proto::AUTH_SALT_LEN],
            iterations: 64,
        }));
        let mut c = cfg(Vec::new());
        c.auth = ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        };
        let mut client = ClientSession::with_parts(
            c,
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        let err = client.handshake().expect_err("乱序 AuthChallenge 必须报错");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(client.state(), SessionState::Closed);
        assert!(client.negotiated().is_none());
        assert_eq!(client.pipeline_generation(), 0, "A6：鉴权失败不得启动管线");
    }

    /// T19（A5 + A6）：挑战后 host 返回 Welcome{Rejected} → 断开、Closed、零管线启动。
    #[test]
    fn client_auth_rejected_after_challenge_closes_without_pipeline() {
        let mut mock = MockTransport::new();
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::ChallengeRequired));
        mock.deliver_reliable(&ControlMessage::AuthChallenge(proto::AuthChallenge {
            salt: [0x22u8; proto::AUTH_SALT_LEN],
            iterations: 64,
        }));
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Rejected));

        let mut c = cfg(Vec::new());
        c.auth = ClientAuthConfig {
            password: Some("wrong".into()),
            auth_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        };
        let mut client = ClientSession::with_parts(
            c,
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        let err = client
            .handshake()
            .expect_err("Rejected 必须返回 Protocol 错误");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(client.state(), SessionState::Closed);
        assert!(client.negotiated().is_none());
        assert_eq!(client.pipeline_generation(), 0, "A6：鉴权拒绝不得启动管线");
    }

    /// T19（A7 + A6）：注入时钟越过 `auth_timeout_us` → `poll_timeout` 触发 Timeout 并断开。
    #[test]
    fn client_auth_timeout_via_injected_clock() {
        let step = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let step_clone = std::sync::Arc::clone(&step);
        // 第 1 次（on_hello_sent）返回 0；后续轮询返回 2_000_000 µs > 1_000_000 µs 超时窗口。
        let clock: SessionClock = Arc::new(move || {
            let n = step_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                0
            } else {
                2_000_000
            }
        });

        let mut mock = MockTransport::new();
        // 只投递过程 Welcome，不投终局 Welcome → 等待超时。
        mock.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::ChallengeRequired));

        let mut c = cfg(Vec::new());
        c.handshake_timeout = Duration::from_secs(10);
        c.auth = ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: 1_000_000,
            ..ClientAuthConfig::default()
        };
        let mut client =
            ClientSession::with_parts(c, Box::new(mock), fake_decoder(0), fake_renderer(), clock)
                .expect("with_parts");
        let err = client
            .handshake()
            .expect_err("越过 auth_timeout_us 必须超时");
        assert!(
            matches!(err, Error::Timeout(d) if d == Duration::from_secs(1)),
            "应为 auth_timeout_us 对应的 Timeout，实际 {err:?}"
        );
        assert_eq!(client.state(), SessionState::Closed);
        assert_eq!(client.pipeline_generation(), 0, "A6：超时不得启动管线");
    }

    /// T19（A11）：Streaming 期收到 AuthChallenge / AuthResponse / Welcome 一律致命 Error::Protocol 断开。
    #[test]
    fn client_mid_stream_auth_messages_are_fatal_protocol_error() {
        let cases = [
            ControlMessage::AuthChallenge(proto::AuthChallenge {
                salt: [0x33u8; proto::AUTH_SALT_LEN],
                iterations: 64,
            }),
            ControlMessage::AuthResponse(proto::AuthResponse {
                proof: [0x44u8; proto::AUTH_PROOF_LEN],
            }),
            welcome_msg(7, req(), proto::AuthStatus::Ok),
        ];
        for msg in cases {
            let (mut client, mut handle) = handshaken(Vec::new());
            handle.deliver_reliable(&msg);
            let err = client
                .tick(Duration::from_millis(0), Instant::now())
                .expect_err("A11：Streaming 期鉴权/Welcome 消息必须致命报错");
            assert!(matches!(err, Error::Protocol { .. }), "{err}");
            assert!(err.is_fatal(), "Error::Protocol 必须 is_fatal()");
            assert_eq!(
                client.state(),
                SessionState::Closed,
                "致命协议错误必须断开并停机"
            );
        }
    }

    /// T19（A2 + A5）：T17 重连降级全新 Hello 走全新 ClientAuthenticator 与新盐挑战应答；
    /// 重连鉴权被拒时直接停机（Closed），不陷入无限重连循环。
    #[test]
    fn client_reconnect_fresh_hello_runs_fresh_password_auth() {
        let salt1 = [0x10u8; proto::AUTH_SALT_LEN];
        let salt2 = [0x20u8; proto::AUTH_SALT_LEN];
        let iterations = 64u32;

        // 首次密码握手（salt1）
        let mut mock1 = MockTransport::new();
        mock1.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::ChallengeRequired));
        mock1.deliver_reliable(&ControlMessage::AuthChallenge(proto::AuthChallenge {
            salt: salt1,
            iterations,
        }));
        mock1.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));

        let mut c = cfg(Vec::new());
        c.auth = ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: crate::auth::DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        };
        let mut client = ClientSession::with_parts(
            c,
            Box::new(mock1),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        client.handshake().expect("initial password handshake");
        assert_eq!(client.pipeline_generation(), 1);

        // 重连：resume 被拒（bit0=0）→ 同连接降级全新 Hello（sid=8）→ salt2 挑战应答 → Rebuilt。
        let mock2 = MockTransport::new();
        let mut handle2 = mock2.clone();
        handle2.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok)); // bit0=0
        let mut slot = Some(mock2);
        client.set_connect_factory(Box::new(move || {
            Ok(Box::new(slot.take().expect("one reconnect transport")))
        }));
        client.set_pipeline_factory(Box::new(|| {
            ControllerPipeline::with_backends(pcfg(), fake_decoder(0), fake_renderer())
        }));

        client.on_disconnected(1_000_000);
        // 首次退避 0.5s → 到点 connect 发 ReconnectRequest
        let out = client.tick_reconnect(2_000_000).expect("connect");
        assert_eq!(out, ReconnectOutcome::Connecting);
        // 收到 Welcome{bit0=0} → 同连接补发全新 Hello（sid=8）
        let out = client
            .tick_reconnect(2_010_000)
            .expect("fallback to fresh hello");
        assert_eq!(
            out,
            ReconnectOutcome::Waiting {
                next_attempt_in_ms: 0
            }
        );

        // 投递过程 Welcome（哑 granted）+ 新盐 salt2 挑战 + 终局 Welcome{Ok}
        let dummy_granted = SessionParams {
            width: 16,
            height: 16,
            fps: 10,
            bitrate_bps: 100_000,
        };
        handle2.deliver_reliable(&welcome_msg(
            8,
            dummy_granted,
            proto::AuthStatus::ChallengeRequired,
        ));
        handle2.deliver_reliable(&ControlMessage::AuthChallenge(proto::AuthChallenge {
            salt: salt2,
            iterations,
        }));
        handle2.deliver_reliable(&welcome_msg(8, req(), proto::AuthStatus::Ok));

        let out = client
            .tick_reconnect(2_020_000)
            .expect("rebuilt after fresh auth");
        assert_eq!(out, ReconnectOutcome::Rebuilt);
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(client.pipeline_generation(), 2);
        assert_eq!(
            client.negotiated(),
            Some(req()),
            "坑 5：忽略过程 Welcome.granted"
        );

        // 校验新连接发出的消息序列：ReconnectRequest → Hello{Password, 空 payload} → AuthResponse{salt2 proof}
        let sent = handle2.reliable_decoded();
        assert_eq!(
            sent.len(),
            3,
            "应依次发 ReconnectRequest / Hello / AuthResponse: {sent:?}"
        );
        match &sent[1] {
            ControlMessage::Hello(h) => {
                assert_eq!(h.session_id, 8);
                assert_eq!(h.auth_method, proto::AuthMethod::Password);
                assert!(h.auth_payload.is_empty(), "A1：重连 Hello 同样绝不携带密码");
            }
            other => panic!("第二条应为全新 Hello，实际 {other:?}"),
        }
        let expected_proof2 = crate::auth::compute_proof(b"hunter2", &salt2, iterations);
        let old_proof1 = crate::auth::compute_proof(b"hunter2", &salt1, iterations);
        assert_ne!(expected_proof2, old_proof1);
        match &sent[2] {
            ControlMessage::AuthResponse(resp) => {
                assert_eq!(
                    resp.proof, expected_proof2,
                    "A2：必须用新盐 salt2 重新计算 proof"
                );
            }
            other => panic!("第三条应为 AuthResponse，实际 {other:?}"),
        }

        // 再验证重连鉴权被拒：Bye(Refresh) 触发全新重建，host 回 Welcome{Rejected} → 致命 Err + Closed（不无限重试）
        let mut mock3 = MockTransport::new();
        mock3.deliver_reliable(&welcome_msg(9, req(), proto::AuthStatus::Rejected));
        let mut slot3 = Some(mock3);
        client.set_connect_factory(Box::new(move || {
            Ok(Box::new(slot3.take().expect("one refresh transport")))
        }));
        client.on_bye_refresh(3_000_000).expect("on_bye_refresh");
        assert_eq!(
            client.tick_reconnect(0).expect("connect refresh"),
            ReconnectOutcome::Connecting
        );
        let err = client
            .tick_reconnect(10_000)
            .expect_err("重连鉴权被拒必须致命失败");
        assert!(matches!(err, Error::Protocol { .. }), "{err}");
        assert_eq!(
            client.state(),
            SessionState::Closed,
            "鉴权失败直接 Closed，不无限重连"
        );
    }

    /// H8 回归：`observations()` 从 `frame_headers` 读元组 `.1`（`encode_done_ts`）而非
    /// `.0`（`capture_ts`），使 `SessionReport` 算出真实非零 `breakdown_ms.encode`。
    #[test]
    fn client_observations_reads_encode_done_ts_for_breakdown_encode() {
        let now_us = Arc::new(std::sync::atomic::AtomicU32::new(20_000));
        let clock: SessionClock = {
            let now = Arc::clone(&now_us);
            Arc::new(move || now.load(std::sync::atomic::Ordering::Relaxed))
        };
        let mut t = MockTransport::new();
        t.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let mut handle = t.clone();
        let mut client = ClientSession::with_parts(
            cfg(vec![]),
            Box::new(t),
            fake_decoder(0),
            fake_renderer(),
            clock,
        )
        .expect("with_parts");
        client.handshake().expect("handshake");

        // 构造一帧 capture_ts=1_000, encode_done_ts=6_000（编码耗时 5ms）的媒体数据报
        let payload = vec![0xABu8; 64];
        let datagrams = split_frame(
            FrameSeq(1),
            FrameKind::Idr,
            Timestamp(1_000),
            Timestamp(6_000),
            0,
            &payload,
            MAX_PAYLOAD,
        );
        for d in datagrams {
            handle.deliver(NetEvent::Datagram(d));
        }
        client
            .tick(Duration::from_millis(0), std::time::Instant::now())
            .expect("tick");

        let obs = client.observations(1_000_000, Some(20_000));
        let frame = obs
            .frames
            .iter()
            .find(|f| f.seq == FrameSeq(1))
            .expect("frame 1 in observations");
        assert_eq!(frame.captured_us, 1_000);
        assert_eq!(
            frame.encode_done_us, 6_000,
            "H8：encode_done_us 必须取自 tuple.1 (encode_done_ts)"
        );

        let snap = crate::report::SessionConfigSnapshot {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 30,
            duration_s: 1.0,
            backend: "fake".into(),
            connect_addr: "127.0.0.1:9901".parse().unwrap(),
            input_mode: "none",
        };
        let report = crate::report::SessionReport::from_observations(&snap, &obs);
        let json = report.to_json();
        assert!(
            json.contains("\"breakdown_ms\":{\"encode\":5,"),
            "H8：breakdown_ms.encode 应为 5ms，实际 JSON={json}"
        );
        client.close(ByeReason::Normal).expect("close");
    }

    /// H4 + H5 回归：经 `Bye(Refresh)` 重建 `ControllerPipeline` 后，
    /// 1) `observations()` 跨重建累计 `total_displayed`（旧管线 + 新管线之和）；
    /// 2) `observations().reconnect` 将饱和刷新单列 `refreshes=1` 且 `attempts=0`，
    ///    不触发 `resumed=false` 误判。
    #[test]
    fn client_observations_accumulates_displayed_and_separates_refreshes_across_rebuild() {
        let now_us = Arc::new(std::sync::atomic::AtomicU32::new(10_000));
        let clock: SessionClock = {
            let now = Arc::clone(&now_us);
            Arc::new(move || now.load(std::sync::atomic::Ordering::Relaxed))
        };
        let mut t1 = MockTransport::new();
        t1.deliver_reliable(&welcome_msg(7, req(), proto::AuthStatus::Ok));
        let mut handle1 = t1.clone();
        let mut client = ClientSession::with_parts(
            cfg(vec![]),
            Box::new(t1),
            fake_decoder(0),
            fake_renderer(),
            clock,
        )
        .expect("with_parts");
        client.handshake().expect("handshake");
        let payload = vec![0xCDu8; 64];

        // 纪元 1：投递 3 帧并上屏
        for seq in 1..=3u64 {
            now_us.store(16_667 * seq as u32, std::sync::atomic::Ordering::Relaxed);
            let dgs = split_frame(
                FrameSeq(seq),
                FrameKind::Idr,
                Timestamp(1_000 * seq as u32),
                Timestamp(1_000 * seq as u32 + 500),
                0,
                &payload,
                MAX_PAYLOAD,
            );
            for d in dgs {
                handle1.deliver(NetEvent::Datagram(d));
            }
            client
                .tick(Duration::from_millis(0), std::time::Instant::now())
                .expect("tick epoch 1");
        }
        assert_eq!(client.pipeline().displayed(), 3);

        // Bye(Refresh) 重建管线
        let mut mock2 = MockTransport::new();
        let mut handle2 = mock2.clone();
        mock2.deliver_reliable(&ControlMessage::Welcome(proto::Welcome {
            session_id: 8,
            granted: req(),
            auth_status: proto::AuthStatus::Ok,
            host_flags: HostFlags(HostFlags::CLOCK_EPOCH_RESET),
        }));
        let mut slot = Some(mock2);
        client.set_connect_factory(Box::new(move || {
            Ok(Box::new(slot.take().expect("rebuild transport")))
        }));
        client.set_pipeline_factory(Box::new(|| {
            ControllerPipeline::with_backends(pcfg(), fake_decoder(0), fake_renderer())
        }));
        client.on_bye_refresh(4_200_000_000).expect("refresh");
        now_us.store(0, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            client.tick_reconnect(0).expect("connect"),
            ReconnectOutcome::Connecting
        );
        assert_eq!(
            client.tick_reconnect(1_000).expect("rebuilt"),
            ReconnectOutcome::Rebuilt
        );

        // 纪元 2：投递 2 帧并上屏
        for seq in 1..=2u64 {
            now_us.store(16_667 * seq as u32, std::sync::atomic::Ordering::Relaxed);
            let dgs = split_frame(
                FrameSeq(seq),
                FrameKind::Idr,
                Timestamp(1_000 * seq as u32),
                Timestamp(1_000 * seq as u32 + 500),
                0,
                &payload,
                MAX_PAYLOAD,
            );
            for d in dgs {
                handle2.deliver(NetEvent::Datagram(d));
            }
            client
                .tick(Duration::from_millis(0), std::time::Instant::now())
                .expect("tick epoch 2");
        }
        assert_eq!(client.pipeline().displayed(), 2);

        let obs = client.observations(5_000_000_000u64, Some(10_000));
        assert_eq!(obs.duration_us, 5_000_000_000u64);
        assert_eq!(
            obs.total_displayed,
            Some(5),
            "H4：应累加重建前(3)+重建后(2)=5 帧"
        );
        assert_eq!(
            obs.reconnect.attempts, 0,
            "H5：饱和刷新不计入意外断线 attempts"
        );
        assert_eq!(obs.reconnect.refreshes, 1, "H5：饱和刷新单列 refreshes");
        assert!(!obs.reconnect.resumed);
        client.close(ByeReason::Normal).expect("close");
    }

    /// H6 回归：ClientSession 在 `Rebuilt` 后自动重置剪贴板纪元态，
    /// 驱动器时钟归零到 0 后本地剪贴板变更仍可立即同步。
    #[test]
    fn client_reset_clipboard_epoch_on_rebuild_allows_sync_after_clock_reset() {
        let (mut client, _handle1) = handshaken(vec![]);
        let cb = crate::testkit::SharedFakeClipboard::new();
        let cb_ctrl = cb.clone();
        client.enable_clipboard(Box::new(cb));

        // 旧纪元高位时刻基线 + 发送 1 条
        client.pump_clipboard(4_000_000_000).expect("baseline");
        cb_ctrl.simulate_local_copy(Some("old-epoch-client"));
        client.pump_clipboard(4_000_010_000).expect("send 1");
        assert_eq!(client.clipboard_stats().sent, 1);

        // Bye(Refresh) 重建（自动 reset_clipboard_epoch）
        let mut mock2 = MockTransport::new();
        mock2.deliver_reliable(&ControlMessage::Welcome(proto::Welcome {
            session_id: 8,
            granted: req(),
            auth_status: proto::AuthStatus::Ok,
            host_flags: HostFlags(HostFlags::CLOCK_EPOCH_RESET),
        }));
        let mut slot = Some(mock2);
        client.set_connect_factory(Box::new(move || {
            Ok(Box::new(slot.take().expect("rebuild transport")))
        }));
        client.set_pipeline_factory(Box::new(|| {
            ControllerPipeline::with_backends(pcfg(), fake_decoder(0), fake_renderer())
        }));
        client.on_bye_refresh(4_200_000_000).expect("refresh");
        assert_eq!(
            client.tick_reconnect(0).expect("connect"),
            ReconnectOutcome::Connecting
        );
        assert_eq!(
            client.tick_reconnect(1_000).expect("rebuilt"),
            ReconnectOutcome::Rebuilt
        );

        // 新纪元时钟归零到 0 → 首拍基线，随后变更立发
        client.pump_clipboard(0).expect("new baseline");
        cb_ctrl.simulate_local_copy(Some("new-epoch-client"));
        client.pump_clipboard(1_000).expect("send 2");
        assert_eq!(client.clipboard_stats().sent, 2);
        client.close(ByeReason::Normal).expect("close");
    }

    #[test]
    fn trailing_tool_channel_in_welcome_poll_batch_is_preserved() {
        let mock = MockTransport::new();
        let mut handle = mock.clone();
        let mut client = ClientSession::with_parts(
            cfg(vec![]),
            Box::new(mock),
            fake_decoder(0),
            fake_renderer(),
            zero_clock(),
        )
        .expect("with_parts");
        handle.deliver_reliable(&ControlMessage::Welcome(proto::Welcome {
            session_id: 7,
            granted: req(),
            auth_status: proto::AuthStatus::Ok,
            host_flags: HostFlags(0),
        }));
        let payload = b"{\"kind\":\"capabilities\"}".to_vec();
        handle.deliver_reliable(
            &ControlMessage::tool_channel(proto::TOOL_CHANNEL_DISPLAY_CTRL, payload.clone())
                .unwrap(),
        );

        client.handshake().expect("handshake");
        assert_eq!(client.state(), SessionState::Streaming);
        assert_eq!(
            client.drain_tool_messages(),
            vec![(proto::TOOL_CHANNEL_DISPLAY_CTRL, payload)]
        );
        client.close(ByeReason::Normal).expect("close");
    }
}
