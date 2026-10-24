//! The forwarding outbox: copying accepted events into it in the writer's transaction, claiming
//! due rows for delivery, recording how each attempt went, and re-driving dead rows.

use std::time::Duration;

use hashbrown::HashSet;
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::forwarding::{Mapping, parse_filter};

use super::rules::active_rules;
use super::sql::{Bind, insert_for};
use crate::StoreError;
use crate::jobs::{DEFAULT_MAX_ATTEMPTS, NOTIFY_CHANNEL};

/// The kind of the job that delivers a destination's outbox rows.
pub const FORWARD_JOB_KIND: &str = "event.forward";

/// Rows kept after delivery, for the idempotency guard, before they are purged.
pub const DELIVERED_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// A stored event, by the primary key of the partitioned `events` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForwardRef {
    pub id: Uuid,
    pub occurred_at: OffsetDateTime,
}

/// Test-only switch: with it set, [`enqueue_forwarding`] does nothing. The ingest measurement uses it to
/// time the path as it was before the outbox existed.
#[cfg(test)]
pub(crate) static DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Copies the events that an active rule selects into the outbox, in the caller's transaction,
/// which also holds the events. Call it once per batch, after the events are inserted.
///
/// With no active rule in the workspace it costs one indexed `EXISTS`. Otherwise it runs one
/// set-based `INSERT ... SELECT` per active rule over the whole batch, then makes sure each
/// destination that got a row has an `event.forward` job. A row already there for the same
/// destination and event is left alone.
pub async fn enqueue_forwarding(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    events: &[ForwardRef],
) -> Result<(), StoreError> {
    #[cfg(test)]
    if DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    if events.is_empty() {
        return Ok(());
    }
    let any: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forwarding_rules WHERE workspace_id = $1 AND active)",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !any {
        return Ok(());
    }
    let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
    let times: Vec<OffsetDateTime> = events.iter().map(|event| event.occurred_at).collect();
    let mut woken = HashSet::new();
    for rule in active_rules(connection, workspace).await? {
        // A rule that does not parse was written by something other than the API: it forwards
        // nothing rather than failing the event write.
        let Ok(mapping) = Mapping::parse(&rule.mapping) else {
            continue;
        };
        let Ok(filters) = parse_filter(&rule.event_name, &rule.filter) else {
            continue;
        };
        let (sql, binds) = insert_for(&mapping, &filters);
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(workspace.as_uuid())
            .bind(rule.id)
            .bind(rule.destination_id)
            .bind(&ids)
            .bind(&times)
            .bind(&rule.event_name);
        for bind in binds {
            query = match bind {
                Bind::Text(text) => query.bind(text),
                Bind::TextArray(items) => query.bind(items),
                Bind::Jsonb(value) => query.bind(value),
            };
        }
        let inserted = query
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
        if inserted > 0 {
            woken.insert(rule.destination_id);
        }
    }
    for destination in woken {
        ensure_forward_job(connection, workspace, destination, None).await?;
    }
    Ok(())
}

