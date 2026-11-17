//! The record of each delivery attempt, and the per-destination report the leader publishes as
//! metrics.
//!
//! An attempt names the destination, the item delivered (a message, a forwarding item or a sync
//! item, by id), the attempt number, how it ended, the class of the answer's HTTP status and how
//! long it took. It holds no person id and no address, so deleting a person leaves it as it is.

use sqlx::prelude::FromRow;
use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// What kind of item an attempt delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubjectKind {
    Message,
    Forwarding,
    Sync,
}

impl SubjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Forwarding => "forwarding",
            Self::Sync => "sync",
        }
    }
}

/// How an attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptOutcome {
    Delivered,
    /// Failed; a later attempt may succeed.
    Retryable,
    /// Failed; no retry can succeed.
    Terminal,
    /// The destination asked for a slower rate.
    Throttled,
}

impl AttemptOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Retryable => "retryable",
            Self::Terminal => "terminal",
            Self::Throttled => "throttled",
        }
    }
}

/// The class of the HTTP status an attempt got back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusClass {
    /// The call produced no answer.
    None,
    Success,
    Redirect,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Success => "2xx",
            Self::Redirect => "3xx",
            Self::ClientError => "4xx",
            Self::ServerError => "5xx",
        }
    }

    /// The class of `status`; any status outside 200 to 599 is `None`.
    pub const fn of(status: Option<u16>) -> Self {
        match status {
            Some(200..=299) => Self::Success,
            Some(300..=399) => Self::Redirect,
            Some(400..=499) => Self::ClientError,
            Some(500..=599) => Self::ServerError,
            _ => Self::None,
        }
    }
}

/// An attempt to record.
#[derive(Clone, Debug)]
pub struct NewDeliveryAttempt {
    pub destination_id: Uuid,
    pub subject_kind: SubjectKind,
    pub subject_id: Uuid,
    /// 1 for the first attempt at this item.
    pub attempt: i32,
    pub outcome: AttemptOutcome,
    pub status_class: StatusClass,
    pub duration_ms: i32,
}

/// A recorded attempt. No person and no address.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DeliveryAttempt {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub attempt: i32,
    pub outcome: String,
    pub status_class: String,
    pub duration_ms: i32,
    pub attempted_at: OffsetDateTime,
}

