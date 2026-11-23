//! 设备身份密钥、PKCS#10 CSR 生成、设备证书持久化与自控 PoP 票据签验。
//!
//! 本模块全纯 Rust 实现，复用工作区既有 `ring`、`rustls-pemfile`、`base64`、`sha2`：
//! - 设备 Ed25519 密钥对（`device_ed25519.pk8`）用于自控免密 PoP 挑战签名；
//! - 设备 ECDSA P-256 密钥对（`device_tls_key.pem`）用于生成 PKCS#10 CSR 与 QUIC mTLS（私钥永不出设备）；
//! - 设备证书（`device_cert.pem` + `device_cert_meta.json`）支持 `< 48h` 或账号/设备解绑后自动续签判定；
//! - 实现 [`rc_session::auth::OwnerTicketSigner`] 与 [`rc_session::auth::OwnerTicketVerifier`]，
//!   字节级对齐 `server/backend/src/auth.rs` 与 `server/backend/src/pki.rs`。
//!
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use rc_net::mtls::{parse_rc_san_uri, MtlsConfig};
use rc_net::quic::Identity;
use rc_session::auth::{OwnerTicketSigner, OwnerTicketVerifier, VerifiedOwnerTicket};
use rc_session::proto::AUTH_SALT_LEN;
use rc_types::Error as RcError;
use ring::rand::SystemRandom;
use ring::signature::{
    self, EcdsaKeyPair, Ed25519KeyPair, KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_ASN1_SIGNING,
};
use serde::{Deserialize, Serialize};

use crate::app_prefs::{atomic_write_bytes, config_root_dir, encode_hex_lower};
use crate::server_config::parse_device_id_input;

/// 设备证书到期前自动续签窗口（48 小时，单位：秒）。
pub const CERT_RENEWAL_WINDOW_SECS: u64 = 48 * 3600;

/// 服务端签发的 `OwnerGrantTicket` 规范签名域分隔前缀（与 `server/backend/src/auth.rs` 完全对齐）。
pub const OWNER_GRANT_DOMAIN: &[u8] = b"RC-OWNER-GRANT-V1\0";

/// 自控 PoP 挑战规范签名域分隔前缀。
pub const OWNER_POP_DOMAIN: &[u8] = b"RC-OWNER-POP-V1\0";

const ED25519_KEY_FILENAME: &str = "device_ed25519.pk8";
const TLS_KEY_FILENAME: &str = "device_tls_key.pem";
const DEVICE_CERT_FILENAME: &str = "device_cert.pem";
const DEVICE_CERT_META_FILENAME: &str = "device_cert_meta.json";

/// `id-ecPublicKey` OID (`1.2.840.10045.2.1`)。
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
/// `prime256v1` / `secp256r1` OID (`1.2.840.10045.3.1.7`)。
const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
/// `ecdsa-with-SHA256` OID (`1.2.840.10045.4.3.2`)。
const OID_ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
/// `commonName` OID (`2.5.4.3`)。
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];

/// 返回默认设备证书与密钥存储目录：`$HOME/.config/rabbitcontrol/certs`。
pub fn default_certs_dir() -> PathBuf {
    config_root_dir().join("certs")
}

/// 持久化于本地磁盘的设备证书元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCertMetadata {
    /// 证书 128-bit 序列号（小写十六进制）。
    pub serial_hex: String,
    /// 证书绑定的用户 UUID（小写带连字符）。
    pub user_id: String,
    /// 证书绑定的 9 位纯数字设备码。
    pub device_id: String,
    /// 证书过期时间（Unix 秒）。
    pub expires_at_unix_sec: u64,
    /// 叶子证书 DER 的 SHA-256 指纹（可选）。
    #[serde(default)]
    pub fingerprint_sha256: String,
    /// 证书签发时间（Unix 秒，可选）。
    #[serde(default)]
    pub issued_at_unix_sec: u64,
}

/// 服务端签发的自控免密一次性票据载荷（匹配 `server/backend/src/auth.rs::OwnerGrantTicket`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGrantPayload {
    /// 16 字节随机票据 ID（32 位小写十六进制）。
    pub ticket_id: String,
    /// 签发用户 UUID（小写带连字符）。
    pub user_id: String,
    /// 主控端 9 位设备码。
    pub caller_device_id: String,
    /// 主控端设备 Ed25519 公钥（64 位小写十六进制）。
    pub caller_pubkey_hex: String,
    /// 被控端目标 9 位设备码。
    pub target_device_id: String,
    /// 关联信令会话 ID。
    pub session_id: String,
    /// 票据过期时间（Unix 秒）。
    pub expires_at_unix_sec: u64,
    /// 服务端 Ed25519 签名（128 位小写十六进制）。
    pub signature_hex: String,
}

/// 本地设备身份密钥、CSR 与 mTLS 设备证书管理器。
#[derive(Debug)]
pub struct CertManager {
    dir: PathBuf,
    ed25519_pk8: Mutex<Vec<u8>>,
    device_pubkey_bytes: Mutex<[u8; 32]>,
    device_pubkey_hex: Mutex<String>,
    expected_binding: Mutex<Option<(String, String)>>,
}

impl CertManager {
    /// 打开或初始化指定目录的设备身份密钥（目录及密钥文件自动收紧为 `0700`/`0600`）。
    pub fn load_or_init(dir: PathBuf) -> Result<Self, String> {
        ensure_dir_0700(&dir)?;

        let ed_path = dir.join(ED25519_KEY_FILENAME);
        let (ed_pk8, pub_bytes, pub_hex) = load_or_create_ed25519(&ed_path)?;

        let tls_path = dir.join(TLS_KEY_FILENAME);
        let _ = load_or_create_tls_key_pem(&tls_path)?;

        Ok(Self {
            dir,
            ed25519_pk8: Mutex::new(ed_pk8),
            device_pubkey_bytes: Mutex::new(pub_bytes),
            device_pubkey_hex: Mutex::new(pub_hex),
            expected_binding: Mutex::new(None),
        })
    }

    /// 使用默认证书目录（`$HOME/.config/rabbitcontrol/certs`）打开或初始化设备身份管理器。
    pub fn default_dir() -> Result<Self, String> {
        Self::load_or_init(default_certs_dir())
    }

    /// 返回当前证书与密钥目录路径。
    pub fn certs_dir(&self) -> &Path {
        &self.dir
    }

