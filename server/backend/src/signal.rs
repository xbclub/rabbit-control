use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::auth::{AuthAuthority, AuthError, OwnerGrantTicket, OWNER_GRANT_DEFAULT_TTL};
use crate::ephemeral::{
    EphemeralError, EphemeralStore, RelayTokenRecord, DEFAULT_MAX_RELAY_TOKENS,
};
use crate::store::{
    format_device_id, is_valid_device_id, normalize_device_id, AuthStore, DeviceStore,
    RegisterInput, StoreError, MAX_DEVICE_PUBKEY_LEN, STATUS_ACTIVE,
};

pub const UDP_PROBE_MAGIC: &[u8; 4] = b"RCSP";
pub const UDP_ANSWER_MAGIC: &[u8; 4] = b"RCSA";
pub const UDP_PROTO_VERSION: u8 = 1;
pub const UDP_PROBE_LEN: usize = 22;
pub const ANONYMOUS_DEVICE_ID: &str = "000000000";

pub const MAX_SIGNAL_FRAME_LEN: usize = 65_536;
pub const MAX_SESSION_ID_LEN: usize = 64;
pub const MAX_NONCE_LEN: usize = 64;
pub const MAX_LAN_ADDRS: usize = 16;
pub const MAX_LAN_ADDR_LEN: usize = 64;
pub const MAX_HOSTNAME_LEN: usize = crate::store::MAX_HOSTNAME_LEN;
pub const MAX_OS_LEN: usize = crate::store::MAX_OS_LEN;
pub const MAX_DEVICE_GROUP_LEN: usize = crate::store::MAX_DEVICE_GROUP_LEN;

pub const DEFAULT_MAX_SIGNAL_CONNECTIONS: usize = 8_192;
pub const DEFAULT_CONNECT_RATE_LIMIT_MAX: usize = 20;
pub const DEFAULT_CONNECT_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);
pub const DEFAULT_UDP_RATE_LIMIT_MAX: usize = 30;
pub const DEFAULT_UDP_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);

const DEVICE_LOCK_STRIPES: usize = 64;
const INITIAL_FRAME_BUF_CAP: usize = 4_096;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
const UDP_EPHEMERAL_UPDATE_TIMEOUT: Duration = Duration::from_millis(200);
const UDP_EPHEMERAL_QUEUE_CAPACITY: usize = 1_024;
const MAX_RATE_LIMIT_TRACKED_KEYS: usize = 16_384;

/// Canonicalize IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) to native `IpAddr::V4`.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        IpAddr::V4(v4) => IpAddr::V4(v4),
    }
}

/// Canonicalize a `SocketAddr`'s IP (converting IPv4-mapped IPv6 to IPv4).
pub fn canonical_socket_addr(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(canonical_ip(addr.ip()), addr.port())
}

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("invalid UDP packet: {0}")]
    InvalidUdpPacket(&'static str),
    #[error("invalid TCP frame: {0}")]
    InvalidFrame(String),
    #[error("frame length {0} exceeds maximum {MAX_SIGNAL_FRAME_LEN}")]
    FrameTooLarge(usize),
    #[error("JSON codec error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

// ─────────────────────────────────────────────────────────────────────────────
// UDP STUN-lite Codec (`RCSP` / `RCSA`)
// ─────────────────────────────────────────────────────────────────────────────

/// Wire layout (22 bytes fixed):
/// `[b"RCSP" (4B)][ver=1 (1B)][device_id: 9B ASCII][nonce: 8B]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpProbeRequest {
    pub device_id: String,
    pub nonce: [u8; 8],
}

impl UdpProbeRequest {
    pub fn encode(&self) -> Result<[u8; UDP_PROBE_LEN], SignalError> {
        let id_bytes = self.device_id.as_bytes();
        if id_bytes.len() != 9 || !id_bytes.iter().all(u8::is_ascii_digit) {
            return Err(SignalError::InvalidUdpPacket(
                "device_id must be exactly 9 ASCII digits",
            ));
        }
        let mut out = [0u8; UDP_PROBE_LEN];
        out[0..4].copy_from_slice(UDP_PROBE_MAGIC);
        out[4] = UDP_PROTO_VERSION;
        out[5..14].copy_from_slice(id_bytes);
        out[14..22].copy_from_slice(&self.nonce);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, SignalError> {
        if buf.len() != UDP_PROBE_LEN {
            return Err(SignalError::InvalidUdpPacket(
                "RCSP packet must be exactly 22 bytes",
            ));
        }
        if &buf[0..4] != UDP_PROBE_MAGIC {
            return Err(SignalError::InvalidUdpPacket("bad RCSP magic"));
        }
        if buf[4] != UDP_PROTO_VERSION {
            return Err(SignalError::InvalidUdpPacket("unsupported RCSP version"));
        }
        let id_bytes = &buf[5..14];
        if !id_bytes.iter().all(u8::is_ascii_digit) {
            return Err(SignalError::InvalidUdpPacket(
                "device_id must be 9 ASCII digits",
            ));
        }
        let device_id = std::str::from_utf8(id_bytes)
            .map_err(|_| SignalError::InvalidUdpPacket("invalid UTF-8 device_id"))?
            .to_string();
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&buf[14..22]);
        Ok(Self { device_id, nonce })
    }
}

/// Wire layout (20 bytes IPv4 / 32 bytes IPv6):
/// `[b"RCSA" (4B)][ver=1 (1B)][ip_len: 1B (4 or 16)][ip_bytes][port: u16 BE][nonce: 8B]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpProbeResponse {
    pub observed_addr: SocketAddr,
    pub nonce: [u8; 8],
}

impl UdpProbeResponse {
    pub fn encode(&self) -> Vec<u8> {
        let canonical = canonical_socket_addr(self.observed_addr);
        let mut out = match canonical.ip() {
            IpAddr::V4(v4) => {
                let mut buf = Vec::with_capacity(20);
                buf.extend_from_slice(UDP_ANSWER_MAGIC);
                buf.push(UDP_PROTO_VERSION);
                buf.push(4);
                buf.extend_from_slice(&v4.octets());
                buf
            }
            IpAddr::V6(v6) => {
                let mut buf = Vec::with_capacity(32);
                buf.extend_from_slice(UDP_ANSWER_MAGIC);
                buf.push(UDP_PROTO_VERSION);
                buf.push(16);
                buf.extend_from_slice(&v6.octets());
                buf
            }
        };
        out.extend_from_slice(&canonical.port().to_be_bytes());
        out.extend_from_slice(&self.nonce);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, SignalError> {
        if buf.len() < 6 {
            return Err(SignalError::InvalidUdpPacket("RCSA packet too short"));
        }
        if &buf[0..4] != UDP_ANSWER_MAGIC {
            return Err(SignalError::InvalidUdpPacket("bad RCSA magic"));
        }
        if buf[4] != UDP_PROTO_VERSION {
            return Err(SignalError::InvalidUdpPacket("unsupported RCSA version"));
        }
        let ip_len = buf[5] as usize;
        let expected_len = 6 + ip_len + 2 + 8;
        if (ip_len != 4 && ip_len != 16) || buf.len() != expected_len {
            return Err(SignalError::InvalidUdpPacket(
                "invalid RCSA ip_len or packet length",
            ));
        }
        let ip = if ip_len == 4 {
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&buf[6..10]);
            IpAddr::V4(Ipv4Addr::from(octets))
        } else {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[6..22]);
            IpAddr::V6(Ipv6Addr::from(octets))
        };
        let port_offset = 6 + ip_len;
        let port = u16::from_be_bytes([buf[port_offset], buf[port_offset + 1]]);
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&buf[port_offset + 2..port_offset + 10]);
        Ok(Self {
            observed_addr: SocketAddr::new(ip, port),
            nonce,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Native TCP Framed Signaling Codec (`[u32 BE length (1..=65536)][UTF-8 JSON]`)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMsg {
    Register {
        #[serde(default)]
        device_id: Option<String>,
        device_key: String,
        #[serde(default)]
        hostname: String,
        #[serde(default)]
        os: String,
        #[serde(default, alias = "group")]
        device_group: String,
        #[serde(default)]
        lan_addrs: Vec<String>,
        #[serde(default)]
        access_token: Option<String>,
        #[serde(default)]
        device_pubkey: Option<String>,
        #[serde(default)]
        allow_remote_assist: Option<bool>,
    },
    Registered {
        device_id: String,
        formatted_id: String,
        server_time: DateTime<Utc>,
        udp_probe_addr: String,
        relay_addr: String,
    },
    Heartbeat {
        #[serde(default)]
        lan_addrs: Vec<String>,
    },
    HeartbeatAck {
        server_time: DateTime<Utc>,
        #[serde(skip_serializing_if = "Option::is_none")]
        public_udp_addr: Option<String>,
        online_peers: usize,
    },
    UpdateAssistState {
        allow_remote_assist: bool,
    },
    DeviceStatusChanged {
        device_id: String,
        online: bool,
        hostname: String,
        alias: String,
        os: String,
        allow_remote_assist: bool,
    },
    ConnectRequest {
        target_id: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        lan_addrs: Vec<String>,
        #[serde(default)]
        nonce: Option<String>,
    },
    PunchCoord {
        session_id: String,
        role: String,
        peer_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        peer_public_udp_addr: Option<String>,
        peer_lan_addrs: Vec<String>,
        nonce: String,
        relay_token: String,
        relay_addr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_grant: Option<OwnerGrantTicket>,
        #[serde(default)]
        is_owner_connection: bool,
    },
    RelayAlloc {
        target_id: String,
        #[serde(default)]
        session_id: Option<String>,
    },
    RelayReady {
        session_id: String,
        role: String,
        peer_id: String,
        relay_token: String,
        relay_addr: String,
        expires_in_sec: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner_grant: Option<OwnerGrantTicket>,
        #[serde(default)]
        is_owner_connection: bool,
    },
    PeerOffline {
        target_id: String,
        session_id: String,
    },
    Kicked {
        reason: String,
    },
    Error {
        code: String,
        message: String,
    },
}

pub fn encode_signal_frame(msg: &SignalMsg) -> Result<Vec<u8>, SignalError> {
    let payload = serde_json::to_vec(msg)?;
    if payload.is_empty() {
        return Err(SignalError::InvalidFrame("empty JSON payload".into()));
    }
    if payload.len() > MAX_SIGNAL_FRAME_LEN {
        return Err(SignalError::FrameTooLarge(payload.len()));
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_signal_frame(bytes: &[u8]) -> Result<SignalMsg, SignalError> {
    if bytes.len() < 4 {
        return Err(SignalError::InvalidFrame(
            "frame shorter than 4-byte header".into(),
        ));
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if len == 0 {
        return Err(SignalError::InvalidFrame(
            "frame length must be >= 1".into(),
        ));
    }
    if len > MAX_SIGNAL_FRAME_LEN {
        return Err(SignalError::FrameTooLarge(len));
    }
    if bytes.len() != 4 + len {
        return Err(SignalError::InvalidFrame(format!(
            "frame length mismatch: header={len}, actual={}",
            bytes.len() - 4
        )));
    }
    Ok(serde_json::from_slice(&bytes[4..])?)
}

pub async fn read_signal_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<SignalMsg>, SignalError> {
    let mut header = [0u8; 4];
    let mut filled = 0usize;
    while filled < 4 {
        let n = reader.read(&mut header[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            return Err(SignalError::InvalidFrame(
                "unexpected EOF while reading frame header".into(),
            ));
        }
        filled += n;
    }

    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Err(SignalError::InvalidFrame(
            "frame length must be >= 1".into(),
        ));
    }
    if len > MAX_SIGNAL_FRAME_LEN {
        return Err(SignalError::FrameTooLarge(len));
    }

    let mut payload = Vec::with_capacity(len.min(INITIAL_FRAME_BUF_CAP));
    let mut limited = (&mut *reader).take(len as u64);
    limited.read_to_end(&mut payload).await?;
    if payload.len() != len {
        return Err(SignalError::InvalidFrame(format!(
            "unexpected EOF while reading frame payload: expected {len} bytes, got {}",
            payload.len()
        )));
    }
    let msg = serde_json::from_slice(&payload)?;
    Ok(Some(msg))
}

pub async fn write_signal_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &SignalMsg,
) -> Result<(), SignalError> {
    let frame = encode_signal_frame(msg)?;
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SignalHub (Native TCP Signaling + UDP STUN-lite Probe)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SignalConfig {
    pub online_ttl: Duration,
    pub udp_ttl: Duration,
    pub relay_token_ttl: Duration,
    pub handshake_timeout: Duration,
    pub read_idle_timeout: Duration,
    pub write_timeout: Duration,
    pub channel_capacity: usize,
    pub max_connections: usize,
    pub max_relay_tokens: usize,
    pub connect_rate_limit_max: usize,
    pub connect_rate_limit_window: Duration,
    pub udp_rate_limit_max: usize,
    pub udp_rate_limit_window: Duration,
    pub public_udp_probe_addr: String,
    pub public_relay_addr: String,
    pub require_auth: bool,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            online_ttl: Duration::from_secs(150),
            udp_ttl: Duration::from_secs(120),
            relay_token_ttl: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(10),
            read_idle_timeout: Duration::from_secs(120),
            write_timeout: Duration::from_secs(5),
            channel_capacity: 64,
            max_connections: DEFAULT_MAX_SIGNAL_CONNECTIONS,
            max_relay_tokens: DEFAULT_MAX_RELAY_TOKENS,
            connect_rate_limit_max: DEFAULT_CONNECT_RATE_LIMIT_MAX,
            connect_rate_limit_window: DEFAULT_CONNECT_RATE_LIMIT_WINDOW,
            udp_rate_limit_max: DEFAULT_UDP_RATE_LIMIT_MAX,
            udp_rate_limit_window: DEFAULT_UDP_RATE_LIMIT_WINDOW,
            public_udp_probe_addr: "127.0.0.1:21116".to_string(),
            public_relay_addr: "127.0.0.1:21117".to_string(),
            require_auth: true,
        }
    }
}

fn validate_ascii_token(val: &str, max_len: usize, field: &'static str) -> Result<(), String> {
    if val.len() > max_len {
        return Err(format!("{field} exceeds maximum length of {max_len} bytes"));
    }
    if !val.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "{field} must contain only printable ASCII non-whitespace characters"
        ));
    }
    Ok(())
}

fn resolve_optional_ascii_token(
    opt: Option<String>,
    max_len: usize,
    default_bytes: usize,
    field: &'static str,
) -> Result<String, String> {
    match opt {
        None => Ok(random_hex(default_bytes)),
        Some(s) if s.is_empty() => Ok(random_hex(default_bytes)),
        Some(s) => {
            validate_ascii_token(&s, max_len, field)?;
            Ok(s)
        }
    }
}

fn validate_lan_addrs(addrs: &[String]) -> Result<(), String> {
    if addrs.len() > MAX_LAN_ADDRS {
        return Err(format!(
            "lan_addrs exceeds maximum of {MAX_LAN_ADDRS} entries"
        ));
    }
    for addr in addrs {
        if addr.is_empty() || addr.len() > MAX_LAN_ADDR_LEN {
            return Err(format!(
                "each lan_addrs entry must be 1..={MAX_LAN_ADDR_LEN} bytes"
            ));
        }
        if !addr.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(
                "lan_addrs entries must contain only printable ASCII non-whitespace characters"
                    .into(),
            );
        }
    }
    Ok(())
}

fn validate_metadata_field(val: &str, max_len: usize, field: &'static str) -> Result<(), String> {
    if val.len() > max_len {
        return Err(format!("{field} exceeds maximum length of {max_len} bytes"));
    }
    if val.contains('\0') || val.chars().any(char::is_control) {
        return Err(format!(
            "{field} must not contain NUL or control characters"
        ));
    }
    Ok(())
}

fn validate_register_fields(
    hostname: &str,
    os: &str,
    device_group: &str,
    lan_addrs: &[String],
) -> Result<(), String> {
    validate_metadata_field(hostname, MAX_HOSTNAME_LEN, "hostname")?;
    validate_metadata_field(os, MAX_OS_LEN, "os")?;
    validate_metadata_field(device_group, MAX_DEVICE_GROUP_LEN, "device_group")?;
    validate_lan_addrs(lan_addrs)?;
    Ok(())
}

fn device_lock_index(device_id: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in device_id.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash as usize) % DEVICE_LOCK_STRIPES
}

