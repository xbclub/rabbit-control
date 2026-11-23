#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod splice;

use std::collections::HashSet;
use std::io;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::ephemeral::{EphemeralStore, RelayTokenRecord};
use crate::pki::parse_peer_cert_identity;
use crate::store::{normalize_device_id, AuthStore, DeviceStore, STATUS_ACTIVE};

pub const RELAY_MAGIC: &[u8; 4] = b"RCR1";
pub const ROLE_HOST: u8 = 1;
pub const ROLE_CLIENT: u8 = 2;
pub const REPLY_OK: u8 = 0x00;
pub const REPLY_ERR: u8 = 0x01;
pub const MAX_TOKEN_LEN: usize = 128;

pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_PAIR_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_MAX_PENDING: usize = 4096;
pub const DEFAULT_MAX_CONNECTIONS: usize = 4096;

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
#[cfg(not(target_os = "linux"))]
const SOCKET_BUF_SIZE: usize = 256 * 1024;
const TCP_KEEPALIVE_TIME: Duration = Duration::from_secs(30);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "linux", target_os = "macos"))]
const TCP_KEEPALIVE_RETRIES: u32 = 3;
const USERSPACE_COPY_BUF_SIZE: usize = 32 * 1024;
const USERSPACE_YIELD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("invalid relay role: {0} (expected 1=host or 2=client)")]
    InvalidRole(u8),
    #[error("invalid relay token length: {0} (expected 1..={MAX_TOKEN_LEN})")]
    InvalidTokenLength(usize),
    #[error("invalid relay magic")]
    InvalidMagic,
    #[error("relay token is not valid UTF-8")]
    InvalidTokenUtf8,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Lock-free, race-free cancellation handle shared across both relay directions and admin kick API.
#[derive(Clone)]
pub struct CancellationFlag {
    cancelled: Arc<AtomicBool>,
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancellationFlag {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationFlag {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            tx: Arc::new(tx),
        }
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(true);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
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

/// Build an `RCR1` binary relay handshake frame:
/// `[b"RCR1" (4B)][role: 1B (1=host, 2=client)][token_len: 1B (1..=128)][token]`
pub fn encode_relay_handshake(role: u8, token: &str) -> Result<Vec<u8>, RelayError> {
    if role != ROLE_HOST && role != ROLE_CLIENT {
        return Err(RelayError::InvalidRole(role));
    }
    let token_bytes = token.as_bytes();
    if token_bytes.is_empty() || token_bytes.len() > MAX_TOKEN_LEN {
        return Err(RelayError::InvalidTokenLength(token_bytes.len()));
    }
    let mut buf = Vec::with_capacity(6 + token_bytes.len());
    buf.extend_from_slice(RELAY_MAGIC);
    buf.push(role);
    buf.push(token_bytes.len() as u8);
    buf.extend_from_slice(token_bytes);
    Ok(buf)
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub handshake_timeout: Duration,
    pub pair_timeout: Duration,
    pub max_pending: usize,
    pub max_connections: usize,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            pair_timeout: DEFAULT_PAIR_TIMEOUT,
            max_pending: DEFAULT_MAX_PENDING,
            max_connections: DEFAULT_MAX_CONNECTIONS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaySessionInfo {
    pub session_id: String,
    pub host_id: String,
    pub client_id: String,
    pub started_at: DateTime<Utc>,
    pub duration_sec: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayStats {
    pub active_sessions: usize,
    pub total_sessions: u64,
    pub total_bytes_up: u64,
    pub total_bytes_down: u64,
    pub total_relay_bytes: u64,
    pub pending_handshakes: usize,
    pub engine: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveState {
    Pending,
    Ready,
    Failed,
}

enum RelayStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl RelayStream {
    fn tcp_ref(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::Tls(s) => s.get_ref().0,
        }
    }
}

impl AsyncRead for RelayStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RelayStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }
}

struct MatchedPeer {
    stream: RelayStream,
    permit: Option<OwnedSemaphorePermit>,
    arriving_kick_seq: u64,
}

struct PendingPeer {
    wait_id: u64,
    role: u8,
    expected_host_id: String,
    expected_client_id: String,
    cancel: CancellationFlag,
    pair_tx: oneshot::Sender<MatchedPeer>,
}

enum PendingSlot {
    Resolving(watch::Sender<ResolveState>),
    Waiting(PendingPeer),
}

struct TaskGuard {
    server: Arc<RelayServer>,
}

impl TaskGuard {
    fn enter(server: Arc<RelayServer>) -> Self {
        server.in_flight_tasks.fetch_add(1, Ordering::AcqRel);
        Self { server }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.server.in_flight_tasks.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.server.tasks_drained.notify_waiters();
        }
    }
}

struct ActiveSessionGuard {
    server: Arc<RelayServer>,
    run_id: u64,
    active: bool,
}

impl ActiveSessionGuard {
    fn register(server: Arc<RelayServer>, run_id: u64, session: ActiveRelaySession) -> Self {
        server.active_sessions.insert(run_id, session);
        server.active_session_count.fetch_add(1, Ordering::AcqRel);
        Self {
            server,
            run_id,
            active: true,
        }
    }

