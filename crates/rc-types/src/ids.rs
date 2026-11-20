//! 强类型 ID。
//!
//! # 为什么用 newtype 而不是裸 `u64`
//! `frame_seq`、`capture_seq`、`timestamp_us` 都是整数，一旦传反，
//! 编译期不会报错，运行期表现为「画面错位」「延迟测量离谱」这类难查的 bug。
//! newtype 让编译器替我们检查。**这是对抗 AI 笔误最便宜的手段。**

use std::fmt;
use std::time::{Duration, Instant};

/// 帧序号。单调递增，从 0 开始，会话内唯一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FrameSeq(pub u64);

/// 采集序号。与 [`FrameSeq`] 分离，因为重复帧会被丢弃：
/// 采集序号连续，帧序号会跳号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CaptureSeq(pub u64);

/// 显示器标识。平台层的 output index 或稳定 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DisplayId(pub u32);

/// 会话标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(pub u64);

/// 微秒时间戳，相对于会话开始时刻。
///
/// 用 `u32` 而非 `Duration`：协议包头里要放它，
/// 且 u32 微秒可表示 ~71 分钟，足够单次会话；超长会话在会话层做回绕处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub u32);

impl Timestamp {
    /// 从会话起点计算相对时间戳。
    ///
    /// # 溢出
    /// 超过 `u32::MAX` 微秒（约 71.6 分钟）时饱和到最大值。
    /// 会话层必须在此之前重建会话或做回绕。
    pub fn since(start: Instant, now: Instant) -> Self {
        let us = now.saturating_duration_since(start).as_micros();
        Timestamp(u32::try_from(us).unwrap_or(u32::MAX))
    }

    /// 转为 `Duration`
    pub const fn as_duration(self) -> Duration {
        Duration::from_micros(self.0 as u64)
    }

    /// 两时间戳之差（饱和到 0，不 panic）。
    ///
    /// **不要用裸减法**：网络乱序会导致 `later < earlier`。
    pub const fn saturating_diff(self, earlier: Timestamp) -> Duration {
        Duration::from_micros(self.0.saturating_sub(earlier.0) as u64)
    }
}

impl FrameSeq {
    /// 下一个序号。溢出时 panic（单次会话不可能达到 2^64 帧）。
    #[inline]
    pub fn next(self) -> Self {
        FrameSeq(self.0.checked_add(1).expect("frame seq overflow"))
    }

    /// 前一个序号，0 时返回 None。
    #[inline]
    pub fn prev(self) -> Option<Self> {
        self.0.checked_sub(1).map(FrameSeq)
    }
}

impl Timestamp {
    /// 微秒差值，无饱和（用于比较）。
    #[inline]
    pub const fn diff_us(self, earlier: Timestamp) -> i64 {
        self.0 as i64 - earlier.0 as i64
    }
}

macro_rules! impl_display {
    ($($t:ty),*) => {
        $(impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        })*
    };
}
impl_display!(FrameSeq, CaptureSeq, DisplayId, SessionId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_since_start_is_zero() {
        let start = Instant::now();
        assert_eq!(Timestamp::since(start, start), Timestamp(0));
    }

    #[test]
    fn timestamp_since_is_micros() {
        let start = Instant::now();
        let later = start + Duration::from_millis(1);
        assert_eq!(Timestamp::since(start, later), Timestamp(1000));
    }

    #[test]
    fn timestamp_saturates_on_overflow() {
        let start = Instant::now();
        let later = start + Duration::from_secs(7200); // 2 小时
        assert_eq!(Timestamp::since(start, later), Timestamp(u32::MAX));
    }

    #[test]
    fn saturating_diff_handles_out_of_order() {
        // 乱序：later 到达但时间戳更小
        let earlier = Timestamp(2000);
        let later = Timestamp(1000);
        assert_eq!(later.saturating_diff(earlier), Duration::ZERO);
    }

    #[test]
    fn frame_seq_next_and_prev() {
        assert_eq!(FrameSeq(0).next(), FrameSeq(1));
        assert_eq!(FrameSeq(0).prev(), None);
        assert_eq!(FrameSeq(1).prev(), Some(FrameSeq(0)));
    }

    #[test]
    fn diff_us_can_be_negative() {
        assert_eq!(Timestamp(1000).diff_us(Timestamp(2000)), -1000);
    }
}
