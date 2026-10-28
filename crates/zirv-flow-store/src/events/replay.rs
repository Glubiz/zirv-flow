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

use hashbrown::HashMap;
use std::time::Duration;

use serde_json::{Map, Value};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::decision_actions;
use zirv_flow_core::events::{
    EventFlag, PropertyType, Reason, RedactionRules, UnplannedPolicy, ValidationState,
    apply_time_rules, check_properties, evaluate, redact,
};
use zirv_flow_core::{HmacKey, TraceContext, WorkspaceId};

use super::apply::{NewEvent, insert_event};
use super::debugger::{FeedEntry, is_watched, notify_feed, outcome_label, write_feed};
use super::schemas::{
    EventPolicy, PublishedSchema, find_published_schema, find_published_schemas, get_event_policy,
};
use crate::bounded::begin_snapshot_for;
use crate::flows::{Arrival, enqueue_arrivals};
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
    /// The redaction rules in force when the reject was written; null for a legacy reject.
    redaction: Option<Value>,
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
    /// The redaction rules differ from the ones the reject was stored under, so storing it
    /// applies the current rules that its payload has not had yet.
    pub redaction_changed: bool,
}

/// Judgements that share their reads across a request.
struct Judge {
    policy: Option<EventPolicy>,
    schemas: HashMap<String, Option<PublishedSchema>>,
    /// Schema queries issued, so a test can show the number does not grow with the names.
    #[cfg(test)]
    schema_loads: usize,
}

impl Judge {
    fn new() -> Self {
        Self {
            policy: None,
            schemas: HashMap::new(),
            #[cfg(test)]
            schema_loads: 0,
        }
    }

