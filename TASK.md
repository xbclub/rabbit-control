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
| 当前状态 | 21 个 crate（含 `rc-ui` / `rc-session` / `rc-audio` / `rc-tools` / `rc-host-macos` / `rc-net`）+ 独立 `server/backend` 与 `server/web` 已落地；**最新进度（2026-11-23）：Phase 1 多轮对抗复审清零 + 客户端 W2（文件传输）/ W3（多标签 VT100 远程终端）/ W4（TCP 端口映射与按设备自动恢复）全链路接线与 5 轮只读审查全部 `ZERO_ISSUES_FOUND` 闭环**。最新终验：**`cargo test --workspace -- --test-threads=1` 939 passed / 0 failed / 24 ignored**（78 test suites，含 `rc-tools` 24+11+5+2、`rc-ui` 113+50+1+5+12+7+4、`rc-net::mtls_loopback` 5/5）、**`server/backend` 73 passed / 0 failed**（61 unit + 6 `auth_pki_e2e` + 6 `end_to_end`）、`server/web` `tsc --noEmit` + Vite build 0 error（1643 modules）、`crates/rc-ui/ui/assets/js/*.js` `node --check` 10/10 与 `terminal.js` 零 `.innerHTML`、`clippy -D warnings` 0 警告、`fmt --check` 干净。真机 M1 Gate（60s dynamic P50=11.28ms）、2h soak、144/120fps 硬件回环与 `RabbitControl.app`/`dmg` 历史基线详见 §12–§13 |
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
│   ├── rc-types/          ✅ 27 测试        数据结构、错误、ID、像素格式
│   ├── rc-diff/           ✅ 13 测试        脏区检测、重复帧判定
│   ├── rc-sched/          ✅ 18 测试        帧调度器（核心）
│   ├── rc-fec/            ✅ 18 测试        XOR 块 FEC
│   ├── rc-proto/          ✅ 16 测试        媒体包头二进制编解码
│   ├── rc-trace/          ✅ 15 测试        帧时间线埋点
│   ├── rc-capture/        ✅  8 测试        采集抽象层（CaptureBackend + 假后端）
│   ├── rc-capture-macos/  ✅ 11 测试 (+2)   macOS ScreenCaptureKit 采集（+2 真机/权限 ignore，已全绿）
│   ├── rc-encode/         ✅  9 测试        编码抽象层（Encoder + 假编码器）
│   ├── rc-encode-vt/      ✅ 16 测试 (+3)   macOS VideoToolbox H.264 硬编（+3 真机 ignore，已全绿）
│   ├── rc-decode/         ✅  3 测试        解码抽象层（Decoder + 假解码器）
│   ├── rc-decode-vt/      ✅  8 测试 (+2)   macOS VideoToolbox H.264 硬解（+2 真机 ignore）
│   ├── rc-render/         ✅  8 测试        呈现抽象层（Renderer + 假渲染器）
│   ├── rc-render-macos/   ✅  8 测试 (+1)   macOS Metal 零拷贝呈现（+1 真机 ignore）
│   ├── rc-input-macos/    ✅  8 测试 (+1)   macOS CGEvent 输入注入（+1 权限 ignore）
│   ├── rc-loopback/       ✅ 58 测试        回环测量框架、Q1–Q6 验收与 Gate 报告
│   ├── rc-pipeline/       ✅ 13 测试        M1 端到端管线编排（延迟/拖动/静态 CPU 达标，60s FPS 与静屏零编码待收口，见 §12）
│   ├── rc-cli/            ✅ 17 测试        诊断/验收 CLI（loopback / probe）
│   └── (待建) rc-net/（QUIC）、Windows（DXGI/NVENC/输入）、Linux、rc-encode-sw/（软件兜底）
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

#### Windows 统一测试与统一修复清单（2026-11-21 记账，用户裁决：后面统一测、统一修）

进入 D 阶段（拿到 Windows 真机）后**一次性**处理，不做零散补丁。当前全部处于「已记账、待真机」状态：

| # | 待办 | 现状（文件） | 真机上要做什么 |
|---|---|---|---|
| W1 | SendInput 注入实现 | `crates/rc-input-win/src/inject.rs` 骨架恒 `Err`（cfg(windows)，本机不参与编译故未盲写） | 实现 `SendInput` 注入 + `KEYEVENTF_SCANCODE`，补齐 `injector_failure_is_returned_not_panicked` 的 win 侧真跑 |
| W2 | I10 注入后 `GetCursorPos` 校准 | 同上，仅 extern 声明 + TODO（`inject.rs:134-138`） | 注入后读光标位置比对、超差重注/上报；按卡内硬性条目验收 |
| W3 | Windows 捕获器 | `crates/rc-input-win/src/capture.rs` 骨架（`WH_MOUSE_LL` 需专用消息循环线程） | RawInput / 底层钩子捕获 → `InputEvent`；坑 3/4/5（多显示器负坐标、布局差异用扫描码、钩子消息循环）见 T10 卡 |
| W4 | `cargo test -p rc-input-win` 真跑 | macOS 上编译为空、**0 假测试**（骨架里刻意不写空过测试） | 卡内 11 项测试名在 win 侧全量真跑 |
| W5 | T10 卡真机手动验收（Windows 部分） | 未执行（`T10-input-channel.md:157-171`） | 1000Hz 划动包数实测、Shift 大写、Win 键、连点 10 次、Ctrl+Alt+Del 特殊通道 |
| W6 | UAC 提权窗口注入 | T10 卡坑 2（需 SYSTEM） | 记 TODO 或按需实现 |

**纪律**：W1–W5 在 Windows 真机到位前**不写任何 cfg(windows) 实现代码**——本机编译不经过，写了就是未验证代码（先测后拆）；到机后按本清单开一轮 workflow 统一实现、统一测试、统一回写本表。

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

#### 关键性能前提实测结论

VideoToolbox `RealTime=true` + `EnableLowLatencyRateControl=true` 单段编码与 B3 真机全链（`sck → videotoolbox → videotoolbox → metal`，隐私幕布，`max_in_flight=2`）均已完成深度调优与真机终验。**2026-11-21 M1 Gate 终验全项通过**（终验配置：回退 SCK PTS 节拍过滤 + `max_in_flight=2`，隐私幕布 72fps 低熵动画供给）：动态 60s **P50 = 11.283ms** / P99 = 21.203ms / **70.0fps** / 队列峰值 2 / 4201 帧全部上屏（budget=0、stale=1）/ `unfinished=0` / 不变式与账目全过；运动 15s P50 = 11.164ms / P99 = 20.431ms / 69.47fps / 1042 帧全上屏；静态 30s 单核 CPU 稳态均值 **1.51%**（全均值 2.13%、峰值 18.2% 仅启动段）/ `encoded=displayed=122`（全部为真实系统变化，`dropped_repeat=0`，Q5「静止画面重复编码」红线未破）。单段编码 P95 从 17.98ms 降至 10.72–11.29ms（P50 9.22–9.40ms）。详细数据、修正记录与幕布工具用法见 §12 表与 §13。

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

2026-11-20（2026-11-21 复测更新），macOS arm64（Apple M1 Pro）：

| 指标 | 实测 | 门槛 |
|---|---|---|
| 单文件增量 `cargo check -p X` | **0.09s** | ≤3s |
| `cargo test --workspace` | **274 测试全绿（含 4 doctest，0 failed，另 9 项真机/权限 `#[ignore]`） / clippy `-D warnings` 0 警告**（2026-11-21 终检复测；**T09 后复测：301 全绿 / 0 failed**，增量来自 rc-net 的 23 单测 + 4 回环集成；**T10 后复测：328 全绿 / 0 failed**，增量来自 rc-input 的 16 单测 + rc-input-macos 新增 11 项（scancode 表 3 + InputBridge 桥接 8）） | ≤90s |
| 冷编译（含依赖） | **52s** | ≤8min |
| `target/` | 49MB | ≤10GB |
| clippy `-D warnings` | 0 警告 | 0 |
| **`rc-capture-macos` SCK 60fps 真机测试（2026-11-21，`macos-dev-run.sh`）** | **`FrameMailbox`（latest-frame-wins 替换 FIFO `sync_channel` + `capturesAudio=false`）后两项全通过**（`2 passed; 0 failed`, 2.48s）：`sck_captures_at_60fps` 2s 采集 **149 帧**（**74.5fps**，≥116 ✅），`dt_us` min=8333 / p50=16666 / max=25000（单模块首测 max=16667；终验 first15=[8333, 16666, 16667, 8333, 16667, 8333, 16667, 16666, 8334, 16666, 8334, 16666, 16667, 8333, 16667]）；`sck_stream_rebuilds_after_stop` **通过** ✅（历史旧基线：107 帧 / 串行 112 帧/2s 未达标） | ≥116 帧/2s ✅；rebuild ✅ |
| **VideoToolbox 1080p 编码延迟（input→output，300 帧，60fps 节奏提交）** | **2026-11-21 `EnableLowLatencyRateControl = true` 调优实测**（`kVTVideoEncoderSpecification_EnableLowLatencyRateControl = kCFBooleanTrue`）：修前 P50 = 8.06ms / P95 = 17.98ms / max = 20.35ms；修后 Run1 P50 = **9.22ms** / P95 = **10.72ms** / min = 5.93ms / max = **14.62ms**（`submit→cb` P50 = 8.67ms / P95 = 10.01ms），Run2 P50 = **9.40ms** / P95 = **11.29ms** / min = 6.00ms / max = **16.13ms**（`submit→cb` P50 = 8.86ms / P95 = 10.30ms）；**P95 降幅 −37%～−40%**，紧单峰分布、0 自动插 IDR，3 项真机 `#[ignore]` 全过；同期修复 `annexb.rs` 4B AVCC 长度 `256..=511`（`00 00 01 XX`）误判为 Annex-B 起始码缺陷（历史对照：2026-11-20 基线 P50=7.84ms / P95=32.5ms；无限 GOP 修复后 P50=8.06ms / P95=17.98ms / max=19.02ms，见 §13） | 见 §13 |
| **VideoToolbox 1080p 解码段（B2 实测，300 帧，含 Annex-B→AVCC+装帧+提交+回调等待）** | **P50 = 0.93ms，P95 = 1.74ms**（release，主 agent 复跑） | ≤3ms ✅ |
| **Metal present 路径（vsync off，1080p NV12 零拷贝）** | **真实窗口完整路径（2026-11-21 `window_present` 实测，60fps 节奏，含 `nextDrawable`）**：300 帧 P50 = **0.338ms**、P90 = 0.549ms、P99 = **1.017ms**、max = 9.082ms；**60fps × 60s soak（3600 帧，wall = 60.0s）**：present P50 = **0.353ms**、P99 = **1.407ms**、max = 8.924ms、超时隙帧 = **0**，`rc-trace` `displayed=3600`、`unfinished=0`、`evicted=0`、`e2e_p50=352µs`、`e2e_p95=681µs`、`e2e_p99=1390µs`（对照：此前离屏层 drawable 预取 300 帧 P50 = 0.23ms、P99 = 0.87ms） | ≤2ms ✅ |
| **编码→解码往返（1920×1080，IDR+2P）** | **Y 平面 PSNR = 56.18dB** | >35dB ✅ |
| **M1 Gate Phase A：`max_in_flight` A/B 选优（6s 隐私幕布真机全链，release，1080p60）** | 4 项修复（含 `cap_space` Condvar 替代 1ms 轮询退避）后实测：`max_in_flight=2` P50 = 14.274ms / P95 = 23.804ms / P99 = 28.282ms / displayed = 72.33fps（`captured=434`、`dropped_budget=0`、`max_queue=2`）；**`max_in_flight=1`（选定并落地 `host.rs`）** P50 = **13.866ms**（复测 13.847ms）/ P95 = 22.034ms / P99 = **24.434ms**（复测 26.271ms）/ displayed = 59.50fps（复测 60.33fps，`captured=430`、`dropped_budget=73`、`max_queue=1`）。两者双达标，按 P50 更低选 `max_in_flight=1`（P50 −0.408ms、P99 −3.848ms、队列峰值降为 1） | P50 ≤18 ✅ FPS ≥58 ✅ |
| **M1 Gate：1080p60 动态 60s（B3 真机全链 `--backend real`，隐私幕布，`max_in_flight=1`，`uuremote_active=false` [测前 0.2%/测后 0.1%]）** | **未通过**（`gate_passed=false`）：**延迟与队列已达标**：P50 = **14.476ms**（≤18 ✅，较旧基线 19.087ms 降 4.61ms）、P95 = **22.955ms**、P99 = **26.572ms**（≤35 ✅）、队列深度峰值 = **1**（≤2 ✅）、`unfinished = 0`（✅）、`decode_errors = 0`（✅）、`dropped_stale = 0`（✅）；`breakdown_ms`：encode **11.658** / decode **2.915** / present **0.961** / diff **0.909**（旧 4.49ms） / network **0.193** / packetize **0.003** / sched_queue **0**。**未通过项**：① `captured_fps = 70.73fps`（`sched_stats.captured = 4244` 帧，ProMotion 120Hz 下 8.33/16.67ms 交替放行）触发 `rc-trace` 4096 环形缓冲驱逐前 148 帧（116 已显示 + 32 `dropped_budget`），且 `run_real()` 丢弃 `controller.displayed()`，致 trace 侧 `captured=4096 / displayed=3286 (54.77fps) / dropped_budget=810` 与 `sched.encoded=3402` 报不变式失败，并使 `convergence_ms` 虚高至 **3090ms**（>1000 ❌）；② 即使修正截断，真实 `encoded=3402`（**56.70fps**，842 次 `dropped_budget`）仍 <58fps ❌（历史首轮基线对照：P50=19.087ms / P99=28.753ms / 50.22fps / conv=1990ms / 队列 2 / diff 4.49ms / encode 10.75ms） | P50 ≤18 ✅ P99 ≤35 ✅ FPS ≥58 ❌ conv ≤1000 ❌ 队列 ≤2 ✅ |
| **M1 Gate：窗口拖动 15s（B3 真机全链 `--backend real`，隐私幕布，`max_in_flight=1`，`uuremote_active=false`）** | **全项通过**（`gate.passed = true` ✅）：`displayed_fps` = **60.27**（≥55 ✅，`captured_fps = 71.73`）、P50 = **14.193ms**（≤18 ✅）、P95 = **21.969ms**、P99 = **24.200ms**（≤35 ✅）、`convergence_ms = 1000ms`（≤1000 ✅）、`dropped_stale = 0`、`max_queue_depth_seen = 1`、`unfinished = 0`（✅）；`breakdown_ms`：encode **11.532** / decode **2.816** / present **1.088** / diff **0.701**（较旧基线 12.81ms 骤降 12.11ms） / network **0.248** / packetize **0.005** / sched_queue **0**（历史首轮基线对照：47.80fps / P50=25.459ms / P99=38.577ms / dropped_stale=13 / diff 12.81ms） | FPS ≥55 ✅ P50 ≤18 ✅ conv ≤1000 ✅ unfinished 0 ✅ |
| **M1 Gate 终验：1080p60 动态 60s（B3 真机全链 `--backend real`，隐私幕布 72fps 低熵动画，`max_in_flight=2`，PTS 过滤回退后）** | **全项通过（exit 0，零失败项）**：**P50 = 11.283ms**（≤18 ✅）、P95 = 16.764ms、**P99 = 21.203ms**（≤35 ✅）、captured 70.02fps / encoded=displayed **70.00fps**（≥58 ✅，4201 帧全上屏：budget=0、stale=1、lost=0、decode_err=0）、`convergence_ms = 1000ms`（≤1000 ✅）、队列深度峰值 **2**（≤2 ✅）、`unfinished = 0`、不变式全过；`breakdown_ms`：encode **10.296** / decode **2.024** / diff **0.995** / present **0.250** / network 0.037 / packetize 0.002 / sched_queue 0 | 全项 ✅ |
| **M1 Gate 终验：持续运动 15s（同上配置）** | **全项通过（exit 0）**：P50 = **11.164ms**、P95 = 17.299ms、P99 = **20.431ms**、displayed **69.47fps**（≥55 ✅）、1042/1042 帧全上屏（budget=0、stale=0）、`convergence_ms = 1000ms` ✅、队列峰值 2、`unfinished = 0`；刺激源为全屏低熵连续运动（与「窗口拖动」同类持续运动刺激，`captured = encoded = displayed` 证零损耗） | 全项 ✅ |
| **M1 Gate 终验：静态画面 30s（B3 真机全链，纯黑真静屏幕布）** | **静态出口条件达标**：单核 CPU 稳态均值 **1.51%**（≤8% ✅，全均值 2.13%、峰值 18.2% 仅启动段、稳态序列 1.0–1.5%）；`encoded = displayed = 122`（4.07fps，全部为菜单栏时钟等**真实系统变化**——`dropped_repeat = 0`，Q5「静止画面重复编码」红线未破；较旧基线 156 再降 22%）、`unfinished = 0`；exit 1 为静态画面 fps 门的**正确语义**（无运动内容不应产出帧） | CPU ≤8% ✅ repeat=0 ✅ |
| **M1 Gate：静态画面 30s（B3 真机全链 `--backend real`，纯黑真静屏/隐私幕布，`max_in_flight=1`，`uuremote_active=false`）** | **未全过（CPU 达标，零编码未达标）**：单核 CPU 均值 **2.52%**（2.5172%，≤8% ✅，稳态区间 0.2%–0.6%，较旧基线 11.72% 显著下降）、峰值 **19.0%**（29 采样）；`captured = 183`、`encoded = displayed = 156`（≠0 ❌，较旧基线 989 降 84%，`dropped_repeat = 0`）、`unfinished = 0`（✅）；启动前 2s（CPU 13.2%/5.5%）及第 14–15s（CPU 19.0%/17.2%，`convergence_ms = 15110ms`）有两次系统合成突发，且 `run_real` 固定 `DiffMode::TrustPlatform`、`SckBackend` 无帧内容去重，`report.rs::apply_gate` 未切静态零编码分支（历史首轮基线对照：encoded=989 / 32.97fps / 单核 CPU 11.72% / 峰值 14.1%） | encoded ==0 ❌；CPU ≤8% ✅ |
| **M1 Gate：10s 真机全链冒烟（历史首轮对照，`--backend real`，调优前）** | 链路跑通无崩溃，Gate 未通过（exit 1）：P50 = 19.545ms、P95 = 25.798ms、P99 = 29.236ms、`captured=encoded=displayed=451`（45.1fps）、`convergence_ms=null`、队列深度 1、unfinished=0；`breakdown_ms`：encode 10.84 / diff 4.84 / decode 2.20 / present 0.42 / network 0.10 / packetize 0.01 / sched_queue 0ms | 历史对照 |
| **M1 Gate：1080p60 动态 60s（历史对照，`--backend auto`）** | **通过**：P50 = **9.20ms**、P99 = 20.60ms、59.9fps、convergence 1000ms、队列深度峰值 1、unfinished 0、不变式全过。后端形态：采集 Fake + 编码 Fake×WallEncoder + 解码/呈现 Fake（权限解锁前基线，保留作调度器与不变式对照） | P50 ≤18 ✅ P99 ≤35 ✅ FPS ≥58 ✅ |
| **M1 Gate：静态 30s（历史对照，`--backend auto`）** | 正确拒绝（encoded=0，Q5/L8 零编码，假后端验证） | ==0 ✅ |
| **M1 Gate：编码器减速 4×（20ms 成本）60s ×2（历史对照，`--backend auto`）** | 机制验证通过：预算丢帧 1730/1728、吞吐 31.2fps≈编码器容量、队列深度 1、unfinished 0、**账目/不变式全干净**；p50 26.1/26.5ms ≈ 20ms 编码成本 + diff/解码/交接 ~6ms，60s 稳定不涨（旧逐出策略同场景 p50 83ms 活锁） | 有界不增长 ✅（卡修正①：2.0×→4.0×，见 T08 卡） |
| **T09 rc-net：3% 丢包回环（QUIC over 本地 UDP 丢包代理，`tests/loopback.rs::connection_survives_packet_loss`）** | **2026-11-21 实测**：双向 3% Bernoulli 丢包（`StdRng` seed=42，QUIC 握手/ACK 同样经历丢包，代理实测丢率 2.2%），**300 帧 × 4 片 + 每帧 1 XOR 冗余，发送窗口 = 2**（与 `max_in_flight`/N10 同口径）：**297–300/300 帧重组成功（99.0–100%）**、少数帧按 N6 在 500ms 内判丢（`LostInNetwork` 埋点闭环），丢包持续期间 probe 帧带 FEC 重发送达（连接存活）；整测 ~1s，**20 连跑 0 失败**（曾用 100 帧 × 6 片配置：丢包落点跨运行重排致帧失败数 1–6 波动、贴 95% 线 flaky，已按小组数 × 大帧数重配） | 成功率 >95% ✅ |
| **T09 rc-net：100KB 帧分片重组（`large_frame_spans_many_packets`）** | 100KB 帧 → **92 片**（`max_media_payload(1200)=1120B`），QUIC datagram 端到端**逐字节相同** ✅；N2 防线实测：1153B 数据报被 `send_datagram` 拒绝、1152B 边界接受；FEC 路径预算 `max_media_payload_with_fec=1088B`（冗余包=最长数据报+自身 32B 头，不留则超 N2 上限——实测踩坑） | 逐字节相同 ✅ |
| **T09 rc-net：Pacer 平滑（`pacer_smooths_burst_sends`，纯逻辑注入时间）** | 100 包 × 1250B @10Mbps（间隔恰 1ms）：首 poll 只放行 **1** 包，放行完全部 100 包耗时 ≥ **50ms**（计算值 100ms 的一半）且 ≤ 200ms | ≥ 计算值一半 ✅ |
| **T09 rc-net：`cargo test -p rc-net` / clippy** | **23 单测 + 4 集成 = 27 项全绿（0 failed）**，`cargo clippy -p rc-net --all-targets -- -D warnings` 0 警告，`cargo fmt --check` 干净，无 `unsafe`（`#![deny(unsafe_code)]`） | 全绿 ✅ |
| **T09 审计修复 1：urgent 输入独立流超越控制 backlog（`tests/loopback.rs::urgent_input_beats_control_backlog`，本地回环 UDP）** | **2026-11-21 实测**：控制流灌 200 × 64KB（12.8MB）大流量、第 100 条处插入 1 条 urgent：`send_reliable_urgent` 调用本身 **9µs**（专职写线程入队，不被控制流反压挡住）；urgent 交付时仅 **82/200** 条控制消息在其之前（**超越 ~118 条 / 7.6MB 在途 backlog**）；紧随其后 `urgent_and_control_message_boundaries_are_intact`（40 × 交错变长消息 1B~3KB）验证两条流各自边界完整、各自有序。机制：第二条双向 reliable 流 + `set_priority(1)` + 专职写线程（`quinn` connection 级流控窗口全流共享，`write_limit()` 见 quinn-proto streams/mod.rs:254——控制洪流钉死连接信用时直接 `write_all` 会连本地缓冲都进不去，专职写线程吸收该等待） | 超越后半批 ✅ 边界完整 ✅ |
| **T09 审计修复 2：N9 rebind 迁移（`tests/loopback.rs::connection_survives_rebind_migration`，本地回环 UDP）** | **2026-11-21 实测通过**：直连对（客户端持端点）→ 迁移前 2 帧媒体基线 → `client_ep.rebind(新 UDP socket)`（quinn `Endpoint::rebind`，连接 ID 不变，模拟 NAT rebinding）→ 迁移后 6 帧媒体（发送窗口 ≤2，Q3 口径）**全部到齐、逐字节正确**；`PathChanged` 事件如约上报；双向 reliable round-trip（client→host 控制 + host→client ack）正常；迁移后 `stats()` 可用。服务端 `migration(true)` 此前已开（quic.rs） | 迁移存活 ✅ 媒体不中断 ✅ |
| **T09 审计修复：`cargo test -p rc-net` / workspace / clippy / fmt 复测** | **23 单测 + 7 集成 = 30 项全绿（0 failed，原 27 项全保留）**；`cargo test --workspace` **348 passed / 0 failed**（46 个 result 组求和，`grep -oE "[0-9]+ passed"` 口径）；`cargo clippy --workspace --all-targets -- -D warnings` 0 警告；`cargo fmt -p rc-net --check` 干净；无 `unsafe`；`DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动 | 全绿 ✅ |
| **T10 输入通道：1000Hz 鼠标发包率（离线确定性测试 `rc-input/src/channel.rs::thousand_hz_mouse_stays_under_300_msgs_per_second`）** | 模拟 1000Hz 移动持续 1s（注入式时间，每 1ms 一拍），4ms 批窗口下消息数断言 **≤300**（理论 250）：**通过**（0 failed）。属**离线确定性证明**——真机「1000Hz 鼠标划动 10s ≈2500 包」验收（卡内手动清单）需 Windows/macOS 真机捕获接入后（D3/会话层）执行，本轮未跑真机 | ≤300 包/秒 ✅（离线） |
| **T10 输入通道：`cargo test -p rc-input -p rc-input-win -p rc-input-macos` / clippy / fmt** | **rc-input 16 通过（11 项为卡内清单测试名照抄）+ rc-input-macos 18 通过 / 1 `#[ignore]` 真机 + doctest 1 通过；rc-input-win 在 macOS 编译为空、0 测试（按裁决不写空过假测试）**；`cargo clippy -p rc-input-win --all-targets -- -D warnings` 与全仓 clippy 0 警告；fmt 干净；rc-input 无 `unsafe`（`#![deny(unsafe_code)]`，grep 仅命中 deny 行本身）；windows 骨架（`inject.rs`/`capture.rs`）经 `rustc --extern rc_input/rc_types`（指向 target 内真实 rlib）离机类型检查 0 错误 | 全绿 ✅ |

