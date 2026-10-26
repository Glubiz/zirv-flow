//! The themes of a workspace: create, read, list, rename, archive and save the draft. Each write
//! is audited with ids and names, never with token values.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_store::{NewTheme, PgPool, Theme, ThemeDraft, ThemePatch};
use zirv_flow_templates::ThemeTokens;

use super::{name_taken, parse_tokens, stored_tokens, theme_error};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::templates::crud::check_name;

#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeBody {
    id: Uuid,
    #[schema(example = "Brand")]
    name: String,
    /// The newest published version; null before the first publish.
    current_version: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    archived_at: Option<OffsetDateTime>,
    /// Whether emails that follow the default theme use this one.
    is_default: bool,
    /// The user who created it; null when an API key did.
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Theme> for ThemeBody {
    fn from(theme: &Theme) -> Self {
        Self {
            id: theme.id,
            name: theme.name.clone(),
            current_version: theme.current_version,
            archived_at: theme.archived_at,
            is_default: theme.is_default,
            created_by: theme.created_by,
            created_at: theme.created_at,
            updated_at: theme.updated_at,
        }
    }
}

/// The one editable copy of a theme's tokens.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeDraftBody {
    /// The tokens. They may fail the publish checks; only a publish validates them fully.
    #[schema(value_type = Object)]
    tokens: Value,
    /// How many times the draft has been saved; the first save is 1. Send it back as `revision`
    /// to save only if nobody saved in between.
    revision: i32,
    updated_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&ThemeDraft> for ThemeDraftBody {
    fn from(draft: &ThemeDraft) -> Self {
        Self {
            tokens: draft.tokens.clone(),
            revision: draft.revision,
            updated_by: draft.updated_by,
            updated_at: draft.updated_at,
        }
    }
}

/// A theme with its draft.
#[derive(Serialize, ToSchema)]
pub(crate) struct ThemeDetailBody {
    #[serde(flatten)]
    theme: ThemeBody,
    draft: ThemeDraftBody,
}

