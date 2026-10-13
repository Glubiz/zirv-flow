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
    ApiKeyId, Cause, Email, HmacPurpose, HmacRoot, Keyring, Role, Scope, SecretBytes, SecretError,
    SecretId, SecretPurpose, Slug, TraceContext, UserId, ValidationError, WorkspaceId,
    WorkspaceName, password, rewrap, seal, secret_aad,
};
use zirv_flow_store::secrets;
use zirv_flow_store::{
    Actor, ApiKey, Membership, NewAuditEntry, PgConnection, PgPool, ReplayReport, StoreError, User,
    Workspace,
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
    /// A stored secret could not be re-wrapped. Names a key id, never key or plaintext bytes.
    Secret(SecretError),
}

impl fmt::Display for AdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(formatter),
            Self::Conflict(message) => formatter.write_str(message),
            Self::NotFound(what) => write!(formatter, "no such {what}"),
            Self::Store(error) => error.fmt(formatter),
            Self::Secret(error) => error.fmt(formatter),
        }
    }
}

impl Error for AdminError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Secret(error) => Some(error),
            Self::Conflict(_) | Self::NotFound(_) => None,
        }
    }
}

impl From<ValidationError> for AdminError {
    fn from(error: ValidationError) -> Self {
        Self::Invalid(error)
    }
}

