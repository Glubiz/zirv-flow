//! Writing contacts (architecture section 7.6). A single-contact write validates against the
//! definition and applies through the profile path of apply-batch inside the caller's
//! transaction, so the request answers once the data is stored.
//!
//! A write takes two advisory locks, in this order: the idempotency key, then the contact. So
//! two requests that share either one run one after the other, and a repeat sees the first
//! one's result.
//!
//! The ingestion drain takes the same contact lock for every contact key of the rows it claimed,
//! so a bulk upsert and a single write of one contact also run one after the other and each
//! reads the fields the other stored. The lock orders that keep this free of cycles:
//!
//! - single write: idempotency lock, contact lock, source record, person;
//! - drain: inbox rows (`SKIP LOCKED`), contact locks (all of them, sorted by key, before any
//!   savepoint), source record, people in id order. The drain never waits for a contact lock
//!   while it holds a record or a person, and never takes one after the first row is applied;
//! - removing a contribution: source record, person.
//!
//! Nothing here logs or labels anything with a field or an identifier value.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use zirv_flow_core::contacts::{
    FieldProblem, KEY_KIND, RegistryKey, SOURCE, STREAM, TypedField, plan, validate_contact_key,
};
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::definition::{contact_registry, find_contact_definition, typed_definition};
use super::records::{Contact, find_contact};
use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_record, remove_contribution_in};
use crate::{PgPool, StoreError};

/// How long an idempotency key is remembered.
const KEY_HOURS: i32 = 24;

/// A write of one contact.
#[derive(Clone, Debug)]
pub enum ContactChange {
    /// A new contact with exactly these fields; refused when the key exists.
    Create(Map<String, Value>),
    /// All the fields of an existing contact are replaced.
    Replace(Map<String, Value>),
    /// A JSON merge patch over the fields of an existing contact: `null` removes a field.
    Patch(Map<String, Value>),
}

/// An `Idempotency-Key` and the hash of the request it was first used for.
#[derive(Clone, Copy, Debug)]
pub struct Idempotency<'a> {
    pub key: &'a str,
    /// The hash of the method, the path and the body. Nothing of the body is stored.
    pub request_hash: &'a [u8],
}

/// Why a contact write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// The fields break the definition or the registry; names fields and reasons only.
    Invalid(Vec<FieldProblem>),
    /// A create over an existing key.
    Exists,
    /// No such contact.
    NotFound,
    /// The idempotency key was used for a different request.
    KeyReused,
    Store(StoreError),
}

impl From<StoreError> for WriteError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// What a write answered.
#[derive(Clone, Debug, PartialEq)]
pub struct Written {
    /// 201 for a create, 200 for a replace or patch, 204 for a delete: the first answer, also
    /// on a replay.
    pub status: u16,
    /// The contact as stored now. None for a delete, and for a replay of a write whose
    /// contact has since been deleted or erased.
    pub contact: Option<Contact>,
    /// The idempotency key had been used before: nothing was written.
    pub replayed: bool,
}

fn invalid(field: &str, reason: &'static str) -> WriteError {
    WriteError::Invalid(vec![FieldProblem {
        field: field.to_owned(),
        reason,
    }])
}

async fn advisory_lock(connection: &mut PgConnection, name: &str) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(name)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Takes the contact lock of every key, until the transaction ends. The keys are locked in
/// sorted order, each once, so two callers that lock overlapping sets cannot wait on each
/// other.
pub(crate) async fn lock_contacts<'a>(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    keys: impl IntoIterator<Item = &'a str>,
) -> Result<(), StoreError> {
    let keys: BTreeSet<&str> = keys.into_iter().collect();
    for key in keys {
        advisory_lock(
            connection,
            &format!("contact:{}:{key}", workspace.as_uuid()),
        )
        .await?;
    }
    Ok(())
}

