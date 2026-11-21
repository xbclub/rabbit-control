//! 采集抽象层：平台无关的 [`CaptureBackend`] trait、帧载体与假后端。
//!
//! # 职责边界
//! - 本 crate 只定义「采集侧的形状」：trait、帧/显示器类型、失效原因与错误映射。
//! - 真正的平台实现（DXGI Desktop Duplication / ScreenCaptureKit / VAAPI）
//!   在各自的平台 crate 里实现 [`CaptureBackend`]，本 crate **不含任何平台依赖**。
//! - [`fake::FakeBackend`] 是纯逻辑时钟的假后端，供回环测试与无 GPU 的 CI 使用。
//!
//! # 时间语义（契约 C2）
//! [`CapturedFrame::timestamp`] 是**内容变化时刻**，不是采集调用返回时刻。
//! 延迟测量的起点是它；把它当成「返回时刻」会让整条链路的延迟归因失真。
//! 抽象层本身不产生时间（无系统时钟）：时间由实现注入（平台后端取
//! `LastPresentTime` 一类的时间戳；假后端按帧率推进逻辑时钟）。
//!
//! # 静止语义（契约 C1）
//! 屏幕无变化时 [`CaptureBackend::next_frame`] 返回 `Ok(None)`（阻塞到超时），
//! **绝不**为凑帧率编造重复帧。静态画面下采集 CPU 占用必须接近 0。
//!
//! # 错误与恢复（契约 C4 / C5）
//! - 采集失败统一用 [`Error::Capture`] 表达，其 `recoverable` 标志决定上层
//!   是「重建后继续」还是「报错退出」；`ACCESS_LOST` 等可重建的失效必须映射为
//!   `recoverable: true`（见 [`InvalidationReason::to_capture_error`]）。
//! - [`CaptureBackend::rebuild`] 必须幂等：失效后可连续调用而不报错。
//!
//! # 纹理载体
//! [`FrameTexture`] 以 `Cpu` 变体为兜底，平台变体（`D3D11` / `CVPixelBuffer` /
//! `DmaBuf`）只携带**不透明句柄**（`*mut c_void` 或零依赖句柄结构），平台 crate
//! 负责在其内部把原生对象转成句柄并在 Drop 时回收，抽象层不触碰 FFI。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod fake;

pub use fake::{FakeBackend, FakeBackendConfig, FakeContent};
pub use rc_types::{Error, Result};

use std::fmt;
use std::time::Duration;

use rc_types::{CaptureSeq, CpuFrame, DisplayId, Rect, Resolution, Timestamp};

/// 采集后端。所有实现必须满足：无屏幕变化时不产生帧（契约 C1）。
///
/// # 生命周期协议
/// 1. 正常循环：反复调用 [`CaptureBackend::next_frame`]。
/// 2. `next_frame` 返回 [`Error::Capture`] 后，用
///    [`CaptureBackend::poll_invalidation`] 查失效原因；
/// 3. 无论何种原因，调用 [`CaptureBackend::rebuild`]（幂等，可连续调用）后
///    继续采集。
///
/// 实现必须在独立线程上驱动（契约 C6）：`next_frame` 的阻塞不得持有会
/// 阻塞编码/调度线程的锁。
pub trait CaptureBackend: Send {
    /// 阻塞等待下一帧。超时返回 `Ok(None)`（不是错误）。
    ///
    /// 返回 `Ok(None)` 的两种情形：
    /// - 超时时间内屏幕无变化（契约 C1，静止画面的常态）；
    /// - 采集已被挂起（如锁屏），此时应配合
    ///   [`CaptureBackend::poll_invalidation`] 判断是否需要重建。
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;

    /// 检查是否需要重建（分辨率变化、ACCESS_LOST 等）。
    ///
    /// 返回 `Some(reason)` 表示内部资源已失效，必须调用
    /// [`CaptureBackend::rebuild`]；同一失效在重建前应持续上报。
    fn poll_invalidation(&mut self) -> Option<InvalidationReason>;

    /// 重建内部资源。调用前必须已处理 [`CaptureBackend::poll_invalidation`]。
    ///
    /// 幂等（契约 C5）：连续调用不报错、不产生副作用叠加。
    fn rebuild(&mut self) -> Result<()>;

