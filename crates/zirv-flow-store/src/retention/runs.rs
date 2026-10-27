//! The last run of each retention window: one instance-level row per window, upserted at the end
//! of the job that enforces it. Day counts and row counts only; no workspace and no person.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;

use crate::StoreError;

/// When a window's job last finished and how many rows it removed.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct RetentionRun {
    pub window: String,
    pub ran_at: OffsetDateTime,
    pub deleted: i64,
}

/// Records that `window` ran at `ran_at` and removed `deleted` rows.
pub async fn record_retention_run(
    executor: impl PgExecutor<'_>,
    window: &str,
    ran_at: OffsetDateTime,
    deleted: u64,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO retention_runs (\"window\", ran_at, deleted) VALUES ($1, $2, $3) \
         ON CONFLICT (\"window\") DO UPDATE SET ran_at = EXCLUDED.ran_at, \
            deleted = EXCLUDED.deleted",
    )
    .bind(window)
    .bind(ran_at)
    .bind(i64::try_from(deleted).unwrap_or(i64::MAX))
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The last run of every window that has run, by window name.
pub async fn retention_runs(
    executor: impl PgExecutor<'_>,
) -> Result<Vec<RetentionRun>, StoreError> {
    sqlx::query_as("SELECT \"window\", ran_at, deleted FROM retention_runs ORDER BY \"window\"")
        .fetch_all(executor)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_run_of_a_window_replaces_the_first(pool: PgPool) {
        let first = datetime!(2026-06-01 4:00 UTC);
        let second = datetime!(2026-06-02 4:00 UTC);
        record_retention_run(&pool, "messages", first, 5)
            .await
            .unwrap();
        record_retention_run(&pool, "audit", first, 0)
            .await
            .unwrap();
        record_retention_run(&pool, "messages", second, 2)
            .await
            .unwrap();

        let runs = retention_runs(&pool).await.unwrap();

        assert_eq!(
            runs,
            [
                RetentionRun {
                    window: "audit".to_owned(),
                    ran_at: first,
                    deleted: 0
                },
                RetentionRun {
                    window: "messages".to_owned(),
                    ran_at: second,
                    deleted: 2
                },
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retention_runs_has_no_person_column(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'retention_runs' \
             ORDER BY ordinal_position",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(columns, ["window", "ran_at", "deleted"]);
    }
}
