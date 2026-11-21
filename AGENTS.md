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

---

## 八、macOS 固定签名开发证书测试指南

Apple Silicon 上默认 ad-hoc 签名每次重编译都会改变 CDHash，导致 macOS TCC（屏幕录制 / 辅助功能 / 输入监控）权限失效。本项目已在登录钥匙串配置固定开发证书并封装统一脚本 [scripts/macos-dev-run.sh](scripts/macos-dev-run.sh)，**一次授权后无论重编译或切换测试二进制均不再掉权限**。

### 固定身份信息

| 项 | 值 |
|---|---|
| 登录钥匙串证书名 | `RabbitControl Dev`（SHA-1：`51F7737F3037EF8858974FF226AC37531B66EEA3`） |
| 固定 Bundle ID | `com.rabbitcontrol.dev` |
| 封装 App 路径 | `target/dev-app/RabbitControlDev.app` |
| Designated Requirement | `identifier "com.rabbitcontrol.dev" and certificate leaf = H"51f7737f3037ef8858974ff226ac37531b66eea3"` |

> **关键约束**：凡涉及 macOS 权限（ScreenCaptureKit 屏幕录制、CGEvent 辅助功能注入、输入监控）的真机运行/测试，**必须**通过 `./scripts/macos-dev-run.sh`（内部用 LaunchServices `open -n -W` 启动 `RabbitControlDev.app`）执行，**不要**直接运行 `cargo run` / `./target/.../rc-cli`——直接子进程的 TCC Responsible Process 会落到宿主终端/IDE，吃不到 `RabbitControlDev.app` 的授权。

### 常用测试命令

```bash
# 1. 检查证书与当前 App 签名身份（不触发重编译）
./scripts/macos-dev-run.sh check-cert

# 2. 验证权限与硬件后端探测（应看到 capture.available=true, screen_recording=true）
./scripts/macos-dev-run.sh cli -- probe --json

# 3. 跑真实硬件全链 M1 Gate（release，SCK → VT 编 → VT 解 → Metal 呈现）
./scripts/macos-dev-run.sh cli --release -- loopback \
    --backend real --resolution 1920x1080 --fps 60 --duration 60s \
    --report /tmp/m1-real.json

# 4. 跑需要屏幕录制权限的 rc-capture-macos 真机测试
./scripts/macos-dev-run.sh test rc-capture-macos -- --ignored --nocapture

# 5. 跑需要辅助功能权限的 rc-input-macos 真机测试（需在「辅助功能」也勾选一次 RabbitControlDev.app）
./scripts/macos-dev-run.sh test rc-input-macos -- --ignored --nocapture
```

（也可用 `just dev-cert`、`just dev-probe`、`just dev-cli ...`、`just dev-test ...` 快捷入口。）

### 注意事项

- **不违反铁律 3**：日常改代码内循环依然只跑 `cargo check -p <crate>` 与普通 `cargo test -p <crate>`；只有跑**真机权限测试或 `--backend real` M1 Gate** 时才调用 `scripts/macos-dev-run.sh`。
- **静屏测试行为**：`sck_captures_at_60fps` 要求测试期间屏幕有持续画面变化；静止桌面下 SCK 返回 0 帧属于符合铁律 2（静止零编码）的正常行为。
- **补授其他权限**：若后续测试 `rc-input-macos` 提示缺少辅助功能/输入监控权限，把同一份 `target/dev-app/RabbitControlDev.app` 拖进系统设置对应列表打开一次即可，同样跨重编译永久生效。

---

## 九、TASK.md 回写强制约束（2026-11-21 起）

**每完成一张任务卡、一个里程碑、或一轮含实测数字的实质性修改，在给出最终回答之前，必须先把结果回写 [TASK.md](TASK.md)。** 未回写即汇报完成，视为任务未完成。

回写的最低要求：

| 位置 | 必须更新什么 |
|---|---|
| §0 速览「当前状态」行 | 一句话反映本轮后的真实状态（含测试数、关键指标） |
| §12 当前实测基线 | 本轮所有新实测数字：场景、配置（后端形态 / 参数）、结果行 |
| §13 过程记录 | 本轮的关键决策、根因、新踩的坑（带文件路径）；上一轮遗留清单若已解决，就地标记闭环 |

补充纪律：

- 数字必须带**场景与配置**（哪个 gate、什么后端、什么供给），不得只写裸数字；结论行要写门槛与是否达标。
- 本轮**没做**的事不要顺手改旧数字；发现旧数据有误时，保留原行并追加更正说明，不直接抹掉历史。
- 真机/带权限测量还须写明运行方式（如幕布工具命令、`scripts/macos-dev-run.sh` 入口），保证可复现。
- 里程碑级结果（如 M1 Gate）还要同步 `docs/reference/design/06-milestones.md` 与对应任务卡（`docs/reference/tickets/Txx-*.md`）。
