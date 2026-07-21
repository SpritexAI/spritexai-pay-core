//! Hosted checkout — the customer-facing payment flow.
//!
//! A merchant creates a checkout via the API (using the common PHP SMS-gateway
//! integration shape) and gets
//! back a public `pay_ref` + hosted URL. The customer opens that page, picks an MFS,
//! sends money to the merchant's registered number, and the page polls until the
//! SMS forwarder settles the charge. A manual TrxID box is a fallback hint only —
//! SMS remains the single source of truth for settlement. Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateCheckout {
    pub full_name: Option<String>,
    pub email_address: Option<String>,
    pub mobile_number: Option<String>,
    pub amount: f64,
    #[serde(default = "default_currency")]
    pub currency: String,
    pub return_url: Option<String>,
    pub webhook_url: Option<String>,
    /// Opaque merchant JSON, echoed back on verify. Accepts an object or a string.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

fn default_currency() -> String {
    "BDT".into()
}

#[derive(Debug, Serialize)]
pub struct CheckoutCreated {
    pub sap_id: String,
    pub sap_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    #[error("amount must be positive")]
    InvalidAmount,
    #[error("checkout not found")]
    NotFound,
    #[error("return_url or webhook_url host is not whitelisted")]
    DomainNotAllowed,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Public, non-sensitive view the checkout page renders. No secrets, no webhook url.
#[derive(Debug, Serialize)]
pub struct PublicCharge {
    pub pay_ref: String,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub gateway: Option<String>,
    pub return_url: Option<String>,
    /// Registered receiving numbers keyed by gateway id (bkash/nagad).
    pub receivers: Vec<Receiver>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Receiver {
    pub gateway: String,
    pub label: Option<String>,
    pub account_msisdn: Option<String>,
}

/// Create a checkout charge and return `{sap_id, sap_url}` (SpritexAI Pay).
/// The response shape mirrors the common PHP SMS-gateway create call (field names
/// rebranded), so such an integration ports by renaming two keys.
/// `webhook_url` is stored on `callback_url` too, so the existing durable webhook
/// worker delivers `charge.paid` on settlement with no new plumbing.
pub async fn create_checkout(
    db: &Db,
    base_url: &str,
    req: CreateCheckout,
) -> Result<CheckoutCreated, CheckoutError> {
    if !req.amount.is_finite() || req.amount <= 0.0 {
        return Err(CheckoutError::InvalidAmount);
    }
    // SSRF / open-redirect guard: both URLs must resolve to a whitelisted host.
    // Empty whitelist = allow all (non-breaking for existing deploys).
    if !crate::domain::is_allowed(db, req.return_url.as_deref()).await?
        || !crate::domain::is_allowed(db, req.webhook_url.as_deref()).await?
    {
        return Err(CheckoutError::DomainNotAllowed);
    }
    let amount_minor = (req.amount * 100.0).round() as i64;

    let id = format!("chg_{}", uuid::Uuid::new_v4().simple());
    // Public reference in the pay URL — opaque, distinct from the internal id.
    let pay_ref = format!("pay_{}", uuid::Uuid::new_v4().simple());
    // order_id carries the UNIQUE constraint; the pay_ref is naturally unique.
    let metadata = if req.metadata.is_null() {
        None
    } else {
        Some(req.metadata.to_string())
    };

    sqlx::query(
        "INSERT INTO charges \
         (id, order_id, amount_minor, currency, customer_name, customer_msisdn, \
          customer_email, callback_url, webhook_url, return_url, metadata, pay_ref) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&pay_ref) // order_id = pay_ref (unique)
    .bind(amount_minor)
    .bind(&req.currency)
    .bind(&req.full_name)
    .bind(&req.mobile_number)
    .bind(&req.email_address)
    .bind(&req.webhook_url) // callback_url — drives the existing webhook worker
    .bind(&req.webhook_url)
    .bind(&req.return_url)
    .bind(&metadata)
    .bind(&pay_ref)
    .execute(db)
    .await?;

    Ok(CheckoutCreated {
        sap_id: pay_ref.clone(),
        sap_url: format!("{}/pay/{}", base_url.trim_end_matches('/'), pay_ref),
    })
}

/// The public view for the checkout page, plus the merchant's registered receivers.
pub async fn get_public_charge(db: &Db, pay_ref: &str) -> Result<PublicCharge, CheckoutError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        amount_minor: i64,
        currency: String,
        status: String,
        gateway: Option<String>,
        return_url: Option<String>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT amount_minor, currency, status, gateway, return_url \
         FROM charges WHERE pay_ref = ?",
    )
    .bind(pay_ref)
    .fetch_optional(db)
    .await?;

