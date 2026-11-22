# T19 M2-5 鉴权：密码挑战 + 接受弹窗联动

> 实装 `rc-session` 的 AuthChallenge/AuthResponse（0x0D/0x0E）：共享密码的 PBKDF2 哈希挑战应答、Hello 鉴权位全决策表（含拒绝路径）、以及 M2-7 接受弹窗的联动位（`ConnectionApprover`——**全项目唯一**审批抽象，T22 只实装弹窗版、不另立 trait）。

## 背景（为什么做这个）

M2 双机直连后，被控端端口直接暴露在局域网——没有准入控制，任何同网段进程都能完成 Hello/Welcome 握手然后注入键鼠、拉取画面，远控产品不可发布。M2-5 用「共享密码 + 挑战应答」做第一道闸：密码明文与密码哈希**绝不上线**，client 对 host 发来的一次性盐计算 PBKDF2-HMAC-SHA256 证明，host 同法校验——录下整条会话重放，旧 proof 换了盐必失败。

信道加密与 host 身份已由 QUIC TLS 1.3 + 固定信任证书解决（rc-net `connect` 即此形态，quic.rs:253-300），本卡不做 E2E 加密、不做证书体系（M5）。与 M2-7 的联动点在同一握手窗口：密码校验通过后 host 侧还要过一道「人工确认」（托盘接受弹窗，T22 实装）——本卡交付**全项目唯一**的审批抽象 `ConnectionApprover`（`submit`/`poll_verdict`/`cancel` 三方法全部非阻塞）与内置替身（`AutoApprover`/`DenyApprover`/`ScriptedApprover`），T22 消费它实装弹窗版、不再另立 approval 模块或第二套 trait；弹窗本身不做。

领取前提：T13（proto.rs 线格式，0x0D/0x0E 已占号拒收、`AuthMethod`/`AuthStatus` 枚举已备）与 T14（HostSession/ClientSession 状态机与控制消息分发）已合并。本卡把鉴权决策收敛成 **transport 无关的纯状态机**（`HostAuthenticator`/`ClientAuthenticator`），T14 的会话层只翻译其动作为线上消息——决策表因此可以不碰任何 IO 全覆盖测试。

## 平台要求

**全平台，纯逻辑。** 无真机、无权限、无 GUI。全部测试离线确定性（时间注入，无 `#[ignore]` 项）；PBKDF2 向量测试为公开标准值，非自造。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/auth.rs` | 新建 | 手写 SHA-256 / HMAC / PBKDF2-HMAC-SHA256（零第三方加密依赖）+ `compute_proof`/`verify_proof` 纯函数 + `HostAuthenticator`/`ClientAuthenticator` 纯状态机 + 唯一审批抽象 `ConnectionApprover`（`ApprovalRequest`/`ApprovalVerdict` + `AutoApprover`/`DenyApprover`/`ScriptedApprover`，T22 消费不另立）+ 内联单测 |
| `crates/rc-session/src/proto.rs` | 修改 | 增量添加 `AuthChallenge`/`AuthResponse` 变体与编解码（号位 0x0D/0x0E 已冻结，只加不改）；同步更新 T13 的相关测试（见已知坑 1） |
| `crates/rc-session/src/lib.rs` | 修改 | `pub mod auth;` + 模块地图补一行 |
| `crates/rc-session/src/host_session.rs` | 修改（T14 交付物） | 构造配置新增 `auth: HostAuthConfig` 与 `approver: Box<dyn ConnectionApprover>` 字段（默认 `AutoApprover`——这是**唯一**一道审批轮询挂点，T22 不再增设）；Hello/AuthResponse 分发接入 `HostAuthenticator`（`on_hello` 带 accept 得到的对端地址，供 `ApprovalRequest` 组装）；`Rejected` 终局发送后立即断开；等待期 client 断开时调 `approver.cancel()`；驱动循环对未终局鉴权每拍 `poll_approval` |
| `crates/rc-session/src/client_session.rs` | 修改（T14 交付物） | 构造配置新增 `auth: ClientAuthConfig` 字段；状态机插入 `Authenticating` 子态（Init ⇄ 终局 Welcome/超时）；Hello 鉴权位来自 `ClientAuthenticator` |
| `crates/rc-session/tests/auth_handshake.rs` | 新建 | 进程内互喂集成测试：两台纯状态机直接对话，零 IO 覆盖完整握手决策表 |
| `crates/rc-cli/src/cmd/`（host/client 子命令） | 修改（**条件**） | 追加 `--password <PWD>` 可选参数（host 出现即启用密码鉴权，client 出现即以密码方式握手）。**若领取时 T18 未合并，本行整体跳过、改由 T22 挂接，不算未完成**；验收不依赖它 |

## API 契约

> 跨卡共用语义以此为准（与 T13 冻结线格式一致，不得自创分歧）。`20-crate-contracts.md` 尚无 rc-session 节，本节与 T13 卡共同构成该 crate 契约。

### proto.rs 增量（T13 既有 15 个常量与 12 个变体一字不动）

```rust
// crates/rc-session/src/proto.rs —— 本卡新增部分
/// AuthChallenge 盐长度（字节）。固定长度，线格式无长度前缀。
pub const AUTH_SALT_LEN: usize = 16;
/// AuthResponse 证明长度（字节）= PBKDF2-HMAC-SHA256 输出长度。
pub const AUTH_PROOF_LEN: usize = 32;
/// AuthChallenge.iterations 的线格式上限。decode 拒收 0 与超限值
/// （对齐 BwProbeRequest.total_bytes 的防灌边界纪律：恶意 host
/// 不得借荒谬迭代数烧 client CPU）。
pub const MAX_AUTH_ITERATIONS: u32 = 1_000_000;

