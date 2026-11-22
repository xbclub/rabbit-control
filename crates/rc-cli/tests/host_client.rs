//! 双子进程 host/client 复跑指标（T18）：经 `CARGO_BIN_EXE_rc-cli` spawn
//! 真实进程，走真 QUIC over UDP loopback（CI 免权限：fake 后端 + record
//! 注入）。端口先绑 `127.0.0.1:0` 保留再放行（坑 9：防并行冲突）。

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// rc-cli 二进制路径（集成测试注入）。
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rc-cli")
}

/// 保留一个空闲端口（绑 0 读端口后立刻放行；小竞态可接受，坑 9）。
fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener.local_addr().expect("local addr").port()
}

/// 进程对串行锁：凡 spawn host+client 双进程跑 60fps 实时管线并断言
/// 毫秒级预算（Gate 门槛 / 恢复时长 / 输入单向）的用例，取本锁后执行
/// ——并行时最多 16 个进程抢 CPU，会把时延测成偶发超限（门禁第 2 轮
/// flake：`input one-way p50 5.79ms > rtt/2+5ms`）。门槛本身不动，
/// 只串行化测量环境。
fn pair_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().expect("进程对串行锁中毒")
}

/// spawn 一个子进程（stderr 落临时文件供断言）。
fn spawn(args: &[&str], stderr_path: &str) -> Child {
    let stderr = std::fs::File::create(stderr_path).expect("create stderr file");
    Command::new(bin())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap_or_else(|e| panic!("spawn rc-cli 失败: {e}"))
}

