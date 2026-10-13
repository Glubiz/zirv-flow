//! The streams of a source: what discovery found and what the user chose to sync. Every function
//! is scoped to one workspace.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
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
    pub staleness_threshold_seconds: Option<i32>,
    /// The descriptor discovery last reported.
    pub discovered: Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`Stream`].
macro_rules! stream_columns {
    () => {
        "id, source_id, name, enabled, sync_mode, cursor_field, primary_key, mapping, \
         staleness_threshold_seconds, discovered, created_at, updated_at"
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

/// Replaces the settings of a stream; `None` when the source has no such stream. The database
/// refuses an incremental stream with no cursor field.
pub async fn configure_stream<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    name: &str,
    settings: &StreamSettings<'_>,
) -> Result<Option<Stream>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE streams SET enabled = $4, sync_mode = $5, cursor_field = $6, primary_key = $7, \
            staleness_threshold_seconds = $8, updated_at = now() \
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
            &pool,
            ws,
            source_id,
            "a",
            &StreamSettings {
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
            enabled: true,
            sync_mode,
            cursor_field,
            primary_key: &[],
            staleness_threshold_seconds: staleness,
        };

        let no_cursor = configure_stream(
            &pool,
            ws,
            source_id,
            "a",
            &settings("incremental", None, None),
        )
        .await;
        let zero = configure_stream(
            &pool,
            ws,
            source_id,
            "a",
            &settings("full_refresh", None, Some(0)),
        )
        .await;
        let unknown = configure_stream(
            &pool,
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
}
