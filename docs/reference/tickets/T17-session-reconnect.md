# T17 断线重连与会话恢复（M2-9）

> 断线不断会话：host 在保持窗口内复用管线（TTL，不拆）、client 指数退避自动重连 + 重连后强制 IDR、PathChanged 透传不中断，外加 70 分钟时钟饱和守卫（Bye(Refresh) → client 全新重建、双端时钟归零）。

## 背景（为什么做这个）

双机直连的现实是传输层一定会断：WiFi 漫游、对端进程重启、笔记本合盖。若每次断线都等价于会话结束（拆管线 → 重新握手 → 再等首帧 IDR），用户看到的就是整屏冻结后黑屏重来。传输层已有两块地基：rc-net 的连接迁移（N9：`rebind` 换 socket 后连接 ID 不变、媒体不断流，`crates/rc-net/src/quic.rs:334`）和 `Disconnected` / `PathChanged` 事件（`crates/rc-net/src/lib.rs:130-136`）；会话层 T14 已交付 `HostSession` / `ClientSession` 状态机（Init→Connected→Streaming→Closed）与 Hello/Welcome 握手。**本卡把「传输层断开」与「会话结束」解耦**：断开后的一个 TTL 窗口内，管线保留、`FrameSeq` 延续，重连握手只需一条 `ReconnectRequest`。

另一条硬约束来自 rc-types：`Timestamp` 是 u32 微秒，**约 71.6 分钟饱和到 `u32::MAX`**（`crates/rc-types/src/ids.rs:37-44`；契约 `docs/reference/impl/20-crate-contracts.md:93` 明写「会话层必须在此之前重建会话」）。本卡交付守卫：host 会话钟到 70 分钟（`REFRESH_GUARD_US`，距饱和留 ≈1.58 分钟）主动发 `Bye(reason=Refresh)` 并按停机红线拆管线，client **零退避**立即以全新 `session_id` 重连，双端时钟重新归零。刷新瞬间画面短暂中断是已知取舍（无缝双纪元切换不做，消息位已留 `host_flags` bit1）。

依赖 T16：重连后的强制 IDR 走 T16 的 `IdrRequest`/`IdrAck` 通道；T16 的「在途请求未确认前不重发」去抖状态**必须在重连成功时清零**（旧连接的 IdrAck 永远不会来，见已知坑 6）。

## 平台要求

**全平台。** 不需要真机、不需要任何 macOS 权限。集成测试只占 `127.0.0.1` 临时端口（照 `crates/rc-net/tests/` 模式），CI 可跑，不标 `#[ignore]`。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/reconnect.rs` | 新建 | 纯逻辑：`Backoff` / `ResumeWindow` / `RefreshGuard` / `ReconnectConfig` / `ReconnectStats` + 常量。**无 IO、无内部时钟** |
| `crates/rc-session/src/host_session.rs` | 修改 | 在 T14 的 `HostSession` 上增补 `Detached` 状态、保持窗口、`ReconnectRequest` 裁定、饱和守卫、PathChanged 计数 |
| `crates/rc-session/src/client_session.rs` | 修改 | 在 T14 的 `ClientSession` 上增补 `Reconnecting` 状态、指数退避自动重连、重连后强制 IDR、`Bye(Refresh)` 全新重建、管线重建工厂 |
| `crates/rc-session/src/lib.rs` | 修改 | `pub mod reconnect;` + 模块地图补一行 |
| `crates/rc-session/tests/reconnect.rs` | 新建 | 真 QUIC 回环集成测试（进程内两端，3 条） |
| `crates/rc-session/tests/certs/` | 沿用（T14 已建） | T14 交付 quic_loopback 集成测试时已建该目录与自签 PEM；本卡**沿用不重建**，仅当材料缺失时照 `crates/rc-net/tests/certs/` 补（头注释写明不构成安全边界） |

`proto.rs` **不改**：本卡消费的消息（0x02 Welcome、0x03 IdrRequest、0x04 IdrAck、0x0B ReconnectRequest、0x0C Bye）已由 T13 按设计师线格式定型。

## API 契约

以下是本卡的**权威定义**（rc-session 尚无契约文档章节，以本卡为准）。`HostSession` / `ClientSession` 的基础构造与驱动接口以 **T14 卡交付为准**：共享 `SessionState { Init, Connected, Streaming, Closed }`、`SessionClock`（时间注入）、host `tick(poll_budget)` / client `tick(poll_budget, now)`、`run(stop)`。本卡只做**增量**：在 T14 的 `SessionState` 上增补 `Detached` / `Reconnecting` 两个变体、接管 T14 S8 明确留给本卡的分发（T14 基线对 `ReconnectRequest` / `Bye(Refresh)` 到达是「忽略、状态不变」）。若 T14 实际代码命名与本文有出入，**语义以本卡为准、命名向 T14 对齐**。

### 线格式消费（T13 已定型，此处只是引用；符号名以 `proto.rs` 交付为准）

| 消息 | 本卡用法 |
|---|---|
| `0x0B ReconnectRequest` | `{session_id u64, resume u8}`：`resume=1` 请求续传；`resume=0` / session_id 不匹配 / 窗口过期一律走「未命中」路径 |
| `0x02 Welcome` | `host_flags` **bit0 = resume 被接受**、**bit1 = 时钟纪元已重置**（仅饱和刷新后的新会话置 1，普通全新连接为 0） |
| `0x03 IdrRequest` | `reason=2`（`proto::IdrReason::ReconnectRecovery`，重连恢复——T13 契约全名）——client 在 resume 成功后立即发送 |
| `0x0C Bye` | `reason=1`（`proto::ByeReason::Refresh`，饱和刷新）——host 守卫触发时主动发送 |

### `reconnect.rs`（全部新增）

```rust
/// host 侧断线保持窗口默认 TTL。
///
/// 覆盖 WiFi 漫游（1–3s）与对端进程快速重启；QUIC 空闲超时是 30s
/// （quic.rs:95），静默断线最坏 30s 后才上报 Disconnected，窗口从
/// Disconnected 起算，因此静默死亡的总宽限 ≈ 30s + TTL。
pub const DEFAULT_RESUME_TTL: Duration = Duration::from_secs(10);

