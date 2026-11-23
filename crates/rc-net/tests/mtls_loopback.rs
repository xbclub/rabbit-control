//! 双向 mTLS 集成测试与 mTLS vs 非 mTLS 延迟对比基准。
//!
//! 覆盖：
//! 1. 同 CA 签发双端设备证书的 mTLS 握手、三通道（datagram / reliable / urgent）互通、
//!    `peer_mtls_identity()` 提取 `user_id` / `device_id` / SHA-256 指纹精确匹配；
//! 2. 缺失客户端证书被拒、非同 CA 伪造证书被拒；
//! 3. 缺 SAN URI 被拒、`expected_peer_device_id` / `expected_peer_user_id` 不匹配被拒；
//! 4. `from_socket_mtls` 与 `connect_mtls_cancelable` 行为；
//! 5. `non-mtls` vs `mtls` 握手耗时与 200 帧 datagram + reliable RTT 稳态延迟对比基准。

mod common;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rc_net::quic::{
    cert_sha256_hex, parse_rc_san_uri, Identity, MtlsConfig, PeerMtlsIdentity, QuicEndpoint,
};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use ring::pkcs8::Document;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::CertificateDer;

const TIMEOUT: Duration = Duration::from_secs(10);

// ─────────────────────────────────────────────────────────────────
// 极简纯 Rust ECDSA P-256 X.509 v3 测试证书生成器（不依赖外部 CLI）
// ─────────────────────────────────────────────────────────────────

fn der_len(len: usize) -> Vec<u8> {
    if len < 128 {
        vec![len as u8]
    } else if len <= 0xff {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, (len & 0xff) as u8]
    }
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 3 + body.len());
    out.push(tag);
    out.extend_from_slice(&der_len(body.len()));
    out.extend_from_slice(body);
    out
}

fn der_seq(parts: &[&[u8]]) -> Vec<u8> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut body = Vec::with_capacity(total);
    for p in parts {
        body.extend_from_slice(p);
    }
    der_tlv(0x30, &body)
}

fn der_bit_string(bytes: &[u8], unused_bits: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + bytes.len());
    body.push(unused_bits);
    body.extend_from_slice(bytes);
    der_tlv(0x03, &body)
}

fn der_oid(oid: &[u8]) -> Vec<u8> {
    der_tlv(0x06, oid)
}

fn der_cn(cn: &str) -> Vec<u8> {
    // CN = 2.5.4.3 -> [0x55, 0x04, 0x03]
    let atv = der_seq(&[&der_oid(&[0x55, 0x04, 0x03]), &der_tlv(0x0C, cn.as_bytes())]);
    let rdn = der_tlv(0x31, &atv);
    der_seq(&[&rdn])
}

fn der_validity() -> Vec<u8> {
    // 2024-01-01 .. 2049-12-31 (UTCTime)
    let not_before = der_tlv(0x17, b"240101000000Z");
    let not_after = der_tlv(0x17, b"491231235959Z");
    der_seq(&[&not_before, &not_after])
}

fn der_spki_p256(pub_key: &[u8]) -> Vec<u8> {
    // id-ecPublicKey (1.2.840.10045.2.1), prime256v1 (1.2.840.10045.3.1.7)
    let alg = der_seq(&[
        &der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]),
        &der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]),
    ]);
    let bits = der_bit_string(pub_key, 0);
    der_seq(&[&alg, &bits])
}