impl ThemeDetailBody {
    pub(super) fn new(theme: &Theme, draft: &ThemeDraft) -> Self {
        Self {
            theme: ThemeBody::from(theme),
            draft: ThemeDraftBody::from(draft),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateThemeRequest {
    /// 1 to 100 characters after trimming; unique among the workspace's live themes, ignoring
    /// case.
    #[schema(example = "Brand")]
    name: String,
    /// The first draft: any of the tokens, the rest take their defaults. Absent: all defaults.
    #[schema(value_type = Option<Object>)]
    tokens: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateThemeRequest {
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveThemeDraftRequest {
    /// The tokens: any of them, the rest take their defaults.
    #[schema(value_type = Object)]
    tokens: Value,
    /// The revision the caller read. When it is not the draft's current revision the save is a
    /// 409 `theme_revision_mismatch` and nothing is written. Absent: save regardless.
    revision: Option<i32>,
}

/// The tokens as they are saved: parsed, so a bad shape is refused, and written out in full.
fn checked_tokens(value: &Value) -> Result<Value, ApiError> {
    stored_tokens(&parse_tokens(value)?)
}

/// Creates a theme with its first draft and audits it as `theme.created`.
pub(crate) async fn create_theme(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: CreateThemeRequest,
) -> Result<(Theme, ThemeDraft), ApiError> {
    let name = check_name(&request.name)?.to_owned();
    let tokens = match &request.tokens {
        Some(value) => checked_tokens(value)?,
        None => stored_tokens(&ThemeTokens::default())?,
    };
    let new = NewTheme {
        id: Uuid::now_v7(),
        name,
        tokens,
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let (theme, draft) = zirv_flow_store::create_theme(&mut tx, workspace, &new, created_by)
        .await
        .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "theme.created",
        Some(("theme", theme.id)),
        json!({ "name": theme.name }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((theme, draft))
}

/// Renames a theme. Not audited as its own action: the name is in every later audit row.
async fn rename_theme(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
    request: UpdateThemeRequest,
) -> Result<(Theme, ThemeDraft), ApiError> {
    let patch = ThemePatch {
        name: Some(check_name(&request.name)?.to_owned()),
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let theme = zirv_flow_store::update_theme(&mut tx, workspace, id, &patch)
        .await
        .map_err(theme_error)?;
    let draft = zirv_flow_store::find_theme_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    zirv_flow_store::commit(tx).await?;
    Ok((theme, draft))
}

/// Replaces a theme's draft and audits it as `theme.draft_saved`.
pub(crate) async fn save_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    updated_by: Option<UserId>,
    request: SaveThemeDraftRequest,
) -> Result<(Theme, ThemeDraft), ApiError> {
    let tokens = checked_tokens(&request.tokens)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_theme(&mut tx, workspace, id)
        .await
        .map_err(theme_error)?;
    let draft = zirv_flow_store::save_theme_draft(
        &mut tx,
        workspace,
        id,
        &tokens,
        request.revision,
        updated_by,
    )
    .await
    .map_err(theme_error)?;
    let theme = locked.theme().clone();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "theme.draft_saved",
        Some(("theme", id)),
        json!({ "name": theme.name, "revision": draft.revision }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((theme, draft))
}

/// Archives a theme and audits it as `theme.archived`.
pub(crate) async fn archive_theme(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(Theme, ThemeDraft), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let theme = zirv_flow_store::archive_theme(&mut tx, workspace, id)
        .await
        .map_err(theme_error)?;
    let draft = zirv_flow_store::find_theme_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "theme.archived",
        Some(("theme", id)),
        json!({ "name": theme.name, "version": theme.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((theme, draft))
}

/// The workspace's live themes by name; archived ones with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_themes",
    path = "/api/v1/workspaces/{ws}/themes",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived themes; default false")
    ),
    responses(
        (status = 200, description = "The themes", body = Vec<ThemeBody>),
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
) -> Result<Json<Vec<ThemeBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let themes = zirv_flow_store::list_themes(&state.pool, workspace.id, archived).await?;
    Ok(Json(themes.iter().map(ThemeBody::from).collect()))
}

/// Creates a theme with its first draft. Audited as `theme.created`.
#[utoipa::path(
    post,
    operation_id = "create_theme",
    path = "/api/v1/workspaces/{ws}/themes",
    tag = "themes",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateThemeRequest,
    responses(
        (status = 201, description = "The new theme with its draft", body = ThemeDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live theme has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, or tokens that are not a theme (`theme_malformed`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateThemeRequest>,
) -> Result<(StatusCode, Json<ThemeDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (theme, draft) = create_theme(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(ThemeDetailBody::new(&theme, &draft)),
    ))
}

/// One theme with its draft.
#[utoipa::path(
    get,
    operation_id = "get_theme",
    path = "/api/v1/workspaces/{ws}/themes/{id}",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    responses(
        (status = 200, description = "The theme", body = ThemeDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ThemeDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let theme = zirv_flow_store::find_theme(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let draft = zirv_flow_store::find_theme_draft(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(ThemeDetailBody::new(&theme, &draft)))
}

/// Renames a theme.
#[utoipa::path(
    put,
    operation_id = "update_theme",
    path = "/api/v1/workspaces/{ws}/themes/{id}",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    request_body = UpdateThemeRequest,
    responses(
        (status = 200, description = "The updated theme", body = ThemeDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live theme has that name, or the theme is archived (`theme_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateThemeRequest>,
) -> Result<Json<ThemeDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (theme, draft) = rename_theme(&state.pool, workspace.id, path_id(&id)?, body).await?;
    Ok(Json(ThemeDetailBody::new(&theme, &draft)))
}

/// Archives a theme. Nothing is deleted: the draft and the versions stay readable, and a template
/// that names the theme still renders with it. Refused while the theme is the workspace default.
/// Audited as `theme.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_theme",
    path = "/api/v1/workspaces/{ws}/themes/{id}",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    responses(
        (status = 200, description = "The archived theme", body = ThemeDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The theme is already archived (`theme_archived`), or is the workspace default (`theme_is_default`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn archive(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ThemeDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (theme, draft) = archive_theme(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(Json(ThemeDetailBody::new(&theme, &draft)))
}

/// Replaces the theme's draft. The draft is not validated beyond being a set of known tokens: a
/// theme is edited in steps, and the publish checks it. Audited as `theme.draft_saved`.
#[utoipa::path(
    put,
    operation_id = "save_theme_draft",
    path = "/api/v1/workspaces/{ws}/themes/{id}/draft",
    tag = "themes",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Theme id")
    ),
    request_body = SaveThemeDraftRequest,
    responses(
        (status = 200, description = "The theme with its saved draft", body = ThemeDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or theme, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The theme is archived (`theme_archived`), or the draft is not at the `revision` named (`theme_revision_mismatch`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Tokens that are not a theme (`theme_malformed`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SaveThemeDraftRequest>,
) -> Result<Json<ThemeDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (theme, draft) = save_draft(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        body,
    )
    .await?;
    Ok(Json(ThemeDetailBody::new(&theme, &draft)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_request_needs_a_name_and_the_tokens_are_optional() {
        let request: CreateThemeRequest = serde_json::from_str(r#"{"name":"A"}"#).unwrap();

        assert!(request.tokens.is_none());
        assert!(serde_json::from_str::<CreateThemeRequest>(r#"{"tokens":{}}"#).is_err());
    }

    #[test]
    fn a_draft_save_may_name_the_revision_it_read() {
        let with: SaveThemeDraftRequest =
            serde_json::from_str(r#"{"tokens":{},"revision":3}"#).unwrap();
        let without: SaveThemeDraftRequest = serde_json::from_str(r#"{"tokens":{}}"#).unwrap();

        assert_eq!((with.revision, without.revision), (Some(3), None));
    }

    #[test]
    fn saved_tokens_are_written_out_in_full_and_a_bad_shape_is_refused() {
        let saved = checked_tokens(&json!({"link": "#0000ee"})).unwrap();

        assert_eq!(saved["link"], "#0000ee");
        assert_eq!(saved["content_width"], 600);
        assert!(checked_tokens(&json!({"link": 5})).is_err());
    }

    #[test]
    fn the_draft_body_keeps_the_tokens_as_stored() {
        let draft = ThemeDraft {
            theme_id: Uuid::nil(),
            tokens: json!({"primary": "#123456"}),
            revision: 2,
            updated_by: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let body = ThemeDraftBody::from(&draft);

        assert_eq!((body.revision, body.tokens), (2, draft.tokens));
    }
}
