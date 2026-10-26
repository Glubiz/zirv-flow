//! Preview: a theme's draft or one of its versions applied to a built-in sample document and
//! rendered for the sample person.
//!
//! The sample document has a logo, a heading, text with a link, a button and a divider, and it
//! follows the theme, so the footer snippet closes it. The render happens only in the render pool
//! of the application state; this process never renders. Links are rewritten to inert tokens, so
//! no link is live.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{Asset, PgConnection};
use zirv_flow_templates::{
    Hooks, Issue, RenderContext, RenderExtras, Severity, SnippetContent, TemplateDocument,
    ThemeTokens, Tracking, code,
};

use super::{check_references, parse_tokens};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem, TemplateIssueProblem};
use crate::snippets::preview::{INERT_BASE_URL, THEME_LOGO_STAND_IN, asset_urls};
use crate::snippets::{self, parse_content};
use crate::templates::{field_catalog, issue_problems, parse_document};

/// The asset id the preview gives its stand-in logo, so a theme with no usable logo still renders.
/// It is an id because the theme validator wants one.
const STAND_IN_LOGO: &str = "00000000-0000-7000-8000-000000000000";

#[derive(Deserialize, ToSchema)]
pub(crate) struct ThemePreviewRequest {
    /// Preview this published version. Absent: the draft.
    version: Option<i32>,
}

/// The rendered sample and the theme's validation results.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemePreviewBody {
    /// `draft` or `version`.
    source: &'static str,
    version: Option<i32>,
    /// False when the tokens have blocking issues, so nothing was rendered.
    rendered: bool,
    /// The sample document's HTML, with inert tracking links.
    html: Option<String>,
    /// The sample document's text part.
    text: Option<String>,
    /// The checks a publish makes. A logo or footer that cannot be used is a warning here: the
    /// preview shows a stand-in logo or no footer.
    issues: Vec<TemplateIssueProblem>,
}

/// The sample block document the theme is shown on: a logo, a heading, text with a link, a button
/// and a divider, following the workspace theme with its footer.
fn sample_document() -> Result<TemplateDocument, ApiError> {
    parse_document(&json!({
        "schema": 2, "channel": "email", "mode": "blocks",
        "blocks": {
            "subject": "Theme preview",
            "category": "transactional",
            "theme": "default",
            "footer": "theme",
            "sections": [{"id": "sample", "columns": [[
                {"type": "image", "id": "logo", "alt": "Logo", "src": {"theme": "logo"}},
                {"type": "heading", "id": "title", "level": 1, "text": "A heading"},
                {"type": "text", "id": "body", "content":
                    "<p>Body text with a <a href=\"https://example.com/page\">link</a>.</p>"},
                {"type": "button", "id": "action", "label": "A button",
                 "url": "https://example.com/action"},
                {"type": "divider", "id": "rule"}
            ]]}]
        }
    }))
}

/// What the references of the theme being previewed came to.
struct Resolved {
    /// The footer snippet's current content, if the theme has a usable footer.
    footer: Option<SnippetContent>,
    /// The live assets the logo and the footer name, with the id as they spell it.
    assets: Vec<(String, Asset)>,
    /// Warnings for the references that could not be used.
    issues: Vec<Issue>,
}

fn warning(mut issue: Issue) -> Issue {
    issue.severity = Severity::Warning;
    issue
}

/// The footer snippet's current version as content, with the assets it names. A footer that is
/// missing, unpublished, unreadable or names a deleted asset is dropped with a warning.
async fn resolve(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    tokens: &ThemeTokens,
) -> Result<Resolved, ApiError> {
    let checked = check_references(connection, workspace, tokens).await?;
    let mut resolved = Resolved {
        footer: None,
        assets: Vec::new(),
        issues: checked.issues.into_iter().map(warning).collect(),
    };
    if let (Some(asset), Some(named)) = (checked.logo, &tokens.logo) {
        resolved.assets.push((named.clone(), asset));
    }
    let Some(footer) = checked.footer else {
        return Ok(resolved);
    };
    let at = "/theme/footer_snippet";
    let content = footer_content(connection, workspace, footer).await?;
    let Some(content) = content else {
        resolved.issues.push(warning(snippets::issue(
            code::BROKEN_SNIPPET_REFERENCE,
            at.to_owned(),
            "the footer snippet has no content to show",
        )));
        return Ok(resolved);
    };
    let held = snippets::check_references(connection, workspace, &content).await?;
    if held
        .issues
        .iter()
        .any(|issue| issue.code == code::BROKEN_ASSET_REFERENCE)
    {
        resolved.issues.push(warning(snippets::issue(
            code::BROKEN_ASSET_REFERENCE,
            at.to_owned(),
            "the footer snippet names an asset that does not exist or was deleted",
        )));
        return Ok(resolved);
    }
    resolved.assets.extend(held.assets);
    resolved.footer = Some(content);
    Ok(resolved)
}

