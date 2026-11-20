# 30 测试规范

> 目标：让任何一个 agent 写的测试，**能真正抓到 bug**，而不是只求覆盖率数字。

## 1. 三条铁律

### 铁律 1：测试名描述「行为」，不描述「函数」

```rust
// ❌ 不知道在测什么
#[test] fn test_update() {}
#[test] fn test_scheduler() {}

// ✅ 一眼看出期望
#[test] fn repeat_frame_is_dropped_without_keyframe() {}
#[test] fn idr_in_flight_is_never_evicted() {}
#[test] fn single_pixel_change_detected() {}
```

名字应该是**一句话断言**：「X 情况下，Y 应该发生」。

### 铁律 2：每段逻辑至少测四类

| 类别 | 要求 | 例子 |
|---|---|---|
| **正常路径** | 最常见输入 | `normal_frame_is_encoded` |
| **边界** | 0 / 最大值 / 空 / 奇数 | `deadline_boundary_is_exclusive` |
| **非法输入** | 越界 / 畸形 / 空 | `parse_rejects_short_buffer` |
| **状态迁移** | 连续操作后的状态 | `on_encoded_releases_slot` |

**只测正常路径 = 没测。**

### 铁律 3：必须能无平台依赖地运行

测试**不得**依赖 GPU、网络、真机、系统时钟。

```rust
// ❌ flaky：依赖真实时间
std::thread::sleep(Duration::from_millis(17));

// ✅ 注入时间
sched.on_captured(seq, kind, false, Timestamp(1000), 1000);
```

需要真实环境才能验证的东西，放到 `rc-loopback`（M1）或人工验收，不放进单元测试。

## 2. 测试分层

| 层 | 内容 | 运行时间 | 位置 |
|---|---|---|---|
| **单元测试** | 纯逻辑、状态机、算法 | 毫秒 | `src/lib.rs` 的 `#[cfg(test)] mod tests` |
| **组件测试** | crate 间协作，用假实现 | 秒 | `tests/` 目录 |
| **回环测试** | 采集→编码→解码→呈现 | 10–30s | `rc-loopback` |
| **集成测试** | 双机、真实网络 | 分钟 | CI + 人工 |
| **人工验收** | 观感、手感 | 分钟 | 里程碑 |

**agent 能自动跑前三层。** 写实现时，单元测试必须写完；组件测试按需；不要试图自动验证第四五层。

## 3. 必测清单（按 crate 类型）

### 3.1 状态机类（`rc-sched`、`rc-session`）

必须测：

- [ ] 每个 `Decision` / 输出分支都有测试
- [ ] **边界**：恰好等于阈值、阈值 ±1
- [ ] **非法状态不可达**：用枚举而非 bool 组合，编译期保证
- [ ] **重复调用**：同一事件调用两次不产生错误状态
- [ ] **重置**：`reset()` 后状态回到初始
- [ ] **统计一致性**：`captured == encoded + dropped_total`

代码参考 `rc-sched` 的 18 个测试，覆盖了：
```
repeat_frame_is_dropped_without_keyframe     正常 + 无副作用
stale_frame_is_dropped                       正常
deadline_boundary_is_exclusive               边界（==）
just_before_deadline_is_encoded              边界（-1）
budget_full_evicts_oldest                    状态迁移
idr_in_flight_is_never_evicted               优先级
all_idr_in_flight_rejects_new_frame          饱和
on_encoded_unknown_seq_is_ignored            非法输入不 panic
latency_does_not_grow_when_encoder_is_slow   **核心行为验证**
stats_encode_ratio_for_static_screen         统计正确性
```

### 3.2 纯算法类（`rc-diff`、`rc-fec`）

必须测：

- [ ] 正常路径
- [ ] 空输入
- [ ] 单元素输入
- [ ] 最大尺寸 / 越界
- [ ] 奇数 / 非对齐尺寸（如 `15x17`、`stride` 带 padding）
- [ ] **真实 bug 的回归测试**（见下）
- [ ] 性能相关的：内部缓冲是否复用（用 `reused` 字段或分配计数断言）

**`rc-diff` 的真实 bug 教训**：

```rust
// 曾经的实现：把 BGRA 四通道异或成一个字节
let mut byte = 0u8;
for c in 0..bpp { byte ^= data[off + c]; }
// 白色 (255,255,255,255) → 0
// 黑色 (0,0,0,0)         → 0
// 判定为「无变化」→ 画面冻结
```

这个 bug 无法通过「测试有变化的情况」发现，只有 `set_px(f, 20, 20, 255)` 这种
**单像素、全通道同值**的用例才能抓到。写测试时想：「什么输入会让我的实现给出错误答案？」

### 3.3 编解码类（`rc-proto`、`rc-fec`）

必须测：

- [ ] **往返测试**：encode → decode → 等于原值
- [ ] **逐字节固定向量**：手写期望字节数组，防止线格式漂移
- [ ] **分片重组**：多片拼回原始字节流
- [ ] **每个错误码都有测试**，且 `offset` 正确
- [ ] **穷举**：如果是小空间（如 4 个位置），全部测一遍

```rust
// rc-fec 的穷举式验证
fn recovery_is_correct_for_any_position() {
    for lost in 0..4usize {   // 每个位置都丢一次
        ...
        assert_eq!(payload, payloads[lost], "lost={lost}");
    }
}
```

