-- SpritEXAI Pay — inbound MFS SMS events.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- Idempotency lives here, at the storage layer: (gateway, txn_id) is UNIQUE, so a
-- replayed SMS can never settle a charge twice regardless of how many times the
-- Android forwarder delivers it.
--
-- We deliberately do NOT persist raw SMS plaintext (PRD security requirement:
-- "no plaintext storage beyond what's needed for audit"). Instead we keep a
-- SHA-256 of the raw text for audit correlation and dispute lookup. When Phase-2
-- adaptive parsing needs the raw body back, add an encrypted-at-rest column then.

CREATE TABLE sms_events (
    id           TEXT PRIMARY KEY,
    gateway      TEXT NOT NULL,                 -- 'bkash' | 'nagad'
    txn_id       TEXT NOT NULL,                 -- MFS transaction id parsed from the SMS
    amount_minor INTEGER NOT NULL,
    sender_msisdn TEXT,
    charge_id    TEXT REFERENCES charges (id),  -- matched charge, if any
    raw_sha256   TEXT NOT NULL,                 -- audit fingerprint of the original SMS
    matched      INTEGER NOT NULL DEFAULT 0,    -- 1 once reconciled to a charge
    received_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- The idempotency guard.
CREATE UNIQUE INDEX idx_sms_dedup ON sms_events (gateway, txn_id);
CREATE INDEX idx_sms_charge ON sms_events (charge_id);
