//! Dashboard settings — the flat key-value config store.
//!
//! Every dashboard settings page (General, Brand, Currency, Themes) reads and
//! writes a slice of one table. Keys are namespaced by group so a page loads its
//! slice with a single `LIKE 'group.%'` scan. Single-tenant by design: "brand" is
//! this instance, its settings kept under `brand.*` with no brands table. Values
//! are opaque strings — the caller owns their meaning. Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::Serialize;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Setting {
    pub key: String,
    pub value: Option<String>,
}

/// Every setting in a group, e.g. `get_group(db, "brand")` → the `brand.*` rows.
pub async fn get_group(db: &Db, group: &str) -> Result<Vec<Setting>, sqlx::Error> {
    sqlx::query_as("SELECT key, value FROM settings WHERE key LIKE ? ORDER BY key")
        .bind(format!("{group}.%"))
        .fetch_all(db)
        .await
}

/// A single setting's value, or None if the key was never set.
pub async fn get(db: &Db, key: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .map(Option::flatten)
}

/// Insert or overwrite a setting, stamping `updated_at` on every write.
pub async fn set(db: &Db, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .bind(key)
    .bind(value)
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

    #[tokio::test]
    async fn set_then_get_roundtrips() {
        let db = test_db().await;
        set(&db, "brand.name", "RexiO").await.unwrap();
        assert_eq!(get(&db, "brand.name").await.unwrap(), Some("RexiO".into()));
        // Unknown key stays None.
        assert_eq!(get(&db, "brand.missing").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_upserts_not_duplicates() {
        let db = test_db().await;
        set(&db, "brand.name", "RexiO").await.unwrap();
        set(&db, "brand.name", "SpritexAI").await.unwrap();
        assert_eq!(
            get(&db, "brand.name").await.unwrap(),
            Some("SpritexAI".into())
        );

        // One row, not two — the conflict updated in place.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM settings WHERE key = 'brand.name'")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn get_group_scopes_to_prefix() {
        let db = test_db().await;
        set(&db, "brand.name", "RexiO").await.unwrap();
        set(&db, "brand.color_primary", "#0a0a0a").await.unwrap();
        set(&db, "currency.default", "BDT").await.unwrap();

        let brand = get_group(&db, "brand").await.unwrap();
        assert_eq!(brand.len(), 2);
        // ORDER BY key → color_primary before name.
        assert_eq!(brand[0].key, "brand.color_primary");
        assert_eq!(brand[1].key, "brand.name");
    }
}
