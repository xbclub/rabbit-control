# T08 M1 端到端串联

> 把 T02–T07 串成真实管线，跑出 M1 Gate 的数字。
> **这是整个项目的第一个硬门槛。Gate 不过不进入 M2。**

## 前置（macOS P0 主线）

- [x] T01 工程基座
- [x] T02 回环框架
- [x] T07 CLI
- [x] T12 macOS 采集 + VideoToolbox
- [x] T05 解码与呈现（macOS 版）
- [ ] T03 Windows 采集（降为 P1 延后，不阻塞 macOS M1）
- [ ] T04 NVENC 编码（降为 P1 延后，不阻塞 macOS M1）
- [ ] T06 软件编码兜底（阶段 F 软件兜底，不阻塞 macOS M1）

**不要提前领。** 缺任何一块，你写的都是无法验证的胶水代码。

## 背景

这个卡没有新技术，只有**接线与测量**。但它是最关键的一卡，因为：

M1 Gate 的数字决定项目是否成立。如果 1080p60 回环延迟做不到 18ms，
说明架构有问题，必须在进入网络层之前发现——否则后面所有工作都建立在错误的基线上。

## 平台要求

> **优先级 P0，平台改为 macOS**（2026-11-20 变更）。
> 本卡原稿按 Windows 编写。执行时请把 DXGI→ScreenCaptureKit、NVENC→VideoToolbox，
> 其余（调度器接线、测量、Gate 判定）完全不变。

**macOS 真机（Apple Silicon）。** 必须实测，不能用推断值。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-pipeline/Cargo.toml` | 新建 | 管线编排（采集→调度→编码） |
| `crates/rc-pipeline/src/lib.rs` | 新建 | |
| `crates/rc-pipeline/src/host.rs` | 新建 | 被控端管线 |
| `crates/rc-pipeline/src/controller.rs` | 新建 | 控制端管线 |
| `crates/rc-loopback/src/lib.rs` | 修改 | 接真实后端 |
| `crates/rc-cli/src/cmd/loopback.rs` | 修改 | 接 `rc-pipeline` |

## API 契约

```rust
// rc-pipeline/src/lib.rs
pub struct PipelineConfig {
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub prefer_hardware: bool,     // true: NVENC 失败才降级
    pub target_latency_us: u32,    // 传给 SchedulerConfig
    pub trace_capacity: usize,
}

/// 被控端管线：采集 → 调度 → 编码 → 分片
pub struct HostPipeline { /* 私有 */ }

impl HostPipeline {
    pub fn new(cfg: PipelineConfig) -> Result<Self>;
    /// 跑一次迭代：尝试取一帧并处理。
    /// 返回 Ok(None) 表示本次无帧（采集超时），调用方应继续循环。
    pub fn tick(&mut self, now_us: u32) -> Result<Option<Vec<Vec<u8>>>>;
    /// 控制端请求关键帧时调用
    pub fn request_idr(&mut self);
    pub fn trace(&self) -> &TraceCollector;
    pub fn sched_stats(&self) -> SchedulerStats;
    pub fn shutdown(&mut self);
}

/// 控制端管线：重组 → 解码 → 呈现
pub struct ControllerPipeline { /* 私有 */ }

impl ControllerPipeline {
    pub fn new(cfg: PipelineConfig) -> Result<Self>;
    /// 投入一个收到的数据报。返回是否有新帧上屏。
    pub fn push_datagram(&mut self, data: &[u8], now_us: u32) -> Result<bool>;
    /// 是否需要向对端请求关键帧
    pub fn take_keyframe_request(&mut self) -> bool;
    pub fn trace(&self) -> &TraceCollector;
    pub fn shutdown(&mut self);
}

