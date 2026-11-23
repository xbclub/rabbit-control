# 07 工程提速：解决「编译慢、测试慢、占硬盘」

> 本文档直接回应用户最初的痛点。**这些问题全部可以在不解换语言的前提下解决**，且必须作为 M0 的交付物，而不是事后优化。
> 本项目的核心命题是低延迟，而低延迟要求 in-process 媒体管线，这决定了必须用 Rust（见 [ADR-0003](../adr/0003-final-rust-decision.md)）。所以「让 Rust 变快」不是可选项，是前置条件。

---

## 1. 痛点的真实来源

先校正一个常见归因。Rust「慢」通常来自四件事，只有第一件是语言固有的：

| 来源 | 是否固有 | 占比（经验值） | 能否消除 |
|---|---|---|---|
| 泛型单态化 + LLVM codegen | 是 | 30% | 部分（减少泛型爆炸） |
| **单个巨型 crate（改一行编全部）** | 否 | **35%** | ✅ 完全消除 |
| 默认链接器慢 | 否 | 15% | ✅ 完全消除 |
| 过程宏重复编译（serde/tokio 等） | 否 | 15% | ✅ 大部分消除 |
| 未清理的增量产物与多 profile | 否 | 5%（磁盘主要来源） | ✅ 完全消除 |

**85% 是可配置问题。** 下面逐项给出配置。

---

## 2. Workspace 拆分（最重要的一项）

### 2.1 原则

| 原则 | 说明 |
|---|---|
| **叶子 crate 依赖少** | 改动只重编该 crate 及上游，不扩散 |
| **平台层独立** | Windows 改动不触碰 macOS/Linux 代码 |
| **依赖倒置** | 抽象（trait）在下，实现在上；业务层只依赖抽象 |
| **禁止循环依赖** | `cargo` 会强制，但设计上要主动避免 |
| **单 crate <3000 行** | 超过就该拆 |

### 2.2 推荐结构

```
rabbitControl/
├── Cargo.toml                    # workspace root
├── .cargo/config.toml            # 编译加速配置
├── justfile                      # 日常命令
├── crates/
│   ├── rc-types/                 # 纯数据结构、错误类型、枚举。零重依赖
│   ├── rc-trace/                 # 延迟埋点、FrameTrace、结构化日志
│   ├── rc-diff/                  # 脏区分析、帧比对（纯算法，无平台依赖）
│   ├── rc-capture/               # CaptureBackend trait + 工厂（薄）
│   │   ├── rc-capture-win/
│   │   ├── rc-capture-mac/
│   │   └── rc-capture-linux/
│   ├── rc-encode/                # Encoder trait + 码率控制策略（薄）
│   │   ├── rc-encode-nvenc/
│   │   ├── rc-encode-vaapi/
│   │   ├── rc-encode-vt/
│   │   └── rc-encode-sw/
│   ├── rc-decode/                # Decoder trait + 平台实现
│   ├── rc-render/                # 呈现（平台相关，可后拆）
│   ├── rc-sched/                 # 帧调度器（纯逻辑，最易测试）
│   ├── rc-input/                 # 输入捕获 + 注入抽象
│   ├── rc-net/                   # QUIC、FEC、拥塞控制、协议
│   ├── rc-proto/                 # protobuf 生成 + 手写媒体包头
│   ├── rc-session/               # 会话状态机（纯逻辑）
│   ├── rc-loopback/              # 回环测试框架 + 网络损伤注入
│   ├── rc-ui/                    # 界面（Tauri v2 + 静态前端，设计规范见 08-ui-design.md）
│   └── rc-cli/                   # 二进制入口
└── xtask/                        # 构建脚本任务（非 workspace 成员可选）
```

### 2.3 依赖分层（改动的扩散半径）

```
rc-types  ←──────────────── 所有 crate 依赖，但几乎不改
   ↑
rc-trace, rc-diff, rc-sched   ←── 纯逻辑，改动只影响自己 + 上游
   ↑
rc-capture(trait), rc-encode(trait)
   ↑
平台实现 crate                 ←── Windows 改动不影响 mac/linux
   ↑
rc-session, rc-net
   ↑
rc-ui, rc-cli                  ←── 最上层，改动最频繁
```

