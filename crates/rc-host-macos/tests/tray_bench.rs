//! macOS 托盘与审批弹窗真机基准（`tray-bench` feature，`harness = false`）。
//!
//! # 为什么 `harness = false`
//! `NSStatusItem` / `NSAlert` 是 `MainThreadOnly` 类型（`objc2` 以
//! `MainThreadMarker` 编码该契约），而 libtest 的测试体永远运行在工作线程上。
//! 关闭 harness 后本文件的 `fn main` 运行在主线程，可合法创建托盘与弹窗。
//!
//! # 运行方式（需要 macOS GUI 登录会话，无需 TCC 权限）
//! ```bash
//! ./scripts/macos-dev-run.sh test rc-host-macos \
//!     --features tray-bench --test tray_bench -- -- --run --nocapture
//! ```
//! 不带 `--run` 时本目标直接跳过并退出 0（语义等价 `#[ignore]`）。

#[cfg(target_os = "macos")]
fn main() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use rc_host_macos::bridge::ApprovalBridge;
    use rc_host_macos::{inspect_approval_alert, TrayConfig, TrayHost, TrayState};
    use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};
    use rc_session::proto::{AuthMethod, SessionParams};

    if !std::env::args().any(|a| a == "--run") {
        eprintln!(
            "跳过：tray_bench 真机基准需要显式 --run（需要 macOS GUI 登录会话与 --features tray-bench）"
        );
        return;
    }

    // 1. 断言运行在主线程（MainThreadOnly 契约前提）。
    let mtm = objc2::MainThreadMarker::new().expect("harness=false 的 main 必须在主线程");
    eprintln!("[tray_bench] 主线程断言通过");

    // 2. 构造 TrayHost（创建 NSStatusItem + 三项菜单）。
    let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9905);
    let tray = TrayHost::new(TrayConfig { listen })
        .expect("TrayHost::new 失败（需要 macOS GUI 登录会话）");
    eprintln!("[tray_bench] TrayHost::new 成功（状态项已创建）");

    // 3. 切换 WaitingForConnection ↔ InSession 两态。
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 88)), 54321);
    tray.set_state(TrayState::InSession(remote));
    tray.set_state(TrayState::WaitingForConnection(listen));
    eprintln!("[tray_bench] set_state 两态切换通过");

    // 4. 用 alert_texts 构造真实 NSAlert 并断言 messageText / informativeText / 按钮标题
    //    （不调用 runModal，弹窗交互由人工验收覆盖）。
    let req = ApprovalRequest {
        session_id: 7,
        remote_addr: remote,
        requested: SessionParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
        },
        auth_method: AuthMethod::Password,
        password_verified: true,
    };
    let snap = inspect_approval_alert(mtm, &req);
    assert!(
        snap.message_text.contains("192.168.1.88:54321"),
        "NSAlert.messageText 应含对端地址，实际：{}",
        snap.message_text
    );
    assert!(
        snap.informative_text.contains("1920x1080@60"),
        "NSAlert.informativeText 应含请求参数，实际：{}",
        snap.informative_text
    );
    assert!(
        snap.informative_text.contains("密码已验证"),
        "NSAlert.informativeText 应含鉴权状态，实际：{}",
        snap.informative_text
    );
    assert_eq!(
        snap.button_titles,
        vec!["接受".to_string(), "拒绝".to_string()],
        "NSAlert 按钮标题应为 [接受, 拒绝]"
    );
    eprintln!(
        "[tray_bench] NSAlert 构造与文案断言通过：msg={:?}, buttons={:?}",
        snap.message_text, snap.button_titles
    );

    // 5. 桥往返验证（submit → take_request → resolve → poll_verdict）。
    let bridge = ApprovalBridge::new();
    let mut ui = bridge.ui_side();
    let mut session = bridge.session_side();
    session.submit(&req);
    assert_eq!(ui.take_request().as_ref(), Some(&req));
    ui.resolve(ApprovalVerdict::Accepted);
    assert_eq!(session.poll_verdict(), Some(ApprovalVerdict::Accepted));
    eprintln!("[tray_bench] ApprovalBridge 往返通过");

    // 6. 释放 TrayHost（Drop 自动从 NSStatusBar 移除状态项）并正常退出。
    drop(tray);
    eprintln!("[tray_bench] 全部基准断言通过，已优雅清理状态项。");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("tray_bench 仅限 macOS");
    std::process::exit(2);
}
