//! Metal / CoreVideo / QuartzCore 的 FFI 边界。
//!
//! 本 crate 的**全部 `unsafe` 都集中在这个模块**：
//! - 设备 / 离屏 `CAMetalLayer` / 命令队列 / `CVMetalTextureCache` 的创建；
//! - `CVPixelBuffer` → `CVMetalTexture` → `MTLTexture` 的零拷贝包装（D1）；
//! - 渲染管线（MSL 着色器：NV12→BGRA 转换在 GPU shader 里做）；
//! - 「编码 → presentDrawable → commit → 完成回调释放源」的呈现链（D8）。
//!
//! 对外（`renderer.rs`）只暴露**安全**的包装方法，每个 `unsafe` 块都带
//! `// SAFETY:` 注释。本模块的存在使 crate 其余部分可以 `deny(unsafe_code)`。
//!
//! # 线程模型
//! [`GpuContext`] 声明 `Send`（同 `rc-capture-macos` 的 `StreamHandles` 约定）：
//! 对象随渲染器一起被**移动**（所有权移交），访问始终发生在持有 `&mut self`
//! 的单一线程上（`Renderer: Send` 的使用约定），不存在并发访问；ObjC/CF 的
//! release 本身线程安全。

// 见模块文档：本模块是 crate 内唯一允许 unsafe 的地方。
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFRetained, CGSize};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess, CVMetalTexture,
    CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVReturn,
};
use objc2_foundation::{NSError, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLFunction, MTLLibrary, MTLLoadAction,
    MTLOrigin, MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder,
    MTLRenderPassDescriptor, MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use rc_render::cursor::{CursorRect, CursorShape};

use crate::shader::{self, CursorUniforms};

// ─────────────────────────────────────────────────────────────────────────────
// 源帧的零拷贝包装（D1）
// ─────────────────────────────────────────────────────────────────────────────

/// Metal 纹理的强引用（`CVMetalTextureGetTexture` 的产物）。
type MetalTexture = Retained<ProtocolObject<dyn MTLTexture>>;

/// Metal drawable 的强引用（`nextDrawable` 的产物）。
type MetalDrawable = Retained<ProtocolObject<dyn CAMetalDrawable>>;

/// 命令缓冲完成回调的 block 类型（释放本帧源引用）。
type PresentDoneBlock = RcBlock<dyn Fn(NonNull<ProtocolObject<dyn MTLCommandBuffer>>)>;

/// 呈现一帧所需的 GPU 纹理（由 [`GpuContext::wrap_buffer`] 产出）。
///
/// 全程不触碰 CPU 像素（D1：禁止 CPU 回读）。变体对应不同的像素格式与
/// 着色器路径。
pub(crate) enum FrameSource {
    /// 单平面 BGRA：直接采样（passthrough 着色器）。
    Bgra {
        /// plane 0 包装出的纹理。
        texture: MetalTexture,
    },
    /// 双平面 NV12（'420v' / '420f'）：shader 里做 BT.709 转换。
    Nv12 {
        /// luma（plane 0，R8Unorm）。
        luma: MetalTexture,
        /// chroma（plane 1，RG8Unorm）。
        chroma: MetalTexture,
        /// '420f'（full range）为 true；矩阵不同。
        full_range: bool,
    },
}

/// 必须活到 GPU 用完源纹理的引用集合。
///
/// CoreVideo 要求 `CVMetalTexture` 包装在 MTLTexture 使用期间保持存活；
/// `CVPixelBuffer`（IOSurface 的持有者）同理。两者随命令缓冲的完成回调
/// 一并释放（见 [`GpuContext::draw_and_present`]）。
pub(crate) struct FrameKeepAlive {
    /// present 期间对源像素缓冲的 +1 retain。
    _buffer: CFRetained<CVPixelBuffer>,
    /// CVMetalTexture 包装（plane 0 / plane 1）。
    _cv_textures: Vec<CFRetained<CVMetalTexture>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// D7 光标：形状缓存（GPU 纹理）+ 单次叠加绘制
// ─────────────────────────────────────────────────────────────────────────────

/// 光标形状的 GPU 缓存项（D7 形状缓存）。
///
/// 由 [`GpuContext::upload_cursor`] 上传一次，之后每次呈现直接复用；
/// 只有形状 id 变化（系统换光标样式）才重新上传。
pub(crate) struct CursorPlane {
    /// 形状 id（缓存键）。
    id: u64,
    /// 形状纹理（BGRA8Unorm，ShaderRead）。
    texture: MetalTexture,
    width: u32,
    height: u32,
    hotspot: (u32, u32),
}

impl CursorPlane {
    /// 是否可以继续缓存复用（同 id 且尺寸 / 热点一致）。
    ///
    /// id 相同但尺寸不同视为不匹配（防御调用方 id 复用错误）：走重新上传。
    pub(crate) fn matches(&self, shape: &CursorShape) -> bool {
        self.id == shape.id()
            && self.width == shape.width()
            && self.height == shape.height()
            && self.hotspot == shape.hotspot()
    }

    /// 形状 id。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// 形状尺寸 `(width, height)`（像素）。
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 热点（形状内偏移）。
    pub(crate) fn hotspot(&self) -> (u32, u32) {
        self.hotspot
    }
}

/// 一次呈现里的光标叠加绘制参数（由渲染器按「当下」光标状态构造）。
pub(crate) struct CursorDraw {
    /// 光标纹理（克隆 [`CursorPlane`] 的强引用，随命令缓冲完成回调释放）。
    texture: MetalTexture,
    /// 顶点参数（NDC 矩形 + uv 子矩形，已含越界裁剪）。
    uniforms: CursorUniforms,
}

impl CursorDraw {
    /// 由缓存的形状平面 + 裁剪后的可见矩形构造叠加绘制。
    ///
    /// `rect` 来自 [`rc_render::cursor::cursor_rect`]（像素坐标系，原点
    /// 左上、y 向下）；这里转成 NDC（y 向上）：
    /// `x' = x / vw * 2 - 1`、`y' = 1 - 2 * y / vh`。
    pub(crate) fn new(plane: &CursorPlane, rect: &CursorRect, viewport: (u32, u32)) -> Self {
        let (vw, vh) = (viewport.0 as f32, viewport.1 as f32);
        let (px0, py0, px1, py1) = rect.px;
        let (u0, v0, u1, v1) = rect.uv; // v0 = 上、v1 = 下（uv 原点左上，v 向下）
        CursorDraw {
            texture: plane.texture.clone(),
            uniforms: CursorUniforms {
                // (x_left, y_bottom, x_right, y_top)
                rect: [
                    px0 / vw * 2.0 - 1.0,
                    1.0 - 2.0 * py1 / vh,
                    px1 / vw * 2.0 - 1.0,
                    1.0 - 2.0 * py0 / vh,
                ],
                // (u_left, v_bottom, u_right, v_top)：v_bottom 是较大的 v
                uv: [u0, v1, u1, v0],
            },
        }
    }
}

/// 四字符码转可读字符串（错误信息用）。
fn fourcc(fmt: u32) -> String {
    let bytes = fmt.to_be_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 支持的 CVPixelBuffer 格式（本地大写常量：绑定层的 k 前缀常量进 match
/// 模式会触发 non_upper_case_globals 告警）。
const FMT_BGRA: u32 = kCVPixelFormatType_32BGRA;
/// '420v'（NV12 video range，SCK / VideoToolbox 常规输出）。
const FMT_420V: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
/// '420f'（NV12 full range）。
const FMT_420F: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;

// ─────────────────────────────────────────────────────────────────────────────
// GPU 上下文：device + layer + queue + texture cache + pipeline states
// ─────────────────────────────────────────────────────────────────────────────

/// 渲染管线的四个状态（同一份 MSL 编译出的 fragment 组合）。
struct Pipelines {
    /// BGRA 直通。
    bgra: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// NV12 video range（'420v'，SCK / VideoToolbox 的常规输出）。
    nv12_video: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// NV12 full range（'420f'）。
    nv12_full: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// 光标本地叠加（D7）：直通 alpha 采样 + SourceAlpha 混合。
    cursor: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

/// 拥有 Metal 呈现所需的全部系统对象。
///
/// `MTLDevice` 保留字段：光标形状上传（[`GpuContext::set_cursor_shape`]）
/// 需要在呈现路径之外创建纹理；queue / texture cache / pipeline state
/// 强引用所在设备（Metal 生命周期规则）。
pub(crate) struct GpuContext {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    cache: CFRetained<CVMetalTextureCache>,
    pipelines: Pipelines,
    /// D7 形状缓存：当前光标纹理（同 id 不重传）。放在这里（而非渲染器）
    /// 是为了搭 [`GpuContext`] 既有的 Send 声明便车——光标纹理只在本
    /// 上下文内存活，不单独穿越线程边界。
    cursor_plane: Option<CursorPlane>,
}

// SAFETY: 这组对象随 MetalRenderer 一起被移动（Send = 跨线程移交所有权）。
// 访问始终发生在持有 &mut self 的单一线程上（Renderer: Send 的使用约定），
// 不存在并发访问；释放（Drop）发生在最终 owning 线程，ObjC/CF 的 release
// 是线程安全的。同 rc-capture-macos 的 StreamHandles。
unsafe impl Send for GpuContext {}

impl GpuContext {
    /// 创建离屏上下文：`CAMetalLayer` 不挂任何 NSView/NSWindow，
    /// 也能 `nextDrawable`（无窗口测试路径）。
    pub(crate) fn new_offscreen(size: (u32, u32), vsync: bool) -> Result<Self, String> {
        // CAMetalLayer 继承 CALayer 的 +layer 类方法（生成 +1 实例，安全方法）。
        let layer = CAMetalLayer::layer();
        Self::init(layer, size, vsync)
    }

    /// 采用调用方已有的 `CAMetalLayer`（NSView / 窗口路径，可选构造）。
    ///
    /// `layer` 指向调用方保持存活的 `CAMetalLayer` 实例；本上下文 retain
    /// 一份强引用，调用方对 layer 的所有权不受影响。
    pub(crate) fn adopt_layer(
        layer: *mut c_void,
        size: (u32, u32),
        vsync: bool,
    ) -> Result<Self, String> {
        if layer.is_null() {
            return Err("layer 指针为空".to_string());
        }
        // SAFETY: 调用方契约保证指针指向合法存活的 CAMetalLayer（或子类）；
        // retain 升级为 +1 强引用（返回 None 仅当对象正在销毁）。
        let layer = unsafe { Retained::retain(layer.cast::<CAMetalLayer>()) }
            .ok_or_else(|| "retain CAMetalLayer 失败".to_string())?;
        Self::init(layer, size, vsync)
    }

    /// 公共初始化：设备、队列、纹理缓存、layer 配置与渲染管线。
    fn init(layer: Retained<CAMetalLayer>, size: (u32, u32), vsync: bool) -> Result<Self, String> {
        // MTLCreateSystemDefaultDevice 是绑定层的安全函数（返回 Option 表示
        // 无 Metal 设备，如无窗口服务的会话）。
        let device =
            MTLCreateSystemDefaultDevice().ok_or_else(|| "无可用 Metal 设备".to_string())?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "newCommandQueue 失败".to_string())?;

        // CVMetalTextureCache（D1 零拷贝路径的入口）。
        let mut cache_out: *mut CVMetalTextureCache = std::ptr::null_mut();
        // SAFETY: cache_out 是本栈上的合法出参指针；其余参数 None 表示默认
        // allocator / 属性；device 引用有效。返回值非 Success 时 cache_out
        // 不会被写入。
        let rv: CVReturn = unsafe {
            CVMetalTextureCache::create(None, None, &device, None, NonNull::from(&mut cache_out))
        };
        if rv != kCVReturnSuccess {
            return Err(format!("CVMetalTextureCacheCreate 失败: CVReturn={rv}"));
        }
        // SAFETY: create 成功（rv == Success）时 cache_out 指向 +1 的新对象。
        let cache =
            unsafe { CFRetained::from_raw(NonNull::new(cache_out).ok_or("cache 出参为空")?) };

        // layer 配置：BGRA8 后备缓冲、物理像素 drawableSize、3 个 drawable、
        // framebufferOnly=true（drawable 只作渲染目标，性能更优）、
        // displaySyncEnabled（D2：false = Present(0) 等价，不等 vblank）。
        // 这些 setter 在绑定层是安全方法（纯属性赋值）。
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setDrawableSize(CGSize {
            width: size.0 as f64,
            height: size.1 as f64,
        });
        layer.setMaximumDrawableCount(3);
        layer.setFramebufferOnly(true);
        layer.setDisplaySyncEnabled(vsync);

        let pipelines = build_pipelines(&device)?;

        Ok(GpuContext {
            device,
            layer,
            queue,
            cache,
            pipelines,
            cursor_plane: None,
        })
    }

    /// 当前 drawable 尺寸（物理像素，`drawableSize` 的单位）。
    pub(crate) fn drawable_size(&self) -> (u32, u32) {
        let s = self.layer.drawableSize();
        (s.width.round() as u32, s.height.round() as u32)
    }

    /// 设置 drawable 尺寸（物理像素）。调用方保证非零。
    pub(crate) fn set_drawable_size(&self, size: (u32, u32)) {
        // 绑定层安全方法（纯属性赋值）。
        self.layer.setDrawableSize(CGSize {
            width: size.0 as f64,
            height: size.1 as f64,
        });
    }

    /// vsync 开关（`displaySyncEnabled`，D2）。
    pub(crate) fn set_display_sync(&self, on: bool) {
        // 绑定层安全方法。
        self.layer.setDisplaySyncEnabled(on);
    }

    /// 读回 layer 的 `displaySyncEnabled`（测试用真值断言）。
    pub(crate) fn display_sync_enabled(&self) -> bool {
        self.layer.displaySyncEnabled()
    }

    /// 取一个 drawable（可能阻塞：池空时等 CoreAnimation 回收）。
    ///
    /// 拆出来是为了让真机测试能把「drawable 获取」与「present 调用」
    /// 分开计时（见 `renderer.rs` 的 `present_with_vsync_off_under_2ms`）。
    pub(crate) fn next_drawable(&self) -> Option<MetalDrawable> {
        self.layer.nextDrawable()
    }

    /// 借出 layer 指针（NSView 路径挂视图用）。
    ///
    /// 借用语义：调用方不得释放 / 修改引用计数；layer 存活期由本上下文
    /// （即 `MetalRenderer`）保证。
    pub(crate) fn layer_ptr(&self) -> *mut c_void {
        Retained::as_ptr(&self.layer).cast_mut().cast::<c_void>()
    }

    /// 设置 / 更新光标形状（D7 形状缓存 + GPU 纹理上传）。
    ///
    /// 返回 `true` = 本次新上传了一次纹理；`false` = 形状缓存命中
    /// （同 id 且尺寸 / 热点一致），未触碰 GPU。调用方（渲染器）据此
    /// 维护上传计数。
    pub(crate) fn set_cursor_shape(&mut self, shape: &CursorShape) -> Result<bool, String> {
        if let Some(plane) = &self.cursor_plane {
            if plane.matches(shape) {
                return Ok(false); // 形状缓存命中：同 id 不重传。
            }
        }
        let (w, h) = (shape.width(), shape.height());
        // SAFETY: descriptor 构造只创建描述对象；unsafe 标记来自绑定层对
        // width/height 不做边界检查的保守约定——此处是 CursorShape::validate
        // 校验过的非零 u32 光标位图尺寸。
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                w as usize,
                h as usize,
                false,
            )
        };
        // setter 是绑定层安全方法（纯属性赋值）。
        desc.setUsage(MTLTextureUsage::ShaderRead);
        // 统一内存（Apple Silicon）用 Shared；离散 GPU（Intel 机型）用
        // Managed，replaceRegion 负责同步 CPU → GPU。
        desc.setStorageMode(if self.device.hasUnifiedMemory() {
            MTLStorageMode::Shared
        } else {
            MTLStorageMode::Managed
        });
        // newTextureWithDescriptor 是绑定层安全方法（返回 None = 创建失败）。
        let texture = self
            .device
            .newTextureWithDescriptor(&desc)
            .ok_or_else(|| "光标纹理创建失败（设备异常）".to_string())?;

        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: w as usize,
                height: h as usize,
                depth: 1,
            },
        };
        // SAFETY: shape 已通过 validate（bgra.len() == w*h*4）；指针指向
        // 合法存活数据且在调用期间不变；bytesPerRow == w*4 与区域宽度一致。
        unsafe {
            texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::from(shape.bgra()).cast(),
                w as usize * 4,
            )
        };
        self.cursor_plane = Some(CursorPlane {
            id: shape.id(),
            texture,
            width: w,
            height: h,
            hotspot: shape.hotspot(),
        });
        Ok(true)
    }

    /// 当前缓存的形状 id（D7 形状缓存）；从未上传过形状则 `None`。
    pub(crate) fn cursor_shape_id(&self) -> Option<u64> {
        self.cursor_plane.as_ref().map(CursorPlane::id)
    }

    /// 借出当前光标形状平面（构造叠加绘制用）。
    pub(crate) fn cursor_plane(&self) -> Option<&CursorPlane> {
        self.cursor_plane.as_ref()
    }

    /// 把 `CVPixelBuffer`（裸指针，借用）零拷贝包装成采样纹理（D1）。
    ///
    /// - `declared` 是 `DecodedFrame` 声明的宽高，与缓冲实际值不一致时报错；
    /// - 返回编码用的纹理与必须活到 GPU 完成的引用集合；
    /// - 支持的格式：'BGRA'（32BGRA）、'420v'、'420f'，其余报错。
    pub(crate) fn wrap_buffer(
        &self,
        raw: *mut c_void,
        declared: (u32, u32),
    ) -> Result<(FrameSource, FrameKeepAlive), String> {
        if raw.is_null() {
            return Err("CVPixelBuffer 指针为空".to_string());
        }
        // SAFETY: 调用方（解码器契约）保证指针指向合法存活的 CVPixelBuffer；
        // retain 升级为 +1 强引用，使源在本次 present 的 GPU 工作完成前不会被
        // 回收（非空已在上面检查，CFRetained::retain 对非空指针返回 +1 引用）。
        let buffer: CFRetained<CVPixelBuffer> = {
            let nn: NonNull<CVPixelBuffer> =
                NonNull::new(raw.cast()).ok_or_else(|| "CVPixelBuffer 指针为空".to_string())?;
            // SAFETY: nn 非空且指向合法 CVPixelBuffer（调用方契约）。
            unsafe { CFRetained::retain(nn) }
        };

        // 以下 getter 均为绑定层安全函数（只读，引用由 CFRetained 保证存活）。
        let actual = (
            CVPixelBufferGetWidth(&buffer) as u32,
            CVPixelBufferGetHeight(&buffer) as u32,
        );
        if actual != declared {
            return Err(format!(
                "帧尺寸不一致: DecodedFrame 声明 {declared:?}, CVPixelBuffer 实际 {actual:?}"
            ));
        }
        let fmt = CVPixelBufferGetPixelFormatType(&buffer);

        let mut keep = FrameKeepAlive {
            _buffer: buffer,
            _cv_textures: Vec::new(),
        };

        match fmt {
            FMT_BGRA => {
                let (tex, cv) = self.cache_texture(
                    &keep._buffer,
                    MTLPixelFormat::BGRA8Unorm,
                    actual.0,
                    actual.1,
                    0,
                )?;
                keep._cv_textures.push(cv);
                Ok((FrameSource::Bgra { texture: tex }, keep))
            }
            FMT_420V | FMT_420F => {
                // 双平面校验：'420x' 必须是 2 平面 planar 缓冲。
                let planes = CVPixelBufferGetPlaneCount(&keep._buffer);
                if planes < 2 {
                    return Err(format!(
                        "NV12 缓冲平面数异常: {} (期望 >= 2), fourcc='{}'",
                        planes,
                        fourcc(fmt)
                    ));
                }
                // luma = plane 0（原尺寸，R8Unorm）；chroma = plane 1（半宽半高，
                // RG8Unorm）—— 平面尺寸按 CoreVideo 上报取，不用 w/2 推算
                // （奇数分辨率时两者不同）。
                let cw = CVPixelBufferGetWidthOfPlane(&keep._buffer, 1) as u32;
                let ch = CVPixelBufferGetHeightOfPlane(&keep._buffer, 1) as u32;
                let (luma_tex, luma_cv) = self.cache_texture(
                    &keep._buffer,
                    MTLPixelFormat::R8Unorm,
                    actual.0,
                    actual.1,
                    0,
                )?;
                let (chroma_tex, chroma_cv) =
                    self.cache_texture(&keep._buffer, MTLPixelFormat::RG8Unorm, cw, ch, 1)?;
                keep._cv_textures.push(luma_cv);
                keep._cv_textures.push(chroma_cv);
                Ok((
                    FrameSource::Nv12 {
                        luma: luma_tex,
                        chroma: chroma_tex,
                        full_range: fmt == FMT_420F,
                    },
                    keep,
                ))
            }
            other => Err(format!(
                "不支持的 CVPixelBuffer 格式 fourcc='{}' (0x{other:08x}); \
                 支持 'BGRA' / '420v' / '420f'",
                fourcc(other)
            )),
        }
    }

    /// 单平面包装：CVMetalTextureCacheCreateTextureFromImage + 取 MTLTexture。
    fn cache_texture(
        &self,
        buffer: &CVPixelBuffer,
        pixel_format: MTLPixelFormat,
        width: u32,
        height: u32,
        plane: usize,
    ) -> Result<(MetalTexture, CFRetained<CVMetalTexture>), String> {
        let mut tex_out: *mut CVMetalTexture = std::ptr::null_mut();
        // SAFETY: tex_out 是本栈上的合法出参指针；cache/buffer 引用有效；
        // plane/尺寸与缓冲布局一致（调用方按 CoreVideo 上报的平面尺寸传入）；
        // 像素格式与平面格式匹配（R8=luma / RG8=chroma / BGRA=整帧）。
        // 返回值非 Success 时 tex_out 不会被写入。
        let rv = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                buffer,
                None,
                pixel_format,
                width as usize,
                height as usize,
                plane,
                NonNull::from(&mut tex_out),
            )
        };
        if rv != kCVReturnSuccess {
            return Err(format!(
                "CVMetalTextureCacheCreateTextureFromImage 失败: CVReturn={rv} \
                 (plane={plane}, {width}x{height}, 非法或非 IOSurface/Metal 兼容缓冲?)"
            ));
        }
        // SAFETY: 成功时 tex_out 指向 +1 的新 CVMetalTexture。
        let cv =
            unsafe { CFRetained::from_raw(NonNull::new(tex_out).ok_or("texture 出参为空")?) };
        // CVMetalTextureGetTexture 是绑定层安全函数（retain 后返回）。
        let texture = CVMetalTextureGetTexture(&cv)
            .ok_or_else(|| "CVMetalTextureGetTexture 返回空".to_string())?;
        Ok((texture, cv))
    }

    /// 编码一次全屏绘制并 present（D8：commit 返回后才回到调用方打点）。
    ///
    /// `overlay` 是 D7 光标本地叠加（`None` = 不叠加：合成关 / 光标隐藏 /
    /// 完全在目标外）。
    ///
    /// `source` / `keep` / `overlay` 被移动进命令缓冲的完成回调：GPU 用完
    /// 源纹理后才释放 `CVMetalTexture` 包装、`CVPixelBuffer` 的 retain 与
    /// 光标纹理引用。
    ///
    /// 失败（nextDrawable 为空等 device-lost 类）以 `Err(String)` 上抛，
    /// 由 `renderer.rs` 映射为可恢复错误（D6）。
    pub(crate) fn draw_and_present(
        &self,
        source: FrameSource,
        keep: FrameKeepAlive,
        overlay: Option<CursorDraw>,
    ) -> Result<(), String> {
        // 离屏 layer 也能取 drawable；为空通常是层不可见 / 尺寸为 0 /
        // 设备异常 —— 对应 Windows 侧「窗口最小化时 Present 失败」的可恢复路径。
        let drawable = self.next_drawable().ok_or_else(|| {
            "nextDrawable 返回空（层不可见 / drawableSize 为 0 / 设备异常）".to_string()
        })?;
        self.draw_present_commit(&drawable, source, keep, overlay)
    }

    /// present 调用本体（drawable 已由调用方取得）：编码一次全屏绘制 +
    /// 可选光标叠加（D7）、`presentDrawable`、`commit`（D8：commit 返回后
    /// 才回到调用方打点）。
    ///
    /// 拆出 drawable 获取，让真机测试可以只对「present 调用」计时
    /// （等价 Windows `Present(0)` 的调用成本），不被离屏层的
    /// drawable 回收节奏污染（见测试文档）。
    ///
    /// `source` / `keep` / `overlay` 被移动进命令缓冲的完成回调：GPU 用完
    /// 源纹理后才释放（同 [`GpuContext::draw_and_present`]）。
    ///
    /// 失败（编码器拿不到等 device-lost 类）以 `Err(String)` 上抛，
    /// 由 `renderer.rs` 映射为可恢复错误（D6）。
    pub(crate) fn draw_present_commit(
        &self,
        drawable: &MetalDrawable,
        source: FrameSource,
        keep: FrameKeepAlive,
        overlay: Option<CursorDraw>,
    ) -> Result<(), String> {
        // 渲染通道：以 drawable 纹理为目标，清屏后画视频 quad + 光标叠加。
        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: 索引 0 在 colorAttachments 有效范围内（系统固定 4 个槽位）。
        let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(&*drawable.texture()));
        att.setLoadAction(MTLLoadAction::Clear);
        att.setStoreAction(MTLStoreAction::Store);
        att.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });

        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer 创建失败（设备异常）".to_string())?;
        let enc = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| "renderCommandEncoder 创建失败（设备异常）".to_string())?;

        self.encode_frame(&enc, &source, &overlay);
        // endEncoding 是绑定层安全方法。
        enc.endEncoding();

        // present 绑定在命令缓冲上：GPU 执行完编码内容后立刻上屏；
        // displaySyncEnabled=false（D2）时不等待 vblank（等价 Present(0)）。
        // CAMetalDrawable: MTLDrawable 协议继承，as_ref 做协议对象上转。
        let drawable_ref: &ProtocolObject<dyn MTLDrawable> = drawable.as_ref();
        cmd.presentDrawable(drawable_ref);

        // 完成回调：释放 keep（CVPixelBuffer retain + CVMetalTexture 包装）、
        // source（MTLTexture 强引用）与 overlay（光标纹理引用）——
        // CoreVideo 要求包装活到 GPU 用完。
        // block 签名要求 Fn（不能 move-out 捕获值），经 RefCell<Option> 释放。
        let release = std::cell::RefCell::new(Some((source, keep, overlay)));
        let block: PresentDoneBlock =
            RcBlock::new(move |_buf: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                // 完成回调恰好触发一次；take 后即释放全部引用。
                drop(release.borrow_mut().take());
            });
        // SAFETY: block 是刚创建的合法 RcBlock，指针在调用期间有效；Metal 按
        // ObjC 约定拷贝 handler block，随后 RcBlock 的释放不影响回调。
        unsafe { cmd.addCompletedHandler(NonNull::from(&*block).as_ptr()) };

        // commit 立即返回（不等待 GPU）；present 返回后由 renderer.rs 打点（D8）。
        cmd.commit();
        Ok(())
    }

    /// 编码一帧的绘制序列：视频全屏 quad → 可选光标本地叠加（D7）。
    ///
    /// 光标矩形由 CPU 侧「当下」缓存位置算出（vertex bytes），不进视频
    /// 管线；叠加在视频之后、同一渲染通道内（blend: SourceAlpha /
    /// OneMinusSourceAlpha，管线在 [`build_pipelines`] 里配）。
    fn encode_frame(
        &self,
        enc: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        source: &FrameSource,
        overlay: &Option<CursorDraw>,
    ) {
        match source {
            FrameSource::Bgra { .. } => enc.setRenderPipelineState(&self.pipelines.bgra),
            FrameSource::Nv12 {
                full_range: false, ..
            } => enc.setRenderPipelineState(&self.pipelines.nv12_video),
            FrameSource::Nv12 {
                full_range: true, ..
            } => enc.setRenderPipelineState(&self.pipelines.nv12_full),
        }
        match source {
            FrameSource::Bgra { texture } => {
                // SAFETY: 索引 0 在 fragment 纹理表有效范围内（管线只用 1 个槽位）。
                unsafe { enc.setFragmentTexture_atIndex(Some(&**texture), 0) };
            }
            FrameSource::Nv12 { luma, chroma, .. } => {
                // SAFETY: 索引 0 在 fragment 纹理表有效范围内（管线只用 2 个槽位）。
                unsafe { enc.setFragmentTexture_atIndex(Some(&**luma), 0) };
                // SAFETY: 索引 1 在 fragment 纹理表有效范围内（管线只用 2 个槽位）。
                unsafe { enc.setFragmentTexture_atIndex(Some(&**chroma), 1) };
            }
        }
        // 全屏 quad 由 vertex_id 在着色器内展开（无顶点缓冲）。
        // SAFETY: 顶点数 4 与 TriangleStrip 的 quad 拓扑一致；着色器只消费
        // vertex_id，没有绑定任何顶点缓冲，不存在越界访问。
        unsafe {
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4)
        };

        // D7：光标本地叠加（视频 quad 之后）。
        if let Some(cur) = overlay {
            enc.setRenderPipelineState(&self.pipelines.cursor);
            // SAFETY: 索引 0 在 fragment 纹理表有效范围内（光标管线只用 1 个槽位）。
            unsafe { enc.setFragmentTexture_atIndex(Some(&*cur.texture), 0) };
            // SAFETY: bytes 指向栈上合法的 CursorUniforms（#[repr(C)]，
            // 32 字节，与 MSL 侧 float4×2 布局一致），length 与结构尺寸一致；
            // 索引 0 是 vs_cursor 声明的唯一 vertex buffer（[[buffer(0)]]）。
            unsafe {
                enc.setVertexBytes_length_atIndex(
                    NonNull::from(&cur.uniforms).cast(),
                    std::mem::size_of::<CursorUniforms>(),
                    0,
                )
            };
            // SAFETY: 顶点数 4 与 TriangleStrip 的 quad 拓扑一致；vs_cursor 只
            // 消费 vertex_id 与 buffer(0)，无顶点缓冲越界；光标矩形可能与视频
            // quad 部分重叠，混合在 tile 内存里完成（无需中间纹理）。
            unsafe {
                enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4)
            };
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 渲染管线：运行时编译 MSL，三个 fragment 组合
// ─────────────────────────────────────────────────────────────────────────────

