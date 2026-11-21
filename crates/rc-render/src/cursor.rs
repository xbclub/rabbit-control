//! 光标形状与呈现几何（D7 的跨平台数据部分）。
//!
//! # 职责边界
//! - [`CursorShape`]：光标位图形状（id + 尺寸 + 热点 + BGRA 像素），
//!   由会话层从输入/控制通道取得后交给平台渲染器；
//! - [`cursor_rect`]：光标在呈现目标上的**可见矩形**计算——含越界裁剪，
//!   纯函数、无 GPU，平台实现据此生成自己的顶点数据；
//! - 形状缓存（同 id 不重传 GPU）由平台渲染器实现，抽象层只定义
//!   「形状」与「几何」两个纯数据 / 纯计算部分。
//!
//! # 坐标与像素约定
//! - `pos`（见 [`crate::CursorState`]）与裁剪矩形都在**呈现目标坐标系**
//!   （物理像素，原点左上）；
//! - [`CursorShape::bgra`] 是 BGRA8、**直通（非预乘）alpha**，每像素 4 字节；
//! - `hotspot` 是形状内的锚点（形状左上角到热点的偏移），`pos` 指向热点。
//!
//! # D7 延迟语义
//! 光标**不进视频管线**（否则位置滞后一帧）：位置经
//! [`Renderer::set_cursor`](crate::Renderer::set_cursor) 独立低延迟更新，
//! 呈现时按**当下缓存的位置**本地叠加（延迟感知为 0）；形状变化低频，
//! 经形状缓存只在 id 变化时上传一次 GPU。

use rc_types::{Error, Result};

/// 光标形状（D7）：一张 BGRA 位图 + 热点，以 `id` 标识同一形状。
///
/// `id` 是形状缓存的键：会话层对**内容相同**的形状必须复用同一 id
/// （典型做法：发送端为每个形状分配序号），渲染器据此避免每帧重传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    id: u64,
    width: u32,
    height: u32,
    /// 热点（形状内偏移，像素）。`pos - hotspot` = 形状左上角。
    hotspot: (u32, u32),
    /// BGRA8 直通 alpha 像素，行主序、无行间填充（stride == width * 4）。
    bgra: Vec<u8>,
}

impl CursorShape {
    /// 创建并校验形状。
    ///
    /// 非法项（任一维为 0、像素长度 ≠ `w*h*4`、热点越界）以
    /// [`Error::InvalidConfig`] 拒绝，信息含字段名。
    pub fn new(
        id: u64,
        width: u32,
        height: u32,
        hotspot: (u32, u32),
        bgra: Vec<u8>,
    ) -> Result<Self> {
        let shape = CursorShape {
            id,
            width,
            height,
            hotspot,
            bgra,
        };
        shape.validate()?;
        Ok(shape)
    }

    /// 校验尺寸 / 像素长度 / 热点。
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidConfig(format!(
                "cursor shape: width/height must be non-zero, got {}x{}",
                self.width, self.height
            )));
        }
        let expected = self.width as usize * self.height as usize * 4;
        if self.bgra.len() != expected {
            return Err(Error::InvalidConfig(format!(
                "cursor shape: bgra length {} != width*height*4 = {expected}",
                self.bgra.len()
            )));
        }
        if self.hotspot.0 >= self.width || self.hotspot.1 >= self.height {
            return Err(Error::InvalidConfig(format!(
                "cursor shape: hotspot {:?} out of bounds for {}x{}",
                self.hotspot, self.width, self.height
            )));
        }
        Ok(())
    }

    /// 形状 id（缓存键）。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 形状宽（像素）。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 形状高（像素）。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 热点（形状内偏移）。
    pub fn hotspot(&self) -> (u32, u32) {
        self.hotspot
    }

    /// BGRA 像素（直通 alpha，stride == `width * 4`）。
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }
}

/// 光标在呈现目标上的可见矩形（已裁剪）与对应的纹理 uv 子矩形。
///
/// 平台实现把 `px` 转成自己的裁剪空间（Metal/D3D 都是 y 向上的 NDC，
/// 但 y 翻转方向不同，故本类型停留在像素空间）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorRect {
    /// 可见区域（物理像素，半开区间 `[x0, x1) × [y0, y1)`，原点左上）。
    pub px: (f32, f32, f32, f32),
    /// 对应的 uv 子矩形 `(u0, v0, u1, v1)`：左上 `(u0, v0)`、右下 `(u1, v1)`，
    /// v 向下（纹理原点左上）。裁剪掉的边缘不参与采样。
    pub uv: (f32, f32, f32, f32),
}

