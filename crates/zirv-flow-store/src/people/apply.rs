//! The profile path of apply-batch (architecture sections 6 and 7.1): validate a record,
//! resolve its person, upsert the ledger row, recompute the affected traits by precedence,
//! mark the person dirty and append decisions.
//!
//! Decision records carry static strings and ids only, never trait keys or values and never
//! identifiers.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sqlx::{Connection, PgConnection};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::contacts;
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::people::{
    Contribution, DropReason, Identifier, TraitValue, coerce, infer_type, resolve, validate_key,
};
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{OrphanPolicy, TraceContext, WorkspaceId};

use super::registry::{TraitDefinition, find_trait_definition, insert_inferred_definition};
use super::resolve::{Resolution, resolve_person};
use super::slots::{lock_and_read_writer_slots, lock_slots_shared, slot_assignments_sql};
use crate::changes::mark_person;
use crate::decisions::DecisionBuffer;
use crate::{DeleteReason, NewDecision, PgPool, StoreError, append_decision, begin, commit};

/// One record from a source, as the profile path receives it.
#[derive(Clone, Debug)]
pub struct ProfileRecord {
    pub source: String,
    pub stream: String,
    pub record_key: String,
    /// `(kind, value)` pairs, not yet normalized.
    pub identifiers: Vec<(String, String)>,
    /// Trait key to value. A JSON `null` removes this source's contribution for the key.
    pub traits: Map<String, Value>,
    pub observed_at: OffsetDateTime,
    pub trace: TraceContext,
}

/// A trait left out of a record, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroppedTrait {
    pub key: String,
    pub reason: &'static str,
}

/// What happened to one record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProfileOutcome {
    /// The record was written. `created` is true when it made a new person; `changed` is
    /// true when the person's materialized traits changed.
    Applied {
        person_id: Uuid,
        created: bool,
        changed: bool,
        dropped: Vec<DroppedTrait>,
    },
    /// The record repeats what the ledger already holds: nothing was written.
    Unchanged {
        person_id: Uuid,
        dropped: Vec<DroppedTrait>,
    },
    /// The record was refused; the rest of the batch is unaffected.
    Rejected { reason: &'static str },
}

/// Applies `records` in one transaction. Callers chunk their input (500 by default).
///
/// When the error is [`StoreError::is_retryable`] (a deadlock, a serialization failure or an
/// identity race that outlasted its retries) nothing was committed, and the caller retries
/// the whole batch.
pub async fn apply_profile_batch(
    pool: &PgPool,
    workspace: WorkspaceId,
    records: &[ProfileRecord],
) -> Result<Vec<ProfileOutcome>, StoreError> {
    let mut tx = begin(pool).await?;
    let outcomes = apply_profile_records(&mut tx, workspace, records).await?;
    crate::changes::request_reevaluation(&mut tx, workspace).await?;
    commit(tx).await?;
    Ok(outcomes)
}

/// [`apply_profile_batch`] inside the caller's transaction, which the caller commits.
pub(crate) async fn apply_profile_records(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    records: &[ProfileRecord],
) -> Result<Vec<ProfileOutcome>, StoreError> {
    crate::changes::cache_change_readers(connection, workspace).await?;
    // Before any row lock; see `slots` for the order.
    lock_slots_shared(connection, workspace).await?;
    let mut outcomes = Vec::with_capacity(records.len());
    for record in records {
        outcomes.push(apply_profile_record(connection, workspace, record).await?);
    }
    Ok(outcomes)
}

/// [`apply_profile_records`] with each record in a savepoint of its own: a record the store
/// cannot apply is `None` (the caller rejects it as `apply_failed`) and the rest carry on. A
/// retryable error still fails the whole batch.
pub(crate) async fn apply_profile_records_singly(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    records: &[ProfileRecord],
) -> Result<Vec<Option<ProfileOutcome>>, StoreError> {
    crate::changes::cache_change_readers(connection, workspace).await?;
    lock_slots_shared(connection, workspace).await?;
    let mut outcomes = Vec::with_capacity(records.len());
    for record in records {
        let mut savepoint = connection.begin().await.map_err(StoreError::query)?;
        match apply_profile_record(&mut savepoint, workspace, record).await {
            Ok(outcome) => {
                savepoint.commit().await.map_err(StoreError::query)?;
                outcomes.push(Some(outcome));
            }
            Err(error) if error.is_retryable() => return Err(error),
            Err(_) => {
                savepoint.rollback().await.map_err(StoreError::query)?;
                outcomes.push(None);
            }
        }
    }
    Ok(outcomes)
}

/// Retracts one source record's contribution, in one transaction:
///
/// - the row is tombstoned and the person's traits are recomputed from what remains;
/// - each identity the record listed that no other live record of the person lists is
///   deleted, with the conflicts about it (an identity first seen only through events is
///   removed too; the next event creates it again);
/// - the person is marked dirty and a `contribution.removed` decision is appended;
/// - when the workspace's orphan policy is `delete` and the person is left with no live
///   record, no event and no explicit consent, the person is deleted.
///
/// - the events that name the record (`source_record_id`) are deleted with the rows that refer
///   to them, so a record-linked event goes with its record; first-party events and the events
///   of other records stay.
///
/// Returns false when no live record matched.
pub async fn remove_contribution(
    pool: &PgPool,
    workspace: WorkspaceId,
    source: &str,
    stream: &str,
    record_key: &str,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut tx = begin(pool).await?;
    let removed =
        remove_contribution_in(&mut tx, workspace, source, stream, record_key, trace).await?;
    if removed.is_some() {
        crate::changes::request_reevaluation(&mut tx, workspace).await?;
        commit(tx).await?;
    }
    Ok(removed.is_some())
}

/// The ledger row [`remove_contribution_in`] removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RemovedRecord {
    pub(crate) record_id: Uuid,
}

