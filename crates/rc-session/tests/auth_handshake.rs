//! T19 进程内互喂集成测试：两台纯状态机（HostAuthenticator / ClientAuthenticator）
//! 经由 ControlMessage 线格式编码/解码直接对话，零 IO 覆盖完整握手决策表与 A1–A11 不变式。

use std::net::SocketAddr;

use rc_session::auth::{
    AutoApprover, ClientAuthAction, ClientAuthConfig, ClientAuthenticator, ConnectionApprover,
    HostAuthAction, HostAuthConfig, HostAuthenticator, DEFAULT_AUTH_TIMEOUT_US,
};
use rc_session::proto::{
    AuthStatus, ControlMessage, Hello, HostFlags, SessionParams, Welcome, PROTOCOL_VERSION,
};

fn sample_addr() -> SocketAddr {
    "127.0.0.1:40001".parse().unwrap()
}

fn sample_params() -> SessionParams {
    SessionParams {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 8_000_000,
    }
}

fn make_welcome(session_id: u64, auth_status: AuthStatus) -> Welcome {
    Welcome {
        session_id,
        granted: sample_params(),
        auth_status,
        host_flags: HostFlags(0),
    }
}

/// 经过线格式 encode + decode 往返一条 ControlMessage，确保测试同时验证线格式。
fn wire_roundtrip(msg: ControlMessage) -> ControlMessage {
    let mut buf = Vec::new();
    msg.encode(&mut buf);
    ControlMessage::decode(&buf).expect("wire roundtrip must succeed")
}

/// 驱动 host 侧动作翻译为线上发送列表（完全模拟 HostSession 的消息翻译）。
fn translate_host_action(
    action: HostAuthAction,
    session_id: u64,
    host_sent: &mut Vec<ControlMessage>,
) {
    match action {
        HostAuthAction::NotifyPending => {
            host_sent.push(wire_roundtrip(ControlMessage::Welcome(make_welcome(
                session_id,
                AuthStatus::ChallengeRequired,
            ))));
        }
        HostAuthAction::Challenge(ch) => {
            host_sent.push(wire_roundtrip(ControlMessage::Welcome(make_welcome(
                session_id,
                AuthStatus::ChallengeRequired,
            ))));
            host_sent.push(wire_roundtrip(ControlMessage::AuthChallenge(ch)));
        }
        HostAuthAction::Conclude(status) => {
            host_sent.push(wire_roundtrip(ControlMessage::Welcome(make_welcome(
                session_id, status,
            ))));
        }
        HostAuthAction::Wait => {}
    }
}

