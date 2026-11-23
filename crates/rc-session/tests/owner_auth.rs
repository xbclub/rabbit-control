//! 自控免密签名票据（`OwnerTicket` PoP）与协助门禁集成测试。
//!
//! 使用测试专用轻量签名替身（无第三方密码库依赖），覆盖：
//! - 合法 `OwnerTicket` + PoP 在 `allow_remote_assist = false`、`password = Some(...)`、
//!   `approver = DenyApprover` 下免密免审批建立会话（含 `authenticated_owner()` 上报）；
//! - `allow_remote_assist = false` 时拒绝 `AuthMethod::None` 与 `AuthMethod::Password`（含正确密码）；
//! - 篡改票据、过期票据、重放同 `ticket_id`、伪造 PoP 签名、错 `session_id` 全部被拒绝；
//! - `Detached` 期间拒绝未鉴权快速 `ReconnectRequest` 与 `ticket_id` 重放，允许新票据替换会话；
//! - `Streaming` 期收到 `OwnerAuthResponse` 按 A11 致命断开；
//! - 真 QUIC localhost（127.0.0.1）全链路 `OwnerTicket` 握手与流帧。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{EncoderConfig, FakeEncoder};
use rc_input::{InputCapture, InputEvent, InputInjector};
use rc_net::{NetEvent, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::auth::{
    ApprovalRequest, ApprovalVerdict, AutoApprover, ClientAuthAction, ClientAuthConfig,
    ClientAuthenticator, ConnectionApprover, DenyApprover, HostAuthAction, HostAuthConfig,
    HostAuthenticator, OwnerTicketSigner, OwnerTicketVerifier, VerifiedOwnerTicket,
    DEFAULT_AUTH_TIMEOUT_US,
};
use rc_session::proto::{
    AuthMethod, AuthStatus, ByeReason, ControlMessage, Hello, SessionParams, AUTH_SALT_LEN,
    PROTOCOL_VERSION,
};
use rc_session::testkit::{pair, MockTransport, RecordingInjector, ScriptedCapture};
use rc_session::{
    ClientSession, ClientSessionConfig, DetachOutcome, HostSession, HostSessionConfig,
    SessionClock, SessionState,
};
use rc_types::{Error, Resolution, Result};

const SERVER_CERT_PEM: &str = include_str!("certs/server-cert.pem");
const SERVER_KEY_PEM: &str = include_str!("certs/server-key.pem");
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 轻量测试替身：载荷格式为 `ticket_id|user_id|caller_id|pubkey|target_id|session_str|exp|mac`
/// PoP 签名为确定性 FNV/XOR 混合摘要（32 字节），绑定 `(secret_key, caller_pubkey_hex, hello_session_id, salt)`。
const TICKET_SIGNING_SECRET: &[u8] = b"test-server-ticket-secret";

fn toy_mac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut state = [0x5Au8; 32];
    for (i, &b) in key.iter().enumerate() {
        state[i % 32] ^= b.wrapping_add((i as u8).wrapping_mul(17));
        state[(i + 7) % 32] = state[(i + 7) % 32].wrapping_add(b.rotate_left(3));
    }
    let mut step = 0usize;
    for part in parts {
        for &b in *part {
            let idx = step % 32;
            state[idx] = state[idx].rotate_left(5).wrapping_add(b) ^ state[(idx + 13) % 32];
            state[(idx + 1) % 32] = state[(idx + 1) % 32].wrapping_mul(33).wrapping_add(b);
            step += 1;
        }
        state[step % 32] ^= 0xA5;
        step += 1;
    }
    state
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Clone)]
struct TestTicketSpec {
    ticket_id: String,
    user_id: String,
    caller_device_id: String,
    caller_pubkey_hex: String,
    target_device_id: String,
    session_id_str: String,
    expires_at_unix_sec: u64,
    caller_priv_secret: Vec<u8>,
}

impl TestTicketSpec {
    fn encode_signed_payload(&self) -> Vec<u8> {
        let body = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.ticket_id,
            self.user_id,
            self.caller_device_id,
            self.caller_pubkey_hex,
            self.target_device_id,
            self.session_id_str,
            self.expires_at_unix_sec
        );
        let mac = toy_mac(TICKET_SIGNING_SECRET, &[body.as_bytes()]);
        format!("{body}|{}", hex_encode(&mac)).into_bytes()
    }
}

struct TestOwnerSigner {
    payload: Vec<u8>,
    caller_priv_secret: Vec<u8>,
    override_session_id: Option<u64>,
}

impl TestOwnerSigner {
    fn from_spec(spec: &TestTicketSpec) -> Self {
        Self {
            payload: spec.encode_signed_payload(),
            caller_priv_secret: spec.caller_priv_secret.clone(),
            override_session_id: None,
        }
    }
}

impl OwnerTicketSigner for TestOwnerSigner {
    fn ticket_payload(&self) -> Vec<u8> {
        self.payload.clone()
    }

