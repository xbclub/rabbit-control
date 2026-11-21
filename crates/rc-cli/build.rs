//! 编译期信息注入：版本号、git commit、编译时间（供 `rc-cli version` 使用）。
//!
//! 拿不到的项一律注入 `"unknown"`，绝不让构建失败——版本信息是诊断辅助，
//! 不值得为它挡住编译（例如从 tarball 或无 git 环境构建）。

use std::process::Command;

fn main() {
    // build.rs 自身变化才重新执行；默认行为下 cargo 不会因为 .git 变化重建。
    println!("cargo:rerun-if-changed=build.rs");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());
    println!("cargo:rustc-env=RC_CLI_VERSION={version}");

    let commit = run_capture("git", &["rev-parse", "--short", "HEAD"]);
    println!("cargo:rustc-env=RC_CLI_GIT_COMMIT={commit}");

    let build_time = run_capture("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]);
    println!("cargo:rustc-env=RC_CLI_BUILD_TIME={build_time}");
}

/// 运行命令取首行 stdout；命令不存在或失败时返回 `"unknown"`。
fn run_capture(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
