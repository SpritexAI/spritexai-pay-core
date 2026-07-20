//! Fraud / anomaly detection.
//!
//! Deterministic rules over recorded SMS events and charges — no AI round-trip.
//! Money-fraud signals must be auditable and reproducible, so this is plain SQL a
//! human can reason about, not a model verdict. The AI layer stays on the parsing
//! side (drift recovery); detection stays on rules. Author: Mohammad Sijan (SpritexAI).
//!
//! Rules:
//!   1. Duplicate TXID across gateways — one transaction id seen on >1 gateway
//!      (per-gateway idempotency can't catch this; a real duplicate is suspicious).
//!   2. Sender mismatch — the paying number differs from the charge's expected
//!      customer number.
//!   3. Amount anomaly — a settled amount far above the recent norm.

use crate::db::Db;
use serde::Serialize;

/// How many times the average a single settled amount must exceed to be flagged.
/// ponytail: fixed multiplier over the running average — swap for a rolling
/// median/MAD if fixed thresholds prove noisy on real traffic.
const AMOUNT_ANOMALY_MULTIPLE: f64 = 10.0;
/// Don't call anything an amount outlier until there's enough history to have a norm.
const MIN_SAMPLE_FOR_AMOUNT: i64 = 5;

#[derive(Debug, Serialize, PartialEq)]
pub struct Anomaly {
    pub kind: &'static str,
    pub severity: &'static str,
    pub detail: String,
    pub txn_id: Option<String>,
    pub charge_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FraudReport {
    pub anomaly_count: usize,
    pub anomalies: Vec<Anomaly>,
}

pub async fn scan(db: &Db) -> Result<FraudReport, sqlx::Error> {
    let mut anomalies = Vec::new();
    anomalies.extend(duplicate_txids(db).await?);
    anomalies.extend(sender_mismatches(db).await?);
    anomalies.extend(amount_outliers(db).await?);

    Ok(FraudReport {
        anomaly_count: anomalies.len(),
        anomalies,
    })
}

/// Same TXID recorded under more than one gateway. The `(gateway, txn_id)` unique
/// index permits this (it only dedups within a gateway), so it surfaces here.
async fn duplicate_txids(db: &Db) -> Result<Vec<Anomaly>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT txn_id, COUNT(DISTINCT gateway) AS n FROM sms_events \
         GROUP BY txn_id HAVING n > 1",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(txn_id, n)| Anomaly {
            kind: "duplicate_txid",
            severity: "high",
            detail: format!("txn_id seen on {n} different gateways"),
            txn_id: Some(txn_id),
            charge_id: None,
        })
        .collect())
}

/// A matched SMS whose paying number disagrees with the charge's expected customer.
async fn sender_mismatches(db: &Db) -> Result<Vec<Anomaly>, sqlx::Error> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT s.charge_id, s.sender_msisdn, c.customer_msisdn \
         FROM sms_events s JOIN charges c ON s.charge_id = c.id \
         WHERE c.customer_msisdn IS NOT NULL AND s.sender_msisdn IS NOT NULL \
           AND s.sender_msisdn <> c.customer_msisdn",
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(charge_id, got, expected)| Anomaly {
            kind: "sender_mismatch",
            severity: "medium",
            detail: format!("paid by {got}, charge expected {expected}"),
            txn_id: None,
            charge_id: Some(charge_id),
        })
        .collect())
}

/// Settled amounts far above the running average of matched inflow.
async fn amount_outliers(db: &Db) -> Result<Vec<Anomaly>, sqlx::Error> {
    let (count, avg): (i64, f64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(AVG(amount_minor), 0) FROM sms_events WHERE matched = 1",
    )
    .fetch_one(db)
    .await?;

    if count < MIN_SAMPLE_FOR_AMOUNT {
        return Ok(Vec::new());
    }
    let threshold = (avg * AMOUNT_ANOMALY_MULTIPLE).round() as i64;

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT txn_id, amount_minor FROM sms_events WHERE matched = 1 AND amount_minor > ?",
    )
    .bind(threshold)
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(txn_id, amount)| Anomaly {
            kind: "amount_anomaly",
            severity: "medium",
            detail: format!(
                "amount {amount} minor is >{AMOUNT_ANOMALY_MULTIPLE}× the average of {}",
                avg.round() as i64
            ),
            txn_id: Some(txn_id),
            charge_id: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{charge, sms};
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    async fn test_db() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    async fn charge_for(db: &SqlitePool, order: &str, amt: i64, msisdn: Option<&str>) {
        charge::create(
            db,
            charge::CreateCharge {
                order_id: order.into(),
                amount_minor: amt,
                currency: "BDT".into(),
                customer_name: None,
                customer_msisdn: msisdn.map(str::to_string),
                callback_url: None,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn flags_sender_mismatch() {
        let db = test_db().await;
        // Charge expects 019..., SMS is paid from 017... .
        charge_for(&db, "M-1", 50_000, Some("01900000000")).await;
        sms::ingest(
            &db,
            "bkash",
            "received Tk 500.00 from 01710000000. TrxID MM1",
        )
        .await
        .unwrap();

        let report = scan(&db).await.unwrap();
        assert!(report.anomalies.iter().any(|a| a.kind == "sender_mismatch"));
    }

    #[tokio::test]
    async fn clean_traffic_has_no_anomalies() {
        let db = test_db().await;
        charge_for(&db, "C-1", 50_000, Some("01710000000")).await;
        sms::ingest(
            &db,
            "bkash",
            "received Tk 500.00 from 01710000000. TrxID CT1",
        )
        .await
        .unwrap();

        let report = scan(&db).await.unwrap();
        assert_eq!(report.anomaly_count, 0, "matching sender, no dup, one txn");
    }
}
