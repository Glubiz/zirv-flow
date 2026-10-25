//! What a `webhook.call` job reads and writes besides the call itself (architecture 9.1): the
//! person and traits to send, read without a lock before the call, and the outcome event with its
//! decision, written after it under a short lock on the person. Every function runs in the job's
//! transaction.
//!
//! `delivery.sync` (architecture 10.2, #226) reads and writes the same way, through the same
//! functions: its outcome is a [`WebhookOutcome`] with a [`SyncDetail`], which makes the event
//! `sync.succeeded` or `sync.failed` and the decision `sync.upsert` or `sync.remove`.

use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::events::{
    SYNC_FAILED_EVENT, SYNC_SUCCEEDED_EVENT, WEBHOOK_FAILED_EVENT, WEBHOOK_SUCCEEDED_EVENT,
};
use zirv_flow_core::flow::SyncOperation;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::arrivals::{Arrival, enqueue_arrivals};
use crate::{
    EnrollmentStatus, GraphPoint, NewDecision, StoreError, append_decision, find_enrollment,
    find_person,
};

/// The source of the events a flow writes.
const FLOWS_SOURCE: &str = "flows";

/// The person a webhook call is for and the traits to send.
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookSubject {
    pub person: Uuid,
    /// Each requested key at the person's effective value, `null` when they have none.
    pub traits: Map<String, Value>,
}

/// Reads the person of `enrollment` and `trait_keys`, taking no row lock, so the HTTP call that
/// follows holds nothing an ingest batch or an erasure could wait for. None when the enrollment
/// or its person is gone (erased), the enrollment exited, or the person was merged into another:
/// the call must not be made.
pub async fn read_webhook_subject(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    trait_keys: &[String],
) -> Result<Option<WebhookSubject>, StoreError> {
    let Some(found) = find_enrollment(connection, workspace, enrollment).await? else {
        return Ok(None);
    };
    // An enrollment an exit ended (a goal, an operator, a merge) calls nobody: the call is not
    // sent. One that completed still sends, since the flow may end right after its node.
    if found.status == EnrollmentStatus::Exited {
        return Ok(None);
    }
    let Some(person) = find_person(&mut *connection, workspace, found.person_id).await? else {
        return Ok(None);
    };
    // `find_person` follows `merged_into`: a different id is a person merged away. The call is
    // not sent for the survivor, who was never the one this enrollment was about.
    if person.id != found.person_id {
        return Ok(None);
    }
    let held = person.traits.as_object();
    let traits = trait_keys
        .iter()
        .map(|key| {
            let value = held.and_then(|held| held.get(key)).cloned();
            (key.clone(), value.unwrap_or(Value::Null))
        })
        .collect();
    Ok(Some(WebhookSubject {
        person: found.person_id,
        traits,
    }))
}

/// Whether the enrollment is still the person's, as seen under the lock of
/// [`lock_webhook_person`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookPerson {
    /// The person exists and the enrollment is still theirs; the lock is held to the end of the
    /// transaction.
    Held,
    /// The person or the enrollment is gone: erased while the call was in flight. No outcome may
    /// be written.
    Gone,
    /// The person was merged into another while the call was in flight. Nothing is written for
    /// them and the job ends, since a retry would read the same.
    Moved,
}

/// After the call, locks `person` with `FOR KEY SHARE` and checks that `enrollment` still exists
/// for them.
///
/// The lock conflicts with the delete of person erasure and with nothing else a step does, so an
/// erasure waits for the short outcome write and a trait write does not. It is taken on the
/// person alone, the first of the person-then-enrollment order (#159); the enrollment is only
/// read, so a step of the same enrollment never waits.
pub async fn lock_webhook_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    person: Uuid,
) -> Result<WebhookPerson, StoreError> {
    let locked: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    match locked {
        None => return Ok(WebhookPerson::Gone),
        // Merged away while the call was in flight: an outcome event written now would sit on
        // an id the merge's rewrite has already passed.
        Some(Some(_)) => return Ok(WebhookPerson::Moved),
        Some(None) => {}
    }
    // The erasure that won the lock before us deleted the enrollment; one that comes later waits.
    Ok(
        match find_enrollment(connection, workspace, enrollment).await? {
            None => WebhookPerson::Gone,
            Some(found) if found.person_id != person => WebhookPerson::Moved,
            Some(_) => WebhookPerson::Held,
        },
    )
}

