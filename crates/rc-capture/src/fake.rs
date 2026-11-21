//! 假后端：无 GPU、无系统时钟的 [`CaptureBackend`] 实现。
//!
//! # 为什么需要它
//! 采集抽象层的行为契约（C1 静止不出帧、C4 错误映射、C5 幂等重建）
//! 必须在**无 GPU 的 CI** 上可验证，否则每个平台后端都要在真机上
//! 重复踩同样的坑。假后端把契约固化成可执行的断言。
//!
//! # 时间注入
//! 假后端不读系统时钟，也不 `sleep`：逻辑时间按「每次成功出帧推进一个
//! 帧间隔」前进（间隔 = ceil(1 秒 / fps) 微秒）。测试因此确定且瞬时，
//! `next_frame` 的 `timeout` 参数被忽略（真后端用它阻塞等待）。

use std::time::Duration;

use crate::{CaptureBackend, CapturedFrame, DisplayInfo, FrameTexture, InvalidationReason};
use rc_types::{CaptureSeq, CpuFrame, Error, PixelFormat, Rect, Result, Timestamp};

/// 假后端的内容模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeContent {
    /// 静止画面：内容永不变化，`next_frame` 恒返回 `Ok(None)`（契约 C1）。
    Static,
    /// 动态画面：每个帧间隔产生一帧**新**内容，时间戳随之前进。
    Dynamic,
    /// 内容不变但平台仍持续交帧（模拟「无法过滤重复帧」的平台）：
    /// 首帧是新内容，之后每帧 `is_repeat = true`、时间戳停在首帧时刻，
    /// 而 `seq` 照常递增（见 [`CapturedFrame::seq`] 的语义）。
    Repeat,
}

/// 假后端配置。
#[derive(Debug, Clone)]
pub struct FakeBackendConfig {
    /// 被采集显示器的描述。
    pub display: DisplayInfo,
    /// 目标帧率。决定逻辑时钟步长（`ceil(1_000_000 / fps)` 微秒），必须 > 0。
    pub fps: u32,
    /// 内容模式（静止/动态/重复）。
    pub content: FakeContent,
    /// 动态帧上报的脏区。
    ///
    /// - `None`：整屏变化，脏区报告为全屏矩形；
    /// - `Some(rect)`：按此矩形上报，**越界部分会被裁剪**（复用
    ///   [`Rect::clamp_to`]），完全越界时报告 `None`。
    pub dirty_rect: Option<Rect>,
}

impl Default for FakeBackendConfig {
    fn default() -> Self {
        FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: rc_types::Resolution::new(1920, 1080),
                scale: 1.0,
                hdr: false,
            },
            fps: 60,
            content: FakeContent::Dynamic,
            dirty_rect: None,
        }
    }
}

impl FakeBackendConfig {
    /// 校验配置。
    ///
    /// 错误是调用方错误（不是运行期故障），统一用
    /// [`Error::InvalidConfig`]，信息里指明字段名。
    pub fn validate(&self) -> Result<()> {
        if self.fps == 0 {
            return Err(Error::InvalidConfig(
                "fake backend: fps must be > 0".to_string(),
            ));
        }
        let res = self.display.resolution;
        if res.width == 0 || res.height == 0 {
            return Err(Error::InvalidConfig(
                "fake backend: resolution must be non-zero".to_string(),
            ));
        }
        // CPU 兜底纹理按 BGRA 分配 stride；宽度乘 4 溢出说明分辨率荒谬。
        if res.width.checked_mul(4).is_none() {
            return Err(Error::InvalidConfig(
                "fake backend: resolution.width too large".to_string(),
            ));
        }
        Ok(())
    }
}

/// 假后端：按逻辑时钟出帧的 [`CaptureBackend`]。
///
/// 支持：
/// - 配置帧率与静止/动态/重复内容（[`FakeBackendConfig`]）；
/// - 注入 `ACCESS_LOST`（[`FakeBackend::inject_access_lost`]）：之后
///   `next_frame` 持续返回 `recoverable: true` 的错误、`poll_invalidation`
///   持续上报 [`InvalidationReason::AccessLost`]，直到 [`FakeBackend::rebuild`]。
///
/// rebuild 不重置序号与逻辑时钟：重建模拟的是「同一会话内的资源恢复」，
/// [`Timestamp`] 与 [`CaptureSeq`] 的连续性必须保持。
#[derive(Debug)]
pub struct FakeBackend {
    cfg: FakeBackendConfig,
    /// 逻辑时钟步长（微秒）。
    interval_us: u32,
    /// 下一帧新内容的时间戳。
    next_content_ts: u32,
    /// Repeat 模式复用的内容时间戳（None = 尚未产出过内容）。
    last_content_ts: Option<u32>,
    /// 下一帧的采集序号。
    next_seq: u64,
    /// 是否处于注入的 ACCESS_LOST 状态（rebuild 清除）。
    access_lost: bool,
    /// rebuild 调用次数（观测幂等性）。
    rebuilds: u64,
}

