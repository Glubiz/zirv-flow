//! Decision-log retention (architecture section 16.3, OQ-28): whole monthly partitions past the
//! deployment window are dropped, the rows of the month that holds the cutoff (and of the DEFAULT
//! partition) are deleted in batches, and a workspace with a shorter window loses its older rows
//! the same way. A decision record is not a source of handled markers, so nothing is marked.
//! Every drop and every batch commits on its own and a rerun continues from what is committed.

use hashbrown::HashMap;
use sqlx::PgPool;
use time::{Date, Duration, Month, OffsetDateTime, UtcOffset};
use zirv_flow_core::WorkspaceId;

use super::next_month;
use crate::StoreError;
use crate::events::RETENTION_BATCH;

const PARTITION_PREFIX: &str = "decision_records_y";

/// The operator's decision-log retention settings, in days.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecisionRetention {
    /// How long records are kept unless their workspace has a shorter window.
    pub days: u32,
    /// The shortest and the longest window a workspace may set.
    pub min_days: u32,
    pub max_days: u32,
}

impl Default for DecisionRetention {
    fn default() -> Self {
        Self {
            days: 395,
            min_days: 30,
            max_days: 760,
        }
    }
}

/// What one run removed. Counts only.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DecisionRetentionReport {
    pub partitions_dropped: u64,
    pub records_deleted: u64,
}

/// Applies the retention as of `now` at the deployment window.
pub async fn apply_decision_retention(
    pool: &PgPool,
    settings: &DecisionRetention,
    now: OffsetDateTime,
) -> Result<DecisionRetentionReport, StoreError> {
    apply_decision_retention_for(pool, settings, &HashMap::new(), now).await
}

/// Like [`apply_decision_retention`], with their own window in days for the workspaces in
/// `workspace_days`. A workspace not in the map keeps the deployment window.
pub async fn apply_decision_retention_for(
    pool: &PgPool,
    settings: &DecisionRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
) -> Result<DecisionRetentionReport, StoreError> {
    retain_in_batches(pool, settings, workspace_days, now, RETENTION_BATCH).await
}

async fn retain_in_batches(
    pool: &PgPool,
    settings: &DecisionRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
    batch_size: i64,
) -> Result<DecisionRetentionReport, StoreError> {
    let cutoff = cutoff_of(now, settings.days);
    let mut report = DecisionRetentionReport {
        partitions_dropped: drop_old_partitions(pool, cutoff, now).await?,
        records_deleted: 0,
    };
    report.records_deleted += delete_in_batches(
        pool,
        "WITH gone AS (DELETE FROM decision_records d USING ( \
            SELECT recorded_at, id FROM decision_records WHERE recorded_at < $1 \
            ORDER BY recorded_at, id LIMIT $2) old \
         WHERE d.recorded_at = old.recorded_at AND d.id = old.id RETURNING 1) \
         SELECT count(*) FROM gone",
        |query| query.bind(cutoff).bind(batch_size),
        batch_size,
    )
    .await?;
    for (workspace, days) in workspace_days {
        report.records_deleted += delete_in_batches(
            pool,
            "WITH gone AS (DELETE FROM decision_records d USING ( \
                SELECT recorded_at, id FROM decision_records \
                WHERE workspace_id = $1 AND recorded_at < $2 LIMIT $3) old \
             WHERE d.recorded_at = old.recorded_at AND d.id = old.id RETURNING 1) \
             SELECT count(*) FROM gone",
            |query| {
                query
                    .bind(workspace.as_uuid())
                    .bind(cutoff_of(now, *days))
                    .bind(batch_size)
            },
            batch_size,
        )
        .await?;
    }
    Ok(report)
}

fn cutoff_of(now: OffsetDateTime, days: u32) -> OffsetDateTime {
    now - Duration::days(i64::from(days))
}

/// Runs `statement`, which returns the number of rows it deleted as one `bigint`, until a batch
/// comes back short. Each run commits on its own.
async fn delete_in_batches<'q>(
    pool: &PgPool,
    statement: &'static str,
    bind: impl Fn(
        sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    batch_size: i64,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted: i64 = bind(sqlx::query_scalar(statement))
            .fetch_one(pool)
            .await
            .map_err(StoreError::query)?;
        total += u64::try_from(deleted).unwrap_or(0);
        if deleted < batch_size {
            return Ok(total);
        }
    }
}

