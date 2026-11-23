//! 中继远控会话端到端集成测试（离线本地 RCR1 回环 + 真实 `172.16.1.2` 9 位设备码真服验证）。

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rc_ui::host_bridge::{HostHandle, HostUpdate, UiHostConfig, TEMP_CODE_LEN};
use rc_ui::server_config::{
    ServerConfig, ServerConnectionState, DEFAULT_RELAY_PORT, DEFAULT_SIGNAL_PORT,
};
use rc_ui::session_bridge::{SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig};
use rc_ui::signal_client::{ConnectGrant, RelayIncoming, SignalClientHandle};

const REPLY_OK: u8 = 0x00;

/// 真服测试默认固定设备密钥（64 位十六进制），避免重复运行在 PostgreSQL 中产生重复设备行。
const DEFAULT_LIVE_CONTROLLER_KEY: &str =
    "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f91";
const DEFAULT_LIVE_HOST_KEY: &str =
    "b1c2d3e4f5061728394a5b6c7d8e9f01b1c2d3e4f5061728394a5b6c7d8e9f02";

/// 本地模拟 `RCR1` TCP 中继服务器：验证双方握手令牌与互补角色，回复 `0x00` 后双向透明转发。
fn spawn_local_rcr1_relay(expected_token: String) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind local rcr1 relay");
    let addr = listener.local_addr().expect("relay local_addr");

    let handle = thread::spawn(move || {
        let (mut s1, _) = listener.accept().expect("accept first peer");
        let (mut s2, _) = listener.accept().expect("accept second peer");
        let _ = s1.set_nodelay(true);
        let _ = s2.set_nodelay(true);

        let r1 = verify_rcr1_handshake(&mut s1, &expected_token);
        let r2 = verify_rcr1_handshake(&mut s2, &expected_token);
        assert_ne!(r1, r2, "host 和 client 必须使用互补角色（1 vs 2）");
        assert!(matches!(r1, 1 | 2) && matches!(r2, 1 | 2));

        s1.write_all(&[REPLY_OK]).expect("reply ok to peer 1");
        s2.write_all(&[REPLY_OK]).expect("reply ok to peer 2");

        let mut s1_r = s1.try_clone().expect("clone s1");
        let mut s2_w = s2.try_clone().expect("clone s2");
        let t1 = thread::spawn(move || {
            let _ = io::copy(&mut s1_r, &mut s2_w);
            let _ = s2_w.shutdown(Shutdown::Write);
        });
        let t2 = thread::spawn(move || {
            let _ = io::copy(&mut s2, &mut s1);
            let _ = s1.shutdown(Shutdown::Write);
        });
        let _ = t1.join();
        let _ = t2.join();
    });

    (addr, handle)
}

fn verify_rcr1_handshake(stream: &mut TcpStream, expected_token: &str) -> u8 {
    let mut head = [0u8; 6];
    stream.read_exact(&mut head).expect("read rcr1 header");
    assert_eq!(&head[0..4], b"RCR1");
    let role = head[4];
    let token_len = usize::from(head[5]);
    let mut token = vec![0u8; token_len];
    stream.read_exact(&mut token).expect("read rcr1 token");
    assert_eq!(
        std::str::from_utf8(&token).expect("utf8 token"),
        expected_token
    );
    role
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
fn inproc_relay_host_and_client_stream_with_password_and_approval() {
    let token = "inproc-rcr1-token-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("RELAY2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: true,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    // 投递入站中继会话（模拟信令服务器向被控端下发 relay_ready）。
    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-inproc-01".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    // 控制端以 relay_grant 启动会话。
    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().expect("placeholder"),
        password: Some("RELAY2026".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: Some(ConnectGrant {
            session_id: "sess-inproc-01".to_string(),
            relay_addr: relay_addr_str,
            relay_token: token,
            target_id: "123456789".to_string(),
            peer_public_addr: None,
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });

    // 1) 被控端应通过密码校验并弹出审批请求。
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
        .expect("host should emit ApprovalRequested over relay");
    assert!(prompt.password_verified, "正确密码后应通过密码校验");
    assert!(
        prompt.remote_addr.contains("604928235"),
        "审批弹窗来源应包含对端设备码，实际：{}",
        prompt.remote_addr
    );

    // 2) 机主点击同意。
    assert!(host.respond_approval(prompt.generation, true));

    // 3) 控制端收到 Negotiated（标注中继）与 fps > 0 的 HUD 快照。
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
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated {
                width: 64,
                height: 64,
                fps: 60,
                backend,
                ..
            } if backend.contains("中继")
        )),
        "client should receive Negotiated noting relay, got: {client_updates:?}"
    );
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0)),
        "client should receive Hud with fps > 0 over relay, got: {client_updates:?}"
    );

    // 4) 被控端应上报 ClientConnected（带设备码与中继标注）。
    let host_conn = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    assert!(
        host_conn.iter().any(|u| matches!(
            u,
            HostUpdate::ClientConnected { remote_addr } if remote_addr == "设备 604928235 (中继)"
        )),
        "host should emit ClientConnected with relay peer label, got: {host_conn:?}"
    );

    // 5) 优雅断开，两端干净退出并回报 ClientDisconnected / Ended。
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

    let host_disc = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. }))
    });
    assert!(
        host_disc
            .iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. })),
        "host should emit ClientDisconnected after client Bye, got: {host_disc:?}"
    );

    session.join();
    host.stop();
    assert!(!host.is_running());
    relay_thread.join().expect("relay thread should not panic");
}