/// client 侧指数退避默认参数：500ms 起步、×2、封顶 8s、共 5 次。
/// 尝试时刻 = 断开后 0.5s / 1.5s / 3.5s / 7.5s / 15.5s。
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(8);
pub const DEFAULT_BACKOFF_MAX_ATTEMPTS: u32 = 5;

/// 时钟饱和守卫线（微秒）：70 分钟 = 4_200_000_000。
/// u32::MAX ≈ 71.58 分钟，余量 ≈ 94.97s，足够走完停机红线排空。
pub const REFRESH_GUARD_US: u64 = 4_200_000_000;

/// 断线重连配置（两侧各持一份，字段同源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// host：断线后管线保持窗口 TTL
    pub resume_ttl: Duration,
    /// client：退避初值 / 封顶 / 最大尝试次数
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub backoff_max_attempts: u32,
    /// 时钟饱和守卫线（微秒）。生产恒 REFRESH_GUARD_US；
    /// 集成测试注入小值走完整刷新路径。
    pub refresh_guard_us: u64,
}
impl Default for ReconnectConfig; // 上述四个默认值 + REFRESH_GUARD_US

/// 断线/重连统计（报告与测试的观测点；T18 报告 reconnect 段的数据源）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectStats {
    /// 传输层断开次数
    pub disconnects: u32,
    /// resume 被接受（管线复用）次数
    pub resumes: u32,
    /// 全新重建（resume 被拒 / 饱和刷新）次数
    pub rebuilds: u32,
    /// 重连尝试次数（含失败）
    pub reconnect_attempts: u32,
    /// 路径迁移次数（PathChanged，不中断）
    pub path_changes: u32,
    /// 饱和刷新次数（host 发出 / client 收到 Bye(Refresh)）
    pub refreshes: u32,
}

/// host 侧会话保持窗口：从 Disconnected 起算的「管线必须拆除」倒计时。
pub struct ResumeWindow { /* 私有：detached_at_us、ttl_us */ }
impl ResumeWindow {
    pub fn new(ttl: Duration) -> Self;
    /// 记录断线时刻。窗口未开时登记；已开时幂等（**不续期**——
    /// 重复的 Disconnected 事件不得刷新 TTL）。
    pub fn detach(&mut self, now_us: u64);
    /// 窗口是否敞开（从未断线恒 true）。
    pub fn is_open(&self, now_us: u64) -> bool;
    /// 剩余毫秒（诊断 / 报告；未断线返回 TTL 满值）。
    pub fn remaining_ms(&self, now_us: u64) -> u64;
}

