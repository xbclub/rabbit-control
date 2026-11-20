# 00 上手指南（AI 代理入口）

> **这是你读的第一篇。** 读完这篇 + 你领的那张任务卡，就足够开工。
> 不需要把 `docs/` 下所有文档都读完。

## 1. 项目一句话

低延迟远程桌面控制软件，用 Rust 实现，目标是**在帧率和延迟上打赢 RustDesk**。

### 1.1 我们到底在跟 RustDesk 比什么（2026-11-20 修正）

**先说结论：不要相信「RustDesk 采集是轮询」这种说法。我们实际读过它的源码
（见 [ADR-0004](../adr/0004-rustdesk-fork-evaluation.md)），它已经事件驱动了。**

真实差距只有三个，其中第一个最致命：

| # | RustDesk 的实际做法 | 我们的要求 | 差距 |
|---|---|---|---|
| **Q3** | **接收队列 `VIDEO_QUEUE_SIZE = 120` 帧（2 秒缓冲）** | **≤2 帧** | **60×** |
| Q1 | `INIT_FPS=15`，连接后前 1 秒强制 15fps | 首帧即目标帧率 | 4× |
| Q4 | 帧率上升 `+1`、下降用除法（不对称） | 同一套 deadline 判定 | 恢复快 9 秒 |
| Q5 | 捕获超时时重复编码同一帧最多 10 次 | 静止画面零编码 | 10× 浪费 |
| Q2 | 帧率判定依赖 1Hz `TestDelay` 探针 | 本地 deadline 判定 | 感知快 1 秒 |
| Q6 | 延迟测量被队列污染（源码注释自认） | 排队延迟独立统计 | 数据可信 |

**如果你的代码违反了 Q3（队列深度 ≤2），你就复刻了 RustDesk 最大的问题。**

对应实现位置：

| 要求 | 实现位置 |
|---|---|
| Q1/Q4 帧率控制 | `rc-sched`（核心）+ `rc-pipeline` |
| Q3 队列深度 | `rc-net` 的 `DEFAULT_MAX_CONCURRENT_FRAMES = 2` |
| Q5 静止不编码 | `rc-diff`（`is_repeat`）+ `rc-sched`（`Drop(Repeat)`） |
| Q2/Q6 延迟判定与统计 | `rc-sched`（本地 deadline）+ `rc-trace`（分段统计） |

## 2. 现在仓库里有什么

**已完成、可运行、有测试：**

```
crates/
├── rc-types/    数据结构、错误、ID newtype       27 测试
├── rc-diff/     脏区检测、重复帧判定             13 测试
├── rc-sched/    帧调度器（核心）                 18 测试
├── rc-fec/      XOR 块 FEC                      18 测试
├── rc-proto/    媒体包头二进制编解码             16 测试
└── rc-trace/    帧时间线埋点与诊断               15 测试
```

全部 107 个测试通过，`cargo check --workspace` 通过。

**待实现：** `rc-capture`、`rc-encode`、`rc-decode`、`rc-render`、`rc-input`、`rc-net`、`rc-session`、`rc-loopback`、`rc-cli`。见 [任务卡清单](../tickets/README.md)。

## 3. 立刻验证环境

```bash
cargo check --workspace      # 应无错误（首次约 1 分钟）
cargo test --workspace       # 应 107 passed（约 1-2 秒）
```

如果失败，先解决环境问题再开工。不要在不干净的基线上开始改代码。

### `just` 是可选依赖

仓库有 `justfile`，装了 `just`（`brew install just`）可用简写：

```bash
just c rc-sched    # 检查单 crate（最常用）
just t rc-sched    # 测单 crate
just v             # 完整验证
```

**没装 `just` 就用等价命令，功能完全一样：**

| just | 等价命令 |
|---|---|
| `just c <crate>` | `cargo check -p <crate>` |
| `just ct <crate>` | `cargo check -p <crate> --tests` |
| `just t <crate>` | `cargo test -p <crate>` |
| `just cw` | `cargo check --workspace` |
| `just ta` | `cargo test --workspace` |
| `just f` | `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings` |
| `just v` | `just f` 之后跑 `cargo test --workspace` |
| `just size` | `du -sh target` |
| `just clean-disk` | `./scripts/clean-disk.sh` |

`cargo nextest` 同样可选：`cargo test` 的语义完全够用，nextest 只是更快。


## 4. 七条硬性规则

违反任何一条，CI 会拒绝，或后续 agent 会因你的改动而破坏功能。

### R1 内循环用 `cargo check`，不要用 `cargo build`

| 目的 | 命令 | 耗时 |
|---|---|---|
| 检查类型/借用 | `cargo check -p <crate>` | ~0.1s |
| 跑测试 | `cargo test -p <crate>` | ~1s |
| 全量测试（提交前） | `cargo test --workspace` | ~1.5s |

`cargo build` 慢 3–10 倍且对验证类型无额外价值。**只有需要运行二进制时才 build/run。**

### R2 `rc-types` 是稳定层

改它会触发全 workspace 重编。规则：
- 只放数据结构、错误类型、newtype
- 不放 trait（trait 放各自的抽象 crate）
- 不加重依赖
- 新字段用 `Option` 或 `#[non_exhaustive]`

