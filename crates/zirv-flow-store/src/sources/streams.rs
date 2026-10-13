//! The streams of a source: what discovery found and what the user chose to sync. Every function
//! is scoped to one workspace.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A stream of a source.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Stream {
    pub id: Uuid,
    pub source_id: Uuid,
    pub name: String,
    pub enabled: bool,
    /// `full_refresh` or `incremental`.
    pub sync_mode: String,
    pub cursor_field: Option<String>,
    pub primary_key: Vec<String>,
    pub mapping: Option<Value>,
    /// Where the next incremental run resumes; none for a full refresh or before any batch.
    pub checkpoint: Option<Value>,
    pub staleness_threshold_seconds: Option<i32>,
    /// The UTC cron of the stream's reconciliation; none for no schedule. Incremental only.
    pub reconcile_schedule: Option<String>,
    /// The largest share of the stream's live records one sweep may remove, above 0 and at
    /// most 1. A sweep over it removes nothing and fails the run.
    pub sweep_limit: f64,
    /// The descriptor discovery last reported.
    pub discovered: Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`Stream`].
macro_rules! stream_columns {
    () => {
        "id, source_id, name, enabled, sync_mode, cursor_field, primary_key, mapping, \
         checkpoint, staleness_threshold_seconds, reconcile_schedule, sweep_limit, discovered, \
         created_at, updated_at"
    };
}

/// A stream discovery reported. A name not seen before is stored disabled with these defaults;
/// a known one only has its `descriptor` refreshed.
#[derive(Debug)]
pub struct DiscoveredStream<'a> {
    pub name: &'a str,
    pub sync_mode: &'a str,
    pub cursor_field: Option<&'a str>,
    pub primary_key: &'a [String],
    pub descriptor: &'a Value,
}

/// What the user chose for a stream. Replaces the previous settings whole.
#[derive(Debug)]
pub struct StreamSettings<'a> {
    pub enabled: bool,
    pub sync_mode: &'a str,
    pub cursor_field: Option<&'a str>,
    pub primary_key: &'a [String],
    pub staleness_threshold_seconds: Option<i32>,
    pub reconcile_schedule: Option<&'a str>,
    pub sweep_limit: f64,
}

/// Stores what discovery reported for a source of the workspace: new names are inserted
/// disabled, known names get a fresh `discovered` snapshot, and the settings the user chose are
/// never touched. Returns every stream of the source, or `None` when the workspace has no such
/// source. The caller holds a transaction so a failure stores nothing.
pub async fn upsert_discovered_streams(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    found: &[DiscoveredStream<'_>],
) -> Result<Option<Vec<Stream>>, StoreError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sources WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !exists {
        return Ok(None);
    }
    for stream in found {
        sqlx::query(
            "INSERT INTO streams (id, workspace_id, source_id, name, sync_mode, cursor_field, \
                primary_key, discovered) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (source_id, name) \
             DO UPDATE SET discovered = EXCLUDED.discovered, updated_at = now()",
        )
        .bind(Uuid::now_v7())
        .bind(workspace.as_uuid())
        .bind(source_id)
        .bind(stream.name)
        .bind(stream.sync_mode)
        .bind(stream.cursor_field)
        .bind(stream.primary_key)
        .bind(stream.descriptor)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    Ok(Some(
        list_streams(&mut *connection, workspace, source_id).await?,
    ))
}

/// The streams of a source, ordered by name.
pub async fn list_streams<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
) -> Result<Vec<Stream>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        stream_columns!(),
        " FROM streams WHERE workspace_id = $1 AND source_id = $2 ORDER BY name"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn find_stream<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    name: &str,
) -> Result<Option<Stream>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        stream_columns!(),
        " FROM streams WHERE workspace_id = $1 AND source_id = $2 AND name = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(name)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn find_stream_by_id<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    id: Uuid,
) -> Result<Option<Stream>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        stream_columns!(),
        " FROM streams WHERE workspace_id = $1 AND source_id = $2 AND id = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Replaces the settings of a stream; `None` when the source has no such stream. The database
