# T22 被控端托盘 + 接受弹窗（M2-7）

> 给被控端装上「常驻 + 人裁决」：macOS 菜单栏托盘（新 crate `rc-host-macos`）
> 与连接请求审批弹窗；审批机制**复用 T19 已沉淀在 `rc-session` auth.rs 的**
> 平台无关 `ConnectionApprover` trait（本卡不另立抽象、不新建 approval
> 模块），与 T19 密码鉴权联动（先密码后弹窗）。

## 背景（为什么做这个）

T18 交付的双进程直连里，被控端是一个 headless CLI：accept 一切来连接，
`--duration` 到点就退出。产品层面对被控端有两条 P0 要求（05-features.md）：
**A4 接受确认弹窗**（可选关闭）与 **C1 被控端无人值守运行（托盘）**——
远控的第一道门必须是「机主知情同意」，否则任何拿到地址的人都能直接看屏。

本卡把「机主裁决」接进会话握手的 Hello→Welcome 之间：审批机制本体
（trait + submit 时机 + 超时判定 + 裁决映射）**已由 T19 沉淀在
`rc-session` 的 `auth.rs`**（`ConnectionApprover` 三方法 +
`HostAuthenticator::poll_approval` 内嵌审批等待，离线确定性可测），本卡
不重复定义、不另插第二道审批轮询；
弹窗与托盘放新平台 crate `rc-host-macos`（objc2-app-kit 系统绑定，对齐
rc-input-macos / rc-render-macos 的既有模式，不引 Tauri/GPUI 这类应用框架
——控制端重 UI 才是 T21 的事，且 08-ui-design.md 的 Tauri 裁决只约束控制端
12 屏）。与 T19 的联动顺序：**先鉴权后弹窗**——密码错直接拒绝（不打扰
机主），密码通过或未启用密码才征询机主。

前置：T18（host/client 子命令）、T19（AuthChallenge/Response、密码校验与
`ConnectionApprover` 审批编排）已完成，`rc-session` 已存在（workspace
成员序号以合并时 `members` 实际数量为准——T21 的 `rc-ui` 与本卡并行，
谁先合并谁先占位，不硬编码「第 N 个」）。本卡新增成员
`rc-host-macos`。**不新增任何控制消息号**：审批的接受/拒绝完全复用
`Welcome.auth_status`（T13 已定 `AuthStatus::Ok/ChallengeRequired/Rejected`）。

## 平台要求

- **审批相关 rc-session 改动（client 超时放宽 + testkit `MockApprover`）**：
  全平台、离线确定性，无 IO、无时间依赖（超时判定用注入会话时钟，
  铁律 4；审批逻辑本体已随 T19 交付，本卡不重复实现）。
- **托盘 + 弹窗（rc-host-macos）**：仅 macOS。纯逻辑模块（状态/标签/通道桥）
  全平台编译可测（对齐 rc-input-macos 的分层）；AppKit 模块 `cfg(target_os =
  "macos")`。托盘与 NSAlert **不需要任何 TCC 权限**（不碰屏幕录制/辅助功能），
  但需要 GUI 登录会话（SSH 下 NSStatusItem 创建失败）。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/client_session.rs` | 修改 | client 的 Hello→Welcome 等待超时放宽为 ≥ `approval_timeout + 15s`（U6，对 T14 默认参数的显式修正）。**host 侧审批等待点不在此改**：审批轮询（submit 时机/超时/裁决映射）已由 T19 `HostAuthenticator::poll_approval` 交付，本卡不另插第二道 |
| `crates/rc-session/src/testkit.rs` | 修改 | 增量加 `MockApprover`（可编排裁决 / 可观察 submit·cancel 调用；审批类型本体不在此定义——见 API 契约 §1） |
| `crates/rc-session/tests/localhost.rs` | 修改 | 增量加审批场景集成测试（§测试清单 1–8） |
| `crates/rc-host-macos/Cargo.toml` | 新建 | objc2 0.6 / 绑定 0.3 版本线对齐 rc-render-macos；`tray-bench` feature |
| `crates/rc-host-macos/src/lib.rs` | 新建 | crate 骨架 + `TrayHost` / `TrayApprover` / `TrayEvent` / `TrayConfig` / `TrayState` / `HostUiError` 组装 |
| `crates/rc-host-macos/src/state.rs` | 新建 | 托盘状态与菜单标签纯函数（无 AppKit，全平台可测） |
| `crates/rc-host-macos/src/bridge.rs` | 新建 | `ApprovalBridge`：会话线程 ⇄ 主线程的审批通道桥（纯 std channel，全平台可测） |
| `crates/rc-host-macos/src/appkit.rs` | 新建 | `cfg(macos)`：NSStatusItem/NSMenu/NSAlert 与主线程手泵事件循环，本 crate 全部 unsafe 集中地 |
| `crates/rc-host-macos/assets/tray-template.png` | 新建 | 16pt 单色模板图（`include_bytes!`，`isTemplate` 随深浅菜单栏自适应）；32@2x 一并内嵌或用 NSImage 单尺寸缩放 |
| `crates/rc-host-macos/tests/tray_bench.rs` | 新建 | `harness = false` + `required-features = ["tray-bench"]`（照抄 rc-render-macos window_present 模式） |
| `crates/rc-cli/src/cmd/mod.rs` | 修改 | `HostArgs` 加 `--tray` / `--approval` / `--approval-timeout`；`ApprovalMode` 枚举 |
| `crates/rc-cli/src/cmd/host.rs` | 修改 | tray 分支派发（组合根，增量约百行内，逻辑不进 CLI） |
| `crates/rc-cli/Cargo.toml` | 修改 | `cfg(target_os = "macos")` 下加 `rc-host-macos` |
| `crates/rc-cli/tests/host_client.rs` | 修改 | 增量加 deny 模式与参数校验集成测试 |
| `Cargo.toml`（workspace 根） | 修改 | `members` 追加 `"crates/rc-host-macos"` 一行，不改其他任何行（成员序号以合并时实际数量为准——T21 的 `rc-ui` 并行添加，先合并者先占位，不硬编码「第 N 个」） |
| `TASK.md`、`docs/reference/tickets/README.md` | 修改 | §0/§12/§13 回写实测与人工验收记录；卡片状态表加 T22 |

## 依赖

```toml
# rc-host-macos/Cargo.toml（新建）
[target.'cfg(target_os = "macos")'.dependencies]
# 版本线与 rc-render-macos 严格一致（objc2 生态 0.6 / 平台绑定 0.3，
# rc-render-macos/Cargo.toml:18-19 有同款注释）——混版本会出现两份 objc2。
rc-session = { path = "../rc-session" }
objc2 = "0.6"
objc2-foundation = "0.3"
objc2-app-kit = { version = "0.3", default-features = false, features = [
    "std",
    "NSApplication",
    "NSResponder",
    "NSStatusBar",
    "NSStatusItem",
    "NSMenu",
    "NSMenuItem",
    "NSButton",     # statusItem.button
    "NSAlert",
    "NSImage",
    # 按 0.3 绑定的实际 feature 名核对增删（activate 相关常量所在 feature
    # 以 docs.rs 为准），保持 default-features = false + 精确列表的纪律。
] }

