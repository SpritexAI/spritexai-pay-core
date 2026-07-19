-- SpritEXAI Pay — charges & double-entry ledger.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- Money is stored as integer minor units (e.g. poisha) — never floating point.
-- The ledger is append-only: rows are inserted, never updated or deleted. Balance
-- is enforced in application code (entries of a transaction must sum to zero) and
-- re-checkable at any time by SUM over ledger_entries.

CREATE TABLE charges (
    id              TEXT PRIMARY KEY,          -- checkout reference (opaque, client-facing)
    order_id        TEXT NOT NULL,             -- merchant's order identifier
    amount_minor    INTEGER NOT NULL CHECK (amount_minor > 0),
    currency        TEXT NOT NULL DEFAULT 'BDT',
    customer_name   TEXT,
    customer_msisdn TEXT,
    callback_url    TEXT,
    status          TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'paid', 'failed', 'expired')),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- One active charge per merchant order — protects against duplicate intents.
CREATE UNIQUE INDEX idx_charges_order ON charges (order_id);

CREATE TABLE ledger_transactions (
    id          TEXT PRIMARY KEY,
    reference   TEXT NOT NULL,                 -- charge id this transaction settles
    memo        TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_ledger_txn_ref ON ledger_transactions (reference);

-- Signed amounts: debit > 0, credit < 0. A well-formed transaction's entries
-- sum to exactly zero. Append-only.
CREATE TABLE ledger_entries (
    id              INTEGER PRIMARY KEY,
    txn_id          TEXT NOT NULL REFERENCES ledger_transactions (id),
    account         TEXT NOT NULL,
    amount_minor    INTEGER NOT NULL,
    currency        TEXT NOT NULL DEFAULT 'BDT',
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_ledger_entries_txn ON ledger_entries (txn_id);
CREATE INDEX idx_ledger_entries_account ON ledger_entries (account);
