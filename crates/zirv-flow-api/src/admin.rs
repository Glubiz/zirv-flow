//! The mutating operations on accounts, workspaces, memberships and API keys.
//!
//! The HTTP handlers and the `zirv-flow admin` commands both call these functions, so
//! the rules and the audit trail are the same on both paths. Every operation changes the
//! data and appends its audit row in one transaction. Audit summaries never carry
//! emails, passwords, tokens or key secrets.

use std::error::Error;
use std::fmt;

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::{
    ApiKeyId, Email, Role, Scope, Slug, UserId, ValidationError, WorkspaceId, WorkspaceName,
    password,
};
use zirv_flow_store::{
    Actor, ApiKey, Membership, NewAuditEntry, PgConnection, PgPool, StoreError, User, Workspace,
};

use crate::{credentials, passwords};

/// Who is acting and under which request, for the audit trail.
#[derive(Clone, Debug)]
pub struct Context {
    actor: Actor,
    request_id: Option<String>,
}

impl Context {
    /// Work done by the operator on the command line.
    pub fn system() -> Self {
        Self {
            actor: Actor::system(),
            request_id: None,
        }
    }

    /// A caller who has not proved who they are, such as a failed sign-in.
    pub fn anonymous() -> Self {
        Self {
            actor: Actor::anonymous(),
            request_id: None,
        }
    }

    pub fn user(id: UserId) -> Self {
        Self {
            actor: Actor::user(id.as_uuid()),
            request_id: None,
        }
    }

    pub fn api_key(id: ApiKeyId) -> Self {
        Self {
            actor: Actor::api_key(id.as_uuid()),
            request_id: None,
        }
    }

    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }
}

/// Why an operation did not happen. The messages are safe to show to callers.
#[derive(Debug)]
pub enum AdminError {
    Invalid(ValidationError),
    Conflict(&'static str),
    NotFound(&'static str),
    Store(StoreError),
}

impl fmt::Display for AdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(formatter),
            Self::Conflict(message) => formatter.write_str(message),
            Self::NotFound(what) => write!(formatter, "no such {what}"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl Error for AdminError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Conflict(_) | Self::NotFound(_) => None,
        }
    }
}

impl From<ValidationError> for AdminError {
    fn from(error: ValidationError) -> Self {
        Self::Invalid(error)
    }
}

impl From<StoreError> for AdminError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// A new API key. The plaintext exists only here and in the response that creates it.
pub struct CreatedApiKey {
    pub key: ApiKey,
    pub plaintext: String,
}

impl fmt::Debug for CreatedApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CreatedApiKey")
            .field("id", &self.key.id)
            .finish_non_exhaustive()
    }
}

