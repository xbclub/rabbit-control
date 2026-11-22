# T15 探测与初始码率（M2-10）

> ProbePing/Pong（RTT + 时钟偏移，兼心跳）、BwProbe 数据报带宽粗测 v0、SetBitrate → `HostPipeline::set_bitrate` 转发、初始码率设定链（协商值 → 探测值 → `encoder.set_bitrate`）。

## 背景（为什么做这个）

双机会话两端时钟是**独立纪元**（各自会话起点归零），路径带宽未知。按架构结论，client 报告不做跨进程 `merge_traces`：e2e = controller trace 的 Presented（client 钟）− 媒体包头 capture_ts（host 钟）+ **握手探测得到的时钟偏移**；没有偏移换算，client 报告的数字就是垃圾。RTT 同时是 T16 看门狗「RTT+余量」复核的输入。

初始码率若只信 Hello 协商值（`PipelineConfig` 默认 8 Mbps，rc-pipeline/src/lib.rs:82），WiFi 下会拥塞丢帧、快链路上浪费画质。BwProbe 用一次数据报突发给出粗估，client 按 `clamp(估速×0.7, 2Mbps, host 上限)` 下发 `SetBitrate`。本卡是 v0 heuristic：**M3-3 拥塞控制接管后整条探测链废弃替换**，文档与代码注释必须写死这一定位。

前置：T13（proto.rs 全部控制消息线格式）与 T14（HostSession/ClientSession 状态机、Hello/Welcome 协商、驱动循环）已交付。本卡在两者之上**纯增量**接线，唯一的 rc-pipeline 改动是 `HostPipeline::set_bitrate`。

## 平台要求

**全平台。** 不需要真机、不需要 GPU、不需要任何 macOS 权限（全部假后端离线测试）。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-session/src/probe.rs` | 新建 | `ProbeEngine`（RTT/偏移/心跳）、`pong_for`、`bw_probe_chunks`/`parse_bw_datagram`、`BwEstimator`、`initial_bitrate_from`、常量 |
| `crates/rc-session/src/host_session.rs` | 修改 | 控制分发新增 3 个臂（ProbePing→Pong、BwProbeRequest、SetBitrate）+ `current_bitrate_bps` |
| `crates/rc-session/src/client_session.rs` | 修改 | `pump_probes`/`on_pong`/`start_bw_probe`/`on_bw_datagram`/`host_to_local_us`/`probe_snapshot` + `ProbeSnapshot` |
| `crates/rc-pipeline/src/host.rs` | 修改 | **本轮唯一 rc-pipeline 改动**：新增 `set_bitrate`（编码线程命令通道） |
| `crates/rc-session/tests/probe_session.rs` | 新建 | 进程内双会话集成测试（自带最小 `LoopTransport`，不依赖 T18 的 testkit） |
| `crates/rc-pipeline/tests/bitrate.rs` | 新建 | `set_bitrate` 系列回归测试 |

## API 契约

> 线格式（0x05–0x09）由 T13 交付，本卡**不改一字节**；下文引用的 `ProbePing`/`ProbePong`/`SetBitrate`/`BwProbeRequest`/`BwProbeData` 以 T13 实际交付的 proto 类型名为准（命名不一致时做等价适配，语义与线格式不得动）。probe.rs 内部类型与 `HostPipeline::set_bitrate` 以本卡为准。

```rust
// ── crates/rc-session/src/probe.rs（新建）──────────────────────

/// ProbePing 发送间隔（微秒）：1s，兼应用层心跳（铁律 2：保活走控制通道）。
/// 与 rc-net 的 QUIC keepalive（quic.rs:95-96，1s）正交：那个保连接，
/// 这个产出 RTT/偏移并供 T16 看门狗复用——两者并存不是重复。
pub const PROBE_INTERVAL_US: u64 = 1_000_000;

/// 单次探测判超时（微秒）：3s 无 Pong 记丢失（≈3 个间隔的余量）。
pub const PROBE_TIMEOUT_US: u64 = 3_000_000;

/// RTT/偏移取中位数的样本窗口长度。
pub const PROBE_MEDIAN_WINDOW: usize = 8;

/// 带宽粗测总字节上限（协议 0x08 的 total_bytes 上限）。
pub const BW_PROBE_MAX_TOTAL_BYTES: u32 = 256 * 1024;

