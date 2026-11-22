# T16 M2-3 IDR 可靠通道完善

> client 停滞看门狗 + IdrRequest 去抖/超时复核 + IdrAck 按 frame_seq 关联确认 + 进程内丢包恢复演练。

## 背景（为什么做这个）

媒体走 QUIC datagram 是「不可靠的可靠」：丢包宁可丢帧也不等待（T09）。丢帧的代价是
**解码参考链断裂后的画面停滞**——P 帧缺一片整帧作废（rc-pipeline/controller.rs:105 的
重组语义），要等下一个 IDR 才能恢复；而 IDR 本身也走 datagram，**同样会被丢**
（03-protocol.md §2.3：IDR 丢失 → 立即请求重发；分片级 NACK 属 M3-2，本卡不做）。

恢复闭环缺三块：① client 发现停滞（>500ms 无完整帧）；② 请求不发疯——同一停滞期
在途请求未确认前不能重发；③ host 编出 IDR 后按 frame_seq 确认，client 据此区分
「请求在路上」「IDR 在编」「IDR 数据报被丢了要再要一次」。T14 已交付会话骨架
（`take_keyframe_request → IdrRequest` 基本接线、`IdrRequest → host.request_idr()`），
本卡在其上补齐看门狗、去抖与关联确认，并用进程内模拟丢包做恢复演练。

本卡**纯逻辑 + 进程内测试**：不碰真 QUIC、不需要任何权限。

## 平台要求

**全平台。** 不需要 GPU，不需要真机，不需要 macOS 权限。CI 可跑。

## 前置

- **T13**（proto.rs 线格式）：本卡消费 `ControlMessage::IdrRequest / IdrAck` 与
  `IdrReason`，不改任何线格式；
- **T14**（HostSession/ClientSession 状态机与驱动循环）：本卡修改其驱动循环接线。
  T14 与本卡同批交付，**接线细节以 T14 实际代码为准**，本卡契约只约定行为。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/watchdog.rs` | 新建 | `DeliveryTracker` / `StallWatchdog` / `IdrController` / `idr_frame_seqs` |
| `crates/rc-session/src/lib.rs` | 修改 | `pub mod watchdog;`（一行） |
| `crates/rc-session/src/client_session.rs` | 修改 | 驱动循环接看门狗/去抖/ack 关联（见「必须做到」W 接线节） |
| `crates/rc-session/src/host_session.rs` | 修改 | tick 产出数据报后扫 IDR 发 `IdrAck` |
| `crates/rc-session/tests/idr_recovery.rs` | 新建 | 丢包恢复演练（进程内假后端管线 + 可脚本丢包的数据报通道） |

不新增任何依赖（rc-session 既有依赖足够，见文末）。

## API 契约

以下签名是本卡**新增**的公开 API（全部在 `watchdog.rs`）。所有时间参数都是**注入的**
会话钟微秒（`u32`，与 `rc_types::Timestamp` 同宽，铁律 4）：本模块内部禁止
`Instant::now()` / `SystemTime` / `thread::sleep`。

```rust
//! crates/rc-session/src/watchdog.rs
use rc_types::{FrameSeq, Result};

// ── 完整帧跟踪 ──────────────────────────────────────────────

/// client 侧完整帧到达跟踪器：只记元数据（frame_seq / packet_idx / 是否 IDR），
/// **不存负载**。
///
/// 为什么会话层需要它：`ControllerPipeline::push_datagram` 只返回 `Ok(bool)`
/// （controller.rs:105），交付帧的 seq / kind 是私有状态——看门狗的
/// 「最近完整帧时刻」与 IdrAck 的按 frame_seq 关联确认都拿不到。本类型在
/// 会话层顺带解析数据报头部补上这个可见性，**管线 API 零改动**（架构红线）。
///
/// 铁律 1：在途残帧表上限 2（对齐 `ControllerPipeline` 的 `MAX_PARTIALS`，
/// controller.rs:56），超出淘汰最旧；完成历史是有界环形（容量 8），只服务
/// 「IDR 帧先到、ack 后到」的关联查询，不做无限留存。
pub struct DeliveryTracker { /* 私有 */ }

