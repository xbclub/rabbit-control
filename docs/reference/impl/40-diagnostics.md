# 40 诊断与延迟测量规范

> **没有这套设施，延迟优化就只能靠猜。** 它是 M1 的交付物，不是可选项。

## 1. 端到端延迟的严格定义

**glass-to-glass**：被控端「屏幕像素实际变化」→ 控制端「该变化呈现到屏幕上」。

必须用**同一条时间线**测量。跨机器的墙钟不可比，所以有两条路：

| 方法 | 精度 | 复杂度 | 用在哪 |
|---|---|---|---|
| **回环自测**（单机） | ±1ms | 低 | M1 Gate、CI、日常验证 |
| **屏幕闪烁编码**（双机） | ±8ms | 高 | M2 之后的人工抽检 |

### 1.1 回环测量原理

```
┌──────────────── 同一进程 ────────────────┐
│                                          │
│  生成测试帧 ──▶ Capture ──▶ Encode ──▶  │
│  (含帧序号)        ▲                     │
│                    │ 时间戳 T0           │
│                    ▼                     │
│  Decode ──▶ 读回帧序号 ──▶ 比对 T0/T1    │
│                      │                   │
│                      └─ 时间戳 T1        │
└──────────────────────────────────────────┘

延迟 = T1 - T0
```

**关键**：测试帧的右上角用 8×8 像素块编码帧序号（二进制，黑白）。
解码后读回该块，即可知道「这张图是哪一帧」，配合时间戳算出延迟。

优点：完全自动、可进 CI、精度高。
缺点：不含真实网络（**这是刻意的**——M1 先证明「不含网络的部分够快」）。

### 1.2 回环模式的 Gate 换算

| 段 | 局域网预算 | 回环是否包含 |
|---|---|---|
| 采集 → 编码 → 打包 → 重组 → 解码 → 呈现 | 17ms | ✅ |
| 网络（发送→接收） | 5ms | ❌ |
| **合计** | 22ms | — |

因此 **M1 回环 Gate = 18ms（P50）**（17ms 预算 + 1ms 余量）。

**换算关系必须保持**：若 `docs/02-architecture.md` §7 的网络预算改了，M1 Gate 同步改。
文档里这两处数字必须一致，改一处就改另一处。

## 2. 帧时间线上的 9 个标记

每个标记的语义**必须精确**，否则分段统计会误导优化方向。

| 标记 | 打点位置 | 常见写错 |
|---|---|---|
| `Captured` | 屏幕内容**实际变化**的时刻（平台提供的事件时间，或 VSync 时刻） | ❌ 写成采集函数返回时刻 → 延迟虚高 4–16ms |
| `Diffed` | 脏区分析完成 | — |
| `EncodeStart` | 帧交给编码器（进入队列前） | ❌ 写成编码器内部开始处理时刻 → 隐藏队列时间 |
| `EncodeDone` | 编码器返回码流 | — |
| `Sent` | `write()` 完成（进入内核） | — |
| `Acked` | 可靠通道收到 ACK（**datagram 无此标记**） | ❌ 给 datagram 也打这个点 → 永远缺失 |
| `Received` | 对端重组完成 | ❌ 写成收到第一个包的时刻 |
| `Decoded` | 解码器返回纹理 | — |
| `Presented` | 呈现调用返回（`Present` / `commit`） | ❌ 写成「提交后」 → 不含实际上屏 |

**规则：`Sent` 和 `Received` 之间就是网络段。回环模式下这段被模拟延迟替代。**

## 3. 分段名称（固定，不可改）

`rc-trace::FrameTrace::latency_breakdown()` 返回这 8 个名字：

```
diff / sched_queue / encode / packetize / network / decode / present / total
```

**验收脚本会按名字解析。改名会导致统计静默漏项。**

对应的计算：

| 名称 | 计算 |
|---|---|
| `diff` | `Diffed - Captured` |
| `sched_queue` | `EncodeStart - Diffed` |
| `encode` | `EncodeDone - EncodeStart` |
| `packetize` | `Sent - EncodeDone` |
| `network` | `Received - Sent` |
| `decode` | `Decoded - Received` |
| `present` | `Presented - Decoded` |
| `total` | `Presented - Captured` |

## 4. 回环报告 JSON（固定 schema）

`rc-loopback` 必须输出这个格式。**字段名不可改**（验收脚本依赖）。

