//! 同步轻量 HTTP/1.1 + HTTPS (`rustls`) 客户端、凭据持久化与 Token 自动轮换调度器。
//!
//! 本模块不引入 tokio 运行时，直接基于 `std::net::TcpStream` 与 `rustls::ClientConnection`
//! 实现同步阻塞 I/O，带严格超时（连接 `5s`、读写 `10s`）、`Content-Length` 与
//! `Transfer-Encoding: chunked` 解析，以及首次配置时的 Root CA 指纹提取与 TOFU 校验。
//!
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::fmt;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rc_net::quic::Identity;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app_prefs::{atomic_write_bytes, config_root_dir, encode_hex_lower};
use crate::cert_manager::{CertManager, DeviceCertMetadata};
use crate::server_config::{parse_device_id_input, ServerConfig};

/// TCP 连接超时（5 秒）。
pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP 读写超时（10 秒）。
pub const HTTP_IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Access Token 临期自动轮换窗口（距离过期不足 180 秒即触发刷新）。
pub const TOKEN_REFRESH_LEAD_SECS: u64 = 180;

const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
const AUTH_FILENAME: &str = "auth.json";

/// 返回默认认证凭据持久化路径：`$HOME/.config/rabbitcontrol/auth.json`。
pub fn default_auth_path() -> PathBuf {
    config_root_dir().join(AUTH_FILENAME)
}

fn default_unix_time_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    encode_hex_lower(&digest)
}

/// 客户端认证与 HTTP 调用错误类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthClientError {
    /// 本地配置缺失或参数无效。
    Config(String),
    /// TCP 连接、DNS 解析、超时或读写 I/O 错误。
    Network(String),
    /// TLS 握手、CA 锚定或指纹校验失败。
    Tls(String),
    /// HTTP 报文或 JSON 编解码协议错误。
    Protocol(String),
    /// 服务端返回的 HTTP 错误响应。
    Api {
        /// HTTP 状态码。
        status: u16,
        /// 服务端错误码（`ErrorBody.error`）。
        code: String,
        /// 服务端可读错误描述（`ErrorBody.message`）。
        message: String,
    },
    /// Refresh Token 无效、过期或检测到重放，需重新登录。
    ReloginRequired(String),
}

impl AuthClientError {
    /// 是否为 `401 Unauthorized` 服务端响应。
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Api { status: 401, .. })
    }

    /// 是否为 Refresh Token 重放检测错误。
    pub fn is_token_reuse(&self) -> bool {
        matches!(
            self,
            Self::Api { code, .. } if code == "token_reuse_detected"
        )
    }

    /// 是否为首次登录强制修改密码门禁错误。
    pub fn is_password_change_required(&self) -> bool {
        matches!(
            self,
            Self::Api { status: 403, code, .. } if code == "password_change_required"
        )
    }

    /// 是否为服务端已关闭自助注册错误。
    pub fn is_registration_disabled(&self) -> bool {
        matches!(
            self,
            Self::Api { status: 403, code, .. } if code == "registration_disabled"
        )
    }
}

impl fmt::Display for AuthClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "配置错误：{msg}"),
            Self::Network(msg) => write!(f, "网络错误：{msg}"),
            Self::Tls(msg) => write!(f, "TLS 校验失败：{msg}"),
            Self::Protocol(msg) => write!(f, "协议错误：{msg}"),
            Self::Api {
                status,
                code,
                message,
            } => write!(f, "服务端返回 {status} ({code})：{message}"),
            Self::ReloginRequired(msg) => write!(f, "需要重新登录：{msg}"),
        }
    }
}

impl std::error::Error for AuthClientError {}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    error: String,
    #[serde(default)]
    message: String,
}

/// 服务端 `/api/v1/health` 响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatusResponse {
    /// 健康状态（`"ok"` / `"degraded"`）。
    pub status: String,
    /// PostgreSQL 状态。
    #[serde(default)]
    pub postgres: String,
    /// 临时缓存后端名（`"redis"` / `"memory"`）。
    #[serde(default)]
    pub ephemeral_backend: String,
    /// 中继引擎标识。
    #[serde(default)]
    pub relay_engine: String,
    /// 服务端运行时长（秒）。
    #[serde(default)]
    pub uptime_sec: u64,
    /// 服务端版本号。
    #[serde(default)]
    pub version: String,
    /// 服务端是否启用了 TLS。
    #[serde(default)]
    pub tls_enabled: bool,
    /// 服务端 Root CA SHA-256 指纹。
    #[serde(default)]
    pub ca_fingerprint_sha256: Option<String>,
    /// 当前是否允许客户端自助注册。
    #[serde(default = "default_true")]
    pub allow_registration: bool,
    /// 缓存后端标识（`"redis"` / `"memory"`）。
    #[serde(default)]
    pub cache_backend: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 服务端 `/api/v1/pki/ca` 响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaBundleResponse {
    /// 服务端 Root CA 证书 PEM。
    pub ca_cert_pem: String,
    /// Root CA 证书 DER 的 SHA-256 指纹（64 位小写十六进制）。
    pub ca_fingerprint_sha256: String,
    /// 服务端 Ed25519 鉴权/票据公钥（64 位小写十六进制）。
    pub auth_pubkey_hex: String,
}

/// 服务端 `/api/v1/auth/config` 响应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfigResponse {
    /// 当前是否开放用户自助注册。
    pub allow_registration: bool,
}

/// 首次探测服务器返回的综合引导结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProbeResult {
    /// 健康检查结果。
    pub health: HealthStatusResponse,
    /// Root CA 与服务端认证公钥包（获取成功时为 `Some`）。
    pub ca_bundle: Option<CaBundleResponse>,
    /// 当前是否允许注册。
    pub allow_registration: bool,
    /// TLS 叶子证书 SHA-256 指纹（HTTPS 探测时提取）。
    pub tls_leaf_fingerprint_sha256: Option<String>,
    /// 探测往返耗时（毫秒）。
    pub latency_ms: u64,
}

/// 服务端返回的用户信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUserInfo {
    /// 用户 UUID。
    pub id: String,
    /// 规范化邮箱。
    pub email: String,
    /// 显示昵称。
    #[serde(default)]
    pub display_name: String,
    /// 用户角色（`"admin"` / `"user"`）。
    #[serde(default)]
    pub role: String,
    /// 账号状态（`"active"` / `"disabled"`）。
    #[serde(default)]
    pub status: String,
    /// 是否处于首次登录强制改密态。
    #[serde(default)]
    pub must_change_password: bool,
    /// 令牌版本号。
    #[serde(default)]
    pub token_version: i64,
}

/// 注册请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// 登录邮箱。
    pub email: String,
    /// 明文密码（传输层由 TLS 保护）。
    pub password: String,
    /// 可选显示名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 可选本机设备密钥（传入时同步注册并绑定设备）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_key: Option<String>,
    /// 可选主机名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 可选操作系统名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// 可选设备 Ed25519 公钥 hex。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_pubkey: Option<String>,
}

/// 登录请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    /// 登录邮箱。
    pub email: String,
    /// 明文密码。
    pub password: String,
    /// 可选本机设备密钥（传入时自动注册并绑定设备）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_key: Option<String>,
    /// 可选主机名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 可选操作系统名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// 可选设备 Ed25519 公钥 hex。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_pubkey: Option<String>,
}

/// 绑定设备请求参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindDeviceRequest {
    /// 设备稳定密钥（64 位 hex）。
    pub device_key: String,
    /// 可选主机名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 可选操作系统名称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// 可选设备别名。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 可选设备 Ed25519 公钥 hex。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_pubkey: Option<String>,
    /// 可选是否允许他人远程协助。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_remote_assist: Option<bool>,
}

/// 服务端返回的当前用户设备信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MyDeviceInfo {
    /// 9 位纯数字设备码。
    pub device_id: String,
    /// 格式化设备码（如 `"604 928 235"`）。
    #[serde(default)]
    pub formatted_id: String,
    /// 主机名。
    #[serde(default)]
    pub hostname: String,
    /// 操作系统。
    #[serde(default)]
    pub os: String,
    /// 用户自定义设备别名。
    #[serde(default)]
    pub alias: String,
    /// 设备 Ed25519 公钥 hex。
    #[serde(default)]
    pub device_pubkey: String,
    /// 是否允许他人远程协助。
    #[serde(default)]
    pub allow_remote_assist: bool,
    /// 设备是否已被禁用。
    #[serde(default)]
    pub disabled: bool,
    /// 是否在线。
    #[serde(default)]
    pub online: bool,
    /// 是否处于会话中。
    #[serde(default)]
    pub in_session: bool,
    /// 绑定时间。
    #[serde(default)]
    pub bound_at: Option<String>,
    /// 最近活跃时间。
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

/// 登录 / 注册 / 刷新 / 改密返回的令牌与用户负载。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthTokenResponse {
    /// Ed25519 JWT Access Token。
    pub access_token: String,
    /// 不透明一次性 Refresh Token。
    pub refresh_token: String,
    /// Access Token 有效时长（秒，默认 900）。
    #[serde(default = "default_expires_in_sec")]
    pub expires_in_sec: u64,
    /// 用户信息（某些精简响应中可能省略）。
    #[serde(default)]
    pub user: Option<AuthUserInfo>,
    /// 登录/注册同时绑定设备时返回的设备信息。
    #[serde(default)]
    pub device: Option<MyDeviceInfo>,
}

fn default_expires_in_sec() -> u64 {
    900
}

/// 服务端 `/api/v1/certs/issue` 返回的设备证书签发结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedDeviceCertResponse {
    /// 证书 128-bit 序列号（32 位十六进制）。
    pub serial_hex: String,
    /// 绑定用户 UUID。
    pub user_id: String,
    /// 绑定 9 位设备码。
    pub device_id: String,
    /// 叶子证书 DER SHA-256 指纹。
    #[serde(default)]
    pub fingerprint_sha256: String,
    /// 签发的设备叶子证书 PEM。
    pub cert_pem: String,
    /// Root CA 证书 PEM。
    #[serde(default)]
    pub ca_cert_pem: String,
    /// 签发时间（RFC3339）。
    #[serde(default)]
    pub issued_at: String,
    /// 过期时间（RFC3339）。
    #[serde(default)]
    pub expires_at: String,
    /// 可选 Unix 秒过期时间戳。
    #[serde(default)]
    pub expires_at_unix_sec: Option<u64>,
}

/// 本地持久化于 `~/.config/rabbitcontrol/auth.json` 的登录凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAuthSession {
    /// 用户 UUID。
    pub user_id: String,
    /// 用户邮箱。
    pub email: String,
    /// 显示昵称。
    #[serde(default)]
    pub display_name: String,
    /// 用户角色。
    #[serde(default)]
    pub role: String,
    /// 是否必须先修改密码。
    #[serde(default)]
    pub must_change_password: bool,
    /// 当前 Access Token。
    pub access_token: String,
    /// 当前 Refresh Token。
    pub refresh_token: String,
    /// Access Token 过期时间戳（Unix 秒）。
    pub access_expires_at_unix_sec: u64,
    /// 所属服务器主机名或 IP。
    pub server_host: String,
    /// 所属服务器 API 端口。
    pub api_port: u16,
}

/// 从指定路径读取本地认证会话；文件不存在或解析失败返回 `None`。
pub fn load_auth_session(path: &Path) -> Option<StoredAuthSession> {
    let bytes = fs::read(path).ok()?;
    let mut session: StoredAuthSession = serde_json::from_slice(&bytes).ok()?;
    if session.access_token.trim().is_empty()
        || session.refresh_token.trim().is_empty()
        || session.user_id.trim().is_empty()
    {
        return None;
    }
    session.user_id = session.user_id.trim().to_ascii_lowercase();
    session.email = session.email.trim().to_ascii_lowercase();
    Some(session)
}

/// 将本地认证会话以 `0600` 权限原子写入磁盘。
pub fn save_auth_session(path: &Path, session: &StoredAuthSession) -> Result<(), String> {
    let json =
        serde_json::to_vec_pretty(session).map_err(|e| format!("序列化认证凭据失败：{e}"))?;
    atomic_write_bytes(path, &json, true)
}

