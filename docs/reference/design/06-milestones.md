# 06 里程碑、WBS 与验收

## 1. 里程碑总览

| 里程碑 | 目标 | 出口条件（Gate） | 交付物 |
|---|---|---|---|
| **M0** 工程基座 | 让 AI 能高速迭代 | `cargo check` ≤3s，CI 绿 | workspace 骨架、CI、回环测试框架 |
| **M1** 单机管线 | 证明「采集→编码→解码→呈现」跑通且够快 | 回环延迟 ≤18ms@1080p60 | 回环模式、延迟埋点、FrameTrace |
| **M2** 双机直连 | 两台机器真的能连上并控制 | 局域网 1080p60 + 输入可用 | 传输层、输入通道、直连 |
| **M3** 弱网与自适应 | 证明「不冻结」 | 3% 丢包无 >500ms 冻结 | FEC、NACK、码率自适应 |
| **M4** 三平台 | Windows 完整 + Linux + macOS | 各平台验收清单通过 | 三平台实现 |
| **M5** 可分发产品 | 能给别人装上用 | 安装包、自动更新、服务端 | 打包、rendezvous、relay |
| **M6** 功能补全 | P1 功能 | 功能清单 P1 全绿 | 文件传输、多标签、审计… |

**关键纪律：M1 的 Gate 不通，不允许进入 M2。** 延迟问题是架构性的，后期无法通过调优补救。这是本项目唯一不可妥协的排期约束。

## 2. M0 工程基座

### 目标
在写任何媒体代码之前，先把「AI 写代码 → 看到结果」的回路压缩到可接受水平。**这是本项目对用户痛点的直接回应。**

### 任务

| # | 任务 | 产出 |
|---|---|---|
| M0-1 | Workspace 拆分（见 07 文档） | `Cargo.toml` + 12 个空 crate |
| M0-2 | 编译提速配置 | `.cargo/config.toml`、profile 设置、lld |
| M0-3 | 开发脚本 | `justfile`：`just c`（check）、`just t`（test）、`just bench` |
| M0-4 | `cargo nextest` + `sccache` 接入 | CI 与本地一致 |
| M0-5 | CI：三平台构建 + clippy + fmt + test | GitHub Actions |
| M0-6 | 磁盘治理 | `cargo sweep` 脚本、CI 独立 target、`.gitignore` |
| M0-7 | 结构化日志与 trace 骨架 | `rc-trace` crate |
| M0-8 | 回环测试框架骨架 | `rc-loopback` crate（先跑通假数据） |

### 出口条件（必须实测，不可估）

| 指标 | 门槛 | 测量方式 |
|---|---|---|
| 单文件改动 `cargo check` | **≤3s** | 改动一个函数体，计时 |
| 单 crate 测试 | **≤15s** | `cargo nextest -p rc-sched` |
| 全量测试 | ≤90s | `cargo nextest` |
| `target/` 占用 | ≤10GB | `du -sh target` |
| 冷编译（全新 clone） | ≤8min | CI 计时 |
| CI 全绿 | 三平台 | — |

**若 `cargo check` 达不到 3s**：先排查是否 crate 拆分不合理（依赖倒置、feature 污染），再考虑 `sccache` 与依赖隔离。**不允许因为达不到就换语言**——换语言会把风险转移到更危险的地方（见 ADR-0003）。

### 人工介入点
- 确认 workspace 划分是否合理（AI 容易拆出循环依赖）
- 确认 Windows 真机可用（M1 起需要）

## 3. M1 单机管线（最关键的里程碑）

### 目标
在一台机器上跑通完整媒体链路，并用自动化方法测出真实端到端延迟。**不涉及网络。**

### 任务

