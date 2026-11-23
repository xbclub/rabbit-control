use std::collections::BTreeSet;
use std::io::BufReader;
use std::net::IpAddr;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyIdMethod, KeyPair, KeyUsagePurpose, PublicKeyData, SanType,
    SerialNumber, SignatureAlgorithm, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PKCS_ED25519,
};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::{
    OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_SIG_ED25519,
};
use x509_parser::prelude::{FromDer, X509Certificate, X509CertificationRequest};

use crate::store::{
    is_valid_device_id, normalize_device_id, DeviceCertRecord, PkiCaRecord, PkiStore, StoreError,
};

pub const ROOT_CA_VALIDITY_DAYS: i64 = 3650;
pub const SERVER_CERT_DEFAULT_VALIDITY_DAYS: i64 = 365;
pub const DEVICE_CERT_VALIDITY_DAYS: i64 = 7;

pub const DEVICE_SAN_URI_PREFIX: &str = "urn:rabbitcontrol:user:";
pub const DEVICE_SAN_URI_MID: &str = ":device:";
pub const DEVICE_SAN_DNS_SUFFIX: &str = ".rc.internal";

#[derive(Debug, Error)]
pub enum PkiError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid CSR: {0}")]
    InvalidCsr(String),
    #[error("invalid certificate: {0}")]
    InvalidCertificate(String),
    #[error("invalid peer identity in certificate: {0}")]
    InvalidIdentity(String),
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("TLS configuration error: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCertIdentity {
    pub user_id: Uuid,
    pub device_id: String,
    pub serial_hex: String,
    pub sha256_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct IssuedServerCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_der: Vec<u8>,
    pub key_pkcs8_der: Vec<u8>,
    pub ca_cert_pem: String,
    pub fingerprint_sha256: String,
}

#[derive(Debug, Clone)]
pub struct IssuedDeviceCert {
    pub serial_hex: String,
    pub user_id: Uuid,
    pub device_id: String,
    pub fingerprint_sha256: String,
    pub cert_pem: String,
    pub cert_der: Vec<u8>,
    pub ca_cert_pem: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Verified PKCS#10 CSR public key ready to be signed by `rcgen`.
#[derive(Debug, Clone)]
pub struct VerifiedCsr {
    raw_public_key: Vec<u8>,
    alg: &'static SignatureAlgorithm,
    pub spki_der: Vec<u8>,
}

impl PublicKeyData for VerifiedCsr {
    fn der_bytes(&self) -> &[u8] {
        &self.raw_public_key
    }

    fn algorithm(&self) -> &SignatureAlgorithm {
        self.alg
    }
}

/// Compute lowercase 64-character SHA-256 hex fingerprint of DER bytes.
pub fn sha256_hex(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

fn generate_positive_128bit_serial() -> Result<([u8; 16], String), PkiError> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| PkiError::InvalidInput("SystemRandom failed".into()))?;
    // Ensure positive ASN.1 INTEGER with no redundant leading 0x00 byte so
    // raw_serial() and SerialNumber::to_bytes() match 1:1 in all parsers.
    bytes[0] = (bytes[0] & 0x7f) | 0x01;
    let serial_hex = hex::encode(bytes);
    Ok((bytes, serial_hex))
}

fn decode_pem_or_der_csr(csr_pem_or_der: &[u8]) -> Result<Vec<u8>, PkiError> {
    let trimmed = csr_pem_or_der.trim_ascii();
    if trimmed.starts_with(b"-----BEGIN ") {
        let mut reader = BufReader::new(trimmed);
        let item = rustls_pemfile::read_one(&mut reader)
            .map_err(|e| PkiError::InvalidCsr(format!("failed to parse PEM CSR: {e}")))?;
        match item {
            Some(rustls_pemfile::Item::Csr(der)) => Ok(der.to_vec()),
            Some(_) => Err(PkiError::InvalidCsr(
                "PEM block is not a CERTIFICATE REQUEST".into(),
            )),
            None => Err(PkiError::InvalidCsr("empty PEM CSR input".into())),
        }
    } else if !csr_pem_or_der.is_empty() {
        Ok(csr_pem_or_der.to_vec())
    } else {
        Err(PkiError::InvalidCsr("empty CSR input".into()))
    }
}

