# 交付索引（给人类看）

> 你要把项目交给别的 agent 实现。这是「它们需要什么」的清单与核对表。

## 一个 agent 从零到提交，只碰这四样

```
1. 一张任务卡        docs/tickets/Txx-*.md
2. 一个参考实现      crates/rc-sched/src/lib.rs（或其他同类）
3. 契约文档          docs/impl/20-crate-contracts.md 的对应小节
4. 完成定义          docs/impl/50-definition-of-done.md
```

**其余文档它不需要读。** 任务卡是自包含的：背景、文件清单、API 签名、
硬性要求、不要做什么、测试清单、验收命令、参考实现、已知坑，全在里面。

## 核对表：每个 agent 都应该能做到

| 能力 | 凭什么能做到 | 证据 |
|---|---|---|
| 知道项目在做什么 | 任务卡「背景」3–5 句 | 每张卡都有 |
| 知道要写哪些文件 | 任务卡「交付物」表格 | 每张卡都有 |
| 知道确切的函数签名 | 任务卡「API 契约」+ `docs/impl/20` | 每张卡都有 |
| 知道哪些事不能做 | 任务卡「不要做什么」 | 每张卡都有 |
| 知道怎么验证 | 任务卡「验收命令」可复制执行 | 每张卡都有 |
| 知道怎么测 | 任务卡「测试清单」列出必须覆盖的用例 | 每张卡都有 |
| 知道什么时候算完成 | 通用 DoD + 卡片「验收标准」 | 两者齐备 |
| 知道会踩什么坑 | 任务卡「已知坑」+ `docs/04-platforms.md` | 每张卡都有 |
| 有可照抄的代码风格 | 6 个可运行参考实现，共 3569 行 | `crates/*/src/` |
| 能跑起来验证 | 107 个测试全绿，`cargo check` 0.09s | 实测 |

## 已交付的具体产物

### 1. 可运行的代码骨架（不是伪代码）

| crate | 行数 | 测试 | 示范能力 |
|---|---|---|---|
| `rc-types` | 148 | 27 | newtype、错误枚举、不变量校验 |
| `rc-diff` | 425 | 13 | 纯算法、缓冲复用、真实 bug 回归测试 |
| `rc-sched` | 650 | 18 | **状态机、时间注入、边界测试** |
| `rc-fec` | 526 | 18 | 编解码对称、穷举验证 |
| `rc-proto` | 615 | 16 | 线格式、逐字节固定向量 |
| `rc-trace` | 598 | 15 | 诊断设施、JSON 导出、百分位 |

**合计 2962 行生产代码 + 107 个测试，全部实测通过。**

### 2. 实现规格（agent 照着写）

| 文档 | 页数 | 内容 |
|---|---|---|
| [00-onboarding](impl/00-onboarding.md) | 197 行 | 入口、7 条硬性规则、参考实现索引 |
| [10-wire-format](impl/10-wire-format.md) | 261 行 | 逐字节布局、固定测试向量、分片规则、错误偏移表 |
| [20-crate-contracts](impl/20-crate-contracts.md) | 607 行 | 12 个 crate 的确切 API 签名与语义 |
| [30-testing](impl/30-testing.md) | 287 行 | 三类必测用例、反模式、覆盖率判据 |
| [40-diagnostics](impl/40-diagnostics.md) | 312 行 | 回环测量原理、报告 schema、Gate 换算、诊断顺序 |
| [50-definition-of-done](impl/50-definition-of-done.md) | 199 行 | 10 项通用 DoD + 6 类专项 DoD + 报告格式 |

### 3. 任务卡（12 张，可独立领取）

| 卡 | 平台 | 前置 | 交付物数 | 测试数 |
|---|---|---|---|---|
| T01 工程基座 | 全 | — | ✅ 已完成 | 107 |
| T02 回环框架 | 全 | T01 | 6 文件 | 12 |
| T03 Windows 采集 | Win | T01 | 8 文件 | 7+3 |
| T04 NVENC 编码 | Win | T01 | 7 文件 | 8+2 |
| T05 解码呈现 | Win | T01 | 7 文件 | 5+3 |
| T06 软件编码 | 全 | T01 | 4 文件 | 8 |
| T07 CLI | 全 | T02 | 5 文件 | 10 |
| T08 M1 串联 | Win | T02–T07 | 5 文件 | 9 |
| T09 QUIC | 全 | T01 | 7 文件 | 12+4 |
| T10 输入通道 | Win | T09 | 6 文件 | 11 |
| T11 Linux 采集 | Linux | T03 | 6 文件 | 7+2 |
| T12 macOS 采集 | macOS | T03 | 7 文件 | 4+2 |