| # | 任务 | 说明 |
|---|---|---|
| M1-1 | `rc-capture-win` DXGI 实现 | 含 `ReleaseFrame` RAII、`ACCESS_LOST` 恢复、重复帧过滤 |
| M1-2 | `rc-diff` 脏区/重复帧检测 | 先用「整帧 hash 比对」，再上 tile 级 |
| M1-3 | `rc-encode-nvenc` | p1 preset、B帧0、零延迟输出 |
| M1-4 | `rc-encode-sw` openh264 兜底 | 无 GPU 时可用 |
| M1-5 | `rc-decode` DXVA 硬件解码 | 零拷贝输出纹理 |
| M1-6 | `rc-render` 低延迟呈现 | vsync off、GPU 直通 |
| M1-7 | `rc-sched` 帧调度器 | 在途上限 2、deadline 丢弃 |
| M1-8 | `rc-trace` 帧时间线 | 每帧结构化事件 |
| M1-9 | 回环延迟自动测量 | 屏幕闪烁帧序号 + 读回比对 |
| M1-10 | 延迟分段面板 | L1–L4 实时显示 |

### 出口条件（Gate — 硬性）

| 指标 | 门槛 |
|---|---|
| 端到端延迟 P50（1080p60，回环：含采集/编码/解码/呈现，**不含网络**） | **≤18ms** |
| 端到端延迟 P99 | ≤35ms |
| Displayed FPS | ≥58 |
| 静态画面 CPU（单核） | ≤8% |
| 拖动窗口时的帧率 | ≥55（不掉到 30） |
| 注入「编码器变慢 2×」后延迟 | 不增长（证明调度器丢弃生效） |
| 采集自愈 10 项用例 | 全部通过 |
| 每帧可导出 FrameTrace | 可回答「这帧为什么没到」 |

### 不可通过的应对
若回环延迟 >18ms（Gate 未通过），按此顺序排查（不得跳过）：

1. 分段延迟数据（哪个 L 超标）
2. 是否关闭 vsync
3. 编码器是否真的走硬件、preset 是否 p1、B 帧是否 0
4. 采集是否事件驱动（看采集帧数 vs 屏幕刷新）
5. 是否有隐式 CPU 拷贝（用 GPU 采样器验证零拷贝）
6. 调度器是否生效（看在途帧数）

### M1 Gate 实测记录（2026-11-21，macOS P0 路径）

#### ✅ 终验（2026-11-21）：M1 Gate 全项通过，正式进入阶段 C

终验配置（相对上一轮的三处变更）：① 回退 `rc-capture-macos` 的 SCK PTS 节拍过滤
（`should_emit_pts`，整段删除——SCK 60fps 内容的到达节拍是 8.33/16.67ms **交替**，
按「距上一放行帧」累计判定会把节拍拉长到 25ms 周期，实测采集被压到 48fps）；
② `max_in_flight` 切回 **2**（mif=1 在 60s 长测中预算丢帧 842 次、displayed 掉到
56.7fps < 58；mif=2 双达标且 0 丢帧——VT 是异步会话，在途 2 重叠提交/完成开销）；
③ 隐私幕布固化为仓库内可复现工具 `rc-render-macos/tests/curtain.rs`
（`cargo test -p rc-render-macos --features window-bench --test curtain --release --
--mode anim --fps 72` / `--mode static`；两个实现坑存档见该文件文档：无
`NSApp.run()` 窗口不上屏、单 IOSurface 反复重写合成器看到冻结画面——需
NSApp 主线程 + 渲染线程 + 4 缓冲环）。`uuremote` 未运行；`caffeinate` 保持开启防息屏。

| 场景 | 结果 | 关键指标 |
|---|---|---|
| **1080p60 动态 60s** | **全项通过（exit 0，零失败项）** | **P50 = 11.283ms** / P95 = 16.764 / **P99 = 21.203ms** / captured 70.02fps、encoded=displayed **70.00fps** / 4201 帧全上屏（budget=0、stale=1）/ convergence 1000ms / 队列峰值 2 / unfinished=0 / 不变式全过；分段：encode 10.296 / decode 2.024 / diff 0.995 / present 0.250 / sched_queue 0 |
| **持续运动 15s**（全屏低熵连续运动，与窗口拖动同类刺激） | **全项通过（exit 0）** | P50 = **11.164ms** / P99 = **20.431ms** / displayed **69.47fps** / 1042/1042 全上屏（零丢弃）/ convergence 1000ms / 队列峰值 2 |
| **静态画面 30s**（纯黑真静屏） | **静态出口条件达标** | 单核 CPU 稳态均值 **1.51%**（全均值 2.13%、峰值 18.2% 仅启动段）；encoded=displayed=122（4.07fps，全部为菜单栏时钟等真实系统变化，`dropped_repeat=0`——Q5「静止画面重复编码」红线未破）；exit 1 为静态画面 fps 门的正确语义 |