# rc-cli/Cargo.toml 增量
[target.'cfg(target_os = "macos")'.dependencies]
rc-host-macos = { path = "../rc-host-macos" }
```

rc-session **零新增依赖**（本卡对 rc-session 只加 testkit 的 `MockApprover`
与 client 超时放宽，均只用 std + crate 内既有类型；审批类型由 T19 auth.rs
交付，不再新建 approval 模块）。
不引 serde / Tauri / GPUI / eframe / 任何托盘抽象库（`tray-icon` 等）。

## 对前置卡的消费面（不重新定义，只声明本卡怎么用）

以下类型与机制由 T13–T19 交付，**确切签名以对应卡契约为准**；实现时若发现
命名有出入，改本卡的调用处对齐前置卡，不改前置卡（T18 卡同款纪律）：

| 消费项 | 来源卡 | 本卡用法 |
|---|---|---|
| `SessionParams` / `AuthMethod` / `AuthStatus` | T13 | `ApprovalRequest` 复用这三个类型组包；拒绝复用 `Welcome{auth_status: Rejected}`，**不加消息号、不改 proto** |
| `HostSession` 握手序列（Init→Connected）与驱动循环 | T14 | 审批等待点在 Hello 之后、Welcome 之前（由 T19 状态机编排，见下行）；驱动循环形态（poll 短超时 + 注入时钟）不变；停机红线（先停采集→排空→停编码）复用，托盘「退出/断开」只是新的停机触发源 |
| **`ConnectionApprover` trait / `ApprovalRequest` / `ApprovalVerdict` / `AutoApprover` / `DenyApprover`**（`rc_session::auth`，T19 auth.rs 交付） | T19 | **唯一审批抽象，本卡不另立**：`TrayApprover`/桥的 `SessionSide` 实现该 trait；submit 时机、`approval_timeout_us` 超时判定（注入时钟）与裁决→Welcome 映射都在 T19 `HostAuthenticator::poll_approval`，本卡不重写编排、不插第二道审批轮询 |
| AuthChallenge/AuthResponse 与密码校验结果 | T19 | **先鉴权后弹窗**：密码校验失败 → 直接 `Welcome(Rejected)` 断开，`approver.submit` 不被调用；成功或未启用密码 → 进入审批。密码校验结果经 `ApprovalRequest::password_verified` 进弹窗文案 |
| `host` / `client` 子命令与 testkit | T18 | 本卡给 host 加三个参数；**headless 默认行为（auto + 30s 超时不可见）与 T18 完全一致**，`tests/host_client.rs` 既有用例零改动通过；client 收 `Welcome(Rejected)` 的处理若 T19 已有拒绝路径则复用，否则补「以明确错误终止、exit 1、stderr 写明被拒，不 panic 不挂死」 |
| `QuicEndpoint::accept` / 端点自驱线程模型 | T09（已交付） | `accept` 阻塞式（quic.rs:303）由 T14 驱动循环的超时 poll 化解；审批等待期间 QUIC ACK/keep-alive 由 rc-net 后台线程推进（quic.rs 模块文档 1-19「端点自驱」），host 驱动循环只管不阻塞即可 |

## API 契约

> `20-crate-contracts.md` 尚无 rc-session 审批节与 rc-host-macos 节，
> **本节即契约**，后续汇总文档照此抄入。跨卡共用语义（`Welcome.auth_status`
> 的审批复用、先鉴权后审批的顺序）以此为准，不得自创分歧。

### 1. 审批机制（**消费 T19 交付，本卡零重复定义**）

审批的类型与编排**全部由 T19 的 `auth.rs` 契约交付**（本卡原计划的
`approval.rs` 模块已并入 T19，不再新建，避免同 crate 出现第二套同职责
trait 与同名 `AutoApprover`）。本卡按以下清单消费：

- `ApprovalRequest { session_id, remote_addr, requested, auth_method,
  password_verified }`——弹窗文案唯一信息源（U9），由 T19
  `HostAuthenticator` 在首次进入等待确认时组装并 submit；
- `ApprovalVerdict { Accepted, Rejected }`——超时不在此枚举，由会话层
  按注入时钟判成 `Rejected`（UI 不感知超时）；
- `trait ConnectionApprover { submit(&ApprovalRequest);
  poll_verdict() -> Option<ApprovalVerdict>; cancel(); }`——三方法
  **全部非阻塞立即返回**（U2），裁决经 `poll_verdict` 异步取回，会话
  驱动循环得以继续 poll 网络事件（U6）；
- 内置替身 `AutoApprover`（`--approval auto`，headless 默认，保持 T18
  行为：审批环节零拍延迟）/ `DenyApprover`（`--approval deny`，远程运维
  锁主机 / CI 验证拒绝路径）/ `ScriptedApprover`（序列回放，行为样板）；
- 编排（**本卡不重写**）：T19 `HostSessionConfig` 已增设 `approver:
  Box<dyn ConnectionApprover>` 字段（默认 `AutoApprover`，未显式配置的
  既有调用点行为不变，U3）+ `HostAuthenticator::poll_approval(now_us,
  approver)` 每拍轮询——submit 时机（先鉴权后审批：密码失败**不
  submit** 直接 `Welcome(Rejected)` 断开）、`approval_timeout_us` 超时
  （注入时钟 + deadline 比对）、裁决映射（Accepted→`Welcome(Ok)`、
  Rejected/超时→`Welcome(Rejected)`+断开）、等待期 client 断开时
  `approver.cancel()` 恰好一次，全在 T19 状态机与会话层。

**本卡对会话层的实际增量只有一处**：

- client 侧：`Hello→Welcome` 等待超时放宽为
  `max(T14 既有值, approval_timeout_us + 15s)`——弹窗等人 30s 期间 client
  不得先超时（U6；这是对 T14 默认参数的显式修正点，写进提交说明）。
- host 侧组合根：托盘模式把 `TrayApprover` 塞进 T19 已有的 `approver`
  字段；`--approval auto/deny` 直接用 T19 交付的 `AutoApprover`/
  `DenyApprover`，不重写。

### 2. `rc-session/src/testkit.rs` 增量：`MockApprover`

```rust
/// 可编排的审批人（离线测试 / scripted 模式复用）。
pub struct MockApprover { /* 私有 */ }

