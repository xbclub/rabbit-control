# RabbitControl 任务书（AI 唯一入口）

> **你只需要读这一个文件。** 需要细节时按文中指出的路径去查 `docs/reference/`。
> 本文档是**权威**的；`docs/reference/` 是参考材料，与本文冲突时以本文为准。

---

## 0. 三十秒速览

| 项 | 内容 |
|---|---|
| 产品 | 低延迟远程桌面控制软件（类 RustDesk） |
| 语言 | **Rust**（已定，不再讨论） |
| 被控端优先级 | **macOS 为 P0**，Windows 为 P1，Linux 为 P2 |
| 核心命题 | 打赢 RustDesk 的**实际**帧率与延迟 |
| 当前状态 | 6 个基础 crate 已完成（3574 行，107 测试全绿），媒体管线 0 行 |
| 你的第一个动作 | 读完 §1–§5，然后接 §8 的第一张任务卡 |

---

## 1. 必须理解的一件事：我们到底在解决什么问题

RustDesk 的参数上限是 120fps，但实际用起来很卡。**我们读过它的源码**，找到了精确原因（不是猜的）：

| # | RustDesk 的实际做法 | 源码位置 | 后果 |
|---|---|---|---|
| **Q3** | 接收端队列 **120 帧** | `src/client.rs:114` `VIDEO_QUEUE_SIZE` | **画面晚 2 秒** |
| **Q1** | `INIT_FPS = 15`，连接后前 1 秒强制 15fps | `src/server/video_qos.rs:54,828` | 起步只有 15fps |
| **Q4** | 帧率升 `+1`、降用除法（不对称） | `video_qos.rs:546` vs `:523` | 恢复要 9 秒+ |
| **Q5** | 静止时重复编码同一帧最多 10 次 | `video_service.rs:729,935` | 纯浪费 |
| **Q2** | 帧率判定依赖 **1Hz** 网络探针 | `connection.rs:510` | 感知慢 1 秒 |
| **Q6** | 延迟测量被队列污染（源码注释自认） | `video_qos.rs:43-44` | 数据失真 |

**这六条就是我们的产品定义。** Q1–Q6 每一条都必须在代码里有对应实现、在测试里有覆盖、在验收里有可测指标。

详细论证见 [`reference/adr/0004-rustdesk-fork-evaluation.md`](docs/reference/adr/0004-rustdesk-fork-evaluation.md)。

### 1.1 一句话记住

> **任何队列深度超过 2 帧的设计，都是在复刻 RustDesk 最大的问题。**

---

## 2. 六条不可违反的铁律

违反任一条，任务视为未完成（不论测试是否绿）。

### 铁律 1：队列深度硬上限 = 2 帧

- 编码在途：`SchedulerConfig::max_in_flight = 2`（已有）
- 网络重组：`DEFAULT_MAX_CONCURRENT_FRAMES = 2`
- 解码队列：≤2
- 呈现队列：1

**禁止**为了「减少丢帧」调大这些值。丢帧是正确行为。

### 铁律 2：静止画面零编码

捕获返回 `None`（无变化）→ **什么都不做**，继续循环。
禁止重复编码同一帧，禁止为了「保活」发送媒体帧。保活走控制通道心跳。

### 铁律 3：内循环用 `cargo check`，禁止 `cargo build`

| 目的 | 命令 | 耗时 |
|---|---|---|
| 检查类型 | `cargo check -p <crate>` | ~0.1s |
| 跑测试 | `cargo test -p <crate>` | ~1s |
| 全量（提交前） | `cargo test --workspace` | ~1.5s |

`cargo build` 慢 3–10 倍且无额外验值。

### 铁律 4：时间必须注入，禁止在可测逻辑里读时钟

```rust
// ❌ 无法测试，必然 flaky
fn decide(&self) -> bool { Instant::now() < self.deadline }

// ✅ 时间由调用方传入
fn decide(&self, now_us: u32) -> bool { now_us < self.deadline }
```

参考 `crates/rc-sched/src/lib.rs`：18 个测试全部精确、无 sleep。

### 铁律 5：`rc-types` 是稳定层，`#[cfg]` 只在工厂函数

- 改 `rc-types` 会触发全 workspace 重编 → 只放数据结构/错误/newtype，不放 trait
- `#[cfg(target_os)]` **只允许出现在工厂函数内**，业务层必须平台无关