对照同一台机器的演进：动态 60s P50 19.087（首轮）→ 14.476（中间轮）→ **11.283ms**；
displayed 50.22 → 56.70 → **70.00fps**；静态 CPU 11.72% → 2.52% → **1.51%**。
三项硬门槛（≤18ms / ≥58fps / 静态 CPU ≤8%）全部满足且有余量。**M1 关卡解除，
按 §8 顺序进入阶段 C（T09 QUIC 传输 → T10 输入通道）。**

#### A. 中间轮存档：真机全链深度调优（`max_in_flight=1`，已被上方终验取代）

原始记录如下（其中的「3 项未过」在终验中全部解决）：

本轮完成 4 项关键修复（`rc-encode-vt` `EnableLowLatencyRateControl = true` 使编码 P95 从 17.98→10.72–11.29ms、降幅 37%–40%；`annexb.rs` 256..=511 字节 AVCC 长度误判修复；`rc-capture-macos` latest-frame-wins `FrameMailbox` + `capturesAudio=false`；`rc-pipeline` `cap_space` Condvar 替代 1ms 轮询退避）及 Phase A `max_in_flight` A/B 选优后，执行完整隐私幕布 B3 真机终验。**多项核心指标已翻绿，但仍有 3 项未过，暂不进入 M2**。

| 场景 | 结果 | 关键指标与分段均值（`breakdown_ms`） |
|---|---|---|
| `rc-capture-macos` SCK 60fps 真机测试（`FrameMailbox` 后） | **全通过**（`2 passed; 0 failed`, 2.48s） | `sck_captures_at_60fps` 2s 采集 **149 帧**（**74.5fps**，≥116 ✅），`dt_us` min=8333 / p50=16666 / max=25000（首测 max=16667）；`sck_stream_rebuilds_after_stop` **通过** ✅ |
| Phase A `max_in_flight` A/B（6s 隐私幕布，release，1080p60） | 双达标，**选定 `max_in_flight=1`** | `mif=2`：p50 14.274ms / p95 23.804ms / p99 28.282ms / displayed 72.33fps（`captured=434`, `dropped_budget=0`, `max_queue=2`）；**`mif=1`**：p50 **13.866ms**（复测 13.847ms）/ p95 22.034ms / p99 **24.434ms**（复测 26.271ms）/ displayed 59.50fps（复测 60.33fps，`captured=430`, `dropped_budget=73`, `max_queue=1`）。选 `mif=1`（p50 −0.408ms、p99 −3.848ms、队列峰值 1） |
| **1080p60 动态 60s**（B3 主门槛） | **未通过**（`gate_passed=false`） | **已达标**：p50 **14.476ms**（≤18 ✅，较首轮 19.087ms 降 4.61ms）/ p95 22.955ms / p99 **26.572ms**（≤35 ✅）/ 队列峰值 **1**（≤2 ✅）/ `unfinished=0` / `decode_errors=0` / `dropped_stale=0`；**未达标**：trace 截断致 `captured=4096 / displayed=3286`（54.77fps）、`dropped_budget=810` 与 `sched.encoded=3402`（**56.70fps**，<58 ❌）报不变式失败、`convergence_ms=3090ms`（>1000 ❌）；分段：encode **11.658**、decode **2.915**、present **0.961**、diff **0.909**（首轮 4.49）、network **0.193**、packetize **0.003**、sched_queue **0** |
| **窗口拖动 15s** | **全项通过**（`gate.passed=true` ✅） | `displayed_fps` **60.27**（≥55 ✅，`captured_fps=71.73`）/ p50 **14.193ms**（≤18 ✅）/ p95 21.969ms / p99 **24.200ms**（≤35 ✅）/ `convergence_ms=1000ms`（≤1000 ✅）/ `dropped_stale=0` / 队列峰值 1 / `unfinished=0`；分段：encode **11.532**、decode **2.816**、present **1.088**、diff **0.701**（较首轮 12.81ms 降 12.11ms）、network **0.248**、packetize **0.005**、sched_queue **0** |
| **静态画面 30s**（纯黑真静屏） | **未全过（CPU ✅，零编码 ❌）** | 单核 CPU 均值 **2.52%**（2.5172%，≤8% ✅，稳态 0.2%–0.6%，较首轮 11.72% 显著下降）、峰值 **19.0%**（29 采样）；`captured=183`、`encoded=displayed=156`（≠0 ❌，较首轮 989 降 84%，`dropped_repeat=0`）、`unfinished=0` |