/// 0x0D AuthChallenge（host→client，M2-5 / T19）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    /// PBKDF2 盐：host 每次握手随机生成、一次性使用——重放旧 proof 必失败。
    pub salt: [u8; AUTH_SALT_LEN],
    /// PBKDF2 迭代次数（decode 校验 1..=MAX_AUTH_ITERATIONS）。
    pub iterations: u32,
}

/// 0x0E AuthResponse（client→host，M2-5 / T19）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResponse {
    /// 证明 = PBKDF2-HMAC-SHA256(password_utf8, salt, iterations)。
    pub proof: [u8; AUTH_PROOF_LEN],
}

// ControlMessage 枚举追加两个变体（走 reliable 控制流，不走 urgent，
// 也不走 datagram——BwProbeData 是唯一例外，那是 T15 的事）：
pub enum ControlMessage {
    // …T13 既有 12 个变体不动…
    /// host→client：密码挑战（M2-5 / T19）。
    AuthChallenge(AuthChallenge),
    /// client→host：挑战应答（M2-5 / T19）。
    AuthResponse(AuthResponse),
}
```

**线格式增量表**（整数一律 LE，定长字段无长度前缀，对齐 IdrAck 风格）：

| 类型 | 消息 | 布局（type 之后） | 定长 |
|---|---|---|---|
| 0x0D | AuthChallenge | `salt u8[16]` `iterations u32` | 21 |
| 0x0E | AuthResponse | `proof u8[32]` | 33 |

Hello/Welcome 的鉴权字段沿用 T13 冻结布局：Hello 的 `auth_method u8 + auth_len u16 + auth_payload`；Welcome 的 `auth_status u8`。**Password 方式下 `auth_payload` 恒空**（密码绝不进 Hello，host 见非空即拒——语义校验在会话层，不塞回 proto，见已知坑 9）。

### auth.rs（全量新文件）

```rust
// crates/rc-session/src/auth.rs
//! M2-5 鉴权：手写 SHA-256 / HMAC-SHA256 / PBKDF2-HMAC-SHA256、密码挑战
//! 应答纯状态机、与 M2-7 接受弹窗的轮询联动位。
//!
//! 为什么手写哈希而不是引 ring/rustls/sha2：本项目纪律「纯 Rust 不引
//! 重型依赖」，为单个哈希函数拉进 TLS 栈不值得；SHA-256 核心约百行，
//! 以 FIPS 180-4 / RFC 4231 / 公开 PBKDF2 向量逐字节钉死正确性。

/// 默认 PBKDF2 迭代次数（写入 AuthChallenge）。release 单次计算毫秒级，
/// 握手一次性成本；防离线字典攻击的最低强度线。
pub const DEFAULT_AUTH_ITERATIONS: u32 = 10_000;
/// 默认鉴权全流程超时（微秒）：client 从发出 Hello 到终局 Welcome 的
/// 窗口，须覆盖 host 侧人工确认的最长等待。
pub const DEFAULT_AUTH_TIMEOUT_US: u64 = 30_000_000;

/// client 侧证明计算（纯函数、确定性——测试可固定盐逐字节断言）。
pub fn compute_proof(
    password: &[u8],
    salt: &[u8; AUTH_SALT_LEN],
    iterations: u32,
) -> [u8; AUTH_PROOF_LEN];

/// host 侧校验：同参重算 + **常数时间比较**（不泄比较结果的时间信息）。
pub fn verify_proof(
    password: &[u8],
    salt: &[u8; AUTH_SALT_LEN],
    iterations: u32,
    proof: &[u8; AUTH_PROOF_LEN],
) -> bool;

/// host 侧鉴权配置。
#[derive(Debug, Clone)]
pub struct HostAuthConfig {
    /// 共享密码。None = 不启用密码鉴权（此时 Hello.auth_method≠0 一律拒绝）。
    pub password: Option<String>,
    /// 挑战迭代次数（进 AuthChallenge；测试可用小值如 64 加速状态机用例）。
    pub iterations: u32,
    /// 人工确认超时（微秒，会话钟）；超时按拒绝处理。
    pub approval_timeout_us: u64,
}
/// 默认值：无密码 / DEFAULT_AUTH_ITERATIONS / DEFAULT_AUTH_TIMEOUT_US。