### R3 `#[cfg(target_os)]` 只出现在工厂函数内

业务逻辑层必须平台无关。否则改 Windows 会破坏 macOS。

### R4 `unsafe` 只能在平台 crate 的 `ffi/` 子模块

- 每个 `unsafe fn` 必须有 `// SAFETY:` 注释
- 业务 crate 一律 `#![deny(unsafe_code)]`

### R5 错误用 `thiserror` 枚举

禁止 `Box<dyn Error>` 跨 crate 传播。调用方需要能 `match` 穷举。

### R6 每个模块必须能无平台依赖地测试

需要 GPU/网络才能测的代码，必须拆成「纯逻辑 + 薄适配层」。
参考 `rc-sched`：全部逻辑用注入的时间戳测试，不读系统时钟。

### R7 时间必须注入

```rust
// ❌ 无法测试，必然 flaky
fn decide(&self) -> bool { Instant::now() < self.deadline }

// ✅ 时间由调用方传入
fn decide(&self, now_us: u32) -> bool { now_us < self.deadline }
```

## 5. 写代码前必做：找一个参考实现

**这是提高一次通过率最有效的方法。** 不要从零设计，先找结构最接近的现成 crate：

| 你要写的东西 | 读这个 | 学什么 |
|---|---|---|
| 状态机 | [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 枚举代替 bool 组合、时间注入、边界测试 |
| 纯算法 / 数据处理 | [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | 缓冲复用、越界处理、真实 bug 的回归测试 |
| 编解码 | [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | 逐字节测试、错误带偏移量、往返测试 |
| 编解码对称性 | [`rc-fec`](../../../crates/rc-fec/src/lib.rs) | 穷举式验证（每个丢包位置都测） |
| 诊断 / 统计 | [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | 零分配热路径、JSON 导出、百分位 |
| 数据结构 / newtype | [`rc-types`](../../../crates/rc-types/src/lib.rs) | lint 声明、Display/Debug、不变量校验 |

**照抄风格，不要自创。** 如果参考实现里没有的模式，先问人类。

## 6. 文档地图

| 你需要知道什么 | 读 |
|---|---|
| 整体架构、延迟预算怎么分配 | [02-architecture.md](../design/02-architecture.md) |
| 每个 crate 的确切 API 签名 | [20-crate-contracts.md](20-crate-contracts.md) |
| 线格式（逐字节） | [10-wire-format.md](10-wire-format.md) |
| 测试怎么写、测什么 | [30-testing.md](30-testing.md) |
| 延迟怎么测、回环模式怎么做 | [40-diagnostics.md](40-diagnostics.md) |
| 「做完了」怎么判定 | [50-definition-of-done.md](50-definition-of-done.md) |
| 平台坑清单 | [04-platforms.md](../design/04-platforms.md) |
| 编译慢、磁盘大 | [07-engineering-velocity.md](../design/07-engineering-velocity.md) |
| 领任务 | [tickets/README.md](../tickets/README.md) |

## 7. 工作流程

```
1. 领一张任务卡（docs/tickets/Txx-*.md）
2. 读卡片里的「上下文」「文件清单」「API 签名」
3. 读一个参考实现（第 5 节）
4. 写实现 + 写测试
5. cargo check -p <crate>   ← 反复，每次 ~0.1s
6. cargo test -p <crate>    ← 反复
7. cargo test --workspace   ← 提交前
8. 按任务卡的「验收清单」逐项自查
9. 更新任务卡状态，报告结果
```

## 8. 绝对禁止

- 为了让代码编译通过而**删除测试**或放宽断言
- 用 `#[allow(dead_code)]` / `#[allow(unused)]` 掩盖警告
- 把 `unsafe` 塞进业务层 crate
- 在可测逻辑里读系统时钟 / 环境变量 / 网络
- 跨 crate 用 `Box<dyn Error>`
- 承诺「应该能工作」但没跑过测试
- 修改 `rc-types` 的现有字段语义（会破坏所有下游）

## 9. 卡住了怎么办

**不要猜。** 按顺序：

1. 查 [20-crate-contracts.md](20-crate-contracts.md) 里有没有写
2. 查参考实现里有没有同样的情况
3. 查 [04-platforms.md](../design/04-platforms.md) 的坑列表
4. 都没有 → 在代码里写 `// TODO(question): <具体问题>`，在报告里明确提出

「我不知道所以先写个大概」是**最糟**的选择——它会让下一个人花更多时间发现它错了。

## 10. 实测基线

判断「有没有变慢」用（2026-11-19，macOS arm64）：

| 指标 | 实测 | 门槛 |
|---|---|---|
| 单文件增量 `cargo check -p X` | 0.09s | ≤3s |
| `cargo test --workspace` | 1.4s / 107 测试 | ≤90s |
| 冷编译（含依赖） | 52s | ≤8min |
| `target/` 体积 | 151MB（其中增量缓存 123MB，`just clean-disk` 后 29MB） | ≤10GB |

显著变差时，先检查 `.cargo/config.toml` 与 `Cargo.toml` 的 profile 配置是否被改动。
