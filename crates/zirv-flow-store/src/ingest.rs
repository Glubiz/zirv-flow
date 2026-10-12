//! The durable ingestion inbox (architecture sections 4.4 and 7.1). The API appends raw
//! items and answers once the append has committed; `ingest.drain` applies them through the
//! profile and event paths of apply-batch and deletes them in the same transaction.
//!
//! Nothing here logs or labels anything with an identifier, a trait or property value or an
//! event name.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::contacts;
use zirv_flow_core::people::Identifier;
use zirv_flow_core::{Cause, HmacKey, TraceContext, TraceId, WorkspaceId};

use crate::contacts::{lock_contacts, replace_stored_fields};
use crate::events::{
    EventBatch, EventOutcome, EventRecord, FailedItem, apply_event_record, reject_apply_failed,
};
use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_record};
use crate::{NewJob, PgConnection, PgPool, StoreError, begin, commit, enqueue};

/// The job kind that drains a workspace's inbox.
pub const DRAIN_JOB_KIND: &str = "ingest.drain";

/// The `source` every ingested record and event carries.
const SOURCE: &str = "api";

/// What an inbox item asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum IngestPayload {
    Identify {
        traits: Map<String, Value>,
    },
    /// A native contact: the traits are all of the contact's fields, and `identifiers` start
    /// with the contact key.
    Contact {
        traits: Map<String, Value>,
    },
    Track {
        event: String,
        properties: Map<String, Value>,
    },
}

impl IngestPayload {
    /// The `kind` column value and metric label.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Identify { .. } => "identify",
            Self::Track { .. } => "track",
            Self::Contact { .. } => "contact",
        }
    }
}

/// One item to append, after the API's structural validation.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestItem {
    /// `(kind, value)` pairs, as the API mapped them.
    pub identifiers: Vec<(String, String)>,
    pub timestamp: Option<OffsetDateTime>,
    /// The client's retry key; a server-assigned UUID v7 when absent.
    pub message_id: Option<String>,
    pub payload: IngestPayload,
}

/// The number of buckets a record key hashes into; `affinity` is in `0..BUCKETS`.
const BUCKETS: u8 = 64;

/// Which of a workspace's buckets a drain takes: shard `index` of `count` owns the buckets
/// `b` with `b % count == index`. Rows of one record key share a bucket, so shards drain
/// concurrently and each key still applies in id order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shard {
    pub index: u32,
    pub count: u32,
}

impl Shard {
    /// Every bucket: one shard of one.
    pub const ALL: Self = Self { index: 0, count: 1 };

    /// The buckets this shard owns.
    fn buckets(self) -> Vec<i16> {
        let count = self.count.max(1);
        (0..i16::from(BUCKETS))
            .filter(|bucket| u32::try_from(*bucket).is_ok_and(|b| b % count == self.index))
            .collect()
    }
}

/// The bucket of a record key: the first byte of its SHA-256, modulo [`BUCKETS`]. Stored
/// buckets must match those of later rows of the same key, so the algorithm never changes.
fn bucket(record_key: &str) -> i16 {
    let digest = Sha256::digest(record_key.as_bytes());
    i16::from(digest[0] % BUCKETS)
}

/// The bucket of the record key of a row of `kind` with `identifiers`: the contact key for a
/// contact, else the strongest identifier; bucket 0 when there is none, since such a row is
/// poison anyway. The append and the backfill both go through here, so they cannot diverge.
fn record_bucket(kind: &str, identifiers: &[(String, String)]) -> i16 {
    let key = if kind == "contact" {
        identifiers
            .iter()
            .find(|(kind, _)| kind == contacts::KEY_KIND)
            .map(|(_, value)| value.clone())
    } else {
        strongest_identifier(identifiers)
    };
    key.map_or(0, |key| bucket(&key))
}

/// The bucket of `item`'s record key.
fn item_bucket(item: &IngestItem) -> i16 {
    record_bucket(item.payload.kind(), &item.identifiers)
}

/// Buckets up to `limit` of the workspace's rows that have none, the oldest first, in a
/// transaction of its own, and returns how many it did. Rows a pre-bucketing or pre-0.29
/// writer appended carry no bucket; this puts them in their key's queue by id. The lock is
/// blocking, not `SKIP LOCKED`: it waits out an erasure and holds nothing else, so it cannot
/// deadlock. A count below `limit` means none are left.
pub async fn backfill_affinity(
    pool: &PgPool,
    workspace: WorkspaceId,
    limit: i64,
) -> Result<usize, StoreError> {
    let mut tx = begin(pool).await?;
    let rows: Vec<(i64, String, Value)> = sqlx::query_as(
        "SELECT id, kind, item FROM ingest_inbox \
         WHERE workspace_id = $1 AND affinity IS NULL \
         ORDER BY id LIMIT $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let ids: Vec<i64> = rows.iter().map(|(id, ..)| *id).collect();
    let buckets: Vec<i16> = rows
        .iter()
        .map(|(_, kind, item)| {
            StoredItem::parse(item).map_or(0, |stored| record_bucket(kind, &stored.identifiers))
        })
        .collect();
    sqlx::query(
        "UPDATE ingest_inbox SET affinity = b.affinity \
         FROM unnest($2::bigint[], $3::smallint[]) AS b(id, affinity) \
         WHERE ingest_inbox.workspace_id = $1 AND ingest_inbox.id = b.id",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .bind(&buckets)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    commit(tx).await?;
    Ok(rows.len())
}

/// An item as kept in the `item` column, read back.
struct StoredItem {
    identifiers: Vec<(String, String)>,
    timestamp: Option<String>,
    traits: Map<String, Value>,
    event: Option<String>,
    properties: Map<String, Value>,
}

/// The `item` column value of `item`.
fn stored_json(item: &IngestItem) -> Value {
    let timestamp = item
        .timestamp
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok());
    let mut stored = json!({ "identifiers": item.identifiers, "timestamp": timestamp });
    match &item.payload {
        IngestPayload::Identify { traits } | IngestPayload::Contact { traits } => {
            stored["traits"] = Value::Object(traits.clone());
        }
        IngestPayload::Track { event, properties } => {
            stored["event"] = Value::String(event.clone());
            stored["properties"] = Value::Object(properties.clone());
        }
    }
    stored
}

impl StoredItem {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let identifiers = object
            .get("identifiers")?
            .as_array()?
            .iter()
            .map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_owned(),
                    pair.get(1)?.as_str()?.to_owned(),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let map = |key: &str| object.get(key).and_then(Value::as_object).cloned();
        Some(Self {
            identifiers,
            timestamp: object
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_owned),
            traits: map("traits").unwrap_or_default(),
            event: object
                .get("event")
                .and_then(Value::as_str)
                .map(str::to_owned),
            properties: map("properties").unwrap_or_default(),
        })
    }
}