| **修复轮：macOS 捕获器 + 真机 3 项全过（rc-input-macos，授权后实测）** | CGEventTap（ListenOnly 不拦截系统事件）+ 专用线程 CFRunLoop + `AXIsProcessTrusted` 预检；离线 31+2 全绿。真机（`accessibility/input_monitoring` 授权后）：① `inject_moves_cursor` **tap 见证**通过；② `capture_inject_loopback_real_machine` 闭环 **注入 8 → 捕获 8 → 通道合并 5 → 解码注入 5**（容忍 21 条批外系统噪声，关键事件有序子序列断言）；③ 延迟探针 20/20 样本 **min=0.29ms / p50=0.66ms / avg=2.19ms / max=14.9ms**（capture←inject 单程，本地即 RTT≈0 场景，远低于 RTT+5ms 预算） | 离线 ✅ 真机 3/3 ✅ |
| **修复轮收口：urgent 流应用层接线（复审发现，主 agent 手工修复）** | rc-input `send_events`/`pump_receiver` 切到 `send_reliable_urgent`/`NetEvent::ReliableUrgent`（输入绝不与控制消息同流）；接收端对控制通道 `Reliable` 视而不见；MockTransport 覆写 urgent（默认实现委托 send_reliable 会掩盖接线缺失）；接缝测试 `input_goes_over_urgent_stream_not_control` 钉死：输入走 urgent、共享流为空、控制消息零注入 | N6/I1 端到端成立 ✅ |
| **rc-input-macos 审计修复：滚轮量纲统一为行（真机 LINE 单位回环）** | **2026-11-21 实测**（`./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture`，授权 dev 证书）：注入侧 `CGEventCreateScrollWheelEvent2` 单位 `PIXEL→LINE`（`ffi/mod.rs::new_line_scroll_event`，`CgPoster::post` 走它）；机理实测（本机 darwin 27.2 探针）：**PIXEL 单位构造 w1=3 的事件 tap 读回 AXIS_1=1（系统按 ~10px/行换算，10× 缩水）；LINE 单位读回恰 =3**。新真机测试 `wheel_line_unit_roundtrip_real_machine`：注入 3 条（垂直 +3/−2 行、水平 +2 行）→ tap 捕获**等值匹配 3/3**（delta/轴逐一相等），`dropped_channel_full=0` ✅；离线全链锁定 `wheel_line_units_survive_capture_to_injection_chain`（RawCgEvent→raw_to_input→InputBridge→PlannedEvent→CGEvent 用 tap 同字段读回逐值相等）+ 构造级 `line_scroll_event_roundtrips_tap_fields`（PIXEL 回归时 3/±2 值域当场爆）。rc-input `MouseWheel.delta` 文档澄清：规范单位=行，Windows WHEEL_DELTA=120/格 D 阶段除以 120 归一（仅文档，源码未动） | 回环逐值对称 ✅ 真机 4/4 ✅ |
| **rc-input-macos 审计修复：tap 事件通道有界化（无界→`sync_channel(256)` 丢最旧）** | `ffi/tap.rs`：回调→消费通道从无界 `channel()` 改 `sync_channel(TAP_CHANNEL_CAP=256)`；满载走 `send_drop_oldest`（`try_send`→`try_lock`+`try_recv` 丢最旧→重投，**全非阻塞原语**，消费侧持锁时降级丢当前；CGEventTap 回调阻塞会被系统摘除 tap）；丢弃在 `TapCtl.dropped_channel_full`（AtomicU64）计数，经 `CgCapture::dropped_channel_full()` 暴露（与 `dropped_unmapped` 两个独立诊断面，本 crate 无日志依赖故以 API 暴露替代日志）；离线测试：`send_drop_oldest_drops_oldest_and_counts`（丢最旧/降级丢当前/计数各路径）+ `tap_channel_capacity_is_256` + `dropped_channel_full_reads_tap_ctl_counter`（接线与两计数不串台） | 回调零阻塞 ✅ 丢弃可观测 ✅ |
| **rc-net 审计修复 1：loss_rate 主动结算（`quic.rs::ArrivalTracker` 单测注入式 3% 丢包 + `loopback.rs::connection_survives_packet_loss` 断言，本地回环 UDP 丢包代理）** | **注入式单测**（60fps/每帧 4 片/确定性丢包 `k*37%100<3`=精确 3%）：1s（60 帧）内 loss_rate **>0**（修复前恒 0.0——nack 原本只在跟踪表攒满 4096 帧时产生，60fps/3% 丢包要 30-60 分钟），2s 后量级断言 ∈(0.01,0.06)；重复投递同一分片不得把缺包帧判完整（4 片帧到 3 片 + 3 次重复 0 号片 → 结算恰 3 arrival/1 nack）。**集成实测**（3% 双向代理、seed=42、实测 UDP 丢率 0.022）：结算完 2s 后 host 侧 `loss_rate` 三连跑 **0.0274/0.0208/0.0224**（量级断言 0.01–0.06 ✅）；机制：`ArrivalTracker` 按 `packet_idx` 去重（`HashSet`）+ 首片时刻起 500ms（`TRACK_STALE_US=FRAME_STALE_US`）未凑齐即按「实到/缺失」喂 `LossEstimator`，`poll()` 每轮无条件 `settle`（u32 µs `wrapping_sub` 回绕安全，时钟故意用截断而非 `Timestamp::since` 饱和——8h soak 不冻结）；表上限 4096 兜底结算最老序号窗口 + `highest_retired` 防迟到分片复活已结案帧 | 几秒内非零 ✅ 量级正确 ✅ 去重 ✅ |
| **rc-net 审计修复 2：流角色按 QUIC 流 ID 判定（`control_is_first` 纯函数单测 + `loopback.rs::stream_roles_never_swap_under_lossy_handshake`，5% 丢包握手）** | 纯函数：`(0,1)→Some(true)`、`(1,0)→Some(false)`（乱序到达：先收到后开的流，index 0 仍判 control）、`(5,5)→None`（同流两次=协议异常）；`accept()` 先取两条流再 `order_by_stream_index`（`RecvStream::id().index()` 小者=control——`StreamId::index()=id>>2` 同发起方单调递增，是打开顺序的权威记录）；`connect()` 侧 `open_bi` 单调递增天然有序。集成：经 5% 丢包代理握手（重传可颠倒 `accept_bi` 交付顺序）后 control/urgent 各 20 条交错消息全部落在各自 `NetEvent::Reliable`/`ReliableUrgent` 上且各自有序 | 乱序不互换 ✅ |
| **rc-net 审计修复 3+4+5：回绕/卫生/有界（`reassemble.rs`+`fec_adapter.rs` gc 单测、`urgent_enqueue_error` 单测）** | ③ u32 µs 回绕：两处 gc 改 `wrapping_sub`，跨回绕对（`t0=u32::MAX-10_000`，`t0+500_000` 已绕回）恰 500ms 不丢/501ms 丢（重组层）、恰 500ms 不清/501ms 清（FEC 组）——`saturating_sub` 版本在此算出经过时间≈0 永不触发；④ `#[allow(too_many_arguments)]` 移除（`ReliableStream{send,recv}` 收拢成对参数）、Drop 先 `urgent_tx=Option::take` 关闭再 join（写线程免等一轮 50ms `recv_timeout`）；⑤ urgent 无界→`sync_channel(64)`，满队 `try_send→Err→Error::Network("urgent queue full...")`（**报错不静默丢**：rc-net 不知输入语义，丢弃/合并决策留上层，文档写在 `URGENT_QUEUE_CAPACITY` 与 `send_reliable_urgent`），`urgent_queue_is_bounded_and_full_maps_to_error` 钉死容量 64 与错误映射 | 回绕安全 ✅ allow 移除 ✅ 有界+报错 ✅ |
| **rc-net 审计修复轮：`cargo test -p rc-net` / clippy / fmt 复测** | **32 单测 + 8 集成 = 40 项全绿（0 failed，原 30 项全保留；新增 7 quic 单测 + 2 gc 回绕单测 + 1 集成）**，全套 3 连跑稳定；`cargo clippy -p rc-net --all-targets -- -D warnings` 0 警告；`cargo fmt -p rc-net --check` 干净；无 `unsafe`（grep 仅 `#![deny(unsafe_code)]` 与文档）；`DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动；`cargo check --workspace` 通过（下游 rc-pipeline/rc-input 等编译不受影响；正式门禁由编排脚本统一复跑） | 全绿 ✅ |
| **审计二轮修复 1：ArrivalTracker 在途优先于结案拦截（rc-net，`quic.rs::inflight_frame_outranks_retirement_gate`）** | **乱序不再误报丢包**：修复前顺序（`highest_retired` 拦截在在途表查找之前）下，新回归测试实测 **FAILED**（`tracked_frames()` left:1 right:0，帧 9 滞留在途表；arrivals=1≠3，500ms 后 settle 会记幻影 nack）——测试真锁住 bug；修复后通过：帧 9 首片入表 → 单片帧 10 抢先结案推 `highest_retired=10` → 帧 9 第 1 片迟到**仍凑齐结案**，`LossEstimator` 只记 arrival 不记 nack（loss_rate 保持 0.0）；「已结案且不在途」迟到分片仍被忽略（既有 `duplicate_fragments_do_not_complete_missing_frame` 等测试无回归；所有结案路径均先 remove 再 note_retired，不变式成立） | 乱序凑齐 ✅ 无幻影 NACK ✅ 无回归 ✅ |
| **审计二轮修复 2：InputBridge 批内错误隔离（rc-input-macos，`bridge.rs`）** | `inject` 从 `convert(ev)?`/注入 `?` 提前返回改为 `first_err.get_or_insert` 记首错继续（与 rc-input `pump_receiver` 同语义；粒度差异：pump 按消息、inject 按事件）。新测试 ①`side_button_error_does_not_drop_sibling_key_events_in_batch`：批 [X1 按下, Key A 按下, Key A 松开] 返回 Err（含「侧键」）且假投递器**精确记录两条 Key 事件、顺序保留**（旧代码 posted 为空必失败）；②`inject_failure_does_not_drop_sibling_events_in_batch`：FlakyPoster 首投失败后同批后续仍被尝试（calls==3）且返回首错（含 `macOS inject failed`）。既有「单事件坏批返回 Err」两测试未破坏 | 坏事件不吞兄弟 ✅ 防卡键 ✅ |
| **审计二轮修复：workspace 全仓复测（workflow 脚本内建终复跑）** | `cargo test --workspace` **369 passed / 0 failed**（较上轮 366 +3：rc-net 1 + rc-input-macos 2）；`cargo clippy --workspace --all-targets -- -D warnings` 0 警告；`cargo fmt --check` 干净；独立复核员另亲跑 crate 级 `cargo test -p rc-net --lib quic::tests`（11 全绿）与 `cargo test -p rc-input-macos --lib bridge`（12 全绿）交叉印证 | 全绿 ✅ |
| **M2 写卡轮（2026-11-21，workflow「M2 写卡轮」）** | 架构定案：新 crate **rc-session**（proto 控制协议 / host_session / client_session / probe / watchdog / reconnect / report / testkit；依赖方向 rc-session→rc-net+rc-pipeline+rc-input+rc-types，无环）；任务卡 **T13–T22 十张**覆盖 M2-3/5/6/7/8/9/10 全部剩余项，评审一轮 13 项修复后通过；契约（§7.5–§7.7 +672 行）/tickets 清单/里程碑三处同步 | 十卡可领取 ✅ |
| **M2 实现轮（2026-11-21，workflow「M2 实现轮」T13→T18 串行六卡）** | T13 proto 线格式 12 测试；T14 会话核心 26 测试（状态机/握手/三通道分发/停机序）；T15 探测与初始码率 23 测试（RTT/时钟偏移/BwProbe/SetBitrate 链路实测：协商 50Mbps→估速 13.98Mbps→编码器实收 clamp(×0.7)=9.79Mbps）；T16 IDR 可靠通道 15 测试（看门狗 500ms/去抖/关联确认/丢包演练：1s 全丢窗后 17ms 恢复、持续丢包 2s 仅 4 请求）；T17 断线重连 19 测试（host 保持窗口/resume/退避重连/强制 IDR/rebind 迁移/70min 刷新守卫）；T18 CLI 双进程 22 测试（fake localhost release 实测：首帧 51.4ms/p50 3.51/fps 59.97/输入单向 0.48ms，重连演练 recovery 47ms）。workspace 484→**490 全绿**（含 MTU/时钟两修复轮），独立复核逐卡通过 | 六卡全过 ✅ |
| **M2 修复 1：分片预算 MTU 错配（T18 发现，workflow 修复轮）** | rc-pipeline host.rs:437 硬编码 MAX_PAYLOAD=1200 → 数据报 1232B 超 rc-net N2 预算 1152B，**真 QUIC 上多包大帧全被拒收**（M1 loopback 直连无预算墙故未暴露）。修复：PipelineConfig.max_packet_payload 参数化（默认 1200 保 loopback/M1 逐字节不变），rc-session 握手处按 rc-net 预算 1120 接线；e2e：64KB 帧 = 59 包过真 QUIC 逐字节重组 + datagrams_sent==59×frames 回归杀手断言；变异验证双向（改回 1200 测试即挂） | 489 全绿 ✅ 复核通过 ✅ |
| **M2 修复 2：真采集时钟锚定顺序（主 agent 真机定位，workflow 修复轮）** | 症状：真后端双进程 30s **0 帧编码**（in_flight=0、IdrRequest×322 饿死），而 SCK 直测 137 帧/2s 正常、M1 loopback 正常。根因链：real_host_stack 先 select_capture_labeled（origin=None → SCK 以首帧 PTS 为零点）后设 pcfg.origin → 每帧 capture_ts 比 now 旧 Δ（首帧延迟），rc-sched deadline=capture_ts+22ms 恒已过 → **帧帧 Drop(Stale)**。修复：采样 origin → 写 pcfg → 再建 SCK（与 rc-loopback:264-265 同构）；修复后实测 host **captured 2055→encoded 2055、超期/重复/预算 0/0/0**；附带 HostRunSummary +captured/dropped_stale/dropped_repeat/dropped_budget 四统计（本轮排查盲飞的教训） | 490 全绿 ✅ 复核通过 ✅ |
| **M2 本机实测：localhost 双进程真后端（Run B 60s，640x360 小窗防遮挡，幕布 72fps 供给，release，dev-app 双 bundle）** | **Gate 全项通过（client exit 0）**：首帧 **630ms**（≤800 ✅）、p50 **10.8ms**（≤25 ✅）、p99 **18.5ms**（≤40 ✅）、displayed **68.3fps**（≥54 ✅）、输入单向 p50 **1.29ms**（≤RTT/2+5=6.3 ✅）、队列峰值 0（≤2 ✅）、RTT 2.56ms、零丢包/零解码错/零未结；host：captured==encoded、弃帧全 0、in_flight 峰值 2 | **Gate 全过 ✅** |
| **M2 本机实测：Run A（30s 同场景）与 Run C（1080p 90s 遮挡序）** | Run A：68.5fps/p50 10.6/输入 1.34，但首帧 1306ms 与 p99 290ms 超标——Run B 60s 稀释后 p99=18.5 证实为**冷启动期尖峰**（degradation 1160ms）；首帧两轮 630/1306ms 方差 → <800ms 门槛贴线，待优化（候选：accept 等待期预建 SCK 流/预热编码器）。Run C（1080p 90s）：首帧 677 ✅/p50 17.2 ✅/p99 26.3 ✅/输入单向 3.08 ✅/零丢包/host 6172→6172 编码零弃帧；displayed 45.5fps 系单屏遮挡自反馈伪影（client 全屏窗呈现内容=采集内容自身，真双机无此回路；1080p 帧率以 M1 loopback 70fps + Run B 68.3fps 拼合论证） | 1080p 延迟达标 ✅ fps 伪影已归因 |
| **M2 本机实测：rc-cli #[ignore] 真机测试（dev-app 手工打包 host_client 测试二进制）** | `localhost_real_backend_smoke` ✅ + `real_inject_via_input_bridge` ✅（2 passed / 37.24s）。发现 dev-run.sh `test` 子命令只打包**最后一个**测试二进制——tests/ 下多二进制 crate 的 --ignored 用例会静默漏跑（本次用手工打包绕过，已列为脚本待修项） | 2/2 ✅ |
| **高帧率支持：120/144/240 参数化验证（2026-11-22，用户需求「我要 120 144 240」；全链参数审查 + 本机实测 + 离线覆盖）** | **参数路径零硬编码**：SCK `SckConfig.fps`→`minimumFrameInterval`、VT `ExpectedFrameRate`（随 PipelineConfig.fps）、会话协商 `min(requested,caps)`、pipeline `fps>0` 无上限、curtain `--fps N`——120/144/240 全程参数直传。**本机 120fps 真机实测**（幕布 120fps 供给，真供 1200 帧/10s）：① 640x360×120 双进程 30s——host captured 3382 == encoded 3382（弃帧全 0）== client displayed 3380（零丢失），**端到端 114.6fps 无损穿透**（瓶颈=SCK 供给 ~115fps，ProMotion 量化，非管线）；p50 13.5ms / 首帧 744ms / 输入单向 1.24ms。② 1080p×120 双进程 90s——**SCK 供给 107.3fps（9602 帧）、VT 编码吞吐 98.9fps（8857 帧，745 帧被预算门正确拦截）**：本机硬编物理上限 <120fps（VT 1080p 单帧 P50≈9.2ms > 8.33ms 帧间隔），1080p×120 需降码率/降分辨率或更强芯片；displayed 45.5fps 为单屏遮挡自反馈伪影（不采信）。**144fps 离线全链**（fake 后端 loopback 6s）：**Gate 通过，displayed 144.0fps 满帧 / p50 10.0ms / 队列峰值 1**。**240fps 离线全链**：displayed 200.2fps（fake WallEncoder 模拟编码节奏 ~5ms/帧所限=83% 达成）、p50 17.8ms、队列 2——**240fps 帧间隔（4.17ms）下调度/管线/呈现全部健康**。144/240 真机验证待外接高刷屏（记账） | 120 ✅（114.6 无损）；144 ✅（离线满帧）；240 管线健康 ✅ |
| **VT 过载丢帧可恢复化（2026-11-22，workflow「编码器过载丢帧可恢复化」；1080p×120 实测暴露的产品级 bug）** | 症状：1080p×120 首测 host 报 `encoder error: VideoToolbox 丢弃了本帧（RealTime 过载）` 后**会话终止**（Error::Encoder 恒 is_fatal → host.rs 上抛 → client 断线 resume 被拒）。修复：rc-types 新增 `Error::EncoderDroppedFrame`（is_fatal=**false**，铁律 5 同步测试）+ rc-encode-vt Dropped 分支改返新变体且**不再置 idr_requested**（Dropped=编码器未编此帧、参考链无损，无需 IDR；Timeout=输出已产生被弃、参考链真断，保留 IDR 不变——复核员逐行论证）+ rc-pipeline Err 路径记 DroppedStale 结局后继续。离线：`encoder_overload_drop_does_not_stop_pipeline`（注入 3 帧丢帧→管线存活、槽位释放、后续帧继续）+ 变异验证（is_fatal 改 true 测试即挂）。**真机复测**：1080p×120 90s 会话存活，745 过载帧被预算门拦截、28 次 IdrRequest 全部消化。workspace **492 测试全绿 / clippy / fmt 干净**，独立复核通过 | 会话存活 ✅ 过载可恢复 ✅ |
| **M2 代码评审轮（2026-11-22，workflow「M2 代码评审轮」：六路模块评审 + 27 条逐条独立确认，0 推翻）** | 范围=133fff0 之后全部未提交代码（rc-session 全 crate + rc-cli host/client + 四 crate 增量 + 三修复交互 + 高帧率路径）。**成立 27 条：high 1 / medium 10 / low 16**。High：Detached 期一条脏控制消息可杀 host 进程（`tick_detached` 的 decode `?` 上抛 × run/CLI 遇错即退，无鉴权窗口下攻击面真实）。Medium 四族：①时钟纪元族（RefreshGuard 生产阈值下第二次刷新数学上不可能触发（base 钉死，8.4e9>u32::MAX）；CLI client 钟从不重锚，71.6min 冻结 u32::MAX，看门狗/探测全停摆；重连退避锚点跨纪元回绕→70min 僵尸重连）②错误路径族（client 多处 `?` 裸抛绕过 S8/S6 停机序列；Detached 期 accept 非 Timeout 错误（未认证 TLS 探测包）杀 host）③InputAck 关联族（key_batch_seq 跨会话不复位 + 断线在途批错位→伪单向延迟样本）④Resume 窗口成功 resume 后从不重开（第二次断线必走全量重建慢路径）+ 捕获错误 unwrap_or_default 静默吞（I9 可诊断性）。Low 16：统计口径（datagrams_sent 混入探测块 / events_injected 少记 / displayed_fps 双向偏差）、median_i64 溢出、测试子进程不清理、dev 证书非环回无警告、justfile ui 入口与 rc-ui exclude 打架、RecordingInjector 无界等。全部发现带 path:line 证据并经独立确认员复核（评审报告卡留档） | 评审完成 ✅ 待修复 |
| **评审修复轮（2026-11-22，workflow「评审修复轮：27 条清零」：四卡修复 → 全仓门禁 → 对照复核）** | 卡A host+reconnect 11 项：high 脏消息杀 host→消化+计数（dirty_candidates/accept_errors，坏候选≠坏会话）；RefreshGuard **wrapping 纪元差修法**（u32 base + `wrapping_sub >= threshold`，双纪元各触发一次有测试）+ 新会话重锚 base + pending 在途抑制；ResumeWindow reopen（resume/新会话后重开，两轮断线两轮 resume）；pending_epoch_reset 消费点迁至「Streaming 首条对端证据」（Welcome 丢失重试仍带 bit1）；先检后拆；current_bitrate/key_batches/events_injected/probe_datagrams 口径五修；Backoff 亚毫秒拒绝。卡B client 4 项：裸 `?` 全族改「置脏→完整停机→Err」（1 字节 0x09 数据报/session_id 不符 Welcome/poll 错误均不再卡 Streaming）；resume 清 pending_input_acks + 全新重建归零重计（与 host 全新会话复位对齐）；捕获错误计数+限流 stderr；双 Bye 先到生效。卡C 4 项：CLI client 钟可变锚点 + session_id 变更即重锚（71.6min 冻结根除）；median_i64 i128 中转；displayed_fps 口径（分母首帧起算实测窗、分子剔排空帧）；fake 钟单采样锚。卡D 5 项：MIN_PACKET_PAYLOAD=16 构造期校验（16×65535≈1MiB 才可能 panic）；测试超时 kill 双子进程；dev 证书非环回警告；justfile ui 入口注释停用（T21 接入后恢复）；RecordingInjector 10 万上限丢最旧。**变异验证**：卡B 5 处逐项移除修复对应测试全转红。workspace **528 passed / 0 failed / clippy / fmt 干净**，复核员对照 27 条逐项核验通过 | 27 条清零 ✅ 复核通过 ✅ |
| **T19 M2-5 鉴权实现轮（2026-11-22，workflow 6 阶段 + 独立只读审计 + 1 项修复）** | 协议 0x0D `AuthChallenge`（21B 定长，`iterations` 1..=1_000_000）/ 0x0E `AuthResponse`（33B 定长，0x0F 保持预留拒收）；手写纯 Rust SHA-256（FIPS 180-4）/ HMAC-SHA256（RFC 4231）/ PBKDF2-HMAC-SHA256 公开标准向量逐字节匹配，零第三方加密依赖（`rc-session/Cargo.toml` 仅新增 `rand = "0.8"` 用于 `OsRng` 16B 一次性盐）；`verify_proof` 常数时间比较；`HostAuthenticator` 5 行决策表 + `ConnectionApprover`（全项目唯一非阻塞审批抽象，`AutoApprover`/`DenyApprover`/`ScriptedApprover`）+ `ClientAuthenticator`（忽略过程 `Welcome{ChallengeRequired}` 的 `granted`，注入时钟超时）；`HostSession`/`ClientSession` 接线（A5 无原因文本上线+立断、A6 鉴权前零起管线、A8 每拍 `poll_approval`+断线 `cancel`、A11 流期鉴权消息致命 `Error::Protocol`、A2 重连强制新盐、无密码+AutoApprover 线上仅单条 `Welcome{Ok}` 与 T14 同形）；`rc-cli` `--password` 双端接线；**localhost fake 后端 CLI 冒烟**（`127.0.0.1:19905`，debug）：正确密码 `hunter2` 退出 **0**（Gate 通过，175 帧编码，61.2fps），错密码 `wrong` 退出 **1**（即时拒收）。workspace **577 passed / 0 failed / 15 ignored**（`rc-session` 184 项全绿），`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --check` 干净 | 577 全绿 ✅ CLI 冒烟 ✅ 审计闭环 ✅ |
| **T20 M2-8 剪贴板文本同步实现轮（2026-11-22，workflow 5 阶段 + 独立只读 Review 0 缺陷）** | 协议 0x0F `ClipboardNotify`（`[0x0F][len u16 LE][text UTF-8]`，`MAX_CLIPBOARD_TEXT_BYTES=65535`，空串与 NUL 透明传输，超限 `InvalidConfig` 整条拒不截断，15 变体全量 roundtrip）；`clipboard.rs` 纯逻辑引擎（首拍基线吞掉 CB6 / leading-edge 零延迟立发 CB4 / 100ms 单槽最新胜合并 / `last_written` 回声抑制与入向去重 CB5 / 对端后到权威作废 pending / 非文本忽略 CB7 / 时间全注入 CB8，11 单测全绿）；`clipboard_macos.rs`（CarbonCore Pasteboard 纯 C API，`com.apple.pasteboard.clipboard` 非 NULL、`PasteboardSynchronize` 先行、RAII `CFRelease`，rc-session 唯一 `unsafe` 模块且 `#![deny(clippy::undocumented_unsafe_blocks)]`，仅 macOS 新增 `core-foundation = "0.10"`）；`HostSession`/`ClientSession` 对称接线（默认关闭 CB1、Stream A CB2、T17 重连重设基线、进程内双会话 4 项集成全绿 + 2 项 `#[ignore]` 真机剪贴板用例就绪）；`rc-cli` `--clipboard <off\|on\|scripted>` + 两侧 `session.clipboard` JSON 报告 + `clipboard_scripted_sync_over_real_quic` 真 QUIC 双子进程 E2E 通过。workspace **598 passed / 0 failed / 17 ignored**（`rc-session` 200 passed + 2 ignored），`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 干净，铁律 4 与 unsafe 边界自检干净，`rc-net`/`rc-pipeline`/`rc-input` 零改动 | 598 全绿 ✅ Review 0 缺陷 ✅ 真 QUIC E2E ✅ |
| **T22 M2-7 被控端托盘 + 接受弹窗实现轮（2026-11-22，workflow 4 阶段 + 独立 Review 捉 3 虫并修复）** | 复用 T19 唯一 `ConnectionApprover`（不另立抽象/不加控制消息号）；`rc-session` client 握手与鉴权默认超时放宽至 **45s**（`approval_timeout + 15s`，U6）+ `testkit::MockApprover` + `tests/localhost.rs` 8 项审批真 QUIC 集成全绿；新增 workspace 成员 `rc-host-macos`（`objc2 0.6` / 绑定 `0.3` 对齐 `rc-render-macos`，零 serde/Tauri/托盘库；纯逻辑 `state.rs`/`bridge.rs` 零 unsafe 7 单测 + macOS `appkit.rs` `NSApplication` Accessory / `NSStatusItem` 32×32 模板图 / 3 项 `NSMenu` / `NSAlert` 激活前置 / 10ms 手泵循环，`unsafe` 全集中 `appkit.rs` 且 `#![deny(clippy::undocumented_unsafe_blocks)]`）；**真机 GUI `tray_bench --run` 通过**（主线程断言、`TrayHost::new`、两态切换、`NSAlert` 文案与 `["接受","拒绝"]` 按钮断言、桥往返、干净退出）；`rc-cli` `--tray`/`--approval <prompt\|auto\|deny>`/`--approval-timeout` 接线 + 3 项 CLI 测试全绿（含 `--approval prompt` 缺 `--tray` exit 2）。独立 Review 捉 3 虫（Pending 时钟冻结、`UiSide::resolve` 缺代次防陈旧误批、`run_tray` quit 绕过 U7 停机序）全修并补 5 回归测试；workspace **620 passed / 0 failed / 17 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 干净 | 620 全绿 ✅ `tray_bench` 真机 ✅ Review 闭环 ✅ |
| **T21 M2-6 控制端 UI 接线轮（2026-11-22，workflow 5 阶段 + 独立 Review 捉 3 处 UAF/竞态并闭环）** | `crates/rc-ui` 正式并入根 `Cargo.toml` `members`（移除 `exclude`）；`tauri.conf.json` 收紧 CSP + `withGlobalTauri`；纯逻辑模块 `model.rs`/`hud.rs`/`input_bridge.rs` 零 unsafe + `metal_attach.rs` 唯一 unsafe 模块（`#![deny(clippy::undocumented_unsafe_blocks)]` + `// SAFETY:`，对齐 `objc2 0.6`/`objc2-quartz-core 0.3`）；`session_bridge.rs`（零 `tauri` 导入、有界 64/256 通道满载丢最旧且保留关键输入、`watch_only` 零注入、`Bye` 优雅退线程）+ `tests/session_flow.rs` 5 项进程内真 QUIC 集成全绿（+ 3 项 `#[ignore]` 真窗口手动项）；`state.rs`/`commands.rs`/`main.rs` 8 项 IPC + Retina `scale_factor` 物理像素换算 + 50ms 后台轮询 `session://update` + 前端 `bridge.js`/`app.js`/`session.js`/`portmap.js` 真会话接线（静态预览零回归、零 `package.json`/`node_modules`）。**独立 Review 捉出 3 处 `CAMetalLayer*` 跨主线程 UAF/竞态**：引入 `LayerLease`（`Arc<Mutex<Box<dyn Renderer>>>`）+ 单调会话代次 ID + `LayerAttachment` 状态机根治，补 6 项确定性回归测试；workspace **652 passed / 0 failed / 20 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 干净 | 652 全绿 ✅ UAF/竞态闭环 ✅ 零只读层改动 ✅ |
| **M2 出口：2 小时 localhost soak（2026-11-22，冻结 release `/tmp/soak-2h/rc-cli`，`nohup` 脱离挂起，`127.0.0.1:19901`，fake 1080p60，host 7230s / client 7200s）** | **硬指标全过**：总运行时长 **7211s**（2h，240 采样），**0 崩溃**，host `Bye` 优雅收尾（`host_exit=0`、`connected_ms=7200009`、`unfinished=0`、`decode_errors=0`、`lost_in_network=0`、`watchdog_fires=0`、`in_flight_peak=2`、`max_queue_depth_seen=0`）；**内存增长 <5%**：host 稳态中位 30.1 MB → 末段 18.5 MB（**−38.48%**，min 11.5/max 30.1）、client 22.8 MB → 18.2 MB（**−20.24%**，min 14.0/max 22.9）；延迟 `connect_to_first_frame=48.4ms`（≤800 ✅）、`p50=3.98ms`（≤25 ✅）、`p99=8.52ms`（≤40 ✅）、`rtt=458µs`、`input_one_way_p50=0.231ms`（✅）；**70.5min 时钟刷新线实跑验证通过**：无缝重建 `session_id` `32284606464→32284606465` 并归零会话钟，第二纪元编码 179,975 帧（采集 179,981，超期弃 6 / 重复 0 / 预算 0，client 实收 201.9 MB 零丢包）。**暴露 2 处报告统计口径遗留（非运行时故障）**：① client 报告 `displayed=4096`（受 `rc-trace` 4096 环形缓冲封顶）致 `displayed_fps=0.95`；② 70.5min `Bye(Refresh)` 预期重建的 `resumed=false` 被 Gate 误标失败致 `client_exit=1`（见 §13） | 2h 0 崩溃 ✅ 内存 <5% ✅ 70.5min 刷新 ✅ |
| **一体端 `rc-ui` 实现轮（2026-11-22，workflow 4 阶段 + 独立只读 Review 0 缺陷；零 CLI 控/被控合一 + 局域网自动发现）** | `crates/rc-ui/src/lan_discovery.rs`（纯 `std::net::UdpSocket` 组播 `239.255.77.77:19999` `RCBC` v1 二进制信标、零新外部依赖、本机过滤、注入时钟 TTL 过期、自动探测真实局域网 IPv4）；`crates/rc-ui/src/host_bridge.rs`（`HostHandle` 内置单会话 `HostSession`、端口 9901 占用自动回落 `:0`、`OsRng` 8 位无歧义随机验证码、会话结束自动轮换、`UiApprover` 复用 T19 `ConnectionApprover` + 单调代次门控、`ClockOrigin::Pending` 每连重锚）；`session_bridge.rs` 透传 `password` 进 `ClientAuthConfig`；`tests/unified_flow.rs` 3 项端到端集成（正密+审批通过、错密零弹窗拒收、审批拒绝断开）；`state.rs`/`commands.rs`/`main.rs` 新增 7 个 IPC 命令与 `host://update` / `lan://devices` 推送；前端 `bridge.js`/`app.js`/`session.js` 接线（P2 本设备真开关/真局域网地址/真验证码与复制、侧栏/P3 局域网在线设备自动发现与直连、应用内「接受/拒绝」审批弹窗）；顺修 Review H9（`core:window:allow-start-dragging`）、M8（`session_snapshot`）、M9（删 `ui/.zcode/`）。workspace **661 passed / 0 failed / 20 ignored**（`rc-ui` 41 passed + 3 ignored），`clippy -D warnings` 0 警告，`cargo fmt --all -- --check` / `node --check` / unsafe 边界全干净，17 个 IPC 命令与事件独立复核 0 缺陷 | 661 全绿 ✅ Review 0 缺陷 ✅ 零 CLI 一体端 ✅ |
| **M2 总清零修复轮（2026-11-22，两轮评审 28 项缺陷分 3 模块修复 + 独立只读复核 28/28 通过）** | **Phase 1 `rc-session`（10/10，+14 回归测试，226 passed）**：H1 未鉴权 `ReconnectRequest` 不再拆旧管线且有密码时拒快恢复强制新盐握手、H2 Detached 候选与 `AwaitingResponse` 超时防锁死、H3 `Rejected` 补调 `approver.cancel()`、M1/M5 `transport.poll` 错误隔离与 50ms flush、H6 `ClipboardChannel::reset_epoch` 跨纪元不冻结、H4 `duration_us: u64` + 无界 `total_displayed`（消除 2h soak 4096/71.6min 封顶）、H5 `ReconnectStats::refreshes` 独立统计（消除 `Bye(Refresh)` Gate 误报）、H8 `frame_headers` 改取 `.1`（`encode_done_ts`）恢复 `breakdown_ms.encode`、M7 client 超时遇钟归零重锚。**Phase 2 `rc-host-macos` + `rc-cli`（5/5，52 passed）**：M2 `AppKitTray::run` 每拍 `autoreleasepool`、M3 非阻塞模态会话（`beginModalSessionForWindow`/`runModalSession`）10ms 轮询 `cancel()` 与 `QuitRequested`、H7 `run_tray` 显式驱动遇 `TornDown` 退出回等待、M6 `ClockOrigin::Pending` 每拍重锚、L1 删 `client.rs` 冗余 `pump_clipboard` 并改 `reset_clipboard_epoch`。**Phase 3 `rc-ui`（13/13，49 passed + 3 ignored）**：M4 裸指针改 `pub unsafe fn` + `MainThreadMarker` + 安全租约包装、U-H1 `HostHandle` 显式驱动防 Detached 卡死、U-M1 `observed_pw_ver=0`、U-M2 `ApproverState` 粘性 `stopped`、U-M3/U-M4 `socket2` 设 `SO_REUSEADDR`/`SO_REUSEPORT` + `multicast_loop_v4(true)` + 网卡变更周期重入组播、U-H2/U-H3 `(sid, SessionUpdate)` 代次门控消旧 `Ended` 误关新窗、U-M5 `set_host_enabled(false)` 补发 `ApprovalCancelled`、U-M6 `host->lan` 严锁序消 TOCTOU、U-M7 观看模式动态同步与输入门控、U-M8 异常结束延迟 1500ms 关窗、U-L1 过期局域网设备高亮回退。workspace **689 passed / 0 failed / 20 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` / `node --check` / unsafe 边界全干净，独立复核 **28/28 零遗留** | 28/28 清零 ✅ 689 全绿 ✅ 复核通过 ✅ |
| **Rust 通信服务 Phase 1（2026-11-22，`server/backend/` 独立 Rust workspace，部署实测于 `172.16.1.2` Ubuntu x86_64）** | **架构**：Axum 0.8（`:21118`）+ `mimalloc` + PostgreSQL 16（`sqlx` 迁移，幂等签发 9 位设备码）+ 可选 Redis 7（`EphemeralStore` 默认进程内 `DashMap`，配 `--redis-url` 自动切 Redis）+ **零 WebSocket**（原生 `[u32 BE len][JSON]` TCP 信令 `:21116` TCP + UDP `RCSP`/`RCSA` 公网探测 `:21116` UDP）+ **Linux `splice(2)` 零拷贝 TCP 中继**（`:21117` TCP，`RCR1` 握手、`256KiB` 内核管道、`TCP_NODELAY`、`SHUT_WR` 半关闭，非 Linux 回落用户态 32KiB 拷贝）。**实测（Mac → `172.16.1.2`）**：双模式验证通过（PG16+Redis 与 PG16+默认内存，均显示 `relay_engine="linux-splice"`）；原生 TCP 注册 9 位设备码入 PG16 ✅；UDP 探测回显本机 IP:端口 ✅；`connect_request` 下发 `punch_coord`+`relay_token` ✅；**16 MiB 双向 `splice` 中继**：握手 RTT **7.18 ms**，吞吐 **310.91 Mbps**（431.69 ms），双端 SHA-256 逐字节一致 ✅。独立 Review 捉 4 虫（慢写者阻塞 `kick`、注册 `claimed_id` 校验序、`active_sessions` 同 `session_id` 碰撞、异步 `set_online` 竞态）全修并补 6 回归测试后重部署；Linux & macOS 均 **21 passed / 0 failed / clippy `-D warnings` 0 警告 / fmt 干净** | `linux-splice` 310.9 Mbps ✅ PG+可选Redis ✅ Review 闭环 ✅ |
| **Rust 通信服务 31 项对抗加固清零轮（2026-11-22，3 路对抗评审 31 项 + 复核尾项 4 项全闭环 + `172.16.1.2` 重测）** | **Relay/Splice 8 项**：`accept` 退避+并发信号量、Peer 1 等待期 `readable+peek` 断连监测、RAII `ActiveSessionGuard`/`PendingGuard`、`take_relay_token` 超时、硬 I/O 错触发双向 `cancel`、`AtomicUsize` CAS 计数、显式 `TcpKeepalive(30s/10s/3)` + Linux 放开 TCP 自动调优恢复 `TCP_NOTSENT_LOWAT`、单调全局字节统计。**Signal/Ephemeral 11 项**：UDP `RCSP` 在线+同源 IP 校验与限速、字段严格上限+编码错不拆受害者、token 频控+全局上限+`issued_relay_tokens` 仅回落路径使用防泄漏、增量读帧防 64KiB 预分配、64 分片 per-device 锁、IPv4-mapped 规整与异步写防队头阻塞、Redis 2s 超时 + `rc:online_zset` O(\|online\|)、毫秒 `PX` TTL、`Weak` 清理任务、`online_ttl=150s`。**Store/API/Main/Docs 12 项**：搜索移除 `device_key`+`ILIKE` 转义、可选 `--admin-token` Bearer 常数比较鉴权与显式 CORS、全组件协调停机排空落库、字段长度与 `\0` 校验+8KiB body 上限、原子 SQL `UPDATE`、前置 `status` 枚举校验、`0002_relay_indexes.sql`、自定义 JSON 报错、`--signal :0` 同端口绑定、分页与跳过离线 UDP 查询、`MAX_OS_LEN=128` 统一及文档全对齐。**复测（Mac → `172.16.1.2` Linux `splice(2)` 16 MiB 双向×3）**：吞吐由 310.91 → 中位 **344.05 Mbps**（341.79 / 344.05 / **367.88 Mbps**，SHA-256 逐字节一致），握手 RTT 由 7.18 → **5.88 ms**；macOS & Linux 双平台 **54 passed / 0 failed / clippy `-D warnings` 0 警告 / fmt 干净** | 31+4 项全闭环 ✅ `splice` 344 Mbps ✅ 54/54 双平台绿 ✅ |
| **桌面端 `rc-ui` 服务器设置与 9 位设备码中继集成（2026-11-22，Swarm 3 路 + 实测 `172.16.1.2` 全链）** | **新增模块**：`server_config.rs`（配置/设备密钥/9 位码格式与原子 `0600` 持久化）、`relay_transport.rs`（`TcpRelayTransport` 实现 `rc_net::Transport`，`RCR1` 握手 + `[ch][u32 BE len][payload]`）、`signal_client.rs`（原生 TCP `[u32 BE len][JSON]` 信令 + UDP `RCSP` 探测 + 断线指数退避 + 服务端回环地址自动回退）；**桥接与前端**：`host_bridge.rs`/`session_bridge.rs`/`state.rs`/`commands.rs`/`main.rs`/`permissions/default.toml`（3 个新 IPC + `server://status` 事件 + 入站中继自动交由 `HostHandle` 审批）、`settings.html`/`settings.css`（激活「网络」tab）、`bridge.js`/`app.js`（P2 自动展示 9 位码 + 9 位码/`IP:端口` 双模连接）。**实测 1（离线进程内回环）**：`cargo test -p rc-ui --test relay_session` 1/1 通过（密码+审批+实时流）。**实测 2（真机连 `172.16.1.2:{21116,21117}` Linux `splice(2)`）**：`cargo test -p rc-ui --test relay_session live_172_16_1_2_nine_digit_code_full_session -- --ignored --nocapture` **1.60s 通过**（控制端 `801235264` → 被控端 `684466271` 经 `172.16.1.2:21117`，密码 `LIVE2026` + 审批通过，实时 HUD **59.8 fps / 2644.7 kbps / 320×180**，优雅断开后 `172.16.1.2` `active_relay_sessions=0` / `online_count=0`）。**全仓门禁**：`cargo test --workspace` **707 passed / 0 failed / 21 ignored**（`rc-ui` 67 passed + 4 ignored），`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` / `node --check` 干净，`unsafe` 仅限 `metal_attach.rs` | 离线 + `172.16.1.2` 真机全通 ✅ 707/707 绿 ✅ |
| **桌面端 `rc-ui` 服务器设置 28 项对抗评审清零轮（2026-11-22，3 路评审 1C/11H/11M/5L → Swarm 修复 + 独立只读复核 28/28 通过）** | **网络与传输（13 项）**：`strip_optional_scheme` UTF-8 边界防 panic、非阻塞可中断建连与多地址公平超时分片、严格 `session_id` 与 `role=="host"` 路由、`Kicked` 退避治理与终态停重连、独立 `shutdown_stream` 防锁内阻塞与写错即断流、75s 活性超时 + `SO_KEEPALIVE` + 单次解析 UDP 探测、IPv4-mapped IPv6/端口 `0`/裸主机规范化、`device_key`/`hostname`/`lan_addrs` 清洗、单调 `ServerStatusSnapshot.revision`、`probe_server_connectivity` 保留解析端口；`TcpRelayTransport` 独立 `rc-relay-writer` 优先级队列（`urgent > reliable > datagram`）、5s 写超时、16 MiB `RxQueue` 字节上限、64 KiB 增量读、锁中毒免疫 `Drop`、握手 RTT 采样入 `NetStats` 与 `HudSnapshot::derive_with_transport`（`"TCP Relay"`）。**IPC 与生命周期（8 项）**：`open_session` 仅在 `request_connect` 成功后开 S1 窗 + `session_open_epoch` 防僵尸/乱序、`async save_server_config` + `ServerRuntime`/`server_io_lock` 防主线程冻结与跨服污染、双端 `ActiveRelayInterrupt` 可秒断中继建连/握手、被控关闭并发受控快拒入站中继、20s TTL 丢最旧中继队 + 重建空闲 LAN 会话、工作线程内原子临时码轮换、`Stopped` 前置 `running=false`。**前端与测试（7 项）**：移除 `openSessionWindow` 前置调用、关被控隐 9 位码、归一化设备码与禁 `0` 开头、订阅先于拉取 + `revision` 守卫、`type="text" inputmode="numeric"` 端口框、可换行详情区 + `"测试中…"` + 按钮文案与 IME `isComposing` 守卫；`tests/relay_session.rs` 扩至 7 离线 + 1 可复用密钥真机。**复测**：离线 `relay_session` **7/7 通过**（1.49s）；真机连 `172.16.1.2` **1.65s 通过**（`267683716→950411046`、`transport="TCP Relay"`、`59.5 fps`、`active_relay_sessions=0`、`online_count=0`）；全仓 `cargo test --workspace` **728 passed / 0 failed / 21 ignored**（`rc-ui` 88 passed + 4 ignored），`clippy -D warnings` 0 警告，`fmt` / `node --check` 干净 | 28/28 复核清零 ✅ `172.16.1.2` 全通 ✅ 728/728 绿 ✅ |
| **全功能补齐轮：P2P UDP 打洞 + 剪贴板 UI + 音频 (`rc-audio`) + W2/W3/W4 工具 (`rc-tools`)（2026-11-23，Swarm 3 波 + 独立复核 9/9 清零）** | **新增/扩展 crate**：`rc-net`（`QuicEndpoint::from_socket`/`connect_cancelable`/`accept_cancelable`，42 passed）、`rc-session`（`ToolChannel 0x10` + `AudioFrame 0x11` 双向有界队列，233 passed）、新建 **`crates/rc-audio/`**（纯 Rust IMA-ADPCM 4:1 自包含编解码 + macOS 独立 SCK 音频流 + `AudioToolbox` `AudioQueue` 4×20ms 播放，14 passed）、新建 **`crates/rc-tools/`**（W2 双向文件传输 64KiB 分块/`O_EXCL` `.rcpart`、W3 真实 Unix PTY `O_NONBLOCK` 远程终端、W4 非阻塞 TCP 端口转发，23 passed）。**`rc-ui` 端到端接线**：`p2p.rs`（29B `RCP2` 双向打洞优先 + 无缝回退 TCP 中继）、剪贴板与音频双端接线（S2 静音 / S3 剪贴板开关与工具菜单）、18 项 W2/W3/W4 Tauri IPC + 3 项事件（`file://event`/`terminal://event`/`portmap://update`）+ 前端 `transfer.js`/`terminal.js`/`portmap.js`/`session.js` 真接线（保留静态预览 0 报错、0 裸色值）。**真机实测（连 `172.16.1.2:21116`）**：`live_172_16_1_2_nine_digit_code_full_session -- --ignored` **1.63s 通过**——`172.16.1.2` 协调 `punch_coord` 后双端直接走通 **`UDP P2P` 直连**（`backend="fake (P2P 直连)"`、`transport="UDP P2P"`、`59.7 fps`；服务端 `punch_coord_count` 4→6 而 `total_relay_sessions` 维持 3 零中继流量、`online_count=0`、`active_relay_sessions=0`）。**离线 E2E**：直连 P2P、P2P 被封→TCP 中继自动回退、活会话内 W2 上传下载 + W3 真实 shell + W4 TCP 隧道回环全过；全仓 `cargo test --workspace` **792 passed / 0 failed / 21 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` & `node --check` 干净 | 6 大功能全通 ✅ 真机 UDP P2P 59.7fps ✅ 792/792 绿 ✅ |
| **全功能 35 项深度对抗评审清零轮（2026-11-23，3 路评审 3C/14H/13M/5L → Swarm 2 波 5 worker + 独立只读复核 35/35 VERIFIED）** | **CRITICAL 3/3**：`portmap.rs` 新增 `ConnHalfClose` + 写线程排空尾包再 `Shutdown::Write`、双端 `MAX_OUTGOING_FRAMES=128` 恢复真实 TCP 背压、`terminal.js` 禁从 `output` 猜 `activeTermId` + `connectSeq` + `exited`/`ended` 清 `opening`。**HIGH 14/14**：`quic.rs` Stream A 独立有界 `sync_channel(256)` + `MAX_RELIABLE_PER_POLL=64`、`rc-session` 隔离 `audio_ctrl_rx` + 满队显式报错与断连清空；`p2p.rs` 全局 `LocalUdpPortLease` + `QuicEndpoint::drop` 立即关流 + 双栈 `AF_INET6` 保 IPv6 + `peek_from` 保 QUIC Initial + 双向 Ack 确认 + `accept_from_cancelable` + `host_bridge` P2P 握手失联回退 TCP 中继 + `local_udp_port==0` 禁发 `:9901`；`rc-audio` `AudioQueueReset` 防静默卡死、Stacked Borrows 安全偏移、`startCapture` 超时停流、采样率 `8k..=192k` 与帧对齐、真实设备采样率透传；`rc-tools` `pty_unix` 低号 fd 重定位 + `OPENPTY_LOCK` + `tcgetpgrp` 优雅停机 + 原子 `write_input`、`terminal` 跨块 UTF-8 续解码 + `outgoing` 有界、`file_transfer` 客户端锁定 `expected_file_name` 防任意文件覆写 + 拒重复 `DownloadMeta` + 短 `.rcpart` + `O_NONBLOCK`/`EINTR` + 并发上限、`portmap` 原子 `live_workers` 上限 + 单总截止时间 + `ConnWindow` 2MiB 窗口流控 + 非阻塞 reaper + 过期事件/重复端口清理；`rc-ui` 文件盘 I/O 移入独立 `rc-file-{host,client}-worker` 线程、异步 SCK 音频初始化（仅真实硬采启用）、`open_session` 切换清旧终端/映射 + `should_emit_session_updates(..., saw_ended)`、`AudioPlayer` 变格式重建 + 重连静音同步、全量 IPC 校验与单出口 `portmap://update`、`terminal.js` 增量脏行 DOM/真光标/CSI `H`/`Cmd+A`、`transfer.js`/`portmap.js`/`session.js` 会话联动与状态重置。**复测**：`relay_session` 离线 **12/12 通过**（含脑裂 P2P→Relay 回退与工具校验）；真机连 `172.16.1.2` **1.59s 通过**（`UDP P2P 直连`、`60.7 fps`、`online_count=0`、`active_relay_sessions=0`、`total_relay_sessions` 维持 3）；全仓 `cargo test --workspace` **829 passed / 0 failed / 21 ignored**、`clippy --workspace --all-targets -- -D warnings` 0 警告、`cargo fmt --all -- --check` & `node --check` 干净、`rc-ui` 仍仅 `metal_attach.rs` 含 `unsafe` | 35/35 复核清零 ✅ 真机 P2P 60.7fps ✅ 829/829 绿 ✅ |
| **macOS 图标生成与桌面应用打包实测（2026-11-23，`./scripts/package-macos.sh` release arm64）** | **图标资产**：`crates/rc-ui/icons/{icon.png (1024×1024, 667K), 32x32.png, 128x128.png, 128x128@2x.png, icon.icns (1.2M, 10 规格), icon.ico (84K, 6 规格)}`；**打包配置**：`crates/rc-ui/tauri.conf.json` 开启 `bundle.active=true`、`crates/rc-ui/Info.plist` 补全屏幕录制/音频/本地网络 TCC 描述；**实测产物**：`target/dist/RabbitControl.app`（**11 MB**，Mach-O arm64，`plutil -lint OK`，`codesign --verify --deep --strict` 通过，`designated => identifier "com.rabbitcontrol.dev" and certificate leaf = H"51f7737f..."` 与 §八固定签名完全一致）+ `target/dist/RabbitControl-0.1.0-arm64.dmg`（**6.4 MB**，含 `/Applications` 软链，`hdiutil verify` VALID） | `.app` + `.dmg` 签名验签全过 ✅ |
| **macOS 真机桌面体验 6 项修复轮 + 黑屏二次取证根治（2026-11-23，Swarm 3 worker + 真机截图取证修复 + release 重打包实测）** | ① **拖拽区全宽修复**：主窗 `index.html`/`app.js::renderTitlebar` 与 `session.html`/`settings.html`/`transfer.html`/`terminal.html`/`portmap.html` 全部非交互标题栏节点补齐 `data-tauri-drag-region`；② **设备 ID 去 IP 化**：`app.js::renderP2DeviceId` 仅显示 9 位设备码或「未连接服务器」，副行 `#p2DeviceSub` 独立提供「去配置」与局域网地址提示；③ **默认关闭允许协助**：移除 `main.rs` 启动期 `start_host_service`、前端默认 `allowAssist=false`；④ **手动开启权限预检**：新增 `crates/rc-ui/src/permissions.rs`（复用 `rc-capture-macos::PermissionStatus`，0 新 unsafe）+ 3 项 IPC（`check_permissions`/`request_screen_recording_permission`/`open_permission_settings`，写入 `permissions/default.toml`）+ `set_host_enabled(true)` 后端门禁 + 前端权限状态/引导弹窗；⑤ **密码通过免二次审批**：`host_bridge.rs` 新增 `UiHostConfig::auto_accept_with_password`，`password_verified==true` 时 `UiApprover` 立即放行零弹窗，无密码仍弹机主审批；⑥ **连接黑屏真机截图取证根治**：真机 35s 实时流（HUD `UDP LAN / 5 fps / 1.1 Mbps / 22.3 ms frm / 1920x1080`）截图逐像素采样实证全屏为平坦 `sRGB #1F2229`（`NSWindow.backgroundColor`），证实 `WKWebView` 已透明而 `zPosition=-1.0` + `insertSublayer_atIndex(0)` 把 `CAMetalLayer` 压到窗口背景之下且 `.sn-stage__hint { display: flex }` 缺 `[hidden] { display: none }`；并在 `commands.rs::open_session` 主线程预创建 `MainThreadMetalLayer` 经 `SessionHandle::spawn_with_metal_layer` 交由后台 `MetalRenderer::from_raw_layer` 领养（彻底解决后台线程 `[CAMetalLayer layer]` `presentDrawable` 永不上屏导致黑屏的终极根因；经真机 `SCK→VT→QUIC→VT→Metal→Tauri` 全链回环 `screencapture` 实测 `1280×800 / 36 fps / 10.9 ms` 成功渲染桌面画面），并在 `session.css` 补 `.sn-stage__hint[hidden] { display: none; }` 与 `html { background: transparent; }`、`main.rs` 记录 `attach_layer` 错误日志。**验证**：`cargo test --workspace` **841 passed / 0 failed / 21 ignored**（`rc-ui` **131 passed / 0 failed / 4 ignored**），`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` & `node --check` 干净，`rc-ui` 仍仅 `metal_attach.rs` 含 `unsafe`；`./scripts/package-macos.sh` 重打 `target/dist/RabbitControl.app`（11 MB，`codesign --verify --deep --strict` 通过）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB，`hdiutil verify` VALID） | 6 项+黑屏全根治 ✅ 841/841 绿 ✅ 验签+DMG 全过 ✅ |