impl MockApprover {
    pub fn new() -> Self;
    /// 预置裁决：`submit` 后 `poll_verdict` 立即返回它。
    pub fn preset(&mut self, verdict: ApprovalVerdict);
    /// 观测：收到的审批请求（断言字段完整性用）。
    pub fn take_request(&mut self) -> Option<ApprovalRequest>;
    /// 观测：`submit` 调用次数。
    pub fn submit_count(&self) -> u32;
    /// 观测：`cancel` 调用次数。
    pub fn cancel_count(&self) -> u32;
}

impl rc_session::auth::ConnectionApprover for MockApprover { /* 按编排回放 */ }
```

### 3. `rc-host-macos`（托盘 + 弹窗，macOS 平台 crate）

```rust
// crates/rc-host-macos/src/lib.rs
//! 被控端驻留 UI（M2-7）：菜单栏托盘 + 连接审批弹窗。
//! 分层对齐 rc-input-macos：state/bridge 是无 AppKit 的纯逻辑（全平台
//! 编译可测）；appkit 模块集中本 crate 全部 unsafe（cfg macos）。
//! **必须在主线程构造并进入 [`TrayHost::run`]**（NSStatusItem/NSAlert 是
//! MainThreadOnly，objc2 以 MainThreadMarker 编码该契约——同 rc-render-macos
//! 的 NSWindow 纪律，见其 lib.rs:38-39）。

/// 托盘配置。
#[derive(Debug, Clone)]
pub struct TrayConfig {
    /// 监听地址（托盘状态行展示「等待连接 (127.0.0.1:9901)」）。
    pub listen: std::net::SocketAddr,
}

/// 托盘可见状态（纯数据，state.rs 的菜单标签函数吃它）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    /// 等待连接（含 listen 地址）。
    WaitingForConnection(std::net::SocketAddr),
    /// 会话中（含对端地址；图标加「●」标记区分）。
    InSession(std::net::SocketAddr),
}

/// 托盘向宿主进程抛的事件（主线程回调，动作由组合根转发给会话线程）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 用户点了「断开当前会话」（无会话时菜单项禁用，不会触发）。
    DisconnectRequested,
    /// 用户点了「退出」（runloop 已停；宿主应优雅停机后从 run 返回）。
    QuitRequested,
}

/// rc-host-macos 的错误。
#[derive(Debug)]
pub enum HostUiError {
    /// 无 GUI 会话（SSH / 无 Aqua 的 launchd 环境）：NSStatusItem 创建失败。
    NoGuiSession,
    /// AppKit 操作失败（附上下文）。
    AppKit(String),
}

/// 菜单栏托盘宿主。构造 → 取 approver → spawn 会话线程 → run。
pub struct TrayHost { /* 私有 */ }

impl TrayHost {
    /// 初始化 AppKit（`ActivationPolicy::Accessory`——不占 Dock、不抢
    /// 焦点）并创建状态项与菜单。**主线程限定**（内部断言
    /// `MainThreadMarker`）。
    pub fn new(cfg: TrayConfig) -> Result<Self, HostUiError>;