/// client 侧指数退避：纯函数 + 计数，**不持任何内部时钟**（铁律 4）。
pub struct Backoff { /* 私有 */ }
impl Backoff {
    /// 校验：initial > 0、max >= initial、max_attempts >= 1，
    /// 违反返回 Error::InvalidConfig。
    pub fn new(initial: Duration, max: Duration, max_attempts: u32) -> Result<Self>;
    /// 第 attempt 次（从 1 计）尝试**前**应等待的时长：
    /// initial × 2^(attempt−1)，封顶 max。500ms 起步 → 500/1000/2000/4000/8000ms。
    pub fn delay_before(&self, attempt: u32) -> Duration;
    /// 记录一次失败。Some(delay) = 按该延迟再试；None = 次数用尽，放弃。
    pub fn on_failure(&mut self) -> Option<Duration>;
    /// 重连成功后清零（下次断线从 initial 重新退避）。
    pub fn reset(&mut self);
    /// 已用尝试次数。
    pub fn attempts_used(&self) -> u32;
}

/// host 侧时钟饱和守卫。
pub struct RefreshGuard { /* 私有：threshold_us */ }
impl RefreshGuard {
    /// 生产传 REFRESH_GUARD_US；测试注入小值。
    pub fn new(guard_us: u64) -> Self;
    /// 到线即 true → 发 Bye(reason=Refresh) 并按红线拆管线。
    pub fn due(&self, now_us: u64) -> bool;
}
```

时间参数统一 `u64` 微秒（会话时钟 u32 拓宽而来）：**单纪元内时钟永不回绕**——守卫在回绕线（71.6min）之前必然触发并重建会话，这正是守卫存在的意义。

### `HostSession` 增补（T14 基线之上）

状态机：T14 的共享枚举增补两个变体（host 只进 `Detached`，client 只进 `Reconnecting`）：

```rust
/// 会话状态（T14 定义四态；两个新变体是 T17 增补）。
pub enum SessionState {
    Init,
    Connected,
    Streaming,
    /// host：传输层已断，管线保留、保持窗口计时中，短超时 accept 轮询（T17）
    Detached,
    /// client：传输层已断，管线保留，按指数退避自动重连（T17）
    Reconnecting,
    Closed,
}

/// Detached 期一拍驱动的迁移结果（测试断言点）。
pub enum DetachOutcome {
    /// 窗口内继续等待（本拍 accept 无连接）
    Waiting,
    /// 窗口过期，旧管线已按停机红线拆除（stop_capture → tick 排空 → shutdown）
    TornDown,
    /// 收到 ReconnectRequest 且命中：管线复用，回到 Streaming
    Resumed,
    /// 收到全新 Hello：旧管线拆除、新会话建立（FrameSeq / 时钟归零）
    Replaced,
}

impl HostSession {
    /// T17：断线重连配置（驱动开始前设置；默认 `ReconnectConfig::default`）。
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig);
    pub fn reconnect_config(&self) -> &ReconnectConfig;
    pub fn reconnect_stats(&self) -> ReconnectStats;
    /// T17：增补变体后仍走 T14 的 `state()`。
    pub fn state(&self) -> SessionState;
    /// 管线代数：每建一次管线 +1。窗口内 resume 不变；过期拆除后的下一个
    /// 全新会话 +1。这是「管线不拆」的可观测判据（报告 / 测试用）。
    pub fn pipeline_generation(&self) -> u32;
    /// T17：管线重建工厂（饱和刷新 / 全新 Hello 时调用）。不注入则用
    /// `HostPipeline::new(cfg.clone())`（auto 后端）；testkit 注入脚本化工厂。
    pub fn set_pipeline_factory(&mut self, f: Box<dyn FnMut() -> Result<HostPipeline> + Send>);

    /// T17：传输层断开处置。进入 Detached、`ResumeWindow::detach(now_us)`、
    /// **不拆管线**。此后驱动循环只跑「短超时 accept + 窗口判定 + 守卫」，
    /// 媒体 tick 暂停——采集线程由容量 1 的交接通道天然背压阻塞
    /// （host.rs:200-224），不是死锁，是省 CPU（铁律 2 同精神）。
    pub fn on_disconnected(&mut self, now_us: u64);

    /// T17：Detached 期驱动拍（短超时，防 accept 饿死，见已知坑 1）。
    /// 窗口过期 → 按红线拆管线（host.rs:486-512 的顺序，500ms 排空上限）
    /// 回到只 accept 状态；新连接到达 → 读首条控制消息分发：
    /// Hello → Replaced；ReconnectRequest → handle_reconnect。
    pub fn tick_detached(&mut self, now_us: u64) -> Result<DetachOutcome>;

    /// T17：重连裁定。命中（session_id 匹配 && resume=1 && 窗口开）→
    /// 换绑新 transport、复用管线（**FrameSeq 延续**）、`host.request_idr()`
    /// （host.rs:462，pending_idr 粘住直到真编出 IDR）、
    /// Welcome{host_flags bit0=1}；未命中 → 旧管线按红线拆除、
    /// Welcome{bit0=0}，**连接保持**等待对端补发全新 Hello。
    pub fn handle_reconnect(&mut self, req: &ReconnectRequest, now_us: u64) -> Result<Welcome>;

    /// T17：时钟饱和守卫（正常 Streaming 驱动循环每拍调用）。
    /// `RefreshGuard::due` → 发 Bye{reason=Refresh}、按红线拆管线、
    /// 回到 accept（等待 client 全新 Hello）。返回 true 表示已触发。
    /// 新会话建立时 Welcome 置 host_flags bit1=1。
    pub fn check_refresh(&mut self, now_us: u64) -> Result<bool>;
}
```

### `ClientSession` 增补（T14 基线之上）

```rust
/// Reconnecting 期一拍驱动的迁移结果。
pub enum ReconnectOutcome {
    /// 退避等待中（next_attempt_in_ms 按注入时钟折算，诊断 / 测试用）
    Waiting { next_attempt_in_ms: u64 },
    /// 本拍发起了一次 connect（结果在后续拍 settle）
    Connecting,
    /// resume 成功：管线复用，已发 IdrRequest{reason=重连恢复}，回 Streaming
    Resumed,
    /// resume 被拒 / Bye(Refresh)：全新 Hello 已发，管线与双端时钟已重建
    Rebuilt,
    /// 退避次数用尽，会话关闭（rc-cli 按退出码处理，属 T18）
    GaveUp,
}

