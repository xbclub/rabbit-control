#![deny(unsafe_code)]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rc_net::quic::Identity;
use rc_session::auth::OwnerTicketSigner;
use rc_ui::cert_manager::{
    owner_grant_signing_message, OwnerGrantPayload, RcOwnerTicketSigner, RcOwnerTicketVerifier,
};
use rc_ui::host_bridge::{HostHandle, HostUpdate, SessionMtlsBundle, UiHostConfig};
use rc_ui::session_bridge::{SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig};
use rc_ui::signal_client::{ConnectGrant, RelayIncoming};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

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

struct TestCa {
    ca_pem: String,
    ca_dn: Vec<u8>,
    ca_kp: EcdsaKeyPair,
    next_serial: u8,
}

impl TestCa {
    fn new(cn: &str) -> Self {
        let rng = SystemRandom::new();
        let pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let ca_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pk8.as_ref(), &rng).unwrap();
        let ca_spki = build_ec_spki_der(ca_kp.public_key().as_ref());
        let ca_dn = build_cn_dn_der(cn);
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

fn spawn_mtls_rcr1_relay(
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while streams.len() < 2 && Instant::now() < deadline && !stop_c.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((tcp, _)) => {
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
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
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
        let mut buf = [0u8; 16384];
        while !stop_c.load(Ordering::Acquire) && Instant::now() < deadline {
            let mut active = false;
            match s0.read(&mut buf) {
                Ok(0) => {
                    let _ = s1_tcp.shutdown(Shutdown::Write);
                    break;
                }
                Ok(n) => {
                    s1.sock.set_nonblocking(false).unwrap();
                    let ok = s1.write_all(&buf[..n]).is_ok() && s1.flush().is_ok();
                    s1.sock.set_nonblocking(true).unwrap();
                    if !ok {
                        break;
                    }
                    active = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            match s1.read(&mut buf) {
                Ok(0) => {
                    let _ = s0_tcp.shutdown(Shutdown::Write);
                    break;
                }
                Ok(n) => {
                    s0.sock.set_nonblocking(false).unwrap();
                    let ok = s0.write_all(&buf[..n]).is_ok() && s0.flush().is_ok();
                    s0.sock.set_nonblocking(true).unwrap();
                    if !ok {
                        break;
                    }
                    active = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            if !active {
                thread::sleep(Duration::from_millis(1));
            }
        }
    });

    (addr, stop, handle)
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
    updates
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
    updates
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn tier1_udp_lan_mtls_prioritized_over_p2p_and_relay() {
    let mut ca = TestCa::new("Tier1LanCA");
    let (h_cert, h_key) = ca.issue_device("u1", "200000002");
    let (c_cert, c_key) = ca.issue_device("u1", "100000001");

    let host_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: h_cert,
        key_pem: h_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let client_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem,
        cert_pem: c_cert,
        key_pem: c_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: Some("200000002".to_string()),
        expected_peer_user_id: Some("u1".to_string()),
    };

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        password: Some("LAN-MTLS-26".to_string()),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        mtls_config: Some(host_bundle),
        accept_mtls_lan: true,
        ..UiHostConfig::default()
    })
    .expect("spawn mTLS LAN host");

    // 传入不可达的 P2P 与 Relay，若 Tier 1 UDP LAN 未命中必定失败；命中则在 <300ms 完成 mTLS LAN 建连。
    let session = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().unwrap(),
        password: Some("LAN-MTLS-26".to_string()),
        watch_only: false,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 2_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: true,
        mtls_config: Some(client_bundle),
        lan_direct_addr: Some(host.listen_addr()),
        relay_grant: Some(ConnectGrant {
            session_id: "sess-lan-tier1".to_string(),
            relay_addr: "127.0.0.1:1".to_string(),
            relay_token: "unused".to_string(),
            target_id: "200000002".to_string(),
            peer_public_addr: Some("127.0.0.1:1".to_string()),
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });

    let updates = collect_client_until(&session, Duration::from_secs(6), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated {
                transport,
                mtls_active: true,
                peer_cert_fingerprint: Some(_),
                ..
            } if transport == "UDP LAN"
        )),
        "expected Tier 1 UDP LAN with mTLS active, got: {updates:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
}

#[test]
fn encrypt_connection_refuses_downgrade_on_missing_or_bad_cert() {
    // 1) encrypt_connection=true 但 mtls_config=None 立即失败，不降级明文
    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn plain host");

    let s1 = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        width: 64,
        height: 64,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: true,
        mtls_config: None,
        ..UiSessionConfig::default()
    });
    let u1 = collect_client_until(&s1, Duration::from_secs(3), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(u1.iter().any(|u| matches!(u, SessionUpdate::Ended { .. })));
    assert!(!u1
        .iter()
        .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));
    s1.join();
    host.stop();

    // 2) LAN mTLS 主机证书 SAN device_id 与预期不符时严禁降级到 P2P/Relay
    let mut ca = TestCa::new("NoDowngradeCA");
    let (h_cert, h_key) = ca.issue_device("u1", "200000002");
    let (c_cert, c_key) = ca.issue_device("u1", "100000001");
    let host_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: h_cert,
        key_pem: h_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let bad_client_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem,
        cert_pem: c_cert,
        key_pem: c_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: Some("999999999".to_string()),
        expected_peer_user_id: None,
    };

    let mut m_host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        mtls_config: Some(host_bundle),
        accept_mtls_lan: true,
        ..UiHostConfig::default()
    })
    .expect("spawn mTLS host");

    let s2 = SessionHandle::spawn(UiSessionConfig {
        connect_addr: m_host.listen_addr(),
        width: 64,
        height: 64,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: true,
        mtls_config: Some(bad_client_bundle),
        lan_direct_addr: Some(m_host.listen_addr()),
        relay_grant: Some(ConnectGrant {
            session_id: "sess-no-downgrade".to_string(),
            relay_addr: "127.0.0.1:1".to_string(),
            relay_token: "unused".to_string(),
            target_id: "200000002".to_string(),
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });
    let u2 = collect_client_until(&s2, Duration::from_secs(4), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        u2.iter()
            .any(|u| matches!(u, SessionUpdate::Ended { reason } if reason.contains("mTLS") || reason.contains("证书"))),
        "cert validation error must abort without fallback, got: {u2:?}"
    );
    s2.join();
    m_host.stop();
}

#[test]
fn owner_ticket_pop_bypasses_password_and_allow_remote_assist_false() {
    let mut ca = TestCa::new("OwnerPopRelayCA");
    let (srv_cert, srv_key) = ca.issue_server_loopback();
    let (h_cert, h_key) = ca.issue_device("owner-uid", "200000002");
    let (c_cert, c_key) = ca.issue_device("owner-uid", "100000001");

    let rng = SystemRandom::new();
    let auth_srv_pk8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let auth_srv_kp = Ed25519KeyPair::from_pkcs8(auth_srv_pk8.as_ref()).unwrap();
    let auth_srv_pub_hex = hex_lower(auth_srv_kp.public_key().as_ref());

    let caller_pk8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let caller_kp = Ed25519KeyPair::from_pkcs8(caller_pk8.as_ref()).unwrap();
    let caller_pub_hex = hex_lower(caller_kp.public_key().as_ref());

    let verifier = Arc::new(
        RcOwnerTicketVerifier::new(
            &auth_srv_pub_hex,
            "owner-uid".to_string(),
            "200000002".to_string(),
        )
        .unwrap(),
    );

    let host_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: h_cert,
        key_pem: h_key,
        local_user_id: "owner-uid".to_string(),
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let client_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: c_cert,
        key_pem: c_key,
        local_user_id: "owner-uid".to_string(),
        expected_peer_device_id: Some("200000002".to_string()),
        expected_peer_user_id: Some("owner-uid".to_string()),
    };

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        password: Some("ASSIST-SECRET".to_string()),
        require_approval: true,
        allow_remote_assist: false,
        owner_verifier: Some(verifier),
        mtls_config: Some(host_bundle),
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn owner-only host");

    // 1) 非同账号他人（即使填对了密码，也无 OwnerTicket）：allow_remote_assist=false 下直接被拒绝且不弹审批
    let non_owner = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        password: Some("ASSIST-SECRET".to_string()),
        width: 64,
        height: 64,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiSessionConfig::default()
    });
    let non_owner_updates = collect_client_until(&non_owner, Duration::from_secs(5), |us| {
        us.iter().any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        !non_owner_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "non-owner must be rejected when allow_remote_assist=false"
    );
    non_owner.join();

    // 2) 同账号本人自控（无密码 + E2EE mTLS Relay + OwnerTicket PoP）免密免审批直通
    let (relay_addr, relay_stop, relay_jh) = spawn_mtls_rcr1_relay(&ca.ca_pem, &srv_cert, &srv_key);
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut grant_payload = OwnerGrantPayload {
        ticket_id: "aa".repeat(16),
        user_id: "owner-uid".to_string(),
        caller_device_id: "100000001".to_string(),
        caller_pubkey_hex: caller_pub_hex,
        target_device_id: "200000002".to_string(),
        session_id: "owner-relay-sess".to_string(),
        expires_at_unix_sec: now_unix + 300,
        signature_hex: String::new(),
    };
    let sign_msg = owner_grant_signing_message(&grant_payload).unwrap();
    grant_payload.signature_hex = hex_lower(auth_srv_kp.sign(&sign_msg).as_ref());
    let signer: Arc<dyn OwnerTicketSigner> = Arc::new(
        RcOwnerTicketSigner::new(grant_payload.clone(), caller_pk8.as_ref().to_vec()).unwrap(),
    );

    host.offer_relay_incoming(RelayIncoming {
        session_id: "owner-relay-sess".to_string(),
        relay_addr: relay_addr.to_string(),
        relay_token: "owner-mtls-tok".to_string(),
        peer_id: "100000001".to_string(),
        is_owner_connection: true,
        ..RelayIncoming::default()
    });

    let owner_sess = SessionHandle::spawn(UiSessionConfig {
        connect_addr: "0.0.0.0:0".parse().unwrap(),
        password: None,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 2_000_000,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: true,
        mtls_config: Some(client_bundle),
        owner_grant: Some(grant_payload),
        owner_signer: Some(signer),
        force_relay: true,
        relay_grant: Some(ConnectGrant {
            session_id: "owner-relay-sess".to_string(),
            relay_addr: relay_addr.to_string(),
            relay_token: "owner-mtls-tok".to_string(),
            target_id: "200000002".to_string(),
            is_owner_connection: true,
            ..ConnectGrant::default()
        }),
        ..UiSessionConfig::default()
    });

    let client_updates = collect_client_until(&owner_sess, Duration::from_secs(8), |us| {
        us.iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0))
    });
    assert!(
        client_updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated {
                transport,
                mtls_active: true,
                peer_cert_fingerprint: Some(_),
                ..
            } if transport == "TCP Relay"
        )),
        "owner must reach Negotiated over E2EE mTLS Relay, got: {client_updates:?}"
    );

    let host_updates = collect_host_until(&host, Duration::from_secs(5), |us| {
        us.iter()
            .any(|u| matches!(u, HostUpdate::OwnerConnected { .. }))
    });
    assert!(
        host_updates.iter().any(|u| matches!(
            u,
            HostUpdate::OwnerConnected { caller_device_id, .. } if caller_device_id == "100000001"
        )),
        "host must emit OwnerConnected without approval prompt, got: {host_updates:?}"
    );
    assert!(
        !host_updates
            .iter()
            .any(|u| matches!(u, HostUpdate::ApprovalRequested(_))),
        "owner connection must NEVER trigger ApprovalRequested"
    );

    owner_sess.send(SessionCommand::Disconnect);
    owner_sess.join();
    host.stop();
    relay_stop.store(true, Ordering::Release);
    let _ = relay_jh.join();
}

