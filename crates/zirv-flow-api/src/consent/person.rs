//! A person's consent and its history.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{
    ChangeSource, Channel, ConsentStatus, Dimensions, validate_dimension_key,
    validate_dimension_value_key,
};
use zirv_flow_store::{
    ConsentChange, ConsentEntry, ConsentHistoryEntry, ConsentOutcome, PgPool, ReadOutcome,
    StoreError, dimensions_of,
};

use super::{invalid, origin};
use crate::AppState;
use crate::admin::AdminError;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The most evidence a change may carry, as serialized JSON.
const MAX_EVIDENCE_BYTES: usize = 4096;

#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentStateBody {
    /// `email`.
    #[schema(example = "email")]
    channel: String,
    /// Null for the channel as a whole.
    topic_key: Option<String>,
    topic_name: Option<String>,
    /// The workspace dimension values the state is scoped to, dimension key to value key. A
    /// dimension that is left out means all its values.
    dimensions: BTreeMap<String, String>,
    /// `subscribed`, `unsubscribed` or `pending_confirmation`. A state that is not listed is
    /// `unknown`.
    #[schema(example = "subscribed")]
    status: String,
    /// Whether the person decided: `subscribed` and `unsubscribed` are explicit.
    explicit: bool,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    changed_at: OffsetDateTime,
}