    /// 取审批人（在 spawn 会话线程**之前**调用，装进 `HostSessionConfig`
    /// 的 `approver` 字段——T19 已增设，默认 `AutoApprover`）。
    /// 它实现 [`rc_session::auth::ConnectionApprover`]：`submit` 经
    /// [`bridge::ApprovalBridge`] 把请求递给主线程弹 NSAlert，机主点击后
    /// 裁决经桥返回。`Send`（只持 channel 端点，不含 AppKit 对象）。
    pub fn approver(&self) -> TrayApprover;

    /// 更新托盘状态（会话线程可调：内部经 channel 转发主线程改菜单，
    /// **不跨线程触碰 AppKit 对象**）。幂等。
    pub fn set_state(&self, state: TrayState);

    /// 进入主线程手泵事件循环（drain AppKit 事件 + poll 桥上请求/状态 +
    /// 10ms 空转退避），阻塞至「退出」。审批到达时同步 `NSAlert::runModal`
    /// （模态期间其余事件由模态循环处理；见已知坑 4）。`on_event` 在主线程
    /// 回调。返回即代表用户已请求退出。
    pub fn run(&mut self, mut on_event: impl FnMut(TrayEvent)) -> Result<(), HostUiError>;
}

/// 会话线程侧审批句柄（`TrayHost::approver` 的返回类型）。
pub struct TrayApprover { /* 私有：仅桥的会话端点 */ }
impl rc_session::auth::ConnectionApprover for TrayApprover { /* 转发桥 */ }
```

```rust
// crates/rc-host-macos/src/state.rs（纯逻辑，无 AppKit，全平台可测）

/// 菜单标签全集（纯函数：状态 → 各菜单项文案与可用性）。
/// 文案是本卡钉死的可见契约，测试锁死（见 menu_labels_render_state）：
/// - 状态行（禁用项）：「RabbitControl · 等待连接 (<addr>)」/
///   「RabbitControl · 会话中 (<remote>)」
/// - 「断开当前会话」：仅 InSession 可用
/// - 「退出」恒可用
pub struct TrayMenuLabels {
    pub status_line: String,
    pub disconnect_title: String,
    pub disconnect_enabled: bool,
    pub quit_title: String,
}

pub fn menu_labels(state: &TrayState) -> TrayMenuLabels;

/// 弹窗文案（纯函数：`ApprovalRequest` → NSAlert 的 messageText 与
/// informativeText，含对端地址 / 请求参数 / 鉴权状态三要素——U9）。
pub fn alert_texts(req: &rc_session::auth::ApprovalRequest)
    -> (String /* messageText */, String /* informativeText */);
```

```rust
// crates/rc-host-macos/src/bridge.rs（纯 std channel，全平台可测）

/// 会话线程 ⇄ 主线程的审批桥。会话端实现 `ConnectionApprover` 语义；
/// 主线程端在 `TrayHost::run` 的手泵循环里被消费（弹窗），全逻辑无
/// AppKit 依赖——桥的往返正确性离线可测。
pub struct ApprovalBridge { /* 私有 */ }

impl ApprovalBridge {
    pub fn new() -> Self;
    /// 会话端句柄（`Send`）。
    pub fn session_side(self) -> SessionSide;
    /// 主线程端句柄。
    pub fn ui_side(&self) -> UiSide;
}

/// 会话端：实现 `ConnectionApprover`（submit/poll_verdict/cancel 全非阻塞）。
pub struct SessionSide { /* 私有 */ }
impl rc_session::auth::ConnectionApprover for SessionSide { /* 转发通道 */ }

/// 主线程端。
pub struct UiSide { /* 私有 */ }
impl UiSide {
    /// 非阻塞取走待审批请求（至多一个在途）。
    pub fn take_request(&mut self) -> Option<rc_session::auth::ApprovalRequest>;
    /// 提交裁决（机主点击「接受/拒绝」后调用）。
    pub fn resolve(&mut self, verdict: rc_session::auth::ApprovalVerdict);
    /// 有待撤销的审批（client 断开）→ true（弹窗应关闭）。
    pub fn take_cancel(&mut self) -> bool;
}
```

### 4. `rc-cli/src/cmd/mod.rs` 参数增量（风格照抄 `HostArgs`，`parse_duration` 复用 mod.rs:134-147）

```rust
/// 常驻托盘模式（仅 macOS；其他平台运行时报错）：菜单栏图标 + 审批
/// 弹窗，进程不随单会话退出——会话结束后回到 accept 等待下一条连接
/// （一次一条，U8）。tray 模式下 `--duration` 默认不再兜底（等 client
/// 的 Bye、托盘「断开」或「退出」；显式传值仍按会话兜底生效）。
#[arg(long)]
pub tray: bool,

/// 审批模式：prompt = 弹窗征询（--tray 时的默认）；auto = 自动接受
/// （headless 默认，A4「可选关闭」，T18 行为不变）；deny = 一律拒绝。
/// prompt 必须与 --tray 同用，否则参数校验报错。
#[arg(long, value_enum)]
pub approval: Option<ApprovalMode>,

/// 审批等待超时（prompt 模式下无人应答按拒绝断开）。
#[arg(long, default_value = "30s", value_parser = parse_duration)]
pub approval_timeout: Duration,

