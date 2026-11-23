# RabbitControl Rust Server Protocol & API Specification (v1)

> Status: **Active (Phase 1)**
> Applies to: `server/backend/` (`rc-server`, standalone Rust workspace)

This document defines the exact wire protocols, storage contracts, and HTTP REST API exposed by the RabbitControl Rust server (`rc-server`):

1. **Ports & Architecture Summary** (NO WebSocket)
2. **Native TCP Framed Signaling Protocol** (`:21116/tcp`, `[u32 BE len][UTF-8 JSON]`)
3. **UDP STUN-lite Public Endpoint Probe Protocol** (`:21116/udp`, `RCSP` / `RCSA`)
4. **Zero-Copy TCP Relay Protocol** (`:21117/tcp`, `RCR1` handshake + Linux `splice(2)` raw stream)
5. **Axum 0.8 REST API** (`:21118/tcp`, `/api/v1/*`)
6. **Storage Architecture** (PostgreSQL `sqlx` + Optional Redis / Default `DashMap`)

---

## 1. Ports & Default Endpoints

| Service | Default Listen | Transport | Purpose |
|---|---|---|---|
| Native TCP Signaling | `:21116` | TCP (`TCP_NODELAY=true`, TLS 1.3 default; `--no-tls` opt-out) | Framed JSON signaling (**NO WebSocket**): device/owner registration (`access_token`, `device_pubkey`), `UpdateAssistState`, `DeviceStatusChanged`, P2P punch coordination & `OwnerGrantTicket` issuance, relay token allocation, device/user kick |
| UDP STUN-lite Probe | `:21116` | UDP | Public `IP:Port` discovery and NAT mapping refresh |
| mTLS TCP Relay | `:21117` | TCP (`TCP_NODELAY=true`, mandatory TLS 1.3 mTLS) | Mutual-TLS authenticated full-duplex relay with `RCR1` header & SAN (`urn:rabbitcontrol:user:{uid}:device:{did}`) binding + serial revocation check |
| Axum 0.8 REST API / Admin Web | `:21118` | HTTP/1.1 (TLS-capable via `rc-server`) | Health check, Root CA fetch, Auth/Refresh/ChangePassword/Logout, CSR device cert issue, My Devices bind/rename/unbind, Admin user CRUD/settings/sessions/stats + Admin Web UI |

---

## 2. Native TCP Framed Signaling Protocol (`:21116/tcp`)

Signaling runs directly over TCP with `TCP_NODELAY = true`. **WebSocket is NOT used.**

### 2.1 Wire Framing

Every message in both directions is length-prefixed:

```text
+-------------------------------+----------------------------------------+
| Length (u32 big-endian, 4 B)  | UTF-8 JSON Payload (Length bytes)      |
| valid range: 1..=65536        | {"type": "...", ...}                   |
+-------------------------------+----------------------------------------+
```

- `length == 0` or `length > 65536` is a protocol error; the peer connection is closed.
- Payloads are tagged JSON objects discriminated by `"type"` (`snake_case`).
- **Backpressure isolation**: each connected peer is backed by a bounded `tokio::sync::mpsc::Sender<SignalMsg>` channel; hub fan-out uses non-blocking `try_send` so a stalled peer can never block other peers.

### 2.2 Session Lifecycle

1. Immediately after TCP connect, the client **MUST** send a `register` frame within the handshake timeout (default `10s`). Any other first frame is rejected with `error` (`code = "expected_register"`) and the connection is closed.
2. The server resolves/issues a permanent 9-digit `device_id` (`100000000..=999999999`) idempotently by `device_key` via `DeviceStore`.
   - If the device is marked `disabled = true`, the server replies with `error` (`code = "device_disabled"`) and closes the connection.
   - If the client also supplied `device_id` and it does not match the stored `device_id` for `device_key`, the server replies with `error` (`code = "device_id_mismatch"`) and closes.
   - If another TCP connection is already registered under the same `device_id`, the older connection is sent `kicked` (`reason = "replaced_by_new_connection"`) and closed.
3. On success, the server marks the device online in `EphemeralStore` and replies with `registered`.

### 2.3 Message Types

#### `register` (Client -> Server)

