# ADR-0003: 语言终选 —— Rust（媒体管线必须 in-process）

- 状态：**已决**
- 日期：2026-11-19
- 取代：ADR-0001 §5 决策表、ADR-0002 §4/§5 方案 B'
- 相关：[ADR-0001](./0001-language-choice.md)、[ADR-0002](./0002-ai-driven-iteration-loop.md)

## 1. 决策

**客户端媒体管线（采集 → 编码 → 调度 → 传输 → 解码 → 呈现 → 输入注入）使用 Rust，单进程内完成。**
**服务端（rendezvous / relay / API / 管理后台）语言不作为本决策的一部分，可独立用 Go 实现。**

## 2. 决定性约束

产品定位要求端到端延迟进入 **20ms 级（局域网）/ 80ms 级（广域网弱网）**。这个约束排除了 FFmpeg 子进程方案：

| 方案 | 延迟下限（实测乐观值） | 是否满足 |
|---|---|---|
| Rust in-process + DXGI + NVENC | **15–25ms** | 满足 |
| FFmpeg 子进程（管道传 Annex-B） | **35–50ms**，保守 60–80ms | **不满足** |
| 纯 Go cgo 直连 | 理论等同 in-process，但 AI 无法自愈运行时崩溃 | 不选 |

FFmpeg 子进程方案的延迟下限来自三处强制开销，且无法消除：

1. 进程间管道至少一次完整内存拷贝（1080p NV12 ≈ 3.1MB/帧，60fps ≈ 187MB/s）
2. FFmpeg 内部有输入缓冲与 `fflush` 语义，无法做到「一帧进、一帧出」
3. 无法给编码器逐帧传递脏区、ROI、强制 IDR 等控制信息，只能靠外部信号，引入至少一个帧间隔的延迟

**结论：延迟要求与进程隔离不兼容。**

## 3. 关于「AI 能造轮子」的判断

ADR-0002 已确认 AI 能写出 DXGI / NVENC 的绑定代码。本 ADR 补充确认：**在这个项目里，Rust 的绑定代码比 Go 的绑定代码更安全**，因为

- `windows` crate 为 COM 提供类型化封装，vtable 偏移错误在编译期被拒绝；Go 的裸 syscall 写错则直接段错误
- Rust 的所有权让「duplication 对象失效后仍被使用」这类 `DXGI_ERROR_ACCESS_LOST` 相关的生命周期错误在编译期暴露
- `unsafe` 边界可以被强制收窄到少数几个模块，并单独 review / 单测 / fuzz

即：**同样让 AI 造轮子，Rust 让错误的暴露点前移到编译期，这恰好是 AI 唯一能自主闭环修复的地方。**

## 4. 明确接受的成本

| 成本 | 缓解措施 | 验收线 |
|---|---|---|
| 编译慢 | crate 拆分 + lld + `cargo check` 内循环 + sccache | 单文件改动 `cargo check` ≤3s |
| 测试慢 | `cargo nextest` + 分层测试 | 受影响 crate 测试 ≤15s |
| 磁盘占用大 | 依赖 `opt-level=2` 单次编译 + `cargo sweep` + CI 独立 target | `target/` ≤10GB |
| AI 试错轮次多 | 架构上规避借用冲突（见下） | — |

架构上的规避手段（**这是降低 AI 成本的真正手段，比换语言有效**）：

1. **FFI 边界极窄**：`*-sys` 绑定层与业务层严格分离，业务层不出现 `unsafe`
2. **状态集中**：会话状态机放在单一拥有者结构体内，异步任务通过消息传递而非共享借用
3. **帧数据用引用计数**（`Arc<Frame>`）+ 无锁槽位交换，避免在热路径传递长生命周期借用
4. **类型化错误**：`thiserror` + 明确的错误枚举，禁止 `Box<dyn Error>` 跨模块传播

## 5. 决策的可逆性

保留一个逃生舱：`CaptureBackend` / `EncoderBackend` / `DecoderBackend` 定义为 trait，平台实现是其中一个 impl。

- 若某个平台的原生 API 路线失败（如 Linux Wayland 的 dmabuf 路径），可只替换该平台的 impl
- 若未来需要 Web 客户端，`DecoderBackend` 可有 WASM 实现
- 若未来服务端换语言，线协议（protobuf）是唯一契约

## 6. 结果

- 客户端：Rust workspace，crate 划分见 [07-engineering-velocity](../design/07-engineering-velocity.md)
- 服务端：**Go 或 Rust 均可**，不在第一里程碑决策范围内；接口先按 protobuf 定死
- 所有媒体层代码必须可通过「回环模式」在单机自测，不依赖第二台设备