fn der_ext(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let oid_tlv = der_oid(oid);
    let val_tlv = der_tlv(0x04, value);
    if critical {
        let crit = der_tlv(0x01, &[0xFF]);
        der_seq(&[&oid_tlv, &crit, &val_tlv])
    } else {
        der_seq(&[&oid_tlv, &val_tlv])
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = *chunk.get(1).unwrap_or(&0) as usize;
        let b2 = *chunk.get(2).unwrap_or(&0) as usize;
        out.push(TABLE[b0 >> 2] as char);
        out.push(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn to_pem(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

struct TestCa {
    cn: String,
    key_pkcs8: Document,
    cert_der: CertificateDer<'static>,
    next_serial: u64,
}

impl TestCa {
    fn new(cn: &str) -> Self {
        let rng = SystemRandom::new();
        let key_pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("keygen");
        let kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key_pkcs8.as_ref(), &rng)
                .expect("from_pkcs8");

        // BasicConstraints(cA=TRUE), KeyUsage(keyCertSign|cRLSign)
        let bc = der_ext(
            &[0x55, 0x1D, 0x13],
            true,
            &der_seq(&[&der_tlv(0x01, &[0xFF])]),
        );
        let ku = der_ext(&[0x55, 0x1D, 0x0F], true, &der_bit_string(&[0x06], 1));
        let exts = der_seq(&[&bc, &ku]);

        let cert_der = sign_tbs(cn, cn, 1, kp.public_key().as_ref(), &exts, &kp, &rng);
        Self {
            cn: cn.to_string(),
            key_pkcs8,
            cert_der,
            next_serial: 2,
        }
    }

    fn issue_device_cert(
        &mut self,
        cn: &str,
        san_dns: Option<&str>,
        san_uri: Option<&str>,
    ) -> Identity {
        let rng = SystemRandom::new();
        let leaf_pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).expect("leaf key");
        let leaf_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, leaf_pkcs8.as_ref(), &rng)
                .expect("leaf from_pkcs8");
        let ca_kp = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            self.key_pkcs8.as_ref(),
            &rng,
        )
        .expect("ca from_pkcs8");

        let serial = self.next_serial;
        self.next_serial += 1;

        let bc = der_ext(&[0x55, 0x1D, 0x13], true, &der_seq(&[]));
        let ku = der_ext(&[0x55, 0x1D, 0x0F], true, &der_bit_string(&[0x80], 7));
        // EKU: serverAuth + clientAuth
        let eku = der_ext(
            &[0x55, 0x1D, 0x25],
            false,
            &der_seq(&[
                &der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01]),
                &der_oid(&[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02]),
            ]),
        );

        let mut ext_list = vec![bc, ku, eku];
        let mut san_items = Vec::new();
        if let Some(dns) = san_dns {
            san_items.push(der_tlv(0x82, dns.as_bytes()));
        }
        if let Some(uri) = san_uri {
            san_items.push(der_tlv(0x86, uri.as_bytes()));
        }
        if !san_items.is_empty() {
            let san_body = der_tlv(0x30, &san_items.concat());
            ext_list.push(der_ext(&[0x55, 0x1D, 0x11], false, &san_body));
        }

        let ext_refs: Vec<&[u8]> = ext_list.iter().map(Vec::as_slice).collect();
        let exts = der_seq(&ext_refs);

        let cert_der = sign_tbs(
            &self.cn,
            cn,
            serial,
            leaf_kp.public_key().as_ref(),
            &exts,
            &ca_kp,
            &rng,
        );

        let cert_pem = to_pem("CERTIFICATE", cert_der.as_ref());
        let key_pem = to_pem("PRIVATE KEY", leaf_pkcs8.as_ref());
        Identity::from_pem(&cert_pem, &key_pem).expect("Identity::from_pem")
    }

    fn issue_standard(&mut self, user_id: &str, device_id: &str) -> Identity {
        let dns = format!("device-{device_id}.rc.internal");
        let uri = format!("urn:rabbitcontrol:user:{user_id}:device:{device_id}");
        self.issue_device_cert(&dns, Some(&dns), Some(&uri))
    }
}