fn check_sliding_window_rate_limit<K>(
    table: &DashMap<K, VecDeque<Instant>>,
    key: K,
    max_requests: usize,
    window: Duration,
) -> bool
where
    K: Eq + std::hash::Hash + Clone,
{
    if max_requests == 0 {
        return false;
    }
    let now = Instant::now();
    if table.len() >= MAX_RATE_LIMIT_TRACKED_KEYS && !table.contains_key(&key) {
        table.retain(|_, q| {
            while q
                .front()
                .is_some_and(|t| now.saturating_duration_since(*t) >= window)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
        if table.len() >= MAX_RATE_LIMIT_TRACKED_KEYS {
            let evict_keys: Vec<K> = table
                .iter()
                .take(256)
                .map(|entry| entry.key().clone())
                .collect();
            for k in evict_keys {
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

#[derive(Clone)]
struct SignalShutdown {
    cancelled: Arc<AtomicBool>,
    tx: Arc<watch::Sender<bool>>,
}

impl SignalShutdown {
    fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            tx: Arc::new(tx),
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(true);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        if *rx.borrow_and_update() || self.is_cancelled() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow_and_update() || self.is_cancelled() {
                return;
            }
        }
    }
}

async fn wait_external_shutdown(rx: &mut Option<watch::Receiver<bool>>) {
    let Some(rx) = rx.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

struct SignalTaskGuard {
    hub: Arc<SignalHub>,
}

impl SignalTaskGuard {
    fn enter(hub: Arc<SignalHub>) -> Self {
        hub.in_flight_tasks.fetch_add(1, Ordering::AcqRel);
        Self { hub }
    }
}

impl Drop for SignalTaskGuard {
    fn drop(&mut self) {
        if self.hub.in_flight_tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.hub.tasks_drained.notify_waiters();
        }
    }
}

#[derive(Debug, Clone)]
struct PeerCancel {
    tx: Arc<watch::Sender<Option<String>>>,
}

impl PeerCancel {
    fn new() -> Self {
        let (tx, _rx) = watch::channel(None);
        Self { tx: Arc::new(tx) }
    }

    fn kick(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(reason);
                true
            }
        });
    }

    fn is_kicked(&self) -> bool {
        self.tx.borrow().is_some()
    }

    fn subscribe(&self) -> watch::Receiver<Option<String>> {
        self.tx.subscribe()
    }
}

async fn wait_kicked(rx: &mut watch::Receiver<Option<String>>) -> String {
    if let Some(reason) = rx.borrow_and_update().clone() {
        return reason;
    }
    while rx.changed().await.is_ok() {
        if let Some(reason) = rx.borrow_and_update().clone() {
            return reason;
        }
    }
    "disconnected".to_string()
}

async fn write_frame_with_timeout<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &SignalMsg,
    timeout: Duration,
) {
    let _ = tokio::time::timeout(timeout, write_signal_frame(writer, msg)).await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnqueueError {
    FrameTooLarge,
    Full,
    Closed,
}

struct DeviceStatusBroadcast<'a> {
    owner_user_id: Option<Uuid>,
    device_id: &'a str,
    online: bool,
    hostname: &'a str,
    alias: &'a str,
    os: &'a str,
    allow_remote_assist: bool,
}

#[derive(Debug, Clone)]
struct ActivePeer {
    conn_id: u64,
    tx: mpsc::Sender<SignalMsg>,
    cancel: PeerCancel,
    lan_addrs: Vec<String>,
    remote_addr: SocketAddr,
    connected_at: DateTime<Utc>,
    owner_user_id: Option<Uuid>,
    device_pubkey: String,
    allow_remote_assist: bool,
    hostname: String,
    alias: String,
    os: String,
}

pub struct SignalHub {
    device_store: Arc<dyn DeviceStore>,
    auth_store: Option<Arc<dyn AuthStore>>,
    auth_authority: Option<Arc<AuthAuthority>>,
    ephemeral: Arc<dyn EphemeralStore>,
    peers: DashMap<String, ActivePeer>,
    device_locks: [Mutex<()>; DEVICE_LOCK_STRIPES],
    connect_rate_limits: DashMap<String, VecDeque<Instant>>,
    udp_rate_limits: DashMap<IpAddr, VecDeque<Instant>>,
    issued_relay_tokens: DashMap<String, Instant>,
    conn_semaphore: Arc<Semaphore>,
    shutdown: SignalShutdown,
    in_flight_tasks: AtomicUsize,
    tasks_drained: Notify,
    config: SignalConfig,
    next_conn_id: AtomicU64,
    punch_coord_count: AtomicU64,
}

impl SignalHub {
    /// Legacy constructor without user authentication (`require_auth` forced to `false`).
    pub fn new(
        device_store: Arc<dyn DeviceStore>,
        ephemeral: Arc<dyn EphemeralStore>,
        mut config: SignalConfig,
    ) -> Self {
        config.require_auth = false;
        let max_connections = config.max_connections.max(1);
        Self {
            device_store,
            auth_store: None,
            auth_authority: None,
            ephemeral,
            peers: DashMap::new(),
            device_locks: std::array::from_fn(|_| Mutex::new(())),
            connect_rate_limits: DashMap::new(),
            udp_rate_limits: DashMap::new(),
            issued_relay_tokens: DashMap::new(),
            conn_semaphore: Arc::new(Semaphore::new(max_connections)),
            shutdown: SignalShutdown::new(),
            in_flight_tasks: AtomicUsize::new(0),
            tasks_drained: Notify::new(),
            config,
            next_conn_id: AtomicU64::new(1),
            punch_coord_count: AtomicU64::new(0),
        }
    }

    /// Production constructor wired with `AuthStore`, `AuthAuthority`, and `EphemeralStore`.
    pub fn with_auth(
        auth_store: Arc<dyn AuthStore>,
        ephemeral: Arc<dyn EphemeralStore>,
        auth_authority: Arc<AuthAuthority>,
        config: SignalConfig,
    ) -> Self {
        let device_store: Arc<dyn DeviceStore> = auth_store.clone();
        let max_connections = config.max_connections.max(1);
        Self {
            device_store,
            auth_store: Some(auth_store),
            auth_authority: Some(auth_authority),
            ephemeral,
            peers: DashMap::new(),
            device_locks: std::array::from_fn(|_| Mutex::new(())),
            connect_rate_limits: DashMap::new(),
            udp_rate_limits: DashMap::new(),
            issued_relay_tokens: DashMap::new(),
            conn_semaphore: Arc::new(Semaphore::new(max_connections)),
            shutdown: SignalShutdown::new(),
            in_flight_tasks: AtomicUsize::new(0),
            tasks_drained: Notify::new(),
            config,
            next_conn_id: AtomicU64::new(1),
            punch_coord_count: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &SignalConfig {
        &self.config
    }

    pub fn online_count(&self) -> usize {
        self.peers.len()
    }

    pub fn in_flight_task_count(&self) -> usize {
        self.in_flight_tasks.load(Ordering::Acquire)
    }

    pub fn punch_coord_count(&self) -> u64 {
        self.punch_coord_count.load(Ordering::Relaxed)
    }

    pub fn is_peer_connected(&self, device_id: &str) -> bool {
        let norm = normalize_device_id(device_id);
        self.peers.contains_key(&norm)
    }

    pub fn peer_lan_addrs(&self, device_id: &str) -> Option<Vec<String>> {
        let norm = normalize_device_id(device_id);
        self.peers.get(&norm).map(|p| p.lan_addrs.clone())
    }

    pub fn peer_remote_addr(&self, device_id: &str) -> Option<SocketAddr> {
        let norm = normalize_device_id(device_id);
        self.peers.get(&norm).map(|p| p.remote_addr)
    }

    pub fn peer_connected_at(&self, device_id: &str) -> Option<DateTime<Utc>> {
        let norm = normalize_device_id(device_id);
        self.peers.get(&norm).map(|p| p.connected_at)
    }

    async fn lock_device(&self, device_id: &str) -> tokio::sync::MutexGuard<'_, ()> {
        self.device_locks[device_lock_index(device_id)].lock().await
    }

    fn is_active_conn(&self, device_id: &str, conn_id: u64, cancel: &PeerCancel) -> bool {
        !cancel.is_kicked()
            && self
                .peers
                .get(device_id)
                .is_some_and(|peer| peer.conn_id == conn_id)
    }

    /// Cancel accepting new connections/probes, kick all connected peers, and drain all
    /// tracked signaling and UDP update tasks cleanly.
    pub async fn shutdown_all(&self) {
        self.shutdown.cancel();
        let peer_ids: Vec<String> = self.peers.iter().map(|e| e.key().clone()).collect();
        for id in peer_ids {
            self.kick_device(&id, "server_shutdown").await;
        }
        loop {
            let notified = self.tasks_drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.in_flight_tasks.load(Ordering::Acquire) == 0 {
                break;
            }
            let remaining: Vec<String> = self.peers.iter().map(|e| e.key().clone()).collect();
            for id in remaining {
                self.kick_device(&id, "server_shutdown").await;
            }
            notified.await;
        }
    }

    fn broadcast_user_device_status(&self, status: DeviceStatusBroadcast<'_>) {
        let Some(uid) = status.owner_user_id else {
            return;
        };
        let targets: Vec<(String, mpsc::Sender<SignalMsg>)> = self
            .peers
            .iter()
            .filter(|entry| {
                entry.key() != status.device_id && entry.value().owner_user_id == Some(uid)
            })
            .map(|entry| (entry.key().clone(), entry.value().tx.clone()))
            .collect();
        if targets.is_empty() {
            return;
        }
        let msg = SignalMsg::DeviceStatusChanged {
            device_id: status.device_id.to_string(),
            online: status.online,
            hostname: status.hostname.to_string(),
            alias: status.alias.to_string(),
            os: status.os.to_string(),
            allow_remote_assist: status.allow_remote_assist,
        };
        for (peer_id, peer_tx) in targets {
            Self::send_nonblocking(&peer_tx, &peer_id, msg.clone());
        }
    }

    /// Kick an online device by signalling its per-connection cancellation handle,
    /// removing it from active peers, and clearing its online status from `EphemeralStore`.
    pub async fn kick_device(&self, device_id: &str, reason: &str) -> bool {
        let norm = normalize_device_id(device_id);
        let removed = {
            let _guard = self.lock_device(&norm).await;
            let removed = self.peers.remove(&norm);
            if let Some((_, ref peer)) = removed {
                peer.cancel.kick(reason);
            }
            if let Err(err) = self.ephemeral.remove_online(&norm).await {
                warn!(device_id = %norm, error = %err, "failed to remove kicked device from ephemeral store");
            }
            removed
        };
        if let Some((_, peer)) = removed {
            self.broadcast_user_device_status(DeviceStatusBroadcast {
                owner_user_id: peer.owner_user_id,
                device_id: &norm,
                online: false,
                hostname: &peer.hostname,
                alias: &peer.alias,
                os: &peer.os,
                allow_remote_assist: peer.allow_remote_assist,
            });
            true
        } else {
            false
        }
    }

    /// Kick all currently online signaling connections belonging to `user_id`.
    pub async fn kick_user_devices(&self, user_id: Uuid, reason: &str) -> Vec<String> {
        let mut targets: Vec<String> = self
            .peers
            .iter()
            .filter(|entry| entry.value().owner_user_id == Some(user_id))
            .map(|entry| entry.key().clone())
            .collect();
        if let Some(ref auth_store) = self.auth_store {
            if let Ok(devs) = auth_store.list_user_devices(user_id).await {
                for dev in devs {
                    if !targets.contains(&dev.device_id) {
                        targets.push(dev.device_id);
                    }
                }
            }
        }
        let mut kicked = Vec::new();
        for did in targets {
            if self.kick_device(&did, reason).await {
                kicked.push(did);
            }
        }
        kicked
    }

    /// Serve UDP STUN-lite probes (`RCSP` -> `RCSA`) on a bound `UdpSocket` until `shutdown_all()`.
    pub async fn serve_udp_probe_on_socket(
        self: Arc<Self>,
        socket: Arc<UdpSocket>,
    ) -> std::io::Result<()> {
        self.serve_udp_probe_on_socket_inner(socket, None).await
    }

    /// Serve UDP STUN-lite probes on a bound `UdpSocket`, exiting cleanly on either
    /// external `shutdown` signal or `shutdown_all()`.
    pub async fn serve_udp_probe_on_socket_with_shutdown(
        self: Arc<Self>,
        socket: Arc<UdpSocket>,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        self.serve_udp_probe_on_socket_inner(socket, Some(shutdown))
            .await
    }

    async fn serve_udp_probe_on_socket_inner(
        self: Arc<Self>,
        socket: Arc<UdpSocket>,
        mut shutdown: Option<watch::Receiver<bool>>,
    ) -> std::io::Result<()> {
        let server_shutdown = self.shutdown.cancelled();
        tokio::pin!(server_shutdown);
        let external_shutdown = wait_external_shutdown(&mut shutdown);
        tokio::pin!(external_shutdown);

        // Bounded background queue for `set_public_udp` updates so Redis/backend latency
        // never head-of-line blocks UDP `RCSA` responses.
        let (udp_update_tx, mut udp_update_rx) =
            mpsc::channel::<(String, SocketAddr)>(UDP_EPHEMERAL_QUEUE_CAPACITY);
        let worker_hub = Arc::clone(&self);
        let worker_guard = SignalTaskGuard::enter(Arc::clone(&self));
        let worker_handle = tokio::spawn(async move {
            let _guard = worker_guard;
            let worker_shutdown = worker_hub.shutdown.cancelled();
            tokio::pin!(worker_shutdown);
            loop {
                let next = tokio::select! {
                    biased;
                    _ = &mut worker_shutdown => break,
                    item = udp_update_rx.recv() => match item {
                        Some(pair) => pair,
                        None => break,
                    },
                };
                let (device_id, canonical_src) = next;
                match tokio::time::timeout(
                    UDP_EPHEMERAL_UPDATE_TIMEOUT,
                    worker_hub.ephemeral.set_public_udp(
                        &device_id,
                        canonical_src,
                        worker_hub.config.udp_ttl,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        warn!(
                            %device_id,
                            %canonical_src,
                            error = %err,
                            "failed to store public UDP endpoint"
                        );
                    }
                    Err(_) => {
                        warn!(
                            %device_id,
                            %canonical_src,
                            "timed out storing public UDP endpoint"
                        );
                    }
                }
            }
        });

        let mut buf = [0u8; 256];
        loop {
            if self.shutdown.is_cancelled() {
                break;
            }
            let received = tokio::select! {
                biased;
                _ = &mut server_shutdown => break,
                _ = &mut external_shutdown => break,
                res = socket.recv_from(&mut buf) => res,
            };

            let (n, src_addr) = match received {
                Ok(pair) => pair,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::Interrupted
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    debug!(error = %err, "transient UDP recv_from error; continuing");
                    continue;
                }
                Err(err) => {
                    warn!(error = %err, "UDP recv_from error; backing off 50ms");
                    tokio::select! {
                        biased;
                        _ = &mut server_shutdown => break,
                        _ = &mut external_shutdown => break,
                        _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                    }
                    continue;
                }
            };

            let canonical_src = canonical_socket_addr(src_addr);

            // Per-source-IP UDP rate limit mitigates reflection/amplification & floods.
            if !check_sliding_window_rate_limit(
                &self.udp_rate_limits,
                canonical_src.ip(),
                self.config.udp_rate_limit_max,
                self.config.udp_rate_limit_window,
            ) {
                debug!(%canonical_src, "dropping rate-limited UDP RCSP probe");
                continue;
            }

            let req = match UdpProbeRequest::decode(&buf[..n]) {
                Ok(req) => req,
                Err(err) => {
                    debug!(%canonical_src, bytes = n, error = %err, "dropping malformed UDP RCSP packet");
                    continue;
                }
            };

            if req.device_id != ANONYMOUS_DEVICE_ID && !is_valid_device_id(&req.device_id) {
                debug!(
                    %canonical_src,
                    device_id = %req.device_id,
                    "dropping UDP RCSP packet with invalid device_id"
                );
                continue;
            }

            // Reply `RCSA` BEFORE updating ephemeral storage so backend latency never
            // delays STUN-lite echo. Send back to original `src_addr` for dual-stack sockets.
            let resp = UdpProbeResponse {
                observed_addr: canonical_src,
                nonce: req.nonce,
            }
            .encode();

            if let Err(err) = socket.send_to(&resp, src_addr).await {
                debug!(%src_addr, error = %err, "failed to send UDP RCSA reply");
            }

            // Only persist public UDP endpoint if the device is currently online over
            // authenticated TCP signaling AND the canonical source IP matches!
            if req.device_id != ANONYMOUS_DEVICE_ID {
                let authorized = self
                    .peers
                    .get(&req.device_id)
                    .is_some_and(|peer| canonical_ip(peer.remote_addr.ip()) == canonical_src.ip());
                if authorized {
                    if let Err(err) = udp_update_tx.try_send((req.device_id, canonical_src)) {
                        debug!(%canonical_src, error = %err, "dropping public UDP update because background queue is full/closed");
                    }
                } else {
                    debug!(
                        device_id = %req.device_id,
                        %canonical_src,
                        "ignoring UDP endpoint update for offline device or mismatched IP"
                    );
                }
            }
        }

        drop(udp_update_tx);
        let _ = tokio::time::timeout(
            UDP_EPHEMERAL_UPDATE_TIMEOUT + Duration::from_millis(100),
            worker_handle,
        )
        .await;
        Ok(())
    }