```json
{
  "type": "register",
  "device_id": "123456789",
  "device_key": "ed25519:7a9f2c...",
  "hostname": "macbook-pro",
  "os": "macos",
  "device_group": "default",
  "lan_addrs": ["192.168.1.10:50000"]
}
```

- `device_key` (required): stable unique identifier (1–256 chars).
- `device_id` (optional): if provided, verified against the resolved permanent 9-digit ID.
- `hostname`, `os`, `device_group` (alias `"group"`), `lan_addrs`: optional metadata.

#### `registered` (Server -> Client)

```json
{
  "type": "registered",
  "device_id": "123456789",
  "formatted_id": "123 456 789",
  "server_time": "2026-11-22T10:00:00Z",
  "udp_probe_addr": "172.16.1.2:21116",
  "relay_addr": "172.16.1.2:21117"
}
```

#### `heartbeat` (Client -> Server)

Refreshes online TTL in `EphemeralStore`, updates `last_seen_at` in `DeviceStore`, and refreshes LAN candidate addresses.

```json
{
  "type": "heartbeat",
  "lan_addrs": ["192.168.1.10:50000"]
}
```

#### `heartbeat_ack` (Server -> Client)

```json
{
  "type": "heartbeat_ack",
  "server_time": "2026-11-22T10:00:25Z",
  "public_udp_addr": "203.0.113.10:51234",
  "online_peers": 42
}
```

#### `connect_request` (Client -> Server)

Initiates P2P hole-punching coordination with `target_id`.

```json
{
  "type": "connect_request",
  "target_id": "987654321",
  "session_id": "optional-session-id",
  "lan_addrs": ["192.168.1.10:50000"],
  "nonce": "a1b2c3d4"
}
```

- If `target_id` is offline, the server responds only to the requester with `peer_offline`.
- If `target_id` is online, the server reads both sides' observed public UDP endpoints from `EphemeralStore`, allocates a one-time fallback `relay_token` in `EphemeralStore`, and pushes `punch_coord` to **both** the client (`role = "client"`) and target host (`role = "host"`).

#### `punch_coord` (Server -> Both Peers)

```json
{
  "type": "punch_coord",
  "session_id": "f3a1c9...",
  "role": "client",
  "peer_id": "987654321",
  "peer_public_udp_addr": "198.51.100.22:60123",
  "peer_lan_addrs": ["192.168.1.20:50000"],
  "nonce": "a1b2c3d4",
  "relay_token": "9f8e7d6c...",
  "relay_addr": "172.16.1.2:21117"
}
```

#### `relay_alloc` (Client -> Server)

Explicitly requests a fresh one-time TCP Relay token when P2P hole-punching fails or is bypassed.

```json
{
  "type": "relay_alloc",
  "target_id": "987654321",
  "session_id": "f3a1c9..."
}
```

- If `target_id` is offline, responds with `peer_offline`.
- Otherwise stores a new `RelayTokenRecord` in `EphemeralStore` and pushes `relay_ready` to **both** peers.

#### `relay_ready` (Server -> Both Peers)

```json
{
  "type": "relay_ready",
  "session_id": "f3a1c9...",
  "role": "client",
  "peer_id": "987654321",
  "relay_token": "4b5c6d7e...",
  "relay_addr": "172.16.1.2:21117",
  "expires_in_sec": 60
}
```

#### `peer_offline` (Server -> Client)

```json
{
  "type": "peer_offline",
  "target_id": "987654321",
  "session_id": "f3a1c9..."
}
```

#### `kicked` (Server -> Client)

Sent before the server terminates the TCP signaling connection (e.g., admin disable via `DELETE /api/v1/devices/:id`, server shutdown, or replacement by a newer connection).

```json
{
  "type": "kicked",
  "reason": "disabled_by_admin"
}
```

#### `error` (Server -> Client)

```json
{
  "type": "error",
  "code": "invalid_payload",
  "message": "target_id is required"
}
```

---

## 3. UDP STUN-lite Probe Protocol (`:21116/udp`)

A fixed-offset binary protocol used by peers to discover their NAT-mapped public UDP `IP:Port` and publish it to `EphemeralStore`. All multi-byte integers are **big-endian**.

### 3.1 Request Packet (`RCSP`, 22 bytes)

```text
+---------------+----------+-------------------------+------------------+
| Magic (4 B)   | Ver (1B) | Device ID (9 B ASCII)   | Nonce (8 B)      |
| "RCSP"        | 0x01     | e.g. "123456789"        | random 8 bytes   |
+---------------+----------+-------------------------+------------------+
```