/// 单块 padding 字节数。探测数据报 = 1 字节类型 0x09 + probe_id u32 LE
/// + chunk_idx u32 LE + padding = 9 + 1024 = 1033 字节，恒 ≤
/// `rc_net::quic::max_media_payload(1200)` = 1120（N2 同源预算，不越界）。
pub const BW_PROBE_CHUNK_PAYLOAD: usize = 1024;

/// 带宽探测结算超时（微秒）：2s 未凑齐按实收结算。
pub const BW_PROBE_SETTLE_TIMEOUT_US: u64 = 2_000_000;

/// 首末块到达间隔低于此值（微秒）= 测量退化（localhost 同拍到达），
/// 估速记 None、保持协商值——不做除零、不产出 ∞。
pub const BW_PROBE_MIN_SPAN_US: u64 = 1_000;

/// 初始码率下限（bps）。
pub const INITIAL_BITRATE_MIN_BPS: u64 = 2_000_000;

/// 估速 → 初始码率折扣因子（留 30% 余量给控制流与突发）。
pub const INITIAL_BITRATE_FACTOR: f64 = 0.7;

/// 一次成功探测的样本。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeSample {
    pub probe_id: u32,
    /// 往返时延（微秒）= t2 − t0（发起方时钟）。
    pub rtt_us: u64,
    /// 时钟偏移（微秒，有符号）= 对端时钟 − 本端时钟（NTP 中点法：
    /// t1 − (t0+t2)/2）。对端时间戳换算到本端时间轴：local = remote − offset_us。
    pub offset_us: i64,
}

/// 发起侧探测引擎（client 持有）。时间全部由调用方注入（铁律 4），
/// 不读任何系统时钟。
pub struct ProbeEngine { /* 私有：next_id、在途表、样本窗口、计数器 */ }

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProbeStats {
    pub rtt_us_median: Option<u64>,
    pub offset_us_median: Option<i64>,
    pub pings_sent: u64,
    pub pongs_received: u64,
    pub probes_lost: u64,
}

impl ProbeEngine {
    pub fn new() -> Self;

    /// 探测泵（驱动循环每拍调用，now_us = 本端会话钟 u64）：
    /// 距上次 ProbePing ≥ PROBE_INTERVAL_US 时产出一条新探测
    /// （probe_id 从 1 递增，u32 回绕允许）；同时把挂起 ≥ PROBE_TIMEOUT_US
    /// 的探测结算为丢失。返回本拍应发送的 Ping（多数拍为 None）。
    pub fn pump(&mut self, now_us: u64) -> Option<ProbePing>;

    /// 消费一条 ProbePong（now_us = 本端收到时刻 t2）。
    /// probe_id 不在途（迟到/伪造/重复）→ 忽略并返回 None。
    pub fn on_pong(&mut self, pong: &ProbePong, now_us: u64) -> Option<ProbeSample>;

    /// 对端时间戳 → 本端时间轴（用窗口中位偏移换算）。
    /// 无样本返回 None——报告侧必须把 None 当「无数据」，不是当 0。
    pub fn to_local_us(&self, remote_us: u64) -> Option<u64>;

    pub fn stats(&self) -> ProbeStats;
}

/// 应答侧（host）无状态应答：echoed_clock_us 原样回显 t0，
/// responder_clock_us 打本端会话钟 t1。host 不持 ProbeEngine
/// （v0 只由 client 发起；消息设计为双向，host 主动发起留待后续）。
pub fn pong_for(ping: &ProbePing, responder_now_us: u64) -> ProbePong;

/// host 侧：把 BwProbeRequest 展开成探测数据报块序列（纯函数）。
/// total_bytes == 0 或 > BW_PROBE_MAX_TOTAL_BYTES → Err(InvalidConfig)。
/// 块数 = ceil(total / BW_PROBE_CHUNK_PAYLOAD)，末块 padding 取余量；
/// 每块字节布局见 BW_PROBE_CHUNK_PAYLOAD 文档。
pub fn bw_probe_chunks(probe_id: u32, total_bytes: u32) -> Result<Vec<Vec<u8>>>;

/// 解析一条探测数据报（client 的 Datagram 分发用）。
/// 首字节非 0x09 → Err(Protocol)。padding 长度不校验（估速按实收算）。
pub fn parse_bw_datagram(buf: &[u8]) -> Result<BwProbeData>;

