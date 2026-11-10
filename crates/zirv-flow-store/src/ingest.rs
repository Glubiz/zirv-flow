//! The durable ingestion inbox (architecture sections 4.4 and 7.1). The API appends raw
//! items and answers once the append has committed; `ingest.drain` applies them through the
//! profile and event paths of apply-batch and deletes them in the same transaction.
//!
//! Nothing here logs or labels anything with an identifier, a trait or property value or an
//! event name.

mod cursors;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use hashbrown::{HashMap, HashSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::Connection;
use sqlx::Row;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::contacts;
use zirv_flow_core::people::Identifier;
use zirv_flow_core::{Cause, HmacKey, TraceContext, TraceId, WorkspaceId};

use crate::contacts::{lock_contacts, replace_stored_fields};
use crate::events::{
    DroppedTraits, EventBatch, EventOutcome, EventRecord, FailedItem, Refusal, apply_event_record,
    insert_dropped_traits_reject, insert_record_rejects, reject_apply_failed,
};
use crate::jobs::{DEFAULT_MAX_ATTEMPTS, NOTIFY_CHANNEL};
use crate::lists::ListCounts;
use crate::people::{
    ProfileOutcome, ProfileRecord, apply_profile_record, apply_profile_record_into,
};
use crate::webhooks::HookResult;
use crate::webhooks::{HookConfigs, HookMapped, apply_hook_record, map_hook_row};
use crate::{NewJob, PgConnection, PgPool, StoreError, begin, commit, enqueue, enqueue_many};
use cursors::{TakeCursors, TakeStart, Taken};

/// The job kind that drains one shard of a workspace's inbox.
pub const DRAIN_JOB_KIND: &str = "ingest.drain";

/// The take cursors of this process ([`cursors`]), keyed by workspace.
static TAKE_CURSORS: LazyLock<TakeCursors> = LazyLock::new(TakeCursors::default);

/// The shard count the appenders and the safety net enqueue by (`ingest.drain_shards`), set once
/// at start-up. A drain job carries the count it was enqueued with and drains that count's
/// buckets, so a worker configured differently, as in a config change or a rolling deploy, still
/// drains what the job names.
static DRAIN_SHARDS: AtomicU32 = AtomicU32::new(8);

/// Sets the shard count new drain jobs are enqueued by, clamped to `1..=Shard::MAX_COUNT`.
pub fn set_drain_shards(count: u32) {
    DRAIN_SHARDS.store(count.clamp(1, Shard::MAX_COUNT), Ordering::Relaxed);
}

/// The shard count new drain jobs are enqueued by.
pub fn drain_shards() -> u32 {
    DRAIN_SHARDS.load(Ordering::Relaxed)
}

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Shard {
    pub index: u32,
    pub count: u32,
}

impl Shard {
    /// Every bucket: one shard of one.
    pub const ALL: Self = Self { index: 0, count: 1 };

    /// The most shards there can be: one per bucket.
    pub const MAX_COUNT: u32 = BUCKETS as u32;

    /// The shard of `count` that owns `bucket`.
    pub(crate) fn of_bucket(bucket: i16, count: u32) -> Self {
        let count = count.clamp(1, Self::MAX_COUNT);
        Self {
            index: u32::try_from(bucket).unwrap_or(0) % count,
            count,
        }
    }

    /// Whether the shard names buckets: `count` in `1..=MAX_COUNT` and `index` below it.
    pub fn is_valid(self) -> bool {
        (1..=Self::MAX_COUNT).contains(&self.count) && self.index < self.count
    }

    /// The buckets this shard owns.
    fn buckets(self) -> Vec<i16> {
        let count = self.count.max(1);
        (0..i16::from(BUCKETS))
            .filter(|bucket| u32::try_from(*bucket).is_ok_and(|b| b % count == self.index))
            .collect()
    }
}

/// The bucket of a lane key: the first byte of its SHA-256, modulo [`BUCKETS`]. A stored bucket
/// is never recomputed, so a row keeps the lane it was received into whatever the key rule was
/// then or has become since (architecture 7.1 lists what that does to order).
pub(crate) fn bucket(record_key: &str) -> i16 {
    let digest = Sha256::digest(record_key.as_bytes());
    i16::from(digest[0] % BUCKETS)
}

/// The people a batch's identifiers already belong to, by normalized `kind:value` text, each
/// resolved to the survivor of a merge.
pub(crate) type KnownPeople = HashMap<String, Uuid>;

/// Looks up, in ONE statement over `identities (workspace_id, kind, value)` joined to `people`,
/// the person each identifier of `rows` belongs to. A person merged into another resolves to the
/// survivor by one hop of `people.merged_into`: a merge points the loser at the survivor, and a
/// survivor merged in turn is rare enough that a stale lane costs only ordering, which 7.1
/// already does not promise across a merge. A batch with no valid identifier skips the query.
pub(crate) async fn known_people<'a, 'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace: WorkspaceId,
    rows: impl IntoIterator<Item = &'a [(String, String)]>,
) -> Result<KnownPeople, StoreError> {
    let mut wanted: HashSet<(String, String)> = HashSet::new();
    for identifiers in rows {
        for (kind, value) in identifiers {
            if let Ok(identifier) = Identifier::parse(kind, value) {
                wanted.insert((
                    identifier.kind().as_text().to_owned(),
                    identifier.value().to_owned(),
                ));
            }
        }
    }
    if wanted.is_empty() {
        return Ok(KnownPeople::new());
    }
    let (kinds, values): (Vec<String>, Vec<String>) = wanted.into_iter().unzip();
    let found: Vec<(String, String, Uuid)> = sqlx::query_as(
        "SELECT i.kind, i.value, COALESCE(p.merged_into, p.id)          FROM unnest($2::text[], $3::text[]) AS q(kind, value)          JOIN identities i            ON i.workspace_id = $1 AND i.kind = q.kind AND i.value = q.value          JOIN people p ON p.id = i.person_id",
    )
    .bind(workspace.as_uuid())
    .bind(&kinds)
    .bind(&values)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(found
        .into_iter()
        .map(|(kind, value, person)| (format!("{kind}:{value}"), person))
        .collect())
}

/// The lane key of a row of `kind` with `identifiers`, given the `people` its batch resolved:
/// 1. the person of the strongest identifier that already belongs to one (`person:<id>`), a
///    contact row included, so a person's rows from every channel share a lane;
/// 2. else, for a contact, its contact key;
/// 3. else the anonymousId, which an identify then shares with the page views before it, or the
///    strongest identifier.
///
/// `None` when no identifier is valid: such a row is poison anyway.
pub(crate) fn lane_key(
    kind: &str,
    identifiers: &[(String, String)],
    people: &KnownPeople,
) -> Option<String> {
    // Strongest first, equal strengths by text, so the key ignores the order the client listed
    // the identifiers in.
    let mut parsed: Vec<(Reverse<u8>, String)> = identifiers
        .iter()
        .filter_map(|(kind, value)| Identifier::parse(kind, value).ok())
        .map(|identifier| {
            let text = format!("{}:{}", identifier.kind().as_text(), identifier.value());
            (Reverse(identifier.kind().strength()), text)
        })
        .collect();
    parsed.sort();
    if let Some(person) = parsed.iter().find_map(|(_, text)| people.get(text)) {
        return Some(format!("person:{person}"));
    }
    if kind == "contact"
        && let Some((_, key)) = identifiers
            .iter()
            .find(|(kind, _)| kind == contacts::KEY_KIND)
    {
        return Some(key.clone());
    }
    let anonymous = parsed
        .iter()
        .find(|(_, text)| text.starts_with("anonymous_id:"));
    anonymous.or(parsed.first()).map(|(_, text)| text.clone())
}

/// The bucket of a row's lane key; bucket 0 when it has none. The append, the webhook receive
/// and the backfill all go through here, so they cannot diverge.
pub(crate) fn record_bucket(
    kind: &str,
    identifiers: &[(String, String)],
    people: &KnownPeople,
) -> i16 {
    lane_key(kind, identifiers, people).map_or(0, |key| bucket(&key))
}

/// The bucket of `item`'s lane key.
fn item_bucket(item: &IngestItem, people: &KnownPeople) -> i16 {
    record_bucket(item.payload.kind(), &item.identifiers, people)
}

/// Buckets up to `limit` of the workspace's rows that have none, the oldest first, in a
/// transaction of its own, and returns how many it did. Rows a pre-bucketing or pre-0.29
/// writer appended carry no bucket; this puts them in their key's queue by id. The lock is
/// blocking, not `SKIP LOCKED`: it waits out an erasure and holds nothing else, so it cannot
/// deadlock. A count below `limit` means none are left, also while several shard jobs run it
/// at once: the locking clause sits below the `LIMIT`, so a row another run bucketed and
/// committed while this one waited fails the re-check and is replaced by the next unbucketed
/// row, and the scan comes up short only when it has seen every row that lacked a bucket.
/// A bucketed row keeps its old id, which may sit below a take cursor, so bucketing any row drops
/// this process's cursors of the workspace; another process's next full walk finds the row.
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
            StoredItem::parse(item).map_or(0, |stored| {
                record_bucket(kind, &stored.identifiers, &KnownPeople::new())
            })
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
    if !rows.is_empty() {
        TAKE_CURSORS.forget(workspace);
    }
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

/// Appends `items` as one multi-row insert and enqueues the drain job of every shard the rows
/// fall in, all in one statement, so the wake-ups commit with the rows. The request pays the
/// person lookup ([`known_people`], skipped without identifiers) and that one insert. Returns
/// once the rows are durable.
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

    let people = known_people(
        pool,
        workspace,
        items.iter().map(|item| item.identifiers.as_slice()),
    )
    .await?;
    let affinities: Vec<i16> = items
        .iter()
        .map(|item| item_bucket(item, &people))
        .collect();

    let count = drain_shards();
    let shards: Vec<i32> = affinities
        .iter()
        .map(|bucket| Shard::of_bucket(*bucket, count).index)
        .collect::<BTreeSet<u32>>()
        .into_iter()
        .filter_map(|index| i32::try_from(index).ok())
        .collect();
    // The jobs are built in the statement from the shard indexes, so the request carries a few
    // bytes per shard and not a job row, which at 64 shards would double its size. A shard whose
    // key is held by a ready or running job is left out before the insert, so a busy shard costs
    // a probe of `jobs_unique_key_idx`; `ON CONFLICT` still covers a job that appears in between.
    // `drain_job` is the same job; a test holds the two together.
    let row = sqlx::query(
        "WITH jobs_in AS ( \
             SELECT s AS shard, $9::text || ':' || $1::text || ':' || s || '/' || $3::int AS key \
             FROM unnest($2::int[]) AS s), \
         inserted AS ( \
             INSERT INTO jobs \
                (id, workspace_id, kind, payload, priority, run_at, max_attempts, unique_key, \
                 trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, $9, \
                    jsonb_build_object('workspace_id', $1::uuid, \
                        'shard', jsonb_build_object('index', t.shard, 'count', $3::int)), \
                    0, now(), $8, t.key, $4, $5, $6 \
             FROM jobs_in t \
             WHERE NOT EXISTS (SELECT 1 FROM jobs j WHERE j.unique_key = t.key \
                               AND j.state IN ('ready', 'running')) \
             ON CONFLICT DO NOTHING \
             RETURNING kind), \
         notified AS ( \
             SELECT pg_notify($7, k.kind) FROM (SELECT DISTINCT kind FROM inserted) k \
             WHERE (SELECT true FROM jobs WHERE state = 'ready' AND kind = k.kind \
                    AND run_at <= now() AND run_at > now() - interval '1 second' \
                    LIMIT 1) IS NULL), \
         appended AS ( \
             INSERT INTO ingest_inbox \
                (workspace_id, api_key_id, kind, item, message_id, received_at, trace_id, \
                 affinity) \
             SELECT $1, $10, i.kind, i.item, i.message_id, now(), $4, i.affinity \
             FROM unnest($11::text[], $12::jsonb[], $13::text[], $14::smallint[]) \
                WITH ORDINALITY AS i(kind, item, message_id, affinity, n) \
             ORDER BY i.n \
             RETURNING 1) \
         SELECT (SELECT count(*) FROM appended), (SELECT count(*) FROM notified)",
    )
    .bind(workspace.as_uuid())
    .bind(&shards)
    .bind(i32::try_from(count).unwrap_or(i32::MAX))
    .bind(trace.trace_id.as_uuid())
    .bind(trace.cause.kind.as_str())
    .bind(&trace.cause.id)
    .bind(NOTIFY_CHANNEL)
    .bind(DEFAULT_MAX_ATTEMPTS)
    .bind(DRAIN_JOB_KIND)
    .bind(api_key_id)
    .bind(&kinds)
    .bind(&stored)
    .bind(&message_ids)
    .bind(&affinities)
    .fetch_one(pool)
    .await
    .map_err(StoreError::query)?;
    let inserted: i64 = row.try_get(0).map_err(StoreError::query)?;
    Ok(usize::try_from(inserted).unwrap_or(usize::MAX))
}