/// 进程内互喂一次完整握手，返回 (host 发出消息序列, client 终局动作, host 是否终局)。
fn run_inproc_handshake(
    host_cfg: HostAuthConfig,
    client_cfg: ClientAuthConfig,
    approver: &mut dyn ConnectionApprover,
) -> (Vec<ControlMessage>, ClientAuthAction, bool) {
    let session_id = 0x1122_3344_5566_7788u64;
    let mut host = HostAuthenticator::new(host_cfg);
    let mut client = ClientAuthenticator::new(client_cfg);

    let (auth_method, auth_payload) = client.hello_auth();
    client.on_hello_sent(0);

    let hello = match wire_roundtrip(ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        session_id,
        requested: sample_params(),
        auth_method,
        auth_payload,
    })) {
        ControlMessage::Hello(h) => h,
        other => panic!("expected Hello, got {other:?}"),
    };

    let mut host_sent = Vec::new();
    let act = host.on_hello(&hello, sample_addr());
    translate_host_action(act, session_id, &mut host_sent);

    let mut cursor = 0usize;
    let mut now_us = 1_000u64;

    for _ in 0..32 {
        // 1. 若 host 尚未终局，轮询审批器
        if !host.concluded() {
            let poll_act = host.poll_approval(now_us, approver);
            translate_host_action(poll_act, session_id, &mut host_sent);
            now_us += 1_000;
        }

        // 2. 把 host 新发出的线上消息逐条喂给 client
        while cursor < host_sent.len() {
            let msg = host_sent[cursor].clone();
            cursor += 1;
            match msg {
                ControlMessage::Welcome(w) => {
                    let act = client.on_welcome(&w);
                    if matches!(act, ClientAuthAction::Done | ClientAuthAction::Failed(_)) {
                        return (host_sent, act, host.concluded());
                    }
                }
                ControlMessage::AuthChallenge(ch) => match client.on_challenge(&ch) {
                    ClientAuthAction::Respond(resp) => {
                        let resp = match wire_roundtrip(ControlMessage::AuthResponse(resp)) {
                            ControlMessage::AuthResponse(r) => r,
                            other => panic!("expected AuthResponse, got {other:?}"),
                        };
                        let h_act = host.on_auth_response(&resp);
                        translate_host_action(h_act, session_id, &mut host_sent);
                    }
                    act @ ClientAuthAction::Failed(_) => {
                        return (host_sent, act, host.concluded());
                    }
                    other => panic!("unexpected client action on challenge: {other:?}"),
                },
                other => panic!("unexpected non-auth message during handshake: {other:?}"),
            }
        }
    }

    panic!("in-process handshake did not conclude within 32 rounds");
}

#[test]
fn inproc_password_handshake_completes() {
    let mut approver = AutoApprover;
    let (host_sent, client_final, host_concluded) = run_inproc_handshake(
        HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        },
        ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        },
        &mut approver,
    );

    assert!(host_concluded);
    assert_eq!(client_final, ClientAuthAction::Done);

    // 完整消息序列恰与握手时序图一致：
    // [Welcome{ChallengeRequired}, AuthChallenge, Welcome{Ok}]
    assert_eq!(host_sent.len(), 3);
    assert!(matches!(
        host_sent[0],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::ChallengeRequired,
            ..
        })
    ));
    assert!(matches!(host_sent[1], ControlMessage::AuthChallenge(_)));
    assert!(matches!(
        host_sent[2],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::Ok,
            ..
        })
    ));
}

#[test]
fn inproc_wrong_password_rejected() {
    let mut approver = AutoApprover;
    let (host_sent, client_final, host_concluded) = run_inproc_handshake(
        HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        },
        ClientAuthConfig {
            password: Some("wrong-password".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        },
        &mut approver,
    );

    assert!(host_concluded);
    assert!(matches!(client_final, ClientAuthAction::Failed(_)));

    // 终局为 Welcome{Rejected}，且全程仅有鉴权握手消息、无任何媒体/输入控制消息（A6）
    assert_eq!(host_sent.len(), 3);
    assert!(matches!(
        host_sent[0],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::ChallengeRequired,
            ..
        })
    ));
    assert!(matches!(host_sent[1], ControlMessage::AuthChallenge(_)));
    assert!(matches!(
        host_sent[2],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::Rejected,
            ..
        })
    ));
}

#[test]
fn inproc_method_mismatch_both_directions_rejected() {
    // 行 2：host 无密码，client 带密码 → Rejected
    let mut approver_a = AutoApprover;
    let (sent_a, final_a, concluded_a) = run_inproc_handshake(
        HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        },
        ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        },
        &mut approver_a,
    );
    assert!(concluded_a);
    assert!(matches!(final_a, ClientAuthAction::Failed(_)));
    assert_eq!(sent_a.len(), 1);
    assert!(matches!(
        sent_a[0],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::Rejected,
            ..
        })
    ));

    // 行 3：host 有密码，client 无密码 → Rejected
    let mut approver_b = AutoApprover;
    let (sent_b, final_b, concluded_b) = run_inproc_handshake(
        HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        },
        ClientAuthConfig {
            password: None,
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        },
        &mut approver_b,
    );
    assert!(concluded_b);
    assert!(matches!(final_b, ClientAuthAction::Failed(_)));
    assert_eq!(sent_b.len(), 1);
    assert!(matches!(
        sent_b[0],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::Rejected,
            ..
        })
    ));
}

