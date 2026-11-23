//! 双向 mTLS 证书验证、SAN URI 身份解析与 SHA-256 指纹工具。
//!
//! # 设备证书约定
//! 服务端 PKI 签发的设备证书遵循：
//! - SAN URI: `urn:rabbitcontrol:user:{user_id}:device:{device_id}`
//! - SAN DNS: `device-{device_id}.rc.internal`
//! - EKU: 同时支持 `ClientAuth` 与 `ServerAuth`
//!
//! 本模块使用纯 Rust DER 解析器直接读取 X.509 v3 扩展 `2.5.29.17`（SubjectAltName）
//! 中的 `GeneralName` `[6]` (`IA5String` URI, tag `0x86`)，零新增第三方解析依赖、零 `unsafe`。

use std::fmt::Write as _;
use std::sync::Arc;

use rc_types::{Error, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::WebPkiClientVerifier;
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, OtherError, RootCertStore,
    SignatureScheme,
};

use crate::quic::Identity;

/// `id-ce-subjectAltName` OID (`2.5.29.17`) 的 DER 编码内容。
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];

/// SAN URI 固定前缀。
const RC_SAN_URI_PREFIX: &str = "urn:rabbitcontrol:user:";

/// 对端通过 mTLS 证书验证后提取的设备与用户身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMtlsIdentity {
    /// 对端用户 ID（从证书 SAN URI 提取）
    pub user_id: String,
    /// 对端设备 ID（从证书 SAN URI 提取）
    pub device_id: String,
    /// 对端叶子证书 DER 的 SHA-256 指纹（64 位小写十六进制）
    pub cert_fingerprint_sha256: String,
}

/// 双向 mTLS 认证配置。
///
/// 用于 Host（[`crate::quic::QuicEndpoint::new_mtls`] /
/// [`crate::quic::QuicEndpoint::from_socket_mtls`]）与 Controller
/// （[`crate::quic::QuicEndpoint::connect_mtls`]）构建强制校验证书链、
/// EKU 与 SAN URI 设备/用户身份的 QUIC TLS 配置。
#[derive(Debug)]
pub struct MtlsConfig {
    /// 本端证书链与私钥
    pub identity: Identity,
    /// 受信任的 Root CA 证书列表（DER）
    pub ca_certs: Vec<CertificateDer<'static>>,
    /// 可选的预期对端设备 ID；若设置，则对端证书 SAN URI 中的 `device_id` 必须精确匹配
    pub expected_peer_device_id: Option<String>,
    /// 可选的预期对端用户 ID；若设置，则对端证书 SAN URI 中的 `user_id` 必须精确匹配
    pub expected_peer_user_id: Option<String>,
}

impl MtlsConfig {
    /// 使用本端身份与 Root CA 集合构造不限定特定对端 ID 的 mTLS 配置。
    ///
    /// 即使不设置 `expected_peer_device_id` / `expected_peer_user_id`，
    /// 对端证书仍必须由 `ca_certs` 签发且携带合法的
    /// `urn:rabbitcontrol:user:{user_id}:device:{device_id}` SAN URI。
    pub fn new(identity: Identity, ca_certs: Vec<CertificateDer<'static>>) -> Self {
        Self {
            identity,
            ca_certs,
            expected_peer_device_id: None,
            expected_peer_user_id: None,
        }
    }
}