/// 计算光标的可见矩形（含越界裁剪）。
///
/// - `shape_size`：形状 `(width, height)`（像素）；
/// - `hotspot`：热点（形状内偏移）；
/// - `pos`：光标位置（呈现目标坐标，指向热点，可在目标外）；
/// - `viewport`：呈现目标尺寸（物理像素）。
///
/// 完全在目标外（或形状 / 目标尺寸为 0）返回 `None`（不绘制）；
/// 部分越界时返回**交集**矩形与对应的 uv 子矩形（被裁掉的边缘不拉伸）。
pub fn cursor_rect(
    shape_size: (u32, u32),
    hotspot: (u32, u32),
    pos: (i32, i32),
    viewport: (u32, u32),
) -> Option<CursorRect> {
    let (w, h) = (shape_size.0 as i64, shape_size.1 as i64);
    if w == 0 || h == 0 || viewport.0 == 0 || viewport.1 == 0 {
        return None;
    }
    // 形状左上角 = pos - hotspot（可为负，i64 防溢出）。
    let tlx = pos.0 as i64 - hotspot.0 as i64;
    let tly = pos.1 as i64 - hotspot.1 as i64;
    // 形状的完整矩形（半开区间）。
    let (brx, bry) = (tlx + w, tly + h);
    // 与呈现目标 [0, vw) × [0, vh) 求交。
    let vw = viewport.0 as i64;
    let vh = viewport.1 as i64;
    let x0 = tlx.max(0);
    let y0 = tly.max(0);
    let x1 = brx.min(vw);
    let y1 = bry.min(vh);
    if x0 >= x1 || y0 >= y1 {
        return None; // 完全在目标外
    }
    // 交集对应形状内的像素偏移 → uv 子矩形。
    let u0 = (x0 - tlx) as f32 / w as f32;
    let u1 = (x1 - tlx) as f32 / w as f32;
    let v0 = (y0 - tly) as f32 / h as f32;
    let v1 = (y1 - tly) as f32 / h as f32;
    Some(CursorRect {
        px: (x0 as f32, y0 as f32, x1 as f32, y1 as f32),
        uv: (u0, v0, u1, v1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 校验拒绝非法形状：零尺寸 / 像素长度不符 / 热点越界。
    #[test]
    fn shape_validation_rejects_invalid() {
        let err = CursorShape::new(1, 0, 8, (0, 0), vec![0; 8 * 4]).expect_err("零宽必须拒绝");
        assert!(
            matches!(err, Error::InvalidConfig(ref m) if m.contains("non-zero")),
            "{err:?}"
        );

        let err = CursorShape::new(1, 8, 8, (0, 0), vec![0; 10]).expect_err("像素长度必须校验");
        assert!(
            matches!(err, Error::InvalidConfig(ref m) if m.contains("bgra length")),
            "{err:?}"
        );

        let err =
            CursorShape::new(1, 8, 8, (8, 0), vec![0; 8 * 8 * 4]).expect_err("热点越界必须拒绝");
        assert!(
            matches!(err, Error::InvalidConfig(ref m) if m.contains("hotspot")),
            "{err:?}"
        );
    }

    /// 热点决定矩形位置：同一 pos，热点 (0,0) 与 (2,3) 的左上角差 (2,3)。
    #[test]
    fn fully_inside_rect_uses_hotspot_offset() {
        // pos (10, 10)，热点 (2, 3) → 左上 (8, 7)，8x8 → 右下 (16, 15)。
        let r = cursor_rect((8, 8), (2, 3), (10, 10), (64, 64)).expect("完全在目标内");
        assert_eq!(r.px, (8.0, 7.0, 16.0, 15.0));
        assert_eq!(r.uv, (0.0, 0.0, 1.0, 1.0), "未裁剪时 uv 覆盖整张形状");

        // 热点 (0,0)：左上角就是 pos。
        let r0 = cursor_rect((8, 8), (0, 0), (10, 10), (64, 64)).expect("完全在目标内");
        assert_eq!(r0.px, (10.0, 10.0, 18.0, 18.0));
    }

    /// 左上越界：负 pos 使形状部分在目标外，交集矩形与 uv 子矩形同步裁剪
    /// （被裁掉的边缘不拉伸、不重复采样）。
    #[test]
    fn partially_clipped_top_left_trims_px_and_uv() {
        // pos (0,0)、热点 (4,4) → 左上 (-4,-4)，可见部分为形状的右下 4x4。
        let r = cursor_rect((8, 8), (4, 4), (0, 0), (64, 64)).expect("部分可见");
        assert_eq!(r.px, (0.0, 0.0, 4.0, 4.0));
        assert_eq!(r.uv, (0.5, 0.5, 1.0, 1.0), "只采样形状右下 4x4 对应的 uv");
    }

    /// 右下越界：贴住右下角时只画进界内的部分。
    #[test]
    fn partially_clipped_bottom_right_trims_px_and_uv() {
        // 目标 64x64，形状 8x8，热点 (0,0)，pos (60, 60) → 矩形 [60,68) 越界 4px。
        let r = cursor_rect((8, 8), (0, 0), (60, 60), (64, 64)).expect("部分可见");
        assert_eq!(r.px, (60.0, 60.0, 64.0, 64.0));
        assert_eq!(r.uv, (0.0, 0.0, 0.5, 0.5), "只采样形状左上 4x4 对应的 uv");
    }

    /// 完全越界（四向各一例）→ None（不绘制）。
    #[test]
    fn fully_outside_returns_none() {
        // 左上角方向完全出去。
        assert!(cursor_rect((8, 8), (0, 0), (-9, -9), (64, 64)).is_none());
        // 右侧完全出去。
        assert!(cursor_rect((8, 8), (0, 0), (64, 0), (64, 64)).is_none());
        // 下方完全出去。
        assert!(cursor_rect((8, 8), (0, 0), (0, 64), (64, 64)).is_none());
        // 形状 / 目标尺寸为 0（防御）。
        assert!(cursor_rect((0, 8), (0, 0), (1, 1), (64, 64)).is_none());
        assert!(cursor_rect((8, 8), (0, 0), (1, 1), (0, 64)).is_none());
    }
}
