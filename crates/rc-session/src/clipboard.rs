//! 剪贴板 UTF-8 纯文本双向同步引擎（T20 / M2-8）。
//!
//! 本模块只含纯逻辑与平台后端抽象 [`ClipboardBackend`]，不碰系统时钟
//! （铁律 4：时间全部通过 `now_us: u64` 注入）、不做任何 IO、纯安全 Rust。
//! macOS 真后端见 [`crate::clipboard_macos`]。
//!
//! # 核心机制
//!
//! - **基线吞并（CB6）**：首拍 [`ClipboardChannel::pump_local`] 仅记录平台
//!   变更计数基线并返回 [`ClipboardAction::Idle`]，连接前剪贴板既有内容
//!   绝不外发；
//! - **Leading-edge + 节流合并（CB4）**：首个本地变更当拍立即产出
//!   [`ClipboardAction::SendToPeer`]（零节流延迟，满足 B4 <200ms），
//!   窗口 [`ClipboardConfig::min_notify_interval_us`] 内的后续变更合并进
//!   唯一挂起槽（最新胜、槽深恒 ≤1），窗口到期冲刷恰一条；
//! - **回声抑制与后到权威覆盖（CB5）**：[`ClipboardChannel::on_peer_text`]
//!   写入本机后记录 `last_written` 并作废本地挂起槽；本机再次观测到与
//!   `last_written` 相同的内容时抑制外发；
//! - **宁不传不截断（CB3）**：超过 `max_text_bytes`（默认
//!   [`crate::proto::MAX_CLIPBOARD_TEXT_BYTES`] = 65535）的文本整条丢弃并
//!   计数，绝不截断；
//! - **纯文本边界（CB7）**：`read_text() == Ok(None)`（图片/文件/非 UTF-8）
//!   静默忽略并计数，对端保留既有文本；入向重复文本跳过写入。

use rc_types::Result;

use crate::proto::MAX_CLIPBOARD_TEXT_BYTES;

/// 节流窗口（微秒）：两次外发通知的最小间隔（100ms）。
///
/// 窗口内的后续变更合并进唯一挂起槽（最新胜），到期冲刷；**首个变更零延迟立发**
/// （leading edge）——这是 B4「<200ms 同步」的确定性前提：驱动拍 ≤16ms（60fps）
/// + localhost RTT ≈1ms，稳态开销远低于 200ms，窗口只用来压突发。
pub const CLIPBOARD_MIN_NOTIFY_INTERVAL_US: u64 = 100_000;

/// 本机剪贴板后端抽象（host/client 对称使用）。
///
/// 实现不得读系统时钟（铁律 4）；macOS 无官方剪贴板变更通知 API，变更检测
/// 使用平台计数器轮询（macOS：`PasteboardSynchronize` 驱动的变更计数）。
pub trait ClipboardBackend: Send {
    /// 平台变更计数。
    ///
    /// 语义：任一进程写入（**含我们自己 `write_text`**）都会递增；单调即可、
    /// 起点任意。实现读前须自行完成跨进程同步（macOS：`PasteboardSynchronize`）。
    fn change_count(&mut self) -> Result<u64>;

    /// 读当前 UTF-8 文本。
    ///
    /// 返回 `Ok(None)` 表示当前剪贴板无 UTF-8 文本 flavor（图片 / 文件 / 非 UTF-8 /
    /// 空板），**不是错误**——非文本内容不参与同步（CB7）。
    fn read_text(&mut self) -> Result<Option<String>>;

    /// 覆盖写入文本（清旧内容）。空串的落点由实现定（clear-only 可）。
    fn write_text(&mut self, text: &str) -> Result<()>;
}

/// 剪贴板同步引擎配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardConfig {
    /// 文本上限（字节）。默认 [`MAX_CLIPBOARD_TEXT_BYTES`]（65535）。
    pub max_text_bytes: usize,
    /// 节流窗口（微秒）。默认 [`CLIPBOARD_MIN_NOTIFY_INTERVAL_US`]（100ms）。
    pub min_notify_interval_us: u64,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            max_text_bytes: MAX_CLIPBOARD_TEXT_BYTES,
            min_notify_interval_us: CLIPBOARD_MIN_NOTIFY_INTERVAL_US,
        }
    }
}

/// 一拍的动作结论（纯数据，便于会话层分发与测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardAction {
    /// 需经 reliable 控制流（Stream A）发送 `ClipboardNotify`（内容即最新值）。
    SendToPeer(String),
    /// 已把对端文本写入本机剪贴板。
    WrittenLocal(String),
    /// 本拍无动作（抑制原因见 [`ClipboardStats`]）。
    Idle,
}

