//! The `email.opened` and `email.clicked` events, and the write they share with
//! `email.unsubscribed`.

use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use super::TrackedMessage;
use crate::flows::{Arrival, enqueue_arrivals};
use crate::{StoreError, begin, commit};

/// The source of the events tracking writes.
const SOURCE: &str = "tracking";

/// One engagement to record on a message.
#[derive(Clone, Debug)]
pub struct Engagement<'a> {
    /// The event name: `email.opened` or `email.clicked`.
    pub name: &'static str,
    /// Properties besides the ids every engagement event carries (`machine`, `link_index`).
    /// Never a URL, an address, an IP or a user agent.
    pub properties: Map<String, Value>,
    pub now: OffsetDateTime,
    /// The trace to write under when the message's enrollment has none to give.
    pub fallback_trace: &'a TraceContext,
}

/// The trace of the enrollment a message was sent for, with the cause that started it; none when
/// the enrollment is gone.
async fn enrollment_trace(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
) -> Result<Option<TraceContext>, StoreError> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT trace_id, cause_kind, cause_id FROM enrollments \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(enrollment)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.and_then(|(trace, kind, id)| {
        let kind = kind.parse::<CauseKind>().ok()?;
        Some(TraceContext {
            trace_id: TraceId::from_uuid(trace),
            cause: Cause::from_stored(kind, id),
        })
    }))
}

/// The trace to write a message's rows under: its enrollment's, or `fallback` when that is gone.
pub(super) async fn message_trace(
    connection: &mut PgConnection,
    message: &TrackedMessage,
    fallback: &TraceContext,
) -> Result<TraceContext, StoreError> {
    Ok(
        enrollment_trace(connection, message.workspace(), message.enrollment_id)
            .await?
            .unwrap_or_else(|| fallback.clone()),
    )
}

/// Writes the event of `person` for `message` and wakes what waits for it. The caller holds the
/// person's lock.
pub(super) async fn write_event(
    connection: &mut PgConnection,
    message: &TrackedMessage,
    person: Uuid,
    name: &'static str,
    extra: Map<String, Value>,
    now: OffsetDateTime,
    trace: &TraceContext,
) -> Result<Uuid, StoreError> {
    let event = Uuid::now_v7();
    let mut properties = json!({
        "message_id": message.id,
        "enrollment_id": message.enrollment_id,
        "template_id": message.template_id,
    });
    if let Some(object) = properties.as_object_mut() {
        object.extend(extra);
    }
    sqlx::query(
        "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
            properties, source, trace_id, validation_state) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, 'valid')",
    )
    .bind(event)
    .bind(message.workspace_id)
    .bind(person)
    .bind(name)
    .bind(now)
    .bind(&properties)
    .bind(SOURCE)
    .bind(trace.trace_id.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    enqueue_arrivals(
        connection,
        message.workspace(),
        &[Arrival {
            event_id: event,
            person_id: person,
            name: name.to_owned(),
        }],
    )
    .await?;
    Ok(event)
}

/// Records the engagement as an event of the message's person, in a transaction of its own.
/// Returns the event's id, or none when nothing may be written: the person was erased, or merged
/// away (the message stays on the id it was sent to, and an event written there would sit on an id
/// the merge's rewrite has passed).
pub async fn record_engagement(
    pool: &sqlx::PgPool,
    message: &TrackedMessage,
    engagement: Engagement<'_>,
) -> Result<Option<Uuid>, StoreError> {
    let mut tx = begin(pool).await?;
    let locked: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(message.workspace_id)
    .bind(message.person_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    if !matches!(locked, Some(None)) {
        return Ok(None);
    }
    let trace = message_trace(&mut tx, message, engagement.fallback_trace).await?;
    let event = write_event(
        &mut tx,
        message,
        message.person_id,
        engagement.name,
        engagement.properties,
        engagement.now,
        &trace,
    )
    .await?;
    commit(tx).await?;
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::*;
    use crate::flows::tests_support::{trace, workspace};
    use crate::tracking::find_tracked_message;
    use crate::tracking::links::tests::message;

    fn open(trace: &TraceContext) -> Engagement<'_> {
        let mut properties = Map::new();
        properties.insert("machine".to_owned(), json!(false));
        Engagement {
            name: "email.opened",
            properties,
            now: datetime!(2031-05-06 07:08:09 UTC),
            fallback_trace: trace,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_engagement_writes_one_event_of_ids_and_the_given_properties(pool: PgPool) {
        let ws = workspace(&pool, "engage").await;
        let (person, id) = message(&pool, ws).await;
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();
        let trace = trace();

        let event = record_engagement(&pool, &tracked, open(&trace))
            .await
            .unwrap();

        assert!(event.is_some());
        let (name, owner, properties, source): (String, Uuid, Value, String) =
            sqlx::query_as("SELECT name, person_id, properties, source FROM events WHERE id = $1")
                .bind(event)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (name.as_str(), owner, source.as_str()),
            ("email.opened", person, "tracking")
        );
        assert_eq!(properties["machine"], json!(false));
        assert_eq!(properties["message_id"], json!(id));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn nothing_is_written_for_a_person_merged_away(pool: PgPool) {
        let ws = workspace(&pool, "engage2").await;
        let (person, id) = message(&pool, ws).await;
        let (survivor, _) = message(&pool, ws).await;
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(person)
            .execute(&pool)
            .await
            .unwrap();
        let tracked = find_tracked_message(&pool, id).await.unwrap().unwrap();

        let event = record_engagement(&pool, &tracked, open(&trace()))
            .await
            .unwrap();

        assert_eq!(event, None);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0);
    }
}