/// 最近一次完整到达的帧（或完成历史中的一条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveredFrame {
    pub seq: FrameSeq,
    /// 是否 IDR（来自包头 flags，rc-proto lib.rs:283）
    pub is_idr: bool,
    /// 凑齐 packet_cnt 的时刻（会话钟，注入值）
    pub at_us: u32,
}

impl DeliveryTracker {
    pub fn new() -> Self;

    /// 观察一个到达的数据报（会话层在 `push_datagram` 之前对同一字节调用；
    /// 内部经 `rc_pipeline::host::datagram_header` 解析，host.rs:624，
    /// 不引入 rc-proto 直接依赖）。同一 (seq, idx) 重复到达幂等。
    /// 凑齐 packet_cnt 时记入完成历史并推进 `last_complete`。
    /// 头部非法返回 `Err`（与管线自己的 push_datagram 判定一致，交给
    /// T14 的错误策略处理）。
    pub fn observe(&mut self, datagram: &[u8], now_us: u32) -> Result<()>;

    /// 最近一次完整帧（任意类型）。看门狗的数据源。
    pub fn last_complete(&self) -> Option<DeliveredFrame>;

    /// 查询指定 seq 是否已完整到达（ack 后到时的关联确认）。
    /// 已滚出环形历史的旧帧返回 `None`（调用方按「未到达」处理即可——
    /// 500ms 级窗口内滚出 8 帧历史意味着流已恢复，闭环无意义）。
    pub fn completed(&self, seq: FrameSeq) -> Option<DeliveredFrame>;

    /// 残帧清理：超时未凑齐的帧移出在途表。时间比较用
    /// `u32::wrapping_sub`（对齐 rc-net 重组层的回绕免疫写法，
    /// reassemble.rs:133-140）。
    pub fn gc(&mut self, now_us: u32);
}

// ── 停滞看门狗 ──────────────────────────────────────────────

/// 看门狗阈值（微秒）：与 rc-net 重组层的残帧放弃线 `FRAME_STALE_US`
/// （reassemble.rs:36，500_000）**刻意对齐**——残帧被判死的同一时刻，
/// 看门狗接管恢复。改一个必须同步另一个（见已知坑 5）。
pub const DEFAULT_STALL_US: u32 = 500_000;

/// client 停滞看门狗：超阈值无完整帧 → 触发一次（会话层把它转成
/// `IdrController::request(IdrReason::WatchdogStall)`，是否真发请求由后者裁决）。
pub struct StallWatchdog { /* 私有 */ }

impl StallWatchdog {
    /// `stall_after_us` 传 [`DEFAULT_STALL_US`]。
    pub fn new(stall_after_us: u32) -> Self;

    /// 进入 Streaming 状态时锚定基线（T14 状态机转入时调用一次）。
    /// 首帧迟到（如首个 IDR 被丢）同样能触发恢复——不要等首帧到达才武装。
    pub fn arm(&mut self, now_us: u32);

    /// 完整帧到达（DeliveryTracker 凑齐时转发）。
    pub fn on_frame_complete(&mut self, now_us: u32);

    /// 每拍驱动：距最近完整帧（或 arm 基线）超过阈值返回 true。
    /// 触发后**自锚定**（基线推进到触发时刻）：同一段停滞每 500ms 至多
    /// 报一次，直到下一帧到达重新武装。
    pub fn poll(&mut self, now_us: u32) -> bool;

    /// 累计触发次数（报告 `watchdog_fires` 数据源，JSON 组装归 T18）。
    pub fn fires(&self) -> u64;
}

// ── IdrRequest 去抖 / IdrAck 关联确认 ───────────────────────

/// 超时复核余量（微秒）：timeout = rtt_us + margin。
/// 覆盖单帧周期（60fps ≈ 16.7ms）+ 编码（~10ms 量级）+ 调度余量。
pub const DEFAULT_ACK_MARGIN_US: u32 = 50_000;

/// 重发最小间隔地板（微秒）：静态画面下（铁律 2 合法零帧）请求-超时-重发
/// 循环必然存在，此地板把它的频率压到 ≤10 次/秒（每条 2 字节 reliable 消息）。
pub const DEFAULT_MIN_RETRY_INTERVAL_US: u32 = 100_000;