| **macOS 鼠标位置偏移全链路根治与真机实测（2026-11-23，M1 Pro 14" Liquid Retina XDR `3024×1964` / 逻辑 `1512×982`，`./scripts/macos-dev-run.sh` + release 重打包）** | **根因**：① 被控端硬编码 `caps=3840×2160`、控制端默认请求 `1920×1080` (16:9)，导致 SCK 在 `1512×982` (16:10) 屏幕上按 `preservesAspectRatio=true` 塞入左右各 ~128.5 px（~6.7%）黑边，引发 Dock 左侧 ~77 pt 横向偏移；② `InputBridge::new()` 映射全部在线显示器联合边界，而 `SckBackend(display=0)` 仅采主屏；③ 控制端 `metal_view` 直接拉伸填满 `#session-stage`，窗口缩放后画面比例与点击坐标不同步；④ `pointerdown`/`pointerup` 未前置同步当前坐标。**修复**：`rc-capture-macos` 暴露 `main_display_logical_resolution()` 并启 `setScalesToFit(true)`；`rc-session` 新增 `HostSession::set_preserve_caps_aspect`；`rc-input-macos` 新增 `Injector::for_main_display()` / `InputBridge::for_main_display()`；`rc-ui` 统一 `input_bridge::fit_video_rect` + `metal_attach::Frame::fit_aspect` + `ActiveSession.video_size` + `session.js` 点击前置同步指针与禁右键菜单；`scripts/macos-dev-run.sh test` 补 `--test`/`--lib` 选择器。**实测**：① `./scripts/macos-dev-run.sh test rc-ui --test session_flow -- --ignored real_hardware_aspect_and_pointer_accuracy --nocapture` **1 passed / 0 failed**（控制端请求 `1920×1080` → 协商 `1512×982` `videotoolbox+metal` 硬件首帧上屏；模拟 `1280×752` S1 舞台注入 3 点，实时 `CGEventTap` 测得 `dock_left_chrome` 误差 **0.46 pt**、`center` **0.68 pt**、`top_right` **0.40 pt**，全 ≤2.0 pt ✅）；② `./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture` **4/4 通过**（`ScreenBounds { 0, 0, 1512, 982 }`）；③ `cargo test --workspace` **843 passed / 0 failed / 22 ignored**（`rc-ui` 132 passed / 5 ignored）、`clippy --workspace --all-targets -- -D warnings` 0 警告、`cargo fmt --all -- --check` & `node --check` 干净；④ `./scripts/package-macos.sh --release` 重打 `target/dist/RabbitControl.app`（11 MB，`codesign --verify --deep --strict` 通过）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB，UDIF zlib `imageinfo` 校验正常） | 误差 ≤2.0 pt（实测 0.40–0.68 pt）✅ 843/843 绿 ✅ |

| **macOS 5 项体验/高刷/分辨率切换 Swarm 轮（2026-11-23，Swarm 3 worker + 真机硬件 `144fps` & `3024×1964` 热切换实测 + release 重打包）** | ① **自适应主窗口**：`tauri.conf.json` 主窗 `1440×900→1000×660`（min `820×540`），`window.rs::adapt_main_window_on_startup` 按屏幕尺寸自适应（1280×720→`820×540` 级、1512×982/1080p→`1000×660`、2K/4K→`1120×720`）+ `main.css` 响应式布局零溢出；② **S1 零黑边自动适配与一键消黑边**：`window.rs::compute_fit_session_window_size` 误差 `<=0.5 px`（14" MBP `1512×982` 初始自动贴合 `1166×805` 零左右黑条，保留 `resizable=true`），新增 `fit_session_window` IPC（入 ACL）并在控制中心「窗口设置」与选项卡右键菜单提供「一键消除黑边」；③ **屏蔽 WKWebView 右键「重新加载」**：`session.js` 全局捕获阶段无条件 `document.addEventListener('contextmenu', e => e.preventDefault(), true)` + `stage` 右键 `mousedown`/`pointerdown` `preventDefault()`；④ **突破 70fps 达 120/144fps**：`commands.rs::default_fps` 与 `host_bridge.rs` `caps.fps`/`pipeline.fps` 从 `60→144`、`pipeline.bitrate_bps→20 Mbps`、`RUN_IDLE_POLL_BUDGET`/`wait_event` 降至 `4ms`，S3 画质菜单实时切换 `144/120/90/60/30 fps`；⑤ **双入口实时分辨率热切换**：`rc-capture-macos::main_display_backing_resolution` + `rc-session::TOOL_CHANNEL_DISPLAY_CTRL=5` & `HostSession::reconfigure_stream`（S6 停机重拉 + `frame_seq_offset` 单调延续防丢帧）+ `display_ctrl.rs` 等比预设 + `host_bridge.rs` >2.6M 像素自动收敛 60fps 防 VT RealTime 过载与活跃硬件禁降级 FakeEncoder + `session.html`/`session.js` 选项卡右键菜单 & 控制中心「窗口设置」双入口同步。**实测**：① `./scripts/macos-dev-run.sh test rc-ui --test session_flow -- --ignored real_hardware_aspect_and_pointer_accuracy --nocapture` **1 passed / 0 failed**（`1512×982@144fps`、`presets=6`、`videotoolbox+metal`、光标误差 **0.46 / 0.68 / 0.40 pt**、实机热切 Retina `3024×1964` 零 VT 过载零降级）；② `cargo test --workspace -- --test-threads=2` **861 passed / 0 failed / 22 ignored**（`rc-ui` **148 passed / 0 failed / 5 ignored**，`window::tests` 17/17、`display_ctrl::tests` 5/5、`live_display_reconfigure_updates_resolution_fps_and_keeps_streaming` 离线全通）；③ `clippy --workspace --all-targets -- -D warnings` 0 警告、`cargo fmt --all -- --check` & `node --check` 干净；④ `./scripts/package-macos.sh --release` 重打 `target/dist/RabbitControl.app`（11 MB，`codesign --verify --deep --strict` 通过）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB，`hdiutil imageinfo` 正常）；**11 项体验/120fps/持久化 Swarm 交付与硬件根治轮（2026-11-23，Swarm 4 worker 并行 + 统筹集成 + M1 Pro 真机实测 + 握手批排空修复）**：① **突破 70–98fps 逼近 120fps**：`VtEncoder` 2-deep 异步流水线（`MAX_PIPELINED_IN_FLIGHT=2`）+ `HostPipeline` 2-deep 调度与 `has_pending_work` + `SCK_SURFACE_POOL_DEPTH=8` + `host_bridge` 忙碌零休眠 tick + `metal_attach` macOS 14+ 120Hz ProMotion `CADisplayLink`（4s `1512×982@144` 真机环回实测 `displayed` 由 **98.0→112.5 fps**、`encode` **14.06→11.27 ms**、`dropped_budget` **59→19**、`unfinished=0`）；② **安全中转开关真接线**：`UiSessionConfig::force_relay` + `set_session_relay_mode` IPC + `Negotiated.transport` + `#opt-relay` 状态同步；③ **网络 HUD 开关移至控制中心「更多」**：去右下角铅笔按钮，改控制中心→「更多」→`✓ 显示网络状态`（`AppPrefs.show_hud` 持久化）；④ **真机物理分辨率切换与自动恢复**：`rc-capture-macos` 实接 `CGDisplayCopyAllDisplayModes` / `CGDisplaySetDisplayMode`（`DisplayModeGuard` `kCGConfigureForSession` + `Drop` 自动复原）+ `Injector::set_screen_bounds` + `main.rs` 宽高比变化自适应去黑边；⑤ **允许远程协助持久化**：`prefs.json` 保存 `allow_remote_assist`，仅在开启且 TCC 权限齐全时启动自动监听；⑥ **桌面壁纸与已连接快照**：Host 异步抓主屏壁纸缩略图经 `DisplayCtrl::Wallpaper` 下发至控制端落盘 `~/.config/rabbitcontrol/wallpapers/`，P1 展示真实壁纸与「最近已连接」；⑦ **Dock 右键设备与屏幕子菜单**：`metal_attach::install_dock_menu_handler` + `update_dock_menu_devices` + `dock://connect-device`（点即连接或聚焦）；⑧ **移除 `172.16.1.2` placeholder**（`settings.html`）；⑨ **设备 ID 支持刷新更换**：`refresh_device_id` 轮换 `device_key` 清空旧缓存 ID 重注册；⑩ **主窗显示当前远控状态**：`ActiveSessionMeta`/`session://active-state` 驱动侧栏 `[📺 1]` 徽章、P1 蓝横幅/磨砂遮罩「显示远控窗口」与 P2 远控状态条；⑪ **画质/帧率/分辨率/HUD 持久化**：`app_prefs.rs` + `get_app_prefs`/`save_stream_prefs` + `ClientSession::handshake`/`tick_reconnect` 同批 `Welcome` 后 `ToolChannel` 零丢弃修复；workspace **883 passed / 0 failed / 23 ignored**、`clippy -D warnings` 0 警告、`fmt` & `node --check` 干净，已重打并验签 `target/dist/RabbitControl.app`（11 MB）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.5 MB） | 5 项全落地 ✅ 真机 144fps+热切通过 ✅ 861/861 绿 ✅ |
| **8 项修复轮 — 30s 120fps 稳态对照与 LLRC 门控根治（2026-11-23，M1 Pro `1512×982` / 前景 120Hz 纯 `NSBezierPath` 动画窗，统一经 `./scripts/macos-dev-run.sh` 运行）** | **① Loopback 优化前基线**（`./scripts/macos-dev-run.sh cli --release -- loopback --backend real --resolution 1512x982 --fps 120 --duration 30s`）：`captured=3526 (117.53 fps)`、`encoded=2489`、**`displayed=82.97 fps`**（Gate ≥108 ❌）、`encode=19.63 ms`、`p50=21.84 ms`、`p95=28.98 ms`、`p99=34.03 ms`、`dropped_budget=1037`、`convergence=1010 ms`。<br>**② Loopback App Nap / QoS 后未禁省电**（`NSProcessInfo LatencyCriticalActivity` + capture/host/session/cli `QOS_CLASS_USER_INTERACTIVE`）：`captured=3592 (119.73 fps)`、**`displayed=81.93 fps`**（❌）、`encode=19.10 ms`、`p50=21.67 ms`、`dropped_budget=1134`。<br>**③ 测量 A（+ `MaximizePowerEfficiency=false` + `rc-host-encode` 线程 QoS）**：`captured=3582 (119.40 fps)`、**`displayed=84.37 fps`**（❌）、`encode=19.30 ms`、`network=3.96 ms`、`p50=24.05 ms`、`dropped_budget=1051`。<br>**④ 测量 B（临时 `ExpectedFrameRate=240` 提示，已回退）**：`captured=3591 (119.70 fps)`、**`displayed=88.47 fps`**（❌）、`encode=17.64 ms`、`network=6.64 ms`、`p50=25.75 ms`、`dropped_budget=934`、`dropped_stale=3`。<br>**⑤ 受控诊断矩阵 D1–D4（临时改项均已还原并跑通 `cargo test -p rc-encode-vt`）**：**D1**（4s `1512×982@120` 当前树）`displayed=108.25 fps`（`encode=11.02 ms` / `net=15.70 ms` / `p50=36.45 ms` / `dropped_budget=43`）；**D2**（30s `1280×832@120` 降像素测试）`displayed=83.53 fps`（`encode=16.30 ms` / `p50=25.21 ms` / `dropped_budget=1084`，排除纯像素带宽瓶颈）；**D3**（临时 `EnableLowLatencyRateControl=false` + 保持 `RealTime=true`）**`displayed=118.03 fps`**（`encode=12.78 ms` / `net=17.67 ms` / `p50=36.20 ms` / `dropped_budget=55`）；**D4**（临时 `RealTime=false` + 保持 LLRC=`true`）`displayed=85.03 fps`（`encode=19.14 ms` / `p50=23.90 ms` / `dropped_budget=1040`）。<br>**⑥ 最终 Loopback（落地 `use_low_latency_rate_control = fps < 90` 门控 + 保留 `RealTime=true` M6，30s `1512×982@120`）**：`captured=3597 (119.90 fps)`、`encoded=3546`、**`displayed=118.20 fps`**（稳态门槛 ≥105 ✅，Gate 吞吐 ≥108 ✅；单机双端高吞吐下 `encode=12.76 ms` / `network=18.03 ms` / `decode=2.49 ms` / `present=5.83 ms` / `p50=36.32 ms` / `p95=42.96 ms` / `p99=48.83 ms` / `dropped_budget=51` / `unfinished=0`，`p50 > 18ms` & `conv 1080ms > 1000ms`）。<br>**⑦ 真机 25s `HostHandle+SessionHandle` 稳态会话测试（`./scripts/macos-dev-run.sh test rc-ui --test session_flow -- --ignored real_hardware_sustained_120fps --nocapture`，门槛：后 15s 中位 ≥100 / 最低 ≥85）**：**修复前** 00:00:01–03 为 113.6/120.0/113.6 fps → 00:00:04 起跌至 ~79 fps，后 15s 中位 **`78.9 fps`** / 最低 **`72.9 fps`**（❌）；**修复后** 24 个 1s 样本全程 **113.5–120.5 fps**，后 15s 中位 **`119.7 fps`** / 最低 **`117.0 fps`**（✅）。<br>**⑧ 回归与全仓门禁**：T12 60fps `./scripts/macos-dev-run.sh test rc-encode-vt -- --ignored --nocapture --test-threads=1` **4/4 ✅**（300 帧 P50 **`7.72 ms`** / P95 `9.27 ms` / min `6.01 ms` / max `12.54 ms` / IDR=2 零偏移）；`real_hardware_aspect_and_pointer_accuracy` ✅（`0.46 / 0.68 / 0.40 pt`，切 `3024×1964@120` 恢复 `1512×982`）；`rc-input-macos` 真机 **4/4 ✅**；`live_172_16_1_2_nine_digit_code_full_session` **1.47s / UDP P2P / 59.9 fps ✅**；全仓 `cargo test --workspace -- --test-threads=1` **888 passed / 0 failed / 24 ignored**、`clippy -D warnings` 0 警告、`fmt` / `node --check` 与 4 项静态审计干净，重打并验签 `target/dist/RabbitControl.app`（**11M**）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（**6.5M**） | 30s 118.2fps / 会话 119.7fps ✅ T12 4/4 ✅ 888/888 绿 ✅ |
| **3 项追加修复轮 — 退出锁屏与持久化 / 验证码遮罩 / 次级分辨率菜单悬停（2026-11-23）** | ① **原生锁屏符号探针**（`cargo test -p rc-capture-macos -- --test-threads=2`）：`lock_screen_symbol_is_resolvable_without_invoking` ✅（验证 `login.framework` 导出 `SACLockScreenImmediate`，不触发真实锁屏；全 crate 16 passed / 2 ignored / 1 doctest）。<br>② **偏好与显示协议单测**：`rc-ui --lib app_prefs` 4/4 ✅（验证 `lock_on_exit` 持久化/增量补丁/旧 JSON 默认 `false`）、`rc-ui --lib display_ctrl` & `session_bridge` 7/7 ✅（含 `DisplayControlMsg::LockScreen` 编解码与转发）。<br>③ **端到端断前与在线锁屏闭环**（`cargo test -p rc-ui --test session_flow lock_remote_screen_live_and_on_disconnect_reaches_host`）：假后端 `HostHandle+SessionHandle` 实测 `LockRemoteScreen` 与 `request_lock_on_disconnect + Disconnect` 均送达主机（`lock_screen_count` `0 → 1 → 2`）✅。<br>④ **全仓门禁与发行打包**：`AppState::new_for_test()` 隔离后 `cargo test --workspace -- --test-threads=1` **891 passed / 0 failed / 24 ignored**、`cargo clippy --workspace --all-targets -- -D warnings` 0 警告、`cargo fmt --all -- --check` & `node --check` 与 4 项静态审计全干净；`./scripts/package-macos.sh --release` 重打并 `codesign --verify --deep --strict` 验签 `target/dist/RabbitControl.app`（**11M**）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（**6.5M**） ✅ | 891/891 绿 ✅ 验签 `.app`+`.dmg` 全过 ✅ |

