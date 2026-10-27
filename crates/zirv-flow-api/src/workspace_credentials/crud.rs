//! The workspace credentials of a workspace: create, read, change, rotate and archive. A secret
//! goes in sealed, in the transaction that stores the credential row, and comes back only as a
//! hint per field (its last four characters when it has at least 16, else nothing) and the
//! redaction fingerprint. Each change is audited with ids, names and field names, never a value.
//! Everything here is for workspace admins.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hashbrown::HashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::credentials::{CredentialKind, CredentialSchema, fingerprint, secret_hint};
use zirv_flow_core::{SecretBytes, SecretPurpose, SecretString, UserId, WorkspaceId};
use zirv_flow_store::{
    ArchiveOutcome, CredentialUpdate, CredentialUse, CredentialUser, NewWorkspaceCredential,
    PgPool, StoreError, WorkspaceCredential,
};

use super::resolve::check_owners;
use super::schema::{SchemaBody, issue_problems, resolved_schema};
use super::{archived, in_use};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::credential::{keyring, to_plaintext};
use crate::sources::crud::check_name;
use crate::sources::{invalid, problems};

const MAX_KEY_BASE: usize = 40;

/// A live source or destination that uses a credential.
#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct UsedByBody {
    /// `source` or `destination`.
    #[schema(example = "destination")]
    kind: String,
    id: Uuid,
    name: String,
}

impl From<&CredentialUser> for UsedByBody {
    fn from(user: &CredentialUser) -> Self {
        Self {
            kind: user.kind.clone(),
            id: user.id,
            name: user.name.clone(),
        }
    }
}

