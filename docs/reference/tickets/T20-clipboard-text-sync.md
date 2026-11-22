# T20 剪贴板文本同步（M2-8）

> 把 T13 冻结的 0x0F 占号实装成 `ClipboardNotify`：UTF-8 纯文本双向同步（B4：<200ms），带大小上限（宁不传不截断）、节流合并与回声抑制；macOS 后端走 CarbonCore Pasteboard C API，默认关闭、显式开启。

## 背景（为什么做这个）

剪贴板是远控「复制即达」的核心手感：控制端复制的验证码要贴进被控端，被控端复制的报错要贴回控制端——**双向**，验收标准 **<200ms 同步**（功能表 B4，P0，`docs/reference/design/05-features.md:40`）。通道归属早已定死：控制流 Stream A 承载「会话、鉴权、**剪贴板**、文件传输」（`docs/reference/design/03-protocol.md:20`）。

T13 已发布 `TYPE_CLIPBOARD_NOTIFY = 0x0F` 常量并在 decode 显式拒收（预留占号，T13 卡 53 行与测试 `decode_rejects_reserved_types`）；T18 的 CLI 约定「T19/T20 扩展 proto 消息、届时各自加旗标」。本卡把占号变成实装：proto 变体 + **纯逻辑同步引擎**（回声抑制 / 节流 coalesce / 大小上限，时间全注入，铁律 4）+ macOS 平台后端 + 会话与 CLI 接线 + 报告计数。

只同步 UTF-8 纯文本。剪贴板同时是敏感面：威胁模型明列「剪贴板窃取 → 权限分级、**剪贴板需显式授权**」（`03-protocol.md:187`）——M5 权限分级接管之前，本卡以「默认关闭 + 显式旗标」代行显式授权。

前置：**T14**（HostSession/ClientSession、控制分发、驱动循环）硬依赖；T13 的 proto.rs 与 T18 的 CLI/testkit/报告按同批计划应已合入——若领卡时 T18 未合并，先交付 rc-session 部分，testkit 挂点与 CLI 旗标顺延到 T18 合入后补。

## 平台要求

**核心逻辑（proto + clipboard.rs + 集成测试）：全平台**，离线确定性，无网络、无权限、CI 可跑。
**`--clipboard on` 真后端：仅 macOS**（CarbonCore Pasteboard）。真机测试**无需 TCC 权限**（剪贴板读取不在 TCC 清单），但会读写你当前登录会话的真实剪贴板，故标 `#[ignore]`，知悉后再跑。Windows/Linux 后端不做（trait 已备，D/E 阶段接入）。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/proto.rs` | 修改 | `ClipboardNotify { text }` 变体 + 受限构造器 + 预留拒收名单收窄（0x0F 移出）+ 测试 |
| `crates/rc-session/src/clipboard.rs` | 新建 | `ClipboardBackend` trait、`ClipboardChannel` 引擎、`ClipboardConfig`/`ClipboardAction`/`ClipboardStats`、节流常量 + 内联单测 |
| `crates/rc-session/src/clipboard_macos.rs` | 新建 | `NspasteboardBackend`（cfg gate macOS；rc-session **唯一** unsafe 模块，块块带 `// SAFETY:`） |
| `crates/rc-session/src/lib.rs` | 修改 | 挂载 `clipboard`（全平台）与 `clipboard_macos`（`#[cfg(target_os = "macos")]`） |
| `crates/rc-session/src/host_session.rs` | 修改 | `enable_clipboard` / `pump_clipboard` / `on_clipboard_notify` / `clipboard_stats` + 驱动循环每拍接线（纯增量） |
| `crates/rc-session/src/client_session.rs` | 修改 | 与 host 完全对称的四个方法（双向同步） |
| `crates/rc-session/src/testkit.rs` | 修改 | `FakeClipboard`（可编程假剪贴板）+ `ScriptedClipboard`（CLI scripted 模式） |
| `crates/rc-session/tests/clipboard_session.rs` | 新建 | 进程内双会话集成（自带最小 `LoopTransport`，照 T15 `tests/probe_session.rs` 形态，不依赖 CLI） |
| `crates/rc-cli/src/cmd/mod.rs`、`host.rs`、`client.rs` | 修改 | `--clipboard <off\|on\|scripted>` 旗标（两侧同参）+ 报告 `session.clipboard` 段接线 |
| `crates/rc-cli/tests/host_client.rs` | 修改 | 追加 `clipboard_scripted_sync_over_real_quic` |
| `crates/rc-session/Cargo.toml` | 修改 | macOS 侧加 `core-foundation = "0.10"`（workspace 既有版本线，rc-input-macos 同款） |
| `TASK.md`、`docs/reference/tickets/README.md` | 修改 | §0/§12/§13 回写实测数字；卡片状态表加 T20（AGENTS.md §九） |

## API 契约