fn sign_tbs(
    issuer_cn: &str,
    subject_cn: &str,
    serial: u64,
    pub_key: &[u8],
    exts: &[u8],
    signer: &EcdsaKeyPair,
    rng: &SystemRandom,
) -> CertificateDer<'static> {
    // ecdsa-with-SHA256 = 1.2.840.10045.4.3.2
    let sig_alg = der_seq(&[&der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02])]);
    let version = der_tlv(0xA0, &der_tlv(0x02, &[0x02])); // v3
    let serial_bytes = serial.to_be_bytes();
    let first_nz = serial_bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(serial_bytes.len() - 1);
    let mut serial_val = serial_bytes[first_nz..].to_vec();
    if serial_val[0] & 0x80 != 0 {
        serial_val.insert(0, 0x00);
    }
    let serial_tlv = der_tlv(0x02, &serial_val);
    let issuer = der_cn(issuer_cn);
    let validity = der_validity();
    let subject = der_cn(subject_cn);
    let spki = der_spki_p256(pub_key);
    let exts_tlv = der_tlv(0xA3, exts);

    let tbs = der_seq(&[
        &version,
        &serial_tlv,
        &sig_alg,
        &issuer,
        &validity,
        &subject,
        &spki,
        &exts_tlv,
    ]);
    let sig = signer.sign(rng, &tbs).expect("sign tbs");
    let sig_bits = der_bit_string(sig.as_ref(), 0);
    let full = der_seq(&[&tbs, &sig_alg, &sig_bits]);
    CertificateDer::from(full)
}

#[test]
fn mtls_handshake_three_channels_and_peer_identity() {
    let mut ca = TestCa::new("RabbitControl Root CA");
    let host_ident = ca.issue_standard("usr-alice", "host00001");
    let ctrl_ident = ca.issue_standard("usr-alice", "ctrl00002");

    let host_leaf_der = host_ident.cert_chain()[0].as_ref().to_vec();
    let ctrl_leaf_der = ctrl_ident.cert_chain()[0].as_ref().to_vec();
    assert_eq!(
        parse_rc_san_uri(&host_leaf_der),
        Some(("usr-alice".to_string(), "host00001".to_string()))
    );

    let mut host_mtls = MtlsConfig::new(host_ident, vec![ca.cert_der.clone()]);
    host_mtls.expected_peer_user_id = Some("usr-alice".into());
    host_mtls.expected_peer_device_id = Some("ctrl00002".into());

    let mut ctrl_mtls = MtlsConfig::new(ctrl_ident, vec![ca.cert_der.clone()]);
    ctrl_mtls.expected_peer_user_id = Some("usr-alice".into());
    ctrl_mtls.expected_peer_device_id = Some("host00001".into());

    let host_ep = QuicEndpoint::new_mtls(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &host_mtls,
    )
    .expect("host new_mtls");
    let host_addr = host_ep.local_addr().expect("host local_addr");

    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client sock");
    let client_addr = client_sock.local_addr().expect("client addr");
    let client_ep = QuicEndpoint::from_socket_mtls(
        client_sock,
        NetConfig::new(Role::Controller, client_addr),
        None,
    )
    .expect("client from_socket_mtls");

    let accepter = thread::spawn(move || host_ep.accept(TIMEOUT).expect("host accept"));
    // 故意测试 server_name 传空时由 expected_peer_device_id 自动推导 device-{did}.rc.internal
    let mut client = client_ep
        .connect_mtls(host_addr, &ctrl_mtls, "", TIMEOUT)
        .expect("client connect_mtls");
    let mut host = accepter.join().expect("accepter join");

    assert_eq!(
        host.peer_mtls_identity(),
        Some(PeerMtlsIdentity {
            user_id: "usr-alice".into(),
            device_id: "ctrl00002".into(),
            cert_fingerprint_sha256: cert_sha256_hex(&ctrl_leaf_der),
        })
    );
    assert_eq!(
        client.peer_mtls_identity(),
        Some(PeerMtlsIdentity {
            user_id: "usr-alice".into(),
            device_id: "host00001".into(),
            cert_fingerprint_sha256: cert_sha256_hex(&host_leaf_der),
        })
    );

    // 验证 datagram + reliable + urgent 三通道双向互通
    let dgram = b"mtls-dgram-frame".to_vec();
    let rel_h2c = b"mtls-reliable-h2c".to_vec();
    let rel_c2h = b"mtls-reliable-c2h".to_vec();
    let urg_c2h = b"mtls-urgent-c2h".to_vec();

    host.send_datagram(&dgram).expect("host send_datagram");
    host.send_reliable(&rel_h2c).expect("host send_reliable");
    client
        .send_reliable(&rel_c2h)
        .expect("client send_reliable");
    client
        .send_reliable_urgent(&urg_c2h)
        .expect("client send_reliable_urgent");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got_dgram = false;
    let mut got_rel_h2c = false;
    let mut got_rel_c2h = false;
    let mut got_urg_c2h = false;

    while Instant::now() < deadline && !(got_dgram && got_rel_h2c && got_rel_c2h && got_urg_c2h) {
        for ev in client.poll(Duration::from_millis(20)).expect("client poll") {
            match ev {
                NetEvent::Datagram(b) if b == dgram => got_dgram = true,
                NetEvent::Reliable(b) if b == rel_h2c => got_rel_h2c = true,
                _ => {}
            }
        }
        for ev in host.poll(Duration::from_millis(20)).expect("host poll") {
            match ev {
                NetEvent::Reliable(b) if b == rel_c2h => got_rel_c2h = true,
                NetEvent::ReliableUrgent(b) if b == urg_c2h => got_urg_c2h = true,
                _ => {}
            }
        }
    }

    assert!(got_dgram, "datagram must pass over mTLS");
    assert!(got_rel_h2c, "reliable h2c must pass over mTLS");
    assert!(got_rel_c2h, "reliable c2h must pass over mTLS");
    assert!(got_urg_c2h, "urgent c2h must pass over mTLS");
}

