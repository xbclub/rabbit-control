//! 断线重连的纯逻辑构件（T17）：退避、保持窗口、饱和守卫与配置/统计。
//!
//! # 为什么存在
//!
//! 「传输层断开」不等于「会话结束」：host 在保持窗口内保留管线等
//! `ReconnectRequest` 续传；client 按指数退避自动重连；会话钟逼近 u32
//! 饱和线（71.6 分钟）时 host 主动 `Bye(Refresh)` 重建会话。这三件事的
//! **判定核心**都是纯函数（时间全部由调用方注入，铁律 4），会话状态机
//! （[`crate::host_session`]/[`crate::client_session`]）只做接线。
//!
//! # 时间纪律
//!
//! 本模块不读任何时钟、不做任何 IO：所有时间参数是调用方注入的微秒
//! （会话钟 u32 拓宽而来的 u64）。单纪元内时钟永不回绕——守卫在回绕线
//! 之前必然触发并重建会话，这正是守卫存在的意义。

use std::time::Duration;

use rc_types::{Error, Result};

/// host 侧断线保持窗口默认 TTL。
///
/// 覆盖 WiFi 漫游（1–3s）与对端进程快速重启；QUIC 空闲超时是 30s
/// （rc-net quic.rs），静默断线最坏 30s 后才上报 Disconnected，窗口从
/// Disconnected 起算，因此静默死亡的总宽限 ≈ 30s + TTL。
pub const DEFAULT_RESUME_TTL: Duration = Duration::from_secs(10);

/// client 侧指数退避默认初值：500ms 起步。
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
/// client 侧指数退避默认封顶：8s。
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(8);
/// client 侧指数退避默认最大尝试次数：5 次（第 5 次失败即放弃）。
pub const DEFAULT_BACKOFF_MAX_ATTEMPTS: u32 = 5;

/// 时钟饱和守卫线（微秒）：70 分钟 = 4_200_000_000。
///
/// u32 时间戳约 71.58 分钟饱和到 `u32::MAX`，余量 ≈ 94.97s，足够走完
/// 停机红线排空（500ms 上限）与全新重建握手。到线即发 `Bye(Refresh)`
/// 并重建会话（双端时钟归零）——刷新瞬间画面短暂中断是接受的取舍
/// （无缝双纪元切换不做，`host_flags` bit1 位已留）。
pub const REFRESH_GUARD_US: u64 = 4_200_000_000;

/// 断线重连配置（两侧各持一份，字段同源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectConfig {
    /// host：断线后管线保持窗口 TTL。
    pub resume_ttl: Duration,
    /// client：退避初值。
    pub backoff_initial: Duration,
    /// client：退避封顶。
    pub backoff_max: Duration,
    /// client：最大尝试次数。
    pub backoff_max_attempts: u32,
    /// 时钟饱和守卫线（微秒）。生产恒 [`REFRESH_GUARD_US`]；
    /// 集成测试注入小值走完整刷新路径（不真等 70 分钟）。
    pub refresh_guard_us: u64,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            resume_ttl: DEFAULT_RESUME_TTL,
            backoff_initial: DEFAULT_BACKOFF_INITIAL,
            backoff_max: DEFAULT_BACKOFF_MAX,
            backoff_max_attempts: DEFAULT_BACKOFF_MAX_ATTEMPTS,
            refresh_guard_us: REFRESH_GUARD_US,
        }
    }
}

/// 断线/重连统计（报告与测试的观测点；T18 报告 reconnect 段的数据源）。
///
/// host 与 client 各自记账：`reconnect_attempts` 只在 client 侧有意义
///（host 恒 0）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectStats {
    /// 传输层断开次数。
    pub disconnects: u32,
    /// resume 被接受（管线复用）次数。
    pub resumes: u32,
    /// 全新重建（resume 被拒 / 饱和刷新后的新会话）次数。
    pub rebuilds: u32,
    /// 重连尝试次数（含失败；仅 client 侧）。
    pub reconnect_attempts: u32,
    /// 路径迁移次数（PathChanged，不中断）。
    pub path_changes: u32,
    /// 饱和刷新次数（host 发出 / client 收到 Bye(Refresh)）。
    pub refreshes: u32,
}

/// `ReconnectRequest`（0x0B）的会话层形态。
///
/// 线格式由 [`crate::proto`] 定型（变体 `ControlMessage::ReconnectRequest`）；
/// 本结构是 [`crate::host_session::HostSession::handle_reconnect`] 裁定入口
/// 的参数对象，字段与线格式一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectRequest {
    /// 要恢复的会话标识（原 Hello 的 session_id）。
    pub session_id: u64,
    /// `true` = 请求保持管线续传（线上严格 0/1）。
    pub resume: bool,
}

