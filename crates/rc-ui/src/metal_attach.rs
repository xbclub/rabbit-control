//! macOS 画面桥接（路径 A）：`NSView` ↔ `CAMetalLayer` 挂载、尺寸同步与摘除。
//!
//! **本 crate 唯一允许 `unsafe` 的模块**（T21 U7 / API 契约 §5），
//! 参照 `crates/rc-render-macos/src/ffi/mod.rs` 先例：每个 `unsafe` 块必须带
//! `// SAFETY:` 注释。
//!
//! # 线程纪律（T21 坑 2）
//! [`attach_layer`] / [`sync_layer_frame`] / [`detach_layer`] 均为 AppKit / CoreAnimation
//! 视图树操作，**必须在主线程调用**（由 Tauri `run_on_main_thread` 派发）。
//!
//! # 借用纪律（T21 坑 4）
//! `layer` 指针来自 `MetalRenderer::layer_ptr()`，为借用语义：本模块绝不 release
//! 或额外改动其所有权计数；[`detach_layer`] 只调用 `removeFromSuperlayer`，由
//! `MetalRenderer` 在会话线程随 `ControllerPipeline` 析构时释放。

#![allow(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex, OnceLock};

use crate::session_bridge::LayerLease;

/// Dock 右键菜单中展示的设备条目（纯安全跨平台数据结构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockDeviceItem {
    /// 设备唯一标识。
    pub id: String,
    /// 设备展示名称。
    pub name: String,
    /// 连接目标（9 位设备码或 `ip:port`）。
    pub target: String,
    /// 显示器子菜单展示文案（如 `"显示屏1 (Built-in Retina Display)"`）。
    pub display_label: String,
    /// 设备当前是否在线。
    pub online: bool,
    /// 连接该设备时是否需要输入访问密码。
    pub requires_password: bool,
}

/// 用户点击 macOS Dock 右键菜单条目时触发的动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockMenuAction {
    /// 打开并聚焦主窗口。
    OpenMain,
    /// 发起对指定设备的远控连接。
    ConnectDevice {
        /// 连接目标（9 位设备码或 `ip:port`）。
        target: String,
        /// 设备展示名称。
        name: String,
        /// 是否需要弹窗输入密码。
        requires_password: bool,
    },
}

type DockActionHandler = Arc<dyn Fn(DockMenuAction) + Send + Sync>;

static DOCK_DEVICES: OnceLock<Mutex<Vec<DockDeviceItem>>> = OnceLock::new();
static DOCK_ACTION_HANDLER: OnceLock<Mutex<Option<DockActionHandler>>> = OnceLock::new();

fn dock_devices_slot() -> &'static Mutex<Vec<DockDeviceItem>> {
    DOCK_DEVICES.get_or_init(|| Mutex::new(Vec::new()))
}

fn dock_handler_slot() -> &'static Mutex<Option<DockActionHandler>> {
    DOCK_ACTION_HANDLER.get_or_init(|| Mutex::new(None))
}

/// 更新 Dock 右键菜单展示的在线设备列表（线程安全；非 macOS 平台同步更新内存快照）。
pub fn update_dock_menu_devices(devices: Vec<DockDeviceItem>) {
    *dock_devices_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = devices;
}

/// 在 macOS 主线程预分配并保活的 `CAMetalLayer` 句柄（供后台 `MetalRenderer::from_raw_layer` 领养）。
///
/// 实测 macOS 上若在后台线程调用 `[CAMetalLayer layer]`，其底层 `CAContext` 无法绑定到
/// 主线程 WindowServer 合成树，导致 `presentDrawable` 永不上屏（黑屏）；因此必须由主线程
/// 创建后跨线程移交强引用给 `SessionHandle`。
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct MainThreadMetalLayer {
    layer: objc2::rc::Retained<objc2_quartz_core::CAMetalLayer>,
}

#[cfg(target_os = "macos")]
impl std::fmt::Debug for MainThreadMetalLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainThreadMetalLayer")
            .field("ptr", &objc2::rc::Retained::as_ptr(&self.layer))
            .finish()
    }
}

// SAFETY: `CAMetalLayer`（`CALayer` 子类）的 Objective-C retain/release 线程安全；
// 仅在移交给渲染器前短暂跨线程持有，后续由 `MetalRenderer` 独占驱动并在主线程挂载。
#[cfg(target_os = "macos")]
unsafe impl Send for MainThreadMetalLayer {}
// SAFETY: 同上，不提供跨线程可变共享，仅允许读取只读指针与克隆强引用。
#[cfg(target_os = "macos")]
unsafe impl Sync for MainThreadMetalLayer {}

#[cfg(target_os = "macos")]
impl MainThreadMetalLayer {
    /// 返回底层 `CAMetalLayer` 借用指针（供 `MetalRenderer::from_raw_layer` 领养）。
    pub fn as_ptr(&self) -> *mut c_void {
        objc2::rc::Retained::as_ptr(&self.layer)
            .cast_mut()
            .cast::<c_void>()
    }
}

/// 非 macOS 平台占位句柄。
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone)]
pub struct MainThreadMetalLayer;

#[cfg(not(target_os = "macos"))]
impl MainThreadMetalLayer {
    /// 非 macOS 平台占位指针。
    pub fn as_ptr(&self) -> *mut c_void {
        std::ptr::null_mut()
    }
}

/// 在 macOS 主线程创建 `CAMetalLayer` 实例（非主线程调用返回 `Err`）。
#[cfg(target_os = "macos")]
pub fn new_metal_layer_on_main_thread() -> Result<MainThreadMetalLayer, String> {
    use objc2::MainThreadMarker;
    use objc2_quartz_core::CAMetalLayer;

    if MainThreadMarker::new().is_none() {
        return Err("new_metal_layer_on_main_thread 必须在主线程调用".to_string());
    }
    Ok(MainThreadMetalLayer {
        layer: CAMetalLayer::layer(),
    })
}

/// 非 macOS 平台占位实现。
#[cfg(not(target_os = "macos"))]
pub fn new_metal_layer_on_main_thread() -> Result<MainThreadMetalLayer, String> {
    Err("metal_attach 仅支持 macOS".to_string())
}

/// stage 区域（物理像素；Retina 上 = CSS 像素 × `backingScaleFactor`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    /// 原点 X（物理像素）。
    pub x: f64,
    /// 原点 Y（物理像素）。
    pub y: f64,
    /// 宽度（物理像素）。
    pub width: f64,
    /// 高度（物理像素）。
    pub height: f64,
}

