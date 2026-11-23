#![deny(unsafe_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

use rc_server::api::{
    self, AdvertisedEndpoints, AppState, AuthRateLimiter, DevicesResponse, DisableDeviceResponse,
    ErrorBody, HealthResponse, KickSessionResponse, RegisterResponse, SessionsResponse,
    StatsResponse,
};
use rc_server::auth::AuthAuthority;
use rc_server::ephemeral::{EphemeralStore, MemoryEphemeralStore, RelayTokenRecord, SettingsCache};
use rc_server::pki::PkiAuthority;
use rc_server::relay::{
    encode_relay_handshake, RelayConfig, RelayServer, REPLY_ERR, REPLY_OK, ROLE_CLIENT, ROLE_HOST,
};
use rc_server::signal::{
    read_signal_frame, write_signal_frame, SignalConfig, SignalHub, SignalMsg, UdpProbeRequest,
    UdpProbeResponse,
};
use rc_server::store::{is_valid_device_id, MemoryDeviceStore};

struct TestCluster {
    http_addr: SocketAddr,
    signal_addr: SocketAddr,
    udp_addr: SocketAddr,
    relay_addr: SocketAddr,
    store: Arc<MemoryDeviceStore>,
    ephemeral: Arc<MemoryEphemeralStore>,
    tasks: Vec<JoinHandle<()>>,
}

impl TestCluster {
    async fn start() -> Self {
        Self::start_with_options(None, Vec::new()).await
    }

    async fn start_with_options(admin_token: Option<String>, cors_origins: Vec<String>) -> Self {
        let store = Arc::new(MemoryDeviceStore::new());
        let ephemeral = Arc::new(MemoryEphemeralStore::new());

        let signal_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let signal_addr = signal_listener.local_addr().unwrap();

        let udp_socket = Arc::new(UdpSocket::bind(signal_addr).await.unwrap());
        let udp_addr = udp_socket.local_addr().unwrap();

        let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = relay_listener.local_addr().unwrap();

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http_listener.local_addr().unwrap();

        let hub = Arc::new(SignalHub::new(
            store.clone(),
            ephemeral.clone(),
            SignalConfig {
                public_udp_probe_addr: udp_addr.to_string(),
                public_relay_addr: relay_addr.to_string(),
                ..SignalConfig::default()
            },
        ));

        let relay = Arc::new(RelayServer::new(
            store.clone(),
            ephemeral.clone(),
            RelayConfig {
                handshake_timeout: Duration::from_secs(5),
                pair_timeout: Duration::from_secs(5),
                max_pending: 4096,
                ..RelayConfig::default()
            },
        ));

        let auth_authority = Arc::new(AuthAuthority::generate_ephemeral().unwrap());
        let pki_authority = Arc::new(PkiAuthority::generate_ephemeral().unwrap());
        let settings_cache = Arc::new(SettingsCache::new(store.clone(), ephemeral.clone()));

        let app = api::router(AppState {
            device_store: store.clone(),
            auth_store: store.clone(),
            ephemeral: ephemeral.clone(),
            auth_authority,
            pki_authority,
            settings_cache,
            signal_hub: hub.clone(),
            relay_server: relay.clone(),
            endpoints: AdvertisedEndpoints {
                signal_tcp_addr: signal_addr.to_string(),
                udp_probe_addr: udp_addr.to_string(),
                tcp_relay_addr: relay_addr.to_string(),
            },
            started_at: Instant::now(),
            admin_token,
            cors_origins,
            tls_enabled: false,
            web_dir: None,
            auth_rate_limiter: Arc::new(AuthRateLimiter::default()),
            allow_unauthenticated_legacy_admin: true,
        });

        let udp_hub = hub.clone();
        let udp_task = tokio::spawn(async move {
            let _ = udp_hub.serve_udp_probe_on_socket(udp_socket).await;
        });

        let sig_hub = hub.clone();
        let signal_task = tokio::spawn(async move {
            let _ = sig_hub
                .serve_tcp_signaling_on_listener(signal_listener)
                .await;
        });

        let relay_srv = relay.clone();
        let relay_task = tokio::spawn(async move {
            let _ = relay_srv.serve_on_listener(relay_listener).await;
        });

        let http_task = tokio::spawn(async move {
            let _ = axum::serve(http_listener, app).await;
        });

        Self {
            http_addr,
            signal_addr,
            udp_addr,
            relay_addr,
            store,
            ephemeral,
            tasks: vec![udp_task, signal_task, relay_task, http_task],
        }
    }
}

impl Drop for TestCluster {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn http_raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    raw_body: Option<&[u8]>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(bytes) = raw_body {
        req.push_str(&format!("Content-Length: {}\r\n", bytes.len()));
    }
    req.push_str("\r\n");