### 铁律 6：不用 ffmpeg，热路径直调系统 API

- macOS：`ScreenCaptureKit` + `VideoToolbox`
- Windows：DXGI Desktop Duplication + NVENC
- **禁止**引入 `ffmpeg-next` / `ffmpeg-sys` / 任何 ffmpeg 绑定
- 软件编码兜底用 `openh264` crate（它不依赖 ffmpeg）

---

## 3. 技术栈与关键决策

| 项 | 决策 | 依据 |
|---|---|---|
| 语言 | **Rust** | [ADR-0003](docs/reference/adr/0003-final-rust-decision.md)：低延迟要求媒体管线 in-process |
| 传输 | **QUIC（`quinn`）**，媒体走 datagram，控制走 reliable stream | [03-protocol](docs/reference/design/03-protocol.md) |
| 不做 | fork RustDesk | [ADR-0004](docs/reference/adr/0004-rustdesk-fork-evaluation.md)：AGPL-3.0 + 190k 行 |
| 编码 | macOS `VideoToolbox`（P0）、Windows NVENC（P1）；**`RealTime=true` 必须设** | [04-platforms](docs/reference/design/04-platforms.md) |
| 采集 | macOS `ScreenCaptureKit`（P0）、DXGI（P1） | 同上 |
| 服务端 | 语言待定（Go/Rust 均可），接口先按 protobuf 定死 | 不阻塞 M1 |

### 3.1 延迟预算（必须逐项兑现）

1080p60 局域网，端到端 **22ms**，SLO 25ms：

| 阶段 | 预算 |
|---|---|
| 屏幕变化 → 采集返回 | 4ms |
| 脏区分析 | 1ms |
| 调度排队 | 0ms |
| 编码（VT/NVENC 最快档） | 5ms |
| 打包 + FEC | 1ms |
| 网络（局域网） | 5ms |
| 重组 + 抖动缓冲 | 1ms |
| 解码 | 3ms |
| 呈现 | 2ms |
| **合计** | **22ms** |

**回环模式（M1 Gate）不含网络段，门槛 = 18ms。**

---

## 4. 工程结构

```
rabbitControl/
├── TASK.md                    ← 你正在读的文件（唯一入口）
├── AGENTS.md                  ← 编排纪律（多 agent 时看）
├── Cargo.toml                 ← workspace + 编译提速 profile
├── crates/
│   ├── rc-types/      ✅ 27 测试   数据结构、错误、ID、像素格式
│   ├── rc-diff/       ✅ 13 测试   脏区检测、重复帧判定
│   ├── rc-sched/      ✅ 18 测试   帧调度器（核心）
│   ├── rc-fec/        ✅ 18 测试   XOR 块 FEC
│   ├── rc-proto/      ✅ 16 测试   媒体包头二进制编解码
│   ├── rc-trace/      ✅ 15 测试   帧时间线埋点
│   └── (待建) rc-capture-*/ rc-encode-*/ rc-decode/ rc-render/ rc-net/ rc-pipeline/ rc-loopback/ rc-cli/
└── docs/reference/    参考材料（31 篇，不要求全读）
    ├── adr/           决策记录
    ├── design/        设计文档
    ├── impl/          实现规格（API 契约、线格式、测试规范）
    └── tickets/       原始任务卡（本文 §8 已重新排序）
```

### 4.1 依赖方向（只能向下）

```
rc-cli / rc-pipeline
     ↑
rc-net, rc-loopback
     ↑
rc-capture-*, rc-encode-*, rc-decode, rc-render, rc-sched
     ↑
rc-diff, rc-fec, rc-proto, rc-trace
     ↑
rc-types   ← 最稳定，几乎不改
```

---

## 5. 参考实现（照抄，不要自创）

写新代码前，**先找一个结构最接近的现成 crate 读一遍**。这是提高一次通过率最有效的方法。