/// client 侧鉴权配置。
#[derive(Debug, Clone)]
pub struct ClientAuthConfig {
    /// 共享密码。None = 以 AuthMethod::None 握手（T14 原行为，不变）。
    pub password: Option<String>,
    /// 鉴权全流程超时（微秒，会话钟）。
    pub auth_timeout_us: u64,
}

/// 一次连接审批请求：审批 UI 需要展示的全部信息（T22 弹窗文案的唯一
/// 信息源）。由 `HostAuthenticator` 在首次进入等待确认时组装，经
/// [`ConnectionApprover::submit`] 递给实现方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// 回显 Hello.session_id。
    pub session_id: u64,
    /// 对端传输层地址（QuicEndpoint accept 得到，非 client 自报；经
    /// `on_hello` 的 `remote_addr` 参数进入状态机）。
    pub remote_addr: std::net::SocketAddr,
    /// Hello 请求参数（弹窗展示「请求 1920x1080@60」）。
    pub requested: SessionParams,
    /// Hello 声明的鉴权方式。
    pub auth_method: AuthMethod,
    /// 密码校验是否已通过（未启用密码 = false）。先鉴权后审批：密码
    /// 失败根本不会走到审批，能到达 submit 的只有「已通过」或「无密码
    /// 要求」两种（T22 弹窗据此显示「密码已验证」/「未启用密码」）。
    pub password_verified: bool,
}

/// 审批裁决（M2-7 接受弹窗的应答值）。超时**不**在此枚举——无人应答
/// 的超时由 `HostAuthenticator::poll_approval` 按注入时钟判成
/// `Rejected`，实现方不感知超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// 接受本次连接
    Accepted,
    /// 拒绝（含弹窗点「拒绝」）
    Rejected,
}

/// 连接审批人——M2-7 接受弹窗的**联动位**（全项目唯一审批抽象，
/// T22 实装托盘弹窗版，本卡交付 trait 与内置替身）。
///
/// 为什么三方法全部非阻塞而不是回调/阻塞：quinn 的 accept/connect
/// 本就是阻塞式（quic.rs:253/303），会话驱动循环靠短超时 poll 推进——
/// 审批人若在循环里阻塞等人，心跳、采集、编码全部停摆。实现方（T22
/// 的托盘弹窗、测试替身）在独立线程更新共享状态，会话循环每拍来问
/// 一次；超时判定收敛在会话层（`HostAuthenticator::poll_approval` 的
/// 注入 `now_us`），trait 方法不携带时间（铁律 4）。
pub trait ConnectionApprover: Send {
    /// 提交一次审批请求（弹窗出现）。同一时刻至多一个在途请求；
    /// 在途期间再次 submit 属实现方 bug，实现可 panic 于 debug、
    /// 覆盖于 release。
    fn submit(&mut self, req: &ApprovalRequest);
    /// 非阻塞取裁决：None = 仍在等待（弹窗未答 / 无在途请求）；
    /// Some = 终局。
    fn poll_verdict(&mut self) -> Option<ApprovalVerdict>;
    /// 撤销在途审批（client 在等待期断开 / 会话被终止）；弹窗应随之
    /// 关闭。无在途请求时为幂等 no-op。
    fn cancel(&mut self);
}

/// 即时接受的审批人：无头 CLI 默认 / 测试基线（submit 即 Accepted，
/// poll_verdict 取走后清空，cancel no-op）。
pub struct AutoApprover;

/// 一律拒绝的审批人：远程运维锁主机 / CI 验证拒绝路径（T22 的
/// `--approval deny` 消费）。
pub struct DenyApprover;

/// 脚本化审批人：按给定序列回放（None=等待，Some=终局）；序列耗尽后
/// 恒返回末项，测试不会悬死。T22 落地弹窗时的行为参考样板。
pub struct ScriptedApprover { /* 私有 */ }
impl ScriptedApprover {
    pub fn new(responses: Vec<Option<ApprovalVerdict>>) -> Self;
}

/// host 侧鉴权纯状态机（transport 无关；HostSession 消费其动作，
/// 不自己发明任何鉴权分支）。
pub struct HostAuthenticator { /* 私有 */ }

/// host 侧动作（由 HostSession 翻译成线上消息与断开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAuthAction {
    /// 发 Welcome{auth_status=ChallengeRequired}（过程消息：granted 填
    /// 预裁定真值但 client 必须忽略）。仅在「确认器首次返回等待」且本次
    /// 握手尚未发过过程消息时出现一次——密码路径 Challenge 已自带过程
    /// 消息，不会重复出现。
    NotifyPending,
    /// 依次发 Welcome{ChallengeRequired} + AuthChallenge（顺序固定）。
    Challenge(AuthChallenge),
    /// 终局：发 Welcome{auth_status}；Rejected 在发送后**立即断开**传输层。
    Conclude(AuthStatus),
    /// 本拍无事（等待 AuthResponse / 人工确认）。
    Wait,
}