    /// Bind a UDP socket on `addr` and serve UDP STUN-lite probes.
    pub async fn serve_udp_probe(self: Arc<Self>, addr: SocketAddr) -> std::io::Result<()> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        info!(%addr, "UDP STUN-lite probe listener started");
        self.serve_udp_probe_on_socket(socket).await
    }

    /// Serve native TCP framed signaling connections on a bound `TcpListener` until `shutdown_all()`.
    pub async fn serve_tcp_signaling_on_listener(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> std::io::Result<()> {
        self.serve_signaling_on_listener_inner(listener, None, None)
            .await
    }

    /// Serve native TCP framed signaling connections on a bound `TcpListener`, exiting cleanly
    /// on either external `shutdown` signal or `shutdown_all()`.
    pub async fn serve_tcp_signaling_on_listener_with_shutdown(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        self.serve_signaling_on_listener_inner(listener, None, Some(shutdown))
            .await
    }

    /// Serve TLS 1.3 framed signaling connections on a bound `TcpListener` until `shutdown_all()`.
    pub async fn serve_tls_signaling_on_listener(
        self: Arc<Self>,
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<()> {
        self.serve_signaling_on_listener_inner(listener, Some(TlsAcceptor::from(tls_config)), None)
            .await
    }

    /// Serve TLS 1.3 framed signaling connections on a bound `TcpListener`, exiting cleanly
    /// on either external `shutdown` signal or `shutdown_all()`.
    pub async fn serve_tls_signaling_on_listener_with_shutdown(
        self: Arc<Self>,
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
        shutdown: watch::Receiver<bool>,
    ) -> std::io::Result<()> {
        self.serve_signaling_on_listener_inner(
            listener,
            Some(TlsAcceptor::from(tls_config)),
            Some(shutdown),
        )
        .await
    }

    async fn serve_signaling_on_listener_inner(
        self: Arc<Self>,
        listener: TcpListener,
        tls_acceptor: Option<TlsAcceptor>,
        mut shutdown: Option<watch::Receiver<bool>>,
    ) -> std::io::Result<()> {
        let server_shutdown = self.shutdown.cancelled();
        tokio::pin!(server_shutdown);
        let external_shutdown = wait_external_shutdown(&mut shutdown);
        tokio::pin!(external_shutdown);

        loop {
            if self.shutdown.is_cancelled() {
                return Ok(());
            }

            // Acquire a connection permit before accept() to bound concurrent TCP connections.
            let permit = tokio::select! {
                biased;
                _ = &mut server_shutdown => return Ok(()),
                _ = &mut external_shutdown => return Ok(()),
                res = self.conn_semaphore.clone().acquire_owned() => match res {
                    Ok(permit) => permit,
                    Err(_) => return Ok(()),
                },
            };

            let accepted = tokio::select! {
                biased;
                _ = &mut server_shutdown => return Ok(()),
                _ = &mut external_shutdown => return Ok(()),
                res = listener.accept() => res,
            };

            let (stream, peer_addr) = match accepted {
                Ok(pair) => pair,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    drop(permit);
                    continue;
                }
                Err(err) => {
                    drop(permit);
                    warn!(error = %err, "TCP signaling accept error; backing off 50ms");
                    tokio::select! {
                        biased;
                        _ = &mut server_shutdown => return Ok(()),
                        _ = &mut external_shutdown => return Ok(()),
                        _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                    }
                    continue;
                }
            };

            let hub = Arc::clone(&self);
            let task_guard = SignalTaskGuard::enter(Arc::clone(&self));
            let tls_acceptor = tls_acceptor.clone();
            tokio::spawn(async move {
                let _task_guard = task_guard;
                if let Err(err) = stream.set_nodelay(true) {
                    debug!(%peer_addr, error = %err, "failed to set TCP_NODELAY on signaling socket");
                }
                if let Some(acceptor) = tls_acceptor {
                    let tls_res = tokio::select! {
                        biased;
                        _ = hub.shutdown.cancelled() => return,
                        res = tokio::time::timeout(hub.config.handshake_timeout, acceptor.accept(stream)) => res,
                    };
                    match tls_res {
                        Ok(Ok(tls_stream)) => {
                            hub.handle_stream_connection_inner(tls_stream, peer_addr, Some(permit))
                                .await;
                        }
                        Ok(Err(err)) => {
                            debug!(%peer_addr, error = %err, "signaling TLS handshake failed");
                        }
                        Err(_) => {
                            debug!(%peer_addr, "signaling TLS handshake timed out");
                        }
                    }
                } else {
                    hub.handle_stream_connection_inner(stream, peer_addr, Some(permit))
                        .await;
                }
            });
        }
    }

    /// Bind a TCP listener on `addr` and serve native TCP framed signaling (NO WebSocket).
    pub async fn serve_tcp_signaling(self: Arc<Self>, addr: SocketAddr) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "native TCP framed signaling listener started");
        self.serve_tcp_signaling_on_listener(listener).await
    }

    pub async fn handle_tcp_connection(
        self: Arc<Self>,
        stream: TcpStream,
        remote_addr: SocketAddr,
    ) {
        let _task_guard = SignalTaskGuard::enter(Arc::clone(&self));
        if let Err(err) = stream.set_nodelay(true) {
            debug!(%remote_addr, error = %err, "failed to set TCP_NODELAY on signaling socket");
        }
        self.handle_stream_connection_inner(stream, remote_addr, None)
            .await;
    }

    async fn authenticate_signal_token(
        &self,
        access_token: Option<&str>,
    ) -> Result<Option<Uuid>, (&'static str, String)> {
        let token = access_token.map(str::trim).filter(|s| !s.is_empty());
        let Some(token) = token else {
            if self.config.require_auth {
                return Err(("unauthorized", "access_token is required".into()));
            }
            return Ok(None);
        };

        let (Some(auth_authority), Some(auth_store)) =
            (self.auth_authority.as_ref(), self.auth_store.as_ref())
        else {
            if self.config.require_auth {
                return Err((
                    "server_misconfigured",
                    "signaling auth authority is not configured".into(),
                ));
            }
            return Ok(None);
        };

        let claims = match auth_authority.verify_access_token(token) {
            Ok(c) => c,
            Err(AuthError::TokenExpired) => {
                return Err(("token_expired", "access token expired".into()));
            }
            Err(_) => {
                return Err(("unauthorized", "invalid access token".into()));
            }
        };

        if claims.pwd_change_only {
            return Err((
                "password_change_required",
                "password change is required before connecting signaling".into(),
            ));
        }

        let user = match auth_store.get_user_by_id(claims.sub).await {
            Ok(Some(u)) => u,
            Ok(None) => return Err(("unauthorized", "user not found".into())),
            Err(err) => return Err(("register_failed", err.to_string())),
        };

        if user.status != STATUS_ACTIVE {
            return Err(("account_disabled", "user account is disabled".into()));
        }
        if user.token_version != claims.ver {
            return Err(("token_revoked", "access token has been revoked".into()));
        }
        if user.must_change_password {
            return Err((
                "password_change_required",
                "password change is required before connecting signaling".into(),
            ));
        }
        match auth_store.get_user_session(claims.sid).await {
            Ok(Some(sess)) => {
                if sess.revoked_at.is_some() {
                    return Err(("token_revoked", "session has been revoked".into()));
                }
            }
            Ok(None) => {
                return Err(("token_revoked", "session not found or expired".into()));
            }
            Err(err) => {
                return Err(("service_unavailable", err.to_string()));
            }
        }

        Ok(Some(user.id))
    }

    async fn handle_stream_connection_inner<S>(
        self: Arc<Self>,
        stream: S,
        remote_addr: SocketAddr,
        _permit: Option<OwnedSemaphorePermit>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let remote_addr = canonical_socket_addr(remote_addr);

        if self.shutdown.is_cancelled() {
            return;
        }

        let (mut reader, mut writer) = tokio::io::split(stream);

        // Step 1: First frame MUST be `register` within handshake_timeout.
        let first_frame = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => return,
            res = tokio::time::timeout(
                self.config.handshake_timeout,
                read_signal_frame(&mut reader),
            ) => match res {
                Ok(Ok(Some(msg))) => msg,
                Ok(Ok(None)) => return,
                Ok(Err(err)) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "invalid_frame".into(),
                            message: err.to_string(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
                Err(_) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "handshake_timeout".into(),
                            message: "register frame timed out".into(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
            },
        };

        let (
            claimed_id,
            register_input,
            initial_lan_addrs,
            access_token,
            device_pubkey,
            allow_remote_assist_opt,
        ) = match first_frame {
            SignalMsg::Register {
                device_id,
                device_key,
                hostname,
                os,
                device_group,
                lan_addrs,
                access_token,
                device_pubkey,
                allow_remote_assist,
            } => {
                if let Err(reason) =
                    validate_register_fields(&hostname, &os, &device_group, &lan_addrs)
                {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "invalid_payload".into(),
                            message: reason,
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
                if let Some(ref pk) = device_pubkey {
                    if let Err(reason) =
                        validate_metadata_field(pk, MAX_DEVICE_PUBKEY_LEN, "device_pubkey")
                    {
                        write_frame_with_timeout(
                            &mut writer,
                            &SignalMsg::Error {
                                code: "invalid_payload".into(),
                                message: reason,
                            },
                            self.config.write_timeout,
                        )
                        .await;
                        return;
                    }
                }
                (
                    device_id,
                    RegisterInput {
                        device_key,
                        hostname,
                        os,
                        device_group,
                    },
                    lan_addrs,
                    access_token,
                    device_pubkey,
                    allow_remote_assist,
                )
            }
            _ => {
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: "expected_register".into(),
                        message: "first signaling frame must be register".into(),
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }
        };

        let authed_user_id = match self
            .authenticate_signal_token(access_token.as_deref())
            .await
        {
            Ok(uid) => uid,
            Err((code, message)) => {
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: code.into(),
                        message,
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }
        };

        // Step 2: If the client claims an existing `device_id`, verify ownership against
        // `device_key` BEFORE calling `register_or_get` so mismatched or unknown keys cannot
        // insert orphan device rows or mutate another device's metadata.
        let claimed = claimed_id
            .as_deref()
            .map(normalize_device_id)
            .filter(|id| !id.is_empty());

        if let Some(ref claimed) = claimed {
            match self.device_store.get_by_id(claimed).await {
                Ok(Some(existing)) if existing.device_key == register_input.device_key.trim() => {
                    if existing.disabled {
                        write_frame_with_timeout(
                            &mut writer,
                            &SignalMsg::Error {
                                code: "device_disabled".into(),
                                message: format!("device {} is disabled", existing.device_id),
                            },
                            self.config.write_timeout,
                        )
                        .await;
                        return;
                    }
                }
                Ok(_) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "device_id_mismatch".into(),
                            message: "provided device_id does not match device_key".into(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "register_failed".into(),
                            message: err.to_string(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
            }
        }

        let record = match self.device_store.register_or_get(register_input).await {
            Ok(rec) => rec,
            Err(StoreError::DeviceDisabled(id)) => {
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: "device_disabled".into(),
                        message: format!("device {id} is disabled"),
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }
            Err(err) => {
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: "register_failed".into(),
                        message: err.to_string(),
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }
        };

        if let Some(ref claimed) = claimed {
            if claimed != &record.device_id {
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: "device_id_mismatch".into(),
                        message: "provided device_id does not match device_key".into(),
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }
        }

        let device_id = record.device_id.clone();

        if let Some(uid) = authed_user_id {
            let clean_pubkey = device_pubkey
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let prev_owner = match self
                .device_store
                .bind_device_to_user_with_prev(&device_id, uid, None, clean_pubkey)
                .await
            {
                Ok(Some((_, prev_owner))) => prev_owner,
                Ok(None) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "register_failed".into(),
                            message: format!("device {device_id} disappeared during bind"),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "register_failed".into(),
                            message: err.to_string(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
            };
            if let Some(prev_uid) = prev_owner {
                if prev_uid != uid {
                    if let Some(ref auth_store) = self.auth_store {
                        if let Ok(certs) = auth_store
                            .list_device_certs(Some(prev_uid), Some(&device_id))
                            .await
                        {
                            for cert in certs {
                                if cert.revoked_at.is_none() {
                                    if let Ok(Some(revoked)) = auth_store
                                        .revoke_cert_by_serial(
                                            &cert.serial_hex,
                                            Some("device_rebound"),
                                        )
                                        .await
                                    {
                                        let rem = (revoked.expires_at - Utc::now())
                                            .to_std()
                                            .unwrap_or_else(|_| Duration::from_secs(1));
                                        let _ = self
                                            .ephemeral
                                            .revoke_cert_serial(&revoked.serial_hex, rem)
                                            .await;
                                    }
                                }
                            }
                        }
                        let _ = auth_store
                            .revoke_device_sessions(prev_uid, &device_id)
                            .await;
                    }
                }
            }
            if let Some(allow) = allow_remote_assist_opt {
                let _ = self
                    .device_store
                    .set_device_allow_remote_assist(&device_id, Some(uid), allow)
                    .await;
            }
        } else if let Some(allow) = allow_remote_assist_opt {
            let _ = self
                .device_store
                .set_device_allow_remote_assist(&device_id, None, allow)
                .await;
        }

        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::channel::<SignalMsg>(self.config.channel_capacity);
        let cancel = PeerCancel::new();

        // Step 3 (M4): Serialize disabled re-check + peers.insert + ephemeral.set_online
        // under the per-device stripe lock so concurrent admin disable/kick or reconnect
        // transitions can never interleave inconsistently.
        let (peer_owner, peer_hostname, peer_alias, peer_os, peer_assist) = {
            let _guard = self.lock_device(&device_id).await;
            if self.shutdown.is_cancelled() {
                return;
            }
            if authed_user_id.is_some() {
                if let Err((code, message)) = self
                    .authenticate_signal_token(access_token.as_deref())
                    .await
                {
                    drop(_guard);
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: code.into(),
                            message,
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
            }
            let latest = match self.device_store.get_by_id(&device_id).await {
                Ok(Some(latest)) if !latest.disabled => latest,
                Ok(_) => {
                    drop(_guard);
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "device_disabled".into(),
                            message: format!("device {device_id} is disabled"),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    drop(_guard);
                    write_frame_with_timeout(
                        &mut writer,
                        &SignalMsg::Error {
                            code: "register_failed".into(),
                            message: err.to_string(),
                        },
                        self.config.write_timeout,
                    )
                    .await;
                    return;
                }
            };
            if authed_user_id.is_some() && latest.owner_user_id != authed_user_id {
                drop(_guard);
                write_frame_with_timeout(
                    &mut writer,
                    &SignalMsg::Error {
                        code: "device_unbound".into(),
                        message: format!("device {device_id} ownership changed"),
                    },
                    self.config.write_timeout,
                )
                .await;
                return;
            }

            let effective_owner = authed_user_id;
            let effective_pubkey = if !latest.device_pubkey.is_empty() {
                latest.device_pubkey.clone()
            } else {
                device_pubkey.unwrap_or_default()
            };
            let effective_assist = allow_remote_assist_opt.unwrap_or(latest.allow_remote_assist);

            if let Some(old_peer) = self.peers.insert(
                device_id.clone(),
                ActivePeer {
                    conn_id,
                    tx: tx.clone(),
                    cancel: cancel.clone(),
                    lan_addrs: initial_lan_addrs,
                    remote_addr,
                    connected_at: Utc::now(),
                    owner_user_id: effective_owner,
                    device_pubkey: effective_pubkey,
                    allow_remote_assist: effective_assist,
                    hostname: latest.hostname.clone(),
                    alias: latest.alias.clone(),
                    os: latest.os.clone(),
                },
            ) {
                old_peer.cancel.kick("replaced_by_new_connection");
            }

            if let Err(err) = self
                .ephemeral
                .set_online(&device_id, self.config.online_ttl)
                .await
            {
                warn!(%device_id, error = %err, "failed to set device online in ephemeral store");
            }

            // Enqueue `registered` response while still serialized before spawning writer.
            let _ = tx.try_send(SignalMsg::Registered {
                device_id: device_id.clone(),
                formatted_id: format_device_id(&device_id),
                server_time: Utc::now(),
                udp_probe_addr: self.config.public_udp_probe_addr.clone(),
                relay_addr: self.config.public_relay_addr.clone(),
            });

            (
                effective_owner,
                latest.hostname,
                latest.alias,
                latest.os,
                effective_assist,
            )
        };

        self.broadcast_user_device_status(DeviceStatusBroadcast {
            owner_user_id: peer_owner,
            device_id: &device_id,
            online: true,
            hostname: &peer_hostname,
            alias: &peer_alias,
            os: &peer_os,
            allow_remote_assist: peer_assist,
        });

        // Writer task drains bounded channel with per-frame write timeout and immediate kick wakeup.
        let (writer_done_tx, mut writer_done_rx) = oneshot::channel::<()>();
        let mut writer_cancel_rx = cancel.subscribe();
        let write_timeout = self.config.write_timeout;
        let writer_device_id = device_id.clone();
        let mut writer_handle = tokio::spawn(async move {
            let mut kicked_reason: Option<String> = None;

            loop {
                let next_msg = tokio::select! {
                    biased;
                    reason = wait_kicked(&mut writer_cancel_rx) => {
                        kicked_reason = Some(reason);
                        break;
                    }
                    msg = rx.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                };

                let is_kicked = matches!(next_msg, SignalMsg::Kicked { .. });

                // H5: Encode before writing and skip unencodable/oversized frames instead of
                // tearing down the victim peer's connection.
                let encoded = match encode_signal_frame(&next_msg) {
                    Ok(frame) => frame,
                    Err(err @ (SignalError::FrameTooLarge(_) | SignalError::Json(_))) => {
                        warn!(
                            device_id = %writer_device_id,
                            error = %err,
                            "skipping oversized or unencodable outbound signaling frame"
                        );
                        continue;
                    }
                    Err(err) => {
                        warn!(
                            device_id = %writer_device_id,
                            error = %err,
                            "skipping invalid outbound signaling frame"
                        );
                        continue;
                    }
                };

                let write_outcome = tokio::select! {
                    biased;
                    _ = wait_kicked(&mut writer_cancel_rx) => {
                        // Cancelled while blocked mid-write on a stalled socket; abort immediately.
                        break;
                    }
                    res = tokio::time::timeout(write_timeout, async {
                        writer.write_all(&encoded).await?;
                        writer.flush().await
                    }) => res,
                };

                match write_outcome {
                    Ok(Ok(())) => {
                        if is_kicked {
                            break;
                        }
                    }
                    Ok(Err(_)) | Err(_) => {
                        break;
                    }
                }
            }

            if let Some(reason) = kicked_reason {
                write_frame_with_timeout(&mut writer, &SignalMsg::Kicked { reason }, write_timeout)
                    .await;
            }

            let _ = writer.shutdown().await;
            let _ = writer_done_tx.send(());
        });

        // Reader loop processes incoming framed messages until disconnect, kick, shutdown, or idle timeout.
        let mut reader_cancel_rx = cancel.subscribe();
        let mut abort_writer_immediately = false;
        loop {
            if cancel.is_kicked() || self.shutdown.is_cancelled() {
                break;
            }
            let next = tokio::select! {
                biased;
                _ = wait_kicked(&mut reader_cancel_rx) => {
                    break;
                }
                _ = self.shutdown.cancelled() => {
                    cancel.kick("server_shutdown");
                    break;
                }
                _ = &mut writer_done_rx => {
                    break;
                }
                res = tokio::time::timeout(
                    self.config.read_idle_timeout,
                    read_signal_frame(&mut reader),
                ) => res,
            };

            match next {
                Ok(Ok(Some(msg))) => {
                    if !self.is_active_conn(&device_id, conn_id, &cancel) {
                        break;
                    }
                    self.handle_peer_message(&device_id, conn_id, &cancel, &tx, msg)
                        .await;
                }
                Ok(Ok(None)) => break,
                Ok(Err(err)) => {
                    debug!(%device_id, %remote_addr, error = %err, "signaling read error");
                    let _ = tx.try_send(SignalMsg::Error {
                        code: "invalid_frame".into(),
                        message: err.to_string(),
                    });
                    break;
                }
                Err(_) => {
                    debug!(%device_id, %remote_addr, "signaling peer idle timeout");
                    abort_writer_immediately = true;
                    break;
                }
            }
        }

        // Cleanup (M4): serialized under per-device stripe lock so we only clear ephemeral
        // online status when this connection is still the active one.
        let removed_peer = {
            let _guard = self.lock_device(&device_id).await;
            let removed = self
                .peers
                .remove_if(&device_id, |_, peer| peer.conn_id == conn_id)
                .map(|(_, p)| p);
            if removed.is_some() && !self.peers.contains_key(&device_id) {
                if let Err(err) = self.ephemeral.remove_online(&device_id).await {
                    warn!(%device_id, error = %err, "failed to remove offline device from ephemeral store");
                }
            }
            removed
        };

        if let Some(peer) = removed_peer {
            self.broadcast_user_device_status(DeviceStatusBroadcast {
                owner_user_id: peer.owner_user_id,
                device_id: &device_id,
                online: false,
                hostname: &peer.hostname,
                alias: &peer.alias,
                os: &peer.os,
                allow_remote_assist: peer.allow_remote_assist,
            });
        }

        drop(tx);
        if abort_writer_immediately
            || tokio::time::timeout(
                self.config.write_timeout + Duration::from_millis(250),
                &mut writer_handle,
            )
            .await
            .is_err()
        {
            writer_handle.abort();
            let _ = writer_handle.await;
        }
    }

    async fn refresh_online_under_lock(
        &self,
        device_id: &str,
        conn_id: u64,
        cancel: &PeerCancel,
        new_lan_addrs: Option<Vec<String>>,
    ) -> bool {
        let _guard = self.lock_device(device_id).await;
        if !self.is_active_conn(device_id, conn_id, cancel) {
            return false;
        }
        if let Some(addrs) = new_lan_addrs {
            if let Some(mut peer) = self.peers.get_mut(device_id) {
                if peer.conn_id == conn_id {
                    peer.lan_addrs = addrs;
                }
            }
        }
        if let Err(err) = self
            .ephemeral
            .set_online(device_id, self.config.online_ttl)
            .await
        {
            warn!(%device_id, error = %err, "failed to refresh ephemeral online TTL");
        }
        true
    }

    async fn allocate_relay_token(
        &self,
        token: &str,
        record: RelayTokenRecord,
    ) -> Result<(), &'static str> {
        let max_tokens = self.config.max_relay_tokens;
        if max_tokens == 0 {
            return Err("relay_capacity_exceeded");
        }
        let use_local_tracker = match self.ephemeral.active_relay_token_count().await {
            Ok(Some(active)) => {
                if active >= max_tokens {
                    return Err("relay_capacity_exceeded");
                }
                false
            }
            Ok(None) | Err(_) => {
                // Fallback local cap tracking when backend does not cheaply report count (e.g. Redis).
                let now = Instant::now();
                if self.issued_relay_tokens.len() >= max_tokens {
                    self.issued_relay_tokens.retain(|_, exp| *exp > now);
                    if self.issued_relay_tokens.len() >= max_tokens {
                        return Err("relay_capacity_exceeded");
                    }
                }
                true
            }
        };

        match self
            .ephemeral
            .put_relay_token(token, record, self.config.relay_token_ttl)
            .await
        {
            Ok(()) => {
                if use_local_tracker {
                    if let Some(exp) = Instant::now().checked_add(self.config.relay_token_ttl) {
                        self.issued_relay_tokens.insert(token.to_string(), exp);
                    }
                }
                Ok(())
            }
            Err(EphemeralError::RelayTokenCapacityExceeded(_)) => Err("relay_capacity_exceeded"),
            Err(err) => {
                warn!(error = %err, "failed to store relay token in ephemeral store");
                Err("relay_token_error")
            }
        }
    }

    async fn cleanup_relay_token(&self, token: &str) {
        self.issued_relay_tokens.remove(token);
        if let Err(err) = self.ephemeral.take_relay_token(token).await {
            warn!(error = %err, "failed to clean up undelivered relay token");
        }
    }

    async fn authorize_peer_connection(
        &self,
        caller_device_id: &str,
        target_device_id: &str,
        session_id: &str,
    ) -> Result<(Option<OwnerGrantTicket>, bool), (&'static str, String)> {
        let caller_peer = self
            .peers
            .get(caller_device_id)
            .map(|p| (p.owner_user_id, p.device_pubkey.clone()));
        let caller_in_peers = caller_peer.is_some();
        let (peer_owner_uid, peer_pubkey) = caller_peer.unwrap_or((None, String::new()));

        let (caller_uid, caller_pubkey) = match self.device_store.get_by_id(caller_device_id).await
        {
            Ok(Some(rec)) => {
                if rec.disabled {
                    return Err(("peer_disabled", "当前设备已被禁用".to_string()));
                }
                if caller_in_peers && peer_owner_uid != rec.owner_user_id {
                    return Err(("unauthorized", "设备归属已变更，请重新登录认证".to_string()));
                }
                let pk = if !rec.device_pubkey.trim().is_empty() {
                    rec.device_pubkey
                } else {
                    peer_pubkey
                };
                (rec.owner_user_id, pk)
            }
            Ok(None)
                if caller_in_peers && !self.config.require_auth && peer_owner_uid.is_none() =>
            {
                (None, peer_pubkey)
            }
            Ok(None) => return Err(("unauthorized", "当前设备未注册或已失效".to_string())),
            Err(err) => {
                warn!(caller = %caller_device_id, error = %err, "failed to load caller device for authorization");
                return Err(("internal_error", "校验当前设备状态失败".to_string()));
            }
        };
        let in_peers = self.peers.contains_key(target_device_id);
        let (target_uid, target_allow_assist) = match self
            .device_store
            .get_by_id(target_device_id)
            .await
        {
            Ok(Some(rec)) => {
                if rec.disabled {
                    return Err(("peer_disabled", "目标设备已被禁用".to_string()));
                }
                (rec.owner_user_id, rec.allow_remote_assist)
            }
            Ok(None) if in_peers && !self.config.require_auth && caller_uid.is_none() => {
                (None, false)
            }
            Ok(None) => return Err(("peer_not_found", "目标设备不存在".to_string())),
            Err(err) => {
                warn!(target = %target_device_id, error = %err, "failed to load target device for authorization");
                return Err(("internal_error", "校验目标设备状态失败".to_string()));
            }
        };

        if let (Some(c_uid), Some(t_uid)) = (caller_uid, target_uid) {
            if c_uid == t_uid {
                let mut owner_grant = None;
                if let Some(ref auth_authority) = self.auth_authority {
                    let trimmed = caller_pubkey.trim();
                    let pk_hex = trimmed.strip_prefix("ed25519:").unwrap_or(trimmed);
                    if pk_hex.len() == 64 && pk_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                        match auth_authority.issue_owner_grant(
                            c_uid,
                            caller_device_id,
                            pk_hex,
                            target_device_id,
                            session_id,
                            OWNER_GRANT_DEFAULT_TTL,
                        ) {
                            Ok(ticket) => owner_grant = Some(ticket),
                            Err(err) => {
                                warn!(
                                    caller = %caller_device_id,
                                    target = %target_device_id,
                                    error = %err,
                                    "failed to issue owner_grant ticket"
                                );
                            }
                        }
                    }
                }
                return Ok((owner_grant, true));
            }
        }

        if (self.config.require_auth || caller_uid.is_some() || target_uid.is_some())
            && !target_allow_assist
        {
            return Err(("assist_disabled", "对方未开启允许他人远程协助".to_string()));
        }

        Ok((None, false))
    }

    async fn handle_peer_message(
        &self,
        device_id: &str,
        conn_id: u64,
        cancel: &PeerCancel,
        self_tx: &mpsc::Sender<SignalMsg>,
        msg: SignalMsg,
    ) {
        if !self.is_active_conn(device_id, conn_id, cancel) {
            return;
        }

        match msg {
            SignalMsg::Heartbeat { lan_addrs } => {
                if let Err(reason) = validate_lan_addrs(&lan_addrs) {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "invalid_payload".into(),
                            message: reason,
                        },
                    );
                    return;
                }
                let updated_addrs = if lan_addrs.is_empty() {
                    None
                } else {
                    Some(lan_addrs)
                };
                if !self
                    .refresh_online_under_lock(device_id, conn_id, cancel, updated_addrs)
                    .await
                {
                    return;
                }
                if let Err(err) = self.device_store.touch_last_seen(device_id).await {
                    warn!(%device_id, error = %err, "heartbeat failed to touch last_seen_at");
                }
                let public_udp_addr = self
                    .ephemeral
                    .get_public_udp(device_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.to_string());

                if !self.is_active_conn(device_id, conn_id, cancel) {
                    return;
                }
                Self::send_nonblocking(
                    self_tx,
                    device_id,
                    SignalMsg::HeartbeatAck {
                        server_time: Utc::now(),
                        public_udp_addr,
                        online_peers: self.peers.len(),
                    },
                );
            }

            SignalMsg::UpdateAssistState {
                allow_remote_assist,
            } => {
                let updated_peer = {
                    let _guard = self.lock_device(device_id).await;
                    if !self.is_active_conn(device_id, conn_id, cancel) {
                        return;
                    }
                    let owner = self.peers.get(device_id).and_then(|p| p.owner_user_id);
                    if let Err(err) = self
                        .device_store
                        .set_device_allow_remote_assist(device_id, owner, allow_remote_assist)
                        .await
                    {
                        warn!(%device_id, error = %err, "failed to persist allow_remote_assist");
                    }
                    self.peers.get_mut(device_id).and_then(|mut peer| {
                        if peer.conn_id == conn_id {
                            peer.allow_remote_assist = allow_remote_assist;
                            Some((
                                peer.owner_user_id,
                                peer.hostname.clone(),
                                peer.alias.clone(),
                                peer.os.clone(),
                            ))
                        } else {
                            None
                        }
                    })
                };

                if let Some((owner, hostname, alias, os)) = updated_peer {
                    self.broadcast_user_device_status(DeviceStatusBroadcast {
                        owner_user_id: owner,
                        device_id,
                        online: true,
                        hostname: &hostname,
                        alias: &alias,
                        os: &os,
                        allow_remote_assist,
                    });
                }
            }

            SignalMsg::ConnectRequest {
                target_id,
                session_id,
                lan_addrs,
                nonce,
            } => {
                let target_norm = normalize_device_id(&target_id);
                if target_norm.is_empty() {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "invalid_payload".into(),
                            message: "target_id is required".into(),
                        },
                    );
                    return;
                }
                if target_norm == device_id {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "self_connect".into(),
                            message: "cannot connect to self".into(),
                        },
                    );
                    return;
                }
                if let Err(reason) = validate_lan_addrs(&lan_addrs) {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "invalid_payload".into(),
                            message: reason,
                        },
                    );
                    return;
                }
                let session_id = match resolve_optional_ascii_token(
                    session_id,
                    MAX_SESSION_ID_LEN,
                    12,
                    "session_id",
                ) {
                    Ok(s) => s,
                    Err(reason) => {
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "invalid_payload".into(),
                                message: reason,
                            },
                        );
                        return;
                    }
                };
                let nonce = match resolve_optional_ascii_token(nonce, MAX_NONCE_LEN, 8, "nonce") {
                    Ok(n) => n,
                    Err(reason) => {
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "invalid_payload".into(),
                                message: reason,
                            },
                        );
                        return;
                    }
                };

                // H6: Per-peer rate limiting on ConnectRequest / RelayAlloc.
                if !check_sliding_window_rate_limit(
                    &self.connect_rate_limits,
                    device_id.to_string(),
                    self.config.connect_rate_limit_max,
                    self.config.connect_rate_limit_window,
                ) {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "rate_limited".into(),
                            message: "too many connect/relay requests".into(),
                        },
                    );
                    return;
                }

                // L5 + M4: Refresh ephemeral online TTL on authenticated peer activity.
                let updated_lan = if lan_addrs.is_empty() {
                    None
                } else {
                    Some(lan_addrs.clone())
                };
                if !self
                    .refresh_online_under_lock(device_id, conn_id, cancel, updated_lan)
                    .await
                {
                    return;
                }

                let client_lan_addrs = if !lan_addrs.is_empty() {
                    lan_addrs
                } else {
                    self.peers
                        .get(device_id)
                        .filter(|p| p.conn_id == conn_id)
                        .map(|p| p.lan_addrs.clone())
                        .unwrap_or_default()
                };

                let target_snapshot = self
                    .peers
                    .get(&target_norm)
                    .map(|p| (p.tx.clone(), p.lan_addrs.clone()));

                let Some((host_tx, host_lan_addrs)) = target_snapshot else {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::PeerOffline {
                            target_id: target_norm,
                            session_id,
                        },
                    );
                    return;
                };

                let (owner_grant, is_owner_connection) = match self
                    .authorize_peer_connection(device_id, &target_norm, &session_id)
                    .await
                {
                    Ok(res) => res,
                    Err((code, message)) => {
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: code.into(),
                                message,
                            },
                        );
                        return;
                    }
                };

                let client_udp = self
                    .ephemeral
                    .get_public_udp(device_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.to_string());
                let host_udp = self
                    .ephemeral
                    .get_public_udp(&target_norm)
                    .await
                    .ok()
                    .flatten()
                    .map(|a| a.to_string());

                if !self.is_active_conn(device_id, conn_id, cancel) {
                    return;
                }

                let relay_token = random_hex(32);
                let client_msg = SignalMsg::PunchCoord {
                    session_id: session_id.clone(),
                    role: "client".into(),
                    peer_id: target_norm.clone(),
                    peer_public_udp_addr: host_udp,
                    peer_lan_addrs: host_lan_addrs,
                    nonce: nonce.clone(),
                    relay_token: relay_token.clone(),
                    relay_addr: self.config.public_relay_addr.clone(),
                    owner_grant,
                    is_owner_connection,
                };
                let host_msg = SignalMsg::PunchCoord {
                    session_id: session_id.clone(),
                    role: "host".into(),
                    peer_id: device_id.to_string(),
                    peer_public_udp_addr: client_udp,
                    peer_lan_addrs: client_lan_addrs,
                    nonce,
                    relay_token: relay_token.clone(),
                    relay_addr: self.config.public_relay_addr.clone(),
                    owner_grant: None,
                    is_owner_connection,
                };

                // H5: Pre-check serialized frame length <= MAX_SIGNAL_FRAME_LEN before
                // allocating token or enqueueing to either peer.
                if encode_signal_frame(&client_msg).is_err()
                    || encode_signal_frame(&host_msg).is_err()
                {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "frame_too_large".into(),
                            message: "punch_coord frame exceeds maximum length".into(),
                        },
                    );
                    return;
                }

                let token_record = RelayTokenRecord {
                    session_id: session_id.clone(),
                    host_id: target_norm.clone(),
                    client_id: device_id.to_string(),
                };
                if let Err(code) = self.allocate_relay_token(&relay_token, token_record).await {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: code.into(),
                            message: "failed to allocate fallback relay token".into(),
                        },
                    );
                    return;
                }

                if !self.is_active_conn(device_id, conn_id, cancel) {
                    self.cleanup_relay_token(&relay_token).await;
                    return;
                }

                // H6: Deliver to host first; if delivery fails, consume/delete the relay token
                // immediately so it never lingers in ephemeral storage.
                match Self::try_enqueue_checked(&host_tx, &target_norm, host_msg) {
                    Ok(()) => {
                        self.punch_coord_count.fetch_add(1, Ordering::Relaxed);
                        Self::send_nonblocking(self_tx, device_id, client_msg);
                    }
                    Err(EnqueueError::Closed) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::PeerOffline {
                                target_id: target_norm,
                                session_id,
                            },
                        );
                    }
                    Err(EnqueueError::Full) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "peer_busy".into(),
                                message: "target peer signaling queue is full".into(),
                            },
                        );
                    }
                    Err(EnqueueError::FrameTooLarge) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "frame_too_large".into(),
                                message: "punch_coord frame exceeds maximum length".into(),
                            },
                        );
                    }
                }
            }

            SignalMsg::RelayAlloc {
                target_id,
                session_id,
            } => {
                let target_norm = normalize_device_id(&target_id);
                if target_norm.is_empty() {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "invalid_payload".into(),
                            message: "target_id is required".into(),
                        },
                    );
                    return;
                }
                if target_norm == device_id {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "self_connect".into(),
                            message: "cannot relay to self".into(),
                        },
                    );
                    return;
                }
                let session_id = match resolve_optional_ascii_token(
                    session_id,
                    MAX_SESSION_ID_LEN,
                    12,
                    "session_id",
                ) {
                    Ok(s) => s,
                    Err(reason) => {
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "invalid_payload".into(),
                                message: reason,
                            },
                        );
                        return;
                    }
                };

                // H6: Per-peer rate limiting on ConnectRequest / RelayAlloc.
                if !check_sliding_window_rate_limit(
                    &self.connect_rate_limits,
                    device_id.to_string(),
                    self.config.connect_rate_limit_max,
                    self.config.connect_rate_limit_window,
                ) {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "rate_limited".into(),
                            message: "too many connect/relay requests".into(),
                        },
                    );
                    return;
                }

                // L5 + M4: Refresh ephemeral online TTL on authenticated peer activity.
                if !self
                    .refresh_online_under_lock(device_id, conn_id, cancel, None)
                    .await
                {
                    return;
                }

                let target_tx = self.peers.get(&target_norm).map(|p| p.tx.clone());
                let Some(host_tx) = target_tx else {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::PeerOffline {
                            target_id: target_norm,
                            session_id,
                        },
                    );
                    return;
                };

                let (owner_grant, is_owner_connection) = match self
                    .authorize_peer_connection(device_id, &target_norm, &session_id)
                    .await
                {
                    Ok(res) => res,
                    Err((code, message)) => {
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: code.into(),
                                message,
                            },
                        );
                        return;
                    }
                };

                if !self.is_active_conn(device_id, conn_id, cancel) {
                    return;
                }

                let relay_token = random_hex(32);
                let expires_in_sec = self.config.relay_token_ttl.as_secs();
                let client_msg = SignalMsg::RelayReady {
                    session_id: session_id.clone(),
                    role: "client".into(),
                    peer_id: target_norm.clone(),
                    relay_token: relay_token.clone(),
                    relay_addr: self.config.public_relay_addr.clone(),
                    expires_in_sec,
                    owner_grant,
                    is_owner_connection,
                };
                let host_msg = SignalMsg::RelayReady {
                    session_id: session_id.clone(),
                    role: "host".into(),
                    peer_id: device_id.to_string(),
                    relay_token: relay_token.clone(),
                    relay_addr: self.config.public_relay_addr.clone(),
                    expires_in_sec,
                    owner_grant: None,
                    is_owner_connection,
                };

                // H5: Pre-check serialized frame length before allocating token or enqueueing.
                if encode_signal_frame(&client_msg).is_err()
                    || encode_signal_frame(&host_msg).is_err()
                {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: "frame_too_large".into(),
                            message: "relay_ready frame exceeds maximum length".into(),
                        },
                    );
                    return;
                }

                let token_record = RelayTokenRecord {
                    session_id: session_id.clone(),
                    host_id: target_norm.clone(),
                    client_id: device_id.to_string(),
                };
                if let Err(code) = self.allocate_relay_token(&relay_token, token_record).await {
                    Self::send_nonblocking(
                        self_tx,
                        device_id,
                        SignalMsg::Error {
                            code: code.into(),
                            message: "failed to allocate relay token".into(),
                        },
                    );
                    return;
                }

                if !self.is_active_conn(device_id, conn_id, cancel) {
                    self.cleanup_relay_token(&relay_token).await;
                    return;
                }

                // H6: Deliver to host first; delete relay token if delivery fails.
                match Self::try_enqueue_checked(&host_tx, &target_norm, host_msg) {
                    Ok(()) => {
                        Self::send_nonblocking(self_tx, device_id, client_msg);
                    }
                    Err(EnqueueError::Closed) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::PeerOffline {
                                target_id: target_norm,
                                session_id,
                            },
                        );
                    }
                    Err(EnqueueError::Full) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "peer_busy".into(),
                                message: "target peer signaling queue is full".into(),
                            },
                        );
                    }
                    Err(EnqueueError::FrameTooLarge) => {
                        self.cleanup_relay_token(&relay_token).await;
                        Self::send_nonblocking(
                            self_tx,
                            device_id,
                            SignalMsg::Error {
                                code: "frame_too_large".into(),
                                message: "relay_ready frame exceeds maximum length".into(),
                            },
                        );
                    }
                }
            }

            SignalMsg::Register { .. } => {
                Self::send_nonblocking(
                    self_tx,
                    device_id,
                    SignalMsg::Error {
                        code: "already_registered".into(),
                        message: "connection is already registered".into(),
                    },
                );
            }

            _ => {
                Self::send_nonblocking(
                    self_tx,
                    device_id,
                    SignalMsg::Error {
                        code: "unexpected_message".into(),
                        message: "unsupported client-to-server signaling message".into(),
                    },
                );
            }
        }
    }

    fn try_enqueue_checked(
        tx: &mpsc::Sender<SignalMsg>,
        target_id: &str,
        msg: SignalMsg,
    ) -> Result<(), EnqueueError> {
        if let Err(err) = encode_signal_frame(&msg) {
            warn!(
                peer = %target_id,
                error = %err,
                "refusing to enqueue oversized or unencodable signaling frame"
            );
            return Err(EnqueueError::FrameTooLarge);
        }
        match tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!(peer = %target_id, "signaling outbound queue full");
                Err(EnqueueError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!(peer = %target_id, "signaling outbound queue closed");
                Err(EnqueueError::Closed)
            }
        }
    }

    fn send_nonblocking(tx: &mpsc::Sender<SignalMsg>, target_id: &str, msg: SignalMsg) {
        let _ = Self::try_enqueue_checked(tx, target_id, msg);
    }
}

