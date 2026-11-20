# ADR-0004: 基于 RustDesk 重构客户端 —— 可行性评估

- 状态：**已评估，结论为「不做 fork，做选择性借鉴」**
- 日期：2026-11-20
- 相关：[ADR-0001](0001-language-choice.md)、[ADR-0003](0003-final-rust-decision.md)
- 调研方法：实际克隆 RustDesk master（2026-11-20，commit 前推 `05:06Z`），阅读 `src/server/video_qos.rs`、`src/server/video_service.rs`、`src/client/io_loop.rs`、`libs/scrap/src/common/dxgi.rs`、`libs/hbb_common/src/stream.rs`，规模统计基于 `depth=1` 克隆。

## 1. 结论先行

**不 fork RustDesk 重构。但这次调研有巨大价值：它证明了我们原先的三个假设，其中两个是错的，一个是对的。**

| 我们原来的假设 | 调研结果 | 影响 |
|---|---|---|
| 采集是固定周期全帧轮询 | ❌ **错**。RustDesk 已用 DXGI 事件驱动 + `WouldBlock` 语义 | 我们的 `rc-diff` 价值下降，但仍有用（macOS/Wayland 无脏区 API） |
| 编码参数是质量优先 | ⚠️ **部分错**。有 `quality` 比值控制，但默认面向质量 | 我们的「延迟优先参数」仍然有效，但不是颠覆性的 |
| **没有 deadline-aware 调度** | ✅ **对，且非常严重** | **这仍是唯一真正的护城河** |

而且我找到了比「没有调度器」更具体、更致命的东西——**导致「帧率上限高但实际低」的三个精确机制**，见 §3。这三个机制是 bug 级别的设计选择，不是架构缺陷。

**所以「基于 RustDesk 重构」的正确用法不是 fork 它，而是：把这三个机制作为我们设计的反面教材，并且明确知道——我们不需要重写 19 万行，只需要在关键路径上不犯这三个错。**

## 2. RustDesk 的真实规模（fork 的成本）

| 部分 | 行数 | 说明 |
|---|---|---|
| `src/` | 154,261 | 主程序 |
| `libs/` | 35,574 | 含 `hbb_common`（子模块，另 14,088） |
| `flutter/` | 84,865 | Dart UI |
| **Rust 合计** | **~190,000** | |
| **含 Flutter** | **~275,000** | |

平台相关代码分散在 39 个文件里。`libs/scrap` 17,000 行，其中 DXGI 只有 280 行，剩下全是 X11/Wayland/Quartz/Android/编码器/VRAM 路径。

**这个规模的现实含义：**

- AI 无法「重构」这个体量。它可能改对 10 个文件，改坏另外 5 个，而你没有能力 review 19 万行来找出来。
- 我们的 CR 骨架目前 3,569 行，完整实现预计 15,000–25,000 行。**比 RustDesk 小一个数量级，因为不做 UI、不做移动端、不做文件/打印/端口转发/终端。**
- 「基于 RustDesk」意味着继承它所有 39 个平台分支的坑，以及它 5 年积累的兼容性补丁。

## 3. 找到的三个帧率杀手（这是本次调研最有价值的产出）

### 杀手一：`INIT_FPS = 15`，且帧率恢复极慢

`src/server/video_qos.rs:54`：

```rust
pub const FPS: u32 = 30;        // 默认目标
pub const MIN_FPS: u32 = 1;
pub const MAX_FPS: u32 = 120;   // 这就是「上限很高」
pub const INIT_FPS: u32 = 15;   // ★ 每个新连接从 15fps 开始
const MIN_AUTO_FPS: u32 = 5;
```

`adjust_fps()`（`video_qos.rs:817`）：

```rust
let mut fps = self.users.iter()
    .map(|u| u.1.delay.fps.unwrap_or(INIT_FPS))   // 未初始化 → 15
    .min()                                        // ★ 取所有观众的最小值
    .unwrap_or(INIT_FPS);

// 新连接的前 1 秒强制限制在 15fps
if self.users.values().any(|u| {
    u.joined_at.is_some_and(|j| self.since(j).as_secs() < 1)
}) {
    fps = fps.min(INIT_FPS);
}
self.fps = fps.clamp(MIN_FPS, highest_fps);
```

**后果**：每次连接的前 1 秒必然是 15fps。这不是网络问题，是硬编码。

### 杀手二：帧率爬升是「+1」，而下降是「除」

`video_qos.rs:546-568` 的爬升逻辑：

```rust
if avg_delay < 50 {
    let mut step = if fps < normal_fps { 1 } else { 0 };
    if quick_increase_fps_count >= 3 {
        step = 5;                      // 每 3 个好样本 +5
    }
    fps = min_fps.max(fps + step);
}
```

下降逻辑（`video_qos.rs:523`）：

```rust
let devide_fps = ((fps as f32) / (avg_delay as f32 / DELAY_THRESHOLD_150MS as f32)).ceil() as u32;
```