/// IdrController 配置。
#[derive(Debug, Clone, Copy)]
pub struct IdrControllerConfig {
    pub ack_margin_us: u32,          // 默认 DEFAULT_ACK_MARGIN_US
    pub min_retry_interval_us: u32,  // 默认 DEFAULT_MIN_RETRY_INTERVAL_US
}

impl Default for IdrControllerConfig { /* 上述默认值 */ }

/// `poll` 的产出：会话层应发出一条 `ControlMessage::IdrRequest { reason }`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendIdrRequest {
    /// 0=DecoderNeedKeyframe（解码器，T14 接线）、1=WatchdogStall（本卡
    /// 看门狗）；2=ReconnectRecovery 属 T17，本卡不发送（变体名用 T13
    /// 全名，不用 NeedKeyframe/Stall/Reconnect 简称）。
    pub reason: crate::proto::IdrReason,
    /// 第几次尝试（1 起）。测试与报告用。
    pub attempt: u8,
}

/// 本卡计数（client 报告 idr_requests / idr_acks / watchdog_fires 的数据源）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdrStats {
    pub requests_sent: u64,
    /// 按原因分计数，下标 = IdrReason 线值（0/1/2）
    pub requests_by_reason: [u64; 3],
    pub retries: u64,
    pub acks_received: u64,
    /// 「等待中的 IDR 帧完整到达」而闭环的次数
    pub resolved_by_idr: u64,
}

/// IdrRequest 去抖 / IdrAck 关联确认状态机（client 侧）。
///
/// 状态（私有）：
/// `Idle` →（触发）→ `InFlight{sent_at, attempt}`（已发请求等 ack）
/// →（on_ack(seq)）→ `AwaitFrame{seq, since, attempt}`（等该 IDR 帧完整到达）
/// →（on_frame_complete 且 seq 匹配且 is_idr）→ `Idle`。
///
/// - 两条超时路径都回到 `InFlight` 重发（attempt+1）：等 ack 超
///   `rtt + margin`；ack 到了但对应 IDR 帧超时未到（**IDR 数据报被丢**）。
///   重发额外受 `min_retry_interval_us` 地板约束。
/// - **在途（InFlight/AwaitFrame）期间一切新触发被吞**（去抖红线 W3）；
///   闭环时清空被吞记录——刚到的 IDR 已覆盖所有原因，持续性需求
///   （解码器仍请求 DecoderNeedKeyframe）下一拍会经 `take_keyframe_request` 自然重触发
///   （controller.rs:214 取走即清、解码器未解出 IDR 前会再置位）。
/// - IDR 走 datagram、ack 走 reliable，**两通道无序**：ack 晚于帧到达是
///   常态，`on_ack` 先查 `DeliveryTracker::completed` 命中即立即闭环。
pub struct IdrController { /* 私有 */ }

impl IdrController {
    pub fn new(cfg: IdrControllerConfig) -> Self;

    /// 外部触发（解码器 DecoderNeedKeyframe：T14 的直接发送接线改为经此；
    /// 看门狗 WatchdogStall：本卡）。在途时记入待清空记录，不产生发送。
    pub fn request(&mut self, reason: crate::proto::IdrReason, now_us: u32);

    /// 收到 `ControlMessage::IdrAck { frame_seq }`。重复/过期 ack 幂等无副作用。
    pub fn on_ack(&mut self, frame_seq: FrameSeq, now_us: u32);

    /// 完整帧到达（DeliveryTracker 凑齐时转发）。
    pub fn on_frame_complete(&mut self, frame: DeliveredFrame);

    /// 每拍超时复核与**唯一发送出口**（`request` 只记意图，发送全部经此）。
    /// `rtt_us` 注入自 `NetStats::rtt_us`（quic.rs:884，quinn 路径 RTT；
    /// 不依赖 T15 的 ProbePing——本卡前置只有 T14）。
    pub fn poll(&mut self, now_us: u32, rtt_us: u32) -> Option<SendIdrRequest>;

    pub fn stats(&self) -> IdrStats;
}

// ── host 侧 ─────────────────────────────────────────────────