impl Frame {
    /// 在当前 stage 物理像素矩形内按视频宽高 `(video_w, video_h)` 等比居中缩放（aspect-fit）。
    ///
    /// 与 [`crate::input_bridge::normalize_pointer_in_video`] 共享 [`crate::input_bridge::fit_video_rect`]，
    /// 保证硬件渲染区域与指针归一化坐标完全重合。
    pub fn fit_aspect(self, video_w: u32, video_h: u32) -> Self {
        let (ox, oy, fw, fh) =
            crate::input_bridge::fit_video_rect(self.width, self.height, video_w, video_h);
        Self {
            x: self.x + ox,
            y: self.y + oy,
            width: fw,
            height: fh,
        }
    }

    /// 校验矩形各分量均为有限值且宽高非负。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn validate(&self) -> Result<(), String> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.width.is_finite()
            || !self.height.is_finite()
            || self.width < 0.0
            || self.height < 0.0
        {
            return Err(format!("非法 layer frame：{self:?}"));
        }
        Ok(())
    }
}

/// 将 `stage_rect` 上报的物理像素矩形换算为 CoreAnimation `CALayer.frame`（逻辑点，含 Y 轴翻转）。
///
/// - `frame`：`commands.rs::stage_rect` 传入的物理像素矩形（原点位于视图左上角、Y 向下）；
/// - `scale`：窗口 `backingScaleFactor`（Retina 2x 为 `2.0`，非法或 `<= 0` 时按 `1.0` 处理）；
/// - `super_height`：父视图 / superlayer 的逻辑高度（单位：points）；
/// - `is_flipped`：父层坐标系是否已翻转（`true` = 原点左上、Y 向下；`false` = AppKit 默认原点左下、Y 向上）。
///
/// 返回 `(x, y, width, height)`（单位：points）。
pub fn physical_frame_to_layer_rect(
    frame: Frame,
    scale: f64,
    super_height: f64,
    is_flipped: bool,
) -> (f64, f64, f64, f64) {
    let eff_scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let pt_x = frame.x / eff_scale;
    let pt_y = frame.y / eff_scale;
    let pt_w = (frame.width / eff_scale).max(0.0);
    let pt_h = (frame.height / eff_scale).max(0.0);
    let ca_y = if is_flipped {
        pt_y
    } else {
        let eff_super_h = if super_height.is_finite() && super_height > 0.0 {
            super_height
        } else {
            pt_y + pt_h
        };
        eff_super_h - (pt_y + pt_h)
    };
    (pt_x, ca_y, pt_w, pt_h)
}

/// 在主线程关闭 CoreAnimation 隐式动画的 RAII 守卫（防窗口 resize 时 250ms 缓动拖尾）。
#[cfg(target_os = "macos")]
struct CaDisableActionsGuard;

#[cfg(target_os = "macos")]
impl CaDisableActionsGuard {
    /// 开启无隐式动画的 `CATransaction` 事务。
    ///
    /// # Safety
    /// 必须在 macOS 主线程调用（调用方已通过 `MainThreadMarker::new()` 校验）。
    unsafe fn begin() -> Self {
        use objc2::{class, msg_send};

        let ca_tx = class!(CATransaction);
        // SAFETY: `CATransaction` 为 QuartzCore 公开类，`+begin` / `+setDisableActions:`
        // 在主线程合法调用，析构时由 `Drop` 成对调用 `+commit`。
        unsafe {
            let _: () = msg_send![ca_tx, begin];
            let _: () = msg_send![ca_tx, setDisableActions: true];
        }
        Self
    }
}

#[cfg(target_os = "macos")]
impl Drop for CaDisableActionsGuard {
    fn drop(&mut self) {
        use objc2::{class, msg_send};

        let ca_tx = class!(CATransaction);
        // SAFETY: 与 `CaDisableActionsGuard::begin` 里的 `+begin` 严格成对提交。
        unsafe {
            let _: () = msg_send![ca_tx, commit];
        }
    }
}

/// 从父 `CALayer` 解析准确的 Retina 缩放因子（优先读取宿主 `NSView.window.backingScaleFactor`）。
///
/// # Safety
/// 必须在 macOS 主线程调用，且 `superlayer` / `child` 为存活的 `CALayer` 引用。
#[cfg(target_os = "macos")]
unsafe fn resolve_scale_from_superlayer(
    superlayer: &objc2_quartz_core::CALayer,
    child: &objc2_quartz_core::CALayer,
) -> f64 {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, sel};

    // SAFETY: 主线程查询层代理（AppKit 层宿主 NSView）；通过 `respondsToSelector:` 保护
    // 再读取 `window` 与 `backingScaleFactor`，不改变任何对象引用计数。
    unsafe {
        let delegate: *mut AnyObject = msg_send![superlayer, delegate];
        if !delegate.is_null() {
            let responds_window: bool = msg_send![delegate, respondsToSelector: sel!(window)];
            if responds_window {
                let ns_win: *mut AnyObject = msg_send![delegate, window];
                if !ns_win.is_null() {
                    let s: f64 = msg_send![ns_win, backingScaleFactor];
                    if s.is_finite() && s > 0.0 {
                        return s;
                    }
                }
            }
        }
    }

    let super_s = superlayer.contentsScale();
    if super_s.is_finite() && super_s > 0.0 {
        return super_s;
    }
    let child_s = child.contentsScale();
    if child_s.is_finite() && child_s > 0.0 {
        return child_s;
    }
    1.0
}