impl FakeBackend {
    /// 按配置创建假后端。配置非法时返回 [`Error::InvalidConfig`]。
    pub fn new(cfg: FakeBackendConfig) -> Result<Self> {
        cfg.validate()?;
        // 向上取整：60fps → 16667µs，保证「第一秒内恰好 fps 帧」
        // （16667 × 60 = 1_000_020 ≥ 1s，第 61 帧落在第二秒）。
        let interval_us = 1_000_000u32.div_ceil(cfg.fps);
        Ok(FakeBackend {
            cfg,
            interval_us,
            next_content_ts: 0,
            last_content_ts: None,
            next_seq: 0,
            access_lost: false,
            rebuilds: 0,
        })
    }

    /// 注入 `ACCESS_LOST`：模拟锁屏/权限收回。
    ///
    /// 此后 `next_frame` 每次都返回 [`InvalidationReason::AccessLost`]
    /// 对应的 `recoverable: true` 错误，直到 [`CaptureBackend::rebuild`]。
    pub fn inject_access_lost(&mut self) {
        self.access_lost = true;
    }

    /// `rebuild` 的累计调用次数（验证幂等性与重建是否真的发生）。
    pub fn rebuild_count(&self) -> u64 {
        self.rebuilds
    }

    /// 生成 CPU 兜底纹理：全零 BGRA 缓冲（假后端没有真实像素）。
    fn cpu_texture(&self) -> FrameTexture {
        let res = self.cfg.display.resolution;
        let stride = res.width * 4; // validate 已保证不溢出
        FrameTexture::Cpu(CpuFrame {
            width: res.width,
            height: res.height,
            format: PixelFormat::Bgra8,
            stride,
            data: vec![0; stride as usize * res.height as usize],
        })
    }

    /// 组装一帧并推进序号。
    fn emit_frame(&mut self, ts: u32, is_repeat: bool) -> CapturedFrame {
        let res = self.cfg.display.resolution;
        // 脏区：新帧按配置（裁剪到界内，空则 None），重复帧无脏区。
        let dirty = if is_repeat {
            None
        } else {
            match self.cfg.dirty_rect {
                Some(rect) => {
                    let clamped = rect.clamp_to(res.width, res.height);
                    if clamped.is_empty() {
                        None
                    } else {
                        Some(clamped)
                    }
                }
                None => Some(Rect::new(0, 0, res.width, res.height)),
            }
        };
        let seq = CaptureSeq(self.next_seq);
        self.next_seq += 1;
        CapturedFrame {
            texture: self.cpu_texture(),
            timestamp: Timestamp(ts),
            dirty,
            seq,
            is_repeat,
            resource_guard: None,
        }
    }
}

impl CaptureBackend for FakeBackend {
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>> {
        // 注入的 ACCESS_LOST 是粘性状态：真 DXGI 在重建前每次
        // AcquireNextFrame 都返回 ACCESS_LOST，假后端保持同一行为。
        if self.access_lost {
            return Err(InvalidationReason::AccessLost
                .to_capture_error("fake backend: injected ACCESS_LOST"));
        }
        match self.cfg.content {
            // C1：静止画面不出帧。真后端会阻塞到超时，假后端立即返回
            // （不 sleep，测试必须瞬时）。
            FakeContent::Static => Ok(None),
            FakeContent::Dynamic => {
                let ts = self.next_content_ts;
                self.next_content_ts = ts.saturating_add(self.interval_us);
                self.last_content_ts = Some(ts);
                Ok(Some(self.emit_frame(ts, false)))
            }
            FakeContent::Repeat => {
                // 首帧是新内容，其后内容不变：时间戳停在首帧，seq 照常递增。
                let (ts, is_repeat) = match self.last_content_ts {
                    Some(ts) => (ts, true),
                    None => {
                        let ts = self.next_content_ts;
                        self.next_content_ts = ts.saturating_add(self.interval_us);
                        self.last_content_ts = Some(ts);
                        (ts, false)
                    }
                };
                Ok(Some(self.emit_frame(ts, is_repeat)))
            }
        }
    }

    fn poll_invalidation(&mut self) -> Option<InvalidationReason> {
        if self.access_lost {
            Some(InvalidationReason::AccessLost)
        } else {
            None
        }
    }

    fn rebuild(&mut self) -> Result<()> {
        // 幂等：只清失效标志、计数 +1；序号与逻辑时钟刻意不重置。
        self.access_lost = false;
        self.rebuilds += 1;
        Ok(())
    }

    fn display_info(&self) -> DisplayInfo {
        self.cfg.display
    }
}
