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
use zirv_flow_core::events::{Reason, SchemaProperty, ValidationState};

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

/// The feed outcome of an event stored in `state`.
pub(crate) const fn outcome_label(state: ValidationState) -> &'static str {
    match state {
        ValidationState::Valid => "accepted",
        ValidationState::Flagged => "flagged",
        ValidationState::Unplanned => "unplanned",
    }
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

impl From<FeedRow> for FeedItem {
    fn from(row: FeedRow) -> Self {
        Self {
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
        }
    }
}

/// The workspace's feed rows after `after`, oldest first, at most `limit`, that match
/// `filter`. An index range scan on `seq`.
pub async fn read_debugger_feed<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    after: i64,
    filter: FeedFilter<'_>,
    limit: i64,
) -> Result<Vec<FeedItem>, StoreError> {
    let rows: Vec<FeedRow> = sqlx::query_as(
        "SELECT f.seq, f.at, f.source, f.name, f.outcome, f.reasons, f.reason, f.properties,                 v.properties AS schema, f.event_id, f.reject_id, f.trace_id          FROM debugger_feed f LEFT JOIN event_schema_versions v             ON v.workspace_id = f.workspace_id AND v.name = f.name            AND v.version = f.schema_version          WHERE f.workspace_id = $1 AND f.seq > $2            AND ($3::text IS NULL OR f.source = $3)            AND ($4::text IS NULL OR f.name = $4)            AND ($5::text IS NULL OR f.outcome = $5)          ORDER BY f.seq LIMIT $6",
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
    Ok(rows.into_iter().map(FeedItem::from).collect())
}

/// The workspace's feed rows whose `seq` is in `seqs`, oldest first, that match `filter`:
/// point lookups through the `seq` index, for the numbers a viewer is still waiting for.
pub async fn read_debugger_feed_at<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    seqs: &[i64],
    filter: FeedFilter<'_>,
) -> Result<Vec<FeedItem>, StoreError> {
    let rows: Vec<FeedRow> = sqlx::query_as(
        "SELECT f.seq, f.at, f.source, f.name, f.outcome, f.reasons, f.reason, f.properties,                 v.properties AS schema, f.event_id, f.reject_id, f.trace_id          FROM debugger_feed f LEFT JOIN event_schema_versions v             ON v.workspace_id = f.workspace_id AND v.name = f.name            AND v.version = f.schema_version          WHERE f.seq = ANY($2) AND f.workspace_id = $1            AND ($3::text IS NULL OR f.source = $3)            AND ($4::text IS NULL OR f.name = $4)            AND ($5::text IS NULL OR f.outcome = $5)          ORDER BY f.seq",
    )
    .bind(workspace.as_uuid())
    .bind(seqs)
    .bind(filter.source)
    .bind(filter.name)
    .bind(filter.outcome)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().map(FeedItem::from).collect())
}

/// The numbers in `(after, upto]` that no row of any workspace holds, oldest first, looking
/// at most `limit` numbers down from `upto`. `seq` is one identity shared by all workspaces,
/// so a number another workspace holds is settled; a number nobody holds is a hole (a writer
/// still open, or rolled back). Only numbers leave this query. An index-only probe per number.
pub async fn debugger_feed_missing<'e>(
    executor: impl PgExecutor<'e>,
    after: i64,
    upto: i64,
    limit: i64,
) -> Result<Vec<i64>, StoreError> {
    sqlx::query_scalar(
        "SELECT n FROM generate_series(GREATEST($1 + 1, $2 - $3 + 1), $2) AS n          WHERE NOT EXISTS (SELECT 1 FROM debugger_feed f WHERE f.seq = n)          ORDER BY n",
    )
    .bind(after)
    .bind(upto)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// What one read of a viewer found, taken in one snapshot.
pub struct FeedBatch {
    /// The rows of the `holes` asked about that have committed.
    pub filled: Vec<FeedItem>,
    /// The next rows above `after`.
    pub rows: Vec<FeedItem>,
    /// The numbers no row of any workspace holds between `after` and the last of `rows`.
    pub missing: Vec<i64>,
}

/// One viewer read: the rows of `holes` that committed, the next `limit` rows above `after`,
/// and the numbers missing up to the last of them, all in one repeatable-read snapshot. A
/// number is therefore missing only if no row held it when the rows were read, so a row that
/// commits in between is never counted as a hole and lost. `reach` bounds how far below the
/// last row the missing numbers are examined.
pub async fn read_debugger_batch(
    pool: &crate::PgPool,
    workspace: WorkspaceId,
    after: i64,
    holes: &[i64],
    filter: FeedFilter<'_>,
    limit: i64,
    reach: i64,
) -> Result<FeedBatch, StoreError> {
    let mut snapshot = pool.begin().await.map_err(StoreError::query)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *snapshot)
        .await
        .map_err(StoreError::query)?;
    let filled = if holes.is_empty() {
        Vec::new()
    } else {
        read_debugger_feed_at(&mut *snapshot, workspace, holes, filter).await?
    };
    let rows = read_debugger_feed(&mut *snapshot, workspace, after, filter, limit).await?;
    let missing = match rows.last() {
        Some(last) => debugger_feed_missing(&mut *snapshot, after, last.seq, reach).await?,
        None => Vec::new(),
    };
    Ok(FeedBatch {
        filled,
        rows,
        missing,
    })
}

