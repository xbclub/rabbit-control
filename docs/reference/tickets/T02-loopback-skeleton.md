# T02 回环框架骨架

> 在单进程里把「假帧 → 假编码 → 假解码」串起来，并输出符合固定 schema 的报告。
> **不需要 GPU、不需要真采集。** 这个卡的价值是先把「测量与验收的骨架」立起来。

## 背景

M1 的核心问题不是「能不能编解码」，而是「延迟到底多少」。
在接入真实 DXGI/NVENC 之前，先把测量框架做好：

- 报告 JSON 的 schema 固定（验收脚本依赖）
- 一致性不变式校验逻辑
- 网络损伤注入器（固定种子可复现）
- Gate 判定逻辑

这样 T03–T07 每完成一块，都能立刻用同一套框架验证。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-loopback/Cargo.toml` | 新建 | 加入 workspace members |
| `crates/rc-loopback/src/lib.rs` | 新建 | 公共 API |
| `crates/rc-loopback/src/damage.rs` | 新建 | 网络损伤注入 |
| `crates/rc-loopback/src/report.rs` | 新建 | 报告 schema 与 Gate 判定 |
| `crates/rc-loopback/src/fake.rs` | 新建 | 假采集/假编码/假解码 |
| `Cargo.toml`（根） | 修改 | members 加 `crates/rc-loopback` |

## API 契约

```rust
// lib.rs
pub struct Loopback { /* 私有 */ }

impl Loopback {
    pub fn new(cfg: LoopbackConfig) -> Result<Self>;
    /// 跑指定时长，返回报告。不 panic，错误通过 Result 返回。
    pub fn run(&mut self, duration: Duration) -> Result<LoopbackReport>;
}

pub struct LoopbackConfig {
    pub resolution: Resolution,
    pub fps: u32,
    pub inject_loss: f32,          // 0.03 = 3%
    pub inject_rtt_us: u32,        // 单程 = rtt/2
    pub inject_jitter_us: u32,
    pub bandwidth_bps: u64,        // 0 = 不限
    pub encoder_slowdown: f32,     // 1.0 = 正常，2.0 = 慢一倍
    pub static_content: bool,      // true: 画面不变（验证重复帧过滤）
    pub seed: u64,                 // 丢包序列种子
    pub trace_capacity: usize,     // 默认 4096
}

pub struct LoopbackReport {
    pub summary: rc_trace::TraceSummary,
    pub sched_stats: rc_sched::SchedulerStats,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub displayed_fps: f64,
    pub captured_fps: f64,
    pub encoded_fps: f64,
    pub breakdown_ms: BreakdownMs,
    pub gate: GateResult,
}

pub struct BreakdownMs {
    pub diff: f64,
    pub sched_queue: f64,
    pub encode: f64,
    pub packetize: f64,
    pub network: f64,
    pub decode: f64,
    pub present: f64,
}

pub struct GateResult {
    pub p50_ms_max: f64,
    pub displayed_fps_min: f64,
    pub static_cpu_pct_max: f64,
    pub passed: bool,
    pub failures: Vec<String>,
}

impl LoopbackReport {
    /// 输出固定 schema 的 JSON（见 docs/impl/40-diagnostics.md §4）
    pub fn to_json(&self) -> String;
    /// 校验一致性不变式，返回违规项
    pub fn check_invariants(&self) -> Vec<String>;
}
```

```rust
// damage.rs
pub struct DamageInjector { /* 私有 */ }

impl DamageInjector {
    pub fn new(cfg: &LoopbackConfig) -> Self;
    /// 处理一个待发送的数据报。返回 None 表示丢弃。
    /// 返回 Some((deliver_at, data)) 表示在 deliver_at 时刻投递。
    pub fn process(&mut self, now: Instant, data: Vec<u8>) -> Option<(Instant, Vec<u8>)>;
    /// 取出所有到期的数据报
    pub fn poll_ready(&mut self, now: Instant) -> Vec<Vec<u8>>;
    /// 观测到的丢包率（用于 FEC 自适应）
    pub fn observed_loss(&self) -> f32;
}
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| L1 | 报告 JSON 字段名与 `docs/impl/40-diagnostics.md` §4 **完全一致** | 验收脚本解析失败 |
| L2 | 校验 §4.3 的一致性不变式，不成立则 `gate.passed = false` 并在 `failures` 说明 | 埋点缺失被掩盖 |
| L3 | `seed` 固定后丢包序列可复现（用 `rand::rngs::StdRng::seed_from_u64`） | 失败不可复现 |
| L4 | 假编码器可配置 `encoder_slowdown`，用**模拟时间**而不是 `sleep` | 测试变慢、flaky |
| L5 | `static_content = true` 时 `captured_fps` 应显著大于 `encoded_fps` | 无法验证 diff 生效 |
| L6 | 无 GPU 依赖，`cargo test -p rc-loopback` 在 CI 上可跑 | CI 红 |
| L7 | `run()` 不 panic（`unwrap` 只在测试里） | 生产不可用 |
| **L8** | **`static_content = true` 时 `encoded_fps == 0`**（不是「显著小于」，是**零**） | 对应 Q5：RustDesk 会重复编码 10 次 |
| **L9** | 接收队列深度硬上限 2 帧，`LoopbackConfig::max_queue_depth` 可配但**默认 2** | 对应 Q3：RustDesk 120 帧 |
| **L10** | 报告必须含 `convergence_ms`：从开始到 `displayed_fps ≥ target × 0.9` 的耗时 | 对应 Q1/Q4：量化爬升速度 |