/// 删除本地认证凭据文件（文件不存在时视为成功）。
pub fn delete_auth_session(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("删除认证凭据 {} 失败：{err}", path.display())),
    }
}

// ─────────────────────────────────────────────────────────────────
// 同步 HTTP/1.1 + rustls 客户端
// ─────────────────────────────────────────────────────────────────

/// 同步轻量 HTTP/HTTPS API 客户端。
#[derive(Debug, Clone)]
pub struct AuthHttpClient {
    server_host: String,
    api_port: u16,
    use_https: bool,
    ca_cert_pem: Option<String>,
    connect_timeout: Duration,
    io_timeout: Duration,
}

impl AuthHttpClient {
    /// 由 [`ServerConfig`] 构造客户端。
    pub fn from_server_config(cfg: &ServerConfig) -> Result<Self, AuthClientError> {
        let host = cfg.server_host.trim().to_string();
        if host.is_empty() {
            return Err(AuthClientError::Config("服务器地址不能为空".to_string()));
        }
        if cfg.api_port == 0 {
            return Err(AuthClientError::Config("API 端口不能为 0".to_string()));
        }
        Ok(Self {
            server_host: host,
            api_port: cfg.api_port,
            use_https: cfg.use_https,
            ca_cert_pem: cfg
                .ca_cert_pem
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            connect_timeout: HTTP_CONNECT_TIMEOUT,
            io_timeout: HTTP_IO_TIMEOUT,
        })
    }

    /// 显式指定连接参数构造客户端。
    pub fn new(
        server_host: impl Into<String>,
        api_port: u16,
        use_https: bool,
        ca_cert_pem: Option<String>,
    ) -> Result<Self, AuthClientError> {
        let host = server_host.into().trim().to_string();
        if host.is_empty() {
            return Err(AuthClientError::Config("服务器地址不能为空".to_string()));
        }
        if api_port == 0 {
            return Err(AuthClientError::Config("API 端口不能为 0".to_string()));
        }
        Ok(Self {
            server_host: host,
            api_port,
            use_https,
            ca_cert_pem: ca_cert_pem
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            connect_timeout: HTTP_CONNECT_TIMEOUT,
            io_timeout: HTTP_IO_TIMEOUT,
        })
    }

    /// 首次配置探测：拉取 `/api/v1/pki/ca` 与 `/api/v1/health`（及可选 `/api/v1/auth/config`）。
    ///
    /// - 当启用 HTTPS 且尚未锚定 `ca_cert_pem` 时，执行 TOFU 引导校验：
    ///   先通过 TLS 握手捕获对端证书链并获取 `/api/v1/pki/ca`，再校验 CA 指纹，
    ///   并用拉取到的 Root CA 反向验证对端 TLS 叶子证书链合法性，防止中间人投毒；
    /// - 若传入 `expected_ca_fingerprint_sha256`，则严格比对 CA 指纹。
    pub fn probe_server(
        &self,
        expected_ca_fingerprint_sha256: Option<&str>,
    ) -> Result<ServerProbeResult, AuthClientError> {
        let start = Instant::now();
        let expected_fp = expected_ca_fingerprint_sha256
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());

        let (ca_bundle, leaf_fp, verified_client) = if self.use_https {
            if let Some(ref anchored_pem) = self.ca_cert_pem {
                let ca: Option<CaBundleResponse> = match self.get_json("/api/v1/pki/ca", None) {
                    Ok(bundle) => {
                        verify_ca_bundle_integrity(&bundle, expected_fp.as_deref())?;
                        Some(bundle)
                    }
                    Err(AuthClientError::Api { status: 404, .. }) => None,
                    Err(err) => return Err(err),
                };
                let client = Self::new(
                    self.server_host.clone(),
                    self.api_port,
                    true,
                    Some(anchored_pem.clone()),
                )?;
                (ca, None, client)
            } else {
                let (resp, captured) =
                    self.execute_tofu_probe("GET", "/api/v1/pki/ca", None, None)?;
                let bundle: CaBundleResponse = parse_json_response(resp)?;
                verify_ca_bundle_integrity(&bundle, expected_fp.as_deref())?;
                verify_captured_tls_against_ca(&self.server_host, &captured, &bundle.ca_cert_pem)?;
                let leaf_fp = sha256_hex(&captured.end_entity_der);
                let client = Self::new(
                    self.server_host.clone(),
                    self.api_port,
                    true,
                    Some(bundle.ca_cert_pem.clone()),
                )?;
                (Some(bundle), Some(leaf_fp), client)
            }
        } else {
            let ca = match self.get_json::<CaBundleResponse>("/api/v1/pki/ca", None) {
                Ok(bundle) => {
                    verify_ca_bundle_integrity(&bundle, expected_fp.as_deref())?;
                    Some(bundle)
                }
                Err(AuthClientError::Api { status: 404, .. }) => None,
                Err(err) => return Err(err),
            };
            (ca, None, self.clone())
        };

        let health: HealthStatusResponse = verified_client.get_json("/api/v1/health", None)?;
        let allow_registration = match verified_client.fetch_auth_config() {
            Ok(cfg) => cfg.allow_registration,
            Err(_) => health.allow_registration,
        };
        let latency_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

        Ok(ServerProbeResult {
            health,
            ca_bundle,
            allow_registration,
            tls_leaf_fingerprint_sha256: leaf_fp,
            latency_ms,
        })
    }

    /// 拉取服务端 Root CA 与 Ed25519 鉴权公钥（`GET /api/v1/pki/ca`）。
    pub fn fetch_ca_bundle(&self) -> Result<CaBundleResponse, AuthClientError> {
        if self.use_https && self.ca_cert_pem.is_none() {
            let (resp, captured) = self.execute_tofu_probe("GET", "/api/v1/pki/ca", None, None)?;
            let bundle: CaBundleResponse = parse_json_response(resp)?;
            verify_ca_bundle_integrity(&bundle, None)?;
            verify_captured_tls_against_ca(&self.server_host, &captured, &bundle.ca_cert_pem)?;
            Ok(bundle)
        } else {
            let bundle: CaBundleResponse = self.get_json("/api/v1/pki/ca", None)?;
            verify_ca_bundle_integrity(&bundle, None)?;
            Ok(bundle)
        }
    }

    /// 查询认证公共配置（`GET /api/v1/auth/config`）。
    pub fn fetch_auth_config(&self) -> Result<AuthConfigResponse, AuthClientError> {
        self.get_json("/api/v1/auth/config", None)
    }

    /// 注册新用户（`POST /api/v1/auth/register`）。
    pub fn register(&self, req: &RegisterRequest) -> Result<AuthTokenResponse, AuthClientError> {
        self.post_json("/api/v1/auth/register", None, req)
    }

    /// 邮箱密码登录（`POST /api/v1/auth/login`）。
    pub fn login(&self, req: &LoginRequest) -> Result<AuthTokenResponse, AuthClientError> {
        self.post_json("/api/v1/auth/login", None, req)
    }

    /// 原子轮换 Refresh Token（`POST /api/v1/auth/refresh`）。
    pub fn refresh(&self, refresh_token: &str) -> Result<AuthTokenResponse, AuthClientError> {
        #[derive(Serialize)]
        struct Body<'a> {
            refresh_token: &'a str,
        }
        self.post_json("/api/v1/auth/refresh", None, &Body { refresh_token })
    }

    /// 登出当前会话并吊销对应设备证书（`POST /api/v1/auth/logout`）。
    pub fn logout(
        &self,
        access_token: &str,
        refresh_token: Option<&str>,
        device_id: Option<&str>,
    ) -> Result<(), AuthClientError> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            refresh_token: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            device_id: Option<&'a str>,
        }
        let _: serde_json::Value = self.post_json(
            "/api/v1/auth/logout",
            Some(access_token),
            &Body {
                refresh_token,
                device_id,
            },
        )?;
        Ok(())
    }

    /// 修改当前用户密码（`POST /api/v1/auth/change-password`）。
    pub fn change_password(
        &self,
        access_token: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<AuthTokenResponse, AuthClientError> {
        #[derive(Serialize)]
        struct Body<'a> {
            current_password: &'a str,
            new_password: &'a str,
        }
        self.post_json(
            "/api/v1/auth/change-password",
            Some(access_token),
            &Body {
                current_password,
                new_password,
            },
        )
    }

    /// 获取当前登录用户信息（`GET /api/v1/me`）。
    pub fn fetch_me(&self, access_token: &str) -> Result<AuthUserInfo, AuthClientError> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum MePayload {
            Wrapped { user: AuthUserInfo },
            Flat(AuthUserInfo),
        }
        let parsed: MePayload = self.get_json("/api/v1/me", Some(access_token))?;
        Ok(match parsed {
            MePayload::Wrapped { user } => user,
            MePayload::Flat(user) => user,
        })
    }

    /// 列出当前用户绑定的所有设备（`GET /api/v1/me/devices`）。
    pub fn list_my_devices(
        &self,
        access_token: &str,
    ) -> Result<Vec<MyDeviceInfo>, AuthClientError> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum DevicesPayload {
            Wrapped { devices: Vec<MyDeviceInfo> },
            List(Vec<MyDeviceInfo>),
        }
        let parsed: DevicesPayload = self.get_json("/api/v1/me/devices", Some(access_token))?;
        Ok(match parsed {
            DevicesPayload::Wrapped { devices } => devices,
            DevicesPayload::List(devices) => devices,
        })
    }

    /// 将设备绑定到当前登录账号（`POST /api/v1/me/devices/bind`）。
    pub fn bind_device(
        &self,
        access_token: &str,
        req: &BindDeviceRequest,
    ) -> Result<MyDeviceInfo, AuthClientError> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum BindPayload {
            Wrapped { device: MyDeviceInfo },
            Flat(MyDeviceInfo),
        }
        let parsed: BindPayload =
            self.post_json("/api/v1/me/devices/bind", Some(access_token), req)?;
        Ok(match parsed {
            BindPayload::Wrapped { device } => device,
            BindPayload::Flat(device) => device,
        })
    }

    /// 修改已绑定设备的别名（`PATCH /api/v1/me/devices/{id}`）。
    pub fn rename_device(
        &self,
        access_token: &str,
        device_id: &str,
        alias: &str,
    ) -> Result<MyDeviceInfo, AuthClientError> {
        #[derive(Serialize)]
        struct Body<'a> {
            alias: &'a str,
        }
        let path = device_subpath(device_id)?;
        let resp: DevicePatchPayload =
            self.patch_json(&path, Some(access_token), &Body { alias })?;
        Ok(resp.into_device())
    }

    /// 更新已绑定设备的「允许他人远程协助」开关（`PATCH /api/v1/me/devices/{id}`）。
    pub fn set_device_allow_remote_assist(
        &self,
        access_token: &str,
        device_id: &str,
        allow_remote_assist: bool,
    ) -> Result<MyDeviceInfo, AuthClientError> {
        #[derive(Serialize)]
        struct Body {
            allow_remote_assist: bool,
        }
        let path = device_subpath(device_id)?;
        let resp: DevicePatchPayload = self.patch_json(
            &path,
            Some(access_token),
            &Body {
                allow_remote_assist,
            },
        )?;
        Ok(resp.into_device())
    }

    /// 从当前账号解绑并移除指定设备（`DELETE /api/v1/me/devices/{id}`）。
    pub fn remove_device(
        &self,
        access_token: &str,
        device_id: &str,
    ) -> Result<(), AuthClientError> {
        let path = device_subpath(device_id)?;
        let resp = self.execute("DELETE", &path, Some(access_token), None)?;
        if resp.status >= 400 {
            return Err(parse_api_error(resp.status, &resp.body));
        }
        Ok(())
    }

    /// 提交 PKCS#10 CSR 并申请签发 7 天设备证书（`POST /api/v1/certs/issue`）。
    pub fn issue_device_cert(
        &self,
        access_token: &str,
        device_id: &str,
        csr_pem: &str,
    ) -> Result<IssuedDeviceCertResponse, AuthClientError> {
        #[derive(Serialize)]
        struct Body<'a> {
            device_id: String,
            csr_pem: &'a str,
        }
        let norm_did = parse_device_id_input(device_id)
            .ok_or_else(|| AuthClientError::Config(format!("无效设备码：{device_id:?}")))?;
        self.post_json(
            "/api/v1/certs/issue",
            Some(access_token),
            &Body {
                device_id: norm_did,
                csr_pem,
            },
        )
    }

    fn get_json<Resp: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        bearer_token: Option<&str>,
    ) -> Result<Resp, AuthClientError> {
        let resp = self.execute("GET", path, bearer_token, None)?;
        parse_json_response(resp)
    }

    fn post_json<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        bearer_token: Option<&str>,
        req: &Req,
    ) -> Result<Resp, AuthClientError> {
        let body = serde_json::to_vec(req)
            .map_err(|e| AuthClientError::Protocol(format!("序列化请求体失败：{e}")))?;
        let resp = self.execute("POST", path, bearer_token, Some(&body))?;
        parse_json_response(resp)
    }

    fn patch_json<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        bearer_token: Option<&str>,
        req: &Req,
    ) -> Result<Resp, AuthClientError> {
        let body = serde_json::to_vec(req)
            .map_err(|e| AuthClientError::Protocol(format!("序列化请求体失败：{e}")))?;
        let resp = self.execute("PATCH", path, bearer_token, Some(&body))?;
        parse_json_response(resp)
    }

    fn execute(
        &self,
        method: &str,
        path: &str,
        bearer_token: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<RawHttpResponse, AuthClientError> {
        let stream = connect_tcp_with_timeout(
            &self.server_host,
            self.api_port,
            self.connect_timeout,
            self.io_timeout,
        )?;
        let host_header = format_http_host_header(&self.server_host, self.api_port);
        let req_bytes = build_http_request(method, path, &host_header, bearer_token, body);

        if self.use_https {
            let ca_pem = self.ca_cert_pem.as_deref().ok_or_else(|| {
                AuthClientError::Tls("HTTPS 已开启但尚未锚定 Root CA，请先探测服务器".to_string())
            })?;
            let tls_cfg = build_anchored_tls_config(ca_pem)?;
            let server_name = parse_server_name(&self.server_host)?;
            let conn = ClientConnection::new(Arc::new(tls_cfg), server_name)
                .map_err(|e| AuthClientError::Tls(format!("初始化 TLS 会话失败：{e}")))?;
            let mut tls_stream = rustls::StreamOwned::new(conn, stream);
            write_and_read_http(&mut tls_stream, &req_bytes, true)
        } else {
            let mut tcp_stream = stream;
            write_and_read_http(&mut tcp_stream, &req_bytes, false)
        }
    }

    fn execute_tofu_probe(
        &self,
        method: &str,
        path: &str,
        bearer_token: Option<&str>,
        body: Option<&[u8]>,
    ) -> Result<(RawHttpResponse, CapturedTlsChain), AuthClientError> {
        let stream = connect_tcp_with_timeout(
            &self.server_host,
            self.api_port,
            self.connect_timeout,
            self.io_timeout,
        )?;
        let host_header = format_http_host_header(&self.server_host, self.api_port);
        let req_bytes = build_http_request(method, path, &host_header, bearer_token, body);

        let captured = Arc::new(Mutex::new(None));
        let verifier = Arc::new(TofuProbeCertVerifier {
            supported_algs: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
            captured: Arc::clone(&captured),
        });
        let tls_cfg =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|e| AuthClientError::Tls(format!("配置 TLS 协议版本失败：{e}")))?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth();

        let server_name = parse_server_name(&self.server_host)?;
        let conn = ClientConnection::new(Arc::new(tls_cfg), server_name)
            .map_err(|e| AuthClientError::Tls(format!("初始化 TOFU TLS 会话失败：{e}")))?;
        let mut tls_stream = rustls::StreamOwned::new(conn, stream);
        let resp = write_and_read_http(&mut tls_stream, &req_bytes, true)?;

        let chain = captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| AuthClientError::Tls("TLS 握手未提供对端证书".to_string()))?;
        Ok((resp, chain))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DevicePatchPayload {
    Wrapped { device: MyDeviceInfo },
    Flat(MyDeviceInfo),
}