/// How a webhook call ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookResult {
    /// A 2xx response.
    Succeeded,
    /// A terminal refusal, or the last attempt of a retryable failure.
    Failed,
}

impl WebhookResult {
    const fn outcome(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// What makes a finished call a profile sync: where it went and what it did. Never the key or a
/// trait value.
#[derive(Clone, Copy, Debug)]
pub struct SyncDetail {
    pub destination: Uuid,
    pub operation: SyncOperation,
}

/// One finished webhook call, or, with a `sync` detail, one finished profile sync.
#[derive(Clone, Debug)]
pub struct WebhookOutcome<'a> {
    pub workspace: WorkspaceId,
    pub enrollment: Uuid,
    pub person: Uuid,
    pub node: &'a str,
    pub result: WebhookResult,
    /// The response status; none when no response came (a transport error).
    pub status: Option<u16>,
    /// How many times the call was attempted, this one included.
    pub attempts: i32,
    /// A fixed code for why a failure failed, such as `http_4xx` or `egress_denied`. Never the
    /// response body.
    pub reason: Option<&'static str>,
    /// The trace of the enrollment.
    pub trace: &'a TraceContext,
    pub now: OffsetDateTime,
    /// The job and attempt that recorded it.
    pub job: (Uuid, i32),
    /// Set for a profile sync: the event and the decision are the sync family's.
    pub sync: Option<SyncDetail>,
}

impl WebhookOutcome<'_> {
    const fn event_name(&self) -> &'static str {
        match (self.sync.is_some(), self.result) {
            (false, WebhookResult::Succeeded) => WEBHOOK_SUCCEEDED_EVENT,
            (false, WebhookResult::Failed) => WEBHOOK_FAILED_EVENT,
            (true, WebhookResult::Succeeded) => SYNC_SUCCEEDED_EVENT,
            (true, WebhookResult::Failed) => SYNC_FAILED_EVENT,
        }
    }

    const fn decision_action(&self) -> &'static str {
        match self.sync {
            None => "webhook.call",
            Some(SyncDetail {
                operation: SyncOperation::Upsert,
                ..
            }) => "sync.upsert",
            Some(SyncDetail {
                operation: SyncOperation::Remove,
                ..
            }) => "sync.remove",
        }
    }
}