#[test]
fn mtls_rejects_missing_client_cert() {
    let mut ca = TestCa::new("RabbitControl Root CA");
    let host_ident = ca.issue_standard("usr-alice", "host00001");
    let host_cert = host_ident.cert_chain().to_vec();
    let host_mtls = MtlsConfig::new(host_ident, vec![ca.cert_der.clone()]);

    let host_ep = QuicEndpoint::new_mtls(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &host_mtls,
    )
    .expect("host ep");
    let host_addr = host_ep.local_addr().expect("host addr");

    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let client_addr = client_sock.local_addr().expect("addr");
    let client_ep = QuicEndpoint::from_socket(
        client_sock,
        NetConfig::new(Role::Controller, client_addr),
        None,
    )
    .expect("client ep");

    let accepter = thread::spawn(move || host_ep.accept(Duration::from_secs(3)));
    // 使用非 mTLS 的 connect（不提供客户端证书）
    let client_res = client_ep.connect(
        host_addr,
        &host_cert,
        "device-host00001.rc.internal",
        Duration::from_secs(3),
    );
    let host_res = accepter.join().expect("accepter join");

    assert!(
        host_res.is_err(),
        "mTLS host must reject client without client certificate"
    );
    // QUIC TLS 1.3 客户端可能在收到 CertificateRequest 后先完成 1-RTT 建连再在后续读到 alert，
    // 或者直接在握手/引导流写入阶段报错；无论哪种情况对端 host 必须拒绝。
    if let Ok(mut client) = client_res {
        let mut saw_disconnect = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_disconnect {
            if client.send_reliable(b"probe").is_err() {
                saw_disconnect = true;
                break;
            }
            for ev in client.poll(Duration::from_millis(20)).unwrap_or_default() {
                if matches!(ev, NetEvent::Disconnected { .. }) {
                    saw_disconnect = true;
                }
            }
        }
        assert!(
            saw_disconnect,
            "client without cert must observe connection rejection/closure"
        );
    }
}