impl ClientSession {
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig);
    pub fn reconnect_config(&self) -> &ReconnectConfig;
    pub fn reconnect_stats(&self) -> ReconnectStats;
    /// T17：增补变体后仍走 T14 的 `state()`。
    pub fn state(&self) -> SessionState;
    pub fn pipeline_generation(&self) -> u32;
    /// T17：管线重建工厂。不注入则 `ControllerPipeline::new(cfg.clone())`；
    /// testkit 注入脚本化工厂。
    pub fn set_pipeline_factory(
        &mut self,
        f: Box<dyn FnMut() -> Result<ControllerPipeline> + Send>,
    );

    /// T17：断开处置。保留 ControllerPipeline、session_id 与 QuicEndpoint
    /// （重连复用**同一 endpoint**：T14 建连时留存的 peer_addr / 证书 /
    /// server_name 直接再用；endpoint 被 drop 则一切重来）。进入
    /// Reconnecting，看门狗（T16）与探测（T15）暂停，输入泵暂停。
    pub fn on_disconnected(&mut self, now_us: u64);

    /// T17：重连驱动拍（时间注入；到点判定全凭 now_us，不读墙钟）。
    /// 到点 → endpoint.connect → 发 ReconnectRequest{session_id=旧, resume=1}：
    /// · Welcome bit0=1 → Resumed：**立即发 IdrRequest{reason=2}**；
    ///   时钟 / RTT / 时钟偏移估计延续（纪元未变，仍有效）；
    ///   **清空 T16 在途 IdrRequest 去抖状态**、重置看门狗基准；
    /// · Welcome bit0=0 → 同连接降级：换新 session_id 补发全新 Hello、
    ///   重建管线（工厂）→ Rebuilt（首帧天然 IDR：host 侧 no_idr_yet，
    ///   host.rs:528，无需额外请求）；
    /// · connect / 握手失败 → Backoff::on_failure 退避再试；
    ///   次数用尽 → GaveUp。
    pub fn tick_reconnect(&mut self, now_us: u64) -> Result<ReconnectOutcome>;

    /// T17：收到 Bye{reason=Refresh}（Streaming 态的正常事件分发调用）。
    /// **零退避**立即全新重连（host 在线等着，退避只会白等）：新
    /// session_id、发全新 Hello、经工厂重建管线；后续 Welcome 必须带
    /// bit1=1（校验之），据此把 T15 时钟偏移估计清零重建、本地会话钟归零。
    pub fn on_bye_refresh(&mut self, now_us: u64) -> Result<()>;
}
```

### 两个场景的时序（规范行为）

```text
场景 A：传输层意外断开 → 恢复
  client                              host（Detached，管线保留，TTL 内）
    │◀── 传输层 Disconnected ──────────│
    ├─ Reconnecting（0.5s/1s/2s/4s/8s）├─ tick_detached：短超时 accept + 窗口判定
    ├─ ReconnectRequest{sid, resume=1}▶│
    │◀────────── Welcome{bit0=1} ──────┤ 复用管线（FrameSeq 延续）+ request_idr()
    ├─ IdrRequest{reason=2} ──────────▶│
    │◀═══ IDR 媒体流（datagram）═══════│
    │◀────────── IdrAck{frame_seq} ────┤（T16 关联确认）

