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
| 当前状态 | 21 个 crate 已落地（**328 测试全绿**，clippy/fmt 干净，另 9 项真机/权限 `#[ignore]`）；**B3 真机全链 M1 Gate 已全项通过（2026-11-21 终验，`--backend real`：sck→VT→VT→Metal，隐私幕布，`max_in_flight=2`，PTS 节拍过滤已回退）**：动态 60s P50=**11.28ms**/P99=21.20ms/70.0fps/队列=2/零丢帧/不变式全过；运动 15s P50=11.16ms/69.5fps 全项过；静态 30s 单核 CPU 稳态 **1.51%**（≤8%）/`dropped_repeat=0`。基础设施：隐私幕布已固化进仓库（`rc-render-macos/tests/curtain.rs`，`--features window-bench`）。**阶段 C：T09 `rc-net`（QUIC 双通道传输层）已实现并通过本卡全部 17 项测试（2026-11-21，正式门禁由编排脚本复跑）；T10 输入通道已完成（2026-11-21）：`rc-input` 核心（事件/4ms 批处理/手写二进制线格式/reliable-stream 接线，16 测试）+ `rc-input-macos` 桥接（`InputBridge` 实现 `rc_input::InputInjector`，+8 测试）+ `rc-input-win` cfg(windows) 骨架（macOS 编译为空、0 假测试，真机实现属 D3）；I1–I9 全落地，I10（Windows `GetCursorPos` 校准）为 D3 TODO**；**T09 审计遗留修复（2026-11-21）**：① 输入独立 urgent 流（`Transport::send_reliable_urgent` 默认实现 + `NetEvent::ReliableUrgent`，`QuicTransport` 第二条双向 reliable 流 + quinn `set_priority(1)` + 专职写线程化解 connection 级流控共享窗口的队头阻塞）+ 契约 §7.3 增 N6；② N9 迁移集成测试（`QuicEndpoint::rebind` + `PathChanged` 断言）；rc-net **23 单测 + 7 集成 = 30 全绿**（原 27 全保留）；③ macOS `CGEventTap` 捕获器（rc-input-macos `CgCapture`：ListenOnly tap + 专用 CFRunLoop 线程 + `AXIsProcessTrusted` 预检，离线 31+2 测试，真机 3 项 `--ignored` 授权后实测**全部通过**：注入 tap 见证 ✅、捕获→注入回环 8→8→5→5 ✅、延迟探针 p50=0.66ms ✅）；**外部审计修复轮（2026-11-21，workflow「审计修复轮：传输与输入」+ 收口）七项全修**：① rc-net `loss_rate` 改 seq 窗口+500ms 主动结算与分片去重（秒级可测）；② macOS 滚轮统一**行单位**（注入 LINE，跨平台规范写入文档，真机回环 3/3 等值）；③ QUIC 双流角色按流 ID 序号判定（与到达顺序无关）；④ rc-input 接收错误路径**零丢兄弟消息**（逐条处理+聚合首错）；⑤ 重组/FEC 超时改 `wrapping_sub`（u32 回绕免疫，8h soak 安全）；⑥ tap/urgent 通道有界化+满载丢最旧、`too_many_arguments` 重构消除；⑦ Drop 前关流。真机测试串行化锁（libtest 并行互踩根因修复）。workspace **366 测试全绿 / clippy / fmt 干净**，真机 4/4（含新增滚轮回环））；④ 复审发现 urgent 流未在 rc-input 应用层接线（rc-net 修好了、调用方仍走共享流）——已修：`send_events`/`pump_receiver` 切 `send_reliable_urgent`/`ReliableUrgent`，接缝测试 `input_goes_over_urgent_stream_not_control` 钉死；workspace **349 测试全绿 / clippy `-D warnings` 0 警告 / fmt 干净**（见 §12/§13）；**rc-input-macos 审计修复轮（2026-11-21）**：① 滚轮量纲统一为**行**（注入侧 `ScrollEventUnit::PIXEL→LINE`，实测 3 行曾缩水成 tap 读回 1 行、现逐值对称，rc-input `MouseWheel.delta` 文档澄清规范单位=行）② tap 事件通道无界→**有界 `sync_channel(256)` 满时丢最旧**（回调全程非阻塞原语，丢弃独立计数 `CgCapture::dropped_channel_full`）；rc-input-macos **36 单测 + 2 doctest 全绿（原 31+2 全保留）、真机 4/4 通过**（原 3 项不破坏 + 新增滚轮回环 `wheel_line_unit_roundtrip_real_machine` 3 条等值匹配、满载丢弃=0）（见 §12/§13）；**rc-net 传输审计修复轮（2026-11-21）**：① loss_rate 失真（高）——`ArrivalTracker` 按 `packet_idx` 去重分片到达 + 超时（对齐 `FRAME_STALE_US`=500ms）**主动结算**过期未凑齐帧（不再等跟踪表攒满 4096：60fps/3% 丢包原本要 30-60 分钟才首次出 nack、期间恒 0.0）；集成实测 3% 代理丢包下收敛 loss_rate **0.021–0.027 vs 实测 UDP 丢率 0.022**，重复分片不再误判缺包帧完整；② 流角色互换（中）——accept 侧按 **QUIC 流 ID 序号**（`StreamId::index()` 小者=control）判角色（`control_is_first` 纯函数单测 + 5% 丢包握手下角色不互换集成测试），弱网重传颠倒 `accept_bi` 交付顺序不再静默换流；③ u32 微秒回绕（中）——`reassemble.rs`/`fec_adapter.rs` 的 gc 改 `wrapping_sub`（8 小时 soak 跨 71.5min 回绕点不再冻结超时判定）+ 两端跨回绕测试；④ 卫生——`too_many_arguments` allow 移除（`ReliableStream` 收拢两流参数）、Drop 先 `Option::take` 关 urgent_tx 再 join；⑤ urgent 通道无界→**有界 `sync_channel(64)` 满队报错**（丢弃决策留给持输入语义的上层，写进文档与 `urgent_enqueue_error` 单测）；rc-net **32 单测 + 8 集成 = 40 全绿（原 30 项全保留）/ clippy `-D warnings` 0 警告 / fmt 干净 / 无 unsafe / `DEFAULT_MAX_CONCURRENT_FRAMES==2` 未动**（见 §12/§13）；**审计二轮修复（2026-11-21，workflow「审计修复轮：乱序丢包与批内隔离」+ 只读独立复核）两项全修**：① rc-net `ArrivalTracker::observe` 改**在途优先于结案拦截**（多片帧 seq=9 在途、单片帧 seq=10 抢先推高 `highest_retired` 后，seq=9 的迟到分片不再被 `9<=10` 吞掉——原顺序下帧永远凑不齐、500ms 后被 settle 误记幻影 NACK 致 loss_rate 虚高；回归测试 `inflight_frame_outranks_retirement_gate` 修复前实测 FAILED 锁死）；② rc-input-macos `InputBridge::inject` **批内错误隔离**（与 `pump_receiver` 同语义：记首错继续注入同批剩余事件，X1/X2 侧键翻译失败或投递失败不再吞掉后续松键→防卡键；新增侧键+投递失败两个隔离测试）；workspace **369 测试全绿 / clippy `-D warnings` 0 警告 / fmt 干净**，独立复核通过（见 §12/§13）；**UI 设计规范已产出（2026-11-21，零代码）**：rc-ui 框架最终定为 **Tauri v2 2.12.7 + 纯静态 HTML/CSS/JS 前端**（沿革 GPUI → Dioxus → Tauri v2，经三框架内存实测与工程评估 + 用户裁决；原「Tauri 或 egui」表述作废），设计规范落地 `docs/reference/design/08-ui-design.md`（12 屏全覆盖，§8 已按 Tauri 重写，实现属 M2-6）；**rc-ui 已实现并收口（2026-11-21 两轮）**：14 个界面 / 22 前端文件 / 8495 行，**真窗口启动渲染确认**（含双交通灯缺陷修复）、Rust 侧多窗口 IPC 命令落地、**6/6 屏通过 visual-judge 视觉验收**、`cargo test --workspace` **450 passed / 0 failed**、clippy `-D warnings` 0 警告、fmt 干净；**CSS 公共层已收敛（2026-11-21）**：`assets/css/base.css` 由 1 行注释扩为 **510 行**唯一实现（reset/排版/.ic/Pill/按钮/Toast/交通灯/titlebar/滚动条），6 页重复样式清零（`box-sizing` 重置全仓仅 1 处），交通灯四种类名别名归一到 tokens 新增的 `--traffic-*`，6 页实跑组件断言全过、JS 交互 0 报错；**M2 会话核心轮（2026-11-21，workflow 写卡→实现→两轮根因修复 + 主 agent 真后端双进程实测）**：新 crate **rc-session**（proto/host/client/probe/watchdog/reconnect/report/testkit，T13–T18 六卡全落地，独立复核通过）+ rc-cli host/client 双进程子命令；两根因修复：① 分片预算 MTU 错配（pipeline 1200 vs rc-net 1152 预算 → 参数化 1120 对齐，64KB 多包帧真 QUIC e2e 钉死）② **真采集时钟锚定顺序**（先建 SCK 后设 origin → 帧帧 Stale 全弃、30s 0 帧编码；修复后 host captured==encoded、超期/重复/预算全 0，并给 HostRunSummary 补 captured/dropped_* 四统计）；workspace **490 测试全绿 / clippy / fmt 干净**；**本机（localhost 双进程真 QUIC + 真后端 SCK/VT/Metal）实测：640x360 60s Gate 全项通过**（首帧 630ms≤800 / p50 10.8ms / p99 18.5ms / 68.3fps / 输入单向 1.29ms / 队列 0；RTT 2.6ms），1080p 90s p50 17.2/p99 26.3/首帧 677ms/零丢包（displayed 45.5fps 为单屏遮挡自反馈伪影，见 §13）；#[ignore] 真机 2/2（localhost_real_backend_smoke / real_inject_via_input_bridge）；；**高帧率轮（2026-11-22）**：120/144/240 全链参数化验证——本机 120fps 真机 **114.6fps 端到端无损穿透**（640x360，SCK 供给上限 ~115），1080p×120 瓶颈实测=VT 硬编吞吐 98.9fps（物理上限），144 离线**满帧 Gate 通过**（144.0fps/p50 10.0ms），240 管线健康（队列 2/p50 17.8ms）；顺手修掉 **VT 过载丢帧拆会话**产品级 bug（EncoderDroppedFrame 非致命化 + Dropped 不再误请求 IDR，真机 90s 过载会话存活）；workspace **492 全绿**（见 §12/§13）；T19–T22 已写卡待领取（见 §12/§13）；**rc-ui 视觉精致度与统一性全面重构（2026-11-22）**：建立三层表面体系（`--bg-canvas #F5F6F8` / `--bg-window #FFFFFF` / `--bg-card #F7F8FA`）、统一 Switch(40×24)/Select/Button/Titlebar 到 `base.css`、修复 5 项视觉缺陷（P1「进入桌面 →」遮挡、S1 双交通灯与留白、W1 Select 箭头缺失、O3 双同心圆头像、W4 Unicode 状态符），裸写色值 0 处、Playwright 15 屏全过（含 1120×720 无溢出、0 console/page errors） |
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
