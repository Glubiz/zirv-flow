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
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::events::{
    EMAIL_BOUNCED_EVENT, EMAIL_FAILED_EVENT, EMAIL_SENT_EVENT, EMAIL_SKIPPED_EVENT,
};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::arrivals::{Arrival, enqueue_arrivals};
use super::delivery::{WebhookPerson, lock_webhook_person};
use super::runtime::find_enrollment;
use crate::messages::{
    COLUMNS, GateRecord, Message, MessageStatus, NewMessage, insert_message,
    insert_message_keeping_gates,
};
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

/// The `message.send` decision; none for a message that has not ended.
const fn outcome_of(status: MessageStatus) -> Option<&'static DecisionSpec> {
    match status {
        MessageStatus::Pending | MessageStatus::Sending => None,
        MessageStatus::Sent => Some(&decision_actions::MESSAGE_SEND_SENT),
        MessageStatus::Skipped => Some(&decision_actions::MESSAGE_SEND_SKIPPED),
        MessageStatus::Failed => Some(&decision_actions::MESSAGE_SEND_FAILED),
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
    /// Whether the gates of `message` are appended to the gate results the row holds instead of
    /// replacing them. Only the leader's orphan sweep sets it.
    pub keep_gates: bool,
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
    let written = if outcome.keep_gates {
        insert_message_keeping_gates(&mut *connection, message).await?
    } else {
        insert_message(&mut *connection, message).await?
    };
    if !written {
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
    crate::forwarding::enqueue_forwarding_of(connection, message.workspace, event, message.now)
        .await?;

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
    // A passed gate writes nothing: `message.send` / `sent` implies every gate passed. A
    // blocked gate keeps its row.
    let mut decisions: Vec<NewDecision> = message
        .gates
        .iter()
        .filter(|gate| !gate.passed)
        .map(|gate| NewDecision {
            graph: graph.clone(),
            workspace_id: message.workspace,
            trace: outcome.trace.clone(),
            spec: &decision_actions::SEND_GATE_BLOCKED,
            detail: Some(gate.gate),
            subject_ids: subject_ids(gate.detail),
            job: outcome.job,
        })
        .collect();
    decisions.push(NewDecision {
        graph,
        workspace_id: message.workspace,
        trace: outcome.trace.clone(),
        spec: result,
        detail: message.reason,
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

/// Why a message the leader ended had no job left to end it: no attempt of it was possibly
/// accepted by the destination.
pub const INTERNAL_ERROR: &str = "internal_error";
/// Why a `sending` message ended without its job: the first call may have been accepted.
pub const DELIVERY_UNCONFIRMED: &str = "delivery_unconfirmed";

/// The note on the `job` gate of a message the sweep ended: its job was dead or gone.
pub const JOB_DEAD: &str = "job_dead";

/// A message the sweep ended, for the caller to log: ids and codes only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EndedOrphan {
    pub message: Uuid,
    pub workspace: Uuid,
    /// The failure code it ended with: [`INTERNAL_ERROR`] or [`DELIVERY_UNCONFIRMED`].
    pub reason: &'static str,
}

/// How many open messages one sweep ends, and how many it examines.
const SWEEP_BATCH: i64 = 100;
const SWEEP_WINDOW: i64 = 20_000;

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
/// retry. The job is found by the message id in its payload, which a deferral copy carries. A
/// send a paused flow holds (`held_sends`, #589) has no job until the resume, and is not open.
const NO_LIVE_JOB: &str = "m.status IN ('pending', 'sending') AND NOT EXISTS ( \
    SELECT 1 FROM jobs j WHERE j.kind = 'delivery.send' AND j.state IN ('ready', 'running') \
    AND j.payload ->> 'message_id' = m.id::text) \
    AND NOT EXISTS (SELECT 1 FROM held_sends h WHERE h.message_id = m.id)";

/// Ends the open messages whose `delivery.send` job is dead or gone, each with its event and
/// decisions through [`record_send_outcome`]. The ending follows the row's marks
/// ([`ending_of`]): `failed: delivery_unconfirmed` when an attempt may have been accepted,
/// `failed: internal_error` when none was. The `job` gate, noted `job_dead`, is appended to the
/// gates the row holds. Returns what it ended, at most one batch.
///
/// A selected row is ended unless it has meanwhile ended or got a live job: neither the person
/// nor the enrollment being gone, merged or erased lets a row stay selected, so a batch of rows
/// that could not be ended never starves newer orphans.
///
/// Each message is ended in a transaction of its own: the person is locked first and then the
/// message row, the order of the job's outcome write, and both the open status and the absence of
/// a live job are checked again under the locks.
pub async fn end_orphaned_messages(
    pool: &PgPool,
    now: OffsetDateTime,
) -> Result<Vec<EndedOrphan>, StoreError> {
    Ok(end_orphaned_messages_from(pool, now, None).await?.ended)
}

/// The place a sweep stopped: the creation time and id of the last open message it passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OrphanCursor {
    created_at: OffsetDateTime,
    id: Uuid,
}

/// What one windowed sweep did.
#[derive(Debug)]
pub struct OrphanSweep {
    /// The messages it ended.
    pub ended: Vec<EndedOrphan>,
    /// Where the next sweep starts; `None` when this one reached the newest open message, so the
    /// next starts over from the oldest.
    pub next: Option<OrphanCursor>,
}

/// [`end_orphaned_messages`] over a window: it examines at most [`SWEEP_WINDOW`] open messages
/// after `cursor` (the oldest first; `None` starts at the oldest) and ends the orphans among
/// them, at most [`SWEEP_BATCH`]. The job probe of an open message costs one index lookup, so a
/// pass over a million open messages every minute was a steady load; the leader keeps the cursor
/// and walks the open messages window by window. An orphan therefore waits up to `open / window`
/// sweeps to be found, which is not time-critical: orphans come from jobs the reaper buried.
pub async fn end_orphaned_messages_from(
    pool: &PgPool,
    now: OffsetDateTime,
    cursor: Option<OrphanCursor>,
) -> Result<OrphanSweep, StoreError> {
    sweep_window(pool, now, cursor, SWEEP_WINDOW).await
}

async fn sweep_window(
    pool: &PgPool,
    now: OffsetDateTime,
    cursor: Option<OrphanCursor>,
    window: i64,
) -> Result<OrphanSweep, StoreError> {
    let (after_at, after_id) = cursor.map_or((OffsetDateTime::UNIX_EPOCH, Uuid::nil()), |c| {
        (c.created_at, c.id)
    });
    let rows: Vec<WindowRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH win AS MATERIALIZED ( \
            SELECT m.id, m.workspace_id, m.person_id, m.enrollment_id, m.created_at, m.status \
            FROM messages m WHERE m.status IN ('pending', 'sending') \
              AND (m.created_at, m.id) > ($2, $3) \
            ORDER BY m.created_at, m.id LIMIT $4), \
         orphans AS ( \
            SELECT m.id, m.workspace_id, m.person_id, m.enrollment_id, m.created_at FROM win m \
            WHERE {NO_LIVE_JOB} ORDER BY m.created_at, m.id LIMIT $1) \
         SELECT o.id, o.workspace_id, o.person_id, o.enrollment_id, o.created_at, \
                (SELECT count(*) FROM win) AS window_rows, \
                (SELECT w.created_at FROM win w ORDER BY w.created_at DESC, w.id DESC LIMIT 1) \
                    AS last_created_at, \
                (SELECT w.id FROM win w ORDER BY w.created_at DESC, w.id DESC LIMIT 1) AS last_id \
         FROM (SELECT 1) one LEFT JOIN orphans o ON true ORDER BY o.created_at, o.id"
    )))
    .bind(SWEEP_BATCH)
    .bind(after_at)
    .bind(after_id)
    .bind(window)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let window_rows = rows.first().map_or(0, |row| row.window_rows);
    let window_end = rows
        .first()
        .and_then(|row| Some((row.last_created_at?, row.last_id?)));
    let found: Vec<(OpenMessage, OffsetDateTime)> = rows
        .iter()
        .filter_map(|row| {
            Some((
                OpenMessage {
                    id: row.id?,
                    workspace_id: row.workspace_id?,
                    person_id: row.person_id?,
                    enrollment_id: row.enrollment_id?,
                },
                row.created_at?,
            ))
        })
        .collect();
    let next = if i64::try_from(found.len()).unwrap_or(i64::MAX) >= SWEEP_BATCH {
        // The batch filled before the window ended: the rest of the window comes next.
        found.last().map(|(open, created_at)| OrphanCursor {
            created_at: *created_at,
            id: open.id,
        })
    } else if window_rows >= window {
        window_end.map(|(created_at, id)| OrphanCursor { created_at, id })
    } else {
        None
    };
    let mut ended = Vec::new();
    for (open, _) in &found {
        if let Some(reason) = end_orphan(pool, open, now).await? {
            ended.push(EndedOrphan {
                message: open.id,
                workspace: open.workspace_id,
                reason,
            });
        }
    }
    Ok(OrphanSweep { ended, next })
}

