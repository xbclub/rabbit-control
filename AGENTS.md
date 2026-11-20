# Workflow 守则

本文件约束主 agent 在编排 workflow（CreateWorkflow / AmendWorkflow / ResumeWorkflowRun）与后台子 agent 时的行为。适用于 RabbitControl 项目内的所有多步骤自动化任务。

---

## 零、写代码前先读：项目入口文档

本文件只讲**编排纪律**。要实际动手写 RabbitControl 的代码，必须先读：

| 文档 | 作用 |
|---|---|
| [docs/impl/00-onboarding.md](docs/reference/impl/00-onboarding.md) | **入口**：项目是什么、硬性规则、参考实现、任务在哪 |
| [docs/impl/20-crate-contracts.md](docs/reference/impl/20-crate-contracts.md) | 每个 crate 的确切 API 签名（照此实现，不要自创） |
| [docs/impl/30-testing.md](docs/reference/impl/30-testing.md) | 测试怎么写、测什么 |
| [docs/impl/50-definition-of-done.md](docs/reference/impl/50-definition-of-done.md) | 「做完了」的判定标准 |
| [docs/tickets/README.md](docs/reference/tickets/README.md) | 可独立领取的任务卡清单 |

**纪律：一个子 agent 只领一张任务卡（一个 `Txx-*.md`），不要把多张卡塞给同一个子 agent。**
每张卡自带上下文、文件清单、API 签名、测试清单与验收命令，子 agent 不需要读其他文档即可开工。

---

## 一、核心原则：禁止给 workflow 子 agent 过重的任务

- 禁止向单个 workflow 子 agent 分配过重的任务。凡判定为过重的任务，必须拆散成多个子 agent 分别执行。
- 主 agent（编排方）的职责是**拆分、派发与汇总**，而不是把整条流水线压给一个子 agent。

---

## 二、过重任务的判定标准

满足以下任一条件，即视为过重，必须拆分：

| 条件 | 说明 |
|---|---|
| **多阶段流程** | 一个任务内包含 3 个及以上性质不同的阶段（如「分析 → 修改 → 测试 → 写报告」）。 |
| **大范围文件读写** | 需要通读或修改大量文件/目录，超出单一明确目标所需。 |
| **长链路依赖** | 前一步的输出决定后一步的做法，需要反复决策而非按既定指令机械执行。 |
| **单一 prompt 难以说清** | 任务描述需要大量上下文铺垫，或子 agent 需要自行做多次架构/方案决策。 |

---

## 三、拆分方法

1. **按阶段拆**：每个子 agent 只负责流水线中的一环（如调研、实现、验证、评审各归一个子 agent）。
2. **按模块/文件拆**：互相独立的模块或文件各归一个子 agent。

每个子 agent 的 prompt 必须满足：

- **目标单一**：只交付一件事。
- **输入明确**：给出确切的路径与上下文。
- **输出明确**：写到哪里、返回什么结论。

---

## 四、编排规则

- 子 agent 之间不直接通信；由主 agent 收集中间结果并派发下一环。
- 无依赖的子任务并行派发，有依赖的严格串行。
- 每个子 agent 返回后，主 agent 负责校验其产出是否符合该环节的验收标准，再进入下一环节。

---

## 五、启动后必须立刻挂起等待

主 agent 每次启动 workflow（CreateWorkflow / AmendWorkflow / ResumeWorkflowRun）或后台子 agent（Agent 且 `run_in_background=true`）后，**下一个动作必须是调用 `TaskOutput`（`block=true`，传入该运行返回的 task id）挂起等它结束**。

禁止以下替代行为：

- 启动后继续回复用户、干别的活，再「等通知」或「以后再看」；
- 用 `GetWorkflowRun` / `ListWorkflowRuns` 轮询代替等待（这两个只用于用户主动问进度，或通知被截断后补看全文）；
- `sleep`、空转重试、不带 `block=true` 的 `TaskOutput`。

并行启动多个时，在同一轮里对每个 task id 各调一次 `TaskOutput`（`block=true`）。取到结果后先按验收标准校验，再派发下一环。

---

## 六、回答收尾约定

每次完成任务、给出最终回答时，必须在回答末尾加上「喵！」。

- 只加在**最终回答**的末行末尾，一次一个，不要在每个中间步骤或工具调用说明里重复。
- 放在正文之后，不与代码块、表格、文件链接混写在同一行内。

---

## 七、运行巡检

主 agent **每间隔 45 分钟**必须探查一次 workflow 是否工作正常（查看运行日志、进度与中间产出）。

巡检时重点识别以下异常，一经发现必须**立即纠正**，不得等待其自行恢复：

- **工具循环调用**：子 agent 反复调用同一工具、以相同参数反复重试。
- **循环查相同代码**：子 agent 反复读取/搜索同一批文件或同一段代码，无明显进展。

纠正手段：

1. 向对应子 agent 发送纠偏指令（明确指出当前陷入循环、给出下一步具体动作）；
2. 纠偏无效时，终止该子 agent，将其剩余任务**重新拆分**后派发给新的子 agent。