// 主线程独占的层宿主 `NSView` 与 ProMotion `CADisplayLink` 注册表（key = `CAMetalLayer` 指针地址）。
//
// 采用 Apple 官方推荐的 **layer-hosting `NSView`** 模式（先 `setLayer:` 再 `setWantsLayer:YES`），
// 再以 `addSubview:positioned:NSWindowBelow relativeTo:webview` 插到 `WryWebView` 之下：
// 避免直接向 AppKit layer-backed `WryWebViewParent` 插入裸 sublayer 或设置负 `zPosition`
// 导致 `CAMetalLayer` 被压到 `NSWindow` 背景色之下的黑屏问题。
#[cfg(target_os = "macos")]
std::thread_local! {
    static HOST_VIEWS: std::cell::RefCell<
        std::collections::HashMap<usize, objc2::rc::Retained<objc2::runtime::AnyObject>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static DISPLAY_LINKS: std::cell::RefCell<
        std::collections::HashMap<usize, DisplayLinkEntry>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
    static DOCK_TARGETS: std::cell::RefCell<
        Vec<objc2::rc::Retained<DockMenuTarget>>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct CaFrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

// SAFETY: 布局与 QuartzCore `CAFrameRateRange`（三个 `float` 字段）及 ObjC `@encode(CAFrameRateRange)` 严格一致。
#[cfg(target_os = "macos")]
unsafe impl objc2::Encode for CaFrameRateRange {
    const ENCODING: objc2::Encoding = objc2::Encoding::Struct(
        "CAFrameRateRange",
        &[
            <f32 as objc2::Encode>::ENCODING,
            <f32 as objc2::Encode>::ENCODING,
            <f32 as objc2::Encode>::ENCODING,
        ],
    );
}

#[cfg(target_os = "macos")]
objc2::define_class!(
    // SAFETY:
    // - 父类 `NSObject` 无额外子类化约束；
    // - `#[thread_kind = MainThreadOnly]` 将该 ObjC 类的分配与回调限定在主线程；
    // - 无自定义 ivars，不涉及跨线程共享或 Drop 副作用。
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    struct DisplayLinkTarget;

    impl DisplayLinkTarget {
        #[unsafe(method(onDisplayLink:))]
        fn on_display_link(&self, _link: Option<&objc2::runtime::AnyObject>) {}
    }
);

#[cfg(target_os = "macos")]
impl DisplayLinkTarget {
    fn new(mtm: objc2::MainThreadMarker) -> objc2::rc::Retained<Self> {
        use objc2::{msg_send, MainThreadOnly};

        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `this` 为刚分配并初始化 ivars 的 `DisplayLinkTarget`，向父类 `NSObject` 发送 `init` 返回 +1 强引用。
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(target_os = "macos")]
struct DisplayLinkEntry {
    link: objc2::rc::Retained<objc2::runtime::AnyObject>,
    _target: objc2::rc::Retained<DisplayLinkTarget>,
}

/// 在 macOS 14+ 窗口上绑定 ProMotion 120Hz `CADisplayLink` 提示系统拉高合成刷新率。
///
/// # Safety
/// 必须在 macOS 主线程调用，且 `ns_win` 为存活的 `NSWindow` 指针。
#[cfg(target_os = "macos")]
unsafe fn attach_display_link_for_layer(
    mtm: objc2::MainThreadMarker,
    ns_win: *mut objc2::runtime::AnyObject,
    layer_key: usize,
) {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel};

    if ns_win.is_null() {
        return;
    }

    #[link(name = "Foundation", kind = "framework")]
    extern "C" {
        static NSRunLoopCommonModes: *const AnyObject;
    }

    // SAFETY: 主线程检测 macOS 14+ `-[NSWindow displayLinkWithTarget:selector:]` 后创建 `CADisplayLink`，
    // 设置 `preferredFrameRateRange = {60.0, 120.0, 120.0}` 并注册到 `NSRunLoop.mainRunLoop`；
    // 强引用与目标一起保存在主线程 `DISPLAY_LINKS` 表中，并在 `remove_host_view_for_layer` 时成对 `invalidate`。
    unsafe {
        let responds_dl: bool = msg_send![
            ns_win,
            respondsToSelector: sel!(displayLinkWithTarget:selector:)
        ];
        if !responds_dl {
            return;
        }
        let target = DisplayLinkTarget::new(mtm);
        let link: Option<Retained<AnyObject>> = msg_send![
            ns_win,
            displayLinkWithTarget: &*target,
            selector: sel!(onDisplayLink:)
        ];
        let Some(link) = link else {
            return;
        };
        let responds_range: bool = msg_send![
            &*link,
            respondsToSelector: sel!(setPreferredFrameRateRange:)
        ];
        if responds_range {
            let range = CaFrameRateRange {
                minimum: 60.0,
                maximum: 120.0,
                preferred: 120.0,
            };
            let _: () = msg_send![&*link, setPreferredFrameRateRange: range];
        }
        if let Some(ns_run_loop_cls) = AnyClass::get(c"NSRunLoop") {
            let main_run_loop: *mut AnyObject = msg_send![ns_run_loop_cls, mainRunLoop];
            let common_modes = NSRunLoopCommonModes;
            if !main_run_loop.is_null() && !common_modes.is_null() {
                let _: () = msg_send![
                    &*link,
                    addToRunLoop: main_run_loop,
                    forMode: common_modes
                ];
            }
        }
        DISPLAY_LINKS.with(|map| {
            map.borrow_mut().insert(
                layer_key,
                DisplayLinkEntry {
                    link,
                    _target: target,
                },
            );
        });
    }
}

/// 清理并摘除先前为该 `CAMetalLayer` 创建的宿主 `NSView` 与 `CADisplayLink`（须在 macOS 主线程调用）。
///
/// # Safety
/// 必须在 macOS 主线程调用（调用方已通过 `MainThreadMarker::new()` 校验）。
#[cfg(target_os = "macos")]
unsafe fn remove_host_view_for_layer(layer_key: usize) {
    use objc2::msg_send;

    let removed_link = DISPLAY_LINKS.with(|map| map.borrow_mut().remove(&layer_key));
    if let Some(entry) = removed_link {
        // SAFETY: `entry.link` 为此前在主线程注册到 `NSRunLoop.mainRunLoop` 的存活 `CADisplayLink`；
        // 先调用 `invalidate` 解绑 RunLoop 与 target，再随 `Retained` 释放。
        unsafe {
            let _: () = msg_send![&*entry.link, invalidate];
        }
    }

    let removed = HOST_VIEWS.with(|map| map.borrow_mut().remove(&layer_key));
    if let Some(metal_view) = removed {
        // SAFETY: `metal_view` 为此前主线程创建的存活 `NSView`；从父视图移除后随 `Retained` 释放，
        // 绝不触碰渲染器持有的 `CAMetalLayer` 所有权。
        unsafe {
            let _: () = msg_send![&*metal_view, removeFromSuperview];
        }
    }
}

/// 将 `MetalRenderer::layer_ptr()` 借出的 `CAMetalLayer` 挂到 Tauri 窗口 `NSView`（layer-backed 子视图）。
///
/// **必须在主线程调用**。不改变 `layer` 的所有权（不手动 retain/release）。
///
/// 层级与坐标系约定（macOS Tauri v2 / wry）：
/// - `win.ns_view()` 返回 `WryWebViewParent`（`NSWindow.contentView`，`isFlipped == false`），
///   其子视图为透明 `WKWebView`（`drawsBackground = NO`）；
/// - 本函数创建专用 layer-backed `NSView`（`setWantsLayer:YES`，由 AppKit 托管其根 `CALayer`），
///   将 `CAMetalLayer` 作为 `host_layer` 的子层（`frame = (0, 0, pt_w, pt_h)`、`zPosition = 0.0`）
///   并通过 `addSubview:positioned:NSWindowBelow(-1) relativeTo:firstSubview` 置于 `WKWebView` 之下、
///   `NSWindow` 背景色之上，使透明 `.sn-stage` 露出实时 60fps 硬件视频，同时保持 `.sn-bar`、
///   S2/S3 浮层、S4 HUD 在视频之上清晰可见且可交互；
/// - 物理像素 `Frame` 按 `backingScaleFactor` 换算为逻辑点并处理 AppKit 左下角 Y 轴翻转，
///   同时设置 `child.setContentsScale(scale)` 与 `CATransaction` 禁用隐式动画。
///
/// # Safety
/// - `ns_view` 必须指向存活的 `NSView` 实例；
/// - `layer` 必须指向存活的 `CAMetalLayer`（isa `CALayer`），且在调用期间不得被释放；
/// - 必须在 macOS 主线程调用（本函数会校验 `MainThreadMarker::new()`，非主线程返回 `Err`）。
#[cfg(target_os = "macos")]
pub unsafe fn attach_layer(
    ns_view: *mut c_void,
    layer: *mut c_void,
    frame: Frame,
) -> Result<(), String> {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, MainThreadMarker};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_quartz_core::CALayer;

    let Some(mtm) = MainThreadMarker::new() else {
        return Err("metal_attach::attach_layer 必须在主线程调用".to_string());
    };
    if ns_view.is_null() {
        return Err("ns_view 指针为空".to_string());
    }
    if layer.is_null() {
        return Err("layer 指针为空".to_string());
    }
    frame.validate()?;

    let layer_key = layer as usize;

    // SAFETY:
    // 1. 已通过 `MainThreadMarker::new()` 确认当前位于主线程；调用方保证 `ns_view`
    //    指向存活的 `NSView`、`layer` 指向 `MetalRenderer::layer_ptr()` 借出的存活
    //    `CAMetalLayer`（isa `CALayer`）。
    // 2. 先清理同一 `layer` 的历史宿主视图，保证重复挂载幂等无泄漏。
    // 3. 创建 layer-backed `NSView`（`setWantsLayer:YES`），将 `CAMetalLayer` 作为其
    //    `host_layer` 的子层挂接（避免直接 `setLayer:` 在跨线程 `CAMetalLayer` 上不合成的问题），
    //    并以 `NSWindowBelow (-1)` 插到 `contentView` 首个子视图（`WryWebView`）下方。
    // 4. 若关联 `NSWindow` 存在，将窗口保持不透明并设为 `STAGE_BG`（`#1F2229`），避免首帧
    //    到达前透过透明 WebView 露出桌面的其他窗口。
    unsafe {
        remove_host_view_for_layer(layer_key);

        let view = ns_view.cast::<AnyObject>();
        let ns_win: *mut AnyObject = msg_send![view, window];
        let raw_scale = if !ns_win.is_null() {
            let s: f64 = msg_send![ns_win, backingScaleFactor];
            s
        } else {
            1.0
        };
        let scale = if raw_scale.is_finite() && raw_scale > 0.0 {
            raw_scale
        } else {
            1.0
        };

        let view_bounds: CGRect = msg_send![view, bounds];
        let is_flipped: bool = msg_send![view, isFlipped];
        let (pt_x, ca_y, pt_w, pt_h) =
            physical_frame_to_layer_rect(frame, scale, view_bounds.size.height, is_flipped);
        let rect = CGRect::new(CGPoint::new(pt_x, ca_y), CGSize::new(pt_w, pt_h));
        let local_bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pt_w, pt_h));

        if !ns_win.is_null() {
            if let Some(ns_color_cls) = AnyClass::get(c"NSColor") {
                let stage_bg: *mut AnyObject = msg_send![
                    ns_color_cls,
                    colorWithSRGBRed: (0x1f_u8 as f64) / 255.0_f64,
                    green: (0x22_u8 as f64) / 255.0_f64,
                    blue: (0x29_u8 as f64) / 255.0_f64,
                    alpha: 1.0_f64
                ];
                if !stage_bg.is_null() {
                    let _: () = msg_send![ns_win, setOpaque: true];
                    let _: () = msg_send![ns_win, setBackgroundColor: stage_bg];
                }
            }
        }

        let ns_view_cls = AnyClass::get(c"NSView").ok_or_else(|| "找不到 NSView 类".to_string())?;
        let raw_alloc: *mut AnyObject = msg_send![ns_view_cls, alloc];
        let raw_metal_view: *mut AnyObject = msg_send![raw_alloc, initWithFrame: rect];
        let metal_view = Retained::from_raw(raw_metal_view)
            .ok_or_else(|| "创建 Metal 宿主 NSView 失败".to_string())?;

        // NSViewWidthSizable (2) | NSViewHeightSizable (16)：窗口拖拽缩放瞬间平滑跟随，
        // 随后由 JS ResizeObserver → stage_rect → sync_layer_frame 精确对齐。
        let _: () = msg_send![&*metal_view, setAutoresizingMask: 18usize];
        let _: () = msg_send![&*metal_view, setWantsLayer: true];
        let host_layer: *mut CALayer = msg_send![&*metal_view, layer];
        if host_layer.is_null() {
            return Err("Metal 宿主 NSView.layer 为空".to_string());
        }

        let child: &CALayer = &*layer.cast::<CALayer>();
        let _tx = CaDisableActionsGuard::begin();
        if child.superlayer().is_some() {
            child.removeFromSuperlayer();
        }
        child.setContentsScale(scale);
        child.setOpaque(true);
        child.setZPosition(0.0);
        child.setFrame(local_bounds);
        (*host_layer).addSublayer(child);

        let subviews: *mut AnyObject = msg_send![view, subviews];
        let subview_count: usize = if subviews.is_null() {
            0
        } else {
            msg_send![subviews, count]
        };
        if subview_count > 0 {
            let first_subview: *mut AnyObject = msg_send![subviews, objectAtIndex: 0usize];
            // NSWindowOrderingMode::NSWindowBelow == -1
            let _: () = msg_send![
                view,
                addSubview: &*metal_view,
                positioned: -1_isize,
                relativeTo: first_subview
            ];
        } else {
            let _: () = msg_send![view, addSubview: &*metal_view];
        }

        HOST_VIEWS.with(|map| {
            map.borrow_mut().insert(layer_key, metal_view);
        });

        attach_display_link_for_layer(mtm, ns_win, layer_key);
    }

    Ok(())
}

