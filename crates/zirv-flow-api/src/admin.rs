//! The mutating operations on accounts, workspaces, memberships and API keys.
//!
//! The HTTP handlers and the `zirv-flow admin` commands both call these functions, so
//! the rules and the audit trail are the same on both paths. Every operation changes the
//! data and appends its audit row in one transaction. Audit summaries never carry
//! emails, passwords, tokens or key secrets.

use std::error::Error;
use std::fmt;
use std::path::Path;

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::{
    ApiKeyId, Cause, Email, HmacPurpose, HmacRoot, Keyring, Role, Scope, SecretBytes, SecretError,
    SecretId, SecretPurpose, Slug, TraceContext, UserId, ValidationError, WorkspaceId,
    WorkspaceName, password, rewrap, seal, secret_aad,
};
use zirv_flow_store::secrets;
use zirv_flow_store::{
    Actor, ActorKind, AiSettings, AiSettingsUpdate, ApiKey, DeploymentRetention, Membership,
    NewAuditEntry, PgConnection, PgPool, ReplayReport, StoreError, User, UtmSettings, Workspace,
    WorkspaceRetention,
};

use crate::{credentials, passwords};

/// Who is acting and under which request, for the audit trail.
#[derive(Clone, Debug)]
pub struct Context {
    actor: Actor,
    request_id: Option<String>,
    owner: bool,
}

impl Context {
    /// Work done by the operator on the command line.
    pub fn system() -> Self {
        Self {
            actor: Actor::system(),
            request_id: None,
            owner: false,
        }
    }

    /// A caller who has not proved who they are, such as a failed sign-in.
    pub fn anonymous() -> Self {
        Self {
            actor: Actor::anonymous(),
            request_id: None,
            owner: false,
        }
    }

    pub fn user(id: UserId) -> Self {
        Self {
            actor: Actor::user(id.as_uuid()),
            request_id: None,
            owner: false,
        }
    }

    pub fn api_key(id: ApiKeyId) -> Self {
        Self {
            actor: Actor::api_key(id.as_uuid()),
            request_id: None,
            owner: false,
        }
    }

    /// Marks the actor as the instance owner, who may remove or demote a workspace's last admin.
    pub fn with_owner(mut self, owner: bool) -> Self {
        self.owner = owner;
        self
    }

    /// Whether the actor may take away a workspace's last admin: the instance owner, or the
    /// operator on the command line.
    fn may_remove_last_admin(&self) -> bool {
        self.owner || self.actor.kind == zirv_flow_store::ActorKind::System
    }

    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }
}

/// The refusal text of [`AdminError::LastAdmin`].
pub(crate) const LAST_ADMIN: &str =
    "the workspace would have no admin; only the owner can remove or demote its last admin";

/// Why an operation did not happen. The messages are safe to show to callers.
#[derive(Debug)]
pub enum AdminError {
    Invalid(ValidationError),
    Conflict(&'static str),
    NotFound(&'static str),
    /// The change would leave the workspace with no admin and the caller is not the owner; the
    /// problem carries the code `last_admin`.
    LastAdmin,
    Store(StoreError),
    /// A ledger file that cannot be merged; names the file, never a value.
    Merge(String),
    /// An erasure ledger file or directory that could not be read or written. Names a file or
    /// directory, never a value.
    Ledger(zirv_flow_store::LedgerFileError),
    /// A trait slot change that was refused; the problem carries its own code.
    Slot(zirv_flow_store::SlotError),
    /// A stored secret could not be re-wrapped. Names a key id, never key or plaintext bytes.
    Secret(SecretError),
    /// A UTM setting that was refused; the problem carries the code (`utm_pattern_invalid`,
    /// `utm_domain_invalid` or `utm_domains_required`). The detail names the field and the rule,
    /// never a value other than the refused placeholder.
    Utm {
        code: &'static str,
        detail: String,
    },
    /// A retention window outside the deployment bounds; the problem carries the code
    /// `retention_out_of_bounds` and the detail names the field and the bounds.
    RetentionBounds(String),
}

impl fmt::Display for AdminError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(formatter),
            Self::Conflict(message) => formatter.write_str(message),
            Self::NotFound(what) => write!(formatter, "no such {what}"),
            Self::LastAdmin => formatter.write_str(LAST_ADMIN),
            Self::Store(error) => error.fmt(formatter),
            Self::Ledger(error) => error.fmt(formatter),
            Self::Merge(message) => formatter.write_str(message),
            Self::Slot(error) => write!(formatter, "{error:?}"),
            Self::Secret(error) => error.fmt(formatter),
            Self::Utm { detail, .. } | Self::RetentionBounds(detail) => formatter.write_str(detail),
        }
    }
}

impl Error for AdminError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Store(error) => Some(error),
            Self::Ledger(error) => Some(error),
            Self::Secret(error) => Some(error),
            Self::Conflict(_)
            | Self::NotFound(_)
            | Self::LastAdmin
            | Self::Merge(_)
            | Self::Slot(_)
            | Self::Utm { .. }
            | Self::RetentionBounds(_) => None,
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

