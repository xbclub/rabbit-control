//! 假渲染器：无 GPU、无系统时钟的 [`Renderer`] 实现。
//!
//! # 用途
//! 把呈现抽象层的行为契约（D8 呈现打点在调用返回后、resize 生效、
//! D7 的光标状态缓存）固化成无 GPU 的 CI 可执行断言；同时供回环测试
//! 填充呈现段。
//!
//! # 时间模型
//! 不读系统时钟、不 sleep：维护逻辑时钟，每次 [`Renderer::present`]
//! 先推进「单帧呈现耗时」再打点（D8：时间戳在呈现调用**返回后**取），
//! 因此返回值 = 本次呈现的完成时刻，包含调用自身的耗时。
//!
//! # 失败语义
//! 假渲染器不模拟失败：[`Renderer::present`] 恒成功。D6 的失败路径
//! （device lost → 返回错误、上层重建）由平台渲染器实现。

use crate::cursor::CursorShape;
use crate::Renderer;
use rc_decode::DecodedFrame;
use rc_types::{Error, Result, Timestamp};

/// 默认单帧呈现耗时（微秒）：2ms，对应任务卡的呈现延迟预算。
pub const DEFAULT_PRESENT_COST_US: u32 = 2_000;

/// 光标状态（D7 的状态部分：位置与可见性的缓存快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    /// 光标位置（像素坐标，呈现目标坐标系）。
    pub pos: (i32, i32),
    /// 光标是否可见。
    pub visible: bool,
}

impl Default for CursorState {
    /// 原点、可见——真实桌面会话的常见初始状态。
    fn default() -> Self {
        CursorState {
            pos: (0, 0),
            visible: true,
        }
    }
}

/// 假渲染器配置。
#[derive(Debug, Clone)]
pub struct FakeRendererConfig {
    /// 呈现目标初始尺寸（像素）。
    pub size: (u32, u32),
    /// 模拟的单帧呈现耗时（微秒）。决定逻辑时钟步长与返回的呈现时刻。
    pub present_cost_us: u32,
    /// 初始 vsync 状态。默认必须为关（D2：开 vsync 每帧 +16.7ms）。
    pub vsync: bool,
    /// 逻辑时钟起点（微秒）。用于把假渲染器对齐到调用方的时间线。
    pub clock_start_us: u32,
}

impl Default for FakeRendererConfig {
    /// 1080p、2ms 呈现耗时、vsync 关、时钟从 0 起。
    fn default() -> Self {
        FakeRendererConfig {
            size: (1920, 1080),
            present_cost_us: DEFAULT_PRESENT_COST_US,
            vsync: false,
            clock_start_us: 0,
        }
    }
}