/// Parse a PKCS#10 CSR (PEM or DER), verify its cryptographic self-signature,
/// and validate that its public key uses a supported strong algorithm (ECDSA P-256/P-384 or Ed25519).
pub fn parse_and_verify_csr(csr_pem_or_der: &[u8]) -> Result<VerifiedCsr, PkiError> {
    let der = decode_pem_or_der_csr(csr_pem_or_der)?;
    let (rem, csr) = X509CertificationRequest::from_der(&der)
        .map_err(|e| PkiError::InvalidCsr(format!("invalid DER CSR: {e}")))?;
    if !rem.is_empty() {
        return Err(PkiError::InvalidCsr("trailing bytes in CSR DER".into()));
    }

    csr.verify_signature()
        .map_err(|e| PkiError::InvalidCsr(format!("CSR signature verification failed: {e}")))?;

    let spki = &csr.certification_request_info.subject_pki;
    let raw_key = spki.subject_public_key.data.as_ref();

    let alg: &'static SignatureAlgorithm = if spki.algorithm.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY
    {
        let curve_oid = spki
            .algorithm
            .parameters
            .as_ref()
            .and_then(|p| p.as_oid().ok())
            .ok_or_else(|| PkiError::InvalidCsr("EC CSR missing named curve OID".into()))?;
        if curve_oid == OID_EC_P256 {
            if raw_key.len() != 65 || raw_key[0] != 0x04 {
                return Err(PkiError::InvalidCsr(
                    "invalid uncompressed P-256 public key in CSR".into(),
                ));
            }
            &PKCS_ECDSA_P256_SHA256
        } else if curve_oid == OID_NIST_EC_P384 {
            if raw_key.len() != 97 || raw_key[0] != 0x04 {
                return Err(PkiError::InvalidCsr(
                    "invalid uncompressed P-384 public key in CSR".into(),
                ));
            }
            &PKCS_ECDSA_P384_SHA384
        } else {
            return Err(PkiError::InvalidCsr(format!(
                "unsupported EC curve in CSR: {curve_oid}"
            )));
        }
    } else if spki.algorithm.algorithm == OID_SIG_ED25519 {
        if raw_key.len() != 32 {
            return Err(PkiError::InvalidCsr(
                "invalid Ed25519 public key length in CSR".into(),
            ));
        }
        &PKCS_ED25519
    } else {
        return Err(PkiError::InvalidCsr(format!(
            "unsupported CSR public key algorithm: {}",
            spki.algorithm.algorithm
        )));
    };

    Ok(VerifiedCsr {
        raw_public_key: raw_key.to_vec(),
        alg,
        spki_der: spki.raw.to_vec(),
    })
}

/// Extract `(user_id, device_id, serial_hex, sha256_fingerprint)` from a peer X.509 certificate DER.
///
/// Looks up the RabbitControl SAN URI (`urn:rabbitcontrol:user:{uuid}:device:{9-digit-did}`).
pub fn parse_peer_cert_identity(der: &[u8]) -> Result<PeerCertIdentity, PkiError> {
    if der.is_empty() {
        return Err(PkiError::InvalidCertificate("empty certificate DER".into()));
    }

    let sha256_fingerprint = sha256_hex(der);
    let (rem, cert) = X509Certificate::from_der(der)
        .map_err(|e| PkiError::InvalidCertificate(format!("invalid X.509 DER: {e}")))?;
    if !rem.is_empty() {
        return Err(PkiError::InvalidCertificate(
            "trailing bytes in certificate DER".into(),
        ));
    }

    let raw_serial = cert.raw_serial();
    if raw_serial.is_empty() {
        return Err(PkiError::InvalidCertificate(
            "certificate has empty serial number".into(),
        ));
    }
    let serial_hex = hex::encode(raw_serial);

    let san = cert
        .subject_alternative_name()
        .map_err(|e| PkiError::InvalidCertificate(format!("malformed SAN extension: {e}")))?
        .ok_or_else(|| PkiError::InvalidIdentity("certificate has no SAN extension".into()))?;

    let mut matched: Option<(Uuid, String)> = None;
    for name in &san.value.general_names {
        if let GeneralName::URI(uri) = name {
            if let Some(rest) = uri.strip_prefix(DEVICE_SAN_URI_PREFIX) {
                let Some((uid_str, did_raw)) = rest.split_once(DEVICE_SAN_URI_MID) else {
                    return Err(PkiError::InvalidIdentity(format!(
                        "malformed RabbitControl SAN URI: {uri}"
                    )));
                };
                let user_id = Uuid::parse_str(uid_str).map_err(|_| {
                    PkiError::InvalidIdentity(format!("invalid user UUID in SAN URI: {uid_str}"))
                })?;
                let device_id = normalize_device_id(did_raw);
                if !is_valid_device_id(&device_id) {
                    return Err(PkiError::InvalidIdentity(format!(
                        "invalid device_id in SAN URI: {did_raw}"
                    )));
                }
                if let Some((prev_uid, prev_did)) = &matched {
                    if *prev_uid != user_id || *prev_did != device_id {
                        return Err(PkiError::InvalidIdentity(
                            "conflicting RabbitControl SAN URIs in certificate".into(),
                        ));
                    }
                } else {
                    matched = Some((user_id, device_id));
                }
            }
        }
    }

    let (user_id, device_id) = matched.ok_or_else(|| {
        PkiError::InvalidIdentity("missing urn:rabbitcontrol:user:...:device:... SAN URI".into())
    })?;

    Ok(PeerCertIdentity {
        user_id,
        device_id,
        serial_hex,
        sha256_fingerprint,
    })
}