#[test]
fn dynamic_mtls_switch_preserves_params_and_reconnects() {
    let mut ca = TestCa::new("SwitchCA");
    let (h_cert, h_key) = ca.issue_device("u1", "200000002");
    let (c_cert, c_key) = ca.issue_device("u1", "100000001");
    let host_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: h_cert,
        key_pem: h_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let client_bundle = SessionMtlsBundle {
        ca_cert_pem: ca.ca_pem.clone(),
        cert_pem: c_cert,
        key_pem: c_key,
        local_user_id: "u1".to_string(),
        expected_peer_device_id: Some("200000002".to_string()),
        expected_peer_user_id: None,
    };

    let host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        mtls_config: Some(host_bundle),
        accept_mtls_lan: false,
        ..UiHostConfig::default()
    })
    .expect("spawn plain host first");

    let s1 = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_bps: 6_000_000,
        watch_only: true,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: false,
        mtls_config: Some(client_bundle),
        ..UiSessionConfig::default()
    });
    assert!(!s1.encrypt_connection());
    let u1 = collect_client_until(&s1, Duration::from_secs(6), |us| {
        us.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::Negotiated {
                    mtls_active: false,
                    ..
                }
            )
        })
    });
    assert!(u1.iter().any(|u| matches!(
        u,
        SessionUpdate::Negotiated {
            mtls_active: false,
            ..
        }
    )));

    let next_cfg = s1.config_with_encrypt_connection(true);
    assert!(next_cfg.encrypt_connection);
    assert_eq!(next_cfg.width, 1280);
    assert_eq!(next_cfg.height, 720);
    assert!(next_cfg.watch_only);

    s1.send(SessionCommand::Disconnect);
    s1.join();

    // 切换主机监听为 LAN mTLS 并重连
    host.set_mtls_config(
        None, false, // 先验证切换前不误用
    );
    drop(host);
    // 重新以相同 bundle 在 mTLS LAN 上监听
    let host_bundle2 = SessionMtlsBundle {
        ca_cert_pem: next_cfg.mtls_config.as_ref().unwrap().ca_cert_pem.clone(),
        cert_pem: next_cfg.mtls_config.as_ref().unwrap().cert_pem.clone(),
        key_pem: next_cfg.mtls_config.as_ref().unwrap().key_pem.clone(),
        local_user_id: "u1".to_string(),
        expected_peer_device_id: None,
        expected_peer_user_id: None,
    };
    let _ = host_bundle2;
    let (h_cert2, h_key2) = ca.issue_device("u1", "200000002");
    let mut host2 = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        mtls_config: Some(SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem.clone(),
            cert_pem: h_cert2,
            key_pem: h_key2,
            local_user_id: "u1".to_string(),
            expected_peer_device_id: None,
            expected_peer_user_id: None,
        }),
        accept_mtls_lan: true,
        ..UiHostConfig::default()
    })
    .expect("spawn mTLS LAN host");
    let mut upgraded_cfg = next_cfg;
    upgraded_cfg.connect_addr = host2.listen_addr();
    let s2 = SessionHandle::spawn(upgraded_cfg);
    assert!(s2.encrypt_connection());
    let u2 = collect_client_until(&s2, Duration::from_secs(6), |us| {
        us.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::Negotiated {
                    mtls_active: true,
                    peer_cert_fingerprint: Some(_),
                    ..
                }
            )
        })
    });
    assert!(u2.iter().any(|u| matches!(
        u,
        SessionUpdate::Negotiated {
            mtls_active: true,
            peer_cert_fingerprint: Some(_),
            watch_only: true,
            ..
        }
    )));
    s2.send(SessionCommand::Disconnect);
    s2.join();
    host2.stop();
}

