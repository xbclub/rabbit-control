# T13 rc-session 控制协议线格式

> 新建 `rc-session` crate（workspace 第 22 个成员），交付 `proto.rs`：M2 会话层全部控制消息的枚举、手写二进制 encode/decode 与版本协商字段校验。**纯逻辑，零 IO。**

## 背景

M2 开双机直连：会话握手、IDR 请求、探测、码率设定、重连恢复都要一条**控制消息协议**。它没有现成的家：rc-net 有 T09 红线「不碰采集/编码/解码」只做传输；rc-pipeline 是已过 M1 Gate 的本机管线驱动（host.rs/controller.rs 只认 tick/push_datagram，不认连接生命周期）；rc-cli 必须薄。所以新建 `rc-session`，本卡打第一块砖——**只定义「控制消息长什么样」**：消息体 = 1 字节类型 + 载荷，手写编解码，对齐 rc-input 的线格式纪律（拒绝未知类型、错误带偏移、绝不 panic）。后续 T14（状态机/握手）、T15（探测/码率）、T16（IDR 可靠性）、T17（重连）全部消费本卡，一个字节都不用再发明。

## 平台要求

**全平台，纯逻辑。** 无网络、无 GPU、无真机、无权限。本卡测试全部离线确定性（时间零依赖，本卡根本不碰时间）。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/Cargo.toml` | 新建 | 依赖**仅** `rc-types` |
| `crates/rc-session/src/lib.rs` | 新建 | crate 骨架：中文 crate 级文档（为什么存在 / 模块地图 / 后续模块归属卡）+ `#![deny(missing_docs)]` + `#![deny(unsafe_code)]` + `pub mod proto;`（本阶段唯一模块） |
| `crates/rc-session/src/proto.rs` | 新建 | 全部控制消息：类型号常量、枚举、encode/decode、版本校验 + 内联单测 |
| `Cargo.toml`（workspace 根） | 修改 | `members` 追加 `"crates/rc-session"` 一行（第 22 个成员），**不改其他任何行** |

## API 契约

> `20-crate-contracts.md` 尚无 rc-session 节，**本节即契约**，后续汇总文档照此抄入。跨卡共用语义（消息字段、类型号）以此为准，不得自创分歧。

