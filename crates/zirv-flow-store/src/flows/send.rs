//! What a `delivery.send` job writes when a message has ended (architecture 10.1): the `messages`
//! row, the `email.sent`, `email.skipped` or `email.failed` event of the person, a decision per
//! gate that ran and one for the message, and the steps of every enrollment that waits for the
//! event. It runs in the job's transaction, after the person was locked
//! ([`lock_webhook_person`](super::lock_webhook_person)), like [`record_webhook_outcome`]
//! (super::record_webhook_outcome).
//!
//! Nothing here carries an address: the event holds ids and codes, and so do the decisions.

use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::consent::Dimensions;
use zirv_flow_core::events::{
    EMAIL_BOUNCED_EVENT, EMAIL_FAILED_EVENT, EMAIL_SENT_EVENT, EMAIL_SKIPPED_EVENT,
};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::arrivals::{Arrival, enqueue_arrivals};
use super::delivery::{WebhookPerson, lock_webhook_person};
use super::runtime::find_enrollment;
use crate::messages::{COLUMNS, GateRecord, Message, MessageStatus, NewMessage, insert_message};
use crate::{GraphPoint, NewDecision, StoreError, append_decisions};
use crate::{begin, commit};

/// The kind of the job that sends one message.
pub const SEND_JOB_KIND: &str = "delivery.send";

/// The source of the events a flow writes.
const FLOWS_SOURCE: &str = "flows";

/// The failure code of a message a mail server refused the recipient of for good (OQ-12). The
/// message ends `failed` with this reason, writes `email.bounced` (hard), and the address is
/// suppressed in every workspace of the organization.
pub const HARD_BOUNCE: &str = "hard_bounce";
/// The failure code of a message whose temporary refusals outlasted the retries. It writes
/// `email.bounced` (soft) and suppresses nobody.
pub const SOFT_BOUNCE: &str = "soft_bounce";

/// The bounce kind a failure reason stands for, if it is one.
fn bounce_of(reason: Option<&str>) -> Option<&'static str> {
    match reason {
        Some(HARD_BOUNCE) => Some("hard"),
        Some(SOFT_BOUNCE) => Some("soft"),
        _ => None,
    }
}

/// The event an ended message writes; none for a message that has not ended. A message that failed
/// as a bounce writes `email.bounced` in place of `email.failed`.
fn event_of(status: MessageStatus, reason: Option<&str>) -> Option<&'static str> {
    match status {
        MessageStatus::Pending | MessageStatus::Sending => None,
        MessageStatus::Sent => Some(EMAIL_SENT_EVENT),
        MessageStatus::Skipped => Some(EMAIL_SKIPPED_EVENT),
        MessageStatus::Failed if bounce_of(reason).is_some() => Some(EMAIL_BOUNCED_EVENT),
        MessageStatus::Failed => Some(EMAIL_FAILED_EVENT),
    }
}

/// The outcome of the `message.send` decision; none for a message that has not ended.
const fn outcome_of(status: MessageStatus) -> Option<&'static str> {
    match status {
        MessageStatus::Pending | MessageStatus::Sending => None,
        MessageStatus::Sent => Some("sent"),
        MessageStatus::Skipped => Some("skipped"),
        MessageStatus::Failed => Some("failed"),
    }
}

/// One finished message.
#[derive(Clone, Debug)]
pub struct SendOutcome<'a> {
    pub message: NewMessage<'a>,
    /// The trace of the enrollment.
    pub trace: &'a TraceContext,
    /// The job and attempt that decided it; none for a message the leader ended because its job
    /// was gone.
    pub job: Option<(Uuid, i32)>,
}