/// Appends an audit row on `connection`.
pub(crate) async fn audit(
    connection: &mut PgConnection,
    context: &Context,
    workspace: Option<WorkspaceId>,
    action: &'static str,
    object: Option<(&'static str, Uuid)>,
    summary: Value,
) -> Result<(), StoreError> {
    zirv_flow_store::append_audit(
        connection,
        &NewAuditEntry {
            workspace_id: workspace,
            actor: context.actor,
            action,
            object_kind: object.map(|(kind, _)| kind),
            object_id: object.map(|(_, id)| id),
            request_id: context.request_id.clone(),
            summary,
        },
    )
    .await
}

/// Creates the instance owner. Refused when an owner already exists.
pub async fn create_owner(
    pool: &PgPool,
    context: &Context,
    email: &str,
    password: &str,
) -> Result<User, AdminError> {
    create_account(pool, context, email, password, true).await
}

/// Creates an ordinary user.
pub async fn create_user(
    pool: &PgPool,
    context: &Context,
    email: &str,
    password: &str,
) -> Result<User, AdminError> {
    create_account(pool, context, email, password, false).await
}

async fn create_account(
    pool: &PgPool,
    context: &Context,
    email: &str,
    plain: &str,
    is_owner: bool,
) -> Result<User, AdminError> {
    let email = Email::parse(email)?;
    password::validate(plain)?;
    let hash = passwords::hash(plain.to_owned()).await;

    let mut tx = zirv_flow_store::begin(pool).await?;
    if is_owner && zirv_flow_store::count_owners(&mut *tx).await? > 0 {
        return Err(AdminError::Conflict("an owner already exists"));
    }
    let user = zirv_flow_store::create_user(&mut *tx, &email, &hash, is_owner)
        .await
        .map_err(|error| {
            if error.is_conflict() {
                if is_owner {
                    AdminError::Conflict("an owner already exists or the email is in use")
                } else {
                    AdminError::Conflict("a user with that email already exists")
                }
            } else {
                error.into()
            }
        })?;
    audit(
        &mut tx,
        context,
        None,
        "user.created",
        Some(("user", user.id.as_uuid())),
        json!({ "owner": is_owner }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(user)
}

/// Sets a new password for the user with this email, clears any lockout and ends all of
/// their sessions.
pub async fn reset_password(
    pool: &PgPool,
    context: &Context,
    email: &str,
    plain: &str,
) -> Result<(), AdminError> {
    let email = Email::parse(email)?;
    password::validate(plain)?;
    let hash = passwords::hash(plain.to_owned()).await;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let user = zirv_flow_store::find_user_by_email(&mut *tx, &email)
        .await?
        .ok_or(AdminError::NotFound("user"))?;
    zirv_flow_store::set_password(&mut *tx, user.id, &hash).await?;
    let ended = zirv_flow_store::delete_sessions_for_user(&mut *tx, user.id).await?;
    audit(
        &mut tx,
        context,
        None,
        "user.password_reset",
        Some(("user", user.id.as_uuid())),
        json!({ "sessions_ended": ended }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// Creates a workspace; the slug is derived from the name when not given.
pub async fn create_workspace(
    pool: &PgPool,
    context: &Context,
    name: &str,
    slug: Option<&str>,
) -> Result<Workspace, AdminError> {
    let name = WorkspaceName::parse(name)?;
    let slug = match slug {
        Some(slug) => Slug::parse(slug)?,
        None => Slug::from_name(&name)?,
    };

    let mut tx = zirv_flow_store::begin(pool).await?;
    let workspace = zirv_flow_store::create_workspace(&mut *tx, &name, &slug)
        .await
        .map_err(|error| {
            if error.is_conflict() {
                AdminError::Conflict("a workspace with that slug already exists")
            } else {
                error.into()
            }
        })?;
    audit(
        &mut tx,
        context,
        Some(workspace.id),
        "workspace.created",
        Some(("workspace", workspace.id.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(workspace)
}

/// Adds a user to a workspace or changes their role.
pub async fn set_member(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    user: UserId,
    role: Role,
) -> Result<Membership, AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    zirv_flow_store::find_user(&mut *tx, user)
        .await?
        .ok_or(AdminError::NotFound("user"))?;
    let membership = zirv_flow_store::set_membership(&mut *tx, workspace, user, role).await?;
    audit(
        &mut tx,
        context,
        Some(workspace),
        "membership.set",
        Some(("user", user.as_uuid())),
        json!({ "role": role.as_str() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(membership)
}

/// Removes a user from a workspace.
pub async fn remove_member(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    if !zirv_flow_store::remove_membership(&mut *tx, workspace, user).await? {
        return Err(AdminError::NotFound("member"));
    }
    audit(
        &mut tx,
        context,
        Some(workspace),
        "membership.removed",
        Some(("user", user.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// Creates an API key. Returns the plaintext once; only its hash is stored.
pub async fn create_api_key(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    name: &str,
    scopes: &[Scope],
) -> Result<CreatedApiKey, AdminError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > 100 || name.chars().any(char::is_control) {
        return Err(ValidationError::new("name", "must be 1 to 100 characters").into());
    }
    let mut unique: Vec<Scope> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        if !unique.contains(scope) {
            unique.push(*scope);
        }
    }
    if unique.is_empty() {
        return Err(ValidationError::new("scopes", "must contain at least one scope").into());
    }

    let id = ApiKeyId::new();
    let generated = credentials::new_key(id);
    let created_by = match context.actor.kind {
        zirv_flow_store::ActorKind::User => context.actor.id.map(UserId::from_uuid),
        _ => None,
    };

    let mut tx = zirv_flow_store::begin(pool).await?;
    let key = zirv_flow_store::create_api_key(
        &mut *tx,
        id,
        workspace,
        name,
        &generated.secret_hash,
        &unique,
        created_by,
    )
    .await?;
    let scope_names: Vec<&str> = unique.iter().map(|scope| scope.as_str()).collect();
    audit(
        &mut tx,
        context,
        Some(workspace),
        "api_key.created",
        Some(("api_key", id.as_uuid())),
        json!({ "scopes": scope_names }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(CreatedApiKey {
        key,
        plaintext: generated.plaintext,
    })
}

/// Revokes a key of this workspace.
pub async fn revoke_api_key(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: ApiKeyId,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    if !zirv_flow_store::revoke_api_key(&mut *tx, workspace, id).await? {
        return Err(AdminError::NotFound("api key"));
    }
    audit(
        &mut tx,
        context,
        Some(workspace),
        "api_key.revoked",
        Some(("api_key", id.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn contexts_record_the_kind_of_actor() {
        let user = UserId::new();

        assert_eq!(Context::system().actor.id, None);
        assert_eq!(Context::anonymous().actor.kind.as_str(), "anonymous");
        assert_eq!(Context::user(user).actor.id, Some(user.as_uuid()));
        assert_eq!(
            Context::api_key(ApiKeyId::new()).actor.kind.as_str(),
            "api_key"
        );
    }

    #[test]
    fn error_messages_are_safe_to_show() {
        assert_eq!(AdminError::NotFound("user").to_string(), "no such user");
        assert_eq!(AdminError::Conflict("taken").to_string(), "taken");
        let invalid = AdminError::from(password::validate("short").unwrap_err());
        assert!(!invalid.to_string().contains("short"));
    }

    #[test]
    fn a_created_key_hides_its_plaintext_in_debug_output() {
        let id = ApiKeyId::new();
        let created = CreatedApiKey {
            key: ApiKey {
                id,
                workspace_id: WorkspaceId::new(),
                name: "ci".into(),
                scopes: vec![Scope::Read],
                created_by: None,
                created_at: time::OffsetDateTime::now_utc(),
                last_used_at: None,
                revoked_at: None,
            },
            plaintext: "zfk_secret".into(),
        };

        assert!(!format!("{created:?}").contains("zfk_secret"));
    }

    async fn migrated(pool: &PgPool) {
        zirv_flow_store::migrate(pool).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_owner_can_be_created_once(pool: PgPool) {
        migrated(&pool).await;

        let owner = create_owner(&pool, &Context::system(), "Owner@Example.com", PASSWORD)
            .await
            .unwrap();
        let second = create_owner(&pool, &Context::system(), "two@example.com", PASSWORD)
            .await
            .unwrap_err();

        assert!(owner.is_owner);
        assert_eq!(owner.email, "owner@example.com");
        assert!(zirv_flow_core::password::verify(
            PASSWORD,
            &owner.password_hash
        ));
        assert!(matches!(second, AdminError::Conflict(_)), "{second}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_input_is_refused_before_anything_is_stored(pool: PgPool) {
        migrated(&pool).await;

        let bad_email = create_user(&pool, &Context::system(), "nope", PASSWORD).await;
        let short = create_user(&pool, &Context::system(), "a@example.com", "short").await;

        assert!(matches!(bad_email, Err(AdminError::Invalid(_))));
        assert!(matches!(short, Err(AdminError::Invalid(_))));
        assert!(zirv_flow_store::list_users(&pool).await.unwrap().is_empty());
        assert!(
            zirv_flow_store::list_audit(&pool, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reset_changes_the_password_clears_the_lock_and_ends_sessions(pool: PgPool) {
        migrated(&pool).await;
        let user = create_user(&pool, &Context::system(), "a@example.com", PASSWORD)
            .await
            .unwrap();
        for _ in 0..5 {
            zirv_flow_store::record_failed_sign_in(&pool, user.id)
                .await
                .unwrap();
        }
        zirv_flow_store::create_session(&pool, b"token", user.id, b"csrf", 3600)
            .await
            .unwrap();

        reset_password(
            &pool,
            &Context::system(),
            "A@example.com",
            "another long password",
        )
        .await
        .unwrap();

        let user = zirv_flow_store::find_user(&pool, user.id)
            .await
            .unwrap()
            .unwrap();
        assert!(zirv_flow_core::password::verify(
            "another long password",
            &user.password_hash
        ));
        assert!(user.locked_until.is_none());
        assert!(
            zirv_flow_store::find_active_session(&pool, b"token", 3600)
                .await
                .unwrap()
                .is_none()
        );
        let missing = reset_password(&pool, &Context::system(), "x@example.com", PASSWORD).await;
        assert!(matches!(missing, Err(AdminError::NotFound(_))));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn workspaces_members_and_keys_are_created_and_audited(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system().with_request_id(Some("req-7".into()));
        let user = create_user(&pool, &context, "a@example.com", PASSWORD)
            .await
            .unwrap();
        let ws = create_workspace(&pool, &context, "Acme Corp", None)
            .await
            .unwrap();
        let clash = create_workspace(&pool, &context, "Other", Some("acme-corp")).await;
        set_member(&pool, &context, ws.id, user.id, Role::Member)
            .await
            .unwrap();
        let created = create_api_key(&pool, &context, ws.id, " ci ", &[Scope::Read, Scope::Read])
            .await
            .unwrap();
        revoke_api_key(&pool, &context, ws.id, created.key.id)
            .await
            .unwrap();
        remove_member(&pool, &context, ws.id, user.id)
            .await
            .unwrap();

        assert!(matches!(clash, Err(AdminError::Conflict(_))));
        assert_eq!(created.key.name, "ci");
        assert_eq!(created.key.scopes, [Scope::Read]);
        assert!(created.plaintext.starts_with("zfk_"));
        let mut actions: Vec<String> = zirv_flow_store::list_audit(&pool, 50)
            .await
            .unwrap()
            .into_iter()
            .inspect(|row| assert_eq!(row.request_id.as_deref(), Some("req-7")))
            .map(|row| row.action)
            .collect();
        actions.reverse();
        assert_eq!(
            actions,
            [
                "user.created",
                "workspace.created",
                "membership.set",
                "api_key.created",
                "api_key.revoked",
                "membership.removed"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn bad_members_and_keys_are_refused(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();

        let unknown_user = set_member(&pool, &context, ws.id, UserId::new(), Role::Admin).await;
        let not_a_member = remove_member(&pool, &context, ws.id, UserId::new()).await;
        let no_scopes = create_api_key(&pool, &context, ws.id, "ci", &[]).await;
        let no_name = create_api_key(&pool, &context, ws.id, "  ", &[Scope::Read]).await;
        let unknown_key = revoke_api_key(&pool, &context, ws.id, ApiKeyId::new()).await;

        assert!(matches!(unknown_user, Err(AdminError::NotFound(_))));
        assert!(matches!(not_a_member, Err(AdminError::NotFound(_))));
        assert!(matches!(no_scopes, Err(AdminError::Invalid(_))));
        assert!(matches!(no_name, Err(AdminError::Invalid(_))));
        assert!(matches!(unknown_key, Err(AdminError::NotFound(_))));
    }
}
