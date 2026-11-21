# T07 CLI 入口

> 建立 `rc-cli` 二进制，把回环模式、诊断导出、编码器探测做成可执行命令。

## 背景

没有 CLI，`rc-loopback` 就只能通过单元测试调用，agent 无法「跑一次看报告」。
这是把「测量」变成**可执行动作**的关键一步。

## 平台要求

全平台（用假后端也能跑）。

## 交付物

| 文件 | 动作 |
|---|---|
| `crates/rc-cli/Cargo.toml` | 新建 |
| `crates/rc-cli/src/main.rs` | 新建 |
| `crates/rc-cli/src/cmd/loopback.rs` | 新建 |
| `crates/rc-cli/src/cmd/probe.rs` | 新建（编码器探测） |
| `crates/rc-cli/src/cmd/version.rs` | 新建 |

## API 契约

CLI 的「API」是命令行接口与退出码。实现时按此签名组织：

```rust
// main.rs
fn main() -> std::process::ExitCode {
    // 顶层捕获所有错误，打印友好信息后 exit 1。
    // 绝不 panic（除 clap 的用法错误，其默认 exit 2）。
}

// cmd/mod.rs
#[derive(clap::Parser)]
#[command(name = "rc-cli", version, about = "RabbitControl 诊断工具")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,   // None → 打印用法，exit 0
}

#[derive(clap::Subcommand)]
enum Command {
    Loopback(LoopbackArgs),
    Probe(ProbeArgs),
    Version,
}

#[derive(clap::Args)]
struct LoopbackArgs {
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    resolution: Resolution,
    #[arg(long, default_value_t = 60)]
    fps: u32,
    #[arg(long, default_value = "30s", value_parser = parse_duration)]
    duration: Duration,
    #[arg(long, default_value_t = 0.0)]
    inject_loss: f32,
    #[arg(long, default_value_t = 0)]
    inject_rtt_us: u32,
    #[arg(long, default_value_t = 0)]
    inject_jitter_us: u32,
    #[arg(long, default_value_t = 0)]
    bandwidth_bps: u64,
    #[arg(long, default_value_t = 1.0)]
    encoder_slowdown: f32,
    #[arg(long)]
    static_content: bool,
    #[arg(long, default_value_t = 42)]
    seed: u64,
    #[arg(long)]
    report: Option<PathBuf>,   // None → stdout
}

// 解析函数必须可单测
fn parse_resolution(s: &str) -> Result<Resolution, String>;
fn parse_duration(s: &str) -> Result<Duration, String>;
```

**退出码约定：**

| 情况 | 退出码 |
|---|---|
| 成功 / Gate 通过 | 0 |
| Gate 未通过 | 1 |
| 运行时错误 | 1 |
| 参数解析错误（clap 默认） | 2 |
| 无参数运行（打印用法） | 0 |

## 子命令规格

### 1. `rc-cli loopback`

```bash
rc-cli loopback \
    --resolution 1920x1080 \
    --fps 60 \
    --duration 60s \
    --inject-loss 0.03 \
    --inject-rtt-us 80000 \
    --inject-jitter-us 5000 \
    --bandwidth-bps 5000000 \
    --encoder-slowdown 2.0 \
    --static-content \
    --seed 42 \
    --report /tmp/report.json
```