/// 同步统计（进两侧报告 `session.clipboard` 段；键名只增不改名）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipboardStats {
    /// 实际发出的通知数。
    pub sent: u64,
    /// 收到并接受（写入本机）的通知数。
    pub received: u64,
    /// 本地变更 == 自己刚写入的值（回声）。
    pub echo_suppressed: u64,
    /// 窗口内被合并进挂起槽的变更数。
    pub coalesced: u64,
    /// 超上限被**整条丢弃**的变更数（绝不截断）。
    pub oversize_dropped: u64,
    /// 非文本 / 非 UTF-8 变更数。
    pub non_text_dropped: u64,
    /// 被对端后到权威作废的本地挂起值次数。
    pub superseded_local: u64,
    /// 未启用时收到的通知数（计数、不写入、不拆会话）。
    pub received_while_disabled: u64,
}

/// 剪贴板同步引擎（host/client 通用，被会话持有；时间全注入，不读系统时钟）。
pub struct ClipboardChannel {
    cfg: ClipboardConfig,
    /// 上一次观测到的平台变更计数；`None` 表示尚未建立首拍基线（CB6）。
    last_count: Option<u64>,
    /// 最近一次由对端写入本机的文本（回声抑制与入向去重基准，CB5/CB7）。
    last_written: Option<String>,
    /// 节流窗口内唯一挂起槽（槽深恒 ≤1，最新胜，CB4）。
    pending: Option<String>,
    /// 最近一次产出 `SendToPeer` 的会话时刻（微秒）；`None` 表示尚未发过。
    last_sent_us: Option<u64>,
    stats: ClipboardStats,
}

impl ClipboardChannel {
    /// 按配置构造同步引擎（初始未建基线，首拍 `pump_local` 记录基线）。
    pub fn new(cfg: ClipboardConfig) -> Self {
        Self {
            cfg,
            last_count: None,
            last_written: None,
            pending: None,
            last_sent_us: None,
            stats: ClipboardStats::default(),
        }
    }

    /// 判断 `now_us` 相对上次外发是否已过节流窗口（首次发送恒为 `true`）。
    fn window_elapsed(&self, now_us: u64) -> bool {
        match self.last_sent_us {
            None => true,
            Some(last) => now_us.saturating_sub(last) >= self.cfg.min_notify_interval_us,
        }
    }

    /// 本机侧泵（驱动循环每拍调用，`now_us` 为本端单调会话钟微秒）：
    ///
    /// 1. **首拍记录基线**（连接前的既有内容不外发，CB6）；
    /// 2. `change_count` 有变化 → `read_text`：
    ///    - `None` → `non_text_dropped += 1`；
    ///    - `Some(t)` 且 `t == last_written` → `echo_suppressed += 1`；
    ///    - `t.len() > max_text_bytes` → `oversize_dropped += 1`（整条丢弃不截断）；
    ///    - 距上次发送 ≥ 窗口 → 立即 `SendToPeer`（leading edge）；
    ///    - 否则写入唯一挂起槽 `pending = Some(t)`（覆盖旧挂起计 `coalesced += 1`）；
    /// 3. 若有挂起且窗口已过 → 冲刷 `SendToPeer`；
    /// 4. 其余返回 `Idle`。
    ///
    /// 后端错误原样上抛（会话侧记错继续，不拆会话）。
    pub fn pump_local(
        &mut self,
        backend: &mut dyn ClipboardBackend,
        now_us: u64,
    ) -> Result<ClipboardAction> {
        let count = backend.change_count()?;
        let Some(prev) = self.last_count else {
            self.last_count = Some(count);
            return Ok(ClipboardAction::Idle);
        };

        if count != prev {
            let maybe_text = backend.read_text()?;
            self.last_count = Some(count);

            match maybe_text {
                None => {
                    self.last_written = None;
                    self.stats.non_text_dropped += 1;
                }
                Some(text) => {
                    if self.last_written.as_deref() == Some(text.as_str()) {
                        self.stats.echo_suppressed += 1;
                    } else if text.len() > self.cfg.max_text_bytes {
                        self.last_written = None;
                        self.stats.oversize_dropped += 1;
                    } else {
                        // 本机出现了与 last_written 不同的新文本，清除旧回声基准，
                        // 以免后续对端再次发送旧值时被误判为重复而跳过写入。
                        self.last_written = None;
                        if self.window_elapsed(now_us) {
                            if self.pending.take().is_some() {
                                self.stats.coalesced += 1;
                            }
                            self.last_sent_us = Some(now_us);
                            self.stats.sent += 1;
                            return Ok(ClipboardAction::SendToPeer(text));
                        }
                        if self.pending.replace(text).is_some() {
                            self.stats.coalesced += 1;
                        }
                    }
                }
            }
        }

        if self.pending.is_some() && self.window_elapsed(now_us) {
            if let Some(text) = self.pending.take() {
                self.last_sent_us = Some(now_us);
                self.stats.sent += 1;
                return Ok(ClipboardAction::SendToPeer(text));
            }
        }

        Ok(ClipboardAction::Idle)
    }

