//! Integration test for the charge → double-entry ledger path.
//!
//! Uses an in-memory SQLite DB with the real migrations, so the schema under test
//! is identical to production. Verifies that settling a charge posts a *balanced*
//! ledger transaction — the core money invariant of SpritEXAI Pay.

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

async fn test_db() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory sqlite");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    pool
}

#[tokio::test]
async fn settling_a_charge_posts_a_balanced_ledger() {
    let db = test_db().await;

    // Create a 500.00 BDT charge (50000 poisha).
    let charge = spritexai_pay::charge::create(
        &db,
        spritexai_pay::charge::CreateCharge {
            order_id: "ORD-1001".into(),
            amount_minor: 50_000,
            currency: "BDT".into(),
            customer_name: Some("Rifat".into()),
            customer_msisdn: Some("01700000000".into()),
            callback_url: None,
        },
    )
    .await
    .expect("charge created");

    assert_eq!(charge.status, "pending");
    assert!(charge.id.starts_with("chg_"));

    // Settle it.
    spritexai_pay::charge::mark_paid(&db, &charge.id)
        .await
        .expect("mark paid");

    let settled = spritexai_pay::charge::get(&db, &charge.id).await.unwrap();
    assert_eq!(settled.status, "paid");

    // The two mirror accounts must net to zero across the whole ledger.
    let receivable = spritexai_pay::ledger::account_balance(&db, "merchant:receivable")
        .await
        .unwrap();
    let clearing = spritexai_pay::ledger::account_balance(&db, "customer:clearing")
        .await
        .unwrap();
    assert_eq!(receivable, 50_000);
    assert_eq!(clearing, -50_000);
    assert_eq!(receivable + clearing, 0, "ledger must balance");
}

#[tokio::test]
async fn duplicate_order_is_rejected() {
    let db = test_db().await;
    let mk = || spritexai_pay::charge::CreateCharge {
        order_id: "ORD-DUP".into(),
        amount_minor: 100,
        currency: "BDT".into(),
        customer_name: None,
        customer_msisdn: None,
        callback_url: None,
    };
    spritexai_pay::charge::create(&db, mk())
        .await
        .expect("first ok");
    let err = spritexai_pay::charge::create(&db, mk()).await.unwrap_err();
    assert!(matches!(
        err,
        spritexai_pay::charge::ChargeError::DuplicateOrder(_)
    ));
}