    fn remove_now(&mut self) {
        if self.active {
            self.active = false;
            if self.server.active_sessions.remove(&self.run_id).is_some() {
                self.server
                    .active_session_count
                    .fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

impl Drop for ActiveSessionGuard {
    fn drop(&mut self) {
        self.remove_now();
    }
}

struct PendingResolveGuard {
    server: Arc<RelayServer>,
    token: String,
    resolve_tx: watch::Sender<ResolveState>,
    armed: bool,
}

impl PendingResolveGuard {
    fn new(
        server: Arc<RelayServer>,
        token: String,
        resolve_tx: watch::Sender<ResolveState>,
    ) -> Self {
        Self {
            server,
            token,
            resolve_tx,
            armed: true,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn into_waiting(
        mut self,
        wait_id: u64,
        role: u8,
        expected_host_id: String,
        expected_client_id: String,
        cancel: CancellationFlag,
        pair_tx: oneshot::Sender<MatchedPeer>,
    ) -> PendingWaitGuard {
        self.armed = false;
        let prev = self.server.pending.insert(
            self.token.clone(),
            PendingSlot::Waiting(PendingPeer {
                wait_id,
                role,
                expected_host_id,
                expected_client_id,
                cancel,
                pair_tx,
            }),
        );
        debug_assert!(matches!(prev, Some(PendingSlot::Resolving(_))));
        let _ = self.resolve_tx.send(ResolveState::Ready);
        PendingWaitGuard {
            server: Arc::clone(&self.server),
            token: self.token.clone(),
            wait_id,
            armed: true,
        }
    }
}

impl Drop for PendingResolveGuard {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            if self.server.pending.remove(&self.token).is_some() {
                self.server.pending_count.fetch_sub(1, Ordering::AcqRel);
            }
            let _ = self.resolve_tx.send(ResolveState::Failed);
        }
    }
}

struct PendingWaitGuard {
    server: Arc<RelayServer>,
    token: String,
    wait_id: u64,
    armed: bool,
}

impl PendingWaitGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingWaitGuard {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            let removed = self.server.pending.remove_if(
                &self.token,
                |_, slot| matches!(slot, PendingSlot::Waiting(p) if p.wait_id == self.wait_id),
            );
            if removed.is_some() {
                self.server.pending_count.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

struct ActiveRelaySession {
    session_id: String,
    host_id: String,
    client_id: String,
    started_at: DateTime<Utc>,
    bytes_up: Arc<AtomicU64>,
    bytes_down: Arc<AtomicU64>,
    cancel: CancellationFlag,
}

pub struct RelayServer {
    device_store: Arc<dyn DeviceStore>,
    ephemeral: Arc<dyn EphemeralStore>,
    config: RelayConfig,
    conn_semaphore: Arc<Semaphore>,
    shutdown: CancellationFlag,
    in_flight_tasks: AtomicUsize,
    tasks_drained: Notify,
    pending: DashMap<String, PendingSlot>,
    pending_count: AtomicUsize,
    active_sessions: DashMap<u64, ActiveRelaySession>,
    active_session_count: AtomicUsize,
    kick_seq: AtomicU64,
    device_kick_seq: DashMap<String, u64>,
    next_id: AtomicU64,
    total_sessions: AtomicU64,
    total_bytes_up: Arc<AtomicU64>,
    total_bytes_down: Arc<AtomicU64>,
}

impl RelayServer {
    pub fn new(
        device_store: Arc<dyn DeviceStore>,
        ephemeral: Arc<dyn EphemeralStore>,
        config: RelayConfig,
    ) -> Self {
        let max_connections = config.max_connections.max(1);
        Self {
            device_store,
            ephemeral,
            config,
            conn_semaphore: Arc::new(Semaphore::new(max_connections)),
            shutdown: CancellationFlag::new(),
            in_flight_tasks: AtomicUsize::new(0),
            tasks_drained: Notify::new(),
            pending: DashMap::new(),
            pending_count: AtomicUsize::new(0),
            active_sessions: DashMap::new(),
            active_session_count: AtomicUsize::new(0),
            kick_seq: AtomicU64::new(0),
            device_kick_seq: DashMap::new(),
            next_id: AtomicU64::new(1),
            total_sessions: AtomicU64::new(0),
            total_bytes_up: Arc::new(AtomicU64::new(0)),
            total_bytes_down: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn engine_name() -> &'static str {
        #[cfg(target_os = "linux")]
        {
            "linux-splice"
        }
        #[cfg(not(target_os = "linux"))]
        {
            "userspace-copy"
        }
    }

    pub fn engine(&self) -> &'static str {
        Self::engine_name()
    }

    pub fn active_sessions(&self) -> Vec<RelaySessionInfo> {
        let now = Utc::now();
        let engine = Self::engine_name().to_string();
        let mut sessions: Vec<RelaySessionInfo> = self
            .active_sessions
            .iter()
            .map(|entry| {
                let s = entry.value();
                let duration_sec = (now - s.started_at).num_seconds().max(0) as u64;
                RelaySessionInfo {
                    session_id: s.session_id.clone(),
                    host_id: s.host_id.clone(),
                    client_id: s.client_id.clone(),
                    started_at: s.started_at,
                    duration_sec,
                    bytes_up: s.bytes_up.load(Ordering::Relaxed),
                    bytes_down: s.bytes_down.load(Ordering::Relaxed),
                    engine: engine.clone(),
                }
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        sessions
    }

    pub fn in_session_device_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        for entry in self.active_sessions.iter() {
            let s = entry.value();
            ids.insert(s.host_id.clone());
            ids.insert(s.client_id.clone());
        }
        ids
    }

    pub fn is_device_in_session(&self, device_id: &str) -> bool {
        let norm = normalize_device_id(device_id);
        self.active_sessions
            .iter()
            .any(|entry| entry.host_id == norm || entry.client_id == norm)
    }

    /// Force-terminate all active relay sessions matching `session_id`.
    pub fn kick_session(&self, session_id: &str) -> bool {
        let mut kicked = false;
        for entry in self.active_sessions.iter() {
            if entry.session_id == session_id {
                entry.cancel.cancel();
                kicked = true;
            }
        }
        kicked
    }

    /// Force-terminate all active AND pending waiting relay sessions involving `device_id`,
    /// recording a monotonic per-device kick watermark so in-flight handoffs cannot slip through.
    pub fn kick_device_sessions(&self, device_id: &str) -> usize {
        let norm = normalize_device_id(device_id);
        let seq = self.kick_seq.fetch_add(1, Ordering::AcqRel) + 1;
        self.device_kick_seq
            .entry(norm.clone())
            .and_modify(|v| *v = (*v).max(seq))
            .or_insert(seq);
        let mut kicked = 0usize;
        for entry in self.active_sessions.iter() {
            if entry.host_id == norm || entry.client_id == norm {
                entry.cancel.cancel();
                kicked += 1;
            }
        }
        for entry in self.pending.iter() {
            if let PendingSlot::Waiting(ref peer) = *entry.value() {
                if peer.expected_host_id == norm || peer.expected_client_id == norm {
                    peer.cancel.cancel();
                    kicked += 1;
                }
            }
        }
        kicked
    }

    fn was_kicked_since(&self, host_id: &str, client_id: &str, since_seq: u64) -> bool {
        self.device_kick_seq
            .get(host_id)
            .is_some_and(|v| *v > since_seq)
            || self
                .device_kick_seq
                .get(client_id)
                .is_some_and(|v| *v > since_seq)
    }

    /// O(1) strictly monotonic relay statistics snapshot.
    pub fn stats(&self) -> RelayStats {
        let total_bytes_up = self.total_bytes_up.load(Ordering::Relaxed);
        let total_bytes_down = self.total_bytes_down.load(Ordering::Relaxed);

        RelayStats {
            active_sessions: self.active_session_count.load(Ordering::Relaxed),
            total_sessions: self.total_sessions.load(Ordering::Relaxed),
            total_bytes_up,
            total_bytes_down,
            total_relay_bytes: total_bytes_up.saturating_add(total_bytes_down),
            pending_handshakes: self.pending_count.load(Ordering::Relaxed),
            engine: Self::engine_name().to_string(),
        }
    }

    /// Cancel accepting new connections, terminate all pending handshakes and active relay
    /// sessions, and drain all spawned connection tasks cleanly.
    pub async fn shutdown_all(&self) {
        self.shutdown.cancel();
        for entry in self.active_sessions.iter() {
            entry.cancel.cancel();
        }
        loop {
            let notified = self.tasks_drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.in_flight_tasks.load(Ordering::Acquire) == 0 {
                break;
            }
            for entry in self.active_sessions.iter() {
                entry.cancel.cancel();
            }
            notified.await;
        }
    }

    fn try_reserve_pending_slot(&self) -> bool {
        let mut current = self.pending_count.load(Ordering::Acquire);
        loop {
            if current >= self.config.max_pending {
                return false;
            }
            match self.pending_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Serve TCP relay connections on a pre-bound `TcpListener` until `shutdown_all()` is invoked.
    pub async fn serve_on_listener(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        self.serve_on_listener_inner(listener, None, None).await
    }

    /// Serve TCP relay connections on a pre-bound `TcpListener`, exiting cleanly when either
    /// `shutdown` transitions to `true` (or its sender drops) or `shutdown_all()` is invoked.
    pub async fn serve_on_listener_with_shutdown(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        self.serve_on_listener_inner(listener, None, Some(shutdown))
            .await
    }

    /// Serve mandatory TLS 1.3 mTLS relay connections on a pre-bound `TcpListener` until `shutdown_all()`.
    pub async fn serve_tls_on_listener(
        self: Arc<Self>,
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
        auth_store: Arc<dyn AuthStore>,
    ) -> io::Result<()> {
        self.serve_on_listener_inner(
            listener,
            Some((TlsAcceptor::from(tls_config), auth_store)),
            None,
        )
        .await
    }

    /// Serve mandatory TLS 1.3 mTLS relay connections on a pre-bound `TcpListener`, exiting cleanly
    /// when either `shutdown` transitions to `true` (or its sender drops) or `shutdown_all()` is invoked.
    pub async fn serve_tls_on_listener_with_shutdown(
        self: Arc<Self>,
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
        auth_store: Arc<dyn AuthStore>,
        shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        self.serve_on_listener_inner(
            listener,
            Some((TlsAcceptor::from(tls_config), auth_store)),
            Some(shutdown),
        )
        .await
    }

    async fn serve_on_listener_inner(
        self: Arc<Self>,
        listener: TcpListener,
        tls_ctx: Option<(TlsAcceptor, Arc<dyn AuthStore>)>,
        mut shutdown: Option<watch::Receiver<bool>>,
    ) -> io::Result<()> {
        let server_shutdown = self.shutdown.cancelled();
        tokio::pin!(server_shutdown);
        let external_shutdown = wait_external_shutdown(&mut shutdown);
        tokio::pin!(external_shutdown);

        loop {
            if self.shutdown.is_cancelled() {
                return Ok(());
            }

            // Acquire a connection permit before accept() so floods cannot exhaust file descriptors.
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
                    warn!(error = %err, "TCP relay accept error; backing off 50ms");
                    tokio::select! {
                        biased;
                        _ = &mut server_shutdown => return Ok(()),
                        _ = &mut external_shutdown => return Ok(()),
                        _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                    }
                    continue;
                }
            };

            let server = Arc::clone(&self);
            let task_guard = TaskGuard::enter(Arc::clone(&self));
            let tls_ctx = tls_ctx.clone();
            tokio::spawn(async move {
                let _task_guard = task_guard;
                if let Some((acceptor, auth_store)) = tls_ctx {
                    server
                        .handle_tls_connection_inner(
                            stream,
                            peer_addr,
                            acceptor,
                            auth_store,
                            Some(permit),
                        )
                        .await;
                } else {
                    tune_socket(&stream, peer_addr);
                    let connect_kick_seq = server.kick_seq.load(Ordering::Acquire);
                    server
                        .handle_relay_stream_inner(
                            RelayStream::Plain(stream),
                            peer_addr,
                            Some(permit),
                            None,
                            connect_kick_seq,
                        )
                        .await;
                }
            });
        }
    }

    /// Bind a TCP listener on `addr` and serve `RCR1` relay connections.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, engine = Self::engine_name(), "TCP relay listener started");
        self.serve_on_listener(listener).await
    }

    pub async fn handle_connection(self: Arc<Self>, stream: TcpStream, peer_addr: SocketAddr) {
        let _task_guard = TaskGuard::enter(Arc::clone(&self));
        tune_socket(&stream, peer_addr);
        let connect_kick_seq = self.kick_seq.load(Ordering::Acquire);
        self.handle_relay_stream_inner(
            RelayStream::Plain(stream),
            peer_addr,
            None,
            None,
            connect_kick_seq,
        )
        .await;
    }

    async fn handle_tls_connection_inner(
        self: Arc<Self>,
        stream: TcpStream,
        peer_addr: SocketAddr,
        acceptor: TlsAcceptor,
        auth_store: Arc<dyn AuthStore>,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let connect_kick_seq = self.kick_seq.load(Ordering::Acquire);
        tune_socket(&stream, peer_addr);

        let tls_res = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => return,
            res = tokio::time::timeout(self.config.handshake_timeout, acceptor.accept(stream)) => res,
        };
        let tls_stream = match tls_res {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                debug!(%peer_addr, error = %err, "mTLS relay handshake failed");
                return;
            }
            Err(_) => {
                debug!(%peer_addr, "mTLS relay handshake timed out");
                return;
            }
        };

        let Some(leaf_der) = tls_stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|c| c.as_ref().to_vec())
        else {
            debug!(%peer_addr, "mTLS relay peer presented no client certificate");
            return;
        };

        let identity = match parse_peer_cert_identity(&leaf_der) {
            Ok(id) => id,
            Err(err) => {
                debug!(%peer_addr, error = %err, "mTLS relay peer certificate identity invalid");
                return;
            }
        };

        match self.ephemeral.is_cert_revoked(&identity.serial_hex).await {
            Ok(true) => {
                debug!(%peer_addr, serial = %identity.serial_hex, "rejecting revoked relay cert (ephemeral cache)");
                return;
            }
            Ok(false) => {}
            Err(err) => {
                warn!(%peer_addr, error = %err, "failed checking ephemeral cert revocation; falling back to store");
            }
        }

        match auth_store.get_device_cert(&identity.serial_hex).await {
            Ok(Some(cert_rec)) => {
                if cert_rec.revoked_at.is_some()
                    || cert_rec.expires_at <= Utc::now()
                    || cert_rec.user_id != identity.user_id
                    || cert_rec.device_id != identity.device_id
                {
                    debug!(%peer_addr, serial = %identity.serial_hex, "rejecting revoked/expired/mismatched relay cert");
                    return;
                }
            }
            Ok(None) => {
                debug!(%peer_addr, serial = %identity.serial_hex, "rejecting unknown relay cert");
                return;
            }
            Err(err) => {
                warn!(%peer_addr, error = %err, "failed querying device cert from store");
                return;
            }
        }

        match auth_store.get_user_by_id(identity.user_id).await {
            Ok(Some(user)) if user.status == STATUS_ACTIVE => {}
            Ok(_) => {
                debug!(%peer_addr, user_id = %identity.user_id, "rejecting relay cert for inactive/missing user");
                return;
            }
            Err(err) => {
                warn!(%peer_addr, error = %err, "failed querying user for mTLS relay");
                return;
            }
        }

        match auth_store.get_by_id(&identity.device_id).await {
            Ok(Some(dev)) => {
                if dev.disabled || dev.owner_user_id != Some(identity.user_id) {
                    debug!(%peer_addr, device_id = %identity.device_id, "rejecting relay cert for disabled/unbound device");
                    return;
                }
            }
            Ok(None) => {
                debug!(%peer_addr, device_id = %identity.device_id, "rejecting relay cert for missing device");
                return;
            }
            Err(err) => {
                warn!(%peer_addr, error = %err, "failed querying device for mTLS relay");
                return;
            }
        }

        self.handle_relay_stream_inner(
            RelayStream::Tls(Box::new(tls_stream)),
            peer_addr,
            permit,
            Some(identity.device_id),
            connect_kick_seq,
        )
        .await;
    }

    async fn handle_relay_stream_inner(
        self: Arc<Self>,
        mut stream: RelayStream,
        peer_addr: SocketAddr,
        permit: Option<OwnedSemaphorePermit>,
        verified_device_id: Option<String>,
        connect_kick_seq: u64,
    ) {
        // Step 1: Read RCR1 binary handshake within handshake_timeout.
        let handshake = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                debug!(%peer_addr, "relay handshake cancelled by shutdown");
                send_reply_best_effort(&mut stream, REPLY_ERR).await;
                return;
            }
            res = tokio::time::timeout(self.config.handshake_timeout, read_handshake(&mut stream)) => res,
        };
        let (role, token) = match handshake {
            Ok(Ok(parsed)) => parsed,
            Ok(Err(err)) => {
                debug!(%peer_addr, error = %err, "relay handshake failed");
                send_reply_best_effort(&mut stream, REPLY_ERR).await;
                return;
            }
            Err(_) => {
                debug!(%peer_addr, "relay handshake timed out");
                send_reply_best_effort(&mut stream, REPLY_ERR).await;
                return;
            }
        };

        // Step 2: Validate token & pair with complementary role without race conditions.
        enum PairingAction {
            WaitResolve(watch::Receiver<ResolveState>),
            ResolveToken(PendingResolveGuard),
            Matched(PendingPeer),
            Reject(&'static str),
        }

        loop {
            if self.shutdown.is_cancelled() {
                send_reply_best_effort(&mut stream, REPLY_ERR).await;
                return;
            }

            let action = match self.pending.entry(token.clone()) {
                Entry::Vacant(vacant) => {
                    if !self.try_reserve_pending_slot() {
                        PairingAction::Reject("pending_full")
                    } else {
                        let (tx, _rx) = watch::channel(ResolveState::Pending);
                        vacant.insert(PendingSlot::Resolving(tx.clone()));
                        PairingAction::ResolveToken(PendingResolveGuard::new(
                            Arc::clone(&self),
                            token.clone(),
                            tx,
                        ))
                    }
                }
                Entry::Occupied(occ) => match occ.get() {
                    PendingSlot::Resolving(tx) => PairingAction::WaitResolve(tx.subscribe()),
                    PendingSlot::Waiting(peer) => {
                        if peer.cancel.is_cancelled() {
                            let _ = occ.remove();
                            self.pending_count.fetch_sub(1, Ordering::AcqRel);
                            PairingAction::Reject("device_kicked")
                        } else if peer.role == role {
                            PairingAction::Reject("duplicate_role")
                        } else if let Some(ref cert_did) = verified_device_id {
                            let expected = if role == ROLE_HOST {
                                &peer.expected_host_id
                            } else {
                                &peer.expected_client_id
                            };
                            if cert_did != expected {
                                PairingAction::Reject("cert_device_id_mismatch")
                            } else {
                                match occ.remove() {
                                    PendingSlot::Waiting(peer) => {
                                        self.pending_count.fetch_sub(1, Ordering::AcqRel);
                                        PairingAction::Matched(peer)
                                    }
                                    PendingSlot::Resolving(_) => unreachable!(),
                                }
                            }
                        } else {
                            match occ.remove() {
                                PendingSlot::Waiting(peer) => {
                                    self.pending_count.fetch_sub(1, Ordering::AcqRel);
                                    PairingAction::Matched(peer)
                                }
                                PendingSlot::Resolving(_) => unreachable!(),
                            }
                        }
                    }
                },
            };

            match action {
                PairingAction::Reject(reason) => {
                    debug!(%peer_addr, role, reason, "rejecting relay peer");
                    send_reply_best_effort(&mut stream, REPLY_ERR).await;
                    return;
                }

                PairingAction::WaitResolve(mut rx) => {
                    let state = tokio::select! {
                        biased;
                        _ = self.shutdown.cancelled() => None,
                        res = tokio::time::timeout(
                            self.config.handshake_timeout,
                            rx.wait_for(|state| !matches!(state, ResolveState::Pending)),
                        ) => res.ok().and_then(|r| r.ok().map(|guard| *guard)),
                    };
                    match state {
                        Some(ResolveState::Ready) => {
                            // First peer finished validating and placed Waiting; loop to pair.
                            continue;
                        }
                        _ => {
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                    }
                }

                PairingAction::Matched(peer) => {
                    match peer.pair_tx.send(MatchedPeer {
                        stream,
                        permit,
                        arriving_kick_seq: connect_kick_seq,
                    }) {
                        Ok(()) => {
                            // Stream and permit transferred to the waiting peer's task; we are done.
                            return;
                        }
                        Err(mut returned) => {
                            // Waiting peer exited right before handoff.
                            send_reply_best_effort(&mut returned.stream, REPLY_ERR).await;
                            return;
                        }
                    }
                }

                PairingAction::ResolveToken(resolve_guard) => {
                    let lookup = tokio::select! {
                        biased;
                        _ = self.shutdown.cancelled() => {
                            drop(resolve_guard);
                            debug!(%peer_addr, "relay token lookup cancelled by shutdown");
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                        res = tokio::time::timeout(
                            self.config.handshake_timeout,
                            self.ephemeral.take_relay_token(&token),
                        ) => res,
                    };
                    let record = match lookup {
                        Ok(Ok(Some(rec))) => rec,
                        Ok(Ok(None)) => {
                            drop(resolve_guard);
                            debug!(%peer_addr, "unknown or expired relay token");
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                        Ok(Err(err)) => {
                            drop(resolve_guard);
                            warn!(%peer_addr, error = %err, "ephemeral store error during relay token lookup");
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                        Err(_) => {
                            drop(resolve_guard);
                            warn!(%peer_addr, "ephemeral store timed out during relay token lookup");
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                    };

                    if let Some(ref cert_did) = verified_device_id {
                        let expected = if role == ROLE_HOST {
                            &record.host_id
                        } else {
                            &record.client_id
                        };
                        if cert_did != expected {
                            drop(resolve_guard);
                            debug!(
                                %peer_addr,
                                role,
                                cert_device_id = %cert_did,
                                expected_device_id = %expected,
                                "rejecting relay peer due to certificate device_id mismatch"
                            );
                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                            return;
                        }
                    }

                    if self.was_kicked_since(&record.host_id, &record.client_id, connect_kick_seq) {
                        drop(resolve_guard);
                        debug!(%peer_addr, session_id = %record.session_id, "rejecting relay peer kicked during token resolve");
                        send_reply_best_effort(&mut stream, REPLY_ERR).await;
                        return;
                    }

                    let wait_cancel = CancellationFlag::new();
                    let matched = {
                        let wait_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                        let (pair_tx, mut pair_rx) = oneshot::channel();
                        let mut wait_guard = resolve_guard.into_waiting(
                            wait_id,
                            role,
                            record.host_id.clone(),
                            record.client_id.clone(),
                            wait_cancel.clone(),
                            pair_tx,
                        );

                        let deadline = tokio::time::sleep(self.config.pair_timeout);
                        tokio::pin!(deadline);
                        let shutdown_wait = self.shutdown.cancelled();
                        tokio::pin!(shutdown_wait);
                        let kicked_wait = wait_cancel.cancelled();
                        tokio::pin!(kicked_wait);
                        let mut peek_buf = [0u8; 1];
                        let mut monitor_peer = true;

                        loop {
                            tokio::select! {
                                biased;
                                _ = &mut kicked_wait => {
                                    drop(wait_guard);
                                    pair_rx.close();
                                    if let Ok(mut orphan) = pair_rx.try_recv() {
                                        send_reply_best_effort(&mut orphan.stream, REPLY_ERR).await;
                                    }
                                    debug!(%peer_addr, session_id = %record.session_id, "waiting relay peer kicked");
                                    send_reply_best_effort(&mut stream, REPLY_ERR).await;
                                    return;
                                }
                                res = &mut pair_rx => {
                                    match res {
                                        Ok(other) => {
                                            wait_guard.disarm();
                                            break other;
                                        }
                                        Err(_) => {
                                            drop(wait_guard);
                                            send_reply_best_effort(&mut stream, REPLY_ERR).await;
                                            return;
                                        }
                                    }
                                }
                                _ = &mut shutdown_wait => {
                                    drop(wait_guard);
                                    pair_rx.close();
                                    if let Ok(mut orphan) = pair_rx.try_recv() {
                                        send_reply_best_effort(&mut orphan.stream, REPLY_ERR).await;
                                    }
                                    debug!(%peer_addr, session_id = %record.session_id, "relay peer-wait cancelled by shutdown");
                                    send_reply_best_effort(&mut stream, REPLY_ERR).await;
                                    return;
                                }
                                _ = &mut deadline => {
                                    drop(wait_guard);
                                    pair_rx.close();
                                    if let Ok(mut orphan) = pair_rx.try_recv() {
                                        send_reply_best_effort(&mut orphan.stream, REPLY_ERR).await;
                                    }
                                    debug!(%peer_addr, session_id = %record.session_id, "relay peer-wait timed out");
                                    send_reply_best_effort(&mut stream, REPLY_ERR).await;
                                    return;
                                }
                                peek_res = stream.tcp_ref().peek(&mut peek_buf), if monitor_peer => {
                                    match peek_res {
                                        Ok(0) => {
                                            drop(wait_guard);
                                            pair_rx.close();
                                            if let Ok(mut orphan) = pair_rx.try_recv() {
                                                send_reply_best_effort(&mut orphan.stream, REPLY_ERR).await;
                                            }
                                            debug!(%peer_addr, session_id = %record.session_id, "waiting relay peer disconnected (EOF)");
                                            return;
                                        }
                                        Ok(_) => {
                                            // Peer pipelined payload bytes before REPLY_OK; preserve them
                                            // untouched and stop polling peek to avoid busy-spinning.
                                            monitor_peer = false;
                                        }
                                        Err(err)
                                            if matches!(
                                                err.kind(),
                                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                            ) =>
                                        {
                                            continue;
                                        }
                                        Err(err) => {
                                            drop(wait_guard);
                                            pair_rx.close();
                                            if let Ok(mut orphan) = pair_rx.try_recv() {
                                                send_reply_best_effort(&mut orphan.stream, REPLY_ERR).await;
                                            }
                                            debug!(%peer_addr, session_id = %record.session_id, error = %err, "waiting relay peer disconnected");
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    };

                    let pair_kick_seq = connect_kick_seq.min(matched.arriving_kick_seq);
                    self.run_paired_session(
                        record,
                        role,
                        stream,
                        matched.stream,
                        permit,
                        matched.permit,
                        wait_cancel,
                        pair_kick_seq,
                    )
                    .await;
                    return;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_paired_session(
        self: Arc<Self>,
        record: RelayTokenRecord,
        waiting_role: u8,
        mut waiting_stream: RelayStream,
        mut arriving_stream: RelayStream,
        waiting_permit: Option<OwnedSemaphorePermit>,
        arriving_permit: Option<OwnedSemaphorePermit>,
        cancel: CancellationFlag,
        connect_kick_seq: u64,
    ) {
        if self.shutdown.is_cancelled()
            || cancel.is_cancelled()
            || self.was_kicked_since(&record.host_id, &record.client_id, connect_kick_seq)
        {
            let _ = tokio::join!(
                send_reply_best_effort(&mut waiting_stream, REPLY_ERR),
                send_reply_best_effort(&mut arriving_stream, REPLY_ERR),
            );
            return;
        }

        // Register in `active_sessions` BEFORE delivering REPLY_OK and re-check the monotonic
        // kick watermark so no concurrent `kick_device_sessions` can miss the Pending->Active transition.
        let started_at = Utc::now();
        let run_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes_up = Arc::new(AtomicU64::new(0));
        let bytes_down = Arc::new(AtomicU64::new(0));
        let mut active_guard = ActiveSessionGuard::register(
            Arc::clone(&self),
            run_id,
            ActiveRelaySession {
                session_id: record.session_id.clone(),
                host_id: record.host_id.clone(),
                client_id: record.client_id.clone(),
                started_at,
                bytes_up: Arc::clone(&bytes_up),
                bytes_down: Arc::clone(&bytes_down),
                cancel: cancel.clone(),
            },
        );
        if self.shutdown.is_cancelled()
            || cancel.is_cancelled()
            || self.was_kicked_since(&record.host_id, &record.client_id, connect_kick_seq)
        {
            active_guard.remove_now();
            let _ = tokio::join!(
                send_reply_best_effort(&mut waiting_stream, REPLY_ERR),
                send_reply_best_effort(&mut arriving_stream, REPLY_ERR),
            );
            return;
        }

        // Verify the waiting peer is not already broken and deliver its 0x00 OK first so that,
        // if Peer 1 disconnected right at handoff, Peer 2 receives REPLY_ERR instead of a phantom session.
        if is_stream_disconnected(waiting_stream.tcp_ref()) {
            debug!(
                session_id = %record.session_id,
                "waiting peer already disconnected before REPLY_OK; notifying surviving peer"
            );
            active_guard.remove_now();
            send_reply_best_effort(&mut arriving_stream, REPLY_ERR).await;
            return;
        }

        let waiting_ok = tokio::time::timeout(
            self.config.handshake_timeout,
            write_reply(&mut waiting_stream, REPLY_OK),
        )
        .await;
        if !matches!(waiting_ok, Ok(Ok(()))) {
            debug!(
                session_id = %record.session_id,
                "failed to deliver 0x00 relay OK reply to waiting peer; notifying surviving peer"
            );
            active_guard.remove_now();
            send_reply_best_effort(&mut arriving_stream, REPLY_ERR).await;
            return;
        }

        let arriving_ok = tokio::time::timeout(
            self.config.handshake_timeout,
            write_reply(&mut arriving_stream, REPLY_OK),
        )
        .await;
        if !matches!(arriving_ok, Ok(Ok(()))) {
            debug!(
                session_id = %record.session_id,
                "failed to deliver 0x00 relay OK reply to arriving peer"
            );
            active_guard.remove_now();
            return;
        }

        if self.shutdown.is_cancelled() || cancel.is_cancelled() {
            cancel.cancel();
            active_guard.remove_now();
            return;
        }

        let (mut host_stream, mut client_stream) = if waiting_role == ROLE_HOST {
            (waiting_stream, arriving_stream)
        } else {
            (arriving_stream, waiting_stream)
        };

        self.total_sessions.fetch_add(1, Ordering::Relaxed);

        info!(
            session_id = %record.session_id,
            host_id = %record.host_id,
            client_id = %record.client_id,
            engine = Self::engine_name(),
            "relay session paired and started"
        );

        match (&mut host_stream, &mut client_stream) {
            (RelayStream::Plain(host_tcp), RelayStream::Plain(client_tcp)) => {
                #[cfg(target_os = "linux")]
                {
                    if let Err(err) = splice::splice_bidirectional(
                        host_tcp,
                        client_tcp,
                        Arc::clone(&bytes_up),
                        Arc::clone(&bytes_down),
                        Arc::clone(&self.total_bytes_up),
                        Arc::clone(&self.total_bytes_down),
                        cancel.clone(),
                    )
                    .await
                    {
                        warn!(
                            session_id = %record.session_id,
                            error = %err,
                            "linux splice(2) setup failed; falling back to userspace copy"
                        );
                        let _ = copy_bidirectional_userspace_with_totals(
                            host_tcp,
                            client_tcp,
                            Arc::clone(&bytes_up),
                            Arc::clone(&bytes_down),
                            Some(Arc::clone(&self.total_bytes_up)),
                            Some(Arc::clone(&self.total_bytes_down)),
                            cancel.clone(),
                        )
                        .await;
                    }
                }

                #[cfg(not(target_os = "linux"))]
                {
                    let _ = copy_bidirectional_userspace_with_totals(
                        host_tcp,
                        client_tcp,
                        Arc::clone(&bytes_up),
                        Arc::clone(&bytes_down),
                        Some(Arc::clone(&self.total_bytes_up)),
                        Some(Arc::clone(&self.total_bytes_down)),
                        cancel.clone(),
                    )
                    .await;
                }
            }
            (host, client) => {
                let _ = copy_bidirectional_streams_with_totals(
                    host,
                    client,
                    Arc::clone(&bytes_up),
                    Arc::clone(&bytes_down),
                    Some(Arc::clone(&self.total_bytes_up)),
                    Some(Arc::clone(&self.total_bytes_down)),
                    cancel.clone(),
                )
                .await;
            }
        }

        // Drop streams and connection permits promptly so peers observe closure immediately before DB logging.
        drop(host_stream);
        drop(client_stream);
        drop(waiting_permit);
        drop(arriving_permit);

        let ended_at = Utc::now();
        let final_up = bytes_up.load(Ordering::Relaxed);
        let final_down = bytes_down.load(Ordering::Relaxed);

        active_guard.remove_now();

        info!(
            session_id = %record.session_id,
            host_id = %record.host_id,
            client_id = %record.client_id,
            bytes_up = final_up,
            bytes_down = final_down,
            "relay session finished"
        );

        if let Err(err) = self
            .device_store
            .record_relay_end(
                &record.session_id,
                &record.host_id,
                &record.client_id,
                final_up,
                final_down,
                started_at,
                ended_at,
            )
            .await
        {
            warn!(
                session_id = %record.session_id,
                error = %err,
                "failed to persist relay session log"
            );
        }
    }
}

async fn wait_external_shutdown(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => {
            if *rx.borrow_and_update() {
                return;
            }
            while rx.changed().await.is_ok() {
                if *rx.borrow_and_update() {
                    return;
                }
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Non-blocking check whether a non-blocking TCP peer has already sent FIN (`Ok(0)`) or
/// encountered a hard socket error, without consuming any pipelined payload bytes.
fn is_stream_disconnected(stream: &TcpStream) -> bool {
    let sock = SockRef::from(stream);
    let mut buf = [MaybeUninit::<u8>::uninit(); 1];
    loop {
        match sock.peek(&mut buf) {
            Ok(0) => return true,
            Ok(_) => return false,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return false,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        }
    }
}

fn tune_socket(stream: &TcpStream, peer_addr: SocketAddr) {
    let sock = SockRef::from(stream);
    if let Err(err) = sock.set_tcp_nodelay(true) {
        debug!(%peer_addr, error = %err, "failed to set TCP_NODELAY on relay socket");
    }
    if let Err(err) = sock.set_keepalive(true) {
        debug!(%peer_addr, error = %err, "failed to set SO_KEEPALIVE on relay socket");
    }
    let keepalive = TcpKeepalive::new().with_time(TCP_KEEPALIVE_TIME);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let keepalive = keepalive
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    if let Err(err) = sock.set_tcp_keepalive(&keepalive) {
        debug!(%peer_addr, error = %err, "failed to configure TCP keepalive parameters");
    }
    // On Linux, skip fixed SO_RCVBUF/SO_SNDBUF so kernel TCP autotuning stays enabled and
    // TCP_NOTSENT_LOWAT=1MiB in splice.rs is not capped by a fixed 256 KiB send buffer.
    #[cfg(not(target_os = "linux"))]
    {
        if let Err(err) = sock.set_recv_buffer_size(SOCKET_BUF_SIZE) {
            debug!(%peer_addr, error = %err, "failed to set SO_RCVBUF on relay socket");
        }
        if let Err(err) = sock.set_send_buffer_size(SOCKET_BUF_SIZE) {
            debug!(%peer_addr, error = %err, "failed to set SO_SNDBUF on relay socket");
        }
    }
}

async fn read_handshake<S: AsyncRead + Unpin>(stream: &mut S) -> Result<(u8, String), RelayError> {
    let mut header = [0u8; 6];
    stream.read_exact(&mut header).await?;
    if &header[0..4] != RELAY_MAGIC {
        return Err(RelayError::InvalidMagic);
    }
    let role = header[4];
    if role != ROLE_HOST && role != ROLE_CLIENT {
        return Err(RelayError::InvalidRole(role));
    }
    let token_len = header[5] as usize;
    if token_len == 0 || token_len > MAX_TOKEN_LEN {
        return Err(RelayError::InvalidTokenLength(token_len));
    }
    let mut token_buf = vec![0u8; token_len];
    stream.read_exact(&mut token_buf).await?;
    let token = String::from_utf8(token_buf).map_err(|_| RelayError::InvalidTokenUtf8)?;
    Ok((role, token))
}

async fn write_reply<S: AsyncWrite + Unpin>(stream: &mut S, code: u8) -> io::Result<()> {
    stream.write_all(&[code]).await?;
    stream.flush().await
}

async fn send_reply_best_effort<S: AsyncWrite + Unpin>(stream: &mut S, code: u8) {
    let _ = write_reply(stream, code).await;
    let _ = stream.shutdown().await;
}

/// Bounded 32 KiB userspace bidirectional copy fallback with half-close propagation and
/// immediate cancellation wakeup (used on non-Linux targets or if Linux `splice(2)` setup fails).
pub async fn copy_bidirectional_userspace(
    a: &mut TcpStream,
    b: &mut TcpStream,
    bytes_a_to_b: Arc<AtomicU64>,
    bytes_b_to_a: Arc<AtomicU64>,
    cancel: CancellationFlag,
) -> io::Result<()> {
    copy_bidirectional_userspace_with_totals(a, b, bytes_a_to_b, bytes_b_to_a, None, None, cancel)
        .await
}

async fn copy_bidirectional_userspace_with_totals(
    a: &mut TcpStream,
    b: &mut TcpStream,
    bytes_a_to_b: Arc<AtomicU64>,
    bytes_b_to_a: Arc<AtomicU64>,
    total_a_to_b: Option<Arc<AtomicU64>>,
    total_b_to_a: Option<Arc<AtomicU64>>,
    cancel: CancellationFlag,
) -> io::Result<()> {
    let (mut a_read, mut a_write) = a.split();
    let (mut b_read, mut b_write) = b.split();

    let ab = copy_one_dir(
        &mut a_read,
        &mut b_write,
        bytes_a_to_b,
        total_a_to_b,
        cancel.clone(),
    );
    let ba = copy_one_dir(
        &mut b_read,
        &mut a_write,
        bytes_b_to_a,
        total_b_to_a,
        cancel,
    );

    let _ = tokio::join!(ab, ba);
    Ok(())
}

async fn copy_bidirectional_streams_with_totals<S>(
    a: &mut S,
    b: &mut S,
    bytes_a_to_b: Arc<AtomicU64>,
    bytes_b_to_a: Arc<AtomicU64>,
    total_a_to_b: Option<Arc<AtomicU64>>,
    total_b_to_a: Option<Arc<AtomicU64>>,
    cancel: CancellationFlag,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);

    let ab = copy_one_dir(
        &mut a_read,
        &mut b_write,
        bytes_a_to_b,
        total_a_to_b,
        cancel.clone(),
    );
    let ba = copy_one_dir(
        &mut b_read,
        &mut a_write,
        bytes_b_to_a,
        total_b_to_a,
        cancel,
    );

    let _ = tokio::join!(ab, ba);
    Ok(())
}

async fn copy_one_dir<R, W>(
    reader: &mut R,
    writer: &mut W,
    counter: Arc<AtomicU64>,
    total_counter: Option<Arc<AtomicU64>>,
    cancel: CancellationFlag,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; USERSPACE_COPY_BUF_SIZE];
    let mut batch_bytes: usize = 0;

    let cancel_wait = cancel.cancelled();
    tokio::pin!(cancel_wait);

    loop {
        if cancel.is_cancelled() {
            let _ = writer.shutdown().await;
            return;
        }

        let read_res = tokio::select! {
            biased;
            _ = &mut cancel_wait => {
                let _ = writer.shutdown().await;
                return;
            }
            res = reader.read(&mut buf) => res,
        };

        let n = match read_res {
            Ok(0) => {
                // Clean read EOF: propagate TCP half-close to peer write side only.
                let _ = writer.shutdown().await;
                return;
            }
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                // Hard socket read error (e.g. ECONNRESET): tear down both directions promptly.
                cancel.cancel();
                let _ = writer.shutdown().await;
                return;
            }
        };

        let write_res = tokio::select! {
            biased;
            _ = &mut cancel_wait => {
                let _ = writer.shutdown().await;
                return;
            }
            res = writer.write_all(&buf[..n]) => res,
        };

        if write_res.is_err() {
            // Hard socket write error (e.g. EPIPE / ECONNRESET): tear down both directions promptly.
            cancel.cancel();
            let _ = writer.shutdown().await;
            return;
        }

        // Flush underlying sink so TLS writers (rustls) push encrypted records out immediately
        // instead of stalling inside internal TLS buffers under TCP backpressure.
        let flush_res = tokio::select! {
            biased;
            _ = &mut cancel_wait => {
                let _ = writer.shutdown().await;
                return;
            }
            res = writer.flush() => res,
        };

        if flush_res.is_err() {
            cancel.cancel();
            let _ = writer.shutdown().await;
            return;
        }

        let delta = n as u64;
        counter.fetch_add(delta, Ordering::Relaxed);
        if let Some(ref total) = total_counter {
            total.fetch_add(delta, Ordering::Relaxed);
        }
        batch_bytes += n;
        if batch_bytes >= USERSPACE_YIELD_BYTES {
            tokio::task::yield_now().await;
            batch_bytes = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ephemeral::MemoryEphemeralStore;
    use crate::store::MemoryDeviceStore;

    #[test]
    fn handshake_encoding_validation() {
        let frame = encode_relay_handshake(ROLE_HOST, "abc123").expect("valid");
        assert_eq!(&frame[0..4], b"RCR1");
        assert_eq!(frame[4], ROLE_HOST);
        assert_eq!(frame[5], 6);
        assert_eq!(&frame[6..], b"abc123");

        assert!(matches!(
            encode_relay_handshake(0, "abc"),
            Err(RelayError::InvalidRole(0))
        ));
        assert!(matches!(
            encode_relay_handshake(ROLE_CLIENT, ""),
            Err(RelayError::InvalidTokenLength(0))
        ));
        assert!(matches!(
            encode_relay_handshake(ROLE_CLIENT, &"a".repeat(MAX_TOKEN_LEN + 1)),
            Err(RelayError::InvalidTokenLength(_))
        ));
    }

    #[tokio::test]
    async fn relay_pairs_transfers_and_records_log() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store.clone(),
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-unit-1",
                RelayTokenRecord {
                    session_id: "sess-unit-1".into(),
                    host_id: "111222333".into(),
                    client_id: "444555666".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut host = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();

        host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-unit-1").unwrap())
            .await
            .unwrap();
        client
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-unit-1").unwrap())
            .await
            .unwrap();

        let mut h_reply = [0u8; 1];
        let mut c_reply = [0u8; 1];
        host.read_exact(&mut h_reply).await.unwrap();
        client.read_exact(&mut c_reply).await.unwrap();
        assert_eq!(h_reply[0], REPLY_OK);
        assert_eq!(c_reply[0], REPLY_OK);

        // Bidirectional small payload + half-close.
        host.write_all(b"hello-from-host").await.unwrap();
        host.shutdown().await.unwrap();
        client.write_all(b"hello-from-client").await.unwrap();
        client.shutdown().await.unwrap();

        let mut got_at_client = Vec::new();
        let mut got_at_host = Vec::new();
        client.read_to_end(&mut got_at_client).await.unwrap();
        host.read_to_end(&mut got_at_host).await.unwrap();

        assert_eq!(got_at_client, b"hello-from-host");
        assert_eq!(got_at_host, b"hello-from-client");

        // Wait briefly for relay session cleanup and DB log persistence.
        for _ in 0..50 {
            if !store.relay_logs().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let logs = store.relay_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].session_id, "sess-unit-1");
        assert_eq!(logs[0].bytes_up, b"hello-from-host".len() as u64);
        assert_eq!(logs[0].bytes_down, b"hello-from-client".len() as u64);

        let stats = relay.stats();
        assert_eq!(stats.active_sessions, 0);
        assert_eq!(stats.total_sessions, 1);
        assert_eq!(
            stats.total_relay_bytes,
            (b"hello-from-host".len() + b"hello-from-client".len()) as u64
        );

        server_task.abort();
    }

    #[tokio::test]
    async fn concurrent_sessions_with_duplicate_session_id_are_tracked_and_kickable() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store.clone(),
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-dup-sess-1",
                RelayTokenRecord {
                    session_id: "shared-session-id".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        ephemeral
            .put_relay_token(
                "tok-dup-sess-2",
                RelayTokenRecord {
                    session_id: "shared-session-id".into(),
                    host_id: "333333333".into(),
                    client_id: "444444444".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        // Pair session 1.
        let mut h1 = TcpStream::connect(addr).await.unwrap();
        let mut c1 = TcpStream::connect(addr).await.unwrap();
        h1.write_all(&encode_relay_handshake(ROLE_HOST, "tok-dup-sess-1").unwrap())
            .await
            .unwrap();
        c1.write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dup-sess-1").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        h1.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        c1.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        // Pair session 2 with the SAME `session_id`.
        let mut h2 = TcpStream::connect(addr).await.unwrap();
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        h2.write_all(&encode_relay_handshake(ROLE_HOST, "tok-dup-sess-2").unwrap())
            .await
            .unwrap();
        c2.write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dup-sess-2").unwrap())
            .await
            .unwrap();
        h2.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        c2.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        // Verify both sessions forward data independently.
        h1.write_all(b"ping-1").await.unwrap();
        h2.write_all(b"ping-2").await.unwrap();
        let mut buf1 = [0u8; 6];
        let mut buf2 = [0u8; 6];
        c1.read_exact(&mut buf1).await.unwrap();
        c2.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf1, b"ping-1");
        assert_eq!(&buf2, b"ping-2");

        // Both concurrent sessions must be visible in active_sessions, stats, and in_session_device_ids.
        let active = relay.active_sessions();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|s| s.session_id == "shared-session-id"));

        let stats = relay.stats();
        assert_eq!(stats.active_sessions, 2);
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.total_bytes_up, 12);

        let in_session = relay.in_session_device_ids();
        assert!(in_session.contains("111111111"));
        assert!(in_session.contains("222222222"));
        assert!(in_session.contains("333333333"));
        assert!(in_session.contains("444444444"));

        // `kick_session("shared-session-id")` must cancel BOTH sessions!
        assert!(relay.kick_session("shared-session-id"));

        let mut tail = [0u8; 8];
        for stream in [&mut h1, &mut c1, &mut h2, &mut c2] {
            let res = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tail))
                .await
                .expect("every peer in duplicate session_id must terminate on kick_session");
            assert!(matches!(res, Ok(0) | Err(_)));
        }

        for _ in 0..50 {
            if relay.stats().active_sessions == 0 && store.relay_logs().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.stats().active_sessions, 0);
        assert_eq!(store.relay_logs().len(), 2);
        assert!(!relay.kick_session("shared-session-id"));

        server_task.abort();
    }

    #[tokio::test]
    async fn kick_device_sessions_only_cancels_matching_device_with_shared_session_id() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-dev-1",
                RelayTokenRecord {
                    session_id: "same-id".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        ephemeral
            .put_relay_token(
                "tok-dev-2",
                RelayTokenRecord {
                    session_id: "same-id".into(),
                    host_id: "333333333".into(),
                    client_id: "444444444".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut h1 = TcpStream::connect(addr).await.unwrap();
        let mut c1 = TcpStream::connect(addr).await.unwrap();
        h1.write_all(&encode_relay_handshake(ROLE_HOST, "tok-dev-1").unwrap())
            .await
            .unwrap();
        c1.write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dev-1").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        h1.read_exact(&mut ack).await.unwrap();
        c1.read_exact(&mut ack).await.unwrap();

        let mut h2 = TcpStream::connect(addr).await.unwrap();
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        h2.write_all(&encode_relay_handshake(ROLE_HOST, "tok-dev-2").unwrap())
            .await
            .unwrap();
        c2.write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dev-2").unwrap())
            .await
            .unwrap();
        h2.read_exact(&mut ack).await.unwrap();
        c2.read_exact(&mut ack).await.unwrap();

        // Kick device 111111111 -> only session 1 terminates; session 2 keeps forwarding.
        assert_eq!(relay.kick_device_sessions("111111111"), 1);

        let mut tail = [0u8; 8];
        for stream in [&mut h1, &mut c1] {
            let res = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut tail))
                .await
                .expect("session 1 must terminate when its device is kicked");
            assert!(matches!(res, Ok(0) | Err(_)));
        }

        h2.write_all(b"still-alive").await.unwrap();
        let mut got = [0u8; 11];
        c2.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"still-alive");

        for _ in 0..50 {
            if relay.stats().active_sessions == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.stats().active_sessions, 1);

        assert_eq!(relay.kick_device_sessions("333333333"), 1);
        server_task.abort();
    }

    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (c, s) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (c.unwrap(), s.unwrap().0)
    }

    struct SlowEphemeralStore {
        inner: MemoryEphemeralStore,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl EphemeralStore for SlowEphemeralStore {
        async fn set_online(
            &self,
            device_id: &str,
            ttl: Duration,
        ) -> Result<(), crate::ephemeral::EphemeralError> {
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
            self.entered.notify_waiters();
            self.release.notified().await;
            self.inner.take_relay_token(token).await
        }
        fn backend_name(&self) -> &'static str {
            "slow-memory"
        }
        async fn health_check(&self) -> Result<(), crate::ephemeral::EphemeralError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn h1_shutdown_all_cancels_and_drains_active_and_pending_sessions() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store.clone(),
            ephemeral.clone(),
            RelayConfig {
                pair_timeout: Duration::from_secs(30),
                ..RelayConfig::default()
            },
        ));

        ephemeral
            .put_relay_token(
                "tok-sd-active",
                RelayTokenRecord {
                    session_id: "sess-sd-active".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        ephemeral
            .put_relay_token(
                "tok-sd-pending",
                RelayTokenRecord {
                    session_id: "sess-sd-pending".into(),
                    host_id: "333333333".into(),
                    client_id: "444444444".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_sd_tx, sd_rx) = watch::channel(false);
        let server_task = tokio::spawn(
            relay
                .clone()
                .serve_on_listener_with_shutdown(listener, sd_rx),
        );

        // Establish 1 active paired session.
        let mut host = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-sd-active").unwrap())
            .await
            .unwrap();
        client
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-sd-active").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        host.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        // Plus 1 waiting pending peer.
        let mut waiter = TcpStream::connect(addr).await.unwrap();
        waiter
            .write_all(&encode_relay_handshake(ROLE_HOST, "tok-sd-pending").unwrap())
            .await
            .unwrap();

        for _ in 0..100 {
            let s = relay.stats();
            if s.active_sessions == 1 && s.pending_handshakes == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.stats().active_sessions, 1);
        assert_eq!(relay.stats().pending_handshakes, 1);

        // Graceful shutdown must cancel active + pending sessions and drain all tasks cleanly.
        tokio::time::timeout(Duration::from_secs(2), relay.shutdown_all())
            .await
            .expect("shutdown_all must complete promptly");

        let serve_res = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("serve_on_listener_with_shutdown must exit on shutdown_all")
            .unwrap();
        assert!(serve_res.is_ok());

        assert_eq!(relay.stats().active_sessions, 0);
        assert_eq!(relay.stats().pending_handshakes, 0);
        assert_eq!(relay.in_flight_tasks.load(Ordering::Acquire), 0);
        assert_eq!(store.relay_logs().len(), 1);

        let mut buf = [0u8; 8];
        for stream in [&mut host, &mut client] {
            let res = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
                .await
                .expect("active peers must observe closure on shutdown_all");
            assert!(matches!(res, Ok(0) | Err(_)));
        }
        let mut waiter_reply = [0u8; 1];
        let res = tokio::time::timeout(Duration::from_secs(2), waiter.read(&mut waiter_reply))
            .await
            .expect("waiting peer must observe shutdown promptly");
        assert!(matches!(res, Ok(0) | Ok(1) | Err(_)));
        if let Ok(1) = res {
            assert_eq!(waiter_reply[0], REPLY_ERR);
        }
    }

    #[tokio::test]
    async fn h1_max_connections_bounds_concurrent_connections() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig {
                handshake_timeout: Duration::from_secs(5),
                pair_timeout: Duration::from_secs(30),
                max_pending: 16,
                max_connections: 2,
            },
        ));

        ephemeral
            .put_relay_token(
                "tok-conn-1",
                RelayTokenRecord {
                    session_id: "sess-conn-1".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        // 2 connections saturate max_connections=2 (one paired session holds both permits).
        let mut host = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-conn-1").unwrap())
            .await
            .unwrap();
        client
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-conn-1").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        host.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        // 3rd connection with invalid handshake will NOT be accepted/replied until a permit frees.
        let mut third = TcpStream::connect(addr).await.unwrap();
        third.write_all(b"BAD!00").await.unwrap();
        let mut reply = [0u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(120), third.read_exact(&mut reply))
                .await
                .is_err(),
            "3rd connection must wait while max_connections=2 permits are held"
        );

        // Closing the active session releases both permits -> 3rd connection is now processed.
        drop(host);
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), third.read_exact(&mut reply))
            .await
            .expect("3rd connection must be processed after permits free")
            .unwrap();
        assert_eq!(reply[0], REPLY_ERR);

        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn h2_peer1_disconnect_frees_pending_slot_immediately() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig {
                pair_timeout: Duration::from_secs(30),
                ..RelayConfig::default()
            },
        ));

        ephemeral
            .put_relay_token(
                "tok-h2-drop",
                RelayTokenRecord {
                    session_id: "sess-h2-drop".into(),
                    host_id: "100000001".into(),
                    client_id: "200000002".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut peer1 = TcpStream::connect(addr).await.unwrap();
        peer1
            .write_all(&encode_relay_handshake(ROLE_HOST, "tok-h2-drop").unwrap())
            .await
            .unwrap();

        for _ in 0..100 {
            if relay.stats().pending_handshakes == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.stats().pending_handshakes, 1);

        // Disconnect Peer 1 cleanly; slot must be freed promptly (well before 30s pair_timeout).
        drop(peer1);

        let freed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if relay.stats().pending_handshakes == 0 && relay.pending.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            freed.is_ok(),
            "pending slot must be freed immediately on Peer 1 disconnect"
        );
        assert_eq!(relay.pending_count.load(Ordering::Acquire), 0);

        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn h2_peer1_pipelined_payload_preserved_while_waiting() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-h2-pipe",
                RelayTokenRecord {
                    session_id: "sess-h2-pipe".into(),
                    host_id: "101000001".into(),
                    client_id: "202000002".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        // Peer 1 pipelines application payload immediately after the handshake before Peer 2 arrives.
        let mut peer1 = TcpStream::connect(addr).await.unwrap();
        let mut first_write = encode_relay_handshake(ROLE_HOST, "tok-h2-pipe").unwrap();
        first_write.extend_from_slice(b"pipelined-payload-bytes");
        peer1.write_all(&first_write).await.unwrap();

        for _ in 0..100 {
            if relay.stats().pending_handshakes == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Give peek loop a moment to observe the pipelined data without disconnecting or spinning.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(relay.stats().pending_handshakes, 1);

        let mut peer2 = TcpStream::connect(addr).await.unwrap();
        peer2
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-h2-pipe").unwrap())
            .await
            .unwrap();

        let mut ack = [0u8; 1];
        peer1.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        peer2.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        let mut received = vec![0u8; b"pipelined-payload-bytes".len()];
        tokio::time::timeout(Duration::from_secs(2), peer2.read_exact(&mut received))
            .await
            .expect("pipelined bytes must arrive intact at Peer 2")
            .unwrap();
        assert_eq!(received, b"pipelined-payload-bytes");

        drop(peer1);
        drop(peer2);
        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn h2_broken_waiting_peer_sends_err_to_surviving_peer() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(store, ephemeral, RelayConfig::default()));

        let (waiting_client_end, waiting_server_end) = loopback_pair().await;
        let (mut arriving_client_end, arriving_server_end) = loopback_pair().await;

        // Break the waiting peer and wait until the kernel actually delivers EOF to waiting_server_end.
        drop(waiting_client_end);
        let mut peek_buf = [0u8; 1];
        assert_eq!(waiting_server_end.peek(&mut peek_buf).await.unwrap(), 0);

        let record = RelayTokenRecord {
            session_id: "sess-broken-waiter".into(),
            host_id: "111111111".into(),
            client_id: "222222222".into(),
        };
        relay
            .clone()
            .run_paired_session(
                record,
                ROLE_HOST,
                RelayStream::Plain(waiting_server_end),
                RelayStream::Plain(arriving_server_end),
                None,
                None,
                CancellationFlag::new(),
                0,
            )
            .await;

        let mut reply = [0u8; 1];
        tokio::time::timeout(
            Duration::from_secs(2),
            arriving_client_end.read_exact(&mut reply),
        )
        .await
        .expect("surviving peer must receive REPLY_ERR")
        .unwrap();
        assert_eq!(reply[0], REPLY_ERR);
        assert_eq!(relay.stats().total_sessions, 0);
        assert_eq!(relay.stats().active_sessions, 0);
    }

    #[tokio::test]
    async fn h3_abort_active_session_cleans_up_active_sessions() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(store, ephemeral, RelayConfig::default()));

        let (mut host_client, host_server) = loopback_pair().await;
        let (mut client_client, client_server) = loopback_pair().await;

        let record = RelayTokenRecord {
            session_id: "sess-h3-abort".into(),
            host_id: "111111111".into(),
            client_id: "222222222".into(),
        };
        let task = tokio::spawn(relay.clone().run_paired_session(
            record,
            ROLE_HOST,
            RelayStream::Plain(host_server),
            RelayStream::Plain(client_server),
            None,
            None,
            CancellationFlag::new(),
            0,
        ));

        let mut ack = [0u8; 1];
        host_client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        client_client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        for _ in 0..100 {
            if relay.stats().active_sessions == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(relay.stats().active_sessions, 1);

        // Abruptly cancel the task mid-session; ActiveSessionGuard Drop must clean up.
        task.abort();
        let _ = task.await;

        assert_eq!(relay.stats().active_sessions, 0);
        assert!(relay.active_sessions().is_empty());
    }

    #[tokio::test]
    async fn m1_cancel_during_resolve_and_wait_cleans_up_pending() {
        // Part A: Abort during PendingSlot::Resolving removes key, decrements pending_count,
        // and wakes any WaitResolve sibling with REPLY_ERR.
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let slow = Arc::new(SlowEphemeralStore {
            inner: MemoryEphemeralStore::new(),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        slow.put_relay_token(
            "tok-m1-resolve",
            RelayTokenRecord {
                session_id: "sess-m1-resolve".into(),
                host_id: "111111111".into(),
                client_id: "222222222".into(),
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        let store = Arc::new(MemoryDeviceStore::new());
        let relay = Arc::new(RelayServer::new(
            store.clone(),
            slow.clone(),
            RelayConfig {
                handshake_timeout: Duration::from_secs(5),
                pair_timeout: Duration::from_secs(30),
                ..RelayConfig::default()
            },
        ));

        let (mut p1_cli, p1_srv) = loopback_pair().await;
        let p1_addr = p1_srv.peer_addr().unwrap();
        let entered_fut = entered.notified();
        tokio::pin!(entered_fut);
        entered_fut.as_mut().enable();

        let p1_task = tokio::spawn(relay.clone().handle_connection(p1_srv, p1_addr));
        p1_cli
            .write_all(&encode_relay_handshake(ROLE_HOST, "tok-m1-resolve").unwrap())
            .await
            .unwrap();
        entered_fut.await;
        assert_eq!(relay.stats().pending_handshakes, 1);

        // Second peer arrives while first is in Resolving -> enters WaitResolve.
        let (mut p2_cli, p2_srv) = loopback_pair().await;
        let p2_addr = p2_srv.peer_addr().unwrap();
        let p2_task = tokio::spawn(relay.clone().handle_connection(p2_srv, p2_addr));
        p2_cli
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-m1-resolve").unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Abort Peer 1 mid-take_relay_token.
        p1_task.abort();
        let _ = p1_task.await;

        assert_eq!(relay.stats().pending_handshakes, 0);
        assert!(relay.pending.is_empty());

        let mut p2_reply = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), p2_cli.read_exact(&mut p2_reply))
            .await
            .expect("WaitResolve peer must promptly receive REPLY_ERR when resolver drops")
            .unwrap();
        assert_eq!(p2_reply[0], REPLY_ERR);
        let _ = p2_task.await;

        // Part B: Hung take_relay_token times out at handshake_timeout and cleans up pending.
        let relay_timeout = Arc::new(RelayServer::new(
            store.clone(),
            slow,
            RelayConfig {
                handshake_timeout: Duration::from_millis(80),
                pair_timeout: Duration::from_secs(30),
                ..RelayConfig::default()
            },
        ));
        let (mut pt_cli, pt_srv) = loopback_pair().await;
        let pt_addr = pt_srv.peer_addr().unwrap();
        let pt_task = tokio::spawn(relay_timeout.clone().handle_connection(pt_srv, pt_addr));
        pt_cli
            .write_all(&encode_relay_handshake(ROLE_HOST, "tok-m1-timeout").unwrap())
            .await
            .unwrap();

        let mut pt_reply = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), pt_cli.read_exact(&mut pt_reply))
            .await
            .expect("timed-out token lookup must return REPLY_ERR")
            .unwrap();
        assert_eq!(pt_reply[0], REPLY_ERR);
        let _ = pt_task.await;
        assert_eq!(relay_timeout.stats().pending_handshakes, 0);
        assert!(relay_timeout.pending.is_empty());

        // Part C: Abort during PendingSlot::Waiting removes wait_id slot and sets pending_count == 0.
        let fast_ephemeral = Arc::new(MemoryEphemeralStore::new());
        fast_ephemeral
            .put_relay_token(
                "tok-m1-wait",
                RelayTokenRecord {
                    session_id: "sess-m1-wait".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let relay_wait = Arc::new(RelayServer::new(
            store,
            fast_ephemeral,
            RelayConfig {
                pair_timeout: Duration::from_secs(30),
                ..RelayConfig::default()
            },
        ));
        let (mut pw_cli, pw_srv) = loopback_pair().await;
        let pw_addr = pw_srv.peer_addr().unwrap();
        let pw_task = tokio::spawn(relay_wait.clone().handle_connection(pw_srv, pw_addr));
        pw_cli
            .write_all(&encode_relay_handshake(ROLE_HOST, "tok-m1-wait").unwrap())
            .await
            .unwrap();
        for _ in 0..100 {
            if relay_wait.stats().pending_handshakes == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(relay_wait.stats().pending_handshakes, 1);

        pw_task.abort();
        let _ = pw_task.await;
        assert_eq!(relay_wait.stats().pending_handshakes, 0);
        assert!(relay_wait.pending.is_empty());
    }

    #[tokio::test]
    async fn m2_hard_reset_tears_down_reverse_direction_promptly() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-m2-rst",
                RelayTokenRecord {
                    session_id: "sess-m2-rst".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut host = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-m2-rst").unwrap())
            .await
            .unwrap();
        client
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-m2-rst").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        host.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack[0], REPLY_OK);

        // Exchange one message to ensure both directions are actively pumping.
        host.write_all(b"ping").await.unwrap();
        let mut ping = [0u8; 4];
        client.read_exact(&mut ping).await.unwrap();
        assert_eq!(&ping, b"ping");

        // Force an immediate TCP RST (ECONNRESET) from host while client stays completely silent.
        SockRef::from(&host)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        drop(host);

        // Silent client must observe closure promptly because hard error cancels reverse pump.
        let mut buf = [0u8; 16];
        let res = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("reverse direction must tear down promptly on TCP RST");
        assert!(matches!(res, Ok(0) | Err(_)));

        let drained = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if relay.stats().active_sessions == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            drained.is_ok(),
            "active_sessions must return to 0 promptly after TCP RST"
        );

        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn m2_clean_half_close_preserves_reverse_direction() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        ephemeral
            .put_relay_token(
                "tok-m2-halfclose",
                RelayTokenRecord {
                    session_id: "sess-m2-halfclose".into(),
                    host_id: "111111111".into(),
                    client_id: "222222222".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut host = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpStream::connect(addr).await.unwrap();
        host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-m2-halfclose").unwrap())
            .await
            .unwrap();
        client
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-m2-halfclose").unwrap())
            .await
            .unwrap();
        let mut ack = [0u8; 1];
        host.read_exact(&mut ack).await.unwrap();
        client.read_exact(&mut ack).await.unwrap();

        // Host writes request and half-closes (FIN) BEFORE client responds.
        host.write_all(b"half-close-req").await.unwrap();
        host.shutdown().await.unwrap();

        // Client reads until clean EOF from host.
        let mut req = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut req))
            .await
            .expect("client must read host payload + EOF")
            .unwrap();
        assert_eq!(req, b"half-close-req");

        // Reverse direction must still be open so client can reply after host's FIN.
        client.write_all(b"reply-after-fin").await.unwrap();
        client.shutdown().await.unwrap();

        let mut resp = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), host.read_to_end(&mut resp))
            .await
            .expect("host must receive reverse reply after half-close")
            .unwrap();
        assert_eq!(resp, b"reply-after-fin");

        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn m3_pending_capacity_enforced_with_atomic_count() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig {
                pair_timeout: Duration::from_secs(30),
                max_pending: 2,
                ..RelayConfig::default()
            },
        ));

        for (tok, sess) in [
            ("tok-cap-1", "s1"),
            ("tok-cap-2", "s2"),
            ("tok-cap-3", "s3"),
        ] {
            ephemeral
                .put_relay_token(
                    tok,
                    RelayTokenRecord {
                        session_id: sess.into(),
                        host_id: "111111111".into(),
                        client_id: "222222222".into(),
                    },
                    Duration::from_secs(30),
                )
                .await
                .unwrap();
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        let mut h1 = TcpStream::connect(addr).await.unwrap();
        let mut h2 = TcpStream::connect(addr).await.unwrap();
        h1.write_all(&encode_relay_handshake(ROLE_HOST, "tok-cap-1").unwrap())
            .await
            .unwrap();
        h2.write_all(&encode_relay_handshake(ROLE_HOST, "tok-cap-2").unwrap())
            .await
            .unwrap();

        for _ in 0..100 {
            if relay.stats().pending_handshakes == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.stats().pending_handshakes, 2);

        // 3rd distinct token must be rejected because pending_count == max_pending (2).
        let mut h3 = TcpStream::connect(addr).await.unwrap();
        h3.write_all(&encode_relay_handshake(ROLE_HOST, "tok-cap-3").unwrap())
            .await
            .unwrap();
        let mut reply = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), h3.read_exact(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply[0], REPLY_ERR);
        assert_eq!(relay.stats().pending_handshakes, 2);

        // Matching an existing pending key must STILL succeed at max_pending capacity.
        let mut c1 = TcpStream::connect(addr).await.unwrap();
        c1.write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-cap-1").unwrap())
            .await
            .unwrap();
        h1.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], REPLY_OK);
        c1.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], REPLY_OK);

        assert_eq!(relay.stats().pending_handshakes, 1);

        drop(h1);
        drop(c1);
        drop(h2);
        relay.shutdown_all().await;
        let _ = server_task.await;
        assert_eq!(relay.stats().pending_handshakes, 0);
    }

    #[tokio::test]
    async fn l2_stats_byte_totals_strictly_monotonic_across_teardown() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        let relay = Arc::new(RelayServer::new(
            store,
            ephemeral.clone(),
            RelayConfig::default(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_task = tokio::spawn(relay.clone().serve_on_listener(listener));

        // Continuously sample relay.stats() across multiple transfers and teardowns, asserting
        // strict monotonicity with zero dip at any point.
        let stop_sampler = Arc::new(AtomicBool::new(false));
        let sampler_stop = Arc::clone(&stop_sampler);
        let sampler_relay = Arc::clone(&relay);
        let sampler = tokio::spawn(async move {
            let mut prev_up = 0u64;
            let mut prev_down = 0u64;
            let mut prev_total = 0u64;
            while !sampler_stop.load(Ordering::Relaxed) {
                let s = sampler_relay.stats();
                assert!(
                    s.total_bytes_up >= prev_up,
                    "total_bytes_up dipped from {prev_up} to {}",
                    s.total_bytes_up
                );
                assert!(
                    s.total_bytes_down >= prev_down,
                    "total_bytes_down dipped from {prev_down} to {}",
                    s.total_bytes_down
                );
                assert!(
                    s.total_relay_bytes >= prev_total,
                    "total_relay_bytes dipped from {prev_total} to {}",
                    s.total_relay_bytes
                );
                assert_eq!(s.total_relay_bytes, s.total_bytes_up + s.total_bytes_down);
                prev_up = s.total_bytes_up;
                prev_down = s.total_bytes_down;
                prev_total = s.total_relay_bytes;
                tokio::task::yield_now().await;
            }
        });

        let up_payload = vec![0xABu8; 64 * 1024];
        let down_payload = vec![0xCDu8; 32 * 1024];
        let sessions = 5usize;

        for i in 0..sessions {
            let tok = format!("tok-l2-{i}");
            ephemeral
                .put_relay_token(
                    &tok,
                    RelayTokenRecord {
                        session_id: format!("sess-l2-{i}"),
                        host_id: "111111111".into(),
                        client_id: "222222222".into(),
                    },
                    Duration::from_secs(30),
                )
                .await
                .unwrap();

            let mut host = TcpStream::connect(addr).await.unwrap();
            let mut client = TcpStream::connect(addr).await.unwrap();
            host.write_all(&encode_relay_handshake(ROLE_HOST, &tok).unwrap())
                .await
                .unwrap();
            client
                .write_all(&encode_relay_handshake(ROLE_CLIENT, &tok).unwrap())
                .await
                .unwrap();
            let mut ack = [0u8; 1];
            host.read_exact(&mut ack).await.unwrap();
            assert_eq!(ack[0], REPLY_OK);
            client.read_exact(&mut ack).await.unwrap();
            assert_eq!(ack[0], REPLY_OK);

            let (mut h_r, mut h_w) = host.into_split();
            let (mut c_r, mut c_w) = client.into_split();

            let up_data = up_payload.clone();
            let down_data = down_payload.clone();
            let send_up = tokio::spawn(async move {
                h_w.write_all(&up_data).await.unwrap();
                h_w.shutdown().await.unwrap();
            });
            let send_down = tokio::spawn(async move {
                c_w.write_all(&down_data).await.unwrap();
                c_w.shutdown().await.unwrap();
            });

            let mut got_up = Vec::new();
            let mut got_down = Vec::new();
            let (ru, rd) =
                tokio::join!(c_r.read_to_end(&mut got_up), h_r.read_to_end(&mut got_down));
            ru.unwrap();
            rd.unwrap();
            send_up.await.unwrap();
            send_down.await.unwrap();

            assert_eq!(got_up.len(), up_payload.len());
            assert_eq!(got_down.len(), down_payload.len());
        }

        // Wait for all sessions to finish cleanup.
        for _ in 0..100 {
            if relay.stats().active_sessions == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        stop_sampler.store(true, Ordering::Relaxed);
        sampler.await.unwrap();

        let final_stats = relay.stats();
        let expected_up = (sessions * up_payload.len()) as u64;
        let expected_down = (sessions * down_payload.len()) as u64;
        assert_eq!(final_stats.active_sessions, 0);
        assert_eq!(final_stats.total_sessions, sessions as u64);
        assert_eq!(final_stats.total_bytes_up, expected_up);
        assert_eq!(final_stats.total_bytes_down, expected_down);
        assert_eq!(final_stats.total_relay_bytes, expected_up + expected_down);

        relay.shutdown_all().await;
        let _ = server_task.await;
    }

    #[tokio::test]
    async fn kick_device_sessions_cancels_pending_waiting_peer() {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());
        ephemeral
            .put_relay_token(
                "tok-pending-kick",
                RelayTokenRecord {
                    session_id: "sess-pending-kick".into(),
                    host_id: "111122223".into(),
                    client_id: "444455556".into(),
                },
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let relay = Arc::new(RelayServer::new(store, ephemeral, RelayConfig::default()));

        let (mut cli, srv) = loopback_pair().await;
        let addr = srv.peer_addr().unwrap();
        let task = tokio::spawn(relay.clone().handle_connection(srv, addr));

        cli.write_all(&encode_relay_handshake(ROLE_HOST, "tok-pending-kick").unwrap())
            .await
            .unwrap();

        for _ in 0..50 {
            if !relay.pending.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(relay.pending.len(), 1);

        let kicked = relay.kick_device_sessions("111122223");
        assert_eq!(kicked, 1);

        let mut reply = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(2), cli.read_exact(&mut reply))
            .await
            .expect("kicked waiting peer must receive REPLY_ERR promptly")
            .unwrap();
        assert_eq!(reply[0], REPLY_ERR);
        let _ = task.await;
        assert!(relay.pending.is_empty());
        relay.shutdown_all().await;
    }
}
