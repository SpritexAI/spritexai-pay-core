-- SpritEXAI Pay — outbound webhook delivery queue.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- This is the durable retry queue for merchant callbacks. When REDIS_URL is unset
-- (single-instance self-hosted) this table IS the queue: a background worker polls
-- for due rows, delivers, and reschedules with exponential backoff. Redis only
-- becomes necessary for multi-instance Cloud deployments where several workers
-- must coordinate — then this table stays as the source of truth and Redis fronts
-- the scheduling.

CREATE TABLE webhook_deliveries (
    id            TEXT PRIMARY KEY,
    charge_id     TEXT NOT NULL REFERENCES charges (id),
    url           TEXT NOT NULL,
    payload       TEXT NOT NULL,               -- exact JSON bytes that get signed
    event         TEXT NOT NULL,               -- e.g. 'charge.paid'
    status        TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'delivered', 'failed')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    max_attempts  INTEGER NOT NULL DEFAULT 8,
    next_attempt_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_error    TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Worker polls: pending rows whose next_attempt_at has passed, oldest first.
CREATE INDEX idx_webhook_due ON webhook_deliveries (status, next_attempt_at);