| Offset | Size | Field | Description |
|---|---|---|---|
| `0` | 4 B | `magic` | ASCII `"RCSP"` (`0x52 0x43 0x53 0x50`) |
| `4` | 1 B | `ver` | Protocol version, MUST be `0x01` |
| `5` | 9 B | `device_id` | 9-byte ASCII digits (e.g. `"123456789"`; `"000000000"` = anonymous probe without storing endpoint) |
| `14` | 8 B | `nonce` | Random 8-byte request nonce echoed verbatim in response |

Total packet length: **22 bytes**. Malformed packets are silently dropped.

### 3.2 Response Packet (`RCSA`, 20 bytes IPv4 / 32 bytes IPv6)

```text
+---------------+----------+------------+------------------------+---------------+------------------+
| Magic (4 B)   | Ver (1B) | IPLen (1B) | IP Bytes (4 B or 16 B) | Port (u16 BE) | Nonce (8 B)      |
| "RCSA"        | 0x01     | 4 or 16    | IPv4 / IPv6 octets     | source port   | echoed 8 bytes   |
+---------------+----------+------------+------------------------+---------------+------------------+
```

| Offset | Size | Field | Description |
|---|---|---|---|
| `0` | 4 B | `magic` | ASCII `"RCSA"` (`0x52 0x43 0x53 0x41`) |
| `4` | 1 B | `ver` | Protocol version, `0x01` |
| `5` | 1 B | `ip_len` | `4` (IPv4) or `16` (IPv6) |
| `6` | `ip_len` B | `ip_bytes` | Observed source IP octets |
| `6 + ip_len` | 2 B | `port` | Observed source UDP port (`u16` big-endian) |
| `8 + ip_len` | 8 B | `nonce` | Echoed 8-byte nonce from request |

Under per-source-IP UDP rate limiting, the server immediately replies `RCSA` with the canonicalized source address (IPv4-mapped IPv6 addresses are normalized to IPv4) and then asynchronously queues the `EphemeralStore` update. When `device_id != "000000000"` and passes `9`-digit validation, `device_id -> observed_ip:port` is stored with TTL **only if** `device_id` is currently online on authenticated TCP signaling from a matching canonical IP (preventing unauthenticated UDP spoofing/poisoning and avoiding Redis head-of-line blocking on the UDP socket).

---

## 4. Zero-Copy TCP Relay Protocol (`:21117/tcp`)

### 4.1 Binary Handshake (`RCR1`)

Immediately after TCP connect, each peer sends the handshake frame within the handshake timeout (`5s`):

```text
+---------------+----------+-------------+-----------------------+
| Magic (4 B)   | Role (1B)| TokenLen(1B)| Token (TokenLen B)    |
| "RCR1"        | 1 or 2   | 1..=128     | UTF-8 / ASCII hex     |
+---------------+----------+-------------+-----------------------+
```

| Field | Size | Values |
|---|---|---|
| `magic` | 4 B | ASCII `"RCR1"` (`0x52 0x43 0x52 0x31`) |
| `role` | 1 B | `0x01` = Host (controlled), `0x02` = Client (controller) |
| `token_len` | 1 B | Length of `token` in bytes (`1..=128`) |
| `token` | `token_len` B | One-time UTF-8 `relay_token` issued by `punch_coord` or `relay_ready` |

### 4.2 Server Reply (1 Byte)

- `0x00` (`OK`): Token validated and complementary peer (`host` + `client`) paired. Immediately after reading `0x00`, the TCP connection transitions into a raw full-duplex byte stream.
- `0x01` (`ERR`): Handshake failed (invalid magic/role/token length, unknown/expired token, duplicate role, or pairing timeout). Server closes immediately after writing `0x01`.

### 4.3 Forwarding Data Path

- **Linux (`#[cfg(target_os = "linux")]`)**: Kernel-to-kernel zero-copy forwarding via `splice(2)` with a 256 KiB pipe, `SPLICE_F_MOVE | SPLICE_F_NONBLOCK`, cooperative Tokio yield every 4 MiB or 64 ops, graceful half-close (`SHUT_WR`), and automatic fallback to bounded userspace copy on resource exhaustion.
- **Non-Linux (macOS / Windows)**: Bounded userspace bidirectional copy with graceful half-close (`shutdown`).
- On session end, totals (`bytes_up`, `bytes_down`, `started_at`, `ended_at`) are persisted to PostgreSQL `relay_sessions_log`.