/// The content of the snippet's current version, if it has one that parses.
async fn footer_content(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    snippet: Uuid,
) -> Result<Option<SnippetContent>, ApiError> {
    let Some(found) = zirv_flow_store::find_snippet(&mut *connection, workspace, snippet).await?
    else {
        return Ok(None);
    };
    let Some(number) = found.current_version else {
        return Ok(None);
    };
    let version =
        zirv_flow_store::find_snippet_version(&mut *connection, workspace, snippet, number).await?;
    Ok(version.and_then(|version| parse_content(&version.content).ok()))
}

/// The theme and what the renderer needs to apply it: the logo's URL (a stand-in when it has none
/// or it cannot be used) and the footer snippet's content.
fn render_extras(
    name: &str,
    mut tokens: ThemeTokens,
    resolved: &Resolved,
    public_url: Option<&str>,
) -> RenderExtras {
    let mut extras = asset_urls(&resolved.assets, public_url);
    let logo_is_live = tokens
        .logo
        .as_ref()
        .is_some_and(|named| resolved.assets.iter().any(|(live, _)| live == named));
    if !logo_is_live {
        tokens.logo = Some(STAND_IN_LOGO.to_owned());
        extras
            .assets
            .insert(STAND_IN_LOGO.to_owned(), THEME_LOGO_STAND_IN.to_owned());
    }
    match (&tokens.footer_snippet, &resolved.footer) {
        (Some(id), Some(content)) => {
            extras.snippets.insert(id.clone(), content.clone());
        }
        _ => tokens.footer_snippet = None,
    }
    extras.theme = Some(tokens);
    extras.theme_name = Some(name.to_owned());
    extras
}

