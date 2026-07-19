-- SpritEXAI Pay — baseline schema.
-- Author: Mohammad Sijan (SpritexAI). Kept Postgres-portable: no SQLite-only syntax.
--
-- This baseline only establishes the migration lineage and an audit spine that
-- every later ledger mutation writes to. Domain tables (charges, ledger entries)
-- arrive in M1.

CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY,
    entity      TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    action      TEXT NOT NULL,
    actor       TEXT NOT NULL,
    detail      TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX idx_audit_entity ON audit_log (entity, entity_id);
