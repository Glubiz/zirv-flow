//! The sources of a workspace: create, read, change and delete. A credential goes in sealed, in
//! the transaction that stores the source, and comes back only as the names of its fields.
//! Each change is audited with ids, names and field names, never a value.

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::sources::{validate_config, validate_credential};
use zirv_flow_core::{Freshness, SecretId, SecretPurpose, SecretString, UserId, WorkspaceId};
use zirv_flow_store::{NewSource, PgPool, Source, SourceUpdate, StoreError};

use super::{
    credential, field_problems, invalid, is_push, kind_unavailable, managed_by_webhooks, registry,
};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 128;
/// The schedules of sources are cron expressions in UTC.
pub(super) const SCHEDULE_TIMEZONE: &str = "UTC";

#[derive(Serialize, ToSchema)]
pub(crate) struct SourceBody {
    id: Uuid,
    /// The source type, one of `GET /api/v1/source-kinds`.
    #[schema(example = "fake")]
    kind: String,
    #[schema(example = "CRM")]
    name: String,
    /// The plain settings. Never holds a credential.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    config: Value,
    /// The names of the credential fields that are set. Their values are never returned.
    credential_fields_set: Vec<String>,
    /// A cron expression in UTC, or null.
    schedule: Option<String>,
    enabled: bool,
    /// The worst freshness among the source's enabled streams (`failing`, then `stale`, then
    /// `fresh`); null when it has none, or the source is disabled.
    freshness: Option<String>,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Source> for SourceBody {
    fn from(source: &Source) -> Self {
        Self {
            id: source.id,
            kind: source.kind.clone(),
            name: source.name.clone(),
            config: source.config.clone(),
            credential_fields_set: source.credential_fields.clone(),
            schedule: source.schedule.clone(),
            enabled: source.enabled,
            freshness: None,
            created_by: source.created_by,
            created_at: source.created_at,
            updated_at: source.updated_at,
        }
    }
}

impl SourceBody {
    fn with_freshness(source: &Source, worst: &HashMap<Uuid, Freshness>) -> Self {
        let mut body = Self::from(source);
        body.freshness = worst.get(&source.id).map(|state| state.as_str().to_owned());
        body
    }
}

/// The worst state of each source that has an enabled stream, for the sources of a workspace.
async fn source_freshness(
    pool: &PgPool,
    workspace: WorkspaceId,
    source: Option<Uuid>,
) -> Result<HashMap<Uuid, Freshness>, ApiError> {
    let now = zirv_flow_store::database_now(pool).await?;
    let mut worst: HashMap<Uuid, Freshness> = HashMap::new();
    for stream in zirv_flow_store::stream_freshness(pool, Some(workspace), source, now).await? {
        let entry = worst.entry(stream.source_id).or_insert(stream.state);
        *entry = (*entry).max(stream.state);
    }
    Ok(worst)
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateSourceRequest {
    /// A source type from `GET /api/v1/source-kinds`.
    #[schema(example = "fake")]
    kind: String,
    /// 1 to 128 characters after trimming; unique in the workspace.
    #[schema(example = "CRM")]
    name: String,
    /// The plain settings, checked against the type's field specs.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    config: Value,
    /// The credential fields by name. Sealed on arrival and never returned.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, String>)]
    credential: BTreeMap<String, SecretString>,
    /// A cron expression in UTC.
    schedule: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateSourceRequest {
    name: Option<String>,
    #[schema(value_type = Option<BTreeMap<String, serde_json::Value>>)]
    config: Option<Value>,
    /// A cron expression in UTC; an empty string clears the schedule.
    schedule: Option<String>,
    enabled: Option<bool>,
    /// Replaces the whole credential: every required credential field must be given.
    #[schema(value_type = Option<BTreeMap<String, String>>)]
    credential: Option<BTreeMap<String, SecretString>>,
}

pub(crate) fn check_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        Err(invalid(
            "name",
            "must be 1 to 128 characters without control characters",
        ))
    } else {
        Ok(name)
    }
}

fn check_schedule(schedule: &str) -> Result<(), ApiError> {
    zirv_flow_core::schedule::validate(schedule, SCHEDULE_TIMEZONE)
        .map_err(|_| invalid("schedule", "must be a valid cron expression"))
}

pub(crate) fn name_taken(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            StatusCode::CONFLICT,
            "a source with that name already exists",
        )
    } else {
        error.into()
    }
}

fn field_names(values: &BTreeMap<String, SecretString>) -> Vec<String> {
    values.keys().cloned().collect()
}