/// Appends `items` as one multi-row insert in one transaction and enqueues the workspace's
/// drain in it, so the wake-up commits with the rows. Returns once the rows are durable.
/// The rows are stamped `received_at` with the database's clock, the one the scheduler's safety
/// net and the inbox-age gauge compare it with.
pub async fn append_inbox(
    pool: &PgPool,
    workspace: WorkspaceId,
    api_key_id: Uuid,
    trace: &TraceContext,
    items: &[IngestItem],
) -> Result<usize, StoreError> {
    let kinds: Vec<&str> = items.iter().map(|item| item.payload.kind()).collect();
    let stored: Vec<Value> = items.iter().map(stored_json).collect();
    let message_ids: Vec<String> = items
        .iter()
        .map(|item| {
            item.message_id
                .clone()
                .unwrap_or_else(|| Uuid::now_v7().to_string())
        })
        .collect();

    let affinities: Vec<i16> = items.iter().map(item_bucket).collect();

    let mut tx = begin(pool).await?;
    let inserted = sqlx::query(
        "INSERT INTO ingest_inbox \
            (workspace_id, api_key_id, kind, item, message_id, received_at, trace_id, affinity) \
         SELECT $1, $2, i.kind, i.item, i.message_id, now(), $3, i.affinity \
         FROM unnest($4::text[], $5::jsonb[], $6::text[], $7::smallint[]) WITH ORDINALITY \
            AS i(kind, item, message_id, affinity, n) \
         ORDER BY i.n",
    )
    .bind(workspace.as_uuid())
    .bind(api_key_id)
    .bind(trace.trace_id.as_uuid())
    .bind(&kinds)
    .bind(&stored)
    .bind(&message_ids)
    .bind(&affinities)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    enqueue_drain(&mut *tx, workspace, trace).await?;
    commit(tx).await?;
    Ok(usize::try_from(inserted.rows_affected()).unwrap_or(usize::MAX))
}

/// Enqueues the workspace's drain. A no-op while one is ready or running.
pub async fn enqueue_drain<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace: WorkspaceId,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(DRAIN_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace.as_uuid() });
    job.unique_key = Some(format!("{DRAIN_JOB_KIND}:{}", workspace.as_uuid()));
    enqueue(executor, &job).await
}

/// A workspace's oldest inbox row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InboxHead {
    pub workspace_id: WorkspaceId,
    /// How long the row has waited, by the database's clock.
    pub age_seconds: f64,
}

/// The oldest row (the lowest id) of every workspace that has any: one index probe per
/// workspace on `(workspace_id, id)`, never a scan of the inbox.
pub async fn inbox_heads(pool: &PgPool) -> Result<Vec<InboxHead>, StoreError> {
    let rows: Vec<(Uuid, f64)> = sqlx::query_as(
        "SELECT w.id, EXTRACT(EPOCH FROM now() - head.received_at)::float8 \
         FROM workspaces w \
         CROSS JOIN LATERAL ( \
             SELECT received_at FROM ingest_inbox WHERE workspace_id = w.id \
             ORDER BY id LIMIT 1) head",
    )
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(workspace, age_seconds)| InboxHead {
            workspace_id: WorkspaceId::from_uuid(workspace),
            age_seconds,
        })
        .collect())
}

/// What one drained batch did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DrainOutcome {
    /// Inbox rows taken and deleted.
    pub rows: usize,
    /// Rows by `(kind, outcome)`, where the outcome is `valid`, `flagged`, `unplanned`,
    /// `rejected`, `duplicate` or `apply_failed`.
    pub applied: BTreeMap<(&'static str, &'static str), u64>,
    /// Accepted events that arrived late.
    pub late: u64,
    /// Whether the shard may hold more rows: a bucket's quota came back full.
    pub more: bool,
}

impl DrainOutcome {
    fn count(&mut self, kind: &'static str, outcome: &'static str) {
        *self.applied.entry((kind, outcome)).or_default() += 1;
    }

    /// Adds another batch's tallies to this one.
    pub fn absorb(&mut self, other: Self) {
        self.rows += other.rows;
        self.late += other.late;
        self.more |= other.more;
        for (key, count) in other.applied {
            *self.applied.entry(key).or_default() += count;
        }
    }
}

#[derive(FromRow)]
struct InboxRow {
    id: i64,
    api_key_id: Uuid,
    kind: String,
    item: Value,
    message_id: String,
    received_at: OffsetDateTime,
    trace_id: Uuid,
    affinity: Option<i16>,
}

impl InboxRow {
    fn trace(&self) -> TraceContext {
        TraceContext {
            trace_id: TraceId::from_uuid(self.trace_id),
            cause: Cause::api(self.api_key_id.to_string()),
        }
    }
}

enum Mapped {
    Profile(ProfileRecord),
    /// A native contact, which replaces the stored contact's fields.
    Contact(ProfileRecord),
    Event(EventRecord),
}