**关键：`rc-types` 必须保持极稳定。** 它一改，全 workspace 重编。所以：
- 只放数据结构与错误类型
- 不放 trait（trait 放各自的抽象 crate）
- 新增字段用 `#[non_exhaustive]` 或 `Option`，避免破坏性改动

### 2.4 常见拆错的信号

| 信号 | 问题 | 修法 |
|---|---|---|
| 改一个平台文件，三个平台都重编 | 平台实现没分离 | 拆成独立 crate |
| 改 UI，`rc-net` 也重编 | 依赖方向反了 | 抽 trait 到下层 |
| `cargo check` 每次都要编 serde | 某 crate 依赖过重 | 把 serde 依赖推到只需要的 crate |
| `rc-types` 频繁改动 | 结构设计不稳定 | 先定接口，再写实现 |

---

## 3. 编译配置

### 3.1 `.cargo/config.toml`

```toml
# .cargo/config.toml

# 链接器加速（按平台启用，见下）
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

[target.x86_64-apple-darwin]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

# 增量编译（默认开，但显式确认）
[build]
incremental = true

# 并行（默认按核数，巨型机器可调）
[build]
jobs = -1
```

需要安装的链接器：

| 平台 | 工具 | 安装 |
|---|---|---|
| Linux | `mold` | `apt install mold` / `dnf install mold` |
| macOS | `lld` | `brew install llvm`（并确保 `lld` 在 PATH） |
| Windows | `lld-link` | 随 `rustup component add llvm-tools` 或 VS 提供 |

> 注意：`lld`/`mold` 主要用于**链接阶段**。链接时间原本占 20–40%，换后降 50–80%。对 `cargo check` 无影响（不链接），所以**内循环用 `cargo check` 才是关键**。

### 3.2 `Cargo.toml` 的 profile 设置

```toml
# 顶层 workspace Cargo.toml

[profile.dev]
opt-level = 0
debug = 1          # 关键：只保留行号信息，不做完整 DWARF
                   # 完整 debug=2 会让 target/ 暴涨数 GB，且拖慢链接
                   # 需要调试时单独用 dev-debug profile

[profile.dev.package."*"]
opt-level = 2      # 依赖编一次就固定，之后不再重编；运行速度提升 10–50×
                   # 这是 Rust 官方推荐的经典配置

[profile.test]
opt-level = 0
debug = 1

# 需要完整调试信息时才用
[profile.dev-debug]
inherits = "dev"
debug = 2

# 性能测试用（接近 release 但有符号）
[profile.bench]
inherits = "release"
debug = 1
lto = "thin"
codegen-units = 1

[profile.release]
opt-level = 3
lto = "thin"          # fat LTO 编译太慢，thin 是甜点
codegen-units = 1     # 发布版用 1 换性能；日常开发不用 release
strip = "symbols"
panic = "abort"
```

**关于 `debug = 1` 的权衡**：`debug = 2` 会生成完整 DWARF，`target/` 常达 20–40GB 且链接慢 2–3 倍。`debug = 1` 保留行号（backtrace 可用），体积降 60–80%。**这是磁盘占用最大的单点优化。**

### 3.3 磁盘占用治理

| 措施 | 效果 |
|---|---|
| `debug = 1` | **-60%~80%** |
| 依赖 `opt-level = 2`（只编一次） | 减少重复产物 |
| `cargo sweep --time 14` 定期清理 | 移除 14 天未用产物 |
| CI 使用独立 `CARGO_TARGET_DIR=/tmp/target` | 不污染工作区 |
| `.gitignore` 加 `target/` | 防误提交（本项目非 git，但预留） |
| 单一 toolchain（不用 nightly 混测） | 避免双份 target |

参考数值：

| 配置 | `target/` 大小（本项目规模） |
|---|---|
| `debug=2` + 默认 | 25–40 GB |
| `debug=1` + 依赖 opt2 | **5–10 GB** |
| release + bench profile 各一份 | +3–5 GB |

