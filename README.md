# RabbitControl

低延迟远程桌面控制软件。核心命题：**打赢 RustDesk 的实际帧率与延迟**。

## 从这里开始

**→ [TASK.md](TASK.md) —— AI 任务书（唯一入口）**

TASK.md 是自包含的：项目定位、六条铁律、技术栈、任务顺序、完成定义、禁止事项全在里面。
一个 agent 读完它就能开工，不需要读其他文件。

## 快速验证环境

```bash
cargo check --workspace     # 首次约 52s（含依赖）
cargo test --workspace      # 107 测试，约 1.4s
```

## 当前状态

| 项 | 状态 |
|---|---|
| 基础 crate（types/diff/sched/fec/proto/trace） | ✅ 3574 行，107 测试全绿 |
| 媒体管线（采集/编码/解码/呈现/传输） | ⬜ 未开始 |
| 当前阶段 | 阶段 A：立起测量骨架 |

## 文档结构

```
TASK.md                     ← AI 唯一入口（权威）
AGENTS.md                   ← 多 agent 编排纪律
docs/reference/             ← 参考材料（31 篇，按需查阅）
├── adr/                    决策记录（为什么选 Rust、为什么不 fork RustDesk）
├── design/                 设计文档（架构、协议、平台、里程碑）
├── impl/                   实现规格（API 契约、线格式、测试规范、诊断）
└── tickets/                原始任务卡（TASK.md §8 已重新排序）
```

`docs/reference/` 与 `TASK.md` 冲突时，**以 TASK.md 为准**。

## 关键决策速览

| 项 | 结论 |
|---|---|
| 语言 | Rust（媒体管线必须 in-process，见 [ADR-0003](docs/reference/adr/0003-final-rust-decision.md)） |
| 是否 fork RustDesk | 否（AGPL-3.0 + 190k 行，见 [ADR-0004](docs/reference/adr/0004-rustdesk-fork-evaluation.md)） |
| 平台优先级 | **macOS P0** → Windows P1 → Linux P2 |
| 传输 | QUIC（媒体走 datagram，控制走 reliable stream） |
| 不使用 ffmpeg | 热路径直调 ScreenCaptureKit / VideoToolbox / DXGI / NVENC |

## 六个可测量的差异（Q1–Q6）

每个都对应 RustDesk 源码里一个可定位的缺陷：

| # | 我们 | RustDesk | 差距 |
|---|---|---|---|
| Q1 | 首帧即目标帧率 | `INIT_FPS=15` | 4× |
| Q2 | 本地 deadline 判定 | 依赖 1Hz 探针 | 慢 1 秒 |
| **Q3** | **接收队列 ≤2 帧** | **120 帧（2 秒缓冲）** | **60×** |
| Q4 | 升降对称 | `+1` vs 除法 | 慢 9 秒 |
| Q5 | 静止零编码 | 重复编 10 次 | 10× |
| Q6 | 排队延迟独立统计 | 自认污染 | 失真 |

**Q3 单独就构成产品差异：120 帧 vs 2 帧，是「画面晚 2 秒」和「晚 33ms」的区别。**

详见 [TASK.md §1](TASK.md)。