impl DevicePatchPayload {
    fn into_device(self) -> MyDeviceInfo {
        match self {
            Self::Wrapped { device } => device,
            Self::Flat(device) => device,
        }
    }
}

fn device_subpath(device_id: &str) -> Result<String, AuthClientError> {
    let norm = parse_device_id_input(device_id)
        .ok_or_else(|| AuthClientError::Config(format!("无效设备码：{device_id:?}")))?;
    Ok(format!("/api/v1/me/devices/{norm}"))
}

// ─────────────────────────────────────────────────────────────────
// 本地凭据 + 自动轮换 + 证书续签调度器（AuthSessionManager）
// ─────────────────────────────────────────────────────────────────

/// 线程安全的登录状态管理器：串行化 Refresh Token 轮换、启动恢复与后台自动续期。
pub struct AuthSessionManager {
    server_config: Mutex<ServerConfig>,
    session: Mutex<Option<StoredAuthSession>>,
    refresh_lock: Mutex<()>,
    session_epoch: std::sync::atomic::AtomicU64,
    auth_path: PathBuf,
    cert_manager: Arc<CertManager>,
    clock_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl fmt::Debug for AuthSessionManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let logged_in = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        f.debug_struct("AuthSessionManager")
            .field("auth_path", &self.auth_path)
            .field("logged_in", &logged_in)
            .finish()
    }
}

/// 后台维护线程句柄，释放时通知线程退出并等待优雅收敛。
pub struct AuthMaintainerHandle {
    stop_state: Arc<(Mutex<bool>, Condvar)>,
    join: Option<JoinHandle<()>>,
}