/// client 侧带宽估计器（每次会话一次探测；时间注入）。
pub struct BwEstimator { /* 私有 */ }

/// 一次带宽粗测的结算结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BwOutcome {
    pub probe_id: u32,
    /// 估速（bps）= 实收 padding 字节 × 8 / (末块到达 − 首块到达)。
    /// None = 退化（间隔 < BW_PROBE_MIN_SPAN_US）或零实收。
    /// 口径：只计 padding，不含 9 字节探测头（与 NetStats.bytes_recv 的
    /// 传输层口径区分，报告侧写清）。
    pub estimate_bps: Option<u64>,
    pub received_bytes: u64,
    pub expected_bytes: u64,
    /// true = 收到最高 chunk_idx 立即结算；false = 超时按实收结算。
    pub completed: bool,
}

impl BwEstimator {
    /// 发起一次探测（expected_chunks 按 bw_probe_chunks 同一分块规则推得）。
    pub fn start(&mut self, probe_id: u32, expected_total_bytes: u32, now_us: u64);

    /// 记录一块到达（按 chunk_idx 去重）。收到最高 chunk_idx 时立即结算。
    pub fn on_chunk(&mut self, data: &BwProbeData, now_us: u64) -> Option<BwOutcome>;

    /// 超时泵：超过 BW_PROBE_SETTLE_TIMEOUT_US 未凑齐按实收结算（一次性）。
    pub fn pump(&mut self, now_us: u64) -> Option<BwOutcome>;

    /// 是否已结算（会话只探测一次，结算后本估计器变空操作）。
    pub fn is_settled(&self) -> bool;
}

/// 估速 → 初始码率：clamp(estimate × INITIAL_BITRATE_FACTOR,
/// INITIAL_BITRATE_MIN_BPS, host_cap)。estimate 为 None → 返回 None
/// （调用方保持协商值）。host_cap < 下限（病态协商）→ 取 host_cap
/// （host 裁定优先，不得超发）。
pub fn initial_bitrate_from(estimate_bps: Option<u64>, host_cap_bps: u32) -> Option<u32>;

// ── client_session.rs 新增（T14 的 ClientSession 之上，纯增量）────

impl ClientSession {
    /// 探测泵（T14 驱动循环每拍调用，now_us = client 会话钟 u64）：
    /// ProbeEngine::pump 到点发 ProbePing（reliable 控制流）+
    /// BwEstimator::pump 超时结算；结算出估速后自动发
    /// SetBitrate { bps: clamp 值, reason: 1（探测） }。
    pub fn pump_probes(&mut self, now_us: u64) -> Result<()>;

    /// 控制分发新增臂（0x06）：ProbePong → ProbeEngine::on_pong。
    pub fn on_pong(&mut self, pong: &ProbePong, now_us: u64);

    /// Welcome 之后发起一次带宽粗测（BwProbeRequest 走 reliable 控制流；
    /// 每次会话仅一次，重复调用为空操作）。不必等探测完成再等首帧——
    /// 协商值已生效，探测是覆盖。
    pub fn start_bw_probe(&mut self, now_us: u64) -> Result<()>;

    /// Datagram 分发新增臂：首字节 0x09 的探测块 → BwEstimator::on_chunk。
    /// 分流规则见「必须做到 B4」——T14 驱动循环里 Datagram 事件先判
    /// rc-proto MAGIC 再决定进管线还是进本方法；若 T14 尚无此分流，
    /// 本卡在分发处补上（这是本卡对 T14 代码的唯一侵入点）。
    pub fn on_bw_datagram(&mut self, buf: &[u8], now_us: u64) -> Result<()>;

    /// host 侧 u32 时间戳 → client 本地时间轴（微秒；无偏移样本返回 None）。
    /// T18 报告的 e2e 与 breakdown.network 换算必须经此（B3）。
    pub fn host_to_local_us(&self, host_us: u32) -> Option<u64>;

    /// 探测快照（T18 嵌入 client 会话报告 JSON 的 session 段）。
    pub fn probe_snapshot(&self) -> ProbeSnapshot;
}

