//! Publishing a snippet's draft into an immutable version, and reading the versions. Publishing
//! needs the workspace admin role.

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
    AssetOwnerKind, AssetReferenceError, PgPool, Snippet, SnippetUse, SnippetVersion,
    SnippetVersionSummary,
};
use zirv_flow_templates::{Issue, code, severity_of};

use super::crud::SnippetBody;
use super::where_used::SnippetUsedByBody;
use super::{check_references, parse_content, snippet_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem, TemplateIssueProblem};
use crate::templates::{field_catalog, issue_problems};
use crate::trace::request_trace;

/// One immutable published version.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetVersionBody {
    version: i32,
    /// The content as it was published.
    #[schema(value_type = Object)]
    content: Value,
    /// The warnings found when it was published; a blocking issue refuses the publish.
    warnings: Vec<TemplateIssueProblem>,
    published_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&SnippetVersion> for SnippetVersionBody {
    fn from(version: &SnippetVersion) -> Self {
        Self {
            version: version.version,
            content: version.content.clone(),
            warnings: stored_warnings(&version.validation),
            published_by: version.published_by,
            published_at: version.published_at,
        }
    }
}

/// One published version in the history list: no content, which `GET .../versions/{n}` returns.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetVersionSummaryBody {
    version: i32,
    /// The warnings stored with the version.
    warnings: i64,
    published_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&SnippetVersionSummary> for SnippetVersionSummaryBody {
    fn from(summary: &SnippetVersionSummary) -> Self {
        Self {
            version: summary.version,
            warnings: summary.warnings,
            published_by: summary.published_by,
            published_at: summary.published_at,
        }
    }
}

/// The issues stored with a version, as the response carries them.
pub(crate) fn stored_warnings(validation: &Value) -> Vec<TemplateIssueProblem> {
    let Some(issues) = validation.as_array() else {
        return Vec::new();
    };
    let text = |issue: &Value, key: &str| issue[key].as_str().map(str::to_owned);
    issues
        .iter()
        .filter_map(|issue| {
            Some(TemplateIssueProblem {
                code: text(issue, "code")?,
                severity: text(issue, "severity")?,
                at: text(issue, "at")?,
                field: text(issue, "field"),
                message: text(issue, "message"),
            })
        })
        .collect()
}

/// The new version, the snippet after the publish, and what uses the snippet.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetPublishedBody {
    snippet: SnippetBody,
    version: SnippetVersionBody,
    /// What picks the new version up: later sends use it without a template being republished.
    used_by: SnippetUsedByBody,
}