impl FakeRendererConfig {
    /// 校验配置。非法项以 [`Error::InvalidConfig`] 返回，信息含字段名。
    pub fn validate(&self) -> Result<()> {
        if self.size.0 == 0 || self.size.1 == 0 {
            return Err(Error::InvalidConfig(
                "fake renderer: size must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

/// 假渲染器：记录呈现时刻与状态的 [`Renderer`]。
#[derive(Debug)]
pub struct FakeRenderer {
    cfg: FakeRendererConfig,
    /// 逻辑时钟（微秒）：下一次 present 调用的时刻。
    clock_us: u32,
    /// 当前 vsync 开关状态。
    vsync: bool,
    /// 光标状态缓存（D7）。
    cursor: CursorState,
    /// 形状缓存（D7）：当前形状 id；`None` = 从未设置过形状。
    cursor_shape_id: Option<u64>,
    /// 形状缓存（D7）：模拟的「上传」次数（同 id 不重传）。
    cursor_shape_uploads: u64,
    /// 成功呈现次数。
    presents: u64,
    /// 每次呈现的完成时刻（D8：返回后打点），按呈现顺序追加。
    present_ts_history: Vec<Timestamp>,
    /// 最近一帧的尺寸（观测帧确实流过呈现器）。
    last_frame_size: Option<(u32, u32)>,
}

impl FakeRenderer {
    /// 按配置创建假渲染器。配置非法返回 [`Error::InvalidConfig`]。
    pub fn new(cfg: FakeRendererConfig) -> Result<Self> {
        cfg.validate()?;
        let clock_us = cfg.clock_start_us;
        let vsync = cfg.vsync;
        Ok(FakeRenderer {
            cfg,
            clock_us,
            vsync,
            cursor: CursorState::default(),
            cursor_shape_id: None,
            cursor_shape_uploads: 0,
            presents: 0,
            present_ts_history: Vec::new(),
            last_frame_size: None,
        })
    }

    /// 当前呈现目标尺寸（`resize` 更新）。
    pub fn size(&self) -> (u32, u32) {
        self.cfg.size
    }

    /// 当前 vsync 开关状态。
    pub fn vsync(&self) -> bool {
        self.vsync
    }

    /// 光标状态缓存（最近一次 `set_cursor` 的快照）。
    pub fn cursor(&self) -> CursorState {
        self.cursor
    }

    /// 当前缓存的形状 id（D7 形状缓存）；从未设置过则 `None`。
    pub fn cursor_shape_id(&self) -> Option<u64> {
        self.cursor_shape_id
    }

    /// 形状「上传」次数（D7 形状缓存）：同 id 重复设置不计数，
    /// 换 id 才 +1 —— 与平台渲染器的缓存语义对齐，无 GPU 可测。
    pub fn cursor_shape_uploads(&self) -> u64 {
        self.cursor_shape_uploads
    }

    /// 设置 / 更新光标形状（D7 形状缓存：同 id 命中不重传）。
    ///
    /// 假渲染器不触碰 GPU，只记录缓存决策；形状本身在
    /// [`CursorShape::new`] 已校验，这里不重复。
    pub fn set_cursor_shape(&mut self, shape: &CursorShape) {
        if self.cursor_shape_id == Some(shape.id()) {
            return; // 形状缓存命中：同 id 不重传。
        }
        self.cursor_shape_id = Some(shape.id());
        self.cursor_shape_uploads += 1;
    }

    /// 成功呈现次数。
    pub fn present_count(&self) -> u64 {
        self.presents
    }

    /// 最近一次呈现的完成时刻；从未呈现过则 `None`。
    pub fn last_present_ts(&self) -> Option<Timestamp> {
        self.present_ts_history.last().copied()
    }

    /// 全部呈现完成时刻（按呈现顺序）。
    pub fn present_ts_history(&self) -> &[Timestamp] {
        &self.present_ts_history
    }

    /// 最近一帧的尺寸 `(width, height)`；从未呈现过则 `None`。
    pub fn last_frame_size(&self) -> Option<(u32, u32)> {
        self.last_frame_size
    }
}

impl Renderer for FakeRenderer {
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp> {
        // D8：先推进逻辑时钟（模拟 Present 调用耗时），**返回后**打点，
        // 因此时间戳包含本次呈现自身的耗时；在入口打点会偏小。
        let done = Timestamp(self.clock_us.saturating_add(self.cfg.present_cost_us));
        self.clock_us = done.0;
        self.presents += 1;
        self.present_ts_history.push(done);
        self.last_frame_size = Some((frame.width, frame.height));
        Ok(done)
    }

    fn resize(&mut self, size: (u32, u32)) -> Result<()> {
        if size.0 == 0 || size.1 == 0 {
            return Err(Error::InvalidConfig(
                "fake renderer: size must be non-zero".to_string(),
            ));
        }
        self.cfg.size = size;
        Ok(())
    }

    fn set_vsync(&mut self, on: bool) {
        self.vsync = on;
    }

    fn set_cursor(&mut self, pos: (i32, i32), visible: bool) {
        // D7 状态部分：只缓存，不进视频管线。
        self.cursor = CursorState { pos, visible };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::CursorShape;
    use rc_capture::FrameTexture;
    use rc_types::{CpuFrame, PixelFormat};

    /// 构造 w×h 的解码帧（CPU 兜底纹理，decode_done_ts = 1000）。
    fn decoded_frame(w: u32, h: u32) -> DecodedFrame {
        let stride = w * 4;
        DecodedFrame {
            texture: FrameTexture::Cpu(CpuFrame {
                width: w,
                height: h,
                format: PixelFormat::Bgra8,
                stride,
                data: vec![0; stride as usize * h as usize],
            }),
            decode_done_ts: Timestamp(1_000),
            width: w,
            height: h,
        }
    }

    /// 任务卡 T05（D8）：呈现时间戳在 present 调用返回后打。
    #[test]
    fn fake_renderer_present_timestamp_after_call() {
        let cost = 2_000u32;
        let mut r = FakeRenderer::new(FakeRendererConfig {
            size: (64, 64),
            present_cost_us: cost,
            ..FakeRendererConfig::default()
        })
        .expect("配置合法");

        // 第一次呈现：入口时钟是 0，返回值必须是 0 + cost——打点发生在
        // 调用返回后、包含本次呈现耗时；若在入口打点会得到 0。
        let first = r.present(decoded_frame(64, 64)).expect("假渲染器不失败");
        assert_eq!(
            first,
            Timestamp(cost),
            "呈现时间戳必须包含本次呈现耗时（调用返回后打点，D8）"
        );
        assert_eq!(r.present_ts_history(), &[Timestamp(cost)][..]);
        assert_eq!(r.last_present_ts(), Some(Timestamp(cost)));

        // 第二次：在第一次完成时刻的基础上再推进 cost。
        let second = r.present(decoded_frame(64, 64)).expect("假渲染器不失败");
        assert_eq!(second, Timestamp(cost * 2));
        assert_eq!(
            r.present_ts_history(),
            &[Timestamp(cost), Timestamp(cost * 2)][..]
        );
        assert_eq!(r.present_count(), 2);
        assert_eq!(r.last_frame_size(), Some((64, 64)), "应记录最近一帧的尺寸");
    }

    /// 任务卡 T05（D7 形状缓存）：同 id 不重传、换 id 才重传。
    #[test]
    fn fake_renderer_caches_cursor_shape_by_id() {
        let mut r = FakeRenderer::new(FakeRendererConfig::default()).expect("配置合法");
        assert_eq!(r.cursor_shape_id(), None, "初始无形状");
        assert_eq!(r.cursor_shape_uploads(), 0);

        let arrow = CursorShape::new(7, 8, 8, (0, 0), vec![0; 8 * 8 * 4]).expect("合法形状");
        r.set_cursor_shape(&arrow);
        assert_eq!(r.cursor_shape_id(), Some(7));
        assert_eq!(r.cursor_shape_uploads(), 1, "首次设置算一次上传");

        // 同 id 重复设置（低频形状刷新的常见情况）：形状缓存命中，不重传。
        r.set_cursor_shape(&arrow);
        assert_eq!(r.cursor_shape_uploads(), 1, "同 id 必须命中缓存");

        // 换 id（系统换光标样式）：重传一次。
        let beam = CursorShape::new(8, 8, 8, (0, 0), vec![0; 8 * 8 * 4]).expect("合法形状");
        r.set_cursor_shape(&beam);
        assert_eq!(r.cursor_shape_id(), Some(8));
        assert_eq!(r.cursor_shape_uploads(), 2, "新 id 才重传");

        // 换回旧 id：对缓存而言仍是新一次上传（单槽缓存，不保留历史）。
        r.set_cursor_shape(&arrow);
        assert_eq!(r.cursor_shape_uploads(), 3);
    }

    /// 任务卡 T05：resize 生效；非法尺寸被拒绝且不改变当前状态。
    #[test]
    fn renderer_resize_updates_size() {
        let mut r = FakeRenderer::new(FakeRendererConfig::default()).expect("配置合法");
        assert_eq!(r.size(), (1920, 1080), "初始尺寸应等于配置尺寸");

        // 经 trait 本身 resize（泛型间接调用，验证契约而非固有方法）。
        fn resize_via_trait<R: Renderer>(r: &mut R, size: (u32, u32)) -> Result<()> {
            r.resize(size)
        }
        resize_via_trait(&mut r, (800, 600)).expect("合法尺寸必须成功");
        assert_eq!(r.size(), (800, 600), "resize 后尺寸必须更新");

        // 非法尺寸：拒绝，且当前尺寸保持不变。
        let err = resize_via_trait(&mut r, (0, 600)).expect_err("零宽必须被拒绝");
        assert!(
            matches!(err, Error::InvalidConfig(ref msg) if msg.contains("size")),
            "应返回指明 size 的 InvalidConfig，实际 {err:?}"
        );
        assert_eq!(r.size(), (800, 600), "失败的 resize 不得改变尺寸");

        // D2 / D7 的状态部分：vsync 默认关、可切换；光标状态可缓存更新。
        assert!(!r.vsync(), "默认必须关闭 vsync（D2）");
        r.set_vsync(true);
        assert!(r.vsync());
        r.set_cursor((10, 20), false);
        assert_eq!(
            r.cursor(),
            CursorState {
                pos: (10, 20),
                visible: false
            }
        );
        r.set_cursor((30, 40), true);
        assert_eq!(
            r.cursor(),
            CursorState {
                pos: (30, 40),
                visible: true
            },
            "光标状态应被覆盖更新（缓存语义）"
        );
    }
}
