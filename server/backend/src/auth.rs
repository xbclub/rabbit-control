use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::store::{
    is_valid_device_id, normalize_device_id, normalize_email, CreateUserInput, PkiStore,
    ServerKeyRecord, StoreError, UserRecord, UserStore, ROLE_ADMIN, STATUS_ACTIVE,
};

pub const AUTH_ED25519_KEY_NAME: &str = "auth_ed25519";
pub const AUTH_ED25519_ALGORITHM: &str = "Ed25519";

pub const DEFAULT_INITIAL_ADMIN_EMAIL: &str = "admin@rabbitcontrol.local";
pub const INITIAL_ADMIN_PASSWORD_LEN: usize = 16;

pub const ACCESS_TOKEN_TTL: Duration = Duration::from_secs(15 * 60);
pub const REFRESH_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
pub const REFRESH_GRACE_PERIOD: Duration = Duration::from_secs(15);
pub const OWNER_GRANT_DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Domain separation prefix for canonical `OwnerGrantTicket` Ed25519 signatures.
///
/// Canonical signing payload bytes:
/// `b"RC-OWNER-GRANT-V1\0"` followed by UTF-8 fields separated by `b'\0'`:
/// `ticket_id \0 user_id (hyphenated lowercase) \0 caller_device_id \0 caller_pubkey_hex (lowercase) \0 target_device_id \0 session_id \0 expires_at_unix_sec (decimal)`
pub const OWNER_GRANT_DOMAIN: &[u8] = b"RC-OWNER-GRANT-V1\0";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    #[error("key generation or loading failed: {0}")]
    KeyError(String),
    #[error("invalid token: {0}")]
    InvalidToken(String),
    #[error("token expired")]
    TokenExpired,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialAdminCredentials {
    pub user_id: Uuid,
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct JwtHeader {
    alg: String,
    typ: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessTokenClaims {
    pub sub: Uuid,
    pub email: String,
    pub role: String,
    pub ver: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub did: Option<String>,
    pub sid: Uuid,
    pub pwd_change_only: bool,
    pub iat: i64,
    pub exp: i64,
}

/// One-time signed `owner_grant` ticket authorizing passwordless owner takeover/connection
/// from `caller_device_id` (`caller_pubkey_hex`) to `target_device_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OwnerGrantTicket {
    /// 16 random bytes formatted as 32 lowercase hex chars.
    pub ticket_id: String,
    pub user_id: Uuid,
    pub caller_device_id: String,
    /// Caller's 32-byte Ed25519 public key encoded as 64 lowercase hex chars.
    pub caller_pubkey_hex: String,
    pub target_device_id: String,
    pub session_id: String,
    pub expires_at_unix_sec: i64,
    /// 64-byte server Ed25519 signature over `owner_grant_signing_message`, 128 lowercase hex chars.
    pub signature_hex: String,
}

fn current_unix_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Hash a plaintext password with Argon2id using a fresh random per-password salt.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    if password.is_empty() || password.len() > 256 {
        return Err(AuthError::InvalidInput(
            "password must be 1..=256 bytes".into(),
        ));
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::PasswordHash(e.to_string()))?;
    Ok(hash.to_string())
}

/// Verify a plaintext password against a stored PHC Argon2id hash string.
pub fn verify_password(password: &str, stored_hash: &str) -> Result<bool, AuthError> {
    if password.is_empty() || password.len() > 256 {
        return Ok(false);
    }
    let parsed =
        PasswordHash::new(stored_hash).map_err(|e| AuthError::PasswordHash(e.to_string()))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(err) => Err(AuthError::PasswordHash(err.to_string())),
    }
}

