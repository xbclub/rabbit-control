//! 脏区检测与重复帧判定。
//!
//! # 为什么这个 crate 必须 100% 可单测
//! 它是「静态画面 CPU 归零」的唯一实现点。如果它错了：
//! - 误判「无变化」→ 画面冻结（用户可见的严重 bug）
//! - 漏判「有变化」→ 白白编码，帧率下降
//!
//! 所以这里**只做纯算法**，输入输出都是 `CpuFrame` + `Rect`，
//! 不碰任何平台 API。GPU 路径的脏区（DXGI `GetFrameDirtyRects`）由
//! `rc-capture-win` 直接提供 `Rect`，再交给本 crate 的 [`merge_rects`] 处理。
//!
//! # 两种工作模式
//! - **平台提供脏区**（Windows DXGI）：用 [`merge_rects`] 合并即可，零 CPU 开销
//! - **平台不提供**（macOS / Linux / 软件采集）：用 [`TileDiff`] 做 tile 级比对

#![deny(missing_docs)]
#![deny(unsafe_code)]

use rc_types::{CpuFrame, Rect};

/// tile 边长（像素）。16 是常见选择：
/// - 太小 → 标记开销超过比对节省
/// - 太大 → 脏区粒度粗，编码浪费
pub const DEFAULT_TILE_SIZE: u32 = 16;

/// 合并多个脏区为一个最小包围矩形。
///
/// # 为什么不返回矩形列表
/// 硬件编码器（NVENC/VAAPI）的脏区支持对「矩形数量」有限制，
/// 且多个小矩形会导致编码器内部路径变慢。**合并成一个包围盒**
/// 在 1080p 办公场景下几乎不增加编码面积（脏区通常集中在局部），
/// 但实现简单、行为可预测。
///
/// # 参数
/// - `rects`: 待合并的脏区，可为空
/// - `bounds`: 屏幕尺寸，用于裁剪越界区域（GPU 上报偶有越界）
///
/// # 返回
/// 空矩形表示「无变化」。
pub fn merge_rects(rects: &[Rect], bounds: (u32, u32)) -> Rect {
    let (w, h) = bounds;
    let mut acc = Rect::EMPTY;
    for r in rects {
        let clipped = r.clamp_to(w, h);
        if clipped.is_empty() {
            continue;
        }
        acc = acc.union(&clipped);
    }
    acc
}

/// tile 级帧比对器。
///
/// 适用于平台不提供脏区的情况（macOS ScreenCaptureKit、Linux PipeWire、
/// 以及所有软件采集路径）。
///
/// # 算法
/// 1. 把画面切成 `tile_size × tile_size` 的块
/// 2. 每块计算 FNV-1a 64 位哈希
/// 3. 与上一帧对应块的哈希比对
/// 4. 收集不同的块 → 合并包围盒
///
/// # 为什么用 FNV 而不是 xxHash/CRC
/// - FNV 实现只有 10 行，无依赖，AI 改动不会出错
/// - 我们对碰撞极不敏感：碰撞意味着「漏掉一小块变化」，下一帧会补上
/// - 性能足够：1080p / 16px tile = 8160 块，全部哈希读取 ≈ 2ms（未优化）
///
/// # 为什么哈希整块而不是抽样
/// 抽样会漏掉细小的光标移动或文字变化，这些恰好是远程桌面最常见的脏区。
pub struct TileDiff {
    tile_size: u32,
    /// 上一帧各 tile 的哈希。长度 = tiles_x * tiles_y
    prev: Vec<u64>,
    /// 上一帧尺寸，变化时必须重置
    prev_dims: (u32, u32),
    /// 复用的输出缓冲，避免每帧分配
    changed: Vec<u32>,
}

impl TileDiff {
    /// 创建比对器。
    pub fn new(tile_size: u32) -> Self {
        assert!(tile_size > 0, "tile_size must be > 0");
        Self {
            tile_size,
            prev: Vec::new(),
            prev_dims: (0, 0),
            changed: Vec::new(),
        }
    }