/// [`remove_contribution`] inside the caller's transaction, which the caller commits.
pub(crate) async fn remove_contribution_in(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: &str,
    stream: &str,
    record_key: &str,
    trace: &TraceContext,
) -> Result<Option<RemovedRecord>, StoreError> {
    lock_slots_shared(connection, workspace).await?;
    // The record is locked first, then its person: the order the drain's existing-record path
    // uses. With the record locked a merge cannot move it, so the owner cannot change.
    let record: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, person_id FROM source_records \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = $4 \
           AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .bind(stream)
    .bind(record_key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some((locked_id, owner)) = record else {
        return Ok(None);
    };
    sqlx::query("SELECT id FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE")
        .bind(workspace.as_uuid())
        .bind(owner)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let (record_id, person_id, traits, identifiers): (Uuid, Uuid, Value, Value) = sqlx::query_as(
        "UPDATE source_records SET deleted_at = now(), updated_at = now() \
             WHERE id = $1 RETURNING id, person_id, traits, identifiers",
    )
    .bind(locked_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let keys: Vec<String> = traits
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    if recompute(&mut *connection, workspace, person_id, &keys).await? {
        record_decision(
            &mut *connection,
            workspace,
            trace,
            &decision_actions::TRAIT_CHANGED,
            None,
            vec![("person", person_id), ("source_record", record_id)],
        )
        .await?;
    }
    remove_unshared_identities(
        &mut *connection,
        workspace,
        person_id,
        record_id,
        &identifiers,
        trace,
    )
    .await?;
    remove_record_events(&mut *connection, workspace, record_id).await?;
    mark_dirty(
        &mut *connection,
        workspace,
        person_id,
        ChangeClasses::TRAITS | ChangeClasses::EVENTS,
        trace,
    )
    .await?;
    record_decision(
        &mut *connection,
        workspace,
        trace,
        &decision_actions::CONTRIBUTION_REMOVED,
        None,
        vec![("person", person_id), ("source_record", record_id)],
    )
    .await?;
    if crate::orphan_policy(&mut *connection, workspace).await? == Some(OrphanPolicy::Delete)
        && is_orphan(&mut *connection, workspace, person_id).await?
    {
        crate::delete_person(
            &mut *connection,
            workspace,
            person_id,
            DeleteReason::Orphan,
            trace,
        )
        .await?;
    }
    Ok(Some(RemovedRecord { record_id }))
}

/// Deletes the events of one ledger record, and the dedupe and debugger rows that name them, in
/// the removal's transaction.
async fn remove_record_events(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record_id: Uuid,
) -> Result<(), StoreError> {
    let events: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM events WHERE workspace_id = $1 AND source_record_id = $2 RETURNING id",
    )
    .bind(workspace.as_uuid())
    .bind(record_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if events.is_empty() {
        return Ok(());
    }
    for statement in [
        "DELETE FROM debugger_feed WHERE workspace_id = $1 AND event_id = ANY($2)",
        "DELETE FROM event_dedupe WHERE workspace_id = $1 AND event_id = ANY($2)",
    ] {
        sqlx::query(statement)
            .bind(workspace.as_uuid())
            .bind(&events)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    }
    Ok(())
}

/// Deletes each identity in `identifiers` (the removed record's `[{kind, value}]`) that
/// belongs to the person and that no other live record of the person lists, together with
/// the conflicts about it.
async fn remove_unshared_identities(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person_id: Uuid,
    record_id: Uuid,
    identifiers: &Value,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    let doomed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM identities i \
         WHERE i.workspace_id = $1 AND i.person_id = $2 \
           AND EXISTS (SELECT 1 FROM jsonb_array_elements($3::jsonb) e \
                       WHERE e ->> 'kind' = i.kind AND e ->> 'value' = i.value) \
           AND NOT EXISTS (SELECT 1 FROM source_records s \
                           WHERE s.workspace_id = $1 AND s.person_id = $2 \
                             AND s.deleted_at IS NULL AND s.id <> $4 \
                             AND s.identifiers @> jsonb_build_array( \
                                 jsonb_build_object('kind', i.kind, 'value', i.value)))",
    )
    .bind(workspace.as_uuid())
    .bind(person_id)
    .bind(identifiers)
    .bind(record_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if doomed.is_empty() {
        return Ok(());
    }
    for statement in [
        "DELETE FROM identity_conflicts WHERE workspace_id = $1 AND identity_id = ANY($2)",
        "DELETE FROM identities WHERE workspace_id = $1 AND id = ANY($2)",
    ] {
        sqlx::query(statement)
            .bind(workspace.as_uuid())
            .bind(&doomed)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    }
    mark_dirty(
        connection,
        workspace,
        person_id,
        ChangeClasses::IDENTIFIERS,
        trace,
    )
    .await
}

/// Whether the person has no live source record, no event and no explicit consent.
async fn is_orphan(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person_id: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM source_records \
                            WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL) \
            AND NOT EXISTS (SELECT 1 FROM events WHERE workspace_id = $1 AND person_id = $2) \
            AND NOT EXISTS (SELECT 1 FROM consent \
                            WHERE workspace_id = $1 AND person_id = $2 AND explicit)",
    )
    .bind(workspace.as_uuid())
    .bind(person_id)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

pub(crate) async fn apply_profile_record(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
) -> Result<ProfileOutcome, StoreError> {
    let mut decisions = DecisionBuffer::default();
    let outcome = apply_profile_record_into(connection, workspace, record, &mut decisions).await?;
    decisions.flush(connection).await?;
    Ok(outcome)
}

/// [`apply_profile_record`] that leaves its decisions in `decisions` for the caller to flush in
/// the same transaction, so a drain batch appends them in one statement.
pub(crate) async fn apply_profile_record_into(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
    decisions: &mut DecisionBuffer,
) -> Result<ProfileOutcome, StoreError> {
    // First, before any row lock: see the lock order in `slots`.
    lock_slots_shared(connection, workspace).await?;
    let identifiers = match normalize_identifiers(&record.identifiers) {
        Ok(identifiers) => identifiers,
        Err(reason) => return Ok(ProfileOutcome::Rejected { reason }),
    };
    let (updates, dropped) = prepare_traits(connection, workspace, &record.traits).await?;
    // A flow's write is a new contribution each time it is made: the time is part of its hash, so
    // writing the same value again after another source won still takes part in latest-wins.
    // Other sources keep deduplicating a replay, whatever its `observed_at`.
    let stamp = record
        .source
        .starts_with("flow:")
        .then_some(record.observed_at);
    let hash = content_hash(&identifiers, &updates, stamp);

    let existing = find_ledger_row(connection, workspace, record).await?;
    if let Some(row) = &existing
        && row.deleted_at.is_none()
        && row.content_hash == hash
    {
        return Ok(ProfileOutcome::Unchanged {
            person_id: row.person_id,
            dropped,
        });
    }

    let resolution = resolve_person(
        connection,
        workspace,
        &identifiers,
        existing.as_ref().map(|row| row.person_id),
        &record.trace,
    )
    .await?;
    let person_id = resolution.person_id;
    // A merge in the resolution moved the record's old person into this one.
    let previous = existing
        .as_ref()
        .map(|row| row.person_id)
        .filter(|previous| !resolution.merged.contains(previous));
    let (record_id, ledger_keys) = upsert_ledger_row(
        connection,
        workspace,
        record,
        existing.as_ref(),
        person_id,
        &Normalized {
            identifiers: &identifiers,
            updates: &updates,
            hash: &hash,
        },
    )
    .await?;

    if resolution.created {
        buffer_decision(
            decisions,
            workspace,
            &record.trace,
            &decision_actions::PERSON_CREATED,
            None,
            vec![("person", person_id), ("source_record", record_id)],
        );
    }
    record_resolution(
        connection,
        decisions,
        workspace,
        &record.trace,
        person_id,
        &resolution,
        Some(record_id),
    )
    .await?;

    // A record that moved to another person takes its traits away from the previous one.
    if let Some(row) = &existing
        && let Some(previous) = previous
        && previous != person_id
        && recompute(connection, workspace, previous, &row.trait_keys()).await?
    {
        mark_dirty(
            connection,
            workspace,
            previous,
            ChangeClasses::TRAITS,
            &record.trace,
        )
        .await?;
        buffer_decision(
            decisions,
            workspace,
            &record.trace,
            &decision_actions::TRAIT_CHANGED,
            None,
            vec![("person", previous), ("source_record", record_id)],
        );
    }

    // Every key the row now holds, plus the keys it just retracted. A record that moved
    // brings all its keys to the new person.
    let mut keys = ledger_keys;
    keys.extend(updates.iter().map(|(key, _)| key.clone()));
    keys.sort();
    keys.dedup();
    let changed = recompute(connection, workspace, person_id, &keys).await?;
    if changed || resolution.created {
        mark_dirty(
            connection,
            workspace,
            person_id,
            ChangeClasses::TRAITS,
            &record.trace,
        )
        .await?;
    }
    if changed {
        buffer_decision(
            decisions,
            workspace,
            &record.trace,
            &decision_actions::TRAIT_CHANGED,
            None,
            vec![("person", person_id), ("source_record", record_id)],
        );
    }
    Ok(ProfileOutcome::Applied {
        person_id,
        created: resolution.created,
        changed,
        dropped,
    })
}

/// Records what a resolution did beyond picking a person: the split from the person the
/// record would have joined, identifiers left off a full person, and the identifiers shared
/// with people it could not merge into. A conflict is one open row per person pair and
/// source record, with a decision for each new one. The source record is absent for an event.
pub(crate) async fn record_resolution(
    connection: &mut PgConnection,
    decisions: &mut DecisionBuffer,
    workspace: WorkspaceId,
    trace: &TraceContext,
    person_id: Uuid,
    resolution: &Resolution,
    source_record_id: Option<Uuid>,
) -> Result<(), StoreError> {
    let with_source = |mut subjects: Vec<(&'static str, Uuid)>| {
        subjects.extend(source_record_id.map(|id| ("source_record", id)));
        subjects
    };
    if let Some(previous) = resolution.split_from {
        buffer_decision(
            decisions,
            workspace,
            trace,
            &decision_actions::IDENTITY_SPLIT,
            None,
            with_source(vec![("person", person_id), ("previous_person", previous)]),
        );
    }
    if resolution.skipped > 0 {
        buffer_decision(
            decisions,
            workspace,
            trace,
            &decision_actions::IDENTITY_LIMIT,
            None,
            with_source(vec![("person", person_id)]),
        );
    }
    for conflict in &resolution.conflicts {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason, \
                 identity_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (workspace_id, person_id, other_person_id, \
                          (COALESCE(source_record_id, '00000000-0000-0000-0000-000000000000'::uuid)), \
                          (COALESCE(identity_id, '00000000-0000-0000-0000-000000000000'::uuid))) \
                WHERE resolved_at IS NULL DO NOTHING \
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(workspace.as_uuid())
        .bind(person_id)
        .bind(conflict.other_person_id)
        .bind(&conflict.kind)
        .bind(source_record_id)
        .bind(conflict.reason)
        .bind(conflict.identity_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if inserted.is_some() {
            buffer_decision(
                decisions,
                workspace,
                trace,
                &decision_actions::IDENTITY_CONFLICT_RECORDED,
                Some(conflict.reason),
                vec![
                    ("person", person_id),
                    ("other_person", conflict.other_person_id),
                ],
            );
        }
    }
    Ok(())
}

pub(crate) fn normalize_identifiers(
    raw: &[(String, String)],
) -> Result<Vec<Identifier>, &'static str> {
    let mut identifiers = Vec::with_capacity(raw.len());
    for (kind, value) in raw {
        identifiers.push(Identifier::parse(kind, value).map_err(|error| error.reason())?);
    }
    if identifiers.is_empty() {
        return Err("no_identifier");
    }
    identifiers
        .sort_by(|a, b| (a.kind().as_text(), a.value()).cmp(&(b.kind().as_text(), b.value())));
    identifiers.dedup();
    Ok(identifiers)
}

/// The accepted changes (`None` removes the key) and the dropped traits of a record.
type Prepared = (Vec<(String, Option<Value>)>, Vec<DroppedTrait>);

async fn prepare_traits(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    traits: &Map<String, Value>,
) -> Result<Prepared, StoreError> {
    let mut sorted: Vec<(&String, &Value)> = traits.iter().collect();
    sorted.sort_by_key(|(key, _)| *key);
    let mut updates = Vec::new();
    let mut dropped = Vec::new();
    for (key, value) in sorted {
        let mut drop = |reason: DropReason| {
            dropped.push(DroppedTrait {
                key: key.clone(),
                reason: reason.as_str(),
            });
        };
        if validate_key(key).is_err() {
            drop(DropReason::InvalidKey);
            continue;
        }
        let mut definition = find_trait_definition(&mut *connection, workspace, key).await?;
        if definition.as_ref().is_some_and(|d| d.archived) {
            drop(DropReason::Archived);
            continue;
        }
        if value.is_null() {
            updates.push((key.clone(), None));
            continue;
        }
        if definition.is_none() {
            let inferred = match infer_type(value) {
                Ok(inferred) => inferred,
                Err(reason) => {
                    drop(reason);
                    continue;
                }
            };
            insert_inferred_definition(connection, workspace, key, inferred).await?;
            definition = find_trait_definition(&mut *connection, workspace, key).await?;
        }
        let Some(definition) = definition else {
            drop(DropReason::NotInferable);
            continue;
        };
        if definition.archived {
            drop(DropReason::Archived);
            continue;
        }
        match coerce(definition.trait_type, value) {
            Ok(accepted) => updates.push((key.clone(), Some(to_json(&accepted)))),
            Err(reason) => drop(reason),
        }
    }
    Ok((updates, dropped))
}

fn to_json(value: &TraitValue) -> Value {
    match value {
        TraitValue::String(text) | TraitValue::Timestamp(text) | TraitValue::Date(text) => {
            Value::String(text.clone())
        }
        TraitValue::Number(number) => Value::Number(number.clone()),
        TraitValue::Boolean(flag) => Value::Bool(*flag),
        TraitValue::StringList(items) => {
            Value::Array(items.iter().cloned().map(Value::String).collect())
        }
    }
}

/// SHA-256 of the canonical JSON of the normalized identifiers and trait values. Times are
/// left out so a replay with a new `observed_at` is still a no-op, unless the caller passes
/// `written_at` (a flow write, which is a new contribution each time).
fn content_hash(
    identifiers: &[Identifier],
    updates: &[(String, Option<Value>)],
    written_at: Option<OffsetDateTime>,
) -> Vec<u8> {
    let identifiers: Vec<[String; 2]> = identifiers
        .iter()
        .map(|i| [i.kind().as_text(), i.value().to_owned()])
        .collect();
    let traits: BTreeMap<&str, &Option<Value>> = updates
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let mut canonical = serde_json::json!({ "identifiers": identifiers, "traits": traits });
    if let Some(at) = written_at {
        canonical["at"] = serde_json::json!(at.unix_timestamp_nanos().to_string());
    }
    sha256(&canonical.to_string()).to_vec()
}

#[derive(sqlx::FromRow)]
struct LedgerRow {
    id: Uuid,
    person_id: Uuid,
    identifiers: Value,
    traits: Value,
    content_hash: Vec<u8>,
    deleted_at: Option<OffsetDateTime>,
}

impl LedgerRow {
    fn trait_keys(&self) -> Vec<String> {
        self.traits
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default()
    }
}

async fn find_ledger_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
) -> Result<Option<LedgerRow>, StoreError> {
    sqlx::query_as(
        "SELECT id, person_id, identifiers, traits, content_hash, deleted_at \
         FROM source_records \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = $4 \
         FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&record.source)
    .bind(&record.stream)
    .bind(&record.record_key)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

fn decode_error(what: &'static str) -> StoreError {
    StoreError::query(sqlx::Error::Decode(what.into()))
}

/// A record after validation: the normalized identifiers, the accepted trait changes and the
/// content hash of both.
struct Normalized<'a> {
    identifiers: &'a [Identifier],
    updates: &'a [(String, Option<Value>)],
    hash: &'a [u8],
}

async fn upsert_ledger_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
    existing: Option<&LedgerRow>,
    person_id: Uuid,
    normalized: &Normalized<'_>,
) -> Result<(Uuid, Vec<String>), StoreError> {
    let Normalized {
        identifiers,
        updates,
        hash,
    } = normalized;
    let at = record
        .observed_at
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| decode_error("observed_at cannot be formatted"))?;
    // A revived tombstone starts empty, so what was deleted never comes back.
    let live = existing.filter(|row| row.deleted_at.is_none());
    let mut traits = live
        .and_then(|row| row.traits.as_object().cloned())
        .unwrap_or_default();
    for (key, value) in *updates {
        match value {
            Some(value) => {
                traits.insert(key.clone(), serde_json::json!({ "v": value, "at": at }));
            }
            None => {
                traits.remove(key);
            }
        }
    }
    let previous: Vec<(String, String)> = live
        .and_then(|row| row.identifiers.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("kind")?.as_str()?.to_owned(),
                entry.get("value")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    // A source's record only ever adds identifiers, except a native contact's, which lists
    // exactly the identifiers its fields name now.
    let replaces = record.source == contacts::SOURCE && record.stream == contacts::STREAM;
    let mut current: Vec<(String, String)> = if replaces {
        Vec::new()
    } else {
        previous.clone()
    };
    current.extend(
        identifiers
            .iter()
            .map(|i| (i.kind().as_text(), i.value().to_owned())),
    );
    current.sort();
    current.dedup();
    let released: Vec<Value> = if replaces {
        previous
            .iter()
            .filter(|pair| !current.contains(pair))
            .map(|(kind, value)| serde_json::json!({ "kind": kind, "value": value }))
            .collect()
    } else {
        Vec::new()
    };
    let stored: Vec<Value> = current
        .into_iter()
        .map(|(kind, value)| serde_json::json!({ "kind": kind, "value": value }))
        .collect();

    let keys: Vec<String> = traits.keys().cloned().collect();
    match existing {
        Some(row) => {
            sqlx::query(
                "UPDATE source_records SET person_id = $3, identifiers = $4, traits = $5, \
                    content_hash = $6, deleted_at = NULL, updated_at = now() \
                 WHERE workspace_id = $1 AND id = $2",
            )
            .bind(workspace.as_uuid())
            .bind(row.id)
            .bind(person_id)
            .bind(Value::Array(stored))
            .bind(Value::Object(traits))
            .bind(hash)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            // An identity this record no longer lists goes, unless another live record of
            // the person lists it.
            if !released.is_empty() {
                remove_unshared_identities(
                    connection,
                    workspace,
                    person_id,
                    row.id,
                    &Value::Array(released),
                    &record.trace,
                )
                .await?;
            }
            Ok((row.id, keys))
        }
        None => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO source_records \
                    (id, workspace_id, source, stream, record_key, person_id, identifiers, \
                     traits, content_hash) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(id)
            .bind(workspace.as_uuid())
            .bind(&record.source)
            .bind(&record.stream)
            .bind(&record.record_key)
            .bind(person_id)
            .bind(Value::Array(stored))
            .bind(Value::Object(traits))
            .bind(hash)
            .execute(connection)
            .await
            .map_err(StoreError::query)?;
            Ok((id, keys))
        }
    }
}