/// 从 `HostPipeline::tick` 一拍产出的数据报里扫出 IDR 帧序号（保序去重：
/// 一个跨多片的 IDR 只出一个 seq）。host 会话在把这批数据报
/// `send_datagram` 完之后、**同一循环体内**对每个 seq 发一条
/// `ControlMessage::IdrAck { frame_seq }`（经 `send_reliable`）——
/// 「IDR 帧编码完成送出数据报的同一 tick 发出」的落地。
/// 纯函数，独立可测。
pub fn idr_frame_seqs(datagrams: &[Vec<u8>]) -> Vec<FrameSeq>;
```

线格式与语义按 T13 已交付的 `proto.rs`（架构控制消息表）：

- `0x03 IdrRequest`（client→host）：`reason u8`（0=DecoderNeedKeyframe、1=WatchdogStall、2=ReconnectRecovery）；
  host 收到即 `host.request_idr()`（host.rs:462，`pending_idr` 粘住直到真编出 IDR，
  host.rs:90-94——静态画面下请求不蒸发，下次内容变化兑现）。
- `0x04 IdrAck`（host→client）：`frame_seq u64`；client 用于去抖与超时复核。

### 会话接线约定（改 client_session.rs / host_session.rs）

client 驱动循环每拍（顺序建议，接 T14 已有循环）：

1. `poll` 传输事件：`Datagram` → `tracker.observe(d, now)` **再** `controller.push_datagram(d, now)`；
   `Reliable` → proto 解码，`IdrAck` → `idr.on_ack(seq, now)`；
2. `tracker` 凑齐完整帧 → `watchdog.on_frame_complete(now)` + `idr.on_frame_complete(frame)`；
3. `controller.take_keyframe_request()` 为真 → `idr.request(IdrReason::DecoderNeedKeyframe, now)`
   （**替换** T14 的直接发送）；
4. `watchdog.poll(now)` 为真 → `idr.request(IdrReason::WatchdogStall, now)`；
5. `idr.poll(now, transport.stats().rtt_us)` 返回 `Some(req)` →
   `send_reliable(IdrRequest{reason: req.reason})`；
6. `tracker.gc(now)`。

host 驱动循环：`tick` 产出数据报逐条 `send_datagram` 后调 `idr_frame_seqs(&datagrams)`，
逐 seq `send_reliable(IdrAck)`。收到 `IdrRequest` → `host.request_idr()`（T14 已接，不改）。

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| W1 | 看门狗阈值 500ms 与 rc-net `FRAME_STALE_US` 对齐；**时间全部注入**（模块内禁墙钟/sleep，铁律 4） | 不可测 / 阈值漂移 |
| W2 | 停滞恢复只发 `IdrRequest(reason=WatchdogStall)`，经 reliable 控制流；绝不直接改 `ControllerPipeline` 内部状态 | 分层污染 |
| W3 | **在途请求未确认（未收 ack 且未见对应 IDR 帧）前不重发**；在途期间新触发一律吞掉 | 请求风暴（弱网下雪崩） |
| W4 | 超时复核窗口 = `rtt_us + ack_margin_us`（rtt 注入自 `NetStats`）；重发再受 `min_retry_interval_us` 地板 | 静态画面下请求每拍重发 |
| W5 | host 在 IDR 数据报送出的**同一拍**发 `IdrAck{frame_seq}`；client 按「该帧完整到达且 is_idr」闭环，ack 早到/晚到两个方向都正确 | 关联语义错误 → 去抖失效 |
| W6 | `DeliveryTracker` 在途残帧 ≤2（铁律 1，对齐 `MAX_PARTIALS`），完成历史有界（8） | 无界内存 |
| W7 | 所有 elapsed 用 `u32::wrapping_sub`（会话钟 71.6 分钟回绕；对齐 rc-net 修复先例 reassemble.rs:133-140） | 回绕点状态机冻结 |
| W8 | 管线 API 零改动：不碰 rc-pipeline / rc-net / rc-types / rc-proto 任何文件；控制线格式用 T13 proto.rs，不自创 | 契约漂移 / 破坏已过 Gate 的稳定层 |
| W9 | host 收 `IdrRequest` 只调 `host.request_idr()`（复用 `pending_idr` 粘住语义），不自行驱动编码器、不自行编 IDR | 双机制竞争 |
| W10 | 上节 6+2 条接线全部落在 T14 的 client/host 会话驱动循环里（不只是独立组件） | 组件正确但会话不通 |

## 不要做什么

- ❌ **NACK / 分片级重传**（M3-2）：不做缺包报告，恢复手段只有「要新 IDR」。
- ❌ **FEC 调参**：`loss_rate` 只读不写，不动 rc-fec 冗余率。
- ❌ 不发送 `IdrReason::ReconnectRecovery(=2)`（重连恢复路径属 T17）；proto 枚举值只被引用。
- ❌ 不碰 rc-pipeline（`set_bitrate` 是 T15 的）；本卡对管线的交互只有既有 public API
  （`tick` / `push_datagram` / `take_keyframe_request` / `request_idr` / `datagram_header`）。
- ❌ 不改 T13 线格式、不加新控制消息类型。
- ❌ 不做跨进程 / localhost 真 QUIC 实测（T18 的 rc-cli host/client 与 `tests/localhost.rs`）；
  本卡验收全在进程内。
- ❌ 不建 testkit.rs 的 ScriptedCapture/RecordingInjector（T18 交付）；演练 harness 只放
  `tests/idr_recovery.rs`。
- ❌ 不引新依赖（quinn 等一概不加；rc-session 既有依赖足够）。

## 测试清单

**单元测试（`watchdog.rs` 内 `#[cfg(test)]`，全部注入时间、确定性）：**