/// `host --approval` 取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum ApprovalMode { Prompt, Auto, Deny }
```

`cmd/host.rs` 的 tray 分支（组合根，逻辑在 rc-host-macos/rc-session）：
macOS 且 `--tray` → 主线程构造 `TrayHost` → spawn 会话线程（循环
`accept` → 构造带审批的 `HostSession` → 驱动到结束 → `set_state`
回等待；`TrayEvent::DisconnectRequested` → 当前会话发 `Bye(Normal)` 收尾；
`QuitRequested` → 会话中则先停会话（停机红线顺序由 T14 会话层保证）再退出
进程）→ 主线程 `run(on_event)` 阻塞。headless 路径除新增两参数映射
（`DenyApprover` / 超时）外零改动。`--report` 在 tray 模式下每会话覆盖写
（最后一次会话的报告留在文件里）。

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| U1 | 审批插在 Hello 之后、Welcome 之前（**编排由 T19 `HostAuthenticator::poll_approval` 承担，本卡验证而不重写**）；**先鉴权后弹窗**（密码失败不 `submit` 直接 Rejected）；裁决映射：Accepted→`Welcome(Ok)`、Rejected/超时→`Welcome(Rejected)`+断开；**不加控制消息号** | 安全模型错位：未鉴权先打扰机主 / 拒绝语义无协议表达 |
| U2 | 审批超时判定只用注入会话时钟（铁律 4）；UI（NSAlert）是人机交互 IO 边界允许墙钟；`ConnectionApprover` 三方法全部非阻塞 | 离线测试不可复现超时；驱动循环被 UI 卡死 |
| U3 | headless 默认行为与 T18 完全一致（默认 AutoApprover + 30s 不可见），`rc-cli/tests/host_client.rs` 既有用例**零改动**通过；`cargo test --workspace` 不回归 | CI 与既有实测口径断裂 |
| U4 | AppKit 对象只活在主线程（`MainThreadMarker` 契约，同 rc-render-macos）；跨线程只传纯数据 + channel | AppKit 线程违约 = 运行期未定义行为 |
| U5 | objc2 版本线对齐 workspace 既有（0.6 / 绑定 0.3），`default-features = false` + 精确 feature；**不引 Tauri/GPUI/eframe/托盘抽象库** | 双份 objc2 编译冲突 / 违反纯 Rust 无重依赖纪律 |
| U6 | 弹窗等待期 host 驱动循环不阻塞（poll 短超时继续泵网络；QUIC 保活由 rc-net 端点自驱后台线程推进，quic.rs:1-19）；client 的 Hello→Welcome 等待放宽为 ≥ `approval_timeout + 15s` | 审批 30s 期间传输层死亡 / client 先超时误判失败 |
| U7 | 托盘「退出/断开」走 T14 停机顺序红线（会话中：先停会话（发 Bye）→ 停采集 → tick 排空 → 停编码）；CLI 不自写停机序列 | 授权帧凭空消失（M1 已踩过的坑） |
| U8 | tray 常驻 = 一次一条会话循环 accept；第二条连接在传输层排队等上一条结束，不做并发多会话 | 范围蔓延成会话管理器 |
| U9 | 弹窗信息以 `ApprovalRequest` 为唯一信息源，展示对端地址（传输层地址，非自报）、请求参数、鉴权状态三要素 | 机主基于错误信息做裁决 = 确认机制形同虚设 |
| U10 | rc-cli 保持薄壳：本卡对 cmd/host.rs 的改动只是参数解析 + tray 分支派发（增量约百行内），托盘/弹窗/审批逻辑全部在 rc-host-macos 与 rc-session | CLI 变厚（T18 红线） |
| U11 | 中文注释贴合周边风格；进度/摘要只进 stderr（loopback.rs:1-8 分流约定）；被拒的 client 以明确错误 exit 1，不 panic 不挂死 | 验收脚本解析失败 / 排障两眼一黑 |
| U12 | 完成后回写 TASK.md §0/§12/§13（人工验收场景逐条记录）与 tickets README 状态表（AGENTS.md §九） | 视为未完成 |

## 不要做什么

- ❌ 不做服务化 / 开机自启（launchd / SMAppService / E1 / E2——M4/M5）。
- ❌ 不做「永久信任」与白名单（A4 的「本次/永久」只做本次；「永久」需要
  设备指纹 H3 / 白名单 H8，后续里程碑）；弹窗只有「接受 / 拒绝」两个裁决。
- ❌ 不做原生系统通知（E4 的 UNUserNotificationCenter 不引；弹窗即本卡的
  连接请求通知形态）。
- ❌ 不做 Windows/Linux 托盘（M4；`ConnectionApprover` 保持平台无关即可）。
- ❌ 不做控制端 UI / 不碰 12 屏设计（T21；Tauri 裁决只约束控制端 App）。
- ❌ 不改 `rc-net` / `rc-pipeline`（对两者只读）；不动 `rc-types`（铁律 5）；
  **不加控制消息号**（审批复用 `Welcome.auth_status`）。
- ❌ 不做并发多会话、不做会话列表 / 历史记录菜单。
- ❌ 不做设置界面（菜单只有状态行 / 断开 / 退出三项）。
- ❌ 不引 serde / Tauri / GPUI / `tray-icon` 等任何新框架或托盘库。
- ❌ 不给 rc-loopback / 回环路径加审批（回环无对端，审批只在双机会话）。

## 测试清单

**rc-session 单测（离线确定性；`AutoApprover`/`DenyApprover` 的行为单测已随审批类型并入 T19 auth.rs 测试清单——`deny_approver_rejects_immediately` 等，本卡不重复；本卡 rc-session 侧仅 testkit 新增件的内联测试）：**

| 测试名 | 断言要点 |
|---|---|
| `mock_approver_preset_and_observation` | `MockApprover`：`preset(Accepted)` 后 submit→`poll_verdict` 即 `Some(Accepted)`；`take_request` 取到 submit 递入的请求（字段完整）；`submit_count`/`cancel_count` 计数正确、`cancel` 幂等 no-op 不 panic |

**rc-session 集成测试（`tests/localhost.rs` 增量，进程内双端、真 QUIC over 127.0.0.1、CI 可跑、免权限）：**

| 测试名 | 断言要点 |
|---|---|
| `approval_request_fields_reach_approver` | MockApprover 收到的 `ApprovalRequest`：`remote_addr` 是 client 传输层地址、`requested` 与 Hello 一致、`auth_method` 一致（U9 的字段完整性） |
| `approval_accept_sends_welcome_ok` | 预置 Accepted → client 收 `Welcome{auth_status: Ok}`，会话正常进入 Streaming 并跑完短会话 |
| `approval_reject_sends_welcome_rejected_and_closes` | 预置 Rejected → client 收 `Welcome{auth_status: Rejected}` 后连接被 host 关闭，client 侧以明确错误终止（U11） |
| `approval_timeout_rejects_by_injected_clock` | MockApprover 不应答：注入时钟停在 deadline 前 1µs → 仍无 Welcome；恰好越过 → Rejected 路径触发——证明超时判定只吃注入时钟（U2/铁律 4） |
| `approval_timeout_is_configurable` | timeout=2s 的 host 在注入时钟推进 2s 后走 Rejected；30s 配置下推进 2s 不触发 |
| `approval_skipped_when_password_fails` | T19 联动顺序：密码校验失败的 client → `submit_count() == 0`（未打扰机主）+ `Welcome(Rejected)`（U1） |
| `approver_cancel_on_client_disconnect_during_approval` | 审批在途时 drop client 传输 → `cancel_count() == 1` 恰好一次，host 会话按 T14 断线路径清理，无死锁无泄漏 |
| `headless_default_keeps_t18_handshake_shape` | 默认（未显式配置审批）host 在 Hello 后同一拍内出 Welcome（AutoApprover 零拍延迟），首帧时序与 T14/T18 基线一致（U3） |

**rc-host-macos 单测（纯逻辑，无 AppKit，`src/state.rs` / `src/bridge.rs` 内联，全平台可跑）：**

| 测试名 | 断言要点 |
|---|---|
| `menu_labels_render_state` | Waiting/InSession 两态的状态行文案、`disconnect_enabled` 开关、「退出」恒可用（文案是可见契约，锁死） |
| `alert_texts_contain_three_elements` | `alert_texts` 输出含对端地址、`WxH@fps` 请求参数、鉴权状态（未启用密码 / 密码已验证两种）（U9） |
| `approval_bridge_roundtrip` | 桥双端：session 侧 `submit` → ui 侧 `take_request` 取到同值 → `resolve(Accepted)` → session 侧 `poll_verdict` 返回；未决时 `poll_verdict == None` |
| `approval_bridge_cancel_idempotent` | ui 侧 `take_cancel` 首次 true 后复位；session 侧 `cancel` 连调两次不 panic、ui 侧只感知一次 |
| `bridge_request_before_submit_is_none` | 无在途时 `take_request`/`take_cancel` 均 None |

**rc-host-macos 真机测试（`tests/tray_bench.rs`，`harness = false` + `required-features = ["tray-bench"]` + 显式 `--run` 旗标——默认 `cargo test --workspace` 既不编译也不运行，语义等价 `#[ignore]`；需 GUI 登录会话，无需 TCC 权限）：**

