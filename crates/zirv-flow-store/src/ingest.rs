//! The durable ingestion inbox (architecture sections 4.4 and 7.1). The API appends raw
//! items and answers once the append has committed; `ingest.drain` applies them through the
//! profile and event paths of apply-batch and deletes them in the same transaction.
//!
//! Nothing here logs or labels anything with an identifier, a trait or property value or an
//! event name.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sqlx::Connection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::people::Identifier;
use zirv_flow_core::{Cause, HmacKey, TraceContext, TraceId, WorkspaceId};

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
        IngestPayload::Identify { traits } => stored["traits"] = Value::Object(traits.clone()),
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
pub async fn append_inbox(
    pool: &PgPool,
    workspace: WorkspaceId,
    api_key_id: Uuid,
    trace: &TraceContext,
    received_at: OffsetDateTime,
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

    let mut tx = begin(pool).await?;
    let inserted = sqlx::query(
        "INSERT INTO ingest_inbox \
            (workspace_id, api_key_id, kind, item, message_id, received_at, trace_id) \
         SELECT $1, $2, i.kind, i.item, i.message_id, $3, $4 \
         FROM unnest($5::text[], $6::jsonb[], $7::text[]) WITH ORDINALITY \
            AS i(kind, item, message_id, n) \
         ORDER BY i.n",
    )
    .bind(workspace.as_uuid())
    .bind(api_key_id)
    .bind(received_at)
    .bind(trace.trace_id.as_uuid())
    .bind(&kinds)
    .bind(&stored)
    .bind(&message_ids)
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
    job.payload = serde_json::json!({ "workspace_id": workspace.as_uuid() });
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
}

impl DrainOutcome {
    fn count(&mut self, kind: &'static str, outcome: &'static str) {
        *self.applied.entry((kind, outcome)).or_default() += 1;
    }