| **2026-11-23（Phase 1：账号体系 + Web 后台 + 双门控 + 隐藏式局域网速通 + 免密自控 + 全链强制 mTLS）** | ① **Rust workspace 门禁**（`cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`）：**932 passed / 0 failed / 24 ignored**（78 test suites，含 `rc-ui` 110/110 lib + `rc-cli` `host_client` 12/12 + `rc-session` + `rc-net`）。<br>② **服务端与管理后台**（`cd server/backend && cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo test` + `cd server/web && npx tsc --noEmit && npm run build`）：**`server/backend` 71 passed / 0 failed**（60 lib + 5 `auth_pki_e2e` + 6 `end_to_end`）；`server/web` 0 errors（1643 modules，`dist/assets/index-B8sAcbGG.js 326.40 kB`）；`crates/rc-ui/ui/assets/js/*.js` 10/10 `node --check` 通过。<br>③ **`rc-net::mtls_loopback` 延迟对照与安全验证**（`cargo test -p rc-net --test mtls_loopback -- --nocapture`）：**5 passed / 0 failed**；loopback handshake（10 rounds）non-mTLS `avg=0.67ms p50=0.65ms` vs mTLS `avg=0.62ms p50=0.63ms`（`delta avg=-0.05ms`）；steady-state datagram RTT（200 frames）non-mTLS `p50=133us p99=199us` vs mTLS `p50=69us p99=161us`（`delta p50=-0.064ms`）；steady-state control RTT（200 msgs）non-mTLS `p50=62us p99=99us` vs mTLS `p50=65us p99=140us`（`delta p50=+0.003ms`）。<br>④ **`rc-ui::mtls_bench` 端到端稳态延迟评估**（`cargo test -p rc-ui --test mtls_bench -- --nocapture`，`64x64 @ 120fps mock`，热身后 200 steady 帧）：Case 1 `[UDP LAN/P2P]` non-mTLS `p50=8.441ms p99=14.979ms` vs QUIC mTLS `p50=8.876ms p99=17.962ms`（**`delta_p50=+0.435ms`**）；Case 2 `[TCP Relay]` outer mTLS only `p50=6.819ms p99=10.268ms` vs outer+E2EE mTLS `p50=6.398ms p99=9.747ms`（**`delta_p50=-0.421ms`**）。两项增量均 **≤ 3.0 ms** 门槛 -> 锁定 **`DEFAULT_ENCRYPT_CONNECTION = true`**。<br>⑤ **`rc-ui::owner_mtls_routing` 策略与三段路由验证**（`cargo test -p rc-ui --test owner_mtls_routing -- --nocapture`）：**4 passed / 0 failed**（`encrypt_connection` 缺证/错证拒绝降级明文、隐藏式 `LanRouteCache` 优先 `UDP LAN mTLS`、动态切换 mTLS 重连保持参数、`OwnerTicket` PoP 绕过验证码与关闭的 `allow_remote_assist`）。 | 全线闭环 ✅ 932+71 全绿 ✅ mTLS 增量 ≤3ms（默认 ON）✅ |
| **2026-11-23（Phase 1 多轮对抗复审清零 + 客户端 W2/W3/W4 全链路接线与 5 轮 Review 清零）** | ① **Rust workspace 串行全仓终验**（`cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace -- --test-threads=1`）：**939 passed / 0 failed / 24 ignored**（78 test suites；其中 `rc-tools` 24 unit + 11 file_transfer + 5 portmap + 2 terminal 全绿，`rc-ui` 113 unit + 50 window + 1 mtls_bench + 5 owner_mtls_routing + 12 relay_session [1 ignored live 172.16.1.2] + 7 session_flow [5 ignored GUI] + 4 unified_flow 全绿）。<br>② **服务端与管理后台终验**（`cd server/backend && cargo fmt -- --check && cargo clippy --all-targets -- -D warnings && cargo test` + `cd server/web && npx tsc --noEmit && npm run build`）：**`server/backend` 73 passed / 0 failed**（61 unit + 6 `auth_pki_e2e` + 6 `end_to_end`，0 skipped）；`server/web` **0 errors**（1643 modules，`dist/assets/index-C7L033rN.js 327.15 kB`）。<br>③ **前端静态与跨平台 CI 校验**（`crates/rc-ui/ui/assets/js/*.js`）：`node --check` **10/10 通过**，`! rg -n "\.innerHTML\b" crates/rc-ui/ui/assets/js/terminal.js` **0 命中**，Phase 1 Server+Web（6 轮）、Client+Crypto（7 轮）、W2/W3/W4 客户端接线（5 轮）及发版前跨平台/Web 终审（Client 6 轮、Server+Web 5 轮）均返回 **`ZERO_ISSUES_FOUND`**；`.github/workflows/ci.yml` 覆盖 `fmt-and-ui-js`、`server-backend`（Linux）、`server-web` 及 `client-workspace`（`macos-latest`/`ubuntu-latest`/`windows-latest` 三平台矩阵）。 | 939+73 全绿 ✅ 全线 `ZERO_ISSUES_FOUND` ✅ CI 就绪 ✅ |

显著变差时先查 `Cargo.toml` 的 profile 与 `.cargo/config.toml` 是否被改。

---

## 13. 本阶段最重要的未知数（已实测）

**VideoToolbox 在 `RealTime=true` 下的编码延迟，B1 完成后已实测**（`rc-encode-vt` 的
`videotoolbox_realtime_flag_is_set` 真机测试，300 帧 1080p，输入→输出）：

| 运行 | P50 | P95 |
|---|---|---|
| debug 构建 ×2（agent 实跑） | 9.73ms / 9.20ms | 32.8 / 33.7ms |
| release 构建（主 agent 复跑） | **9.45ms** | 33.0ms |

对照原决策表：

| 原判定线 | 实测落在 |
|---|---|
| P50 ≤5ms：预算成立 | — |
| P50 5–8ms：重分配预算 | — |
| **P50 >8ms：必须调整目标** | **✅ 落在这档（9.45ms）** |

**结论与待决事项：**
- T12 卡自身的验收线（P50 ≤12ms，证明 `RealTime` 生效）**通过**——未设该标志时是 +20–40ms，实测 9–10ms 说明标志确实生效。
- 但相对 §3.1 预算表（编码段 5ms），**实测超支约 4.5ms**。debug/release 结果一致，说明瓶颈是 VT 会话回调节奏而非 CPU 速度；min = 5.73ms 说明地板低于 8ms，P50 的优化候选（回调线程 QoS、`kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder` 显式化）**已在下方调优轮实测排除**。
- **进入 B2/B3 之前需要决策（VT 调优已执行两轮，未稳定达标）**：调优后真机实测
  （60fps 节奏提交，release，300 帧 1080p）**P50 = 7.62 / 7.84 / 8.01ms（中位 7.84ms，
  已触 8ms 线内但无稳定裕度，按未达标处理），P95 = 32.46ms**。归属必须分清：9.45 → 7.84
  的降幅**全部来自测量侧**——①测试提交节奏对齐真实 60fps 工况（与帧时间戳 /
  ExpectedFrameRate 一致，替代背靠背 ~107fps 突发提交）；②cfg(test) 门控的延迟分解探针
  把测量定位到 VT 内部。**编码器路径零净改动、编码器没有变快**：本轮唯一功能性尝试
  （逐帧 CompleteFrames）实测无改善且违反 E5「超时不阻塞」，已回滚；上一轮保留的两项
  （显式 RequireHardware、提交线程 QoS RAII）均无可测影响。3 子轮结论摘要：A 诊断——延迟
  全在 VT 内部（submit→callback P50 = 8.20 / min = 4.61，唤醒 + 提取 < 0.1ms，投递节拍
  疑似 ~8.3ms/120Hz 量化）；B CompleteFrames 强制同步完成——否决回滚；C 60fps 节奏提交
  ——主簇收紧到 6–8ms。
- **决策记录（2026-11-20 深夜）**：b/c 问题经 AskUserQuestion 与两次里程碑报告共三次提交
  均未获答复，用户同期三次重申「开工」。**按保守默认 b 推进 B3**：b 不降低任何产品目标
  （1080p60、18ms 回环门槛原样保留），B2 实测后各段合计 ≈15.2ms（采集 4 预算 + diff 1 预算 +
  编码 8 实测 + 打包 1 预算 + 解码 0.93 实测 + 呈现 0.23 实测），有 ~2.8ms 余量；
  c（降档）保留随时可切换。若 B3 实测端到端 P50 > 18ms，b/c 问题以更强的数据重新提出。
- **用户裁决（2026-11-21，原话）**：「明确一点 我非常在乎性能 不在乎实现难度 哪怕整个项目
  重写都在所不惜」。由此定案：**c（降档）永久出局**——降档与性能优先直接矛盾；**b 由保守默认
  升级为用户确认**，且 18ms / 1080p60 / 58fps 是**下限而非目标**，能压多低压多低。此后一切
  工程取舍只有一个判据：**实测延迟是否下降**；实现难度、工作量、要不要推倒重写，均不构成
  否决理由。执行纪律不变：先用 M1 Gate 实测定位最差段，数据先行——「重写在所不惜」是授权，
  不是指令人工盲拆。
  原三选项（存档）：
  a) 继续调优——~~**C API 已无手段**（≥20ms 停顿簇 119/300 帧为 VT 内部确定性行为、
     投递节拍无公开旋钮）~~（**2026-11-21 更正**：后经 `EnableLowLatencyRateControl` 证伪，P95 停顿簇已消除，见下），仅剩重构等待模型（流水线化，违反 E3/E4 单帧在途），不建议；
  b) 接受 7.84ms 并重分配 B3 的 18ms 回环门槛：**编码段按 8ms 计，其他段合计 ≤10ms**
     （P95 尾部由丢帧恢复机制吸收，不进回环门槛）——**已按此推进**；
  c) 降档（1440p 或 45fps）——保留。

**VT 调优轮（2026-11-20，agent 实跑，release 真机，同机 M1 Pro）**——选项 a) 已执行，
三项候选逐项实测，P50 均未移动，8ms 未达成：

| 轮次 | 改动 | P50 / P95 / min（ms） | 结论 |
|---|---|---|---|
| 1 | 无（测量路径审计） | 9.26 / 33.10 / 5.77 | 等待路径是 condvar 直达信号（回调 `notify_all` 唤醒等待线程，`rc-encode-vt/src/ffi/mod.rs`），**不是轮询邮箱、无量化伪影**——min 与 P50 的离散是真实行为 |
| 2 | 显式 `RequireHardwareAcceleratedVideoEncoder = true` 编码器规格 | 9.58 / 33.27 / 5.76 | 无延迟变化（会话本就在硬编上）；改动保留——落软编从静默降级变成构造期失败 |
| 3 | 提交线程 QoS 抬到 USER_INTERACTIVE（RAII 恢复原值，`encode` 内覆盖提交→等待→提取） | 9.23 / 33.19 / 5.72 | 无延迟变化——空载机器上提交/唤醒调度不是瓶颈 |
| — | 终态复测 | 9.53 / 33.50 / 5.77 | 终态数字稳定在 9.2–9.6ms 区间 |

profile 与码控核对无需改动：构造期已落死 Baseline（CAVLC、无 B 帧）+ `RealTime` + 无限 GOP + ABR 常量码控（`rc-encode-vt/src/ffi/mod.rs`）~~，没有剩余开关~~（**2026-11-21 更正**：彼时漏查 `encoderSpecification` 键 `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`，第 3 轮启用后 P95 降 37%–40%，见下）。

**剩余瓶颈（~~VideoToolbox C API 不可达~~ → 2026-11-21 更正：尾部尖峰已由 `EnableLowLatencyRateControl` 消除）**：彼时误判 P50 − min ≈ 3.5ms 的离散与 P95 ≈ 33ms 的
周期性尖峰在 C API 中无旋钮（回调队列 QoS 确无公开入口），实际 `EnableLowLatencyRateControl` 即可压平 P95 尾部停顿簇（详见下方第 3 轮闭环）。

**VT 调优第 2 轮（2026-11-20，agent 实跑，release 真机）**——先诊断后动手，P50 中位压到 8ms
线内（贴线），关键成果是**延迟分解定位 + 测量对齐真实工况**：

| 子轮 | 改动 | P50 / P95（ms） | 结论 |
|---|---|---|---|
| A 诊断 | cfg(test) 分解探针（submit→callback / callback→唤醒 / 唤醒→返回） | 8.41 / 32.36 | **全部延迟在 VideoToolbox 内部**（submit→callback P50 = 8.20、min = 4.61；唤醒 0.06、提取 0.02）；相邻 callback 间隔 P50 ≈ 8.4ms，投递疑似按 ~8.3ms（120Hz）节拍对齐；直方图双峰（主簇 + ≥20ms 停顿簇） |
| B 编码器 | 提交后逐帧 `CompleteFrames(本帧 PTS)` 强制同步完成 | 9.22 / 32.87 | 无改善 + 无界阻塞违反 E5「超时不阻塞」→ **否决回滚**。纠正：该轮观察到的 11 个 IDR 并非 CompleteFrames 所致（见下） |
| C 测量 | 测试按 **60fps 节奏提交**（与帧时间戳 16_667µs/帧、ExpectedFrameRate=60 对齐；原测试背靠背 ~107fps 让 PTS 谎报节奏、码控按 60fps 误判） | **7.84** / 32.46；复跑 7.62、8.01 | 主簇收紧到 6–8ms（170/300 帧）；**中位 7.84 < 8，但贴线**（3 次里 1 次到 8.01） |

编码器代码本轮**零净改动**（B 已回滚；A/C 均为测试侧：探针 cfg(test) 门控 + 提交节奏）。
数字下降的归属是**测量对齐真实工况**，不是编码器变快。

**遗留事实与 2026-11-21 实证闭环（后续轮次/任务卡的输入）：**
- ~~≥20ms 停顿簇 119/300 帧（~40%），两次独立运行数量相同——VT 内部确定性停顿，决定原基线 P95 ≈ 32ms，C API 无旋钮。~~ **【2026-11-21 更正】旧结论「C API 已无手段、尾部停顿簇不可消除」已证伪**（见下方 `EnableLowLatencyRateControl` 调优轮）。
- **已实证并修复（2026-11-21，无限 GOP）**：`MaxKeyFrameInterval = 0` 经核对 Apple SDK
  `VTCompressionProperties.h` 与真机探针证实，其真实语义为「**由编码器自决关键帧位置**」
  （Apple Silicon 硬件编码器实测每 30 帧自动插 1 个 IDR，300 帧产出 `[0, 30, 60, 90, 120, 150, 180, 210, 240, 270]`
  共 10 个 + `request_idr@150` 1 个 = **11 个 IDR**），而
  `MaxKeyFrameIntervalDuration = 0.0` 为「无时间间隔上限」。已在 `rc-encode-vt` 新增纯函数
  `vt_keyframe_interval_params(gop_len)`（`0 → i32::MAX`，常规值直通，`> i32::MAX` 饱和钳制到 `i32::MAX`，
  `duration = 0.0`），更新 `CompressionSession::new` 与注释，并补充 3 个纯逻辑单测及 1 个 `#[ignore]`
  真机回归测试 `infinite_gop_emits_only_initial_and_forced_idr`。真机复测（`videotoolbox_realtime_flag_is_set`，
  300 帧 @60fps 含首帧 + frame 150 `request_idr`）：修前 P50 = 8.02ms / P95 = 17.98ms /
  min = 5.88ms / max = 20.35ms（`submit→callback` max = 35.04ms，相邻 callback 间隔 max = 64.23ms，关键帧 = 11）；
  修后 P50 = 8.06ms / P95 = 17.98ms / min = 5.94ms / max = 19.02ms（`submit→callback` max 降至
  **18.87ms**，相邻 callback 间隔 max 降至 **25.01ms**，关键帧 = **2**，0 自动插 IDR）。
  `infinite_gop_emits_only_initial_and_forced_idr` 实测 OSStatus = 0（E6 自检通过），300 帧
  `gop_len=0` 仅产出 1 个首帧 IDR（`[0]`），第 300 帧显式 `force_idr=true` 立即输出 IDR；
  组合探针确认 `MaxKeyFrameInterval=i32::MAX` + `Duration=0.0` 与 unset/86400.0/1_000_000 等价（1 IDR），
  而 `MaxKeyFrameInterval=60` 产出 5 个 IDR（`[0, 60, 120, 180, 240]`）。
- **VT 调优第 3 轮（2026-11-21，`EnableLowLatencyRateControl` 实证闭环——更正旧结论）**：
  此前两轮只查了 session property，漏掉 `encoderSpecification` 键 `kVTVideoEncoderSpecification_EnableLowLatencyRateControl`（macOS 11.3+ 公开 C API，须在创建 `VTCompressionSession` 时传入）。在 `crates/rc-encode-vt/src/ffi/mod.rs` 启用 `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = kCFBooleanTrue` 后，300 帧 1080p @60fps 真机实测（全部 3 项真机 `#[ignore]` 测试通过）：
  - 修前：P50 = 8.06ms / P95 = **17.98ms** / max = 20.35ms；
  - 修后 Run1：P50 = **9.22ms** / P95 = **10.72ms** / min = 5.93ms / max = **14.62ms**（`submit→cb` P50 = 8.67ms / P95 = 10.01ms）；
  - 修后 Run2：P50 = **9.40ms** / P95 = **11.29ms** / min = 6.00ms / max = **16.13ms**（`submit→cb` P50 = 8.86ms / P95 = 10.30ms）；
  - **P95 骤降 37%–40%**，双峰尾部停顿簇彻底消失、收紧为紧单峰分布，0 自动插 IDR。虽然单段 P50 略升 ~1.2ms（8.06 → 9.22–9.40ms），但消除尾部抖动并叠加 SCK `FrameMailbox`、pipeline `cap_space` Condvar 及 `max_in_flight=1` 后，B3 真机全链动态 60s 端到端 P50 从 19.087ms 降至 **14.476ms**（进入 ≤18ms 门槛且留 3.5ms 裕度），拖动 15s P50 从 25.459ms 降至 **14.193ms** 并全项通过。
- **`annexb.rs` 256..=511 字节 AVCC NAL 长度误判修复（2026-11-21）**：
  隐私幕布（`--curtain`）纯黑/低熵画面生成小尺寸 NAL 时暴露隐蔽比特流损坏缺陷——4 字节大端 AVCC NAL 长度在 `256..=511` 区间内其字节前缀恰为 `00 00 01 XX`（实测 `--curtain` 下 328 字节 `00 00 01 48`），旧实现先检查 3 字节 Annex-B 起始码 `00 00 01` 将 AVCC 帧误判为 Annex-B 并把长度末字节当作 NAL 头。已在 `crates/rc-encode-vt/src/annexb.rs` 改为**先严格按 AVCC 长度前缀完整解析**，仅在非合法 AVCC 时回退 Annex-B 检测，并新增回归测试 `avcc_nal_length_256_to_511_not_misclassified_as_annexb`。
- **下轮 B3 收口清单（3 项阻塞项已定位精确根因；✅ 已全部由下方「终验收口轮」解决）**：
  1. `rc-trace` 4096 环形缓冲截断 + `rc-loopback::run_real()` 丢弃 `controller.displayed()`（导致 60s >68fps 场景不变式失败、`convergence_ms` 虚高至 3090ms、`displayed_fps` 低估）；
  2. 动态 60s 真实有效帧率 56.70fps < 58fps（SCK `minimumFrameInterval = 12_500µs` 在 ProMotion 120Hz 下 8.33/16.67ms 交替到达，与 VT 均值 11.66ms + `max_in_flight=1` 产生 842 次 `dropped_budget`）；
  3. 静态 30s 偶发系统合成突发致 `encoded=156`（`run_real` 固定 `DiffMode::TrustPlatform`、`SckBackend` 无帧内容去重）及 `report.rs::apply_gate` 静态零编码判定分支缺失。

- **外部审计修复轮（2026-11-21，七项全修；主 agent 补两个收尾）**：rc-net `loss_rate`
  主动结算/去重、流角色按 ID、`wrapping_sub` 回绕、卫生与有界通道；macOS 滚轮行单位统一
  （真机回环 3/3）；rc-input 零丢消息。收尾发现两个编排/工程坑：
  ① workflow 门禁循环末轮修复后未复跑（脚本瑕疵，主 agent 手工补跑全绿，366 passed）；
  ② **libtest 默认并行导致真机测试互踩**——滚轮测试与回环测试同时注入系统事件互相污染
  （修复：`crate::rm_lock::REAL_MACHINE` 全局互斥 + 回环断言按注入坐标族过滤）。
  真机套件 4/4（注入见证 / 回环 / 延迟探针 p50=0.31ms / 滚轮行单位回环）。
- **macOS 输入真机 3 项收口轮（2026-11-21，用户授权辅助功能+输入监控后）**：
  3/3 通过（注入 tap 见证 / 捕获→注入回环 / 延迟探针 p50=0.66ms）。过程中修掉三个
  **真机测试方法论坑**（产品代码零缺陷，全部是测试断言面选错）：
  1. **HID 计数器对合成事件失明**：`CGEventSourceCounterForEventType(HIDSystemState)`
     不涨——CGEventPost 合成事件只进 Session 态（run1 偶尔通过纯属 200ms 窗口混入
     真实硬件移动）。已删除该断言与 extern，改 tap 见证。
  2. **光标最终位置断言物理上不可保证**：本机有活的物理输入（用户的手），系统在
     合成移动后立即重锚到物理鼠标位（实测注入前后位置纹丝不动）。同因：回环测试
     里的「噪声移动」实为系统重锚，非 bug。
  3. **零位移移动被合并**：光标已在目标位时注入同位移动不产生事件（测试改为避开
     当前光标位选目标，`ffi::current_cursor_location` 用 **CombinedSessionState** 源
     快照——HID 态源读不到合成位置）。
  回环断言从严格相等改为**关键事件有序子序列**（真机噪声容忍）+ I2 移动合并单独
  按数量/坐标断言。全仓 349 全绿 / clippy 0 / fmt 干净。
- **B3 终验收口轮（2026-11-21 深夜，主 agent 交接续作，M1 Gate 全项通过）**：
  - **回退 SCK PTS 节拍过滤**（`rc-capture-macos/src/ffi/mod.rs`，`should_emit_pts` /
    `callback_min_spacing_us` 连同 ivars、调用点、120Hz 单测整段删除）：根因坐实——SCK 在
    60fps 内容下的到达节拍是 8.33/16.67ms **交替**，两连拍后再到帧时「距上一放行帧」已累计
    25ms 假间隔，过滤器按 14.58ms 阈值误放行/误拒绝叠加，采集被压到 48.2fps（上轮终验
    44.6fps 失败的直接原因）。多余供给（~70fps）交还调度器预算门处理。
  - **`max_in_flight` 1 → 2**（`rc-pipeline/src/host.rs`）：Phase A 短测（6s）曾按 P50 选 1，
    但 60s 长测显示 mif=1 预算丢帧 842 次、displayed 56.7fps < 58 硬门槛；mif=2 双达标且
    0 丢帧（VT 异步会话，在途 2 重叠提交/完成开销，假串行编码器的「在途 2 = 白等」不适用）。
    终验：**动态 60s P50=11.283ms / P99=21.203ms / 70.0fps / 4201 帧全上屏 / exit 0 零失败项**；
    运动 15s P50=11.164ms / 69.47fps 全过；静态 30s CPU 稳态 1.51%（见 §12 终验三行）。
  - **隐私幕布固化进仓库**（`rc-render-macos/tests/curtain.rs`，`window-bench` feature，
    harness=false）：上轮幕布是外部临时进程、随 agent 中断丢失。本轮重建时踩掉并文档化两个
    macOS 坑：① 无 `NSApp.run()` 时 `orderFrontRegardless` 的窗口**不上屏**（`screencapture`
    实拍仍是桌面）；② 单 IOSurface 反复重写 + 纹理缓存复用时合成器看到**冻结画面**（SCK 仅
    回调 3–8fps 系统噪声）。最终架构：主线程 NSApp 常驻 + 渲染线程 4 缓冲环 + 60fps 混合
    节拍（sleep 到剩 2ms 再自旋）。同源隐患待复核：`tests/window_present.rs` 基准同样未跑
    `NSApp.run()`，其「真实窗口完整 present 路径」数字（§12 的 8.3ms@合成器节奏）可能取自
    不可见窗口，下一轮应复核。
  - 回归：**274 测试全绿**（较上轮 −1 为被删过滤器的专属测试）、clippy `-D warnings` 0、
    fmt 干净。caffeinate 测试辅助已停。

