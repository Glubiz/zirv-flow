//! Event retention (architecture section 16.3): per-name deletes for names with an override,
//! deletes at the deployment default for the others, whole old partitions, old rejects and the
//! dedupe records of events that no longer exist. Every step is batched and idempotent.

use sqlx::PgConnection;
use time::{Date, Duration, Month, OffsetDateTime};

use super::partitions::PARTITION_PREFIX;
use crate::StoreError;
use crate::decisions::next_month;

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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionReport {
    pub events_deleted: u64,
    pub partitions_dropped: u64,
    pub rejects_deleted: u64,
    pub dedupe_deleted: u64,
    /// Test-send log rows past their 13 months (architecture 16.3).
    pub test_sends_deleted: u64,
}

/// Applies the retention rules as of `now`. Run it in the job's transaction: a rerun after
/// a failure finds nothing left to do for the steps that completed.
pub async fn apply_event_retention(
    connection: &mut PgConnection,
    settings: &EventRetention,
    now: OffsetDateTime,
) -> Result<RetentionReport, StoreError> {
    let default_days = i32::try_from(settings.default_days).unwrap_or(i32::MAX);
    let mut report = RetentionReport::default();

    report.events_deleted += delete_events_in_batches(connection, &overridden_delete(), |query| {
        query.bind(now).bind(RETENTION_BATCH)
    })
    .await?;

    let default_cutoff = now - Duration::days(i64::from(settings.default_days));
    report.events_deleted += delete_events_in_batches(connection, &default_delete(), |query| {
        query.bind(default_cutoff).bind(RETENTION_BATCH)
    })
    .await?;

    report.rejects_deleted = delete_in_batches(
        connection,
        "DELETE FROM ingest_rejects WHERE id IN ( \
            SELECT id FROM ingest_rejects WHERE received_at < $1 LIMIT $2)",
        |query| {
            query
                .bind(now - Duration::days(i64::from(settings.reject_days)))
                .bind(RETENTION_BATCH)
        },
    )
    .await?;

    let longest_cutoff = longest_cutoff(connection, default_days, now).await?;

    report.dedupe_deleted = delete_in_batches(
        connection,
        "DELETE FROM event_dedupe d USING ( \
            SELECT workspace_id, message_id FROM event_dedupe \
            WHERE event_occurred_at < $1 LIMIT $2) old \
         WHERE d.workspace_id = old.workspace_id AND d.message_id = old.message_id",
        |query| query.bind(longest_cutoff).bind(RETENTION_BATCH),
    )
    .await?;

    report.test_sends_deleted = crate::delete_test_sends_before(connection, now).await?;

    // Last, so the lock a drop takes is held briefly.
    report.partitions_dropped = drop_old_partitions(connection, default_days, now).await?;
    Ok(report)
}

/// [`delete_in_batches`] for a statement built at run time that returns the number of rows it
/// deleted as one `bigint` (the event deletes, which write markers).
async fn delete_events_in_batches<'q>(
    connection: &mut PgConnection,
    statement: &str,
    bind: impl Fn(
        sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted: i64 = bind(sqlx::query_scalar(sqlx::AssertSqlSafe(
            statement.to_owned(),
        )))
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        total += u64::try_from(deleted).unwrap_or(0);
        if deleted < RETENTION_BATCH {
            return Ok(total);
        }
    }
}

async fn delete_in_batches<'q>(
    connection: &mut PgConnection,
    statement: &'static str,
    bind: impl Fn(
        sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted = bind(sqlx::query(statement))
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
        total += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < RETENTION_BATCH {
            return Ok(total);
        }
    }
}

/// The instant before which no event is kept under any rule: `now` less the longest of the
/// default and every override, read as it is now.
async fn longest_cutoff(
    connection: &mut PgConnection,
    default_days: i32,
    now: OffsetDateTime,
) -> Result<OffsetDateTime, StoreError> {
    let overrides: Vec<i32> = sqlx::query_scalar(
        "SELECT retention_days FROM event_schemas WHERE retention_days IS NOT NULL",
    )
    .fetch_all(&mut *connection)
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
/// cutoff. The DEFAULT partition is never dropped. Run it inside a transaction: the overrides
/// are read under `lock_retention_overrides`, which every change to one takes first, so a
/// change waits for the drops and an earlier one is seen.
async fn drop_old_partitions(
    connection: &mut PgConnection,
    default_days: i32,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    lock_retention_overrides(connection).await?;
    let cutoff = longest_cutoff(connection, default_days, now).await?;
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid 
         WHERE i.inhparent = 'events'::regclass",
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut dropped = 0;
    for name in names {
        let Some(first) = month_of(&name) else {
            continue;
        };
        if next_month(first).midnight().assume_utc() <= cutoff {
            // The name was parsed from digits only. The sync events of the partition leave their
            // markers first, in the same transaction as the drop.
            sqlx::query(sqlx::AssertSqlSafe(crate::privacy::mark_partition(&name)))
                .execute(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE {name}")))
                .execute(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            dropped += 1;
        }
    }
    Ok(dropped)
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
        let mut connection = pool.acquire().await.unwrap();
        apply_event_retention(&mut connection, &settings(), NOW)
            .await
            .unwrap()
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

        let mut connection = pool.acquire().await.unwrap();
        let report = apply_event_retention(&mut connection, &settings, NOW)
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
            async move {
                let mut tx = begin(&pool).await.unwrap();
                let dropped = drop_old_partitions(&mut tx, 100, NOW).await.unwrap();
                commit(tx).await.unwrap();
                dropped
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !drop.is_finished(),
            "the drop did not wait for the override change"
        );

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