/// 计算 DER 编码证书的 SHA-256 指纹（64 位小写十六进制字符串）。
pub fn cert_sha256_hex(cert_der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let bytes = digest.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// 从 X.509 v3 DER 证书中解析 RabbitControl SAN URI，返回 `(user_id, device_id)`。
///
/// 匹配格式为 `urn:rabbitcontrol:user:{user_id}:device:{device_id}` 的
/// `GeneralName` `[6]` (`uniformResourceIdentifier`, tag `0x86`)。
/// 若证书 DER 结构非法、缺少 SAN 扩展、缺少合法 URI、或存在多个冲突的
/// RabbitControl SAN URI，均返回 `None`。
pub fn parse_rc_san_uri(cert_der: &[u8]) -> Option<(String, String)> {
    let (cert_tlv, rem) = read_tlv(cert_der)?;
    if !rem.is_empty() || cert_tlv.tag != 0x30 {
        return None;
    }

    // Certificate.tbsCertificate
    let (tbs_tlv, _) = read_tlv(cert_tlv.value)?;
    if tbs_tlv.tag != 0x30 {
        return None;
    }

    let mut cur = tbs_tlv.value;
    // version [0] EXPLICIT OPTIONAL
    if peek_tag(cur) == Some(0xA0) {
        let (_, rest) = read_tlv(cur)?;
        cur = rest;
    }

    // serialNumber (INTEGER), signature (SEQUENCE), issuer (SEQUENCE),
    // validity (SEQUENCE), subject (SEQUENCE), subjectPublicKeyInfo (SEQUENCE)
    let expected_tags = [0x02u8, 0x30, 0x30, 0x30, 0x30, 0x30];
    for expected in expected_tags {
        let (tlv, rest) = read_tlv(cur)?;
        if tlv.tag != expected {
            return None;
        }
        cur = rest;
    }

    // issuerUniqueID [1] OPTIONAL (primitive 0x81 or constructed 0xA1)
    if matches!(peek_tag(cur), Some(0x81 | 0xA1)) {
        let (_, rest) = read_tlv(cur)?;
        cur = rest;
    }
    // subjectUniqueID [2] OPTIONAL (primitive 0x82 or constructed 0xA2)
    if matches!(peek_tag(cur), Some(0x82 | 0xA2)) {
        let (_, rest) = read_tlv(cur)?;
        cur = rest;
    }

    // extensions [3] EXPLICIT Extensions OPTIONAL
    if peek_tag(cur) != Some(0xA3) {
        return None;
    }
    let (exts_explicit, _) = read_tlv(cur)?;
    let (exts_seq, rem_exts) = read_tlv(exts_explicit.value)?;
    if !rem_exts.is_empty() || exts_seq.tag != 0x30 {
        return None;
    }

    let mut ext_cur = exts_seq.value;
    while !ext_cur.is_empty() {
        let (ext_tlv, next_ext) = read_tlv(ext_cur)?;
        ext_cur = next_ext;
        if ext_tlv.tag != 0x30 {
            return None;
        }

        let (oid_tlv, mut field_cur) = read_tlv(ext_tlv.value)?;
        if oid_tlv.tag != 0x06 {
            return None;
        }
        if oid_tlv.value != OID_SUBJECT_ALT_NAME {
            continue;
        }

        // critical BOOLEAN OPTIONAL
        if peek_tag(field_cur) == Some(0x01) {
            let (_, rest) = read_tlv(field_cur)?;
            field_cur = rest;
        }
        let (val_tlv, rem_fields) = read_tlv(field_cur)?;
        if !rem_fields.is_empty() || val_tlv.tag != 0x04 {
            return None;
        }

        return parse_general_names(val_tlv.value);
    }

    None
}

fn parse_general_names(san_der: &[u8]) -> Option<(String, String)> {
    let (seq, rem) = read_tlv(san_der)?;
    if !rem.is_empty() || seq.tag != 0x30 {
        return None;
    }

    let mut found: Option<(String, String)> = None;
    let mut cur = seq.value;
    while !cur.is_empty() {
        let (gn, rest) = read_tlv(cur)?;
        cur = rest;
        // [6] IMPLICIT IA5String = uniformResourceIdentifier
        if gn.tag == 0x86 && !gn.high_tag {
            let uri = std::str::from_utf8(gn.value).ok()?;
            if uri.starts_with(RC_SAN_URI_PREFIX) {
                let parsed = parse_rc_uri_str(uri)?;
                if found.is_some() {
                    // 拒绝同证书内出现多个 RabbitControl SAN URI，消除身份二义性。
                    return None;
                }
                found = Some(parsed);
            }
        }
    }
    found
}

fn parse_rc_uri_str(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(RC_SAN_URI_PREFIX)?;
    let (user_id, device_id) = rest.split_once(":device:")?;
    if user_id.is_empty() || device_id.is_empty() {
        return None;
    }
    if user_id.contains(':')
        || device_id.contains(':')
        || user_id.bytes().any(|b| b.is_ascii_control() || b == b' ')
        || device_id.bytes().any(|b| b.is_ascii_control() || b == b' ')
    {
        return None;
    }
    Some((user_id.to_string(), device_id.to_string()))
}

struct DerTlv<'a> {
    tag: u8,
    high_tag: bool,
    value: &'a [u8],
}

