//! RabbitControl 会话层：M2 双机直连的控制平面。
//!
//! # 为什么存在
//!
//! M1 的 rc-pipeline 是**本机**管线驱动（host.rs / controller.rs 只认
//! tick / push_datagram，不认连接生命周期）；rc-net 有 T09 红线「不碰
//! 采集/编码/解码」只做传输；rc-cli 必须薄。会话握手、IDR 请求、探测、
//! 码率设定、重连恢复需要一条**控制消息协议**与一个**会话状态机**，
//! 它们的家在这里。T13 定义了「控制消息长什么样」（[`proto`]）；
//! T14 在其上落地两侧会话（[`host_session`]/[`client_session`]）与
//! 进程内测试对接件（[`testkit`]）。
//!
//! # 模块地图（随任务卡增量落地，骨架期不预建空壳）
//!
//! | 模块 | 职责 | 落地卡 |
//! |---|---|---|
//! | [`proto`] | 控制消息线格式：类型号、枚举、encode/decode、版本校验 | T13 |
//! | [`host_session`] / [`client_session`] | 会话状态机与握手（Init→Connected→Streaming→Closed）、驱动循环与停机 | T14 |
//! | [`testkit`] | 进程内 MockTransport + pair 对接件（离线确定性测试） | T14 |
//! | [`probe`] | RTT / 时钟偏移 / 带宽探测算法与初始码率（v0 粗 heuristic，M3-3 废弃） | T15 |
//! | [`watchdog`] | IDR 停滞看门狗与请求去抖 | T16 |
//! | [`reconnect`] | 断线重连纯逻辑：指数退避、保持窗口、时钟饱和守卫 | T17 |
//! | [`report`] | client 会话报告与 Gate（跨钟域换算、JSON schema） | T18 |
//! | [`dev_cert`] | 内嵌开发证书：localhost 双进程直连的零配置身份 | T18 |
//!
//! # 纪律
//!
//! 纯 Rust、无 `unsafe`、时间注入（铁律 4）：会话钟由调用方以
//! [`SessionClock`] 闭包注入，两侧管线共享同一闭包；唯一的例外是
//! `client_session` 的 `run()` 用 `Instant::now()` 作驱动钟（坑 5 认可，
//! 见该模块文档）。线格式纪律对齐 rc-input：拒绝未知类型、错误带偏移、
//! 绝不 panic、绝不静默跳过。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod dev_cert;
pub mod probe;
pub mod proto;
pub mod reconnect;
pub mod report;
pub mod watchdog;

mod client_session;
mod host_session;
pub mod testkit;

pub use client_session::{
    ClientRunSummary, ClientSession, ClientSessionConfig, ClientTickReport, ProbeSnapshot,
    ReconnectOutcome,
};
pub use host_session::{
    DetachOutcome, HostRunSummary, HostSession, HostSessionConfig, HostTickReport,
};
pub use reconnect::{
    Backoff, ReconnectConfig, ReconnectStats, RefreshGuard, ResumeWindow, DEFAULT_BACKOFF_INITIAL,
    DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_MAX_ATTEMPTS, DEFAULT_RESUME_TTL, REFRESH_GUARD_US,
};
pub use testkit::{pair, MockTransport};

/// 会话默认路径 MTU 预算（rc-net 常量的薄转发：T18 的 CLI 组合根不直接
/// 依赖 rc-net，经这里取同一常量）。
pub const DEFAULT_SESSION_MTU: usize = rc_net::quic::DEFAULT_MTU;

use crate::proto::ByeReason;

/// 会话时钟：微秒，相对会话零点。照抄 run_real 的 clock Arc 模式
/// （`crates/rc-loopback/src/lib.rs` 的 `run_real`）：由构造方注入，两侧
/// 管线共享同一闭包；会话内部除驱动循环的 poll 预算外不读任何时钟。
///
/// 闭包返回值用 `u32::try_from(...).unwrap_or(u32::MAX)` 饱和，不 `as`
/// 截断（坑 6；饱和守卫本身是 T17 的 `Bye(Refresh)`）。
pub type SessionClock = std::sync::Arc<dyn Fn() -> u32 + Send + Sync>;

/// 会话生命周期（严格单向：Init→Connected→Streaming→Closed；`Detached` 与
/// `Reconnecting` 是 T17 的重连中间态——传输层断开不再等价于会话结束）。
///
/// - `Init`：端点已创建，传输未建立；
/// - `Connected`：QUIC 握手完成，Hello/Welcome 参数协商进行中；
/// - `Streaming`：参数已裁定，媒体/输入双向流动；
/// - `Detached`（host）：传输层已断，管线保留、保持窗口计时中，
///   短超时 accept 轮询等 `ReconnectRequest` 续传或全新 Hello；
/// - `Reconnecting`（client）：传输层已断，管线保留，按指数退避自动重连；
/// - `Closed`：Bye 已发/收或传输断开，且停机序列已完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 端点已创建，传输未建立。
    Init,
    /// QUIC 握手完成，Hello/Welcome 参数协商进行中。
    Connected,
    /// 参数已裁定，媒体/输入双向流动。
    Streaming,
    /// host：传输层已断，管线保留、保持窗口计时中，短超时 accept 轮询
    /// （T17 增补；client 不进此态）。
    Detached,
    /// client：传输层已断，管线保留，按指数退避自动重连（T17 增补；
    /// host 不进此态）。
    Reconnecting,
    /// Bye 已发/收或传输断开，且停机序列已完成。
    Closed,
}

/// 会话结束原因（run/close 的摘要字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// stop 闭包要求退出（到时 / 上层指令），本端随后发 Bye(Normal)。
    Requested,
    /// 对端发来 Bye（`Refresh` 语义的自动重连由 T17 在 tick 分发内接管，
    /// 不再走到停机；此变体只承载 `Normal` 关闭）。
    ByeReceived(ByeReason),
    /// 传输层断开（本卡不做重连，直接走停机序列）。
    ///
    /// 字符串也复用于「协议/注入错误导致的停机」的自由文本原因——
    /// 错误停机与断连对上层（T17 重连决策）是同一类输入。
    Disconnected(String),
}
