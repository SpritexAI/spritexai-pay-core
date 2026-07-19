-- SpritEXAI Pay — gateway configuration and paired devices.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- gateway_configs: per-merchant instance of a gateway (e.g. the bKash number that
-- receives payments). devices: Android SMS forwarders linked via one-time QR/token
-- pairing. A device holds a scoped, revocable token; multiple devices/SIMs can feed
-- one merchant account.

CREATE TABLE gateway_configs (
    id           TEXT PRIMARY KEY,
    gateway      TEXT NOT NULL,                 -- 'bkash' | 'nagad'
    label        TEXT,                          -- merchant-facing name
    account_msisdn TEXT,                        -- the MFS number receiving funds
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE devices (
    id           TEXT PRIMARY KEY,
    label        TEXT,
    token_sha256 TEXT NOT NULL,                 -- pairing token stored hashed, never plaintext
    status       TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'revoked')),
    last_seen_at TEXT,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE UNIQUE INDEX idx_devices_token ON devices (token_sha256);
