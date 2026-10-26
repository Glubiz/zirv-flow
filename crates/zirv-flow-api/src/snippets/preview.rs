//! Preview: a snippet's draft or one of its versions rendered for the sample person.
//!
//! The snippet's blocks are placed as the one column of a built-in sample block document, with no
//! theme and no footer, so no snippet resolution is needed. The render happens only in the render
//! pool of the application state; this process never renders. Links are rewritten to inert
//! tokens, so no link is live.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{Asset, asset_url};
use zirv_flow_templates::{
    Block, BlocksContent, Category, FooterRef, Hooks, Issue, RenderContext, RenderExtras, Section,
    SnippetContent, TemplateDocument, ThemeRef, Tracking,
};

use super::{check_references, parse_content};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, TemplateIssueProblem};
use crate::templates::{field_catalog, issue_problems};

/// Where an inert link points: the structure of a real tracked link, none of its meaning.
const INERT_BASE_URL: &str = "https://preview.sample.invalid/t";

/// The host asset URLs get when the server has no `https` public URL: the renderer takes `https`
/// URLs only, so the image is placed but the browser cannot load it.
const INERT_ASSET_HOST: &str = "https://preview.sample.invalid";

#[derive(Deserialize, ToSchema)]
pub(crate) struct SnippetPreviewRequest {
    /// Preview this published version. Absent: the draft.
    version: Option<i32>,
}

/// The rendered snippet and its validation results.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetPreviewBody {
    /// `draft` or `version`.
    source: &'static str,
    version: Option<i32>,
    /// False when the snippet has blocking issues, so nothing was rendered.
    rendered: bool,
    /// The sample document's HTML, with inert tracking links.
    html: Option<String>,
    /// The sample document's text part.
    text: Option<String>,
    /// The same checks a publish makes, against the workspace's fields and assets now.
    issues: Vec<TemplateIssueProblem>,
}

/// The sample document a snippet is previewed in: its blocks as the single column of one section,
/// with `theme: "none"`, `footer: "none"` and the default settings.
fn sample_document(blocks: Vec<Block>) -> TemplateDocument {
    let mut content = BlocksContent::new("Snippet preview");
    content.category = Category::Transactional;
    content.theme = ThemeRef::None;
    content.footer = FooterRef::None;
    content.sections.push(Section {
        id: "snippet".to_owned(),
        background_color: None,
        padding: None,
        visible_if: None,
        columns: vec![blocks],
    });
    TemplateDocument::email_blocks(content)
}

/// The URL of each named asset, by the id as the block spells it.
fn asset_urls(assets: &[(String, Asset)], public_url: Option<&str>) -> RenderExtras {
    let base = public_url
        .filter(|url| url.starts_with("https://"))
        .unwrap_or(INERT_ASSET_HOST);
    let mut extras = RenderExtras::default();
    for (named, asset) in assets {
        extras.assets.insert(
            named.clone(),
            asset_url(Some(base), &asset.sha256, &asset.ext),
        );
    }
    extras
}