/// Drops each monthly partition whose upper bound is at or before `cutoff`. Never the DEFAULT
/// partition, never a partition that is not named for a month, and never the current or the next
/// month, whatever the window. Each drop is its own transaction and `DROP TABLE IF EXISTS`, so a
/// concurrent run that dropped it first is not an error.
async fn drop_old_partitions(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
         WHERE i.inhparent = 'decision_records'::regclass",
    )
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let this_month = now
        .to_offset(UtcOffset::UTC)
        .date()
        .replace_day(1)
        .expect("day 1 exists in every month");
    let mut dropped = 0;
    for name in names {
        let Some(first) = month_of(&name) else {
            continue;
        };
        if first >= this_month || next_month(first).midnight().assume_utc() > cutoff {
            continue;
        }
        // The name was parsed from digits only and rebuilt from them.
        let statement = format!(
            "DROP TABLE IF EXISTS decision_records_y{:04}m{:02}",
            first.year(),
            u8::from(first.month())
        );
        let mut tx = crate::begin(pool).await?;
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)?;
        crate::commit(tx).await?;
        dropped += 1;
    }
    Ok(dropped)
}

/// The first day of the month a partition named `decision_records_yYYYYmMM` covers.
fn month_of(name: &str) -> Option<Date> {
    let (year, month) = name.strip_prefix(PARTITION_PREFIX)?.split_once('m')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    let month = Month::try_from(month.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(year.parse().ok()?, month, 1).ok()
}

#[cfg(test)]
mod tests {
    use time::macros::{date, datetime};
    use uuid::Uuid;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::decisions::ensure_decision_partitions;
    use crate::{create_workspace, decisions::decision_log_size};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    fn settings() -> DecisionRetention {
        DecisionRetention {
            days: 100,
            min_days: 7,
            max_days: 700,
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn insert_at(pool: &PgPool, ws: WorkspaceId, at: OffsetDateTime, action: &str) {
        sqlx::query(
            "INSERT INTO decision_records \
             (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id) \
             VALUES ($1, $2, $3, $4, $5, 'done', 'api', 'x')",
        )
        .bind(Uuid::new_v4())
        .bind(ws.as_uuid())
        .bind(Uuid::new_v4())
        .bind(at)
        .bind(action)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn actions(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM decision_records ORDER BY action")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn exists(pool: &PgPool, name: &str) -> bool {
        sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Makes the partitions of the month of `day` and the next one.
    async fn partitions(pool: &PgPool, day: Date) {
        ensure_decision_partitions(pool, day).await.unwrap();
    }

    #[test]
    fn the_defaults_are_13_months_and_30_days_to_25_months() {
        let defaults = DecisionRetention::default();

        assert_eq!(
            (defaults.days, defaults.min_days, defaults.max_days),
            (395, 30, 760)
        );
    }

    #[test]
    fn only_a_partition_named_for_a_month_has_a_month() {
        assert_eq!(
            month_of("decision_records_y2025m01"),
            Some(date!(2025 - 01 - 01))
        );
        for name in [
            "decision_records_default",
            "decision_records_y2025m13",
            "decision_records_y25m01",
            "decision_records_y2025m1",
            "events_y2025m01",
        ] {
            assert_eq!(month_of(name), None, "{name}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_partition_wholly_past_the_window_is_dropped_and_the_cutoff_month_and_default_stay(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        for day in [
            date!(2025 - 01 - 10),
            date!(2026 - 01 - 10),
            date!(2026 - 05 - 10),
        ] {
            partitions(&pool, day).await;
        }
        insert_at(&pool, ws, datetime!(2025-01-15 00:00 UTC), "january-2025").await;
        insert_at(&pool, ws, datetime!(2026-01-31 23:00 UTC), "january-2026").await;
        insert_at(&pool, ws, datetime!(2026-02-25 00:00 UTC), "cutoff-month").await;
        insert_at(&pool, ws, datetime!(2026-05-20 00:00 UTC), "may").await;

        let report = apply_decision_retention(&pool, &settings(), NOW)
            .await
            .unwrap();

        assert_eq!(report.partitions_dropped, 3);
        for gone in [
            "decision_records_y2025m01",
            "decision_records_y2025m02",
            "decision_records_y2026m01",
        ] {
            assert!(!exists(&pool, gone).await, "{gone}");
        }
        for kept in [
            "decision_records_y2026m02",
            "decision_records_y2026m05",
            "decision_records_y2026m06",
            "decision_records_default",
        ] {
            assert!(exists(&pool, kept).await, "{kept}");
        }
        assert_eq!(actions(&pool).await, ["cutoff-month", "may"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_current_month_is_not_dropped_even_when_the_window_is_over(pool: PgPool) {
        partitions(&pool, date!(2026 - 05 - 10)).await;
        let none = DecisionRetention {
            days: 0,
            ..settings()
        };

        let report = apply_decision_retention(&pool, &none, NOW).await.unwrap();

        assert_eq!(report.partitions_dropped, 1);
        assert!(!exists(&pool, "decision_records_y2026m05").await);
        assert!(exists(&pool, "decision_records_y2026m06").await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rows_past_the_cutoff_in_the_cutoff_month_go_in_batches_and_rows_inside_stay(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        partitions(&pool, date!(2026 - 02 - 10)).await;
        // The cutoff is 2026-02-21 12:00 UTC.
        for day in 1..=5 {
            let at = datetime!(2026-02-01 00:00 UTC) + Duration::days(day);
            insert_at(&pool, ws, at, &format!("old{day}")).await;
        }
        insert_at(&pool, ws, datetime!(2026-02-21 11:59 UTC), "just-before").await;
        insert_at(&pool, ws, datetime!(2026-02-21 12:00 UTC), "at-cutoff").await;
        insert_at(&pool, ws, datetime!(2026-02-27 00:00 UTC), "inside").await;

        let report = retain_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2)
            .await
            .unwrap();

        assert_eq!(report.records_deleted, 6);
        assert_eq!(actions(&pool).await, ["at-cutoff", "inside"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rows_in_the_default_partition_past_the_window_are_deleted_and_it_stays(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        insert_at(&pool, ws, datetime!(2020-03-05 00:00 UTC), "ancient").await;

        let report = apply_decision_retention(&pool, &settings(), NOW)
            .await
            .unwrap();

        assert_eq!((report.partitions_dropped, report.records_deleted), (0, 1));
        assert!(exists(&pool, "decision_records_default").await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_with_a_shorter_window_loses_its_older_rows_only(pool: PgPool) {
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        partitions(&pool, date!(2026 - 04 - 10)).await;
        let forty_days_ago = NOW - Duration::days(40);
        insert_at(&pool, ours, forty_days_ago, "ours-old").await;
        insert_at(&pool, ours, NOW - Duration::days(10), "ours-new").await;
        insert_at(&pool, theirs, forty_days_ago, "theirs-old").await;
        let windows = HashMap::from([(ours, 30_u32)]);

        let report = apply_decision_retention_for(&pool, &settings(), &windows, NOW)
            .await
            .unwrap();

        assert_eq!(report.records_deleted, 1);
        assert_eq!(actions(&pool).await, ["ours-new", "theirs-old"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drop_that_races_another_drop_succeeds_and_a_rerun_is_a_no_op(pool: PgPool) {
        partitions(&pool, date!(2025 - 01 - 10)).await;
        let settings = settings();
        let run = || apply_decision_retention(&pool, &settings, NOW);

        let (first, second) = tokio::join!(run(), run());

        assert!(first.is_ok() && second.is_ok());
        assert!(!exists(&pool, "decision_records_y2025m01").await);
        let again = run().await.unwrap();
        assert_eq!(again, DecisionRetentionReport::default());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_drop_tolerates_a_partition_that_is_already_gone(pool: PgPool) {
        partitions(&pool, date!(2025 - 01 - 10)).await;
        sqlx::query("DROP TABLE decision_records_y2025m01")
            .execute(&pool)
            .await
            .unwrap();

        let report = apply_decision_retention(&pool, &settings(), NOW)
            .await
            .unwrap();

        assert_eq!(
            report.partitions_dropped, 1,
            "only decision_records_y2025m02"
        );
        assert!(decision_log_size(&pool).await.unwrap().partitions >= 3);
    }
}