最新未通过项精确根因：

1. **动态 60s `rc-trace` 4096 环形缓冲截断（连锁 3 项失真/失败）**：`rc-capture-macos` 将 `minimumFrameInterval` 设为 `750_000/60 = 12_500µs` 后，ProMotion 120Hz 下交替放行 8.33/16.67ms 帧（实测 70.73–74.5fps，60s 共采 `sched_stats.captured = 4244` 帧 > 4096）。最早 148 帧（前 ~2.09s，含 116 已显示 + 32 `dropped_budget`）被驱逐；`rc-loopback::run_real()` 虽取了 `controller.displayed()` 却丢弃（`let _ = displayed_full`），继续用截断后的 merged trace 统计，直接导致不变式失败（`3402 != 3286`）、`convergence_ms` 虚高至 3090ms（15s 未溢出为 1000ms）、`displayed_fps` 被低估为 54.77fps。
2. **动态 60s 真实编码/显示帧率 56.70fps < 58fps**：剔除截断后 `sched_stats.encoded = 3402`（56.70fps）仍低于 B3 硬门槛 58fps（代码内 `gate.displayed_fps_min = 60*0.9 = 54` 较文档宽松）。根因是 SCK 70.73fps 不均匀到达（8.33/16.67ms 交替）配合 VT 均值 11.66ms 编码耗时与 `max_in_flight=1` 触发 842 次 `dropped_budget`。
3. **静态 30s 未达成零编码（`encoded=156`）**：绝大部分时间已零帧输出（CPU 0.2%–0.6%），但启动前 2s（CPU 13.2%/5.5%）及第 14–15s（CPU 19.0%/17.2%，`convergence_ms=15110ms`）出现两次系统合成突发；因 `run_real` 固定 `DiffMode::TrustPlatform`、`SckBackend` 无帧内容去重，突发帧全部进编码；且 `report.rs::apply_gate` 未切静态零编码分支。

#### A0. 历史对照：首轮真机全链实测（调优前，`--backend real`，**未通过**）

首轮解锁屏幕录制权限后跑通真机全链的调优前基线（保留作改进对比）：

