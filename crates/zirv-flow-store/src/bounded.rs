//! Reads that must not run unbounded: the people search, the timeline and the event explorer run in a
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

/// Starts a read-only transaction that PostgreSQL cancels a statement of after `timeout`.
pub(crate) async fn begin_bounded_for(
    pool: &sqlx::PgPool,
    timeout: Duration,
) -> Result<Tx, StoreError> {
    let mut tx = begin(pool).await?;
    limit_statements(&mut tx, timeout).await?;
    Ok(tx)
}

/// Like [`begin_bounded_for`], and every statement of the transaction sees one snapshot
/// (`REPEATABLE READ`, `READ ONLY`). A request that reads twice, such as a breakdown and its
/// totals, then cannot be torn by a concurrent write.
pub(crate) async fn begin_snapshot_for(
    pool: &sqlx::PgPool,
    timeout: Duration,
) -> Result<Tx, StoreError> {
    let mut tx = begin(pool).await?;
    // It must come before the first query of the transaction.
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
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
            .map_err(StoreError::bounded)
            .unwrap_err();

        assert!(error.is_timeout());
        assert!(!error.is_conflict() && !error.is_retryable());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancelled_statement_outside_a_bounded_read_is_not_a_timeout(pool: PgPool) {
        let mut tx = begin_bounded_for(&pool, Duration::from_millis(50))
            .await
            .unwrap();

        let error = sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)
            .unwrap_err();

        assert!(!error.is_timeout());
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_snapshot_transaction_is_repeatable_read_only_and_ignores_later_commits(
        pool: PgPool,
    ) {
        sqlx::query("CREATE TABLE snapshot_probe (n integer)")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = begin_snapshot_for(&pool, Duration::from_secs(5))
            .await
            .unwrap();
        let (level, read_only): (String, String) = sqlx::query_as(
            "SELECT current_setting('transaction_isolation'), current_setting('transaction_read_only')",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM snapshot_probe")
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        sqlx::query("INSERT INTO snapshot_probe VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM snapshot_probe")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let write = sqlx::query("INSERT INTO snapshot_probe VALUES (2)")
            .execute(&mut *tx)
            .await;

        assert_eq!(
            (level.as_str(), read_only.as_str()),
            ("repeatable read", "on")
        );
        assert_eq!((before, after), (0, 0));
        assert!(write.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_plain_bounded_read_stays_read_committed(pool: PgPool) {
        let mut tx = begin_bounded_for(&pool, Duration::from_secs(5))
            .await
            .unwrap();

        let level: String = sqlx::query_scalar("SELECT current_setting('transaction_isolation')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        assert_eq!(level, "read committed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancelled_plain_query_error_is_reclassified_as_a_timeout_only_when_asked(
        pool: PgPool,
    ) {
        let mut tx = begin_snapshot_for(&pool, Duration::from_millis(50))
            .await
            .unwrap();

        let plain = sqlx::query("SELECT pg_sleep(2)")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)
            .unwrap_err();
        let plain_is_timeout = plain.is_timeout();
        let reclassified = plain.into_bounded();

        assert!(!plain_is_timeout);
        assert!(reclassified.is_timeout());
        assert!(!StoreError::retryable_race().into_bounded().is_timeout());
    }
}