fn make_relay_session_cfg(
    relay_addr: String,
    token: String,
    password: Option<&str>,
) -> UiSessionConfig {
    UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().expect("placeholder"),
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
        relay_grant: Some(ConnectGrant {
            session_id: format!("sess-{token}"),
            relay_addr,
            relay_token: token,
            target_id: "123456789".to_string(),
            peer_public_addr: None,
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    }
}

#[test]
fn inproc_relay_rejects_wrong_password_without_approval_prompt() {
    let token = "inproc-wrong-pw-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("RIGHT2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: true,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-wrong-pw".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(make_relay_session_cfg(
        relay_addr_str,
        token,
        Some("WRONG0000"),
    ));

    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "wrong password over relay should emit Ended, got: {client_updates:?}"
    );
    assert!(
        !client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "wrong password over relay must not reach Negotiated, got: {client_updates:?}"
    );

    let host_updates = collect_host_until(&host, Duration::from_millis(300), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    assert!(
        !host_updates
            .iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_))),
        "wrong password over relay must not prompt approval, got: {host_updates:?}"
    );

    session.join();
    host.stop();
    relay_thread.join().expect("relay thread should not panic");
}

#[test]
fn inproc_relay_rejects_when_user_denies_approval() {
    let token = "inproc-deny-approval-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("PASS2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: true,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-deny".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(make_relay_session_cfg(
        relay_addr_str,
        token,
        Some("PASS2026"),
    ));

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
    assert!(host.respond_approval(prompt.generation, false));

    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "denied approval over relay should end client session, got: {client_updates:?}"
    );
    assert!(
        !client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "denied approval over relay must not reach Negotiated, got: {client_updates:?}"
    );

    session.join();
    host.stop();
    relay_thread.join().expect("relay thread should not panic");
}

#[test]
fn inproc_relay_host_disconnect_client_ends_session_and_reports_tcp_relay_hud() {
    let token = "inproc-host-disc-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("DISC2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-host-disc".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(make_relay_session_cfg(
        relay_addr_str,
        token,
        Some("DISC2026"),
    ));

    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0))
    });
    let relay_hud = client_updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Hud(h) if h.fps > 0.0 => Some(h.clone()),
            _ => None,
        })
        .expect("client should receive Hud with fps > 0 over relay");
    assert_eq!(
        relay_hud.transport, "TCP Relay",
        "relay HUD must report TCP Relay transport"
    );

    // 被控端主动断开：客户端与被控端均须干净收尾。
    host.disconnect_client();

    let client_ended = collect_client_until(&session, Duration::from_secs(5), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        client_ended
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "client should emit Ended after host.disconnect_client(), got: {client_ended:?}"
    );

    let host_disc = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. }))
    });
    assert!(
        host_disc
            .iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. })),
        "host should emit ClientDisconnected after disconnect_client(), got: {host_disc:?}"
    );

    session.join();
    host.stop();
    relay_thread.join().expect("relay thread should not panic");
}

#[test]
fn host_stop_aborts_stalled_relay_dial_promptly() {
    // 模拟黑洞/无响应中继：接受 TCP 后永不回写 1 字节 RCR1 应答，验证 HostHandle::stop() 在 < 1s 内中断。
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind blackhole relay");
    let blackhole_addr = listener.local_addr().expect("blackhole local_addr");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel::<()>(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);

    let blackhole_thread = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = accepted_tx.send(());
            // 保持套接字打开直到测试结束，确保主机确实阻塞在 10s RCR1 握手等待中。
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            drop(stream);
        }
    });

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: None,
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-blackhole".to_string(),
        relay_addr: blackhole_addr.to_string(),
        relay_token: "blackhole-token-2026".to_string(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    accepted_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("host should connect to blackhole relay and stall waiting for RCR1 reply");

    let stop_start = Instant::now();
    host.stop();
    let elapsed = stop_start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "HostHandle::stop() must abort stalled relay connect within 1s, took {elapsed:?}"
    );
    assert!(!host.is_running());

    let _ = release_tx.send(());
    blackhole_thread
        .join()
        .expect("blackhole relay thread should not panic");
}

