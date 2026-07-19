//! Inbound SMS ingestion and reconciliation.
//!
//! Flow: parsed transaction arrives → record it idempotently (the `(gateway,
//! txn_id)` unique index is the guard) → if it matches an open charge by amount,
//! settle that charge into the ledger. A replayed SMS short-circuits at the
//! idempotency check and never double-credits.

use crate::charge;
use crate::crypto;
use crate::db::Db;
use crate::gateway::{self, ParsedTxn};

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("unknown gateway")]
    UnknownGateway,
    #[error("could not parse SMS: {0}")]
    Parse(#[from] gateway::ParseError),
    #[error("duplicate transaction (already processed)")]
    Duplicate,
    #[error(transparent)]
    Charge(#[from] charge::ChargeError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug)]
pub struct Ingested {
    pub sms_event_id: String,
    pub txn_id: String,
    pub matched_charge: Option<String>,
}

/// Parse a raw SMS for the named gateway, store it idempotently, and reconcile.
pub async fn ingest(db: &Db, gw: &str, raw_body: &str) -> Result<Ingested, IngestError> {
    let parsed = {
        let gateway = gateway::resolve(gw).ok_or(IngestError::UnknownGateway)?;
        gateway.parse_sms(raw_body)?
    };

    let raw_sha256 = crypto::sign(b"sms-audit", raw_body.as_bytes());
    let matched_charge = find_matching_charge(db, &parsed).await?;

    let event_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
    let insert = sqlx::query(
        "INSERT INTO sms_events \
         (id, gateway, txn_id, amount_minor, sender_msisdn, charge_id, raw_sha256, matched) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(parsed.gateway)
    .bind(&parsed.txn_id)
    .bind(parsed.amount_minor)
    .bind(&parsed.sender_msisdn)
    .bind(&matched_charge)
    .bind(&raw_sha256)
    .bind(matched_charge.is_some() as i64)
    .execute(db)
    .await;

    if let Err(sqlx::Error::Database(ref e)) = insert {
        if e.is_unique_violation() {
            return Err(IngestError::Duplicate);
        }
    }
    insert?;

    if let Some(ref charge_id) = matched_charge {
        charge::mark_paid(db, charge_id).await?;
    }

    Ok(Ingested {
        sms_event_id: event_id,
        txn_id: parsed.txn_id,
        matched_charge,
    })
}

/// Match by exact amount against a still-pending charge. Amount-only matching is
/// the v1 heuristic; Phase-2 fraud detection tightens this with sender/history.
// ponytail: amount-only match, add sender+time correlation when fraud layer lands.
async fn find_matching_charge(db: &Db, parsed: &ParsedTxn) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM charges WHERE amount_minor = ? AND status = 'pending' \
         ORDER BY created_at ASC LIMIT 1",
    )
    .bind(parsed.amount_minor)
    .fetch_optional(db)
    .await
}