/// host 侧会话保持窗口：从 Disconnected 起算的「管线必须拆除」倒计时。
///
/// 时间全注入（[`Self::detach`]/[`Self::is_open`] 吃 `now_us`），本类型
/// 不持任何时钟。
pub struct ResumeWindow {
    /// 断线时刻（None = 从未断线，恒开）。
    detached_at_us: Option<u64>,
    /// 窗口长度（微秒）。
    ttl_us: u64,
}

impl ResumeWindow {
    /// 以 TTL 构造（未断线，窗口敞开）。
    pub fn new(ttl: Duration) -> Self {
        Self {
            detached_at_us: None,
            ttl_us: ttl.as_micros() as u64,
        }
    }

    /// 记录断线时刻。窗口未开时登记；已开时**幂等不续期**——重复的
    /// Disconnected 事件（如静默死亡后的重复上报）不得刷新 TTL。
    pub fn detach(&mut self, now_us: u64) {
        if self.detached_at_us.is_none() {
            self.detached_at_us = Some(now_us);
        }
    }

    /// 窗口是否敞开（从未断线恒 true；断线后 `now < detach + ttl` 为真）。
    pub fn is_open(&self, now_us: u64) -> bool {
        match self.detached_at_us {
            None => true,
            Some(t0) => now_us < t0.saturating_add(self.ttl_us),
        }
    }

    /// 剩余毫秒（诊断 / 报告；未断线返回 TTL 满值；过期返回 0）。
    pub fn remaining_ms(&self, now_us: u64) -> u64 {
        match self.detached_at_us {
            None => self.ttl_us / 1_000,
            Some(t0) => t0.saturating_add(self.ttl_us).saturating_sub(now_us) / 1_000,
        }
    }

    /// 重新敞开窗口（回到「从未断线」态）：resume 命中或全新会话建立后由
    /// host 调用。不重开的话，第二轮断线会沿用首轮的锚点（`detach` 对
    /// 已开窗口幂等不续期），第二轮必在旧窗口过期后被错杀成全量重建
    ///（发现 5：两轮断线两轮都应能 resume）。
    pub fn reopen(&mut self) {
        self.detached_at_us = None;
    }
}

/// client 侧指数退避：纯函数 + 计数，**不持任何内部时钟**（铁律 4）。
///
/// 尝试时刻由调用方按 [`Self::delay_before`] 折算（到点判定全凭注入的
/// `now_us`）；本类型只负责「第几次尝试前等多久」与「次数用尽与否」。
#[derive(Debug)]
pub struct Backoff {
    /// 退避初值（毫秒）。
    initial_ms: u64,
    /// 退避封顶（毫秒）。
    max_ms: u64,
    /// 最大尝试次数（≥1）。
    max_attempts: u32,
    /// 已用尝试次数。
    used: u32,
}

impl Backoff {
    /// 构造并校验：`initial > 0`、`max >= initial`、`max_attempts >= 1`，
    /// 违反返回 [`Error::InvalidConfig`]。
    pub fn new(initial: Duration, max: Duration, max_attempts: u32) -> Result<Self> {
        if initial.is_zero() {
            return Err(Error::InvalidConfig(
                "backoff initial must be > 0 (reconnect storm guard)".into(),
            ));
        }
        // 发现 11：亚毫秒初值过 `is_zero` 校验但 `as_millis()` 截断为 0，
        // delay_before 会返回 0ms → 重连风暴自旋。构造期按毫秒口径拦下。
        if initial.as_millis() == 0 {
            return Err(Error::InvalidConfig(
                "backoff initial must be >= 1ms (sub-millisecond truncates to a 0ms delay)".into(),
            ));
        }
        if max < initial {
            return Err(Error::InvalidConfig(format!(
                "backoff max {max:?} must be >= initial {initial:?}"
            )));
        }
        if max_attempts == 0 {
            return Err(Error::InvalidConfig(
                "backoff max_attempts must be >= 1".into(),
            ));
        }
        Ok(Self {
            initial_ms: initial.as_millis() as u64,
            max_ms: max.as_millis() as u64,
            max_attempts,
            used: 0,
        })
    }

    /// 第 `attempt` 次（从 1 计）尝试**前**应等待的时长：
    /// `initial × 2^(attempt−1)`，封顶 `max`。500ms 起步 →
    /// 500/1000/2000/4000/8000ms。
    pub fn delay_before(&self, attempt: u32) -> Duration {
        debug_assert!(attempt >= 1, "attempt 从 1 计");
        // 溢出护栏：2^(attempt−1) 封顶在 2^63 之后不再翻倍（早已被 max 压平）。
        let shift = (attempt - 1).min(63);
        let raw = self.initial_ms.saturating_mul(1u64 << shift);
        Duration::from_millis(raw.min(self.max_ms))
    }