| 测试名 | 验证 |
|---|---|
| `tracker_completes_frame_only_when_all_packets_arrive` | 多片乱序 + 重复投递幂等；凑齐才进完成历史，`last_complete` 推进 |
| `tracker_bounded_inflight_drops_oldest` | 3 个并发残帧 → 最旧被逐，在途表 ≤2（W6/铁律 1） |
| `tracker_gc_evicts_stale_partials_across_wrap` | 构造跨 u32::MAX 的时刻，`gc` 用 wrapping 比较不误杀/不漏杀（W7） |
| `watchdog_fires_after_500ms_without_complete_frame` | 499ms 不触发、501ms 触发（阈值边界） |
| `watchdog_refires_only_after_next_window` | 同一段停滞 500ms 一次；持续停滞按 500ms 节拍重复（自锚定） |
| `watchdog_arms_at_streaming_start` | arm 后首帧 >500ms 才到也触发（首 IDR 丢失可恢复，W1） |
| `idr_controller_dedups_inflight_request` | 在途时第二个触发（不同 reason）不产生发送；闭环后清空记录（W3） |
| `idr_controller_resolves_on_acked_frame_arrival` | ack 先到→帧到达闭环；**帧先到→ack 到达立即闭环**（两通道无序，W5） |
| `idr_controller_retries_when_ack_times_out` | rtt+margin 无 ack → 恰好重发一次，attempt 递增、计入 `retries` |
| `idr_controller_retries_when_idr_datagram_lost` | ack 到了但对应帧超时未到（IDR 被丢）→ 重发（W5 关键路径） |
| `idr_controller_retry_floor_bounds_static_storm` | 持续停滞 2s（模拟静态画面）：`requests_sent` 受地板约束 ≤ 2s/100ms + 1（W4） |
| `idr_frame_seqs_scans_split_datagrams` | `rc_proto::split_frame` 多片 IDR → 单 seq；P 帧不产出；多帧混排保序去重 |

**集成测试（`tests/idr_recovery.rs`，进程内丢包恢复演练）：**

harness：真实 `HostPipeline::with_backends_paced`（假采集+假编码，60fps 节流）+
`ControllerPipeline::with_backends`（假解码+假呈现），驱动形态**照抄**
`rc-pipeline/tests/m1.rs:100-178`（墙钟会话钟 + drive 循环 + `set_clock`）；
数据报经可脚本化的 `LossyGate`（按时间窗丢弃 `Datagram`，reliable 通道进程内
直连不丢，替身形态参考 `rc-input/channel.rs:195-223` 的 MockTransport）；
client 侧循环复刻上节接线（tracker/watchdog/idr），host 侧收 IdrRequest →
`request_idr`、`idr_frame_seqs` → IdrAck 回环。

