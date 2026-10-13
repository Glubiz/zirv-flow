//! Creating, reading, changing and deleting sources. Every function is scoped to one workspace.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
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
        "id, kind, name, config, credential_secret_id, credential_fields, schedule, enabled, \
         created_by, created_at, updated_at"
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

/// Changes the given fields. `None` when there is no such source; a name taken by another
/// source is a conflict.
pub async fn update_source<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    update: &SourceUpdate<'_>,
) -> Result<Option<Source>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE sources SET name = COALESCE($3, name), config = COALESCE($4, config), \
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
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Deletes a source with its streams and its credential secret, and returns what was deleted.
/// `None` when there is no such source. The caller holds a transaction, so the source and its
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
            &pool,
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
        let untouched = update_source(&pool, ws, created.id, &SourceUpdate::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.schedule.as_deref(), Some("0 * * * *"));
        let cleared = update_source(
            &pool,
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
            &pool,
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
        let kept = update_source(&pool, ws, created.id, &SourceUpdate::default())
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
}