    fn sign_challenge(&self, hello_session_id: u64, salt: &[u8; AUTH_SALT_LEN]) -> Result<Vec<u8>> {
        let sid = self.override_session_id.unwrap_or(hello_session_id);
        let sig = toy_mac(&self.caller_priv_secret, &[&sid.to_le_bytes(), salt]);
        Ok(sig.to_vec())
    }
}

struct TestOwnerVerifier {
    expected_target_device_id: String,
}

impl OwnerTicketVerifier for TestOwnerVerifier {
    fn verify_ticket(&self, payload: &[u8], now_unix_sec: u64) -> Result<VerifiedOwnerTicket> {
        let raw = std::str::from_utf8(payload).map_err(|_| Error::Protocol {
            offset: 0,
            msg: "ticket payload is not utf8".into(),
        })?;
        let parts: Vec<&str> = raw.split('|').collect();
        if parts.len() != 8 {
            return Err(Error::Protocol {
                offset: 0,
                msg: "invalid ticket segment count".into(),
            });
        }
        let body = parts[..7].join("|");
        let expected_mac = hex_encode(&toy_mac(TICKET_SIGNING_SECRET, &[body.as_bytes()]));
        if parts[7] != expected_mac {
            return Err(Error::Protocol {
                offset: 0,
                msg: "ticket signature tampered".into(),
            });
        }
        let expires_at: u64 = parts[6].parse().map_err(|_| Error::Protocol {
            offset: 0,
            msg: "invalid expires_at".into(),
        })?;
        if expires_at <= now_unix_sec {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!("ticket expired at {expires_at} <= {now_unix_sec}"),
            });
        }
        if parts[4] != self.expected_target_device_id {
            return Err(Error::Protocol {
                offset: 0,
                msg: "target_device_id mismatch".into(),
            });
        }
        Ok(VerifiedOwnerTicket {
            ticket_id: parts[0].to_owned(),
            user_id: parts[1].to_owned(),
            caller_device_id: parts[2].to_owned(),
            caller_pubkey_hex: parts[3].to_owned(),
            target_device_id: parts[4].to_owned(),
            session_id_str: parts[5].to_owned(),
            expires_at_unix_sec: expires_at,
        })
    }

    fn verify_challenge_signature(
        &self,
        ticket: &VerifiedOwnerTicket,
        hello_session_id: u64,
        salt: &[u8; AUTH_SALT_LEN],
        signature: &[u8],
    ) -> Result<()> {
        // 测试约定：caller_pubkey_hex = "pub:<secret>"，以便在不引入非对称密码库的前提下校验 PoP 绑定公钥。
        let expected_secret = ticket
            .caller_pubkey_hex
            .strip_prefix("pub:")
            .ok_or_else(|| Error::Protocol {
                offset: 0,
                msg: "bad caller_pubkey_hex".into(),
            })?;
        let expected = toy_mac(
            expected_secret.as_bytes(),
            &[&hello_session_id.to_le_bytes(), salt],
        );
        if signature == expected {
            Ok(())
        } else {
            Err(Error::Protocol {
                offset: 0,
                msg: "PoP signature mismatch".into(),
            })
        }
    }
}

struct SpyDenyApprover {
    submit_called: Arc<AtomicBool>,
}

impl ConnectionApprover for SpyDenyApprover {
    fn submit(&mut self, _req: &ApprovalRequest) {
        self.submit_called.store(true, Ordering::SeqCst);
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        Some(ApprovalVerdict::Rejected)
    }

    fn cancel(&mut self) {}
}

struct YieldingTransport(MockTransport);

impl Transport for YieldingTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        self.0.send_datagram(data)
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        self.0.send_reliable(data)
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>> {
        let evs = self.0.poll(timeout)?;
        if evs.is_empty() && !timeout.is_zero() {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(evs)
    }

    fn stats(&self) -> rc_net::NetStats {
        self.0.stats()
    }
}

struct NoopInjector;
impl InputInjector for NoopInjector {
    fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
        Ok(())
    }
    fn set_screen_size(&mut self, _w: u32, _h: u32) {}
}

struct EmptyCapture;
impl InputCapture for EmptyCapture {
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

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
        trace_capacity: 256,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

fn zero_clock() -> SessionClock {
    Arc::new(|| 0)
}

fn sample_spec(ticket_id: &str, expires_at_unix_sec: u64) -> TestTicketSpec {
    TestTicketSpec {
        ticket_id: ticket_id.into(),
        user_id: "user-001".into(),
        caller_device_id: "111222333".into(),
        caller_pubkey_hex: "pub:caller-priv-key".into(),
        target_device_id: "999888777".into(),
        session_id_str: "sess-abc".into(),
        expires_at_unix_sec,
        caller_priv_secret: b"caller-priv-key".to_vec(),
    }
}

fn make_host_parts(
    transport: Box<dyn Transport>,
    auth: HostAuthConfig,
    approver: Box<dyn ConnectionApprover>,
) -> HostSession {
    let mut cfg = HostSessionConfig::new(
        "127.0.0.1:9901".parse().unwrap(),
        params(),
        Box::new(NoopInjector),
    );
    cfg.pipeline = pcfg();
    cfg.handshake_timeout = Duration::from_millis(500);
    cfg.pace_capture = true;
    cfg.auth = auth;
    cfg.approver = approver;

    let capture = Box::new(
        FakeBackend::new(FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 60,
            content: FakeContent::Static,
            dirty_rect: None,
        })
        .expect("fake capture"),
    );
    let encoder = Box::new(
        FakeEncoder::with_tuning(
            EncoderConfig {
                resolution: Resolution::new(64, 64),
                fps: 60,
                bitrate_bps: 2_000_000,
                ..EncoderConfig::default()
            },
            500,
            128,
        )
        .expect("fake encoder"),
    );
    HostSession::with_parts(cfg, transport, capture, encoder, zero_clock()).expect("host")
}

