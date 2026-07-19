//! Outbound webhook dispatcher.
//!
//! Merchant callbacks are delivered at-least-once with exponential backoff. The
//! `webhook_deliveries` table is the durable queue; a background worker claims due
//! rows, POSTs the signed payload, and reschedules on failure until `max_attempts`
//! is reached. Every request carries an `X-SpritexAI-Signature` header — HMAC-SHA256
//! over the exact payload bytes — so merchants can verify authenticity.

use crate::crypto;
use crate::db::Db;
use std::time::Duration;

const SIGNATURE_HEADER: &str = "X-SpritexAI-Signature";
const IDEMPOTENCY_HEADER: &str = "X-SpritexAI-Delivery";
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Queue a webhook for delivery. The payload is stored verbatim so the signature
/// the worker computes matches the bytes the merchant receives.
pub async fn enqueue(
    db: &Db,
    charge_id: &str,
    url: &str,
    event: &str,
    payload: &str,
) -> Result<String, sqlx::Error> {
    let id = format!("whd_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO webhook_deliveries (id, charge_id, url, payload, event) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(charge_id)
    .bind(url)
    .bind(payload)
    .bind(event)
    .execute(db)
    .await?;
    Ok(id)
}

/// Exponential backoff with a ceiling: 2^attempts seconds, capped at 1 hour.
fn backoff_seconds(attempts: i64) -> i64 {
    2i64.saturating_pow(attempts.min(20) as u32).min(3600)
}

#[cfg(test)]
mod tests {
    use super::backoff_seconds;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_seconds(1), 2);
        assert_eq!(backoff_seconds(4), 16);
        // Monotonic non-decreasing.
        for a in 1..30 {
            assert!(backoff_seconds(a) <= backoff_seconds(a + 1));
        }
        // Never exceeds the 1h ceiling, never overflows.
        assert_eq!(backoff_seconds(1_000), 3600);
    }
}

#[derive(sqlx::FromRow)]
struct Due {
    id: String,
    url: String,
    payload: String,
    attempts: i64,
    max_attempts: i64,
}

/// Spawn the background delivery worker. Returns immediately; the loop runs until
/// the process exits. A single worker is correct for single-instance deployments;
/// Redis-based coordination is added for multi-instance Cloud.
// ponytail: one in-process worker, polling. Redis-fronted scheduling when Cloud
// needs multiple instances to share the queue.
pub fn spawn_worker(db: Db, secret: String) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        loop {
            if let Err(e) = drain_due(&db, &client, &secret).await {
                tracing::error!(error = %e, "webhook worker poll failed");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn drain_due(db: &Db, client: &reqwest::Client, secret: &str) -> Result<(), sqlx::Error> {
    let due: Vec<Due> = sqlx::query_as(
        "SELECT id, url, payload, attempts, max_attempts FROM webhook_deliveries \
         WHERE status = 'pending' AND next_attempt_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
         ORDER BY next_attempt_at ASC LIMIT 20",
    )
    .fetch_all(db)
    .await?;

    for d in due {
        deliver_one(db, client, secret, d).await?;
    }
    Ok(())
}

async fn deliver_one(
    db: &Db,
    client: &reqwest::Client,
    secret: &str,
    d: Due,
) -> Result<(), sqlx::Error> {
    let signature = crypto::sign(secret.as_bytes(), d.payload.as_bytes());
    let attempts = d.attempts + 1;

    let outcome = client
        .post(&d.url)
        .header("content-type", "application/json")
        .header(SIGNATURE_HEADER, &signature)
        .header(IDEMPOTENCY_HEADER, &d.id)
        .body(d.payload.clone())
        .send()
        .await;

    let success = matches!(&outcome, Ok(r) if r.status().is_success());

    if success {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempts = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
        )
        .bind(attempts)
        .bind(&d.id)
        .execute(db)
        .await?;
        tracing::info!(delivery = %d.id, attempts, "webhook delivered");
        return Ok(());
    }

    let err_msg = match outcome {
        Ok(r) => format!("http {}", r.status()),
        Err(e) => e.to_string(),
    };

    // Exhausted → mark failed. Otherwise reschedule with backoff.
    if attempts >= d.max_attempts {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'failed', attempts = ?, last_error = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
        )
        .bind(attempts)
        .bind(&err_msg)
        .bind(&d.id)
        .execute(db)
        .await?;
        tracing::warn!(delivery = %d.id, attempts, error = %err_msg, "webhook exhausted retries");
    } else {
        let delay = backoff_seconds(attempts);
        sqlx::query(
            "UPDATE webhook_deliveries SET attempts = ?, last_error = ?, \
             next_attempt_at = strftime('%Y-%m-%dT%H:%M:%fZ','now', '+' || ? || ' seconds'), \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
        )
        .bind(attempts)
        .bind(&err_msg)
        .bind(delay)
        .bind(&d.id)
        .execute(db)
        .await?;
        tracing::info!(delivery = %d.id, attempts, retry_in = delay, error = %err_msg, "webhook retry scheduled");
    }
    Ok(())
}
