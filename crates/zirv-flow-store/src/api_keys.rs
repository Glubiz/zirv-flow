//! Workspace API keys. Only the hash of the secret is stored.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{ApiKeyId, Scope, UserId, WorkspaceId};

use crate::StoreError;

macro_rules! api_key_columns {
    () => {
        "id, workspace_id, name, secret_hash, scopes, created_by, created_at, last_used_at, revoked_at"
    };
}

/// An API key's public metadata.
#[derive(Clone, Debug)]
pub struct ApiKey {
    pub id: ApiKeyId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub created_by: Option<UserId>,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
    pub revoked_at: Option<OffsetDateTime>,
}

/// A key together with the hash of its secret, for authentication only.
#[derive(Clone, Debug)]
pub struct StoredApiKey {
    pub key: ApiKey,
    pub secret_hash: Vec<u8>,
}

#[derive(FromRow)]
struct ApiKeyRow {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    secret_hash: Vec<u8>,
    scopes: Vec<String>,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    last_used_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
}

impl From<ApiKeyRow> for StoredApiKey {
    fn from(row: ApiKeyRow) -> Self {
        Self {
            key: ApiKey {
                id: ApiKeyId::from_uuid(row.id),
                workspace_id: WorkspaceId::from_uuid(row.workspace_id),
                name: row.name,
                // An unrecognised scope is dropped, which can only reduce access.
                scopes: row.scopes.iter().filter_map(|s| s.parse().ok()).collect(),
                created_by: row.created_by.map(UserId::from_uuid),
                created_at: row.created_at,
                last_used_at: row.last_used_at,
                revoked_at: row.revoked_at,
            },
            secret_hash: row.secret_hash,
        }
    }
}

pub async fn create_api_key<'e>(
    executor: impl PgExecutor<'e>,
    id: ApiKeyId,
    workspace: WorkspaceId,
    name: &str,
    secret_hash: &[u8],
    scopes: &[Scope],
    created_by: Option<UserId>,
) -> Result<ApiKey, StoreError> {
    let scopes: Vec<&str> = scopes.iter().map(|scope| scope.as_str()).collect();
    let row: ApiKeyRow = sqlx::query_as(concat!(
        "INSERT INTO api_keys (id, workspace_id, name, secret_hash, scopes, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING ",
        api_key_columns!(),
        ""
    ))
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(secret_hash)
    .bind(&scopes)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(StoredApiKey::from(row).key)
}

/// Every key of one workspace, newest first, without secrets.
pub async fn list_api_keys<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<ApiKey>, StoreError> {
    let rows: Vec<ApiKeyRow> = sqlx::query_as(concat!(
        "SELECT ",
        api_key_columns!(),
        " FROM api_keys WHERE workspace_id = $1 ORDER BY created_at DESC, id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|row| StoredApiKey::from(row).key)
        .collect())
}

/// Looks a key up by id, revoked or not, to authenticate a request.
pub async fn find_api_key_for_auth<'e>(
    executor: impl PgExecutor<'e>,
    id: ApiKeyId,
) -> Result<Option<StoredApiKey>, StoreError> {
    let row: Option<ApiKeyRow> = sqlx::query_as(concat!(
        "SELECT ",
        api_key_columns!(),
        " FROM api_keys WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// Revokes a key of the given workspace. Returns whether a live key was revoked; a key of
/// another workspace is never touched.
pub async fn revoke_api_key<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: ApiKeyId,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE api_keys SET revoked_at = now() \
         WHERE id = $1 AND workspace_id = $2 AND revoked_at IS NULL",
    )
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Records use of a key, at most once a minute.
pub async fn touch_api_key<'e>(
    executor: impl PgExecutor<'e>,
    id: ApiKeyId,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE api_keys SET last_used_at = now() \
         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
    )
    .bind(id.as_uuid())
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(name).unwrap();
        create_workspace(pool, &name, &Slug::from_name(&name).unwrap())
            .await
            .unwrap()
            .id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_is_stored_with_its_scopes_and_found_for_auth(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let id = ApiKeyId::new();

        let key = create_api_key(
            &pool,
            id,
            ws,
            "ci",
            b"hash",
            &[Scope::Read, Scope::Write],
            None,
        )
        .await
        .unwrap();
        let stored = find_api_key_for_auth(&pool, id).await.unwrap().unwrap();

        assert_eq!(key.scopes, [Scope::Read, Scope::Write]);
        assert_eq!(stored.secret_hash, b"hash");
        assert_eq!(stored.key.workspace_id, ws);
        assert!(stored.key.revoked_at.is_none());
        assert!(
            find_api_key_for_auth(&pool, ApiKeyId::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn listing_is_scoped_to_one_workspace(pool: PgPool) {
        let a = workspace(&pool, "A").await;
        let b = workspace(&pool, "B").await;
        create_api_key(&pool, ApiKeyId::new(), a, "one", b"h", &[Scope::Read], None)
            .await
            .unwrap();

        assert_eq!(list_api_keys(&pool, a).await.unwrap().len(), 1);
        assert!(list_api_keys(&pool, b).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn revoking_works_once_and_only_within_the_workspace(pool: PgPool) {
        let a = workspace(&pool, "A").await;
        let b = workspace(&pool, "B").await;
        let id = ApiKeyId::new();
        create_api_key(&pool, id, a, "one", b"h", &[Scope::Read], None)
            .await
            .unwrap();

        assert!(!revoke_api_key(&pool, b, id).await.unwrap());
        assert!(revoke_api_key(&pool, a, id).await.unwrap());
        assert!(!revoke_api_key(&pool, a, id).await.unwrap());
        let stored = find_api_key_for_auth(&pool, id).await.unwrap().unwrap();
        assert!(stored.key.revoked_at.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn last_used_is_updated_at_most_once_a_minute(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let id = ApiKeyId::new();
        create_api_key(&pool, id, ws, "one", b"h", &[Scope::Read], None)
            .await
            .unwrap();
        let last_used = |pool: PgPool| async move {
            find_api_key_for_auth(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .key
                .last_used_at
        };

        assert!(last_used(pool.clone()).await.is_none());
        touch_api_key(&pool, id).await.unwrap();
        let first = last_used(pool.clone()).await;
        assert!(first.is_some());
        touch_api_key(&pool, id).await.unwrap();
        assert_eq!(last_used(pool.clone()).await, first);
    }
}