fn make_client_parts(
    transport: Box<dyn Transport>,
    session_id: u64,
    auth: ClientAuthConfig,
) -> ClientSession {
    let mut cfg = ClientSessionConfig::new(
        "127.0.0.1:9901".parse().unwrap(),
        params(),
        session_id,
        Box::new(EmptyCapture),
    );
    cfg.pipeline = pcfg();
    cfg.handshake_timeout = Duration::from_millis(500);
    cfg.auth = auth;

    let decoder = Box::new(FakeDecoder::new(FakeDecoderConfig::default()).expect("decoder"));
    let renderer = Box::new(FakeRenderer::new(FakeRendererConfig::default()).expect("renderer"));
    ClientSession::with_parts(cfg, transport, decoder, renderer, zero_clock()).expect("client")
}

#[test]
fn owner_ticket_succeeds_even_with_assist_disabled_password_set_and_deny_approver() {
    let (host_t, client_t) = pair();
    let spec = sample_spec("ticket-1", 2_000);
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let signer = Arc::new(TestOwnerSigner::from_spec(&spec));
    let submit_called = Arc::new(AtomicBool::new(false));

    let mut host = make_host_parts(
        Box::new(YieldingTransport(host_t)),
        HostAuthConfig {
            password: Some("secret".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            allow_remote_assist: false,
            owner_verifier: Some(verifier),
            unix_time_fn: Some(Arc::new(|| 1_000)),
            ..HostAuthConfig::default()
        },
        Box::new(SpyDenyApprover {
            submit_called: Arc::clone(&submit_called),
        }),
    );

    let mut client = make_client_parts(
        Box::new(YieldingTransport(client_t)),
        0xCAFE_BABE_1234_5678,
        ClientAuthConfig {
            password: None,
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            owner_signer: Some(signer),
        },
    );

    let h_thread = thread::spawn(move || {
        host.handshake()
            .expect("owner ticket must bypass password and DenyApprover");
        host
    });
    let granted = client
        .handshake()
        .expect("client owner handshake must succeed");
    assert_eq!(granted, params());
    assert_eq!(client.state(), SessionState::Streaming);

    let mut host = h_thread.join().expect("host thread");
    assert_eq!(host.state(), SessionState::Streaming);
    assert!(
        !submit_called.load(Ordering::SeqCst),
        "OwnerTicket 成功不得调用 ConnectionApprover::submit"
    );

    let owner = host
        .authenticated_owner()
        .expect("authenticated_owner 必须返回已验证的自控票据信息");
    assert_eq!(owner.ticket_id, "ticket-1");
    assert_eq!(owner.user_id, "user-001");
    assert_eq!(owner.caller_device_id, "111222333");
    assert_eq!(owner.target_device_id, "999888777");
    assert_eq!(owner.session_id_str, "sess-abc");
    assert_eq!(owner.expires_at_unix_sec, 2_000);

    client.close(ByeReason::Normal).expect("client close");
    host.close(ByeReason::Normal).expect("host close");
}

#[test]
fn assist_disabled_rejects_none_and_even_correct_password() {
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });

    // 1. AuthMethod::None 被拒
    {
        let (host_t, client_t) = pair();
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            HostAuthConfig {
                password: None,
                allow_remote_assist: false,
                owner_verifier: Some(Arc::clone(&verifier) as Arc<dyn OwnerTicketVerifier>),
                unix_time_fn: Some(Arc::new(|| 1_000)),
                ..HostAuthConfig::default()
            },
            Box::new(AutoApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            101,
            ClientAuthConfig::default(),
        );

        let h_thread = thread::spawn(move || {
            let res = host.handshake();
            (host.state(), host.authenticated_owner().cloned(), res)
        });
        let c_err = client
            .handshake()
            .expect_err("allow_remote_assist=false 必须拒绝 None");
        assert!(matches!(c_err, Error::Protocol { .. }), "{c_err}");
        assert_eq!(client.state(), SessionState::Closed);

        let (h_state, h_owner, h_res) = h_thread.join().expect("host thread");
        assert!(matches!(h_res, Err(Error::Protocol { .. })));
        assert_eq!(h_state, SessionState::Closed);
        assert!(h_owner.is_none());
    }

    // 2. AuthMethod::Password（即使密码完全正确且 AutoApprover）仍被拒
    {
        let (host_t, client_t) = pair();
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            HostAuthConfig {
                password: Some("correct-secret".into()),
                iterations: 64,
                allow_remote_assist: false,
                owner_verifier: Some(verifier),
                unix_time_fn: Some(Arc::new(|| 1_000)),
                ..HostAuthConfig::default()
            },
            Box::new(AutoApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            102,
            ClientAuthConfig {
                password: Some("correct-secret".into()),
                ..ClientAuthConfig::default()
            },
        );

        let h_thread = thread::spawn(move || {
            let res = host.handshake();
            (host.state(), host.authenticated_owner().cloned(), res)
        });
        let c_err = client
            .handshake()
            .expect_err("allow_remote_assist=false 必须拒绝正确密码");
        assert!(matches!(c_err, Error::Protocol { .. }), "{c_err}");
        assert_eq!(client.state(), SessionState::Closed);

        let (h_state, h_owner, h_res) = h_thread.join().expect("host thread");
        assert!(matches!(h_res, Err(Error::Protocol { .. })));
        assert_eq!(h_state, SessionState::Closed);
        assert!(h_owner.is_none());
    }
}

