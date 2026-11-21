//! 子命令定义、值解析函数与派发。

pub mod loopback;
pub mod probe;
pub mod version;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use rc_types::Resolution;

/// 子命令执行结果：`Err` 的消息由 `main` 打印后 exit 1。
pub type CliResult<T> = Result<T, String>;

/// 派发子命令。
pub fn dispatch(command: Command) -> CliResult<ExitCode> {
    match command {
        Command::Loopback(args) => loopback::run(&args),
        Command::Probe(args) => probe::run(&args),
        Command::Version => version::run(),
    }
}

/// CLI 根定义。`command` 为 `None`（无参数）时 main 打印用法并 exit 0。
#[derive(Parser)]
#[command(name = "rc-cli", version, about = "RabbitControl 诊断工具")]
pub struct Cli {
    /// 子命令
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 跑一次回环假管线并输出诊断报告（Gate 通过 exit 0，失败 exit 1）
    Loopback(LoopbackArgs),
    /// 探测采集/编码/解码后端可用性（当前为假后端）
    Probe(ProbeArgs),
    /// 打印版本、git commit 与编译时间
    Version,
}

/// `loopback` 子命令参数（字段与 `rc_loopback::LoopbackConfig` 一一对应）。
#[derive(Args)]
pub struct LoopbackArgs {
    /// 分辨率，格式 WxH
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    /// 目标帧率
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// 测量时长，格式 Ns / Nms
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub duration: Duration,
    /// 注入丢包率（0.0–1.0）
    #[arg(long, default_value_t = 0.0)]
    pub inject_loss: f32,
    /// 注入往返延迟（微秒）
    #[arg(long, default_value_t = 0)]
    pub inject_rtt_us: u32,
    /// 注入抖动（微秒）
    #[arg(long, default_value_t = 0)]
    pub inject_jitter_us: u32,
    /// 带宽上限（bps），0 = 不限
    #[arg(long, default_value_t = 0)]
    pub bandwidth_bps: u64,
    /// 编码器减速倍数（1.0 = 正常）
    #[arg(long, default_value_t = 1.0)]
    pub encoder_slowdown: f32,
    /// 静止画面（验证 Q5：零编码）
    #[arg(long)]
    pub static_content: bool,
    /// 丢包序列随机种子
    #[arg(long, default_value_t = 42)]
    pub seed: u64,
    /// 后端模式：fake = 逻辑时钟假管线（确定性，T02 原路径）；
    /// auto = 真实后端优先、失败降级假后端（T08 M1 Gate 用）；
    /// real = 强制真实后端，任一降级即报错 exit 1
    #[arg(long, default_value = "fake", value_parser = parse_backend)]
    pub backend: BackendMode,
    /// 报告 JSON 输出路径；省略或 `-` 表示 stdout
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `loopback --backend` 的取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackendMode {
    /// 逻辑时钟假管线（`Loopback::run`，确定性）。
    Fake,
    /// 真实后端优先，失败降级假后端（标签打到 stderr，不静默）。
    Auto,
    /// 强制真实后端；任一部件降级即失败。
    Real,
}

/// 解析 `--backend`。
pub fn parse_backend(s: &str) -> Result<BackendMode, String> {
    match s {
        "fake" => Ok(BackendMode::Fake),
        "auto" => Ok(BackendMode::Auto),
        "real" => Ok(BackendMode::Real),
        other => Err(format!(
            "无效的后端 {other:?}：应为 fake / auto / real 之一"
        )),
    }
}

/// `probe` 子命令参数。
#[derive(Args)]
pub struct ProbeArgs {
    /// 以 JSON 输出（机器可读）
    #[arg(long)]
    pub json: bool,
}

/// 解析 `WxH` 分辨率。拒绝非数字、缺 `x`、多段、零尺寸。
pub fn parse_resolution(s: &str) -> Result<Resolution, String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("无效的分辨率 {s:?}：应为 WxH（如 1920x1080）"))?;
    let bad = |part: &str| format!("无效的分辨率 {s:?}：{part:?} 不是合法的非负整数");
    let width = w.parse::<u32>().map_err(|_| bad(w))?;
    let height = h.parse::<u32>().map_err(|_| bad(h))?;
    if width == 0 || height == 0 {
        return Err(format!("无效的分辨率 {s:?}：宽与高都必须大于 0"));
    }
    Ok(Resolution::new(width, height))
}

/// 解析 `Ns` / `Nms` 时长。不支持裸数字与其他单位（如 `1m`、`-1s`）。
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("无效的时长 {s:?}：仅支持 Ns / Nms（如 30s、500ms），且不能为负");
    // "500ms" 也以 's' 结尾，必须先判 "ms"。
    let (digits, mult) = if let Some(digits) = s.strip_suffix("ms") {
        (digits, 1u64)
    } else if let Some(digits) = s.strip_suffix('s') {
        (digits, 1_000)
    } else {
        return Err(bad());
    };
    // u64 解析天然拒绝负号与空串。
    let value = digits.parse::<u64>().map_err(|_| bad())?;
    Ok(Duration::from_millis(value * mult))
}

#[cfg(test)]
mod tests {
    // 纯解析函数的单元测试。跑子进程的 CLI 测试在 tests/cli.rs：assert_cmd
    // 依赖的 `CARGO_BIN_EXE_rc-cli` 环境变量只对集成测试注入。
    use super::{parse_duration, parse_resolution};
    use std::time::Duration;

    #[test]
    fn parse_resolution_accepts_wxh() {
        let r = parse_resolution("1920x1080").expect("1920x1080 必须合法");
        assert_eq!((r.width, r.height), (1920, 1080));
        assert_eq!(
            (parse_resolution("64x64").expect("64x64 必须合法").width),
            64
        );
    }

    #[test]
    fn parse_resolution_rejects_invalid() {
        for bad in [
            "abc",
            "0x0",
            "1920",
            "x1080",
            "1920x",
            "1920x1080x2",
            "-1x100",
        ] {
            assert!(
                parse_resolution(bad).is_err(),
                "{bad:?} 必须被拒绝，实际 {:?}",
                parse_resolution(bad)
            );
        }
    }

    #[test]
    fn parse_duration_accepts_s_and_ms() {
        assert_eq!(
            parse_duration("60s").expect("60s 必须合法"),
            Duration::from_secs(60)
        );
        assert_eq!(
            parse_duration("500ms").expect("500ms 必须合法"),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_duration("3s").expect("3s 必须合法"),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn parse_duration_rejects_negative() {
        for bad in ["-1s", "-100ms", "1m", "500", "s", "ms", "1.5s"] {
            assert!(
                parse_duration(bad).is_err(),
                "{bad:?} 必须被拒绝，实际 {:?}",
                parse_duration(bad)
            );
        }
    }
}
