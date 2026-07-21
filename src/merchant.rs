//! Merchant API keys — the drop-in authentication layer.
//!
//! A merchant site authenticates checkout calls with an API key (scoped:
//! `create_payment`, `verify_payment`). Keys are stored hashed with the same keyed
//! digest as device tokens; the raw `spk_…` key is returned exactly once at
//! creation and never persisted in the clear. Author: Mohammad Sijan (SpritexAI).

use crate::crypto;
use crate::db::Db;
use serde::{Deserialize, Serialize};

/// Namespace byte-string for the API-key digest — distinct from the device-token
/// domain so the two hash spaces can never collide.
const KEY_DOMAIN: &[u8] = b"api-key";

#[derive(Debug, Deserialize)]
pub struct CreateApiKey {
    pub label: Option<String>,
    /// Defaults to both scopes when omitted — a full-access merchant key.
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct NewApiKey {
    pub id: String,
    pub label: Option<String>,
    pub scopes: Vec<String>,
    /// The raw key, shown once. Never stored — only its digest lives in the DB.
    pub api_key: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ApiKeyRow {
    pub id: String,
    pub label: Option<String>,
    pub scopes: String,
    pub status: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

/// The verified key context handed to a request handler once auth passes.
pub struct KeyContext {
    pub id: String,
    pub scopes: Vec<String>,
}

fn parse_scopes(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

pub async fn create_api_key(db: &Db, req: CreateApiKey) -> Result<NewApiKey, sqlx::Error> {
    let id = format!("ak_{}", uuid::Uuid::new_v4().simple());
    // 256 bits from two v4 UUIDs — no extra RNG dependency, same trick as device tokens.
    let raw = format!(
        "spk_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let key_sha256 = crypto::sign(KEY_DOMAIN, raw.as_bytes());
    let scopes = req
        .scopes
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| vec!["create_payment".into(), "verify_payment".into()]);
    let scopes_json = serde_json::to_string(&scopes).unwrap_or_else(|_| "[]".into());

    sqlx::query("INSERT INTO api_keys (id, key_sha256, label, scopes) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(&key_sha256)
        .bind(&req.label)
        .bind(&scopes_json)
        .execute(db)
        .await?;

    Ok(NewApiKey {
        id,
        label: req.label,
        scopes,
        api_key: raw,
    })
}

/// Verify a raw key holds [`required_scope`]. Returns the key context and stamps
/// `last_used_at`; None on unknown/revoked key or a missing scope.
pub async fn verify_api_key(
    db: &Db,
    raw: &str,
    required_scope: &str,
) -> Result<Option<KeyContext>, sqlx::Error> {
    let key_sha256 = crypto::sign(KEY_DOMAIN, raw.as_bytes());
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, scopes FROM api_keys WHERE key_sha256 = ? AND status = 'active'",
    )
    .bind(&key_sha256)
    .fetch_optional(db)
    .await?;

    let Some((id, scopes_json)) = row else {
        return Ok(None);
    };
    let scopes = parse_scopes(&scopes_json);
    if !scopes.iter().any(|s| s == required_scope) {
        return Ok(None);
    }

    sqlx::query(
        "UPDATE api_keys SET last_used_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = ?",
    )
    .bind(&id)
    .execute(db)
    .await?;

    Ok(Some(KeyContext { id, scopes }))
}

pub async fn list_api_keys(db: &Db) -> Result<Vec<ApiKeyRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, label, scopes, status, created_at, last_used_at \
         FROM api_keys ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await
}

pub async fn revoke_api_key(db: &Db, id: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("UPDATE api_keys SET status = 'revoked' WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(done.rows_affected() > 0)
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

    #[tokio::test]
    async fn create_then_verify_with_scope() {
        let db = test_db().await;
        let key = create_api_key(
            &db,
            CreateApiKey {
                label: Some("site".into()),
                scopes: None,
            },
        )
        .await
        .unwrap();
        assert!(key.api_key.starts_with("spk_"));

        // Correct scope resolves.
        assert!(verify_api_key(&db, &key.api_key, "create_payment")
            .await
            .unwrap()
            .is_some());
        // Unknown key does not.
        assert!(verify_api_key(&db, "spk_bogus", "create_payment")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn scope_and_revoke_are_enforced() {
        let db = test_db().await;
        let key = create_api_key(
            &db,
            CreateApiKey {
                label: None,
                scopes: Some(vec!["create_payment".into()]),
            },
        )
        .await
        .unwrap();

        // Missing scope → rejected.
        assert!(verify_api_key(&db, &key.api_key, "verify_payment")
            .await
            .unwrap()
            .is_none());

        // Revoked → rejected even with the right scope.
        revoke_api_key(&db, &key.id).await.unwrap();
        assert!(verify_api_key(&db, &key.api_key, "create_payment")
            .await
            .unwrap()
            .is_none());
    }
}