#[test]
fn owner_ticket_rejects_tampered_expired_replayed_forged_and_wrong_session_id() {
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let host_cfg = |now_sec: u64| HostAuthConfig {
        allow_remote_assist: false,
        owner_verifier: Some(Arc::clone(&verifier) as Arc<dyn OwnerTicketVerifier>),
        unix_time_fn: Some(Arc::new(move || now_sec)),
        ..HostAuthConfig::default()
    };

    // 1. 篡改票据载荷
    {
        let (host_t, client_t) = pair();
        let spec = sample_spec("t-tamper", 2_000);
        let mut tampered_payload = spec.encode_signed_payload();
        tampered_payload[0] ^= 0x01;
        let signer = Arc::new(TestOwnerSigner {
            payload: tampered_payload,
            caller_priv_secret: spec.caller_priv_secret,
            override_session_id: None,
        });
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            host_cfg(1_000),
            Box::new(DenyApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            201,
            ClientAuthConfig {
                owner_signer: Some(signer),
                ..ClientAuthConfig::default()
            },
        );
        let h_thread = thread::spawn(move || host.handshake());
        assert!(client.handshake().is_err(), "篡改票据必须拒绝");
        assert!(h_thread.join().unwrap().is_err());
    }

    // 2. 过期票据（now == expires_at_unix_sec）
    {
        let (host_t, client_t) = pair();
        let spec = sample_spec("t-expired", 1_500);
        let signer = Arc::new(TestOwnerSigner::from_spec(&spec));
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            host_cfg(1_500),
            Box::new(DenyApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            202,
            ClientAuthConfig {
                owner_signer: Some(signer),
                ..ClientAuthConfig::default()
            },
        );
        let h_thread = thread::spawn(move || host.handshake());
        assert!(client.handshake().is_err(), "过期票据必须拒绝");
        assert!(h_thread.join().unwrap().is_err());
    }

    // 3. 伪造 PoP 签名（私钥与票据公钥不匹配）
    {
        let (host_t, client_t) = pair();
        let mut spec = sample_spec("t-forged", 2_000);
        spec.caller_priv_secret = b"wrong-attacker-key".to_vec();
        let signer = Arc::new(TestOwnerSigner::from_spec(&spec));
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            host_cfg(1_000),
            Box::new(DenyApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            203,
            ClientAuthConfig {
                owner_signer: Some(signer),
                ..ClientAuthConfig::default()
            },
        );
        let h_thread = thread::spawn(move || host.handshake());
        assert!(client.handshake().is_err(), "伪造 PoP 签名必须拒绝");
        assert!(h_thread.join().unwrap().is_err());
    }

    // 4. 签名绑定错 session_id
    {
        let (host_t, client_t) = pair();
        let spec = sample_spec("t-wrong-sid", 2_000);
        let mut signer = TestOwnerSigner::from_spec(&spec);
        signer.override_session_id = Some(999_999);
        let mut host = make_host_parts(
            Box::new(YieldingTransport(host_t)),
            host_cfg(1_000),
            Box::new(DenyApprover),
        );
        let mut client = make_client_parts(
            Box::new(YieldingTransport(client_t)),
            204,
            ClientAuthConfig {
                owner_signer: Some(Arc::new(signer)),
                ..ClientAuthConfig::default()
            },
        );
        let h_thread = thread::spawn(move || host.handshake());
        assert!(client.handshake().is_err(), "错 session_id PoP 必须拒绝");
        assert!(h_thread.join().unwrap().is_err());
    }

    // 5. 纯状态机：重放同 ticket_id 必须拒绝（含跨状态机实例共享与单实例连续调用）
    {
        let addr: SocketAddr = "127.0.0.1:45000".parse().unwrap();
        let spec = sample_spec("t-replay-1", 2_000);
        let payload = spec.encode_signed_payload();
        let signer = TestOwnerSigner::from_spec(&spec);
        let mut host = HostAuthenticator::new(host_cfg(1_000));

        let hello1 = Hello {
            version: PROTOCOL_VERSION,
            session_id: 555,
            requested: params(),
            auth_method: AuthMethod::OwnerTicket,
            auth_payload: payload.clone(),
        };
        let ch1 = match host.on_hello_at(&hello1, addr, 1_000) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("首次合法票据应下发 Challenge，实际 {other:?}"),
        };
        let sig1 = signer.sign_challenge(555, &ch1.salt).unwrap();
        assert_eq!(
            host.on_owner_auth_response(&sig1),
            HostAuthAction::Conclude(AuthStatus::Ok)
        );
        assert!(host.authenticated_owner().is_some());

        // 同一 ticket_id 再次发 Hello → 立即拒绝
        let hello2 = Hello {
            version: PROTOCOL_VERSION,
            session_id: 556,
            requested: params(),
            auth_method: AuthMethod::OwnerTicket,
            auth_payload: payload,
        };
        assert_eq!(
            host.on_hello_at(&hello2, addr, 1_001),
            HostAuthAction::Conclude(AuthStatus::Rejected),
            "重放同 ticket_id 必须拒绝"
        );
        assert!(host.authenticated_owner().is_none());
    }

    // 6. ClientAuthenticator + HostAuthenticator 端到端线格式错包/未配置 verifier 拒绝
    {
        let addr: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        let spec = sample_spec("t-no-verifier", 2_000);
        let client = ClientAuthenticator::with_session_id(
            ClientAuthConfig {
                owner_signer: Some(Arc::new(TestOwnerSigner::from_spec(&spec))),
                ..ClientAuthConfig::default()
            },
            777,
        );
        let (method, payload) = client.hello_auth();
        assert_eq!(method, AuthMethod::OwnerTicket);
        let mut host = HostAuthenticator::new(HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: None,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            host.on_hello_at(
                &Hello {
                    version: PROTOCOL_VERSION,
                    session_id: 777,
                    requested: params(),
                    auth_method: method,
                    auth_payload: payload,
                },
                addr,
                1_000
            ),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
    }

    // 7. ClientAuthAction::RespondOwner 线格式动作检查
    {
        let spec = sample_spec("t-client-action", 2_000);
        let mut client = ClientAuthenticator::with_session_id(
            ClientAuthConfig {
                owner_signer: Some(Arc::new(TestOwnerSigner::from_spec(&spec))),
                ..ClientAuthConfig::default()
            },
            888,
        );
        assert_eq!(
            client.on_welcome(&rc_session::proto::Welcome {
                session_id: 888,
                granted: params(),
                auth_status: AuthStatus::ChallengeRequired,
                host_flags: rc_session::proto::HostFlags(0),
            }),
            ClientAuthAction::Waiting
        );
        let salt = [0x42u8; AUTH_SALT_LEN];
        match client.on_challenge(&rc_session::proto::AuthChallenge {
            salt,
            iterations: 1,
        }) {
            ClientAuthAction::RespondOwner(sig) => {
                assert_eq!(sig.len(), 32);
            }
            other => panic!("expected RespondOwner, got {other:?}"),
        }
    }
}