/// NSError → 可读字符串（管线创建失败的定位信息）。
fn nserror_message(err: &NSError) -> String {
    // localizedDescription 在绑定层是安全方法（纯属性 getter）；
    // NSString 实现了 Display。
    err.localizedDescription().to_string()
}

fn build_pipelines(device: &ProtocolObject<dyn MTLDevice>) -> Result<Pipelines, String> {
    // 运行时编译 shader 源码（构造期一次，几 ms 量级，不进呈现路径）。
    let source = NSString::from_str(shader::MSL_SOURCE);
    // newLibraryWithSource / newFunctionWithName 是绑定层安全方法。
    let library: Retained<ProtocolObject<dyn MTLLibrary>> = device
        .newLibraryWithSource_options_error(&source, None)
        .map_err(|e| format!("shader 编译失败: {}", nserror_message(&e)))?;
    let function = |name: &str| -> Result<Retained<ProtocolObject<dyn MTLFunction>>, String> {
        library
            .newFunctionWithName(&NSString::from_str(name))
            .ok_or_else(|| format!("着色器函数不存在: {name}"))
    };
    let vs = function("vs_quad")?;
    let fs_bgra = function("fs_bgra")?;
    let fs_nv12_video = function("fs_nv12_video")?;
    let fs_nv12_full = function("fs_nv12_full")?;
    let vs_cursor = function("vs_cursor")?;
    let fs_cursor = function("fs_cursor")?;

    let make = |vertex: &Retained<ProtocolObject<dyn MTLFunction>>,
                fragment: &Retained<ProtocolObject<dyn MTLFunction>>,
                blending: bool|
     -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
        // 预先借出协议对象引用（setXxxFunction 要 Option<&ProtocolObject<..>>）。
        let vs_ref: &ProtocolObject<dyn MTLFunction> = vertex;
        let fs_ref: &ProtocolObject<dyn MTLFunction> = fragment;
        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(Some(vs_ref));
        desc.setFragmentFunction(Some(fs_ref));
        // SAFETY: 索引 0 在 colorAttachments 有效范围内（系统固定 4 个槽位）。
        let color0 = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
        // 与 layer.pixelFormat 一致（BGRA8Unorm）。
        color0.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        if blending {
            // D7 光标叠加：直通 alpha 的 SourceOver 混合。
            // rgb: src.rgb * src.a + dst.rgb * (1 - src.a)；
            // alpha 通道用 One / OneMinusSourceAlpha（目标已不透明时保持
            // 不透明——alpha 不做平方衰减）。
            color0.setBlendingEnabled(true);
            color0.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
            color0.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            color0.setSourceAlphaBlendFactor(MTLBlendFactor::One);
            color0.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        }
        device
            .newRenderPipelineStateWithDescriptor_error(&desc)
            .map_err(|e| format!("渲染管线创建失败: {}", nserror_message(&e)))
    };

    Ok(Pipelines {
        bgra: make(&vs, &fs_bgra, false)?,
        nv12_video: make(&vs, &fs_nv12_video, false)?,
        nv12_full: make(&vs, &fs_nv12_full, false)?,
        // 光标管线：独立 vs（矩形经 vertex bytes 传入）+ alpha 混合。
        cursor: make(&vs_cursor, &fs_cursor, true)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试支撑：构造真实 CVPixelBuffer（单测用，渲染路径不碰 CPU 像素）
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    };
    use objc2_core_video::CVPixelBufferCreate;

    /// 构造「Metal 兼容」的像素缓冲属性字典：
    /// `kCVPixelBufferIOSurfacePropertiesKey = {}` +
    /// `kCVPixelBufferMetalCompatibilityKey = true`。
    ///
    /// 无属性创建的缓冲不是 Metal 兼容的 IOSurface 后备，
    /// `CVMetalTextureCacheCreateTextureFromImage` 会返回 -6660。
    /// 构造方式与 rc-encode-vt 的 `require_hardware_dictionary` 同一套。
    fn metal_compatible_attributes() -> CFRetained<CFDictionary> {
        // SAFETY: 只对 extern static 做一次读取（SDK 常量，框架加载即初始化、
        // 永不释放，读取无数据竞争）。
        let io_key = unsafe { objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey };
        // SAFETY: 同上。
        let metal_key = unsafe { objc2_core_video::kCVPixelBufferMetalCompatibilityKey };

        // 空字典 = 默认 IOSurface 属性。
        let empty_keys: [*const c_void; 0] = [];
        let empty_values: [*const c_void; 0] = [];
        // SAFETY: 空数组、num_values=0、回调为 SDK 常量。
        let io_props = unsafe {
            CFDictionary::new(
                None,
                empty_keys.as_ptr() as *mut *const c_void,
                empty_values.as_ptr() as *mut *const c_void,
                0,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .expect("创建 IOSurface 属性字典失败");

        // CFBoolean::new 返回 SDK 单例的 &'static 引用，可直接取指针。
        let metal_true = CFBoolean::new(true);
        let keys: [*const c_void; 2] = [
            io_key as *const _ as *const c_void,
            metal_key as *const _ as *const c_void,
        ];
        let values: [*const c_void; 2] = [
            CFRetained::as_ptr(&io_props)
                .as_ptr()
                .cast_const()
                .cast::<c_void>(),
            metal_true as *const CFBoolean as *const c_void,
        ];
        // SAFETY: keys/values 指向栈上数组的 2 个合法指针，在调用期间存活；
        // key 为 SDK 常量 CFString、value 为不可变单例 CFBoolean / 字典，
        // CF 类型回调（retain/release）对三者均正确。
        let attrs = unsafe {
            CFDictionary::new(
                None,
                keys.as_ptr() as *mut *const c_void,
                values.as_ptr() as *mut *const c_void,
                2,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        };
        attrs.expect("创建 Metal 兼容属性字典失败")
    }

    /// 创建 w×h 的 'BGRA' CVPixelBuffer（内容未初始化；离屏冒烟不校验像素）。
    pub(crate) fn pixel_buffer_bgra(w: usize, h: usize) -> CFRetained<CVPixelBuffer> {
        let attrs = metal_compatible_attributes();
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: out 是本栈上的合法出参指针；attrs 在调用期间存活。
        let rv = unsafe {
            CVPixelBufferCreate(
                None,
                w,
                h,
                kCVPixelFormatType_32BGRA,
                Some(&attrs),
                NonNull::from(&mut out),
            )
        };
        assert_eq!(
            rv, kCVReturnSuccess,
            "CVPixelBufferCreate(BGRA) 失败: CVReturn={rv}"
        );
        // SAFETY: rv == Success 时 out 指向 +1 的新缓冲。
        unsafe { CFRetained::from_raw(NonNull::new(out).expect("成功时非空")) }
    }

    /// 创建 w×h 的 'BGRA' CVPixelBuffer 并填充单色 `bgra`（D7 回读测试用：
    /// 视频帧内容已知，才能断言光标叠加后的混合结果）。
    ///
    /// CPU 写入只发生在测试支撑里（生产呈现路径不 LockBaseAddress，D1）。
    pub(crate) fn pixel_buffer_bgra_filled(
        w: usize,
        h: usize,
        bgra: [u8; 4],
    ) -> CFRetained<CVPixelBuffer> {
        let buffer = pixel_buffer_bgra(w, h);
        use objc2_core_video::{
            CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress,
            CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        };
        // SAFETY: buffer 是合法存活的 CVPixelBuffer；空 flags 是唯一合法值；
        // lock/unlock 配对，期间做一次 CPU 写入。
        let lock =
            unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        assert_eq!(
            lock, kCVReturnSuccess,
            "CVPixelBufferLockBaseAddress 失败: CVReturn={lock}"
        );
        // GetBaseAddress / GetBytesPerRow 是绑定层安全函数（只读）。
        let base = CVPixelBufferGetBaseAddress(&buffer);
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        assert!(!base.is_null(), "单平面缓冲的 base address 不应为空");
        for row in 0..h {
            // SAFETY: base 指向 stride*h 的可写内存；row*stride+offset 在
            // 缓冲范围内（CoreVideo 保证 stride >= w*4）。
            let row_ptr: *mut u8 = unsafe { base.byte_add(row * stride) }.cast();
            for col in 0..w {
                // SAFETY: 同上，col*4+4 <= w*4 <= stride。
                unsafe {
                    std::ptr::copy_nonoverlapping(bgra.as_ptr(), row_ptr.byte_add(col * 4), 4);
                }
            }
        }
        // SAFETY: 与上面的 lock 配对（只解一次锁）。
        let unlock =
            unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::empty()) };
        assert_eq!(
            unlock, kCVReturnSuccess,
            "CVPixelBufferUnlockBaseAddress 失败: CVReturn={unlock}"
        );
        buffer
    }

    /// 把一帧渲染到**离屏 BGRA 纹理**并回读像素（D7 验证专用）。
    ///
    /// 与生产路径走**同一套编码序列**（`GpuContext::encode_frame`：视频
    /// quad + 可选光标叠加），只是渲染目标是可回读纹理而非 drawable——
    /// drawable 是 framebufferOnly 的，回读必须换目标。CPU 回读只发生在
    /// 测试支撑里（生产路径禁止，D1）。
    pub(crate) fn render_to_bgra_readback(
        gpu: &GpuContext,
        buf: &CFRetained<CVPixelBuffer>,
        overlay: Option<CursorDraw>,
    ) -> Result<Vec<u8>, String> {
        let w = CVPixelBufferGetWidth(buf);
        let h = CVPixelBufferGetHeight(buf);
        // SAFETY: 同 upload_cursor 的 descriptor 论证（w/h 来自合法缓冲）。
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                w,
                h,
                false,
            )
        };
        // setter 是绑定层安全方法。
        desc.setUsage(MTLTextureUsage::RenderTarget.union(MTLTextureUsage::ShaderRead));
        desc.setStorageMode(if gpu.device.hasUnifiedMemory() {
            MTLStorageMode::Shared
        } else {
            MTLStorageMode::Managed
        });
        // newTextureWithDescriptor 是绑定层安全方法。
        let target = gpu
            .device
            .newTextureWithDescriptor(&desc)
            .ok_or_else(|| "回读纹理创建失败（设备异常）".to_string())?;

        // 与 draw_present_commit 相同的通道设置，只是目标换成回读纹理。
        let (source, keep) = gpu.wrap_buffer(as_opaque_ptr(buf), (w as u32, h as u32))?;
        let pass = MTLRenderPassDescriptor::renderPassDescriptor();
        // SAFETY: 索引 0 在 colorAttachments 有效范围内（系统固定 4 个槽位）。
        let att = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        att.setTexture(Some(&*target));
        att.setLoadAction(MTLLoadAction::Clear);
        att.setStoreAction(MTLStoreAction::Store);
        att.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });
        let cmd = gpu
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer 创建失败".to_string())?;
        let enc = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| "renderCommandEncoder 创建失败".to_string())?;
        gpu.encode_frame(&enc, &source, &overlay);
        // endEncoding 是绑定层安全方法。
        enc.endEncoding();
        // commit / waitUntilCompleted 是绑定层安全方法。
        cmd.commit();
        cmd.waitUntilCompleted();
        // 完成后 source / keep 才能释放（CoreVideo 要求包装活到 GPU 用完）。
        drop(source);
        drop(keep);

        let mut out = vec![0u8; w * h * 4];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: w,
                height: h,
                depth: 1,
            },
        };
        // SAFETY: out.len() == w*h*4 与区域尺寸一致；bytesPerRow == w*4；
        // 指针在调用期间有效。
        unsafe {
            target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::from(out.as_mut_slice()).cast(),
                w * 4,
                region,
                0,
            )
        };
        Ok(out)
    }

    /// 创建 w×h 的 '420v'（NV12 video range）双平面 CVPixelBuffer。
    pub(crate) fn pixel_buffer_nv12(w: usize, h: usize) -> CFRetained<CVPixelBuffer> {
        let attrs = metal_compatible_attributes();
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: 同上。
        let rv = unsafe {
            CVPixelBufferCreate(
                None,
                w,
                h,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(&attrs),
                NonNull::from(&mut out),
            )
        };
        assert_eq!(
            rv, kCVReturnSuccess,
            "CVPixelBufferCreate(420v) 失败: CVReturn={rv}"
        );
        // SAFETY: 同上。
        unsafe { CFRetained::from_raw(NonNull::new(out).expect("成功时非空")) }
    }

    /// CFRetained 的裸指针视图（塞进 FrameTexture::CVPixelBuffer 用）。
    pub(crate) fn as_opaque_ptr(buf: &CFRetained<CVPixelBuffer>) -> *mut c_void {
        CFRetained::as_ptr(buf).as_ptr().cast()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 真实窗口基准（`window-bench` feature）：NSWindow/NSView + 帧源的安全封装
// ─────────────────────────────────────────────────────────────────────────────

/// 真实窗口基准的安全封装（`window-bench` feature）。
///
/// 供 `tests/window_present.rs`（harness = false，`fn main` 在主线程——
/// NSWindow 是 MainThreadOnly 类型，libtest 的测试体永远不在主线程）
/// 使用。unsafe 全部留在本模块（ffi 边界），对外只暴露安全 API。
///
/// # 用途
/// 挂真实窗口后测**完整 present 路径**（含 `nextDrawable`）的调用成本
/// 与 60fps×60s soak——离屏 layer 的 drawable 回收按合成器节奏（本机
/// 120Hz XDR ≈ 8.3ms/枚），不挂窗口测不出真实路径。
#[cfg(all(target_os = "macos", feature = "window-bench"))]
pub mod window_bench {
    use super::*;
    use objc2::MainThreadMarker;
    use objc2::MainThreadOnly;
    use objc2_app_kit::{NSBackingStoreType, NSView, NSWindow, NSWindowStyleMask};
    use objc2_core_foundation::{CGPoint, CGRect};

    /// 测试窗口 + 挂在其 `contentView` 上的 Metal layer。
    ///
    /// 注意：`Retained<NSWindow>` 的 Drop（dealloc）按 AppKit 约定应在
    /// 主线程——基准结束时应调用 [`BenchWindow::leak`] 故意泄漏（进程
    /// 随即退出，由 OS 回收全部资源），不要让它自然 Drop。
    pub struct BenchWindow {
        _window: Retained<NSWindow>,
        _view: Retained<NSView>,
        _layer: Retained<CAMetalLayer>,
    }

    impl BenchWindow {
        /// 借出 layer 裸指针（借用语义；交给
        /// [`MetalRenderer::from_raw_layer`](crate::MetalRenderer::from_raw_layer)
        /// retain 成强引用）。
        pub fn layer_ptr(&self) -> *mut c_void {
            Retained::as_ptr(&self._layer).cast_mut().cast::<c_void>()
        }

        /// 故意泄漏整个窗口（见类型文档：dealloc 留给进程退出）。
        pub fn leak(self) {
            std::mem::forget(self);
        }
    }

    /// 创建铺满主屏的无边框窗口（隐私幕布，`tests/curtain.rs` 用）。
    ///
    /// 与 [`window_with_metal_layer`] 的差别只有 contentRect：取
    /// `NSScreen.mainScreen().frame()`（逻辑点；Retina 物理像素由窗口
    /// 服务器按 contentsScale 处理，幕布只关心覆盖与逐帧变化）。
    /// 返回窗口句柄 + 主屏尺寸（点）——渲染器 `cfg.size` 用它，
    /// layer 的 drawableSize 由 `GpuContext::init` 按 cfg 设置。
    pub fn fullscreen_window_with_metal_layer(
        mtm: MainThreadMarker,
    ) -> Result<(BenchWindow, (u32, u32)), String> {
        use objc2_app_kit::NSScreen;
        let screen = NSScreen::mainScreen(mtm).ok_or_else(|| "mainScreen 为空".to_string())?;
        // frame 是安全属性 getter，返回值类型（CGRect）拷贝走。
        let rect = screen.frame();
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        let view: Retained<NSView> = window
            .contentView()
            .ok_or_else(|| "window.contentView 为空".to_string())?;
        let layer = CAMetalLayer::layer();
        // 与 window_with_metal_layer 同构：wantsLayer + setLayer 安全方法。
        view.setWantsLayer(true);
        view.setLayer(Some(layer.as_ref()));
        window.orderFrontRegardless();
        let size = (rect.size.width as u32, rect.size.height as u32);
        Ok((
            BenchWindow {
                _window: window,
                _view: view,
                _layer: layer,
            },
            size,
        ))
    }

    /// 创建真实（无边框）窗口，把新建的 `CAMetalLayer` 挂到其
    /// `contentView` 上并立即显示（`orderFrontRegardless`，不要求进程
    /// 激活）。返回窗口句柄 + layer 裸指针。
    ///
    /// 需要窗口服务器（GUI 会话）。`mtm` 必须来自主线程（NSWindow 是
    /// MainThreadOnly 类型——AppKit 契约，绑定层用 MainThreadMarker 编码）。
    pub fn window_with_metal_layer(
        mtm: MainThreadMarker,
        size: (u32, u32),
    ) -> Result<BenchWindow, String> {
        let content_rect = CGRect::new(
            CGPoint::new(64.0, 64.0),
            CGSize::new(size.0 as f64, size.1 as f64),
        );
        // SAFETY: 标准 NSWindow 初始化：对 alloc 出的实例做首次 init，
        // 参数全是值类型（无别名 / 所有权陷阱），返回 +1 强引用；
        // Borderless + Buffered + 不延迟创建是窗口基准的常规组合。
        // mtm 保证本调用发生在主线程（AppKit 的 MainThreadOnly 契约）。
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                content_rect,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        let view: Retained<NSView> = window
            .contentView()
            .ok_or_else(|| "window.contentView 为空".to_string())?;
        let layer = CAMetalLayer::layer();
        // wantsLayer + setLayer 是绑定层安全方法（属性赋值；setLayer 内部
        // retain，view 持有 layer）。CAMetalLayer isa CALayer，as_ref 上转。
        view.setWantsLayer(true);
        view.setLayer(Some(layer.as_ref()));
        // orderFrontRegardless 是绑定层安全方法：不等激活直接显示。
        window.orderFrontRegardless();
        Ok(BenchWindow {
            _window: window,
            _view: view,
            _layer: layer,
        })
    }

    /// 基准用的 NV12（'420v'）帧源：Metal 兼容 CVPixelBuffer 的持有者。
    ///
    /// VideoToolbox / SCK 的真实输出格式；`ptr()` 借出裸指针塞进
    /// [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)
    /// （present 期间由渲染器 +1 retain；本持有者必须活过最后一次 present）。
    pub struct BenchFrameSource {
        _buf: CFRetained<CVPixelBuffer>,
        width: u32,
        height: u32,
        /// UV 平面是否已初始化（IOSurface 内容不确定，不能靠读值判定）。
        uv_init: std::cell::Cell<bool>,
    }

    impl BenchFrameSource {
        /// 创建 w×h 的 '420v' 双平面缓冲（内容未初始化——吞吐基准不校验像素）。
        pub fn nv12(w: u32, h: u32) -> Result<Self, String> {
            use objc2_core_foundation::{
                kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean,
                CFDictionary,
            };
            use objc2_core_video::CVPixelBufferCreate;

            // Metal 兼容属性（IOSurface 后备；无属性创建的缓冲
            // CVMetalTextureCacheCreateTextureFromImage 会返回 -6660）。
            // SAFETY: 只对 extern static 做一次读取（SDK 常量）。
            let io_key = unsafe { objc2_core_video::kCVPixelBufferIOSurfacePropertiesKey };
            // SAFETY: 同上。
            let metal_key = unsafe { objc2_core_video::kCVPixelBufferMetalCompatibilityKey };
            let empty_keys: [*const c_void; 0] = [];
            let empty_values: [*const c_void; 0] = [];
            // SAFETY: 空数组、num_values=0、回调为 SDK 常量。
            let io_props = unsafe {
                CFDictionary::new(
                    None,
                    empty_keys.as_ptr() as *mut *const c_void,
                    empty_values.as_ptr() as *mut *const c_void,
                    0,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            }
            .ok_or_else(|| "创建 IOSurface 属性字典失败".to_string())?;
            let metal_true = CFBoolean::new(true);
            let keys: [*const c_void; 2] = [
                io_key as *const _ as *const c_void,
                metal_key as *const _ as *const c_void,
            ];
            let values: [*const c_void; 2] = [
                CFRetained::as_ptr(&io_props)
                    .as_ptr()
                    .cast_const()
                    .cast::<c_void>(),
                metal_true as *const CFBoolean as *const c_void,
            ];
            // SAFETY: keys/values 指向栈上数组的 2 个合法指针，调用期间存活；
            // key 为 SDK 常量、value 为不可变单例 / 字典，CF 回调正确。
            let attrs = unsafe {
                CFDictionary::new(
                    None,
                    keys.as_ptr() as *mut *const c_void,
                    values.as_ptr() as *mut *const c_void,
                    2,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            }
            .ok_or_else(|| "创建 Metal 兼容属性字典失败".to_string())?;

            let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
            // SAFETY: out 是本栈上的合法出参指针；attrs 在调用期间存活。
            let rv = unsafe {
                CVPixelBufferCreate(
                    None,
                    w as usize,
                    h as usize,
                    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    Some(&attrs),
                    NonNull::from(&mut out),
                )
            };
            if rv != kCVReturnSuccess {
                return Err(format!("CVPixelBufferCreate(420v) 失败: CVReturn={rv}"));
            }
            // SAFETY: rv == Success 时 out 指向 +1 的新缓冲。
            let buf = unsafe {
                CFRetained::from_raw(NonNull::new(out).ok_or_else(|| "出参为空".to_string())?)
            };
            Ok(BenchFrameSource {
                _buf: buf,
                width: w,
                height: h,
                uv_init: std::cell::Cell::new(false),
            })
        }

        /// 借出缓冲裸指针（塞进 `FrameTexture::CVPixelBuffer`；借用语义）。
        pub fn ptr(&self) -> *mut c_void {
            CFRetained::as_ptr(&self._buf).as_ptr().cast()
        }

        /// 帧尺寸 `(width, height)`。
        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        /// 幕布测试图案：纯黑底 + 移动灰块 + 帧计数闪烁块（低熵，Y 平面）。
        ///
        /// M1 Gate 真机终验的内容源（`tests/curtain.rs`）：每帧都有真实
        /// 像素变化（SCK 依赖 damage 才回调），但熵极低 → 小 NAL，编码段
        /// 测的是管线而不是内容码率。首帧顺带把 UV 平面初始化为中性 128
        /// （`CVPixelBufferCreate` 不清零）。
        pub fn paint_test_pattern(&self, frame: u64) -> Result<(), String> {
            self.with_locked_planes(|y_base, y_stride, w, h, uv_base, uv_stride, first| {
                // Y 平面：黑底（video range 16）。整帧重涂（≤2MB memset），
                // 保证上一帧的灰块痕迹不留。
                for row in 0..h {
                    unsafe { std::ptr::write_bytes(y_base.add(row * y_stride), 16, w) };
                }
                // 移动灰块 64×64（Y=180），轨迹互质不重复。
                let bw = 64usize;
                let bh = 64usize;
                let x = ((frame as usize * 13) % (w - bw)) as usize;
                let y = ((frame as usize * 7) % (h - bh)) as usize;
                for row in 0..bh {
                    unsafe { std::ptr::write_bytes(y_base.add((y + row) * y_stride + x), 180, bw) };
                }
                // 帧计数块 16×16：灰度随帧号变（保证每帧 damage，与灰块位置无关）。
                let cval: u8 = 60 + (frame % 128) as u8;
                for row in 0..16usize {
                    unsafe {
                        std::ptr::write_bytes(y_base.add((row + 8) * y_stride + 8), cval, 16)
                    };
                }
                if first {
                    // UV 平面（w/2 × h/2 交错）：中性色度 128，一次性。
                    for row in 0..(h / 2) {
                        unsafe { std::ptr::write_bytes(uv_base.add(row * uv_stride), 128, w) };
                    }
                }
            })
        }

        /// 纯黑静止（静态画面 gate：零像素变化）。
        pub fn paint_black(&self) -> Result<(), String> {
            self.with_locked_planes(|y_base, y_stride, w, h, uv_base, uv_stride, first| {
                for row in 0..h {
                    unsafe { std::ptr::write_bytes(y_base.add(row * y_stride), 16, w) };
                }
                if first {
                    for row in 0..(h / 2) {
                        unsafe { std::ptr::write_bytes(uv_base.add(row * uv_stride), 128, w) };
                    }
                }
            })
        }

        /// 锁双平面 → 回调（基址/步长/尺寸）→ 解锁。回调内不做除指针写外
        /// 的任何 FFI；`first` 表示本缓冲首次写入（UV 初始化判定）。
        fn with_locked_planes(
            &self,
            f: impl FnOnce(*mut u8, usize, usize, usize, *mut u8, usize, bool),
        ) -> Result<(), String> {
            use objc2_core_video::{
                CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
                CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane,
                CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
                CVPixelBufferUnlockBaseAddress,
            };
            // SAFETY: buf 是本源持有的合法 CVPixelBuffer；lock/unlock 配对，
            // flags=empty（读写锁）；plane 0/1 是 '420v' 双平面布局的合法下标。
            let buf = &self._buf;
            let rv = unsafe { CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::empty()) };
            if rv != kCVReturnSuccess {
                return Err(format!("CVPixelBufferLockBaseAddress 失败: CVReturn={rv}"));
            }
            // 这些 getter 在绑定层是安全函数（同步只读）。
            let y_base = CVPixelBufferGetBaseAddressOfPlane(buf, 0) as *mut u8;
            let uv_base = CVPixelBufferGetBaseAddressOfPlane(buf, 1) as *mut u8;
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(buf, 0);
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(buf, 1);
            let w = CVPixelBufferGetWidthOfPlane(buf, 0);
            let h = CVPixelBufferGetHeightOfPlane(buf, 0);
            if y_base.is_null() || uv_base.is_null() {
                // SAFETY: 与上面的 lock 配对。
                unsafe { CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::empty()) };
                return Err("CVPixelBuffer 平面基址为空".to_string());
            }
            // 首帧判定用显式标志（IOSurface 初始内容不确定，不能读值猜）。
            let first = !self.uv_init.get();
            f(y_base, y_stride, w, h, uv_base, uv_stride, first);
            if first {
                self.uv_init.set(true);
            }
            // SAFETY: 与 lock 配对，写完再解锁。
            unsafe { CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::empty()) };
            Ok(())
        }
    }
}
