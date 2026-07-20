//! M4 surface: reconciliation summary + device pairing token hygiene.

use spritexai_pay::{ai, charge, device, reconcile, sms};
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

#[tokio::test]
async fn reconciliation_sums_only_settled_inflow() {
    let db = test_db().await;

    // Two charges; only one gets a matching SMS.
    for (order, amt) in [("R-1", 30_000i64), ("R-2", 70_000)] {
        charge::create(
            &db,
            charge::CreateCharge {
                order_id: order.into(),
                amount_minor: amt,
                currency: "BDT".into(),
                customer_name: None,
                customer_msisdn: None,
                callback_url: None,
            },
        )
        .await
        .unwrap();
    }

    sms::ingest(
        &db,
        "bkash",
        "received Tk 300.00 from 01710000000. TrxID RC1",
    )
    .await
    .unwrap();

    let all = reconcile::reconcile(&db, None).await.unwrap();
    assert_eq!(all.total_settled_minor, 30_000);
    assert_eq!(all.settled_count, 1);
    assert_eq!(all.pending_charges, 1); // R-2 still pending

    let bkash = reconcile::reconcile(&db, Some("nagad")).await.unwrap();
    assert_eq!(bkash.total_settled_minor, 0, "no nagad inflow");
}

#[tokio::test]
async fn pairing_returns_token_once_and_stores_only_hash() {
    let db = test_db().await;

    let paired = device::pair_device(
        &db,
        device::PairDevice {
            label: Some("Pixel".into()),
        },
    )
    .await
    .unwrap();
    assert!(paired.pairing_token.starts_with("spx_"));

    // The raw token must never be findable in the devices table.
    let leaked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE token_sha256 = ?")
        .bind(&paired.pairing_token)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(leaked, 0, "plaintext token must not be stored");

    let listed = device::list_devices(&db).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, "active");
}

#[tokio::test]
async fn regex_suggestion_none_without_drift_data() {
    // No AI keys + no recovered samples → nothing to suggest, never panics.
    // Guards the query/no-op path so it survives without hitting the network.
    let db = test_db().await;
    assert!(ai::suggest_regex(&db, "bkash").await.is_none());
}
