-- Track rotated refresh token hashes per session to catch multi-generation token replay
-- across the full token family.

ALTER TABLE user_sessions
    ADD COLUMN IF NOT EXISTS retired_token_hashes TEXT[] NOT NULL DEFAULT '{}';
