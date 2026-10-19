//! What a `webhook.call` job reads and writes besides the call itself (architecture 9.1): the
//! person and traits to send, and the outcome event with its decision. Every function runs in the
//! job's transaction.

use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::events::{WEBHOOK_FAILED_EVENT, WEBHOOK_SUCCEEDED_EVENT};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::arrivals::{Arrival, enqueue_arrivals};
use crate::{NewDecision, StoreError, append_decision, find_enrollment, find_person};

/// The source of the events a flow writes.
const FLOWS_SOURCE: &str = "flows";

/// The person a webhook call is for, read under a lock that keeps them from being erased until
/// the transaction ends.
#[derive(Clone, Debug, PartialEq)]
pub enum WebhookSubject {
    /// The enrollment and its person exist. `traits` holds each requested key at the person's
    /// effective value, `null` when they have none.
    Present {
        person: Uuid,
        traits: Map<String, Value>,
    },
    /// The enrollment is gone: its person was erased, and the call must not be made.
    Gone,
    /// The enrollment moved to another person while this read ran (a merge); a retry reads it
    /// again.
    Moved,
}

/// Locks the person of `enrollment` with `FOR KEY SHARE` and reads `trait_keys`.
///
/// The lock conflicts with the delete of person erasure and with nothing else a step does, so an
/// erasure waits for the call and a trait write does not. It is taken on the person alone, the
/// first of the person-then-enrollment order (#159); the enrollment is only read, so a step of the
/// same enrollment never waits for a call in flight.
pub async fn lock_webhook_subject(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    trait_keys: &[String],
) -> Result<WebhookSubject, StoreError> {
    let Some(found) = find_enrollment(connection, workspace, enrollment).await? else {
        return Ok(WebhookSubject::Gone);
    };
    let locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM people WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(found.person_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if locked.is_none() {
        return Ok(WebhookSubject::Gone);
    }
    // The erasure that won the lock before us deleted the enrollment; one that comes later waits.
    match find_enrollment(connection, workspace, enrollment).await? {
        None => return Ok(WebhookSubject::Gone),
        Some(again) if again.person_id != found.person_id => return Ok(WebhookSubject::Moved),
        Some(_) => {}
    }
    let person = find_person(&mut *connection, workspace, found.person_id)
        .await?
        .ok_or_else(|| StoreError::query(sqlx::Error::RowNotFound))?;
    let held = person.traits.as_object();
    let traits = trait_keys
        .iter()
        .map(|key| {
            let value = held.and_then(|held| held.get(key)).cloned();
            (key.clone(), value.unwrap_or(Value::Null))
        })
        .collect();
    Ok(WebhookSubject::Present {
        person: found.person_id,
        traits,
    })
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
    const fn event(self) -> &'static str {
        match self {
            Self::Succeeded => WEBHOOK_SUCCEEDED_EVENT,
            Self::Failed => WEBHOOK_FAILED_EVENT,
        }
    }

    const fn outcome(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// One finished webhook call.
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
}

/// Records the outcome: the `webhook.succeeded` or `webhook.failed` event of the person (source
/// `flows`, properties the node and enrollment ids, the status and the attempt count, never the
/// response), a `webhook.call` decision under the enrollment's trace, and the steps of every
/// enrollment that waits for the event. Returns the event's id.
pub async fn record_webhook_outcome(
    connection: &mut PgConnection,
    outcome: &WebhookOutcome<'_>,
) -> Result<Uuid, StoreError> {
    let event = Uuid::now_v7();
    let properties = json!({
        "node_id": outcome.node,
        "enrollment_id": outcome.enrollment,
        "status": outcome.status,
        "attempts": outcome.attempts,
    });
    sqlx::query(
        "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
            properties, source, trace_id, validation_state) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, 'valid')",
    )
    .bind(event)
    .bind(outcome.workspace.as_uuid())
    .bind(outcome.person)
    .bind(outcome.result.event())
    .bind(outcome.now)
    .bind(&properties)
    .bind(FLOWS_SOURCE)
    .bind(outcome.trace.trace_id.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    append_decision(
        &mut *connection,
        &NewDecision {
            workspace_id: outcome.workspace,
            trace: outcome.trace.clone(),
            action: "webhook.call",
            outcome: outcome.result.outcome(),
            reason: outcome.reason,
            subject_ids: vec![("enrollment", outcome.enrollment), ("event", event)],
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
            name: outcome.result.event().to_owned(),
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
            lock_webhook_subject(&mut connection, ws, enrollment, &keys(&["plan", "age"]))
                .await
                .unwrap();

        let WebhookSubject::Present { person, traits } = subject else {
            panic!("the person exists");
        };
        assert_eq!(person, id);
        assert_eq!(Value::Object(traits), json!({"plan": "pro", "age": null}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_enrollment_reads_as_gone_and_another_workspaces_does_too(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut connection = pool.acquire().await.unwrap();

        let foreign = lock_webhook_subject(&mut connection, other, enrollment, &[])
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
        let erased = lock_webhook_subject(&mut connection, ws, enrollment, &[])
            .await
            .unwrap();

        assert_eq!(
            (foreign, erased),
            (WebhookSubject::Gone, WebhookSubject::Gone)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_person_lock_makes_an_erasure_wait_and_a_trait_write_not(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let id = person(&pool, ws, json!({})).await;
        let enrollment = enrolled(&pool, ws, id, "never").await;
        let mut holder = crate::begin(&pool).await.unwrap();
        lock_webhook_subject(&mut holder, ws, enrollment, &[])
            .await
            .unwrap();

        let mut writer = pool.acquire().await.unwrap();
        sqlx::query("SET lock_timeout = '300ms'")
            .execute(&mut *writer)
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

        assert!(write.is_ok(), "a trait write does not wait for the call");
        assert!(erase.is_err(), "an erasure waits until the call ends");
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
}
