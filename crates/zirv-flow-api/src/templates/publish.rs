//! Publishing a template's draft into an immutable version, and reading the versions. Publishing
//! needs the workspace admin role (ruling 43163028).

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{AssetId, Cause, TraceContext, UserId, WorkspaceId};
use zirv_flow_render::RenderPool;
use zirv_flow_store::{
    AssetOwnerKind, AssetReferenceError, SenderError, TEMPLATE_NOTE_MAX_CHARS, Template,
    TemplateUse, TemplateVersion, TemplateVersionSummary, require_verified_sender,
};
use zirv_flow_templates::{Issue, code, content_type_of, extras_needs, migrate_to_current};

use super::crud::TemplateBody;
use super::inputs::{Subject, resolve};
use super::where_used::TemplateUseBody;
use super::{field_catalog, issue_problems, stored_document, template_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, ItemProblem, Problem, TemplateIssueProblem};
use crate::sending::sender_error;
use crate::trace::request_trace;

/// One immutable published version.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateVersionBody {
    version: i32,
    /// The document as it was published.
    #[schema(value_type = Object)]
    document: Value,
    /// The warnings found when it was published; a blocking issue refuses the publish.
    warnings: Vec<TemplateIssueProblem>,
    published_by: Option<Uuid>,
    /// The note written when it was published.
    note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&TemplateVersion> for TemplateVersionBody {
    fn from(version: &TemplateVersion) -> Self {
        Self {
            version: version.version,
            // A version is stored as it was published and served at the current schema.
            document: migrate_to_current(version.document.clone())
                .unwrap_or_else(|_| version.document.clone()),
            warnings: stored_warnings(&version.validation),
            published_by: version.published_by,
            note: version.note.clone(),
            published_at: version.published_at,
        }
    }
}

/// One published version in the history list: no document, which `GET .../versions/{n}` returns.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateVersionSummaryBody {
    version: i32,
    /// The blocking issues stored with the version; always 0, because a blocking issue refuses the
    /// publish.
    blocking: i64,
    /// The warnings stored with the version.
    warnings: i64,
    published_by: Option<Uuid>,
    /// The note written when it was published.
    note: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    published_at: OffsetDateTime,
}

impl From<&TemplateVersionSummary> for TemplateVersionSummaryBody {
    fn from(summary: &TemplateVersionSummary) -> Self {
        Self {
            version: summary.version,
            blocking: summary.blocking,
            warnings: summary.warnings,
            published_by: summary.published_by,
            note: summary.note.clone(),
            published_at: summary.published_at,
        }
    }
}

/// The issues stored with a version, as the response carries them.
fn stored_warnings(validation: &Value) -> Vec<TemplateIssueProblem> {
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

/// The optional body of a publish.
#[derive(Default, Deserialize, ToSchema)]
pub(crate) struct TemplatePublishRequest {
    /// A note for the version history, at most 500 characters. Blank is no note.
    note: Option<String>,
}

/// The note of a publish request: an absent body or a blank note is none, and more than
/// [`TEMPLATE_NOTE_MAX_CHARS`] characters is a 400 naming `note`.
fn note_of(body: &[u8]) -> Result<Option<String>, ApiError> {
    let request: TemplatePublishRequest = if body.iter().all(u8::is_ascii_whitespace) {
        TemplatePublishRequest::default()
    } else {
        serde_json::from_slice(body)
            .map_err(|_| ApiError::bad_request("request body is not valid JSON"))?
    };
    let note = request.note.map(|note| note.trim().to_owned());
    match note {
        Some(note) if note.chars().count() > TEMPLATE_NOTE_MAX_CHARS => Err(ApiError::bad_request(
            "note is too long",
        )
        .with_errors(vec![ItemProblem {
            index: 0,
            field: "note".to_owned(),
            reason: "too_long".to_owned(),
        }])),
        Some(note) if !note.is_empty() => Ok(Some(note)),
        _ => Ok(None),
    }
}

/// The new version and the template after the publish.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplatePublishedBody {
    template: TemplateBody,
    version: TemplateVersionBody,
    /// The active flows that send this template, which now send the new version.
    used_by: Vec<TemplateUseBody>,
}