/// Generate a cryptographically random strong password of `len` characters (minimum 12).
pub fn generate_random_password(len: usize) -> String {
    const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
    const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
    const DIGITS: &[u8] = b"23456789";
    const SYMBOLS: &[u8] = b"!@#$%^&*-_=+";
    const ALL: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#$%^&*-_=+";

    let target_len = len.max(12);
    let rng = SystemRandom::new();
    let mut raw = vec![0u8; target_len * 2];
    rng.fill(&mut raw).expect("SystemRandom failed");

    let mut chars = Vec::with_capacity(target_len);
    chars.push(UPPER[usize::from(raw[0]) % UPPER.len()]);
    chars.push(LOWER[usize::from(raw[1]) % LOWER.len()]);
    chars.push(DIGITS[usize::from(raw[2]) % DIGITS.len()]);
    chars.push(SYMBOLS[usize::from(raw[3]) % SYMBOLS.len()]);
    for i in 4..target_len {
        chars.push(ALL[usize::from(raw[i]) % ALL.len()]);
    }

    // Fisher-Yates shuffle using remaining random bytes.
    for i in (1..chars.len()).rev() {
        let j = usize::from(raw[target_len + (i % target_len)]) % (i + 1);
        chars.swap(i, j);
    }

    String::from_utf8(chars).expect("ASCII charset is valid UTF-8")
}

/// Ensure at least one active administrator account exists on startup.
///
/// If no active `role='admin'` user exists, creates one with `must_change_password = true`
/// and a cryptographically random 16-character password, returning the credentials once so
/// `main.rs` can log them on first boot.
pub async fn ensure_initial_admin(
    store: &dyn UserStore,
    admin_email: Option<&str>,
) -> Result<Option<InitialAdminCredentials>, AuthError> {
    if store.count_active_admins().await? > 0 {
        return Ok(None);
    }

    let email = admin_email
        .map(normalize_email)
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_INITIAL_ADMIN_EMAIL.to_string());
    let password = generate_random_password(INITIAL_ADMIN_PASSWORD_LEN);
    let password_hash = hash_password(&password)?;

    match store
        .create_user(CreateUserInput {
            email: email.clone(),
            display_name: "Administrator".to_string(),
            password_hash,
            role: ROLE_ADMIN.to_string(),
            status: STATUS_ACTIVE.to_string(),
            must_change_password: true,
        })
        .await
    {
        Ok(user) => Ok(Some(InitialAdminCredentials {
            user_id: user.id,
            email: user.email,
            password,
        })),
        Err(StoreError::Conflict(_)) => {
            if store.count_active_admins().await? > 0 {
                Ok(None)
            } else {
                Err(AuthError::Store(StoreError::Conflict(format!(
                    "email '{email}' already exists as a non-admin or disabled user"
                ))))
            }
        }
        Err(err) => Err(AuthError::Store(err)),
    }
}

/// Generate a cryptographically random opaque refresh token (32 bytes base64url no-pad).
pub fn generate_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("SystemRandom failed");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compute lowercase 64-character SHA-256 hex digest for opaque token storage/lookup.
pub fn hash_token_sha256_hex(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    hex::encode(digest)
}