    /// 记录一次失败。`Some(delay)` = 按该延迟再试；`None` = 次数用尽，
    /// 放弃（GaveUp）。
    pub fn on_failure(&mut self) -> Option<Duration> {
        self.used += 1;
        if self.used >= self.max_attempts {
            return None;
        }
        Some(self.delay_before(self.used + 1))
    }

    /// 重连成功后清零（下次断线从 initial 重新退避）。
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// 已用尝试次数。
    pub fn attempts_used(&self) -> u32 {
        self.used
    }
}

/// host 侧时钟饱和守卫：会话钟到线即触发刷新（`Bye(Refresh)` + 红线拆
/// 管线 + 回 accept 等全新 Hello，双端时钟归零）。
pub struct RefreshGuard {
    /// 触发线（微秒）。
    threshold_us: u64,
}

impl RefreshGuard {
    /// 构造。生产传 [`REFRESH_GUARD_US`]；测试注入小值走完整路径。
    pub fn new(guard_us: u64) -> Self {
        Self {
            threshold_us: guard_us,
        }
    }

    /// 到线即 true（`now_us >= threshold_us`）。
    pub fn due(&self, now_us: u64) -> bool {
        now_us >= self.threshold_us
    }
}

// ─────────────────────────────────────────────────────────────────
// 测试：纯逻辑、时间全注入，无 IO、无 sleep
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 退避序列 500ms→1s→2s→4s→8s；第 5 次 `on_failure` 即 `None`
    ///（max_attempts=5：第 1–4 次失败各返回第 2–5 次尝试的等待）；
    /// `reset` 后从头开始。
    #[test]
    fn backoff_schedule_doubles_and_caps() {
        let mut b = Backoff::new(
            DEFAULT_BACKOFF_INITIAL,
            DEFAULT_BACKOFF_MAX,
            DEFAULT_BACKOFF_MAX_ATTEMPTS,
        )
        .expect("默认参数合法");
        for (attempt, want) in [
            (1u32, 500u64),
            (2, 1_000),
            (3, 2_000),
            (4, 4_000),
            (5, 8_000),
            // 超出配置次数的查询也必须封顶不溢出（幂等诊断口径）。
            (6, 8_000),
            (100, 8_000),
        ] {
            assert_eq!(
                b.delay_before(attempt),
                Duration::from_millis(want),
                "delay_before({attempt})"
            );
        }

        // 第 1–4 次失败各返回下一次尝试的等待；第 5 次用尽 → None。
        let delays = [
            Duration::from_millis(1_000),
            Duration::from_millis(2_000),
            Duration::from_millis(4_000),
            Duration::from_millis(8_000),
        ];
        for (i, want) in delays.iter().enumerate() {
            assert_eq!(b.on_failure(), Some(*want), "第 {} 次失败", i + 1);
            assert_eq!(b.attempts_used(), i as u32 + 1);
        }
        assert_eq!(b.on_failure(), None, "第 5 次失败次数用尽直接 None");
        assert_eq!(b.attempts_used(), 5);

        // reset 后从头开始。
        b.reset();
        assert_eq!(b.attempts_used(), 0);
        assert_eq!(
            b.on_failure(),
            Some(Duration::from_millis(1_000)),
            "reset 后首次失败仍返回第 2 次尝试的等待"
        );
    }

    /// initial=0、max<initial、max_attempts=0 均 `Error::InvalidConfig`。
    #[test]
    fn backoff_rejects_degenerate_config() {
        for (initial, max, attempts) in [
            (Duration::ZERO, DEFAULT_BACKOFF_MAX, 5),
            (Duration::from_secs(9), DEFAULT_BACKOFF_MAX, 5),
            (DEFAULT_BACKOFF_INITIAL, DEFAULT_BACKOFF_MAX, 0),
        ] {
            let err = Backoff::new(initial, max, attempts).expect_err("退化配置必须拒绝");
            assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
        }
        // max == initial 合法（不退避的恒定间隔）。
        assert!(Backoff::new(Duration::from_millis(500), Duration::from_millis(500), 1).is_ok());
    }

    /// 发现 11：亚毫秒 initial（500µs）通过 `is_zero` 但 `as_millis()`
    /// 截断为 0，会让 delay_before 返回 0ms（重连自旋）——构造期必须拒。
    #[test]
    fn backoff_rejects_sub_millisecond_initial() {
        let err = Backoff::new(Duration::from_micros(500), DEFAULT_BACKOFF_MAX, 5)
            .expect_err("亚毫秒初值必须拒绝");
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
        // 恰为 1ms 的下界合法。
        assert!(Backoff::new(Duration::from_millis(1), DEFAULT_BACKOFF_MAX, 1).is_ok());
    }

    /// `detach(t0)` 后 `is_open(t0+ttl−1ms)`=true、`is_open(t0+ttl)`=false；
    /// 未 detach 恒 open；`remaining_ms` 单调递减到 0。
    #[test]
    fn resume_window_expires_at_ttl() {
        let ttl = Duration::from_secs(10);
        let mut w = ResumeWindow::new(ttl);
        assert!(w.is_open(u64::MAX), "未断线恒开");
        assert_eq!(w.remaining_ms(0), 10_000, "未断线返回 TTL 满值");

        let t0 = 1_000u64;
        w.detach(t0);
        assert!(w.is_open(t0));
        assert!(w.is_open(t0 + 10_000_000 - 1_000), "TTL−1ms 仍开");
        assert!(!w.is_open(t0 + 10_000_000), "到 TTL 即过期");

        let mut prev = w.remaining_ms(t0);
        assert_eq!(prev, 10_000);
        for step in 1..=10u64 {
            let now = t0 + step * 1_000_000;
            let rem = w.remaining_ms(now);
            assert!(rem <= prev, "remaining_ms 单调递减");
            prev = rem;
        }
        assert_eq!(w.remaining_ms(t0 + 10_000_000), 0, "过期后为 0");
        assert_eq!(w.remaining_ms(t0 + 60_000_000), 0, "远期饱和为 0 不回绕");
    }

    /// 窗口内二次 `detach` 不续期（仍按 t0 过期）。
    #[test]
    fn resume_window_detach_does_not_extend_ttl() {
        let mut w = ResumeWindow::new(Duration::from_secs(10));
        let t0 = 5_000u64;
        w.detach(t0);
        // 窗口中段重复断线上报：TTL 不刷新。
        w.detach(t0 + 4_000_000);
        assert!(!w.is_open(t0 + 10_000_000), "仍按原 t0 过期（不续期）");
        assert!(w.is_open(t0 + 9_999_999));
    }

    /// 发现 5：`reopen` 回到「从未断线」态——第二轮 `detach` 重新锚定
    /// t1，第二轮窗口按 t1+TTL 判定（不沿用首轮 t0）。
    #[test]
    fn resume_window_reopen_reattaches_next_detach() {
        let mut w = ResumeWindow::new(Duration::from_secs(10));
        let t0 = 1_000u64;
        w.detach(t0);
        // 模拟 resume 命中 / 全新会话建立后的重开。
        w.reopen();
        assert!(w.is_open(u64::MAX), "重开后回到未断线态（恒开）");
        assert_eq!(w.remaining_ms(0), 10_000, "重开后剩余回满值");
        // 第二轮断线锚定 t1 = t0 + 2s：新窗口 t1+10s 内开。
        let t1 = t0 + 2_000_000;
        w.detach(t1);
        assert!(w.is_open(t0 + 10_000_000), "旧锚点过期不再影响新窗口");
        assert!(w.is_open(t1 + 9_999_999));
        assert!(!w.is_open(t1 + 10_000_000), "按新锚点 t1 过期");
    }

    /// `due(th−1)`=false、`due(th)`=true；阈值可注入。
    #[test]
    fn refresh_guard_due_at_threshold() {
        let th = 4_200_000_000u64;
        let g = RefreshGuard::new(th);
        assert!(!g.due(th - 1));
        assert!(g.due(th));
        assert!(g.due(u64::MAX));
        // 阈值可注入：集成测试用小值走完整刷新路径。
        let small = RefreshGuard::new(3_000_000);
        assert!(small.due(3_000_000));
        assert!(!small.due(2_999_999));
    }

    /// `u32::MAX as u64 − REFRESH_GUARD_US ≥ 60_000_000`（≥60s 排空余量）
    /// 且 `REFRESH_GUARD_US` 可被 u32 表示。
    #[test]
    fn refresh_guard_margin_below_u32_max() {
        let margin = u32::MAX as u64 - REFRESH_GUARD_US;
        assert!(
            margin >= 60_000_000,
            "距 u32::MAX 至少 60s 余量，实际 {margin}µs"
        );
        assert!(
            u32::try_from(REFRESH_GUARD_US).is_ok(),
            "REFRESH_GUARD_US 必须落在 u32 值域内（守卫先于饱和触发）"
        );
    }
}
