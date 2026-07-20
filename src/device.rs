//! Gateway configuration and device pairing.
//!
//! A device (Android SMS forwarder) pairs once: the server mints a random token,
//! returns it exactly once (rendered as a QR on the client), and stores only its
//! SHA-256. Thereafter the device authenticates with the raw token, which is hashed
//! and looked up. Tokens are scoped per device and revocable.

use crate::crypto;
use crate::db::Db;
use serde::{Deserialize, Serialize};

// ---- gateways ----

#[derive(Debug, Deserialize)]
pub struct RegisterGateway {
    pub gateway: String,
    pub label: Option<String>,
    pub account_msisdn: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GatewayConfig {
    pub id: String,
    pub gateway: String,
    pub label: Option<String>,
    pub account_msisdn: Option<String>,
    pub enabled: i64,
    pub created_at: String,
}

pub async fn register_gateway(db: &Db, req: RegisterGateway) -> Result<GatewayConfig, sqlx::Error> {
    let id = format!("gw_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO gateway_configs (id, gateway, label, account_msisdn) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&req.gateway)
    .bind(&req.label)
    .bind(&req.account_msisdn)
    .execute(db)
    .await?;

    sqlx::query_as("SELECT * FROM gateway_configs WHERE id = ?")
        .bind(&id)
        .fetch_one(db)
        .await
}

pub async fn list_gateways(db: &Db) -> Result<Vec<GatewayConfig>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM gateway_configs ORDER BY created_at DESC")
        .fetch_all(db)
        .await
}

// ---- devices ----

#[derive(Debug, Deserialize)]
pub struct PairDevice {
    pub label: Option<String>,
}

/// Returned exactly once at pairing time — the raw token is never persisted.
#[derive(Debug, Serialize)]
pub struct PairedDevice {
    pub id: String,
    pub label: Option<String>,
    pub pairing_token: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Device {
    pub id: String,
    pub label: Option<String>,
    pub status: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}

pub async fn pair_device(db: &Db, req: PairDevice) -> Result<PairedDevice, sqlx::Error> {
    let id = format!("dev_{}", uuid::Uuid::new_v4().simple());
    // 256-bit token from two v4 UUIDs — no extra RNG dependency needed.
    let token = format!(
        "spx_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let token_sha256 = crypto::sign(b"device-token", token.as_bytes());

    sqlx::query("INSERT INTO devices (id, label, token_sha256) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(&req.label)
        .bind(&token_sha256)
        .execute(db)
        .await?;

    Ok(PairedDevice {
        id,
        label: req.label,
        pairing_token: token,
    })
}

pub async fn list_devices(db: &Db) -> Result<Vec<Device>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, label, status, last_seen_at, created_at FROM devices ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await
}

/// Exchange a pairing token for the shared HMAC secret. The Android forwarder
/// pairs once (QR carries only `{url, token}`), then calls this to fetch the
/// secret it signs SMS payloads with — the secret never travels in the QR, so a
/// photographed code alone can't leak it. Returns the active device's id and
/// stamps `last_seen_at`; an unknown or revoked token yields None (→ 401).
pub async fn exchange_token(db: &Db, token: &str) -> Result<Option<String>, sqlx::Error> {
    let token_sha256 = crypto::sign(b"device-token", token.as_bytes());
    let device_id: Option<String> =
        sqlx::query_scalar("SELECT id FROM devices WHERE token_sha256 = ? AND status = 'active'")
            .bind(&token_sha256)
            .fetch_optional(db)
            .await?;

    if let Some(ref id) = device_id {
        sqlx::query(
            "UPDATE devices SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
        )
        .bind(id)
        .execute(db)
        .await?;
    }
    Ok(device_id)
}