> 线格式号 0x0F 由 T13 冻结发布，本卡只做增量实装，**不改 0x01–0x0C 任何字节**；`HostSession`/`ClientSession` 以 T14 实际交付为准做纯增量（命名有出入时改调用处对齐 T14，语义不动）。变长段长度前缀沿用 T13 裁定的 u16 风格（与 Hello.auth_len、BwProbeRequest 同族），因此文本上限天然 = u16::MAX。

```rust
// ── crates/rc-session/src/proto.rs（T13 之上增量）─────────────────

/// 剪贴板文本的最大**字节数**（不是字符数）。与变长段 u16 长度前缀同宽，
/// 线格式上限与引擎上限共用这一个常量（单一事实源）。
/// 要提升上限必须走 PROTOCOL_VERSION 版本协商，不得私自改 u32 前缀。
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = u16::MAX as usize; // 65535

// ControlMessage 新增变体（TYPE_CLIPBOARD_NOTIFY = 0x0F 已由 T13 发布）：
/// 0x0F ClipboardNotify（双向，M2-8）。
/// 线格式：`[0x0F][len u16 LE][text]`——text 是 UTF-8 原始字节，
/// 可含 NUL，长度前缀定界、不做转义；len 按字节计（非字符数）。
/// 空文本合法（len=0，语义：对端剪贴板无文本可粘）。
/// 走 reliable 控制流（Stream A），绝不走 datagram / urgent。
ClipboardNotify {
    /// UTF-8 文本，字节数 ≤ MAX_CLIPBOARD_TEXT_BYTES。
    text: String,
},

impl ControlMessage {
    /// 构造一条剪贴板通知（唯一合法构造路径）。
    /// text 字节数 > MAX_CLIPBOARD_TEXT_BYTES → Err(InvalidConfig)：
    /// 发送侧「宁不传不截断」在构造点就拦住，encode 因此可以保持
    /// T13 契约「不失败、不 panic」（越界值属构造契约违反，
    /// debug_assert 钉开发期；release 下长度字段饱和 u16::MAX 仅为
    /// 防御性处理，按契约不可达）。
    pub fn clipboard_notify(text: String) -> Result<Self>;
}
```

decode 对 0x0F 的校验与 T13 同纪律：截断（每个严格前缀）、尾部多余字节、`len` 与实际字节数不符一律 `Error::Protocol`（带 offset）。**预留拒收名单收窄**：0x0F 从名单移出转为实装变体；0x0D/0x0E 视 T19 是否已合入按 proto.rs 当下实装集合处理（未实装号照旧显式拒收，错误文本含出错字节值）。

