//! The templates of a workspace: create, read, list, change, archive and save the draft. Each
//! write is audited with ids and names, never with the document's content.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{UserId, ValidationError, WorkspaceId};
use zirv_flow_store::{
    NewTemplate, PgPool, Template, TemplateDraft, TemplateListQuery, TemplatePatch, TemplateSort,
    TemplateStatus, TemplateSummary, TemplateTheme, TemplateThemeFilter,
    create_template as store_create,
};
use zirv_flow_templates::{
    Category, Channel, EmailContent, Mode, Syntax, TemplateDocument, blocks_to_mjml,
    content_type_of, migrate_to_current,
};

use super::publish::{publish_inputs, publish_issues};
use super::{
    category_name, channel_name, check_matches, issue_problems, name_taken, parse_document,
    skeleton, stored_document, template_error,
};
use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::consent::bool_param;
use crate::json::ApiJson;
use crate::problem::{ApiError, FlowRefProblem, Problem};

const MAX_NAME_CHARS: usize = 100;

/// How long a draft save waits for its validation before it answers without checks, so a busy
/// render pool never holds the editor.
const CHECKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct TemplateBody {
    id: Uuid,
    #[schema(example = "Order shipped")]
    name: String,
    /// `email` or `webhook`; fixed when the template is made.
    channel: String,
    /// `marketing` or `transactional`; fixed when the template is made.
    category: String,
    /// `inherit` (the default) follows the workspace's UTM setting; `off` never adds UTM
    /// parameters to this template's links.
    utm: String,
    /// The sender identity the draft's document names (`email.sender`), when it is an identity of
    /// this workspace. Read-only: it follows the draft, which is where the sender is set.
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
            utm: template.utm.clone(),
            sender_identity_id: template.sender_identity_id,
            current_version: template.current_version,
            archived_at: template.archived_at,
            created_by: template.created_by,
            created_at: template.created_at,
            updated_at: template.updated_at,
        }
    }
}

/// The counts of the issues the draft's last validation found.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateChecksBody {
    blocking: i32,
    warnings: i32,
}

/// A theme choice that names no theme row.
#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TemplateThemeChoice {
    /// Follows the workspace default theme.
    Default,
    /// Follows no theme: code mode, a webhook, or a block email that names none.
    None,
}

/// A theme a block email names.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateNamedTheme {
    id: Uuid,
    name: String,
}

/// The theme the draft follows: `default`, `none`, or the theme it names.
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub(crate) enum TemplateThemeBody {
    Choice(TemplateThemeChoice),
    Named(TemplateNamedTheme),
}

/// A template in the library list: the template and the facts the library shows.
#[derive(Serialize, ToSchema)]
pub(crate) struct TemplateListItemBody {
    #[serde(flatten)]
    template: TemplateBody,
    /// The draft differs from the current version; false before the first publish.
    draft_changed: bool,
    /// The draft's last validation; null when it has not been checked.
    checks: Option<TemplateChecksBody>,
    /// How many active flows send the template.
    active_flows: i64,
    theme: TemplateThemeBody,
    /// The email of the member who saved the draft last; null for an API key or a removed member.
    edited_by_email: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    edited_at: OffsetDateTime,
}

impl TemplateListItemBody {
    fn new(summary: &TemplateSummary, active_flows: i64) -> Self {
        Self {
            template: TemplateBody::from(&summary.template),
            draft_changed: summary.draft_changed,
            checks: summary
                .checks
                .map(|(blocking, warnings)| TemplateChecksBody { blocking, warnings }),
            active_flows,
            theme: match &summary.theme {
                TemplateTheme::Default => TemplateThemeBody::Choice(TemplateThemeChoice::Default),
                TemplateTheme::None => TemplateThemeBody::Choice(TemplateThemeChoice::None),
                TemplateTheme::Named { id, name } => TemplateThemeBody::Named(TemplateNamedTheme {
                    id: *id,
                    name: name.clone(),
                }),
            },
            edited_by_email: summary.edited_by_email.clone(),
            edited_at: summary.edited_at,
        }
    }
}