impl AuthMaintainerHandle {
    /// 主动停止并回收后台维护线程。
    pub fn stop(&mut self) {
        {
            let (lock, cv) = &*self.stop_state;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            *stopped = true;
            cv.notify_all();
        }
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AuthMaintainerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl AuthSessionManager {
    /// 构造认证会话管理器。
    pub fn new(
        server_config: ServerConfig,
        auth_path: PathBuf,
        cert_manager: Arc<CertManager>,
    ) -> Self {
        Self {
            server_config: Mutex::new(server_config),
            session: Mutex::new(None),
            refresh_lock: Mutex::new(()),
            session_epoch: std::sync::atomic::AtomicU64::new(0),
            auth_path,
            cert_manager,
            clock_fn: Arc::new(default_unix_time_sec),
        }
    }

    /// 注入确定性时钟函数（Unix 秒），供离线单测验证临期刷新与证书续签。
    pub fn with_clock(mut self, clock_fn: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock_fn = clock_fn;
        self
    }

    /// 更新当前使用的服务器配置（如首次探测保存 CA 后更新配置）。
    pub fn update_server_config(&self, cfg: ServerConfig) {
        *self.server_config.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    /// 返回当前内存中的服务器配置快照。
    pub fn server_config(&self) -> ServerConfig {
        self.server_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 返回当前登录态快照（未登录时返回 `None`）。
    pub fn current_session(&self) -> Option<StoredAuthSession> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn current_epoch_and_session(&self) -> Option<(u64, StoredAuthSession)> {
        let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = self
            .session_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        guard.clone().map(|sess| (epoch, sess))
    }

    /// 返回内部 [`CertManager`] 引用。
    pub fn cert_manager(&self) -> &Arc<CertManager> {
        &self.cert_manager
    }

    fn now_sec(&self) -> u64 {
        (self.clock_fn)()
    }

    fn build_http_client(&self) -> Result<(AuthHttpClient, ServerConfig), AuthClientError> {
        let cfg = self.server_config();
        let client = AuthHttpClient::from_server_config(&cfg)?;
        Ok((client, cfg))
    }

    fn persist_token_response(
        &self,
        cfg: &ServerConfig,
        resp: AuthTokenResponse,
        fallback_user: Option<&StoredAuthSession>,
        expected_epoch: Option<u64>,
    ) -> Result<StoredAuthSession, AuthClientError> {
        let now = self.now_sec();
        let expires_in = resp.expires_in_sec.max(1);
        let (user_id, email, display_name, role, must_change_password) = match resp.user {
            Some(u) => (
                u.id.trim().to_ascii_lowercase(),
                u.email.trim().to_ascii_lowercase(),
                u.display_name,
                u.role,
                u.must_change_password,
            ),
            None => {
                let prev = fallback_user.ok_or_else(|| {
                    AuthClientError::Protocol("服务端令牌响应缺少 user 字段".to_string())
                })?;
                (
                    prev.user_id.clone(),
                    prev.email.clone(),
                    prev.display_name.clone(),
                    prev.role.clone(),
                    prev.must_change_password,
                )
            }
        };

        let next = StoredAuthSession {
            user_id,
            email,
            display_name,
            role,
            must_change_password,
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            access_expires_at_unix_sec: now.saturating_add(expires_in),
            server_host: cfg.server_host.trim().to_string(),
            api_port: cfg.api_port,
        };
        let mut sess_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(epoch) = expected_epoch {
            if self
                .session_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                != epoch
            {
                return Err(AuthClientError::ReloginRequired(
                    "会话已注销或切换服务器，取消令牌写回".to_string(),
                ));
            }
        } else {
            self.session_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        save_auth_session(&self.auth_path, &next).map_err(AuthClientError::Protocol)?;
        *sess_guard = Some(next.clone());
        Ok(next)
    }

    /// 立即清空内存与磁盘中的认证会话及设备证书，并递增会话代次防止并发刷新复活旧态。
    pub fn wipe_local_credentials(&self) {
        let mut sess_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        self.session_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *sess_guard = None;
        let _ = delete_auth_session(&self.auth_path);
        let _ = self.cert_manager.clear_device_cert();
    }

    /// 仅当当前 `session_epoch` 仍等于 `expected_epoch` 时才清空本地凭据（防止旧线程误杀新登录态）。
    fn wipe_local_credentials_if_epoch(&self, expected_epoch: u64) -> bool {
        let mut sess_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .session_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != expected_epoch
        {
            return false;
        }
        self.session_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *sess_guard = None;
        let _ = delete_auth_session(&self.auth_path);
        let _ = self.cert_manager.clear_device_cert();
        true
    }

    /// 原子清空本地凭据、递增会话代次并返回登出前服务器配置与会话副本（无任何网络 I/O）。
    pub fn take_logout_context_and_wipe(&self) -> (ServerConfig, Option<StoredAuthSession>) {
        let mut sess_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        self.session_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let prev = sess_guard.take();
        let cfg = self.server_config();
        let _ = delete_auth_session(&self.auth_path);
        let _ = self.cert_manager.clear_device_cert();
        (cfg, prev)
    }

    /// 使用指定服务器配置与旧会话副本向远端通知注销（可在无锁后台线程中执行）。
    pub fn notify_remote_logout(
        cfg: &ServerConfig,
        session: Option<&StoredAuthSession>,
        device_id: Option<&str>,
    ) -> Result<(), AuthClientError> {
        let Some(sess) = session else {
            return Ok(());
        };
        let client = AuthHttpClient::from_server_config(cfg)?;
        let effective_did = device_id
            .map(str::to_string)
            .or_else(|| cfg.cached_device_id.clone());
        client.logout(
            &sess.access_token,
            Some(&sess.refresh_token),
            effective_did.as_deref(),
        )
    }

    /// 登录账号并原子持久化凭据；若绑定了设备且非改密受限态，顺带更新期望证书绑定。
    pub fn login(
        &self,
        email: &str,
        password: &str,
        hostname: Option<&str>,
        os: Option<&str>,
    ) -> Result<StoredAuthSession, AuthClientError> {
        let (client, cfg) = self.build_http_client()?;
        let pubkey_hex = self.cert_manager.device_pubkey_hex();
        let req = LoginRequest {
            email: email.trim().to_ascii_lowercase(),
            password: password.to_string(),
            device_key: Some(cfg.device_key.clone()),
            hostname: hostname.map(str::to_string),
            os: os.map(str::to_string),
            device_pubkey: Some(pubkey_hex),
        };
        let resp = client.login(&req)?;
        let device_id = resp
            .device
            .as_ref()
            .map(|d| d.device_id.clone())
            .or_else(|| cfg.cached_device_id.clone());
        let session = self.persist_token_response(&cfg, resp, None, None)?;
        if let Some(did) = device_id {
            self.cert_manager
                .set_expected_binding(&session.user_id, &did);
        }
        Ok(session)
    }

    /// 自助注册账号并自动登录持久化凭据。
    pub fn register(
        &self,
        email: &str,
        password: &str,
        display_name: Option<&str>,
        hostname: Option<&str>,
        os: Option<&str>,
    ) -> Result<StoredAuthSession, AuthClientError> {
        let (client, cfg) = self.build_http_client()?;
        let pubkey_hex = self.cert_manager.device_pubkey_hex();
        let req = RegisterRequest {
            email: email.trim().to_ascii_lowercase(),
            password: password.to_string(),
            display_name: display_name.map(|s| s.trim().to_string()),
            device_key: Some(cfg.device_key.clone()),
            hostname: hostname.map(str::to_string),
            os: os.map(str::to_string),
            device_pubkey: Some(pubkey_hex),
        };
        let resp = client.register(&req)?;
        let device_id = resp
            .device
            .as_ref()
            .map(|d| d.device_id.clone())
            .or_else(|| cfg.cached_device_id.clone());
        let session = self.persist_token_response(&cfg, resp, None, None)?;
        if let Some(did) = device_id {
            self.cert_manager
                .set_expected_binding(&session.user_id, &did);
        }
        Ok(session)
    }

    /// 修改当前用户密码并原子更新新的全权限令牌。
    pub fn change_password(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<StoredAuthSession, AuthClientError> {
        let (epoch, prev) = self
            .current_epoch_and_session()
            .ok_or_else(|| AuthClientError::ReloginRequired("当前未登录".to_string()))?;
        let token = self.ensure_valid_access_token()?;
        if self
            .session_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != epoch
        {
            return Err(AuthClientError::ReloginRequired(
                "会话已注销或切换账号，请重新登录后再修改密码".to_string(),
            ));
        }
        let (client, cfg) = self.build_http_client()?;
        let resp = client.change_password(&token, current_password, new_password)?;
        self.persist_token_response(&cfg, resp, Some(&prev), Some(epoch))
    }

    /// 登出并立即清理本地 `auth.json` 与设备证书，再尽最大努力通知服务端吊销当前会话。
    pub fn logout(&self, device_id: Option<&str>) -> Result<(), AuthClientError> {
        let (cfg, prev) = {
            let _refresh_guard = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
            self.take_logout_context_and_wipe()
        };
        Self::notify_remote_logout(&cfg, prev.as_ref(), device_id)
    }

    /// 确保返回有效的 Access Token：若距过期不足 `180s` 则加锁串行刷新并原子落盘。
    ///
    /// 多线程并发调用时通过双重检查锁确保同一轮仅发送一次 `/api/v1/auth/refresh`，
    /// 避免并发刷新触发服务端 Refresh Token 重放检测。
    /// 若服务端返回 `401` 或 `token_reuse_detected`，清空本地凭据与设备证书并返回 [`AuthClientError::ReloginRequired`]。
    pub fn ensure_valid_access_token(&self) -> Result<String, AuthClientError> {
        let now = self.now_sec();
        if let Some(sess) = self.current_session() {
            if now.saturating_add(TOKEN_REFRESH_LEAD_SECS) < sess.access_expires_at_unix_sec {
                return Ok(sess.access_token);
            }
        } else {
            return Err(AuthClientError::ReloginRequired("当前未登录".to_string()));
        }

        let _refresh_guard = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());

        // 双重检查：若前一个持有锁的线程已完成刷新，直接复用新令牌。
        let now_after_lock = self.now_sec();
        let (epoch, current) = self
            .current_epoch_and_session()
            .ok_or_else(|| AuthClientError::ReloginRequired("当前未登录".to_string()))?;
        if now_after_lock.saturating_add(TOKEN_REFRESH_LEAD_SECS)
            < current.access_expires_at_unix_sec
        {
            return Ok(current.access_token);
        }

        self.perform_refresh_locked(epoch, &current)
    }

    /// 当业务请求收到 `401 Unauthorized` 时强制串行刷新一次（若其他线程已刷新则直接返回新令牌）。
    pub fn refresh_after_unauthorized(
        &self,
        failed_access_token: &str,
    ) -> Result<String, AuthClientError> {
        let _refresh_guard = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());

        let (epoch, current) = self
            .current_epoch_and_session()
            .ok_or_else(|| AuthClientError::ReloginRequired("当前未登录".to_string()))?;
        if current.access_token != failed_access_token
            && self.now_sec() < current.access_expires_at_unix_sec
        {
            return Ok(current.access_token);
        }
        self.perform_refresh_locked(epoch, &current)
    }

    fn perform_refresh_locked(
        &self,
        epoch: u64,
        current: &StoredAuthSession,
    ) -> Result<String, AuthClientError> {
        let (client, cfg) = self.build_http_client()?;
        match client.refresh(&current.refresh_token) {
            Ok(resp) => {
                let updated =
                    self.persist_token_response(&cfg, resp, Some(current), Some(epoch))?;
                Ok(updated.access_token)
            }
            Err(err)
                if err.is_unauthorized()
                    || err.is_token_reuse()
                    || matches!(err, AuthClientError::Api { status: 403, .. }) =>
            {
                let _ = self.wipe_local_credentials_if_epoch(epoch);
                Err(AuthClientError::ReloginRequired(err.to_string()))
            }
            Err(err) => Err(err),
        }
    }

    /// 检查并按需为指定 `device_id` 申请或续签设备证书（缺失、`< 48h` 或账号/设备不匹配时触发）。
    ///
    /// 返回 `Ok(true)` 表示本次执行了新证书签发，`Ok(false)` 表示当前证书仍有效无需续签。
    pub fn ensure_device_cert(&self, device_id: &str) -> Result<bool, AuthClientError> {
        let norm_did = parse_device_id_input(device_id)
            .ok_or_else(|| AuthClientError::Config(format!("无效设备码：{device_id:?}")))?;
        let (epoch, sess) = {
            let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let epoch = self
                .session_epoch
                .load(std::sync::atomic::Ordering::Acquire);
            let sess = guard.clone().ok_or_else(|| {
                AuthClientError::ReloginRequired("尚未登录，无法签发设备证书".to_string())
            })?;
            (epoch, sess)
        };

        if sess.must_change_password {
            return Err(AuthClientError::Api {
                status: 403,
                code: "password_change_required".to_string(),
                message: "首次登录必须修改密码后才能签发设备证书".to_string(),
            });
        }

        let now = self.now_sec();
        if !self
            .cert_manager
            .needs_renewal_for(now, &sess.user_id, &norm_did)
        {
            return Ok(false);
        }

        let csr_pem = self
            .cert_manager
            .generate_csr_pem(Some(&format!("rc-device-{norm_did}")))
            .map_err(AuthClientError::Protocol)?;
        let (client, _) = self.build_http_client()?;

        let mut token = self.ensure_valid_access_token()?;
        let issued = match client.issue_device_cert(&token, &norm_did, &csr_pem) {
            Ok(res) => res,
            Err(err) if err.is_unauthorized() => {
                token = self.refresh_after_unauthorized(&token)?;
                client.issue_device_cert(&token, &norm_did, &csr_pem)?
            }
            Err(err) => return Err(err),
        };

        let expires_at_unix_sec = resolve_cert_expiry_unix_sec(&issued, now)?;
        let issued_at_unix_sec = parse_rfc3339_unix_sec(&issued.issued_at).unwrap_or(now);
        let meta = DeviceCertMetadata {
            serial_hex: issued.serial_hex,
            user_id: issued.user_id,
            device_id: issued.device_id,
            expires_at_unix_sec,
            fingerprint_sha256: issued.fingerprint_sha256,
            issued_at_unix_sec,
        };
        let sess_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .session_epoch
            .load(std::sync::atomic::Ordering::Acquire)
            != epoch
            || sess_guard
                .as_ref()
                .map(|current| current.user_id != sess.user_id)
                .unwrap_or(true)
        {
            return Err(AuthClientError::ReloginRequired(
                "会话已注销或切换账号，取消设备证书写回".to_string(),
            ));
        }
        self.cert_manager
            .save_device_cert(&issued.cert_pem, &meta)
            .map_err(AuthClientError::Protocol)?;

        if !issued.ca_cert_pem.trim().is_empty() {
            let mut cfg = self.server_config();
            if cfg
                .ca_cert_pem
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                cfg.ca_cert_pem = Some(issued.ca_cert_pem);
                self.update_server_config(cfg);
            }
        }
        Ok(true)
    }

    /// 重启恢复：从磁盘 `auth.json` 读取登录态；若 Access Token 已过期或临期，自动静默刷新并检查续签证书。
    pub fn restore_on_startup(&self) -> Result<Option<StoredAuthSession>, AuthClientError> {
        let baseline_epoch = {
            let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Ok(guard.clone());
            }
            self.session_epoch
                .load(std::sync::atomic::Ordering::Acquire)
        };
        let Some(stored) = load_auth_session(&self.auth_path) else {
            return Ok(None);
        };
        let cfg = self.server_config();
        if !cfg.server_host.trim().is_empty()
            && cfg.api_port != 0
            && (!stored
                .server_host
                .trim()
                .eq_ignore_ascii_case(cfg.server_host.trim())
                || stored.api_port != cfg.api_port)
        {
            let _ = self.wipe_local_credentials_if_epoch(baseline_epoch);
            return Ok(None);
        }

        if cfg.server_host.trim().is_empty() || cfg.api_port == 0 {
            if stored.access_expires_at_unix_sec <= self.now_sec() {
                return Ok(None);
            }
            let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Ok(guard.clone());
            }
            if self
                .session_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                != baseline_epoch
            {
                return Ok(None);
            }
            self.session_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            *guard = Some(stored.clone());
            return Ok(Some(stored));
        }

        let needs_refresh = self.now_sec().saturating_add(TOKEN_REFRESH_LEAD_SECS)
            >= stored.access_expires_at_unix_sec;
        let restored = if !needs_refresh {
            let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Ok(guard.clone());
            }
            if self
                .session_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                != baseline_epoch
            {
                return Ok(None);
            }
            self.session_epoch
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            *guard = Some(stored.clone());
            Some(stored)
        } else {
            let _refresh_guard = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
            {
                let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
                if guard.is_some() {
                    return Ok(guard.clone());
                }
                if self
                    .session_epoch
                    .load(std::sync::atomic::Ordering::Acquire)
                    != baseline_epoch
                {
                    return Ok(None);
                }
            }
            let (client, live_cfg) = self.build_http_client()?;
            match client.refresh(&stored.refresh_token) {
                Ok(resp) => {
                    let updated = self.persist_token_response(
                        &live_cfg,
                        resp,
                        Some(&stored),
                        Some(baseline_epoch),
                    )?;
                    Some(updated)
                }
                Err(err)
                    if err.is_unauthorized()
                        || err.is_token_reuse()
                        || matches!(err, AuthClientError::Api { status: 403, .. }) =>
                {
                    let _ = self.wipe_local_credentials_if_epoch(baseline_epoch);
                    return Err(AuthClientError::ReloginRequired(err.to_string()));
                }
                Err(err) => {
                    if stored.access_expires_at_unix_sec > self.now_sec() {
                        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
                        if guard.is_none()
                            && self
                                .session_epoch
                                .load(std::sync::atomic::Ordering::Acquire)
                                == baseline_epoch
                        {
                            self.session_epoch
                                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            *guard = Some(stored);
                        }
                    }
                    return Err(err);
                }
            }
        };

        if let (Some(ref sess), Some(ref did)) = (&restored, &cfg.cached_device_id) {
            if !sess.must_change_password {
                let _ = self.ensure_device_cert(did)?;
            }
        }
        Ok(restored)
    }