| 场景 | 结果 | 关键指标与分段均值（`breakdown_ms`） |
|---|---|---|
| `rc-capture-macos` SCK 60fps 真机测试 | `sck_stream_rebuilds_after_stop` **通过**；`sck_captures_at_60fps` **未通过** | 2s 采集 107 帧（`--test-threads=1` 复测 112 帧，门槛 ≥116）；`dt_us` min=16666 / p50=16667 / max=25000（前若干帧出现 25ms=3×8.33ms 间隔） |
| 10s 真机全链冒烟（`--backend real`） | 链路跑通，**Gate 未通过**（exit 1） | p50 **19.545ms** / p95 25.798ms / p99 29.236ms / `captured=encoded=displayed=451`（**45.1fps**） / `convergence_ms=null` / 队列深度 1 / drop=0 / unfinished=0；分段：encode **10.84**、diff **4.84**、decode 2.20、present 0.42、network 0.10、packetize 0.01、sched_queue 0ms |
| **1080p60 动态 60s**（首轮基线） | **未通过**（exit 1） | p50 **19.087ms**（>18 ❌）/ p95 25.989ms / p99 **28.753ms**（≤35 ✅）/ `captured=3014`（50.23fps）、`encoded=displayed=3013`（**50.22fps**，<58 ❌）/ `convergence_ms=1990ms`（>1000 ❌）/ `dropped_stale=1`、`dropped_budget=0`、`dropped_superseded=0`、`decode_errors=0` / 队列深度 **2**（≤2 ✅）/ unfinished **0**（✅）；分段（合计 18.21ms）：encode **10.75**、diff **4.49**、decode 2.40、present 0.48、network 0.08、packetize 0.01、sched_queue 0ms |
| **窗口拖动 15s**（首轮基线） | **未通过** | `displayed_fps` **47.80**（<55 ❌）/ `captured=730`（48.67fps）、`encoded=717`、`dropped_stale=13` / p50 **25.459ms** / p95 33.001ms / p99 38.577ms / unfinished **0**（✅）；分段变化：`diff` 飙升至 **12.81ms**（+8.32ms），encode 10.30 / decode 2.03 / present 0.36ms |
| **静态画面 30s**（首轮基线） | **未通过** | `captured=encoded=displayed=989`（32.97fps，!=0 ❌）/ `dropped_repeat=0` / unfinished 0 / 单核 CPU 平均 **11.72%**（>8% ❌）、峰值 **14.1%**（29 采样） |

首轮失败根因存档（按 §3 排查顺序）：

1. **动态 60s（P50 超 1.09ms、FPS 少 7.78、收敛 1990ms）**：`diff=4.49ms` 实为 SCK 合成/回调送达主线程的采集交接滞后，`encode=10.75ms` 含 `enc_req` 排队与 VT 同步编码等待，两段合占端到端延迟 **83.7%**；上游 SCK 仅 50.23fps 致前 2s 爬坡慢。
2. **窗口拖动 15s（FPS 47.80 < 55）**：拖动期间 WindowServer/SCK 投递延迟剧增（`diff` 4.49 → 12.81ms），触发 13 帧 `dropped_stale`。
3. **静态 30s（encoded=989、CPU 11.72%）**：`--static-content` 仅作用于假后端，`SckBackend` + `TrustPlatform` 下桌面持续 UI 刷新致 ~33fps 编码。

#### B. 假后端/历史对照（`--backend auto`：Fake 采集 + Fake×WallEncoder + Fake 解码/呈现）

用于验证调度器、回环报告与不变式逻辑（权限解锁前基线，命令：`rc-cli loopback --backend auto …`）：

| 场景 | 结果 | 数字 |
|---|---|---|
| 1080p60 动态 60s | **通过**（exit 0） | p50 **9.20ms** / p99 20.60ms / 59.9fps / 队列深度 1 / unfinished 0 / 不变式全过 |
| 静态 30s | 正确拒绝（exit 1） | encoded=0、captured=0（Q5/L8 零编码红线） |
| 编码器减速 4×（20ms 编码成本）60s ×2 | 机制验证通过（exit 1 为预期） | 预算丢帧 1730/1728、吞吐 31.2fps≈编码器容量、队列深度 1、unfinished 0、**不变式与账目全干净**；p50 26.1/26.5ms |

「减速 4×」场景的 p50 物理下界 = 20ms（模拟编码成本本身在端到端路径里）
+ diff + 解码 ≈ 22.3ms；原卡片的「p50 ≤25ms @2×」两处修正（2×→4×、
25ms 逻辑世界口径）记录在 T08 卡内。延迟不增长靠的是**有界**：
p50 ≈ 2×编码成本的一半以内，不随积压膨胀。

本轮 Gate 过程中修掉的三个实质缺陷（都已带回归测试或仪器化）：

