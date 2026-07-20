-- SpritEXAI Pay — Phase 2: adaptive AI parsing.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- When regex parsing fails on a drifted SMS format, the AI fallback attempts
-- structured extraction. Every attempt is logged here — successes become training
-- signal for suggesting regex updates, failures show which formats we still miss.

CREATE TABLE ai_parse_log (
    id           INTEGER PRIMARY KEY,
    gateway      TEXT NOT NULL,
    raw_sha256   TEXT NOT NULL,               -- fingerprint of the SMS that regex missed
    provider     TEXT NOT NULL,               -- which AI provider answered
    success      INTEGER NOT NULL DEFAULT 0,
    txn_id       TEXT,
    amount_minor INTEGER,
    sender_msisdn TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_ai_parse_gateway ON ai_parse_log (gateway, success);

-- Track how each stored SMS event was parsed ('regex' | 'ai').
ALTER TABLE sms_events ADD COLUMN parse_source TEXT NOT NULL DEFAULT 'regex';
