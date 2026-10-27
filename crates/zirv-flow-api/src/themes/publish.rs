//! Publishing a theme's draft into an immutable version, and reading the versions. Publishing
//! needs the workspace publisher role.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{AssetId, Cause, TraceContext, UserId, WorkspaceId};
use zirv_flow_render::RenderPool;
use zirv_flow_store::{
    AssetOwnerKind, AssetReferenceError, Candidate, Theme, ThemeUse, ThemeVersion,
    ThemeVersionSummary,
};
use zirv_flow_templates::{Issue, code, severity_of};

use super::crud::ThemeBody;
use super::where_used::ThemeUsedByBody;
use super::{check_references, parse_tokens, theme_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem, TemplateIssueProblem};
use crate::snippets::publish::stored_warnings;
use crate::templates::dependents::broken_by;
use crate::templates::issue_problems;
use crate::trace::request_trace;

/// One immutable published version.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeVersionBody {
    version: i32,
    /// The tokens as they were published.
    #[schema(value_type = Object)]
    tokens: Value,
    /// The warnings found when it was published; a blocking issue refuses the publish.
    warnings: Vec<TemplateIssueProblem>,
    published_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&ThemeVersion> for ThemeVersionBody {
    fn from(version: &ThemeVersion) -> Self {
        Self {
            version: version.version,
            tokens: version.tokens.clone(),
            warnings: stored_warnings(&version.validation),
            published_by: version.published_by,
            published_at: version.published_at,
        }
    }
}

/// One published version in the history list: no tokens, which `GET .../versions/{n}` returns.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeVersionSummaryBody {
    version: i32,
    /// The warnings stored with the version.
    warnings: i64,
    published_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&ThemeVersionSummary> for ThemeVersionSummaryBody {
    fn from(summary: &ThemeVersionSummary) -> Self {
        Self {
            version: summary.version,
            warnings: summary.warnings,
            published_by: summary.published_by,
            published_at: summary.published_at,
        }
    }
}

/// The new version, the theme after the publish, and what uses the theme.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemePublishedBody {
    theme: ThemeBody,
    version: ThemeVersionBody,
    /// What picks the new version up: later renders use it without a template being republished.
    used_by: ThemeUsedByBody,
}

