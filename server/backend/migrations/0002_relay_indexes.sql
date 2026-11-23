CREATE INDEX IF NOT EXISTS idx_relay_log_host_started
    ON relay_sessions_log (host_id, started_at DESC);

CREATE INDEX IF NOT EXISTS idx_relay_log_client_started
    ON relay_sessions_log (client_id, started_at DESC);

UPDATE relay_sessions_log
SET ended_at = COALESCE(started_at, NOW())
WHERE ended_at IS NULL;

ALTER TABLE relay_sessions_log
    ALTER COLUMN ended_at SET DEFAULT NOW(),
    ALTER COLUMN ended_at SET NOT NULL;
