//! Reconciliation queries over the ledger and settled events.
//!
//! Phase-2 exposes this same data through the natural-language chat interface;
//! here it's a plain structured endpoint so merchants (and the dashboard) can pull
//! totals without an AI round-trip.

use crate::db::Db;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub gateway: Option<String>,
    pub total_settled_minor: i64,
    pub settled_count: i64,
    pub pending_charges: i64,
}

/// Summarize settled inflow, optionally scoped to one gateway. Amounts sum the
/// recorded SMS events that matched a charge — the authoritative "money in" signal.
pub async fn reconcile(db: &Db, gateway: Option<&str>) -> Result<Reconciliation, sqlx::Error> {
    let (total, count): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(amount_minor), 0), COUNT(*) FROM sms_events \
         WHERE matched = 1 AND (?1 IS NULL OR gateway = ?1)",
    )
    .bind(gateway)
    .fetch_one(db)
    .await?;

    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM charges WHERE status = 'pending'")
        .fetch_one(db)
        .await?;

    Ok(Reconciliation {
        gateway: gateway.map(str::to_string),
        total_settled_minor: total,
        settled_count: count,
        pending_charges: pending,
    })
}
