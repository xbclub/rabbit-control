#![deny(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequest, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoHttpBuilder;
use hyper_util::service::TowerToHyperService;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::auth::{
    generate_refresh_token, hash_password, hash_token_sha256_hex, verify_password,
    AccessTokenClaims, AuthAuthority, AuthError, ACCESS_TOKEN_TTL, REFRESH_TOKEN_TTL,
};
use crate::ephemeral::{EphemeralStore, SettingsCache};
use crate::pki::{PkiAuthority, PkiError};
use crate::relay::{RelayServer, RelaySessionInfo};
use crate::signal::{canonical_ip, SignalHub};
use crate::store::{
    format_device_id, is_valid_device_id, normalize_device_id, normalize_email, validate_email,
    validate_role, validate_status, validate_text_field, AuthStore, CreateSessionInput,
    CreateUserInput, DeviceCertRecord, DeviceRecord, DeviceStore, RegisterInput, StoreError,
    UpdateUserInput, UserRecord, DEFAULT_LIST_LIMIT, MAX_CLIENT_INFO_LEN, MAX_DEVICE_ALIAS_LEN,
    MAX_DEVICE_GROUP_LEN, MAX_DEVICE_KEY_LEN, MAX_DEVICE_PUBKEY_LEN, MAX_HOSTNAME_LEN,
    MAX_LIST_LIMIT, MAX_OS_LEN, ROLE_ADMIN, ROLE_USER, STATUS_ACTIVE,
};

pub const MAX_REGISTER_BODY_BYTES: usize = 8 * 1024;
pub const MAX_AUTH_BODY_BYTES: usize = 32 * 1024;
pub const MIN_PASSWORD_LEN: usize = 8;
pub const MAX_PASSWORD_LEN: usize = 128;

const DEFAULT_AUTH_RATE_LIMIT_MAX_IP: usize = 30;
const DEFAULT_AUTH_RATE_LIMIT_MAX_EMAIL: usize = 15;
const DEFAULT_AUTH_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const MAX_RATE_LIMIT_KEYS: usize = 16_384;
const HTTPS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct AdvertisedEndpoints {
    pub signal_tcp_addr: String,
    pub udp_probe_addr: String,
    pub tcp_relay_addr: String,
}

#[derive(Debug)]
pub struct AuthRateLimiter {
    by_ip: DashMap<String, VecDeque<Instant>>,
    by_email: DashMap<String, VecDeque<Instant>>,
    max_per_ip: usize,
    max_per_email: usize,
    window: Duration,
}

impl Default for AuthRateLimiter {
    fn default() -> Self {
        Self::new(
            DEFAULT_AUTH_RATE_LIMIT_MAX_IP,
            DEFAULT_AUTH_RATE_LIMIT_MAX_EMAIL,
            DEFAULT_AUTH_RATE_LIMIT_WINDOW,
        )
    }
}

impl AuthRateLimiter {
    pub fn new(max_per_ip: usize, max_per_email: usize, window: Duration) -> Self {
        Self {
            by_ip: DashMap::new(),
            by_email: DashMap::new(),
            max_per_ip,
            max_per_email,
            window,
        }
    }

    fn check_bucket(
        table: &DashMap<String, VecDeque<Instant>>,
        key: String,
        max_requests: usize,
        window: Duration,
    ) -> bool {
        if max_requests == 0 {
            return false;
        }
        let now = Instant::now();
        if table.len() >= MAX_RATE_LIMIT_KEYS && !table.contains_key(&key) {
            table.retain(|_, q| {
                while q
                    .front()
                    .is_some_and(|t| now.saturating_duration_since(*t) >= window)
                {
                    q.pop_front();
                }
                !q.is_empty()
            });
            if table.len() >= MAX_RATE_LIMIT_KEYS {
                let evict: Vec<String> = table.iter().take(256).map(|e| e.key().clone()).collect();
                for k in evict {
                    table.remove(&k);
                }
            }
        }

        let mut entry = table.entry(key).or_default();
        let queue = entry.value_mut();
        while queue
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= window)
        {
            queue.pop_front();
        }
        if queue.len() >= max_requests {
            return false;
        }
        queue.push_back(now);
        true
    }

    pub fn check_ip(&self, ip: Option<IpAddr>) -> bool {
        let Some(ip) = ip else {
            return true;
        };
        Self::check_bucket(
            &self.by_ip,
            canonical_ip(ip).to_string(),
            self.max_per_ip,
            self.window,
        )
    }

    pub fn check_email(&self, normalized_email: &str) -> bool {
        if normalized_email.is_empty() {
            return true;
        }
        Self::check_bucket(
            &self.by_email,
            normalized_email.to_string(),
            self.max_per_email,
            self.window,
        )
    }
}

#[derive(Clone)]
pub struct AppState {
    pub device_store: Arc<dyn DeviceStore>,
    pub auth_store: Arc<dyn AuthStore>,
    pub ephemeral: Arc<dyn EphemeralStore>,
    pub auth_authority: Arc<AuthAuthority>,
    pub pki_authority: Arc<PkiAuthority>,
    pub settings_cache: Arc<SettingsCache>,
    pub signal_hub: Arc<SignalHub>,
    pub relay_server: Arc<RelayServer>,
    pub endpoints: AdvertisedEndpoints,
    pub started_at: Instant,
    pub admin_token: Option<String>,
    pub cors_origins: Vec<String>,
    pub tls_enabled: bool,
    pub web_dir: Option<PathBuf>,
    pub auth_rate_limiter: Arc<AuthRateLimiter>,
    pub allow_unauthenticated_legacy_admin: bool,
}

#[derive(Clone, Debug)]
struct AuthenticatedUser {
    user: UserRecord,
    claims: AccessTokenClaims,
}

#[derive(Clone, Debug)]
enum AdminActor {
    Jwt(Box<AuthenticatedUser>),
    StaticToken,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
}

fn api_error(status: StatusCode, error: impl Into<String>, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: error.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

fn map_store_error(err: StoreError, context: &'static str) -> Response {
    match err {
        StoreError::InvalidInput(msg) => api_error(StatusCode::BAD_REQUEST, "invalid_input", msg),
        StoreError::Conflict(msg) => api_error(StatusCode::CONFLICT, "conflict", msg),
        StoreError::NotFound(msg) => api_error(StatusCode::NOT_FOUND, "not_found", msg),
        StoreError::Forbidden(msg) => api_error(StatusCode::FORBIDDEN, "forbidden", msg),
        StoreError::Unauthorized(msg) => api_error(StatusCode::UNAUTHORIZED, "unauthorized", msg),
        StoreError::DeviceDisabled(id) => api_error(
            StatusCode::FORBIDDEN,
            "device_disabled",
            format!("device {id} is disabled"),
        ),
        StoreError::Cache(msg) => {
            warn!(error = %msg, context, "ephemeral cache coherence operation failed");
            api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "cache_unavailable",
                "settings cache temporarily unavailable",
            )
        }
        other => {
            warn!(error = %other, context, "store operation failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "internal storage error",
            )
        }
    }
}

/// Custom JSON extractor that always responds with `application/json` `ErrorBody`
/// on syntax/type/content-type/body-limit rejections instead of Axum's `text/plain` default.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request<Body>, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => {
                let status = rejection.status();
                let code = match rejection {
                    JsonRejection::JsonDataError(_) => "invalid_json_data",
                    JsonRejection::JsonSyntaxError(_) => "invalid_json_syntax",
                    JsonRejection::MissingJsonContentType(_) => "missing_json_content_type",
                    JsonRejection::BytesRejection(_) => {
                        if status == StatusCode::PAYLOAD_TOO_LARGE {
                            "payload_too_large"
                        } else {
                            "invalid_body"
                        }
                    }
                    _ => "invalid_json",
                };
                Err(api_error(status, code, rejection.body_text()))
            }
        }
    }
}

/// Constant-time byte slice equality check for bearer token comparison.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let max_len = a.len().max(b.len());
    for i in 0..max_len {
        let lhs = a.get(i).copied().unwrap_or(0);
        let rhs = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(lhs ^ rhs);
    }
    std::hint::black_box(diff) == 0
}

/// Validate and build a strict `CorsLayer` from configured origin strings.
///
/// - Empty list: no cross-origin requests allowed (same-origin only).
/// - Contains `"*"`: allows any origin (`Access-Control-Allow-Origin: *`), explicitly opted in.
/// - Otherwise: parses every entry as an HTTP `HeaderValue` origin allowlist.
pub fn build_cors_layer(origins: &[String]) -> Result<CorsLayer, String> {
    let cleaned: Vec<&str> = origins
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let allow_origin = if cleaned.is_empty() {
        AllowOrigin::list(Vec::<HeaderValue>::new())
    } else if cleaned.contains(&"*") {
        AllowOrigin::any()
    } else {
        let mut parsed = Vec::with_capacity(cleaned.len());
        for origin in cleaned {
            let val = HeaderValue::from_str(origin)
                .map_err(|err| format!("invalid CORS origin '{origin}': {err}"))?;
            parsed.push(val);
        }
        AllowOrigin::list(parsed)
    };

    Ok(CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]))
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|val| val.to_str().ok())
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn unauthorized_response(code: &'static str, message: &'static str) -> Response {
    let mut resp = api_error(StatusCode::UNAUTHORIZED, code, message);
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"rc-server\""),
    );
    resp
}

