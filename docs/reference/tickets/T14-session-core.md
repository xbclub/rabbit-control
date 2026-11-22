# T14 会话核心：HostSession/ClientSession 状态机与握手

> 在 `rc-session`（T13 已建骨架与 `proto.rs`）内实现被控端/控制端两侧会话：四态状态机、Hello/Welcome 参数协商、三通道事件分发、照 `run_real` 形态的驱动循环、Bye 优雅关闭与停机顺序红线。

## 背景（为什么做这个）

M1 的全链验证是**单进程回环**：`run_real` 把 host 与 controller 接在进程内 `sync_channel(2)` 上（`crates/rc-loopback/src/lib.rs:364`），IDR 请求靠 `AtomicBool` 跨线程传递。M2 要把它**进程间化**：`sync_channel` 换成 rc-net 三通道 transport，`AtomicBool` 换成 reliable 流上的 `IdrRequest` 消息，管线两侧 API 零改动复用（架构结论：复用 rc-pipeline，不绕过）。

会话生命周期（连接、握手、关闭）不属于任何现有 crate：rc-net 有 T09 红线「不碰采集/编码/解码」；rc-pipeline 是已过 M1 Gate 的稳定层；rc-cli 必须薄。所以落在 `rc-session`（workspace 第 22 个 crate，骨架由 T13 先行交付）。本卡是该 crate 的第一块**行为**：状态机与驱动循环。探测/看门狗/重连分别在 T15/T16/T17。

## 平台要求

**全平台，无需任何权限。** 离线测试用进程内 mock transport；真 QUIC 集成测试走 `127.0.0.1` UDP 回环（与 rc-net 既有测试同一形态，CI 可跑）。真机双进程实测属 T18。

## 领取前置检查

- [ ] `crates/rc-session/` 已存在（T13 交付）：`Cargo.toml`、`src/lib.rs`、`src/proto.rs`（全部控制消息的枚举/encode/decode 可用）。
- [ ] `cargo test -p rc-session` 在 T13 状态下全绿。

**proto 符号约定**：本卡引用 `proto::ControlMessage`（含 `Hello`/`Welcome`/`IdrRequest`/`IdrAck`/`Bye` 等全集变体）、`proto::SessionParams`（`width`/`height`/`fps`/`bitrate_bps`）、`proto::encode`/`proto::decode`、`proto::ByeReason::{Normal, Refresh}`、`proto::IdrReason::{DecoderNeedKeyframe, WatchdogStall, ReconnectRecovery}`（与 T13 契约的全名一致，本卡行文统一用全名，不用 NeedKeyframe/Stall/Reconnect 简称）。这些的**唯一事实源是 T13 交付代码**（两卡同源自同一架构定义）；若实际命名与本文有出入，以 `proto.rs` 为准做适配，禁止在 proto 之外另造平行类型。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/host_session.rs` | 新建 | `HostSession`：accept/握手/分发/驱动循环/停机 |
| `crates/rc-session/src/client_session.rs` | 新建 | `ClientSession`：connect/握手/分发/驱动循环/停机 |
| `crates/rc-session/src/testkit.rs` | 新建 | 进程内 `MockTransport` + `pair()` 对接件（T18 将在本模块**追加** `ScriptedCapture`/`RecordingInjector`，本卡不放，避免撞车；后续卡对本模块只追加、沿用 `MockTransport`/`pair()` 既有符号名，不另造 `transport_pair()` 等平行名） |
| `crates/rc-session/src/lib.rs` | 修改 | 追加三个 `pub mod` 与 `SessionClock`/`SessionState`/`StopReason` 导出（骨架是 T13 的，只增不改既有内容） |
| `crates/rc-session/Cargo.toml` | 修改 | 补 `rc-pipeline`/`rc-input`/`rc-trace` 依赖与 dev-dependencies（见文末） |
| `crates/rc-input/src/channel.rs` | 修改 | **唯一 rc-input 改动**：新增 `pub fn inject_message`（纯增量，`pump_receiver` 内部改为复用它，既有测试断言一个不改） |
| `crates/rc-session/tests/certs/*.pem` | 新建 | localhost 测试自签证书（照 `crates/rc-net/tests/certs` 模式复制一份，仅测试用） |
| `crates/rc-session/tests/session_roundtrip.rs` | 新建 | mock pair 进程内双会话闭环 |
| `crates/rc-session/tests/quic_loopback.rs` | 新建 | 真 QUIC over UDP 回环集成测试 |

## API 契约

### lib.rs 追加（两侧共用）

```rust
/// 会话时钟：微秒，相对会话零点。照抄 run_real 的 clock Arc 模式
/// （crates/rc-loopback/src/lib.rs:342-358）：由构造方注入，两侧管线
/// 共享同一闭包；会话内部除驱动循环的 poll 预算外不读任何时钟。
pub type SessionClock = std::sync::Arc<dyn Fn() -> u32 + Send + Sync>;

/// 会话生命周期（严格单向：Init→Connected→Streaming→Closed）。
///
/// - `Init`：端点已创建，传输未建立；
/// - `Connected`：QUIC 握手完成，Hello/Welcome 参数协商进行中；
/// - `Streaming`：参数已裁定，媒体/输入双向流动；
/// - `Closed`：Bye 已发/收或传输断开，且停机序列已完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState { Init, Connected, Streaming, Closed }

/// 会话结束原因（run/close 的摘要字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// stop 闭包要求退出（到时 / 上层指令），本端随后发 Bye(Normal)。
    Requested,
    /// 对端发来 Bye（Refresh 语义的自动重连属 T17，本卡只透传记录）。
    ByeReceived(ByeReason),
    /// 传输层断开（本卡不做重连，直接走停机序列）。
    Disconnected(String),
}
```

### host_session.rs

```rust
/// 被控端会话配置。
pub struct HostSessionConfig {
    /// 监听地址（`0.0.0.0:9901` / `127.0.0.1:0` 均可）。
    pub listen_addr: SocketAddr,
    /// 路径 MTU 预算，默认 `rc_net::quic::DEFAULT_MTU`。
    pub mtu: usize,
    /// host 能力上界：Welcome 裁定时把 client 请求逐字段 min 夹紧到此。
    pub caps: SessionParams,
    /// 管线配置模板；resolution/fps/bitrate 在握手后以裁定值覆盖。
    pub pipeline: rc_pipeline::PipelineConfig,
    /// accept 后等 Hello 的上限，默认 5s。
    pub handshake_timeout: Duration,
    /// 输入注入器（真机 = rc-input-macos 的 CGEvent 桥；测试 = 记录型替身）。
    pub injector: Box<dyn rc_input::InputInjector>,
    /// 假采集节流：假后端必须 true（见坑 4），真采集 false。
    pub pace_capture: bool,
}

impl HostSessionConfig {
    pub fn new(listen_addr: SocketAddr, caps: SessionParams,
               injector: Box<dyn rc_input::InputInjector>) -> Self;
}

/// 被控端会话：QuicEndpoint(accept) + QuicTransport + HostPipeline + 注入。
pub struct HostSession { /* 私有 */ }