/// Enqueues the drain of `shard`. A no-op while that shard's job is ready or running.
pub async fn enqueue_drain_shard<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace: WorkspaceId,
    shard: Shard,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    enqueue(executor, &drain_job(workspace, shard, trace)).await
}

/// A condition on workspace `$1`'s inbox: it holds a row in a bucket shard `index` of count `$2`
/// owns, or a row with no bucket yet (the backfill buckets it, and any shard may own it). A probe
/// of `ingest_inbox_affinity_idx` per bucket, never a scan of the backlog.
fn shard_pending_sql(index: &str) -> String {
    format!(
        "(EXISTS (SELECT 1 FROM generate_series({index}, 63, $2::int) b \
            WHERE EXISTS (SELECT 1 FROM ingest_inbox WHERE workspace_id = $1 AND affinity = b)) \
          OR EXISTS (SELECT 1 FROM ingest_inbox WHERE workspace_id = $1 AND affinity IS NULL))"
    )
}

/// Whether the inbox holds a row `shard` would drain.
pub async fn shard_pending(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
) -> Result<bool, StoreError> {
    if !shard.is_valid() {
        return Ok(false);
    }
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {}",
        shard_pending_sql("$3::int")
    )))
    .bind(workspace.as_uuid())
    .bind(i32::try_from(shard.count).unwrap_or(i32::MAX))
    .bind(i32::try_from(shard.index).unwrap_or(i32::MAX))
    .fetch_one(pool)
    .await
    .map_err(StoreError::query)
}

/// Enqueues the drain of `shard` when its buckets still hold a row, and reports whether it did.
/// A worker calls it once a drain run has completed, so a run that ended on its time budget, or
/// that rows were appended behind, is followed by the next at once instead of at the safety net.
pub async fn enqueue_drain_if_pending(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    if !shard_pending(pool, workspace, shard).await? {
        return Ok(false);
    }
    enqueue_drain_shard(pool, workspace, shard, trace).await
}

/// Enqueues the drain of every shard of `count` whose buckets hold a row, in one statement, and
/// returns how many jobs it inserted (a shard whose job is ready or running is skipped). The
/// safety net and the hand-off of a pre-shard job use it.
pub async fn enqueue_pending_drains(
    pool: &PgPool,
    workspace: WorkspaceId,
    count: u32,
    trace: &TraceContext,
) -> Result<u64, StoreError> {
    let count = count.clamp(1, Shard::MAX_COUNT);
    let pending: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT s FROM generate_series(0, $2::int - 1) s WHERE {}",
        shard_pending_sql("s")
    )))
    .bind(workspace.as_uuid())
    .bind(i32::try_from(count).unwrap_or(i32::MAX))
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let jobs: Vec<NewJob> = pending
        .into_iter()
        .filter_map(|index| u32::try_from(index).ok())
        .map(|index| drain_job(workspace, Shard { index, count }, trace))
        .collect();
    enqueue_many(pool, &jobs).await
}

/// The shard a drain job's payload names. `Ok(None)` is a payload from before the drain was
/// split by shard; `Err` is a shard that names no buckets.
pub fn payload_shard(payload: &Value) -> Result<Option<Shard>, &'static str> {
    let Some(shard) = payload.get("shard") else {
        return Ok(None);
    };
    let field = |name: &str| {
        shard
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    match (field("index"), field("count")) {
        (Some(index), Some(count)) if (Shard { index, count }).is_valid() => {
            Ok(Some(Shard { index, count }))
        }
        _ => Err("the payload names a shard that owns no buckets"),
    }
}

fn drain_job(workspace: WorkspaceId, shard: Shard, trace: &TraceContext) -> NewJob {
    let mut job = NewJob::new(DRAIN_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({
        "workspace_id": workspace.as_uuid(),
        "shard": { "index": shard.index, "count": shard.count },
    });
    job.unique_key = Some(format!(
        "{DRAIN_JOB_KIND}:{}:{}/{}",
        workspace.as_uuid(),
        shard.index,
        shard.count
    ));
    job
}

/// Every workspace's oldest inbox row, `$1` and `$2` being the lowest and highest ids.
const INBOX_HEADS: &str = "SELECT w.id, EXTRACT(EPOCH FROM now() - head.received_at)::float8 \
    FROM workspaces w \
    CROSS JOIN LATERAL ( \
        SELECT i.received_at FROM ingest_inbox i \
        WHERE ROW(i.workspace_id, i.id) >= ROW(w.id, $1::bigint) \
          AND ROW(i.workspace_id, i.id) <= ROW(w.id, $2::bigint) \
        ORDER BY i.workspace_id, i.id LIMIT 1) head";

/// A workspace's oldest inbox row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InboxHead {
    pub workspace_id: WorkspaceId,
    /// How long the row has waited, by the database's clock.
    pub age_seconds: f64,
}

