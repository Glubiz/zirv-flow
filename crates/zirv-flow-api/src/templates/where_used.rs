//! Where a template is used: the flows with a version whose graph names it.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::TemplateUse;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// One flow with a version that names the template.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateUseBody {
    flow_id: Uuid,
    flow_name: String,
    /// `draft`, `published`, `paused` or `archived`.
    flow_state: String,
    /// The numbers of the flow's versions that name the template, newest first. An older version
    /// still runs for the enrollments pinned to it.
    versions: Vec<i32>,
    /// Whether the version the flow runs now is one of them.
    active: bool,
}

impl From<&TemplateUse> for TemplateUseBody {
    fn from(used: &TemplateUse) -> Self {
        Self {
            flow_id: used.flow_id,
            flow_name: used.flow_name.clone(),
            flow_state: used.flow_state.clone(),
            versions: used.versions.clone(),
            active: used.active,
        }
    }
}

/// The flows with a version whose graph has a node naming this template, by flow name, one entry
/// per flow however many versions name it. The draft of a flow is not a version and is not listed.
#[utoipa::path(
    get,
    operation_id = "template_where_used",
    path = "/api/v1/workspaces/{ws}/templates/{id}/where-used",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    responses(
        (status = 200, description = "The flow versions that name the template", body = Vec<TemplateUseBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn where_used(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<TemplateUseBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_template(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let used = zirv_flow_store::template_where_used(&state.pool, workspace.id, id).await?;
    Ok(Json(used.iter().map(TemplateUseBody::from).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_use_keeps_the_versions_that_name_the_template_and_whether_the_active_one_does() {
        let used = TemplateUse {
            flow_id: Uuid::nil(),
            flow_name: "Welcome".into(),
            flow_state: "published".into(),
            versions: vec![3, 1],
            active: true,
        };

        let body = serde_json::to_value(TemplateUseBody::from(&used)).unwrap();

        assert_eq!(body["flow_name"], "Welcome");
        assert_eq!(body["versions"], serde_json::json!([3, 1]));
        assert_eq!(body["active"], true);
        assert!(body.get("version").is_none() && body.get("published_at").is_none());
    }
}