    stream.write_all(req.as_bytes()).await.unwrap();
    if let Some(bytes) = raw_body {
        stream.write_all(bytes).await.unwrap();
    }
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();

    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("HTTP response header terminator");
    let header_str = std::str::from_utf8(&raw[..header_end]).unwrap().to_string();
    let status_line = header_str.lines().next().unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();

    let is_chunked = header_str.lines().any(|line| {
        line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
    });

    let body_bytes = &raw[header_end + 4..];
    let decoded = if is_chunked {
        decode_chunked(body_bytes)
    } else {
        body_bytes.to_vec()
    };

    (status, header_str, String::from_utf8(decoded).unwrap())
}

async fn http_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, String) {
    let payload = body.map(|b| serde_json::to_vec(b).unwrap());
    let headers: &[(&str, &str)] = if payload.is_some() {
        &[("Content-Type", "application/json")]
    } else {
        &[]
    };
    let (status, _, resp_body) = http_raw(addr, method, path, headers, payload.as_deref()).await;
    (status, resp_body)
}

fn decode_chunked(mut input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(pos) = input.windows(2).position(|w| w == b"\r\n") {
        let len_str = std::str::from_utf8(&input[..pos])
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim();
        let len = usize::from_str_radix(len_str, 16).unwrap();
        input = &input[pos + 2..];
        if len == 0 {
            break;
        }
        out.extend_from_slice(&input[..len]);
        input = &input[len + 2..];
    }
    out
}

fn deterministic_payload(len: usize, seed: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len);
    let mut x: u32 = (seed as u32).wrapping_mul(0x9E37_79B9).wrapping_add(1);
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        buf.push((x >> 24) as u8);
    }
    buf
}

