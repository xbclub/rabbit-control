CREATE TABLE IF NOT EXISTS devices (
    device_id CHAR(9) PRIMARY KEY,
    device_key TEXT UNIQUE NOT NULL,
    hostname TEXT NOT NULL,
    os TEXT NOT NULL,
    device_group TEXT NOT NULL DEFAULT '',
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_devices_last_seen_at ON devices (last_seen_at DESC);

CREATE TABLE IF NOT EXISTS relay_sessions_log (
    id BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    host_id CHAR(9) NOT NULL,
    client_id CHAR(9) NOT NULL,
    bytes_up BIGINT NOT NULL DEFAULT 0,
    bytes_down BIGINT NOT NULL DEFAULT 0,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_relay_sessions_log_started_at ON relay_sessions_log (started_at DESC);
CREATE INDEX IF NOT EXISTS idx_relay_sessions_log_session_id ON relay_sessions_log (session_id);