#[test]
fn session_early_disconnect_aborts_relay_handshake_promptly() {
    let token = "inproc-early-disc-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("WAIT2026".to_string()),
        approval_timeout_us: 45_000_000,
        prefer_hardware: false,
        require_approval: true,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-early-disc".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(make_relay_session_cfg(
        relay_addr_str,
        token,
        Some("WAIT2026"),
    ));

    // 等待控制端进入 45s 审批等待阶段（主机已收到审批弹窗，但机主暂不点击）。
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

    // 控制端提前发送 Disconnect：必须在 < 1s 内中止握手并退出，且自动取消主机侧审批弹窗。
    let disc_start = Instant::now();
    session.send(SessionCommand::Disconnect);
    session.join();
    let elapsed = disc_start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "SessionHandle early Disconnect must abort 45s relay handshake within 1s, took {elapsed:?}"
    );

    let host_cancel = collect_host_until(&host, Duration::from_secs(3), |us| {
        us.iter().any(|u| {
            matches!(
                u,
                HostUpdate::ApprovalCancelled { generation } if *generation == prompt.generation
            )
        })
    });
    assert!(
        host_cancel.iter().any(|u| matches!(
            u,
            HostUpdate::ApprovalCancelled { generation } if *generation == prompt.generation
        )),
        "host approval prompt should be cancelled promptly on client early disconnect, got: {host_cancel:?}"
    );

    host.stop();
    relay_thread.join().expect("relay thread should not panic");
}

#[test]
fn inproc_relay_auto_rotates_temp_password_between_back_to_back_sessions() {
    let initial_pw = "INIT2345".to_string();
    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some(initial_pw.clone()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: true,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    // 1) 第一次中继会话使用初始临时密码成功连接并随后断开。
    let tok1 = "inproc-rotate-1".to_string();
    let (relay1_addr, relay1_thread) = spawn_local_rcr1_relay(tok1.clone());
    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-rot-1".to_string(),
        relay_addr: relay1_addr.to_string(),
        relay_token: tok1.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });
    let session1 = SessionHandle::spawn(make_relay_session_cfg(
        relay1_addr.to_string(),
        tok1,
        Some(&initial_pw),
    ));
    let s1_updates = collect_client_until(&session1, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        s1_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "first session should negotiate with initial temp password"
    );
    session1.send(SessionCommand::Disconnect);
    session1.join();

    // 等待主机原子轮换密码并上报 PasswordRotated + ClientDisconnected。
    let host_after_1 = collect_host_until(&host, Duration::from_secs(5), |us| {
        let has_rot = us
            .iter()
            .any(|u| matches!(u, HostUpdate::PasswordRotated { .. }));
        let has_disc = us
            .iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. }));
        has_rot && has_disc
    });
    let rotated_pw = host_after_1
        .iter()
        .find_map(|u| match u {
            HostUpdate::PasswordRotated { password } => Some(password.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("host must emit PasswordRotated after session ends when auto_rotate_password=true, got: {host_after_1:?}")
        });
    assert_eq!(rotated_pw.len(), TEMP_CODE_LEN);
    assert_ne!(
        rotated_pw, initial_pw,
        "rotated temp password must differ from initial password"
    );

    // 2) 第二次背靠背中继会话继续使用旧临时密码：必须被拒绝，绝不能进入 Negotiated。
    let tok2 = "inproc-rotate-2-old".to_string();
    let (relay2_addr, relay2_thread) = spawn_local_rcr1_relay(tok2.clone());
    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-rot-2-old".to_string(),
        relay_addr: relay2_addr.to_string(),
        relay_token: tok2.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });
    let session2 = SessionHandle::spawn(make_relay_session_cfg(
        relay2_addr.to_string(),
        tok2,
        Some(&initial_pw),
    ));
    let s2_updates = collect_client_until(&session2, Duration::from_secs(10), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        s2_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "second session using old temp password must end with auth failure, got: {s2_updates:?}"
    );
    assert!(
        !s2_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "second session using old temp password must NOT negotiate, got: {s2_updates:?}"
    );
    session2.join();

    // 3) 第三次中继会话使用轮换后的新临时密码：必须成功握手并收到 TCP Relay HUD。
    let tok3 = "inproc-rotate-3-new".to_string();
    let (relay3_addr, relay3_thread) = spawn_local_rcr1_relay(tok3.clone());
    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-rot-3-new".to_string(),
        relay_addr: relay3_addr.to_string(),
        relay_token: tok3.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: None,
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });
    let session3 = SessionHandle::spawn(make_relay_session_cfg(
        relay3_addr.to_string(),
        tok3,
        Some(&rotated_pw),
    ));
    let s3_updates = collect_client_until(&session3, Duration::from_secs(10), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_hud = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.transport == "TCP Relay"));
        has_neg && has_hud
    });
    assert!(
        s3_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "third session with rotated temp password must succeed, got: {s3_updates:?}"
    );
    assert!(
        s3_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.transport == "TCP Relay")),
        "third session HUD must report TCP Relay, got: {s3_updates:?}"
    );

    session3.send(SessionCommand::Disconnect);
    session3.join();
    host.stop();
    relay1_thread
        .join()
        .expect("relay1 thread should not panic");
    relay2_thread
        .join()
        .expect("relay2 thread should not panic");
    relay3_thread
        .join()
        .expect("relay3 thread should not panic");
}