场景 B：时钟饱和刷新（host 会话钟 ≥ 70min）
  client                              host
    │◀──── Bye{reason=Refresh} ────────┤ check_refresh 触发；随后按红线拆管线
    ├─ 全新 Hello（新 session_id）────▶│（零退避，立即）
    │◀──── Welcome{bit0=0, bit1=1} ───┤ 新管线：FrameSeq 与会话钟归零；首帧天然 IDR
    │  client 重建管线、偏移估计清零    │
```

PathChanged（N9 透传）：两侧收到 `NetEvent::PathChanged` **不迁移状态、不断流**，仅 `ReconnectStats.path_changes += 1`（事件在 `poll` 内按地址变化产生，`quic.rs:858-864`，会话必须持续 poll 才看得到）。

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| R1 | Disconnected 后 host 不拆管线：进入 Detached，保持窗口 TTL 内等待 `ReconnectRequest`；窗口从 Disconnected 起算，重复 Disconnected 不续期 | 短断线变整会话重来，重连恢复退化为冷启动 |
| R2 | 重连裁定：命中 → `Welcome{bit0=1}` + 复用管线（FrameSeq 延续）+ `host.request_idr()`；未命中（resume=0 / session_id 不匹配 / 过期）→ `bit0=0`，连接保持等全新 Hello，旧管线按红线拆除 | 状态错乱：续传到错误会话，或死连接上等不到 Hello |
| R3 | client 自动重连：指数退避 500ms 起步 ×2 封顶 8s、默认 5 次；**全部判定吃参数 `now_us`，禁止内部 Instant/睡眠**（铁律 4）；重连复用同一 QuicEndpoint | 要么风暴重连打爆对端，要么真实 sleep 让测试慢且不可复现 |
| R4 | 重连恢复后强制 IDR：client 发 `IdrRequest{reason=2}`，host 恢复时也主动 `request_idr()`（双保险，`pending_idr` 粘住语义幂等）；恢复后首个续流数据报必须是 IDR 帧；重连成功时**清空 T16 在途请求去抖状态**、重置看门狗基准 | 恢复后花屏（参考链断裂）或恢复被去抖堵死 |
| R5 | PathChanged 不算断线：状态机不迁移、媒体不断流、计数进 `ReconnectStats`（N9 复用，`quic.rs:334` rebind 已保证连接存活） | 换网（WiFi→有线）被误判断线，触发无谓重建 |
| R6 | 时钟饱和守卫：host 会话钟 ≥ `REFRESH_GUARD_US`（70min）→ 发 `Bye{reason=Refresh}` + 红线拆管线 + 回 accept；client 收到后零退避全新重连，双端时钟归零，新 `Welcome` 必带 bit1=1 且 client 校验后清零偏移估计 | u32 时钟饱和 → 所有时间戳冻结在 `u32::MAX`，e2e 测量与调度全废 |
| R7 | 任何路径拆管线都走停机红线：`stop_capture` → tick 排空（`has_pending_encodes()==false`，500ms 上限）→ `shutdown`（`host.rs:486-512`，反序会让授权帧凭空消失） | trace 账目破洞、编码线程里的帧无结局 |
| R8 | 非 Streaming 态（Detached / Reconnecting）看门狗不评估、探测与输入泵暂停——不得误发 IdrRequest 或把无帧期误判停滞 | 重连期间看门狗风暴 / 假停滞告警 |
| R9 | 队列深度纪律不破（铁律 1）：本卡不新增任何队列；交接通道 1、编码在途 2、client 重组表 2（`controller.rs:56`）全部维持 | 延迟预算被隐性队列吃掉 |
| R10 | 本卡**零下层改动**：不碰 rc-net / rc-pipeline / rc-types / rc-proto（`set_bitrate` 是 T15 的事，本卡不做） | 污染已过 Gate 的稳定层 |

## 不要做什么

- ❌ 不做跨网段迁移策略（M3-8）：`rebind` / N9 已由 rc-net 提供，本卡只透传计数。
- ❌ 不做 2h 长跑认证（属 M2 出口实测，后置）。本卡只交付守卫机制 + 缩短阈值的功能测试，**不要在测试里真等 70 分钟**。
- ❌ 不做无缝双纪元并行切换（刷新瞬间画面中断是接受的取舍；`host_flags` bit1 只是告知位，位已留）。
- ❌ 不改 `proto.rs` 线格式；不占用 0x0D/0x0E/0x0F（M2-5 / M2-8 预留）。
- ❌ 不做鉴权 / 剪贴板 / UI / CLI 子命令（T19 / T20 / T21 / T18）。
- ❌ 不做多会话并发：host 单 accept 槽，一个 Detached 旧会话 + 一个新连接裁定即可。
- ❌ 不动 rc-cli、不写 TASK.md 实测数字（真机 localhost 双进程归 T18）。

## 测试清单

**纯逻辑测试（`reconnect.rs` 内 `#[cfg(test)]`，无 IO、时间全注入）：**

