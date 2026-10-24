//! The workspace's test-send allowlist: addresses a test may go to besides the members'. Only the
//! keyed hash and a masked hint are stored.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// An allowlist entry as listed. It holds no address and no hash.
#[derive(Clone, Debug, Eq, PartialEq, FromRow)]
pub struct AllowlistEntry {
    pub id: Uuid,
    /// The first character, `***`, then `@domain`.
    pub display_hint: String,
    pub created_at: OffsetDateTime,
}

/// The hint shown for an address: its first character, `***`, then `@domain`. Text with no `@`
/// has no domain to show and is masked entirely.
pub fn masked_hint(address: &str) -> String {
    match address.split_once('@') {
        Some((local, domain)) => {
            let first = local.chars().next().map(String::from).unwrap_or_default();
            format!("{first}***@{domain}")
        }
        None => "***".to_owned(),
    }
}

/// Adds an entry. `None` when the hash is on the list already.
pub async fn add_allowlist_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    address_hash: &[u8; 32],
    display_hint: &str,
) -> Result<Option<AllowlistEntry>, StoreError> {
    sqlx::query_as(
        "INSERT INTO test_send_allowlist (id, workspace_id, address_hash, display_hint) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (workspace_id, address_hash) DO NOTHING \
         RETURNING id, display_hint, created_at",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(address_hash.as_slice())
    .bind(display_hint)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// The workspace's entries, oldest first.
pub async fn list_allowlist(
    pool: &PgPool,
    workspace: WorkspaceId,
) -> Result<Vec<AllowlistEntry>, StoreError> {
    sqlx::query_as(
        "SELECT id, display_hint, created_at FROM test_send_allowlist \
         WHERE workspace_id = $1 ORDER BY created_at, id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)
}

/// Deletes an entry; false when the workspace has no such one.
pub async fn delete_allowlist_entry(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM test_send_allowlist WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.as_uuid())
        .bind(id)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(done.rows_affected() > 0)
}

/// Whether the hash is on the workspace's list.
pub async fn is_allowlisted(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    address_hash: &[u8; 32],
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM test_send_allowlist \
                        WHERE workspace_id = $1 AND address_hash = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(address_hash.as_slice())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;

    #[test]
    fn a_hint_keeps_the_first_character_and_the_domain_only() {
        assert_eq!(masked_hint("ada@example.com"), "a***@example.com");
        assert_eq!(masked_hint("@example.com"), "***@example.com");
        assert_eq!(masked_hint("no-at-sign"), "***");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_entry_is_added_once_listed_checked_and_deleted(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = crate::create_workspace(
            &pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let mut connection = pool.acquire().await.unwrap();
        let hash = [7_u8; 32];

        let first = add_allowlist_entry(&mut connection, ws, Uuid::now_v7(), &hash, "a***@x.test")
            .await
            .unwrap()
            .expect("added");
        let again = add_allowlist_entry(&mut connection, ws, Uuid::now_v7(), &hash, "a***@x.test")
            .await
            .unwrap();

        assert!(again.is_none());
        assert!(is_allowlisted(&mut connection, ws, &hash).await.unwrap());
        assert!(
            !is_allowlisted(&mut connection, ws, &[8_u8; 32])
                .await
                .unwrap()
        );
        assert_eq!(list_allowlist(&pool, ws).await.unwrap(), [first.clone()]);
        assert!(
            delete_allowlist_entry(&mut connection, ws, first.id)
                .await
                .unwrap()
        );
        assert!(
            !delete_allowlist_entry(&mut connection, ws, first.id)
                .await
                .unwrap()
        );
        assert!(!is_allowlisted(&mut connection, ws, &hash).await.unwrap());
    }
}
