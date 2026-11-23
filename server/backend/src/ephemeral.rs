use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dashmap::DashMap;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;
use uuid::Uuid;

use crate::store::{SettingsStore, StoreError};

const REDIS_ONLINE_PREFIX: &str = "rc:online:";
const REDIS_ONLINE_ZSET: &str = "rc:online_zset";
const REDIS_UDP_PREFIX: &str = "rc:udp:";
const REDIS_TOKEN_PREFIX: &str = "rc:token:";
const REDIS_ALLOW_REGISTRATION_KEY: &str = "rc:setting:allow_registration";
const REDIS_REVOKED_CERT_PREFIX: &str = "rc:revoked_cert:";

pub const DEFAULT_MAX_RELAY_TOKENS: usize = 16_384;
pub const DEFAULT_SETTINGS_CACHE_TTL: Duration = Duration::from_secs(60);

const REDIS_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REDIS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const REDIS_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const REDIS_NUMBER_OF_RETRIES: usize = 1;

#[derive(Debug, Error)]
pub enum EphemeralError {
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid address stored in ephemeral backend: {0}")]
    InvalidAddress(String),
    #[error("invalid boolean stored in ephemeral backend: {0}")]
    InvalidBool(String),
    #[error("ephemeral backend timeout: {0}")]
    Timeout(&'static str),
    #[error("invalid TTL: {0}")]
    InvalidTtl(&'static str),
    #[error("relay token capacity exceeded (max {0})")]
    RelayTokenCapacityExceeded(usize),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayTokenRecord {
    pub session_id: String,
    pub host_id: String,
    pub client_id: String,
}

#[async_trait]
pub trait EphemeralStore: Send + Sync {
    async fn set_online(&self, device_id: &str, ttl: Duration) -> Result<(), EphemeralError>;
    async fn remove_online(&self, device_id: &str) -> Result<(), EphemeralError>;
    async fn is_online(&self, device_id: &str) -> Result<bool, EphemeralError>;
    async fn online_ids(&self) -> Result<Vec<String>, EphemeralError>;

    async fn set_public_udp(
        &self,
        device_id: &str,
        addr: SocketAddr,
        ttl: Duration,
    ) -> Result<(), EphemeralError>;
    async fn get_public_udp(&self, device_id: &str) -> Result<Option<SocketAddr>, EphemeralError>;

    async fn put_relay_token(
        &self,
        token: &str,
        record: RelayTokenRecord,
        ttl: Duration,
    ) -> Result<(), EphemeralError>;
    async fn take_relay_token(
        &self,
        token: &str,
    ) -> Result<Option<RelayTokenRecord>, EphemeralError>;

    async fn get_allow_registration(&self) -> Result<Option<bool>, EphemeralError> {
        Ok(None)
    }
    async fn set_allow_registration(
        &self,
        _allowed: bool,
        _ttl: Duration,
    ) -> Result<(), EphemeralError> {
        Ok(())
    }
    /// Backfill `allow_registration` ONLY if currently absent/expired in the cache (`SET NX`),
    /// preventing stale read backfills from overwriting newer concurrent writes.
    async fn set_allow_registration_if_absent(
        &self,
        _allowed: bool,
        _ttl: Duration,
    ) -> Result<(), EphemeralError> {
        Ok(())
    }
    async fn invalidate_allow_registration(&self) -> Result<(), EphemeralError> {
        Ok(())
    }

    async fn revoke_cert_serial(
        &self,
        _serial_hex: &str,
        _ttl: Duration,
    ) -> Result<(), EphemeralError> {
        Ok(())
    }
    async fn is_cert_revoked(&self, _serial_hex: &str) -> Result<bool, EphemeralError> {
        Ok(false)
    }

    /// Optional count of active (non-expired) relay tokens when cheaply available.
    async fn active_relay_token_count(&self) -> Result<Option<usize>, EphemeralError> {
        Ok(None)
    }

    fn backend_name(&self) -> &'static str;
    async fn health_check(&self) -> Result<(), EphemeralError>;
}

/// Default zero-dependency in-memory `EphemeralStore` backed by `DashMap` with TTL expiry.
#[derive(Debug)]
pub struct MemoryEphemeralStore {
    online: DashMap<String, Instant>,
    public_udp: DashMap<String, (SocketAddr, Instant)>,
    relay_tokens: DashMap<String, (RelayTokenRecord, Instant)>,
    allow_registration: Mutex<Option<(bool, Instant)>>,
    revoked_certs: DashMap<String, Instant>,
    max_relay_tokens: usize,
    token_insert_lock: Mutex<()>,
}

impl Default for MemoryEphemeralStore {
    fn default() -> Self {
        Self::with_max_relay_tokens(DEFAULT_MAX_RELAY_TOKENS)
    }
}

impl MemoryEphemeralStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_relay_tokens(max_relay_tokens: usize) -> Self {
        Self {
            online: DashMap::new(),
            public_udp: DashMap::new(),
            relay_tokens: DashMap::new(),
            allow_registration: Mutex::new(None),
            revoked_certs: DashMap::new(),
            max_relay_tokens,
            token_insert_lock: Mutex::new(()),
        }
    }

    pub fn max_relay_tokens(&self) -> usize {
        self.max_relay_tokens
    }

    fn compute_expiry(ttl: Duration) -> Result<Option<Instant>, EphemeralError> {
        if ttl.is_zero() {
            return Ok(None);
        }
        Instant::now()
            .checked_add(ttl)
            .map(Some)
            .ok_or(EphemeralError::InvalidTtl("TTL overflows monotonic clock"))
    }

    /// Purge all expired entries across online devices, UDP endpoints, relay tokens,
    /// settings cache, and certificate revocations.
    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        self.online.retain(|_, exp| *exp > now);
        self.public_udp.retain(|_, (_, exp)| *exp > now);
        self.relay_tokens.retain(|_, (_, exp)| *exp > now);
        self.revoked_certs.retain(|_, exp| *exp > now);
        let mut reg = self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some((_, exp)) = *reg {
            if exp <= now {
                *reg = None;
            }
        }
    }

    /// Spawn a background task that periodically removes expired entries.
    ///
    /// Holds only a `Weak` reference so dropping the last `Arc<MemoryEphemeralStore>`
    /// causes the task to terminate automatically without leaking.
    pub fn spawn_cleanup_task(self: &Arc<Self>, interval: Duration) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let interval = interval.max(Duration::from_millis(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(store) = weak.upgrade() else {
                    break;
                };
                store.cleanup_expired();
            }
        })
    }

    /// Spawn a background cleanup task that exits on either shutdown signal or when the
    /// store's last strong `Arc` is dropped.
    pub fn spawn_cleanup_task_with_shutdown(
        self: &Arc<Self>,
        interval: Duration,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let weak = Arc::downgrade(self);
        let interval = interval.max(Duration::from_millis(1));
        tokio::spawn(async move {
            if *shutdown.borrow() {
                return;
            }
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    res = shutdown.changed() => {
                        if res.is_err() || *shutdown.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let Some(store) = weak.upgrade() else {
                            break;
                        };
                        store.cleanup_expired();
                    }
                }
            }
        })
    }

    /// Current non-expired relay token count (useful for stats/tests).
    pub fn active_token_count(&self) -> usize {
        let now = Instant::now();
        self.relay_tokens
            .iter()
            .filter(|entry| entry.value().1 > now)
            .count()
    }
}

