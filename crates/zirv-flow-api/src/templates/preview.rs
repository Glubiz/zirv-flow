//! Preview: a template's draft or one of its versions rendered for a sample person or, for an
//! admin, a real one (architecture 11.7).
//!
//! The render happens only in the render pool of the application state, in a child process with
//! an OS memory limit and a deadline; this process never renders a template itself, so a template
//! that exhausts memory costs one preview and not the API. Links are rewritten to inert tokens, so
//! the structure matches a real send and no link is live.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_render::RenderPool;
use zirv_flow_store::TemplateFieldCatalog;
use zirv_flow_templates::{
    Hooks, Issue, RenderContext, RenderKind, TemplateDocument, Tracking, content_type_of,
};

use super::{authorize_render, field_catalog, issue_problems, stored_document};
use crate::AppState;
use crate::admin;
use crate::auth::{Principal, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, TemplateIssueProblem};

/// Where an inert link points: the structure of a real tracked link, none of its meaning.
const INERT_BASE_URL: &str = "https://preview.sample.invalid/t";

#[derive(Deserialize, ToSchema)]
pub(crate) struct TemplatePreviewRequest {
    /// Preview as this person. Needs the workspace admin role, because the render shows their
    /// values. Absent: a sample person built from the field registry's types.
    person_id: Option<Uuid>,
    /// Preview this published version. Absent: the draft.
    version: Option<i32>,
}

/// A header of the rendered message or request.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateHeaderBody {
    name: String,
    value: String,
}

/// The rendered template and its validation results.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplatePreviewBody {
    /// `draft` or `version`.
    source: &'static str,
    version: Option<i32>,
    /// The registered content type that rendered it (`html`, `webhook`).
    content_type: &'static str,
    /// The person whose values were used; null for the sample person. When the id asked for was
    /// merged away, the survivor.
    person_id: Option<Uuid>,
    /// False when the document has blocking issues, so nothing was rendered.
    rendered: bool,
    /// An email's subject.
    subject: Option<String>,
    /// An email's HTML, with inert tracking links.
    html: Option<String>,
    /// An email's text part.
    text: Option<String>,
    /// An email's extra headers (`List-Unsubscribe` for marketing mail) or a webhook's headers.
    headers: Vec<TemplateHeaderBody>,
    /// A webhook's JSON body.
    body: Option<String>,
    /// The same checks a publish makes, against the workspace's fields now.
    issues: Vec<TemplateIssueProblem>,
}

/// The rendered parts of a preview.
struct Rendered {
    subject: Option<String>,
    html: Option<String>,
    text: Option<String>,
    headers: Vec<TemplateHeaderBody>,
    body: Option<String>,
}

fn headers_of(headers: Vec<(String, String)>) -> Vec<TemplateHeaderBody> {
    headers
        .into_iter()
        .map(|(name, value)| TemplateHeaderBody { name, value })
        .collect()
}

/// Renders `doc` through the pool, by the render call its content type uses.
async fn render(
    pool: &RenderPool,
    kind: RenderKind,
    doc: &TemplateDocument,
    context: &RenderContext,
) -> Result<Rendered, ApiError> {
    match kind {
        RenderKind::Email => {
            let hooks = Hooks {
                tracking: Tracking::Inert {
                    base_url: INERT_BASE_URL,
                },
                utm: None,
            };
            let email = pool
                .render_email(doc, context, &hooks)
                .await
                .map_err(|failure| ApiError::render_failed(&failure))?;
            Ok(Rendered {
                subject: Some(email.subject),
                html: Some(email.html),
                text: Some(email.text),
                headers: headers_of(email.headers),
                body: None,
            })
        }
        RenderKind::Webhook => {
            let webhook = pool
                .render_webhook(doc, context)
                .await
                .map_err(|failure| ApiError::render_failed(&failure))?;
            Ok(Rendered {
                subject: None,
                html: None,
                text: None,
                headers: headers_of(webhook.headers),
                body: Some(webhook.body),
            })
        }
    }
}