/// 后端选择（P8）：按能力探测并降级
pub fn select_capture(cfg: &PipelineConfig) -> Result<Box<dyn CaptureBackend>>;
pub fn select_encoder(cfg: &PipelineConfig) -> Result<Box<dyn Encoder>>;
pub fn select_decoder(cfg: &PipelineConfig) -> Result<Box<dyn Decoder>>;
```

**关键：`HostPipeline::tick` 的返回语义**

| 返回 | 含义 | 调用方动作 |
|---|---|---|
| `Ok(Some(datagrams))` | 有帧被编码 | 发送这些数据报 |
| `Ok(None)` | 采集超时（屏幕无变化） | **继续循环，不 sleep** |
| `Err(e)` where `!e.is_fatal()` | 可恢复错误（如单帧编码失败） | 记录、继续 |
| `Err(e)` where `e.is_fatal()` | 致命错误 | 终止会话 |

## 数据流（必须严格按此接线）

```
被控端：
  CaptureBackend::next_frame(timeout)
        │ Some(frame) / None
        ├─ None → 循环（不产生帧，不占 CPU）
        └─ Some(frame)
              │
              ▼
        diff.update(&frame)  ──────► is_repeat?
              │                          │ true → trace.finish(DroppedRepeat)
              ▼ false                    │
        sched.alloc_seq()                │
              │                          │
              ▼                          │
        sched.on_captured(seq, kind, is_repeat, frame.timestamp, now)
              │
              ├─ Decision::Drop { reason, needs_keyframe } →
              │      trace.finish(reason→Outcome)
              │      if needs_keyframe || sched.take_keyframe_request() { request_idr() }
              │
              └─ Decision::Encode { seq, deadline_us } →
                     trace.mark(EncodeStart, now)
                     encoder.encode(&frame, force_idr, deadline)?
                     trace.mark(EncodeDone, now)
                     sched.on_encoded(seq, now)
                     split_frame(...) → Vec<datagram>
                     trace.mark(Sent, now)

控制端：
  recv datagram
        │
        ▼
  MediaHeader::parse_datagram
        │
        ▼
  reassemble（按 frame_seq 分组，凑齐 packet_cnt）
        │
        ▼
  trace.mark(Received, now)
        │
        ▼
  decoder.decode(data, kind, deadline)
        │
        ├─ NeedKeyframe → 发关键帧请求
        └─ Frame(f)
              trace.mark(Decoded, now)
              renderer.present(f)
              trace.mark(Presented, now)
              trace.finish(Displayed)
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| P1 | `next_frame` 返回 `None` 时**继续循环，不 sleep 不空转** | CPU 满载 或 采集停止 |
| P2 | `sched.on_encoded` **必须**在所有路径调用（含编码失败） | 在途永不释放 → 画面卡死但 CPU 很低 |
| P3 | `sched.take_keyframe_request()` 每次 `on_captured` 后检查 | 画面花屏 |
| P4 | 每帧**恰好一个** `Outcome`，`unfinished == 0` | 统计失效 |
| P5 | 采集的 `timestamp` 传给 `sched` 作为 `capture_ts`，`now_us` 用同一时间基准 | 调度器判定错误 |
| P6 | 重组按 `frame_seq` 分组，`packet_cnt` 齐了才交付 | 半帧解码 → 花屏 |
| P7 | 编码失败时 `trace.finish(DecodeError)` 并继续下一帧，不中断管线 | 一帧失败导致会话崩溃 |
| P8 | 工厂函数按平台/能力选择后端（DXGI→失败→GDI；NVENC→失败→openh264） | 无 GPU 机器无法启动 |
| P9 | 单线程跑通后再考虑多线程；**先保证正确** | 竞态难查 |

## 不要做什么

- ❌ 不要引入网络。回环模式是在进程内传递字节，不是 socket。
- ❌ 不要优化。**这一卡只要数字，不要调优**——优化是 M2 之后的事。
- ❌ 不要加多线程。串起最简单版本，先看数字。
- ❌ 不要为了「数字好看」而跳过丢帧统计。丢帧是**正确行为**，要如实记录。

## 测试清单

| 测试名 | 验证 |
|---|---|
| `pipeline_produces_trace_for_every_frame` | P4：`unfinished == 0` |
| `static_content_produces_no_encoded_frames` | 重复帧被丢弃 |
| `encoder_failure_does_not_stop_pipeline` | P7 |
| `on_encoded_called_on_all_paths` | P2：编码失败后在途仍能释放 |
| `reassembly_waits_for_all_packets` | P6：缺包不交付 |
| `keyframe_request_propagates_to_encoder` | P3 |
| `pipeline_reports_consistent_counts` | 一致性不变式 |
| `factory_falls_back_when_primary_unavailable` | P8 |
| `latency_breakdown_sums_to_total` | 分段之和 == 总延迟（容差 1ms） |

## 验收命令

