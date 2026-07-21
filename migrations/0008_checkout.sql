-- SpritEXAI Pay — merchant API keys + hosted checkout fields.
-- Author: Mohammad Sijan (SpritexAI). Postgres-portable.
--
-- This is the drop-in layer: a merchant site calls the checkout API with an API
-- key, gets a hosted pay URL, and redirects the customer. Mirrors PipraPay's
-- contract so existing integrations work by only swapping the base URL.

-- API keys authenticate merchant → engine calls. Stored hashed (SHA-256 via the
-- same keyed digest as device tokens); the raw key is shown once at creation.
CREATE TABLE api_keys (
    id           TEXT PRIMARY KEY,
    key_sha256   TEXT NOT NULL,                 -- keyed digest, never the raw key
    label        TEXT,
    scopes       TEXT NOT NULL DEFAULT '[]',    -- JSON array: create_payment / verify_payment
    status       TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'revoked')),
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    last_used_at TEXT
);

CREATE UNIQUE INDEX idx_api_keys_hash ON api_keys (key_sha256);

-- Hosted-checkout fields on the existing charge. All additive + nullable so old
-- rows and the direct charge API are unaffected.
ALTER TABLE charges ADD COLUMN pay_ref        TEXT;   -- public checkout id (opaque, in the pay URL)
ALTER TABLE charges ADD COLUMN customer_email TEXT;
ALTER TABLE charges ADD COLUMN return_url     TEXT;   -- where to send the customer after paying
ALTER TABLE charges ADD COLUMN webhook_url    TEXT;   -- merchant notify URL (maps onto callback_url delivery)
ALTER TABLE charges ADD COLUMN metadata       TEXT;   -- opaque merchant JSON, echoed back on verify
ALTER TABLE charges ADD COLUMN gateway        TEXT;   -- MFS the customer picked on the checkout page
ALTER TABLE charges ADD COLUMN claimed_trx_id TEXT;   -- manual fallback: customer-entered TrxID (a hint, never auto-settles)
ALTER TABLE charges ADD COLUMN claimed_sender TEXT;

CREATE INDEX idx_charges_pay_ref ON charges (pay_ref);