async fn authenticate_bearer_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, Box<Response>> {
    let Some(token) = extract_bearer_token(headers) else {
        return Err(Box::new(unauthorized_response(
            "unauthorized",
            "missing bearer token",
        )));
    };

    let claims = match state.auth_authority.verify_access_token(token) {
        Ok(c) => c,
        Err(AuthError::TokenExpired) => {
            return Err(Box::new(unauthorized_response(
                "token_expired",
                "access token expired",
            )));
        }
        Err(_) => {
            return Err(Box::new(unauthorized_response(
                "unauthorized",
                "invalid access token",
            )));
        }
    };

    let user = match state.auth_store.get_user_by_id(claims.sub).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return Err(Box::new(unauthorized_response(
                "unauthorized",
                "user account not found",
            )));
        }
        Err(err) => {
            warn!(error = %err, "failed to load authenticated user");
            return Err(Box::new(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "failed to verify user",
            )));
        }
    };

    if user.status != STATUS_ACTIVE {
        return Err(Box::new(api_error(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "user account is disabled",
        )));
    }
    if user.token_version != claims.ver {
        return Err(Box::new(unauthorized_response(
            "token_revoked",
            "access token has been revoked",
        )));
    }
    match state.auth_store.get_user_session(claims.sid).await {
        Ok(Some(sess)) => {
            if sess.revoked_at.is_some() {
                return Err(Box::new(unauthorized_response(
                    "token_revoked",
                    "session has been revoked",
                )));
            }
        }
        Ok(None) => {
            return Err(Box::new(unauthorized_response(
                "token_revoked",
                "session not found or expired",
            )));
        }
        Err(err) => {
            warn!(error = %err, "failed to verify user session");
            return Err(Box::new(api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "database_error",
                "failed to verify session",
            )));
        }
    }

    Ok(AuthenticatedUser { user, claims })
}

async fn require_user_auth_allow_pwd_change(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match authenticate_bearer_user(&state, request.headers()).await {
        Ok(authed) => {
            request.extensions_mut().insert(authed);
            next.run(request).await
        }
        Err(resp) => *resp,
    }
}

async fn require_user_auth_full(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    match authenticate_bearer_user(&state, request.headers()).await {
        Ok(authed) => {
            if authed.user.must_change_password || authed.claims.pwd_change_only {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "password_change_required",
                    "initial password must be changed before accessing this endpoint",
                );
            }
            request.extensions_mut().insert(authed);
            next.run(request).await
        }
        Err(resp) => *resp,
    }
}

async fn require_admin_auth(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(provided) = extract_bearer_token(request.headers()) {
        if let Some(expected) = state.admin_token.as_deref() {
            if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
                request.extensions_mut().insert(AdminActor::StaticToken);
                return next.run(request).await;
            }
        }

        match authenticate_bearer_user(&state, request.headers()).await {
            Ok(authed) => {
                if authed.user.must_change_password || authed.claims.pwd_change_only {
                    return api_error(
                        StatusCode::FORBIDDEN,
                        "password_change_required",
                        "initial password must be changed before accessing admin endpoints",
                    );
                }
                if authed.user.role != ROLE_ADMIN {
                    return api_error(
                        StatusCode::FORBIDDEN,
                        "forbidden",
                        "administrator privileges required",
                    );
                }
                request
                    .extensions_mut()
                    .insert(AdminActor::Jwt(Box::new(authed)));
                return next.run(request).await;
            }
            Err(resp) => return *resp,
        }
    }

    if state.allow_unauthenticated_legacy_admin && state.admin_token.is_none() {
        request.extensions_mut().insert(AdminActor::StaticToken);
        return next.run(request).await;
    }

    unauthorized_response("unauthorized", "missing or invalid admin bearer token")
}

fn validate_password_policy(password: &str) -> Result<(), &'static str> {
    if password.contains('\0') {
        return Err("password must not contain NUL bytes");
    }
    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&password.len()) {
        return Err("password length must be between 8 and 128 bytes");
    }
    Ok(())
}

async fn revoke_cert_list_in_ephemeral(ephemeral: &dyn EphemeralStore, certs: &[DeviceCertRecord]) {
    let now = Utc::now();
    for cert in certs {
        if cert.expires_at > now {
            let ttl = (cert.expires_at - now)
                .to_std()
                .unwrap_or_else(|_| Duration::from_secs(1));
            if let Err(err) = ephemeral.revoke_cert_serial(&cert.serial_hex, ttl).await {
                warn!(serial = %cert.serial_hex, error = %err, "failed to write cert revocation to ephemeral cache");
            }
        }
    }
}

async fn revoke_and_kick_device_previous_owner(
    state: &AppState,
    device_id: &str,
    prev_owner_id: Uuid,
    reason: &str,
) {
    if let Ok(certs) = state
        .auth_store
        .list_device_certs(Some(prev_owner_id), Some(device_id))
        .await
    {
        for cert in certs {
            if cert.revoked_at.is_none() {
                if let Ok(Some(revoked)) = state
                    .auth_store
                    .revoke_cert_by_serial(&cert.serial_hex, Some(reason))
                    .await
                {
                    revoke_cert_list_in_ephemeral(&*state.ephemeral, &[revoked]).await;
                }
            }
        }
    }
    let _ = state
        .auth_store
        .revoke_device_sessions(prev_owner_id, device_id)
        .await;
    let _ = state.signal_hub.kick_device(device_id, reason).await;
    let _ = state.relay_server.kick_device_sessions(device_id);
}

struct BindDeviceParams<'a> {
    device_key: &'a str,
    hostname: Option<&'a str>,
    os: Option<&'a str>,
    device_group: Option<&'a str>,
    alias: Option<&'a str>,
    device_pubkey: Option<&'a str>,
}

fn validate_optional_auth_device_fields(
    device_key: Option<&str>,
    hostname: Option<&str>,
    os: Option<&str>,
    device_group: Option<&str>,
    alias: Option<&str>,
    device_pubkey: Option<&str>,
    client_info: Option<&str>,
) -> Result<(), Box<Response>> {
    if let Some(dk) = device_key.map(str::trim).filter(|s| !s.is_empty()) {
        validate_text_field(dk, MAX_DEVICE_KEY_LEN, "device_key")
            .map_err(|e| Box::new(map_store_error(e, "validate_device_key")))?;
    }
    if let Some(h) = hostname {
        validate_text_field(h, MAX_HOSTNAME_LEN, "hostname")
            .map_err(|e| Box::new(map_store_error(e, "validate_hostname")))?;
    }
    if let Some(o) = os {
        validate_text_field(o, MAX_OS_LEN, "os")
            .map_err(|e| Box::new(map_store_error(e, "validate_os")))?;
    }
    if let Some(g) = device_group {
        validate_text_field(g, MAX_DEVICE_GROUP_LEN, "device_group")
            .map_err(|e| Box::new(map_store_error(e, "validate_device_group")))?;
    }
    if let Some(a) = alias {
        validate_text_field(a, MAX_DEVICE_ALIAS_LEN, "alias")
            .map_err(|e| Box::new(map_store_error(e, "validate_alias")))?;
    }
    if let Some(pk) = device_pubkey {
        validate_text_field(pk, MAX_DEVICE_PUBKEY_LEN, "device_pubkey")
            .map_err(|e| Box::new(map_store_error(e, "validate_device_pubkey")))?;
    }
    if let Some(ci) = client_info {
        validate_text_field(ci, MAX_CLIENT_INFO_LEN, "client_info")
            .map_err(|e| Box::new(map_store_error(e, "validate_client_info")))?;
    }
    Ok(())
}

