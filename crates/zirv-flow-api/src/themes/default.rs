//! The workspace default theme: the theme that block emails following the default render with.
//! Setting it needs the workspace admin role.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{PgPool, Theme, ThemeError};

use super::crud::ThemeBody;
use super::theme_error;
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetDefaultThemeRequest {
    /// A published, live theme of the workspace.
    theme_id: Uuid,
}

/// The problem for a theme that cannot be the default. A theme the workspace does not have is
/// refused the same way as an unpublished or archived one: the id came from the body, not the path.
fn default_refusal(error: ThemeError) -> ApiError {
    match error {
        ThemeError::NotFound => {
            ApiError::theme_not_publishable_default("the theme is not a theme of this workspace")
        }
        ThemeError::Archived => ApiError::theme_not_publishable_default("the theme is archived"),
        other => theme_error(other),
    }
}

/// Makes a theme the workspace default and audits it as `theme.default_set`.
pub(crate) async fn set_default(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: SetDefaultThemeRequest,
) -> Result<Theme, ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let theme = zirv_flow_store::set_default_theme(&mut tx, workspace, request.theme_id)
        .await
        .map_err(default_refusal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "theme.default_set",
        Some(("theme", theme.id)),
        json!({ "name": theme.name, "version": theme.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(theme)
}

/// Makes a published theme the workspace default, replacing the previous one. Emails that follow
/// the default use the theme's current version at their next render; emails that name a theme are
/// unchanged. The theme must be a live, published theme of the workspace, else 409
/// `theme_not_publishable_default`. Audited as `theme.default_set`.
#[utoipa::path(
    put,
    operation_id = "set_default_theme",
    path = "/api/v1/workspaces/{ws}/themes/default",
    tag = "themes",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = SetDefaultThemeRequest,
    responses(
        (status = 200, description = "The new default theme", body = ThemeBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The theme is not a published, live theme of the workspace (`theme_not_publishable_default`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_default(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<SetDefaultThemeRequest>,
) -> Result<Json<ThemeBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let theme = set_default(&state.pool, &principal.context(), workspace.id, body).await?;
    Ok(Json(ThemeBody::from(&theme)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_a_theme_cannot_be_the_default_is_one_code() {
        for error in [
            ThemeError::NotFound,
            ThemeError::Archived,
            ThemeError::NotPublished,
        ] {
            let problem = default_refusal(error);
            assert_eq!(problem.code(), Some("theme_not_publishable_default"));
            assert_eq!(problem.status_code(), axum::http::StatusCode::CONFLICT);
        }
    }

    #[test]
    fn the_request_names_a_theme_id_and_nothing_else_is_needed() {
        let id = Uuid::now_v7();
        let request: SetDefaultThemeRequest =
            serde_json::from_value(json!({ "theme_id": id })).unwrap();

        assert_eq!(request.theme_id, id);
        assert!(serde_json::from_str::<SetDefaultThemeRequest>("{}").is_err());
    }
}