#[tokio::test]
async fn full_flow_register_udp_signal_relay_2mib_and_api_stats() {
    let cluster = TestCluster::start().await;

    // 1. Health endpoint reports healthy memory backend + active relay engine.
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/health", None).await;
    assert_eq!(status, 200, "health body: {body}");
    let health: HealthResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.postgres, "ok");
    assert_eq!(health.ephemeral_backend, "memory");
    assert_eq!(health.relay_engine, RelayServer::engine_name());

    // 2. Register two devices via HTTP REST API (and verify idempotency + 9-digit IDs).
    let host_reg_req = serde_json::json!({
        "device_key": "ed25519:e2e-host-key",
        "hostname": "studio-host",
        "os": "macos",
        "device_group": "office"
    });
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&host_reg_req),
    )
    .await;
    assert_eq!(status, 200, "register host body: {body}");
    let host_reg: RegisterResponse = serde_json::from_str(&body).unwrap();
    assert!(is_valid_device_id(&host_reg.device_id));
    assert_eq!(host_reg.signal_tcp_addr, cluster.signal_addr.to_string());
    assert_eq!(host_reg.udp_probe_addr, cluster.udp_addr.to_string());
    assert_eq!(host_reg.tcp_relay_addr, cluster.relay_addr.to_string());

    // Idempotent second call returns identical 9-digit device_id.
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&host_reg_req),
    )
    .await;
    assert_eq!(status, 200);
    let host_reg_again: RegisterResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(host_reg_again.device_id, host_reg.device_id);

    let client_reg_req = serde_json::json!({
        "device_key": "ed25519:e2e-client-key",
        "hostname": "laptop-client",
        "os": "linux",
        "device_group": "mobile"
    });
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&client_reg_req),
    )
    .await;
    assert_eq!(status, 200);
    let client_reg: RegisterResponse = serde_json::from_str(&body).unwrap();
    assert!(is_valid_device_id(&client_reg.device_id));
    assert_ne!(client_reg.device_id, host_reg.device_id);

    // 3. Both peers connect over Native TCP framed signaling (NO WebSocket) BEFORE UDP probes
    // so the server binds their authenticated TCP IP before persisting public UDP endpoints (H4).
    let mut host_sig = TcpStream::connect(cluster.signal_addr).await.unwrap();
    write_signal_frame(
        &mut host_sig,
        &SignalMsg::Register {
            device_id: Some(host_reg.device_id.clone()),
            device_key: "ed25519:e2e-host-key".into(),
            hostname: "studio-host".into(),
            os: "macos".into(),
            device_group: "office".into(),
            lan_addrs: vec!["192.168.10.2:50000".into()],
            access_token: None,
            device_pubkey: None,
            allow_remote_assist: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_signal_frame(&mut host_sig).await.unwrap().unwrap(),
        SignalMsg::Registered { device_id, .. } if device_id == host_reg.device_id
    ));

    let mut client_sig = TcpStream::connect(cluster.signal_addr).await.unwrap();
    write_signal_frame(
        &mut client_sig,
        &SignalMsg::Register {
            device_id: Some(client_reg.device_id.clone()),
            device_key: "ed25519:e2e-client-key".into(),
            hostname: "laptop-client".into(),
            os: "linux".into(),
            device_group: "mobile".into(),
            lan_addrs: vec!["192.168.10.3:50001".into()],
            access_token: None,
            device_pubkey: None,
            allow_remote_assist: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_signal_frame(&mut client_sig).await.unwrap().unwrap(),
        SignalMsg::Registered { device_id, .. } if device_id == client_reg.device_id
    ));

    // 4. Both online peers probe public UDP endpoint via RCSP -> RCSA.
    let host_udp_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let host_udp_local = host_udp_sock.local_addr().unwrap();
    let host_probe = UdpProbeRequest {
        device_id: host_reg.device_id.clone(),
        nonce: *b"host0001",
    };
    host_udp_sock
        .send_to(&host_probe.encode().unwrap(), cluster.udp_addr)
        .await
        .unwrap();
    let mut udp_buf = [0u8; 64];
    let (n, _) = tokio::time::timeout(
        Duration::from_secs(2),
        host_udp_sock.recv_from(&mut udp_buf),
    )
    .await
    .unwrap()
    .unwrap();
    let host_ans = UdpProbeResponse::decode(&udp_buf[..n]).unwrap();
    assert_eq!(host_ans.observed_addr, host_udp_local);
    assert_eq!(host_ans.nonce, *b"host0001");

    let client_udp_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client_udp_local = client_udp_sock.local_addr().unwrap();
    let client_probe = UdpProbeRequest {
        device_id: client_reg.device_id.clone(),
        nonce: *b"clnt0002",
    };
    client_udp_sock
        .send_to(&client_probe.encode().unwrap(), cluster.udp_addr)
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(
        Duration::from_secs(2),
        client_udp_sock.recv_from(&mut udp_buf),
    )
    .await
    .unwrap()
    .unwrap();
    let client_ans = UdpProbeResponse::decode(&udp_buf[..n]).unwrap();
    assert_eq!(client_ans.observed_addr, client_udp_local);
    assert_eq!(client_ans.nonce, *b"clnt0002");

    // Wait briefly for the bounded background UDP update worker to persist both endpoints.
    for _ in 0..100 {
        if cluster
            .ephemeral
            .get_public_udp(&host_reg.device_id)
            .await
            .unwrap()
            .is_some()
            && cluster
                .ephemeral
                .get_public_udp(&client_reg.device_id)
                .await
                .unwrap()
                .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // 5. Client sends connect_request -> both receive punch_coord + matching relay_token.
    write_signal_frame(
        &mut client_sig,
        &SignalMsg::ConnectRequest {
            target_id: host_reg.device_id.clone(),
            session_id: Some("sess-e2e-2mib".into()),
            lan_addrs: vec!["192.168.10.3:50001".into()],
            nonce: Some("cafe1234".into()),
        },
    )
    .await
    .unwrap();

    let client_punch = read_signal_frame(&mut client_sig).await.unwrap().unwrap();
    let host_punch = read_signal_frame(&mut host_sig).await.unwrap().unwrap();

    let relay_token = match (client_punch, host_punch) {
        (
            SignalMsg::PunchCoord {
                session_id: cs,
                role: cr,
                peer_id: cp,
                peer_public_udp_addr: c_udp,
                relay_token: ct,
                ..
            },
            SignalMsg::PunchCoord {
                session_id: hs,
                role: hr,
                peer_id: hp,
                peer_public_udp_addr: h_udp,
                relay_token: ht,
                ..
            },
        ) => {
            assert_eq!(cs, "sess-e2e-2mib");
            assert_eq!(hs, "sess-e2e-2mib");
            assert_eq!(cr, "client");
            assert_eq!(hr, "host");
            assert_eq!(cp, host_reg.device_id);
            assert_eq!(hp, client_reg.device_id);
            assert_eq!(c_udp, Some(host_udp_local.to_string()));
            assert_eq!(h_udp, Some(client_udp_local.to_string()));
            assert_eq!(ct, ht);
            ct
        }
        other => panic!("expected PunchCoord for both peers, got {other:?}"),
    };

    // 6. Pair over TCP Relay (`RCR1`) and transfer 2 MiB bidirectionally with byte-for-byte match!
    let mut host_relay = TcpStream::connect(cluster.relay_addr).await.unwrap();
    let mut client_relay = TcpStream::connect(cluster.relay_addr).await.unwrap();

    host_relay
        .write_all(&encode_relay_handshake(ROLE_HOST, &relay_token).unwrap())
        .await
        .unwrap();
    client_relay
        .write_all(&encode_relay_handshake(ROLE_CLIENT, &relay_token).unwrap())
        .await
        .unwrap();

    let mut h_ack = [0u8; 1];
    let mut c_ack = [0u8; 1];
    host_relay.read_exact(&mut h_ack).await.unwrap();
    client_relay.read_exact(&mut c_ack).await.unwrap();
    assert_eq!(h_ack[0], REPLY_OK);
    assert_eq!(c_ack[0], REPLY_OK);

    // While the relay session is active, verify `/api/v1/devices?status=in_session` and `/api/v1/sessions`.
    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?status=in_session",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let in_sess_devs: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(in_sess_devs.total, 2);
    assert!(in_sess_devs
        .devices
        .iter()
        .all(|d| d.status == "in_session"));

    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/sessions", None).await;
    assert_eq!(status, 200);
    let active_sessions: SessionsResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(active_sessions.total, 1);
    assert_eq!(active_sessions.sessions[0].session_id, "sess-e2e-2mib");

    const TWO_MIB: usize = 2 * 1024 * 1024;
    let host_payload = deterministic_payload(TWO_MIB, 0x5A);
    let client_payload = deterministic_payload(TWO_MIB, 0xA5);

    let (mut hr, mut hw) = host_relay.into_split();
    let (mut cr, mut cw) = client_relay.into_split();

    let send_host = {
        let data = host_payload.clone();
        tokio::spawn(async move {
            hw.write_all(&data).await.unwrap();
            hw.shutdown().await.unwrap();
        })
    };
    let send_client = {
        let data = client_payload.clone();
        tokio::spawn(async move {
            cw.write_all(&data).await.unwrap();
            cw.shutdown().await.unwrap();
        })
    };
    let recv_at_client = tokio::spawn(async move {
        let mut got = Vec::with_capacity(TWO_MIB);
        cr.read_to_end(&mut got).await.unwrap();
        got
    });
    let recv_at_host = tokio::spawn(async move {
        let mut got = Vec::with_capacity(TWO_MIB);
        hr.read_to_end(&mut got).await.unwrap();
        got
    });

    send_host.await.unwrap();
    send_client.await.unwrap();
    let got_at_client = recv_at_client.await.unwrap();
    let got_at_host = recv_at_host.await.unwrap();

    assert_eq!(got_at_client.len(), TWO_MIB);
    assert_eq!(got_at_host.len(), TWO_MIB);
    assert_eq!(got_at_client, host_payload, "host->client 2 MiB mismatch");
    assert_eq!(got_at_host, client_payload, "client->host 2 MiB mismatch");

    // Wait briefly for relay session teardown & DB log write.
    for _ in 0..100 {
        if !cluster.store.relay_logs().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let logs = cluster.store.relay_logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].session_id, "sess-e2e-2mib");
    assert_eq!(logs[0].bytes_up, TWO_MIB as u64);
    assert_eq!(logs[0].bytes_down, TWO_MIB as u64);

    // 7. Verify `/api/v1/stats` & `/api/v1/devices` queries.
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/stats", None).await;
    assert_eq!(status, 200, "stats body: {body}");
    let stats: StatsResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(stats.online_count, 2);
    assert_eq!(stats.registered_count, 2);
    assert_eq!(stats.active_relay_sessions, 0);
    assert_eq!(stats.total_relay_sessions, 1);
    assert_eq!(stats.total_relay_bytes, (TWO_MIB * 2) as u64);
    assert_eq!(stats.punch_coord_count, 1);
    assert_eq!(stats.ephemeral_backend, "memory");
    assert_eq!(stats.relay_engine, RelayServer::engine_name());

    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?q=studio&status=online",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let filtered: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.devices[0].device_id, host_reg.device_id);
    assert_eq!(filtered.devices[0].status, "online");
    assert_eq!(
        filtered.devices[0].public_udp_addr,
        Some(host_udp_local.to_string())
    );

    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?status=offline",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let offline: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(offline.total, 0);
}

