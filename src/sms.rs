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
/// Regex first; on format drift the AI fallback chain takes over (Phase 2).
pub async fn ingest(db: &Db, gw: &str, raw_body: &str) -> Result<Ingested, IngestError> {
    let raw_sha256 = crypto::sign(b"sms-audit", raw_body.as_bytes());

    let (parsed, parse_source) = {
        let gateway = gateway::resolve(gw).ok_or(IngestError::UnknownGateway)?;
        let gateway_id = gateway.id();
        match gateway.parse_sms(raw_body) {
            Ok(p) => (p, "regex"),
            Err(regex_err) => {
                drop(gateway); // Box<dyn Gateway> must not cross the await below
                match crate::ai::extract(db, gateway_id, raw_body, &raw_sha256).await {
                    Some(p) => (p, "ai"),
                    None => return Err(regex_err.into()),
                }
            }
        }
    };

    let matched_charge = find_matching_charge(db, &parsed).await?;

    let event_id = format!("sms_{}", uuid::Uuid::new_v4().simple());
    let insert = sqlx::query(
        "INSERT INTO sms_events \
         (id, gateway, txn_id, amount_minor, sender_msisdn, charge_id, raw_sha256, matched, parse_source) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event_id)
    .bind(parsed.gateway)
    .bind(&parsed.txn_id)
    .bind(parsed.amount_minor)
    .bind(&parsed.sender_msisdn)
    .bind(&matched_charge)
    .bind(&raw_sha256)
    .bind(matched_charge.is_some() as i64)
    .bind(parse_source)
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

/// Canonical MSISDN for comparison: digits only, last 10 (the subscriber part).
/// Makes "+8801712345678", "8801712345678", "01712345678" all compare equal, so a
/// merchant-declared number in any format matches the SMS sender's "01…" form.
fn normalize_msisdn(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.len();
    if n > 10 {
        digits[n - 10..].to_string()
    } else {
        digits
    }
}

/// Find the ONE pending charge this settlement belongs to — or none.
///
/// Security invariant: a charge settles only on an unambiguous match. When two
/// customers have a pending charge for the exact same amount (routine for fixed-
/// price plans), amount alone can't tell them apart, so we disambiguate by the
/// SMS sender against the number the merchant declared for that charge. If that
/// still isn't a unique hit we return None and settle nothing — never guess which
/// customer paid. The prior "oldest-first" tie-break could credit the wrong one.
async fn find_matching_charge(db: &Db, parsed: &ParsedTxn) -> Result<Option<String>, sqlx::Error> {
    let candidates: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT id, customer_msisdn FROM charges \
         WHERE amount_minor = ? AND status = 'pending'",
    )
    .bind(parsed.amount_minor)
    .fetch_all(db)
    .await?;

    match candidates.as_slice() {
        [] => Ok(None),
        // Exactly one pending claim on this amount — unambiguous, settle it.
        [(id, _)] => Ok(Some(id.clone())),
        // Two+ same-amount charges: only the SMS sender can break the tie.
        _ => {
            let Some(sms_sender) = parsed.sender_msisdn.as_deref().map(normalize_msisdn) else {
                return Ok(None); // no sender to match on → refuse
            };
            let mut hits = candidates.iter().filter(|(_, declared)| {
                declared
                    .as_deref()
                    .map(normalize_msisdn)
                    .is_some_and(|d| d == sms_sender)
            });
            match (hits.next(), hits.next()) {
                (Some((id, _)), None) => Ok(Some(id.clone())), // exactly one → settle
                _ => Ok(None),                                 // zero or many → refuse
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkout::{self, CreateCheckout};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn test_db() -> Db {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    fn req(amount: f64, msisdn: &str) -> CreateCheckout {
        CreateCheckout {
            full_name: None,
            email_address: None,
            mobile_number: Some(msisdn.into()),
            amount,
            currency: "BDT".into(),
            return_url: None,
            webhook_url: None,
            metadata: serde_json::Value::Null,
        }
    }

    async fn status_of(db: &Db, pay_ref: &str) -> String {
        sqlx::query_scalar("SELECT status FROM charges WHERE pay_ref = ?")
            .bind(pay_ref)
            .fetch_one(db)
            .await
            .unwrap()
    }

    #[test]
    fn normalize_collapses_country_code_and_formatting() {
        assert_eq!(normalize_msisdn("+8801712345678"), "1712345678");
        assert_eq!(normalize_msisdn("8801712345678"), "1712345678");
        assert_eq!(normalize_msisdn("01712345678"), "1712345678");
        assert_eq!(normalize_msisdn("017-123 456 78"), "1712345678");
    }

    #[tokio::test]
    async fn unique_amount_settles_regardless_of_sender() {
        let db = test_db().await;
        let a = checkout::create_checkout(&db, "https://p", req(700.0, "01710000000"))
            .await
            .unwrap();
        // Paid from a different number than declared — still the sole claim on ৳700.
        ingest(
            &db,
            "bkash",
            "received Tk 700.00 from 01999999999. TrxID TRX700A",
        )
        .await
        .unwrap();
        assert_eq!(status_of(&db, &a.sap_id).await, "paid");
    }

    #[tokio::test]
    async fn sender_disambiguates_same_amount_charges() {
        let db = test_db().await;
        let a = checkout::create_checkout(&db, "https://p", req(1500.0, "01710000000"))
            .await
            .unwrap();
        let b = checkout::create_checkout(&db, "https://p", req(1500.0, "01820000000"))
            .await
            .unwrap();
        // SMS from B's number → B settles, A stays pending. No oldest-first guess.
        ingest(
            &db,
            "bkash",
            "received Tk 1500.00 from 01820000000. TrxID TRXB01",
        )
        .await
        .unwrap();
        assert_eq!(status_of(&db, &b.sap_id).await, "paid");
        assert_eq!(status_of(&db, &a.sap_id).await, "pending");
    }

    #[tokio::test]
    async fn ambiguous_amount_unknown_sender_refuses() {
        let db = test_db().await;
        let a = checkout::create_checkout(&db, "https://p", req(1500.0, "01710000000"))
            .await
            .unwrap();
        let b = checkout::create_checkout(&db, "https://p", req(1500.0, "01820000000"))
            .await
            .unwrap();
        // Paid from a number matching neither → refuse; nothing settles.
        let out = ingest(
            &db,
            "bkash",
            "received Tk 1500.00 from 01990000000. TrxID TRXX01",
        )
        .await
        .unwrap();
        assert!(out.matched_charge.is_none());
        assert_eq!(status_of(&db, &a.sap_id).await, "pending");
        assert_eq!(status_of(&db, &b.sap_id).await, "pending");
    }
}

/// Dashboard-safe projection of an SMS event. Deliberately omits `raw_sha256` and
/// any raw body — the audit fingerprint never leaves the server. `parse_source`
/// is intentionally excluded too; the feed shows what settled, not how it parsed.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct SmsEventView {
    pub id: String,
    pub gateway: String,
    pub txn_id: String,
    pub amount_minor: i64,
    pub sender_msisdn: Option<String>,
    pub charge_id: Option<String>,
    pub matched: i64,
    pub received_at: String,
}

/// Newest inbound SMS events first, capped at `limit`. Non-sensitive columns only.
pub async fn list_events(db: &Db, limit: i64) -> Result<Vec<SmsEventView>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, gateway, txn_id, amount_minor, sender_msisdn, charge_id, matched, received_at \
         FROM sms_events ORDER BY received_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db)
    .await
}