/// Ends the message: writes its outcome over the row the job started, and its event and its
/// decisions. Returns the event's id, or none when the message had ended already (or `outcome` is
/// not an end), in which case nothing is written: the job that committed its outcome and was run
/// again changes nothing.
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
    let (Some(name), Some(result)) = (
        event_of(message.status, message.reason),
        outcome_of(message.status),
    ) else {
        return Ok(None);
    };
    if !insert_message(&mut *connection, message).await? {
        return Ok(None);
    }
    let event = Uuid::now_v7();
    let mut properties = json!({
        "message_id": message.id,
        "node_id": message.node,
        "enrollment_id": message.enrollment,
        "template_id": message.template,
        "destination_id": message.destination,
        "reason": message.reason,
        "attempts": message.attempts,
    });
    if let (Some(bounce), Some(object)) = (bounce_of(message.reason), properties.as_object_mut()) {
        object.insert("bounce".to_owned(), json!(bounce));
    }
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
    crate::enqueue_forwarding(
        connection,
        message.workspace,
        &[crate::ForwardRef {
            id: event,
            occurred_at: message.now,
        }],
    )
    .await?;

    let subject_ids =|extra: Option<(&'static str, Uuid)>| {
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
            job: outcome.job,
        })
        .collect();
    decisions.push(NewDecision {
        graph,
        workspace_id: message.workspace,
        trace: outcome.trace.clone(),
        action: "message.send",
        outcome: result,
        reason: message.reason,
        subject_ids: subject_ids(None),
        job: outcome.job,
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

/// Why a message the leader ended had no job left to end it: it never reached the destination.
pub const INTERNAL_ERROR: &str = "internal_error";
/// Why a `sending` message ended without its job: the first call may have been accepted.
pub const DELIVERY_UNCONFIRMED: &str = "delivery_unconfirmed";

/// How many open messages one sweep ends.
const SWEEP_BATCH: i64 = 100;

/// A message the sweep found open.
#[derive(sqlx::FromRow)]
struct OpenMessage {
    id: Uuid,
    workspace_id: Uuid,
    person_id: Uuid,
    enrollment_id: Uuid,
}

/// The `messages` row that is open and has no live `delivery.send` job. A running job counts as
/// live, even with an expired lease, until the reaper buries it, so the sweep never races a
/// retry. The job is found by the message id in its payload, which a deferral copy carries.
const NO_LIVE_JOB: &str = "m.status IN ('pending', 'sending') AND NOT EXISTS ( \
    SELECT 1 FROM jobs j WHERE j.kind = 'delivery.send' AND j.state IN ('ready', 'running') \
    AND j.payload ->> 'message_id' = m.id::text)";

/// Ends the open messages whose `delivery.send` job is dead or gone: a `pending` one `failed:
/// internal_error`, a `sending` one `failed: delivery_unconfirmed`, each with its event and
/// decisions through [`record_send_outcome`]. Returns the ids it ended, at most one batch.
///
/// Each message is ended in a transaction of its own: the person is locked first and then the
/// message row, the order of the job's outcome write, and both the open status and the absence of
/// a live job are checked again under the locks.
pub async fn end_orphaned_messages(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<Vec<Uuid>, StoreError> {
    let found: Vec<OpenMessage> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT m.id, m.workspace_id, m.person_id, m.enrollment_id FROM messages m \
         WHERE {NO_LIVE_JOB} ORDER BY m.created_at, m.id LIMIT $1"
    )))
    .bind(SWEEP_BATCH)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let mut ended = Vec::new();
    for open in found {
        if end_orphan(pool, &open, now).await? {
            ended.push(open.id);
        }
    }
    Ok(ended)
}

