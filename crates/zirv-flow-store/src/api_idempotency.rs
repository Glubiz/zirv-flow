//! Idempotency keys of the creating requests that nothing else guards (architecture section 15).
//! A row is keyed by workspace, route and the hash of the client's key, and remembers the hash of
//! the request, the status, the id of what was created and, for a route whose response holds no
//! secret and no person's value, the response body.
//!
//! The caller takes the key's advisory lock in a transaction it keeps for the whole request, so
//! two requests with one key run one after the other and the second sees the first one's row.

use serde_json::Value;
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// Keys are remembered for 24 hours, like a contact's.
const KEY_HOURS: i32 = 24;

/// What a remembered key answers.
#[derive(Debug, PartialEq)]
pub struct RememberedRequest {
    /// The hash of the request the key was first used for.
    pub request_hash: Vec<u8>,
    /// The status the first request answered with.
    pub status: u16,
    /// The id of the resource it created.
    pub resource_id: Uuid,
    /// The first response's body, for a route that stores one.
    pub response: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct StoredRow {
    request_hash: Vec<u8>,
    status: i16,
    resource_id: Uuid,
    response: Option<Json<Value>>,
}

/// A key to remember, with what it answers.
pub struct NewRememberedRequest<'a> {
    pub workspace: WorkspaceId,
    pub route: &'a str,
    pub key_hash: &'a [u8],
    pub request_hash: &'a [u8],
    pub status: u16,
    pub resource_id: Uuid,
    pub response: Option<&'a Value>,
}

/// Takes the key's advisory lock until the transaction ends, forgets the key if it has expired,
/// and returns what it remembers.
pub async fn lock_api_idempotency(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    route: &str,
    key_hash: &[u8],
) -> Result<Option<RememberedRequest>, StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "api-idem:{}:{route}:{}",
            workspace.as_uuid(),
            key_hash
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query(
        "DELETE FROM api_idempotency WHERE workspace_id = $1 AND route = $2 AND key_hash = $3 \
           AND created_at <= now() - make_interval(hours => $4)",
    )
    .bind(workspace.as_uuid())
    .bind(route)
    .bind(key_hash)
    .bind(KEY_HOURS)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let row: Option<StoredRow> = sqlx::query_as(
        "SELECT request_hash, status, resource_id, response FROM api_idempotency \
         WHERE workspace_id = $1 AND route = $2 AND key_hash = $3",
    )
    .bind(workspace.as_uuid())
    .bind(route)
    .bind(key_hash)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|row| RememberedRequest {
        request_hash: row.request_hash,
        status: u16::try_from(row.status).unwrap_or(200),
        resource_id: row.resource_id,
        response: row.response.map(|Json(value)| value),
    }))
}

/// Remembers a key that [`lock_api_idempotency`] found free.
pub async fn remember_api_idempotency(
    connection: &mut PgConnection,
    request: &NewRememberedRequest<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO api_idempotency \
           (workspace_id, route, key_hash, request_hash, status, resource_id, response) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(request.workspace.as_uuid())
    .bind(request.route)
    .bind(request.key_hash)
    .bind(request.request_hash)
    .bind(i16::try_from(request.status).unwrap_or(200))
    .bind(request.resource_id)
    .bind(request.response.map(Json))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Forgets idempotency keys older than 24 hours. Returns how many.
pub async fn expire_api_idempotency(pool: &PgPool) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM api_idempotency WHERE created_at <= now() - make_interval(hours => $1)",
    )
    .bind(KEY_HOURS)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(name).unwrap();
        create_workspace(pool, &name, &Slug::from_name(&name).unwrap())
            .await
            .unwrap()
            .id
    }

    async fn remember(
        pool: &PgPool,
        workspace: WorkspaceId,
        key: &[u8],
        resource: Uuid,
        response: Option<&Value>,
    ) {
        let mut tx = begin(pool).await.unwrap();
        assert_eq!(
            lock_api_idempotency(&mut tx, workspace, "create_x", key)
                .await
                .unwrap(),
            None
        );
        remember_api_idempotency(
            &mut tx,
            &NewRememberedRequest {
                workspace,
                route: "create_x",
                key_hash: key,
                request_hash: b"request",
                status: 201,
                resource_id: resource,
                response,
            },
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_remembered_key_answers_with_its_status_resource_and_body(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let resource = Uuid::now_v7();
        let body = json!({"id": resource});
        remember(&pool, ws, b"key", resource, Some(&body)).await;

        let mut connection = pool.acquire().await.unwrap();
        let found = lock_api_idempotency(&mut connection, ws, "create_x", b"key")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            found,
            RememberedRequest {
                request_hash: b"request".to_vec(),
                status: 201,
                resource_id: resource,
                response: Some(body),
            }
        );
        assert_eq!(
            lock_api_idempotency(&mut connection, ws, "create_y", b"key")
                .await
                .unwrap(),
            None,
            "another route is another key"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_without_a_body_remembers_none(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        remember(&pool, ws, b"key", Uuid::now_v7(), None).await;

        let mut connection = pool.acquire().await.unwrap();
        let found = lock_api_idempotency(&mut connection, ws, "create_x", b"key")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.response, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_key_is_forgotten_on_lookup_and_by_the_sweep(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        remember(&pool, ws, b"old", Uuid::now_v7(), None).await;
        sqlx::query("UPDATE api_idempotency SET created_at = now() - interval '25 hours'")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let lookup = lock_api_idempotency(&mut connection, ws, "create_x", b"old")
            .await
            .unwrap();

        assert_eq!(lookup, None);
        drop(connection);
        remember(&pool, ws, b"older", Uuid::now_v7(), None).await;
        remember(&pool, ws, b"new", Uuid::now_v7(), None).await;
        sqlx::query(
            "UPDATE api_idempotency SET created_at = now() - interval '25 hours' \
             WHERE key_hash = $1",
        )
        .bind(b"older".as_slice())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(expire_api_idempotency(&pool).await.unwrap(), 1);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM api_idempotency")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }
}