/// 一次会话的探测结论（纯数据，报告/诊断用）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProbeSnapshot {
    pub rtt_us_median: Option<u64>,
    pub offset_us_median: Option<i64>,
    /// v0 粗测估速（None = 未发起 / 退化 / 未结算）。
    pub bw_estimate_bps: Option<u64>,
    /// 探测后实际下发的初始码率（None = 保持协商值）。
    pub initial_bitrate_bps: Option<u32>,
    /// Welcome 裁定的协商码率（对照用）。
    pub negotiated_bitrate_bps: u32,
    pub pings_sent: u64,
    pub pongs_received: u64,
    pub probes_lost: u64,
}

// ── host_session.rs 新增（T14 的 HostSession 之上，纯增量）──────

impl HostSession {
    /// 控制分发新增臂（0x07）：SetBitrate → HostPipeline::set_bitrate 转发。
    /// bps == 0 由管线侧拒绝；reason 仅记 stderr 摘要与统计，不影响行为。
    /// 返回 Err 时按 T14 的错误策略记错（不拆会话）。
    pub fn on_set_bitrate(&mut self, bps: u32, reason: u8) -> Result<()>;

    /// 控制分发新增臂（0x08）：BwProbeRequest → bw_probe_chunks 生成块序列
    /// → 逐块 transport.send_datagram（走 datagram 通道，不占控制流——
    /// 设计师定义）。total_bytes 超限/为零 → Err，不发任何块。
    pub fn on_bw_probe_request(&mut self, req: &BwProbeRequest) -> Result<()>;

    /// 当前生效目标码率（协商值或最近一次 SetBitrate；诊断与测试断言用。
    /// 会话侧自行记账，不查编码线程——编码器在管线线程内）。
    pub fn current_bitrate_bps(&self) -> u32;
}

// ── crates/rc-pipeline/src/host.rs 新增（本轮唯一 rc-pipeline 改动）──