#[test]
fn detached_owner_session_rejects_fast_resume_and_ticket_replay_allows_fresh_ticket() {
    let (host_t, client_t) = pair();
    let spec1 = sample_spec("t-det-1", 2_000);
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let signer1 = Arc::new(TestOwnerSigner::from_spec(&spec1));

    let mut host = make_host_parts(
        Box::new(YieldingTransport(host_t)),
        HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: Some(Arc::clone(&verifier) as Arc<dyn OwnerTicketVerifier>),
            unix_time_fn: Some(Arc::new(|| 1_000)),
            ..HostAuthConfig::default()
        },
        Box::new(DenyApprover),
    );
    let mut client = make_client_parts(
        Box::new(YieldingTransport(client_t)),
        3001,
        ClientAuthConfig {
            owner_signer: Some(signer1),
            ..ClientAuthConfig::default()
        },
    );

    let h_thread = thread::spawn(move || {
        host.handshake().expect("initial owner handshake");
        host
    });
    client.handshake().expect("client initial handshake");
    let mut host = h_thread.join().expect("host join");
    let gen1 = host.pipeline_generation();
    assert_eq!(
        host.authenticated_owner().map(|o| o.ticket_id.as_str()),
        Some("t-det-1")
    );

    host.set_pipeline_factory(Box::new(|| {
        rc_pipeline::HostPipeline::with_backends_paced(
            pcfg(),
            Box::new(
                FakeBackend::new(FakeBackendConfig {
                    display: DisplayInfo {
                        id: rc_types::DisplayId(0),
                        resolution: Resolution::new(64, 64),
                        scale: 1.0,
                        hdr: false,
                    },
                    fps: 60,
                    content: FakeContent::Static,
                    dirty_rect: None,
                })
                .unwrap(),
            ),
            Box::new(
                FakeEncoder::with_tuning(
                    EncoderConfig {
                        resolution: Resolution::new(64, 64),
                        fps: 60,
                        bitrate_bps: 2_000_000,
                        ..EncoderConfig::default()
                    },
                    500,
                    128,
                )
                .unwrap(),
            ),
            true,
        )
    }));

    // 断线进 Detached
    host.on_disconnected(1_000);
    assert_eq!(host.state(), SessionState::Detached);

    // 1. 未鉴权快速 resume 必须被拒（!resume_accepted），且旧管线不被拆除
    let mut cand_resume = MockTransport::new();
    cand_resume.deliver_reliable(&ControlMessage::ReconnectRequest {
        session_id: 3001,
        resume: true,
    });
    let cand_resume_handle = cand_resume.clone();
    host.offer_transport(Box::new(cand_resume))
        .expect("offer resume cand");
    assert_eq!(
        host.tick_detached(2_000).expect("tick resume"),
        DetachOutcome::Waiting
    );
    match ControlMessage::decode(&cand_resume_handle.reliable_sent()[0]).expect("decode welcome") {
        ControlMessage::Welcome(w) => assert!(
            !w.host_flags.resume_accepted(),
            "OwnerTicket 会话 Detached 期禁止未鉴权快速 resume"
        ),
        other => panic!("expected Welcome, got {other:?}"),
    }
    assert_eq!(host.state(), SessionState::Detached);
    assert_eq!(host.pipeline_generation(), gen1);

    // 2. 在 Detached 候选上重放同 ticket_id ("t-det-1") → 必须被拒绝
    let mut cand_replay = MockTransport::new();
    cand_replay.deliver_reliable(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        session_id: 3002,
        requested: params(),
        auth_method: AuthMethod::OwnerTicket,
        auth_payload: spec1.encode_signed_payload(),
    }));
    let cand_replay_handle = cand_replay.clone();
    host.offer_transport(Box::new(cand_replay))
        .expect("offer replay cand");
    assert_eq!(
        host.tick_detached(3_000).expect("tick replay"),
        DetachOutcome::Waiting
    );
    match ControlMessage::decode(cand_replay_handle.reliable_sent().last().unwrap())
        .expect("decode reject")
    {
        ControlMessage::Welcome(w) => assert_eq!(w.auth_status, AuthStatus::Rejected),
        other => panic!("expected Welcome{{Rejected}}, got {other:?}"),
    }
    assert_eq!(host.pipeline_generation(), gen1);

    // 3. 新票据 ("t-det-2") + 合法 PoP 签名 → Detached 替换成功，进入 Streaming
    let spec2 = sample_spec("t-det-2", 2_000);
    let signer2 = TestOwnerSigner::from_spec(&spec2);
    let mut cand_fresh = MockTransport::new();
    cand_fresh.deliver_reliable(&ControlMessage::Hello(Hello {
        version: PROTOCOL_VERSION,
        session_id: 3003,
        requested: params(),
        auth_method: AuthMethod::OwnerTicket,
        auth_payload: spec2.encode_signed_payload(),
    }));
    let mut cand_fresh_handle = cand_fresh.clone();
    host.offer_transport(Box::new(cand_fresh))
        .expect("offer fresh cand");
    assert_eq!(
        host.tick_detached(4_000).expect("tick fresh hello"),
        DetachOutcome::Waiting
    );
    let sent = cand_fresh_handle.reliable_sent();
    let ch = match ControlMessage::decode(sent.last().unwrap()).expect("decode ch") {
        ControlMessage::AuthChallenge(ch) => ch,
        other => panic!("expected AuthChallenge, got {other:?}"),
    };
    let sig2 = signer2.sign_challenge(3003, &ch.salt).unwrap();
    cand_fresh_handle.deliver_reliable(&ControlMessage::OwnerAuthResponse { signature: sig2 });
    assert_eq!(
        host.tick_detached(5_000).expect("tick fresh pop"),
        DetachOutcome::Replaced
    );
    assert_eq!(host.state(), SessionState::Streaming);
    assert_eq!(host.pipeline_generation(), gen1 + 1);
    assert_eq!(
        host.authenticated_owner().map(|o| o.ticket_id.as_str()),
        Some("t-det-2")
    );

    client.close(ByeReason::Normal).expect("client close");
    host.close(ByeReason::Normal).expect("host close");
}

