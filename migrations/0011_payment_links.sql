-- Reusable payment link: one row, many checkouts. A merchant shares `/link/:ref`
-- and every open spins up a fresh checkout. amount_minor NULL = open/customer-entered
-- amount; quantity NULL = unlimited uses.
CREATE TABLE payment_links (
    id            TEXT PRIMARY KEY,
    ref           TEXT NOT NULL,
    product_name  TEXT NOT NULL,
    amount_minor  INTEGER,
    currency      TEXT NOT NULL DEFAULT 'BDT',
    quantity      INTEGER,
    status        TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','inactive')),
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX idx_payment_links_ref ON payment_links (ref);