```rust
// ── crates/rc-session/src/clipboard.rs（新建）─────────────────────

/// 节流窗口（微秒）：两次外发通知的最小间隔。窗口内的后续变更合并进
/// 唯一挂起槽（最新胜），到期冲刷；**首个变更零延迟立发**（leading
/// edge）——这是 B4「<200ms 同步」的确定性前提：驱动拍 ≤16ms（60fps）
/// + localhost RTT ≈1ms，稳态开销远低于 200ms，窗口只用来压突发。
pub const CLIPBOARD_MIN_NOTIFY_INTERVAL_US: u64 = 100_000;

/// 本机剪贴板后端抽象（host/client 对称使用，与 rc-input 的
/// InputCapture/InputInjector 同定位：trait 在核心 crate，平台实现在旁）。
/// 实现不得读系统时钟（铁律 4）；macOS 无官方剪贴板变更通知 API，
/// 变更检测就用平台计数器轮询（macOS：changeCount）。
pub trait ClipboardBackend: Send {
    /// 平台变更计数。语义：任一进程写入（**含我们自己 write_text**）
    /// 都会递增；单调即可、起点任意。实现读前须自行完成跨进程同步
    /// （macOS：PasteboardSynchronize）。
    fn change_count(&mut self) -> rc_types::Result<u64>;
    /// 读当前文本。返回 None = 无 UTF-8 文本 flavor（图片 / 文件 /
    /// 非 UTF-8），**不是错误**——非文本内容不参与同步（富格式 D5/D6
    /// 属 P2、文件属 M6）。
    fn read_text(&mut self) -> rc_types::Result<Option<String>>;
    /// 覆盖写入文本（清旧内容）。空串的落点由实现定（clear-only 可）。
    fn write_text(&mut self, text: &str) -> rc_types::Result<()>;
}

/// 引擎配置。
pub struct ClipboardConfig {
    /// 文本上限（字节）。默认 proto::MAX_CLIPBOARD_TEXT_BYTES。
    pub max_text_bytes: usize,
    /// 节流窗口。默认 CLIPBOARD_MIN_NOTIFY_INTERVAL_US。
    pub min_notify_interval_us: u64,
}
impl Default for ClipboardConfig;

/// 一拍的动作结论（纯数据，测试断言用）。
pub enum ClipboardAction {
    /// 需经 reliable 控制流发 ClipboardNotify（内容即最新值）。
    SendToPeer(String),
    /// 已把对端文本写入本机剪贴板。
    WrittenLocal(String),
    /// 本拍无动作（抑制原因见 stats）。
    Idle,
}

/// 同步统计（进两侧报告 session.clipboard 段；键名只增不改名）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardStats {
    /// 实际发出的通知数。
    pub sent: u64,
    /// 收到并接受（写入本机）的通知数。
    pub received: u64,
    /// 本地变更 == 自己刚写入的值（回声）。
    pub echo_suppressed: u64,
    /// 窗口内被合并进挂起槽的变更数。
    pub coalesced: u64,
    /// 超上限被**整条丢弃**的变更数（绝不截断）。
    pub oversize_dropped: u64,
    /// 非文本 / 非 UTF-8 变更数。
    pub non_text_dropped: u64,
    /// 被对端后到权威作废的本地挂起值次数。
    pub superseded_local: u64,
    /// 未启用时收到的通知数（计数、不写入、不拆会话）。
    pub received_while_disabled: u64,
}

/// 剪贴板同步引擎（host/client 通用，被会话持有；时间全注入，
/// 不读任何系统时钟——铁律 4）。
pub struct ClipboardChannel {
    /* 私有：cfg、last_count、last_written、pending、last_sent_us、stats */
}

impl ClipboardChannel {
    pub fn new(cfg: ClipboardConfig) -> Self;

    /// 本机侧泵（驱动循环每拍调用，now_us = 本端会话钟 u64）：
    /// 1. change_count 无变化 → 冲刷检查后 Idle；
    /// 2. **首拍记录基线**（连接前的既有内容不外发，见必须做到 CB6）；
    /// 3. 变更 → read_text：
    ///    None → non_text_dropped；
    ///    Some(t) 且 t == last_written → echo_suppressed（自己刚写的）；
    ///    t.len() > max_text_bytes → oversize_dropped（整条丢弃不截断）；
    ///    距上次发送 ≥ 窗口 → 立即 SendToPeer（leading edge）；
    ///    否则 pending = Some(t)（覆盖旧挂起 = coalesce，槽深恒 ≤1）；
    /// 4. 有挂起且窗口已过 → 冲刷 SendToPeer。
    /// 后端错误原样上抛（会话侧记错继续，不拆会话）。
    pub fn pump_local(
        &mut self,
        backend: &mut dyn ClipboardBackend,
        now_us: u64,
    ) -> rc_types::Result<ClipboardAction>;

    /// 对端 ClipboardNotify 的落点（无需时间参数）：
    /// text == last_written → dedup 跳过写入（对端重发 / 失控流量的兜底）；
    /// 否则 write_text + 记 last_written（回声抑制基准）+
    /// **作废 pending**（后到权威覆盖：对端内容刚成为权威，本地挂起的
    /// 旧值若再冲刷出去会用旧值覆盖对端新值）。返回 WrittenLocal / Idle。
    pub fn on_peer_text(
        &mut self,
        backend: &mut dyn ClipboardBackend,
        text: &str,
    ) -> rc_types::Result<ClipboardAction>;

    pub fn stats(&self) -> ClipboardStats;
}
```

```rust
// ── crates/rc-session/src/clipboard_macos.rs（新建，cfg gate macOS）──
//! 本模块是 rc-session 唯一允许 unsafe 的地方：模块头
//! `#![allow(unsafe_code)]`，每个 unsafe 块带 `// SAFETY:` 注释——
//! 完整照抄 rc-input-macos ffi 的纪律（crates/rc-input-macos/src/ffi/mod.rs:19
//! 的模块级 allow 与 lib.rs:37 的 undocumented_unsafe_blocks 检查）。

/// macOS 剪贴板后端：CarbonCore Pasteboard **纯 C API**（无 objc 依赖，
/// 不引 objc2/objc——纯 Rust 不引重型依赖的硬性规则）。
///
/// - 粘贴板名：自建 CFString "com.apple.pasteboard.clipboard"
///   （== kPasteboardClipboard 的值。**不能传 NULL**：NULL 创建的是
///   私有粘贴板，读写永远打到空气）；
/// - 变更计数：读前先 PasteboardSynchronize（跨进程缓存一致性），
///   再 PasteboardGetPasteboardChangeCount（确切 C 签名以本机 SDK
///   Pasteboard.h 为准核对，计数语义单调）；
/// - 读文本：flavor "public.utf8-plain-text"（kUTTypeUTF8PlainText
///   的 UTI 字符串，自建 CFString，不引 UTType 头）→ CFData →
///   `String::from_utf8`，失败或缺 flavor → None（不 Err）；
/// - 写文本：PasteboardClear → PasteboardPutItemFlavor（itemID 用
///   固定 CFString，utf8 flavor，CFData）；空串可只 clear。
/// 所有 OSStatus != 0 → `Error::Io(io::Error::other(...))`（带 osstatus
/// 数值，日志可诊断）。
pub struct NspasteboardBackend { /* 私有：PasteboardRef、缓存的 flavor CFString */ }