async fn end_orphan(
    pool: &PgPool,
    open: &OpenMessage,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let workspace = WorkspaceId::from_uuid(open.workspace_id);
    let mut tx = begin(pool).await?;
    let held =
        match lock_webhook_person(&mut tx, workspace, open.enrollment_id, open.person_id).await? {
            WebhookPerson::Held => true,
            WebhookPerson::Moved => false,
            WebhookPerson::Gone => return Ok(false),
        };
    let locked: Option<Message> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM messages m WHERE m.workspace_id = $1 AND m.id = $2 AND {NO_LIVE_JOB} \
         FOR UPDATE OF m"
    )))
    .bind(open.workspace_id)
    .bind(open.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let Some(stored) = locked else {
        return Ok(false);
    };
    let Some(enrollment) = find_enrollment(&mut tx, workspace, open.enrollment_id).await? else {
        return Ok(false);
    };
    let reason = if stored.status == "sending" {
        DELIVERY_UNCONFIRMED
    } else {
        INTERNAL_ERROR
    };
    let dimensions: Dimensions =
        serde_json::from_value(stored.dimensions.clone()).unwrap_or_default();
    let gates = [GateRecord::blocked("job", reason)];
    let message = NewMessage {
        id: stored.id,
        workspace,
        person: stored.person_id,
        enrollment: stored.enrollment_id,
        flow: stored.flow_id,
        flow_version: stored.flow_version,
        node: &stored.node_id,
        template: stored.template_id,
        template_version: stored.template_version,
        destination: stored.destination_id,
        sender_identity: stored.sender_identity_id,
        category: stored.category.as_deref(),
        topic: stored.topic_id,
        dimensions: &dimensions,
        status: MessageStatus::Failed,
        reason: Some(reason),
        deciding_rule: stored.deciding_rule.as_deref(),
        gates: &gates,
        attempts: stored.attempts,
        now,
    };
    if held {
        record_send_outcome(
            &mut tx,
            &SendOutcome {
                message,
                trace: &enrollment.trace,
                job: None,
            },
        )
        .await?;
    } else {
        insert_message(&mut *tx, &message).await?;
    }
    commit(tx).await?;
    Ok(true)
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
        assert_eq!(event_of(MessageStatus::Sent, None), Some("email.sent"));
        assert_eq!(
            event_of(MessageStatus::Skipped, None),
            Some("email.skipped")
        );
        assert_eq!(
            event_of(MessageStatus::Failed, Some("render_error")),
            Some("email.failed")
        );
        assert_eq!(
            event_of(MessageStatus::Failed, Some(HARD_BOUNCE)),
            Some("email.bounced")
        );
        assert_eq!(
            event_of(MessageStatus::Failed, Some(SOFT_BOUNCE)),
            Some("email.bounced")
        );
        assert_eq!(bounce_of(Some(HARD_BOUNCE)), Some("hard"));
        assert_eq!(bounce_of(Some(SOFT_BOUNCE)), Some("soft"));
        assert_eq!(bounce_of(Some("delivery_unconfirmed")), None);
        assert_eq!(outcome_of(MessageStatus::Sent), Some("sent"));
        assert_eq!(outcome_of(MessageStatus::Skipped), Some("skipped"));
        assert_eq!(outcome_of(MessageStatus::Failed), Some("failed"));
        assert_eq!(event_of(MessageStatus::Pending, None), None);
        assert_eq!(outcome_of(MessageStatus::Pending), None);
        for status in [
            MessageStatus::Sent,
            MessageStatus::Skipped,
            MessageStatus::Failed,
        ] {
            assert!(zirv_flow_core::events::is_reserved_event_name(
                event_of(status, None).unwrap()
            ));
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
            job: Some((Uuid::from_u128(9), 1)),
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_message_event_reaches_the_forwarding_outbox_in_its_transaction(pool: PgPool) {
        use crate::forwarding::support::{
            add_rule, destination_in, forward_jobs, outbox_rows, payloads,
        };

        let (ws, person, enrollment) = seeded(&pool).await;
        let destination = destination_in(&pool, ws).await;
        add_rule(
            &pool,
            ws,
            destination,
            "email.skipped",
            serde_json::json!([]),
            serde_json::json!({"reason": {"from": "event.properties", "pointer": "/reason"}}),
        )
        .await;
        let dimensions = Dimensions::new();
        let trace = trace();
        let write = |message: Uuid| {
            let dimensions = &dimensions;
            let trace = &trace;
            async move {
                SendOutcome {
                    message: NewMessage {
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
                        dimensions,
                        status: MessageStatus::Skipped,
                        reason: Some("no_consent"),
                        deciding_rule: Some("topic_default"),
                        gates: &[],
                        attempts: 0,
                        now: OffsetDateTime::now_utc(),
                    },
                    trace,
                    job: Some((Uuid::from_u128(9), 1)),
                }
            }
        };

        let mut tx = crate::begin(&pool).await.unwrap();
        record_send_outcome(&mut tx, &write(Uuid::now_v7()).await)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!((outbox_rows(&pool, ws).await, forward_jobs(&pool).await), (0, 0));

        let mut tx = crate::begin(&pool).await.unwrap();
        record_send_outcome(&mut tx, &write(Uuid::now_v7()).await)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        assert_eq!(
            payloads(&pool, ws).await,
            [serde_json::json!({"reason": "no_consent"})]
        );
        assert_eq!(forward_jobs(&pool).await, 1);
    }
}
