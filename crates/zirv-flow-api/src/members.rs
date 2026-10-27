//! Workspace membership.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{Role, UserId};
use zirv_flow_store::Member;

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct MemberBody {
    user_id: Uuid,
    email: String,
    /// `admin`, `publisher`, `editor` or `viewer`.
    #[schema(value_type = String, example = "editor")]
    role: Role,
    /// Whether a publisher, editor or viewer sees personal values unmasked. An admin always does.
    view_pii: bool,
    /// Whether this member is the instance owner, who has every right in every workspace.
    /// Present for workspace admins and the owner only; left out for everyone else.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<bool>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    added_at: OffsetDateTime,
}

impl From<&Member> for MemberBody {
    fn from(member: &Member) -> Self {
        Self {
            user_id: member.user_id.as_uuid(),
            email: member.email.clone(),
            role: member.role,
            view_pii: member.view_pii,
            owner: Some(member.is_owner),
            added_at: member.created_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetRoleRequest {
    /// `admin`, `publisher`, `editor` or `viewer`.
    role: String,
    /// Grants or revokes the personal-values grant; leave it out to keep the stored value.
    #[serde(default)]
    view_pii: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RoleBody {
    user_id: Uuid,
    #[schema(value_type = String, example = "editor")]
    role: Role,
    view_pii: bool,
}

/// The members of a workspace. Any member, whatever the role, can read this.
#[utoipa::path(
    get,
    operation_id = "list_members",
    path = "/api/v1/workspaces/{ws}/members",
    tag = "members",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The members", body = Vec<MemberBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The API key lacks the read or admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<Vec<MemberBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::ReadMembers).await?;
    let members = zirv_flow_store::list_members(&state.pool, workspace.id).await?;
    let may_see_owner = authorize(&state, &principal, workspace.id, Need::Admin)
        .await
        .is_ok();
    Ok(Json(
        members
            .iter()
            .map(|member| {
                let mut body = MemberBody::from(member);
                if !may_see_owner {
                    body.owner = None;
                }
                body
            })
            .collect(),
    ))
}

/// Adds a user to the workspace or changes their role. Workspace admins and the owner only.
#[utoipa::path(
    put,
    operation_id = "set_member_role",
    path = "/api/v1/workspaces/{ws}/members/{user_id}",
    tag = "members",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("user_id" = Uuid, Path, description = "User id")
    ),
    request_body = SetRoleRequest,
    responses(
        (status = 200, description = "The membership", body = RoleBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or user, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The change would leave the workspace with no admin (`last_admin`); only the owner may do that", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid role", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn set(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, user)): Path<(String, String)>,
    ApiJson(body): ApiJson<SetRoleRequest>,
) -> Result<Json<RoleBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user: UserId = path_id(&user)?;
    let role: Role = body.role.parse()?;
    let membership = admin::set_member_grant(
        &state.pool,
        &principal.context(),
        workspace.id,
        user,
        role,
        body.view_pii,
    )
    .await?;
    Ok(Json(RoleBody {
        user_id: membership.user_id.as_uuid(),
        role: membership.role,
        view_pii: membership.view_pii,
    }))
}

/// Removes a user from the workspace. Workspace admins and the owner only.
#[utoipa::path(
    delete,
    operation_id = "remove_member",
    path = "/api/v1/workspaces/{ws}/members/{user_id}",
    tag = "members",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("user_id" = Uuid, Path, description = "User id")
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or member, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The change would leave the workspace with no admin (`last_admin`); only the owner may do that", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, user)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user: UserId = path_id(&user)?;
    admin::remove_member(&state.pool, &principal.context(), workspace.id, user).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_is_shown_with_a_lower_case_role() {
        let member = Member {
            user_id: UserId::new(),
            email: "a@example.com".into(),
            role: Role::Admin,
            view_pii: true,
            is_owner: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_value(MemberBody::from(&member)).unwrap();

        assert_eq!(json["role"], "admin");
        assert_eq!(json["view_pii"], true);
        assert_eq!(json["owner"], true);
        assert_eq!(json["added_at"], "1970-01-01T00:00:00Z");
    }

    #[test]
    fn an_omitted_view_pii_is_none_so_the_stored_grant_is_kept() {
        let without: SetRoleRequest = serde_json::from_str(r#"{"role":"editor"}"#).unwrap();
        let with: SetRoleRequest =
            serde_json::from_str(r#"{"role":"editor","view_pii":false}"#).unwrap();

        assert_eq!(without.view_pii, None);
        assert_eq!(with.view_pii, Some(false));
    }

    #[test]
    fn the_role_request_is_validated_later_so_the_error_names_the_field() {
        let request: SetRoleRequest = serde_json::from_str(r#"{"role":"root"}"#).unwrap();

        let error = request.role.parse::<Role>().unwrap_err();

        assert_eq!(error.field(), "role");
    }
}