/// How long [`inbox_heads`] may run before PostgreSQL cancels it.
const HEADS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The oldest row (the lowest id) of every workspace that has any: one probe per workspace of
/// `ingest_inbox_workspace_idx`, never a scan of the inbox. Each probe is a row comparison led by
/// `workspace_id` and ordered by `(workspace_id, id)`, which no other index can answer: the
/// primary key's order by `id`, filtered to the workspace, would walk every other workspace's
/// rows to find that an empty one has none (#397).
///
/// The leader reads the heads every tick, so the read runs under a statement timeout of
/// [`HEADS_TIMEOUT`], like the queue depths and the enrollment sample: a slow database or a lock
/// on the inbox fails the read instead of holding the tick.
pub async fn inbox_heads(pool: &PgPool) -> Result<Vec<InboxHead>, StoreError> {
    let mut tx = crate::bounded::begin_bounded_for(pool, HEADS_TIMEOUT).await?;
    let rows: Vec<(Uuid, f64)> = sqlx::query_as(INBOX_HEADS)
        .bind(i64::MIN)
        .bind(i64::MAX)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    tx.commit().await.map_err(StoreError::query)?;
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
    /// Whether the shard may hold more rows: the oldest `limit` rows were all taken.
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
pub(crate) struct InboxRow {
    pub(crate) id: i64,
    pub(crate) api_key_id: Option<Uuid>,
    pub(crate) webhook_id: Option<Uuid>,
    pub(crate) kind: String,
    pub(crate) item: Value,
    pub(crate) message_id: String,
    pub(crate) received_at: OffsetDateTime,
    pub(crate) trace_id: Uuid,
}

impl InboxRow {
    pub(crate) fn trace(&self) -> TraceContext {
        let cause = match (self.webhook_id, self.api_key_id) {
            (Some(hook), _) => Cause::api(hook.to_string()),
            (None, Some(key)) => Cause::api(key.to_string()),
            (None, None) => Cause::api("unknown"),
        };
        TraceContext {
            trace_id: TraceId::from_uuid(self.trace_id),
            cause,
        }
    }
}

enum Mapped {
    Profile(ProfileRecord),
    /// A native contact, which replaces the stored contact's fields.
    Contact(ProfileRecord),
    Event(EventRecord),
    /// A webhook payload, mapped by its hook.
    Hook(HookMapped),
}

/// [`map_row`] that also maps a webhook row with its hook's configuration.
fn map_with(row: &InboxRow, hooks: &HookConfigs) -> Option<Mapped> {
    if row.kind == "webhook" {
        map_hook_row(row, hooks).map(Mapped::Hook)
    } else {
        map_row(row)
    }
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
            source_record_id: None,
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
pub(crate) fn strongest_identifier(identifiers: &[(String, String)]) -> Option<String> {
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

/// Takes up to `limit` of the oldest rows of the buckets `shard` owns, in id order. Step one
/// picks the ids without locking, each bucket through `(workspace_id, affinity, id)`, so the
/// scan is bounded by the owned buckets times `limit` index entries, whatever the backlog. The
/// rows of a bucket among the `limit` oldest overall are a prefix of its queue, so a key's
/// order holds. Step two locks and fetches them, skipping locked rows. A bucket's rows after
/// one that is missing (locked by an erasure, or deleted in between) are left out of the
/// batch, so nothing applies past a skipped row; their locks release at commit and they are
/// picked again. Also returns whether the shard may hold more: step one filled `limit` and
/// something was taken, so a batch that takes nothing ends the loop instead of spinning.
///
/// Step one reads each bucket after its cursor in `cursors` ([`cursors`]), past the entries of the
/// rows earlier batches deleted; a pick that finds nothing above its cursors reads every bucket
/// from its first row instead. The third value is what the take proved, for
/// [`TakeCursors::advance`] once the batch has committed.
async fn take_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    shard: Shard,
    limit: i64,
    cursors: &TakeCursors,
) -> Result<(Vec<InboxRow>, bool, Taken), StoreError> {
    let owned = shard.buckets();
    let start = cursors.start(workspace, &owned);
    let mut after = start.after.clone();
    let mut picked = pick_rows(connection, workspace, &owned, &after, limit).await?;
    if picked.is_empty() && after.iter().any(|after| *after > 0) {
        // Nothing above the cursors, yet the drain ran for this shard: a straggler that
        // committed below a cursor is the likely reason, so walk every bucket from its start.
        after.fill(0);
        picked = pick_rows(connection, workspace, &owned, &after, limit).await?;
    }
    let returned = Instant::now();
    let ids: Vec<i64> = picked.iter().map(|(id, _)| *id).collect();
    let fetched: Vec<InboxRow> = sqlx::query_as(
        "SELECT id, api_key_id, webhook_id, kind, item, message_id, received_at, trace_id          FROM ingest_inbox WHERE workspace_id = $1 AND id = ANY($2)          ORDER BY id FOR UPDATE SKIP LOCKED",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    // The first id of each bucket the batch could not lock.
    let mut blocked: BTreeMap<i16, i64> = BTreeMap::new();
    let mut keep = std::collections::BTreeSet::new();
    for (id, bucket) in &picked {
        if blocked.contains_key(bucket) {
            continue;
        }
        if fetched.iter().any(|row| row.id == *id) {
            keep.insert(*id);
        } else {
            blocked.insert(*bucket, *id);
        }
    }
    let rows: Vec<InboxRow> = fetched
        .into_iter()
        .filter(|row| keep.contains(&row.id))
        .collect();
    let full = i64::try_from(picked.len()).is_ok_and(|n| n >= limit.max(1));
    let more = !rows.is_empty() && full;
    Ok((
        rows,
        more,
        proof(&start, &owned, &after, &picked, &blocked, full, returned),
    ))
}

/// Step one of [`take_rows`]: the ids of up to `limit` of the oldest rows of `buckets` in id
/// order, each bucket read after its entry in `after`, without locking.
async fn pick_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    buckets: &[i16],
    after: &[i64],
    limit: i64,
) -> Result<Vec<(i64, i16)>, StoreError> {
    if buckets.is_empty() {
        return Ok(Vec::new());
    }
    // The buckets are inlined, one branch each: with a bound array the planner estimates
    // every bucket from the column's average frequency, and under a skewed backlog it walks
    // the primary key past the hot bucket's rows to find an empty bucket's none. Each branch's
    // cursor is a parameter of its own, so a shard's statement text stays the same.
    let branches: Vec<String> = buckets
        .iter()
        .enumerate()
        .map(|(n, bucket)| {
            format!(
                "(SELECT id, affinity FROM ingest_inbox WHERE workspace_id = $1 AND affinity = {bucket} \
                 AND id > ${} ORDER BY id LIMIT $2)",
                n + 3
            )
        })
        .collect();
    let sql = format!(
        "SELECT id, affinity FROM ({}) AS oldest ORDER BY id LIMIT $2",
        branches.join(" UNION ALL ")
    );
    let mut query = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(limit.max(1));
    for after in after {
        query = query.bind(*after);
    }
    query.fetch_all(connection).await.map_err(StoreError::query)
}

/// What a take proved about each bucket it read: every row visible to its pick up to the first
/// one it could not lock was taken, and, when the pick came back short of the limit, everything
/// after that too; a full pick proves it up to its newest id only.
fn proof(
    start: &TakeStart,
    buckets: &[i16],
    after: &[i64],
    picked: &[(i64, i16)],
    blocked: &BTreeMap<i16, i64>,
    full: bool,
    returned: Instant,
) -> Taken {
    let newest = picked.last().map(|(id, _)| *id);
    Taken {
        cap: start.cap,
        returned,
        newest,
        buckets: buckets
            .iter()
            .zip(after)
            .map(|(bucket, after)| {
                let through = match blocked.get(bucket) {
                    Some(first) => Some(first - 1),
                    None if full => newest,
                    None => None,
                };
                (*bucket, *after, through)
            })
            .collect(),
    }
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

/// [`apply_contact`] as one record of the drain's batch, which recomputes its traits and appends
/// its decisions when it finishes ([`EventBatch::finish`]).
async fn apply_contact_into(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    record: &ProfileRecord,
    events: &mut EventBatch,
) -> Result<ProfileOutcome, StoreError> {
    let mut record = record.clone();
    replace_stored_fields(connection, workspace, &mut record).await?;
    apply_profile_record_into(
        connection,
        workspace,
        &record,
        &mut events.profiles,
        &mut events.decisions,
    )
    .await
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

/// Leaves a reject for the traits an applied identify dropped (keys and codes only), so the
/// reason can be read while the rest of the record stays applied.
async fn record_dropped_traits(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    row: &InboxRow,
    result: &ProfileOutcome,
) -> Result<(), StoreError> {
    let (ProfileOutcome::Applied {
        person_id, dropped, ..
    }
    | ProfileOutcome::Unchanged { person_id, dropped }) = result
    else {
        return Ok(());
    };
    if dropped.is_empty() {
        return Ok(());
    }
    let dropped: Vec<(String, &'static str)> = dropped
        .iter()
        .map(|dropped| (dropped.key.clone(), dropped.reason))
        .collect();
    insert_dropped_traits_reject(
        connection,
        workspace,
        &DroppedTraits {
            source: SOURCE,
            trace: &row.trace(),
            received_at: row.received_at,
            message_id: &row.message_id,
            person_id: *person_id,
            dropped: &dropped,
        },
    )
    .await
}

fn tally_profile(outcome: &mut DrainOutcome, kind: &'static str, result: &ProfileOutcome) {
    let label = match result {
        ProfileOutcome::Applied { .. } => "valid",
        ProfileOutcome::Unchanged { .. } => "duplicate",
        ProfileOutcome::Rejected { .. } => "rejected",
    };
    outcome.count(kind, label);
}

/// Tallies a hook record under kind `webhook`: its profile as `applied`, `unchanged` or
/// `rejected`, and its event, when it had one, as `accepted`, `duplicate` or `rejected`.
fn tally_hook(outcome: &mut DrainOutcome, result: &HookResult) {
    outcome.count(
        "webhook",
        match result.profile {
            ProfileOutcome::Applied { .. } => "applied",
            ProfileOutcome::Unchanged { .. } => "unchanged",
            ProfileOutcome::Rejected { .. } => "rejected",
        },
    );
    match &result.event {
        Some(EventOutcome::Accepted { flags, .. }) => {
            if flags.contains(&zirv_flow_core::events::EventFlag::Late) {
                outcome.late += 1;
            }
            outcome.count("webhook", "accepted");
        }
        Some(EventOutcome::Duplicate { .. }) => outcome.count("webhook", "duplicate"),
        Some(EventOutcome::Rejected { .. }) => outcome.count("webhook", "rejected"),
        None => {}
    }
}

/// Keeps a payload the hook's mapping refused as a reject of the hook's stream, with codes and
/// a masked sample.
async fn reject_hook(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    row: &InboxRow,
    rejected: &crate::webhooks::HookRejected,
) -> Result<(), StoreError> {
    insert_record_rejects(
        connection,
        workspace,
        &rejected.source,
        &rejected.stream,
        &row.trace(),
        None,
        &[Refusal {
            id: Uuid::now_v7(),
            reasons: rejected.reasons.clone(),
            sample: rejected.sample.clone(),
        }],
    )
    .await
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
    drain_batch_using(pool, workspace, shard, key, limit, &TAKE_CURSORS).await
}

/// [`drain_batch`] with the take cursors in `cursors`, which it moves on once the batch commits.
pub(crate) async fn drain_batch_using(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
    key: &HmacKey,
    limit: i64,
    cursors: &TakeCursors,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    crate::changes::cache_change_readers(&mut tx, workspace).await?;
    let (rows, more, taken) = take_rows(&mut tx, workspace, shard, limit, cursors).await?;
    let hooks = HookConfigs::load(&mut tx, workspace, &rows).await?;
    lock_contact_rows(&mut tx, workspace, &rows).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        more,
        ..DrainOutcome::default()
    };
    let mut events = EventBatch::default();
    events.readers_cached(workspace);
    // The shared slot lock once for the batch, after the contact locks and before any person's:
    // the batch's records and events then share what they hold and read, and its profile records
    // recompute their traits together when the batch finishes.
    events.profiles.guard(&mut tx, workspace).await?;
    let mut lists = ListCounts::default();
    let mapped: Vec<Mapped> = rows
        .iter()
        .map(|row| map_with(row, &hooks).ok_or_else(StoreError::poison))
        .collect::<Result<_, _>>()?;
    // One statement looks up every `messageId` of the batch, instead of one probe per event.
    events
        .prefetch_duplicates(
            &mut tx,
            workspace,
            mapped.iter().filter_map(|mapped| match mapped {
                Mapped::Event(record) => Some(record),
                Mapped::Hook(HookMapped::Record(record)) => record.event.as_ref(),
                _ => None,
            }),
        )
        .await?;
    for (row, mapped) in rows.iter().zip(mapped) {
        match mapped {
            Mapped::Profile(record) => {
                let result = apply_profile_record_into(
                    &mut tx,
                    workspace,
                    &record,
                    &mut events.profiles,
                    &mut events.decisions,
                )
                .await?;
                record_dropped_traits(&mut tx, workspace, row, &result).await?;
                tally_profile(&mut outcome, "identify", &result);
            }
            Mapped::Contact(record) => {
                let result = apply_contact_into(&mut tx, workspace, &record, &mut events).await?;
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
            Mapped::Hook(HookMapped::Record(record)) => {
                let result =
                    apply_hook_record(&mut tx, workspace, key, &mut events, &mut lists, &record)
                        .await?;
                tally_hook(&mut outcome, &result);
            }
            Mapped::Hook(HookMapped::Rejected(rejected)) => {
                reject_hook(&mut tx, workspace, row, &rejected).await?;
                outcome.count("webhook", "rejected");
            }
        }
    }
    events.finish(&mut tx, workspace).await?;
    delete_rows(&mut tx, workspace, &rows).await?;
    crate::changes::request_reevaluation(&mut tx, workspace).await?;
    // Last, so the rows of the lists the batch added to are locked for the commit alone.
    lists.apply(&mut tx).await?;
    commit(tx).await?;
    cursors.advance(workspace, &taken);
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
    drain_batch_singly_using(pool, workspace, shard, key, limit, &TAKE_CURSORS).await
}

/// [`drain_batch_singly`] with the take cursors in `cursors`.
pub(crate) async fn drain_batch_singly_using(
    pool: &PgPool,
    workspace: WorkspaceId,
    shard: Shard,
    key: &HmacKey,
    limit: i64,
    cursors: &TakeCursors,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    crate::changes::cache_change_readers(&mut tx, workspace).await?;
    let (rows, more, taken) = take_rows(&mut tx, workspace, shard, limit, cursors).await?;
    let hooks = HookConfigs::load(&mut tx, workspace, &rows).await?;
    lock_contact_rows(&mut tx, workspace, &rows).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        more,
        ..DrainOutcome::default()
    };
    let mut lists = ListCounts::default();
    for row in &rows {
        let mapped = map_with(row, &hooks);
        let mark = lists.mark();
        let mut savepoint = tx.begin().await.map_err(StoreError::query)?;
        let mut unstorable = Map::new();
        let applied = match &mapped {
            Some(Mapped::Profile(record)) => {
                match apply_profile_record(&mut savepoint, workspace, record).await {
                    Ok(result) => {
                        tally_profile(&mut outcome, "identify", &result);
                        record_dropped_traits(&mut savepoint, workspace, row, &result).await
                    }
                    Err(error) => Err(error),
                }
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
            Some(Mapped::Hook(HookMapped::Record(record))) => {
                let mut events = EventBatch::default();
                match apply_hook_record(
                    &mut savepoint,
                    workspace,
                    key,
                    &mut events,
                    &mut lists,
                    record,
                )
                .await
                {
                    Ok(result) => {
                        tally_hook(&mut outcome, &result);
                        events.finish(&mut savepoint, workspace).await
                    }
                    Err(error) => Err(error),
                }
            }
            Some(Mapped::Hook(HookMapped::Rejected(rejected))) => {
                let done = reject_hook(&mut savepoint, workspace, row, rejected).await;
                if done.is_ok() {
                    outcome.count("webhook", "rejected");
                }
                done
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
                lists.discard_after(mark);
                if let Some(Mapped::Contact(record)) = &mapped {
                    let mut rejected = record.clone();
                    rejected.traits = unstorable;
                    reject_row(
                        &mut tx,
                        workspace,
                        key,
                        row,
                        Some(&Mapped::Contact(rejected)),
                        &hooks,
                    )
                    .await?;
                }
                outcome.count("contact", "rejected");
            }
            Err(error) if error.is_retryable() => return Err(error),
            Err(_) => {
                savepoint.rollback().await.map_err(StoreError::query)?;
                lists.discard_after(mark);
                reject_row(&mut tx, workspace, key, row, mapped.as_ref(), &hooks).await?;
                let kind = if row.kind == "webhook" {
                    "webhook"
                } else if row.kind == "contact" {
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
    crate::changes::request_reevaluation(&mut tx, workspace).await?;
    lists.apply(&mut tx).await?;
    commit(tx).await?;
    cursors.advance(workspace, &taken);
    Ok(outcome)
}

async fn reject_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &HmacKey,
    row: &InboxRow,
    mapped: Option<&Mapped>,
    hooks: &HookConfigs,
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
        Some(Mapped::Hook(HookMapped::Record(record))) => FailedItem {
            kind: "record",
            source: &record.profile.source,
            name: None,
            values: &record.profile.traits,
            timestamp: Some(record.profile.observed_at),
            received_at: row.received_at,
            message_id: Some(&row.message_id),
            trace: &trace,
        },
        Some(Mapped::Hook(HookMapped::Rejected(rejected))) => FailedItem {
            kind: "record",
            source: &rejected.source,
            name: None,
            values: &empty,
            timestamp: None,
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
            source: row
                .webhook_id
                .and_then(|hook| hooks.origin(hook))
                .map_or(SOURCE, |(source, _)| source),
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
    use crate::decision_testing::PERSONAL_DATA;
    use crate::webhooks::fixtures::{affinities, person_with};

    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{CauseKind, HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceName};

    use super::*;
    use crate::{create_workspace, list_trace};

    fn key() -> HmacKey {
        HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction)
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
            api_key_id: Some(Uuid::nil()),
            webhook_id: None,
            kind: kind.to_owned(),
            item,
            message_id: "m-1".to_owned(),
            received_at: datetime!(2026-02-03 04:05:06 UTC),
            trace_id: Uuid::nil(),
        }
    }

    #[test]
    fn a_webhook_cause_is_its_uuid_and_keeps_the_inbox_trace() {
        let mut inbox = row("webhook", json!({}));
        let hook = Uuid::now_v7();
        inbox.webhook_id = Some(hook);
        inbox.trace_id = Uuid::now_v7();

        let trace = inbox.trace();

        assert_eq!(trace.trace_id.as_uuid(), inbox.trace_id);
        assert_eq!(trace.cause, Cause::api(hook.to_string()));
        assert_eq!(Uuid::parse_str(&trace.cause.id).unwrap(), hook);
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
    async fn an_append_and_its_drain_wake_up_are_one_client_statement(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            sqlx::raw_sql(
                "CREATE TABLE statement_log (client_query text NOT NULL); \
                 CREATE FUNCTION log_statement() RETURNS trigger LANGUAGE plpgsql AS \
                   $$ BEGIN INSERT INTO statement_log VALUES (current_query()); RETURN NULL; END $$; \
                 CREATE TRIGGER log_inbox AFTER INSERT ON ingest_inbox \
                   FOR EACH STATEMENT EXECUTE FUNCTION log_statement(); \
                 CREATE TRIGGER log_jobs AFTER INSERT ON jobs \
                   FOR EACH STATEMENT EXECUTE FUNCTION log_statement();",
            )
            .execute(&pool)
            .await
            .unwrap();

            let trace = TraceContext::root(Cause::api("req-1"));
            append(&pool, ws, &trace, &[identify("u1", "m-1", json!({}))]).await;

            let (writes, distinct): (i64, i64) =
                sqlx::query_as("SELECT count(*), count(DISTINCT client_query) FROM statement_log")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                (writes, distinct),
                (2, 1),
                "the inbox and the job in one statement"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_empty_append_enqueues_no_drain(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req-1"));
            append(&pool, ws, &trace, &[]).await;
            assert_eq!(inbox_count(&pool).await, 0);
            let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = $1")
                .bind(DRAIN_JOB_KIND)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(jobs, 0, "no row, no shard to drain");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_writes_every_row_with_the_trace_and_enqueues_its_shards_once(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
            let shards: Vec<i32> =
                sqlx::query_scalar("SELECT DISTINCT affinity % 8 FROM ingest_inbox ORDER BY 1")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let jobs: Vec<(String, Option<String>)> =
                sqlx::query_as("SELECT kind, unique_key FROM jobs ORDER BY unique_key")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let expected: Vec<(String, Option<String>)> = shards
                .iter()
                .map(|index| {
                    (
                        DRAIN_JOB_KIND.to_owned(),
                        Some(format!("ingest.drain:{}:{index}/8", ws.as_uuid())),
                    )
                })
                .collect();
            assert_eq!(jobs, expected, "one job per shard the rows fall in, once");
        })
        .await;
    }

    /// A drain is queued at the default priority, in age order with every other job, so it waits
    /// behind older due wakes like any job (supervisor ruling 98e6546e): a band of its own ahead of
    /// the wakes let more backlogged workspaces than slots, each draining for up to 20 s, starve
    /// every other kind.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_next_drain_is_enqueued_only_while_the_shard_holds_a_row(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req-1"));
            let jobs = || async {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
                    .bind(DRAIN_JOB_KIND)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            };
            let shard = Shard::of_bucket(
                item_bucket(&identify("u1", "m-1", json!({})), &KnownPeople::new()),
                8,
            );
            let other = Shard {
                index: (shard.index + 1) % 8,
                count: 8,
            };
            assert!(
                !enqueue_drain_if_pending(&pool, ws, shard, &trace)
                    .await
                    .unwrap(),
                "an empty inbox needs no drain"
            );
            assert_eq!(jobs().await, 0);

            append_inbox(
                &pool,
                ws,
                Uuid::now_v7(),
                &trace,
                &[identify("u1", "m-1", json!({"plan": "free"}))],
            )
            .await
            .unwrap();
            assert_eq!(jobs().await, 1, "the append enqueued a drain");
            assert!(
                !enqueue_drain_if_pending(&pool, ws, shard, &trace)
                    .await
                    .unwrap(),
                "a drain that is ready holds the key"
            );
            assert!(
                !enqueue_drain_if_pending(&pool, ws, other, &trace)
                    .await
                    .unwrap(),
                "another shard's buckets hold no row"
            );

            sqlx::query("DELETE FROM jobs WHERE kind = $1")
                .bind(DRAIN_JOB_KIND)
                .execute(&pool)
                .await
                .unwrap();
            assert!(
                enqueue_drain_if_pending(&pool, ws, shard, &trace)
                    .await
                    .unwrap(),
                "a completed run that left a row is followed by the next drain"
            );
            assert_eq!(jobs().await, 1);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_enqueued_behind_due_wakes_waits_its_turn(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            const STEP: &str = "flow.step";
            let lease = std::time::Duration::from_secs(60);
            let ws = workspace(&pool, "ours").await;
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, run_at, unique_key, trace_id, \
                               cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, $2, jsonb_build_object('n', g), \
                    now() - interval '1 hour' + make_interval(secs => g), \
                    'wake:' || g, gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, 1000) g",
            )
            .bind(ws.as_uuid())
            .bind(STEP)
            .execute(&pool)
            .await
            .unwrap();
            assert!(
                enqueue_drain_shard(
                    &pool,
                    ws,
                    Shard::ALL,
                    &TraceContext::root(Cause::api("req-1"))
                )
                .await
                .unwrap()
            );
            let priority: i16 = sqlx::query_scalar("SELECT priority FROM jobs WHERE kind = $1")
                .bind(DRAIN_JOB_KIND)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(priority, 0);

            let batch = crate::claim_jobs(&pool, "w", &[STEP, DRAIN_JOB_KIND], lease, None, 32)
                .await
                .unwrap();
            assert_eq!(
                batch
                    .iter()
                    .map(|job| job.payload["n"].as_i64())
                    .collect::<Vec<_>>(),
                (1..=32).map(Some).collect::<Vec<_>>(),
                "the oldest wakes, not the younger drain"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_applies_in_order_deletes_the_rows_and_records_the_request_trace(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
            assert!(!actions.contains(&"event.accepted"), "{actions:?}");
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
        })
        .await;
    }

    async fn dirty_and_reevaluation_count(pool: &PgPool, ws: WorkspaceId) -> (i64, i64) {
        sqlx::query_as(
            "SELECT (SELECT count(*) FROM person_changes WHERE workspace_id = $1), \
                    (SELECT count(*) FROM jobs WHERE workspace_id = $1 AND kind = 'segment.reevaluate')",
        )
        .bind(ws.as_uuid())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_without_active_segments_writes_no_marks_or_reevaluation_job(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            for singly in [false, true] {
                let ws = workspace(&pool, if singly { "singly" } else { "batch" }).await;
                append(
                    &pool,
                    ws,
                    &TraceContext::root(Cause::api("req")),
                    &[
                        identify("u1", "identify-1", json!({"plan": "pro"})),
                        track("u1", "track-1", "seen", json!({})),
                    ],
                )
                .await;

                let outcome = if singly {
                    drain_batch_singly(&pool, ws, Shard::ALL, &key(), 500).await
                } else {
                    drain_batch(&pool, ws, Shard::ALL, &key(), 500).await
                }
                .unwrap();

                assert_eq!(outcome.rows, 2);
                assert_eq!(outcome.applied[&("identify", "valid")], 1);
                assert_eq!(outcome.applied[&("track", "unplanned")], 1);
                assert_eq!(dirty_and_reevaluation_count(&pool, ws).await, (0, 0));
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_with_matching_segment_classes_keeps_marks_and_reevaluation(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            use zirv_flow_core::segment::ChangeClasses;

            let classes =
                ChangeClasses::TRAITS | ChangeClasses::EVENTS | ChangeClasses::IDENTIFIERS;
            for singly in [false, true] {
                let ws = workspace(&pool, if singly { "singly" } else { "batch" }).await;
                crate::segments::tests_support::segment_reading(&pool, ws, classes).await;
                append(
                    &pool,
                    ws,
                    &TraceContext::root(Cause::api("req")),
                    &[
                        identify("u1", "identify-1", json!({"plan": "pro"})),
                        track("u1", "track-1", "seen", json!({})),
                    ],
                )
                .await;

                let outcome = if singly {
                    drain_batch_singly(&pool, ws, Shard::ALL, &key(), 500).await
                } else {
                    drain_batch(&pool, ws, Shard::ALL, &key(), 500).await
                }
                .unwrap();

                assert_eq!(outcome.rows, 2);
                assert_eq!(dirty_and_reevaluation_count(&pool, ws).await, (1, 1));
                let marked: i16 = sqlx::query_scalar(
                    "SELECT classes FROM person_changes WHERE workspace_id = $1",
                )
                .bind(ws.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(marked, i16::from(classes.bits()));
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_ignores_archived_other_workspace_and_unrelated_segment_readers(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            use crate::segments::tests_support::segment_reading;
            use zirv_flow_core::segment::ChangeClasses;

            let ws = workspace(&pool, "ours").await;
            let other = workspace(&pool, "other").await;
            segment_reading(&pool, other, ChangeClasses::EVENTS).await;
            segment_reading(&pool, ws, ChangeClasses::TRAITS).await;
            let archived = segment_reading(&pool, ws, ChangeClasses::EVENTS).await;
            let mut tx = begin(&pool).await.unwrap();
            crate::lock_segments(&mut tx, ws).await.unwrap();
            crate::archive_segment(&mut tx, ws, archived.id)
                .await
                .unwrap();
            commit(tx).await.unwrap();
            append(
                &pool,
                ws,
                &TraceContext::root(Cause::api("req")),
                &[track("u1", "track-1", "seen", json!({}))],
            )
            .await;

            let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            assert_eq!(outcome.applied[&("track", "unplanned")], 1);
            assert_eq!(dirty_and_reevaluation_count(&pool, ws).await, (0, 0));
        })
        .await;
    }

    async fn wal_lsn(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT pg_current_wal_lsn()::text")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn report_ingest_wal(
        pool: &PgPool,
        phase: &str,
        start: &str,
        end: &str,
        inspect: bool,
    ) -> i64 {
        let delta: i64 = sqlx::query_scalar("SELECT ($2::pg_lsn - $1::pg_lsn)::bigint")
            .bind(start)
            .bind(end)
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(delta > 0, "{phase} must write WAL");
        println!("\n{phase}: {start}..{end}; cluster LSN delta = {delta} bytes");
        if !inspect {
            println!(
                "  fallback: statement-group LSN bytes = {delta}; relation attribution unavailable"
            );
            return delta;
        }

        // Several block references can belong to one record. Attribute its complete length
        // once, to the distinct relation set; keep records without blocks (e.g. commits) too.
        let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            r#"
            WITH refs AS (
                SELECT DISTINCT b.start_lsn,
                    CASE WHEN b.reldatabase = d.oid THEN
                        COALESCE(pg_filenode_relation(b.reltablespace, b.relfilenode)::text,
                                 format('unmapped %s/%s', b.reltablespace, b.relfilenode))
                    ELSE format('other database %s: %s/%s',
                                b.reldatabase, b.reltablespace, b.relfilenode)
                    END AS relation
                FROM pg_get_wal_block_info($1::pg_lsn, $2::pg_lsn, false) b
                CROSS JOIN pg_database d
                WHERE d.datname = current_database()
            ), relations AS (
                SELECT start_lsn, string_agg(relation, ' + ' ORDER BY relation) AS relation
                FROM refs GROUP BY start_lsn
            )
            SELECT COALESCE(r.relation, 'no relation: ' || w.resource_manager),
                   count(*), sum(w.record_length)::bigint, sum(w.fpi_length)::bigint
            FROM pg_get_wal_records_info($1::pg_lsn, $2::pg_lsn) w
            LEFT JOIN relations r USING (start_lsn)
            GROUP BY 1 ORDER BY 1
            "#,
        )
        .bind(start)
        .bind(end)
        .fetch_all(pool)
        .await
        .unwrap();
        println!(
            "  {:<60} {:>8} {:>12} {:>12}",
            "relation(s) / resource", "records", "WAL bytes", "FPI bytes"
        );
        for (relation, records, bytes, fpi) in &rows {
            println!("  {relation:<60} {records:>8} {bytes:>12} {fpi:>12}");
        }
        let total: i64 = rows.iter().map(|row| row.2).sum();
        let fpi: i64 = rows.iter().map(|row| row.3).sum();
        // Reconcile against the independent resource-manager totals, including non-relation WAL.
        let expected: i64 = sqlx::query_scalar(
            "SELECT sum(combined_size)::bigint FROM pg_get_wal_stats($1::pg_lsn, $2::pg_lsn)",
        )
        .bind(start)
        .bind(end)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(total, expected, "every WAL record must be counted once");
        assert!(total > 0);
        println!(
            "  total = {total} record bytes (includes {fpi} FPI bytes; excludes WAL page headers/padding)"
        );
        total
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_batch_reports_wal_by_relation_without_active_segments(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "wal-measurement").await;
            let (user, superuser): (String, bool) = sqlx::query_as(
                "SELECT rolname::text, rolsuper FROM pg_roles WHERE rolname = current_user",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            let inspect = match sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_walinspect")
                .execute(&pool)
                .await
            {
                Ok(_) => sqlx::query_scalar::<_, bool>(
                    "SELECT has_function_privilege('pg_get_wal_records_info(pg_lsn,pg_lsn)', 'EXECUTE') \
                     AND has_function_privilege('pg_get_wal_block_info(pg_lsn,pg_lsn,boolean)', 'EXECUTE') \
                     AND has_function_privilege('pg_get_wal_stats(pg_lsn,pg_lsn,boolean)', 'EXECUTE')",
                )
                .fetch_one(&pool)
                .await
                .unwrap(),
                Err(error)
                    if matches!(
                        error.as_database_error().and_then(|error| error.code()).as_deref(),
                        Some("42501" | "0A000" | "58P01")
                    ) =>
                {
                    println!("pg_walinspect unavailable; using statement-group LSN deltas: {error}");
                    false
                }
                Err(error) => panic!("create pg_walinspect: {error}"),
            };
            println!("ingest WAL fixture: user={user}, superuser={superuser}, pg_walinspect={inspect}");

            // Seed directly so changes to ingest's marking policy cannot change the baseline.
            // Both batches use these same 500 existing people; no segments or workers run.
            let people: Vec<Uuid> = (1..=500).map(Uuid::from_u128).collect();
            let users: Vec<String> = (1..=500).map(|n| format!("wal-{n:04}")).collect();
            sqlx::query(
                "INSERT INTO people (id, workspace_id) SELECT id, $1 FROM unnest($2::uuid[]) p(id)",
            )
            .bind(ws.as_uuid())
            .bind(&people)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
                 SELECT id, $1, id, 'external:user_id', value \
                 FROM unnest($2::uuid[], $3::text[]) p(id, value)",
            )
            .bind(ws.as_uuid())
            .bind(&people)
            .bind(&users)
            .execute(&pool)
            .await
            .unwrap();
            let segments: i64 = sqlx::query_scalar("SELECT count(*) FROM segments")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(segments, 0);

            let tracks: Vec<_> = users.iter().map(|user| {
                track(user, &format!("track-{user}"), "seen", json!({"n": 1}))
            }).collect();
            let identifies: Vec<_> = users.iter().map(|user| {
                identify(user, &format!("identify-{user}"), json!({"plan": "pro"}))
            }).collect();
            let trace = TraceContext {
                trace_id: TraceId::from_uuid(Uuid::from_u128(501)),
                cause: Cause::api(Uuid::from_u128(502).to_string()),
            };
            let key = key();
            // CI connects as postgres. Start after a checkpoint so FPI eligibility is repeatable.
            if superuser {
                sqlx::query("CHECKPOINT").execute(&pool).await.unwrap();
            }
            println!("500 track + 500 identify items; append + drain only; checkpoint={superuser}");
            let mut workload_bytes = 0;
            for (kind, items, outcome) in [
                ("track", tracks, "unplanned"),
                ("identify", identifies, "valid"),
            ] {
                let start = wal_lsn(&pool).await;
                let appended = append_inbox(&pool, ws, Uuid::from_u128(502), &trace, &items)
                    .await
                    .unwrap();
                let appended_lsn = wal_lsn(&pool).await;
                let drained = drain_batch(&pool, ws, Shard::ALL, &key, 500).await.unwrap();
                let drained_lsn = wal_lsn(&pool).await;
                assert_eq!(appended, 500);
                assert_eq!(drained.rows, 500);
                assert_eq!(drained.applied[&(kind, outcome)], 500);
                assert_eq!(inbox_count(&pool).await, 0);

                let append_bytes = report_ingest_wal(
                    &pool, &format!("{kind} append"), &start, &appended_lsn, inspect,
                ).await;
                let drain_bytes = report_ingest_wal(
                    &pool, &format!("{kind} drain"), &appended_lsn, &drained_lsn, inspect,
                ).await;
                let bytes = append_bytes + drain_bytes;
                let unit = if kind == "track" { "event" } else { "identify" };
                println!("{kind}: {bytes} total WAL bytes / 500 = {:.2} bytes/{unit}", bytes as f64 / 500.0);
                workload_bytes += bytes;
            }
            println!("workload: {workload_bytes} total WAL bytes / 1000 = {:.2} bytes/input item", workload_bytes as f64 / 1000.0);
            let people: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(people, 500, "both batches must resolve the existing people");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_batch_appends_its_decisions_in_one_statement_in_the_order_made(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req-1"));
            append(
                &pool,
                ws,
                &trace,
                &[
                    track("a", "m-1", "seen", json!({})),
                    identify("b", "m-2", json!({"plan": "pro"})),
                    track("a", "m-3", "seen", json!({})),
                    track("c", "m-4", "seen", json!({})),
                    track("a", "m-3", "seen", json!({})),
                ],
            )
            .await;

            let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            assert_eq!(outcome.rows, 5);
            let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 50).await.unwrap();
            let made: Vec<(&str, &str)> = decisions
                .iter()
                .map(|d| (d.action.as_str(), d.outcome.as_str()))
                .collect();
            assert_eq!(
                made,
                [
                    ("person.created", "created"),
                    ("person.created", "created"),
                    ("trait.change", "changed"),
                    ("person.created", "created"),
                ]
            );
            let keys: Vec<Vec<String>> = decisions
                .iter()
                .map(|d| {
                    let mut keys: Vec<String> =
                        d.subject_ids.as_object().unwrap().keys().cloned().collect();
                    keys.sort();
                    keys
                })
                .collect();
            assert_eq!(keys[0], ["person"]);
            assert_eq!(keys[1], ["person", "source_record"]);
            assert_eq!(keys[2], ["person", "source_record"]);
            assert_eq!(keys[3], ["person"]);
            assert!(decisions.iter().all(|d| d.cause.kind == CauseKind::Api));
            assert!(
                decisions
                    .iter()
                    .all(|d| d.job_id.is_none() && d.attempt.is_none())
            );
            // Rows written by one statement share a command id; one drain batch, one statement.
            let statements: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT cmin::text) FROM decision_records WHERE workspace_id = $1",
            )
            .bind(ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(statements, 1);
        })
        .await;
    }

    /// 100 known people, then 500 track items: 190 for known people, 300 for new ones and 10 with
    /// a reserved event name. Returns the workspace and the decision counts the drain added, as
    /// `(action, count)`, and the WAL bytes it wrote.
    async fn drain_five_hundred_track_items(
        pool: &PgPool,
    ) -> (WorkspaceId, Vec<(String, i64)>, i64) {
        let ws = workspace(pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        let known: Vec<IngestItem> = (0..100)
            .map(|n| identify(&format!("k{n}"), &format!("i-{n}"), json!({"plan": "free"})))
            .collect();
        append(pool, ws, &trace, &known).await;
        drain_batch(pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();

        let items: Vec<IngestItem> = (0..500)
            .map(|n| match n {
                0..190 => track(
                    &format!("k{}", n % 100),
                    &format!("t-{n}"),
                    "seen",
                    json!({}),
                ),
                190..490 => track(&format!("n{n}"), &format!("t-{n}"), "seen", json!({})),
                _ => track(
                    &format!("bad{n}"),
                    &format!("t-{n}"),
                    "segment.entered",
                    json!({}),
                ),
            })
            .collect();
        append(pool, ws, &trace, &items).await;
        let before: i64 =
            sqlx::query_scalar("SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint")
                .fetch_one(pool)
                .await
                .unwrap();
        let counted = |pool: &PgPool| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (String, i64)>(
                    "SELECT action, count(*) FROM decision_records GROUP BY action ORDER BY action",
                )
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        let earlier = counted(pool).await;

        let outcome = drain_batch(pool, ws, Shard::ALL, &key(), 500)
            .await
            .unwrap();
        let after: i64 =
            sqlx::query_scalar("SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint")
                .fetch_one(pool)
                .await
                .unwrap();

        assert_eq!(outcome.rows, 500);
        let mut added = counted(pool).await;
        for (action, count) in &mut added {
            *count -= earlier
                .iter()
                .find(|(name, _)| name == action)
                .map_or(0, |(_, count)| *count);
        }
        added.retain(|(_, count)| *count > 0);
        (ws, added, after - before)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_of_track_items_writes_no_accepted_decision_only_new_people_and_rejects(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let (ws, added, wal_bytes) = drain_five_hundred_track_items(&pool).await;

            eprintln!("drain of 500 track items: {wal_bytes} WAL bytes, decisions {added:?}");
            assert_eq!(
                added,
                [
                    ("event.rejected".to_owned(), 10),
                    ("person.created".to_owned(), 300)
                ]
            );
            let events: i64 =
                sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
                    .bind(ws.as_uuid())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(events, 490, "every valid item is stored");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_track_drain_writes_decision_rows_only_for_new_people_and_rejects_and_the_dropped_rows_cost_wal(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
        let (ws, added, wal_bytes) = drain_five_hundred_track_items(&pool).await;
        let rows: i64 = added.iter().map(|(_, count)| count).sum();

        // What the removed decisions would have cost: 490 rows of the shape earlier versions
        // wrote, inserted in one statement and rolled back. This is a measured price of the
        // dropped rows, not a before and after run; the drain's own WAL is only printed, because
        // the log position is shared with every other test on the server.
        let mut tx = pool.begin().await.unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (ANALYZE, WAL, FORMAT JSON) \
             INSERT INTO decision_records \
                (id, workspace_id, trace_id, action, outcome, cause_kind, cause_id, subject_ids) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), 'event.accepted', 'unplanned', 'api', \
                    'x', jsonb_build_object('event', gen_random_uuid(), 'person', gen_random_uuid()) \
             FROM generate_series(1, 490)",
        )
        .bind(ws.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        let removed_bytes = plan[0]["Plan"]["WAL Bytes"].as_i64().unwrap();

        eprintln!(
            "drain WAL {wal_bytes} bytes in {rows} decision rows; 490 accepted rows: {removed_bytes} bytes"
        );
        assert_eq!(
            rows, 310,
            "the new people and the rejects, 490 fewer than before"
        );
        assert!(
            removed_bytes > 0,
            "the dropped decisions were a real share of the log"
        );
        })
        .await;
    }

    /// The benchmark script's rows as `(kind, items, p50_ms, p99_ms)`.
    async fn applied_latency(pool: &PgPool) -> Vec<(String, i64, Option<f64>, Option<f64>)> {
        let script = include_str!("../../../scripts/bench/applied-latency.sql");
        let statement = format!(
            "SELECT kind, items, p50_ms::float8, p99_ms::float8 FROM ({}) s ORDER BY kind",
            script.trim().trim_end_matches(';')
        );
        sqlx::query_as(sqlx::AssertSqlSafe(statement))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_applied_latency_script_measures_inbox_arrival_to_the_decision(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req-1"));

            // Before anything is applied both kinds are listed with no items.
            let empty = applied_latency(&pool).await;
            assert_eq!(
                empty,
                [
                    ("identify".to_owned(), 0, None, None),
                    ("track".to_owned(), 0, None, None)
                ]
            );

            // First drain: both rows arrived 5 s before they were applied.
            append(
                &pool,
                ws,
                &trace,
                &[
                    identify("u1", "m-1", json!({"plan": "free"})),
                    track("u1", "m-2", "seen", json!({})),
                ],
            )
            .await;
            sqlx::query("UPDATE ingest_inbox SET received_at = now() - interval '5 seconds'")
                .execute(&pool)
                .await
                .unwrap();
            drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            let rows = applied_latency(&pool).await;
            for (kind, items, p50, p99) in &rows {
                let (p50, p99) = (p50.unwrap(), p99.unwrap());
                assert_eq!(*items, 1, "{kind}");
                assert!((5000.0..30_000.0).contains(&p50), "{kind} p50 {p50}");
                assert!(p99 >= p50, "{kind}");
            }

            // A later identify of the same person changes the trait again; only that latest change
            // counts for the record, so there is still one identify item and it is the newer 2 s one.
            append(
                &pool,
                ws,
                &trace,
                &[identify("u1", "m-3", json!({"plan": "pro"}))],
            )
            .await;
            sqlx::query("UPDATE ingest_inbox SET received_at = now() - interval '2 seconds'")
                .execute(&pool)
                .await
                .unwrap();
            drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            let rows = applied_latency(&pool).await;
            let (kind, items, p50, _) = &rows[0];
            assert_eq!((kind.as_str(), *items), ("identify", 1));
            let p50 = p50.unwrap();
            assert!((2000.0..5000.0).contains(&p50), "identify p50 {p50}");
            assert_eq!((rows[1].0.as_str(), rows[1].1), ("track", 1));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_takes_at_most_the_limit_oldest_rows_of_its_own_workspace(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
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
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            crate::segments::tests_support::segment_reading(
                &pool,
                ws,
                zirv_flow_core::segment::ChangeClasses::from_bits(63),
            )
            .await;
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_drainers_apply_every_row_exactly_once(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_row_becomes_an_apply_failed_reject_while_the_rest_apply(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unmappable_identify_row_becomes_a_record_reject(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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

            // The drain records the row's API key as the cause, not the request id.
            let key_id: Uuid = sqlx::query_scalar("SELECT api_key_id FROM ingest_inbox")
                .fetch_one(&pool)
                .await
                .unwrap();
            let drained = TraceContext {
                trace_id: trace.trace_id,
                cause: Cause::api(key_id.to_string()),
            };

            let outcome = drain_batch_singly(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            assert_eq!(outcome.applied[&("identify", "apply_failed")], 1);
            let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(kinds, ["record"]);

            let reject: Uuid = sqlx::query_scalar("SELECT id FROM ingest_rejects")
                .fetch_one(&pool)
                .await
                .unwrap();
            crate::decision_testing::assert_fixture(
                &pool,
                ws,
                &drained,
                ("record.rejected", "rejected", Some("apply_failed")),
                json!({"reject": reject}),
            )
            .await;
        })
        .await;
    }

    async fn register_trait(
        pool: &PgPool,
        ws: WorkspaceId,
        key: &str,
        trait_type: zirv_flow_core::people::TraitType,
    ) {
        crate::people::create_trait_definition(
            pool,
            ws,
            &crate::people::NewTraitDefinition {
                key: key.to_owned(),
                trait_type,
                precedence: zirv_flow_core::people::Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identify_that_drops_traits_applies_the_rest_and_leaves_one_reject_naming_them(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            for singly in [false, true] {
                let ws = workspace(&pool, if singly { "singly" } else { "batch" }).await;
                register_trait(&pool, ws, "age", zirv_flow_core::people::TraitType::Number).await;
                register_trait(&pool, ws, "old", zirv_flow_core::people::TraitType::String).await;
                crate::people::update_trait_definition(
                    &pool,
                    ws,
                    "old",
                    &crate::people::TraitDefinitionUpdate {
                        archived: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                let trace = TraceContext::root(Cause::api("req"));
                let items = [identify(
                    "u1",
                    "identify-1",
                    json!({"age": "forty-secret", "old": "gone-secret", "plan": "pro"}),
                )];
                // The same message sent twice leaves the one reject.
                for _ in 0..2 {
                    append(&pool, ws, &trace, &items).await;
                    let outcome = if singly {
                        drain_batch_singly(&pool, ws, Shard::ALL, &key(), 500).await
                    } else {
                        drain_batch(&pool, ws, Shard::ALL, &key(), 500).await
                    }
                    .unwrap();
                    assert_eq!(outcome.rows, 1);
                }

                let plan: Option<Value> = sqlx::query_scalar(
                    "SELECT traits -> 'plan' FROM people WHERE workspace_id = $1",
                )
                .bind(ws.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(plan, Some(json!("pro")));

                let rows: Vec<(String, String, Option<String>, Value, Value, bool)> =
                    sqlx::query_as(
                        "SELECT kind, reason, message_id, reasons, sample, person_id IS NOT NULL \
                         FROM ingest_rejects WHERE workspace_id = $1",
                    )
                    .bind(ws.as_uuid())
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                assert_eq!(rows.len(), 1);
                let (kind, reason, message_id, reasons, sample, has_person) = &rows[0];
                assert_eq!(kind, "record");
                assert_eq!(reason, "traits_dropped");
                assert_eq!(message_id.as_deref(), Some("identify-1"));
                assert!(has_person);
                let mut reasons: Vec<(String, String)> = reasons
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|entry| {
                        (
                            entry["property"].as_str().unwrap().to_owned(),
                            entry["reason"].as_str().unwrap().to_owned(),
                        )
                    })
                    .collect();
                reasons.sort();
                assert_eq!(
                    reasons,
                    [
                        ("age".to_owned(), "wrong_type".to_owned()),
                        ("old".to_owned(), "archived_key".to_owned()),
                    ]
                );
                let kept = format!("{sample}");
                assert!(!kept.contains("secret"), "{kept}");
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_heads_report_the_oldest_row_of_each_workspace_with_rows(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_heads_probe_the_workspace_index_beside_a_large_inbox_and_an_empty_one(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let one = workspace(&pool, "one").await;
            let name = WorkspaceName::parse("two").unwrap();
            let slug = Slug::parse("two").unwrap();
            let two = create_workspace(&pool, &name, &slug).await.unwrap().id;
            sqlx::query(
            "INSERT INTO ingest_inbox (workspace_id, api_key_id, kind, item, message_id, trace_id) \
             SELECT $1, gen_random_uuid(), 'track', '{}', 'm-' || g, gen_random_uuid() \
             FROM generate_series(1, 20000) g",
        )
        .bind(one.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
            sqlx::query("ANALYZE ingest_inbox")
                .execute(&pool)
                .await
                .unwrap();

            let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {INBOX_HEADS}"
            )))
            .bind(i64::MIN)
            .bind(i64::MAX)
            .fetch_one(&pool)
            .await
            .unwrap();
            let heads = inbox_heads(&pool).await.unwrap();

            let mut stack = vec![&plan[0]["Plan"]];
            let mut inbox_scans = Vec::new();
            while let Some(node) = stack.pop() {
                if node["Relation Name"] == "ingest_inbox" {
                    inbox_scans.push((
                        node["Node Type"].as_str().unwrap_or_default(),
                        node["Index Name"].as_str().unwrap_or_default(),
                    ));
                }
                if let Some(children) = node["Plans"].as_array() {
                    stack.extend(children);
                }
            }
            assert_eq!(
                inbox_scans,
                [("Index Scan", "ingest_inbox_workspace_idx")],
                "{plan}"
            );
            let top = &plan[0]["Plan"];
            let buffers = top["Shared Hit Blocks"].as_i64().unwrap()
                + top["Shared Read Blocks"].as_i64().unwrap();
            assert!(buffers < 50, "{buffers} buffers\n{plan}");
            assert_eq!(
                heads
                    .iter()
                    .map(|head| head.workspace_id)
                    .collect::<Vec<_>>(),
                [one]
            );
            assert!(heads.iter().all(|head| head.workspace_id != two));
        })
        .await;
    }

    /// The leader reads the heads every tick, so a slow inbox must not hold the tick: the read
    /// gives up after a second. The outer timeout only keeps an unbounded read from hanging the
    /// test on its own blocker.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_heads_give_up_after_a_second(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let mut blocker = pool.begin().await.unwrap();
            sqlx::query("LOCK TABLE ingest_inbox IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *blocker)
                .await
                .unwrap();
            let started = std::time::Instant::now();

            let blocked =
                tokio::time::timeout(std::time::Duration::from_secs(10), inbox_heads(&pool)).await;

            let waited = started.elapsed();
            blocker.rollback().await.unwrap();
            assert!(matches!(blocked, Ok(Err(_))), "{blocked:?}");
            assert!(
                waited >= std::time::Duration::from_millis(900)
                    && waited < std::time::Duration::from_secs(4),
                "{waited:?}"
            );
        })
        .await;
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
        let one = item_bucket(&item, &KnownPeople::new());
        item.identifiers.reverse();

        assert_eq!(item_bucket(&item, &KnownPeople::new()), one);
        assert_eq!(one, bucket("external:user_id:u1"));
    }

    #[test]
    fn an_item_without_a_valid_identifier_is_in_bucket_zero() {
        let mut item = identify("u1", "m-1", json!({}));
        item.identifiers = pairs(&[("not-a-kind", "x")]);

        assert_eq!(item_bucket(&item, &KnownPeople::new()), 0);
        item.identifiers.clear();
        assert_eq!(item_bucket(&item, &KnownPeople::new()), 0);
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

        assert_eq!(
            item_bucket(&contact(&[]), &KnownPeople::new()),
            bucket("c-1")
        );
        assert_eq!(
            item_bucket(
                &contact(&[("email", PERSONAL_DATA.email)]),
                &KnownPeople::new()
            ),
            bucket("c-1")
        );
    }

    fn web(anonymous: &str, message_id: &str) -> IngestItem {
        let mut item = track("-", message_id, "viewed", json!({}));
        item.identifiers = pairs(&[("anonymous_id", anonymous)]);
        item
    }

    fn known(entries: &[(&str, Uuid)]) -> KnownPeople {
        entries
            .iter()
            .map(|(text, person)| ((*text).to_owned(), *person))
            .collect()
    }

    #[test]
    fn a_lane_key_is_the_person_else_the_contact_key_else_the_anonymous_id_else_the_strongest() {
        let (ada, other) = (Uuid::now_v7(), Uuid::now_v7());
        let none = KnownPeople::new();
        let both = pairs(&[("email", "ada@example.com"), ("anonymous_id", "a-1")]);

        assert_eq!(
            lane_key("identify", &both, &none).unwrap(),
            "anonymous_id:a-1"
        );
        let people = known(&[("anonymous_id:a-1", ada)]);
        assert_eq!(
            lane_key("identify", &both, &people),
            Some(format!("person:{ada}"))
        );
        // Two people: the strongest linked identifier decides.
        let split = known(&[("anonymous_id:a-1", ada), ("email:ada@example.com", other)]);
        assert_eq!(
            lane_key("identify", &both, &split),
            Some(format!("person:{other}"))
        );
        let contact = pairs(&[(contacts::KEY_KIND, "c-1"), ("email", "ada@example.com")]);
        assert_eq!(lane_key("contact", &contact, &none).unwrap(), "c-1");
        let email = known(&[("email:ada@example.com", ada)]);
        assert_eq!(
            lane_key("contact", &contact, &email),
            Some(format!("person:{ada}"))
        );
        assert_eq!(lane_key("identify", &pairs(&[("fax", "1")]), &none), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_page_view_and_the_identify_naming_its_visitor_share_a_lane_before_the_person_exists(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let mut named = identify("-", "m-2", json!({}));
            named.identifiers = pairs(&[
                ("anonymous_id", "a-1"),
                ("email", "ada@example.com"),
                ("external:user_id", "u1"),
            ]);

            append(&pool, ws, &trace, &[web("a-1", "m-1"), named]).await;

            let stored = affinities(&pool).await;
            assert_eq!(stored[0], stored[1]);
            assert_eq!(stored[0], bucket("anonymous_id:a-1"));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn once_the_person_exists_a_server_row_and_a_web_row_share_its_lane(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let ada = person_with(
                &pool,
                ws,
                &[("anonymous_id", "a-1"), ("external:user_id", "u1")],
                None,
            )
            .await;

            append(
                &pool,
                ws,
                &trace,
                &[web("a-1", "m-1"), track("u1", "m-2", "bought", json!({}))],
            )
            .await;

            let stored = affinities(&pool).await;
            assert_eq!(stored, [bucket(&format!("person:{ada}")); 2]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_of_a_merged_person_and_a_contact_of_a_known_person_key_by_the_survivor(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let survivor = person_with(&pool, ws, &[("email", "ada@example.com")], None).await;
            person_with(&pool, ws, &[("external:user_id", "u9")], Some(survivor)).await;
            let mut contact = identify("-", "m-2", json!({}));
            contact.identifiers =
                pairs(&[(contacts::KEY_KIND, "c-1"), ("email", "ada@example.com")]);
            contact.payload = IngestPayload::Contact { traits: Map::new() };

            append(
                &pool,
                ws,
                &trace,
                &[track("u9", "m-1", "seen", json!({})), contact],
            )
            .await;

            let expected = bucket(&format!("person:{survivor}"));
            assert_eq!(affinities(&pool).await, [expected; 2]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_resolves_its_people_in_one_lookup_and_a_batch_without_identifiers_skips_it(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let ada = person_with(&pool, ws, &[("external:user_id", "u1")], None).await;
            let rows: Vec<Vec<(String, String)>> = (0..50)
                .map(|n| pairs(&[("external:user_id", &format!("u{}", n % 2 + 1))]))
                .collect();

            let found = known_people(&pool, ws, rows.iter().map(Vec::as_slice))
                .await
                .unwrap();

            assert_eq!(found.len(), 1);
            assert_eq!(found["external:user_id:u1"], ada);
            // `known_people` runs that one statement and no other: a pool that reaches no
            // database refuses it, and the batch without identifiers still answers.
            let nowhere = PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap();
            let none: Vec<&[(String, String)]> = vec![&[]];
            assert!(known_people(&nowhere, ws, none).await.unwrap().is_empty());
            assert!(
                known_people(&nowhere, ws, rows.iter().map(Vec::as_slice))
                    .await
                    .is_err()
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_stores_the_bucket_of_each_items_record_key(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    /// What `shard` takes from the workspace's inbox, in id order, and whether it may hold more.
    async fn taken(pool: &PgPool, ws: WorkspaceId, shard: Shard, limit: i64) -> (Vec<i64>, bool) {
        let mut tx = begin(pool).await.unwrap();
        let (rows, more, _) = take_rows(&mut tx, ws, shard, limit, &TakeCursors::default())
            .await
            .unwrap();
        // A dropped transaction rolls back only when its connection is next used, so the
        // next call could run on another connection while these rows are still locked.
        tx.rollback().await.unwrap();
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
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let items: Vec<IngestItem> = (0..60)
                .map(|n| track(&format!("u{n}"), &format!("m-{n}"), "seen", json!({})))
                .collect();
            append(&pool, ws, &trace, &items).await;
            sqlx::query(
                "UPDATE ingest_inbox SET affinity = NULL WHERE message_id IN ('m-0', 'm-1')",
            )
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_backfill_gives_a_row_the_bucket_an_append_would_have(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let contact = IngestItem {
                identifiers: pairs(&[(contacts::KEY_KIND, "c-1"), ("email", PERSONAL_DATA.email)]),
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hot_key_fills_a_whole_batch_whatever_the_shard_count(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            raw_rows(&pool, ws, Some(0), 24, "hot").await;
            let hot: Vec<i64> = sqlx::query_scalar("SELECT id FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();

            for count in [1, 4] {
                let (ids, more) = taken(&pool, ws, Shard { index: 0, count }, 8).await;

                assert_eq!(ids, hot[..8], "{count} shards");
                assert!(more);
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cold_buckets_rows_are_not_starved_by_a_hot_one(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            raw_rows(&pool, ws, Some(4), 1, "older").await;
            raw_rows(&pool, ws, Some(0), 24, "hot").await;
            raw_rows(&pool, ws, Some(4), 1, "newer").await;
            let cold: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM ingest_inbox WHERE affinity = 4 ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let shard = Shard { index: 0, count: 4 };

            let (first, _) = taken(&pool, ws, shard, 8).await;
            assert!(
                first.contains(&cold[0]),
                "the older cold row is in the first batch"
            );

            let key = key();
            let mut batches = 0;
            while sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ingest_inbox WHERE id = $1")
                .bind(cold[1])
                .fetch_one(&pool)
                .await
                .unwrap()
                == 1
            {
                drain_batch(&pool, ws, shard, &key, 8).await.unwrap();
                batches += 1;
                assert!(
                    batches <= 4,
                    "within ceil(3 * limit / limit) batches, plus the first"
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shard_with_empty_buckets_takes_nothing_while_another_holds_a_backlog(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            raw_rows(&pool, ws, Some(0), 300, "hot").await;

            let (ids, more) = taken(&pool, ws, Shard { index: 1, count: 4 }, 8).await;

            assert!(ids.is_empty());
            assert!(!more, "an empty shard is finished after one batch");
            let (ids, more) = taken(&pool, ws, Shard { index: 0, count: 4 }, 8).await;
            assert!(!ids.is_empty() && ids.len() <= 8 && more);
        })
        .await;
    }

    async fn plan_of(pool: &PgPool, user: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT p.traits->>'plan' FROM people p JOIN identities i ON i.person_id = p.id \
             WHERE i.value = $1",
        )
        .bind(user)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn nothing_applies_past_a_locked_row_of_its_bucket(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let mut items = vec![identify("k", "k-old", json!({"plan": "old"}))];
            items.extend((0..5).map(|n| identify(&format!("f{n}"), &format!("f-{n}"), json!({}))));
            items.push(identify("k", "k-new", json!({"plan": "new"})));
            append(&pool, ws, &trace, &items).await;
            sqlx::query("UPDATE ingest_inbox SET affinity = 0 WHERE message_id LIKE 'k-%'")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "UPDATE ingest_inbox SET affinity = 1 + id % 60 WHERE message_id LIKE 'f-%'",
            )
            .execute(&pool)
            .await
            .unwrap();
            let mut holder = begin(&pool).await.unwrap();
            sqlx::query("SELECT id FROM ingest_inbox WHERE message_id = 'k-old' FOR UPDATE")
                .fetch_all(&mut *holder)
                .await
                .unwrap();

            let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            assert_eq!(outcome.rows, 5, "the other buckets' rows apply");
            let left: Vec<String> =
                sqlx::query_scalar("SELECT message_id FROM ingest_inbox ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, ["k-old", "k-new"]);
            assert_eq!(plan_of(&pool, "k").await, None);

            commit(holder).await.unwrap();
            drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap();

            assert_eq!(inbox_count(&pool).await, 0);
            assert_eq!(plan_of(&pool, "k").await.as_deref(), Some("new"));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_whose_picked_rows_are_all_locked_takes_nothing_and_is_not_more(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let items: Vec<IngestItem> = (0..3)
                .map(|n| identify(&format!("u{n}"), &format!("m-{n}"), json!({})))
                .collect();
            append(&pool, ws, &trace, &items).await;
            let mut holder = begin(&pool).await.unwrap();
            sqlx::query("SELECT id FROM ingest_inbox FOR UPDATE")
                .fetch_all(&mut *holder)
                .await
                .unwrap();

            let outcome = drain_batch(&pool, ws, Shard::ALL, &key(), 2).await.unwrap();

            assert_eq!((outcome.rows, outcome.more), (0, false));
            assert_eq!(inbox_count(&pool).await, 3);
        })
        .await;
    }

    #[test]
    fn a_shard_owns_buckets_only_with_a_count_up_to_the_bucket_count_and_an_index_below_it() {
        for (index, count, valid) in [(0, 1, true), (63, 64, true), (4, 4, false), (0, 0, false)] {
            assert_eq!(Shard { index, count }.is_valid(), valid, "{index}/{count}");
        }
        assert!(
            !Shard {
                index: 0,
                count: 65
            }
            .is_valid()
        );
        assert_eq!(Shard::of_bucket(13, 8), Shard { index: 5, count: 8 });
        assert_eq!(Shard::of_bucket(13, 0), Shard { index: 0, count: 1 });
        assert_eq!(
            Shard::of_bucket(63, 200),
            Shard {
                index: 63,
                count: 64
            }
        );
    }

    #[test]
    fn a_payload_names_its_shard_or_none_or_one_that_owns_no_buckets() {
        assert_eq!(payload_shard(&json!({ "workspace_id": "w" })), Ok(None));
        assert_eq!(
            payload_shard(&json!({ "shard": { "index": 2, "count": 8 } })),
            Ok(Some(Shard { index: 2, count: 8 }))
        );
        for shard in [
            json!({ "index": 8, "count": 8 }),
            json!({ "index": 0, "count": 65 }),
            json!({ "index": "0", "count": 8 }),
            json!({ "count": 8 }),
        ] {
            assert!(payload_shard(&json!({ "shard": shard })).is_err());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_enqueues_exactly_the_shards_its_rows_fall_in_in_one_statement(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            sqlx::raw_sql(
                "CREATE TABLE statement_log (client_query text NOT NULL); \
                 CREATE FUNCTION log_statement() RETURNS trigger LANGUAGE plpgsql AS \
                   $$ BEGIN INSERT INTO statement_log VALUES (current_query()); RETURN NULL; END $$; \
                 CREATE TRIGGER log_inbox AFTER INSERT ON ingest_inbox \
                   FOR EACH STATEMENT EXECUTE FUNCTION log_statement(); \
                 CREATE TRIGGER log_jobs AFTER INSERT ON jobs \
                   FOR EACH STATEMENT EXECUTE FUNCTION log_statement();",
            )
            .execute(&pool)
            .await
            .unwrap();
            let items: Vec<IngestItem> = (0..40)
                .map(|n| identify(&format!("u{n}"), &format!("m-{n}"), json!({})))
                .collect();

            append(&pool, ws, &TraceContext::root(Cause::api("req")), &items).await;

            let shards: Vec<i32> =
                sqlx::query_scalar("SELECT DISTINCT affinity % 8 FROM ingest_inbox ORDER BY 1")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(shards.len() > 1, "forty keys fall in several shards");
            let mut keys: Vec<String> =
                sqlx::query_scalar("SELECT unique_key FROM jobs ORDER BY unique_key")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let mut expected: Vec<String> = shards
                .iter()
                .map(|index| format!("ingest.drain:{}:{index}/8", ws.as_uuid()))
                .collect();
            keys.sort();
            expected.sort();
            assert_eq!(keys, expected);
            // The statement builds each job itself; it must be the job `drain_job` describes.
            let trace = TraceContext::root(Cause::api("req"));
            for index in &shards {
                let shard = Shard {
                    index: u32::try_from(*index).unwrap(),
                    count: 8,
                };
                let want = drain_job(ws, shard, &trace);
                let got: (Value, i16, i32, String, String, Option<Uuid>, Option<String>) =
                    sqlx::query_as(
                        "SELECT payload, priority, max_attempts, kind, cause_kind, workspace_id, \
                                cause_id FROM jobs WHERE unique_key = $1",
                    )
                    .bind(want.unique_key.as_deref())
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                assert_eq!(got.0, want.payload);
                assert_eq!((got.1, got.2), (want.priority, want.max_attempts));
                assert_eq!(got.3, want.kind);
                assert_eq!(got.4, want.trace.cause.kind.as_str());
                assert_eq!(got.5, want.workspace_id.map(|id| id.as_uuid()));
                assert_eq!(got.6.as_deref(), Some(want.trace.cause.id.as_str()));
            }
            // A second append finds every key held and inserts nothing.
            append(&pool, ws, &trace, &items).await;
            let held: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(held, i64::try_from(shards.len()).unwrap());
            let (writes, distinct): (i64, i64) =
                sqlx::query_as("SELECT count(*), count(DISTINCT client_query) FROM statement_log")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!((writes, distinct), (4, 1), "each append is one statement for its rows and jobs");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_drains_over_one_bucket_with_different_counts_apply_a_key_in_id_order(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let trace = TraceContext::root(Cause::api("req"));
            let items: Vec<IngestItem> = (0..120)
                .map(|n| identify("k", &format!("m-{n:03}"), json!({ "n": n })))
                .collect();
            append(&pool, ws, &trace, &items).await;
            let bucket: i16 = sqlx::query_scalar("SELECT affinity FROM ingest_inbox LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap();
            let shards = [
                Shard::of_bucket(bucket, 8),
                Shard::of_bucket(bucket, 4),
                Shard::ALL,
            ];

            let drains = shards.map(|shard| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    while inbox_count(&pool).await > 0 {
                        drain_batch(&pool, ws, shard, &key(), 5).await.unwrap();
                        tokio::task::yield_now().await;
                    }
                })
            });
            for drain in drains {
                drain.await.unwrap();
            }

            let last: Option<i64> = sqlx::query_scalar(
                "SELECT (p.traits->>'n')::bigint FROM people p JOIN identities i \
                 ON i.person_id = p.id WHERE i.value = 'k'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                last,
                Some(119),
                "no row applied after a later one of its key"
            );
        })
        .await;
    }

    // Holds with row locks alone, which is why the backfill takes no further lock: a run
    // that waited on another's rows skips them and fills its batch from later rows.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_backfills_finish_only_when_no_row_is_left_unbucketed(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            raw_rows(&pool, ws, None, 90, "old").await;

            let runs: Vec<_> = (0..4)
                .map(|_| {
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        while backfill_affinity(&pool, ws, 7).await.unwrap() >= 7 {}
                        // This backfill reported done: nothing may still lack a bucket.
                        sqlx::query_scalar::<_, i64>(
                            "SELECT count(*) FROM ingest_inbox WHERE affinity IS NULL",
                        )
                        .fetch_one(&pool)
                        .await
                        .unwrap()
                    })
                })
                .collect();

            for run in runs {
                assert_eq!(run.await.unwrap(), 0);
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_take_leaves_out_the_later_rows_of_a_bucket_whose_earlier_row_another_transaction_holds(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            raw_rows(&pool, ws, Some(0), 3, "held").await;
            raw_rows(&pool, ws, Some(8), 2, "free").await;
            let held: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM ingest_inbox WHERE affinity = 0 ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let free: Vec<i64> =
                sqlx::query_scalar("SELECT id FROM ingest_inbox WHERE affinity = 8 ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let mut holder = begin(&pool).await.unwrap();
            sqlx::query("SELECT id FROM ingest_inbox WHERE id = $1 FOR UPDATE")
                .bind(held[0])
                .fetch_all(&mut *holder)
                .await
                .unwrap();

            // Two jobs of different counts both cover buckets 0 and 8.
            for shard in [Shard { index: 0, count: 8 }, Shard::ALL] {
                let (ids, _) = taken(&pool, ws, shard, 10).await;

                assert_eq!(
                    ids, free,
                    "{shard:?}: the later rows of the held bucket wait"
                );
            }
            commit(holder).await.unwrap();
            let (ids, _) = taken(&pool, ws, Shard::ALL, 10).await;
            let mut all: Vec<i64> = held.iter().chain(&free).copied().collect();
            all.sort_unstable();
            assert_eq!(
                ids, all,
                "once released the bucket's rows follow in id order"
            );
        })
        .await;
    }

    /// The one shard of 64 that owns bucket 3 alone.
    const BUCKET_3: Shard = Shard {
        index: 3,
        count: 64,
    };

    /// One batch of `BUCKET_3` with `cursors`: the ids it took.
    async fn drained_with(pool: &PgPool, ws: WorkspaceId, cursors: &TakeCursors) -> Vec<i64> {
        let before: Vec<i64> = sqlx::query_scalar("SELECT id FROM ingest_inbox ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        let outcome = drain_batch_using(pool, ws, BUCKET_3, &key(), 500, cursors)
            .await
            .unwrap();
        let after: Vec<i64> = sqlx::query_scalar("SELECT id FROM ingest_inbox ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
        let took: Vec<i64> = before
            .into_iter()
            .filter(|id| !after.contains(id))
            .collect();
        assert_eq!(took.len(), outcome.rows);
        took
    }

    /// Appends a track row to bucket 3 on `connection` and returns its id; the caller commits.
    async fn bucket_3_row(connection: &mut PgConnection, ws: WorkspaceId, tag: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO ingest_inbox \
                (workspace_id, api_key_id, kind, item, message_id, trace_id, affinity) \
             VALUES ($1, gen_random_uuid(), 'track', \
                     jsonb_build_object('identifiers', \
                        jsonb_build_array(jsonb_build_array('external:user_id', $2::text)), \
                        'timestamp', null, 'event', 'seen', 'properties', jsonb_build_object()), \
                     $2, gen_random_uuid(), 3) \
             RETURNING id",
        )
        .bind(ws.as_uuid())
        .bind(tag)
        .fetch_one(connection)
        .await
        .unwrap()
    }

    /// Commits rows to bucket 3 until `cursors` reads it after an id above `straggler`, which a
    /// transaction still holds uncommitted, and returns the ids taken meanwhile.
    async fn move_the_cursor_past(
        pool: &PgPool,
        ws: WorkspaceId,
        cursors: &TakeCursors,
        straggler: i64,
    ) -> Vec<i64> {
        let mut took = Vec::new();
        for round in 0..3 {
            let mut connection = pool.acquire().await.unwrap();
            bucket_3_row(&mut connection, ws, &format!("newer-{round}")).await;
            took.extend(drained_with(pool, ws, cursors).await);
        }
        assert!(
            cursors.start(ws, &[3]).after[0] > straggler,
            "the cursor passed the uncommitted row"
        );
        took
    }

    /// A row whose id is below a take cursor but that commits after the cursor passed it (ids come
    /// from a sequence, commits do not follow it) is still taken: at once by the next take, when
    /// nothing above the cursor is left, as the requeue's probe of the shard then still finds it.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_that_commits_below_a_take_cursor_is_taken_by_the_next_take(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(3600));
            let mut late = begin(&pool).await.unwrap();
            let straggler = bucket_3_row(&mut late, ws, "late").await;
            let took = move_the_cursor_past(&pool, ws, &cursors, straggler).await;
            assert_eq!(took.len(), 3);
            commit(late).await.unwrap();

            assert!(shard_pending(&pool, ws, BUCKET_3).await.unwrap());
            assert_eq!(drained_with(&pool, ws, &cursors).await, [straggler]);
            assert_eq!(inbox_count(&pool).await, 0);
        })
        .await;
    }

    /// While newer rows keep its bucket's takes busy above the cursor, a straggler below it is
    /// taken by the bucket's next full walk, at most a period after it committed.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_below_a_take_cursor_is_taken_within_the_period_behind_newer_rows(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let period = Duration::from_millis(1500);
            let cursors = TakeCursors::new(Duration::ZERO, period);
            let mut late = begin(&pool).await.unwrap();
            let straggler = bucket_3_row(&mut late, ws, "late").await;
            move_the_cursor_past(&pool, ws, &cursors, straggler).await;
            commit(late).await.unwrap();
            let committed = Instant::now();

            let mut round = 0;
            loop {
                let mut connection = pool.acquire().await.unwrap();
                bucket_3_row(&mut connection, ws, &format!("busy-{round}")).await;
                drop(connection);
                round += 1;
                if drained_with(&pool, ws, &cursors).await.contains(&straggler) {
                    break;
                }
                assert!(
                    committed.elapsed() < period + Duration::from_secs(2),
                    "the row below the cursor waited past the period"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            drained_with(&pool, ws, &cursors).await;
            assert_eq!(inbox_count(&pool).await, 0, "no row is lost");
        })
        .await;
    }

    /// A take stopped by a locked row keeps its cursor below that row, so the row and the rows
    /// after it are taken in id order once it is free.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_take_stopped_by_a_locked_row_keeps_its_cursor_below_it(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(3600));
            move_the_cursor_past(&pool, ws, &cursors, 0).await;
            let mut connection = pool.acquire().await.unwrap();
            let held = bucket_3_row(&mut connection, ws, "held").await;
            let after = bucket_3_row(&mut connection, ws, "after").await;
            drop(connection);
            let mut holder = begin(&pool).await.unwrap();
            sqlx::query("SELECT id FROM ingest_inbox WHERE id = $1 FOR UPDATE")
                .bind(held)
                .fetch_all(&mut *holder)
                .await
                .unwrap();

            // The second take begins after the first saw both rows, so only the stop keeps its
            // cursor below the held one.
            for _ in 0..2 {
                assert!(drained_with(&pool, ws, &cursors).await.is_empty());
            }
            assert!(cursors.start(ws, &[3]).after[0] < held);
            commit(holder).await.unwrap();

            assert_eq!(drained_with(&pool, ws, &cursors).await, [held, after]);
        })
        .await;
    }

    /// Drains in small batches while the cursors trail the takes by nothing, with appends between
    /// the batches: every row is taken, and each key ends on its last value. The people exist
    /// before the rounds, so every row is keyed to its person's lane (7.1: order is not kept
    /// across a key's first resolution, cursors or not).
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_with_moving_cursors_takes_every_row_in_key_order(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(3600));
            let trace = TraceContext::root(Cause::api("req"));
            let shards: Vec<Shard> = (0..4).map(|index| Shard { index, count: 4 }).collect();
            let people: Vec<IngestItem> = (0..40)
                .map(|n| identify(&format!("u{n}"), &format!("seed-{n}"), json!({})))
                .collect();
            append(&pool, ws, &trace, &people).await;
            while drain_batch(&pool, ws, Shard::ALL, &key(), 500)
                .await
                .unwrap()
                .rows
                > 0
            {}
            for round in ["free", "basic", "pro"] {
                let items: Vec<IngestItem> = (0..40)
                    .map(|n| {
                        identify(
                            &format!("u{n}"),
                            &format!("{round}-{n}"),
                            json!({ "plan": round }),
                        )
                    })
                    .collect();
                append(&pool, ws, &trace, &items).await;
                for shard in &shards {
                    drain_batch_using(&pool, ws, *shard, &key(), 7, &cursors)
                        .await
                        .unwrap();
                }
            }
            for _ in 0..40 {
                for shard in &shards {
                    drain_batch_using(&pool, ws, *shard, &key(), 7, &cursors)
                        .await
                        .unwrap();
                }
            }

            assert_eq!(inbox_count(&pool).await, 0);
            for n in 0..40 {
                assert_eq!(
                    plan_of(&pool, &format!("u{n}")).await.as_deref(),
                    Some("pro"),
                    "u{n} ended on its last value"
                );
            }
            assert!(
                shards
                    .iter()
                    .flat_map(|shard| shard.buckets())
                    .any(|bucket| cursors.start(ws, &[bucket]).after[0] > 0),
                "the takes read from their cursors"
            );
        })
        .await;
    }
}