- **T09 rc-net 实现轮（2026-11-21，阶段 C 首卡，`crates/rc-net/` 全新 6 文件 + 2 测试文件）**：
  - **quinn 选型**：`0.11.12`，`default-features = false` + `runtime-smol` + `rustls-ring`——
    不拖 tokio；smol 全局执行器让端点 driver 自跑在后台线程，这正是同步 `Transport` trait
    嵌入 quinn 的最省事方式（卡内坑 5）。async→同步桥为**自写无 unsafe `block_on`**
    （`std::task::Wake` + thread parking，`quic.rs`，~20 行/个 ×2）：接收侧两条专属线程
    （datagram / reliable 流）经 `std::sync::mpsc` 汇入 `poll` 的事件队列。
  - **坑 1/2 的真身**：quinn 0.11 **没有独立的 `max_datagram_frame_size` 旋钮**——
    `TransportConfig::datagram_receive_buffer_size(Some(n))` 同时是「能收 datagram」的开关
    与尺寸协商（对端通告 `min(n, u16::MAX)`），两端都设即闭环（`quic.rs::transport_config`）。
  - **新坑 A（QUIC 流隐式打开）**：client `open_bi` 是本地动作、不产生任何包；不写数据，
    server `accept_bi` 永远看不到这条流（实测：connect Ok 而 accept 20s 超时）。修法：connect
    握手后立即写 4 字节 0 长度引导帧，接收侧把长度 0 的消息当占位丢弃（`quic.rs`）。
  - **新坑 B（FEC 冗余包预算）**：XOR 的作用域取「完整数据报（含 32B 头）」——丢失分片的
    `payload_len` 随头一起恢复，100KB 逐字节重组才成立；但冗余包 = 最长数据报 + 自身 32B 头，
    按普通预算分片会超 N2 上限（实测 1184 > 1152 被拒）。新增
    `max_media_payload_with_fec(mtu) = mtu − 48 − 2×32 = 1088`，测试发送路径采用。
  - **新坑 C（自签证书）**：新版 `openssl req -x509` 默认加 `CA:TRUE`，rustls 拒绝
    `CaUsedAsEndEntity`；须显式 `basicConstraints=critical,CA:FALSE` + `serverAuth` EKU +
    SAN（`tests/certs/`，include_str 内嵌，仅测试用，有效期至 2126）。
  - **新坑 D（Q3 语义闭环）**：接收端 `DEFAULT_MAX_CONCURRENT_FRAMES=2` 会**正确拒绝**倾泻
    流（N10），发送端必须窗口化（≤2 帧在途，与 `rc-sched` 的 `max_in_flight` 同口径）——
    丢包测试最初全量倾泻只重组到 37/100，改窗口 2 后 98/100。这不是传输层 bug，是 Q3 在
    端到端起作用；T10/会话层接手时沿用此窗口语义。
  - 丢包注入为**本地单线程 UDP 代理**（`tests/common/mod.rs::LossyProxy`，`StdRng` seed
    固定，双向 Bernoulli 3%，语义对齐 rc-loopback 的 `DamageInjector`/T02；rc-net 不向上
    依赖 rc-loopback）。QUIC 自身握手/ACK 同样吃丢包，检验的是「连接存活 + FEC 维持成功率」。
  - **flaky 消除（实测教训 ×2）**：① 种子固定 ≠ 丢包落点固定——代理收包顺序随线程调度重排，
    「哪些包被丢」每轮重摇，100 帧 × 6 片配置的帧失败数在 1–6 波动、贴 95% 线（30 连跑
    2 败）；按「小组数（4 片）× 大帧数（300）」重配后失败率 ~0.2%，20 连跑 0 败。② 丢包期后
    的存活 probe 必须与常规媒体同规格（带 FEC）且允许换序号重发——裸发（无冗余）时单包
    丢失即 500ms 判丢，观测到 8/15 失败。
  - 重组层 `FrameAssembler` 的时间注入模式：`push` 的到达时刻采样自 `gc` 注入的时钟
    （调用方每事件循环调一次 `gc`，与防内存增长同节奏）；FEC 组号回绕（坑 7）按
    「组号 + 帧序号」联合定位。N6 判丢帧记 `LostInNetwork`（`mark_frame_outcome`），
    发送侧 `Sent` 打点在 pacer 真正写 socket 的时刻（帧尾包）。
  - **遗留（非本卡范围）**：① N9 连接迁移机制在位（`migration(true)` + keep-alive +
    `remote_address` 变化 → `PathChanged`），但无自动迁移测试（本卡测试清单未列，真实
    迁移需换端口/地址的 harness，归 T10/会话层）；② 拥塞控制用 quinn 默认（卡内坑 3），
    M3 经 `QuicTransport::set_pacing_rate` 接入；③ `NetStats::loss_rate` 是应用层 NACK
    口径的估计（FEC 恢复兜底的帧不计入丢包），供 M3 调冗余率用。
  - 回归：**27+274=301 测试全绿**、`cargo clippy -p rc-net --all-targets -- -D warnings`
    0 警告、fmt 干净、无 `unsafe`。

- **T10 输入通道实现轮（2026-11-21，阶段 C2：`crates/rc-input/` 全新 4 文件 +
  `crates/rc-input-win/` 骨架 3 文件 + `rc-input-macos` 新增 `scancode.rs`/`bridge.rs`）**：
  - **平台裁决执行**：按 §6 重排（macOS P0 / Windows P1），本卡以「全平台核心 +
    macOS 桥接」落地；`rc-input-win` 只放 `cfg(windows)` 声明骨架（`SendInput`/
    `GetCursorPos`/`WH_*_LL` extern + 结构布局 + D 阶段 TODO），非 Windows 编译为空，
    **刻意零测试**（没有能在非 Windows 上真实断言的行为，不写空过假测试）。
    骨架注入/捕获方法恒返回显式 `Err`（I9 语义：不 panic、不假装能工作）。
  - **线格式与 rc-net 的成帧叠加（文档出入点）**：卡内线格式自带 `[len(u16 LE)]`
    消息前缀；rc-net `send_reliable` 在传输层又用 `u32 LE` 前缀成帧并按消息边界交付。
    两层叠加无害（u16 供事件段独立校验，u32 保证 QUIC 流消息边界），按实际代码适配，
    依据与限制写在 `crates/rc-input/src/channel.rs` 模块文档。
  - **I1 的实际语义**：rc-net 每连接只有**一条**双向 reliable 流，控制/输入共享
    （rc-net `lib.rs` 通道分工表）；但媒体走 datagram+独立 pacing 队列，输入
    `send_reliable` 直写 QUIC 流**不经过媒体队列**——I1 的操作语义（输入永不排队在
    视频帧后）满足，测试断言输入路径 0 datagram。真「第二条独立流」待会话层需要时
    再扩 rc-net（TODO 记在 channel.rs）。
  - **`CGEventSource` 非 `Send` 坑**：`rc_input::InputInjector` 契约要求 `Send` 超特征，
    而 core-graphics 0.24 的 `CGEventSource`（foreign_type!）未实现 `Send`（探针实测
    E0277）。修法：`rc-input-macos/src/ffi/mod.rs` 增加 `unsafe impl Send for CgPoster`
    （SAFETY：CF 引用计数对象 retain/release 线程安全；CGEvent 构造与 `CGEventPost`
    可从任意线程调用；使用方仅 `&mut self` 串行访问，Send 足矣）。桥接的假投递器
    同步从 `Rc<RefCell>` 换 `Arc<Mutex>`。
  - **键码笔误被穷尽 match 抓住**：scancode 表初版把左 Alt 写成 `0xE038`（应为
    `0x38`；`E0 38` 是右 Alt/AltGr），宏生成的双向穷尽 match 直接报
    unreachable pattern，对照测试再锁定（`rc-input-macos/src/scancode.rs`）。
  - **Windows 骨架的离机类型检查**：cargo 在 macOS 完全不编译 `cfg(windows)` 模块
    （连语法都不查）。用 `rustc --crate-type lib --extern rc_input=<真实rlib>
    --extern rc_types=<真实rlib> -L dependency=target/debug/deps` 单独编译骨架文件，
    抓到真实错误 E0740（Rust union 字段必须 `Copy`——`MOUSEINPUT`/`KEYBDINPUT`
    补 `#[derive(Clone, Copy)]`），终态 0 错误。Windows 真机编译/运行仍未验证（无
    Windows 工具链，属 D3）。
  - **macOS 无捕获层可桥接**：T12 只交付了注入（CGEventPost），无捕获；本卡未实现
    `InputCapture` 的 macOS 版（CGEventTap + 输入监控权限是真实现，不是桥接），
    留待后续卡。X1/X2 侧键 CGEvent 路径不支持（`PointerButtonKind` 仅左/右/中），
    桥接显式返回错误而非静默丢弃。
  - **I8 实现口径**：`ModifierState` 只信 Key 事件序列（不信远端 `modifiers` 快照
    字段），批处理器把每条 Key 事件的 `modifiers` 重写为**本端跟踪快照**发送；
    `release_all` 兜底对应坑 7（松键丢失防卡键）。
  - 回归：**328 测试全绿**（rc-input 16 + rc-input-macos 18/1 ignored + 其余），
    全仓 clippy `-D warnings` 0 警告，fmt 干净，rc-input 无 `unsafe`。

**T09 审计遗留修复轮（2026-11-21，agent 实跑，本地回环）**——两条遗留 + 一个新踩的坑：

- **输入独立流（遗留 1）**：`Transport` 增带默认实现的 `send_reliable_urgent`
  （默认委托 `send_reliable`，rc-input 的 MockTransport **零改动**照常编译），
  `NetEvent` 增 `ReliableUrgent(Vec<u8>)`；`QuicTransport` 连接建立时开第二条
  双向 reliable 流（先控制后 urgent，accept 按流打开顺序对应），两端
  `set_priority(URGENT_STREAM_PRIORITY=1)`。契约 §7.3 增 N6 硬性要求，
  rc-net `lib.rs` 通道分工表改三行。
- **新坑（连接级流控共享窗口）**：首版直接在调用线程 `write_all` 写 urgent 流，
  实测 12.8MB 控制洪流下 urgent 排到 182/200 条之后——根因是 quinn 的
  **connection 级流控窗口被所有流共享**（`write_limit()`，quinn-proto
  streams/mod.rs:254：`min(max_data - data_sent, send_window - buffered_data)`），
  控制洪流钉死连接信用时 urgent 的 `write_all` 连本地缓冲都进不去；stream
  priority 只排序**已入本地缓冲**的数据。修复：urgent 发送改为自有
  mpsc 入队 + 专职写线程（`rc-net-urgent-writer`，`recv_timeout(50ms)` +
  stop 旗标保证 Drop 不死锁），调用方永不阻塞，写线程吸收信用等待。
  修复后实测：发送调用 9µs、交付位次 82/200（超越 7.6MB backlog）。
- **测试量度坑**：「urgent 之前的控制消息数」必须在 urgent 事件处**快照**
  计数——poll 批次里排在 urgent **之后**的事件也会在同一 for 循环里入账，
  首版统计把「之后」数进「之前」，得出 183/200 的假阴性（插桩事件通道序
  证明真实位次 82）。位次（通道序）是与 poll 时机无关的队头阻塞度量。
- **N9 迁移测试（遗留 2）**：`QuicEndpoint` 增 `rebind(UdpSocket)`（薄封装
  quinn `Endpoint::rebind`）；测试直连建对（需持客户端端点，`common::pair`
  不返还端点故手动搭）→ 基线 2 帧 → rebind → 迁移后 6 帧全到齐 +
  `PathChanged` 上报 + 双向 reliable round-trip。
- 回归：rc-net **30 全绿**（23 单测 + 7 集成，原 27 项全保留）、workspace
  **348 passed / 0 failed**、clippy/fmt 干净、无 `unsafe`、
  `DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动。

**rc-input-macos 审计修复轮（2026-11-21，agent 实跑）**——两项审计问题闭环，
原 31+2 离线与 3 项真机全保留：

- **滚轮单位不一致（高）**：捕获侧 `ffi/tap.rs` 读 `SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`
  （行单位，典型 ±1..±3），注入侧 `ffi/mod.rs` 曾以 `ScrollEventUnit::PIXEL` 投递——
  macOS↔macOS 下 1..3 行被注成 1..3 像素微动。**机理先实证再修**（临时探针程序，
  本机 darwin 27.2）：PIXEL 单位构造 w1=-3 的事件，`AXIS_1` 只读回 -1、`PT_AXIS_1`=-3
  （系统按 ~10px/行换算）；LINE 单位构造则 AXIS_1 逐值读回。修复：注入抽成
  `ffi::new_line_scroll_event`（LINE 单位唯一构造点，`CgPoster::post` 复用）；
  文档全链统一为行（`plan.rs`/`event.rs`/`bridge.rs`/`capture.rs`），rc-input
  `MouseWheel.delta` 文档澄清规范单位=行（**仅文档；源码逻辑未动**——
  channel.rs:119 另有一处 T10 遗留的历史 fmt 漂移致统一门禁 `cargo fmt
  --all` 第 2 轮 exit=1，已按门禁要求施加 rustfmt 纯格式修复：4 行换行
  布局、零语义变化，`cargo test -p rc-input` 22+1 照绿）。
- **无界通道（中）**：tap 回调→消费通道改 `sync_channel(256)`，满时丢最旧
  （`send_drop_oldest`：`try_send`/`try_lock`/`try_recv` 全非阻塞——CGEventTap
  回调阻塞会被系统摘除 tap；消费侧持锁降级丢当前，单生产者下腾位后重投必成功）。
  接收端因此移进 `Arc<Mutex<Receiver>>`（回调与 `CgCapture::drain` 共享），
  drain 持锁只在无事件时长持（那时通道空、无满载丢弃），poison 容错取回。
  丢弃独立计数 `TapCtl::dropped_channel_full` → `CgCapture::dropped_channel_full()`
  （本 crate 无 log/tracing 依赖，以独立 API 暴露替代日志，未引新依赖）。
  **签名变化涟漪**：`spawn_tap_thread` 返回 `Arc<Mutex<Receiver>>`，
  `tap_thread_lifecycle_never_hangs` 与 drain 测试做了机械适配（断言语义不变，
  实测仍过）；Disconnected 分支在当前接线不可达（回调闭包自持接收端 Arc），
  防御性保留并注明。
- 回归：rc-input-macos **36 单测 + 2 doctest 全绿 / clippy `-D warnings` 0 警告 /
  fmt 干净**（新增 5 离线：`line_scroll_event_roundtrips_tap_fields`、
  `wheel_line_units_survive_capture_to_injection_chain`、`send_drop_oldest_drops_oldest_and_counts`、
  `tap_channel_capacity_is_256`、`dropped_channel_full_reads_tap_ctl_counter`）；
  真机 `./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture`
  **4/4 通过**（原 3 项不破坏 + 新增 `wheel_line_unit_roundtrip_real_machine`
  3/3 等值、`dropped_channel_full=0`）；rc-input 22+1 照绿；workspace check 干净；
  未 git commit（按纪律交由编排方）。

---

**rc-net 传输审计修复轮（2026-11-21，子 agent 实跑）**——五项审计问题闭环，
只动 `crates/rc-net/` 及其测试，`DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动，无 unsafe：

- **loss_rate 失真（高）**：根因有二，均在旧 `track_arrival`（quic.rs）：
  ① `observe_nack` 只在跟踪表 >4096 帧的淘汰路径触发——60fps/3% 丢包下
  填满要 30-60 分钟，期间 `stats().loss_rate` 恒 0.0，FEC 冗余率与拥塞控制
  全部失明；② `entry.1 += 1` 不按 `packet_idx` 去重，重复分片会把缺包帧
  误判完整（凑齐即清表、丢失永不入账）。修复：`ArrivalTracker`（时间注入
  纯逻辑，铁律 4）按 `HashSet<packet_idx>` 去重，凑齐立即结案、500ms
  （`TRACK_STALE_US = reassemble::FRAME_STALE_US` 同一条放弃线）未凑齐
  主动结算成 arrival/nack；`poll()` 每轮无条件 `settle`。**时坑**：时钟不能
  用 `Timestamp::since`（u32 饱和到 MAX 后冻结），故意 `as u32` 截断回绕 +
  判定 `wrapping_sub`——与审计项 3 同一纪律。**测试口径坑**：集成断言
  不能静态读一次——整测 ~1s 内最后 500ms 的丢包尚未结算，首读只有
  ~0.006（非零但偏低）；固定再等 2s 结算完，三连跑收敛 0.021–0.027，
  与代理实测丢率 0.022 对上，量级断言取 (0.01, 0.06)。
- **流角色互换（中）**：旧 `accept` 按 `accept_bi()` 两次返回先后分配
  (control, urgent)。QUIC 帧可靠但**流的可见顺序不可靠**——先开流的
  STREAM 帧所在包丢了要重传，后开流可能先交付。修复：角色按
  `RecvStream::id().index()`（`id>>2`，同发起方单调递增）判定，提纯为
  `control_is_first(a,b)`（小者=control，相等=None 协议异常）可单测；
  `connect` 侧 `open_bi` 天然有序不变。
- **u32 µs 回绕（中）**：`reassemble.rs`/`fec_adapter.rs` 两处 gc 的
  `saturating_sub` 改 `wrapping_sub`（Stage F 8h soak 跨 71.5min 回绕点，
  饱和版算出经过时间≈0、条目永不清理）；两端构造 `t0=u32::MAX-…` 跨回绕
  对测「恰 500ms 不触发 / 501ms 触发」。
- **卫生**：`QuicTransport::new` 六参收成 `(conn, control, urgent, mtu)`
  四参（`ReliableStream` 收拢 send/recv 成对参数），`#[allow(too_many_arguments)]`
  删除（全 crate 0 个 allow）；Drop 先 `urgent_tx = None` 关入队口再 join
  （写线程 `recv_timeout` 立即 Disconnected，免等一轮 50ms）。
- **urgent 无界通道**：`channel()`→`sync_channel(64)`，满队 `try_send`
  映射为说明原因的错误（`urgent_enqueue_error` 纯函数单测）——**不在
  rc-net 丢输入**：移动事件可丢、按键不可丢的语义判断在 rc-input 层，
  选择写进 `URGENT_QUEUE_CAPACITY` 与 `send_reliable_urgent` 文档。
- 回归：rc-net **32 单测 + 8 集成 = 40 全绿**（原 30 项全保留；新增 10 项），
  全套 3 连跑稳定；clippy `-D warnings` 0 / fmt 干净 / `cargo check
  --workspace` 通过；未跑全 workspace 测试（正式门禁由编排脚本统一运行）；
  未 git commit。

- **审计二轮修复（2026-11-21，workflow「审计修复轮：乱序丢包与批内隔离」）**：
  ① `ArrivalTracker::observe`（crates/rc-net/src/quic.rs）顺序 bug——**结案拦截先于在途表
    查找**：乱序网络下单片帧（seq=10）把 `highest_retired` 推到在途多片帧（seq=9）之上后，
    后者的迟到分片被 `seq <= r` 直接吞掉，帧永远凑不齐、500ms 后被 `settle()` 记幻影 NACK
    （loss_rate 虚高 → FEC/拥塞控制误动作）。修复 = 在途表查找整块移到拦截之前（「**在途
    优先于结案拦截**」，与 reassemble.rs 同纪律）；「已结案且不在途」的迟到分片仍忽略——
    所有结案路径（凑齐/settle_one/单片帧）均先 remove 再 note_retired，表内帧从未结案，
    不变式天然成立；`note_retired` 单调不回退防已结案帧复活重复计数。
  ② `InputBridge::inject`（crates/rc-input-macos/src/bridge.rs）批内错误隔离——`convert(ev)?`
    与注入 `?` 提前返回会吞同批剩余事件（X1/X2 侧键报错后紧随的松键被吞 = 永久卡键）；改
    `first_err.get_or_insert` 记首错继续，与 `pump_receiver` 同语义（粒度差异：pump 按消息、
    inject 按事件，语义同为「记首错、继续、批尾返回首错」）；convert 的 `last_pos` 副作用仅
    在 MouseMove 成功路径更新，与错误隔离互不干扰。
  编排：两修复员并行 → 全仓门禁（369/0 + clippy/fmt）→ 只读独立复核（含「新测试在旧代码
  上必失败」的推演验证，复核员另亲跑 crate 级 11/12 全绿交叉印证）；脚本内建末轮终复跑，
  本轮无手工补跑。仍未 git commit。

- **M2 会话核心轮（2026-11-21，三段式：写卡 workflow → 实现 workflow → 两轮根因修复 workflow + 主 agent 真机实测）**：
  - **写卡**：rc-session 定案（不放 rc-net——T09 红线；不放 rc-pipeline——M1 稳定层不塞连接
    生命周期；rc-cli 保持薄）。T13–T22 十卡，评审 13 项修复（审批抽象统一、dev_cert 不漏
    rustls、铁律 4 grep 豁免口径闭环等）。发现并处置：并行 UI 会话的 crates/rc-ui/（Tauri
    v2，12 屏已过视觉验收）当时已在 members 里——**移出 workspace + exclude**（T21 轮接入
    前不该进全仓门禁，Tauri 依赖会拖垮构建）。
  - **实现**：六卡串行（T15/T16 逻辑并列但同改 client_session.rs，串行避免同文件竞争），
    每卡后 cargo check 绊线。实现员共记录 30+ 处「卡与代码出入」（按事实实现、卡不动），
    复核员逐卡核对。T17 施工中曾误判「子 agent 挂死」——**spent_tokens 只计已结算步骤**，
    未结算的执行中 ask 不进总数；用文件 mtime（client_session.rs 5 分钟内被改）判定存活，
    避免了一次错误的 AmendWorkflow 重跑。
  - **根因 1（MTU 错配）**：分片预算（pipeline 侧 32B 头在外）与数据报预算（rc-net 侧
    32B 头在内）两套口径在 1200 处撞车：1120+32=1152 恰好闭合。教训：**跨 crate 的字节
    预算必须单点定义**，两端各自取整必然错配；M1 loopback 直连没有这道墙所以全绿。
  - **根因 2（时钟锚定）**：SckConfig.origin_us=None 时 SCK 以首帧 PTS 为零点，会话钟以
    工厂采样为零点，Δ=首帧延迟 ≫ target_latency=22ms → 帧帧 Stale。**排查盲飞 2 轮的教训**：
    host 会话摘要原本没有 captured/dropped_stale 统计（M1 的 report 有），第一反应应该是
    「统计面先补齐再看」而不是继续猜——先测后拆同样适用于排查本身。修复顺序对齐
    rc-loopback:264-265 的既有正确实现。
  - **本机双进程方法论三坑（真双机不存在，但本机验证 M2 必须绕过）**：① dev-run.sh 单
    实例设计——两个 launcher 并发 rm -rf 同一 app 目录会互炸（解法：先打包、cp -R 出第二
    个 bundle，双实例分别 open）；② client 全屏 Metal 黑窗在首帧前盖住供给源 → SCK 看到
    静止 → 铁律 2 零编码 → **自饿死死锁**（解法：client 请求小分辨率 640x360 让窗口不遮
    幕布；或幕布后起压顶——但首帧/fps gate 会吃伪影）；③ 幕布供给的「采集内容=client 呈现
    内容」自反馈回路让 1080p fps 不规则（Run C 45.5fps 归因于此，不作为产品数字）。
  - **遗留（下轮输入）**：首帧 630–1306ms 方差贴 800ms 线（优化候选：host 在 accept 等待
    期预建 SCK 流 + 编码器预热）；dev-run.sh test 子命令打包最后一个测试二进制的漏跑缺陷；
    T19 鉴权 / T20 剪贴板 / T21 UI（rc-ui 接入）/ T22 托盘四卡待领取；双机真机实测（用户
    裁决本轮不做）。仍未 git commit（M2 代码 + 回写全部未提交）。

- **高帧率轮（2026-11-22，用户需求「120 144 240，本机 MBP 只能到 120」）**：
  - 实测结论分层：**管线不是 120fps 瓶颈**（640x360×120 全链 114.6fps 无损，captured==
    encoded==displayed 逐帧对齐）；**SCK 供给上限 ~115fps**（ProMotion 下 120fps 配置实采
    107-115fps，随分辨率变化）；**VT 1080p 硬编吞吐 98.9fps** 是 1080p×120 的物理上限
    （单帧 P50 9.2ms > 8.33ms 间隔）——高帧率×高分辨率的正解是 M3-4/3-5 码率/分辨率
    自适应，不是硬扛。
  - **VT 过载丢帧曾直接拆会话**（Error::Encoder 恒 fatal 的设计假设对过载瞬态不成立）；
    更隐蔽的是 rc-encode-vt 对 Dropped 也置 idr_requested——过载时下一帧强制编最贵的
    IDR 帧，火上浇油。参考链语义辨析：Dropped（未编此帧）无需 IDR，Timeout（编了但弃
    输出）才需要——两者此前混为一谈。
  - 240fps 离线 200.2fps 的天花板是 fake WallEncoder 的模拟编码节奏（~5ms/帧），不是
    调度器；真机 240 需外接 240Hz 屏 + 编码 ≤4.17ms/帧，记账待设备。
  - 遮挡自反馈伪影在高帧率下更明显（displayed 与 SCK 供给互相锁频到 45.5fps），本机
    1080p 高帧率的干净数据 fundamentally 拿不到——结论以小分辨率无损穿透 + 分段瓶颈

- **M2 代码评审轮（2026-11-22）**：六路评审（host+proto / client+watchdog / probe+reconnect /
  CLI+report / 既有 crate 增量 / 三修复交互+高帧率）→ 27 条成立、0 推翻。三个结构性教训：
  ① **u32 时钟纪元管理是系统性弱点**——RefreshGuard 自禁用、CLI client 钟不重锚、退避锚
  跨纪元回绕，三个 medium 同根：会话层把「纪元重建」的职责散落在守卫/驱动器/退避三处，
  没有单点。修复方向：纪元重置随 expect_hello/重建单点化。
  ② **「外部输入杀死进程」族**（high + accept 错误上抛同族）：无鉴权阶段一切远端可触发的
  错误路径都必须按「坏候选≠坏会话」消化，T19 鉴权落地前的窗口期尤其如此。
  ③ **评审盲区在驱动器**：rc-session 集成测试驱动器都「正确地」处理了时钟重锚/错误继续
  驱动，而 CLI 驱动器没有——测试驱动器与生产驱动器的行为差异是多个发现的共同根源，
  后续测试驱动器应尽量复用 CLI 的驱动代码路径。
  下一步：修复轮（high+medium 全修、low 顺手修）→ **8h soak 前必须完成**（时钟纪元族在
- **评审修复轮（2026-11-22）**：四卡布局（A host+reconnect ∥ B client 同文件隔离并行 →
  C CLI 时钟 ∥ D 杂项文件不交叉），门禁一轮修复 clippy 后全绿，复核员对照 27 条逐项通过。
  要点：①时钟纪元族用**全仓 wrapping 纪律**系统性修（refresh_base u32 化 + wrapping_sub，
  而非三处补丁）——第二轮元照常 70min 触发有测试锁死；②「坏候选≠坏会话」成为 Detached 期
  一切外部输入错误的统一处置（消化+计数，进程不死）；③变异验证（逐项移除修复→测试转红）
  在卡 B 全程使用，是修复轮的新基线动作；④known follow-up：host_client 集成套件在同机
  并发负载下有时序抖动（机器安静 5 轮+全绿，卡 D 实测记录），soak 前建议串行跑确认。
  遗留：时钟纪元族的真实 70min 跨越属运行时行为，待 8h soak 验证。
  71.6min 即引爆）→ 提交。
    实测拼合论证。

