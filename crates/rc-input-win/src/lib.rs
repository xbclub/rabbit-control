//! RabbitControl 的 Windows 输入骨架（任务卡 T10 的 Windows 部分）。
//!
//! # 平台裁决与现状
//!
//! TASK.md §6（2026-11-20）把 Windows 任务降为 **P1**：真机实现
//! （`SendInput` 注入、`WH_MOUSE_LL`/RawInput 捕获）属 **D 阶段**，
//! 本 crate 目前只交付 `cfg(windows)` 的 API 声明骨架 + TODO：
//!
//! - [`inject`]：`SendInput` / `GetCursorPos` 声明与注入器骨架；
//! - [`capture`]：低层钩子声明与捕获器骨架。
//!
//! 非 Windows 目标上两个模块都不编译，本 crate **编译为空**。骨架也
//! **刻意不含任何测试**：没有能在非 Windows 主机上真实断言的行为，
//! 空过的假测试没有价值；Windows 真机测试（含 I10 光标校准、
//! Ctrl+Alt+Del 特殊通道）随 D 阶段实现一起交付。
//!
//! # 合规红线（任务卡「不要做什么」）
//!
//! `SendInput` 注入的输入带 `LLMHF_INJECTED` 标记——这是系统事实，
//! 本项目**不做反检测 / 输入伪装**；UAC 提权窗口的注入（需 SYSTEM）
//! 记 TODO。

#![deny(missing_docs)]
#![deny(unsafe_code)]

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod inject;
