//! The event path of apply-batch (architecture sections 12.1 and 12.2): drop a repeated
//! `messageId`, validate against the published schema or the unplanned-name policy, then
//! store an ingest reject (naming the person only if the identifiers already belong to one)
//! or resolve the person and store a redacted event, with a decision either way.
//!
//! Decision records carry static strings and ids only: never an event name, a property or a
//! value.

use hashbrown::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use sqlx::types::Json;
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::events::{
    EventFlag, RESERVED_EVENT_CODE, Reason, UnplannedPolicy, ValidationState, apply_time_rules,
    check_properties, evaluate, infer_properties, is_reserved_event_name, mask_sample, redact,
    validate_event_name,
};
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{Cause, HmacKey, TraceContext, TraceId, WorkspaceId};

use super::debugger::{FeedEntry, is_watched, notify_feed, outcome_label, write_feed};
use super::schemas::{
    EventPolicy, PublishedSchema, draft_inferred_schema, find_published_schema, get_event_policy,
};
use crate::changes::mark_person;
use crate::decisions::DecisionBuffer;
use crate::flows::{Arrival, enqueue_arrivals};
use crate::people::{find_known_person, normalize_identifiers, record_resolution, resolve_person};
use crate::{NewDecision, PgPool, StoreError, append_decision, begin, commit};

const MAX_MESSAGE_ID_CHARS: usize = 256;

/// One event, as the event path receives it.
#[derive(Clone, Debug)]
pub struct EventRecord {
    pub source: String,
    /// `(kind, value)` pairs, not yet normalized.
    pub identifiers: Vec<(String, String)>,
    pub name: String,
    pub properties: Map<String, Value>,
    /// The client timestamp, when it sent one.
    pub timestamp: Option<OffsetDateTime>,
    pub received_at: OffsetDateTime,
    /// The client's retry key.
    pub message_id: Option<String>,
    pub trace: TraceContext,
    /// The ledger row of the synced record whose mapping produced the event. The event is
    /// deleted with the row; every other event leaves it `None`.
    pub source_record_id: Option<Uuid>,
}

/// What happened to one event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventOutcome {
    Accepted {
        event_id: Uuid,
        person_id: Uuid,
        state: ValidationState,
        flags: Vec<EventFlag>,
    },
    /// The `messageId` already produced an event: nothing was written but the counter.
    Duplicate { original_event_id: Uuid },
    /// The event was refused. A resend of an already rejected message reports the reject
    /// that exists.
    Rejected {
        reject_id: Uuid,
        reason: &'static str,
    },
}

/// Applies `records` in one transaction. Callers chunk their input.
///
/// When the error is [`StoreError::is_retryable`] nothing was committed, and the caller
/// retries the whole batch.
pub async fn apply_event_batch(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    records: &[EventRecord],
) -> Result<Vec<EventOutcome>, StoreError> {
    let mut tx = begin(pool).await?;
    let outcomes = apply_event_records(&mut tx, workspace, key, records).await?;
    crate::changes::request_reevaluation(&mut tx, workspace).await?;
    commit(tx).await?;
    Ok(outcomes)
}

/// [`apply_event_batch`] inside the caller's transaction, which the caller commits.
pub(crate) async fn apply_event_records(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    records: &[EventRecord],
) -> Result<Vec<EventOutcome>, StoreError> {
    crate::changes::cache_change_readers(connection, workspace).await?;
    let mut batch = EventBatch::default();
    let mut outcomes = Vec::with_capacity(records.len());
    for record in records {
        outcomes.push(apply_event_record(connection, workspace, key, &mut batch, record).await?);
    }
    batch.finish(connection, workspace).await?;
    Ok(outcomes)
}

/// What one batch loads once.
#[derive(Default)]
pub(crate) struct EventBatch {
    policy: Option<EventPolicy>,
    schemas: HashMap<String, Option<PublishedSchema>>,
    drafted: HashSet<String>,
    /// Whether anyone watches the workspace; asked once per batch.
    watched: Option<bool>,
    /// Feed rows were written, so [`EventBatch::finish`] must notify.
    fed: bool,
    /// The accepted events that may wake a waiting enrollment, matched once in `finish`.
    arrivals: Vec<Arrival>,
    /// The stored events the forwarding outbox is told about in `finish`.
    forwards: Vec<crate::ForwardRef>,
    /// The decisions of the batch, appended in one statement by `finish`. A drain batch queues the
    /// decisions of its profile records here too, so a trace keeps the order they were made in.
    pub(crate) decisions: DecisionBuffer,
}

