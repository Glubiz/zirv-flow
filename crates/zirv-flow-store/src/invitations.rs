//! Workspace invitations. Only the hash of the link's token is stored.
//!
//! An invitation is live while it is neither accepted nor revoked and `expires_at` is in the
//! future; expiry is read from the clock of the database, with no sweep and no write.

use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Role, UserId, WorkspaceId};

use crate::StoreError;

macro_rules! invitation_columns {
    () => {
        "id, workspace_id, email, role, view_pii, invited_by, created_at, expires_at, \
         accepted_at, accepted_by, revoked_at"
    };
}

/// An invitation without its token hash.
#[derive(Clone)]
pub struct Invitation {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    /// The invitee's address as the caller stored it.
    pub email: String,
    pub role: Role,
    pub view_pii: bool,
    pub invited_by: Option<UserId>,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub accepted_at: Option<OffsetDateTime>,
    pub accepted_by: Option<UserId>,
    pub revoked_at: Option<OffsetDateTime>,
}

// The address is personal data: it is never printed.
impl std::fmt::Debug for Invitation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Invitation")
            .field("id", &self.id)
            .field("workspace_id", &self.workspace_id)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

/// What a new invitation holds. The token is the caller's; only its hash comes here.
pub struct NewInvitation<'a> {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub email: &'a str,
    pub role: Role,
    pub view_pii: bool,
    pub token_hash: &'a [u8],
    pub invited_by: Option<UserId>,
    pub expires_in_days: i32,
}

#[derive(FromRow)]
struct InvitationRow {
    id: Uuid,
    workspace_id: Uuid,
    email: String,
    role: String,
    view_pii: bool,
    invited_by: Option<Uuid>,
    created_at: OffsetDateTime,
    expires_at: OffsetDateTime,
    accepted_at: Option<OffsetDateTime>,
    accepted_by: Option<Uuid>,
    revoked_at: Option<OffsetDateTime>,
}

impl InvitationRow {
    fn into_invitation(self) -> Option<Invitation> {
        Some(Invitation {
            id: self.id,
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            email: self.email,
            role: self.role.parse().ok()?,
            view_pii: self.view_pii,
            invited_by: self.invited_by.map(UserId::from_uuid),
            created_at: self.created_at,
            expires_at: self.expires_at,
            accepted_at: self.accepted_at,
            accepted_by: self.accepted_by.map(UserId::from_uuid),
            revoked_at: self.revoked_at,
        })
    }
}

const LIVE: &str = "accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now()";

fn invitations(rows: Vec<InvitationRow>) -> Vec<Invitation> {
    rows.into_iter()
        .filter_map(InvitationRow::into_invitation)
        .collect()
}

/// Creates an invitation. An expired invitation of the same address that nobody accepted or
/// revoked is revoked first, in the same transaction, because it still holds the address's unique
/// slot. A live one is a conflict.
pub async fn create_invitation(
    connection: &mut PgConnection,
    new: &NewInvitation<'_>,
) -> Result<Invitation, StoreError> {
    sqlx::query(
        "UPDATE invitations SET revoked_at = now() \
         WHERE workspace_id = $1 AND lower(email) = lower($2) \
           AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at <= now()",
    )
    .bind(new.workspace.as_uuid())
    .bind(new.email)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let row: InvitationRow = sqlx::query_as(concat!(
        "INSERT INTO invitations (id, workspace_id, email, role, view_pii, token_hash, invited_by, \
         expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8)) \
         RETURNING ",
        invitation_columns!()
    ))
    .bind(new.id)
    .bind(new.workspace.as_uuid())
    .bind(new.email)
    .bind(new.role.as_str())
    .bind(new.view_pii)
    .bind(new.token_hash)
    .bind(new.invited_by.map(|user| user.as_uuid()))
    .bind(new.expires_in_days)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    row.into_invitation()
        .ok_or_else(|| StoreError::query(sqlx::Error::RowNotFound))
}

/// The pending invitations of a workspace, newest first.
pub async fn list_pending_invitations(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<Invitation>, StoreError> {
    let rows: Vec<InvitationRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM invitations WHERE workspace_id = $1 AND {LIVE} \
         ORDER BY created_at DESC, id",
        invitation_columns!()
    )))
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(invitations(rows))
}

/// The live invitation with this token hash, locked until the transaction ends.
pub async fn find_live_invitation_for_update(
    connection: &mut PgConnection,
    token_hash: &[u8],
) -> Result<Option<Invitation>, StoreError> {
    find_live(connection, token_hash, " FOR UPDATE").await
}

/// The live invitation with this token hash, without a lock.
pub async fn find_live_invitation(
    connection: &mut PgConnection,
    token_hash: &[u8],
) -> Result<Option<Invitation>, StoreError> {
    find_live(connection, token_hash, "").await
}

async fn find_live(
    connection: &mut PgConnection,
    token_hash: &[u8],
    lock: &'static str,
) -> Result<Option<Invitation>, StoreError> {
    let row: Option<InvitationRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM invitations WHERE token_hash = $1 AND {LIVE}{lock}",
        invitation_columns!()
    )))
    .bind(token_hash)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.and_then(InvitationRow::into_invitation))
}