/// refuses an incremental stream with no cursor field. A change of the sync mode, the cursor
/// field or the primary key clears the stream's checkpoint, which belongs to the old ones; any
/// other change keeps it.
///
/// Such a change is refused with [`StoreError::is_run_active`], and nothing is written, while
/// the stream has a `queued` or `running` sync run: that run would write a checkpoint of the
/// old cursor back, and a retried attempt would resume from it. The stream row is locked first,
/// so a run cannot be queued between the check and the commit: queueing a run inserts a row that
/// refers to the stream, and that waits for this lock. A run reads the stream's settings only
/// when its job starts, after the run row is committed, so a run queued after this commit sees
/// the new settings.
pub async fn configure_stream(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    name: &str,
    settings: &StreamSettings<'_>,
) -> Result<Option<Stream>, StoreError> {
    let mut tx = connection.begin().await.map_err(StoreError::query)?;
    let current: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT id, (sync_mode IS DISTINCT FROM $4 OR cursor_field IS DISTINCT FROM $5 \
                     OR primary_key IS DISTINCT FROM $6) \
         FROM streams WHERE workspace_id = $1 AND source_id = $2 AND name = $3 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(name)
    .bind(settings.sync_mode)
    .bind(settings.cursor_field)
    .bind(settings.primary_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    let Some((stream_id, cursor_changes)) = current else {
        return Ok(None);
    };
    if cursor_changes && has_active_run(&mut tx, stream_id).await? {
        return Err(StoreError::run_active());
    }
    let stream = sqlx::query_as(concat!(
        "UPDATE streams SET enabled = $4, sync_mode = $5, cursor_field = $6, primary_key = $7, \
            staleness_threshold_seconds = $8, reconcile_schedule = $9, sweep_limit = $10, \
            updated_at = now(), \
            checkpoint = CASE WHEN sync_mode IS DISTINCT FROM $5 \
                               OR cursor_field IS DISTINCT FROM $6 \
                               OR primary_key IS DISTINCT FROM $7 \
                         THEN NULL ELSE checkpoint END \
         WHERE workspace_id = $1 AND source_id = $2 AND name = $3 RETURNING ",
        stream_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(name)
    .bind(settings.enabled)
    .bind(settings.sync_mode)
    .bind(settings.cursor_field)
    .bind(settings.primary_key)
    .bind(settings.staleness_threshold_seconds)
    .bind(settings.reconcile_schedule)
    .bind(settings.sweep_limit)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    tx.commit().await.map_err(StoreError::query)?;
    Ok(stream)
}

/// Whether the stream has a sync run that is queued or running.
async fn has_active_run(
    connection: &mut PgConnection,
    stream_id: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sync_runs \
                        WHERE stream_id = $1 AND state IN ('queued', 'running'))",
    )
    .bind(stream_id)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// Sets the mapping of a stream; `None` when the source has no such stream. The caller has
/// checked it against the stream's fields.
pub async fn set_stream_mapping<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    name: &str,
    mapping: &Value,
) -> Result<Option<Stream>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE streams SET mapping = $4, updated_at = now() \
         WHERE workspace_id = $1 AND source_id = $2 AND name = $3 RETURNING ",
        stream_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(name)
    .bind(mapping)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::super::{NewSource, create_source};
    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn source(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let config = json!({});
        create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "crm",
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn found<'a>(name: &'a str, descriptor: &'a Value, pk: &'a [String]) -> DiscoveredStream<'a> {
        DiscoveredStream {
            name,
            sync_mode: "full_refresh",
            cursor_field: None,
            primary_key: pk,
            descriptor,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn discovery_inserts_new_streams_disabled_and_keeps_the_users_settings(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let source_id = source(&pool, ws).await;
        let pk = vec!["id".to_owned()];
        let first = json!({"v": 1});
        let mut tx = begin(&pool).await.unwrap();
        let streams = upsert_discovered_streams(
            &mut tx,
            ws,
            source_id,
            &[found("a", &first, &pk), found("b", &first, &pk)],
        )
        .await
        .unwrap()
        .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(streams.len(), 2);
        assert!(streams.iter().all(|stream| !stream.enabled));
        assert_eq!(streams[0].primary_key, pk);

        let chosen = configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source_id,
            "a",
            &StreamSettings {
                reconcile_schedule: None,
                sweep_limit: 0.5,
                enabled: true,
                sync_mode: "incremental",
                cursor_field: Some("updated_at"),
                primary_key: &pk,
                staleness_threshold_seconds: Some(3600),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(chosen.enabled);

        let second = json!({"v": 2});
        let mut tx = begin(&pool).await.unwrap();
        let again = upsert_discovered_streams(
            &mut tx,
            ws,
            source_id,
            &[found("a", &second, &pk), found("c", &second, &pk)],
        )
        .await
        .unwrap()
        .unwrap();
        commit(tx).await.unwrap();

        let names: Vec<_> = again.iter().map(|stream| stream.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        let a = &again[0];
        assert_eq!(a.discovered, second);
        assert!(a.enabled);
        assert_eq!(a.sync_mode, "incremental");
        assert_eq!(a.cursor_field.as_deref(), Some("updated_at"));
        assert_eq!(a.staleness_threshold_seconds, Some(3600));
        assert_eq!(a.id, chosen.id);
        assert_eq!(again[1].discovered, first);
        assert!(!again[2].enabled);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_refuses_incremental_without_a_cursor_and_a_zero_threshold(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let source_id = source(&pool, ws).await;
        let pk = vec![];
        let descriptor = json!({});
        let mut tx = begin(&pool).await.unwrap();
        upsert_discovered_streams(&mut tx, ws, source_id, &[found("a", &descriptor, &pk)])
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let settings = |sync_mode, cursor_field, staleness| StreamSettings {
            reconcile_schedule: None,
            sweep_limit: 0.5,
            enabled: true,
            sync_mode,
            cursor_field,
            primary_key: &[],
            staleness_threshold_seconds: staleness,
        };

        let no_cursor = configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source_id,
            "a",
            &settings("incremental", None, None),
        )
        .await;
        let zero = configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source_id,
            "a",
            &settings("full_refresh", None, Some(0)),
        )
        .await;
        let unknown = configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source_id,
            "a",
            &settings("sometimes", None, None),
        )
        .await;

        assert!(no_cursor.is_err());
        assert!(zero.is_err());
        assert!(unknown.is_err());
        let stream = find_stream(&pool, ws, source_id, "a")
            .await
            .unwrap()
            .unwrap();
        assert!(!stream.enabled);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn streams_go_with_their_source_and_a_foreign_source_stores_nothing(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let source_id = source(&pool, one).await;
        let pk = vec![];
        let descriptor = json!({});
        let mut tx = begin(&pool).await.unwrap();
        let foreign =
            upsert_discovered_streams(&mut tx, two, source_id, &[found("a", &descriptor, &pk)])
                .await
                .unwrap();
        upsert_discovered_streams(&mut tx, one, source_id, &[found("a", &descriptor, &pk)])
            .await
            .unwrap();
        commit(tx).await.unwrap();
        assert!(foreign.is_none());

        let mut connection = pool.acquire().await.unwrap();
        super::super::delete_source(&mut connection, one, source_id)
            .await
            .unwrap()
            .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM streams")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn neither_table_has_a_person_column(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name IN ('sources', 'streams') \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }

    async fn set_checkpoint(pool: &PgPool) {
        sqlx::query("UPDATE streams SET checkpoint = '{\"offset\": 5}'::jsonb")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn checkpoint_of(pool: &PgPool, ws: WorkspaceId, source_id: Uuid) -> Option<Value> {
        find_stream(pool, ws, source_id, "contacts")
            .await
            .unwrap()
            .unwrap()
            .checkpoint
    }

    async fn configure(
        pool: &PgPool,
        ws: WorkspaceId,
        source_id: Uuid,
        settings: StreamSettings<'_>,
    ) {
        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source_id,
            "contacts",
            &settings,
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_changed_cursor_sync_mode_or_primary_key_clears_the_checkpoint_and_other_changes_keep_it(
        pool: PgPool,
    ) {
        use crate::sync_runs::testing::source_with_stream;

        let ws = workspace(&pool, "one").await;
        let (source_id, _) = source_with_stream(&pool, ws, "crm", "incremental").await;
        let key = vec!["id".to_owned()];
        let settings = |mode, cursor, primary_key, staleness| StreamSettings {
            reconcile_schedule: None,
            sweep_limit: 0.5,
            enabled: true,
            sync_mode: mode,
            cursor_field: cursor,
            primary_key,
            staleness_threshold_seconds: staleness,
        };
        // The first settings are those the fixture saved, so nothing changes.
        set_checkpoint(&pool).await;
        configure(
            &pool,
            ws,
            source_id,
            settings("incremental", Some("updated_at"), &[], Some(60)),
        )
        .await;
        assert!(
            checkpoint_of(&pool, ws, source_id).await.is_some(),
            "a staleness change keeps the cursor"
        );
        configure(
            &pool,
            ws,
            source_id,
            settings("incremental", Some("updated_at"), &[], None),
        )
        .await;
        assert!(
            checkpoint_of(&pool, ws, source_id).await.is_some(),
            "nothing changed"
        );

        configure(
            &pool,
            ws,
            source_id,
            settings("incremental", Some("id"), &[], None),
        )
        .await;
        assert!(
            checkpoint_of(&pool, ws, source_id).await.is_none(),
            "a new cursor field"
        );
        set_checkpoint(&pool).await;
        configure(
            &pool,
            ws,
            source_id,
            settings("incremental", Some("id"), &key, None),
        )
        .await;
        assert!(
            checkpoint_of(&pool, ws, source_id).await.is_none(),
            "a new primary key"
        );
        set_checkpoint(&pool).await;
        configure(
            &pool,
            ws,
            source_id,
            settings("full_refresh", Some("id"), &key, None),
        )
        .await;
        assert!(
            checkpoint_of(&pool, ws, source_id).await.is_none(),
            "a new sync mode"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cursor_change_is_refused_while_a_run_is_active_and_allowed_after(pool: PgPool) {
        use crate::sync_runs::testing::fixture;

        let fx = fixture(&pool, "one", "incremental").await;
        set_checkpoint(&pool).await;
        let key = vec!["id".to_owned()];
        let new_cursor = StreamSettings {
            reconcile_schedule: None,
            sweep_limit: 0.5,
            enabled: true,
            sync_mode: "incremental",
            cursor_field: Some("id"),
            primary_key: &key,
            staleness_threshold_seconds: None,
        };
        let same_cursor = StreamSettings {
            reconcile_schedule: None,
            sweep_limit: 0.5,
            enabled: false,
            sync_mode: "incremental",
            cursor_field: Some("updated_at"),
            primary_key: &[],
            staleness_threshold_seconds: Some(60),
        };

        let refused = configure_stream(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            "contacts",
            &new_cursor,
        )
        .await;
        let allowed = configure_stream(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            "contacts",
            &same_cursor,
        )
        .await;

        assert!(refused.unwrap_err().is_run_active());
        let stream = allowed.unwrap().unwrap();
        assert_eq!(stream.cursor_field.as_deref(), Some("updated_at"));
        assert!(
            !stream.enabled && stream.checkpoint.is_some(),
            "other changes stay allowed"
        );
        sqlx::query("UPDATE sync_runs SET state = 'cancelled'")
            .execute(&pool)
            .await
            .unwrap();
        let after = configure_stream(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            "contacts",
            &new_cursor,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(after.cursor_field.as_deref(), Some("id"));
        assert_eq!(after.checkpoint, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reconcile_schedule_and_the_sweep_limit_are_stored_replaced_and_checked(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let source_id = source(&pool, ws).await;
        let pk = vec![];
        let descriptor = json!({});
        let mut tx = begin(&pool).await.unwrap();
        upsert_discovered_streams(&mut tx, ws, source_id, &[found("a", &descriptor, &pk)])
            .await
            .unwrap();
        commit(tx).await.unwrap();
        let fresh = find_stream(&pool, ws, source_id, "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (fresh.reconcile_schedule.clone(), fresh.sweep_limit),
            (None, 0.5)
        );
        let settings = |schedule, limit| StreamSettings {
            reconcile_schedule: schedule,
            sweep_limit: limit,
            enabled: true,
            sync_mode: "incremental",
            cursor_field: Some("updated_at"),
            primary_key: &[],
            staleness_threshold_seconds: None,
        };
        let configure = |schedule, limit| {
            let pool = pool.clone();
            async move {
                configure_stream(
                    &mut pool.acquire().await.unwrap(),
                    ws,
                    source_id,
                    "a",
                    &settings(schedule, limit),
                )
                .await
            }
        };

        let set = configure(Some("0 3 * * *"), 0.25).await.unwrap().unwrap();
        assert_eq!(
            (set.reconcile_schedule.as_deref(), set.sweep_limit),
            (Some("0 3 * * *"), 0.25)
        );
        let cleared = configure(None, 1.0).await.unwrap().unwrap();
        assert_eq!(
            (cleared.reconcile_schedule, cleared.sweep_limit),
            (None, 1.0)
        );

        for bad in [0.0, -0.1, 1.5, f64::NAN] {
            assert!(configure(None, bad).await.is_err(), "{bad}");
        }
        let stored = find_stream(&pool, ws, source_id, "a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.sweep_limit, 1.0);
    }
}
