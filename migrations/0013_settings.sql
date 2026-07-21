-- Flat key-value config for the dashboard settings pages. Keys are namespaced by
-- group, e.g. `general.site_name`, `brand.color_primary`, `currency.default`.
-- Single-tenant: brand settings live under `brand.*` — there is no brands table.
CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT,
    updated_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