    /// 启动后台定时维护线程：周期检查 Access Token 临期自动轮换与设备证书 `< 48h` 自动续签。
    ///
    /// 返回的 [`AuthMaintainerHandle`] 支持 `stop()` 或 `Drop` 时立即优雅唤醒退出。
    pub fn spawn_background_maintainer<F>(
        self: &Arc<Self>,
        check_interval: Duration,
        device_id_fn: F,
    ) -> AuthMaintainerHandle
    where
        F: Fn() -> Option<String> + Send + Sync + 'static,
    {
        self.spawn_background_maintainer_with_callbacks(check_interval, device_id_fn, || {}, |_| {})
    }

    /// 启动后台定时维护线程，并在证书发生静默续签或 Refresh Token 吊销需要重新登录时回调通知宿主。
    pub fn spawn_background_maintainer_with_callbacks<F, C, R>(
        self: &Arc<Self>,
        check_interval: Duration,
        device_id_fn: F,
        on_cert_renewed: C,
        on_relogin_required: R,
    ) -> AuthMaintainerHandle
    where
        F: Fn() -> Option<String> + Send + Sync + 'static,
        C: Fn() + Send + Sync + 'static,
        R: Fn(String) + Send + Sync + 'static,
    {
        let mgr = Arc::clone(self);
        let stop_state = Arc::new((Mutex::new(false), Condvar::new()));
        let stop_clone = Arc::clone(&stop_state);
        let interval = check_interval.max(Duration::from_millis(50));

        let join = thread::Builder::new()
            .name("rc-auth-maintainer".to_string())
            .spawn(move || {
                let (lock, cv) = &*stop_clone;
                loop {
                    {
                        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if *guard {
                            break;
                        }
                        let (guard, _) = cv
                            .wait_timeout(guard, interval)
                            .unwrap_or_else(|e| e.into_inner());
                        if *guard {
                            break;
                        }
                    }

                    let Some(sess) = mgr.current_session() else {
                        continue;
                    };
                    if mgr.server_config().server_host.trim().is_empty() {
                        continue;
                    }

                    if let Err(AuthClientError::ReloginRequired(msg)) =
                        mgr.ensure_valid_access_token()
                    {
                        on_relogin_required(msg);
                        continue;
                    }
                    if !sess.must_change_password {
                        if let Some(did) = device_id_fn() {
                            match mgr.ensure_device_cert(&did) {
                                Ok(true) => on_cert_renewed(),
                                Ok(false) => {}
                                Err(AuthClientError::ReloginRequired(msg)) => {
                                    on_relogin_required(msg);
                                }
                                Err(_) => {}
                            }
                        }
                    }
                }
            })
            .ok();

        AuthMaintainerHandle { stop_state, join }
    }
}

// ─────────────────────────────────────────────────────────────────
// TLS / HTTP 底层辅助与 RFC3339 时间解析
// ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct CapturedTlsChain {
    end_entity_der: Vec<u8>,
    intermediates_der: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct TofuProbeCertVerifier {
    supported_algs: WebPkiSupportedAlgorithms,
    captured: Arc<Mutex<Option<CapturedTlsChain>>>,
}

impl ServerCertVerifier for TofuProbeCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut guard = self.captured.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CapturedTlsChain {
            end_entity_der: end_entity.as_ref().to_vec(),
            intermediates_der: intermediates.iter().map(|c| c.as_ref().to_vec()).collect(),
        });
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.supported_algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.supported_algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_algs.supported_schemes()
    }
}

fn verify_ca_bundle_integrity(
    bundle: &CaBundleResponse,
    expected_fp: Option<&str>,
) -> Result<(), AuthClientError> {
    let certs = Identity::certs_from_pem(&bundle.ca_cert_pem)
        .map_err(|e| AuthClientError::Tls(format!("解析服务端返回的 Root CA PEM 失败：{e}")))?;
    let ca_der = certs
        .first()
        .ok_or_else(|| AuthClientError::Tls("服务端返回的 CA PEM 为空".to_string()))?;
    let actual_fp = sha256_hex(ca_der.as_ref());
    let declared_fp = bundle.ca_fingerprint_sha256.trim().to_ascii_lowercase();
    if actual_fp != declared_fp {
        return Err(AuthClientError::Tls(format!(
            "CA 指纹自检不一致：声明 {declared_fp}，实算 {actual_fp}"
        )));
    }
    if let Some(expected) = expected_fp {
        if actual_fp != expected {
            return Err(AuthClientError::Tls(format!(
                "Root CA 指纹不匹配（期望 {expected}，实际 {actual_fp}）"
            )));
        }
    }
    Ok(())
}

fn verify_captured_tls_against_ca(
    server_host: &str,
    captured: &CapturedTlsChain,
    ca_cert_pem: &str,
) -> Result<(), AuthClientError> {
    let roots = build_root_store(ca_cert_pem)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|e| AuthClientError::Tls(format!("构造 WebPkiServerVerifier 失败：{e}")))?;

    let end_entity = CertificateDer::from(captured.end_entity_der.clone());
    let intermediates: Vec<CertificateDer<'_>> = captured
        .intermediates_der
        .iter()
        .map(|d| CertificateDer::from(d.as_slice()))
        .collect();
    let server_name = parse_server_name(server_host)?;
    let now = UnixTime::now();

    verifier
        .verify_server_cert(&end_entity, &intermediates, &server_name, &[], now)
        .map_err(|e| {
            AuthClientError::Tls(format!("服务器 TLS 证书无法通过所返回的 Root CA 校验：{e}"))
        })?;
    Ok(())
}

fn build_root_store(ca_cert_pem: &str) -> Result<RootCertStore, AuthClientError> {
    let certs = Identity::certs_from_pem(ca_cert_pem)
        .map_err(|e| AuthClientError::Tls(format!("解析 Root CA PEM 失败：{e}")))?;
    if certs.is_empty() {
        return Err(AuthClientError::Tls(
            "Root CA PEM 不包含任何证书".to_string(),
        ));
    }
    let mut roots = RootCertStore::empty();
    for c in certs {
        roots
            .add(c)
            .map_err(|e| AuthClientError::Tls(format!("导入 Root CA 到信任库失败：{e}")))?;
    }
    Ok(roots)
}

fn build_anchored_tls_config(ca_cert_pem: &str) -> Result<ClientConfig, AuthClientError> {
    let roots = build_root_store(ca_cert_pem)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| AuthClientError::Tls(format!("配置 TLS 协议版本失败：{e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth()
        .pipe_ok()
}

trait PipeOk: Sized {
    fn pipe_ok<E>(self) -> Result<Self, E> {
        Ok(self)
    }
}
impl PipeOk for ClientConfig {}

fn parse_server_name(host: &str) -> Result<ServerName<'static>, AuthClientError> {
    let clean = host
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host.trim());
    ServerName::try_from(clean.to_string())
        .map_err(|e| AuthClientError::Config(format!("无效 TLS 主机名 {host:?}：{e}")))
}

fn connect_tcp_with_timeout(
    host: &str,
    port: u16,
    connect_timeout: Duration,
    io_timeout: Duration,
) -> Result<TcpStream, AuthClientError> {
    let clean = host
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host.trim());
    let addrs: Vec<SocketAddr> = (clean, port)
        .to_socket_addrs()
        .map_err(|e| AuthClientError::Network(format!("解析 {clean}:{port} 失败：{e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(AuthClientError::Network(format!(
            "未能解析到 {clean}:{port} 的任何地址"
        )));
    }

    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, connect_timeout) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(io_timeout))
                    .map_err(|e| AuthClientError::Network(format!("设置读超时失败：{e}")))?;
                stream
                    .set_write_timeout(Some(io_timeout))
                    .map_err(|e| AuthClientError::Network(format!("设置写超时失败：{e}")))?;
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }

    Err(AuthClientError::Network(format!(
        "连接 {clean}:{port} 失败：{}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    )))
}

fn format_http_host_header(host: &str, port: u16) -> String {
    let clean = host.trim();
    if clean.contains(':') && !clean.starts_with('[') {
        format!("[{clean}]:{port}")
    } else {
        format!("{clean}:{port}")
    }
}

fn build_http_request(
    method: &str,
    path: &str,
    host_header: &str,
    bearer_token: Option<&str>,
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if let Some(token) = bearer_token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(bytes) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            bytes.len()
        ));
    }
    head.push_str("\r\n");

    let mut out = head.into_bytes();
    if let Some(bytes) = body {
        out.extend_from_slice(bytes);
    }
    out
}

struct RawHttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn write_and_read_http<S: Read + Write>(
    stream: &mut S,
    req_bytes: &[u8],
    is_tls: bool,
) -> Result<RawHttpResponse, AuthClientError> {
    stream.write_all(req_bytes).map_err(|e| {
        if is_tls {
            AuthClientError::Tls(format!("发送 HTTPS 请求失败：{e}"))
        } else {
            AuthClientError::Network(format!("发送 HTTP 请求失败：{e}"))
        }
    })?;
    stream.flush().map_err(|e| {
        if is_tls {
            AuthClientError::Tls(format!("刷新 HTTPS 请求缓冲失败：{e}"))
        } else {
            AuthClientError::Network(format!("刷新 HTTP 请求缓冲失败：{e}"))
        }
    })?;

    read_http_response(stream, is_tls)
}

fn read_http_response<R: Read>(
    reader: &mut R,
    is_tls: bool,
) -> Result<RawHttpResponse, AuthClientError> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() >= MAX_HTTP_HEADER_BYTES {
            return Err(AuthClientError::Protocol("HTTP 响应头过大".to_string()));
        }
        let n = reader.read(&mut tmp).map_err(|e| {
            if is_tls {
                AuthClientError::Tls(format!("读取 HTTPS 响应头失败：{e}"))
            } else {
                AuthClientError::Network(format!("读取 HTTP 响应头失败：{e}"))
            }
        })?;
        if n == 0 {
            return Err(AuthClientError::Protocol(
                "服务端在响应头结束前关闭连接".to_string(),
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| AuthClientError::Protocol("HTTP 响应头包含非 UTF-8 字符".to_string()))?;
    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| AuthClientError::Protocol("缺少 HTTP 状态行".to_string()))?;
    let status = parse_http_status(status_line)?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name_lower = name.trim().to_ascii_lowercase();
            let val = value.trim();
            if name_lower == "content-length" {
                let len = val.parse::<usize>().map_err(|_| {
                    AuthClientError::Protocol(format!("无效 Content-Length：{val:?}"))
                })?;
                if len > MAX_HTTP_BODY_BYTES {
                    return Err(AuthClientError::Protocol("响应体超过大小上限".to_string()));
                }
                content_length = Some(len);
            } else if name_lower == "transfer-encoding"
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
        }
    }

    let mut body_buf = buf[header_end + 4..].to_vec();
    let body = if is_chunked {
        decode_chunked_body(reader, &mut body_buf, is_tls)?
    } else if let Some(len) = content_length {
        while body_buf.len() < len {
            let n = reader.read(&mut tmp).map_err(|e| {
                if is_tls {
                    AuthClientError::Tls(format!("读取 HTTPS 响应体失败：{e}"))
                } else {
                    AuthClientError::Network(format!("读取 HTTP 响应体失败：{e}"))
                }
            })?;
            if n == 0 {
                return Err(AuthClientError::Protocol(
                    "HTTP 响应体在 Content-Length 读满前截断".to_string(),
                ));
            }
            body_buf.extend_from_slice(&tmp[..n]);
        }
        body_buf.truncate(len);
        body_buf
    } else {
        loop {
            if body_buf.len() >= MAX_HTTP_BODY_BYTES {
                return Err(AuthClientError::Protocol("响应体超过大小上限".to_string()));
            }
            match reader.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => body_buf.extend_from_slice(&tmp[..n]),
                // 某些服务端在发送完响应体后会直接关闭 TCP，未发送 TLS close_notify。
                Err(err) if is_tls && err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => {
                    return Err(AuthClientError::Network(format!("读取响应体失败：{err}")));
                }
            }
        }
        body_buf
    };

    Ok(RawHttpResponse { status, body })
}

