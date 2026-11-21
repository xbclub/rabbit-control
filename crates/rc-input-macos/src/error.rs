//! 输入注入的错误类型。
//!
//! 风格照抄 rc-types：枚举 + `thiserror`，错误信息包含定位问题所需的数据。
//!
//! 为什么不用 [`rc_types::Error`]：它目前没有「权限缺失 / 输入注入」变体，
//! 把 `NotTrusted` 塞进 `InvalidConfig` 之类的现有变体会语义失真。所以本
//! crate 先用本地枚举；待 rc-types 增加输入变体后再接 `#[from]`。

use crate::keymap::Key;

/// 输入注入错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InputError {
    /// 进程未持有辅助功能授权。`CGEventPost` 在未授权时**静默丢弃事件**
    /// （不报错），所以必须在注入前显式拒绝（T12 M2）。
    #[error(
        "辅助功能未授权：CGEventPost 会被系统静默丢弃。\
         请到 系统设置 → 隐私与安全性 → 辅助功能 允许本进程，并重启进程后重试"
    )]
    NotTrusted,

    /// 键映射表未覆盖该键。当前表穷尽所有 [`Key`] 变体，此变体为将来
    /// 新增「无虚拟键码的键」（如 JIS 布局专属键）预留。
    #[error("unsupported key: {0:?}")]
    UnsupportedKey(Key),

    /// `CGEventSource` 创建失败（系统资源耗尽等）。
    #[error("CGEventSource create failed")]
    SourceCreateFailed,

    /// `CGEvent` 构造失败（CG 返回 NULL）。`0` 标注出错的构造器名。
    #[error("CGEvent create failed: {0}")]
    EventCreateFailed(&'static str),

    /// `CGEventTap` 创建失败（CGEventTapCreate 返回 NULL / 事件源挂接失败 /
    /// 捕获线程 setup 握手超时）。未授权（辅助功能或输入监控）时
    /// `CGEventTapCreate` 对 NULL 返回是少数**可见**的失败信号——本错误
    /// 的信息里直接给出授权路径。
    #[error(
        "CGEventTap create failed: {0}。监听 tap 需要 系统设置 → 隐私与安全性 →\
         辅助功能（或 输入监控）允许本进程；刚授权过必须重启进程再试"
    )]
    TapCreateFailed(&'static str),

    /// 捕获线程已停止（runloop 退出后事件通道断开）。此后继续调用
    /// `next_events` 只会得到本错误，绝不静默返回空表冒充「无事件」。
    #[error("capture tap thread stopped (runloop exited)")]
    TapStopped,
}