/// The one editable copy of a template's content.
#[derive(Debug, Serialize, ToSchema)]
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
        // A draft is served at the current schema, whatever schema it was stored at.
        let document =
            migrate_to_current(draft.document.clone()).unwrap_or_else(|_| draft.document.clone());
        Self {
            document,
            content_type,
            revision: draft.revision,
            updated_by: draft.updated_by,
            updated_at: draft.updated_at,
        }
    }
}

/// A template with its draft.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct TemplateDetailBody {
    #[serde(flatten)]
    template: TemplateBody,
    draft: TemplateDraftBody,
}

impl TemplateDetailBody {
    pub(crate) fn new(template: &Template, draft: &TemplateDraft) -> Self {
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
    /// The first draft. Its channel and category must be the template's. Absent: an empty
    /// code-mode document.
    #[schema(value_type = Option<Object>)]
    document: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateTemplateRequest {
    name: Option<String>,
    /// `inherit` or `off`: whether the workspace's automatic UTM parameters apply to this
    /// template.
    utm: Option<TemplateUtmRequest>,
}

/// The per-template override of the workspace UTM setting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TemplateUtmRequest {
    Inherit,
    Off,
}

impl TemplateUtmRequest {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Off => "off",
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SaveTemplateDraftRequest {
    /// The content document.
    #[schema(value_type = Object)]
    document: Value,
    /// The draft revision the caller read. When given and the draft has been saved since, the save
    /// is a 409 `template_draft_revision_mismatch` and writes nothing. Absent saves unconditionally.
    revision: Option<i32>,
}

fn invalid(field: &'static str, message: &'static str) -> ApiError {
    ValidationError::new(field, message).into()
}

pub(crate) fn check_name(name: &str) -> Result<&str, ApiError> {
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
///
/// The result is the document at the current schema, which is what a draft is stored as: an
/// older document is migrated on the way in.
pub(crate) fn check_draft(template: &Template, document: &Value) -> Result<Value, ApiError> {
    let doc = parse_document(document)?;
    check_matches(template, &doc)?;
    content_type_of(&doc).map_err(|_| ApiError::template_content_unsupported())?;
    migrate_to_current(document.clone()).map_err(|_| ApiError::template_unsupported_schema())
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
    let mut new = NewTemplate {
        id: Uuid::now_v7(),
        name,
        channel: request.channel.as_str().to_owned(),
        category: category.as_str().to_owned(),
        document,
    };
    // The same checks a draft save makes, against the template this will be.
    let prospective = Template {
        id: new.id,
        name: new.name.clone(),
        channel: new.channel.clone(),
        category: new.category.clone(),
        utm: "inherit".into(),
        sender_identity_id: None,
        current_version: None,
        archived_at: None,
        created_by: created_by.map(|user| user.as_uuid()),
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };
    new.document = check_draft(&prospective, &new.document)?;
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
    if request.utm.is_some() {
        fields.push("utm");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }
    let patch = TemplatePatch {
        name,
        utm: request.utm.map(|utm| utm.as_str().to_owned()),
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
        json!({ "name": template.name, "utm": template.utm, "fields": fields }),
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
    if request
        .revision
        .is_some_and(|revision| revision != locked.draft().revision)
    {
        return Err(ApiError::template_draft_revision_mismatch(
            locked.draft().revision,
        ));
    }
    let document = check_draft(locked.template(), &request.document)?;
    let draft = zirv_flow_store::save_template_draft(&mut tx, workspace, id, &document, updated_by)
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

#[derive(Deserialize, ToSchema)]
pub(crate) struct ConvertTemplateDraftRequest {
    /// The draft revision the caller read. When given and the draft has been saved since, the
    /// conversion is a 409 `template_draft_revision_mismatch` and writes nothing. Absent converts
    /// unconditionally.
    revision: Option<i32>,
}

/// Replaces a block-mode draft with an MJML code-mode draft holding the converted source, and
/// audits it as `template.draft_converted`. One way: nothing turns MJML back into blocks. Only
/// the draft changes; published versions stay as they were.
pub(crate) async fn convert_draft_to_mjml(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    updated_by: Option<UserId>,
    revision: Option<i32>,
) -> Result<(Template, TemplateDraft), ApiError> {
    let mut tx = zirv_flow_store::begin(pool).await?;
    let locked = zirv_flow_store::lock_template(&mut tx, workspace, id)
        .await
        .map_err(template_error)?;
    if revision.is_some_and(|revision| revision != locked.draft().revision) {
        return Err(ApiError::template_draft_revision_mismatch(
            locked.draft().revision,
        ));
    }
    let current = zirv_flow_store::find_template_draft(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let doc = stored_document(&current.document)?;
    let content = match (&doc.mode, &doc.blocks) {
        (Mode::Blocks, Some(content)) if !doc.is_ambiguous() => content,
        _ => {
            return Err(ApiError::template_malformed(
                "only a draft in blocks mode can be converted to MJML",
            ));
        }
    };
    let converted = blocks_to_mjml(content)
        .map_err(|error| ApiError::template_invalid(issue_problems(&error.issues)))?;
    let code = TemplateDocument::email_code(EmailContent {
        subject: content.subject.clone(),
        sender: content.sender.clone(),
        lang: content.lang.clone(),
        category: content.category,
        syntax: Syntax::Mjml,
        html: converted.mjml,
        text: converted.text,
    });
    let document = serde_json::to_value(&code).map_err(|_| ApiError::internal())?;
    let document = check_draft(locked.template(), &document)?;
    let draft = zirv_flow_store::save_template_draft(&mut tx, workspace, id, &document, updated_by)
        .await
        .map_err(template_error)?;
    let template = locked.template().clone();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "template.draft_converted",
        Some(("template", id)),
        json!({ "name": template.name, "revision": draft.revision, "to": "mjml" }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((template, draft))
}

/// Validates the saved draft once, exactly as a publish does (the same catalog, render inputs and checks), and stores the issue counts on it. This runs
/// after the save committed and never fails it: a document the renderer cannot read, a render pool
/// that is down or a validation that fails leaves the checks empty and logs a warning with ids only.
pub(crate) async fn record_checks(state: &AppState, workspace: WorkspaceId, draft: &TemplateDraft) {
    let Some(pool) = state.render_pool() else {
        tracing::warn!(template = %draft.template_id, "draft checks skipped: no render pool");
        return;
    };
    let Ok(doc) = stored_document(&draft.document) else {
        tracing::warn!(template = %draft.template_id, "draft checks skipped: unreadable document");
        return;
    };
    let found = tokio::time::timeout(CHECKS_TIMEOUT, async {
        let mut connection = state
            .pool
            .acquire()
            .await
            .map_err(|_| ApiError::internal())?;
        let (catalog, inputs) = publish_inputs(state, &mut connection, workspace, &doc).await?;
        drop(connection);
        publish_issues(pool, &doc, &catalog, &inputs).await
    })
    .await;
    let issues = match found {
        Ok(Ok(issues)) => issues,
        Ok(Err(error)) => {
            tracing::warn!(template = %draft.template_id, "draft checks skipped: {}", error.detail());
            return;
        }
        Err(_) => {
            tracing::warn!(template = %draft.template_id, "draft checks skipped: timed out");
            return;
        }
    };
    let blocking = issues.iter().filter(|issue| issue.is_blocking()).count();
    let counts = (
        i32::try_from(blocking).unwrap_or(i32::MAX),
        i32::try_from(issues.len() - blocking).unwrap_or(i32::MAX),
    );
    if let Err(error) = zirv_flow_store::set_template_draft_checks(
        &state.pool,
        workspace,
        draft.template_id,
        draft.revision,
        counts.0,
        counts.1,
    )
    .await
    {
        tracing::warn!(template = %draft.template_id, "draft checks not stored: {error}");
    }
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
    // Nothing is written while an active flow still sends the template.
    let blocking = zirv_flow_store::active_published_flows(&mut *tx, workspace, id).await?;
    if !blocking.is_empty() {
        return Err(ApiError::template_in_use(
            blocking
                .into_iter()
                .map(|used| FlowRefProblem {
                    flow_id: used.flow_id,
                    flow_name: used.flow_name,
                })
                .collect(),
        ));
    }
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

/// The `GET /templates` parameters other than `archived`, checked. An unknown value is a 400 that
/// names the parameter.
fn list_query(query: &str) -> Result<TemplateListQuery, ApiError> {
    let mut parsed = TemplateListQuery {
        include_archived: bool_param(query, "archived")?,
        ..TemplateListQuery::default()
    };
    let bad = |name: &str| ApiError::bad_request(format!("{name} is not a known value"));
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "q" => {
                let term = value.trim();
                parsed.search = (!term.is_empty()).then(|| term.to_owned());
            }
            "channel" => {
                parsed.channel = match value.as_ref() {
                    "email" | "webhook" => Some(value.into_owned()),
                    _ => return Err(bad("channel")),
                };
            }
            "status" => {
                parsed.status = Some(match value.as_ref() {
                    "live" => TemplateStatus::Live,
                    "changes" => TemplateStatus::Changes,
                    "draft" => TemplateStatus::Draft,
                    "archived" => TemplateStatus::Archived,
                    _ => return Err(bad("status")),
                });
            }
            "theme" => {
                parsed.theme = Some(match value.as_ref() {
                    "default" => TemplateThemeFilter::Default,
                    "none" => TemplateThemeFilter::None,
                    other => TemplateThemeFilter::Id(other.parse().map_err(|_| bad("theme"))?),
                });
            }
            "sort" => {
                parsed.sort = match value.as_ref() {
                    "edited" => TemplateSort::Edited,
                    "name" => TemplateSort::Name,
                    "most_used" => TemplateSort::MostUsed,
                    _ => return Err(bad("sort")),
                };
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// The workspace's templates for the library, newest edit first; archived ones with
/// `archived=true` or `status=archived`.
#[utoipa::path(
    get,
    operation_id = "list_templates",
    path = "/api/v1/workspaces/{ws}/templates",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived templates; default false"),
        ("q" = Option<String>, Query, description = "A case-insensitive substring of the name, the draft's subject or a merge field the draft uses"),
        ("channel" = Option<String>, Query, description = "`email` or `webhook`"),
        ("status" = Option<String>, Query, description = "`live` (published, draft unchanged), `changes` (published, draft changed), `draft` (never published) or `archived`; without it, everything except archived templates"),
        ("theme" = Option<String>, Query, description = "A theme id, `default` or `none`"),
        ("sort" = Option<String>, Query, description = "`edited` (the default), `name` or `most_used` (non-test messages in the last 30 days)")
    ),
    responses(
        (status = 200, description = "The templates", body = Vec<TemplateListItemBody>),
        (status = 400, description = "archived is not true or false, or another parameter has an unknown value", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<Vec<TemplateListItemBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let query = list_query(query.as_deref().unwrap_or_default())?;
    let templates = zirv_flow_store::list_templates(&state.pool, workspace.id, &query).await?;
    let counts = zirv_flow_store::template_active_flow_counts(&state.pool, workspace.id).await?;
    Ok(Json(
        templates
            .iter()
            .map(|summary| {
                let active = counts.get(&summary.template.id).copied().unwrap_or(0);
                TemplateListItemBody::new(summary, active)
            })
            .collect(),
    ))
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
        (status = 422, description = "An invalid name, a document that is not a template document (`template_malformed`), has an unsupported `schema` (`template_unsupported_schema`), an unknown block (`template_unknown_block`) or field (`template_unknown_field`), is for another channel or category (`template_mismatch`) or has no registered content type (`template_content_unsupported`)", body = Problem, content_type = "application/problem+json")
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

/// Changes a template's name. The channel and the category are fixed when the
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
        (status = 409, description = "The template is already archived (`code` is `template_archived`), or an active flow still sends it (`code` is `template_in_use`, with the flows in `flows`)", body = Problem, content_type = "application/problem+json")
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
/// steps, and the publish checks it. After the save commits the draft is validated once and its
/// issue counts stored (`checks` in the template list); a validation that cannot run leaves them
/// empty and never fails the save. Audited as `template.draft_saved`.
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
        (status = 409, description = "The template is archived (`code` is `template_archived`), or `revision` is not the draft's current revision (`template_draft_revision_mismatch`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A document that is not a template document (`template_malformed`), has an unsupported `schema` (`template_unsupported_schema`), an unknown block (`template_unknown_block`) or field (`template_unknown_field`), is for another channel or category (`template_mismatch`) or has no registered content type (`template_content_unsupported`)", body = Problem, content_type = "application/problem+json")
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
    record_checks(&state, workspace.id, &draft).await;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

/// Converts a block-mode draft to MJML code: the draft becomes a code-mode MJML document holding
/// the converted source, merge tags kept, and the new draft is returned. One way. Published
/// versions are untouched. Audited as `template.draft_converted`.
#[utoipa::path(
    post,
    operation_id = "convert_template_draft_to_mjml",
    path = "/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml",
    tag = "templates",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Template id")
    ),
    request_body(content = Option<ConvertTemplateDraftRequest>, description = "Optional: an empty body or `null` converts unconditionally; otherwise an object naming the draft revision the caller read"),
    responses(
        (status = 200, description = "The template with its converted draft", body = TemplateDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or template, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The template is archived (`code` is `template_archived`), or `revision` is not the draft's current revision (`template_draft_revision_mismatch`, with `current_revision`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft is not in blocks mode (`template_malformed`), or the block document has blocking issues (`template_invalid`, with the issues)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn convert_draft(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    body: Option<ApiJson<ConvertTemplateDraftRequest>>,
) -> Result<Json<TemplateDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (template, draft) = convert_draft_to_mjml(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        principal.user_id(),
        body.and_then(|ApiJson(body)| body.revision),
    )
    .await?;
    record_checks(&state, workspace.id, &draft).await;
    Ok(Json(TemplateDetailBody::new(&template, &draft)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_parameters_are_read_and_an_unknown_value_names_its_parameter() {
        let query = list_query("q=%20sale%20&channel=email&status=changes&sort=most_used").unwrap();
        assert_eq!(query.search.as_deref(), Some("sale"));
        assert_eq!(query.channel.as_deref(), Some("email"));
        assert_eq!(query.status, Some(TemplateStatus::Changes));
        assert_eq!(query.sort, TemplateSort::MostUsed);
        assert!(!query.include_archived);
        let id = Uuid::now_v7();
        assert_eq!(
            list_query(&format!("theme={id}")).unwrap().theme,
            Some(TemplateThemeFilter::Id(id))
        );
        assert_eq!(
            list_query("theme=none").unwrap().theme,
            Some(TemplateThemeFilter::None)
        );
        assert!(list_query("archived=true").unwrap().include_archived);
        assert!(list_query("q=").unwrap().search.is_none());
        for name in ["channel", "status", "theme", "sort"] {
            let error = list_query(&format!("{name}=nope")).unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST);
            assert!(error.detail().contains(name), "{name}");
        }
    }

    #[test]
    fn a_list_item_serializes_the_theme_as_a_word_or_an_object() {
        let summary = |theme| TemplateSummary {
            template: Template {
                id: Uuid::nil(),
                name: "T".into(),
                channel: "email".into(),
                category: "marketing".into(),
                utm: "inherit".into(),
                sender_identity_id: None,
                current_version: None,
                archived_at: None,
                created_by: None,
                created_at: OffsetDateTime::UNIX_EPOCH,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            draft_changed: false,
            checks: Some((1, 2)),
            theme,
            edited_by_email: None,
            edited_at: OffsetDateTime::UNIX_EPOCH,
        };
        let json = |theme| serde_json::to_value(TemplateListItemBody::new(&summary(theme), 3));

        let default = json(TemplateTheme::Default).unwrap();
        let named = json(TemplateTheme::Named {
            id: Uuid::nil(),
            name: "Brand".into(),
        })
        .unwrap();

        assert_eq!(default["theme"], "default");
        assert_eq!(named["theme"]["name"], "Brand");
        assert_eq!(default["checks"], json!({"blocking": 1, "warnings": 2}));
        assert_eq!(default["active_flows"], 3);
        assert_eq!(default["name"], "T");
    }

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
    fn a_document_with_two_contents_is_malformed_whatever_its_categories_say() {
        let template = Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: "email".into(),
            category: "marketing".into(),
            utm: "inherit".into(),
            sender_identity_id: None,
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        // A dummy marketing `email` makes the category check pass; the renderer would read the
        // transactional `blocks` and send without unsubscribe headers.
        let both = json!({"schema": 1, "channel": "email", "mode": "blocks",
            "email": {"subject": "x", "html": "", "category": "marketing"},
            "blocks": {"subject": "Hi", "category": "transactional"}});
        assert_eq!(
            check_draft(&template, &both).unwrap_err().code(),
            Some("template_malformed")
        );
        let mut code_pair = both.clone();
        code_pair["mode"] = json!("code");
        assert_eq!(
            check_draft(&template, &code_pair).unwrap_err().code(),
            Some("template_malformed")
        );
        let with_webhook = json!({"schema": 1, "channel": "email", "mode": "code",
            "email": {"subject": "x", "html": ""}, "webhook": {"body": "{}"}});
        assert_eq!(
            check_draft(&template, &with_webhook).unwrap_err().code(),
            Some("template_malformed")
        );
    }

    #[test]
    fn typed_parse_errors_have_their_own_codes() {
        let template = Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: "email".into(),
            category: "marketing".into(),
            utm: "inherit".into(),
            sender_identity_id: None,
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let blocks = |change: &dyn Fn(&mut Value)| {
            let mut doc = json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
                "subject": "Hi", "sections": [{"id": "s", "columns": [[
                    {"type": "divider", "id": "d"}]]}]}});
            change(&mut doc);
            check_draft(&template, &doc)
                .map(|_| ())
                .map_err(|error| error.code())
        };
        assert_eq!(blocks(&|_| {}), Ok(()));
        assert_eq!(
            blocks(&|doc| doc["schema"] = json!(3)),
            Err(Some("template_unsupported_schema"))
        );
        assert_eq!(
            blocks(
                &|doc| doc["blocks"]["sections"][0]["columns"][0][0]["type"] = json!("carousel")
            ),
            Err(Some("template_unknown_block"))
        );
        assert_eq!(
            blocks(&|doc| doc["blocks"]["sections"][0]["columns"][0][0]["style"] = json!("x")),
            Err(Some("template_unknown_field"))
        );
        assert_eq!(
            blocks(&|doc| doc["palette"] = json!("dark")),
            Err(Some("template_unknown_field"))
        );
        // A bad enum value is not an unknown block.
        assert_eq!(
            blocks(&|doc| doc["channel"] = json!("fax")),
            Err(Some("template_malformed"))
        );
    }

    #[test]
    fn a_block_document_keeps_its_templates_category() {
        let template = |category: &str| Template {
            id: Uuid::nil(),
            name: "T".into(),
            channel: "email".into(),
            category: category.into(),
            utm: "inherit".into(),
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
            utm: "inherit".into(),
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
