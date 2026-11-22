# T18 rc-cli host/client 双进程直连 + localhost 实测

> 给 `rc-cli` 加 `host` / `client` 两个子命令，把 rc-session 会话层进程间化：
> 两个独立进程走真 QUIC over UDP loopback 跑完整会话，client 侧落盘带
> session 段的会话报告并按 Gate 决定 exit code；在本机完成 localhost 双进程
> 实测并回写 TASK.md。

## 背景（为什么做这个）

M2 的会话层（T13–T17）已经把控制协议、HostSession/ClientSession、探测与
码率、IDR 可靠通道、断线重连全部做完，但它们至今只在进程内被测试。会话层
真正的形态是**两个操作系统进程**：被控端与控制端各持一套完整的
rc-net QUIC 栈与 rc-pipeline 管线，中间只有网络。不做双进程直连，
「重连」「输入单向延迟」「RTT 口径」这些 M2 出口指标就永远测不到真值。

本卡是 M2 的**集成出口**：CLI 薄壳接线（rc-cli 现有 3 个子命令
（Loopback / Probe / Version，`crates/rc-cli/src/cmd/mod.rs:35-43` 的
`Command` 枚举）共约 1044 行，不许变厚）+ testkit（脚本输入源 / 记录注入器，让 localhost 实测免权限、
CI 可跑）+ client 会话报告（对齐 rc-loopback 报告口径，跨钟域换算有单测
钉死）+ localhost 双进程实测。

前置：T13（proto）、T14（会话状态机与驱动循环）、T15（探测与初始码率）、
T16（IDR 可靠通道）、T17（断线重连）全部完成。本卡**不新增会话机制**，
只做接线、测量设施与实测。

## 平台要求

**开发与 CI：全平台**（默认假后端 + record 注入，无需任何 macOS 权限）。
**真后端冒烟（`--backend real`）：仅 macOS**，且必须经
`./scripts/macos-dev-run.sh cli` 执行（AGENTS.md §八：直接子进程吃不到
`RabbitControlDev.app` 的 TCC 授权）；host 真采集需幕布供帧，client 真渲染
会弹 Metal 窗口。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/testkit.rs` | 修改（T14 已建） | **追加** `ScriptedCapture` / `RecordingInjector` / `batch_is_key`；`MockTransport` 与 `pair()` 沿用 T14 交付的既有符号（不重定义类型、不另造 `transport_pair()` 平行名——T14 交付物表已冻结符号名） |
| `crates/rc-session/src/report.rs` | 新建 | `SessionObservations` / `SessionReport` / `SessionGateResult` / 手写 `to_json` |
| `crates/rc-session/src/dev_cert.rs` | 新建 | 内嵌开发证书：`dev_identity()` / `DEV_SERVER_NAME`（client 固定信任直接用 `DEV_CERT_PEM` 常量传 `ClientSession::new`，见契约 §1——rc-session 不依赖 rustls） |
| `crates/rc-session/certs/dev-cert.pem`、`dev-key.pem` | 新建 | 从 `crates/rc-net/tests/certs/` 拷贝同一对材料（EC P-256，SAN 含 `rabbitcontrol`/`localhost`/`127.0.0.1`），文件头注释写明仅开发用 |
| `crates/rc-session/src/lib.rs` | 修改 | 挂载 `report` / `dev_cert` 两个模块（`testkit` 已由 T14 挂载，骨架是 T13 的） |
| `crates/rc-cli/src/cmd/mod.rs` | 修改 | 注册 `Host`/`Client` 子命令；`HostArgs`/`ClientArgs`；`InjectMode`/`InputMode` 枚举与解析 |
| `crates/rc-cli/src/cmd/host.rs` | 新建 | `host` 子命令实现 |
| `crates/rc-cli/src/cmd/client.rs` | 新建 | `client` 子命令实现 |
| `crates/rc-cli/Cargo.toml` | 修改 | 加 `rc-session`、`rc-input`；macOS 加 `rc-input-macos` |
| `crates/rc-session/tests/localhost.rs` | 新建 | 进程内双端（真 QUIC over 127.0.0.1）会话集成测试 |
| `crates/rc-cli/tests/host_client.rs` | 新建 | 双子进程复跑指标（`CARGO_BIN_EXE_rc-cli`） |
| `TASK.md`、`docs/reference/tickets/README.md` | 修改 | §0/§12/§13 回写实测数字；卡片状态表加 T18 |

## 依赖

```toml
# rc-cli/Cargo.toml 新增（rc-session 依赖集由 T13 建好，本卡不动）
rc-session = { path = "../rc-session" }
rc-input = { path = "../rc-input" }