    /// 对端 `ClipboardNotify` 的落点（无需时间参数）：
    ///
    /// - `text == last_written` → 去重跳过写入，返回 `Idle`（CB7）；
    /// - 否则调用 `backend.write_text(text)`，记录 `last_written`（回声抑制基准），
    ///   `received += 1`，并**作废 `pending`**（若存在则 `superseded_local += 1`，
    ///   后到权威覆盖，CB5），返回 `WrittenLocal`。
    pub fn on_peer_text(
        &mut self,
        backend: &mut dyn ClipboardBackend,
        text: &str,
    ) -> Result<ClipboardAction> {
        if self.last_written.as_deref() == Some(text) {
            return Ok(ClipboardAction::Idle);
        }
        backend.write_text(text)?;
        let owned = text.to_string();
        self.last_written = Some(owned.clone());
        self.stats.received += 1;
        if self.pending.take().is_some() {
            self.stats.superseded_local += 1;
        }
        Ok(ClipboardAction::WrittenLocal(owned))
    }

    /// 重置纪元态（基线 `last_count`、回声基准 `last_written`、挂起槽 `pending`、
    /// 节流时间戳 `last_sent_us`），保留累计 [`ClipboardStats`]。
    ///
    /// 在新会话落成、重连恢复或时钟纪元归零时调用，避免跨纪元复用旧的
    /// `last_sent_us`（时钟归零后 `now_us.saturating_sub(last)` 恒为 0 导致节流卡死）
    /// 或断线前的变更计数与回声基准（H6）。
    pub fn reset_epoch(&mut self) {
        self.last_count = None;
        self.last_written = None;
        self.pending = None;
        self.last_sent_us = None;
    }

    /// 返回当前统计快照。
    pub fn stats(&self) -> ClipboardStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FakeClipboard;

    /// B4/CB4 确定性证明：基线拍之后首次变更**当拍**即产 `SendToPeer`（零节流延迟）。
    #[test]
    fn first_change_sends_immediately() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        // 首拍建立基线
        assert_eq!(
            ch.pump_local(&mut cb, 0).expect("baseline"),
            ClipboardAction::Idle
        );