impl EventBatch {
    /// Ends the batch: one statement appends its decisions, one query matches the accepted events
    /// against the event waits of enrollments (a `flow.step` job per match), and one notification
    /// goes to the debugger if it wrote feed rows. Call it once after the last record, in the transaction that holds them.
    pub(crate) async fn finish(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<(), StoreError> {
        self.decisions.flush(connection).await?;
        let arrivals = std::mem::take(&mut self.arrivals);
        enqueue_arrivals(connection, workspace, &arrivals).await?;
        let forwards = std::mem::take(&mut self.forwards);
        crate::enqueue_forwarding(connection, workspace, &forwards).await?;
        if std::mem::take(&mut self.fed) {
            notify_feed(connection, workspace).await?;
        }
        Ok(())
    }

    async fn watched(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<bool, StoreError> {
        if let Some(watched) = self.watched {
            return Ok(watched);
        }
        let watched = is_watched(connection, workspace).await?;
        self.watched = Some(watched);
        Ok(watched)
    }

    async fn policy(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<EventPolicy, StoreError> {
        if let Some(policy) = &self.policy {
            return Ok(policy.clone());
        }
        let policy = get_event_policy(connection, workspace).await?;
        self.policy = Some(policy.clone());
        Ok(policy)
    }

    async fn schema(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<PublishedSchema>, StoreError> {
        if let Some(found) = self.schemas.get(name) {
            return Ok(found.clone());
        }
        let found = find_published_schema(connection, workspace, name).await?;
        self.schemas.insert(name.to_owned(), found.clone());
        Ok(found)
    }
}

/// Why an event is refused, with what the reject row keeps of it.
struct Refusal<'a> {
    reason: &'static str,
    reasons: &'a [Reason],
    published: Option<&'a PublishedSchema>,
    person_id: Option<Uuid>,
}

/// Applies one event. While the workspace is watched, also writes its debugger feed row; the
/// caller ends the batch with [`EventBatch::finish`].
pub(crate) async fn apply_event_record(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    record: &EventRecord,
) -> Result<EventOutcome, StoreError> {
    crate::changes::cache_change_readers(connection, workspace).await?;
    let outcome = judge_event_record(connection, workspace, key, batch, record).await?;
    if let EventOutcome::Accepted {
        event_id,
        person_id,
        flags,
        ..
    } = &outcome
        && !flags.contains(&EventFlag::NoTriggers)
    {
        batch.arrivals.push(Arrival {
            event_id: *event_id,
            person_id: *person_id,
            name: record.name.clone(),
        });
    }
    if batch.watched(connection, workspace).await? {
        feed_outcome(connection, workspace, key, batch, record, &outcome).await?;
        batch.fed = true;
    }
    Ok(outcome)
}

/// Writes the debugger feed row of one outcome: the redacted properties, never the raw ones.
async fn feed_outcome(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    record: &EventRecord,
    outcome: &EventOutcome,
) -> Result<(), StoreError> {
    let name = validate_event_name(&record.name)
        .is_ok()
        .then_some(record.name.as_str());
    let policy = batch.policy(connection, workspace).await?;
    let published = match name {
        Some(name) => batch.schema(connection, workspace, name).await?,
        None => None,
    };
    let mut properties = record.properties.clone();
    redact(&mut properties, &policy.redaction, workspace, key);
    let reasons = || {
        published
            .as_ref()
            .map(|schema| check_properties(&record.properties, &schema.properties))
            .unwrap_or_default()
    };
    let (label, reasons, reason, event_id, reject_id) = match outcome {
        EventOutcome::Accepted {
            event_id, state, ..
        } => (
            outcome_label(*state),
            if *state == ValidationState::Unplanned {
                Vec::new()
            } else {
                reasons()
            },
            None,
            Some(*event_id),
            None,
        ),
        EventOutcome::Duplicate { original_event_id } => (
            "duplicate",
            Vec::new(),
            None,
            Some(*original_event_id),
            None,
        ),
        EventOutcome::Rejected { reject_id, reason } => (
            "rejected",
            if *reason == "validation_failed" {
                reasons()
            } else {
                Vec::new()
            },
            Some(*reason),
            None,
            Some(*reject_id),
        ),
    };
    write_feed(
        connection,
        workspace,
        &FeedEntry {
            source: &record.source,
            name,
            outcome: label,
            reasons: &reasons,
            reason,
            properties: &properties,
            schema_version: published.as_ref().map(|schema| schema.version),
            event_id,
            reject_id,
            trace_id: record.trace.trace_id.as_uuid(),
        },
    )
    .await
}

async fn judge_event_record(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    record: &EventRecord,
) -> Result<EventOutcome, StoreError> {
    let identifiers = match checked_input(record) {
        Ok(identifiers) => identifiers,
        Err(reason) => {
            let refusal = Refusal {
                reason,
                reasons: &[],
                published: None,
                person_id: None,
            };
            return refuse(connection, workspace, key, batch, record, &refusal).await;
        }
    };

    if let Some(message_id) = &record.message_id
        && let Some(original) = count_duplicate(connection, workspace, message_id).await?
    {
        return Ok(EventOutcome::Duplicate {
            original_event_id: original,
        });
    }

    let policy = batch.policy(connection, workspace).await?;
    let published = batch.schema(connection, workspace, &record.name).await?;
    let (state, schema_version, reasons) = match &published {
        None => match policy.unplanned {
            UnplannedPolicy::Reject => {
                let refusal = Refusal {
                    reason: "unplanned_event",
                    reasons: &[],
                    published: None,
                    person_id: find_known_person(connection, workspace, &identifiers).await?,
                };
                return refuse(connection, workspace, key, batch, record, &refusal).await;
            }
            UnplannedPolicy::Draft => {
                if batch.drafted.insert(record.name.clone()) {
                    draft_inferred_schema(
                        connection,
                        workspace,
                        &record.name,
                        &infer_properties(&record.properties),
                    )
                    .await?;
                }
                (ValidationState::Unplanned, None, Vec::new())
            }
        },
        Some(schema) => {
            let reasons = check_properties(&record.properties, &schema.properties);
            let Some(state) = evaluate(schema.mode, &reasons) else {
                let refusal = Refusal {
                    reason: "validation_failed",
                    reasons: &reasons,
                    published: Some(schema),
                    person_id: find_known_person(connection, workspace, &identifiers).await?,
                };
                return refuse(connection, workspace, key, batch, record, &refusal).await;
            };
            (state, Some(schema.version), reasons)
        }
    };

    let timing = apply_time_rules(
        record.timestamp,
        record.received_at,
        i64::from(policy.future_skew_seconds),
        i64::from(policy.lateness_seconds),
    );
    let event_id = Uuid::now_v7();

    // Only an accepted event creates or attaches anything. If a concurrent batch claims the
    // `messageId` first, the person writes are rolled back with the savepoint.
    let decisions_mark = batch.decisions.mark();
    let mut savepoint = connection.begin().await.map_err(StoreError::query)?;
    let resolution =
        resolve_person(&mut savepoint, workspace, &identifiers, None, &record.trace).await?;
    let person_id = resolution.person_id;
    if resolution.created {
        decide(
            batch,
            workspace,
            record,
            &decision_actions::PERSON_CREATED,
            None,
            vec![("person", person_id)],
        );
    }
    record_resolution(
        &mut savepoint,
        &mut batch.decisions,
        workspace,
        &record.trace,
        person_id,
        &resolution,
        None,
    )
    .await?;
    if let Some(message_id) = &record.message_id {
        let claimed = sqlx::query(
            "INSERT INTO event_dedupe \
                (workspace_id, message_id, event_id, event_occurred_at, person_id) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (workspace_id, message_id) DO NOTHING",
        )
        .bind(workspace.as_uuid())
        .bind(message_id)
        .bind(event_id)
        .bind(timing.occurred_at)
        .bind(person_id)
        .execute(&mut *savepoint)
        .await
        .map_err(StoreError::query)?;
        if claimed.rows_affected() == 0 {
            savepoint.rollback().await.map_err(StoreError::query)?;
            batch.decisions.discard_after(decisions_mark);
            let original = count_duplicate(connection, workspace, message_id)
                .await?
                .ok_or_else(StoreError::retryable_race)?;
            return Ok(EventOutcome::Duplicate {
                original_event_id: original,
            });
        }
    }
    savepoint.commit().await.map_err(StoreError::query)?;

    let mut properties = record.properties.clone();
    redact(&mut properties, &policy.redaction, workspace, key);
    let flags: Vec<&str> = timing.flags.iter().map(|flag| flag.as_str()).collect();
    insert_event(
        connection,
        workspace,
        &NewEvent {
            id: event_id,
            person_id,
            name: &record.name,
            schema_version,
            occurred_at: timing.occurred_at,
            received_at: record.received_at,
            properties: &properties,
            source: &record.source,
            message_id: record.message_id.as_deref(),
            trace_id: record.trace.trace_id.as_uuid(),
            state,
            reasons: &reasons,
            flags: &flags,
            late: timing.is_late(),
            source_record_id: record.source_record_id,
        },
    )
    .await?;
    batch.forwards.push(crate::ForwardRef {
        id: event_id,
        occurred_at: timing.occurred_at,
    });
    decide(
        batch,
        workspace,
        record,
        match state {
            ValidationState::Valid => &decision_actions::EVENT_ACCEPTED_VALID,
            ValidationState::Flagged => &decision_actions::EVENT_ACCEPTED_FLAGGED,
            ValidationState::Unplanned => &decision_actions::EVENT_ACCEPTED_UNPLANNED,
        },
        None,
        vec![("event", event_id), ("person", person_id)],
    );
    Ok(EventOutcome::Accepted {
        event_id,
        person_id,
        state,
        flags: timing.flags,
    })
}

/// An event to store.
pub(crate) struct NewEvent<'a> {
    pub(crate) id: Uuid,
    pub(crate) person_id: Uuid,
    pub(crate) name: &'a str,
    pub(crate) schema_version: Option<i32>,
    pub(crate) occurred_at: OffsetDateTime,
    pub(crate) received_at: OffsetDateTime,
    /// Already redacted.
    pub(crate) properties: &'a Map<String, Value>,
    pub(crate) source: &'a str,
    pub(crate) message_id: Option<&'a str>,
    pub(crate) trace_id: Uuid,
    pub(crate) state: ValidationState,
    pub(crate) reasons: &'a [Reason],
    pub(crate) flags: &'a [&'a str],
    /// The event is late: the person's traits are recomputed from its time.
    pub(crate) late: bool,
    /// The ledger record the event came from; removing the record removes the event. None for
    /// a first-party event.
    pub(crate) source_record_id: Option<Uuid>,
}

/// Inserts the event and marks its person changed.
pub(crate) async fn insert_event(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    event: &NewEvent<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO events             (id, workspace_id, person_id, name, schema_version, occurred_at, received_at,              properties, source, message_id, trace_id, validation_state, validation_reasons,              flags, source_record_id)          VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
    )
    .bind(event.id)
    .bind(workspace.as_uuid())
    .bind(event.person_id)
    .bind(event.name)
    .bind(event.schema_version)
    .bind(event.occurred_at)
    .bind(event.received_at)
    .bind(Json(event.properties))
    .bind(event.source)
    .bind(event.message_id)
    .bind(event.trace_id)
    .bind(event.state.as_str())
    .bind(Json(event.reasons))
    .bind(event.flags)
    .bind(event.source_record_id)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let trace = TraceContext {
        trace_id: TraceId::from_uuid(event.trace_id),
        cause: Cause::event(event.id),
    };
    mark_person(
        connection,
        workspace,
        event.person_id,
        ChangeClasses::EVENTS,
        &trace,
        event.late.then_some(event.occurred_at),
    )
    .await
}