```json
{
  "config": {
    "resolution": "1920x1080",
    "fps": 60,
    "duration_s": 60,
    "inject_loss": 0.0,
    "inject_rtt_us": 0,
    "bandwidth_bps": 0,
    "encoder": "nvenc"
  },
  "latency_ms": { "p50": 17.4, "p95": 21.2, "p99": 28.1 },
  "fps": { "displayed": 59.2, "encoded": 59.2, "captured": 60.1 },
  "frames": {
    "captured": 3606,
    "encoded": 3552,
    "displayed": 3551,
    "dropped_repeat": 54,
    "dropped_stale": 0,
    "dropped_budget": 0,
    "dropped_superseded": 0,
    "lost_in_network": 1,
    "decode_errors": 0,
    "unfinished": 0
  },
  "breakdown_ms": {
    "diff": 0.9,
    "sched_queue": 0.0,
    "encode": 4.8,
    "packetize": 0.4,
    "network": 0.0,
    "decode": 3.1,
    "present": 1.9
  },
  "convergence_ms": 1000,
  "degradation_ms": null,
  "max_queue_depth_seen": 2,
  "probe_enabled": false,
  "gate": {
    "p50_ms_max": 18.0,
    "displayed_fps_min": 54.0,
    "static_cpu_pct_max": 8.0,
    "convergence_ms_max": 1000,
    "max_queue_depth": 2,
    "passed": true,
    "failures": []
  }
}
```

> **`gate.displayed_fps_min` 不是写死的 58**：门槛 = `target_fps × 0.9`
> （本例 60fps → 54.0）。写死 58 会让合法的 30fps 配置永远失败；
> 实现见 `rc-loopback/src/report.rs` 的 `GateResult::for_config`，
> 由测试 `gate_fps_threshold_follows_target` 钉住。
> 同理 `convergence_ms` / `degradation_ms` 用整 1 秒窗口判定，
> 最小可判定值是 1000ms（示例中分别为 1000 / null）。

### 4.1 新增字段说明（对应架构文档 Q1–Q6）

| 字段 | 类型 | 对应 | 含义 |
|---|---|---|---|
| `convergence_ms` | `u64 \| null` | Q1 / Q4 | 从循环开始到 `displayed_fps ≥ target × 0.9`（**整 1 秒**滑动窗口，最小可判定值 1000ms）的毫秒数。30 秒内未达到为 `null` |
| `degradation_ms` | `u64 \| null` | Q4 | 帧率从目标降到 50% 所用毫秒数（与 `convergence_ms` 同一套 1 秒窗口）。全程未低于 50% 为 `null`。用于与 `convergence_ms` 比较对称性 |
| `max_queue_depth_seen` | `u32` | Q3 | 整个运行期间任意一层队列的最大深度 |
| `probe_enabled` | `bool` | Q2 | 是否启用了网络延迟探针（`--probe` 开关，默认 `false`） |

**为什么要有 `probe_enabled`**：Q2 要求「帧率判定不依赖网络探针」。
默认关闭探针跑一次，再开启跑一次，两次的 `latency_ms.p50` 差值应 <2ms——
证明我们的调度不依赖 RustDesk 那样的 1Hz 探针回路。

### 4.2 Q4 对称性判定

```bash
# 上升：从冷启动到达标
jq '.convergence_ms' report.json          # 期望 <= 1000

# 下降：人为让编码变慢，看帧率跌落速度
cargo run -- loopback --encoder-slowdown 4.0 --duration 30s --report slow.json
jq '.degradation_ms' slow.json            # 期望与 convergence_ms 同量级

# 对称性判定
# ratio = max(convergence, degradation) / min(convergence, degradation)
# 要求 ratio <= 2.0
```

RustDesk 在这里必然失败：上升是 `+1` 每次（`video_qos.rs:546`），
下降是除法（`:523`），两者差一个数量级。

### 4.3 一致性不变式（必须校验）

```
frames.captured == frames.encoded + dropped_repeat + dropped_stale
                 + dropped_budget + dropped_superseded
frames.encoded  == frames.displayed + lost_in_network + decode_errors + unfinished
```

**任何一条不成立，说明埋点缺失。** 验收脚本必须检查，不通过就判失败。

### 4.4 `fps.captured` 与 `fps.encoded` 必须有差

静态或半静态场景下，`captured > encoded` 是**正确行为**（重复帧被丢弃）。
如果两者完全相等，说明 `rc-diff` 没生效——**这是失败信号，不是成功**。

## 5. 验收命令

### 5.1 基本场景（M1 Gate）

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 \
    --fps 60 \
    --duration 60s \
    --report /tmp/loopback-basic.json

# 判定
jq -e '.gate.passed' /tmp/loopback-basic.json
jq '.latency_ms.p50' /tmp/loopback-basic.json    # 应 <= 18.0
jq '.fps.displayed' /tmp/loopback-basic.json     # 应 >= 54（= 60 × 0.9，随 --fps 变化）
```

### 5.2 静态画面 CPU

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 30s \
    --static-content \
    --report /tmp/loopback-static.json

jq '.gate' /tmp/loopback-static.json
# 期望：frames.encoded 远小于 frames.captured（几乎全是 dropped_repeat）
# 期望：CPU 单核 <= 8%
```

### 5.3 弱网场景（M3 Gate）

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 120s \
    --inject-loss 0.03 \
    --inject-rtt-us 80000 \
    --report /tmp/loopback-lossy.json