async fn register_and_bind_device_for_user(
    state: &AppState,
    user_id: Uuid,
    params: BindDeviceParams<'_>,
) -> Result<DeviceRecord, Box<Response>> {
    validate_optional_auth_device_fields(
        Some(params.device_key),
        params.hostname,
        params.os,
        params.device_group,
        params.alias,
        params.device_pubkey,
        None,
    )?;

    let reg = state
        .device_store
        .register_or_get(RegisterInput {
            device_key: params.device_key.to_string(),
            hostname: params.hostname.unwrap_or_default().to_string(),
            os: params.os.unwrap_or_default().to_string(),
            device_group: params.device_group.unwrap_or_default().to_string(),
        })
        .await
        .map_err(|e| Box::new(map_store_error(e, "register_device_for_user")))?;

    let (bound, prev_owner) = state
        .auth_store
        .bind_device_to_user_with_prev(
            &reg.device_id,
            user_id,
            params.alias.map(str::trim).filter(|s| !s.is_empty()),
            params
                .device_pubkey
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        )
        .await
        .map_err(|e| Box::new(map_store_error(e, "bind_device_to_user")))?
        .ok_or_else(|| {
            Box::new(api_error(
                StatusCode::NOT_FOUND,
                "device_not_found",
                "device vanished",
            ))
        })?;

    if let Some(prev_uid) = prev_owner {
        if prev_uid != user_id {
            revoke_and_kick_device_previous_owner(
                state,
                &bound.device_id,
                prev_uid,
                "device_rebound_to_another_user",
            )
            .await;
        }
    }

    Ok(bound)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceStatusFilter {
    All,
    Online,
    Offline,
    InSession,
}

impl DeviceStatusFilter {
    fn parse(raw: Option<&str>) -> Result<Self, &'static str> {
        let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::All);
        };
        match trimmed.to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            "in_session" => Ok(Self::InSession),
            _ => Err("status must be one of: online, offline, in_session, all"),
        }
    }

    fn matches(self, online: bool, in_session: bool) -> bool {
        match self {
            Self::All => true,
            Self::Online => online || in_session,
            Self::Offline => !online && !in_session,
            Self::InSession => in_session,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub postgres: String,
    pub ephemeral_backend: String,
    pub cache_backend: String,
    pub relay_engine: String,
    pub tls_enabled: bool,
    pub ca_fingerprint_sha256: String,
    pub allow_registration: bool,
    pub uptime_sec: u64,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PkiCaResponse {
    pub ca_cert_pem: String,
    pub ca_fingerprint_sha256: String,
    pub auth_pubkey_hex: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthConfigResponse {
    pub allow_registration: bool,
}

#[derive(Debug, Deserialize)]
pub struct AuthRegisterRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub device_key: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default, alias = "group")]
    pub device_group: Option<String>,
    #[serde(default)]
    pub device_pubkey: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub client_info: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthLoginRequest {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub device_key: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default, alias = "group")]
    pub device_group: Option<String>,
    #[serde(default)]
    pub device_pubkey: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub client_info: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuthLogoutRequest {
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicUserView {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub must_change_password: bool,
    pub token_version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

impl From<UserRecord> for PublicUserView {
    fn from(u: UserRecord) -> Self {
        Self {
            id: u.id,
            email: u.email,
            display_name: u.display_name,
            role: u.role,
            status: u.status,
            must_change_password: u.must_change_password,
            token_version: u.token_version,
            created_at: u.created_at,
            updated_at: u.updated_at,
            last_login_at: u.last_login_at,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_sec: u64,
    pub user: PublicUserView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RefreshTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in_sec: u64,
    pub user: PublicUserView,
}

#[derive(Debug, Deserialize)]
pub struct BindDeviceRequest {
    #[serde(default)]
    pub device_id: Option<String>,
    pub device_key: String,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default, alias = "group")]
    pub device_group: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub device_pubkey: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMyDeviceRequest {
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub allow_remote_assist: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MyDeviceView {
    pub device_id: String,
    pub formatted_id: String,
    pub hostname: String,
    pub os: String,
    pub device_group: String,
    pub alias: String,
    pub allow_remote_assist: bool,
    pub disabled: bool,
    pub online: bool,
    pub in_session: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_udp_addr: Option<String>,
    pub lan_addrs: Vec<String>,
    pub bound_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MyDevicesResponse {
    pub devices: Vec<MyDeviceView>,
    pub total: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceBindingResponse {
    pub device: DeviceRecord,
}

#[derive(Debug, Deserialize)]
pub struct IssueCertRequest {
    pub device_id: String,
    #[serde(alias = "csr")]
    pub csr_pem: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IssueCertResponse {
    pub serial_hex: String,
    pub user_id: Uuid,
    pub device_id: String,
    pub fingerprint_sha256: String,
    pub cert_pem: String,
    pub ca_cert_pem: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminSettingsResponse {
    pub allow_registration: bool,
    pub cache_backend: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminSettingsRequest {
    pub allow_registration: bool,
}

#[derive(Debug, Deserialize)]
pub struct AdminUsersQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminUserView {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub status: String,
    pub must_change_password: bool,
    pub token_version: i64,
    pub device_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminUsersResponse {
    pub users: Vec<AdminUserView>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Deserialize)]
pub struct CreateAdminUserRequest {
    pub email: String,
    #[serde(default)]
    pub display_name: String,
    pub password: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub must_change_password: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminUserRequest {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reset_password: Option<String>,
    #[serde(default)]
    pub must_change_password: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminUserMutationResponse {
    pub user: AdminUserView,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub device_id: String,
    pub formatted_id: String,
    pub device_key: String,
    pub hostname: String,
    pub os: String,
    pub device_group: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub signal_tcp_addr: String,
    pub udp_probe_addr: String,
    pub tcp_relay_addr: String,
    pub relay_addr: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatsResponse {
    pub online_count: usize,
    pub registered_count: u64,
    pub active_relay_sessions: usize,
    pub total_relay_sessions: u64,
    pub total_relay_bytes: u64,
    pub total_bytes_up: u64,
    pub total_bytes_down: u64,
    pub punch_coord_count: u64,
    pub ephemeral_backend: String,
    pub relay_engine: String,
    pub uptime_sec: u64,
}

#[derive(Debug, Deserialize)]
pub struct DevicesQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceView {
    pub device_id: String,
    pub formatted_id: String,
    pub hostname: String,
    pub os: String,
    pub device_group: String,
    #[serde(default)]
    pub alias: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_email: Option<String>,
    #[serde(default)]
    pub allow_remote_assist: bool,
    pub disabled: bool,
    pub online: bool,
    pub in_session: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_udp_addr: Option<String>,
    pub lan_addrs: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceView>,
    pub total: usize,
    #[serde(default)]
    pub total_count: usize,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<RelaySessionInfo>,
    pub total: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KickSessionResponse {
    pub status: String,
    pub session_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DisableDeviceResponse {
    pub status: String,
    pub device_id: String,
    pub kicked_signal: bool,
    pub kicked_sessions: usize,
}

/// Build the Axum 0.8 REST API router (plus optional SPA static hosting).
pub fn router(state: AppState) -> Router {
    let cors_layer = build_cors_layer(&state.cors_origins).unwrap_or_else(|err| {
        warn!(error = %err, "invalid CORS configuration; falling back to same-origin only");
        build_cors_layer(&[]).expect("empty CORS allowlist is always valid")
    });

    let register_route = Router::new()
        .route("/api/v1/devices/register", post(post_register_device))
        .layer(DefaultBodyLimit::max(MAX_REGISTER_BODY_BYTES));

    let auth_public_routes = Router::new()
        .route("/api/v1/auth/config", get(get_auth_config))
        .route("/api/v1/auth/register", post(post_auth_register))
        .route("/api/v1/auth/login", post(post_auth_login))
        .route("/api/v1/auth/refresh", post(post_auth_refresh))
        .route("/api/v1/auth/logout", post(post_auth_logout))
        .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES));

    let public_routes = Router::new()
        .route("/api/v1/health", get(get_health))
        .route("/api/v1/pki/ca", get(get_pki_ca))
        .merge(register_route)
        .merge(auth_public_routes);

    let restricted_user_routes = Router::new()
        .route("/api/v1/me", get(get_me))
        .route("/api/v1/auth/change-password", post(post_change_password))
        .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_user_auth_allow_pwd_change,
        ));

    let full_user_routes = Router::new()
        .route("/api/v1/me/devices", get(get_my_devices))
        .route("/api/v1/me/devices/bind", post(post_bind_my_device))
        .route(
            "/api/v1/me/devices/{id}",
            patch(patch_my_device).delete(delete_my_device),
        )
        .route("/api/v1/certs/issue", post(post_issue_cert))
        .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_user_auth_full,
        ));

    let admin_routes = Router::new()
        .route(
            "/api/v1/admin/settings",
            get(get_admin_settings).patch(patch_admin_settings),
        )
        .route(
            "/api/v1/admin/users",
            get(get_admin_users).post(post_admin_user),
        )
        .route(
            "/api/v1/admin/users/{id}",
            patch(patch_admin_user).delete(delete_admin_user),
        )
        .route("/api/v1/stats", get(get_stats))
        .route("/api/v1/devices", get(get_devices))
        .route("/api/v1/sessions", get(get_sessions))
        .route("/api/v1/sessions/{id}", delete(delete_session))
        .route("/api/v1/devices/{id}", delete(delete_device))
        .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_auth,
        ));

    let api_fallback = Router::new().route("/api/{*rest}", axum::routing::any(api_not_found));

    let mut app = public_routes
        .merge(restricted_user_routes)
        .merge(full_user_routes)
        .merge(admin_routes)
        .merge(api_fallback);

    if let Some(ref web_dir) = state.web_dir {
        let index_file = web_dir.join("index.html");
        let serve_dir = ServeDir::new(web_dir).fallback(ServeFile::new(index_file));
        app = app.fallback_service(serve_dir);
    }

    app.layer(cors_layer)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn api_not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "API route not found")
}

/// Serve `app` over TLS (`tokio_rustls::TlsAcceptor`) with `ConnectInfo<SocketAddr>`
/// injected into request extensions and coordinated graceful shutdown.
pub async fn serve_https(
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    app: Router,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let builder = AutoHttpBuilder::new(TokioExecutor::new());

    loop {
        if *shutdown.borrow_and_update() {
            return Ok(());
        }

        let accepted = tokio::select! {
            biased;
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow_and_update() {
                    return Ok(());
                }
                continue;
            }
            res = listener.accept() => res,
        };

        let (tcp_stream, peer_addr) = match accepted {
            Ok(pair) => pair,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(err) => {
                warn!(error = %err, "HTTPS accept error; backing off 50ms");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        let acceptor = acceptor.clone();
        let builder = builder.clone();
        let app = app.clone();
        let mut conn_shutdown = shutdown.clone();

        tokio::spawn(async move {
            let tls_res =
                tokio::time::timeout(HTTPS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp_stream)).await;
            let tls_stream = match tls_res {
                Ok(Ok(s)) => s,
                Ok(Err(err)) => {
                    debug!(%peer_addr, error = %err, "HTTPS TLS handshake failed");
                    return;
                }
                Err(_) => {
                    debug!(%peer_addr, "HTTPS TLS handshake timed out");
                    return;
                }
            };

            let svc = TowerToHyperService::new(app.map_request(
                move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(ConnectInfo(peer_addr));
                    req
                },
            ));
            let io = TokioIo::new(tls_stream);
            let conn = builder.serve_connection_with_upgrades(io, svc);
            tokio::pin!(conn);

            tokio::select! {
                res = conn.as_mut() => {
                    if let Err(err) = res {
                        debug!(%peer_addr, error = %err, "HTTPS connection ended with error");
                    }
                }
                _ = async {
                    if *conn_shutdown.borrow_and_update() {
                        return;
                    }
                    while conn_shutdown.changed().await.is_ok() {
                        if *conn_shutdown.borrow_and_update() {
                            break;
                        }
                    }
                } => {
                    conn.as_mut().graceful_shutdown();
                    let _ = tokio::time::timeout(Duration::from_secs(2), conn).await;
                }
            }
        });
    }
}

async fn get_health(State(state): State<AppState>) -> impl IntoResponse {
    let pg_ok = match state.device_store.health_check().await {
        Ok(()) => true,
        Err(err) => {
            warn!(error = %err, "postgres health check failed");
            false
        }
    };
    let eph_ok = match state.ephemeral.health_check().await {
        Ok(()) => true,
        Err(err) => {
            warn!(error = %err, "ephemeral store health check failed");
            false
        }
    };
    let allow_registration = state
        .settings_cache
        .get_allow_registration()
        .await
        .unwrap_or(false);

    let healthy = pg_ok && eph_ok;
    let status_code = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let backend_name = state.ephemeral.backend_name().to_string();

    (
        status_code,
        Json(HealthResponse {
            status: if healthy { "ok" } else { "degraded" }.to_string(),
            postgres: if pg_ok { "ok" } else { "error" }.to_string(),
            ephemeral_backend: backend_name.clone(),
            cache_backend: backend_name,
            relay_engine: state.relay_server.engine().to_string(),
            tls_enabled: state.tls_enabled,
            ca_fingerprint_sha256: state.pki_authority.ca_fingerprint_sha256().to_string(),
            allow_registration,
            uptime_sec: state.started_at.elapsed().as_secs(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }),
    )
}

async fn get_pki_ca(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(PkiCaResponse {
            ca_cert_pem: state.pki_authority.ca_cert_pem().to_string(),
            ca_fingerprint_sha256: state.pki_authority.ca_fingerprint_sha256().to_string(),
            auth_pubkey_hex: state.auth_authority.public_key_hex(),
        }),
    )
}

async fn get_auth_config(State(state): State<AppState>) -> Response {
    match state.settings_cache.get_allow_registration().await {
        Ok(allow_registration) => (
            StatusCode::OK,
            Json(AuthConfigResponse { allow_registration }),
        )
            .into_response(),
        Err(err) => map_store_error(err, "get_auth_config"),
    }
}

fn client_ip_from_headers_and_ext(req_ip: Option<&ConnectInfo<SocketAddr>>) -> Option<IpAddr> {
    req_ip.map(|c| c.0.ip())
}

async fn post_auth_register(
    State(state): State<AppState>,
    conn_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    ApiJson(req): ApiJson<AuthRegisterRequest>,
) -> Response {
    let client_ip = client_ip_from_headers_and_ext(conn_info.as_ref().map(|e| &e.0));
    if !state.auth_rate_limiter.check_ip(client_ip) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many authentication attempts from this IP",
        );
    }

    match state.settings_cache.get_allow_registration().await {
        Ok(true) => {}
        Ok(false) => {
            return api_error(
                StatusCode::FORBIDDEN,
                "registration_disabled",
                "self-service user registration is currently disabled",
            );
        }
        Err(err) => return map_store_error(err, "check_allow_registration"),
    }

    let email = match validate_email(&req.email) {
        Ok(e) => e,
        Err(err) => return map_store_error(err, "validate_email"),
    };
    if !state.auth_rate_limiter.check_email(&email) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many authentication attempts for this email",
        );
    }

    if let Err(msg) = validate_password_policy(&req.password) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
    }

    if let Err(resp) = validate_optional_auth_device_fields(
        req.device_key.as_deref(),
        req.hostname.as_deref(),
        req.os.as_deref(),
        req.device_group.as_deref(),
        req.alias.as_deref(),
        req.device_pubkey.as_deref(),
        req.client_info.as_deref(),
    ) {
        return *resp;
    }
    if let Some(dk) = req
        .device_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Ok(Some(existing_dev)) = state.device_store.get_by_key(dk).await {
            if existing_dev.disabled {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "device_disabled",
                    format!("device {} is disabled", existing_dev.device_id),
                );
            }
        }
    }

    let password_hash = match hash_password(&req.password) {
        Ok(h) => h,
        Err(err) => {
            warn!(error = %err, "password hashing failed");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to process password",
            );
        }
    };

    let user = match state
        .auth_store
        .create_user(CreateUserInput {
            email: email.clone(),
            display_name: req.display_name.clone(),
            password_hash,
            role: ROLE_USER.to_string(),
            status: STATUS_ACTIVE.to_string(),
            must_change_password: false,
        })
        .await
    {
        Ok(u) => u,
        Err(err) => return map_store_error(err, "create_user_register"),
    };

    let device = match req
        .device_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(dk) => {
            match register_and_bind_device_for_user(
                &state,
                user.id,
                BindDeviceParams {
                    device_key: dk,
                    hostname: req.hostname.as_deref(),
                    os: req.os.as_deref(),
                    device_group: req.device_group.as_deref(),
                    alias: req.alias.as_deref(),
                    device_pubkey: req.device_pubkey.as_deref(),
                },
            )
            .await
            {
                Ok(dev) => Some(dev),
                Err(resp) => return *resp,
            }
        }
        None => None,
    };

    let bound_device_id = device.as_ref().map(|d| d.device_id.clone());
    let refresh_token = generate_refresh_token();
    let refresh_hash = hash_token_sha256_hex(&refresh_token);
    let expires_at =
        Utc::now() + chrono::Duration::from_std(REFRESH_TOKEN_TTL).expect("TTL fits chrono");

    let session = match state
        .auth_store
        .create_user_session(CreateSessionInput {
            user_id: user.id,
            device_id: bound_device_id.clone(),
            family_id: None,
            token_hash: refresh_hash,
            expires_at,
            client_info: req.client_info.unwrap_or_default(),
        })
        .await
    {
        Ok(s) => s,
        Err(err) => return map_store_error(err, "create_session_register"),
    };

    let access_token = match state.auth_authority.issue_access_token_for_user(
        &user,
        session.id,
        bound_device_id.as_deref(),
        ACCESS_TOKEN_TTL,
    ) {
        Ok(t) => t,
        Err(err) => {
            warn!(error = %err, "failed to sign JWT");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to issue access token",
            );
        }
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(user.id),
            bound_device_id.as_deref(),
            "auth.register",
            serde_json::json!({ "email": user.email }),
        )
        .await;

    (
        StatusCode::OK,
        Json(AuthTokenResponse {
            access_token,
            refresh_token,
            expires_in_sec: ACCESS_TOKEN_TTL.as_secs(),
            user: user.into(),
            device,
        }),
    )
        .into_response()
}