/// Maps a row to the record the apply path takes. A row that cannot be mapped is poison.
fn map_row(row: &InboxRow) -> Option<Mapped> {
    let stored = StoredItem::parse(&row.item)?;
    let timestamp = stored
        .timestamp
        .map(|text| OffsetDateTime::parse(&text, &Rfc3339))
        .transpose()
        .ok()?;
    let trace = row.trace();
    match row.kind.as_str() {
        "identify" => Some(Mapped::Profile(ProfileRecord {
            source: SOURCE.to_owned(),
            stream: "identify".to_owned(),
            record_key: strongest_identifier(&stored.identifiers)?,
            identifiers: stored.identifiers,
            traits: stored.traits,
            observed_at: timestamp.unwrap_or(row.received_at),
            trace,
        })),
        "contact" => Some(Mapped::Contact(ProfileRecord {
            source: contacts::SOURCE.to_owned(),
            stream: contacts::STREAM.to_owned(),
            record_key: stored
                .identifiers
                .iter()
                .find(|(kind, _)| kind == contacts::KEY_KIND)?
                .1
                .clone(),
            identifiers: stored.identifiers,
            traits: stored.traits,
            observed_at: timestamp.unwrap_or(row.received_at),
            trace,
        })),
        "track" => Some(Mapped::Event(EventRecord {
            source: SOURCE.to_owned(),
            identifiers: stored.identifiers,
            name: stored.event?,
            properties: stored.properties,
            timestamp,
            received_at: row.received_at,
            message_id: Some(row.message_id.clone()),
            trace,
        })),
        _ => None,
    }
}

/// The strongest normalised identifier as `kind:value`; equal strengths break by text, so
/// the key does not depend on the order the client listed them in.
fn strongest_identifier(identifiers: &[(String, String)]) -> Option<String> {
    identifiers
        .iter()
        .filter_map(|(kind, value)| Identifier::parse(kind, value).ok())
        .map(|identifier| {
            let text = format!("{}:{}", identifier.kind().as_text(), identifier.value());
            (Reverse(identifier.kind().strength()), text)
        })
        .min()
        .map(|(_, text)| text)
}

/// Takes the oldest rows of each bucket `shard` owns, up to `ceil(limit / owned)` per bucket
/// through the `(workspace_id, affinity, id)` index, then the `limit` oldest of those in id
/// order. Each bucket's rows are its id-ordered prefix, so a key's order holds. Also returns
/// whether the shard may hold more: a bucket came back full or the cut dropped rows.
async fn take_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    shard: Shard,
    limit: i64,
) -> Result<(Vec<InboxRow>, bool), StoreError> {
    let owned = shard.buckets();
    if owned.is_empty() {
        return Ok((Vec::new(), false));
    }
    let owned_count = i64::try_from(owned.len()).unwrap_or(1);
    let per = (limit.max(1) + owned_count - 1) / owned_count;
    let mut rows: Vec<InboxRow> = sqlx::query_as(
        "SELECT r.id, r.api_key_id, r.kind, r.item, r.message_id, r.received_at, r.trace_id, \
                r.affinity \
         FROM unnest($2::smallint[]) AS o(bucket) \
         CROSS JOIN LATERAL ( \
             SELECT id, api_key_id, kind, item, message_id, received_at, trace_id, affinity \
             FROM ingest_inbox WHERE workspace_id = $1 AND affinity = o.bucket \
             ORDER BY id LIMIT $3 FOR UPDATE SKIP LOCKED) r",
    )
    .bind(workspace.as_uuid())
    .bind(&owned)
    .bind(per)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    let mut more = false;
    for bucket in &owned {
        let taken = rows
            .iter()
            .filter(|row| row.affinity == Some(*bucket))
            .count();
        more |= i64::try_from(taken).is_ok_and(|taken| taken >= per);
    }
    rows.sort_unstable_by_key(|row| row.id);
    let cap = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
    if rows.len() > cap {
        rows.truncate(cap);
        more = true;
    }
    Ok((rows, more))
}

async fn delete_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    rows: &[InboxRow],
) -> Result<(), StoreError> {
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    sqlx::query("DELETE FROM ingest_inbox WHERE workspace_id = $1 AND id = ANY($2)")
        .bind(workspace.as_uuid())
        .bind(&ids)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Takes the contact lock of every contact in `rows`, in the drain's transaction and before any
/// row is applied, so a single write of the same contact (which holds the lock while it reads
/// and writes) and this drain see each other's fields. See the module doc of `contacts::write`
/// for the lock orders.
async fn lock_contact_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    rows: &[InboxRow],
) -> Result<(), StoreError> {
    let keys: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == "contact")
        .filter_map(|row| match map_row(row) {
            Some(Mapped::Contact(record)) => Some(record.record_key),
            _ => None,
        })
        .collect();
    lock_contacts(connection, workspace, keys.iter().map(String::as_str)).await
}

/// Applies a contact record so that it replaces the stored contact's fields.
async fn apply_contact(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
) -> Result<ProfileOutcome, StoreError> {
    let mut record = record.clone();
    replace_stored_fields(connection, workspace, &mut record).await?;
    apply_profile_record(connection, workspace, &record).await
}

/// The traits a contact record could not store, as a key to reason-code map: a single write
/// answers `422` for them, so the drain rejects the row. Holds keys and codes, never values.
fn dropped_fields(result: &ProfileOutcome) -> Map<String, Value> {
    match result {
        ProfileOutcome::Applied { dropped, .. } | ProfileOutcome::Unchanged { dropped, .. } => {
            dropped
                .iter()
                .map(|dropped| (dropped.key.clone(), Value::from(dropped.reason)))
                .collect()
        }
        ProfileOutcome::Rejected { .. } => Map::new(),
    }
}

fn tally_profile(outcome: &mut DrainOutcome, kind: &'static str, result: &ProfileOutcome) {
    let label = match result {
        ProfileOutcome::Applied { .. } => "valid",
        ProfileOutcome::Unchanged { .. } => "duplicate",
        ProfileOutcome::Rejected { .. } => "rejected",
    };
    outcome.count(kind, label);
}

fn tally_event(outcome: &mut DrainOutcome, result: &EventOutcome) {
    let label = match result {
        EventOutcome::Accepted { state, flags, .. } => {
            if flags.contains(&zirv_flow_core::events::EventFlag::Late) {
                outcome.late += 1;
            }
            state.as_str()
        }
        EventOutcome::Duplicate { .. } => "duplicate",
        EventOutcome::Rejected { .. } => "rejected",
    };
    outcome.count("track", label);
}