#[test]
fn awaiting_mtls_cert_never_downgrades_to_plain_and_recovers_once_cert_installed() {
    let mut ca = TestCa::new("awaiting-mtls-ca");
    let (h_cert, h_key) = ca.issue_device("u1", "200000002");
    let (c_cert, c_key) = ca.issue_device("u1", "100000001");

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        password: Some("MTLS-FAIL-CLOSED".to_string()),
        require_approval: false,
        clipboard_enabled: false,
        audio_enabled: false,
        mtls_config: None,
        accept_mtls_lan: true,
        ..UiHostConfig::default()
    })
    .expect("spawn host awaiting mTLS cert");

    // 1) 未签发证书前禁止静默降级为 Plain：明文连接不得成功协商
    let plain_cfg = UiSessionConfig {
        connect_addr: host.listen_addr(),
        password: Some("MTLS-FAIL-CLOSED".to_string()),
        watch_only: true,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 2_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: false,
        ..UiSessionConfig::default()
    };
    let s_plain = SessionHandle::spawn(plain_cfg);
    let plain_updates = collect_client_until(&s_plain, Duration::from_millis(600), |us| {
        us.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::Negotiated { .. } | SessionUpdate::Ended { .. }
            )
        })
    });
    assert!(
        !plain_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "awaiting mTLS host must never accept plain downgrade"
    );
    s_plain.send(SessionCommand::Disconnect);
    s_plain.join();

    // 2) 补齐证书并热更新后，自动切换至 MtlsLan 并完成 mTLS 建连
    host.set_mtls_config(
        Some(SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem.clone(),
            cert_pem: h_cert,
            key_pem: h_key,
            local_user_id: "u1".to_string(),
            expected_peer_device_id: None,
            expected_peer_user_id: None,
        }),
        true,
    );
    std::thread::sleep(Duration::from_millis(250));

    let s_mtls = SessionHandle::spawn(UiSessionConfig {
        connect_addr: host.listen_addr(),
        password: Some("MTLS-FAIL-CLOSED".to_string()),
        watch_only: true,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_bps: 2_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        encrypt_connection: true,
        mtls_config: Some(SessionMtlsBundle {
            ca_cert_pem: ca.ca_pem,
            cert_pem: c_cert,
            key_pem: c_key,
            local_user_id: "u1".to_string(),
            expected_peer_device_id: Some("200000002".to_string()),
            expected_peer_user_id: Some("u1".to_string()),
        }),
        ..UiSessionConfig::default()
    });
    let mtls_updates = collect_client_until(&s_mtls, Duration::from_secs(6), |us| {
        us.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::Negotiated {
                    mtls_active: true,
                    ..
                }
            )
        })
    });
    assert!(mtls_updates.iter().any(|u| matches!(
        u,
        SessionUpdate::Negotiated {
            mtls_active: true,
            ..
        }
    )));
    s_mtls.send(SessionCommand::Disconnect);
    s_mtls.join();
    host.stop();
}
