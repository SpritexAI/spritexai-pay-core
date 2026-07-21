//! Audit-log reads — the admin activity feed.
//!
//! Every ledger-affecting mutation is meant to drop a row into `audit_log`; this
//! module is the thin read side that surfaces those rows to the dashboard, plus a
//! [`record`] helper so callers append an entry without hand-writing SQL. The table
//! is the audit spine from the baseline migration — we never mint schema here.
//! Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::Serialize;

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Activity {
    pub id: i64,
    pub entity: String,
    pub entity_id: String,
    pub action: String,
    pub actor: String,
    pub detail: Option<String>,
    pub created_at: String,
}

/// Newest audit rows first, capped at `limit` — the dashboard's activity feed.
pub async fn list(db: &Db, limit: i64) -> Result<Vec<Activity>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, entity, entity_id, action, actor, detail, created_at \
         FROM audit_log ORDER BY id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db)
    .await
}

/// Append one audit entry. `actor` defaults to "admin" — the only writer today is
/// the dashboard. `id` is INTEGER AUTOINCREMENT, so we let SQLite mint it.
pub async fn record(
    db: &Db,
    entity: &str,
    entity_id: &str,
    action: &str,
    actor: Option<&str>,
    detail: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_log (entity, entity_id, action, actor, detail) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(entity)
    .bind(entity_id)
    .bind(action)
    .bind(actor.unwrap_or("admin"))
    .bind(detail)
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
    async fn record_then_list_returns_it() {
        let db = test_db().await;
        record(
            &db,
            "charge",
            "chg_123",
            "settled",
            None,
            Some("matched via SMS"),
        )
        .await
        .unwrap();

        let rows = list(&db, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.entity, "charge");
        assert_eq!(row.entity_id, "chg_123");
        assert_eq!(row.action, "settled");
        assert_eq!(row.actor, "admin"); // defaulted
        assert_eq!(row.detail.as_deref(), Some("matched via SMS"));
    }

    #[tokio::test]
    async fn list_is_newest_first_and_capped() {
        let db = test_db().await;
        for i in 0..3 {
            record(
                &db,
                "charge",
                &format!("chg_{i}"),
                "created",
                Some("system"),
                None,
            )
            .await
            .unwrap();
        }
        let rows = list(&db, 2).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Highest id (last inserted) leads.
        assert!(rows[0].id > rows[1].id);
        assert_eq!(rows[0].actor, "system"); // explicit actor is honored over the default
    }
}