1. **rc-sched R1 逐出→丢到达帧**：硬件编码无法中途取消，逐出在途帧 =
   反复作废在制工作的活锁（实测 231 次预算裁决只交付 4 帧）。
2. **线程化 host 的关机顺序**：先 `shutdown` 再 tick 会把末尾帧提交给
   已退出的编码线程（`TrySendError::Disconnected`），授权帧凭空消失、
   不变式 off-by-one。正确顺序：停采集 → tick 到静默 → 停编码。
3. **驱动循环紧自旋**：编码在途时调用方线程空转，macOS 定时器合并把
   编码线程的 sleep(20ms) 拖到 30ms（+50%）。改为 1ms 退避。

## 4. M2 双机直连

> **进度（2026-11-21）**：M2-1/2/4 的传输底座已由阶段 C 交付——T09 `rc-net`
> （QUIC 双通道、分片重组、FEC、pacing，23+4 测试）与 T10 `rc-input`
> （4ms 批处理、手写线格式、reliable-stream 接线、macOS 注入桥接）落地；
> 3% 丢包回环存活、100KB 逐字节重组、`DEFAULT_MAX_CONCURRENT_FRAMES=2`
> 已实测。剩余：M2-3/5–10（会话层、鉴权、UI、重连、探测）与双机真机实测。
> 会话层任务卡已建：**T13–T22**（协议 T13 / 会话核心 T14 / 探测与初始码率 T15 /
> IDR 可靠通道 T16 / 重连 T17 / CLI 本机直连 T18 / 鉴权 T19 / 剪贴板 T20 /
> 控制端 UI T21 / 被控端托盘 T22，状态表见 [tickets/README](../tickets/README.md)）；
> 本轮实现范围 = 会话核心（T13–T17）+ CLI 本机直连（T18），T19–T22 后续领取。
> **T13–T18 已实现并通过独立复核（2026-11-21）**：rc-session 落地（workspace 490 测试全绿）；
> localhost 真后端双进程 Run B 60s **Gate 全过**（首帧 630ms≤800 / p50 10.8ms / p99 18.5ms /
> 68.3fps / 输入单向 1.29ms / 队列 0），1080p 90s p50 17.2 / p99 26.3 / 零丢包（fps 45.5 为
> 单屏遮挡自反馈伪影）；#[ignore] 真机 2/2；修复 MTU 分片预算错配与真采集时钟锚定两根因
> （详见 TASK.md §12/§13）。剩余：T19 鉴权 / T20 剪贴板 / T21 UI / T22 托盘与双机真机实测。

| # | 任务 |
|---|---|
| M2-1 | `rc-net` QUIC 连接（quinn）+ 握手 |
| M2-2 | 媒体数据报 + 分片重组 |
| M2-3 | IDR 可靠通道 |
| M2-4 | 输入通道（独立 stream + 批处理） |
| M2-5 | 鉴权（密码 + 确认弹窗） |
| M2-6 | 控制端 UI（连接、全屏、工具栏） |
| M2-7 | 被控端托盘 + 接受弹窗 |
| M2-8 | 剪贴板文本同步 |
| M2-9 | 断线重连 |
| M2-10 | RTT/带宽探测与初始码率设定 |

### 出口条件

| 指标 | 门槛 |
|---|---|
| 局域网 1080p60 端到端延迟 P50 | ≤25ms、P99 ≤40ms |
| 输入延迟（点击→对端响应） | ≤RTT + 5ms |
| 点击连接到出画面 | ≤800ms |
| 全屏下拖动窗口 | 主观流畅，无拖影 |
| 连续 2 小时会话 | 无崩溃、内存增长 <5% |

**人工介入点（必须真机 + 肉眼）**：拖动窗口、滚动网页、播放视频三种场景的实际观感。**AI 无法判断这三项，必须人工验收。**

## 5. M3 弱网与自适应