impl HostSession {
    /// 真网络构造：绑定 QUIC 端点（Init）。管线**此刻不建**——参数未裁定。
    /// `cert_pem`/`key_pem` 是本端服务证书（内部经 `rc_net::quic::Identity::from_pem`）。
    pub fn new(cfg: HostSessionConfig, cert_pem: &str, key_pem: &str,
               clock: SessionClock) -> Result<Self>;

    /// 测试注入：transport 已连通（Connected），媒体后端由调用方给定
    /// （能力需与 `cfg.caps` 一致）。管线同样推迟到握手完成。
    pub fn with_parts(cfg: HostSessionConfig, transport: Box<dyn rc_net::Transport>,
                      capture: Box<dyn rc_capture::CaptureBackend>,
                      encoder: Box<dyn rc_encode::Encoder>,
                      clock: SessionClock) -> Result<Self>;

    /// 阻塞 accept 一个连接，随后完成 Hello/Welcome（Init→Connected→Streaming）。
    /// 返回裁定参数。Hello 违规（version≠1 等）返回 Err 且连接关闭。
    pub fn accept_and_handshake(&mut self, timeout: Duration) -> Result<SessionParams>;

    /// 已连通 transport 上的握手步骤（with_parts 后由测试直接驱动）。
    /// 内部以 `handshake_timeout` 为预算 poll 等待 Hello。
    pub fn handshake(&mut self) -> Result<SessionParams>;

    pub fn state(&self) -> SessionState;
    /// Hello 带来的会话号。
    pub fn session_id(&self) -> Option<u64>;
    pub fn negotiated(&self) -> Option<SessionParams>;

    /// 一拍（run_real 形态的最小单元）：
    /// ① `transport.poll(poll_budget)` 单点分发——`ReliableUrgent`→
    /// `rc_input::inject_message` 解码注入；`Reliable`→控制分发
    /// （`IdrRequest`→`pipeline.request_idr()`，host.rs:462 的 pending_idr
    /// 粘住直到真编出 IDR）；`Disconnected`→内联停机序列；
    /// ② `pipeline.tick(clock())` 产出数据报→逐条 `send_datagram`。
    pub fn tick(&mut self, poll_budget: Duration) -> Result<HostTickReport>;