/// Build the deterministic canonical signing payload for an `OwnerGrantTicket`.
///
/// Format:
/// `b"RC-OWNER-GRANT-V1\0"`
/// `+ ticket_id (32 hex) + "\0"`
/// `+ user_id (hyphenated lowercase UUID) + "\0"`
/// `+ caller_device_id (9 digits) + "\0"`
/// `+ caller_pubkey_hex (64 lowercase hex) + "\0"`
/// `+ target_device_id (9 digits) + "\0"`
/// `+ session_id + "\0"`
/// `+ expires_at_unix_sec (decimal)`
pub fn owner_grant_signing_message(ticket: &OwnerGrantTicket) -> Result<Vec<u8>, AuthError> {
    let ticket_id = ticket.ticket_id.trim().to_ascii_lowercase();
    if ticket_id.len() != 32 || !ticket_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AuthError::InvalidInput(
            "ticket_id must be 32 hex characters (16 bytes)".into(),
        ));
    }

    let caller_did = normalize_device_id(&ticket.caller_device_id);
    if !is_valid_device_id(&caller_did) {
        return Err(AuthError::InvalidInput(format!(
            "invalid caller_device_id: {}",
            ticket.caller_device_id
        )));
    }

    let target_did = normalize_device_id(&ticket.target_device_id);
    if !is_valid_device_id(&target_did) {
        return Err(AuthError::InvalidInput(format!(
            "invalid target_device_id: {}",
            ticket.target_device_id
        )));
    }

    let caller_pubkey_hex = ticket.caller_pubkey_hex.trim().to_ascii_lowercase();
    if caller_pubkey_hex.len() != 64 || !caller_pubkey_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AuthError::InvalidInput(
            "caller_pubkey_hex must be 64 hex characters (32-byte Ed25519 key)".into(),
        ));
    }

    let session_id = ticket.session_id.trim();
    if session_id.is_empty()
        || session_id.len() > 128
        || session_id.contains('\0')
        || session_id.chars().any(char::is_control)
    {
        return Err(AuthError::InvalidInput(
            "session_id must be 1..=128 non-control characters".into(),
        ));
    }

    let uid_str = ticket.user_id.hyphenated().to_string();
    let exp_str = ticket.expires_at_unix_sec.to_string();

    let mut msg = Vec::with_capacity(OWNER_GRANT_DOMAIN.len() + 256);
    msg.extend_from_slice(OWNER_GRANT_DOMAIN);
    msg.extend_from_slice(ticket_id.as_bytes());
    msg.push(0);
    msg.extend_from_slice(uid_str.as_bytes());
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

/// Verify an `OwnerGrantTicket` against a raw 32-byte server Ed25519 public key and timestamp.
pub fn verify_owner_grant_with_pubkey(
    ticket: &OwnerGrantTicket,
    server_ed25519_pubkey: &[u8],
    now_unix_sec: i64,
) -> Result<(), AuthError> {
    if server_ed25519_pubkey.len() != 32 {
        return Err(AuthError::KeyError(
            "server Ed25519 public key must be 32 bytes".into(),
        ));
    }
    if now_unix_sec >= ticket.expires_at_unix_sec {
        return Err(AuthError::TokenExpired);
    }

    let sig_bytes = hex::decode(ticket.signature_hex.trim())
        .map_err(|_| AuthError::InvalidInput("signature_hex must be valid hex".into()))?;
    if sig_bytes.len() != 64 {
        return Err(AuthError::InvalidSignature);
    }

    let msg = owner_grant_signing_message(ticket)?;
    UnparsedPublicKey::new(&signature::ED25519, server_ed25519_pubkey)
        .verify(&msg, &sig_bytes)
        .map_err(|_| AuthError::InvalidSignature)?;
    Ok(())
}

/// Verify an EdDSA JWT against a raw 32-byte Ed25519 public key and timestamp.
pub fn verify_access_token_with_pubkey(
    token: &str,
    server_ed25519_pubkey: &[u8],
    now_unix_sec: i64,
) -> Result<AccessTokenClaims, AuthError> {
    if server_ed25519_pubkey.len() != 32 {
        return Err(AuthError::KeyError(
            "server Ed25519 public key must be 32 bytes".into(),
        ));
    }

    let mut parts = token.split('.');
    let Some(header_b64) = parts.next() else {
        return Err(AuthError::InvalidToken("missing JWT header".into()));
    };
    let Some(payload_b64) = parts.next() else {
        return Err(AuthError::InvalidToken("missing JWT payload".into()));
    };
    let Some(sig_b64) = parts.next() else {
        return Err(AuthError::InvalidToken("missing JWT signature".into()));
    };
    if parts.next().is_some() {
        return Err(AuthError::InvalidToken("too many JWT segments".into()));
    }

    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| AuthError::InvalidToken("invalid base64url header".into()))?;
    let header: JwtHeader = serde_json::from_slice(&header_bytes)
        .map_err(|_| AuthError::InvalidToken("malformed JWT header JSON".into()))?;
    if header.alg != "EdDSA" || (!header.typ.is_empty() && header.typ != "JWT") {
        return Err(AuthError::InvalidToken("unsupported JWT alg or typ".into()));
    }

    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| AuthError::InvalidToken("invalid base64url signature".into()))?;
    let signing_input_len = header_b64.len() + 1 + payload_b64.len();
    let signing_input = &token.as_bytes()[..signing_input_len];
    UnparsedPublicKey::new(&signature::ED25519, server_ed25519_pubkey)
        .verify(signing_input, &sig_bytes)
        .map_err(|_| AuthError::InvalidSignature)?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| AuthError::InvalidToken("invalid base64url payload".into()))?;
    let claims: AccessTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|_| AuthError::InvalidToken("malformed JWT claims JSON".into()))?;

    if now_unix_sec >= claims.exp {
        return Err(AuthError::TokenExpired);
    }

    Ok(claims)
}

