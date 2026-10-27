//! Event retention (architecture section 16.3): per-name deletes for names with an override,
//! deletes at the deployment default for the others, whole old partitions, old rejects and the
//! dedupe records of events that no longer exist. Every step is batched and idempotent.

use sqlx::{PgConnection, PgExecutor, PgPool};
use time::{Date, Duration, Month, OffsetDateTime};

use super::partitions::PARTITION_PREFIX;
use crate::StoreError;
use crate::decisions::next_month;
use crate::drop_lock::{is_lock_timeout, set_drop_lock_timeout};

/// Rows deleted per statement.
pub const RETENTION_BATCH: i64 = 5_000;

/// The operator's retention settings, in days.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventRetention {
    /// How long events are kept unless their name has an override.
    pub default_days: u32,
    /// The shortest and the longest override a workspace may set.
    pub min_days: u32,
    pub max_days: u32,
    /// How long ingest rejects are kept.
    pub reject_days: u32,
}

impl Default for EventRetention {
    fn default() -> Self {
        Self {
            default_days: 760,
            min_days: 30,
            max_days: 1095,
            reject_days: 30,
        }
    }
}

/// Deletes events past the retention of their own name, read from the schema row by every
/// statement, so a batch always uses the override as it is now. Both event deletes also write the
/// handled markers of the `sync.*` events they remove (`privacy::handled_markers`), in the same
/// statement.
fn overridden_delete() -> String {
    crate::privacy::delete_and_mark(&format!(
        "DELETE FROM events e USING (             SELECT x.occurred_at, x.id FROM events x             JOIN event_schemas s ON s.workspace_id = x.workspace_id AND s.name = x.name             WHERE s.retention_days IS NOT NULL               AND x.occurred_at < $1::timestamptz - make_interval(days => s.retention_days)             LIMIT $2) d          WHERE e.occurred_at = d.occurred_at AND e.id = d.id RETURNING {}",
        crate::privacy::event_columns("e")
    ))
}

/// Deletes the events past the deployment default that have no override, the same way.
fn default_delete() -> String {
    crate::privacy::delete_and_mark(&format!(
        "DELETE FROM events e USING (             SELECT occurred_at, id FROM events x WHERE occurred_at < $1               AND NOT EXISTS (SELECT 1 FROM event_schemas s                               WHERE s.workspace_id = x.workspace_id AND s.name = x.name                                 AND s.retention_days IS NOT NULL)             LIMIT $2) d          WHERE e.occurred_at = d.occurred_at AND e.id = d.id RETURNING {}",
        crate::privacy::event_columns("e")
    ))
}

/// What one run removed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetentionReport {
    pub events_deleted: u64,
    pub partitions_dropped: u64,
    /// Partitions a drop gave up on because the table was locked; the next run retries them.
    pub skipped_partitions: Vec<String>,
    pub rejects_deleted: u64,
    pub dedupe_deleted: u64,
    /// Test-send log rows past their 13 months (architecture 16.3).
    pub test_sends_deleted: u64,
}

/// Applies the retention rules as of `now`, committing each batch on the pool. A rerun after
/// a failure continues from the committed batches and counts only what it removes.
pub async fn apply_event_retention(
    pool: &PgPool,
    settings: &EventRetention,
    windows: &crate::RetentionSettings,
    now: OffsetDateTime,
) -> Result<RetentionReport, StoreError> {
    apply_event_retention_in_batches(pool, settings, windows, now, RETENTION_BATCH).await
}