| 测试名 | 断言要点 |
|---|---|
| `backoff_schedule_doubles_and_caps` | `delay_before(1..=5)` = 500ms/1s/2s/4s/8s；**第 5 次 `on_failure` 即返回 `None`**（max_attempts=5：第 1–4 次 `on_failure` 各返回第 2–5 次尝试的等待，第 5 次 `on_failure` 次数用尽直接 `None`——与 `client_gives_up_after_max_attempts` 的 `reconnect_attempts==5` 同一口径，不是「第 6 次返回 None」）；`reset` 后从头开始 |
| `backoff_rejects_degenerate_config` | initial=0、max<initial、max_attempts=0 均 `Error::InvalidConfig` |
| `resume_window_expires_at_ttl` | `detach(t0)` 后 `is_open(t0+ttl−1ms)`=true、`is_open(t0+ttl)`=false；未 detach 恒 open；`remaining_ms` 单调递减到 0 |
| `resume_window_detach_does_not_extend_ttl` | 窗口内二次 `detach` 不续期（仍按 t0 过期） |
| `refresh_guard_due_at_threshold` | `due(th−1)`=false、`due(th)`=true；阈值可注入 |
| `refresh_guard_margin_below_u32_max` | `u32::MAX as u64 − REFRESH_GUARD_US ≥ 60_000_000`（≥60s 排空余量）且 `REFRESH_GUARD_US` 可被 u32 表示 |

**会话层离线测试（testkit MockTransport 对接件 + 注入 now_us，T14 的 testkit 已有；MockTransport 范式照 `channel.rs:195-223`）：**

| 测试名 | 断言要点 |
|---|---|
| `host_detached_keeps_pipeline_until_ttl` | Disconnected → `Detached`；`pipeline_generation` 不变；注入时钟推过 TTL 后一拍返回 `TornDown` |
| `host_resume_within_window_reuses_pipeline_with_idr` | 窗口内 `ReconnectRequest{sid, resume=1}` → `Welcome` bit0=1；generation 不变；恢复后首个产出的媒体数据报 `flags` 为 IDR 且 `frame_seq` 延续（不归零） |
| `host_rejects_resume_after_expiry_via_fresh_hello` | 过期后 resume=1 → bit0=0；同连接补发全新 Hello → 新会话建立、generation+1、FrameSeq 归零 |
| `host_refresh_sends_bye_and_sets_epoch_bit` | 注入时钟到守卫线 → reliable 流出现 `Bye{reason=Refresh}` 字节；随后全新 Hello → `Welcome{bit0=0, bit1=1}` |
| `host_path_changed_keeps_streaming` | PathChanged 事件 → 状态仍 `Streaming`、`path_changes`+1、数据报继续产出不断流 |
| `client_disconnect_reconnects_with_backoff_and_forces_idr` | Disconnected → `Reconnecting`；注入推进 0.5/1.5/3.5s 时各恰有一次 connect 尝试（不多不少）；`ReconnectRequest` 带原 session_id；Welcome bit0=1 → `Resumed` 且**立即**发出 `IdrRequest{reason=2}` |
| `client_resume_rejected_falls_back_to_fresh_hello` | bit0=0 → 同连接补发带**新** session_id 的 Hello、generation+1、会话钟归零 |
| `client_gives_up_after_max_attempts` | connect 恒失败 → 第 5 次后 `GaveUp`、状态 `Closed`、`reconnect_attempts`==5 |
| `client_bye_refresh_rebuilds_without_backoff` | Streaming 中收到 `Bye{Refresh}` → 下一拍即发全新 Hello（零等待，断言无退避延迟）；Welcome bit1=1 后偏移估计已清零（观测 probe 状态重置） |
| `watchdog_silent_while_reconnecting` | Reconnecting 期间注入时钟推进 10s，无任何 IdrRequest 发出（T16 看门狗被状态门禁，R8） |

**集成测试（`tests/reconnect.rs`，真 QUIC over `127.0.0.1`，进程内两端，证书照 `rc-net/tests/common/mod.rs:21-60` 模式内嵌）：**