**建议：本地只保留 `dev` + `release` 两个 profile。`bench` 走 CI 或用后即删。**

### 3.4 清理脚本

```bash
# scripts/clean-disk.sh
#!/usr/bin/env bash
set -euo pipefail
echo "before: $(du -sh target 2>/dev/null | cut -f1)"
cargo sweep --time 14 || true          # 14 天未用的产物
cargo sweep --installed || true        # 旧的编译器版本产物
rm -rf target/debug/incremental        # 增量缓存（可安全删除，下次重建）
echo "after:  $(du -sh target 2>/dev/null | cut -f1)"
```

```bash
# 彻底重置（谨慎）
cargo clean && du -sh target   # target 应不存在
```

---

## 4. 反馈回路设计（内循环 vs 外循环）

**这是降低 AI 迭代成本的核心方法。** 不同问题用不同命令，不要用一个命令解决所有事。

| 场景 | 命令 | 预期耗时 | 说明 |
|---|---|---|---|
| 写完一段代码，看类型对不对 | `cargo check -p rc-sched` | **1–3s** | 不做 codegen、不链接 |
| 检查测试代码能否编译 | `cargo check -p rc-sched --tests` | 2–5s | 提前发现测试错误 |
| 跑受影响 crate 的测试 | `cargo nextest run -p rc-sched` | 5–15s | 只编该 crate |
| 跑全量测试 | `cargo nextest run --workspace` | 60–90s | 提交前 |
| 性能基准 | `cargo bench -p rc-sched` | 30s+ | 改了算法才跑 |
| 全平台构建 | CI | — | 本地不做，交给 CI |

### `justfile`

```make
# justfile

# 内循环（最常用）
c *ARGS:
    cargo check -p {{ARGS}}

# 检查测试编译
ct *ARGS:
    cargo check -p {{ARGS}} --tests

# 快速测试
t *ARGS:
    cargo nextest run -p {{ARGS}}

# 全量测试
ta:
    cargo nextest run --workspace

# 格式化 + lint
f:
    cargo fmt --all
    cargo clippy --workspace --all-targets -- -D warnings

# 完整验证（提交前）
v: f ta

# 磁盘清理
clean-disk:
    ./scripts/clean-disk.sh

# 回环自测（本项目专用）
loopback *ARGS:
    cargo run -p rc-cli --release -- loopback {{ARGS}}
```

### 关键纪律

1. **AI 写完代码先用 `cargo check -p <crate>`，不要直接 `cargo build`**
2. **只测改动过的 crate，不要每次全量**
3. **`cargo build` 只在需要运行二进制时用**
4. **绝不在本地跑 `--release` 做验证**（release 编译 2–5 分钟，改动频繁时纯浪费）

---

## 5. 测试策略

### 5.1 `cargo nextest`

```toml
# .config/nextest.toml
[profile.default]
# 并发执行
test-threads = "num-cpus"

[profile.default.junit]
path = "junit.xml"

# 慢测试单独分组，可用 `--ignore` 跳过
[[profile.default.overrides]]
filter = "test(slow_)"
threads-required = 2
slow-timeout = { period = "60s", terminate-after = 2 }
```

`nextest` 相比 `cargo test` 的优势：
- 每个测试独立进程（隔离好）
- 并行调度更优（通常快 2–10×）
- 输出更清晰

### 5.2 测试分层（按运行成本）

| 层 | 内容 | 运行成本 | 何时跑 |
|---|---|---|---|
| 单元测试 | 纯函数、状态机、码率控制算法 | 毫秒 | 每次改动 |
| 组件测试 | 编码器/解码器（用假帧） | 秒 | 改相关 crate |
| 回环测试 | 采集→编码→解码→呈现 | 10–30s | 改媒体层，需 GPU |
| 集成测试 | 双机/模拟网络 | 分钟 | 提交前 / CI |
| 手动验收 | 肉眼观感 | 分钟 | 里程碑 |

**AI 能自动跑前三层。第四层需 CI + 网络模拟。第五层必须人工。**