    let Some(c) = row else {
        return Err(CheckoutError::NotFound);
    };

    let receivers: Vec<Receiver> = sqlx::query_as(
        "SELECT gateway, label, account_msisdn FROM gateway_configs \
         WHERE enabled = 1 ORDER BY gateway",
    )
    .fetch_all(db)
    .await?;

    Ok(PublicCharge {
        pay_ref: pay_ref.to_string(),
        amount_minor: c.amount_minor,
        currency: c.currency,
        status: c.status,
        gateway: c.gateway,
        return_url: c.return_url,
        receivers,
    })
}

/// Record which MFS the customer chose (display only — settlement is amount-matched).
pub async fn select_gateway(db: &Db, pay_ref: &str, gateway: &str) -> Result<(), CheckoutError> {
    let done =
        sqlx::query("UPDATE charges SET gateway = ? WHERE pay_ref = ? AND status = 'pending'")
            .bind(gateway)
            .bind(pay_ref)
            .execute(db)
            .await?;
    if done.rows_affected() == 0 {
        return Err(CheckoutError::NotFound);
    }
    Ok(())
}

/// Store a customer-entered TrxID as a reconciliation hint. Deliberately does NOT
/// settle the charge — a customer must never be able to mark themselves paid.
pub async fn submit_manual(
    db: &Db,
    pay_ref: &str,
    trx_id: &str,
    sender: Option<&str>,
) -> Result<(), CheckoutError> {
    let done = sqlx::query(
        "UPDATE charges SET claimed_trx_id = ?, claimed_sender = ? \
         WHERE pay_ref = ? AND status = 'pending'",
    )
    .bind(trx_id)
    .bind(sender)
    .bind(pay_ref)
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(CheckoutError::NotFound);
    }
    Ok(())
}

/// Verify payload for `POST /api/verify-payment` (SpritexAI Pay).
#[derive(Debug, Serialize)]
pub struct VerifyResult {
    pub sap_id: String,
    pub full_name: Option<String>,
    pub email_address: Option<String>,
    pub mobile_number: Option<String>,
    pub gateway: Option<String>,
    pub amount: String,
    pub currency: String,
    pub transaction_id: Option<String>,
    pub sender: Option<String>,
    pub metadata: serde_json::Value,
    /// Public payment status: `pending` until settled, then `completed`.
    pub status: String,
    pub date: String,
}