async fn post_auth_login(
    State(state): State<AppState>,
    conn_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    ApiJson(req): ApiJson<AuthLoginRequest>,
) -> Response {
    let client_ip = client_ip_from_headers_and_ext(conn_info.as_ref().map(|e| &e.0));
    if !state.auth_rate_limiter.check_ip(client_ip) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many authentication attempts from this IP",
        );
    }

    let email = normalize_email(&req.email);
    if email.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "email is required",
        );
    }
    if !state.auth_rate_limiter.check_email(&email) {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many authentication attempts for this email",
        );
    }

    let user = match state.auth_store.get_user_by_email(&email).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "invalid email or password",
            );
        }
        Err(err) => return map_store_error(err, "get_user_by_email_login"),
    };

    let password_ok = verify_password(&req.password, &user.password_hash).unwrap_or(false);
    if !password_ok {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "invalid email or password",
        );
    }

    if user.status != STATUS_ACTIVE {
        return api_error(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "user account is disabled",
        );
    }

    if let Err(resp) = validate_optional_auth_device_fields(
        req.device_key.as_deref(),
        req.hostname.as_deref(),
        req.os.as_deref(),
        req.device_group.as_deref(),
        req.alias.as_deref(),
        req.device_pubkey.as_deref(),
        req.client_info.as_deref(),
    ) {
        return *resp;
    }

    let device = if user.must_change_password {
        None
    } else if let Some(dk) = req
        .device_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match register_and_bind_device_for_user(
            &state,
            user.id,
            BindDeviceParams {
                device_key: dk,
                hostname: req.hostname.as_deref(),
                os: req.os.as_deref(),
                device_group: req.device_group.as_deref(),
                alias: req.alias.as_deref(),
                device_pubkey: req.device_pubkey.as_deref(),
            },
        )
        .await
        {
            Ok(dev) => Some(dev),
            Err(resp) => return *resp,
        }
    } else {
        None
    };

    let bound_device_id = device.as_ref().map(|d| d.device_id.clone());
    let refresh_token = generate_refresh_token();
    let refresh_hash = hash_token_sha256_hex(&refresh_token);
    let expires_at =
        Utc::now() + chrono::Duration::from_std(REFRESH_TOKEN_TTL).expect("TTL fits chrono");

    let session = match state
        .auth_store
        .create_user_session(CreateSessionInput {
            user_id: user.id,
            device_id: bound_device_id.clone(),
            family_id: None,
            token_hash: refresh_hash,
            expires_at,
            client_info: req.client_info.unwrap_or_default(),
        })
        .await
    {
        Ok(s) => s,
        Err(err) => return map_store_error(err, "create_session_login"),
    };

    if let Err(err) = state.auth_store.touch_user_last_login(user.id).await {
        warn!(user_id = %user.id, error = %err, "failed to update last_login_at");
    }
    let user = state
        .auth_store
        .get_user_by_id(user.id)
        .await
        .ok()
        .flatten()
        .unwrap_or(user);

    let access_token = match state.auth_authority.issue_access_token_for_user(
        &user,
        session.id,
        bound_device_id.as_deref(),
        ACCESS_TOKEN_TTL,
    ) {
        Ok(t) => t,
        Err(err) => {
            warn!(error = %err, "failed to sign JWT");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to issue access token",
            );
        }
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(user.id),
            bound_device_id.as_deref(),
            "auth.login",
            serde_json::json!({
                "email": user.email,
                "must_change_password": user.must_change_password,
            }),
        )
        .await;

    (
        StatusCode::OK,
        Json(AuthTokenResponse {
            access_token,
            refresh_token,
            expires_in_sec: ACCESS_TOKEN_TTL.as_secs(),
            user: user.into(),
            device,
        }),
    )
        .into_response()
}

async fn post_auth_refresh(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<AuthRefreshRequest>,
) -> Response {
    let presented = req.refresh_token.trim();
    if presented.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "refresh_token is required",
        );
    }

    let presented_hash = hash_token_sha256_hex(presented);
    let new_refresh_token = generate_refresh_token();
    let new_hash = hash_token_sha256_hex(&new_refresh_token);
    let new_expires_at =
        Utc::now() + chrono::Duration::from_std(REFRESH_TOKEN_TTL).expect("TTL fits chrono");

    let session = match state
        .auth_store
        .rotate_refresh_token(
            &presented_hash,
            &new_hash,
            new_expires_at,
            crate::auth::REFRESH_GRACE_PERIOD,
        )
        .await
    {
        Ok(sess) => sess,
        Err(StoreError::TokenReplayDetected { family_id, user_id }) => {
            let _ = state
                .auth_store
                .append_audit_log(
                    Some(user_id),
                    None,
                    "auth.refresh_replay_detected",
                    serde_json::json!({ "family_id": family_id }),
                )
                .await;
            return api_error(
                StatusCode::UNAUTHORIZED,
                "token_reuse_detected",
                "refresh token replay detected; session family revoked",
            );
        }
        Err(StoreError::Unauthorized(msg)) => {
            return api_error(StatusCode::UNAUTHORIZED, "invalid_refresh_token", msg);
        }
        Err(err) => return map_store_error(err, "rotate_refresh_token"),
    };

    let user = match state.auth_store.get_user_by_id(session.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            let _ = state.auth_store.revoke_session(session.id).await;
            return api_error(
                StatusCode::UNAUTHORIZED,
                "invalid_refresh_token",
                "user account not found",
            );
        }
        Err(err) => return map_store_error(err, "get_user_refresh"),
    };

    if user.status != STATUS_ACTIVE {
        let _ = state
            .auth_store
            .revoke_session_family(session.family_id)
            .await;
        return api_error(
            StatusCode::FORBIDDEN,
            "account_disabled",
            "user account is disabled",
        );
    }

    let access_token = match state.auth_authority.issue_access_token_for_user(
        &user,
        session.id,
        session.device_id.as_deref(),
        ACCESS_TOKEN_TTL,
    ) {
        Ok(t) => t,
        Err(err) => {
            warn!(error = %err, "failed to sign JWT during refresh");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to issue access token",
            );
        }
    };

    (
        StatusCode::OK,
        Json(RefreshTokenResponse {
            access_token,
            refresh_token: new_refresh_token,
            expires_in_sec: ACCESS_TOKEN_TTL.as_secs(),
            user: user.into(),
        }),
    )
        .into_response()
}