/// Applies up to `limit` of the workspace's oldest inbox rows of `shard` in id order and
/// deletes them, all in one transaction. Concurrent drainers skip each other's locked rows, so a row is
/// never applied twice, and a crash leaves every row in place.
///
/// An error rolls the whole batch back. When it is [`StoreError::is_retryable`] the caller
/// retries the batch; otherwise a row is probably poison, and
/// [`drain_batch_singly`] isolates it.
pub async fn drain_batch(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
    key: &HmacKey,
    limit: i64,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    let (rows, more) = take_rows(&mut tx, workspace, shard, limit).await?;
    lock_contact_rows(&mut tx, workspace, &rows).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        more,
        ..DrainOutcome::default()
    };
    let mut events = EventBatch::default();
    for row in &rows {
        match map_row(row).ok_or_else(StoreError::poison)? {
            Mapped::Profile(record) => {
                let result = apply_profile_record(&mut tx, workspace, &record).await?;
                tally_profile(&mut outcome, "identify", &result);
            }
            Mapped::Contact(record) => {
                let result = apply_contact(&mut tx, workspace, &record).await?;
                // Rolls the batch back, so that the row is isolated and rejected singly.
                if !dropped_fields(&result).is_empty() {
                    return Err(StoreError::poison());
                }
                tally_profile(&mut outcome, "contact", &result);
            }
            Mapped::Event(record) => {
                let result =
                    apply_event_record(&mut tx, workspace, key, &mut events, &record).await?;
                tally_event(&mut outcome, &result);
            }
        }
    }
    events.finish(&mut tx, workspace).await?;
    delete_rows(&mut tx, workspace, &rows).await?;
    commit(tx).await?;
    Ok(outcome)
}

/// [`drain_batch`] that applies each row in a savepoint of its own. A row that fails with a
/// non-retryable error becomes an `apply_failed` ingest reject and is deleted, and the
/// batch carries on. A retryable error still rolls the whole batch back.
pub async fn drain_batch_singly(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
    key: &HmacKey,
    limit: i64,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    let (rows, more) = take_rows(&mut tx, workspace, shard, limit).await?;
    lock_contact_rows(&mut tx, workspace, &rows).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        more,
        ..DrainOutcome::default()
    };
    for row in &rows {
        let mapped = map_row(row);
        let mut savepoint = tx.begin().await.map_err(StoreError::query)?;
        let mut unstorable = Map::new();
        let applied = match &mapped {
            Some(Mapped::Profile(record)) => {
                apply_profile_record(&mut savepoint, workspace, record)
                    .await
                    .map(|result| tally_profile(&mut outcome, "identify", &result))
            }
            Some(Mapped::Contact(record)) => apply_contact(&mut savepoint, workspace, record)
                .await
                .map(|result| {
                    unstorable = dropped_fields(&result);
                    if unstorable.is_empty() {
                        tally_profile(&mut outcome, "contact", &result);
                    }
                }),
            Some(Mapped::Event(record)) => {
                let mut events = EventBatch::default();
                match apply_event_record(&mut savepoint, workspace, key, &mut events, record).await
                {
                    Ok(result) => {
                        tally_event(&mut outcome, &result);
                        events.finish(&mut savepoint, workspace).await
                    }
                    Err(error) => Err(error),
                }
            }
            None => Err(StoreError::poison()),
        };
        match applied {
            Ok(()) if unstorable.is_empty() => {
                savepoint.commit().await.map_err(StoreError::query)?;
            }
            Ok(()) => {
                // What a single write answers `422` for: the row is rejected whole, with the
                // keys and codes of the fields as its sample.
                savepoint.rollback().await.map_err(StoreError::query)?;
                if let Some(Mapped::Contact(record)) = &mapped {
                    let mut rejected = record.clone();
                    rejected.traits = unstorable;
                    reject_row(
                        &mut tx,
                        workspace,
                        key,
                        row,
                        Some(&Mapped::Contact(rejected)),
                    )
                    .await?;
                }
                outcome.count("contact", "rejected");
            }
            Err(error) if error.is_retryable() => return Err(error),
            Err(_) => {
                savepoint.rollback().await.map_err(StoreError::query)?;
                reject_row(&mut tx, workspace, key, row, mapped.as_ref()).await?;
                let kind = if row.kind == "contact" {
                    "contact"
                } else if row.kind == "identify" {
                    "identify"
                } else {
                    "track"
                };
                outcome.count(kind, "apply_failed");
            }
        }
    }
    delete_rows(&mut tx, workspace, &rows).await?;
    commit(tx).await?;
    Ok(outcome)
}