/// Recomputes `keys` of one person from the live ledger rows, and writes the person only if
/// a value or its source changed. Returns whether a value changed.
pub(super) async fn recompute(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person_id: Uuid,
    keys: &[String],
) -> Result<bool, StoreError> {
    let person: Option<(Value, Value)> = sqlx::query_as(
        "SELECT traits, trait_sources FROM people WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(person_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some((traits, sources)) = person else {
        return Ok(false);
    };
    let (mut traits, mut sources) = (
        traits.as_object().cloned().unwrap_or_default(),
        sources.as_object().cloned().unwrap_or_default(),
    );
    let rows: Vec<(Uuid, String, Value)> = sqlx::query_as(
        "SELECT id, source, traits FROM source_records \
         WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(person_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    let (mut values_changed, mut sources_changed) = (false, false);
    for key in keys {
        let definition: Option<TraitDefinition> =
            find_trait_definition(&mut *connection, workspace, key).await?;
        let mut contributions = Vec::new();
        // An archived key is not materialized; the ledger keeps its values for an unarchive.
        let archived = definition.as_ref().is_some_and(|d| d.archived);
        for (record_id, source, record_traits) in rows.iter().filter(|_| !archived) {
            let Some(entry) = record_traits.get(key) else {
                continue;
            };
            let (Some(value), Some(at)) = (entry.get("v"), entry.get("at").and_then(Value::as_str))
            else {
                return Err(decode_error("malformed ledger trait"));
            };
            contributions.push(Contribution {
                source: source.clone(),
                at: OffsetDateTime::parse(at, &Rfc3339)
                    .map_err(|_| decode_error("malformed ledger time"))?,
                record_id: *record_id,
                value: value.clone(),
            });
        }
        let (precedence, order) = definition.map_or_else(
            || (zirv_flow_core::people::Precedence::Latest, Vec::new()),
            |d| (d.precedence, d.source_order),
        );
        match resolve(precedence, &order, &contributions) {
            Some(winner) => {
                if !traits
                    .get(key)
                    .is_some_and(|current| same_value(current, &winner.value))
                {
                    traits.insert(key.clone(), winner.value.clone());
                    values_changed = true;
                }
                let source = Value::String(winner.source.clone());
                if sources.get(key) != Some(&source) {
                    sources.insert(key.clone(), source);
                    sources_changed = true;
                }
            }
            None => {
                values_changed |= traits.remove(key).is_some();
                sources_changed |= sources.remove(key).is_some();
            }
        }
    }
    if values_changed || sources_changed {
        // The writer lock comes in a statement of its own, before the read of the assignments,
        // and the slots are written by the statement that writes `traits`.
        let slots = lock_and_read_writer_slots(connection, workspace).await?;
        let sql = format!(
            "UPDATE people SET traits = $3, trait_sources = $4, updated_at = now(){} \
             WHERE workspace_id = $1 AND id = $2",
            slot_assignments_sql(&slots, "$3::jsonb", 5)
        );
        let mut update = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(workspace.as_uuid())
            .bind(person_id)
            .bind(Value::Object(traits))
            .bind(Value::Object(sources));
        for (key, _) in &slots {
            update = update.bind(key.as_str());
        }
        update
            .execute(connection)
            .await
            .map_err(StoreError::query)?;
    }
    Ok(values_changed)
}

/// Whether two stored values are the same for change detection: numbers compare by numeric
/// value (5 and 5.0 are equal; two integers compare exactly as integers, and an integer
/// equals a float only when the float is that exact integer), everything else exactly. Storage always keeps the number as sent.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if let (Some(i), Some(j)) = (exact_integer(x), exact_integer(y)) {
                i == j
            } else if let (Some(integer), Some(float)) = (exact_integer(x), y.as_f64()) {
                float_is_integer(float, integer)
            } else if let (Some(float), Some(integer)) = (x.as_f64(), exact_integer(y)) {
                float_is_integer(float, integer)
            } else {
                x.as_f64() == y.as_f64()
            }
        }
        _ => a == b,
    }
}