| 你要写的 | 读这个 | 学什么 |
|---|---|---|
| 状态机 / 决策逻辑 | [`crates/rc-sched/src/lib.rs`](crates/rc-sched/src/lib.rs) | 枚举代替 bool、时间注入、边界测试 |
| 纯算法 / 数据处理 | [`crates/rc-diff/src/lib.rs`](crates/rc-diff/src/lib.rs) | 缓冲复用、越界处理、真实 bug 回归 |
| 编解码 / 线格式 | [`crates/rc-proto/src/lib.rs`](crates/rc-proto/src/lib.rs) | 逐字节固定向量、错误带偏移量 |
| 编解码对称性 | [`crates/rc-fec/src/lib.rs`](crates/rc-fec/src/lib.rs) | 穷举式验证（每个丢包位置都测） |
| 诊断 / 统计 | [`crates/rc-trace/src/lib.rs`](crates/rc-trace/src/lib.rs) | 零分配热路径、JSON 导出、百分位 |
| 数据结构 / newtype | [`crates/rc-types/src/lib.rs`](crates/rc-types/src/lib.rs) | lint 声明、不变量校验 |

**照抄风格。** 参考实现里没有的模式，先问，不要发明。

---

## 6. 平台优先级（2026-11-20 变更）

| 优先 | 平台 | 状态 | 说明 |
|---|---|---|---|
| **P0** | **macOS 13+ (Apple Silicon)** | 主线 | 用户主要远控目标；开发机即是 |
| P1 | Windows 10 2004+ / 11 | 次要 | 用户次要目标；需物理机验证 GPU |
| P2 | Linux (X11 / Wayland) | 后续 | 归档，暂不投入 |

**这意味着：**
- 主线任务 = **T12（macOS 采集）→ T08（M1 Gate）**
- Windows 任务（T03/T04/T05/T10）**降为 P1，可延后**
- Linux 任务（T11）**P2，暂不做**

### 6.1 macOS 特有约束（必须提前知道）

| 坑 | 后果 | 处理 |
|---|---|---|
| 三重授权（屏幕录制/辅助功能/输入监控） | 缺一则无法工作 | 启动即检测 + 引导到系统设置 |
| `kVTCompressionPropertyKey_RealTime` 未设 | **延迟 +20–40ms** | 必须设为 `true` |
| TCC 按「签名 + bundle id」记录 | 重编译后权限失效 | 开发期用固定签名 |
| `SCStream` 不可复用 | 停止后 restart 崩溃 | 必须重建实例 |
| Retina 2x | 带宽翻倍 | 按逻辑分辨率下采样 |

---

## 7. 工作流程（每个任务固定六步）

```
1. 读 §8 指定的任务卡（docs/reference/tickets/Txx-*.md）
2. 读 §5 指定的参考实现
3. 写实现 + 写测试
4. cargo check -p <crate>       ← 反复，每次 ~0.1s
5. cargo test -p <crate>        ← 反复
6. cargo test --workspace       ← 提交前
7. 按本文 §9 自检 + 任务卡「验收标准」逐项确认
```

### 7.1 提交前必跑

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

三条全绿才算完成。

---

## 8. 任务顺序（按此执行，不要跳）

### 阶段 A：立起测量骨架（当前阶段，全部可在 macOS 完成）

| 序 | 任务 | 卡 | 产出 | 前置 |
|---|---|---|---|---|
| **A1** | 回环框架骨架 | [T02](docs/reference/tickets/T02-loopback-skeleton.md) | `rc-loopback`，假后端跑通 Q1–Q6 指标 | 无 |
| **A2** | CLI 入口 | [T07](docs/reference/tickets/T07-cli.md) | `rc-cli loopback/probe`，可执行验收 | A1 |

**A 阶段出口指标（全部可在假后端上验证）：**

| 指标 | 门槛 |
|---|---|
| 报告 JSON 字段完整度 | 与 `impl/40-diagnostics.md` §4 schema 逐字段一致 |
| `static_content` 时 `fps.encoded` | **== 0**（Q5） |
| `convergence_ms` | **≤1000**（Q1/Q4） |
| 任意队列深度 | **≤2**（Q3） |
| `seed` 固定后重跑 | 结果完全一致 |
| 报告中的一致性不变式 | 全部成立 |

```bash
cargo run -p rc-cli -- loopback --static-content --duration 30s --report /tmp/a.json
jq '.fps.encoded' /tmp/a.json          # 必须为 0
jq '.gate.passed' /tmp/a.json          # 必须 true
```

> **为什么先做这个**：没有它，后面所有延迟数字都无法验证，只能靠猜。
> 它是「让 AI 能自己判断改得对不对」的关键设施。

### 阶段 B：macOS 真实管线（P0 主线）

