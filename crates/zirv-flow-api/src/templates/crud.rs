//! The templates of a workspace: create, read, list, change, archive and save the draft. Each
//! write is audited with ids and names, never with the document's content.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{UserId, ValidationError, WorkspaceId};
use zirv_flow_store::{
    NewTemplate, PgPool, Template, TemplateDraft, TemplatePatch, create_template as store_create,
};
use zirv_flow_templates::{Category, Channel, content_type_of};

use super::{
    category_name, channel_name, check_matches, name_taken, parse_document, skeleton,
    stored_document, template_error,
};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 100;

/// Tells an absent field from a `null` one: absent is `None`, `null` is `Some(None)`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// The channel of a template. Only the two with a registered content type can be made.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TemplateChannelRequest {
    Email,
    Webhook,
}

/// Whether a message needs the unsubscribe link and headers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TemplateCategoryRequest {
    Marketing,
    Transactional,
}

impl TemplateChannelRequest {
    fn as_str(self) -> &'static str {
        channel_name(self.channel())
    }

    fn channel(self) -> Channel {
        match self {
            Self::Email => Channel::Email,
            Self::Webhook => Channel::Webhook,
        }
    }
}

impl TemplateCategoryRequest {
    fn as_str(self) -> &'static str {
        category_name(self.category())
    }

    fn category(self) -> Category {
        match self {
            Self::Marketing => Category::Marketing,
            Self::Transactional => Category::Transactional,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateBody {
    id: Uuid,
    #[schema(example = "Order shipped")]
    name: String,
    /// `email` or `webhook`; fixed when the template is made.
    channel: String,
    /// `marketing` or `transactional`; fixed when the template is made.
    category: String,
    /// The sender identity of an email template.
    sender_identity_id: Option<Uuid>,
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

impl From<&Template> for TemplateBody {
    fn from(template: &Template) -> Self {
        Self {
            id: template.id,
            name: template.name.clone(),
            channel: template.channel.clone(),
            category: template.category.clone(),
            sender_identity_id: template.sender_identity_id,
            current_version: template.current_version,
            archived_at: template.archived_at,
            created_by: template.created_by,
            created_at: template.created_at,
            updated_at: template.updated_at,
        }
    }
}

/// The one editable copy of a template's content.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateDraftBody {
    /// The content document. It may be incomplete; only a publish validates it.
    #[schema(value_type = Object)]
    document: Value,
    /// The registered content type that owns the document (`html`, `webhook`); null when none does.
    content_type: Option<&'static str>,
    /// How many times the draft has been saved; the first save is 1.
    revision: i32,
    updated_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&TemplateDraft> for TemplateDraftBody {
    fn from(draft: &TemplateDraft) -> Self {
        let content_type = stored_document(&draft.document)
            .ok()
            .and_then(|doc| content_type_of(&doc).ok())
            .map(|content| content.id());
        Self {
            document: draft.document.clone(),
            content_type,
            revision: draft.revision,
            updated_by: draft.updated_by,
            updated_at: draft.updated_at,
        }
    }
}

/// A template with its draft.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateDetailBody {
    #[serde(flatten)]
    template: TemplateBody,
    draft: TemplateDraftBody,
}

impl TemplateDetailBody {
    fn new(template: &Template, draft: &TemplateDraft) -> Self {
        Self {
            template: TemplateBody::from(template),
            draft: TemplateDraftBody::from(draft),
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateTemplateRequest {
    /// 1 to 100 characters after trimming; unique among the workspace's live templates, ignoring
    /// case.
    #[schema(example = "Order shipped")]
    name: String,
    channel: TemplateChannelRequest,
    /// Default `marketing` for email and `transactional` for a webhook.
    category: Option<TemplateCategoryRequest>,
    sender_identity_id: Option<Uuid>,
    /// The first draft. Its channel and category must be the template's. Absent: an empty
    /// code-mode document.
    #[schema(value_type = Option<Object>)]
    document: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateTemplateRequest {
    name: Option<String>,
    /// `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Uuid>, nullable)]
    sender_identity_id: Option<Option<Uuid>>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveTemplateDraftRequest {
    /// The content document.
    #[schema(value_type = Object)]
    document: Value,
}

fn invalid(field: &'static str, message: &'static str) -> ApiError {
    ValidationError::new(field, message).into()
}

fn check_name(name: &str) -> Result<&str, ApiError> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 || length > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        Err(invalid(
            "name",
            "must be 1 to 100 characters without control characters",
        ))
    } else {
        Ok(name)
    }
}

/// A document that may be saved as `template`'s draft: a template document of the template's
/// channel and category, of a content type that is registered. Whether it is complete is for the
/// publish to say.
fn check_draft(template: &Template, document: &Value) -> Result<(), ApiError> {
    let doc = parse_document(document)?;
    check_matches(template, &doc)?;
    content_type_of(&doc).map_err(|_| ApiError::template_content_unsupported())?;
    Ok(())
}

/// Creates a template with its first draft and audits it as `template.created`.
pub(crate) async fn create_template(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    created_by: Option<UserId>,
    request: CreateTemplateRequest,
) -> Result<(Template, TemplateDraft), ApiError> {
    let name = check_name(&request.name)?.to_owned();
    let category = request.category.unwrap_or(match request.channel {
        TemplateChannelRequest::Email => TemplateCategoryRequest::Marketing,
        TemplateChannelRequest::Webhook => TemplateCategoryRequest::Transactional,
    });
    let document = match request.document {
        Some(document) => document,
        None => serde_json::to_value(skeleton(request.channel.channel(), category.category()))
            .map_err(|_| ApiError::internal())?,
    };
    let new = NewTemplate {
        id: Uuid::now_v7(),
        name,
        channel: request.channel.as_str().to_owned(),
        category: category.as_str().to_owned(),
        sender_identity_id: request.sender_identity_id,
        document,
    };
    // The same checks a draft save makes, against the template this will be.
    let prospective = Template {
        id: new.id,
        name: new.name.clone(),
        channel: new.channel.clone(),
        category: new.category.clone(),
        sender_identity_id: new.sender_identity_id,
        current_version: None,
        archived_at: None,
        created_by: created_by.map(|user| user.as_uuid()),
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };
    check_draft(&prospective, &new.document)?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let (template, draft) = store_create(&mut tx, workspace, &new, created_by)
        .await
        .map_err(name_taken)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.created",
        Some(("template", template.id)),
        json!({ "name": template.name, "channel": template.channel, "category": template.category }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((template, draft))
}

/// Changes a template's settings and audits the changed fields as `template.updated`.
pub(crate) async fn update_template(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: UpdateTemplateRequest,
) -> Result<(Template, TemplateDraft), ApiError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let name = request
        .name
        .as_deref()
        .map(check_name)
        .transpose()?
        .map(str::to_owned);
    if name.is_some() {
        fields.push("name");
    }
    if request.sender_identity_id.is_some() {
        fields.push("sender_identity_id");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let patch = TemplatePatch {
        name,
        sender_identity_id: request.sender_identity_id,
    };
    let mut tx = zirv_flow_store::begin(pool).await?;
    let template = zirv_flow_store::update_template(&mut tx, workspace, id, &patch)
        .await
        .map_err(template_error)?;
    let draft = zirv_flow_store::find_template_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.updated",
        Some(("template", template.id)),
        json!({ "name": template.name, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((template, draft))
}

/// Replaces a template's draft and audits it as `template.draft_saved`.
pub(crate) async fn save_draft(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    updated_by: Option<UserId>,
    request: SaveTemplateDraftRequest,
) -> Result<(Template, TemplateDraft), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_template(&mut tx, workspace, id)
        .await
        .map_err(template_error)?;
    check_draft(locked.template(), &request.document)?;
    let draft =
        zirv_flow_store::save_template_draft(&mut tx, workspace, id, &request.document, updated_by)
            .await
            .map_err(template_error)?;
    let template = locked.template().clone();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.draft_saved",
        Some(("template", id)),
        json!({ "name": template.name, "revision": draft.revision }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((template, draft))
}

/// Archives a template and audits it as `template.archived`.
pub(crate) async fn archive_template(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(Template, TemplateDraft), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let template = zirv_flow_store::archive_template(&mut tx, workspace, id)
        .await
        .map_err(template_error)?;
    let draft = zirv_flow_store::find_template_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.archived",
        Some(("template", id)),
        json!({ "name": template.name, "version": template.current_version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((template, draft))
}

/// The workspace's live templates by name; archived ones with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_templates",
    path = "/api/v1/workspaces/{ws}/templates",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived templates; default false")
    ),
    responses(
        (status = 200, description = "The templates", body = Vec<TemplateBody>),
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
) -> Result<Json<Vec<TemplateBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = bool_param(query.as_deref().unwrap_or_default(), "archived")?;
    let templates = zirv_flow_store::list_templates(&state.pool, workspace.id, archived).await?;
    Ok(Json(templates.iter().map(TemplateBody::from).collect()))
}

/// Creates a template with its first draft. Audited as `template.created`.
#[utoipa::path(
    post,
    operation_id = "create_template",
    path = "/api/v1/workspaces/{ws}/templates",
    tag = "templates",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateTemplateRequest,
    responses(
        (status = 201, description = "The new template with its draft", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A live template has that name", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, a document that is not a template document (`template_malformed`), is for another channel or category (`template_mismatch`) or has no registered content type (`template_content_unsupported`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateTemplateRequest>,
) -> Result<(StatusCode, Json<TemplateDetailBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (template, draft) = create_template(
        &state.pool,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        body,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(TemplateDetailBody::new(&template, &draft)),
    ))
}

/// One template with its draft.
#[utoipa::path(
    get,
    operation_id = "get_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    responses(
        (status = 200, description = "The template", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<TemplateDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let id = path_id(&id)?;
    let template = zirv_flow_store::find_template(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let draft = zirv_flow_store::find_template_draft(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

/// Changes a template's name or sender identity. The channel and the category are fixed when the
/// template is made. Audited as `template.updated`.
#[utoipa::path(
    put,
    operation_id = "update_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = UpdateTemplateRequest,
    responses(
        (status = 200, description = "The updated template", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "Another live template has that name, or the template is archived (`code` is `template_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid name, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateTemplateRequest>,
) -> Result<Json<TemplateDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (template, draft) = update_template(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        body,
    )
    .await?;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

/// Archives a template. Nothing is deleted: the draft and the versions stay readable. Audited as
/// `template.archived`.
#[utoipa::path(
    delete,
    operation_id = "archive_template",
    path = "/api/v1/workspaces/{ws}/templates/{id}",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    responses(
        (status = 200, description = "The archived template", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is already archived (`code` is `template_archived`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn archive(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<TemplateDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (template, draft) = archive_template(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
    )
    .await?;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

/// Replaces the template's draft. The draft is not validated beyond being a template document of
/// the template's channel and category with a registered content type: a template is edited in
/// steps, and the publish checks it. Audited as `template.draft_saved`.
#[utoipa::path(
    put,
    operation_id = "save_template_draft",
    path = "/api/v1/workspaces/{ws}/templates/{id}/draft",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body = SaveTemplateDraftRequest,
    responses(
        (status = 200, description = "The template with its saved draft", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is archived (`code` is `template_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A document that is not a template document (`template_malformed`), is for another channel or category (`template_mismatch`) or has no registered content type (`template_content_unsupported`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SaveTemplateDraftRequest>,
) -> Result<Json<TemplateDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (template, draft) = save_draft(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        body,
    )
    .await?;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_bounded_to_100_characters() {
        assert_eq!(check_name("  Order ").unwrap(), "Order");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(100)).is_ok());
        assert!(check_name(&"x".repeat(101)).is_err());
        assert!(check_name("two\nlines").is_err());
    }

    #[test]
    fn a_create_request_needs_a_name_and_a_channel_and_defaults_the_rest() {
        let request: CreateTemplateRequest =
            serde_json::from_str(r#"{"name":"A","channel":"email"}"#).unwrap();

        assert_eq!(request.channel, TemplateChannelRequest::Email);
        assert!(request.category.is_none() && request.document.is_none());
        assert!(serde_json::from_str::<CreateTemplateRequest>(r#"{"name":"A"}"#).is_err());
        assert!(
            serde_json::from_str::<CreateTemplateRequest>(r#"{"name":"A","channel":"sms"}"#)
                .is_err()
        );
    }

    #[test]
    fn an_update_tells_a_cleared_sender_from_an_absent_one() {
        let absent: UpdateTemplateRequest = serde_json::from_str(r#"{"name":"A"}"#).unwrap();
        let cleared: UpdateTemplateRequest =
            serde_json::from_str(r#"{"sender_identity_id":null}"#).unwrap();
        let set: UpdateTemplateRequest =
            serde_json::from_str(&format!(r#"{{"sender_identity_id":"{}"}}"#, Uuid::nil()))
                .unwrap();

        assert_eq!(absent.sender_identity_id, None);
        assert_eq!(cleared.sender_identity_id, Some(None));
        assert_eq!(set.sender_identity_id, Some(Some(Uuid::nil())));
    }

    #[test]
    fn a_block_document_keeps_its_templates_category() {
        let template = |category: &str| Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: "email".into(),
            category: category.into(),
            sender_identity_id: None,
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let blocks = |category: &str| {
            json!({"schema": 1, "channel": "email", "mode": "blocks",
                   "blocks": {"subject": "Hi", "category": category}})
        };

        assert!(check_draft(&template("marketing"), &blocks("marketing")).is_ok());
        assert!(check_draft(&template("transactional"), &blocks("transactional")).is_ok());
        assert_eq!(
            check_draft(&template("marketing"), &blocks("transactional"))
                .unwrap_err()
                .code(),
            Some("template_mismatch")
        );
        assert_eq!(
            check_draft(&template("transactional"), &blocks("marketing"))
                .unwrap_err()
                .code(),
            Some("template_mismatch")
        );
    }

    #[test]
    fn a_draft_of_another_channel_or_an_unsupported_mode_is_refused() {
        let template = |channel: &str| Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: channel.into(),
            category: "marketing".into(),
            sender_identity_id: None,
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let email = json!({"schema": 1, "channel": "email", "mode": "code",
                           "email": {"subject": "", "html": ""}});
        let blocks = json!({"schema": 1, "channel": "email", "mode": "blocks"});

        assert!(check_draft(&template("email"), &email).is_ok());
        assert_eq!(
            check_draft(&template("webhook"), &email)
                .unwrap_err()
                .code(),
            Some("template_mismatch")
        );
        assert_eq!(
            check_draft(&template("email"), &blocks).unwrap_err().code(),
            Some("template_content_unsupported")
        );
        assert_eq!(
            check_draft(&template("email"), &json!(3))
                .unwrap_err()
                .code(),
            Some("template_malformed")
        );
    }
}