/// 等子进程退出；超时先 kill `child` 与 `also_kill` 里的全部同伴进程，
/// 再返回 `None`（修复卡D-2：不清理会留孤儿 host/client 继续占端口，
/// 拖垮同测试二进制内的后续用例）。
fn wait_or_kill(
    child: &mut Child,
    also_kill: &mut [&mut Child],
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("try wait") {
            Some(status) => return Some(status),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    for other in also_kill {
                        let _ = other.kill();
                    }
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// 等子进程退出（超时 panic 防挂死；超时路径已在 [`wait_or_kill`] 里
/// kill 过本体与同伴，panic 不留孤儿）。`also_kill` 给出应一并清理的
/// 同伴进程（如另一端的 host/client）。
fn wait(
    child: &mut Child,
    also_kill: &mut [&mut Child],
    timeout: Duration,
) -> std::process::ExitStatus {
    wait_or_kill(child, also_kill, timeout).unwrap_or_else(|| {
        panic!("子进程超时未退出（已 kill 本体与同伴，防孤儿占端口）");
    })
}

/// 临时路径（按测试名 + pid 区分）。
fn tmp(name: &str, ext: &str) -> String {
    format!("/tmp/rc-t18-{name}-{}.{ext}", std::process::id())
}

/// host + client 双进程跑一轮。`host_extra` / `client_extra` 需各自给出
/// `--duration`（host 兜底时长应大于 client 时长——正常路径是 client 到时
/// 发 Bye 后 host 提前优雅收尾）。返回 (client 状态, client JSON 文本)。
fn run_pair(
    test_name: &str,
    host_extra: &[&str],
    client_extra: &[&str],
) -> (std::process::ExitStatus, String) {
    let port = reserve_port();
    let connect = format!("127.0.0.1:{port}");

    let mut host = spawn(
        &[
            "host",
            "--listen",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--inject-mode",
            "record",
        ]
        .iter()
        .chain(host_extra.iter())
        .copied()
        .collect::<Vec<_>>(),
        &tmp(test_name, "host.err"),
    );

    // host 起来需要毫秒级（endpoint + 等待连接），client 自带 10s connect
    // 超时——直接 connect 即可。
    let report_path = tmp(test_name, "client.json");
    let mut client = spawn(
        &[
            "client",
            "--connect",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--input-mode",
            "scripted",
            "--report",
            &report_path,
        ]
        .iter()
        .chain(client_extra.iter())
        .copied()
        .collect::<Vec<_>>(),
        &tmp(test_name, "client.err"),
    );

    let client_status = wait(&mut client, &mut [&mut host], Duration::from_secs(30));
    let host_started = Instant::now();
    let host_status = wait(&mut host, &mut [], Duration::from_secs(30));
    // host 必须先于自身兜底时长正常收尾（client 的 Bye 触发优雅关闭）。
    assert!(
        host_status.success(),
        "host 应正常收尾（exit {:?}），stderr: {}",
        host_status.code(),
        std::fs::read_to_string(tmp(test_name, "host.err")).unwrap_or_default()
    );
    assert!(
        host_started.elapsed() < Duration::from_secs(5),
        "host 应在 client 结束后很快收尾（Bye 优先于兜底时长）"
    );
    let json = std::fs::read_to_string(&report_path)
        .unwrap_or_else(|e| panic!("client 报告必须落盘（{e}）"));
    (client_status, json)
}

/// 双进程 fake localhost：client exit 0、JSON 含 session 段与
/// `"passed":true`。
#[test]
fn host_and_client_pass_gate_on_localhost() {
    let _serial = pair_serial();
    let (status, json) = run_pair("pass", &["--duration", "8s"], &["--duration", "4s"]);
    assert_eq!(
        status.code(),
        Some(0),
        "client 应 exit 0，stderr: {}",
        std::fs::read_to_string(tmp("pass", "client.err")).unwrap_or_default()
    );
    assert!(json.contains("\"session\""), "报告缺 session 段：{json}");
    assert!(json.contains("\"passed\":true"), "Gate 应通过：{json}");
    // 报告文件只含 JSON（S1 不混流）。
    assert!(!json.contains("client 完成"), "报告不得混入 stderr：{json}");
    std::fs::remove_file(tmp("pass", "client.json")).ok();
}

/// 修复卡 C-3：displayed_fps 口径 e2e 锁——分母 = 首帧起算的测量窗
///（不含建连死区与停机排空）、分子 = 窗口内上屏帧。fake 动态内容
/// localhost 满帧运行 4s：fps 必须落在 60 的紧邻区间；config 的
/// duration_s 仍是请求值 4.0（请求时长与测量窗长分开记）。
#[test]
fn displayed_fps_uses_first_frame_measurement_window() {
    let _serial = pair_serial();
    let (status, json) = run_pair("fpscale", &["--duration", "8s"], &["--duration", "4s"]);
    assert_eq!(
        status.code(),
        Some(0),
        "满帧运行 Gate 应通过，stderr: {}",
        std::fs::read_to_string(tmp("fpscale", "client.err")).unwrap_or_default()
    );
    assert!(
        json.contains("\"duration_s\":4"),
        "config.duration_s 应仍是请求时长 4s：{json}"
    );
    let seg = json
        .split("\"fps\":{\"displayed\":")
        .nth(1)
        .unwrap_or_else(|| panic!("报告缺 fps.displayed：{json}"));
    let fps: f64 = seg
        .split('}')
        .next()
        .unwrap_or_else(|| panic!("fps.displayed 应为数字：{json}"))
        .parse()
        .unwrap_or_else(|_| panic!("fps.displayed 必须可解析：{json}"));
    assert!(
        (58.0..=62.0).contains(&fps),
        "首帧起算口径下满帧 fps 应 ≈60，实际 {fps}（口径回退会系统性偏低）：{json}"
    );
    std::fs::remove_file(tmp("fpscale", "client.json")).ok();
}

/// host 静止画面（铁律 2：静止零编码）→ client 全程无帧上屏：
/// `connect_to_first_frame_ms:null`、Gate 失败于「无上屏帧」→ exit 1 且
/// 报告仍落盘、`frames.displayed == 0`。不能用短 duration 造失败（500ms
/// 内假管线照样满帧、各项门槛照过）。
#[test]
fn client_exits_one_when_gate_fails() {
    let _serial = pair_serial();
    let (status, json) = run_pair(
        "static",
        &["--duration", "8s", "--static-content"],
        &["--duration", "2s"],
    );
    assert_eq!(
        status.code(),
        Some(1),
        "Gate 未通过应 exit 1，stderr: {}",
        std::fs::read_to_string(tmp("static", "client.err")).unwrap_or_default()
    );
    assert!(
        json.contains("\"connect_to_first_frame_ms\":null"),
        "无帧上屏必须如实报 null：{json}"
    );
    assert!(
        json.contains("\"frames\":{\"displayed\":0"),
        "零上屏：{json}"
    );
    assert!(json.contains("\"passed\":false"), "Gate 必须失败：{json}");
    let stderr = std::fs::read_to_string(tmp("static", "client.err")).unwrap_or_default();
    assert!(
        stderr.contains("无上屏帧"),
        "失败项应写明无上屏帧：{stderr}"
    );
    std::fs::remove_file(tmp("static", "client.json")).ok();
}

/// host 报告含 config/session 键、不含 `"gate"`；stderr 有摘要行。
#[test]
fn host_report_has_no_gate_key() {
    let _serial = pair_serial();
    let port = reserve_port();
    let connect = format!("127.0.0.1:{port}");
    let host_report = tmp("hostrep", "host.json");
    let mut host = spawn(
        &[
            "host",
            "--listen",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "5s",
            "--inject-mode",
            "record",
            "--report",
            &host_report,
        ],
        &tmp("hostrep", "host.err"),
    );
    let mut client = spawn(
        &[
            "client",
            "--connect",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "2s",
            "--input-mode",
            "scripted",
        ],
        &tmp("hostrep", "client.err"),
    );
    let client_status = wait(&mut client, &mut [&mut host], Duration::from_secs(20));
    let host_status = wait(&mut host, &mut [], Duration::from_secs(20));
    assert!(client_status.success());
    assert!(host_status.success());

    let json = std::fs::read_to_string(&host_report).expect("host 报告必须落盘");
    assert!(json.contains("\"config\""), "host 报告缺 config：{json}");
    assert!(json.contains("\"session\""), "host 报告缺 session：{json}");
    assert!(
        json.contains("\"listen\""),
        "host config 应含 listen：{json}"
    );
    assert!(
        !json.contains("\"gate\""),
        "host 报告不得含 gate 键：{json}"
    );
    let stderr = std::fs::read_to_string(tmp("hostrep", "host.err")).unwrap_or_default();
    assert!(stderr.contains("host 完成"), "stderr 应有摘要行：{stderr}");
    assert!(
        stderr.contains("in_flight 峰值"),
        "摘要应含 in_flight 峰值（S5）：{stderr}"
    );
    std::fs::remove_file(&host_report).ok();
}

/// host 报告与 stderr 摘要都带调度器帧去向四数（2026-11-21 localhost
/// 真后端「0 帧编码」排查增补：`HostRunSummary.captured / dropped_stale /
/// dropped_repeat / dropped_budget` 必须贯通到 JSON——下次再出「没帧」
/// 时能立刻分辨是没采到帧还是被调度器丢了）。fake 动态内容 + 2s 会话：
/// captured 必 > 0（有帧进调度器）；dropped_* 只验证键存在与数值合法
///（正常路径应接近 0，不硬断言为 0——调度行为属 rc-sched 的测试域）。
#[test]
fn host_report_exposes_scheduler_frame_stats() {
    let _serial = pair_serial();
    let port = reserve_port();
    let connect = format!("127.0.0.1:{port}");
    let host_report = tmp("sched", "host.json");
    let mut host = spawn(
        &[
            "host",
            "--listen",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "5s",
            "--inject-mode",
            "record",
            "--report",
            &host_report,
        ],
        &tmp("sched", "host.err"),
    );
    let mut client = spawn(
        &[
            "client",
            "--connect",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "2s",
            "--input-mode",
            "scripted",
        ],
        &tmp("sched", "client.err"),
    );
    let client_status = wait(&mut client, &mut [&mut host], Duration::from_secs(20));
    let host_status = wait(&mut host, &mut [], Duration::from_secs(20));
    assert!(client_status.success());
    assert!(host_status.success());

    let json = std::fs::read_to_string(&host_report).expect("host 报告必须落盘");
    // 四键贯通（summary → 报告 JSON）。
    for key in [
        "captured",
        "dropped_stale",
        "dropped_repeat",
        "dropped_budget",
    ] {
        assert!(
            json.contains(&format!("\"{key}\":")),
            "host 报告缺调度器字段 {key}：{json}"
        );
    }
    // captured 数值 > 0：fake 动态内容 2s 会话必然有帧进调度器。
    let captured: u64 = json
        .split("\"captured\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("captured 必须是数字：{json}"));
    assert!(captured > 0, "fake 动态内容应有帧进调度器：{json}");

    // stderr 完成摘要同步输出同一组数。
    let stderr = std::fs::read_to_string(tmp("sched", "host.err")).unwrap_or_default();
    assert!(
        stderr.contains("调度器：采集"),
        "stderr 摘要应含调度器帧去向：{stderr}"
    );
    assert!(
        stderr.contains(&format!("采集 {captured}")),
        "stderr 与 JSON 的 captured 应同值（{captured}）：{stderr}"
    );
    std::fs::remove_file(&host_report).ok();
}

/// client `--reconnect-at 1s --duration 6s`：`session.reconnect.resumed
/// == true`、`recovery_ms ≤ 500`、Gate 仍通过、exit 0。
#[test]
fn reconnect_scenario_recovers_within_budget() {
    let _serial = pair_serial();
    let (status, json) = run_pair(
        "reconnect",
        &["--duration", "10s"],
        &["--duration", "6s", "--reconnect-at", "1s"],
    );
    assert_eq!(
        status.code(),
        Some(0),
        "重连后 Gate 仍应通过，stderr: {}",
        std::fs::read_to_string(tmp("reconnect", "client.err")).unwrap_or_default()
    );
    assert!(
        json.contains("\"reconnect\":{\"attempts\":1,\"resumed\":true"),
        "重连演练必须 resumed=true：{json}"
    );
    // recovery_ms ≤ 500（从 JSON 抠数字段做数值断言）。
    let seg = json
        .split("\"reconnect\":{\"attempts\":1,\"resumed\":true,\"recovery_ms\":")
        .nth(1)
        .expect("recovery_ms 键");
    let value: u64 = seg
        .split('}')
        .next()
        .expect("数值")
        .parse()
        .unwrap_or_else(|_| panic!("recovery_ms 必须是数字：{json}"));
    assert!(value <= 500, "recovery_ms {value} 应 ≤ 500：{json}");
    assert!(json.contains("\"passed\":true"), "Gate 仍通过：{json}");
    std::fs::remove_file(tmp("reconnect", "client.json")).ok();
}

/// `--input-mode bogus` → clap exit 2。
#[test]
fn client_rejects_invalid_input_mode() {
    let mut child = Command::new(bin())
        .args([
            "client",
            "--connect",
            "127.0.0.1:9901",
            "--input-mode",
            "bogus",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let status = wait(&mut child, &mut [], Duration::from_secs(10));
    assert_eq!(status.code(), Some(2), "clap 参数错应 exit 2");
}

/// 修复卡D-2：`wait_or_kill` 超时路径必须先 kill 本体与同伴再返回
/// `None`——无 client 的 host 会阻塞在 accept（上限 300s），1s 超时必走
/// 超时路径；断言两个 host 进程都在短窗内被终止（不留孤儿占端口），
/// 而不是等 accept 兜底。
#[test]
fn wait_timeout_kills_children_instead_of_leaking() {
    let mut host = spawn(
        &[
            "host",
            "--listen",
            &format!("127.0.0.1:{}", reserve_port()),
            "--backend",
            "fake",
            "--duration",
            "30s",
        ],
        &tmp("tokill", "host.err"),
    );
    let mut sibling = spawn(
        &[
            "host",
            "--listen",
            &format!("127.0.0.1:{}", reserve_port()),
            "--backend",
            "fake",
            "--duration",
            "30s",
        ],
        &tmp("tokill", "sib.err"),
    );

    // 两个 host 都活着等 accept → 1s 后必超时。
    let timed_out = wait_or_kill(&mut host, &mut [&mut sibling], Duration::from_secs(1));
    assert!(timed_out.is_none(), "无 client 的 host 1s 内不会退出");

    // kill 是 SIGKILL：两个进程都必须在 2s 内被收割（若未 kill，host
    // 会一直挂到 300s accept 兜底，这里直接判失败）。
    let deadline = Instant::now() + Duration::from_secs(2);
    for (name, child) in [("host", &mut host), ("sibling", &mut sibling)] {
        let status = loop {
            match child.try_wait().expect("try wait") {
                Some(s) => break s,
                None => {
                    assert!(
                        Instant::now() < deadline,
                        "{name} 应被超时路径 kill（2s 内退出），而不是挂着等 accept"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        assert!(
            !status.success(),
            "{name} 被 kill 的退出状态不应是 success（实际 {status:?}）"
        );
    }
}

// ─────────────────────────────────────────────────────────────────
// 真机 / 权限测试（#[ignore]：经 ./scripts/macos-dev-run.sh 跑，需屏幕
// 录制 / 辅助功能权限与幕布供帧；本轮 CI 场景不执行）
// ─────────────────────────────────────────────────────────────────

/// 双进程 `--backend real`：host SCK+VT（需屏幕录制 + 幕布供帧）、client
/// VT 解码 + Metal 窗口。只断言：会话完成、报告落盘、
/// connect_to_first_frame_ms ≤800、无 panic；**不断言 fps/e2e 门槛**
/// （CPU 竞争噪声 + 静屏零编码是铁律 2 的正确行为，坑 4）。
#[test]
#[ignore = "真机权限场景：./scripts/macos-dev-run.sh test rc-cli -- --ignored --nocapture"]
fn localhost_real_backend_smoke() {
    let port = reserve_port();
    let connect = format!("127.0.0.1:{port}");
    let mut host = spawn(
        &[
            "host",
            "--listen",
            &connect,
            "--backend",
            "real",
            "--duration",
            "35s",
            "--inject-mode",
            "record",
            "--report",
            &tmp("real-host", "json"),
        ],
        &tmp("real-host", "err"),
    );
    std::thread::sleep(Duration::from_secs(2)); // SCK 流启动
    let mut client = spawn(
        &[
            "client",
            "--connect",
            &connect,
            "--backend",
            "real",
            "--duration",
            "30s",
            "--input-mode",
            "scripted",
            "--report",
            &tmp("real-client", "json"),
        ],
        &tmp("real-client", "err"),
    );
    let client_status = wait(&mut client, &mut [&mut host], Duration::from_secs(60));
    let host_status = wait(&mut host, &mut [], Duration::from_secs(60));
    assert!(
        client_status.success() || client_status.code() == Some(1),
        "client 应完成（Gate 结果不限，真后端不断言门槛）"
    );
    assert!(host_status.success(), "host 应正常收尾");
    let json = std::fs::read_to_string(tmp("real-client", "json")).expect("报告落盘");
    let c2ff = json
        .split("\"connect_to_first_frame_ms\":")
        .nth(1)
        .expect("键存在");
    if !c2ff.starts_with("null") {
        let ms: f64 = c2ff.split(',').next().unwrap().parse().unwrap();
        assert!(ms <= 800.0, "首帧 {ms}ms ≤ 800");
    }
}

/// host `--inject-mode real`（InputBridge，需辅助功能）：少量事件注入成功
/// 计数 >0（复用幕布工具授权）。
#[test]
#[ignore = "真机权限场景：./scripts/macos-dev-run.sh test rc-cli -- --ignored --nocapture"]
fn real_inject_via_input_bridge() {
    let port = reserve_port();
    let connect = format!("127.0.0.1:{port}");
    let mut host = spawn(
        &[
            "host",
            "--listen",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "10s",
            "--inject-mode",
            "real",
            "--report",
            &tmp("realinj", "json"),
        ],
        &tmp("realinj", "err"),
    );
    let mut client = spawn(
        &[
            "client",
            "--connect",
            &connect,
            "--backend",
            "fake",
            "--resolution",
            "64x64",
            "--duration",
            "5s",
            "--input-mode",
            "scripted",
        ],
        &tmp("realinj", "cerr"),
    );
    let client_status = wait(&mut client, &mut [&mut host], Duration::from_secs(20));
    let host_status = wait(&mut host, &mut [], Duration::from_secs(20));
    assert!(host_status.success(), "host 应正常收尾");
    assert!(client_status.success() || client_status.code() == Some(1));
    let json = std::fs::read_to_string(tmp("realinj", "json")).expect("host 报告落盘");
    let injected = json
        .split("\"injected_events\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<u64>().ok())
        .expect("injected_events 数值");
    assert!(injected > 0, "真注入应有事件计数 > 0：{json}");
}