---

## 5. Axum 0.8 REST API (`:21118/tcp`, `/api/v1/*`)

All responses — including malformed JSON, body-limit (`413`), and auth (`401`) errors — return `Content-Type: application/json` (`{"error": "<code>", "message": "<detail>"}`).

### 5.1 Admin Authentication & CORS

- **Admin Bearer Auth (`--admin-token` / `RC_ADMIN_TOKEN`)**:
  - When configured, all admin routes (`/api/v1/stats`, `/api/v1/devices`, `/api/v1/devices/:id`, `/api/v1/sessions`, `/api/v1/sessions/:id`) require `Authorization: Bearer <token>` (verified with constant-time byte comparison). Missing or invalid tokens receive `401 Unauthorized` with JSON `ErrorBody` and `WWW-Authenticate: Bearer realm="rc-server-admin"`.
  - Public routes (`GET /api/v1/health`, `POST /api/v1/devices/register`) remain accessible without an admin token.
  - When `--admin-token` is unset, the server logs a startup warning and leaves admin routes unauthenticated.
- **CORS (`--cors-origin` / `RC_CORS_ORIGIN`)**:
  - Defaults to same-origin only (no cross-origin allowlist).
  - Accepts comma-separated origin values (e.g. `https://admin.example.com`) or explicit `*` wildcard only when explicitly requested via `--cors-origin '*'`.
  - Allowed methods: `GET`, `POST`, `DELETE`, `OPTIONS`. Allowed request headers: `Authorization`, `Content-Type`.

### 5.2 Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/api/v1/health` | Public | Liveness & dependency health (`status`, `postgres`, `ephemeral_backend`, `relay_engine`, `uptime_sec`, `version`) |
| `POST` | `/api/v1/devices/register` | Public | Idempotently register device by `device_key` and issue/return permanent 9-digit `device_id` (`400` on invalid input, `403` if disabled, `413` if body > 8 KiB) |
| `GET` | `/api/v1/stats` | Admin | Server metrics: `online_count`, `registered_count`, `active_relay_sessions`, `total_relay_sessions`, `total_relay_bytes`, `total_bytes_up`, `total_bytes_down`, `punch_coord_count`, `ephemeral_backend`, `relay_engine`, `uptime_sec` |
| `GET` | `/api/v1/devices` | Admin | Paginated device list (`?q=<search>&status=online\|offline\|in_session\|all&limit=1..=500&offset=0..`; default `limit=100`, `offset=0`). Searches `device_id`, `hostname`, `os`, `device_group`, and owner email (`owner_email`; excludes `device_key`). Invalid `status` always returns `400`. |
| `GET` | `/api/v1/sessions` | Admin | List currently active TCP relay sessions (`session_id`, `host_id`, `client_id`, `started_at`, `duration_sec`, `bytes_up`, `bytes_down`, `engine`) |
| `DELETE` | `/api/v1/sessions/:id` | Admin | Force-terminate an active relay session (`200 OK` or `404 Not Found`) |
| `DELETE` | `/api/v1/devices/:id` | Admin | Mark device `disabled = true`, kick online signaling peer (`reason = "disabled_by_admin"`), and terminate active relay sessions (`200 OK` or `404 Not Found`) |

### `POST /api/v1/devices/register`

- Request body limit: **8 KiB** (`8192` bytes).
- Validation limits (no NUL `\0` or control characters allowed): `device_key` 1–256 B, `hostname` ≤ 128 B, `os` ≤ 128 B, `device_group` ≤ 64 B.

Request:

```json
{
  "device_key": "ed25519:7a9f2c...",
  "hostname": "macbook-pro",
  "os": "macos",
  "device_group": "default"
}
```

Response (`200 OK`):