fn wait_signal_connected_id(client: &SignalClientHandle, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let snap = client.status();
        if snap.state == ServerConnectionState::Connected {
            if let Some(id) = snap.device_id {
                if id.len() == 9 && id.bytes().all(|b| b.is_ascii_digit()) {
                    return id;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("信令服务器连接/注册超时，最后状态: {:?}", client.status());
}

#[test]
#[ignore = "需要局域网实时服务器 172.16.1.2:21116/21117 在线，手动运行真服全链路验收"]
fn live_172_16_1_2_nine_digit_code_full_session() {
    let server_host =
        std::env::var("RC_LIVE_SERVER_HOST").unwrap_or_else(|_| "172.16.1.2".to_string());
    let controller_key = std::env::var("RC_LIVE_CONTROLLER_KEY")
        .unwrap_or_else(|_| DEFAULT_LIVE_CONTROLLER_KEY.to_string());
    let host_key =
        std::env::var("RC_LIVE_HOST_KEY").unwrap_or_else(|_| DEFAULT_LIVE_HOST_KEY.to_string());

    let make_cfg = |device_key: String| ServerConfig {
        enabled: true,
        server_host: server_host.clone(),
        signal_port: DEFAULT_SIGNAL_PORT,
        relay_port: DEFAULT_RELAY_PORT,
        use_https: false,
        device_key,
        cached_device_id: None,
        ..ServerConfig::default()
    };

    // 1) 两台设备使用稳定固定测试密钥接入信令服，幂等复用 PostgreSQL 中的 9 位设备码。
    let mut sig_a = SignalClientHandle::spawn(
        make_cfg(controller_key),
        "rc-test-controller".to_string(),
        vec![],
    );
    let mut sig_b =
        SignalClientHandle::spawn(make_cfg(host_key), "rc-test-host".to_string(), vec![]);

    let id_a = wait_signal_connected_id(&sig_a, Duration::from_secs(15));
    let id_b = wait_signal_connected_id(&sig_b, Duration::from_secs(15));
    assert_ne!(id_a, id_b, "两台独立设备应获得不同的 9 位设备码");
    println!("[live] server={server_host} controller device_id={id_a}, host device_id={id_b}");

    // 2) Device B 启动本机被控端（密码 LIVE2026，假后端，开启审批）。
    let mut host_b = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("LIVE2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: true,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host B");

    // 3) Device A 用 9 位设备码向 172.16.1.2 发起连接请求，获取一次性中继令牌。
    let grant = sig_a
        .request_connect(&id_b, Duration::from_secs(10))
        .expect("Device A request_connect(id_b) must succeed");
    println!(
        "[live] got ConnectGrant: session_id={} relay_addr={} target_id={}",
        grant.session_id, grant.relay_addr, grant.target_id
    );

    // 4) Device B 收到服务器推送的入站 RelayIncoming，交给 HostHandle 拨号 172.16.1.2:21117。
    let incoming_deadline = Instant::now() + Duration::from_secs(10);
    let incoming = loop {
        if let Some(item) = sig_b.try_recv_incoming() {
            break item;
        }
        assert!(
            Instant::now() < incoming_deadline,
            "Device B 等待 RelayIncoming 超时"
        );
        thread::sleep(Duration::from_millis(20));
    };
    println!(
        "[live] Device B received RelayIncoming: session_id={} relay_addr={} peer_id={}",
        incoming.session_id, incoming.relay_addr, incoming.peer_id
    );
    assert_eq!(incoming.peer_id, id_a);
    host_b.offer_relay_incoming(incoming);

    // 5) Device A 用 ConnectGrant 启动控制端会话。
    let session_a = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().expect("placeholder"),
        password: Some("LIVE2026".to_string()),
        watch_only: false,
        width: 320,
        height: 180,
        fps: 60,
        bitrate_bps: 4_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        relay_grant: Some(grant),
        force_relay: false,
        ..UiSessionConfig::default()
    });

    // 6) 验证密码校验通过并触发机主审批弹窗，机主点击同意。
    let host_updates = collect_host_until(&host_b, Duration::from_secs(15), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_)))
    });
    let prompt = host_updates
        .iter()
        .find_map(|u| match u {
            HostUpdate::ApprovalRequested(p) => Some(p.clone()),
            _ => None,
        })
        .expect("Device B must emit ApprovalRequested");
    assert!(prompt.password_verified, "LIVE2026 密码必须校验通过");
    assert!(
        prompt.remote_addr.contains(&id_a),
        "审批来源应标注控制端设备码 {id_a}，实际: {}",
        prompt.remote_addr
    );
    assert!(host_b.respond_approval(prompt.generation, true));

    // 7) Device A 经 172.16.1.2 协调的 UDP P2P 直连或 Linux splice(2) TCP 中继收到 Negotiated 与实时视频帧 HUD（fps > 0）。
    let client_updates = collect_client_until(&session_a, Duration::from_secs(15), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_fps = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        has_neg && has_fps
    });
    let neg_backend = client_updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Negotiated { backend, .. } => Some(backend.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!("Device A should receive Negotiated, got: {client_updates:?}");
        });
    let is_p2p = neg_backend.contains("P2P 直连");
    let is_relay = neg_backend.contains("中继");
    assert!(
        is_p2p || is_relay,
        "Device A Negotiated backend must report either P2P 直连 or 中继, got: {neg_backend}"
    );
    let live_hud = client_updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Hud(h) if h.fps > 0.0 => Some(h.clone()),
            _ => None,
        })
        .expect("Device A must receive live Hud with fps > 0 via 172.16.1.2");
    let expected_transport = if is_p2p { "UDP P2P" } else { "TCP Relay" };
    assert_eq!(
        live_hud.transport, expected_transport,
        "HUD transport must match negotiated path (backend={neg_backend})"
    );
    let winning_path = if is_p2p {
        "UDP P2P 直连"
    } else {
        "TCP Relay 中继"
    };
    println!(
        "[live] Device A streaming HUD via {server_host} ({winning_path}): backend=\"{neg_backend}\" transport={} fps={:.1} bitrate={}kbps res={}",
        live_hud.transport, live_hud.fps, live_hud.bitrate_kbps, live_hud.resolution
    );

    // 8) Device B 上报 ClientConnected（与获胜路径一致）。
    let host_conn = collect_host_until(&host_b, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    let expected_peer = if is_p2p {
        format!("设备 {id_a} (P2P 直连)")
    } else {
        format!("设备 {id_a} (中继)")
    };
    assert!(
        host_conn.iter().any(|u| matches!(
            u,
            HostUpdate::ClientConnected { remote_addr } if remote_addr == &expected_peer
        )),
        "Device B should emit ClientConnected({expected_peer}), got: {host_conn:?}"
    );

    // 9) 优雅断开，确认两端干净收尾。
    session_a.send(SessionCommand::Disconnect);
    let ended = collect_client_until(&session_a, Duration::from_secs(5), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(ended
        .iter()
        .any(|u| matches!(u, SessionUpdate::Ended { .. })));

    let host_disc = collect_host_until(&host_b, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. }))
    });
    assert!(host_disc
        .iter()
        .any(|u| matches!(u, HostUpdate::ClientDisconnected { .. })));

    session_a.join();
    host_b.stop();
    sig_a.stop();
    sig_b.stop();
    println!("[live] full 9-digit code session via 172.16.1.2 PASSED cleanly!");
}