async fn logout_cleanup_owned_device(
    state: &AppState,
    user_id: Uuid,
    explicit_did: Option<&str>,
    fallback_did: Option<&str>,
) -> Result<Option<String>, Box<Response>> {
    let target_did = if let Some(did) = explicit_did {
        match state.device_store.get_device_owner(did).await {
            Ok(Some(owner)) if owner == user_id => Some(did.to_string()),
            Ok(_) => {
                return Err(Box::new(api_error(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "device is not owned by the authenticated user",
                )));
            }
            Err(err) => {
                return Err(Box::new(map_store_error(err, "verify_logout_device_owner")));
            }
        }
    } else if let Some(did) = fallback_did {
        match state.device_store.get_device_owner(did).await {
            Ok(Some(owner)) if owner == user_id => Some(did.to_string()),
            Ok(_) => None,
            Err(err) => {
                return Err(Box::new(map_store_error(err, "verify_logout_device_owner")));
            }
        }
    } else {
        None
    };

    if let Some(ref did) = target_did {
        if let Ok(certs) = state
            .auth_store
            .list_device_certs(Some(user_id), Some(did))
            .await
        {
            for cert in certs {
                if cert.revoked_at.is_none() {
                    if let Ok(Some(revoked)) = state
                        .auth_store
                        .revoke_cert_by_serial(&cert.serial_hex, Some("user_logout"))
                        .await
                    {
                        revoke_cert_list_in_ephemeral(&*state.ephemeral, &[revoked]).await;
                    }
                }
            }
        }
        let _ = state.auth_store.revoke_device_sessions(user_id, did).await;
        let _ = state.signal_hub.kick_device(did, "user_logout").await;
        let _ = state.relay_server.kick_device_sessions(did);
    }

    Ok(target_did)
}

async fn verify_explicit_device_owner(
    state: &AppState,
    user_id: Uuid,
    explicit_did: Option<&str>,
) -> Result<(), Box<Response>> {
    if let Some(did) = explicit_did {
        match state.device_store.get_device_owner(did).await {
            Ok(Some(owner)) if owner == user_id => Ok(()),
            Ok(_) => Err(Box::new(api_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "device is not owned by the authenticated user",
            ))),
            Err(err) => Err(Box::new(map_store_error(err, "verify_logout_device_owner"))),
        }
    } else {
        Ok(())
    }
}

async fn logout_via_refresh_token(
    state: &AppState,
    refresh_token: &str,
    explicit_did: Option<&str>,
) -> Result<Option<(Uuid, Uuid, Option<String>, bool)>, Box<Response>> {
    let token_hash = hash_token_sha256_hex(refresh_token);
    if explicit_did.is_some() {
        let Some(sess) = state
            .auth_store
            .get_active_session_by_refresh_hash(&token_hash)
            .await
            .map_err(|e| Box::new(map_store_error(e, "lookup_session_by_refresh")))?
        else {
            return Ok(None);
        };
        verify_explicit_device_owner(state, sess.user_id, explicit_did).await?;
    }

    let Some(sess) = state
        .auth_store
        .revoke_session_by_refresh_hash(&token_hash)
        .await
        .map_err(|e| Box::new(map_store_error(e, "revoke_session_by_refresh")))?
    else {
        return Ok(None);
    };
    let is_active = state
        .auth_store
        .get_user_by_id(sess.user_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|u| u.status == STATUS_ACTIVE);
    Ok(Some((sess.user_id, sess.id, sess.device_id, is_active)))
}

async fn post_auth_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req: AuthLogoutRequest = if body.is_empty() {
        AuthLogoutRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_json_syntax",
                    err.to_string(),
                );
            }
        }
    };

    let explicit_did = req
        .device_id
        .as_deref()
        .map(normalize_device_id)
        .filter(|d| is_valid_device_id(d));
    if req
        .device_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|raw| !raw.is_empty())
        && explicit_did.is_none()
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "device_id must be a valid 9-digit device id",
        );
    }

    let (logout_user_id, logout_session_id, fallback_did, run_device_cleanup) =
        if extract_bearer_token(&headers).is_some() {
            match authenticate_bearer_user(&state, &headers).await {
                Ok(authed) => {
                    if let Err(resp) = verify_explicit_device_owner(
                        &state,
                        authed.user.id,
                        explicit_did.as_deref(),
                    )
                    .await
                    {
                        return *resp;
                    }
                    if let Err(err) = state.auth_store.revoke_session(authed.claims.sid).await {
                        return map_store_error(err, "revoke_session_logout");
                    }
                    (authed.user.id, authed.claims.sid, authed.claims.did, true)
                }
                Err(bearer_err_resp) => {
                    let Some(rt) = req
                        .refresh_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    else {
                        return *bearer_err_resp;
                    };
                    match logout_via_refresh_token(&state, rt, explicit_did.as_deref()).await {
                        Ok(Some(res)) => res,
                        Ok(None) => return *bearer_err_resp,
                        Err(resp) => return *resp,
                    }
                }
            }
        } else {
            let Some(rt) = req
                .refresh_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return unauthorized_response(
                    "unauthorized",
                    "missing bearer token or refresh_token",
                );
            };
            match logout_via_refresh_token(&state, rt, explicit_did.as_deref()).await {
                Ok(Some(res)) => res,
                Ok(None) => {
                    return unauthorized_response(
                        "unauthorized",
                        "invalid or expired refresh_token",
                    );
                }
                Err(resp) => return *resp,
            }
        };

    let kicked_did = if run_device_cleanup {
        match logout_cleanup_owned_device(
            &state,
            logout_user_id,
            explicit_did.as_deref(),
            fallback_did.as_deref(),
        )
        .await
        {
            Ok(did) => did,
            Err(resp) => return *resp,
        }
    } else {
        None
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(logout_user_id),
            kicked_did.as_deref(),
            "auth.logout",
            serde_json::json!({ "session_id": logout_session_id }),
        )
        .await;

    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response()
}

async fn post_change_password(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
    ApiJson(req): ApiJson<ChangePasswordRequest>,
) -> Response {
    let current_ok =
        verify_password(&req.current_password, &authed.user.password_hash).unwrap_or(false);
    if !current_ok {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_current_password",
            "current password is incorrect",
        );
    }

    if let Err(msg) = validate_password_policy(&req.new_password) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
    }
    if req.new_password == req.current_password {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "new password must differ from current password",
        );
    }

    let new_hash = match hash_password(&req.new_password) {
        Ok(h) => h,
        Err(err) => {
            warn!(error = %err, "password hashing failed");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to process password",
            );
        }
    };

    let updated_user = match state
        .auth_store
        .update_user(
            authed.user.id,
            UpdateUserInput {
                password_hash: Some(new_hash),
                must_change_password: Some(false),
                bump_token_version: true,
                ..Default::default()
            },
        )
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "user_not_found", "user not found");
        }
        Err(err) => return map_store_error(err, "update_user_password"),
    };

    let _ = state.auth_store.revoke_user_sessions(updated_user.id).await;
    if let Ok(revoked_certs) = state
        .auth_store
        .revoke_user_certs(updated_user.id, Some("password_changed"))
        .await
    {
        revoke_cert_list_in_ephemeral(&*state.ephemeral, &revoked_certs).await;
    }
    let caller_did = authed.claims.did.as_deref();
    if let Ok(devs) = state.auth_store.list_user_devices(updated_user.id).await {
        for dev in devs {
            let _ = state.relay_server.kick_device_sessions(&dev.device_id);
            if caller_did != Some(dev.device_id.as_str()) {
                let _ = state
                    .signal_hub
                    .kick_device(&dev.device_id, "password_changed")
                    .await;
            }
        }
    }

    let refresh_token = generate_refresh_token();
    let refresh_hash = hash_token_sha256_hex(&refresh_token);
    let expires_at =
        Utc::now() + chrono::Duration::from_std(REFRESH_TOKEN_TTL).expect("TTL fits chrono");

    let new_session = match state
        .auth_store
        .create_user_session(CreateSessionInput {
            user_id: updated_user.id,
            device_id: authed.claims.did.clone(),
            family_id: None,
            token_hash: refresh_hash,
            expires_at,
            client_info: "change-password".to_string(),
        })
        .await
    {
        Ok(s) => s,
        Err(err) => return map_store_error(err, "create_session_change_password"),
    };

    let access_token = match state.auth_authority.issue_access_token_for_user(
        &updated_user,
        new_session.id,
        authed.claims.did.as_deref(),
        ACCESS_TOKEN_TTL,
    ) {
        Ok(t) => t,
        Err(err) => {
            warn!(error = %err, "failed to sign JWT after password change");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to issue access token",
            );
        }
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(updated_user.id),
            authed.claims.did.as_deref(),
            "auth.change_password",
            serde_json::json!({ "token_version": updated_user.token_version }),
        )
        .await;

    (
        StatusCode::OK,
        Json(RefreshTokenResponse {
            access_token,
            refresh_token,
            expires_in_sec: ACCESS_TOKEN_TTL.as_secs(),
            user: updated_user.into(),
        }),
    )
        .into_response()
}

async fn get_me(Extension(authed): Extension<AuthenticatedUser>) -> impl IntoResponse {
    (StatusCode::OK, Json(PublicUserView::from(authed.user)))
}