[target.'cfg(target_os = "macos")'.dependencies]
rc-input-macos = { path = "../rc-input-macos" }   # --input-mode real / --inject-mode real
```

不引 serde / serde_json：报告 `to_json` 手写（对齐
`crates/rc-loopback/src/report.rs:339-424` 的 `format!` + `escape` 风格）。

rc-session / rc-cli 均**不直接依赖 rustls**：`CertificateDer` 等证书对象
一律在 rc-net 边界内构造——host 侧 `rc_net::quic::Identity::from_pem`，
client 侧把 PEM 文本传 `ClientSession::new`（其内部经
`Identity::certs_from_pem` 解析，T14 契约）；两 crate 的公开 API 都不
出现 rustls 类型。

## 对前置卡的消费面（不重新定义，只声明本卡怎么用）

以下类型由 T13–T17 交付，**确切签名以对应卡契约为准**；本卡按此消费，
实现时若发现命名有出入，改本卡的调用处对齐前置卡，不改前置卡：

| 消费项 | 来源卡 | 本卡用法 |
|---|---|---|
| `rc_session::proto` 控制消息（Hello/Welcome/IdrRequest/IdrAck/ProbePing/Pong/SetBitrate/Bye/ReconnectRequest…） | T13 | 不直接编解码，经会话层 API |
| `HostSession` / `ClientSession` 状态机与驱动循环 | T14 | host.rs / client.rs 构造后驱动到 duration 结束；停机顺序红线（先停采集→排空→停编码）由会话层保证，CLI 不另写 |
| RTT / 时钟偏移探测、初始码率链 | T15 | 报告 `session.rtt_us` / `clock_offset_us` 取探测结果 |
| 停滞看门狗、IdrRequest/IdrAck 计数 | T16 | 报告 `session.watchdog_fires` / `idr_requests` / `idr_acks` |
| 重连（`--reconnect-at` 触发）、饱和刷新 | T17 | 报告 `session.reconnect` 段（attempts/resumed/recovery_ms） |
| client 侧观测访问器：`controller.trace()` 级别的逐帧标记、逐数据报 `MediaHeader`（capture_ts/encode_done_ts）、重组表峰值、InputAck 样本、`NetStats` | T14–T17 | 组装成 `SessionObservations` 后交给 `SessionReport::from_observations` |

管线侧 API 零改动复用（与设计师结论一致）：host 侧
`HostPipeline::tick(now_us)`（`crates/rc-pipeline/src/host.rs:367`）产出
`Vec<Vec<u8>>` → 逐条 `transport.send_datagram`；client 侧
`NetEvent::Datagram` → `ControllerPipeline::push_datagram`
（`crates/rc-pipeline/src/controller.rs:105`）；每拍
`take_keyframe_request()`（`controller.rs:214`）→ 发 `IdrRequest`；
输入走 `InputChannel::pump_sender`（`crates/rc-input/src/channel.rs:83`）与
`pump_receiver`（`channel.rs:111`）。本卡对 rc-net / rc-pipeline **只读**。

## API 契约

### 1. `rc-session/src/dev_cert.rs`（内嵌开发证书）

```rust
/// 内嵌开发证书的 PEM 文本（`include_str!`，照
/// `crates/rc-net/tests/common/mod.rs:21-22` 的模式）。
///
/// **仅开发 / localhost 用**：私钥公开在仓库里，不构成任何安全边界；
/// 真实会话的证书体系是 M5 的事。材料与 rc-net tests/certs 同一对
/// （EC P-256，有效期到 2126 年，SAN：rabbitcontrol / localhost / 127.0.0.1）。
pub const DEV_CERT_PEM: &str = include_str!("../certs/dev-cert.pem");
pub const DEV_KEY_PEM: &str = include_str!("../certs/dev-key.pem");

/// 证书 SAN 内的服务名（与 `rc_net::quic::ALPN` 一样是两端约定）。
/// client 默认用它做 `QuicEndpoint::connect` 的 `server_name`。
pub const DEV_SERVER_NAME: &str = "rabbitcontrol";

/// 默认身份（host 与 client 的 QuicEndpoint 都要一份 Identity——
/// `QuicEndpoint::new` 无论角色都构建服务端配置，见 quic.rs:201-235）。
pub fn dev_identity() -> Result<rc_net::quic::Identity>;

/// client 固定信任的服务端证书（PEM 文本，即 [`DEV_CERT_PEM`]）。
/// 用法：直接传 `ClientSession::new(cfg, server_cert_pem, server_name,
/// clock)`（T14 契约），由会话层内部经 `rc_net::quic::Identity::certs_from_pem`
/// （quic.rs:171-177）解析成 `CertificateDer` 链、再喂
/// `QuicEndpoint::connect` 的 `server_certs`（quic.rs:253-259）。
/// **rc-session 不依赖 rustls，本模块与 crate 公开 API 均不出现
/// rustls 类型**（返回 `Vec<CertificateDer>` 会让 rc-session 直接依赖
/// rustls——T13/T14/T18 依赖清单都没有它；若未来确需直接持有证书链
/// 对象，先由 rc-net re-export 该类型，不在本 crate 引 rustls）。
pub fn dev_server_cert_pem() -> &'static str;
```

模块文档须写明再生成命令（openssl 自签 + SAN 三项），保证来源可追溯。

### 2. `rc-session/src/testkit.rs`

```rust
/// 脚本输入源（client `--input-mode scripted`）：实现
/// [`rc_input::InputCapture`]，按墙钟回放时间线。
///
/// 测量基础设施定位对齐 rc-loopback 的 `WallEncoder`（真实睡眠模拟硬件）：
/// IO 边界允许读墙钟；纯逻辑判定见 [`batch_is_key`]。
pub struct ScriptedCapture { /* 私有 */ }

impl ScriptedCapture {
    /// 内置标准脚本：8ms 间隔 MouseMove + 每 500ms 一次 MouseButton
    /// 按下/抬起（关键事件强制成批）。覆盖「移动合并」与「关键事件
    /// 立即 flush」两条批处理路径，并给 InputAck 提供持续样本。
    pub fn standard() -> Self;
    /// 自定义时间线：`(距开始的微秒, 事件)` 列表，时刻必须非降
    /// （违反返回 `Error::InvalidConfig`）。脚本播完后返回空（会话继续）。
    pub fn from_timeline(timeline: Vec<(u64, InputEvent)>) -> Result<Self>;
    /// 脚本总时长（微秒）。
    pub fn duration_us(&self) -> u64;
}