    /// 当前捕获的显示器信息。
    fn display_info(&self) -> DisplayInfo;
}

/// 采集到的一帧。
///
/// # 字段语义
/// - [`timestamp`](CapturedFrame::timestamp) 是**内容变化时刻**（契约 C2）；
/// - [`dirty`](CapturedFrame::dirty) 是平台上报的脏区（已裁剪到分辨率内），
///   平台不提供脏区时为 `None`（此时上层按全帧处理）；
/// - [`seq`](CapturedFrame::seq) 每产出一帧（含重复帧）都前进，
///   与「帧序号会因丢帧跳号」的 [`FrameSeq`](rc_types::FrameSeq) 分离；
/// - [`is_repeat`](CapturedFrame::is_repeat) 在平台能判断「内容与上一帧相同」
///   时置位，上层据此丢弃重复帧而不是送去编码。
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// 帧内容载体：CPU 像素缓冲或平台 GPU 纹理句柄。
    pub texture: FrameTexture,
    /// 内容变化时刻（不是采集返回时刻）。
    pub timestamp: Timestamp,
    /// 脏区（像素坐标，已裁剪到显示器分辨率内）。平台提供时填写。
    pub dirty: Option<Rect>,
    /// 采集序号。每产出一帧（含重复帧）单调 +1。
    pub seq: CaptureSeq,
    /// 是否为内容未变化的重复帧。平台能判断时填写。
    pub is_repeat: bool,
    /// 可选的平台纹理 RAII 保活守卫（如 macOS `CFRetained<CVPixelBuffer>`）。
    ///
    /// 当帧跨线程送入异步编码器时，由该守卫保证底层 GPU 缓冲存活到本帧
    /// （及其 `Clone`）全部 `Drop`，并在丢弃或编码完成后立即归还平台缓冲池，
    /// 避免悬垂指针或固定环缓存占满缓冲池。CPU 假后端为 `None`。
    pub resource_guard: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
}

/// 帧内容的载体。
///
/// 平台变体只携带**不透明句柄**，不引入平台依赖：
/// - `D3D11`：Windows 上是 `ID3D11Texture2D` 的 COM 指针（以 `*mut c_void`
///   传递，平台 crate 负责类型还原与引用计数）；
/// - `CVPixelBuffer`：macOS ScreenCaptureKit 交出的像素缓冲指针；
/// - `DmaBuf`：Linux 上的 DMA-BUF 描述（见 [`DmaBufHandle`]）。
///
/// # 生命周期警告
/// GPU 句柄仅在**下一次采集**之前有效（DXGI 的帧在下次 `AcquireNextFrame`
/// 前必须释放）。上层若要异步使用，必须在平台 crate 内拷贝到自有纹理，
/// 不要把句柄留存到后续帧。
#[derive(Clone)]
pub enum FrameTexture {
    /// CPU 内存中的像素缓冲。软件采集与软件编码路径的兜底。
    Cpu(CpuFrame),
    /// Windows D3D11 纹理（`ID3D11Texture2D*`，不透明句柄）。
    #[cfg(windows)]
    D3D11(*mut std::ffi::c_void),
    /// macOS CoreVideo 像素缓冲（`CVPixelBufferRef`，不透明句柄）。
    #[cfg(target_os = "macos")]
    CVPixelBuffer(*mut std::ffi::c_void),
    /// Linux DMA-BUF。
    #[cfg(target_os = "linux")]
    DmaBuf(DmaBufHandle),
}

impl fmt::Debug for FrameTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不展开像素数据（可能是几 MB），句柄只打印地址。
        match self {
            FrameTexture::Cpu(frame) => f.debug_tuple("Cpu").field(frame).finish(),
            #[cfg(windows)]
            FrameTexture::D3D11(ptr) => write!(f, "D3D11({ptr:p})"),
            #[cfg(target_os = "macos")]
            FrameTexture::CVPixelBuffer(ptr) => write!(f, "CVPixelBuffer({ptr:p})"),
            #[cfg(target_os = "linux")]
            FrameTexture::DmaBuf(handle) => f.debug_tuple("DmaBuf").field(handle).finish(),
        }
    }
}