#[test]
fn streaming_mock_rejects_owner_auth_response_on_both_sides() {
    // Host Streaming 期收到 OwnerAuthResponse → 致命 Error::Protocol
    let (host_t, mut client_t) = pair();
    let spec = sample_spec("t-a11-h", 2_000);
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let signer = Arc::new(TestOwnerSigner::from_spec(&spec));

    let mut host = make_host_parts(
        Box::new(YieldingTransport(host_t)),
        HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: Some(verifier),
            unix_time_fn: Some(Arc::new(|| 1_000)),
            ..HostAuthConfig::default()
        },
        Box::new(DenyApprover),
    );
    let mut client = make_client_parts(
        Box::new(YieldingTransport(client_t.clone())),
        4002,
        ClientAuthConfig {
            owner_signer: Some(signer),
            ..ClientAuthConfig::default()
        },
    );

    let h_thread = thread::spawn(move || {
        host.handshake().expect("handshake");
        host
    });
    client.handshake().expect("client handshake");
    let mut host = h_thread.join().expect("host join");

    let mut raw = Vec::new();
    ControlMessage::OwnerAuthResponse {
        signature: vec![0x11; 32],
    }
    .encode(&mut raw);
    client_t.send_reliable(&raw).expect("send raw 0x12");

    let err = host
        .tick(Duration::ZERO)
        .expect_err("A11: Host Streaming 期 OwnerAuthResponse 必须致命断开");
    assert!(matches!(err, Error::Protocol { .. }), "{err}");
    assert_eq!(host.state(), SessionState::Closed);
    let _ = client.close(ByeReason::Normal);

    // Client Streaming 期收到 OwnerAuthResponse → 致命 Error::Protocol
    let mut mock_c = MockTransport::new();
    mock_c.deliver_reliable(&ControlMessage::Welcome(rc_session::proto::Welcome {
        session_id: 4003,
        granted: params(),
        auth_status: AuthStatus::Ok,
        host_flags: rc_session::proto::HostFlags(0),
    }));
    let mut handle_c = mock_c.clone();
    let mut client2 = make_client_parts(Box::new(mock_c), 4003, ClientAuthConfig::default());
    client2.handshake().expect("client2 handshake");
    handle_c.deliver_reliable(&ControlMessage::OwnerAuthResponse {
        signature: vec![0x22; 32],
    });
    let err2 = client2
        .tick(Duration::ZERO, Instant::now())
        .expect_err("A11: Client Streaming 期 OwnerAuthResponse 必须致命断开");
    assert!(matches!(err2, Error::Protocol { .. }), "{err2}");
    assert_eq!(client2.state(), SessionState::Closed);
}

