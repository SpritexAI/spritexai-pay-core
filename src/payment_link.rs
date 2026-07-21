//! Payment links — one reusable link, many checkouts.
//!
//! A merchant creates a link once and shares `/link/:ref`. Every open mints a fresh
//! hosted checkout via [`crate::checkout::create_checkout`], so links reuse the whole
//! settlement path with no new plumbing. A link either pins an amount (fixed price) or
//! leaves it open for the customer to enter; an optional `quantity` caps how many times
//! it can be opened. Author: Mohammad Sijan (SpritexAI).

use crate::checkout::{self, CheckoutCreated, CreateCheckout};
use crate::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreatePaymentLink {
    pub product_name: String,
    /// `None` = open amount: the customer enters it when the link is opened.
    pub amount_minor: Option<i64>,
    pub currency: Option<String>,
    /// `None` = unlimited uses.
    pub quantity: Option<i64>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PaymentLink {
    pub id: String,
    /// Public opaque slug in the shared URL. `ref` is a Rust keyword, so the field is
    /// `ref_` and renamed on both the DB column and the JSON key.
    #[sqlx(rename = "ref")]
    #[serde(rename = "ref")]
    pub ref_: String,
    pub product_name: String,
    pub amount_minor: Option<i64>,
    pub currency: String,
    pub quantity: Option<i64>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("payment link not found")]
    NotFound,
    #[error("payment link is inactive")]
    Inactive,
    #[error("amount is required for an open-amount link")]
    AmountRequired,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Create a reusable link and return the stored row.
pub async fn create(db: &Db, req: CreatePaymentLink) -> Result<PaymentLink, LinkError> {
    let id = format!("lnk_{}", uuid::Uuid::new_v4().simple());
    // Short opaque slug — first 12 hex chars of a v4 simple form is plenty of entropy
    // for a shareable link and keeps the URL tidy.
    let ref_: String = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let currency = req
        .currency
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "BDT".into());

    sqlx::query(
        "INSERT INTO payment_links (id, ref, product_name, amount_minor, currency, quantity) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&ref_)
    .bind(&req.product_name)
    .bind(req.amount_minor)
    .bind(&currency)
    .bind(req.quantity)
    .execute(db)
    .await?;

    load(db, &ref_).await?.ok_or(LinkError::NotFound)
}

pub async fn list(db: &Db, limit: i64) -> Result<Vec<PaymentLink>, LinkError> {
    let rows = sqlx::query_as(
        "SELECT id, ref, product_name, amount_minor, currency, quantity, status, created_at \
         FROM payment_links ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Open a link: resolve the amount, decrement any quantity, and mint a checkout.
/// `amount_override` supplies the amount for an open-amount link (ignored when the link
/// pins its own amount).
pub async fn open(
    db: &Db,
    base_url: &str,
    ref_: &str,
    amount_override: Option<i64>,
) -> Result<CheckoutCreated, LinkError> {
    let link = load(db, ref_).await?.ok_or(LinkError::NotFound)?;
    if link.status != "active" {
        return Err(LinkError::Inactive);
    }

    let amount_minor = match link.amount_minor {
        Some(a) => a,
        None => amount_override.ok_or(LinkError::AmountRequired)?,
    };

    // Consume one use when the link is quantity-limited. NULL quantity = unlimited.
    if let Some(q) = link.quantity {
        if q > 0 {
            sqlx::query("UPDATE payment_links SET quantity = quantity - 1 WHERE ref = ?")
                .bind(ref_)
                .execute(db)
                .await?;
        }
    }

    let req = CreateCheckout {
        full_name: None,
        email_address: None,
        mobile_number: None,
        // checkout.rs takes major-unit f64 and re-derives minor internally.
        amount: amount_minor as f64 / 100.0,
        currency: link.currency,
        return_url: None,
        webhook_url: None,
        metadata: serde_json::Value::Null,
    };

    // The only checkout error reachable here is a non-positive amount; fold everything
    // into AmountRequired so an invalid link amount surfaces as "give me a good amount".
    checkout::create_checkout(db, base_url, req)
        .await
        .map_err(|_| LinkError::AmountRequired)
}

/// Load one link by its public ref.
async fn load(db: &Db, ref_: &str) -> Result<Option<PaymentLink>, LinkError> {
    let row = sqlx::query_as(
        "SELECT id, ref, product_name, amount_minor, currency, quantity, status, created_at \
         FROM payment_links WHERE ref = ?",
    )
    .bind(ref_)
    .fetch_optional(db)
    .await?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn fixed() -> CreatePaymentLink {
        CreatePaymentLink {
            product_name: "T-shirt".into(),
            amount_minor: Some(50_000),
            currency: None,
            quantity: None,
        }
    }

    #[tokio::test]
    async fn create_then_list_returns_it() {
        let db = test_db().await;
        let link = create(&db, fixed()).await.unwrap();
        assert!(link.id.starts_with("lnk_"));
        assert_eq!(link.amount_minor, Some(50_000));

        let all = list(&db, 10).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].ref_, link.ref_);
    }

    #[tokio::test]
    async fn open_on_inactive_link_is_rejected() {
        let db = test_db().await;
        let link = create(&db, fixed()).await.unwrap();
        sqlx::query("UPDATE payment_links SET status = 'inactive' WHERE ref = ?")
            .bind(&link.ref_)
            .execute(&db)
            .await
            .unwrap();

        let err = open(&db, "https://pay.example", &link.ref_, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LinkError::Inactive));
    }

    #[tokio::test]
    async fn open_amount_link_without_override_requires_amount() {
        let db = test_db().await;
        let link = create(
            &db,
            CreatePaymentLink {
                product_name: "Donation".into(),
                amount_minor: None,
                currency: None,
                quantity: None,
            },
        )
        .await
        .unwrap();

        let err = open(&db, "https://pay.example", &link.ref_, None)
            .await
            .unwrap_err();
        assert!(matches!(err, LinkError::AmountRequired));
    }
}