#[tokio::test]
async fn relay_rejects_invalid_token_duplicate_role_and_replay() {
    let cluster = TestCluster::start().await;

    // 1. Unknown token -> 0x01.
    {
        let mut s = TcpStream::connect(cluster.relay_addr).await.unwrap();
        s.write_all(&encode_relay_handshake(ROLE_HOST, "nonexistent-token").unwrap())
            .await
            .unwrap();
        let mut reply = [0u8; 1];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], REPLY_ERR);
    }

    // 2. Invalid magic -> 0x01.
    {
        let mut s = TcpStream::connect(cluster.relay_addr).await.unwrap();
        s.write_all(b"BAD!\x01\x04test").await.unwrap();
        let mut reply = [0u8; 1];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], REPLY_ERR);
    }

    // 3. Expired token -> 0x01.
    cluster
        .ephemeral
        .put_relay_token(
            "tok-short-ttl",
            RelayTokenRecord {
                session_id: "sess-exp".into(),
                host_id: "111111111".into(),
                client_id: "222222222".into(),
            },
            Duration::from_millis(40),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    {
        let mut s = TcpStream::connect(cluster.relay_addr).await.unwrap();
        s.write_all(&encode_relay_handshake(ROLE_HOST, "tok-short-ttl").unwrap())
            .await
            .unwrap();
        let mut reply = [0u8; 1];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], REPLY_ERR);
    }

    // 4. Duplicate role is rejected with 0x01 while original waiting peer still pairs cleanly!
    cluster
        .ephemeral
        .put_relay_token(
            "tok-dup-role",
            RelayTokenRecord {
                session_id: "sess-dup".into(),
                host_id: "111111111".into(),
                client_id: "222222222".into(),
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();

    let mut host1 = TcpStream::connect(cluster.relay_addr).await.unwrap();
    host1
        .write_all(&encode_relay_handshake(ROLE_HOST, "tok-dup-role").unwrap())
        .await
        .unwrap();
    // Give host1 a moment to enter the pending map.
    tokio::time::sleep(Duration::from_millis(40)).await;

    let mut dup_host = TcpStream::connect(cluster.relay_addr).await.unwrap();
    dup_host
        .write_all(&encode_relay_handshake(ROLE_HOST, "tok-dup-role").unwrap())
        .await
        .unwrap();
    let mut dup_reply = [0u8; 1];
    dup_host.read_exact(&mut dup_reply).await.unwrap();
    assert_eq!(
        dup_reply[0], REPLY_ERR,
        "duplicate host role must be rejected"
    );

    // Now legitimate client connects and pairs with host1.
    let mut client = TcpStream::connect(cluster.relay_addr).await.unwrap();
    client
        .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dup-role").unwrap())
        .await
        .unwrap();

    let mut h1_reply = [0u8; 1];
    let mut c_reply = [0u8; 1];
    host1.read_exact(&mut h1_reply).await.unwrap();
    client.read_exact(&mut c_reply).await.unwrap();
    assert_eq!(h1_reply[0], REPLY_OK);
    assert_eq!(c_reply[0], REPLY_OK);

    // 5. Replaying the already-paired token is rejected with 0x01.
    {
        let mut replay = TcpStream::connect(cluster.relay_addr).await.unwrap();
        replay
            .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-dup-role").unwrap())
            .await
            .unwrap();
        let mut r = [0u8; 1];
        replay.read_exact(&mut r).await.unwrap();
        assert_eq!(r[0], REPLY_ERR);
    }

    drop(host1);
    drop(client);
}

#[tokio::test]
async fn admin_kick_live_relay_session() {
    let cluster = TestCluster::start().await;

    cluster
        .ephemeral
        .put_relay_token(
            "tok-live-kick",
            RelayTokenRecord {
                session_id: "sess-kick-me".into(),
                host_id: "333444555".into(),
                client_id: "666777888".into(),
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();

    let mut host = TcpStream::connect(cluster.relay_addr).await.unwrap();
    let mut client = TcpStream::connect(cluster.relay_addr).await.unwrap();
    host.write_all(&encode_relay_handshake(ROLE_HOST, "tok-live-kick").unwrap())
        .await
        .unwrap();
    client
        .write_all(&encode_relay_handshake(ROLE_CLIENT, "tok-live-kick").unwrap())
        .await
        .unwrap();

    let mut ack = [0u8; 1];
    host.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack[0], REPLY_OK);
    client.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack[0], REPLY_OK);

    // Push a ping payload to confirm data flows before kick.
    host.write_all(b"live-ping").await.unwrap();
    let mut buf = [0u8; 9];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"live-ping");

    // Verify session appears in GET /api/v1/sessions.
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/sessions", None).await;
    assert_eq!(status, 200);
    let sessions: SessionsResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(sessions.total, 1);
    assert_eq!(sessions.sessions[0].session_id, "sess-kick-me");
    assert_eq!(sessions.sessions[0].bytes_up, 9);

    // Admin kick via DELETE /api/v1/sessions/{id}.
    let (status, body) = http_json(
        cluster.http_addr,
        "DELETE",
        "/api/v1/sessions/sess-kick-me",
        None,
    )
    .await;
    assert_eq!(status, 200, "kick body: {body}");
    let kicked: KickSessionResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(kicked.status, "kicked");
    assert_eq!(kicked.session_id, "sess-kick-me");

    // Both streams must terminate promptly upon kick.
    let mut tail = [0u8; 16];
    let host_eof = tokio::time::timeout(Duration::from_secs(2), host.read(&mut tail))
        .await
        .expect("host must disconnect after admin kick");
    assert!(matches!(host_eof, Ok(0) | Err(_)));

    let client_eof = tokio::time::timeout(Duration::from_secs(2), client.read(&mut tail))
        .await
        .expect("client must disconnect after admin kick");
    assert!(matches!(client_eof, Ok(0) | Err(_)));

    // Wait briefly for session removal, then verify 404 on repeat kick.
    for _ in 0..50 {
        let (_, b) = http_json(cluster.http_addr, "GET", "/api/v1/sessions", None).await;
        let s: SessionsResponse = serde_json::from_str(&b).unwrap();
        if s.total == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let (status, _) = http_json(
        cluster.http_addr,
        "DELETE",
        "/api/v1/sessions/sess-kick-me",
        None,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn admin_disable_device_kicks_online_peer_and_blocks_reregister() {
    let cluster = TestCluster::start().await;

    // 1. Register device via HTTP API and bring it online via Native TCP signaling.
    let reg_body = serde_json::json!({
        "device_key": "ed25519:ban-me-key",
        "hostname": "rogue-box",
        "os": "windows",
        "device_group": "untrusted"
    });
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&reg_body),
    )
    .await;
    assert_eq!(status, 200);
    let reg: RegisterResponse = serde_json::from_str(&body).unwrap();

    let mut sig = TcpStream::connect(cluster.signal_addr).await.unwrap();
    write_signal_frame(
        &mut sig,
        &SignalMsg::Register {
            device_id: Some(reg.device_id.clone()),
            device_key: "ed25519:ban-me-key".into(),
            hostname: "rogue-box".into(),
            os: "windows".into(),
            device_group: "untrusted".into(),
            lan_addrs: vec![],
            access_token: None,
            device_pubkey: None,
            allow_remote_assist: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_signal_frame(&mut sig).await.unwrap().unwrap(),
        SignalMsg::Registered { device_id, .. } if device_id == reg.device_id
    ));

    // 2. Admin DELETE /api/v1/devices/{id} disables device and kicks online peer.
    let (status, body) = http_json(
        cluster.http_addr,
        "DELETE",
        &format!("/api/v1/devices/{}", reg.device_id),
        None,
    )
    .await;
    assert_eq!(status, 200, "disable body: {body}");
    let disabled: DisableDeviceResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(disabled.status, "disabled");
    assert_eq!(disabled.device_id, reg.device_id);
    assert!(disabled.kicked_signal);

    // 3. Online signaling peer receives Kicked frame followed by EOF.
    let kicked_frame = tokio::time::timeout(Duration::from_secs(2), read_signal_frame(&mut sig))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        kicked_frame,
        SignalMsg::Kicked { reason } if reason == "disabled_by_admin"
    ));
    assert_eq!(read_signal_frame(&mut sig).await.unwrap(), None);

    // 4. Re-register via HTTP REST API is rejected with 403 Forbidden.
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&reg_body),
    )
    .await;
    assert_eq!(status, 403, "expected 403 for disabled device, got: {body}");

    // 5. Re-register via Native TCP signaling is rejected with device_disabled error.
    let mut retry_sig = TcpStream::connect(cluster.signal_addr).await.unwrap();
    write_signal_frame(
        &mut retry_sig,
        &SignalMsg::Register {
            device_id: None,
            device_key: "ed25519:ban-me-key".into(),
            hostname: "rogue-box".into(),
            os: "windows".into(),
            device_group: "untrusted".into(),
            lan_addrs: vec![],
            access_token: None,
            device_pubkey: None,
            allow_remote_assist: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        read_signal_frame(&mut retry_sig).await.unwrap().unwrap(),
        SignalMsg::Error { code, .. } if code == "device_disabled"
    ));

    // 6. Deleting an unknown device ID returns 404.
    let (status, _) = http_json(
        cluster.http_addr,
        "DELETE",
        "/api/v1/devices/999999999",
        None,
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn h9_admin_bearer_auth_and_cors_policy() {
    let cluster = TestCluster::start_with_options(
        Some("super-secret-admin-token".to_string()),
        vec!["https://admin.example.com".to_string()],
    )
    .await;

    // Public endpoints remain accessible without Authorization header.
    let (status, _) = http_json(cluster.http_addr, "GET", "/api/v1/health", None).await;
    assert_eq!(status, 200);

    let reg_req = serde_json::json!({
        "device_key": "ed25519:auth-test-key",
        "hostname": "auth-host",
        "os": "linux",
        "device_group": "ops"
    });
    let (status, body) = http_json(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        Some(&reg_req),
    )
    .await;
    assert_eq!(status, 200, "register should stay public: {body}");
    let reg: RegisterResponse = serde_json::from_str(&body).unwrap();

    // Admin endpoints reject missing or wrong Bearer token with JSON 401.
    for (method, path) in [
        ("GET", "/api/v1/stats"),
        ("GET", "/api/v1/devices"),
        ("GET", "/api/v1/sessions"),
        ("DELETE", "/api/v1/sessions/nonexistent"),
        ("DELETE", &format!("/api/v1/devices/{}", reg.device_id)),
    ] {
        let (status, headers, body) = http_raw(cluster.http_addr, method, path, &[], None).await;
        assert_eq!(status, 401, "{method} {path} without token: {body}");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json"),
            "401 must be JSON, got headers: {headers}"
        );
        let err: ErrorBody = serde_json::from_str(&body).unwrap();
        assert_eq!(err.error, "unauthorized");

        let (status, _, _) = http_raw(
            cluster.http_addr,
            method,
            path,
            &[("Authorization", "Bearer wrong-token")],
            None,
        )
        .await;
        assert_eq!(status, 401, "{method} {path} with wrong token must be 401");
    }

    // Valid Bearer token succeeds on admin routes.
    let auth_hdr = &[("Authorization", "Bearer super-secret-admin-token")];
    let (status, _, body) =
        http_raw(cluster.http_addr, "GET", "/api/v1/stats", auth_hdr, None).await;
    assert_eq!(status, 200, "stats with valid token: {body}");
    let (status, _, body) =
        http_raw(cluster.http_addr, "GET", "/api/v1/devices", auth_hdr, None).await;
    assert_eq!(status, 200, "devices with valid token: {body}");
    let (status, _, body) =
        http_raw(cluster.http_addr, "GET", "/api/v1/sessions", auth_hdr, None).await;
    assert_eq!(status, 200, "sessions with valid token: {body}");

    // CORS allows configured origin and omits header for unconfigured origin.
    let (status, headers, _) = http_raw(
        cluster.http_addr,
        "GET",
        "/api/v1/health",
        &[("Origin", "https://admin.example.com")],
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin: https://admin.example.com"),
        "expected allowed CORS origin header, got: {headers}"
    );

    let (status, headers, _) = http_raw(
        cluster.http_addr,
        "GET",
        "/api/v1/health",
        &[("Origin", "https://evil.example.com")],
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(
        !headers
            .to_ascii_lowercase()
            .contains("access-control-allow-origin:"),
        "disallowed origin must not receive ACAO header, got: {headers}"
    );
}

#[tokio::test]
async fn h8_m10_m11_m13_h11_l7_http_api_regressions() {
    let cluster = TestCluster::start().await;

    // M11: Invalid `?status=` on an EMPTY database must return 400 JSON error.
    let (status, headers, body) = http_raw(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?status=definitely_invalid",
        &[],
        None,
    )
    .await;
    assert_eq!(
        status, 400,
        "invalid status on empty DB must be 400: {body}"
    );
    assert!(headers
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    let err: ErrorBody = serde_json::from_str(&body).unwrap();
    assert_eq!(err.error, "invalid_status");

    // M13: Malformed JSON returns `application/json` ErrorBody (not text/plain).
    let (status, headers, body) = http_raw(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        &[("Content-Type", "application/json")],
        Some(b"{not valid json"),
    )
    .await;
    assert_eq!(status, 400, "malformed JSON must be 400: {body}");
    assert!(
        headers
            .to_ascii_lowercase()
            .contains("content-type: application/json"),
        "malformed JSON must return application/json, got: {headers}"
    );
    let err: ErrorBody = serde_json::from_str(&body).unwrap();
    assert_eq!(err.error, "invalid_json_syntax");

    // H11: Oversized fields & NUL/control chars return 400 JSON ErrorBody.
    for bad in [
        serde_json::json!({
            "device_key": "ed25519:h11-a",
            "hostname": "a".repeat(129),
            "os": "linux",
            "device_group": "ops"
        }),
        serde_json::json!({
            "device_key": "ed25519:h11-b",
            "hostname": "ok-host",
            "os": "o".repeat(129),
            "device_group": "ops"
        }),
        serde_json::json!({
            "device_key": "ed25519:h11-c",
            "hostname": "ok-host",
            "os": "linux",
            "device_group": "g".repeat(65)
        }),
        serde_json::json!({
            "device_key": "ed25519:h11-d",
            "hostname": "bad\u{0000}nul",
            "os": "linux",
            "device_group": "ops"
        }),
        serde_json::json!({
            "device_key": "ed25519:h11-e",
            "hostname": "ok-host",
            "os": "linux\u{0007}ctrl",
            "device_group": "ops"
        }),
    ] {
        let (status, body) = http_json(
            cluster.http_addr,
            "POST",
            "/api/v1/devices/register",
            Some(&bad),
        )
        .await;
        assert_eq!(status, 400, "invalid register payload must be 400: {body}");
        let err: ErrorBody = serde_json::from_str(&body).unwrap();
        assert_eq!(err.error, "invalid_input");
    }

    // H11: Body > 8 KiB rejected with 413 JSON ErrorBody.
    let huge_body = format!(
        r#"{{"device_key":"ed25519:huge","hostname":"{}"}}"#,
        "x".repeat(9 * 1024)
    );
    let (status, headers, body) = http_raw(
        cluster.http_addr,
        "POST",
        "/api/v1/devices/register",
        &[("Content-Type", "application/json")],
        Some(huge_body.as_bytes()),
    )
    .await;
    assert_eq!(status, 413, "oversized body > 8 KiB must be 413: {body}");
    assert!(headers
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    let err: ErrorBody = serde_json::from_str(&body).unwrap();
    assert_eq!(err.error, "payload_too_large");

    // Register 5 devices for H8, M10, and L7 checks.
    let mut registered_ids = Vec::new();
    for (idx, host) in ["alpha_node", "beta_100%_node", "gamma", "delta", "epsilon"]
        .iter()
        .enumerate()
    {
        let req = serde_json::json!({
            "device_key": format!("ed25519:secret-device-key-{idx}"),
            "hostname": host,
            "os": "linux",
            "device_group": "fleet"
        });
        let (status, body) = http_json(
            cluster.http_addr,
            "POST",
            "/api/v1/devices/register",
            Some(&req),
        )
        .await;
        assert_eq!(status, 200, "register {host}: {body}");
        let reg: RegisterResponse = serde_json::from_str(&body).unwrap();
        registered_ids.push(reg.device_id);
    }

    // H8: device_key is NOT searchable via `?q=`.
    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?q=secret-device-key",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let resp: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(resp.total, 0, "device_key must never match ?q=");

    // H8: Literal `%` only matches `beta_100%_node`, not all devices.
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/devices?q=%25", None).await;
    assert_eq!(status, 200);
    let resp: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(resp.total, 1);
    assert_eq!(resp.devices[0].hostname, "beta_100%_node");

    // M10: `?q=-` must NOT match all devices (none of the 5 hostnames/os/groups contain `-`).
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/devices?q=-", None).await;
    assert_eq!(status, 200);
    let resp: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(resp.total, 0, "?q=- must not match all devices");

    // L7: Offline devices skip `get_public_udp` lookups (even if a stale UDP entry exists in ephemeral).
    cluster
        .ephemeral
        .set_public_udp(
            &registered_ids[0],
            "203.0.113.55:44444".parse().unwrap(),
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?q=alpha_node",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let resp: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(resp.total, 1);
    assert_eq!(resp.devices[0].status, "offline");
    assert_eq!(
        resp.devices[0].public_udp_addr, None,
        "offline device must skip get_public_udp"
    );

    // L7: Pagination across all 5 devices (`limit` & `offset` + clamping).
    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?limit=2&offset=0",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let p1: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(p1.total, 2);
    assert_eq!(p1.limit, 2);
    assert_eq!(p1.offset, 0);

    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?limit=2&offset=2",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let p2: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(p2.total, 2);
    assert_eq!(p2.offset, 2);

    let (status, body) = http_json(
        cluster.http_addr,
        "GET",
        "/api/v1/devices?limit=2&offset=4",
        None,
    )
    .await;
    assert_eq!(status, 200);
    let p3: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(p3.total, 1);

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for d in p1.devices.into_iter().chain(p2.devices).chain(p3.devices) {
        assert!(seen.insert(d.device_id), "duplicate device across pages");
    }
    assert_eq!(seen.len(), 5);

    // `limit=0` clamps to 1; `limit=999` clamps to 500.
    let (status, body) = http_json(cluster.http_addr, "GET", "/api/v1/devices?limit=0", None).await;
    assert_eq!(status, 200);
    let clamped_low: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(clamped_low.limit, 1);
    assert_eq!(clamped_low.total, 1);

    let (status, body) =
        http_json(cluster.http_addr, "GET", "/api/v1/devices?limit=999", None).await;
    assert_eq!(status, 200);
    let clamped_high: DevicesResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(clamped_high.limit, 500);
    assert_eq!(clamped_high.total, 5);
}