async fn get_my_devices(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
) -> Response {
    let records = match state.auth_store.list_user_devices(authed.user.id).await {
        Ok(list) => list,
        Err(err) => return map_store_error(err, "list_user_devices"),
    };

    let online_set: HashSet<String> = state
        .ephemeral
        .online_ids()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let in_session_set = state.relay_server.in_session_device_ids();

    let mut devices = Vec::with_capacity(records.len());
    for rec in records {
        let in_session = in_session_set.contains(&rec.device_id);
        let online = online_set.contains(&rec.device_id)
            || state.signal_hub.is_peer_connected(&rec.device_id);
        let status = if in_session {
            "in_session"
        } else if online {
            "online"
        } else {
            "offline"
        };
        let public_udp_addr = if online || in_session {
            state
                .ephemeral
                .get_public_udp(&rec.device_id)
                .await
                .ok()
                .flatten()
                .map(|a| a.to_string())
        } else {
            None
        };
        let lan_addrs = if online {
            state
                .signal_hub
                .peer_lan_addrs(&rec.device_id)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        devices.push(MyDeviceView {
            formatted_id: format_device_id(&rec.device_id),
            device_id: rec.device_id,
            hostname: rec.hostname,
            os: rec.os,
            device_group: rec.device_group,
            alias: rec.alias,
            allow_remote_assist: rec.allow_remote_assist,
            disabled: rec.disabled,
            online,
            in_session,
            status: status.to_string(),
            public_udp_addr,
            lan_addrs,
            bound_at: rec.bound_at,
            created_at: rec.created_at,
            last_seen_at: rec.last_seen_at,
        });
    }

    let total = devices.len();
    (StatusCode::OK, Json(MyDevicesResponse { devices, total })).into_response()
}

async fn post_bind_my_device(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
    ApiJson(req): ApiJson<BindDeviceRequest>,
) -> Response {
    let dk = req.device_key.trim();
    if dk.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "device_key is required",
        );
    }

    if let Some(ref raw_did) = req.device_id {
        let norm_did = normalize_device_id(raw_did);
        if !is_valid_device_id(&norm_did) {
            return api_error(
                StatusCode::BAD_REQUEST,
                "invalid_input",
                "invalid device_id",
            );
        }
        match state.device_store.get_by_id(&norm_did).await {
            Ok(Some(existing)) if existing.device_key == dk => {
                if existing.disabled {
                    return api_error(
                        StatusCode::FORBIDDEN,
                        "device_disabled",
                        format!("device {norm_did} is disabled"),
                    );
                }
            }
            Ok(Some(_)) | Ok(None) => {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "invalid_device_key",
                    "device_id does not match device_key",
                );
            }
            Err(err) => return map_store_error(err, "verify_bind_device"),
        }
    }

    let device = match register_and_bind_device_for_user(
        &state,
        authed.user.id,
        BindDeviceParams {
            device_key: dk,
            hostname: req.hostname.as_deref(),
            os: req.os.as_deref(),
            device_group: req.device_group.as_deref(),
            alias: req.alias.as_deref(),
            device_pubkey: req.device_pubkey.as_deref(),
        },
    )
    .await
    {
        Ok(d) => d,
        Err(resp) => return *resp,
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(authed.user.id),
            Some(&device.device_id),
            "device.bind",
            serde_json::json!({ "hostname": device.hostname }),
        )
        .await;

    (StatusCode::OK, Json(DeviceBindingResponse { device })).into_response()
}

async fn patch_my_device(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<UpdateMyDeviceRequest>,
) -> Response {
    let norm = normalize_device_id(&id);
    if !is_valid_device_id(&norm) {
        return api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            format!("device {id} not found"),
        );
    }

    let existing = match state.device_store.get_by_id(&norm).await {
        Ok(Some(d)) if d.owner_user_id == Some(authed.user.id) => d,
        Ok(_) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "device_not_found",
                format!("device {norm} not found"),
            );
        }
        Err(err) => return map_store_error(err, "get_my_device_patch"),
    };

    if let Some(ref alias) = req.alias {
        match state
            .auth_store
            .update_device_alias(&norm, authed.user.id, alias)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return api_error(
                    StatusCode::NOT_FOUND,
                    "device_not_found",
                    format!("device {norm} not found"),
                );
            }
            Err(err) => return map_store_error(err, "update_device_alias"),
        }
    }

    if let Some(allow) = req.allow_remote_assist {
        if let Err(err) = state
            .auth_store
            .set_device_allow_remote_assist(&norm, Some(authed.user.id), allow)
            .await
        {
            return map_store_error(err, "set_device_allow_remote_assist");
        }
    }

    let updated = state
        .device_store
        .get_by_id(&norm)
        .await
        .ok()
        .flatten()
        .unwrap_or(existing);

    (
        StatusCode::OK,
        Json(DeviceBindingResponse { device: updated }),
    )
        .into_response()
}

async fn delete_my_device(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> Response {
    let norm = normalize_device_id(&id);
    if !is_valid_device_id(&norm) {
        return api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            format!("device {id} not found"),
        );
    }

    match state.auth_store.unbind_device(&norm, authed.user.id).await {
        Ok(true) => {}
        Ok(false) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "device_not_found",
                format!("device {norm} not found"),
            );
        }
        Err(err) => return map_store_error(err, "unbind_device"),
    }

    if let Ok(revoked_certs) = state
        .auth_store
        .revoke_device_certs(&norm, Some("device_unbound"))
        .await
    {
        revoke_cert_list_in_ephemeral(&*state.ephemeral, &revoked_certs).await;
    }
    let _ = state
        .auth_store
        .revoke_device_sessions(authed.user.id, &norm)
        .await;
    let _ = state.signal_hub.kick_device(&norm, "device_unbound").await;
    let _ = state.relay_server.kick_device_sessions(&norm);

    let _ = state
        .auth_store
        .append_audit_log(
            Some(authed.user.id),
            Some(&norm),
            "device.unbind",
            serde_json::json!({}),
        )
        .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "unbound",
            "device_id": norm,
        })),
    )
        .into_response()
}

async fn post_issue_cert(
    State(state): State<AppState>,
    Extension(authed): Extension<AuthenticatedUser>,
    ApiJson(req): ApiJson<IssueCertRequest>,
) -> Response {
    let norm = normalize_device_id(&req.device_id);
    if !is_valid_device_id(&norm) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "invalid device_id",
        );
    }

    match state.device_store.get_by_id(&norm).await {
        Ok(Some(dev)) => {
            if dev.disabled {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "device_disabled",
                    format!("device {norm} is disabled"),
                );
            }
            if dev.owner_user_id != Some(authed.user.id) {
                return api_error(
                    StatusCode::FORBIDDEN,
                    "device_not_owned",
                    "device is not bound to the current user",
                );
            }
        }
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "device_not_found",
                format!("device {norm} not found"),
            );
        }
        Err(err) => return map_store_error(err, "get_device_issue_cert"),
    }

    let issued = match state
        .pki_authority
        .issue_and_persist_device_cert(
            &*state.auth_store,
            req.csr_pem.as_bytes(),
            authed.user.id,
            &norm,
        )
        .await
    {
        Ok(c) => c,
        Err(PkiError::InvalidCsr(msg)) | Err(PkiError::InvalidInput(msg)) => {
            return api_error(StatusCode::BAD_REQUEST, "invalid_csr", msg);
        }
        Err(PkiError::Store(err)) => return map_store_error(err, "persist_device_cert"),
        Err(err) => {
            warn!(error = %err, "failed to issue device certificate");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "pki_error",
                "failed to issue device certificate",
            );
        }
    };

    let _ = state
        .auth_store
        .append_audit_log(
            Some(authed.user.id),
            Some(&norm),
            "cert.issue",
            serde_json::json!({
                "serial_hex": issued.serial_hex,
                "fingerprint_sha256": issued.fingerprint_sha256,
            }),
        )
        .await;

    (
        StatusCode::OK,
        Json(IssueCertResponse {
            serial_hex: issued.serial_hex,
            user_id: issued.user_id,
            device_id: issued.device_id,
            fingerprint_sha256: issued.fingerprint_sha256,
            cert_pem: issued.cert_pem,
            ca_cert_pem: issued.ca_cert_pem,
            issued_at: issued.issued_at,
            expires_at: issued.expires_at,
        }),
    )
        .into_response()
}

async fn get_admin_settings(State(state): State<AppState>) -> Response {
    match state
        .settings_cache
        .get_allow_registration_with_meta()
        .await
    {
        Ok((allow_registration, updated_at)) => (
            StatusCode::OK,
            Json(AdminSettingsResponse {
                allow_registration,
                cache_backend: state.ephemeral.backend_name().to_string(),
                updated_at: updated_at.unwrap_or_else(Utc::now),
            }),
        )
            .into_response(),
        Err(err) => map_store_error(err, "get_admin_settings"),
    }
}

async fn patch_admin_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<AdminActor>,
    ApiJson(req): ApiJson<UpdateAdminSettingsRequest>,
) -> Response {
    let actor_uid = match actor {
        AdminActor::Jwt(ref u) => Some(u.user.id),
        AdminActor::StaticToken => None,
    };

    if let Err(err) = state
        .settings_cache
        .set_allow_registration(req.allow_registration, actor_uid)
        .await
    {
        return map_store_error(err, "set_allow_registration");
    }

    let _ = state
        .auth_store
        .append_audit_log(
            actor_uid,
            None,
            "admin.settings_update",
            serde_json::json!({ "allow_registration": req.allow_registration }),
        )
        .await;

    (
        StatusCode::OK,
        Json(AdminSettingsResponse {
            allow_registration: req.allow_registration,
            cache_backend: state.ephemeral.backend_name().to_string(),
            updated_at: Utc::now(),
        }),
    )
        .into_response()
}

fn to_admin_user_view(user: UserRecord, device_count: u64) -> AdminUserView {
    AdminUserView {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        role: user.role,
        status: user.status,
        must_change_password: user.must_change_password,
        token_version: user.token_version,
        device_count,
        created_at: user.created_at,
        updated_at: user.updated_at,
        last_login_at: user.last_login_at,
    }
}