pub async fn verify(db: &Db, pay_ref: &str) -> Result<VerifyResult, CheckoutError> {
    let row: Option<CheckoutRow> = sqlx::query_as("SELECT * FROM charges WHERE pay_ref = ?")
        .bind(pay_ref)
        .fetch_optional(db)
        .await?;
    let c = row.ok_or(CheckoutError::NotFound)?;

    // Prefer the verified TrxID from the settling SMS; fall back to a manual claim.
    let settled_trx: Option<String> = sqlx::query_scalar(
        "SELECT txn_id FROM sms_events WHERE charge_id = ? AND matched = 1 ORDER BY received_at DESC LIMIT 1",
    )
    .bind(&c.id)
    .fetch_optional(db)
    .await?;

    let status = match c.status.as_str() {
        "paid" => "completed",
        other => other,
    }
    .to_string();

    Ok(VerifyResult {
        sap_id: c.pay_ref.unwrap_or_default(),
        full_name: c.customer_name,
        email_address: c.customer_email,
        mobile_number: c.customer_msisdn,
        gateway: c.gateway,
        amount: format!("{:.2}", c.amount_minor as f64 / 100.0),
        currency: c.currency,
        transaction_id: settled_trx.or(c.claimed_trx_id),
        sender: c.claimed_sender,
        metadata: c
            .metadata
            .and_then(|m| serde_json::from_str(&m).ok())
            .unwrap_or(serde_json::Value::Null),
        status,
        date: c.created_at,
    })
}

/// Row mirror for the verify read — only the columns we surface.
#[derive(sqlx::FromRow)]
struct CheckoutRow {
    id: String,
    pay_ref: Option<String>,
    amount_minor: i64,
    currency: String,
    customer_name: Option<String>,
    customer_email: Option<String>,
    customer_msisdn: Option<String>,
    gateway: Option<String>,
    metadata: Option<String>,
    claimed_trx_id: Option<String>,
    claimed_sender: Option<String>,
    status: String,
    created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{charge, sms};
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

    fn req(amount: f64) -> CreateCheckout {
        CreateCheckout {
            full_name: Some("Rahim".into()),
            email_address: Some("r@example.com".into()),
            mobile_number: Some("01710000000".into()),
            amount,
            currency: "BDT".into(),
            return_url: Some("https://shop.example/thanks".into()),
            webhook_url: Some("https://shop.example/hook".into()),
            metadata: serde_json::json!({"order": "A-1"}),
        }
    }

    #[tokio::test]
    async fn create_returns_pay_url_and_public_view_hides_secrets() {
        let db = test_db().await;
        let out = create_checkout(&db, "https://pay.example", req(500.0))
            .await
            .unwrap();
        assert!(out.sap_url.starts_with("https://pay.example/pay/pay_"));
        assert_eq!(out.sap_id, out.sap_url.rsplit('/').next().unwrap());

        let pub_view = get_public_charge(&db, &out.sap_id).await.unwrap();
        assert_eq!(pub_view.amount_minor, 50_000);
        assert_eq!(pub_view.status, "pending");
        // PublicCharge has no webhook_url / metadata fields — secrets can't leak here.
    }

    #[tokio::test]
    async fn manual_claim_does_not_settle() {
        let db = test_db().await;
        let out = create_checkout(&db, "https://pay.example", req(300.0))
            .await
            .unwrap();
        submit_manual(&db, &out.sap_id, "FAKE123", Some("01710000000"))
            .await
            .unwrap();

        // Still pending — a customer can't self-settle.
        let v = verify(&db, &out.sap_id).await.unwrap();
        assert_eq!(v.status, "pending");
        assert_eq!(v.transaction_id.as_deref(), Some("FAKE123"));
    }

    #[tokio::test]
    async fn sms_settlement_flips_status_to_completed() {
        let db = test_db().await;
        let out = create_checkout(&db, "https://pay.example", req(500.0))
            .await
            .unwrap();

        // The forwarder delivers a matching bKash SMS → amount match settles it.
        sms::ingest(
            &db,
            "bkash",
            "You have received Tk 500.00 from 01710000000. TrxID REALTRX9",
        )
        .await
        .unwrap();

        let v = verify(&db, &out.sap_id).await.unwrap();
        assert_eq!(v.status, "completed");
        assert_eq!(v.transaction_id.as_deref(), Some("REALTRX9"));

        // And the underlying charge is paid.
        let internal_id: String = sqlx::query_scalar("SELECT id FROM charges WHERE pay_ref = ?")
            .bind(&out.sap_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(charge::get(&db, &internal_id).await.unwrap().status, "paid");
    }
}
