//! Events that wake a waiting enrollment (architecture 9.1 and 9.3). The event path of apply-batch
//! collects the events it accepted and, once per batch, asks one query which enrollments wait for
//! them. Each match becomes a `flow.step` job that carries the `state_version` the wait was
//! registered at, so an event that races the wait's timeout leaves one winner: whichever commits
//! first moves the version, and the other job finds it stale.

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use super::runtime::{StepJobInput, StepJobPayload};
use crate::{StoreError, enqueue};

/// An accepted event, as far as the waits care.
#[derive(Clone, Debug)]
pub(crate) struct Arrival {
    pub(crate) event_id: Uuid,
    pub(crate) person_id: Uuid,
    pub(crate) name: String,
}

#[derive(FromRow)]
struct Match {
    event_id: Uuid,
    event_name: String,
    enrollment_id: Uuid,
    wake_version: i64,
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
}

/// Enqueues a `flow.step` for every enrollment waiting for one of `arrivals`, with one query for
/// the whole batch. Property filters are not applied here: the store cannot read the segment AST,
/// so the step job applies them to the stored event before it moves the enrollment. Returns the
/// number of jobs enqueued.
pub(crate) async fn enqueue_arrivals(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    arrivals: &[Arrival],
) -> Result<usize, StoreError> {
    if arrivals.is_empty() {
        return Ok(0);
    }
    let events: Vec<Uuid> = arrivals.iter().map(|a| a.event_id).collect();
    let people: Vec<Uuid> = arrivals.iter().map(|a| a.person_id).collect();
    let names: Vec<&str> = arrivals.iter().map(|a| a.name.as_str()).collect();
    let matches: Vec<Match> = sqlx::query_as(
        "SELECT t.event_id, w.event_name, w.enrollment_id, w.wake_version, \
                e.trace_id, e.cause_kind, e.cause_id \
         FROM unnest($2::uuid[], $3::uuid[], $4::text[]) AS t(event_id, person_id, name) \
         JOIN enrollment_waits w \
           ON w.workspace_id = $1 AND w.person_id = t.person_id AND w.event_name = t.name \
         JOIN enrollments e ON e.id = w.enrollment_id",
    )
    .bind(workspace.as_uuid())
    .bind(&events)
    .bind(&people)
    .bind(&names)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut enqueued = 0;
    for found in matches {
        let Ok(kind) = found.cause_kind.parse::<CauseKind>() else {
            continue;
        };
        let trace = TraceContext {
            trace_id: TraceId::from_uuid(found.trace_id),
            cause: Cause::from_stored(kind, found.cause_id),
        };
        let payload = StepJobPayload {
            workspace,
            enrollment: found.enrollment_id,
            state_version: found.wake_version,
            input: StepJobInput::Event {
                event_id: found.event_id,
                name: found.event_name,
            },
        };
        let key = format!("event:{}:{}", found.enrollment_id, found.event_id);
        if enqueue(&mut *connection, &payload.job(&trace, None, key)).await? {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// An event as the step job filters it: its name, time and (redacted) properties.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEvent {
    pub name: String,
    pub occurred_at: OffsetDateTime,
    pub properties: Value,
}

/// Reads one event of a person.
pub async fn find_event_of_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    event_id: Uuid,
) -> Result<Option<StoredEvent>, StoreError> {
    let row: Option<(String, OffsetDateTime, Value)> = sqlx::query_as(
        "SELECT name, occurred_at, properties FROM events \
         WHERE workspace_id = $1 AND person_id = $2 AND id = $3",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .bind(event_id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|(name, occurred_at, properties)| StoredEvent {
        name,
        occurred_at,
        properties,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support::{create, publish, waiting_enrollment, workspace};

    async fn waiting(pool: &PgPool, ws: WorkspaceId, flow: Uuid, event: &str) -> (Uuid, Uuid) {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        (
            waiting_enrollment(pool, ws, flow, person, event).await,
            person,
        )
    }

    fn arrival(person: Uuid, name: &str) -> Arrival {
        Arrival {
            event_id: Uuid::now_v7(),
            person_id: person,
            name: name.into(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_query_matches_a_batch_and_enqueues_a_job_per_match(pool: PgPool) {
        let ws = workspace(&pool, "arrivals").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let (enrollment, person) = waiting(&pool, ws, flow.id, "order").await;
        let (_, other) = waiting(&pool, ws, flow.id, "refund").await;
        let hit = arrival(person, "order");
        let batch = [
            hit.clone(),
            arrival(person, "refund"),
            arrival(other, "order"),
            arrival(Uuid::now_v7(), "order"),
        ];
        let mut connection = pool.acquire().await.unwrap();

        let enqueued = enqueue_arrivals(&mut connection, ws, &batch).await.unwrap();
        let again = enqueue_arrivals(&mut connection, ws, &batch).await.unwrap();

        assert_eq!(enqueued, 1);
        assert_eq!(again, 0, "the unique key holds the first job");
        let (kind, payload): (String, Value) = sqlx::query_as("SELECT kind, payload FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "flow.step");
        let payload = StepJobPayload::parse(&payload).unwrap();
        assert_eq!(payload.enrollment, enrollment);
        assert_eq!(payload.state_version, 3);
        assert_eq!(
            payload.input,
            StepJobInput::Event {
                event_id: hit.event_id,
                name: "order".into()
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_empty_batch_asks_nothing_and_an_event_is_found_by_its_person(pool: PgPool) {
        let ws = workspace(&pool, "find").await;
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(enqueue_arrivals(&mut connection, ws, &[]).await.unwrap(), 0);
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let event = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                properties, source, trace_id, validation_state) \
             VALUES ($1, $2, $3, 'order', now(), now(), $4, 'web', $5, 'valid')",
        )
        .bind(event)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(json!({"total": 9}))
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();

        let found = find_event_of_person(&mut connection, ws, person, event)
            .await
            .unwrap()
            .unwrap();
        let other = find_event_of_person(&mut connection, ws, Uuid::now_v7(), event)
            .await
            .unwrap();

        assert_eq!(found.name, "order");
        assert_eq!(found.properties, json!({"total": 9}));
        assert!(other.is_none());
    }
}
