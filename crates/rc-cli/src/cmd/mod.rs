//! 子命令定义、值解析函数与派发。

pub mod client;
pub mod host;
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
        Command::Host(args) => host::run(&args),
        Command::Client(args) => client::run(&args),
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
    /// 被控端：accept 一条 QUIC 连接并流媒体 + 注入输入（T18）
    Host(HostArgs),
    /// 控制端：connect 被控端，跑完整会话并出报告（Gate 通过 exit 0）
    Client(ClientArgs),
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

/// `host` 子命令参数（被控端，T18）。
#[derive(Args)]
pub struct HostArgs {
    /// 监听地址。--duration 是「接受连接后的会话时长」：accept 等待期不消耗
    ///（accept 等待上限 300s，超时报错 exit 1）。
    #[arg(long, default_value = "127.0.0.1:9901")]
    pub listen: std::net::SocketAddr,
    /// 后端模式：fake / auto / real（复用 parse_backend）。real 下
    /// host = SCK + VideoToolbox；任一降级即报错 exit 1。
    #[arg(long, default_value = "fake", value_parser = parse_backend)]
    pub backend: BackendMode,
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    /// 目标码率（Hello 请求值；host 按编码器能力夹紧后经 Welcome 裁定）。
    #[arg(long, default_value_t = 8_000_000)]
    pub bitrate_bps: u32,
    /// 会话时长兜底（client 正常关闭发 Bye；到时未收到则自行优雅收尾）。
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub duration: Duration,
    /// 静止画面（铁律 2：静止零编码）。fake 后端下构造 FakeContent::Static
    /// 栈；client 侧将观测到零媒体数据报。
    #[arg(long)]
    pub static_content: bool,
    /// 注入模式：record = 记录不真注入（默认，免权限）；real = CGEvent
    /// 真注入（需辅助功能权限，仅 macOS）。
    #[arg(long, default_value = "record", value_parser = parse_inject_mode)]
    pub inject_mode: InjectMode,
    /// 服务端证书 / 私钥 PEM；默认内嵌开发证书（仅开发用，S3）。
    #[arg(long)]
    pub cert: Option<PathBuf>,
    #[arg(long)]
    pub key: Option<PathBuf>,
    /// host 侧摘要 JSON 输出路径；省略或 `-` 表示 stdout（无 gate 段）。
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `client` 子命令参数（控制端，T18）。
#[derive(Args)]
pub struct ClientArgs {
    /// 对端地址（必填）。
    #[arg(long)]
    pub connect: std::net::SocketAddr,
    /// 后端模式：fake / auto / real。real 下 client = VideoToolbox 解码 +
    /// Metal 呈现（会弹真窗口）。
    #[arg(long, default_value = "fake", value_parser = parse_backend)]
    pub backend: BackendMode,
    /// 请求参数（Hello 携带；实际以 Welcome 裁定值为准）。
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    #[arg(long, default_value_t = 60)]
    pub fps: u32,
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    pub duration: Duration,
    /// 输入模式：scripted = 脚本回放（默认，持续产出输入流量供 InputAck
    /// 采样）；none = 不发输入；real = CgCapture 本机捕获（需输入监控
    /// 权限，仅 macOS）。
    #[arg(long, default_value = "scripted", value_parser = parse_input_mode)]
    pub input_mode: InputMode,
    /// 到点主动断开传输层，触发自动退避重连（重连演练，T17 机制）。
    #[arg(long, value_parser = parse_duration)]
    pub reconnect_at: Option<Duration>,
    /// 固定信任的服务端证书 PEM；默认内嵌开发证书公钥。
    #[arg(long)]
    pub server_cert: Option<PathBuf>,
    /// 证书 SAN 名（须与证书匹配）。
    #[arg(long, default_value = rc_session::dev_cert::DEV_SERVER_NAME)]
    pub server_name: String,
    /// 报告 JSON 输出路径；省略或 `-` 表示 stdout（含 gate 段）。
    #[arg(long)]
    pub report: Option<PathBuf>,
}

/// `host --inject-mode` 取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InjectMode {
    /// 记录不真注入（localhost 实测默认，免辅助功能权限）。
    Record,
    /// CGEvent 真注入（仅 macOS，需辅助功能权限）。
    Real,
}

/// 解析 `host --inject-mode`。
pub fn parse_inject_mode(s: &str) -> Result<InjectMode, String> {
    match s {
        "record" => Ok(InjectMode::Record),
        "real" => Ok(InjectMode::Real),
        other => Err(format!(
            "无效的注入模式 {other:?}：应为 record / real 之一（real 仅 macOS 且需权限）"
        )),
    }
}

/// `client --input-mode` 取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputMode {
    /// 不发输入。
    None,
    /// 脚本回放（默认）。
    Scripted,
    /// CgCapture 本机捕获（仅 macOS，需输入监控权限）。
    Real,
}

/// 解析 `client --input-mode`。
pub fn parse_input_mode(s: &str) -> Result<InputMode, String> {
    match s {
        "none" => Ok(InputMode::None),
        "scripted" => Ok(InputMode::Scripted),
        "real" => Ok(InputMode::Real),
        other => Err(format!(
            "无效的输入模式 {other:?}：应为 none / scripted / real 之一（real 仅 macOS 且需权限）"
        )),
    }
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