impl HostAuthenticator {
    pub fn new(cfg: HostAuthConfig) -> Self;

    /// 收到 Hello 的裁定（决策表唯一入口；盐在本方法内以 OS 熵随机生成）。
    /// `remote_addr` 为 accept 得到的对端传输层地址（进入 `ApprovalRequest`
    /// 的唯一途径——弹窗不得展示 client 自报地址）。
    pub fn on_hello(&mut self, hello: &Hello, remote_addr: std::net::SocketAddr)
        -> HostAuthAction;

    /// 收到 AuthResponse 的裁定（只应出现在 Challenge 之后；否则视为
    /// 协议违规并 Conclude(Rejected)——宁拒不猜）。
    pub fn on_auth_response(&mut self, response: &AuthResponse) -> HostAuthAction;

    /// 每拍轮询人工确认（会话循环对未终局鉴权恒调；now_us 注入驱动
    /// approval_timeout_us 超时判定，铁律 4）。首次进入等待确认时向
    /// approver `submit` 一条 `ApprovalRequest`（先鉴权后审批：密码
    /// 失败的路径不会走到 submit），此后每拍 `poll_verdict`，裁决或
    /// 超时出终局；client 等待期断开时由会话层调 `approver.cancel()`。
    pub fn poll_approval(
        &mut self,
        now_us: u64,
        approver: &mut dyn ConnectionApprover,
    ) -> HostAuthAction;

    /// 鉴权是否已终局（终局后会话层停止调用上面三个方法）。
    pub fn concluded(&self) -> bool;
}

/// client 侧鉴权纯状态机（transport 无关）。
pub struct ClientAuthenticator { /* 私有 */ }

/// client 侧动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAuthAction {
    /// 终局成功（终局 Welcome{Ok}）：进入 Connected。
    Done,
    /// 发送 AuthResponse。
    Respond(AuthResponse),
    /// 过程通知（收到 Welcome{ChallengeRequired}）：等待挑战或终局，不发消息。
    Waiting,
    /// 终局失败：断开。字符串为本端呈现原因（stderr/日志用，**不上线**）。
    Failed(String),
}

impl ClientAuthenticator {
    pub fn new(cfg: ClientAuthConfig) -> Self;

    /// 构造 Hello 的鉴权位：Some → (AuthMethod::Password, 空 payload)；
    /// None → (AuthMethod::None, 空 payload)。**密码绝不进 Hello。**
    pub fn hello_auth(&self) -> (AuthMethod, Vec<u8>);

    /// 记录 Hello 发出时刻（auth 超时窗口起点，会话钟注入）。
    pub fn on_hello_sent(&mut self, now_us: u64);

    /// 收到 Welcome 的处置：Ok→Done（仅握手期）、ChallengeRequired→Waiting
    /// （进入 Authenticating，忽略 granted）、Rejected→Failed。
    pub fn on_welcome(&mut self, welcome: &Welcome) -> ClientAuthAction;

    /// 收到 AuthChallenge：仅在 Waiting 态接受，否则 Failed（协议违规）；
    /// 接受则同步计算 proof 并返回 Respond。
    pub fn on_challenge(&mut self, challenge: &AuthChallenge) -> ClientAuthAction;

    /// 超时复核：true = 已越 auth_timeout_us 窗口，应断开。
    /// 终局（Done/Failed）后恒 false。
    pub fn poll_timeout(&mut self, now_us: u64) -> bool;
}
```

### host 决策表（`on_hello` 的全部行，测试逐行钉死）

| 行 | host 密码 | client auth_method | auth_payload | 动作 |
|---|---|---|---|---|
| 1 | 无 | None | 空 | `Wait`（进入等待确认；AutoApprover 首拍即 `Conclude(Ok)`，**全程无过程消息**——T14 线上行为不变） |
| 2 | 无 | Password | 任意 | `Conclude(Rejected)` + 断开（不支持该方式——这就是「Hello.auth_method≠0 拒绝路径」） |
| 3 | 有 | None | 空 | `Conclude(Rejected)` + 断开（host 要求凭据，client 拒给） |
| 4 | 有 | Password | 非空 | `Conclude(Rejected)` + 断开（载荷必须为空——密码绝不进 Hello） |
| 5 | 有 | Password | 空 | `Challenge(ch)`（ch 盐随机、iterations 取配置）→ 等 `AuthResponse` → proof 对 → 等确认器 → `Conclude(Ok)`；proof 错 / 确认拒绝 / 确认超时 → `Conclude(Rejected)` + 断开 |

> T14 原有的「auth_method≠0 一刀切拒绝」被本决策表取代：行 2 保留同形拒绝（host 无密码时），行 3-5 是新增的密码路径。

### 握手时序（密码正确路径）

```
client                                host（密码 P，确认器）
  │ Hello{Password, payload 空}          │
  │─────────────────────────────────────>│
  │  Welcome{auth_status=ChallengeRequired}   ← 过程消息（granted 预填，client 忽略）
  │<─────────────────────────────────────│
  │ AuthChallenge{salt, iters}           │
  │<─────────────────────────────────────│
  │ AuthResponse{proof}                  │   proof = PBKDF2-HMAC-SHA256(P, salt, iters)
  │─────────────────────────────────────>│
  │            （host 重算校验 + ConnectionApprover 轮询——人工确认窗口）
  │ Welcome{auth_status=Ok, granted 真值} │
  │<─────────────────────────────────────│   → 双方 Connected → Streaming