```bash
cargo test -p rc-pipeline -p rc-loopback -p rc-cli
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### Gate 验证（必须真机）

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 60s \
    --report /tmp/m1-gate.json

# 逐项检查
jq '.latency_ms.p50' /tmp/m1-gate.json        # <= 18.0
jq '.latency_ms.p99' /tmp/m1-gate.json        # <= 35.0
jq '.fps.displayed'  /tmp/m1-gate.json        # >= 58
jq '.gate.passed'    /tmp/m1-gate.json        # true
jq '.frames.unfinished' /tmp/m1-gate.json     # == 0
jq '.breakdown_ms'   /tmp/m1-gate.json        # 每一项都要看
```

### 静态画面 CPU

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 30s \
    --static-content --report /tmp/m1-static.json
# 同时用任务管理器/Activity Monitor 观察 CPU
# 期望 <= 8% 单核
jq '.frames' /tmp/m1-static.json   # encoded 应远小于 captured
```

### 调度器生效验证（关键）

```bash
cargo run -p rc-cli --release -- loopback \
    --resolution 1920x1080 --fps 60 --duration 60s \
    --encoder-slowdown 4.0 --report /tmp/m1-slow.json

jq '.latency_ms.p50' /tmp/m1-slow.json          # 有界 ≈ 编码成本 + ~6ms，不随积压增长
jq '.frames.dropped_budget' /tmp/m1-slow.json   # 必须 > 0
jq '.frames.unfinished' /tmp/m1-slow.json       # 必须 == 0
```

**这一条是调度器是否真的工作的唯一证据。** 延迟随积压增长 = 调度器没生效。

> **修正 ①（2026-11-21）**：`2.0` → `4.0`。2×（10ms）小于 60fps 帧间隔
> （16.7ms），编码器跟得上，`dropped_budget` 永远为 0——验证不了调度器。
> 4×（20ms > 16.7ms）才形成持续过载。实测 p50 26.1/26.5ms ≈ 20ms 编码
> 成本 + diff/解码/交接 ~6ms，60 秒内稳定不涨；对照：逐出式旧策略同场景
> p50 83ms + 活锁（231 次预算裁决只交付 4 帧），那才是「延迟增长」。

## 验收标准（M1 Gate — 硬性）

| 指标 | 门槛 | 不通过怎么办 |
|---|---|---|
| 端到端延迟 P50 | **≤18ms** | 按下方排查表逐项查 |
| 端到端延迟 P99 | ≤35ms | 查抖动来源 |
| Displayed FPS | ≥58 | 查采集与呈现 |
| 静态画面 CPU（单核） | ≤8% | 查重复帧过滤 |
| 拖动时帧率 | ≥55 | 查编码耗时 |
| `unfinished` | == 0 | 补埋点 |
| 一致性不变式 | 全部成立 | 修统计 |
| `encoder-slowdown 4.0` 时延迟有界不增长 | 成立（实测 26.1/26.5ms ≈ 编码成本+固定开销，见修正①） | 查调度器接线 |

### Q1–Q6 专项门槛（与 RustDesk 的差异必须可测量）

这六项是本项目存在的理由。**任何一项不通过，产品定位不成立**，
因为用户完全可以选择免费的 RustDesk。

| # | 指标 | 门槛 | 对照 RustDesk | 怎么测 |
|---|---|---|---|---|
| **Q1** | `convergence_ms`（首帧达目标帧率的耗时） | **≤1000ms** | 9000ms+（`INIT_FPS=15` + `+1` 爬升） | 报告字段 `convergence_ms` |
| **Q2** | 无网络探针时帧率判定仍生效 | 成立 | 依赖 1Hz `TestDelay` | 跑回环，探针关闭，观察 `sched_queue` 与延迟仍随编码变慢而丢帧 |
| **Q3** | 任意队列深度 | **≤2 帧** | 120 帧 | `--encoder-slowdown 4.0` 后查 `max_queue_depth_seen`（实测 1） |
| **Q4** | 升降对称（15→60 vs 60→15 用时比） | **≤2:1** | 上升 +1/次，下降除法 | 两次跑，比 `convergence_ms` 与 `degradation_ms` |
| **Q5** | 静止画面编码帧数 | **== 0** | 最多重复编 10 次 | `--static-content` 跑 30s，查 `fps.encoded` |
| **Q6** | 排队延迟独立统计 | 成立 | 自认污染 | `breakdown_ms.sched_queue` 与 `network` 分列 |

#### 新增报告字段（T02 需一并实现）

| 字段 | 类型 | 含义 |
|---|---|---|
| `convergence_ms` | `u64 \| null` | 从开始到 `displayed_fps ≥ target×0.9` 的毫秒数 |
| `degradation_ms` | `u64 \| null` | 帧率从目标降到 50% 所用毫秒数（用于 Q4 对称性对比） |
| `max_queue_depth_seen` | `u32` | 整个运行期间观测到的最大队列深度（用于 Q3） |
| `probe_enabled` | `bool` | 是否启用了网络延迟探针（用于 Q2 对照） |

### Gate 不通过的排查顺序（**不可跳步**）

| 步 | 检查 | 命令/方法 |
|---|---|---|
| 1 | 哪一段超标 | `jq '.breakdown_ms' report.json` |
| 2 | `encode` 大 | 确认走硬件、preset P1、B帧0、outputDelay=0 |
| 3 | `present` 大 | 确认 `Present(0)` |
| 4 | `diff` 大 | 确认平台脏区被使用（DXGI），不是每帧全量 TileDiff |
| 5 | `sched_queue` 大 | 看在途帧数，编码器吞吐是否够 |
| 6 | `captured == encoded` | `rc-diff` 是否生效 |
| 7 | `unfinished > 0` | 哪条路径漏了 `finish()` |
| 8 | 整体都大但分段都小 | 打点位置错了（尤其 `Captured` 是否用了采集返回时刻） |

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 调度器完整用法（含 `take_keyframe_request`） |
| [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | `split_frame` / `parse_datagram` |
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | 打点顺序与 `Outcome` 归属 |
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | `is_repeat` 如何交给调度器 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | 忘了 `on_encoded` → 在途永不释放 | 用 RAII guard 或在所有分支调用 |
| 2 | 时间基准不一致（`Instant` vs `Timestamp`） | 全局统一用一个 `session_start: Instant` |
| 3 | `Captured` 打点用了采集返回时刻 | 用帧自带的 `timestamp`（内容变化时刻） |
| 4 | 重组时按到达顺序而不是 `packet_idx` 排列 | 必须按 idx 排序 |
| 5 | 丢帧后没请求 IDR → 花屏 | P3 |
| 6 | 静态画面下 `next_frame` 返回 `None` 时退出循环 | P1：必须继续 |
| 7 | 只测了 3 秒就下结论 | 必须 60 秒，短测会漏掉累积问题 |
| 8 | 线程化 host 先 `shutdown` 再 tick | 停机后的 tick 把末尾帧提交给已退出的编码线程（`Disconnected`），授权帧凭空消失、不变式 off-by-one。正确顺序：停采集 → tick 到静默 → 停编码（`HostPipeline::stop_capture`） |
| 9 | 驱动循环在编码在途时紧自旋 | macOS 定时器合并把编码线程的 `sleep(20ms)` 拖到 30ms（+50%）。空闲迭代退避 1ms |
| 10 | 用纯 `sleep` 模拟硬件耗时 | 定时器 slack 随睡眠时长按比例放大（~+35%），模拟器失真会顶穿门槛。混合等待：睡到剩 2ms 再自旋 |

## 终验记录（2026-11-21，M1 Gate 全项通过）

真机全链（`--backend real`：sck→VT→VT→Metal，隐私幕布 72fps 低熵动画，`max_in_flight=2`，
PTS 节拍过滤已回退）：动态 60s **P50=11.283ms / P99=21.203ms / 70.0fps / 4201 帧全上屏 /
零失败项（exit 0）**；运动 15s P50=11.164ms / 69.47fps 全项过；静态 30s 单核 CPU 稳态
**1.51%**（≤8%，`dropped_repeat=0`）。隐私幕布已固化进仓库：
`cargo test -p rc-render-macos --features window-bench --test curtain --release -- --mode anim --fps 72`
（静态 gate 用 `--mode static`；先启动幕布再跑本卡验收命令，测完 kill 幕布进程）。
「窗口拖动」场景以全屏低熵连续运动为刺激源（同类持续运动；`captured=encoded=displayed`
证零损耗）。完整数据见 [06-milestones.md](../design/06-milestones.md) 与 TASK.md §12。

## 完成后的动作

1. 把实测数字写回 [06-milestones.md](../design/06-milestones.md) 的 M1 出口条件
2. 更新 [tickets/README.md](README.md) 的状态表
3. 更新 `AGENTS.md` §9 的进度表
4. 若发现新坑，写进本文档「已知坑」与 [04-platforms.md](../design/04-platforms.md)
