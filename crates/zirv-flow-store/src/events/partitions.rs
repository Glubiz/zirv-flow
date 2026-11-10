//! Monthly partitions of the event table, kept like the decision log's (architecture
//! section 12.4): this month and the next, created by the scheduler leader.

use sqlx::PgPool;
use time::Date;

use crate::StoreError;
use crate::decisions::{bound, next_month};

/// Creates the monthly partitions for the month of `today` and the next one, if they are
/// missing. Fails when the DEFAULT partition already holds rows of a month, which cannot then
/// get a partition of its own; those rows stay readable. Every month is attempted before the
/// first error is returned.
pub async fn ensure_event_partitions(pool: &PgPool, today: Date) -> Result<(), StoreError> {
    let first = today.replace_day(1).expect("day 1 exists in every month");
    let mut first_error = None;
    for start in [first, next_month(first)] {
        let end = next_month(start);
        // The names and bounds are built from integers only.
        let statement = format!(
            "CREATE TABLE IF NOT EXISTS {} PARTITION OF events \
             FOR VALUES FROM ('{}') TO ('{}')",
            partition_name(start),
            bound(start),
            bound(end),
        );
        // Idempotent, so a partition that already exists gets the settings too.
        let tune = format!(
            "ALTER TABLE {} SET ({VACUUM_SETTINGS})",
            partition_name(start)
        );
        let created = async {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(pool)
                .await?;
            sqlx::query(sqlx::AssertSqlSafe(tune)).execute(pool).await
        }
        .await;
        if let Err(error) = created {
            first_error.get_or_insert(StoreError::query(error));
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Events are insert-only, so autovacuum would reach a partition only after 20% growth and its
/// visibility map would lag; a fixed insert threshold keeps index-only scans off the heap. A
/// partitioned parent cannot hold these options, so every partition carries them (migration 0094
/// sets them on the existing ones).
const VACUUM_SETTINGS: &str =
    "autovacuum_vacuum_insert_scale_factor = 0, autovacuum_vacuum_insert_threshold = 500000";

pub(super) const PARTITION_PREFIX: &str = "events_y";

fn partition_name(first_of_month: Date) -> String {
    format!(
        "{PARTITION_PREFIX}{:04}m{:02}",
        first_of_month.year(),
        u8::from(first_of_month.month())
    )
}

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;

    #[test]
    fn names_roll_over_the_year() {
        assert_eq!(partition_name(date!(2027 - 01 - 01)), "events_y2027m01");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn this_month_and_the_next_get_partitions_and_a_second_call_changes_nothing(
        pool: PgPool,
    ) {
        ensure_event_partitions(&pool, date!(2041 - 12 - 17))
            .await
            .unwrap();
        ensure_event_partitions(&pool, date!(2041 - 12 - 03))
            .await
            .unwrap();

        let partitions: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'events'::regclass AND c.relname LIKE 'events_y2041%' \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(partitions, ["events_y2041m12"]);
        let next: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_class WHERE relname = 'events_y2042m01'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(next, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_partition_carries_the_autovacuum_insert_settings(pool: PgPool) {
        ensure_event_partitions(&pool, date!(2043 - 05 - 10))
            .await
            .unwrap();

        let options: Vec<String> = sqlx::query_scalar(
            "SELECT unnest(reloptions) FROM pg_class WHERE relname = 'events_y2043m05' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            options,
            [
                "autovacuum_vacuum_insert_scale_factor=0",
                "autovacuum_vacuum_insert_threshold=500000"
            ]
        );
        let untuned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'events'::regclass AND c.reloptions IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(untuned, 0, "the migration tunes the partitions it made");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_created_partitions_back_to_the_default_retention(pool: PgPool) {
        let oldest: String = sqlx::query_scalar(
            "SELECT min(c.relname::text) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'events'::regclass AND c.relname <> 'events_default'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let expected: String = sqlx::query_scalar(
            "SELECT to_char(date_trunc('month', (now() - interval '760 days') AT TIME ZONE 'UTC'), \
                            '\"events_y\"YYYY\"m\"MM')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(oldest, expected);
    }
}