| 序 | 任务 | 卡 | 产出 | 前置 |
|---|---|---|---|---|
| **B1** | macOS 采集 + VideoToolbox 编码 | [T12](docs/reference/tickets/T12-capture-macos.md) | `rc-capture-macos` + `rc-encode-vt` | A1 |
| **B2** | 解码与呈现 | [T05](docs/reference/tickets/T05-decode-render.md)（**需改 macOS 版**） | `rc-decode` + `rc-render` | B1 |
| **B3** | **M1 Gate 端到端串联** | [T08](docs/reference/tickets/T08-m1-pipeline.md)（**需改 macOS 版**） | `rc-pipeline`，跑出真实延迟 | B1, B2 |

**B1 出口指标（先于 B2，因为它是产品成立的前提）：**

| 指标 | 门槛 | 不达标怎么办 |
|---|---|---|
| VideoToolbox 单帧编码 P50 | **≤5ms** | 5–8ms 可重分配预算；>8ms 必须调整目标 |
| 采集 1080p60 帧率 | ≥58 | 检查 `SCStream` 配置 |
| 零拷贝验证 | 无 CPU 往返 | GPU 采样器确认 |
| `RealTime=true` 已生效 | 是 | 验证延迟分布 |

**B3 是硬门槛**，必须全部通过才进入阶段 C：

| 指标 | 门槛 |
|---|---|
| 端到端延迟 P50（1080p60，回环，不含网络） | **≤18ms** |
| 端到端延迟 P99 | ≤35ms |
| Displayed FPS | **≥58** |
| 静态画面 CPU（单核） | **≤8%** |
| 拖动窗口时 FPS | ≥55 |
| `unfinished` 帧数 | == 0 |
| `convergence_ms` | ≤1000 |
| `static` 时编码帧数 | == 0 |
| 队列深度 | ≤2 |
| `--encoder-slowdown 2.0` 时延迟 | 不增长 |

### 阶段 C：网络与输入

| 序 | 任务 | 卡 | 前置 |
|---|---|---|---|
| C1 | QUIC 传输 | [T09](docs/reference/tickets/T09-net-quic.md) | 可与 B 并行（无平台依赖） |
| C2 | 输入通道 | [T10](docs/reference/tickets/T10-input-channel.md) | C1 |

**C 阶段出口指标：**

| 指标 | 门槛 |
|---|---|
| 输入延迟（点击→对端响应） | **≤ RTT + 5ms** |
| 100KB 帧分片重组 | 逐字节一致 |
| 3% 丢包下重组成功率 | >95%（FEC 生效） |
| `DEFAULT_MAX_CONCURRENT_FRAMES` | **== 2** |

### 阶段 D：Windows 支持（P1，延后）

| 序 | 任务 | 卡 | 前置 |
|---|---|---|---|
| D1 | Windows DXGI 采集 | [T03](docs/reference/tickets/T03-capture-windows.md) | B3 完成 |
| D2 | NVENC 编码 | [T04](docs/reference/tickets/T04-encode-nvenc.md) | D1 |
| D3 | Windows 输入 | [T10](docs/reference/tickets/T10-input-channel.md)（Windows 部分） | C1 |

**D 阶段出口指标：** Windows 平台达到与 B3 同等的延迟与帧率（NVENC 单帧 P50 ≤6ms）。
弱网表现见 §9.1 的 3% 丢包场景。

### 阶段 E：Linux（P2，暂不做）

[T11](docs/reference/tickets/T11-capture-linux.md) — 归档待命。

### 阶段 F：产品化

软件编码兜底 [T06](docs/reference/tickets/T06-encode-software.md)（CI 无 GPU 时需要）、
服务端、打包、自动更新。详见 [`reference/design/06-milestones.md`](docs/reference/design/06-milestones.md) 的 M5/M6。

**F 阶段（发布前）出口指标：**

| 指标 | 门槛 |
|---|---|
| 画面冻结（>500ms） | 30 分钟会话内 **0 次** |
| 8 小时长跑 | 无崩溃、内存增长 <5% |
| 点击连接到出画面 | <800ms |
| 盲测优于 RustDesk | **≥70% 选 RC**（5 人 × 3 场景） |
| 4K 局域网 | FPS ≥45、P50 ≤60ms |

---

## 9. 完成定义（每项都要有证据）