```

密码错 / 弹窗拒绝 / 确认超时：在应答终局处改为 `Welcome{auth_status=Rejected}`，随后 host 断开传输层，client 终态 `Failed`。无密码路径（行 1 + AutoApprover）：线上恰一条 `Welcome{Ok}`，与 T14 逐字节同形。

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| A1 | 密码明文与密码哈希一律不上线：Hello 的 Password 方式 `auth_payload` 恒空（host 见非空即行 4 拒绝）；proof 只出现在 AuthResponse | 线上抓包即得密码/密码等效值 |
| A2 | 每次握手新随机盐（OS 熵 16 字节，一次性）；proof 绑定盐——重放上一连接的 AuthResponse 必失败。**T17 重连（ReconnectRequest resume）后的重新握手同样走全新盐** | 会话录制重放 = 免密码控制被控端 |
| A3 | proof = PBKDF2-HMAC-SHA256(password, salt, iterations)，SHA-256/HMAC/PBKDF2 全部手写、零第三方加密依赖；以 FIPS 180-4（SHA-256）、RFC 4231（HMAC）、公开 PBKDF2 向量逐字节钉死 | 为一个哈希拉进 ring/rustls；或自造哈希自证正确性 |
| A4 | `AuthChallenge.iterations` decode 校验 `1..=MAX_AUTH_ITERATIONS`（1_000_000），0 与超限报 `Error::Protocol`（带 offset 与出错值） | 恶意 host 指定 1e9 迭代烧 client CPU |
| A5 | 一切失败（方式不支持 / 密码错 / 弹窗拒 / 确认超时）对 client 只有 `Welcome{Rejected}` + 断开，**无原因文本上线**；区分信息只进本端 stderr（进度/摘要分流约定照 rc-cli cmd/loopback.rs:1-8） | 拒绝原因文本 = 给攻击者的免费探测信息 |
| A6 | 鉴权未终局前双方管线零启动：host 不采集不编码、client 不解码不呈现 | 未授权先泄画面/白烧编码器 |
| A7 | 时间全注入：approval 超时、client auth 超时一律以 `now_us` 参数进（铁律 4），状态机不读系统钟；PBKDF2 计算是纯 CPU，不涉钟 | 不可测 + 违反铁律 4 |
| A8 | `ConnectionApprover` 三方法（submit/poll_verdict/cancel）全部非阻塞立即返回；审批等待不得阻塞会话驱动循环（quinn accept/connect 已是阻塞式，quic.rs:253/303——驱动循环只剩短超时 poll 可用）；超时判定只在本卡状态机（注入 `now_us`），trait 实现方不判超时 | 弹窗等人期间心跳/采集/编码全停 |
| A9 | 号位与 T13 布局零改动：只新增 0x0D/0x0E 变体；T13 的 `decode_rejects_reserved_types`、`roundtrip_all_variants` 等既有测试随行为变更**同步更新**（0x0F 仍拒收、其余未知号仍拒收） | 忘改 T13 测试 = 假绿或无谓红 |
| A10 | `verify_proof` 用常数时间比较（无「先比首字节」短路）；host 侧对错密码的计算量一致 | 时序侧信道泄密码信息 |
| A11 | 鉴权消息（AuthChallenge/AuthResponse/Welcome）仅握手期有效：Streaming 期到达一律按致命 `Error::Protocol` 断开（`is_fatal()==true`，rc-types error.rs:68-75） | 协议倒退/中段注入 |

## 不要做什么

- ❌ 不做证书体系/对端身份认证（M5）：host 身份仍靠 QUIC TLS + 固定信任证书（quic.rs:253-300 既有形态）。
- ❌ 不做 E2E 加密：QUIC TLS 1.3 已加密信道，不在应用层再叠一层。
- ❌ 不做用户账户/多密码/权限分级：只有单一共享密码。
- ❌ 不做密码存储（Keychain/密码文件/哈希落盘）：host 密码来自 CLI 参数或环境变量；Keychain 归 M4 服务化。
- ❌ 不做 SRP/OPAQUE 等不泄验证器的 PAKE：恶意 host 能对抓到的 proof 做离线字典攻击，这是 M2 文档化接受的边界（QUIC TLS 已挡中间人；M5 后再评估升级）。
- ❌ 不做失败计数锁定/尝试限频（host 侧抗爆破属 M4 服务化）。
- ❌ 不做真弹窗/托盘/GUI（T22）：本卡只交付 `ConnectionApprover` trait + 内置替身（`AutoApprover`/`DenyApprover`/`ScriptedApprover`）。
- ❌ 不动 `rc-types`（铁律 5）、不把控制消息塞进 `rc-proto`；不引 ring/rustls/sha2/subtle/zeroize 等任何加密类依赖（哈希手写、常数比较手写、密码就放 `String`——零依赖优先，内存擦除不做并在文档注明边界）。
- ❌ 不改 T13 冻结的 15 个 `TYPE_*` 常量与既有 12 个变体的线格式。

## 测试清单

全部离线确定性（时间/盐均可控或与断言无关），无 `#[ignore]` 项。**状态机类测试用 `iterations: 64` 小值加速；10_000 全量只跑纯函数向量**（debug 构建 opt-level=0，见已知坑 4）。