impl From<zirv_flow_store::LedgerFileError> for AdminError {
    fn from(error: zirv_flow_store::LedgerFileError) -> Self {
        Self::Ledger(error)
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
            trace_id: crate::trace::current().map(|trace| trace.trace_id.as_uuid()),
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

/// Refuses to take `user` out of the admin role when they are the workspace's only admin, unless
/// the caller is the owner (or the operator). The memberships are locked until the transaction
/// ends, so two concurrent demotions cannot each leave the other admin in place.
async fn refuse_last_admin(
    tx: &mut PgConnection,
    context: &Context,
    workspace: WorkspaceId,
    user: UserId,
) -> Result<(), AdminError> {
    let members = zirv_flow_store::lock_memberships(tx, workspace).await?;
    if context.may_remove_last_admin() {
        return Ok(());
    }
    let is_admin = members
        .iter()
        .any(|(member, role)| *member == user && *role == Role::Admin);
    let admins = members
        .iter()
        .filter(|(_, role)| *role == Role::Admin)
        .count();
    if is_admin && admins == 1 {
        return Err(AdminError::LastAdmin);
    }
    Ok(())
}

/// Adds a user to a workspace or changes their role. A new member has no `view_pii` grant; an
/// existing one keeps theirs.
pub async fn set_member(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    user: UserId,
    role: Role,
) -> Result<Membership, AdminError> {
    set_member_grant(pool, context, workspace, user, role, None).await
}

/// As [`set_member`], and sets the `view_pii` grant when it is `Some`.
pub async fn set_member_grant(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    user: UserId,
    role: Role,
    view_pii: Option<bool>,
) -> Result<Membership, AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    zirv_flow_store::find_user(&mut *tx, user)
        .await?
        .ok_or(AdminError::NotFound("user"))?;
    if role != Role::Admin {
        refuse_last_admin(&mut tx, context, workspace, user).await?;
    }
    let membership =
        zirv_flow_store::set_membership(&mut *tx, workspace, user, role, view_pii).await?;
    audit(
        &mut tx,
        context,
        Some(workspace),
        "membership.set",
        Some(("user", user.as_uuid())),
        json!({ "role": role.as_str(), "view_pii": membership.view_pii }),
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
    refuse_last_admin(&mut tx, context, workspace, user).await?;
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
    let mut tx = zirv_flow_store::begin(pool).await?;
    let created = create_api_key_in(&mut tx, context, workspace, name, scopes).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(created)
}

/// [`create_api_key`] inside the caller's transaction, which the caller commits.
pub(crate) async fn create_api_key_in(
    tx: &mut zirv_flow_store::Tx,
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

    let key = zirv_flow_store::create_api_key(
        &mut **tx,
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
        tx,
        context,
        Some(workspace),
        "api_key.created",
        Some(("api_key", id.as_uuid())),
        json!({ "scopes": scope_names }),
    )
    .await?;
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

/// The id of the workspace with this slug.
pub async fn workspace_by_slug(pool: &PgPool, slug: &str) -> Result<WorkspaceId, AdminError> {
    let slug = Slug::parse(slug)?;
    Ok(zirv_flow_store::find_workspace_by_slug(pool, &slug)
        .await?
        .ok_or(AdminError::NotFound("workspace"))?
        .id)
}

/// What [`replay_erasures`] did: the counts, and the ledger files it could not use.
#[derive(Debug)]
pub struct ErasureReplay {
    pub report: ReplayReport,
    /// The files of `from` that were corrupt or of an unknown format, in file-name order. They
    /// name a file, never a value. The caller must report them and fail.
    pub bad_files: Vec<zirv_flow_store::LedgerFileError>,
}

/// Re-applies the completed erasure ledger of one workspace (by slug) or of all of them after a
/// backup restore: re-adds the `erased` suppressions, erases every ledger person that exists
/// again, and removes pending inbox rows that name an erased identifier. Returns counts only,
/// and audits them as `privacy.erasures_replayed`.
///
/// With `from`, the ledger files of that directory are read one by one. A corrupt or
/// unknown-format file does not stop the others: it is listed in [`ErasureReplay::bad_files`] and
/// every good entry is still applied. The entries the database lacks (of the workspace, when one
/// is given) are merged in as completed, files for a workspace that does not exist here are
/// skipped and counted, and the replay runs over the merged ledger.
pub async fn replay_erasures(
    pool: &PgPool,
    context: &Context,
    workspace: Option<&str>,
    root: &HmacRoot,
    from: Option<&Path>,
) -> Result<ErasureReplay, AdminError> {
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
    let mut entries = Vec::new();
    let mut bad_files = Vec::new();
    if let Some(dir) = from {
        for read in zirv_flow_store::read_ledger_dir_each(dir)? {
            match read {
                Ok(entry) => entries.push(entry),
                Err(error) => bad_files.push(error),
            }
        }
    }
    if let Some(only) = workspace {
        entries.retain(|entry| entry.workspace_id == only.as_uuid());
    }
    let merge = zirv_flow_store::merge_ledger_entries(pool, &entries)
        .await
        .map_err(|error| match error {
            zirv_flow_store::MergeError::Store(error) => AdminError::Store(error),
            other @ zirv_flow_store::MergeError::IdInOtherWorkspace { .. } => {
                AdminError::Merge(other.to_string())
            }
        })?;
    let key = root.hmac_key(HmacPurpose::Identifier);
    let trace = TraceContext::root(Cause::api("admin.replay-erasures"));
    let mut report = zirv_flow_store::replay_erasures(pool, workspace, Some(&key), &trace).await?;
    report.merged = merge.merged;
    report.skipped_workspaces = merge.skipped_workspaces;
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
            "merged": report.merged,
            "skipped_workspaces": report.skipped_workspaces,
            "bad_files": bad_files.len(),
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(ErasureReplay { report, bad_files })
}

/// Writes a ledger file into `out` for every completed erasure of the database, a full snapshot
/// that also covers entries made before `privacy.ledger_dir` was set. Returns counts. Like the
/// other operations that only read the database it writes no audit entry.
pub async fn export_ledger(
    pool: &PgPool,
    out: &Path,
) -> Result<zirv_flow_store::ExportReport, AdminError> {
    zirv_flow_store::export_ledger(pool, out)
        .await
        .map_err(|error| match error {
            zirv_flow_store::ExportError::Store(error) => AdminError::Store(error),
            zirv_flow_store::ExportError::File(error) => AdminError::Ledger(error),
        })
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
pub(crate) fn write_refusal(error: secrets::SecretWriteError) -> AdminError {
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
    let mut tx = zirv_flow_store::begin(pool).await?;
    let id = create_secret_in(&mut tx, context, keyring, workspace, purpose, plaintext).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(id)
}

/// As [`create_secret`], inside the caller's transaction, so the secret and the row that
/// refers to it commit or roll back together.
pub(crate) async fn create_secret_in(
    tx: &mut PgConnection,
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
    secrets::insert(tx, &keyring.key_ids(), workspace, purpose, id, &sealed)
        .await
        .map_err(write_refusal)?;
    audit(
        tx,
        context,
        Some(workspace),
        "secret.created",
        Some(("secret", id.as_uuid())),
        json!({"purpose": purpose.as_str(), "key_id": sealed.key_id}),
    )
    .await?;
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
    replace_secret_in(&mut tx, context, keyring, workspace, id, plaintext).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// As [`replace_secret`], inside the caller's transaction.
pub(crate) async fn replace_secret_in(
    tx: &mut PgConnection,
    context: &Context,
    keyring: &Keyring,
    workspace: WorkspaceId,
    id: SecretId,
    plaintext: &SecretBytes,
) -> Result<(), AdminError> {
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
    if !secrets::replace(tx, &keyring.key_ids(), workspace, id, &sealed)
        .await
        .map_err(write_refusal)?
    {
        return Err(AdminError::NotFound("secret"));
    }
    audit(
        tx,
        context,
        Some(workspace),
        "secret.replaced",
        Some(("secret", id.as_uuid())),
        json!({"purpose": stored.purpose.as_str(), "key_id": sealed.key_id}),
    )
    .await?;
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
    audit_secret_deleted(&mut tx, context, workspace, id).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The audit entry of a secret deleted in the caller's transaction by another store path, such
/// as deleting the source that owned it.
pub(crate) async fn audit_secret_deleted(
    tx: &mut PgConnection,
    context: &Context,
    workspace: WorkspaceId,
    id: SecretId,
) -> Result<(), StoreError> {
    audit(
        tx,
        context,
        Some(workspace),
        "secret.deleted",
        Some(("secret", id.as_uuid())),
        json!({}),
    )
    .await
}

/// The new state of a workspace's assistant settings. A key is sealed into an
/// `ai_provider_key` secret in the same transaction that stores the settings.
pub struct AiSettingsChange {
    pub enabled: bool,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub monthly_budget_micros: i64,
    pub pii_opt_in: bool,
    pub transcript_days: Option<i32>,
    /// A new provider key; `None` keeps the current one.
    pub provider_key: Option<SecretBytes>,
    /// The result of the capability probe the caller ran for this change, if it ran one.
    pub probe: Option<Value>,
}

impl fmt::Debug for AiSettingsChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AiSettingsChange")
            .field("enabled", &self.enabled)
            .field("provider_id", &self.provider_id)
            .field("model", &self.model)
            .field(
                "provider_key",
                &self.provider_key.as_ref().map(|_| "[redacted]"),
            )
            .finish_non_exhaustive()
    }
}

/// Saves a workspace's assistant settings. Audited as `ai.settings.updated` with the names of
/// the changed fields and their new non-secret values, never the key; a key write also audits
/// `secret.created` or `secret.replaced`. Settings and key are stored or refused together.
pub async fn save_ai_settings(
    pool: &PgPool,
    context: &Context,
    keyring: Option<&Keyring>,
    workspace: WorkspaceId,
    change: &AiSettingsChange,
) -> Result<AiSettings, AdminError> {
    if change.monthly_budget_micros < 0 {
        return Err(ValidationError::new("monthly_budget_micros", "must not be negative").into());
    }
    if change
        .transcript_days
        .is_some_and(|days| !(1..=30).contains(&days))
    {
        return Err(ValidationError::new("transcript_days", "must be between 1 and 30").into());
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    let current = zirv_flow_store::lock_ai_settings(&mut tx, workspace)
        .await?
        .unwrap_or_else(|| AiSettings::defaults(workspace));

    let mut secret_id = current.provider_key_secret_id;
    if let Some(key) = &change.provider_key {
        let keyring = keyring.ok_or(AdminError::Conflict(
            "stored secrets are not configured on this deployment",
        ))?;
        let purpose = SecretPurpose::AiProviderKey;
        let (id, action) = match secret_id {
            Some(id) => (id, "secret.replaced"),
            None => (SecretId::new(), "secret.created"),
        };
        let sealed = seal(keyring, key.expose(), &secret_aad(workspace, id, purpose));
        if action == "secret.created" {
            secrets::insert(&mut tx, &keyring.key_ids(), workspace, purpose, id, &sealed)
                .await
                .map_err(write_refusal)?;
        } else if !secrets::replace(&mut tx, &keyring.key_ids(), workspace, id, &sealed)
            .await
            .map_err(write_refusal)?
        {
            return Err(AdminError::NotFound("secret"));
        }
        audit(
            &mut tx,
            context,
            Some(workspace),
            action,
            Some(("secret", id.as_uuid())),
            json!({"purpose": purpose.as_str(), "key_id": sealed.key_id}),
        )
        .await?;
        secret_id = Some(id);
    }

    let updated_by = (context.actor.kind == ActorKind::User)
        .then_some(context.actor.id)
        .flatten()
        .map(UserId::from_uuid);
    let saved = zirv_flow_store::put_ai_settings(
        &mut *tx,
        workspace,
        &AiSettingsUpdate {
            enabled: change.enabled,
            provider_id: change.provider_id.clone(),
            model: change.model.clone(),
            monthly_budget_micros: change.monthly_budget_micros,
            pii_opt_in: change.pii_opt_in,
            transcript_days: change.transcript_days,
            provider_key_secret_id: secret_id,
            last_probe: change.probe.clone(),
            updated_by,
        },
    )
    .await?;

    let mut changed = Vec::new();
    let mut values = serde_json::Map::new();
    let mut note = |field: &'static str, differs: bool, value: Value| {
        if differs {
            changed.push(field);
            values.insert(field.to_owned(), value);
        }
    };
    note(
        "enabled",
        current.enabled != saved.enabled,
        json!(saved.enabled),
    );
    note(
        "provider_id",
        current.provider_id != saved.provider_id,
        json!(saved.provider_id),
    );
    note("model", current.model != saved.model, json!(saved.model));
    note(
        "monthly_budget_micros",
        current.monthly_budget_micros != saved.monthly_budget_micros,
        json!(saved.monthly_budget_micros),
    );
    note(
        "pii_opt_in",
        current.pii_opt_in != saved.pii_opt_in,
        json!(saved.pii_opt_in),
    );
    note(
        "transcript_days",
        current.transcript_days != saved.transcript_days,
        json!(saved.transcript_days),
    );
    if change.provider_key.is_some() {
        // The key is a changed field by name only: no value, not even a mask.
        changed.push("provider_key");
    }
    if !changed.is_empty() {
        audit(
            &mut tx,
            context,
            Some(workspace),
            "ai.settings.updated",
            Some(("ai_settings", workspace.as_uuid())),
            json!({"changed": changed, "after": values}),
        )
        .await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(saved)
}

/// Validates the UTM setting the caller sent: the patterns may only use the fixed placeholder set,
/// so a person or event field is refused; every domain is normalised; enabling with no domain is
/// refused.
fn validate_utm(change: &UtmSettings) -> Result<UtmSettings, AdminError> {
    use zirv_flow_templates::{MAX_DOMAINS, normalize_domain, validate_utm_pattern};

    let patterns = [
        ("utm.source", &change.source),
        ("utm.medium", &change.medium),
        ("utm.campaign", &change.campaign),
        ("utm.content", &change.content),
    ];
    for (field, pattern) in patterns {
        validate_utm_pattern(pattern).map_err(|error| AdminError::Utm {
            code: "utm_pattern_invalid",
            detail: format!("{field}: {error}"),
        })?;
    }
    if change.domains.len() > MAX_DOMAINS {
        return Err(AdminError::Utm {
            code: "utm_domain_invalid",
            detail: format!("utm.domains: at most {MAX_DOMAINS} domains"),
        });
    }
    let mut domains: Vec<String> = Vec::with_capacity(change.domains.len());
    for domain in &change.domains {
        let normal = normalize_domain(domain).ok_or_else(|| AdminError::Utm {
            code: "utm_domain_invalid",
            detail: "utm.domains: each entry must be a domain name such as example.com".to_owned(),
        })?;
        if !domains.contains(&normal) {
            domains.push(normal);
        }
    }
    if change.enabled && domains.is_empty() {
        return Err(AdminError::Utm {
            code: "utm_domains_required",
            detail: "utm.domains: list at least one domain before enabling UTM parameters"
                .to_owned(),
        });
    }
    Ok(UtmSettings {
        enabled: change.enabled,
        source: change.source.clone(),
        medium: change.medium.clone(),
        campaign: change.campaign.clone(),
        content: change.content.clone(),
        domains,
    })
}

/// Saves a workspace's settings, the time zone and the automatic UTM setting (#240), in one
/// transaction: a refused part changes nothing. The caller has already checked the time zone name.
/// Audited as `workspace.updated` for the time zone and `workspace.utm_updated` for the UTM setting
/// (the switch and the number of domains, never a pattern or a domain).
pub async fn update_workspace(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    timezone: Option<&str>,
    utm: Option<&UtmSettings>,
) -> Result<Workspace, AdminError> {
    let settings = utm.map(validate_utm).transpose()?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let mut saved = None;
    if let Some(settings) = &settings {
        saved = Some(
            zirv_flow_store::set_workspace_utm(&mut *tx, workspace, settings)
                .await?
                .ok_or(AdminError::NotFound("workspace"))?,
        );
        audit(
            &mut tx,
            context,
            Some(workspace),
            "workspace.utm_updated",
            Some(("workspace", workspace.as_uuid())),
            json!({ "enabled": settings.enabled, "domains": settings.domains.len() }),
        )
        .await?;
    }
    if let Some(timezone) = timezone {
        let updated = zirv_flow_store::set_workspace_timezone(&mut *tx, workspace, timezone)
            .await?
            .ok_or(AdminError::NotFound("workspace"))?;
        audit(
            &mut tx,
            context,
            Some(workspace),
            "workspace.updated",
            Some(("workspace", workspace.as_uuid())),
            json!({ "timezone": updated.timezone }),
        )
        .await?;
        saved = Some(updated);
    }
    zirv_flow_store::commit(tx).await?;
    saved.ok_or(AdminError::NotFound("workspace"))
}

/// Checks one window a workspace may shorten: not below the deployment minimum and not above the
/// deployment window. A null returns to the deployment value. The refusal names the bounds.
fn check_window(
    field: &'static str,
    days: Option<i64>,
    min: u32,
    max: u32,
    deployment: u32,
) -> Result<Option<i32>, AdminError> {
    let Some(days) = days else {
        return Ok(None);
    };
    let longest = deployment.min(max);
    if days < i64::from(min) || days > i64::from(longest) {
        return Err(AdminError::RetentionBounds(format!(
            "{field}: must be between {min} and {longest} days (the deployment window is {deployment} days and its bounds are {min} to {max})"
        )));
    }
    Ok(i32::try_from(days).ok())
}

/// Changes a workspace's own message and decision-log windows (ruling d732a27c). Each is in days
/// and must lie within the deployment bounds and not exceed the deployment window. A window left
/// out (`None`) keeps its stored value and `Some(None)` returns it to the deployment value. The
/// other windows are the deployment's alone. Audited as
/// `retention.updated` with the window names and day counts only.
pub async fn set_workspace_retention(
    pool: &PgPool,
    context: &Context,
    deployment: &DeploymentRetention,
    workspace: WorkspaceId,
    messages_days: Option<Option<i64>>,
    decision_days: Option<Option<i64>>,
) -> Result<WorkspaceRetention, AdminError> {
    let messages = messages_days
        .map(|days| {
            check_window(
                "messages_days",
                days,
                deployment.messages.min_days,
                deployment.messages.max_days,
                deployment.messages.days,
            )
        })
        .transpose()?;
    let decisions = decision_days
        .map(|days| {
            check_window(
                "decision_days",
                days,
                deployment.decisions.min_days,
                deployment.decisions.max_days,
                deployment.decisions.days,
            )
        })
        .transpose()?;
    let updated_by = (context.actor.kind == ActorKind::User)
        .then_some(context.actor.id)
        .flatten();
    let mut tx = zirv_flow_store::begin(pool).await?;
    let saved = zirv_flow_store::set_workspace_retention(
        &mut tx, workspace, messages, decisions, updated_by,
    )
    .await?
    .ok_or(AdminError::NotFound("workspace"))?;
    let mut summary = serde_json::Map::new();
    if messages.is_some() {
        summary.insert("messages_days".into(), json!(saved.messages_days));
    }
    if decisions.is_some() {
        summary.insert("decision_days".into(), json!(saved.decision_days));
    }
    audit(
        &mut tx,
        context,
        Some(workspace),
        "retention.updated",
        Some(("workspace", workspace.as_uuid())),
        serde_json::Value::Object(summary),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{MasterKey, deployment_key_aad, open};

    use super::*;

    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn a_window_must_lie_between_the_minimum_and_the_deployment_window() {
        assert_eq!(check_window("w", None, 30, 1095, 395).unwrap(), None);
        assert_eq!(
            check_window("w", Some(30), 30, 1095, 395).unwrap(),
            Some(30)
        );
        assert_eq!(
            check_window("w", Some(395), 30, 1095, 395).unwrap(),
            Some(395)
        );
        for refused in [29, 0, -5, 396, 1000] {
            let error = check_window("w", Some(refused), 30, 1095, 395).unwrap_err();
            let text = error.to_string();
            assert!(text.contains("w: must be between 30 and 395"), "{text}");
            assert!(text.contains("1095"), "{text}");
        }
    }

    #[test]
    fn a_window_never_exceeds_the_maximum_even_when_the_deployment_window_does() {
        assert!(check_window("w", Some(800), 30, 760, 900).is_err());
        assert_eq!(
            check_window("w", Some(760), 30, 760, 900).unwrap(),
            Some(760)
        );
    }

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
        set_member(&pool, &context, ws.id, user.id, Role::Editor)
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
    async fn membership_set_audits_the_grant_as_a_boolean_and_never_the_email(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let user = create_user(&pool, &context, "carol@example.com", PASSWORD)
            .await
            .unwrap();
        let ws = create_workspace(&pool, &context, "Acme Corp", None)
            .await
            .unwrap();

        set_member_grant(&pool, &context, ws.id, user.id, Role::Editor, Some(true))
            .await
            .unwrap();
        let kept = set_member(&pool, &context, ws.id, user.id, Role::Viewer)
            .await
            .unwrap();

        assert!(kept.view_pii);
        let summaries: Vec<serde_json::Value> = zirv_flow_store::list_audit(&pool, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(|row| row.action == "membership.set")
            .map(|row| row.summary)
            .collect();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.contains(&json!({ "role": "editor", "view_pii": true })));
        assert!(summaries.contains(&json!({ "role": "viewer", "view_pii": true })));
        assert!(
            summaries
                .iter()
                .all(|summary| !summary.to_string().contains("carol"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_last_admin_stays_unless_the_caller_is_the_owner_or_the_operator(pool: PgPool) {
        migrated(&pool).await;
        let operator = Context::system();
        let admin_user = create_user(&pool, &operator, "a@example.com", PASSWORD)
            .await
            .unwrap();
        let other = create_user(&pool, &operator, "b@example.com", PASSWORD)
            .await
            .unwrap();
        let ws = create_workspace(&pool, &operator, "Acme", None)
            .await
            .unwrap();
        set_member(&pool, &operator, ws.id, admin_user.id, Role::Admin)
            .await
            .unwrap();
        let as_admin = Context::user(admin_user.id);

        let demoted = set_member(&pool, &as_admin, ws.id, admin_user.id, Role::Publisher).await;
        let removed = remove_member(&pool, &as_admin, ws.id, admin_user.id).await;
        // Adding a second member, or keeping the role, is not a demotion.
        set_member(&pool, &as_admin, ws.id, other.id, Role::Viewer)
            .await
            .unwrap();
        set_member(&pool, &as_admin, ws.id, admin_user.id, Role::Admin)
            .await
            .unwrap();
        let by_owner = Context::user(admin_user.id).with_owner(true);
        set_member(&pool, &by_owner, ws.id, admin_user.id, Role::Publisher)
            .await
            .unwrap();
        assert_eq!(
            zirv_flow_store::membership_role(&pool, ws.id, admin_user.id)
                .await
                .unwrap(),
            Some(Role::Publisher)
        );
        for user in [admin_user.id, other.id] {
            set_member(&pool, &operator, ws.id, user, Role::Admin)
                .await
                .unwrap();
        }
        let with_two = set_member(&pool, &as_admin, ws.id, other.id, Role::Viewer).await;

        assert!(matches!(demoted, Err(AdminError::LastAdmin)));
        assert!(matches!(removed, Err(AdminError::LastAdmin)));
        assert!(with_two.is_ok());
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

    fn ai_change() -> AiSettingsChange {
        AiSettingsChange {
            enabled: true,
            provider_id: Some("openai".into()),
            model: Some("gpt-x".into()),
            monthly_budget_micros: 5_000_000,
            pii_opt_in: false,
            transcript_days: Some(7),
            provider_key: Some(SecretBytes::new(b"sk-live-hunter2".to_vec())),
            probe: Some(json!({"ok": true, "missing": []})),
        }
    }

    #[test]
    fn a_settings_change_hides_the_key_in_debug_output() {
        let printed = format!("{:?}", ai_change());
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("[redacted]"), "{printed}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn saving_ai_settings_seals_the_key_and_audits_the_changed_fields_without_it(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let keys = keyring(&MasterKey::generate(), None);
        seed_root(&pool, &keys).await;

        let saved = save_ai_settings(&pool, &context, Some(&keys), ws.id, &ai_change())
            .await
            .unwrap();
        // A second save with no new key changes only the budget and keeps the sealed key.
        let second = AiSettingsChange {
            monthly_budget_micros: 9_000_000,
            provider_key: None,
            probe: None,
            ..ai_change()
        };
        let again = save_ai_settings(&pool, &context, Some(&keys), ws.id, &second)
            .await
            .unwrap();
        // The same values again change nothing and audit nothing.
        save_ai_settings(&pool, &context, Some(&keys), ws.id, &second)
            .await
            .unwrap();
        let replaced = AiSettingsChange {
            provider_key: Some(SecretBytes::new(b"sk-live-hunter3".to_vec())),
            ..second
        };
        save_ai_settings(&pool, &context, Some(&keys), ws.id, &replaced)
            .await
            .unwrap();

        let secret = saved.provider_key_secret_id.unwrap();
        assert_eq!(again.provider_key_secret_id, Some(secret));
        let stored = secrets::get(&pool, ws.id, secret).await.unwrap().unwrap();
        assert_eq!(stored.purpose, SecretPurpose::AiProviderKey);
        let opened = open(
            &keys,
            &stored.sealed,
            &secret_aad(ws.id, secret, stored.purpose),
        )
        .unwrap();
        assert_eq!(opened.expose(), b"sk-live-hunter3");
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        let relevant: Vec<&str> = actions(&entries)
            .into_iter()
            .filter(|action| action.starts_with("ai.") || action.starts_with("secret."))
            .collect();
        assert_eq!(
            relevant,
            [
                "ai.settings.updated",
                "secret.replaced",
                "ai.settings.updated",
                "ai.settings.updated",
                "secret.created"
            ]
        );
        let updates: Vec<&Value> = entries
            .iter()
            .filter(|entry| entry.action == "ai.settings.updated")
            .map(|entry| &entry.summary)
            .collect();
        // Newest first: the key replacement, the budget change, then the first save.
        assert_eq!(updates[0]["changed"], json!(["provider_key"]));
        assert_eq!(updates[1]["changed"], json!(["monthly_budget_micros"]));
        assert_eq!(updates[1]["after"]["monthly_budget_micros"], 9_000_000);
        assert!(
            updates[2]["changed"]
                .as_array()
                .unwrap()
                .contains(&json!("transcript_days"))
        );
        assert_eq!(updates[2]["after"]["transcript_days"], 7);
        let text = format!("{entries:?}");
        assert!(!text.contains("hunter"), "{text}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn invalid_ai_settings_and_a_key_without_a_keyring_store_nothing(pool: PgPool) {
        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let keys = keyring(&MasterKey::generate(), None);
        seed_root(&pool, &keys).await;

        for bad in [
            AiSettingsChange {
                monthly_budget_micros: -1,
                ..ai_change()
            },
            AiSettingsChange {
                transcript_days: Some(31),
                ..ai_change()
            },
            AiSettingsChange {
                transcript_days: Some(0),
                ..ai_change()
            },
        ] {
            let error = save_ai_settings(&pool, &context, Some(&keys), ws.id, &bad).await;
            assert!(matches!(error, Err(AdminError::Invalid(_))));
        }
        let no_ring = save_ai_settings(&pool, &context, None, ws.id, &ai_change()).await;

        assert!(matches!(no_ring, Err(AdminError::Conflict(_))));
        assert!(
            zirv_flow_store::find_ai_settings(&pool, ws.id)
                .await
                .unwrap()
                .is_none()
        );
        let entries = zirv_flow_store::list_audit(&pool, 100).await.unwrap();
        assert!(!actions(&entries).iter().any(|a| a.starts_with("ai.")));
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

    const ALLOWED_ADDRESS: &str = "rot@example.com";

    /// A legacy source or destination credential: the sealed row and the `credentials` row that
    /// points at it, which is how `open_credential` reaches it. Returns the credential id.
    async fn seed_legacy(
        pool: &PgPool,
        keys: &Keyring,
        ws: WorkspaceId,
        purpose: SecretPurpose,
    ) -> Uuid {
        let secret = create_secret(
            pool,
            &Context::system(),
            keys,
            ws,
            purpose,
            &SecretBytes::new(br#"{"password":"legacy-value"}"#.to_vec()),
        )
        .await
        .unwrap();
        zirv_flow_store::create_credential(
            pool,
            ws,
            &zirv_flow_store::NewWorkspaceCredential {
                id: Uuid::now_v7(),
                key: purpose.as_str(),
                name: purpose.as_str(),
                kind: "basic",
                schema: None,
                fields: &json!({"username": "u"}),
                secret_id: secret,
                hints: &json!({"password": ""}),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Stores one secret of `purpose` through the writer its feature uses and returns the id of
    /// the row that refers to it (the source, hook, domain or credential), or nil when the
    /// secret is found some other way. The match has no wildcard, so a new purpose does not
    /// compile until it is seeded here and read back in `read_back`.
    async fn seed_purpose(
        purpose: SecretPurpose,
        state: &crate::AppState,
        keys: &Keyring,
        ws: WorkspaceId,
    ) -> Uuid {
        let context = Context::system();
        let pool = &state.pool;
        match purpose {
            SecretPurpose::ConnectorCredential | SecretPurpose::DestinationCredential => {
                seed_legacy(pool, keys, ws, purpose).await
            }
            SecretPurpose::WorkspaceCredential => {
                let request = serde_json::from_value(json!({
                    "kind": "api_key",
                    "name": "Rotation",
                    "secret": {"api_key": "rotation-value"},
                }))
                .unwrap();
                crate::workspace_credentials::crud::create_credential(
                    state, &context, ws, None, &request,
                )
                .await
                .unwrap()
                .id
            }
            SecretPurpose::OAuthToken => {
                let tokens = zirv_flow_store::StoredTokens {
                    access_token: zirv_flow_core::SecretString::new("oauth-access".into()),
                    refresh_token: zirv_flow_core::SecretString::new("oauth-refresh".into()),
                    expires_at: None,
                    binding: "binding".into(),
                };
                let mut tx = zirv_flow_store::begin(pool).await.unwrap();
                let source = zirv_flow_store::create_source(
                    &mut *tx,
                    ws,
                    &zirv_flow_store::NewSource {
                        id: Uuid::now_v7(),
                        kind: "http",
                        name: "Rotation source",
                        config: &json!({}),
                        credential_id: None,
                        schedule: None,
                        created_by: None,
                    },
                )
                .await
                .unwrap();
                let secret = create_secret_in(
                    &mut tx,
                    &context,
                    keys,
                    ws,
                    purpose,
                    &tokens.to_plaintext().unwrap(),
                )
                .await
                .unwrap();
                zirv_flow_store::link_tokens(&mut tx, ws, source.id, Some(secret))
                    .await
                    .unwrap();
                zirv_flow_store::commit(tx).await.unwrap();
                source.id
            }
            SecretPurpose::AiProviderKey => {
                save_ai_settings(pool, &context, Some(keys), ws, &ai_change())
                    .await
                    .unwrap();
                Uuid::nil()
            }
            SecretPurpose::WebhookSecret => {
                let request = serde_json::from_value(json!({
                    "name": "Rotation hook",
                    "dedupe_field": "id",
                }))
                .unwrap();
                let (hook, _) =
                    crate::webhooks::ops::create_webhook(state, &context, ws, None, &request)
                        .await
                        .unwrap();
                hook.id
            }
            SecretPurpose::WebhookSigning => {
                let mut tx = zirv_flow_store::begin(pool).await.unwrap();
                let id = create_secret_in(
                    &mut tx,
                    &context,
                    keys,
                    ws,
                    purpose,
                    &SecretBytes::new(b"signing-value".to_vec()),
                )
                .await
                .unwrap();
                let created = zirv_flow_store::create_signing_key(
                    &mut tx,
                    ws,
                    id,
                    time::OffsetDateTime::now_utc(),
                )
                .await
                .unwrap();
                assert!(created);
                zirv_flow_store::commit(tx).await.unwrap();
                Uuid::nil()
            }
            SecretPurpose::DkimKey => {
                let request =
                    serde_json::from_value(json!({"domain": "rotation-test.com"})).unwrap();
                let trace = TraceContext::root(Cause::api("test.rotation"));
                let (domain, _) = crate::sending::domains::create_sending_domain(
                    state, &context, ws, None, trace, &request,
                )
                .await
                .unwrap();
                domain.id
            }
            SecretPurpose::TestSendAddress => {
                let identifier =
                    zirv_flow_core::people::Identifier::parse("email", ALLOWED_ADDRESS).unwrap();
                let hash = zirv_flow_core::people::identifier_hash(
                    state.identifier_key.as_deref().unwrap(),
                    &identifier,
                );
                let id = Uuid::now_v7();
                let sealed = seal(
                    keys,
                    identifier.value().as_bytes(),
                    &secret_aad(ws, SecretId::from_uuid(id), purpose),
                );
                let mut tx = zirv_flow_store::begin(pool).await.unwrap();
                let entry = zirv_flow_store::add_allowlist_entry(
                    &mut tx,
                    &keys.key_ids(),
                    ws,
                    id,
                    &hash,
                    &zirv_flow_store::masked_hint(identifier.value()),
                    &sealed,
                )
                .await
                .unwrap();
                assert!(entry.is_some());
                zirv_flow_store::commit(tx).await.unwrap();
                id
            }
        }
    }

    /// Reads the secret of `purpose` back the way production does, with the state's keyring, and
    /// returns its plaintext. `parent` is what `seed_purpose` returned. Nothing here prints a
    /// value; a failure names the purpose.
    async fn read_back(
        purpose: SecretPurpose,
        state: &crate::AppState,
        ws: WorkspaceId,
        parent: Uuid,
    ) -> Vec<u8> {
        let name = purpose.as_str();
        let pool = &state.pool;
        let keys = state.keyring().unwrap();
        match purpose {
            SecretPurpose::ConnectorCredential
            | SecretPurpose::DestinationCredential
            | SecretPurpose::WorkspaceCredential => {
                // The one opener every credential read goes through (the legacy purposes
                // included): sources, destinations and the credential test call it.
                let opened = zirv_flow_store::open_credential(pool, keys, ws, parent)
                    .await
                    .unwrap_or_else(|_| panic!("{name} did not open"));
                opened.secret.expose().to_vec()
            }
            SecretPurpose::OAuthToken => {
                let tokens = zirv_flow_store::load_tokens(pool, keys, ws, parent)
                    .await
                    .unwrap_or_else(|_| panic!("{name} did not load"))
                    .unwrap_or_else(|| panic!("{name} has no tokens"));
                tokens
                    .to_plaintext()
                    .unwrap_or_else(|_| panic!("{name} did not encode"))
                    .expose()
                    .to_vec()
            }
            SecretPurpose::AiProviderKey => {
                let settings = zirv_flow_store::find_ai_settings(pool, ws)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{name} has no settings"));
                let secret = settings
                    .provider_key_secret_id
                    .unwrap_or_else(|| panic!("{name} is not linked"));
                let key = crate::assistant::dispatch::open_provider_key(state, ws, secret)
                    .await
                    .unwrap_or_else(|_| panic!("{name} did not open"));
                key.expose().as_bytes().to_vec()
            }
            SecretPurpose::WebhookSecret => {
                let hook = zirv_flow_store::find_for_receive(pool, parent)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{name} has no hook"));
                zirv_flow_store::secrets::open_json(
                    pool,
                    keys,
                    ws,
                    SecretId::from_uuid(hook.secret_id),
                    purpose,
                )
                .await
                .unwrap_or_else(|_| panic!("{name} did not open"))
                .expose()
                .to_vec()
            }
            SecretPurpose::WebhookSigning => {
                let mut connection = pool.acquire().await.unwrap();
                let live = zirv_flow_store::live_signing_keys(
                    &mut connection,
                    ws,
                    time::OffsetDateTime::now_utc(),
                )
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("{name} has no key"));
                zirv_flow_store::secrets::open_json(
                    &mut *connection,
                    keys,
                    ws,
                    live.current.secret_id,
                    purpose,
                )
                .await
                .unwrap_or_else(|_| panic!("{name} did not open"))
                .expose()
                .to_vec()
            }
            SecretPurpose::DkimKey => {
                let domain = zirv_flow_store::find_sending_domain(pool, ws, parent)
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{name} has no domain"));
                let key = zirv_flow_store::secrets::open_json(
                    pool,
                    keys,
                    ws,
                    SecretId::from_uuid(domain.dkim_secret_id),
                    purpose,
                )
                .await
                .unwrap_or_else(|_| panic!("{name} did not open"));
                let signer = zirv_flow_connectors::destinations::DkimSigner::from_pkcs8(
                    &domain.domain,
                    &domain.selector,
                    key.expose(),
                );
                assert!(signer.is_ok(), "{name} is not a usable key");
                key.expose().to_vec()
            }
            SecretPurpose::TestSendAddress => {
                let identifier =
                    zirv_flow_core::people::Identifier::parse("email", ALLOWED_ADDRESS).unwrap();
                let hash = zirv_flow_core::people::identifier_hash(
                    state.identifier_key.as_deref().unwrap(),
                    &identifier,
                );
                let mut connection = pool.acquire().await.unwrap();
                zirv_flow_store::open_allowlist_address(&mut connection, keys, ws, &hash)
                    .await
                    .unwrap_or_else(|_| panic!("{name} did not open"))
                    .unwrap_or_else(|| panic!("{name} has no entry"))
                    .expose()
                    .to_vec()
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotation_keeps_a_secret_of_every_purpose_and_every_hash_valid(pool: PgPool) {
        use zirv_flow_core::people::{Identifier, identifier_hash};
        use zirv_flow_core::tracking::{self, TokenKind};

        migrated(&pool).await;
        let context = Context::system();
        let ws = create_workspace(&pool, &context, "Acme", None)
            .await
            .unwrap();
        let old = MasterKey::generate();
        let new = MasterKey::generate();
        let before = keyring(&old, None);
        seed_root(&pool, &before).await;
        let root = HmacRoot::bootstrap_from(&old);
        let state = crate::AppState::new(pool.clone(), std::sync::Arc::new(String::new))
            .with_keyring(std::sync::Arc::new(keyring(&old, None)))
            .with_hmac_root(&root);
        let mut parents = Vec::new();
        for purpose in SecretPurpose::ALL {
            parents.push((purpose, seed_purpose(purpose, &state, &before, ws.id).await));
        }
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT id, purpose FROM encrypted_secrets ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        for purpose in SecretPurpose::ALL {
            let count = rows
                .iter()
                .filter(|(_, stored)| stored == purpose.as_str())
                .count();
            assert_eq!(
                count,
                1,
                "{} should have exactly one secret",
                purpose.as_str()
            );
        }
        // What each purpose reads as before the rotation, through the same reader.
        let mut expected = Vec::new();
        for (purpose, parent) in &parents {
            expected.push(read_back(*purpose, &state, ws.id, *parent).await);
        }
        let identifier = Identifier::parse("email", "hash@example.com").unwrap();
        let identifier_before =
            identifier_hash(&root.hmac_key(HmacPurpose::Identifier), &identifier);
        let redaction_before = zirv_flow_core::credentials::fingerprint(
            &root.hmac_key(HmacPurpose::Redaction),
            b"redacted",
        );
        let message = Uuid::now_v7();
        let token = tracking::sign(
            &root.hmac_key(HmacPurpose::Tracking),
            TokenKind::Click,
            message,
            3,
        );

        let report = rotate_master_key(&pool, &context, &keyring(&new, Some(&old)))
            .await
            .unwrap();

        assert_eq!(report.rows, rows.len() + 1);
        // The old key is gone: everything below reads with a keyring that holds only the new one.
        let only_new = keyring(&new, None);
        let after = crate::AppState::new(pool.clone(), std::sync::Arc::new(String::new))
            .with_keyring(std::sync::Arc::new(keyring(&new, None)))
            .with_hmac_root(&root);
        for ((purpose, parent), expected) in parents.iter().zip(&expected) {
            let name = purpose.as_str();
            let read = read_back(*purpose, &after, ws.id, *parent).await;
            assert!(read == *expected, "{name} changed in rotation");
        }
        for (id, _) in &rows {
            let id = SecretId::from_uuid(*id);
            let stored = secrets::get(&pool, ws.id, id).await.unwrap().unwrap();
            assert_eq!(
                stored.sealed.key_id,
                new.key_id(),
                "{}",
                stored.purpose.as_str()
            );
        }
        let sealed_root = secrets::hmac_root(&pool).await.unwrap().unwrap();
        let aad = deployment_key_aad(secrets::HMAC_ROOT);
        let rotated = HmacRoot::from_bytes(&open(&only_new, &sealed_root, &aad).unwrap()).unwrap();
        assert!(
            identifier_hash(&rotated.hmac_key(HmacPurpose::Identifier), &identifier)
                == identifier_before,
            "the identifier hash changed"
        );
        assert_eq!(
            zirv_flow_core::credentials::fingerprint(
                &rotated.hmac_key(HmacPurpose::Redaction),
                b"redacted"
            ),
            redaction_before,
            "the redaction hash changed"
        );
        let claims = tracking::verify(
            &rotated.hmac_key(HmacPurpose::Tracking),
            TokenKind::Click,
            &token,
        );
        assert_eq!(
            claims.map(|claims| (claims.message, claims.link)),
            Some((message, 3))
        );
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
