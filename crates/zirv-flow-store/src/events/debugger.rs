//! The live event debugger's feed (architecture section 12.2). While a workspace is watched,
//! the event path writes one `debugger_feed` row per event outcome in the transaction that
//! decides it, then notifies once per batch; when nobody watches, ingestion pays nothing.
//!
//! The feed holds the redacted properties only, as the event or reject keeps them. Masking
//! for the viewer happens when a viewer reads, from the published schema version the row
//! names. Nothing here logs an identifier, a name or a value.

use std::fmt;

use serde_json::{Map, Value};
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::events::{Reason, SchemaProperty};

use crate::StoreError;

/// The `LISTEN` channel a batch notifies; the payload is the workspace id.
pub const DEBUGGER_CHANNEL: &str = "zf_debugger";

/// How far ahead of now a viewer's heartbeat keeps the workspace watched.
pub const WATCH_SECONDS: i64 = 30;

/// How long a feed row lives.
pub const FEED_RETENTION_MINUTES: i64 = 15;

/// Marks the workspace as watched for the next [`WATCH_SECONDS`]. A `serve` process with a
/// viewer calls this every few seconds.
pub async fn touch_debugger_watch<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO debugger_watch (workspace_id, until) \
         VALUES ($1, now() + make_interval(secs => $2)) \
         ON CONFLICT (workspace_id) DO UPDATE SET until = EXCLUDED.until",
    )
    .bind(workspace.as_uuid())
    .bind(WATCH_SECONDS as f64)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

pub(crate) async fn is_watched(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM debugger_watch WHERE workspace_id = $1 AND until > now())",
    )
    .bind(workspace.as_uuid())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// One feed row to write.
pub(crate) struct FeedEntry<'a> {
    pub(crate) source: &'a str,
    pub(crate) name: Option<&'a str>,
    pub(crate) outcome: &'static str,
    pub(crate) reasons: &'a [Reason],
    pub(crate) reason: Option<&'static str>,
    /// Redacted.
    pub(crate) properties: &'a Map<String, Value>,
    pub(crate) schema_version: Option<i32>,
    pub(crate) event_id: Option<Uuid>,
    pub(crate) reject_id: Option<Uuid>,
    pub(crate) trace_id: Uuid,
}

pub(crate) async fn write_feed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    entry: &FeedEntry<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO debugger_feed \
            (workspace_id, source, name, outcome, reasons, reason, properties, schema_version, \
             event_id, reject_id, trace_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(workspace.as_uuid())
    .bind(entry.source)
    .bind(entry.name)
    .bind(entry.outcome)
    .bind(Json(entry.reasons))
    .bind(entry.reason)
    .bind(Json(entry.properties))
    .bind(entry.schema_version)
    .bind(entry.event_id)
    .bind(entry.reject_id)
    .bind(entry.trace_id)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Tells the listening `serve` processes that the workspace's feed grew, when the
/// transaction commits.
pub(crate) async fn notify_feed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_notify($1, $2)")
        .bind(DEBUGGER_CHANNEL)
        .bind(workspace.as_uuid().to_string())
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// A dedicated connection listening for feed notifications.
pub struct DebuggerListener {
    inner: sqlx::postgres::PgListener,
}

/// Starts listening on [`DEBUGGER_CHANNEL`] with a connection of its own.
pub async fn listen_for_debugger(pool: &crate::PgPool) -> Result<DebuggerListener, StoreError> {
    let mut inner = sqlx::postgres::PgListener::connect_with(pool)
        .await
        .map_err(StoreError::query)?;
    inner
        .listen(DEBUGGER_CHANNEL)
        .await
        .map_err(StoreError::query)?;
    Ok(DebuggerListener { inner })
}

impl DebuggerListener {
    /// Waits for a notification and returns the workspace whose feed grew. `Ok(None)` means
    /// the connection was lost (it reconnects on the next call, and notifications sent
    /// meanwhile are lost, so callers poll) or the payload was not a workspace id.
    pub async fn wait(&mut self) -> Result<Option<WorkspaceId>, StoreError> {
        let notification = self.inner.try_recv().await.map_err(StoreError::query)?;
        Ok(
            notification
                .and_then(|notification| notification.payload().parse::<WorkspaceId>().ok()),
        )
    }
}

/// The newest feed `seq` of the workspace, or 0 when the feed is empty.
pub async fn debugger_feed_head<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT COALESCE(max(seq), 0) FROM debugger_feed WHERE workspace_id = $1")
        .bind(workspace.as_uuid())
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

/// Narrows a read of the feed; every field that is set must match.
#[derive(Clone, Copy, Debug, Default)]
pub struct FeedFilter<'a> {
    pub source: Option<&'a str>,
    pub name: Option<&'a str>,
    pub outcome: Option<&'a str>,
}

