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
    // The isolation level and access mode ride on the `BEGIN`, so the snapshot costs no round
    // trip of its own.
    let mut tx = pool
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(StoreError::query)?;
    limit_statements(&mut tx, timeout).await?;
    Ok(tx)
}

/// The planner's estimate of how many rows `sql` returns, from the table statistics: the
/// statement is planned and never run, so the estimate costs a plan however many rows match.
/// `binds` are its text parameters, `$1` first; it is sent as an unnamed statement, so it is
/// planned for their values and never with a generic plan. It is as current as the last
/// `ANALYZE` of the table and the table's size.
pub(crate) async fn planned_rows(
    connection: &mut PgConnection,
    sql: &'static str,
    binds: &[&str],
) -> Result<i64, StoreError> {
    let mut query = sqlx::query_scalar::<_, serde_json::Value>(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) {sql}"
    )))
    .persistent(false);
    for bind in binds {
        query = query.bind(*bind);
    }
    let plan = query
        .fetch_one(connection)
        .await
        .map_err(StoreError::query)?;
    let rows = plan
        .pointer("/0/Plan/Plan Rows")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    // A float that does not fit saturates.
    Ok(rows.round() as i64)
}

/// A count that stays cheap however many rows match. `counted` is how many rows a read limited
/// to `exact + 1` found: up to `exact` it is the count; above, the count is [`planned_rows`] of
/// `sql`, and never less than `counted`.
pub(crate) async fn count_or_estimate(
    connection: &mut PgConnection,
    counted: i64,
    exact: i64,
    sql: &'static str,
    binds: &[&str],
) -> Result<i64, StoreError> {
    if counted <= exact {
        return Ok(counted);
    }
    Ok(planned_rows(connection, sql, binds).await?.max(counted))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_count_above_its_exact_limit_is_the_planners_estimate(pool: PgPool) {
        const PROBE: &str = "SELECT 1 FROM estimate_probe WHERE tag = $1";
        sqlx::query("CREATE TABLE estimate_probe (tag text NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO estimate_probe SELECT CASE WHEN g <= 3 THEN 'few' ELSE 'many' END \
             FROM generate_series(1, 5000) g",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE estimate_probe")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let many = planned_rows(&mut connection, PROBE, &["many"])
            .await
            .unwrap();
        let few = planned_rows(&mut connection, PROBE, &["few"])
            .await
            .unwrap();

        // ANALYZE samples every row of a table this small, so the estimates are the counts.
        assert_eq!((many, few), (4997, 3));
        assert_eq!(
            count_or_estimate(&mut connection, 3, 10, PROBE, &["few"])
                .await
                .unwrap(),
            3,
            "exact up to the limit"
        );
        assert_eq!(
            count_or_estimate(&mut connection, 11, 10, PROBE, &["many"])
                .await
                .unwrap(),
            4997
        );
        assert_eq!(
            count_or_estimate(&mut connection, 11, 10, PROBE, &["few"])
                .await
                .unwrap(),
            11,
            "never below the rows read"
        );
    }

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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_snapshot_transaction_carries_the_isolation_access_mode_and_timeout_from_its_first_statement(
        pool: PgPool,
    ) {
        let mut tx = begin_snapshot_for(&pool, Duration::from_millis(7_500))
            .await
            .unwrap();

        let (level, read_only, timeout): (String, String, String) = sqlx::query_as(
            "SELECT current_setting('transaction_isolation'),                     current_setting('transaction_read_only'), current_setting('statement_timeout')",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();

        assert_eq!(
            (level.as_str(), read_only.as_str(), timeout.as_str()),
            ("repeatable read", "on", "7500ms")
        );
    }
}