/// Locks the idempotency key (when there is one) and the contact, and answers a repeat of a
/// remembered request. Expired keys are forgotten first.
async fn begin_write(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    idempotency: Option<&Idempotency<'_>>,
) -> Result<Option<Written>, WriteError> {
    if let Some(idempotency) = idempotency {
        advisory_lock(
            connection,
            &format!("contact-idem:{}:{}", workspace.as_uuid(), idempotency.key),
        )
        .await?;
    }
    lock_contacts(connection, workspace, [key]).await?;
    let Some(idempotency) = idempotency else {
        return Ok(None);
    };
    sqlx::query(
        "DELETE FROM contact_idempotency WHERE workspace_id = $1 AND key = $2 \
           AND created_at <= now() - make_interval(hours => $3)",
    )
    .bind(workspace.as_uuid())
    .bind(idempotency.key)
    .bind(KEY_HOURS)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let remembered: Option<(Vec<u8>, i16)> = sqlx::query_as(
        "SELECT request_hash, status FROM contact_idempotency \
         WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace.as_uuid())
    .bind(idempotency.key)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some((hash, status)) = remembered else {
        return Ok(None);
    };
    if hash != idempotency.request_hash {
        return Err(WriteError::KeyReused);
    }
    let status = u16::try_from(status).unwrap_or(200);
    let contact = if status == 204 {
        None
    } else {
        find_contact(connection, workspace, key).await?
    };
    Ok(Some(Written {
        status,
        contact,
        replayed: true,
    }))
}

async fn remember(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    idempotency: Option<&Idempotency<'_>>,
    status: u16,
) -> Result<(), StoreError> {
    let Some(idempotency) = idempotency else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO contact_idempotency (workspace_id, key, request_hash, status) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace.as_uuid())
    .bind(idempotency.key)
    .bind(idempotency.request_hash)
    .bind(i16::try_from(status).unwrap_or(200))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Sets every key of `existing` that `traits` leaves out to `null`, so applying the record
/// replaces the contact's fields instead of adding to them.
fn retract_absent(traits: &mut Map<String, Value>, existing: &Map<String, Value>) {
    for key in existing.keys() {
        traits.entry(key.clone()).or_insert(Value::Null);
    }
}

/// The stored fields of a contact that a write may change: those of archived registry keys
/// keep their values, as every archived trait does, and are neither merged nor retracted.
fn live_fields(fields: &Map<String, Value>, registry: &[RegistryKey]) -> Map<String, Value> {
    fields
        .iter()
        .filter(|(key, _)| {
            !registry
                .iter()
                .any(|entry| entry.archived && &entry.key == *key)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// The fields the definition lists; all of them while the workspace has no definition.
fn listed_fields(
    fields: Map<String, Value>,
    definition: Option<&[TypedField]>,
) -> Map<String, Value> {
    let Some(definition) = definition else {
        return fields;
    };
    fields
        .into_iter()
        .filter(|(key, _)| definition.iter().any(|typed| &typed.field.key == key))
        .collect()
}

/// The identities that stored values of archived identifier fields give the contact. A write
/// never validates those fields, so it lists these identities itself, and replacing the
/// contact's identifiers does not release them while the trait stays.
async fn archived_identifiers(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    stored: &Map<String, Value>,
    registry: &[RegistryKey],
) -> Result<Vec<(String, String)>, StoreError> {
    let Some(definition) = find_contact_definition(&mut *connection, workspace).await? else {
        return Ok(Vec::new());
    };
    Ok(definition
        .fields
        .into_iter()
        .filter(|field| {
            registry
                .iter()
                .any(|entry| entry.archived && entry.key == field.key)
        })
        .filter_map(|field| {
            let value = stored.get(&field.key)?.as_str()?;
            Some((field.identifier?, value.to_owned()))
        })
        .collect())
}

/// A patch keeps stored identifiers whose kind no writable definition field produces. Read
/// the native record: the current definition no longer maps the fields it dropped.
async fn unlisted_identifiers(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    definition: Option<&[TypedField]>,
) -> Result<Vec<(String, String)>, StoreError> {
    let replaced: Vec<&str> = std::iter::once(KEY_KIND)
        .chain(
            definition
                .into_iter()
                .flatten()
                .filter_map(|typed| typed.field.identifier.as_deref()),
        )
        .collect();
    sqlx::query_as(
        "SELECT identifier ->> 'kind', identifier ->> 'value' \
         FROM source_records, jsonb_array_elements(identifiers) identifier \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = $4 \
           AND deleted_at IS NULL AND NOT (identifier ->> 'kind' = ANY($5))",
    )
    .bind(workspace.as_uuid())
    .bind(SOURCE)
    .bind(STREAM)
    .bind(key)
    .bind(&replaced)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// Makes an inbox contact record replace the contact's fields: the keys the stored contact has
/// and the record leaves out are retracted, except those of archived registry keys.
pub(crate) async fn replace_stored_fields(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &mut ProfileRecord,
) -> Result<(), StoreError> {
    if let Some(contact) = find_contact(connection, workspace, &record.record_key).await? {
        let registry = contact_registry(connection, workspace).await?;
        retract_absent(&mut record.traits, &live_fields(&contact.fields, &registry));
        record
            .identifiers
            .extend(archived_identifiers(connection, workspace, &contact.fields, &registry).await?);
    }
    Ok(())
}

fn merge_patch(mut fields: Map<String, Value>, patch: Map<String, Value>) -> Map<String, Value> {
    for (key, value) in patch {
        if value.is_null() {
            fields.remove(&key);
        } else {
            fields.insert(key, value);
        }
    }
    fields
}

/// Creates, replaces or patches contact `key` in the caller's transaction, which the caller
/// commits when this returns `Ok`. On an error nothing of the write stays.
///
/// A repeat of a request whose `idempotency` key is remembered writes nothing and answers
/// the first answer's status with the contact as stored now.
pub async fn write_contact(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    change: ContactChange,
    idempotency: Option<&Idempotency<'_>>,
    trace: &TraceContext,
) -> Result<Written, WriteError> {
    validate_contact_key(key).map_err(|_| invalid("key", "invalid_key"))?;
    let mut savepoint = connection.begin().await.map_err(StoreError::query)?;
    if let Some(replay) = begin_write(&mut savepoint, workspace, key, idempotency).await? {
        return Ok(replay);
    }
    let existing = find_contact(&mut savepoint, workspace, key).await?;
    let registry = contact_registry(&mut savepoint, workspace).await?;
    let stored = existing
        .as_ref()
        .map(|contact| live_fields(&contact.fields, &registry));
    let definition = typed_definition(&mut savepoint, workspace).await?;
    let is_patch = matches!(&change, ContactChange::Patch(_));
    // The stored fields the write retracts: a replacement retracts all of them, a patch only
    // those the definition still lists, so it leaves the others as they are.
    let (status, fields, retracted) = match change {
        ContactChange::Create(fields) => {
            if existing.is_some() {
                return Err(WriteError::Exists);
            }
            (201, fields, None)
        }
        ContactChange::Replace(fields) => {
            if existing.is_none() {
                return Err(WriteError::NotFound);
            }
            (200, fields, stored)
        }
        ContactChange::Patch(patch) => {
            let current = stored.ok_or(WriteError::NotFound)?;
            let current = listed_fields(current, definition.as_deref());
            (200, merge_patch(current.clone(), patch), Some(current))
        }
    };
    let planned =
        plan(definition.as_deref(), &registry, key, &fields).map_err(WriteError::Invalid)?;
    let mut traits = planned.traits;
    let mut identifiers = planned.identifiers;
    if let Some(retracted) = &retracted {
        retract_absent(&mut traits, retracted);
    }
    if let Some(existing) = &existing {
        let preserved = if is_patch {
            unlisted_identifiers(&mut savepoint, workspace, key, definition.as_deref()).await?
        } else {
            archived_identifiers(&mut savepoint, workspace, &existing.fields, &registry).await?
        };
        identifiers.extend(preserved);
    }
    let record = ProfileRecord {
        source: SOURCE.to_owned(),
        stream: STREAM.to_owned(),
        record_key: key.to_owned(),
        identifiers,
        traits,
        observed_at: OffsetDateTime::now_utc(),
        trace: trace.clone(),
    };
    match apply_profile_record(&mut savepoint, workspace, &record).await? {
        ProfileOutcome::Rejected { reason } => return Err(invalid("", reason)),
        ProfileOutcome::Applied { dropped, .. } | ProfileOutcome::Unchanged { dropped, .. } => {
            if !dropped.is_empty() {
                return Err(WriteError::Invalid(
                    dropped
                        .into_iter()
                        .map(|dropped| FieldProblem {
                            field: dropped.key,
                            reason: dropped.reason,
                        })
                        .collect(),
                ));
            }
        }
    }
    remember(&mut savepoint, workspace, idempotency, status).await?;
    let contact = find_contact(&mut savepoint, workspace, key).await?;
    crate::changes::request_reevaluation(&mut savepoint, workspace).await?;
    savepoint.commit().await.map_err(StoreError::query)?;
    Ok(Written {
        status,
        contact,
        replayed: false,
    })
}

/// Deletes contact `key` in the caller's transaction, which the caller commits when this
/// returns `Ok`: its native record is retracted, and the workspace's orphan policy decides
/// whether the person goes too. Another source's data on the person stays.
pub async fn delete_contact(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    idempotency: Option<&Idempotency<'_>>,
    trace: &TraceContext,
) -> Result<Written, WriteError> {
    validate_contact_key(key).map_err(|_| invalid("key", "invalid_key"))?;
    let mut savepoint = connection.begin().await.map_err(StoreError::query)?;
    if let Some(replay) = begin_write(&mut savepoint, workspace, key, idempotency).await? {
        return Ok(replay);
    }
    if remove_contribution_in(&mut savepoint, workspace, SOURCE, STREAM, key, trace)
        .await?
        .is_none()
    {
        return Err(WriteError::NotFound);
    }
    remember(&mut savepoint, workspace, idempotency, 204).await?;
    crate::changes::request_reevaluation(&mut savepoint, workspace).await?;
    savepoint.commit().await.map_err(StoreError::query)?;
    Ok(Written {
        status: 204,
        contact: None,
        replayed: false,
    })
}

/// Forgets idempotency keys older than 24 hours. Returns how many.
pub async fn expire_contact_idempotency(pool: &PgPool) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM contact_idempotency WHERE created_at <= now() - make_interval(hours => $1)",
    )
    .bind(KEY_HOURS)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

/// Deletes the workspace's idempotency keys that equal any of `needles` as a whole, ignoring
/// case (the key is chosen by the client, so it can be an address or an id), or that contain one
/// that is an email ([`crate::by_value`]). A key that merely contains a short id stays. Bounded
/// by the primary key's workspace prefix, and the keys' own 24 hour expiry keeps that range
/// short. Returns how many keys were deleted.
pub async fn erase_contact_idempotency(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    needles: &[String],
) -> Result<u64, StoreError> {
    if needles.is_empty() {
        return Ok(0);
    }
    let done = sqlx::query(
        r"DELETE FROM contact_idempotency k WHERE k.workspace_id = $1 AND EXISTS (
            SELECT 1 FROM unnest($2::text[]) AS needle
            WHERE needle <> '' AND (lower(k.key) = lower(needle)
                OR (position('@' IN needle) > 0
                    AND position(lower(needle) IN lower(k.key)) > 0)))",
    )
    .bind(workspace.as_uuid())
    .bind(needles)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn a_merge_patch_sets_and_removes_fields() {
        let merged = merge_patch(
            map(json!({"a": 1, "b": 2})),
            map(json!({"b": null, "c": 3})),
        );

        assert_eq!(merged, map(json!({"a": 1, "c": 3})));
    }

    #[test]
    fn a_replacing_record_retracts_the_fields_it_leaves_out() {
        let mut traits = map(json!({"a": 2}));

        retract_absent(&mut traits, &map(json!({"a": 1, "b": 1})));

        assert_eq!(traits, map(json!({"a": 2, "b": null})));
    }
}