async fn apply_event_retention_in_batches(
    pool: &PgPool,
    settings: &EventRetention,
    windows: &crate::RetentionSettings,
    now: OffsetDateTime,
    batch_size: i64,
) -> Result<RetentionReport, StoreError> {
    let default_days = i32::try_from(settings.default_days).unwrap_or(i32::MAX);
    let mut report = RetentionReport::default();

    report.events_deleted +=
        delete_events_in_batches(pool, &overridden_delete(), batch_size, |query| {
            query.bind(now).bind(batch_size)
        })
        .await?;

    let default_cutoff = now - Duration::days(i64::from(settings.default_days));
    report.events_deleted +=
        delete_events_in_batches(pool, &default_delete(), batch_size, |query| {
            query.bind(default_cutoff).bind(batch_size)
        })
        .await?;

    report.rejects_deleted = delete_in_batches(
        pool,
        "DELETE FROM ingest_rejects WHERE id IN ( \
            SELECT id FROM ingest_rejects WHERE received_at < $1 LIMIT $2)",
        batch_size,
        |query| {
            query
                .bind(now - Duration::days(i64::from(settings.reject_days)))
                .bind(batch_size)
        },
    )
    .await?;

    let longest_cutoff = longest_cutoff(pool, default_days, now).await?;

    report.dedupe_deleted = delete_in_batches(
        pool,
        "DELETE FROM event_dedupe d USING ( \
            SELECT workspace_id, message_id FROM event_dedupe \
            WHERE event_occurred_at < $1 LIMIT $2) old \
         WHERE d.workspace_id = old.workspace_id AND d.message_id = old.message_id",
        batch_size,
        |query| query.bind(longest_cutoff).bind(batch_size),
    )
    .await?;

    report.test_sends_deleted =
        crate::delete_test_sends_before(pool, now, windows.test_send_months).await?;

    drop_old_partitions(pool, default_days, now, &mut report).await?;
    Ok(report)
}

/// [`delete_in_batches`] for a statement built at run time that returns the number of rows it
/// deleted as one `bigint` (the event deletes, which write markers).
async fn delete_events_in_batches<'q>(
    pool: &PgPool,
    statement: &str,
    batch_size: i64,
    bind: impl Fn(
        sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted: i64 = bind(sqlx::query_scalar(sqlx::AssertSqlSafe(
            statement.to_owned(),
        )))
        .fetch_one(pool)
        .await
        .map_err(StoreError::query)?;
        total += u64::try_from(deleted).unwrap_or(0);
        if deleted < batch_size {
            return Ok(total);
        }
    }
}

async fn delete_in_batches<'q>(
    pool: &PgPool,
    statement: &'static str,
    batch_size: i64,
    bind: impl Fn(
        sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted = bind(sqlx::query(statement))
            .execute(pool)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
        total += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < batch_size {
            return Ok(total);
        }
    }
}

/// The instant before which no event is kept under any rule: `now` less the longest of the
/// default and every override, read as it is now.
async fn longest_cutoff<'e>(
    executor: impl PgExecutor<'e>,
    default_days: i32,
    now: OffsetDateTime,
) -> Result<OffsetDateTime, StoreError> {
    let overrides: Vec<i32> = sqlx::query_scalar(
        "SELECT retention_days FROM event_schemas WHERE retention_days IS NOT NULL",
    )
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let longest = overrides.into_iter().fold(default_days, i32::max);
    Ok(now - Duration::days(i64::from(longest)))
}

/// Takes the lock that serialises an override change with a partition drop, until the
/// transaction ends. A row lock cannot do it: it neither covers a NULL that becomes a number
/// nor a schema that is inserted.
pub(crate) async fn lock_retention_overrides(
    connection: &mut PgConnection,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('event_retention_overrides', 0))")
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Drops the monthly partitions whose whole range ends at or before the longest retention
/// cutoff. The DEFAULT partition is never dropped. Each drop commits with its markers in its
/// own transaction. The cutoff is read under `lock_retention_overrides`, which every override
/// change takes first, so a change waits for the current drop and an earlier one is seen. A drop
/// that cannot get its lock within the drop lock timeout is counted as skipped and left for the
/// next run.
async fn drop_old_partitions(
    pool: &PgPool,
    default_days: i32,
    now: OffsetDateTime,
    report: &mut RetentionReport,
) -> Result<(), StoreError> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid 
         WHERE i.inhparent = 'events'::regclass",
    )
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    for name in names {
        let Some(first) = month_of(&name) else {
            continue;
        };
        let mut tx = crate::begin(pool).await?;
        lock_retention_overrides(&mut tx).await?;
        let cutoff = longest_cutoff(&mut *tx, default_days, now).await?;
        if next_month(first).midnight().assume_utc() <= cutoff {
            // The name was parsed from digits only. The sync events of the partition leave their
            // markers first, in the same transaction as the drop.
            sqlx::query(sqlx::AssertSqlSafe(crate::privacy::mark_partition(&name)))
                .execute(&mut *tx)
                .await
                .map_err(StoreError::query)?;
            set_drop_lock_timeout(&mut tx).await?;
            match sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {name}")))
                .execute(&mut *tx)
                .await
            {
                Ok(_) => report.partitions_dropped += 1,
                Err(error) if is_lock_timeout(&error) => {
                    report.skipped_partitions.push(name);
                    continue;
                }
                Err(error) => return Err(StoreError::query(error)),
            }
        }
        crate::commit(tx).await?;
    }
    Ok(())
}