/// Marks a live invitation accepted by `user`. Returns whether it was live.
pub async fn mark_invitation_accepted(
    connection: &mut PgConnection,
    id: Uuid,
    user: UserId,
) -> Result<bool, StoreError> {
    let result = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE invitations SET accepted_at = now(), accepted_by = $2 WHERE id = $1 AND {LIVE}"
    )))
    .bind(id)
    .bind(user.as_uuid())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Revokes a not yet accepted invitation of this workspace. Returns whether one was revoked.
pub async fn revoke_invitation(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE invitations SET revoked_at = now() \
         WHERE id = $1 AND workspace_id = $2 AND accepted_at IS NULL AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
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

    fn new<'a>(ws: WorkspaceId, email: &'a str, hash: &'a [u8]) -> NewInvitation<'a> {
        NewInvitation {
            id: Uuid::now_v7(),
            workspace: ws,
            email,
            role: Role::Viewer,
            view_pii: false,
            token_hash: hash,
            invited_by: None,
            expires_in_days: 7,
        }
    }

    async fn make(pool: &PgPool, ws: WorkspaceId, email: &str, hash: &[u8]) -> Invitation {
        let mut connection = pool.acquire().await.unwrap();
        create_invitation(&mut connection, &new(ws, email, hash))
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invitation_is_stored_with_the_hash_of_its_token_and_found_by_it(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;

        let made = make(&pool, ws, "cy@acme.test", b"hash-one").await;
        let mut connection = pool.acquire().await.unwrap();
        let found = find_live_invitation(&mut connection, b"hash-one")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(found.id, made.id);
        assert_eq!(found.role, Role::Viewer);
        assert!(!found.view_pii);
        assert!(found.expires_at > OffsetDateTime::now_utc() + time::Duration::days(6));
        assert!(
            find_live_invitation(&mut connection, b"other")
                .await
                .unwrap()
                .is_none()
        );
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM invitations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, b"hash-one");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_live_invitation_for_an_address_is_a_conflict_in_any_case(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let other = workspace(&pool, "Other").await;
        make(&pool, ws, "cy@acme.test", b"one").await;
        let mut connection = pool.acquire().await.unwrap();

        let error = create_invitation(&mut connection, &new(ws, "CY@acme.test", b"two"))
            .await
            .unwrap_err();

        assert!(error.is_conflict());
        // Another workspace may invite the same address.
        create_invitation(&mut connection, &new(other, "cy@acme.test", b"three"))
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_invitation_is_not_live_and_the_address_can_be_invited_again(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let old = make(&pool, ws, "cy@acme.test", b"old").await;
        sqlx::query("UPDATE invitations SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            find_live_invitation(&mut connection, b"old")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            list_pending_invitations(&mut connection, ws)
                .await
                .unwrap()
                .is_empty()
        );
        let again = create_invitation(&mut connection, &new(ws, "cy@acme.test", b"new"))
            .await
            .unwrap();

        assert_ne!(again.id, old.id);
        let revoked: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT revoked_at FROM invitations WHERE id = $1")
                .bind(old.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(revoked.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_pending_list_holds_one_workspace_and_only_the_pending(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let other = workspace(&pool, "Other").await;
        let first = make(&pool, ws, "a@acme.test", b"a").await;
        let second = make(&pool, ws, "b@acme.test", b"b").await;
        make(&pool, other, "c@acme.test", b"c").await;
        let mut connection = pool.acquire().await.unwrap();
        revoke_invitation(&mut connection, ws, first.id)
            .await
            .unwrap();

        let pending = list_pending_invitations(&mut connection, ws).await.unwrap();

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, second.id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn revoking_works_once_only_in_its_workspace_and_frees_the_address(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let other = workspace(&pool, "Other").await;
        let made = make(&pool, ws, "cy@acme.test", b"one").await;
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            !revoke_invitation(&mut connection, other, made.id)
                .await
                .unwrap()
        );
        assert!(
            revoke_invitation(&mut connection, ws, made.id)
                .await
                .unwrap()
        );
        assert!(
            !revoke_invitation(&mut connection, ws, made.id)
                .await
                .unwrap()
        );
        assert!(
            find_live_invitation(&mut connection, b"one")
                .await
                .unwrap()
                .is_none()
        );
        create_invitation(&mut connection, &new(ws, "cy@acme.test", b"two"))
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invitation_is_accepted_once(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;
        let made = make(&pool, ws, "cy@acme.test", b"one").await;
        let user = crate::create_user(
            &pool,
            &zirv_flow_core::Email::parse("cy@acme.test").unwrap(),
            None,
            false,
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            mark_invitation_accepted(&mut connection, made.id, user.id)
                .await
                .unwrap()
        );
        assert!(
            !mark_invitation_accepted(&mut connection, made.id, user.id)
                .await
                .unwrap()
        );
        assert!(
            find_live_invitation(&mut connection, b"one")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !revoke_invitation(&mut connection, ws, made.id)
                .await
                .unwrap()
        );
        // An accepted invitation no longer holds the address's slot.
        create_invitation(&mut connection, &new(ws, "cy@acme.test", b"two"))
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_role_check_refuses_a_role_that_is_not_one(pool: PgPool) {
        let ws = workspace(&pool, "Acme").await;

        let result = sqlx::query(
            "INSERT INTO invitations (id, workspace_id, email, role, token_hash, expires_at) \
             VALUES ($1, $2, 'a@b.test', 'owner', 'h', now())",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .execute(&pool)
        .await;

        assert!(result.is_err());
    }
}
