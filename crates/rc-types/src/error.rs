//! 错误类型。
//!
//! # 规则（对所有 crate 生效）
//! 1. 禁止用 `Box<dyn Error>` 跨 crate 边界传播——AI 无法穷举分支处理。
//! 2. 每个 crate 有自己的错误枚举，通过 `#[from]` 转换到 `Error`。
//! 3. 错误信息必须包含**足够定位问题的数据**（偏移量、期望值、实际值）。

use thiserror::Error;

/// 全项目统一错误类型。
///
/// 新增变体时务必同步更新 [`Error::is_fatal`]——
/// 它决定了调度器/会话层是否应该重建资源。
#[derive(Debug, Error)]
pub enum Error {
    /// 帧数据结构非法（stride、缓冲长度、尺寸不匹配）
    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    /// 配置非法
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// 协议解析失败。`offset` 指向出错字节位置，便于定位。
    #[error("protocol error at offset {offset}: {msg}")]
    Protocol {
        /// 出错位置（相对于包起始）
        offset: usize,
        /// 说明
        msg: String,
    },

    /// 编码器不可用或失败
    #[error("encoder error: {0}")]
    Encoder(String),

    /// 编码器因过载/预算**丢弃了本帧输入**（未编码）——可恢复。
    ///
    /// 典型来源：VideoToolbox RealTime 模式过载（`kVTEncodeInfo_FrameDropped`）。
    /// 帧根本没被编码，编码器内部与解码端的参考链**均无损**，下一帧照常
    /// P 帧即可；高帧率逼近硬编吞吐上限时属于预期瞬态（1080p×120 实测
    /// VT 吞吐上限 ≈105–108fps < 120fps），正确行为是丢帧继续，不是像
    /// [`Error::Encoder`] 那样拆会话。
    #[error("encoder dropped frame (overloaded): {msg}")]
    EncoderDroppedFrame {
        /// 说明
        msg: String,
    },

    /// 解码器不可用或失败
    #[error("decoder error: {0}")]
    Decoder(String),

    /// 采集失败。`recoverable` 决定上层是重建还是报错退出。
    #[error("capture error (recoverable={recoverable}): {msg}")]
    Capture {
        /// 是否可通过重建恢复
        recoverable: bool,
        /// 说明
        msg: String,
    },

    /// 网络层错误
    #[error("network error: {0}")]
    Network(String),

    /// 超时
    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),

    /// io 错误
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// 是否属于「致命错误」——需要中断当前阶段而不是重试。
    ///
    /// 采集错误通常是可恢复的（切分辨率、锁屏），
    /// 编码器/协议错误通常不可恢复（需要重建会话）。
    pub fn is_fatal(&self) -> bool {
        match self {
            Error::Capture { recoverable, .. } => !recoverable,
            Error::Encoder(_) | Error::Decoder(_) | Error::Protocol { .. } => true,
            Error::InvalidFrame(_) | Error::InvalidConfig(_) => true,
            // 过载丢帧是预期瞬态（见变体文档），管线丢帧继续，不拆会话。
            Error::EncoderDroppedFrame { .. } => false,
            Error::Network(_) | Error::Io(_) | Error::Timeout(_) => false,
        }
    }
}

/// 项目统一 Result 别名。
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_error_recoverability_drives_fatality() {
        let recoverable = Error::Capture {
            recoverable: true,
            msg: "ACCESS_LOST".into(),
        };
        assert!(!recoverable.is_fatal());

        let fatal = Error::Capture {
            recoverable: false,
            msg: "no output".into(),
        };
        assert!(fatal.is_fatal());
    }

    #[test]
    fn encoder_error_is_fatal() {
        assert!(Error::Encoder("init failed".into()).is_fatal());
    }

    /// 过载丢帧是预期瞬态：非致命（管线丢帧继续），与恒致命的
    /// [`Error::Encoder`]（真编码器故障）形成对照。
    #[test]
    fn encoder_dropped_frame_is_not_fatal() {
        let dropped = Error::EncoderDroppedFrame {
            msg: "VideoToolbox RealTime 过载".into(),
        };
        assert!(
            !dropped.is_fatal(),
            "过载丢帧不得拆会话（参考链无损，下一帧照常 P 帧）"
        );
        // 对照：同一错误族里的真编码器故障仍然恒致命。
        assert!(Error::Encoder("session broken".into()).is_fatal());
    }

    #[test]
    fn timeout_is_not_fatal() {
        assert!(!Error::Timeout(std::time::Duration::from_millis(16)).is_fatal());
    }

    #[test]
    fn protocol_error_reports_offset() {
        let e = Error::Protocol {
            offset: 12,
            msg: "bad magic".into(),
        };
        let s = e.to_string();
        assert!(s.contains("12"), "error text must include offset: {s}");
        assert!(s.contains("bad magic"));
    }
}