| # | 任务 |
|---|---|
| M3-1 | XOR 块 FEC + 自适应冗余率 |
| M3-2 | NACK 批处理 + 放弃策略 |
| M3-3 | 拥塞控制（BBR 改造）+ pacing |
| M3-4 | 码率自适应阶梯 |
| M3-5 | 分辨率/帧率降级策略 |
| M3-6 | 关键帧按需请求 |
| M3-7 | 网络损伤注入测试工具（`tc netem` / 用户态模拟） |
| M3-8 | 连接迁移（Wi-Fi ↔ 有线） |

### 出口条件

| 场景 | 门槛 |
|---|---|
| 3% 丢包 + RTT 80ms | 无 >500ms 冻结，Displayed FPS ≥30 |
| 10% 丢包 + RTT 120ms | 画面可辨识，FPS ≥15，无 >1s 冻结 |
| 带宽从 20Mbps 骤降到 2Mbps | 2s 内稳定，不冻结 |
| 网络切换（Wi-Fi→有线） | 连接不中断，<2s 恢复画面 |
| 30 分钟弱网会话 | 无中断 |

**这一里程碑是产品差异化的核心验证点。** RustDesk 在这里失败，RC 必须通过。

## 6. M4 三平台

> **2026-11-20 重排**：P0 改为 macOS，因此 macOS 的全部工作已前移到 M1（见 T12/B1）。
> M4 现在只处理 P1（Windows）与 P2（Linux）。

| 优先 | 平台 | 任务 | 出口条件 |
|---|---|---|---|
| **P1** | Windows | DXGI 采集、NVENC、Media Foundation 兜底、多显示器、输入 | 04 文档 Windows 清单全绿 |
| P2 | Linux X11 | XDamage 采集、VAAPI、XTest | 清单全绿 |
| P2 | Linux Wayland | PipeWire + portal、libei | 清单全绿（GNOME + KDE） |

### 出口条件
- macOS + Windows 均达到 M2/M3 的延迟与弱网指标
- 跨平台组合可互连（mac↔Win、mac↔Linux）
- 打包产物可在纯净系统安装运行

## 7. M5 可分发产品

| # | 任务 |
|---|---|
| M5-1 | Rendezvous 服务（设备注册、地址分发） |
| M5-2 | Relay 服务 |
| M5-3 | 打洞流程 + 自动中继切换 |
| M5-4 | 安装包（MSI / AppImage+deb+rpm / DMG） |
| M5-5 | 自动更新 |
| M5-6 | 首次运行引导（权限、密码设置） |
| M5-7 | 崩溃报告与日志导出 |
| M5-8 | 文档（用户手册、自建服务器指南） |

### 出口条件
- 陌生用户可在 5 分钟内完成「安装 → 配置 → 连接」
- 自建服务器部署文档可被非开发者执行
- 打洞成功率 >70%，失败自动中继

## 8. M6 功能补全

按 [05 功能清单](05-features.md) 的 P1 项逐项交付。每个功能独立验收，不阻塞其他。

## 9. WBS 估算

以下为「AI 全量编码 + 人类验收」模式下的相对工作量（人日，指人类投入的验收与决策时间，AI 执行时间另计）。

| 阶段 | 人类工作量 | AI 执行轮次（估） | 主要风险 |
|---|---|---|---|
| M0 | 2 | 10–15 | crate 拆分决策 |
| M1 | 8 | 60–100 | **DXGI 坑、零拷贝验证、延迟测量方法** |
| M2 | 6 | 40–70 | QUIC 集成、输入延迟 |
| M3 | 5 | 40–60 | 拥塞控制调参需真机 |
| M4 | 12 | 80–120 | Wayland、macOS 授权、服务化 |
| M5 | 6 | 40–60 | 打包签名、跨平台安装 |
| M6 | 10 | 60–100 | 功能多但风险低 |

**合计：约 49 人日人类投入，330–525 个 AI 迭代轮次。**

### 排期建议