#[test]
fn mtls_rejects_wrong_ca_and_san_violations() {
    let mut ca_a = TestCa::new("RabbitControl CA A");
    let mut ca_b = TestCa::new("RabbitControl CA B");

    let host_ident_a = ca_a.issue_standard("usr-alice", "host00001");
    let ctrl_ident_a = ca_a.issue_standard("usr-alice", "ctrl00002");
    let ctrl_ident_b = ca_b.issue_standard("usr-alice", "ctrl00002");
    let ctrl_no_uri = ca_a.issue_device_cert(
        "device-ctrl00002.rc.internal",
        Some("device-ctrl00002.rc.internal"),
        None,
    );
    let host_no_uri = ca_a.issue_device_cert(
        "device-host00001.rc.internal",
        Some("device-host00001.rc.internal"),
        None,
    );

    // 1) 非同 CA 客户端证书被 Host 拒绝
    {
        let host_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "host00001"),
            vec![ca_a.cert_der.clone()],
        );
        let ctrl_mtls = MtlsConfig::new(ctrl_ident_b, vec![ca_a.cert_der.clone()]);
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 2) 非同 CA 服务端证书被 Controller 拒绝
    {
        let host_mtls = MtlsConfig::new(
            ca_b.issue_standard("usr-alice", "host00001"),
            vec![ca_b.cert_der.clone()],
        );
        let ctrl_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "ctrl00002"),
            vec![ca_a.cert_der.clone()],
        );
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 3) 客户端证书缺少 SAN URI 被 Host 拒绝
    {
        let host_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "host00001"),
            vec![ca_a.cert_der.clone()],
        );
        let ctrl_mtls = MtlsConfig::new(ctrl_no_uri, vec![ca_a.cert_der.clone()]);
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 4) 服务端证书缺少 SAN URI 被 Controller 拒绝
    {
        let host_mtls = MtlsConfig::new(host_no_uri, vec![ca_a.cert_der.clone()]);
        let ctrl_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "ctrl00002"),
            vec![ca_a.cert_der.clone()],
        );
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 5) Host 期望 device_id 不匹配被拒绝
    {
        let mut host_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "host00001"),
            vec![ca_a.cert_der.clone()],
        );
        host_mtls.expected_peer_device_id = Some("ctrl99999".into());
        let ctrl_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "ctrl00002"),
            vec![ca_a.cert_der.clone()],
        );
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 6) Host 期望 user_id 不匹配被拒绝
    {
        let mut host_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "host00001"),
            vec![ca_a.cert_der.clone()],
        );
        host_mtls.expected_peer_user_id = Some("usr-bob".into());
        let ctrl_mtls = MtlsConfig::new(ctrl_ident_a, vec![ca_a.cert_der.clone()]);
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 7) Controller 期望 device_id 不匹配被拒绝
    {
        let host_mtls = MtlsConfig::new(host_ident_a, vec![ca_a.cert_der.clone()]);
        let mut ctrl_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "ctrl00002"),
            vec![ca_a.cert_der.clone()],
        );
        ctrl_mtls.expected_peer_device_id = Some("host99999".into());
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }

    // 8) Controller 期望 user_id 不匹配被拒绝
    {
        let host_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "host00001"),
            vec![ca_a.cert_der.clone()],
        );
        let mut ctrl_mtls = MtlsConfig::new(
            ca_a.issue_standard("usr-alice", "ctrl00002"),
            vec![ca_a.cert_der.clone()],
        );
        ctrl_mtls.expected_peer_user_id = Some("usr-bob".into());
        assert_handshake_rejected(host_mtls, ctrl_mtls, "device-host00001.rc.internal");
    }
}

