# 50 完成定义（Definition of Done）

> **「做完了」不是「代码能编译」。** 本文用可执行的检查替代主观判断。
> 任何 agent 交付前必须逐项证据化确认。

## 1. 通用 DoD（每个任务都必须满足）

逐项检查，**每一项都要有证据**（命令输出、文件路径、测试名）。

| # | 检查项 | 验证命令 | 证据 |
|---|---|---|---|
| D1 | 编译通过 | `cargo check -p <crate> --all-targets` | 无 error/warning |
| D2 | 该 crate 测试全绿 | `cargo test -p <crate>` | `N passed; 0 failed` |
| D3 | 全 workspace 不受影响 | `cargo test --workspace` | 之前的测试数 + 新增数，全绿 |
| D4 | clippy 无警告 | `cargo clippy -p <crate> --all-targets -- -D warnings` | 无输出 |
| D5 | 格式化 | `cargo fmt --all -- --check` | 无 diff |
| D6 | 公开 API 有文档 | `cargo doc -p <crate> --no-deps` | 无 missing_docs 错误 |
| D7 | 无新增 `unsafe`（业务层） | `grep -rn "unsafe" crates/<crate>/src` | 仅平台 crate 的 ffi/ 下 |
| D8 | 无 `Box<dyn Error>` 跨 crate | `grep -rn "Box<dyn Error>" crates/<crate>/src` | 无结果 |
| D9 | 无真实时钟依赖（可测逻辑） | `grep -rn "Instant::now\|SystemTime::now" crates/<crate>/src` | 仅测试或明确标注的边界层 |
| D10 | 无 `unwrap()` 在失败路径 | 人工 review | 仅测试与 `expect` 带说明 |

### 一键验证

```bash
just v      # fmt + clippy(-D warnings) + 全部测试
```

## 2. 按任务类型的附加 DoD

### 2.1 新建 crate

除 D1–D10 外：

- [ ] `Cargo.toml` 使用 workspace 字段（`version.workspace = true` 等）
- [ ] crate 头部含 `#![deny(missing_docs)]`、`#![deny(unsafe_code)]`
- [ ] 加入根 `Cargo.toml` 的 `members`
- [ ] 依赖方向符合 AGENTS.md 的依赖图（不向上依赖）
- [ ] 至少有一个「非法输入」测试
- [ ] 至少有一个「边界」测试
- [ ] 在 `docs/impl/20-crate-contracts.md` 中补充实际签名（若与契约不一致）

### 2.2 状态机 / 决策逻辑

- [ ] 每个输出分支有测试（用 `enum` 保证可穷举）
- [ ] 边界测试：`阈值-1`、`阈值`、`阈值+1`
- [ ] 重复调用的幂等性测试
- [ ] 统计一致性测试（`captured == encoded + dropped_*`）
- [ ] 单一职责：不读时钟、不读环境变量、不做 IO

### 2.3 纯算法 / 数据处理

- [ ] 空输入不 panic
- [ ] 单元素输入正确
- [ ] 最大尺寸正确
- [ ] 奇数 / 非对齐输入（如 `15x17`）
- [ ] 内部缓冲复用（无每帧分配）—— 用 `reused` 字段或 bench 证明
- [ ] 真实 bug 的回归测试（若修过 bug）

### 2.4 编解码 / 线格式

- [ ] 往返测试（encode → decode == 原值）
- [ ] **逐字节固定向量**测试（防止格式漂移）
- [ ] 分片重组测试（多片拼回原字节流）
- [ ] 每个错误变体都有测试，且 `offset` 正确
- [ ] 若改了格式：同步更新 `docs/impl/10-wire-format.md`
- [ ] 若改了格式：确认 `field_offsets_are_stable` 测试已更新

### 2.5 平台绑定（capture / encode / input）

- [ ] `unsafe` 全部收敛在 `ffi/` 子模块
- [ ] 每个 `unsafe fn` 有 `// SAFETY:` 注释
- [ ] 所有 OS 资源有 RAII guard（Drop 中释放）
- [ ] 错误映射到 `Error` 枚举，`recoverable` 正确标注
- [ ] 有假实现（`#[cfg(test)]`）以便上层无平台测试
- [ ] 自愈路径有测试（模拟 `ACCESS_LOST` 等）
- [ ] 在 `docs/04-platforms.md` 的坑列表中补充新发现的坑

