-- checkout return_url/webhook_url host whitelist; empty table = allow all (non-breaking for existing deploys).
CREATE TABLE allowed_domains (
    id          TEXT PRIMARY KEY,
    domain      TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX idx_allowed_domains ON allowed_domains (domain);