impl rc_input::InputCapture for ScriptedCapture { /* 到期事件返回，未到期空 */ }

/// 记录型注入器（host `--inject-mode record`）：实现
/// [`rc_input::InputInjector`]，不真注入——localhost 实测无需辅助功能权限。
pub struct RecordingInjector { /* 私有 */ }

impl RecordingInjector {
    pub fn new() -> Self;
    /// 已记录事件总数（host stderr 摘要与双子进程测试断言用）。
    pub fn injected_count(&self) -> u64;
    /// 含关键事件的批数。与 InputAck 的 `key_batch_seq` 同一口径计数：
    /// 两侧判定不一致会让输入单向延迟采样错位（见 [`batch_is_key`]）。
    pub fn key_batches(&self) -> u64;
    /// 取走全部已记录事件（测试断言用，取走后清空）。
    pub fn take_events(&mut self) -> Vec<InputEvent>;
}

impl rc_input::InputInjector for RecordingInjector { /* 记录 + set_screen_size 记尺寸 */ }

/// 批内是否含关键事件：按下/抬起（MouseButton/Key）、滚轮（MouseWheel）
/// ——即 MouseMove 与 Sync 之外的全部。
///
/// **共享口径函数**：InputAck「关键批」的判定（client 发送侧统计与
/// host 回执侧计数）必须与 testkit/真实 InputBridge 用同一个实现，
/// 不得各写一套（设计师风险 6）。
pub fn batch_is_key(events: &[InputEvent]) -> bool;

/// 进程内假传输 `MockTransport` 与对接件 `pair()` **沿用 T14 已交付的
/// testkit 符号**（`crates/rc-session/src/testkit.rs`）：本卡只在该模块
/// **追加** `ScriptedCapture` / `RecordingInjector` / `batch_is_key`，
/// 不重定义类型、不另造 `transport_pair()` 平行名——会话层单测、
/// CLI scripted 模式与双子进程测试共用同一实现。
///
/// （`pair()` 语义见 T14 卡：A 的 send_* 依次出现在 B 的 poll 里、
/// 反之亦然，无损无延迟、容量不设界。）
```

### 3. `rc-session/src/report.rs`（client 会话报告 + Gate）

```rust
/// 单帧的跨钟域观测（报告计算的最小输入，纯数据）。
///
/// `captured_us` / `encode_done_us` 来自媒体包头（host 会话钟域）；
/// `received_us` / `decoded_us` / `presented_us` 来自 client 管线 trace
/// （client 会话钟域——注意 `controller.rs:176-178` 会把 host 域的
/// capture_ts 直接写进 trace 的 Captured 标记，**报告侧必须自己持
/// 包头值并做偏移换算**，不能拿 trace 的 Captured 直接减 Presented）。
pub struct FrameObservation {
    pub seq: rc_types::FrameSeq,
    pub outcome: Option<rc_trace::Outcome>,
    pub captured_us: u32,
    pub encode_done_us: u32,
    pub received_us: Option<u32>,
    pub decoded_us: Option<u32>,
    pub presented_us: Option<u32>,
}

/// 重连观测（T17 机制的结果，`--reconnect-at` 演练时填充）。
pub struct ReconnectStats {
    /// 触发的重连次数（含主动演练与被动断线）。
    pub attempts: u64,
    /// 末次重连是否以 resume=true 被接受（Welcome host_flags bit0）。
    pub resumed: bool,
    /// 重连成功到首个 IDR 帧上屏的毫秒数。
    pub recovery_ms: Option<u64>,
}

/// client 会话驱动循环结束时交出的全部原始观测（铁律 4：报告组装
/// 只吃注入数据，不读任何时钟）。
pub struct SessionObservations {
    /// 全部帧的跨钟域观测（含未上屏帧，供 lost/decode_errors 计数）。
    pub frames: Vec<FrameObservation>,
    /// 请求的测量时长（微秒）——fps 分母，不含排空（对齐
    /// rc-loopback「fps_uses_measurement_duration_not_drain_time」口径）。
    pub duration_us: u32,
    /// 从 client 进程 Endpoint 创建**之前**到首帧 Presented 的微秒数。
    /// None = 全程无帧上屏。计时起点定在 Endpoint 创建之前并写死在
    /// 报告定义里，避免后续真机口径漂移（设计师风险 7）。
    pub connect_to_first_frame_us: Option<u64>,
    /// 探测得到的平滑 RTT 与时钟偏移（host 钟 − client 钟，可为负）。
    /// None = 尚无探测样本。
    pub rtt_us: Option<u32>,
    pub clock_offset_us: Option<i64>,
    /// 输入单向延迟样本（InputAck 换算：inject_done_us + offset − 发送时刻）。
    pub input_one_way_samples_us: Vec<u32>,
    pub idr_requests: u64,
    pub idr_acks: u64,
    pub watchdog_fires: u64,
    /// 重组表深度峰值（铁律 1：恒 ≤ MAX_PARTIALS = 2，controller.rs:56）。
    pub max_reassembly_depth: u32,
    pub net: rc_net::NetStats,
    pub reconnect: ReconnectStats,
    pub session_id: u64,
}