    /// 返回本机设备 Ed25519 公钥（32 字节编码为 64 位小写十六进制字符串）。
    pub fn device_pubkey_hex(&self) -> String {
        self.device_pubkey_hex
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 返回本机设备 Ed25519 原始 32 字节公钥。
    pub fn device_pubkey_bytes(&self) -> [u8; 32] {
        *self
            .device_pubkey_bytes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 读取本机设备 Ed25519 密钥对（用于创建 [`RcOwnerTicketSigner`]）。
    pub fn ed25519_keypair(&self) -> Result<Ed25519KeyPair, String> {
        let pk8 = self
            .ed25519_pk8
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ed25519KeyPair::from_pkcs8(&pk8)
            .map_err(|e| format!("解析本地 Ed25519 PKCS#8 密钥失败：{e}"))
    }

    /// 读取或按需重建本地 ECDSA P-256 TLS 私钥（PKCS#8 PEM，私钥永不出设备）。
    pub fn tls_key_pem(&self) -> Result<String, String> {
        let tls_path = self.dir.join(TLS_KEY_FILENAME);
        let (pem, _) = load_or_create_tls_key_pem(&tls_path)?;
        Ok(pem)
    }

    /// 使用本地 ECDSA P-256 私钥生成纯 Rust ASN.1 DER PKCS#10 `CertificationRequest` PEM。
    ///
    /// 生成结构严格匹配 `server/backend/src/pki.rs::parse_and_verify_csr`：
    /// `OID_KEY_TYPE_EC_PUBLIC_KEY` + `OID_EC_P256` + 65 字节未压缩公钥点 + `ecdsa-with-SHA256` 自签名。
    pub fn generate_csr_pem(&self, common_name: Option<&str>) -> Result<String, String> {
        let tls_path = self.dir.join(TLS_KEY_FILENAME);
        let (_, pkcs8_der) = load_or_create_tls_key_pem(&tls_path)?;
        let rng = SystemRandom::new();
        let key_pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8_der, &rng)
            .map_err(|e| format!("加载 ECDSA P-256 私钥失败：{e}"))?;

        let raw_pub = key_pair.public_key().as_ref();
        if raw_pub.len() != 65 || raw_pub[0] != 0x04 {
            return Err(format!(
                "ECDSA P-256 公钥格式异常（期望 65 字节 0x04 未压缩点，实际 {} 字节）",
                raw_pub.len()
            ));
        }

        let cn = common_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("RabbitControl Device");

        let cri_der = build_certification_request_info_der(cn, raw_pub);
        let sig = key_pair
            .sign(&rng, &cri_der)
            .map_err(|e| format!("签发 PKCS#10 CSR 签名失败：{e}"))?;

        let sig_alg = der_sequence(&der_oid(OID_ECDSA_WITH_SHA256));
        let sig_bits = der_bit_string(sig.as_ref());
        let mut csr_body = Vec::with_capacity(cri_der.len() + sig_alg.len() + sig_bits.len());
        csr_body.extend_from_slice(&cri_der);
        csr_body.extend_from_slice(&sig_alg);
        csr_body.extend_from_slice(&sig_bits);
        let csr_der = der_sequence(&csr_body);

        Ok(encode_pem("CERTIFICATE REQUEST", &csr_der))
    }

    /// 设置当前会话期望绑定的 `(user_id, device_id)`，供 [`Self::needs_renewal`] 校验归属。
    pub fn set_expected_binding(&self, user_id: &str, device_id: &str) {
        let uid = user_id.trim().to_ascii_lowercase();
        let did = parse_device_id_input(device_id).unwrap_or_else(|| device_id.trim().to_string());
        let mut guard = self
            .expected_binding
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some((uid, did));
    }

    /// 清除当前期望绑定的 `(user_id, device_id)`。
    pub fn clear_expected_binding(&self) {
        *self
            .expected_binding
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 校验证书 PEM、SAN URI 与本地 TLS 私钥匹配性后，原子持久化 `device_cert.pem` 与元数据。
    pub fn save_device_cert(
        &self,
        cert_pem: &str,
        meta: &DeviceCertMetadata,
    ) -> Result<(), String> {
        let norm_uid = meta.user_id.trim().to_ascii_lowercase();
        let norm_did = parse_device_id_input(&meta.device_id)
            .ok_or_else(|| format!("设备证书元数据 device_id 无效：{:?}", meta.device_id))?;
        if norm_uid.is_empty() {
            return Err("设备证书元数据 user_id 不能为空".to_string());
        }
        if meta.serial_hex.trim().is_empty() {
            return Err("设备证书元数据 serial_hex 不能为空".to_string());
        }

        let certs = Identity::certs_from_pem(cert_pem)
            .map_err(|e| format!("解析设备证书 PEM 失败：{e}"))?;
        let leaf = certs
            .first()
            .ok_or_else(|| "设备证书 PEM 不包含任何证书".to_string())?;
        let (cert_uid, cert_did) = parse_rc_san_uri(leaf.as_ref())
            .ok_or_else(|| "设备证书缺少合法的 RabbitControl SAN URI".to_string())?;
        if cert_uid.to_ascii_lowercase() != norm_uid || cert_did != norm_did {
            return Err(format!(
                "设备证书 SAN URI ({cert_uid}:{cert_did}) 与元数据 ({norm_uid}:{norm_did}) 不一致"
            ));
        }

        let key_pem = self.tls_key_pem()?;
        let _ = Identity::from_pem(cert_pem, &key_pem)
            .map_err(|e| format!("设备证书与本地 TLS 私钥无法配对：{e}"))?;

        let clean_meta = DeviceCertMetadata {
            serial_hex: meta.serial_hex.trim().to_ascii_lowercase(),
            user_id: norm_uid.clone(),
            device_id: norm_did.clone(),
            expires_at_unix_sec: meta.expires_at_unix_sec,
            fingerprint_sha256: if meta.fingerprint_sha256.trim().is_empty() {
                rc_net::mtls::cert_sha256_hex(leaf.as_ref())
            } else {
                meta.fingerprint_sha256.trim().to_ascii_lowercase()
            },
            issued_at_unix_sec: meta.issued_at_unix_sec,
        };

        ensure_dir_0700(&self.dir)?;
        let cert_path = self.dir.join(DEVICE_CERT_FILENAME);
        let meta_path = self.dir.join(DEVICE_CERT_META_FILENAME);
        let meta_json = serde_json::to_vec_pretty(&clean_meta)
            .map_err(|e| format!("序列化设备证书元数据失败：{e}"))?;

        atomic_write_bytes(&cert_path, cert_pem.as_bytes(), !cert_pem.ends_with('\n'))?;
        atomic_write_bytes(&meta_path, &meta_json, true)?;
        self.set_expected_binding(&norm_uid, &norm_did);
        Ok(())
    }

    /// 读取本地已持久化的设备证书 PEM 与元数据；若任一文件缺失或损坏则返回 `None`。
    pub fn load_device_cert(&self) -> Option<(String, DeviceCertMetadata)> {
        let cert_path = self.dir.join(DEVICE_CERT_FILENAME);
        let meta_path = self.dir.join(DEVICE_CERT_META_FILENAME);
        let cert_pem = fs::read_to_string(&cert_path).ok()?;
        let meta_bytes = fs::read(&meta_path).ok()?;
        let meta: DeviceCertMetadata = serde_json::from_slice(&meta_bytes).ok()?;
        let certs = Identity::certs_from_pem(&cert_pem).ok()?;
        let leaf = certs.first()?;
        let (cert_uid, cert_did) = parse_rc_san_uri(leaf.as_ref())?;
        if !cert_uid.eq_ignore_ascii_case(meta.user_id.trim()) || cert_did != meta.device_id.trim()
        {
            return None;
        }
        Some((cert_pem, meta))
    }

    /// 读取本地已持久化的设备证书元数据。
    pub fn load_device_cert_metadata(&self) -> Option<DeviceCertMetadata> {
        self.load_device_cert().map(|(_, meta)| meta)
    }

    /// 判断当前设备证书是否需要重新签发或续签（剩余 `< 48h` 或未绑定当前 `user_id`/`device_id`）。
    pub fn needs_renewal(&self, now_unix_sec: u64) -> bool {
        let binding = self
            .expected_binding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match binding {
            Some((uid, did)) => self.needs_renewal_for(now_unix_sec, &uid, &did),
            None => {
                let Some((_, meta)) = self.load_device_cert() else {
                    return true;
                };
                if meta.user_id.trim().is_empty() || meta.device_id.trim().is_empty() {
                    return true;
                }
                meta.expires_at_unix_sec <= now_unix_sec.saturating_add(CERT_RENEWAL_WINDOW_SECS)
            }
        }
    }

    /// 判断当前设备证书相对指定 `(expected_user_id, expected_device_id)` 是否需要签发或续签。
    pub fn needs_renewal_for(
        &self,
        now_unix_sec: u64,
        expected_user_id: &str,
        expected_device_id: &str,
    ) -> bool {
        let norm_uid = expected_user_id.trim().to_ascii_lowercase();
        let Some(norm_did) = parse_device_id_input(expected_device_id) else {
            return true;
        };
        if norm_uid.is_empty() {
            return true;
        }
        let Some((_, meta)) = self.load_device_cert() else {
            return true;
        };
        if meta.user_id.trim().to_ascii_lowercase() != norm_uid || meta.device_id != norm_did {
            return true;
        }
        meta.expires_at_unix_sec <= now_unix_sec.saturating_add(CERT_RENEWAL_WINDOW_SECS)
    }

    /// 登出或切号时删除本地设备证书与元数据，并清除绑定状态（支持后续重新签发）。
    pub fn clear_device_cert(&self) -> Result<(), String> {
        self.clear_expected_binding();
        for name in [DEVICE_CERT_FILENAME, DEVICE_CERT_META_FILENAME] {
            let path = self.dir.join(name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(format!("删除证书文件 {} 失败：{err}", path.display()));
                }
            }
        }
        Ok(())
    }

    /// 构造用于 QUIC 双向 mTLS 的 [`MtlsConfig`]。
    pub fn build_mtls_config(
        &self,
        ca_cert_pem: &str,
        expected_peer_device_id: Option<String>,
        expected_peer_user_id: Option<String>,
    ) -> Result<MtlsConfig, String> {
        let cert_path = self.dir.join(DEVICE_CERT_FILENAME);
        let cert_pem = fs::read_to_string(&cert_path)
            .map_err(|e| format!("读取本地设备证书 {} 失败：{e}", cert_path.display()))?;
        let key_pem = self.tls_key_pem()?;

        let identity = Identity::from_pem(&cert_pem, &key_pem)
            .map_err(|e| format!("构造 mTLS 本端身份失败：{e}"))?;
        let ca_certs = Identity::certs_from_pem(ca_cert_pem)
            .map_err(|e| format!("解析 Root CA PEM 失败：{e}"))?;
        if ca_certs.is_empty() {
            return Err("Root CA PEM 不包含任何证书".to_string());
        }

        let norm_peer_did = match expected_peer_device_id {
            Some(raw) => {
                let norm = parse_device_id_input(&raw)
                    .ok_or_else(|| format!("expected_peer_device_id 格式无效：{raw:?}"))?;
                Some(norm)
            }
            None => None,
        };
        let norm_peer_uid = expected_peer_user_id
            .map(|u| u.trim().to_ascii_lowercase())
            .filter(|u| !u.is_empty());

        Ok(MtlsConfig {
            identity,
            ca_certs,
            expected_peer_device_id: norm_peer_did,
            expected_peer_user_id: norm_peer_uid,
        })
    }

    /// 使用本机设备 Ed25519 私钥与服务端下发的 `owner_grant` 构造 [`RcOwnerTicketSigner`]。
    pub fn create_owner_ticket_signer(
        &self,
        grant: OwnerGrantPayload,
    ) -> Result<RcOwnerTicketSigner, String> {
        let pk8 = self
            .ed25519_pk8
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        RcOwnerTicketSigner::new(grant, pk8)
    }
}

/// 构造 `OwnerGrantPayload` 的确定性规范签名原文（字节级对齐 `server/backend/src/auth.rs`）。
pub fn owner_grant_signing_message(grant: &OwnerGrantPayload) -> Result<Vec<u8>, String> {
    let ticket_id = grant.ticket_id.trim().to_ascii_lowercase();
    if ticket_id.len() != 32 || !ticket_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("ticket_id 必须为 32 位十六进制字符串".to_string());
    }
    let user_id = grant.user_id.trim().to_ascii_lowercase();
    if user_id.is_empty() || user_id.contains('\0') || user_id.chars().any(char::is_control) {
        return Err("user_id 不能为空或包含控制字符".to_string());
    }
    let caller_did = parse_device_id_input(&grant.caller_device_id)
        .ok_or_else(|| format!("caller_device_id 无效：{:?}", grant.caller_device_id))?;
    let target_did = parse_device_id_input(&grant.target_device_id)
        .ok_or_else(|| format!("target_device_id 无效：{:?}", grant.target_device_id))?;
    let caller_pubkey_hex = grant.caller_pubkey_hex.trim().to_ascii_lowercase();
    if caller_pubkey_hex.len() != 64 || !caller_pubkey_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("caller_pubkey_hex 必须为 64 位十六进制字符串".to_string());
    }
    let session_id = grant.session_id.trim();
    if session_id.is_empty()
        || session_id.len() > 128
        || session_id.contains('\0')
        || session_id.chars().any(char::is_control)
    {
        return Err("session_id 长度必须为 1..=128 且不含控制字符".to_string());
    }
    let exp_str = grant.expires_at_unix_sec.to_string();

    let mut msg = Vec::with_capacity(OWNER_GRANT_DOMAIN.len() + 256);
    msg.extend_from_slice(OWNER_GRANT_DOMAIN);
    msg.extend_from_slice(ticket_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(user_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(caller_did.as_bytes());
    msg.push(0);
    msg.extend_from_slice(caller_pubkey_hex.as_bytes());
    msg.push(0);
    msg.extend_from_slice(target_did.as_bytes());
    msg.push(0);
    msg.extend_from_slice(session_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(exp_str.as_bytes());
    Ok(msg)
}

/// 构造自控 PoP 挑战的规范签名原文：
/// `b"RC-OWNER-POP-V1\0" + hello_session_id.to_le_bytes() + salt + target_device_id.as_bytes()`。
pub fn owner_pop_signing_message(
    hello_session_id: u64,
    salt: &[u8; AUTH_SALT_LEN],
    target_device_id: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(OWNER_POP_DOMAIN.len() + 8 + AUTH_SALT_LEN + 16);
    msg.extend_from_slice(OWNER_POP_DOMAIN);
    msg.extend_from_slice(&hello_session_id.to_le_bytes());
    msg.extend_from_slice(salt);
    msg.extend_from_slice(target_device_id.as_bytes());
    msg
}

/// 控制端（Caller）自控免密票据签名器，实现 [`OwnerTicketSigner`]。
pub struct RcOwnerTicketSigner {
    ticket_bytes: Vec<u8>,
    target_device_id: String,
    ed25519: Ed25519KeyPair,
}

impl std::fmt::Debug for RcOwnerTicketSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RcOwnerTicketSigner")
            .field("target_device_id", &self.target_device_id)
            .field("ticket_len", &self.ticket_bytes.len())
            .finish()
    }
}