/// Validates the draft and publishes it as the next version, or changes nothing. Audited as
/// `template.published`; records a `template.publish` decision caused by a definition change.
///
/// The draft is read and validated with no lock and no connection held, because validation waits
/// on the render host for as long as its deadline. Only then does the publish lock the template,
/// and it publishes only if the draft is still the revision that was validated: a draft saved in
/// the meantime is a 409 `template_draft_changed`, and nothing is written.
pub(crate) async fn publish_template(
    state: &AppState,
    render: &RenderPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    published_by: Option<UserId>,
    note: Option<&str>,
) -> Result<(Template, TemplateVersion, Vec<TemplateUse>), ApiError> {
    let (draft, catalog, inputs) = {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        let template = zirv_flow_store::find_template(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        if template.archived_at.is_some() {
            return Err(ApiError::template_archived());
        }
        let draft = zirv_flow_store::find_template_draft(&mut *connection, workspace, id)
            .await?
            .ok_or_else(ApiError::internal)?;
        let catalog = field_catalog(
            zirv_flow_store::read_template_field_catalog(&mut connection, workspace).await?,
        );
        let doc = stored_document(&draft.document)?;
        let now = state.clock.now(&mut connection).await?;
        let inputs = resolve(
            &mut connection,
            workspace,
            &doc,
            Subject::Nobody,
            state.public_url(),
            now,
        )
        .await?;
        (draft, catalog, inputs)
    };
    let doc = stored_document(&draft.document)?;
    content_type_of(&doc).map_err(|_| ApiError::template_content_unsupported())?;
    // A template's sample render allocates like any render, so it runs in the render host, never
    // in this process.
    let mut issues: Vec<Issue> = render
        .validate_with(&doc, &catalog, &inputs.extras)
        .await
        .map_err(|failure| ApiError::render_failed(&failure))?;
    issues.extend(inputs.issues.iter().cloned());
    if issues.iter().any(Issue::is_blocking) {
        return Err(ApiError::template_invalid(issue_problems(&issues)));
    }
    let validation = serde_json::to_value(&issues).map_err(|_| ApiError::internal())?;
    let trace = TraceContext {
        trace_id: request_trace().trace_id,
        cause: Cause::definition_change(id),
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let locked = zirv_flow_store::lock_template(&mut tx, workspace, id)
        .await
        .map_err(template_error)?;
    if locked.draft().revision != draft.revision {
        // Dropping the transaction writes nothing.
        return Err(ApiError::template_draft_changed());
    }
    // The sender the document names must be a live identity on a verified domain. The check runs
    // in the publishing transaction, after the draft is pinned, so the sender that is checked is
    // the sender that is published.
    // A code document names its sender in `email`, a block document in `blocks`.
    let named_sender = doc
        .email
        .as_ref()
        .map(|email| email.sender.as_deref())
        .or_else(|| doc.blocks.as_ref().map(|blocks| blocks.sender.as_deref()));
    if let Some(named) = named_sender {
        let sender = named.and_then(|sender| sender.trim().parse::<Uuid>().ok());
        let Some(sender) = sender else {
            return Err(sender_error(SenderError::Unknown));
        };
        require_verified_sender(&mut tx, workspace, sender)
            .await
            .map_err(sender_error)?;
    }
    let (template, version) = zirv_flow_store::publish_template(
        &mut tx,
        workspace,
        &locked,
        &validation,
        published_by,
        note,
        &trace,
    )
    .await
    .map_err(template_error)?;
    // Only the assets the template itself names: a snippet and a theme are referenced by their
    // own versions.
    let mut ids: Vec<Uuid> = extras_needs(&doc)
        .assets
        .iter()
        .filter(|spelled| inputs.loaded.assets.contains_key(*spelled))
        .filter_map(|spelled| spelled.parse().ok())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let named: Vec<AssetId> = ids.into_iter().map(AssetId::from_uuid).collect();
    zirv_flow_store::record_references(
        &mut tx,
        workspace,
        AssetOwnerKind::Template,
        id,
        version.version,
        &named,
    )
    .await
    .map_err(|error| match error {
        // The asset was deleted after the check and before the lock.
        AssetReferenceError::Missing(_) => {
            ApiError::template_invalid(issue_problems(&[Issue::new(
                code::BROKEN_ASSET_REFERENCE,
                "/blocks",
            )
            .with_message("an image names an asset that was deleted")]))
        }
        AssetReferenceError::Store(error) => error.into(),
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.published",
        Some(("template", id)),
        json!({ "name": template.name, "version": version.version, "warnings": issues.len() }),
    )
    .await?;
    let used = zirv_flow_store::template_where_used(&mut *tx, workspace, id)
        .await?
        .into_iter()
        .filter(|used| used.active && used.flow_state != "archived")
        .collect();
    zirv_flow_store::commit(tx).await?;
    Ok((template, version, used))
}

/// Publishes the draft as the next immutable version. The draft is validated against the
/// workspace's field registry and event catalog and the reserved `person.email`; a blocking issue
/// is a 422 `template_invalid` that lists every issue and writes nothing. The warnings of a
/// successful publish are in the response and stored with the version. The document's sender must
/// be a live identity on a verified sending domain, or the publish is a 422 `sender_unverified`.
/// An optional body `{ "note" }` adds a note of at most 500 characters to the version.
/// Audited as `template.published`.
#[utoipa::path(
    post,
    operation_id = "publish_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}/publish",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = Option<TemplatePublishRequest>,
    responses(
        (status = 201, description = "The template and its new version", body = TemplatePublishedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 400, description = "The body is not valid JSON, or the note is longer than 500 characters (`errors` names `note`)", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is archived (`code` is `template_archived`), or its draft was saved while it was being validated (`template_draft_changed`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft has blocking issues (`code` is `template_invalid`, with `template_issues`), no registered content type handles it (`template_content_unsupported`), or its sender is unknown, archived or on a domain that is not verified (`sender_unverified`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    body: Bytes,
) -> Result<(StatusCode, Json<TemplatePublishedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let note = note_of(&body)?;
    let render = state
        .render_pool()
        .ok_or_else(ApiError::render_unavailable)?;
    let (template, version, used) = publish_template(
        &state,
        render,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        note.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(TemplatePublishedBody {
            template: TemplateBody::from(&template),
            version: TemplateVersionBody::from(&version),
            used_by: used.iter().map(TemplateUseBody::from).collect(),
        }),
    ))
}

/// The versions of a template, newest first.
#[utoipa::path(
    get,
    operation_id = "list_template_versions",
    path = "/api/v1/workspaces/{ws}/templates/{id}/versions",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    responses(
        (status = 200, description = "The versions without their documents, newest first", body = Vec<TemplateVersionSummaryBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn versions(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<Vec<TemplateVersionSummaryBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    zirv_flow_store::find_template(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let versions = zirv_flow_store::list_template_versions(&state.pool, workspace.id, id).await?;
    Ok(Json(
        versions
            .iter()
            .map(TemplateVersionSummaryBody::from)
            .collect(),
    ))
}

/// One version of a template.
#[utoipa::path(
    get,
    operation_id = "get_template_version",
    path = "/api/v1/workspaces/{ws}/templates/{id}/versions/{n}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The version", body = TemplateVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, template or version, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<TemplateVersionBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let version = zirv_flow_store::find_template_version(
        &state.pool,
        workspace.id,
        path_id(&id)?,
        path_id(&n)?,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(TemplateVersionBody::from(&version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_warnings_read_back_what_publish_stored() {
        let stored = json!([
            {"code": "missing_lang", "severity": "warning", "at": "/email/lang"},
            {"code": "insecure_link", "severity": "warning", "at": "/email/html",
             "field": "person.x", "message": "http"},
            {"code": "broken"}
        ]);

        let warnings = stored_warnings(&stored);

        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].code, "missing_lang");
        assert_eq!(warnings[1].field.as_deref(), Some("person.x"));
        assert_eq!(warnings[1].message.as_deref(), Some("http"));
        assert!(stored_warnings(&json!({})).is_empty());
    }

    #[test]
    fn issues_survive_the_round_trip_through_the_stored_json() {
        let doc = zirv_flow_templates::TemplateDocument::email_code(
            zirv_flow_templates::EmailContent::default(),
        );
        let issues =
            zirv_flow_templates::validate(&doc, &zirv_flow_templates::FieldCatalog::default());
        let stored = serde_json::to_value(&issues).unwrap();

        let back = stored_warnings(&stored);

        assert_eq!(back.len(), issues.len());
        assert_eq!(back[0].code, issues[0].code);
    }
}
