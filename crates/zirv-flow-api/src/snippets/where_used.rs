//! Where a snippet is used: templates, themes and the active flows that send those templates.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{SnippetFlowUse, SnippetTemplate, SnippetThemeUse, SnippetUse};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// A template that holds a block for the snippet.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetTemplateBody {
    id: Uuid,
    name: String,
    /// Whether the template's draft holds the block.
    in_draft: bool,
    /// Whether the template's current version holds the block.
    in_current: bool,
}

/// A theme that uses the snippet as its footer.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetThemeBody {
    id: Uuid,
    name: String,
}

/// An active flow that sends a template holding the snippet.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetFlowBody {
    flow_id: Uuid,
    flow_name: String,
    /// `published` or `paused`.
    flow_state: String,
    /// The templates of the flow that hold the snippet.
    template_ids: Vec<Uuid>,
}

/// Everything that uses a snippet.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetUsedByBody {
    /// Templates, not archived, whose draft or current version holds a snippet block for it,
    /// in a section column.
    templates: Vec<SnippetTemplateBody>,
    /// Themes that use it as their footer. Always empty until themes exist (#304).
    themes: Vec<SnippetThemeBody>,
    /// Flows whose running version names one of those templates. A flow that does not run it now,
    /// and an archived flow, is not listed.
    active_flows: Vec<SnippetFlowBody>,
}

impl From<&SnippetTemplate> for SnippetTemplateBody {
    fn from(template: &SnippetTemplate) -> Self {
        Self {
            id: template.id,
            name: template.name.clone(),
            in_draft: template.in_draft,
            in_current: template.in_current,
        }
    }
}

impl From<&SnippetThemeUse> for SnippetThemeBody {
    fn from(theme: &SnippetThemeUse) -> Self {
        Self {
            id: theme.id,
            name: theme.name.clone(),
        }
    }
}

impl From<&SnippetFlowUse> for SnippetFlowBody {
    fn from(flow: &SnippetFlowUse) -> Self {
        Self {
            flow_id: flow.flow_id,
            flow_name: flow.flow_name.clone(),
            flow_state: flow.flow_state.clone(),
            template_ids: flow.template_ids.clone(),
        }
    }
}

impl From<&SnippetUse> for SnippetUsedByBody {
    fn from(used: &SnippetUse) -> Self {
        Self {
            templates: used.templates.iter().map(Into::into).collect(),
            themes: used.themes.iter().map(Into::into).collect(),
            active_flows: used.active_flows.iter().map(Into::into).collect(),
        }
    }
}

/// The templates, themes and active flows that use a snippet.
#[utoipa::path(
    get,
    operation_id = "snippet_where_used",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/where-used",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    responses(
        (status = 200, description = "What uses the snippet", body = SnippetUsedByBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn where_used(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SnippetUsedByBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    zirv_flow_store::find_snippet(&mut *connection, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let used = zirv_flow_store::snippet_where_used(&mut connection, workspace.id, id).await?;
    Ok(Json(SnippetUsedByBody::from(&used)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_body_carries_each_list_as_the_store_found_it() {
        let template = Uuid::now_v7();
        let flow = Uuid::now_v7();
        let used = SnippetUse {
            templates: vec![SnippetTemplate {
                id: template,
                name: "Welcome".into(),
                in_draft: true,
                in_current: false,
            }],
            themes: Vec::new(),
            active_flows: vec![SnippetFlowUse {
                flow_id: flow,
                flow_name: "Onboarding".into(),
                flow_state: "published".into(),
                template_ids: vec![template],
            }],
        };

        let body = serde_json::to_value(SnippetUsedByBody::from(&used)).unwrap();

        assert_eq!(
            body,
            json!({
                "templates": [{"id": template, "name": "Welcome",
                               "in_draft": true, "in_current": false}],
                "themes": [],
                "active_flows": [{"flow_id": flow, "flow_name": "Onboarding",
                                  "flow_state": "published", "template_ids": [template]}]
            })
        );
    }
}