/// Renders the draft or a version of a template for a sample or a real person. Without
/// `person_id` it needs the write role; with one, the admin role (until the `view_pii` grant
/// exists), because the render shows that person's values. A render that fails its limits (memory,
/// deadline) or whose host dies is a problem document, and the next preview works. A document with
/// blocking issues is not rendered: the response lists the issues and `rendered` is false.
#[utoipa::path(
    post,
    operation_id = "preview_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}/preview",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = TemplatePreviewRequest,
    responses(
        (status = 200, description = "The rendered template and its validation results", body = TemplatePreviewBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, a missing CSRF token, or a real person asked for without the admin role (`code` is `template_preview_values_forbidden`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, template, version or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The renderer refused the template (`template_render_failed`), it used too much memory or time (`render_memory_limit`, `render_deadline`), or no registered content type handles it", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The render host answered unusably (`render_protocol`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The render host stopped (`render_child_exited`), or rendering is not available (`render_unavailable`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<TemplatePreviewRequest>,
) -> Result<Json<TemplatePreviewBody>, ApiError> {
    let ws = path_id(&ws)?;
    let workspace = authorize_render(&state, &principal, ws, body.person_id.is_some()).await?;
    let id: Uuid = path_id(&id)?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::find_template(&mut *tx, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (document, source) = match body.version {
        Some(number) => {
            let version =
                zirv_flow_store::find_template_version(&mut *tx, workspace.id, id, number)
                    .await?
                    .ok_or_else(ApiError::not_found)?;
            (version.document, "version")
        }
        None => {
            let draft = zirv_flow_store::find_template_draft(&mut *tx, workspace.id, id)
                .await?
                .ok_or_else(ApiError::internal)?;
            (draft.document, "draft")
        }
    };
    let doc = stored_document(&document)?;
    let read: TemplateFieldCatalog =
        zirv_flow_store::read_template_field_catalog(&mut tx, workspace.id).await?;
    let catalog = field_catalog(read);
    let (context, person_id) = match body.person_id {
        None => (RenderContext::sample_person(&catalog), None),
        Some(requested) => {
            let person = zirv_flow_store::find_person(&mut *tx, workspace.id, requested)
                .await?
                .ok_or_else(ApiError::not_found)?;
            let email = zirv_flow_store::list_identities(&mut *tx, workspace.id, person.id)
                .await?
                .into_iter()
                .find(|identity| identity.kind == "email")
                .map(|identity| identity.value);
            (
                RenderContext::real_person(&catalog, &person.traits, email),
                Some(person.id),
            )
        }
    };
    if let Some(person) = person_id {
        // A real person's values were read for this preview: record who looked, never the values.
        admin::audit(
            &mut tx,
            &principal.context(),
            Some(workspace.id),
            "template.previewed",
            Some(("template", id)),
            json!({ "person_id": person, "source": source, "version": body.version }),
        )
        .await?;
        zirv_flow_store::commit(tx).await?;
    } else {
        drop(tx);
    }

    let content = content_type_of(&doc).map_err(|_| ApiError::template_content_unsupported())?;
    // Validation renders a sample, so it runs in the render host like the render itself.
    let pool = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let issues: Vec<Issue> = pool
        .validate(&doc, &catalog)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    let blocked = issues.iter().any(Issue::is_blocking);
    let rendered = if blocked {
        None
    } else {
        Some(render(pool, content.render_kind(), &doc, &context).await?)
    };
    let (subject, html, text, headers, rendered_body) = match rendered {
        Some(parts) => (
            parts.subject,
            parts.html,
            parts.text,
            parts.headers,
            parts.body,
        ),
        None => (None, None, None, Vec::new(), None),
    };
    Ok(Json(TemplatePreviewBody {
        source,
        version: body.version,
        content_type: content.id(),
        person_id,
        rendered: !blocked,
        subject,
        html,
        text,
        headers,
        body: rendered_body,
        issues: issue_problems(&issues),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_render::RenderFailure;
    use zirv_flow_templates::FieldCatalog;

    use super::*;

    fn catalog() -> FieldCatalog {
        let mut catalog = FieldCatalog::default();
        catalog.person.insert("plan".into(), TraitType::String);
        catalog.person.insert("age".into(), TraitType::Number);
        catalog.person.insert("email".into(), TraitType::String);
        catalog
    }

    #[test]
    fn the_sample_person_has_a_value_per_type_and_an_address() {
        let context = RenderContext::sample_person(&catalog());

        assert_eq!(context.person["plan"], json!("sample"));
        assert_eq!(context.person["age"], json!(1));
        assert_eq!(
            context.person["email"],
            json!(zirv_flow_templates::SAMPLE_EMAIL)
        );
        assert_eq!(context, RenderContext::sample_person(&catalog()));
    }

    #[test]
    fn every_render_failure_is_a_problem_with_its_own_code() {
        use axum::http::StatusCode;
        use zirv_flow_templates::RenderError;

        let cases = [
            (
                RenderFailure::MemoryLimit,
                "render_memory_limit",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                RenderFailure::Deadline,
                "render_deadline",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                RenderFailure::ChildExited,
                "render_child_exited",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                RenderFailure::Protocol,
                "render_protocol",
                StatusCode::BAD_GATEWAY,
            ),
            (
                RenderFailure::RequestTooLarge,
                "render_request_too_large",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                RenderFailure::Render(RenderError::Undefined { field: "html" }),
                "template_render_failed",
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (failure, code, status) in cases {
            let problem = ApiError::render_failed(&failure);
            assert_eq!(problem.code(), Some(code), "{failure}");
            assert_eq!(problem.status(), status, "{failure}");
        }
        let undefined = ApiError::render_failed(&RenderFailure::Render(RenderError::Undefined {
            field: "html",
        }));
        assert!(undefined.detail().contains("html"));
    }
}