/// 会话 Gate 门槛（对齐 M2 出口指标；client 口径，host 不判 Gate）。
pub struct SessionGateResult {
    pub connect_to_first_frame_ms_max: f64, // 800.0
    pub p50_ms_max: f64,                    // 25.0（M2 出口）
    pub p99_ms_max: f64,                    // 40.0（M2 出口）
    pub displayed_fps_min: f64,             // fps × 0.9（随目标帧率，不写死 54）
    pub input_one_way_margin_ms: f64,       // 5.0：input ≤ RTT/2 + 5ms
    pub reconnect_recovery_ms_max: f64,     // 500.0，仅发生过重连时判定
    pub max_queue_depth: u32,               // 2（铁律 1）
    pub passed: bool,
    pub failures: Vec<String>,
}

/// client 会话报告。JSON 键名对齐 rc-loopback 报告（40-diagnostics.md §4
/// 口径），新增 `session` 段承载双机会话才有的量。
#[derive(Debug, Clone)]
pub struct SessionReport { /* 私有字段，经 from_observations 构造 */ }

impl SessionReport {
    /// 由原始观测组装报告并跑 Gate。所有换算在此完成：
    /// - e2e = Presented(client) − (captured(host) + clock_offset)；
    /// - network = Received(client) − (encode_done(host) + clock_offset)；
    /// - encode = encode_done − captured（同为 host 域，无需换算）；
    /// - decode / present 取 trace 相邻标记差；
    /// - convergence / degradation 用整 1 秒滑动窗口（复制
    ///   rc-loopback/src/lib.rs:717-764 `scan_presented` 的语义：
    ///   窗口最小 5 帧、收敛 30s 上限；依赖方向不允许 rc-session →
    ///   rc-loopback，故同语义重实现并在注释注明来源）。
    /// 偏移缺失（无探测样本）时：有上屏帧 → Gate 判失败并写明
    /// 「无时钟偏移样本」；绝不静默跳过换算。
    pub fn from_observations(
        cfg: &SessionConfigSnapshot,
        obs: &SessionObservations,
    ) -> Self;

    /// Gate 判定（构造时已跑一次；此方法供测试注入超标值复核）。
    pub fn apply_gate(&mut self);

    /// 输出固定 schema 的 JSON（手写，不引 serde；escape 对齐
    /// rc-loopback/src/report.rs:428-442）。
    pub fn to_json(&self) -> String;
}

