#![deny(unsafe_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{EncoderConfig, FakeEncoder};
use rc_input::{InputCapture, InputEvent};
use rc_net::quic::{Identity, QuicEndpoint};
use rc_net::{NetConfig, Role, Transport};
use rc_pipeline::{DiffMode, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::dev_cert::{dev_server_cert_pem, DEV_CERT_PEM, DEV_KEY_PEM, DEV_SERVER_NAME};
use rc_session::proto::SessionParams;
use rc_session::testkit::RecordingInjector;
use rc_session::{
    AutoApprover, ClientAuthConfig, ClientSession, ClientSessionConfig, HostAuthConfig,
    HostSession, HostSessionConfig, SessionClock,
};
use rc_types::{DisplayId, Resolution};
use rc_ui::relay_transport::{
    E2eeRelayTransport, RelayE2eeMode, RelayRole, SessionMtlsBundle, TcpRelayTransport,
};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

const BENCH_FRAMES: usize = 220;
const WARMUP_FRAMES: usize = 20;
const BENCH_FPS: u32 = 120;
const BENCH_RES: (u32, u32) = (64, 64);

const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    if body.len() < 128 {
        out.push(body.len() as u8);
    } else if body.len() <= 0xFF {
        out.push(0x81);
        out.push(body.len() as u8);
    } else {
        out.push(0x82);
        out.push((body.len() >> 8) as u8);
        out.push((body.len() & 0xFF) as u8);
    }
    out.extend_from_slice(body);
    out
}

fn der_sequence(body: &[u8]) -> Vec<u8> {
    der_tlv(0x30, body)
}

fn der_oid(oid: &[u8]) -> Vec<u8> {
    der_tlv(0x06, oid)
}

fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len() + 1);
    body.push(0x00);
    body.extend_from_slice(bytes);
    der_tlv(0x03, &body)
}

fn encode_pem(label: &str, der: &[u8]) -> String {
    let b64 = B64.encode(der);
    let mut pem = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str(&format!("-----END {label}-----\n"));
    pem
}

fn build_ec_spki_der(uncompressed_pub: &[u8]) -> Vec<u8> {
    let alg_body = [der_oid(OID_EC_PUBLIC_KEY), der_oid(OID_PRIME256V1)].concat();
    let spki = [der_sequence(&alg_body), der_bit_string(uncompressed_pub)].concat();
    der_sequence(&spki)
}

fn build_cn_dn_der(cn: &str) -> Vec<u8> {
    let atv = [der_oid(OID_COMMON_NAME), der_tlv(0x0C, cn.as_bytes())].concat();
    der_sequence(&der_tlv(0x31, &der_sequence(&atv)))
}

fn sign_x509_v3(
    ca_kp: &EcdsaKeyPair,
    serial: u8,
    issuer_dn: &[u8],
    subject_dn: &[u8],
    spki_der: &[u8],
    exts: &[u8],
) -> Vec<u8> {
    let sig_alg = der_sequence(&der_oid(OID_ECDSA_WITH_SHA256));
    let validity = der_sequence(
        &[
            der_tlv(0x17, b"250101000000Z"),
            der_tlv(0x17, b"350101000000Z"),
        ]
        .concat(),
    );
    let mut tbs_body = Vec::new();
    tbs_body.extend_from_slice(&der_tlv(0xA0, &der_tlv(0x02, &[0x02])));
    tbs_body.extend_from_slice(&der_tlv(0x02, &[serial]));
    tbs_body.extend_from_slice(&sig_alg);
    tbs_body.extend_from_slice(issuer_dn);
    tbs_body.extend_from_slice(&validity);
    tbs_body.extend_from_slice(subject_dn);
    tbs_body.extend_from_slice(spki_der);
    tbs_body.extend_from_slice(exts);
    let tbs = der_sequence(&tbs_body);
    let sig = ca_kp.sign(&SystemRandom::new(), &tbs).unwrap();
    let cert_body = [tbs, sig_alg, der_bit_string(sig.as_ref())].concat();
    der_sequence(&cert_body)
}