/// Server Ed25519 signing authority persisted in `server_keys('auth_ed25519')`.
pub struct AuthAuthority {
    key_pair: Ed25519KeyPair,
    public_key: [u8; 32],
}

impl AuthAuthority {
    /// Load `server_keys('auth_ed25519')` or atomically generate and persist a new Ed25519 keypair.
    pub async fn load_or_create(store: &dyn PkiStore) -> Result<Self, AuthError> {
        if let Some(existing) = store.get_server_key(AUTH_ED25519_KEY_NAME).await? {
            return Self::from_pkcs8(&existing.private_key_der);
        }

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|e| AuthError::KeyError(format!("Ed25519 keygen failed: {e}")))?;
        let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
            .map_err(|e| AuthError::KeyError(format!("Ed25519 load failed: {e}")))?;
        let pub_bytes = kp.public_key().as_ref().to_vec();

        let stored = store
            .put_server_key_if_absent(ServerKeyRecord {
                key_name: AUTH_ED25519_KEY_NAME.to_string(),
                algorithm: AUTH_ED25519_ALGORITHM.to_string(),
                private_key_der: pkcs8.as_ref().to_vec(),
                public_key_der: pub_bytes,
                created_at: Utc::now(),
            })
            .await?;

        Self::from_pkcs8(&stored.private_key_der)
    }

    /// Generate a fresh in-memory `AuthAuthority` (useful for tests).
    pub fn generate_ephemeral() -> Result<Self, AuthError> {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|e| AuthError::KeyError(format!("Ed25519 keygen failed: {e}")))?;
        Self::from_pkcs8(pkcs8.as_ref())
    }

    pub fn from_pkcs8(pkcs8_der: &[u8]) -> Result<Self, AuthError> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8_der)
            .map_err(|e| AuthError::KeyError(format!("invalid Ed25519 PKCS#8 DER: {e}")))?;
        let pub_slice = key_pair.public_key().as_ref();
        let public_key: [u8; 32] = pub_slice
            .try_into()
            .map_err(|_| AuthError::KeyError("unexpected Ed25519 public key length".into()))?;
        Ok(Self {
            key_pair,
            public_key,
        })
    }

    pub fn public_key_bytes(&self) -> &[u8; 32] {
        &self.public_key
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key)
    }

    pub fn issue_access_token(&self, claims: &AccessTokenClaims) -> Result<String, AuthError> {
        let header = JwtHeader {
            alg: "EdDSA".to_string(),
            typ: "JWT".to_string(),
        };
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
        let signing_input = format!("{header_b64}.{payload_b64}");
        let sig = self.key_pair.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());
        Ok(format!("{signing_input}.{sig_b64}"))
    }

    pub fn issue_access_token_for_user(
        &self,
        user: &UserRecord,
        session_id: Uuid,
        device_id: Option<&str>,
        ttl: Duration,
    ) -> Result<String, AuthError> {
        let iat = current_unix_sec();
        let ttl_secs = i64::try_from(ttl.as_secs().max(1)).unwrap_or(i64::MAX);
        let claims = AccessTokenClaims {
            sub: user.id,
            email: user.email.clone(),
            role: user.role.clone(),
            ver: user.token_version,
            did: device_id.map(normalize_device_id),
            sid: session_id,
            pwd_change_only: user.must_change_password,
            iat,
            exp: iat.saturating_add(ttl_secs),
        };
        self.issue_access_token(&claims)
    }

    pub fn verify_access_token(&self, token: &str) -> Result<AccessTokenClaims, AuthError> {
        self.verify_access_token_at(token, current_unix_sec())
    }

    pub fn verify_access_token_at(
        &self,
        token: &str,
        now_unix_sec: i64,
    ) -> Result<AccessTokenClaims, AuthError> {
        verify_access_token_with_pubkey(token, &self.public_key, now_unix_sec)
    }

    pub fn issue_owner_grant(
        &self,
        user_id: Uuid,
        caller_device_id: &str,
        caller_pubkey_hex: &str,
        target_device_id: &str,
        session_id: &str,
        ttl: Duration,
    ) -> Result<OwnerGrantTicket, AuthError> {
        let now = current_unix_sec();
        let ttl_secs = i64::try_from(ttl.as_secs().max(1)).unwrap_or(i64::MAX);
        self.issue_owner_grant_with_expiry(
            user_id,
            caller_device_id,
            caller_pubkey_hex,
            target_device_id,
            session_id,
            now.saturating_add(ttl_secs),
        )
    }

    pub fn issue_owner_grant_with_expiry(
        &self,
        user_id: Uuid,
        caller_device_id: &str,
        caller_pubkey_hex: &str,
        target_device_id: &str,
        session_id: &str,
        expires_at_unix_sec: i64,
    ) -> Result<OwnerGrantTicket, AuthError> {
        let mut ticket_bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut ticket_bytes)
            .map_err(|_| AuthError::KeyError("SystemRandom failed".into()))?;

        let mut ticket = OwnerGrantTicket {
            ticket_id: hex::encode(ticket_bytes),
            user_id,
            caller_device_id: normalize_device_id(caller_device_id),
            caller_pubkey_hex: caller_pubkey_hex.trim().to_ascii_lowercase(),
            target_device_id: normalize_device_id(target_device_id),
            session_id: session_id.trim().to_string(),
            expires_at_unix_sec,
            signature_hex: String::new(),
        };

        let msg = owner_grant_signing_message(&ticket)?;
        let sig = self.key_pair.sign(&msg);
        ticket.signature_hex = hex::encode(sig.as_ref());
        Ok(ticket)
    }

    pub fn verify_owner_grant(
        &self,
        ticket: &OwnerGrantTicket,
        now_unix_sec: i64,
    ) -> Result<(), AuthError> {
        verify_owner_grant_with_pubkey(ticket, &self.public_key, now_unix_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryDeviceStore;

    #[test]
    fn argon2id_password_hashing_and_salts() {
        let h1 = hash_password("CorrectHorseBatteryStaple!99").unwrap();
        let h2 = hash_password("CorrectHorseBatteryStaple!99").unwrap();
        assert!(h1.starts_with("$argon2id$"));
        assert_ne!(h1, h2, "each user hash must use a fresh random salt");

        assert!(verify_password("CorrectHorseBatteryStaple!99", &h1).unwrap());
        assert!(!verify_password("WrongPassword", &h1).unwrap());
    }

    #[tokio::test]
    async fn ensure_initial_admin_creates_once_and_is_idempotent() {
        let store = MemoryDeviceStore::new();

        let creds = ensure_initial_admin(&store, None)
            .await
            .unwrap()
            .expect("first boot must create initial admin");
        assert_eq!(creds.email, DEFAULT_INITIAL_ADMIN_EMAIL);
        assert_eq!(creds.password.len(), INITIAL_ADMIN_PASSWORD_LEN);

        let user = store.get_user_by_id(creds.user_id).await.unwrap().unwrap();
        assert_eq!(user.role, ROLE_ADMIN);
        assert_eq!(user.status, STATUS_ACTIVE);
        assert!(user.must_change_password);
        assert!(verify_password(&creds.password, &user.password_hash).unwrap());

        // Second call must be a no-op returning None.
        assert!(ensure_initial_admin(&store, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ed25519_authority_persists_in_store_and_signs_verifies_jwt() {
        let store = MemoryDeviceStore::new();
        let auth1 = AuthAuthority::load_or_create(&store).await.unwrap();
        let auth2 = AuthAuthority::load_or_create(&store).await.unwrap();
        assert_eq!(auth1.public_key_bytes(), auth2.public_key_bytes());

        let uid = Uuid::new_v4();
        let sid = Uuid::new_v4();
        let claims = AccessTokenClaims {
            sub: uid,
            email: "alice@example.com".into(),
            role: "user".into(),
            ver: 3,
            did: Some("123456789".into()),
            sid,
            pwd_change_only: false,
            iat: 1_700_000_000,
            exp: 1_700_000_900,
        };
        let jwt = auth1.issue_access_token(&claims).unwrap();

        let decoded = auth2.verify_access_token_at(&jwt, 1_700_000_100).unwrap();
        assert_eq!(decoded, claims);

        // Expired token rejected.
        assert!(matches!(
            auth2.verify_access_token_at(&jwt, 1_700_000_900),
            Err(AuthError::TokenExpired)
        ));

        // Tampered payload rejected.
        let mut parts: Vec<&str> = jwt.split('.').collect();
        let tampered_payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&AccessTokenClaims {
                role: "admin".into(),
                ..claims
            })
            .unwrap(),
        );
        parts[1] = &tampered_payload;
        let tampered = parts.join(".");
        assert!(matches!(
            auth2.verify_access_token_at(&tampered, 1_700_000_100),
            Err(AuthError::InvalidSignature)
        ));
    }

    #[test]
    fn owner_grant_ticket_signing_verification_and_tamper_detection() {
        let auth = AuthAuthority::generate_ephemeral().unwrap();
        let uid = Uuid::new_v4();
        let caller_pk = "ab".repeat(32);

        let ticket = auth
            .issue_owner_grant_with_expiry(
                uid,
                "111 222 333",
                &caller_pk,
                "444-555-666",
                "sess-xyz",
                2_000_000_060,
            )
            .unwrap();
        assert_eq!(ticket.ticket_id.len(), 32);
        assert_eq!(ticket.caller_device_id, "111222333");
        assert_eq!(ticket.target_device_id, "444555666");
        assert_eq!(ticket.signature_hex.len(), 128);

        // Canonical message starts with exact domain prefix.
        let msg = owner_grant_signing_message(&ticket).unwrap();
        assert!(msg.starts_with(OWNER_GRANT_DOMAIN));

        // Valid verification via authority and via standalone public key.
        auth.verify_owner_grant(&ticket, 2_000_000_000).unwrap();
        verify_owner_grant_with_pubkey(&ticket, auth.public_key_bytes(), 2_000_000_000).unwrap();

        // Expired ticket rejected.
        assert!(matches!(
            auth.verify_owner_grant(&ticket, 2_000_000_060),
            Err(AuthError::TokenExpired)
        ));

        // Tampered target_device_id rejected.
        let mut tampered = ticket.clone();
        tampered.target_device_id = "999888777".into();
        assert!(matches!(
            auth.verify_owner_grant(&tampered, 2_000_000_000),
            Err(AuthError::InvalidSignature)
        ));
    }
}