impl NspasteboardBackend {
    /// 构造并取得系统粘贴板引用（失败返回 Err，不 panic）。
    pub fn new() -> rc_types::Result<Self>;
}

impl ClipboardBackend for NspasteboardBackend { /* 按上述映射 */ }
```

```rust
// ── host_session.rs / client_session.rs 对称新增（T14 之上纯增量）──
impl HostSession { /* ClientSession 四个方法完全对称（双向同步） */

    /// 启用剪贴板同步（默认禁用：剪贴板是敏感面，须显式开启——
    /// 03-protocol.md:187 要求剪贴板显式授权，M5 权限分级接管前以
    /// 显式旗标代行）。替换式：重复调用换后端并重置基线
    /// （T17 重连恢复后应重新启用——旧纪元的 last_written/基线不可复用）。
    pub fn enable_clipboard(&mut self, backend: Box<dyn ClipboardBackend>);

    /// 剪贴板泵：并入 T14 驱动循环每拍（未启用时空操作、零开销——
    /// 不建后端不碰剪贴板）。SendToPeer → transport.send_reliable
    /// （Stream A；绝不 urgent/datagram）。后端错误按 T14 错误策略
    /// 记错继续，不拆会话。
    pub fn pump_clipboard(&mut self, now_us: u64) -> Result<()>;

    /// 控制分发新增臂（0x0F）：未启用 → received_while_disabled 计数、
    /// 不写本机、不拆会话（对端可能是版本行为差异，不是攻击面击穿——
    /// 反正没后端可写）；启用 → ClipboardChannel::on_peer_text。
    pub fn on_clipboard_notify(&mut self, text: &str) -> Result<()>;

    /// 统计快照（报告 / 诊断；含未启用期间的 received_while_disabled）。
    pub fn clipboard_stats(&self) -> ClipboardStats;
}
```

```rust
// ── testkit.rs 追加（T18 已建模块）────────────────────────────────

/// 可编程假剪贴板（集成测试直接操纵）：确定性、不读时钟。
pub struct FakeClipboard { /* count、text、written */ }

impl FakeClipboard {
    pub fn new() -> Self;
    /// 模拟用户在本机复制文本（bump 变更计数）。
    /// None = 复制了非文本内容（图片/文件：read_text 将返回 None）。
    pub fn simulate_local_copy(&mut self, text: Option<&str>);
    /// 引擎历次 write_text 的记录（断言用）。
    pub fn writes(&self) -> &[String];
}
impl ClipboardBackend for FakeClipboard { /* 计数/读/写全确定性 */ }

/// CLI `--clipboard scripted` 用：**IO 边界墙钟**（定位与 T18 的
/// ScriptedCapture 同款——测量设施允许读墙钟，引擎逻辑不允许）。
/// 构造后 500ms 模拟一次固定文本复制（"rabbitcontrol-clipboard-test"），
/// 之后保持不变——给双子进程 CI 测试提供确定可断言的流量，
/// 全程不碰真实剪贴板。
pub struct ScriptedClipboard { /* 私有 */ }

impl ScriptedClipboard {
    pub fn standard() -> Self;
}
impl ClipboardBackend for ScriptedClipboard { /* change_count/read 按墙钟脚本 */ }
```

```rust
// ── rc-cli/src/cmd/mod.rs 追加（风格照抄 T18 的 InputMode/InjectMode）──

/// `host`/`client` 共用 `--clipboard` 取值。
/// off = 默认（敏感面显式授权原则，03-protocol.md:187）；
/// on = 真后端（仅 macOS，其他平台启动即报 InvalidConfig 退出，
///       不做静默空操作——静默等于用户以为在同步实际没有）；
/// scripted = 测试假后端（ScriptedClipboard，CI 可跑、不碰真实剪贴板）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardMode { Off, On, Scripted }
```

**报告 JSON**（T18 契约「session 段键名只增不改名」）：host 与 client 两侧
session 段各加一个嵌套对象 `clipboard`：

```json
"clipboard": {"enabled": true, "sent": 2, "received": 1, "echo_suppressed": 1,
              "coalesced": 0, "oversize_dropped": 0, "non_text_dropped": 0,
              "superseded_local": 0, "received_while_disabled": 0}