/// A feed row. Its properties are as the event or reject stored them, redacted at ingest;
/// the viewer's masking is applied by the caller from `schema`.
#[derive(Clone, PartialEq)]
pub struct FeedItem {
    pub seq: i64,
    pub at: OffsetDateTime,
    pub source: String,
    pub name: Option<String>,
    /// `accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`.
    pub outcome: String,
    pub reasons: Vec<Reason>,
    /// The static reject code of a rejected event.
    pub reason: Option<String>,
    pub properties: Map<String, Value>,
    /// The published schema the event was judged against; absent when there was none.
    pub schema: Option<Vec<SchemaProperty>>,
    pub event_id: Option<Uuid>,
    pub reject_id: Option<Uuid>,
    pub trace_id: Uuid,
}

// Properties and names are personal data.
impl fmt::Debug for FeedItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FeedItem")
            .field("seq", &self.seq)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

#[derive(FromRow)]
struct FeedRow {
    seq: i64,
    at: OffsetDateTime,
    source: String,
    name: Option<String>,
    outcome: String,
    reasons: Json<Vec<Reason>>,
    reason: Option<String>,
    properties: Value,
    schema: Option<Value>,
    event_id: Option<Uuid>,
    reject_id: Option<Uuid>,
    trace_id: Uuid,
}

/// The workspace's feed rows after `after`, oldest first, at most `limit`, that match
/// `filter`.
pub async fn read_debugger_feed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    after: i64,
    filter: FeedFilter<'_>,
    limit: i64,
) -> Result<Vec<FeedItem>, StoreError> {
    let rows: Vec<FeedRow> = sqlx::query_as(
        "SELECT f.seq, f.at, f.source, f.name, f.outcome, f.reasons, f.reason, f.properties, \
                v.properties AS schema, f.event_id, f.reject_id, f.trace_id \
         FROM debugger_feed f LEFT JOIN event_schema_versions v \
            ON v.workspace_id = f.workspace_id AND v.name = f.name \
           AND v.version = f.schema_version \
         WHERE f.workspace_id = $1 AND f.seq > $2 \
           AND ($3::text IS NULL OR f.source = $3) \
           AND ($4::text IS NULL OR f.name = $4) \
           AND ($5::text IS NULL OR f.outcome = $5) \
         ORDER BY f.seq LIMIT $6",
    )
    .bind(workspace.as_uuid())
    .bind(after)
    .bind(filter.source)
    .bind(filter.name)
    .bind(filter.outcome)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| FeedItem {
            seq: row.seq,
            at: row.at,
            source: row.source,
            name: row.name,
            outcome: row.outcome,
            reasons: row.reasons.0,
            reason: row.reason,
            properties: match row.properties {
                Value::Object(map) => map,
                _ => Map::new(),
            },
            schema: row
                .schema
                .and_then(|schema| serde_json::from_value(schema).ok()),
            event_id: row.event_id,
            reject_id: row.reject_id,
            trace_id: row.trace_id,
        })
        .collect())
}