// SAFETY: 平台句柄的跨线程移动是安全的——`CVPixelBuffer` 与 D3D11 纹理
// 都是引用计数对象（CFRelease / Release 可在任意线程调用），DMA-BUF 的
// fd 是普通整数；CPU 缓冲是 `Vec<u8>` 本身就是 `Send`。**所有权**语义
// （句柄随本枚举移动，析构责任在平台 crate）与线程无关。采集线程 →
// 管线线程的交接（契约 C8）依赖这一点。
#[allow(unsafe_code)]
unsafe impl Send for FrameTexture {}

// SAFETY: 共享引用不产生新的句柄操作（`Debug` 只读地址），见上方 Send 依据。
#[allow(unsafe_code)]
unsafe impl Sync for FrameTexture {}

/// Linux DMA-BUF 句柄：跨进程共享的图形缓冲描述。
///
/// 零依赖写法（不引 `libc`/`rustix`）：`fd` 即 Unix 原始文件描述符（`i32`）。
/// 引用归持有采集会话的平台 crate 所有，抽象层不关闭它。
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DmaBufHandle {
    /// DMA-BUF 文件描述符（不接管所有权，平台 crate 负责关闭）。
    pub fd: i32,
    /// 缓冲宽（像素）。
    pub width: u32,
    /// 缓冲高（像素）。
    pub height: u32,
    /// 每行字节数（可能大于 `width * bpp`，因对齐填充）。
    pub stride: u32,
    /// 缓冲像素格式。
    pub format: rc_types::PixelFormat,
    /// 驱动相关的布局修饰符（格式协商用，无修饰时为 0）。
    pub modifier: u64,
}

/// 采集资源失效的原因。
///
/// 「失效」指内部资源需要重建才能继续采集。原因本身决定
/// [`CapturedFrame`] 错误的 `recoverable` 标志（契约 C4，见
/// [`InvalidationReason::is_recoverable`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvalidationReason {
    /// 分辨率变化（改显示器设置、窗口移动到不同 DPI 的屏）。
    ResolutionChanged,
    /// 显示器拓扑变化（插拔显示器、主屏切换）。
    DisplayChanged,
    /// 会话锁定/切换（锁屏、UAC 安全桌面）。
    SessionLocked,
    /// 系统收回采集权限（DXGI `ACCESS_LOST`、SCK 停止回调）。
    /// 契约 C4：必须映射为 `recoverable: true`，锁屏后不得永久黑屏。
    AccessLost,
    /// GPU 设备被移除（TDR / 驱动重置），需要重建整个设备链。
    DeviceRemoved,
    /// HDR 状态切换，色彩空间需要重新协商。
    HdrToggled,
    /// 未知失效。按不可恢复处理，避免不可诊断的无限重建循环。
    Unknown,
}

impl InvalidationReason {
    /// 该失效是否可通过 [`CaptureBackend::rebuild`] 恢复。
    ///
    /// 所有**已知**失效（分辨率/拓扑/锁屏/权限/设备/HDR）都定义了明确的
    /// 重建路径，因此可恢复；只有 [`InvalidationReason::Unknown`] 按致命
    /// 处理——原因不明时盲目重建可能陷入死循环。
    pub fn is_recoverable(self) -> bool {
        !matches!(self, InvalidationReason::Unknown)
    }

    /// 把失效原因映射成带 `recoverable` 标志的采集错误（契约 C4）。
    ///
    /// `recoverable = self.is_recoverable()`，与 [`Error::is_fatal`] 联动：
    /// 可恢复错误上层应走「重建 → 继续采集」，不可恢复错误应上报退出。
    pub fn to_capture_error(self, msg: impl Into<String>) -> Error {
        Error::Capture {
            recoverable: self.is_recoverable(),
            msg: msg.into(),
        }
    }
}

