CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'user',
    status TEXT NOT NULL DEFAULT 'active',
    must_change_password BOOLEAN NOT NULL DEFAULT FALSE,
    token_version BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS user_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id CHAR(9),
    family_id UUID NOT NULL,
    token_hash CHAR(64) NOT NULL UNIQUE,
    prev_token_hash CHAR(64),
    prev_grace_until TIMESTAMPTZ,
    rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    client_info TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_user_sessions_user_id ON user_sessions (user_id);
CREATE INDEX IF NOT EXISTS idx_user_sessions_family_id ON user_sessions (family_id);
CREATE INDEX IF NOT EXISTS idx_user_sessions_prev_token_hash ON user_sessions (prev_token_hash);

CREATE TABLE IF NOT EXISTS system_settings (
    key TEXT PRIMARY KEY,
    value_json JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_by UUID REFERENCES users(id) ON DELETE SET NULL
);

INSERT INTO system_settings (key, value_json)
VALUES ('allow_registration', 'true'::jsonb)
ON CONFLICT (key) DO NOTHING;

CREATE TABLE IF NOT EXISTS server_keys (
    key_name TEXT PRIMARY KEY,
    algorithm TEXT NOT NULL,
    private_key_der BYTEA NOT NULL,
    public_key_der BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pki_ca (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    cert_pem TEXT NOT NULL,
    key_pem TEXT NOT NULL,
    fingerprint_sha256 CHAR(64) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS device_certificates (
    serial_hex TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id CHAR(9) NOT NULL,
    fingerprint_sha256 CHAR(64) NOT NULL,
    cert_pem TEXT NOT NULL,
    issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    revoke_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_device_certificates_user_id ON device_certificates (user_id);
CREATE INDEX IF NOT EXISTS idx_device_certificates_device_id ON device_certificates (device_id);
CREATE INDEX IF NOT EXISTS idx_device_certificates_revoked_at ON device_certificates (revoked_at);

CREATE TABLE IF NOT EXISTS audit_logs (
    id BIGSERIAL PRIMARY KEY,
    actor_user_id UUID,
    target_device_id CHAR(9),
    action TEXT NOT NULL,
    detail_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE devices
    ADD COLUMN IF NOT EXISTS owner_user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN IF NOT EXISTS alias TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS device_pubkey TEXT NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS allow_remote_assist BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS bound_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_devices_owner_user_id ON devices (owner_user_id);