| 参数 | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--resolution` | `WxH` | `1920x1080` | |
| `--fps` | u32 | 60 | |
| `--duration` | `Ns` / `Nms` | `30s` | |
| `--inject-loss` | f32 0..1 | 0.0 | |
| `--inject-rtt-us` | u32 | 0 | |
| `--inject-jitter-us` | u32 | 0 | |
| `--bandwidth-bps` | u64 | 0（不限） | |
| `--encoder-slowdown` | f32 | 1.0 | |
| `--static-content` | flag | false | |
| `--seed` | u64 | 42 | |
| `--report` | path | stdout | 输出 JSON 路径 |

行为：
- 进度输出到 stderr（`\r` 覆盖一行）
- 报告 JSON 到 `--report` 文件或 stdout
- **退出码：Gate 通过 0，失败 1**
- `--report` 同时打印人类可读摘要到 stderr

### 2. `rc-cli probe`

```bash
rc-cli probe            # 人类可读
rc-cli probe --json     # 机器可读
```

输出：

```json
{
  "capture": { "available": true, "backend": "dxgi" },
  "encoders": [
    { "codec": "h264", "hardware": true, "backend": "nvenc", "available": true },
    { "codec": "h264", "hardware": false, "backend": "openh264", "available": true }
  ],
  "decoders": [
    { "codec": "h264", "hardware": true, "backend": "dxva", "available": true }
  ],
  "platform": "windows",
  "arch": "x86_64"
}
```

用途：**用户在报 bug 时先跑这个**，一眼看出是硬件路径还是软件兜底。

### 3. `rc-cli version`

打印版本 + git commit + 编译时间（用 `build.rs` 注入 `env!`）。

## 必须做到

| # | 要求 |
|---|---|
| B1 | 参数解析用 `clap`（derive），非法值给出明确错误与用法 |
| B2 | `--report` 写出的 JSON 符合 `40-diagnostics.md` §4 的 schema |
| B3 | 退出码可用于 CI 判定 |
| B4 | 进度输出到 stderr，不与 JSON 混 |
| B5 | 所有子命令有 `--help` |
| B6 | 不带参数运行时打印简洁用法（不是错误） |
| B7 | 不 panic（顶层 catch `Result`，打印友好错误后 exit 1） |

## 不要做什么

- ❌ 不要在这里实现真实采集/编码。用 T02 的假后端 + 后续卡的工厂函数。
- ❌ 不要做守护进程/服务模式（那是 M5）。
- ❌ 不要做 UI。
- ❌ 不要打印彩色 ANSI 到 `--report` 文件（只在终端着色）。

## 测试清单

| 测试名 | 验证 |
|---|---|
| `parse_resolution_accepts_wxh` | `1920x1080` → `Resolution` |
| `parse_resolution_rejects_invalid` | `abc` / `0x0` 报错 |
| `parse_duration_accepts_s_and_ms` | `60s` / `500ms` |
| `parse_duration_rejects_negative` | 错误 |
| `loopback_exits_zero_when_gate_passes` | B3 |
| `loopback_exits_one_when_gate_fails` | B3 |
| `report_to_file_matches_schema` | B2 |
| `probe_json_is_valid` | 用字符串断言关键字段 |
| `no_args_prints_usage_not_error` | B6 |
| `invalid_flag_returns_error_exit_code` | B7 |

**CLI 测试用 `assert_cmd` + `predicates`：**

```toml
[dev-dependencies]
assert_cmd = "2"
predicates = "3"
```

```rust
#[test]
fn loopback_exits_zero_when_gate_passes() {
    Command::cargo_bin("rc-cli").unwrap()
        .args(["loopback", "--duration", "1s", "--static-content"])
        .assert()
        .success();
}
```

## 验收命令

```bash
cargo test -p rc-cli
cargo clippy -p rc-cli --all-targets -- -D warnings

# 手动
cargo run -p rc-cli -- --help
cargo run -p rc-cli -- probe --json | jq .
cargo run -p rc-cli -- loopback --duration 3s --report /tmp/r.json
jq '.gate' /tmp/r.json
echo "exit=$?"
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] `--help` 列出全部子命令
- [ ] `probe --json | jq .` 合法
- [ ] `loopback --duration 3s` 能跑完并输出完整报告
- [ ] Gate 通过时 exit 0，失败时 exit 1
- [ ] 无参数运行打印用法，exit 0
- [ ] 非法参数 exit 2（clap 默认）或 1，且打印错误

### 报告 schema 数值校验（可 jq 断言）

```bash
cargo run -p rc-cli -- loopback --static-content --duration 30s --report /tmp/t07.json

# 1. 必含字段（缺一判失败）
jq -e '.config and .latency_ms and .fps and .frames and .breakdown_ms and .gate' /tmp/t07.json

# 2. Q5：静止画面零编码
jq '.fps.encoded' /tmp/t07.json            # 必须 == 0

# 3. Q3：队列深度
jq '.max_queue_depth_seen' /tmp/t07.json   # 必须 <= 2

# 4. Q1/Q4：收敛时间 —— 必须在**动态内容**报告上断言。
#    静态画面无帧上屏，convergence_ms 恒为 null（T02 的 L10/30 秒规则），
#    Gate 必失败 exit 1——这是 Q5 的预期行为，不是缺陷；断言 2 与本断言
#    不可能在同一份 --static-content 报告上同时成立。
cargo run -p rc-cli -- loopback --duration 3s --report /tmp/t07d.json
jq '.convergence_ms' /tmp/t07d.json        # 必须 <= 1000

# 5. 一致性不变式
jq -e '.frames.captured == (.frames.encoded + .frames.dropped_repeat
       + .frames.dropped_stale + .frames.dropped_budget
       + .frames.dropped_superseded)' /tmp/t07.json
```

| 指标 | 门槛 | 来源 |
|---|---|---|
| 报告必含字段 | 6 个顶层键齐全 | `impl/40-diagnostics.md` §4 |
| `fps.encoded`（`--static-content`） | **== 0** | Q5 |
| `max_queue_depth_seen` | **≤2** | Q3 |
| `convergence_ms` | **≤1000**（动态内容报告；静态恒 null） | Q1 / Q4 |
| 一致性不变式 | 成立 | §4.3 |
| 退出码语义 | 0 / 1 / 2 正确 | 本文「退出码约定」 |

### 性能

- [ ] `probe` 在 <500ms 内返回

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | `TraceSummary::to_json` 的 schema |
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 配置校验模式（`SchedulerConfig::validate`） |

## 已知坑

- `clap` 的 `--duration 60s` 需要自定义 parser（`humantime` crate 或手写）
- 退出码：clap 解析错误默认 exit 2，业务失败用 1，不要混
- `--report -` 表示 stdout（约定 1）
- 进度条在非 TTY（如 CI）时不要输出 `\r`，检测 `std::io::stderr().is_terminal()`
