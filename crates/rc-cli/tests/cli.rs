//! CLI 端到端测试（assert_cmd + predicates）。
//!
//! 放在 `tests/`（集成测试）而不是 `src/` 里的原因：assert_cmd 通过
//! `CARGO_BIN_EXE_rc-cli` 定位二进制，该环境变量只对集成测试注入。
//! 测试名与任务卡 T07 的十项清单一一对应（前 4 项纯解析测试在
//! `src/cmd/mod.rs`，此处覆盖其余 6 项子进程行为）。

use assert_cmd::Command;
use predicates::prelude::*;

/// Gate 通过路径：动态内容。
///
/// 任务卡示例在此处用了 `--static-content`，但静态画面 encoded==0、
/// convergence 为 null，Gate 必然失败——那是 T02 的 L8/Q1 红线，不能为
/// 测试放宽。因此 pass 场景改用动态内容，fail 场景才用 `--static-content`。
///
/// `--resolution 64x64` 只为提速（debug 构建下 1920x1080 的仿真每秒
/// 60 帧脏区比对很慢）；Gate 门槛只随 `--fps` 走（target×0.9），分辨率
/// 不参与判定。与 rc-loopback 自身测试的小分辨率先例一致。
#[test]
fn loopback_exits_zero_when_gate_passes() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args(["loopback", "--resolution", "64x64", "--duration", "3s"])
        .assert()
        .success();
}

/// Gate 失败路径：静态画面（encoded=0 → displayed fps 与 convergence 全挂）。
#[test]
fn loopback_exits_one_when_gate_fails() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args([
            "loopback",
            "--resolution",
            "64x64",
            "--static-content",
            "--duration",
            "2s",
        ])
        .assert()
        .failure()
        .code(1);
}

/// B2：`--report` 写出的 JSON 必含 `40-diagnostics.md` §4 的 6 个顶层键。
#[test]
fn report_to_file_matches_schema() {
    let path = std::env::temp_dir().join(format!("rc-t07-report-{}.json", std::process::id()));
    let path_str = path.to_str().expect("临时路径必须是 UTF-8");

    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args([
            "loopback",
            "--resolution",
            "64x64",
            "--static-content",
            "--duration",
            "2s",
            "--report",
            path_str,
        ])
        .assert()
        .failure()
        .code(1);

    let json = std::fs::read_to_string(&path).expect("--report 指定的文件必须被写出");
    for key in [
        "\"config\"",
        "\"latency_ms\"",
        "\"fps\"",
        "\"frames\"",
        "\"breakdown_ms\"",
        "\"gate\"",
    ] {
        assert!(json.contains(key), "报告缺少顶层字段 {key}：{json}");
    }
    // Q5：静态画面零编码（to_json 里 f64 的 0 打印为 `0`）。
    assert!(
        json.contains("\"encoded\":0"),
        "静态画面 encoded 必须为 0：{json}"
    );
    // 静态画面从未达到目标帧率，convergence 必须是 null（不编造数值）。
    assert!(
        json.contains("\"convergence_ms\":null"),
        "静态运行 convergence_ms 必须为 null：{json}"
    );
    // 报告文件只含 JSON，不混入进度输出（B4）。
    assert!(
        !json.contains("loopback 开始") && !json.contains("loopback 完成"),
        "报告文件不得混入 stderr 进度信息：{json}"
    );
    std::fs::remove_file(&path).ok();
}

/// probe --json 的关键字段用字符串断言（不引 serde，与任务卡口径一致）。
#[test]
fn probe_json_is_valid() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args(["probe", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"backend\": \"fake-loopback\""))
        .stdout(predicate::str::contains("\"available\": true"))
        .stdout(predicate::str::contains("\"platform\":"))
        .stdout(predicate::str::contains("\"arch\":"));
}

/// macOS 真实探测：probe --json 报告 SCK 权限明细、VideoToolbox 条目与 fake-loopback 兜底；
/// 人类可读模式同时打印三项权限状态。
#[cfg(target_os = "macos")]
#[test]
fn probe_macos_reports_sck_permissions_and_videotoolbox() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args(["probe", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"backend\": \"sck\""))
        .stdout(predicate::str::contains("\"permissions\":"))
        .stdout(predicate::str::contains("\"screen_recording\":"))
        .stdout(predicate::str::contains("\"accessibility\":"))
        .stdout(predicate::str::contains("\"input_monitoring\":"))
        .stdout(predicate::str::contains("\"backend\": \"videotoolbox\""))
        .stdout(predicate::str::contains("\"hardware\": true"))
        .stdout(predicate::str::contains("\"backend\": \"fake-loopback\""))
        .stdout(predicate::str::contains("\"platform\": \"macos\""));

    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args(["probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("backend=sck"))
        .stdout(predicate::str::contains("screen_recording="))
        .stdout(predicate::str::contains("accessibility="))
        .stdout(predicate::str::contains("input_monitoring="))
        .stdout(predicate::str::contains("backend=videotoolbox"))
        .stdout(predicate::str::contains("backend=fake-loopback"));
}

/// B6：无参数运行打印用法且 exit 0，不是错误。
#[test]
fn no_args_prints_usage_not_error() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"));
}

/// B1/B7：非法参数走 clap 默认行为，exit 2 且打印错误。
#[test]
fn invalid_flag_returns_error_exit_code() {
    Command::cargo_bin("rc-cli")
        .expect("测试必须能定位 rc-cli 二进制")
        .args(["--no-such-flag"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("error"));
}