/// Validates the draft and publishes it as the next version, or changes nothing. Audited as
/// `theme.published`.
///
/// The tokens are checked by the renderer's theme rules with no lock and no connection held,
/// because that waits on the render host for as long as its deadline. Only then does the publish
/// lock the theme, and it publishes only if the draft is still the revision that was checked: a
/// draft saved in the meantime is a 409 `theme_draft_changed`, and nothing is written. The logo
/// and the footer are checked under the lock.
///
/// Between the two, the live templates that render with the theme are validated with the draft in
/// place of the published version (see `templates::dependents`); one that would stop sending is a
/// 422 `theme_breaks_templates`.
pub(crate) async fn publish_theme(
    state: &AppState,
    render: &RenderPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    published_by: Option<UserId>,
) -> Result<(Theme, ThemeVersion, ThemeUse), ApiError> {
    let pool = &state.pool;
    let (draft, tokens, early) = {
        let mut connection = pool.acquire().await.map_err(|_| ApiError::internal())?;
        let theme = zirv_flow_store::find_theme(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        if theme.archived_at.is_some() {
            return Err(ApiError::theme_archived());
        }
        let draft = zirv_flow_store::find_theme_draft(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::internal)?;
        let tokens = parse_tokens(&draft.tokens)?;
        let early = check_references(&mut connection, workspace, &tokens).await?;
        (draft, tokens, early)
    };
    let mut issues: Vec<Issue> = render
        .validate_theme(&tokens)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    // The references are checked again under the lock; here they only keep a theme with a broken
    // logo or footer from being reported as breaking templates.
    let seen: Vec<Issue> = issues.iter().chain(&early.issues).cloned().collect();
    if seen.iter().any(Issue::is_blocking) {
        return Err(ApiError::theme_invalid(issue_problems(&seen)));
    }
    let live = {
        let mut connection = pool.acquire().await.map_err(|_| ApiError::internal())?;
        zirv_flow_store::theme_live_templates(&mut connection, workspace, id).await?
    };
    let candidate = Candidate::Theme {
        id,
        tokens: draft.tokens.clone(),
    };
    let broken = broken_by(state, render, workspace, live, &candidate).await?;
    if !broken.is_empty() {
        return Err(ApiError::theme_breaks_templates(broken));
    }
    let trace = TraceContext {
        trace_id: request_trace().trace_id,
        cause: Cause::definition_change(id),
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_theme(&mut tx, workspace, id)
        .await
        .map_err(theme_error)?;
    if locked.draft().revision != draft.revision {
        // Dropping the transaction writes nothing.
        return Err(ApiError::theme_draft_changed());
    }
    let checked = check_references(&mut tx, workspace, &tokens).await?;
    issues.extend(checked.issues);
    if issues.iter().any(Issue::is_blocking) {
        return Err(ApiError::theme_invalid(issue_problems(&issues)));
    }
    let validation = serde_json::to_value(&issues).map_err(|_| ApiError::internal())?;
    let (theme, version) = zirv_flow_store::publish_theme(
        &mut tx,
        workspace,
        &locked,
        &validation,
        published_by,
        &trace,
    )
    .await
    .map_err(theme_error)?;
    let named: Vec<AssetId> = checked
        .logo
        .iter()
        .map(|asset| AssetId::from_uuid(asset.id))
        .collect();
    zirv_flow_store::record_references(
        &mut tx,
        workspace,
        AssetOwnerKind::Theme,
        id,
        version.version,
        &named,
    )
    .await
    .map_err(|error| match error {
        // The asset was deleted after the check and before the lock was taken on it.
        AssetReferenceError::Missing(_) => ApiError::theme_invalid(issue_problems(&[Issue {
            code: code::BROKEN_ASSET_REFERENCE,
            severity: severity_of(code::BROKEN_ASSET_REFERENCE),
            at: "/theme/logo".to_owned(),
            field: None,
            message: Some("the logo names an asset that was deleted".to_owned()),
        }])),
        AssetReferenceError::Store(error) => error.into(),
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "theme.published",
        Some(("theme", id)),
        json!({ "name": theme.name, "version": version.version, "warnings": issues.len() }),
    )
    .await?;
    let used = zirv_flow_store::theme_where_used(&mut tx, workspace, id).await?;
    zirv_flow_store::commit(tx).await?;
    Ok((theme, version, used))
}

/// Publishes the draft as the next immutable version. The tokens are checked by the renderer's
/// theme rules (colours, width, font, URL and ids; contrast below WCAG AA is a warning and never
/// blocks), the logo must be a live asset of the workspace (`broken_asset_reference`), and the
/// footer a snippet of the workspace that is not archived and has a published version
/// (`broken_snippet_reference`). A blocking issue is a 422 `theme_invalid` that lists every issue
/// and writes nothing. The live templates that name the theme, or follow it while it is the
/// default, are validated with the draft in place of the published version, at most 200 in
/// template id order; past 200 the publish goes ahead on the result for the first 200. A blocking
/// issue in one of them, such as a removed footer in a marketing template, is a 422
/// `theme_breaks_templates` that lists up to 20 of them with their issue codes and the total, and
/// writes nothing. Warnings do not refuse. The logo's asset reference is written with the version. The response lists
/// the templates and active flows that use the theme. Audited as `theme.published`.
#[utoipa::path(
    post,
    operation_id = "publish_theme",
    path = "/api/v1/workspaces/{ws}/themes/{id}/publish",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    responses(
        (status = 201, description = "The theme, its new version and what uses it", body = ThemePublishedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a publisher, a key without the publish scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The theme is archived (`theme_archived`), or its draft was saved while it was being validated (`theme_draft_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft has blocking issues (`theme_invalid`, with `template_issues`), or would stop live templates from sending (`theme_breaks_templates`, with `templates` and `total`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Rendering is not available (`render_unavailable`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<ThemePublishedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Publish).await?;
    let render = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let (theme, version, used) = publish_theme(
        &state,
        render,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(ThemePublishedBody {
            theme: ThemeBody::from(&theme),
            version: ThemeVersionBody::from(&version),
            used_by: ThemeUsedByBody::from(&used),
        }),
    ))
}

/// The versions of a theme, newest first.
#[utoipa::path(
    get,
    operation_id = "list_theme_versions",
    path = "/api/v1/workspaces/{ws}/themes/{id}/versions",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    responses(
        (status = 200, description = "The versions without their tokens, newest first", body = Vec<ThemeVersionSummaryBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<ThemeVersionSummaryBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_theme(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let versions = zirv_flow_store::list_theme_versions(&state.pool, workspace.id, id).await?;
    Ok(Json(
        versions.iter().map(ThemeVersionSummaryBody::from).collect(),
    ))
}

/// One version of a theme.
#[utoipa::path(
    get,
    operation_id = "get_theme_version",
    path = "/api/v1/workspaces/{ws}/themes/{id}/versions/{n}",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The version", body = ThemeVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, theme or version, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<ThemeVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let version =
        zirv_flow_store::find_theme_version(&state.pool, workspace.id, path_id(&id)?, path_id(&n)?)
            .await?
            .ok_or_else(ApiError::not_found)?;
    Ok(Json(ThemeVersionBody::from(&version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_body_serves_the_tokens_as_published_and_the_warnings_stored() {
        let version = ThemeVersion {
            theme_id: Uuid::nil(),
            version: 2,
            tokens: json!({"primary": "#123456"}),
            validation: json!([
                {"code": "low_contrast", "severity": "warning", "at": "/theme/text"}
            ]),
            published_at: OffsetDateTime::UNIX_EPOCH,
            published_by: None,
            trace_id: Uuid::nil(),
        };

        let body = ThemeVersionBody::from(&version);

        assert_eq!(
            (body.version, body.tokens),
            (2, json!({"primary": "#123456"}))
        );
        assert_eq!(body.warnings.len(), 1);
        assert_eq!(body.warnings[0].code, "low_contrast");
    }

    #[test]
    fn a_summary_body_carries_the_warning_count_and_no_tokens() {
        let summary = ThemeVersionSummary {
            version: 3,
            published_at: OffsetDateTime::UNIX_EPOCH,
            published_by: None,
            warnings: 2,
        };

        let body = serde_json::to_value(ThemeVersionSummaryBody::from(&summary)).unwrap();

        assert_eq!(body["warnings"], 2);
        assert!(body.get("tokens").is_none());
    }
}