```rust
// crates/rc-session/src/proto.rs
//! 控制协议线格式（M2 会话层）。
//! 消息体 = [type u8] + 载荷；多字节整数一律小端；变长段带 [len u16 LE] 前缀
//! （BwProbeData 的 padding 例外：剩余字节即 padding，见变体文档）。

/// 控制协议版本。Hello 携带，不匹配即拒收（本卡校验，T14 直接消费错误）。
pub const PROTOCOL_VERSION: u8 = 1;

// ── 类型号（0x01..=0x0C 本卡实现；0x0D..=0x0F 冻结占号，T19/T20 增量添加）──
pub const TYPE_HELLO: u8 = 0x01;
pub const TYPE_WELCOME: u8 = 0x02;
pub const TYPE_IDR_REQUEST: u8 = 0x03;
pub const TYPE_IDR_ACK: u8 = 0x04;
pub const TYPE_PROBE_PING: u8 = 0x05;
pub const TYPE_PROBE_PONG: u8 = 0x06;
pub const TYPE_SET_BITRATE: u8 = 0x07;
pub const TYPE_BW_PROBE_REQUEST: u8 = 0x08;
pub const TYPE_BW_PROBE_DATA: u8 = 0x09;
pub const TYPE_INPUT_ACK: u8 = 0x0A;
pub const TYPE_RECONNECT_REQUEST: u8 = 0x0B;
pub const TYPE_BYE: u8 = 0x0C;
/// 预留：鉴权挑战（M2-5 / T19）。占号公开，载荷语义 T19 定义。
pub const TYPE_AUTH_CHALLENGE: u8 = 0x0D;
/// 预留：鉴权应答（M2-5 / T19）。
pub const TYPE_AUTH_RESPONSE: u8 = 0x0E;
/// 预留：剪贴板文本通知（M2-8 / T20）。
pub const TYPE_CLIPBOARD_NOTIFY: u8 = 0x0F;

/// BwProbeRequest.total_bytes 的线格式上限（设计师裁定 256KB）。
pub const MAX_BW_PROBE_BYTES: u32 = 256 * 1024;

/// Hello 请求 / Welcome 裁定共用的流参数（client 请求值，host 夹紧后的真值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// Hello 的鉴权方式。0=无（本轮唯一实装），1=密码（M2-5 / T19 预留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthMethod { None = 0, Password = 1 }

/// Welcome 的鉴权状态。2=拒绝并断开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthStatus { Ok = 0, ChallengeRequired = 1, Rejected = 2 }

/// IdrRequest 的原因（M2-3）：0=解码器 NeedKeyframe、1=停滞看门狗、2=重连恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IdrReason { DecoderNeedKeyframe = 0, WatchdogStall = 1, ReconnectRecovery = 2 }

/// SetBitrate 的原因（M2-10）：0=初始、1=探测、2=手动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BitrateReason { Initial = 0, Probe = 1, Manual = 2 }

/// Bye 的原因：0=正常关闭、1=u32 时间戳饱和刷新（T17：host 钟逼近 71.6 分钟
/// 守卫线时发 1，client 以 resume=false 全新重连）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ByeReason { Normal = 0, Refresh = 1 }

/// Welcome 的 host 能力位。未知 bit **拒收**（版本升级走 version 协商，
/// 不靠容忍未知位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFlags(pub u8);

impl HostFlags {
    /// bit0：ReconnectRequest.resume 被接受。
    pub const RESUME_ACCEPTED: u8 = 0b0000_0001;
    /// bit1：时钟纪元已重置（饱和刷新后新会话）。
    pub const CLOCK_EPOCH_RESET: u8 = 0b0000_0010;
    /// 已定义位的掩码；decode 用它拒绝未知 bit。
    pub const KNOWN_MASK: u8 = Self::RESUME_ACCEPTED | Self::CLOCK_EPOCH_RESET;

    pub const fn resume_accepted(&self) -> bool;
    pub const fn clock_epoch_reset(&self) -> bool;
}

/// 0x01 Hello（client→host）。version ≠ PROTOCOL_VERSION 在 decode 即拒收
/// （本卡无状态机，拒收语义必须全部落在线格式层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// 线上原样字段；decode 成功后恒等于 PROTOCOL_VERSION。
    pub version: u8,
    /// client 生成的会话标识（随机 u64，T14 负责生成策略）。
    pub session_id: u64,
    /// 请求参数（host 侧按显示器与编码器能力夹紧，见 Welcome）。
    pub requested: SessionParams,
    /// 鉴权方式。本轮会话层只接受 None（T19 实装 Password）。
    pub auth_method: AuthMethod,
    /// 鉴权载荷（线上带 auth_len u16 前缀；T19 前恒空）。
    pub auth_payload: Vec<u8>,
}

/// 0x02 Welcome（host→client）：session_id 回显 + 裁定参数 + 状态位。
/// client 一律以 granted（裁定真值）为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub session_id: u64,
    pub granted: SessionParams,
    pub auth_status: AuthStatus,
    pub host_flags: HostFlags,
}

/// 一条控制消息。**通道约定**：除 BwProbeData 走 datagram 通道外，
/// 其余全部走 rc-net reliable 流（rc-net 已按消息边界交付，decode 输入
/// 恰是一条完整消息的字节，不含传输层 u32 长度前缀）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    Hello(Hello),
    Welcome(Welcome),
    /// client→host。host 收到即 `HostPipeline::request_idr()`（T14 接线）。
    IdrRequest { reason: IdrReason },
    /// host→client。frame_seq 对应 host 真编出的 IDR 帧（T16 去抖/复核用）。
    IdrAck { frame_seq: u64 },
    /// 双向（T15）：RTT + 时钟偏移探测，默认 1s 间隔，兼保活。
    ProbePing { probe_id: u32, sender_clock_us: u64 },
    /// 双向（T15）：NTP 风格回显。发起方 RTT = now − t0，偏移 = 中点差。
    ProbePong { probe_id: u32, echoed_clock_us: u64, responder_clock_us: u64 },
    /// client→host（T15）：经 HostPipeline::set_bitrate 转发编码线程。
    SetBitrate { bps: u32, reason: BitrateReason },
    /// client→host（T15 v0）：请求 host 发送 total_bytes 字节的带宽探测流。
    /// total_bytes > MAX_BW_PROBE_BYTES 在 decode 即拒收。
    BwProbeRequest { probe_id: u32, total_bytes: u32 },
    /// host→client，**走 datagram 通道**（不占 reliable 控制流）。padding
    /// 无长度前缀：消息 = [type][probe_id u32][chunk_idx u32] + 剩余字节即
    /// padding（边界由承载它的数据报给出，消息内不再自带上限）。
    /// 注意：v0 测的是 pacer 天花板（50Mbps）而非真实带宽，只是粗启发式，
    /// 禁止据此宣称「精确带宽」（T15 / M3-3 边界）。
    BwProbeData { probe_id: u32, chunk_idx: u32, padding: Vec<u8> },
    /// host→client：key_batch_seq 按注入批计数（两侧 reliable 有序流独立
    /// 计数天然对齐）；inject_done_us 为 host 会话钟，client 用偏移换算
    /// 输入单程延迟（T15/T18）。
    InputAck { key_batch_seq: u64, inject_done_us: u64 },
    /// client→host（T17）：resume=1 请求保持管线续传；host 以
    /// Welcome.host_flags.bit0 应答是否接受。
    ReconnectRequest { session_id: u64, resume: bool },
    /// 双向：优雅关闭 / 饱和刷新。
    Bye { reason: ByeReason },
}

impl ControlMessage {
    /// 编码并**追加**到 `out`（风格对齐 rc-input 的 `encode_events`）。
    /// 空载荷变体只写类型字节。不失败、不 panic。
    pub fn encode(&self, out: &mut Vec<u8>);

    /// 解码一条完整消息（`encode` 的逆）。空缓冲、未知类型字节、预留号
    /// （0x0D..=0x0F）、version 不匹配、枚举字段越界、布尔字段非 0/1、
    /// host_flags 未知 bit、auth_len 与实际字节数不符、载荷截断、尾部
    /// 多余字节——一律返回 [`rc_types::Error::Protocol`]（带 offset 与
    /// 出错值），绝不 panic、绝不静默跳过。
    pub fn decode(buf: &[u8]) -> Result<Self>;
}
```