| 测试名（fn main 内步骤） | 断言要点 |
|---|---|
| `tray_bench`（`--run`） | `MainThreadMarker` 断言主线程 → `TrayHost::new` 成功（状态项创建）→ `set_state` 两态切换 → 用 `alert_texts` 构造 NSAlert 并读取其 `messageText`/按钮标题断言（**不 `runModal`**，人工验收弹窗本体）→ 桥上模拟一次 submit→resolve 往返 → 正常 teardown 退出 0 |

**rc-cli 集成测试（`tests/host_client.rs` 增量，双子进程、CI 可跑、headless）：**

| 测试名 | 断言要点 |
|---|---|
| `deny_mode_rejects_client` | host `--approval deny` + client 连接：client exit 1、stderr 含拒绝语义、连接被 host 主动关闭（U11 的端到端验证） |
| `invalid_approval_mode_rejected` | `--approval bogus` → clap exit 2 |
| `prompt_without_tray_is_rejected` | headless 下 `--approval prompt`（无 `--tray`）→ 参数校验报错 exit 2（不挂起等一个不存在的弹窗） |

**人工验收清单（真机 + 肉眼，M2-7 的产品验收；逐条记入 TASK.md §12/§13）：**

托盘图标出现（深浅菜单栏均可见）→ 状态行「等待连接」→ client 连接触发弹窗
（地址/参数/鉴权三要素正确）→「接受」后 client 会话正常（`gate.passed=true`）
→「拒绝」后 client 断开 → `--approval-timeout 5s` 不点 → 5s 自动拒绝 →
会话中图标状态变化 +「断开当前会话」→ client 收 Bye 断开 → 「退出」→
会话中退出也走停机红线优雅收尾。

## 验收命令