/// 报告 config 段的配置快照。
pub struct SessionConfigSnapshot {
    pub resolution: rc_types::Resolution,
    pub fps: u32,
    pub duration_s: f64,
    /// 实际后端标签（"fake" / "sck+videotoolbox+videotoolbox+metal" 等）。
    pub backend: String,
    pub connect_addr: std::net::SocketAddr,
    pub input_mode: &'static str, // "scripted" / "none" / "real"
}
```

**client 报告 JSON schema（键名固定，验收脚本依赖）：**

```json
{
  "config": {"resolution":"1920x1080","fps":60,"duration_s":30.0,
             "backend":"fake","connect":"127.0.0.1:9901","input_mode":"scripted"},
  "latency_ms": {"p50":15.2,"p95":18.1,"p99":21.4},
  "fps": {"displayed":59.8},
  "frames": {"displayed":1794,"lost_in_network":0,"decode_errors":0,"unfinished":0},
  "breakdown_ms": {"encode":5.0,"network":1.2,"decode":3.0,"present":2.0},
  "convergence_ms": 1000, "degradation_ms": null,
  "max_queue_depth_seen": 2,
  "session": {
    "session_id": 1234567890,
    "connect_to_first_frame_ms": 62.5,
    "rtt_us": 480, "clock_offset_us": -35,
    "input_one_way_p50_ms": 0.31,
    "idr_requests": 1, "idr_acks": 1, "watchdog_fires": 0,
    "loss_rate": 0.0, "bytes_sent": 1048576, "bytes_recv": 9482112,
    "reconnect": {"attempts":0,"resumed":false,"recovery_ms":null}
  },
  "gate": {"connect_to_first_frame_ms_max":800.0,"p50_ms_max":25.0,
           "p99_ms_max":40.0,"displayed_fps_min":54.0,
           "input_one_way_margin_ms":5.0,"reconnect_recovery_ms_max":500.0,
           "max_queue_depth":2,"passed":true,"failures":[]}
}
```

与 loopback 报告的差异**必须**在模块文档写明：`frames` 只含 client 可观测
计数（dropped_* 是 host 调度器决策，client 不可知，不编造）；
`breakdown_ms` 只含 4 个可观测段（diff/sched_queue/packetize 是 host 侧
专有段，不出现也不补 0）；`convergence_ms`/`degradation_ms` 记录但不进
Gate（首帧门槛已覆盖爬坡职责，双机含网络抖动，1s 收敛口径易 flaky）。
`input_mode == "none"` 时跳过输入门槛判定（`session` 段如实记录）。

**host 侧报告**（`--report` 可选，无 `gate` 键）：`config`
（含 `listen`）+ `session`（`session_id`、`in_flight_peak`、
`injected_events`、`key_batches`、`idr_requests_received`、
`connected_ms`）。host 不判 Gate——被控端没有 e2e 观测。

### 4. `rc-cli/src/cmd/mod.rs`（参数定义，风格照抄 `LoopbackArgs` mod.rs:45-86）

```rust
/// `host` 子命令参数（被控端）。
#[derive(Args)]
pub struct HostArgs {
    /// 监听地址。--duration 是「接受连接后的会话时长」：accept 等待期不消耗。
    #[arg(long, default_value = "127.0.0.1:9901")]
    pub listen: std::net::SocketAddr,
    /// 后端模式：fake / auto / real（复用 parse_backend，mod.rs:100-109）。
    /// real 下 host = SCK + VideoToolbox；任一降级即报错 exit 1。
    #[arg(long, default_value = "fake", value_parser = parse_backend)]
    pub backend: BackendMode,
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// 目标码率（Hello 请求值；host 按编码器能力夹紧后经 Welcome 裁定）。
    #[arg(long, default_value_t = 8_000_000)]
    pub bitrate_bps: u32,
    /// 会话时长兜底（client 正常关闭发 Bye；到时未收到则自行优雅收尾）。
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub duration: Duration,
    /// 静止画面（铁律 2：静止零编码）。fake 后端下手动构造
    /// `FakeContent::Static` 栈（照 `rc-loopback/src/lib.rs:206-241` 的
    /// fake_stack 形态）；client 侧将观测到零媒体数据报。
    #[arg(long)]
    pub static_content: bool,
    /// 注入模式：record = 记录不真注入（默认，免权限）；real = CGEvent
    /// 真注入（需辅助功能权限，仅 macOS）。
    #[arg(long, default_value = "record")]
    pub inject_mode: InjectMode,
    /// 服务端证书 / 私钥 PEM；默认内嵌开发证书。
    #[arg(long)]
    pub cert: Option<PathBuf>,
    #[arg(long)]
    pub key: Option<PathBuf>,
    /// host 侧摘要 JSON 输出路径；省略或 `-` 表示 stdout（无 gate 段）。
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `client` 子命令参数（控制端）。
#[derive(Args)]
pub struct ClientArgs {
    /// 对端地址（必填）。
    #[arg(long)]
    pub connect: std::net::SocketAddr,
    /// 后端模式：fake / auto / real。real 下 client = VideoToolbox 解码 +
    /// Metal 呈现（会弹真窗口）。
    #[arg(long, default_value = "fake", value_parser = parse_backend)]
    pub backend: BackendMode,
    /// 请求参数（Hello 携带；实际以 Welcome 裁定值为准）。
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub duration: Duration,
    /// 输入模式：scripted = 脚本回放（默认，持续产出输入流量供 InputAck
    /// 采样）；none = 不发输入；real = CgCapture 本机捕获（需输入监控
    /// 权限，仅 macOS）。
    #[arg(long, default_value = "scripted")]
    pub input_mode: InputMode,
    /// 到点主动断开传输层，触发自动退避重连（重连演练，T17 机制）。
    #[arg(long, value_parser = parse_duration)]
    pub reconnect_at: Option<Duration>,
    /// 固定信任的服务端证书 PEM；默认内嵌开发证书公钥。
    #[arg(long)]
    pub server_cert: Option<PathBuf>,
    /// 证书 SAN 名（须与证书匹配）。
    #[arg(long, default_value = rc_session::dev_cert::DEV_SERVER_NAME)]
    pub server_name: String,
    /// 报告 JSON 输出路径；省略或 `-` 表示 stdout（含 gate 段）。
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `host --inject-mode` 取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectMode { Record, Real }

/// `client --input-mode` 取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode { None, Scripted, Real }
```

### 5. `rc-cli/src/cmd/host.rs` / `client.rs`

```rust
/// 执行 `host` 子命令：构造 Identity 与 HostSession（注入选定后端与
/// Injector）→ accept 一条连接 → stderr 打「等待连接」与后端标签 →
/// 驱动到 Bye 或 --duration 兜底 → stderr 摘要（in_flight 峰值、注入
/// 事件数）+ 可选报告落盘。正常收尾 SUCCESS；运行期错误经 CliResult
/// 上抛（main 打印后 exit 1）。**不做 Gate 判定。**
pub fn run(args: &HostArgs) -> CliResult<ExitCode>;

/// 执行 `client` 子命令：计时起点（Endpoint 创建之前）→ connect →
/// 驱动会话（scripted 输入经 pump_sender；--reconnect-at 到点触发 T17
/// 重连）→ duration 到时发 Bye 正常关闭 → 组装 SessionReport →
/// stderr 摘要 + 报告落盘。Gate 通过 SUCCESS，未通过 FAILURE
/// （loopback.rs:90-94 模式）。
pub fn run(args: &ClientArgs) -> CliResult<ExitCode>;
```

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| S1 | 分流约定照抄 loopback（`rc-cli/src/cmd/loopback.rs:1-8`）：进度/摘要只进 stderr；JSON 只进 `--report` 文件或 stdout，永不混流 | 验收脚本解析失败 |
| S2 | exit code：client `gate.passed` → 0 / 未通过 → 1；clap 参数错 → 2；运行期错误 → 1（main.rs 约定）。host 正常收尾 → 0（Gate 只在 client 判） | CI 无法判定 |
| S3 | 默认内嵌开发证书，用户零配置即可 localhost 直连；`--cert/--key/--server-cert/--server-name` 可覆盖。证书文档写明「仅开发用，不构成安全边界，真实体系属 M5」 | 误当安全边界 |
| S4 | 跨钟域换算必须走时钟偏移：e2e = Presented − (captured + offset)、network = Received − (encode_done + offset)；**不得**拿 trace 的 Captured 标记直接减 Presented（controller.rs:176-178 写入的是 host 域值）。用合成偏移单测钉死换算路径 | 报告数字是垃圾（设计师风险 1） |
| S5 | 队列深度（铁律 1）：client 重组表峰值进 `max_queue_depth_seen` 并参与 Gate（≤2）；host `in_flight` 峰值 ≤2 打 stderr 摘要 | 违反 Q3 |
| S6 | 时间注入（铁律 4）：报告组装 / 偏移换算 / Gate / convergence 扫描全部吃注入参数；只有 CLI 组合根（驱动循环、connect 计时）与 testkit 的 IO 边界允许读墙钟 | 测试 flaky |
| S7 | 双进程实测必须走**真 QUIC**：两进程各自完整 rc-net 栈（datagram + 双 reliable 流 + pacing），无任何进程内短路；CI 用假后端 + record 注入保证免权限 | 测量无效 |
| S8 | host 停机顺序红线由 T14 会话层保证（先停采集 → tick 排空 → 停编码，对齐 `rc-loopback/src/lib.rs:424-459` 收尾形态）；CLI 不自写停机、不绕过 | 授权帧凭空消失 |
| S9 | 报告 JSON 键名固定（上方 schema），手写 `to_json` 不引 serde；`escape` 对齐 rc-loopback | 键漂移破坏验收脚本 |
| S10 | 完成后回写 TASK.md §0/§12/§13（数字带场景与命令）与 tickets README 状态表（AGENTS.md §九） | 视为未完成 |

## 不要做什么

- ❌ 不做双机真机（LAN）测量——localhost 双进程即本卡实测范围，LAN 属 M2 出口后置项。
- ❌ 不做 UI（T21）、鉴权（T19）、剪贴板（T20）。
- ❌ 不改 `rc-net` / `rc-pipeline` 任何代码（本轮 rc-pipeline 唯一改动 `set_bitrate` 属 T15；本卡对两者只读）。
- ❌ 不在 rc-cli 里写会话逻辑——CLI 是薄壳，编排全在 rc-session（接线错误 = 会话层 API 设计有问题，回去改前置卡的卡，不在 CLI 打补丁）。
- ❌ 不做跨进程 `merge_traces`——e2e 走「包头时间 + 探测偏移」换算口径（设计师结论）。
- ❌ 不给 BwProbe 估速做 Gate 或精确带宽断言（v0 受 pacer 天花板约束，M3-3 替换）。
- ❌ 不为真后端 localhost 实测断言 fps 门槛（同机双进程 CPU 竞争，设计师风险 4）——真后端只冒烟。
- ❌ 不引 serde / 任何新重型依赖（clap 已有）。

## 测试清单

**rc-session 单测（离线确定性，`src/report.rs` / `src/testkit.rs` 内嵌）：**

| 测试名 | 验证 |
|---|---|
| `offset_conversion_is_applied_to_e2e_and_network` | S4：合成 trace（captured=host 域、presented=client 域、已知 offset）→ e2e/network 精确等于手算值；offset 置 0 时数字必须不同（证明换算真的发生了） |
| `missing_offset_fails_gate_loudly` | 有上屏帧但无偏移样本 → Gate 失败且 failures 含「时钟偏移」；不静默给 0 |
| `session_report_json_has_schema_keys` | S9：顶层键 config/latency_ms/fps/frames/breakdown_ms/convergence_ms/degradation_ms/max_queue_depth_seen/session/gate 齐全；frames 不含 dropped_* 键；breakdown_ms 只含 4 段 |
| `session_gate_fails_each_threshold` | 逐项注入超标值：首帧 >800ms / p50 >25 / p99 >40 / fps <0.9× / 队列深度 >2，各产生对应失败项 |
| `input_one_way_gate_uses_rtt_half_plus_margin` | 样本 p50 = rtt/2+4.9ms 通过、+5.1ms 失败；input_mode="none" 跳过判定 |
| `reconnect_gate_only_when_attempted` | attempts=0 不判 recovery；attempts>0 且 resumed=false 或 recovery>500 失败 |
| `convergence_scan_uses_full_one_second_window` | 合成上屏时刻表：满速 1000ms 收敛、31s 才达标报 null（复制 rc-loopback 同名单测语义，lib.rs:1204-1235） |
| `zero_frame_report_is_not_nan` | 空观测：全 0 / null、无 NaN/Infinity、Gate 失败于「无上屏帧」 |
| `batch_is_key_matches_input_semantics` | MouseMove/Sync 非关键；按下/抬起/滚轮/按键关键 |
| `recording_injector_counts_key_batches` | 3 批（纯移动/含点击/含按键）→ injected_count=事件总数、key_batches=2 |
| `scripted_capture_replays_in_order` | from_timeline 首拍返回 t=0 事件、未到期返回空、播完返回空；时间线降序被拒 |
| `mock_transport_pair_roundtrips` | `testkit::pair()`（T14 交付符号）双向投递、urgent 与 reliable 分道（urgent 不进 reliable 队列） |

**rc-session 集成测试（`tests/localhost.rs`，进程内双端、真 QUIC over 127.0.0.1、CI 可跑、免权限）：**

| 测试名 | 验证 |
|---|---|
| `localhost_fake_session_meets_gate` | host 会话线程 + client 会话线程（各持 QuicEndpoint，照 `rc-net/tests/common/mod.rs:37-60` 的 pair 形态），假后端 + ScriptedCapture + RecordingInjector，64x64@60fps 跑 ~3s：SessionReport Gate 通过 |
| `localhost_input_flows_to_injector` | client 发 scripted 输入 → host RecordingInjector 收到事件（归一化坐标原样），InputAck 样本数 >0 |

**rc-cli 集成测试（`tests/host_client.rs`，双子进程、CI 可跑）：**

| 测试名 | 验证 |
|---|---|
| `host_and_client_pass_gate_on_localhost` | 先绑 `127.0.0.1:0` 保留端口再放行（防并行冲突）；spawn host（`--listen <port> --backend fake --resolution 64x64 --duration 8s --inject-mode record`）与 client（`--connect <port> --backend fake --duration 4s --input-mode scripted --report <tmp>`）：client exit 0、JSON 含 session 段与 `"passed":true`、host 先于自身兜底时长正常收尾 |
| `client_exits_one_when_gate_fails` | host 加 `--static-content`（静止零编码，铁律 2）→ client 全程无帧上屏：`"connect_to_first_frame_ms":null`、Gate 失败于「无上屏帧」→ exit 1 且报告仍落盘、`frames.displayed == 0`。注意不能用短 duration 造失败：500ms 内假管线照样满帧、各项门槛照过 |
| `host_report_has_no_gate_key` | 一对短会话（host `--report <tmp> --duration 5s` + client `--duration 2s`）：host 报告含 config/session 键、**不含** `"gate"`；stderr 有摘要行 |
| `reconnect_scenario_recovers_within_budget` | client `--reconnect-at 1s --duration 6s`：`session.reconnect.resumed == true`、`recovery_ms ≤ 500`、Gate 仍通过、exit 0 |
| `client_rejects_invalid_input_mode` | `--input-mode bogus` → clap exit 2 |

**真机 / 权限测试（`#[ignore]`，经 `./scripts/macos-dev-run.sh` 跑）：**

| 测试名 | 验证 |
|---|---|
| `#[ignore] localhost_real_backend_smoke` | 双进程 `--backend real`：host SCK+VT（需屏幕录制 + 幕布供帧）、client VT 解码 + Metal 窗口。只断言：会话完成、报告落盘、connect_to_first_frame_ms ≤800、无 panic；**不断言 fps/e2e 门槛**（CPU 竞争噪声 + 静屏零编码是正确行为） |
| `#[ignore] real_inject_via_input_bridge` | host `--inject-mode real`（InputBridge，需辅助功能）：少量事件注入成功计数 >0（复用幕布工具授权） |

## 验收命令

```bash
# 1. 内循环（铁律 3：check + 单 crate 测试）
cargo check -p rc-session --all-targets
cargo check -p rc-cli --all-targets
cargo test -p rc-session
cargo test -p rc-cli

# 2. 全仓回归 + 质量门（T15 动过 rc-pipeline，必须全量）
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# 3. localhost 双进程实测（release；终端 1）
./scripts/macos-dev-run.sh cli --release -- host \
    --listen 127.0.0.1:9901 --backend fake --resolution 1920x1080 \
    --fps 60 --duration 35s --inject-mode record --report /tmp/m2-host.json

#    （终端 2，host 起来后）
./scripts/macos-dev-run.sh cli --release -- client \
    --connect 127.0.0.1:9901 --backend fake --duration 30s \
    --input-mode scripted --report /tmp/m2-client.json

# 4. 重连演练（另起一对进程）
./scripts/macos-dev-run.sh cli --release -- client \
    --connect 127.0.0.1:9902 --backend fake --duration 30s \
    --input-mode scripted --reconnect-at 15s --report /tmp/m2-reconnect.json

# 5. 真后端冒烟（仅 macOS，需权限与幕布；只看不判门槛）
./scripts/macos-dev-run.sh cli --release -- host \
    --listen 127.0.0.1:9903 --backend real --duration 35s --inject-mode record \
    --report /tmp/m2-host-real.json
./scripts/macos-dev-run.sh cli --release -- client \
    --connect 127.0.0.1:9903 --backend real --duration 30s \
    --input-mode scripted --report /tmp/m2-client-real.json
```

说明：命令 3/4 的 host `--duration`（35s）大于 client（30s）——host 的
时长是兜底，正常路径是 client 到时发 Bye 后 host 提前优雅收尾。fake 后端
不涉及任何 macOS 权限，但统一走幕布工具保持口径一致（脚本以完成摘要行
判定子进程结束，`scripts/macos-dev-run.sh:115-120`；`open -n` 允许多实例，
两个终端各自调用互不干扰，但不要在同一终端用 `&` 拼后台）。

## 验收标准

- [ ] `cargo test --workspace` 全绿（含 rc-session、rc-cli 新增测试）
- [ ] 双进程 fake localhost 30s：client `gate.passed == true` 且 exit 0；
      `connect_to_first_frame_ms ≤ 800`
- [ ] e2e `p50 ≤ 25ms` / `p99 ≤ 40ms`（M2 出口门槛；本机 fake 基线预期
      ~15ms 量级）；换算正确性由 `offset_conversion_is_applied_*` 单测钉死
- [ ] `fps.displayed ≥ 目标 × 0.9`（60fps → ≥54）
- [ ] `input_one_way_p50_ms ≤ rtt_us/2/1000 + 5`
- [ ] `max_queue_depth_seen ≤ 2`（client 重组表）；host stderr 的
      in_flight 峰值 ≤ 2（铁律 1）
- [ ] 重连演练：`resumed == true`、`recovery_ms ≤ 500`、Gate 仍通过
- [ ] 真后端冒烟经幕布工具跑通（不断言 fps；静屏低帧属铁律 2 正确行为）
- [ ] 无 `unsafe`、无 `Box<dyn Error>` 跨 crate、无新增重型依赖
- [ ] TASK.md §0/§12/§13 已回写（实测数字带场景与命令），tickets README
      状态表已更新

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-cli/src/cmd/loopback.rs`](../../../crates/rc-cli/src/cmd/loopback.rs) | 参数→配置→运行→报告落盘→exit code 的全链模板（分流约定 1-8、exit 90-94、write_report 99-108） |
| [`rc-cli/src/cmd/mod.rs`](../../../crates/rc-cli/src/cmd/mod.rs) | 子命令注册、Args 定义、`parse_backend/parse_resolution/parse_duration` 复用（100-147） |
| [`rc-loopback/src/lib.rs`](../../../crates/rc-loopback/src/lib.rs) | `run_real` 驱动循环形态（336-423）、时钟锚定与 Arc 闭包（246-263、342-360）、收尾红线（424-459）、`scan_presented` 语义（717-764） |
| [`rc-loopback/src/report.rs`](../../../crates/rc-loopback/src/report.rs) | 报告 schema / Gate / 手写 `to_json` 与 `escape`（339-442） |
| [`rc-net/tests/common/mod.rs`](../../../crates/rc-net/tests/common/mod.rs) | 内嵌证书模式（21-34）与 endpoint pair 连接形态（37-60） |
| [`rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | `pump_sender`/`pump_receiver` 双拍（83/111）、MockTransport 测试替身先例（194-255） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **忘做偏移换算数字就是垃圾**：`controller.push_datagram` 把 host 域 capture_ts 直接写进 trace 的 Captured 标记（controller.rs:176-178），直接 `Presented − Captured` 会把两钟原点差整体吃进 e2e | 报告侧持包头值自行换算；`offset_conversion_is_applied_*` 单测钉死（含「offset 置 0 数字必须变」的反向证明） |
| 2 | **双进程测试放哪**：`CARGO_BIN_EXE_rc-cli` 只对 rc-cli 自己的集成测试注入（`rc-cli/src/cmd/mod.rs:150-152` 注释、tests/cli.rs:3-4），rc-session 的 tests 拿不到 rc-cli 二进制 | 设计师原文把双进程测试写在 rc-session tests——落地拆两处：双子进程测试放 `rc-cli/tests/host_client.rs`，`rc-session/tests/localhost.rs` 做进程内双端（真 QUIC）。这是对设计师计划的唯一机制性修正，意图（CI 免权限复跑指标）不变 |
| 3 | **800ms 计时起点**：真机 LAN 上主要耗时在证书加载与 Endpoint 建立 | 起点定在 Endpoint 创建之前（`SessionObservations::connect_to_first_frame_us` 文档写死），避免后续真机测口径漂移 |
| 4 | **同机双进程 CPU 竞争**：真后端下 SCK+VT 编码与 VT 解码+Metal 呈现争同一台机器；真采集静屏时铁律 2 压低 fps 属正确行为 | fps/e2e 门槛只在 fake 后端上判（FakeBackend 动态供给稳定 60fps）；real 只冒烟不断言 |
| 5 | **BwProbe v0 测的是 pacer 天花板**（`DEFAULT_INITIAL_PACING_BPS`=50Mbps，quic.rs:79）而非真实路径带宽，datagram 丢包还使估速偏低 | 只用于初始码率下限保护（T15）；不进 Gate、不做精确断言；文档写死边界，M3-3 拥塞控制接管后废弃 |
| 6 | **InputAck 批口径漂移**：RecordingInjector 与真实 InputBridge 对「关键批」判定不一致会造成输入延迟采样偏差 | 关键批判定抽成 `testkit::batch_is_key` 共享函数，两侧同源（设计师风险 6） |
| 7 | **quinn accept/connect 阻塞**（quic.rs:253/303） | 会话驱动循环 poll 短超时（T14 已覆盖「网络静默+采集活跃」两象限）；CLI 的 host 在 accept 等待期打 stderr「等待连接」提示，`--duration` 从握手完成起算 |
| 8 | **u32 时间戳 71.6 分钟饱和** | 会话由 T17 的 70min `Bye(Refresh)` 守卫；报告换算一律用饱和减法（`saturating_sub`），30s 级实测无回绕 |
| 9 | **CI 端口冲突**：并行测试共用固定端口会撞 | 测试内先绑 `127.0.0.1:0` 保留端口、读端口号、drop 后传给 host 子进程（小竞态可接受，撞了重跑即可）；手动实测用 9901-9903 错开 |
| 10 | **rc-session 依赖扇出大**（rc-net/rc-pipeline/rc-input/rc-trace） | 遵守 M0 纪律：内循环只 `cargo check -p rc-session` / `-p rc-cli`，全仓验证留给验收命令（设计师风险 9） |
| 11 | **幕布工具的结束判定**：`macos-dev-run.sh cli` 用 `open -n -W` 封装并靠摘要行判定结束 | 两终端各自调用（`-n` 允许多实例）；不要在同一终端 `&` 拼后台，否则脚本等不到摘要行 |
| 12 | **stderr `\r` 进度在非 TTY 交错** | 照抄 loopback.rs:42-54 的 is_terminal 分支：TTY 用 `\r` 覆盖，CI/管道打整行 |

## 与后续卡的接口

- **T21 控制端 UI** 会复用本卡的会话驱动与报告口径做数据源；session 段
  键名是契约，**只增不改名**。
- **T19 鉴权 / T20 剪贴板**扩展 proto 消息，不动本卡 CLI 参数面（届时各自加旗标）。
- **M3-3 拥塞控制**替换 BwProbe 后，session 段的 `loss_rate` 等键语义不变，
  带宽估速若上报告另立新键。
- **M2 出口实测（2h 长跑、LAN 双机）**复用本卡两个子命令与报告 schema，
  只换参数与网络位置。