impl HostPipeline {
    /// 动态调整编码目标码率（会话层 SetBitrate 消息的落点，M2-10）。
    ///
    /// 编码器在构造时被 move 进编码线程（本文件 267-268 行），本方法经
    /// **命令通道**转发：纯增量——不改既有 tick / request_idr /
    /// stop_capture / shutdown 的任何签名或时序。命令入队即返回
    /// （非阻塞），编码线程在下一个循环迭代（≤5ms，其 recv_timeout
    /// 周期）应用并调用 `Encoder::set_bitrate`（trait 已有，
    /// rc-encode/src/lib.rs:56，Fake/VT 均实现）。编码器侧拒绝（超范围）
    /// 只进 stderr——对齐本文件 569 行「留 stderr 痕迹便于发现回归」
    /// 的既有风格，不回传：会话层不该为一次码率调整阻塞等待。
    ///
    /// - `bps == 0`：本地拒绝（Err InvalidConfig），不发命令；
    /// - 命令通道（sync_channel(8)，有界）满：Err——调用方失控重发的信号；
    /// - 编码线程已退出（停机后）：Err——调用方按会话错误策略记错。
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()>;
}
```

命令通道内部形态（实现约束，不是公开 API）：

```rust
// host.rs 私有：enum EncCmd { SetBitrate(u32) }，
// 构造时 (enc_cmd_tx, enc_cmd_rx) = sync_channel(8)；
// 编码线程循环每轮迭代先 while let Ok(cmd) = enc_cmd_rx.try_recv() 应用
// （含 5ms 超时醒来后的轮次）；停机排空段同样先排命令再排编码请求。
```

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| B1 | ProbePing 默认 1s 间隔、走 reliable 控制流（铁律 2：保活不占媒体/输入通道）；探测消息绝不出现在 datagram / urgent 通道 | 心跳被媒体 pacing 延迟；违反通道分工 N1 |
| B2 | RTT = t2 − t0、偏移 = t1 − (t0+t2)/2（NTP 中点法）；probe.rs 与会话泵的时间**全部由 now_us 参数注入**，不读 `Instant::now`（铁律 4） | 不可测、CI flaky |
| B3 | client 报告侧一切 host 时间戳必须经 `host_to_local_us` 换算（`push_datagram` 把 host 域 capture_ts 原样写进 trace 的 Captured 标记，controller.rs:176-178）；无偏移样本时报告标「无数据」而不是按 0 偏移出数 | e2e/breakdown 数字是垃圾 |
| B4 | client 的 Datagram 分发先判 `rc-proto` MAGIC（0x31564352，LE 首字节 0x52）：匹配 → 进 `push_datagram`；首字节 0x09 → `on_bw_datagram`；其余记错丢弃。探测块**绝不**进 `push_datagram` | 解析错误污染会话日志甚至中断驱动循环 |
| B5 | 带宽探测：total_bytes ≤ 256KB、单块总长 ≤ `max_media_payload(1200)` = 1120（N2 同源预算）、按 chunk_idx 去重 | IP 分片；重复计数导致估速虚高 |
| B6 | 初始码率 = `clamp(估速×0.7, 2Mbps, Welcome 上限)` 后发 `SetBitrate(reason=1)`；退化/失败/未结算 → **不发**、保持协商值 | 弱网起播即拥塞 |
| B7 | `HostPipeline::set_bitrate` 纯增量：既有签名与时序零改动；编码线程 ≤5ms 内应用；命令通道有界（容量 8，满即报错不无界堆积，对齐 rc-net `URGENT_QUEUE_CAPACITY` 纪律 quic.rs:121） | 触碰已过 M1 Gate 的稳定层无回归保护 |
| B8 | rc-pipeline 被触碰（本轮唯一改动点），必须全套 `cargo test --workspace` 回归 | M1 基线回归无人发现 |
| B9 | 探测钟（u64）与管线钟（u32）**同源同纪元**：会话驱动循环用同一个时钟闭包喂两侧管线与探测泵。70 分钟守卫（T17 的 Bye(Refresh)）保证 u64 < 2^32，帧时间戳 u32 零扩展无损（契约 §1：Timestamp 71.6 分钟饱和，20-crate-contracts.md:93） | 偏移换算引入常数误差，e2e 整体偏移 |
| B10 | v0 带宽探测是**粗 heuristic**：测的是 pacer 天花板（`DEFAULT_INITIAL_PACING_BPS` = 50Mbps，quic.rs:79）与真实路径带宽的较小者，丢包使估速偏低——只用于初始码率，代码注释与本卡文档写死边界，M3-3 接管后废弃 | 被当精确带宽使用，误导调参 |

## 不要做什么

- ❌ 不做自定义拥塞控制、不做根据 loss/rtt 的**动态**码率闭环——只做会话建立时的一次初始校准；闭环属 M3-3。
- ❌ 不动 rc-net（pacer 上限、拥塞信号、通道语义都不改；探测块只是普通 datagram 用户）。
- ❌ 不改 proto.rs 线格式。0x05–0x09 的字节布局是 T13 交付物，本卡只消费；发现线格式问题回 T13 修，不得在本卡私改。
- ❌ 不做 host 主动发起的探测（消息定义为双向，v0 只实现 client→host 方向 + host 无状态应答，不影响线格式）。
- ❌ 不做看门狗（T16）、重连后的重新探测（T17）、报告 JSON 落盘与 gate 判定（T18）——本卡只交 `ProbeSnapshot` 数据与 `host_to_local_us` 换算口。
- ❌ 不给 rc-types 加任何东西（铁律 5）；rc-session 不新增任何 workspace 外依赖。
- ❌ 不做 seamless 码率切换的画质处理（编码器自行处理参考帧连续性；VT `set_bitrate` 已有实现，rc-encode-vt/src/lib.rs:299）。

## 测试清单

**probe.rs 单元测试（内联 `#[cfg(test)]`，全离线确定性、逻辑时钟注入）：**

| 测试名 | 验证 |
|---|---|
| `ping_pump_fires_once_per_interval` | B1/B2：逻辑时钟 0 / 0.9s / 1.0s / 2.0s 推进 → 恰 2 条 ping，probe_id 递增不重复 |
| `pong_yields_rtt_and_ntp_midpoint_offset` | t0=1_000_000、t1=1_000_500、t2=1_002_000 → rtt=2_000µs、offset=−500µs，方向与 `ProbeSample::offset_us` 文档一致 |
| `offset_converts_remote_to_local` | `to_local_us(remote) = remote − offset`；正/负偏移各一例，换算误差为 0 |
| `stale_ping_counts_lost_after_timeout` | 3s 无应答 → `probes_lost` 递增，样本窗口不受污染 |
| `unsolicited_pong_is_ignored` | 未知 probe_id / 重复 pong → None，不 panic 不入窗口 |
| `median_window_uses_recent_samples` | 喂超窗口数量样本（含离群值）→ 中位数取最近 `PROBE_MEDIAN_WINDOW` 个 |
| `pong_for_echoes_and_stamps_responder_clock` | host 侧应答：echoed 原样回显、responder = 注入钟 |