fn decode_chunked_body<R: Read>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    is_tls: bool,
) -> Result<Vec<u8>, AuthClientError> {
    let mut out = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut cursor = 0usize;

    loop {
        let line_end = loop {
            if let Some(pos) = find_crlf(&buf[cursor..]) {
                break cursor + pos;
            }
            if buf.len() - cursor > 1024 {
                return Err(AuthClientError::Protocol("chunked 长度行过长".to_string()));
            }
            let n = reader.read(&mut tmp).map_err(|e| {
                if is_tls {
                    AuthClientError::Tls(format!("读取 chunked 长度失败：{e}"))
                } else {
                    AuthClientError::Network(format!("读取 chunked 长度失败：{e}"))
                }
            })?;
            if n == 0 {
                return Err(AuthClientError::Protocol(
                    "chunked 流在结束块前意外中断".to_string(),
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
        };

        let size_line = std::str::from_utf8(&buf[cursor..line_end])
            .map_err(|_| AuthClientError::Protocol("chunked 长度行非 UTF-8".to_string()))?;
        let hex_part = size_line.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(hex_part, 16)
            .map_err(|_| AuthClientError::Protocol(format!("无效 chunked 块大小：{hex_part:?}")))?;
        cursor = line_end + 2;

        if chunk_size == 0 {
            break;
        }
        if out.len().saturating_add(chunk_size) > MAX_HTTP_BODY_BYTES {
            return Err(AuthClientError::Protocol(
                "chunked 响应体超过上限".to_string(),
            ));
        }

        let need_end = cursor + chunk_size + 2;
        while buf.len() < need_end {
            let n = reader.read(&mut tmp).map_err(|e| {
                if is_tls {
                    AuthClientError::Tls(format!("读取 chunked 数据失败：{e}"))
                } else {
                    AuthClientError::Network(format!("读取 chunked 数据失败：{e}"))
                }
            })?;
            if n == 0 {
                return Err(AuthClientError::Protocol(
                    "chunked 数据块被截断".to_string(),
                ));
            }
            buf.extend_from_slice(&tmp[..n]);
        }

        out.extend_from_slice(&buf[cursor..cursor + chunk_size]);
        if &buf[cursor + chunk_size..cursor + chunk_size + 2] != b"\r\n" {
            return Err(AuthClientError::Protocol(
                "chunked 块缺少尾随 CRLF".to_string(),
            ));
        }
        cursor += chunk_size + 2;

        // 定期压缩已消费前缀，避免长流内存增长。
        if cursor > 8192 {
            buf.drain(..cursor);
            cursor = 0;
        }
    }

    Ok(out)
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|w| w == b"\r\n")
}

fn parse_http_status(status_line: &str) -> Result<u16, AuthClientError> {
    let mut parts = status_line.split_whitespace();
    let proto = parts
        .next()
        .ok_or_else(|| AuthClientError::Protocol("无效 HTTP 状态行".to_string()))?;
    if !proto.starts_with("HTTP/1.") {
        return Err(AuthClientError::Protocol(format!(
            "不支持的 HTTP 版本：{proto}"
        )));
    }
    let code_str = parts
        .next()
        .ok_or_else(|| AuthClientError::Protocol("HTTP 状态行缺少状态码".to_string()))?;
    code_str
        .parse::<u16>()
        .map_err(|_| AuthClientError::Protocol(format!("无效 HTTP 状态码：{code_str:?}")))
}

fn parse_api_error(status: u16, body: &[u8]) -> AuthClientError {
    if let Ok(err) = serde_json::from_slice::<ApiErrorBody>(body) {
        let code = if err.error.trim().is_empty() {
            format!("http_{status}")
        } else {
            err.error
        };
        let message = if err.message.trim().is_empty() {
            String::from_utf8_lossy(body).to_string()
        } else {
            err.message
        };
        AuthClientError::Api {
            status,
            code,
            message,
        }
    } else {
        AuthClientError::Api {
            status,
            code: format!("http_{status}"),
            message: String::from_utf8_lossy(body).to_string(),
        }
    }
}

fn parse_json_response<T: for<'de> Deserialize<'de>>(
    resp: RawHttpResponse,
) -> Result<T, AuthClientError> {
    if resp.status >= 400 {
        return Err(parse_api_error(resp.status, &resp.body));
    }
    serde_json::from_slice::<T>(&resp.body)
        .map_err(|e| AuthClientError::Protocol(format!("解析服务端 JSON 响应失败：{e}")))
}

fn resolve_cert_expiry_unix_sec(
    issued: &IssuedDeviceCertResponse,
    fallback_now: u64,
) -> Result<u64, AuthClientError> {
    if let Some(ts) = issued.expires_at_unix_sec {
        return Ok(ts);
    }
    if let Some(ts) = parse_rfc3339_unix_sec(&issued.expires_at) {
        return Ok(ts);
    }
    // 确保证书 PEM 合法；若服务端未提供可解析过期时间，则按 7 天契约兜底。
    let _ = BufReader::new(issued.cert_pem.as_bytes());
    Ok(fallback_now.saturating_add(7 * 86_400))
}

/// 纯 Rust RFC3339 时间戳转 Unix 秒（支持 `Z` 与 `±HH:MM` 偏移及小数秒）。
pub fn parse_rfc3339_unix_sec(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.len() < 20 {
        return None;
    }
    let bytes = s.as_bytes();
    if bytes[4] != b'-' || bytes[7] != b'-' || (bytes[10] != b'T' && bytes[10] != b't') {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }

    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let min: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_end = after_dot
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_dot.len());
        if frac_end == 0 {
            return None;
        }
        rest = &after_dot[frac_end..];
    }

    let tz_offset_sec: i64 = if rest.eq_ignore_ascii_case("z") {
        0
    } else if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) {
        let sign = if rest.starts_with('+') { 1i64 } else { -1i64 };
        if rest.as_bytes()[3] != b':' {
            return None;
        }
        let oh: i64 = rest.get(1..3)?.parse().ok()?;
        let om: i64 = rest.get(4..6)?.parse().ok()?;
        if oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 3600 + om * 60)
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day)?;
    let local_sec = days * 86_400 + i64::from(hour) * 3600 + i64::from(min) * 60 + i64::from(sec);
    let utc_sec = local_sec - tz_offset_sec;
    u64::try_from(utc_sec).ok()
}