fn reserve_ephemeral_udp_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
    sock.local_addr().expect("udp local_addr").port()
}

fn reserve_ephemeral_tcp_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral tcp");
    listener.local_addr().expect("tcp local_addr").port()
}

#[test]
fn inproc_p2p_udp_hole_punch_connects_directly_without_relay() {
    let host_udp_port = reserve_ephemeral_udp_port();
    let client_udp_port = reserve_ephemeral_udp_port();
    assert_ne!(host_udp_port, client_udp_port);

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("P2P2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    // 故意填入不可达的 TCP 中继地址（127.0.0.1:1），证明本次建连 100% 经由 UDP P2P 打洞直连。
    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-p2p-direct-01".to_string(),
        relay_addr: "127.0.0.1:1".to_string(),
        relay_token: "unused-relay-token".to_string(),
        peer_id: "604928235".to_string(),
        peer_public_addr: Some(format!("127.0.0.1:{client_udp_port}")),
        peer_lan_addrs: Vec::new(),
        local_udp_port: Some(host_udp_port),
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().expect("placeholder"),
        password: Some("P2P2026".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: Some(ConnectGrant {
            session_id: "sess-p2p-direct-01".to_string(),
            relay_addr: "127.0.0.1:1".to_string(),
            relay_token: "unused-relay-token".to_string(),
            target_id: "123456789".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{host_udp_port}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: Some(client_udp_port),
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });

    let client_updates = collect_client_until(&session, Duration::from_secs(10), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_hud = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        has_neg && has_hud
    });
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated { backend, .. } if backend.contains("P2P 直连")
        )),
        "client Negotiated must note P2P 直连, got: {client_updates:?}"
    );
    assert!(
        client_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.transport == "UDP P2P" && h.fps > 0.0)),
        "client HUD must report UDP P2P transport with fps > 0, got: {client_updates:?}"
    );

    let host_conn = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    assert!(
        host_conn.iter().any(|u| matches!(
            u,
            HostUpdate::ClientConnected { remote_addr } if remote_addr == "设备 604928235 (P2P 直连)"
        )),
        "host must emit ClientConnected with P2P 直连 label, got: {host_conn:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
}