**这是不对称的**：一次延迟抖动可能把 60fps 直接除掉大半，但恢复要靠 `+1` / `+5` 慢慢爬。

**关键：`TestDelay` 探针间隔是 1 秒。**

`src/server/connection.rs:510`：

```rust
const TEST_DELAY_TIMEOUT: Duration = Duration::from_secs(1);
```

所以帧率调整的采样周期是 **1 Hz**。从 15fps 爬到 60fps，最快也要：
- `+5` 每 3 个样本 → 约 9 次调整 → **9 秒**
- 若遇到一次抖动回退，重新爬 → **再 9 秒**

**这就是用户说的「帧率上限能给的很高，但实际相当低」的精确原因。** 不是硬件不行，是控制回路以 1Hz 运行且恢复缓慢。

### 杀手三：客户端队列 120 帧，没有任何 deadline 丢弃

`src/client.rs:114`：

```rust
pub const VIDEO_QUEUE_SIZE: usize = 120;
```

`src/client/io_loop.rs:1723`：

```rust
if video_queue.force_push(vf).is_some() {
    // 队列满时挤掉最旧的
}
```

**120 帧 = 60fps 下的 2 秒缓冲。**

服务端还有 `TestDelay` 探针注释自己承认了这一点（`video_qos.rs:43-44`）：

> `TestDelay` shares the video stream, so it measures the queue **in front of it** rather than the path RTT.
> （TestDelay 与视频共享流，所以它测量的是它前面的队列，而不是路径 RTT。）

即：RustDesk 自己知道延迟反馈被队列污染了。这个队列深度决定了「用户看到的画面比现实晚多久」，而没有任何机制在队列深时丢帧。

### 杀手四（附加）：服务端重复编码最多 10 次

`video_service.rs:729`：

```rust
let mut repeat_encode_counter = 0;
let repeat_encode_max = 10;
```

`video_service.rs:935`：

```rust
if repeat_encode_counter < repeat_encode_max {
    repeat_encode_counter += 1;
    handle_one_frame(...)   // 对同一份 yuv 反复编码
}
```

捕获超时（画面无变化）时，**对同一帧重复编码最多 10 次**。这在设计上是「保持码流活跃」，但在延迟上是纯浪费——10 次编码的成本本可以省下。

## 4. 为什么「fork 重构」仍然不是好路径

### 4.1 AGPL-3.0 是硬约束

`LICENCE` 是 **GNU AGPL-3.0**。这不是「开源」，是**网络服务强 copyleft**：

| 你的用法 | 后果 |
|---|---|
| 自用、不分发 | 无义务 |
| 分发给客户（哪怕内部） | **必须以 AGPL 开源你的全部修改** |
| 作为 SaaS 提供服务 | **必须向所有用户提供完整源码**（AGPL §13） |
| 想闭源商用 | **不可行**，除非购买商业授权 |

如果 RabbitControl 的目标是「做一个自己的产品」，AGPL fork 会让这条路基本堵死。这是比技术成本更硬的约束。

### 4.2 19 万行里的绝大多数我们不需要

我们不做：Flutter UI（84,865 行 Dart）、移动端、文件传输、远程打印、端口转发、终端、VRAM 高级路径、多用户并发控制。

我们要的是：**采集 + 编码 + 调度 + 传输 + 解码 + 呈现 + 输入**，这正好是 RustDesk 里最难改、耦合最深的部分（`video_qos` 979 行 + `video_service` 1,592 行 + `io_loop` 3,325 行，且互相引用）。

**重构最难的部分，比从零写这部分更贵。**

### 4.3 「改 QOS」看起来简单，实际不行

最诱人的想法：「只改 `video_qos.rs` 不就行了？」

问题在于：
- `fps` 是**所有观众的最小值**（`adjust_fps` 取 `.min()`）
- 新连接强制 15fps 的逻辑写在 `adjust_fps` 里，且服务于「多观众」场景
- 帧率与 `bitrate` 的联动在 `limit_fps_change` / `needs_bitrate_reduction` 里，二者互相依赖
- 客户端还会反向限制（`io_loop.rs:1573`：`min_decode_fps * 9 / 10`），服务端改了客户端不配合等于无效
- `static_refresh` 820 行与 fps 联动

**这是一个闭环控制系统，单点修改会破坏稳定性。** 而「稳定性」正是它存在的理由——它防的是「网络差时把画面打死」。

改成 deadline 丢弃，等于**换掉整个控制哲学**：从「保平稳」改成「保新鲜」。这在 RustDesk 的架构里是伤筋动骨的。

## 5. 但这次调研修正了我们的设计

**必须承认：我们原来的 ADR-0001 对 RustDesk 的三条归因，有一条是错的。**

| 原假设 | 修正 |
|---|---|
| 采集全帧轮询 | RustDesk 的 DXGI 已是事件驱动（`dxgi.rs:45` `WouldBlock`）。我们的 `rc-diff` 只在 macOS/Wayland 有意义 |
| 编码质量优先 | 有 `quality` 比值（0.2–40.0），配合 ABR 动态调整。不是纯质量优先 |
| 无 deadline 调度 | **确认，且比想象严重**（120 帧队列 + 1Hz 反馈 + 不对称爬升） |