| 测试名 | 验证 |
|---|---|
| `loss_drill_recovers_via_watchdog_idr` | 正常流出帧后开 1s 丢包窗（全部数据报丢弃）：500–700ms 间 watchdog 触发 → IdrRequest(WatchdogStall) 经 reliable 到 host → host 编出 IDR → client 完整收到并上屏；断言窗口内 `requests_sent(reason=WatchdogStall)` ≤ 2（去抖生效）、窗口结束后 ≤700ms 恢复上屏、`resolved_by_idr ≥ 1` |
| `loss_drill_sustained_loss_keeps_rate_bounded` | 持续丢包 2s：请求总数有界（500ms 看门狗节拍 × 地板双重约束，≤5）；恢复后 displayed 继续增长 |
| `single_frame_loss_does_not_fire_watchdog` | 只丢一帧的数据报（<500ms 空隙）：`watchdog.fires() == 0`（不误报），后续帧正常上屏 |

（真机 / 权限测试：无。本卡不需要 `#[ignore]` 项。）

## 验收命令

```bash
cargo test -p rc-session
cargo test --workspace
cargo clippy -p rc-session --all-targets -- -D warnings
cargo fmt --check
# W8：下游稳定层零改动（应无任何输出）
git diff --stat crates/rc-pipeline crates/rc-net crates/rc-types crates/rc-proto
```

## 验收标准