**proto.rs 内联（更新 T13 既有 + 新增）：**

| 测试名 | 断言要点 |
|---|---|
| `auth_challenge_roundtrip_and_wire_layout` | 21 字节定长 `[0x0D][salt 16B][iters LE]`；roundtrip 相等 |
| `auth_response_roundtrip_and_wire_layout` | 33 字节定长 `[0x0E][proof 32B]`；roundtrip 相等 |
| `auth_challenge_rejects_zero_iterations` | iterations=0 → `Protocol`（带 offset） |
| `auth_challenge_iteration_bounds` | 恰 `MAX_AUTH_ITERATIONS` 可解；+1 拒收且错误文本含上限值 |
| `auth_messages_truncation_and_trailing` | 两变体合法编码的每个严格前缀拒收（21/33 字节穷举）；+1 尾字节拒收 |
| `decode_rejects_reserved_types`（更新） | 只剩 0x0F 拒收（含 "reserved" 与 M2-8 归属）；0x0D/0x0E 移出并注释「T19 已实装」 |
| `roundtrip_all_variants`（更新） | 12 → 14 变体全量往返 |

**auth.rs 内联（纯函数 + 状态机）：**

| 测试名 | 断言要点 |
|---|---|
| `sha256_fips_vectors` | `""`、`"abc"`、448bit 长消息（FIPS 180-4）逐字节匹配 |
| `hmac_sha256_rfc4231_vectors` | case 1（key=0x0b×20, "Hi There"）、case 2（"Jefe", "what do ya want for nothing?"）逐字节匹配 |
| `pbkdf2_hmac_sha256_vectors` | ("password","salt",c=1)→`120fb6cf…70be17b`、(c=2)→`ae4d0c95…74c43`、(c=4096)→`c5e478d5…98134a`（dkLen=32 标准公开向量） |
| `compute_proof_deterministic_salt_sensitive` | 同参两次输出相等；盐翻 1 bit → proof 全变 |
| `verify_proof_rejects_single_bit_flip` | proof 改 1 字节 → false；正确 → true |
| `host_decision_table_full_matrix` | 决策表 5 行逐一断言动作（行 2/3/4 均 `Conclude(Rejected)` 且 `concluded()`） |
| `challenge_flow_correct_password_auto_approver` | 行 5：`Challenge` → 对 proof `on_auth_response` → `poll_approval` 首拍即 `Conclude(Ok)`；全程无 `NotifyPending`（过程消息已由 Challenge 承担） |
| `challenge_flow_wrong_password_rejected` | 错 proof → `Conclude(Rejected)` |
| `deny_approver_rejects_immediately` | `DenyApprover`：submit 后 `poll_verdict` 即 `Some(Rejected)`（不 panic）；`cancel` 幂等 no-op（T22 `--approval deny` 的行为基线） |
| `approver_request_carries_hello_and_addr` | submit 递出的 `ApprovalRequest`：session_id/requested/auth_method 与 Hello 一致、`remote_addr` 为 `on_hello` 传入值、行 5 下 `password_verified == true`（ScriptedApprover 换可观察替身记录，T22 弹窗文案依赖这些字段） |
| `no_password_immediate_accept_no_process_welcome` | 行 1 + Auto：`on_hello`→`Wait`，首拍 poll→`Conclude(Ok)`，**全程无 NotifyPending**（T14 线上行为不变的钉子） |
| `no_password_pending_approver_notifies_then_concludes` | Scripted `[None,None,Accepted]`：首拍 `NotifyPending` → `Wait` → `Conclude(Ok)` |
| `approval_timeout_rejects` | 注入 now_us 越过 `approval_timeout_us` → `Conclude(Rejected)`（时间注入，不 sleep） |
| `auth_response_without_challenge_is_rejected` | 未 Challenge 先 `on_auth_response` → `Conclude(Rejected)`（宁拒不猜） |
| `fresh_salt_per_handshake` | 同一 authenticator 两次行 5 握手，盐不同（统计性断言 16 字节非全等） |
| `client_hello_auth_never_carries_password` | Some/None 两分支 payload 皆空、method 正确 |
| `client_welcome_lifecycle` | Ok→Done；ChallengeRequired→Waiting；Rejected→Failed（含原因字符串） |
| `client_challenge_requires_process_welcome_first` | 未 Waiting 先 `on_challenge` → Failed（协议违规文本） |
| `client_challenge_computes_expected_proof` | 固定盐/迭代 → `Respond` 的 proof == `compute_proof` 同参输出（交叉验证） |
| `client_timeout_window` | `on_hello_sent(t0)`：窗口−1µs 为 false、+1µs 为 true；Done 后恒 false |