fn random_hex(num_bytes: usize) -> String {
    let mut bytes = vec![0u8; num_bytes];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ephemeral::MemoryEphemeralStore;
    use crate::store::MemoryDeviceStore;

    #[test]
    fn udp_rcsp_rcsa_codec_roundtrip_v4_and_v6() {
        let req = UdpProbeRequest {
            device_id: "123456789".into(),
            nonce: [1, 2, 3, 4, 5, 6, 7, 8],
        };
        let encoded = req.encode().expect("encode RCSP");
        assert_eq!(encoded.len(), 22);
        assert_eq!(&encoded[0..4], b"RCSP");
        assert_eq!(encoded[4], 1);
        assert_eq!(&encoded[5..14], b"123456789");
        assert_eq!(&encoded[14..22], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(UdpProbeRequest::decode(&encoded).unwrap(), req);

        // IPv4 RCSA (20 bytes)
        let resp_v4 = UdpProbeResponse {
            observed_addr: "203.0.113.42:54321".parse().unwrap(),
            nonce: req.nonce,
        };
        let enc_v4 = resp_v4.encode();
        assert_eq!(enc_v4.len(), 20);
        assert_eq!(&enc_v4[0..4], b"RCSA");
        assert_eq!(enc_v4[4], 1);
        assert_eq!(enc_v4[5], 4);
        assert_eq!(&enc_v4[6..10], &[203, 0, 113, 42]);
        assert_eq!(&enc_v4[10..12], &54321u16.to_be_bytes());
        assert_eq!(&enc_v4[12..20], &req.nonce);
        assert_eq!(UdpProbeResponse::decode(&enc_v4).unwrap(), resp_v4);

        // IPv6 RCSA (32 bytes)
        let resp_v6 = UdpProbeResponse {
            observed_addr: "[2001:db8::1]:60000".parse().unwrap(),
            nonce: [9, 8, 7, 6, 5, 4, 3, 2],
        };
        let enc_v6 = resp_v6.encode();
        assert_eq!(enc_v6.len(), 32);
        assert_eq!(enc_v6[5], 16);
        assert_eq!(UdpProbeResponse::decode(&enc_v6).unwrap(), resp_v6);

        // Reject bad magic / version / non-digit ID / length
        let mut bad = encoded;
        bad[0] = b'X';
        assert!(UdpProbeRequest::decode(&bad).is_err());
        assert!(UdpProbeRequest::decode(&encoded[..21]).is_err());
        let mut bad_id = encoded;
        bad_id[6] = b'z';
        assert!(UdpProbeRequest::decode(&bad_id).is_err());
    }

    #[test]
    fn tcp_framing_encode_decode_and_bounds() {
        let msg = SignalMsg::Heartbeat {
            lan_addrs: vec!["192.168.1.10:50000".into()],
        };
        let frame = encode_signal_frame(&msg).expect("encode frame");
        let len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - 4);
        assert_eq!(decode_signal_frame(&frame).unwrap(), msg);

        // Zero-length rejected
        assert!(decode_signal_frame(&[0, 0, 0, 0]).is_err());
        // Oversized (>65536) rejected
        let mut oversized = (MAX_SIGNAL_FRAME_LEN as u32 + 1).to_be_bytes().to_vec();
        oversized.push(b'{');
        assert!(matches!(
            decode_signal_frame(&oversized),
            Err(SignalError::FrameTooLarge(_))
        ));
    }

    #[tokio::test]
    async fn udp_probe_listener_roundtrip_stores_public_addr() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();
        let tcp_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(tcp_listener));

        // Authenticate and register online over TCP signaling first (required by H4).
        let mut sig = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut sig,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:udp-probe-key".into(),
                hostname: "udp-box".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let device_id = match read_signal_frame(&mut sig).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_addr = server_sock.local_addr().unwrap();
        let udp_task = tokio::spawn(hub.clone().serve_udp_probe_on_socket(server_sock));

        let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_sock.local_addr().unwrap();

        let req = UdpProbeRequest {
            device_id: device_id.clone(),
            nonce: *b"rc-nonce",
        };
        client_sock
            .send_to(&req.encode().unwrap(), server_addr)
            .await
            .unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), client_sock.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let resp = UdpProbeResponse::decode(&buf[..n]).unwrap();
        assert_eq!(resp.observed_addr, client_addr);
        assert_eq!(resp.nonce, *b"rc-nonce");

        // Wait briefly for the bounded background UDP update worker to persist the endpoint.
        let mut stored = None;
        for _ in 0..100 {
            stored = ephemeral.get_public_udp(&device_id).await.unwrap();
            if stored.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(stored, Some(client_addr));

        drop(sig);
        hub.shutdown_all().await;
        let _ = udp_task.await;
        let _ = tcp_task.await;
    }

    #[tokio::test]
    async fn native_tcp_signaling_full_flow_and_kick() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store.clone(),
            ephemeral.clone(),
            SignalConfig {
                public_udp_probe_addr: "127.0.0.1:21116".into(),
                public_relay_addr: "127.0.0.1:21117".into(),
                ..SignalConfig::default()
            },
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        // 1. Non-register first frame is rejected.
        {
            let mut bad = TcpStream::connect(tcp_addr).await.unwrap();
            write_signal_frame(&mut bad, &SignalMsg::Heartbeat { lan_addrs: vec![] })
                .await
                .unwrap();
            let reply = read_signal_frame(&mut bad).await.unwrap().unwrap();
            assert!(matches!(
                reply,
                SignalMsg::Error { code, .. } if code == "expected_register"
            ));
        }

        // 2. Register Host and Client over native framed TCP.
        let mut host = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut host,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:host-key".into(),
                hostname: "host-mac".into(),
                os: "macos".into(),
                device_group: "lab".into(),
                lan_addrs: vec!["192.168.1.10:50000".into()],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let host_id = match read_signal_frame(&mut host).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered for host, got {other:?}"),
        };

        let mut client = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut client,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:client-key".into(),
                hostname: "client-mac".into(),
                os: "macos".into(),
                device_group: "lab".into(),
                lan_addrs: vec!["192.168.1.20:50001".into()],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let client_id = match read_signal_frame(&mut client).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered for client, got {other:?}"),
        };

        // Simulate UDP probes stored in ephemeral store.
        let host_udp: SocketAddr = "198.51.100.10:40001".parse().unwrap();
        let client_udp: SocketAddr = "203.0.113.20:40002".parse().unwrap();
        ephemeral
            .set_public_udp(&host_id, host_udp, Duration::from_secs(60))
            .await
            .unwrap();
        ephemeral
            .set_public_udp(&client_id, client_udp, Duration::from_secs(60))
            .await
            .unwrap();

        // 3. Heartbeat reflects observed UDP addr and online count.
        write_signal_frame(
            &mut client,
            &SignalMsg::Heartbeat {
                lan_addrs: vec!["192.168.1.20:50001".into()],
            },
        )
        .await
        .unwrap();
        match read_signal_frame(&mut client).await.unwrap().unwrap() {
            SignalMsg::HeartbeatAck {
                public_udp_addr,
                online_peers,
                ..
            } => {
                assert_eq!(public_udp_addr.as_deref(), Some("203.0.113.20:40002"));
                assert_eq!(online_peers, 2);
            }
            other => panic!("expected HeartbeatAck, got {other:?}"),
        }

        // 4. connect_request pushes punch_coord to BOTH client and host + stores fallback token.
        write_signal_frame(
            &mut client,
            &SignalMsg::ConnectRequest {
                target_id: host_id.clone(),
                session_id: Some("sess-punch-1".into()),
                lan_addrs: vec!["192.168.1.20:50001".into()],
                nonce: Some("deadbeef".into()),
            },
        )
        .await
        .unwrap();

        let client_punch = read_signal_frame(&mut client).await.unwrap().unwrap();
        let host_punch = read_signal_frame(&mut host).await.unwrap().unwrap();

        let (c_tok, h_tok) = match (client_punch, host_punch) {
            (
                SignalMsg::PunchCoord {
                    session_id: cs,
                    role: cr,
                    peer_id: cp,
                    peer_public_udp_addr: c_udp,
                    peer_lan_addrs: c_lan,
                    relay_token: ct,
                    ..
                },
                SignalMsg::PunchCoord {
                    session_id: hs,
                    role: hr,
                    peer_id: hp,
                    peer_public_udp_addr: h_udp,
                    peer_lan_addrs: h_lan,
                    relay_token: ht,
                    ..
                },
            ) => {
                assert_eq!(cs, "sess-punch-1");
                assert_eq!(hs, "sess-punch-1");
                assert_eq!(cr, "client");
                assert_eq!(hr, "host");
                assert_eq!(cp, host_id);
                assert_eq!(hp, client_id);
                assert_eq!(c_udp.as_deref(), Some("198.51.100.10:40001"));
                assert_eq!(h_udp.as_deref(), Some("203.0.113.20:40002"));
                assert_eq!(c_lan, vec!["192.168.1.10:50000".to_string()]);
                assert_eq!(h_lan, vec!["192.168.1.20:50001".to_string()]);
                (ct, ht)
            }
            other => panic!("expected PunchCoord on both sides, got {other:?}"),
        };
        assert_eq!(c_tok, h_tok);
        let tok_rec = ephemeral
            .take_relay_token(&c_tok)
            .await
            .unwrap()
            .expect("relay token stored");
        assert_eq!(tok_rec.session_id, "sess-punch-1");
        assert_eq!(tok_rec.host_id, host_id);
        assert_eq!(tok_rec.client_id, client_id);

        // 5. relay_alloc pushes relay_ready to BOTH sides.
        write_signal_frame(
            &mut client,
            &SignalMsg::RelayAlloc {
                target_id: host_id.clone(),
                session_id: Some("sess-relay-2".into()),
            },
        )
        .await
        .unwrap();

        let client_ready = read_signal_frame(&mut client).await.unwrap().unwrap();
        let host_ready = read_signal_frame(&mut host).await.unwrap().unwrap();
        match (client_ready, host_ready) {
            (
                SignalMsg::RelayReady {
                    session_id: cs,
                    role: cr,
                    relay_token: ct,
                    ..
                },
                SignalMsg::RelayReady {
                    session_id: hs,
                    role: hr,
                    relay_token: ht,
                    ..
                },
            ) => {
                assert_eq!(cs, "sess-relay-2");
                assert_eq!(hs, "sess-relay-2");
                assert_eq!(cr, "client");
                assert_eq!(hr, "host");
                assert_eq!(ct, ht);
                assert!(ephemeral.take_relay_token(&ct).await.unwrap().is_some());
            }
            other => panic!("expected RelayReady on both sides, got {other:?}"),
        }

        // 6. Offline target returns peer_offline.
        write_signal_frame(
            &mut client,
            &SignalMsg::ConnectRequest {
                target_id: "999999999".into(),
                session_id: Some("sess-off".into()),
                lan_addrs: vec![],
                nonce: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut client).await.unwrap().unwrap(),
            SignalMsg::PeerOffline { target_id, session_id }
                if target_id == "999999999" && session_id == "sess-off"
        ));

        // 7. kick_device sends `kicked` and closes the TCP stream.
        assert!(hub.kick_device(&host_id, "admin_test").await);
        assert!(matches!(
            read_signal_frame(&mut host).await.unwrap().unwrap(),
            SignalMsg::Kicked { reason } if reason == "admin_test"
        ));
        assert_eq!(read_signal_frame(&mut host).await.unwrap(), None);
        assert!(!hub.is_peer_connected(&host_id));
        assert!(!ephemeral.is_online(&host_id).await.unwrap());

        // 8. Disabled device is rejected on register.
        store.set_disabled(&host_id, true).await.unwrap();
        let mut disabled_conn = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut disabled_conn,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:host-key".into(),
                hostname: "host-mac".into(),
                os: "macos".into(),
                device_group: "lab".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut disabled_conn).await.unwrap().unwrap(),
            SignalMsg::Error { code, .. } if code == "device_disabled"
        ));

        server_task.abort();
    }

    #[tokio::test]
    async fn duplicate_reconnect_kicks_old_and_mismatch_rejected() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        let mut conn1 = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut conn1,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:dup-key".into(),
                hostname: "mac-1".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let device_id = match read_signal_frame(&mut conn1).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        // Claiming wrong device_id for same device_key is rejected without kicking conn1.
        let mut wrong = TcpStream::connect(tcp_addr).await.unwrap();
        let wrong_id = if device_id == "111111111" {
            "222222222"
        } else {
            "111111111"
        };
        write_signal_frame(
            &mut wrong,
            &SignalMsg::Register {
                device_id: Some(wrong_id.into()),
                device_key: "ed25519:dup-key".into(),
                hostname: "mac-1".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut wrong).await.unwrap().unwrap(),
            SignalMsg::Error { code, .. } if code == "device_id_mismatch"
        ));

        // Reconnecting legitimately replaces conn1 and kicks conn1.
        let mut conn2 = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut conn2,
            &SignalMsg::Register {
                device_id: Some(device_id.clone()),
                device_key: "ed25519:dup-key".into(),
                hostname: "mac-1".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut conn2).await.unwrap().unwrap(),
            SignalMsg::Registered { device_id: id, .. } if id == device_id
        ));

        assert!(matches!(
            read_signal_frame(&mut conn1).await.unwrap().unwrap(),
            SignalMsg::Kicked { reason } if reason == "replaced_by_new_connection"
        ));
        assert_eq!(read_signal_frame(&mut conn1).await.unwrap(), None);

        // New connection remains online after old connection finishes cleanup.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(hub.is_peer_connected(&device_id));
        assert!(ephemeral.is_online(&device_id).await.unwrap());

        server_task.abort();
    }

    async fn accept_and_spawn_one(
        hub: Arc<SignalHub>,
        listener: &TcpListener,
    ) -> tokio::task::JoinHandle<()> {
        let (stream, addr) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            hub.handle_tcp_connection(stream, addr).await;
        })
    }

    async fn flood_peer_offline_to_stall(stream: &mut TcpStream) {
        // Shrink client receive buffer and flood valid `Heartbeat` frames without reading
        // `HeartbeatAck` replies so both kernel TCP buffers and the bounded server channel fill.
        let _ = socket2::SockRef::from(&*stream).set_recv_buffer_size(4096);
        for _ in 0..2048 {
            let msg = SignalMsg::Heartbeat { lan_addrs: vec![] };
            if write_signal_frame(stream, &msg).await.is_err() {
                break;
            }
        }
    }

    #[tokio::test]
    async fn kick_and_replace_terminate_stalled_peer_and_protect_new_lan_addrs() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig {
                channel_capacity: 2,
                write_timeout: Duration::from_millis(200),
                read_idle_timeout: Duration::from_secs(30),
                ..SignalConfig::default()
            },
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();

        // Register a target peer so we can prove kicked/stalled peers cannot allocate relay tokens.
        let mut target_client = TcpStream::connect(tcp_addr).await.unwrap();
        let target_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut target_client,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:target-peer".into(),
                hostname: "target".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let target_id = match read_signal_frame(&mut target_client)
            .await
            .unwrap()
            .unwrap()
        {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered for target, got {other:?}"),
        };

        // 1. Kick a stalled peer whose channel & kernel send buffer are full.
        let mut stalled = TcpStream::connect(tcp_addr).await.unwrap();
        let stalled_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut stalled,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:stalled-kick-key".into(),
                hostname: "stalled-box".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec!["10.0.0.1:5000".into()],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let stalled_id = match read_signal_frame(&mut stalled).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered for stalled peer, got {other:?}"),
        };

        // Stop reading from `stalled` and saturate its outbound buffers + channel.
        flood_peer_offline_to_stall(&mut stalled).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(hub.kick_device(&stalled_id, "admin_stalled_kick").await);
        tokio::time::timeout(Duration::from_secs(2), stalled_task)
            .await
            .expect("kicked stalled connection must exit promptly")
            .unwrap();

        // Subsequent frames from the kicked socket cannot re-mark it online or allocate relay tokens.
        let _ = write_signal_frame(
            &mut stalled,
            &SignalMsg::Heartbeat {
                lan_addrs: vec!["10.99.99.99:9999".into()],
            },
        )
        .await;
        let _ = write_signal_frame(
            &mut stalled,
            &SignalMsg::RelayAlloc {
                target_id: target_id.clone(),
                session_id: Some("sess-after-kick".into()),
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(!hub.is_peer_connected(&stalled_id));
        assert!(!ephemeral.is_online(&stalled_id).await.unwrap());
        assert_eq!(ephemeral.active_token_count(), 0);

        // 2. Duplicate reconnect replaces a stalled old connection and protects new `lan_addrs`.
        let mut old_conn = TcpStream::connect(tcp_addr).await.unwrap();
        let old_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut old_conn,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:replace-stalled-key".into(),
                hostname: "dup-box".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec!["10.1.0.1:1111".into()],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let dup_id = match read_signal_frame(&mut old_conn).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered for old_conn, got {other:?}"),
        };

        flood_peer_offline_to_stall(&mut old_conn).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut new_conn = TcpStream::connect(tcp_addr).await.unwrap();
        let new_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut new_conn,
            &SignalMsg::Register {
                device_id: Some(dup_id.clone()),
                device_key: "ed25519:replace-stalled-key".into(),
                hostname: "dup-box".into(),
                os: "macos".into(),
                device_group: "".into(),
                lan_addrs: vec!["10.2.0.2:2222".into()],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut new_conn).await.unwrap().unwrap(),
            SignalMsg::Registered { device_id, .. } if device_id == dup_id
        ));

        // Attempt a stale heartbeat from `old_conn` carrying old `lan_addrs`.
        let _ = write_signal_frame(
            &mut old_conn,
            &SignalMsg::Heartbeat {
                lan_addrs: vec!["10.99.99.99:9999".into()],
            },
        )
        .await;

        tokio::time::timeout(Duration::from_secs(2), old_task)
            .await
            .expect("replaced stalled connection must exit promptly")
            .unwrap();

        assert!(hub.is_peer_connected(&dup_id));
        assert!(ephemeral.is_online(&dup_id).await.unwrap());
        assert_eq!(
            hub.peer_lan_addrs(&dup_id),
            Some(vec!["10.2.0.2:2222".to_string()])
        );

        drop(new_conn);
        drop(target_client);
        let _ = tokio::time::timeout(Duration::from_secs(2), new_task).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), target_task).await;
    }

    #[tokio::test]
    async fn idle_timeout_exits_promptly_when_writer_is_stalled() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig {
                channel_capacity: 2,
                write_timeout: Duration::from_millis(200),
                read_idle_timeout: Duration::from_millis(150),
                ..SignalConfig::default()
            },
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();

        let mut client = TcpStream::connect(tcp_addr).await.unwrap();
        let conn_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut client,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:idle-stalled-key".into(),
                hostname: "idle-box".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let device_id = match read_signal_frame(&mut client).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        flood_peer_offline_to_stall(&mut client).await;

        // Do not read or write anymore; idle timeout must abort the stalled writer without hanging.
        tokio::time::timeout(Duration::from_secs(2), conn_task)
            .await
            .expect("idle timeout must not hang waiting on stalled writer")
            .unwrap();
        assert!(!hub.is_peer_connected(&device_id));
        assert!(!ephemeral.is_online(&device_id).await.unwrap());
    }

    #[tokio::test]
    async fn claimed_device_id_mismatch_does_not_insert_or_mutate_store() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store.clone(),
            ephemeral,
            SignalConfig::default(),
        ));

        let rec_a = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-a".into(),
                hostname: "host-a".into(),
                os: "macos".into(),
                device_group: "grp-a".into(),
            })
            .await
            .unwrap();
        let rec_b = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:key-b".into(),
                hostname: "host-b".into(),
                os: "linux".into(),
                device_group: "grp-b".into(),
            })
            .await
            .unwrap();
        assert_eq!(store.count_devices().await.unwrap(), 2);

        tokio::time::sleep(Duration::from_millis(20)).await;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        // 1. Unknown device_key claiming existing device A's ID -> rejected, no insert, no mutation.
        {
            let mut s = TcpStream::connect(tcp_addr).await.unwrap();
            write_signal_frame(
                &mut s,
                &SignalMsg::Register {
                    device_id: Some(rec_a.device_id.clone()),
                    device_key: "ed25519:unknown-attacker-key".into(),
                    hostname: "evil-host".into(),
                    os: "evil-os".into(),
                    device_group: "evil-grp".into(),
                    lan_addrs: vec![],
                    access_token: None,
                    device_pubkey: None,
                    allow_remote_assist: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut s).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "device_id_mismatch"
            ));
        }

        // 2. Unknown device_key claiming non-existent ID -> rejected, no insert.
        {
            let mut s = TcpStream::connect(tcp_addr).await.unwrap();
            write_signal_frame(
                &mut s,
                &SignalMsg::Register {
                    device_id: Some("999999999".into()),
                    device_key: "ed25519:another-unknown-key".into(),
                    hostname: "ghost".into(),
                    os: "linux".into(),
                    device_group: "ghost".into(),
                    lan_addrs: vec![],
                    access_token: None,
                    device_pubkey: None,
                    allow_remote_assist: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut s).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "device_id_mismatch"
            ));
        }

        // 3. Existing key-a claiming device B's ID -> rejected, neither A nor B mutated.
        {
            let mut s = TcpStream::connect(tcp_addr).await.unwrap();
            write_signal_frame(
                &mut s,
                &SignalMsg::Register {
                    device_id: Some(rec_b.device_id.clone()),
                    device_key: "ed25519:key-a".into(),
                    hostname: "mutated-a".into(),
                    os: "mutated-os".into(),
                    device_group: "mutated-grp".into(),
                    lan_addrs: vec![],
                    access_token: None,
                    device_pubkey: None,
                    allow_remote_assist: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut s).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "device_id_mismatch"
            ));
        }

        assert_eq!(store.count_devices().await.unwrap(), 2);
        let after_a = store.get_by_id(&rec_a.device_id).await.unwrap().unwrap();
        let after_b = store.get_by_id(&rec_b.device_id).await.unwrap().unwrap();
        assert_eq!(after_a, rec_a);
        assert_eq!(after_b, rec_b);

        server_task.abort();
    }

    struct GatedEphemeralStore {
        inner: MemoryEphemeralStore,
        gate_enabled: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl GatedEphemeralStore {
        fn new() -> Self {
            Self {
                inner: MemoryEphemeralStore::new(),
                gate_enabled: std::sync::atomic::AtomicBool::new(false),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }

        fn enable_gate(&self, enabled: bool) {
            self.gate_enabled
                .store(enabled, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl EphemeralStore for GatedEphemeralStore {
        async fn set_online(
            &self,
            device_id: &str,
            ttl: Duration,
        ) -> Result<(), crate::ephemeral::EphemeralError> {
            if self
                .gate_enabled
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.set_online(device_id, ttl).await
        }

        async fn remove_online(
            &self,
            device_id: &str,
        ) -> Result<(), crate::ephemeral::EphemeralError> {
            self.inner.remove_online(device_id).await
        }

        async fn is_online(
            &self,
            device_id: &str,
        ) -> Result<bool, crate::ephemeral::EphemeralError> {
            self.inner.is_online(device_id).await
        }

        async fn online_ids(&self) -> Result<Vec<String>, crate::ephemeral::EphemeralError> {
            self.inner.online_ids().await
        }

        async fn set_public_udp(
            &self,
            device_id: &str,
            addr: SocketAddr,
            ttl: Duration,
        ) -> Result<(), crate::ephemeral::EphemeralError> {
            self.inner.set_public_udp(device_id, addr, ttl).await
        }

        async fn get_public_udp(
            &self,
            device_id: &str,
        ) -> Result<Option<SocketAddr>, crate::ephemeral::EphemeralError> {
            self.inner.get_public_udp(device_id).await
        }

        async fn put_relay_token(
            &self,
            token: &str,
            record: RelayTokenRecord,
            ttl: Duration,
        ) -> Result<(), crate::ephemeral::EphemeralError> {
            self.inner.put_relay_token(token, record, ttl).await
        }

        async fn take_relay_token(
            &self,
            token: &str,
        ) -> Result<Option<RelayTokenRecord>, crate::ephemeral::EphemeralError> {
            self.inner.take_relay_token(token).await
        }

        fn backend_name(&self) -> &'static str {
            "gated-memory"
        }

        async fn health_check(&self) -> Result<(), crate::ephemeral::EphemeralError> {
            self.inner.health_check().await
        }
    }

    #[tokio::test]
    async fn kick_during_inflight_register_and_heartbeat_clears_ephemeral_online() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(GatedEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store.clone(),
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();

        // Pre-register device so we know its device_id before Register's `set_online` completes.
        let rec = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:race-key".into(),
                hostname: "race-box".into(),
                os: "linux".into(),
                device_group: "".into(),
            })
            .await
            .unwrap();

        // Sub-case A: Kick arrives while Register's `set_online` is in flight.
        ephemeral.enable_gate(true);
        let mut conn1 = TcpStream::connect(tcp_addr).await.unwrap();
        let conn1_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut conn1,
            &SignalMsg::Register {
                device_id: Some(rec.device_id.clone()),
                device_key: "ed25519:race-key".into(),
                hostname: "race-box".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), ephemeral.entered.notified())
            .await
            .expect("register must enter set_online");

        // M4 stripe lock serializes kick_device behind the in-flight Register transition;
        // spawn kick concurrently, then release the gate so kick executes immediately after.
        let kick_task1 = tokio::spawn({
            let hub = hub.clone();
            let id = rec.device_id.clone();
            async move { hub.kick_device(&id, "kick_during_register").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        ephemeral.release.notify_one();

        assert!(tokio::time::timeout(Duration::from_secs(2), kick_task1)
            .await
            .expect("kick_device must complete after register releases stripe lock")
            .unwrap());

        tokio::time::timeout(Duration::from_secs(2), conn1_task)
            .await
            .expect("kicked conn1 must exit promptly")
            .unwrap();
        assert!(!hub.is_peer_connected(&rec.device_id));
        assert!(
            !ephemeral.is_online(&rec.device_id).await.unwrap(),
            "device must not remain falsely online after kick during Register"
        );

        // Sub-case B: Kick arrives while Heartbeat's `set_online` is in flight.
        let mut conn2 = TcpStream::connect(tcp_addr).await.unwrap();
        let conn2_task = accept_and_spawn_one(hub.clone(), &listener).await;
        write_signal_frame(
            &mut conn2,
            &SignalMsg::Register {
                device_id: Some(rec.device_id.clone()),
                device_key: "ed25519:race-key".into(),
                hostname: "race-box".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut conn2).await.unwrap().unwrap(),
            SignalMsg::Registered { device_id, .. } if device_id == rec.device_id
        ));

        ephemeral.enable_gate(true);
        write_signal_frame(
            &mut conn2,
            &SignalMsg::Heartbeat {
                lan_addrs: vec!["10.0.0.5:5000".into()],
            },
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), ephemeral.entered.notified())
            .await
            .expect("heartbeat must enter set_online");

        let kick_task2 = tokio::spawn({
            let hub = hub.clone();
            let id = rec.device_id.clone();
            async move { hub.kick_device(&id, "kick_during_heartbeat").await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        ephemeral.release.notify_one();

        assert!(tokio::time::timeout(Duration::from_secs(2), kick_task2)
            .await
            .expect("kick_device must complete after heartbeat releases stripe lock")
            .unwrap());

        tokio::time::timeout(Duration::from_secs(2), conn2_task)
            .await
            .expect("kicked conn2 must exit promptly")
            .unwrap();
        assert!(!hub.is_peer_connected(&rec.device_id));
        assert!(
            !ephemeral.is_online(&rec.device_id).await.unwrap(),
            "device must not remain falsely online after kick during Heartbeat"
        );
    }

    async fn register_peer_helper(
        tcp_addr: SocketAddr,
        device_key: &str,
        lan_addrs: Vec<String>,
    ) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut stream,
            &SignalMsg::Register {
                device_id: None,
                device_key: device_key.into(),
                hostname: "test-host".into(),
                os: "linux".into(),
                device_group: "test".into(),
                lan_addrs,
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let device_id = match read_signal_frame(&mut stream).await.unwrap().unwrap() {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered, got {other:?}"),
        };
        (stream, device_id)
    }

    #[tokio::test]
    async fn h4_udp_spoof_offline_invalid_id_and_ip_mismatch_rejected() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig {
                udp_rate_limit_max: 4,
                udp_rate_limit_window: Duration::from_secs(10),
                ..SignalConfig::default()
            },
        ));

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();

        // Register a victim peer whose recorded TCP remote IP is 198.51.100.99 (different from 127.0.0.1!).
        let mut victim_client = TcpStream::connect(tcp_addr).await.unwrap();
        let (victim_srv, _) = tcp_listener.accept().await.unwrap();
        let spoofed_remote: SocketAddr = "198.51.100.99:44444".parse().unwrap();
        let victim_task = tokio::spawn(
            hub.clone()
                .handle_tcp_connection(victim_srv, spoofed_remote),
        );
        write_signal_frame(
            &mut victim_client,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:h4-victim".into(),
                hostname: "victim".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        let victim_id = match read_signal_frame(&mut victim_client)
            .await
            .unwrap()
            .unwrap()
        {
            SignalMsg::Registered { device_id, .. } => device_id,
            other => panic!("expected Registered, got {other:?}"),
        };

        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_addr = server_sock.local_addr().unwrap();
        let udp_task = tokio::spawn(hub.clone().serve_udp_probe_on_socket(server_sock));

        let attacker_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = [0u8; 64];

        // 1. Invalid device_id ("012345678", starts with 0 and is not ANONYMOUS_DEVICE_ID) -> dropped.
        let invalid_req = UdpProbeRequest {
            device_id: "012345678".into(),
            nonce: *b"bad00001",
        };
        attacker_sock
            .send_to(&invalid_req.encode().unwrap(), server_addr)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), attacker_sock.recv_from(&mut buf))
                .await
                .is_err(),
            "invalid device_id RCSP must be dropped without RCSA"
        );
        assert_eq!(ephemeral.get_public_udp("012345678").await.unwrap(), None);

        // 2. Offline valid device_id -> echoes RCSA, but MUST NOT store public_udp!
        let offline_req = UdpProbeRequest {
            device_id: "987654321".into(),
            nonce: *b"off00002",
        };
        attacker_sock
            .send_to(&offline_req.encode().unwrap(), server_addr)
            .await
            .unwrap();
        let (n, _) =
            tokio::time::timeout(Duration::from_secs(2), attacker_sock.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            UdpProbeResponse::decode(&buf[..n]).unwrap().nonce,
            *b"off00002"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(ephemeral.get_public_udp("987654321").await.unwrap(), None);

        // 3. Online victim_id probed from mismatched IP (127.0.0.1 != 198.51.100.99) -> spoof rejected!
        let spoof_req = UdpProbeRequest {
            device_id: victim_id.clone(),
            nonce: *b"spf00003",
        };
        attacker_sock
            .send_to(&spoof_req.encode().unwrap(), server_addr)
            .await
            .unwrap();
        let (n, _) =
            tokio::time::timeout(Duration::from_secs(2), attacker_sock.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            UdpProbeResponse::decode(&buf[..n]).unwrap().nonce,
            *b"spf00003"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            ephemeral.get_public_udp(&victim_id).await.unwrap(),
            None,
            "mismatched-IP UDP probe must NOT poison victim's public_udp"
        );

        // 4. Anonymous probe consumes 4th rate-limit slot, then 5th probe is rate-limited!
        let anon_req = UdpProbeRequest {
            device_id: ANONYMOUS_DEVICE_ID.into(),
            nonce: *b"ann00004",
        };
        attacker_sock
            .send_to(&anon_req.encode().unwrap(), server_addr)
            .await
            .unwrap();
        let (n, _) =
            tokio::time::timeout(Duration::from_secs(2), attacker_sock.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            UdpProbeResponse::decode(&buf[..n]).unwrap().nonce,
            *b"ann00004"
        );

        // 5th packet from same IP exceeds udp_rate_limit_max (4) -> dropped!
        attacker_sock
            .send_to(&anon_req.encode().unwrap(), server_addr)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), attacker_sock.recv_from(&mut buf))
                .await
                .is_err(),
            "UDP probe exceeding per-IP rate limit must be dropped"
        );

        drop(victim_client);
        hub.shutdown_all().await;
        let _ = victim_task.await;
        let _ = udp_task.await;
    }

    struct SlowUdpEphemeralStore {
        inner: MemoryEphemeralStore,
        udp_delay: Duration,
    }

    #[async_trait::async_trait]
    impl EphemeralStore for SlowUdpEphemeralStore {
        async fn set_online(&self, device_id: &str, ttl: Duration) -> Result<(), EphemeralError> {
            self.inner.set_online(device_id, ttl).await
        }
        async fn remove_online(&self, device_id: &str) -> Result<(), EphemeralError> {
            self.inner.remove_online(device_id).await
        }
        async fn is_online(&self, device_id: &str) -> Result<bool, EphemeralError> {
            self.inner.is_online(device_id).await
        }
        async fn online_ids(&self) -> Result<Vec<String>, EphemeralError> {
            self.inner.online_ids().await
        }
        async fn set_public_udp(
            &self,
            device_id: &str,
            addr: SocketAddr,
            ttl: Duration,
        ) -> Result<(), EphemeralError> {
            tokio::time::sleep(self.udp_delay).await;
            self.inner.set_public_udp(device_id, addr, ttl).await
        }
        async fn get_public_udp(
            &self,
            device_id: &str,
        ) -> Result<Option<SocketAddr>, EphemeralError> {
            self.inner.get_public_udp(device_id).await
        }
        async fn put_relay_token(
            &self,
            token: &str,
            record: RelayTokenRecord,
            ttl: Duration,
        ) -> Result<(), EphemeralError> {
            self.inner.put_relay_token(token, record, ttl).await
        }
        async fn take_relay_token(
            &self,
            token: &str,
        ) -> Result<Option<RelayTokenRecord>, EphemeralError> {
            self.inner.take_relay_token(token).await
        }
        fn backend_name(&self) -> &'static str {
            "slow-udp-memory"
        }
        async fn health_check(&self) -> Result<(), EphemeralError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn m5_ipv4_mapped_ipv6_canonicalized_and_slow_backend_does_not_block_udp() {
        // 1. Canonicalization of IPv4-mapped IPv6 addresses in helper & RCSA wire encoding.
        let mapped: SocketAddr = "[::ffff:203.0.113.42]:54321".parse().unwrap();
        let expected_v4: SocketAddr = "203.0.113.42:54321".parse().unwrap();
        assert_eq!(canonical_socket_addr(mapped), expected_v4);

        let encoded = UdpProbeResponse {
            observed_addr: mapped,
            nonce: *b"12345678",
        }
        .encode();
        assert_eq!(
            encoded.len(),
            20,
            "IPv4-mapped IPv6 must encode as 20B IPv4"
        );
        let decoded = UdpProbeResponse::decode(&encoded).unwrap();
        assert_eq!(decoded.observed_addr, expected_v4);

        // 2. Slow ephemeral backend must NEVER head-of-line block UDP RCSA replies.
        let store = Arc::new(MemoryDeviceStore::new());
        let slow_ephemeral = Arc::new(SlowUdpEphemeralStore {
            inner: MemoryEphemeralStore::new(),
            udp_delay: Duration::from_secs(2),
        });
        let hub = Arc::new(SignalHub::new(
            store,
            slow_ephemeral,
            SignalConfig::default(),
        ));

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();
        let tcp_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(tcp_listener));
        let (sig, device_id) = register_peer_helper(tcp_addr, "ed25519:m5-slow", vec![]).await;

        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let server_addr = server_sock.local_addr().unwrap();
        let udp_task = tokio::spawn(hub.clone().serve_udp_probe_on_socket(server_sock));

        let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = Instant::now();
        let mut buf = [0u8; 64];
        for i in 0u8..5 {
            let req = UdpProbeRequest {
                device_id: device_id.clone(),
                nonce: [i; 8],
            };
            client_sock
                .send_to(&req.encode().unwrap(), server_addr)
                .await
                .unwrap();
            let (n, _) =
                tokio::time::timeout(Duration::from_millis(200), client_sock.recv_from(&mut buf))
                    .await
                    .expect("RCSA must not be head-of-line blocked by slow ephemeral store")
                    .unwrap();
            assert_eq!(UdpProbeResponse::decode(&buf[..n]).unwrap().nonce, [i; 8]);
        }
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "5 RCSA roundtrips should complete rapidly without waiting for 2s backend delay"
        );

        drop(sig);
        hub.shutdown_all().await;
        let _ = udp_task.await;
        let _ = tcp_task.await;
    }

    #[tokio::test]
    async fn h5_oversized_fields_rejected_without_kicking_victim_and_writer_skips_bad_frames() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        // 1. Register with oversized hostname / NUL in os / >16 lan_addrs is rejected.
        for bad_reg in [
            SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:bad-host".into(),
                hostname: "h".repeat(MAX_HOSTNAME_LEN + 1),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
            SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:bad-os".into(),
                hostname: "ok".into(),
                os: "lin\0ux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
            SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:bad-lan".into(),
                hostname: "ok".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec!["10.0.0.1:5000".into(); MAX_LAN_ADDRS + 1],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        ] {
            let mut s = TcpStream::connect(tcp_addr).await.unwrap();
            write_signal_frame(&mut s, &bad_reg).await.unwrap();
            assert!(matches!(
                read_signal_frame(&mut s).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "invalid_payload"
            ));
        }

        // 2. Register victim and attacker.
        let (mut victim, victim_id) =
            register_peer_helper(tcp_addr, "ed25519:h5-victim", vec!["10.0.0.2:5000".into()]).await;
        let (mut attacker, _) = register_peer_helper(tcp_addr, "ed25519:h5-attacker", vec![]).await;

        // Attacker sends oversized/control-char ConnectRequest & RelayAlloc payloads.
        for bad_msg in [
            SignalMsg::ConnectRequest {
                target_id: victim_id.clone(),
                session_id: Some("a".repeat(MAX_SESSION_ID_LEN + 1)),
                lan_addrs: vec![],
                nonce: None,
            },
            SignalMsg::ConnectRequest {
                target_id: victim_id.clone(),
                session_id: Some("bad\nsess".into()),
                lan_addrs: vec![],
                nonce: None,
            },
            SignalMsg::ConnectRequest {
                target_id: victim_id.clone(),
                session_id: Some("ok-sess".into()),
                lan_addrs: vec![],
                nonce: Some("n".repeat(MAX_NONCE_LEN + 1)),
            },
            SignalMsg::ConnectRequest {
                target_id: victim_id.clone(),
                session_id: Some("ok-sess".into()),
                lan_addrs: vec!["10.0.0.9:5000".into(); MAX_LAN_ADDRS + 1],
                nonce: None,
            },
            SignalMsg::ConnectRequest {
                target_id: victim_id.clone(),
                session_id: Some("ok-sess".into()),
                lan_addrs: vec!["x".repeat(MAX_LAN_ADDR_LEN + 1)],
                nonce: None,
            },
            SignalMsg::RelayAlloc {
                target_id: victim_id.clone(),
                session_id: Some("r".repeat(MAX_SESSION_ID_LEN + 1)),
            },
        ] {
            write_signal_frame(&mut attacker, &bad_msg).await.unwrap();
            assert!(matches!(
                read_signal_frame(&mut attacker).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "invalid_payload"
            ));
        }
        assert_eq!(ephemeral.active_token_count(), 0);

        // 3. Directly enqueue an oversized frame (>64 KiB) to victim's channel to prove the
        // writer task logs and skips SignalError::FrameTooLarge WITHOUT kicking the victim!
        {
            let victim_tx = hub.peers.get(&victim_id).unwrap().tx.clone();
            victim_tx
                .try_send(SignalMsg::Error {
                    code: "oversized_test".into(),
                    message: "Z".repeat(MAX_SIGNAL_FRAME_LEN + 1024),
                })
                .unwrap();
        }

        // Victim must remain connected and receive a normal HeartbeatAck!
        write_signal_frame(&mut victim, &SignalMsg::Heartbeat { lan_addrs: vec![] })
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), read_signal_frame(&mut victim))
                .await
                .expect("victim must stay alive after writer skips oversized frame")
                .unwrap()
                .unwrap(),
            SignalMsg::HeartbeatAck { .. }
        ));
        assert!(hub.is_peer_connected(&victim_id));

        drop(victim);
        drop(attacker);
        hub.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn h6_rate_limit_token_cap_and_failed_delivery_token_cleanup() {
        // Sub-case A: Per-peer rate limit on ConnectRequest & RelayAlloc.
        {
            let store = Arc::new(MemoryDeviceStore::new());
            let ephemeral = Arc::new(MemoryEphemeralStore::new());
            let hub = Arc::new(SignalHub::new(
                store,
                ephemeral.clone(),
                SignalConfig {
                    connect_rate_limit_max: 2,
                    connect_rate_limit_window: Duration::from_secs(10),
                    ..SignalConfig::default()
                },
            ));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

            let (mut host, host_id) =
                register_peer_helper(tcp_addr, "ed25519:rl-host", vec![]).await;
            let (mut client, _) = register_peer_helper(tcp_addr, "ed25519:rl-client", vec![]).await;

            // Request 1 (ConnectRequest) -> succeeds.
            write_signal_frame(
                &mut client,
                &SignalMsg::ConnectRequest {
                    target_id: host_id.clone(),
                    session_id: Some("s1".into()),
                    lan_addrs: vec![],
                    nonce: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::PunchCoord { .. }
            ));
            assert!(matches!(
                read_signal_frame(&mut host).await.unwrap().unwrap(),
                SignalMsg::PunchCoord { .. }
            ));

            // Request 2 (RelayAlloc) -> succeeds.
            write_signal_frame(
                &mut client,
                &SignalMsg::RelayAlloc {
                    target_id: host_id.clone(),
                    session_id: Some("s2".into()),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::RelayReady { .. }
            ));
            assert!(matches!(
                read_signal_frame(&mut host).await.unwrap().unwrap(),
                SignalMsg::RelayReady { .. }
            ));
            assert_eq!(ephemeral.active_token_count(), 2);

            // Request 3 -> rejected with rate_limited and NO token allocated!
            write_signal_frame(
                &mut client,
                &SignalMsg::RelayAlloc {
                    target_id: host_id.clone(),
                    session_id: Some("s3".into()),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "rate_limited"
            ));
            assert_eq!(ephemeral.active_token_count(), 2);

            drop(host);
            drop(client);
            hub.shutdown_all().await;
            let _ = server_task.await;
        }

        // Sub-case B: Global active relay token cap enforced in SignalHub & MemoryEphemeralStore.
        {
            let store = Arc::new(MemoryDeviceStore::new());
            let ephemeral = Arc::new(MemoryEphemeralStore::with_max_relay_tokens(2));
            let hub = Arc::new(SignalHub::new(
                store,
                ephemeral.clone(),
                SignalConfig {
                    max_relay_tokens: 2,
                    connect_rate_limit_max: 20,
                    ..SignalConfig::default()
                },
            ));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

            let (mut host, host_id) =
                register_peer_helper(tcp_addr, "ed25519:cap-host", vec![]).await;
            let (mut client, _) =
                register_peer_helper(tcp_addr, "ed25519:cap-client", vec![]).await;

            for i in 1..=2 {
                write_signal_frame(
                    &mut client,
                    &SignalMsg::RelayAlloc {
                        target_id: host_id.clone(),
                        session_id: Some(format!("cap-{i}")),
                    },
                )
                .await
                .unwrap();
                assert!(matches!(
                    read_signal_frame(&mut client).await.unwrap().unwrap(),
                    SignalMsg::RelayReady { .. }
                ));
                assert!(matches!(
                    read_signal_frame(&mut host).await.unwrap().unwrap(),
                    SignalMsg::RelayReady { .. }
                ));
            }
            assert_eq!(ephemeral.active_token_count(), 2);

            // 3rd request hits max_relay_tokens = 2.
            write_signal_frame(
                &mut client,
                &SignalMsg::RelayAlloc {
                    target_id: host_id.clone(),
                    session_id: Some("cap-3".into()),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "relay_capacity_exceeded"
            ));
            assert_eq!(ephemeral.active_token_count(), 2);

            drop(host);
            drop(client);
            hub.shutdown_all().await;
            let _ = server_task.await;
        }

        // Sub-case C: Failed delivery to host_tx immediately deletes the allocated relay token!
        {
            let store = Arc::new(MemoryDeviceStore::new());
            let ephemeral = Arc::new(MemoryEphemeralStore::new());
            let hub = Arc::new(SignalHub::new(
                store,
                ephemeral.clone(),
                SignalConfig {
                    channel_capacity: 1,
                    ..SignalConfig::default()
                },
            ));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

            let (mut client, _) = register_peer_helper(tcp_addr, "ed25519:fd-client", vec![]).await;

            // Insert a synthetic host peer whose mpsc channel (cap 1) is permanently full
            // (receiver kept alive and never drained) to deterministically trigger TrySendError::Full.
            let (full_tx, _hold_rx) = mpsc::channel::<SignalMsg>(1);
            full_tx
                .try_send(SignalMsg::Heartbeat { lan_addrs: vec![] })
                .unwrap();
            let full_host_id = "888777666".to_string();
            hub.peers.insert(
                full_host_id.clone(),
                ActivePeer {
                    conn_id: 999_999,
                    tx: full_tx,
                    cancel: PeerCancel::new(),
                    lan_addrs: vec![],
                    remote_addr: "127.0.0.1:50000".parse().unwrap(),
                    connected_at: Utc::now(),
                    owner_user_id: None,
                    device_pubkey: String::new(),
                    allow_remote_assist: false,
                    hostname: String::new(),
                    alias: String::new(),
                    os: String::new(),
                },
            );

            // ConnectRequest fails to deliver PunchCoord to full_host -> token must be deleted!
            write_signal_frame(
                &mut client,
                &SignalMsg::ConnectRequest {
                    target_id: full_host_id.clone(),
                    session_id: Some("full-punch".into()),
                    lan_addrs: vec![],
                    nonce: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "peer_busy"
            ));
            assert_eq!(
                ephemeral.active_token_count(),
                0,
                "failed PunchCoord delivery must delete allocated relay token"
            );

            // RelayAlloc fails to deliver RelayReady to full_host -> token must be deleted!
            write_signal_frame(
                &mut client,
                &SignalMsg::RelayAlloc {
                    target_id: full_host_id.clone(),
                    session_id: Some("full-relay".into()),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::Error { code, .. } if code == "peer_busy"
            ));
            assert_eq!(
                ephemeral.active_token_count(),
                0,
                "failed RelayReady delivery must delete allocated relay token"
            );

            // Closed host_tx -> returns PeerOffline and deletes allocated relay token!
            drop(_hold_rx);
            write_signal_frame(
                &mut client,
                &SignalMsg::RelayAlloc {
                    target_id: full_host_id.clone(),
                    session_id: Some("closed-relay".into()),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_signal_frame(&mut client).await.unwrap().unwrap(),
                SignalMsg::PeerOffline { target_id, session_id }
                    if target_id == full_host_id && session_id == "closed-relay"
            ));
            assert_eq!(
                ephemeral.active_token_count(),
                0,
                "closed-peer RelayReady delivery must delete allocated relay token"
            );

            hub.peers.remove(&full_host_id);
            drop(client);
            hub.shutdown_all().await;
            let _ = server_task.await;
        }
    }

    #[tokio::test]
    async fn h7_max_connections_and_incremental_truncated_frame_read() {
        // 1. Incremental payload read rejects truncated 64 KiB frame cleanly without over-allocating.
        let mut wire = (MAX_SIGNAL_FRAME_LEN as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(b"{\"type\":\"");
        let mut cursor = std::io::Cursor::new(wire);
        let err = read_signal_frame(&mut cursor).await.unwrap_err();
        assert!(matches!(err, SignalError::InvalidFrame(_)));

        // 2. Semaphore bounds concurrent TCP signaling connections.
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral,
            SignalConfig {
                max_connections: 2,
                ..SignalConfig::default()
            },
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        let (c1, _) = register_peer_helper(tcp_addr, "ed25519:h7-1", vec![]).await;
        let (c2, _) = register_peer_helper(tcp_addr, "ed25519:h7-2", vec![]).await;

        // 3rd connection cannot complete handshake while 2 permits are held.
        let mut c3 = TcpStream::connect(tcp_addr).await.unwrap();
        write_signal_frame(
            &mut c3,
            &SignalMsg::Register {
                device_id: None,
                device_key: "ed25519:h7-3".into(),
                hostname: "h7-3".into(),
                os: "linux".into(),
                device_group: "".into(),
                lan_addrs: vec![],
                access_token: None,
                device_pubkey: None,
                allow_remote_assist: None,
            },
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), read_signal_frame(&mut c3))
                .await
                .is_err(),
            "3rd connection must wait while max_connections=2 permits are held"
        );

        // Dropping c1 frees a permit so c3 registers immediately.
        drop(c1);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), read_signal_frame(&mut c3))
                .await
                .expect("3rd connection must succeed once a permit is released")
                .unwrap()
                .unwrap(),
            SignalMsg::Registered { .. }
        ));

        drop(c2);
        drop(c3);
        hub.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn m4_concurrent_register_disable_and_kick_maintains_consistency() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store.clone(),
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let rec = store
            .register_or_get(RegisterInput {
                device_key: "ed25519:m4-storm".into(),
                hostname: "storm".into(),
                os: "linux".into(),
                device_group: "".into(),
            })
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        // 1. Concurrent register & kick storm on the same device_id.
        let mut handles = Vec::new();
        for _ in 0..12 {
            let id = rec.device_id.clone();
            handles.push(tokio::spawn(async move {
                let mut s = TcpStream::connect(tcp_addr).await.unwrap();
                let _ = write_signal_frame(
                    &mut s,
                    &SignalMsg::Register {
                        device_id: Some(id),
                        device_key: "ed25519:m4-storm".into(),
                        hostname: "storm".into(),
                        os: "linux".into(),
                        device_group: "".into(),
                        lan_addrs: vec![],
                        access_token: None,
                        device_pubkey: None,
                        allow_remote_assist: None,
                    },
                )
                .await;
                let _ = read_signal_frame(&mut s).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }));
            let hub_k = hub.clone();
            let id_k = rec.device_id.clone();
            handles.push(tokio::spawn(async move {
                let _ = hub_k.kick_device(&id_k, "storm_kick").await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        // Wait briefly for all disconnect cleanups to finish, then check invariant.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            hub.is_peer_connected(&rec.device_id),
            ephemeral.is_online(&rec.device_id).await.unwrap(),
            "peers map and ephemeral online state must remain consistent after storm"
        );

        // 2. Concurrent disable + kick vs reconnect must NEVER leave a disabled device online.
        let disable_task = tokio::spawn({
            let store = store.clone();
            let hub = hub.clone();
            let id = rec.device_id.clone();
            async move {
                store.set_disabled(&id, true).await.unwrap();
                hub.kick_device(&id, "disabled_by_admin").await;
            }
        });
        let mut reg_tasks = Vec::new();
        for _ in 0..8 {
            let id = rec.device_id.clone();
            reg_tasks.push(tokio::spawn(async move {
                let mut s = TcpStream::connect(tcp_addr).await.unwrap();
                let _ = write_signal_frame(
                    &mut s,
                    &SignalMsg::Register {
                        device_id: Some(id),
                        device_key: "ed25519:m4-storm".into(),
                        hostname: "storm".into(),
                        os: "linux".into(),
                        device_group: "".into(),
                        lan_addrs: vec![],
                        access_token: None,
                        device_pubkey: None,
                        allow_remote_assist: None,
                    },
                )
                .await;
                let _ = read_signal_frame(&mut s).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }));
        }
        disable_task.await.unwrap();
        for t in reg_tasks {
            t.await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!hub.is_peer_connected(&rec.device_id));
        assert!(!ephemeral.is_online(&rec.device_id).await.unwrap());

        hub.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn l5_default_online_ttl_exceeds_idle_timeout_and_activity_refreshes_ttl() {
        let default_cfg = SignalConfig::default();
        assert_eq!(default_cfg.online_ttl, Duration::from_secs(150));
        assert!(
            default_cfg.online_ttl > default_cfg.read_idle_timeout,
            "default online_ttl ({:?}) must exceed read_idle_timeout ({:?})",
            default_cfg.online_ttl,
            default_cfg.read_idle_timeout
        );

        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig {
                online_ttl: Duration::from_millis(120),
                read_idle_timeout: Duration::from_secs(10),
                ..SignalConfig::default()
            },
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(listener));

        let (mut client, client_id) =
            register_peer_helper(tcp_addr, "ed25519:l5-activity", vec![]).await;
        assert!(ephemeral.is_online(&client_id).await.unwrap());

        // Sleep past half of online_ttl, then perform authenticated non-heartbeat activity.
        tokio::time::sleep(Duration::from_millis(70)).await;
        write_signal_frame(
            &mut client,
            &SignalMsg::ConnectRequest {
                target_id: "999999999".into(),
                session_id: Some("l5-keepalive".into()),
                lan_addrs: vec![],
                nonce: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_signal_frame(&mut client).await.unwrap().unwrap(),
            SignalMsg::PeerOffline { .. }
        ));

        // Total elapsed > 140ms > original 120ms TTL, yet device must remain online!
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(
            ephemeral.is_online(&client_id).await.unwrap(),
            "authenticated ConnectRequest activity must refresh online_ttl"
        );

        drop(client);
        hub.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn h10_shutdown_all_kicks_peers_stops_listeners_and_drains_tasks() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(
            store,
            ephemeral.clone(),
            SignalConfig::default(),
        ));

        let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tcp_addr = tcp_listener.local_addr().unwrap();
        let udp_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());

        let tcp_task = tokio::spawn(hub.clone().serve_tcp_signaling_on_listener(tcp_listener));
        let udp_task = tokio::spawn(hub.clone().serve_udp_probe_on_socket(udp_sock));

        // One authenticated peer + one idle unauthenticated pre-auth TCP peer.
        let (mut peer, peer_id) = register_peer_helper(tcp_addr, "ed25519:h10-peer", vec![]).await;
        let _idle_pre_auth = TcpStream::connect(tcp_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(hub.in_flight_task_count() >= 2);

        tokio::time::timeout(Duration::from_secs(2), hub.shutdown_all())
            .await
            .expect("shutdown_all must complete promptly");

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), read_signal_frame(&mut peer))
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            SignalMsg::Kicked { reason } if reason == "server_shutdown"
        ));
        assert_eq!(read_signal_frame(&mut peer).await.unwrap(), None);

        tokio::time::timeout(Duration::from_secs(2), tcp_task)
            .await
            .expect("TCP signaling listener must exit on shutdown_all")
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), udp_task)
            .await
            .expect("UDP probe listener must exit on shutdown_all")
            .unwrap()
            .unwrap();

        assert_eq!(hub.in_flight_task_count(), 0);
        assert_eq!(hub.online_count(), 0);
        assert!(!ephemeral.is_online(&peer_id).await.unwrap());
    }

    #[tokio::test]
    async fn memory_ephemeral_does_not_leak_issued_relay_tokens_and_os_len_matches_store() {
        assert_eq!(MAX_OS_LEN, crate::store::MAX_OS_LEN);
        assert_eq!(MAX_OS_LEN, 128);
        validate_register_fields("host", &"o".repeat(128), "grp", &[]).unwrap();

        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let hub = Arc::new(SignalHub::new(store, ephemeral, SignalConfig::default()));

        for i in 0..8 {
            hub.allocate_relay_token(
                &format!("tok-{i}"),
                RelayTokenRecord {
                    session_id: format!("sess-{i}"),
                    host_id: "100000001".to_string(),
                    client_id: "100000002".to_string(),
                },
            )
            .await
            .unwrap();
        }
        assert_eq!(
            hub.issued_relay_tokens.len(),
            0,
            "MemoryEphemeralStore reports exact counts so fallback issued_relay_tokens must not accumulate"
        );
    }
}