- [ ] `cargo test --workspace` 全绿；本卡新增 ≥15 个测试（12 单元 + 3 演练）
- [ ] 演练：1s 全丢包窗口触发看门狗且**经 IdrRequest→IDR 恢复上屏**，窗口结束后恢复 ≤700ms
- [ ] 同一停滞窗口内 `IdrRequest(WatchdogStall)` 恰发 ≤2 条（在途去抖实测，不是纸面）
- [ ] 持续丢包 2s 请求总数 ≤5（有界），无逐拍重发
- [ ] 丢单帧（<500ms）不触发看门狗（无误报）
- [ ] `DeliveryTracker` 在途表 ≤2（铁律 1）
- [ ] rc-pipeline / rc-net / rc-types / rc-proto 的 `git diff` 为空
- [ ] 无 `unsafe`、无新依赖、clippy `-D warnings` 0 警告
- [ ] 完成后按 AGENTS.md §9 回写 TASK.md（§0/§12/§13，测试数与演练数字带场景）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-pipeline/src/host.rs`](../../../crates/rc-pipeline/src/host.rs) | `tick`/`request_idr`/`pending_idr` 粘住语义（:90-94/:367/:462）、`datagram_header`（:624） |
| [`rc-pipeline/src/controller.rs`](../../../crates/rc-pipeline/src/controller.rs) | `push_datagram` 返回语义（:105）、`take_keyframe_request` 取走即清（:214）、`MAX_PARTIALS`（:56） |
| [`rc-net/src/reassemble.rs`](../../../crates/rc-net/src/reassemble.rs) | `FRAME_STALE_US`（:36）、wrapping gc 写法（:133-140）、并发上限纪律（:33） |
| [`rc-pipeline/tests/m1.rs`](../../../crates/rc-pipeline/tests/m1.rs) | 假后端 + 墙钟会话钟 + drive 循环（:100-178）——演练 harness 照抄 |
| [`rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | MockTransport 测试替身与批内错误隔离风格（:195-223） |
| [`rc-loopback/src/lib.rs`](../../../crates/rc-loopback/src/lib.rs) | `run_real` 驱动循环（:336-423）——本卡把 `AtomicBool` IDR 标志（:365/:391）换成 IdrRequest/IdrAck 消息化的位置 |
| 本 crate T13/T14 已交付代码 | proto 编解码、会话驱动循环与错误策略；接线以其实际代码为准 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **静态画面（铁律 2）= 合法零帧**：看门狗必然周期性触发，且 host 因无采集帧编不出 IDR → 无 ack → 超时复核循环存在。这不是回归：`pending_idr` 粘住（host.rs:90-94），下次内容变化立即兑现 IDR；用 `min_retry_interval_us` 地板压频率（≤10 条/秒、每条 2 字节 reliable），报告把 `watchdog_fires` 单独计数，勿与丢包触发混算 |
| 2 | `ControllerPipeline` 不暴露交付帧的 seq/kind（partials 私有，push_datagram 只回 bool）——不要为此改管线 API；`DeliveryTracker` 顺带解析头部补可见性（解析 <1µs，与管线内部重复解析一次是接受的代价） |
| 3 | u32 会话钟 71.6 分钟回绕：elapsed 一律 `wrapping_sub`。rc-net 重组/FEC 的 gc 曾在这里栽过（已修，reassemble.rs:133-140），照抄其写法；饱和刷新（Bye(Refresh)）是 T17 的事，本卡只要求回绕免疫 |
| 4 | **IDR 与 ack 两通道无序**：IDR 走 datagram、ack 走 reliable，帧先到 ack 后到是常态；`on_ack` 必须先查完成历史命中即闭环，否则去抖状态挂死到超时 |
| 5 | 500ms 与 `FRAME_STALE_US` 是**刻意对齐**的一对：残帧判死与看门狗接管同刻。只改一个会打开「帧未判死就请求」或「判死了没人管」的窗口；要改一起改并注明 |
| 6 | `NetStats.rtt_us` 握手初期可能为 0（quic.rs:884 路径 RTT 未收敛）→ 超时退化为纯 margin（50ms），localhost/内网足够；**不要**为更准的 RTT 提前依赖 T15 的 ProbePing（本卡前置只有 T14） |
| 7 | 首帧必是 IDR（`no_idr_yet`，host.rs:92）：看门狗 arm 基线放 Streaming 进入时刻而非首帧到达——否则「首个 IDR 被丢」这一最坏情况没有恢复路径 |
| 8 | 同一 IDR 跨多片（`split_frame`，rc-proto lib.rs:310）：host 扫 ack 必须按 frame_seq 去重，否则一个 30 片 IDR 发 30 条 ack；client 的 tracker 观察也要按 (seq, idx) 幂等去重 |
| 9 | 演练测试假采集**必须节流**（`with_backends_paced` pace=true）：不节流假后端瞬时洪灌，帧数变竞态（m1.rs:169-173 注释明说）；墙钟驱动下断言用宽窗（±100ms 量级）避免定时器毛刺 |
| 10 | 丢包窗内到达的 IDR 也会被丢：看门狗 500ms 自锚定节拍保证持续丢包期间每 500ms 重新要一次，窗口一关即恢复——演练断言「窗口结束后 ≤700ms 恢复」依赖这个节拍，别把它改成一次性触发 |

## 依赖

```toml
# crates/rc-session/Cargo.toml（T13 已建；本卡零新增）
[dependencies]
rc-net      = { path = "../rc-net" }      # Transport / NetEvent / NetStats
rc-pipeline = { path = "../rc-pipeline" } # Host/ControllerPipeline、datagram_header
rc-input    = { path = "../rc-input" }    # T14 已引入
rc-types    = { path = "../rc-types" }    # FrameSeq / Result（铁律 5：不改它）
rc-trace    = { path = "../rc-trace" }    # T14/T15 已引入则沿用
```

## 与后续卡的接口

- **T17 断线重连**：重连后强制 IDR 复用 `IdrController::request(IdrReason::ReconnectRecovery)`
  （发送路径 T17 补）；饱和刷新（Bye(Refresh)）后 tracker/watchdog/idr 随会话重建
  （时钟归零，无需跨纪元迁移状态）。
- **T18 报告**：`IdrStats` 与 `StallWatchdog::fires()` 是 client 报告
  `idr_requests / idr_acks / watchdog_fires` 的数据源；JSON 组装与 gate 判定归 T18
  （口径对齐 rc-cli/src/cmd/loopback.rs:1-8 的 stderr/JSON 分流）。
- **M3-2 NACK**：届时分片级重传接管「缺 1-2 片」的场景，本卡状态机仍是
  「放弃恢复」的最后一级（NACK 放弃 → IdrRequest 入口不变）。