/// Renders the draft or a version of a snippet for the sample person. A snippet with blocking
/// issues, including an asset that is missing, is not rendered: the response lists the issues and
/// `rendered` is false. A render that fails its limits (memory, deadline) or whose host dies is a
/// problem document, and the next preview works.
#[utoipa::path(
    post,
    operation_id = "preview_snippet",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/preview",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    request_body = SnippetPreviewRequest,
    responses(
        (status = 200, description = "The rendered snippet and its validation results", body = SnippetPreviewBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, snippet or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The renderer refused the content (`template_render_failed`), or it used too much memory or time (`render_memory_limit`, `render_deadline`)", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The render host answered unusably (`render_protocol`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The render host stopped (`render_child_exited`), or rendering is not available (`render_unavailable`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SnippetPreviewRequest>,
) -> Result<Json<SnippetPreviewBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let (content, source, checked, catalog) = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        zirv_flow_store::find_snippet(&mut *connection, workspace.id, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        let (stored, source) = match body.version {
            Some(number) => {
                let version = zirv_flow_store::find_snippet_version(
                    &mut *connection,
                    workspace.id,
                    id,
                    number,
                )
                .await?
                .ok_or_else(ApiError::not_found)?;
                (version.content, "version")
            }
            None => {
                let draft = zirv_flow_store::find_snippet_draft(&mut *connection, workspace.id, id)
                    .await?
                    .ok_or_else(ApiError::internal)?;
                (draft.content, "draft")
            }
        };
        let content: SnippetContent = parse_content(&stored)?;
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace.id).await?,
        );
        let checked = check_references(&mut connection, workspace.id, &content, false).await?;
        (content, source, checked, catalog)
    };
    let pool = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let mut issues: Vec<Issue> = pool
        .validate_snippet(&content, &catalog)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    issues.extend(checked.issues.iter().cloned());
    let blocked = issues.iter().any(Issue::is_blocking);
    let (html, text) = if blocked {
        (None, None)
    } else {
        let doc = sample_document(content.blocks);
        let context = RenderContext::sample_person(&catalog);
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: INERT_BASE_URL,
            },
            utm: None,
        };
        let extras = asset_urls(&checked.assets, state.public_url());
        let email = pool
            .render_email_with(&doc, &context, &hooks, &extras)
            .await
            .map_err(|failure| ApiError::render_failed(&failure))?;
        (Some(email.html), Some(email.text))
    };
    Ok(Json(SnippetPreviewBody {
        source,
        version: body.version,
        rendered: !blocked,
        html,
        text,
        issues: issue_problems(&issues),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::OffsetDateTime;
    use zirv_flow_templates::{Mode, SCHEMA_VERSION};

    use super::*;

    fn asset(sha: u8) -> Asset {
        Asset {
            id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            sha256: vec![sha; 32],
            ext: "png".into(),
            content_type: "image/png".into(),
            size_bytes: 1,
            width: 1,
            height: 1,
            name: "logo".into(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_sample_document_holds_the_blocks_as_one_column_without_theme_or_footer() {
        let content = parse_content(&json!({"blocks": [
            {"type": "heading", "id": "h", "level": 2, "text": "Hi"},
            {"type": "divider", "id": "d"}
        ]}))
        .unwrap();

        let doc = sample_document(content.blocks.clone());

        assert_eq!((doc.schema, doc.mode), (SCHEMA_VERSION, Mode::Blocks));
        let blocks = doc.blocks.unwrap();
        assert_eq!(
            (blocks.theme, blocks.footer),
            (ThemeRef::None, FooterRef::None)
        );
        assert_eq!(blocks.sections.len(), 1);
        assert_eq!(blocks.sections[0].columns, vec![content.blocks]);
    }

    #[test]
    fn an_asset_url_is_absolute_https_whatever_the_public_url() {
        let assets = vec![("a1".to_owned(), asset(0xab))];
        let hex = "ab".repeat(32);

        let public = asset_urls(&assets, Some("https://flow.example.com"));
        let plain = asset_urls(&assets, Some("http://localhost:8080"));
        let none = asset_urls(&assets, None);

        assert_eq!(
            public.asset_url("a1"),
            Some(format!("https://flow.example.com/a/{hex}.png").as_str())
        );
        for inert in [plain, none] {
            assert_eq!(
                inert.asset_url("a1"),
                Some(format!("{INERT_ASSET_HOST}/a/{hex}.png").as_str())
            );
        }
    }

    #[test]
    fn a_preview_request_names_a_version_or_nothing() {
        let draft: SnippetPreviewRequest = serde_json::from_str("{}").unwrap();
        let version: SnippetPreviewRequest = serde_json::from_str(r#"{"version":2}"#).unwrap();

        assert_eq!((draft.version, version.version), (None, Some(2)));
    }
}