async fn reject_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    row: &InboxRow,
    mapped: Option<&Mapped>,
) -> Result<(), StoreError> {
    let empty = Map::new();
    let trace = row.trace();
    let item = match mapped {
        Some(Mapped::Profile(record)) => FailedItem {
            kind: "record",
            source: SOURCE,
            name: None,
            values: &record.traits,
            timestamp: Some(record.observed_at),
            received_at: row.received_at,
            message_id: Some(&row.message_id),
            trace: &trace,
        },
        Some(Mapped::Contact(record)) => FailedItem {
            kind: "record",
            source: &record.source,
            name: None,
            values: &record.traits,
            timestamp: Some(record.observed_at),
            received_at: row.received_at,
            message_id: Some(&row.message_id),
            trace: &trace,
        },
        Some(Mapped::Event(record)) => FailedItem {
            kind: "event",
            source: SOURCE,
            name: Some(&record.name),
            values: &record.properties,
            timestamp: record.timestamp,
            received_at: row.received_at,
            message_id: Some(&row.message_id),
            trace: &trace,
        },
        None => FailedItem {
            kind: if row.kind != "track" {
                "record"
            } else {
                "event"
            },
            source: SOURCE,
            name: None,
            values: &empty,
            timestamp: None,
            received_at: row.received_at,
            message_id: Some(&row.message_id),
            trace: &trace,
        },
    };
    reject_apply_failed(connection, workspace, key, &item).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{CauseKind, HmacPurpose, MasterKey, Slug, WorkspaceName};

    use super::*;
    use crate::{create_workspace, list_trace};

    fn key() -> HmacKey {
        MasterKey::generate().hmac_key(HmacPurpose::Redaction)
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn pairs(identifiers: &[(&str, &str)]) -> Vec<(String, String)> {
        identifiers
            .iter()
            .map(|(kind, value)| ((*kind).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn identify(user: &str, message_id: &str, traits: Value) -> IngestItem {
        IngestItem {
            identifiers: pairs(&[("external:user_id", user)]),
            timestamp: None,
            message_id: Some(message_id.to_owned()),
            payload: IngestPayload::Identify {
                traits: object(traits),
            },
        }
    }

    fn track(user: &str, message_id: &str, event: &str, properties: Value) -> IngestItem {
        IngestItem {
            identifiers: pairs(&[("external:user_id", user)]),
            timestamp: None,
            message_id: Some(message_id.to_owned()),
            payload: IngestPayload::Track {
                event: event.to_owned(),
                properties: object(properties),
            },
        }
    }

    fn row(kind: &str, item: Value) -> InboxRow {
        InboxRow {
            id: 1,
            api_key_id: Uuid::nil(),
            kind: kind.to_owned(),
            item,
            message_id: "m-1".to_owned(),
            received_at: datetime!(2026-02-03 04:05:06 UTC),
            trace_id: Uuid::nil(),
            affinity: None,
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn append(pool: &PgPool, ws: WorkspaceId, trace: &TraceContext, items: &[IngestItem]) {
        let appended = append_inbox(pool, ws, Uuid::now_v7(), trace, items)
            .await
            .unwrap();
        assert_eq!(appended, items.len());
    }

    async fn inbox_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM ingest_inbox")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn the_record_key_is_the_strongest_identifier_whatever_the_order() {
        let forward = pairs(&[
            ("anonymous_id", "a1"),
            ("email", "A@Example.com"),
            ("external:crm", "7"),
        ]);
        let mut backward = forward.clone();
        backward.reverse();

        assert_eq!(
            strongest_identifier(&forward).as_deref(),
            Some("external:crm:7")
        );
        assert_eq!(
            strongest_identifier(&backward),
            strongest_identifier(&forward)
        );
        assert_eq!(
            strongest_identifier(&pairs(&[("email", "A@Example.com")])).as_deref(),
            Some("email:a@example.com")
        );
        assert_eq!(strongest_identifier(&pairs(&[("fax", "1")])), None);
    }

    #[test]
    fn an_identify_row_maps_to_a_profile_record_observed_at_its_timestamp_or_arrival() {
        let stamped = json!({
            "identifiers": [["email", "a@example.com"]],
            "timestamp": "2026-01-01T00:00:00Z",
            "traits": {"plan": "pro"},
        });
        let unstamped = json!({"identifiers": [["email", "a@example.com"]], "timestamp": null});

        let Some(Mapped::Profile(with)) = map_row(&row("identify", stamped)) else {
            panic!("not a profile record");
        };
        let Some(Mapped::Profile(without)) = map_row(&row("identify", unstamped)) else {
            panic!("not a profile record");
        };

        assert_eq!(with.observed_at, datetime!(2026-01-01 00:00 UTC));
        assert_eq!(without.observed_at, datetime!(2026-02-03 04:05:06 UTC));
        assert_eq!(
            (with.source.as_str(), with.stream.as_str()),
            ("api", "identify")
        );
        assert_eq!(with.record_key, "email:a@example.com");
        assert_eq!(with.traits["plan"], "pro");
    }

    #[test]
    fn a_contact_row_maps_to_a_native_record_keyed_by_its_contact_key() {
        let item = IngestItem {
            identifiers: pairs(&[("external:contact_key", "c-1"), ("email", "a@example.com")]),
            timestamp: None,
            message_id: None,
            payload: IngestPayload::Contact {
                traits: object(json!({"plan": "pro"})),
            },
        };
        let stored = stored_json(&item);

        let Some(Mapped::Contact(record)) = map_row(&row("contact", stored)) else {
            panic!("not a contact");
        };

        assert_eq!(
            (record.source.as_str(), record.stream.as_str()),
            ("native", "contacts")
        );
        assert_eq!(record.record_key, "c-1");
        assert_eq!(record.identifiers.len(), 2);
        assert_eq!(record.traits["plan"], "pro");
        assert_eq!(item.payload.kind(), "contact");
        // Without its key the row cannot be placed: it is poison.
        let keyless = json!({"identifiers": [["email", "a@example.com"]], "traits": {}});
        assert!(map_row(&row("contact", keyless)).is_none());
    }

    #[test]
    fn a_track_row_maps_to_an_event_record_with_its_message_id() {
        let item = json!({
            "identifiers": [["anonymous_id", "a1"]],
            "timestamp": null,
            "event": "seen",
            "properties": {"n": 1},
        });

        let Some(Mapped::Event(event)) = map_row(&row("track", item)) else {
            panic!("not an event record");
        };

        assert_eq!(event.name, "seen");
        assert_eq!(event.message_id.as_deref(), Some("m-1"));
        assert_eq!(event.received_at, datetime!(2026-02-03 04:05:06 UTC));
        assert_eq!(event.timestamp, None);
    }

    #[test]
    fn a_row_that_does_not_match_its_kind_is_poison() {
        let one = json!({"identifiers": [["email", "a@example.com"]]});
        let bad_time = json!({
            "identifiers": [["email", "a@example.com"]],
            "event": "x",
            "timestamp": "soon",
        });

        assert!(map_row(&row("identify", json!({}))).is_none());
        assert!(map_row(&row("identify", json!({"identifiers": []}))).is_none());
        assert!(map_row(&row("track", one.clone())).is_none());
        assert!(map_row(&row("track", bad_time)).is_none());
        assert!(map_row(&row("other", one)).is_none());
    }

    #[test]
    fn a_stored_item_reads_back_as_it_was_written() {
        let item = IngestItem {
            timestamp: Some(datetime!(2026-01-01 00:00 UTC)),
            ..track("u1", "m-1", "seen", json!({"n": 1}))
        };

        let stored = StoredItem::parse(&stored_json(&item)).unwrap();

        assert_eq!(stored.identifiers, item.identifiers);
        assert_eq!(stored.timestamp.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(stored.event.as_deref(), Some("seen"));
        assert_eq!(stored.properties["n"], 1);
    }

    #[test]
    fn absorbing_adds_every_tally() {
        let mut total = DrainOutcome::default();
        let mut one = DrainOutcome {
            rows: 2,
            late: 1,
            ..DrainOutcome::default()
        };
        one.count("track", "valid");
        one.count("track", "valid");
        total.absorb(one.clone());
        total.absorb(one);

        assert_eq!((total.rows, total.late), (4, 2));
        assert_eq!(total.applied[&("track", "valid")], 4);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_writes_every_row_with_the_trace_and_enqueues_one_drain(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req-1"));
        let mut unlabelled = track("u1", "unused", "seen", json!({}));
        unlabelled.message_id = None;

        append(
            &pool,
            ws,
            &trace,
            &[identify("u1", "m-1", json!({})), unlabelled],
        )
        .await;
        append(&pool, ws, &trace, &[identify("u2", "m-2", json!({}))]).await;

        let rows: Vec<(String, String, Uuid)> =
            sqlx::query_as("SELECT kind, message_id, trace_id FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, "identify");
        assert_eq!(rows[1].0, "track");
        assert!(Uuid::parse_str(&rows[1].1).is_ok(), "a server-assigned id");
        assert!(rows.iter().all(|row| row.2 == trace.trace_id.as_uuid()));
        let jobs: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT kind, unique_key FROM jobs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            jobs,
            [(
                DRAIN_JOB_KIND.to_owned(),
                Some(format!("ingest.drain:{}", ws.as_uuid()))
            )]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_applies_in_order_deletes_the_rows_and_records_the_request_trace(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req-1"));
        append(
            &pool,
            ws,
            &trace,
            &[
                identify("u1", "m-1", json!({"plan": "free"})),
                identify("u1", "m-2", json!({"plan": "pro"})),
                track("u1", "m-3", "seen", json!({})),
                track("u1", "m-3", "seen", json!({})),
            ],
        )
        .await;

        let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();

        assert_eq!(outcome.rows, 4);
        assert_eq!(outcome.applied[&("identify", "valid")], 2);
        assert_eq!(outcome.applied[&("track", "unplanned")], 1);
        assert_eq!(outcome.applied[&("track", "duplicate")], 1);
        assert_eq!(inbox_count(&pool).await, 0);
        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 50).await.unwrap();
        let actions: Vec<&str> = decisions.iter().map(|d| d.action.as_str()).collect();
        assert!(actions.contains(&"person.created"), "{actions:?}");
        assert!(actions.contains(&"event.accepted"), "{actions:?}");
        assert!(decisions.iter().all(|d| d.cause.kind == CauseKind::Api));
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE trace_id = $1")
            .bind(trace.trace_id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 1);
        let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(people, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_takes_at_most_the_limit_oldest_rows_of_its_own_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let name = WorkspaceName::parse("two").unwrap();
        let slug = Slug::parse("two").unwrap();
        let two = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let trace = TraceContext::root(Cause::api("req"));
        let items: Vec<IngestItem> = (0..5)
            .map(|n| track("u1", &format!("m-{n}"), "seen", json!({"n": n})))
            .collect();
        append(&pool, one, &trace, &items).await;
        append(&pool, two, &trace, &items).await;
        // One bucket each, so the per-bucket quota does not narrow the batch.
        sqlx::query("UPDATE ingest_inbox SET affinity = id % 64")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = drain_batch(&pool, one, Shard::ALL, &key(), 3)
            .await
            .unwrap();

        assert_eq!(outcome.rows, 3);
        let left: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT workspace_id, message_id FROM ingest_inbox ORDER BY workspace_id, id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(left.iter().filter(|(w, _)| *w == two.as_uuid()).count(), 5);
        let mine: Vec<&str> = left
            .iter()
            .filter(|(w, _)| *w == one.as_uuid())
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(mine, ["m-3", "m-4"]);
    }

    /// Every row of the tables a drain writes, with ids, in a stable order. `duplicates`
    /// counts resends and is the one column a replay is meant to change.
    async fn dump(pool: &PgPool) -> Vec<(&'static str, Vec<String>)> {
        let mut state = Vec::new();
        for table in [
            "people",
            "identities",
            "source_records",
            "person_changes",
            "events",
            "event_dedupe",
            "ingest_rejects",
            "decision_records",
        ] {
            let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT (to_jsonb(t) - 'duplicates')::text AS row FROM {table} t ORDER BY 1"
            )))
            .fetch_all(pool)
            .await
            .unwrap();
            state.push((table, rows));
        }
        state
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replaying_the_same_batch_leaves_the_database_identical(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        crate::set_event_policy(
            &pool,
            ws,
            &crate::EventPolicy {
                unplanned: zirv_flow_core::events::UnplannedPolicy::Reject,
                ..crate::EventPolicy::default()
            },
        )
        .await
        .unwrap();
        crate::create_event_schema(
            &pool,
            ws,
            &crate::NewEventSchema {
                name: "planned".into(),
                mode: zirv_flow_core::events::EventMode::Allow,
                description: String::new(),
                properties: Vec::new(),
            },
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        crate::publish_schema_draft(&mut tx, ws, "planned", None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let trace = TraceContext::root(Cause::api("req-1"));
        let batch = || {
            let mut stamped = identify("u2", "m-2", json!({"plan": "pro", "seats": 3}));
            stamped.timestamp = Some(datetime!(2026-01-01 00:00 UTC));
            let mut late = track("u1", "m-4", "planned", json!({"n": 1}));
            late.timestamp = Some(datetime!(2020-01-01 00:00 UTC));
            vec![
                identify("u1", "m-1", json!({"plan": "free"})),
                stamped,
                track("u1", "m-3", "planned", json!({"n": 1})),
                late,
                track("u1", "m-5", "not_planned", json!({"n": 1})),
                track("u3", "m-6", "planned", json!({})),
            ]
        };
        append(&pool, ws, &trace, &batch()).await;
        drain_batch(&pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();
        let before = dump(&pool).await;
        assert!(
            before.iter().all(|(_, rows)| !rows.is_empty()),
            "{before:?}"
        );

        append(&pool, ws, &trace, &batch()).await;
        let replay = drain_batch(&pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();

        assert_eq!(replay.rows, 6);
        assert_eq!(dump(&pool).await, before);
        let duplicates: i64 =
            sqlx::query_scalar("SELECT COALESCE(sum(duplicates), 0)::bigint FROM event_dedupe")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(duplicates, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_drainers_apply_every_row_exactly_once(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        sqlx::query(
            "INSERT INTO ingest_inbox \
                (workspace_id, api_key_id, kind, item, message_id, trace_id, affinity) \
             SELECT $1, gen_random_uuid(), 'track', \
                    jsonb_build_object('identifiers', \
                        jsonb_build_array(jsonb_build_array('external:user_id', 'u' || (n % 400))), \
                        'timestamp', null, 'event', 'seen', 'properties', jsonb_build_object('n', n)), \
                    'm-' || n, $2, n % 64 \
             FROM generate_series(1, 2000) AS n",
        )
        .bind(ws.as_uuid())
        .bind(trace.trace_id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let key = std::sync::Arc::new(key());

        let drainer = |pool: PgPool, key: std::sync::Arc<HmacKey>| async move {
            loop {
                match drain_batch(&pool, ws, Shard::ALL, &key, 100).await {
                    Ok(outcome) if outcome.rows == 0 => break,
                    Ok(_) => {}
                    Err(error) if error.is_retryable() => {}
                    Err(error) => panic!("{error}"),
                }
            }
        };
        tokio::join!(
            drainer(pool.clone(), key.clone()),
            drainer(pool.clone(), key.clone())
        );

        let (events, distinct, duplicates): (i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM events), \
                    (SELECT count(DISTINCT message_id) FROM events), \
                    (SELECT COALESCE(sum(duplicates), 0)::bigint FROM event_dedupe)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((events, distinct, duplicates), (2000, 2000, 0));
        assert_eq!(inbox_count(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_row_becomes_an_apply_failed_reject_while_the_rest_apply(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        append(
            &pool,
            ws,
            &trace,
            &[
                track("u1", "m-1", "seen", json!({})),
                track("u1", "poison", "seen", json!({})),
                track("u1", "m-3", "seen", json!({})),
            ],
        )
        .await;
        sqlx::query(
            "UPDATE ingest_inbox SET item = '{\"identifiers\": 5}' WHERE message_id = 'poison'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let key = key();

        let whole = drain_batch(&pool, ws, Shard::ALL, &key, 500)
            .await
            .unwrap_err();
        assert!(!whole.is_retryable());
        assert_eq!(
            inbox_count(&pool).await,
            3,
            "the failed batch left every row"
        );
        let outcome = drain_batch_singly(&pool, ws, Shard::ALL, &key, 500)
            .await
            .unwrap();

        assert_eq!(outcome.rows, 3);
        assert_eq!(outcome.applied[&("track", "apply_failed")], 1);
        assert_eq!(outcome.applied[&("track", "unplanned")], 2);
        assert_eq!(inbox_count(&pool).await, 0);
        let rejects: Vec<(String, String, Option<String>)> =
            sqlx::query_as("SELECT kind, reason, message_id FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            rejects,
            [("event".into(), "apply_failed".into(), Some("poison".into()))]
        );
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 2);
        let rejected: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records \
             WHERE action = 'event.rejected' AND reason = 'apply_failed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rejected, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unmappable_identify_row_becomes_a_record_reject(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        append(
            &pool,
            ws,
            &trace,
            &[identify("u1", "bad", json!({"plan": "pro"}))],
        )
        .await;
        sqlx::query("UPDATE ingest_inbox SET item = '[]'")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = drain_batch_singly(&pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();

        assert_eq!(outcome.applied[&("identify", "apply_failed")], 1);
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM ingest_rejects")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kinds, ["record"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_heads_report_the_oldest_row_of_each_workspace_with_rows(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let name = WorkspaceName::parse("two").unwrap();
        let slug = Slug::parse("two").unwrap();
        let two = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let trace = TraceContext::root(Cause::api("req"));
        append(&pool, one, &trace, &[track("u1", "m-1", "seen", json!({}))]).await;
        sqlx::query("UPDATE ingest_inbox SET received_at = now() - interval '30 seconds'")
            .execute(&pool)
            .await
            .unwrap();
        append(&pool, one, &trace, &[track("u1", "m-2", "seen", json!({}))]).await;

        let heads = inbox_heads(&pool).await.unwrap();

        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].workspace_id, one);
        assert!((29.0..60.0).contains(&heads[0].age_seconds), "{heads:?}");
        assert!(heads.iter().all(|head| head.workspace_id != two));
    }

    #[test]
    fn the_bucket_of_a_key_is_pinned() {
        assert_eq!(bucket("external:user_id:u1"), 19);
        assert_eq!(bucket("email:ada@example.com"), 27);
        assert!((0..500).all(|n| (0..64).contains(&bucket(&format!("external:user_id:u{n}")))));
    }

    #[test]
    fn an_items_bucket_is_that_of_its_strongest_identifier_whatever_the_order() {
        let mut item = identify("u1", "m-1", json!({}));
        item.identifiers = pairs(&[("external:user_id", "u1"), ("email", "Ada@Example.com")]);
        let one = item_bucket(&item);
        item.identifiers.reverse();

        assert_eq!(item_bucket(&item), one);
        assert_eq!(one, bucket("external:user_id:u1"));
    }

    #[test]
    fn an_item_without_a_valid_identifier_is_in_bucket_zero() {
        let mut item = identify("u1", "m-1", json!({}));
        item.identifiers = pairs(&[("not-a-kind", "x")]);

        assert_eq!(item_bucket(&item), 0);
        item.identifiers.clear();
        assert_eq!(item_bucket(&item), 0);
    }

    #[test]
    fn a_contact_is_bucketed_by_its_contact_key_whatever_its_other_identifiers() {
        let contact = |extra: &[(&str, &str)]| {
            let mut identifiers = pairs(&[(contacts::KEY_KIND, "c-1")]);
            identifiers.extend(pairs(extra));
            IngestItem {
                identifiers,
                timestamp: None,
                message_id: None,
                payload: IngestPayload::Contact { traits: Map::new() },
            }
        };

        assert_eq!(item_bucket(&contact(&[])), bucket("c-1"));
        assert_eq!(
            item_bucket(&contact(&[("email", "ada@example.com")])),
            bucket("c-1")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_stores_the_bucket_of_each_items_record_key(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        let mut unkeyed = identify("u1", "m-3", json!({}));
        unkeyed.identifiers.clear();

        append(
            &pool,
            ws,
            &trace,
            &[
                identify("u1", "m-1", json!({})),
                track("u1", "m-2", "seen", json!({})),
                unkeyed,
            ],
        )
        .await;

        let stored: Vec<Option<i16>> =
            sqlx::query_scalar("SELECT affinity FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(stored, [Some(19), Some(19), Some(0)]);
    }

    /// What `shard` takes from the workspace's inbox, in id order, and whether it may hold more.
    async fn taken(pool: &PgPool, ws: WorkspaceId, shard: Shard, limit: i64) -> (Vec<i64>, bool) {
        let mut tx = begin(pool).await.unwrap();
        let (rows, more) = take_rows(&mut tx, ws, shard, limit).await.unwrap();
        (rows.iter().map(|row| row.id).collect(), more)
    }

    /// Inserts `count` track rows of distinct keys straight into bucket `affinity`.
    async fn raw_rows(
        pool: &PgPool,
        ws: WorkspaceId,
        affinity: Option<i16>,
        count: i32,
        tag: &str,
    ) {
        sqlx::query(
            "INSERT INTO ingest_inbox \
                (workspace_id, api_key_id, kind, item, message_id, trace_id, affinity) \
             SELECT $1, gen_random_uuid(), 'track', \
                    jsonb_build_object('identifiers', \
                        jsonb_build_array(jsonb_build_array('external:user_id', $4::text || n::text)), \
                        'timestamp', null, 'event', 'seen', 'properties', jsonb_build_object()), \
                    $4::text || n::text, gen_random_uuid(), $2::smallint \
             FROM generate_series(1, $3) AS n",
        )
        .bind(ws.as_uuid())
        .bind(affinity)
        .bind(count)
        .bind(tag)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shard_takes_only_its_own_buckets_in_id_order(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        let items: Vec<IngestItem> = (0..60)
            .map(|n| track(&format!("u{n}"), &format!("m-{n}"), "seen", json!({})))
            .collect();
        append(&pool, ws, &trace, &items).await;
        sqlx::query("UPDATE ingest_inbox SET affinity = NULL WHERE message_id IN ('m-0', 'm-1')")
            .execute(&pool)
            .await
            .unwrap();
        let all: Vec<(i64, Option<i16>)> =
            sqlx::query_as("SELECT id, affinity FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();

        let mut shards = Vec::new();
        for index in 0..4 {
            shards.push(taken(&pool, ws, Shard { index, count: 4 }, 1000).await.0);
        }

        for (index, ids) in shards.iter().enumerate() {
            let expected: Vec<i64> = all
                .iter()
                .filter(|(_, affinity)| {
                    affinity.is_some_and(|a| usize::try_from(a).unwrap() % 4 == index)
                })
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(ids, &expected, "shard {index}");
        }
        let unbucketed = all
            .iter()
            .filter(|(_, affinity)| affinity.is_none())
            .count();
        assert_eq!(shards.concat().len() + unbucketed, all.len());
        assert_eq!(unbucketed, 2, "no shard takes an unbucketed row");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_backfill_gives_a_row_the_bucket_an_append_would_have(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        let contact = IngestItem {
            identifiers: pairs(&[(contacts::KEY_KIND, "c-1"), ("email", "ada@example.com")]),
            timestamp: None,
            message_id: Some("m-3".to_owned()),
            payload: IngestPayload::Contact { traits: Map::new() },
        };
        let mut email = identify("u2", "m-4", json!({}));
        email.identifiers = pairs(&[("email", "Ada@Example.com"), ("external:user_id", "u2")]);
        append(
            &pool,
            ws,
            &trace,
            &[
                identify("u1", "m-1", json!({})),
                track("u7", "m-2", "seen", json!({})),
                contact,
                email,
            ],
        )
        .await;
        let appended: Vec<Option<i16>> =
            sqlx::query_scalar("SELECT affinity FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        sqlx::query("UPDATE ingest_inbox SET affinity = NULL")
            .execute(&pool)
            .await
            .unwrap();

        let done = backfill_affinity(&pool, ws, 3).await.unwrap();
        let rest = backfill_affinity(&pool, ws, 3).await.unwrap();
        let none = backfill_affinity(&pool, ws, 3).await.unwrap();

        assert_eq!((done, rest, none), (3, 1, 0));
        let backfilled: Vec<Option<i16>> =
            sqlx::query_scalar("SELECT affinity FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(backfilled, appended);
        assert!(appended.iter().all(Option::is_some));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hot_bucket_does_not_starve_the_shards_other_buckets(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        raw_rows(&pool, ws, Some(0), 24, "hot").await;
        raw_rows(&pool, ws, Some(4), 1, "cold").await;
        let cold: i64 = sqlx::query_scalar("SELECT id FROM ingest_inbox WHERE affinity = 4")
            .fetch_one(&pool)
            .await
            .unwrap();

        let (ids, more) = taken(&pool, ws, Shard { index: 0, count: 4 }, 8).await;

        assert!(
            ids.contains(&cold),
            "the first batch includes the cold bucket's row"
        );
        assert!(ids.len() <= 8);
        assert!(more, "the hot bucket has more");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shard_with_empty_buckets_takes_nothing_while_another_holds_a_backlog(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        raw_rows(&pool, ws, Some(0), 300, "hot").await;

        let (ids, more) = taken(&pool, ws, Shard { index: 1, count: 4 }, 8).await;

        assert!(ids.is_empty());
        assert!(!more, "an empty shard is finished after one batch");
        let (ids, more) = taken(&pool, ws, Shard { index: 0, count: 4 }, 8).await;
        assert!(!ids.is_empty() && ids.len() <= 8 && more);
    }
}