        // 本地复制后当拍即发
        cb.simulate_local_copy(Some("hello"));
        assert_eq!(
            ch.pump_local(&mut cb, 1_000).expect("first change"),
            ClipboardAction::SendToPeer("hello".to_string())
        );
        assert_eq!(ch.stats().sent, 1);
        assert_eq!(ch.stats().coalesced, 0);
    }

    /// CB6：首拍只记基线返回 Idle；无变更连推多拍全 Idle、stats 全零。
    #[test]
    fn initial_baseline_does_not_send() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();
        // 连接前剪贴板已有敏感内容
        cb.simulate_local_copy(Some("pre-existing-password"));

        assert_eq!(
            ch.pump_local(&mut cb, 0).expect("baseline"),
            ClipboardAction::Idle
        );
        for t in [10_000, 100_000, 500_000, 1_000_000] {
            assert_eq!(
                ch.pump_local(&mut cb, t).expect("idle"),
                ClipboardAction::Idle
            );
        }
        assert_eq!(ch.stats(), ClipboardStats::default());
    }

    /// 100ms 窗口内 3 次变更 → 到期冲刷恰 1 条、内容 = 第 3 次；coalesced = 2。
    #[test]
    fn changes_within_window_coalesce_to_latest() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        // 基线 + 启动窗口的首发
        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);
        cb.simulate_local_copy(Some("init"));
        assert_eq!(
            ch.pump_local(&mut cb, 10_000).unwrap(),
            ClipboardAction::SendToPeer("init".to_string())
        );

        // 100ms 窗口内（10_000..110_000）连续 3 次变更
        cb.simulate_local_copy(Some("first"));
        assert_eq!(
            ch.pump_local(&mut cb, 20_000).unwrap(),
            ClipboardAction::Idle
        );

        cb.simulate_local_copy(Some("second"));
        assert_eq!(
            ch.pump_local(&mut cb, 50_000).unwrap(),
            ClipboardAction::Idle
        );

        cb.simulate_local_copy(Some("third"));
        assert_eq!(
            ch.pump_local(&mut cb, 80_000).unwrap(),
            ClipboardAction::Idle
        );

        // 窗口到期（110_000）：冲刷恰 1 条且为第 3 次
        assert_eq!(
            ch.pump_local(&mut cb, 110_000).unwrap(),
            ClipboardAction::SendToPeer("third".to_string())
        );
        // 后续无再发
        assert_eq!(
            ch.pump_local(&mut cb, 250_000).unwrap(),
            ClipboardAction::Idle
        );

        assert_eq!(ch.stats().sent, 2);
        assert_eq!(ch.stats().coalesced, 2);
    }

    /// 窗口内冲刷不发生（对端零消息）；窗口过后的第一拍恰冲刷一条。
    #[test]
    fn pending_flushes_only_after_window() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);
        cb.simulate_local_copy(Some("a"));
        assert_eq!(
            ch.pump_local(&mut cb, 1_000).unwrap(),
            ClipboardAction::SendToPeer("a".to_string())
        );

        cb.simulate_local_copy(Some("b"));
        // 窗口内多次泵均不冲刷
        for t in [2_000, 50_000, 100_999] {
            assert_eq!(ch.pump_local(&mut cb, t).unwrap(), ClipboardAction::Idle);
        }
        assert_eq!(ch.stats().sent, 1);

        // 窗口过后第一拍（1_000 + 100_000 = 101_000）恰冲刷一条
        assert_eq!(
            ch.pump_local(&mut cb, 101_000).unwrap(),
            ClipboardAction::SendToPeer("b".to_string())
        );
        assert_eq!(ch.stats().sent, 2);
        assert_eq!(
            ch.pump_local(&mut cb, 200_000).unwrap(),
            ClipboardAction::Idle
        );
    }

    /// CB5：`on_peer_text` 写入后 `pump_local` 观测到变更计数递增 → Idle、`echo_suppressed +1`、零外发。
    #[test]
    fn own_write_echo_is_suppressed() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);

        assert_eq!(
            ch.on_peer_text(&mut cb, "from-peer").unwrap(),
            ClipboardAction::WrittenLocal("from-peer".to_string())
        );
        assert_eq!(cb.writes(), &["from-peer"]);

        // write_text 递增了 change_count，下一拍必须判为回声抑制
        assert_eq!(
            ch.pump_local(&mut cb, 10_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().sent, 0);
        assert_eq!(ch.stats().received, 1);
        assert_eq!(ch.stats().echo_suppressed, 1);
    }

    /// 用户再复制与 `last_written` 相同文本 → 抑制（对端已有同值，信息无损）。
    #[test]
    fn identical_local_copy_is_suppressed() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);
        assert_eq!(
            ch.on_peer_text(&mut cb, "same").unwrap(),
            ClipboardAction::WrittenLocal("same".to_string())
        );
        // 吸收 write_text 的首次回声
        assert_eq!(
            ch.pump_local(&mut cb, 10_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().echo_suppressed, 1);

        // 用户再次本地复制同一文本
        cb.simulate_local_copy(Some("same"));
        assert_eq!(
            ch.pump_local(&mut cb, 500_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().sent, 0);
        assert_eq!(ch.stats().echo_suppressed, 2);
    }

    /// 挂起 `"old"` 期间收到对端 `"new"` → `superseded_local +1`，此后任何拍都不再发出 `"old"`。
    #[test]
    fn incoming_notify_discards_pending() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);
        cb.simulate_local_copy(Some("start"));
        assert_eq!(
            ch.pump_local(&mut cb, 1_000).unwrap(),
            ClipboardAction::SendToPeer("start".to_string())
        );

        // 窗口内本地变更进入 pending
        cb.simulate_local_copy(Some("old"));
        assert_eq!(
            ch.pump_local(&mut cb, 20_000).unwrap(),
            ClipboardAction::Idle
        );

        // 对端通知到达：写入 "new" 并作废 pending("old")
        assert_eq!(
            ch.on_peer_text(&mut cb, "new").unwrap(),
            ClipboardAction::WrittenLocal("new".to_string())
        );
        assert_eq!(ch.stats().superseded_local, 1);

        // 窗口过后多拍均不再发出 "old"
        for t in [120_000, 300_000, 1_000_000] {
            assert_eq!(ch.pump_local(&mut cb, t).unwrap(), ClipboardAction::Idle);
        }
        assert_eq!(ch.stats().sent, 1);
        assert_eq!(ch.stats().echo_suppressed, 1);
    }

    /// CB3：70KB → 无 Send、`oversize_dropped +1`、绝不截断。
    #[test]
    fn oversize_text_dropped_not_truncated() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);

        // 70KB 多字节中文字符串（约 70_000 字节 > 65535）
        let huge = "中".repeat(24_000); // 72_000 bytes
        assert!(huge.len() > MAX_CLIPBOARD_TEXT_BYTES);
        cb.simulate_local_copy(Some(&huge));

        assert_eq!(
            ch.pump_local(&mut cb, 10_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(
            ch.pump_local(&mut cb, 500_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().sent, 0);
        assert_eq!(ch.stats().oversize_dropped, 1);
    }

    /// `read_text = None`（复制图片等非文本）→ 不发、`non_text_dropped +1`。
    #[test]
    fn non_text_change_not_synced() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);

        cb.simulate_local_copy(None);
        assert_eq!(
            ch.pump_local(&mut cb, 10_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().sent, 0);
        assert_eq!(ch.stats().non_text_dropped, 1);
    }

    /// `Some("")` → 发送空文本（线 len=0），对端 `write_text("")` 被调用。
    #[test]
    fn empty_text_syncs_clear() {
        let mut sender = ClipboardChannel::new(ClipboardConfig::default());
        let mut receiver = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb_a = FakeClipboard::new();
        let mut cb_b = FakeClipboard::new();

        assert_eq!(
            sender.pump_local(&mut cb_a, 0).unwrap(),
            ClipboardAction::Idle
        );
        cb_a.simulate_local_copy(Some(""));

        let action = sender.pump_local(&mut cb_a, 10_000).unwrap();
        assert_eq!(action, ClipboardAction::SendToPeer(String::new()));

        let rx_action = receiver.on_peer_text(&mut cb_b, "").unwrap();
        assert_eq!(rx_action, ClipboardAction::WrittenLocal(String::new()));
        assert_eq!(cb_b.writes(), &[String::new()]);
    }

    /// 对端连发两条相同文本 → 第二条不写本机（`writes` 长度不变）。
    #[test]
    fn inbound_identical_text_skips_write() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        assert_eq!(
            ch.on_peer_text(&mut cb, "dup").unwrap(),
            ClipboardAction::WrittenLocal("dup".to_string())
        );
        assert_eq!(cb.writes().len(), 1);
        assert_eq!(ch.stats().received, 1);

        assert_eq!(
            ch.on_peer_text(&mut cb, "dup").unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(cb.writes().len(), 1);
        assert_eq!(ch.stats().received, 1);
    }

    /// H6 回归：时钟纪元归零后调用 `reset_epoch()` 清除旧纪元 `last_sent_us` /
    /// `last_count` / `last_written` / `pending` 且保留统计；新纪元从 0 起算不卡死节流。
    #[test]
    fn reset_epoch_allows_sync_after_clock_reset_and_preserves_stats() {
        let mut ch = ClipboardChannel::new(ClipboardConfig::default());
        let mut cb = FakeClipboard::new();

        // 旧纪元高位时刻发送一条文本，再塞一条窗口内挂起文本
        assert_eq!(
            ch.pump_local(&mut cb, 4_000_000_000).unwrap(),
            ClipboardAction::Idle
        );
        cb.simulate_local_copy(Some("old-epoch"));
        assert_eq!(
            ch.pump_local(&mut cb, 4_000_010_000).unwrap(),
            ClipboardAction::SendToPeer("old-epoch".to_string())
        );
        cb.simulate_local_copy(Some("pending-old"));
        assert_eq!(
            ch.pump_local(&mut cb, 4_000_020_000).unwrap(),
            ClipboardAction::Idle
        );
        assert_eq!(ch.stats().sent, 1);

        // 重置纪元态并把会话钟归零到 0
        ch.reset_epoch();
        assert_eq!(ch.stats().sent, 1, "reset_epoch 保留累计统计");

        // 新纪元首拍记录基线（不把旧挂起/既有内容外发）
        assert_eq!(ch.pump_local(&mut cb, 0).unwrap(), ClipboardAction::Idle);

        // 新纪元本地变更在 t=1_000 立即 leading-edge 外发（不会被旧 last_sent_us 卡死）
        cb.simulate_local_copy(Some("new-epoch"));
        assert_eq!(
            ch.pump_local(&mut cb, 1_000).unwrap(),
            ClipboardAction::SendToPeer("new-epoch".to_string())
        );
        assert_eq!(ch.stats().sent, 2);
    }
}