| # | 检查 | 验证命令 |
|---|---|---|
| D1 | 编译 | `cargo check -p <crate> --all-targets` |
| D2 | 该 crate 测试 | `cargo test -p <crate>` → `N passed; 0 failed` |
| D3 | 全仓不受影响 | `cargo test --workspace` → 数量 ≥ 107 + 新增 |
| D4 | clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| D5 | 格式 | `cargo fmt --all -- --check` |
| D6 | 无新增 unsafe（业务层） | `grep -rn "unsafe" crates/<crate>/src` |
| D7 | 无 `Box<dyn Error>` 跨 crate | `grep -rn "Box<dyn Error>" crates/<crate>/src` |
| D8 | 无 ffmpeg 依赖 | `grep -rn ffmpeg crates/` |
| D9 | 队列深度 ≤2 | 检查相关常量 |

**报告格式（缺「验证证据」一节视为未完成）：**

```markdown
## 完成情况
### 改动文件
### 验证证据
$ cargo test -p rc-xxx
test result: ok. 12 passed; 0 failed
$ cargo test --workspace
合计 119 passed; 0 failed
### 实现说明（关键决策 1-3 句）
### 未完成 / 已知问题（若无则写「无」）
```

### 9.1 产品级验收指标（SLO 矩阵）

**这一节是产品能否成立的判据。** 上面的 D1–D9 是「代码质量」，这一节是「产品能力」。

权威定义见 [`reference/design/01-overview.md`](docs/reference/design/01-overview.md) §4；此处为必须随身携带的摘要。

| 场景 | 分辨率 | Displayed FPS | 延迟 P50 | 延迟 P99 | 画质策略 |
|---|---|---|---|---|---|
| **局域网千兆/有线** | 1080p60 | **60** | **≤25ms** | **≤40ms** | 高质量 |
| 同城 Wi-Fi 6（RTT 20ms） | 1080p60 | ≥50 | ≤50ms | ≤80ms | 平衡 |
| 跨地域（RTT 80ms, 1% 丢包） | 1080p60 | ≥40 | ≤110ms | ≤180ms | 平衡 |
| 弱网（RTT 80ms, **3% 丢包**） | 1080p | ≥30 | ≤150ms | ≤250ms | 降质保帧 |
| 4K 局域网 | 3840×2160 | ≥45 | ≤60ms | ≤100ms | 高质量 |
| **静态画面** | — | — | — | — | **CPU ≤8% 单核** |

#### 附加质量标准（不可协商）

| 指标 | 门槛 | 为什么关键 |
|---|---|---|
| 画面冻结（>500ms 无更新） | 30 分钟会话内 **0 次** | 用户抱怨「卡」的主要来源 |
| `Displayed FPS` 与 `Encoded FPS` 差值 | **<5%** | 差值大 = 帧在半路被丢，编码再快也没用 |
| 会话中断（非网络原因） | <0.5% | 稳定性 |
| 8 小时会话内存增长 | <5% | 长会话可用性 |
| 点击连接到出画面 | <800ms | 启动速度感知 |
| 主观盲测优于 RustDesk | **≥70% 选 RC** | 最终判据（5 人 × 3 场景） |

#### 各阶段负责验证哪些指标

**不要把全部指标堆到 M1——M1 只验证「本机能测的部分」。**

| 阶段 | 验证指标 | 本阶段不测 |
|---|---|---|
| **A**（T02/T07） | 报告 schema 完整、`static` 时编码帧数 ==0、`convergence_ms ≤1000`、队列 ≤2 | 真实延迟（假后端） |
| **B3**（T08） | 局域网 1080p60：**P50 ≤18ms（回环）、P99 ≤35ms、Displayed FPS ≥58、静态 CPU ≤8%、拖动 FPS ≥55** | 网络相关指标 |
| **C**（T09/T10） | 输入延迟 ≤ RTT+5ms | 弱网（需损伤注入） |
| **D**（M2/M3） | 3% 丢包：无冻结、FPS ≥30；连接迁移不断流 | 4K |
| **E**（M4） | 4K：FPS ≥45、P50 ≤60ms；跨平台互连 | — |
| **发布前** | 盲测 ≥70%、8h 长跑、冻结 0 次 | — |

#### 当前唯一未验证的前提