```

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| CB1 | **默认关闭、显式开启**（`--clipboard` 旗标 / `enable_clipboard`）；未启用时收到 0x0F 只计数（received_while_disabled）、不写本机剪贴板、不拆会话 | 剪贴板窃取面失守（03-protocol.md:187）；密码类内容无声外流 |
| CB2 | ClipboardNotify 只走 reliable 控制流（Stream A，03-protocol.md:20）；绝不进 datagram / urgent 通道 | 丢包 = 静默失同步；挤占输入 urgent 流违反 N6 通道分工 |
| CB3 | 文本上限 65535 字节 = u16 长度前缀宽度（`proto::MAX_CLIPBOARD_TEXT_BYTES` 单一事实源）；本地超限**整条丢弃 + 计数，绝不截断**（截断 UTF-8 会劈开多字节字符且静默损坏用户数据） | 大文本灌爆控制流；半截乱码上对端剪贴板 |
| CB4 | 节流：首个变更**当拍立发**（B4 <200ms 的确定性前提），窗口（默认 100ms）内后续变更 coalesce 进**唯一**挂起槽（最新胜，槽深恒 ≤1，对齐铁律 1 的队列纪律精神）；到期冲刷恰一条 | 剪贴板管理器高频写把控制流刷屏；旧值晚于新值到达 |
| CB5 | 回声抑制：`on_peer_text` 写入后记 last_written；本地变更与 last_written 内容相同 → 抑制不发；收到对端通知时**作废本地挂起**（后到权威覆盖） | A↔B 无限回环打满控制流；旧值覆盖对端新值 |
| CB6 | 连接建立时基线吞掉既有内容（引擎首拍只记录 change_count 基线不发送）：**只同步会话期间发生的复制** | 建连瞬间泄露会话前剪贴板（可能正是密码管理器刚写的内容） |
| CB7 | 非文本 / 非 UTF-8 内容不同步（read_text → None），对端保留原文本；入向与 last_written 相同的重复通知 dedup 跳过写入 | 越界做富格式（M6/D5/D6）；乱码上剪贴板；重发洪泛打写 |
| CB8 | 时间注入（铁律 4）：`ClipboardChannel` 一切节流 / 冲刷 / 基线判定只用 now_us 参数；平台后端不读时钟；`ScriptedClipboard` 墙钟仅限 IO 边界（对齐 T18 ScriptedCapture 定位） | CI flaky、不可离线测 |
| CB9 | unsafe 只允许出现在 `clipboard_macos.rs`（模块级 `#![allow(unsafe_code)]` + 每个 unsafe 块 `// SAFETY:`，照 rc-input-macos ffi/mod.rs:19 模式）；rc-session 其余模块维持 `deny(unsafe_code)` | 破坏 crate 纪律（20-crate-contracts.md §8 检查清单） |
| CB10 | 统计可观测：`session.clipboard` 段进两侧报告（T18 键名契约只增）；host stderr 摘要含 sent/received | 同步静默失效无从诊断 |
| CB11 | 本卡对 rc-net / rc-pipeline / rc-input **零改动**（与 T15 不同，本卡不碰 rc-pipeline）；proto.rs 只做 0x0F 增量与预留名单收窄，既有 0x01–0x0C 字节布局一字不动 | 污染已过 M1 Gate 的稳定层 |

## 不要做什么

- ❌ 不做文件传输（M6）与富格式 / 图片 / 文件路径同步（D5/D6，P2）——只同步 UTF-8 纯文本。
- ❌ 不做剪贴板历史 / 管理器、拖拽、批量 flavor 协商；不做「跟随用户复制事件」的系统钩子（macOS 无官方剪贴板变更通知 API，本卡就是 changeCount 轮询）。
- ❌ 不做 M5 权限分级 / 鉴权联动（T19/M5 接管；本卡以显式旗标代「显式授权」）。
- ❌ 不做 Windows/Linux 真后端（trait 已备，D/E 阶段接入）；`--clipboard on` 在非 macOS 平台报错退出而不是静默空操作。
- ❌ 不引 objc2 / objc / icrate 等任何新依赖：macOS 后端走 CarbonCore Pasteboard C API + workspace 既有的 core-foundation 0.10（rc-input-macos 同版本线）。
- ❌ 不把剪贴板泵塞进媒体 / 输入泵——独立 `pump_clipboard`，未启用时零开销。
- ❌ 不改 0x0D/0x0E（T19 领地）；号位不回收不复用（T13 冻结承诺）。

## 测试清单

**proto.rs 内联单测（追加到 T13 测试集，全离线确定性）：**

| 测试名 | 断言要点 |
|---|---|
| `clipboard_roundtrip_and_wire_layout` | 空文本（`[0x0F, 0x00, 0x00]`）、纯 ASCII、中文+emoji 多字节、含 NUL 字节：encode→decode 相等，且逐字节断言 `[0x0F][len u16 LE][text]` 布局（NUL 透明传输、len 按字节计） |
| `clipboard_decode_rejects_truncation_and_trailing` | 合法编码的**每个**严格前缀解码报 Protocol；合法编码 + 1 尾字节报 Protocol（照 T13 `decode_rejects_truncation_at_every_offset` 同款穷举） |
| `clipboard_type_decodes_and_unknowns_still_rejected` | 0x0F 从预留名单移出、可解出 `ClipboardNotify`；未知号 0x10/0x99 照旧拒收；0x0D/0x0E 按 proto.rs 当下实装集合断言（T19 未合入则仍拒收且错误文本含出错字节值） |
| `clipboard_constructor_enforces_cap` | 65535 字节恰好可构造；65536 → Err(InvalidConfig)；构造成功路径 encode 不 panic |

