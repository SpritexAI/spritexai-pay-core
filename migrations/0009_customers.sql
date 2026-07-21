-- Customers — the instance's contact address-book.
--
-- Single-tenant: this is a plain standalone list of people we've dealt with.
-- Charges already denormalize the customer fields they need, so there is
-- deliberately NO foreign key from charges into this table — deleting or
-- editing a customer never rewrites history.
CREATE TABLE customers (
    id          TEXT PRIMARY KEY,
    name        TEXT,
    email       TEXT,
    msisdn      TEXT,
    status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','inactive')),
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX idx_customers_msisdn ON customers (msisdn);