/// Validates the draft and publishes it as the next version, or changes nothing. Audited as
/// `snippet.published`.
///
/// The draft is read and validated with no lock and no connection held, because validation waits
/// on the render host for as long as its deadline. Only then does the publish lock the snippet,
/// and it publishes only if the draft is still the revision that was validated: a draft saved in
/// the meantime is a 409 `snippet_draft_changed`, and nothing is written.
pub(crate) async fn publish_snippet(
    pool: &PgPool,
    render: &RenderPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    published_by: Option<UserId>,
) -> Result<(Snippet, SnippetVersion, SnippetUse), ApiError> {
    let (draft, catalog, content, checked) = {
        let mut connection = pool.acquire().await.map_err(|_| ApiError::internal())?;
        let snippet = zirv_flow_store::find_snippet(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        if snippet.archived_at.is_some() {
            return Err(ApiError::snippet_archived());
        }
        let draft = zirv_flow_store::find_snippet_draft(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::internal)?;
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace).await?,
        );
        let content = parse_content(&draft.content)?;
        let checked = check_references(&mut connection, workspace, &content).await?;
        (draft, catalog, content, checked)
    };
    let mut issues: Vec<Issue> = render
        .validate_snippet(&content, &catalog)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    issues.extend(checked.issues.iter().cloned());
    if issues.iter().any(Issue::is_blocking) {
        return Err(ApiError::snippet_invalid(issue_problems(&issues)));
    }
    let validation = serde_json::to_value(&issues).map_err(|_| ApiError::internal())?;
    let trace = TraceContext {
        trace_id: request_trace().trace_id,
        cause: Cause::definition_change(id),
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_snippet(&mut tx, workspace, id)
        .await
        .map_err(snippet_error)?;
    if locked.draft().revision != draft.revision {
        // Dropping the transaction writes nothing.
        return Err(ApiError::snippet_draft_changed());
    }
    let (snippet, version) = zirv_flow_store::publish_snippet(
        &mut tx,
        workspace,
        &locked,
        &validation,
        published_by,
        &trace,
    )
    .await
    .map_err(snippet_error)?;
    let mut ids: Vec<Uuid> = checked.assets.iter().map(|(_, asset)| asset.id).collect();
    ids.sort_unstable();
    ids.dedup();
    let named: Vec<AssetId> = ids.into_iter().map(AssetId::from_uuid).collect();
    zirv_flow_store::record_references(
        &mut tx,
        workspace,
        AssetOwnerKind::Snippet,
        id,
        version.version,
        &named,
    )
    .await
    .map_err(|error| match error {
        // The asset was deleted after the check and before the lock.
        AssetReferenceError::Missing(_) => ApiError::snippet_invalid(issue_problems(&[Issue {
            code: code::BROKEN_ASSET_REFERENCE,
            severity: severity_of(code::BROKEN_ASSET_REFERENCE),
            at: "/blocks".to_owned(),
            field: None,
            message: Some("an image names an asset that was deleted".to_owned()),
        }])),
        AssetReferenceError::Store(error) => error.into(),
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "snippet.published",
        Some(("snippet", id)),
        json!({ "name": snippet.name, "version": version.version, "warnings": issues.len() }),
    )
    .await?;
    let used = zirv_flow_store::snippet_where_used(&mut tx, workspace, id).await?;
    zirv_flow_store::commit(tx).await?;
    Ok((snippet, version, used))
}

/// Publishes the draft as the next immutable version. The draft is validated by the renderer's
/// snippet rules (no section, no snippet block, no `visible_if`, known merge fields, sound
/// blocks), and each asset image must name a live asset of the workspace; a snippet with no block
/// is refused as `missing_content`. A theme logo image is accepted: the template or theme that
/// holds the snippet checks it. A blocking issue is a 422
/// `snippet_invalid` that lists every issue and writes nothing. The response lists the templates,
/// themes and active flows that use the snippet. Audited as `snippet.published`.
#[utoipa::path(
    post,
    operation_id = "publish_snippet",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/publish",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    responses(
        (status = 201, description = "The snippet, its new version and what uses it", body = SnippetPublishedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The snippet is archived (`snippet_archived`), or its draft was saved while it was being validated (`snippet_draft_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft has blocking issues (`snippet_invalid`, with `template_issues`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<(StatusCode, Json<SnippetPublishedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let render = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let (snippet, version, used) = publish_snippet(
        &state.pool,
        render,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(SnippetPublishedBody {
            snippet: SnippetBody::from(&snippet),
            version: SnippetVersionBody::from(&version),
            used_by: SnippetUsedByBody::from(&used),
        }),
    ))
}

/// The versions of a snippet, newest first.
#[utoipa::path(
    get,
    operation_id = "list_snippet_versions",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/versions",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    responses(
        (status = 200, description = "The versions without their content, newest first", body = Vec<SnippetVersionSummaryBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<SnippetVersionSummaryBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_snippet(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let versions = zirv_flow_store::list_snippet_versions(&state.pool, workspace.id, id).await?;
    Ok(Json(
        versions
            .iter()
            .map(SnippetVersionSummaryBody::from)
            .collect(),
    ))
}

/// One version of a snippet.
#[utoipa::path(
    get,
    operation_id = "get_snippet_version",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The version", body = SnippetVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, snippet or version, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<SnippetVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let version = zirv_flow_store::find_snippet_version(
        &state.pool,
        workspace.id,
        path_id(&id)?,
        path_id(&n)?,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(SnippetVersionBody::from(&version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_warnings_read_back_what_publish_stored() {
        let stored = json!([
            {"code": "image_without_alt", "severity": "warning", "at": "/blocks/block/i"},
            {"code": "insecure_link", "severity": "warning", "at": "/blocks/block/b",
             "field": "person.x", "message": "http"},
            {"code": "broken"}
        ]);

        let warnings = stored_warnings(&stored);

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, "image_without_alt");
        assert_eq!(warnings[1].field.as_deref(), Some("person.x"));
        assert!(stored_warnings(&json!({})).is_empty());
    }

    #[test]
    fn a_version_body_serves_the_content_as_published() {
        let version = SnippetVersion {
            snippet_id: Uuid::nil(),
            version: 2,
            content: json!({"blocks": []}),
            validation: json!([]),
            published_at: OffsetDateTime::UNIX_EPOCH,
            published_by: None,
            trace_id: Uuid::nil(),
        };

        let body = SnippetVersionBody::from(&version);

        assert_eq!((body.version, body.content), (2, json!({"blocks": []})));
        assert!(body.warnings.is_empty());
    }
}