async fn get_admin_users(
    State(state): State<AppState>,
    Query(query): Query<AdminUsersQuery>,
) -> Response {
    let role_filter = match query
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(r) => match validate_role(r) {
            Ok(v) => Some(v),
            Err(StoreError::InvalidInput(msg)) => {
                return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
            }
            Err(err) => return map_store_error(err, "validate_role_filter"),
        },
        None => None,
    };
    let status_filter = match query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => match validate_status(s) {
            Ok(v) => Some(v),
            Err(StoreError::InvalidInput(msg)) => {
                return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
            }
            Err(err) => return map_store_error(err, "validate_status_filter"),
        },
        None => None,
    };

    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let offset = query.offset.unwrap_or(0);

    let mut users = Vec::with_capacity(limit.min(64));

    if role_filter.is_none() && status_filter.is_none() {
        let rows = match state
            .auth_store
            .list_users(query.q.as_deref(), limit, offset)
            .await
        {
            Ok(list) => list,
            Err(err) => return map_store_error(err, "list_users"),
        };
        let total = state
            .auth_store
            .count_users(query.q.as_deref())
            .await
            .unwrap_or(rows.len() as u64) as usize;
        for row in rows {
            users.push(to_admin_user_view(row.user, row.device_count));
        }
        return (
            StatusCode::OK,
            Json(AdminUsersResponse {
                users,
                total,
                limit,
                offset,
            }),
        )
            .into_response();
    }

    let mut db_offset = 0usize;
    let mut matched_total = 0usize;
    let batch_size = MAX_LIST_LIMIT;

    loop {
        let batch = match state
            .auth_store
            .list_users(query.q.as_deref(), batch_size, db_offset)
            .await
        {
            Ok(list) => list,
            Err(err) => return map_store_error(err, "list_users_filtered"),
        };
        let batch_len = batch.len();
        if batch_len == 0 {
            break;
        }

        for row in batch {
            if let Some(ref rf) = role_filter {
                if &row.user.role != rf {
                    continue;
                }
            }
            if let Some(ref sf) = status_filter {
                if &row.user.status != sf {
                    continue;
                }
            }
            if matched_total >= offset && users.len() < limit {
                users.push(to_admin_user_view(row.user, row.device_count));
            }
            matched_total += 1;
        }

        if batch_len < batch_size {
            break;
        }
        db_offset = db_offset.saturating_add(batch_len);
    }

    (
        StatusCode::OK,
        Json(AdminUsersResponse {
            users,
            total: matched_total,
            limit,
            offset,
        }),
    )
        .into_response()
}

async fn post_admin_user(
    State(state): State<AppState>,
    Extension(actor): Extension<AdminActor>,
    ApiJson(req): ApiJson<CreateAdminUserRequest>,
) -> Response {
    let email = match validate_email(&req.email) {
        Ok(e) => e,
        Err(err) => return map_store_error(err, "admin_create_user_email"),
    };
    if let Err(msg) = validate_password_policy(&req.password) {
        return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
    }
    let role = match validate_role(req.role.as_deref().unwrap_or(ROLE_USER)) {
        Ok(r) => r,
        Err(err) => return map_store_error(err, "admin_create_user_role"),
    };
    let status = match validate_status(req.status.as_deref().unwrap_or(STATUS_ACTIVE)) {
        Ok(s) => s,
        Err(err) => return map_store_error(err, "admin_create_user_status"),
    };
    let password_hash = match hash_password(&req.password) {
        Ok(h) => h,
        Err(err) => {
            warn!(error = %err, "password hashing failed");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to process password",
            );
        }
    };

    let created = match state
        .auth_store
        .create_user(CreateUserInput {
            email,
            display_name: req.display_name,
            password_hash,
            role,
            status,
            must_change_password: req.must_change_password.unwrap_or(false),
        })
        .await
    {
        Ok(u) => u,
        Err(err) => return map_store_error(err, "admin_create_user"),
    };

    let actor_uid = match actor {
        AdminActor::Jwt(ref u) => Some(u.user.id),
        AdminActor::StaticToken => None,
    };
    let _ = state
        .auth_store
        .append_audit_log(
            actor_uid,
            None,
            "admin.user_create",
            serde_json::json!({
                "target_user_id": created.id,
                "email": created.email,
                "role": created.role,
            }),
        )
        .await;

    (
        StatusCode::OK,
        Json(AdminUserMutationResponse {
            user: to_admin_user_view(created, 0),
        }),
    )
        .into_response()
}

async fn patch_admin_user(
    State(state): State<AppState>,
    Extension(actor): Extension<AdminActor>,
    Path(user_id): Path<Uuid>,
    ApiJson(req): ApiJson<UpdateAdminUserRequest>,
) -> Response {
    let existing = match state.auth_store.get_user_by_id(user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "user_not_found", "user not found");
        }
        Err(err) => return map_store_error(err, "get_user_patch"),
    };

    let password_hash = if let Some(ref raw_pw) = req.reset_password {
        if let Err(msg) = validate_password_policy(raw_pw) {
            return api_error(StatusCode::BAD_REQUEST, "invalid_input", msg);
        }
        match hash_password(raw_pw) {
            Ok(h) => Some(h),
            Err(err) => {
                warn!(error = %err, "password hashing failed");
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "failed to process password",
                );
            }
        }
    } else {
        None
    };

    let next_role_norm = req.role.as_deref().map(validate_role).transpose();
    let next_role = match next_role_norm {
        Ok(r) => r,
        Err(err) => return map_store_error(err, "validate_patch_role"),
    };
    let next_status_norm = req.status.as_deref().map(validate_status).transpose();
    let next_status = match next_status_norm {
        Ok(s) => s,
        Err(err) => return map_store_error(err, "validate_patch_status"),
    };

    let role_changed = next_role.as_deref().is_some_and(|r| r != existing.role);
    let disabled_now = next_status.as_deref().is_some_and(|s| s != STATUS_ACTIVE);
    let must_change_enabled = req.must_change_password == Some(true);
    let password_reset = password_hash.is_some();

    let should_revoke_and_kick =
        role_changed || disabled_now || must_change_enabled || password_reset;

    let updated = match state
        .auth_store
        .update_user(
            user_id,
            UpdateUserInput {
                email: req.email,
                display_name: req.display_name,
                role: next_role,
                status: next_status,
                must_change_password: req.must_change_password,
                password_hash,
                bump_token_version: should_revoke_and_kick,
            },
        )
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => {
            return api_error(StatusCode::NOT_FOUND, "user_not_found", "user not found");
        }
        Err(err) => return map_store_error(err, "admin_update_user"),
    };

    if should_revoke_and_kick {
        let _ = state.auth_store.revoke_user_sessions(user_id).await;
        if let Ok(revoked_certs) = state
            .auth_store
            .revoke_user_certs(user_id, Some("admin_user_updated"))
            .await
        {
            revoke_cert_list_in_ephemeral(&*state.ephemeral, &revoked_certs).await;
        }
        let _ = state
            .signal_hub
            .kick_user_devices(user_id, "account_updated")
            .await;
        if let Ok(devs) = state.auth_store.list_user_devices(user_id).await {
            for dev in devs {
                let _ = state.relay_server.kick_device_sessions(&dev.device_id);
            }
        }
    }

    let device_count = state
        .auth_store
        .list_user_devices(user_id)
        .await
        .map(|d| d.len() as u64)
        .unwrap_or(0);

    let actor_uid = match actor {
        AdminActor::Jwt(ref u) => Some(u.user.id),
        AdminActor::StaticToken => None,
    };
    let _ = state
        .auth_store
        .append_audit_log(
            actor_uid,
            None,
            "admin.user_update",
            serde_json::json!({
                "target_user_id": user_id,
                "revoked_and_kicked": should_revoke_and_kick,
                "status": updated.status,
                "role": updated.role,
            }),
        )
        .await;

    (
        StatusCode::OK,
        Json(AdminUserMutationResponse {
            user: to_admin_user_view(updated, device_count),
        }),
    )
        .into_response()
}

async fn delete_admin_user(
    State(state): State<AppState>,
    Extension(actor): Extension<AdminActor>,
    Path(user_id): Path<Uuid>,
) -> Response {
    let user_devices = state
        .auth_store
        .list_user_devices(user_id)
        .await
        .unwrap_or_default();
    let user_certs = state
        .auth_store
        .list_device_certs(Some(user_id), None)
        .await
        .unwrap_or_default();

    let deleted = match state.auth_store.delete_user(user_id).await {
        Ok(flag) => flag,
        Err(err) => return map_store_error(err, "admin_delete_user"),
    };

    if !deleted {
        return api_error(StatusCode::NOT_FOUND, "user_not_found", "user not found");
    }

    // Cascade-deleted certs in DB must immediately enter ephemeral revocation cache,
    // and every bound device must be kicked from signaling & relay.
    revoke_cert_list_in_ephemeral(&*state.ephemeral, &user_certs).await;
    let _ = state
        .signal_hub
        .kick_user_devices(user_id, "user_deleted")
        .await;
    for dev in user_devices {
        let _ = state
            .signal_hub
            .kick_device(&dev.device_id, "user_deleted")
            .await;
        let _ = state.relay_server.kick_device_sessions(&dev.device_id);
    }

    let actor_uid = match actor {
        AdminActor::Jwt(ref u) => Some(u.user.id),
        AdminActor::StaticToken => None,
    };
    let _ = state
        .auth_store
        .append_audit_log(
            actor_uid,
            None,
            "admin.user_delete",
            serde_json::json!({ "deleted_user_id": user_id }),
        )
        .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "deleted",
            "user_id": user_id,
        })),
    )
        .into_response()
}