### 5.3 让 AI 能自测的关键：回环模式

见 [02 架构](02-architecture.md) 第 8 节「回环模式」。这是让 AI 从「只能写代码」变成「能验证效果」的关键设施，**M1 必须优先交付**。

```bash
# AI 可自行执行的验证
cargo run -p rc-cli --release -- loopback \
    --duration 60s \
    --resolution 1920x1080 \
    --fps 60 \
    --inject-loss 3% \
    --report frame-timeline.json
```

输出应包含：

```json
{
  "displayed_fps_p50": 59.2,
  "latency_p50_ms": 19.4,
  "latency_p95_ms": 25.1,
  "latency_p99_ms": 31.7,
  "frames_captured": 3601,
  "frames_encoded": 3601,
  "frames_delivered": 3560,
  "frames_displayed": 3554,
  "drop_reasons": {
    "repeat": 0,
    "stale": 0,
    "budget": 0,
    "network_lost": 41
  }
}
```

**有了这个，AI 可以自主判断「我的改动是否提高了延迟」。这是本项目工程效率的最高杠杆。**

---

## 6. CI 设计

### 6.1 分层 CI

| 触发 | 内容 | 时长目标 |
|---|---|---|
| 每次 push | fmt + clippy + `cargo check --workspace`（三平台矩阵） | ≤6 min |
| 每次 PR | + 单测 + 组件测试 | ≤15 min |
| 每晚 | + 回环测试（自托管 GPU runner） | ≤40 min |
| 手动 | 完整验收脚本 | — |

### 6.2 缓存

```yaml
# .github/workflows/ci.yml（节选）
- uses: actions/cache@v4
  with:
    path: |
      ~/.cargo/registry/index
      ~/.cargo/registry/cache
      ~/.cargo/git/db
      target
    key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
    restore-keys: ${{ runner.os }}-cargo-

- uses: mozilla-actions/sccache-action@v0.0.5
```

`sccache` 的价值：跨分支、跨 worktree 复用编译产物，**在 AI 频繁创建分支/重置工作区的场景下收益极大**。

### 6.3 CI 的 target 隔离

```yaml
env:
  CARGO_TARGET_DIR: /tmp/rc-target
```

避免 CI 缓存与本地干扰，也避免把 CI 产物算进仓库体积。

---

## 7. 降低 AI 试错成本（比提速更有效）

编译时间从 3 分钟降到 3 秒，收益是线性的。但把「AI 需要试错的轮次」从 8 次降到 2 次，收益是**乘性**的。后者更重要。

### 7.1 用类型系统把错误前移

| 手段 | 作用 |
|---|---|
| 状态机用枚举而非 bool 组合 | 「非法状态」不可表示，编译期拒绝 |
| 错误用 `thiserror` 枚举，禁止 `Box<dyn Error>` | 错误处理路径明确，AI 不会漏分支 |
| 句柄用 newtype（`struct FrameSeq(u64)`） | 防止把 seq 和 pts 传反 |
| FFI 资源用 RAII guard | 消除「忘记 Release」类错误（DXGI 头号坑） |
| `#[non_exhaustive]` + `Option` | 兼容性演进不破坏编译 |

示例：

```rust
// 错误示范：bool 组合导致非法状态
struct State {
    is_connected: bool,
    is_authenticated: bool,
    is_streaming: bool,
}
// 可表示 is_streaming=true 但 is_connected=false 这种非法状态

// 正确：枚举使非法状态不可表示
enum SessionState {
    Idle,
    Connecting { started: Instant },
    Authenticating { channel: Arc<ControlChannel> },
    Streaming { stream: MediaStream, started: Instant },
    Closing { reason: CloseReason },
}
```

### 7.2 收窄 unsafe 边界

**规则：`unsafe` 只允许出现在平台 crate 的最内层模块，且每个 `unsafe fn` 必须有 `// SAFETY:` 注释说明前置条件。**

