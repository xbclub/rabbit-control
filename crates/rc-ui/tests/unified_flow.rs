//! 一体化 `rc-ui` 被控端 + 控制端进程内集成测试（Phase 1）。
//!
//! 全程使用临时端口 `127.0.0.1:0` 与假后端（`prefer_hardware: false`），
//! 验证 `HostHandle` + `UiApprover` + `SessionHandle` 的密码鉴权与审批闭环。

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use rc_ui::host_bridge::{HostHandle, HostUpdate, UiHostConfig};
use rc_ui::session_bridge::{SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig};

fn loopback_ephemeral() -> SocketAddr {
    "127.0.0.1:0".parse().expect("valid loopback addr")
}

fn client_cfg(addr: SocketAddr, password: Option<&str>) -> UiSessionConfig {
    UiSessionConfig {
        connect_addr: addr,
        password: password.map(str::to_string),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: None,
        ..UiSessionConfig::default()
    }
}

fn host_cfg(password: Option<&str>, require_approval: bool) -> UiHostConfig {
    UiHostConfig {
        listen_addr: loopback_ephemeral(),
        password: password.map(str::to_string),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    }
}

fn collect_host_until(
    host: &HostHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[HostUpdate]) -> bool,
) -> Vec<HostUpdate> {
    let mut updates = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(u) = host.try_recv_update() {
            updates.push(u);
        }
        if pred(&updates) {
            return updates;
        }
        thread::sleep(Duration::from_millis(10));
    }
    while let Some(u) = host.try_recv_update() {
        updates.push(u);
    }
    updates
}

fn collect_client_until(
    session: &SessionHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionUpdate]) -> bool,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(u) = session.try_recv_update() {
            updates.push(u);
        }
        if pred(&updates) {
            return updates;
        }
        thread::sleep(Duration::from_millis(10));
    }
    while let Some(u) = session.try_recv_update() {
        updates.push(u);
    }
    updates
}

#[test]
fn unified_host_and_client_with_password_and_approval() {
    let mut host = HostHandle::spawn(host_cfg(Some("TEST1234"), true)).expect("spawn host");
    let listen_addr = host.listen_addr();
    assert_ne!(listen_addr.port(), 0);

    let session = SessionHandle::spawn(client_cfg(listen_addr, Some("TEST1234")));

    // 1) Host 应收到密码已验证的审批请求。
    let host_updates = collect_host_until(&host, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    let prompt = host_updates
        .iter()
        .find_map(|u| match u {
            HostUpdate::ApprovalRequested(p) => Some(p.clone()),
            _ => None,
        })
        .expect("host should emit ApprovalRequested");
    assert!(
        prompt.password_verified,
        "正确密码后审批请求应标记 password_verified=true"
    );
    assert_eq!(prompt.requested_res, "64x64");
    assert_eq!(prompt.requested_fps, 60);

    // 2) 机主点击同意。
    assert!(host.respond_approval(prompt.generation, true));

    // 3) Client 收到 Negotiated 与 fps > 0 的 HUD 快照。
    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_fps = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        has_neg && has_fps
    });
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "client should receive Negotiated, got: {client_updates:?}"
    );
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0)),
        "client should receive Hud with fps > 0, got: {client_updates:?}"
    );

    // 4) Host 侧应报告 ClientConnected。
    let host_conn = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    assert!(
        host_conn
            .iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. })),
        "host should emit ClientConnected, got: {host_conn:?}"
    );

    // 5) 优雅断开，双方干净停机。
    session.send(SessionCommand::Disconnect);
    let end_updates = collect_client_until(&session, Duration::from_secs(5), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        end_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "client should emit Ended after Disconnect"
    );
    session.join();

    host.stop();
    assert!(!host.is_running());
}

#[test]
fn unified_host_rejects_wrong_password_without_approval_prompt() {
    let mut host = HostHandle::spawn(host_cfg(Some("RIGHT999"), true)).expect("spawn host");
    let session = SessionHandle::spawn(client_cfg(host.listen_addr(), Some("WRONG000")));

    // 错误密码应导致控制端握手失败结束。
    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    let ended_reason = client_updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Ended { reason } => Some(reason.clone()),
            _ => None,
        })
        .expect("client should end with failure on wrong password");
    assert!(
        !client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "wrong password must not reach Negotiated (reason={ended_reason})"
    );

    // 停机前收集 host 侧所有事件，确认从未弹出审批请求。
    let host_updates = collect_host_until(&host, Duration::from_millis(300), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    assert!(
        !host_updates
            .iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_))),
        "错误密码绝不能触发 ApprovalRequested，实际收到: {host_updates:?}"
    );

    session.join();
    host.stop();
    assert!(!host.is_running());
}

#[test]
fn unified_host_rejects_when_user_denies_approval() {
    let mut host = HostHandle::spawn(host_cfg(Some("PASS2345"), true)).expect("spawn host");
    let session = SessionHandle::spawn(client_cfg(host.listen_addr(), Some("PASS2345")));

    // 1) 正确密码后弹出审批请求。
    let host_updates = collect_host_until(&host, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    let prompt = host_updates
        .iter()
        .find_map(|u| match u {
            HostUpdate::ApprovalRequested(p) => Some(p.clone()),
            _ => None,
        })
        .expect("host should emit ApprovalRequested");
    assert!(prompt.password_verified);

    // 2) 机主点击拒绝。
    assert!(host.respond_approval(prompt.generation, false));

    // 3) 控制端以失败结束且未进入 Negotiated。
    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "client should end after host denies approval"
    );
    assert!(
        !client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "denied approval must not reach Negotiated"
    );

    session.join();
    host.stop();
    assert!(!host.is_running());
}

#[test]
fn unified_host_auto_accepts_verified_password_and_prompts_when_passwordless() {
    // 1) 有密码 + auto_accept_with_password = true：密码通过后自动放行，不弹 ApprovalRequested。
    let mut cfg = host_cfg(Some("AUTO2026"), true);
    cfg.auto_accept_with_password = true;
    let mut host = HostHandle::spawn(cfg).expect("spawn host");
    let session = SessionHandle::spawn(client_cfg(host.listen_addr(), Some("AUTO2026")));

    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "密码校验通过且启用 auto_accept_with_password 应直接进入 Negotiated: {client_updates:?}"
    );

    let host_updates = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    assert!(
        host_updates
            .iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. })),
        "host 应上报 ClientConnected: {host_updates:?}"
    );
    assert!(
        !host_updates
            .iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_))),
        "密码通过自动放行时不得弹出 ApprovalRequested: {host_updates:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();

    // 2) 无密码 + auto_accept_with_password = true：仍必须弹出 ApprovalRequested 并等待机主同意。
    let mut no_pw_cfg = host_cfg(None, true);
    no_pw_cfg.auto_accept_with_password = true;
    let mut host2 = HostHandle::spawn(no_pw_cfg).expect("spawn passwordless host");
    let session2 = SessionHandle::spawn(client_cfg(host2.listen_addr(), None));

    let host2_updates = collect_host_until(&host2, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    let prompt = host2_updates
        .iter()
        .find_map(|u| match u {
            HostUpdate::ApprovalRequested(p) => Some(p.clone()),
            _ => None,
        })
        .expect("无密码连接必须弹出 ApprovalRequested");
    assert!(!prompt.password_verified);
    assert!(host2.respond_approval(prompt.generation, true));

    let client2_updates = collect_client_until(&session2, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        client2_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "机主手动同意后无密码会话应进入 Negotiated: {client2_updates:?}"
    );

    session2.send(SessionCommand::Disconnect);
    session2.join();
    host2.stop();
}