**VideoToolbox 在 `RealTime=true` 下的单帧编码耗时从未实测**（见 §13）。它决定 §3.1 的 5ms 编码预算是否成立，进而决定整张 SLO 表是否可行。

**B1 完成后第一件事：测这个数字并更新 §12。超标则先停下重新评估，不要继续往下做。**

---

## 10. 绝对禁止

| 禁止 | 原因 |
|---|---|
| 引入 ffmpeg 依赖 | 用户明确要求；热路径必须直调系统 API |
| 把队列深度改成 >2 | 复刻 RustDesk 的核心缺陷 |
| 静止画面重复编码 | 违反 Q5 |
| 为了让代码编译通过而删测试/放宽断言 | 等于删掉验证 |
| 用 `#[allow(...)]` 掩盖警告 | 掩盖真实问题 |
| 把 `unsafe` 放进业务层 crate | 业务层必须平台无关 |
| 在可测逻辑里读 `Instant::now()` | 测试会 flaky |
| 跨 crate 用 `Box<dyn Error>` | 调用方无法穷举分支 |
| 返回假数据让功能「看起来能用」 | 下一个人会以为已完成 |
| 修改 `rc-types` 现有字段语义 | 破坏所有下游 |
| 跳过 M1 Gate 直接做网络/Windows | 基线未验证，后续全是空中楼阁 |

---

## 11. 卡住时的处理顺序

1. 查 [`reference/impl/20-crate-contracts.md`](docs/reference/impl/20-crate-contracts.md) 有没有确切签名
2. 查 §5 的参考实现里有没有同类处理
3. 查 [`reference/design/04-platforms.md`](docs/reference/design/04-platforms.md) 的坑列表
4. 查 [`reference/impl/40-diagnostics.md`](docs/reference/impl/40-diagnostics.md) 的排查顺序
5. 都没有 → 在代码写 `// TODO(question): <具体问题>`，**停下并报告**

**禁止自己发明架构。** 契约不一致时报告，不要私自改接口。

---

## 12. 当前实测基线

2026-11-20，macOS arm64（Apple M1 Pro）：

| 指标 | 实测 | 门槛 |
|---|---|---|
| 单文件增量 `cargo check -p X` | **0.09s** | ≤3s |
| `cargo test --workspace` | **1.4s / 107 测试** | ≤90s |
| 冷编译（含依赖） | **52s** | ≤8min |
| `target/` | 49MB | ≤10GB |
| clippy `-D warnings` | 0 警告 | 0 |

显著变差时先查 `Cargo.toml` 的 profile 与 `.cargo/config.toml` 是否被改。

---

## 13. 本阶段最重要的未知数

**VideoToolbox 在 `RealTime=true` 下的单帧编码耗时从未实测。** 这是产品成立的前提：

| 实测结果 | 结论 |
|---|---|
| P50 ≤5ms | 22ms 预算成立，产品可行 |
| P50 5–8ms | 需重分配预算，可行但紧 |
| P50 >8ms | **必须调整目标**（降分辨率或降帧率） |

**B1 完成后第一件事就是测这个数字，并记录到本文 §12。**
如果超标，先停下来重新评估 §3.1 的预算表，不要硬着头皮往下做。

---

## 14. 快速索引

| 我需要 | 文件 |
|---|---|
| 为什么这么设计 | [`reference/design/02-architecture.md`](docs/reference/design/02-architecture.md)（**开头 §0 是 Q1–Q6**） |
| 确切 API 签名 | [`reference/impl/20-crate-contracts.md`](docs/reference/impl/20-crate-contracts.md) |
| 线格式逐字节 | [`reference/impl/10-wire-format.md`](docs/reference/impl/10-wire-format.md) |
| 测试怎么写 | [`reference/impl/30-testing.md`](docs/reference/impl/30-testing.md) |
| 延迟怎么测 | [`reference/impl/40-diagnostics.md`](docs/reference/impl/40-diagnostics.md) |
| 平台坑 | [`reference/design/04-platforms.md`](docs/reference/design/04-platforms.md) |
| 完整里程碑 | [`reference/design/06-milestones.md`](docs/reference/design/06-milestones.md) |
| 为什么不用 Go / 不 fork | [`reference/adr/`](docs/reference/adr/) |
| 原始任务卡全集 | [`reference/tickets/`](docs/reference/tickets/) |
