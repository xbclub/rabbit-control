//! 真实窗口基准（`window-bench` feature，harness = false）。
//!
//! # 为什么 harness = false
//! NSWindow / NSView 是 MainThreadOnly 类型（objc2 绑定用 MainThreadMarker
//! 编码该契约），而 libtest 的测试体**永远**运行在工作线程上（实测即使
//! `--test-threads=1` 也不在主线程）。关掉 harness 后本文件的 `fn main`
//! 就在主线程上，可以合法创建 NSWindow。窗口创建的 unsafe 全部在
//! [`rc_render_macos::window_bench`]（ffi 边界），本文件无 unsafe。
//!
//! # 运行（需要 GUI 会话 + `--release`）
//! ```bash
//! cargo test -p rc-render-macos --features window-bench \
//!     --test window_present --release -- --run
//! ```
//! 不带 `--run` 时本目标跳过并退出 0（语义等价 `#[ignore]`：默认的
//! `cargo test --workspace` 既不会编译本目标（required-features），也无
//! 窗口环境也能过）。
//!
//! # 断言（任务卡 T05 验收的补口）
//! 1. **完整 present 路径 P50 ≤ 2ms**（真实窗口、vsync 关、60fps 节奏）：
//!    离屏测试 `present_with_vsync_off_under_2ms` 只断言「drawable 预取后
//!    的调用成本」段（离屏 layer 的 drawable 回收按合成器节奏，全路径
//!    P50≈8.6ms），挂窗口后的完整路径（含 nextDrawable）在这里断言；
//! 2. **60fps × 60 秒无掉帧**：rc-trace `unfinished == 0`、全部上屏、
//!    每帧 present 都在自己的 16.67ms 时隙内完成。

use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
fn main() {
    if !std::env::args().any(|a| a == "--run") {
        eprintln!(
            "跳过：真实窗口基准需要显式 --run（运行方式见本文件文档；\
             需要 GUI 会话与 --features window-bench）"
        );
        return;
    }
    // harness = false：本函数运行在主线程（MainThreadOnly 契约的前提）。
    let mtm = objc2::MainThreadMarker::new().expect("harness=false 的 main 必须在主线程");
    full_present_path_with_real_window_under_2ms(mtm);
    present_60fps_60s_no_dropped_frames_trace_unfinished_zero(mtm);
    eprintln!("全部真实窗口基准通过。");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("window_present 基准仅限 macOS");
    std::process::exit(2);
}

/// 睡到目标时刻：先 sleep 到差 2ms，最后 2ms 自旋——纯 sleep 的超调
/// （可达 1-2ms）会污染 60fps 节拍。
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

/// 任务卡 T05 验收：「`Present` 调用 P50 ≤ 2ms（vsync off）」的**完整路径**
/// 版本——经 [`Renderer::present`]（wrap → nextDrawable → 编码 →
/// presentDrawable → commit），真实窗口上 60fps 节奏驱动。
#[cfg(target_os = "macos")]
fn full_present_path_with_real_window_under_2ms(mtm: objc2::MainThreadMarker) {
    eprintln!("== 完整 present 路径（真实窗口 / vsync 关 / 60fps 节奏 / 含 nextDrawable）==");
    let size = (960, 540);
    let win = window_bench::window_with_metal_layer(mtm, size)
        .expect("创建真实窗口失败（需要 GUI 会话；headless 环境不能跑本基准）");
    let mut r = MetalRenderer::from_raw_layer(
        win.layer_ptr(),
        MetalRendererConfig {
            size,
            ..MetalRendererConfig::default()
        },
    )
    .expect("adopt layer 失败");
    assert!(!r.vsync(), "vsync 必须关（D2）");

    let src = window_bench::BenchFrameSource::nv12(size.0, size.1).expect("NV12 帧源创建失败");
    let make_frame = || {
        let (w, h) = src.size();
        DecodedFrame {
            texture: FrameTexture::CVPixelBuffer(src.ptr()),
            decode_done_ts: Timestamp(0),
            width: w,
            height: h,
        }
    };

    // 预热 30 帧（窗口映射 + drawable 池），不计时。
    for _ in 0..30 {
        r.present(make_frame()).expect("预热 present 失败");
    }

    const N: usize = 300;
    const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);
    let start = Instant::now();
    let mut call_ms = Vec::with_capacity(N);
    for i in 0..N {
        wait_until(start + TICK * i as u32);
        let t0 = Instant::now();
        r.present(make_frame()).expect("计时 present 失败");
        call_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    call_ms.sort_by(|a, b| a.partial_cmp(b).expect("无 NaN"));
    let p50 = call_ms[N / 2];
    let p90 = call_ms[(N * 90) / 100];
    let p99 = call_ms[(N * 99) / 100];
    let max = call_ms[N - 1];
    eprintln!(
        "完整 present 路径 {N} 帧 @60fps: P50={p50:.3}ms P90={p90:.3}ms \
         P99={p99:.3}ms max={max:.3}ms"
    );
    assert!(
        p50 <= 2.0,
        "完整 present 路径（真实窗口、含 nextDrawable）P50={p50:.3}ms 超过 2ms 呈现预算（任务卡 T05 验收）"
    );
    // NSWindow 的 dealloc 按约定留在主线程——泄漏给进程退出。
    win.leak();
}