/// The first day of the month a partition named `events_yYYYYmMM` covers.
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
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceId, WorkspaceName,
    };

    use super::*;
    use crate::events::{
        EventOutcome, EventRecord, EventSchemaUpdate, apply_event_batch, update_event_schema,
    };
    use crate::{begin, commit, create_workspace};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    fn settings() -> EventRetention {
        EventRetention {
            default_days: 100,
            min_days: 7,
            max_days: 1000,
            reject_days: 30,
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn store(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        age_days: i64,
        message_id: Option<&str>,
    ) {
        let record = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: name.into(),
            properties: json!({}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: NOW - Duration::days(age_days),
            message_id: message_id.map(str::to_owned),
            trace: TraceContext::root(Cause::api("t")),
        };
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let outcome = apply_event_batch(pool, ws, &key, &[record])
            .await
            .unwrap()
            .remove(0);
        assert!(
            matches!(outcome, EventOutcome::Accepted { .. }),
            "{outcome:?}"
        );
    }

    async fn remaining(pool: &PgPool, name: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn run(pool: &PgPool) -> RetentionReport {
        apply_event_retention(pool, &settings(), &Default::default(), NOW)
            .await
            .unwrap()
    }

    async fn sync_events(pool: &PgPool) -> Uuid {
        let ws = workspace(pool, "batches").await;
        let destination = Uuid::now_v7();
        crate::create_destination(
            pool,
            ws,
            &crate::NewDestination {
                id: destination,
                kind: "fake",
                name: "sink",
                config: &json!({}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        for index in 0..2 {
            store(pool, ws, "old", 200, Some(&format!("m-{index}"))).await;
        }
        sqlx::query(
            "UPDATE events SET name = $1, properties = jsonb_build_object('destination_id', $2::text)",
        )
        .bind(zirv_flow_core::events::SYNC_SUCCEEDED_EVENT)
        .bind(destination)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query_scalar("SELECT person_id FROM events LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn run_small_batches(pool: &PgPool) -> Result<RetentionReport, StoreError> {
        apply_event_retention_in_batches(pool, &settings(), &Default::default(), NOW, 1).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_earlier_batch_releases_its_rows_and_markers_while_a_later_batch_waits(
        pool: PgPool,
    ) {
        let person = sync_events(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO event_schemas (workspace_id, name, retention_days)
             SELECT DISTINCT workspace_id, name, 100 FROM events;
             CREATE SEQUENCE retention_batches;
             CREATE FUNCTION pause_later_batch() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF nextval('retention_batches') > 1 THEN
                     PERFORM pg_advisory_xact_lock(289);
                 END IF;
                 RETURN OLD;
             END $$;
             CREATE TRIGGER pause_later_batch BEFORE DELETE ON events
             FOR EACH ROW EXECUTE FUNCTION pause_later_batch()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut barrier = begin(&pool).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(289)")
            .execute(&mut *barrier)
            .await
            .unwrap();
        let retention = tokio::spawn({
            let pool = pool.clone();
            async move { run_small_batches(&pool).await.unwrap() }
        });
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory'
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                 AND classid = 0 AND objid = 289 AND NOT granted)",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            assert!(
                !retention.is_finished(),
                "retention never reached the later batch"
            );
            tokio::task::yield_now().await;
        }

        let state: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM events), (SELECT count(*) FROM erasure_handled_markers)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            state,
            (1, 1),
            "the first delete and marker committed together"
        );
        let mut writer = begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(person)
            .fetch_one(&mut *writer)
            .await
            .unwrap();
        sqlx::query("DELETE FROM erasure_handled_markers WHERE person_id = $1")
            .bind(person)
            .execute(&mut *writer)
            .await
            .unwrap();
        commit(writer).await.unwrap();
        assert!(!retention.is_finished());
        commit(barrier).await.unwrap();

        assert_eq!(retention.await.unwrap().events_deleted, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_later_failure_keeps_committed_deletes_and_markers_and_a_rerun_continues(
        pool: PgPool,
    ) {
        sync_events(&pool).await;
        sqlx::raw_sql(
            "CREATE FUNCTION fail_reject_retention() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RAISE EXCEPTION 'later retention step failed'; END $$;
             CREATE TRIGGER fail_reject_retention BEFORE DELETE ON ingest_rejects
             FOR EACH STATEMENT EXECUTE FUNCTION fail_reject_retention()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = run_small_batches(&pool).await.unwrap_err();
        assert!(format!("{error:?}").contains("later retention step failed"));
        let state: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM events), (SELECT count(*) FROM erasure_handled_markers)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, (0, 1), "earlier batches survive the failed run");
        sqlx::query("DROP TRIGGER fail_reject_retention ON ingest_rejects")
            .execute(&pool)
            .await
            .unwrap();

        let report = run_small_batches(&pool).await.unwrap();
        assert_eq!((report.events_deleted, report.dedupe_deleted), (0, 2));
        assert_eq!(
            run_small_batches(&pool).await.unwrap(),
            RetentionReport::default()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reject_batches_commit_before_a_later_batch_fails(pool: PgPool) {
        sync_events(&pool).await;
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, reason, trace_id)
             SELECT gen_random_uuid(), workspace_id, occurred_at, 'web', 'event',
                    'validation_failed', gen_random_uuid() FROM events",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_first_batch_commits(&pool, "ingest_rejects").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dedupe_batches_commit_before_a_later_batch_fails(pool: PgPool) {
        sync_events(&pool).await;
        assert_first_batch_commits(&pool, "event_dedupe").await;
    }

    async fn assert_first_batch_commits(pool: &PgPool, table: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE SEQUENCE retention_batches;
             CREATE FUNCTION fail_later_batch() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF nextval('retention_batches') > 1 THEN
                     RAISE EXCEPTION 'later batch failed';
                 END IF;
                 RETURN NULL;
             END $$;
             CREATE TRIGGER fail_later_batch BEFORE DELETE ON {table}
             FOR EACH STATEMENT EXECUTE FUNCTION fail_later_batch()"
        )))
        .execute(pool)
        .await
        .unwrap();

        let error = run_small_batches(pool).await.unwrap_err();
        assert!(format!("{error:?}").contains("later batch failed"));
        let remaining: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(remaining, 1, "the first {table} batch committed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_partition_drop_commits_with_its_markers_even_if_a_later_drop_fails(pool: PgPool) {
        sync_events(&pool).await;
        sqlx::raw_sql(
            "CREATE TABLE events_y2020m01 PARTITION OF events
             FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-02-01 00:00:00+00');
             CREATE TABLE events_y2020m02 PARTITION OF events
             FOR VALUES FROM ('2020-02-01 00:00:00+00') TO ('2020-03-01 00:00:00+00');
             UPDATE events SET occurred_at = CASE
                 WHEN id = (SELECT min(id::text)::uuid FROM events) THEN '2020-01-15'::timestamptz
                 ELSE '2020-02-15'::timestamptz END;
             INSERT INTO destinations (id, workspace_id, kind, name, config)
             SELECT gen_random_uuid(), workspace_id, kind, 'second sink', config FROM destinations;
             UPDATE events SET properties = jsonb_build_object('destination_id',
                 (SELECT id::text FROM destinations WHERE name = 'second sink'))
             WHERE occurred_at = '2020-02-15'::timestamptz;
             CREATE SEQUENCE retention_drops;
             CREATE FUNCTION fail_later_drop() RETURNS event_trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF nextval('retention_drops') > 1 THEN
                     RAISE EXCEPTION 'later partition drop failed';
                 END IF;
             END $$;
             CREATE EVENT TRIGGER fail_later_drop ON ddl_command_start
             WHEN TAG IN ('DROP TABLE') EXECUTE FUNCTION fail_later_drop()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = drop_old_partitions(
            &pool,
            100,
            datetime!(2020-07-01 00:00 UTC),
            &mut RetentionReport::default(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:?}").contains("later partition drop failed"));
        let partitions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_class WHERE relname IN ('events_y2020m01', 'events_y2020m02')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(partitions, 1, "the first partition drop survived");
        let markers: i64 = sqlx::query_scalar("SELECT count(*) FROM erasure_handled_markers")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(markers, 1, "the failed drop rolls back its own marker");
        let overlap: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM erasure_handled_markers m JOIN events e
             ON e.properties ->> 'destination_id' = m.destination_id::text",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(overlap, 0, "only the dropped partition left a marker");
    }

    /// What one drop pass did.
    async fn drop_pass(pool: &PgPool, default_days: i32, now: OffsetDateTime) -> RetentionReport {
        let mut report = RetentionReport::default();
        drop_old_partitions(pool, default_days, now, &mut report)
            .await
            .unwrap();
        report
    }

    /// A pass at a time when only the two 2020 partitions of `two_old_partitions` are past it.
    async fn drop_pass_2020(pool: &PgPool) -> RetentionReport {
        drop_pass(pool, 100, datetime!(2020-07-01 00:00 UTC)).await
    }

    async fn two_old_partitions(pool: &PgPool) {
        sqlx::raw_sql(
            "CREATE TABLE events_y2020m01 PARTITION OF events
             FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-02-01 00:00:00+00');
             CREATE TABLE events_y2020m02 PARTITION OF events
             FOR VALUES FROM ('2020-02-01 00:00:00+00') TO ('2020-03-01 00:00:00+00')",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn exists(pool: &PgPool, name: &str) -> bool {
        sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_held_lock_on_events_skips_the_drops_within_the_timeout_and_a_rerun_drops_them(
        pool: PgPool,
    ) {
        two_old_partitions(&pool).await;
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE events IN ACCESS SHARE MODE")
            .execute(&mut *holder)
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let report = drop_pass_2020(&pool).await;

        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(report.partitions_dropped, 0);
        let mut skipped = report.skipped_partitions.clone();
        skipped.sort();
        assert_eq!(skipped, ["events_y2020m01", "events_y2020m02"]);
        assert!(exists(&pool, "events_y2020m01").await);
        holder.rollback().await.unwrap();

        let again = drop_pass_2020(&pool).await;

        assert_eq!(again.partitions_dropped, 2);
        assert!(again.skipped_partitions.is_empty());
        assert!(!exists(&pool, "events_y2020m01").await);
        assert!(!exists(&pool, "events_y2020m02").await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_partition_whose_drop_times_out_is_skipped_and_the_run_drops_the_other(pool: PgPool) {
        two_old_partitions(&pool).await;
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE events_y2020m01 IN ACCESS SHARE MODE")
            .execute(&mut *holder)
            .await
            .unwrap();

        let report = drop_pass_2020(&pool).await;

        assert_eq!(report.partitions_dropped, 1);
        assert_eq!(report.skipped_partitions, ["events_y2020m01"]);
        assert!(exists(&pool, "events_y2020m01").await);
        assert!(!exists(&pool, "events_y2020m02").await);
        holder.rollback().await.unwrap();

        let again = drop_pass_2020(&pool).await;

        assert_eq!(again.partitions_dropped, 1);
        assert!(again.skipped_partitions.is_empty());
        assert!(!exists(&pool, "events_y2020m01").await);
    }

    #[test]
    fn partition_names_map_to_their_month_and_nothing_else_does() {
        assert_eq!(
            month_of("events_y2026m06"),
            Some(time::macros::date!(2026 - 06 - 01))
        );
        for other in [
            "events_default",
            "events_y2026m13",
            "events_y26m06",
            "events_y2026m6",
            "decision_records_y2026m06",
        ] {
            assert_eq!(month_of(other), None, "{other}");
        }
    }

    #[test]
    fn the_defaults_are_25_months_with_30_to_1095_day_overrides() {
        let defaults = EventRetention::default();

        assert_eq!(
            (
                defaults.default_days,
                defaults.min_days,
                defaults.max_days,
                defaults.reject_days
            ),
            (760, 30, 1095, 30)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn events_go_by_the_default_or_their_names_override(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        for name in ["short", "long", "plain"] {
            for age in [5, 50, 200] {
                store(&pool, ws, name, age, None).await;
            }
        }
        for (name, days) in [("short", 10), ("long", 500)] {
            update_event_schema(
                &mut pool.acquire().await.unwrap(),
                ws,
                name,
                &EventSchemaUpdate {
                    retention_days: Some(Some(days)),
                    mode: Some(EventMode::Warn),
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        }

        let report = run(&pool).await;

        assert_eq!(report.events_deleted, 3);
        assert_eq!(remaining(&pool, "short").await, 1);
        assert_eq!(remaining(&pool, "long").await, 3);
        assert_eq!(remaining(&pool, "plain").await, 2);
        assert_eq!(run(&pool).await, RetentionReport::default());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_override_longer_than_the_default_is_enforced_at_its_own_age(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        // 900 days old lies before the first partition the migration made: DEFAULT.
        for age in [5, 790, 900] {
            store(&pool, ws, "kept_long", age, None).await;
        }
        store(&pool, ws, "plain", 790, None).await;
        update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "kept_long",
            &EventSchemaUpdate {
                retention_days: Some(Some(800)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let in_default: i64 = sqlx::query_scalar("SELECT count(*) FROM events_default")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(in_default >= 1);
        let settings = EventRetention {
            default_days: 760,
            ..settings()
        };

        let report = apply_event_retention(&pool, &settings, &Default::default(), NOW)
            .await
            .unwrap();

        assert_eq!(report.events_deleted, 2);
        assert_eq!(remaining(&pool, "kept_long").await, 2);
        assert_eq!(remaining(&pool, "plain").await, 0);
        let ages: Vec<i64> = sqlx::query_scalar(
            "SELECT (extract(epoch FROM $1::timestamptz - occurred_at) / 86400)::bigint \
             FROM events WHERE name = 'kept_long' ORDER BY 1",
        )
        .bind(NOW)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ages, [5, 790]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_batch_reads_the_override_as_it_is_when_the_batch_runs(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        for index in 0..3 {
            store(&pool, ws, "tuned", 50, Some(&format!("m-{index}"))).await;
        }
        let set = |days: Option<i32>| {
            let pool = pool.clone();
            async move {
                let update = EventSchemaUpdate {
                    retention_days: Some(days),
                    ..Default::default()
                };
                update_event_schema(&mut pool.acquire().await.unwrap(), ws, "tuned", &update).await
            }
        };
        let batch = |size: i64| {
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(overridden_delete()))
                .bind(NOW)
                .bind(size)
                .fetch_one(&pool)
        };
        set(Some(30)).await.unwrap();

        let first = batch(1).await.unwrap();
        set(Some(90)).await.unwrap();
        let after_raise = batch(5000).await.unwrap();
        set(Some(30)).await.unwrap();
        let after_lowering = batch(5000).await.unwrap();

        assert_eq!((first, after_raise, after_lowering), (1, 0, 2));
        assert_eq!(remaining(&pool, "tuned").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_old_partition_is_dropped_whole_and_the_default_partition_stays(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        sqlx::query(
            "CREATE TABLE events_y2020m01 PARTITION OF events \
             FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-02-01 00:00:00+00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let old = (NOW - datetime!(2020-01-15 00:00 UTC)).whole_days();
        store(&pool, ws, "ancient", old, None).await;
        store(&pool, ws, "recent", 1, None).await;
        let in_partition: i64 = sqlx::query_scalar("SELECT count(*) FROM events_y2020m01")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(in_partition, 1);

        let report = run(&pool).await;

        assert!(report.partitions_dropped >= 1);
        assert_eq!(remaining(&pool, "ancient").await, 0);
        assert_eq!(remaining(&pool, "recent").await, 1);
        let gone: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('events_y2020m01')::text")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(gone.is_none());
        let default: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('events_default')::text")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(default.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dropped_partition_leaves_the_markers_of_its_sync_events(pool: PgPool) {
        let seeded = crate::privacy::testing::seed_everything(&pool).await;
        let destination = Uuid::now_v7();
        crate::create_destination(
            &pool,
            seeded.ws,
            &crate::NewDestination {
                id: destination,
                kind: "fake",
                name: "partitioned",
                config: &json!({}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE events_y2020m01 PARTITION OF events              FOR VALUES FROM ('2020-01-01 00:00:00+00') TO ('2020-02-01 00:00:00+00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at,                 properties, source, trace_id, validation_state)              VALUES (gen_random_uuid(), $1, $2, $3, '2020-01-15 00:00:00+00', now(),                 jsonb_build_object('destination_id', $4::text), 'flows', gen_random_uuid(),                 'valid')",
        )
        .bind(seeded.ws.as_uuid())
        .bind(seeded.person)
        .bind(zirv_flow_core::events::SYNC_SUCCEEDED_EVENT)
        .bind(destination)
        .execute(&pool)
        .await
        .unwrap();

        // The event deletes of a run would take the event first, so the drop is called alone.
        let dropped = drop_pass(&pool, 100, NOW).await.partitions_dropped;

        assert!(dropped >= 1);
        let table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('events_y2020m01')::text")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(table.is_none(), "the partition is gone");
        let markers: Vec<Uuid> = sqlx::query_scalar(
            "SELECT person_id FROM erasure_handled_markers WHERE destination_id = $1",
        )
        .bind(destination)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(markers, [seeded.person]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_longer_override_keeps_an_old_partition(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events_y2025m12 PARTITION OF events \
             FOR VALUES FROM ('2025-12-01 00:00:00+00') TO ('2026-01-01 00:00:00+00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        store(&pool, ws, "keeper", 170, None).await;
        update_event_schema(
            &mut pool.acquire().await.unwrap(),
            ws,
            "keeper",
            &EventSchemaUpdate {
                retention_days: Some(Some(900)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let report = run(&pool).await;

        assert_eq!((report.events_deleted, report.partitions_dropped), (0, 0));
        assert_eq!(remaining(&pool, "keeper").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drop_waits_for_an_override_set_on_a_schema_without_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS events_y2025m12 PARTITION OF events \
             FOR VALUES FROM ('2025-12-01 00:00:00+00') TO ('2026-01-01 00:00:00+00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The schema has no override, so the default of 100 days alone would drop the month.
        store(&pool, ws, "keeper", 170, None).await;
        let mut writer = begin(&pool).await.unwrap();
        update_event_schema(
            &mut writer,
            ws,
            "keeper",
            &EventSchemaUpdate {
                retention_days: Some(Some(900)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let drop = tokio::spawn({
            let pool = pool.clone();
            async move { drop_pass(&pool, 100, NOW).await.partitions_dropped }
        });
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory'
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                 AND NOT granted)",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            assert!(
                !drop.is_finished(),
                "the drop did not wait for the override change"
            );
            tokio::task::yield_now().await;
        }

        commit(writer).await.unwrap();

        assert_eq!(drop.await.unwrap(), 0);
        assert_eq!(remaining(&pool, "keeper").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn old_rejects_and_dedupe_records_go(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        store(&pool, ws, "old", 200, Some("old-message")).await;
        store(&pool, ws, "recent", 50, Some("recent-message")).await;
        for (age, id) in [(40, "old-reject"), (3, "new-reject")] {
            sqlx::query(
                "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, reason, trace_id) \
                 VALUES ($1, $2, $3, 'web', 'event', 'validation_failed', $4)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(NOW - Duration::days(age))
            .bind(Uuid::now_v7())
            .execute(&pool)
            .await
            .unwrap();
            let _ = id;
        }

        let report = run(&pool).await;

        assert_eq!((report.rejects_deleted, report.dedupe_deleted), (1, 1));
        let messages: Vec<String> =
            sqlx::query_scalar("SELECT message_id FROM event_dedupe ORDER BY 1")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(messages, ["recent-message"]);
        let rejects: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rejects, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn test_sends_older_than_13_months_go_and_newer_ones_stay(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        for age in [10, 300, 420, 500] {
            sqlx::query(
                "INSERT INTO test_sends (id, workspace_id, template_id, destination_id, \
                    sender_identity_id, recipient_hash, render_context, created_at) \
                 VALUES ($1, $2, $1, $1, $1, $3, 'sample', $4)",
            )
            .bind(Uuid::now_v7())
            .bind(ws.as_uuid())
            .bind(vec![0_u8; 32])
            .bind(NOW - Duration::days(age))
            .execute(&pool)
            .await
            .unwrap();
        }

        let report = run(&pool).await;

        assert_eq!(report.test_sends_deleted, 2);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM test_sends")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 2);
    }
}