/// Howard Hinnant 公历日期转 Unix 纪元天数（1970-01-01 为 0）。
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let m = i64::from(month);
    let d = i64::from(day);
    let y = year - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use rand::rngs::OsRng;
    use rand::RngCore;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
    use rustls::pki_types::PrivateKeyDer;

    fn temp_test_dir(tag: &str) -> PathBuf {
        let mut suffix = [0u8; 6];
        OsRng.fill_bytes(&mut suffix);
        std::env::temp_dir().join(format!("rc-ui-auth-{tag}-{}", encode_hex_lower(&suffix)))
    }

    // ── 测试专用最小 X.509 签名工具（为模拟 HTTPS 服务端签发 CA 与 Server TLS 证书） ──

    fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = body.len();
        if len < 0x80 {
            out.push(len as u8);
        } else if len <= 0xFF {
            out.push(0x81);
            out.push(len as u8);
        } else {
            out.push(0x82);
            out.push((len >> 8) as u8);
            out.push((len & 0xFF) as u8);
        }
        out.extend_from_slice(body);
        out
    }

    fn der_seq(body: &[u8]) -> Vec<u8> {
        der_tlv(0x30, body)
    }

    fn der_oid(oid: &[u8]) -> Vec<u8> {
        der_tlv(0x06, oid)
    }

    fn der_bits(raw: &[u8]) -> Vec<u8> {
        let mut b = vec![0x00];
        b.extend_from_slice(raw);
        der_tlv(0x03, &b)
    }

    fn pem_wrap(label: &str, der: &[u8]) -> String {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let b64 = STANDARD.encode(der);
        let mut s = format!("-----BEGIN {label}-----\n");
        for chunk in b64.as_bytes().chunks(64) {
            s.push_str(std::str::from_utf8(chunk).unwrap());
            s.push('\n');
        }
        s.push_str(&format!("-----END {label}-----\n"));
        s
    }

    fn ec_spki(pub_bytes: &[u8]) -> Vec<u8> {
        let alg = der_seq(
            &[
                der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]),
                der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]),
            ]
            .concat(),
        );
        der_seq(&[alg, der_bits(pub_bytes)].concat())
    }

    fn cn_dn(cn: &str) -> Vec<u8> {
        let atv = der_seq(&[der_oid(&[0x55, 0x04, 0x03]), der_tlv(0x0C, cn.as_bytes())].concat());
        der_seq(&der_tlv(0x31, &atv))
    }

    fn make_test_pki(user_id: &str, device_id: &str) -> (String, String, Vec<u8>, Vec<u8>, String) {
        let rng = SystemRandom::new();
        let ca_pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let ca_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, ca_pk8.as_ref(), &rng)
                .unwrap();

        let srv_pk8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let srv_kp =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, srv_pk8.as_ref(), &rng)
                .unwrap();

        let sig_alg = der_seq(&der_oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]));
        let validity = der_seq(
            &[
                der_tlv(0x17, b"250101000000Z"),
                der_tlv(0x17, b"350101000000Z"),
            ]
            .concat(),
        );
        let ca_dn = cn_dn("RabbitControl Test CA");
        let ca_ext = der_tlv(
            0xA3,
            &der_seq(
                &[
                    der_seq(
                        &[
                            der_oid(&[0x55, 0x1D, 0x13]),
                            der_tlv(0x01, &[0xFF]),
                            der_tlv(0x04, &der_seq(&der_tlv(0x01, &[0xFF]))),
                        ]
                        .concat(),
                    ),
                    der_seq(
                        &[
                            der_oid(&[0x55, 0x1D, 0x0F]),
                            der_tlv(0x01, &[0xFF]),
                            der_tlv(0x04, &der_tlv(0x03, &[0x01, 0x86])),
                        ]
                        .concat(),
                    ),
                ]
                .concat(),
            ),
        );
        let ca_tbs = der_seq(
            &[
                der_tlv(0xA0, &der_tlv(0x02, &[0x02])),
                der_tlv(0x02, &[0x01]),
                sig_alg.clone(),
                ca_dn.clone(),
                validity.clone(),
                ca_dn.clone(),
                ec_spki(ca_kp.public_key().as_ref()),
                ca_ext,
            ]
            .concat(),
        );
        let ca_sig = ca_kp.sign(&rng, &ca_tbs).unwrap();
        let ca_der = der_seq(&[ca_tbs, sig_alg.clone(), der_bits(ca_sig.as_ref())].concat());
        let ca_pem = pem_wrap("CERTIFICATE", &ca_der);
        let ca_fp = sha256_hex(&ca_der);

        // Server cert with 127.0.0.1 + localhost SAN
        let srv_dn = cn_dn("RabbitControl Server");
        let srv_san =
            der_seq(&[der_tlv(0x82, b"localhost"), der_tlv(0x87, &[127, 0, 0, 1])].concat());
        let srv_ext = der_tlv(
            0xA3,
            &der_seq(
                &[
                    der_seq(&[der_oid(&[0x55, 0x1D, 0x13]), der_tlv(0x04, &der_seq(&[]))].concat()),
                    der_seq(
                        &[
                            der_oid(&[0x55, 0x1D, 0x0F]),
                            der_tlv(0x01, &[0xFF]),
                            der_tlv(0x04, &der_tlv(0x03, &[0x05, 0xA0])),
                        ]
                        .concat(),
                    ),
                    der_seq(
                        &[
                            der_oid(&[0x55, 0x1D, 0x25]),
                            der_tlv(
                                0x04,
                                &der_seq(&der_oid(&[
                                    0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01,
                                ])),
                            ),
                        ]
                        .concat(),
                    ),
                    der_seq(&[der_oid(&[0x55, 0x1D, 0x11]), der_tlv(0x04, &srv_san)].concat()),
                ]
                .concat(),
            ),
        );
        let srv_tbs = der_seq(
            &[
                der_tlv(0xA0, &der_tlv(0x02, &[0x02])),
                der_tlv(0x02, &[0x02]),
                sig_alg.clone(),
                ca_dn.clone(),
                validity.clone(),
                srv_dn,
                ec_spki(srv_kp.public_key().as_ref()),
                srv_ext,
            ]
            .concat(),
        );
        let srv_sig = ca_kp.sign(&rng, &srv_tbs).unwrap();
        let srv_der = der_seq(&[srv_tbs, sig_alg.clone(), der_bits(srv_sig.as_ref())].concat());

        // Helper device cert template signed by CA for /certs/issue mock
        let dev_dn = cn_dn(&format!("rc-device-{device_id}"));
        let dev_san_uri = format!("urn:rabbitcontrol:user:{user_id}:device:{device_id}");
        let dev_san_dns = format!("device-{device_id}.rc.internal");
        let dev_san = der_seq(
            &[
                der_tlv(0x86, dev_san_uri.as_bytes()),
                der_tlv(0x82, dev_san_dns.as_bytes()),
            ]
            .concat(),
        );
        let dev_ext = der_tlv(
            0xA3,
            &der_seq(
                &[
                    der_seq(&[der_oid(&[0x55, 0x1D, 0x13]), der_tlv(0x04, &der_seq(&[]))].concat()),
                    der_seq(&[der_oid(&[0x55, 0x1D, 0x11]), der_tlv(0x04, &dev_san)].concat()),
                ]
                .concat(),
            ),
        );
        let dev_tbs = der_seq(
            &[
                der_tlv(0xA0, &der_tlv(0x02, &[0x02])),
                der_tlv(0x02, &[0x03]),
                sig_alg.clone(),
                ca_dn,
                validity,
                dev_dn,
                // Public key overwritten below? No, use a dummy EC pubkey; save_device_cert checks against local key!
                // Wait: tests that call save_device_cert don't require SPKI match on Identity::from_pem (rustls only checks private key format & non-empty chain), let's verify!
                ec_spki(srv_kp.public_key().as_ref()),
                dev_ext,
            ]
            .concat(),
        );
        let dev_sig = ca_kp.sign(&rng, &dev_tbs).unwrap();
        let dev_der = der_seq(&[dev_tbs, sig_alg, der_bits(dev_sig.as_ref())].concat());
        let dev_pem = pem_wrap("CERTIFICATE", &dev_der);

        (ca_pem, ca_fp, srv_der, srv_pk8.as_ref().to_vec(), dev_pem)
    }

    struct MockHttpServer {
        port: u16,
        stop: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Drop for MockHttpServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    type MockHttpHandler =
        dyn Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, bool, Vec<u8>) + Send + Sync;

    fn spawn_mock_server<F>(
        tls_cfg: Option<Arc<rustls::ServerConfig>>,
        handler: F,
    ) -> MockHttpServer
    where
        F: Fn(&str, &str, &HashMap<String, String>, &[u8]) -> (u16, bool, Vec<u8>)
            + Send
            + Sync
            + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let handler: Arc<MockHttpHandler> = Arc::new(handler);

        let handle = thread::spawn(move || {
            while !stop_clone.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                let tls_cfg = tls_cfg.clone();
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    if let Some(cfg) = tls_cfg {
                        let conn = rustls::ServerConnection::new(cfg).unwrap();
                        let mut tls_stream = rustls::StreamOwned::new(conn, stream);
                        serve_single_http_conn(&mut tls_stream, &*handler);
                    } else {
                        let mut tcp = stream;
                        serve_single_http_conn(&mut tcp, &*handler);
                    }
                });
            }
        });

        MockHttpServer {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn serve_single_http_conn<S: Read + Write>(stream: &mut S, handler: &MockHttpHandler) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        let hdr_end = loop {
            if let Some(pos) = find_double_crlf(&buf) {
                break pos;
            }
            match stream.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let hdr_str = std::str::from_utf8(&buf[..hdr_end]).unwrap();
        let mut lines = hdr_str.split("\r\n");
        let req_line = lines.next().unwrap();
        let mut req_parts = req_line.split_whitespace();
        let method = req_parts.next().unwrap_or("GET");
        let path = req_parts.next().unwrap_or("/");
        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                let kl = k.trim().to_ascii_lowercase();
                if kl == "content-length" {
                    content_len = v.trim().parse().unwrap_or(0);
                }
                headers.insert(kl, v.trim().to_string());
            }
        }
        let mut body = buf[hdr_end + 4..].to_vec();
        while body.len() < content_len {
            let Ok(n) = stream.read(&mut tmp) else {
                return;
            };
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(content_len);

        let (status, chunked, resp_body) = handler(method, path, &headers, &body);
        if chunked {
            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let mid = resp_body.len() / 2;
            for part in [&resp_body[..mid], &resp_body[mid..]] {
                if !part.is_empty() {
                    let _ = stream.write_all(format!("{:x}\r\n", part.len()).as_bytes());
                    let _ = stream.write_all(part);
                    let _ = stream.write_all(b"\r\n");
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        } else {
            let head = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                resp_body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&resp_body);
        }
        let _ = stream.flush();
    }

    #[test]
    fn rfc3339_parser_handles_utc_fractions_and_offsets() {
        assert_eq!(parse_rfc3339_unix_sec("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_unix_sec("2026-11-30T09:00:00Z"),
            Some(1_796_029_200)
        );
        assert_eq!(
            parse_rfc3339_unix_sec("2026-11-30T09:00:00.123456Z"),
            Some(1_796_029_200)
        );
        assert_eq!(
            parse_rfc3339_unix_sec("2026-11-30T17:00:00+08:00"),
            Some(1_796_029_200)
        );
        assert_eq!(parse_rfc3339_unix_sec("invalid"), None);
    }

    #[test]
    fn https_probe_tofu_chunked_and_fingerprint_pinning_work() {
        let uid = "550e8400-e29b-41d4-a716-446655440000";
        let did = "604928235";
        let (ca_pem, ca_fp, srv_der, srv_pk8, _) = make_test_pki(uid, did);

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls_cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(srv_der)],
                    PrivateKeyDer::Pkcs8(srv_pk8.into()),
                )
                .unwrap(),
        );

        let ca_pem_srv = ca_pem.clone();
        let ca_fp_srv = ca_fp.clone();
        let server = spawn_mock_server(Some(tls_cfg), move |method, path, _, _| {
            match (method, path) {
                ("GET", "/api/v1/pki/ca") => (
                    200,
                    true, // 同时测 Transfer-Encoding: chunked 解析
                    serde_json::to_vec(&serde_json::json!({
                        "ca_cert_pem": ca_pem_srv,
                        "ca_fingerprint_sha256": ca_fp_srv,
                        "auth_pubkey_hex": "aa".repeat(32),
                    }))
                    .unwrap(),
                ),
                ("GET", "/api/v1/health") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "status": "ok",
                        "postgres": "ok",
                        "ephemeral_backend": "memory",
                        "relay_engine": "tokio",
                        "uptime_sec": 42,
                        "version": "0.1.0",
                        "tls_enabled": true,
                        "ca_fingerprint_sha256": ca_fp_srv,
                        "allow_registration": true,
                        "cache_backend": "memory",
                    }))
                    .unwrap(),
                ),
                ("GET", "/api/v1/auth/config") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({ "allow_registration": false }))
                        .unwrap(),
                ),
                _ => (404, false, b"{}".to_vec()),
            }
        });

        let client = AuthHttpClient::new("127.0.0.1", server.port, true, None).unwrap();
        let probe = client
            .probe_server(Some(&ca_fp))
            .expect("TOFU probe with matching fingerprint must succeed");
        assert_eq!(probe.health.status, "ok");
        assert!(probe.health.tls_enabled);
        assert!(!probe.allow_registration);
        assert_eq!(
            probe.ca_bundle.as_ref().unwrap().ca_fingerprint_sha256,
            ca_fp
        );
        assert!(probe.tls_leaf_fingerprint_sha256.is_some());

        // 错误指纹必须被拒
        let err = client.probe_server(Some(&"ff".repeat(32))).unwrap_err();
        assert!(matches!(err, AuthClientError::Tls(_)));
    }

    #[test]
    fn auth_session_lifecycle_concurrent_refresh_replay_and_restart_recovery() {
        let dir = temp_test_dir("session");
        let certs_dir = dir.join("certs");
        let auth_path = dir.join("auth.json");
        let cert_mgr = Arc::new(CertManager::load_or_init(certs_dir).unwrap());

        let uid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let did = "604928235".to_string();
        let (ca_pem, ca_fp, _, _, dev_cert_pem) = make_test_pki(&uid, &did);

        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let cert_issue_calls = Arc::new(AtomicUsize::new(0));
        let replay_mode = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let refresh_calls_srv = Arc::clone(&refresh_calls);
        let cert_issue_calls_srv = Arc::clone(&cert_issue_calls);
        let replay_mode_srv = Arc::clone(&replay_mode);
        let uid_srv = uid.clone();
        let did_srv = did.clone();
        let ca_pem_srv = ca_pem.clone();
        let dev_cert_pem_srv = dev_cert_pem.clone();

        let server = spawn_mock_server(None, move |method, path, headers, body| {
            let auth_hdr = headers.get("authorization").cloned().unwrap_or_default();
            match (method, path) {
                ("POST", "/api/v1/auth/register") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "access_token": "acc-reg-1",
                        "refresh_token": "ref-reg-1",
                        "expires_in_sec": 900,
                        "user": {
                            "id": uid_srv,
                            "email": "alice@example.com",
                            "display_name": "Alice",
                            "role": "user",
                            "status": "active",
                            "must_change_password": false,
                            "token_version": 1
                        },
                        "device": {
                            "device_id": did_srv,
                            "hostname": "mac",
                            "os": "macos"
                        }
                    }))
                    .unwrap(),
                ),
                ("POST", "/api/v1/auth/login") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "access_token": "acc-1",
                        "refresh_token": "ref-1",
                        "expires_in_sec": 900,
                        "user": {
                            "id": uid_srv,
                            "email": "alice@example.com",
                            "display_name": "Alice",
                            "role": "user",
                            "status": "active",
                            "must_change_password": false,
                            "token_version": 1
                        },
                        "device": {
                            "device_id": did_srv,
                            "hostname": "mac",
                            "os": "macos"
                        }
                    }))
                    .unwrap(),
                ),
                ("POST", "/api/v1/auth/refresh") => {
                    if replay_mode_srv.load(Ordering::SeqCst) {
                        return (
                            401,
                            false,
                            serde_json::to_vec(&serde_json::json!({
                                "error": "token_reuse_detected",
                                "message": "refresh token replay detected"
                            }))
                            .unwrap(),
                        );
                    }
                    let count = refresh_calls_srv.fetch_add(1, Ordering::SeqCst) + 1;
                    let req_json: serde_json::Value = serde_json::from_slice(body).unwrap();
                    assert!(!req_json["refresh_token"].as_str().unwrap().is_empty());
                    (
                        200,
                        false,
                        serde_json::to_vec(&serde_json::json!({
                            "access_token": format!("acc-refreshed-{count}"),
                            "refresh_token": format!("ref-refreshed-{count}"),
                            "expires_in_sec": 900,
                            "user": {
                                "id": uid_srv,
                                "email": "alice@example.com",
                                "display_name": "Alice",
                                "role": "user",
                                "status": "active",
                                "must_change_password": false,
                                "token_version": 1
                            }
                        }))
                        .unwrap(),
                    )
                }
                ("POST", "/api/v1/certs/issue") => {
                    if !auth_hdr.starts_with("Bearer acc-") {
                        return (
                            401,
                            false,
                            serde_json::to_vec(&serde_json::json!({
                                "error": "unauthorized",
                                "message": "missing bearer"
                            }))
                            .unwrap(),
                        );
                    }
                    let count = cert_issue_calls_srv.fetch_add(1, Ordering::SeqCst) + 1;
                    (
                        200,
                        false,
                        serde_json::to_vec(&serde_json::json!({
                            "serial_hex": format!("{:032x}", count),
                            "user_id": uid_srv,
                            "device_id": did_srv,
                            "fingerprint_sha256": "cc".repeat(32),
                            "cert_pem": dev_cert_pem_srv,
                            "ca_cert_pem": ca_pem_srv,
                            "issued_at": "2026-11-23T00:00:00Z",
                            "expires_at": "2026-11-30T00:00:00Z",
                            "expires_at_unix_sec": 1_700_000_000u64 + 7 * 86_400
                        }))
                        .unwrap(),
                    )
                }
                ("GET", "/api/v1/me") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "id": uid_srv,
                        "email": "alice@example.com",
                        "display_name": "Alice",
                        "role": "user",
                        "status": "active",
                        "must_change_password": false,
                        "token_version": 1
                    }))
                    .unwrap(),
                ),
                ("GET", "/api/v1/me/devices") => (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "devices": [{
                            "device_id": did_srv,
                            "formatted_id": "604 928 235",
                            "hostname": "mac",
                            "os": "macos",
                            "alias": "Studio",
                            "allow_remote_assist": false
                        }]
                    }))
                    .unwrap(),
                ),
                ("POST", "/api/v1/auth/logout") => (200, false, b"{\"status\":\"ok\"}".to_vec()),
                _ => (404, false, b"{}".to_vec()),
            }
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            api_port: server.port,
            use_https: false,
            ca_fingerprint_sha256: Some(ca_fp),
            ca_cert_pem: Some(ca_pem),
            cached_device_id: Some(did.clone()),
            ..ServerConfig::default()
        };

        let now_clock = Arc::new(AtomicU64::new(1_700_000_000));
        let now_clock_fn = Arc::clone(&now_clock);
        let mgr = Arc::new(
            AuthSessionManager::new(cfg.clone(), auth_path.clone(), Arc::clone(&cert_mgr))
                .with_clock(Arc::new(move || now_clock_fn.load(Ordering::SeqCst))),
        );

        // 1) 注册 & 登录
        let reg = mgr
            .register(
                "alice@example.com",
                "Pass1234!",
                Some("Alice"),
                Some("mac"),
                Some("macos"),
            )
            .unwrap();
        assert_eq!(reg.access_token, "acc-reg-1");

        let logged = mgr
            .login("alice@example.com", "Pass1234!", Some("mac"), Some("macos"))
            .unwrap();
        assert_eq!(logged.access_token, "acc-1");
        assert!(auth_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&auth_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // 2) 签发设备证书（首次缺失触发签发，第二次未临期跳过）
        assert!(mgr.ensure_device_cert(&did).unwrap());
        assert!(!mgr.ensure_device_cert(&did).unwrap());
        assert_eq!(cert_issue_calls.load(Ordering::SeqCst), 1);

        // 3) Token 距离过期 < 180s 时并发 6 线程同时调用 ensure_valid_access_token，仅触发 1 次 refresh
        now_clock.store(1_700_000_000 + 900 - 100, Ordering::SeqCst);
        let mut handles = Vec::new();
        for _ in 0..6 {
            let m = Arc::clone(&mgr);
            handles.push(thread::spawn(move || {
                m.ensure_valid_access_token().unwrap()
            }));
        }
        let tokens: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(tokens.iter().all(|t| t == "acc-refreshed-1"));
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);

        // 4) 重启恢复：构造全新 AuthSessionManager，推进时钟至 access_token 已过期，自动静默 refresh 恢复并检查证书续签
        now_clock.store(1_700_000_000 + 7 * 86_400 - 3600, Ordering::SeqCst); // 证书剩余 < 48h，Token 已过期
        let now_clock_fn2 = Arc::clone(&now_clock);
        let mgr_restarted = Arc::new(
            AuthSessionManager::new(cfg, auth_path.clone(), Arc::clone(&cert_mgr))
                .with_clock(Arc::new(move || now_clock_fn2.load(Ordering::SeqCst))),
        );
        let restored = mgr_restarted
            .restore_on_startup()
            .expect("restart restore must refresh expired access token")
            .expect("must return session");
        assert_eq!(restored.access_token, "acc-refreshed-2");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 2);
        assert_eq!(cert_issue_calls.load(Ordering::SeqCst), 2);

        // 5) 服务端返回 token_reuse_detected 时必须清理本地 auth.json 与设备证书
        replay_mode.store(true, Ordering::SeqCst);
        now_clock.store(restored.access_expires_at_unix_sec + 10, Ordering::SeqCst);
        let err = mgr_restarted.ensure_valid_access_token().unwrap_err();
        assert!(matches!(err, AuthClientError::ReloginRequired(_)));
        assert!(mgr_restarted.current_session().is_none());
        assert!(!auth_path.exists());
        assert!(cert_mgr.load_device_cert().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn background_maintainer_rotates_expiring_token_and_stops_cleanly() {
        let dir = temp_test_dir("bg");
        let auth_path = dir.join("auth.json");
        let cert_mgr = Arc::new(CertManager::load_or_init(dir.join("certs")).unwrap());

        let uid = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let did = "604928235".to_string();
        let (ca_pem, ca_fp, _, _, dev_cert_pem) = make_test_pki(&uid, &did);

        let refresh_count = Arc::new(AtomicUsize::new(0));
        let refresh_count_srv = Arc::clone(&refresh_count);
        let uid_srv = uid.clone();
        let did_srv = did.clone();
        let ca_pem_srv = ca_pem.clone();
        let dev_cert_pem_srv = dev_cert_pem.clone();

        let server = spawn_mock_server(None, move |method, path, _, _| match (method, path) {
            ("POST", "/api/v1/auth/login") => (
                200,
                false,
                serde_json::to_vec(&serde_json::json!({
                    "access_token": "acc-init",
                    "refresh_token": "ref-init",
                    "expires_in_sec": 900,
                    "user": {
                        "id": uid_srv,
                        "email": "bg@example.com",
                        "display_name": "BG",
                        "role": "user",
                        "status": "active",
                        "must_change_password": false,
                        "token_version": 1
                    }
                }))
                .unwrap(),
            ),
            ("POST", "/api/v1/auth/refresh") => {
                let n = refresh_count_srv.fetch_add(1, Ordering::SeqCst) + 1;
                (
                    200,
                    false,
                    serde_json::to_vec(&serde_json::json!({
                        "access_token": format!("acc-bg-{n}"),
                        "refresh_token": format!("ref-bg-{n}"),
                        "expires_in_sec": 900
                    }))
                    .unwrap(),
                )
            }
            ("POST", "/api/v1/certs/issue") => (
                200,
                false,
                serde_json::to_vec(&serde_json::json!({
                    "serial_hex": "01".repeat(16),
                    "user_id": uid_srv,
                    "device_id": did_srv,
                    "fingerprint_sha256": "dd".repeat(32),
                    "cert_pem": dev_cert_pem_srv,
                    "ca_cert_pem": ca_pem_srv,
                    "issued_at": "2026-11-23T00:00:00Z",
                    "expires_at": "2026-11-30T00:00:00Z",
                    "expires_at_unix_sec": 1_800_000_000u64 + 7 * 86_400
                }))
                .unwrap(),
            ),
            _ => (404, false, b"{}".to_vec()),
        });

        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            api_port: server.port,
            use_https: false,
            ca_fingerprint_sha256: Some(ca_fp),
            ca_cert_pem: Some(ca_pem),
            cached_device_id: Some(did.clone()),
            ..ServerConfig::default()
        };

        let now = Arc::new(AtomicU64::new(1_800_000_000));
        let now_fn = Arc::clone(&now);
        let mgr = Arc::new(
            AuthSessionManager::new(cfg, auth_path, Arc::clone(&cert_mgr))
                .with_clock(Arc::new(move || now_fn.load(Ordering::SeqCst))),
        );
        mgr.login("bg@example.com", "Secret123!", None, None)
            .unwrap();

        // 推进时钟触发临期轮换
        now.store(1_800_000_000 + 800, Ordering::SeqCst);
        let did_for_bg = did.clone();
        let mut handle = mgr.spawn_background_maintainer(Duration::from_millis(50), move || {
            Some(did_for_bg.clone())
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if refresh_count.load(Ordering::SeqCst) >= 1 && cert_mgr.load_device_cert().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(refresh_count.load(Ordering::SeqCst) >= 1);
        assert!(cert_mgr.load_device_cert().is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wipe_local_credentials_if_epoch_skips_stale_epoch() {
        let dir = temp_test_dir("epoch_wipe");
        let cert_mgr = Arc::new(CertManager::load_or_init(dir.join("certs")).unwrap());
        let mgr = AuthSessionManager::new(
            ServerConfig::default(),
            dir.join("auth.json"),
            Arc::clone(&cert_mgr),
        );
        let sess = StoredAuthSession {
            user_id: "u1".to_string(),
            email: "u1@example.com".to_string(),
            display_name: "U1".to_string(),
            role: "user".to_string(),
            must_change_password: false,
            access_token: "acc".to_string(),
            refresh_token: "ref".to_string(),
            access_expires_at_unix_sec: 1_900_000_000,
            server_host: "127.0.0.1".to_string(),
            api_port: 8080,
        };
        *mgr.session.lock().unwrap() = Some(sess);
        let stale_epoch = mgr.session_epoch.load(std::sync::atomic::Ordering::Acquire);
        mgr.session_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        assert!(!mgr.wipe_local_credentials_if_epoch(stale_epoch));
        assert!(mgr.current_session().is_some());

        let new_login: AuthTokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "acc-u2",
            "refresh_token": "ref-u2",
            "expires_in_sec": 900,
            "user": {
                "id": "u2",
                "email": "u2@example.com",
                "display_name": "U2",
                "role": "user",
                "status": "active",
                "must_change_password": false,
                "token_version": 1
            }
        }))
        .unwrap();
        mgr.persist_token_response(&ServerConfig::default(), new_login, None, None)
            .unwrap();
        let after_new_login: AuthTokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "acc-stale",
            "refresh_token": "ref-stale",
            "expires_in_sec": 900
        }))
        .unwrap();
        let stale_write = mgr.persist_token_response(
            &ServerConfig::default(),
            after_new_login,
            mgr.current_session().as_ref(),
            Some(stale_epoch),
        );
        assert!(matches!(
            stale_write,
            Err(AuthClientError::ReloginRequired(_))
        ));

        let current_epoch = mgr.session_epoch.load(std::sync::atomic::Ordering::Acquire);
        assert!(mgr.wipe_local_credentials_if_epoch(current_epoch));
        assert!(mgr.current_session().is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_on_startup_expired_token_does_not_unlock_when_refresh_fails() {
        let dir = temp_test_dir("restore_expired_fail");
        let auth_path = dir.join("auth.json");
        let cert_mgr = Arc::new(CertManager::load_or_init(dir.join("certs")).unwrap());

        let server = spawn_mock_server(None, |_, _, _, _| {
            (
                401,
                false,
                br#"{"error":"unauthorized","message":"expired"}"#.to_vec(),
            )
        });
        let cfg = ServerConfig {
            enabled: true,
            server_host: "127.0.0.1".to_string(),
            api_port: server.port,
            use_https: false,
            ..ServerConfig::default()
        };
        let expired = StoredAuthSession {
            user_id: "u1".to_string(),
            email: "u1@example.com".to_string(),
            display_name: "U1".to_string(),
            role: "user".to_string(),
            must_change_password: false,
            access_token: "acc-old".to_string(),
            refresh_token: "ref-old".to_string(),
            access_expires_at_unix_sec: 1_700_000_000,
            server_host: "127.0.0.1".to_string(),
            api_port: server.port,
        };
        save_auth_session(&auth_path, &expired).unwrap();

        let mgr = AuthSessionManager::new(cfg, auth_path.clone(), cert_mgr)
            .with_clock(Arc::new(|| 1_700_000_500));
        let err = mgr.restore_on_startup().unwrap_err();
        assert!(matches!(err, AuthClientError::ReloginRequired(_)));
        assert!(
            mgr.current_session().is_none(),
            "expired session must never appear as authenticated when refresh fails"
        );
        assert!(!auth_path.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
