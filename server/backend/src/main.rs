#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use rc_server::api::{self, AdvertisedEndpoints, AppState, AuthRateLimiter};
use rc_server::auth::{ensure_initial_admin, AuthAuthority};
use rc_server::ephemeral::{
    EphemeralStore, MemoryEphemeralStore, RedisEphemeralStore, SettingsCache,
};
use rc_server::pki::PkiAuthority;
use rc_server::relay::{RelayConfig, RelayServer};
use rc_server::signal::{SignalConfig, SignalHub};
use rc_server::store::{AuthStore, DeviceStore, PgDeviceStore, PkiStore};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    name = "rc-server",
    version,
    about = "RabbitControl Rust Rendezvous, Mandatory mTLS TCP Relay, and Admin REST API Server"
)]
struct Args {
    /// Axum HTTPS/HTTP Admin, Auth, PKI & Web UI listen address
    #[arg(long, env = "RC_HTTP_ADDR", default_value = "0.0.0.0:21118")]
    http: SocketAddr,

    /// Native TLS 1.3 TCP framed signaling + UDP STUN-lite probe listen address (NO WebSocket)
    #[arg(long, env = "RC_SIGNAL_ADDR", default_value = "0.0.0.0:21116")]
    signal: SocketAddr,

    /// Mandatory TLS 1.3 mTLS TCP relay listen address (RCR1 binary handshake)
    #[arg(long, env = "RC_RELAY_ADDR", default_value = "0.0.0.0:21117")]
    relay: SocketAddr,

    /// PostgreSQL connection URL (required)
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Optional Redis URL for distributed ephemeral state (omit to use in-memory DashMap)
    #[arg(long, env = "REDIS_URL")]
    redis_url: Option<String>,

    /// Optional public hostname or IP advertised to clients (e.g. 172.16.1.2 or relay.example.com)
    #[arg(long, env = "RC_PUBLIC_HOST")]
    public_host: Option<String>,

    /// Extra Subject Alternative Names (DNS or IP) included in the server TLS certificate
    #[arg(long = "tls-san", env = "RC_TLS_SAN", value_delimiter = ',')]
    tls_san: Vec<String>,

    /// Serve plain HTTP instead of HTTPS on --http (intended ONLY behind a TLS-terminating reverse proxy)
    #[arg(long, env = "RC_PLAIN_HTTP", default_value_t = false)]
    plain_http: bool,

    /// Optional directory containing built frontend static files (e.g. server/web/dist)
    #[arg(long, env = "RC_WEB_DIR")]
    web_dir: Option<PathBuf>,

    /// Optional email for the auto-generated initial administrator on first boot
    #[arg(long, env = "RC_INIT_ADMIN_EMAIL")]
    init_admin_email: Option<String>,

    /// Optional legacy static bearer token also accepted on admin REST endpoints
    #[arg(long, env = "RC_ADMIN_TOKEN")]
    admin_token: Option<String>,

    /// Allowed CORS origins (comma-separated, e.g. "https://admin.example.com" or "*" for wildcard; omit for same-origin only)
    #[arg(long = "cors-origin", env = "RC_CORS_ORIGIN", value_delimiter = ',')]
    cors_origin: Vec<String>,
}

fn format_advertised_addr(public_host: Option<&str>, bind: SocketAddr) -> String {
    if let Some(host) = public_host.map(str::trim).filter(|s| !s.is_empty()) {
        if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{}", bind.port())
        } else {
            format!("{host}:{}", bind.port())
        }
    } else if bind.ip().is_unspecified() {
        if bind.is_ipv6() {
            format!("[::1]:{}", bind.port())
        } else {
            format!("127.0.0.1:{}", bind.port())
        }
    } else {
        bind.to_string()
    }
}

fn build_tls13_server_config(
    cert_der: &[u8],
    key_pkcs8_der: &[u8],
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert_der.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8_der.to_vec())),
        )?;
    Ok(Arc::new(cfg))
}

