//! 隐私幕布：M1 Gate 真机终验的标准测试内容源（harness = false）。
//!
//! # 为什么存在
//! 真机全链 gate（`rc-cli loopback --backend real`）采集的是整块屏幕：
//! 没有受控内容就没有可复现的帧率供给，而且会拍到用户桌面（隐私）。
//! 本工具铺满主屏，提供两种内容：
//! - `anim`（默认）：纯黑底 + 移动灰块 + 帧计数块，逐帧变化（低熵 →
//!   小 NAL，编码段测的是管线而不是内容码率），60fps 节奏呈现；
//! - `static`：纯黑静止，零像素变化（静态画面 gate 的真静屏）。
//!
//! 幕布进程独立于被测管线：先启动本进程，再跑 `rc-cli loopback
//! --backend real`，测完 kill 本进程（SIGTERM 即可，窗口随进程回收）。
//!
//! # 架构（坑：光 orderFrontRegardless 窗口不会上屏）
//! 主线程：`NSApplication`（Accessory 策略，不抢 Dock）+ 建窗口 +
//! `makeKeyAndOrderFront` + `NSApp.run()` 常驻跑事件循环——没有 run
//! loop 时窗口服务不处理上屏请求（实测 `orderFrontRegardless` 后
//! `screencapture` 仍拍出桌面）。渲染线程：Metal 渲染器 / NV12 帧源 /
//! 60fps 混合节拍全部在工作线程内创建与驱动（Metal/CoreVideo 允许
//! 非主线程；不跨线程传任何 AppKit 对象）。
//!
//! # 运行
//! ```bash
//! cargo test -p rc-render-macos --features window-bench --test curtain \
//!     --release -- --mode anim --fps 60
//! # 选项：--mode static | --fps <N>；进程常驻直到被 kill。
//! ```
//!
//! # 与 window_present 的关系
//! 同一套 `window-bench` 基础设施（NSWindow 是 MainThreadOnly，
//! harness = false 让 `fn main` 落在主线程）。本文件无 unsafe。

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
fn main() {
    let mut mode = "anim".to_string();
    let mut fps: u32 = 60;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mode" => mode = args.next().unwrap_or_else(|| "anim".into()),
            "--fps" => {
                fps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(60)
                    .clamp(1, 240);
            }
            other => {
                eprintln!("curtain: 未知参数 {other}（支持 --mode anim|static / --fps N）");
                std::process::exit(2);
            }
        }
    }
    if mode != "anim" && mode != "static" {
        eprintln!("curtain: 未知 --mode {mode}（anim | static）");
        std::process::exit(2);
    }

    // NSWindow / NSApplication 是 MainThreadOnly：harness=false 的 main
    // 就在主线程。
    let mtm = objc2::MainThreadMarker::new().expect("main 必须在主线程");
    let (win, size) = window_bench::fullscreen_window_with_metal_layer(mtm)
        .expect("创建全屏幕布窗口失败（需要 GUI 会话）");

    {
        use objc2_app_kit::NSApplication;
        let app = NSApplication::sharedApplication(mtm);
        // 激活自己把幕布窗口带到前台（方法在 macOS 14+ 标记弃用但可用；
        // 新 API activate(activatingWindows) 需要更高版本 feature 面）。
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    // 渲染线程：layer 裸指针 + 尺寸传过去（值传递，无 AppKit 对象跨线程）。
    // 窗口由 main 持有且随 NSApp.run 常驻，指针存活期覆盖渲染全程。
    let layer_ptr = win.layer_ptr() as usize;
    let render = std::thread::Builder::new()
        .name("rc-curtain-render".into())
        .spawn(move || render_loop(layer_ptr as *mut _, size, &mode, fps))
        .expect("spawn render thread");

    // NSApp.run 常驻处理事件（窗口上屏的前提）；渲染线程独立节拍。
    // 进程生命周期 = 外层 kill（gate 脚本负责）。
    {
        use objc2_app_kit::NSApplication;
        NSApplication::sharedApplication(mtm).run();
    }
    // run 返回（正常不该发生）：等渲染线程一并退出。
    let _ = render.join();
}

/// 渲染线程主体：Metal 渲染器 + NV12 帧源环 + 60fps 混合节拍。
/// Metal / CoreVideo 允许非主线程使用；全部对象在本线程创建与消费。
#[cfg(target_os = "macos")]
fn render_loop(layer_ptr: *mut std::ffi::c_void, size: (u32, u32), mode: &str, fps: u32) {
    let mut r = MetalRenderer::from_raw_layer(
        layer_ptr,
        MetalRendererConfig {
            size,
            ..MetalRendererConfig::default()
        },
    )
    .expect("渲染器创建失败");
    assert!(!r.vsync(), "幕布关 vsync（与生产呈现形态一致）");

    let make_frame = |src: &window_bench::BenchFrameSource| {
        let (w, h) = src.size();
        rc_decode::DecodedFrame {
            texture: rc_capture::FrameTexture::CVPixelBuffer(src.ptr()),
            decode_done_ts: rc_types::Timestamp(0),
            width: w,
            height: h,
        }
    };

    if mode == "static" {
        let src = window_bench::BenchFrameSource::nv12(size.0, size.1).expect("NV12 帧源失败");
        src.paint_black().expect("静止帧绘制失败");
        r.present(make_frame(&src)).expect("present 失败");
        eprintln!(
            "curtain static: 纯黑静止已提交（{}x{}），常驻直到被 kill",
            size.0, size.1
        );
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    // anim：帧源用 4 缓冲环，逐帧换 IOSurface（单缓冲反复重写时合成器
    // 只看到冻结画面，SCK 实测 ~3-8fps 系统级噪声）。环大小 ≈ drawable 池深。
    let ring: Vec<_> = (0..4)
        .map(|_| window_bench::BenchFrameSource::nv12(size.0, size.1).expect("NV12 帧源失败"))
        .collect();
    // 预热：窗口映射 + drawable 池 + UV 初始化。
    ring[0].paint_test_pattern(0).expect("首帧绘制失败");
    for src in &ring {
        r.present(make_frame(src)).expect("预热 present 失败");
    }
    for _ in 0..26 {
        r.present(make_frame(&ring[0])).expect("预热 present 失败");
    }
    let tick = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let start = Instant::now();
    let mut frame: u64 = 1;
    loop {
        wait_until(start + tick * frame as u32);
        let src = &ring[(frame % ring.len() as u64) as usize];
        src.paint_test_pattern(frame).expect("绘制失败");
        r.present(make_frame(src)).expect("present 失败");
        if frame % (u64::from(fps) * 10) == 0 {
            eprintln!(
                "curtain anim: {frame} 帧 / {:.1}s（{}x{} @ {fps}fps）",
                start.elapsed().as_secs_f64(),
                size.0,
                size.1
            );
        }
        frame += 1;
    }
}

/// 睡到目标时刻：先 sleep 到差 2ms，最后 2ms 自旋——纯 sleep 的超调
/// 会把 60fps 节拍拖到 ~46fps（macOS 定时器 slack 随睡眠时长放大）。
#[cfg(target_os = "macos")]
fn wait_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let remain = t.saturating_duration_since(now);
        if remain > Duration::from_millis(2) {
            std::thread::sleep(remain - Duration::from_millis(2));
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("curtain 仅限 macOS");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
use rc_render::Renderer;
#[cfg(target_os = "macos")]
use rc_render_macos::{window_bench, MetalRenderer, MetalRendererConfig};
