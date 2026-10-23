//! What a `delivery.send` job writes when a message has ended (architecture 10.1): the `messages`
//! row, the `email.sent`, `email.skipped` or `email.failed` event of the person, a decision per
//! gate that ran and one for the message, and the steps of every enrollment that waits for the
//! event. It runs in the job's transaction, after the person was locked
//! ([`lock_webhook_person`](super::lock_webhook_person)), like [`record_webhook_outcome`]
//! (super::record_webhook_outcome).
//!
//! Nothing here carries an address: the event holds ids and codes, and so do the decisions.

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::TraceContext;
use zirv_flow_core::events::{EMAIL_FAILED_EVENT, EMAIL_SENT_EVENT, EMAIL_SKIPPED_EVENT};

use super::arrivals::{Arrival, enqueue_arrivals};
use crate::messages::{MessageStatus, NewMessage, insert_message};
use crate::{GraphPoint, NewDecision, StoreError, append_decisions};

/// The kind of the job that sends one message.
pub const SEND_JOB_KIND: &str = "delivery.send";

/// The source of the events a flow writes.
const FLOWS_SOURCE: &str = "flows";

const fn event_of(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Sent => EMAIL_SENT_EVENT,
        MessageStatus::Skipped => EMAIL_SKIPPED_EVENT,
        MessageStatus::Failed => EMAIL_FAILED_EVENT,
    }
}

const fn outcome_of(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Sent => "sent",
        MessageStatus::Skipped => "skipped",
        MessageStatus::Failed => "failed",
    }
}

/// One finished message.
#[derive(Clone, Debug)]
pub struct SendOutcome<'a> {
    pub message: NewMessage<'a>,
    /// The trace of the enrollment.
    pub trace: &'a TraceContext,
    /// The job and attempt that decided it.
    pub job: (Uuid, i32),
}