impl RcOwnerTicketSigner {
    /// 使用 `OwnerGrantPayload` 与本地设备 Ed25519 PKCS#8 DER 私钥构造签名器。
    pub fn new(grant: OwnerGrantPayload, ed25519_pk8_der: Vec<u8>) -> Result<Self, String> {
        let target_did = parse_device_id_input(&grant.target_device_id)
            .ok_or_else(|| format!("target_device_id 无效：{:?}", grant.target_device_id))?;
        let ed25519 = Ed25519KeyPair::from_pkcs8(&ed25519_pk8_der)
            .map_err(|e| format!("加载 Ed25519 PKCS#8 私钥失败：{e}"))?;
        let mut normalized = grant;
        normalized.target_device_id = target_did.clone();
        if let Some(caller) = parse_device_id_input(&normalized.caller_device_id) {
            normalized.caller_device_id = caller;
        }
        normalized.user_id = normalized.user_id.trim().to_ascii_lowercase();
        normalized.caller_pubkey_hex = normalized.caller_pubkey_hex.trim().to_ascii_lowercase();
        let ticket_bytes = serde_json::to_vec(&normalized)
            .map_err(|e| format!("序列化 OwnerGrantPayload 失败：{e}"))?;
        Ok(Self {
            ticket_bytes,
            target_device_id: target_did,
            ed25519,
        })
    }
}