/// Makes sure one `event.forward` job of `destination` is ready, due at `run_at` (now when
/// `None`): a ready job is pulled forward when it is due later, and a new one is inserted when
/// none is ready. A running job does not count, since it may already have looked for rows. Two
/// unique keys alternate (`gen` 0 and 1) so that a running job can leave its successor behind.
pub async fn ensure_forward_job(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    destination: Uuid,
    run_at: Option<OffsetDateTime>,
) -> Result<(), StoreError> {
    sqlx::query(
        "WITH due AS (SELECT COALESCE($3::timestamptz, now()) AS at), \
         pulled AS ( \
             UPDATE jobs SET run_at = (SELECT at FROM due) \
             WHERE kind = 'event.forward' AND state = 'ready' \
               AND payload ->> 'destination_id' = $2::text AND run_at > (SELECT at FROM due) \
             RETURNING id), \
         ready AS ( \
             SELECT 1 FROM jobs WHERE kind = 'event.forward' AND state = 'ready' \
               AND payload ->> 'destination_id' = $2::text), \
         gen AS ( \
             SELECT COALESCE((SELECT 1 - (payload ->> 'gen')::int FROM jobs \
                              WHERE kind = 'event.forward' AND state = 'running' \
                                AND payload ->> 'destination_id' = $2::text LIMIT 1), 0) AS g), \
         inserted AS ( \
             INSERT INTO jobs (id, workspace_id, kind, payload, priority, run_at, max_attempts, \
                               unique_key, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, 'event.forward', \
                    jsonb_build_object('workspace_id', $1::text, 'destination_id', $2::text, \
                                       'gen', gen.g), \
                    0, (SELECT at FROM due), $4, \
                    'event.forward:' || $2::text || ':' || gen.g, gen_random_uuid(), \
                    'schedule', $2::text \
             FROM gen WHERE NOT EXISTS (SELECT 1 FROM ready) \
             ON CONFLICT DO NOTHING RETURNING 1) \
         SELECT pg_notify($5, 'event.forward') \
         FROM (SELECT 1 FROM inserted UNION ALL SELECT 1 FROM pulled) woken LIMIT 1",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(run_at)
    .bind(DEFAULT_MAX_ATTEMPTS)
    .bind(NOTIFY_CHANNEL)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// A row taken for delivery.
#[derive(Clone, Debug, FromRow)]
pub struct ClaimedRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub event_id: Uuid,
    pub rule_id: Uuid,
    pub idempotency_key: String,
    pub payload: Value,
    /// The attempt this claim is, counting from 1 in the current cycle.
    pub attempts: i32,
    /// Attempts of earlier cycles (before a re-drive).
    pub attempt_offset: i32,
}

impl ClaimedRow {
    /// The number recorded in `delivery_attempts`: unique across re-drives.
    pub fn attempt_number(&self) -> i32 {
        self.attempt_offset + self.attempts
    }
}

/// Takes up to `limit` due pending rows of `destination` for delivery. A claim counts as an
/// attempt and pushes the row's next attempt `lease` into the future, so a worker that dies
/// leaves the row to be taken again after the lease and no lock is held while the destination
/// is called. Rows are returned oldest first.
pub async fn claim_due(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    destination: Uuid,
    limit: i64,
    lease: Duration,
) -> Result<Vec<ClaimedRow>, StoreError> {
    let mut rows: Vec<(OffsetDateTime, ClaimedRow)> = sqlx::query_as::<_, ClaimedWithDue>(
        "UPDATE forwarding_outbox o SET attempts = o.attempts + 1, \
             next_attempt_at = clock_timestamp() + make_interval(secs => $4), updated_at = now() \
         FROM (SELECT id, next_attempt_at AS due FROM forwarding_outbox \
               WHERE workspace_id = $1 AND destination_id = $2 AND state = 'pending' \
                 AND next_attempt_at <= clock_timestamp() \
               ORDER BY next_attempt_at, id LIMIT $3 FOR UPDATE SKIP LOCKED) c \
         WHERE o.id = c.id \
         RETURNING c.due, o.id, o.workspace_id, o.event_id, o.rule_id, o.idempotency_key, o.payload, o.attempts, \
                   o.attempt_offset",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(limit)
    .bind(lease.as_secs_f64())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?
    .into_iter()
    .map(|row| (row.due, row.row))
    .collect();
    rows.sort_by_key(|(due, row)| (*due, row.id));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

#[derive(FromRow)]
struct ClaimedWithDue {
    due: OffsetDateTime,
    #[sqlx(flatten)]
    row: ClaimedRow,
}

/// How a claimed attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Settled {
    Delivered,
    /// Try again at this time.
    Retry(OffsetDateTime),
    Dead,
}

/// Records how attempt `attempts` of row `id` ended, with the fixed `error` code for a retry or
/// a dead letter. The write is fenced on the attempt number and the pending state: a row that was
/// re-driven, erased or settled meanwhile is left as it is. Returns whether the row changed.
pub async fn settle_row(
    connection: &mut PgConnection,
    id: Uuid,
    attempts: i32,
    how: Settled,
    error: Option<&str>,
) -> Result<bool, StoreError> {
    let (state, at) = match how {
        Settled::Delivered => ("delivered", None),
        Settled::Retry(at) => ("pending", Some(at)),
        Settled::Dead => ("dead", None),
    };
    let changed = sqlx::query(
        "UPDATE forwarding_outbox SET state = $3, \
             payload = CASE WHEN $3 = 'delivered' THEN '{}'::jsonb ELSE payload END, \
             last_error = $5, next_attempt_at = COALESCE($4, next_attempt_at), updated_at = now() \
         WHERE id = $1 AND state = 'pending' AND attempts = $2",
    )
    .bind(id)
    .bind(attempts)
    .bind(state)
    .bind(at)
    .bind(error)
    .execute(connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(changed > 0)
}

/// What a re-drive covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Redrive {
    /// One row, dead or pending.
    Row(Uuid),
    /// Every dead row of the destination.
    Destination(Uuid),
    /// Every dead row of the workspace.
    Workspace,
}

/// Puts rows back to `pending`, due now, with the attempts of the cycle reset, and makes sure
/// each affected destination has a job. It writes no event and starts no flow. Returns the number
/// of rows re-driven.
pub async fn redrive(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    scope: Redrive,
) -> Result<u64, StoreError> {
    let (row, destination) = match scope {
        Redrive::Row(id) => (Some(id), None),
        Redrive::Destination(id) => (None, Some(id)),
        Redrive::Workspace => (None, None),
    };
    let destinations: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE forwarding_outbox SET state = 'pending', attempt_offset = attempt_offset + attempts, \
             attempts = 0, next_attempt_at = now(), updated_at = now() \
         WHERE workspace_id = $1 \
           AND CASE WHEN $2::uuid IS NOT NULL THEN id = $2 AND state IN ('dead', 'pending') \
                    ELSE state = 'dead' AND ($3::uuid IS NULL OR destination_id = $3) END \
         RETURNING destination_id",
    )
    .bind(workspace.as_uuid())
    .bind(row)
    .bind(destination)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let count = destinations.len() as u64;
    let distinct: HashSet<Uuid> = destinations.into_iter().collect();
    for destination in distinct {
        ensure_forward_job(connection, workspace, destination, None).await?;
    }
    Ok(count)
}

/// A dead row as the dead-letter list shows it: ids, the fixed code and times, never the payload.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DeadLetter {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub rule_id: Uuid,
    pub event_id: Uuid,
    pub last_error: Option<String>,
    /// Attempts in all, re-drives included.
    pub attempts: i32,
    pub created_at: OffsetDateTime,
    pub died_at: OffsetDateTime,
}