**clipboard.rs 内联单测（逻辑钟注入，`FakeClipboard` 全内联，无 IO）：**

| 测试名 | 断言要点 |
|---|---|
| `first_change_sends_immediately` | B4/CB4 确定性证明：基线拍之后首次变更**当拍**即产 `SendToPeer`（零节流延迟） |
| `initial_baseline_does_not_send` | CB6：首拍只记基线返回 Idle；无变更连推多拍全 Idle、stats 全零 |
| `changes_within_window_coalesce_to_latest` | 100ms 窗口内 3 次变更 → 到期冲刷恰 1 条、内容 = 第 3 次；coalesced = 2 |
| `pending_flushes_only_after_window` | 窗口内冲刷不发生（对端零消息）；窗口过后的第一拍恰冲刷一条 |
| `own_write_echo_is_suppressed` | CB5：`on_peer_text` 写入后 pump_local 观测到变更计数递增 → Idle、echo_suppressed +1、零外发 |
| `identical_local_copy_is_suppressed` | 用户再复制与 last_written 相同文本 → 抑制（对端已有同值，信息无损） |
| `incoming_notify_discards_pending` | 挂起 "old" 期间收到对端 "new" → superseded_local +1，此后任何拍都不再发出 "old" |
| `oversize_text_dropped_not_truncated` | CB3：70KB → 无 Send、oversize_dropped +1、对端永远不会收到被劈开的多字节尾巴 |
| `non_text_change_not_synced` | read_text = None（复制图片）→ 不发、non_text_dropped +1 |
| `empty_text_syncs_clear` | Some("") → 发送空文本（线 len=0），对端 write_text("") 被调用 |
| `inbound_identical_text_skips_write` | 对端连发两条相同文本 → 第二条不写本机（writes 长度不变） |

**进程内双会话集成（`crates/rc-session/tests/clipboard_session.rs`，自带最小 `LoopTransport` 对接两条 reliable 流，照 T15 `tests/probe_session.rs` 形态，不依赖 CLI）：**

| 测试名 | 断言要点 |
|---|---|
| `clipboard_syncs_both_directions` | client FakeClipboard 模拟复制 → host 侧 FakeClipboard.writes() 收到同文本；host 反向复制 → client 收到 |
| `no_ping_pong_after_sync` | 同步后连续驱动 20 拍（逻辑钟），两侧 reliable 通道零新增消息、echo_suppressed 恰各 1 |
| `throttle_burst_sends_single_latest_over_transport` | 窗口内 3 连复制 → 对端恰收 1 条且为最新值 |
| `notify_while_disabled_is_dropped_not_written` | 未 `enable_clipboard` 的会话收到 0x0F → received_while_disabled +1、writes() 为空、会话不拆（后续媒体/输入照常流动） |

**rc-cli 双子进程测试（追加 `tests/host_client.rs`，真 QUIC over 127.0.0.1、CI 可跑、不碰真实剪贴板）：**

| 测试名 | 断言要点 |
|---|---|
| `clipboard_scripted_sync_over_real_quic` | host `--clipboard scripted` + client `--clipboard scripted`，client 侧 500ms 模拟复制 → host 报告 `session.clipboard.received ≥ 1`；`--clipboard` 缺省时两侧报告 `enabled == false` |

**真机测试（`#[ignore]`；无需 TCC，但会改你登录会话的真实剪贴板）：**

| 测试名 | 断言要点 |
|---|---|
| `#[ignore] real_pasteboard_write_read_roundtrip` | `NspasteboardBackend::write_text` → change_count 递增 → read_text 回读一致；手造无文本 flavor 状态 → read_text = None |
| `#[ignore] real_pasteboard_sync_under_200ms` | 一端真 pasteboard + 一端 FakeClipboard，进程内双会话驱动（墙钟仅测试测量用）：模拟复制 → 对端 writes 出现同文本的墙钟耗时 < 200ms（B4） |

## 验收命令

```bash
# 1. 内循环（M0 纪律：rc-session 依赖扇出大，日常只查本 crate）
cargo check -p rc-session --all-targets
cargo check -p rc-cli --all-targets
cargo test -p rc-session
cargo test -p rc-cli

# 2. 全仓回归 + 质量门（本卡含 unsafe 平台模块，必须全量）
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# 3. 铁律 4 自检：命中只允许出现在 testkit.rs（ScriptedClipboard 的 IO
#    边界墙钟）与 client_session.rs（T14 坑 5 认可的 run() 驱动循环
#    pump_sender 批窗口钟——T14 落地后该文件必有命中，不属违规）
grep -rn "Instant::now\|SystemTime::now" crates/rc-session/src \
  | grep -v "testkit.rs" | grep -v "client_session.rs" \
  && echo "违规" || echo "干净"

# 4. unsafe 边界自检：只允许 clipboard_macos.rs（末个 grep 排除
#    `#![deny(unsafe_code)]` 属性行本身——lib.rs 等模块头的 deny 文本
#    含 "unsafe" 关键字，属合规声明不是违规，不排除则恒误报）
grep -rn "unsafe" crates/rc-session/src --include="*.rs" \
  | grep -v "clipboard_macos.rs" | grep -v "deny(unsafe_code)" \
  && echo "违规" || echo "干净"

