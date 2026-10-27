//! Workspaces and their memberships.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{OrphanPolicy, Role, Slug, UserId, WorkspaceId, WorkspaceName};

use crate::StoreError;

macro_rules! workspace_columns {
    () => {
        "id, name, slug, timezone, created_at, utm"
    };
}

/// The workspace setting for automatic UTM parameters on email links (#240). The patterns are
/// stored as typed; the API validates them against the placeholder set before saving, and the
/// renderer crate expands them. Stored as one jsonb document, read with defaults so an older
/// document still loads.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UtmSettings {
    pub enabled: bool,
    pub source: String,
    pub medium: String,
    pub campaign: String,
    pub content: String,
    /// The link domains that get parameters; their subdomains do too.
    pub domains: Vec<String>,
}

impl Default for UtmSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            source: "{workspace}".to_owned(),
            medium: "email".to_owned(),
            campaign: "{flow}".to_owned(),
            content: "{template}".to_owned(),
            domains: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    /// The IANA time zone name of the workspace; `UTC` until it is set.
    pub timezone: String,
    pub created_at: OffsetDateTime,
    /// The automatic UTM parameter setting (#240); disabled until an admin turns it on.
    pub utm: UtmSettings,
}

#[derive(FromRow)]
struct WorkspaceRow {
    id: Uuid,
    name: String,
    slug: String,
    timezone: String,
    created_at: OffsetDateTime,
    /// Absent when the row was read before migration 0058 (the credential backfill tests) or by
    /// a create, which returns the defaults.
    #[sqlx(default)]
    utm: Option<sqlx::types::Json<UtmSettings>>,
}