**因此 `docs/02-architecture.md` 需要修正：**

1. 「采集事件驱动」不是差异化点，是**基线要求**（RustDesk 已有）——不改，但要降低它在营销叙事里的权重
2. 「编码延迟优先」是**参数级优化**，不是架构优势——仍然要做，但收益从「翻倍」降到「+20~30%」
3. **「deadline-aware 调度」是唯一真正的护城河**，且我们的设计（在途上限 2 帧）对比 RustDesk（队列 120 帧）是 60 倍的差异

## 6. 决策

### 6.1 不做

- ❌ **不 fork RustDesk**（AGPL + 19 万行 + 控制闭环耦合）
- ❌ 不复制其代码（许可污染）
- ❌ 不试图「只改 video_qos」（闭环依赖太多）

### 6.2 要做

- ✅ **维持原计划：从零实现，但规模小一个数量级**（15k–25k 行 vs 190k）
- ✅ **把这四个「杀手」写进设计文档作为明确的反面教材**，并要求每个对应模块的测试覆盖它
- ✅ **借鉴其正确的部分**：
  - `hbb_common/src/stream.rs` 的 `Stream` 枚举抽象（Tcp/WebSocket/WebRTC）—— 这印证了我们的 `Transport` trait 设计
  - DXGI 的 `WouldBlock` 语义（我们 T03 已写对）
  - FEC/ABR 思路（我们已有）
- ✅ **新增一条验收指标**：「连接到 60fps 的收敛时间 ≤ 1 秒」，直接对应杀手一、二

### 6.3 新增的设计要求（写进任务卡）

| # | 要求 | 反面对照 |
|---|---|---|
| Q1 | **首帧即目标帧率**，不设 `INIT_FPS` | RustDesk 从 15 起步 |
| Q2 | 帧率调整**基于本地 deadline，不依赖网络探针周期** | RustDesk 依赖 1Hz TestDelay |
| Q3 | 接收端队列**深度 ≤ 2 帧**，超过立即丢旧帧 | RustDesk 120 帧 |
| Q4 | 帧率**上升与下降对称**（都按 deadline 判定，无加减速差） | RustDesk +1 vs 除法 |
| Q5 | **捕获无变化时不重复编码**（只发心跳/空帧标记） | RustDesk 重复编码 10 次 |
| Q6 | 延迟反馈**必须扣除队列排队时间**，测量真实路径 RTT | RustDesk 自己承认被污染 |

**Q3 单独就足以构成产品差异。** 120 帧 vs 2 帧的队列深度，差 60 倍。

## 7. 如果一定要走 fork 路线，最小可行方案

假设忽略 AGPL（例如纯自用）：

| 步骤 | 工作量（AI 执行，人类验收） | 风险 |
|---|---|---|
| 1. 剥离 Flutter，只保留 Rust core | 中（84k 行 Dart 要摘干净） | 高（构建系统复杂） |
| 2. 重写 `video_qos` 的帧率控制 | 中（979 行，但是闭环） | **高（破坏稳定性）** |
| 3. 客户端队列 120 → 2 帧 | 小（改常量 + 丢帧逻辑） | 中（上层假设队列非空） |
| 4. 去 TestDelay 依赖，改本地 deadline | 中 | 高 |
| 5. 三平台回归测试 | **大** | 高（39 个平台分支） |

**结论：前 4 步合计约等于重写媒体管线的 60%，第 5 步是最贵的部分且无法避免。**
这与从零实现的工作量接近，但多背了 19 万行历史包袱和 AGPL。

## 8. 可验证的下一步（1 天）

**在写任何代码前，先验证 §3 的三个杀手是否真的成立。** 方法：

```bash
# 1. 下载 RustDesk 官方发行版
# 2. 两台机器直连（不走中继）
# 3. 用 rc-trace 的思路，记录服务端日志里的 qos_trace 行
#    （RustDesk 已内置该日志，见 video_qos.rs:583 的 qos_trace）
# 4. 观察：连接后前 1 秒 fps 是否 = 15
# 5. 观察：从 15 升到 60 需要多久
# 6. 观察：拖动窗口时 fps 波动范围
```

如果实测符合 §3，则本 ADR 的结论成立，按原计划走。
如果实测不符（例如新版已修复），则需要重新评估——但即便如此，AGPL 与 19 万行的结论不变。

**这项验证不阻塞开发**：T02/T06/T07/T09 不需要它。只有 T08（M1 Gate）之前值得做完。

## 9. 一句话总结

> **fork RustDesk 是在解决一个我们不需要解决的问题（195k 行 + AGPL），同时继承我们要解决的那个问题（帧率控制闭环）。**
> **正确的做法是读它的源码，找出它为什么慢，然后不在自己的代码里犯同样的错——这件事已经做完了，成本是 1 次调研而不是 3 个月重构。**