### 线格式总表（字节布局，整数一律 LE）

| 类型 | 消息 | 布局（type 之后） | 定长 |
|---|---|---|---|
| 0x01 | Hello | `version u8` `session_id u64` `width u32` `height u32` `fps u32` `bitrate_bps u32` `auth_method u8` `auth_len u16` `auth_payload[len]` | 29 + len（本轮恒 29） |
| 0x02 | Welcome | `session_id u64` `width u32` `height u32` `fps u32` `bitrate_bps u32` `auth_status u8` `host_flags u8` | 27 |
| 0x03 | IdrRequest | `reason u8` | 2 |
| 0x04 | IdrAck | `frame_seq u64` | 9 |
| 0x05 | ProbePing | `probe_id u32` `sender_clock_us u64` | 13 |
| 0x06 | ProbePong | `probe_id u32` `echoed_clock_us u64` `responder_clock_us u64` | 21 |
| 0x07 | SetBitrate | `bps u32` `reason u8` | 6 |
| 0x08 | BwProbeRequest | `probe_id u32` `total_bytes u32`（≤ 256KB） | 9 |
| 0x09 | BwProbeData | `probe_id u32` `chunk_idx u32` `padding`（剩余字节，无前缀） | 9 + n |
| 0x0A | InputAck | `key_batch_seq u64` `inject_done_us u64` | 17 |
| 0x0B | ReconnectRequest | `session_id u64` `resume u8`（严格 0/1） | 10 |
| 0x0C | Bye | `reason u8` | 2 |
| 0x0D–0x0F | 预留 | —（占号拒收，T19/T20 增量定义） | — |

