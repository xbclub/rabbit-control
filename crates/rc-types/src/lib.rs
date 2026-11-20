//! 共享类型与错误定义。
//!
//! # 稳定性契约
//! 本 crate 被所有其他 crate 依赖，改动它会导致整个 workspace 重编。
//! 因此：
//! - 只放「数据结构」「错误类型」「ID newtype」
//! - **不要**放 trait（trait 放各自的抽象 crate）
//! - **不要**引入重依赖（本 crate 目前只依赖 `thiserror`）
//! - 新增字段一律用 `Option` 或标记 `#[non_exhaustive]`

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod error;
pub mod ids;
pub mod pixel;

pub use error::{Error, Result};
pub use ids::{CaptureSeq, DisplayId, FrameSeq, SessionId, Timestamp};
pub use pixel::{PixelFormat, Rect, Resolution};

/// 一帧的语义类型。
///
/// 调度器据此决定「能否丢弃」——IDR 永远不丢，P 帧可丢。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    /// 关键帧，解码不依赖任何前序帧。不可丢。
    Idr,
    /// 间预测帧，依赖前序帧。可丢，但丢弃后必须通知解码端。
    P,
}

impl FrameKind {
    /// 该帧是否允许被调度器丢弃。
    #[inline]
    pub const fn is_droppable(self) -> bool {
        matches!(self, FrameKind::P)
    }
}

/// 一帧的原始像素缓冲（CPU 内存）。
///
/// 平台相关的 GPU 纹理句柄**不在这里**（那会污染 `rc-types` 的稳定性）。
/// 平台层用 `rc-capture` 里各自的 `FrameTexture` 承载 GPU 句柄，
/// 只有需要 CPU 访问时才转换成本类型。
#[derive(Clone)]
pub struct CpuFrame {
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
    /// 像素格式
    pub format: PixelFormat,
    /// 每行字节数（可能 > width * bpp，因为对齐填充）
    pub stride: u32,
    /// 连续的像素数据，长度必须 >= stride * height
    pub data: Vec<u8>,
}

impl CpuFrame {
    /// 校验 `data` 长度是否与 `stride * height` 匹配。
    ///
    /// 这是 AI 最容易写错的地方之一（把 stride 当成 width * bpp），
    /// 所以做成公开方法，供各 crate 的入口处调用。
    pub fn validate(&self) -> Result<()> {
        let bpp = self.format.bytes_per_pixel();
        let min_stride = (self.width as usize).saturating_mul(bpp);
        if (self.stride as usize) < min_stride {
            return Err(Error::InvalidFrame(format!(
                "stride {} < width {} * bpp {}",
                self.stride, self.width, bpp
            )));
        }
        let need = (self.stride as usize).saturating_mul(self.height as usize);
        if self.data.len() < need {
            return Err(Error::InvalidFrame(format!(
                "data len {} < stride {} * height {} = {}",
                self.data.len(),
                self.stride,
                self.height,
                need
            )));
        }
        Ok(())
    }
}

impl std::fmt::Debug for CpuFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 data 内容（可能是几 MB）
        f.debug_struct("CpuFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("stride", &self.stride)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, stride: u32, data_len: usize) -> CpuFrame {
        CpuFrame {
            width,
            height,
            format: PixelFormat::Bgra8,
            stride,
            data: vec![0; data_len],
        }
    }

    #[test]
    fn validate_accepts_exact_fit() {
        // 1920 * 4 = 7680 stride，1080 行
        let f = frame(1920, 1080, 7680, 7680 * 1080);
        assert!(f.validate().is_ok());
    }

    #[test]
    fn validate_accepts_padded_stride() {
        // GPU 常见：stride 对齐到 256
        let f = frame(1920, 1080, 7936, 7936 * 1080);
        assert!(f.validate().is_ok());
    }

    #[test]
    fn validate_rejects_stride_smaller_than_width() {
        let f = frame(1920, 1080, 100, 100 * 1080);
        let err = f.validate().unwrap_err();
        assert!(matches!(err, Error::InvalidFrame(_)));
    }

    #[test]
    fn validate_rejects_short_buffer() {
        let f = frame(1920, 1080, 7680, 7680 * 1079);
        assert!(f.validate().is_err());
    }

    #[test]
    fn idr_is_not_droppable() {
        assert!(!FrameKind::Idr.is_droppable());
        assert!(FrameKind::P.is_droppable());
    }
}