**tests/auth_handshake.rs（进程内互喂，两台纯状态机直接对话，零 IO）：**

| 测试名 | 断言要点 |
|---|---|
| `inproc_password_handshake_completes` | 完整消息序列恰与握手时序图一致：Hello → [过程 Welcome + AuthChallenge] → Respond → Conclude(Ok) → Done |
| `inproc_wrong_password_rejected` | host `Conclude(Rejected)`、client `Failed`；且无任何媒体/输入消息产生（A6） |
| `inproc_method_mismatch_both_directions_rejected` | 决策表行 2 与行 3 各跑一遍 |
| `inproc_replayed_proof_fails_on_second_connection` | 第二次握手新盐，重放旧 proof → Rejected（A2） |
| `inproc_no_password_matches_t14_shape` | 无密码 + Auto：host 侧对外消息恰 `[Welcome{Ok}]` 一条（T14 兼容不变式） |

## 验收命令

```bash
cargo check -p rc-session
cargo test -p rc-session
cargo clippy -p rc-session --all-targets -- -D warnings
cargo test --workspace
```

（内循环只跑前三条——rc-session 依赖扇出大（设计师 risk 9）；`--workspace` 合并前跑一次。）

可选复核（**仅当 T18 已合并**，localhost 双进程冒烟，macOS 用统一入口）：

```bash
# 终端 1：host 启用密码
./scripts/macos-dev-run.sh cli --release -- host --listen 127.0.0.1:9901 \
    --backend fake --resolution 1920x1080 --fps 60 --duration 30s \
    --inject-mode record --password hunter2 --report /tmp/t19-host.json
# 终端 2：正确密码应完成握手；换 --password wrong 应被拒并退出非 0
./scripts/macos-dev-run.sh cli --release -- client --connect 127.0.0.1:9901 \
    --backend fake --duration 30s --input-mode scripted \
    --password hunter2 --report /tmp/t19-client.json
```

## 验收标准