fn peek_tag(input: &[u8]) -> Option<u8> {
    let first = *input.first()?;
    if (first & 0x1f) == 0x1f {
        None
    } else {
        Some(first)
    }
}

fn read_tlv(input: &[u8]) -> Option<(DerTlv<'_>, &[u8])> {
    let first_tag = *input.first()?;
    let mut idx = 1usize;
    let high_tag = (first_tag & 0x1f) == 0x1f;
    if high_tag {
        // 跳过多字节高编号 tag，保证含非标准扩展的证书也能安全跳过无关 TLV。
        loop {
            let b = *input.get(idx)?;
            idx += 1;
            if (b & 0x80) == 0 {
                break;
            }
            if idx > 8 {
                return None;
            }
        }
    }

    let len_byte = *input.get(idx)?;
    idx += 1;
    let len: usize = if len_byte < 0x80 {
        len_byte as usize
    } else if len_byte == 0x80 {
        // DER 禁止不定长编码
        return None;
    } else {
        let count = (len_byte & 0x7f) as usize;
        if count == 0 || count > 4 {
            return None;
        }
        let len_bytes = input.get(idx..idx + count)?;
        // DER 要求最短长度编码：不能有前导 0x00，且值必须 >= 128
        if len_bytes[0] == 0 {
            return None;
        }
        idx += count;
        let mut val = 0usize;
        for &b in len_bytes {
            val = (val << 8) | (b as usize);
        }
        if val < 128 {
            return None;
        }
        val
    };

    let end = idx.checked_add(len)?;
    let value = input.get(idx..end)?;
    let rest = input.get(end..)?;
    Some((
        DerTlv {
            tag: first_tag,
            high_tag,
            value,
        },
        rest,
    ))
}

// ─────────────────────────────────────────────────────────────────
// 自定义 mTLS 证书验证器（委托 WebPki*Verifier + SAN URI 强校验）
// ─────────────────────────────────────────────────────────────────

fn cert_rejected(reason: impl Into<String>) -> rustls::Error {
    rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(
        std::io::Error::new(std::io::ErrorKind::InvalidData, reason.into()),
    ))))
}

fn validate_peer_san_uri(
    end_entity: &[u8],
    expected_user_id: Option<&str>,
    expected_device_id: Option<&str>,
) -> std::result::Result<(), rustls::Error> {
    let Some((user_id, device_id)) = parse_rc_san_uri(end_entity) else {
        return Err(cert_rejected(
            "peer certificate missing valid urn:rabbitcontrol:user:<uid>:device:<did> SAN URI",
        ));
    };
    if let Some(exp_uid) = expected_user_id {
        if !user_id.eq_ignore_ascii_case(exp_uid) {
            return Err(cert_rejected(format!(
                "peer certificate user_id mismatch: expected {exp_uid}, got {user_id}"
            )));
        }
    }
    if let Some(exp_did) = expected_device_id {
        if device_id != exp_did {
            return Err(cert_rejected(format!(
                "peer certificate device_id mismatch: expected {exp_did}, got {device_id}"
            )));
        }
    }
    Ok(())
}