| 测试名 | 断言要点 |
|---|---|
| `reconnect_over_real_quic_resumes_with_idr` | 建立会话流数帧 → drop client 传输（Drop 会 `close(0)`，host 立刻收到 Disconnected，`quic.rs:894-907`）→ host `Detached` → client 以测试配置（initial=50ms）自动重连 → `resumes`≥1、恢复后首个到达 client 的数据报是 IDR、`frame_seq` 延续、host generation 不变 |
| `rebind_migration_does_not_interrupt_session` | 照 `rc-net/tests/loopback.rs:540`（`connection_survives_rebind_migration`）范式：client 端点 `rebind` 新 socket → 会话仍 `Streaming`、host `path_changes`≥1、媒体数据报持续到达、状态机从未进 Detached/Reconnecting |
| `refresh_guard_rebuilds_session_end_to_end` | 注入 `refresh_guard_us=3_000_000`（3s）+ 注入会话时钟 → host 发 `Bye{Refresh}` 并拆管线 → client 零退避全新重连 → 新 session_id ≠ 旧、`Welcome` bit1=1、新会话 FrameSeq 从头、双端时钟读数回到小值 |

## 验收命令

```bash
# 内循环（M0 纪律：rc-session 依赖扇出大，日常只跑这两条）
cargo check -p rc-session
cargo test -p rc-session

# 全仓回归 + lint（验收时跑）
cargo test --workspace
cargo clippy -p rc-session --all-targets -- -D warnings

# R10 佐证：下层三 crate 零改动（exit 0 = 干净）
git diff --exit-code -- crates/rc-net crates/rc-pipeline crates/rc-types crates/rc-proto
```

## 验收标准