/// Records the message, its event and its decisions. Returns the event's id, or none when a
/// message of this id was recorded already, in which case nothing else is written: the job that
/// committed its outcome and was run again changes nothing.
///
/// The decisions are one `send.gate` per gate that ran (`passed` or `blocked`, the gate as the
/// reason) and the `message.send` decision (`sent`, `skipped` or `failed`, with the skip or
/// failure code), all under the enrollment's trace. They name the message, destination, template,
/// enrollment and person ids.
pub async fn record_send_outcome(
    connection: &mut PgConnection,
    outcome: &SendOutcome<'_>,
) -> Result<Option<Uuid>, StoreError> {
    let message = &outcome.message;
    if !insert_message(connection, message).await? {
        return Ok(None);
    }
    let name = event_of(message.status);
    let event = Uuid::now_v7();
    let properties = json!({
        "message_id": message.id,
        "node_id": message.node,
        "enrollment_id": message.enrollment,
        "template_id": message.template,
        "destination_id": message.destination,
        "reason": message.reason,
        "attempts": message.attempts,
    });
    sqlx::query(
        "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
            properties, source, trace_id, validation_state) \
         VALUES ($1, $2, $3, $4, $5, $5, $6, $7, $8, 'valid')",
    )
    .bind(event)
    .bind(message.workspace.as_uuid())
    .bind(message.person)
    .bind(name)
    .bind(message.now)
    .bind(&properties)
    .bind(FLOWS_SOURCE)
    .bind(outcome.trace.trace_id.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    let subject_ids = |extra: Option<(&'static str, Uuid)>| {
        let mut ids = vec![
            ("message", message.id),
            ("enrollment", message.enrollment),
            ("person", message.person),
            ("template", message.template),
            ("destination", message.destination),
            ("event", event),
        ];
        ids.extend(message.flow.map(|flow| ("flow", flow)));
        ids.extend(extra);
        ids
    };
    let graph = message
        .flow_version
        .map(|version| GraphPoint::at(version, message.node));
    let mut decisions: Vec<NewDecision> = message
        .gates
        .iter()
        .map(|gate| NewDecision {
            graph: graph.clone(),
            workspace_id: message.workspace,
            trace: outcome.trace.clone(),
            action: "send.gate",
            outcome: if gate.passed { "passed" } else { "blocked" },
            reason: Some(gate.gate),
            subject_ids: subject_ids(gate.detail),
            job: Some(outcome.job),
        })
        .collect();
    decisions.push(NewDecision {
        graph,
        workspace_id: message.workspace,
        trace: outcome.trace.clone(),
        action: "message.send",
        outcome: outcome_of(message.status),
        reason: message.reason,
        subject_ids: subject_ids(None),
        job: Some(outcome.job),
    });
    append_decisions(&mut *connection, &decisions).await?;
    enqueue_arrivals(
        connection,
        message.workspace,
        &[Arrival {
            event_id: event,
            person_id: message.person,
            name: name.to_owned(),
        }],
    )
    .await?;
    Ok(Some(event))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::WorkspaceId;
    use zirv_flow_core::consent::Dimensions;

    use super::*;
    use crate::flows::tests_support::{create, publish, trace, waiting_enrollment, workspace};
    use crate::messages::GateRecord;

    #[test]
    fn each_status_has_its_own_event_name_and_outcome() {
        assert_eq!(event_of(MessageStatus::Sent), "email.sent");
        assert_eq!(event_of(MessageStatus::Skipped), "email.skipped");
        assert_eq!(event_of(MessageStatus::Failed), "email.failed");
        assert_eq!(outcome_of(MessageStatus::Sent), "sent");
        assert_eq!(outcome_of(MessageStatus::Skipped), "skipped");
        assert_eq!(outcome_of(MessageStatus::Failed), "failed");
        for status in [
            MessageStatus::Sent,
            MessageStatus::Skipped,
            MessageStatus::Failed,
        ] {
            assert!(zirv_flow_core::events::is_reserved_event_name(event_of(
                status
            )));
        }
    }

    async fn seeded(pool: &PgPool) -> (WorkspaceId, Uuid, Uuid) {
        let ws = workspace(pool, "one").await;
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        let flow = create(pool, ws, "Flow").await;
        publish(pool, ws, flow.id).await.unwrap();
        let enrollment = waiting_enrollment(pool, ws, flow.id, person, "never").await;
        (ws, person, enrollment)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_outcome_writes_the_message_event_and_decisions_once_and_no_address(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        let dimensions = Dimensions::new();
        let message = Uuid::now_v7();
        let gates = [
            GateRecord::passed("enrollment"),
            GateRecord::passed("address"),
            GateRecord::blocked("consent", "no_consent"),
        ];
        let new = NewMessage {
            id: message,
            workspace: ws,
            person,
            enrollment,
            flow: None,
            flow_version: None,
            node: "mail",
            template: Uuid::from_u128(1),
            template_version: Some(2),
            destination: Uuid::from_u128(3),
            sender_identity: None,
            category: Some("marketing"),
            topic: None,
            dimensions: &dimensions,
            status: MessageStatus::Skipped,
            reason: Some("no_consent"),
            deciding_rule: Some("topic_default"),
            gates: &gates,
            attempts: 0,
            now: OffsetDateTime::now_utc(),
        };
        let trace = trace();
        let outcome = SendOutcome {
            message: new,
            trace: &trace,
            job: (Uuid::from_u128(9), 1),
        };
        let mut connection = pool.acquire().await.unwrap();

        let first = record_send_outcome(&mut connection, &outcome)
            .await
            .unwrap();
        let again = record_send_outcome(&mut connection, &outcome)
            .await
            .unwrap();

        assert!(first.is_some());
        assert_eq!(again, None, "a recorded message changes nothing");
        let stored = crate::find_message(&pool, ws, message)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, "skipped");
        assert_eq!(stored.reason.as_deref(), Some("no_consent"));
        assert_eq!(stored.deciding_rule.as_deref(), Some("topic_default"));
        assert_eq!(stored.gate_results.as_array().map(Vec::len), Some(3));
        assert!(!stored.test);
        let events: Vec<(String, serde_json::Value)> = sqlx::query_as(
            "SELECT name, properties FROM events WHERE workspace_id = $1 AND person_id = $2",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "email.skipped");
        assert_eq!(events[0].1["message_id"], serde_json::json!(message));
        let decisions: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT action, outcome, reason FROM decision_records \
             WHERE workspace_id = $1 AND action IN ('send.gate', 'message.send') \
             ORDER BY recorded_at, action DESC, reason",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(decisions.len(), 4, "{decisions:?}");
        assert!(decisions.contains(&(
            "send.gate".into(),
            "blocked".into(),
            Some("consent".into())
        )));
        assert!(decisions.contains(&(
            "message.send".into(),
            "skipped".into(),
            Some("no_consent".into())
        )));
        let everything: String = sqlx::query_scalar(
            "SELECT (SELECT string_agg(m::text, '') FROM messages m) \
                 || (SELECT string_agg(e.properties::text, '') FROM events e) \
                 || (SELECT string_agg(d.subject_ids::text, '') FROM decision_records d)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!everything.contains('@'), "no address is written");
    }
}
