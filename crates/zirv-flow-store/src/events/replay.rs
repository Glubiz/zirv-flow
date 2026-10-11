//! Reject replay (architecture section 12.2): after a schema or policy fix, a rejected event
//! is judged again from its stored, already redacted payload and stored once.
//!
//! The payload keeps no identifiers, so an event is replayed for the person the reject named
//! when it was refused (following merges); a reject that named none cannot be replayed.
//! Redaction is not applied again: the payload was redacted when it was stored, and hashing a
//! hash would change it. A replayed event carries the flag `replayed`, and `no_triggers`
//! unless the caller asked for triggers, and the `event.replayed` decision names the cause.
//!
//! The dry run ([`preview_replay`]) runs the same judgement and writes nothing. Nothing here
//! logs or returns a name, an identifier or a value.

use std::collections::HashMap;

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::events::{
    EventFlag, PropertyType, Reason, UnplannedPolicy, ValidationState, apply_time_rules,
    check_properties, evaluate,
};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::{NewEvent, insert_event};
use super::schemas::{EventPolicy, PublishedSchema, find_published_schema, get_event_policy};
use crate::{NewDecision, NewJob, PgPool, StoreError, append_decision, begin, commit, enqueue};

/// The most rejects one replay request may cover.
pub const MAX_REPLAY: usize = 10_000;

/// The job kind that replays rejects.
pub const REPLAY_JOB_KIND: &str = "ingest.replay";

/// Enqueues a replay of `ids` as one `ingest.replay` job on `trace`, whose cause names why:
/// a manual request or a definition change. The job payload is `{workspace_id, ids,
/// emit_triggers}`.
pub async fn enqueue_replay<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace: WorkspaceId,
    ids: &[Uuid],
    emit_triggers: bool,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(REPLAY_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = serde_json::json!({
        "workspace_id": workspace.as_uuid(),
        "ids": ids,
        "emit_triggers": emit_triggers,
    });
    enqueue(executor, &job).await
}

/// Which rejects to replay.
#[derive(Clone, Copy, Debug)]
pub enum ReplaySelection<'a> {
    Ids(&'a [Uuid]),
    /// The workspace's open event rejects that match; all fields narrow.
    Filter {
        name: Option<&'a str>,
        reason: Option<&'a str>,
        from: Option<OffsetDateTime>,
        to: Option<OffsetDateTime>,
    },
}

/// The ids a selection names, oldest first. `None` when more than [`MAX_REPLAY`] match.
pub async fn select_rejects(
    pool: &PgPool,
    workspace: WorkspaceId,
    selection: ReplaySelection<'_>,
) -> Result<Option<Vec<Uuid>>, StoreError> {
    let ids: Vec<Uuid> = match selection {
        ReplaySelection::Ids(ids) => {
            if ids.len() > MAX_REPLAY {
                return Ok(None);
            }
            sqlx::query_scalar(
                "SELECT id FROM ingest_rejects \
                 WHERE workspace_id = $1 AND id = ANY($2) ORDER BY received_at, id",
            )
            .bind(workspace.as_uuid())
            .bind(ids)
            .fetch_all(pool)
            .await
            .map_err(StoreError::query)?
        }
        ReplaySelection::Filter {
            name,
            reason,
            from,
            to,
        } => sqlx::query_scalar(
            "SELECT id FROM ingest_rejects \
             WHERE workspace_id = $1 AND kind = 'event' AND replayed_at IS NULL \
               AND ($2::text IS NULL OR name = $2) \
               AND ($3::text IS NULL OR reason = $3) \
               AND ($4::timestamptz IS NULL OR received_at >= $4) \
               AND ($5::timestamptz IS NULL OR received_at < $5) \
             ORDER BY received_at, id LIMIT $6",
        )
        .bind(workspace.as_uuid())
        .bind(name)
        .bind(reason)
        .bind(from)
        .bind(to)
        .bind(i64::try_from(MAX_REPLAY + 1).unwrap_or(i64::MAX))
        .fetch_all(pool)
        .await
        .map_err(StoreError::query)?,
    };
    Ok((ids.len() <= MAX_REPLAY).then_some(ids))
}