### 2.6 诊断 / 回环

- [ ] 输出 JSON schema 与 `docs/impl/40-diagnostics.md` §4 完全一致
- [ ] 校验一致性不变式
- [ ] `--seed` 固定后结果可复现
- [ ] 无 GPU 时降级不 panic
- [ ] 退出码正确（passed → 0，失败 → 1）

## 3. 任务卡自带的验收清单

每张 `docs/tickets/Txx-*.md` 底部有**该任务特有的验收清单**。
通用 DoD + 任务特有清单 = 完整验收。

**两者都必须全绿才算完成。** 不允许「通用过了但任务特有项跳过」。

## 4. 不可接受的「完成」说法

以下说法**不构成完成证据**：

| 说法 | 问题 |
|---|---|
| 「应该能工作」 | 没跑过 |
| 「代码逻辑是对的」 | 没验证 |
| 「测试都过了」（但没给输出） | 无法核实 |
| 「和参考实现一样的模式」 | 模式一样不代表实现了 |
| 「剩下的小问题后面修」 | 未定义什么是「小问题」 |
| 「我加了 TODO」 | TODO 是给未来的，不是完成 |
| 「编译通过了」 | 只是 D1 |
| 「只加了一行，不会影响别的」 | 必须跑 `cargo test --workspace` |

## 5. 提交报告的格式

交付时必须报告这几项，缺一不可：

```markdown
## 完成情况

### 改动文件
- crates/rc-xxx/src/lib.rs（新增 320 行）
- crates/rc-xxx/Cargo.toml（新建）

### 验证证据
$ cargo test -p rc-xxx
running 12 tests
test result: ok. 12 passed; 0 failed

$ cargo test --workspace
（各 crate 测试数）
合计 119 passed; 0 failed

$ cargo clippy -p rc-xxx --all-targets -- -D warnings
（无输出）

### 实现说明
- 关键决策：为什么这样设计（1-3 句）
- 与契约的偏差：无 / 有（说明原因）

### 未完成 / 已知问题
- 无
  （或：明确列出，并说明影响与建议）
```

**没有「验证证据」这一节，视为未完成。**

## 6. 已知问题怎么写

如果确实有未解决的部分，必须这样报告：

```markdown
### 已知问题

1. **Wayland 下 PipeWire 流在分辨率变化时偶发断开**
   - 影响：Linux Wayland 用户切换分辨率后可能黑屏 1-2 秒
   - 已做的缓解：加了 rebuild 重试（3 次，间隔 100ms）
   - 建议：需要真机验证 GNOME 46 + KDE 6，我无法在本地测试
   - 相关代码：crates/rc-capture-linux/src/pipewire.rs:142
```

需要包含：**现象 / 影响 / 已做的缓解 / 缺什么才能解决 / 代码位置**。

「TODO: 修一下」不是合格的已知问题描述。

## 7. 交接检查

代码要交给下一个 agent 时，确认：

- [ ] 桩函数 / 未实现路径用 `todo!()` 或明确 `Err(...)`，**不要返回假数据**
- [ ] 所有 `todo!()` 的位置在报告里列出
- [ ] 文档中的状态表已更新（`AGENTS.md` §9 或任务卡状态）
- [ ] 新增的坑写进了 `docs/04-platforms.md`
- [ ] 若改了公共契约，更新了 `docs/impl/20-crate-contracts.md`

**返回假数据是最坏的交接方式**——下一个人会以为功能已经工作。

## 8. 验收脚本（未来）

当 `rc-cli` 就绪后，以下命令应能一键判定：

```bash
# 全部静态检查 + 测试
just v

# M1 Gate（回环延迟）
just loopback --duration 60s --report /tmp/r.json && jq -e '.gate.passed' /tmp/r.json
```

在这些就绪之前，用 §1 的表格逐项人工确认。

## 9. 当前基线（用于对比）

2026-11-19 实测：

| 指标 | 值 |
|---|---|
| 测试总数 | 107 |
| `cargo test --workspace` | 1.4s |
| 单 crate 增量 check | 0.09s |
| `target/` | 80MB |

交付后应报告新的总数。**测试数只能增加，不能减少。**
如果发现测试数变少，先查是不是有人删了测试。