async fn wait_for_watch_shutdown(mut rx: watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!(error = %err, "failed to install Ctrl+C handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                error!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("received SIGINT (Ctrl+C); initiating graceful shutdown");
        }
        _ = terminate => {
            info!("received SIGTERM; initiating graceful shutdown");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,rc_server=info,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let args = Args::parse();

    // Validate CORS configuration upfront so malformed origins fail fast.
    let _ = api::build_cors_layer(&args.cors_origin)
        .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;

    let admin_token = args
        .admin_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);

    let web_dir = args.web_dir.as_ref().and_then(|dir| {
        if dir.is_dir() {
            Some(dir.clone())
        } else {
            warn!(path = %dir.display(), "--web-dir does not exist or is not a directory; static web hosting disabled");
            None
        }
    });

    // Shared shutdown signal coordinated across HTTPS, UDP probe, TLS signaling, mTLS relay, and cleanup.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // 1. Connect PostgreSQL and run embedded SQL migrations.
    info!("connecting to PostgreSQL and running migrations");
    let pg_store = Arc::new(PgDeviceStore::connect(&args.database_url).await?);
    let device_store: Arc<dyn DeviceStore> = pg_store.clone();
    let auth_store: Arc<dyn AuthStore> = pg_store.clone();

    // 2. Ensure initial admin account on first boot.
    if let Some(creds) =
        ensure_initial_admin(pg_store.as_ref(), args.init_admin_email.as_deref()).await?
    {
        println!(
            "\n\
            ========================================================================\n\
              RabbitControl Initial Administrator Account Created (First Boot)\n\
              Email    : {email}\n\
              Password : {password}\n\
              Notice   : You MUST change this password on first login!\n\
            ========================================================================",
            email = creds.email,
            password = creds.password,
        );
        info!(
            admin_user_id = %creds.user_id,
            admin_email = %creds.email,
            "initial administrator account created; password change required on first login"
        );
    }

    // 3. Load or initialize persistent Ed25519 AuthAuthority and ECDSA P-256 Root CA.
    let auth_authority = Arc::new(AuthAuthority::load_or_create(pg_store.as_ref()).await?);
    let pki_authority = Arc::new(PkiAuthority::load_or_create(pg_store.as_ref()).await?);
    let server_cert =
        pki_authority.issue_default_server_tls_cert(args.public_host.as_deref(), &args.tls_san)?;
    let https_tls_config = pki_authority.build_server_tls_config(&server_cert, false)?;
    let signal_tls_config =
        build_tls13_server_config(&server_cert.cert_der, &server_cert.key_pkcs8_der)?;
    let relay_mtls_config = pki_authority.build_server_tls_config(&server_cert, true)?;

    // 4. Initialize EphemeralStore (Redis if configured, otherwise zero-dependency DashMap).
    let mut cleanup_task = None;
    let ephemeral: Arc<dyn EphemeralStore> = match args
        .redis_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(redis_url) => {
            info!("connecting to Redis ephemeral backend");
            let redis_store = RedisEphemeralStore::connect(redis_url).await?;
            Arc::new(redis_store)
        }
        None => {
            info!("no REDIS_URL provided; using in-memory DashMap ephemeral backend");
            let mem = Arc::new(MemoryEphemeralStore::new());
            cleanup_task = Some(
                mem.spawn_cleanup_task_with_shutdown(Duration::from_secs(30), shutdown_rx.clone()),
            );
            mem
        }
    };

    // Pre-warm SettingsCache and certificate revocation cache.
    let settings_cache = Arc::new(SettingsCache::new(pg_store.clone(), Arc::clone(&ephemeral)));
    let allow_registration = settings_cache.get_allow_registration().await?;
    let active_revoked = pg_store.list_active_revoked_certs().await?;
    let now = Utc::now();
    for cert in &active_revoked {
        if cert.expires_at > now {
            let ttl = (cert.expires_at - now)
                .to_std()
                .unwrap_or_else(|_| Duration::from_secs(1));
            let _ = ephemeral.revoke_cert_serial(&cert.serial_hex, ttl).await;
        }
    }

    // 5. Bind all network sockets upfront so address conflicts fail fast.
    let signal_listener = TcpListener::bind(args.signal).await?;
    let signal_local = signal_listener.local_addr()?;
    let udp_socket = Arc::new(UdpSocket::bind(signal_local).await?);
    let udp_local = udp_socket.local_addr()?;
    let relay_listener = TcpListener::bind(args.relay).await?;
    let relay_local = relay_listener.local_addr()?;
    let http_listener = TcpListener::bind(args.http).await?;
    let http_local = http_listener.local_addr()?;

    let advertised_signal = format_advertised_addr(args.public_host.as_deref(), signal_local);
    let advertised_udp = format_advertised_addr(args.public_host.as_deref(), udp_local);
    let advertised_relay = format_advertised_addr(args.public_host.as_deref(), relay_local);

    let signal_hub = Arc::new(SignalHub::with_auth(
        Arc::clone(&auth_store),
        Arc::clone(&ephemeral),
        Arc::clone(&auth_authority),
        SignalConfig {
            public_udp_probe_addr: advertised_udp.clone(),
            public_relay_addr: advertised_relay.clone(),
            ..SignalConfig::default()
        },
    ));

    let relay_server = Arc::new(RelayServer::new(
        Arc::clone(&device_store),
        Arc::clone(&ephemeral),
        RelayConfig::default(),
    ));

    let tls_enabled = !args.plain_http;
    let app_state = AppState {
        device_store,
        auth_store: Arc::clone(&auth_store),
        ephemeral: Arc::clone(&ephemeral),
        auth_authority: Arc::clone(&auth_authority),
        pki_authority: Arc::clone(&pki_authority),
        settings_cache: Arc::clone(&settings_cache),
        signal_hub: Arc::clone(&signal_hub),
        relay_server: Arc::clone(&relay_server),
        endpoints: AdvertisedEndpoints {
            signal_tcp_addr: advertised_signal.clone(),
            udp_probe_addr: advertised_udp.clone(),
            tcp_relay_addr: advertised_relay.clone(),
        },
        started_at: Instant::now(),
        admin_token: admin_token.clone(),
        cors_origins: args.cors_origin.clone(),
        tls_enabled,
        web_dir: web_dir.clone(),
        auth_rate_limiter: Arc::new(AuthRateLimiter::default()),
        allow_unauthenticated_legacy_admin: false,
    };
    let app = api::router(app_state);

    info!(
        http_addr = %http_local,
        https_enabled = tls_enabled,
        signal_tls_addr = %signal_local,
        udp_probe_addr = %udp_local,
        relay_mtls_addr = %relay_local,
        advertised_signal = %advertised_signal,
        advertised_udp = %advertised_udp,
        advertised_relay = %advertised_relay,
        ca_fingerprint_sha256 = %pki_authority.ca_fingerprint_sha256(),
        auth_pubkey_hex = %auth_authority.public_key_hex(),
        allow_registration,
        prewarmed_revoked_certs = active_revoked.len(),
        web_dir = ?web_dir,
        static_admin_token_configured = admin_token.is_some(),
        ephemeral_backend = ephemeral.backend_name(),
        relay_engine = relay_server.engine(),
        "rc-server started"
    );

    // 6. Spawn all listeners into a tracked JoinSet so any fatal exit triggers coordinated shutdown.
    let mut listeners: JoinSet<(&'static str, std::io::Result<()>)> = JoinSet::new();

    {
        let udp_hub = Arc::clone(&signal_hub);
        let udp_shutdown = shutdown_rx.clone();
        listeners.spawn(async move {
            (
                "udp_probe",
                udp_hub
                    .serve_udp_probe_on_socket_with_shutdown(udp_socket, udp_shutdown)
                    .await,
            )
        });
    }

    {
        let tcp_hub = Arc::clone(&signal_hub);
        let sig_shutdown = shutdown_rx.clone();
        listeners.spawn(async move {
            (
                "tls_signaling",
                tcp_hub
                    .serve_tls_signaling_on_listener_with_shutdown(
                        signal_listener,
                        signal_tls_config,
                        sig_shutdown,
                    )
                    .await,
            )
        });
    }

    {
        let relay_srv = Arc::clone(&relay_server);
        let relay_auth_store = Arc::clone(&auth_store);
        let relay_shutdown = shutdown_rx.clone();
        listeners.spawn(async move {
            (
                "mtls_relay",
                relay_srv
                    .serve_tls_on_listener_with_shutdown(
                        relay_listener,
                        relay_mtls_config,
                        relay_auth_store,
                        relay_shutdown,
                    )
                    .await,
            )
        });
    }

    {
        let http_shutdown = shutdown_rx.clone();
        if args.plain_http {
            listeners.spawn(async move {
                (
                    "http_api",
                    axum::serve(
                        http_listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(wait_for_watch_shutdown(http_shutdown))
                    .await,
                )
            });
        } else {
            listeners.spawn(async move {
                (
                    "https_api",
                    api::serve_https(http_listener, https_tls_config, app, http_shutdown).await,
                )
            });
        }
    }

    let mut fatal_error: Option<String> = None;

    tokio::select! {
        _ = wait_for_shutdown_signal() => {
            let _ = shutdown_tx.send(true);
        }
        Some(joined) = listeners.join_next() => {
            let _ = shutdown_tx.send(true);
            match joined {
                Ok((name, Ok(()))) => {
                    error!(service = name, "listener exited unexpectedly; initiating coordinated shutdown");
                    fatal_error = Some(format!("service '{name}' exited unexpectedly"));
                }
                Ok((name, Err(err))) => {
                    error!(service = name, error = %err, "listener exited with fatal error; initiating coordinated shutdown");
                    fatal_error = Some(format!("service '{name}' failed: {err}"));
                }
                Err(join_err) => {
                    error!(error = %join_err, "listener task panicked or was cancelled; initiating coordinated shutdown");
                    fatal_error = Some(format!("listener task failed: {join_err}"));
                }
            }
        }
    }

    // 7. Coordinated graceful drain with a bounded 5s timeout so active relay sessions
    // flush `record_relay_end` to PostgreSQL and online peers clean up ephemeral state.
    let drain_result = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
        let drain_listeners = async {
            let mut drain_err: Option<String> = None;
            while let Some(joined) = listeners.join_next().await {
                match joined {
                    Ok((name, Ok(()))) => {
                        info!(service = name, "listener stopped cleanly");
                    }
                    Ok((name, Err(err))) => {
                        error!(service = name, error = %err, "listener exited with error during shutdown");
                        if drain_err.is_none() {
                            drain_err = Some(format!("service '{name}' failed during shutdown: {err}"));
                        }
                    }
                    Err(join_err) => {
                        error!(error = %join_err, "listener task failed during shutdown");
                        if drain_err.is_none() {
                            drain_err = Some(format!("listener task failed during shutdown: {join_err}"));
                        }
                    }
                }
            }
            drain_err
        };

        let drain_cleanup = async {
            if let Some(handle) = cleanup_task.take() {
                let _ = handle.await;
            }
        };

        let ((), (), listener_err, ()) = tokio::join!(
            signal_hub.shutdown_all(),
            relay_server.shutdown_all(),
            drain_listeners,
            drain_cleanup,
        );
        listener_err
    })
    .await;

    match drain_result {
        Ok(listener_err) => {
            if fatal_error.is_none() {
                fatal_error = listener_err;
            }
        }
        Err(_) => {
            warn!(
                timeout_secs = SHUTDOWN_DRAIN_TIMEOUT.as_secs(),
                "graceful shutdown drain timed out; aborting remaining tasks"
            );
            listeners.abort_all();
            if let Some(handle) = cleanup_task.take() {
                handle.abort();
            }
        }
    }

    if let Some(msg) = fatal_error {
        return Err(std::io::Error::other(msg).into());
    }

    info!("rc-server stopped cleanly");
    Ok(())
}