/// 任务卡 T05 验收：「呈现输出 60fps 连续 60 秒无掉帧（rc-trace
/// unfinished == 0）」。60fps 节奏 × 60s = 3600 帧，每帧打 Decoded /
/// Presented 标记并 finish(Displayed)；掉帧 = present 调用没在自己的
/// 16.67ms 时隙内完成。
#[cfg(target_os = "macos")]
fn present_60fps_60s_no_dropped_frames_trace_unfinished_zero(mtm: objc2::MainThreadMarker) {
    eprintln!("== 60fps × 60s soak（rc-trace unfinished == 0）==");
    let size = (1920, 1080);
    let win = window_bench::window_with_metal_layer(mtm, size)
        .expect("创建真实窗口失败（需要 GUI 会话）");
    let mut r = MetalRenderer::from_raw_layer(
        win.layer_ptr(),
        MetalRendererConfig {
            size,
            ..MetalRendererConfig::default()
        },
    )
    .expect("adopt layer 失败");
    assert!(!r.vsync(), "vsync 必须关（D2）");

    // 光标叠加常开（生产形态，D7）：16x16 不透明白块。
    let shape = CursorShape::new(1, 16, 16, (0, 0), vec![255; 16 * 16 * 4]).expect("合法形状");
    r.set_cursor_shape(&shape).expect("光标形状上传失败");
    r.set_cursor((100, 100), true);

    let src = window_bench::BenchFrameSource::nv12(size.0, size.1).expect("NV12 帧源创建失败");
    let make_frame = || {
        let (w, h) = src.size();
        DecodedFrame {
            texture: FrameTexture::CVPixelBuffer(src.ptr()),
            decode_done_ts: Timestamp(0),
            width: w,
            height: h,
        }
    };

    // 预热 30 帧（不计入 trace）。
    for _ in 0..30 {
        r.present(make_frame()).expect("预热 present 失败");
    }

    const FPS: u32 = 60;
    const SECS: u32 = 60;
    const TOTAL: u32 = FPS * SECS;
    const TICK: Duration = Duration::from_nanos(1_000_000_000 / 60);
    let mut trace = TraceCollector::new((TOTAL + 64) as usize); // 容量 > 帧数 → evicted == 0
    let start = Instant::now();
    let mut late: u64 = 0;
    let mut durs_ms = Vec::with_capacity(TOTAL as usize);
    for i in 0..TOTAL {
        wait_until(start + TICK * i);
        let t = trace.begin(FrameSeq(i as u64));
        let call_start = Instant::now();
        // 合成帧源：本帧「产生」在时隙起点（Captured），
        // 解码完成的近似 = present 调用入口（Decoded）。
        t.mark(
            Mark::Captured,
            Timestamp::since(r.session_start(), call_start),
        );
        t.mark(
            Mark::Decoded,
            Timestamp::since(r.session_start(), call_start),
        );
        let presented = r.present(make_frame()).expect("soak present 失败");
        let dur = call_start.elapsed();
        // D8 语义：Presented 用 present 返回的时间戳（commit 返回后打点）。
        t.mark(Mark::Presented, presented);
        t.finish(Outcome::Displayed);
        durs_ms.push(dur.as_secs_f64() * 1000.0);
        if dur > TICK {
            late += 1; // 掉帧：present 没在自己的 16.67ms 时隙内完成。
        }
    }
    let wall = start.elapsed();
    durs_ms.sort_by(|a, b| a.partial_cmp(b).expect("无 NaN"));
    let s = trace.summary();
    eprintln!(
        "soak {TOTAL} 帧 @60fps：wall={:.1}s（节拍目标 {:.1}s）present P50={:.3}ms \
         P99={:.3}ms max={:.3}ms 超时隙帧={late}",
        wall.as_secs_f64(),
        SECS,
        durs_ms[durs_ms.len() / 2],
        durs_ms[(durs_ms.len() * 99) / 100],
        durs_ms[durs_ms.len() - 1],
    );
    eprintln!("rc-trace summary: {}", s.to_json());
    assert_eq!(
        s.unfinished, 0,
        "验收：rc-trace unfinished == 0（每帧必须有结局）"
    );
    assert_eq!(s.displayed, TOTAL as u64, "全部帧必须上屏");
    assert_eq!(s.dropped_total(), 0, "无丢弃");
    assert_eq!(
        s.evicted, 0,
        "trace 容量必须覆盖全程（evicted != 0 则统计不完整）"
    );
    assert_eq!(
        late, 0,
        "无掉帧：每个 60fps 时隙内的 present 都必须完成（{late} 帧超出 16.67ms 时隙）"
    );
    assert_eq!(
        r.present_count(),
        (TOTAL + 30) as u64,
        "预热 30 帧 + soak 3600 帧"
    );
    win.leak();
}

#[cfg(target_os = "macos")]
use rc_capture::FrameTexture;
#[cfg(target_os = "macos")]
use rc_decode::DecodedFrame;
#[cfg(target_os = "macos")]
use rc_render::cursor::CursorShape;
#[cfg(target_os = "macos")]
use rc_render::Renderer;
#[cfg(target_os = "macos")]
use rc_render_macos::{window_bench, MetalRenderer, MetalRendererConfig};
#[cfg(target_os = "macos")]
use rc_trace::{Mark, Outcome, TraceCollector};
#[cfg(target_os = "macos")]
use rc_types::{FrameSeq, Timestamp};
