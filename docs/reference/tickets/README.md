# 任务卡总览

> **一张卡 = 一个可独立交付的单元。** 一个 agent 只领一张卡。
> 每张卡自带全部上下文：目标、文件、API 签名、测试清单、验收命令。

## 怎么用

1. 按下面的「依赖关系」选一张**前置已满足**的卡
2. 读该卡全文（尤其是「不要做什么」）
3. 读 §「参考实现」里指定的一两个 crate
4. 开工 → 按卡里的验收清单自检 → 报告

## 依赖关系

```
T01 工程骨架补全 ✅（已完成）
  │
  ├── T02 回环框架骨架 ────────────┐
  │                                 │
  ├── T03 Windows 采集 ────────────┤
  │     │                           │
  │     └── T04 NVENC 编码 ─────────┤
  │                                 │
  ├── T05 解码与呈现 ───────────────┤
  │                                 │
  ├── T06 软编兜底（openh264）──────┤
  │                                 │
  └── T07 CLI 入口 ─────────────────┘
                                    │
                          T08 M1 端到端串联（依赖 T02–T07）
                                    │
                          T09 QUIC 传输（可与 T03–T07 并行）
                                    │
                          T10 输入通道
                                    │
                          T11 Linux 采集
                                    │
                          T12 macOS 采集
```

**可并行的组：**

| 组 | 卡片 | 前提 |
|---|---|---|
| A | T02, T09 | 已完成基座（两者互不依赖，可同一 agent 依次做或两个 agent 并行） |
| B | T03（先交 trait 层）→ T04 / T05 / T06 / T07 | trait 层定型后，各卡可用假实现独立开发 |
| C | T11, T12 | T03 的 trait 层定型后，需对应平台真机 |

**注意：T02、T09、T06、T07 不需要任何真机**（全平台可做，含 macOS）。
只有 T03 / T04 / T05 / T08 / T10 需要 Windows 物理机，
T11 需要 Linux，T12 需要 macOS。

## 卡片状态

| 卡 | 标题 | 依赖 | 平台 | 阶段 | 状态 |
|---|---|---|---|---|---|
| [T01](T01-engineering-base.md) | 工程基座 | — | 全 | — | ✅ 已完成 |
| [T02](T02-loopback-skeleton.md) | 回环框架骨架 | T01 | 全 | **A1** | ✅ 已完成 |
| [T07](T07-cli.md) | CLI 入口 | T02 | 全 | **A2** | ✅ 已完成 |
| [T12](T12-capture-macos.md) | macOS 采集 + VideoToolbox | T02 | **macOS** | **B1** | ✅ 已完成 |
| [T05](T05-decode-render.md) | 解码与呈现 | T12 | **macOS 版** | **B2** | ✅ 已完成 |
| [T08](T08-m1-pipeline.md) | **M1 Gate 端到端** | T02, T07, T12, T05 | **macOS** | **B3** | ✅ 已完成（2026-11-21，卡内两处修正见卡片末尾） |
| [T09](T09-net-quic.md) | QUIC 传输 | T01 | 全 | C1 | ✅ 已完成（2026-11-21，23 单测 + 4 回环集成全绿；3% 丢包存活、100KB 分片逐字节重组、`DEFAULT_MAX_CONCURRENT_FRAMES=2` 均实测；N9 真路径迁移测试为已知空档） |
| [T10](T10-input-channel.md) | 输入通道 | T09 | Win | C2/D3 | ✅ 已完成（2026-11-21，按 §6 平台重排以「全平台核心 `rc-input` + macOS 桥接 `rc-input-macos`」交付 C2；Windows SendInput/捕获为 `rc-input-win` cfg(windows) 骨架，真机实现属 D3 待领） |
| [T03](T03-capture-windows.md) | Windows DXGI 采集 | T08 | Win | D1 | ⬜ 待领取（P1） |
| [T04](T04-encode-nvenc.md) | NVENC 编码 | T03 | Win | D2 | ⬜ 待领取（P1） |
| [T06](T06-encode-software.md) | 软件编码兜底 | T01 | 全 | F | ⬜ 待领取 |
| [T11](T11-capture-linux.md) | Linux 采集 | T03 | Linux | E | 🔒 P2 归档 |
| [T13](T13-session-proto.md) | rc-session 控制协议线格式 | — | 全 | M2 | ✅ 已完成（2026-11-21，12 测试；线格式冻结 0x01–0x0C + 0x0D–0x0F 预留） |
| [T14](T14-session-core.md) | 会话核心：HostSession/ClientSession 状态机与握手 | T13 | 全 | M2 | ✅ 已完成（2026-11-21，26 测试；含真 QUIC 回环集成） |
| [T15](T15-probe-initial-bitrate.md) | 探测与初始码率（M2-10） | T13, T14 | 全 | M2 | ✅ 已完成（2026-11-21，23 测试；估速→SetBitrate 链路实测） |
| [T16](T16-idr-reliable-channel.md) | M2-3 IDR 可靠通道完善 | T13, T14 | 全 | M2 | ✅ 已完成（2026-11-21，15 测试；丢包演练 17ms 恢复） |
| [T17](T17-session-reconnect.md) | 断线重连与会话恢复（M2-9） | T15, T16 | 全 | M2 | ✅ 已完成（2026-11-21，19 测试；rebind 迁移 + 70min 刷新守卫） |
| [T18](T18-cli-host-client-localhost.md) | rc-cli host/client 双进程直连 + localhost 实测 | T13–T17 | 全（real 仅 macOS） | M2 | ✅ 已完成（2026-11-21，22 测试；localhost 真后端 Run B Gate 全过（首帧 630ms/p50 10.8/p99 18.5/68.3fps），MTU+时钟两根因修复见 TASK.md §13） |
| [T19](T19-auth-password-approval.md) | M2-5 鉴权：密码挑战 + 接受弹窗联动 | T13, T14 | 全 | M2 | ✅ 已完成（2026-11-22，workflow 6 阶段 + 独立审计捉虫修复；手写 SHA-256/HMAC/PBKDF2 + 唯一非阻塞 `ConnectionApprover` + `--password` 冒烟通过，workspace 577 全绿） |
| [T20](T20-clipboard-text-sync.md) | 剪贴板文本同步（M2-8） | T13, T14 | 全（real 仅 macOS） | M2 | ✅ 已完成（2026-11-22，workflow 5 阶段 + 独立只读 Review 0 缺陷；0x0F + 同步引擎 + CarbonCore Pasteboard C API + `--clipboard` 真 QUIC E2E，workspace 598 全绿 / 17 ignore） |
| [T21](T21-controller-ui.md) | M2-6 控制端 UI（rc-ui） | T13–T18 | **macOS** | M2 | ✅ 已完成（2026-11-22，workflow 5 阶段 + 独立 Review 捉 3 处 `CAMetalLayer*` 生命周期/竞态缺陷并以 `LayerLease`+代次状态机闭环；`rc-ui` 并入 workspace `members`，26+6=32 项测试全绿，workspace 652 全绿 / 20 ignore） |
| [T22](T22-host-tray-approval.md) | 被控端托盘 + 接受弹窗（M2-7） | T18, T19 | **macOS**（会话侧全平台） | M2 | ✅ 已完成（2026-11-22，workflow 4 阶段 + 独立 Review 捉出 3 处并发/时钟/停机缺陷并补 5 回归测试；`rc-host-macos` + `tray_bench --run` 真机通过 + `--tray/--approval/--approval-timeout`，workspace 620 全绿） |

