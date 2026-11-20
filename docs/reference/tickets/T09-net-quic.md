# T09 QUIC 传输层

> 实现 `rc-net`：QUIC 双通道（datagram 媒体 + reliable 控制）、分片重组、FEC 集成、pacing。

## 背景

TCP 的队头阻塞是「画面冻结」的直接原因：丢一个包，后面全等。
QUIC 的 datagram 扩展（RFC 9221）提供「不重传的可靠」——丢包时宁可丢帧也不等待。

这一卡**不依赖任何平台的采集/编码**，可以在 macOS 上纯逻辑 + 本地回环 socket 完成。

## 平台要求

**全平台。** 不需要 GPU，不需要真机。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-net/Cargo.toml` | 新建 | |
| `crates/rc-net/src/lib.rs` | 新建 | `Transport` trait |
| `crates/rc-net/src/quic.rs` | 新建 | `quinn` 封装 |
| `crates/rc-net/src/pacer.rs` | 新建 | 平滑发送 |
| `crates/rc-net/src/reassemble.rs` | 新建 | 分片重组 |
| `crates/rc-net/src/fec_adapter.rs` | 新建 | 接 `rc-fec` |
| `crates/rc-net/src/stats.rs` | 新建 | 统计与拥塞信号 |

## API 契约

照抄 [`20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.3。补充内部类型：