```
rc-capture-win/
├── src/
│   ├── lib.rs              # 安全接口（无 unsafe）
│   ├── backend.rs          # DXGI 逻辑（少量 unsafe，已封装）
│   └── ffi/
│       ├── mod.rs
│       └── duplication.rs  # 所有 unsafe 集中在此
```

这样 AI 改业务逻辑时不会碰到 `unsafe`，改 `unsafe` 时有明确的边界和文档。**配合 `#![deny(unsafe_op_in_unsafe_fn)]` 强制。**

### 7.3 可验证的组件边界

每个 trait 实现都必须能通过「假实现」测试：

```rust
#[cfg(test)]
struct FakeEncoder { frame_time: Duration, ... }

#[test]
fn scheduler_drops_frames_when_encoder_slow() {
    // 不依赖 GPU，纯逻辑验证调度器行为
}
```

**要求：`rc-sched`、`rc-diff`、`rc-session`、`rc-net` 的全部核心逻辑必须能在无 GPU、无网络的条件下 100% 测试覆盖。**

### 7.4 诊断设施前移

| 设施 | 交付时间 | 作用 |
|---|---|---|
| `rc-trace` 帧时间线 | M0 | AI 能读出「哪一环慢」 |
| 回环模式 | M1 | AI 能自主验证效果 |
| 网络损伤注入 | M1（提前于 M3） | AI 能自主测弱网 |
| 编码器自检命令 | M1 | AI 能验证硬件路径真的走通 |

**没有这些，AI 只能写代码不能验证，等于回到「人肉测试」时代。**

---

## 8. 一个具体的对照：优化前后

假设「修改 `rc-sched` 的丢弃策略」这个任务：

| 阶段 | 优化前 | 优化后 |
|---|---|---|
| 改完看类型 | `cargo build` 180s | `cargo check -p rc-sched` **2s** |
| 跑测试 | `cargo test` 240s | `cargo nextest -p rc-sched` **8s** |
| 发现问题，再改 | 180s | 2s |
| 循环 5 次 | 2100s（35min） | 50s |
| 验证实际效果 | 需人肉 + 两台机器 | `just loopback` **30s**，自动出报告 |
| **总计** | **~40 min + 1 次人工** | **~1.5 min，AI 自主** |

**这就是「Rust 开发效率低」的真相：不是语言的问题，是回路没建。**

---

## 9. M0 交付清单（对应 [06 里程碑](06-milestones.md#2-m0-工程基座)）

- [ ] workspace 拆分完成，12+ crate，依赖无环
- [ ] `.cargo/config.toml` 配置 lld/mold
- [ ] `Cargo.toml` profile（dev debug=1、依赖 opt2）
- [ ] `justfile` 全部命令
- [ ] `cargo nextest` 接入
- [ ] `sccache` 接入（本地 + CI）
- [ ] `scripts/clean-disk.sh`
- [ ] CI 三平台 fmt/clippy/check
- [ ] `rc-trace` 骨架可输出结构化事件
- [ ] `rc-loopback` 骨架可跑假数据链路
- [ ] **实测并记录：`cargo check` ≤3s、`nextest` ≤15s、`target/` ≤10GB**

## 10. 常见误判与纠正

| 常见说法 | 事实 |
|---|---|
| 「Rust 编译慢是语言缺陷」 | 主要瓶颈是 crate 粒度与 profile 配置，85% 可消除 |
| 「用 `cargo build` 就行，`check` 不算数」 | `check` 做完整类型检查与借用检查，只是不做 codegen；内循环就该用它 |
| 「release 才是真实性能」 | 是，但日常验证逻辑用 dev 即可；性能测试走 `bench` profile |
| 「`debug=2` 方便调试」 | 常用调试靠 backtrace，`debug=1` 足够；`debug=2` 只在需要逐变量调试时开 |
| 「拆太多 crate 会变慢」 | 拆到 10–30 个范围，链接次数增加但重编范围骤减，净收益显著；超过 50 个才需重新评估 |
| 「换 Go 就没这些问题」 | Go 在编译上确实快，但换语言的代价是把风险转移到 OS 绑定层（见 ADR-0002/0003） |