/// Records one attempt. The same item, destination and attempt number twice is a conflict.
///
/// This does not count the attempt in the metrics: the caller may still roll back or retry the
/// transaction. After the transaction that holds this row commits, call
/// [`count_delivery_attempt`] once.
pub async fn record_delivery_attempt<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewDeliveryAttempt,
) -> Result<DeliveryAttempt, StoreError> {
    sqlx::query_as(
        "INSERT INTO delivery_attempts (id, workspace_id, destination_id, subject_kind, \
            subject_id, attempt, outcome, status_class, duration_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         RETURNING id, destination_id, subject_kind, subject_id, attempt, outcome, status_class, \
            duration_ms, attempted_at",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(new.destination_id)
    .bind(new.subject_kind.as_str())
    .bind(new.subject_id)
    .bind(new.attempt)
    .bind(new.outcome.as_str())
    .bind(new.status_class.as_str())
    .bind(new.duration_ms)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// Counts a recorded attempt in this process:
/// `zirv_flow_destination_attempts_total{workspace,destination}` and
/// `zirv_flow_destination_outcomes_total{workspace,destination,outcome}`. Ids only, never a name,
/// a host or an address.
///
/// Call it once per attempt, **after** the transaction that holds the row from
/// [`record_delivery_attempt`] has committed, so a rolled-back insert is never counted and a
/// committed retry of the transaction is counted once. The send path (#220) calls it; the leader
/// never scans the table.
pub fn count_delivery_attempt(workspace: WorkspaceId, attempt: &NewDeliveryAttempt) {
    let workspace = workspace.as_uuid().to_string();
    let destination = attempt.destination_id.to_string();
    metrics::counter!(
        "zirv_flow_destination_attempts_total",
        "workspace" => workspace.clone(),
        "destination" => destination.clone(),
    )
    .increment(1);
    metrics::counter!(
        "zirv_flow_destination_outcomes_total",
        "workspace" => workspace,
        "destination" => destination,
        "outcome" => attempt.outcome.as_str(),
    )
    .increment(1);
}

/// Counts an attempt that was made but could not be recorded:
/// `zirv_flow_delivery_attempt_record_failures_total{workspace,destination}`. Ids only. The send
/// is not repeated for it, so this counter is the only trace of the missing row.
pub fn count_delivery_attempt_record_failure(workspace: WorkspaceId, destination: Uuid) {
    metrics::counter!(
        "zirv_flow_delivery_attempt_record_failures_total",
        "workspace" => workspace.as_uuid().to_string(),
        "destination" => destination.to_string(),
    )
    .increment(1);
}

/// The newest attempts of a destination, newest first, at most `limit`.
pub async fn list_delivery_attempts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    destination: Uuid,
    limit: i64,
) -> Result<Vec<DeliveryAttempt>, StoreError> {
    sqlx::query_as(
        "SELECT id, destination_id, subject_kind, subject_id, attempt, outcome, status_class, \
            duration_ms, attempted_at \
         FROM delivery_attempts WHERE workspace_id = $1 AND destination_id = $2 \
         ORDER BY attempted_at DESC, id DESC LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// How many attempts of one destination ended each way, and with which status class, over a
/// window. Counts only: no item id, no person.
#[derive(Clone, Copy, Debug, Default, Eq, FromRow, PartialEq)]
pub struct AttemptStats {
    pub attempts: i64,
    pub delivered: i64,
    pub retryable: i64,
    pub terminal: i64,
    pub throttled: i64,
    pub class_none: i64,
    pub class_2xx: i64,
    pub class_3xx: i64,
    pub class_4xx: i64,
    pub class_5xx: i64,
}

/// The statement of [`destination_attempt_stats`]; a test explains it.
const STATS_SQL: &str = "SELECT count(*) AS attempts, \
            count(*) FILTER (WHERE outcome = 'delivered') AS delivered, \
            count(*) FILTER (WHERE outcome = 'retryable') AS retryable, \
            count(*) FILTER (WHERE outcome = 'terminal') AS terminal, \
            count(*) FILTER (WHERE outcome = 'throttled') AS throttled, \
            count(*) FILTER (WHERE status_class = 'none') AS class_none, \
            count(*) FILTER (WHERE status_class = '2xx') AS class_2xx, \
            count(*) FILTER (WHERE status_class = '3xx') AS class_3xx, \
            count(*) FILTER (WHERE status_class = '4xx') AS class_4xx, \
            count(*) FILTER (WHERE status_class = '5xx') AS class_5xx \
         FROM delivery_attempts \
         WHERE workspace_id = $1 AND destination_id = $2 \
            AND attempted_at >= now() - make_interval(hours => $3)";

/// The attempts of a destination made in the last `hours` hours, counted by outcome and by status
/// class. Reads through `delivery_attempts_destination_idx`, which carries both columns
/// (`INCLUDE`), so on a vacuumed table it is an index-only scan that never visits the heap.
pub async fn destination_attempt_stats<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    destination: Uuid,
    hours: i32,
) -> Result<AttemptStats, StoreError> {
    sqlx::query_as(STATS_SQL)
        .bind(workspace.as_uuid())
        .bind(destination)
        .bind(hours)
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

/// Deletes the attempts recorded before `cutoff`, oldest first, `batch` rows to a statement that
/// commits alone, until a batch comes back short. Returns the number of rows deleted.
///
/// An attempt names no person (see the module doc), so erasure leaves it alone and one global
/// cutoff serves every workspace; the retention job passes the longest message window. Reads
/// through `delivery_attempts_attempted_at_idx`.
pub async fn purge_attempts(
    pool: &PgPool,
    cutoff: OffsetDateTime,
    batch: i64,
) -> Result<u64, StoreError> {
    let mut total = 0_u64;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM delivery_attempts WHERE ctid IN ( \
                SELECT ctid FROM delivery_attempts WHERE attempted_at < $1 \
                ORDER BY attempted_at LIMIT $2)",
        )
        .bind(cutoff)
        .bind(batch)
        .execute(pool)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        total += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < batch {
            return Ok(total);
        }
    }
}

/// What the leader publishes for one live destination: its queue gauges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DestinationStat {
    pub workspace_id: Uuid,
    pub destination_id: Uuid,
    /// Items waiting for delivery: the destination's `delivery.send` jobs that are ready, a
    /// deferred one (waiting for a rate-limit token) included. Saturates at [`QUEUE_DEPTH_CAP`].
    pub queue_depth: i64,
    /// Age in seconds of the oldest waiting item, counted from the moment it became runnable;
    /// 0 when none waits or the oldest is not yet due.
    pub oldest_age_seconds: i64,
}