/// The workspace's dead rows, newest first, strictly before `before` (a `(died_at, id)` cursor).
pub async fn list_dead_letters(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    destination: Option<Uuid>,
    before: Option<(OffsetDateTime, Uuid)>,
    limit: i64,
) -> Result<Vec<DeadLetter>, StoreError> {
    sqlx::query_as(
        "SELECT id, destination_id, rule_id, event_id, last_error, \
                attempt_offset + attempts AS attempts, created_at, updated_at AS died_at \
         FROM forwarding_outbox \
         WHERE workspace_id = $1 AND state = 'dead' AND ($2::uuid IS NULL OR destination_id = $2) \
           AND ($3::timestamptz IS NULL OR (updated_at, id) < ($3, $4)) \
         ORDER BY updated_at DESC, id DESC LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .bind(before.map(|(at, _)| at))
    .bind(before.map(|(_, id)| id))
    .bind(limit)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// The state of one destination's forwarding queue.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct QueueStatus {
    pub destination_id: Uuid,
    /// Rows waiting for delivery, a backed-off row included.
    pub depth: i64,
    /// Seconds the oldest waiting row has been past due; 0 when none is.
    pub oldest_lag_seconds: i64,
    pub dead: i64,
}

/// A queue status per live destination of the workspace that has a rule or any forwarding row.
pub async fn forwarding_status(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<QueueStatus>, StoreError> {
    sqlx::query_as(
        "SELECT d.id AS destination_id, \
                (SELECT count(*) FROM forwarding_outbox o WHERE o.destination_id = d.id \
                   AND o.state = 'pending') AS depth, \
                COALESCE((SELECT GREATEST(0, EXTRACT(EPOCH FROM clock_timestamp() - \
                                                     min(o.next_attempt_at)))::bigint \
                          FROM forwarding_outbox o \
                          WHERE o.destination_id = d.id AND o.state = 'pending'), 0) \
                    AS oldest_lag_seconds, \
                (SELECT count(*) FROM forwarding_outbox o WHERE o.destination_id = d.id \
                   AND o.workspace_id = d.workspace_id AND o.state = 'dead') AS dead \
         FROM destinations d \
         WHERE d.workspace_id = $1 AND d.archived_at IS NULL \
           AND (EXISTS (SELECT 1 FROM forwarding_rules r WHERE r.destination_id = d.id) \
                OR EXISTS (SELECT 1 FROM forwarding_outbox o WHERE o.destination_id = d.id)) \
         ORDER BY lower(d.name), d.id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// What the leader publishes for one destination with waiting rows.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ForwardingStat {
    pub workspace_id: Uuid,
    pub destination_id: Uuid,
    pub depth: i64,
    pub oldest_lag_seconds: i64,
    /// Whether an `event.forward` job of the destination is ready or running.
    pub has_job: bool,
}

/// One row per destination that has pending rows, across workspaces.
pub async fn forwarding_report(
    connection: &mut PgConnection,
) -> Result<Vec<ForwardingStat>, StoreError> {
    sqlx::query_as(
        "SELECT d.workspace_id, d.id AS destination_id, p.depth, \
                GREATEST(0, EXTRACT(EPOCH FROM clock_timestamp() - p.oldest))::bigint \
                    AS oldest_lag_seconds, \
                EXISTS (SELECT 1 FROM jobs j WHERE j.kind = 'event.forward' \
                          AND j.state IN ('ready', 'running') \
                          AND j.payload ->> 'destination_id' = d.id::text) AS has_job \
         FROM (SELECT destination_id, count(*) AS depth, min(next_attempt_at) AS oldest \
               FROM forwarding_outbox WHERE state = 'pending' GROUP BY destination_id) p \
         JOIN destinations d ON d.id = p.destination_id AND d.archived_at IS NULL \
         ORDER BY d.id",
    )
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// Deletes up to `limit` delivered rows of `destination` older than `retention`. Returns the
/// number deleted.
pub async fn purge_delivered(
    connection: &mut PgConnection,
    destination: Uuid,
    retention: Duration,
    limit: i64,
) -> Result<u64, StoreError> {
    Ok(sqlx::query(
        "DELETE FROM forwarding_outbox WHERE id IN ( \
             SELECT id FROM forwarding_outbox \
             WHERE destination_id = $1 AND state = 'delivered' \
               AND updated_at < now() - make_interval(secs => $2) \
             LIMIT $3)",
    )
    .bind(destination)
    .bind(retention.as_secs_f64())
    .bind(limit)
    .execute(connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected())
}

/// The earliest time a pending row of `destination` is due, if any.
pub async fn next_due(
    connection: &mut PgConnection,
    destination: Uuid,
) -> Result<Option<OffsetDateTime>, StoreError> {
    sqlx::query_scalar(
        "SELECT min(next_attempt_at) FROM forwarding_outbox \
         WHERE destination_id = $1 AND state = 'pending'",
    )
    .bind(destination)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// [`enqueue_forwarding`] for the `segment.entered` and `segment.exited` events a segment pass
/// wrote at `at`. The pass inserts them in SQL of its own, so they are found again by name, time
/// and segment in the same transaction. With no active rule for either name it costs one indexed
/// lookup and reads no event.
pub async fn enqueue_segment_forwarding(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    let wanted: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forwarding_rules WHERE workspace_id = $1 AND active \
            AND event_name IN ('segment.entered', 'segment.exited'))",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !wanted {
        return Ok(());
    }
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM events WHERE workspace_id = $1 AND occurred_at = $2 \
            AND name IN ('segment.entered', 'segment.exited') \
            AND properties ->> 'segment_id' = $3::text",
    )
    .bind(workspace.as_uuid())
    .bind(at)
    .bind(segment)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let refs: Vec<ForwardRef> = ids
        .into_iter()
        .map(|id| ForwardRef { id, occurred_at: at })
        .collect();
    enqueue_forwarding(connection, workspace, &refs).await
}

/// Gives a claimed row back without counting the attempt, due at `at`: the destination was not
/// called (a rate-limit wait). Fenced like [`settle_row`].
pub async fn defer_row(
    connection: &mut PgConnection,
    id: Uuid,
    attempts: i32,
    at: OffsetDateTime,
) -> Result<bool, StoreError> {
    Ok(sqlx::query(
        "UPDATE forwarding_outbox SET attempts = attempts - 1, next_attempt_at = $3, \
             updated_at = now() \
         WHERE id = $1 AND state = 'pending' AND attempts = $2",
    )
    .bind(id)
    .bind(attempts)
    .bind(at)
    .execute(connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected()
        > 0)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::forwarding::idempotency_key;
    use zirv_flow_core::{HmacKey, HmacPurpose, HmacRoot};

    use super::super::rules::tests::{add_rule, fixture, payloads};
    use super::super::rules::{RuleUpdate, update_rule};
    use super::*;
    use crate::privacy::testing::{master_key, trace};
    use crate::{EventOutcome, EventRecord, apply_event_batch};

    fn redaction_key() -> HmacKey {
        HmacRoot::bootstrap_from(&master_key()).hmac_key(HmacPurpose::Redaction)
    }

    fn record(name: &str, properties: Value, message_id: &str) -> EventRecord {
        EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: name.into(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: Some(datetime!(2031-05-06 07:08:09.25 UTC)),
            received_at: datetime!(2031-05-06 07:08:10 UTC),
            message_id: Some(message_id.into()),
            trace: trace(),
        }
    }

    async fn rows(pool: &PgPool, ws: WorkspaceId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM forwarding_outbox WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn refs(pool: &PgPool, ws: WorkspaceId) -> Vec<ForwardRef> {
        let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
            "SELECT id, occurred_at FROM events WHERE workspace_id = $1 ORDER BY id",
        )
        .bind(ws.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(id, occurred_at)| ForwardRef { id, occurred_at })
            .collect()
    }

    fn accepted(outcomes: &[EventOutcome]) -> Vec<(Uuid, Uuid)> {
        outcomes
            .iter()
            .map(|outcome| match outcome {
                EventOutcome::Accepted {
                    event_id,
                    person_id,
                    ..
                } => (*event_id, *person_id),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_mapping_reads_every_source_kind_in_the_writers_transaction(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "map").await;
        let mapping = json!({
            "event_id": {"from": "event.id"},
            "name": {"from": "event.name"},
            "at": {"from": "event.time"},
            "seen": {"from": "event.received_at"},
            "who": {"from": "person.id"},
            "plan": {"from": "event.properties", "pointer": "/plan"},
            "deep": {"from": "event.properties", "pointer": "/cart/items/0"},
            "missing": {"from": "event.properties", "pointer": "/nope"},
            "src": {"value": "shop"},
            "n": {"value": 3},
            "flag": {"value": true},
        });
        add_rule(&pool, ws, destination, "order", json!([]), mapping).await;

        let outcomes = apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[
                record(
                    "order",
                    json!({"plan": "pro", "cart": {"items": ["a", "b"]}}),
                    "m-1",
                ),
                record("other", json!({"plan": "pro"}), "m-2"),
            ],
        )
        .await
        .unwrap();

        let ids = accepted(&outcomes);
        let sent = payloads(&pool, ws).await;
        assert_eq!(sent.len(), 1, "only the event of the rule's name is copied");
        assert_eq!(
            sent[0],
            json!({
                "event_id": ids[0].0.to_string(),
                "name": "order",
                "at": "2031-05-06T07:08:09.250000Z",
                "seen": "2031-05-06T07:08:10.000000Z",
                "who": ids[0].1.to_string(),
                "plan": "pro",
                "deep": "a",
                "missing": null,
                "src": "shop",
                "n": 3,
                "flag": true,
            })
        );
        let (key, person, state): (String, Uuid, String) = sqlx::query_as(
            "SELECT idempotency_key, person_id, state FROM forwarding_outbox \
             WHERE workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(key, idempotency_key(ids[0].0, destination));
        assert_eq!((person, state.as_str()), (ids[0].1, "pending"));
        let jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'event.forward' AND state = 'ready'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(jobs, 1, "one ready job for the destination");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn with_no_active_rule_nothing_is_copied_and_no_job_is_made(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "norule").await;
        let rule = add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;
        let mut connection = pool.acquire().await.unwrap();
        update_rule(
            &mut connection,
            ws,
            rule,
            &RuleUpdate {
                active: Some(false),
                ..RuleUpdate::default()
            },
        )
        .await
        .unwrap();
        drop(connection);

        apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[record("order", json!({}), "m-1")],
        )
        .await
        .unwrap();

        assert_eq!(rows(&pool, ws).await, 0);
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'event.forward'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rolled_back_write_leaves_no_outbox_row_and_no_job(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "rollback").await;
        add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;

        let mut tx = crate::begin(&pool).await.unwrap();
        let mut batch = crate::events::EventBatch::default();
        crate::events::apply_event_record(
            &mut tx,
            ws,
            &redaction_key(),
            &mut batch,
            &record("order", json!({}), "m-1"),
        )
        .await
        .unwrap();
        batch.finish(&mut tx, ws).await.unwrap();
        let inside: i64 = sqlx::query_scalar("SELECT count(*) FROM forwarding_outbox")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();

        assert_eq!(inside, 1, "the row exists inside the writer's transaction");
        assert_eq!(rows(&pool, ws).await, 0);
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
                .bind(ws.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 0);
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'event.forward'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_filter_selects_by_property_and_a_type_mismatch_never_matches(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "filter").await;
        let events = [
            json!({"plan": "pro", "total": 20, "tags": "vip-gold"}),
            json!({"plan": "free", "total": 5, "tags": "100%_off"}),
            json!({"plan": 7, "total": "20"}),
            json!({}),
        ];
        let records: Vec<EventRecord> = events
            .iter()
            .enumerate()
            .map(|(n, properties)| record("order", properties.clone(), &format!("m-{n}")))
            .collect();
        apply_event_batch(&pool, ws, &redaction_key(), &records)
            .await
            .unwrap();
        let rule = add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"plan": {"from": "event.properties", "pointer": "/plan"}}),
        )
        .await;
        let stored = refs(&pool, ws).await;
        let all = vec![json!("pro"), json!("free"), json!(7), Value::Null];
        let cases: Vec<(Value, Vec<Value>)> = vec![
            (json!([]), all),
            (
                json!([{"prop": "plan", "op": "eq", "value": "pro"}]),
                vec![json!("pro")],
            ),
            (
                json!([{"prop": "plan", "op": "neq", "value": "pro"}]),
                vec![json!("free")],
            ),
            (
                json!([{"prop": "plan", "op": "in", "value": ["pro", "free"]}]),
                vec![json!("pro"), json!("free")],
            ),
            (
                json!([{"prop": "plan", "op": "not_in", "value": ["pro"]}]),
                vec![json!("free")],
            ),
            (
                json!([{"prop": "total", "op": "gte", "value": 20}]),
                vec![json!("pro")],
            ),
            (
                json!([{"prop": "total", "op": "lt", "value": 10}]),
                vec![json!("free")],
            ),
            (
                json!([{"prop": "total", "op": "between", "value": [1, 25]}]),
                vec![json!("pro"), json!("free")],
            ),
            (
                json!([{"prop": "tags", "op": "contains", "value": "gold"}]),
                vec![json!("pro")],
            ),
            (
                json!([{"prop": "tags", "op": "contains", "value": "%_"}]),
                vec![json!("free")],
            ),
            (
                json!([{"prop": "tags", "op": "starts_with", "value": "vip"}]),
                vec![json!("pro")],
            ),
            (
                json!([{"prop": "tags", "op": "exists"}]),
                vec![json!("pro"), json!("free")],
            ),
            (
                json!([{"prop": "tags", "op": "not_exists"}]),
                vec![json!(7), Value::Null],
            ),
            (
                json!([
                    {"prop": "plan", "op": "eq", "value": "pro"},
                    {"prop": "total", "op": "gt", "value": 100}
                ]),
                vec![],
            ),
        ];

        for (filter, want) in cases {
            let mut connection = pool.acquire().await.unwrap();
            sqlx::query("DELETE FROM forwarding_outbox")
                .execute(&mut *connection)
                .await
                .unwrap();
            update_rule(
                &mut connection,
                ws,
                rule,
                &RuleUpdate {
                    filter: Some(&filter),
                    ..RuleUpdate::default()
                },
            )
            .await
            .unwrap();
            enqueue_forwarding(&mut connection, ws, &stored)
                .await
                .unwrap();
            drop(connection);

            let mut got: Vec<String> = payloads(&pool, ws)
                .await
                .into_iter()
                .map(|payload| payload["plan"].to_string())
                .collect();
            let mut want: Vec<String> = want.iter().map(Value::to_string).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "{filter}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_batches_with_the_same_event_make_one_row(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "dup").await;
        apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[record("order", json!({}), "m-1")],
        )
        .await
        .unwrap();
        add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;
        let stored = refs(&pool, ws).await;

        let mut first = crate::begin(&pool).await.unwrap();
        enqueue_forwarding(&mut first, ws, &stored).await.unwrap();
        let second = tokio::spawn({
            let (pool, stored) = (pool.clone(), stored.clone());
            async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                enqueue_forwarding(&mut tx, ws, &stored).await.unwrap();
                crate::commit(tx).await.unwrap();
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        crate::commit(first).await.unwrap();
        second.await.unwrap();

        assert_eq!(rows(&pool, ws).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_claim_counts_the_attempt_and_a_settle_is_fenced_on_it(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "claim").await;
        add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;
        apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[record("order", json!({}), "m-1")],
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let lease = Duration::from_secs(300);

        let first = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        let again = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        assert_eq!((first.len(), first[0].attempts, again.len()), (1, 1, 0));
        let row = &first[0];

        // A settle for another attempt number changes nothing.
        let stale = settle_row(&mut connection, row.id, 2, Settled::Dead, Some("http_4xx"))
            .await
            .unwrap();
        let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
        let retried = settle_row(
            &mut connection,
            row.id,
            1,
            Settled::Retry(later),
            Some("http_5xx"),
        )
        .await
        .unwrap();
        assert!(!stale && retried);
        let none = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        assert!(none.is_empty(), "a retry waits for its time");
        assert!(next_due(&mut connection, destination).await.unwrap().is_some());

        sqlx::query("UPDATE forwarding_outbox SET next_attempt_at = now()")
            .execute(&mut *connection)
            .await
            .unwrap();
        let second = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        assert_eq!(second[0].attempts, 2);
        assert_eq!(second[0].idempotency_key, row.idempotency_key);
        settle_row(&mut connection, row.id, 2, Settled::Dead, Some("http_5xx"))
            .await
            .unwrap();
        let letters = list_dead_letters(&mut connection, ws, None, None, 10)
            .await
            .unwrap();
        assert_eq!(letters.len(), 1);
        assert_eq!(
            (letters[0].last_error.as_deref(), letters[0].attempts),
            (Some("http_5xx"), 2)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_redrive_resets_the_cycle_keeps_attempt_numbers_unique_and_starts_no_flow(
        pool: PgPool,
    ) {
        let (ws, destination) = fixture(&pool, "redrive").await;
        add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;
        apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[
                record("order", json!({}), "m-1"),
                record("order", json!({}), "m-2"),
            ],
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let lease = Duration::from_secs(300);
        let claimed = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        for row in &claimed {
            settle_row(&mut connection, row.id, 1, Settled::Dead, Some("http_4xx"))
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM jobs WHERE kind = 'event.forward'")
            .execute(&mut *connection)
            .await
            .unwrap();

        let one = redrive(&mut connection, ws, Redrive::Row(claimed[0].id))
            .await
            .unwrap();
        let rest = redrive(&mut connection, ws, Redrive::Destination(destination))
            .await
            .unwrap();
        let nothing = redrive(&mut connection, ws, Redrive::Workspace)
            .await
            .unwrap();

        assert_eq!((one, rest, nothing), (1, 1, 0));
        let again = claim_due(&mut connection, ws, destination, 10, lease)
            .await
            .unwrap();
        assert_eq!(again.len(), 2);
        for row in &again {
            assert_eq!((row.attempts, row.attempt_offset), (1, 1));
            assert_eq!(
                row.attempt_number(),
                2,
                "the number continues past the dead cycle"
            );
        }
        let forward: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'event.forward'")
                .fetch_one(&mut *connection)
                .await
                .unwrap();
        let flow: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind LIKE 'flow.%'")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!((forward, flow), (1, 0), "one forwarding job, and no flow job");
        let enrollments: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollments")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(enrollments, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn at_most_one_job_is_ready_and_a_running_one_leaves_a_successor(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "jobs").await;
        let mut connection = pool.acquire().await.unwrap();
        async fn ready(connection: &mut sqlx::PgConnection) -> Result<Vec<(String, bool)>, sqlx::Error> {
            sqlx::query_as(
                "SELECT unique_key, run_at > now() + interval '30 minutes' FROM jobs \
                 WHERE kind = 'event.forward' AND state = 'ready' ORDER BY unique_key",
            )
            .fetch_all(connection)
            .await
        }

        ensure_forward_job(&mut connection, ws, destination, None)
            .await
            .unwrap();
        ensure_forward_job(&mut connection, ws, destination, None)
            .await
            .unwrap();
        let first = ready(&mut connection).await.unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].0.ends_with(":0"));

        // A job that is running does not count as the ready one.
        let claimed = crate::claim_job(
            &pool,
            "w",
            &[FORWARD_JOB_KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(claimed.unique_key.as_deref(), Some(first[0].0.as_str()));
        ensure_forward_job(&mut connection, ws, destination, None)
            .await
            .unwrap();
        ensure_forward_job(&mut connection, ws, destination, None)
            .await
            .unwrap();
        let successor = ready(&mut connection).await.unwrap();
        assert_eq!(successor.len(), 1);
        assert!(successor[0].0.ends_with(":1"), "{successor:?}");

        // A ready job due later is pulled forward, not duplicated.
        sqlx::query("UPDATE jobs SET run_at = now() + interval '1 hour' WHERE state = 'ready'")
            .execute(&mut *connection)
            .await
            .unwrap();
        assert!(ready(&mut connection).await.unwrap()[0].1);
        ensure_forward_job(&mut connection, ws, destination, None)
            .await
            .unwrap();
        let pulled = ready(&mut connection).await.unwrap();
        assert_eq!((pulled.len(), pulled[0].1), (1, false));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_status_reports_depth_lag_and_dead_and_delivered_rows_are_purged(pool: PgPool) {
        let (ws, destination) = fixture(&pool, "status").await;
        add_rule(
            &pool,
            ws,
            destination,
            "order",
            json!([]),
            json!({"id": {"from": "event.id"}}),
        )
        .await;
        apply_event_batch(
            &pool,
            ws,
            &redaction_key(),
            &[
                record("order", json!({}), "m-1"),
                record("order", json!({}), "m-2"),
                record("order", json!({}), "m-3"),
            ],
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        sqlx::query(
            "UPDATE forwarding_outbox SET next_attempt_at = now() - interval '90 seconds' \
             WHERE id = (SELECT id FROM forwarding_outbox ORDER BY id LIMIT 1)",
        )
        .execute(&mut *connection)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE forwarding_outbox SET state = 'dead', last_error = 'http_4xx' \
             WHERE id = (SELECT id FROM forwarding_outbox ORDER BY id DESC LIMIT 1)",
        )
        .execute(&mut *connection)
        .await
        .unwrap();

        let status = forwarding_status(&mut connection, ws).await.unwrap();
        let report = forwarding_report(&mut connection).await.unwrap();

        assert_eq!(status.len(), 1);
        assert_eq!((status[0].depth, status[0].dead), (2, 1));
        assert!(
            (90..=95).contains(&status[0].oldest_lag_seconds),
            "{status:?}"
        );
        assert_eq!(report.len(), 1);
        assert_eq!((report[0].depth, report[0].has_job), (2, true));

        sqlx::query(
            "UPDATE forwarding_outbox SET state = 'delivered', payload = '{}', \
                updated_at = now() - interval '8 days' WHERE state = 'pending'",
        )
        .execute(&mut *connection)
        .await
        .unwrap();
        let purged = purge_delivered(&mut connection, destination, DELIVERED_RETENTION, 10)
            .await
            .unwrap();
        assert_eq!(purged, 2);
        assert_eq!(rows(&pool, ws).await, 1, "the dead letter stays");
    }

    /// Source files under `crates/*/src`, with the text from the first test module on cut off.
    fn production_sources() -> Vec<(PathBuf, String)> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&crates).unwrap() {
            let src = entry.unwrap().path().join("src");
            if src.is_dir() {
                walk(&src, &mut files);
            }
        }
        files
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .unwrap()
                    .replace("\r\n", "\n");
                let lines: Vec<&str> = text.lines().collect();
                let cut = lines
                    .windows(2)
                    .position(|pair| {
                        pair[0].trim() == "#[cfg(test)]"
                            && (pair[1].trim_start().starts_with("mod ")
                                || pair[1].trim_start().starts_with("pub(crate) mod "))
                    })
                    .unwrap_or(lines.len());
                (path, lines[..cut].join("\n"))
            })
            .collect()
    }

    /// Every file that writes events, and the call that tells the outbox about them. A new writer
    /// must call one of these in its transaction and be added here; the test fails until it is.
    const WRITERS: &[(&str, &str)] = &[
        ("zirv-flow-store/src/events/apply.rs", "forwards.push"),
        ("zirv-flow-store/src/flows/delivery.rs", "enqueue_forwarding"),
        ("zirv-flow-store/src/flows/send.rs", "enqueue_forwarding"),
        ("zirv-flow-store/src/feedback.rs", "enqueue_forwarding"),
        (
            "zirv-flow-store/src/tracking/engagement.rs",
            "enqueue_forwarding",
        ),
        (
            "zirv-flow-store/src/segments/refresh.rs",
            "enqueue_segment_forwarding",
        ),
    ];

    #[test]
    fn every_file_that_writes_events_is_a_known_writer_that_tells_the_outbox() {
        let sources = production_sources();
        let normal = |path: &Path| path.to_string_lossy().replace('\\', "/");
        let writers: Vec<String> = sources
            .iter()
            .filter(|(_, text)| text.contains("INSERT INTO events"))
            .map(|(path, _)| normal(path))
            .collect();

        assert!(!writers.is_empty(), "the scan found no event writer at all");
        for path in &writers {
            let known = WRITERS.iter().find(|(file, _)| path.ends_with(file));
            let (_, call) = known.unwrap_or_else(|| {
                panic!(
                    "{path} inserts events but is not a known writer: tell the outbox with \
                     enqueue_forwarding and add it to WRITERS"
                )
            });
            let (_, text) = sources
                .iter()
                .find(|(candidate, _)| normal(candidate) == *path)
                .unwrap();
            assert!(text.contains(call), "{path} does not call {call}");
        }
        for (file, _) in WRITERS {
            assert!(
                writers.iter().any(|path| path.ends_with(file)),
                "{file} is listed but no longer writes events"
            );
        }
    }

    #[test]
    fn replay_writes_no_outbox_row() {
        let sources = production_sources();
        let (_, replay) = sources
            .iter()
            .find(|(path, _)| path.ends_with("zirv-flow-store/src/events/replay.rs"))
            .expect("the replay module exists");

        assert!(replay.contains("insert_event"), "replay still writes events");
        assert!(!replay.contains("enqueue_forwarding"));
        assert!(!replay.contains("forwards"));
    }

    /// Ingest cost of the outbox step: the same batches pushed through `apply_event_batch` into
    /// three workspaces, interleaved so drift hits all alike: the enqueue switched off (what the
    /// path cost before the outbox), no rule, and one active rule that matches about 10% of the
    /// events. Prints the p50 and p99 batch time of each. Run with `--ignored --nocapture`.
    #[sqlx::test]
    #[ignore = "measurement; requires PostgreSQL; run it by hand with --nocapture"]
    async fn ingest_batch_time_with_the_outbox_switched_off_with_no_rule_and_with_a_rule(
        pool: PgPool,
    ) {
        let batches: usize = std::env::var("ZIRV_MEASURE_BATCHES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
        let warm_up: usize = 20.min(batches);
        const SIZE: usize = 500;
        let (off_ws, _) = fixture(&pool, "measure-off").await;
        let (none_ws, _) = fixture(&pool, "measure-none").await;
        let (rule_ws, rule_destination) = fixture(&pool, "measure-rule").await;
        add_rule(
            &pool,
            rule_ws,
            rule_destination,
            "order",
            json!([]),
            json!({"who": {"from": "person.id"}, "plan": {"from": "event.properties", "pointer": "/plan"}}),
        )
        .await;
        let key = redaction_key();
        let batch = |number: usize| -> Vec<EventRecord> {
            (0..SIZE)
                .map(|n| {
                    let global = number * SIZE + n;
                    let name = if global % 10 == 0 { "order" } else { "view" };
                    let mut event = record(
                        name,
                        json!({"plan": "pro", "n": global}),
                        &format!("m-{global}"),
                    );
                    event.identifiers = vec![("email".into(), format!("u{}@example.com", global % 2000))];
                    event
                })
                .collect()
        };
        // (workspace, enqueue switched off)
        let cases = [(off_ws, true), (none_ws, false), (rule_ws, false)];
        let mut times: [Vec<f64>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        // One case alone (`off`, `none` or `rule`), for a run too long to interleave.
        let only = std::env::var("ZIRV_MEASURE_CASE").ok().and_then(|case| {
            ["off", "none", "rule"].iter().position(|name| *name == case)
        });
        for number in 0..warm_up + batches {
            let records = batch(number);
            for step in 0..3 {
                let index = (number + step) % 3;
                if only.is_some_and(|only| only != index) {
                    continue;
                }
                let (ws, off) = cases[index];
                DISABLED.store(off, std::sync::atomic::Ordering::Relaxed);
                let started = std::time::Instant::now();
                apply_event_batch(&pool, ws, &key, &records).await.unwrap();
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                if number >= warm_up {
                    times[index].push(elapsed);
                }
            }
        }
        DISABLED.store(false, std::sync::atomic::Ordering::Relaxed);

        let forwarded = rows(&pool, rule_ws).await;
        assert_eq!(rows(&pool, off_ws).await + rows(&pool, none_ws).await, 0);
        if only.is_none() || only == Some(2) {
            assert_eq!(forwarded as usize, (warm_up + batches) * SIZE / 10);
        }
        for (label, series) in ["switched off", "no rule", "one rule, 10%"]
            .iter()
            .zip(times.iter_mut())
        {
            if series.is_empty() {
                continue;
            }
            series.sort_by(f64::total_cmp);
            let at = |q: f64| series[((series.len() as f64 - 1.0) * q).round() as usize];
            println!(
                "ingest {label:>14}: p50 {:.2} ms  p99 {:.2} ms  mean {:.2} ms  ({} batches of {SIZE})",
                at(0.5),
                at(0.99),
                series.iter().sum::<f64>() / series.len() as f64,
                series.len()
            );
        }
    }
}