**校验边界**：decode 严格校验「线格式完整性」——版本号、枚举值域、布尔 0/1、flags 已知位、变长段长度一致性、截断/尾字节。**数值参数（width/fps/bps 等）不做语义校验**（0 或离谱值照收）——夹紧与裁定是 T14 host 的职责，本卡不越权。

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| P1 | 消息体 = 1 字节类型 + 载荷，多字节整数一律小端；变长段带 u16 LE 长度前缀（padding 例外，见表） | 对端解不出；字节序错 = 静默数据错乱 |
| P2 | 未知类型字节与预留号（0x0D/0x0E/0x0F）一律 `Error::Protocol` 拒收，错误文本含出错字节值 | 静默跳过 = 安全边界失守（rc-input 同款纪律） |
| P3 | Hello 的 `version ≠ PROTOCOL_VERSION` 在 decode 即拒收，错误文本含收到的版本号 | 新旧版本混连 = 字段错位解释，远控场景不可接受 |
| P4 | 枚举型字段（auth_method / auth_status / 各 reason）、布尔字段（严格 0/1）、host_flags 未知 bit 全部严格校验 | 「宁拒不猜」被破坏，脏数据变成幽灵行为 |
| P5 | 截断与尾部多余字节都报 `Protocol`（带 offset） | 帧错位蔓延到下一条消息 |
| P6 | decode 绝不 panic、不读越界；所有错误带 offset 与出错值 | 恶意/损坏流量直接打崩 host |
| P7 | `BwProbeRequest.total_bytes ≤ MAX_BW_PROBE_BYTES`（256KB），decode 拒超限 | 恶意 client 借探测流量打满带宽 |
| P8 | 时钟字段（sender_clock_us / responder_clock_us / inject_done_us）线格式一律 **u64**——不得改用 rc-types 的 u32 `Timestamp`（它 71.6 分钟饱和，饱和守卫是 T17 会话层职责，线格式本身不能饱和） | 长会话探测/应答在 71.6 分钟后错乱 |
| P9 | 本 crate 此阶段依赖**仅 rc-types**；不 import rc-net / rc-pipeline / rc-input，不做任何 IO | 范围蔓延成「半个状态机」；编译扇出无谓变大 |
| P10 | crate 级 `#![deny(missing_docs)]` + `#![deny(unsafe_code)]`，注释中文 | 违反项目纪律（对齐 rc-net / rc-input） |

## 不要做什么

- ❌ 不做任何 IO：不建 Endpoint、不碰 `Transport`、不开 socket（T14 的事）。
- ❌ 不做状态机、握手流程、超时/重试逻辑（T14）；探测算法、码率策略（T15）；请求去抖（T16）；重连退避（T17）。
- ❌ 不实装 AuthChallenge / AuthResponse / ClipboardNotify 的变体与载荷——只发布占号常量并在 decode 显式拒收；变体由 T19/T20 **增量**添加，号位已冻结不得改。
- ❌ 不动 `rc-types`（铁律 5）；控制消息**不进** `rc-proto`（它保持纯媒体数据报格式）；不创建 host_session.rs / client_session.rs / probe.rs / watchdog.rs / report.rs / testkit.rs 空壳模块（各自随 T14+ 落地，骨架期 lib.rs 只声明 `pub mod proto;`）。
- ❌ 不引入 serde / protobuf / bytes 等任何新依赖（手写编解码）。
- ❌ 不改 workspace 根 Cargo.toml 除 members 追加一行之外的任何内容；不改其他 crate。

## 测试清单

全部为**离线确定性单测**（内联于 `proto.rs` 的 `#[cfg(test)]`，无网络、无时间依赖、无 `#[ignore]` 项——本卡没有真机测试）：