/// Deletes feed rows older than [`FEED_RETENTION_MINUTES`] and watch marks that expired more
/// than a minute ago. Returns the feed rows deleted.
pub async fn prune_debugger_feed(pool: &crate::PgPool) -> Result<u64, StoreError> {
    let deleted =
        sqlx::query("DELETE FROM debugger_feed WHERE at < now() - make_interval(mins => $1)")
            .bind(i32::try_from(FEED_RETENTION_MINUTES).unwrap_or(15))
            .execute(pool)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
    sqlx::query("DELETE FROM debugger_watch WHERE until < now() - interval '1 minute'")
        .execute(pool)
        .await
        .map_err(StoreError::query)?;
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn feed(pool: &PgPool, ws: WorkspaceId, name: &str, outcome: &'static str) {
        let mut connection = pool.acquire().await.unwrap();
        let properties = json!({"plan": "pro"}).as_object().cloned().unwrap();
        write_feed(
            &mut connection,
            ws,
            &FeedEntry {
                source: "web",
                name: Some(name),
                outcome,
                reasons: &[],
                reason: None,
                properties: &properties,
                schema_version: None,
                event_id: None,
                reject_id: None,
                trace_id: Uuid::now_v7(),
            },
        )
        .await
        .unwrap();
    }

    #[test]
    fn a_feed_item_debug_output_shows_no_property() {
        let item = FeedItem {
            seq: 3,
            at: OffsetDateTime::UNIX_EPOCH,
            source: "web".into(),
            name: Some("secret-name".into()),
            outcome: "accepted".into(),
            reasons: Vec::new(),
            reason: None,
            properties: json!({"email": "a@example.com"})
                .as_object()
                .cloned()
                .unwrap(),
            schema: None,
            event_id: None,
            reject_id: None,
            trace_id: Uuid::nil(),
        };

        let shown = format!("{item:?}");

        assert!(!shown.contains("a@example.com") && !shown.contains("secret-name"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_listener_names_the_workspace_that_notified(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut listener = listen_for_debugger(&pool).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        notify_feed(&mut connection, ws).await.unwrap();

        let heard = tokio::time::timeout(Duration::from_secs(5), listener.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(heard, Some(ws));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_watch_lasts_thirty_seconds_and_is_per_workspace(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let mut connection = pool.acquire().await.unwrap();
        assert!(!is_watched(&mut connection, one).await.unwrap());

        touch_debugger_watch(&pool, one).await.unwrap();

        assert!(is_watched(&mut connection, one).await.unwrap());
        assert!(!is_watched(&mut connection, two).await.unwrap());
        sqlx::query("UPDATE debugger_watch SET until = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(!is_watched(&mut connection, one).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_feed_reads_in_order_after_a_cursor_with_filters_and_a_limit(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        feed(&pool, ws, "a", "accepted").await;
        feed(&pool, ws, "b", "rejected").await;
        feed(&pool, ws, "a", "flagged").await;

        let all = read_debugger_feed(&pool, ws, 0, FeedFilter::default(), 10)
            .await
            .unwrap();
        let after = read_debugger_feed(&pool, ws, all[0].seq, FeedFilter::default(), 1)
            .await
            .unwrap();
        let named = FeedFilter {
            name: Some("a"),
            ..FeedFilter::default()
        };
        let by_name = read_debugger_feed(&pool, ws, 0, named, 10).await.unwrap();
        let head = debugger_feed_head(&pool, ws).await.unwrap();

        assert_eq!(
            all.iter()
                .map(|item| item.outcome.as_str())
                .collect::<Vec<_>>(),
            ["accepted", "rejected", "flagged"]
        );
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].outcome, "rejected");
        assert_eq!(by_name.len(), 2);
        assert_eq!(head, all[2].seq);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pruning_deletes_old_rows_and_keeps_recent_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        feed(&pool, ws, "old", "accepted").await;
        feed(&pool, ws, "new", "accepted").await;
        sqlx::query(
            "UPDATE debugger_feed SET at = now() - interval '16 minutes' WHERE name = 'old'",
        )
        .execute(&pool)
        .await
        .unwrap();

        let deleted = prune_debugger_feed(&pool).await.unwrap();

        assert_eq!(deleted, 1);
        let left = read_debugger_feed(&pool, ws, 0, FeedFilter::default(), 10)
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name.as_deref(), Some("new"));
    }
}