struct BenchCa {
    ca_pem: String,
    ca_dn: Vec<u8>,
    ca_kp: EcdsaKeyPair,
    next_serial: u8,
}

impl BenchCa {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let ca_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng).unwrap();
        let ca_spki = build_ec_spki_der(ca_kp.public_key().as_ref());
        let ca_dn = build_cn_dn_der("RabbitControl Bench Root CA");
        let bc = [
            der_oid(&[0x55, 0x1D, 0x13]),
            der_tlv(0x01, &[0xFF]),
            der_tlv(0x04, &der_sequence(&der_tlv(0x01, &[0xFF]))),
        ]
        .concat();
        let ku = [
            der_oid(&[0x55, 0x1D, 0x0F]),
            der_tlv(0x01, &[0xFF]),
            der_tlv(0x04, &der_tlv(0x03, &[0x01, 0x86])),
        ]
        .concat();
        let exts = der_tlv(
            0xA3,
            &der_sequence(&[der_sequence(&bc), der_sequence(&ku)].concat()),
        );
        let ca_der = sign_x509_v3(&ca_kp, 1, &ca_dn, &ca_dn, &ca_spki, &exts);
        Self {
            ca_pem: encode_pem("CERTIFICATE", &ca_der),
            ca_dn,
            ca_kp,
            next_serial: 2,
        }
    }

    fn issue_leaf(&mut self, cn: &str, san_entries: &[Vec<u8>]) -> (String, String) {
        let rng = SystemRandom::new();
        let pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng).unwrap();
        let spki = build_ec_spki_der(kp.public_key().as_ref());
        let dn = build_cn_dn_der(cn);
        let bc = [
            der_oid(&[0x55, 0x1D, 0x13]),
            der_tlv(0x04, &der_sequence(&[])),
        ]
        .concat();
        let ku = [
            der_oid(&[0x55, 0x1D, 0x0F]),
            der_tlv(0x01, &[0xFF]),
            der_tlv(0x04, &der_tlv(0x03, &[0x05, 0xA0])),
        ]
        .concat();
        let eku_oids = [
            der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]),
            der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02]),
        ]
        .concat();
        let eku = [
            der_oid(&[0x55, 0x1D, 0x25]),
            der_tlv(0x04, &der_sequence(&eku_oids)),
        ]
        .concat();
        let san = [
            der_oid(&[0x55, 0x1D, 0x11]),
            der_tlv(0x04, &der_sequence(&san_entries.concat())),
        ]
        .concat();
        let exts = der_tlv(
            0xA3,
            &der_sequence(
                &[
                    der_sequence(&bc),
                    der_sequence(&ku),
                    der_sequence(&eku),
                    der_sequence(&san),
                ]
                .concat(),
            ),
        );
        let serial = self.next_serial;
        self.next_serial += 1;
        let cert_der = sign_x509_v3(&self.ca_kp, serial, &self.ca_dn, &dn, &spki, &exts);
        (
            encode_pem("CERTIFICATE", &cert_der),
            encode_pem("PRIVATE KEY", pk8.as_ref()),
        )
    }

    fn issue_device(&mut self, user_id: &str, device_id: &str) -> (String, String) {
        let san_uri = format!("urn:rabbitcontrol:user:{user_id}:device:{device_id}");
        let san_dns = format!("device-{device_id}.rc.internal");
        self.issue_leaf(
            &format!("rc-device-{device_id}"),
            &[
                der_tlv(0x86, san_uri.as_bytes()),
                der_tlv(0x82, san_dns.as_bytes()),
                der_tlv(0x87, &[127, 0, 0, 1]),
            ],
        )
    }

    fn issue_server_loopback(&mut self) -> (String, String) {
        self.issue_leaf(
            "127.0.0.1",
            &[der_tlv(0x82, b"localhost"), der_tlv(0x87, &[127, 0, 0, 1])],
        )
    }
}