fn build_ca_params() -> CertificateParams {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "RabbitControl");
    dn.push(DnType::CommonName, "RabbitControl Root CA");

    let now = OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(ROOT_CA_VALIDITY_DAYS);
    params.key_identifier_method = KeyIdMethod::Sha256;
    params
}

fn reconstruct_issuer_cert(ca_der: &[u8], ca_key: &KeyPair) -> Result<Certificate, PkiError> {
    let (_, x509) = X509Certificate::from_der(ca_der)
        .map_err(|e| PkiError::InvalidCertificate(format!("invalid CA cert DER: {e}")))?;

    let mut dn = DistinguishedName::new();
    for attr in x509.subject().iter_attributes() {
        let oid: Vec<u64> = attr
            .attr_type()
            .iter()
            .ok_or_else(|| PkiError::InvalidCertificate("invalid DN OID in CA cert".into()))?
            .collect();
        let val = attr
            .as_str()
            .map_err(|e| PkiError::InvalidCertificate(format!("non-UTF8 CA DN value: {e}")))?;
        dn.push(DnType::from_oid(&oid), val);
    }

    let ski = x509.iter_extensions().find_map(|ext| {
        if let ParsedExtension::SubjectKeyIdentifier(id) = ext.parsed_extension() {
            Some(id.0.to_vec())
        } else {
            None
        }
    });

    let mut params = CertificateParams::default();
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.key_identifier_method = match ski {
        Some(bytes) => KeyIdMethod::PreSpecified(bytes),
        None => KeyIdMethod::Sha256,
    };

    Ok(params.self_signed(ca_key)?)
}

fn decode_single_cert_pem(pem_str: &str) -> Result<Vec<u8>, PkiError> {
    let mut reader = BufReader::new(pem_str.as_bytes());
    let mut certs = rustls_pemfile::certs(&mut reader);
    let first = certs
        .next()
        .transpose()
        .map_err(|e| PkiError::InvalidCertificate(format!("invalid CA cert PEM: {e}")))?
        .ok_or_else(|| PkiError::InvalidCertificate("no certificate found in CA PEM".into()))?;
    Ok(first.to_vec())
}

fn push_san_host(
    seen: &mut BTreeSet<String>,
    out: &mut Vec<SanType>,
    raw: &str,
) -> Result<(), PkiError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let canonical = unbracketed.to_ascii_lowercase();
    if !seen.insert(canonical) {
        return Ok(());
    }
    if let Ok(ip) = unbracketed.parse::<IpAddr>() {
        out.push(SanType::IpAddress(ip));
    } else {
        let dns = unbracketed.to_string().try_into().map_err(|e| {
            PkiError::InvalidInput(format!("invalid SAN DNS name '{trimmed}': {e}"))
        })?;
        out.push(SanType::DnsName(dns));
    }
    Ok(())
}