/// Renders the sample document under the draft tokens, or under a published version's, for the
/// sample person. Tokens with blocking issues are not rendered: the response lists the issues and
/// `rendered` is false. A logo that is not a live asset is shown as a stand-in image, and a footer
/// snippet that cannot be used is left out, each with a warning (`broken_asset_reference`,
/// `broken_snippet_reference`). A render that fails its limits (memory, deadline) or whose host
/// dies is a problem document, and the next preview works.
#[utoipa::path(
    post,
    operation_id = "preview_theme",
    path = "/api/v1/workspaces/{ws}/themes/{id}/preview",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    request_body = ThemePreviewRequest,
    responses(
        (status = 200, description = "The rendered sample and the validation results", body = ThemePreviewBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, theme or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The renderer refused the sample (`template_render_failed`), or it used too much memory or time (`render_memory_limit`, `render_deadline`)", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The render host answered unusably (`render_protocol`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The render host stopped (`render_child_exited`), or rendering is not available (`render_unavailable`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn preview(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<ThemePreviewRequest>,
) -> Result<Json<ThemePreviewBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let id: Uuid = path_id(&id)?;
    let (name, tokens, source, resolved, catalog) = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        let theme = zirv_flow_store::find_theme(&mut *connection, workspace.id, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        let (stored, source) = match body.version {
            Some(number) => {
                let version =
                    zirv_flow_store::find_theme_version(&mut *connection, workspace.id, id, number)
                        .await?
                        .ok_or_else(ApiError::not_found)?;
                (version.tokens, "version")
            }
            None => {
                let draft = zirv_flow_store::find_theme_draft(&mut *connection, workspace.id, id)
                    .await?
                    .ok_or_else(ApiError::internal)?;
                (draft.tokens, "draft")
            }
        };
        let tokens = parse_tokens(&stored)?;
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace.id).await?,
        );
        let resolved = resolve(&mut connection, workspace.id, &tokens).await?;
        (theme.name, tokens, source, resolved, catalog)
    };
    let pool = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let mut issues: Vec<Issue> = pool
        .validate_theme(&tokens)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    let blocked = issues.iter().any(Issue::is_blocking);
    issues.extend(resolved.issues.iter().cloned());
    let (html, text) = if blocked {
        (None, None)
    } else {
        let doc = sample_document()?;
        let context = RenderContext::sample_person(&catalog);
        let hooks = Hooks {
            tracking: Tracking::Inert {
                base_url: INERT_BASE_URL,
            },
            utm: None,
        };
        let extras = render_extras(&name, tokens, &resolved, state.public_url());
        let email = pool
            .render_email_with(&doc, &context, &hooks, &extras)
            .await
            .map_err(|failure| ApiError::render_failed(&failure))?;
        (Some(email.html), Some(email.text))
    };
    Ok(Json(ThemePreviewBody {
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
    use time::OffsetDateTime;
    use zirv_flow_templates::{Mode, SCHEMA_VERSION, ThemeRef};

    use super::*;

    fn asset() -> Asset {
        Asset {
            id: Uuid::nil(),
            workspace_id: Uuid::nil(),
            sha256: vec![0xab; 32],
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

    fn resolved(footer: Option<SnippetContent>, assets: Vec<(String, Asset)>) -> Resolved {
        Resolved {
            footer,
            assets,
            issues: Vec::new(),
        }
    }

    #[test]
    fn the_sample_document_follows_the_theme_and_holds_every_block_the_theme_styles() {
        let doc = sample_document().unwrap();

        assert_eq!((doc.schema, doc.mode), (SCHEMA_VERSION, Mode::Blocks));
        let blocks = doc.blocks.unwrap();
        assert_eq!(blocks.theme, ThemeRef::Default);
        let types: Vec<&str> = blocks.sections[0].columns[0]
            .iter()
            .map(|block| match block {
                zirv_flow_templates::Block::Image(_) => "image",
                zirv_flow_templates::Block::Heading(_) => "heading",
                zirv_flow_templates::Block::Text(_) => "text",
                zirv_flow_templates::Block::Button(_) => "button",
                zirv_flow_templates::Block::Divider(_) => "divider",
                _ => "other",
            })
            .collect();
        assert_eq!(types, ["image", "heading", "text", "button", "divider"]);
    }

    #[test]
    fn a_theme_with_no_usable_logo_shows_the_stand_in_and_one_with_a_live_logo_its_url() {
        let live_id = "11111111-1111-7111-8111-111111111111";
        let none = render_extras(
            "Brand",
            ThemeTokens::default(),
            &resolved(None, vec![]),
            None,
        );
        let gone = ThemeTokens {
            logo: Some(live_id.into()),
            ..ThemeTokens::default()
        };
        let missing = render_extras("Brand", gone.clone(), &resolved(None, vec![]), None);
        let live = render_extras(
            "Brand",
            gone,
            &resolved(None, vec![(live_id.into(), asset())]),
            Some("https://flow.example.com"),
        );

        assert_eq!(none.theme_logo_url(), Some(THEME_LOGO_STAND_IN));
        assert_eq!(missing.theme_logo_url(), Some(THEME_LOGO_STAND_IN));
        let hex = "ab".repeat(32);
        assert_eq!(
            live.theme_logo_url(),
            Some(format!("https://flow.example.com/a/{hex}.png").as_str())
        );
        assert_eq!(live.theme_name.as_deref(), Some("Brand"));
    }

    #[test]
    fn the_footer_is_passed_only_when_it_resolved() {
        let footer_id = "22222222-2222-7222-8222-222222222222";
        let tokens = ThemeTokens {
            footer_snippet: Some(footer_id.into()),
            ..ThemeTokens::default()
        };
        let content = SnippetContent::default();

        let with = render_extras(
            "Brand",
            tokens.clone(),
            &resolved(Some(content.clone()), vec![]),
            None,
        );
        let without = render_extras("Brand", tokens, &resolved(None, vec![]), None);

        assert_eq!(with.snippets.get(footer_id), Some(&content));
        assert_eq!(
            with.theme.and_then(|theme| theme.footer_snippet).as_deref(),
            Some(footer_id)
        );
        assert!(without.snippets.is_empty());
        assert_eq!(without.theme.unwrap().footer_snippet, None);
    }

    #[test]
    fn a_reference_issue_is_softened_to_a_warning() {
        let blocking = snippets::issue(code::BROKEN_ASSET_REFERENCE, "/theme/logo".into(), "m");
        assert!(blocking.is_blocking());

        assert!(!warning(blocking).is_blocking());
    }

    #[test]
    fn a_preview_request_names_a_version_or_nothing() {
        let draft: ThemePreviewRequest = serde_json::from_str("{}").unwrap();
        let version: ThemePreviewRequest = serde_json::from_str(r#"{"version":2}"#).unwrap();

        assert_eq!((draft.version, version.version), (None, Some(2)));
    }
}