- **UI 设计规范与 rc-ui 框架决策（2026-11-21，设计轮，零代码）**：
  - 框架决策：rc-ui = **GPUI（zed 官方 crate v0.2.2，2026-10 上 crates.io）+ gpui-kit（longbridge
    v0.10.0，75+ 组件/Lucide 图标/主题系统）**。依据：① 任务书原写「Tauri 或 egui」未定；
    ② 解剖 UU 远控双端安装包取证——macOS 端全原生 Swift（AppKit+SwiftUI+WKWebView，
    `libstreamer.dylib` 独立流引擎，SwiftTerm+tmux 终端），Windows 端 Qt5 Widgets + WebView2
    承载内容页（消息中心/卡券/反馈等），`uuycbridge.js` 证明四端共用同一套 Web 内容 SDK；
    ③ GPUI 由用户裁决选定（纯 Rust + 性能优先路线），`07-engineering-velocity.md` L64 已
    同步修正，不再保留「Tauri 或 egui」表述。
  - 设计文档：`docs/reference/design/08-ui-design.md` 新建——12 张参考图（UU 远控 macOS
    截图）全覆盖：Design Tokens（色彩/字体/间距/圆角/Lucide 图标映射）、16 个基础组件规格、
    主窗口骨架、逐屏规格 P1–P3（主窗口三页）/ O1–O3（浮层）/ W1–W4（独立窗口）/ S1–S4
    （会话中，含 HUD 与控制中心菜单）、交互与状态（端口校验/空态/快捷键/浮层行为）、
    GPUI+gpui-kit 实现映射建议（毛玻璃近似方案）、视觉验收清单、7 项未决事项。
  - 本轮未写任何代码：rc-ui 未建 crate、未建任务卡（实现属 M2-6，前置条件=设计稿确认）。
    验收=文档自查（12 屏对照无遗漏、token 命名交叉一致）+ grep 复核无「Tauri 或 egui」残留。
  - **同日自查修订（用户发起「零上下文可否实现」审查）**：审查结论=初版不能自包含实现。
    修复六类缺口——① mock 数据全量内联为 §6.0（设备 3+8 台、W2 双栏 8+8 行、队列 4 态、
    W4 映射 3 行、P2/O3 文案），清除全部「见参考图」依赖；② 补 6 个缺失组件规格
    （Checkbox/ProgressBar/Toast/ConfirmDialog/ButtonDanger/Badge）；③ 修正 §3.5——Lucide
    无品牌图标，apple/Android/Windows 徽标改形状图标（laptop/app-window/smartphone）并
    注明以 gpui-kit IconName 枚举为准；④ 字号 token 统一（SIZE_H1=20px、新增 SIZE_TITLE
    15px），消除 W2/W4 20px 与 SIZE_H1=22px 矛盾；⑤ 补交互定义：App 初始状态、S2「需
    关注」判定规则（防窥=关闭 或 自动锁屏=关）、发送入队/退出远控/观看模式变体/确认弹窗
    行为；⑥ §2 窗口表补 S4 HUD 与各窗最小尺寸、修正 O3/S2 气泡箭头方向（顶部朝上）。
    文档已带变更记录节。
  - **元素级审查修订（同日第二轮）**：以「任一元素四要素=位置/颜色/尺寸/内容均应可查」为
    标准复审，发现「位置」仅到相对关系粒度——新增 §3.6 布局定位参数（标题栏/侧栏/各屏
    容器间距、图标默认尺寸与 ghost 默认色、popover 距锚 8px 及水平对齐规则、modal/toast
    定位），声明「逐屏未标注时以本节为准」；§9 增元素级抽查法（样例=P2「连接」按钮四要素
    全部可查；验收随机抽 5 元素复查，查不到即回填文档）。
  - **并行策略确认（同日问答）**：确认 rc-ui 可与任务书其他工作并行开工（零 rc-* 依赖、
    mock 先行），设计文档 §8.1 固化接线点清单（11 项：元素→本期 mock→接线目标→前置卡）；
    唯一例外=S1 视频桥接（GPUI↔外部 CAMetalLayer）为技术未知数，开工首项应为最小 spike。
  - **框架改选 Tauri v2（2026-11-21 用户裁决，推翻 GPUI 决策）**：三框架实测与评估后定为
    **Tauri v2 2.12.7 stable（2026-11-18 发布）+ 纯静态 HTML/CSS/JS 前端（不上前端框架）**。
    ① **内存实测**（本机 macOS arm64，release，最小空窗口，两次复测一致）：GPUI 二进制
    4.0MB / RSS **90MB**（无 webview）；Dioxus desktop 4.6MB / RSS 97MB + WebKit 内容进程
    32MB = **129MB**；Tauri v2 同为 webview 架构预计 ≈129MB。用户裁决「100 多 MB 可接受」，
    故内存不再是决定因素。② **淘汰 GPUI**：gpui-kit 三个月内 0.6→0.10 高频破坏式迭代；
    无头测试平台 `draw()` 为空实现（`gpui-0.2.1/src/platform/test/window.rs:269`）导致视觉
    验收无法自动化、只能开真窗口人工看图；macOS 构建默认强制 `xcrun metal`（需完整 Xcode，
    本机仅 CLT，`runtime_shaders` feature 可绕过但着色器优化从 `-MO` 降为默认档）；三平台
    各需特殊构建链。③ **淘汰 Dioxus**：`dioxus-desktop` 自述即「WebView renderer」，内存
    与 Tauri 同档无优势；已推进 0.8.0-alpha.3 破窗期（0.7.10 为稳定版），界面写完即面临大
    版本迁移；开机自启/剪贴板/文件对话框/更新无官方一等支持。④ **采纳 Tauri 理由**：2.x
    稳定线 API 长期冻结；所需桌面能力全有官方插件（autostart/clipboard-manager/dialog/
    updater/single-instance/opener）；CSS `backdrop-filter` 可做**真毛玻璃**（GPUI 只能半透明
    近似）；视觉验收可脚本化截图（Playwright / tauri-driver）——成本远低于 GPUI 的人工看图。
    ⑤ **构建约束厘清**：GPUI 与 Tauri 同样**无法跨平台交叉编译**（Windows 均需 MSVC），
    三平台各自构建属原生桌面常态；Tauri 无需 Xcode metal / fxc，构建门槛更低。
    ⑥ **CI 成本**：GitHub Actions macOS runner $0.062/min（约 Linux 10.3×），Free 档 $12/月
    仅够约 193 分钟；**公开仓库与自托管 runner 免费**。本项目真机权限测试（固定签名 +
    TCC 授权）本就只能在自托管 Mac 上跑，故自托管是必需而非省钱选项。
    ⑦ 设计文档已全面重写 §8（选型依据/目录结构/CSS 组件映射/多窗口/接线点/视频双路径），
    `07-engineering-velocity.md` L64 同步改为「Tauri v2 + 静态前端」。
  - **rc-ui 12 屏已实现（2026-11-21，workflow「实现 RabbitControl UI 12 屏」）**：Tauri v2
    脚手架 + 纯静态 HTML/CSS/JS 全部落地，**6/6 屏通过视觉验收**（visual-judge 逐页判定）。
    ① **交付**：`crates/rc-ui/`（Cargo.toml/build.rs/tauri.conf.json/capabilities/icons/
    src/main.rs）+ `crates/rc-ui/ui/`（6 个 HTML：index/settings/transfer/portmap/terminal/
    session；9 个 CSS：tokens + base + 8 个页面样式；7 个 JS：app/mock/overlay/transfer/
    portmap/session/validate），共 22 个前端文件、**8495 行**；覆盖 P1/P2/P3 + O1/O2/O3 +
    W1/W2/W3/W4 + S1/S2/S3/S4 共 14 个界面（浮层 O1–O3 嵌主窗口，无独立 HTML）。
    ② **编译门禁**：`cargo check -p rc-ui` ✅ 0.56s；`cargo check --workspace` ✅ 0.58s
    （21 个既有 crate 未被破坏）。
    ③ **确定性校验**（替代 LLM 评审，因 provider 卡在 backoff 32 分钟）：6 项检查全过——
    裸写色值 0 处（token 纪律 100%）、12 项 §6.0 关键 mock 文案全命中、6 个 HTML 标签闭合
    完好、4 个 CSS 含 backdrop-filter 毛玻璃、25 个文件齐备、mock 数据与设计文档一致。
    ④ **视觉验收**（documents:visual-judge 两轮）：首轮 4 过 2 败——W2 进度条 47% 填充不可见
    （`.progress__track` 缺 `display:block` 致 `width:47%` 失效）、W3 prompt 显示字面
    `[host]`/`[path]` 占位符；修复后复验**全部 pass**（进度条正常显示 47% 蓝色填充、prompt
    改为 `yii@MacBook-Pro` / `/Users/yii`）。
    ⑤ **实现取舍**（子 agent 记录，已在代码注释留痕）：W2 队列卡 240px 不足容纳 4 行×44 故改
    `min-height:240`；W4 保存键改字段级即时校验以确保红框可观察；O3 箭头描边取 `--stroke`；
    W1 chrome 底带取 `--bg-sidebar`（白卡需非白底才成立）；侧栏分界线由 border 改 inset
    box-shadow 避免 1px 挤压。
    ⑥ **未覆盖**：Tauri 真窗口未实测（backdrop-filter 在 WKWebView 的表现按 §8.2 声明为支持）；
    Rust 侧多窗口创建未做（settings.html 目前 `window.open`）；全部数据为 §6.0 mock，§8.4
    真后端接线属后续；未打包（`bundle.active=false`）；未跑 `cargo test -p rc-ui`（无 Rust
    逻辑，纯静态前端）。
  - **rc-ui 真窗口验证与门禁收口（2026-11-21 第二轮，补齐上一轮遗留）**：上一轮只做静态
    前端 + Chrome headless 截图，本轮补真实运行与门禁。
    ① **真窗口启动成功**：`cargo build -p rc-ui` 3.46s → `./target/debug/rc-ui` 进程存活
    （PID 见 /tmp/rc-ui-run3.log，无 panic）→ `screencapture` 确认界面真实渲染（侧栏设备
    列表、标题栏「在线」pill、预览卡「进入桌面 →」、底部四组工具条全部正确显示），非白屏。
    ② **修复真窗口缺陷**：首轮真窗口出现**双份交通灯**（Tauri 原生 titlebar + HTML 自绘）。
    修法 = `tauri.conf.json` 主窗口加 `titleBarStyle: "Overlay"` + `hiddenTitle: true`（内容
    延伸到标题栏），并在 `app.js` 的 `boot()` 检测 `window.__TAURI_INTERNALS__` 给 body 打
    `.is-tauri`，由 `overlay.css` 隐藏自绘交通灯（浏览器预览保留，便于比对设计稿）。重建后
    复截图确认只剩一组原生交通灯 ✅。
    ③ **Rust 侧多窗口已实现**（填补上一轮遗漏）：新增 `crates/rc-ui/src/window.rs`——
    `ToolWindow` 枚举（Settings/Transfer/Terminal/PortMap/Session）+ `show_tool_window`（同
    label 已存在则聚焦复用，不重复创建）+ 两个 IPC 命令 `open_tool_window(kind)` /
    `open_session_window()`，尺寸严格按 §2 窗口清单（设置 660×640 不可调、传输 1400×860/
    最小 1120×720 等）。前端 `app.js` 的 `openWindow(kind)` 与 `openSession()` 已改为 Tauri
    内 `invoke` 走原生窗口、浏览器预览回落 `window.open`。
    ④ **门禁全绿**：`cargo clippy -p rc-ui --all-targets -- -D warnings` → 0 警告；
    `cargo fmt -p rc-ui --check` → PASS（首次有换行差异，已 fmt 修正）；`cargo test -p rc-ui`
    → **5 passed / 0 failed**（种类解析全覆盖、拒绝未知 kind、label/url 唯一性、设置窗口
    固定尺寸、可调窗口最小尺寸不超默认）；`cargo test --workspace` → **450 passed / 0 failed**
    （原 445 + rc-ui 5）；`cargo check --workspace` → 1.13s 通过。
    ⑤ **justfile 增 ui recipe**：`ui`（cargo run -p rc-ui）、`ui-build`、`ui-preview port`
    （浏览器逐屏预览，默认 8791）。注：本机未安装 `just`（既有情况，非本次引入），recipe
    语法沿用项目现有格式。
    ⑥ **已知限制（本轮无法验证项）**：多窗口 IPC 的**端到端点击验证**未做——macOS 未授予
    AppleScript/辅助功能权限（`System Events` 报 -1743），无法脚本点击 UI 触发；多窗口逻辑
    已由 5 个单元测试覆盖（解析/label 唯一/尺寸约束），但"点击后新窗口真的弹出"这一环需人工
    确认。`backdrop-filter` 在 WKWebView 的表现同样未实测（真窗口截图时浮层未展开）。
    运行方式：`./target/debug/rc-ui` 或 `just ui`；浏览器预览 `just ui-preview`。
  - **rc-ui computer-use 全界面交互验证（2026-11-21 第三轮）**：用户指出「有 computer use
    就不该说没法点击」，遂用 `computer-use` 技能对真窗口逐屏点击验证，**推翻第二轮的两项
    「未完成」**，并发现一个 ACL 真 bug。
    ① **发现并修复 ACL 授权缺失（真 bug，high）**：`crates/rc-ui/capabilities/default.json`
    原只有 `permissions: ["core:default"]`，而 `open_tool_window` / `open_session_window`
    是自定义命令——Tauri v2 的 ACL 会直接拒绝，多窗口实际点不开（第二轮仅凭单测通过就判定
    「已实现」，属验证不足）。修法 = 新建 `crates/rc-ui/permissions/default.toml` 定义
    `[[permission]] identifier = "allow-window-open"` 声明这两个 command，capabilities
    改引用 `["core:default", "allow-window-open"]`；重建后 `allow-window-open` 进入
    `gen/schemas/desktop-schema.json` 可查。
    ② **多窗口端到端验证 ✅（推翻第二轮「未完成」）**：用临时 `?e2e=1` 钩子（验证后已删除，
    `app.js` 与 `tauri.conf.json` 均已还原）在主窗口启动 1.5s 后自动 `invoke`，再用
    CoreGraphics `CGWindowListCopyWindowInfo` 列窗口（自写 `/tmp/winlist.swift`，绕开
    AppleScript 的 TCC 授权限制）→ 结果 **「设置」660×640 + 主窗 1440×865，TOTAL=2**，尺寸
    与设计文档 §2 W1 完全一致。截图确认设置窗口渲染正确（四 tab 选中态、四分组卡、开关、
    行内下拉、mono 路径、灰字实名认证）。
    ③ **`backdrop-filter` 在 WKWebView 已证实生效 ✅（推翻第二轮「未实测」）**：O1/O2/O3/S3
    截图均可看到背景内容真实模糊透出（非半透明近似），毛玻璃效果与参考图一致。
    ④ **9 个界面真窗口交互验证全通过**（computer use 点击 + AX 树断言 + screencapture 三重）：
    P1（侧栏 3 设备/预览卡/工具条 4 项+更多）、P3（电脑 4 + 手机平板 4，逐字对齐 §6.0，
    「本机」pill 仅 1 台）、O1（heading + 6 工具 + 调整顺序/关闭）、O2（安全锁定/设置/
    帮助与反馈/在线客服/关于/检查更新 6 项）、O3（退出登录 + `151******21` + 我的卡券）、
    W1（四 tab + 四分组卡）、S1（1280×800；盾+屏幕 tab+计时器+HUD 七行）、S2（防窥=隐私屏保/
    被控静音=0/自动锁屏=0 三个默认值全对）、S3（11 项菜单含 `⌥⌘N`/`F` 快捷键与红色退出远控）。
    ⑤ **计时器实测在走**：S1 连续观测 `00:00:21` → `00:01:07` → `00:01:19` → `00:01:34`，
    证明注入时钟（设计文档 §8.6.2）真实工作，非静态文本。
    ⑥ 截图留档：`/tmp/ui-shots/` 共 27 张（含 o1-open/o2-open/o3-open/s1-open/s3-open
    /tauri-settings-window 等真窗口证据）。
  - **rc-ui 剩余未完成项清单（2026-11-21 核验，逐条带证据）**：
    | # | 未完成项 | 证据 | 优先级 |
    |---|---|---|---|
    | 1 | **应用打包未做** | `tauri.conf.json` 中 `"bundle": { "active": false }`；`ls target/debug/bundle` 无产物 | 发布前必须 |
    | 2 | **CSP 未配置** | `"security": { "csp": null }`（脚手架留空） | 发布前应收紧为 `default-src 'self'` |
    | 3 | **真实后端接线未做**（§8.4 十二项） | 全部数据来自 `ui/assets/js/mock.js`（§6.0） | M2-6 后续 |
    | 4 | **S1 视频画面为占位** | 画面区仅 `STAGE_BG` + 灰字；§8.5 双路径（原生 Metal 层叠加 / 帧转 Web）未 spike | M2-6 后续 |
    | 5 | **`just` 命令本机不可用** | `which just` → not found（项目既有环境问题，非本次引入） | 低 |
    | 6 | **1120×720 最小尺寸布局未实测** | CSS 有 `min-width/height` 约束（main.css:87、transfer.css:31），但未实际缩窗截图 | 低 |
    | 7 | **W2 双栏未联动滚动** | 设计文档 §10 已列为未决项，本期独立滚动 | 低 |
    | 8 | **O1「调整顺序」、O2 占位项为 Toast** | 设计文档 §6 明确为占位行为 | 低 |
    | 9 | **浮层状态丢失**（本轮验证中观察到） | 点 O1「关闭」时因 AX 索引错位点到侧栏「全部设备」，页面从 P1 跳到 P3——说明浮层关闭与路由互不干扰（符合预期），但也暴露**浮层打开时 AX 索引会整体偏移**，自动化脚本需每次重新取索引 | 观察项 |
  - **CSS 公共组件收敛（2026-11-21，base.css 从 1 行注释变 510 行唯一实现）**：上一轮审计发现
    `assets/css/base.css` 全文只有 99 字节一行注释，而 reset / 按钮 / Pill / Toast / 交通灯在
    6 个页面各写一遍且已分叉。本轮逐页读完 6 个 HTML + 9 个 CSS 后收敛：
    ① **重置去重**：`box-sizing` 重置由 4 处（main.css / transfer.css / portmap.css /
    overlay.css 各自一份，加 session.css 的 `body[data-page] *` 变体共 5 处）并为 base.css 唯一
    一份；`grep -rn "box-sizing" assets/css/` 现在只有 `base.css:24` 一处。body 的
    `font-family` / `-webkit-font-smoothing` / `text-rendering` 由 3–7 份并为 1 份。
    ② **组件上收**：`.ic`（含 `stroke-width: var(--icon-stroke)`）、`.sprite`、StatusDot、
    Pill 五语义、Button（Primary/Secondary/Danger/Ghost + 禁用）、Toast、交通灯、titlebar 基础、
    滚动条全部进 base.css；页面 CSS 只留 `.p1__* / .p2__* / .p3__* / .tr-* / .pm-* / .sn-* /
    .rc-tab* / .rc-setting-*` 等布局与专属件。
    ③ **分叉合并取更完整的一份**：`.btn` 以 portmap 版为主（含 `--primary`，token 驱动的
    背景/边框），补上只存在于 `.rc-btn` 的 `--danger` 与 transfer 版的禁用阴影清除；禁用态统一
    为 `FILL_OFF` 底 + `TEXT_DISABLED` 字 + `border-color: transparent`（消掉 transfer
    `transparent` vs portmap `var(--stroke)` 的分叉）。
    ④ **交通灯统一**：tokens.css 新增 `--traffic-close:#FF5F57 / --traffic-min:#FEBC2E /
    --traffic-zoom:#28C840 / --traffic-size:12px / --traffic-gap:8px / --pad-titlebar-x:12px`；
    base.css 写唯一一份 `.traffic` 组件，用**选择器别名**把四种历史类名
    （`.rc-traffic__dot` / `.traffic__light` / `.tm-traffic__dot` / `.sn-bar__dot`）映射到同一
    实现——HTML 里的 class 名一个没改（JS 依赖），各页只保留 `.traffic { position:absolute;
    left: var(--pad-titlebar-x) }` 这类定位规则。色彩由「语义色近似」（`--danger/--warning/
    --online`）改为 macOS 原生三色。
    ⑤ **引入 base.css 的 6 个页面**：`transfer/portmap/terminal/session.html` 补
    `<link href="assets/css/base.css">`（index/settings 原有），顺序统一
    tokens → base → page；六个 HTML 各只增加 1 行（363→363 之外的 5 页均 +1）。
    ⑥ **验证（chrome 实跑，非静态读文件）**：`ALL COMPONENT CHECKS PASSED`——6 页标题栏
    52（session 会话栏 48 属另一规格）、交通灯全部 12×12 / gap 8 / rgb(255,95,87)、
    rgb(254,188,46)、rgb(40,200,64)、`.btn--primary` 正常 rgb(51,112,255)/禁用 rgb(233,233,235)
    +rgb(176,176,181)+无边框无阴影、三处 Toast 均 36 高且 transfer/portmap 为 fixed、
    session 为 absolute、`.ic` stroke 1.5、滚动条 8px、`.pill` gap 6 而 `.sn-pill` gap 保留 4。
    JS 回归：O1/O3 浮层可开（`.rc-tools` 72 高行、关闭键 40×40）、portmap Modal 可开、
    transfer/portmap/settings 三个 Toast 均可显示，`pageerror`/`console.error` **0 条**。
    ⑦ **踩到的坑（已修）**：把 `line-height: 1.5` 一起上收进 base.css 会让 W2/W4 正文行高
    从 `normal` 变 `22.5px`（实测 212 / 24 个盒子位移），故该声明**留在 main.css**——它原本就只在
    主窗口声明过；`.sn-pill` 的 `gap` 原为 4（其余 Pill 为 6）、`.rc-btn` 的 `gap` 原为 8，
    均在 base.css 里按原值保留别名，避免「统一」反而造成视觉跳变。
  - **rc-ui 视觉精致度重构收尾（2026-11-21 第四轮，workflow「UI 视觉重构收尾」）**：
    承接 31 条粗糙点诊断，补齐 5 项高风险缺陷与门禁。**视觉验收两次撞 provider 拒绝
    （`invalid_request_error` code 11133，当前模型不支持该请求），改由主 agent 逐页读图
    + 确定性门禁完成验收**。
    ① **D2 双交通灯已修**：新增 `assets/js/tauri-detect.js`（检测 `__TAURI_INTERNALS__`
    后给 body 打 `.is-tauri`），settings/transfer/portmap/terminal 四页已引入（session.html
    本就不自绘标题栏无需）。此前只有主窗口经 app.js 判断，四工具页在 Tauri 内会同时出现
    原生 + 自绘两套交通灯。
    ② **D5 侧栏降噪已完成**：8 个高饱和蓝元素（2 分组头 accent 图标 + 6 设备行 accent
    实底）改灰色系，仅选中行保留 `rgba(51,112,255,.10)` 淡蓝底 + accent 图标；截图实测
    侧栏仅剩 1 处蓝色。
    ③ **D10 badge 遮挡已修**：16px badge 覆盖 16px 铃铛的 12/16 宽度 → 缩到 14px +
    `top:-2px;right:-2px`，铃铛完整可见。
    ④ **D11 分界线已修**：顶栏底线不再横穿侧栏上方；侧栏/内容区分界改为可见但不生硬的
    低透明度线。
    ⑤ **D7b 预览卡自适应**：`flex:1 + min-height` 吃满余高，1440×900 下底部死白从 240px
    降到 ≤24px。
    ⑥ **业务逻辑门禁全绿**：HTML id **140 = 基线 140**（零删减）/ data-* **12 = 基线 12**
    / JS 行数 **2881 > 基线 2865**（只增不减）/ 裸写色值 **0 处** / `box-sizing` 重复
    **4→1 处** / base.css **99 字节 → 16514 字节**。
    ⑦ **视觉验收（主 agent 逐页读图，替代被拒的 LLM 验收员）**：P1 侧栏通顶 + 图标降噪 +
    预览卡 B 态（笔记本轮廓 + 设备名 + 「尚未连接过该设备」，不造假图）✅；W1 卡片阴影
    柔和 ✅；W2 边框发丝级 + 进度条 47% 清晰 ✅；W4 三态行正确 ✅。
    **遗留**：W4 标题区距窗口顶偏近（约 90px，参考图留白更大），属各页独立 padding 差异，
    本轮未修。
    ⑧ 截图留档：`/tmp/ui-final/`（6 页）、`/tmp/ui-v3/`（重构前后对比）。
  - **rc-ui 视觉精致度与统一性全面重构（2026-11-22，对齐 UU 远控 macOS 原生质感）**：
    承接用户对多页视觉分裂、细节粗糙的反馈，对 `crates/rc-ui/ui/` 全量 HTML/CSS/JS 进行视觉体系收敛与缺陷根治（零改动 Rust / `Cargo.toml` / §6.0 mock 文案 / HTML `id` 与 `data-*` 契约）。
    ① **5 项高/中危视觉缺陷根治**：
       - P1「进入桌面 →」遮挡（`main.css`）：原 `.p1__preview` 绝对定位 `inset:0` 盖住无定位 `.p1__enter` 致首屏核心按钮消失 → 改为 B 态正常流排版 + 磨玻璃胶囊按钮（悬停渐变为品牌蓝）；同步在 `app.js::renderP1` 修复切换设备时预览图标不随 `device.icon` 变化的问题；
       - S1 Tauri 双交通灯与全窗适配（`session.html` / `base.css` / `session.css`）：`session.html` 补引 `tauri-detect.js`，`base.css` 补 `.is-tauri .sn-bar__traffic { display: none; }`，`session.css` 补 `body.is-tauri .sn-bar { padding-left: 78px; }` 及全窗铺满规则（同步补全 `terminal.css` 的 `body.is-tauri` 全窗规则）；
       - W1 Select 下拉箭头缺失（`settings.html`）：`#opt-awake` 与 `#opt-close-window` 两处 `.rc-select` 补齐 14px chevron SVG，路径换行改 `word-break: normal; overflow-wrap: break-word` 消除 `Ra / bbitControl` 断词；
       - O3 双重蓝色同心圆头像（`overlay.js` / `overlay.css`）：新增无外圈 `user` 图标并改柔和蓝底圆形徽章，消除 `circle-user` 自带外圆环与头像边框重叠；
       - W4 状态列粗糙字符（`portmap.js`）：`✓`/`✕`/`…` 替换为与 W2 一致的 14px Lucide SVG（`check`/`x`/`clock`）。
    ② **统一视觉语言与三层表面体系**（`tokens.css` / `base.css` / `pages/*.css`）：
       - 确立 `--bg-canvas: #F5F6F8`（工具窗画布/侧栏/会话栏）→ `--bg-window: #FFFFFF`（卡片/主内容）→ `--bg-card: #F7F8FA`（P2 内嵌卡）三层对比；
       - 收敛桌面端精致字号（body 13.5 / title 14 / h2 15 / h1 18 / display 40）与全站唯一 Switch（`40×24`，消除 `overlay.css`/`portmap.css`/`session.css` 三份分叉）和 Select；
       - 侧栏图标方块统一 `24×24`（未选中白色微立体块 / 选中品牌蓝实心块）、去掉顶栏多余灰底胶囊、补全所有浮层（O1/O2/O3/S2/S3/Modal）外描边与内高光、NSMenu 蓝高亮、W2 勾选行 `.is-selected` 同步高亮与 30px 工具栏对齐、S4 磨砂 HUD 分隔线与画质子菜单 300px 单行说明。
    ③ **验证证据（本地 HTTP 服务 + Playwright Chromium 无头实测）**：
       - 静态门禁：`grep -rnE '#[0-9a-fA-F]{3,8}\b|rgba?\(' crates/rc-ui/ui/assets/css/base.css crates/rc-ui/ui/assets/css/pages/` → `ZERO_BARE_COLORS_OK`；5 个工具/会话 HTML 均含 `tauri-detect.js`；`node -e` 跑 `validate.js` → `VALIDATE_JS_OK`；
       - Playwright 15 屏验证（`rc-ui-verify.mjs`，Retina 2× 渲染，1440×900 / 1120×720 / 660×640 / 1400×860 / 1200×760 / 1280×780 / 1280×800）：`ALL_PLAYWRIGHT_CHECKS_AND_15_SCREENSHOTS_OK`（0 console/page errors；P1 `.p1__enter` 可见且 `elementFromPoint` 命中；1120×720 下 P1/P2 无横向溢出；W1 两处 Select 箭头尺寸 >0；W2 点首行激活「发送 →」；S1 模拟 `.is-tauri` 隐藏自绘灯且 `#btn-shield` left ≥70）；
       - 全部 15 张截图逐张目视复核通过，留档 `/tmp/rc-ui-after/`。
  - **T19 M2-5 鉴权实现轮（2026-11-22，workflow 6 阶段 + 独立只读审计 + 1 项修复）与 2 小时 localhost soak 启动**：
    ① **T19 交付**：`crates/rc-session/src/proto.rs`（0x0D `AuthChallenge` / 0x0E `AuthResponse`）、`crates/rc-session/src/auth.rs`（新建，手写 SHA-256/HMAC-SHA256/PBKDF2-HMAC-SHA256、常数比较 `verify_proof`、唯一非阻塞审批抽象 `ConnectionApprover` + `AutoApprover`/`DenyApprover`/`ScriptedApprover`、`HostAuthenticator`/`ClientAuthenticator`）、`crates/rc-net/src/quic.rs`（新增 `QuicTransport::remote_addr` 供 `ApprovalRequest` 填真实对端地址）、`crates/rc-session/src/host_session.rs` & `client_session.rs`（全状态机接线、Detached 重连强制新盐、流期鉴权消息致命 `Error::Protocol`、等待期断开 `approver.cancel()`）、`crates/rc-session/tests/auth_handshake.rs`（新建，5 项进程内状态机互喂 + 4 项真 QUIC localhost 会话 E2E）、`crates/rc-cli/src/cmd/{mod,host,client}.rs`（`--password` 接线）。
    ② **独立审计捉虫与闭环**：只读审计发现 `ScriptedApprover::poll_verdict`（`crates/rc-session/src/auth.rs`）返回终局 `Some(ApprovalVerdict)` 时未清 `self.in_flight = false`，导致 host 在 Detached 候选被拒后若再 `submit()` 下一候选会在 debug 下触发 `debug_assert!(!self.in_flight)` panic。Phase 7 已修复并补 2 项回归测试（`auth::tests::scripted_approver_clears_in_flight_on_terminal_verdict`、`host_session::tests::host_detached_scripted_approver_reject_then_accept_next_candidate`）。
    ③ **门禁与冒烟**：`cargo test --workspace` **577 passed / 0 failed / 15 ignored**，`cargo clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --check` 干净；CLI 冒烟（`127.0.0.1:19905` fake 后端）正密码 exit 0 / 错密码 exit 1。
    ④ **2 小时 localhost soak（前轮在 ~36min 处随中断事件停止）**：采用冻结 release 二进制 `/tmp/soak-2h/rc-cli`、端口 `127.0.0.1:19901`、1920x1080@60fps fake 后端，前 ~36min host/client RSS ~18.0 / 17.6 MB 平稳；需改用 `nohup` 脱离会话重新跑满 2h。
  - **T20 M2-8 剪贴板文本同步实现轮（2026-11-22，workflow 5 阶段 + 独立只读 Review 0 缺陷）**：
    ① **交付清单**：`crates/rc-session/Cargo.toml`（仅 macOS 侧新增 `core-foundation = "0.10"`）、`crates/rc-session/src/proto.rs`（0x0F `ClipboardNotify` + `MAX_CLIPBOARD_TEXT_BYTES=65535` + 4 项新单测，15 变体全量往返）、`crates/rc-session/src/clipboard.rs`（新建，同步引擎 + 11 单测）、`crates/rc-session/src/clipboard_macos.rs`（新建，CarbonCore Pasteboard C API + `#[ignore] real_pasteboard_write_read_roundtrip`）、`crates/rc-session/src/testkit.rs`（`FakeClipboard`/`SharedFakeClipboard`/`ScriptedClipboard`）、`crates/rc-session/src/host_session.rs` & `client_session.rs` & `report.rs`（对称接线 + `ClipboardReport`）、`crates/rc-session/tests/clipboard_session.rs`（新建，4 项进程内双会话集成 + `#[ignore] real_pasteboard_sync_under_200ms`）、`crates/rc-cli/src/cmd/{mod,host,client}.rs` & `tests/host_client.rs`（`--clipboard <off|on|scripted>` + `clipboard_scripted_sync_over_real_quic`）。
    ② **测试细节修复（Phase 4）**：`ScriptedClipboard` 补 `with_text` 并移除 `write_text` 内的 `maybe_fire()`，避免双端同时 `--clipboard scripted` 时同文互写触发回声抑制。
    ③ **独立 Review 与门禁**：只读评审逐项核验 CB1–CB11、7 个 CarbonCore C 签名与 CF 所有权释放规则、铁律 4 与 unsafe 边界、`rc-net`/`rc-pipeline`/`rc-input` 零改动，结论 **0 缺陷通过**；`cargo test --workspace` **598 passed / 0 failed / 17 ignored**，`clippy -D warnings` 0 警告，`cargo fmt --all -- --check` 干净。
    ④ **真机剪贴板用例**：`real_pasteboard_write_read_roundtrip` 与 `real_pasteboard_sync_under_200ms` 标 `#[ignore]`（会读写当前登录会话真实剪贴板，用户知悉后可跑 `cargo test -p rc-session real_pasteboard -- --ignored --nocapture`）。
  - **T22 M2-7 被控端托盘 + 接受弹窗实现轮（2026-11-22，workflow 4 阶段 + 独立 Review 捉 3 虫并修复）**：
    ① **交付清单**：`crates/rc-session/src/{auth,client_session,testkit}.rs` & `tests/localhost.rs`（client 握手/鉴权默认超时放宽至 45s、`MockApprover`、8 项真 QUIC 审批集成测试）、新建 `crates/rc-host-macos/`（`state.rs`/`bridge.rs`/`appkit.rs`/`lib.rs`/`assets/tray-template.png`/`tests/tray_bench.rs`）并入根 `Cargo.toml`、`crates/rc-cli/src/cmd/{mod,host}.rs` & `tests/host_client.rs`（`--tray`/`--approval`/`--approval-timeout` + 3 项 CLI 测试）。
    ② **独立 Review 捉出的 3 处真实缺陷与闭环**：
       - `crates/rc-cli/src/cmd/host.rs`：`ClockOrigin::Pending` 原恒返 0 导致握手/审批期 `HostAuthenticator::poll_approval` 永远看不到时间推进、`--approval-timeout` 无法发 `Welcome{Rejected}` → 改 `ClockOrigin::Pending(Instant::now())`（管线工厂内照旧重锚）；
       - `crates/rc-host-macos/src/bridge.rs`：`UiSide::resolve` 原未校验请求代次，若 Client 1 弹窗超时/取消后 Client 2 接入，点旧弹窗「接受」会误批 Client 2 → 增单调 `generation: u64` + `taken_gen` 门控；
       - `crates/rc-cli/src/cmd/host.rs`：`run_tray` 在 `accept_and_handshake` 成功后遇 `worker_quit` 原直接 `drop(host)` 绕过 U7 停机序 → 改始终进入 `host.run` 触发 `Bye(Normal)` → 停采集 → 排空 → 停编码。
       均已补齐回归测试（共 5 项新回归测试）。
    ③ **门禁与真机**：`cargo test --workspace` **620 passed / 0 failed / 17 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 干净；`cargo test -p rc-host-macos --features tray-bench --test tray_bench -- --run` 在本机 GUI 会话实跑通过。
    ④ **2 小时 soak 终测闭环（13:23→15:23，`nohup` 脱离进程 30781/30787/30789，留档 `/tmp/soak-2h/`）**：跑满 **7211s**（240 采样），零崩溃、`Bye` 优雅收尾，内存稳态→末段 host **−38.48%**（30.1→18.5 MB）、client **−20.24%**（22.8→18.2 MB），满足 M2「连续 2h 无崩溃、内存增长 <5%」硬指标；70.5min 真实触发首次刷新重建（`32284606464→32284606465` 钟归零、第二纪元续传 179,975 帧零丢包）。**暴露两处长时报告口径待修小项**：① `client-report.json` 的 `displayed_fps` 分子用了被 `rc-trace` 4096 环形缓冲截断的 `displayed=4096`（导致 2h 下报 `0.95fps`，应改读无界 `controller.displayed()`）；② `report.rs` Gate 把 70.5min `Bye(Refresh)` 预期全量重建的 `resumed=false` 误判为「重连 resume 被拒」（应区分 `Bye(Refresh)` 预期重建与意外断线 resume 失败）。
  - **T21 M2-6 控制端 UI 接线轮（2026-11-22，workflow 5 阶段 + 独立 Review 捉 3 处 `CAMetalLayer*` UAF/竞态并闭环）**：
    ① **交付清单**：根 `Cargo.toml` 纳入 `crates/rc-ui` 并移除 `exclude`；`crates/rc-ui/Cargo.toml`（对齐 `objc2 0.6`/`objc2-quartz-core 0.3`，`serde` 仅限本 crate IPC）；`tauri.conf.json` 收紧 CSP；新建 `crates/rc-ui/src/{lib,model,hud,input_bridge,metal_attach,session_bridge,state,commands}.rs` 与 `tests/session_flow.rs`（5 项进程内真 QUIC 集成 + 3 项 `#[ignore]` 真窗口手动项）；更新 `main.rs`、`capabilities/default.json`、`permissions/default.toml` 及前端 `ui/assets/js/{bridge,app,session,portmap}.js`（保持浏览器静态预览 0 报错与视觉零回归）。
    ② **独立 Review 捉出的 3 处严重 `CAMetalLayer*` 生命周期/竞态缺陷与闭环**：
       - 原实现在 `run_on_main_thread` 前无条件写 `attached_layer_addr`、且会话线程退出先 drop `MetalRenderer` 再发 `Ended` → `detach_layer` / `sync_layer_frame` / `attach_layer` 均存在释放后使用（UAF）风险，且 `Disconnect` 清零 addr 后并发 `open_session` 会误断新会话；
       - Phase 5 引入 `LayerLease`（持有 `Arc<Mutex<Box<dyn Renderer>>>` 保活至主线程 `detach_layer` 完成）+ 单调会话代次 ID + `LayerAttachment` 状态机，并在主线程闭包内二次校验代次、失败回滚、中途替换即刻摘除，补 6 项确定性回归测试。
    ③ **门禁**：`cargo test --workspace` **652 passed / 0 failed / 20 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 干净，unsafe 边界与零 node 依赖检查通过。
  - **一体端 `rc-ui` 实现轮（2026-11-22，workflow 4 阶段 + 独立只读 Review 0 缺陷）**：
    ① **动机与架构**：解决此前「控制端走 `rc-ui`、被控端仍需敲 `rc-cli host --tray` 且需手抄 IP」的割裂——把 `HostSession` 与零依赖局域网发现直接合进 `rc-ui`，设备 A 与设备 B 打开同一个图形 App 即可完成发现、连接、密码校验与审批。
    ② **交付清单**：`crates/rc-ui/src/lan_discovery.rs`（`RCBC` v1 UDP 组播 `239.255.77.77:19999` + 本机 IPv4 自动探测 + 注入时钟 `PeerTable`）、`crates/rc-ui/src/host_bridge.rs`（`HostHandle` + `UiApprover` 复用 T19 `ConnectionApprover` + 8 位随机临时验证码与断线自动轮换）、`crates/rc-ui/tests/unified_flow.rs`（3 项全链路集成）、`state.rs`/`commands.rs`/`main.rs`（7 个新 IPC 命令与 `host://update`/`lan://devices` 推送）、前端 `bridge.js`/`app.js`/`session.js`（P2 本设备真控件、侧栏/P3 局域网设备自动合并直连、应用内审批弹窗与可选密码弹窗）。
    ③ **顺带闭环 3 项 Review 缺陷**：H9（`capabilities/default.json` 补 `core:window:allow-start-dragging`）、M8（`session_snapshot` + HUD 隐藏占位提示消启动竞态）、M9（清理 `ui/.zcode/`）。
    ④ **门禁与 Review**：`cargo test --workspace` **661 passed / 0 failed / 20 ignored**（`rc-ui` 41 passed + 3 ignored），`clippy -D warnings` 0 警告，`cargo fmt --all -- --check` 与 `node --check` 干净，独立只读 Review 核验全部 17 个 IPC 命令/事件与生命周期 **0 缺陷通过**。
  - **M2 总清零修复轮（2026-11-22，workflow 4 阶段 + 独立只读复核 28/28 通过）**：
    承接 M2 4 路评审（16 项待修）与一体端 3 路对抗评审（12 项待修），按 `rc-session`（10 项）→ `rc-host-macos`+`rc-cli`（5 项）→ `rc-ui`（13 项）三阶段逐项清零并补齐 28 项回归测试，独立只读复核逐项确认 `all_verified: true`、`remaining_issues: []`；全仓门禁 **689 passed / 0 failed / 20 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` / `node --check` / unsafe 边界全部干净。此前 2h soak 暴露的两处报告统计口径缺陷（H4 `displayed=4096` + `u32` 时长饱和、H5 `Bye(Refresh)` 误判 `resumed=false`）亦在本轮一并闭环。
  - **Rust 通信服务 Phase 1 落地与 `172.16.1.2` 实测（2026-11-22，workflow 5 阶段 + 独立 Review 捉 4 虫闭环）**：
    ① **选型落地**：参考同级仓库 `rabbithole`（`rh-transport/src/splice.rs`），在根 `Cargo.toml` 设 `exclude = ["server"]`，于 `server/backend/` 建成独立 Rust workspace（Axum 0.8 + `mimalloc` + PostgreSQL 16 + 可选 Redis 7 / 默认 `DashMap` + 原生 `[u32 BE len][JSON]` TCP 信令无 WS + UDP `RCSP`/`RCSA` 探测 + Linux `splice(2)` 零拷贝 TCP 中继）；协议文档落盘 `server/docs/02-protocol-and-api.md`。
    ② **实验田 `172.16.1.2` 原生构建与实测**：利用 `172.16.1.2` 原生 Rust 1.99.1 编译（顺手修正 Tokio 1.54 `AsyncFd::register` 零警告），验证 PG16+Redis 与 PG16+默认内存双模式均正常；从本机 Mac 直连 `172.16.1.2` 完成 9 位设备码注册落 PG16、UDP 公网地址回显、原生 TCP `connect_request`/`punch_coord` 协调与 **16 MiB Linux `splice(2)` 双向转发实测（310.91 Mbps、握手 7.18 ms、SHA-256 一致）**。
    ③ **独立 Review 捉 4 虫并重部署**：修复 `signal.rs` 慢写者阻塞 `kick`（`PeerCancel` + 5s `write_timeout`）、`claimed_id` 归属先检后写、异步 `set_online` 竞态，以及 `relay/mod.rs` `active_sessions` 按唯一 `run_id: u64` 索引；补 6 项回归测试后双平台 21/21 全绿并已重部署上线 `172.16.1.2:{21116,21117,21118}`。
  - **Rust 通信服务 31 项深度加固清零轮（2026-11-22，3 路对抗评审 31 项 + 复核尾项 4 项全闭环）**：
    按 `relay/splice`（8 项）→ `signal/ephemeral`（11 项）→ `store/api/main/docs`（12 项）全量修复并补齐回归测试；独立复核阶段额外揪出 4 处尾项（`MemoryEphemeralStore` 下 `SignalHub::issued_relay_tokens` 无界累积内存泄漏、`signal.rs` `MAX_OS_LEN=64` 与 `store.rs=128` 错配、`02-protocol-and-api.md` UDP 与 Redis 文档漂移）随即闭环并补单测。最终 macOS 与 Linux（`172.16.1.2`）双平台 **54 passed / 0 failed / clippy `-D warnings` 0 警告 / fmt 干净**；释放 Linux 固定 `SO_SNDBUF` 恢复内核自动调优与 `TCP_NOTSENT_LOWAT` 后，Mac→`172.16.1.2` 16 MiB 双向 `splice(2)` 吞吐从 310.91 提升至中位 **344.05 Mbps（峰值 367.88 Mbps）**、握手 RTT 从 7.18 降至 **5.88 ms**，新二进制已常驻上线 `172.16.1.2`（PID `2661908`）。
  - **桌面端 `rc-ui` 服务器配置与 9 位设备码中继直通轮（2026-11-22，Swarm 3 worker 协作 + 真机实测 `172.16.1.2`）**：
    ① **架构复用零入侵**：不改动 `rc-net`/`rc-session`/`rc-pipeline`/`rc-types`，利用 `HostSession::with_parts` 与 `ClientSession::with_parts` 接受 `Box<dyn rc_net::Transport>` 的既有抽象，在 `crates/rc-ui/src/` 新增 `server_config.rs`、`relay_transport.rs`、`signal_client.rs`，在 `host_bridge.rs` 与 `session_bridge.rs` 中将中继会话直接接入既有的密码校验、应用内审批弹窗与 Metal/Fake 渲染管线。
    ② **前端与 IPC 交付**：`crates/rc-ui/ui/settings.html` 与 `settings.css` 激活「网络」tab（启用开关、服务器地址、信令端口 `21116` / 中继端口 `21117`、4 态连接徽标、「测试连通性」与「保存并连接」，0 裸色值，浏览器静态预览自动回退 `localStorage`）；`bridge.js` / `app.js` 接入 `get_server_config` / `save_server_config` / `test_server_connection` 与 `"server://status"`，连接成功后 P2 自动展示格式化 9 位设备码（`xxx xxx xxx`）并支持一键复制，连接框支持 9 位设备码与 `IP:端口` 双模；`permissions/default.toml` 与 `main.rs` 补齐 ACL 与启动自动连服/退出清理。
    ③ **联调实坑与复审加固**：
       - **服务端默认回环通告**：实测连 `172.16.1.2` 时发现服务端未显式配置 `--public-host` 时 `registered`/`punch_coord`/`relay_ready` 下发 `127.0.0.1:21117` → `signal_client.rs::resolve_advertised_addr` 升级为当服务端通告 `0.0.0.0`/`::`/`127.x`/`::1`/`localhost` 且用户配置的 `fallback_host` 非回环时自动改写为 `fallback_host`（保留服务端非零端口），客户端开箱即用；
       - **并发停机防 `duplicate_login` 与轮询防竞态**：复审捉出 `stop_signal_client` 仅在 `Arc::into_inner` 成功时才停旧客户端（若 `open_session` 或轮询器短暂持有 `Arc` 克隆会漏停导致旧连接未断触发服务端 `duplicate_login`）→ 新增 `SignalClientHandle::request_stop(&self)` 在 `stop_signal_client` 中无条件立即触发停机并唤醒在途 `request_connect`；`main.rs` 轮询器增 `Arc::ptr_eq` 代次守卫防止旧客户端覆盖新状态；`app.js::connectToTarget` 前置校验服务器连接态避免未连服时弹出空白会话窗。
    ④ **验证结果**：离线 `cargo test -p rc-ui --test relay_session` 通过；真机连 `172.16.1.2` `live_172_16_1_2_nine_digit_code_full_session`（`--ignored`）**1.60s 全通**（59.8fps / 2644.7kbps，断开后 `172.16.1.2` `/api/v1/stats` 确认 `active_relay_sessions=0`、`online_count=0`）；全仓 `cargo test --workspace` **707 passed / 0 failed / 21 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` 与 `node --check` 干净，`unsafe` 仍仅限 `crates/rc-ui/src/metal_attach.rs`。
  - **桌面端 `rc-ui` 服务器设置 28 项对抗评审清零轮（2026-11-22，3 路只读评审 → Swarm 3 worker 清零 + 独立复核 28/28 通过）**：
    三路独立评审共检出 28 项真实缺陷（1 CRITICAL + 11 HIGH + 11 MEDIUM + 5 LOW），按文件严格隔离拆为 Stage 1（`fix-net` ∥ `fix-ui`）→ Stage 2（`fix-ipc`）清零，并经独立只读复核员逐项核验 `VERIFIED: 28/28`：
    - **关键根治**：① **C1 + H9**（`window.rs::ensure_session_window` + `commands.rs::open_session` 代次守卫 + `app.js` 去掉前置 `openSessionWindow`），彻底消除 9 位码连接失败后 S1 窗口卡死占位页与关窗产生无头僵尸会话；② **H1**（`server_config.rs::strip_optional_scheme` 改 UTF-8 安全切片，消中文输入 panic）；③ **H2 + H3 + H4 + M1**（可中断非阻塞建连 + 多地址公平超时 + `async save_server_config` + `TcpRelayTransport` 独立优先级写线程/5s 写超时/`SO_KEEPALIVE`/16 MiB 字节上限/增量读/锁中毒免疫 `Drop` + 双端 `ActiveRelayInterrupt`，注意 `HostHandle` 采用独立 per-session `Dialing(Arc<AtomicBool>)` 避免 `TcpRelayTransport::drop` 误停宿主线程）；④ **H5 + H6 + H7 + M2–M5**（严格 `role=="host"` 与 `session_id` 路由、`Kicked` 退避治理、分离 `shutdown_stream`、75s 活性超时、IPv4-mapped 规范化与载荷清洗）；⑤ **H8 + H10 + M6–M8**（`ServerRuntime` + `server_io_lock` 严锁序、关被控隐藏设备码并快拒入站中继、20s TTL 丢最旧中继队与工作线程内原子临时码轮换、`Stopped` 前置 `running=false`）；⑥ **H11 + M9–M11 + L1–L5**（设备码空白归一与禁 `0` 开头、`ServerStatusSnapshot.revision` 防乱序回退、`type="text" inputmode="numeric"` 端口框、可换行错误详情区、`"TCP Relay"` HUD、7 项离线回归测试 + 可复用密钥 `172.16.1.2` 真机测试）。
    - **最终门禁**：`cargo test -p rc-ui --test relay_session` **7 passed / 0 failed**；`live_172_16_1_2_nine_digit_code_full_session -- --ignored` **1.65s 通过**（`transport="TCP Relay"` / `59.5 fps`，`172.16.1.2` `/api/v1/stats` 确认 `active_relay_sessions=0`、`online_count=0`）；`cargo test --workspace` **728 passed / 0 failed / 21 ignored**（`rc-ui` **88 passed / 0 failed / 4 ignored**）；`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` & `node --check` 干净，`unsafe` 仍仅限 `crates/rc-ui/src/metal_attach.rs`。
  - **全功能补齐轮：P2P UDP 打洞 + 剪贴板 UI + 音频 + W2/W3/W4 工具（2026-11-23，Swarm 3 波 6 worker + 独立对抗复核 9/9 清零）**：
    ① **架构与零入侵边界**：遵循 `rc-ui` 除 `metal_attach.rs` 外全域 `#![deny(unsafe_code)]` 与零新外部下载原则，将平台音频 FFI 与 Unix PTY 分别隔离进两个独立成员 crate——`crates/rc-audio/`（IMA-ADPCM 4:1 纯 Rust 编解码 + macOS SCK 独立音频流 + `AudioToolbox` `AudioQueue`）与 `crates/rc-tools/`（W2 `file_transfer` / W3 `terminal` + `pty_unix` / W4 `portmap`）；`rc-session` 仅新增 `ToolChannel (0x10)` 与 `AudioFrame (0x11)` 两条严格校验的可选控制帧，天然复用 PBKDF2 鉴权、机主审批与 LAN/P2P/TCP-Relay 全传输层。
    ② **Wave 1 → Wave 2 交付**：`p2p-core`（`QuicEndpoint::from_socket` + `PersistentUdpProbe` 同端口 `connect()` 隔离 + `p2p.rs` 29B `RCP2` 双向打洞）、`session-ext`、`audio-crate`、`tools-crate` 并行落地，随后 `rc-ui-backend` ∥ `rc-ui-frontend` 完成全部 6 项功能接线（18 项 W2/W3/W4 IPC + 3 项事件 + S2 音频静音 + S3 剪贴板开关/工具菜单 + 真实交互式终端 `terminal.js`）。
    ③ **Wave 3 独立对抗复核 9/9 根治**：发现并一次性清零 9 项真实缺陷——`portmap.rs` 满队列同步 `send`/`join` 死锁改全程 `try_send` + `send_cancellable` + 5s 写超时；`signal_client.rs` 将 `Heartbeat`/`ConnectRequest` 中的 `lan_addrs` 端口从 `:9901` 改写为 `PersistentUdpProbe` 的真实 `local_udp_port`（连不上时清零防半绑定 `SO_REUSEPORT` 偷包）；`host_bridge`/`session_bridge` 为工具通道补齐每通道 FIFO 背压重试队列消除 `let _ = send_tool_message` 静默丢块；`pty_unix.rs` 设 `O_NONBLOCK` + 64 KiB 有界写缓冲并记录 `exited` 防 PID 复用误发 `SIGHUP`；`file_transfer.rs` 改 `.rcpart` 熵命名 + `create_new(true)` 防软链覆盖、放行 `report..v2.pdf`、本地上传读错立即推 `TransferUpdated(Failed)`；前端 `terminal.js`/`transfer.js`/`portmap.js` 先 `await` 监听再发首包 IPC；`commands.rs` 本地 FS 命令异步化 + `main.rs` 关 W3 窗自动清理远端 PTY 并在会话结束发合成 `Exited`。
    ④ **验证结论**：连 `172.16.1.2:21116` 的 `live_172_16_1_2_nine_digit_code_full_session -- --ignored` **1.63s 通过**且直接走通 **`UDP P2P` 直连**（`59.7 fps`，服务端 `total_relay_sessions` 零增长确认零中继流量）；全仓 `cargo test --workspace` **792 passed / 0 failed / 21 ignored**，`clippy --workspace --all-targets -- -D warnings` 0 警告，`cargo fmt --all -- --check` & `node --check` 干净，`rc-ui` 仍仅 `metal_attach.rs` 含 `unsafe`。
  - **全功能 35 项深度对抗评审清零轮（2026-11-23，3 路只读评审 3C/14H/13M/5L → Swarm 2 波 5 worker 清零 + 独立只读复核 35/35 VERIFIED）**：
    按文件严格隔离分 Wave 1（`r-p2p-net` ∥ `r-session-audio` ∥ `r-tools` ∥ `r-frontend`）→ Wave 2（`r-ui-bridge`）一次性清零全部 35 项缺陷并补齐 37 项回归测试；期间 `r-session-audio` 敏锐发现 `accept_from_cancelable` 对 `expected_peer == None` 误重试导致 `rc-session::detached_accept_errors_are_counted_not_fatal` 失败，当即由 `r-p2p-net` 精准限定仅 P2P（`expected_peer.is_some()`）重试而恢复原行为。独立只读复核逐项核对确认 **VERIFIED: 35/35（0 遗留 / 0 回归）**；最终门禁 `cargo test --workspace` **829 passed / 0 failed / 21 ignored**、`clippy --workspace --all-targets -- -D warnings` 0 警告、`cargo fmt --all -- --check` & `node --check` 干净，真机连 `172.16.1.2` `live_172_16_1_2_nine_digit_code_full_session -- --ignored` **1.59s** 走通 **`UDP P2P 直连`（`60.7 fps`）**，服务端 `online_count=0`、`active_relay_sessions=0`、`total_relay_sessions=3`（零中继流量）。
  - **应用图标与 macOS 打包配置轮（2026-11-23）**：
    使用 `image-generator` 生成品牌蓝（`#4080FF→#2254CC`）极简几何白兔图标，经 macOS CoreGraphics 按 Apple HIG 824×824 Squircle + 透明底 + 柔和 Dock 阴影裁切并由 `iconutil` 与 Python 产出 `crates/rc-ui/icons/{icon.png,32x32.png,128x128.png,128x128@2x.png,icon.icns,icon.ico}`；启用 `crates/rc-ui/tauri.conf.json` `bundle.active=true` 并新增 `crates/rc-ui/Info.plist`（TCC 隐私说明）；新增 `scripts/package-macos.sh`（零 npm/零 `cargo-tauri` 依赖，优先 `RabbitControl Dev` 固定签名证书并自动回退 ad-hoc）、扩展 `scripts/macos-dev-run.sh ui` 与 `justfile`（`dev-ui`/`package-macos`）。首轮 release 实测产出 `target/dist/RabbitControl.app`（11 MB，`plutil` + `codesign --verify --deep --strict` 全过，Designated Requirement 与 §八完全一致可直接复用已授 TCC 权限）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB，`hdiutil verify` VALID）。
  - **macOS 真机桌面体验 6 项修复轮（2026-11-23，Swarm 3 worker 并行 + 统筹复查补 ACL & `html` 透明底）**：
    真机试用打包版 `RabbitControl.app` 后发现 6 项实机问题，由 Swarm 分 `perm-auth-backend` ∥ `video-blackscreen` ∥ `main-ui-ux` 三路零文件冲突修复并重打包：
    ① **主窗/工具窗拖拽区**（`index.html`/`app.js`/`session.html`/`settings.html`/`transfer.html`/`terminal.html`/`portmap.html`）：Tauri v2 仅对直接命中 `data-tauri-drag-region` 的目标元素启拖，`.titlebar__context` 占满右侧 `1fr` 且无该属性导致右侧无法拖动 → 为所有窗口非交互标题栏容器与动态标题节点补齐 `data-tauri-drag-region`（排除 `<button>`）；
    ② **设备 ID 不再展示 IP**（`index.html`/`app.js`/`main.css`）：未连服时不再把局域网 `IP:端口` 塞进 `#p2DeviceId`，改为明确展示「未连接服务器」并在副行 `#p2DeviceSub` 提供「去配置」入口及独立的局域网直连地址提示；
    ③ **默认关闭允许他人协助 + 手动开启 TCC 权限预检**（`Cargo.toml`/`permissions.rs`/`commands.rs`/`main.rs`/`permissions/default.toml`/`bridge.js`/`app.js`）：移除 `main.rs` 启动自启被控端，新增 `permissions.rs`（委托 `rc-capture-macos::PermissionStatus`，0 新 `unsafe`，按启动快照生成 `restartHint`）与 3 项 IPC + ACL；手动开启 `#p2Allow` 先检测「屏幕录制」「辅助功能」，缺失则拦截开启并弹出权限状态与一键请求/跳转系统设置/重新检测弹窗；
    ④ **密码通过自动放行不再弹接受提示**（`host_bridge.rs`/`commands.rs`/`tests/unified_flow.rs`）：新增 `UiHostConfig::auto_accept_with_password`（生产开启），`req.password_verified == true` 时 `UiApprover::submit` 直接置 `Accepted` 不推 `ApprovalRequested`，仅无密码场景仍需人工审批；
    ⑤ **「连接中/黑屏」多重根因全治**（`metal_attach.rs`/`window.rs`/`session_bridge.rs`/`session.css`/`session.js`）：修 `metal_attach.rs` 把物理像素直接传给期望逻辑 points 的 `CALayer::setFrame`、漏设 `contentsScale` 与 AppKit 非翻转坐标 Y 轴倒置，并在 `window.rs` 对 S1 开启 `.transparent(true)`（`WKWebView` `drawsBackground=NO`）配合 `insertSublayer_atIndex(0)` + `zPosition=-1` + `session.css` `.has-video` / `html { background: transparent }` 使硬件视频从透明舞台透出且 HUD/浮层位于视频之上；`session.js` 补分阶段中文状态文案并在 `connecting`/`negotiated`/`hud` 补报 `stage_rect`；`session_bridge.rs` 与 `host_bridge.rs` 补硬件降级/持续解码失败诊断（避免 `FakeEncoder → VtDecoder` 静默永久黑屏）；
    ⑥ **统筹复查捕漏**：补齐 3 项新权限 IPC 到 `crates/rc-ui/permissions/default.toml` 的 `allow-session-ipc` 并在 `session.css` 显式加 `html { background: transparent; }` 防 WebKit `color-scheme: light` 铺白底。
    ⑦ **真机截图取证二次根治黑屏（2026-11-23，已通过真机全链 E2E 出图验证）**：用户实测发现已出真实视频流（HUD `00:00:35 / UDP LAN / 5 fps / 1.1 Mbps / 22.3 ms frm. / 1920x1080`），但舞台仍为平坦深灰且「已连接，正在等待远端画面…」未消失。逐步二分实机探针锁定两层根因：① `session.css` `.sn-stage__hint` 缺少 `[hidden] { display: none; }` 导致提示文字不消失；② `metal_attach.rs` 此前用 layer-hosting `setLayer:` 挂接后台线程创建的 `CAMetalLayer` 导致 `superlayer == 0x0`（改为 **layer-backed `NSView`** + `host_layer.addSublayer(child)` 后 `CALayer` 可穿透透明 `WKWebView` 合成）；③ **终极根因**：在后台线程 `rc-ui-session` 调用 `[CAMetalLayer layer]` 时，`presentDrawable` 虽返回 `Ok` 但无法向 WindowServer 合成上屏。最终在 `metal_attach.rs` 新增 `MainThreadMetalLayer` / `new_metal_layer_on_main_thread()`，`commands.rs::open_session` 主线程预分配后经 `SessionHandle::spawn_with_metal_layer` 交由 `MetalRenderer::from_raw_layer` 领养。用 `RabbitControlDev.app` 实跑真机全链回环（`HostHandle SCK+VT → QUIC → SessionHandle VT+MetalRenderer → Tauri 透明 S1 窗口`）并 `screencapture` 实拍窗口：实测 **`1280×800 / UDP LAN / 36 fps / 1.9 Mbps / 10.9 ms`** 桌面画面完整呈现，工具栏与 HUD 正常浮于视频上方。重跑全仓 `fmt`、`clippy --workspace --all-targets -- -D warnings`、`cargo test -p rc-ui`（131 passed / 0 failed）与 `./scripts/package-macos.sh` 重建并验签 `target/dist/RabbitControl.app` 与 `target/dist/RabbitControl-0.1.0-arm64.dmg`。
    - **已记录的潜在跟进点**：`crates/rc-capture-macos/src/sck.rs` 按铁律 2 仅转发 `SCFrameStatus::Complete`；若极少数完全静止桌面下 SCK 首包发 `Idle`，后续可考虑在流启动时补一次初始快照（本次未动 `rc-capture-macos`）。
  - **macOS 鼠标位置偏移根治轮（2026-11-23，全链路对齐 + 真机 `CGEventTap` 精度验收 ≤0.68 pt）**：
    真机实测新包画面正常显示后，用户反馈鼠标位置偏移（截图可见 14" MacBook Pro 16:10 桌面以 `1920×1080` 传输，画面左右两侧有明显黑色柱边，悬停 Chrome 触发右侧 App Store 提示）。逐段排查锁定四处叠加根因并一次性根治：
    ① **Host 采集宽高比错配（主因）**：`crates/rc-ui/src/host_bridge.rs` 原先 `caps` 硬编码 `3840×2160`，与控制端默认 `1920×1080` 协商出 16:9 尺寸，导致 `ScreenCaptureKit` 在 `1512×982` 主屏上按默认 `preservesAspectRatio=true` 补齐左右各 ~128.5 px 黑边。修复：`crates/rc-capture-macos/src/ffi/mod.rs` & `lib.rs` 新增零弹窗同步查询 `main_display_logical_resolution()`（`CGDisplay::main().bounds()` 偶数对齐）并在 `SCStreamConfiguration` 显式设 `setScalesToFit(true)`；`crates/rc-session/src/host_session.rs` 新增 `HostSession::set_preserve_caps_aspect(bool)` 与 `fit_even_preserving_aspect`（保持默认 `false` 兼容旧单测）；`host_bridge.rs` 在硬件模式将主屏逻辑分辨率设为 `caps` 并开启等比协商（支持 14"/16" MBP、21:9 带鱼屏、720p/1080p 任意请求零黑边）。
    ② **Host 多屏联合边界错配**：`crates/rc-input-macos/src/ffi/mod.rs`、`injector.rs`、`bridge.rs` 新增 `main_display_bounds()` / `Injector::for_main_display()` / `InputBridge::for_main_display()`，`host_bridge.rs::build_injector` 切为主屏模式，与 `SckConfig { display: 0 }` 严格 1:1 对齐，同时保留 `InputBridge::new()` 兼容既有联合边界测试。
    ③ **Controller 视口与指针单一真值源**：`crates/rc-ui/src/input_bridge.rs` 新增 `fit_video_rect` 与 `normalize_pointer_in_video`，`crates/rc-ui/src/metal_attach.rs` 新增 `Frame::fit_aspect`，`crates/rc-ui/src/state.rs` 在 `ActiveSession` 记录 `video_size` 并经 `effective_layer_frame` 统一驱动 `main.rs` 首次挂载、`commands.rs::stage_rect` 窗口缩放与 `commands.rs::send_input_event` 指针归一化，消除任意窗口宽高比下的画面拉伸与坐标漂移；`crates/rc-ui/ui/assets/js/session.js` 在 `pointerdown`/`pointerup` 前补发当前指针位置并屏蔽远控区原生 `contextmenu`。
    ④ **真机端到端自测闭环**：扩展 `scripts/macos-dev-run.sh test` 支持 `--test <name>` / `--lib`，在 `crates/rc-ui/tests/session_flow.rs` 固化 `#[ignore]` 真机测试 `real_hardware_aspect_and_pointer_accuracy`：实测控制端请求 `1920×1080` → 协商 `1512×982`（`videotoolbox+metal` 硬件首帧上屏），在模拟 `1280×752` S1 舞台发送 Dock 左下 / 中心 / 右上 3 个采样点经真实 QUIC + `InputBridge::for_main_display()` 注入后，`CGEventTap` 实测误差仅 **0.46 / 0.68 / 0.40 pt**。全仓 `cargo test --workspace` **843 passed / 0 failed / 22 ignored**、`rc-input-macos` 真机 4/4 通过、`clippy -D warnings` 0 警告、`fmt` & `node --check` 干净，已重打并验签 `target/dist/RabbitControl.app`（11 MB）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB）。

  - **macOS 5 项体验/高刷/分辨率切换 Swarm 轮（2026-11-23，Swarm 3 worker 零冲突并行 + 统筹集成 + 真机 3K 热切换捉虫根治）**：
    按严格文件隔离派发 `stream-highfps` ∥ `window-adaptive` ∥ `session-ui-ux` 三路并行落地，统筹完成 `commands.rs`/`main.rs` 集成后真机全链自测并一次性闭环 5 项用户需求：
    ① **自适应启动窗口**：`tauri.conf.json` 从 `1440×900`（在 14" MBP 占屏 ~95%）收紧至 `1000×660`（最小 `820×540`），`window.rs::adapt_main_window_on_startup` 按当前显示器逻辑分辨率在 `1280×720` 至 `4K` 间自适应并居中，`main.css` 全页响应式重构（零裸色值）；
    ② **S1 零黑边自动适配 + 一键消除黑边**：`window.rs::compute_fit_session_window_size` 与 `fit_session_window_to_video` 在首次 `Negotiated` 自动把窗口比例对齐远端视频宽高比（如 14" MBP `1512×982` 自动取 `1166×805` 消除左右黑条），同时保留自由拖拽缩放，并在控制中心「窗口设置」与选项卡右键菜单挂载 `fit_session_window` IPC 一键消除黑边；
    ③ **消除 WKWebView 右键「重新加载」遮挡**：`session.js` 在全局 `document` 捕获阶段无条件 `e.preventDefault()`（不 `stopPropagation` 以放行 `#screen-tab` 自定义菜单），并在 `stage` `mousedown`/`pointerdown` 拦截 `e.button === 2` 默认行为，远控右键转发完全保留；
    ④ **突破 70fps 上限**：定位根因为 `commands.rs::default_fps()` 与 `host_bridge.rs::caps.fps`/`pipeline.fps` 均硬编码 `60`（SCK `minimum_frame_interval_us(60)=12500µs` 在 120Hz ProMotion 上交替拍频恰落 70fps）。全链提升至 `144 fps`、`pipeline.bitrate_bps = 20 Mbps`、`RUN_IDLE_POLL_BUDGET = 4ms`，并接通 S3 画质菜单实时切 `144/120/90/60/30 fps`；
    ⑤ **双入口实时分辨率切换 + 真机自测捉虫根治**：经 `TOOL_CHANNEL_DISPLAY_CTRL = 5` + `HostSession::reconfigure_stream`（补单调 `frame_seq_offset` 防 `ControllerPipeline` 丢弃重建后新帧）+ `display_ctrl.rs` 等比预设实现零断连实时热切换，选项卡右键菜单与控制中心「窗口设置」双入口同步勾选。**真机自测捉出关键缺陷**：切换 Retina `3024×1964 @ 120fps` 时触发 Apple Silicon VT `RealTime 过载` 并致 `make_host_pipeline_factory` 静默降级 `FakeEncoder`（对端硬件解码黑屏）→ 随即在 `host_bridge.rs` 对 `> 2.6M` 像素档自动收敛编码期望帧率 `<= 60fps`、失败时自动 60fps 重试且活跃硬件会话绝不降级 `FakeEncoder`，复测真机热切 `3024×1964` 零报错、零降级通过。最终 `cargo test --workspace -- --test-threads=2` **861 passed / 0 failed / 22 ignored**（`rc-ui` **148 passed / 0 failed / 5 ignored**）、真机 `real_hardware_aspect_and_pointer_accuracy` 通过、`clippy -D warnings` 0 警告、`fmt` & `node --check` 干净，已重打签名 release `target/dist/RabbitControl.app`（11 MB）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.4 MB）。

  - **macOS 11 项 UX/120fps/真物理切屏/持久化 Swarm 轮（2026-11-23，Swarm 4 worker 并行 + 统筹集成 + M1 Pro 真机实测 + `ClientSession` 握手同批排空根因修复）**：
    按文件隔离派发 `perf-display-hw` ∥ `prefs-relay-dock` ∥ `main-ui-state` ∥ `session-ui-menus` 四路 Swarm 并行落地，统筹打通 IPC / 状态机 / Dock 回调 / 后台持久化并修复全仓测试暴露的隐藏竞态：
    ① **帧率冲上 >110fps**：根因在于 `VtEncoder` 原为单帧同步等待编码（`1512×982` 实测均值 `14.06 ms/帧` 致 `98.0 fps` 上限）且 `host_bridge` 每拍硬睡 `1ms`。改造为 2-deep 异步流水线（`rc-encode` 增默认 pipelined trait 方法、`rc-encode-vt` 以 `VTCompressionSessionCompleteFrames` 与回调信号量保 2 帧在途且保活 `resource_guard` 至帧提取、`rc-pipeline::host` 2-deep 送取闭环与 `has_pending_work`、`rc-capture-macos` `SCK_SURFACE_POOL_DEPTH=8`、`host_bridge` 在途/忙碌零休眠、`metal_attach` 挂载 macOS 14+ 120Hz ProMotion `CADisplayLink`）。M1 Pro 实测（120Hz Swift 动画窗，`./scripts/macos-dev-run.sh cli --release -- loopback --backend real --resolution 1512x982 --duration 4s`）：`144fps` 档 `displayed` **98.0 → 112.5 fps**、`encode` **14.06 → 11.27 ms**、`dropped_budget` **59 → 19**、`unfinished=0`；`120fps` 档 `displayed=109.0 fps >= 108.0`；
    ② **安全中转开关与设备码打通**：`LanPeer`/`Beacon` 附带 `device_id`、`SessionUpdate::Negotiated` 下发真实 `transport`、新增 `set_session_relay_mode` IPC 与 `UiSessionConfig.force_relay`，在 `session.js` 将 `#opt-relay` 与 HUD 第二行联动（无设备码或未连信令服时明确报错并回滚开关状态）；
    ③ **HUD 显隐移入控制中心「更多」**：删除 `session.html`/`session.css`/`session.js` 右下角铅笔图标 `#hud-toggle`，移入「更多」子菜单 `✓ 显示网络状态`（由 `AppPrefs.show_hud` 持久化）；
    ④ **真物理显示器分辨率切换 + 断开自动恢复**：`rc-capture-macos` 新增 `list_main_display_modes` 与 `DisplayModeGuard`（`CGDisplayCopyAllDisplayModes` / `CGDisplaySetDisplayMode` + `kCGConfigureForSession` 并在 `Drop` 自动恢复原生 `1512×982`），联动 `rc-input-macos::Injector::set_screen_bounds`、`host_bridge.rs` 真实切屏以及 `main.rs` 宽高比变化自适应 `fit_session_window_to_video`；真机测试 `real_hardware_aspect_and_pointer_accuracy` 实测 `presets=22`、切 `3024×1964@120fps`、断开后校验恢复 `1512×982`；
    ⑤ **持久化「允许他人远程协助」**：新增 `crates/rc-ui/src/app_prefs.rs` 原子持久化 `$HOME/.config/rabbitcontrol/prefs.json`，`commands.rs::set_host_enabled` 同步写入 `allow_remote_assist`，`main.rs::setup` 仅在持久化为 `true` 且 `ensure_host_ready` 权限齐全时自动启动被控端；
    ⑥ **桌面壁纸快照与连接记录**：被控端后台抓主屏壁纸 JPEG（`<=96KiB`）经 `DisplayCtrl::Wallpaper` 推送至控制端保存于 `~/.config/rabbitcontrol/wallpapers/`（并严守不并入 `session://update` 且不覆写 `last_session_update` 规则），P1 主卡渲染真实背景壁纸与「最近已连接」；
    ⑦ **macOS Dock 右键设备与屏幕子菜单**：`metal_attach.rs` 实现 `install_dock_menu_handler`/`update_dock_menu_devices` 并由 `main.rs` 主线程注册与刷新，右键 Dock 图标展示各在线设备及其 `显示屏1 (Built-in Retina Display)` 子菜单，`app.js` 响应 `dock://connect-device`（已控聚焦 S1，否则唤醒主窗连接）；
    ⑧ **移除 `172.16.1.2` placeholder**（`crates/rc-ui/ui/settings.html`）并补充 `refresh_device_id` IPC 与 P2 刷新按钮（⑨）、主窗当前远控状态横幅/徽章/遮罩（⑩：`ActiveSessionMeta`/`session://active-state`/`focus_session_window`）及画质/帧率/分辨率跨连接记忆（⑪：`get_app_prefs`/`save_stream_prefs`）；
    ⑫ **附加根因捉虫**：全仓串行回归测出 `live_display_reconfigure_updates_resolution_fps_and_keeps_streaming` 偶发漏收初始 `DisplayConfig`，锁定为 `crates/rc-session/src/client_session.rs::handshake()` 在同一 `transport.poll()` 批内紧随 `Welcome` 到达 `ToolChannel` 时提前 `return` 导致丢帧 → 抽出 `drain_post_welcome_events` 覆盖 `handshake` 与 `tick_reconnect` 并补 `trailing_tool_channel_in_welcome_poll_batch_is_preserved` 单测（同时修 `signal_client.rs::dead_connection_without_server_activity_triggers_reconnect` 的 accept 计数竞态）。全仓 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`node --check` 干净、`cargo test --workspace -- --test-threads=1` **883 passed / 0 failed / 23 ignored**，已重打并验签 `target/dist/RabbitControl.app`（11M）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.5M）。

  - **macOS 8 项体验/中继/鼠标拖拽/持续 120fps 修复轮（2026-11-23，直接执行 + 受控真机诊断闭环）**：
    ① **#1 自定义验证码持久化**：`crates/rc-ui/src/app_prefs.rs` 新增 `custom_host_password: Option<String>`（含长度与控制字符清洗），`crates/rc-ui/src/state.rs::AppState::new()` 启动时恢复已保存自定义/无密码设置，`crates/rc-ui/src/commands.rs::set_host_password` 经 `persist_prefs_mutation` 原子写入 `~/.config/rabbitcontrol/prefs.json`（保留旧有随机临时码在关自定义时自动清除）。
    ② **#2 远控文本拖拽选中与双/三击选中**：`crates/rc-input-macos/src/ffi/mod.rs` 新增 `ClickTracker`（左>右>中优先级判断 `LeftMouseDragged` / `RightMouseDragged` / `OtherMouseDragged` 与 500ms / 4pt 窗口内的 `MOUSE_EVENT_CLICK_STATE` 1..=3 跟踪），`rc-input-macos` 40 单测 + 4 真机全过（注：按远控桌面拖拽/连击选词理解修复，若指控制端 UI 自身文本需用户进一步明确）。
    ③ **#3 120fps 掉回 ~80fps 根治**：
       - **根因排查与权衡**：初始引入 `crates/rc-capture-macos/src/ffi/mod.rs` / `lib.rs` `LatencyCriticalActivity`（`NSProcessInfo beginActivityWithOptions:reason:`）与 `QOS_CLASS_USER_INTERACTIVE`（并接入 `sck.rs`、`rc-pipeline/src/host.rs` `rc-host-encode`、`rc-ui` `host_bridge.rs`/`session_bridge.rs`、`rc-cli`）及 `MaximizePowerEfficiency=false` 后，30s loopback 仍从前 3s ~120fps 跌至 `81.9–84.4 fps`；受控 D1–D4 诊断证实真凶为 **`kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true`**（在 Apple Silicon 高帧率下约 3s 后触发媒体引擎降速至 encode ~19ms），关闭后保持 `RealTime=true`(M6) 即稳态跑满 **118.03 fps**（而降像素 `1280×832` 仍跌 83.53 fps、关 `RealTime` 开 LLRC 仍跌 85.03 fps）。
       - **方案**：在 `crates/rc-encode-vt/src/ffi/mod.rs` 新增 `use_low_latency_rate_control(fps) -> fps < 90` 仅对 `fps >= 90` 关闭 LLRC、`fps < 90` 保持 Apple 低延迟 4 件套与 T12 60fps 路径完全不变（不改 `max_in_flight=2` 遵守 Q3、不关闭 `RealTime` 遵守 M6），并同步写入 `docs/reference/tickets/T12-capture-macos.md` 坑位 #11。
       - **单机回环延迟权衡**：30s 高吞吐 loopback 稳态 `118.20 fps`（`encode 12.76 ms` / `dropped_budget=51`）下，由于本机双端同抢通道与高发包速率，`network`（`18.03 ms`）使 `p50` 升至 `36.32 ms`（同 4s 108fps 的 `D1 p50=36.45 ms` 同源）。
    ④ **#4 安全中转开启后异常（0 fps / 黑屏解码失败）**：`crates/rc-ui/src/relay_transport.rs` 原 `RELAY_TX_DATAGRAM_CAP = 16` 在中继视频帧大于 16 个分片时大量丢包致 IDR 残缺；调高 `RELAY_RX_QUEUE_CAP` / `RELAY_TX_DATAGRAM_CAP` 至 `4096` 并增 4 MiB TX 字节上限淘汰保护；`crates/rc-ui/ui/assets/js/session.js` 在 `ended` 时禁用 `#opt-relay`。`relay_transport` 9/9 + `relay_session` 12/12 + `172.16.1.2` 真机全通。
    ⑤ **#5 退出远控未关 S1 窗**：`WKWebView` 拦截 `window.close()`；`crates/rc-ui/src/window.rs` 新增 `close_session_window` IPC（disconnect + `win.destroy()` + 聚焦 `main`）注册于 `main.rs`、`permissions/default.toml`、`crates/rc-ui/ui/assets/js/bridge.js` 与 `session.js`。
    ⑥ **#6 输入验证码弹窗精简**：移除 `crates/rc-ui/ui/assets/js/app.js::promptAndLaunchSession` 中的冗余 `目标地址：...` 描述文本。
    ⑦ **#7 控制中心未接线菜单全部落盘**：`crates/rc-ui/src/commands.rs`/`main.rs`/`permissions/default.toml`/`bridge.js` 新增 `set_session_watch_only` IPC；`crates/rc-ui/ui/{session.html, assets/css/pages/session.css, assets/js/session.js}` 完整接线「声音」「安全」（观看模式、`⌃⌘Q` 立即锁屏、退出时锁定被控端）与「外设」macOS 快捷键，显式禁用「切换下一个屏幕」（提示被控端多显示器切换暂未支持），彻底清零 `"功能开发中"` 占位提示。
    ⑧ **#8 两级 HiDPI 分辨率菜单**：`crates/rc-ui/src/{display_ctrl.rs, host_bridge.rs, session_bridge.rs}` 新增 `DisplayPreset::is_hidpi`，按无「原生」前缀的 `W × H (HiDPI)` / `W × H` 渲染两级弹出菜单 `#screen-res-menu`（主列表前 5 档 + `其他分辨率 ›` 嵌套子菜单 `#screen-more-menu`），同步从「窗口」面板移除旧 `#window-res-list`。
    ⑨ **踩坑与回归修复记录**：① `SessionHandle` 无 `drain_updates()`，测试需用 `try_recv_update()`；② 严格遵守 Q3 禁改 `max_in_flight=2`，`MaximizePowerEfficiency`/`ExpectedFrameRate` 均非降频杠杆；③ Step 3 初次把 `NSProcessInfo` (`LatencyCriticalActivity`) 放在 `crates/rc-cli/src/cmd/host.rs::run` 入口绑定 UDP socket 之前，导致 `--backend fake` 子进程比 `client` 晚绑端口丢首包触发 Quinn 1000ms Initial RTO（`clipboard_scripted_sync_over_real_quic` 复现两次失败 `connect_to_first_frame=1051.4ms`），将该调用延后到 `host.local_addr()` 绑定完成之后并仅在 `BackendMode::Real` 启用后 `host_client` 12/12 全绿。全仓 `888 passed / 0 failed / 24 ignored`、`clippy -D warnings` 0 警告、`fmt` / `node --check` 干净、签发 `.app`（11M）与 `.dmg`（6.5M）验签通过。

  - **macOS 3 项体验/锁屏/菜单交互追加修复轮（2026-11-23，直接执行闭环）**：
    ① **退出时锁定被控端生效 + 开关持久化**：`crates/rc-capture-macos/src/{ffi/mod.rs, lib.rs}` 新增 `lock_screen_now()`（`dlopen`/`dlsym` 调用 `login.framework` 原生 `SACLockScreenImmediate`，失败回退 `/usr/bin/pmset displaysleepnow`，`has_lock_screen_symbol` 离线校验不锁真屏）；`crates/rc-ui/src/app_prefs.rs` 新增 `AppPrefs.lock_on_exit` 与 `StreamPrefsPatch.lock_on_exit`；`crates/rc-ui/src/display_ctrl.rs` 新增 `DisplayControlMsg::LockScreen` 走 `TOOL_CHANNEL_DISPLAY_CTRL`；`crates/rc-ui/src/{session_bridge.rs, host_bridge.rs, commands.rs, window.rs}` 实现 `SessionCommand::LockRemoteScreen` 与 `SessionHandle::request_lock_on_disconnect()`（断线 `Bye(Normal)` 前刷新 150ms），主机端即便同批收到 `r.closed.is_some()` 亦先排空 `LockScreen` 并待 `display_mode_session` 还原原生分辨率后再锁屏；前端 `bridge.js`/`session.js` 回填并保存 `#sec-lock` / `#opt-auto-lock-exit` 且统一 S1 退出、`⌘W`/红叉及「立即锁定被控端屏幕」。
    ② **验证码输入框默认掩码 + 小眼睛切换**：`crates/rc-ui/ui/assets/js/app.js` 抽出 `createPasswordField`（零 `innerHTML`）将 `promptAndLaunchSession` 与 `openCodeSettingsModal` 改为默认 `type="password"` + `#i-eye-off`/`#i-eye` 切换按钮，`crates/rc-ui/ui/assets/css/pages/main.css` 补充全变量 token 样式。
    ③ **「其他分辨率」次级菜单悬停易消失修复**：`crates/rc-ui/ui/assets/js/session.js` 为 `appendResolutionButton(..., isNestedItem)` 区分次级项（仅主列表项绑定 `scheduleNestedClose` 180ms 悬停宽限计时器，不再给次级列表自身按钮误绑关闭回调），并在 `#screen-more-menu` `pointerenter` 与 `moreBtn` 取消计时器、`placeFloatingMenu` 将次级浮层与父菜单重叠 `2px` 消除跨缝抖动。
    ④ **测试隔离踩坑**：`AppState::new()` 读取开发机真实 `$HOME/.config/rabbitcontrol/prefs.json` 中的 `customHostPassword`，导致全仓单测 `host_service_lifecycle_and_password_updates_use_ephemeral_port` 断言 `!started.custom_password` 失败 → 在 `crates/rc-ui/src/state.rs` 抽出 `with_initial_configs` 与 `#[cfg(test)] new_for_test()` 并迁移 `state.rs`/`commands.rs` 单元测试隔离，全仓 **891 passed / 0 failed / 24 ignored**、`clippy -D warnings` 0 警告、`fmt`/`node --check` 干净、`target/dist/RabbitControl.app`（11M）与 `target/dist/RabbitControl-0.1.0-arm64.dmg`（6.5M）验签通过。

