//! 像素格式、矩形、分辨率。
//!
//! 只放**与平台无关**的表示。平台纹理句柄不在此处。

use crate::error::{Error, Result};

/// 像素格式。
///
/// 只列出本项目实际用到的格式。**不要**为了「通用」而加一堆用不到的格式——
/// 每个格式都会增加「转换路径」的测试负担。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 位 BGRA，每像素 4 字节。Windows DXGI 原生格式。
    Bgra8,
    /// 8 位 RGBA，每像素 4 字节。
    Rgba8,
    /// YUV 4:2:0，NV12 布局（Y 平面 + 交错 UV），硬件编码器的标准输入。
    Nv12,
    /// YUV 4:2:0，I420 布局（Y + U + V 三平面）。部分 VAAPI 驱动只支持这个。
    I420,
}

impl PixelFormat {
    /// 每像素字节数（用于计算最小 stride）。
    ///
    /// **注意**：NV12/I420 是 4:2:0 子采样，平均每像素 1.5 字节，
    /// 但这里的返回值用于 **luma 平面**的 stride 校验，所以返回 1。
    /// 平面尺寸计算请用 [`PixelFormat::plane_dims`]。
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => 4,
            PixelFormat::Nv12 | PixelFormat::I420 => 1,
        }
    }

    /// 是否为 YUV 格式（用于判断是否需要色彩空间转换）。
    pub const fn is_yuv(self) -> bool {
        matches!(self, PixelFormat::Nv12 | PixelFormat::I420)
    }

    /// 是否为平面格式。
    pub const fn is_planar(self) -> bool {
        self.is_yuv()
    }

    /// 给定尺寸，返回该格式所需的「最小 CPU 缓冲字节数」。
    ///
    /// - BGRA8 / RGBA8: `w * h * 4`
    /// - NV12: `w * h + 2 * (w/2 * h/2)` = `w * h * 3 / 2`（4:2:0）
    /// - I420: 同 NV12
    ///
    /// 宽高为奇数时向上取整（YUV 4:2:0 要求偶数尺寸，奇数输入是调用方错误）。
    pub fn min_buffer_size(self, width: u32, height: u32) -> usize {
        let pixels = (width as usize).saturating_mul(height as usize);
        match self {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => pixels.saturating_mul(4),
            PixelFormat::Nv12 | PixelFormat::I420 => {
                // 4:2:0: Y 全采样 + UV 各 1/4 => 1.5 字节/像素
                pixels.saturating_mul(3) / 2
            }
        }
    }
}

/// 矩形区域（像素坐标）。用于脏区。
///
/// 不变量：`x + width <= 屏幕宽`、`y + height <= 屏幕高`，
/// 且宽高为 0 的矩形表示「空区域」（应被跳过，不要送去编码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    /// 左上角 x
    pub x: u32,
    /// 左上角 y
    pub y: u32,
    /// 宽
    pub width: u32,
    /// 高
    pub height: u32,
}

impl Rect {
    /// 构造并检查是否为空。
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// 空矩形。
    pub const EMPTY: Rect = Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    /// 是否为空（无像素）。
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// 面积（像素数）。
    #[inline]
    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// 右边界（不含）。
    #[inline]
    pub const fn right(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    /// 下边界（不含）。
    #[inline]
    pub const fn bottom(&self) -> u32 {
        self.y.saturating_add(self.height)
    }

    /// 与另一矩形是否相交。
    pub fn intersects(&self, other: &Rect) -> bool {
        !self.is_empty()
            && !other.is_empty()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// 合并为包含两者的最小矩形。
    ///
    /// 用于把多个脏区合成为一个「编码区域」。
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    /// 裁剪到给定边界内。
    ///
    /// 越界的脏区（GPUD 上报的坐标偶尔会超出，尤其是 DPI 变化瞬间）必须裁剪，
    /// 否则会在编码器里产生越界访问 → 崩溃或花屏。
    pub fn clamp_to(&self, width: u32, height: u32) -> Rect {
        if self.is_empty() || width == 0 || height == 0 {
            return Rect::EMPTY;
        }
        let x = self.x.min(width);
        let y = self.y.min(height);
        let right = self.right().min(width);
        let bottom = self.bottom().min(height);
        Rect {
            x,
            y,
            width: right.saturating_sub(x),
            height: bottom.saturating_sub(y),
        }
    }

    /// 校验是否完全落在给定尺寸内。
    pub fn validate_within(&self, width: u32, height: u32) -> Result<()> {
        if self.right() > width || self.bottom() > height {
            return Err(Error::InvalidFrame(format!(
                "rect {}x{}+{}+{} exceeds {}x{}",
                self.width, self.height, self.x, self.y, width, height
            )));
        }
        Ok(())
    }
}

/// 分辨率。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Resolution {
    /// 宽（像素）
    pub width: u32,
    /// 高（像素）
    pub height: u32,
}

impl Resolution {
    /// 构造
    pub const fn new(width: u32, height: u32) -> Self {
        Resolution { width, height }
    }