jq '.fps.displayed' /tmp/loopback-lossy.json     # 应 >= 30
jq '.frames.unfinished' /tmp/loopback-lossy.json # 应为 0
```

### 5.4 编码器变慢（验证调度器）

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 60s \
    --encoder-slowdown 2.0 \
    --report /tmp/loopback-slow.json

# 关键断言：延迟不增长
jq '.latency_ms.p50' /tmp/loopback-slow.json     # 必须仍 <= 25
jq '.frames.dropped_budget' /tmp/loopback-slow.json  # 必须 > 0
```

**这一条是调度器是否真的生效的唯一证据。** 延迟增长 = 调度器没工作。

## 6. 回环模式的实现要点

### 6.1 必须能在无 GPU 时降级

```rust
// CI 上没有 NVENC 也要能跑
let encoder = match EncoderKind::detect() {
    EncoderKind::Nvenc => Encoder::nvenc(cfg)?,
    EncoderKind::Vaapi => Encoder::vaapi(cfg)?,
    EncoderKind::None  => Encoder::software(cfg)?,   // openh264
};
```

无 GPU 时延迟会超标，`gate.passed` 为 false 是**预期**的。
CI 只校验「逻辑正确 + 一致性不变式 + 无 panic」，不校验延迟门槛。

### 6.2 网络损伤注入

不要用 `tc netem`（需要 root、不可移植、影响整机）。在进程内模拟：

```rust
struct DamageInjector {
    loss: f32,
    rtt_us: u32,
    jitter_us: u32,
    bandwidth_bps: u64,
    rng: SmallRng,
    queue: VecDeque<(Instant, Vec<u8>)>,   // 延迟队列
}

impl DamageInjector {
    /// 收到一个数据报，决定：丢弃 / 排队后投递 / 立即投递
    fn process(&mut self, now: Instant, data: Vec<u8>) -> Option<Vec<u8>> { ... }
}
```

必须用**固定种子**的 PRNG，让失败可复现：

```rust
--seed 42   // 同一种子必须产生完全相同的丢包序列
```

### 6.3 时间源

回环模式内部用 `Instant`，但**注入给调度器的是相对微秒**（`Timestamp`）。
这样调度器的测试与生产完全一致。

## 7. 双机延迟测量（M2+）

回环证明不了真实网络。双机需要：

1. 被控端全屏播放一个「闪烁帧序号条」（120Hz 刷新，编码 8 位二进制）
2. 控制端用高速相机（240fps+）对准两边屏幕拍一张照片
3. 读出两边的序号差 → 帧数差 → 延迟

**精度 ±8ms（60fps 一个帧间隔）。** 适合人工抽检，不适合 CI。

替代方案（精度差但便宜）：在两台机器上同时录屏，事后比对可见的同一个动画帧。

## 8. 出问题时的诊断顺序

延迟/帧率不达标时，**按此顺序查，不要跳**：

| 步 | 检查 | 命令/方法 | 若不符 |
|---|---|---|---|
| 1 | 哪个段超标 | `jq '.breakdown_ms' report.json` | 进入对应步骤 |
| 2 | `network` 段大 | 双机 ping / `NetStats.rtt_us` | 网络问题，非本机 |
| 3 | `encode` 段大 | 确认走了硬件 + preset 最快档 + B帧0 | 改编码参数 |
| 4 | `present` 段大 | 确认 vsync 关闭 | 关 vsync |
| 5 | `diff` 段大 | tile 尺寸 / 是否每帧分配 | 优化 TileDiff |
| 6 | `sched_queue` 段大 | 在途帧数 | 检查编码器吞吐 |
| 7 | `frames.captured` == `frames.encoded` | `rc-diff` 是否生效 | 检查 `is_repeat` 判定 |
| 8 | `frames.unfinished > 0` | 埋点缺失 | 补 `finish()` 调用 |
| 9 | 一致性不变式不成立 | 统计逻辑错误 | 修 `rc-trace` 或调用方 |

## 9. 生产环境的诊断

不只在回环里做。生产会话也要：

- 被控端每帧打点（`rc-trace`），缓冲 4096 帧
- 用户可在工具栏「导出诊断信息」→ 输出 JSONL + 汇总
- 用户反馈卡顿时，要求导出这个文件

这比「描述一下怎么卡的」有效 100 倍。

## 10. 检查清单

实现 `rc-loopback` 时必须满足：

- [ ] 输出 §4 的完整 JSON schema，字段名完全一致
- [ ] 校验 §4.3 的一致性不变式
- [ ] `--seed` 固定后结果可复现
- [ ] 无 GPU 时降级到软件编码且不 panic
- [ ] 支持 `--inject-loss` / `--inject-rtt-us` / `--inject-jitter-us` / `--bandwidth-bps`
- [ ] 支持 `--encoder-slowdown`（用于验证调度器）
- [ ] 支持 `--static-content`（用于验证 CPU 占用）
- [ ] `gate.passed` 的判定逻辑与 `docs/06-milestones.md` 的 Gate 表一致
- [ ] `failures` 数组说明每一项失败原因
- [ ] 退出码：`passed` → 0，失败 → 1（便于 CI）