/// 非 macOS 平台占位实现：直接返回错误。
///
/// # Safety
/// 与 macOS 版本保持一致的 `unsafe fn` 签名约束；本平台不解引用任何指针。
#[cfg(not(target_os = "macos"))]
pub unsafe fn attach_layer(
    _ns_view: *mut c_void,
    _layer: *mut c_void,
    _frame: Frame,
) -> Result<(), String> {
    Err("metal_attach 仅支持 macOS".to_string())
}

/// 窗口 resize 或 stage 布局变化时同步宿主 `NSView` 与 `CALayer` frame（**必须在主线程调用**）。
///
/// 优先更新关联的 layer-hosting `NSView.frame` 与 `child.contentsScale`，若为脱离宿主视图的
/// 独立层则回退读取 `child.superlayer()` 同步。
///
/// # Safety
/// - `layer` 必须指向存活的 `CAMetalLayer`（isa `CALayer`），且在调用期间不得被释放；
/// - 必须在 macOS 主线程调用（本函数会校验 `MainThreadMarker::new()`，非主线程返回 `Err`）。
#[cfg(target_os = "macos")]
pub unsafe fn sync_layer_frame(layer: *mut c_void, frame: Frame) -> Result<(), String> {
    use objc2::runtime::AnyObject;
    use objc2::{msg_send, MainThreadMarker};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_quartz_core::CALayer;

    if MainThreadMarker::new().is_none() {
        return Err("metal_attach::sync_layer_frame 必须在主线程调用".to_string());
    }
    if layer.is_null() {
        return Err("layer 指针为空".to_string());
    }
    frame.validate()?;

    let layer_key = layer as usize;
    let host_view = HOST_VIEWS.with(|map| map.borrow().get(&layer_key).cloned());

    // SAFETY: 已通过 `MainThreadMarker::new()` 确认位于主线程，调用方保证 `layer`
    // 指向存活的 `CAMetalLayer`（isa `CALayer`）；若存在托管 `NSView` 则在无隐式动画的
    // `CATransaction` 内更新 `NSView.frame` 与 `contentsScale`，否则回退更新 `CALayer.frame`。
    unsafe {
        let child: &CALayer = &*layer.cast::<CALayer>();
        if let Some(metal_view) = host_view {
            let superview: *mut AnyObject = msg_send![&*metal_view, superview];
            let ns_win: *mut AnyObject = msg_send![&*metal_view, window];
            let raw_scale = if !ns_win.is_null() {
                let s: f64 = msg_send![ns_win, backingScaleFactor];
                s
            } else {
                child.contentsScale()
            };
            let scale = if raw_scale.is_finite() && raw_scale > 0.0 {
                raw_scale
            } else {
                1.0
            };
            let (super_height, is_flipped) = if !superview.is_null() {
                let b: CGRect = msg_send![superview, bounds];
                let f: bool = msg_send![superview, isFlipped];
                (b.size.height, f)
            } else {
                (0.0, true)
            };
            let (pt_x, ca_y, pt_w, pt_h) =
                physical_frame_to_layer_rect(frame, scale, super_height, is_flipped);
            let rect = CGRect::new(CGPoint::new(pt_x, ca_y), CGSize::new(pt_w, pt_h));
            let local_bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(pt_w, pt_h));

            let _tx = CaDisableActionsGuard::begin();
            let _: () = msg_send![&*metal_view, setFrame: rect];
            child.setContentsScale(scale);
            child.setFrame(local_bounds);
            return Ok(());
        }

        let (scale, super_height, is_flipped) = match child.superlayer() {
            Some(superlayer) => {
                let scale = resolve_scale_from_superlayer(&superlayer, child);
                let bounds = superlayer.bounds();
                (scale, bounds.size.height, superlayer.isGeometryFlipped())
            }
            None => {
                let s = child.contentsScale();
                let scale = if s.is_finite() && s > 0.0 { s } else { 1.0 };
                (scale, 0.0, true)
            }
        };
        let (pt_x, ca_y, pt_w, pt_h) =
            physical_frame_to_layer_rect(frame, scale, super_height, is_flipped);
        let rect = CGRect::new(CGPoint::new(pt_x, ca_y), CGSize::new(pt_w, pt_h));

        let _tx = CaDisableActionsGuard::begin();
        child.setContentsScale(scale);
        child.setFrame(rect);
    }

    Ok(())
}