# 5. 真机剪贴板（#[ignore]，无需 TCC；会改你当前登录会话的剪贴板，知悉后再跑）
cargo test -p rc-session real_pasteboard -- --ignored --nocapture
```

## 验收标准

- [ ] `cargo test --workspace` 全绿（含 T13 proto 全量回归——0x0F 增量不破坏既有 12 变体线格式测试）
- [ ] 0x0F 逐字节线布局锁定（空文本 / 多字节 / NUL），截断与尾字节全拒；`clipboard_notify` 构造器 65536 字节拒绝
- [ ] B4 确定性证明：变更当拍即 Send（离线）；`#[ignore]` 真机墙钟 <200ms
- [ ] 回声抑制被 `no_ping_pong_after_sync` 钉死：同步后 20 拍零新增消息
- [ ] 节流突发 → 对端恰收 1 条最新值；挂起值被对端后到通知作废
- [ ] 70KB 整条丢弃不截断；非文本不同步；未启用收到 0x0F 不写本机不拆会话
- [ ] 双进程真 QUIC（scripted 模式）同步打通：host 报告 `session.clipboard.received ≥ 1`
- [ ] unsafe 仅存在于 clipboard_macos.rs 且块块带 `// SAFETY:`；`cargo clippy --workspace --all-targets -- -D warnings` 干净
- [ ] 两侧报告含 `session.clipboard` 段（T18 键名只增不改名）
- [ ] rc-net / rc-pipeline / rc-input 零改动（diff 审查确认）
- [ ] 依赖面：仅 macOS 侧新增 core-foundation 0.10（workspace 既有版本线），无 workspace 外新 crate
- [ ] TASK.md §0/§12/§13 已回写（实测数字带场景与命令），tickets README 状态表已更新

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`crates/rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | 泵式 API 形态（`pump_sender`:83 / `pump_receiver`:111）与「IO 归边、逻辑归引擎」的分层；MockTransport 测试替身先例 |
| [`crates/rc-input/src/lib.rs`](../../../crates/rc-input/src/lib.rs) | 手写线格式纪律：encode 追加式（:156）、decode 带偏移报错（:222）、`decode_bool` 严格 0/1（:324）——0x0F 的编解码照这个手感写 |
| [`crates/rc-input-macos/src/ffi/mod.rs`](../../../crates/rc-input-macos/src/ffi/mod.rs) | unsafe 集中单一模块的完整模式：模块级 `#![allow(unsafe_code)]`（:19）、手写 extern "C" raw 模块、每个块 `// SAFETY:`（lib.rs:37 的 lint 纪律） |
| [T13 卡](T13-session-proto.md) | 0x0F 占号现状与「变体增量添加、号位冻结」纪律（「与后续卡的接口」节） |
| [T15 卡](T15-probe-initial-bitrate.md) | 会话层增量扩展先例：`pump_xxx(now_us)` + 控制分发新增臂 + 自带 LoopTransport 的进程内双会话集成测试形态 |
| [T18 卡](T18-cli-host-client-localhost.md) | CLI 旗标风格（InputMode/InjectMode 先例）、ScriptedCapture 的 IO 边界墙钟定位、报告 session 段键名契约（只增不改名） |
| [`crates/rc-net/src/quic.rs`](../../../crates/rc-net/src/quic.rs) | reliable 流 u32 LE 成帧与消息边界（模块文档 :23-25）、单条上限 4MB（:102）——64KB 剪贴板消息的安全余量依据 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **回声回环**：我们把对端文本写进本机剪贴板，本机 changeCount 随即 +1，下一拍就被当成「本地变更」发回去——A↔B 无限乒乓打满控制流 | 内容比对抑制（last_written）+ 集成测试 `no_ping_pong_after_sync` 钉死；别试图用计数差区分「谁写的」（计数器不区分来源） |
| 2 | **挂起旧值迟到覆盖**：本地变更挂起（节流窗口内）时对端通知先到，若照常冲挂起，旧值会晚于对端新值到达、覆盖对端 | `on_peer_text` 作废 pending（后到权威覆盖），`incoming_notify_discards_pending` 钉死 |
| 3 | **NULL ≠ kPasteboardClipboard**：`PasteboardCreate` 传 NULL 得到的是**私有**粘贴板，读写永远不触系统剪贴板，测试还全绿（私有板上自洽） | 自建同值 CFString "com.apple.pasteboard.clipboard"；真机 `#[ignore]` 测试用另一个进程可见性兜底（回读走同一引用仍不足，需真机人工核一次） |
| 4 | **读前不 Synchronize**：Carbon Pasteboard 有跨进程缓存，不 `PasteboardSynchronize` 可能读到旧 changeCount / 旧数据 | 后端实现读路径固定「Synchronize → GetChangeCount / CopyItemFlavorData」顺序 |
| 5 | **macOS 15+ 剪贴板隐私提示**：后台进程读剪贴板可能触发系统「允许粘贴」弹窗（宿主 App 无用户交互时）。真机联调经 `RabbitControlDev.app`（幕布工具）跑，提示出现时允许一次即可 | 已知机制、不绕过；「跟随用户复制事件再读」的长期方案记为后续增强，不在本卡。本卡 `#[ignore]` 测试直接 `cargo test -- --ignored` 跑（无 TCC 需求） |
| 6 | **64KB 走 reliable 控制流的队头影响**：一条 64KB 通知会短暂挤占控制流（同流的 ProbePing / IdrRequest 排在后面）。输入走独立 urgent 流不受影响（N6，rc-net quic.rs 模块文档 :23-25 的通道分工） | 65535 上限本身就是缓解（对照 MAX_RELIABLE_MSG 4MB）；节流把最坏速率压到 ≤10 条/秒。文档写明取舍，不为此改走 datagram（丢包 = 静默失同步） |
| 7 | **截断诱惑**：超限文本「截到 64KB 发一半」看似优雅，实际劈开多字节字符且静默损坏用户数据（对端粘出半行乱码比不同步更糟） | CB3 整条丢弃 + oversize_dropped 计数；上限提升走版本协商（PROTOCOL_VERSION bump），不得私改 u32 前缀 |
| 8 | **u16 前缀的天然封顶**：65535 是线格式与前缀宽度绑死的协议属性，改动 = 破坏 T13 冻结的线风格 | 单一常量 `proto::MAX_CLIPBOARD_TEXT_BYTES`，引擎默认直接引用它；真需要更大文本是版本协商 + 新前缀宽度的事 |
| 9 | **剪贴板管理器 / 密码管理器高频写**：每秒数十次 changeCount 跳变，逐条转发即风暴 | leading-edge + coalesce：窗口内无论多少次变更，到期只发最新一条（CB4）；同值重写由内容比对天然去重 |
| 10 | **非文本内容的语义空档**：本机复制图片时对端**保留旧文本**（用户在对端粘贴会得到粘贴前的旧文本）——文本-only 同步的固有边界，不是 bug | 文档写明（CB7）；富格式/图片/文件是 D5/D6（P2）与 M6 的事 |
| 11 | **rc-session 编译扇出大**（rc-net/rc-pipeline/rc-input 三条链，设计师风险 9） | 内循环只 `cargo check -p rc-session`；`cargo test --workspace` 只在收尾跑（M0 纪律） |
| 12 | **T17 重连后的旧纪元状态**：resume 恢复的会话里，旧 `last_written` / changeCount 基线属于断线前世界（对端剪贴板可能已变） | 重连成功后重新 `enable_clipboard`（替换式、重置基线）；无缝剪贴板续传不承诺 |