```bash
# 1. 内循环（铁律 3：check + 单 crate 测试）
cargo check -p rc-session --all-targets
cargo check -p rc-host-macos --all-targets
cargo check -p rc-cli --all-targets
cargo test -p rc-session
cargo test -p rc-host-macos
cargo test -p rc-cli

# 2. 全仓回归 + 质量门
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# 3. 托盘真机基准（GUI 登录会话；无需 TCC 权限；统一走幕布工具保持口径）
./scripts/macos-dev-run.sh test rc-host-macos \
    --features tray-bench --test tray_bench -- -- --run --nocapture

# 4. 手动实测（release；终端 1 —— 托盘出现，等弹窗）
./scripts/macos-dev-run.sh cli --release -- host \
    --tray --listen 127.0.0.1:9905 --backend fake --inject-mode record

#    （终端 2；host 弹窗点「接受」后 client 正常跑完）
./scripts/macos-dev-run.sh cli --release -- client \
    --connect 127.0.0.1:9905 --backend fake --duration 10s \
    --input-mode scripted --report /tmp/m2-7-client.json

# 5. 拒绝与超时演练（host 端分别：弹窗点「拒绝」；或 --approval-timeout 5s 不点）
#    另跑一对：host 换 --approval deny（headless 自动化口径同 4 的 client 断言）
```

说明：命令 4 的 host 无 `--duration`——tray 模式默认不兜底（见 API 契约 §4），
进程由托盘「退出」结束；终端 1 的 `macos-dev-run.sh` 以摘要行判定结束
（scripts/macos-dev-run.sh:115-120），点「退出」前它会一直等，属预期。
弹窗出现在 host 机器（localhost 实测即本机屏幕）。

## 验收标准

- [ ] `cargo test --workspace` 全绿（含 rc-session 侧 testkit 1 项 + 审批集成
      8 项、rc-host-macos 纯逻辑 5 项、rc-cli 增量 3 项；`AutoApprover`/
      `DenyApprover` 行为单测已并入 T19 auth.rs 清单）；**T18 既有 host_client
      用例零改动通过**（U3）
- [ ] 离线确定性测试覆盖：接受/拒绝/超时（注入时钟推进判定）/先鉴权后
      弹窗/断线 cancel/字段完整性——全部不依赖墙钟与真人（U2）
- [ ] `tray_bench`（`--run`）通过：托盘创建、状态切换、弹窗文案三要素、
      桥往返（GUI 会话）
- [ ] 人工验收清单逐条通过并记入 TASK.md（接受后 `gate.passed == true`、
      拒绝/超时后 client 明确断开、托盘断开/退出走停机红线）
- [ ] 审批未决期（30s 级）host 驱动循环存活：QUIC 保活不断（U6，
      `approver_cancel_on_client_disconnect_during_approval` + 手动演练）
- [ ] `--tray` 仅 macOS 可用（其他平台明确报错）；headless `--approval
      auto/deny` 全平台语义一致
- [ ] unsafe 仅存在于 `appkit.rs` 的 objc2 边界且每处带 `// SAFETY:` 注释
      （`#![deny(clippy::undocumented_unsafe_blocks)]`，对齐 rc-input-macos
      lib.rs:32-33）；state/bridge/approval 模块零 unsafe
