//! Charge (payment intent) domain.
//!
//! A charge is the merchant-facing unit of intent: "collect X from order Y". Creating
//! one is pure bookkeeping — no money moves until a matching MFS SMS is verified
//! (M2), at which point the charge is marked paid and settled into the ledger.

use crate::db::Db;
use crate::ledger;
use serde::{Deserialize, Serialize};

/// Ledger account that funds land in once a charge is confirmed paid.
const MERCHANT_RECEIVABLE: &str = "merchant:receivable";
/// Contra account representing the customer's obligation.
const CUSTOMER_CLEARING: &str = "customer:clearing";

#[derive(Debug, Deserialize)]
pub struct CreateCharge {
    pub order_id: String,
    pub amount_minor: i64,
    #[serde(default = "default_currency")]
    pub currency: String,
    pub customer_name: Option<String>,
    pub customer_msisdn: Option<String>,
    pub callback_url: Option<String>,
}

fn default_currency() -> String {
    "BDT".into()
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Charge {
    pub id: String,
    pub order_id: String,
    pub amount_minor: i64,
    pub currency: String,
    pub customer_name: Option<String>,
    pub customer_msisdn: Option<String>,
    pub callback_url: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ChargeError {
    #[error("amount must be positive")]
    InvalidAmount,
    #[error("a charge already exists for order_id {0}")]
    DuplicateOrder(String),
    #[error("charge not found")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub async fn create(db: &Db, req: CreateCharge) -> Result<Charge, ChargeError> {
    if req.amount_minor <= 0 {
        return Err(ChargeError::InvalidAmount);
    }

    let id = format!("chg_{}", uuid::Uuid::new_v4().simple());

    let result = sqlx::query(
        "INSERT INTO charges \
         (id, order_id, amount_minor, currency, customer_name, customer_msisdn, callback_url) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.order_id)
    .bind(req.amount_minor)
    .bind(&req.currency)
    .bind(&req.customer_name)
    .bind(&req.customer_msisdn)
    .bind(&req.callback_url)
    .execute(db)
    .await;

    if let Err(sqlx::Error::Database(ref e)) = result {
        if e.is_unique_violation() {
            return Err(ChargeError::DuplicateOrder(req.order_id));
        }
    }
    result?;

    get(db, &id).await
}

pub async fn get(db: &Db, id: &str) -> Result<Charge, ChargeError> {
    sqlx::query_as::<_, Charge>("SELECT * FROM charges WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(ChargeError::NotFound)
}

/// Settle a confirmed charge: flip status to paid and post the balanced ledger
/// transaction. Called from the SMS verification path in M2; exposed here so the
/// ledger wiring lives with the charge it settles.
pub async fn mark_paid(db: &Db, id: &str) -> Result<(), ChargeError> {
    let charge = get(db, id).await?;

    ledger::post(
        db,
        &format!("ltx_{}", uuid::Uuid::new_v4().simple()),
        &charge.id,
        &format!("charge {} settled for order {}", charge.id, charge.order_id),
        &[
            ledger::Entry::debit(MERCHANT_RECEIVABLE, charge.amount_minor, &charge.currency),
            ledger::Entry::credit(CUSTOMER_CLEARING, charge.amount_minor, &charge.currency),
        ],
    )
    .await
    .map_err(|e| match e {
        ledger::LedgerError::Db(db) => ChargeError::Db(db),
        // A balance error here is a programming fault, not a client error.
        other => ChargeError::Db(sqlx::Error::Protocol(other.to_string())),
    })?;

    sqlx::query("UPDATE charges SET status = 'paid', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;

    // Notify the merchant, if they registered a callback. Delivery is durable and
    // retried by the background worker — this only enqueues.
    if let Some(url) = charge.callback_url.as_deref() {
        let payload = serde_json::json!({
            "event": "charge.paid",
            "charge_id": charge.id,
            "order_id": charge.order_id,
            "amount_minor": charge.amount_minor,
            "currency": charge.currency,
            "status": "paid",
        })
        .to_string();
        crate::webhook::enqueue(db, &charge.id, url, "charge.paid", &payload).await?;
    }

    Ok(())
}
