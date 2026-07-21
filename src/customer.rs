//! Customers — the instance's contact address-book.
//!
//! A flat list of people this instance has transacted with, held for convenience:
//! charges denormalize the customer fields they need, so nothing links back here.
//! Single-tenant, so no owner column — the whole table belongs to this instance.
//! Author: Mohammad Sijan (SpritexAI).

use crate::db::Db;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateCustomer {
    pub name: Option<String>,
    pub email: Option<String>,
    pub msisdn: Option<String>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct Customer {
    pub id: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub msisdn: Option<String>,
    pub status: String,
    pub created_at: String,
}

pub async fn create(db: &Db, req: CreateCustomer) -> Result<Customer, sqlx::Error> {
    let id = format!("cus_{}", uuid::Uuid::new_v4().simple());

    sqlx::query("INSERT INTO customers (id, name, email, msisdn) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(&req.name)
        .bind(&req.email)
        .bind(&req.msisdn)
        .execute(db)
        .await?;

    // SELECT back so status/created_at come from the DB defaults, not guessed here.
    sqlx::query_as("SELECT id, name, email, msisdn, status, created_at FROM customers WHERE id = ?")
        .bind(&id)
        .fetch_one(db)
        .await
}

pub async fn list(db: &Db, limit: i64) -> Result<Vec<Customer>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, email, msisdn, status, created_at \
         FROM customers ORDER BY created_at DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(db)
    .await
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Customer>, sqlx::Error> {
    sqlx::query_as("SELECT id, name, email, msisdn, status, created_at FROM customers WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
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
    async fn create_then_list_round_trip() {
        let db = test_db().await;
        let made = create(
            &db,
            CreateCustomer {
                name: Some("Rahim".into()),
                email: Some("rahim@example.com".into()),
                msisdn: Some("017xxxxxxxx".into()),
            },
        )
        .await
        .unwrap();
        assert!(made.id.starts_with("cus_"));
        assert_eq!(made.status, "active");

        let all = list(&db, 10).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, made.id);
        assert_eq!(all[0].name.as_deref(), Some("Rahim"));
    }
}