> **2026-11-20 重排**：P0 改为 macOS，主线为 **T02 → T07 → T12 → T05 → T08**。
> Windows 任务降为 P1（阶段 D），Linux 降为 P2（阶段 E，归档）。
> 完整顺序见 [TASK.md §8](../../../TASK.md)。

> **¹ 关于 T04 的依赖**：T04 的 `encode()` 入参是 `&CapturedFrame`，该类型定义在
> `rc-capture`（T03 的 crate）。因此 T04 需要 T03 **先交付 `rc-capture` 的 trait 与类型定义**
> （只有 `lib.rs`，约 100 行，不需要 DXGI 实现）。这样两个 agent 可以在 T03 交付 trait 层后并行：
> 一个做 DXGI 后端，一个做 NVENC 编码器，双方用假帧对接。
> 若 T03 尚未开工，T04 可自行定义 `CapturedFrame` 并在 T03 合入时对齐——但不推荐，会增加合并冲突。

**T11/T12 依赖 T03 的原因**：`CaptureBackend` trait 与 `CapturedFrame` 必须在 T03 定型后
才能实现其他平台后端，否则三平台会各自发明一套接口。

## 领取前的三个问题

**Q1: 我在哪个平台上？**
需要真机的卡标了平台。在 macOS 上做不了 T03/T04。若你的平台不匹配，选标「全」的卡（T02、T06、T07、T09）。

**Q2: 我能不能跑测试？**
所有卡的验收都要求 `cargo test -p <crate>` 全绿。若环境跑不了 cargo，不要领卡。

**Q3: 前置真的满足吗？**
按依赖关系查。T08 需要 T02–T07 全部完成——**不要提前领**，否则会写一堆无法验证的胶水代码。

## 卡片模板（新建卡片时照抄）

```markdown
# Txx 标题

> 一句话目标

## 背景（为什么做这个）
（3-5 句，只给必要上下文）

## 交付物
| 文件 | 动作 | 说明 |

## API 契约
（确切签名，从这个文件照抄）

## 必须做到（硬性要求）
| # | 要求 | 违反后果 |

## 不要做什么
（明确排除项，防止范围蔓延）

## 测试清单
（必须覆盖的用例，逐条列出）

## 验收命令
```bash
（可直接复制执行）
```

## 验收标准
（可判定的数字或条件）

## 参考实现
（读哪个 crate，学什么）

## 已知坑
（平台相关，来自 docs/04-platforms.md）
```
