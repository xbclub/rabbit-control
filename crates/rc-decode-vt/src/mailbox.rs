//! 帧序邮箱：解码回调线程 → 等待线程的单帧结果收集器。
//!
//! 纯 Rust、无 unsafe、无平台依赖 —— 顺序语义（D5）在这里单元测试，
//! 不需要 GPU。
//!
//! # 为什么需要它（D5：无 B 帧进一出一）
//! `VTDecompressionSessionDecodeFrame` 是**异步**的：提交立即返回，
//! 输出在 VideoToolbox 内部队列上的回调里到达。要保持「进一出一、
//! 输出顺序 == 输入顺序」，本项目的策略是**严格单帧在途 + 帧号匹配**：
//!
//! 1. 每次提交带上单调帧号（`sourceFrameRefcon`），回调原样带回；
//! 2. 提交线程随后在邮箱上 `take(frame_id)` 等到**该帧**的回调才返回
//!    （有界等待，见 [`FrameMailbox::take`]），因此下一帧提交必然在
//!    上一帧回收之后——顺序由结构保证，不存在重排窗口；
//! 3. 迟到输出（超时帧的回调在超时后才到达）留在邮箱里，下一次提交前
//!    由 [`FrameMailbox::clear`] 清空丢弃——与编码侧 rc-encode-vt 的
//!    「迟到输出丢弃」策略对称。
//!
//! 邮箱按帧号存取而不是单槽覆盖：万一 VideoToolbox 乱序投递回调
//! （防御假设，未见于此），按号取件也不会张冠李戴。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// 一个 `(帧号, 载荷)` 槽位队列。
///
/// 回调线程只 [`post`](FrameMailbox::post)，提交线程只
/// [`take`](FrameMailbox::take) / [`clear`](FrameMailbox::clear)。
/// 所有可变访问都在 `slots` 这个 Mutex 里。
///
/// `Send`/`Sync` 走自动推导：字段只有 `Mutex<VecDeque<(u64, T)>>` 与
/// `Condvar`，二者在 `T: Send` 时均自动满足——因此 `FrameMailbox<T>`
/// 自动 `Send + Sync`（当且仅当 `T: Send`），无需手写 unsafe impl。
pub(crate) struct FrameMailbox<T> {
    slots: Mutex<VecDeque<(u64, T)>>,
    cond: Condvar,
}

/// 锁获取：容忍中毒。回调线程绝不 panic（跨 C 边界 unwind 是未定义
/// 行为），中毒只可能来自等待线程的 bug，此时继续消费比 panic 传播更安全。
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl<T> FrameMailbox<T> {
    /// 新建空邮箱。
    pub(crate) fn new() -> Self {
        Self {
            slots: Mutex::new(VecDeque::new()),
            cond: Condvar::new(),
        }
    }

    /// 回调线程投递一帧结果。绝不 panic、绝不阻塞（只拿锁、入队、唤醒）。
    pub(crate) fn post(&self, frame_id: u64, value: T) {
        let mut guard = lock(&self.slots);
        guard.push_back((frame_id, value));
        drop(guard);
        self.cond.notify_all();
    }

    /// 等待线程按帧号取件：等到 `deadline` 仍无该帧号则返回 `None`。
    ///
    /// 其他帧号的结果（如迟到回调）留在邮箱里不动——按号取件不会误取。
    /// 不无限阻塞：剩余预算归零立即返回。
    pub(crate) fn take(&self, frame_id: u64, deadline: Instant) -> Option<T> {
        let mut guard = lock(&self.slots);
        loop {
            if let Some(pos) = guard.iter().position(|(id, _)| *id == frame_id) {
                return guard.remove(pos).map(|(_, value)| value);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, _) = self
                .cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = next;
        }
    }

    /// 清空全部残留（迟到输出丢弃）。返回清掉的条数，供诊断。
    pub(crate) fn clear(&self) -> usize {
        let mut guard = lock(&self.slots);
        let n = guard.len();
        guard.clear();
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// D5 顺序保持：回调乱序到达（先 2 后 1），提交线程按提交顺序取件，
    /// 每次拿到的都是**自己那帧**的载荷；未知帧号有界等待后超时返回
    /// None；clear 丢弃迟到残留。
    #[test]
    fn mailbox_delivers_in_submit_order_despite_out_of_order_posts() {
        let mbox = Arc::new(FrameMailbox::new());

        // 回调线程：帧 2 先到、帧 1 后到（乱序投递的防御假设）。
        let poster = Arc::clone(&mbox);
        let callback_thread = thread::spawn(move || {
            poster.post(2, "frame-2");
            poster.post(1, "frame-1");
        });

        // 提交线程按提交顺序取件：先取 1 再取 2，各得各的载荷。
        let deadline = Instant::now() + Duration::from_secs(5);
        assert_eq!(
            mbox.take(1, deadline),
            Some("frame-1"),
            "按号取件不得张冠李戴"
        );
        assert_eq!(mbox.take(2, deadline), Some("frame-2"));
        callback_thread.join().expect("回调线程绝不 panic");

        // 未知帧号：有界等待（近零预算）立即超时，不无限阻塞。
        assert_eq!(mbox.take(42, Instant::now()), None);

        // 迟到残留：clear 清空并报告条数。
        mbox.post(3, "late-frame-3");
        mbox.post(4, "late-frame-4");
        assert_eq!(mbox.clear(), 2);
        assert_eq!(mbox.clear(), 0, "清空后无残留");
        assert_eq!(mbox.take(3, Instant::now() + Duration::from_secs(1)), None);
    }
}