/// Records the outcome: the `webhook.succeeded` or `webhook.failed` event of the person (source
/// `flows`, properties the node and enrollment ids, the status and the attempt count, never the
/// response), a `webhook.call` decision under the enrollment's trace, and the steps of every
/// enrollment that waits for the event. Returns the event's id. A profile sync writes the
/// `sync.` event and a `sync.upsert` or `sync.remove` decision instead, with the destination and
/// the operation as well; never the external key or a trait value.
pub async fn record_webhook_outcome(
    connection: &mut PgConnection,
    outcome: &WebhookOutcome<'_>,
) -> Result<Uuid, StoreError> {
    let event = Uuid::now_v7();
    let mut properties = json!({
        "node_id": outcome.node,
        "enrollment_id": outcome.enrollment,
        "status": outcome.status,
        "attempts": outcome.attempts,
    });
    if let (Some(sync), Some(map)) = (outcome.sync, properties.as_object_mut()) {
        map.insert("destination_id".to_owned(), json!(sync.destination));
        map.insert("operation".to_owned(), json!(sync.operation.as_str()));
    }
    sqlx::query(
        "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
            properties, source, trace_id, validation_state) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, 'valid')",
    )
    .bind(event)
    .bind(outcome.workspace.as_uuid())
    .bind(outcome.person)
    .bind(outcome.event_name())
    .bind(outcome.now)
    .bind(&properties)
    .bind(FLOWS_SOURCE)
    .bind(outcome.trace.trace_id.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    crate::forwarding::enqueue_forwarding_of(connection, outcome.workspace, event, outcome.now)
        .await?;
    // The flow and version the call belongs to, for the trace to link from; an erased enrollment has
    // none.
    let pinned: Option<(Uuid, i32)> = sqlx::query_as(
        "SELECT flow_id, flow_version FROM enrollments WHERE workspace_id = $1 AND id = $2",
    )
    .bind(outcome.workspace.as_uuid())
    .bind(outcome.enrollment)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut subject_ids = vec![
        ("enrollment", outcome.enrollment),
        ("person", outcome.person),
        ("event", event),
    ];
    subject_ids.extend(pinned.map(|(flow, _)| ("flow", flow)));
    subject_ids.extend(outcome.sync.map(|sync| ("destination", sync.destination)));
    append_decision(
        &mut *connection,
        &NewDecision {
            graph: pinned.map(|(_, version)| GraphPoint::at(version, outcome.node)),
            workspace_id: outcome.workspace,
            trace: outcome.trace.clone(),
            action: outcome.decision_action(),
            outcome: outcome.result.outcome(),
            reason: outcome.reason,
            subject_ids,
            job: Some(outcome.job),
        },
    )
    .await?;
    enqueue_arrivals(
        connection,
        outcome.workspace,
        &[Arrival {
            event_id: event,
            person_id: outcome.person,
            name: outcome.event_name().to_owned(),
        }],
    )
    .await?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support::{
        create, decisions, publish, trace, waiting_enrollment, workspace,
    };

    async fn person(pool: &PgPool, ws: WorkspaceId, traits: Value) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(ws.as_uuid())
            .bind(traits)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn enrolled(pool: &PgPool, ws: WorkspaceId, person: Uuid, waits_for: &str) -> Uuid {
        let flow = create(pool, ws, "Flow").await;
        publish(pool, ws, flow.id).await.unwrap();
        waiting_enrollment(pool, ws, flow.id, person, waits_for).await
    }

    fn keys(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|key| (*key).to_owned()).collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_subject_carries_the_listed_traits_and_null_for_a_missing_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({"plan": "pro", "other": 1})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut connection = pool.acquire().await.unwrap();

        let subject =
            read_webhook_subject(&mut connection, ws, enrollment, &keys(&["plan", "age"]))
                .await
                .unwrap()
                .expect("the person exists");

        assert_eq!(subject.person, id);
        assert_eq!(
            Value::Object(subject.traits),
            json!({"plan": "pro", "age": null})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_enrollment_reads_as_gone_and_another_workspaces_does_too(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut connection = pool.acquire().await.unwrap();

        let foreign = read_webhook_subject(&mut connection, other, enrollment, &[])
            .await
            .unwrap();
        sqlx::query("DELETE FROM enrollment_waits WHERE enrollment_id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM enrollments WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();
        let erased = read_webhook_subject(&mut connection, ws, enrollment, &[])
            .await
            .unwrap();
        let locked = lock_webhook_person(&mut connection, ws, enrollment, id)
            .await
            .unwrap();

        assert_eq!((foreign, erased), (None, None));
        assert_eq!(locked, WebhookPerson::Gone);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reading_the_subject_takes_no_lock_and_the_person_lock_makes_an_erasure_wait_only(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut holder = crate::begin(&pool).await.unwrap();
        read_webhook_subject(&mut holder, ws, enrollment, &[])
            .await
            .unwrap();
        let mut writer = pool.acquire().await.unwrap();
        sqlx::query("SET lock_timeout = '300ms'")
            .execute(&mut *writer)
            .await
            .unwrap();
        let during_read = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *writer)
            .await;

        let held = lock_webhook_person(&mut holder, ws, enrollment, id)
            .await
            .unwrap();
        let write = sqlx::query("UPDATE people SET traits = '{\"plan\":\"x\"}' WHERE id = $1")
            .bind(id)
            .execute(&mut *writer)
            .await;
        let erase = sqlx::query("DELETE FROM people WHERE id = $1")
            .bind(id)
            .execute(&mut *writer)
            .await;

        assert!(during_read.is_ok(), "a read holds no lock");
        assert_eq!(held, WebhookPerson::Held);
        assert!(write.is_ok(), "a trait write does not wait for the outcome");
        assert!(erase.is_err(), "an erasure waits until the outcome commits");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_outcome_writes_the_event_and_the_decision_and_wakes_a_waiting_enrollment(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, WEBHOOK_SUCCEEDED_EVENT).await;
        let trace = trace();
        let mut connection = pool.acquire().await.unwrap();

        let event = record_webhook_outcome(
            &mut connection,
            &WebhookOutcome {
                workspace: ws,
                enrollment,
                person: id,
                node: "hook",
                result: WebhookResult::Succeeded,
                status: Some(204),
                attempts: 2,
                reason: None,
                trace: &trace,
                now: OffsetDateTime::now_utc(),
                job: (Uuid::now_v7(), 2),
                sync: None,
            },
        )
        .await
        .unwrap();

        let (name, source, properties, trace_id): (String, String, Value, Uuid) =
            sqlx::query_as("SELECT name, source, properties, trace_id FROM events WHERE id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (name.as_str(), source.as_str()),
            ("webhook.succeeded", "flows")
        );
        assert_eq!(
            properties,
            json!({"node_id": "hook", "enrollment_id": enrollment, "status": 204, "attempts": 2})
        );
        assert_eq!(trace_id, trace.trace_id.as_uuid());
        assert_eq!(decisions(&pool, ws, "webhook.call").await, 1);
        let woken: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'flow.step' AND unique_key = $1",
        )
        .bind(format!("event:{enrollment}:{event}"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(woken, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sync_outcome_writes_the_sync_event_and_decision_with_ids_and_codes_only(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({"plan": "SENTINEL-TRAIT"})).await;
        let enrollment = enrolled(&pool, ws, id, "sync.failed").await;
        let destination = Uuid::now_v7();
        let trace = trace();
        let mut connection = pool.acquire().await.unwrap();

        let event = record_webhook_outcome(
            &mut connection,
            &WebhookOutcome {
                workspace: ws,
                enrollment,
                person: id,
                node: "push",
                result: WebhookResult::Failed,
                status: None,
                attempts: 1,
                reason: Some("missing_key"),
                trace: &trace,
                now: OffsetDateTime::now_utc(),
                job: (Uuid::now_v7(), 1),
                sync: Some(SyncDetail {
                    destination,
                    operation: SyncOperation::Upsert,
                }),
            },
        )
        .await
        .unwrap();

        let (name, properties): (String, Value) =
            sqlx::query_as("SELECT name, properties FROM events WHERE id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name, "sync.failed");
        assert_eq!(
            properties,
            json!({
                "node_id": "push",
                "enrollment_id": enrollment,
                "status": null,
                "attempts": 1,
                "destination_id": destination,
                "operation": "upsert"
            })
        );
        let (outcome, reason, ids): (String, Option<String>, Value) = sqlx::query_as(
            "SELECT outcome, reason, subject_ids FROM decision_records WHERE action = 'sync.upsert'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (outcome.as_str(), reason.as_deref()),
            ("failed", Some("missing_key"))
        );
        assert_eq!(ids["destination"], json!(destination));
        assert_eq!(ids["person"], json!(id));
        assert_eq!(decisions(&pool, ws, "webhook.call").await, 0);
        let woken: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(woken, 1, "a wait for sync.failed is woken");
        let everything: String = sqlx::query_scalar(
            "SELECT (SELECT coalesce(string_agg(properties::text, ' '), '') FROM events) || \
                    (SELECT coalesce(string_agg(subject_ids::text || coalesce(reason, ''), ' '), '') \
                     FROM decision_records)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!everything.contains("SENTINEL"), "{everything}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_wakes_only_a_wait_for_the_failed_event(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, WEBHOOK_SUCCEEDED_EVENT).await;
        let trace = trace();
        let mut connection = pool.acquire().await.unwrap();

        record_webhook_outcome(
            &mut connection,
            &WebhookOutcome {
                workspace: ws,
                enrollment,
                person: id,
                node: "hook",
                result: WebhookResult::Failed,
                status: None,
                attempts: 5,
                reason: Some("transport"),
                trace: &trace,
                now: OffsetDateTime::now_utc(),
                job: (Uuid::now_v7(), 5),
                sync: None,
            },
        )
        .await
        .unwrap();

        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (outcome, reason): (String, Option<String>) = sqlx::query_as(
            "SELECT outcome, reason FROM decision_records WHERE action = 'webhook.call'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(jobs, 0);
        assert_eq!(
            (outcome.as_str(), reason.as_deref()),
            ("failed", Some("transport"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_exited_enrollment_and_a_merged_away_person_are_not_called_about(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({"plan": "pro"})).await;
        let survivor = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut connection = pool.acquire().await.unwrap();
        let live = read_webhook_subject(&mut connection, ws, enrollment, &keys(&["plan"]))
            .await
            .unwrap();

        sqlx::query("UPDATE enrollments SET state = 'completed', ended_at = now() WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();
        let completed = read_webhook_subject(&mut connection, ws, enrollment, &[])
            .await
            .unwrap();
        sqlx::query("UPDATE enrollments SET state = 'exited', exit_reason = 'goal', ended_at = now() \n             WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();
        let exited = read_webhook_subject(&mut connection, ws, enrollment, &[])
            .await
            .unwrap();
        sqlx::query("UPDATE enrollments SET state = 'waiting', exit_reason = NULL, ended_at = NULL \n             WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(id)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();
        let merged = read_webhook_subject(&mut connection, ws, enrollment, &[])
            .await
            .unwrap();
        let locked = lock_webhook_person(&mut connection, ws, enrollment, id)
            .await
            .unwrap();

        assert!(live.is_some());
        assert!(
            completed.is_some(),
            "an enrollment that finished still sends"
        );
        assert_eq!(exited, None, "an exited enrollment is not called about");
        assert_eq!(merged, None, "a merged-away person is not called about");
        assert_eq!(locked, WebhookPerson::Moved);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_outcome_event_reaches_the_forwarding_outbox_in_its_transaction(pool: PgPool) {
        use crate::forwarding::support::{add_rule, forward_jobs, outbox_rows, payloads};

        let (ws, destination) = crate::forwarding::support::fixture(&pool, "fwd").await;
        add_rule(
            &pool,
            ws,
            destination,
            WEBHOOK_SUCCEEDED_EVENT,
            json!([]),
            json!({"status": {"from": "event.properties", "pointer": "/status"},
                   "name": {"from": "event.name"}}),
        )
        .await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let trace = trace();
        let outcome = WebhookOutcome {
            workspace: ws,
            enrollment,
            person: id,
            node: "hook",
            result: WebhookResult::Succeeded,
            status: Some(204),
            attempts: 1,
            reason: None,
            trace: &trace,
            now: OffsetDateTime::now_utc(),
            job: (Uuid::now_v7(), 1),
            sync: None,
        };

        let mut tx = crate::begin(&pool).await.unwrap();
        record_webhook_outcome(&mut tx, &outcome).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            (outbox_rows(&pool, ws).await, forward_jobs(&pool).await),
            (0, 0)
        );

        let mut tx = crate::begin(&pool).await.unwrap();
        record_webhook_outcome(&mut tx, &outcome).await.unwrap();
        crate::commit(tx).await.unwrap();
        assert_eq!(
            payloads(&pool, ws).await,
            [json!({"status": 204, "name": "webhook.succeeded"})]
        );
        assert_eq!(forward_jobs(&pool).await, 1);
    }
}