    /// Loads the published schemas of every name in one query, so judging a reject never asks
    /// for its own. A name without one is remembered as having none.
    async fn preload(
        &mut self,
        connection: &mut PgConnection,
        workspace: WorkspaceId,
        names: &[String],
    ) -> Result<(), StoreError> {
        let missing: Vec<String> = names
            .iter()
            .filter(|name| !self.schemas.contains_key(*name))
            .cloned()
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.schema_loads += 1;
        }
        let mut found = find_published_schemas(connection, workspace, &missing).await?;
        for name in missing {
            let schema = found.remove(&name);
            self.schemas.insert(name, schema);
        }
        Ok(())
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
        #[cfg(test)]
        {
            self.schema_loads += 1;
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
        let mut person = None;
        let mut original = None;
        let mut schema = None;
        let policy = self.policy(connection, workspace).await?;
        if let (Some(known), Some(name), true) = (
            reject.person_id,
            reject.name.as_deref(),
            reject.replayed_at.is_none(),
        ) {
            person = sqlx::query_scalar(
                "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(known)
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            if let (Some(_), Some(message_id)) = (person, &reject.message_id) {
                original = sqlx::query_scalar(
                    "SELECT event_id FROM event_dedupe WHERE workspace_id = $1 AND message_id = $2",
                )
                .bind(workspace.as_uuid())
                .bind(message_id)
                .fetch_optional(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            }
            schema = self.schema(connection, workspace, name).await?;
        }
        Ok(decide(reject, person, original, &policy, schema.as_ref()))
    }
}

/// What judging `reject` finds, given what the database says about its person, its
/// `messageId` and its schema. Pure.
fn decide(
    reject: &RejectRow,
    person: Option<Uuid>,
    original: Option<Uuid>,
    policy: &EventPolicy,
    schema: Option<&PublishedSchema>,
) -> Verdict {
    let still = |reason| Verdict::StillRejected {
        reason,
        reasons: Vec::new(),
    };
    if reject.replayed_at.is_some() {
        return Verdict::AlreadyReplayed;
    }
    if reject.name.is_none() || reject.kind != "event" {
        return still("not_replayable");
    }
    let (Some(_), Some(person_id)) = (reject.person_id, person) else {
        return still("no_person");
    };
    if let Some(original) = original {
        return Verdict::Duplicate { original };
    }
    let properties = payload_properties(&reject.payload);
    match schema {
        None => match policy.unplanned {
            UnplannedPolicy::Reject => still("unplanned_event"),
            UnplannedPolicy::Draft => Verdict::Accept {
                person_id,
                state: ValidationState::Unplanned,
                schema_version: None,
                reasons: Vec::new(),
            },
        },
        Some(schema) => {
            let reasons = check_properties(&properties, &schema.properties);
            match evaluate(schema.mode, &reasons) {
                None => Verdict::StillRejected {
                    reason: "validation_failed",
                    reasons,
                },
                Some(state) => Verdict::Accept {
                    person_id,
                    state,
                    schema_version: Some(schema.version),
                    reasons,
                },
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
                replayed_at, redaction FROM ingest_rejects WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    } else {
        "SELECT id, kind, source, name, payload, person_id, message_id, received_at, trace_id, \
                replayed_at, redaction FROM ingest_rejects WHERE workspace_id = $1 AND id = $2"
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
///
/// It reads in one snapshot, under a statement `timeout`, with a fixed number of statements
/// (the rejects, their people and their message ids in bulk), so the whole prediction is
/// bounded; an overrun is [`StoreError::is_timeout`].
pub async fn preview_replay(
    pool: &PgPool,
    workspace: WorkspaceId,
    ids: &[Uuid],
    timeout: Duration,
) -> Result<Vec<ReplayPrediction>, StoreError> {
    let mut tx = begin_snapshot_for(pool, timeout).await?;
    preview_in(&mut tx, workspace, ids, &mut Judge::new())
        .await
        .map_err(StoreError::into_bounded)
}

async fn preview_in(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
    judge: &mut Judge,
) -> Result<Vec<ReplayPrediction>, StoreError> {
    let mut rejects: Vec<RejectRow> = sqlx::query_as(
        "SELECT id, kind, source, name, payload, person_id, message_id, received_at, trace_id, \
                replayed_at, redaction FROM ingest_rejects WHERE workspace_id = $1 AND id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let position: HashMap<Uuid, usize> = ids.iter().enumerate().map(|(n, id)| (*id, n)).collect();
    rejects.sort_by_key(|reject| position.get(&reject.id).copied());

    let people: Vec<Uuid> = rejects
        .iter()
        .filter_map(|reject| reject.person_id)
        .collect();
    let survivors: HashMap<Uuid, Uuid> = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT id, COALESCE(merged_into, id) FROM people \
         WHERE workspace_id = $1 AND id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(&people)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .into_iter()
    .collect();
    let messages: Vec<&str> = rejects
        .iter()
        .filter_map(|reject| reject.message_id.as_deref())
        .collect();
    let stored: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT message_id, event_id FROM event_dedupe \
         WHERE workspace_id = $1 AND message_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(&messages)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .into_iter()
    .collect();

    let names: Vec<String> = rejects
        .iter()
        .filter_map(|reject| reject.name.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    judge.preload(connection, workspace, &names).await?;
    let policy = judge.policy(connection, workspace).await?;
    let mut predictions = Vec::with_capacity(rejects.len());
    for reject in rejects {
        let schema = match reject.name.as_deref() {
            Some(name) => judge.schema(connection, workspace, name).await?,
            None => None,
        };
        let person = reject
            .person_id
            .and_then(|person| survivors.get(&person).copied());
        let original = reject
            .message_id
            .as_ref()
            .and_then(|message| stored.get(message).copied());
        let verdict = decide(&reject, person, original, &policy, schema.as_ref());
        let changed = redacted_type_changed(&policy, &reject, schema.as_ref());
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
            redaction_changed: redaction_changed(
                &policy.redaction,
                recorded_rules(&reject).as_ref(),
            ),
        });
    }
    Ok(predictions)
}

/// The rules the reject was written under; none for a legacy reject.
fn recorded_rules(reject: &RejectRow) -> Option<RedactionRules> {
    reject
        .redaction
        .as_ref()
        .and_then(|rules| serde_json::from_value(rules.clone()).ok())
}

/// Whether the current rules differ from the recorded ones. For a legacy reject nothing is
/// recorded, so any current rule counts.
fn redaction_changed(current: &RedactionRules, recorded: Option<&RedactionRules>) -> bool {
    match recorded {
        Some(recorded) => recorded != current,
        None => {
            !(current.drop.is_empty() && current.hash.is_empty() && current.truncate_ip.is_empty())
        }
    }
}

/// The rules to apply when storing a replayed reject: `drop` and `truncate_ip` in full, both
/// idempotent, and `hash` only for paths the recorded rules did not already hash, so a value
/// is never hashed twice. A legacy reject applies the current rules in full.
fn replay_rules(current: &RedactionRules, recorded: Option<&RedactionRules>) -> RedactionRules {
    RedactionRules {
        drop: current.drop.clone(),
        hash: current
            .hash
            .iter()
            .filter(|path| recorded.is_none_or(|rules| !rules.hash.contains(path)))
            .cloned()
            .collect(),
        truncate_ip: current.truncate_ip.clone(),
    }
}

/// Properties the schema types as something other than a string that the payload already
/// holds as a hash, so they cannot validate. A legacy reject may have been hashed under any
/// current path.
fn redacted_type_changed(
    policy: &EventPolicy,
    reject: &RejectRow,
    schema: Option<&PublishedSchema>,
) -> Vec<String> {
    let Some(schema) = schema else {
        return Vec::new();
    };
    let applied =
        recorded_rules(reject).map_or_else(|| policy.redaction.hash.clone(), |rules| rules.hash);
    let properties = payload_properties(&reject.payload);
    let mut changed: Vec<String> = schema
        .properties
        .iter()
        .filter(|declared| {
            declared.property_type != PropertyType::String
                && applied.contains(&declared.name)
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
///
/// The stored event gets the current redaction rules its payload has not had yet (see
/// [`replay_rules`]); `key` hashes the paths that are new.
pub async fn replay_rejects(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    ids: &[Uuid],
    trace: &TraceContext,
    emit_triggers: bool,
) -> Result<ReplayTotals, StoreError> {
    let mut totals = ReplayTotals::default();
    let mut judge = Judge::new();
    {
        // Every schema the batch needs, in one query rather than one per name.
        let mut connection = pool.acquire().await.map_err(StoreError::query)?;
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT name FROM ingest_rejects \
             WHERE workspace_id = $1 AND id = ANY($2) AND name IS NOT NULL",
        )
        .bind(workspace.as_uuid())
        .bind(ids)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        judge.preload(&mut connection, workspace, &names).await?;
    }
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
                    key,
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
    key: &HmacKey,
    judge: &mut Judge,
    reject: &RejectRow,
    person_id: Uuid,
    (state, schema_version, reasons): (ValidationState, Option<i32>, &[Reason]),
    trace: &TraceContext,
    emit_triggers: bool,
) -> Result<bool, StoreError> {
    let policy = judge.policy(connection, workspace).await?;
    // Validation saw the payload as stored; what is kept has the rules it has not had yet.
    let mut properties = payload_properties(&reject.payload);
    redact(
        &mut properties,
        &replay_rules(&policy.redaction, recorded_rules(reject).as_ref()),
        workspace,
        key,
    );
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
            source_record_id: None,
        },
    )
    .await?;
    // Only a replay that emits triggers may wake an enrollment waiting for the event.
    if emit_triggers {
        let arrival = Arrival {
            event_id,
            person_id,
            name: name.to_owned(),
        };
        enqueue_arrivals(connection, workspace, &[arrival]).await?;
    }
    crate::changes::request_reevaluation(connection, workspace).await?;
    mark_replayed(connection, workspace, reject.id, event_id).await?;
    // A replayed event is an outcome like any other: watched viewers see it.
    if is_watched(connection, workspace).await? {
        write_feed(
            connection,
            workspace,
            &FeedEntry {
                source: &reject.source,
                name: Some(name),
                outcome: outcome_label(state),
                reasons,
                reason: None,
                properties: &properties,
                schema_version,
                event_id: Some(event_id),
                reject_id: Some(reject.id),
                trace_id: reject.trace_id,
            },
        )
        .await?;
        notify_feed(connection, workspace).await?;
    }
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            spec: match state {
                ValidationState::Valid => &decision_actions::EVENT_REPLAYED_VALID,
                ValidationState::Flagged => &decision_actions::EVENT_REPLAYED_FLAGGED,
                ValidationState::Unplanned => &decision_actions::EVENT_REPLAYED_UNPLANNED,
            },
            detail: None,
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
    use zirv_flow_core::{Cause, HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceName};

    use super::*;
    use crate::events::{
        EventOutcome, EventRecord, NewEventSchema, apply_event_batch, create_event_schema,
        publish_schema_draft, save_schema_draft, set_event_policy,
    };
    use crate::{create_workspace, list_trace};

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);
    const LONG: Duration = Duration::from_secs(30);

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
            source_record_id: None,
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
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
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

        let before = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();
        fix_the_schema(&pool, ws).await;
        let after = preview_replay(&pool, ws, &[reject, Uuid::now_v7()], LONG)
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

        let first = replay_rejects(&pool, ws, &key(), &[reject], &trace, false)
            .await
            .unwrap();
        let second = replay_rejects(&pool, ws, &key(), &[reject], &trace, false)
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

        replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), true)
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

    /// Replays a reject for a person with an enrollment waiting for the event and counts the
    /// `flow.step` jobs that follow.
    async fn step_jobs_after_a_replay(pool: &PgPool, emit_triggers: bool) -> i64 {
        use crate::flows::tests_support::{create, publish as publish_flow, waiting_enrollment};
        let (ws, reject) = rejected(pool).await;
        fix_the_schema(pool, ws).await;
        let person: Uuid = sqlx::query_scalar("SELECT person_id FROM event_dedupe LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap();
        let flow = create(pool, ws, "Flow").await;
        publish_flow(pool, ws, flow.id).await.unwrap();
        waiting_enrollment(pool, ws, flow.id, person, "order").await;

        replay_rejects(pool, ws, &key(), &[reject], &manual_trace(), emit_triggers)
            .await
            .unwrap();

        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_that_emits_triggers_wakes_a_waiting_enrollment(pool: PgPool) {
        assert_eq!(step_jobs_after_a_replay(&pool, true).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_flagged_no_triggers_wakes_nobody(pool: PgPool) {
        assert_eq!(step_jobs_after_a_replay(&pool, false).await, 0);
    }

    /// Replays a reject of an `order` event with a published flow whose event trigger names
    /// `order`, and counts the `flow.enroll` jobs that follow.
    async fn enroll_jobs_after_a_replay(pool: &PgPool, emit_triggers: bool) -> i64 {
        use crate::flows::tests_support::{create, publish as publish_flow};
        use crate::flows::{FlowPatch, update_flow_draft};
        let (ws, reject) = rejected(pool).await;
        fix_the_schema(pool, ws).await;
        let flow = create(pool, ws, "Flow").await;
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                settings: Some(serde_json::json!({
                    "trigger": {"type": "event", "event": "order"},
                    "reentry": {"mode": "once"}
                })),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish_flow(pool, ws, flow.id).await.unwrap();

        replay_rejects(pool, ws, &key(), &[reject], &manual_trace(), emit_triggers)
            .await
            .unwrap();

        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.enroll'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_that_emits_triggers_starts_a_flow_with_an_event_trigger(pool: PgPool) {
        assert_eq!(enroll_jobs_after_a_replay(&pool, true).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_flagged_no_triggers_starts_no_flow(pool: PgPool) {
        assert_eq!(enroll_jobs_after_a_replay(&pool, false).await, 0);
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

        let duplicate = replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(duplicate.duplicate, 1);
        assert_eq!(count(&pool, "events").await, 1);
        let resolved: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT replayed_at, redaction FROM ingest_rejects WHERE id = $1")
                .bind(reject)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(resolved.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_person_is_never_brought_back_by_a_replay(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        let person: Uuid = sqlx::query_scalar("SELECT person_id FROM ingest_rejects WHERE id = $1")
            .bind(reject)
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        let deleted = crate::delete_person(
            &mut tx,
            ws,
            person,
            crate::DeleteReason::Erasure,
            &manual_trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        assert!(deleted);

        let selected = select_rejects(&pool, ws, ReplaySelection::Ids(&[reject]))
            .await
            .unwrap()
            .unwrap();
        let preview = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();
        let totals = replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), true)
            .await
            .unwrap();

        assert!(selected.is_empty() && preview.is_empty());
        assert_eq!((totals.stored(), totals.skipped), (0, 1));
        assert_eq!(count(&pool, "events").await, 0);
        assert_eq!(count(&pool, "people").await, 0);
        assert_eq!(count(&pool, "ingest_rejects").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_fix_nothing_is_stored_and_the_reject_stays_open(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;

        let totals = replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
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
        replay_rejects(&pool, ws, &key(), &found, &manual_trace(), false)
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

        // The reject was stored while the policy already hashed `total`.
        sqlx::query("UPDATE ingest_rejects SET redaction = $2 WHERE id = $1")
            .bind(reject)
            .bind(json!({"drop": [], "hash": ["total"], "truncate_ip": []}))
            .execute(&pool)
            .await
            .unwrap();

        let prediction = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();

        assert_eq!(prediction[0].redacted_type_changed, ["total"]);
    }

    /// A block-mode `order` schema (number `total`, text `phone`, `secret`, `token`, `ip`), a
    /// policy that hashes `secret`, and one reject of an event with a text `total`.
    async fn rejected_with_values(pool: &PgPool) -> (WorkspaceId, Uuid) {
        let ws = workspace(pool).await;
        let all = |total: PropertyType| {
            vec![
                property("total", total, true),
                property("phone", PropertyType::String, false),
                property("secret", PropertyType::String, false),
                property("token", PropertyType::String, false),
                property("ip", PropertyType::String, false),
            ]
        };
        create_event_schema(
            pool,
            ws,
            &NewEventSchema {
                name: "order".into(),
                mode: EventMode::Block,
                description: String::new(),
                properties: all(PropertyType::Number),
            },
        )
        .await
        .unwrap();
        let mut tx = begin(pool).await.unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        set_event_policy(
            pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    drop: Vec::new(),
                    hash: vec!["secret".into()],
                    truncate_ip: Vec::new(),
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();
        let first = record("a@example.com", "m-0", json!({"total": 1}));
        apply_event_batch(pool, ws, &key(), &[first]).await.unwrap();
        let outcomes = apply_event_batch(
            pool,
            ws,
            &key(),
            &[record(
                "a@example.com",
                "m-1",
                json!({"total": "ten", "phone": "555-1234", "secret": "s3", "token": "tok",
                       "ip": "203.0.113.9"}),
            )],
        )
        .await
        .unwrap();
        let EventOutcome::Rejected { reject_id, .. } = outcomes[0] else {
            panic!("not rejected");
        };
        let mut tx = begin(pool).await.unwrap();
        save_schema_draft(&mut *tx, ws, "order", &all(PropertyType::String))
            .await
            .unwrap();
        publish_schema_draft(&mut tx, ws, "order", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        (ws, reject_id)
    }

    async fn stricter_rules(pool: &PgPool, ws: WorkspaceId) {
        set_event_policy(
            pool,
            ws,
            &EventPolicy {
                redaction: RedactionRules {
                    drop: vec!["phone".into()],
                    hash: vec!["secret".into(), "token".into()],
                    truncate_ip: vec!["ip".into()],
                },
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_applies_the_rules_added_since_and_never_hashes_a_path_twice(pool: PgPool) {
        let (ws, reject) = rejected_with_values(&pool).await;
        stricter_rules(&pool, ws).await;
        let (recorded, payload): (Value, Value) =
            sqlx::query_as("SELECT redaction, payload FROM ingest_rejects WHERE id = $1")
                .bind(reject)
                .fetch_one(&pool)
                .await
                .unwrap();
        let hashed_once = payload["properties"]["secret"].as_str().unwrap().to_owned();

        let preview = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();
        replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(recorded["hash"], json!(["secret"]));
        assert_eq!(hashed_once.len(), 64);
        assert!(preview[0].redaction_changed);
        let stored: Value =
            sqlx::query_scalar("SELECT properties FROM events WHERE message_id = 'm-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stored.get("phone").is_none(), "{stored}");
        assert_eq!(stored["secret"], hashed_once.as_str());
        let token = stored["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, "tok");
        assert_eq!(stored["ip"], "203.0.113.0");
        assert!(!stored.to_string().contains("555-1234"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unchanged_rules_change_nothing_and_a_legacy_reject_gets_the_full_current_rules(
        pool: PgPool,
    ) {
        let (ws, reject) = rejected_with_values(&pool).await;
        let unchanged = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();
        sqlx::query("UPDATE ingest_rejects SET redaction = NULL WHERE id = $1")
            .bind(reject)
            .execute(&pool)
            .await
            .unwrap();
        stricter_rules(&pool, ws).await;

        let legacy = preview_replay(&pool, ws, &[reject], LONG).await.unwrap();
        replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert!(!unchanged[0].redaction_changed);
        assert!(legacy[0].redaction_changed);
        let stored: Value =
            sqlx::query_scalar("SELECT properties FROM events WHERE message_id = 'm-1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stored.get("phone").is_none());
        assert_eq!(stored["token"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn replay_rules_hash_only_paths_the_record_did_not() {
        let current = RedactionRules {
            drop: vec!["a".into()],
            hash: vec!["x".into(), "y".into()],
            truncate_ip: vec!["ip".into()],
        };
        let recorded = RedactionRules {
            drop: Vec::new(),
            hash: vec!["x".into()],
            truncate_ip: Vec::new(),
        };

        let with_record = replay_rules(&current, Some(&recorded));
        let legacy = replay_rules(&current, None);

        assert_eq!(with_record.hash, ["y"]);
        assert_eq!(
            (with_record.drop.clone(), with_record.truncate_ip),
            (current.drop.clone(), current.truncate_ip.clone())
        );
        assert_eq!(legacy.hash, ["x", "y"]);
        assert!(redaction_changed(&current, Some(&recorded)));
        assert!(!redaction_changed(&current, Some(&current)));
        assert!(!redaction_changed(
            &RedactionRules {
                drop: vec![],
                hash: vec![],
                truncate_ip: vec![]
            },
            None
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_event_reaches_the_debugger_feed_and_notifies_when_watched(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        crate::touch_debugger_watch(&pool, ws).await.unwrap();
        let mut listener = crate::listen_for_debugger(&pool).await.unwrap();

        replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        let (outcome, event, rejected, properties): (String, Option<Uuid>, Option<Uuid>, Value) =
            sqlx::query_as("SELECT outcome, event_id, reject_id, properties FROM debugger_feed")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(outcome, "accepted");
        assert_eq!(rejected, Some(reject));
        let stored: Uuid = sqlx::query_scalar("SELECT id FROM events WHERE message_id = 'm-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(event, Some(stored));
        assert_eq!(properties["total"], "ten");
        let heard = tokio::time::timeout(Duration::from_secs(5), listener.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(heard, Some(ws));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_writes_no_feed_row_when_nobody_watches(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;

        replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(count(&pool, "debugger_feed").await, 0);
        assert_eq!(count(&pool, "events").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_loads_the_schemas_in_one_query_however_many_names_there_are(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        sqlx::query(
            "INSERT INTO ingest_rejects \
                (id, workspace_id, received_at, source, kind, name, reason, trace_id) \
             SELECT gen_random_uuid(), $1, now(), 'web', 'event', 'name_' || n, \
                    'validation_failed', gen_random_uuid() \
             FROM generate_series(1, 400) AS n",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let mut ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM ingest_rejects WHERE name <> 'order'")
                .fetch_all(&pool)
                .await
                .unwrap();
        ids.push(reject);
        let mut judge = Judge::new();
        let mut connection = pool.acquire().await.unwrap();

        let predictions = preview_in(&mut connection, ws, &ids, &mut judge)
            .await
            .unwrap();

        assert_eq!(predictions.len(), 401);
        assert_eq!(
            judge.schema_loads, 1,
            "one query for 401 rejects of 401 names"
        );
        let order = predictions.iter().find(|p| p.reject_id == reject).unwrap();
        assert_eq!(
            order.outcome, "accept",
            "the schema that exists is still found"
        );
        assert!(
            predictions
                .iter()
                .filter(|p| p.reject_id != reject)
                .all(|p| p.outcome == "still_rejected" && p.reason == Some("no_person"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_name_with_no_published_schema_is_remembered_so_it_is_not_asked_again(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut judge = Judge::new();
        let mut connection = pool.acquire().await.unwrap();
        let names = vec!["a".to_owned(), "b".to_owned()];

        judge.preload(&mut connection, ws, &names).await.unwrap();
        judge.preload(&mut connection, ws, &names).await.unwrap();
        let asked = judge.schema(&mut connection, ws, "a").await.unwrap();

        assert!(asked.is_none());
        assert_eq!(judge.schema_loads, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_that_stores_an_event_requests_the_reevaluation_it_marked(pool: PgPool) {
        let (ws, reject) = rejected(&pool).await;
        fix_the_schema(&pool, ws).await;
        sqlx::query("DELETE FROM jobs WHERE kind = 'segment.reevaluate'")
            .execute(&pool)
            .await
            .unwrap();

        let totals = replay_rejects(&pool, ws, &key(), &[reject], &manual_trace(), false)
            .await
            .unwrap();

        assert_eq!(totals.accepted, 1);
        assert_eq!(count(&pool, "person_changes").await, 1);
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'segment.reevaluate'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(jobs, 1);
    }
}
