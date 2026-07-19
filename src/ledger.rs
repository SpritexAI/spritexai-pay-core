//! Double-entry ledger.
//!
//! Every settlement is written as a `ledger_transaction` plus two or more signed
//! `ledger_entries`. Debits are positive, credits negative; a valid transaction's
//! entries sum to zero. The tables are append-only — corrections are new
//! compensating transactions, never edits.

use crate::db::Db;
use sqlx::Sqlite;

#[derive(Debug, Clone)]
pub struct Entry {
    pub account: String,
    pub amount_minor: i64,
    pub currency: String,
}

impl Entry {
    pub fn debit(account: &str, amount_minor: i64, currency: &str) -> Self {
        Self {
            account: account.into(),
            amount_minor,
            currency: currency.into(),
        }
    }

    pub fn credit(account: &str, amount_minor: i64, currency: &str) -> Self {
        Self {
            account: account.into(),
            amount_minor: -amount_minor,
            currency: currency.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger transaction does not balance: entries sum to {0}, expected 0")]
    Unbalanced(i64),
    #[error("ledger transaction needs at least two entries")]
    TooFewEntries,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Post a balanced transaction. Runs inside a DB transaction so a partial write can
/// never leave the ledger unbalanced.
pub async fn post(
    db: &Db,
    txn_id: &str,
    reference: &str,
    memo: &str,
    entries: &[Entry],
) -> Result<(), LedgerError> {
    if entries.len() < 2 {
        return Err(LedgerError::TooFewEntries);
    }
    let sum: i64 = entries.iter().map(|e| e.amount_minor).sum();
    if sum != 0 {
        return Err(LedgerError::Unbalanced(sum));
    }

    let mut tx = db.begin().await?;

    sqlx::query::<Sqlite>("INSERT INTO ledger_transactions (id, reference, memo) VALUES (?, ?, ?)")
        .bind(txn_id)
        .bind(reference)
        .bind(memo)
        .execute(&mut *tx)
        .await?;

    for e in entries {
        sqlx::query::<Sqlite>(
            "INSERT INTO ledger_entries (txn_id, account, amount_minor, currency) VALUES (?, ?, ?, ?)",
        )
        .bind(txn_id)
        .bind(&e.account)
        .bind(e.amount_minor)
        .bind(&e.currency)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Current balance of an account (sum of signed entries). Cheap integrity probe.
pub async fn account_balance(db: &Db, account: &str) -> Result<i64, sqlx::Error> {
    let bal: Option<i64> = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_minor), 0) FROM ledger_entries WHERE account = ?",
    )
    .bind(account)
    .fetch_one(db)
    .await?;
    Ok(bal.unwrap_or(0))
}