    /// 使用默认 tile 尺寸创建。
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_TILE_SIZE)
    }

    /// 重置内部状态。分辨率变化、会话重建时必须调用。
    pub fn reset(&mut self) {
        self.prev.clear();
        self.prev_dims = (0, 0);
        self.changed.clear();
    }

    /// 计算当前帧的脏区。
    ///
    /// # 返回
    /// - `(Rect::EMPTY, false)`: 首帧或尺寸变化后，需要整帧编码
    ///   —— 注意此时返回 `false` 表示 `is_repeat`，首帧不是重复帧
    /// - `(rect, true)`: 与上一帧完全一致，`rect` 为空
    /// - `(rect, false)`: 有变化，`rect` 为脏区包围盒
    ///
    /// # 状态更新
    /// 每次调用都会把当前帧记为「上一帧」，无论是否有变化。
    pub fn update(&mut self, frame: &CpuFrame) -> DiffResult {
        let dims = (frame.width, frame.height);

        // 尺寸变化 → 重置，整帧视为脏
        if dims != self.prev_dims {
            self.prev_dims = dims;
            self.rehash(frame);
            return DiffResult {
                dirty: Rect::new(0, 0, dims.0, dims.1),
                is_repeat: false,
                reused: false,
            };
        }

        let tiles_x = dims.0.div_ceil(self.tile_size) as usize;
        let tiles_y = dims.1.div_ceil(self.tile_size) as usize;
        self.changed.clear();

        let stride = frame.stride as usize;
        let bpp = frame.format.bytes_per_pixel();
        let ts = self.tile_size as usize;

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let h = hash_tile(frame, stride, bpp, tx * ts, ty * ts, ts);
                let idx = ty * tiles_x + tx;
                let prev = self.prev.get(idx).copied();
                if prev != Some(h) {
                    self.changed.push(idx as u32);
                    // 立即更新，避免二次遍历
                    if idx < self.prev.len() {
                        self.prev[idx] = h;
                    }
                }
            }
        }

        if self.changed.is_empty() {
            return DiffResult {
                dirty: Rect::EMPTY,
                is_repeat: true,
                reused: false,
            };
        }

        // 合并所有变化 tile 为包围盒
        let mut min_tx = u32::MAX;
        let mut min_ty = u32::MAX;
        let mut max_tx = 0u32;
        let mut max_ty = 0u32;
        for &i in &self.changed {
            let tx = i % tiles_x as u32;
            let ty = i / tiles_x as u32;
            min_tx = min_tx.min(tx);
            min_ty = min_ty.min(ty);
            max_tx = max_tx.max(tx);
            max_ty = max_ty.max(ty);
        }

        let x = min_tx * self.tile_size;
        let y = min_ty * self.tile_size;
        let right = ((max_tx + 1) * self.tile_size).min(dims.0);
        let bottom = ((max_ty + 1) * self.tile_size).min(dims.1);

        DiffResult {
            dirty: Rect::new(x, y, right - x, bottom - y),
            is_repeat: false,
            reused: true,
        }
    }

    /// 重建全部 tile 哈希（尺寸变化时用）。
    fn rehash(&mut self, frame: &CpuFrame) {
        let tiles_x = frame.width.div_ceil(self.tile_size) as usize;
        let tiles_y = frame.height.div_ceil(self.tile_size) as usize;
        let n = tiles_x * tiles_y;
        self.prev.clear();
        self.prev.reserve(n);

        let stride = frame.stride as usize;
        let bpp = frame.format.bytes_per_pixel();
        let ts = self.tile_size as usize;

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                self.prev
                    .push(hash_tile(frame, stride, bpp, tx * ts, ty * ts, ts));
            }
        }
    }
}

/// 单帧比对结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffResult {
    /// 脏区包围盒。`EMPTY` 表示无变化。
    pub dirty: Rect,
    /// 与上一帧完全相同
    pub is_repeat: bool,
    /// 内部缓冲是否被复用（供测试断言，无业务含义）
    pub reused: bool,
}

impl DiffResult {
    /// 是否需要送去编码。
    pub fn needs_encode(&self) -> bool {
        !self.is_repeat && !self.dirty.is_empty()
    }

    /// 脏区占整帧的比例（0.0 ~ 1.0）。用于决策「全帧编码还是分区编码」。
    pub fn dirty_ratio(&self, width: u32, height: u32) -> f64 {
        let total = width as u64 * height as u64;
        if total == 0 {
            return 0.0;
        }
        self.dirty.area() as f64 / total as f64
    }
}