**带宽粗测单元测试（同上）：**

| 测试名 | 验证 |
|---|---|
| `bw_chunks_respect_size_and_count_budget` | B5：256KB → 块数 = ceil(256KB/1024)、单块 ≤1120、padding 总和 = total_bytes |
| `bw_request_over_limit_is_rejected` | total_bytes = 0 / 256KB+1 → Err，不产任何块 |
| `bw_estimate_from_first_last_arrival_span` | 脚本化到达（注入首末间隔）→ 估速 = 实收字节×8/间隔（确定性断言） |
| `bw_estimate_tolerates_missing_chunks` | 丢 25% 块 → 超时路径仍按实收结算出估速（偏保守） |
| `bw_settles_on_last_chunk_immediately` | 收到最高 chunk_idx 即结算，不等超时 |
| `bw_degenerate_span_yields_none` | 首末块同拍（间隔 <1ms）→ estimate = None → 保持协商值（B6） |
| `initial_bitrate_clamps_low_high_and_cap` | est=1Mbps → 2M 下限；est=200Mbps → host 上限；est=None → None；host_cap < 下限 → host_cap |

**进程内双会话集成（`crates/rc-session/tests/probe_session.rs`，假后端、无权限、CI 可跑）：**

自带最小 `LoopTransport`（实现 `rc_net::Transport`：两条 mpsc 对接 datagram/reliable/urgent 三通道，仿 rc-input channel.rs 测试里 MockTransport 的形态但成对互联），**不依赖 T18 的 testkit**。

| 测试名 | 验证 |
|---|---|
| `heartbeat_ping_pong_roundtrip_over_transport` | 双会话对接，逻辑钟驱动 5s → host 收 ≥4 条 ProbePing 且各回 Pong，client 窗口 ≥4 样本；探测消息只出现在 reliable 通道 |
| `bw_probe_and_setbitrate_end_to_end` | client `start_bw_probe` → host 经 datagram 逐块发出 → client 结算 → 发出 `SetBitrate{reason:1}` → `host.current_bitrate_bps()` == clamp 值 |
| `setbitrate_reaches_recording_encoder` | host 侧管线用记录型编码器（包装 `FakeEncoder`，`set_bitrate` 调用写入 `Arc<Mutex<Vec<u32>>>`）→ 链路走完后断言编码器最终收到下发值（M1 测试 m1.rs:69-91 已有包装编码器先例） |
| `bw_datagram_does_not_leak_into_pipeline` | 同通道混合媒体帧与探测块 → ControllerPipeline 照常收帧上屏，探测块不触发 `push_datagram` 的 Protocol 错（B4） |
| `probe_failure_keeps_negotiated_bitrate` | `LoopTransport` 配置为丢弃探测数据报 → 超时结算 est=None → 不发 SetBitrate、host 码率保持 Welcome 值（B6 反向） |

**rc-pipeline 回归（`crates/rc-pipeline/tests/bitrate.rs` 新建）：**

| 测试名 | 验证 |
|---|---|
| `set_bitrate_reaches_encoder_thread` | 记录型编码器 → `set_bitrate(3_500_000)` 后有界等待（≤2s 真实时间，轮询共享记录；真实线程等待与 m1.rs 既有线程测试同一形态）断言 3_500_000 出现 |
| `set_bitrate_zero_rejected_without_command` | bps=0 → Err(InvalidConfig)，编码器记录不增 |
| `set_bitrate_does_not_disturb_tick_or_idr` | 设置码率前后 `tick` 照常产出数据报、`request_idr` 后下一帧仍是 IDR（M1 语义回归保护） |
| `set_bitrate_after_shutdown_errors_cleanly` | `shutdown()` 后调用 → Err 不 panic（通道 Disconnected） |

真机/权限测试：**本卡没有**。全部离线确定性或进程内假后端；localhost 双进程实测归 T18。

## 验收命令