## 依赖

```toml
# crates/rc-session/Cargo.toml 新增（其余依赖集由 T13/T14 建好，不动）
[target.'cfg(target_os = "macos")'.dependencies]
# CarbonCore Pasteboard C API 的 CFString/CFData 包装。与 rc-input-macos
# 同一版本线（0.10），workspace 依赖树内已有，不算引入新第三方 crate。
core-foundation = "0.10"
```

不引 objc2 / objc / serde：编解码手写（对齐 rc-input / T13），平台 FFI 纯 C 符号自行声明（对齐 rc-input-macos ffi 的 raw 模块模式）。

## 与后续卡的接口

- **T19 鉴权**：0x0D/0x0E 实装时预留拒收名单进一步收窄，与本卡号位互不影响；Hello.auth_method 语义不动。
- **T21 控制端 UI**：会话工具栏的剪贴板开关消费 `enable_clipboard` / `clipboard_stats`；`session.clipboard` 段是数据源（键名只增）。
- **T22 被控端托盘 / 接受弹窗**：M2-7 的连接请求弹窗后续可挂「本次会话允许剪贴板」开关，落点就是 `enable_clipboard`。
- **M5 权限体系**：接管「显式授权」（本卡旗标是过渡形态）；E2E 加密在 QUIC TLS 之下，不动本卡消息。
- **M6 文件传输**：复用本卡的「reliable 控制流大消息预算 + 上限拒绝不截断」先例，另立消息号。
- **D5/D6（剪贴板图片 / 文件路径，P2）**：若做，走版本协商扩展 flavor 元数据，不改 0x0F 文本语义。