- [ ] `cargo test --workspace` 全绿；`cargo clippy -p rc-session --all-targets -- -D warnings` 无警告
- [ ] 退避序列 500ms→1s→2s→4s→8s、5 次后放弃——纯注入时间证明，测试内无真实 sleep
- [ ] 保持窗口：TTL 内重连复用管线（generation 不变、FrameSeq 延续、恢复后首帧 IDR）；过期后 resume 被拒（bit0=0）走全新 Hello
- [ ] 真 QUIC 回环：断线 → 自动重连 → 恢复，恢复后首个上屏帧为 IDR 且 500ms 内到达（缩阈值退避配置下）
- [ ] rebind 迁移：会话不断流、状态机不迁移、`path_changes`≥1
- [ ] 饱和守卫：注入时钟到 70min → host 发 `Bye{Refresh}`；client 零退避立即重连；新会话 session_id 更新、时钟归零、`Welcome` bit1=1
- [ ] `REFRESH_GUARD_US` 距 `u32::MAX` ≥ 60s 余量（测试断言）
- [ ] `git diff --exit-code -- crates/rc-net crates/rc-pipeline crates/rc-types crates/rc-proto` 干净（R10）
- [ ] 无 `unsafe`；公开 API 有中文文档注释（说明为什么）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-net/src/quic.rs`](../../../crates/rc-net/src/quic.rs) | `rebind`（:334）、`PathChanged` 在 poll 内产生（:858-864）、`Disconnected` 由读线程上报（:933-939）、Drop 即 close（:894-907）、空闲 30s/keep-alive 1s（:95-96）、**accept/connect 阻塞语义**（:253/:303） |
| [`rc-net/tests/loopback.rs`](../../../crates/rc-net/tests/loopback.rs) | `connection_survives_rebind_migration`（:540 起）：rebind 迁移测试范式，本卡集成测试直接照抄骨架 |
| [`rc-net/tests/common/mod.rs`](../../../crates/rc-net/tests/common/mod.rs) | 进程内连接对 + 内嵌自签证书 + `CONNECT_TIMEOUT` 防挂死模式（:21-60） |
| [`rc-pipeline/src/host.rs`](../../../crates/rc-pipeline/src/host.rs) | `request_idr` 粘住语义（:462）、停机红线顺序（:486-512）、采集背压（:200-224）、`tick`（:367）、`wait_event`（:355） |
| [`rc-loopback/src/lib.rs`](../../../crates/rc-loopback/src/lib.rs) | 驱动循环形态（:336-423）、会话时钟注入与零点锚定（:343-360、:399-405） |
| [`rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | MockTransport 测试替身 + 「脚本化事件 → 断言字节」范式（:195-223） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | `accept`/`connect` 是阻塞式（`quic.rs:253`/`quic.rs:303`）：Detached 期的 accept 轮询若用长超时，会把窗口判定和守卫检查饿死 | accept 一律短超时（≤ 驱动拍预算），超时返回当拍 `Waiting`；集成测试的 connect 用 `CONNECT_TIMEOUT` 上限防挂死 |
| 2 | 两种断线的 Disconnected 延迟差极大：对端 Drop 会立刻 `close(0)` 可见（`quic.rs:894-907`）；**网络静默死亡要等 30s 空闲超时**（`quic.rs:95`） | 窗口从 Disconnected 起算（不是「最后收包时刻」），总宽限 = idle 检出 + TTL；测试模拟断线用 drop transport（立刻可见），不要真等 idle |
| 3 | Detached 期间暂停 tick 后，采集线程阻塞在容量 1 的交接通道（`host.rs:200-224`）——这是背压不是死锁 | 恢复后先 `request_idr()` 再恢复 tick；积压的旧帧会被调度器按 Stale/Repeat 判丢或直接编成 IDR，都在一个帧间隔内出恢复帧 |
| 4 | 重建管线 = 重建后端：rc-pipeline **没有** reset API，本卡不得给它加（本轮 rc-pipeline 唯一允许的改动是 T15 的 `set_bitrate`） | 刷新/全新路径经 `set_pipeline_factory` 整体重建 `HostPipeline`/`ControllerPipeline`；FrameSeq 与时钟随之归零，恰好就是刷新语义 |
| 5 | 饱和刷新不是无缝的：刷新瞬间画面短暂中断；「连续 2 小时会话无崩溃」的 M2 出口指标需在此机制上实测确认 | 本卡不做双纪元并行切换；测试用注入阈值（3s）走完整路径，**不要真等 70 分钟** |
| 6 | T16 的「在途 IdrRequest 未确认不重发」去抖会把重连恢复堵死：旧连接的 IdrAck 永远不会来 | 重连成功（Resumed）时清空在途请求状态、重置看门狗基准；resume 路径时钟未归零，T15 的 RTT/偏移估计可延续，Rebuilt 路径则必须清零（bit1 告知） |
| 7 | client 的 QuicEndpoint 必须长存：endpoint 被 drop，重连所需的连接上下文就没了 | `ClientSession` 持 endpoint（T14 已然），断线只丢 transport；peer_addr / 证书 / server_name 建连时留存重用 |
| 8 | 时钟注入纪律（铁律 4）：退避到点、窗口过期、守卫触发全部比对参数 `now_us`；测试推进注入时钟即可「快进」TTL / 70min，禁止真实 sleep 等待 | 唯一允许的真实等待是集成测试里的 QUIC 握手（有上限防挂死） |
| 9 | `PathChanged` 只在 `poll` 里产生（`quic.rs:858-864`）：会话不持续 poll 就看不到迁移，也看不到断线 | 驱动循环每拍必 poll（含 Detached/Reconnecting 态）；迁移测试断言「状态从未离开 Streaming」 |
| 10 | 报告口径：resume 路径 trace / 时钟跨断线连续（可整体算 p50）；Rebuilt 路径双端时钟归零，两段**不可**混进同一个分位数 | T17 只负责 `ReconnectStats`（refreshes/rebuilds 计数）供 T18 报告分段；偏移换算忘了清零，数字就是垃圾（设计师 risk 1 同源） |
| 11 | rc-session 依赖扇出大（rc-net + rc-pipeline + rc-input），全仓 test 慢 | 遵守 M0 纪律：内循环只 `cargo check -p rc-session` / `cargo test -p rc-session`，`cargo test --workspace` 留给验收 |

## 依赖

```toml
# Cargo.toml 不加新依赖（T13 已建全：rc-net / rc-pipeline / rc-input / rc-types / rc-trace）
```

前置卡：**T15 与 T16**（两者并列，各自只依赖 T14，不可默认「T16 自带 T15」）——本卡与 T15 的直接交互面：刷新/重建路径清零其时钟偏移估计（R6）、resume 成功沿用其 RTT/偏移、重连成功后重新 `start_bw_probe`（T15 接口节）；与 T16 的直接交互面：重连成功清空在途 IdrRequest 去抖状态、重置看门狗基准（R4/坑 6）。上游 T13（proto 线格式）、T14（会话状态机与 testkit）。本卡不改任何下层 crate。

## 与后续卡的接口

- **T18**（rc-cli host/client）：`--reconnect-at <t>` 场景 = 到点 drop 传输层，然后靠本卡自动恢复；client 报告的 `reconnect` 段（resumed / 首个 IDR ≤500ms / ReconnectStats）数据全部来自本卡；`Rebuilt` 段的时钟归零口径见已知坑 10。
- **M3-8**（跨网段迁移策略）：在本卡 PathChanged 透传之上做策略（何时主动 rebind、多路径选择），不改本卡状态机。
- **M2 出口**（2h 长跑认证）：实测本卡守卫的中断可接受性；若不可接受再立项双纪元并行切换（`host_flags` bit1 位已留）。