#[async_trait]
impl EphemeralStore for MemoryEphemeralStore {
    async fn set_online(&self, device_id: &str, ttl: Duration) -> Result<(), EphemeralError> {
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            self.online.remove(device_id);
            return Ok(());
        };
        self.online.insert(device_id.to_string(), expires_at);
        Ok(())
    }

    async fn remove_online(&self, device_id: &str) -> Result<(), EphemeralError> {
        self.online.remove(device_id);
        Ok(())
    }

    async fn is_online(&self, device_id: &str) -> Result<bool, EphemeralError> {
        let now = Instant::now();
        let expired = match self.online.get(device_id) {
            Some(entry) if *entry.value() > now => return Ok(true),
            Some(_) => true,
            None => false,
        };
        if expired {
            self.online.remove_if(device_id, |_, exp| *exp <= now);
        }
        Ok(false)
    }

    async fn online_ids(&self) -> Result<Vec<String>, EphemeralError> {
        let now = Instant::now();
        let mut ids = Vec::new();
        let mut stale = Vec::new();
        for entry in self.online.iter() {
            if *entry.value() > now {
                ids.push(entry.key().clone());
            } else {
                stale.push(entry.key().clone());
            }
        }
        for key in stale {
            self.online.remove_if(&key, |_, exp| *exp <= now);
        }
        ids.sort();
        Ok(ids)
    }

    async fn set_public_udp(
        &self,
        device_id: &str,
        addr: SocketAddr,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            self.public_udp.remove(device_id);
            return Ok(());
        };
        self.public_udp
            .insert(device_id.to_string(), (addr, expires_at));
        Ok(())
    }

    async fn get_public_udp(&self, device_id: &str) -> Result<Option<SocketAddr>, EphemeralError> {
        let now = Instant::now();
        let expired = match self.public_udp.get(device_id) {
            Some(entry) if entry.value().1 > now => return Ok(Some(entry.value().0)),
            Some(_) => true,
            None => false,
        };
        if expired {
            self.public_udp
                .remove_if(device_id, |_, (_, exp)| *exp <= now);
        }
        Ok(None)
    }

    async fn put_relay_token(
        &self,
        token: &str,
        record: RelayTokenRecord,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            self.relay_tokens.remove(token);
            return Ok(());
        };
        let _guard = self
            .token_insert_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self.relay_tokens.contains_key(token)
            && self.relay_tokens.len() >= self.max_relay_tokens
        {
            let now = Instant::now();
            self.relay_tokens.retain(|_, (_, exp)| *exp > now);
            if self.relay_tokens.len() >= self.max_relay_tokens {
                return Err(EphemeralError::RelayTokenCapacityExceeded(
                    self.max_relay_tokens,
                ));
            }
        }
        self.relay_tokens
            .insert(token.to_string(), (record, expires_at));
        Ok(())
    }

    async fn take_relay_token(
        &self,
        token: &str,
    ) -> Result<Option<RelayTokenRecord>, EphemeralError> {
        let now = Instant::now();
        match self.relay_tokens.remove(token) {
            Some((_, (record, exp))) if exp > now => Ok(Some(record)),
            _ => Ok(None),
        }
    }

    async fn get_allow_registration(&self) -> Result<Option<bool>, EphemeralError> {
        let now = Instant::now();
        let mut guard = self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some((val, exp)) if exp > now => Ok(Some(val)),
            Some(_) => {
                *guard = None;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn set_allow_registration(
        &self,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            return self.invalidate_allow_registration().await;
        };
        let mut guard = self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some((allowed, expires_at));
        Ok(())
    }

    async fn set_allow_registration_if_absent(
        &self,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            return Ok(());
        };
        let now = Instant::now();
        let mut guard = self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some((_, exp)) if exp > now => Ok(()),
            _ => {
                *guard = Some((allowed, expires_at));
                Ok(())
            }
        }
    }

    async fn invalidate_allow_registration(&self) -> Result<(), EphemeralError> {
        let mut guard = self
            .allow_registration
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = None;
        Ok(())
    }

    async fn revoke_cert_serial(
        &self,
        serial_hex: &str,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let key = serial_hex.trim().to_ascii_lowercase();
        let Some(expires_at) = Self::compute_expiry(ttl)? else {
            self.revoked_certs.remove(&key);
            return Ok(());
        };
        self.revoked_certs.insert(key, expires_at);
        Ok(())
    }

    async fn is_cert_revoked(&self, serial_hex: &str) -> Result<bool, EphemeralError> {
        let key = serial_hex.trim().to_ascii_lowercase();
        let now = Instant::now();
        let expired = match self.revoked_certs.get(&key) {
            Some(entry) if *entry.value() > now => return Ok(true),
            Some(_) => true,
            None => false,
        };
        if expired {
            self.revoked_certs.remove_if(&key, |_, exp| *exp <= now);
        }
        Ok(false)
    }

    async fn active_relay_token_count(&self) -> Result<Option<usize>, EphemeralError> {
        Ok(Some(self.active_token_count()))
    }

    fn backend_name(&self) -> &'static str {
        "memory"
    }

    async fn health_check(&self) -> Result<(), EphemeralError> {
        Ok(())
    }
}