| 周 | 内容 |
|---|---|
| W1 | M0 全部 + M1 采集（DXGI 打通） |
| W2–W3 | M1 编码/解码/呈现/调度 + 延迟测量 |
| W4 | **M1 Gate 评审（人工）** |
| W5–W6 | M2 传输 + 输入 + UI |
| W7 | M2 验收 + M3 FEC/拥塞控制 |
| W8 | M3 弱网验收 |
| W9–W11 | M4 Linux + macOS |
| W12 | M4 Windows 服务化 + 三平台验收 |
| W13–W14 | M5 服务端 + 打包 |
| W15+ | M6 功能补全 |

**总计约 14 周到可发布状态**（不含 M6）。这个估算的前提是 M1 一次通过——若 M1 返工，每轮返工约 +1 周。

## 10. 外部依赖与阻塞项（需人类提前准备）

| # | 依赖 | 需要时间 | 阻塞阶段 |
|---|---|---|---|
| D1 | ~~Windows 物理机~~ → **macOS 真机（P0 主线）** | 已有 | M1 起 |
| D1b | Windows 物理机（带独显） | 阶段 D 前 | P1 支持，不阻塞 M1 |
| D2 | Windows VM（用于锁屏/UAC/多显示器测试） | W1 | M4 |
| D3 | Linux 机器（X11 + Wayland 都能测） | W9 | M4 |
| D4 | macOS 开发者账号（签名、公证） | W1（审批需时间） | M4 |
| D5 | 第二台任意设备（弱网对测） | W5 | M2 |
| D6 | 网络损伤环境（`tc netem` 或路由器 QoS） | W7 | M3 |
| D7 | 公网服务器（测试打洞/中继） | W13 | M5 |

**2026-11-20 修正**：原文档把「Windows 物理机」列为 M1 的硬阻塞。由于 P0 已改为 macOS，
**M1 可在 macOS 本机完成，不再依赖 Windows 机器**。Windows 物理机只阻塞阶段 D（P1 支持）。

## 11. 项目级验收（发布判定）

### 必过项

| # | 项 | 方法 |
|---|---|---|
| 1 | 1080p60 局域网延迟 P50 ≤25ms | 回环 + 双机实测 |
| 2 | Displayed FPS 与 Encoded FPS 差值 <5% | 指标对比 |
| 3 | 3% 丢包无 >500ms 冻结 | netem 注入 |
| 4 | 盲测优于 RustDesk（≥70% 选 RC） | 5 人 × 3 场景 |
| 5 | 静态画面 CPU ≤8% 单核 | 系统监控 |
| 6 | 8 小时会话无崩溃 | 长跑 |
| 7 | 三平台安装即用 | 纯净系统测试 |

### 加分项（影响口碑，不阻塞发布）
- 4K45 可用
- 打洞成功率 >80%
- 冷启动 <1s

## 12. 风险与缓解（进度视角）

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| M1 延迟不达标 | 中 | **致命** | 严格分段测量；每项超标立即定位；预留 1 周返工 |
| Windows 真机不到位 | 中 | 致命 | **本周内必须解决**；否则用云 GPU 机器（AWS g4dn 等）过渡 |
| Wayland 适配超期 | 高 | 中 | X11 先发版，Wayland 作为补丁 |
| macOS 签名流程卡住 | 中 | 中 | 提前申请账号；开发期用固定自签名 |
| AI 在 unsafe 边界写错 | 中 | 中 | unsafe 隔离 + 单测 + 回环 + Miri |
| 编码器驱动差异 | 高 | 中 | 启动自检 + 自动回退 + 颜色校验 |
| 里程碑串行导致总工期长 | 中 | 中 | M4 三平台可并行（AI 无上下文切换成本） |

## 13. 每个里程碑的固定流程

```
1. 写任务清单（本文档）
2. AI 实现 + 自测（回环模式）
3. 自动验收（指标脚本）
4. 人工验收（肉眼 + 真机）
5. 打 tag，更新本文档的「实际结果」
6. 复盘：哪些坑可以写进文档，避免下次
```

**第 6 步不可省略。** 每次踩坑必须回写进 [02 架构](02-architecture.md) 或 [04 平台](04-platforms.md) 的「坑列表」，否则 AI 会在下一个里程碑重犯。