/// 非 macOS 平台占位实现：直接返回错误。
///
/// # Safety
/// 与 macOS 版本保持一致的 `unsafe fn` 签名约束；本平台不解引用任何指针。
#[cfg(not(target_os = "macos"))]
pub unsafe fn sync_layer_frame(_layer: *mut c_void, _frame: Frame) -> Result<(), String> {
    Err("metal_attach 仅支持 macOS".to_string())
}

/// 会话结束：从宿主 `NSView` 与父层移除（**必须在主线程调用**）。
///
/// **绝不 release `layer`**；空指针或非主线程为 no-op。
///
/// # Safety
/// - `layer` 为空指针时安全 no-op；非空时必须指向存活的 `CAMetalLayer`（isa `CALayer`）；
/// - 必须在 macOS 主线程调用（非主线程直接返回 no-op，避免跨线程触碰 AppKit/CALayer）。
#[cfg(target_os = "macos")]
pub unsafe fn detach_layer(layer: *mut c_void) {
    use objc2::MainThreadMarker;
    use objc2_quartz_core::CALayer;

    if MainThreadMarker::new().is_none() || layer.is_null() {
        return;
    }

    let layer_key = layer as usize;

    // SAFETY: 已通过 `MainThreadMarker::new()` 确认位于主线程，调用方保证 `layer`
    // 指向存活的 `CAMetalLayer`（isa `CALayer`）；移除宿主 `NSView` 并解除 `setLayer:`
    // 绑定后再 `removeFromSuperlayer`，不释放渲染器持有的强引用。
    unsafe {
        remove_host_view_for_layer(layer_key);
        let child: &CALayer = &*layer.cast::<CALayer>();
        child.removeFromSuperlayer();
    }
}

/// 非 macOS 平台占位实现：no-op。
///
/// # Safety
/// 与 macOS 版本保持一致的 `unsafe fn` 签名约束；本平台不解引用任何指针。
#[cfg(not(target_os = "macos"))]
pub unsafe fn detach_layer(_layer: *mut c_void) {}

