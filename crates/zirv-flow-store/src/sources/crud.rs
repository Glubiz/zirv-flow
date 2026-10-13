//! Creating, reading, changing and deleting sources. Every function is scoped to one workspace.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{Acquire, PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{SecretId, UserId, WorkspaceId};

use crate::{StoreError, secrets};

/// A configured source. The credential is a reference to a sealed secret, never a value.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Source {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub config: Value,
    pub credential_secret_id: Option<Uuid>,
    /// The sealed OAuth2 tokens of the source (purpose `oauth_token`), once an admin has
    /// connected an account. A reference, never a value.
    pub oauth_secret_id: Option<Uuid>,
    pub oauth_connected_at: Option<OffsetDateTime>,
    /// The names of the credential fields that are set; never their values.
    pub credential_fields: Vec<String>,
    pub schedule: Option<String>,
    pub enabled: bool,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`Source`].
macro_rules! source_columns {
    () => {
        "id, kind, name, config, credential_secret_id, oauth_secret_id, oauth_connected_at, \
         credential_fields, schedule, enabled, created_by, created_at, updated_at"
    };
}

/// A source to create. The secret named by `credential_secret_id` must already exist in the
/// same transaction.
#[derive(Debug)]
pub struct NewSource<'a> {
    pub id: Uuid,
    pub kind: &'a str,
    pub name: &'a str,
    pub config: &'a Value,
    pub credential_secret_id: Option<SecretId>,
    pub credential_fields: &'a [String],
    pub schedule: Option<&'a str>,
    pub created_by: Option<UserId>,
}

/// The fields of a source that can change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct SourceUpdate<'a> {
    pub name: Option<&'a str>,
    pub config: Option<&'a Value>,
    /// `Some(None)` clears the schedule.
    pub schedule: Option<Option<&'a str>>,
    pub enabled: Option<bool>,
    /// Links a credential secret the source had none of.
    pub credential_secret_id: Option<SecretId>,
    /// Replaces the names of the credential fields that are set.
    pub credential_fields: Option<&'a [String]>,
}