impl From<&CredentialUse> for UsedByBody {
    fn from(used: &CredentialUse) -> Self {
        Self {
            kind: used.kind.clone(),
            id: used.id,
            name: used.name.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CredentialBody {
    id: Uuid,
    /// A short stable identifier, unique among the workspace's live credentials.
    #[schema(example = "mail-account")]
    key: String,
    #[schema(example = "Mail account")]
    name: String,
    /// A built-in kind (`GET .../credential-kinds`), `custom` (its own schema) or `imported` (a
    /// credential a source or destination sealed before credentials existed).
    #[schema(example = "smtp")]
    kind: String,
    schema_version: i32,
    /// The fields of the credential and which are secret. Draw the form from this.
    schema: SchemaBody,
    /// The non-secret values.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    fields: Value,
    /// Per secret field that is set: its last four characters when the value has at least 16
    /// characters, else an empty string (fully masked). The values are never returned.
    hints: BTreeMap<String, String>,
    /// The redaction HMAC of the sealed secret, equal for equal secrets; null for an imported
    /// credential until it is rotated.
    fingerprint: Option<String>,
    /// The other live credentials sealed with the same secret.
    same_secret_as: Vec<Uuid>,
    /// The live sources and destinations that use it.
    used_by: Vec<UsedByBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    rotated_at: OffsetDateTime,
    /// The user who last rotated it; null when an API key did, or it was never rotated by a user.
    rotated_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_tested_at: Option<OffsetDateTime>,
    /// `ok` or `failed`; null when never tested.
    last_test_outcome: Option<String>,
    /// An archived credential is read-only history.
    archived: bool,
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl CredentialBody {
    /// `all` is every live credential of the workspace, for the shared-secret warning.
    fn build(
        credential: &WorkspaceCredential,
        used_by: Vec<UsedByBody>,
        all: &[WorkspaceCredential],
    ) -> Result<Self, ApiError> {
        let hints = credential
            .hints
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, hint)| (key.clone(), hint.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        let same_secret_as = match &credential.fingerprint {
            Some(print) => all
                .iter()
                .filter(|other| {
                    other.id != credential.id && other.fingerprint.as_ref() == Some(print)
                })
                .map(|other| other.id)
                .collect(),
            None => Vec::new(),
        };
        Ok(Self {
            id: credential.id,
            key: credential.key.clone(),
            name: credential.name.clone(),
            kind: credential.kind.clone(),
            schema_version: credential.schema_version,
            schema: SchemaBody::from(&resolved_schema(credential)?),
            fields: credential.fields.clone(),
            hints,
            fingerprint: credential.fingerprint.clone(),
            same_secret_as,
            used_by,
            rotated_at: credential.rotated_at,
            rotated_by: credential.rotated_by,
            last_tested_at: credential.last_tested_at,
            last_test_outcome: credential.last_test_outcome.clone(),
            archived: credential.archived_at.is_some(),
            created_by: credential.created_by,
            created_at: credential.created_at,
            updated_at: credential.updated_at,
        })
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateCredentialRequest {
    /// A built-in kind from `GET .../credential-kinds`, or `custom` with its own `schema`.
    #[schema(example = "smtp")]
    kind: String,
    /// 1 to 128 characters after trimming; unique among the workspace's live credentials,
    /// ignoring case.
    #[schema(example = "Mail account")]
    name: String,
    /// A stable identifier of 1 to 64 lowercase letters, digits, `_` and `-`. Default: made from
    /// the name.
    key: Option<String>,
    /// The fields of a `custom` credential; refused for a built-in kind.
    schema: Option<SchemaBody>,
    /// The non-secret values by field key.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    fields: BTreeMap<String, Value>,
    /// The secret values by field key. Sealed on arrival and never returned.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, String>)]
    secret: BTreeMap<String, SecretString>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateCredentialRequest {
    name: Option<String>,
    /// Replaces the whole object of non-secret values.
    #[schema(value_type = Option<BTreeMap<String, serde_json::Value>>)]
    fields: Option<BTreeMap<String, Value>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RotateCredentialRequest {
    /// Every secret field of the credential, with its new value. Sealed on arrival and never
    /// returned.
    #[schema(value_type = BTreeMap<String, String>)]
    secret: BTreeMap<String, SecretString>,
}

fn conflict(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            StatusCode::CONFLICT,
            "a credential with that name or key already exists",
        )
    } else {
        error.into()
    }
}

/// Lowercase letters and digits of `name`, joined by single dashes, cut short.
fn key_base(name: &str) -> String {
    let mut base = String::new();
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            base.push(ch);
        } else if !base.ends_with('-') && !base.is_empty() {
            base.push('-');
        }
    }
    let base: String = base
        .trim_end_matches('-')
        .chars()
        .take(MAX_KEY_BASE)
        .collect();
    let base = base.trim_end_matches('-');
    if base.is_empty() {
        "credential".to_owned()
    } else {
        base.to_owned()
    }
}

fn key_is_valid(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && key.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The key of a new credential: the one given, or one made from the name that no live credential
/// of the workspace has.
fn pick_key(
    given: Option<&str>,
    name: &str,
    taken: &[WorkspaceCredential],
) -> Result<String, ApiError> {
    if let Some(key) = given {
        return if key_is_valid(key) {
            Ok(key.to_owned())
        } else {
            Err(invalid(
                "key",
                "must be 1 to 64 lowercase letters, digits, underscores and dashes",
            ))
        };
    }
    let base = key_base(name);
    let used = |key: &str| taken.iter().any(|other| other.key == key);
    if !used(&base) {
        return Ok(base);
    }
    (2..)
        .map(|number| format!("{base}-{number}"))
        .find(|key| !used(key))
        .ok_or_else(ApiError::internal)
}

/// The kind of a request and the schema it is checked against.
fn schema_of_request(
    request: &CreateCredentialRequest,
) -> Result<(CredentialSchema, Option<Value>), ApiError> {
    if request.kind == "custom" {
        let body = request
            .schema
            .as_ref()
            .ok_or_else(|| invalid("schema", "is required for the custom kind"))?;
        let schema = CredentialSchema::from(body);
        schema
            .check()
            .map_err(|issues| issue_problems("schema", issues))?;
        let stored = serde_json::to_value(&schema).map_err(|_| ApiError::internal())?;
        return Ok((schema, Some(stored)));
    }
    let kind = CredentialKind::parse(&request.kind)
        .ok_or_else(|| invalid("kind", "is not a credential kind"))?;
    if request.schema.is_some() {
        return Err(invalid("schema", "is only for the custom kind"));
    }
    Ok((kind.schema(), None))
}

/// The non-secret values as stored: an object without nulls.
fn public_object(fields: &BTreeMap<String, Value>) -> Value {
    Value::Object(
        fields
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<String, Value>>(),
    )
}

/// The hint of each secret field of `values`, as the stored object.
fn hints_of(values: &BTreeMap<String, SecretString>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(secret_hint(value.expose()))))
            .collect::<Map<String, Value>>(),
    )
}

fn fingerprint_of(state: &AppState, sealed: &SecretBytes) -> Option<String> {
    state
        .redaction_key
        .as_ref()
        .map(|key| fingerprint(key, sealed.expose()))
}

fn names_of(values: &BTreeMap<String, SecretString>) -> Vec<&str> {
    values.keys().map(String::as_str).collect()
}

/// Creates a credential, seals its secret fields as one object and audits `credential.created`
/// (and `secret.created` for the secret), all in one transaction.
pub(crate) async fn create_credential(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateCredentialRequest,
) -> Result<WorkspaceCredential, ApiError> {
    let name = check_name(&request.name)?;
    let (schema, stored_schema) = schema_of_request(request)?;
    let mut found = Vec::new();
    let checks = [
        ("fields", schema.validate_fields(&request.fields, true)),
        ("secret", schema.validate_secrets(&request.secret, true)),
    ];
    for (group, result) in checks {
        for issue in result.err().into_iter().flatten() {
            found.push((
                format!("{group}.{}", issue.field),
                issue.code.message().to_owned(),
            ));
        }
    }
    if !found.is_empty() {
        return Err(problems(found));
    }
    let keyring = keyring(state)?;
    let live = zirv_flow_store::list_credentials(&state.pool, workspace, false).await?;
    let key = pick_key(request.key.as_deref(), name, &live)?;
    let plaintext = to_plaintext(&request.secret)?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let secret_id = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::WorkspaceCredential,
        &plaintext,
    )
    .await?;
    let credential = zirv_flow_store::create_credential(
        &mut *tx,
        workspace,
        &NewWorkspaceCredential {
            id: Uuid::now_v7(),
            key: &key,
            name,
            kind: &request.kind,
            schema: stored_schema.as_ref(),
            fields: &public_object(&request.fields),
            secret_id,
            hints: &hints_of(&request.secret),
            fingerprint: fingerprint_of(state, &plaintext).as_deref(),
            created_by,
        },
    )
    .await
    .map_err(conflict)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "credential.created",
        Some(("credential", credential.id)),
        json!({"kind": credential.kind, "name": crate::admin::audit_text(&credential.name), "key": crate::admin::audit_text(&credential.key),
               "fields": request.fields.keys().collect::<Vec<_>>(),
               "secret_fields": names_of(&request.secret)}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(credential)
}

/// Changes the name or the non-secret values of a live credential and audits
/// `credential.updated` with the names of the changed parts.
pub(crate) async fn update_credential(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateCredentialRequest,
) -> Result<WorkspaceCredential, ApiError> {
    let name = request.name.as_deref().map(check_name).transpose()?;
    let mut changed: Vec<&'static str> = Vec::new();
    if name.is_some() {
        changed.push("name");
    }
    if request.fields.is_some() {
        changed.push("fields");
    }
    if changed.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let locked = zirv_flow_store::find_credential_for_update(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if locked.archived_at.is_some() {
        return Err(archived());
    }
    let fields = match &request.fields {
        Some(fields) => {
            resolved_schema(&locked)?
                .validate_fields(fields, true)
                .map_err(|issues| issue_problems("fields", issues))?;
            Some(public_object(fields))
        }
        None => None,
    };
    // A change that drops a field a live source or destination requires is refused.
    if let Some(fields) = &fields {
        let after = WorkspaceCredential {
            fields: fields.clone(),
            ..locked.clone()
        };
        check_owners(&mut tx, workspace, &after).await?;
    }
    let credential = zirv_flow_store::update_credential(
        &mut *tx,
        workspace,
        id,
        &CredentialUpdate {
            name,
            fields: fields.as_ref(),
        },
    )
    .await
    .map_err(conflict)?
    .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "credential.updated",
        Some(("credential", credential.id)),
        json!({"name": crate::admin::audit_text(&credential.name), "fields": changed}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(credential)
}

/// Seals new secret values, points the credential at them and deletes the old secret row, all in
/// one transaction, and audits `credential.rotated` with the field names only. Every source and
/// destination that uses the credential opens the new secret on its next call.
pub(crate) async fn rotate_credential(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    rotated_by: Option<UserId>,
    id: Uuid,
    request: &RotateCredentialRequest,
) -> Result<WorkspaceCredential, ApiError> {
    let keyring = keyring(state)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let locked = zirv_flow_store::find_credential_for_update(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if locked.archived_at.is_some() {
        return Err(archived());
    }
    resolved_schema(&locked)?
        .validate_secrets(&request.secret, true)
        .map_err(|issues| issue_problems("secret", issues))?;
    // A rotation that drops a secret field a live source or destination requires is refused.
    let after = WorkspaceCredential {
        hints: hints_of(&request.secret),
        ..locked.clone()
    };
    check_owners(&mut tx, workspace, &after).await?;
    let plaintext = to_plaintext(&request.secret)?;
    let new_secret = admin::create_secret_in(
        &mut tx,
        context,
        keyring,
        workspace,
        SecretPurpose::WorkspaceCredential,
        &plaintext,
    )
    .await?;
    let old = zirv_flow_store::swap_credential_secret(
        &mut tx,
        workspace,
        id,
        new_secret,
        &hints_of(&request.secret),
        fingerprint_of(state, &plaintext).as_deref(),
        rotated_by,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !zirv_flow_store::secrets::delete(&mut *tx, workspace, old).await? {
        return Err(ApiError::internal());
    }
    admin::audit_secret_deleted(&mut tx, context, workspace, old).await?;
    let credential = zirv_flow_store::find_credential(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "credential.rotated",
        Some(("credential", id)),
        json!({"kind": credential.kind, "name": crate::admin::audit_text(&credential.name),
               "secret_fields": names_of(&request.secret)}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(credential)
}

/// Archives a credential nothing live uses and audits `credential.archived`. Refused with
/// `credential_in_use` while a live source or destination references it.
pub(crate) async fn archive_credential(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    match zirv_flow_store::archive_credential(&mut tx, workspace, id).await? {
        ArchiveOutcome::Archived(credential) => {
            admin::audit(
                &mut tx,
                context,
                Some(workspace),
                "credential.archived",
                Some(("credential", credential.id)),
                json!({"kind": credential.kind, "name": crate::admin::audit_text(&credential.name)}),
            )
            .await?;
            zirv_flow_store::commit(tx).await?;
            Ok(())
        }
        ArchiveOutcome::InUse(_) => Err(in_use()),
        ArchiveOutcome::AlreadyArchived => Err(archived()),
        ArchiveOutcome::NotFound => Err(ApiError::not_found()),
    }
}

/// One credential as the API shows it, with where it is used.
async fn detail(
    state: &AppState,
    workspace: WorkspaceId,
    credential: &WorkspaceCredential,
) -> Result<CredentialBody, ApiError> {
    let users = zirv_flow_store::credential_users(&state.pool, workspace, credential.id).await?;
    let live = zirv_flow_store::list_credentials(&state.pool, workspace, false).await?;
    CredentialBody::build(
        credential,
        users.iter().map(UsedByBody::from).collect(),
        &live,
    )
}

/// The workspace's live credentials, ordered by name. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "list_credentials",
    path = "/api/v1/workspaces/{ws}/credentials",
    tag = "credentials",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The live credentials", body = Vec<CredentialBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<CredentialBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let credentials = zirv_flow_store::list_credentials(&state.pool, workspace.id, false).await?;
    let uses = zirv_flow_store::credential_uses(&state.pool, workspace.id).await?;
    let mut by_credential: HashMap<Uuid, Vec<UsedByBody>> = HashMap::new();
    for used in &uses {
        by_credential
            .entry(used.credential_id)
            .or_default()
            .push(UsedByBody::from(used));
    }
    let bodies = credentials
        .iter()
        .map(|credential| {
            CredentialBody::build(
                credential,
                by_credential.remove(&credential.id).unwrap_or_default(),
                &credentials,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(bodies))
}

/// Creates a credential and seals its secret fields. Workspace admins only. Audited as
/// `credential.created`.
#[utoipa::path(
    post,
    operation_id = "create_credential",
    path = "/api/v1/workspaces/{ws}/credentials",
    tag = "credentials",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateCredentialRequest,
    responses(
        (status = 201, description = "The new credential", body = CredentialBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live credential has that name or key, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, key, kind, schema, field or secret", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateCredentialRequest>,
) -> Result<(StatusCode, Json<CredentialBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let credential = create_credential(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(detail(&state, workspace.id, &credential).await?),
    ))
}

/// One credential, archived or not, with the sources and destinations that use it. Workspace
/// admins only.
#[utoipa::path(
    get,
    operation_id = "get_credential",
    path = "/api/v1/workspaces/{ws}/credentials/{id}",
    tag = "credentials",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Credential id")
    ),
    responses(
        (status = 200, description = "The credential", body = CredentialBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or credential, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<CredentialBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let credential = zirv_flow_store::find_credential(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(detail(&state, workspace.id, &credential).await?))
}

/// Changes the name or the non-secret values of a credential. Secret values change only through
/// the rotate endpoint. Workspace admins only. Audited as `credential.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_credential",
    path = "/api/v1/workspaces/{ws}/credentials/{id}",
    tag = "credentials",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Credential id")
    ),
    request_body = UpdateCredentialRequest,
    responses(
        (status = 200, description = "The updated credential", body = CredentialBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or credential, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live credential has that name, or the credential is archived (`code` is `credential_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name or field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateCredentialRequest>,
) -> Result<Json<CredentialBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let credential = update_credential(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(detail(&state, workspace.id, &credential).await?))
}

/// Rotates the secret: seals the new values, swaps them in and deletes the old secret in one
/// transaction. Every source and destination that uses the credential opens the new value on its
/// next call. Workspace admins only. Audited as `credential.rotated`, with field names only.
#[utoipa::path(
    put,
    operation_id = "rotate_credential",
    path = "/api/v1/workspaces/{ws}/credentials/{id}/secret",
    tag = "credentials",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Credential id")
    ),
    request_body = RotateCredentialRequest,
    responses(
        (status = 200, description = "The rotated credential", body = CredentialBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or credential, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The credential is archived (`code` is `credential_archived`), or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A missing, unknown or empty secret field", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the secret", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn rotate(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<RotateCredentialRequest>,
) -> Result<Json<CredentialBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let credential = rotate_credential(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(detail(&state, workspace.id, &credential).await?))
}

/// Archives a credential. Refused with `credential_in_use` while a live source or destination
/// uses it. Workspace admins only. Audited as `credential.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_credential",
    path = "/api/v1/workspaces/{ws}/credentials/{id}",
    tag = "credentials",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Credential id")
    ),
    responses(
        (status = 204, description = "Archived"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or credential, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live source or destination uses it (`code` is `credential_in_use`), or it is already archived (`credential_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    archive_credential(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(key: &str) -> WorkspaceCredential {
        WorkspaceCredential {
            id: Uuid::now_v7(),
            key: key.into(),
            name: key.into(),
            kind: "api_key".into(),
            schema_version: 1,
            schema: None,
            fields: json!({}),
            secret_id: Uuid::nil(),
            hints: json!({"api_key": "wxyz"}),
            fingerprint: Some("print".into()),
            rotated_at: OffsetDateTime::UNIX_EPOCH,
            rotated_by: None,
            last_tested_at: None,
            last_test_outcome: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_key_is_made_from_the_name_and_made_unique() {
        assert_eq!(key_base("Mail account!"), "mail-account");
        assert_eq!(key_base("  ---  "), "credential");
        assert_eq!(key_base(&"x".repeat(100)).len(), MAX_KEY_BASE);
        let taken = [stored("mail"), stored("mail-2")];

        assert_eq!(pick_key(None, "Mail", &taken).unwrap(), "mail-3");
        assert_eq!(pick_key(None, "Other", &taken).unwrap(), "other");
        assert_eq!(pick_key(Some("mine"), "Mail", &taken).unwrap(), "mine");
        for bad in ["", "Upper", "has space", "-lead", &"a".repeat(65)] {
            assert_eq!(
                pick_key(Some(bad), "Mail", &taken).unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_body_shows_hints_the_fingerprint_and_who_shares_the_secret_and_no_secret_reference() {
        let one = stored("one");
        let two = stored("two");
        let mut other = stored("three");
        other.fingerprint = Some("different".into());

        let body =
            CredentialBody::build(&one, Vec::new(), &[one.clone(), two.clone(), other]).unwrap();
        let wire = serde_json::to_value(&body).unwrap();

        assert_eq!(wire["hints"], json!({"api_key": "wxyz"}));
        assert_eq!(wire["fingerprint"], "print");
        assert_eq!(wire["same_secret_as"], json!([two.id]));
        assert_eq!(wire["archived"], false);
        assert!(wire.get("secret_id").is_none() && wire.get("secret").is_none());
        assert_eq!(wire["schema"]["fields"][0]["key"], "api_key");
    }

    #[test]
    fn hints_follow_the_sixteen_character_rule_and_fields_drop_nulls() {
        let mut values = BTreeMap::new();
        values.insert("short".to_owned(), SecretString::new("a".repeat(15)));
        values.insert(
            "long".to_owned(),
            SecretString::new(format!("{}wxyz", "a".repeat(12))),
        );

        assert_eq!(hints_of(&values), json!({"short": "", "long": "wxyz"}));
        let mut fields = BTreeMap::new();
        fields.insert("a".to_owned(), json!("x"));
        fields.insert("b".to_owned(), Value::Null);
        assert_eq!(public_object(&fields), json!({"a": "x"}));
    }
}
