//! The lists of a workspace: create, read, rename and delete. Each change is audited with
//! ids and the list's name.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{List, PgPool};

use super::invalid;
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Serialize, ToSchema)]
pub(crate) struct ListBody {
    id: Uuid,
    #[schema(example = "Newsletter")]
    name: String,
    description: String,
    /// How many people are in the list.
    member_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
}

impl From<&List> for ListBody {
    fn from(list: &List) -> Self {
        Self {
            id: list.id,
            name: list.name.clone(),
            description: list.description.clone(),
            member_count: list.member_count,
            created_at: list.created_at,
            updated_at: list.updated_at,
            created_by: list.created_by,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateListRequest {
    /// 1 to 128 characters after trimming; unique in the workspace, ignoring case.
    #[schema(example = "Newsletter")]
    name: String,
    /// At most 500 characters.
    #[serde(default)]
    description: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateListRequest {
    name: Option<String>,
    description: Option<String>,
}

fn check_name(name: &str) -> Result<&str, AdminError> {
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

fn check_description(description: &str) -> Result<(), AdminError> {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid("description", "must be at most 500 characters"))
    }
}

fn name_taken(error: zirv_flow_store::StoreError) -> AdminError {
    if error.is_conflict() {
        AdminError::Conflict("a list with that name already exists")
    } else {
        error.into()
    }
}

/// Creates a list and audits it as `list.created`.
pub(crate) async fn create_list(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: &CreateListRequest,
) -> Result<List, AdminError> {
    let name = check_name(&request.name)?;
    check_description(&request.description)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let list =
        zirv_flow_store::create_list(&mut *tx, workspace, name, &request.description, created_by)
            .await
            .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "list.created",
        Some(("list", list.id)),
        json!({ "name": list.name }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(list)
}

/// Renames a list or changes its description, and audits the changed fields as
/// `list.updated`.
pub(crate) async fn update_list(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateListRequest,
) -> Result<List, AdminError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request.name.as_deref().map(check_name).transpose()?;
    if name.is_some() {
        fields.push("name");
    }
    if let Some(description) = &request.description {
        check_description(description)?;
        fields.push("description");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let mut tx = zirv_flow_store::begin(pool).await?;
    let list = zirv_flow_store::update_list(
        &mut *tx,
        workspace,
        id,
        name,
        request.description.as_deref(),
    )
    .await
    .map_err(name_taken)?
    .ok_or(AdminError::NotFound("list"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "list.updated",
        Some(("list", list.id)),
        json!({ "name": list.name, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(list)
}

/// Deletes a list with its members and history, and audits it as `list.deleted`.
pub(crate) async fn delete_list(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), AdminError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let list = zirv_flow_store::delete_list(&mut tx, workspace, id)
        .await?
        .ok_or(AdminError::NotFound("list"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "list.deleted",
        Some(("list", list.id)),
        json!({ "name": list.name, "member_count": list.member_count }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The workspace's lists with their member counts, ordered by name.
#[utoipa::path(
    get,
    operation_id = "list_lists",
    path = "/api/v1/workspaces/{ws}/lists",
    tag = "lists",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The lists", body = Vec<ListBody>),
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
) -> Result<Json<Vec<ListBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let lists = zirv_flow_store::list_lists(&state.pool, workspace.id).await?;
    Ok(Json(lists.iter().map(ListBody::from).collect()))
}

/// Creates an empty list. Audited as `list.created`.
#[utoipa::path(
    post,
    operation_id = "create_list",
    path = "/api/v1/workspaces/{ws}/lists",
    tag = "lists",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateListRequest,
    responses(
        (status = 201, description = "The new list", body = ListBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A list with that name exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or description", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateListRequest>,
) -> Result<(StatusCode, Json<ListBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let list = create_list(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        &body,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(ListBody::from(&list))))
}

/// One list.
#[utoipa::path(
    get,
    operation_id = "get_list",
    path = "/api/v1/workspaces/{ws}/lists/{id}",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id")
    ),
    responses(
        (status = 200, description = "The list", body = ListBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ListBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let list = zirv_flow_store::find_list(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(ListBody::from(&list)))
}

/// Renames a list or changes its description. Audited as `list.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_list",
    path = "/api/v1/workspaces/{ws}/lists/{id}",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id")
    ),
    request_body = UpdateListRequest,
    responses(
        (status = 200, description = "The updated list", body = ListBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another list has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateListRequest>,
) -> Result<Json<ListBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let list = update_list(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(ListBody::from(&list)))
}

/// Deletes a list together with its members and their history. Audited as `list.deleted`.
#[utoipa::path(
    delete,
    operation_id = "delete_list",
    path = "/api/v1/workspaces/{ws}/lists/{id}",
    tag = "lists",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "List id")
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or list, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    delete_list(
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

    #[test]
    fn a_name_is_trimmed_and_bounded() {
        assert_eq!(check_name("  Newsletter ").unwrap(), "Newsletter");
        assert!(check_name("   ").is_err());
        assert!(check_name("").is_err());
        assert!(check_name(&"x".repeat(128)).is_ok());
        assert!(check_name(&"x".repeat(129)).is_err());
        assert!(check_name(&"é".repeat(128)).is_ok());
        assert!(check_name("two\nlines").is_err());
    }

    #[test]
    fn a_description_may_be_empty_but_not_long() {
        assert!(check_description("").is_ok());
        assert!(check_description(&"x".repeat(500)).is_ok());
        assert!(check_description(&"x".repeat(501)).is_err());
    }

    #[test]
    fn a_create_request_needs_a_name_and_defaults_the_description() {
        let request: CreateListRequest = serde_json::from_str(r#"{"name":"A"}"#).unwrap();

        assert_eq!(request.description, "");
        assert!(serde_json::from_str::<CreateListRequest>("{}").is_err());
    }

    #[test]
    fn a_list_body_shows_the_count_and_no_members() {
        let list = List {
            id: Uuid::now_v7(),
            name: "A".into(),
            description: String::new(),
            member_count: 3,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            created_by: None,
        };

        let json = serde_json::to_value(ListBody::from(&list)).unwrap();

        assert_eq!(json["member_count"], 3);
        assert!(json["created_by"].is_null());
        assert_eq!(json["created_at"], "1970-01-01T00:00:00Z");
    }
}