### 4. 工程基座（解决编译慢/磁盘大）

| 文件 | 作用 |
|---|---|
| `Cargo.toml` | profile：`debug=1`（磁盘 -60~80%）、依赖 `opt-level=2` |
| `.cargo/config.toml` | lld/mold 配置（默认注释，装了就开） |
| `justfile` | 日常命令（**可选**，无 just 也有等价命令表） |
| `.config/nextest.toml` | 测试并行与超时 |
| `.github/workflows/ci.yml` | 三平台 fmt + clippy + test |
| `scripts/clean-disk.sh` | 磁盘清理（已加执行权限） |

## 实测基线（2026-11-19，macOS arm64）

| 指标 | 实测 | 门槛 | 余量 |
|---|---|---|---|
| 单文件增量 `cargo check -p X` | **0.09s** | ≤3s | 33× |
| `cargo test --workspace` | **1.4s / 107 测试** | ≤90s | 64× |
| 冷编译（含拉依赖） | **52s** | ≤8min | 9× |
| `target/`（含增量缓存） | **151MB** | ≤10GB | 66× |
| `cargo clippy -D warnings` | **0 警告** | 0 | — |
| `cargo fmt --check` | **通过** | — | — |

## 已完成的验证（可复现）

```bash
$ cargo check --workspace          # 通过
$ cargo test --workspace           # 107 passed; 0 failed
$ cargo clippy --workspace --all-targets -- -D warnings   # exit 0
$ cargo fmt --all -- --check       # 无 diff
```

链接完整性：32 个 markdown 文件，全部相对链接经脚本校验（含大小写精确匹配）通过。

## 已知的边界（诚实说明）

| 项 | 状态 | 影响 |
|---|---|---|
| 真实 DXGI/NVENC/ScreenCaptureKit | **未实现**（T03/T04/T12） | 需要 Windows/macOS 真机，我只有 macOS |
| 回环模式 | **未实现**（T02） | 是 M1 Gate 的前提，但没有它也能继续开发其他 crate |
| QUIC 传输 | **未实现**（T09） | 独立性强，可并行 |
| M1 Gate 数字 | **未测** | 必须真机；文档给出了门槛与排查顺序 |
| 双机延迟测量 | **未做** | M2 之后才需要 |
| RustDesk 对比实测 | **未做** | ADR-0004 的结论基于**源码阅读**，不是运行时测量。该文档 §8 给了 1 天的验证方法 |

**没有模拟或伪造任何未实现的功能。** 所有已交付代码都真实运行、真实测试。

## 2026-11-20 重大更新：读过 RustDesk 源码后的修正

见 [ADR-0004](adr/0004-rustdesk-fork-evaluation.md)。三句话总结：

1. **不 fork RustDesk**（AGPL-3.0 + 190k 行 + 帧率控制是闭环，单点改不动）
2. **但我们原先给出的三条原因，错了两条**——它已经事件驱动采集，编码也不是纯质量优先
3. **找到真正的根因**：接收队列 120 帧（2 秒缓冲）+ `INIT_FPS=15` + 帧率爬升不对称 + 静止画面重复编码 10 次

这四项被固化为 **Q1–Q6 硬性要求**，写进了：

- [02-architecture §0](design/02-architecture.md)（文档开头的第一节）
- [T02 L8–L10](tickets/T02-loopback-skeleton.md)（回环必须验证）
- [T08 专项门槛](tickets/T08-m1-pipeline.md)（M1 Gate 的一部分）
- [T09 `DEFAULT_MAX_CONCURRENT_FRAMES = 2`](tickets/T09-net-quic.md)
- [40-diagnostics 报告 schema](impl/40-diagnostics.md)（`convergence_ms` 等四个新字段）

**给下一个 agent 的最重要一句话：任何队列深度超过 2 帧的设计，都是在复刻 RustDesk 最大的问题。**

## 给下一位 agent 的第一句话

> 读 [`docs/tickets/README.md`](tickets/README.md)，按依赖关系选一张平台匹配的卡，
> 读它全文，然后读它「参考实现」里指定的那个 crate。
> 不要读其他文档，不要自己设计架构，不要改公共契约。