```rust
// reassemble.rs
/// 按 frame_seq 累积分片，凑齐后交付完整帧。
pub struct FrameAssembler { /* 私有 */ }

/// **默认并发帧数 = 2**（对应架构文档 Q3）。不要改成 8 或更大。
/// 队列越深 = 用户看到的画面越旧。RustDesk 用 120 帧，代价是 2 秒延迟。
pub const DEFAULT_MAX_CONCURRENT_FRAMES: usize = 2;

impl FrameAssembler {
    /// `max_concurrent_frames` 默认传 [`DEFAULT_MAX_CONCURRENT_FRAMES`]。
    /// 传入 >2 的值是可以的（例如离线抓包分析），但**在线传输路径必须用 2**。
    pub fn new(max_concurrent_frames: usize) -> Self;
    /// 投入一个已解析的包。返回 Some(完整帧) 表示该帧已齐。
    pub fn push(&mut self, header: &MediaHeader, payload: &[u8]) -> Option<AssembledFrame>;
    /// 请求 FEC 恢复（在 FEC 适配层调用）
    pub fn try_fec_recover(&mut self, group: u16) -> Option<AssembledFrame>;
    /// 超时清理（超过 500ms 未凑齐的帧丢弃）
    pub fn gc(&mut self, now_us: u32) -> Vec<FrameSeq>;
}

pub struct AssembledFrame {
    pub seq: FrameSeq,
    pub kind: FrameKind,
    pub capture_ts: Timestamp,
    pub encode_done_ts: Timestamp,
    pub data: Vec<u8>,
}

// pacer.rs
/// 按目标速率平滑发送。防止瞬间打满带宽导致 bufferbloat。
pub struct Pacer { /* 私有 */ }

impl Pacer {
    pub fn new(initial_rate_bps: u64) -> Self;
    /// 入队一个待发送的包。
    pub fn enqueue(&mut self, data: Vec<u8>);
    /// 取出当前时刻允许发送的包（可能 0 个或多个）。
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>>;
    /// 更新速率（由拥塞控制驱动）。
    pub fn set_rate(&mut self, bps: u64);
    /// 队列积压字节数。
    pub fn queued_bytes(&self) -> usize;
    pub fn rate_bps(&self) -> u64;
}

// fec_adapter.rs
/// 把编码后的帧切成 FEC 组，生成冗余包。
pub struct FecSender { /* 私有 */ }
impl FecSender {
    pub fn new(cfg: FecConfig) -> Self;
    /// 输入一帧的完整分片，返回「数据包 + 冗余包」的发送序列。
    pub fn wrap_frame(&mut self, seq: FrameSeq, packets: Vec<Vec<u8>>,
                      loss_rate: f32) -> Vec<Vec<u8>>;
}

/// 接收入侧的 FEC 恢复。
pub struct FecReceiver { /* 私有 */ }
impl FecReceiver {
    pub fn new() -> Self;
    /// 记录收到的包（数据或冗余）。
    pub fn observe(&mut self, header: &MediaHeader, payload: &[u8]);
    /// 尝试恢复某组缺失的包。
    pub fn try_recover(&mut self, group: u16) -> Option<(u16, Vec<u8>)>;
    /// 清理超时的组。
    pub fn gc(&mut self, now_us: u32);
}
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| N1 | 媒体走 QUIC datagram，控制/输入走 reliable stream | 媒体等重传 → 冻结 |
| N2 | 单数据报 ≤ `mtu - QUIC_OVERHEAD`（默认 ≤1200 负载） | IP 分片，丢一等于丢全 |
| N3 | **pacing 必须实现**（不能 for 循环发完） | bufferbloat → 延迟尖峰 |
| N4 | `poll(timeout)` 不阻塞超过 timeout | 上层卡住 |
| N5 | 分片按 `packet_idx` 排序重组，不按到达顺序 | 数据错乱 |
| N6 | 超时未凑齐的帧被丢弃并记 `LostInNetwork` | 内存泄漏 + 统计不准 |
| N7 | FEC 只恢复缺 1 个包的情况；缺 ≥2 直接放弃 | 无限等待 |
| N8 | `NetStats` 提供 `rtt_us` / `loss_rate` / `pacing_rate_bps` | 拥塞控制无输入 |
| N9 | 连接迁移后媒体流不中断 | 换网断线 |
| N10 | 重组缓冲**默认上限 2 个并发帧**（不是 8） | 恶意/异常流量导致 OOM；且违反 Q3 |

## 不要做什么

- ❌ 不要实现自定义拥塞控制。**先用 quinn 默认的**，M3 再换。
- ❌ 不要做 rendezvous / NAT 穿透（那是 M5）。
- ❌ 不要做加密层（QUIC 已含 TLS 1.3）。E2E 加密是 M5。
- ❌ 不要做 UDP 打洞。本卡只处理「已有对端地址」的连接。
- ❌ 不要在 `rc-net` 里碰采集/编码/解码。

## 测试清单

**纯逻辑测试（无网络，CI 可跑）：**

| 测试名 | 验证 |
|---|---|
| `assembler_waits_for_all_packets` | N5/N6 |
| `assembler_delivers_in_order` | 乱序投递 → 按 idx 重组 |
| `assembler_gc_drops_stale_frames` | N6 |
| `assembler_respects_concurrency_limit` | N10 |
| `assembler_default_limit_is_two` | **N10 / Q3：`DEFAULT_MAX_CONCURRENT_FRAMES == 2`** |
| `pacer_smooths_burst_sends` | N3：100 包不能一次发出 |
| `pacer_respects_rate_change` | 改速率后节奏变化 |
| `pacer_queued_bytes_tracks_backlog` | 积压统计 |
| `fec_wraps_frame_with_parity` | 冗余包生成 |
| `fec_receiver_recovers_single_loss` | 接 `rc-fec` |
| `fec_receiver_gives_up_on_two_losses` | N7 |
| `stats_loss_rate_computed_from_nacks` | 统计 |
| `empty_frame_assembles_to_one_packet` | 边界 |

**本地 socket 集成测试（loopback UDP，无需外网）：**

| 测试名 | 验证 |
|---|---|
| `loopback_media_stream_passes_through` | 端到端分片→重组 |
| `reliable_stream_preserves_order` | 输入通道语义 |
| `connection_survives_packet_loss` | 用 `DamageInjector`（T02）注入 3% 丢包 |
| `large_frame_spans_many_packets` | 100KB 帧 |

## 验收命令

```bash
cargo test -p rc-net
cargo test --workspace
cargo clippy -p rc-net --all-targets -- -D warnings
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] 100KB 帧能正确分片并重组（逐字节相同）
- [ ] 注入 3% 丢包后，重组成功率 > 95%（FEC 起作用）
- [ ] 缺 2 包时 500ms 内放弃，不无限等待
- [ ] `Pacer` 在 100 包突发下耗时 ≥ 计算值的一半（确实在平滑）
- [ ] 重组并发数达上限后，新帧被拒而非无限增长
- [ ] **`DEFAULT_MAX_CONCURRENT_FRAMES == 2`**（Q3：不要设成 8，那是 RustDesk 120 帧的同一个错误方向）
- [ ] 无 `unsafe`

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | `MediaHeader`、`parse_datagram`、`split_frame` |
| [`rc-fec`](../../../crates/rc-fec/src/lib.rs) | `FecGroup` / `FecRecoverer` 的用法 |
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | `Sent` / `Received` 标记语义 |
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 时间注入模式 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | quinn 的 datagram 需显式启用（`TransportConfig::datagram_receive_buffer_size`） | 设非 0 |
| 2 | datagram 大小上限需协商（`max_datagram_frame_size`） | 两端都设 |
| 3 | 默认 `congestion` 是 CUBIC，对延迟不友好 | M3 再换；本卡先跑通 |
| 4 | stream 与 datagram 共享拥塞窗口 | 大量媒体流量会挤压输入通道 → 用 stream priority |
| 5 | `quinn` 的 `recv` 是 async，本项目用同步 API → 用 `block_on` 或独立线程 + channel | 注意不要在 async 里阻塞 |
| 6 | 重组 `HashMap<FrameSeq, _>` 需要定期 GC | 否则内存涨 |
| 7 | FEC 组号回绕（u16，65536 组后归零） | 用「组号 + 帧序号」联合判断，不单靠组号 |

## 依赖

```toml
[dependencies]
quinn = "0.11"
rustls = "0.23"
rc-types = { path = "../rc-types" }
rc-proto = { path = "../rc-proto" }
rc-fec = { path = "../rc-fec" }
rc-trace = { path = "../rc-trace" }
```

## 与后续卡的接口

- **T10 输入通道**会用 `send_reliable` 与一个独立的 `send_reliable_urgent`（若 quinn 支持 stream priority）
- **M5** 会在 `Transport` 之上加 E2E 加密层（X25519 + ChaCha20），**不改本卡接口**
- **M3** 会把 quinn 默认拥塞控制替换为自定义，只需改 `quic.rs` 内部