- [ ] objc2 依赖只有既有版本线（0.6 / 0.3），无 Tauri/GPUI/serde 等新依赖（U5）
- [ ] TASK.md §0/§12/§13 与 tickets README 状态表已回写（人工验收场景带
      命令与配置）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-input-macos/src/lib.rs`](../../../crates/rc-input-macos/src/lib.rs) | 平台 crate 分层范本：纯逻辑全平台可测 + `cfg(macos)` 平台模块 + unsafe 集中 + `deny(clippy::undocumented_unsafe_blocks)`（lib.rs:14-33）；「预检不静默失败」的构造约定（权限预检 → 本卡的 NoGuiSession 预检同思想） |
| [`rc-render-macos`](../../../crates/rc-render-macos) | objc2-app-kit 接入与版本线注释（Cargo.toml:78-97）；`MainThreadOnly`/`MainThreadMarker` 契约（src/lib.rs:38-39）；`window-bench` feature + `harness=false` + `required-features` + `--run` 旗标的真机测试模式（Cargo.toml:103-110、tests/window_present.rs:1-38）——tray_bench 逐条照抄 |
| [`rc-cli/src/cmd/mod.rs`](../../../crates/rc-cli/src/cmd/mod.rs) | Args 定义风格、`parse_duration` 复用（134-147）、`ValueEnum` 用法照 ProbeArgs/BackendMode |
| [`rc-cli/src/main.rs`](../../../crates/rc-cli/src/main.rs) | exit code 约定：参数错 2（main.rs:21）、业务错 1（main.rs:34）、panic 兜底 1（main.rs:48-55） |
| [T18 卡](T18-cli-host-client-localhost.md) | host 子命令形态与「消费面对齐前置卡」纪律；`--report` 分流约定 |
| [T13 卡](T13-session-proto.md) | `AuthMethod`/`AuthStatus`/`SessionParams` 的确切定义（审批复用，不得改） |
| [`rc-net/src/quic.rs`](../../../crates/rc-net/src/quic.rs) | 模块文档 1-19「端点自驱」线程模型（审批等待期传输层存活的前提）；`accept` 阻塞形态（quic.rs:303） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **NSStatusItem/NSAlert 是 MainThreadOnly**：libtest 测试体永远在工作线程（即使 `--test-threads=1`，rc-render-macos tests/window_present.rs:4-7 实测结论） | AppKit 逻辑只在 `TrayHost::new/run`（主线程限定，`MainThreadMarker` 断言）；真机验证走 `tray-bench` feature + `harness = false`（`required-features` 让默认 `cargo test --workspace` 完全跳过该目标） |
| 2 | **必须设 `ActivationPolicy::Accessory`**：默认 policy 会占 Dock 图标、进程像普通 App 一样抢焦点 | `TrayHost::new` 里 `NSApplication` 初始化后立即设置；这是「托盘常驻」的形态前提 |
| 3 | **accessory 进程默认不抢焦点，NSAlert 会沉底**：弹窗在别的窗口后面，机主看不见 → 30s 白白超时 | 弹窗前 `NSApp.activate`（activateIgnoringOtherApps 语义）；`alert_texts` 已把地址放显眼位置兜底 |
| 4 | **`NSAlert::runModal` 接管事件循环**：模态期间托盘菜单点击不响应 | 文档写明并接受（审批是独占决策，一次一件事）；不要为此引入异步 alert + 窗口层级管理的复杂度 |
| 5 | **quinn `accept` 阻塞式**（quic.rs:303）且审批可能等真人 30s | accept/审批等待全部走 T14 驱动循环的短超时 poll（设计师风险 8 的本卡落点）；rc-net 端点自驱（quic.rs:1-19）保证弹窗等待期 QUIC ACK/keep-alive 在后台线程推进，host 循环不阻塞即传输层不死 |
| 6 | **client 握手超时 vs 审批超时赛跑**：弹窗等人 30s，client 若 10s 就放弃则审批形同虚设 | 本卡显式放宽 client 的 Hello→Welcome 等待为 `approval_timeout + 15s`（U6，对 T14 默认参数的修正点）；`approval_timeout_rejects_by_injected_clock` 钉死 host 侧行为 |
| 7 | **SSH / 无 GUI 会话**：NSStatusItem 创建直接失败 | `TrayHost::new` 预检并返回 `HostUiError::NoGuiSession`（学 rc-input-macos 的「预检不静默失败」，lib.rs:9-13），绝不静默降级成无托盘进程；tray_bench 文档写明需要 GUI 登录会话 |
| 8 | **objc2 双版本编译冲突**：objc2-app-kit 的 feature 名在 0.3 线按类划分（`"NSStatusItem"`/`"NSAlert"` 等，见 rc-render-macos Cargo.toml:81-96 的精确列表纪律） | 版本线严格对齐 workspace 既有（objc2 0.6 / 绑定 0.3）；feature 名以 0.3 docs.rs 为准核对，`default-features = false` |
| 9 | **`deny(unsafe_code)` 开不了**（objc2 绑定必须 unsafe），但纯逻辑模块必须零 unsafe | crate 级 `#![deny(clippy::undocumented_unsafe_blocks)]`（rc-input-macos lib.rs:32-33 同款）；unsafe 全部集中在 `appkit.rs` 且逐块 `// SAFETY:` 论证；state/bridge 模块照常 `deny(unsafe_code)`（模块级属性） |
| 10 | **时间注入边界**：审批超时若在 UI 侧判（NSAlert 计时）则离线不可测、违反铁律 4 | 超时判定只放会话层（T19 状态机：注入时钟 + deadline 比较）；UI 只管「人点了什么」；trait 三方法非阻塞是契约（T19 auth.rs） |
| 11 | **tray 常驻的 accept 循环与单会话兜底**：T18 的 host 是「一次会话」生命周期，`--duration` 到点退出 | tray 分支：`--duration` 默认 0（不兜底），显式传值仍按单会话生效；一次只 accept 一条连接（U8），第二条在传输层 listen 队列排队——这是行为不是 bug，文档写明 |
| 12 | **A4 的「永久」不做≠偷懒**：设备指纹（H3）/白名单（H8）是后续里程碑，本卡弹窗只有「接受/拒绝」 | 不发明「记住此设备」的本地存储（没有指纹体系前，按地址记忆是安全漏洞——地址可伪造）；卡内写死边界防范围蔓延 |
| 13 | **弹窗信息只有传输层一跳**：`remote_addr` 是直连对端地址，NAT 中继后（M5）不等于发起者真实地址 | `alert_texts` 措辞用「来自 <addr> 的连接请求」，不承诺「发起者身份」；E2E/指纹体系属后续 |
| 14 | **rc-cli 变厚**：tray 分支最容易把 accept 循环、状态机写进 cmd/host.rs | U10 硬性约束：CLI 只做参数解析 + 组合（构造 TrayHost、spawn 会话线程、转发 TrayEvent）；accept 循环里「会话驱动」调 rc-session API，UI 动作全在 rc-host-macos |
| 15 | **幕布工具结束判定**：`macos-dev-run.sh cli` 靠摘要行判子进程结束（scripts/macos-dev-run.sh:115-120），tray host 点「退出」前会一直等 | 属预期（卡内验收命令已注明）；CI 集成测试全部 headless（auto/deny），不依赖托盘 |

## 与后续卡的接口

- **M4/M5（E1 开机自启 / E2 服务模式）**：在 tray 进程之上加 launchd/SMAppService，
  `TrayHost` 与 `ConnectionApprover` 不改；服务化后弹窗降级路径（无 GUI 会话
  → `NoGuiSession` 预检已有）届时由配置切换（auto 或远程批准）。
- **H8 白名单 / H3 指纹**：届时在 host 审批前加白名单短路（免弹窗），
  `ConnectionApprover`（T19 auth.rs）之前的一层纯逻辑判定即可，trait 不动。
- **T20 剪贴板 / T21 控制端 UI**：与本卡无接口交叠；T21 的 Tauri App 是
  控制端进程，被控端托盘不进 Tauri。
- **多会话管理**（若未来支持并发会话）：`TrayState::InSession` 单地址形态
  与「一次一条」循环（U8）届时再扩，本卡不留半成品开关。