impl From<SecretError> for AdminError {
    fn from(error: SecretError) -> Self {
        Self::Secret(error)
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
    create_account(pool, context, email, password, true, None).await
}

/// Creates the instance owner for the first-run setup endpoint; the audit entry says so.
pub async fn create_first_owner(
    pool: &PgPool,
    context: &Context,
    email: &str,
    password: &str,
) -> Result<User, AdminError> {
    create_account(
        pool,
        context,
        email,
        password,
        true,
        Some("first-run setup"),
    )
    .await
}

/// Creates an ordinary user.
pub async fn create_user(
    pool: &PgPool,
    context: &Context,
    email: &str,
    password: &str,
) -> Result<User, AdminError> {
    create_account(pool, context, email, password, false, None).await
}

async fn create_account(
    pool: &PgPool,
    context: &Context,
    email: &str,
    plain: &str,
    is_owner: bool,
    via: Option<&'static str>,
) -> Result<User, AdminError> {
    let email = Email::parse(email)?;
    password::validate(plain)?;
    let hash = passwords::hash(plain.to_owned()).await;

    let mut tx = zirv_flow_store::begin(pool).await?;
    if is_owner && zirv_flow_store::count_owners(&mut *tx).await? > 0 {
        return Err(AdminError::Conflict("an owner already exists"));
    }
    let user = zirv_flow_store::create_user(&mut *tx, &email, Some(&hash), is_owner)
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
        match via {
            Some(via) => json!({ "owner": is_owner, "via": via }),
            None => json!({ "owner": is_owner }),
        },
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

/// Re-applies the completed erasure ledger of one workspace (by slug) or of all of them after a
/// backup restore: re-adds the `erased` suppressions, erases every ledger person that exists
/// again, and removes pending inbox rows that name an erased identifier. Returns counts only,
/// and audits them as `privacy.erasures_replayed`.
pub async fn replay_erasures(
    pool: &PgPool,
    context: &Context,
    workspace: Option<&str>,
    root: &HmacRoot,
) -> Result<ReplayReport, AdminError> {
    let workspace = match workspace {
        Some(slug) => {
            let slug = Slug::parse(slug)?;
            Some(
                zirv_flow_store::find_workspace_by_slug(pool, &slug)
                    .await?
                    .ok_or(AdminError::NotFound("workspace"))?
                    .id,
            )
        }
        None => None,
    };
    let key = root.hmac_key(HmacPurpose::Identifier);
    let trace = TraceContext::root(Cause::api("admin.replay-erasures"));
    let report = zirv_flow_store::replay_erasures(pool, workspace, Some(&key), &trace).await?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    audit(
        &mut tx,
        context,
        workspace,
        "privacy.erasures_replayed",
        None,
        json!({
            "requests": report.requests,
            "suppressions": report.suppressions,
            "people": report.people,
            "inbox_rows": report.inbox_rows,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(report)
}

/// What [`rotate_master_key`] re-wrapped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RotationReport {
    /// Data keys now wrapped by the current master key that another key wrapped before.
    pub rows: usize,
    /// The ids of the master keys that wrapped them, sorted.
    pub from_key_ids: Vec<String>,
    /// The id of the current master key.
    pub key_id: String,
}

/// Re-wraps every data key, the deployment HMAC root included, with the current master key in
/// one transaction. Ciphertexts are untouched, and a second run re-wraps nothing. A row
/// wrapped by a key the keyring does not hold aborts the whole rotation. Audited as
/// `secrets.master_key_rotated`, with key ids and the count only.
pub async fn rotate_master_key(
    pool: &PgPool,
    context: &Context,
    keyring: &Keyring,
) -> Result<RotationReport, AdminError> {
    let key_id = keyring.current().key_id();
    let mut tx = zirv_flow_store::begin(pool).await?;
    // The store locks the root first: it refuses a deployment with no root before anything
    // changes, and it makes secret writes in flight finish (or wait) before it scans.
    let rotated = secrets::rotate(&mut tx, &key_id, |sealed| rewrap(keyring, sealed))
        .await
        .map_err(|error| match error {
            secrets::RotateError::NoRoot => AdminError::Conflict(secrets::NO_HMAC_ROOT),
            secrets::RotateError::Store(error) => error.into(),
            secrets::RotateError::Rewrap(error) => error.into(),
        })?;
    let report = RotationReport {
        rows: rotated.rows,
        from_key_ids: rotated.from_key_ids.into_iter().collect(),
        key_id,
    };
    audit(
        &mut tx,
        context,
        None,
        "secrets.master_key_rotated",
        None,
        json!({
            "from_key_ids": report.from_key_ids,
            "key_id": report.key_id,
            "rows": report.rows,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(report)
}

/// Maps the store's refusal of a secret write: a process whose keyring does not hold the key
/// that wraps the HMAC root would store a row no later keyring opens after a rotation, and a
/// missing root cannot be ordered against a rotation (serve and worker create it at start).
fn write_refusal(error: secrets::SecretWriteError) -> AdminError {
    match error {
        secrets::SecretWriteError::NoRoot => AdminError::Conflict(
            "no HMAC root exists yet; start the server or worker once before storing secrets",
        ),
        secrets::SecretWriteError::Outdated { .. } => AdminError::Conflict(
            "this process's master key is outdated; restart it with the current master key",
        ),
        secrets::SecretWriteError::Store(error) => error.into(),
    }
}

/// Seals `plaintext` and stores it as a secret of the workspace. Audited as `secret.created`
/// with the purpose and the wrapping key id, never the value.
pub async fn create_secret(
    pool: &PgPool,
    context: &Context,
    keyring: &Keyring,
    workspace: WorkspaceId,
    purpose: SecretPurpose,
    plaintext: &SecretBytes,
) -> Result<SecretId, AdminError> {
    let id = SecretId::new();
    let sealed = seal(
        keyring,
        plaintext.expose(),
        &secret_aad(workspace, id, purpose),
    );
    let mut tx = zirv_flow_store::begin(pool).await?;
    secrets::insert(&mut tx, &keyring.key_ids(), workspace, purpose, id, &sealed)
        .await
        .map_err(write_refusal)?;
    audit(
        &mut tx,
        context,
        Some(workspace),
        "secret.created",
        Some(("secret", id.as_uuid())),
        json!({"purpose": purpose.as_str(), "key_id": sealed.key_id}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(id)
}

/// Seals a new value over an existing secret of the workspace. Audited as `secret.replaced`.
pub async fn replace_secret(
    pool: &PgPool,
    context: &Context,
    keyring: &Keyring,
    workspace: WorkspaceId,
    id: SecretId,
    plaintext: &SecretBytes,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let stored = secrets::get(&mut *tx, workspace, id)
        .await?
        .ok_or(AdminError::NotFound("secret"))?;
    let sealed = seal(
        keyring,
        plaintext.expose(),
        &secret_aad(workspace, id, stored.purpose),
    );
    // A replace that matched no row (the secret vanished since the read) is not a success: the
    // transaction rolls back and nothing is audited.
    if !secrets::replace(&mut tx, &keyring.key_ids(), workspace, id, &sealed)
        .await
        .map_err(write_refusal)?
    {
        return Err(AdminError::NotFound("secret"));
    }
    audit(
        &mut tx,
        context,
        Some(workspace),
        "secret.replaced",
        Some(("secret", id.as_uuid())),
        json!({"purpose": stored.purpose.as_str(), "key_id": sealed.key_id}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// Deletes a secret of the workspace. Audited as `secret.deleted`.
pub async fn delete_secret(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: SecretId,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    if !secrets::delete(&mut *tx, workspace, id).await? {
        return Err(AdminError::NotFound("secret"));
    }
    audit(
        &mut tx,
        context,
        Some(workspace),
        "secret.deleted",
        Some(("secret", id.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{MasterKey, deployment_key_aad, open};

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

    /// Stores the HMAC root the way startup does, wrapped by the keyring's current key.
    async fn seed_root(pool: &PgPool, keys: &Keyring) {
        let bytes = HmacRoot::bootstrap_from(keys.current()).to_secret_bytes();
        let root = seal(
            keys,
            bytes.expose(),
            &deployment_key_aad(secrets::HMAC_ROOT),
        );
        secrets::insert_hmac_root_if_absent(pool, &root)
            .await
            .unwrap();
    }

    async fn write_with(
        pool: &PgPool,
        keys: &Keyring,
        ws: WorkspaceId,
    ) -> Result<SecretId, AdminError> {
        create_secret(
            pool,
            &Context::system(),
            keys,
            ws,
            SecretPurpose::OAuthToken,
            &SecretBytes::new(b"token".to_vec()),
        )
        .await
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
            owner.password_hash.as_deref().unwrap()
        ));
        assert!(matches!(second, AdminError::Conflict(_)), "{second}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_first_owner_is_audited_as_first_run_setup_and_only_once(pool: PgPool) {
        migrated(&pool).await;

        let owner = create_first_owner(&pool, &Context::anonymous(), "o@example.com", PASSWORD)
            .await
            .unwrap();
        let second =
            create_first_owner(&pool, &Context::anonymous(), "p@example.com", PASSWORD).await;

        assert!(owner.is_owner);
        assert!(matches!(second, Err(AdminError::Conflict(_))));
        let entry = &zirv_flow_store::list_audit(&pool, 10).await.unwrap()[0];
        assert_eq!(entry.action, "user.created");
        assert_eq!(entry.actor_kind, "anonymous");
        assert_eq!(entry.object_id, Some(owner.id.as_uuid()));
        assert_eq!(
            entry.summary,
            json!({ "owner": true, "via": "first-run setup" })
        );
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
            user.password_hash.as_deref().unwrap()
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

    fn keyring(current: &MasterKey, previous: Option<&MasterKey>) -> Keyring {
        let copy = |key: &MasterKey| MasterKey::parse(&key.to_base64()).unwrap();
        Keyring::new(copy(current), previous.map(copy)).unwrap()
    }

    fn actions(entries: &[zirv_flow_store::AuditEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.action.as_str()).collect()
    }

    #[test]
    fn a_secret_error_names_a_key_and_is_an_internal_error_over_http() {
        use axum::response::IntoResponse;

        let error = AdminError::from(SecretError::UnknownKey {
            key_id: "0123456789abcdef".into(),
        });

        assert!(error.to_string().contains("0123456789abcdef"));
        assert_eq!(
            crate::problem::ApiError::from(error)
                .into_response()
                .status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn secret_writes_are_audited_without_the_value(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let keys = keyring(&MasterKey::generate(), None);
        seed_root(&pool, &keys).await;

        let id = create_secret(
            &pool,
            &context,
            &keys,
            ws.id,
            SecretPurpose::OAuthToken,
            &SecretBytes::new(b"hunter2-token".to_vec()),
        )
        .await
        .unwrap();
        replace_secret(
            &pool,
            &context,
            &keys,
            ws.id,
            id,
            &SecretBytes::new(b"hunter3-token".to_vec()),
        )
        .await
        .unwrap();
        let stored = secrets::get(&pool, ws.id, id).await.unwrap().unwrap();
        let aad = secret_aad(ws.id, id, stored.purpose);
        let opened = open(&keys, &stored.sealed, &aad).unwrap();
        delete_secret(&pool, &context, ws.id, id).await.unwrap();
        let missing = delete_secret(&pool, &context, ws.id, id).await;
        let replace_missing = replace_secret(
            &pool,
            &context,
            &keys,
            ws.id,
            id,
            &SecretBytes::new(b"x".to_vec()),
        )
        .await;

        assert_eq!(opened.expose(), b"hunter3-token");
        assert!(matches!(missing, Err(AdminError::NotFound(_))));
        assert!(matches!(replace_missing, Err(AdminError::NotFound(_))));
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let secret_actions: Vec<&str> = actions(&entries)
            .into_iter()
            .filter(|action| action.starts_with("secret."))
            .collect();
        assert_eq!(
            secret_actions,
            ["secret.deleted", "secret.replaced", "secret.created"]
        );
        let text = format!("{entries:?}");
        assert!(!text.contains("hunter"), "{text}");
        assert!(text.contains("oauth_token"), "{text}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotation_rewraps_every_row_once_keeps_ciphertexts_and_is_audited(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let old = MasterKey::generate();
        let new = MasterKey::generate();
        let before = keyring(&old, None);
        seed_root(&pool, &before).await;
        let id = create_secret(
            &pool,
            &context,
            &before,
            ws.id,
            SecretPurpose::ConnectorCredential,
            &SecretBytes::new(b"hunter2".to_vec()),
        )
        .await
        .unwrap();
        let root_bytes = HmacRoot::bootstrap_from(&old).to_secret_bytes();
        let ciphertext = secrets::get(&pool, ws.id, id)
            .await
            .unwrap()
            .unwrap()
            .sealed
            .ciphertext;
        let both = keyring(&new, Some(&old));

        let first = rotate_master_key(&pool, &context, &both).await.unwrap();
        let second = rotate_master_key(&pool, &context, &both).await.unwrap();

        assert_eq!(first.rows, 2);
        assert_eq!(first.from_key_ids, [old.key_id()]);
        assert_eq!(first.key_id, new.key_id());
        assert_eq!(second.rows, 0);
        let only_new = keyring(&new, None);
        let stored = secrets::get(&pool, ws.id, id).await.unwrap().unwrap();
        assert_eq!(stored.sealed.ciphertext, ciphertext);
        assert_eq!(stored.sealed.key_id, new.key_id());
        let aad = secret_aad(ws.id, id, stored.purpose);
        let opened = open(&only_new, &stored.sealed, &aad).unwrap();
        assert_eq!(opened.expose(), b"hunter2");
        let root = secrets::hmac_root(&pool).await.unwrap().unwrap();
        let aad = deployment_key_aad(secrets::HMAC_ROOT);
        let opened = open(&only_new, &root, &aad).unwrap();
        assert_eq!(opened.expose(), root_bytes.expose());
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let rotations: Vec<_> = entries
            .iter()
            .filter(|entry| entry.action == "secrets.master_key_rotated")
            .collect();
        assert_eq!(rotations.len(), 2);
        assert_eq!(rotations[1].summary["rows"], 2);
        assert_eq!(rotations[1].summary["key_id"], new.key_id());
        assert_eq!(rotations[1].summary["from_key_ids"], json!([old.key_id()]));
        assert_eq!(rotations[0].summary["rows"], 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_wrapped_by_an_unknown_key_aborts_the_whole_rotation(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let (a, b, c) = (
            MasterKey::generate(),
            MasterKey::generate(),
            MasterKey::generate(),
        );
        seed_root(&pool, &keyring(&a, None)).await;
        let known = create_secret(
            &pool,
            &context,
            &keyring(&a, None),
            ws.id,
            SecretPurpose::AiProviderKey,
            &SecretBytes::new(b"one".to_vec()),
        )
        .await
        .unwrap();
        // A row wrapped by B, stored directly: a writer on B is refused by the writer check.
        let stray = SecretId::new();
        let sealed = seal(
            &keyring(&b, None),
            b"two",
            &secret_aad(ws.id, stray, SecretPurpose::AiProviderKey),
        );
        secrets::insert_unchecked(&pool, ws.id, SecretPurpose::AiProviderKey, stray, &sealed)
            .await
            .unwrap();

        let error = rotate_master_key(&pool, &context, &keyring(&c, Some(&a)))
            .await
            .unwrap_err();

        assert!(error.to_string().contains(&b.key_id()), "{error}");
        for (id, expected) in [(known, a.key_id()), (stray, b.key_id())] {
            let stored = secrets::get(&pool, ws.id, id).await.unwrap().unwrap();
            assert_eq!(stored.sealed.key_id, expected);
        }
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(!actions(&entries).contains(&"secrets.master_key_rotated"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotation_without_an_hmac_root_fails_and_changes_and_audits_nothing(pool: PgPool) {
        migrated(&pool).await;
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();
        // A row under A exists, but no root does (as in an upgraded deployment).
        let id = SecretId::new();
        let sealed = seal(
            &keyring(&a, None),
            b"x",
            &secret_aad(ws.id, id, SecretPurpose::OAuthToken),
        );
        secrets::insert_unchecked(&pool, ws.id, SecretPurpose::OAuthToken, id, &sealed)
            .await
            .unwrap();

        let error = rotate_master_key(&pool, &Context::system(), &keyring(&b, Some(&a)))
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), secrets::NO_HMAC_ROOT);
        let stored = secrets::get(&pool, ws.id, id).await.unwrap().unwrap();
        assert_eq!(stored.sealed.key_id, a.key_id());
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(!actions(&entries).contains(&"secrets.master_key_rotated"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_write_with_an_outdated_key_is_refused_after_rotation_and_stores_nothing(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();
        seed_root(&pool, &keyring(&a, None)).await;
        rotate_master_key(&pool, &Context::system(), &keyring(&b, Some(&a)))
            .await
            .unwrap();

        let error = write_with(&pool, &keyring(&a, None), ws.id)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("outdated"), "{error}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(!actions(&entries).contains(&"secret.created"));
        // A restarted process on the new key writes.
        write_with(&pool, &keyring(&b, None), ws.id).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn writes_in_the_rotation_window_succeed_and_are_wrapped_by_the_writers_key(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();
        seed_root(&pool, &keyring(&a, None)).await;

        // Root on A: a B-current/A-previous writer and a stale A-only writer both write.
        let by_b = write_with(&pool, &keyring(&b, Some(&a)), ws.id)
            .await
            .unwrap();
        let by_a = write_with(&pool, &keyring(&a, None), ws.id).await.unwrap();

        let stored = secrets::get(&pool, ws.id, by_b).await.unwrap().unwrap();
        assert_eq!(stored.sealed.key_id, b.key_id());
        let report = rotate_master_key(&pool, &Context::system(), &keyring(&b, Some(&a)))
            .await
            .unwrap();
        assert_eq!(report.rows, 2, "the root and the stale writer's row");
        let stored = secrets::get(&pool, ws.id, by_a).await.unwrap().unwrap();
        assert_eq!(stored.sealed.key_id, b.key_id());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_write_without_an_hmac_root_is_refused(pool: PgPool) {
        migrated(&pool).await;
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();

        let error = write_with(&pool, &keyring(&MasterKey::generate(), None), ws.id)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no HMAC root"), "{error}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replace_that_matches_no_row_is_not_found_and_writes_no_audit_entry(pool: PgPool) {
        use std::time::Duration;

        migrated(&pool).await;
        let a = MasterKey::generate();
        let keys = keyring(&a, None);
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();
        seed_root(&pool, &keys).await;
        let id = write_with(&pool, &keys, ws.id).await.unwrap();
        // A delete in flight holds the row: the replace reads it, then waits on the update.
        let mut deleting = zirv_flow_store::begin(&pool).await.unwrap();
        assert!(secrets::delete(&mut *deleting, ws.id, id).await.unwrap());

        let replacing = {
            let pool = pool.clone();
            let keys = keyring(&a, None);
            let ws = ws.id;
            tokio::spawn(async move {
                replace_secret(
                    &pool,
                    &Context::system(),
                    &keys,
                    ws,
                    id,
                    &SecretBytes::new(b"new".to_vec()),
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!replacing.is_finished(), "the replace waits for the delete");
        zirv_flow_store::commit(deleting).await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(10), replacing)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(AdminError::NotFound("secret"))));
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(!actions(&entries).contains(&"secret.replaced"));
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stale_insert_waits_for_a_rotation_and_is_refused_after_it(pool: PgPool) {
        use std::time::Duration;

        migrated(&pool).await;
        let (a, b) = (MasterKey::generate(), MasterKey::generate());
        let ws = create_workspace(&pool, &Context::system(), "Acme", None)
            .await
            .unwrap();
        let only_a = keyring(&a, None);
        seed_root(&pool, &only_a).await;
        // An in-progress rotation: it re-wraps the root to B and holds the root lock until it
        // commits.
        let rotated_keys = keyring(&b, Some(&a));
        let mut rotation = zirv_flow_store::begin(&pool).await.unwrap();
        let report = secrets::rotate(&mut rotation, &b.key_id(), |sealed| {
            rewrap(&rotated_keys, sealed)
        })
        .await
        .unwrap();
        assert_eq!(report.rows, 1);

        let writer = {
            let pool = pool.clone();
            let ws = ws.id;
            tokio::spawn(async move { write_with(&pool, &only_a, ws).await })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!writer.is_finished(), "the insert waits for the rotation");

        zirv_flow_store::commit(rotation).await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(10), writer)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("outdated"), "{error}");
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
