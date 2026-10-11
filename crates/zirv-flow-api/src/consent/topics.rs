//! The topic catalog of a workspace.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{Channel, TopicPolicy, validate_topic_key};
use zirv_flow_store::{NewTopic, PgPool, Topic, TopicUpdate};

use super::invalid;
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 100;
const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Serialize, ToSchema)]
pub(crate) struct TopicBody {
    id: Uuid,
    /// Lowercase letters, digits, underscores and hyphens; unique in the workspace.
    #[schema(example = "newsletter")]
    key: String,
    name: String,
    description: String,
    /// `email`.
    #[schema(example = "email")]
    channel: String,
    /// `opt_in`: allowed only when subscribed. `opt_out`: allowed unless unsubscribed.
    #[schema(example = "opt_out")]
    policy: String,
    archived: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    archived_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<&Topic> for TopicBody {
    fn from(topic: &Topic) -> Self {
        Self {
            id: topic.id,
            key: topic.key.clone(),
            name: topic.name.clone(),
            description: topic.description.clone(),
            channel: topic.channel.as_str().to_owned(),
            policy: topic.policy.as_str().to_owned(),
            archived: topic.archived_at.is_some(),
            archived_at: topic.archived_at,
            created_at: topic.created_at,
            updated_at: topic.updated_at,
        }
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateTopicRequest {
    /// 1 to 64 lowercase letters, digits, underscores or hyphens. It cannot be changed.
    #[schema(example = "newsletter")]
    key: String,
    /// 1 to 100 characters.
    name: String,
    /// At most 500 characters.
    #[serde(default)]
    description: String,
    /// `email`. It cannot be changed.
    #[schema(example = "email")]
    channel: String,
    /// `opt_in` or `opt_out`.
    #[schema(example = "opt_out")]
    policy: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateTopicRequest {
    name: Option<String>,
    description: Option<String>,
    policy: Option<String>,
    archived: Option<bool>,
}

fn check_name(name: &str) -> Result<(), AdminError> {
    let length = name.chars().count();
    if name.trim().is_empty() || length > MAX_NAME_CHARS {
        Err(invalid("name", "must be 1 to 100 characters"))
    } else {
        Ok(())
    }
}

fn check_description(description: &str) -> Result<(), AdminError> {
    if description.chars().count() <= MAX_DESCRIPTION_CHARS {
        Ok(())
    } else {
        Err(invalid("description", "must be at most 500 characters"))
    }
}

/// Creates a topic and audits it. Refused when the key exists, archived or not.
pub(crate) async fn create_topic(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    request: &CreateTopicRequest,
) -> Result<Topic, AdminError> {
    validate_topic_key(&request.key)?;
    check_name(&request.name)?;
    check_description(&request.description)?;
    let channel: Channel = request.channel.parse()?;
    let policy: TopicPolicy = request.policy.parse()?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let topic = zirv_flow_store::create_topic(
        &mut *tx,
        workspace,
        &NewTopic {
            key: request.key.clone(),
            name: request.name.clone(),
            description: request.description.clone(),
            channel,
            policy,
        },
    )
    .await
    .map_err(|error| {
        if error.is_conflict() {
            AdminError::Conflict("a topic with that key already exists")
        } else {
            error.into()
        }
    })?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "topic.created",
        Some(("topic", topic.id)),
        json!({ "key": topic.key }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(topic)
}

/// Changes the mutable fields of a topic and audits the names of the changed fields.
pub(crate) async fn update_topic(
    pool: &PgPool,
    context: &Context,
    workspace: WorkspaceId,
    id: Uuid,
    request: &UpdateTopicRequest,
) -> Result<Topic, AdminError> {
    let mut fields: Vec<&'static str> = Vec::new();
    let mut update = TopicUpdate::default();
    if let Some(name) = &request.name {
        check_name(name)?;
        update.name = Some(name.clone());
        fields.push("name");
    }
    if let Some(description) = &request.description {
        check_description(description)?;
        update.description = Some(description.clone());
        fields.push("description");
    }
    if let Some(policy) = &request.policy {
        update.policy = Some(policy.parse()?);
        fields.push("policy");
    }
    if let Some(archived) = request.archived {
        update.archived = Some(archived);
        fields.push("archived");
    }
    if fields.is_empty() {
        return Err(invalid("body", "must change at least one field"));
    }

    let mut tx = zirv_flow_store::begin(pool).await?;
    let topic = zirv_flow_store::update_topic(&mut *tx, workspace, id, &update)
        .await?
        .ok_or(AdminError::NotFound("topic"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "topic.updated",
        Some(("topic", topic.id)),
        json!({ "key": topic.key, "fields": fields }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(topic)
}

/// Whether the query asks for archived topics too: `archived=true`.
fn wants_archived(query: &str) -> Result<bool, ApiError> {
    let mut archived = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "archived" {
            archived = match value.as_ref() {
                "true" => true,
                "false" => false,
                _ => return Err(ApiError::bad_request("archived must be true or false")),
            };
        }
    }
    Ok(archived)
}

/// The workspace's topics ordered by key. Archived ones are included with `archived=true`.
#[utoipa::path(
    get,
    operation_id = "list_topics",
    path = "/api/v1/workspaces/{ws}/topics",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("archived" = Option<bool>, Query, description = "Include archived topics; default false")
    ),
    responses(
        (status = 200, description = "The topics, ordered by key", body = Vec<TopicBody>),
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
) -> Result<Json<Vec<TopicBody>>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let archived = wants_archived(query.as_deref().unwrap_or_default())?;
    let topics = zirv_flow_store::list_topics(&state.pool, workspace.id, archived).await?;
    Ok(Json(topics.iter().map(TopicBody::from).collect()))
}

/// Creates a topic.
#[utoipa::path(
    post,
    operation_id = "create_topic",
    path = "/api/v1/workspaces/{ws}/topics",
    tag = "consent",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = CreateTopicRequest,
    responses(
        (status = 201, description = "The new topic", body = TopicBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The key already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid key, name, description, channel or policy", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<CreateTopicRequest>,
) -> Result<(StatusCode, Json<TopicBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let topic = create_topic(&state.pool, &principal.context(), workspace.id, &body).await?;
    Ok((StatusCode::CREATED, Json(TopicBody::from(&topic))))
}

/// Changes a topic's name, description or policy, or archives it. The key and the channel
/// never change.
#[utoipa::path(
    patch,
    operation_id = "update_topic",
    path = "/api/v1/workspaces/{ws}/topics/{id}",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Topic id")
    ),
    request_body = UpdateTopicRequest,
    responses(
        (status = 200, description = "The updated topic", body = TopicBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or topic, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An invalid field, or no field at all", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn update(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateTopicRequest>,
) -> Result<Json<TopicBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let topic = update_topic(
        &state.pool,
        &principal.context(),
        workspace.id,
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok(Json(TopicBody::from(&topic)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_to_a_hundred_characters_and_not_blank() {
        assert!(check_name("News").is_ok());
        assert!(check_name(&"n".repeat(100)).is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("   ").is_err());
        assert!(check_name(&"n".repeat(101)).is_err());
    }

    #[test]
    fn a_description_is_at_most_five_hundred_characters() {
        assert!(check_description(&"d".repeat(500)).is_ok());
        assert!(check_description(&"d".repeat(501)).is_err());
    }

    #[test]
    fn the_archived_query_is_true_false_or_absent() {
        assert!(!wants_archived("").unwrap());
        assert!(wants_archived("archived=true").unwrap());
        assert!(!wants_archived("archived=false").unwrap());
        assert!(wants_archived("archived=yes").is_err());
    }

    #[test]
    fn the_body_marks_an_archived_topic_and_shows_rfc3339_times() {
        let topic = Topic {
            id: Uuid::now_v7(),
            workspace_id: WorkspaceId::new(),
            key: "news".into(),
            name: "News".into(),
            description: String::new(),
            channel: Channel::Email,
            policy: TopicPolicy::OptIn,
            archived_at: Some(OffsetDateTime::UNIX_EPOCH),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        let json = serde_json::to_value(TopicBody::from(&topic)).unwrap();

        assert_eq!(json["archived"], true);
        assert_eq!(json["archived_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["policy"], "opt_in");
        assert_eq!(json["channel"], "email");
    }

    #[tokio::test]
    async fn an_update_with_no_field_is_refused_before_the_database_is_asked() {
        let request: UpdateTopicRequest = serde_json::from_value(json!({})).unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();

        let refused = update_topic(
            &pool,
            &Context::system(),
            WorkspaceId::new(),
            Uuid::now_v7(),
            &request,
        )
        .await;

        assert!(matches!(refused, Err(AdminError::Invalid(error)) if error.field() == "body"));
    }
}