impl From<WorkspaceRow> for Workspace {
    fn from(row: WorkspaceRow) -> Self {
        Self {
            id: WorkspaceId::from_uuid(row.id),
            name: row.name,
            slug: row.slug,
            timezone: row.timezone,
            created_at: row.created_at,
            utm: row.utm.map(|json| json.0).unwrap_or_default(),
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
        "id, name, slug, timezone, created_at",
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
        "SELECT w.id, w.name, w.slug, w.timezone, w.created_at, w.utm FROM workspaces w \
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

/// Locks every membership of the workspace until the transaction ends and returns the roles, so
/// two changes to the same workspace's members cannot both count the same last admin.
pub async fn lock_memberships(
    executor: &mut sqlx::PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<(UserId, Role)>, StoreError> {
    let rows: Vec<(uuid::Uuid, String)> = sqlx::query_as(
        "SELECT user_id, role FROM memberships WHERE workspace_id = $1 ORDER BY user_id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .filter_map(|(user, role)| Some((UserId::from_uuid(user), role.parse().ok()?)))
        .collect())
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

/// The workspace with this slug.
pub async fn find_workspace_by_slug<'e>(
    executor: impl PgExecutor<'e>,
    slug: &Slug,
) -> Result<Option<Workspace>, StoreError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(concat!(
        "SELECT ",
        workspace_columns!(),
        " FROM workspaces WHERE slug = $1"
    ))
    .bind(slug.as_str())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// The workspace's orphan policy; `None` when there is no such workspace.
pub async fn orphan_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Option<OrphanPolicy>, StoreError> {
    let policy: Option<String> =
        sqlx::query_scalar("SELECT orphan_policy FROM workspaces WHERE id = $1")
            .bind(workspace.as_uuid())
            .fetch_optional(executor)
            .await
            .map_err(StoreError::query)?;
    Ok(policy.map(|policy| policy.parse().unwrap_or_default()))
}

/// Sets the orphan policy. Returns whether the workspace exists.
pub async fn set_orphan_policy<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    policy: OrphanPolicy,
) -> Result<bool, StoreError> {
    let done = sqlx::query("UPDATE workspaces SET orphan_policy = $2 WHERE id = $1")
        .bind(workspace.as_uuid())
        .bind(policy.as_str())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Sets the workspace time zone and returns the saved workspace, or `None` when there is no such
/// workspace. The caller has checked the name against the zone database; the store only keeps it.
pub async fn set_workspace_timezone<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    timezone: &str,
) -> Result<Option<Workspace>, StoreError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(concat!(
        "UPDATE workspaces SET timezone = $2 WHERE id = $1 RETURNING ",
        workspace_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(timezone)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// Sets the workspace UTM setting and returns the saved workspace, or `None` when there is no such
/// workspace. The caller has validated the patterns and the domains; the store only keeps them.
pub async fn set_workspace_utm<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    settings: &UtmSettings,
) -> Result<Option<Workspace>, StoreError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(concat!(
        "UPDATE workspaces SET utm = $2 WHERE id = $1 RETURNING ",
        workspace_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(sqlx::types::Json(settings))
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
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
    async fn the_utm_setting_starts_disabled_with_the_defaults_and_is_kept(pool: PgPool) {
        let created = workspace(&pool, "Acme").await;
        let other = workspace(&pool, "Other").await;
        assert_eq!(created.utm, UtmSettings::default());
        assert!(!created.utm.enabled);
        assert_eq!(created.utm.source, "{workspace}");
        assert_eq!(created.utm.medium, "email");
        assert_eq!(created.utm.campaign, "{flow}");
        assert_eq!(created.utm.content, "{template}");
        assert!(created.utm.domains.is_empty());

        let settings = UtmSettings {
            enabled: true,
            domains: vec!["example.com".into()],
            ..UtmSettings::default()
        };
        let saved = set_workspace_utm(&pool, created.id, &settings)
            .await
            .unwrap()
            .unwrap();
        let missing = set_workspace_utm(&pool, WorkspaceId::new(), &settings)
            .await
            .unwrap();

        assert_eq!(saved.utm, settings);
        assert!(missing.is_none());
        let found = find_workspace(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(found.utm, settings);
        let untouched = find_workspace(&pool, other.id).await.unwrap().unwrap();
        assert_eq!(untouched.utm, UtmSettings::default());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_starts_in_utc_and_keeps_the_time_zone_it_is_given(pool: PgPool) {
        let created = workspace(&pool, "Acme").await;
        let other = workspace(&pool, "Other").await;
        assert_eq!(created.timezone, "UTC");

        let saved = set_workspace_timezone(&pool, created.id, "Europe/Copenhagen")
            .await
            .unwrap()
            .unwrap();
        let missing = set_workspace_timezone(&pool, WorkspaceId::new(), "UTC")
            .await
            .unwrap();
        let too_long = set_workspace_timezone(&pool, created.id, &"x".repeat(65)).await;

        assert_eq!(saved.timezone, "Europe/Copenhagen");
        assert!(missing.is_none());
        assert!(too_long.is_err());
        let found = find_workspace(&pool, created.id).await.unwrap().unwrap();
        assert_eq!(found.timezone, "Europe/Copenhagen");
        let untouched = find_workspace(&pool, other.id).await.unwrap().unwrap();
        assert_eq!(untouched.timezone, "UTC");
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

        set_membership(&pool, ws.id, jane, Role::Editor)
            .await
            .unwrap();
        assert_eq!(
            membership_role(&pool, ws.id, jane).await.unwrap(),
            Some(Role::Editor)
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
    async fn locking_memberships_returns_the_roles_of_that_workspace_only(pool: PgPool) {
        let a = workspace(&pool, "A").await;
        let b = workspace(&pool, "B").await;
        let jane = user(&pool, "jane@example.com").await;
        let joe = user(&pool, "joe@example.com").await;
        set_membership(&pool, a.id, jane, Role::Admin)
            .await
            .unwrap();
        set_membership(&pool, a.id, joe, Role::Viewer)
            .await
            .unwrap();
        set_membership(&pool, b.id, jane, Role::Publisher)
            .await
            .unwrap();

        let mut tx = pool.begin().await.unwrap();
        let mut locked = lock_memberships(&mut tx, a.id).await.unwrap();
        locked.sort_by_key(|(user, _)| user.as_uuid());

        let mut expected = vec![(jane, Role::Admin), (joe, Role::Viewer)];
        expected.sort_by_key(|(user, _)| user.as_uuid());
        assert_eq!(locked, expected);
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
        set_membership(&pool, a.id, jane, Role::Editor)
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

        let result = set_membership(&pool, ws.id, UserId::new(), Role::Editor).await;

        assert!(result.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_orphan_policy_defaults_to_keep_and_can_be_set(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;

        let before = orphan_policy(&pool, ws.id).await.unwrap();
        let set = set_orphan_policy(&pool, ws.id, OrphanPolicy::Delete)
            .await
            .unwrap();
        let after = orphan_policy(&pool, ws.id).await.unwrap();
        let unknown = set_orphan_policy(&pool, WorkspaceId::new(), OrphanPolicy::Delete)
            .await
            .unwrap();

        assert_eq!(before, Some(OrphanPolicy::Keep));
        assert!(set);
        assert_eq!(after, Some(OrphanPolicy::Delete));
        assert!(!unknown);
        assert_eq!(
            orphan_policy(&pool, WorkspaceId::new()).await.unwrap(),
            None
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_is_found_by_its_slug(pool: PgPool) {
        let ws = workspace(&pool, "Acme Corp").await;

        let found = find_workspace_by_slug(&pool, &Slug::parse("acme-corp").unwrap())
            .await
            .unwrap();
        let missing = find_workspace_by_slug(&pool, &Slug::parse("nope").unwrap())
            .await
            .unwrap();

        assert_eq!(found.unwrap().id, ws.id);
        assert!(missing.is_none());
    }
}