/// One row of the windowed sweep: an orphan (every column of it is NULL when the window held
/// none) with the facts of the window.
#[derive(sqlx::FromRow)]
struct WindowRow {
    id: Option<Uuid>,
    workspace_id: Option<Uuid>,
    person_id: Option<Uuid>,
    enrollment_id: Option<Uuid>,
    created_at: Option<OffsetDateTime>,
    window_rows: i64,
    last_created_at: Option<OffsetDateTime>,
    last_id: Option<Uuid>,
}

/// Whether a stored `gate_results` carries a `delivery` mark of an attempt that may have been
/// accepted: the in-flight mark (written before every call, cleared by a definitive refusal or a
/// failure before the call) or the unconfirmed mark (a reply lost after the final dot).
fn possibly_accepted(gate_results: &serde_json::Value) -> bool {
    gate_results.as_array().is_some_and(|entries| {
        entries.iter().any(|entry| {
            entry["gate"] == "delivery"
                && (entry["result"] == "in_flight" || entry["result"] == "unconfirmed")
        })
    })
}

/// The failure code the sweep ends an open message with. A `pending` message never reached the
/// destination. A `sending` one ends `delivery_unconfirmed` while a mark says an attempt may have
/// been accepted, and `internal_error` only when neither mark is left (every attempt was refused
/// for good). Never "not delivered" when a call may have gone out. An `internal_error` ending
/// gives the message's cap slot back (nothing went out); `delivery_unconfirmed` keeps it.
///
/// | status    | in-flight | unconfirmed | ends                    |
/// |-----------|-----------|-------------|-------------------------|
/// | `pending` | n/a       | n/a         | `internal_error`        |
/// | `sending` | set       | any         | `delivery_unconfirmed`  |
/// | `sending` | clear     | set         | `delivery_unconfirmed`  |
/// | `sending` | clear     | clear       | `internal_error`        |
fn ending_of(status: &str, gate_results: &serde_json::Value) -> &'static str {
    if status == "sending" && possibly_accepted(gate_results) {
        DELIVERY_UNCONFIRMED
    } else {
        INTERNAL_ERROR
    }
}