fn assert_handshake_rejected(host_mtls: MtlsConfig, ctrl_mtls: MtlsConfig, server_name: &str) {
    let host_ep = QuicEndpoint::new_mtls(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &host_mtls,
    )
    .expect("host ep");
    let host_addr = host_ep.local_addr().expect("host addr");

    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let client_addr = client_sock.local_addr().expect("addr");
    let client_ep = QuicEndpoint::from_socket_mtls(
        client_sock,
        NetConfig::new(Role::Controller, client_addr),
        None,
    )
    .expect("client ep");

    let accepter = thread::spawn(move || host_ep.accept(Duration::from_secs(2)));
    let client_res =
        client_ep.connect_mtls(host_addr, &ctrl_mtls, server_name, Duration::from_secs(2));
    let host_res = accepter.join().expect("accepter join");

    assert!(
        client_res.is_err() || host_res.is_err(),
        "mTLS handshake must fail: client_err={}, host_err={}",
        client_res.is_err(),
        host_res.is_err()
    );
}

#[test]
fn mtls_connect_cancelable_aborts_when_cancelled() {
    let mut ca = TestCa::new("RabbitControl Root CA");
    let ctrl_ident = ca.issue_standard("usr-alice", "ctrl00002");
    let mut ctrl_mtls = MtlsConfig::new(ctrl_ident, vec![ca.cert_der.clone()]);
    ctrl_mtls.expected_peer_device_id = Some("host00001".into());

    // 绑定一个无人监听的 UDP 端口
    let dead_sock = UdpSocket::bind("127.0.0.1:0").expect("dead sock");
    let dead_addr = dead_sock.local_addr().expect("dead addr");

    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("client sock");
    let client_addr = client_sock.local_addr().expect("client addr");
    let client_ep = QuicEndpoint::from_socket_mtls(
        client_sock,
        NetConfig::new(Role::Controller, client_addr),
        None,
    )
    .expect("client ep");

    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let cancel_bg = std::sync::Arc::clone(&cancel);
    let cancel_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(60));
        cancel_bg.store(true, Ordering::Release);
    });

    let start = Instant::now();
    let res = client_ep.connect_mtls_cancelable(
        dead_addr,
        &ctrl_mtls,
        "",
        Duration::from_secs(10),
        &cancel,
    );
    let _ = cancel_thread.join();
    drop(dead_sock);
    assert!(res.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "cancelable connect took too long: {:?}",
        start.elapsed()
    );
}

