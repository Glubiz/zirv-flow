//! The consent dimensions of a workspace: the axes, such as a vertical or a brand, that a
//! consent record can be scoped to. The built-in `topic` dimension is listed first and cannot
//! be changed here; its values are the topics.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{validate_dimension_key, validate_dimension_value_key};
use zirv_flow_store::{
    Dimension, DimensionUpdate, DimensionValue, NewDimension, PgConnection, PgPool,
};

use super::{bool_param, invalid};
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 100;
/// The most values one request may create a dimension with.
const MAX_INITIAL_VALUES: usize = 200;

#[derive(Serialize, ToSchema)]
pub(crate) struct DimensionValueBody {
    /// Lowercase letters, digits, underscores and hyphens; unique in the dimension.
    #[schema(example = "retail")]
    key: String,
    name: String,
    archived: bool,
}

impl From<&DimensionValue> for DimensionValueBody {
    fn from(value: &DimensionValue) -> Self {
        Self {
            key: value.key.clone(),
            name: value.name.clone(),
            archived: value.archived_at.is_some(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DimensionBody {
    /// Lowercase letters, digits, underscores and hyphens; unique in the workspace.
    #[schema(example = "vertical")]
    key: String,
    name: String,
    /// True for the `topic` dimension, whose values are the workspace's topics.
    builtin: bool,
    archived: bool,
    /// By key. Archived values are left out of a listing unless `archived=true`.
    values: Vec<DimensionValueBody>,
}

impl From<&Dimension> for DimensionBody {
    fn from(dimension: &Dimension) -> Self {
        Self {
            key: dimension.key.clone(),
            name: dimension.name.clone(),
            builtin: dimension.builtin,
            archived: dimension.archived_at.is_some(),
            values: dimension.values.iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct NewValueRequest {
    /// 1 to 64 lowercase letters, digits, underscores or hyphens. It cannot be changed.
    #[schema(example = "retail")]
    key: String,
    /// 1 to 100 characters.
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateDimensionRequest {
    /// 1 to 64 lowercase letters, digits, underscores or hyphens, and not `topic`. It cannot
    /// be changed.
    #[schema(example = "vertical")]
    key: String,
    /// 1 to 100 characters.
    name: String,
    /// The first values, at most 200. More can be added later.
    #[serde(default)]
    values: Vec<NewValueRequest>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateDimensionRequest {
    name: Option<String>,
    archived: Option<bool>,
}

fn check_name(name: &str) -> Result<(), AdminError> {
    let length = name.chars().count();
    if name.trim().is_empty() || length > MAX_NAME_CHARS {
        Err(invalid("name", "must be 1 to 100 characters"))
    } else {
        Ok(())
    }
}

fn update_of(
    request: &UpdateDimensionRequest,
) -> Result<(DimensionUpdate, Vec<&'static str>), AdminError> {
    let mut fields = Vec::new();
    if let Some(name) = &request.name {
        check_name(name)?;
        fields.push("name");
    }
    if request.archived.is_some() {
        fields.push("archived");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    Ok((
        DimensionUpdate {
            name: request.name.clone(),
            archived: request.archived,
        },
        fields,
    ))
}

fn conflict(error: zirv_flow_store::StoreError, message: &'static str) -> AdminError {
    if error.is_conflict() {
        AdminError::Conflict(message)
    } else {
        error.into()
    }
}

/// Declares a dimension, with its first values, and audits it. Refused when the key exists,
/// archived or not.
pub(crate) async fn create_dimension(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: &CreateDimensionRequest,
) -> Result<Dimension, AdminError> {
    validate_dimension_key(&request.key)?;
    check_name(&request.name)?;
    if request.values.len() > MAX_INITIAL_VALUES {
        return Err(invalid("values", "must be at most 200 values"));
    }
    for value in &request.values {
        validate_dimension_value_key(&value.key)?;
        check_name(&value.name)?;
    }

    let mut tx = zirv_flow_store::begin(pool).await?;
    let mut dimension = zirv_flow_store::create_dimension(
        &mut *tx,
        workspace,
        &NewDimension {
            key: request.key.clone(),
            name: request.name.clone(),
        },
    )
    .await
    .map_err(|error| conflict(error, "a dimension with that key already exists"))?;
    for value in &request.values {
        let added = zirv_flow_store::create_dimension_value(
            &mut *tx,
            workspace,
            &request.key,
            &value.key,
            &value.name,
        )
        .await
        .map_err(|error| conflict(error, "a value with that key appears twice"))?
        .ok_or(AdminError::NotFound("dimension"))?;
        dimension.values.push(added);
    }
    let id = dimension.id.unwrap_or_else(Uuid::nil);
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_dimension.created",
        Some(("consent_dimension", id)),
        json!({ "key": crate::admin::audit_text(&dimension.key), "value_count": dimension.values.len() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(dimension)
}

/// The declared dimension `key` as stored, or not found. The built-in `topic` is not
/// declared, so it is not found here.
async fn declared(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Dimension, AdminError> {
    zirv_flow_store::find_dimension(connection, workspace, key)
        .await?
        .filter(|dimension| !dimension.builtin)
        .ok_or(AdminError::NotFound("dimension"))
}

/// Renames or archives a dimension and audits the names of the changed fields.
pub(crate) async fn update_dimension(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    key: &str,
    request: &UpdateDimensionRequest,
) -> Result<Dimension, AdminError> {
    let (update, fields) = update_of(request)?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    declared(&mut tx, workspace, key).await?;
    zirv_flow_store::update_dimension(&mut *tx, workspace, key, &update).await?;
    let dimension = declared(&mut tx, workspace, key).await?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_dimension.updated",
        Some(("consent_dimension", dimension.id.unwrap_or_else(Uuid::nil))),
        json!({ "key": key, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(dimension)
}

/// Adds a value to a dimension and audits it. Refused when the key exists, archived or not.
pub(crate) async fn add_value(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    dimension: &str,
    request: &NewValueRequest,
) -> Result<DimensionValue, AdminError> {
    validate_dimension_value_key(&request.key)?;
    check_name(&request.name)?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let parent = declared(&mut tx, workspace, dimension).await?;
    let value = zirv_flow_store::create_dimension_value(
        &mut *tx,
        workspace,
        dimension,
        &request.key,
        &request.name,
    )
    .await
    .map_err(|error| conflict(error, "a value with that key already exists"))?
    .ok_or(AdminError::NotFound("dimension"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_dimension.value_added",
        Some(("consent_dimension", parent.id.unwrap_or_else(Uuid::nil))),
        json!({ "dimension": dimension, "key": crate::admin::audit_text(&value.key) }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(value)
}

/// Renames or archives a value and audits the names of the changed fields.
pub(crate) async fn update_value(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    dimension: &str,
    key: &str,
    request: &UpdateDimensionRequest,
) -> Result<DimensionValue, AdminError> {
    let (update, fields) = update_of(request)?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let parent = declared(&mut tx, workspace, dimension).await?;
    let value =
        zirv_flow_store::update_dimension_value(&mut *tx, workspace, dimension, key, &update)
            .await?
            .ok_or(AdminError::NotFound("dimension value"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "consent_dimension.value_updated",
        Some(("consent_dimension", parent.id.unwrap_or_else(Uuid::nil))),
        json!({ "dimension": dimension, "key": key, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(value)
}

/// The workspace's consent dimensions: the built-in `topic` first, then the declared ones by
/// key. Archived dimensions and values are included with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_consent_dimensions",
    path = "/api/v1/workspaces/{ws}/consent/dimensions",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived dimensions and values; default false")
    ),
    responses(
        (status = 200, description = "The dimensions, `topic` first", body = Vec<DimensionBody>),
        (status = 400, description = "archived is not true or false", body = Problem, content_type = "application/problem+json"),
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
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<DimensionBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let mut connection = zirv_flow_store::begin(&state.pool).await?;
    let dimensions =
        zirv_flow_store::list_dimensions(&mut connection, workspace.id, archived).await?;
    Ok(Json(dimensions.iter().map(DimensionBody::from).collect()))
}

/// Declares a dimension, optionally with its first values.
#[utoipa::path(
    post,
    operation_id = "create_consent_dimension",
    path = "/api/v1/workspaces/{ws}/consent/dimensions",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateDimensionRequest,
    responses(
        (status = 201, description = "The new dimension", body = DimensionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The key already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid key or name, `topic` as the key, or a bad value", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateDimensionRequest>,
) -> Result<(StatusCode, Json<DimensionBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let dimension =
        create_dimension(&state.pool, &principal.context(), workspace.id, &body).await?;
    Ok((StatusCode::CREATED, Json(DimensionBody::from(&dimension))))
}

/// Renames a dimension or archives it. The key never changes, and the built-in `topic`
/// dimension is not found here.
#[utoipa::path(
    patch,
    operation_id = "update_consent_dimension",
    path = "/api/v1/workspaces/{ws}/consent/dimensions/{key}",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Dimension key")
    ),
    request_body = UpdateDimensionRequest,
    responses(
        (status = 200, description = "The updated dimension", body = DimensionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or dimension, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateDimensionRequest>,
) -> Result<Json<DimensionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let dimension =
        update_dimension(&state.pool, &principal.context(), workspace.id, &key, &body).await?;
    Ok(Json(DimensionBody::from(&dimension)))
}

/// Adds a value to a declared dimension.
#[utoipa::path(
    post,
    operation_id = "add_consent_dimension_value",
    path = "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Dimension key")
    ),
    request_body = NewValueRequest,
    responses(
        (status = 201, description = "The new value", body = DimensionValueBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or dimension, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The value key already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid key or name", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn add_value_handler(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key)): Path<(String, String)>,
    ApiJson(body): ApiJson<NewValueRequest>,
) -> Result<(StatusCode, Json<DimensionValueBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let value = add_value(&state.pool, &principal.context(), workspace.id, &key, &body).await?;
    Ok((StatusCode::CREATED, Json(DimensionValueBody::from(&value))))
}

/// Renames a value or archives it. The key never changes.
#[utoipa::path(
    patch,
    operation_id = "update_consent_dimension_value",
    path = "/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("key" = String, Path, description = "Dimension key"),
        ("value" = String, Path, description = "Value key")
    ),
    request_body = UpdateDimensionRequest,
    responses(
        (status = 200, description = "The updated value", body = DimensionValueBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, dimension or value, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update_value_handler(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, key, value)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<UpdateDimensionRequest>,
) -> Result<Json<DimensionValueBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let value = update_value(
        &state.pool,
        &principal.context(),
        workspace.id,
        &key,
        &value,
        &body,
    )
    .await?;
    Ok(Json(DimensionValueBody::from(&value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn a_name_is_one_to_a_hundred_characters_and_not_blank() {
        assert!(check_name("Vertical").is_ok());
        assert!(check_name(&"n".repeat(100)).is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("  ").is_err());
        assert!(check_name(&"n".repeat(101)).is_err());
    }

    #[test]
    fn an_update_needs_a_field_and_names_the_ones_it_changes() {
        let none = UpdateDimensionRequest {
            name: None,
            archived: None,
        };
        let both = UpdateDimensionRequest {
            name: Some("Brand".into()),
            archived: Some(true),
        };

        assert!(
            matches!(update_of(&none), Err(AdminError::Invalid(error)) if error.field() == "body")
        );
        let (update, fields) = update_of(&both).unwrap();
        assert_eq!(fields, ["name", "archived"]);
        assert_eq!(update.archived, Some(true));
        assert!(
            update_of(&UpdateDimensionRequest {
                name: Some(String::new()),
                archived: None
            })
            .is_err()
        );
    }

    #[test]
    fn the_body_marks_archived_values_and_the_built_in_dimension() {
        let dimension = Dimension {
            id: None,
            key: "topic".into(),
            name: "Topic".into(),
            builtin: true,
            archived_at: None,
            values: vec![DimensionValue {
                key: "news".into(),
                name: "News".into(),
                archived_at: Some(OffsetDateTime::UNIX_EPOCH),
            }],
        };

        let json = serde_json::to_value(DimensionBody::from(&dimension)).unwrap();

        assert_eq!(json["builtin"], true);
        assert_eq!(json["archived"], false);
        assert_eq!(json["values"][0]["archived"], true);
    }
}