/// Ends one open message; the code it ended with, or none when it was not ended because it ended
/// or got a live job since it was selected.
async fn end_orphan(
    pool: &PgPool,
    open: &OpenMessage,
    now: OffsetDateTime,
) -> Result<Option<&'static str>, StoreError> {
    let workspace = WorkspaceId::from_uuid(open.workspace_id);
    let mut tx = begin(pool).await?;
    // Only a person who still holds the enrollment gets the event and the decisions; for a person
    // merged away or erased, or an enrollment that is gone, the row is ended all the same.
    let held = matches!(
        lock_webhook_person(&mut tx, workspace, open.enrollment_id, open.person_id).await?,
        WebhookPerson::Held
    );
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
        return Ok(None);
    };
    let reason = ending_of(&stored.status, &stored.gate_results);
    let dimensions: Dimensions =
        serde_json::from_value(stored.dimensions.clone()).unwrap_or_default();
    let gates = [GateRecord::blocked("job", reason).with_note(JOB_DEAD)];
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
        theme_id: None,
        theme_version: None,
        snippet_versions: None,
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
    let enrollment = if held {
        find_enrollment(&mut tx, workspace, open.enrollment_id).await?
    } else {
        None
    };
    if let Some(enrollment) = enrollment {
        record_send_outcome(
            &mut tx,
            &SendOutcome {
                message,
                trace: &enrollment.trace,
                job: None,
                keep_gates: true,
            },
        )
        .await?;
    } else {
        insert_message_keeping_gates(&mut *tx, &message).await?;
    }
    if reason == INTERNAL_ERROR {
        // Nothing went out, so the cap slot gate 5 reserved goes back with the ending. A
        // `delivery_unconfirmed` row keeps it: the message may have been delivered.
        crate::release_message_slot(&mut *tx, workspace, stored.id).await?;
    }
    commit(tx).await?;
    Ok(Some(reason))
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
        assert_eq!(
            outcome_of(MessageStatus::Sent),
            Some(&decision_actions::MESSAGE_SEND_SENT)
        );
        assert_eq!(
            outcome_of(MessageStatus::Skipped),
            Some(&decision_actions::MESSAGE_SEND_SKIPPED)
        );
        assert_eq!(
            outcome_of(MessageStatus::Failed),
            Some(&decision_actions::MESSAGE_SEND_FAILED)
        );
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
            theme_id: None,
            theme_version: None,
            snippet_versions: None,
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
            keep_gates: false,
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
        // The two gates that passed write nothing; only the blocked one and the send do.
        assert_eq!(decisions.len(), 2, "{decisions:?}");
        assert!(decisions.iter().all(|(_, outcome, _)| outcome != "passed"));
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

    /// Writes a `pending` message of the enrollment with the given gates, as the job does first.
    async fn open_message(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        enrollment: Uuid,
        gates: &[GateRecord],
    ) -> Uuid {
        let dimensions = Dimensions::new();
        let id = Uuid::now_v7();
        let new = NewMessage {
            id,
            workspace: ws,
            person,
            enrollment,
            flow: None,
            flow_version: None,
            node: "mail",
            template: Uuid::from_u128(1),
            template_version: Some(1),
            theme_id: None,
            theme_version: None,
            snippet_versions: None,
            destination: Uuid::from_u128(3),
            sender_identity: None,
            category: Some("marketing"),
            topic: None,
            dimensions: &dimensions,
            status: MessageStatus::Pending,
            reason: None,
            deciding_rule: None,
            gates,
            attempts: 0,
            now: OffsetDateTime::now_utc(),
        };
        assert!(insert_message(pool, &new).await.unwrap());
        id
    }

    async fn stored(pool: &PgPool, ws: WorkspaceId, id: Uuid) -> crate::Message {
        crate::find_message(pool, ws, id).await.unwrap().unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_moved_message_whose_enrollment_was_erased_is_ended_and_blocks_nothing(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        let target = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(target)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let moved = open_message(&pool, ws, person, enrollment, &[]).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(person)
            .bind(target)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM enrollments WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();

        let ended = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
            .await
            .unwrap();

        assert_eq!(
            ended,
            [EndedOrphan {
                message: moved,
                workspace: ws.as_uuid(),
                reason: INTERNAL_ERROR
            }]
        );
        let row = stored(&pool, ws, moved).await;
        assert_eq!(
            (row.status.as_str(), row.reason.as_deref()),
            ("failed", Some("internal_error"))
        );
        let again = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(again.is_empty(), "an ended row is not selected again");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_whose_enrollment_was_erased_under_its_person_is_ended(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        let id = open_message(&pool, ws, person, enrollment, &[]).await;
        sqlx::query("DELETE FROM enrollments WHERE id = $1")
            .bind(enrollment)
            .execute(&pool)
            .await
            .unwrap();

        let ended = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
            .await
            .unwrap();

        assert_eq!(ended.len(), 1);
        assert_eq!(stored(&pool, ws, id).await.status, "failed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_keeps_the_gates_a_message_passed_and_adds_its_job_gate_with_a_note(
        pool: PgPool,
    ) {
        let (ws, person, enrollment) = seeded(&pool).await;
        let gates = [
            GateRecord::passed("enrollment"),
            GateRecord::passed("address"),
            GateRecord::passed("consent"),
        ];
        let id = open_message(&pool, ws, person, enrollment, &gates).await;
        crate::mark_message_sending(&pool, ws, id, 1, None)
            .await
            .unwrap();

        let ended = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
            .await
            .unwrap();

        assert_eq!(ended.len(), 1);
        let row = stored(&pool, ws, id).await;
        assert_eq!(
            (row.status.as_str(), row.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(
            row.gate_results,
            serde_json::json!([
                {"gate": "enrollment", "result": "passed"},
                {"gate": "address", "result": "passed"},
                {"gate": "consent", "result": "passed"},
                {"gate": "job", "result": "blocked", "reason": "delivery_unconfirmed",
                 "note": "job_dead"},
            ]),
            "the passed gates stay, the transient in-flight mark goes, the job gate is noted"
        );
    }

    /// Caps the workspace at one marketing-channel email per day, skipping past it.
    async fn cap_at_one(pool: &PgPool, ws: WorkspaceId) {
        let caps = [crate::NewFrequencyCap {
            channel: "email".to_owned(),
            window_hours: 24,
            limit: 1,
            mode: crate::CapMode::Skip,
        }];
        let mut conn = pool.acquire().await.unwrap();
        crate::replace_send_limits(&mut conn, ws, 0, &caps, None)
            .await
            .unwrap();
    }

    async fn slot_of(pool: &PgPool, id: Uuid) -> Option<OffsetDateTime> {
        sqlx::query_scalar("SELECT cap_slot_at FROM messages WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_orphan_ended_internal_error_gives_its_cap_slot_back(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        cap_at_one(&pool, ws).await;
        let id = open_message(&pool, ws, person, enrollment, &[]).await;
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            crate::check_frequency_caps(&pool, ws, person, id, "email", now)
                .await
                .unwrap(),
            crate::CapVerdict::Reserved
        );
        assert!(slot_of(&pool, id).await.is_some());

        let ended = end_orphaned_messages(&pool, now).await.unwrap();

        assert_eq!(ended.len(), 1);
        let row = stored(&pool, ws, id).await;
        assert_eq!(row.reason.as_deref(), Some("internal_error"));
        assert_eq!(slot_of(&pool, id).await, None, "nothing went out: no slot");
        let next = open_message(&pool, ws, person, enrollment, &[]).await;
        assert_eq!(
            crate::check_frequency_caps(&pool, ws, person, next, "email", now)
                .await
                .unwrap(),
            crate::CapVerdict::Reserved,
            "the person is not capped by a message that never went out"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_orphan_ended_delivery_unconfirmed_keeps_its_cap_slot(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        cap_at_one(&pool, ws).await;
        let id = open_message(&pool, ws, person, enrollment, &[]).await;
        let now = OffsetDateTime::now_utc();
        assert_eq!(
            crate::check_frequency_caps(&pool, ws, person, id, "email", now)
                .await
                .unwrap(),
            crate::CapVerdict::Reserved
        );
        crate::mark_message_sending(&pool, ws, id, 1, None)
            .await
            .unwrap();

        end_orphaned_messages(&pool, now).await.unwrap();

        let row = stored(&pool, ws, id).await;
        assert_eq!(row.reason.as_deref(), Some("delivery_unconfirmed"));
        assert!(slot_of(&pool, id).await.is_some(), "it may have gone out");
        let next = open_message(&pool, ws, person, enrollment, &[]).await;
        assert_eq!(
            crate::check_frequency_caps(&pool, ws, person, next, "email", now)
                .await
                .unwrap(),
            crate::CapVerdict::Skip
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_ending_follows_the_marks_of_the_row(pool: PgPool) {
        let (ws, person, enrollment) = seeded(&pool).await;
        let gates = [GateRecord::passed("enrollment")];
        // pending: no attempt was made.
        let pending = open_message(&pool, ws, person, enrollment, &gates).await;
        // sending, in-flight set.
        let in_flight = open_message(&pool, ws, person, enrollment, &gates).await;
        crate::mark_message_sending(&pool, ws, in_flight, 1, None)
            .await
            .unwrap();
        // sending, in-flight cleared by a definitive refusal, nothing else.
        let refused = open_message(&pool, ws, person, enrollment, &gates).await;
        crate::mark_message_sending(&pool, ws, refused, 1, None)
            .await
            .unwrap();
        crate::clear_message_in_flight(&pool, ws, refused, 1)
            .await
            .unwrap();
        // sending, in-flight cleared but a lost reply left the unconfirmed mark.
        let lost = open_message(&pool, ws, person, enrollment, &gates).await;
        crate::mark_message_sending(&pool, ws, lost, 1, None)
            .await
            .unwrap();
        crate::mark_message_unconfirmed(&pool, ws, lost)
            .await
            .unwrap();
        crate::clear_message_in_flight(&pool, ws, lost, 1)
            .await
            .unwrap();
        // sending, both marks set.
        let both = open_message(&pool, ws, person, enrollment, &gates).await;
        crate::mark_message_sending(&pool, ws, both, 1, None)
            .await
            .unwrap();
        crate::mark_message_unconfirmed(&pool, ws, both)
            .await
            .unwrap();

        let ended = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
            .await
            .unwrap();

        assert_eq!(ended.len(), 5);
        for (id, reason) in [
            (pending, "internal_error"),
            (in_flight, "delivery_unconfirmed"),
            (refused, "internal_error"),
            (lost, "delivery_unconfirmed"),
            (both, "delivery_unconfirmed"),
        ] {
            let row = stored(&pool, ws, id).await;
            assert_eq!(
                (row.status.as_str(), row.reason.as_deref()),
                ("failed", Some(reason)),
                "{id}"
            );
            assert!(
                row.gate_results.to_string().contains("\"job_dead\""),
                "{id}: {}",
                row.gate_results
            );
        }
    }

    #[test]
    fn the_ending_is_unconfirmed_only_while_a_mark_says_a_call_may_have_gone_out() {
        let none = serde_json::json!([{"gate": "enrollment", "result": "passed"}]);
        let in_flight = serde_json::json!([{"gate": "delivery", "result": "in_flight"}]);
        let unconfirmed = serde_json::json!([{"gate": "delivery", "result": "unconfirmed"}]);
        assert_eq!(ending_of("pending", &in_flight), INTERNAL_ERROR);
        assert_eq!(ending_of("sending", &none), INTERNAL_ERROR);
        assert_eq!(ending_of("sending", &in_flight), DELIVERY_UNCONFIRMED);
        assert_eq!(ending_of("sending", &unconfirmed), DELIVERY_UNCONFIRMED);
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
                        theme_id: None,
                        theme_version: None,
                        snippet_versions: None,
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
                    keep_gates: false,
                }
            }
        };

        let mut tx = crate::begin(&pool).await.unwrap();
        record_send_outcome(&mut tx, &write(Uuid::now_v7()).await)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(
            (outbox_rows(&pool, ws).await, forward_jobs(&pool).await),
            (0, 0)
        );

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

    /// Open messages `minutes` apart, the oldest first, none with a job; returns their ids.
    async fn open_messages(
        pool: &PgPool,
        (ws, person, enrollment): (WorkspaceId, Uuid, Uuid),
        count: i32,
    ) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for n in 0..count {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, created_at) \
                 VALUES ($1, $2, $3, $4, 'mail', gen_random_uuid(), gen_random_uuid(), \
                    'pending', now() - make_interval(mins => $5))",
            )
            .bind(id)
            .bind(ws.as_uuid())
            .bind(person)
            .bind(enrollment)
            .bind(count - n)
            .execute(pool)
            .await
            .unwrap();
            ids.push(id);
        }
        ids
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_examines_one_window_a_run_and_the_cursor_walks_on_then_wraps(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let seeded = seeded(&pool).await;
            let ids = open_messages(&pool, seeded, 7).await;
            let now = OffsetDateTime::now_utc();

            // A window of 3 holds the three oldest: they are ended and the cursor stands after them.
            let first = sweep_window(&pool, now, None, 3).await.unwrap();
            let ended =
                |sweep: &OrphanSweep| sweep.ended.iter().map(|e| e.message).collect::<Vec<_>>();
            assert_eq!(ended(&first), ids[..3]);
            assert!(first.next.is_some());
            let open: i64 =
                sqlx::query_scalar("SELECT count(*) FROM messages WHERE status = 'pending'")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(open, 4, "one run does not look past its window");

            // An orphan beyond the first window is found by a later run.
            let second = sweep_window(&pool, now, first.next, 3).await.unwrap();
            assert_eq!(ended(&second), ids[3..6]);
            let third = sweep_window(&pool, now, second.next, 3).await.unwrap();
            assert_eq!(ended(&third), ids[6..]);
            assert_eq!(third.next, None, "a short window means the walk is over");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_with_a_live_job_is_passed_and_the_wrapper_sweeps_from_the_oldest(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let seeded = seeded(&pool).await;
            let ws = seeded.0;
            let ids = open_messages(&pool, seeded, 3).await;
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, state, run_at, unique_key, \
                    cause_kind, cause_id, trace_id) \
                 VALUES (gen_random_uuid(), $1, 'delivery.send', \
                    jsonb_build_object('message_id', $2::text), 'ready', now(), \
                    gen_random_uuid()::text, 'api', 't', gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(ids[1])
            .execute(&pool)
            .await
            .unwrap();

            let sweep = sweep_window(&pool, OffsetDateTime::now_utc(), None, 4)
                .await
                .unwrap();
            let rest = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
                .await
                .unwrap();

            let ended: Vec<Uuid> = sweep.ended.iter().map(|e| e.message).collect();
            assert_eq!(ended, [ids[0], ids[2]]);
            assert_eq!(sweep.next, None);
            assert!(rest.is_empty(), "the live job's message is never ended");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_a_paused_flow_holds_is_not_ended_by_the_sweep(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let seeded = seeded(&pool).await;
            let ws = seeded.0;
            let ids = open_messages(&pool, seeded, 2).await;
            sqlx::query(
                "INSERT INTO held_sends (message_id, workspace_id, flow_id, payload, max_attempts, \
                    unique_key, trace_id, cause_kind, cause_id) \
                 VALUES ($1, $2, gen_random_uuid(), '{}', 5, 'held', gen_random_uuid(), 'api', 't')",
            )
            .bind(ids[0])
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

            let ended = end_orphaned_messages(&pool, OffsetDateTime::now_utc())
                .await
                .unwrap();

            let ended: Vec<Uuid> = ended.iter().map(|e| e.message).collect();
            assert_eq!(ended, [ids[1]], "the held message waits for the resume");
            assert_eq!(stored(&pool, ws, ids[0]).await.status, "pending");
        })
        .await;
    }
}
