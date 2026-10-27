//! The snippets of a workspace: create, read, list, rename, archive and save the draft. Each write
//! is audited with ids and names, never with the content.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{NewSnippet, PgPool, Snippet, SnippetDraft, SnippetPatch};

use super::{name_taken, parse_content, snippet_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::templates::crud::check_name;

#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetBody {
    id: Uuid,
    #[schema(example = "Free shipping note")]
    name: String,
    /// The newest published version; null before the first publish.
    current_version: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    archived_at: Option<OffsetDateTime>,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Snippet> for SnippetBody {
    fn from(snippet: &Snippet) -> Self {
        Self {
            id: snippet.id,
            name: snippet.name.clone(),
            current_version: snippet.current_version,
            archived_at: snippet.archived_at,
            created_by: snippet.created_by,
            created_at: snippet.created_at,
            updated_at: snippet.updated_at,
        }
    }
}

/// A snippet in the list, with how many templates hold it.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetListItemBody {
    #[serde(flatten)]
    snippet: SnippetBody,
    /// How many templates, not archived, hold a block for it in their draft or current version.
    used_by_templates: i64,
}

/// The one editable copy of a snippet's content.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetDraftBody {
    /// The content: `{"blocks": [...]}`. It may be incomplete; only a publish validates it.
    #[schema(value_type = Object)]
    content: Value,
    /// How many times the draft has been saved; the first save is 1. Send it back as `revision`
    /// to save only if nobody saved in between.
    revision: i32,
    updated_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&SnippetDraft> for SnippetDraftBody {
    fn from(draft: &SnippetDraft) -> Self {
        Self {
            content: draft.content.clone(),
            revision: draft.revision,
            updated_by: draft.updated_by,
            updated_at: draft.updated_at,
        }
    }
}

/// A snippet with its draft.
#[derive(Serialize, ToSchema)]
pub(crate) struct SnippetDetailBody {
    #[serde(flatten)]
    snippet: SnippetBody,
    draft: SnippetDraftBody,
}

