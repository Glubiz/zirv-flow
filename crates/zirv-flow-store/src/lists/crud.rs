//! Creating, reading, renaming and deleting lists. Every function is scoped to one workspace.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// A list and the number of people in it.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct List {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub member_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub created_by: Option<Uuid>,
}

/// The columns of [`List`].
macro_rules! list_columns {
    () => {
        "id, name, description, member_count, created_at, updated_at, created_by"
    };
}

/// Creates an empty list. A name already taken in the workspace, ignoring case, is a conflict.
pub async fn create_list<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    name: &str,
    description: &str,
    created_by: Option<UserId>,
) -> Result<List, StoreError> {
    sqlx::query_as(concat!(
        "INSERT INTO lists (id, workspace_id, name, description, created_by) \
         VALUES ($1, $2, $3, $4, $5) RETURNING ",
        list_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(name)
    .bind(description)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's lists, ordered by name.
pub async fn list_lists<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<List>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        list_columns!(),
        " FROM lists WHERE workspace_id = $1 ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn find_list<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<List>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        list_columns!(),
        " FROM lists WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Changes the name and the description that are given. `None` when there is no such list; a
/// name taken by another list is a conflict.
pub async fn update_list<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Option<List>, StoreError> {
    sqlx::query_as(concat!(
        "UPDATE lists SET name = COALESCE($3, name), description = COALESCE($4, description), \
            updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        list_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(name)
    .bind(description)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Deletes a list with its memberships and history, and returns what was deleted. `None` when
/// there is no such list.
pub async fn delete_list(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<List>, StoreError> {
    sqlx::query_as(concat!(
        "DELETE FROM lists WHERE workspace_id = $1 AND id = $2 RETURNING ",
        list_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_is_created_listed_renamed_and_deleted(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let beta = create_list(&pool, ws, "Beta", "", None).await.unwrap();
        let alpha = create_list(&pool, ws, "alpha", "first", None)
            .await
            .unwrap();

        assert_eq!(beta.member_count, 0);
        let names: Vec<String> = list_lists(&pool, ws)
            .await
            .unwrap()
            .into_iter()
            .map(|list| list.name)
            .collect();
        assert_eq!(names, ["alpha", "Beta"]);
        let renamed = update_list(&pool, ws, alpha.id, Some("Gamma"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (renamed.name.as_str(), renamed.description.as_str()),
            ("Gamma", "first")
        );
        assert!(renamed.updated_at >= alpha.updated_at);
        let described = update_list(&pool, ws, alpha.id, None, Some("second"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (described.name.as_str(), described.description.as_str()),
            ("Gamma", "second")
        );
        assert!(
            update_list(&pool, ws, Uuid::now_v7(), Some("x"), None)
                .await
                .unwrap()
                .is_none()
        );

        let mut connection = pool.acquire().await.unwrap();
        let deleted = delete_list(&mut connection, ws, beta.id).await.unwrap();
        assert_eq!(deleted.map(|list| list.name), Some("Beta".to_owned()));
        assert!(
            delete_list(&mut connection, ws, beta.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(find_list(&pool, ws, beta.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_name_is_unique_per_workspace_ignoring_case_and_bounded(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let first = create_list(&pool, one, "VIPs", "", None).await.unwrap();
        let other = create_list(&pool, one, "Others", "", None).await.unwrap();

        let duplicate = create_list(&pool, one, "vips", "", None).await.unwrap_err();
        let renamed = update_list(&pool, one, other.id, Some("VIPS"), None)
            .await
            .unwrap_err();

        assert!(duplicate.is_conflict());
        assert!(renamed.is_conflict());
        create_list(&pool, two, "VIPs", "", None).await.unwrap();
        assert!(create_list(&pool, one, "", "", None).await.is_err());
        assert!(
            create_list(&pool, one, &"x".repeat(129), "", None)
                .await
                .is_err()
        );
        assert_eq!(find_list(&pool, two, first.id).await.unwrap(), None);
    }
}
