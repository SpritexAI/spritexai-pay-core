//! Return/webhook URL host whitelist — the open-redirect & SSRF guard.
//!
//! Checkout accepts merchant-supplied `return_url` and `webhook_url`. Left
//! unchecked those are an open-redirect and a server-side-request-forgery
//! vector. This module holds the allow-list of hosts and the [`is_allowed`]
//! predicate the checkout path calls before honouring either URL. An empty
//! table means "allow all", so existing deploys keep working until an operator
//! opts in by adding a domain. Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateDomain {
    pub domain: String,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Domain {
    pub id: String,
    pub domain: String,
    pub created_at: String,
}

pub async fn create(db: &Db, req: CreateDomain) -> Result<Domain, sqlx::Error> {
    let id = format!("dom_{}", uuid::Uuid::new_v4().simple());
    sqlx::query("INSERT INTO allowed_domains (id, domain) VALUES (?, ?)")
        .bind(&id)
        .bind(&req.domain)
        .execute(db)
        .await?;

    sqlx::query_as("SELECT id, domain, created_at FROM allowed_domains WHERE id = ?")
        .bind(&id)
        .fetch_one(db)
        .await
}

pub async fn list(db: &Db) -> Result<Vec<Domain>, sqlx::Error> {
    sqlx::query_as("SELECT id, domain, created_at FROM allowed_domains ORDER BY created_at DESC")
        .fetch_all(db)
        .await
}

pub async fn delete(db: &Db, id: &str) -> Result<bool, sqlx::Error> {
    let done = sqlx::query("DELETE FROM allowed_domains WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Pull the host out of a URL. None input → None; anything unparseable is
/// treated as hostless.
fn host_of(url: &str) -> Option<String> {
    // ponytail: naive host split, swap for url crate if edge cases bite
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Is `url` allowed to be used as a return/webhook target?
///
/// - `None` URL → true (nothing to redirect to).
/// - empty allow-list → true (feature is opt-in, non-breaking).
/// - otherwise → true iff the URL's host exactly matches a whitelisted domain.
pub async fn is_allowed(db: &Db, url: Option<&str>) -> Result<bool, sqlx::Error> {
    let Some(url) = url else {
        return Ok(true);
    };

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM allowed_domains")
        .fetch_one(db)
        .await?;
    if count == 0 {
        return Ok(true);
    }

    let Some(host) = host_of(url) else {
        return Ok(false);
    };

    let hit: Option<String> =
        sqlx::query_scalar("SELECT domain FROM allowed_domains WHERE domain = ?")
            .bind(&host)
            .fetch_optional(db)
            .await?;
    Ok(hit.is_some())
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
    async fn empty_table_allows_everything() {
        let db = test_db().await;
        assert!(is_allowed(&db, Some("https://anything.example/x"))
            .await
            .unwrap());
        assert!(is_allowed(&db, None).await.unwrap());
    }

    #[tokio::test]
    async fn whitelist_matches_host_only() {
        let db = test_db().await;
        create(
            &db,
            CreateDomain {
                domain: "shop.example.com".into(),
            },
        )
        .await
        .unwrap();

        assert!(is_allowed(&db, Some("https://shop.example.com/thanks"))
            .await
            .unwrap());
        assert!(!is_allowed(&db, Some("https://evil.com/x")).await.unwrap());
        // None is always fine, even with a non-empty list.
        assert!(is_allowed(&db, None).await.unwrap());
    }
}
