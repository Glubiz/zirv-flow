//! Decision-log retention (architecture section 16.3, OQ-28): whole monthly partitions past the
//! deployment window are dropped, the rows of the month that holds the cutoff (and of the DEFAULT
//! partition) are deleted in batches, and a workspace with a shorter window loses its older rows
//! the same way. A decision record is not a source of handled markers, so nothing is marked.
//! Every drop and every batch commits on its own and a rerun continues from what is committed.
//!
//! A drop takes ACCESS EXCLUSIVE on `decision_records`, so it waits a short lock timeout at most
//! (`drop_lock`) and is then skipped until the next run; otherwise one long transaction would queue every
//! append behind the drop. The workspace delete walks `(recorded_at, id)`, the primary key order
//! (no new index: each one adds bytes to every record), with a cursor carried between batches, so
//! no batch rescans what an earlier one passed. The remaining cost is that it reads the rows of
//! the other workspaces below the cutoff once.

use hashbrown::HashMap;
use sqlx::PgPool;
use time::macros::datetime;
use time::{Date, Duration, Month, OffsetDateTime, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::next_month;
use crate::StoreError;
use crate::drop_lock::{is_lock_timeout, set_drop_lock_timeout};
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
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecisionRetentionReport {
    pub partitions_dropped: u64,
    /// Partitions a drop gave up on because the table was locked; the next run retries them.
    pub skipped_partitions: Vec<String>,
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
    let mut report = DecisionRetentionReport::default();
    apply_decision_retention_progress(pool, settings, workspace_days, now, &mut report).await?;
    Ok(report)
}

/// Like [`apply_decision_retention_for`], but adds what it removed to `report` as it goes, so a
/// caller keeps the count of the drops and batches that committed before a later step failed.
pub async fn apply_decision_retention_progress(
    pool: &PgPool,
    settings: &DecisionRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
    report: &mut DecisionRetentionReport,
) -> Result<(), StoreError> {
    retain_in_batches(pool, settings, workspace_days, now, RETENTION_BATCH, report).await
}

async fn retain_in_batches(
    pool: &PgPool,
    settings: &DecisionRetention,
    workspace_days: &HashMap<WorkspaceId, u32>,
    now: OffsetDateTime,
    batch_size: i64,
    report: &mut DecisionRetentionReport,
) -> Result<(), StoreError> {
    let cutoff = cutoff_of(now, settings.days);
    drop_old_partitions(pool, cutoff, now, report).await?;
    delete_before_cutoff(pool, cutoff, batch_size, report).await?;
    for (workspace, days) in workspace_days {
        let cutoff = cutoff_of(now, *days);
        delete_workspace_rows(pool, *workspace, cutoff, batch_size, report).await?;
    }
    Ok(())
}

fn cutoff_of(now: OffsetDateTime, days: u32) -> OffsetDateTime {
    now - Duration::days(i64::from(days))
}

/// Deletes every row older than `cutoff`, `batch_size` at a time, each batch in its own commit.
/// The rows found are the oldest, so each batch starts where the deleted ones were.
async fn delete_before_cutoff(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    batch_size: i64,
    report: &mut DecisionRetentionReport,
) -> Result<(), StoreError> {
    loop {
        let deleted: i64 = sqlx::query_scalar(
            "WITH gone AS (DELETE FROM decision_records d USING ( \
                SELECT recorded_at, id FROM decision_records WHERE recorded_at < $1 \
                ORDER BY recorded_at, id LIMIT $2) old \
             WHERE d.recorded_at = old.recorded_at AND d.id = old.id RETURNING 1) \
             SELECT count(*) FROM gone",
        )
        .bind(cutoff)
        .bind(batch_size)
        .fetch_one(pool)
        .await
        .map_err(StoreError::query)?;
        report.records_deleted += u64::try_from(deleted).unwrap_or(0);
        if deleted < batch_size {
            return Ok(());
        }
    }
}

/// Deletes the rows of `workspace` older than `cutoff`, `batch_size` at a time, each batch in its
/// own commit, and returns how many batches ran. Each batch starts after the last key of the one
/// before, so the rows of other workspaces below the cutoff are read once, not by every batch.
async fn delete_workspace_rows(
    pool: &PgPool,
    workspace: WorkspaceId,
    cutoff: OffsetDateTime,
    batch_size: i64,
    report: &mut DecisionRetentionReport,
) -> Result<u64, StoreError> {
    let mut last: (OffsetDateTime, Uuid) = (datetime!(0001-01-01 0:00 UTC), Uuid::nil());
    let mut batches = 0;
    loop {
        let (deleted, at, id): (i64, Option<OffsetDateTime>, Option<Uuid>) = sqlx::query_as(
            "WITH batch AS ( \
                SELECT recorded_at, id FROM decision_records \
                WHERE workspace_id = $1 AND recorded_at < $2 AND (recorded_at, id) > ($3, $4) \
                ORDER BY recorded_at, id LIMIT $5), \
             gone AS (DELETE FROM decision_records d USING batch \
                WHERE d.recorded_at = batch.recorded_at AND d.id = batch.id RETURNING 1) \
             SELECT (SELECT count(*) FROM gone), l.recorded_at, l.id \
             FROM (SELECT 1) one LEFT JOIN LATERAL ( \
                SELECT recorded_at, id FROM batch ORDER BY recorded_at DESC, id DESC LIMIT 1) l \
                ON true",
        )
        .bind(workspace.as_uuid())
        .bind(cutoff)
        .bind(last.0)
        .bind(last.1)
        .bind(batch_size)
        .fetch_one(pool)
        .await
        .map_err(StoreError::query)?;
        batches += 1;
        report.records_deleted += u64::try_from(deleted).unwrap_or(0);
        if let (Some(at), Some(id)) = (at, id) {
            last = (at, id);
        }
        if deleted < batch_size {
            return Ok(batches);
        }
    }
}

/// Drops each monthly partition whose upper bound is at or before `cutoff`. Never the DEFAULT
/// partition, never a partition that is not named for a month, and never the current or the next
/// month, whatever the window. Each drop is its own transaction and `DROP TABLE IF EXISTS`, so a
/// concurrent run that dropped it first is not an error. A drop that cannot get its lock within
/// the lock timeout of `drop_lock` is counted as skipped and left for the next run.
async fn drop_old_partitions(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
    report: &mut DecisionRetentionReport,
) -> Result<(), StoreError> {
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
        set_drop_lock_timeout(&mut tx).await?;
        match sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {
                crate::commit(tx).await?;
                report.partitions_dropped += 1;
            }
            Err(error) if is_lock_timeout(&error) => {
                report.skipped_partitions.push(name);
            }
            Err(error) => return Err(StoreError::query(error)),
        }
    }
    Ok(())
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

        let mut report = DecisionRetentionReport::default();
        retain_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2, &mut report)
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_workspace_delete_walks_forward_and_reads_each_row_below_the_cutoff_once(
        pool: PgPool,
    ) {
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        partitions(&pool, date!(2026 - 04 - 10)).await;
        let base = NOW - Duration::days(60);
        for minute in 0..5 {
            let at = base + Duration::minutes(minute);
            insert_at(&pool, ours, at, &format!("ours{minute}")).await;
            insert_at(&pool, theirs, at, &format!("theirs{minute}")).await;
        }
        let mut report = DecisionRetentionReport::default();

        let batches = delete_workspace_rows(&pool, ours, NOW - Duration::days(30), 2, &mut report)
            .await
            .unwrap();

        // 2 + 2 + 1 rows: three batches, the last one short, none of them a rescan.
        assert_eq!((batches, report.records_deleted), (3, 5));
        let left = actions(&pool).await;
        assert_eq!(left.len(), 5);
        assert!(left.iter().all(|action| action.starts_with("theirs")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_locked_table_skips_the_drop_and_other_work_runs_and_a_rerun_drops_it(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        partitions(&pool, date!(2025 - 01 - 10)).await;
        insert_at(&pool, ws, datetime!(2020-03-05 00:00 UTC), "ancient").await;
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE decision_records IN ROW EXCLUSIVE MODE")
            .execute(&mut *holder)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let report = apply_decision_retention(&pool, &settings(), NOW)
            .await
            .unwrap();

        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!((report.partitions_dropped, report.records_deleted), (0, 1));
        let mut skipped = report.skipped_partitions.clone();
        skipped.sort();
        assert_eq!(
            skipped,
            ["decision_records_y2025m01", "decision_records_y2025m02"]
        );
        assert!(exists(&pool, "decision_records_y2025m01").await);
        holder.rollback().await.unwrap();

        let again = apply_decision_retention(&pool, &settings(), NOW)
            .await
            .unwrap();

        assert_eq!(again.partitions_dropped, 2);
        assert!(again.skipped_partitions.is_empty());
        assert!(!exists(&pool, "decision_records_y2025m01").await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_a_committed_batch_keeps_that_batch_in_the_report(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        for minute in 0..3 {
            let at = datetime!(2020-03-05 00:00 UTC) + Duration::minutes(minute);
            insert_at(&pool, ws, at, &format!("old{minute}")).await;
        }
        sqlx::query(
            "CREATE FUNCTION refuse_last() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF OLD.action = 'old2' THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN OLD; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_last BEFORE DELETE ON decision_records_default \
             FOR EACH ROW EXECUTE FUNCTION refuse_last()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut report = DecisionRetentionReport::default();

        let result =
            retain_in_batches(&pool, &settings(), &HashMap::new(), NOW, 2, &mut report).await;

        assert!(result.is_err());
        assert_eq!(report.records_deleted, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retention_pass_racing_the_partition_create_succeeds_and_both_months_exist(
        pool: PgPool,
    ) {
        partitions(&pool, date!(2025 - 01 - 10)).await;
        let today = OffsetDateTime::now_utc();
        let settings = settings();

        let (pass, created) = tokio::join!(
            apply_decision_retention(&pool, &settings, today),
            ensure_decision_partitions(&pool, today.date()),
        );

        assert!(pass.is_ok() && created.is_ok(), "{pass:?} {created:?}");
        let this = today.date().replace_day(1).unwrap();
        for month in [this, next_month(this)] {
            let name = format!(
                "decision_records_y{:04}m{:02}",
                month.year(),
                u8::from(month.month())
            );
            assert!(exists(&pool, &name).await, "{name}");
        }
    }
}