impl From<ConsentEntry> for ConsentStateBody {
    fn from(entry: ConsentEntry) -> Self {
        Self {
            channel: entry.channel,
            topic_key: entry.topic_key,
            topic_name: entry.topic_name,
            dimensions: dimensions_of(&entry.dimensions),
            status: entry.status,
            explicit: entry.explicit,
            changed_at: entry.changed_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ConsentBody {
    /// The person the states belong to: the survivor when the requested person was merged.
    person_id: Uuid,
    /// The channel-wide state first, then topics by key, and within a topic the unscoped state
    /// before the dimension-scoped ones.
    states: Vec<ConsentStateBody>,
    /// Whether a send is permitted, present when the request names a message with `channel`,
    /// `topic_key` or `dimension.<key>` query parameters.
    read: Option<ReadBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReadBody {
    /// The read rule answer: among the states that cover the message the latest change wins;
    /// with none, the topic policy default applies.
    allowed: bool,
    /// The change that decided it; null when the default applied.
    deciding_change: Option<Uuid>,
    /// The policy whose default applies when no state decides: `opt_in` or `opt_out`. A message
    /// without a topic is judged as `opt_in`.
    #[schema(example = "opt_out")]
    default_policy: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetConsentRequest {
    /// `email`.
    #[schema(example = "email")]
    channel: String,
    /// The topic to change; absent changes the channel as a whole.
    topic_key: Option<String>,
    /// Scopes the decision to dimension values, dimension key to value key, such as
    /// `{"vertical": "retail"}`. A dimension that is left out means all its values. `topic` is
    /// not a dimension key here: use `topic_key`.
    #[serde(default)]
    dimensions: BTreeMap<String, String>,
    /// `subscribed`, `unsubscribed` or `pending_confirmation`. `unknown` cannot be set.
    #[schema(example = "unsubscribed")]
    status: String,
    /// What proves the decision, such as `ip`, `user_agent`, `form` or `text`. A JSON object
    /// of at most 4 KiB as compact JSON.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    evidence: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HistoryEntryBody {
    id: Uuid,
    #[schema(example = "email")]
    channel: String,
    topic_key: Option<String>,
    /// The dimension values the change is scoped to.
    dimensions: BTreeMap<String, String>,
    /// Null on the first decision.
    from_status: Option<String>,
    to_status: String,
    /// `api`, `ui`, `import`, `merge`, `unsubscribe_link` or `ingest`.
    #[schema(example = "ui")]
    source: String,
    /// Null when `masked`: only workspace admins, the owner and `admin` keys see evidence,
    /// which can hold an IP address or user agent.
    #[schema(value_type = Option<Object>)]
    evidence: Option<Value>,
    masked: bool,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    occurred_at: OffsetDateTime,
    actor_user_id: Option<Uuid>,
}

impl HistoryEntryBody {
    fn new(entry: ConsentHistoryEntry, reveal: bool) -> Self {
        Self {
            id: entry.id,
            channel: entry.channel,
            topic_key: entry.topic_key,
            dimensions: dimensions_of(&entry.dimensions),
            from_status: entry.from_status,
            to_status: entry.to_status,
            source: entry.source,
            evidence: reveal.then_some(entry.evidence),
            masked: !reveal,
            occurred_at: entry.occurred_at,
            actor_user_id: entry.actor_user_id,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HistoryBody {
    /// Newest first.
    entries: Vec<HistoryEntryBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// A request dimension values, with every key and value checked for form. Whether they name
/// live dimensions of the workspace is the store check.
pub(super) fn check_dimensions(
    requested: &BTreeMap<String, String>,
) -> Result<Dimensions, AdminError> {
    for (key, value) in requested {
        validate_dimension_key(key).map_err(|_| {
            invalid(
                "dimensions",
                "a key must be a declared dimension, not topic; use topic_key for a topic",
            )
        })?;
        validate_dimension_value_key(value)
            .map_err(|_| invalid("dimensions", "a value must be a value key of its dimension"))?;
    }
    Ok(requested.clone())
}

/// The message a GET or a history request names, in its query: `channel`, `topic_key` and
/// `dimension.<key>=<value>` parameters.
struct MessageQuery {
    channel: Channel,
    topic_key: Option<String>,
    dimensions: BTreeMap<String, String>,
    /// Whether the query named any of them.
    named: bool,
}

fn message_query(query: &str) -> Result<MessageQuery, AdminError> {
    let mut parsed = MessageQuery {
        channel: Channel::Email,
        topic_key: None,
        dimensions: BTreeMap::new(),
        named: false,
    };
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match name.as_ref() {
            "channel" => {
                parsed.channel = value.parse()?;
                parsed.named = true;
            }
            "topic_key" => {
                parsed.topic_key = Some(value.into_owned());
                parsed.named = true;
            }
            other => {
                if let Some(key) = other.strip_prefix("dimension.") {
                    parsed.dimensions.insert(key.to_owned(), value.into_owned());
                    parsed.named = true;
                }
            }
        }
    }
    check_dimensions(&parsed.dimensions)?;
    Ok(parsed)
}

/// The status to write, refusing `unknown`, which only an absent row means.
fn writable_status(text: &str) -> Result<ConsentStatus, AdminError> {
    match text.parse()? {
        ConsentStatus::Unknown => Err(invalid("status", "unknown cannot be set")),
        status => Ok(status),
    }
}

fn check_evidence(evidence: Option<&Value>) -> Result<Value, AdminError> {
    let Some(evidence) = evidence else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    if !evidence.is_object() {
        return Err(invalid("evidence", "must be an object"));
    }
    if evidence.to_string().len() > MAX_EVIDENCE_BYTES {
        return Err(invalid("evidence", "must be at most 4 KiB"));
    }
    Ok(evidence.clone())
}

async fn states_of(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<ConsentBody>, StoreError> {
    let Some(current) = zirv_flow_store::current_person(pool, workspace, person).await? else {
        return Ok(None);
    };
    let entries = zirv_flow_store::consent_for(pool, workspace, person)
        .await?
        .unwrap_or_default();
    Ok(Some(ConsentBody {
        person_id: current,
        states: entries.into_iter().map(ConsentStateBody::from).collect(),
        read: None,
    }))
}

/// The id of the topic `key` names, if any key is given.
async fn topic_id(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: Option<&str>,
) -> Result<Option<Uuid>, AdminError> {
    let Some(key) = key else { return Ok(None) };
    zirv_flow_store::find_topic_by_key(pool, workspace, key)
        .await?
        .map(|topic| Some(topic.id))
        .ok_or_else(|| invalid("topic_key", "is not a topic of this workspace"))
}

/// Answers whether a send is permitted for the message the query names.
async fn read_of(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    message: &MessageQuery,
) -> Result<ReadBody, AdminError> {
    let topic = topic_id(pool, workspace, message.topic_key.as_deref()).await?;
    match zirv_flow_store::consent_read(
        pool,
        workspace,
        person,
        message.channel,
        topic,
        &message.dimensions,
    )
    .await?
    {
        ReadOutcome::Read { verdict, policy } => Ok(ReadBody {
            allowed: verdict.allowed,
            deciding_change: verdict.deciding_change,
            default_policy: policy.as_str().to_owned(),
        }),
        ReadOutcome::NoSuchPerson => Err(AdminError::NotFound("person")),
        ReadOutcome::NoSuchTopic => Err(invalid("topic_key", "is not a topic of this channel")),
        ReadOutcome::NoSuchDimension => Err(invalid(
            "dimensions",
            "names a dimension or value that is not live in this workspace",
        )),
    }
}

/// Records a decision for a person. `source` and `actor` say who made it. Returns the
/// person's states afterwards.
pub(crate) async fn set_person_consent(
    pool: &PgPool,
    workspace: WorkspaceId,
    person: Uuid,
    request: &SetConsentRequest,
    source: ChangeSource,
    actor: Option<Uuid>,
) -> Result<ConsentBody, AdminError> {
    let channel: Channel = request.channel.parse()?;
    let status = writable_status(&request.status)?;
    let evidence = check_evidence(request.evidence.as_ref())?;
    let dimensions = check_dimensions(&request.dimensions)?;
    let topic = topic_id(pool, workspace, request.topic_key.as_deref()).await?;

    let mut tx = zirv_flow_store::begin(pool).await?;
    let outcome = zirv_flow_store::set_consent(
        &mut tx,
        workspace,
        &ConsentChange {
            person,
            channel,
            topic,
            dimensions,
            status,
            source,
            evidence,
            occurred_at: OffsetDateTime::now_utc(),
            actor,
        },
        &crate::trace::request_trace(),
    )
    .await?;
    match outcome {
        ConsentOutcome::NoSuchPerson => return Err(AdminError::NotFound("person")),
        ConsentOutcome::NoSuchTopic => {
            return Err(invalid("topic_key", "is not a topic of this channel"));
        }
        ConsentOutcome::NoSuchDimension => {
            return Err(invalid(
                "dimensions",
                "names a dimension or value that is not live in this workspace",
            ));
        }
        ConsentOutcome::Unchanged | ConsentOutcome::Changed { .. } => {}
    }
    zirv_flow_store::commit(tx).await?;
    states_of(pool, workspace, person)
        .await?
        .ok_or(AdminError::NotFound("person"))
}

/// A person's consent: the channel-wide state and the state of each topic, with the dimension
/// values each is scoped to. Naming a message with `channel`, `topic_key` and
/// `dimension.<key>=<value>` query parameters adds `read`: whether a send to it is permitted,
/// by the latest change among the states that cover it.
#[utoipa::path(
    get,
    operation_id = "get_person_consent",
    path = "/api/v1/workspaces/{ws}/people/{id}/consent",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("channel" = Option<String>, Query, description = "The message channel; default email"),
        ("topic_key" = Option<String>, Query, description = "The message topic"),
        ("dimension.{key}" = Option<String>, Query, description = "The message value of the dimension `key`; repeat for several dimensions")
    ),
    responses(
        (status = 200, description = "The states held for the person", body = ConsentBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unknown channel, topic, dimension or value in the message", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<ConsentBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let message = message_query(query.as_deref().unwrap_or_default())?;
    let person = path_id(&id)?;
    let mut body = states_of(&state.pool, workspace.id, person)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if message.named {
        body.read = Some(read_of(&state.pool, workspace.id, person, &message).await?);
    }
    Ok(Json(body))
}

/// Records a decision: subscribe, unsubscribe or mark pending, for the channel or one
/// topic. Writing the status already held changes nothing and leaves no history. The source
/// is `ui` for a session and `api` for a key.
#[utoipa::path(
    put,
    operation_id = "set_person_consent",
    path = "/api/v1/workspaces/{ws}/people/{id}/consent",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id")
    ),
    request_body = SetConsentRequest,
    responses(
        (status = 200, description = "The states held for the person afterwards", body = ConsentBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "An unknown channel, status, topic, dimension or value, `unknown` as the status, or bad evidence", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn set(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<SetConsentRequest>,
) -> Result<Json<ConsentBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let (source, actor) = origin(&principal);
    let states = set_person_consent(
        &state.pool,
        workspace.id,
        path_id(&id)?,
        &body,
        source,
        actor,
    )
    .await?;
    Ok(Json(states))
}

/// A person's consent history, newest first: every change with its source and evidence.
/// Evidence is shown to the owner, workspace admins and `admin` keys, and masked for
/// everyone else.
#[utoipa::path(
    get,
    operation_id = "get_person_consent_history",
    path = "/api/v1/workspaces/{ws}/people/{id}/consent/history",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("limit" = Option<u32>, Query, description = "Entries per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("dimension.{key}" = Option<String>, Query, description = "Only changes scoped to at least this value of the dimension `key`; repeat for several dimensions")
    ),
    responses(
        (status = 200, description = "A page of changes", body = HistoryBody),
        (status = 400, description = "limit is out of range, or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A malformed dimension key or value in the filter", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn history(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<HistoryBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let within = message_query(query.as_deref().unwrap_or_default())?.dimensions;
    let (entries, more) = zirv_flow_store::consent_history(
        &state.pool,
        workspace.id,
        path_id(&id)?,
        limit,
        before,
        &within,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    let next_before = match entries.last() {
        Some(last) if more => Some(encode_cursor((last.occurred_at, last.id))),
        _ => None,
    };
    Ok(Json(HistoryBody {
        entries: entries
            .into_iter()
            .map(|entry| HistoryEntryBody::new(entry, reveal))
            .collect(),
        next_before,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;

    #[test]
    fn unknown_can_be_read_but_never_written() {
        assert_eq!(
            writable_status("subscribed").unwrap(),
            ConsentStatus::Subscribed
        );
        assert_eq!(
            writable_status("pending_confirmation").unwrap(),
            ConsentStatus::PendingConfirmation
        );
        assert!(matches!(
            writable_status("unknown"),
            Err(AdminError::Invalid(error)) if error.field() == "status"
        ));
        assert!(writable_status("maybe").is_err());
    }

    #[test]
    fn evidence_is_an_object_of_at_most_four_kib_and_defaults_to_empty() {
        assert_eq!(check_evidence(None).unwrap(), json!({}));
        assert_eq!(
            check_evidence(Some(&json!({"ip": "203.0.113.9"}))).unwrap(),
            json!({"ip": "203.0.113.9"})
        );
        assert!(check_evidence(Some(&json!(["x"]))).is_err());
        assert!(check_evidence(Some(&json!("x"))).is_err());
        assert!(check_evidence(Some(&json!({"text": "x".repeat(4096)}))).is_err());
        assert!(check_evidence(Some(&json!({"text": "x".repeat(4000)}))).is_ok());
    }

    #[test]
    fn a_request_needs_a_channel_and_a_status_and_takes_an_optional_topic_and_evidence() {
        let request: SetConsentRequest =
            serde_json::from_value(json!({"channel": "email", "status": "subscribed"})).unwrap();

        assert!(request.topic_key.is_none() && request.evidence.is_none());
        assert!(serde_json::from_value::<SetConsentRequest>(json!({"channel": "email"})).is_err());
    }

    #[test]
    fn dimension_values_are_checked_for_form_and_topic_is_not_a_dimension_key() {
        let map = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };

        assert_eq!(
            check_dimensions(&map(&[("vertical", "retail")]))
                .unwrap()
                .len(),
            1
        );
        assert!(check_dimensions(&map(&[])).unwrap().is_empty());
        for bad in [
            map(&[("topic", "news")]),
            map(&[("Vertical", "retail")]),
            map(&[("vertical", "Retail Shops")]),
            map(&[("vertical", "")]),
        ] {
            assert!(
                matches!(check_dimensions(&bad), Err(AdminError::Invalid(error)) if error.field() == "dimensions"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_message_is_named_by_channel_topic_and_dimension_parameters() {
        let none = message_query("limit=5&before=x").unwrap();
        let some =
            message_query("topic_key=news&dimension.vertical=retail&dimension.brand=acme").unwrap();

        assert!(!none.named && none.dimensions.is_empty());
        assert!(some.named);
        assert_eq!(some.topic_key.as_deref(), Some("news"));
        assert_eq!(some.dimensions.len(), 2);
        assert!(message_query("channel=sms").is_err());
        assert!(message_query("dimension.topic=news").is_err());
        assert!(message_query("dimension.vertical=A%20B").is_err());
    }

    #[test]
    fn stored_dimensions_read_back_as_a_map_of_strings() {
        let stored = json!({"vertical": "retail", "odd": 3});

        assert_eq!(
            dimensions_of(&stored).into_iter().collect::<Vec<_>>(),
            [("vertical".to_owned(), "retail".to_owned())]
        );
        assert!(dimensions_of(&json!([])).is_empty());
    }

    #[test]
    fn the_history_page_defaults_to_fifty_and_refuses_bad_values() {
        assert_eq!(parse_page("").unwrap(), (50, None));
        assert_eq!(parse_page("limit=200").unwrap().0, 200);
        for bad in ["limit=0", "limit=201", "limit=x", "before=%21"] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_history_cursor_is_the_one_the_list_hands_out() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_001_000).unwrap();
        let id = Uuid::now_v7();

        let (_, before) = parse_page(&format!("before={}", encode_cursor((at, id)))).unwrap();

        assert_eq!(before, Some((at, id)));
    }
}