| 测试名 | 断言要点 |
|---|---|
| `roundtrip_all_variants` | 12 个变体（含 Hello/Welcome 结构体、BwProbeData 带非空 padding）逐一 encode→decode→`assert_eq` 相等 |
| `wire_layout_is_locked_to_spec` | 代表性消息逐字节断言：Hello（29 字节，LE、字段顺序、auth_len=0）、Welcome（27 字节）、IdrRequest（`[0x03, 0x01]`）、ProbePong（21 字节）——锁死线格式表 |
| `decode_rejects_empty_and_unknown_type` | 空缓冲、`[0x00]`、`[0x10]`、`[0x99]` 均 `Error::Protocol` 且文本含出错字节值 |
| `decode_rejects_reserved_types` | `[0x0D]`/`[0x0E]`/`[0x0F]` 拒收，错误文本含 "reserved" 与对应 M2-5/M2-8 归属 |
| `decode_rejects_truncation_at_every_offset` | 对每个变体的合法编码，取**每个**严格前缀（0..len−1 字节）解码全部报错（消息最长 29 字节，穷举代价可忽略） |
| `decode_rejects_trailing_bytes` | 合法编码 + 1 字节尾巴报错（含 `trailing` 字样与 offset） |
| `hello_version_mismatch_rejected_with_value` | 手工构包 version=0 / 2 / 255：拒收且错误文本含收到的版本号与 `PROTOCOL_VERSION`；encode 出的 Hello 恒 version=1 可解 |
| `auth_len_must_equal_payload_len` | auth_len=3 而载荷 0 字节 → truncated；auth_len=0 而带 3 字节尾巴 → trailing bytes |
| `enum_fields_strict_value_domains` | auth_method=2、auth_status=3、IdrReason=3、BitrateReason=3、ByeReason=2、resume=2 逐个手工构包，全部拒收 |
| `host_flags_reject_unknown_bits` | 0x00/0x01/0x02/0x03 可解且访问器正确（resume_accepted / clock_epoch_reset）；0x04、0x80、0xFF 拒收 |
| `bw_probe_request_caps_total_bytes` | total_bytes = `MAX_BW_PROBE_BYTES` 恰好可解；+1 拒收（含上限值） |
| `bw_probe_data_padding_is_opaque` | padding 为空与含 `0x00`/`0xFF`/类型字节同款字节的二进制垃圾均无损往返（剩余字节语义不被解释） |

## 验收命令

```bash
cargo check -p rc-session
cargo test -p rc-session
cargo clippy -p rc-session --all-targets -- -D warnings
cargo test --workspace
```

（内循环只跑前三条；`--workspace` 是合并前的全仓回归，见已知坑 #8。）

## 验收标准