```bash
# 内循环（M0 纪律：rc-session 依赖扇出大，日常只查本 crate）
cargo check -p rc-session
cargo test -p rc-session
cargo test -p rc-pipeline

# 本卡触碰了已过 M1 Gate 的 rc-pipeline，必须全套回归
cargo test --workspace
cargo clippy -p rc-session -p rc-pipeline --all-targets -- -D warnings

# 铁律 4 自检：探测与会话逻辑不读系统时钟。
# 豁免 client_session.rs：T14 坑 5 认可 ClientSession::run() 驱动循环内
# Instant::now() 作 pump_sender 批窗口驱动钟（与 run_real 的
# start.elapsed() 同纪律）——T14 落地后该文件必有命中，不属违规；
# 本卡不得在 probe.rs 及其余任何文件新增系统钟读取。
grep -rn "Instant::now\|SystemTime::now" crates/rc-session/src \
  | grep -v "client_session.rs" && echo "违规" || echo "干净"
```

## 验收标准

- [ ] `cargo test --workspace` 全绿（含 rc-pipeline m1.rs 与 rc-loopback 全量回归——`set_bitrate` 不破坏 M1 基线）
- [ ] 时钟偏移换算被合成偏移单测钉死：已知 ±固定偏移下 `host_to_local_us` 换算误差为 0（确定性、无网络）
- [ ] RTT/偏移公式：合成 t0/t1/t2 下 rtt = t2−t0、offset = t1−(t0+t2)/2（NTP 中点法），方向语义与 probe.rs 文档一致
- [ ] 心跳：1s 间隔泵在 5s 逻辑时钟内产出 ≥4 条 ProbePing，且全部经 reliable 控制流（datagram/urgent 通道零探测消息）
- [ ] 带宽探测：256KB → 块数正确、单块 ≤1120 字节（N2 预算）、丢 25% 块仍能出估速
- [ ] 初始码率链完整：Welcome 协商值先生效（host 以裁定值构造编码器）→ 探测结算 → `SetBitrate(reason=1)` → 记录型编码器实际收到 `clamp(估速×0.7, 2Mbps, host 上限)`
- [ ] 探测退化/失败 → 不发 SetBitrate、保持协商值
- [ ] `grep -rn "Instant::now\|SystemTime::now" crates/rc-session/src | grep -v "client_session.rs"` 为空（铁律 4；client_session.rs 的 run() 驱动钟豁免依据 T14 坑 5）；rc-pipeline 新增代码同样不读时钟
- [ ] rc-pipeline 的 diff 审查确认：仅新增 `set_bitrate` + 私有命令通道，既有签名与时序零改动
- [ ] 无 `unsafe`、rc-session 零新增依赖

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`crates/rc-pipeline/src/host.rs`](../../../crates/rc-pipeline/src/host.rs) | 编码线程结构（267-312 行：encoder move、recv_timeout 循环、停机排空）——命令通道就插在这里；569 行 stderr 痕迹风格 |
| [`crates/rc-net/src/quic.rs`](../../../crates/rc-net/src/quic.rs) | `max_media_payload` 预算（127-129 行）、`DEFAULT_INITIAL_PACING_BPS`（79 行）、有界通道纪律（`URGENT_QUEUE_CAPACITY`，121 行） |
| [`crates/rc-input/src/channel.rs`](../../../crates/rc-input/src/channel.rs) | 泵式 API 形态（`pump_sender`/`pump_receiver`，111 行）与测试用 MockTransport 模式（时间注入 + 通道断言） |
| [`crates/rc-loopback/src/lib.rs`](../../../crates/rc-loopback/src/lib.rs) | 驱动循环与会话时钟闭包注入模式（343-360 行：单闭包喂双侧管线）——B9 同纪元就是照抄这个模式 |
| [`crates/rc-pipeline/tests/m1.rs`](../../../crates/rc-pipeline/tests/m1.rs) | 包装编码器先例（69-91 行）与真实线程的有界等待测试形态 |
| [`crates/rc-encode/src/fake.rs`](../../../crates/rc-encode/src/fake.rs) | `FakeEncoder::bitrate_bps()` 观测口（91-93 行）——记录型编码器照此扩展 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | `push_datagram` 把 host 域 capture_ts 原样写进 trace 的 Captured 标记（controller.rs:176-178）——报告侧忘做偏移换算，e2e 数字整体错一个纪元差，且 localhost 下小到看不出来、LAN 下暴露 | 合成偏移单测钉死 `host_to_local_us` 路径（B3）；偏移误差 ≤ RTT/2（NTP 中点法），LAN 最坏 +½RTT 使测量偏保守、不会虚优 |
| 2 | BwProbe v0 测的是 pacer 天花板（50Mbps，quic.rs:79）而非真实路径带宽，且 datagram 丢包使估速偏低 | 定位为初始码率下限保护的粗 heuristic（B10 写死）；M3-3 拥塞控制接管后整条链废弃 |
| 3 | 探测块与媒体数据报共用 datagram 通道：媒体报文首字节是 MAGIC LE 的 0x52，探测块首字节 0x09——不做分流，`push_datagram` 会报 Protocol 错误 | B4 的 MAGIC 判定先于管线投递；单测 `bw_datagram_does_not_leak_into_pipeline` 钉死 |
| 4 | 探测钟 u64 与管线钟 u32 纪元不一致 → 偏移换算引入常数误差 | B9：同一时钟闭包喂两侧；70 分钟守卫（T17）内 u64 < 2^32，u32 零扩展无损（契约 §1 的 71.6 分钟饱和提示，20-crate-contracts.md:93） |
| 5 | localhost 首末块同拍到达 → 间隔趋 0，估速除零/∞ | `BW_PROBE_MIN_SPAN_US` 门槛：间隔 <1ms 记退化（None），保持协商值 |
| 6 | 编码器在编码线程内（host.rs:267-268），`set_bitrate` 直接调不到；做成同步确认又会阻塞会话驱动循环 | 命令通道 fire-and-forget（≤5ms 生效），编码器侧错误只进 stderr；测试用记录型编码器 + 有界轮询断言，不做 ack 往返 |
| 7 | rc-session 依赖扇出大（rc-net/rc-pipeline/rc-input 三条链），全量编译慢 | 内循环只 `cargo check -p rc-session`；`cargo test --workspace` 只在收尾跑（M0 纪律） |
| 8 | QUIC 层已有 1s keepalive（quic.rs:95-96），容易误以为 ProbePing 重复可删 | 两者正交：QUIC keepalive 保连接活着，ProbePing 产出 RTT/偏移并供 T16 看门狗复用；probe.rs 文档已写明，别删任何一个 |
| 9 | 同机双进程 CPU 竞争会让真后端下的探测数字带调度噪声 | 本卡验收全用假后端离线测试；真后端只归 T18 的 localhost 冒烟 |