/// 安全封装：将租约保活的 `CAMetalLayer` 挂到 Tauri `WebviewWindow` 的 `NSView`（须在主线程调用）。
#[cfg(target_os = "macos")]
pub fn attach_layer_to_window(
    win: &tauri::WebviewWindow,
    lease: &LayerLease,
    frame: Frame,
) -> Result<(), String> {
    let ns_view = win
        .ns_view()
        .map_err(|e| format!("获取窗口 NSView 失败: {e}"))?;
    // SAFETY: `win.ns_view()` 返回当前存活窗口的 `NSView` 指针，`lease` 在整个调用期间
    // 持有底层 `MetalRenderer` 强引用保活 `CAMetalLayer`，且 `attach_layer` 内部会校验
    // `MainThreadMarker::new()`。
    unsafe { attach_layer(ns_view, lease.as_ptr(), frame) }
}

/// 非 macOS 平台占位实现：返回不支持错误。
#[cfg(not(target_os = "macos"))]
pub fn attach_layer_to_window(
    _win: &tauri::WebviewWindow,
    _lease: &LayerLease,
    _frame: Frame,
) -> Result<(), String> {
    Err("metal_attach 仅支持 macOS".to_string())
}

/// 安全封装：同步租约保活的 `CAMetalLayer` frame（须在主线程调用）。
pub fn sync_leased_layer_frame(lease: &LayerLease, frame: Frame) -> Result<(), String> {
    // SAFETY: `lease` 在整个调用期间持有底层 `MetalRenderer` 强引用保活 `CAMetalLayer`，
    // 且 `sync_layer_frame` 内部在 macOS 上会校验 `MainThreadMarker::new()`。
    unsafe { sync_layer_frame(lease.as_ptr(), frame) }
}

/// 安全封装：从父层摘除租约保活的 `CAMetalLayer`（须在主线程调用）。
pub fn detach_leased_layer(lease: &LayerLease) {
    // SAFETY: `lease` 在整个调用期间持有底层 `MetalRenderer` 强引用保活 `CAMetalLayer`，
    // 且 `detach_layer` 内部在 macOS 上会校验 `MainThreadMarker::new()`。
    unsafe { detach_layer(lease.as_ptr()) }
}

#[cfg(target_os = "macos")]
struct DockMenuTargetIvars {
    action: DockMenuAction,
}

#[cfg(target_os = "macos")]
objc2::define_class!(
    // SAFETY:
    // - 父类 `NSObject` 无额外子类化约束；
    // - `#[thread_kind = MainThreadOnly]` 将该 ObjC 类的分配与点击回调限定在主线程；
    // - `DockMenuTargetIvars` 仅含纯值语义的 `DockMenuAction`。
    #[unsafe(super(objc2::runtime::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[ivars = DockMenuTargetIvars]
    struct DockMenuTarget;

    impl DockMenuTarget {
        #[unsafe(method(onDockItem:))]
        fn on_dock_item(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            use objc2::DefinedClass;

            let handler = dock_handler_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(cb) = handler {
                cb(self.ivars().action.clone());
            }
        }
    }
);

#[cfg(target_os = "macos")]
impl DockMenuTarget {
    fn new(mtm: objc2::MainThreadMarker, action: DockMenuAction) -> objc2::rc::Retained<Self> {
        use objc2::{msg_send, MainThreadOnly};

        let this = Self::alloc(mtm).set_ivars(DockMenuTargetIvars { action });
        // SAFETY: `this` 为刚分配并写入 `DockMenuTargetIvars` 的实例，向父类 `NSObject` 发送 `init` 返回 +1 强引用。
        unsafe { msg_send![super(this), init] }
    }
}

/// 在主线程构造 `NSString`（清理 NUL 字节，防 C 字符串截断）。
///
/// # Safety
/// 必须在 macOS 主线程或合法 autorelease pool 上下文中调用。
#[cfg(target_os = "macos")]
unsafe fn make_ns_string(text: &str) -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    let cls = AnyClass::get(c"NSString")?;
    let sanitized = if text.contains('\0') {
        std::borrow::Cow::Owned(text.replace('\0', ""))
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let cstr = std::ffi::CString::new(sanitized.as_ref()).ok()?;
    // SAFETY: `NSString +stringWithUTF8String:` 接收以 NUL 结尾的 UTF-8 指针并返回 autoreleased `NSString*`，
    // 由 `msg_send!` 直接 retain 为 `Option<Retained<AnyObject>>`。
    unsafe { msg_send![cls, stringWithUTF8String: cstr.as_ptr()] }
}

/// 在主线程创建 `NSMenuItem`（无绑定动作与快捷键）。
///
/// # Safety
/// 必须在 macOS 主线程调用，`item_cls` 须为 `NSMenuItem` 类对象。
#[cfg(target_os = "macos")]
unsafe fn make_ns_menu_item(
    item_cls: &objc2::runtime::AnyClass,
    title: &str,
) -> Option<objc2::rc::Retained<objc2::runtime::AnyObject>> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Sel};

    // SAFETY: 主线程创建 `NSString` 与 `NSMenuItem`，遵循 `alloc`/`initWithTitle:action:keyEquivalent:` +1 所有权。
    unsafe {
        let ns_title = make_ns_string(title)?;
        let ns_key = make_ns_string("")?;
        let raw_alloc: *mut AnyObject = msg_send![item_cls, alloc];
        let raw_item: *mut AnyObject = msg_send![
            raw_alloc,
            initWithTitle: &*ns_title,
            action: Option::<Sel>::None,
            keyEquivalent: &*ns_key
        ];
        Retained::from_raw(raw_item)
    }
}