#[derive(FromRow)]
struct StatRow {
    workspace_id: Uuid,
    destination_id: Uuid,
    queue_depth: i64,
    oldest_age_seconds: i64,
}

/// The most ready sends the queue-depth gauge counts for one destination. A deeper queue reads
/// as this many, so the leader's read costs O(destinations x cap), not O(queued sends).
pub const QUEUE_DEPTH_CAP: i64 = 50_000;

/// One row per live destination with its queue gauges. The attempt and outcome counters are not
/// here: they are counted in-process where an attempt is recorded, so this never scans
/// `delivery_attempts`.
///
/// The gauges read the send queue: the ready `delivery.send` jobs by the destination in their
/// payload, through the partial index `jobs_delivery_send_idx`, one probe per destination. The
/// depth saturates at [`QUEUE_DEPTH_CAP`]; the oldest age is exact.
pub async fn destination_report<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<Vec<DestinationStat>, StoreError> {
    destination_report_capped(executor, QUEUE_DEPTH_CAP).await
}

async fn destination_report_capped<'e>(
    executor: impl PgExecutor<'e>,
    cap: i64,
) -> Result<Vec<DestinationStat>, StoreError> {
    let rows: Vec<StatRow> = sqlx::query_as(
        "SELECT d.workspace_id, d.id AS destination_id, \
                (SELECT count(*) FROM (SELECT 1 FROM jobs j \
                    WHERE j.kind = 'delivery.send' AND j.state = 'ready' \
                      AND j.payload ->> 'destination_id' = d.id::text LIMIT $1) c) AS queue_depth, \
                COALESCE(GREATEST(0, EXTRACT(EPOCH FROM (clock_timestamp() - \
                    (SELECT j.run_at FROM jobs j \
                      WHERE j.kind = 'delivery.send' AND j.state = 'ready' \
                        AND j.payload ->> 'destination_id' = d.id::text \
                      ORDER BY j.run_at LIMIT 1)))), 0)::bigint AS oldest_age_seconds \
         FROM destinations d WHERE d.archived_at IS NULL ORDER BY d.id",
    )
    .bind(cap)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| DestinationStat {
            workspace_id: row.workspace_id,
            destination_id: row.destination_id,
            queue_depth: row.queue_depth,
            oldest_age_seconds: row.oldest_age_seconds,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{NewDestination, create_destination, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    #[test]
    fn a_status_is_classified_and_every_label_is_stable() {
        assert_eq!(StatusClass::of(Some(202)), StatusClass::Success);
        assert_eq!(StatusClass::of(Some(302)), StatusClass::Redirect);
        assert_eq!(StatusClass::of(Some(429)), StatusClass::ClientError);
        assert_eq!(StatusClass::of(Some(503)), StatusClass::ServerError);
        assert_eq!(StatusClass::of(Some(99)), StatusClass::None);
        assert_eq!(StatusClass::of(None), StatusClass::None);
        assert_eq!(
            [
                StatusClass::None,
                StatusClass::Success,
                StatusClass::Redirect,
                StatusClass::ClientError,
                StatusClass::ServerError
            ]
            .map(StatusClass::as_str),
            ["none", "2xx", "3xx", "4xx", "5xx"]
        );
        assert_eq!(
            [
                AttemptOutcome::Delivered,
                AttemptOutcome::Retryable,
                AttemptOutcome::Terminal,
                AttemptOutcome::Throttled
            ]
            .map(AttemptOutcome::as_str),
            ["delivered", "retryable", "terminal", "throttled"]
        );
        assert_eq!(
            [
                SubjectKind::Message,
                SubjectKind::Forwarding,
                SubjectKind::Sync
            ]
            .map(SubjectKind::as_str),
            ["message", "forwarding", "sync"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_stats_are_an_index_only_scan_of_the_destination_index(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "hook").await;
        let other = destination(&pool, ws, "other").await;
        // The destination is a tenth of the table, so its index is the way to it.
        for (destination, rows) in [(id, 3000), (other, 27_000)] {
            sqlx::query(
                "INSERT INTO delivery_attempts (id, workspace_id, destination_id, subject_kind, \
                    subject_id, attempt, outcome, status_class, duration_ms) \
                 SELECT gen_random_uuid(), $1, $2, 'message', gen_random_uuid(), 1, \
                        'delivered', '2xx', 5 FROM generate_series(1, $3)",
            )
            .bind(ws.as_uuid())
            .bind(destination)
            .bind(rows)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("VACUUM (PARALLEL 0, ANALYZE) delivery_attempts")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query("SET enable_seqscan = off")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("SET enable_bitmapscan = off")
            .execute(&mut *connection)
            .await
            .unwrap();

        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) {}",
            STATS_SQL
                .replace("$1", &format!("'{}'", ws.as_uuid()))
                .replace("$2", &format!("'{id}'"))
                .replace("$3", "168")
        )))
        .fetch_all(&mut *connection)
        .await
        .unwrap();

        let plan = plan.join("\n");
        assert!(
            plan.contains("Index Only Scan using delivery_attempts_destination_idx"),
            "{plan}"
        );
        assert!(plan.contains("Heap Fetches: 0"), "{plan}");
        let stats = destination_attempt_stats(&mut *connection, ws, id, 168)
            .await
            .unwrap();
        assert_eq!((stats.attempts, stats.delivered), (3000, 3000));
    }

    async fn destination(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let config = json!({});
        create_destination(
            pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name,
                config: &config,
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn attempt(
        destination: Uuid,
        subject: Uuid,
        number: i32,
        outcome: AttemptOutcome,
    ) -> NewDeliveryAttempt {
        NewDeliveryAttempt {
            destination_id: destination,
            subject_kind: SubjectKind::Message,
            subject_id: subject,
            attempt: number,
            outcome,
            status_class: StatusClass::of(Some(503)),
            duration_ms: 12,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn attempts_are_recorded_listed_newest_first_and_a_repeat_number_conflicts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;
        let subject = Uuid::now_v7();

        let first = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 1, AttemptOutcome::Retryable),
        )
        .await
        .unwrap();
        let second = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 2, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();
        let repeat = record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, subject, 2, AttemptOutcome::Terminal),
        )
        .await
        .unwrap_err();

        assert!(repeat.is_conflict());
        assert_eq!(first.status_class, "5xx");
        assert_eq!(second.outcome, "delivered");
        let listed = list_delivery_attempts(&pool, ws, id, 10).await.unwrap();
        assert_eq!(listed, vec![second, first]);
        assert_eq!(
            list_delivery_attempts(&pool, ws, id, 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    async fn age(pool: &PgPool, subject: Uuid, days: i64) {
        sqlx::query(
            "UPDATE delivery_attempts SET attempted_at = now() - make_interval(days => $2::int) \
             WHERE subject_id = $1",
        )
        .bind(subject)
        .bind(days as i32)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn old_attempts_are_purged_in_batches_and_newer_ones_stay(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;
        let mut old = Vec::new();
        for _ in 0..5 {
            let subject = Uuid::now_v7();
            record_delivery_attempt(
                &pool,
                ws,
                &attempt(id, subject, 1, AttemptOutcome::Delivered),
            )
            .await
            .unwrap();
            age(&pool, subject, 1100).await;
            old.push(subject);
        }
        let fresh = Uuid::now_v7();
        record_delivery_attempt(&pool, ws, &attempt(id, fresh, 1, AttemptOutcome::Delivered))
            .await
            .unwrap();
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(400);

        // Batches of 2 over 5 rows: 2, 2, then a short batch of 1.
        let deleted = purge_attempts(&pool, cutoff, 2).await.unwrap();

        assert_eq!(deleted, 5);
        let left = list_delivery_attempts(&pool, ws, id, 10).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].subject_id, fresh);
        assert_eq!(purge_attempts(&pool, cutoff, 2).await.unwrap(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_age_predicate_of_the_purge_uses_the_attempted_at_index(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;
        sqlx::query(
            "INSERT INTO delivery_attempts (id, workspace_id, destination_id, subject_kind, \
                subject_id, attempt, outcome, status_class, duration_ms, attempted_at) \
             SELECT gen_random_uuid(), $1, $2, 'message', gen_random_uuid(), 1, 'delivered', \
                '2xx', 5, now() - make_interval(secs => g) FROM generate_series(1, 30000) g",
        )
        .bind(ws.as_uuid())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE delivery_attempts")
            .execute(&pool)
            .await
            .unwrap();

        let plan: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT ctid FROM delivery_attempts \
             WHERE attempted_at < now() - interval '400 days' ORDER BY attempted_at LIMIT 5000",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let plan = plan.join("\n");
        assert!(
            plan.contains("delivery_attempts_attempted_at_idx"),
            "the purge reads the age index: {plan}"
        );
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_lists_live_destinations_with_zero_queue_gauges(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let live = destination(&pool, ws, "Live").await;
        let gone = destination(&pool, ws, "Gone").await;
        crate::archive_destination(&pool, ws, gone).await.unwrap();

        let report = destination_report(&pool).await.unwrap();

        assert_eq!(
            report,
            vec![DestinationStat {
                workspace_id: ws.as_uuid(),
                destination_id: live,
                queue_depth: 0,
                oldest_age_seconds: 0,
            }]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_counts_the_ready_sends_of_each_destination_and_ages_the_oldest(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let busy = destination(&pool, ws, "Busy").await;
        let idle = destination(&pool, ws, "Idle").await;
        let job = |kind: &'static str, destination: Uuid, state: &'static str, age: i32| {
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, state, run_at, unique_key, \
                    cause_kind, cause_id, trace_id) \
                 VALUES (gen_random_uuid(), $1, $2, \
                    jsonb_build_object('destination_id', $3::text), $4, \
                    now() - $5 * interval '1 second', gen_random_uuid()::text, 'api', 't', \
                    gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(kind)
            .bind(destination)
            .bind(state)
            .bind(age)
        };
        // Two ready sends (the older is 90 s due), one deferred into the future, one running, one
        // of another kind, and one of a destination that is not live.
        for (kind, state, age) in [
            ("delivery.send", "ready", 90),
            ("delivery.send", "ready", 10),
            ("delivery.send", "ready", -600),
            ("delivery.send", "running", 500),
            ("webhook.call", "ready", 500),
        ] {
            job(kind, busy, state, age).execute(&pool).await.unwrap();
        }
        let gone = destination(&pool, ws, "Gone").await;
        job("delivery.send", gone, "ready", 5)
            .execute(&pool)
            .await
            .unwrap();
        crate::archive_destination(&pool, ws, gone).await.unwrap();

        let report = destination_report(&pool).await.unwrap();

        let of = |id: Uuid| {
            report
                .iter()
                .find(|stat| stat.destination_id == id)
                .unwrap()
        };
        assert_eq!(report.len(), 2, "{report:?}");
        assert_eq!(of(busy).queue_depth, 3);
        assert!(
            (90..=95).contains(&of(busy).oldest_age_seconds),
            "{report:?}"
        );
        assert_eq!(of(idle).queue_depth, 0);
        assert_eq!(of(idle).oldest_age_seconds, 0);
    }

    async fn ready_sends(pool: &PgPool, ws: WorkspaceId, destination: Uuid, count: i32) {
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, state, run_at, unique_key, \
                cause_kind, cause_id, trace_id) \
             SELECT gen_random_uuid(), $1, 'delivery.send', \
                jsonb_build_object('destination_id', $2::text), 'ready', \
                now() - make_interval(secs => g), gen_random_uuid()::text, 'api', 't', \
                gen_random_uuid() FROM generate_series(1, $3) g",
        )
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_depth_saturates_at_the_cap_and_the_oldest_stays_exact(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let deep = destination(&pool, ws, "Deep").await;
        let shallow = destination(&pool, ws, "Shallow").await;
        ready_sends(&pool, ws, deep, 40).await;
        ready_sends(&pool, ws, shallow, 2).await;

        let capped = destination_report_capped(&pool, 10).await.unwrap();
        let exact = destination_report(&pool).await.unwrap();

        let of = |report: &[DestinationStat], id: Uuid| {
            report
                .iter()
                .find(|stat| stat.destination_id == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(of(&capped, deep).queue_depth, 10);
        assert_eq!(of(&capped, shallow).queue_depth, 2);
        assert_eq!(of(&exact, deep).queue_depth, 40);
        assert!((40..=45).contains(&of(&capped, deep).oldest_age_seconds));
        assert!((2..=7).contains(&of(&capped, shallow).oldest_age_seconds));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_probes_the_send_index_per_destination_without_a_scan_of_the_queue(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let busy = destination(&pool, ws, "Busy").await;
        let other = destination(&pool, ws, "Other").await;
        ready_sends(&pool, ws, busy, 60_000).await;
        ready_sends(&pool, ws, other, 20_000).await;
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();

        let plan: Vec<String> = sqlx::query_scalar(
            "EXPLAIN (ANALYZE, BUFFERS) SELECT d.id, \
                (SELECT count(*) FROM (SELECT 1 FROM jobs j \
                    WHERE j.kind = 'delivery.send' AND j.state = 'ready' \
                      AND j.payload ->> 'destination_id' = d.id::text LIMIT 5000) c) \
             FROM destinations d WHERE d.archived_at IS NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let plan = plan.join("\n");
        assert!(plan.contains("jobs_delivery_send_idx"), "{plan}");
        assert!(!plan.contains("Seq Scan on jobs"), "{plan}");
    }

    #[test]
    fn a_failed_recording_is_counted_by_ids_only() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = WorkspaceId::new();
        let id = Uuid::from_u128(9);

        count_delivery_attempt_record_failure(ws, id);

        let output = handle.render();
        let want = format!(
            "zirv_flow_delivery_attempt_record_failures_total{{workspace=\"{}\",destination=\"{id}\"}} 1",
            ws.as_uuid()
        );
        assert!(output.contains(&want), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn counting_a_committed_attempt_counts_it_by_destination_and_outcome_with_ids_only(
        pool: PgPool,
    ) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "secret-mail-name").await;

        for (number, outcome) in [
            (1, AttemptOutcome::Retryable),
            (2, AttemptOutcome::Retryable),
            (3, AttemptOutcome::Delivered),
        ] {
            let new = attempt(id, Uuid::nil(), number, outcome);
            record_delivery_attempt(&pool, ws, &new).await.unwrap();
            count_delivery_attempt(ws, &new);
        }
        // A refused (conflicting) record is never counted: the caller has nothing to count.
        record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, Uuid::nil(), 3, AttemptOutcome::Terminal),
        )
        .await
        .unwrap_err();

        let output = handle.render();
        let labels = format!("workspace=\"{}\",destination=\"{id}\"", ws.as_uuid());
        assert!(
            output.contains(&format!(
                "zirv_flow_destination_attempts_total{{{labels}}} 3"
            )),
            "{output}"
        );
        for (outcome, count) in [("retryable", 2), ("delivered", 1)] {
            assert!(
                output.contains(&format!(
                    "zirv_flow_destination_outcomes_total{{{labels},outcome=\"{outcome}\"}} {count}"
                )),
                "{output}"
            );
        }
        assert!(!output.contains("terminal"), "{output}");
        assert!(!output.contains("secret-mail-name"), "{output}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn recording_an_attempt_alone_counts_nothing(pool: PgPool) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let ws = workspace(&pool, "one").await;
        let id = destination(&pool, ws, "Mail").await;

        // Recorded, then rolled back: nothing was delivered as far as the database knows.
        let mut tx = crate::begin(&pool).await.unwrap();
        record_delivery_attempt(
            &mut *tx,
            ws,
            &attempt(id, Uuid::nil(), 1, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();
        drop(tx);
        record_delivery_attempt(
            &pool,
            ws,
            &attempt(id, Uuid::nil(), 1, AttemptOutcome::Delivered),
        )
        .await
        .unwrap();

        let output = handle.render();
        assert!(
            !output.contains("zirv_flow_destination_attempts_total"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_index_of_the_new_tables_leads_with_the_workspace_except_the_primary_keys(
        pool: PgPool,
    ) {
        // The one exception is the age index of the retention purge (0108): attempts name no
        // person and the purge cuts them at one instance-wide age, so it cannot lead with a tenant.
        let leading: Vec<(String, String)> = sqlx::query_as(
            "SELECT t.relname::text, a.attname::text \
             FROM pg_index x \
             JOIN pg_class t ON t.oid = x.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace AND n.nspname = current_schema() \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = x.indkey[0] \
             WHERE t.relname IN ('destinations', 'destination_rate_buckets', 'delivery_attempts') \
               AND NOT x.indisprimary \
               AND x.indexrelid::regclass::text <> 'delivery_attempts_attempted_at_idx' \
             ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(leading.len() >= 5, "{leading:?}");
        for (table, column) in &leading {
            assert_eq!(
                column, "workspace_id",
                "an index of {table} leads with {column}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_new_tables_hold_no_person_or_address_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('destinations', 'destination_rate_buckets', 'delivery_attempts') \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name ~ '(email|address|phone|recipient|\\mto\\M)') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
        // The scan the privacy tests run names person columns the same way.
        let scan: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND data_type = 'uuid' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
               AND table_name IN ('destinations', 'destination_rate_buckets', 'delivery_attempts')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(scan.is_empty(), "{scan:?}");
    }
}
