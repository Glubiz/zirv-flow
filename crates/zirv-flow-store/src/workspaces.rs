//! Workspaces and their memberships.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Role, Slug, UserId, WorkspaceId, WorkspaceName};

use crate::StoreError;

macro_rules! workspace_columns {
    () => {
        "id, name, slug, created_at"
    };
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    pub created_at: OffsetDateTime,
}

#[derive(FromRow)]
struct WorkspaceRow {
    id: Uuid,
    name: String,
    slug: String,
    created_at: OffsetDateTime,
}

impl From<WorkspaceRow> for Workspace {
    fn from(row: WorkspaceRow) -> Self {
        Self {
            id: WorkspaceId::from_uuid(row.id),
            name: row.name,
            slug: row.slug,
            created_at: row.created_at,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Membership {
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub role: Role,
}

/// A workspace member as listed to other members.
#[derive(Clone)]
pub struct Member {
    pub user_id: UserId,
    pub email: String,
    pub role: Role,
    pub created_at: OffsetDateTime,
}

#[derive(FromRow)]
struct MemberRow {
    user_id: Uuid,
    email: String,
    role: String,
    created_at: OffsetDateTime,
}

/// Creates a workspace. A taken slug is a conflict.
pub async fn create_workspace<'e>(
    executor: impl PgExecutor<'e>,
    name: &WorkspaceName,
    slug: &Slug,
) -> Result<Workspace, StoreError> {
    let row: WorkspaceRow = sqlx::query_as(concat!(
        "INSERT INTO workspaces (id, name, slug) VALUES ($1, $2, $3) RETURNING ",
        workspace_columns!(),
        ""
    ))
    .bind(WorkspaceId::new().as_uuid())
    .bind(name.as_str())
    .bind(slug.as_str())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.into())
}

pub async fn find_workspace<'e>(
    executor: impl PgExecutor<'e>,
    id: WorkspaceId,
) -> Result<Option<Workspace>, StoreError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(concat!(
        "SELECT ",
        workspace_columns!(),
        " FROM workspaces WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// The workspaces a user can see: every workspace for the owner, otherwise the ones
/// they are a member of.
pub async fn list_workspaces_for_user<'e>(
    executor: impl PgExecutor<'e>,
    user: UserId,
    is_owner: bool,
) -> Result<Vec<Workspace>, StoreError> {
    let rows: Vec<WorkspaceRow> = sqlx::query_as(
        "SELECT w.id, w.name, w.slug, w.created_at FROM workspaces w \
         WHERE $2 OR EXISTS (SELECT 1 FROM memberships m \
                             WHERE m.workspace_id = w.id AND m.user_id = $1) \
         ORDER BY w.created_at, w.id",
    )
    .bind(user.as_uuid())
    .bind(is_owner)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn membership_role<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<Option<Role>, StoreError> {
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM memberships WHERE workspace_id = $1 AND user_id = $2")
            .bind(workspace.as_uuid())
            .bind(user.as_uuid())
            .fetch_optional(executor)
            .await
            .map_err(StoreError::query)?;
    Ok(role.and_then(|role| role.parse().ok()))
}

/// Adds a member or changes their role.
pub async fn set_membership<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    user: UserId,
    role: Role,
) -> Result<Membership, StoreError> {
    sqlx::query(
        "INSERT INTO memberships (workspace_id, user_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(workspace.as_uuid())
    .bind(user.as_uuid())
    .bind(role.as_str())
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(Membership {
        workspace_id: workspace,
        user_id: user,
        role,
    })
}

/// Removes a member. Returns whether they were one.
pub async fn remove_membership<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<bool, StoreError> {
    let result = sqlx::query("DELETE FROM memberships WHERE workspace_id = $1 AND user_id = $2")
        .bind(workspace.as_uuid())
        .bind(user.as_uuid())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

pub async fn list_members<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<Member>, StoreError> {
    let rows: Vec<MemberRow> = sqlx::query_as(
        "SELECT m.user_id, u.email, m.role, m.created_at FROM memberships m \
         JOIN users u ON u.id = m.user_id WHERE m.workspace_id = $1 \
         ORDER BY m.created_at, m.user_id",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some(Member {
                user_id: UserId::from_uuid(row.user_id),
                email: row.email,
                role: row.role.parse().ok()?,
                created_at: row.created_at,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::Email;

    use super::*;
    use crate::create_user;

    async fn user(pool: &PgPool, email: &str) -> UserId {
        create_user(pool, &Email::parse(email).unwrap(), Some("hash"), false)
            .await
            .unwrap()
            .id
    }

    async fn workspace(pool: &PgPool, name: &str) -> Workspace {
        let name = WorkspaceName::parse(name).unwrap();
        create_workspace(pool, &name, &Slug::from_name(&name).unwrap())
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_can_be_created_and_found(pool: PgPool) {
        let created = workspace(&pool, "Acme Corp").await;

        let found = find_workspace(&pool, created.id).await.unwrap().unwrap();

        assert_eq!(found.name, "Acme Corp");
        assert_eq!(found.slug, "acme-corp");
        assert!(
            find_workspace(&pool, WorkspaceId::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_slug_is_a_conflict(pool: PgPool) {
        workspace(&pool, "Acme").await;
        let name = WorkspaceName::parse("Another").unwrap();

        let error = create_workspace(&pool, &name, &Slug::parse("acme").unwrap())
            .await
            .unwrap_err();

        assert!(error.is_conflict());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn membership_is_added_changed_listed_and_removed(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let jane = user(&pool, "jane@example.com").await;

        set_membership(&pool, ws.id, jane, Role::Member)
            .await
            .unwrap();
        assert_eq!(
            membership_role(&pool, ws.id, jane).await.unwrap(),
            Some(Role::Member)
        );
        set_membership(&pool, ws.id, jane, Role::Admin)
            .await
            .unwrap();
        assert_eq!(
            membership_role(&pool, ws.id, jane).await.unwrap(),
            Some(Role::Admin)
        );
        let members = list_members(&pool, ws.id).await.unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].email, "jane@example.com");
        assert_eq!(members[0].role, Role::Admin);

        assert!(remove_membership(&pool, ws.id, jane).await.unwrap());
        assert!(!remove_membership(&pool, ws.id, jane).await.unwrap());
        assert_eq!(membership_role(&pool, ws.id, jane).await.unwrap(), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_membership_is_scoped_to_its_workspace(pool: PgPool) {
        let a = workspace(&pool, "A").await;
        let b = workspace(&pool, "B").await;
        let jane = user(&pool, "jane@example.com").await;
        set_membership(&pool, a.id, jane, Role::Admin)
            .await
            .unwrap();

        assert_eq!(membership_role(&pool, b.id, jane).await.unwrap(), None);
        assert!(list_members(&pool, b.id).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn listing_shows_members_their_workspaces_and_the_owner_all(pool: PgPool) {
        let a = workspace(&pool, "A").await;
        workspace(&pool, "B").await;
        let jane = user(&pool, "jane@example.com").await;
        set_membership(&pool, a.id, jane, Role::Member)
            .await
            .unwrap();

        let mine = list_workspaces_for_user(&pool, jane, false).await.unwrap();
        let owners = list_workspaces_for_user(&pool, UserId::new(), true)
            .await
            .unwrap();
        let strangers = list_workspaces_for_user(&pool, UserId::new(), false)
            .await
            .unwrap();

        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, a.id);
        assert_eq!(owners.len(), 2);
        assert!(strangers.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_membership_for_an_unknown_user_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;

        let result = set_membership(&pool, ws.id, UserId::new(), Role::Member).await;

        assert!(result.is_err());
    }
}