#[derive(FromRow)]
struct RejectRow {
    id: Uuid,
    kind: String,
    source: String,
    name: Option<String>,
    payload: Value,
    person_id: Option<Uuid>,
    message_id: Option<String>,
    received_at: OffsetDateTime,
    trace_id: Uuid,
    replayed_at: Option<OffsetDateTime>,
}

/// What judging a reject again finds.
enum Verdict {
    AlreadyReplayed,
    StillRejected {
        reason: &'static str,
        reasons: Vec<Reason>,
    },
    /// The `messageId` already produced an event.
    Duplicate {
        original: Uuid,
    },
    Accept {
        person_id: Uuid,
        state: ValidationState,
        schema_version: Option<i32>,
        reasons: Vec<Reason>,
    },
}

/// What a dry run predicts for one reject.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayPrediction {
    pub reject_id: Uuid,
    /// `accept`, `flag`, `still_rejected`, `duplicate` or `already_replayed`.
    pub outcome: &'static str,
    /// A static code for `still_rejected`.
    pub reason: Option<&'static str>,
    pub reasons: Vec<Reason>,
    /// Properties the schema types as something other than a string that the policy hashes:
    /// the stored payload holds a hash, so they cannot validate.
    pub redacted_type_changed: Vec<String>,
}

/// Judgements that share their reads across a request.
struct Judge {
    policy: Option<EventPolicy>,
    schemas: HashMap<String, Option<PublishedSchema>>,
}

impl Judge {
    fn new() -> Self {
        Self {
            policy: None,
            schemas: HashMap::new(),
        }
    }

    async fn policy(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
    ) -> Result<EventPolicy, StoreError> {
        if self.policy.is_none() {
            self.policy = Some(get_event_policy(&mut *connection, workspace).await?);
        }
        Ok(self.policy.clone().unwrap_or_default())
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

    async fn judge(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        reject: &RejectRow,
    ) -> Result<Verdict, StoreError> {
        let still = |reason| {
            Ok(Verdict::StillRejected {
                reason,
                reasons: Vec::new(),
            })
        };
        if reject.replayed_at.is_some() {
            return Ok(Verdict::AlreadyReplayed);
        }
        let Some(name) = reject.name.as_deref().filter(|_| reject.kind == "event") else {
            return still("not_replayable");
        };
        let Some(person) = reject.person_id else {
            return still("no_person");
        };
        let person: Option<Uuid> = sqlx::query_scalar(
            "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(person)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let Some(person_id) = person else {
            return still("no_person");
        };
        if let Some(message_id) = &reject.message_id {
            let original: Option<Uuid> = sqlx::query_scalar(
                "SELECT event_id FROM event_dedupe WHERE workspace_id = $1 AND message_id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(message_id)
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            if let Some(original) = original {
                return Ok(Verdict::Duplicate { original });
            }
        }
        let policy = self.policy(connection, workspace).await?;
        let properties = payload_properties(&reject.payload);
        match self.schema(connection, workspace, name).await? {
            None => match policy.unplanned {
                UnplannedPolicy::Reject => still("unplanned_event"),
                UnplannedPolicy::Draft => Ok(Verdict::Accept {
                    person_id,
                    state: ValidationState::Unplanned,
                    schema_version: None,
                    reasons: Vec::new(),
                }),
            },
            Some(schema) => {
                let reasons = check_properties(&properties, &schema.properties);
                match evaluate(schema.mode, &reasons) {
                    None => Ok(Verdict::StillRejected {
                        reason: "validation_failed",
                        reasons,
                    }),
                    Some(state) => Ok(Verdict::Accept {
                        person_id,
                        state,
                        schema_version: Some(schema.version),
                        reasons,
                    }),
                }
            }
        }
    }
}

fn payload_properties(payload: &Value) -> Map<String, Value> {
    match payload.get("properties") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

fn payload_timestamp(payload: &Value) -> Option<OffsetDateTime> {
    payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| OffsetDateTime::parse(text, &Rfc3339).ok())
}

async fn load_reject(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    lock: bool,
) -> Result<Option<RejectRow>, StoreError> {
    let sql = if lock {
        "SELECT id, kind, source, name, payload, person_id, message_id, received_at, trace_id, \
                replayed_at FROM ingest_rejects WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    } else {
        "SELECT id, kind, source, name, payload, person_id, message_id, received_at, trace_id, \
                replayed_at FROM ingest_rejects WHERE workspace_id = $1 AND id = $2"
    };
    sqlx::query_as(sql)
        .bind(workspace.as_uuid())
        .bind(id)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)
}

/// Predicts what replaying each of `ids` would do, in the order given. Writes nothing. An id
/// that names no reject of the workspace is left out.
pub async fn preview_replay(
    pool: &PgPool,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<Vec<ReplayPrediction>, StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::query)?;
    let mut judge = Judge::new();
    let mut predictions = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(reject) = load_reject(&mut connection, workspace, *id, false).await? else {
            continue;
        };
        let verdict = judge.judge(&mut connection, workspace, &reject).await?;
        let changed = match reject.name.as_deref() {
            Some(name) => {
                let policy = judge.policy(&mut connection, workspace).await?;
                let schema = judge.schema(&mut connection, workspace, name).await?;
                redacted_type_changed(&policy, schema.as_ref(), &reject.payload)
            }
            None => Vec::new(),
        };
        let (outcome, reason, reasons) = match verdict {
            Verdict::AlreadyReplayed => ("already_replayed", None, Vec::new()),
            Verdict::Duplicate { .. } => ("duplicate", None, Vec::new()),
            Verdict::StillRejected { reason, reasons } => ("still_rejected", Some(reason), reasons),
            Verdict::Accept { state, reasons, .. } => (
                if state == ValidationState::Flagged {
                    "flag"
                } else {
                    "accept"
                },
                None,
                reasons,
            ),
        };
        predictions.push(ReplayPrediction {
            reject_id: reject.id,
            outcome,
            reason,
            reasons,
            redacted_type_changed: changed,
        });
    }
    Ok(predictions)
}