/// 计算一个 tile 的 FNV-1a 64 位哈希。
///
/// 越界的部分（最后一行/列可能不满一个 tile）用 0 填充参与计算——
/// 这样同一帧重复计算得到相同结果，且不同帧不会被误判相同。
///
/// # 必须逐字节混入，不能异或各通道
/// 曾经的实现把 B/G/R/A 四个通道异或成一个字节：
/// `255^255^255^255 == 0`，与全黑帧的 `0^0^0^0 == 0` 相同，
/// 于是「整块变白」会被判定为「没有变化」——画面冻结。
/// 这是真实发生过的 bug（`single_pixel_change_detected` 测试捕获）。
fn hash_tile(frame: &CpuFrame, stride: usize, bpp: usize, x0: usize, y0: usize, ts: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut h = FNV_OFFSET;
    let height = frame.height as usize;
    let width = frame.width as usize;
    let data = &frame.data;

    for y in y0..(y0 + ts).min(height) {
        let row_start = y * stride;
        let x_end = (x0 + ts).min(width);
        for x in x0..x_end {
            let off = row_start + x * bpp;
            for c in 0..bpp {
                if let Some(&byte) = data.get(off + c) {
                    h ^= byte as u64;
                    h = h.wrapping_mul(FNV_PRIME);
                }
            }
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_types::{PixelFormat, Resolution};

    fn frame(w: u32, h: u32) -> CpuFrame {
        CpuFrame {
            width: w,
            height: h,
            format: PixelFormat::Bgra8,
            stride: w * 4,
            data: vec![0; (w * h * 4) as usize],
        }
    }

    fn set_px(f: &mut CpuFrame, x: u32, y: u32, val: u8) {
        let off = (y * f.stride + x * 4) as usize;
        f.data[off..off + 4].fill(val);
    }

    #[test]
    fn first_frame_is_fully_dirty() {
        let mut d = TileDiff::with_defaults();
        let f = frame(64, 64);
        let r = d.update(&f);
        assert!(!r.is_repeat);
        assert_eq!(r.dirty, Rect::new(0, 0, 64, 64));
    }

    #[test]
    fn identical_frame_is_repeat() {
        let mut d = TileDiff::with_defaults();
        let f = frame(64, 64);
        d.update(&f);
        let r = d.update(&f);
        assert!(r.is_repeat, "identical frame must be detected as repeat");
        assert!(r.dirty.is_empty());
        assert!(!r.needs_encode());
    }

    #[test]
    fn single_pixel_change_detected() {
        let mut d = TileDiff::with_defaults();
        let f1 = frame(64, 64);
        d.update(&f1);

        let mut f2 = f1.clone();
        set_px(&mut f2, 20, 20, 255);
        let r = d.update(&f2);
        assert!(!r.is_repeat);
        assert!(!r.dirty.is_empty(), "single pixel change must be detected");
        assert!(r.needs_encode());
    }

    /// 关键用例：脏区必须精确到 tile，不能整帧
    #[test]
    fn dirty_region_is_localized() {
        let mut d = TileDiff::new(16);
        let f1 = frame(256, 256);
        d.update(&f1);

        let mut f2 = f1.clone();
        // 只改 (32..48, 32..48) 这个 tile
        set_px(&mut f2, 40, 40, 200);
        let r = d.update(&f2);

        assert_eq!(r.dirty, Rect::new(32, 32, 16, 16));
        assert!(r.dirty_ratio(256, 256) < 0.01);
    }

    #[test]
    fn two_separate_changes_merge_into_bounding_box() {
        let mut d = TileDiff::new(16);
        let f1 = frame(256, 256);
        d.update(&f1);

        let mut f2 = f1.clone();
        set_px(&mut f2, 20, 20, 100); // tile (1,1)
        set_px(&mut f2, 100, 100, 100); // tile (6,6)
        let r = d.update(&f2);

        // 包围盒应覆盖两者
        assert!(r.dirty.x <= 16);
        assert!(r.dirty.right() >= 112);
    }

    #[test]
    fn resolution_change_triggers_full_frame() {
        let mut d = TileDiff::with_defaults();
        d.update(&frame(64, 64));
        let r = d.update(&frame(128, 128));
        assert!(!r.is_repeat);
        assert_eq!(r.dirty, Rect::new(0, 0, 128, 128));
    }

    #[test]
    fn reset_clears_state() {
        let mut d = TileDiff::with_defaults();
        let f = frame(64, 64);
        d.update(&f);
        d.reset();
        let r = d.update(&f);
        // 重置后首帧应整帧脏
        assert_eq!(r.dirty, Rect::new(0, 0, 64, 64));
    }

    /// 非 4 字节对齐宽度也必须工作（GPU stride 常有 padding）
    #[test]
    fn padded_stride_handled() {
        let mut d = TileDiff::new(16);
        let mut f = CpuFrame {
            width: 100,
            height: 100,
            format: PixelFormat::Bgra8,
            stride: 512, // 128 px * 4，比 width*4 大
            data: vec![0; 512 * 100],
        };
        d.update(&f);
        let off = (50 * 512 + 50 * 4) as usize;
        f.data[off..off + 4].fill(255);
        let r = d.update(&f);
        assert!(!r.is_repeat);
    }

    #[test]
    fn merge_rects_empty_input() {
        assert!(merge_rects(&[], (1920, 1080)).is_empty());
    }

    #[test]
    fn merge_rects_clips_out_of_bounds() {
        let r = Rect::new(1900, 1070, 100, 100);
        let m = merge_rects(&[r], (1920, 1080));
        assert_eq!(m, Rect::new(1900, 1070, 20, 10));
    }

    #[test]
    fn merge_rects_ignores_fully_clipped() {
        let r = Rect::new(5000, 5000, 10, 10);
        assert!(merge_rects(&[r], (1920, 1080)).is_empty());
    }

    /// 奇数尺寸不得 panic（div_ceil 正确性）
    #[test]
    fn odd_dimensions_do_not_panic() {
        let mut d = TileDiff::new(16);
        let f = frame(15, 17);
        let r = d.update(&f);
        assert_eq!(r.dirty, Rect::new(0, 0, 15, 17));
    }

    #[test]
    fn resolution_helper_is_used() {
        // 保证测试里用的 Resolution 与 crate 一致
        assert!(Resolution::new(64, 64).is_even());
    }
}
