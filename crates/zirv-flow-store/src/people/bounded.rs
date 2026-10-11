//! Reads that must not run unbounded: the people search and the timeline run in a
//! transaction with a statement timeout, so a slow scan is cancelled rather than left to hog
//! the pool.

use std::time::Duration;

use sqlx::PgConnection;

use crate::{StoreError, Tx, begin};

/// How long a search or timeline read may run before PostgreSQL cancels it.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Sets the statement timeout for the rest of the transaction.
async fn limit_statements(
    connection: &mut PgConnection,
    timeout: Duration,
) -> Result<(), StoreError> {
    // `SET LOCAL` takes no bind parameters; the value is a number of milliseconds.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL statement_timeout = {}",
        timeout.as_millis()
    )))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Starts a read-only transaction bounded by [`QUERY_TIMEOUT`].
pub(crate) async fn begin_bounded(pool: &sqlx::PgPool) -> Result<Tx, StoreError> {
    begin_bounded_for(pool, QUERY_TIMEOUT).await
}

pub(crate) async fn begin_bounded_for(
    pool: &sqlx::PgPool,
    timeout: Duration,
) -> Result<Tx, StoreError> {
    let mut tx = begin(pool).await?;
    limit_statements(&mut tx, timeout).await?;
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_statement_past_the_limit_is_cancelled_as_a_timeout(pool: PgPool) {
        let mut tx = begin_bounded_for(&pool, Duration::from_millis(50))
            .await
            .unwrap();

        let error = sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)
            .unwrap_err();

        assert!(error.is_timeout());
        assert!(!error.is_conflict() && !error.is_retryable());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_limit_ends_with_the_transaction(pool: PgPool) {
        let tx = begin_bounded_for(&pool, Duration::from_millis(50))
            .await
            .unwrap();
        drop(tx);

        sqlx::query("SELECT pg_sleep(0.2)")
            .execute(&pool)
            .await
            .unwrap();
    }
}