/// Creates a source. A name already taken in the workspace is a conflict.
pub async fn create_source<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewSource<'_>,
) -> Result<Source, StoreError> {
    sqlx::query_as(concat!(
        "INSERT INTO sources (id, workspace_id, kind, name, config, credential_secret_id, \
            credential_fields, schedule, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING ",
        source_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.kind)
    .bind(new.name)
    .bind(new.config)
    .bind(new.credential_secret_id.map(|id| id.as_uuid()))
    .bind(new.credential_fields)
    .bind(new.schedule)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's sources, ordered by name.
pub async fn list_sources<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<Source>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        source_columns!(),
        " FROM sources WHERE workspace_id = $1 ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn find_source<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Source>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        source_columns!(),
        " FROM sources WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Reads a source and locks its row until the transaction ends, so concurrent changes to it
/// (a credential replaced twice, say) run one after the other. `None` when there is no such
/// source.
pub async fn find_source_for_update(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Source>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        source_columns!(),
        " FROM sources WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)
}

/// Changes the given fields. `None` when there is no such source; a name taken by another
/// source is a conflict. A changed `config` clears the checkpoints of the source's streams: a
/// new database or URL invalidates a cursor.
///
/// A changed `config` is refused with [`StoreError::is_run_active`], and nothing is written,
/// while any stream of the source has a `queued` or `running` sync run, which would write an
/// old-cursor checkpoint back. The streams are locked first, so no run is queued between the
/// check and the commit (see [`configure_stream`](super::configure_stream)); a run queued
/// after it reads the new `config` when its job starts.
pub async fn update_source(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    update: &SourceUpdate<'_>,
) -> Result<Option<Source>, StoreError> {
    let mut tx = connection.begin().await.map_err(StoreError::query)?;
    if let Some(config) = update.config {
        sqlx::query(
            "SELECT id FROM streams WHERE workspace_id = $1 AND source_id = $2 \
             ORDER BY id FOR UPDATE",
        )
        .bind(workspace.as_uuid())
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM sync_runs \
                            WHERE workspace_id = $1 AND source_id = $2 \
                              AND state IN ('queued', 'running')) \
                    AND $3::jsonb IS DISTINCT FROM (SELECT config FROM sources \
                                                    WHERE workspace_id = $1 AND id = $2)",
        )
        .bind(workspace.as_uuid())
        .bind(id)
        .bind(config)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        if blocked {
            return Err(StoreError::run_active());
        }
    }
    let source = sqlx::query_as(concat!(
        "WITH cleared AS ( \
            UPDATE streams SET checkpoint = NULL \
            WHERE workspace_id = $1 AND source_id = $2 AND checkpoint IS NOT NULL \
              AND $4::jsonb IS NOT NULL \
              AND $4::jsonb IS DISTINCT FROM (SELECT config FROM sources \
                                              WHERE workspace_id = $1 AND id = $2)) \
         UPDATE sources SET name = COALESCE($3, name), config = COALESCE($4, config), \
            schedule = CASE WHEN $5 THEN $6 ELSE schedule END, \
            enabled = COALESCE($7, enabled), \
            credential_secret_id = COALESCE($8, credential_secret_id), \
            credential_fields = COALESCE($9, credential_fields), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        source_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.name)
    .bind(update.config)
    .bind(update.schedule.is_some())
    .bind(update.schedule.flatten())
    .bind(update.enabled)
    .bind(update.credential_secret_id.map(|id| id.as_uuid()))
    .bind(update.credential_fields)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    tx.commit().await.map_err(StoreError::query)?;
    Ok(source)
}

/// Deletes a source with its streams, its credential secret and its token secret, and returns
/// what was deleted. `None` when there is no such source. The caller holds a transaction, so the source and its
/// secret go together.
pub async fn delete_source(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Source>, StoreError> {
    let deleted: Option<Source> = sqlx::query_as(concat!(
        "DELETE FROM sources WHERE workspace_id = $1 AND id = $2 RETURNING ",
        source_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if let Some(secret) = deleted
        .as_ref()
        .and_then(|source| source.credential_secret_id)
    {
        secrets::delete(&mut *connection, workspace, SecretId::from_uuid(secret)).await?;
    }
    if let Some(secret) = deleted.as_ref().and_then(|source| source.oauth_secret_id) {
        secrets::delete(&mut *connection, workspace, SecretId::from_uuid(secret)).await?;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{
        Keyring, MasterKey, SecretPurpose, Slug, WorkspaceName, seal, secret_aad,
    };

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn new<'a>(name: &'a str, config: &'a Value) -> NewSource<'a> {
        NewSource {
            id: Uuid::now_v7(),
            kind: "fake",
            name,
            config,
            credential_secret_id: None,
            credential_fields: &[],
            schedule: None,
            created_by: None,
        }
    }

    async fn secret(pool: &PgPool, ws: WorkspaceId) -> SecretId {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = SecretId::new();
        let purpose = SecretPurpose::ConnectorCredential;
        let sealed = seal(&keys, b"{}", &secret_aad(ws, id, purpose));
        secrets::insert_unchecked(pool, ws, purpose, id, &sealed)
            .await
            .unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_source_is_created_read_listed_updated_and_deleted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let config = json!({"streams": 2});

        let created = create_source(&pool, ws, &new("Beta", &config))
            .await
            .unwrap();
        create_source(&pool, ws, &new("alpha", &config))
            .await
            .unwrap();

        assert_eq!(created.config, config);
        assert!(created.enabled);
        assert_eq!(
            find_source(&pool, ws, created.id).await.unwrap().unwrap(),
            created
        );
        let names: Vec<_> = list_sources(&pool, ws)
            .await
            .unwrap()
            .into_iter()
            .map(|source| source.name)
            .collect();
        assert_eq!(names, ["alpha", "Beta"]);

        let more = json!({"streams": 3});
        let updated = update_source(
            &mut pool.acquire().await.unwrap(),
            ws,
            created.id,
            &SourceUpdate {
                name: Some("Gamma"),
                config: Some(&more),
                schedule: Some(Some("0 * * * *")),
                enabled: Some(false),
                ..SourceUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "Gamma");
        assert_eq!(updated.config, more);
        assert_eq!(updated.schedule.as_deref(), Some("0 * * * *"));
        assert!(!updated.enabled);
        let untouched = update_source(
            &mut pool.acquire().await.unwrap(),
            ws,
            created.id,
            &SourceUpdate::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(untouched.schedule.as_deref(), Some("0 * * * *"));
        let cleared = update_source(
            &mut pool.acquire().await.unwrap(),
            ws,
            created.id,
            &SourceUpdate {
                schedule: Some(None),
                ..SourceUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(cleared.schedule, None);

        let secret_id = secret(&pool, ws).await;
        let fields = vec!["token".to_owned()];
        let linked = update_source(
            &mut pool.acquire().await.unwrap(),
            ws,
            created.id,
            &SourceUpdate {
                credential_secret_id: Some(secret_id),
                credential_fields: Some(&fields),
                ..SourceUpdate::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(linked.credential_secret_id, Some(secret_id.as_uuid()));
        assert_eq!(linked.credential_fields, fields);
        let kept = update_source(
            &mut pool.acquire().await.unwrap(),
            ws,
            created.id,
            &SourceUpdate::default(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(kept.credential_fields, fields);

        let mut tx = begin(&pool).await.unwrap();
        assert!(
            delete_source(&mut tx, ws, created.id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            delete_source(&mut tx, ws, created.id)
                .await
                .unwrap()
                .is_none()
        );
        commit(tx).await.unwrap();
        assert!(find_source(&pool, ws, created.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_name_is_unique_per_workspace_only(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let config = json!({});
        create_source(&pool, one, &new("crm", &config))
            .await
            .unwrap();

        let duplicate = create_source(&pool, one, &new("crm", &config)).await;
        let elsewhere = create_source(&pool, two, &new("crm", &config)).await;

        assert!(duplicate.unwrap_err().is_conflict());
        assert!(elsewhere.is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_source_deletes_its_secret_in_the_same_transaction(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let secret_id = secret(&pool, ws).await;
        let config = json!({});
        let source = create_source(
            &pool,
            ws,
            &NewSource {
                credential_secret_id: Some(secret_id),
                ..new("crm", &config)
            },
        )
        .await
        .unwrap();

        let mut tx = begin(&pool).await.unwrap();
        delete_source(&mut tx, ws, source.id)
            .await
            .unwrap()
            .unwrap();
        // Not yet committed: a rollback keeps both.
        drop(tx);
        assert!(secrets::get(&pool, ws, secret_id).await.unwrap().is_some());
        assert!(find_source(&pool, ws, source.id).await.unwrap().is_some());

        let mut tx = begin(&pool).await.unwrap();
        delete_source(&mut tx, ws, source.id)
            .await
            .unwrap()
            .unwrap();
        commit(tx).await.unwrap();
        assert!(secrets::get(&pool, ws, secret_id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_workspace_deletes_its_sources(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let config = json!({});
        create_source(&pool, ws, &new("crm", &config))
            .await
            .unwrap();

        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_locked_source_makes_a_second_locker_wait_until_the_first_commits(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let config = json!({});
        let source = create_source(&pool, ws, &new("crm", &config))
            .await
            .unwrap();
        let mut first = begin(&pool).await.unwrap();
        find_source_for_update(&mut first, ws, source.id)
            .await
            .unwrap()
            .unwrap();

        let second_pool = pool.clone();
        let second = tokio::spawn(async move {
            let mut tx = begin(&second_pool).await.unwrap();
            let found = find_source_for_update(&mut tx, ws, source.id).await;
            found.unwrap().is_some()
        });
        let early = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            while !second.is_finished() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(early.is_err(), "the second locker did not wait");

        commit(first).await.unwrap();
        assert!(second.await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_store_refuses_a_secret_or_a_source_of_another_workspace(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let foreign_secret = secret(&pool, two).await;
        let config = json!({});

        let created = create_source(
            &pool,
            one,
            &NewSource {
                credential_secret_id: Some(foreign_secret),
                ..new("crm", &config)
            },
        )
        .await;
        let own = create_source(&pool, one, &new("erp", &config))
            .await
            .unwrap();
        let linked = update_source(
            &mut pool.acquire().await.unwrap(),
            one,
            own.id,
            &SourceUpdate {
                credential_secret_id: Some(foreign_secret),
                ..SourceUpdate::default()
            },
        )
        .await;
        let stream = sqlx::query(
            "INSERT INTO streams (id, workspace_id, source_id, name, sync_mode, discovered) \
             VALUES ($1, $2, $3, 'a', 'full_refresh', '{}')",
        )
        .bind(Uuid::now_v7())
        .bind(two.as_uuid())
        .bind(own.id)
        .execute(&pool)
        .await;

        assert!(created.is_err());
        assert!(linked.is_err());
        assert!(stream.is_err());
    }

    async fn set_checkpoint(pool: &PgPool) {
        sqlx::query("UPDATE streams SET checkpoint = '{\"offset\": 5}'::jsonb")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn kept(pool: &PgPool) -> bool {
        sqlx::query_scalar("SELECT checkpoint IS NOT NULL FROM streams")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn update(pool: &PgPool, ws: WorkspaceId, source_id: Uuid, change: SourceUpdate<'_>) {
        update_source(&mut pool.acquire().await.unwrap(), ws, source_id, &change)
            .await
            .unwrap()
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_changed_config_clears_the_checkpoints_of_the_streams_and_other_changes_keep_them(
        pool: PgPool,
    ) {
        use crate::sync_runs::testing::source_with_stream;

        let ws = workspace(&pool, "one").await;
        let (source_id, _) = source_with_stream(&pool, ws, "crm", "incremental").await;
        let config = json!({"records": 3});
        let same = json!({});

        set_checkpoint(&pool).await;
        update(
            &pool,
            ws,
            source_id,
            SourceUpdate {
                name: Some("renamed"),
                enabled: Some(false),
                ..SourceUpdate::default()
            },
        )
        .await;
        assert!(kept(&pool).await, "a rename keeps the cursor");
        update(
            &pool,
            ws,
            source_id,
            SourceUpdate {
                config: Some(&same),
                ..SourceUpdate::default()
            },
        )
        .await;
        assert!(kept(&pool).await, "the same config keeps the cursor");
        update(
            &pool,
            ws,
            source_id,
            SourceUpdate {
                config: Some(&config),
                ..SourceUpdate::default()
            },
        )
        .await;
        assert!(!kept(&pool).await, "a new config clears it");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_config_change_is_refused_while_any_stream_has_an_active_run(pool: PgPool) {
        use crate::sync_runs::testing::fixture;

        let fx = fixture(&pool, "one", "incremental").await;
        set_checkpoint(&pool).await;
        let config = json!({"records": 3});
        let same = json!({});

        let refused = update_source(
            &mut pool.acquire().await.unwrap(),
            fx.ws,
            fx.source_id,
            &SourceUpdate {
                config: Some(&config),
                ..SourceUpdate::default()
            },
        )
        .await;
        update(
            &pool,
            fx.ws,
            fx.source_id,
            SourceUpdate {
                name: Some("renamed"),
                config: Some(&same),
                schedule: Some(Some("0 * * * *")),
                ..SourceUpdate::default()
            },
        )
        .await;

        assert!(refused.unwrap_err().is_run_active());
        assert!(
            kept(&pool).await,
            "nothing was written, and other changes stay allowed"
        );
        let stored: Value = sqlx::query_scalar("SELECT config FROM sources")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, same);
        sqlx::query("UPDATE sync_runs SET state = 'failed'")
            .execute(&pool)
            .await
            .unwrap();
        update(
            &pool,
            fx.ws,
            fx.source_id,
            SourceUpdate {
                config: Some(&config),
                ..SourceUpdate::default()
            },
        )
        .await;
        assert!(!kept(&pool).await);
    }
}
