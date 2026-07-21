//! Invoices — itemized bills that collect through the hosted checkout.
//!
//! A merchant drafts an invoice with line items; we sum them into a single amount
//! and, on demand, open a hosted checkout to collect it (reusing the checkout flow,
//! so settlement stays SMS-driven with no new plumbing). This mirrors the common PHP
//! SMS-gateway integration shape, where invoicing sits on top of the pay endpoint.
//! Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct LineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_minor: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateInvoice {
    pub number: Option<String>,
    pub customer_id: Option<String>,
    pub currency: Option<String>,
    pub items: Vec<LineItem>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Invoice {
    pub id: String,
    pub number: String,
    pub customer_id: Option<String>,
    pub amount_minor: i64,
    pub currency: String,
    pub status: String,
    pub charge_id: Option<String>,
    pub pay_ref: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InvoiceError {
    #[error("invoice amount must be positive")]
    InvalidAmount,
    #[error("invoice not found")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A checkout error only surfaces here via `issue_payment`. InvalidAmount is
/// impossible there (the invoice already passed the > 0 check), so both real
/// variants fold into the appropriate invoice error.
impl From<crate::checkout::CheckoutError> for InvoiceError {
    fn from(e: crate::checkout::CheckoutError) -> Self {
        use crate::checkout::CheckoutError as C;
        match e {
            C::NotFound => InvoiceError::NotFound,
            C::InvalidAmount => InvoiceError::InvalidAmount,
            C::DomainNotAllowed => InvoiceError::InvalidAmount,
            C::Db(e) => InvoiceError::Db(e),
        }
    }
}

/// Draft an invoice from its line items. `amount_minor` is the summed total;
/// zero or empty items are rejected. `number` defaults to `INV-<8 hex>`.
pub async fn create(db: &Db, req: CreateInvoice) -> Result<Invoice, InvoiceError> {
    let amount_minor: i64 = req.items.iter().map(|i| i.quantity * i.unit_minor).sum();
    if amount_minor <= 0 {
        return Err(InvoiceError::InvalidAmount);
    }

    let id = format!("inv_{}", uuid::Uuid::new_v4().simple());
    let number = req.number.unwrap_or_else(|| format!("INV-{}", &id[4..12]));
    let currency = req.currency.unwrap_or_else(|| "BDT".into());

    sqlx::query(
        "INSERT INTO invoices (id, number, customer_id, amount_minor, currency) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&number)
    .bind(&req.customer_id)
    .bind(amount_minor)
    .bind(&currency)
    .execute(db)
    .await?;

    for item in &req.items {
        let item_id = format!("itm_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO invoice_items (id, invoice_id, description, quantity, unit_minor) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&item_id)
        .bind(&id)
        .bind(&item.description)
        .bind(item.quantity)
        .bind(item.unit_minor)
        .execute(db)
        .await?;
    }

    get(db, &id).await?.ok_or(InvoiceError::NotFound)
}

pub async fn list(db: &Db, limit: i64) -> Result<Vec<Invoice>, InvoiceError> {
    let rows = sqlx::query_as("SELECT * FROM invoices ORDER BY created_at DESC LIMIT ?")
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows)
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Invoice>, InvoiceError> {
    let row = sqlx::query_as("SELECT * FROM invoices WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(row)
}

/// Open a hosted checkout to collect this invoice and record the linkage.
///
/// `create_checkout` returns `sap_id`, which is the public `pay_ref` used in
/// `/pay/:ref` (checkout.rs: `sap_id: pay_ref.clone()`). Settlement, however,
/// matches on the internal `charges.id`, so we look that up by `pay_ref` and
/// store it on `charge_id` — that is the column `settle_for_charge` matches.
pub async fn issue_payment(
    db: &Db,
    base_url: &str,
    id: &str,
) -> Result<crate::checkout::CheckoutCreated, InvoiceError> {
    let invoice = get(db, id).await?.ok_or(InvoiceError::NotFound)?;

    let req = crate::checkout::CreateCheckout {
        full_name: None,
        email_address: None,
        mobile_number: None,
        amount: invoice.amount_minor as f64 / 100.0,
        currency: invoice.currency.clone(),
        return_url: None,
        webhook_url: None,
        metadata: serde_json::Value::Null,
    };
    let created = crate::checkout::create_checkout(db, base_url, req).await?;

    // sap_id IS the pay_ref; resolve the internal charge id that settlement uses.
    let charge_id: Option<String> = sqlx::query_scalar("SELECT id FROM charges WHERE pay_ref = ?")
        .bind(&created.sap_id)
        .fetch_optional(db)
        .await?;

    sqlx::query(
        "UPDATE invoices SET pay_ref = ?, charge_id = ?, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
    )
    .bind(&created.sap_id)
    .bind(&charge_id)
    .bind(id)
    .execute(db)
    .await?;

    Ok(created)
}

/// Mark any unpaid invoice bound to this charge as paid. No-op when none match.
/// The orchestrator calls this from `charge::mark_paid`.
pub async fn settle_for_charge(db: &Db, charge_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE invoices SET status = 'paid', \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
         WHERE charge_id = ? AND status = 'unpaid'",
    )
    .bind(charge_id)
    .execute(db)
    .await?;
    Ok(())
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

    fn item(desc: &str, qty: i64, unit: i64) -> LineItem {
        LineItem {
            description: desc.into(),
            quantity: qty,
            unit_minor: unit,
        }
    }

    #[tokio::test]
    async fn create_sums_items_and_starts_unpaid() {
        let db = test_db().await;
        let inv = create(
            &db,
            CreateInvoice {
                number: None,
                customer_id: Some("cus_1".into()),
                currency: None,
                items: vec![item("Design", 2, 15_000), item("Hosting", 1, 5_000)],
            },
        )
        .await
        .unwrap();

        assert_eq!(inv.amount_minor, 35_000);
        assert_eq!(inv.status, "unpaid");
        assert!(inv.number.starts_with("INV-"));
    }

    #[tokio::test]
    async fn empty_or_zero_items_are_rejected() {
        let db = test_db().await;
        let empty = create(
            &db,
            CreateInvoice {
                number: None,
                customer_id: None,
                currency: None,
                items: vec![],
            },
        )
        .await;
        assert!(matches!(empty, Err(InvoiceError::InvalidAmount)));

        let zero = create(
            &db,
            CreateInvoice {
                number: None,
                customer_id: None,
                currency: None,
                items: vec![item("Freebie", 3, 0)],
            },
        )
        .await;
        assert!(matches!(zero, Err(InvoiceError::InvalidAmount)));
    }

    #[tokio::test]
    async fn settle_flips_matching_unpaid_invoice() {
        let db = test_db().await;
        let inv = create(
            &db,
            CreateInvoice {
                number: None,
                customer_id: None,
                currency: None,
                items: vec![item("Retainer", 1, 50_000)],
            },
        )
        .await
        .unwrap();

        // Bind a charge id the way issue_payment would, then settle on it.
        sqlx::query("UPDATE invoices SET charge_id = 'chg_test' WHERE id = ?")
            .bind(&inv.id)
            .execute(&db)
            .await
            .unwrap();

        settle_for_charge(&db, "chg_test").await.unwrap();

        let after = get(&db, &inv.id).await.unwrap().unwrap();
        assert_eq!(after.status, "paid");
    }
}