/// The checks that need no database: the name, the message id and the identifiers.
fn checked_input(
    record: &EventRecord,
) -> Result<Vec<zirv_flow_core::people::Identifier>, &'static str> {
    if validate_event_name(&record.name).is_err() {
        return Err("invalid_event_name");
    }
    if is_reserved_event_name(&record.name) {
        return Err(RESERVED_EVENT_CODE);
    }
    if let Some(message_id) = &record.message_id {
        let length = message_id.chars().count();
        if !(1..=MAX_MESSAGE_ID_CHARS).contains(&length) || message_id.chars().any(char::is_control)
        {
            return Err("invalid_message_id");
        }
    }
    normalize_identifiers(&record.identifiers)
}

/// Counts a repeat of `message_id` and returns the id of the event it repeats.
async fn count_duplicate(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    message_id: &str,
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar(
        "UPDATE event_dedupe SET duplicates = duplicates + 1 \
         WHERE workspace_id = $1 AND message_id = $2 RETURNING event_id",
    )
    .bind(workspace.as_uuid())
    .bind(message_id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Writes the reject with its masked sample and redacted payload, and the decision. A
/// message that already has an open reject writes nothing more.
async fn refuse(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    batch: &mut EventBatch,
    record: &EventRecord,
    refusal: &Refusal<'_>,
) -> Result<EventOutcome, StoreError> {
    let policy = batch.policy(connection, workspace).await?;
    let mut redacted = record.properties.clone();
    redact(&mut redacted, &policy.redaction, workspace, key);
    let sample = mask_sample(
        &redacted,
        refusal.published.map(|schema| &schema.properties[..]),
    );
    let payload = json!({
        "properties": redacted,
        "timestamp": record.timestamp.and_then(|at| at.format(&Rfc3339).ok()),
    });
    let sample = json!({ "properties": sample.properties, "masked": sample.masked });
    let name = validate_event_name(&record.name)
        .is_ok()
        .then_some(record.name.as_str());
    let message_id = (refusal.reason != "invalid_message_id")
        .then_some(record.message_id.as_deref())
        .flatten();
    let reject_id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO ingest_rejects \
            (id, workspace_id, received_at, source, kind, name, reason, reasons, sample, payload, \
             person_id, message_id, trace_id, redaction) \
         VALUES ($1, $2, $3, $4, 'event', $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (workspace_id, message_id) \
            WHERE message_id IS NOT NULL AND replayed_at IS NULL DO NOTHING \
         RETURNING id",
    )
    .bind(reject_id)
    .bind(workspace.as_uuid())
    .bind(record.received_at)
    .bind(&record.source)
    .bind(name)
    .bind(refusal.reason)
    .bind(Json(refusal.reasons))
    .bind(Json(&sample))
    .bind(Json(&payload))
    .bind(refusal.person_id)
    .bind(message_id)
    .bind(record.trace.trace_id.as_uuid())
    .bind(Json(&policy.redaction))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if inserted.is_none() {
        let existing: Uuid = sqlx::query_scalar(
            "SELECT id FROM ingest_rejects \
             WHERE workspace_id = $1 AND message_id = $2 AND replayed_at IS NULL",
        )
        .bind(workspace.as_uuid())
        .bind(message_id)
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        return Ok(EventOutcome::Rejected {
            reject_id: existing,
            reason: refusal.reason,
        });
    }
    let mut subjects = vec![("reject", reject_id)];
    subjects.extend(refusal.person_id.map(|person| ("person", person)));
    decide(
        batch,
        workspace,
        record,
        &decision_actions::EVENT_REJECTED,
        Some(refusal.reason),
        subjects,
    );
    Ok(EventOutcome::Rejected {
        reject_id,
        reason: refusal.reason,
    })
}

/// A drained inbox row that could not be applied, as the reject keeps it.
pub(crate) struct FailedItem<'a> {
    /// `event` or `record`.
    pub(crate) kind: &'static str,
    pub(crate) source: &'a str,
    pub(crate) name: Option<&'a str>,
    /// The event's properties or the record's traits.
    pub(crate) values: &'a Map<String, Value>,
    pub(crate) timestamp: Option<OffsetDateTime>,
    pub(crate) received_at: OffsetDateTime,
    pub(crate) message_id: Option<&'a str>,
    pub(crate) trace: &'a TraceContext,
}

/// Stores an `apply_failed` reject for an item that cannot be applied, with the masked
/// sample and the redacted payload, and a decision. An item whose message already has an
/// open reject writes nothing more.
pub(crate) async fn reject_apply_failed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    item: &FailedItem<'_>,
) -> Result<(), StoreError> {
    let policy = get_event_policy(&mut *connection, workspace).await?;
    let mut redacted = item.values.clone();
    redact(&mut redacted, &policy.redaction, workspace, key);
    let sample = mask_sample(&redacted, None);
    let payload = json!({
        "properties": redacted,
        "timestamp": item.timestamp.and_then(|at| at.format(&Rfc3339).ok()),
    });
    let sample = json!({ "properties": sample.properties, "masked": sample.masked });
    let name = item.name.filter(|name| validate_event_name(name).is_ok());
    let reject_id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO ingest_rejects \
            (id, workspace_id, received_at, source, kind, name, reason, reasons, sample, payload, \
             message_id, trace_id, redaction) \
         VALUES ($1, $2, $3, $4, $5, $6, 'apply_failed', '[]'::jsonb, $7, $8, $9, $10, $11) \
         ON CONFLICT (workspace_id, message_id) \
            WHERE message_id IS NOT NULL AND replayed_at IS NULL DO NOTHING \
         RETURNING id",
    )
    .bind(reject_id)
    .bind(workspace.as_uuid())
    .bind(item.received_at)
    .bind(item.source)
    .bind(item.kind)
    .bind(name)
    .bind(Json(&sample))
    .bind(Json(&payload))
    .bind(item.message_id)
    .bind(item.trace.trace_id.as_uuid())
    .bind(Json(&policy.redaction))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if inserted.is_none() {
        return Ok(());
    }
    let (spec, detail) = if item.kind == "event" {
        (&decision_actions::EVENT_REJECTED, Some("apply_failed"))
    } else {
        (&decision_actions::RECORD_REJECTED_APPLY_FAILED, None)
    };
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: item.trace.clone(),
            spec,
            detail,
            subject_ids: vec![("reject", reject_id)],
            job: None,
        },
    )
    .await
}
/// Queues the decision of one event for the batch's flush in [`EventBatch::finish`].
fn decide(
    batch: &mut EventBatch,
    workspace: WorkspaceId,
    record: &EventRecord,
    spec: &'static DecisionSpec,
    detail: Option<&'static str>,
    subject_ids: Vec<(&'static str, Uuid)>,
) {
    batch.decisions.push(NewDecision {
        graph: None,
        workspace_id: workspace,
        trace: record.trace.clone(),
        spec,
        detail,
        subject_ids,
        job: None,
    });
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::events::{EventMode, PropertyType, RedactionRules, SchemaProperty};
    use zirv_flow_core::{Cause, HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceName};

    use super::*;
    use crate::events::{
        EventPolicy, NewEventSchema, create_event_schema, find_event_schema, publish_schema_draft,
        set_event_policy,
    };
    use crate::{create_workspace, list_trace};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    /// outcome, reason, reasons, properties, schema version, event id, reject id
    type FeedRow = (
        String,
        Option<String>,
        Value,
        Value,
        Option<i32>,
        Option<Uuid>,
        Option<Uuid>,
    );

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn key() -> HmacKey {
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
    }

    fn property(
        name: &str,
        property_type: PropertyType,
        required: bool,
        pii: bool,
    ) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        mode: EventMode,
        properties: Vec<SchemaProperty>,
    ) {
        create_event_schema(
            pool,
            ws,
            &NewEventSchema {
                name: name.to_owned(),
                mode,
                description: String::new(),
                properties,
            },
        )
        .await
        .unwrap();
        let mut tx = begin(pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, name, None).await.unwrap();
        commit(tx).await.unwrap();
    }

    fn event(name: &str, properties: Value) -> EventRecord {
        EventRecord {
            source_record_id: None,
            source: "web".to_owned(),
            identifiers: vec![("email".to_owned(), "ada@example.com".to_owned())],
            name: name.to_owned(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: None,
            received_at: NOW,
            message_id: None,
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn apply_one(pool: &PgPool, ws: WorkspaceId, record: EventRecord) -> EventOutcome {
        apply_event_batch(pool, ws, &key(), &[record])
            .await
            .unwrap()
            .remove(0)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_accepted_event_wakes_the_enrollments_waiting_for_it_once_per_batch(pool: PgPool) {
        use crate::flows::tests_support::{create, publish as publish_flow, waiting_enrollment};
        let ws = workspace(&pool, "waits").await;
        let key = key();
        apply_event_batch(&pool, ws, &key, &[event("order", json!({}))])
            .await
            .unwrap();
        let person: Uuid = sqlx::query_scalar("SELECT person_id FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        let flow = create(&pool, ws, "Flow").await;
        publish_flow(&pool, ws, flow.id).await.unwrap();
        let waiting = waiting_enrollment(&pool, ws, flow.id, person, "order").await;
        let other = create(&pool, ws, "Other").await;
        publish_flow(&pool, ws, other.id).await.unwrap();
        let idle = waiting_enrollment(&pool, ws, other.id, person, "refund").await;

        let outcomes = apply_event_batch(
            &pool,
            ws,
            &key,
            &[event("order", json!({})), event("order", json!({}))],
        )
        .await
        .unwrap();

        let jobs: Vec<Value> =
            sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'flow.step'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(jobs.len(), 2, "one job per accepted event that matches");
        for payload in &jobs {
            assert_eq!(payload["enrollment_id"], waiting.to_string());
            assert_eq!(payload["state_version"], 3);
            assert_ne!(payload["enrollment_id"], idle.to_string());
        }
        let events: Vec<String> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                EventOutcome::Accepted { event_id, .. } => Some(event_id.to_string()),
                _ => None,
            })
            .collect();
        let mut named: Vec<String> = jobs
            .iter()
            .map(|payload| payload["event_id"].as_str().unwrap().to_owned())
            .collect();
        named.sort();
        let mut accepted = events;
        accepted.sort();
        assert_eq!(named, accepted);
    }

    #[test]
    fn a_client_event_under_the_reserved_segment_prefix_is_refused_before_the_database() {
        assert_eq!(
            checked_input(&event("segment.entered", json!({}))).unwrap_err(),
            "reserved_event_name"
        );
        assert_eq!(
            checked_input(&event("Segment.exited", json!({}))).unwrap_err(),
            "reserved_event_name"
        );
        assert!(checked_input(&event("segments", json!({}))).is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_mode_wrong_type_is_rejected_with_a_masked_sample_and_no_event(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![
                property("total", PropertyType::Number, true, false),
                property("email", PropertyType::String, false, true),
            ],
        )
        .await;

        let outcome = apply_one(
            &pool,
            ws,
            event(
                "order",
                json!({"total": "free", "email": "ada@example.com", "extra": "x"}),
            ),
        )
        .await;

        let EventOutcome::Rejected { reject_id, reason } = outcome else {
            panic!("not rejected: {outcome:?}");
        };
        assert_eq!(reason, "validation_failed");
        assert_eq!(count(&pool, "events").await, 0);
        let (reasons, sample, stored_reason): (Value, Value, String) =
            sqlx::query_as("SELECT reasons, sample, reason FROM ingest_rejects WHERE id = $1")
                .bind(reject_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_reason, "validation_failed");
        assert_eq!(
            reasons,
            json!([
                {"property": "total", "reason": "wrong_type"},
                {"property": "extra", "reason": "unplanned_property"}
            ])
        );
        assert_eq!(
            sample,
            json!({
                "properties": {"total": null, "email": null, "extra": null},
                "masked": ["email", "extra", "total"]
            })
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn warn_flags_and_allow_keeps_the_reasons_without_flagging(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let schema = vec![property("total", PropertyType::Number, true, false)];
        publish(&pool, ws, "warned", EventMode::Warn, schema.clone()).await;
        publish(&pool, ws, "allowed", EventMode::Allow, schema).await;

        let warned = apply_one(&pool, ws, event("warned", json!({}))).await;
        let allowed = apply_one(&pool, ws, event("allowed", json!({}))).await;

        assert!(matches!(
            warned,
            EventOutcome::Accepted {
                state: ValidationState::Flagged,
                ..
            }
        ));
        assert!(matches!(
            allowed,
            EventOutcome::Accepted {
                state: ValidationState::Valid,
                ..
            }
        ));
        let rows: Vec<(String, String, Value, Option<i32>)> = sqlx::query_as(
            "SELECT name, validation_state, validation_reasons, schema_version FROM events \
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let reasons = json!([{"property": "total", "reason": "missing_required"}]);
        assert_eq!(
            rows[0],
            ("allowed".into(), "valid".into(), reasons.clone(), Some(1))
        );
        assert_eq!(
            rows[1],
            ("warned".into(), "flagged".into(), reasons, Some(1))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicated_message_id_stores_one_event_and_counts_the_repeat(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut record = event("anything", json!({"a": 1}));
        record.message_id = Some("m-1".to_owned());

        let first = apply_one(&pool, ws, record.clone()).await;
        let second = apply_one(&pool, ws, record.clone()).await;
        let within_batch = apply_event_batch(&pool, ws, &key(), &[record.clone(), record])
            .await
            .unwrap();

        let EventOutcome::Accepted { event_id, .. } = first else {
            panic!("not accepted");
        };
        assert_eq!(
            second,
            EventOutcome::Duplicate {
                original_event_id: event_id
            }
        );
        assert!(within_batch.iter().all(|o| *o == second));
        assert_eq!(count(&pool, "events").await, 1);
        let duplicates: i32 = sqlx::query_scalar("SELECT duplicates FROM event_dedupe")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(duplicates, 3);
        let decisions = count(&pool, "decision_records").await;
        assert_eq!(decisions, 2, "one person.created and one event.accepted");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_message_id_in_another_workspace_is_not_a_duplicate(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mut record = event("anything", json!({}));
        record.message_id = Some("m-1".to_owned());

        apply_one(&pool, one, record.clone()).await;
        let other = apply_one(&pool, two, record).await;

        assert!(matches!(other, EventOutcome::Accepted { .. }));
        assert_eq!(count(&pool, "events").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_excessive_future_timestamp_is_clamped_and_flagged(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut record = event("anything", json!({}));
        record.timestamp = Some(NOW + Duration::hours(2));

        let outcome = apply_one(&pool, ws, record).await;

        assert!(
            matches!(&outcome, EventOutcome::Accepted { flags, .. } if *flags == [EventFlag::Clamped])
        );
        let (occurred_at, flags): (OffsetDateTime, Vec<String>) =
            sqlx::query_as("SELECT occurred_at, flags FROM events")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(occurred_at, NOW);
        assert_eq!(flags, ["clamped"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_late_event_is_flagged_and_keeps_the_earliest_late_from(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::EVENTS,
        )
        .await;
        let day = |days: i64| NOW - Duration::days(days);
        let mut batch = Vec::new();
        for (index, at) in [day(3), day(5), day(4)].into_iter().enumerate() {
            let mut record = event("anything", json!({"n": index}));
            record.timestamp = Some(at);
            batch.push(record);
        }

        let outcomes = apply_event_batch(&pool, ws, &key(), &batch).await.unwrap();

        assert!(outcomes.iter().all(
            |o| matches!(o, EventOutcome::Accepted { flags, .. } if *flags == [EventFlag::Late])
        ));
        let late_from: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT late_from FROM person_changes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(late_from, Some(day(5)));
        let on_time = apply_one(&pool, ws, event("anything", json!({}))).await;
        assert!(matches!(on_time, EventOutcome::Accepted { flags, .. } if flags.is_empty()));
        let still: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT late_from FROM person_changes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(still, Some(day(5)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_draft_policy_accepts_unplanned_names_and_infers_without_publishing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let outcome = apply_one(
            &pool,
            ws,
            event("new thing", json!({"n": 1, "tags": ["a"]})),
        )
        .await;
        apply_one(&pool, ws, event("new thing", json!({"other": true}))).await;

        assert!(matches!(
            outcome,
            EventOutcome::Accepted {
                state: ValidationState::Unplanned,
                ..
            }
        ));
        let schema = find_event_schema(&pool, ws, "new thing")
            .await
            .unwrap()
            .unwrap();
        assert!(schema.inferred);
        assert_eq!(schema.current_version, None);
        let draft = schema.draft.unwrap();
        let names: Vec<&str> = draft.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["n", "tags"], "the first event's shape is kept");
        assert_eq!(count(&pool, "event_schema_versions").await, 0);
        let state: String = sqlx::query_scalar("SELECT DISTINCT validation_state FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "unplanned");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reject_policy_refuses_unplanned_names_and_drafts_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                unplanned: UnplannedPolicy::Reject,
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        let outcome = apply_one(&pool, ws, event("new", json!({"secret": "hunter2"}))).await;

        let EventOutcome::Rejected { reject_id, reason } = outcome else {
            panic!("not rejected");
        };
        assert_eq!(reason, "unplanned_event");
        assert_eq!(count(&pool, "events").await, 0);
        assert_eq!(count(&pool, "event_schemas").await, 0);
        let sample: Value = sqlx::query_scalar("SELECT sample FROM ingest_rejects WHERE id = $1")
            .bind(reject_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            sample,
            json!({"properties": {"secret": null}, "masked": ["secret"]})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schema_with_only_a_draft_is_unplanned(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        create_event_schema(
            &pool,
            ws,
            &NewEventSchema {
                name: "drafty".into(),
                mode: EventMode::Block,
                description: String::new(),
                properties: vec![property("a", PropertyType::Number, true, false)],
            },
        )
        .await
        .unwrap();

        let outcome = apply_one(&pool, ws, event("drafty", json!({}))).await;

        assert!(matches!(
            outcome,
            EventOutcome::Accepted {
                state: ValidationState::Unplanned,
                ..
            }
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn properties_are_validated_before_and_stored_after_redaction(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "signup",
            EventMode::Block,
            vec![
                property("email", PropertyType::String, true, true),
                property("card", PropertyType::String, false, true),
                property("ip", PropertyType::String, false, false),
            ],
        )
        .await;
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    drop: vec!["card".into()],
                    hash: vec!["email".into()],
                    truncate_ip: vec!["ip".into()],
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        let outcome = apply_one(
            &pool,
            ws,
            event(
                "signup",
                json!({"email": "ada@example.com", "card": "4111", "ip": "203.0.113.9"}),
            ),
        )
        .await;

        assert!(matches!(outcome, EventOutcome::Accepted { .. }));
        let stored: Value = sqlx::query_scalar("SELECT properties FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stored.get("card").is_none());
        assert_eq!(stored["ip"], "203.0.113.0");
        let hashed = stored["email"].as_str().unwrap();
        assert_eq!(hashed.len(), 64);
        assert!(!stored.to_string().contains("ada@example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_payload_is_redacted_too(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![property("total", PropertyType::Number, true, false)],
        )
        .await;
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    drop: vec!["card".into()],
                    ..RedactionRules::default()
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        apply_one(
            &pool,
            ws,
            event("order", json!({"card": "4111", "ip": "203.0.113.9"})),
        )
        .await;

        let payload: Value = sqlx::query_scalar("SELECT payload FROM ingest_rejects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payload["properties"], json!({"ip": "203.0.113.0"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_without_a_usable_identifier_is_rejected_without_a_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut none = event("anything", json!({}));
        none.identifiers = Vec::new();
        let mut bad = event("anything", json!({}));
        bad.identifiers = vec![("email".into(), "nope".into())];
        let mut nameless = event("", json!({"a": 1}));
        nameless.name = "bad\nname".into();

        let outcomes = apply_event_batch(&pool, ws, &key(), &[none, bad, nameless])
            .await
            .unwrap();

        let reasons: Vec<&str> = outcomes
            .iter()
            .map(|o| match o {
                EventOutcome::Rejected { reason, .. } => *reason,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            ["no_identifier", "invalid_email", "invalid_event_name"]
        );
        assert_eq!(count(&pool, "people").await, 0);
        let nulls: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ingest_rejects WHERE person_id IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(nulls, 3);
        let name_stored: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ingest_rejects WHERE name IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name_stored, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rejected_message_id_can_later_be_accepted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![property("total", PropertyType::Number, true, false)],
        )
        .await;
        let mut bad = event("order", json!({"total": "x"}));
        bad.message_id = Some("m-1".into());
        let mut fixed = event("order", json!({"total": 5}));
        fixed.message_id = Some("m-1".into());

        let rejected = apply_one(&pool, ws, bad).await;
        let accepted = apply_one(&pool, ws, fixed.clone()).await;
        let repeated = apply_one(&pool, ws, fixed).await;

        assert!(matches!(rejected, EventOutcome::Rejected { .. }));
        let EventOutcome::Accepted { event_id, .. } = accepted else {
            panic!("not accepted: {accepted:?}");
        };
        assert_eq!(
            repeated,
            EventOutcome::Duplicate {
                original_event_id: event_id
            }
        );
        assert_eq!(count(&pool, "events").await, 1);
        assert_eq!(count(&pool, "event_dedupe").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_repeated_reject_writes_one_row_and_one_decision(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![property("total", PropertyType::Number, true, false)],
        )
        .await;
        let mut bad = event("order", json!({"total": "x"}));
        bad.message_id = Some("m-1".into());

        let first = apply_one(&pool, ws, bad.clone()).await;
        let second = apply_one(&pool, ws, bad.clone()).await;
        let third = apply_event_batch(&pool, ws, &key(), &[bad.clone(), bad])
            .await
            .unwrap();

        let EventOutcome::Rejected { reject_id, .. } = first else {
            panic!("not rejected");
        };
        assert!(matches!(second, EventOutcome::Rejected { reject_id: id, .. } if id == reject_id));
        assert!(third.iter().all(
            |o| matches!(o, EventOutcome::Rejected { reject_id: id, .. } if *id == reject_id)
        ));
        assert_eq!(count(&pool, "ingest_rejects").await, 1);
        let rejected_decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'event.rejected'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rejected_decisions, 1);

        sqlx::query("UPDATE ingest_rejects SET replayed_at = now()")
            .execute(&pool)
            .await
            .unwrap();
        let mut again = event("order", json!({"total": "x"}));
        again.message_id = Some("m-1".into());
        apply_one(&pool, ws, again).await;
        assert_eq!(
            count(&pool, "ingest_rejects").await,
            2,
            "a replayed reject is no longer open"
        );
    }

    async fn block_schema(pool: &PgPool, ws: WorkspaceId) {
        publish(
            pool,
            ws,
            "order",
            EventMode::Block,
            vec![property("total", PropertyType::Number, true, false)],
        )
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_with_a_new_identifier_creates_no_person_and_no_identity(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        block_schema(&pool, ws).await;
        let mut bad = event("order", json!({"total": "x"}));
        bad.identifiers = vec![("anonymous_id".into(), "anon-1".into())];
        let mut unplanned = event("nothing", json!({}));
        unplanned.identifiers = vec![("anonymous_id".into(), "anon-2".into())];
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                unplanned: UnplannedPolicy::Reject,
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        let outcomes = apply_event_batch(&pool, ws, &key(), &[bad, unplanned])
            .await
            .unwrap();

        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, EventOutcome::Rejected { .. }))
        );
        assert_eq!(count(&pool, "people").await, 0);
        assert_eq!(count(&pool, "identities").await, 0);
        let with_person: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ingest_rejects WHERE person_id IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(with_person, 0);
        let created: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'person.created'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(created, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_with_a_known_identifier_records_the_person_and_attaches_nothing(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        block_schema(&pool, ws).await;
        let EventOutcome::Accepted { person_id, .. } =
            apply_one(&pool, ws, event("order", json!({"total": 1}))).await
        else {
            panic!("not accepted");
        };
        let mut bad = event("order", json!({"total": "x"}));
        bad.identifiers
            .push(("anonymous_id".into(), "anon-1".into()));

        let outcome = apply_one(&pool, ws, bad).await;

        let EventOutcome::Rejected { reject_id, .. } = outcome else {
            panic!("not rejected");
        };
        let stored: Option<Uuid> =
            sqlx::query_scalar("SELECT person_id FROM ingest_rejects WHERE id = $1")
                .bind(reject_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, Some(person_id));
        assert_eq!(count(&pool, "people").await, 1);
        assert_eq!(count(&pool, "identities").await, 1);
        let subjects: Value = sqlx::query_scalar(
            "SELECT subject_ids FROM decision_records WHERE action = 'event.rejected'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(subjects["person"], person_id.to_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_with_other_identifiers_changes_no_people_or_identities(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut first = event("anything", json!({}));
        first.message_id = Some("m-1".into());
        apply_one(&pool, ws, first.clone()).await;
        let mut again = first;
        again.identifiers = vec![
            ("email".into(), "other@example.com".into()),
            ("anonymous_id".into(), "anon-9".into()),
        ];

        let outcome = apply_one(&pool, ws, again).await;

        assert!(matches!(outcome, EventOutcome::Duplicate { .. }));
        assert_eq!(count(&pool, "people").await, 1);
        assert_eq!(count(&pool, "identities").await, 1);
        assert_eq!(count(&pool, "decision_records").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_whose_identifiers_belong_to_two_people_records_the_conflict(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let with = |identifiers: &[(&str, &str)]| {
            let mut record = event("anything", json!({}));
            record.identifiers = identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect();
            record
        };
        // Two people that hold different external ids cannot be merged by an email.
        let EventOutcome::Accepted { person_id: p1, .. } = apply_one(
            &pool,
            ws,
            with(&[("external:crm", "1"), ("email", "x@example.com")]),
        )
        .await
        else {
            panic!("not accepted");
        };
        let EventOutcome::Accepted { person_id: p2, .. } =
            apply_one(&pool, ws, with(&[("external:crm", "2")])).await
        else {
            panic!("not accepted");
        };
        let both = with(&[("external:crm", "2"), ("email", "x@example.com")]);

        let first = apply_one(&pool, ws, both.clone()).await;
        apply_one(&pool, ws, both).await;

        assert!(matches!(first, EventOutcome::Accepted { person_id, .. } if person_id == p2));
        let rows: Vec<(Uuid, Uuid, Option<Uuid>, String)> = sqlx::query_as(
            "SELECT person_id, other_person_id, source_record_id, reason, identity_id \
             FROM identity_conflicts",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].0, rows[0].1, rows[0].2, rows[0].3.as_str()),
            (p2, p1, None, "unique_kind")
        );
        let with_identity: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM identity_conflicts WHERE identity_id IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(with_identity, 1);
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'identity.conflict'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 1);
        assert_eq!(count(&pool, "people").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_shared_identities_between_the_same_people_leave_two_open_conflicts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let with = |identifiers: &[(&str, &str)]| {
            let mut record = event("anything", json!({}));
            record.identifiers = identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect();
            record
        };
        apply_one(
            &pool,
            ws,
            with(&[
                ("external:crm", "1"),
                ("email", "x@example.com"),
                ("email", "y@example.com"),
            ]),
        )
        .await;
        apply_one(&pool, ws, with(&[("external:crm", "2")])).await;
        let sharing_x = with(&[("external:crm", "2"), ("email", "x@example.com")]);
        let sharing_y = with(&[("external:crm", "2"), ("email", "y@example.com")]);

        apply_one(&pool, ws, sharing_x.clone()).await;
        apply_one(&pool, ws, sharing_y.clone()).await;
        apply_one(&pool, ws, sharing_x).await;
        apply_one(&pool, ws, sharing_y).await;

        let identities: Vec<Option<Uuid>> = sqlx::query_scalar(
            "SELECT identity_id FROM identity_conflicts WHERE resolved_at IS NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(identities.len(), 2);
        assert_ne!(identities[0], identities[1]);
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'identity.conflict'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn decisions_name_ids_and_codes_only(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![property("total", PropertyType::Number, true, false)],
        )
        .await;
        let accepted = event("anything", json!({"secret": "hunter2"}));
        let trace = accepted.trace.trace_id;
        let mut rejected = event("order", json!({"total": "hunter2"}));
        rejected.trace = TraceContext::root(Cause::api("other"));
        let rejected_trace = rejected.trace.trace_id;

        apply_event_batch(&pool, ws, &key(), &[accepted, rejected])
            .await
            .unwrap();

        let (ok, _) = list_trace(&pool, ws, trace, 10).await.unwrap();
        let (refused, _) = list_trace(&pool, ws, rejected_trace, 10).await.unwrap();
        let mut actions: Vec<&str> = ok.iter().map(|d| d.action.as_str()).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["event.accepted", "person.created"]);
        assert_eq!(refused.len(), 1);
        assert_eq!(
            (
                refused[0].action.as_str(),
                refused[0].outcome.as_str(),
                refused[0].reason.as_deref()
            ),
            ("event.rejected", "rejected", Some("validation_failed"))
        );
        let everything = serde_json::to_string(
            &sqlx::query_scalar::<_, Value>(
                "SELECT jsonb_agg(to_jsonb(d)) FROM decision_records d",
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
        )
        .unwrap();
        for leaked in ["hunter2", "order", "anything", "ada@example.com"] {
            assert!(!everything.contains(leaked), "{leaked}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_feed_is_written_only_while_the_workspace_is_watched(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;

        apply_one(&pool, ws, event("seen", json!({}))).await;
        assert_eq!(count(&pool, "debugger_feed").await, 0);

        crate::touch_debugger_watch(&pool, other).await.unwrap();
        apply_one(&pool, ws, event("seen", json!({}))).await;
        assert_eq!(count(&pool, "debugger_feed").await, 0);

        crate::touch_debugger_watch(&pool, ws).await.unwrap();
        apply_one(&pool, ws, event("seen", json!({}))).await;
        assert_eq!(count(&pool, "debugger_feed").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_feed_holds_redacted_values_and_one_row_per_outcome(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        publish(
            &pool,
            ws,
            "order",
            EventMode::Block,
            vec![
                property("total", PropertyType::Number, true, false),
                property("email", PropertyType::String, false, true),
                property("card", PropertyType::String, false, true),
            ],
        )
        .await;
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    drop: vec!["card".into()],
                    hash: vec!["email".into()],
                    truncate_ip: Vec::new(),
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();
        crate::touch_debugger_watch(&pool, ws).await.unwrap();
        let mut accepted = event(
            "order",
            json!({"total": 5, "email": "ada@example.com", "card": "4111"}),
        );
        accepted.message_id = Some("m-1".into());
        let mut repeat = accepted.clone();
        repeat.trace = TraceContext::root(Cause::api("again"));
        let mut rejected = event(
            "order",
            json!({"total": "free", "email": "ada@example.com", "card": "4111"}),
        );
        rejected.message_id = Some("m-2".into());
        let unplanned = event("surprise", json!({"note": "plain"}));

        let outcomes =
            apply_event_batch(&pool, ws, &key(), &[accepted, repeat, rejected, unplanned])
                .await
                .unwrap();

        assert_eq!(outcomes.len(), 4);
        let rows: Vec<FeedRow> =
            sqlx::query_as(
                "SELECT outcome, reason, reasons, properties, schema_version, event_id, reject_id                  FROM debugger_feed ORDER BY seq",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
        let labels: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(labels, ["accepted", "duplicate", "rejected", "unplanned"]);
        assert_eq!(rows[2].1.as_deref(), Some("validation_failed"));
        assert_eq!(
            rows[2].2,
            json!([{"property": "total", "reason": "wrong_type"}])
        );
        assert_eq!(rows[0].4, Some(1));
        assert!(rows[0].5.is_some() && rows[1].5 == rows[0].5);
        assert!(rows[2].6.is_some());
        let mut values: Vec<&Value> = rows.iter().map(|row| &row.3).collect();
        while let Some(value) = values.pop() {
            match value {
                Value::Object(object) => {
                    assert!(!object.contains_key("card"));
                    values.extend(object.values());
                }
                Value::Array(array) => values.extend(array),
                Value::String(text) => {
                    // The workspace-bound HMAC can contain "4111" by chance.
                    let digest = text.len() == 64
                        && text
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
                    if !digest {
                        assert!(!text.contains("ada@example.com"));
                        assert!(!text.contains("4111"));
                    }
                }
                _ => {}
            }
        }
        assert_eq!(rows[0].3["email"].as_str().unwrap().len(), 64);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_watched_batch_notifies_once(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        crate::touch_debugger_watch(&pool, ws).await.unwrap();
        let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
            .await
            .unwrap();
        listener.listen(crate::DEBUGGER_CHANNEL).await.unwrap();

        apply_event_batch(
            &pool,
            ws,
            &key(),
            &[event("a", json!({})), event("b", json!({}))],
        )
        .await
        .unwrap();

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.payload(), ws.as_uuid().to_string());
        let second =
            tokio::time::timeout(std::time::Duration::from_millis(300), listener.recv()).await;
        assert!(second.is_err(), "one notification per batch");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_marks_its_person_in_the_events_class_on_its_own_trace_and_cause(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::EVENTS
                | zirv_flow_core::segment::ChangeClasses::IDENTIFIERS,
        )
        .await;

        apply_one(&pool, ws, event("anything", json!({}))).await;

        let (event_id, event_trace): (Uuid, Uuid) =
            sqlx::query_as("SELECT id, trace_id FROM events")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (classes, trace, kind, id): (i16, Option<Uuid>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT classes, trace_id, cause_kind, cause_id FROM person_changes")
                .fetch_one(&pool)
                .await
                .unwrap();
        // The new person's identifier was marked first; the event's mark is the latest.
        assert_eq!(classes, 2 | 16);
        assert_eq!(trace, Some(event_trace));
        assert_eq!(kind.as_deref(), Some("event"));
        assert_eq!(id, Some(event_id.to_string()));
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'segment.reevaluate'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1);
    }
}