```json
{
  "device_id": "123456789",
  "formatted_id": "123 456 789",
  "device_key": "ed25519:7a9f2c...",
  "hostname": "macbook-pro",
  "os": "macos",
  "device_group": "default",
  "created_at": "2026-11-22T10:00:00Z",
  "last_seen_at": "2026-11-22T10:05:00Z",
  "signal_tcp_addr": "172.16.1.2:21116",
  "udp_probe_addr": "172.16.1.2:21116",
  "tcp_relay_addr": "172.16.1.2:21117",
  "relay_addr": "172.16.1.2:21117"
}
```

### `GET /api/v1/devices`

Query parameters:

- `q` (optional): case-insensitive literal substring search across `device_id`, `hostname`, `os`, `device_group` (SQL `LIKE` wildcards `\`, `%`, `_` are escaped; `device_key` is never searched).
- `status` (optional): `all` (default), `online`, `offline`, or `in_session`. Any other value returns `400 Bad Request` (`error = "invalid_status"`).
- `limit` (optional): page size, default `100`, clamped to `1..=500`.
- `offset` (optional): zero-based offset, default `0`.

Response (`200 OK`):

```json
{
  "devices": [
    {
      "device_id": "123456789",
      "formatted_id": "123 456 789",
      "hostname": "macbook-pro",
      "os": "macos",
      "device_group": "default",
      "disabled": false,
      "online": true,
      "in_session": false,
      "status": "online",
      "public_udp_addr": "203.0.113.10:51234",
      "lan_addrs": ["192.168.1.10:50000"],
      "owner_email": "alice@example.com",
      "created_at": "2026-11-22T10:00:00Z",
      "last_seen_at": "2026-11-22T10:05:00Z"
    }
  ],
  "total": 1,
  "total_count": 1,
  "limit": 100,
  "offset": 0
}
```

Note: `total` equals `devices.len()` (page length), while `total_count` is the total number of matched devices across all pages.

---

## 6. Storage Architecture

### 6.1 PostgreSQL (`DeviceStore` / `migrations/0001_init.sql` – `0004_session_replay_history.sql`)

- `devices`: `device_id CHAR(9) PRIMARY KEY`, `device_key TEXT UNIQUE NOT NULL`, `hostname TEXT NOT NULL`, `os TEXT NOT NULL`, `device_group TEXT NOT NULL DEFAULT ''`, `disabled BOOLEAN NOT NULL DEFAULT FALSE`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, plus `owner_user_id UUID REFERENCES users(id)`, `alias TEXT`, `device_pubkey TEXT`, `allow_remote_assist BOOLEAN`.
- `relay_sessions_log`: `id BIGSERIAL PRIMARY KEY`, `session_id TEXT NOT NULL`, `host_id CHAR(9) NOT NULL`, `client_id CHAR(9) NOT NULL`, `bytes_up BIGINT NOT NULL DEFAULT 0`, `bytes_down BIGINT NOT NULL DEFAULT 0`, `started_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `ended_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, indexed on `(started_at DESC)`, `(session_id)`, `(host_id, started_at DESC)`, and `(client_id, started_at DESC)`.
- `users`, `user_sessions` (refresh family tracking with single-use 15s grace + `retired_token_hashes` multi-generation replay detection), `system_settings` (`allow_registration`), `server_keys`, `pki_ca`, `device_certificates`, and `audit_logs` (`0003_users_auth_pki.sql`, `0004_session_replay_history.sql`).

### 6.2 Ephemeral Store (`EphemeralStore` & `SettingsCache`)

- **Default (`MemoryEphemeralStore`)**: Zero-dependency `DashMap` with per-entry `Instant` TTL expiry and periodic background cleanup. Used automatically when `--redis-url` / `REDIS_URL` is not set.
- **Optional (`RedisEphemeralStore`)**: Enabled when `--redis-url` / `REDIS_URL` is configured. Uses `redis::aio::ConnectionManager` (with bounded 2s connect/response timeouts and 1 retry) and millisecond `PX` TTLs (zero TTL executes `DEL`):
  - `rc:online:{device_id}` (`SET ... PX <ms>`) + `rc:online_zset` (Sorted Set scored by expiry unix ms; `online_ids()` prunes via `ZREMRANGEBYSCORE` and reads via `ZRANGEBYSCORE` in `O(|online|)`)
  - `rc:udp:{device_id}` (`SET ... PX <ms>`)
  - `rc:token:{token}` (`SET ... PX <ms>`, atomically consumed via `GETDEL`)
  - `rc:setting:allow_registration` (60s TTL cache backed by PostgreSQL `system_settings`, guarded by `SettingsCache` `write_epoch` against stale concurrent backfill)
  - `rc:revoked_cert:{serial_hex}` (revoked device certificate serial cache for `:21117` mTLS validation)

---

## 7. Account Auth, Admin Bootstrapping, Owner Self-Control & Mandatory Relay mTLS (`Phase 1`)

### 7.1 Initial Admin & `must_change_password` Gate
- On first startup with no admin user, `rc-server` creates the initial admin with a randomly generated password printed to server logs and sets `must_change_password = true`.
- Until `POST /api/v1/auth/change-password` succeeds, `require_user_auth_allow_pwd_change` only permits `/api/v1/me`, `/api/v1/auth/logout`, and `/api/v1/auth/change-password`, while `require_user_auth_full` and `require_admin_auth` fail closed with `403` (`password_change_required`).

### 7.2 REST Auth, Device Binding, PKI & Admin Routes (`:21118`)
- **Public**: `GET /api/v1/health`, `GET /api/v1/pki/ca`, `POST /api/v1/devices/register`, `GET /api/v1/auth/config`, `POST /api/v1/auth/register` (gated by `allow_registration`), `POST /api/v1/auth/login`, `POST /api/v1/auth/refresh`.
- **Restricted User (allowed before password change)**: `GET /api/v1/me`, `POST /api/v1/auth/logout` (accepts Bearer token or `refresh_token` fallback with explicit `device_id` ownership pre-check via `verify_explicit_device_owner`), `POST /api/v1/auth/change-password` (revokes other sessions and kicks signaling/relay peers).
- **Full User**: `GET /api/v1/me/devices`, `POST /api/v1/me/devices/bind` (atomic ownership transfer via `bind_device_to_user_with_prev`), `PATCH / DELETE /api/v1/me/devices/{id}`, `POST /api/v1/certs/issue` (CSR verification -> 7-day device cert with URI SAN `urn:rabbitcontrol:user:{user_id}:device:{device_id}`).
- **Admin**: `GET / PATCH /api/v1/admin/settings` (`allow_registration`, `updated_at`), `GET / POST /api/v1/admin/users`, `PATCH / DELETE /api/v1/admin/users/{id}`, `GET /api/v1/stats`, `GET /api/v1/devices`, `GET / DELETE /api/v1/sessions`, `DELETE /api/v1/devices/{id}`.

### 7.3 Signaling Extensions (`:21116`) & Owner Self-Control
- **TLS 1.3**: Enabled by default using server leaf cert signed by `pki_ca` (`--no-tls` opt-out for plaintext dev).
- **`Register`**: Accepts `access_token`, `device_pubkey`, and `allow_remote_assist`. Missing/invalid/revoked sessions return `Error` (`account_disabled`, `user_deleted`, `token_revoked`). Re-binding a device across users atomically transfers ownership (`bind_device_to_user_with_prev`), revokes the previous owner's certificates and sessions for that device, and preserves `effective_owner`.
- **Owner Self-Control**: `authorize_peer_connection` loads caller record and enforces owner consistency; when caller and target device belong to the same `owner_user_id` and both have verified `device_pubkey`, signaling bypasses `allow_remote_assist == false` and issues a signed `OwnerGrantTicket` (`RC-OWNER-GRANT-V1\0`) in `PunchCoord`/`RelayReady`, paired with client Ed25519 `OwnerTicket` PoP inside `rc-session`.
- **Dynamic Kick Reasons**: `account_disabled`, `user_deleted`, `account_updated`, `password_changed`, `token_revoked`, `device_unbound`, `cert_revoked`, `disabled_by_admin`.

### 7.4 Mandatory Relay mTLS (`:21117`) & Direct P2P/LAN mTLS
- `:21117` enforces TLS 1.3 `WebPkiClientVerifier` client authentication against the Root CA, extracts SAN `urn:rabbitcontrol:user:{uid}:device:{did}` (`eq_ignore_ascii_case` for user ID), verifies `device_id == RelayTokenRecord.peer_id`, and checks `rc:revoked_cert:{serial_hex}` + PostgreSQL `device_certificates`.
- Direct P2P QUIC and Relay inner E2EE both support mutual TLS with `DEFAULT_ENCRYPT_CONNECTION = true`.
