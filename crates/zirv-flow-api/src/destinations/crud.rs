//! The destinations of a workspace: create, read, change and archive. A destination uses a
//! workspace credential by id and shows only the names of its fields. Each change is audited with
//! ids, names and field names, never a value. Everything here is for workspace admins.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    provider_default_rate, validate_config, validate_credential_fit_with, validate_credential_names,
};
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{Destination, DestinationUpdate, NewDestination, PgPool, StoreError};

use super::{archived, kind_unavailable, registry};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::crud::check_name;
use crate::sources::{field_problems, invalid};
use crate::workspace_credentials::resolve::{
    check_schema_credential, lock_for_use, manifest_schema, schema_field_names,
};

/// The rate a destination gets when the request names none, in deliveries per second.
pub(crate) const DEFAULT_RATE: i32 = 10;
const MAX_RATE: i32 = 100_000;

/// What a destination can do, from its type and, for a provider, its manifest.
#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationAbilitiesBody {
    /// Delivers email: an `action.send` step may name it.
    send: bool,
    /// Runs profile operations: an `action.sync` step may name it.
    sync: bool,
    /// Declares the erase capability.
    erase: bool,
}

impl DestinationAbilitiesBody {
    /// The abilities of a stored destination; none when its type is not in this build.
    fn of(kind: &str, config: &Value) -> Self {
        let abilities = registry()
            .get(kind)
            .map(|driver| driver.spec().abilities(config))
            .unwrap_or_default();
        Self {
            send: abilities.send,
            sync: abilities.sync,
            erase: abilities.erase,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationBody {
    id: Uuid,
    /// The destination type.
    #[schema(example = "smtp")]
    kind: String,
    #[schema(example = "Transactional mail")]
    name: String,
    /// The plain settings. Never holds a credential.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    config: Value,
    /// The workspace credential the destination uses.
    credential_id: Option<Uuid>,
    /// The names of the fields the credential holds. Their values are never returned.
    credential_fields_set: Vec<String>,
    /// Deliveries per second the destination tolerates; the rate bucket enforces it across
    /// workers and processes.
    rate: i32,
    /// The most deliveries it may take at once.
    burst: i32,
    /// An archived destination is read-only history.
    archived: bool,
    /// What the destination can do.
    capabilities: DestinationAbilitiesBody,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Destination> for DestinationBody {
    fn from(destination: &Destination) -> Self {
        Self {
            id: destination.id,
            kind: destination.kind.clone(),
            name: destination.name.clone(),
            config: destination.config.clone(),
            credential_id: destination.credential_id,
            credential_fields_set: destination.credential_fields.clone(),
            rate: destination.rate,
            burst: destination.burst,
            archived: destination.archived_at.is_some(),
            capabilities: DestinationAbilitiesBody::of(&destination.kind, &destination.config),
            created_by: destination.created_by,
            created_at: destination.created_at,
            updated_at: destination.updated_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateDestinationRequest {
    /// A destination type this build ships.
    #[schema(example = "smtp")]
    kind: String,
    /// 1 to 128 characters after trimming; unique among the workspace's live destinations,
    /// ignoring case.
    #[schema(example = "Transactional mail")]
    name: String,
    /// The plain settings, checked against the type's field specs.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    config: Value,
    /// The workspace credential the destination uses (`/credentials`). Required when the
    /// destination type has required credential fields, which the credential must hold.
    credential_id: Option<Uuid>,
    /// Deliveries per second, 1 to 100000. Default 10; for a `provider` destination, the rate
    /// of its manifest's `destination.rate_limit` when it declares one.
    rate: Option<i32>,
    /// The most deliveries at once, 1 to 100000. Default: the rate; for a `provider`
    /// destination, the manifest's burst when it declares a rate limit.
    burst: Option<i32>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateDestinationRequest {
    name: Option<String>,
    #[schema(value_type = Option<BTreeMap<String, serde_json::Value>>)]
    config: Option<Value>,
    rate: Option<i32>,
    burst: Option<i32>,
    /// Uses another workspace credential, which must hold every required credential field.
    credential_id: Option<Uuid>,
}

fn check_limit(field: &str, value: i32) -> Result<i32, ApiError> {
    if (1..=MAX_RATE).contains(&value) {
        Ok(value)
    } else {
        Err(invalid(field, "must be between 1 and 100000"))
    }
}

fn name_taken(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            StatusCode::CONFLICT,
            "a destination with that name already exists",
        )
    } else {
        error.into()
    }
}

/// Creates a destination with its rate bucket and audits `destination.created`, in one
/// transaction.
pub(crate) async fn create_destination(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateDestinationRequest,
) -> Result<Destination, ApiError> {
    let registry = registry();
    let driver = registry
        .get(&request.kind)
        .ok_or_else(|| invalid("kind", "is not a known destination type"))?;
    let spec = driver.spec();
    let name = check_name(&request.name)?;
    validate_config(spec, &request.config).map_err(|errors| field_problems("config", errors))?;
    if request.credential_id.is_none() {
        validate_credential_names(spec, &BTreeSet::new())
            .map_err(|errors| field_problems("credential", errors))?;
    }
    // A provider manifest may declare the rate its API tolerates: it feeds the bucket of a
    // destination created without one.
    let declared = (spec.kind == "provider")
        .then(|| provider_default_rate(&request.config))
        .flatten();
    let rate = check_limit(
        "rate",
        request
            .rate
            .or(declared.map(|(rate, _)| rate))
            .unwrap_or(DEFAULT_RATE),
    )?;
    let burst = check_limit(
        "burst",
        request
            .burst
            .or(request
                .rate
                .is_none()
                .then_some(declared)
                .flatten()
                .map(|(_, burst)| burst))
            .unwrap_or(rate),
    )?;

    // A provider manifest declares credential fields of its own: they count as the destination's.
    let schema = (spec.kind == "provider")
        .then(|| manifest_schema(&request.config))
        .flatten();
    let declared = schema_field_names(schema.as_ref());
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let linked = match request.credential_id {
        Some(id) => Some(
            lock_for_use(&mut tx, workspace, id, |present| {
                validate_credential_fit_with(spec, &declared, present)
            })
            .await?,
        ),
        None => None,
    };
    if spec.kind == "provider" {
        check_schema_credential(schema.as_ref(), linked.as_ref())?;
    }
    let destination = zirv_flow_store::create_destination(
        &mut *tx,
        workspace,
        &NewDestination {
            id: Uuid::now_v7(),
            kind: spec.kind,
            name,
            config: &request.config,
            credential_id: linked.as_ref().map(|credential| credential.id),
            rate,
            burst,
            created_by,
        },
    )
    .await
    .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "destination.created",
        Some(("destination", destination.id)),
        json!({"kind": destination.kind, "name": destination.name,
               "rate": rate, "burst": burst, "credential_id": destination.credential_id,
               "credential_fields": destination.credential_fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(destination)
}

/// Changes a live destination and audits `destination.updated` with the names of the changed
/// fields.
pub(crate) async fn update_destination(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateDestinationRequest,
) -> Result<Destination, ApiError> {
    let existing = zirv_flow_store::find_destination(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if existing.archived_at.is_some() {
        return Err(archived());
    }
    let registry = registry();
    let spec = registry
        .get(&existing.kind)
        .ok_or_else(kind_unavailable)?
        .spec();
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request.name.as_deref().map(check_name).transpose()?;
    if name.is_some() {
        fields.push("name");
    }
    if let Some(config) = &request.config {
        validate_config(spec, config).map_err(|errors| field_problems("config", errors))?;
        fields.push("config");
    }
    let rate = request
        .rate
        .map(|rate| check_limit("rate", rate))
        .transpose()?;
    if rate.is_some() {
        fields.push("rate");
    }
    let burst = request
        .burst
        .map(|burst| check_limit("burst", burst))
        .transpose()?;
    if burst.is_some() {
        fields.push("burst");
    }
    if request.credential_id.is_some() {
        fields.push("credential");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    // Lock the row first: two changes that both carry a credential must not each create or
    // replace a secret from the same stale read.
    let locked = zirv_flow_store::find_destination_for_update(&mut tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if locked.archived_at.is_some() {
        return Err(archived());
    }
    // The manifest this save leaves declares credential fields of its own.
    let schema = (spec.kind == "provider")
        .then(|| manifest_schema(request.config.as_ref().unwrap_or(&locked.config)))
        .flatten();
    let declared = schema_field_names(schema.as_ref());
    let linked = match request.credential_id {
        Some(id) => Some(
            lock_for_use(&mut tx, workspace, id, |present| {
                validate_credential_fit_with(spec, &declared, present)
            })
            .await?,
        ),
        None => None,
    };
    // A provider manifest names the credential fields it reads: check the state this save
    // leaves, a new manifest or a new credential, against it.
    if spec.kind == "provider" && (request.config.is_some() || linked.is_some()) {
        let effective = match (&linked, locked.credential_id) {
            (Some(credential), _) => Some(credential.clone()),
            (None, Some(current)) => {
                zirv_flow_store::lock_credential_for_use(&mut tx, workspace, current).await?
            }
            (None, None) => None,
        };
        check_schema_credential(schema.as_ref(), effective.as_ref())?;
    }
    let destination = zirv_flow_store::update_destination(
        &mut *tx,
        workspace,
        id,
        &DestinationUpdate {
            name,
            config: request.config.as_ref(),
            rate,
            burst,
            credential_id: linked.as_ref().map(|credential| credential.id),
        },
    )
    .await
    .map_err(name_taken)?
    .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "destination.updated",
        Some(("destination", destination.id)),
        json!({"name": destination.name, "fields": fields}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(destination)
}

/// Archives a live destination and audits `destination.archived`. Its history stays; its name
/// is free again.
pub(crate) async fn archive_destination(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let destination = match zirv_flow_store::archive_destination(&mut *tx, workspace, id).await? {
        Some(destination) => destination,
        None => {
            return Err(
                match zirv_flow_store::find_destination(&mut *tx, workspace, id).await? {
                    Some(_) => archived(),
                    None => ApiError::not_found(),
                },
            );
        }
    };
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "destination.archived",
        Some(("destination", destination.id)),
        json!({"kind": destination.kind, "name": destination.name}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The workspace's live destinations, ordered by name. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "list_destinations",
    path = "/api/v1/workspaces/{ws}/destinations",
    tag = "destinations",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The live destinations", body = Vec<DestinationBody>),
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
) -> Result<Json<Vec<DestinationBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let destinations = zirv_flow_store::list_destinations(&state.pool, workspace.id, false).await?;
    Ok(Json(
        destinations.iter().map(DestinationBody::from).collect(),
    ))
}

/// One destination a writer may pick: no settings, no credential.
#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationOptionBody {
    id: Uuid,
    name: String,
    /// An archived destination is listed so a step that names it still shows its name; it
    /// cannot be picked.
    archived: bool,
    capabilities: DestinationAbilitiesBody,
}

/// The workspace's destinations as picker options, archived ones included and marked, ordered by
/// name. Needs the write role: this is what a writer picks a flow step's destination from.
#[utoipa::path(
    get,
    operation_id = "list_destination_options",
    path = "/api/v1/workspaces/{ws}/destinations/options",
    tag = "destinations",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The options: id, name, state and capabilities, nothing else", body = Vec<DestinationOptionBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn options(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<DestinationOptionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let destinations = zirv_flow_store::list_destinations(&state.pool, workspace.id, true).await?;
    Ok(Json(
        destinations
            .iter()
            .map(|destination| DestinationOptionBody {
                id: destination.id,
                name: destination.name.clone(),
                archived: destination.archived_at.is_some(),
                capabilities: DestinationAbilitiesBody::of(&destination.kind, &destination.config),
            })
            .collect(),
    ))
}

/// Creates a destination and seals its credential. Workspace admins only. Audited as
/// `destination.created`.
#[utoipa::path(
    post,
    operation_id = "create_destination",
    path = "/api/v1/workspaces/{ws}/destinations",
    tag = "destinations",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateDestinationRequest,
    responses(
        (status = 201, description = "The new destination", body = DestinationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live destination with that name exists, or the master key is outdated", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, kind, setting, credential field, rate or burst", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the credential", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateDestinationRequest>,
) -> Result<(StatusCode, Json<DestinationBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let destination = create_destination(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(DestinationBody::from(&destination)),
    ))
}

/// One destination, archived or not. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "get_destination",
    path = "/api/v1/workspaces/{ws}/destinations/{id}",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id")
    ),
    responses(
        (status = 200, description = "The destination", body = DestinationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or destination, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<DestinationBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let destination = zirv_flow_store::find_destination(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(DestinationBody::from(&destination)))
}

/// Changes a destination: name, settings, rate, burst, or the credential. Workspace admins only.
/// Audited as `destination.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_destination",
    path = "/api/v1/workspaces/{ws}/destinations/{id}",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id")
    ),
    request_body = UpdateDestinationRequest,
    responses(
        (status = 200, description = "The updated destination", body = DestinationBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or destination, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live destination has that name, the destination is archived (`code` is `destination_archived`), or its type is not in this build", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to seal the credential", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateDestinationRequest>,
) -> Result<Json<DestinationBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let destination = update_destination(
        &state,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(DestinationBody::from(&destination)))
}

/// Archives a destination: it stops being listed, changed or checked, and keeps its delivery
/// history. Workspace admins only. Audited as `destination.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_destination",
    path = "/api/v1/workspaces/{ws}/destinations/{id}",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id")
    ),
    responses(
        (status = 204, description = "Archived"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or destination, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The destination is already archived (`code` is `destination_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    archive_destination(
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

    fn destination() -> Destination {
        Destination {
            id: Uuid::nil(),
            kind: "fake".into(),
            name: "Mail".into(),
            config: json!({"mode": "ok"}),
            credential_id: Some(Uuid::nil()),
            credential_secret_id: Some(Uuid::nil()),
            credential_fields: vec!["token".into()],
            rate: 10,
            burst: 20,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_destination_body_names_the_credential_fields_and_has_no_secret_reference() {
        let json = serde_json::to_value(DestinationBody::from(&destination())).unwrap();

        assert_eq!(json["credential_fields_set"], json!(["token"]));
        assert_eq!(json["config"], json!({"mode": "ok"}));
        assert_eq!(json["archived"], false);
        assert!(json.get("credential_secret_id").is_none());
        assert!(json.get("credential").is_none());
    }

    #[test]
    fn a_destination_body_has_the_capabilities_of_its_type() {
        let capabilities = |kind: &str| {
            let mut found = destination();
            found.kind = kind.into();
            serde_json::to_value(DestinationBody::from(&found)).unwrap()["capabilities"].clone()
        };

        assert_eq!(
            capabilities("smtp"),
            json!({"send": true, "sync": false, "erase": false})
        );
        assert_eq!(
            capabilities("webhook"),
            json!({"send": false, "sync": false, "erase": false})
        );
        assert_eq!(
            capabilities("fake"),
            json!({"send": true, "sync": false, "erase": true})
        );
        // A provider with no usable manifest declares nothing; a type this build lacks, nothing.
        assert_eq!(
            capabilities("provider"),
            json!({"send": false, "sync": false, "erase": false})
        );
        assert_eq!(
            capabilities("gone"),
            json!({"send": false, "sync": false, "erase": false})
        );
    }

    #[test]
    fn rate_and_burst_are_bounded() {
        assert_eq!(check_limit("rate", 1).unwrap(), 1);
        assert_eq!(check_limit("rate", 100_000).unwrap(), 100_000);
        assert_eq!(
            check_limit("rate", 0).unwrap_err().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(check_limit("burst", 100_001).is_err());
    }
}