/// Optional Redis-backed `EphemeralStore` enabled when `--redis-url` / `REDIS_URL` is configured.
#[derive(Clone)]
pub struct RedisEphemeralStore {
    conn: ConnectionManager,
}

impl RedisEphemeralStore {
    pub async fn connect(redis_url: &str) -> Result<Self, EphemeralError> {
        let client = redis::Client::open(redis_url)?;
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(REDIS_CONNECT_TIMEOUT)
            .set_response_timeout(REDIS_RESPONSE_TIMEOUT)
            .set_number_of_retries(REDIS_NUMBER_OF_RETRIES);
        let conn = ConnectionManager::new_with_config(client, config).await?;
        let store = Self { conn };
        store.health_check().await?;
        Ok(store)
    }

    fn ttl_millis(ttl: Duration) -> Result<Option<u64>, EphemeralError> {
        if ttl.is_zero() {
            return Ok(None);
        }
        let raw_ms = ttl.as_millis().max(1);
        let ms = u64::try_from(raw_ms)
            .ok()
            .filter(|&m| m <= i64::MAX as u64)
            .ok_or(EphemeralError::InvalidTtl(
                "TTL exceeds Redis millisecond range",
            ))?;
        Ok(Some(ms))
    }

    fn now_unix_ms() -> i64 {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        i64::try_from(ms).unwrap_or(i64::MAX)
    }
}

