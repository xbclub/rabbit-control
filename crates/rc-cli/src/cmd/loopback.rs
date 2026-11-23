//! `rc-cli loopback`：跑一次回环假管线，输出诊断报告。
//!
//! 流向约定（B4）：进度与人类可读摘要只进 stderr；报告 JSON 只进
//! `--report` 文件或 stdout，两者永不混流。

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use rc_loopback::{Loopback, LoopbackConfig};

use super::{CliResult, LoopbackArgs};

/// 执行 `loopback` 子命令。Gate 通过返回 SUCCESS，未通过返回 FAILURE。
pub fn run(args: &LoopbackArgs) -> CliResult<ExitCode> {
    #[cfg(target_os = "macos")]
    let _activity = if matches!(args.backend, super::BackendMode::Real) {
        rc_capture_macos::promote_current_thread_qos();
        Some(rc_capture_macos::LatencyCriticalActivity::begin(
            "RabbitControl loopback",
            true,
        ))
    } else {
        None
    };

    let cfg = LoopbackConfig {
        resolution: args.resolution,
        fps: args.fps,
        inject_loss: args.inject_loss,
        inject_rtt_us: args.inject_rtt_us,
        inject_jitter_us: args.inject_jitter_us,
        bandwidth_bps: args.bandwidth_bps,
        encoder_slowdown: args.encoder_slowdown,
        static_content: args.static_content,
        seed: args.seed,
        // CLI 不暴露这两个旋钮：trace 容量与队列上限走默认值（4096 / 2），
        // 队列上限 2 是 Q3 的硬约束，不该被命令行随手调大。
        ..LoopbackConfig::default()
    };

    let width = args.resolution.width;
    let height = args.resolution.height;
    let seconds = args.duration.as_secs_f64();
    let content = if args.static_content {
        "静态"
    } else {
        "动态"
    };
    let tty = std::io::stderr().is_terminal();

    // 进度只进 stderr；非 TTY（CI、管道）时不用 `\r` 覆盖，避免日志交错。
    if tty {
        eprint!(
            "\rloopback 运行中：{width}x{height} @ {}fps（{content}内容，{seconds:.1}s）…",
            args.fps
        );
        let _ = std::io::stderr().flush();
    } else {
        eprintln!(
            "loopback 开始：{width}x{height} @ {}fps（{content}内容，{seconds:.1}s）",
            args.fps
        );
    }

    let mut loopback = Loopback::new(cfg).map_err(|e| format!("回环配置非法：{e}"))?;
    let report = match args.backend {
        super::BackendMode::Fake => loopback
            .run(args.duration)
            .map_err(|e| format!("回环运行失败：{e}"))?,
        super::BackendMode::Auto => loopback
            .run_real(args.duration, true, false)
            .map_err(|e| format!("真实管线运行失败：{e}"))?,
        super::BackendMode::Real => loopback
            .run_real(args.duration, true, true)
            .map_err(|e| format!("真实管线运行失败：{e}"))?,
    };

    // 结束“运行中”那一行，再打印摘要（无论 JSON 去向如何，摘要总在 stderr）。
    if tty {
        eprint!("\r\n");
        let _ = std::io::stderr().flush();
    }
    let convergence = report
        .convergence_ms
        .map_or("null".to_string(), |ms| format!("{ms}ms"));
    eprintln!(
        "loopback 完成：Gate {}（p50 {:.1}ms，displayed {:.1}fps，convergence {convergence}，队列深度峰值 {}）",
        if report.gate.passed { "通过" } else { "未通过" },
        report.latency_p50_ms,
        report.displayed_fps,
        report.max_queue_depth_seen,
    );
    for failure in &report.gate.failures {
        eprintln!("  失败项：{failure}");
    }

    write_report(args, &report.to_json())?;

    if report.gate.passed {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

/// 报告输出：`--report <path>` 写文件（不带 ANSI 控制字符），`--report -`
/// 或省略写 stdout（约定 1）。
fn write_report(args: &LoopbackArgs, json: &str) -> CliResult<()> {
    match args.report.as_deref() {
        Some(path) if path != Path::new("-") => fs::write(path, format!("{json}\n"))
            .map_err(|e| format!("无法写报告文件 {}：{e}", path.display())),
        _ => {
            println!("{json}");
            Ok(())
        }
    }
}