#[test]
fn inproc_p2p_unreachable_falls_back_seamlessly_to_tcp_relay() {
    let token = "inproc-p2p-fallback-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    // 使用两端口互不匹配的假候选地址（各自朝向空洞端口），模拟对称 NAT / UDP 阻断，
    // 验证双方打洞超时后自动无缝回退至 TCP Relay。
    let dead_port_a = reserve_ephemeral_udp_port();
    let dead_port_b = reserve_ephemeral_udp_port();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("FALLBACK26".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-p2p-fallback".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: Some(format!("127.0.0.1:{dead_port_a}")),
        peer_lan_addrs: Vec::new(),
        local_udp_port: None,
        ..RelayIncoming::default()
    });

    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().expect("placeholder"),
        password: Some("FALLBACK26".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: Some(ConnectGrant {
            session_id: "sess-p2p-fallback".to_string(),
            relay_addr: relay_addr_str,
            relay_token: token,
            target_id: "123456789".to_string(),
            peer_public_addr: Some(format!("127.0.0.1:{dead_port_b}")),
            peer_lan_addrs: Vec::new(),
            local_udp_port: None,
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });

    let client_updates = collect_client_until(&session, Duration::from_secs(12), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_hud = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        has_neg && has_hud
    });
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated { backend, .. } if backend.contains("中继")
        )),
        "fallback client Negotiated must note 中继, got: {client_updates:?}"
    );
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Hud(h) if h.transport == "TCP Relay" && h.fps > 0.0
        )),
        "fallback client HUD must report TCP Relay, got: {client_updates:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
    relay_thread.join().expect("relay thread join");
}