fn build_root_store(ca_certs: &[CertificateDer<'static>]) -> Result<RootCertStore> {
    if ca_certs.is_empty() {
        return Err(Error::InvalidConfig("mtls ca_certs cannot be empty".into()));
    }
    let mut roots = RootCertStore::empty();
    for c in ca_certs {
        roots
            .add(c.clone())
            .map_err(|e| Error::InvalidConfig(format!("invalid mtls ca cert: {e}")))?;
    }
    Ok(roots)
}

/// 服务端侧自定义客户端证书验证器：
/// 先委托 [`WebPkiClientVerifier`] 校验证书链、有效期与 ClientAuth EKU，
/// 再强制校验 `urn:rabbitcontrol:user:{user_id}:device:{device_id}` SAN URI
/// 与可选的 `expected_peer_user_id` / `expected_peer_device_id`。
#[derive(Debug)]
struct RcClientCertVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    expected_user_id: Option<String>,
    expected_device_id: Option<String>,
}

impl ClientCertVerifier for RcClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        validate_peer_san_uri(
            end_entity.as_ref(),
            self.expected_user_id.as_deref(),
            self.expected_device_id.as_deref(),
        )?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
}

/// 客户端侧自定义服务端证书验证器：
/// 先委托 [`WebPkiServerVerifier`] 校验证书链、有效期、SAN DNS 与 ServerAuth EKU，
/// 再强制校验 `urn:rabbitcontrol:user:{user_id}:device:{device_id}` SAN URI
/// 与可选的 `expected_peer_user_id` / `expected_peer_device_id`。
#[derive(Debug)]
struct RcServerCertVerifier {
    inner: Arc<WebPkiServerVerifier>,
    expected_user_id: Option<String>,
    expected_device_id: Option<String>,
}

impl ServerCertVerifier for RcServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        validate_peer_san_uri(
            end_entity.as_ref(),
            self.expected_user_id.as_deref(),
            self.expected_device_id.as_deref(),
        )?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }

    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        self.inner.root_hint_subjects()
    }
}

pub(crate) fn build_mtls_server_tls(
    mtls: &MtlsConfig,
    alpn: &[u8],
) -> Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = build_root_store(&mtls.ca_certs)?;
    let inner = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
        .build()
        .map_err(|e| Error::InvalidConfig(format!("mtls client verifier: {e}")))?;

    let verifier = Arc::new(RcClientCertVerifier {
        inner,
        expected_user_id: mtls.expected_peer_user_id.clone(),
        expected_device_id: mtls.expected_peer_device_id.clone(),
    });

    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("tls versions: {e}")))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            mtls.identity.cert_chain().to_vec(),
            mtls.identity.clone_key(),
        )
        .map_err(|e| Error::InvalidConfig(format!("mtls server cert: {e}")))?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    Ok(tls)
}

pub(crate) fn build_mtls_client_tls(
    mtls: &MtlsConfig,
    alpn: &[u8],
) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = build_root_store(&mtls.ca_certs)?;
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
        .build()
        .map_err(|e| Error::InvalidConfig(format!("mtls server verifier: {e}")))?;

    let verifier = Arc::new(RcServerCertVerifier {
        inner,
        expected_user_id: mtls.expected_peer_user_id.clone(),
        expected_device_id: mtls.expected_peer_device_id.clone(),
    });

    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| Error::InvalidConfig(format!("tls versions: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(
            mtls.identity.cert_chain().to_vec(),
            mtls.identity.clone_key(),
        )
        .map_err(|e| Error::InvalidConfig(format!("mtls client cert: {e}")))?;
    tls.alpn_protocols = vec![alpn.to_vec()];
    Ok(tls)
}

