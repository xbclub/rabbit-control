//! rc-cli：RabbitControl 诊断工具入口。
//!
//! 退出码约定（docs/reference/tickets/T07-cli.md）：
//! - Gate 通过 / 成功 → 0
//! - Gate 未通过 / 运行时错误 → 1
//! - 参数解析错误 → 2（clap 默认）
//! - 无参数运行 → 打印用法，0

mod cmd;

use std::process::ExitCode;

use clap::Parser;

use cmd::Cli;

fn main() -> ExitCode {
    install_panic_hook();

    // clap 的用法错误（含 --help / 非法 flag）由 parse 内部按默认行为处理：
    // --help exit 0，非法参数打印错误后 exit 2。业务错误走下面的 Result 路径。
    let cli = Cli::parse();

    let command = match cli.command {
        Some(command) => command,
        // 无参数运行不是错误：打印用法，exit 0（B6）。
        None => return print_usage(),
    };

    match cmd::dispatch(command) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

/// 打印简洁用法并以 0 退出。
fn print_usage() -> ExitCode {
    use clap::CommandFactory;
    let mut command = Cli::command();
    // stdout 不可写时也没必要报错：用法信息本身不是数据契约。
    let _ = command.print_help();
    ExitCode::SUCCESS
}

/// 兜底 panic 钩子：把任何漏网的 panic 转成友好信息后 exit 1（B7）。
///
/// 业务错误全部走 `Result`，这里只防「本不该发生」的编码失误。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("错误：rc-cli 发生未预期的内部异常：{info}");
        eprintln!("这是 bug，请携带上述信息反馈。");
        std::process::exit(1);
    }));
}