/// The value of a JSON number that is stored as an integer.
fn exact_integer(number: &serde_json::Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

/// Whether `float` is integral, in range, and exactly `integer`. A plain `f64` comparison
/// would round a large integer to the nearest float.
fn float_is_integer(float: f64, integer: i128) -> bool {
    // 2^127 is the first float outside i128, so every float below it converts exactly.
    float.fract() == 0.0 && float.abs() < 2f64.powi(127) && float as i128 == integer
}

/// Marks the person dirty in `classes`, on the trace of the change (see `changes`).
pub(super) async fn mark_dirty(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person_id: Uuid,
    classes: ChangeClasses,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    mark_person(connection, workspace, person_id, classes, trace, None).await
}

/// Queues a decision for the caller's flush; the same record as [`record_decision`].
pub(super) fn buffer_decision(
    decisions: &mut DecisionBuffer,
    workspace: WorkspaceId,
    trace: &TraceContext,
    spec: &'static DecisionSpec,
    detail: Option<&'static str>,
    subject_ids: Vec<(&'static str, Uuid)>,
) {
    decisions.push(NewDecision {
        graph: None,
        workspace_id: workspace,
        trace: trace.clone(),
        spec,
        detail,
        subject_ids,
        job: None,
    });
}

pub(super) async fn record_decision(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    trace: &TraceContext,
    spec: &'static DecisionSpec,
    detail: Option<&'static str>,
    subject_ids: Vec<(&'static str, Uuid)>,
) -> Result<(), StoreError> {
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            spec,
            detail,
            subject_ids,
            job: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::registry::{
        NewTraitDefinition, TraitDefinitionUpdate, create_trait_definition, update_trait_definition,
    };
    use crate::people::{find_person, list_identities, list_identity_conflicts};
    use crate::{create_workspace, list_trace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("test"))
    }

    fn record(
        source: &str,
        key: &str,
        email: &str,
        traits: Value,
        at: OffsetDateTime,
    ) -> ProfileRecord {
        ProfileRecord {
            source: source.to_owned(),
            stream: "contacts".to_owned(),
            record_key: key.to_owned(),
            identifiers: vec![("email".to_owned(), email.to_owned())],
            traits: traits.as_object().cloned().unwrap(),
            observed_at: at,
            trace: trace(),
        }
    }

    async fn define(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        precedence: Precedence,
        order: &[&str],
    ) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.to_owned(),
                trait_type: zirv_flow_core::people::TraitType::String,
                precedence,
                source_order: order.iter().map(|s| (*s).to_owned()).collect(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    fn person_of(outcome: &ProfileOutcome) -> Uuid {
        match outcome {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn trait_of(pool: &PgPool, ws: WorkspaceId, person: Uuid, key: &str) -> Option<Value> {
        let person = find_person(pool, ws, person).await.unwrap().unwrap();
        person.traits.get(key).cloned()
    }

    /// Stores one event of `person`, linked to `record` when given, with its dedupe row and a
    /// debugger feed row, and returns its id.
    async fn linked_event(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        record: Option<Uuid>,
        occurred_at: OffsetDateTime,
        message_id: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        crate::events::insert_event(
            &mut connection,
            ws,
            &crate::events::NewEvent {
                id,
                person_id: person,
                name: "seen",
                schema_version: None,
                occurred_at,
                received_at: occurred_at,
                properties: &serde_json::Map::new(),
                source: "web",
                message_id: Some(message_id),
                trace_id: Uuid::now_v7(),
                state: zirv_flow_core::events::ValidationState::Valid,
                reasons: &[],
                flags: &[],
                late: false,
                source_record_id: record,
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO event_dedupe (workspace_id, message_id, event_id, event_occurred_at, \
                person_id) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(ws.as_uuid())
        .bind(message_id)
        .bind(id)
        .bind(occurred_at)
        .bind(person)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO debugger_feed (workspace_id, source, name, outcome, event_id, trace_id) \
             VALUES ($1, 'web', 'seen', 'accepted', $2, $3)",
        )
        .bind(ws.as_uuid())
        .bind(id)
        .bind(Uuid::now_v7())
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn event_ids(pool: &PgPool) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM events ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_a_record_deletes_the_events_that_name_it_in_every_partition_and_nothing_else(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other_ws = workspace(&pool, "two").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                record("crm", "1", "a@example.com", json!({}), at),
                record("crm", "2", "a@example.com", json!({}), at),
                record("crm", "3", "b@example.com", json!({}), at),
            ],
        )
        .await
        .unwrap();
        let (person, other_person) = (person_of(&outcomes[0]), person_of(&outcomes[2]));
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM source_records WHERE workspace_id = $1 ORDER BY record_key",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        let (first, second, third) = (ids[0], ids[1], ids[2]);

        // Events of the first record in the current month, an old month (its own partition) and
        // a year no partition covers (the default partition).
        let now = OffsetDateTime::now_utc();
        let mut doomed = Vec::new();
        for (n, when) in [
            now,
            now - time::Duration::days(70),
            datetime!(2001-01-01 00:00 UTC),
        ]
        .into_iter()
        .enumerate()
        {
            doomed.push(linked_event(&pool, ws, person, Some(first), when, &format!("d{n}")).await);
        }
        let partitions: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT tableoid) FROM events WHERE source_record_id = $1",
        )
        .bind(first)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(partitions, 3, "the events sit in three partitions");
        // Kept: a first-party event of the same person, an event of the person's other record, an
        // event of another person's record.
        let first_party = linked_event(&pool, ws, person, None, now, "k0").await;
        let sibling = linked_event(&pool, ws, person, Some(second), now, "k1").await;
        let stranger = linked_event(&pool, ws, other_person, Some(third), now, "k2").await;
        // The same record id in another workspace is not this record.
        let foreign_person = person_of(
            &apply_profile_batch(
                &pool,
                other_ws,
                &[record("crm", "1", "c@example.com", json!({}), at)],
            )
            .await
            .unwrap()[0],
        );
        let foreign = linked_event(&pool, other_ws, foreign_person, Some(first), now, "k3").await;

        let removed = remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();

        assert!(removed);
        let mut kept = vec![first_party, sibling, stranger, foreign];
        kept.sort();
        assert_eq!(event_ids(&pool).await, kept);
        for id in &doomed {
            let dedupe: i64 =
                sqlx::query_scalar("SELECT count(*) FROM event_dedupe WHERE event_id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            let feed: i64 =
                sqlx::query_scalar("SELECT count(*) FROM debugger_feed WHERE event_id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!((dedupe, feed), (0, 0), "{id}");
        }
        let dedupe_left: i64 = sqlx::query_scalar("SELECT count(*) FROM event_dedupe")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(dedupe_left, 4);
        assert!(find_person(&pool, ws, person).await.unwrap().is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_orphan_policy_decides_a_person_whose_only_record_and_events_went(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                record("crm", "1", "keep@example.com", json!({}), at),
                record("crm", "2", "gone@example.com", json!({}), at),
            ],
        )
        .await
        .unwrap();
        let (keep, gone) = (person_of(&outcomes[0]), person_of(&outcomes[1]));
        let records: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM source_records WHERE workspace_id = $1 ORDER BY record_key",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        let now = OffsetDateTime::now_utc();
        linked_event(&pool, ws, keep, Some(records[0]), now, "e1").await;
        linked_event(&pool, ws, gone, Some(records[1]), now, "e2").await;

        // Under `keep` the person stays, without the record and the event.
        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        assert!(find_person(&pool, ws, keep).await.unwrap().is_some());
        assert_eq!(event_ids(&pool).await.len(), 1);

        // Under `delete` the event does not keep the person alive: it went with its record.
        crate::set_orphan_policy(&pool, ws, OrphanPolicy::Delete)
            .await
            .unwrap();
        remove_contribution(&pool, ws, "crm", "contacts", "2", &trace())
            .await
            .unwrap();
        assert!(find_person(&pool, ws, gone).await.unwrap().is_none());
        assert!(event_ids(&pool).await.is_empty());
    }

    #[test]
    fn identifiers_are_normalized_sorted_and_deduplicated() {
        let raw = vec![
            ("phone".to_owned(), "+45 1234 5678".to_owned()),
            ("email".to_owned(), "A@Example.com".to_owned()),
            ("email".to_owned(), "a@example.com".to_owned()),
        ];

        let identifiers = normalize_identifiers(&raw).unwrap();

        assert_eq!(identifiers.len(), 2);
        assert_eq!(identifiers[0].value(), "a@example.com");
        assert_eq!(identifiers[1].value(), "+4512345678");
    }

    #[test]
    fn a_bad_or_missing_identifier_is_a_typed_rejection() {
        let bad = vec![("email".to_owned(), "nope".to_owned())];
        let fax = vec![("fax".to_owned(), "1".to_owned())];

        assert_eq!(normalize_identifiers(&bad).unwrap_err(), "invalid_email");
        assert_eq!(
            normalize_identifiers(&fax).unwrap_err(),
            "unknown_identifier_kind"
        );
        assert_eq!(normalize_identifiers(&[]).unwrap_err(), "no_identifier");
    }

    #[test]
    fn the_content_hash_ignores_times_and_key_order_but_not_values() {
        let identifiers =
            normalize_identifiers(&[("email".to_owned(), "a@b.co".to_owned())]).unwrap();
        let one = vec![("a".to_owned(), Some(json!(1))), ("b".to_owned(), None)];
        let swapped = vec![("b".to_owned(), None), ("a".to_owned(), Some(json!(1)))];
        let other = vec![("a".to_owned(), Some(json!(2))), ("b".to_owned(), None)];

        assert_eq!(
            content_hash(&identifiers, &one, None),
            content_hash(&identifiers, &swapped, None)
        );
        assert_ne!(
            content_hash(&identifiers, &one, None),
            content_hash(&identifiers, &other, None)
        );
    }

    #[test]
    fn whole_numbers_stay_integers_in_json() {
        assert_eq!(to_json(&TraitValue::Number(5.into())), json!(5));
        let big = 9_007_199_254_740_993_i64;
        assert_eq!(to_json(&TraitValue::Number(big.into())), json!(big));
        assert_eq!(
            to_json(&TraitValue::Number(big.into())).to_string(),
            "9007199254740993"
        );
        assert_eq!(
            to_json(&TraitValue::Number(
                serde_json::Number::from_f64(2.5).unwrap()
            )),
            json!(2.5)
        );
        assert_eq!(
            to_json(&TraitValue::StringList(vec!["a".into()])),
            json!(["a"])
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_first_record_creates_a_person_with_identities_traits_and_decisions(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::TRAITS
                | zirv_flow_core::segment::ChangeClasses::IDENTIFIERS,
        )
        .await;
        let at = datetime!(2026-05-01 10:00 UTC);
        let mut rec = record(
            "crm",
            "1",
            "Ada@Example.com",
            json!({"plan": "pro", "age": 36}),
            at,
        );
        rec.identifiers
            .push(("phone".into(), "+45 12345678".into()));

        let outcomes = apply_profile_batch(&pool, ws, &[rec.clone()])
            .await
            .unwrap();

        let person = person_of(&outcomes[0]);
        assert!(matches!(
            outcomes[0],
            ProfileOutcome::Applied {
                created: true,
                changed: true,
                ..
            }
        ));
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("pro"))
        );
        assert_eq!(trait_of(&pool, ws, person, "age").await, Some(json!(36)));
        let identities = list_identities(&pool, ws, person).await.unwrap();
        assert_eq!(identities.len(), 2);
        let dirty: i64 =
            sqlx::query_scalar("SELECT count(*) FROM person_changes WHERE person_id = $1")
                .bind(person)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dirty, 1);
        let (decisions, _) = list_trace(&pool, ws, rec.trace.trace_id, 10).await.unwrap();
        // Records of one transaction share a time, so their order is not defined.
        let mut actions: Vec<&str> = decisions.iter().map(|d| d.action.as_str()).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["person.created", "trait.change"]);
        let text =
            serde_json::to_string(&decisions.iter().map(|d| &d.subject_ids).collect::<Vec<_>>())
                .unwrap();
        assert!(!text.contains("pro") && !text.contains("example.com") && !text.contains("plan"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_sources_writing_one_trait_resolve_by_the_policy(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        define(&pool, ws, "latest_t", Precedence::Latest, &[]).await;
        define(&pool, ws, "first_t", Precedence::First, &[]).await;
        define(
            &pool,
            ws,
            "prio_t",
            Precedence::SourcePriority,
            &["shop", "crm"],
        )
        .await;
        let traits = |tag: &str| json!({"latest_t": tag, "first_t": tag, "prio_t": tag});
        let old = record(
            "crm",
            "1",
            "a@example.com",
            traits("crm"),
            datetime!(2026-01-01 00:00 UTC),
        );
        let new = record(
            "shop",
            "9",
            "a@example.com",
            traits("shop"),
            datetime!(2026-02-01 00:00 UTC),
        );

        let outcomes = apply_profile_batch(&pool, ws, &[new, old]).await.unwrap();

        let person = person_of(&outcomes[0]);
        assert_eq!(person, person_of(&outcomes[1]));
        assert_eq!(
            trait_of(&pool, ws, person, "latest_t").await,
            Some(json!("shop"))
        );
        assert_eq!(
            trait_of(&pool, ws, person, "first_t").await,
            Some(json!("crm"))
        );
        assert_eq!(
            trait_of(&pool, ws, person, "prio_t").await,
            Some(json!("shop"))
        );
        let found = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(found.trait_sources["latest_t"], json!("shop"));
        assert_eq!(found.trait_sources["first_t"], json!("crm"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_a_contribution_restores_the_other_sources_value(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let crm = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": "free"}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let shop = record(
            "shop",
            "9",
            "a@example.com",
            json!({"plan": "pro"}),
            datetime!(2026-02-01 00:00 UTC),
        );
        let outcomes = apply_profile_batch(&pool, ws, &[crm, shop]).await.unwrap();
        let person = person_of(&outcomes[0]);
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("pro"))
        );

        let removed = remove_contribution(&pool, ws, "shop", "contacts", "9", &trace())
            .await
            .unwrap();

        assert!(removed);
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("free"))
        );
        assert!(
            !remove_contribution(&pool, ws, "shop", "contacts", "9", &trace())
                .await
                .unwrap()
        );
        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        assert_eq!(trait_of(&pool, ws, person, "plan").await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_null_removes_only_that_sources_contribution(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let crm = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": "free"}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let shop = record(
            "shop",
            "9",
            "a@example.com",
            json!({"plan": "pro"}),
            datetime!(2026-02-01 00:00 UTC),
        );
        let retract = record(
            "shop",
            "9",
            "a@example.com",
            json!({"plan": null}),
            datetime!(2026-03-01 00:00 UTC),
        );
        let outcomes = apply_profile_batch(&pool, ws, &[crm, shop]).await.unwrap();
        let person = person_of(&outcomes[0]);

        apply_profile_batch(&pool, ws, &[retract]).await.unwrap();

        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("free"))
        );
    }

    async fn snapshot(pool: &PgPool) -> Vec<String> {
        let mut rows: Vec<String> = Vec::new();
        for query in [
            "SELECT row_to_json(p)::text FROM people p ORDER BY id",
            "SELECT row_to_json(i)::text FROM identities i ORDER BY id",
            "SELECT row_to_json(s)::text FROM source_records s ORDER BY id",
            "SELECT row_to_json(c)::text FROM person_changes c ORDER BY person_id",
            "SELECT row_to_json(d)::text FROM decision_records d ORDER BY id",
        ] {
            rows.extend(
                sqlx::query_scalar::<_, String>(query)
                    .fetch_all(pool)
                    .await
                    .unwrap(),
            );
        }
        rows
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replay_of_the_same_record_is_a_no_op(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::from_bits(63),
        )
        .await;
        let first = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": "pro"}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let replay = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": "pro"}),
            datetime!(2026-06-01 00:00 UTC),
        );
        apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let before = snapshot(&pool).await;

        let outcomes = apply_profile_batch(&pool, ws, &[replay]).await.unwrap();

        assert!(matches!(outcomes[0], ProfileOutcome::Unchanged { .. }));
        assert_eq!(snapshot(&pool).await, before);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wrongly_typed_value_is_dropped_with_its_reason_and_the_rest_applies(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        define(&pool, ws, "plan", Precedence::Latest, &[]).await;
        let rec = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": 7, "city": "Aarhus", "Bad Key": 1, "meta": {"a": 1}}),
            datetime!(2026-01-01 00:00 UTC),
        );

        let outcomes = apply_profile_batch(&pool, ws, &[rec]).await.unwrap();

        let ProfileOutcome::Applied {
            person_id, dropped, ..
        } = &outcomes[0]
        else {
            panic!("not applied: {outcomes:?}");
        };
        let reasons: Vec<(&str, &str)> =
            dropped.iter().map(|d| (d.key.as_str(), d.reason)).collect();
        assert_eq!(
            reasons,
            [
                ("Bad Key", "invalid_key"),
                ("meta", "not_inferable"),
                ("plan", "wrong_type")
            ]
        );
        assert_eq!(
            trait_of(&pool, ws, *person_id, "city").await,
            Some(json!("Aarhus"))
        );
        assert_eq!(trait_of(&pool, ws, *person_id, "plan").await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unregistered_keys_are_inferred_and_archived_keys_are_dropped(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = record(
            "crm",
            "1",
            "a@example.com",
            json!({"score": 4.5, "tags": ["a"]}),
            datetime!(2026-01-01 00:00 UTC),
        );
        apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let definitions = crate::people::list_trait_definitions(&pool, ws)
            .await
            .unwrap();
        assert!(
            definitions
                .iter()
                .all(|d| d.inferred && d.precedence == Precedence::Latest && !d.pii)
        );
        assert_eq!(definitions.len(), 2);
        update_trait_definition(
            &pool,
            ws,
            "score",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let second = record(
            "crm",
            "2",
            "b@example.com",
            json!({"score": 1}),
            datetime!(2026-01-02 00:00 UTC),
        );

        let outcomes = apply_profile_batch(&pool, ws, &[second]).await.unwrap();

        let ProfileOutcome::Applied { dropped, .. } = &outcomes[0] else {
            panic!("not applied");
        };
        assert_eq!(
            dropped,
            &[DroppedTrait {
                key: "score".into(),
                reason: "archived_key"
            }]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rejected_record_does_not_abort_the_batch(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let bad = record(
            "crm",
            "1",
            "not-an-email",
            json!({"plan": "x"}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let mut none = record(
            "crm",
            "2",
            "a@example.com",
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        );
        none.identifiers.clear();
        let good = record(
            "crm",
            "3",
            "c@example.com",
            json!({"plan": "x"}),
            datetime!(2026-01-01 00:00 UTC),
        );

        let outcomes = apply_profile_batch(&pool, ws, &[bad, none, good])
            .await
            .unwrap();

        assert_eq!(
            outcomes[0],
            ProfileOutcome::Rejected {
                reason: "invalid_email"
            }
        );
        assert_eq!(
            outcomes[1],
            ProfileOutcome::Rejected {
                reason: "no_identifier"
            }
        );
        assert!(matches!(
            outcomes[2],
            ProfileOutcome::Applied { created: true, .. }
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_the_rules_forbid_attaches_to_the_strongest_identifier_and_records_a_conflict(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let mut first = record(
            "crm",
            "1",
            "a@example.com",
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        );
        first.identifiers.push(("external:crm".into(), "1".into()));
        let mut second = record(
            "crm",
            "2",
            "b@example.com",
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        );
        second.identifiers = vec![("external:crm".into(), "2".into())];
        let outcomes = apply_profile_batch(&pool, ws, &[first, second])
            .await
            .unwrap();
        let (email_person, external_person) = (person_of(&outcomes[0]), person_of(&outcomes[1]));
        assert_ne!(email_person, external_person);
        let mut both = record(
            "web",
            "7",
            "a@example.com",
            json!({}),
            datetime!(2026-01-02 00:00 UTC),
        );
        both.identifiers = vec![
            ("email".into(), "a@example.com".into()),
            ("external:crm".into(), "2".into()),
            ("anonymous_id".into(), "anon-1".into()),
        ];

        let outcomes = apply_profile_batch(&pool, ws, &[both.clone()])
            .await
            .unwrap();

        assert_eq!(person_of(&outcomes[0]), external_person);
        let conflicts = list_identity_conflicts(&pool, ws, external_person)
            .await
            .unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].other_person_id, email_person);
        assert_eq!(conflicts[0].reason, "unique_kind");
        assert_eq!(conflicts[0].kind, "email");
        assert!(conflicts[0].resolved_at.is_none());
        let shared: Uuid = sqlx::query_scalar(
            "SELECT id FROM identities WHERE kind = 'email' AND value = 'a@example.com'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(conflicts[0].identity_id, Some(shared));
        let identities = list_identities(&pool, ws, external_person).await.unwrap();
        let kinds: Vec<&str> = identities.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, ["anonymous_id", "external:crm"]);
        let merges: i64 = sqlx::query_scalar("SELECT count(*) FROM person_merges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(merges, 0);
        let (decisions, _) = list_trace(&pool, ws, both.trace.trace_id, 10)
            .await
            .unwrap();
        assert!(decisions.iter().any(|d| d.action == "identity.conflict"
            && d.outcome == "recorded"
            && d.reason.as_deref() == Some("unique_kind")));
    }

    fn with_identifiers(mut record: ProfileRecord, identifiers: &[(&str, &str)]) -> ProfileRecord {
        record.identifiers = identifiers
            .iter()
            .map(|(kind, value)| ((*kind).into(), (*value).into()))
            .collect();
        record
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_value_of_a_unique_kind_makes_the_record_a_different_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let first = with_identifiers(
            record("crm", "1", "a@example.com", json!({}), at),
            &[("email", "a@example.com"), ("external:crm", "1")],
        );
        let owner = person_of(&apply_profile_batch(&pool, ws, &[first]).await.unwrap()[0]);
        let second = with_identifiers(
            record("web", "2", "a@example.com", json!({"plan": "pro"}), at),
            &[("email", "a@example.com"), ("external:crm", "2")],
        );

        let outcomes = apply_profile_batch(&pool, ws, std::slice::from_ref(&second))
            .await
            .unwrap();

        let newcomer = person_of(&outcomes[0]);
        assert_ne!(newcomer, owner);
        assert!(matches!(
            outcomes[0],
            ProfileOutcome::Applied { created: true, .. }
        ));
        let kinds = |identities: Vec<crate::StoredIdentity>| {
            identities.into_iter().map(|i| i.kind).collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(list_identities(&pool, ws, owner).await.unwrap()),
            ["email", "external:crm"]
        );
        assert_eq!(
            kinds(list_identities(&pool, ws, newcomer).await.unwrap()),
            ["external:crm"]
        );
        assert_eq!(
            trait_of(&pool, ws, newcomer, "plan").await,
            Some(json!("pro"))
        );
        assert_eq!(trait_of(&pool, ws, owner, "plan").await, None);
        let conflicts = list_identity_conflicts(&pool, ws, newcomer).await.unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].other_person_id, owner);
        assert_eq!(conflicts[0].reason, "unique_kind");
        assert_eq!(conflicts[0].kind, "email");
        let email_identity: Uuid =
            sqlx::query_scalar("SELECT id FROM identities WHERE kind = 'email'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(conflicts[0].identity_id, Some(email_identity));
        let (decisions, _) = list_trace(&pool, ws, second.trace.trace_id, 10)
            .await
            .unwrap();
        assert!(decisions.iter().any(|d| d.action == "identity.conflict"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn identifiers_beyond_the_maximum_of_a_kind_are_skipped_and_the_rest_applies(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let emails: Vec<(String, String)> = (0..10)
            .map(|n| ("email".to_owned(), format!("e{n}@example.com")))
            .collect();
        let mut full = record("crm", "1", "e0@example.com", json!({}), at);
        full.identifiers = emails;
        let person = person_of(&apply_profile_batch(&pool, ws, &[full]).await.unwrap()[0]);
        let mut more = record("web", "2", "e0@example.com", json!({"plan": "pro"}), at);
        more.identifiers = vec![
            ("email".into(), "e0@example.com".into()),
            ("email".into(), "extra@example.com".into()),
            ("phone".into(), "+4512345678".into()),
        ];

        let outcomes = apply_profile_batch(&pool, ws, &[more.clone()])
            .await
            .unwrap();

        assert_eq!(person_of(&outcomes[0]), person);
        let identities = list_identities(&pool, ws, person).await.unwrap();
        assert_eq!(identities.len(), 11);
        assert!(identities.iter().any(|i| i.kind == "phone"));
        assert!(identities.iter().all(|i| i.value != "extra@example.com"));
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("pro"))
        );
        let (decisions, _) = list_trace(&pool, ws, more.trace.trace_id, 10)
            .await
            .unwrap();
        let skipped = decisions
            .iter()
            .find(|d| d.action == "identity.limit")
            .expect("an identity.limit decision");
        assert_eq!(skipped.outcome, "skipped");
        assert_eq!(skipped.reason.as_deref(), Some("max_per_person"));
        assert_eq!(skipped.subject_ids["person"], person.to_string());
        assert!(skipped.subject_ids["source_record"].is_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_allowed_new_identifier_still_attaches_to_the_one_matching_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let person = person_of(
            &apply_profile_batch(
                &pool,
                ws,
                &[record("crm", "1", "a@example.com", json!({}), at)],
            )
            .await
            .unwrap()[0],
        );
        let both = with_identifiers(
            record("web", "2", "a@example.com", json!({}), at),
            &[("email", "a@example.com"), ("email", "b@example.com")],
        );

        let outcomes = apply_profile_batch(&pool, ws, &[both]).await.unwrap();

        assert_eq!(person_of(&outcomes[0]), person);
        assert_eq!(list_identities(&pool, ws, person).await.unwrap().len(), 2);
        assert!(
            list_identity_conflicts(&pool, ws, person)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_already_over_a_rule_keeps_receiving_its_records(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let first = with_identifiers(
            record("crm", "1", "a@example.com", json!({"plan": "free"}), at),
            &[("external:crm", "1")],
        );
        let person = person_of(&apply_profile_batch(&pool, ws, &[first]).await.unwrap()[0]);
        sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
             VALUES ($1, $2, $3, 'external:crm', '2')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(person)
        .execute(&pool)
        .await
        .unwrap();
        let again = with_identifiers(
            record("crm", "1", "a@example.com", json!({"plan": "pro"}), at),
            &[("external:crm", "1"), ("external:crm", "2")],
        );

        let outcomes = apply_profile_batch(&pool, ws, &[again]).await.unwrap();

        assert_eq!(person_of(&outcomes[0]), person);
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("pro"))
        );
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        let conflicts: i64 = sqlx::query_scalar("SELECT count(*) FROM identity_conflicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((people, conflicts), (1, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_split_with_no_matching_owner_is_recorded(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let first = with_identifiers(
            record("crm", "1", "a@example.com", json!({}), at),
            &[("external:crm", "1")],
        );
        let previous = person_of(&apply_profile_batch(&pool, ws, &[first]).await.unwrap()[0]);
        let moved = with_identifiers(
            record("crm", "1", "a@example.com", json!({}), at),
            &[("external:crm", "2")],
        );

        let outcomes = apply_profile_batch(&pool, ws, std::slice::from_ref(&moved))
            .await
            .unwrap();

        let person = person_of(&outcomes[0]);
        assert_ne!(person, previous);
        let (decisions, _) = list_trace(&pool, ws, moved.trace.trace_id, 10)
            .await
            .unwrap();
        let split = decisions
            .iter()
            .find(|d| d.action == "identity.split")
            .expect("an identity.split decision");
        assert_eq!(split.outcome, "new_person");
        assert_eq!(split.reason.as_deref(), Some("unique_kind"));
        assert_eq!(split.subject_ids["person"], person.to_string());
        assert_eq!(split.subject_ids["previous_person"], previous.to_string());
        assert!(split.subject_ids["source_record"].is_string());
        assert!(
            list_identity_conflicts(&pool, ws, person)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_race_between_batches_still_yields_one_identity_row(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let one = record(
            "crm",
            "1",
            "race@example.com",
            json!({"a": "x"}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let two = record(
            "shop",
            "2",
            "race@example.com",
            json!({"b": "y"}),
            datetime!(2026-01-01 00:00 UTC),
        );

        let (batch_one, batch_two) = ([one], [two]);
        let (first, second) = tokio::join!(
            apply_profile_batch(&pool, ws, &batch_one),
            apply_profile_batch(&pool, ws, &batch_two),
        );

        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(person_of(&first[0]), person_of(&second[0]));
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((identities, people), (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_new_identifier_twice_in_one_batch_makes_one_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let one = record(
            "crm",
            "1",
            "a@example.com",
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        );
        let two = record(
            "crm",
            "2",
            "a@example.com",
            json!({}),
            datetime!(2026-01-01 00:00 UTC),
        );

        let outcomes = apply_profile_batch(&pool, ws, &[one, two]).await.unwrap();

        assert_eq!(person_of(&outcomes[0]), person_of(&outcomes[1]));
        assert!(matches!(
            outcomes[1],
            ProfileOutcome::Applied { created: false, .. }
        ));
    }

    const T0: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_that_changes_its_identifier_keeps_its_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = record("crm", "1", "old@example.com", json!({"plan": "pro"}), T0);
        let renamed = record("crm", "1", "new@example.com", json!({"plan": "pro"}), T0);
        let outcomes = apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let person = person_of(&outcomes[0]);

        let outcomes = apply_profile_batch(&pool, ws, &[renamed]).await.unwrap();

        assert_eq!(person_of(&outcomes[0]), person);
        assert!(matches!(
            outcomes[0],
            ProfileOutcome::Applied { created: false, .. }
        ));
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 1);
        assert_eq!(list_identities(&pool, ws, person).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_moving_between_people_takes_all_its_keys_with_it(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mine = record("crm", "1", "a@example.com", json!({"a": "1", "b": "2"}), T0);
        let other = record("shop", "9", "b@example.com", json!({"c": "3"}), T0);
        let outcomes = apply_profile_batch(&pool, ws, &[mine, other])
            .await
            .unwrap();
        let (old, new) = (person_of(&outcomes[0]), person_of(&outcomes[1]));
        assert_ne!(old, new);
        // The record now carries the other person's identifier, and touches only key `a`.
        let moved = record("crm", "1", "b@example.com", json!({"a": "5"}), T0);

        let outcomes = apply_profile_batch(&pool, ws, &[moved]).await.unwrap();

        assert_eq!(person_of(&outcomes[0]), new);
        let new = find_person(&pool, ws, new).await.unwrap().unwrap();
        assert_eq!(new.traits, json!({"a": "5", "b": "2", "c": "3"}));
        let old = find_person(&pool, ws, old).await.unwrap().unwrap();
        assert_eq!(old.traits, json!({}));
        assert_eq!(old.trait_sources, json!({}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_revived_tombstone_does_not_resurrect_deleted_traits(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = record("crm", "1", "a@example.com", json!({"a": "1", "b": "2"}), T0);
        let outcomes = apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let person = person_of(&outcomes[0]);
        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        assert_eq!(
            find_person(&pool, ws, person)
                .await
                .unwrap()
                .unwrap()
                .traits,
            json!({})
        );

        let revived = record("crm", "1", "a@example.com", json!({"b": "3"}), T0);
        apply_profile_batch(&pool, ws, &[revived]).await.unwrap();

        let found = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(found.traits, json!({"b": "3"}));
        let ledger: Value =
            sqlx::query_scalar("SELECT traits FROM source_records WHERE record_key = '1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ledger.get("a").is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn null_for_an_unregistered_key_registers_nothing_and_drops_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let rec = record("crm", "1", "a@example.com", json!({"ghost": null}), T0);

        let outcomes = apply_profile_batch(&pool, ws, &[rec]).await.unwrap();

        let ProfileOutcome::Applied { dropped, .. } = &outcomes[0] else {
            panic!("not applied");
        };
        assert!(dropped.is_empty());
        let definitions = crate::people::list_trait_definitions(&pool, ws)
            .await
            .unwrap();
        assert!(definitions.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn numbers_keep_their_exact_value_through_the_ledger_and_the_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut rec = record("crm", "1", "a@example.com", json!({}), T0);
        rec.traits = serde_json::from_str(r#"{"big": 9007199254740993, "small": 7}"#).unwrap();

        let outcomes = apply_profile_batch(&pool, ws, &[rec]).await.unwrap();

        let person = find_person(&pool, ws, person_of(&outcomes[0]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(person.traits["big"].to_string(), "9007199254740993");
        assert_eq!(person.traits["small"].to_string(), "7");
        let ledger: String = sqlx::query_scalar("SELECT traits::text FROM source_records LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(ledger.contains("9007199254740993"), "{ledger}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_key_is_not_materialized_even_when_a_record_is_rewritten(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = record(
            "crm",
            "1",
            "a@example.com",
            json!({"plan": "pro", "x": "1"}),
            T0,
        );
        let outcomes = apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let person = person_of(&outcomes[0]);
        update_trait_definition(
            &pool,
            ws,
            "plan",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let again = record("crm", "1", "a@example.com", json!({"x": "2"}), T0);
        apply_profile_batch(&pool, ws, &[again]).await.unwrap();

        let found = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(found.traits, json!({"x": "2"}));
    }

    #[test]
    fn numbers_are_equal_by_value_but_big_integers_compare_exactly() {
        assert!(same_value(&json!(5), &json!(5.0)));
        assert!(!same_value(&json!(5), &json!(5.5)));
        assert!(!same_value(
            &json!(9_007_199_254_740_993_i64),
            &json!(9_007_199_254_740_992_i64)
        ));
        assert!(same_value(&json!(u64::MAX), &json!(u64::MAX)));
    }

    #[test]
    fn a_mixed_integer_and_float_pair_is_equal_only_when_the_float_is_that_exact_integer() {
        let big = 9_007_199_254_740_993_i64;
        assert!(!same_value(&json!(big), &json!(9_007_199_254_740_992.0)));
        assert!(!same_value(&json!(9_007_199_254_740_992.0), &json!(big)));
        assert!(same_value(
            &json!(9_007_199_254_740_992_i64),
            &json!(9_007_199_254_740_992.0)
        ));
        assert!(same_value(&json!(-3.0), &json!(-3)));
        assert!(!same_value(&json!(3), &json!(3.5)));
        assert!(!same_value(&json!(u64::MAX), &json!(1.8446744073709552e19)));
        assert!(!same_value(&json!(i64::MIN), &json!(1.0e300)));
        assert!(same_value(&json!(2.5), &json!(2.5)));
        assert!(!same_value(&json!(5), &json!("5")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_number_in_another_form_from_another_source_is_not_a_change(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = record("crm", "1", "a@example.com", json!({"n": 5}), T0);
        let outcomes = apply_profile_batch(&pool, ws, &[first]).await.unwrap();
        let person = person_of(&outcomes[0]);
        let later = datetime!(2026-02-01 00:00 UTC);
        let second = record("shop", "2", "a@example.com", json!({"n": 5.0}), later);

        let outcomes = apply_profile_batch(&pool, ws, &[second]).await.unwrap();

        assert!(matches!(
            outcomes[0],
            ProfileOutcome::Applied { changed: false, .. }
        ));
        let found = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(found.traits["n"].to_string(), "5");
        assert_eq!(found.trait_sources["n"], "shop");
        let changes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'trait.change'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(changes, 1);
    }

    async fn identity_kinds(pool: &PgPool, ws: WorkspaceId, person: Uuid) -> Vec<String> {
        list_identities(pool, ws, person)
            .await
            .unwrap()
            .into_iter()
            .map(|identity| identity.kind)
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_a_contribution_deletes_only_the_identities_no_other_record_lists(
        pool: PgPool,
    ) {
        use crate::privacy::testing::profile;

        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::from_bits(63),
        )
        .await;
        let crm = profile(
            "crm",
            "1",
            &[("email", "a@example.com"), ("external:crm", "1")],
            json!({"plan": "free"}),
        );
        let shop = profile(
            "shop",
            "9",
            &[("email", "a@example.com"), ("anonymous_id", "anon-1")],
            json!({"plan": "pro"}),
        );
        let outcomes = apply_profile_batch(&pool, ws, &[crm, shop]).await.unwrap();
        let person = person_of(&outcomes[0]);
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        remove_contribution(&pool, ws, "shop", "contacts", "9", &trace())
            .await
            .unwrap();

        // The anonymous id only the shop listed is gone; the shared email stays, and so does
        // the external id.
        assert_eq!(
            identity_kinds(&pool, ws, person).await,
            ["email", "external:crm"]
        );
        assert_eq!(
            trait_of(&pool, ws, person, "plan").await,
            Some(json!("free"))
        );
        let dirty: i64 = sqlx::query_scalar("SELECT count(*) FROM person_changes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(dirty, 1);
        let removed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed' \
               AND subject_ids ? 'person' AND subject_ids ? 'source_record'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(removed, 1);

        // Removing the last record frees the rest, and the person stays under `keep`.
        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        assert!(identity_kinds(&pool, ws, person).await.is_empty());
        assert!(find_person(&pool, ws, person).await.unwrap().is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn removing_a_contribution_deletes_the_conflicts_about_its_identities(pool: PgPool) {
        use crate::privacy::testing::profile;

        let ws = workspace(&pool, "one").await;
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                profile("crm", "1", &[("email", "a@example.com")], json!({})),
                profile("shop", "2", &[("email", "b@example.com")], json!({})),
            ],
        )
        .await
        .unwrap();
        let (one, two) = (person_of(&outcomes[0]), person_of(&outcomes[1]));
        let identity: Uuid =
            sqlx::query_scalar("SELECT id FROM identities WHERE value = 'b@example.com'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, reason, identity_id) \
             VALUES ($1, $2, $3, $4, 'email', 'unique_kind', $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(one)
        .bind(two)
        .bind(identity)
        .execute(&pool)
        .await
        .unwrap();

        remove_contribution(&pool, ws, "shop", "contacts", "2", &trace())
            .await
            .unwrap();

        let conflicts: i64 = sqlx::query_scalar("SELECT count(*) FROM identity_conflicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(conflicts, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_delete_policy_deletes_an_orphan_and_keep_does_not(pool: PgPool) {
        use crate::privacy::testing::profile;

        let ws = workspace(&pool, "one").await;
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                profile("crm", "1", &[("email", "a@example.com")], json!({"n": 1})),
                profile("crm", "2", &[("email", "b@example.com")], json!({"n": 2})),
            ],
        )
        .await
        .unwrap();
        let (kept, orphan) = (person_of(&outcomes[0]), person_of(&outcomes[1]));

        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        assert!(find_person(&pool, ws, kept).await.unwrap().is_some());
        crate::set_orphan_policy(&pool, ws, OrphanPolicy::Delete)
            .await
            .unwrap();
        remove_contribution(&pool, ws, "crm", "contacts", "2", &trace())
            .await
            .unwrap();

        assert!(find_person(&pool, ws, orphan).await.unwrap().is_none());
        assert!(find_person(&pool, ws, kept).await.unwrap().is_some());
        assert_eq!(
            crate::people::erasure::rows_referencing_person(&pool, orphan).await,
            []
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_delete_policy_spares_a_person_with_another_record_an_event_or_explicit_consent(
        pool: PgPool,
    ) {
        use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};

        use crate::privacy::testing::profile;
        use crate::{ConsentChange, EventRecord, apply_event_batch, set_consent};

        let ws = workspace(&pool, "one").await;
        crate::set_orphan_policy(&pool, ws, OrphanPolicy::Delete)
            .await
            .unwrap();
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                profile("crm", "1", &[("email", "two@example.com")], json!({})),
                profile("shop", "2", &[("email", "two@example.com")], json!({})),
                profile("crm", "3", &[("email", "event@example.com")], json!({})),
                profile("crm", "4", &[("email", "consent@example.com")], json!({})),
            ],
        )
        .await
        .unwrap();
        let [two, with_event, with_consent] = [0, 2, 3].map(|index| person_of(&outcomes[index]));
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        apply_event_batch(
            &pool,
            ws,
            &key,
            &[EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), "event@example.com".into())],
                name: "seen".into(),
                properties: serde_json::Map::new(),
                timestamp: None,
                received_at: OffsetDateTime::now_utc(),
                message_id: None,
                trace: trace(),
            }],
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person: with_consent,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Subscribed,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: T0,
                actor: None,
            },
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();

        for (source, key) in [("crm", "1"), ("crm", "3"), ("crm", "4")] {
            remove_contribution(&pool, ws, source, "contacts", key, &trace())
                .await
                .unwrap();
        }

        for person in [two, with_event, with_consent] {
            assert!(find_person(&pool, ws, person).await.unwrap().is_some());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_removal_locks_the_record_before_the_person(pool: PgPool) {
        use crate::privacy::testing::profile;

        let ws = workspace(&pool, "one").await;
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[profile(
                "crm",
                "1",
                &[("email", "a@example.com")],
                json!({"n": 1}),
            )],
        )
        .await
        .unwrap();
        let person = person_of(&outcomes[0]);
        // The drain's existing-record path holds the record first.
        let mut drain = begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM source_records FOR UPDATE")
            .execute(&mut *drain)
            .await
            .unwrap();
        let removal = tokio::spawn({
            let pool = pool.clone();
            async move { remove_contribution(&pool, ws, "crm", "contacts", "1", &trace()).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // The removal waits on the record; it holds no lock on the person yet, so the drain
        // can take the person without a deadlock.
        let mut probe = begin(&pool).await.unwrap();
        let locked = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(person)
            .execute(&mut *probe)
            .await;
        drop(probe);
        commit(drain).await.unwrap();

        assert!(
            locked.is_ok(),
            "the removal locked the person before the record"
        );
        assert!(removal.await.unwrap().unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_trait_change_an_added_identifier_and_a_removed_record_mark_their_classes(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::from_bits(63),
        )
        .await;
        define(&pool, ws, "plan", Precedence::Latest, &[]).await;
        let at = datetime!(2026-01-01 00:00 UTC);
        let first = record("crm", "1", "a@example.com", json!({"plan": "free"}), at);
        let ada = person_of(&apply_profile_batch(&pool, ws, &[first]).await.unwrap()[0]);
        let marks = || async {
            let marks: Vec<(Uuid, i16, Option<String>)> =
                sqlx::query_as("SELECT person_id, classes, cause_id FROM person_changes")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            sqlx::query("DELETE FROM person_changes")
                .execute(&pool)
                .await
                .unwrap();
            marks
        };
        // A new person is marked for her traits and her identifier.
        assert_eq!(marks().await, [(ada, 1 | 16, Some("test".into()))]);

        // A changed trait is the traits class alone.
        let changed = record("crm", "1", "a@example.com", json!({"plan": "pro"}), at);
        apply_profile_batch(&pool, ws, &[changed]).await.unwrap();
        assert_eq!(marks().await, [(ada, 1, Some("test".into()))]);

        // A second record that only adds an identifier changes no trait.
        let mut extra = record("crm", "1", "a@example.com", json!({"plan": "pro"}), at);
        extra
            .identifiers
            .push(("email".into(), "a2@example.com".into()));
        apply_profile_batch(&pool, ws, &[extra]).await.unwrap();
        assert_eq!(marks().await, [(ada, 16, Some("test".into()))]);

        // Removing the record marks its traits and events, and the identifiers it took with it.
        remove_contribution(&pool, ws, "crm", "contacts", "1", &trace())
            .await
            .unwrap();
        let removed = marks().await;
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].1 & 3, 3);
    }
}