- [ ] `cargo test -p rc-session` 全绿（≥ 12 个测试，覆盖上表全部用例）
- [ ] `cargo test --workspace` 全绿（证明未破坏既有 21 crate）
- [ ] `cargo clippy -p rc-session --all-targets -- -D warnings` 干净
- [ ] 未知类型（0x00/0x10/0x99）与预留号（0x0D..=0x0F）全部显式拒收（P2）
- [ ] `version=2` 的 Hello 被拒收且错误文本含 `2`（P3）
- [ ] `Cargo.toml` 显示 workspace 成员恰 22 个，rc-session 依赖列表只有 rc-types（P9）
- [ ] 无 `unsafe`、无新第三方依赖、crate 编译零警告（P10）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-input/src/lib.rs`](../../../crates/rc-input/src/lib.rs) | 手写线格式的全套纪律：encode 追加式（lib.rs:156）、decode 带偏移报错（lib.rs:222）、`decode_bool` 严格 0/1（lib.rs:324）、未知类型拒绝（lib.rs:319） |
| [`rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | 两层成帧关系：u16 消息前缀叠在 rc-net 的 u32 流前缀之上，无害且各有职责（channel.rs:21-32）——控制消息沿用同一格局 |
| [`rc-net/src/lib.rs`](../../../crates/rc-net/src/lib.rs) + [`quic.rs`](../../../crates/rc-net/src/quic.rs) | `NetEvent::Reliable` 一条 = 一条完整消息（decode 的输入粒度）；reliable 消息上限 `MAX_RELIABLE_MSG`（quic.rs:102）与 datagram 预算 `QUIC_OVERHEAD`（quic.rs:72） |
| [`docs/reference/impl/20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.3 | 传输层契约：本卡消息将跑在哪些通道上（边界知识，不实现） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **双层长度前缀**：rc-net reliable 流已按 u32 LE 长度前缀成帧并按消息边界交付（quic.rs 模块文档 21-27 行；上限 4MB，quic.rs:102）。控制消息自带 u16 变长段前缀叠在其上无害（rc-input channel.rs:21-32 已论证） | 本卡**不要**再给整条消息包一层消息级长度前缀；decode 输入 = 去掉传输层前缀后的一条完整消息 |
| 2 | BwProbeData 走 **datagram** 通道（设计师指定，避免探测流量阻塞控制流），padding 无长度前缀、边界由数据报给出 | decode 不假设通道（字节进字节出）；但单条尺寸天然受 `mtu − 48` 约束，测试**不要**构造 MB 级 padding 去验证上限 |
| 3 | 时钟字段是 u64 而非 rc-types `Timestamp`(u32)：`Timestamp::since` 71.6 分钟饱和且契约明示「会话层必须在此之前重建」（20-crate-contracts.md:93）。线格式故意用 u64，饱和守卫（70 分钟 Bye(Refresh)→T17）是会话层职责 | 不要「顺手统一类型」改成 u32/Timestamp——那会把饱和提前埋进协议 |
| 4 | 预留号语义真空：0x0D/0x0E/0x0F 本卡只占号。若实现变体等于提前发明 T19/T20 的载荷语义；若静默跳过则违反「宁拒不猜」 | 公开常量冻结号位 + decode 显式拒收（错误文本注明归属 T19/T20），变体日后增量添加、号不变 |
| 5 | 版本校验放 decode 而不是留给 T14：本卡没有状态机，且 `Error::Protocol` 本就是 `is_fatal() == true`（rc-types error.rs:71）——T14 把它按致命处理即可，无需自己再比一次 version | 错误文本必须带收到的版本号（日志可诊断「client 说 v2、host 说 v1」） |
| 6 | BwProbe 消息 ≠ 带宽测量精度：v0 探测受 pacer 初始速率天花板 50Mbps（`DEFAULT_INITIAL_PACING_BPS`，quic.rs:79）与 datagram 丢包影响，只是初始码率下限保护的粗启发式 | 变体文档写死边界（「禁止据此宣称精确带宽」），M3-3 拥塞控制接管后废弃 |
| 7 | `deny(missing_docs)` 覆盖所有 pub 项**包括结构体字段与常量**（rc-net/rc-input 同款纪律） | 每个 pub 字段都要 `///` 中文文档注释，别只给类型注释 |
| 8 | rc-session 后续会同时依赖 rc-net/rc-pipeline/rc-input 三个 crate（编译扇出大），但**本卡只依赖 rc-types**——不要提前把依赖挂上「为以后做准备」 | 内循环 `cargo check -p rc-session`；全仓 `cargo test --workspace` 只在收尾跑一次 |

## 依赖

```toml
[dependencies]
rc-types = { path = "../rc-types" }
```

（serde/protobuf 一概不引：高频小包，手写二进制对齐 rc-input；`Error::Protocol` 直接复用 rc-types。）

## 与后续卡的接口

- **T14 会话状态机**：`NetEvent::Reliable` → `ControlMessage::decode` 分发；Hello/Welcome/Bye 驱动 `Init→Connected→Streaming→Closed`；version/脏消息错误直接按 `is_fatal` 处理。
- **T15 探测与码率**：ProbePing/Pong、SetBitrate、BwProbeRequest/Data 五个消息已就绪，T15 只做算法（RTT/偏移/估速）与 `HostPipeline::set_bitrate` 转发，不再动线格式。
- **T16 IDR 可靠通道**：IdrRequest（含 `IdrReason::WatchdogStall`）与 IdrAck 的 reason/frame_seq 字段已就绪，T16 做去抖与超时复核。
- **T17 重连**：ReconnectRequest{resume}、Bye(reason=Refresh)、Welcome.host_flags 的 bit0/bit1 已在线格式就绪，T17 做保持窗口与退避。
- **T19 鉴权 / T20 剪贴板**：增量添加 0x0D/0x0E/0x0F 变体——号位已冻结，只加不改；Hello.auth_method=Password 字段已备。
- **号位冻结承诺**：本卡发布的 15 个 `TYPE_*` 常量是公开线格式承诺，任何后续卡不得改号、不得复用已占号。