/// The last `seq` the feed has handed out, of any workspace, or 0 when none was (a fresh or
/// crashed feed). It does not fall when rows are pruned, so a position at or below it is
/// real; one above it can only come from a feed that started over.
pub async fn debugger_feed_position<'e>(executor: impl PgExecutor<'e>) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(pg_sequence_last_value(\
            pg_get_serial_sequence('debugger_feed', 'seq')::regclass), 0)",
    )
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// Where a viewer without a position starts: the highest `seq` whose row is older than
/// `settled`, so rows of writers that were still open a moment ago are still delivered. 0 when
/// no row is that old.
pub async fn debugger_feed_start<'e>(
    executor: impl PgExecutor<'e>,
    settled: std::time::Duration,
) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(max(seq), 0) FROM debugger_feed \
         WHERE at < now() - make_interval(secs => $1)",
    )
    .bind(settled.as_secs_f64())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The name of this life of the feed: a random value kept in an unlogged one-row table, so a
/// crash that empties the feed empties it too and the next call names a new life. It creates
/// the row when it is missing; concurrent callers agree on one value.
pub async fn debugger_epoch<'e>(executor: impl PgExecutor<'e>) -> Result<String, StoreError> {
    sqlx::query_scalar(
        "INSERT INTO debugger_epoch (epoch)          VALUES (left(replace(gen_random_uuid()::text, '-', ''), 12))          ON CONFLICT (singleton) DO UPDATE SET epoch = debugger_epoch.epoch          RETURNING epoch",
    )
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The current epoch without creating one: `None` when the table is empty, as after a crash.
/// An open stream polls this; it writes nothing.
pub async fn debugger_epoch_peek<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<Option<String>, StoreError> {
    sqlx::query_scalar("SELECT epoch FROM debugger_epoch")
        .fetch_optional(executor)
        .await
        .map_err(StoreError::query)
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

    #[test]
    fn a_stored_state_has_a_feed_outcome() {
        assert_eq!(outcome_label(ValidationState::Valid), "accepted");
        assert_eq!(outcome_label(ValidationState::Flagged), "flagged");
        assert_eq!(outcome_label(ValidationState::Unplanned), "unplanned");
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
        let skipping = read_debugger_feed_at(
            &pool,
            ws,
            &[all[1].seq, all[2].seq + 40],
            FeedFilter::default(),
        )
        .await
        .unwrap();
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
        assert_eq!(
            skipping.iter().map(|item| item.seq).collect::<Vec<_>>(),
            [all[1].seq]
        );
        // A number another workspace holds is not the viewer's to read, and not a hole.
        let other = workspace(&pool, "two").await;
        feed(&pool, other, "theirs", "accepted").await;
        let theirs = all[2].seq + 1;
        let theirs_read = read_debugger_feed_at(&pool, ws, &[theirs], FeedFilter::default())
            .await
            .unwrap();
        let position = debugger_feed_position(&pool).await.unwrap();
        assert!(theirs_read.is_empty());
        assert_eq!(position, theirs);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_position_survives_pruning_and_the_start_is_the_newest_settled_row(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        assert_eq!(debugger_feed_position(&pool).await.unwrap(), 0);
        feed(&pool, ws, "old", "accepted").await;
        feed(&pool, ws, "new", "accepted").await;
        sqlx::query("UPDATE debugger_feed SET at = now() - interval '1 minute' WHERE name = 'old'")
            .execute(&pool)
            .await
            .unwrap();

        let start = debugger_feed_start(&pool, std::time::Duration::from_secs(10))
            .await
            .unwrap();
        sqlx::query("DELETE FROM debugger_feed")
            .execute(&pool)
            .await
            .unwrap();
        let position = debugger_feed_position(&pool).await.unwrap();
        let nothing_settled = debugger_feed_start(&pool, std::time::Duration::from_secs(10))
            .await
            .unwrap();

        assert_eq!(start, 1, "only the old row is settled");
        assert_eq!(position, 2, "pruning does not move the position back");
        assert_eq!(nothing_settled, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_epoch_is_stable_until_the_table_is_emptied_as_a_crash_does(pool: PgPool) {
        let first = debugger_epoch(&pool).await.unwrap();
        let again = debugger_epoch(&pool).await.unwrap();
        sqlx::query("TRUNCATE debugger_epoch")
            .execute(&pool)
            .await
            .unwrap();

        let after_crash = debugger_epoch(&pool).await.unwrap();

        assert_eq!(first, again);
        assert_eq!(first.len(), 12);
        assert_ne!(first, after_crash);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn missing_numbers_are_those_no_workspace_holds_within_the_limit(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        feed(&pool, one, "a", "accepted").await;
        feed(&pool, two, "b", "accepted").await;
        feed(&pool, one, "c", "accepted").await;
        sqlx::query("DELETE FROM debugger_feed WHERE seq = 2")
            .execute(&pool)
            .await
            .unwrap();
        // A rolled-back writer leaves a number nobody holds.
        let mut writer = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO debugger_feed (workspace_id, source, outcome, trace_id)              VALUES ($1, 'web', 'accepted', gen_random_uuid())",
        )
        .bind(one.as_uuid())
        .execute(&mut *writer)
        .await
        .unwrap();
        writer.rollback().await.unwrap();
        feed(&pool, two, "d", "accepted").await;

        let all = debugger_feed_missing(&pool, 0, 5, 100).await.unwrap();
        let limited = debugger_feed_missing(&pool, 0, 5, 2).await.unwrap();
        let above = debugger_feed_missing(&pool, 2, 5, 100).await.unwrap();

        assert_eq!(all, [2, 4]);
        assert_eq!(limited, [4]);
        assert_eq!(above, [4]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn both_parts_of_a_viewers_read_are_index_scans(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        feed(&pool, ws, "a", "accepted").await;
        // The tables are tiny, so the planner would scan them; forbidding a sequential scan
        // shows the indexes serve the reads.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let range: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT f.seq FROM debugger_feed f              WHERE f.workspace_id = $1 AND f.seq > 10 ORDER BY f.seq LIMIT 200",
        )
        .bind(ws.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let points: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT f.seq FROM debugger_feed f              WHERE f.seq = ANY($2) AND f.workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .bind(vec![1_i64, 2, 3])
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let missing: Vec<String> = sqlx::query_scalar(
            "EXPLAIN SELECT n FROM generate_series(1::bigint, 100::bigint) AS n              WHERE NOT EXISTS (SELECT 1 FROM debugger_feed f WHERE f.seq = n)",
        )
        .fetch_all(&mut *tx)
        .await
        .unwrap();

        for plan in [range, points, missing] {
            let plan = plan.join(
                "
",
            );
            assert!(plan.contains("Index"), "{plan}");
            assert!(!plan.contains("Seq Scan on debugger_feed"), "{plan}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_epoch_is_one_value_for_connections_that_start_it_together(pool: PgPool) {
        let mut starters = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            starters.push(tokio::spawn(async move { debugger_epoch(&pool).await }));
        }
        let mut epochs = Vec::new();
        for starter in starters {
            epochs.push(starter.await.unwrap().unwrap());
        }

        assert!(epochs.iter().all(|epoch| *epoch == epochs[0]), "{epochs:?}");
        assert_eq!(
            debugger_epoch_peek(&pool).await.unwrap().as_deref(),
            Some(epochs[0].as_str())
        );
        sqlx::query("TRUNCATE debugger_epoch")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(debugger_epoch_peek(&pool).await.unwrap(), None);
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
