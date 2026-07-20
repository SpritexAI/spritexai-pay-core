//! SMS parsing + ingestion invariants.
//!
//! The critical money property under test: a replayed confirmation SMS settles a
//! charge exactly once. The `(gateway, txn_id)` idempotency guard must hold even
//! when the Android forwarder delivers the same message twice.

use spritexai_pay::gateway::{self, Gateway};
use spritexai_pay::{charge, ledger, sms};
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

#[test]
fn bkash_parses_txid_amount_sender() {
    let sms = "You have received Tk 500.00 from 01710000000. TrxID BGL7AH92KX at 12:04pm. \
               Balance Tk 1,250.00";
    let p = gateway::Bkash.parse_sms(sms).expect("bkash parse");
    assert_eq!(p.txn_id, "BGL7AH92KX");
    assert_eq!(p.amount_minor, 50_000);
    assert_eq!(p.sender_msisdn.as_deref(), Some("01710000000"));
}

#[test]
fn nagad_parses_txid_amount_sender() {
    let sms = "Money Received. Amount: Tk 320.50 Sender: 01820000000 TxnID: NGD8842PLQ";
    let p = gateway::Nagad.parse_sms(sms).expect("nagad parse");
    assert_eq!(p.txn_id, "NGD8842PLQ");
    assert_eq!(p.amount_minor, 32_050);
    assert_eq!(p.sender_msisdn.as_deref(), Some("01820000000"));
}

#[test]
fn garbage_sms_does_not_match() {
    assert!(gateway::Bkash
        .parse_sms("hello world, no txn here")
        .is_err());
}

#[tokio::test]
async fn unparseable_sms_errors_when_no_ai_providers() {
    // Without OPENROUTER_API_KEY / OPENCODE_ZEN_API_KEY the AI fallback is skipped
    // and the regex error surfaces — Phase 1 behavior, no network involved.
    let db = test_db().await;
    let result = sms::ingest(&db, "bkash", "totally drifted format with no fields").await;
    assert!(matches!(result, Err(sms::IngestError::Parse(_))));

    // Nothing was stored.
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sms_events")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn replayed_sms_settles_charge_exactly_once() {
    let db = test_db().await;

    // A merchant expects 500.00 BDT.
    let chg = charge::create(
        &db,
        charge::CreateCharge {
            order_id: "ORD-SMS-1".into(),
            amount_minor: 50_000,
            currency: "BDT".into(),
            customer_name: None,
            customer_msisdn: None,
            callback_url: None,
        },
    )
    .await
    .unwrap();

    let raw = "You have received Tk 500.00 from 01710000000. TrxID BGL7AH92KX";

    // First delivery: matches and settles.
    let first = sms::ingest(&db, "bkash", raw).await.expect("first ingest");
    assert_eq!(first.matched_charge.as_deref(), Some(chg.id.as_str()));
    assert_eq!(charge::get(&db, &chg.id).await.unwrap().status, "paid");

    // Replay: same TrxID → rejected as duplicate, no second settlement.
    let replay = sms::ingest(&db, "bkash", raw).await;
    assert!(matches!(replay, Err(sms::IngestError::Duplicate)));

    // Ledger reflects exactly one settlement (500.00, not 1000.00).
    let receivable = ledger::account_balance(&db, "merchant:receivable")
        .await
        .unwrap();
    assert_eq!(receivable, 50_000, "charge must settle exactly once");
}

#[tokio::test]
async fn settlement_enqueues_webhook_when_callback_set() {
    let db = test_db().await;
    charge::create(
        &db,
        charge::CreateCharge {
            order_id: "ORD-CB".into(),
            amount_minor: 25_000,
            currency: "BDT".into(),
            customer_name: None,
            customer_msisdn: None,
            callback_url: Some("https://merchant.example/hook".into()),
        },
    )
    .await
    .unwrap();

    sms::ingest(
        &db,
        "bkash",
        "received Tk 250.00 from 01710000000. TrxID CBHK1",
    )
    .await
    .unwrap();

    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_deliveries WHERE status = 'pending' AND event = 'charge.paid'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        queued, 1,
        "a pending merchant webhook must be enqueued on settlement"
    );
}