#[test]
fn inproc_replayed_proof_fails_on_second_connection() {
    let cfg = HostAuthConfig {
        password: Some("hunter2".into()),
        iterations: 64,
        approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
        ..HostAuthConfig::default()
    };
    let mut host = HostAuthenticator::new(cfg);
    let mut client1 = ClientAuthenticator::new(ClientAuthConfig {
        password: Some("hunter2".into()),
        auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
        ..ClientAuthConfig::default()
    });

    // 第一次连接：拿到合法 AuthResponse
    let (m1, p1) = client1.hello_auth();
    let hello1 = Hello {
        version: PROTOCOL_VERSION,
        session_id: 1,
        requested: sample_params(),
        auth_method: m1,
        auth_payload: p1,
    };
    let ch1 = match host.on_hello(&hello1, sample_addr()) {
        HostAuthAction::Challenge(ch) => ch,
        other => panic!("expected Challenge, got {other:?}"),
    };
    assert_eq!(
        client1.on_welcome(&make_welcome(1, AuthStatus::ChallengeRequired)),
        ClientAuthAction::Waiting
    );
    let captured_resp = match client1.on_challenge(&ch1) {
        ClientAuthAction::Respond(r) => r,
        other => panic!("expected Respond, got {other:?}"),
    };
    assert_eq!(host.on_auth_response(&captured_resp), HostAuthAction::Wait);
    let mut approver = AutoApprover;
    assert_eq!(
        host.poll_approval(1_000, &mut approver),
        HostAuthAction::Conclude(AuthStatus::Ok)
    );

    // 第二次连接（或重连）：host 颁布新盐，重放旧 proof 必被拒（A2）
    let hello2 = Hello {
        version: PROTOCOL_VERSION,
        session_id: 2,
        requested: sample_params(),
        auth_method: m1,
        auth_payload: Vec::new(),
    };
    let ch2 = match host.on_hello(&hello2, sample_addr()) {
        HostAuthAction::Challenge(ch) => ch,
        other => panic!("expected Challenge, got {other:?}"),
    };
    assert_ne!(ch1.salt, ch2.salt, "second handshake must use a fresh salt");
    assert_eq!(
        host.on_auth_response(&captured_resp),
        HostAuthAction::Conclude(AuthStatus::Rejected)
    );
    assert!(host.concluded());
}

#[test]
fn inproc_no_password_matches_t14_shape() {
    let mut approver = AutoApprover;
    let (host_sent, client_final, host_concluded) = run_inproc_handshake(
        HostAuthConfig::default(),
        ClientAuthConfig::default(),
        &mut approver,
    );

    assert!(host_concluded);
    assert_eq!(client_final, ClientAuthAction::Done);
    // 无密码 + AutoApprover：host 对外消息恰 [Welcome{Ok}] 一条（T14 兼容不变式）
    assert_eq!(host_sent.len(), 1);
    assert!(matches!(
        host_sent[0],
        ControlMessage::Welcome(Welcome {
            auth_status: AuthStatus::Ok,
            ..
        })
    ));
}

// ─────────────────────────────────────────────────────────────────
// 真 QUIC localhost（127.0.0.1）会话级鉴权集成测试（Phase 5）
// ─────────────────────────────────────────────────────────────────

mod quic_session_auth {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use rc_pipeline::{DiffMode, PipelineConfig};
    use rc_session::auth::{
        ApprovalRequest, ApprovalVerdict, AutoApprover, ClientAuthConfig, ConnectionApprover,
        DenyApprover, HostAuthConfig, ScriptedApprover, DEFAULT_AUTH_TIMEOUT_US,
    };
    use rc_session::proto::{ByeReason, SessionParams};
    use rc_session::testkit::{RecordingInjector, ScriptedCapture};
    use rc_session::{
        ClientSession, ClientSessionConfig, HostSession, HostSessionConfig, ReconnectConfig,
        SessionClock, SessionState,
    };
    use rc_types::Resolution;