/// Creates a source, seals its credential and audits `source.created` (and `secret.created`
/// for the credential), all in one transaction.
pub(crate) async fn create_source(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateSourceRequest,
) -> Result<Source, ApiError> {
    let registry = registry();
    let driver = registry
        .get(&request.kind)
        .ok_or_else(|| invalid("kind", "is not a known source type"))?;
    let spec = driver.spec();
    if spec.capabilities.push {
        return Err(managed_by_webhooks());
    }
    let name = check_name(&request.name)?;
    validate_config(spec, &request.config).map_err(|errors| field_problems("config", errors))?;
    validate_credential(spec, &request.credential, true)
        .map_err(|errors| field_problems("credential", errors))?;
    if let Some(schedule) = &request.schedule {
        check_schedule(schedule)?;
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let secret = if request.credential.is_empty() {
        None
    } else {
        let plaintext = credential::to_plaintext(&request.credential)?;
        Some(
            admin::create_secret_in(
                &mut tx,
                context,
                credential::keyring(state)?,
                workspace,
                SecretPurpose::ConnectorCredential,
                &plaintext,
            )
            .await?,
        )
    };
    let fields = field_names(&request.credential);
    let source = zirv_flow_store::create_source(
        &mut *tx,
        workspace,
        &NewSource {
            id: Uuid::now_v7(),
            kind: spec.kind,
            name,
            config: &request.config,
            credential_secret_id: secret,
            credential_fields: &fields,
            schedule: request.schedule.as_deref().filter(|text| !text.is_empty()),
            created_by,
        },
    )
    .await
    .map_err(name_taken)?;
    zirv_flow_store::sync_schedule(
        &mut tx,
        workspace,
        source.id,
        source.schedule.as_deref(),
        source.enabled,
    )
    .await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "source.created",
        Some(("source", source.id)),
        json!({"kind": source.kind, "name": source.name, "credential_fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(source)
}

/// Changes a source and audits `source.updated` with the names of the changed fields.
pub(crate) async fn update_source(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateSourceRequest,
) -> Result<Source, ApiError> {
    let existing = zirv_flow_store::find_source(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let registry = registry();
    let spec = registry
        .get(&existing.kind)
        .ok_or_else(kind_unavailable)?
        .spec();
    if spec.capabilities.push {
        return Err(managed_by_webhooks());
    }
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request.name.as_deref().map(check_name).transpose()?;
    if name.is_some() {
        fields.push("name");
    }
    if let Some(config) = &request.config {
        validate_config(spec, config).map_err(|errors| field_problems("config", errors))?;
        fields.push("config");
    }
    if let Some(schedule) = &request.schedule {
        if !schedule.is_empty() {
            check_schedule(schedule)?;
        }
        fields.push("schedule");
    }
    if request.enabled.is_some() {
        fields.push("enabled");
    }
    if let Some(values) = &request.credential {
        if values.is_empty() {
            return Err(invalid("credential", "must not be empty"));
        }
        validate_credential(spec, values, true)
            .map_err(|errors| field_problems("credential", errors))?;
        fields.push("credential");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    // Lock the row first: two changes that both carry a credential must not each create or
    // replace a secret from the same stale read.
    let locked = zirv_flow_store::find_source_for_update(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mut linked = None;
    let credential_names = request.credential.as_ref().map(field_names);
    if let Some(values) = &request.credential {
        let plaintext = credential::to_plaintext(values)?;
        let keyring = credential::keyring(state)?;
        match locked.credential_secret_id {
            Some(secret) => {
                admin::replace_secret_in(
                    &mut tx,
                    context,
                    keyring,
                    workspace,
                    SecretId::from_uuid(secret),
                    &plaintext,
                )
                .await?;
            }
            None => {
                linked = Some(
                    admin::create_secret_in(
                        &mut tx,
                        context,
                        keyring,
                        workspace,
                        SecretPurpose::ConnectorCredential,
                        &plaintext,
                    )
                    .await?,
                );
            }
        }
    }
    let source = zirv_flow_store::update_source(
        &mut tx,
        workspace,
        id,
        &SourceUpdate {
            name,
            config: request.config.as_ref(),
            schedule: request
                .schedule
                .as_deref()
                .map(|text| Some(text).filter(|text| !text.is_empty())),
            enabled: request.enabled,
            credential_secret_id: linked,
            credential_fields: credential_names.as_deref(),
        },
    )
    .await
    .map_err(name_taken)?
    .ok_or_else(ApiError::not_found)?;
    zirv_flow_store::sync_schedule(
        &mut tx,
        workspace,
        source.id,
        source.schedule.as_deref(),
        source.enabled,
    )
    .await?;
    zirv_flow_store::sync_source_reconcile_schedules(&mut tx, workspace, source.id, source.enabled)
        .await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "source.updated",
        Some(("source", source.id)),
        json!({"name": source.name, "fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(source)
}

/// Deletes a source with its streams and its credential secret, and audits `source.deleted`
/// (and `secret.deleted` for the credential), all in one transaction.
pub(crate) async fn delete_source(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    if let Some(existing) = zirv_flow_store::find_source(pool, workspace, id).await?
        && is_push(&existing.kind)
    {
        return Err(managed_by_webhooks());
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    let source = zirv_flow_store::delete_source(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "source.deleted",
        Some(("source", source.id)),
        json!({"kind": source.kind, "name": source.name}),
    )
    .await?;
    if let Some(secret) = source.credential_secret_id {
        admin::audit_secret_deleted(&mut tx, context, workspace, SecretId::from_uuid(secret))
            .await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The sources of a workspace, ordered by name.
#[utoipa::path(
    get,
    operation_id = "list_sources",
    path = "/api/v1/workspaces/{ws}/sources",
    tag = "sources",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The sources", body = Vec<SourceBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<SourceBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let sources = zirv_flow_store::list_sources(&state.pool, workspace.id).await?;
    let worst = source_freshness(&state.pool, workspace.id, None).await?;
    Ok(Json(
        sources
            .iter()
            .map(|source| SourceBody::with_freshness(source, &worst))
            .collect(),
    ))
}

/// Creates a source and seals its credential. Workspace admins only. Audited as
/// `source.created`.
#[utoipa::path(
    post,
    operation_id = "create_source",
    path = "/api/v1/workspaces/{ws}/sources",
    tag = "sources",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateSourceRequest,
    responses(
        (status = 201, description = "The new source", body = SourceBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A source with that name exists, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, kind, setting, credential field or schedule", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the credential", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateSourceRequest>,
) -> Result<(StatusCode, Json<SourceBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let source = create_source(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(SourceBody::from(&source))))
}

/// One source.
#[utoipa::path(
    get,
    operation_id = "get_source",
    path = "/api/v1/workspaces/{ws}/sources/{id}",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 200, description = "The source", body = SourceBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SourceBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let source = zirv_flow_store::find_source(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let worst = source_freshness(&state.pool, workspace.id, Some(source.id)).await?;
    Ok(Json(SourceBody::with_freshness(&source, &worst)))
}

/// Changes a source: name, settings, schedule, enabled, or the credential. Workspace admins
/// only. Audited as `source.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_source",
    path = "/api/v1/workspaces/{ws}/sources/{id}",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    request_body = UpdateSourceRequest,
    responses(
        (status = 200, description = "The updated source", body = SourceBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another source has that name, the type is not in this build, or `config` changes while a stream of the source has a queued or running run (`code` is `sync_run_active`); wait for the run or cancel it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the credential", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateSourceRequest>,
) -> Result<Json<SourceBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let source = update_source(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    let worst = source_freshness(&state.pool, workspace.id, Some(source.id)).await?;
    Ok(Json(SourceBody::with_freshness(&source, &worst)))
}

/// Deletes a source with its streams and its stored credential. Workspace admins only. Audited
/// as `source.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_source",
    path = "/api/v1/workspaces/{ws}/sources/{id}",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    delete_source(
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

    fn source() -> Source {
        Source {
            id: Uuid::nil(),
            kind: "fake".into(),
            name: "CRM".into(),
            config: json!({"streams": 2}),
            credential_secret_id: Some(Uuid::nil()),
            credential_fields: vec!["token".into()],
            schedule: None,
            enabled: true,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_source_body_names_the_credential_fields_and_has_no_secret_reference() {
        let json = serde_json::to_value(SourceBody::from(&source())).unwrap();

        assert_eq!(json["credential_fields_set"], json!(["token"]));
        assert_eq!(json["config"], json!({"streams": 2}));
        assert!(json.get("credential_secret_id").is_none());
        assert!(json.get("credential").is_none());
    }

    #[test]
    fn a_name_is_trimmed_and_must_be_short_and_printable() {
        assert_eq!(check_name("  CRM ").unwrap(), "CRM");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(129)).is_err());
        assert!(check_name("a\nb").is_err());
    }

    #[test]
    fn a_schedule_must_be_a_cron_expression() {
        assert!(check_schedule("0 * * * *").is_ok());
        assert_eq!(
            check_schedule("not cron").unwrap_err().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn field_names_are_the_keys_only() {
        let mut values = BTreeMap::new();
        values.insert(
            "token".to_owned(),
            SecretString::new("secret-value".to_owned()),
        );

        assert_eq!(field_names(&values), ["token"]);
    }
}