fn redacted_type_changed(
    policy: &EventPolicy,
    schema: Option<&PublishedSchema>,
    payload: &Value,
) -> Vec<String> {
    let Some(schema) = schema else {
        return Vec::new();
    };
    let properties = payload_properties(payload);
    let mut changed: Vec<String> = schema
        .properties
        .iter()
        .filter(|declared| {
            declared.property_type != PropertyType::String
                && policy.redaction.hash.contains(&declared.name)
                && properties.contains_key(&declared.name)
        })
        .map(|declared| declared.name.clone())
        .collect();
    changed.sort();
    changed
}

/// What a replay did, by reject.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplayTotals {
    /// Stored as valid or unplanned.
    pub accepted: u64,
    /// Stored as flagged.
    pub flagged: u64,
    pub duplicate: u64,
    pub still_rejected: u64,
    /// Already replayed before, or gone.
    pub skipped: u64,
}

impl ReplayTotals {
    pub fn stored(&self) -> u64 {
        self.accepted + self.flagged
    }
}

/// Replays each reject of `ids` in a transaction of its own and returns the totals. `trace`
/// carries the cause: a manual request or a definition change. With `emit_triggers` false
/// the stored events are flagged `no_triggers`.
///
/// Replaying a reject twice stores one event: the second finds `replayed_at` set.
pub async fn replay_rejects(
    pool: &PgPool,
    workspace: WorkspaceId,
    ids: &[Uuid],
    trace: &TraceContext,
    emit_triggers: bool,
) -> Result<ReplayTotals, StoreError> {
    let mut totals = ReplayTotals::default();
    let mut judge = Judge::new();
    for id in ids {
        let mut tx = begin(pool).await?;
        let Some(reject) = load_reject(&mut tx, workspace, *id, true).await? else {
            totals.skipped += 1;
            continue;
        };
        match judge.judge(&mut tx, workspace, &reject).await? {
            Verdict::AlreadyReplayed => totals.skipped += 1,
            Verdict::StillRejected { .. } => totals.still_rejected += 1,
            Verdict::Duplicate { original } => {
                mark_replayed(&mut tx, workspace, reject.id, original).await?;
                totals.duplicate += 1;
            }
            Verdict::Accept {
                person_id,
                state,
                schema_version,
                reasons,
            } => {
                if store_replayed(
                    &mut tx,
                    workspace,
                    &mut judge,
                    &reject,
                    person_id,
                    (state, schema_version, &reasons),
                    trace,
                    emit_triggers,
                )
                .await?
                {
                    if state == ValidationState::Flagged {
                        totals.flagged += 1;
                    } else {
                        totals.accepted += 1;
                    }
                } else {
                    totals.duplicate += 1;
                }
            }
        }
        commit(tx).await?;
    }
    Ok(totals)
}