    const SERVER_CERT_PEM: &str = include_str!("certs/server-cert.pem");
    const SERVER_KEY_PEM: &str = include_str!("certs/server-key.pem");
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

    fn params() -> SessionParams {
        SessionParams {
            width: 64,
            height: 64,
            fps: 60,
            bitrate_bps: 2_000_000,
        }
    }

    fn pcfg() -> PipelineConfig {
        PipelineConfig {
            resolution: Resolution::new(64, 64),
            fps: 60,
            bitrate_bps: 2_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 1024,
            diff: DiffMode::TrustPlatform,
            clock_origin_us: None,
            max_packet_payload: rc_pipeline::MAX_PAYLOAD,
        }
    }

    type OriginSlot = Arc<Mutex<Option<Instant>>>;

    fn origin_clock(origin: OriginSlot) -> SessionClock {
        Arc::new(move || match *origin.lock().expect("origin lock") {
            Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
            None => 0,
        })
    }

    fn build_host(
        auth: HostAuthConfig,
        approver: Box<dyn ConnectionApprover>,
        pipelines_built: Arc<AtomicUsize>,
        origin: OriginSlot,
    ) -> HostSession {
        let clock = origin_clock(Arc::clone(&origin));
        let mut host = HostSession::new(
            HostSessionConfig {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                mtu: rc_session::DEFAULT_SESSION_MTU,
                caps: params(),
                pipeline: pcfg(),
                handshake_timeout: Duration::from_secs(5),
                injector: Box::new(RecordingInjector::new()),
                pace_capture: true,
                auth,
                approver,
            },
            SERVER_CERT_PEM,
            SERVER_KEY_PEM,
            clock,
        )
        .expect("host session");

        let factory_origin = Arc::clone(&origin);
        host.set_pipeline_factory_for(Box::new(move |pcfg| {
            pipelines_built.fetch_add(1, Ordering::SeqCst);
            *factory_origin.lock().expect("origin lock") = Some(Instant::now());
            let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
                display: rc_capture::DisplayInfo {
                    id: rc_types::DisplayId(0),
                    resolution: pcfg.resolution,
                    scale: 1.0,
                    hdr: false,
                },
                fps: pcfg.fps,
                content: rc_capture::FakeContent::Dynamic,
                dirty_rect: None,
            })?;
            let encoder = rc_encode::FakeEncoder::with_tuning(
                rc_encode::EncoderConfig {
                    resolution: pcfg.resolution,
                    fps: pcfg.fps,
                    bitrate_bps: pcfg.bitrate_bps,
                    ..rc_encode::EncoderConfig::default()
                },
                500,
                512,
            )?;
            rc_pipeline::HostPipeline::with_backends_paced(
                pcfg.clone(),
                Box::new(capture),
                Box::new(encoder),
                true,
            )
        }));
        host
    }

    fn build_client(
        host_addr: SocketAddr,
        session_id: u64,
        auth: ClientAuthConfig,
        pipelines_built: Arc<AtomicUsize>,
        clock: SessionClock,
    ) -> ClientSession {
        let mut client = ClientSession::new(
            ClientSessionConfig {
                peer_addr: host_addr,
                mtu: rc_session::DEFAULT_SESSION_MTU,
                requested: params(),
                session_id,
                pipeline: pcfg(),
                connect_timeout: CONNECT_TIMEOUT,
                handshake_timeout: Duration::from_secs(5),
                capture: Box::new(ScriptedCapture::standard()),
                input_window: rc_input::DEFAULT_WINDOW,
                auth,
            },
            SERVER_CERT_PEM,
            SERVER_KEY_PEM,
            SERVER_CERT_PEM,
            "rabbitcontrol",
            clock,
        )
        .expect("client session");

        client.set_pipeline_factory_for(Box::new(move |pcfg| {
            pipelines_built.fetch_add(1, Ordering::SeqCst);
            let decoder = rc_decode::FakeDecoder::new(rc_decode::FakeDecoderConfig {
                resolution: pcfg.resolution,
                ..rc_decode::FakeDecoderConfig::default()
            })?;
            let renderer = rc_render::FakeRenderer::new(rc_render::FakeRendererConfig {
                size: (pcfg.resolution.width, pcfg.resolution.height),
                ..rc_render::FakeRendererConfig::default()
            })?;
            rc_pipeline::ControllerPipeline::with_backends(
                pcfg.clone(),
                Box::new(decoder),
                Box::new(renderer),
            )
        }));
        client
    }

    fn wall_clock() -> SessionClock {
        let t0 = Instant::now();
        Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX))
    }

    /// 记录审批请求并在每次提交时自动接受的测试审批人（用于校验重建也经过完整密码+审批流程）。
    struct RecordingAutoApprover {
        requests: Arc<Mutex<Vec<ApprovalRequest>>>,
    }

    impl ConnectionApprover for RecordingAutoApprover {
        fn submit(&mut self, req: &ApprovalRequest) {
            self.requests
                .lock()
                .expect("requests lock")
                .push(req.clone());
        }

        fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
            Some(ApprovalVerdict::Accepted)
        }

        fn cancel(&mut self) {}
    }

    /// (a) 正确密码：QUIC localhost 握手通过并正常流帧。
    #[test]
    fn quic_password_handshake_streams_frames() {
        let host_built = Arc::new(AtomicUsize::new(0));
        let client_built = Arc::new(AtomicUsize::new(0));
        let origin: OriginSlot = Arc::new(Mutex::new(None));

        let mut host = build_host(
            HostAuthConfig {
                password: Some("hunter2".into()),
                iterations: 64,
                approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..HostAuthConfig::default()
            },
            Box::new(AutoApprover),
            Arc::clone(&host_built),
            origin,
        );
        let host_addr = host.local_addr().expect("host addr");

        let host_thread = thread::spawn(move || {
            host.accept_and_handshake(CONNECT_TIMEOUT)
                .expect("host handshake with matching password must succeed");
            let started = Instant::now();
            host.run(&mut || started.elapsed() >= Duration::from_secs(3))
                .expect("host run")
        });

        let clock = wall_clock();
        let mut client = build_client(
            host_addr,
            10_001,
            ClientAuthConfig {
                password: Some("hunter2".into()),
                auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..ClientAuthConfig::default()
            },
            Arc::clone(&client_built),
            Arc::clone(&clock),
        );
        client
            .connect_and_handshake()
            .expect("client handshake with matching password must succeed");
        assert_eq!(client.state(), SessionState::Streaming);

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && client.pipeline().displayed() < 3 {
            let now_us = u64::from((clock)());
            client.pump_probes(now_us).expect("pump_probes");
            let r = client
                .tick(Duration::from_millis(5), Instant::now())
                .expect("client tick");
            if r.closed.is_some() {
                break;
            }
        }
        let displayed = client.pipeline().displayed();
        client.close(ByeReason::Normal).expect("client close");

        let host_summary = host_thread.join().expect("host thread");
        assert!(
            displayed >= 3,
            "密码通过后 client 应上屏至少 3 帧，实际 {displayed}"
        );
        assert!(
            host_summary.frames_encoded >= 1,
            "密码通过后 host 应编码至少 1 帧，实际 {}",
            host_summary.frames_encoded
        );
        assert_eq!(host_built.load(Ordering::SeqCst), 1);
        assert_eq!(client_built.load(Ordering::SeqCst), 1);
    }

    /// (b) 错误密码：client 干净失败，host 拒绝断开，0 帧编码 / 0 帧渲染（A6）。
    #[test]
    fn quic_wrong_password_rejected_zero_frames() {
        let host_built = Arc::new(AtomicUsize::new(0));
        let client_built = Arc::new(AtomicUsize::new(0));
        let origin: OriginSlot = Arc::new(Mutex::new(None));

        let mut host = build_host(
            HostAuthConfig {
                password: Some("hunter2".into()),
                iterations: 64,
                approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..HostAuthConfig::default()
            },
            Box::new(AutoApprover),
            Arc::clone(&host_built),
            origin,
        );
        let host_addr = host.local_addr().expect("host addr");

        let host_thread = thread::spawn(move || {
            let res = host.accept_and_handshake(CONNECT_TIMEOUT);
            (res, host.state())
        });

        let clock = wall_clock();
        let mut client = build_client(
            host_addr,
            10_002,
            ClientAuthConfig {
                password: Some("wrong-password".into()),
                auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..ClientAuthConfig::default()
            },
            Arc::clone(&client_built),
            clock,
        );
        let client_res = client.connect_and_handshake();
        let (host_res, host_state) = host_thread.join().expect("host thread");

        assert!(
            client_res.is_err(),
            "错误密码时 client 握手必须失败，实际 {client_res:?}"
        );
        assert!(
            host_res.is_err(),
            "错误密码时 host 握手必须拒绝，实际 {host_res:?}"
        );
        assert_eq!(client.state(), SessionState::Closed);
        assert_eq!(host_state, SessionState::Closed);
        // A6：未通过鉴权绝不建管线，0 帧编码 / 0 帧渲染。
        assert_eq!(
            host_built.load(Ordering::SeqCst),
            0,
            "错误密码时 host 不得构造采集/编码管线（A6）"
        );
        assert_eq!(
            client_built.load(Ordering::SeqCst),
            0,
            "错误密码时 client 不得构造解码/渲染管线（A6）"
        );
    }

    /// (c) 审批器联动：`ScriptedApprover`（多次 Pending 后接受）通过并流帧；
    /// `DenyApprover` 拒绝且 0 帧编码/渲染（A6）。
    #[test]
    fn quic_approver_scripted_accept_and_deny_reject() {
        // ── 子用例 1：ScriptedApprover(Pending x3 -> Accepted) ──
        {
            let host_built = Arc::new(AtomicUsize::new(0));
            let client_built = Arc::new(AtomicUsize::new(0));
            let origin: OriginSlot = Arc::new(Mutex::new(None));

            let approver =
                ScriptedApprover::new(vec![None, None, None, Some(ApprovalVerdict::Accepted)]);
            let mut host = build_host(
                HostAuthConfig {
                    password: Some("hunter2".into()),
                    iterations: 64,
                    approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                    ..HostAuthConfig::default()
                },
                Box::new(approver),
                Arc::clone(&host_built),
                origin,
            );
            let host_addr = host.local_addr().expect("host addr");

            let host_thread = thread::spawn(move || {
                host.accept_and_handshake(CONNECT_TIMEOUT)
                    .expect("ScriptedApprover 延迟接受后 host 必须完成握手");
                let started = Instant::now();
                host.run(&mut || started.elapsed() >= Duration::from_secs(3))
                    .expect("host run")
            });

            let clock = wall_clock();
            let mut client = build_client(
                host_addr,
                10_003,
                ClientAuthConfig {
                    password: Some("hunter2".into()),
                    auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                    ..ClientAuthConfig::default()
                },
                Arc::clone(&client_built),
                Arc::clone(&clock),
            );
            client
                .connect_and_handshake()
                .expect("ScriptedApprover 延迟接受后 client 必须完成握手");

            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && client.pipeline().displayed() < 2 {
                let now_us = u64::from((clock)());
                client.pump_probes(now_us).expect("pump_probes");
                let r = client
                    .tick(Duration::from_millis(5), Instant::now())
                    .expect("client tick");
                if r.closed.is_some() {
                    break;
                }
            }
            let displayed = client.pipeline().displayed();
            client.close(ByeReason::Normal).expect("client close");
            let host_summary = host_thread.join().expect("host thread");

            assert!(displayed >= 2, "审批通过后应正常上屏，实际 {displayed}");
            assert!(host_summary.frames_encoded >= 1);
            assert_eq!(host_built.load(Ordering::SeqCst), 1);
            assert_eq!(client_built.load(Ordering::SeqCst), 1);
        }

        // ── 子用例 2：DenyApprover 拒绝（密码正确但人工拒绝）──
        {
            let host_built = Arc::new(AtomicUsize::new(0));
            let client_built = Arc::new(AtomicUsize::new(0));
            let origin: OriginSlot = Arc::new(Mutex::new(None));

            let mut host = build_host(
                HostAuthConfig {
                    password: Some("hunter2".into()),
                    iterations: 64,
                    approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                    ..HostAuthConfig::default()
                },
                Box::new(DenyApprover),
                Arc::clone(&host_built),
                origin,
            );
            let host_addr = host.local_addr().expect("host addr");

            let host_thread = thread::spawn(move || {
                let res = host.accept_and_handshake(CONNECT_TIMEOUT);
                (res, host.state())
            });

            let clock = wall_clock();
            let mut client = build_client(
                host_addr,
                10_004,
                ClientAuthConfig {
                    password: Some("hunter2".into()),
                    auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                    ..ClientAuthConfig::default()
                },
                Arc::clone(&client_built),
                clock,
            );
            let client_res = client.connect_and_handshake();
            let (host_res, host_state) = host_thread.join().expect("host thread");

            assert!(client_res.is_err(), "DenyApprover 拒绝后 client 必须失败");
            assert!(host_res.is_err(), "DenyApprover 拒绝后 host 必须失败");
            assert_eq!(client.state(), SessionState::Closed);
            assert_eq!(host_state, SessionState::Closed);
            assert_eq!(host_built.load(Ordering::SeqCst), 0);
            assert_eq!(client_built.load(Ordering::SeqCst), 0);
        }
    }

    /// (d) 密码会话重连（Bye(Refresh) 全新重建）：使用全新盐再次完成挑战应答并继续流帧。
    #[test]
    fn quic_password_reconnect_uses_fresh_salt_and_succeeds() {
        let host_built = Arc::new(AtomicUsize::new(0));
        let client_built = Arc::new(AtomicUsize::new(0));
        let origin: OriginSlot = Arc::new(Mutex::new(None));
        let approval_reqs: Arc<Mutex<Vec<ApprovalRequest>>> = Arc::new(Mutex::new(Vec::new()));

        let mut host = build_host(
            HostAuthConfig {
                password: Some("hunter2".into()),
                iterations: 64,
                approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..HostAuthConfig::default()
            },
            Box::new(RecordingAutoApprover {
                requests: Arc::clone(&approval_reqs),
            }),
            Arc::clone(&host_built),
            Arc::clone(&origin),
        );
        // 缩短时钟饱和守卫线至 1.5s，触发 Bye(Refresh) 全新重建（走完整新 Hello + 新盐密码挑战）。
        host.set_reconnect(ReconnectConfig {
            refresh_guard_us: 1_500_000,
            ..ReconnectConfig::default()
        })
        .expect("set_reconnect");
        let host_addr = host.local_addr().expect("host addr");

        let host_clock = origin_clock(Arc::clone(&origin));
        let client_origin: OriginSlot = Arc::new(Mutex::new(Some(Instant::now())));
        let client_clock = origin_clock(Arc::clone(&client_origin));

        // 阶段 1：初始密码握手（双线程避免阻塞互等）
        let mut client = build_client(
            host_addr,
            20_001,
            ClientAuthConfig {
                password: Some("hunter2".into()),
                auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                ..ClientAuthConfig::default()
            },
            Arc::clone(&client_built),
            Arc::clone(&client_clock),
        );
        let h_init = thread::spawn(move || {
            host.accept_and_handshake(CONNECT_TIMEOUT)
                .expect("initial password handshake");
            host
        });
        client
            .connect_and_handshake()
            .expect("client initial password handshake");
        let host = h_init.join().expect("host init thread");
        let old_sid = client.session_id();

        // 阶段 2：驱动双端直至触发 Bye(Refresh) 并完成带密码鉴权的全新重连
        let stop = Arc::new(AtomicBool::new(false));
        let shared_host = Arc::new(Mutex::new(host));
        let host_driver = {
            let shared = Arc::clone(&shared_host);
            let stop = Arc::clone(&stop);
            let clock = Arc::clone(&host_clock);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut h = shared.lock().expect("host lock");
                    match h.state() {
                        SessionState::Streaming => {
                            if h.tick(Duration::from_millis(1)).is_err() {
                                break;
                            }
                        }
                        SessionState::Detached => {
                            if h.tick_detached(u64::from((clock)())).is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                    drop(h);
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };

        let mut refreshed = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let now_us = u64::from((client_clock)());
            match client.state() {
                SessionState::Streaming => {
                    client.pump_probes(now_us).expect("pump_probes");
                    client
                        .tick(Duration::from_millis(1), Instant::now())
                        .expect("client tick");
                }
                SessionState::Reconnecting => {
                    client.tick_reconnect(now_us).expect("tick_reconnect");
                    thread::sleep(Duration::from_millis(1));
                }
                _ => break,
            }
            let host_refreshed = shared_host
                .lock()
                .expect("host lock")
                .reconnect_stats()
                .refreshes
                >= 1;
            if !refreshed && host_refreshed {
                refreshed = true;
                *origin.lock().expect("origin lock") = Some(Instant::now());
                *client_origin.lock().expect("client origin lock") = Some(Instant::now());
            }
            if refreshed && client.reconnect_stats().rebuilds >= 1 {
                break;
            }
        }

        assert!(refreshed, "refresh_guard 必须触发 Bye(Refresh)");
        assert_eq!(
            client.reconnect_stats().rebuilds,
            1,
            "密码会话重连必须完成 1 次全新重建"
        );
        assert_eq!(client.state(), SessionState::Streaming);
        let new_sid = client.session_id();
        assert_ne!(new_sid, old_sid, "重建后必须换新 session_id");

        // 阶段 3：重连后持续流帧
        let post_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < post_deadline && client.pipeline().displayed() < 2 {
            let now_us = u64::from((client_clock)());
            client.pump_probes(now_us).expect("pump_probes");
            client
                .tick(Duration::from_millis(2), Instant::now())
                .expect("post-reconnect tick");
        }
        assert!(
            client.pipeline().displayed() >= 2,
            "重连后应继续上屏，实际 {}",
            client.pipeline().displayed()
        );

        // 校验两次握手均经由完整密码挑战 + 审批（每次新 HostAuthenticator 颁布新盐）。
        {
            let reqs = approval_reqs.lock().expect("requests lock");
            assert_eq!(
                reqs.len(),
                2,
                "初始握手与重连各触发一次审批，实际 {}",
                reqs.len()
            );
            assert!(reqs[0].password_verified, "初始握手必须通过密码校验");
            assert!(reqs[1].password_verified, "重连握手必须再次通过密码校验");
            assert_eq!(reqs[0].session_id, old_sid);
            assert_eq!(reqs[1].session_id, new_sid);
        }

        client.close(ByeReason::Normal).expect("client close");
        stop.store(true, Ordering::Relaxed);
        host_driver.join().expect("host driver join");
    }
}