#[test]
fn inproc_tools_file_terminal_and_portmap_work_end_to_end_over_session() {
    use rc_tools::{FileEvent, TransferStatus};

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("TOOLS2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        password: Some("TOOLS2026".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 4_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: true,
        relay_grant: None,
        force_relay: false,
        ..UiSessionConfig::default()
    });

    let neg = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        neg.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "session must reach Negotiated, got: {neg:?}"
    );

    // 验证运行期音频静音与剪贴板切换命令不中断串流。
    session.send(SessionCommand::SetAudioMuted { muted: true });
    session.send(SessionCommand::SetClipboardEnabled { enabled: false });

    let tools = session.tools();

    // ── 1) W2 文件传输：远端建目录 → 列目录 → 上传 96 KiB → 下载回本地 → 字节一致 ──
    let base_dir = std::env::temp_dir().join(format!(
        "rc-ui-e2e-tools-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let local_src_dir = base_dir.join("local_src");
    let local_dst_dir = base_dir.join("local_dst");
    let remote_root = base_dir.join("remote_root");
    std::fs::create_dir_all(&local_src_dir).expect("mkdir local_src");
    std::fs::create_dir_all(&local_dst_dir).expect("mkdir local_dst");
    std::fs::create_dir_all(&remote_root).expect("mkdir remote_root");

    tools
        .file_mkdir_remote(
            remote_root.to_string_lossy().into_owned(),
            "uploads".to_string(),
        )
        .expect("file_mkdir_remote");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut mkdir_ok = false;
    while Instant::now() < deadline && !mkdir_ok {
        while let Some(ev) = session.try_recv_file_event() {
            if matches!(ev, FileEvent::MkdirResult { ok: true, .. }) {
                mkdir_ok = true;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(mkdir_ok, "remote mkdir must succeed");

    let remote_uploads = remote_root.join("uploads");
    let payload: Vec<u8> = (0..98_304usize).map(|i| (i % 251) as u8).collect();
    let src_file = local_src_dir.join("sample.bin");
    std::fs::write(&src_file, &payload).expect("write sample.bin");

    let up_id = tools
        .file_upload(
            src_file.to_string_lossy().as_ref(),
            remote_uploads.to_string_lossy().as_ref(),
        )
        .expect("file_upload");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut upload_done = false;
    while Instant::now() < deadline && !upload_done {
        while let Some(ev) = session.try_recv_file_event() {
            if let FileEvent::TransferUpdated(item) = ev {
                if item.id == up_id && item.status == TransferStatus::Done {
                    upload_done = true;
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(upload_done, "96 KiB upload must finish with Done");
    let uploaded_path = remote_uploads.join("sample.bin");
    assert_eq!(
        std::fs::read(&uploaded_path).expect("read uploaded file"),
        payload
    );

    let down_id = tools
        .file_download(
            uploaded_path.to_string_lossy().as_ref(),
            local_dst_dir.to_string_lossy().as_ref(),
        )
        .expect("file_download");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut download_done = false;
    while Instant::now() < deadline && !download_done {
        while let Some(ev) = session.try_recv_file_event() {
            if let FileEvent::TransferUpdated(item) = ev {
                if item.id == down_id && item.status == TransferStatus::Done {
                    download_done = true;
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(download_done, "96 KiB download must finish with Done");
    assert_eq!(
        std::fs::read(local_dst_dir.join("sample.bin")).expect("read downloaded file"),
        payload
    );

    // ── 2) W3 真实 PTY 远程终端：打开 → 执行 echo 并 exit 0 → 收到输出与退出码 ──
    #[cfg(unix)]
    {
        use rc_tools::TerminalEvent;

        let term_id = tools.terminal_open(80, 24).expect("terminal_open");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut opened = false;
        while Instant::now() < deadline && !opened {
            while let Some(ev) = session.try_recv_terminal_event() {
                if matches!(
                    ev,
                    TerminalEvent::Opened {
                        term_id: id,
                        ok: true,
                        ..
                    } if id == term_id
                ) {
                    opened = true;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(opened, "remote PTY terminal must open cleanly");

        tools
            .terminal_input(term_id, b"echo RC_UI_E2E_PTY_OK && exit 0\n")
            .expect("terminal_input");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut captured_text = String::new();
        let mut exited_code: Option<i32> = None;
        while Instant::now() < deadline
            && (exited_code.is_none() || !captured_text.contains("RC_UI_E2E_PTY_OK"))
        {
            while let Some(ev) = session.try_recv_terminal_event() {
                match ev {
                    TerminalEvent::Output {
                        term_id: id,
                        text_utf8_lossy,
                        ..
                    } if id == term_id => captured_text.push_str(&text_utf8_lossy),
                    TerminalEvent::Exited { term_id: id, code } if id == term_id => {
                        exited_code = Some(code);
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            captured_text.contains("RC_UI_E2E_PTY_OK"),
            "PTY output must contain marker, got: {captured_text:?}"
        );
        assert_eq!(exited_code, Some(0), "PTY shell must exit with code 0");
    }

    // ── 3) W4 TCP 端口映射：本地监听 → 隧道代理回环 Echo 16 KiB → 流量统计 + 删除 ──
    let echo_listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let echo_port = echo_listener.local_addr().expect("echo addr").port();
    let echo_thread = thread::spawn(move || {
        if let Ok((mut stream, _)) = echo_listener.accept() {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let local_map_port = reserve_ephemeral_tcp_port();
    let (rule, _) = tools
        .portmap_add(
            "e2e-echo".to_string(),
            local_map_port,
            "127.0.0.1".to_string(),
            echo_port,
        )
        .expect("portmap_add");
    assert_eq!(rule.status, rc_tools::PortMapStatus::Ok);

    let mut proxy_stream =
        TcpStream::connect(("127.0.0.1", local_map_port)).expect("connect mapped local port");
    proxy_stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    let echo_payload: Vec<u8> = (0..16_384usize).map(|i| (i % 239) as u8).collect();
    proxy_stream
        .write_all(&echo_payload)
        .expect("write through portmap");
    let mut echoed = vec![0u8; echo_payload.len()];
    proxy_stream
        .read_exact(&mut echoed)
        .expect("read echoed bytes through portmap");
    assert_eq!(echoed, echo_payload, "portmap TCP echo payload must match");
    drop(proxy_stream);

    // 验证流量统计已计入规则快照。
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stats_ok = false;
    while Instant::now() < deadline && !stats_ok {
        let rules = tools.portmap_list();
        if let Some(r) = rules.iter().find(|r| r.id == rule.id) {
            if r.bytes_tx >= 16_384 && r.bytes_rx >= 16_384 {
                stats_ok = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(stats_ok, "portmap rule must record >= 16 KiB tx & rx");

    let _ = tools
        .portmap_toggle(rule.id, false)
        .expect("portmap_toggle");
    let remaining = tools.portmap_delete(rule.id).expect("portmap_delete");
    assert!(remaining.is_empty());

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
    let _ = echo_thread.join();
    let _ = std::fs::remove_dir_all(&base_dir);
}

#[test]
fn inproc_p2p_split_brain_handshake_drop_falls_back_to_tcp_relay() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let token = "inproc-split-brain-2026".to_string();
    let (relay_addr, relay_thread) = spawn_local_rcr1_relay(token.clone());
    let relay_addr_str = relay_addr.to_string();

    let host_udp_port = reserve_ephemeral_udp_port();
    let client_udp_port = reserve_ephemeral_udp_port();
    assert_ne!(host_udp_port, client_udp_port);

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("SPLIT2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    host.offer_relay_incoming(RelayIncoming {
        session_id: "sess-split-brain-01".to_string(),
        relay_addr: relay_addr_str.clone(),
        relay_token: token.clone(),
        peer_id: "604928235".to_string(),
        peer_public_addr: Some(format!("127.0.0.1:{client_udp_port}")),
        peer_lan_addrs: Vec::new(),
        local_udp_port: Some(host_udp_port),
        ..RelayIncoming::default()
    });

    // 模拟分裂脑：对端与 Host 完成 P2P QUIC 传输建连后立即断开（未发送 Hello，模拟 Client P2P 刚好超时切中继）。
    let p2p_candidates = rc_ui::p2p::P2pCandidates {
        session_id: "sess-split-brain-01".to_string(),
        peer_public_addr: Some(format!("127.0.0.1:{host_udp_port}")),
        peer_lan_addrs: Vec::new(),
        local_udp_port: Some(client_udp_port),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let p2p_conn = rc_ui::p2p::attempt_p2p_quic_client(
        &p2p_candidates,
        rc_ui::p2p::DEFAULT_PUNCH_TIMEOUT,
        rc_ui::p2p::DEFAULT_QUIC_HANDSHAKE_TIMEOUT,
        &cancel,
    )
    .expect("split-brain test must first establish P2P QUIC with host");
    drop(p2p_conn);

    // 真实控制端已经回退到 TCP Relay 并使用同一中继令牌建连。
    let session = SessionHandle::spawn(make_relay_session_cfg(
        relay_addr_str,
        token,
        Some("SPLIT2026"),
    ));

    let client_updates = collect_client_until(&session, Duration::from_secs(12), |us| {
        let has_neg = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let has_hud = us
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        has_neg && has_hud
    });
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated { backend, .. } if backend.contains("中继")
        )),
        "host must fall back to TCP Relay after P2P pre-Streaming disconnect, got: {client_updates:?}"
    );
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Hud(h) if h.transport == "TCP Relay" && h.fps > 0.0
        )),
        "fallback HUD must report TCP Relay with fps > 0, got: {client_updates:?}"
    );

    let host_conn = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::ClientConnected { .. }))
    });
    assert!(
        host_conn.iter().any(|u| matches!(
            u,
            HostUpdate::ClientConnected { remote_addr } if remote_addr == "设备 604928235 (中继)"
        )),
        "host must report ClientConnected over relay after split-brain fallback, got: {host_conn:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
    relay_thread.join().expect("relay thread join");
}

#[test]
fn inproc_tools_validation_rejects_bad_terminal_args_and_duplicate_portmap_port() {
    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().expect("loopback"),
        password: Some("VALID2026".to_string()),
        approval_timeout_us: 15_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: false,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn host");

    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        password: Some("VALID2026".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 4_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        relay_grant: None,
        force_relay: false,
        ..UiSessionConfig::default()
    });

    let neg = collect_client_until(&session, Duration::from_secs(10), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(neg
        .iter()
        .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));

    let tools = session.tools();

    // 1) 终端尺寸与输入边界校验（M10）。
    let open_zero = tools.terminal_open(0, 24).unwrap_err();
    assert!(open_zero.contains("1–512"), "unexpected err: {open_zero}");
    assert!(tools.terminal_open(513, 24).is_err());

    let term_id = tools.terminal_open(80, 24).expect("valid terminal_open");
    assert!(tools.has_active_terminal(term_id));
    assert!(tools.terminal_resize(term_id, 0, 24).is_err());
    assert!(tools.terminal_resize(term_id, 80, 513).is_err());

    let big_input = vec![b'a'; 64 * 1024 + 1];
    let big_err = tools.terminal_input(term_id, &big_input).unwrap_err();
    assert!(big_err.contains("64 KiB"), "unexpected err: {big_err}");

    let missing_err = tools.terminal_input(999_999, b"echo hi\n").unwrap_err();
    assert!(
        missing_err.contains("不存在或已关闭"),
        "unexpected err: {missing_err}"
    );
    tools.terminal_close(term_id).expect("terminal_close");
    assert!(!tools.has_active_terminal(term_id));
    assert!(tools.terminal_input(term_id, b"after close\n").is_err());

    // 2) 端口映射参数与重复本地端口拒绝（M10）。
    assert!(tools
        .portmap_add(String::new(), 18080, "127.0.0.1".to_string(), 80)
        .is_err());
    assert!(tools
        .portmap_add("zero-local".to_string(), 0, "127.0.0.1".to_string(), 80)
        .is_err());
    assert!(tools
        .portmap_add("zero-target".to_string(), 18080, "127.0.0.1".to_string(), 0)
        .is_err());

    let port = reserve_ephemeral_tcp_port();
    let (rule1, _) = tools
        .portmap_add("dup-first".to_string(), port, "127.0.0.1".to_string(), 22)
        .expect("first rule on port must succeed");
    let dup_err = tools
        .portmap_add("dup-second".to_string(), port, "127.0.0.1".to_string(), 80)
        .unwrap_err();
    assert!(
        dup_err.contains("占用") || dup_err.contains("duplicate") || !dup_err.is_empty(),
        "duplicate local_port must be rejected: {dup_err}"
    );
    let _ = tools.portmap_delete(rule1.id).expect("delete rule1");

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
}