async fn post_register_device(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<RegisterInput>,
) -> Response {
    match state.device_store.register_or_get(input).await {
        Ok(rec) => {
            let resp = RegisterResponse {
                formatted_id: format_device_id(&rec.device_id),
                device_id: rec.device_id,
                device_key: rec.device_key,
                hostname: rec.hostname,
                os: rec.os,
                device_group: rec.device_group,
                created_at: rec.created_at,
                last_seen_at: rec.last_seen_at,
                signal_tcp_addr: state.endpoints.signal_tcp_addr.clone(),
                udp_probe_addr: state.endpoints.udp_probe_addr.clone(),
                tcp_relay_addr: state.endpoints.tcp_relay_addr.clone(),
                relay_addr: state.endpoints.tcp_relay_addr.clone(),
            };
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(StoreError::InvalidInput(msg)) => {
            api_error(StatusCode::BAD_REQUEST, "invalid_input", msg)
        }
        Err(StoreError::DeviceDisabled(id)) => api_error(
            StatusCode::FORBIDDEN,
            "device_disabled",
            format!("device {id} is disabled"),
        ),
        Err(err) => {
            warn!(error = %err, "device registration failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to register device",
            )
        }
    }
}

async fn get_stats(State(state): State<AppState>) -> Response {
    let registered_count = match state.device_store.count_devices().await {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "failed to count registered devices");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "failed to query device count",
            );
        }
    };

    let online_count = match state.ephemeral.online_ids().await {
        Ok(ids) => ids.len().max(state.signal_hub.online_count()),
        Err(err) => {
            warn!(error = %err, "failed to list online ids from ephemeral store");
            state.signal_hub.online_count()
        }
    };

    let relay_stats = state.relay_server.stats();

    (
        StatusCode::OK,
        Json(StatsResponse {
            online_count,
            registered_count,
            active_relay_sessions: relay_stats.active_sessions,
            total_relay_sessions: relay_stats.total_sessions,
            total_relay_bytes: relay_stats.total_relay_bytes,
            total_bytes_up: relay_stats.total_bytes_up,
            total_bytes_down: relay_stats.total_bytes_down,
            punch_coord_count: state.signal_hub.punch_coord_count(),
            ephemeral_backend: state.ephemeral.backend_name().to_string(),
            relay_engine: relay_stats.engine,
            uptime_sec: state.started_at.elapsed().as_secs(),
        }),
    )
        .into_response()
}

async fn get_devices(State(state): State<AppState>, Query(query): Query<DevicesQuery>) -> Response {
    // M11: Validate `status` upfront before touching DeviceStore so invalid status
    // always yields 400 Bad Request even on an empty database.
    let status_filter = match DeviceStatusFilter::parse(query.status.as_deref()) {
        Ok(filter) => filter,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, "invalid_status", msg),
    };

    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let offset = query.offset.unwrap_or(0);

    let online_set: HashSet<String> = state
        .ephemeral
        .online_ids()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();
    let in_session_set = state.relay_server.in_session_device_ids();
    let mut email_cache: HashMap<Uuid, Option<String>> = HashMap::new();

    let mut devices = Vec::with_capacity(limit.min(64));

    let total_count: usize = if status_filter == DeviceStatusFilter::All {
        let records = match state
            .device_store
            .list_devices(query.q.as_deref(), limit, offset)
            .await
        {
            Ok(list) => list,
            Err(err) => {
                warn!(error = %err, "failed to list devices");
                return api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "database_error",
                    "failed to list devices",
                );
            }
        };

        for rec in records {
            let view =
                build_device_view(&state, rec, &online_set, &in_session_set, &mut email_cache)
                    .await;
            devices.push(view);
        }

        match state
            .device_store
            .count_devices_filtered(query.q.as_deref())
            .await
        {
            Ok(n) => usize::try_from(n).unwrap_or(devices.len()),
            Err(err) => {
                warn!(error = %err, "failed to count filtered devices");
                devices.len()
            }
        }
    } else {
        // Page through DeviceStore in bounded chunks so filtered pagination never loads
        // unbounded rows into memory while still respecting `offset`, `limit`, and true `total_count`.
        let mut db_offset = 0usize;
        let mut matched_seen = 0usize;
        let batch_size = MAX_LIST_LIMIT;

        loop {
            let batch = match state
                .device_store
                .list_devices(query.q.as_deref(), batch_size, db_offset)
                .await
            {
                Ok(list) => list,
                Err(err) => {
                    warn!(error = %err, "failed to list devices");
                    return api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "database_error",
                        "failed to list devices",
                    );
                }
            };
            let batch_len = batch.len();
            if batch_len == 0 {
                break;
            }

            for rec in batch {
                let in_session = in_session_set.contains(&rec.device_id);
                let online = online_set.contains(&rec.device_id)
                    || state.signal_hub.is_peer_connected(&rec.device_id);

                if !status_filter.matches(online, in_session) {
                    continue;
                }
                let idx = matched_seen;
                matched_seen += 1;
                if idx < offset || devices.len() >= limit {
                    continue;
                }

                let view =
                    build_device_view_with_state(&state, rec, online, in_session, &mut email_cache)
                        .await;
                devices.push(view);
            }

            if batch_len < batch_size {
                break;
            }
            db_offset = db_offset.saturating_add(batch_len);
        }
        matched_seen
    };

    let total = devices.len();
    (
        StatusCode::OK,
        Json(DevicesResponse {
            devices,
            total,
            total_count,
            limit,
            offset,
        }),
    )
        .into_response()
}

async fn resolve_owner_email(
    state: &AppState,
    owner_user_id: Option<Uuid>,
    cache: &mut HashMap<Uuid, Option<String>>,
) -> Option<String> {
    let uid = owner_user_id?;
    if let Some(cached) = cache.get(&uid) {
        return cached.clone();
    }
    let email = state
        .auth_store
        .get_user_by_id(uid)
        .await
        .ok()
        .flatten()
        .map(|u| u.email);
    cache.insert(uid, email.clone());
    email
}

async fn build_device_view(
    state: &AppState,
    rec: DeviceRecord,
    online_set: &HashSet<String>,
    in_session_set: &HashSet<String>,
    email_cache: &mut HashMap<Uuid, Option<String>>,
) -> DeviceView {
    let in_session = in_session_set.contains(&rec.device_id);
    let online =
        online_set.contains(&rec.device_id) || state.signal_hub.is_peer_connected(&rec.device_id);
    build_device_view_with_state(state, rec, online, in_session, email_cache).await
}

async fn build_device_view_with_state(
    state: &AppState,
    rec: DeviceRecord,
    online: bool,
    in_session: bool,
    email_cache: &mut HashMap<Uuid, Option<String>>,
) -> DeviceView {
    let status = if in_session {
        "in_session"
    } else if online {
        "online"
    } else {
        "offline"
    };

    // L7: Skip ephemeral `get_public_udp` lookup for offline devices.
    let public_udp_addr = if online || in_session {
        state
            .ephemeral
            .get_public_udp(&rec.device_id)
            .await
            .ok()
            .flatten()
            .map(|a| a.to_string())
    } else {
        None
    };

    let lan_addrs = if online {
        state
            .signal_hub
            .peer_lan_addrs(&rec.device_id)
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let owner_email = resolve_owner_email(state, rec.owner_user_id, email_cache).await;

    DeviceView {
        formatted_id: format_device_id(&rec.device_id),
        device_id: rec.device_id,
        hostname: rec.hostname,
        os: rec.os,
        device_group: rec.device_group,
        alias: rec.alias,
        owner_user_id: rec.owner_user_id,
        owner_email,
        allow_remote_assist: rec.allow_remote_assist,
        disabled: rec.disabled,
        online,
        in_session,
        status: status.to_string(),
        public_udp_addr,
        lan_addrs,
        created_at: rec.created_at,
        last_seen_at: rec.last_seen_at,
    }
}

async fn get_sessions(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.relay_server.active_sessions();
    let total = sessions.len();
    (StatusCode::OK, Json(SessionsResponse { sessions, total }))
}

async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.relay_server.kick_session(&id) {
        (
            StatusCode::OK,
            Json(KickSessionResponse {
                status: "kicked".to_string(),
                session_id: id,
            }),
        )
            .into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "session_not_found",
            format!("active relay session {id} not found"),
        )
    }
}

async fn delete_device(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let norm = normalize_device_id(&id);
    if norm.len() != 9 {
        return api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            format!("device {id} not found"),
        );
    }

    let updated = match state.device_store.set_disabled(&norm, true).await {
        Ok(found) => found,
        Err(err) => {
            warn!(device_id = %norm, error = %err, "failed to disable device");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database_error",
                "failed to disable device",
            );
        }
    };

    if !updated {
        return api_error(
            StatusCode::NOT_FOUND,
            "device_not_found",
            format!("device {norm} not found"),
        );
    }

    let owner_uid = state
        .device_store
        .get_device_owner(&norm)
        .await
        .ok()
        .flatten();

    if let Ok(revoked_certs) = state
        .auth_store
        .revoke_device_certs(&norm, Some("disabled_by_admin"))
        .await
    {
        revoke_cert_list_in_ephemeral(&*state.ephemeral, &revoked_certs).await;
    }
    if let Some(uid) = owner_uid {
        let _ = state.auth_store.revoke_device_sessions(uid, &norm).await;
    }

    let kicked_signal = state
        .signal_hub
        .kick_device(&norm, "disabled_by_admin")
        .await;
    let kicked_sessions = state.relay_server.kick_device_sessions(&norm);

    (
        StatusCode::OK,
        Json(DisableDeviceResponse {
            status: "disabled".to_string(),
            device_id: norm,
            kicked_signal,
            kicked_sessions,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_checks_exact_bytes_and_lengths() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"secret-token-123", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-123", b"secret-token-124"));
        assert!(!constant_time_eq(b"secret-token", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-123", b"secret-token"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn device_status_filter_parses_and_rejects_invalid() {
        assert_eq!(DeviceStatusFilter::parse(None), Ok(DeviceStatusFilter::All));
        assert_eq!(
            DeviceStatusFilter::parse(Some("")),
            Ok(DeviceStatusFilter::All)
        );
        assert_eq!(
            DeviceStatusFilter::parse(Some(" ALL ")),
            Ok(DeviceStatusFilter::All)
        );
        assert_eq!(
            DeviceStatusFilter::parse(Some("online")),
            Ok(DeviceStatusFilter::Online)
        );
        assert_eq!(
            DeviceStatusFilter::parse(Some("OFFLINE")),
            Ok(DeviceStatusFilter::Offline)
        );
        assert_eq!(
            DeviceStatusFilter::parse(Some("in_session")),
            Ok(DeviceStatusFilter::InSession)
        );
        assert!(DeviceStatusFilter::parse(Some("bogus")).is_err());
    }

    #[test]
    fn build_cors_layer_validates_origins() {
        assert!(build_cors_layer(&[]).is_ok());
        assert!(build_cors_layer(&["*".to_string()]).is_ok());
        assert!(build_cors_layer(&["https://admin.example.com".to_string()]).is_ok());
        assert!(build_cors_layer(&["bad\norigin".to_string()]).is_err());
    }
}
