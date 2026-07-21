CREATE TABLE invoices (
    id            TEXT PRIMARY KEY,
    number        TEXT NOT NULL,
    customer_id   TEXT,
    amount_minor  INTEGER NOT NULL CHECK (amount_minor > 0),
    currency      TEXT NOT NULL DEFAULT 'BDT',
    status        TEXT NOT NULL DEFAULT 'unpaid' CHECK (status IN ('unpaid','paid','refunded','canceled')),
    charge_id     TEXT,
    pay_ref       TEXT,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE UNIQUE INDEX idx_invoices_number ON invoices (number);
CREATE INDEX idx_invoices_charge ON invoices (charge_id);

CREATE TABLE invoice_items (
    id            TEXT PRIMARY KEY,
    invoice_id    TEXT NOT NULL REFERENCES invoices (id),
    description   TEXT NOT NULL,
    quantity      INTEGER NOT NULL DEFAULT 1,
    unit_minor    INTEGER NOT NULL,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX idx_invoice_items_inv ON invoice_items (invoice_id);