- [ ] `cargo test -p rc-session` 全绿（本卡新增 ≥ 20 个测试）
- [ ] `cargo test --workspace` 全绿（T13 的 proto 测试更新后仍全绿；0x0F 仍拒收）
- [ ] `cargo clippy -p rc-session --all-targets -- -D warnings` 干净
- [ ] SHA-256 / HMAC / PBKDF2 公开向量逐字节匹配（不允许只做自定义向量自证）
- [ ] 决策表 5 行 + 确认三场景（即时接受 / 挂起后接受 / 超时拒绝）全部有测试钉死
- [ ] 无密码 + AutoApprover 路径 host 对外消息与 T14 完全同形（仅一条 Welcome{Ok}）
- [ ] `Cargo.toml`：相对 T14 基线只新增 `rand = "0.8"`；无 ring/rustls/sha2 等加密依赖
- [ ] 无 `unsafe`；`#![deny(missing_docs)]` 保持，pub 项（含字段）中文文档齐全
- [ ] （条件项）若 T18 已合并且挂了 `--password`：localhost 正/错密码冒烟行为与退出码正确；未合并则本条跳过不判失败

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`crates/rc-input/src/lib.rs`](../../../crates/rc-input/src/lib.rs) | 手写线格式全套纪律：encode 追加式（lib.rs:156 一带）、decode 带偏移报错（lib.rs:222 一带）、未知类型拒收（lib.rs:319）、布尔严格 0/1（lib.rs:324）——新增两变体沿用 |
| [`crates/rc-net/src/quic.rs`](../../../crates/rc-net/src/quic.rs) | TLS 边界为什么够用：固定信任证书连接（quic.rs:253-300）、自签 `Identity`（quic.rs:148-183）；阻塞式 accept/connect（quic.rs:253/303）是 A8 轮询式设计的由来；reliable 消息上限 `MAX_RELIABLE_MSG`（quic.rs:102） |
| [`crates/rc-net/src/lib.rs`](../../../crates/rc-net/src/lib.rs) | `Transport`/`NetEvent` 通道语义（lib.rs:86-136）：鉴权消息走 `Reliable` 控制流，绝不进 urgent/datagram |
| [T13 卡](T13-session-proto.md) 交付的 `proto.rs` | 本卡直接扩展的文件；「占号拒收」纪律与线格式总表的出处（0x0D/0x0E 注释已写明归本卡） |
| [`docs/reference/impl/20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.3/§8 | 传输契约边界 + 新模块检查清单（时间注入、常数比较、边界测试） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **T13 的测试会被本卡有意打破**：`decode_rejects_reserved_types` 断言 0x0D/0x0E 拒收、`roundtrip_all_variants` 数 12 个变体——本卡实装后必须同步更新（保留 0x0F 拒收与其他未知号断言） | 改 proto.rs 的同一次提交里改测试；漏改就是假绿/红，不是「测试不稳定」 |
| 2 | 双层长度前缀（T13 坑 1）延续：rc-net reliable 流已有 u32 前缀成帧，本卡两变体**定长无前缀**，decode 输入 = 一条完整消息 | 别给 salt/proof 加 u16 前缀——定长字段不需要，加了反而多一处错位源 |
| 3 | 确认器阻塞 = 会话全停：quinn accept/connect 阻塞式（quic.rs:253/303），驱动循环只剩短超时 poll；T22 落地弹窗时若在 `poll_verdict` 里 `recv()` 阻塞等 UI，心跳/采集/编码全停（设计师 risk 8 的会话层翻版） | 三方法非阻塞写死在 trait 契约；`ScriptedApprover` 耗尽恒返末项，杜绝测试悬死 |
| 4 | debug 构建 opt-level=0（根 Cargo.toml `[profile.dev]`）：PBKDF2 10k 迭代单次可达百毫秒级——20 个状态机测试各跑两次就是秒级拖累 | 状态机测试 `iterations: 64`；10k/4096 只进纯函数向量测试；别为提速调小 `DEFAULT_AUTH_ITERATIONS`（那是安全参数） |
| 5 | 过程 Welcome 的 granted 是「预裁定值」，client 误用会拿到未夹紧参数 | `ClientAuthenticator::on_welcome` 对 ChallengeRequired **显式忽略 granted**（文档 + `client_welcome_lifecycle` 断言 Waiting 不携带参数） |
| 6 | Welcome{Rejected} 无原因文本（A5），排查全靠本端 stderr | host 侧在 Conclude(Rejected) 时向 stderr 打具体行（密码错/方式不符/弹窗拒/超时），分流约定照 rc-cli cmd/loopback.rs:1-8 |
| 7 | 密码以 `String` 常驻内存、无 zeroize：这是「零依赖优先」的文档化取舍 | auth.rs 模块文档写明边界；M4 服务化若引 Keychain 再一并处理 |
| 8 | 迭代数是双刃：host 侧每次校验同样花 CPU（错密码洪泛 = 变相 DoS） | M2 接受（QUIC 握手本身先挡一层）；限频/锁定明确不做，写进「不要做什么」 |
| 9 | 语义校验（Password 方式 payload 必须为空）放**会话层**，proto decode 只管线格式完整性——这是 T13 定下的分层（「数值参数不做语义校验」） | 别把「auth_len==0 when Password」塞进 proto.rs 的 decode；放 `HostAuthenticator::on_hello`（决策表行 4） |
| 10 | 盐生成是 auth 模块唯一的随机性来源：核心 proof 函数纯确定性，测试不依赖 RNG 注入 | `rand` 只在 `on_hello` 用一次；测试对盐的断言只有「两次不同」（统计性），不预测具体值 |
| 11 | rc-session 依赖扇出大（rc-net/rc-pipeline/rc-input，设计师 risk 9） | 内循环只 `cargo check -p rc-session`；`cargo test --workspace` 收尾一次 |

## 依赖

```toml
[dependencies]
# T14 已挂的 rc-types/rc-net/rc-pipeline/rc-input/rc-trace 不动，本卡只新增：
rand = "0.8"   # 仅挑战盐生成（OS 熵）；与 rc-net/rc-loopback 的 dev-deps 同版本，
               # 复用既有 lock 条目，不引入新依赖家族（禁止混用 0.9）
```

## 与后续卡的接口

- **T22（M2-7 托盘 + 接受弹窗）**：实现弹窗版 `ConnectionApprover`（本卡 auth.rs 定义的全项目唯一审批抽象——`submit` 经桥把 `ApprovalRequest` 递给主线程 NSAlert，机主点击后裁决经 `poll_verdict` 返回，client 断开经 `cancel` 撤弹窗）；审批编排（submit 时机、`approval_timeout_us` 超时、裁决→Welcome 映射）已收敛在本卡 `HostAuthenticator::poll_approval`，T22 **不再增设 approval 模块或第二道审批轮询**，只交付 `TrayApprover`、testkit 的 `MockApprover`（可观察 submit/cancel 调用）与 CLI 参数；若本卡未挂 CLI 参数（T18 时序原因），由 T22 补 `--password` 接线。`AutoApprover`/`DenyApprover`/`ScriptedApprover` 是其行为样板。
- **T17（重连）**：`ReconnectRequest{resume}` 后的重新握手必须走全新 `on_hello` → 新盐；禁止复用旧连接的盐或 proof（A2 对重连路径同样生效）。
- **T20（剪贴板）**：0x0F 仍预留拒收，本卡不动。
- **M5（证书体系）**：密码鉴权与 host 身份正交，挑战机制不变；若需抗恶意 host 离线字典攻击，届时评估升级 PAKE——本卡已把 proof 计算收敛在 `compute_proof` 单点，替换面最小。