#[async_trait]
impl EphemeralStore for RedisEphemeralStore {
    async fn set_online(&self, device_id: &str, ttl: Duration) -> Result<(), EphemeralError> {
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            return self.remove_online(device_id).await;
        };
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_ONLINE_PREFIX}{device_id}");
        let expiry_ms =
            Self::now_unix_ms().saturating_add(i64::try_from(ttl_ms).unwrap_or(i64::MAX));
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("PX")
            .arg(ttl_ms)
            .ignore()
            .cmd("ZADD")
            .arg(REDIS_ONLINE_ZSET)
            .arg(expiry_ms as f64)
            .arg(device_id)
            .ignore();
        let _: () = pipe.query_async(&mut conn).await?;
        Ok(())
    }

    async fn remove_online(&self, device_id: &str) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_ONLINE_PREFIX}{device_id}");
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("DEL")
            .arg(&key)
            .ignore()
            .cmd("ZREM")
            .arg(REDIS_ONLINE_ZSET)
            .arg(device_id)
            .ignore();
        let _: () = pipe.query_async(&mut conn).await?;
        Ok(())
    }

    async fn is_online(&self, device_id: &str) -> Result<bool, EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_ONLINE_PREFIX}{device_id}");
        let exists: bool = conn.exists(key).await?;
        Ok(exists)
    }

    async fn online_ids(&self) -> Result<Vec<String>, EphemeralError> {
        let mut conn = self.conn.clone();
        let now_ms = Self::now_unix_ms();
        let min_exclusive = format!("({now_ms}");
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("ZREMRANGEBYSCORE")
            .arg(REDIS_ONLINE_ZSET)
            .arg("-inf")
            .arg(now_ms)
            .ignore()
            .cmd("ZRANGEBYSCORE")
            .arg(REDIS_ONLINE_ZSET)
            .arg(&min_exclusive)
            .arg("+inf");
        let (mut ids,): (Vec<String>,) = pipe.query_async(&mut conn).await?;
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    async fn set_public_udp(
        &self,
        device_id: &str,
        addr: SocketAddr,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_UDP_PREFIX}{device_id}");
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            let _: () = conn.del(&key).await?;
            return Ok(());
        };
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(addr.to_string())
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn get_public_udp(&self, device_id: &str) -> Result<Option<SocketAddr>, EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_UDP_PREFIX}{device_id}");
        let val: Option<String> = conn.get(key).await?;
        match val {
            Some(raw) => {
                let addr = raw
                    .parse::<SocketAddr>()
                    .map_err(|_| EphemeralError::InvalidAddress(raw))?;
                Ok(Some(addr))
            }
            None => Ok(None),
        }
    }

    async fn put_relay_token(
        &self,
        token: &str,
        record: RelayTokenRecord,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_TOKEN_PREFIX}{token}");
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            let _: () = conn.del(&key).await?;
            return Ok(());
        };
        let payload = serde_json::to_string(&record)?;
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(payload)
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn take_relay_token(
        &self,
        token: &str,
    ) -> Result<Option<RelayTokenRecord>, EphemeralError> {
        let mut conn = self.conn.clone();
        let key = format!("{REDIS_TOKEN_PREFIX}{token}");
        let raw: Option<String> = redis::cmd("GETDEL")
            .arg(&key)
            .query_async(&mut conn)
            .await?;
        match raw {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    async fn get_allow_registration(&self) -> Result<Option<bool>, EphemeralError> {
        let mut conn = self.conn.clone();
        let val: Option<String> = conn.get(REDIS_ALLOW_REGISTRATION_KEY).await?;
        match val.as_deref() {
            Some("true") | Some("1") => Ok(Some(true)),
            Some("false") | Some("0") => Ok(Some(false)),
            Some(other) => Err(EphemeralError::InvalidBool(other.to_string())),
            None => Ok(None),
        }
    }

    async fn set_allow_registration(
        &self,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            return self.invalidate_allow_registration().await;
        };
        let mut conn = self.conn.clone();
        let payload = if allowed { "true" } else { "false" };
        let _: () = redis::cmd("SET")
            .arg(REDIS_ALLOW_REGISTRATION_KEY)
            .arg(payload)
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn set_allow_registration_if_absent(
        &self,
        allowed: bool,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            return Ok(());
        };
        let mut conn = self.conn.clone();
        let payload = if allowed { "true" } else { "false" };
        let _: Option<String> = redis::cmd("SET")
            .arg(REDIS_ALLOW_REGISTRATION_KEY)
            .arg(payload)
            .arg("NX")
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn invalidate_allow_registration(&self) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let _: () = conn.del(REDIS_ALLOW_REGISTRATION_KEY).await?;
        Ok(())
    }

    async fn revoke_cert_serial(
        &self,
        serial_hex: &str,
        ttl: Duration,
    ) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let serial = serial_hex.trim().to_ascii_lowercase();
        let key = format!("{REDIS_REVOKED_CERT_PREFIX}{serial}");
        let Some(ttl_ms) = Self::ttl_millis(ttl)? else {
            let _: () = conn.del(&key).await?;
            return Ok(());
        };
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .arg("PX")
            .arg(ttl_ms)
            .query_async(&mut conn)
            .await?;
        Ok(())
    }

    async fn is_cert_revoked(&self, serial_hex: &str) -> Result<bool, EphemeralError> {
        let mut conn = self.conn.clone();
        let serial = serial_hex.trim().to_ascii_lowercase();
        let key = format!("{REDIS_REVOKED_CERT_PREFIX}{serial}");
        let exists: bool = conn.exists(key).await?;
        Ok(exists)
    }

    fn backend_name(&self) -> &'static str {
        "redis"
    }

    async fn health_check(&self) -> Result<(), EphemeralError> {
        let mut conn = self.conn.clone();
        let cmd = redis::cmd("PING");
        tokio::time::timeout(
            REDIS_HEALTH_CHECK_TIMEOUT,
            cmd.query_async::<String>(&mut conn),
        )
        .await
        .map_err(|_| EphemeralError::Timeout("redis health_check timed out"))??;
        Ok(())
    }
}

