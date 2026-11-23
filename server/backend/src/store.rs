use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, PgPool};
use thiserror::Error;
use uuid::Uuid;

pub const MAX_DEVICE_KEY_LEN: usize = 256;
pub const MAX_HOSTNAME_LEN: usize = 128;
pub const MAX_OS_LEN: usize = 128;
pub const MAX_DEVICE_GROUP_LEN: usize = 64;
pub const MAX_DEVICE_ALIAS_LEN: usize = 128;
pub const MAX_DEVICE_PUBKEY_LEN: usize = 256;
pub const MAX_EMAIL_LEN: usize = 254;
pub const MAX_DISPLAY_NAME_LEN: usize = 128;
pub const MAX_CLIENT_INFO_LEN: usize = 256;
pub const MAX_REVOKE_REASON_LEN: usize = 256;
pub const MAX_AUDIT_ACTION_LEN: usize = 128;
pub const DEFAULT_LIST_LIMIT: usize = 100;
pub const MAX_LIST_LIMIT: usize = 500;
const MAX_ID_GENERATION_ATTEMPTS: usize = 64;

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_USER: &str = "user";
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_DISABLED: &str = "disabled";
pub const SETTING_ALLOW_REGISTRATION: &str = "allow_registration";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("device {0} is disabled")]
    DeviceDisabled(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("refresh token replay detected for family {family_id}")]
    TokenReplayDetected { family_id: Uuid, user_id: Uuid },
    #[error("failed to allocate unique 9-digit device_id after retries")]
    IdAllocationExhausted,
    #[error("cache error: {0}")]
    Cache(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRecord {
    pub device_id: String,
    pub device_key: String,
    pub hostname: String,
    pub os: String,
    pub device_group: String,
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub device_pubkey: String,
    #[serde(default)]
    pub allow_remote_assist: bool,
    #[serde(default)]
    pub bound_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterInput {
    pub device_key: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default, alias = "group")]
    pub device_group: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaySessionLog {
    pub session_id: String,
    pub host_id: String,
    pub client_id: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserRecord {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: String,
    pub status: String,
    pub must_change_password: bool,
    pub token_version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserWithDeviceCount {
    pub user: UserRecord,
    pub device_count: u64,
}

#[derive(Debug, Clone)]
pub struct CreateUserInput {
    pub email: String,
    pub display_name: String,
    pub password_hash: String,
    pub role: String,
    pub status: String,
    pub must_change_password: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateUserInput {
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub status: Option<String>,
    pub must_change_password: Option<bool>,
    pub password_hash: Option<String>,
    pub bump_token_version: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserSessionRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub device_id: Option<String>,
    pub family_id: Uuid,
    #[serde(skip_serializing)]
    pub token_hash: String,
    #[serde(skip_serializing)]
    pub prev_token_hash: Option<String>,
    pub prev_grace_until: Option<DateTime<Utc>>,
    pub rotated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub client_info: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CreateSessionInput {
    pub user_id: Uuid,
    pub device_id: Option<String>,
    pub family_id: Option<Uuid>,
    pub token_hash: String,
    pub expires_at: DateTime<Utc>,
    pub client_info: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerKeyRecord {
    pub key_name: String,
    pub algorithm: String,
    pub private_key_der: Vec<u8>,
    pub public_key_der: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkiCaRecord {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint_sha256: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceCertRecord {
    pub serial_hex: String,
    pub user_id: Uuid,
    pub device_id: String,
    pub fingerprint_sha256: String,
    pub cert_pem: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoke_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditLogRecord {
    pub id: i64,
    pub actor_user_id: Option<Uuid>,
    pub target_device_id: Option<String>,
    pub action: String,
    pub detail_json: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Normalize a device ID by stripping ASCII whitespace and dashes.
pub fn normalize_device_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-')
        .collect()
}

/// Check whether `id` is a valid 9-digit numeric device ID in `100000000..=999999999`.
pub fn is_valid_device_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 9 && (b'1'..=b'9').contains(&bytes[0]) && bytes.iter().all(u8::is_ascii_digit)
}

/// Format a 9-digit device ID as `"123 456 789"`.
pub fn format_device_id(id: &str) -> String {
    if id.len() == 9 && id.is_ascii() {
        format!("{} {} {}", &id[0..3], &id[3..6], &id[6..9])
    } else {
        id.to_string()
    }
}

/// Generate a cryptographically random 9-digit device ID in `100000000..=999999999`.
pub fn generate_device_id() -> String {
    let code: u32 = OsRng.gen_range(100_000_000..=999_999_999);
    format!("{code:09}")
}

/// Escape SQL `LIKE`/`ILIKE` wildcard characters (`\`, `%`, `_`) using `\` as the escape char.
pub fn escape_like_pattern(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Normalize an email address by trimming whitespace and lowercasing ASCII characters.
pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Validate and normalize an email address.
pub fn validate_email(raw: &str) -> Result<String, StoreError> {
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(
            "email must not contain NUL or control characters".into(),
        ));
    }
    let norm = normalize_email(raw);
    if norm.is_empty() || norm.len() > MAX_EMAIL_LEN {
        return Err(StoreError::InvalidInput(format!(
            "email must be 1..={MAX_EMAIL_LEN} bytes"
        )));
    }
    let Some((local, domain)) = norm.split_once('@') else {
        return Err(StoreError::InvalidInput(
            "email must contain a single '@'".into(),
        ));
    };
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || !domain.contains('.')
        || norm.chars().any(char::is_whitespace)
    {
        return Err(StoreError::InvalidInput("invalid email format".into()));
    }
    Ok(norm)
}

pub fn validate_role(raw: &str) -> Result<String, StoreError> {
    let norm = raw.trim().to_ascii_lowercase();
    match norm.as_str() {
        ROLE_ADMIN | ROLE_USER => Ok(norm),
        _ => Err(StoreError::InvalidInput(format!(
            "invalid role '{raw}' (expected 'admin' or 'user')"
        ))),
    }
}

pub fn validate_status(raw: &str) -> Result<String, StoreError> {
    let norm = raw.trim().to_ascii_lowercase();
    match norm.as_str() {
        STATUS_ACTIVE | STATUS_DISABLED => Ok(norm),
        _ => Err(StoreError::InvalidInput(format!(
            "invalid status '{raw}' (expected 'active' or 'disabled')"
        ))),
    }
}

pub fn validate_sha256_hex(raw: &str, field: &'static str) -> Result<String, StoreError> {
    let norm = raw.trim().to_ascii_lowercase();
    if norm.len() != 64 || !norm.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::InvalidInput(format!(
            "{field} must be a 64-character hex string"
        )));
    }
    Ok(norm)
}

pub fn validate_text_field(
    raw: &str,
    max_len: usize,
    field: &'static str,
) -> Result<String, StoreError> {
    if raw.contains('\0') || raw.chars().any(char::is_control) {
        return Err(StoreError::InvalidInput(format!(
            "{field} must not contain NUL or control characters"
        )));
    }
    let trimmed = raw.trim();
    if trimmed.len() > max_len {
        return Err(StoreError::InvalidInput(format!(
            "{field} exceeds maximum length of {max_len} bytes"
        )));
    }
    Ok(trimmed.to_string())
}

fn validate_register_input(
    input: &RegisterInput,
) -> Result<(String, String, String, String), StoreError> {
    let device_key = validate_text_field(&input.device_key, MAX_DEVICE_KEY_LEN, "device_key")?;
    if device_key.is_empty() {
        return Err(StoreError::InvalidInput(format!(
            "device_key must be 1..={MAX_DEVICE_KEY_LEN} bytes"
        )));
    }
    let hostname = validate_text_field(&input.hostname, MAX_HOSTNAME_LEN, "hostname")?;
    let os = validate_text_field(&input.os, MAX_OS_LEN, "os")?;
    let device_group =
        validate_text_field(&input.device_group, MAX_DEVICE_GROUP_LEN, "device_group")?;
    Ok((device_key, hostname, os, device_group))
}

fn validate_optional_device_id(raw: Option<&str>) -> Result<Option<String>, StoreError> {
    match raw {
        Some(did) => {
            let norm = normalize_device_id(did);
            if !is_valid_device_id(&norm) {
                return Err(StoreError::InvalidInput(format!(
                    "invalid device_id: {did}"
                )));
            }
            Ok(Some(norm))
        }
        None => Ok(None),
    }
}

fn is_pg_unique_violation(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23505")
    )
}

fn is_pg_fk_violation(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("23503")
    )
}

#[async_trait]
pub trait DeviceStore: Send + Sync {
    async fn register_or_get(&self, input: RegisterInput) -> Result<DeviceRecord, StoreError>;
    async fn get_by_id(&self, device_id: &str) -> Result<Option<DeviceRecord>, StoreError>;
    async fn get_by_key(&self, device_key: &str) -> Result<Option<DeviceRecord>, StoreError>;
    async fn verify_key(&self, device_id: &str, device_key: &str) -> Result<bool, StoreError>;
    async fn list_devices(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DeviceRecord>, StoreError>;
    async fn count_devices_filtered(&self, query: Option<&str>) -> Result<u64, StoreError> {
        let _ = query;
        self.count_devices().await
    }
    async fn touch_last_seen(&self, device_id: &str) -> Result<(), StoreError>;
    async fn set_disabled(&self, device_id: &str, disabled: bool) -> Result<bool, StoreError>;
    async fn delete_device(&self, device_id: &str) -> Result<bool, StoreError>;
    async fn count_devices(&self) -> Result<u64, StoreError>;
    async fn health_check(&self) -> Result<(), StoreError>;
    #[allow(clippy::too_many_arguments)]
    async fn record_relay_end(
        &self,
        session_id: &str,
        host_id: &str,
        client_id: &str,
        bytes_up: u64,
        bytes_down: u64,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;

    /// Bind a device to `user_id`, automatically replacing any previous owner.
    async fn bind_device_to_user(
        &self,
        device_id: &str,
        user_id: Uuid,
        alias: Option<&str>,
        device_pubkey: Option<&str>,
    ) -> Result<Option<DeviceRecord>, StoreError> {
        Ok(self
            .bind_device_to_user_with_prev(device_id, user_id, alias, device_pubkey)
            .await?
            .map(|(rec, _)| rec))
    }

    /// Atomically bind a device to `user_id` and return `(updated_record, previous_owner_user_id)`.
    async fn bind_device_to_user_with_prev(
        &self,
        device_id: &str,
        user_id: Uuid,
        alias: Option<&str>,
        device_pubkey: Option<&str>,
    ) -> Result<Option<(DeviceRecord, Option<Uuid>)>, StoreError>;

    async fn list_user_devices(&self, user_id: Uuid) -> Result<Vec<DeviceRecord>, StoreError>;

    async fn update_device_alias(
        &self,
        device_id: &str,
        owner_user_id: Uuid,
        alias: &str,
    ) -> Result<bool, StoreError>;

    async fn unbind_device(&self, device_id: &str, owner_user_id: Uuid)
        -> Result<bool, StoreError>;

    async fn admin_unbind_device(&self, device_id: &str) -> Result<bool, StoreError>;

    async fn set_device_allow_remote_assist(
        &self,
        device_id: &str,
        owner_user_id: Option<Uuid>,
        allow: bool,
    ) -> Result<bool, StoreError>;

    async fn get_device_owner(&self, device_id: &str) -> Result<Option<Uuid>, StoreError>;

    async fn set_device_pubkey(
        &self,
        device_id: &str,
        device_pubkey: &str,
    ) -> Result<bool, StoreError>;
}

#[async_trait]
pub trait UserStore: Send + Sync {
    async fn create_user(&self, input: CreateUserInput) -> Result<UserRecord, StoreError>;
    async fn get_user_by_id(&self, user_id: Uuid) -> Result<Option<UserRecord>, StoreError>;
    async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRecord>, StoreError>;
    async fn list_users(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<UserWithDeviceCount>, StoreError>;
    async fn count_users(&self, query: Option<&str>) -> Result<u64, StoreError>;
    async fn count_active_admins(&self) -> Result<u64, StoreError>;
    async fn update_user(
        &self,
        user_id: Uuid,
        input: UpdateUserInput,
    ) -> Result<Option<UserRecord>, StoreError>;
    async fn touch_user_last_login(&self, user_id: Uuid) -> Result<(), StoreError>;
    async fn delete_user(&self, user_id: Uuid) -> Result<bool, StoreError>;

    async fn create_user_session(
        &self,
        input: CreateSessionInput,
    ) -> Result<UserSessionRecord, StoreError>;
    async fn get_user_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<UserSessionRecord>, StoreError>;
    async fn list_user_sessions(&self, user_id: Uuid)
        -> Result<Vec<UserSessionRecord>, StoreError>;
    async fn rotate_refresh_token(
        &self,
        presented_token_hash: &str,
        new_token_hash: &str,
        new_expires_at: DateTime<Utc>,
        grace_period: Duration,
    ) -> Result<UserSessionRecord, StoreError>;
    async fn revoke_session(&self, session_id: Uuid) -> Result<bool, StoreError>;
    async fn get_active_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError>;
    async fn revoke_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError>;
    async fn revoke_session_family(&self, family_id: Uuid) -> Result<u64, StoreError>;
    async fn revoke_user_sessions(&self, user_id: Uuid) -> Result<u64, StoreError>;
    async fn revoke_device_sessions(
        &self,
        user_id: Uuid,
        device_id: &str,
    ) -> Result<u64, StoreError>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>, StoreError>;
    async fn set_setting(
        &self,
        key: &str,
        value: serde_json::Value,
        updated_by: Option<Uuid>,
    ) -> Result<(), StoreError>;

    async fn get_allow_registration(&self) -> Result<bool, StoreError> {
        match self.get_setting(SETTING_ALLOW_REGISTRATION).await? {
            Some(serde_json::Value::Bool(b)) => Ok(b),
            Some(other) => Err(StoreError::InvalidInput(format!(
                "setting '{SETTING_ALLOW_REGISTRATION}' must be a JSON boolean, got {other}"
            ))),
            None => Ok(true),
        }
    }

    async fn get_allow_registration_with_meta(
        &self,
    ) -> Result<(bool, Option<DateTime<Utc>>), StoreError> {
        Ok((self.get_allow_registration().await?, None))
    }

    async fn set_allow_registration(
        &self,
        allowed: bool,
        updated_by: Option<Uuid>,
    ) -> Result<(), StoreError> {
        self.set_setting(
            SETTING_ALLOW_REGISTRATION,
            serde_json::Value::Bool(allowed),
            updated_by,
        )
        .await
    }
}

#[async_trait]
pub trait PkiStore: Send + Sync {
    async fn get_server_key(&self, key_name: &str) -> Result<Option<ServerKeyRecord>, StoreError>;
    async fn put_server_key_if_absent(
        &self,
        key: ServerKeyRecord,
    ) -> Result<ServerKeyRecord, StoreError>;

    async fn get_pki_ca(&self) -> Result<Option<PkiCaRecord>, StoreError>;
    async fn put_pki_ca_if_absent(&self, ca: PkiCaRecord) -> Result<PkiCaRecord, StoreError>;

    async fn insert_device_cert(&self, cert: DeviceCertRecord) -> Result<(), StoreError>;
    async fn get_device_cert(
        &self,
        serial_hex: &str,
    ) -> Result<Option<DeviceCertRecord>, StoreError>;
    async fn list_device_certs(
        &self,
        user_id: Option<Uuid>,
        device_id: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError>;
    async fn revoke_cert_by_serial(
        &self,
        serial_hex: &str,
        reason: Option<&str>,
    ) -> Result<Option<DeviceCertRecord>, StoreError>;
    async fn revoke_user_certs(
        &self,
        user_id: Uuid,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError>;
    async fn revoke_device_certs(
        &self,
        device_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError>;
    async fn list_active_revoked_certs(&self) -> Result<Vec<DeviceCertRecord>, StoreError>;
}

#[async_trait]
pub trait AuditStore: Send + Sync {
    async fn append_audit_log(
        &self,
        actor_user_id: Option<Uuid>,
        target_device_id: Option<&str>,
        action: &str,
        detail_json: serde_json::Value,
    ) -> Result<(), StoreError>;
    async fn list_audit_logs(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditLogRecord>, StoreError>;
}

pub trait AuthStore:
    DeviceStore + UserStore + SettingsStore + PkiStore + AuditStore + Send + Sync
{
}

impl<T> AuthStore for T where
    T: DeviceStore + UserStore + SettingsStore + PkiStore + AuditStore + Send + Sync
{
}

#[derive(Debug, FromRow)]
struct DeviceRow {
    device_id: String,
    device_key: String,
    hostname: String,
    os: String,
    device_group: String,
    disabled: bool,
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    owner_user_id: Option<Uuid>,
    alias: String,
    device_pubkey: String,
    allow_remote_assist: bool,
    bound_at: Option<DateTime<Utc>>,
}

impl From<DeviceRow> for DeviceRecord {
    fn from(row: DeviceRow) -> Self {
        Self {
            device_id: row.device_id.trim().to_string(),
            device_key: row.device_key,
            hostname: row.hostname,
            os: row.os,
            device_group: row.device_group,
            disabled: row.disabled,
            created_at: row.created_at,
            last_seen_at: row.last_seen_at,
            owner_user_id: row.owner_user_id,
            alias: row.alias,
            device_pubkey: row.device_pubkey,
            allow_remote_assist: row.allow_remote_assist,
            bound_at: row.bound_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct UserRow {
    id: Uuid,
    email: String,
    display_name: String,
    password_hash: String,
    role: String,
    status: String,
    must_change_password: bool,
    token_version: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_login_at: Option<DateTime<Utc>>,
}

impl From<UserRow> for UserRecord {
    fn from(row: UserRow) -> Self {
        Self {
            id: row.id,
            email: row.email,
            display_name: row.display_name,
            password_hash: row.password_hash,
            role: row.role,
            status: row.status,
            must_change_password: row.must_change_password,
            token_version: row.token_version,
            created_at: row.created_at,
            updated_at: row.updated_at,
            last_login_at: row.last_login_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct UserWithCountRow {
    id: Uuid,
    email: String,
    display_name: String,
    password_hash: String,
    role: String,
    status: String,
    must_change_password: bool,
    token_version: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_login_at: Option<DateTime<Utc>>,
    device_count: i64,
}

impl From<UserWithCountRow> for UserWithDeviceCount {
    fn from(row: UserWithCountRow) -> Self {
        Self {
            user: UserRecord {
                id: row.id,
                email: row.email,
                display_name: row.display_name,
                password_hash: row.password_hash,
                role: row.role,
                status: row.status,
                must_change_password: row.must_change_password,
                token_version: row.token_version,
                created_at: row.created_at,
                updated_at: row.updated_at,
                last_login_at: row.last_login_at,
            },
            device_count: row.device_count.max(0) as u64,
        }
    }
}

#[derive(Debug, FromRow)]
struct UserSessionRow {
    id: Uuid,
    user_id: Uuid,
    device_id: Option<String>,
    family_id: Uuid,
    token_hash: String,
    prev_token_hash: Option<String>,
    prev_grace_until: Option<DateTime<Utc>>,
    rotated_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    client_info: String,
    created_at: DateTime<Utc>,
}

impl From<UserSessionRow> for UserSessionRecord {
    fn from(row: UserSessionRow) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            device_id: row.device_id.map(|s| s.trim().to_string()),
            family_id: row.family_id,
            token_hash: row.token_hash.trim().to_string(),
            prev_token_hash: row.prev_token_hash.map(|s| s.trim().to_string()),
            prev_grace_until: row.prev_grace_until,
            rotated_at: row.rotated_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            client_info: row.client_info,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct ServerKeyRow {
    key_name: String,
    algorithm: String,
    private_key_der: Vec<u8>,
    public_key_der: Vec<u8>,
    created_at: DateTime<Utc>,
}

impl From<ServerKeyRow> for ServerKeyRecord {
    fn from(row: ServerKeyRow) -> Self {
        Self {
            key_name: row.key_name,
            algorithm: row.algorithm,
            private_key_der: row.private_key_der,
            public_key_der: row.public_key_der,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct PkiCaRow {
    cert_pem: String,
    key_pem: String,
    fingerprint_sha256: String,
    created_at: DateTime<Utc>,
}

impl From<PkiCaRow> for PkiCaRecord {
    fn from(row: PkiCaRow) -> Self {
        Self {
            cert_pem: row.cert_pem,
            key_pem: row.key_pem,
            fingerprint_sha256: row.fingerprint_sha256.trim().to_string(),
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, FromRow)]
struct DeviceCertRow {
    serial_hex: String,
    user_id: Uuid,
    device_id: String,
    fingerprint_sha256: String,
    cert_pem: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoke_reason: Option<String>,
}

impl From<DeviceCertRow> for DeviceCertRecord {
    fn from(row: DeviceCertRow) -> Self {
        Self {
            serial_hex: row.serial_hex.trim().to_ascii_lowercase(),
            user_id: row.user_id,
            device_id: row.device_id.trim().to_string(),
            fingerprint_sha256: row.fingerprint_sha256.trim().to_ascii_lowercase(),
            cert_pem: row.cert_pem,
            issued_at: row.issued_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            revoke_reason: row.revoke_reason,
        }
    }
}

#[derive(Debug, FromRow)]
struct AuditLogRow {
    id: i64,
    actor_user_id: Option<Uuid>,
    target_device_id: Option<String>,
    action: String,
    detail_json: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl From<AuditLogRow> for AuditLogRecord {
    fn from(row: AuditLogRow) -> Self {
        Self {
            id: row.id,
            actor_user_id: row.actor_user_id,
            target_device_id: row.target_device_id.map(|s| s.trim().to_string()),
            action: row.action,
            detail_json: row.detail_json,
            created_at: row.created_at,
        }
    }
}

const DEVICE_COLUMNS: &str = "device_id, device_key, hostname, os, device_group, disabled, \
    created_at, last_seen_at, owner_user_id, alias, device_pubkey, allow_remote_assist, bound_at";

const USER_COLUMNS: &str = "id, email, display_name, password_hash, role, status, \
    must_change_password, token_version, created_at, updated_at, last_login_at";

const SESSION_COLUMNS: &str = "id, user_id, device_id, family_id, token_hash, prev_token_hash, \
    prev_grace_until, rotated_at, expires_at, revoked_at, client_info, created_at";

const CERT_COLUMNS: &str = "serial_hex, user_id, device_id, fingerprint_sha256, cert_pem, \
    issued_at, expires_at, revoked_at, revoke_reason";

/// PostgreSQL-backed persistent store using `sqlx`.
#[derive(Debug, Clone)]
pub struct PgDeviceStore {
    pool: PgPool,
}

impl PgDeviceStore {
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(database_url)
            .await?;
        let store = Self { pool };
        store.run_migrations().await?;
        Ok(store)
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn run_migrations(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    async fn fetch_by_key(&self, device_key: &str) -> Result<Option<DeviceRecord>, StoreError> {
        let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_key = $1");
        let row: Option<DeviceRow> = sqlx::query_as(&sql)
            .bind(device_key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn update_existing_metadata(
        &self,
        device_key: &str,
        hostname: &str,
        os: &str,
        device_group: &str,
    ) -> Result<Option<DeviceRecord>, StoreError> {
        let sql = format!(
            "UPDATE devices
             SET hostname = COALESCE(NULLIF($1, ''), hostname),
                 os = COALESCE(NULLIF($2, ''), os),
                 device_group = COALESCE(NULLIF($3, ''), device_group),
                 last_seen_at = NOW()
             WHERE device_key = $4 AND disabled = FALSE
             RETURNING {DEVICE_COLUMNS}"
        );
        let row: Option<DeviceRow> = sqlx::query_as(&sql)
            .bind(hostname)
            .bind(os)
            .bind(device_group)
            .bind(device_key)
            .fetch_optional(&self.pool)
            .await?;

        if let Some(updated) = row {
            return Ok(Some(updated.into()));
        }

        // Distinguish disabled device from concurrently deleted/absent device.
        if let Some(existing) = self.fetch_by_key(device_key).await? {
            if existing.disabled {
                return Err(StoreError::DeviceDisabled(existing.device_id));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl DeviceStore for PgDeviceStore {
    async fn register_or_get(&self, input: RegisterInput) -> Result<DeviceRecord, StoreError> {
        let (device_key, hostname, os, device_group) = validate_register_input(&input)?;

        if let Some(updated) = self
            .update_existing_metadata(&device_key, &hostname, &os, &device_group)
            .await?
        {
            return Ok(updated);
        }

        let insert_sql = format!(
            "INSERT INTO devices (device_id, device_key, hostname, os, device_group)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING
             RETURNING {DEVICE_COLUMNS}"
        );

        for _ in 0..MAX_ID_GENERATION_ATTEMPTS {
            let candidate_id = generate_device_id();
            let inserted: Option<DeviceRow> = sqlx::query_as(&insert_sql)
                .bind(&candidate_id)
                .bind(&device_key)
                .bind(&hostname)
                .bind(&os)
                .bind(&device_group)
                .fetch_optional(&self.pool)
                .await?;

            if let Some(row) = inserted {
                return Ok(row.into());
            }

            // Conflict could be a concurrent register with the same device_key or a 9-digit ID collision.
            if let Some(updated) = self
                .update_existing_metadata(&device_key, &hostname, &os, &device_group)
                .await?
            {
                return Ok(updated);
            }
        }

        Err(StoreError::IdAllocationExhausted)
    }

    async fn get_by_id(&self, device_id: &str) -> Result<Option<DeviceRecord>, StoreError> {
        let norm = normalize_device_id(device_id);
        if norm.len() != 9 {
            return Ok(None);
        }
        let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = $1");
        let row: Option<DeviceRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn get_by_key(&self, device_key: &str) -> Result<Option<DeviceRecord>, StoreError> {
        let clean = device_key.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        let sql = format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE device_key = $1");
        let row: Option<DeviceRow> = sqlx::query_as(&sql)
            .bind(clean)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn verify_key(&self, device_id: &str, device_key: &str) -> Result<bool, StoreError> {
        let Some(rec) = self.get_by_id(device_id).await? else {
            return Ok(false);
        };
        Ok(rec.device_key == device_key.trim())
    }

    async fn list_devices(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DeviceRecord>, StoreError> {
        let limit_i64 = limit.clamp(1, MAX_LIST_LIMIT) as i64;
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);

        let rows: Vec<DeviceRow> = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => {
                let pattern = format!("%{}%", escape_like_pattern(q));
                let norm_q = normalize_device_id(q);
                let norm_pattern =
                    (!norm_q.is_empty()).then(|| format!("%{}%", escape_like_pattern(&norm_q)));
                let sql = format!(
                    "SELECT {DEVICE_COLUMNS}
                     FROM devices
                     WHERE ($1::text IS NOT NULL AND device_id ILIKE $1 ESCAPE '\\')
                        OR hostname ILIKE $2 ESCAPE '\\'
                        OR os ILIKE $2 ESCAPE '\\'
                        OR device_group ILIKE $2 ESCAPE '\\'
                        OR alias ILIKE $2 ESCAPE '\\'
                        OR EXISTS (
                            SELECT 1 FROM users u
                            WHERE u.id = devices.owner_user_id
                              AND u.email ILIKE $2 ESCAPE '\\'
                        )
                     ORDER BY last_seen_at DESC, device_id ASC
                     LIMIT $3 OFFSET $4"
                );
                sqlx::query_as(&sql)
                    .bind(norm_pattern.as_deref())
                    .bind(&pattern)
                    .bind(limit_i64)
                    .bind(offset_i64)
                    .fetch_all(&self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT {DEVICE_COLUMNS}
                     FROM devices
                     ORDER BY last_seen_at DESC, device_id ASC
                     LIMIT $1 OFFSET $2"
                );
                sqlx::query_as(&sql)
                    .bind(limit_i64)
                    .bind(offset_i64)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn count_devices_filtered(&self, query: Option<&str>) -> Result<u64, StoreError> {
        let count: i64 = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => {
                let pattern = format!("%{}%", escape_like_pattern(q));
                let norm_q = normalize_device_id(q);
                let norm_pattern =
                    (!norm_q.is_empty()).then(|| format!("%{}%", escape_like_pattern(&norm_q)));
                sqlx::query_scalar(
                    "SELECT COUNT(*)
                     FROM devices
                     WHERE ($1::text IS NOT NULL AND device_id ILIKE $1 ESCAPE '\\')
                        OR hostname ILIKE $2 ESCAPE '\\'
                        OR os ILIKE $2 ESCAPE '\\'
                        OR device_group ILIKE $2 ESCAPE '\\'
                        OR alias ILIKE $2 ESCAPE '\\'
                        OR EXISTS (
                            SELECT 1 FROM users u
                            WHERE u.id = devices.owner_user_id
                              AND u.email ILIKE $2 ESCAPE '\\'
                        )",
                )
                .bind(norm_pattern.as_deref())
                .bind(&pattern)
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query_scalar("SELECT COUNT(*) FROM devices")
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn touch_last_seen(&self, device_id: &str) -> Result<(), StoreError> {
        let norm = normalize_device_id(device_id);
        if norm.len() != 9 {
            return Ok(());
        }
        sqlx::query("UPDATE devices SET last_seen_at = NOW() WHERE device_id = $1")
            .bind(&norm)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_disabled(&self, device_id: &str, disabled: bool) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if norm.len() != 9 {
            return Ok(false);
        }
        let res = sqlx::query("UPDATE devices SET disabled = $1 WHERE device_id = $2")
            .bind(disabled)
            .bind(&norm)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn delete_device(&self, device_id: &str) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if norm.len() != 9 {
            return Ok(false);
        }
        let res = sqlx::query("DELETE FROM devices WHERE device_id = $1")
            .bind(&norm)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn count_devices(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices")
            .fetch_one(&self.pool)
            .await?;
        Ok(count.max(0) as u64)
    }

    async fn health_check(&self) -> Result<(), StoreError> {
        let _: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&self.pool).await?;
        Ok(())
    }

    async fn record_relay_end(
        &self,
        session_id: &str,
        host_id: &str,
        client_id: &str,
        bytes_up: u64,
        bytes_down: u64,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let norm_host = normalize_device_id(host_id);
        let norm_client = normalize_device_id(client_id);
        if !is_valid_device_id(&norm_host) {
            return Err(StoreError::InvalidInput(format!(
                "invalid host_id: {host_id}"
            )));
        }
        if !is_valid_device_id(&norm_client) {
            return Err(StoreError::InvalidInput(format!(
                "invalid client_id: {client_id}"
            )));
        }
        sqlx::query(
            "INSERT INTO relay_sessions_log (session_id, host_id, client_id, bytes_up, bytes_down, started_at, ended_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(session_id)
        .bind(&norm_host)
        .bind(&norm_client)
        .bind(i64::try_from(bytes_up).unwrap_or(i64::MAX))
        .bind(i64::try_from(bytes_down).unwrap_or(i64::MAX))
        .bind(started_at)
        .bind(ended_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn bind_device_to_user_with_prev(
        &self,
        device_id: &str,
        user_id: Uuid,
        alias: Option<&str>,
        device_pubkey: Option<&str>,
    ) -> Result<Option<(DeviceRecord, Option<Uuid>)>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(None);
        }
        let alias = alias
            .map(|a| validate_text_field(a, MAX_DEVICE_ALIAS_LEN, "alias"))
            .transpose()?;
        let pubkey = device_pubkey
            .map(|k| validate_text_field(k, MAX_DEVICE_PUBKEY_LEN, "device_pubkey"))
            .transpose()?;

        let mut tx = self.pool.begin().await?;
        let prev_row: Option<(Option<Uuid>,)> =
            sqlx::query_as("SELECT owner_user_id FROM devices WHERE device_id = $1 FOR UPDATE")
                .bind(&norm)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((prev_owner,)) = prev_row else {
            return Ok(None);
        };

        let sql = format!(
            "UPDATE devices
             SET alias = COALESCE($3, CASE WHEN owner_user_id IS DISTINCT FROM $2 THEN '' ELSE alias END),
                 allow_remote_assist = CASE WHEN owner_user_id IS DISTINCT FROM $2 THEN FALSE ELSE allow_remote_assist END,
                 device_pubkey = COALESCE($4, CASE WHEN owner_user_id IS DISTINCT FROM $2 THEN '' ELSE device_pubkey END),
                 owner_user_id = $2,
                 bound_at = NOW()
             WHERE device_id = $1
             RETURNING {DEVICE_COLUMNS}"
        );
        let row: Option<DeviceRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .bind(user_id)
            .bind(alias.as_deref())
            .bind(pubkey.as_deref())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|err| {
                if is_pg_fk_violation(&err) {
                    StoreError::NotFound(format!("user {user_id} not found"))
                } else {
                    StoreError::Database(err)
                }
            })?;
        tx.commit().await?;
        Ok(row.map(|r| (r.into(), prev_owner)))
    }

    async fn list_user_devices(&self, user_id: Uuid) -> Result<Vec<DeviceRecord>, StoreError> {
        let sql = format!(
            "SELECT {DEVICE_COLUMNS}
             FROM devices
             WHERE owner_user_id = $1
             ORDER BY last_seen_at DESC, device_id ASC"
        );
        let rows: Vec<DeviceRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn update_device_alias(
        &self,
        device_id: &str,
        owner_user_id: Uuid,
        alias: &str,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let clean = validate_text_field(alias, MAX_DEVICE_ALIAS_LEN, "alias")?;
        let res = sqlx::query(
            "UPDATE devices SET alias = $1 WHERE device_id = $2 AND owner_user_id = $3",
        )
        .bind(&clean)
        .bind(&norm)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn unbind_device(
        &self,
        device_id: &str,
        owner_user_id: Uuid,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let res = sqlx::query(
            "UPDATE devices
             SET owner_user_id = NULL, bound_at = NULL, alias = '', allow_remote_assist = FALSE
             WHERE device_id = $1 AND owner_user_id = $2",
        )
        .bind(&norm)
        .bind(owner_user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn admin_unbind_device(&self, device_id: &str) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let res = sqlx::query(
            "UPDATE devices
             SET owner_user_id = NULL, bound_at = NULL, alias = '', allow_remote_assist = FALSE
             WHERE device_id = $1 AND owner_user_id IS NOT NULL",
        )
        .bind(&norm)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn set_device_allow_remote_assist(
        &self,
        device_id: &str,
        owner_user_id: Option<Uuid>,
        allow: bool,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let res = match owner_user_id {
            Some(uid) => {
                sqlx::query(
                    "UPDATE devices SET allow_remote_assist = $1 WHERE device_id = $2 AND owner_user_id = $3",
                )
                .bind(allow)
                .bind(&norm)
                .bind(uid)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query("UPDATE devices SET allow_remote_assist = $1 WHERE device_id = $2")
                    .bind(allow)
                    .bind(&norm)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(res.rows_affected() > 0)
    }

    async fn get_device_owner(&self, device_id: &str) -> Result<Option<Uuid>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(None);
        }
        let owner: Option<Option<Uuid>> =
            sqlx::query_scalar("SELECT owner_user_id FROM devices WHERE device_id = $1")
                .bind(&norm)
                .fetch_optional(&self.pool)
                .await?;
        Ok(owner.flatten())
    }

    async fn set_device_pubkey(
        &self,
        device_id: &str,
        device_pubkey: &str,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let clean = validate_text_field(device_pubkey, MAX_DEVICE_PUBKEY_LEN, "device_pubkey")?;
        let res = sqlx::query("UPDATE devices SET device_pubkey = $1 WHERE device_id = $2")
            .bind(&clean)
            .bind(&norm)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[async_trait]
impl UserStore for PgDeviceStore {
    async fn create_user(&self, input: CreateUserInput) -> Result<UserRecord, StoreError> {
        let email = validate_email(&input.email)?;
        let display_name =
            validate_text_field(&input.display_name, MAX_DISPLAY_NAME_LEN, "display_name")?;
        let role = validate_role(&input.role)?;
        let status = validate_status(&input.status)?;
        if input.password_hash.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "password_hash must not be empty".into(),
            ));
        }

        let id = Uuid::new_v4();
        let sql = format!(
            "INSERT INTO users (id, email, display_name, password_hash, role, status, must_change_password)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {USER_COLUMNS}"
        );
        let row: UserRow = sqlx::query_as(&sql)
            .bind(id)
            .bind(&email)
            .bind(&display_name)
            .bind(&input.password_hash)
            .bind(&role)
            .bind(&status)
            .bind(input.must_change_password)
            .fetch_one(&self.pool)
            .await
            .map_err(|err| {
                if is_pg_unique_violation(&err) {
                    StoreError::Conflict(format!("email '{email}' is already registered"))
                } else {
                    StoreError::Database(err)
                }
            })?;
        Ok(row.into())
    }

    async fn get_user_by_id(&self, user_id: Uuid) -> Result<Option<UserRecord>, StoreError> {
        let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1");
        let row: Option<UserRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRecord>, StoreError> {
        let norm = normalize_email(email);
        if norm.is_empty() {
            return Ok(None);
        }
        let sql = format!("SELECT {USER_COLUMNS} FROM users WHERE email = $1");
        let row: Option<UserRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn list_users(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<UserWithDeviceCount>, StoreError> {
        let limit_i64 = limit.clamp(1, MAX_LIST_LIMIT) as i64;
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);

        let rows: Vec<UserWithCountRow> = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => {
                let pattern = format!("%{}%", escape_like_pattern(q));
                sqlx::query_as(
                    "SELECT u.id, u.email, u.display_name, u.password_hash, u.role, u.status,
                            u.must_change_password, u.token_version, u.created_at, u.updated_at,
                            u.last_login_at, COUNT(d.device_id) AS device_count
                     FROM users u
                     LEFT JOIN devices d ON d.owner_user_id = u.id
                     WHERE u.email ILIKE $1 ESCAPE '\\'
                        OR u.display_name ILIKE $1 ESCAPE '\\'
                     GROUP BY u.id
                     ORDER BY u.created_at DESC, u.email ASC
                     LIMIT $2 OFFSET $3",
                )
                .bind(&pattern)
                .bind(limit_i64)
                .bind(offset_i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT u.id, u.email, u.display_name, u.password_hash, u.role, u.status,
                            u.must_change_password, u.token_version, u.created_at, u.updated_at,
                            u.last_login_at, COUNT(d.device_id) AS device_count
                     FROM users u
                     LEFT JOIN devices d ON d.owner_user_id = u.id
                     GROUP BY u.id
                     ORDER BY u.created_at DESC, u.email ASC
                     LIMIT $1 OFFSET $2",
                )
                .bind(limit_i64)
                .bind(offset_i64)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn count_users(&self, query: Option<&str>) -> Result<u64, StoreError> {
        let count: i64 = match query.map(str::trim).filter(|q| !q.is_empty()) {
            Some(q) => {
                let pattern = format!("%{}%", escape_like_pattern(q));
                sqlx::query_scalar(
                    "SELECT COUNT(*) FROM users
                     WHERE email ILIKE $1 ESCAPE '\\' OR display_name ILIKE $1 ESCAPE '\\'",
                )
                .bind(&pattern)
                .fetch_one(&self.pool)
                .await?
            }
            None => {
                sqlx::query_scalar("SELECT COUNT(*) FROM users")
                    .fetch_one(&self.pool)
                    .await?
            }
        };
        Ok(count.max(0) as u64)
    }

    async fn count_active_admins(&self) -> Result<u64, StoreError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status = 'active'",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count.max(0) as u64)
    }

    async fn update_user(
        &self,
        user_id: Uuid,
        input: UpdateUserInput,
    ) -> Result<Option<UserRecord>, StoreError> {
        let email = input.email.as_deref().map(validate_email).transpose()?;
        let display_name = input
            .display_name
            .as_deref()
            .map(|n| validate_text_field(n, MAX_DISPLAY_NAME_LEN, "display_name"))
            .transpose()?;
        let role = input.role.as_deref().map(validate_role).transpose()?;
        let status = input.status.as_deref().map(validate_status).transpose()?;
        if let Some(ref hash) = input.password_hash {
            if hash.trim().is_empty() {
                return Err(StoreError::InvalidInput(
                    "password_hash must not be empty".into(),
                ));
            }
        }

        let mut tx = self.pool.begin().await?;
        let select_sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1 FOR UPDATE");
        let Some(existing): Option<UserRow> = sqlx::query_as(&select_sql)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(None);
        };

        let next_role = role.as_deref().unwrap_or(&existing.role);
        let next_status = status.as_deref().unwrap_or(&existing.status);
        let was_active_admin = existing.role == ROLE_ADMIN && existing.status == STATUS_ACTIVE;
        let stays_active_admin = next_role == ROLE_ADMIN && next_status == STATUS_ACTIVE;

        if was_active_admin && !stays_active_admin {
            let active_admins: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM users WHERE role = 'admin' AND status = 'active' ORDER BY id FOR UPDATE",
            )
            .fetch_all(&mut *tx)
            .await?;
            if !active_admins.iter().any(|&id| id != user_id) {
                return Err(StoreError::Forbidden(
                    "cannot demote or disable the last active admin".into(),
                ));
            }
        }

        let update_sql = format!(
            "UPDATE users
             SET email = COALESCE($2, email),
                 display_name = COALESCE($3, display_name),
                 role = COALESCE($4, role),
                 status = COALESCE($5, status),
                 must_change_password = COALESCE($6, must_change_password),
                 password_hash = COALESCE($7, password_hash),
                 token_version = token_version + CASE WHEN $8 THEN 1 ELSE 0 END,
                 updated_at = NOW()
             WHERE id = $1
             RETURNING {USER_COLUMNS}"
        );
        let updated: UserRow = sqlx::query_as(&update_sql)
            .bind(user_id)
            .bind(email.as_deref())
            .bind(display_name.as_deref())
            .bind(role.as_deref())
            .bind(status.as_deref())
            .bind(input.must_change_password)
            .bind(input.password_hash.as_deref())
            .bind(input.bump_token_version)
            .fetch_one(&mut *tx)
            .await
            .map_err(|err| {
                if is_pg_unique_violation(&err) {
                    StoreError::Conflict("email is already in use".into())
                } else {
                    StoreError::Database(err)
                }
            })?;

        tx.commit().await?;
        Ok(Some(updated.into()))
    }

    async fn touch_user_last_login(&self, user_id: Uuid) -> Result<(), StoreError> {
        sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_user(&self, user_id: Uuid) -> Result<bool, StoreError> {
        let mut tx = self.pool.begin().await?;
        let select_sql = format!("SELECT {USER_COLUMNS} FROM users WHERE id = $1 FOR UPDATE");
        let Some(existing): Option<UserRow> = sqlx::query_as(&select_sql)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };

        if existing.role == ROLE_ADMIN && existing.status == STATUS_ACTIVE {
            let active_admins: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM users WHERE role = 'admin' AND status = 'active' ORDER BY id FOR UPDATE",
            )
            .fetch_all(&mut *tx)
            .await?;
            if !active_admins.iter().any(|&id| id != user_id) {
                return Err(StoreError::Forbidden(
                    "cannot delete the last active admin".into(),
                ));
            }
        }

        // Clear bound metadata before ON DELETE SET NULL clears owner_user_id.
        sqlx::query(
            "UPDATE devices SET owner_user_id = NULL, bound_at = NULL, alias = '', allow_remote_assist = FALSE WHERE owner_user_id = $1",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

        let res = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(res.rows_affected() > 0)
    }

    async fn create_user_session(
        &self,
        input: CreateSessionInput,
    ) -> Result<UserSessionRecord, StoreError> {
        let token_hash = validate_sha256_hex(&input.token_hash, "token_hash")?;
        let device_id = validate_optional_device_id(input.device_id.as_deref())?;
        let client_info =
            validate_text_field(&input.client_info, MAX_CLIENT_INFO_LEN, "client_info")?;

        let id = Uuid::new_v4();
        let family_id = input.family_id.unwrap_or_else(Uuid::new_v4);
        let sql = format!(
            "INSERT INTO user_sessions (id, user_id, device_id, family_id, token_hash, expires_at, client_info)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {SESSION_COLUMNS}"
        );
        let row: UserSessionRow = sqlx::query_as(&sql)
            .bind(id)
            .bind(input.user_id)
            .bind(device_id.as_deref())
            .bind(family_id)
            .bind(&token_hash)
            .bind(input.expires_at)
            .bind(&client_info)
            .fetch_one(&self.pool)
            .await
            .map_err(|err| {
                if is_pg_fk_violation(&err) {
                    StoreError::NotFound(format!("user {} not found", input.user_id))
                } else {
                    StoreError::Database(err)
                }
            })?;
        Ok(row.into())
    }

    async fn get_user_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM user_sessions WHERE id = $1");
        let row: Option<UserSessionRow> = sqlx::query_as(&sql)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn list_user_sessions(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<UserSessionRecord>, StoreError> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS} FROM user_sessions WHERE user_id = $1 ORDER BY created_at DESC"
        );
        let rows: Vec<UserSessionRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn rotate_refresh_token(
        &self,
        presented_token_hash: &str,
        new_token_hash: &str,
        new_expires_at: DateTime<Utc>,
        grace_period: Duration,
    ) -> Result<UserSessionRecord, StoreError> {
        let presented = validate_sha256_hex(presented_token_hash, "presented_token_hash")?;
        let replacement = validate_sha256_hex(new_token_hash, "new_token_hash")?;
        if presented == replacement {
            return Err(StoreError::InvalidInput(
                "new_token_hash must differ from presented_token_hash".into(),
            ));
        }

        let grace_chrono = chrono::Duration::from_std(grace_period)
            .map_err(|_| StoreError::InvalidInput("grace_period out of range".into()))?;
        let now = Utc::now();

        let mut tx = self.pool.begin().await?;
        let select_sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM user_sessions
             WHERE token_hash = $1 OR prev_token_hash = $1 OR $1 = ANY(retired_token_hashes)
             FOR UPDATE"
        );
        let Some(row): Option<UserSessionRow> = sqlx::query_as(&select_sql)
            .bind(&presented)
            .fetch_optional(&mut *tx)
            .await?
        else {
            tx.rollback().await?;
            return Err(StoreError::Unauthorized("invalid refresh token".into()));
        };

        let current_hash = row.token_hash.trim();
        let prev_hash = row.prev_token_hash.as_deref().map(str::trim);

        if current_hash == presented {
            if row.revoked_at.is_some() {
                tx.rollback().await?;
                return Err(StoreError::Unauthorized("session has been revoked".into()));
            }
            if row.expires_at <= now {
                tx.rollback().await?;
                return Err(StoreError::Unauthorized("refresh token expired".into()));
            }

            let grace_until = now + grace_chrono;
            let update_sql = format!(
                "UPDATE user_sessions
                 SET retired_token_hashes = CASE
                         WHEN prev_token_hash IS NOT NULL THEN
                             (array_append(retired_token_hashes, prev_token_hash))[
                                 GREATEST(1, COALESCE(array_length(retired_token_hashes, 1), 0) - 62):
                             ]
                         ELSE retired_token_hashes
                     END,
                     prev_token_hash = token_hash,
                     prev_grace_until = $2,
                     token_hash = $3,
                     rotated_at = $4,
                     expires_at = $5
                 WHERE id = $1
                 RETURNING {SESSION_COLUMNS}"
            );
            let updated: UserSessionRow = sqlx::query_as(&update_sql)
                .bind(row.id)
                .bind(grace_until)
                .bind(&replacement)
                .bind(now)
                .bind(new_expires_at)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(updated.into());
        }

        if prev_hash == Some(presented.as_str()) {
            let within_grace = row.revoked_at.is_none()
                && row.expires_at > now
                && row
                    .prev_grace_until
                    .map(|until| now <= until)
                    .unwrap_or(false);

            if within_grace {
                // One-shot grace retry: consume `prev_grace_until` immediately and retire replaced
                // current `token_hash` so any second reuse triggers family-wide replay revocation.
                let update_sql = format!(
                    "UPDATE user_sessions
                     SET retired_token_hashes = (array_append(retired_token_hashes, token_hash))[
                             GREATEST(1, COALESCE(array_length(retired_token_hashes, 1), 0) - 62):
                         ],
                         prev_grace_until = NULL,
                         token_hash = $2,
                         rotated_at = $3,
                         expires_at = $4
                     WHERE id = $1
                     RETURNING {SESSION_COLUMNS}"
                );
                let updated: UserSessionRow = sqlx::query_as(&update_sql)
                    .bind(row.id)
                    .bind(&replacement)
                    .bind(now)
                    .bind(new_expires_at)
                    .fetch_one(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(updated.into());
            }
        }

        // Outside grace window, duplicate grace reuse, retired token replay, or already revoked:
        // revoke entire token family!
        sqlx::query(
            "UPDATE user_sessions
             SET revoked_at = COALESCE(revoked_at, $2)
             WHERE family_id = $1",
        )
        .bind(row.family_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Err(StoreError::TokenReplayDetected {
            family_id: row.family_id,
            user_id: row.user_id,
        })
    }

    async fn revoke_session(&self, session_id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query(
            "UPDATE user_sessions SET revoked_at = COALESCE(revoked_at, NOW()) WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(session_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_active_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let clean_hash = validate_sha256_hex(token_hash, "token_hash")?;
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM user_sessions
             WHERE (
                 token_hash = $1
                 OR (prev_token_hash = $1 AND prev_grace_until IS NOT NULL AND prev_grace_until >= NOW())
             )
               AND revoked_at IS NULL
               AND expires_at > NOW()"
        );
        let row: Option<UserSessionRow> = sqlx::query_as(&sql)
            .bind(&clean_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn revoke_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let clean_hash = validate_sha256_hex(token_hash, "token_hash")?;
        let sql = format!(
            "UPDATE user_sessions
             SET revoked_at = NOW()
             WHERE (
                 token_hash = $1
                 OR (prev_token_hash = $1 AND prev_grace_until IS NOT NULL AND prev_grace_until >= NOW())
             )
               AND revoked_at IS NULL
               AND expires_at > NOW()
             RETURNING {SESSION_COLUMNS}"
        );
        let row: Option<UserSessionRow> = sqlx::query_as(&sql)
            .bind(&clean_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn revoke_session_family(&self, family_id: Uuid) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE user_sessions SET revoked_at = COALESCE(revoked_at, NOW()) WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn revoke_user_sessions(&self, user_id: Uuid) -> Result<u64, StoreError> {
        let res = sqlx::query(
            "UPDATE user_sessions SET revoked_at = COALESCE(revoked_at, NOW()) WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    async fn revoke_device_sessions(
        &self,
        user_id: Uuid,
        device_id: &str,
    ) -> Result<u64, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(0);
        }
        let res = sqlx::query(
            "UPDATE user_sessions
             SET revoked_at = COALESCE(revoked_at, NOW())
             WHERE user_id = $1 AND device_id = $2 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .bind(&norm)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

#[async_trait]
impl SettingsStore for PgDeviceStore {
    async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let val: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT value_json FROM system_settings WHERE key = $1")
                .bind(key.trim())
                .fetch_optional(&self.pool)
                .await?;
        Ok(val)
    }

    async fn get_allow_registration_with_meta(
        &self,
    ) -> Result<(bool, Option<DateTime<Utc>>), StoreError> {
        let row: Option<(serde_json::Value, DateTime<Utc>)> =
            sqlx::query_as("SELECT value_json, updated_at FROM system_settings WHERE key = $1")
                .bind(SETTING_ALLOW_REGISTRATION)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((serde_json::Value::Bool(b), ts)) => Ok((b, Some(ts))),
            Some((other, _)) => Err(StoreError::InvalidInput(format!(
                "setting '{SETTING_ALLOW_REGISTRATION}' must be a JSON boolean, got {other}"
            ))),
            None => Ok((true, None)),
        }
    }

    async fn set_setting(
        &self,
        key: &str,
        value: serde_json::Value,
        updated_by: Option<Uuid>,
    ) -> Result<(), StoreError> {
        let clean_key = validate_text_field(key, 128, "setting key")?;
        if clean_key.is_empty() {
            return Err(StoreError::InvalidInput(
                "setting key must not be empty".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO system_settings (key, value_json, updated_at, updated_by)
             VALUES ($1, $2, NOW(), $3)
             ON CONFLICT (key) DO UPDATE
             SET value_json = EXCLUDED.value_json,
                 updated_at = NOW(),
                 updated_by = EXCLUDED.updated_by",
        )
        .bind(&clean_key)
        .bind(value)
        .bind(updated_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait]
impl PkiStore for PgDeviceStore {
    async fn get_server_key(&self, key_name: &str) -> Result<Option<ServerKeyRecord>, StoreError> {
        let row: Option<ServerKeyRow> = sqlx::query_as(
            "SELECT key_name, algorithm, private_key_der, public_key_der, created_at
             FROM server_keys WHERE key_name = $1",
        )
        .bind(key_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    async fn put_server_key_if_absent(
        &self,
        key: ServerKeyRecord,
    ) -> Result<ServerKeyRecord, StoreError> {
        let inserted: Option<ServerKeyRow> = sqlx::query_as(
            "INSERT INTO server_keys (key_name, algorithm, private_key_der, public_key_der, created_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (key_name) DO NOTHING
             RETURNING key_name, algorithm, private_key_der, public_key_der, created_at",
        )
        .bind(&key.key_name)
        .bind(&key.algorithm)
        .bind(&key.private_key_der)
        .bind(&key.public_key_der)
        .bind(key.created_at)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            return Ok(row.into());
        }
        self.get_server_key(&key.key_name)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("server key '{}' vanished", key.key_name)))
    }

    async fn get_pki_ca(&self) -> Result<Option<PkiCaRecord>, StoreError> {
        let row: Option<PkiCaRow> = sqlx::query_as(
            "SELECT cert_pem, key_pem, fingerprint_sha256, created_at FROM pki_ca WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    async fn put_pki_ca_if_absent(&self, ca: PkiCaRecord) -> Result<PkiCaRecord, StoreError> {
        let fp = validate_sha256_hex(&ca.fingerprint_sha256, "fingerprint_sha256")?;
        let inserted: Option<PkiCaRow> = sqlx::query_as(
            "INSERT INTO pki_ca (id, cert_pem, key_pem, fingerprint_sha256, created_at)
             VALUES (1, $1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING
             RETURNING cert_pem, key_pem, fingerprint_sha256, created_at",
        )
        .bind(&ca.cert_pem)
        .bind(&ca.key_pem)
        .bind(&fp)
        .bind(ca.created_at)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            return Ok(row.into());
        }
        self.get_pki_ca()
            .await?
            .ok_or_else(|| StoreError::NotFound("pki_ca row vanished".into()))
    }

    async fn insert_device_cert(&self, cert: DeviceCertRecord) -> Result<(), StoreError> {
        let norm_did = normalize_device_id(&cert.device_id);
        if !is_valid_device_id(&norm_did) {
            return Err(StoreError::InvalidInput(format!(
                "invalid device_id: {}",
                cert.device_id
            )));
        }
        let serial_hex = cert.serial_hex.trim().to_ascii_lowercase();
        if serial_hex.is_empty() || !serial_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidInput(
                "serial_hex must be non-empty hex".into(),
            ));
        }
        let fp = validate_sha256_hex(&cert.fingerprint_sha256, "fingerprint_sha256")?;

        sqlx::query(
            "INSERT INTO device_certificates
                (serial_hex, user_id, device_id, fingerprint_sha256, cert_pem, issued_at, expires_at, revoked_at, revoke_reason)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&serial_hex)
        .bind(cert.user_id)
        .bind(&norm_did)
        .bind(&fp)
        .bind(&cert.cert_pem)
        .bind(cert.issued_at)
        .bind(cert.expires_at)
        .bind(cert.revoked_at)
        .bind(cert.revoke_reason.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|err| {
            if is_pg_unique_violation(&err) {
                StoreError::Conflict(format!("certificate serial '{serial_hex}' already exists"))
            } else if is_pg_fk_violation(&err) {
                StoreError::NotFound(format!("user {} not found", cert.user_id))
            } else {
                StoreError::Database(err)
            }
        })?;
        Ok(())
    }

    async fn get_device_cert(
        &self,
        serial_hex: &str,
    ) -> Result<Option<DeviceCertRecord>, StoreError> {
        let norm = serial_hex.trim().to_ascii_lowercase();
        if norm.is_empty() {
            return Ok(None);
        }
        let sql = format!("SELECT {CERT_COLUMNS} FROM device_certificates WHERE serial_hex = $1");
        let row: Option<DeviceCertRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn list_device_certs(
        &self,
        user_id: Option<Uuid>,
        device_id: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let norm_did = validate_optional_device_id(device_id)?;
        let sql = format!(
            "SELECT {CERT_COLUMNS}
             FROM device_certificates
             WHERE ($1::uuid IS NULL OR user_id = $1)
               AND ($2::char(9) IS NULL OR device_id = $2)
             ORDER BY issued_at DESC"
        );
        let rows: Vec<DeviceCertRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .bind(norm_did.as_deref())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn revoke_cert_by_serial(
        &self,
        serial_hex: &str,
        reason: Option<&str>,
    ) -> Result<Option<DeviceCertRecord>, StoreError> {
        let norm = serial_hex.trim().to_ascii_lowercase();
        if norm.is_empty() {
            return Ok(None);
        }
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let sql = format!(
            "UPDATE device_certificates
             SET revoked_at = COALESCE(revoked_at, NOW()),
                 revoke_reason = COALESCE(revoke_reason, $2)
             WHERE serial_hex = $1
             RETURNING {CERT_COLUMNS}"
        );
        let row: Option<DeviceCertRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .bind(clean_reason.as_deref())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    async fn revoke_user_certs(
        &self,
        user_id: Uuid,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let sql = format!(
            "UPDATE device_certificates
             SET revoked_at = NOW(),
                 revoke_reason = COALESCE(revoke_reason, $2)
             WHERE user_id = $1 AND revoked_at IS NULL
             RETURNING {CERT_COLUMNS}"
        );
        let rows: Vec<DeviceCertRow> = sqlx::query_as(&sql)
            .bind(user_id)
            .bind(clean_reason.as_deref())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn revoke_device_certs(
        &self,
        device_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(Vec::new());
        }
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let sql = format!(
            "UPDATE device_certificates
             SET revoked_at = NOW(),
                 revoke_reason = COALESCE(revoke_reason, $2)
             WHERE device_id = $1 AND revoked_at IS NULL
             RETURNING {CERT_COLUMNS}"
        );
        let rows: Vec<DeviceCertRow> = sqlx::query_as(&sql)
            .bind(&norm)
            .bind(clean_reason.as_deref())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    async fn list_active_revoked_certs(&self) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let sql = format!(
            "SELECT {CERT_COLUMNS}
             FROM device_certificates
             WHERE revoked_at IS NOT NULL AND expires_at > NOW()
             ORDER BY revoked_at DESC"
        );
        let rows: Vec<DeviceCertRow> = sqlx::query_as(&sql).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[async_trait]
impl AuditStore for PgDeviceStore {
    async fn append_audit_log(
        &self,
        actor_user_id: Option<Uuid>,
        target_device_id: Option<&str>,
        action: &str,
        detail_json: serde_json::Value,
    ) -> Result<(), StoreError> {
        let clean_action = validate_text_field(action, MAX_AUDIT_ACTION_LEN, "action")?;
        if clean_action.is_empty() {
            return Err(StoreError::InvalidInput("action must not be empty".into()));
        }
        let norm_did = validate_optional_device_id(target_device_id)?;
        sqlx::query(
            "INSERT INTO audit_logs (actor_user_id, target_device_id, action, detail_json)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(actor_user_id)
        .bind(norm_did.as_deref())
        .bind(&clean_action)
        .bind(detail_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_audit_logs(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditLogRecord>, StoreError> {
        let limit_i64 = limit.clamp(1, MAX_LIST_LIMIT) as i64;
        let offset_i64 = i64::try_from(offset).unwrap_or(i64::MAX);
        let rows: Vec<AuditLogRow> = sqlx::query_as(
            "SELECT id, actor_user_id, target_device_id, action, detail_json, created_at
             FROM audit_logs
             ORDER BY id DESC
             LIMIT $1 OFFSET $2",
        )
        .bind(limit_i64)
        .bind(offset_i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[derive(Debug)]
struct MemoryState {
    by_id: HashMap<String, DeviceRecord>,
    by_key: HashMap<String, String>,
    relay_logs: Vec<RelaySessionLog>,
    users_by_id: HashMap<Uuid, UserRecord>,
    user_id_by_email: HashMap<String, Uuid>,
    sessions_by_id: HashMap<Uuid, UserSessionRecord>,
    session_by_retired_hash: HashMap<String, Uuid>,
    settings: HashMap<String, serde_json::Value>,
    setting_updated_at: HashMap<String, DateTime<Utc>>,
    server_keys: HashMap<String, ServerKeyRecord>,
    pki_ca: Option<PkiCaRecord>,
    device_certs: HashMap<String, DeviceCertRecord>,
    audit_logs: Vec<AuditLogRecord>,
    next_audit_id: i64,
}

impl Default for MemoryState {
    fn default() -> Self {
        let mut settings = HashMap::new();
        settings.insert(
            SETTING_ALLOW_REGISTRATION.to_string(),
            serde_json::Value::Bool(true),
        );
        let mut setting_updated_at = HashMap::new();
        setting_updated_at.insert(SETTING_ALLOW_REGISTRATION.to_string(), Utc::now());
        Self {
            by_id: HashMap::new(),
            by_key: HashMap::new(),
            relay_logs: Vec::new(),
            users_by_id: HashMap::new(),
            user_id_by_email: HashMap::new(),
            sessions_by_id: HashMap::new(),
            session_by_retired_hash: HashMap::new(),
            settings,
            setting_updated_at,
            server_keys: HashMap::new(),
            pki_ca: None,
            device_certs: HashMap::new(),
            audit_logs: Vec::new(),
            next_audit_id: 1,
        }
    }
}

/// In-memory `AuthStore` (`DeviceStore` + `UserStore` + `SettingsStore` + `PkiStore` + `AuditStore`)
/// for fast offline unit and integration tests without PostgreSQL.
#[derive(Debug, Default)]
pub struct MemoryDeviceStore {
    state: RwLock<MemoryState>,
}

impl MemoryDeviceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn relay_logs(&self) -> Vec<RelaySessionLog> {
        self.state
            .read()
            .expect("MemoryDeviceStore lock poisoned")
            .relay_logs
            .clone()
    }
}

#[async_trait]
impl DeviceStore for MemoryDeviceStore {
    async fn register_or_get(&self, input: RegisterInput) -> Result<DeviceRecord, StoreError> {
        let (device_key, hostname, os, device_group) = validate_register_input(&input)?;
        let now = Utc::now();

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(existing_id) = guard.by_key.get(&device_key).cloned() {
            if let Some(existing) = guard.by_id.get_mut(&existing_id) {
                if existing.disabled {
                    return Err(StoreError::DeviceDisabled(existing.device_id.clone()));
                }
                if !hostname.is_empty() {
                    existing.hostname = hostname;
                }
                if !os.is_empty() {
                    existing.os = os;
                }
                if !device_group.is_empty() {
                    existing.device_group = device_group;
                }
                existing.last_seen_at = now;
                return Ok(existing.clone());
            }
        }

        for _ in 0..MAX_ID_GENERATION_ATTEMPTS {
            let candidate_id = generate_device_id();
            if !guard.by_id.contains_key(&candidate_id) {
                let record = DeviceRecord {
                    device_id: candidate_id.clone(),
                    device_key: device_key.clone(),
                    hostname,
                    os,
                    device_group,
                    disabled: false,
                    created_at: now,
                    last_seen_at: now,
                    owner_user_id: None,
                    alias: String::new(),
                    device_pubkey: String::new(),
                    allow_remote_assist: false,
                    bound_at: None,
                };
                guard.by_key.insert(device_key, candidate_id.clone());
                guard.by_id.insert(candidate_id, record.clone());
                return Ok(record);
            }
        }

        Err(StoreError::IdAllocationExhausted)
    }

    async fn get_by_id(&self, device_id: &str) -> Result<Option<DeviceRecord>, StoreError> {
        let norm = normalize_device_id(device_id);
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.by_id.get(&norm).cloned())
    }

    async fn get_by_key(&self, device_key: &str) -> Result<Option<DeviceRecord>, StoreError> {
        let clean = device_key.trim();
        if clean.is_empty() {
            return Ok(None);
        }
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard
            .by_key
            .get(clean)
            .and_then(|id| guard.by_id.get(id))
            .cloned())
    }

    async fn verify_key(&self, device_id: &str, device_key: &str) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard
            .by_id
            .get(&norm)
            .map(|r| r.device_key == device_key.trim())
            .unwrap_or(false))
    }

    async fn list_devices(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<DeviceRecord>, StoreError> {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let q_lower = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let q_norm = q_lower.as_deref().map(normalize_device_id);

        let mut list: Vec<DeviceRecord> = guard
            .by_id
            .values()
            .filter(|rec| memory_device_matches(&guard, rec, q_lower.as_deref(), q_norm.as_deref()))
            .cloned()
            .collect();

        list.sort_by(|a, b| {
            b.last_seen_at
                .cmp(&a.last_seen_at)
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        Ok(list.into_iter().skip(offset).take(limit).collect())
    }

    async fn count_devices_filtered(&self, query: Option<&str>) -> Result<u64, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let q_lower = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let q_norm = q_lower.as_deref().map(normalize_device_id);
        let count = guard
            .by_id
            .values()
            .filter(|rec| memory_device_matches(&guard, rec, q_lower.as_deref(), q_norm.as_deref()))
            .count();
        Ok(count as u64)
    }

    async fn touch_last_seen(&self, device_id: &str) -> Result<(), StoreError> {
        let norm = normalize_device_id(device_id);
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            rec.last_seen_at = Utc::now();
        }
        Ok(())
    }

    async fn set_disabled(&self, device_id: &str, disabled: bool) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            rec.disabled = disabled;
            return Ok(true);
        }
        Ok(false)
    }

    async fn delete_device(&self, device_id: &str) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(removed) = guard.by_id.remove(&norm) {
            guard.by_key.remove(&removed.device_key);
            return Ok(true);
        }
        Ok(false)
    }

    async fn count_devices(&self) -> Result<u64, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.by_id.len() as u64)
    }

    async fn health_check(&self) -> Result<(), StoreError> {
        Ok(())
    }

    async fn record_relay_end(
        &self,
        session_id: &str,
        host_id: &str,
        client_id: &str,
        bytes_up: u64,
        bytes_down: u64,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let norm_host = normalize_device_id(host_id);
        let norm_client = normalize_device_id(client_id);
        if !is_valid_device_id(&norm_host) {
            return Err(StoreError::InvalidInput(format!(
                "invalid host_id: {host_id}"
            )));
        }
        if !is_valid_device_id(&norm_client) {
            return Err(StoreError::InvalidInput(format!(
                "invalid client_id: {client_id}"
            )));
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        guard.relay_logs.push(RelaySessionLog {
            session_id: session_id.to_string(),
            host_id: norm_host,
            client_id: norm_client,
            bytes_up,
            bytes_down,
            started_at,
            ended_at,
        });
        Ok(())
    }

    async fn bind_device_to_user_with_prev(
        &self,
        device_id: &str,
        user_id: Uuid,
        alias: Option<&str>,
        device_pubkey: Option<&str>,
    ) -> Result<Option<(DeviceRecord, Option<Uuid>)>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(None);
        }
        let alias = alias
            .map(|a| validate_text_field(a, MAX_DEVICE_ALIAS_LEN, "alias"))
            .transpose()?;
        let pubkey = device_pubkey
            .map(|k| validate_text_field(k, MAX_DEVICE_PUBKEY_LEN, "device_pubkey"))
            .transpose()?;

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let Some(rec) = guard.by_id.get_mut(&norm) else {
            return Ok(None);
        };
        let prev_owner = rec.owner_user_id;
        if rec.owner_user_id != Some(user_id) {
            rec.allow_remote_assist = false;
            if alias.is_none() {
                rec.alias.clear();
            }
            if pubkey.is_none() {
                rec.device_pubkey.clear();
            }
        }
        if let Some(a) = alias {
            rec.alias = a;
        }
        if let Some(pk) = pubkey {
            rec.device_pubkey = pk;
        }
        rec.owner_user_id = Some(user_id);
        rec.bound_at = Some(Utc::now());
        Ok(Some((rec.clone(), prev_owner)))
    }

    async fn list_user_devices(&self, user_id: Uuid) -> Result<Vec<DeviceRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let mut list: Vec<DeviceRecord> = guard
            .by_id
            .values()
            .filter(|rec| rec.owner_user_id == Some(user_id))
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            b.last_seen_at
                .cmp(&a.last_seen_at)
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        Ok(list)
    }

    async fn update_device_alias(
        &self,
        device_id: &str,
        owner_user_id: Uuid,
        alias: &str,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let clean = validate_text_field(alias, MAX_DEVICE_ALIAS_LEN, "alias")?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            if rec.owner_user_id == Some(owner_user_id) {
                rec.alias = clean;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn unbind_device(
        &self,
        device_id: &str,
        owner_user_id: Uuid,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            if rec.owner_user_id == Some(owner_user_id) {
                rec.owner_user_id = None;
                rec.bound_at = None;
                rec.alias.clear();
                rec.allow_remote_assist = false;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn admin_unbind_device(&self, device_id: &str) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            if rec.owner_user_id.is_some() {
                rec.owner_user_id = None;
                rec.bound_at = None;
                rec.alias.clear();
                rec.allow_remote_assist = false;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn set_device_allow_remote_assist(
        &self,
        device_id: &str,
        owner_user_id: Option<Uuid>,
        allow: bool,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            if owner_user_id.is_none() || rec.owner_user_id == owner_user_id {
                rec.allow_remote_assist = allow;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn get_device_owner(&self, device_id: &str) -> Result<Option<Uuid>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(None);
        }
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.by_id.get(&norm).and_then(|r| r.owner_user_id))
    }

    async fn set_device_pubkey(
        &self,
        device_id: &str,
        device_pubkey: &str,
    ) -> Result<bool, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(false);
        }
        let clean = validate_text_field(device_pubkey, MAX_DEVICE_PUBKEY_LEN, "device_pubkey")?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(rec) = guard.by_id.get_mut(&norm) {
            rec.device_pubkey = clean;
            return Ok(true);
        }
        Ok(false)
    }
}

#[async_trait]
impl UserStore for MemoryDeviceStore {
    async fn create_user(&self, input: CreateUserInput) -> Result<UserRecord, StoreError> {
        let email = validate_email(&input.email)?;
        let display_name =
            validate_text_field(&input.display_name, MAX_DISPLAY_NAME_LEN, "display_name")?;
        let role = validate_role(&input.role)?;
        let status = validate_status(&input.status)?;
        if input.password_hash.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "password_hash must not be empty".into(),
            ));
        }

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if guard.user_id_by_email.contains_key(&email) {
            return Err(StoreError::Conflict(format!(
                "email '{email}' is already registered"
            )));
        }

        let now = Utc::now();
        let record = UserRecord {
            id: Uuid::new_v4(),
            email: email.clone(),
            display_name,
            password_hash: input.password_hash,
            role,
            status,
            must_change_password: input.must_change_password,
            token_version: 1,
            created_at: now,
            updated_at: now,
            last_login_at: None,
        };
        guard.user_id_by_email.insert(email, record.id);
        guard.users_by_id.insert(record.id, record.clone());
        Ok(record)
    }

    async fn get_user_by_id(&self, user_id: Uuid) -> Result<Option<UserRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.users_by_id.get(&user_id).cloned())
    }

    async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRecord>, StoreError> {
        let norm = normalize_email(email);
        if norm.is_empty() {
            return Ok(None);
        }
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard
            .user_id_by_email
            .get(&norm)
            .and_then(|id| guard.users_by_id.get(id))
            .cloned())
    }

    async fn list_users(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<UserWithDeviceCount>, StoreError> {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let q_lower = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);

        let mut counts: HashMap<Uuid, u64> = HashMap::new();
        for dev in guard.by_id.values() {
            if let Some(uid) = dev.owner_user_id {
                *counts.entry(uid).or_insert(0) += 1;
            }
        }

        let mut list: Vec<UserWithDeviceCount> = guard
            .users_by_id
            .values()
            .filter(|u| match &q_lower {
                Some(q) => u.email.contains(q) || u.display_name.to_lowercase().contains(q),
                None => true,
            })
            .map(|u| UserWithDeviceCount {
                user: u.clone(),
                device_count: counts.get(&u.id).copied().unwrap_or(0),
            })
            .collect();

        list.sort_by(|a, b| {
            b.user
                .created_at
                .cmp(&a.user.created_at)
                .then_with(|| a.user.email.cmp(&b.user.email))
        });
        Ok(list.into_iter().skip(offset).take(limit).collect())
    }

    async fn count_users(&self, query: Option<&str>) -> Result<u64, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let q_lower = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);
        let count = guard
            .users_by_id
            .values()
            .filter(|u| match &q_lower {
                Some(q) => u.email.contains(q) || u.display_name.to_lowercase().contains(q),
                None => true,
            })
            .count();
        Ok(count as u64)
    }

    async fn count_active_admins(&self) -> Result<u64, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let count = guard
            .users_by_id
            .values()
            .filter(|u| u.role == ROLE_ADMIN && u.status == STATUS_ACTIVE)
            .count();
        Ok(count as u64)
    }

    async fn update_user(
        &self,
        user_id: Uuid,
        input: UpdateUserInput,
    ) -> Result<Option<UserRecord>, StoreError> {
        let email = input.email.as_deref().map(validate_email).transpose()?;
        let display_name = input
            .display_name
            .as_deref()
            .map(|n| validate_text_field(n, MAX_DISPLAY_NAME_LEN, "display_name"))
            .transpose()?;
        let role = input.role.as_deref().map(validate_role).transpose()?;
        let status = input.status.as_deref().map(validate_status).transpose()?;
        if let Some(ref hash) = input.password_hash {
            if hash.trim().is_empty() {
                return Err(StoreError::InvalidInput(
                    "password_hash must not be empty".into(),
                ));
            }
        }

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let Some(existing) = guard.users_by_id.get(&user_id).cloned() else {
            return Ok(None);
        };

        if let Some(ref next_email) = email {
            if let Some(&owner_id) = guard.user_id_by_email.get(next_email) {
                if owner_id != user_id {
                    return Err(StoreError::Conflict("email is already in use".into()));
                }
            }
        }

        let next_role = role.as_deref().unwrap_or(&existing.role);
        let next_status = status.as_deref().unwrap_or(&existing.status);
        let was_active_admin = existing.role == ROLE_ADMIN && existing.status == STATUS_ACTIVE;
        let stays_active_admin = next_role == ROLE_ADMIN && next_status == STATUS_ACTIVE;
        if was_active_admin && !stays_active_admin {
            let other_admins = guard
                .users_by_id
                .values()
                .filter(|u| u.id != user_id && u.role == ROLE_ADMIN && u.status == STATUS_ACTIVE)
                .count();
            if other_admins == 0 {
                return Err(StoreError::Forbidden(
                    "cannot demote or disable the last active admin".into(),
                ));
            }
        }

        if let Some(ref next_email) = email {
            if next_email != &existing.email {
                guard.user_id_by_email.remove(&existing.email);
                guard.user_id_by_email.insert(next_email.clone(), user_id);
            }
        }

        let user = guard
            .users_by_id
            .get_mut(&user_id)
            .expect("verified user exists");
        if let Some(e) = email {
            user.email = e;
        }
        if let Some(n) = display_name {
            user.display_name = n;
        }
        if let Some(r) = role {
            user.role = r;
        }
        if let Some(s) = status {
            user.status = s;
        }
        if let Some(m) = input.must_change_password {
            user.must_change_password = m;
        }
        if let Some(h) = input.password_hash {
            user.password_hash = h;
        }
        if input.bump_token_version {
            user.token_version += 1;
        }
        user.updated_at = Utc::now();
        Ok(Some(user.clone()))
    }

    async fn touch_user_last_login(&self, user_id: Uuid) -> Result<(), StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(user) = guard.users_by_id.get_mut(&user_id) {
            user.last_login_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn delete_user(&self, user_id: Uuid) -> Result<bool, StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let Some(existing) = guard.users_by_id.get(&user_id).cloned() else {
            return Ok(false);
        };

        if existing.role == ROLE_ADMIN && existing.status == STATUS_ACTIVE {
            let other_admins = guard
                .users_by_id
                .values()
                .filter(|u| u.id != user_id && u.role == ROLE_ADMIN && u.status == STATUS_ACTIVE)
                .count();
            if other_admins == 0 {
                return Err(StoreError::Forbidden(
                    "cannot delete the last active admin".into(),
                ));
            }
        }

        guard.users_by_id.remove(&user_id);
        guard.user_id_by_email.remove(&existing.email);
        guard.sessions_by_id.retain(|_, s| s.user_id != user_id);
        let remaining_sessions: std::collections::HashSet<Uuid> =
            guard.sessions_by_id.keys().copied().collect();
        guard
            .session_by_retired_hash
            .retain(|_, sid| remaining_sessions.contains(sid));
        guard.device_certs.retain(|_, c| c.user_id != user_id);
        for dev in guard.by_id.values_mut() {
            if dev.owner_user_id == Some(user_id) {
                dev.owner_user_id = None;
                dev.bound_at = None;
                dev.alias.clear();
                dev.allow_remote_assist = false;
            }
        }
        Ok(true)
    }

    async fn create_user_session(
        &self,
        input: CreateSessionInput,
    ) -> Result<UserSessionRecord, StoreError> {
        let token_hash = validate_sha256_hex(&input.token_hash, "token_hash")?;
        let device_id = validate_optional_device_id(input.device_id.as_deref())?;
        let client_info =
            validate_text_field(&input.client_info, MAX_CLIENT_INFO_LEN, "client_info")?;

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if !guard.users_by_id.contains_key(&input.user_id) {
            return Err(StoreError::NotFound(format!(
                "user {} not found",
                input.user_id
            )));
        }
        if guard
            .sessions_by_id
            .values()
            .any(|s| s.token_hash == token_hash)
        {
            return Err(StoreError::Conflict("duplicate session token_hash".into()));
        }

        let now = Utc::now();
        let rec = UserSessionRecord {
            id: Uuid::new_v4(),
            user_id: input.user_id,
            device_id,
            family_id: input.family_id.unwrap_or_else(Uuid::new_v4),
            token_hash,
            prev_token_hash: None,
            prev_grace_until: None,
            rotated_at: now,
            expires_at: input.expires_at,
            revoked_at: None,
            client_info,
            created_at: now,
        };
        guard.sessions_by_id.insert(rec.id, rec.clone());
        Ok(rec)
    }

    async fn get_user_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.sessions_by_id.get(&session_id).cloned())
    }

    async fn list_user_sessions(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<UserSessionRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let mut list: Vec<UserSessionRecord> = guard
            .sessions_by_id
            .values()
            .filter(|s| s.user_id == user_id)
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list)
    }

    async fn rotate_refresh_token(
        &self,
        presented_token_hash: &str,
        new_token_hash: &str,
        new_expires_at: DateTime<Utc>,
        grace_period: Duration,
    ) -> Result<UserSessionRecord, StoreError> {
        let presented = validate_sha256_hex(presented_token_hash, "presented_token_hash")?;
        let replacement = validate_sha256_hex(new_token_hash, "new_token_hash")?;
        if presented == replacement {
            return Err(StoreError::InvalidInput(
                "new_token_hash must differ from presented_token_hash".into(),
            ));
        }

        let grace_chrono = chrono::Duration::from_std(grace_period)
            .map_err(|_| StoreError::InvalidInput("grace_period out of range".into()))?;
        let now = Utc::now();

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(&sid) = guard.session_by_retired_hash.get(&presented) {
            if let Some((family_id, user_id)) = guard
                .sessions_by_id
                .get(&sid)
                .map(|s| (s.family_id, s.user_id))
            {
                for session in guard.sessions_by_id.values_mut() {
                    if session.family_id == family_id && session.revoked_at.is_none() {
                        session.revoked_at = Some(now);
                    }
                }
                return Err(StoreError::TokenReplayDetected { family_id, user_id });
            }
        }

        let matched_id = guard
            .sessions_by_id
            .iter()
            .find(|(_, s)| {
                s.token_hash == presented || s.prev_token_hash.as_deref() == Some(&presented)
            })
            .map(|(&id, _)| id);

        let Some(sid) = matched_id else {
            return Err(StoreError::Unauthorized("invalid refresh token".into()));
        };

        let existing = guard
            .sessions_by_id
            .get(&sid)
            .cloned()
            .expect("session exists");

        if existing.token_hash == presented {
            if existing.revoked_at.is_some() {
                return Err(StoreError::Unauthorized("session has been revoked".into()));
            }
            if existing.expires_at <= now {
                return Err(StoreError::Unauthorized("refresh token expired".into()));
            }
            if let Some(old_prev) = existing.prev_token_hash.clone() {
                guard.session_by_retired_hash.insert(old_prev, sid);
            }
            let session = guard.sessions_by_id.get_mut(&sid).unwrap();
            session.prev_token_hash = Some(session.token_hash.clone());
            session.prev_grace_until = Some(now + grace_chrono);
            session.token_hash = replacement;
            session.rotated_at = now;
            session.expires_at = new_expires_at;
            return Ok(session.clone());
        }

        // Matched prev_token_hash
        let within_grace = existing.revoked_at.is_none()
            && existing.expires_at > now
            && existing
                .prev_grace_until
                .map(|until| now <= until)
                .unwrap_or(false);

        if within_grace {
            guard
                .session_by_retired_hash
                .insert(existing.token_hash.clone(), sid);
            let session = guard.sessions_by_id.get_mut(&sid).unwrap();
            // One-shot grace consumption: clear prev_grace_until so a second replay revokes family.
            session.prev_grace_until = None;
            session.token_hash = replacement;
            session.rotated_at = now;
            session.expires_at = new_expires_at;
            return Ok(session.clone());
        }

        let family_id = existing.family_id;
        let user_id = existing.user_id;
        for session in guard.sessions_by_id.values_mut() {
            if session.family_id == family_id && session.revoked_at.is_none() {
                session.revoked_at = Some(now);
            }
        }
        Err(StoreError::TokenReplayDetected { family_id, user_id })
    }

    async fn revoke_session(&self, session_id: Uuid) -> Result<bool, StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(s) = guard.sessions_by_id.get_mut(&session_id) {
            if s.revoked_at.is_none() {
                s.revoked_at = Some(Utc::now());
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn get_active_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let clean_hash = validate_sha256_hex(token_hash, "token_hash")?;
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        for s in guard.sessions_by_id.values() {
            if s.revoked_at.is_some() || s.expires_at <= now {
                continue;
            }
            let matches_curr = s.token_hash == clean_hash;
            let matches_prev_grace = s.prev_token_hash.as_deref() == Some(clean_hash.as_str())
                && s.prev_grace_until.is_some_and(|u| u >= now);
            if matches_curr || matches_prev_grace {
                return Ok(Some(s.clone()));
            }
        }
        Ok(None)
    }

    async fn revoke_session_by_refresh_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<UserSessionRecord>, StoreError> {
        let clean_hash = validate_sha256_hex(token_hash, "token_hash")?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        for s in guard.sessions_by_id.values_mut() {
            if s.revoked_at.is_some() || s.expires_at <= now {
                continue;
            }
            let matches_curr = s.token_hash == clean_hash;
            let matches_prev_grace = s.prev_token_hash.as_deref() == Some(clean_hash.as_str())
                && s.prev_grace_until.is_some_and(|u| u >= now);
            if matches_curr || matches_prev_grace {
                s.revoked_at = Some(now);
                return Ok(Some(s.clone()));
            }
        }
        Ok(None)
    }

    async fn revoke_session_family(&self, family_id: Uuid) -> Result<u64, StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        let mut count = 0;
        for s in guard.sessions_by_id.values_mut() {
            if s.family_id == family_id && s.revoked_at.is_none() {
                s.revoked_at = Some(now);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn revoke_user_sessions(&self, user_id: Uuid) -> Result<u64, StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        let mut count = 0;
        for s in guard.sessions_by_id.values_mut() {
            if s.user_id == user_id && s.revoked_at.is_none() {
                s.revoked_at = Some(now);
                count += 1;
            }
        }
        Ok(count)
    }

    async fn revoke_device_sessions(
        &self,
        user_id: Uuid,
        device_id: &str,
    ) -> Result<u64, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(0);
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        let mut count = 0;
        for s in guard.sessions_by_id.values_mut() {
            if s.user_id == user_id
                && s.device_id.as_deref() == Some(norm.as_str())
                && s.revoked_at.is_none()
            {
                s.revoked_at = Some(now);
                count += 1;
            }
        }
        Ok(count)
    }
}

#[async_trait]
impl SettingsStore for MemoryDeviceStore {
    async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.settings.get(key.trim()).cloned())
    }

    async fn get_allow_registration_with_meta(
        &self,
    ) -> Result<(bool, Option<DateTime<Utc>>), StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let ts = guard
            .setting_updated_at
            .get(SETTING_ALLOW_REGISTRATION)
            .copied();
        let allowed = match guard.settings.get(SETTING_ALLOW_REGISTRATION) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(other) => {
                return Err(StoreError::InvalidInput(format!(
                    "setting '{SETTING_ALLOW_REGISTRATION}' must be a JSON boolean, got {other}"
                )));
            }
            None => true,
        };
        Ok((allowed, ts))
    }

    async fn set_setting(
        &self,
        key: &str,
        value: serde_json::Value,
        _updated_by: Option<Uuid>,
    ) -> Result<(), StoreError> {
        let clean_key = validate_text_field(key, 128, "setting key")?;
        if clean_key.is_empty() {
            return Err(StoreError::InvalidInput(
                "setting key must not be empty".into(),
            ));
        }
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        guard.settings.insert(clean_key.clone(), value);
        guard.setting_updated_at.insert(clean_key, Utc::now());
        Ok(())
    }
}

fn memory_device_matches(
    guard: &MemoryState,
    rec: &DeviceRecord,
    q_lower: Option<&str>,
    q_norm: Option<&str>,
) -> bool {
    match (q_lower, q_norm) {
        (Some(q), Some(qn)) => {
            let email_match = rec
                .owner_user_id
                .and_then(|uid| guard.users_by_id.get(&uid))
                .is_some_and(|u| u.email.to_lowercase().contains(q));
            (!qn.is_empty() && rec.device_id.contains(qn))
                || rec.hostname.to_lowercase().contains(q)
                || rec.os.to_lowercase().contains(q)
                || rec.device_group.to_lowercase().contains(q)
                || rec.alias.to_lowercase().contains(q)
                || email_match
        }
        _ => true,
    }
}

#[async_trait]
impl PkiStore for MemoryDeviceStore {
    async fn get_server_key(&self, key_name: &str) -> Result<Option<ServerKeyRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.server_keys.get(key_name).cloned())
    }

    async fn put_server_key_if_absent(
        &self,
        key: ServerKeyRecord,
    ) -> Result<ServerKeyRecord, StoreError> {
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let entry = guard.server_keys.entry(key.key_name.clone()).or_insert(key);
        Ok(entry.clone())
    }

    async fn get_pki_ca(&self) -> Result<Option<PkiCaRecord>, StoreError> {
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.pki_ca.clone())
    }

    async fn put_pki_ca_if_absent(&self, ca: PkiCaRecord) -> Result<PkiCaRecord, StoreError> {
        let fp = validate_sha256_hex(&ca.fingerprint_sha256, "fingerprint_sha256")?;
        let mut ca = ca;
        ca.fingerprint_sha256 = fp;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(existing) = &guard.pki_ca {
            return Ok(existing.clone());
        }
        guard.pki_ca = Some(ca.clone());
        Ok(ca)
    }

    async fn insert_device_cert(&self, cert: DeviceCertRecord) -> Result<(), StoreError> {
        let norm_did = normalize_device_id(&cert.device_id);
        if !is_valid_device_id(&norm_did) {
            return Err(StoreError::InvalidInput(format!(
                "invalid device_id: {}",
                cert.device_id
            )));
        }
        let serial_hex = cert.serial_hex.trim().to_ascii_lowercase();
        if serial_hex.is_empty() || !serial_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidInput(
                "serial_hex must be non-empty hex".into(),
            ));
        }
        let fp = validate_sha256_hex(&cert.fingerprint_sha256, "fingerprint_sha256")?;

        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if !guard.users_by_id.contains_key(&cert.user_id) {
            return Err(StoreError::NotFound(format!(
                "user {} not found",
                cert.user_id
            )));
        }
        if guard.device_certs.contains_key(&serial_hex) {
            return Err(StoreError::Conflict(format!(
                "certificate serial '{serial_hex}' already exists"
            )));
        }
        let mut normalized = cert;
        normalized.serial_hex = serial_hex.clone();
        normalized.device_id = norm_did;
        normalized.fingerprint_sha256 = fp;
        guard.device_certs.insert(serial_hex, normalized);
        Ok(())
    }

    async fn get_device_cert(
        &self,
        serial_hex: &str,
    ) -> Result<Option<DeviceCertRecord>, StoreError> {
        let norm = serial_hex.trim().to_ascii_lowercase();
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard.device_certs.get(&norm).cloned())
    }

    async fn list_device_certs(
        &self,
        user_id: Option<Uuid>,
        device_id: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let norm_did = validate_optional_device_id(device_id)?;
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let mut list: Vec<DeviceCertRecord> = guard
            .device_certs
            .values()
            .filter(|c| user_id.is_none_or(|u| c.user_id == u))
            .filter(|c| norm_did.as_deref().is_none_or(|d| c.device_id == d))
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.issued_at));
        Ok(list)
    }

    async fn revoke_cert_by_serial(
        &self,
        serial_hex: &str,
        reason: Option<&str>,
    ) -> Result<Option<DeviceCertRecord>, StoreError> {
        let norm = serial_hex.trim().to_ascii_lowercase();
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        if let Some(cert) = guard.device_certs.get_mut(&norm) {
            if cert.revoked_at.is_none() {
                cert.revoked_at = Some(Utc::now());
            }
            if cert.revoke_reason.is_none() {
                cert.revoke_reason = clean_reason;
            }
            return Ok(Some(cert.clone()));
        }
        Ok(None)
    }

    async fn revoke_user_certs(
        &self,
        user_id: Uuid,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        let mut revoked = Vec::new();
        for cert in guard.device_certs.values_mut() {
            if cert.user_id == user_id && cert.revoked_at.is_none() {
                cert.revoked_at = Some(now);
                if cert.revoke_reason.is_none() {
                    cert.revoke_reason = clean_reason.clone();
                }
                revoked.push(cert.clone());
            }
        }
        Ok(revoked)
    }

    async fn revoke_device_certs(
        &self,
        device_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let norm = normalize_device_id(device_id);
        if !is_valid_device_id(&norm) {
            return Ok(Vec::new());
        }
        let clean_reason = reason
            .map(|r| validate_text_field(r, MAX_REVOKE_REASON_LEN, "revoke_reason"))
            .transpose()?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let now = Utc::now();
        let mut revoked = Vec::new();
        for cert in guard.device_certs.values_mut() {
            if cert.device_id == norm && cert.revoked_at.is_none() {
                cert.revoked_at = Some(now);
                if cert.revoke_reason.is_none() {
                    cert.revoke_reason = clean_reason.clone();
                }
                revoked.push(cert.clone());
            }
        }
        Ok(revoked)
    }

    async fn list_active_revoked_certs(&self) -> Result<Vec<DeviceCertRecord>, StoreError> {
        let now = Utc::now();
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        let mut list: Vec<DeviceCertRecord> = guard
            .device_certs
            .values()
            .filter(|c| c.revoked_at.is_some() && c.expires_at > now)
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.revoked_at));
        Ok(list)
    }
}

#[async_trait]
impl AuditStore for MemoryDeviceStore {
    async fn append_audit_log(
        &self,
        actor_user_id: Option<Uuid>,
        target_device_id: Option<&str>,
        action: &str,
        detail_json: serde_json::Value,
    ) -> Result<(), StoreError> {
        let clean_action = validate_text_field(action, MAX_AUDIT_ACTION_LEN, "action")?;
        if clean_action.is_empty() {
            return Err(StoreError::InvalidInput("action must not be empty".into()));
        }
        let norm_did = validate_optional_device_id(target_device_id)?;
        let mut guard = self.state.write().expect("MemoryDeviceStore lock poisoned");
        let id = guard.next_audit_id;
        guard.next_audit_id += 1;
        guard.audit_logs.push(AuditLogRecord {
            id,
            actor_user_id,
            target_device_id: norm_did,
            action: clean_action,
            detail_json,
            created_at: Utc::now(),
        });
        Ok(())
    }

    async fn list_audit_logs(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditLogRecord>, StoreError> {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        let guard = self.state.read().expect("MemoryDeviceStore lock poisoned");
        Ok(guard
            .audit_logs
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_helpers_work() {
        for _ in 0..200 {
            let id = generate_device_id();
            assert!(is_valid_device_id(&id), "invalid generated id: {id}");
            let num: u32 = id.parse().expect("numeric");
            assert!((100_000_000..=999_999_999).contains(&num));
            let formatted = format_device_id(&id);
            assert_eq!(formatted.len(), 11);
            assert_eq!(normalize_device_id(&formatted), id);
        }
        assert!(!is_valid_device_id("012345678"));
        assert!(!is_valid_device_id("12345678"));
        assert!(!is_valid_device_id("12345678a"));
    }

    #[test]
    fn escape_like_pattern_escapes_wildcards_and_backslash() {
        assert_eq!(escape_like_pattern("plain-host"), "plain-host");
        assert_eq!(escape_like_pattern("100%"), r"100\%");
        assert_eq!(escape_like_pattern("dev_01"), r"dev\_01");
        assert_eq!(escape_like_pattern(r"a\b%c_d"), r"a\\b\%c\_d");
        assert_eq!(escape_like_pattern("%%__\\\\"), r"\%\%\_\_\\\\");
    }

    #[tokio::test]
    async fn register_is_idempotent_and_rejects_disabled() {
        let store = MemoryDeviceStore::new();

        let first = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-alpha".into(),
                hostname: "mac-a".into(),
                os: "macos".into(),
                device_group: "dev".into(),
            })
            .await
            .expect("first register");
        assert!(is_valid_device_id(&first.device_id));
        assert_eq!(first.hostname, "mac-a");

        // Re-registering with the same device_key returns the same 9-digit device_id and refreshes metadata.
        let second = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-alpha".into(),
                hostname: "mac-a-renamed".into(),
                os: "macos".into(),
                device_group: "prod".into(),
            })
            .await
            .expect("second register");
        assert_eq!(second.device_id, first.device_id);
        assert_eq!(second.hostname, "mac-a-renamed");
        assert_eq!(second.device_group, "prod");
        assert_eq!(store.count_devices().await.unwrap(), 1);

        // Another device_key gets a distinct 9-digit device_id.
        let other = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-beta".into(),
                hostname: "linux-box".into(),
                os: "linux".into(),
                device_group: "".into(),
            })
            .await
            .expect("other register");
        assert_ne!(other.device_id, first.device_id);
        assert_eq!(store.count_devices().await.unwrap(), 2);

        // Verify key & formatted ID lookup.
        assert!(store
            .verify_key(&format_device_id(&first.device_id), "ed25519:key-alpha")
            .await
            .unwrap());
        assert!(!store
            .verify_key(&first.device_id, "ed25519:wrong")
            .await
            .unwrap());

        // Disable device and ensure re-registration fails with DeviceDisabled.
        assert!(store.set_disabled(&first.device_id, true).await.unwrap());
        let err = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-alpha".into(),
                hostname: "mac-a".into(),
                os: "macos".into(),
                device_group: "prod".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::DeviceDisabled(_)));

        // Search & relay log & delete.
        let filtered = store
            .list_devices(Some("linux"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].device_id, other.device_id);

        let now = Utc::now();
        store
            .record_relay_end(
                "sess-1",
                &first.device_id,
                &other.device_id,
                1024,
                2048,
                now,
                now,
            )
            .await
            .unwrap();
        assert_eq!(store.relay_logs().len(), 1);

        assert!(store.delete_device(&other.device_id).await.unwrap());
        assert_eq!(store.count_devices().await.unwrap(), 1);
        store.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn register_rejects_empty_device_key() {
        let store = MemoryDeviceStore::new();
        let err = store
            .register_or_get(RegisterInput {
                device_key: "   ".into(),
                hostname: "x".into(),
                os: "macos".into(),
                device_group: "".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn h8_and_m10_list_devices_excludes_device_key_and_dash_only_query() {
        let store = MemoryDeviceStore::new();
        let rec = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:super-secret-token-xyz".into(),
                hostname: "alpha-node".into(),
                os: "linux".into(),
                device_group: "fleet".into(),
            })
            .await
            .unwrap();
        let _rec2 = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:another-secret-999".into(),
                hostname: "beta_100%_node".into(),
                os: "macos".into(),
                device_group: "lab".into(),
            })
            .await
            .unwrap();

        // H8: device_key must NOT be searchable.
        assert!(store
            .list_devices(Some("super-secret-token"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .list_devices(Some("ed25519"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap()
            .is_empty());

        // Wildcard characters must match literally (only beta_100%_node contains `%`).
        let pct = store
            .list_devices(Some("%"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap();
        assert_eq!(pct.len(), 1);
        assert_eq!(pct[0].hostname, "beta_100%_node");

        // M10: `?q=-` normalizes to empty string and must NOT match devices without `-` in hostname/os/group.
        let dash = store
            .list_devices(Some("-"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap();
        assert_eq!(dash.len(), 1);
        assert_eq!(dash[0].device_id, rec.device_id);

        // Multi-dash query matches neither device.
        assert!(store
            .list_devices(Some("---"), DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn h11_register_enforces_field_lengths_and_rejects_control_chars() {
        let store = MemoryDeviceStore::new();

        // Exact max lengths are accepted.
        let ok = store
            .register_or_get(RegisterInput {
                device_key: "k".repeat(MAX_DEVICE_KEY_LEN),
                hostname: "h".repeat(MAX_HOSTNAME_LEN),
                os: "o".repeat(MAX_OS_LEN),
                device_group: "g".repeat(MAX_DEVICE_GROUP_LEN),
            })
            .await;
        assert!(ok.is_ok(), "boundary lengths should succeed: {ok:?}");

        // Oversized hostname / os / device_group rejected.
        for bad in [
            RegisterInput {
                device_key: "key-1".into(),
                hostname: "h".repeat(MAX_HOSTNAME_LEN + 1),
                os: "linux".into(),
                device_group: "".into(),
            },
            RegisterInput {
                device_key: "key-2".into(),
                hostname: "host".into(),
                os: "o".repeat(MAX_OS_LEN + 1),
                device_group: "".into(),
            },
            RegisterInput {
                device_key: "key-3".into(),
                hostname: "host".into(),
                os: "linux".into(),
                device_group: "g".repeat(MAX_DEVICE_GROUP_LEN + 1),
            },
            // NUL byte & control chars rejected across all fields.
            RegisterInput {
                device_key: "key\0nul".into(),
                hostname: "host".into(),
                os: "linux".into(),
                device_group: "".into(),
            },
            RegisterInput {
                device_key: "key-4".into(),
                hostname: "bad\0host".into(),
                os: "linux".into(),
                device_group: "".into(),
            },
            RegisterInput {
                device_key: "key-5".into(),
                hostname: "host".into(),
                os: "linux\x07bell".into(),
                device_group: "".into(),
            },
            RegisterInput {
                device_key: "key-6".into(),
                hostname: "host".into(),
                os: "linux".into(),
                device_group: "group\nline".into(),
            },
        ] {
            let err = store.register_or_get(bad).await.unwrap_err();
            assert!(matches!(err, StoreError::InvalidInput(_)));
        }
    }

    #[tokio::test]
    async fn m12_record_relay_end_validates_device_ids() {
        let store = MemoryDeviceStore::new();
        let now = Utc::now();

        let err1 = store
            .record_relay_end("s1", "invalid", "123456789", 10, 20, now, now)
            .await
            .unwrap_err();
        assert!(matches!(err1, StoreError::InvalidInput(_)));

        let err2 = store
            .record_relay_end("s2", "123456789", "012345678", 10, 20, now, now)
            .await
            .unwrap_err();
        assert!(matches!(err2, StoreError::InvalidInput(_)));

        store
            .record_relay_end("s3", "123 456 789", "987-654-321", 10, 20, now, now)
            .await
            .unwrap();
        assert_eq!(store.relay_logs().len(), 1);
        assert_eq!(store.relay_logs()[0].host_id, "123456789");
        assert_eq!(store.relay_logs()[0].client_id, "987654321");
    }

    #[tokio::test]
    async fn l7_list_devices_pagination() {
        let store = MemoryDeviceStore::new();
        for i in 0..5 {
            store
                .register_or_get(RegisterInput {
                    device_key: format!("key-page-{i}"),
                    hostname: format!("node-{i}"),
                    os: "linux".into(),
                    device_group: "cluster".into(),
                })
                .await
                .unwrap();
        }

        let all = store
            .list_devices(None, DEFAULT_LIST_LIMIT, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 5);

        let page1 = store.list_devices(None, 2, 0).await.unwrap();
        let page2 = store.list_devices(None, 2, 2).await.unwrap();
        let page3 = store.list_devices(None, 2, 4).await.unwrap();
        let page4 = store.list_devices(None, 2, 10).await.unwrap();

        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_eq!(page3.len(), 1);
        assert!(page4.is_empty());

        let combined: Vec<String> = page1
            .into_iter()
            .chain(page2)
            .chain(page3)
            .map(|r| r.device_id)
            .collect();
        let expected: Vec<String> = all.into_iter().map(|r| r.device_id).collect();
        assert_eq!(combined, expected);

        // limit=0 clamps to 1.
        let clamped_min = store.list_devices(None, 0, 0).await.unwrap();
        assert_eq!(clamped_min.len(), 1);
    }

    #[tokio::test]
    async fn user_crud_and_last_active_admin_protection() {
        let store = MemoryDeviceStore::new();

        let admin = store
            .create_user(CreateUserInput {
                email: "  Admin@Example.COM ".into(),
                display_name: "Primary Admin".into(),
                password_hash: "$argon2id$dummy".into(),
                role: "admin".into(),
                status: "active".into(),
                must_change_password: true,
            })
            .await
            .unwrap();
        assert_eq!(admin.email, "admin@example.com");
        assert_eq!(admin.token_version, 1);
        assert_eq!(store.count_active_admins().await.unwrap(), 1);

        // Duplicate normalized email rejected.
        let dup = store
            .create_user(CreateUserInput {
                email: "admin@example.com".into(),
                display_name: "Dup".into(),
                password_hash: "$argon2id$dummy".into(),
                role: "user".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(dup, StoreError::Conflict(_)));

        // Cannot delete or demote/disable the last active admin.
        assert!(matches!(
            store.delete_user(admin.id).await,
            Err(StoreError::Forbidden(_))
        ));
        assert!(matches!(
            store
                .update_user(
                    admin.id,
                    UpdateUserInput {
                        role: Some("user".into()),
                        ..Default::default()
                    }
                )
                .await,
            Err(StoreError::Forbidden(_))
        ));
        assert!(matches!(
            store
                .update_user(
                    admin.id,
                    UpdateUserInput {
                        status: Some("disabled".into()),
                        ..Default::default()
                    }
                )
                .await,
            Err(StoreError::Forbidden(_))
        ));

        // Add second admin and a regular user.
        let admin2 = store
            .create_user(CreateUserInput {
                email: "admin2@example.com".into(),
                display_name: "Backup Admin".into(),
                password_hash: "$argon2id$dummy2".into(),
                role: "admin".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap();
        let user = store
            .create_user(CreateUserInput {
                email: "alice@example.com".into(),
                display_name: "Alice".into(),
                password_hash: "$argon2id$dummy3".into(),
                role: "user".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap();

        // Bind a device to alice and verify device_count in list_users.
        let dev = store
            .register_or_get(RegisterInput {
                device_key: "key-alice-1".into(),
                hostname: "alice-mac".into(),
                os: "macos".into(),
                device_group: "".into(),
            })
            .await
            .unwrap();
        let bound = store
            .bind_device_to_user(&dev.device_id, user.id, Some("Work Mac"), Some("pk1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.owner_user_id, Some(user.id));
        assert_eq!(bound.alias, "Work Mac");
        assert_eq!(bound.device_pubkey, "pk1");

        let listed = store.list_users(Some("alice"), 10, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_count, 1);

        // New user claiming the device automatically unbinds the previous owner and clears old alias.
        let rebound = store
            .bind_device_to_user(&dev.device_id, admin2.id, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rebound.owner_user_id, Some(admin2.id));
        assert_eq!(rebound.alias, "");
        assert!(store.list_user_devices(user.id).await.unwrap().is_empty());
        assert_eq!(store.list_user_devices(admin2.id).await.unwrap().len(), 1);

        // Now deleting admin works because admin2 is still active.
        assert!(store.delete_user(admin.id).await.unwrap());
        assert_eq!(store.count_active_admins().await.unwrap(), 1);

        // Deleting admin2 fails (last active admin), while deleting regular user succeeds.
        assert!(matches!(
            store.delete_user(admin2.id).await,
            Err(StoreError::Forbidden(_))
        ));
        assert!(store.delete_user(user.id).await.unwrap());
    }

    #[tokio::test]
    async fn refresh_token_rotation_grace_window_and_replay_family_revocation() {
        let store = MemoryDeviceStore::new();
        let user = store
            .create_user(CreateUserInput {
                email: "bob@example.com".into(),
                display_name: "Bob".into(),
                password_hash: "hash".into(),
                role: "user".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap();

        let h1 = "1".repeat(64);
        let h2 = "2".repeat(64);
        let h3 = "3".repeat(64);
        let h4 = "4".repeat(64);
        let h_other = "a".repeat(64);
        let h_other2 = "b".repeat(64);

        let exp = Utc::now() + chrono::Duration::days(7);
        let s1 = store
            .create_user_session(CreateSessionInput {
                user_id: user.id,
                device_id: None,
                family_id: None,
                token_hash: h1.clone(),
                expires_at: exp,
                client_info: "macOS".into(),
            })
            .await
            .unwrap();
        // Second session sharing the same family_id to verify whole-family revocation on replay.
        let s2 = store
            .create_user_session(CreateSessionInput {
                user_id: user.id,
                device_id: None,
                family_id: Some(s1.family_id),
                token_hash: h_other.clone(),
                expires_at: exp,
                client_info: "sibling".into(),
            })
            .await
            .unwrap();

        // 1. Normal rotation h1 -> h2 with 5s grace period.
        let rotated = store
            .rotate_refresh_token(&h1, &h2, exp, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(rotated.token_hash, h2);
        assert_eq!(rotated.prev_token_hash.as_deref(), Some(h1.as_str()));

        // 2. Retrying h1 within grace window succeeds ONCE and rotates to h3 without failing.
        let retried = store
            .rotate_refresh_token(&h1, &h3, exp, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(retried.token_hash, h3);

        // 2b. Reusing h1 a second time inside the grace window must trigger TokenReplayDetected
        // without corrupting a separate session; test multi-generation replay on a dedicated session.
        let h_g1 = "a1".repeat(32);
        let h_g2 = "a2".repeat(32);
        let h_g3 = "a3".repeat(32);
        let h_g4 = "a4".repeat(32);
        let s_gen = store
            .create_user_session(CreateSessionInput {
                user_id: user.id,
                device_id: None,
                family_id: None,
                token_hash: h_g1.clone(),
                expires_at: exp,
                client_info: "multi-gen".into(),
            })
            .await
            .unwrap();
        store
            .rotate_refresh_token(&h_g1, &h_g2, exp, Duration::from_secs(15))
            .await
            .unwrap();
        store
            .rotate_refresh_token(&h_g2, &h_g3, exp, Duration::from_secs(15))
            .await
            .unwrap();
        // Multi-gen replay of h_g1 must revoke s_gen's family.
        let gen_replay = store
            .rotate_refresh_token(&h_g1, &h_g4, exp, Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(matches!(
            gen_replay,
            StoreError::TokenReplayDetected { family_id, user_id }
                if family_id == s_gen.family_id && user_id == user.id
        ));

        // 2c. Duplicate grace reuse within grace window must also revoke family.
        let h_d1 = "b1".repeat(32);
        let h_d2 = "b2".repeat(32);
        let h_d3 = "b3".repeat(32);
        let h_d4 = "b4".repeat(32);
        let s_dup = store
            .create_user_session(CreateSessionInput {
                user_id: user.id,
                device_id: None,
                family_id: None,
                token_hash: h_d1.clone(),
                expires_at: exp,
                client_info: "dup-grace".into(),
            })
            .await
            .unwrap();
        store
            .rotate_refresh_token(&h_d1, &h_d2, exp, Duration::from_secs(15))
            .await
            .unwrap();
        store
            .rotate_refresh_token(&h_d1, &h_d3, exp, Duration::from_secs(15))
            .await
            .unwrap();
        let dup_replay = store
            .rotate_refresh_token(&h_d1, &h_d4, exp, Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(matches!(
            dup_replay,
            StoreError::TokenReplayDetected { family_id, user_id }
                if family_id == s_dup.family_id && user_id == user.id
        ));

        // 3. Rotate h3 -> h4 with 10ms grace period, wait past grace, then replay h3 -> revokes whole family!
        store
            .rotate_refresh_token(&h3, &h4, exp, Duration::from_millis(10))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let replay_err = store
            .rotate_refresh_token(&h3, &h_other2, exp, Duration::from_secs(15))
            .await
            .unwrap_err();
        assert!(matches!(
            replay_err,
            StoreError::TokenReplayDetected { family_id, user_id }
                if family_id == s1.family_id && user_id == user.id
        ));

        // Both s1 and sibling s2 in the same family must now be revoked.
        assert!(store
            .get_user_session(s1.id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some());
        assert!(store
            .get_user_session(s2.id)
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some());
        assert!(matches!(
            store
                .rotate_refresh_token(&h_other, &h_other2, exp, Duration::from_secs(15))
                .await,
            Err(StoreError::Unauthorized(_))
        ));
    }

    #[tokio::test]
    async fn device_self_control_settings_pki_and_audit_store() {
        let store = MemoryDeviceStore::new();
        let user = store
            .create_user(CreateUserInput {
                email: "carol@example.com".into(),
                display_name: "Carol".into(),
                password_hash: "hash".into(),
                role: "user".into(),
                status: "active".into(),
                must_change_password: false,
            })
            .await
            .unwrap();

        // Default allow_registration = true.
        assert!(store.get_allow_registration().await.unwrap());
        store
            .set_allow_registration(false, Some(user.id))
            .await
            .unwrap();
        assert!(!store.get_allow_registration().await.unwrap());

        let dev = store
            .register_or_get(RegisterInput {
                device_key: "key-carol".into(),
                hostname: "carol-pc".into(),
                os: "linux".into(),
                device_group: "".into(),
            })
            .await
            .unwrap();
        store
            .bind_device_to_user(&dev.device_id, user.id, Some("Home"), None)
            .await
            .unwrap();
        assert_eq!(
            store.get_device_owner(&dev.device_id).await.unwrap(),
            Some(user.id)
        );
        assert!(store
            .set_device_allow_remote_assist(&dev.device_id, Some(user.id), true)
            .await
            .unwrap());
        assert!(
            store
                .get_by_id(&dev.device_id)
                .await
                .unwrap()
                .unwrap()
                .allow_remote_assist
        );
        assert!(store
            .update_device_alias(&dev.device_id, user.id, "Studio")
            .await
            .unwrap());
        assert!(store
            .set_device_pubkey(&dev.device_id, "ed25519:abcd")
            .await
            .unwrap());
        let fetched = store.get_by_id(&dev.device_id).await.unwrap().unwrap();
        assert_eq!(fetched.alias, "Studio");
        assert_eq!(fetched.device_pubkey, "ed25519:abcd");
        let email_hits = store
            .list_devices(Some("carol@example.com"), 20, 0)
            .await
            .unwrap();
        assert_eq!(email_hits.len(), 1);
        assert_eq!(email_hits[0].device_id, dev.device_id);
        assert_eq!(
            store
                .count_devices_filtered(Some("carol@example.com"))
                .await
                .unwrap(),
            1
        );

        assert!(store.unbind_device(&dev.device_id, user.id).await.unwrap());
        assert_eq!(store.get_device_owner(&dev.device_id).await.unwrap(), None);
        assert!(
            !store
                .get_by_id(&dev.device_id)
                .await
                .unwrap()
                .unwrap()
                .allow_remote_assist
        );

        // Device cert insert & revocation.
        let now = Utc::now();
        store
            .insert_device_cert(DeviceCertRecord {
                serial_hex: "01ab".into(),
                user_id: user.id,
                device_id: dev.device_id.clone(),
                fingerprint_sha256: "f".repeat(64),
                cert_pem: "PEM".into(),
                issued_at: now,
                expires_at: now + chrono::Duration::days(7),
                revoked_at: None,
                revoke_reason: None,
            })
            .await
            .unwrap();
        let revoked = store
            .revoke_device_certs(&dev.device_id, Some("unbound"))
            .await
            .unwrap();
        assert_eq!(revoked.len(), 1);
        assert_eq!(store.list_active_revoked_certs().await.unwrap().len(), 1);

        // Audit log.
        store
            .append_audit_log(
                Some(user.id),
                Some(&dev.device_id),
                "device.unbind",
                serde_json::json!({ "reason": "test" }),
            )
            .await
            .unwrap();
        let logs = store.list_audit_logs(10, 0).await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].action, "device.unbind");
    }
}