impl OwnerTicketSigner for RcOwnerTicketSigner {
    fn ticket_payload(&self) -> Vec<u8> {
        self.ticket_bytes.clone()
    }

    fn sign_challenge(
        &self,
        hello_session_id: u64,
        salt: &[u8; AUTH_SALT_LEN],
    ) -> rc_types::Result<Vec<u8>> {
        let msg = owner_pop_signing_message(hello_session_id, salt, &self.target_device_id);
        Ok(self.ed25519.sign(&msg).as_ref().to_vec())
    }
}

/// 被控端（Host）自控免密票据校验器，实现 [`OwnerTicketVerifier`]。
#[derive(Debug, Clone)]
pub struct RcOwnerTicketVerifier {
    server_auth_pubkey: [u8; 32],
    local_user_id: String,
    local_device_id: String,
}

impl RcOwnerTicketVerifier {
    /// 使用锚定的服务端 Ed25519 公钥（64 位十六进制）、本机绑定 `user_id` 与 `device_id` 构造校验器。
    pub fn new(
        server_auth_pubkey_hex: &str,
        local_user_id: String,
        local_device_id: String,
    ) -> Result<Self, String> {
        let pubkey_vec = decode_hex(server_auth_pubkey_hex.trim())
            .map_err(|e| format!("服务端 Ed25519 公钥 hex 无效：{e}"))?;
        let server_auth_pubkey: [u8; 32] = pubkey_vec
            .try_into()
            .map_err(|_| "服务端 Ed25519 公钥长度必须为 32 字节（64 位十六进制）".to_string())?;
        let norm_uid = local_user_id.trim().to_ascii_lowercase();
        if norm_uid.is_empty() {
            return Err("local_user_id 不能为空".to_string());
        }
        let norm_did = parse_device_id_input(&local_device_id)
            .ok_or_else(|| format!("local_device_id 无效：{local_device_id:?}"))?;

        Ok(Self {
            server_auth_pubkey,
            local_user_id: norm_uid,
            local_device_id: norm_did,
        })
    }
}

fn proto_err(msg: impl Into<String>) -> RcError {
    RcError::Protocol {
        offset: 0,
        msg: msg.into(),
    }
}