## 依赖

```toml
# rc-session 的 Cargo.toml 由 T13 建立（rc-types / rc-net / rc-pipeline /
# rc-input / rc-trace），本卡不新增任何依赖。
# 注意 Cargo 语义：dev-dependencies 不传递——tests/probe_session.rs 用到的
# rc-capture（FakeBackend）/ rc-encode（FakeEncoder）/ rc-decode /
# rc-render 必须出现在 rc-session 自己的 [dev-dependencies] 里才会被
# 编译进测试（T14 已显式列出这四项，本卡沿用即可，不要误以为
# 「随 rc-pipeline 传递可用」而漏声明）；也不引 workspace 外 crate。
```

## 与后续卡的接口

- **T16（看门狗）**：IdrRequest 超时复核的「RTT+余量」直接读 `ProbeEngine` 的 RTT 中位数；泵形态（`pump(now_us)`）照抄。
- **T17（重连）**：时钟纪元重置（host_flags bit1）后 `ProbeEngine` 必须重建（旧偏移属于旧纪元）；重连成功后 client 应重新 `start_bw_probe`（可能换了路径）。
- **T18（CLI/报告）**：会话报告 session 段嵌入 `ProbeSnapshot`（键名建议：`rtt_us` / `clock_offset_us` / `bw_estimate_bps` / `initial_bitrate_bps` / `negotiated_bitrate_bps`）；e2e 与 breakdown.network 的一切 host 时间戳统一经 `host_to_local_us` 换算，无偏移样本时对应字段输出 null 而非数字。
- **M3-3（拥塞控制）**：接管后 `BwProbeRequest/Data` 与 `SetBitrate(reason=1)` 初始链废弃替换（线格式号保留不回收）；`HostPipeline::set_bitrate` 转发通道保留复用。