#[test]
fn mtls_vs_non_mtls_latency_benchmark() {
    const HANDSHAKE_ROUNDS: usize = 10;
    const STEADY_FRAMES: usize = 200;

    // 预热消除 smol 执行器与调度器冷启动偏差
    let _ = measure_non_mtls_handshakes(1);
    let mut ca = TestCa::new("RabbitControl Bench CA");
    let _ = measure_mtls_handshakes(&mut ca, 1);

    // 1) non-mtls handshake & steady-state
    let non_mtls_hs_ms = measure_non_mtls_handshakes(HANDSHAKE_ROUNDS);
    let (mut non_client, mut non_host) = common::pair();
    let (non_dgram_us, non_ctrl_us) =
        measure_steady_rtt(&mut non_client, &mut non_host, STEADY_FRAMES);

    // 2) mtls handshake & steady-state
    let host_ident = ca.issue_standard("usr-bench", "host00001");
    let ctrl_ident = ca.issue_standard("usr-bench", "ctrl00002");
    let ca_der = ca.cert_der.clone();

    let mtls_hs_ms = measure_mtls_handshakes(&mut ca, HANDSHAKE_ROUNDS);

    let mut host_mtls = MtlsConfig::new(host_ident, vec![ca_der.clone()]);
    host_mtls.expected_peer_user_id = Some("usr-bench".into());
    host_mtls.expected_peer_device_id = Some("ctrl00002".into());
    let mut ctrl_mtls = MtlsConfig::new(ctrl_ident, vec![ca_der]);
    ctrl_mtls.expected_peer_user_id = Some("usr-bench".into());
    ctrl_mtls.expected_peer_device_id = Some("host00001".into());

    let host_ep = QuicEndpoint::new_mtls(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &host_mtls,
    )
    .expect("host mtls ep");
    let host_addr = host_ep.local_addr().expect("host addr");
    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let client_addr = client_sock.local_addr().expect("addr");
    let client_ep = QuicEndpoint::from_socket_mtls(
        client_sock,
        NetConfig::new(Role::Controller, client_addr),
        None,
    )
    .expect("client mtls ep");

    let accepter = thread::spawn(move || host_ep.accept(TIMEOUT).expect("accept"));
    let mut mtls_client = client_ep
        .connect_mtls(
            host_addr,
            &ctrl_mtls,
            "device-host00001.rc.internal",
            TIMEOUT,
        )
        .expect("connect_mtls");
    let mut mtls_host = accepter.join().expect("join");

    let (mtls_dgram_us, mtls_ctrl_us) =
        measure_steady_rtt(&mut mtls_client, &mut mtls_host, STEADY_FRAMES);

    let (non_hs_avg, non_hs_p50) = stats_ms(&non_mtls_hs_ms);
    let (mtls_hs_avg, mtls_hs_p50) = stats_ms(&mtls_hs_ms);
    let (non_dg_p50, non_dg_p99) = percentiles_us(&non_dgram_us);
    let (mtls_dg_p50, mtls_dg_p99) = percentiles_us(&mtls_dgram_us);
    let (non_ctl_p50, non_ctl_p99) = percentiles_us(&non_ctrl_us);
    let (mtls_ctl_p50, mtls_ctl_p99) = percentiles_us(&mtls_ctrl_us);

    println!(
        "[bench] loopback handshake ({HANDSHAKE_ROUNDS} rounds): \
         non-mtls avg={non_hs_avg:.2}ms p50={non_hs_p50:.2}ms | \
         mtls avg={mtls_hs_avg:.2}ms p50={mtls_hs_p50:.2}ms | \
         delta avg={:+.2}ms",
        mtls_hs_avg - non_hs_avg
    );
    println!(
        "[bench] steady-state datagram RTT ({STEADY_FRAMES} frames): \
         non-mtls p50={non_dg_p50}us p99={non_dg_p99}us | \
         mtls p50={mtls_dg_p50}us p99={mtls_dg_p99}us | \
         delta p50={:+.3}ms",
        (mtls_dg_p50 as f64 - non_dg_p50 as f64) / 1000.0
    );
    println!(
        "[bench] steady-state control RTT ({STEADY_FRAMES} msgs): \
         non-mtls p50={non_ctl_p50}us p99={non_ctl_p99}us | \
         mtls p50={mtls_ctl_p50}us p99={mtls_ctl_p99}us | \
         delta p50={:+.3}ms",
        (mtls_ctl_p50 as f64 - non_ctl_p50 as f64) / 1000.0
    );

    assert_eq!(non_dgram_us.len(), STEADY_FRAMES);
    assert_eq!(mtls_dgram_us.len(), STEADY_FRAMES);
    assert_eq!(non_ctrl_us.len(), STEADY_FRAMES);
    assert_eq!(mtls_ctrl_us.len(), STEADY_FRAMES);
}

fn measure_non_mtls_handshakes(rounds: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let ident = common::identity();
        let certs = common::server_certs();
        let host_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("host ep");
        let host_addr = host_ep.local_addr().expect("host addr");
        let client_ep = QuicEndpoint::new(
            &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
            &ident,
        )
        .expect("client ep");

        let accepter = thread::spawn(move || host_ep.accept(TIMEOUT).expect("accept"));
        let t0 = Instant::now();
        let _c = client_ep
            .connect(host_addr, &certs, "rabbitcontrol", TIMEOUT)
            .expect("connect");
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let _h = accepter.join().expect("join");
        samples.push(elapsed_ms);
    }
    samples
}