impl OwnerTicketVerifier for RcOwnerTicketVerifier {
    fn verify_ticket(
        &self,
        payload: &[u8],
        now_unix_sec: u64,
    ) -> rc_types::Result<VerifiedOwnerTicket> {
        let grant: OwnerGrantPayload = serde_json::from_slice(payload)
            .map_err(|e| proto_err(format!("invalid OwnerGrantPayload JSON: {e}")))?;

        if now_unix_sec >= grant.expires_at_unix_sec {
            return Err(proto_err("owner grant ticket expired"));
        }

        let grant_uid = grant.user_id.trim().to_ascii_lowercase();
        if grant_uid != self.local_user_id {
            return Err(proto_err(format!(
                "owner grant user_id mismatch: expected {}, got {grant_uid}",
                self.local_user_id
            )));
        }

        let target_did = parse_device_id_input(&grant.target_device_id).ok_or_else(|| {
            proto_err(format!(
                "invalid target_device_id in owner grant: {:?}",
                grant.target_device_id
            ))
        })?;
        if target_did != self.local_device_id {
            return Err(proto_err(format!(
                "owner grant target_device_id mismatch: expected {}, got {target_did}",
                self.local_device_id
            )));
        }

        let caller_did = parse_device_id_input(&grant.caller_device_id).ok_or_else(|| {
            proto_err(format!(
                "invalid caller_device_id in owner grant: {:?}",
                grant.caller_device_id
            ))
        })?;

        let sig_bytes = decode_hex(grant.signature_hex.trim())
            .map_err(|e| proto_err(format!("invalid owner grant signature hex: {e}")))?;
        if sig_bytes.len() != 64 {
            return Err(proto_err("owner grant signature must be 64 bytes"));
        }

        let msg = owner_grant_signing_message(&grant).map_err(proto_err)?;
        UnparsedPublicKey::new(&signature::ED25519, &self.server_auth_pubkey)
            .verify(&msg, &sig_bytes)
            .map_err(|_| proto_err("owner grant server signature invalid"))?;

        Ok(VerifiedOwnerTicket {
            ticket_id: grant.ticket_id.trim().to_ascii_lowercase(),
            user_id: grant_uid,
            caller_device_id: caller_did,
            caller_pubkey_hex: grant.caller_pubkey_hex.trim().to_ascii_lowercase(),
            target_device_id: target_did,
            session_id_str: grant.session_id.trim().to_string(),
            expires_at_unix_sec: grant.expires_at_unix_sec,
        })
    }

    fn verify_challenge_signature(
        &self,
        ticket: &VerifiedOwnerTicket,
        hello_session_id: u64,
        salt: &[u8; AUTH_SALT_LEN],
        signature: &[u8],
    ) -> rc_types::Result<()> {
        if signature.len() != 64 {
            return Err(proto_err(format!(
                "owner PoP signature must be 64 bytes, got {}",
                signature.len()
            )));
        }
        let caller_pub = decode_hex(ticket.caller_pubkey_hex.trim())
            .map_err(|e| proto_err(format!("invalid caller_pubkey_hex: {e}")))?;
        if caller_pub.len() != 32 {
            return Err(proto_err("caller_pubkey_hex must decode to 32 bytes"));
        }

        let msg = owner_pop_signing_message(hello_session_id, salt, &ticket.target_device_id);
        UnparsedPublicKey::new(&signature::ED25519, &caller_pub)
            .verify(&msg, signature)
            .map_err(|_| proto_err("owner PoP challenge signature invalid"))?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────
// 内部辅助函数：目录权限、密钥加载、ASN.1 DER 编码与 Hex/PEM 工具
// ─────────────────────────────────────────────────────────────────

fn ensure_dir_0700(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("创建证书目录 {} 失败：{e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn load_or_create_ed25519(path: &Path) -> Result<(Vec<u8>, [u8; 32], String), String> {
    if let Ok(existing) = fs::read(path) {
        if let Ok(kp) = Ed25519KeyPair::from_pkcs8(&existing) {
            if let Ok(pub_bytes) = <[u8; 32]>::try_from(kp.public_key().as_ref()) {
                let hex = encode_hex_lower(&pub_bytes);
                return Ok((existing, pub_bytes, hex));
            }
        }
    }

    let rng = SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|e| format!("生成设备 Ed25519 密钥对失败：{e}"))?;
    let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref())
        .map_err(|e| format!("加载新生成的 Ed25519 密钥对失败：{e}"))?;
    let pub_bytes: [u8; 32] = kp
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| "Ed25519 公钥长度异常".to_string())?;
    let pub_hex = encode_hex_lower(&pub_bytes);

    atomic_write_bytes(path, doc.as_ref(), false)?;
    Ok((doc.as_ref().to_vec(), pub_bytes, pub_hex))
}

fn load_or_create_tls_key_pem(path: &Path) -> Result<(String, Vec<u8>), String> {
    let rng = SystemRandom::new();
    if let Ok(existing_pem) = fs::read_to_string(path) {
        if let Ok(Some(rustls::pki_types::PrivateKeyDer::Pkcs8(pk8))) =
            rustls_pemfile::private_key(&mut BufReader::new(existing_pem.as_bytes()))
        {
            let der = pk8.secret_pkcs8_der().to_vec();
            if EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &der, &rng).is_ok() {
                return Ok((existing_pem, der));
            }
        }
    }

    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .map_err(|e| format!("生成 ECDSA P-256 TLS 密钥失败：{e}"))?;
    let der = doc.as_ref().to_vec();
    let pem = encode_pem("PRIVATE KEY", &der);
    atomic_write_bytes(path, pem.as_bytes(), false)?;
    Ok((pem, der))
}

fn build_certification_request_info_der(common_name: &str, uncompressed_pubkey: &[u8]) -> Vec<u8> {
    let version = der_tlv(0x02, &[0x00]); // INTEGER 0 (v1)

    // Subject: SEQUENCE { SET { SEQUENCE { OID(2.5.4.3), UTF8String(cn) } } }
    let mut cn_attr_body = Vec::new();
    cn_attr_body.extend_from_slice(&der_oid(OID_COMMON_NAME));
    cn_attr_body.extend_from_slice(&der_tlv(0x0C, common_name.as_bytes()));
    let rdn = der_tlv(0x31, &der_sequence(&cn_attr_body));
    let subject = der_sequence(&rdn);

    // SubjectPublicKeyInfo: SEQUENCE { SEQUENCE { OID(ecPublicKey), OID(prime256v1) }, BIT STRING }
    let mut alg_body = Vec::new();
    alg_body.extend_from_slice(&der_oid(OID_EC_PUBLIC_KEY));
    alg_body.extend_from_slice(&der_oid(OID_PRIME256V1));
    let spki_alg = der_sequence(&alg_body);
    let spki_key = der_bit_string(uncompressed_pubkey);
    let mut spki_body = Vec::with_capacity(spki_alg.len() + spki_key.len());
    spki_body.extend_from_slice(&spki_alg);
    spki_body.extend_from_slice(&spki_key);
    let spki = der_sequence(&spki_body);

    // attributes [0] IMPLICIT Attributes (empty)
    let attrs = [0xA0u8, 0x00];

    let mut cri_body = Vec::with_capacity(version.len() + subject.len() + spki.len() + attrs.len());
    cri_body.extend_from_slice(&version);
    cri_body.extend_from_slice(&subject);
    cri_body.extend_from_slice(&spki);
    cri_body.extend_from_slice(&attrs);
    der_sequence(&cri_body)
}

fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 5 + content.len());
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xFF {
        out.push(0x81);
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xFF) as u8);
    } else {
        out.push(0x84);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(content);
    out
}

fn der_sequence(content: &[u8]) -> Vec<u8> {
    der_tlv(0x30, content)
}

fn der_oid(oid_bytes: &[u8]) -> Vec<u8> {
    der_tlv(0x06, oid_bytes)
}

fn der_bit_string(raw: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(1 + raw.len());
    body.push(0x00); // unused bits = 0
    body.extend_from_slice(raw);
    der_tlv(0x03, &body)
}

fn encode_pem(label: &str, der: &[u8]) -> String {
    let b64 = BASE64_STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        if let Ok(line) = std::str::from_utf8(chunk) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

pub(crate) fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("十六进制字符串长度必须为偶数".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("非法十六进制字符：{:?}", b as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use rand::RngCore;
    use ring::signature::{ECDSA_P256_SHA256_ASN1, ED25519};

    fn temp_test_dir(tag: &str) -> PathBuf {
        let mut suffix = [0u8; 6];
        OsRng.fill_bytes(&mut suffix);
        std::env::temp_dir().join(format!("rc-ui-cert-{tag}-{}", encode_hex_lower(&suffix)))
    }

    struct ParsedTlv<'a> {
        tag: u8,
        full: &'a [u8],
        body: &'a [u8],
        rest: &'a [u8],
    }

    fn read_der_tlv(input: &[u8]) -> Option<ParsedTlv<'_>> {
        let tag = *input.first()?;
        let lb = *input.get(1)?;
        let (hdr_len, len) = if lb < 0x80 {
            (2usize, lb as usize)
        } else {
            let cnt = (lb & 0x7F) as usize;
            let mut val = 0usize;
            for &b in input.get(2..2 + cnt)? {
                val = (val << 8) | (b as usize);
            }
            (2 + cnt, val)
        };
        let end = hdr_len + len;
        let full = input.get(..end)?;
        let body = input.get(hdr_len..end)?;
        let rest = input.get(end..)?;
        Some(ParsedTlv {
            tag,
            full,
            body,
            rest,
        })
    }

    /// 用纯 Rust DER 生成器签发自签名 CA 证书与绑定到 CA 的设备叶子证书（用于离线 mTLS 测试）。
    fn test_issue_ca_and_leaf_pem(
        leaf_spki_der: &[u8],
        user_id: &str,
        device_id: &str,
    ) -> (String, String) {
        let rng = SystemRandom::new();
        let ca_pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let ca_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, ca_pk8.as_ref(), &rng)
                .unwrap();

        let ca_spki = build_ec_spki_der(ca_kp.public_key().as_ref());
        let ca_dn = build_cn_dn_der("RabbitControl Test Root CA");
        let ca_ext = build_ca_extensions_der();
        let ca_cert_der = sign_x509_v3(
            &ca_kp,
            &[0x01],
            &ca_dn,
            &ca_dn,
            "250101000000Z",
            "350101000000Z",
            &ca_spki,
            &ca_ext,
        );

        let leaf_cn = format!("rc-device-{device_id}");
        let leaf_dn = build_cn_dn_der(&leaf_cn);
        let san_uri = format!("urn:rabbitcontrol:user:{user_id}:device:{device_id}");
        let san_dns = format!("device-{device_id}.rc.internal");
        let leaf_ext = build_leaf_extensions_der(&san_uri, &san_dns);
        let leaf_cert_der = sign_x509_v3(
            &ca_kp,
            &[0x02, 0x10],
            &ca_dn,
            &leaf_dn,
            "250101000000Z",
            "350101000000Z",
            leaf_spki_der,
            &leaf_ext,
        );

        (
            encode_pem("CERTIFICATE", &ca_cert_der),
            encode_pem("CERTIFICATE", &leaf_cert_der),
        )
    }

    fn build_ec_spki_der(uncompressed_pub: &[u8]) -> Vec<u8> {
        let mut alg_body = Vec::new();
        alg_body.extend_from_slice(&der_oid(OID_EC_PUBLIC_KEY));
        alg_body.extend_from_slice(&der_oid(OID_PRIME256V1));
        let mut spki = Vec::new();
        spki.extend_from_slice(&der_sequence(&alg_body));
        spki.extend_from_slice(&der_bit_string(uncompressed_pub));
        der_sequence(&spki)
    }

    fn build_cn_dn_der(cn: &str) -> Vec<u8> {
        let mut atv = Vec::new();
        atv.extend_from_slice(&der_oid(OID_COMMON_NAME));
        atv.extend_from_slice(&der_tlv(0x0C, cn.as_bytes()));
        der_sequence(&der_tlv(0x31, &der_sequence(&atv)))
    }

    fn build_ca_extensions_der() -> Vec<u8> {
        // BasicConstraints (2.5.29.19) critical=true, cA=true
        let bc = [
            der_oid(&[0x55, 0x1D, 0x13]),
            der_tlv(0x01, &[0xFF]),
            der_tlv(0x04, &der_sequence(&der_tlv(0x01, &[0xFF]))),
        ]
        .concat();
        // KeyUsage (2.5.29.15) critical=true, keyCertSign + cRLSign + digitalSignature
        let ku = [
            der_oid(&[0x55, 0x1D, 0x0F]),
            der_tlv(0x01, &[0xFF]),
            der_tlv(0x04, &der_tlv(0x03, &[0x01, 0x86])),
        ]
        .concat();
        der_tlv(
            0xA3,
            &der_sequence(&[der_sequence(&bc), der_sequence(&ku)].concat()),
        )
    }

    fn build_leaf_extensions_der(san_uri: &str, san_dns: &str) -> Vec<u8> {
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
        // EKU (2.5.29.37): serverAuth (1.3.6.1.5.5.7.3.1), clientAuth (1.3.6.1.5.5.7.3.2)
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
        let san_entries = [
            der_tlv(0x86, san_uri.as_bytes()),
            der_tlv(0x82, san_dns.as_bytes()),
        ]
        .concat();
        let san = [
            der_oid(&[0x55, 0x1D, 0x11]),
            der_tlv(0x04, &der_sequence(&san_entries)),
        ]
        .concat();
        der_tlv(
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
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_x509_v3(
        ca_kp: &EcdsaKeyPair,
        serial: &[u8],
        issuer_dn: &[u8],
        subject_dn: &[u8],
        not_before: &str,
        not_after: &str,
        spki_der: &[u8],
        exts: &[u8],
    ) -> Vec<u8> {
        let sig_alg = der_sequence(&der_oid(OID_ECDSA_WITH_SHA256));
        let validity = der_sequence(
            &[
                der_tlv(0x17, not_before.as_bytes()),
                der_tlv(0x17, not_after.as_bytes()),
            ]
            .concat(),
        );
        let mut tbs_body = Vec::new();
        tbs_body.extend_from_slice(&der_tlv(0xA0, &der_tlv(0x02, &[0x02]))); // v3
        tbs_body.extend_from_slice(&der_tlv(0x02, serial));
        tbs_body.extend_from_slice(&sig_alg);
        tbs_body.extend_from_slice(issuer_dn);
        tbs_body.extend_from_slice(&validity);
        tbs_body.extend_from_slice(subject_dn);
        tbs_body.extend_from_slice(spki_der);
        tbs_body.extend_from_slice(exts);
        let tbs = der_sequence(&tbs_body);
        let sig = ca_kp.sign(&SystemRandom::new(), &tbs).unwrap();
        let mut cert_body = Vec::new();
        cert_body.extend_from_slice(&tbs);
        cert_body.extend_from_slice(&sig_alg);
        cert_body.extend_from_slice(&der_bit_string(sig.as_ref()));
        der_sequence(&cert_body)
    }

    #[test]
    fn key_persistence_and_pkcs10_csr_matches_backend_contract() {
        let dir = temp_test_dir("csr");
        let mgr1 = CertManager::load_or_init(dir.clone()).expect("init cert manager");
        let pub1 = mgr1.device_pubkey_hex();
        assert_eq!(pub1.len(), 64);
        assert!(pub1.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));

        // 再次加载应复用同一份磁盘密钥
        let mgr2 = CertManager::load_or_init(dir.clone()).expect("reload cert manager");
        assert_eq!(mgr2.device_pubkey_hex(), pub1);
        assert_eq!(mgr2.tls_key_pem().unwrap(), mgr1.tls_key_pem().unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ed_mode = fs::metadata(dir.join(ED25519_KEY_FILENAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            let tls_mode = fs::metadata(dir.join(TLS_KEY_FILENAME))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(ed_mode, 0o600);
            assert_eq!(tls_mode, 0o600);
        }

        // 生成 PKCS#10 CSR 并逐字段校验 DER 结构与 ECDSA-P256-SHA256 自签名
        let csr_pem = mgr2
            .generate_csr_pem(Some("rc-device-604928235"))
            .expect("generate CSR");
        assert!(csr_pem.starts_with("-----BEGIN CERTIFICATE REQUEST-----\n"));
        let item = rustls_pemfile::read_one(&mut BufReader::new(csr_pem.as_bytes()))
            .unwrap()
            .unwrap();
        let csr_der = match item {
            rustls_pemfile::Item::Csr(der) => der.to_vec(),
            other => panic!("expected CSR item, got {other:?}"),
        };

        let outer = read_der_tlv(&csr_der).unwrap();
        assert_eq!(outer.tag, 0x30);
        assert!(outer.rest.is_empty());

        let cri = read_der_tlv(outer.body).unwrap();
        assert_eq!(cri.tag, 0x30);
        let alg = read_der_tlv(cri.rest).unwrap();
        assert_eq!(alg.tag, 0x30);
        let oid = read_der_tlv(alg.body).unwrap();
        assert_eq!(oid.tag, 0x06);
        assert_eq!(oid.body, OID_ECDSA_WITH_SHA256);

        let sig = read_der_tlv(alg.rest).unwrap();
        assert_eq!(sig.tag, 0x03);
        assert!(sig.rest.is_empty());
        assert_eq!(sig.body[0], 0x00);
        let sig_bytes = &sig.body[1..];

        // 解析 CertificationRequestInfo 内的 SPKI 并验证自签名
        let ver = read_der_tlv(cri.body).unwrap();
        assert_eq!(ver.body, &[0x00]);
        let subj = read_der_tlv(ver.rest).unwrap();
        let spki = read_der_tlv(subj.rest).unwrap();
        assert_eq!(spki.tag, 0x30);
        assert_eq!(spki.rest, &[0xA0, 0x00]);

        let spki_alg = read_der_tlv(spki.body).unwrap();
        let ec_oid = read_der_tlv(spki_alg.body).unwrap();
        let curve_oid = read_der_tlv(ec_oid.rest).unwrap();
        assert_eq!(ec_oid.body, OID_EC_PUBLIC_KEY);
        assert_eq!(curve_oid.body, OID_PRIME256V1);
        assert!(curve_oid.rest.is_empty());

        let pk = read_der_tlv(spki_alg.rest).unwrap();
        assert_eq!(pk.tag, 0x03);
        assert!(pk.rest.is_empty());
        assert_eq!(pk.body[0], 0x00);
        let raw_pub = &pk.body[1..];
        assert_eq!(raw_pub.len(), 65);
        assert_eq!(raw_pub[0], 0x04);

        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, raw_pub)
            .verify(cri.full, sig_bytes)
            .expect("CSR self-signature must verify");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cert_persistence_renewal_clear_and_mtls_config_work() {
        let dir = temp_test_dir("mtls");
        let mgr = CertManager::load_or_init(dir.clone()).unwrap();
        assert!(mgr.needs_renewal(1_700_000_000));

        // 提取本地 TLS 公钥构造测试用 CA 签发叶子证书
        let tls_pem = mgr.tls_key_pem().unwrap();
        let pk8 = match rustls_pemfile::private_key(&mut BufReader::new(tls_pem.as_bytes()))
            .unwrap()
            .unwrap()
        {
            rustls::pki_types::PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der().to_vec(),
            _ => unreachable!(),
        };
        let tls_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pk8, &SystemRandom::new())
                .unwrap();
        let spki_der = build_ec_spki_der(tls_kp.public_key().as_ref());
        let uid = "550e8400-e29b-41d4-a716-446655440000";
        let did = "604928235";
        let (ca_pem, leaf_pem) = test_issue_ca_and_leaf_pem(&spki_der, uid, did);

        let now = 1_700_000_000u64;
        let meta = DeviceCertMetadata {
            serial_hex: "0210".to_string(),
            user_id: uid.to_string(),
            device_id: did.to_string(),
            expires_at_unix_sec: now + 7 * 86_400,
            fingerprint_sha256: String::new(),
            issued_at_unix_sec: now,
        };
        mgr.save_device_cert(&leaf_pem, &meta).unwrap();

        assert!(!mgr.needs_renewal(now));
        assert!(!mgr.needs_renewal_for(now, uid, did));
        // 剩余 < 48h 触发续签
        assert!(mgr.needs_renewal(now + 7 * 86_400 - CERT_RENEWAL_WINDOW_SECS));
        assert!(mgr.needs_renewal(now + 7 * 86_400 - 3600));
        // 用户或设备切换触发续签
        assert!(mgr.needs_renewal_for(now, "11111111-1111-1111-1111-111111111111", did));
        assert!(mgr.needs_renewal_for(now, uid, "999888777"));

        let mtls = mgr
            .build_mtls_config(
                &ca_pem,
                Some("987 654 321".to_string()),
                Some(uid.to_string()),
            )
            .expect("build MtlsConfig");
        assert_eq!(mtls.ca_certs.len(), 1);
        assert_eq!(mtls.expected_peer_device_id.as_deref(), Some("987654321"));
        assert_eq!(mtls.expected_peer_user_id.as_deref(), Some(uid));
        assert_eq!(mtls.identity.cert_chain().len(), 1);

        // 登出清理
        mgr.clear_device_cert().unwrap();
        assert!(mgr.load_device_cert().is_none());
        assert!(mgr.needs_renewal(now));
        assert!(mgr.build_mtls_config(&ca_pem, None, None).is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn owner_ticket_signer_and_verifier_roundtrip_and_reject_tamper() {
        let dir = temp_test_dir("owner");
        let mgr = CertManager::load_or_init(dir.clone()).unwrap();
        let caller_pk8 = fs::read(dir.join(ED25519_KEY_FILENAME)).unwrap();
        let caller_pub_hex = mgr.device_pubkey_hex();

        let srv_pk8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let srv_kp = Ed25519KeyPair::from_pkcs8(srv_pk8.as_ref()).unwrap();
        let srv_pub_hex = encode_hex_lower(srv_kp.public_key().as_ref());

        let uid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let caller_did = "111222333".to_string();
        let target_did = "444555666".to_string();
        let now = 1_750_000_000u64;

        let mut grant = OwnerGrantPayload {
            ticket_id: "ab".repeat(16),
            user_id: uid.clone(),
            caller_device_id: caller_did.clone(),
            caller_pubkey_hex: caller_pub_hex,
            target_device_id: target_did.clone(),
            session_id: "sig-sess-001".to_string(),
            expires_at_unix_sec: now + 60,
            signature_hex: String::new(),
        };
        let msg = owner_grant_signing_message(&grant).unwrap();
        assert!(msg.starts_with(OWNER_GRANT_DOMAIN));
        grant.signature_hex = encode_hex_lower(srv_kp.sign(&msg).as_ref());

        let signer = RcOwnerTicketSigner::new(grant.clone(), caller_pk8).unwrap();
        let verifier =
            RcOwnerTicketVerifier::new(&srv_pub_hex, uid.clone(), target_did.clone()).unwrap();

        let payload = signer.ticket_payload();
        let verified = verifier.verify_ticket(&payload, now).unwrap();
        assert_eq!(verified.ticket_id, grant.ticket_id);
        assert_eq!(verified.user_id, uid);
        assert_eq!(verified.caller_device_id, caller_did);
        assert_eq!(verified.target_device_id, target_did);

        let salt = [0x42u8; AUTH_SALT_LEN];
        let hello_sid = 987_654_321u64;
        let pop_sig = signer.sign_challenge(hello_sid, &salt).unwrap();
        assert_eq!(pop_sig.len(), 64);
        verifier
            .verify_challenge_signature(&verified, hello_sid, &salt, &pop_sig)
            .expect("valid PoP signature must verify");

        // 过期票据必须被拒
        assert!(verifier.verify_ticket(&payload, now + 60).is_err());

        // 篡改票据必须被拒
        let mut tampered_grant = grant.clone();
        tampered_grant.session_id = "evil-sess".to_string();
        let tampered_bytes = serde_json::to_vec(&tampered_grant).unwrap();
        assert!(verifier.verify_ticket(&tampered_bytes, now).is_err());

        // 错 target_device_id 必须被拒
        let wrong_target_verifier =
            RcOwnerTicketVerifier::new(&srv_pub_hex, uid.clone(), "999888777".to_string()).unwrap();
        assert!(wrong_target_verifier.verify_ticket(&payload, now).is_err());

        // 错 user_id 必须被拒
        let wrong_user_verifier = RcOwnerTicketVerifier::new(
            &srv_pub_hex,
            "00000000-0000-0000-0000-000000000000".to_string(),
            target_did.clone(),
        )
        .unwrap();
        assert!(wrong_user_verifier.verify_ticket(&payload, now).is_err());

        // 错 hello_session_id / 错 salt / 篡改 PoP 签名必须被拒
        assert!(verifier
            .verify_challenge_signature(&verified, hello_sid + 1, &salt, &pop_sig)
            .is_err());
        let mut bad_salt = salt;
        bad_salt[0] ^= 0xFF;
        assert!(verifier
            .verify_challenge_signature(&verified, hello_sid, &bad_salt, &pop_sig)
            .is_err());
        let mut bad_sig = pop_sig.clone();
        bad_sig[0] ^= 0xFF;
        assert!(verifier
            .verify_challenge_signature(&verified, hello_sid, &salt, &bad_sig)
            .is_err());

        // 额外验证规范 PoP 消息格式
        let pop_msg = owner_pop_signing_message(hello_sid, &salt, &target_did);
        assert!(pop_msg.starts_with(OWNER_POP_DOMAIN));
        let caller_pub_bytes = decode_hex(&verified.caller_pubkey_hex).unwrap();
        UnparsedPublicKey::new(&ED25519, &caller_pub_bytes)
            .verify(&pop_msg, &pop_sig)
            .unwrap();

        let _ = fs::remove_dir_all(&dir);
    }
}