async fn mark_replayed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    reject: Uuid,
    event: Uuid,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE ingest_rejects SET replayed_at = now(), replay_event_id = $3 \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(reject)
    .bind(event)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Stores the event of an accepted verdict. False when a concurrent event claimed the
/// `messageId` first, in which case the reject is resolved against that event.
#[expect(clippy::too_many_arguments, reason = "one step of replay_rejects")]
async fn store_replayed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    judge: &mut Judge,
    reject: &RejectRow,
    person_id: Uuid,
    (state, schema_version, reasons): (ValidationState, Option<i32>, &[Reason]),
    trace: &TraceContext,
    emit_triggers: bool,
) -> Result<bool, StoreError> {
    let policy = judge.policy(connection, workspace).await?;
    let properties = payload_properties(&reject.payload);
    let timing = apply_time_rules(
        payload_timestamp(&reject.payload),
        reject.received_at,
        i64::from(policy.future_skew_seconds),
        i64::from(policy.lateness_seconds),
    );
    let event_id = Uuid::now_v7();
    if let Some(message_id) = &reject.message_id {
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
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if claimed.rows_affected() == 0 {
            let original: Uuid = sqlx::query_scalar(
                "SELECT event_id FROM event_dedupe WHERE workspace_id = $1 AND message_id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(message_id)
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            mark_replayed(connection, workspace, reject.id, original).await?;
            return Ok(false);
        }
    }
    let mut flags: Vec<&str> = timing.flags.iter().map(|flag| flag.as_str()).collect();
    flags.push(EventFlag::Replayed.as_str());
    if !emit_triggers {
        flags.push(EventFlag::NoTriggers.as_str());
    }
    let name = reject.name.as_deref().unwrap_or_default();
    insert_event(
        connection,
        workspace,
        &NewEvent {
            id: event_id,
            person_id,
            name,
            schema_version,
            occurred_at: timing.occurred_at,
            received_at: reject.received_at,
            properties: &properties,
            source: &reject.source,
            message_id: reject.message_id.as_deref(),
            trace_id: reject.trace_id,
            state,
            reasons,
            flags: &flags,
            late: timing.is_late(),
        },
    )
    .await?;
    mark_replayed(connection, workspace, reject.id, event_id).await?;
    append_decision(
        connection,
        &NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "event.replayed",
            outcome: state.as_str(),
            reason: None,
            subject_ids: vec![
                ("reject", reject.id),
                ("event", event_id),
                ("person", person_id),
            ],
            job: None,
        },
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::events::{EventMode, RedactionRules, SchemaProperty};
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, WorkspaceName};

    use super::*;
    use crate::events::{
        EventOutcome, EventRecord, NewEventSchema, apply_event_batch, create_event_schema,
        publish_schema_draft, save_schema_draft, set_event_policy,
    };
    use crate::{create_workspace, list_trace};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    fn property(name: &str, property_type: PropertyType, required: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.to_owned(),
            property_type,
            required,
            enum_values: None,
            description: String::new(),
            pii: false,
        }
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn publish(pool: &PgPool, ws: WorkspaceId, properties: Vec<SchemaProperty>) {
        let mut tx = begin(pool).await.unwrap();
        save_schema_draft(&mut *tx, ws, "order", &properties)
            .await
            .unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    fn record(who: &str, message: &str, properties: Value) -> EventRecord {
        EventRecord {
            source: "web".to_owned(),
            identifiers: vec![("email".to_owned(), who.to_owned())],
            name: "order".to_owned(),
            properties: properties.as_object().cloned().unwrap(),
            timestamp: Some(NOW - time::Duration::minutes(5)),
            received_at: NOW,
            message_id: Some(message.to_owned()),
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    fn key() -> zirv_flow_core::HmacKey {
        MasterKey::generate().hmac_key(HmacPurpose::Redaction)
    }

    /// A workspace whose block-mode schema requires a number `total`, and one reject of an
    /// event that sent it as text, for a known person.
    async fn rejected(pool: &PgPool) -> (WorkspaceId, Uuid) {
        let ws = workspace(pool).await;
        create_event_schema(
            pool,
            ws,
            &NewEventSchema {
                name: "order".into(),
                mode: EventMode::Block,
                description: String::new(),
                properties: vec![property("total", PropertyType::Number, true)],
            },
        )
        .await
        .unwrap();
        let mut tx = begin(pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        // The person exists from an earlier accepted event.
        let first = record("a@example.com", "m-0", json!({"total": 1}));
        apply_event_batch(pool, ws, &key(), &[first]).await.unwrap();
        let outcomes = apply_event_batch(
            pool,
            ws,
            &key(),
            &[record("a@example.com", "m-1", json!({"total": "ten"}))],
        )
        .await
        .unwrap();
        let EventOutcome::Rejected { reject_id, .. } = outcomes[0] else {
            panic!("not rejected");
        };
        (ws, reject_id)
    }

    async fn fix_the_schema(pool: &PgPool, ws: WorkspaceId) {
        publish(
            pool,
            ws,
            vec![property("total", PropertyType::String, true)],
        )
        .await;
    }

    fn manual_trace() -> TraceContext {
        TraceContext::root(Cause::definition_change(Uuid::now_v7()))
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_dry_run_predicts_still_rejected_then_accept_and_writes_nothing(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        let events = count(&pool, "events").await;
        let decisions = count(&pool, "decision_records").await;

        let before = preview_replay(&pool, ws, &[reject]).await.unwrap();
        fix_the_schema(&pool, ws).await;
        let after = preview_replay(&pool, ws, &[reject, Uuid::now_v7()])
            .await
            .unwrap();

        assert_eq!(before[0].outcome, "still_rejected");
        assert_eq!(before[0].reason, Some("validation_failed"));
        assert_eq!(before[0].reasons[0].property, "total");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].outcome, "accept");
        assert_eq!(count(&pool, "events").await, events);
        assert_eq!(count(&pool, "decision_records").await, decisions);
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ingest_rejects WHERE replayed_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(open, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_after_a_schema_fix_stores_the_event_once_and_a_second_changes_nothing(
        pool: PgPool,
    ) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        let trace = manual_trace();

        let first = replay_rejects(&pool, ws, &[reject], &trace, false)
            .await
            .unwrap();
        let second = replay_rejects(&pool, ws, &[reject], &trace, false)
            .await
            .unwrap();

        assert_eq!((first.accepted, first.stored()), (1, 1));
        assert_eq!((second.stored(), second.skipped), (0, 1));
        let (flags, state, version, message): (Vec<String>, String, Option<i32>, Option<String>) =
            sqlx::query_as(
                "SELECT flags, validation_state, schema_version, message_id FROM events \
                 WHERE message_id = 'm-1'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(
            flags.contains(&"replayed".to_owned()) && flags.contains(&"no_triggers".to_owned())
        );
        assert_eq!(
            (state.as_str(), version, message.as_deref()),
            ("valid", Some(2), Some("m-1"))
        );
        assert_eq!(count(&pool, "events").await, 2);
        let (replayed_at, event): (Option<OffsetDateTime>, Option<Uuid>) =
            sqlx::query_as("SELECT replayed_at, replay_event_id FROM ingest_rejects WHERE id = $1")
                .bind(reject)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(replayed_at.is_some() && event.is_some());
        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].action, "event.replayed");
        assert_eq!(
            decisions[0].cause.kind,
            zirv_flow_core::CauseKind::DefinitionChange
        );
        assert_eq!(decisions[0].subject_ids["reject"], reject.to_string());
        assert_eq!(
            decisions[0].subject_ids["event"],
            event.unwrap().to_string()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn emitting_triggers_leaves_the_no_triggers_flag_off(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;

        replay_rejects(&pool, ws, &[reject], &manual_trace(), true)
            .await
            .unwrap();

        let flags: Vec<String> =
            sqlx::query_scalar("SELECT flags FROM events WHERE message_id = 'm-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(flags.contains(&"replayed".to_owned()));
        assert!(!flags.contains(&"no_triggers".to_owned()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reject_that_still_fails_stays_open_and_a_stored_message_id_is_a_duplicate(
        pool: PgPool,
    ) {
        let (ws, reject) = rejected(&pool).await;
        // The same message later arrived and was accepted, so the reject is a duplicate.
        sqlx::query(
            "INSERT INTO event_dedupe (workspace_id, message_id, event_id, event_occurred_at, \
                person_id) SELECT workspace_id, 'm-1', gen_random_uuid(), now(), person_id \
             FROM event_dedupe LIMIT 1",
        )
        .execute(&pool)
        .await
        .unwrap();

        let duplicate = replay_rejects(&pool, ws, &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(duplicate.duplicate, 1);
        assert_eq!(count(&pool, "events").await, 1);
        let resolved: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT replayed_at FROM ingest_rejects WHERE id = $1")
                .bind(reject)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(resolved.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_fix_nothing_is_stored_and_the_reject_stays_open(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;

        let totals = replay_rejects(&pool, ws, &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(totals.still_rejected, 1);
        assert_eq!(count(&pool, "events").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_replay_job_is_enqueued_on_the_given_trace_with_its_ids(pool: PgPool) {
        let ws = workspace(&pool).await;
        let trace = manual_trace();
        let ids = [Uuid::now_v7(), Uuid::now_v7()];

        assert!(
            enqueue_replay(&pool, ws, &ids, false, &trace)
                .await
                .unwrap()
        );

        let (kind, payload, trace_id): (String, Value, Uuid) =
            sqlx::query_as("SELECT kind, payload, trace_id FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(kind, REPLAY_JOB_KIND);
        assert_eq!(payload["ids"].as_array().unwrap().len(), 2);
        assert_eq!(payload["emit_triggers"], false);
        assert_eq!(trace_id, trace.trace_id.as_uuid());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn selection_by_filter_skips_replayed_rejects_and_caps_ids(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        let filter = ReplaySelection::Filter {
            name: Some("order"),
            reason: Some("validation_failed"),
            from: None,
            to: None,
        };

        let found = select_rejects(&pool, ws, filter).await.unwrap().unwrap();
        replay_rejects(&pool, ws, &found, &manual_trace(), false)
            .await
            .unwrap();
        let after = select_rejects(&pool, ws, filter).await.unwrap().unwrap();
        let too_many = vec![reject; MAX_REPLAY + 1];
        let capped = select_rejects(&pool, ws, ReplaySelection::Ids(&too_many))
            .await
            .unwrap();

        assert_eq!(found, [reject]);
        assert!(after.is_empty());
        assert!(capped.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hashed_non_string_property_is_reported_as_changed_by_redaction(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    hash: vec!["total".into()],
                    ..RedactionRules::default()
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();

        let prediction = preview_replay(&pool, ws, &[reject]).await.unwrap();

        assert_eq!(prediction[0].redacted_type_changed, ["total"]);
    }
}
