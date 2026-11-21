//! T08 真实模式（假后端 + 真实时钟）冒烟：驱动循环、节流与报告口径。

use rc_loopback::{Loopback, LoopbackConfig};
use rc_types::Resolution;
use std::time::{Duration, Instant};

fn cfg() -> LoopbackConfig {
    LoopbackConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        ..LoopbackConfig::default()
    }
}

/// 真实模式下动态内容应上屏、不变式成立、每帧有结局。
#[test]
fn real_mode_fake_backends_dynamic_run() {
    let mut lb = Loopback::new(cfg()).expect("cfg valid");
    // 2 秒足够形成满窗口（convergence 可判定）且测试仍然很快。
    let t0 = Instant::now();
    let report = lb
        .run_real(Duration::from_secs(2), false, false)
        .expect("run ok");
    let wall = t0.elapsed();
    // 真实时钟驱动：墙钟必须 ≈ 请求时长（节流生效，不空转提前退出）。
    assert!(
        wall >= Duration::from_secs(2),
        "真实模式必须跑满时长，实际 {wall:?}"
    );
    assert!(
        wall < Duration::from_secs(4),
        "假后端节流应维持 ~60fps，2s 运行不应拖到 {wall:?}"
    );
    assert!(report.summary.displayed > 0, "动态内容必须上屏：{report:?}");
    assert_eq!(report.summary.unfinished, 0, "每帧必须有结局");
    assert!(
        report
            .gate
            .failures
            .iter()
            .all(|f| !f.starts_with("invariant")),
        "一致性不变式必须成立：{:?}",
        report.gate.failures
    );
    // L9：队列深度 ≤ 2（Q3 红线在真实模式同样成立）。
    assert!(
        report.max_queue_depth_seen <= 2,
        "Q3：队列深度 {} 超 2",
        report.max_queue_depth_seen
    );
}

/// 静止内容（假后端 Static）：零编码红线在真实模式成立。
#[test]
fn real_mode_fake_backends_static_zero_encode() {
    let mut c = cfg();
    c.static_content = true;
    let mut lb = Loopback::new(c).expect("cfg valid");
    let report = lb
        .run_real(Duration::from_secs(1), false, false)
        .expect("run ok");
    assert_eq!(report.summary.frames, 0, "静止画面不产帧");
    assert_eq!(report.sched_stats.encoded, 0, "L8/Q5：零编码");
    assert_eq!(report.encoded_fps, 0.0);
}

/// strict 模式下假后端必须显式失败（P8 的反面：不静默）。
#[test]
fn real_mode_strict_rejects_fake_backends() {
    let mut lb = Loopback::new(cfg()).expect("cfg valid");
    // prefer_hardware=false → 全假后端 → strict 必须拒绝。
    let err = lb
        .run_real(Duration::from_millis(100), false, true)
        .expect_err("strict + fake 必须失败");
    assert!(
        err.to_string().contains("strict"),
        "错误信息应指明 strict：{err}"
    );
}