/// Server-side internal PKI Root CA (ECDSA P-256) persisted in `pki_ca`.
pub struct PkiAuthority {
    ca_cert_pem: String,
    ca_cert_der: Vec<u8>,
    ca_fingerprint_sha256: String,
    issuer_cert: Certificate,
    ca_key: KeyPair,
}

impl PkiAuthority {
    /// Load the Root CA from `pki_ca` or atomically generate and persist a 10-year ECDSA P-256 CA.
    pub async fn load_or_create(store: &dyn PkiStore) -> Result<Self, PkiError> {
        if let Some(existing) = store.get_pki_ca().await? {
            return Self::from_pem(&existing.cert_pem, &existing.key_pem);
        }

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let (serial_bytes, _) = generate_positive_128bit_serial()?;
        let mut params = build_ca_params();
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));
        let ca_cert = params.self_signed(&ca_key)?;
        let cert_pem = ca_cert.pem();
        let key_pem = ca_key.serialize_pem();
        let fingerprint_sha256 = sha256_hex(ca_cert.der());

        let stored = store
            .put_pki_ca_if_absent(PkiCaRecord {
                cert_pem,
                key_pem,
                fingerprint_sha256,
                created_at: Utc::now(),
            })
            .await?;

        Self::from_pem(&stored.cert_pem, &stored.key_pem)
    }

    /// Generate an ephemeral in-memory Root CA (useful for tests).
    pub fn generate_ephemeral() -> Result<Self, PkiError> {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let (serial_bytes, _) = generate_positive_128bit_serial()?;
        let mut params = build_ca_params();
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));
        let ca_cert = params.self_signed(&ca_key)?;
        let ca_cert_pem = ca_cert.pem();
        let ca_cert_der = ca_cert.der().to_vec();
        let ca_fingerprint_sha256 = sha256_hex(&ca_cert_der);
        Ok(Self {
            ca_cert_pem,
            ca_cert_der,
            ca_fingerprint_sha256,
            issuer_cert: ca_cert,
            ca_key,
        })
    }

    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, PkiError> {
        let ca_key = KeyPair::from_pem(key_pem)?;
        let ca_cert_der = decode_single_cert_pem(cert_pem)?;
        let ca_fingerprint_sha256 = sha256_hex(&ca_cert_der);
        let issuer_cert = reconstruct_issuer_cert(&ca_cert_der, &ca_key)?;
        Ok(Self {
            ca_cert_pem: cert_pem.to_string(),
            ca_cert_der,
            ca_fingerprint_sha256,
            issuer_cert,
            ca_key,
        })
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn ca_cert_der(&self) -> &[u8] {
        &self.ca_cert_der
    }

    pub fn ca_fingerprint_sha256(&self) -> &str {
        &self.ca_fingerprint_sha256
    }

    /// Build a `rustls::RootCertStore` trusting only this internal Root CA.
    pub fn root_cert_store(&self) -> Result<RootCertStore, PkiError> {
        let mut store = RootCertStore::empty();
        store.add(CertificateDer::from(self.ca_cert_der.clone()))?;
        Ok(store)
    }

    /// Issue a server TLS leaf certificate signed by the Root CA.
    ///
    /// Always includes SANs `localhost`, `127.0.0.1`, `::1`, plus optional `public_host` and `extra_sans`.
    pub fn issue_server_tls_cert(
        &self,
        public_host: Option<&str>,
        extra_sans: &[String],
        validity_days: i64,
    ) -> Result<IssuedServerCert, PkiError> {
        if validity_days <= 0 {
            return Err(PkiError::InvalidInput(
                "validity_days must be positive".into(),
            ));
        }

        let mut seen = BTreeSet::new();
        let mut sans = Vec::new();
        for default_san in ["localhost", "127.0.0.1", "::1"] {
            push_san_host(&mut seen, &mut sans, default_san)?;
        }
        if let Some(host) = public_host {
            push_san_host(&mut seen, &mut sans, host)?;
        }
        for san in extra_sans {
            push_san_host(&mut seen, &mut sans, san)?;
        }

        let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let (serial_bytes, _) = generate_positive_128bit_serial()?;
        let now = OffsetDateTime::now_utc();

        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "RabbitControl");
        dn.push(DnType::CommonName, "RabbitControl Server");

        let mut params = CertificateParams::default();
        params.distinguished_name = dn;
        params.subject_alt_names = sans;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = now + time::Duration::days(validity_days);

        let cert = params.signed_by(&server_key, &self.issuer_cert, &self.ca_key)?;
        let cert_der = cert.der().to_vec();
        let fingerprint_sha256 = sha256_hex(&cert_der);

        Ok(IssuedServerCert {
            cert_pem: cert.pem(),
            key_pem: server_key.serialize_pem(),
            cert_der,
            key_pkcs8_der: server_key.serialize_der(),
            ca_cert_pem: self.ca_cert_pem.clone(),
            fingerprint_sha256,
        })
    }

    pub fn issue_default_server_tls_cert(
        &self,
        public_host: Option<&str>,
        extra_sans: &[String],
    ) -> Result<IssuedServerCert, PkiError> {
        self.issue_server_tls_cert(public_host, extra_sans, SERVER_CERT_DEFAULT_VALIDITY_DAYS)
    }

    /// Build a `rustls::ServerConfig` with the given server cert and optional mTLS client CA verification.
    pub fn build_server_tls_config(
        &self,
        server_cert: &IssuedServerCert,
        require_client_cert: bool,
    ) -> Result<Arc<ServerConfig>, PkiError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider));
        let builder = if require_client_cert {
            builder.with_protocol_versions(&[&rustls::version::TLS13])?
        } else {
            builder.with_safe_default_protocol_versions()?
        };

        let builder = if require_client_cert {
            let roots = Arc::new(self.root_cert_store()?);
            let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| PkiError::InvalidCertificate(format!("client verifier error: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };

        let cert_chain = vec![CertificateDer::from(server_cert.cert_der.clone())];
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_cert.key_pkcs8_der.clone()));
        let cfg = builder.with_single_cert(cert_chain, key_der)?;
        Ok(Arc::new(cfg))
    }

    /// Validate a client PKCS#10 CSR and sign a 7-day device certificate with the Root CA.
    pub fn issue_device_cert(
        &self,
        csr_pem_or_der: &[u8],
        user_id: Uuid,
        device_id: &str,
    ) -> Result<IssuedDeviceCert, PkiError> {
        let norm_did = normalize_device_id(device_id);
        if !is_valid_device_id(&norm_did) {
            return Err(PkiError::InvalidInput(format!(
                "invalid device_id: {device_id}"
            )));
        }

        let verified_csr = parse_and_verify_csr(csr_pem_or_der)?;
        let (serial_bytes, serial_hex) = generate_positive_128bit_serial()?;

        let now_utc = Utc::now();
        let expires_utc = now_utc + chrono::Duration::days(DEVICE_CERT_VALIDITY_DAYS);
        let now = OffsetDateTime::now_utc();

        let cn = format!("rc-device-{norm_did}");
        let san_uri = format!("{DEVICE_SAN_URI_PREFIX}{user_id}{DEVICE_SAN_URI_MID}{norm_did}");
        let san_dns = format!("device-{norm_did}{DEVICE_SAN_DNS_SUFFIX}");

        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, &cn);

        let mut params = CertificateParams::default();
        params.distinguished_name = dn;
        params.subject_alt_names = vec![
            SanType::URI(san_uri.try_into()?),
            SanType::DnsName(san_dns.try_into()?),
        ];
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(SerialNumber::from_slice(&serial_bytes));
        params.not_before = now - time::Duration::minutes(1);
        params.not_after = now + time::Duration::days(DEVICE_CERT_VALIDITY_DAYS);

        let cert = params.signed_by(&verified_csr, &self.issuer_cert, &self.ca_key)?;
        let cert_der = cert.der().to_vec();
        let fingerprint_sha256 = sha256_hex(&cert_der);

        Ok(IssuedDeviceCert {
            serial_hex,
            user_id,
            device_id: norm_did,
            fingerprint_sha256,
            cert_pem: cert.pem(),
            cert_der,
            ca_cert_pem: self.ca_cert_pem.clone(),
            issued_at: now_utc,
            expires_at: expires_utc,
        })
    }

    /// Issue a 7-day device certificate from a CSR and persist it into `device_certificates`.
    pub async fn issue_and_persist_device_cert(
        &self,
        store: &dyn PkiStore,
        csr_pem_or_der: &[u8],
        user_id: Uuid,
        device_id: &str,
    ) -> Result<IssuedDeviceCert, PkiError> {
        let issued = self.issue_device_cert(csr_pem_or_der, user_id, device_id)?;
        store
            .insert_device_cert(DeviceCertRecord {
                serial_hex: issued.serial_hex.clone(),
                user_id: issued.user_id,
                device_id: issued.device_id.clone(),
                fingerprint_sha256: issued.fingerprint_sha256.clone(),
                cert_pem: issued.cert_pem.clone(),
                issued_at: issued.issued_at,
                expires_at: issued.expires_at,
                revoked_at: None,
                revoke_reason: None,
            })
            .await?;
        Ok(issued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CreateUserInput, MemoryDeviceStore, UserStore};

    #[tokio::test]
    async fn root_ca_persists_and_reloads_cleanly() {
        let store = MemoryDeviceStore::new();
        let pki1 = PkiAuthority::load_or_create(&store).await.unwrap();
        let pki2 = PkiAuthority::load_or_create(&store).await.unwrap();

        assert_eq!(pki1.ca_fingerprint_sha256(), pki2.ca_fingerprint_sha256());
        assert_eq!(pki1.ca_cert_pem(), pki2.ca_cert_pem());
        assert_eq!(pki1.ca_fingerprint_sha256().len(), 64);

        // Verify CA cert is valid CA with 10-year validity.
        let (_, ca_x509) = X509Certificate::from_der(pki1.ca_cert_der()).unwrap();
        assert!(ca_x509.is_ca());
        let span_secs =
            ca_x509.validity().not_after.timestamp() - ca_x509.validity().not_before.timestamp();
        assert!(span_secs >= ROOT_CA_VALIDITY_DAYS * 86_400);
    }

    #[test]
    fn server_tls_cert_contains_required_sans_and_verifies_against_ca() {
        let pki = PkiAuthority::generate_ephemeral().unwrap();
        let extra = vec![
            "10.0.0.5".to_string(),
            "relay.rabbitcontrol.test".to_string(),
        ];
        let srv = pki
            .issue_default_server_tls_cert(Some("172.16.1.2"), &extra)
            .unwrap();

        let (_, srv_x509) = X509Certificate::from_der(&srv.cert_der).unwrap();
        let (_, ca_x509) = X509Certificate::from_der(pki.ca_cert_der()).unwrap();
        srv_x509
            .verify_signature(Some(ca_x509.public_key()))
            .expect("server cert must verify under Root CA");

        let san = srv_x509.subject_alternative_name().unwrap().unwrap();
        let has_dns = |target: &str| {
            san.value
                .general_names
                .iter()
                .any(|g| matches!(g, GeneralName::DNSName(d) if *d == target))
        };
        let has_ip = |target: &[u8]| {
            san.value
                .general_names
                .iter()
                .any(|g| matches!(g, GeneralName::IPAddress(b) if *b == target))
        };
        assert!(has_dns("localhost"));
        assert!(has_dns("relay.rabbitcontrol.test"));
        assert!(has_ip(&[127, 0, 0, 1]));
        assert!(has_ip(&std::net::Ipv6Addr::LOCALHOST.octets()));
        assert!(has_ip(&[172, 16, 1, 2]));
        assert!(has_ip(&[10, 0, 0, 5]));
    }

    #[tokio::test]
    async fn csr_validation_device_cert_issuance_and_identity_parsing() {
        let store = MemoryDeviceStore::new();
        let user = store
            .create_user(CreateUserInput {
                email: "dev@example.com".into(),
                display_name: "Dev".into(),
                password_hash: "hash".into(),
                role: "user".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap();

        let pki1 = PkiAuthority::load_or_create(&store).await.unwrap();
        // Reload from store to confirm persisted CA produces valid signatures too.
        let pki2 = PkiAuthority::load_or_create(&store).await.unwrap();

        // Client generates an ECDSA P-256 keypair + CSR.
        let client_kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut csr_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        csr_params
            .distinguished_name
            .push(DnType::CommonName, "client-requested-ignored");
        let csr = csr_params.serialize_request(&client_kp).unwrap();
        let csr_pem = csr.pem().unwrap();

        let issued = pki2
            .issue_and_persist_device_cert(&store, csr_pem.as_bytes(), user.id, "123 456 789")
            .await
            .unwrap();

        assert_eq!(issued.device_id, "123456789");
        assert_eq!(issued.user_id, user.id);
        assert_eq!(issued.serial_hex.len(), 32);
        assert_eq!(issued.fingerprint_sha256.len(), 64);

        // Verify cert against Root CA.
        let (_, leaf_x509) = X509Certificate::from_der(&issued.cert_der).unwrap();
        let (_, ca_x509) = X509Certificate::from_der(pki1.ca_cert_der()).unwrap();
        leaf_x509
            .verify_signature(Some(ca_x509.public_key()))
            .expect("device cert must verify under Root CA");

        // Check CN, DNS SAN, EKU.
        let cn = leaf_x509
            .subject()
            .iter_common_name()
            .next()
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(cn, "rc-device-123456789");
        let eku = leaf_x509.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth);
        assert!(eku.server_auth);

        let san = leaf_x509.subject_alternative_name().unwrap().unwrap().value;
        assert!(san.general_names.iter().any(|n| matches!(
            n,
            GeneralName::DNSName(dns) if *dns == "device-123456789.rc.internal"
        )));

        // Parse peer identity via pure function.
        let identity = parse_peer_cert_identity(&issued.cert_der).unwrap();
        assert_eq!(identity.user_id, user.id);
        assert_eq!(identity.device_id, "123456789");
        assert_eq!(identity.serial_hex, issued.serial_hex);
        assert_eq!(identity.sha256_fingerprint, issued.fingerprint_sha256);

        // Persisted record matches.
        let stored_cert = store
            .get_device_cert(&issued.serial_hex)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_cert.device_id, "123456789");

        // Test mTLS rustls handshake between client & server using issued certs.
        let srv_cert = pki2.issue_default_server_tls_cert(None, &[]).unwrap();
        let server_cfg = pki2.build_server_tls_config(&srv_cert, true).unwrap();

        let client_roots = Arc::new(pki2.root_cert_store().unwrap());
        let client_cfg = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(client_roots)
            .with_client_auth_cert(
                vec![CertificateDer::from(issued.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(client_kp.serialize_der())),
            )
            .unwrap(),
        );

        let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let mut client_conn = rustls::ClientConnection::new(client_cfg, server_name).unwrap();
        let mut server_conn = rustls::ServerConnection::new(server_cfg).unwrap();

        // Drive in-memory TLS handshake to completion.
        while client_conn.is_handshaking() || server_conn.is_handshaking() {
            let mut buf = Vec::new();
            client_conn.write_tls(&mut buf).unwrap();
            if !buf.is_empty() {
                server_conn.read_tls(&mut &buf[..]).unwrap();
                server_conn.process_new_packets().unwrap();
            }
            buf.clear();
            server_conn.write_tls(&mut buf).unwrap();
            if !buf.is_empty() {
                client_conn.read_tls(&mut &buf[..]).unwrap();
                client_conn.process_new_packets().unwrap();
            }
        }

        let peer_certs = server_conn.peer_certificates().expect("mTLS peer cert");
        let parsed = parse_peer_cert_identity(peer_certs[0].as_ref()).unwrap();
        assert_eq!(parsed.user_id, user.id);
        assert_eq!(parsed.device_id, "123456789");
    }

    #[test]
    fn tampered_csr_is_rejected() {
        let pki = PkiAuthority::generate_ephemeral().unwrap();
        let kp = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let csr_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        let csr = csr_params.serialize_request(&kp).unwrap();
        let mut der = csr.der().to_vec();
        // Flip last byte of signature.
        let last = der.len() - 1;
        der[last] ^= 0xff;

        let err = pki
            .issue_device_cert(&der, Uuid::new_v4(), "123456789")
            .unwrap_err();
        assert!(matches!(err, PkiError::InvalidCsr(_)));
    }
}