#[test]
fn quic_localhost_owner_ticket_streams_frames_without_password_or_approval() {
    let host_built = Arc::new(AtomicUsize::new(0));
    let client_built = Arc::new(AtomicUsize::new(0));
    let submit_called = Arc::new(AtomicBool::new(false));
    let spec = sample_spec("t-quic-1", 2_000);
    let verifier = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let signer = Arc::new(TestOwnerSigner::from_spec(&spec));

    let origin: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let clock: SessionClock = {
        let o = Arc::clone(&origin);
        Arc::new(move || match *o.lock().expect("origin") {
            Some(t) => u32::try_from(t.elapsed().as_micros()).unwrap_or(u32::MAX),
            None => 0,
        })
    };

    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mtu: rc_session::DEFAULT_SESSION_MTU,
            caps: params(),
            pipeline: pcfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(RecordingInjector::new()),
            pace_capture: true,
            auth: HostAuthConfig {
                password: Some("host-secret".into()),
                iterations: 64,
                approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                allow_remote_assist: false,
                owner_verifier: Some(verifier),
                unix_time_fn: Some(Arc::new(|| 1_000)),
                ..HostAuthConfig::default()
            },
            approver: Box::new(SpyDenyApprover {
                submit_called: Arc::clone(&submit_called),
            }),
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        clock,
    )
    .expect("host new");

    let host_built_c = Arc::clone(&host_built);
    let origin_c = Arc::clone(&origin);
    host.set_pipeline_factory_for(Box::new(move |pcfg| {
        host_built_c.fetch_add(1, Ordering::SeqCst);
        *origin_c.lock().expect("origin") = Some(Instant::now());
        let capture = FakeBackend::new(FakeBackendConfig {
            display: DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: pcfg.resolution,
                scale: 1.0,
                hdr: false,
            },
            fps: pcfg.fps,
            content: FakeContent::Dynamic,
            dirty_rect: None,
        })?;
        let encoder = FakeEncoder::with_tuning(
            EncoderConfig {
                resolution: pcfg.resolution,
                fps: pcfg.fps,
                bitrate_bps: pcfg.bitrate_bps,
                ..EncoderConfig::default()
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

    let host_addr = host.local_addr().expect("local_addr");
    let host_thread = thread::spawn(move || {
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("QUIC OwnerTicket handshake must succeed");
        let owner_id = host
            .authenticated_owner()
            .expect("authenticated_owner")
            .ticket_id
            .clone();
        let started = Instant::now();
        let summary = host
            .run(&mut || started.elapsed() >= Duration::from_secs(3))
            .expect("host run");
        (owner_id, summary)
    });

    let c_t0 = Instant::now();
    let c_clock: SessionClock =
        Arc::new(move || u32::try_from(c_t0.elapsed().as_micros()).unwrap_or(u32::MAX));
    let mut client = ClientSession::new(
        ClientSessionConfig {
            peer_addr: host_addr,
            mtu: rc_session::DEFAULT_SESSION_MTU,
            requested: params(),
            session_id: 90_001,
            pipeline: pcfg(),
            connect_timeout: CONNECT_TIMEOUT,
            handshake_timeout: Duration::from_secs(5),
            capture: Box::new(ScriptedCapture::standard()),
            input_window: rc_input::DEFAULT_WINDOW,
            auth: ClientAuthConfig {
                password: None,
                auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
                owner_signer: Some(signer),
            },
        },
        SERVER_CERT_PEM,
        SERVER_KEY_PEM,
        SERVER_CERT_PEM,
        "rabbitcontrol",
        Arc::clone(&c_clock),
    )
    .expect("client new");

    let client_built_c = Arc::clone(&client_built);
    client.set_pipeline_factory_for(Box::new(move |pcfg| {
        client_built_c.fetch_add(1, Ordering::SeqCst);
        let decoder = FakeDecoder::new(FakeDecoderConfig {
            resolution: pcfg.resolution,
            ..FakeDecoderConfig::default()
        })?;
        let renderer = FakeRenderer::new(FakeRendererConfig {
            size: (pcfg.resolution.width, pcfg.resolution.height),
            ..FakeRendererConfig::default()
        })?;
        rc_pipeline::ControllerPipeline::with_backends(
            pcfg.clone(),
            Box::new(decoder),
            Box::new(renderer),
        )
    }));

    client
        .connect_and_handshake()
        .expect("client QUIC OwnerTicket handshake");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && client.pipeline().displayed() < 3 {
        let now_us = u64::from((c_clock)());
        client.pump_probes(now_us).expect("pump_probes");
        let r = client
            .tick(Duration::from_millis(5), Instant::now())
            .expect("tick");
        if r.closed.is_some() {
            break;
        }
    }
    let displayed = client.pipeline().displayed();
    client.close(ByeReason::Normal).expect("client close");

    let (owner_id, host_summary) = host_thread.join().expect("host thread");
    assert_eq!(owner_id, "t-quic-1");
    assert!(!submit_called.load(Ordering::SeqCst));
    assert!(displayed >= 3, "expected >= 3 frames, got {displayed}");
    assert!(host_summary.frames_encoded >= 1);
    assert_eq!(host_built.load(Ordering::SeqCst), 1);
    assert_eq!(client_built.load(Ordering::SeqCst), 1);
}

#[test]
fn shared_replay_cache_blocks_ticket_reuse_across_host_sessions() {
    use rc_session::OwnerReplayCache;
    use std::sync::Mutex;

    let shared_cache = Arc::new(Mutex::new(OwnerReplayCache::new()));
    let verifier: Arc<dyn OwnerTicketVerifier> = Arc::new(TestOwnerVerifier {
        expected_target_device_id: "999888777".into(),
    });
    let spec = sample_spec("ticket-cross-session-1", 2_000);
    let signer: Arc<dyn OwnerTicketSigner> = Arc::new(TestOwnerSigner::from_spec(&spec));

    // Session 1 consumes ticket-cross-session-1.
    let (h1_t, c1_t) = pair();
    let mut h1 = make_host_parts(
        Box::new(YieldingTransport(h1_t)),
        HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: Some(Arc::clone(&verifier)),
            replay_cache: Some(Arc::clone(&shared_cache)),
            unix_time_fn: Some(Arc::new(|| 1_000)),
            ..HostAuthConfig::default()
        },
        Box::new(AutoApprover),
    );
    let mut c1 = make_client_parts(
        Box::new(YieldingTransport(c1_t)),
        0x1111,
        ClientAuthConfig {
            owner_signer: Some(Arc::clone(&signer)),
            ..ClientAuthConfig::default()
        },
    );
    let j1 = thread::spawn(move || {
        h1.handshake()
            .expect("first session must accept fresh ticket");
        h1
    });
    c1.handshake().expect("first client handshake");
    let mut h1 = j1.join().unwrap();
    let _ = c1.close(ByeReason::Normal);
    let _ = h1.close(ByeReason::Normal);

    // Session 2 (brand new HostSession sharing `shared_cache`) must reject the same ticket_id!
    let (h2_t, c2_t) = pair();
    let mut h2 = make_host_parts(
        Box::new(YieldingTransport(h2_t)),
        HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: Some(verifier),
            replay_cache: Some(shared_cache),
            unix_time_fn: Some(Arc::new(|| 1_000)),
            ..HostAuthConfig::default()
        },
        Box::new(AutoApprover),
    );
    let mut c2 = make_client_parts(
        Box::new(YieldingTransport(c2_t)),
        0x2222,
        ClientAuthConfig {
            owner_signer: Some(signer),
            ..ClientAuthConfig::default()
        },
    );
    let j2 = thread::spawn(move || h2.handshake());
    let c2_err = c2
        .handshake()
        .expect_err("replayed OwnerGrantPayload across HostSessions must be rejected");
    assert!(
        matches!(c2_err, Error::Protocol { .. } | Error::Network(_)),
        "{c2_err}"
    );
    assert!(matches!(
        j2.join().unwrap(),
        Err(Error::Protocol { .. } | Error::Network(_))
    ));
}