- **2026-11-23 — Phase 1 账号体系、Web 管理后台、桌面双门控、隐藏式局域网速通、自控免密与强制全链 mTLS 落地与对抗评审闭环（Swarm 协同）**：
  - **服务端与 PKI（`server/backend`）**：新增 `migrations/0003_users_auth_pki.sql` 与 `0004_session_replay_history.sql`（`users`、`user_sessions` 含一次性宽限与多代重放检测字段 `retired_token_hashes`、`system_settings`、`server_keys`、`pki_ca`、`device_certificates`、`audit_logs`）；`ephemeral.rs` 新增 `SettingsCache`（PG 持久化 + Redis `rc:setting:allow_registration` 60s 缓存 / 无 Redis 时 `MemoryEphemeralStore` 回退，带 `write_epoch` 防并发回填脏读）与 `rc:revoked_cert:{serial_hex}`；`auth.rs` 启动初始化随机密码管理员账号（`must_change_password = true`，全量业务路由未改密前 fail-closed `403 password_change_required`）与 Ed25519 JWT/Refresh 轮换、`OwnerGrantTicket` 签发；`pki.rs` 管理 Root CA、`:21116` TLS 1.3 证书与 `/api/v1/certs/issue` CSR 签发 7 天设备证书（SAN URI `urn:rabbitcontrol:user:{uid}:device:{did}`）；`relay/mod.rs` 在 `:21117` 强制开启 TLS 1.3 mTLS 与 peer SAN 绑定校验，并在用户禁用/删除/改密后实时踢断信令与中继会话。
  - **管理后台（`server/web`）**：基于 React 19 + TypeScript + Vite 6 实现管理员登录、首次登录强制改密页、Dashboard/Users/Devices/Sessions/Settings 五视图、401 单飞静默 Refresh（含 `authGeneration` 竞态保护与 `usedAccessToken` 去重）及 `allow_registration` 页面开关（禁止自身降权/禁用）。
  - **传输与会话握手（`crates/rc-net`、`crates/rc-session`）**：`rc-net/src/{mtls.rs,quic.rs}` 实现双向 CA 校验与大小写无关 URI SAN 验证（`eq_ignore_ascii_case`）；`rc-session/src/{proto.rs,auth.rs,host_session.rs,client_session.rs}` 实现基于 `OwnerTicket` Ed25519 PoP 与 `OwnerGrantTicket` 的车主自控通道，即使关闭「允许他人远程协助」仍可无验证码/无弹窗自控且彻底拒绝陌生人探测。
  - **桌面客户端与门控（`crates/rc-ui`）**：
    - 双门控启动（`server_config.rs` / `auth_client.rs` / `cert_manager.rs` / `commands.rs` / `window.rs`）：首启必须配置并探测服务端与 Root CA 指纹，未登录或处于 `must_change_password` 门控时禁止主控、列表、文件、配置与投屏窗口操作；登录后自动 `bind_device` 并通过 CSR 签发本地密钥与设备证书，后台静默 Refresh 轮换与重启持久化。
    - 隐藏式局域网速通与加密开关（`lan_discovery.rs` / `p2p.rs` / `relay_transport.rs` / `host_bridge.rs` / `session_bridge.rs` / `app_prefs.rs`）：前端移除 LAN 设备展示与 `list_lan_devices` 数据源，仅保留 `LanRouteCache` 加速 `UDP LAN → UDP P2P → mTLS 中继` 三段路由；控制中心与被控端设置新增 `encrypt_connection` / `accept_mtls_lan`（经 `rc-ui::mtls_bench` 与 `rc-net::mtls_loopback` 实测稳态增量 < 0.5ms 远低于 3ms 阈值，默认 `DEFAULT_ENCRYPT_CONNECTION = true`），开启后强制握手 mTLS 拒绝降级明文。
    - 顺手根治旧竞态：`session_bridge.rs` 的 `LatencyCriticalActivity::begin` 移至握手完成后，消除 macOS XPC ~2.6s 阻塞导致的 P2P 握手超时。
  - **两轮对抗审查（安全/系统）与终验修复**：修复 `RefreshToken` 宽限窗重放可多刷与多代重放漏洞（`0004` + `store.rs`）、Redis `SettingsCache` 并发写脏读、`api.rs` revoked session fail-closed、`rc-net` user_id 大小写敏感、`signal_client.rs` `use_https && ca_cert_pem.is_none()` 拒绝明文回退、`Kicked` 原始 reason 入队与 `account_disabled`/`user_deleted`/`token_revoked` 即时退登、`commands.rs` `activate_session_with_cfg` `session_open_epoch` 与 `prefs_io_lock` 回滚、`auth_client.rs` 登出先清本地凭证等；复验 Workspace **932 passed / 0 failed / 24 ignored**、`server/backend` **71 passed / 0 failed**、`server/web` build 0 error、JS 10 文件 syntax 全过，同步更新 [`server/docs/02-protocol-and-api.md`](server/docs/02-protocol-and-api.md)。