impl SnippetDetailBody {
    fn new(snippet: &Snippet, draft: &SnippetDraft) -> Self {
        Self {
            snippet: SnippetBody::from(snippet),
            draft: SnippetDraftBody::from(draft),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateSnippetRequest {
    /// 1 to 100 characters after trimming; unique among the workspace's live snippets, ignoring
    /// case.
    #[schema(example = "Free shipping note")]
    name: String,
    /// The first draft. Absent: an empty block list.
    #[schema(value_type = Option<Object>)]
    content: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateSnippetRequest {
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveSnippetDraftRequest {
    /// The content: `{"blocks": [...]}`.
    #[schema(value_type = Object)]
    content: Value,
    /// The revision the caller read. When it is not the draft's current revision the save is a
    /// 409 `snippet_revision_mismatch` and nothing is written. Absent: save regardless.
    revision: Option<i32>,
}

/// The content a new snippet starts with.
fn empty_content() -> Value {
    json!({ "blocks": [] })
}

/// Creates a snippet with its first draft and audits it as `snippet.created`.
pub(crate) async fn create_snippet(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: CreateSnippetRequest,
) -> Result<(Snippet, SnippetDraft), ApiError> {
    let name = check_name(&request.name)?.to_owned();
    let content = request.content.unwrap_or_else(empty_content);
    parse_content(&content)?;
    let new = NewSnippet {
        id: Uuid::now_v7(),
        name,
        content,
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let (snippet, draft) = zirv_flow_store::create_snippet(&mut tx, workspace, &new, created_by)
        .await
        .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "snippet.created",
        Some(("snippet", snippet.id)),
        json!({ "name": crate::admin::audit_text(&snippet.name) }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((snippet, draft))
}

/// Renames a snippet. Not audited as its own action: a rename is a draft-level change that the
/// next publish carries, and the name is in every later audit row.
async fn rename_snippet(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: UpdateSnippetRequest,
) -> Result<(Snippet, SnippetDraft), ApiError> {
    let patch = SnippetPatch {
        name: Some(check_name(&request.name)?.to_owned()),
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let snippet = zirv_flow_store::update_snippet(&mut tx, workspace, id, &patch)
        .await
        .map_err(snippet_error)?;
    let draft = zirv_flow_store::find_snippet_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "snippet.updated",
        Some(("snippet", id)),
        json!({ "name": crate::admin::audit_text(&snippet.name) }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((snippet, draft))
}

/// Replaces a snippet's draft and audits it as `snippet.draft_saved`.
pub(crate) async fn save_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    updated_by: Option<UserId>,
    request: SaveSnippetDraftRequest,
) -> Result<(Snippet, SnippetDraft), ApiError> {
    parse_content(&request.content)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_snippet(&mut tx, workspace, id)
        .await
        .map_err(snippet_error)?;
    let draft = zirv_flow_store::save_snippet_draft(
        &mut tx,
        workspace,
        id,
        &request.content,
        request.revision,
        updated_by,
    )
    .await
    .map_err(snippet_error)?;
    let snippet = locked.snippet().clone();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "snippet.draft_saved",
        Some(("snippet", id)),
        json!({ "name": crate::admin::audit_text(&snippet.name), "revision": draft.revision }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((snippet, draft))
}

/// Archives a snippet and audits it as `snippet.archived`.
pub(crate) async fn archive_snippet(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(Snippet, SnippetDraft), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let snippet = zirv_flow_store::archive_snippet(&mut tx, workspace, id)
        .await
        .map_err(snippet_error)?;
    let draft = zirv_flow_store::find_snippet_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "snippet.archived",
        Some(("snippet", id)),
        json!({ "name": crate::admin::audit_text(&snippet.name), "version": snippet.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((snippet, draft))
}

/// The workspace's live snippets by name; archived ones with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_snippets",
    path = "/api/v1/workspaces/{ws}/snippets",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived snippets; default false")
    ),
    responses(
        (status = 200, description = "The snippets", body = Vec<SnippetListItemBody>),
        (status = 400, description = "archived is not true or false", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<Vec<SnippetListItemBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let snippets = zirv_flow_store::list_snippets(&state.pool, workspace.id, archived).await?;
    let counts = zirv_flow_store::snippet_template_counts(&state.pool, workspace.id).await?;
    Ok(Json(
        snippets
            .iter()
            .map(|snippet| SnippetListItemBody {
                snippet: SnippetBody::from(snippet),
                used_by_templates: counts.get(&snippet.id).copied().unwrap_or(0),
            })
            .collect(),
    ))
}

/// Creates a snippet with its first draft. Audited as `snippet.created`.
#[utoipa::path(
    post,
    operation_id = "create_snippet",
    path = "/api/v1/workspaces/{ws}/snippets",
    tag = "snippets",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateSnippetRequest,
    responses(
        (status = 201, description = "The new snippet with its draft", body = SnippetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live snippet has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, or content that is not a block list (`snippet_malformed`, `template_unknown_block`, `template_unknown_field`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateSnippetRequest>,
) -> Result<(StatusCode, Json<SnippetDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (snippet, draft) = create_snippet(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(SnippetDetailBody::new(&snippet, &draft)),
    ))
}

/// One snippet with its draft.
#[utoipa::path(
    get,
    operation_id = "get_snippet",
    path = "/api/v1/workspaces/{ws}/snippets/{id}",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    responses(
        (status = 200, description = "The snippet", body = SnippetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SnippetDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let snippet = zirv_flow_store::find_snippet(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let draft = zirv_flow_store::find_snippet_draft(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(SnippetDetailBody::new(&snippet, &draft)))
}

/// Renames a snippet.
#[utoipa::path(
    put,
    operation_id = "update_snippet",
    path = "/api/v1/workspaces/{ws}/snippets/{id}",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    request_body = UpdateSnippetRequest,
    responses(
        (status = 200, description = "The updated snippet", body = SnippetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live snippet has that name, or the snippet is archived (`snippet_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateSnippetRequest>,
) -> Result<Json<SnippetDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (snippet, draft) = rename_snippet(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        body,
    )
    .await?;
    Ok(Json(SnippetDetailBody::new(&snippet, &draft)))
}

/// Archives a snippet. Nothing is deleted: the draft and the versions stay readable. Refused
/// while the draft of a template holds a block for it, or the published version of a theme uses it
/// as its footer. Audited as `snippet.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_snippet",
    path = "/api/v1/workspaces/{ws}/snippets/{id}",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    responses(
        (status = 200, description = "The archived snippet", body = SnippetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The snippet is already archived (`snippet_archived`), or template drafts or published theme footers use it (`snippet_in_use`, with `snippet_templates` and `snippet_themes`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn archive(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<SnippetDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (snippet, draft) = archive_snippet(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(Json(SnippetDetailBody::new(&snippet, &draft)))
}

/// Replaces the snippet's draft. The draft is not validated beyond being a block list of known
/// blocks: a snippet is edited in steps, and the publish checks it. Audited as
/// `snippet.draft_saved`.
#[utoipa::path(
    put,
    operation_id = "save_snippet_draft",
    path = "/api/v1/workspaces/{ws}/snippets/{id}/draft",
    tag = "snippets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Snippet id")
    ),
    request_body = SaveSnippetDraftRequest,
    responses(
        (status = 200, description = "The snippet with its saved draft", body = SnippetDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or snippet, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The snippet is archived (`snippet_archived`), or the draft is not at the `revision` named (`snippet_revision_mismatch`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Content that is not a block list (`snippet_malformed`, `template_unknown_block`, `template_unknown_field`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SaveSnippetDraftRequest>,
) -> Result<Json<SnippetDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let (snippet, draft) = save_draft(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        body,
    )
    .await?;
    Ok(Json(SnippetDetailBody::new(&snippet, &draft)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_request_needs_a_name_and_the_content_is_optional() {
        let request: CreateSnippetRequest = serde_json::from_str(r#"{"name":"A"}"#).unwrap();

        assert!(request.content.is_none());
        assert!(serde_json::from_str::<CreateSnippetRequest>(r#"{"content":{}}"#).is_err());
    }

    #[test]
    fn a_draft_save_may_name_the_revision_it_read() {
        let with: SaveSnippetDraftRequest =
            serde_json::from_str(r#"{"content":{"blocks":[]},"revision":3}"#).unwrap();
        let without: SaveSnippetDraftRequest =
            serde_json::from_str(r#"{"content":{"blocks":[]}}"#).unwrap();

        assert_eq!((with.revision, without.revision), (Some(3), None));
    }

    #[test]
    fn a_new_snippet_starts_with_an_empty_block_list_that_parses() {
        assert!(parse_content(&empty_content()).is_ok());
    }

    #[test]
    fn the_draft_body_keeps_the_content_as_stored() {
        let draft = SnippetDraft {
            snippet_id: Uuid::nil(),
            content: json!({"blocks": [{"type": "divider", "id": "d"}]}),
            revision: 2,
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let body = SnippetDraftBody::from(&draft);

        assert_eq!((body.revision, body.content), (2, draft.content));
    }
}