    /// 主动优雅关闭（停机红线顺序见「必须做到」S6）。
    pub fn close(&mut self, reason: ByeReason) -> Result<HostRunSummary>;

    /// 驱动循环：跑到 `stop()` 为真、收到 Bye 或断连。退出前若未发过
    /// Bye 则补发 Bye(Normal) 并执行停机序列。
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<HostRunSummary>;

    /// 诊断出口（trace/sched_stats/in_flight 等经它取）。
    pub fn pipeline(&self) -> &rc_pipeline::HostPipeline;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostTickReport {
    /// 本拍发出的媒体数据报数。
    pub datagrams_sent: usize,
    /// 本拍注入的输入事件数。
    pub events_injected: usize,
    /// 本拍处理的控制消息数。
    pub control_msgs_handled: usize,
    /// 本拍有任何事件/产出；run 在 !busy 时挂起等待（坑 2）。
    pub busy: bool,
    /// 本拍内进入 Closed 的原因（停机序列已完成）。
    pub closed: Option<StopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRunSummary {
    pub stop_reason: StopReason,
    pub datagrams_sent: usize,
    pub events_injected: u64,
    pub frames_encoded: u64,
    /// 停机后 trace 未结帧数，必须为 0（S5 断言）。
    pub unfinished_traces: u64,
}
```

### client_session.rs

```rust
/// 控制端会话配置。
pub struct ClientSessionConfig {
    pub peer_addr: SocketAddr,
    pub mtu: usize,
    /// Hello 请求参数（裁定后以 Welcome 回值为准，此值不生效）。
    pub requested: SessionParams,
    /// 会话号（client 生成；调用方负责，测试用固定值保证确定性）。
    pub session_id: u64,
    pub pipeline: rc_pipeline::PipelineConfig,
    pub connect_timeout: Duration,   // 默认 10s
    pub handshake_timeout: Duration, // 默认 5s：发 Hello 后等 Welcome
    /// 输入捕获器（真机 = 平台捕获桥；测试 = 脚本型替身）。
    pub capture: Box<dyn rc_input::InputCapture>,
    /// InputChannel 批窗口，默认 `rc_input::DEFAULT_WINDOW`。
    pub input_window: Duration,
}

pub struct ClientSession { /* 私有 */ }

impl ClientSession {
    /// 真网络构造（Init）。`server_cert_pem` 为固定信任的对端证书
    /// （内部经 `Identity::certs_from_pem`，不直接依赖 rustls）。
    pub fn new(cfg: ClientSessionConfig, server_cert_pem: &str, server_name: &str,
               clock: SessionClock) -> Result<Self>;

    /// 测试注入（Connected，等握手）。
    pub fn with_parts(cfg: ClientSessionConfig, transport: Box<dyn rc_net::Transport>,
                      decoder: Box<dyn rc_decode::Decoder>,
                      renderer: Box<dyn rc_render::Renderer>,
                      clock: SessionClock) -> Result<Self>;

    /// connect + Hello/Welcome（Init→Connected→Streaming），返回裁定参数。
    pub fn connect_and_handshake(&mut self) -> Result<SessionParams>;

    /// 已连通 transport 上的握手步骤：发 Hello → 以 handshake_timeout
    /// 为预算 poll 等 Welcome → 校验 session_id 回显与 auth_status →
    /// 以裁定参数建管线（ControllerPipeline::with_backends）。
    pub fn handshake(&mut self) -> Result<SessionParams>;

    pub fn state(&self) -> SessionState;
    pub fn session_id(&self) -> u64;
    pub fn negotiated(&self) -> Option<SessionParams>;

    /// 一拍：① `input.pump_sender(capture, transport, now, ZERO)`（输入走
    /// urgent 流，channel.rs:83）；② `poll(poll_budget)` 分发——
    /// `Datagram`→`pipeline.push_datagram(data, clock())`（controller.rs:105，
    /// 含重组+解码+呈现）；`Reliable`→控制分发（Welcome 之外的 Bye/IdrAck
    /// 按 S8 处理）；`Disconnected`→内联停机；③ 每拍末
    /// `pipeline.take_keyframe_request()`（controller.rs:214）为真则发
    /// `IdrRequest{reason: DecoderNeedKeyframe}`（基本接线，无节流——节流是 T16）。
    /// `now` 供批窗口判断（Instant），须与会话钟同轴（坑 5）。
    pub fn tick(&mut self, poll_budget: Duration, now: std::time::Instant)
        -> Result<ClientTickReport>;

    /// 手动请求关键帧（T16 会基于它加去抖；本卡直发）。
    pub fn request_idr(&mut self, reason: IdrReason) -> Result<()>;

    pub fn close(&mut self, reason: ByeReason) -> Result<ClientRunSummary>;
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<ClientRunSummary>;

    pub fn pipeline(&self) -> &rc_pipeline::ControllerPipeline;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClientTickReport {
    pub datagrams_pushed: usize,
    /// 本拍新上屏帧数。
    pub frames_displayed_delta: u64,
    pub input_bytes_sent: usize,
    pub idr_requests_sent: usize,
    pub busy: bool,
    pub closed: Option<StopReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRunSummary {
    pub stop_reason: StopReason,
    pub frames_displayed: u64,
    pub input_bytes_sent: u64,
    pub idr_requests_sent: u64,
    /// 停机后 trace 未结帧数，必须为 0（S5 断言）。
    pub unfinished_traces: u64,
}
```

### testkit.rs

```rust
/// 进程内传输对接件：记录发送、脚本化接收、可互连。
///
/// `poll` **忽略 timeout 立即返回**已就绪事件（确定性优先；N4 的超时
/// 语义由 rc-net 自己的测试覆盖，别拿它验证）。
pub struct MockTransport { /* 私有 */ }

impl MockTransport {
    pub fn new() -> Self;
    /// 预置一条下一拍 poll 要交付的事件（按投递序）。
    pub fn deliver(&mut self, ev: rc_net::NetEvent);
    /// 编码一条控制消息并入箱（等价 `deliver(NetEvent::Reliable(msg))`）。
    pub fn deliver_reliable(&mut self, msg: &ControlMessage);
    pub fn datagrams_sent(&self) -> &[Vec<u8>];
    pub fn reliable_sent(&self) -> &[Vec<u8>];
    pub fn urgent_sent(&self) -> &[Vec<u8>];
    /// 可靠流上收到的控制消息（按序解码，测试断言用）。
    pub fn reliable_decoded(&self) -> Vec<ControlMessage>;
}

impl rc_net::Transport for MockTransport { /* send_* 记录（或镜像给对端），poll 即刻返回 */ }

/// 双工对接：一对互连的 MockTransport——A 的 `send_datagram`/`send_reliable`
/// /`send_reliable_urgent` 依次出现在 B 的 `poll` 里，反之亦然。
/// 无损无延迟；容量不设界（铁律 1 管管线队列，不管测试件）。
pub fn pair() -> (MockTransport, MockTransport);
```

### rc-input 的唯一改动（channel.rs 追加）

```rust
/// 解码并注入**一条** urgent 消息（`pump_receiver` 循环体的提取）。
///
/// 为什么提取：`pump_receiver` 内部自己 `transport.poll`（channel.rs:119），
/// 同批到达的非 urgent 事件（控制消息 / `Disconnected`）会被静默丢弃——
/// 会话层必须**独占** poll 做三通道分发，urgent 注入改走本函数，逐条
/// 调用，错误隔离语义与 `pump_receiver` 完全一致（单条失败返回 Err、
/// 不注入半批）。`pump_receiver` 改为内部循环调用本函数，行为不变。
pub fn inject_message(msg: &[u8], injector: &mut dyn InputInjector) -> Result<usize>;
```

### 驱动循环骨架（照 run_real，`crates/rc-loopback/src/lib.rs:336-423` 改写）

```rust
// host 侧 run()（client 对称，方向相反）：
while !stop() && self.state == SessionState::Streaming {
    let r = self.tick(Duration::from_millis(1))?;   // poll 分发 + tick + 发送
    if let Some(_) = r.closed { break; }            // Bye/断连已在 tick 内完成停机
    if !r.busy {
        // host：pipeline.wait_event(≤10ms)（host.rs:355，事件驱动挂起零自旋）；
        // client：无 wait_event，靠下一拍 poll 的预算承担等待（N4）。
    }
}
// 退出后若未发过 Bye：发 Bye(Normal) → 停机红线序列（S6）。
```

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| S1 | 状态机严格四态单向 `Init→Connected→Streaming→Closed`；非法迁移（如 Closed 后 tick、Init 里 handshake 未连通）返回 `Err` | 状态混乱不可测 |
| S2 | Hello 校验：`version ≠ 1` 直接断开（不发 Welcome，返回 `Error::Protocol`）；`auth_method ≠ 0` 回 `Welcome{auth_status: 拒绝}` 并断开（挑战路径是 T19）；host 把请求参数**逐字段 min 夹紧到 `caps`** 再裁定，分辨率过 `to_even` | 协商出非法/越界管线配置 |
| S3 | client 以 **Welcome 裁定值为准**（`requested` 不生效）；Welcome 的 `session_id` 与本地不符即断；`auth_status ≠ 0` 即断（本卡只认通过） | 两侧管线配置漂移 / 会话号错配 |
| S4 | **会话独占 `transport.poll`**，三通道单一分发点：媒体只走 datagram、控制只走 reliable、输入只走 urgent（host 注入用 `rc_input::inject_message`，不得调用 `pump_receiver`——其内部 poll 会吞同批控制/断连事件）；`Connected` 事件忽略（transport 层事件，会话状态由握手推进）；Streaming 之前到达的媒体/输入事件**丢弃并计数**，不缓存 | 事件丢失、输入/控制队头阻塞回归 |
| S5 | 驱动循环照 run_real 形态：`tick(now_us)→逐条 send_datagram`、`take_keyframe_request→IdrRequest(DecoderNeedKeyframe)`、`pump_sender` 每拍注入 `now`；空闲挂起（host 用 `wait_event`）零自旋 | 延迟回归 / CPU 空转 |
| S6 | **停机顺序红线（两侧各自成立）**：host = 发 Bye（尽力、短超时不等待确认）→ `stop_capture()` → tick 排空到静默（交接通道空 + `has_pending_encodes()==false`，500ms 上限）→ `shutdown()` → 末轮 tick 收编码完成并发送（照 `lib.rs:424-460`；反序的后果见 `host.rs:486-512` 文档）；client = 发 Bye → 停 pump → 继续收/推数据报到静默或 500ms 上限 → `finish_in_flight()` → `controller.shutdown()`。停机后双端 `trace.summary().unfinished == 0` 且无「授权帧凭空消失」（`sched_stats().encoded` == 有结局帧数） | 账目破洞、授权帧蒸发、铁律 1/P2 回归 |
| S7 | 会话层不新增任何深度 >2 的队列；host tick 产出的数据报**当拍发完**，不缓存积压（rc-net pacing 队列与管线在途上限已承担背压） | 铁律 1 违反 |
| S8 | 已知但属后续卡的消息（`ProbePing`/`ProbePong`/`SetBitrate`/`BwProbeRequest`/`BwProbeData`/`InputAck`/`ReconnectRequest`/`AuthChallenge`/`AuthResponse`/`ClipboardNotify`）到达：**忽略**（stderr 记一次，限流不刷屏），状态不变、不断连——保证 T15/T16/T17 增量交付不破坏互通；proto 解码失败的**脏控制消息**：返回 `Error::Protocol` 并走停机（宁拒收不猜测，对齐 rc-input 安全边界纪律） | 增量交付互相打断 / 毒消息无限重试 |
| S9 | 时间注入（铁律 4）：两侧管线共享**同一** `SessionClock` 闭包（`set_clock` 注入，`host.rs:340`/`controller.rs:95`）；会话零点锚**首个采集帧**（照 `run_real` 的 `origin_slot` 模式 `lib.rs:342-358`；真 macOS 链路的 `clock_origin_us` 路径见 `lib.rs:246-263`，由调用方在 cfg 里给定，本卡不读平台钟）；闭包返回值用 `u32::try_from(...).unwrap_or(u32::MAX)` 饱和，不 `as` 截断 | 铁律 4 违反 / 时间轴漂移 |
| S10 | 纯 Rust、无 `unsafe`、无新第三方依赖（quinn 等都在 rc-net 里）；`#![deny(missing_docs)]`；注释/文档中文 | 项目纪律 |

## 不要做什么

- ❌ 不实现探测/看门狗/重连（T15/T16/T17）——S8 列的消息到站即忽略；不做 `HostPipeline::set_bitrate`（rc-pipeline 的唯一 M2 改动点归 T15）。
- ❌ 不改 rc-net、rc-types、rc-pipeline 任何一行；rc-input 只加 `inject_message` 一个函数（`pump_receiver` 行为不变，rc-input 既有测试断言一个不改）。
- ❌ 不做 CLI 子命令、会话报告 JSON schema 与 gate（T18）；本卡 `RunSummary` 只是内存结构，不落盘、不回写 TASK.md 实测数字。
- ❌ 不做 Timestamp 70 分钟饱和守卫与 `Bye(Refresh)` 的自动触发（T17）——本卡只实现 Bye 的收、发与停机序列；`StopReason::ByeReceived(Refresh)` 原样透传给上层。
- ❌ 不改 `proto.rs`、不新增控制消息（线格式是 T13 的，唯一事实源）。
- ❌ 不做鉴权挑战、剪贴板（T19/T20 占号）、双机真机实测（T18）。
- ❌ 不把 `connect_to_first_frame_ms` 等 M2 指标口径写进本卡（那是 T18 的报告定义与 localhost 实测）。

## 测试清单

**离线确定性单测（mock transport + 假后端，无网络无 sleep）：**

host_session.rs 内嵌：

| 测试名 | 验证 |
|---|---|
| `host_handshake_replies_welcome_with_clamped_params` | S2/S3：Hello 请求超过 caps → Welcome 回夹紧值；session_id 回显一致；state==Streaming |
| `host_rejects_hello_with_wrong_version` | S2：version=2 → `Err(Protocol)`、不发 Welcome、连接关闭 |
| `host_rejects_hello_with_auth_method_nonzero` | S2：回 `Welcome{auth_status: 拒绝}` 后断开，不进 Streaming |
| `host_applies_idr_request_and_emits_idr_datagram` | S4/S5：投递 `IdrRequest` → `request_idr()` 生效，随后 tick 产出的下一个数据报包头 `is_idr()`（用 `rc_pipeline::host::datagram_header` 断言——它是 host.rs 模块自由函数（host.rs:624），rc-pipeline/lib.rs 只 re-export `HostPipeline`/`ControllerPipeline`，写 `rc_pipeline::datagram_header` 编译不过） |
| `host_injects_urgent_input_events` | S4：投递 `ReliableUrgent`（rc-input 编码的点击事件）→ 记录型注入器收到且序号/内容一致；**同一事件若走 Reliable 则不得注入** |
| `host_processes_control_and_input_from_same_poll_batch` | S4 接缝钉死：`IdrRequest` 与 urgent 输入**同批**投递 → 两者都被处理（`pump_receiver` 路径会吞掉前者，此测试防回归） |
| `host_close_sends_bye_then_drains_in_order` | S6：close 中途有在途编码 → Bye 先发出；停机后 `unfinished==0` 且 `sched_stats().encoded` == 计数编码器实际完成数（无凭空消失） |
| `host_dirty_control_message_closes_session` | S8：Reliable 投递 proto 解码必败的字节 → `Err(Protocol)`、有序停机、`unfinished==0` |
| `host_ignores_messages_reserved_for_later_cards` | S8：投递 `ProbePing`/`SetBitrate` → 状态不变、不断连、stderr 记录一次 |
| `host_stops_on_bye_or_disconnect_mid_stream` | S6：流中投递 `Bye(Normal)` / `Disconnected` → tick 报告 `closed`，停机后 `unfinished==0` |
| `host_streaming_continues_when_network_is_silent` | 象限 A（坑 2）：无任何网络事件、假采集持续供帧 → run 若干拍数据报持续发出、不饿死 |
| `host_handles_input_when_capture_is_static` | 象限 B（坑 7）：`FakeContent::Static`（零编码属正常）+ urgent 事件持续到达 → 注入照常、循环不空转不卡死 |
| `constant_session_clock_pins_all_trace_marks` | S9：注入恒返 42 的会话钟 → 管线 trace 各打点全为 42（证明未读系统钟） |

client_session.rs 内嵌：

| 测试名 | 验证 |
|---|---|
| `client_handshake_sends_hello_and_adopts_welcome_params` | S3：Hello 字段正确（version/session_id/请求参数）；Welcome 裁定值覆盖 `requested`；state==Streaming |
| `client_rejects_welcome_with_wrong_session_id` | S3：回显不符 → `Err`、断开 |
| `client_rejects_welcome_with_auth_denied` | S3：`auth_status=2` → `Err`、不建管线 |
| `client_sends_input_over_urgent_stream_only` | S4/S5：脚本捕获点击（关键事件立即成批）→ `urgent_sent` 非空、`reliable_sent` 无输入消息 |
| `client_requests_idr_when_decoder_needs_keyframe` | S5 基本接线：`FakeDecoderConfig{drop_first_n: 1}` 丢首 IDR（`crates/rc-decode/src/fake.rs` 定点断链）→ P 帧触发 `DecoderNeedKeyframe` → `IdrRequest{DecoderNeedKeyframe}` 发往 reliable 流 |
| `client_close_sends_bye_and_finishes_in_flight` | S6：Bye 发出、残帧判 `LostInNetwork`、`unfinished==0`、state==Closed |
| `client_stops_on_bye_from_host` | 收 Bye → tick 报告 `closed(StopReason::ByeReceived(Normal))`，停机完整 |

tests/session_roundtrip.rs（testkit::pair 双工对接）：

| 测试名 | 验证 |
|---|---|
| `mock_pair_session_roundtrip_streams_and_injects` | 端到端闭环：host+client 经 pair 互连，假后端，注入式时钟推进若干拍 → client `frames_displayed > 0`、host 注入器收到 client 事件、首帧数据报为 IDR、双端停机后 `unfinished==0` |
| `mock_pair_close_one_side_closes_the_other` | 一侧 `close(Normal)` → 对侧 run/tick 观测 `ByeReceived` 并完成停机 |

tests/quic_loopback.rs（真 QUIC over UDP 回环，无需权限，CI 可跑；证书照 rc-net tests/certs 模式）：

| 测试名 | 验证 |
|---|---|
| `quic_handshake_streams_and_closes_over_loopback` | 真 `QuicEndpoint::accept/connect`（127.0.0.1:0）→ 握手 → 假后端流 1–2 秒 → client `frames_displayed > 0` → client `close(Normal)` → 双端 Closed、`unfinished==0` |
| `quic_host_survives_client_abrupt_disconnect` | client 直接 drop transport（不发 Bye）→ host 观测 `Disconnected` → 有序停机、`unfinished==0`、不 panic 不挂死 |

## 验收命令

```bash
# 内循环（M0 纪律：rc-session 依赖扇出大，日常只跑 check）
cargo check -p rc-session

# 单测 + 集成
cargo test -p rc-session

# 唯一被改的既有 crate 的回归（inject_message 提取后行为必须不变）
cargo test -p rc-input

# 全仓 + lint
cargo test --workspace
cargo clippy -p rc-session --all-targets -- -D warnings
cargo clippy -p rc-input --all-targets -- -D warnings
```

## 验收标准

- [ ] `cargo test -p rc-session` 全绿（含两个集成测试文件）；`cargo test -p rc-input` 既有测试**不改断言**全绿；`cargo test --workspace` 全绿。
- [ ] mock pair 闭环：假后端下 client 上屏 > 0、host 注入器收到事件、首帧数据报 `is_idr()`、双端停机后 `unfinished == 0`。
- [ ] 停机红线：流中关闭（含在途编码）时无「授权帧凭空消失」（`sched_stats().encoded` == 有结局帧数）且采集线程先于编码线程停止（行为断言，见 `host_close_sends_bye_then_drains_in_order`）。
- [ ] version≠1、session_id 不回显、auth 拒绝三条路径各有独立测试且均断开连接。
- [ ] 常量会话钟测试通过（铁律 4：管线打点不读系统钟）。
- [ ] 同批「控制 + 输入」事件不丢（S4 接缝测试通过）。
- [ ] clippy 两条命令无警告；无 `unsafe`；无新第三方依赖（`Cargo.toml` 只增 workspace 内 path 依赖）。

## 参考实现

| 读这个 | 学什么 |
|---|---|
| `crates/rc-loopback/src/lib.rs` `run_real`（175-515；驱动循环 336-423、收尾 424-460、时钟 337-360） | **驱动循环与停机顺序的直接模板**：本卡就是把它进程间化 |
| `crates/rc-pipeline/src/host.rs`（tick:367、request_idr:462、wait_event:355、stop_capture/shutdown:486-512、set_clock:340） | 管线半场 API 语义与「先停采集再排空再停编码」的因果 |
| `crates/rc-pipeline/src/controller.rs`（push_datagram:105、take_keyframe_request:214、finish_in_flight:238、MAX_PARTIALS:56） | 重组/解码/呈现一拍完成；停机兜底 |
| `crates/rc-input/src/channel.rs`（pump_sender:83、pump_receiver:111 及其错误隔离文档） | 输入泵语义、`inject_message` 的提取落点 |
| `crates/rc-net/src/quic.rs`（connect:253、accept:303、三通道与成帧文档 1-46） | transport 的阻塞点与事件形态 |
| `crates/rc-net/tests/common/mod.rs` | 真 QUIC 回环测试的证书与连接对模式（本卡集成测试照抄此风格） |
| [T09 卡](T09-net-quic.md) | 卡片红线编号与「不要做什么」的风格 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | `pump_receiver` 内部自己 `transport.poll`（channel.rs:119），同批的非 urgent 事件（含 `Disconnected`、Bye）被静默丢弃 | 会话独占 poll；urgent 注入走新提取的 `rc_input::inject_message`；`host_processes_control_and_input_from_same_poll_batch` 钉死接缝 |
| 2 | quinn `accept`/`connect` 阻塞（quic.rs:253/303）；驱动循环里网络等待可能饿死 `host.tick` 的事件等待 | 握手在循环外完成；循环内 `poll` 只给 1ms 级预算 + `wait_event`（≤10ms）承担空闲挂起；两个象限测试（网络静默+采集活跃 / 网络活跃+采集静默）必测 |
| 3 | 会话零点锚错位置（如锚在连接建立）会把管线启动延迟整体吃进零点偏移 | 照 `run_real`：假后端锚 `host.first_capture()`（origin_slot 模式）；真 macOS 链路由调用方在 `PipelineConfig.clock_origin_us` 给定 `host_clock_us()` 原点（lib.rs:246-263），本卡不读平台钟 |
| 4 | 假采集瞬时出帧：不节流会把逻辑时间戳跑到墙钟前面 | 假后端必须 `with_backends_paced(..., true)`（`HostSessionConfig::pace_capture`，照 `run_real` 传 `!real_ok` 的做法）；真采集 `next_frame` 自带阻塞，传 false |
| 5 | 双时间轴：管线打点要 u32 会话钟，`pump_sender` 批窗口要 `Instant` | 会话钟走注入闭包；`Instant` 经 `ClientSession::tick` 参数传入并在文档中要求同轴；`run()` 内取 `Instant::now()` 是驱动器的合法时钟源（与 `run_real` 的 `start.elapsed()` 同一纪律）——**后续卡（T15/T20 等）的铁律 4 grep 自检必须豁免 `client_session.rs`**：该文件命中 `Instant::now` 属本行认可的驱动钟，不算违规；除此之外任何文件零命中 |
| 6 | `Timestamp` u32 约 71.6 分钟饱和（契约 §1） | 本卡不做守卫（T17 发 `Bye(Refresh)`）；会话钟闭包返回值按 `u32::try_from(...).unwrap_or(u32::MAX)` 饱和，禁止 `as` 截断 |
| 7 | 静屏零编码（铁律 2）：`FakeContent::Static` 下 `host.tick` 返回 `None` 属正确行为 | 象限 B 测试用它造「采集静默」；不要把无产出当死循环修 |
| 8 | 控制消息体与长度前缀的关系：rc-net reliable 流自带 u32 LE 长度前缀与消息边界（quic.rs 模块文档 23-27），proto 消息体 = 1 字节类型 + 载荷，**不含**自己的长度前缀 | 两层前缀叠加无害（与 rc-input u16 前缀同理，channel.rs「成帧关系」节）；会话层只调 `proto::encode/decode`，不手搓字节 |
| 9 | rc-session 依赖 rc-net/rc-pipeline/rc-input 三 crate，编译扇出大 | 内循环只跑 `cargo check -p rc-session`；全仓验证留给验收命令 |
| 10 | `run_real` 的 `sync_channel(2)`（lib.rs:364）换成 transport 后，容量纪律的归属 | rc-net 内部队列 + 管线在途上限（`max_in_flight=2`、`MAX_PARTIALS=2`）已承担背压；会话层不得再堆一层无界 Vec 缓存数据报（S7） |
| 11 | mock pair 两端共用同一会话钟闭包，trace 天然同轴；真跨进程后 host/client 两钟不同轴 | `controller.push_datagram` 会把 host 域 `capture_ts` 直接写进 Captured 标记（controller.rs:176-178），跨进程 e2e 需时钟偏移换算——那是 T15（探测）/T18（报告）的事，本卡不换算、不在测试里断言跨钟 e2e 数值 |
| 12 | `MockTransport::poll` 忽略 timeout 即刻返回 | 确定性优先；N4 超时语义由 rc-net 测试覆盖，本卡测试不验证它 |

## 依赖

```toml
# crates/rc-session/Cargo.toml（在 T13 骨架上补齐）
[dependencies]
rc-types = { path = "../rc-types" }
rc-net = { path = "../rc-net" }
rc-pipeline = { path = "../rc-pipeline" }
rc-input = { path = "../rc-input" }
rc-trace = { path = "../rc-trace" }   # 只读 trace 汇报（unfinished 断言等）

[dev-dependencies]
rc-capture = { path = "../rc-capture" }  # FakeBackend（Static/Dynamic 供帧）
rc-encode = { path = "../rc-encode" }    # FakeEncoder
rc-decode = { path = "../rc-decode" }    # FakeDecoder（drop_first_n 定点断链）
rc-render = { path = "../rc-render" }    # FakeRenderer
```

前置卡：**T13**（crate 骨架 + proto.rs）。

## 与后续卡的接口

- **T15**：`SetBitrate`→`HostPipeline::set_bitrate` 转发、`ProbePing/Pong`（RTT/时钟偏移）挂在 `HostSession::tick`/`ClientSession::tick` 的控制分发改上（本卡 S8 的忽略位即它们的接入点）；初始码率在握手后经 `request_idr` 同款 reliable 路径下发。
- **T16**：看门狗与 IdrRequest 节流/去抖包住本卡的 `ClientSession::request_idr` 基本接线；`IdrAck` 的消费也在本卡的控制分发位接入。
- **T17**：`Bye(Refresh)` 触发守卫读本卡会话钟；`StopReason::ByeReceived(Refresh)` 与 `Disconnected(String)` 是重连逻辑的输入。
- **T18**：CLI 用 `new/accept_and_handshake/run`（真网络）与 `with_parts`（scripted 模式）组装双进程；会话报告 JSON 取 `HostRunSummary`/`ClientRunSummary` + `pipeline()` 的 trace。