**只有「跳过每个位置都测」才能证明 XOR 恢复真的对。** 测一个位置是不够的。

### 3.4 诊断类（`rc-trace`）

必须测：

- [ ] 统计字段正确
- [ ] 百分位边界（0%、50%、100%、空集）
- [ ] JSON 格式合法（能被解析）
- [ ] 缓冲回绕（容量满时淘汰最旧）
- [ ] 乱序时间戳不下溢

## 4. 断言怎么写

### 4.1 失败信息必须能定位问题

```rust
// ❌ 失败时只知道「错了」
assert!(r.is_repeat);

// ✅ 失败时知道「哪个输入导致错了」
assert!(!r.is_repeat, "single pixel change at (20,20) must not be repeat");
```

### 4.2 用 `matches!` 测枚举

```rust
assert!(matches!(d, Decision::Encode { .. }));

// 需要检查字段时
match d {
    Decision::Drop { reason, needs_keyframe, .. } => {
        assert_eq!(reason, DropReason::Stale);
        assert!(needs_keyframe);
    }
    other => panic!("expected Drop, got {other:?}"),
}
```

`panic!` 里带 `{other:?}` ——失败时能直接看到实际值。

### 4.3 浮点用容差

```rust
assert!((r - 0.15).abs() < 1e-6, "got {r}");
```

### 4.4 不要用 `assert_eq!` 比较浮点

## 5. 测试辅助函数

**每个测试模块顶部放构造函数**，减少重复：

```rust
fn sched() -> Scheduler {
    Scheduler::new(SchedulerConfig::default()).unwrap()
}

const TS0: u32 = 1_000_000;  // 会话开始后 1 秒，避免与 0 混淆

fn frame(w: u32, h: u32) -> CpuFrame {
    CpuFrame { width: w, height: h, format: PixelFormat::Bgra8,
               stride: w * 4, data: vec![0; (w * h * 4) as usize] }
}

fn set_px(f: &mut CpuFrame, x: u32, y: u32, val: u8) {
    let off = (y * f.stride + x * 4) as usize;
    f.data[off..off + 4].fill(val);
}
```

**不要**为了复用而引入复杂抽象——测试辅助函数应该一眼看懂。

## 6. 什么时候用 `#[ignore]`

```rust
#[test]
#[ignore = "requires GPU: run with --ignored on a machine with NVENC"]
fn nvenc_produces_valid_h264() { ... }
```

规则：
- 需要 GPU / 真机 / 网络的测试用 `#[ignore]`，并在**任务卡里单独列出运行方法**
- 常规 `cargo test` 必须全绿，不能有 ignore 之外的跳过
- CI 里只有自托管 GPU runner 跑 `--ignored`

## 7. 性能测试

用 `cargo bench`（不放进 CI 常规流程）：

```rust
// benches/diff.rs
fn bench_tile_diff_1080p(c: &mut Criterion) {
    let mut d = TileDiff::with_defaults();
    let f = frame(1920, 1080);
    c.bench_function("tile_diff_1080p", |b| {
        b.iter(|| d.update(black_box(&f)))
    });
}
```

**关键性能指标**（写进任务卡验收）：

| 组件 | 目标 |
|---|---|
| `TileDiff::update` @1080p | ≤2ms |
| `MediaHeader::parse` | ≤200ns |
| `MediaHeader::write` | ≤100ns |
| `FecGroup::build_parity` @16 包×1200B | ≤50µs |
| `Scheduler::on_captured` | ≤100ns |

## 8. 覆盖率

不追求数字，但要保证：

```bash
# 如果需要看覆盖率（需安装 cargo-llvm-cov）
cargo llvm-cov -p rc-sched --summary-only
```

**判定标准不是百分比，而是：**

- 每个 `pub fn` 至少被调用一次
- 每个 `Err(...)` 返回路径至少有一个测试
- 每个 `match` 分支至少被走到

## 9. 反模式清单

| 反模式 | 为什么错 |
|---|---|
| 只测 happy path | 出错时抓不到 |
| 测试名 `test_xxx` | 不知道期望什么 |
| `assert!(result.is_ok())` 不检查值 | 函数返回错了值也算通过 |
| `unwrap()` 掩盖错误路径 | 出错变成 panic，看不出是哪个假设错了 |
| 依赖 `sleep` | flaky |
| 一个测试测 10 件事 | 失败时不知道哪件错了 |
| 为了通过而放宽断言 | 等于删掉测试 |
| 复制粘贴导致 `let x = 5` 忘记改 | 建议每个用例重新构造输入 |
| 测试里用真实的 `Instant::now()` | 不可重复 |

## 10. 提交前自检

```bash
cargo test --workspace      # 必须 0 failed
cargo clippy --workspace --all-targets -- -D warnings   # 必须无警告
```

逐项确认：

- [ ] 新增的 `pub fn` 都有测试
- [ ] 新增的错误分支都有测试
- [ ] 每个测试名是一句可读的断言
- [ ] 有边界测试（0 / 最大 / 空 / 越界）
- [ ] 失败信息包含实际值（`{other:?}`）
- [ ] 没有 `sleep`、没有真实时钟、没有网络
- [ ] 没有为了让测试过而放宽断言
