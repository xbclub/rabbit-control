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
| 当前状态 | 21 个 crate 已落地（**328 测试全绿**，clippy/fmt 干净，另 9 项真机/权限 `#[ignore]`）；**B3 真机全链 M1 Gate 已全项通过（2026-11-21 终验，`--backend real`：sck→VT→VT→Metal，隐私幕布，`max_in_flight=2`，PTS 节拍过滤已回退）**：动态 60s P50=**11.28ms**/P99=21.20ms/70.0fps/队列=2/零丢帧/不变式全过；运动 15s P50=11.16ms/69.5fps 全项过；静态 30s 单核 CPU 稳态 **1.51%**（≤8%）/`dropped_repeat=0`。基础设施：隐私幕布已固化进仓库（`rc-render-macos/tests/curtain.rs`，`--features window-bench`）。**阶段 C：T09 `rc-net`（QUIC 双通道传输层）已实现并通过本卡全部 17 项测试（2026-11-21，正式门禁由编排脚本复跑）；T10 输入通道已完成（2026-11-21）：`rc-input` 核心（事件/4ms 批处理/手写二进制线格式/reliable-stream 接线，16 测试）+ `rc-input-macos` 桥接（`InputBridge` 实现 `rc_input::InputInjector`，+8 测试）+ `rc-input-win` cfg(windows) 骨架（macOS 编译为空、0 假测试，真机实现属 D3）；I1–I9 全落地，I10（Windows `GetCursorPos` 校准）为 D3 TODO**；**T09 审计遗留修复（2026-11-21）**：① 输入独立 urgent 流（`Transport::send_reliable_urgent` 默认实现 + `NetEvent::ReliableUrgent`，`QuicTransport` 第二条双向 reliable 流 + quinn `set_priority(1)` + 专职写线程化解 connection 级流控共享窗口的队头阻塞）+ 契约 §7.3 增 N6；② N9 迁移集成测试（`QuicEndpoint::rebind` + `PathChanged` 断言）；rc-net **23 单测 + 7 集成 = 30 全绿**（原 27 全保留）；③ macOS `CGEventTap` 捕获器（rc-input-macos `CgCapture`：ListenOnly tap + 专用 CFRunLoop 线程 + `AXIsProcessTrusted` 预检，离线 31+2 测试，真机 3 项 `--ignored` 授权后实测**全部通过**：注入 tap 见证 ✅、捕获→注入回环 8→8→5→5 ✅、延迟探针 p50=0.66ms ✅）；**外部审计修复轮（2026-11-21，workflow「审计修复轮：传输与输入」+ 收口）七项全修**：① rc-net `loss_rate` 改 seq 窗口+500ms 主动结算与分片去重（秒级可测）；② macOS 滚轮统一**行单位**（注入 LINE，跨平台规范写入文档，真机回环 3/3 等值）；③ QUIC 双流角色按流 ID 序号判定（与到达顺序无关）；④ rc-input 接收错误路径**零丢兄弟消息**（逐条处理+聚合首错）；⑤ 重组/FEC 超时改 `wrapping_sub`（u32 回绕免疫，8h soak 安全）；⑥ tap/urgent 通道有界化+满载丢最旧、`too_many_arguments` 重构消除；⑦ Drop 前关流。真机测试串行化锁（libtest 并行互踩根因修复）。workspace **366 测试全绿 / clippy / fmt 干净**，真机 4/4（含新增滚轮回环））；④ 复审发现 urgent 流未在 rc-input 应用层接线（rc-net 修好了、调用方仍走共享流）——已修：`send_events`/`pump_receiver` 切 `send_reliable_urgent`/`ReliableUrgent`，接缝测试 `input_goes_over_urgent_stream_not_control` 钉死；workspace **349 测试全绿 / clippy `-D warnings` 0 警告 / fmt 干净**（见 §12/§13）；**rc-input-macos 审计修复轮（2026-11-21）**：① 滚轮量纲统一为**行**（注入侧 `ScrollEventUnit::PIXEL→LINE`，实测 3 行曾缩水成 tap 读回 1 行、现逐值对称，rc-input `MouseWheel.delta` 文档澄清规范单位=行）② tap 事件通道无界→**有界 `sync_channel(256)` 满时丢最旧**（回调全程非阻塞原语，丢弃独立计数 `CgCapture::dropped_channel_full`）；rc-input-macos **36 单测 + 2 doctest 全绿（原 31+2 全保留）、真机 4/4 通过**（原 3 项不破坏 + 新增滚轮回环 `wheel_line_unit_roundtrip_real_machine` 3 条等值匹配、满载丢弃=0）（见 §12/§13）；**rc-net 传输审计修复轮（2026-11-21）**：① loss_rate 失真（高）——`ArrivalTracker` 按 `packet_idx` 去重分片到达 + 超时（对齐 `FRAME_STALE_US`=500ms）**主动结算**过期未凑齐帧（不再等跟踪表攒满 4096：60fps/3% 丢包原本要 30-60 分钟才首次出 nack、期间恒 0.0）；集成实测 3% 代理丢包下收敛 loss_rate **0.021–0.027 vs 实测 UDP 丢率 0.022**，重复分片不再误判缺包帧完整；② 流角色互换（中）——accept 侧按 **QUIC 流 ID 序号**（`StreamId::index()` 小者=control）判角色（`control_is_first` 纯函数单测 + 5% 丢包握手下角色不互换集成测试），弱网重传颠倒 `accept_bi` 交付顺序不再静默换流；③ u32 微秒回绕（中）——`reassemble.rs`/`fec_adapter.rs` 的 gc 改 `wrapping_sub`（8 小时 soak 跨 71.5min 回绕点不再冻结超时判定）+ 两端跨回绕测试；④ 卫生——`too_many_arguments` allow 移除（`ReliableStream` 收拢两流参数）、Drop 先 `Option::take` 关 urgent_tx 再 join；⑤ urgent 通道无界→**有界 `sync_channel(64)` 满队报错**（丢弃决策留给持输入语义的上层，写进文档与 `urgent_enqueue_error` 单测）；rc-net **32 单测 + 8 集成 = 40 全绿（原 30 项全保留）/ clippy `-D warnings` 0 警告 / fmt 干净 / 无 unsafe / `DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动**（见 §12/§13）；**审计二轮修复（2026-11-21，workflow「审计修复轮：乱序丢包与批内隔离」+ 只读独立复核）两项全修**：① rc-net `ArrivalTracker::observe` 改**在途优先于结案拦截**（多片帧 seq=9 在途、单片帧 seq=10 抢先推高 `highest_retired` 后，seq=9 的迟到分片不再被 `9<=10` 吞掉——原顺序下帧永远凑不齐、500ms 后被 settle 误记幻影 NACK 致 loss_rate 虚高；回归测试 `inflight_frame_outranks_retirement_gate` 修复前实测 FAILED 锁死）；② rc-input-macos `InputBridge::inject` **批内错误隔离**（与 `pump_receiver` 同语义：记首错继续注入同批剩余事件，X1/X2 侧键翻译失败或投递失败不再吞掉后续松键→防卡键；新增侧键+投递失败两个隔离测试）；workspace **369 测试全绿 / clippy `-D warnings` 0 警告 / fmt 干净**，独立复核通过（见 §12/§13）；**UI 设计规范已产出（2026-11-21，零代码）**：rc-ui 框架定为 **GPUI + gpui-kit**（经 UU 远控双端解剖取证与用户裁决，原「Tauri 或 egui」表述作废，`07-engineering-velocity.md` 已修正），设计规范落地 `docs/reference/design/08-ui-design.md`（12 屏全覆盖，实现属 M2-6） |
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