fn spawn_low_jitter_mtls_relay(
    ca_pem: &str,
    srv_cert_pem: &str,
    srv_key_pem: &str,
) -> (SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let mut root_store = rustls::RootCertStore::empty();
    for cert in Identity::certs_from_pem(ca_pem).unwrap() {
        root_store.add(cert).unwrap();
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(root_store),
        Arc::new(rustls::crypto::ring::default_provider()),
    )
    .build()
    .unwrap();
    let certs = Identity::certs_from_pem(srv_cert_pem).unwrap();
    let key = rustls_pemfile::private_key(&mut srv_key_pem.as_bytes())
        .unwrap()
        .unwrap();
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .unwrap(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_c = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let mut streams = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        while streams.len() < 2 && Instant::now() < deadline && !stop_c.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((tcp, _)) => {
                    tcp.set_nodelay(true).unwrap();
                    tcp.set_nonblocking(false).unwrap();
                    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let conn = rustls::ServerConnection::new(Arc::clone(&cfg)).unwrap();
                    let mut tls = rustls::StreamOwned::new(conn, tcp);
                    let mut hdr = [0u8; 6];
                    if tls.read_exact(&mut hdr).is_ok() && &hdr[..4] == b"RCR1" {
                        let mut tok = vec![0u8; usize::from(hdr[5])];
                        if tls.read_exact(&mut tok).is_ok() {
                            streams.push(tls);
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => break,
            }
        }
        if streams.len() != 2 {
            return;
        }
        for s in &mut streams {
            let _ = s.write_all(&[0x00]);
            let _ = s.flush();
        }
        let mut it = streams.into_iter();
        let mut s0 = it.next().unwrap();
        let mut s1 = it.next().unwrap();
        s0.sock.set_nonblocking(true).unwrap();
        s1.sock.set_nonblocking(true).unwrap();
        let s0_tcp = s0.sock.try_clone().unwrap();
        let s1_tcp = s1.sock.try_clone().unwrap();
        let mut buf = [0u8; 32768];
        let mut idle_spins = 0u32;

        while !stop_c.load(Ordering::Acquire) && Instant::now() < deadline {
            let mut active = false;
            loop {
                match s0.read(&mut buf) {
                    Ok(0) => {
                        let _ = s1_tcp.shutdown(Shutdown::Write);
                        return;
                    }
                    Ok(n) => {
                        s1.sock.set_nonblocking(false).unwrap();
                        let ok = s1.write_all(&buf[..n]).is_ok() && s1.flush().is_ok();
                        s1.sock.set_nonblocking(true).unwrap();
                        if !ok {
                            return;
                        }
                        active = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
            loop {
                match s1.read(&mut buf) {
                    Ok(0) => {
                        let _ = s0_tcp.shutdown(Shutdown::Write);
                        return;
                    }
                    Ok(n) => {
                        s0.sock.set_nonblocking(false).unwrap();
                        let ok = s0.write_all(&buf[..n]).is_ok() && s0.flush().is_ok();
                        s0.sock.set_nonblocking(true).unwrap();
                        if !ok {
                            return;
                        }
                        active = true;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
            if active {
                idle_spins = 0;
            } else {
                idle_spins += 1;
                if idle_spins < 64 {
                    thread::yield_now();
                } else {
                    thread::sleep(Duration::from_micros(50));
                }
            }
        }
    });

    (addr, stop, handle)
}

struct BenchNoopCapture;

impl InputCapture for BenchNoopCapture {
    fn next_events(&mut self, _timeout: Duration) -> rc_types::Result<Vec<InputEvent>> {
        Ok(Vec::new())
    }
}

#[derive(Debug, Clone, Copy)]
struct LatencyStats {
    frames: usize,
    p50_ms: f64,
    p99_ms: f64,
}

fn measure_e2e_session(
    host_transport: Box<dyn Transport>,
    client_transport: Box<dyn Transport>,
    label: &str,
) -> LatencyStats {
    let res = Resolution::new(BENCH_RES.0, BENCH_RES.1);
    let t0 = Instant::now();
    let clock: SessionClock =
        Arc::new(move || u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX));

    let pipeline_cfg = PipelineConfig {
        resolution: res,
        fps: BENCH_FPS,
        bitrate_bps: 4_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let host_cfg = HostSessionConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        mtu: rc_session::DEFAULT_SESSION_MTU,
        caps: SessionParams {
            width: res.width,
            height: res.height,
            fps: BENCH_FPS,
            bitrate_bps: 10_000_000,
        },
        pipeline: pipeline_cfg.clone(),
        handshake_timeout: Duration::from_secs(5),
        injector: Box::new(RecordingInjector::new()),
        pace_capture: true,
        auth: HostAuthConfig::default(),
        approver: Box::new(AutoApprover),
    };
    let capture = FakeBackend::new(FakeBackendConfig {
        display: DisplayInfo {
            id: DisplayId(0),
            resolution: res,
            scale: 1.0,
            hdr: false,
        },
        fps: BENCH_FPS,
        content: FakeContent::Dynamic,
        dirty_rect: None,
    })
    .unwrap();
    let encoder = FakeEncoder::new(EncoderConfig {
        resolution: res,
        fps: BENCH_FPS,
        bitrate_bps: 4_000_000,
        ..EncoderConfig::default()
    })
    .unwrap();
    let mut host = HostSession::with_parts(
        host_cfg,
        host_transport,
        Box::new(capture),
        Box::new(encoder),
        Arc::clone(&clock),
    )
    .unwrap();

    let client_cfg = ClientSessionConfig {
        peer_addr: "127.0.0.1:0".parse().unwrap(),
        mtu: rc_session::DEFAULT_SESSION_MTU,
        requested: SessionParams {
            width: res.width,
            height: res.height,
            fps: BENCH_FPS,
            bitrate_bps: 4_000_000,
        },
        session_id: 0xBEEF_0001,
        pipeline: pipeline_cfg,
        connect_timeout: Duration::from_secs(5),
        handshake_timeout: Duration::from_secs(5),
        capture: Box::new(BenchNoopCapture),
        input_window: rc_input::DEFAULT_WINDOW,
        auth: ClientAuthConfig::default(),
    };
    let decoder = FakeDecoder::new(FakeDecoderConfig {
        resolution: res,
        ..FakeDecoderConfig::default()
    })
    .unwrap();
    let renderer = FakeRenderer::new(FakeRendererConfig {
        size: (res.width, res.height),
        ..FakeRendererConfig::default()
    })
    .unwrap();
    let mut client = ClientSession::with_parts(
        client_cfg,
        client_transport,
        Box::new(decoder),
        Box::new(renderer),
        Arc::clone(&clock),
    )
    .unwrap();

    let stop_host = Arc::new(AtomicBool::new(false));
    let stop_host_c = Arc::clone(&stop_host);
    let host_jh = thread::spawn(move || {
        host.handshake().expect("bench host handshake");
        while !stop_host_c.load(Ordering::Acquire) {
            if host.tick(Duration::from_millis(2)).is_err() {
                break;
            }
        }
    });

    client.handshake().expect("bench client handshake");
    let start = Instant::now();
    let deadline = start + Duration::from_secs(15);
    while client.pipeline().displayed() < BENCH_FRAMES as u64 && Instant::now() < deadline {
        client
            .tick(Duration::from_millis(2), Instant::now())
            .expect("bench client tick");
    }
    let obs = client.observations(
        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX),
        None,
    );
    stop_host.store(true, Ordering::Release);
    drop(client);
    let _ = host_jh.join();

    let mut samples_ms: Vec<f64> = obs
        .frames
        .iter()
        .filter_map(|f| f.presented_us.map(|p| p.saturating_sub(f.captured_us)))
        .skip(WARMUP_FRAMES)
        .map(|us| us as f64 / 1000.0)
        .collect();

    assert!(
        samples_ms.len() >= 200,
        "{label}: expected >= 200 steady frames after warmup, got {}",
        samples_ms.len()
    );
    samples_ms.sort_by(|a, b| a.total_cmp(b));
    let n = samples_ms.len();
    let p50_ms = samples_ms[n / 2];
    let p99_idx = ((n as f64) * 0.99).floor() as usize;
    let p99_ms = samples_ms[p99_idx.min(n - 1)];

    LatencyStats {
        frames: n,
        p50_ms,
        p99_ms,
    }
}

#[test]
fn mtls_latency_comparison_benchmark() {
    let mut ca = BenchCa::new();
    let (srv_cert, srv_key) = ca.issue_server_loopback();
    let (h_cert, h_key) = ca.issue_device("bench-uid", "200000002");
    let (c_cert, c_key) = ca.issue_device("bench-uid", "100000001");

    let host_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: h_cert,
        key_pem: h_key,
        local_user_id: "bench-uid".to_string(),
        expected_peer_device_id: Some("100000001".to_string()),
        expected_peer_user_id: Some("bench-uid".to_string()),
    };
    let client_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: c_cert,
        key_pem: c_key,
        local_user_id: "bench-uid".to_string(),
        expected_peer_device_id: Some("200000002".to_string()),
        expected_peer_user_id: Some("bench-uid".to_string()),
    };

    // ── Case 1: UDP LAN/P2P 非 mTLS vs QUIC mTLS ──
    let host_ep_plain = QuicEndpoint::new(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM).unwrap(),
    )
    .unwrap();
    let host_addr_plain = host_ep_plain.local_addr().unwrap();
    let h_plain_jh = thread::spawn(move || host_ep_plain.accept(Duration::from_secs(5)).unwrap());
    let client_ep_plain = QuicEndpoint::new(
        &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
        &Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM).unwrap(),
    )
    .unwrap();
    let client_t_plain = client_ep_plain
        .connect(
            host_addr_plain,
            &Identity::certs_from_pem(dev_server_cert_pem()).unwrap(),
            DEV_SERVER_NAME,
            Duration::from_secs(5),
        )
        .unwrap();
    let host_t_plain = h_plain_jh.join().unwrap();
    let udp_plain = measure_e2e_session(
        Box::new(host_t_plain),
        Box::new(client_t_plain),
        "UDP QUIC non-mTLS",
    );

    let host_mtls_cfg = host_bundle.to_quic_mtls_config().unwrap();
    let client_mtls_cfg = client_bundle.to_quic_mtls_config().unwrap();
    let host_ep_mtls = QuicEndpoint::new_mtls(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &host_mtls_cfg,
    )
    .unwrap();
    let host_addr_mtls = host_ep_mtls.local_addr().unwrap();
    let h_mtls_jh = thread::spawn(move || host_ep_mtls.accept(Duration::from_secs(5)).unwrap());
    let client_ep_mtls = QuicEndpoint::new(
        &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
        &Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM).unwrap(),
    )
    .unwrap();
    let client_t_mtls = client_ep_mtls
        .connect_mtls(host_addr_mtls, &client_mtls_cfg, "", Duration::from_secs(5))
        .unwrap();
    let host_t_mtls = h_mtls_jh.join().unwrap();
    let udp_mtls = measure_e2e_session(
        Box::new(host_t_mtls),
        Box::new(client_t_mtls),
        "UDP QUIC mTLS",
    );

    // ── Case 2: TCP Relay (仅外层 mTLS vs 外层 mTLS + 内层 E2EE mTLS) ──
    let (relay1_addr, relay1_stop, relay1_jh) =
        spawn_low_jitter_mtls_relay(&ca.ca_pem, &srv_cert, &srv_key);
    let h_relay_mtls = host_bundle.to_relay_mtls_config(None);
    let c_relay_mtls = client_bundle.to_relay_mtls_config(None);
    let r1_addr_s = relay1_addr.to_string();
    let r1_addr_s2 = r1_addr_s.clone();
    let h_r1 = thread::spawn(move || {
        TcpRelayTransport::connect_mtls_cancellable(
            &r1_addr_s2,
            RelayRole::Host,
            "bench-relay-outer",
            Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
            &h_relay_mtls,
        )
        .unwrap()
    });
    let c_r1 = TcpRelayTransport::connect_mtls_cancellable(
        &r1_addr_s,
        RelayRole::Client,
        "bench-relay-outer",
        Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
        &c_relay_mtls,
    )
    .unwrap();
    let h_r1 = h_r1.join().unwrap();
    let relay_outer = measure_e2e_session(Box::new(h_r1), Box::new(c_r1), "TCP Relay outer mTLS");
    relay1_stop.store(true, Ordering::Release);
    let _ = relay1_jh.join();

    let (relay2_addr, relay2_stop, relay2_jh) =
        spawn_low_jitter_mtls_relay(&ca.ca_pem, &srv_cert, &srv_key);
    let r2_addr_s = relay2_addr.to_string();
    let r2_addr_s2 = r2_addr_s.clone();
    let hb_c = host_bundle.clone();
    let cb_c = client_bundle.clone();
    let h_r2 = thread::spawn(move || {
        E2eeRelayTransport::connect_cancellable(
            &r2_addr_s2,
            RelayRole::Host,
            "bench-relay-e2ee",
            Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
            Some(&hb_c.to_relay_mtls_config(None)),
            RelayE2eeMode::HostRequired(hb_c),
        )
        .unwrap()
    });
    let c_r2 = E2eeRelayTransport::connect_cancellable(
        &r2_addr_s,
        RelayRole::Client,
        "bench-relay-e2ee",
        Duration::from_secs(5),
        Arc::new(AtomicBool::new(false)),
        Some(&cb_c.to_relay_mtls_config(None)),
        RelayE2eeMode::ClientRequired(cb_c),
    )
    .unwrap();
    let h_r2 = h_r2.join().unwrap();
    let relay_e2ee = measure_e2e_session(
        Box::new(h_r2),
        Box::new(c_r2),
        "TCP Relay outer + inner E2EE mTLS",
    );
    relay2_stop.store(true, Ordering::Release);
    let _ = relay2_jh.join();

    let udp_delta_p50_ms = udp_mtls.p50_ms - udp_plain.p50_ms;
    let relay_delta_p50_ms = relay_e2ee.p50_ms - relay_outer.p50_ms;

    println!(
        "\n=== mTLS Latency Benchmark (steady frames >= 200, 64x64 @ {BENCH_FPS}fps mock) ==="
    );
    println!(
        "Case 1 [UDP LAN/P2P] non-mTLS : frames={} p50={:.3}ms p99={:.3}ms",
        udp_plain.frames, udp_plain.p50_ms, udp_plain.p99_ms
    );
    println!(
        "Case 1 [UDP LAN/P2P] QUIC mTLS: frames={} p50={:.3}ms p99={:.3}ms delta_p50_ms={:+.3}ms",
        udp_mtls.frames, udp_mtls.p50_ms, udp_mtls.p99_ms, udp_delta_p50_ms
    );
    println!(
        "Case 2 [TCP Relay] outer mTLS only       : frames={} p50={:.3}ms p99={:.3}ms",
        relay_outer.frames, relay_outer.p50_ms, relay_outer.p99_ms
    );
    println!(
        "Case 2 [TCP Relay] outer + E2EE mTLS     : frames={} p50={:.3}ms p99={:.3}ms delta_p50_ms={:+.3}ms",
        relay_e2ee.frames, relay_e2ee.p50_ms, relay_e2ee.p99_ms, relay_delta_p50_ms
    );
}