fn measure_mtls_handshakes(ca: &mut TestCa, rounds: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(rounds);
    for i in 0..rounds {
        let host_ident = ca.issue_standard("usr-bench", &format!("host{i:05}"));
        let ctrl_ident = ca.issue_standard("usr-bench", &format!("ctrl{i:05}"));
        let host_mtls = MtlsConfig::new(host_ident, vec![ca.cert_der.clone()]);
        let ctrl_mtls = MtlsConfig::new(ctrl_ident, vec![ca.cert_der.clone()]);
        let sni = format!("device-host{i:05}.rc.internal");

        let host_ep = QuicEndpoint::new_mtls(
            &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
            &host_mtls,
        )
        .expect("host mtls");
        let host_addr = host_ep.local_addr().expect("host addr");
        let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let client_addr = client_sock.local_addr().expect("addr");
        let client_ep = QuicEndpoint::from_socket_mtls(
            client_sock,
            NetConfig::new(Role::Controller, client_addr),
            None,
        )
        .expect("client mtls");

        let accepter = thread::spawn(move || host_ep.accept(TIMEOUT).expect("accept"));
        let t0 = Instant::now();
        let _c = client_ep
            .connect_mtls(host_addr, &ctrl_mtls, &sni, TIMEOUT)
            .expect("connect_mtls");
        let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let _h = accepter.join().expect("join");
        samples.push(elapsed_ms);
    }
    samples
}

fn measure_steady_rtt(
    client: &mut rc_net::quic::QuicTransport,
    host: &mut rc_net::quic::QuicTransport,
    frames: usize,
) -> (Vec<u64>, Vec<u64>) {
    // 排掉 Connected
    let _ = client.poll(Duration::from_millis(20));
    let _ = host.poll(Duration::from_millis(20));

    let mut dgram_rtts = Vec::with_capacity(frames);
    let mut payload = vec![0u8; 256];
    for i in 0..frames {
        payload[..8].copy_from_slice(&(i as u64).to_le_bytes());
        let t0 = Instant::now();
        host.send_datagram(&payload).expect("host dgram");
        wait_for_datagram(client, i as u64);
        client.send_datagram(&payload).expect("client dgram");
        wait_for_datagram(host, i as u64);
        dgram_rtts.push(t0.elapsed().as_micros() as u64);
    }

    let mut ctrl_rtts = Vec::with_capacity(frames);
    for i in 0..frames {
        let msg = (i as u64).to_le_bytes();
        let t0 = Instant::now();
        client.send_reliable(&msg).expect("client reliable");
        wait_for_reliable(host, i as u64);
        host.send_reliable(&msg).expect("host reliable");
        wait_for_reliable(client, i as u64);
        ctrl_rtts.push(t0.elapsed().as_micros() as u64);
    }

    (dgram_rtts, ctrl_rtts)
}

fn wait_for_datagram(t: &mut dyn Transport, expected_seq: u64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        for ev in t.poll(Duration::from_millis(20)).expect("poll") {
            if let NetEvent::Datagram(b) = ev {
                if b.len() >= 8 && u64::from_le_bytes(b[..8].try_into().unwrap()) == expected_seq {
                    return;
                }
            }
        }
    }
    panic!("timeout waiting for datagram seq={expected_seq}");
}

fn wait_for_reliable(t: &mut dyn Transport, expected_seq: u64) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        for ev in t.poll(Duration::from_millis(20)).expect("poll") {
            if let NetEvent::Reliable(b) = ev {
                if b.len() >= 8 && u64::from_le_bytes(b[..8].try_into().unwrap()) == expected_seq {
                    return;
                }
            }
        }
    }
    panic!("timeout waiting for reliable seq={expected_seq}");
}

fn stats_ms(samples: &[f64]) -> (f64, f64) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let p50 = sorted[sorted.len() / 2];
    (avg, p50)
}

fn percentiles_us(samples: &[u64]) -> (u64, u64) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[sorted.len() / 2];
    let p99_idx = ((sorted.len() * 99) / 100).min(sorted.len() - 1);
    (p50, sorted[p99_idx])
}
