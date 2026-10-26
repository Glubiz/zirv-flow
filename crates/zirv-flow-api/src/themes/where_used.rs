//! Where a theme is used: the templates that name it, the templates that follow the default, and
//! the active flows that send those templates.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{TemplateFlowUse, ThemeTemplate, ThemeUse};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// A template whose draft or current version uses the theme.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeTemplateBody {
    id: Uuid,
    name: String,
    /// Whether the template's draft uses the theme.
    in_draft: bool,
    /// Whether the template's current version uses the theme.
    in_current: bool,
}

/// An active flow that sends a template that uses the theme.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeFlowBody {
    flow_id: Uuid,
    flow_name: String,
    /// `published` or `paused`.
    flow_state: String,
    /// The templates of the flow that use the theme.
    template_ids: Vec<Uuid>,
}

/// Everything that uses a theme.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeUsedByBody {
    /// Templates, not archived, whose draft or current version names the theme.
    named_templates: Vec<ThemeTemplateBody>,
    /// Block templates, not archived, whose draft or current version follows the workspace
    /// default (`theme` absent or `"default"`). Empty unless this theme is the default.
    default_templates: Vec<ThemeTemplateBody>,
    /// Flows whose running version names a template of either list. A flow that does not run it
    /// now, and an archived flow, is not listed.
    active_flows: Vec<ThemeFlowBody>,
}

impl From<&ThemeTemplate> for ThemeTemplateBody {
    fn from(template: &ThemeTemplate) -> Self {
        Self {
            id: template.id,
            name: template.name.clone(),
            in_draft: template.in_draft,
            in_current: template.in_current,
        }
    }
}

impl From<&TemplateFlowUse> for ThemeFlowBody {
    fn from(flow: &TemplateFlowUse) -> Self {
        Self {
            flow_id: flow.flow_id,
            flow_name: flow.flow_name.clone(),
            flow_state: flow.flow_state.clone(),
            template_ids: flow.template_ids.clone(),
        }
    }
}

impl From<&ThemeUse> for ThemeUsedByBody {
    fn from(used: &ThemeUse) -> Self {
        Self {
            named_templates: used.named_templates.iter().map(Into::into).collect(),
            default_templates: used.default_templates.iter().map(Into::into).collect(),
            active_flows: used.active_flows.iter().map(Into::into).collect(),
        }
    }
}

/// The templates that name a theme, the templates that follow it as the default, and the active
/// flows that send them.
#[utoipa::path(
    get,
    operation_id = "theme_where_used",
    path = "/api/v1/workspaces/{ws}/themes/{id}/where-used",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    responses(
        (status = 200, description = "What uses the theme", body = ThemeUsedByBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn where_used(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ThemeUsedByBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    zirv_flow_store::find_theme(&mut *connection, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let used = zirv_flow_store::theme_where_used(&mut connection, workspace.id, id).await?;
    Ok(Json(ThemeUsedByBody::from(&used)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_body_keeps_the_two_template_lists_apart() {
        let template = Uuid::now_v7();
        let used = ThemeUse {
            named_templates: vec![ThemeTemplate {
                id: template,
                name: "Welcome".into(),
                in_draft: true,
                in_current: false,
            }],
            default_templates: Vec::new(),
            active_flows: vec![TemplateFlowUse {
                flow_id: Uuid::nil(),
                flow_name: "Onboarding".into(),
                flow_state: "published".into(),
                template_ids: vec![template],
            }],
        };

        let body = serde_json::to_value(ThemeUsedByBody::from(&used)).unwrap();

        assert_eq!(
            body,
            json!({
                "named_templates": [{"id": template, "name": "Welcome",
                                     "in_draft": true, "in_current": false}],
                "default_templates": [],
                "active_flows": [{"flow_id": Uuid::nil(), "flow_name": "Onboarding",
                                  "flow_state": "published", "template_ids": [template]}]
            })
        );
    }
}
