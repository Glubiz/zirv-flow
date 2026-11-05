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
        let created = sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await
            .map_err(StoreError::query);
        if let Err(error) = created {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

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

    /// The partitions of `events`, by name.
    async fn partitions(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'events'::regclass ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// Each partition of `events` with the index attached to `events_name_person_idx` on it, as
    /// `(partition, Some((index oid, valid)))`, or `None` for a partition without one.
    async fn name_person_indexes(pool: &PgPool) -> Vec<(String, Option<(i64, bool)>)> {
        let rows: Vec<(String, Option<i64>, Option<bool>)> = sqlx::query_as(
            "SELECT c.relname::text, x.indexrelid::int8, x.indisvalid \
             FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             LEFT JOIN pg_index x ON x.indrelid = c.oid AND x.indexrelid IN \
                 (SELECT inhrelid FROM pg_inherits \
                  WHERE inhparent = to_regclass('events_name_person_idx')) \
             WHERE i.inhparent = 'events'::regclass ORDER BY 1",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(partition, oid, valid)| (partition, oid.zip(valid)))
            .collect()
    }

    /// Whether the old index is gone and the new parent index is valid.
    async fn swapped(pool: &PgPool) -> (bool, bool) {
        sqlx::query_as(
            "SELECT to_regclass('events_name_idx') IS NULL, \
                    (SELECT indisvalid FROM pg_index \
                     WHERE indexrelid = 'events_name_person_idx'::regclass)",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// The statement upgrade.md gives for one partition.
    fn build_by_hand(partition: &str) -> String {
        format!(
            "CREATE INDEX CONCURRENTLY {partition}_workspace_id_name_occurred_at_person_id_idx \
             ON {partition} (workspace_id, name, occurred_at DESC) INCLUDE (person_id)"
        )
    }

    fn attach_by_hand(partition: &str) -> String {
        format!(
            "ALTER INDEX events_name_person_idx \
             ATTACH PARTITION {partition}_workspace_id_name_occurred_at_person_id_idx"
        )
    }

    async fn run(pool: &PgPool, statement: String) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await
            .unwrap();
    }

    /// The oid of the index the statements above name on `partition`.
    async fn index_oid(pool: &PgPool, partition: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT ($1 || '_workspace_id_name_occurred_at_person_id_idx')::regclass::int8",
        )
        .bind(partition)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn migrate_before_0091(pool: &PgPool) {
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 91)
            .cloned()
            .collect();
        before.run(pool).await.unwrap();
    }

    /// Leaves the index of `partition` as a failed CONCURRENTLY build leaves it, invalid: the
    /// build waits for an open snapshot and is cancelled at its statement timeout first.
    async fn fail_build_by_hand(pool: &PgPool, partition: &str) {
        let mut holder = pool
            .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
            .unwrap();
        sqlx::query("SELECT 1").execute(&mut *holder).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::raw_sql("SET statement_timeout = 300")
            .execute(&mut *connection)
            .await
            .unwrap();
        let failed = sqlx::raw_sql(sqlx::AssertSqlSafe(build_by_hand(partition)))
            .execute(&mut *connection)
            .await;
        sqlx::raw_sql("RESET statement_timeout")
            .execute(&mut *connection)
            .await
            .unwrap();
        holder.rollback().await.unwrap();
        assert!(failed.is_err(), "the build was not cancelled");
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migration_0091_accepts_the_indexes_changed_by_hand(pool: PgPool) {
        migrate_before_0091(&pool).await;
        let names = partitions(&pool).await;
        for partition in &names {
            run(&pool, build_by_hand(partition)).await;
        }
        run(
            &pool,
            "CREATE INDEX events_name_person_idx ON ONLY events \
             (workspace_id, name, occurred_at DESC) INCLUDE (person_id)"
                .to_owned(),
        )
        .await;
        for partition in &names {
            run(&pool, attach_by_hand(partition)).await;
        }
        run(&pool, "DROP INDEX events_name_idx".to_owned()).await;
        let by_hand = name_person_indexes(&pool).await;

        let applied = crate::migrate(&pool).await.unwrap();

        assert_eq!(applied, [91]);
        assert_eq!(
            name_person_indexes(&pool).await,
            by_hand,
            "nothing is rebuilt"
        );
        assert!(
            by_hand
                .iter()
                .all(|(_, index)| index.is_some_and(|(_, valid)| valid))
        );
        assert_eq!(swapped(&pool).await, (true, true));
        // A partition the scheduler adds later gets the index too.
        ensure_event_partitions(&pool, date!(2041 - 12 - 17))
            .await
            .unwrap();
        let later: Vec<(String, Option<(i64, bool)>)> = name_person_indexes(&pool)
            .await
            .into_iter()
            .filter(|(partition, _)| partition.as_str() >= "events_y2041")
            .collect();
        assert_eq!(later.len(), 2);
        assert!(
            later
                .iter()
                .all(|(_, index)| index.is_some_and(|(_, valid)| valid))
        );
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migration_0091_resumes_a_manual_run_that_stopped_halfway(pool: PgPool) {
        migrate_before_0091(&pool).await;
        let names = partitions(&pool).await;
        // Two partitions built and attached, one built and not attached, one failed build that
        // was attached anyway and one that was not, and the rest not started; the old index stays.
        for partition in &names[..3] {
            run(&pool, build_by_hand(partition)).await;
        }
        fail_build_by_hand(&pool, &names[3]).await;
        fail_build_by_hand(&pool, &names[4]).await;
        run(
            &pool,
            "CREATE INDEX events_name_person_idx ON ONLY events \
             (workspace_id, name, occurred_at DESC) INCLUDE (person_id)"
                .to_owned(),
        )
        .await;
        for partition in [&names[0], &names[1], &names[3]] {
            run(&pool, attach_by_hand(partition)).await;
        }
        let unattached = index_oid(&pool, &names[2]).await;
        let invalid_unattached = index_oid(&pool, &names[4]).await;
        let halfway = name_person_indexes(&pool).await;
        assert_eq!(halfway[3].1.map(|(_, valid)| valid), Some(false));

        let applied = crate::migrate(&pool).await.unwrap();

        assert_eq!(applied, [91]);
        let after = name_person_indexes(&pool).await;
        assert_eq!(after.len(), names.len());
        assert!(
            after
                .iter()
                .all(|(_, index)| index.is_some_and(|(_, valid)| valid))
        );
        let oid = |at: usize| after[at].1.map(|(oid, _)| oid);
        assert_eq!(oid(0), halfway[0].1.map(|(oid, _)| oid), "kept as built");
        assert_eq!(oid(1), halfway[1].1.map(|(oid, _)| oid), "kept as built");
        assert_eq!(oid(2), Some(unattached), "attached as built");
        assert_eq!(oid(3), halfway[3].1.map(|(oid, _)| oid), "rebuilt in place");
        assert_ne!(oid(4), Some(invalid_unattached), "dropped and built again");
        assert_eq!(swapped(&pool).await, (true, true));
    }
}