/// 为 macOS Dock 右键菜单动态构建并返回已加入 autorelease pool 的 `NSMenu*`。
///
/// # Safety
/// 必须由 AppKit 在 macOS 主线程的 `applicationDockMenu:` 回调中调用。
#[cfg(target_os = "macos")]
unsafe fn build_autoreleased_dock_menu() -> *mut objc2::runtime::AnyObject {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{msg_send, sel, MainThreadMarker};

    let Some(mtm) = MainThreadMarker::new() else {
        return std::ptr::null_mut();
    };
    let Some(menu_cls) = AnyClass::get(c"NSMenu") else {
        return std::ptr::null_mut();
    };
    let Some(item_cls) = AnyClass::get(c"NSMenuItem") else {
        return std::ptr::null_mut();
    };

    let devices: Vec<DockDeviceItem> = dock_devices_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|d| d.online)
        .cloned()
        .collect();

    let mut new_targets: Vec<Retained<DockMenuTarget>> = Vec::new();

    // SAFETY: 全程在 macOS 主线程操作 `NSMenu`/`NSMenuItem`，所有 `NSMenuItem.target` 强引用
    // 保存在主线程 `DOCK_TARGETS` 中直至下次打开 Dock 菜单时替换，最后通过 `Retained::autorelease_return`
    // 按 Cocoa `applicationDockMenu:` 约定返回 autoreleased 对象。
    unsafe {
        let Some(empty_title) = make_ns_string("") else {
            return std::ptr::null_mut();
        };
        let raw_menu: *mut AnyObject = msg_send![menu_cls, alloc];
        let raw_menu: *mut AnyObject = msg_send![raw_menu, initWithTitle: &*empty_title];
        let Some(menu) = Retained::from_raw(raw_menu) else {
            return std::ptr::null_mut();
        };
        let _: () = msg_send![&*menu, setAutoenablesItems: false];

        if devices.is_empty() {
            if let Some(empty_item) = make_ns_menu_item(item_cls, "暂无在线设备") {
                let _: () = msg_send![&*empty_item, setEnabled: false];
                let _: () = msg_send![&*menu, addItem: &*empty_item];
            }
        } else {
            for dev in devices {
                let Some(parent_item) = make_ns_menu_item(item_cls, &dev.name) else {
                    continue;
                };
                let _: () = msg_send![&*parent_item, setEnabled: true];

                let Some(sub_title_ns) = make_ns_string(&dev.name) else {
                    continue;
                };
                let raw_sub: *mut AnyObject = msg_send![menu_cls, alloc];
                let raw_sub: *mut AnyObject = msg_send![raw_sub, initWithTitle: &*sub_title_ns];
                let Some(submenu) = Retained::from_raw(raw_sub) else {
                    continue;
                };
                let _: () = msg_send![&*submenu, setAutoenablesItems: false];

                let display_title = if dev.display_label.trim().is_empty() {
                    "显示屏1 (Built-in Retina Display)"
                } else {
                    dev.display_label.as_str()
                };
                if let Some(sub_item) = make_ns_menu_item(item_cls, display_title) {
                    let target = DockMenuTarget::new(
                        mtm,
                        DockMenuAction::ConnectDevice {
                            target: dev.target,
                            name: dev.name,
                            requires_password: dev.requires_password,
                        },
                    );
                    let _: () = msg_send![&*sub_item, setTarget: &*target];
                    let _: () = msg_send![&*sub_item, setAction: sel!(onDockItem:)];
                    let _: () = msg_send![&*sub_item, setEnabled: true];
                    new_targets.push(target);
                    let _: () = msg_send![&*submenu, addItem: &*sub_item];
                }

                let _: () = msg_send![&*parent_item, setSubmenu: &*submenu];
                let _: () = msg_send![&*menu, addItem: &*parent_item];
            }
        }

        let sep: Option<Retained<AnyObject>> = msg_send![item_cls, separatorItem];
        if let Some(sep_item) = sep {
            let _: () = msg_send![&*menu, addItem: &*sep_item];
        }
        if let Some(open_item) = make_ns_menu_item(item_cls, "打开主窗口") {
            let target = DockMenuTarget::new(mtm, DockMenuAction::OpenMain);
            let _: () = msg_send![&*open_item, setTarget: &*target];
            let _: () = msg_send![&*open_item, setAction: sel!(onDockItem:)];
            let _: () = msg_send![&*open_item, setEnabled: true];
            new_targets.push(target);
            let _: () = msg_send![&*menu, addItem: &*open_item];
        }

        DOCK_TARGETS.with(|slot| {
            *slot.borrow_mut() = new_targets;
        });

        Retained::autorelease_return(menu)
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn dock_menu_imp(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) -> *mut objc2::runtime::AnyObject {
    // SAFETY: `applicationDockMenu:` 由 AppKit 在主线程调用；内部捕获 panic 避免跨越 ObjC FFI 边界展开，
    // 返回遵循 Cocoa 约定且已注册到当前 autorelease pool 的 `NSMenu*`。
    std::panic::catch_unwind(|| unsafe { build_autoreleased_dock_menu() })
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(target_os = "macos")]
static DOCK_IMP_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 在现有 `NSApplication.sharedApplication.delegate` 类上挂载或覆写 `applicationDockMenu:` 方法，
/// **绝不替换 delegate 实例本身**，从而完整保留 Tauri 原生窗口/托盘/菜单委托链。
///
/// **必须在 macOS 主线程调用**；`on_action` 在用户点击 Dock 菜单项时同步在主线程触发。
#[cfg(target_os = "macos")]
pub fn install_dock_menu_handler(
    on_action: Arc<dyn Fn(DockMenuAction) + Send + Sync>,
) -> Result<(), String> {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{msg_send, sel, MainThreadMarker};
    use std::sync::atomic::Ordering;

    if MainThreadMarker::new().is_none() {
        return Err("install_dock_menu_handler 必须在主线程调用".to_string());
    }

    *dock_handler_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(on_action);

    if DOCK_IMP_INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }

    // SAFETY: 已校验位于 macOS 主线程；读取 `NSApplication.sharedApplication.delegate` 所属运行时类，
    // 在不替换 delegate 实例的前提下通过 `class_addMethod` / `class_replaceMethod` 安装签名严格匹配
    // `- (nullable NSMenu *)applicationDockMenu:(NSApplication *)sender`（`"@@:@"`）的 `IMP`。
    unsafe {
        let ns_app_cls =
            AnyClass::get(c"NSApplication").ok_or_else(|| "找不到 NSApplication 类".to_string())?;
        let app: *mut AnyObject = msg_send![ns_app_cls, sharedApplication];
        if app.is_null() {
            return Err("NSApplication.sharedApplication 为空".to_string());
        }
        let delegate: *mut AnyObject = msg_send![app, delegate];
        if delegate.is_null() {
            return Err("NSApplication.delegate 尚未初始化".to_string());
        }
        let delegate_cls: *const AnyClass = msg_send![delegate, class];
        if delegate_cls.is_null() {
            return Err("无法读取 NSApplication.delegate 的 Objective-C 类".to_string());
        }

        let imp_fn: unsafe extern "C-unwind" fn(
            *mut AnyObject,
            Sel,
            *mut AnyObject,
        ) -> *mut AnyObject = dock_menu_imp;
        let imp: Imp = std::mem::transmute(imp_fn);
        let added = objc2::ffi::class_addMethod(
            delegate_cls.cast_mut(),
            sel!(applicationDockMenu:),
            imp,
            c"@@:@".as_ptr(),
        );
        if !added.as_bool() {
            let _ = objc2::ffi::class_replaceMethod(
                delegate_cls.cast_mut(),
                sel!(applicationDockMenu:),
                imp,
                c"@@:@".as_ptr(),
            );
        }
    }

    DOCK_IMP_INSTALLED.store(true, Ordering::Release);
    Ok(())
}