### 为什么 L8 是「等于零」而不是「显著小于」

`static_content` 模式下画面完全不变，`rc-diff` 应判定每帧都是 `is_repeat`，
调度器全部返回 `Drop(Repeat)`，因此**没有任何帧进入编码器**。

若 `encoded_fps > 0`，说明链路里有一处偷偷在编码——这正是 RustDesk 的
`repeat_encode_max = 10` 的行为（`src/server/video_service.rs:729`）。
**这是可判定的红线，不是「大概」的要求。**

### `convergence_ms` 的测量方法

```
t0 = 循环开始时刻
t1 = 第一个满足 displayed_fps_window_1s >= target * 0.9 的时刻
convergence_ms = (t1 - t0) 的毫秒数
```

- 用 1 秒滑动窗口计算 fps（不是瞬时值，避免抖动误判）
- 若 30 秒内未达到 → `convergence_ms = null`，且 `gate.passed = false`
- **验收门槛：`convergence_ms <= 1000`**（RustDesk 是 9000ms+，见 Q4）

## 不要做什么

- ❌ **不要真的接 DXGI/NVENC。** 这个卡完全是假实现。真实后端是 T03/T04。
- ❌ 不要用 `std::thread::sleep` 模拟编码耗时。用「逻辑时钟」：每帧推进 `now_us`。
- ❌ 不要自己定义报告字段名。必须照抄 `40-diagnostics.md` §4。
- ❌ 不要做 UI。CLI 集成是 T07。

## 测试清单

必须覆盖：

| 测试名 | 验证什么 |
|---|---|
| `fake_pipeline_produces_zero_latency_without_damage` | 无损伤时延迟 ≈ 0（纯逻辑时钟） |
| `loss_injection_drops_expected_fraction` | 3% 丢包下实际丢弃比例在 2%–4% |
| `same_seed_produces_identical_drop_pattern` | L3 可复现性 |
| `rtt_injection_delays_delivery` | 注入 80ms RTT → network 段 ≈ 80ms |
| `jitter_stays_within_bounds` | 抖动不超过配置值 |
| `static_content_encodes_few_frames` | L5：编码帧数 << 采集帧数 |
| `static_content_encodes_zero_frames` | **L8：编码帧数严格为 0**（Q5） |
| `queue_depth_capped_at_two` | **L9：队列不超过 max_queue_depth**（Q3） |
| `convergence_within_one_second` | **L10：convergence_ms ≤ 1000**（Q1/Q4） |
| `fps_recovery_is_symmetric` | **Q4：15→60 与 60→15 用时比 ≤ 2:1** |
| `latency_breakdown_separates_queue_and_network` | **Q6：排队延迟不计入网络段** |
| `encoder_slowdown_causes_budget_drops` | 慢 2× → `dropped_budget > 0` 且延迟不增长 |
| `report_json_matches_schema` | L1：字段名逐个断言 |
| `invariant_violation_fails_gate` | L2：构造不一致数据 → passed = false |
| `report_json_parses_as_valid_json` | 用最小 JSON 解析器或字符串断言 |
| `gate_fails_when_p50_exceeds` | 判定逻辑 |
| `bandwidth_limit_queues_packets` | 限速生效 |

**每帧都必须有心跳**：假实现里也调用 `FrameTrace::finish()`，保证 `unfinished == 0`。

## 验收命令

```bash
cargo test -p rc-loopback
cargo test --workspace
cargo clippy -p rc-loopback --all-targets -- -D warnings
```

报告输出验证（需 T07 完成后才能跑 CLI，此卡内用单元测试验证）：

```rust
#[test]
fn report_json_matches_schema() {
    let r = run_test_loopback(60);  // 60 帧
    let j = r.to_json();
    for key in ["config", "latency_ms", "fps", "frames",
                "breakdown_ms", "gate"] {
        assert!(j.contains(&format!("\"{key}\"")), "missing {key}");
    }
    assert!(j.contains("\"p50\"")); 
    assert!(j.contains("\"displayed_fps\""));  // 或按 §4 的确切字段
}
```

## 验收标准

- [ ] `cargo test -p rc-loopback` 全绿
- [ ] `cargo test --workspace` 全绿（107 + 新增）
- [ ] 报告 JSON 字段与 `40-diagnostics.md` §4 一一对应
- [ ] 固定 seed 两次运行结果完全一致
- [ ] `encoder_slowdown=2.0` 时 `dropped_budget > 0`
- [ ] `static_content=true` 时 **`encoded_fps == 0`**（L8 / Q5）
- [ ] 任意路径上队列深度 ≤ 2（L9 / Q3）
- [ ] `convergence_ms <= 1000`（L10 / Q1 / Q4）
- [ ] 15→60 与 60→15 的用时比 ≤ 2:1（Q4）
- [ ] `breakdown_ms.sched_queue` 与 `network` 分列，未合并（Q6）
- [ ] 无 `sleep`、无真实时钟依赖（除了注入延迟用的 `Instant` 边界）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 时间注入模式、状态机测试 |
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | `TraceCollector` 用法、百分位统计、JSON 导出 |
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | 假帧构造、`reused` 字段的测试用途 |

## 已知坑

- 用 `VecDeque` 做延迟队列，按 `deliver_at` 排序插入（或每次线性扫描——帧数少，可接受）
- 抖动必须夹在 `[0, ...]`，不能产生负延迟
- `bandwidth_bps` 限速要按「字节/时间」计算，不要按包数
- 一致性不变式**必须**在 `run()` 结束时断言一次，而不是只放在测试里