    /// 是否为偶数尺寸（YUV 4:2:0 编码的前置条件）。
    pub const fn is_even(&self) -> bool {
        self.width % 2 == 0 && self.height % 2 == 0
    }

    /// 向下取整到偶数尺寸。
    ///
    /// 硬件编码器普遍要求偶数尺寸；奇数输入必须先裁剪。
    pub const fn to_even(&self) -> Resolution {
        Resolution {
            width: self.width & !1,
            height: self.height & !1,
        }
    }

    /// 该分辨率下的像素总数。
    pub const fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_buffer_is_4_bytes_per_pixel() {
        assert_eq!(
            PixelFormat::Bgra8.min_buffer_size(1920, 1080),
            1920 * 1080 * 4
        );
    }

    #[test]
    fn nv12_buffer_is_1_5_bytes_per_pixel() {
        assert_eq!(
            PixelFormat::Nv12.min_buffer_size(1920, 1080),
            1920 * 1080 * 3 / 2
        );
    }

    #[test]
    fn rect_empty_detection() {
        assert!(Rect::new(0, 0, 0, 10).is_empty());
        assert!(Rect::new(0, 0, 10, 0).is_empty());
        assert!(!Rect::new(0, 0, 1, 1).is_empty());
    }

    #[test]
    fn rect_area() {
        assert_eq!(Rect::new(0, 0, 100, 50).area(), 5000);
    }

    #[test]
    fn rect_intersects() {
        let a = Rect::new(0, 0, 100, 100);
        let b = Rect::new(50, 50, 100, 100);
        let c = Rect::new(200, 200, 10, 10);
        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));
    }

    #[test]
    fn rect_union_covers_both() {
        let a = Rect::new(10, 10, 20, 20);
        let b = Rect::new(100, 100, 10, 10);
        let u = a.union(&b);
        assert_eq!(u.x, 10);
        assert_eq!(u.y, 10);
        assert_eq!(u.right(), 110);
        assert_eq!(u.bottom(), 110);
    }

    #[test]
    fn rect_union_with_empty() {
        let a = Rect::new(10, 10, 20, 20);
        assert_eq!(a.union(&Rect::EMPTY), a);
        assert_eq!(Rect::EMPTY.union(&a), a);
    }

    /// 这是最关键的用例：DPGD 上报的越界脏区必须被裁剪，不能进编码器
    #[test]
    fn rect_clamp_truncates_overflow() {
        let r = Rect::new(1900, 1070, 100, 100);
        let c = r.clamp_to(1920, 1080);
        assert_eq!(c, Rect::new(1900, 1070, 20, 10));
    }

    #[test]
    fn rect_clamp_fully_outside_yields_empty() {
        let r = Rect::new(5000, 5000, 10, 10);
        assert!(r.clamp_to(1920, 1080).is_empty());
    }

    #[test]
    fn rect_validate_rejects_overflow() {
        let r = Rect::new(1900, 1070, 100, 100);
        assert!(r.validate_within(1920, 1080).is_err());
        assert!(Rect::new(0, 0, 1920, 1080)
            .validate_within(1920, 1080)
            .is_ok());
    }

    #[test]
    fn resolution_even_check() {
        assert!(Resolution::new(1920, 1080).is_even());
        assert!(!Resolution::new(1921, 1080).is_even());
    }

    #[test]
    fn resolution_to_even_rounds_down() {
        assert_eq!(
            Resolution::new(1921, 1081).to_even(),
            Resolution::new(1920, 1080)
        );
    }
}