/// 非 macOS 平台占位实现：仅记录回调（无 Dock 菜单可挂接）并返回 `Ok(())`。
#[cfg(not(target_os = "macos"))]
pub fn install_dock_menu_handler(
    on_action: Arc<dyn Fn(DockMenuAction) + Send + Sync>,
) -> Result<(), String> {
    *dock_handler_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(on_action);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_frame_to_layer_rect_handles_1x_and_2x_retina_with_toolbar_offset() {
        // 1x 屏幕、1280x800 窗口、顶部 48pt 工具栏、非翻转（AppKit 默认 bottom-left 原点）：
        // stage 占下方 (0, 48, 1280, 752) → CoreAnimation 坐标应为 (0, 0, 1280, 752)。
        let frame_1x = Frame {
            x: 0.0,
            y: 48.0,
            width: 1280.0,
            height: 752.0,
        };
        let (x, y, w, h) = physical_frame_to_layer_rect(frame_1x, 1.0, 800.0, false);
        assert!((x - 0.0).abs() < 1e-9);
        assert!((y - 0.0).abs() < 1e-9);
        assert!((w - 1280.0).abs() < 1e-9);
        assert!((h - 752.0).abs() < 1e-9);

        // 2x Retina 屏幕、同一 1280x800 逻辑窗口：
        // `stage_rect` 传入物理像素 (0, 96, 2560, 1504)，必须除以 2.0 回到逻辑点 (0, 0, 1280, 752)。
        let frame_2x = Frame {
            x: 0.0,
            y: 96.0,
            width: 2560.0,
            height: 1504.0,
        };
        let (x, y, w, h) = physical_frame_to_layer_rect(frame_2x, 2.0, 800.0, false);
        assert!((x - 0.0).abs() < 1e-9);
        assert!((y - 0.0).abs() < 1e-9);
        assert!((w - 1280.0).abs() < 1e-9);
        assert!((h - 752.0).abs() < 1e-9);

        // 2x Retina + 局域内嵌矩形（非翻转）：CSS (10, 48, 600, 400) → 物理 (20, 96, 1200, 800)，
        // AppKit Y = 800 - (48 + 400) = 352。
        let inset_2x = Frame {
            x: 20.0,
            y: 96.0,
            width: 1200.0,
            height: 800.0,
        };
        let (x, y, w, h) = physical_frame_to_layer_rect(inset_2x, 2.0, 800.0, false);
        assert!((x - 10.0).abs() < 1e-9);
        assert!((y - 352.0).abs() < 1e-9);
        assert!((w - 600.0).abs() < 1e-9);
        assert!((h - 400.0).abs() < 1e-9);
    }

    #[test]
    fn physical_frame_to_layer_rect_handles_flipped_and_degenerate_inputs() {
        // 已翻转坐标系（top-left 原点）：保留 pt_y，不做翻转。
        let frame_2x = Frame {
            x: 20.0,
            y: 96.0,
            width: 2520.0,
            height: 1504.0,
        };
        let (x, y, w, h) = physical_frame_to_layer_rect(frame_2x, 2.0, 800.0, true);
        assert!((x - 10.0).abs() < 1e-9);
        assert!((y - 48.0).abs() < 1e-9);
        assert!((w - 1260.0).abs() < 1e-9);
        assert!((h - 752.0).abs() < 1e-9);

        // 非法 scale（0 / 负 / NaN / Inf）回退 1.0，绝不产生 NaN 或 Inf。
        for bad_scale in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            let (x, y, w, h) = physical_frame_to_layer_rect(
                Frame {
                    x: 0.0,
                    y: 48.0,
                    width: 1280.0,
                    height: 752.0,
                },
                bad_scale,
                800.0,
                false,
            );
            assert!(x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite());
            assert!((y - 0.0).abs() < 1e-9);
            assert!((w - 1280.0).abs() < 1e-9);
            assert!((h - 752.0).abs() < 1e-9);
        }

        // super_height 为 0（尚未完成首轮布局）时不产出负偏移。
        let (_, y, _, _) = physical_frame_to_layer_rect(frame_2x, 2.0, 0.0, false);
        assert!((y - 0.0).abs() < 1e-9);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn non_main_thread_calls_are_rejected_before_touching_pointers() {
        let frame = Frame {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let handle = std::thread::spawn(move || {
            // SAFETY: 非主线程分支在校验 `MainThreadMarker::new()` 时即提前返回，绝不解引用指针。
            let attach_err =
                unsafe { attach_layer(std::ptr::null_mut(), std::ptr::null_mut(), frame) };
            assert!(
                attach_err.unwrap_err().contains("必须在主线程调用"),
                "非主线程 attach_layer 必须被拦下"
            );

            // SAFETY: 同上，非主线程提前返回，不解引用指针。
            let sync_err = unsafe { sync_layer_frame(std::ptr::null_mut(), frame) };
            assert!(
                sync_err.unwrap_err().contains("必须在主线程调用"),
                "非主线程 sync_layer_frame 必须被拦下"
            );

            // SAFETY: 非主线程安全 no-op，不解引用指针。
            unsafe { detach_layer(std::ptr::null_mut()) };

            let dock_err = install_dock_menu_handler(Arc::new(|_| {}));
            assert!(
                dock_err.unwrap_err().contains("必须在主线程调用"),
                "非主线程 install_dock_menu_handler 必须被拦下"
            );
        });
        handle.join().expect("worker thread should not panic");
    }

    #[test]
    fn update_dock_menu_devices_stores_snapshot_cleanly() {
        update_dock_menu_devices(vec![DockDeviceItem {
            id: "dev-1".to_string(),
            name: "Studio Mac".to_string(),
            target: "604928235".to_string(),
            display_label: "显示屏1 (Built-in Retina Display)".to_string(),
            online: true,
            requires_password: true,
        }]);
        let snapshot = dock_devices_slot().lock().unwrap().clone();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].name, "Studio Mac");
        assert_eq!(snapshot[0].target, "604928235");
        assert!(snapshot[0].online);
        assert!(snapshot[0].requires_password);
        update_dock_menu_devices(Vec::new());
    }
}