/// Write-through / read-through cache helper for persistent settings backed by `SettingsStore`
/// and cached in `EphemeralStore` (Redis or Memory).
#[derive(Clone)]
pub struct SettingsCache {
    store: Arc<dyn SettingsStore>,
    ephemeral: Arc<dyn EphemeralStore>,
    ttl: Duration,
    write_epoch: Arc<std::sync::atomic::AtomicU64>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SettingsCache {
    pub fn new(store: Arc<dyn SettingsStore>, ephemeral: Arc<dyn EphemeralStore>) -> Self {
        Self::with_ttl(store, ephemeral, DEFAULT_SETTINGS_CACHE_TTL)
    }

    pub fn with_ttl(
        store: Arc<dyn SettingsStore>,
        ephemeral: Arc<dyn EphemeralStore>,
        ttl: Duration,
    ) -> Self {
        Self {
            store,
            ephemeral,
            ttl,
            write_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub async fn get_allow_registration(&self) -> Result<bool, StoreError> {
        match self.ephemeral.get_allow_registration().await {
            Ok(Some(cached)) => return Ok(cached),
            Ok(None) => {}
            Err(err) => {
                warn!(error = %err, "ephemeral allow_registration cache read failed; falling back to database");
            }
        }

        let epoch_before = self.write_epoch.load(std::sync::atomic::Ordering::Acquire);
        let allowed = self.store.get_allow_registration().await?;
        if self.write_epoch.load(std::sync::atomic::Ordering::Acquire) == epoch_before {
            if let Err(err) = self
                .ephemeral
                .set_allow_registration_if_absent(allowed, self.ttl)
                .await
            {
                warn!(error = %err, "failed to backfill allow_registration cache");
            }
        }
        Ok(allowed)
    }

    pub async fn get_allow_registration_with_meta(
        &self,
    ) -> Result<(bool, Option<chrono::DateTime<chrono::Utc>>), StoreError> {
        let (allowed, updated_at) = self.store.get_allow_registration_with_meta().await?;
        if let Err(err) = self
            .ephemeral
            .set_allow_registration_if_absent(allowed, self.ttl)
            .await
        {
            warn!(error = %err, "failed to backfill allow_registration cache");
        }
        Ok((allowed, updated_at))
    }

    pub async fn set_allow_registration(
        &self,
        allowed: bool,
        updated_by: Option<Uuid>,
    ) -> Result<(), StoreError> {
        let _guard = self.write_lock.lock().await;
        self.write_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.store
            .set_allow_registration(allowed, updated_by)
            .await?;
        self.write_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Err(err) = self
            .ephemeral
            .set_allow_registration(allowed, self.ttl)
            .await
        {
            warn!(error = %err, "failed to update allow_registration cache; invalidating");
            if let Err(inv_err) = self.ephemeral.invalidate_allow_registration().await {
                return Err(StoreError::Cache(format!(
                    "failed to update ({err}) and invalidate ({inv_err}) allow_registration cache"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryDeviceStore;

    #[tokio::test]
    async fn memory_ephemeral_online_udp_and_token_ttl_expiry() {
        let store = MemoryEphemeralStore::new();
        assert_eq!(store.backend_name(), "memory");
        store.health_check().await.unwrap();

        // Online set + read + expiry.
        store
            .set_online("111222333", Duration::from_millis(60))
            .await
            .unwrap();
        store
            .set_online("444555666", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(store.is_online("111222333").await.unwrap());
        assert_eq!(
            store.online_ids().await.unwrap(),
            vec!["111222333".to_string(), "444555666".to_string()]
        );

        // Public UDP set + read + expiry.
        let addr: SocketAddr = "203.0.113.9:51234".parse().unwrap();
        store
            .set_public_udp("111222333", addr, Duration::from_millis(60))
            .await
            .unwrap();
        assert_eq!(store.get_public_udp("111222333").await.unwrap(), Some(addr));

        // Relay token: one-time take semantics.
        let tok_rec = RelayTokenRecord {
            session_id: "sess-1".into(),
            host_id: "111222333".into(),
            client_id: "444555666".into(),
        };
        store
            .put_relay_token("tok-once", tok_rec.clone(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(
            store.take_relay_token("tok-once").await.unwrap(),
            Some(tok_rec.clone())
        );
        // Second take must return None!
        assert_eq!(store.take_relay_token("tok-once").await.unwrap(), None);

        // Relay token TTL expiry.
        store
            .put_relay_token("tok-exp", tok_rec, Duration::from_millis(60))
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(90)).await;

        assert!(!store.is_online("111222333").await.unwrap());
        assert!(store.is_online("444555666").await.unwrap());
        assert_eq!(
            store.online_ids().await.unwrap(),
            vec!["444555666".to_string()]
        );
        assert_eq!(store.get_public_udp("111222333").await.unwrap(), None);
        assert_eq!(store.take_relay_token("tok-exp").await.unwrap(), None);

        store.remove_online("444555666").await.unwrap();
        assert!(!store.is_online("444555666").await.unwrap());
        store.cleanup_expired();
        assert_eq!(store.active_token_count(), 0);
    }

    #[tokio::test]
    async fn h6_memory_relay_token_cap_enforced_and_expired_reclaimed() {
        let store = MemoryEphemeralStore::with_max_relay_tokens(2);
        let rec = RelayTokenRecord {
            session_id: "s1".into(),
            host_id: "111111111".into(),
            client_id: "222222222".into(),
        };

        store
            .put_relay_token("tok-1", rec.clone(), Duration::from_millis(40))
            .await
            .unwrap();
        store
            .put_relay_token("tok-2", rec.clone(), Duration::from_secs(5))
            .await
            .unwrap();

        // Third distinct active token must be rejected when cap is reached.
        let err = store
            .put_relay_token("tok-3", rec.clone(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, EphemeralError::RelayTokenCapacityExceeded(2)));
        assert_eq!(store.active_relay_token_count().await.unwrap(), Some(2));

        // Wait for tok-1 to expire; put_relay_token must reclaim expired slots.
        tokio::time::sleep(Duration::from_millis(60)).await;
        store
            .put_relay_token("tok-3", rec.clone(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(store.active_token_count(), 2);

        // Taking a token frees a slot immediately.
        assert!(store.take_relay_token("tok-2").await.unwrap().is_some());
        store
            .put_relay_token("tok-4", rec, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(store.active_token_count(), 2);
    }

    #[tokio::test]
    async fn l3_millisecond_ttl_zero_deletes_and_overflow_safe() {
        let store = MemoryEphemeralStore::new();
        let addr: SocketAddr = "203.0.113.10:50000".parse().unwrap();
        let rec = RelayTokenRecord {
            session_id: "sess-l3".into(),
            host_id: "111222333".into(),
            client_id: "444555666".into(),
        };

        // Zero TTL deletes existing entries immediately and does not store new ones.
        store
            .set_online("111222333", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(store.is_online("111222333").await.unwrap());
        store.set_online("111222333", Duration::ZERO).await.unwrap();
        assert!(!store.is_online("111222333").await.unwrap());

        store
            .set_public_udp("111222333", addr, Duration::from_secs(5))
            .await
            .unwrap();
        store
            .set_public_udp("111222333", addr, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(store.get_public_udp("111222333").await.unwrap(), None);

        store
            .put_relay_token("tok-zero", rec.clone(), Duration::from_secs(5))
            .await
            .unwrap();
        store
            .put_relay_token("tok-zero", rec.clone(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(store.take_relay_token("tok-zero").await.unwrap(), None);

        // Overflowing Duration::MAX must return Err(InvalidTtl) instead of panicking.
        assert!(matches!(
            store.set_online("111222333", Duration::MAX).await,
            Err(EphemeralError::InvalidTtl(_))
        ));
        assert!(matches!(
            store.set_public_udp("111222333", addr, Duration::MAX).await,
            Err(EphemeralError::InvalidTtl(_))
        ));
        assert!(matches!(
            store.put_relay_token("tok-max", rec, Duration::MAX).await,
            Err(EphemeralError::InvalidTtl(_))
        ));

        // Verify Redis millisecond TTL conversion helper (L3).
        assert_eq!(
            RedisEphemeralStore::ttl_millis(Duration::ZERO).unwrap(),
            None
        );
        assert_eq!(
            RedisEphemeralStore::ttl_millis(Duration::from_nanos(500)).unwrap(),
            Some(1)
        );
        assert_eq!(
            RedisEphemeralStore::ttl_millis(Duration::from_millis(250)).unwrap(),
            Some(250)
        );
        assert!(matches!(
            RedisEphemeralStore::ttl_millis(Duration::MAX),
            Err(EphemeralError::InvalidTtl(_))
        ));
    }

    #[tokio::test]
    async fn l4_cleanup_task_clamps_zero_interval_and_exits_on_drop_or_shutdown() {
        // 1. Duration::ZERO must not panic (clamped to 1ms), and dropping Arc stops the task.
        let store = Arc::new(MemoryEphemeralStore::new());
        store
            .set_online("111222333", Duration::from_millis(10))
            .await
            .unwrap();
        let handle = store.spawn_cleanup_task(Duration::ZERO);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(!store.is_online("111222333").await.unwrap());

        drop(store);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cleanup task must exit after Arc<MemoryEphemeralStore> is dropped")
            .unwrap();

        // 2. Shutdown signal stops spawn_cleanup_task_with_shutdown even while Arc is still alive.
        let store2 = Arc::new(MemoryEphemeralStore::new());
        let (tx, rx) = watch::channel(false);
        let handle2 = store2.spawn_cleanup_task_with_shutdown(Duration::from_secs(60), rx);
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle2)
            .await
            .expect("cleanup task must exit promptly on shutdown signal")
            .unwrap();
    }

    #[tokio::test]
    async fn settings_cache_and_cert_revocation_cache_work() {
        let persistent = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let cache = SettingsCache::with_ttl(
            persistent.clone(),
            ephemeral.clone(),
            Duration::from_millis(40),
        );

        // Cache miss reads from persistent store (seeded true) and backfills ephemeral cache.
        assert_eq!(ephemeral.get_allow_registration().await.unwrap(), None);
        assert!(cache.get_allow_registration().await.unwrap());
        assert_eq!(
            ephemeral.get_allow_registration().await.unwrap(),
            Some(true)
        );

        // Admin updates via cache -> updates both PG/memory store and ephemeral cache immediately.
        cache.set_allow_registration(false, None).await.unwrap();
        assert!(!persistent.get_allow_registration().await.unwrap());
        assert_eq!(
            ephemeral.get_allow_registration().await.unwrap(),
            Some(false)
        );
        assert!(!cache.get_allow_registration().await.unwrap());

        //Wait for TTL expiry -> ephemeral returns None, next read falls back to persistent store and backfills.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(ephemeral.get_allow_registration().await.unwrap(), None);
        assert!(!cache.get_allow_registration().await.unwrap());
        assert_eq!(
            ephemeral.get_allow_registration().await.unwrap(),
            Some(false)
        );

        // Cert revocation TTL cache.
        ephemeral
            .revoke_cert_serial("ABCD1234", Duration::from_millis(40))
            .await
            .unwrap();
        assert!(ephemeral.is_cert_revoked("abcd1234").await.unwrap());
        assert!(!ephemeral.is_cert_revoked("deadbeef").await.unwrap());

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!ephemeral.is_cert_revoked("ABCD1234").await.unwrap());
    }
}