pub(crate) fn resolve_mtls_server_name(mtls: &MtlsConfig, server_name: &str) -> Result<String> {
    let trimmed = server_name.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }
    if let Some(did) = mtls.expected_peer_device_id.as_deref() {
        return Ok(format!("device-{did}.rc.internal"));
    }
    Err(Error::InvalidConfig(
        "server_name is empty and expected_peer_device_id is unset".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(tag);
        if body.len() < 128 {
            out.push(body.len() as u8);
        } else if body.len() <= 0xff {
            out.push(0x81);
            out.push(body.len() as u8);
        } else {
            out.push(0x82);
            out.push((body.len() >> 8) as u8);
            out.push((body.len() & 0xff) as u8);
        }
        out.extend_from_slice(body);
        out
    }

    fn synthetic_cert_with_san(san_entries: &[Vec<u8>]) -> Vec<u8> {
        let gn_seq = der_tlv(0x30, &san_entries.concat());
        let san_ext = [
            der_tlv(0x06, OID_SUBJECT_ALT_NAME),
            der_tlv(0x01, &[0x00]), // critical = FALSE
            der_tlv(0x04, &gn_seq),
        ]
        .concat();
        let exts = der_tlv(0x30, &der_tlv(0x30, &san_ext));
        let tbs = [
            der_tlv(0xA0, &der_tlv(0x02, &[0x02])), // v3
            der_tlv(0x02, &[0x01]),                 // serial
            der_tlv(0x30, &[]),                     // sig alg
            der_tlv(0x30, &[]),                     // issuer
            der_tlv(0x30, &[]),                     // validity
            der_tlv(0x30, &[]),                     // subject
            der_tlv(0x30, &[]),                     // spki
            der_tlv(0xA3, &exts),                   // extensions [3]
        ]
        .concat();
        let cert = [
            der_tlv(0x30, &tbs),
            der_tlv(0x30, &[]),
            der_tlv(0x03, &[0x00]),
        ]
        .concat();
        der_tlv(0x30, &cert)
    }

    #[test]
    fn parse_rc_san_uri_extracts_user_and_device() {
        let san_dns = der_tlv(0x82, b"device-dev000001.rc.internal");
        let san_uri = der_tlv(0x86, b"urn:rabbitcontrol:user:usr-42:device:dev000001");
        let cert = synthetic_cert_with_san(&[san_dns, san_uri]);

        assert_eq!(
            parse_rc_san_uri(&cert),
            Some(("usr-42".to_string(), "dev000001".to_string()))
        );
    }

    #[test]
    fn parse_rc_san_uri_rejects_missing_or_malformed_or_duplicate() {
        let only_dns = synthetic_cert_with_san(&[der_tlv(0x82, b"device-dev1.rc.internal")]);
        assert_eq!(parse_rc_san_uri(&only_dns), None);

        let empty_uid =
            synthetic_cert_with_san(&[der_tlv(0x86, b"urn:rabbitcontrol:user::device:dev1")]);
        assert_eq!(parse_rc_san_uri(&empty_uid), None);

        let extra_colon =
            synthetic_cert_with_san(&[der_tlv(0x86, b"urn:rabbitcontrol:user:u1:device:d1:extra")]);
        assert_eq!(parse_rc_san_uri(&extra_colon), None);

        let duplicate = synthetic_cert_with_san(&[
            der_tlv(0x86, b"urn:rabbitcontrol:user:u1:device:d1"),
            der_tlv(0x86, b"urn:rabbitcontrol:user:u2:device:d2"),
        ]);
        assert_eq!(parse_rc_san_uri(&duplicate), None);

        // 非 RabbitControl URI 不干扰合法 URI 提取
        let with_other_uri = synthetic_cert_with_san(&[
            der_tlv(0x86, b"https://example.com/device"),
            der_tlv(0x86, b"urn:rabbitcontrol:user:u9:device:d9"),
        ]);
        assert_eq!(
            parse_rc_san_uri(&with_other_uri),
            Some(("u9".to_string(), "d9".to_string()))
        );

        // 截断 DER 必须安全返回 None
        assert_eq!(parse_rc_san_uri(&with_other_uri[..10]), None);
    }

    #[test]
    fn cert_sha256_hex_is_64_lowercase_hex_chars() {
        let hex = cert_sha256_hex(b"abc");
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // FIPS 180-4 标准向量 SHA-256("abc")
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            cert_sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
