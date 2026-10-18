//! Workspaces: listing, creating and reading.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::ValidationError;
use zirv_flow_store::Workspace;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct WorkspaceBody {
    id: Uuid,
    name: String,
    slug: String,
    /// The workspace's IANA time zone, the local clock of a wait for a person with no zone of
    /// their own. `UTC` until an admin sets it.
    #[schema(example = "Europe/Copenhagen")]
    timezone: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl From<&Workspace> for WorkspaceBody {
    fn from(workspace: &Workspace) -> Self {
        Self {
            id: workspace.id.as_uuid(),
            name: workspace.name.clone(),
            slug: workspace.slug.clone(),
            timezone: workspace.timezone.clone(),
            created_at: workspace.created_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateWorkspaceRequest {
    /// An IANA time zone name such as `Europe/Copenhagen` or `UTC`, at most 64 characters.
    #[schema(example = "Europe/Copenhagen")]
    timezone: String,
}

/// The longest time zone name stored.
const MAX_TIMEZONE_CHARS: usize = 64;

/// Whether `name` is a zone of the IANA database, as the schedules read it.
fn valid_timezone(name: &str) -> bool {
    name.chars().count() <= MAX_TIMEZONE_CHARS
        && zirv_flow_core::schedule::validate("* * * * *", name).is_ok()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateWorkspaceRequest {
    /// 1 to 100 characters.
    #[schema(example = "Acme Corp")]
    name: String,
    /// `a-z`, `0-9` and `-`, up to 63 characters. Derived from the name when omitted.
    #[schema(example = "acme-corp")]
    slug: Option<String>,
}

/// The workspaces the caller can see: all of them for the owner, otherwise their memberships.
#[utoipa::path(
    get,
    operation_id = "list_workspaces",
    path = "/api/v1/workspaces",
    tag = "workspaces",
    responses(
        (status = 200, description = "Visible workspaces", body = Vec<WorkspaceBody>),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "An API key cannot use this endpoint", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<Vec<WorkspaceBody>>, ApiError> {
    let session = principal.require_user()?;
    let workspaces = zirv_flow_store::list_workspaces_for_user(
        &state.pool,
        session.user.id,
        session.user.is_owner,
    )
    .await?;
    Ok(Json(workspaces.iter().map(WorkspaceBody::from).collect()))
}

/// Creates a workspace. Owner only. Needs the CSRF token.
#[utoipa::path(
    post,
    operation_id = "create_workspace",
    path = "/api/v1/workspaces",
    tag = "workspaces",
    request_body = CreateWorkspaceRequest,
    responses(
        (status = 201, description = "The new workspace", body = WorkspaceBody),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the owner, an API key, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The slug is taken", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid name or slug", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    ApiJson(body): ApiJson<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceBody>), ApiError> {
    principal.require_owner()?;
    let workspace = admin::create_workspace(
        &state.pool,
        &principal.context(),
        &body.name,
        body.slug.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(WorkspaceBody::from(&workspace))))
}

/// One workspace. A caller with no access gets 404, never 403.
#[utoipa::path(
    get,
    operation_id = "get_workspace",
    path = "/api/v1/workspaces/{ws}",
    tag = "workspaces",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The workspace", body = WorkspaceBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The API key lacks the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<WorkspaceBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    Ok(Json(WorkspaceBody::from(&workspace)))
}

/// Changes the workspace's settings: today its time zone. Needs the workspace admin role and the
/// CSRF token. Audited as `workspace.updated`.
#[utoipa::path(
    patch,
    operation_id = "update_workspace",
    path = "/api/v1/workspaces/{ws}",
    tag = "workspaces",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = UpdateWorkspaceRequest,
    responses(
        (status = 200, description = "The updated workspace", body = WorkspaceBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The time zone is not an IANA zone name", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    if !valid_timezone(&body.timezone) {
        return Err(ValidationError::new("timezone", "must be an IANA time zone name").into());
    }
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let saved = zirv_flow_store::set_workspace_timezone(&mut *tx, workspace.id, &body.timezone)
        .await?
        .ok_or_else(ApiError::not_found)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "workspace.updated",
        Some(("workspace", workspace.id.as_uuid())),
        json!({ "timezone": saved.timezone }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(WorkspaceBody::from(&saved)))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::WorkspaceId;

    use super::*;

    #[test]
    fn a_time_zone_is_an_iana_name_of_at_most_64_characters() {
        for valid in [
            "UTC",
            "Europe/Copenhagen",
            "America/Argentina/Buenos_Aires",
            "Etc/GMT+1",
        ] {
            assert!(valid_timezone(valid), "{valid}");
        }
        for invalid in [
            "",
            "Mars/Olympus",
            "europe/copenhagen ",
            "+02:00",
            "Europe/Copenhagen\n",
            &"x".repeat(65),
        ] {
            assert!(!valid_timezone(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn the_body_serialises_the_creation_time_as_rfc3339() {
        let workspace = Workspace {
            id: WorkspaceId::new(),
            name: "Acme".into(),
            slug: "acme".into(),
            timezone: "Europe/Copenhagen".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_value(WorkspaceBody::from(&workspace)).unwrap();

        assert_eq!(json["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["slug"], "acme");
        assert_eq!(json["timezone"], "Europe/Copenhagen");
    }

    #[test]
    fn the_slug_is_optional_in_the_request() {
        let request: CreateWorkspaceRequest = serde_json::from_str(r#"{"name":"Acme"}"#).unwrap();

        assert!(request.slug.is_none());
    }
}