/// 被采集显示器的描述。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayInfo {
    /// 显示器标识（平台层的 output index 或稳定 ID）。
    pub id: DisplayId,
    /// 当前分辨率（像素）。
    pub resolution: Resolution,
    /// HiDPI 缩放系数（逻辑分辨率 = 分辨率 / scale）。
    pub scale: f32,
    /// 是否处于 HDR 输出模式。
    pub hdr: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeBackend, FakeBackendConfig, FakeContent};
    use rc_types::{CaptureSeq, DisplayId};

    /// 构造 1920×1080、指定内容模式的假后端配置。
    fn cfg(content: FakeContent) -> FakeBackendConfig {
        FakeBackendConfig {
            content,
            ..FakeBackendConfig::default()
        }
    }

    /// 构造并解包假后端（测试配置必然合法，失败即测试自身写错）。
    fn backend(content: FakeContent) -> FakeBackend {
        FakeBackend::new(cfg(content)).expect("测试配置必须合法")
    }

    /// 任务卡 T03：假后端基础行为——按请求的帧率出帧。
    #[test]
    fn fake_backend_returns_frames_at_requested_rate() {
        for fps in [30u32, 60, 120] {
            let mut b = FakeBackend::new(FakeBackendConfig {
                fps,
                ..cfg(FakeContent::Dynamic)
            })
            .expect("fps > 0 必须合法");
            // 收集逻辑时间第一秒（[0, 1s)）内产出的帧数，应恰好等于 fps。
            let mut frames_in_first_second = 0u32;
            loop {
                let frame = b
                    .next_frame(Duration::from_millis(16))
                    .expect("动态内容不应出错")
                    .expect("动态内容必须持续出帧");
                if frame.timestamp.0 >= 1_000_000 {
                    break;
                }
                frames_in_first_second += 1;
            }
            assert_eq!(
                frames_in_first_second, fps,
                "{fps}fps 配置下第一秒应恰好产出 {fps} 帧"
            );
        }
    }

    /// 任务卡 T03：契约 C1——静止内容不出帧。
    #[test]
    fn fake_backend_returns_none_when_static() {
        let mut b = backend(FakeContent::Static);
        for _ in 0..3 {
            assert!(
                b.next_frame(Duration::from_millis(16))
                    .expect("静止不是错误")
                    .is_none(),
                "静止画面 next_frame 必须返回 Ok(None)，不能编造帧"
            );
        }
        // 静止不等于失效：不应上报 invalidation。
        assert_eq!(b.poll_invalidation(), None);
    }

    /// 任务卡 T03：契约 C4——失效原因到 recoverable 标志的错误映射。
    #[test]
    fn capture_error_recoverable_flag_maps_correctly() {
        // 所有已知失效都可重建恢复（C4：ACCESS_LOST 明确要求 recoverable）。
        for reason in [
            InvalidationReason::ResolutionChanged,
            InvalidationReason::DisplayChanged,
            InvalidationReason::SessionLocked,
            InvalidationReason::AccessLost,
            InvalidationReason::DeviceRemoved,
            InvalidationReason::HdrToggled,
        ] {
            let err = reason.to_capture_error("injected");
            assert!(
                matches!(
                    err,
                    Error::Capture {
                        recoverable: true,
                        ..
                    }
                ),
                "{reason:?} 应映射为 recoverable = true"
            );
            assert!(!err.is_fatal(), "{reason:?} 可恢复错误不应判为致命");
        }
        // 未知失效按致命处理。
        let unknown = InvalidationReason::Unknown.to_capture_error("injected");
        assert!(matches!(
            unknown,
            Error::Capture {
                recoverable: false,
                ..
            }
        ));
        assert!(unknown.is_fatal(), "Unknown 应判为致命");

        // 全链路验证：注入 ACCESS_LOST 后 next_frame 返回 recoverable 错误，
        // poll_invalidation 上报原因（直到 rebuild）。
        let mut b = backend(FakeContent::Dynamic);
        b.inject_access_lost();
        let err = b
            .next_frame(Duration::from_millis(16))
            .expect_err("注入后必须报错");
        assert!(
            matches!(
                err,
                Error::Capture {
                    recoverable: true,
                    ..
                }
            ),
            "注入的 ACCESS_LOST 应产生 recoverable = true 的错误，实际 {err:?}"
        );
        assert!(!err.is_fatal());
        assert_eq!(
            b.poll_invalidation(),
            Some(InvalidationReason::AccessLost),
            "失效必须同时能被 poll_invalidation 观测到"
        );
    }

    /// 任务卡 T03：契约 C5——rebuild 幂等，可连续调用。
    #[test]
    fn rebuild_is_idempotent_on_fake() {
        let mut b = backend(FakeContent::Dynamic);
        b.inject_access_lost();
        // 连续 rebuild 三次：全部成功，且不产生叠加副作用。
        for i in 0..3 {
            assert!(
                b.rebuild().is_ok(),
                "第 {} 次 rebuild 必须成功（C5 幂等）",
                i + 1
            );
        }
        assert_eq!(b.rebuild_count(), 3);
        assert_eq!(b.poll_invalidation(), None, "重建后失效状态应被清除");
        // 重建后采集恢复。
        assert!(
            b.next_frame(Duration::from_millis(16))
                .expect("重建后不应出错")
                .is_some(),
            "重建后必须恢复出帧"
        );
    }

    /// 任务卡 T03：越界脏区必须被裁剪到分辨率内。
    #[test]
    fn dirty_rects_are_clipped_to_bounds() {
        // 部分越界：只保留落在 1920×1080 内的部分。
        let mut b = FakeBackend::new(FakeBackendConfig {
            dirty_rect: Some(Rect::new(1900, 1070, 100, 100)),
            ..cfg(FakeContent::Dynamic)
        })
        .expect("配置合法");
        let frame = b
            .next_frame(Duration::from_millis(16))
            .expect("动态内容")
            .expect("必须出帧");
        assert_eq!(
            frame.dirty,
            Some(Rect::new(1900, 1070, 20, 10)),
            "越界脏区应被裁剪到 1920×1080 内"
        );

        // 完全越界：裁剪后为空，应报告为 None（空区域不送编码）。
        let mut b = FakeBackend::new(FakeBackendConfig {
            dirty_rect: Some(Rect::new(5000, 5000, 10, 10)),
            ..cfg(FakeContent::Dynamic)
        })
        .expect("配置合法");
        let frame = b
            .next_frame(Duration::from_millis(16))
            .expect("动态内容")
            .expect("必须出帧");
        assert_eq!(frame.dirty, None, "完全越界的脏区裁剪为空后应报 None");
    }

    /// 任务卡 T03：契约 C2——时间戳（内容变化时刻）单调递增。
    #[test]
    fn frame_timestamp_is_monotonic() {
        let mut b = backend(FakeContent::Dynamic);
        let mut prev = None;
        for _ in 0..200 {
            let frame = b
                .next_frame(Duration::from_millis(16))
                .expect("动态内容")
                .expect("必须出帧");
            if let Some(p) = prev {
                assert!(
                    frame.timestamp > p,
                    "时间戳必须严格递增：{} -> {}",
                    p.0,
                    frame.timestamp.0
                );
            }
            prev = Some(frame.timestamp);
        }
    }

    /// 任务卡 T03：seq 语义——重复帧也占用（递增）采集序号。
    #[test]
    fn capture_seq_increases_even_for_repeats() {
        let mut b = backend(FakeContent::Repeat);
        let first = b
            .next_frame(Duration::from_millis(16))
            .expect("Repeat 模式")
            .expect("首帧是新内容");
        assert!(!first.is_repeat, "首帧是新内容，不是重复帧");

        let mut prev_seq = first.seq;
        let prev_ts = first.timestamp;
        for _ in 0..4 {
            let frame = b
                .next_frame(Duration::from_millis(16))
                .expect("Repeat 模式")
                .expect("Repeat 模式持续交帧");
            assert!(frame.is_repeat, "首帧之后应标记为重复帧");
            assert_eq!(frame.timestamp, prev_ts, "内容未变，时间戳不应前进");
            assert!(
                frame.seq > prev_seq,
                "重复帧的采集序号也必须递增：{prev_seq} -> {}",
                frame.seq
            );
            prev_seq = frame.seq;
        }
        // seq 从 0 连续递增，没有跳号。
        assert_eq!(first.seq, CaptureSeq(0));
        assert_eq!(prev_seq, CaptureSeq(4));
    }

    /// 显示器信息原样透传（含 id）。
    #[test]
    fn fake_backend_reports_display_info() {
        let b = backend(FakeContent::Static);
        let info = b.display_info();
        assert_eq!(info.id, DisplayId(0));
        assert_eq!(info.resolution, Resolution::new(1920, 1080));
        assert!((info.scale - 1.0).abs() < f32::EPSILON);
        assert!(!info.hdr);
    }
}