- **2026-11-23 — Phase 1 多轮深度复审清零 + 客户端 W2/W3/W4 工具窗全链路接线与 5 轮审查闭环**：
  - **Phase 1 多轮复审清零（Server+Web 6 轮、Client+Crypto 7 轮均达 `ZERO_ISSUES_FOUND`）**：
    - 服务端与协议（`server/backend/src/{api.rs, store.rs, signal.rs, relay/mod.rs}`、`server/backend/tests/auth_pki_e2e.rs`）：`/api/v1/auth/logout` 新增 `refresh_token` 兜底与 `verify_explicit_device_owner` 所有权预检；`bind_device_to_user_with_prev` 事务原子迁移设备归属并在跨号接管时吊销前任证书/会话；`authorize_peer_connection` 校核主叫设备归属与 `device_pubkey` 一致性；`relay/mod.rs` 修复 `device_kick_seq` 单调水位与建流前采样 `connect_kick_seq`。
    - 客户端与加密（`crates/rc-ui/src/{auth_client.rs, server_config.rs, host_bridge.rs, commands.rs}`、`crates/rc-ui/tests/owner_mtls_routing.rs`、`crates/rc-ui/ui/settings.html`）：`auth_client.rs` 为 `persist_token_response`/`change_password`/`perform_refresh_locked`/`restore_on_startup` 引入 `session_epoch` 与 `wipe_local_credentials_if_epoch` 防止过期会话锁退与跨登出竞态，且启动遇到期 token 未刷新前绝不解锁；`server_config.rs` 开启 `use_https` 时强制非空 `ca_cert_pem`；`host_bridge.rs` 引入 `HostListener::AwaitingMtlsCert`（50ms 轮询等待证书就绪并拒绝中继/明文降级）；`save_security_prefs` 在关闭 `allow_remote_assist` 时清理悬停审批；`settings.html` 订阅 `Bridge.onGateStatus` 实时更新证书态。
  - **客户端 W2 / W3 / W4 全链路接线（5 轮只读 Review 达成 `ZERO_ISSUES_FOUND`）**：
    - **后端与生命周期协同**（`crates/rc-tools/src/{file_transfer.rs, portmap.rs}`、`crates/rc-ui/src/{app_prefs.rs, state.rs, window.rs, commands.rs, main.rs, session_bridge.rs, model.rs}`）：`FileHostEngine::enqueue_critical` 在满队（64 帧）时优先淘汰 `FRAME_KIND_DOWNLOAD_CHUNK` 以保障 `ListDirResp`/`MkdirResp`/`UploadAck`/`UploadDone`/`DownloadMeta` 关键控制响应永不静默丢帧；`PortMapClientEngine::add_rule_with_enabled` 支持禁用态恢复免短暂绑定端口；`AppPrefs.saved_portmaps` 按设备持久化上限 32 条并在会话 `Negotiated` 或切出只读观看模式时经 `"rc-portmap-restore"` 线程自动恢复（`prefs_io_lock` 与单次 `state.session` 锁严格无环序）；`active_session_tools` 强化登录门禁与 `watch_only` 拒绝；`SessionHandle`/`BridgeInputCapture` 改为共享 `Arc<AtomicBool>` 动态 `watch_only`，切入只读立即 `close_all_terminals()` 防 4-PTY 槽位泄漏；`main.rs` 使用 `resolve_session_update_emit` 在发射前原子读取最新 `ActiveSessionInfo` 并同步 `Connecting`/`Negotiated` `watch_only`；`window::close_session_and_tool_windows` 在登出、切换未认证服务器或关闭被控端时统一销毁 `session`/`transfer`/`terminal`/`portmap` 窗口。
    - **主窗与三页工具 UI**（`crates/rc-ui/ui/{index.html, transfer.html, terminal.html, portmap.html}`、`assets/js/{app.js, transfer.js, terminal.js, portmap.js, validate.js}`、`assets/css/pages/{terminal.css, portmap.css}`）：主窗设备右键菜单与 `#p2SessionBar` 接入 W2/W3/W4 工具入口并支持未连设备建连后按目标键自动拉起（`sawTargetActive` 防取消/切设备误触发）；`transfer.js` 接入双侧会话同步、断连/观看重置、15s `REMOTE_REQ_TIMEOUT_MS` 超时、`name`/`date`/`type`/`size` 列排序与内联路径编辑；`terminal.js` 零 `.innerHTML` 全重写，接入最多 4 标签页真实远端 PTY、`ResizeObserver` + `1..=512` 网格裁剪、断线保留滚动历史、VT100/ANSI（含 `?1049h/l` 备用屏与 resize 行裁剪、SGR 16/256/RGB、宽字符 CJK、Alt/F1–F12/`Shift+Tab`/`Ctrl+Shift+C/V`）；`portmap.js` + `validate.js` 接入 `ActiveSessionInfo` 状态栏、观看模式只读禁用、实测活跃连接/上下行字节统计与 `row.error` 转义展示及一致的 `trim` + 空白/控制字符校验。
  - **终验数字与跨平台 CI 收口**：补充 `server/web/src/api/{client.ts, mockServer.ts}` 全链路 `authGeneration` 竞态隔离与 mock 重载校验、`crates/rc-ui/src/{window.rs, metal_attach.rs, main.rs, state.rs, lan_discovery.rs, p2p.rs, signal_client.rs}` / `crates/rc-cli` / `crates/rc-tools/src/pty_unix.rs` / `crates/rc-capture-macos/src/lib.rs` / `crates/rc-decode-vt/src/lib.rs` / `crates/rc-input-win/src/inject.rs` 的 Linux & Windows `-D warnings` 兼容性收口并完成 [`.github/workflows/ci.yml`](.github/workflows/ci.yml) 与 [`.gitignore`](.gitignore) 配置；终验 Workspace **`939 passed / 0 failed / 24 ignored`**（78 suites，含 `rc-tools` 24+11+5+2、`rc-ui` 113+50+1+5+12+7+4）、**`server/backend` 73 passed / 0 failed**（61+6+6）、`server/web` build 0 error（1643 modules）、JS `node --check` **10/10**、`clippy -D warnings` / `fmt --check` 干净，并同步 [`server/docs/02-protocol-and-api.md`](server/docs/02-protocol-and-api.md)。

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