    /// Adds another batch's tallies to this one.
    pub fn absorb(&mut self, other: Self) {
        self.rows += other.rows;
        self.late += other.late;
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
}

enum Mapped {
    Profile(ProfileRecord),
    Event(EventRecord),
}

/// Maps a row to the record the apply path takes. A row that cannot be mapped is poison.
fn map_row(row: &InboxRow) -> Option<Mapped> {
    let stored = StoredItem::parse(&row.item)?;
    let timestamp = match &stored.timestamp {
        Some(text) => Some(OffsetDateTime::parse(text, &Rfc3339).ok()?),
        None => None,
    };
    let trace = TraceContext {
        trace_id: TraceId::from_uuid(row.trace_id),
        cause: Cause::api(row.api_key_id.to_string()),
    };
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

async fn take_rows(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    limit: i64,
) -> Result<Vec<InboxRow>, StoreError> {
    sqlx::query_as(
        "SELECT id, api_key_id, kind, item, message_id, received_at, trace_id \
         FROM ingest_inbox WHERE workspace_id = $1 \
         ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED",
    )
    .bind(workspace.as_uuid())
    .bind(limit)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
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

fn tally_profile(outcome: &mut DrainOutcome, result: &ProfileOutcome) {
    let label = match result {
        ProfileOutcome::Applied { .. } => "valid",
        ProfileOutcome::Unchanged { .. } => "duplicate",
        ProfileOutcome::Rejected { .. } => "rejected",
    };
    outcome.count("identify", label);
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

/// Applies up to `limit` of the workspace's oldest inbox rows in id order and deletes them,
/// all in one transaction. Concurrent drainers skip each other's locked rows, so a row is
/// never applied twice, and a crash leaves every row in place.
///
/// An error rolls the whole batch back. When it is [`StoreError::is_retryable`] the caller
/// retries the batch; otherwise a row is probably poison, and
/// [`drain_batch_singly`] isolates it.
pub async fn drain_batch(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    limit: i64,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    let rows = take_rows(&mut tx, workspace, limit).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        ..DrainOutcome::default()
    };
    let mut events = EventBatch::default();
    for row in &rows {
        match map_row(row).ok_or_else(StoreError::poison)? {
            Mapped::Profile(record) => {
                let result = apply_profile_record(&mut tx, workspace, &record).await?;
                tally_profile(&mut outcome, &result);
            }
            Mapped::Event(record) => {
                let result =
                    apply_event_record(&mut tx, workspace, key, &mut events, &record).await?;
                tally_event(&mut outcome, &result);
            }
        }
    }
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
    key: &HmacKey,
    limit: i64,
) -> Result<DrainOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    let rows = take_rows(&mut tx, workspace, limit).await?;
    let mut outcome = DrainOutcome {
        rows: rows.len(),
        ..DrainOutcome::default()
    };
    for row in &rows {
        let mapped = map_row(row);
        let mut savepoint = tx.begin().await.map_err(StoreError::query)?;
        let applied = match &mapped {
            Some(Mapped::Profile(record)) => {
                apply_profile_record(&mut savepoint, workspace, record)
                    .await
                    .map(|result| tally_profile(&mut outcome, &result))
            }
            Some(Mapped::Event(record)) => {
                let mut events = EventBatch::default();
                apply_event_record(&mut savepoint, workspace, key, &mut events, record)
                    .await
                    .map(|result| tally_event(&mut outcome, &result))
            }
            None => Err(StoreError::poison()),
        };
        match applied {
            Ok(()) => savepoint.commit().await.map_err(StoreError::query)?,
            Err(error) if error.is_retryable() => return Err(error),
            Err(_) => {
                savepoint.rollback().await.map_err(StoreError::query)?;
                reject_row(&mut tx, workspace, key, row, mapped.as_ref()).await?;
                outcome.count(
                    if row.kind == "identify" {
                        "identify"
                    } else {
                        "track"
                    },
                    "apply_failed",
                );
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
    let trace = TraceContext {
        trace_id: TraceId::from_uuid(row.trace_id),
        cause: Cause::api(row.api_key_id.to_string()),
    };
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
            kind: if row.kind == "identify" {
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
        }
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn append(pool: &PgPool, ws: WorkspaceId, trace: &TraceContext, items: &[IngestItem]) {
        let appended = append_inbox(
            pool,
            ws,
            Uuid::now_v7(),
            trace,
            OffsetDateTime::now_utc(),
            items,
        )
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

        let outcome = drain_batch(&pool, ws, &key(), 500).await.unwrap();

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

        let outcome = drain_batch(&pool, one, &key(), 3).await.unwrap();

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
        drain_batch(&pool, ws, &key(), 500).await.unwrap();
        let before = dump(&pool).await;
        assert!(
            before.iter().all(|(_, rows)| !rows.is_empty()),
            "{before:?}"
        );

        append(&pool, ws, &trace, &batch()).await;
        let replay = drain_batch(&pool, ws, &key(), 500).await.unwrap();

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
            "INSERT INTO ingest_inbox (workspace_id, api_key_id, kind, item, message_id, trace_id) \
             SELECT $1, gen_random_uuid(), 'track', \
                    jsonb_build_object('identifiers', \
                        jsonb_build_array(jsonb_build_array('external:user_id', 'u' || (n % 400))), \
                        'timestamp', null, 'event', 'seen', 'properties', jsonb_build_object('n', n)), \
                    'm-' || n, $2 \
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
                match drain_batch(&pool, ws, &key, 100).await {
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

        let whole = drain_batch(&pool, ws, &key, 500).await.unwrap_err();
        assert!(!whole.is_retryable());
        assert_eq!(
            inbox_count(&pool).await,
            3,
            "the failed batch left every row"
        );
        let outcome = drain_batch_singly(&pool, ws, &key, 500).await.unwrap();

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

        let outcome = drain_batch_singly(&pool, ws, &key(), 500).await.unwrap();

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
        let received = OffsetDateTime::now_utc() - time::Duration::seconds(30);
        append_inbox(
            &pool,
            one,
            Uuid::now_v7(),
            &trace,
            received,
            &[track("u1", "m-1", "seen", json!({}))],
        )
        .await
        .unwrap();
        append(&pool, one, &trace, &[track("u1", "m-2", "seen", json!({}))]).await;

        let heads = inbox_heads(&pool).await.unwrap();

        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].workspace_id, one);
        assert!((29.0..60.0).contains(&heads[0].age_seconds), "{heads:?}");
        assert!(heads.iter().all(|head| head.workspace_id != two));
    }
}
