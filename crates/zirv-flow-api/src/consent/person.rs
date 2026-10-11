//! A person's consent and its history.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
use zirv_flow_store::{
    ConsentChange, ConsentEntry, ConsentHistoryEntry, ConsentOutcome, PgPool, StoreError,
};

use super::{invalid, origin};
use crate::AppState;
use crate::admin::AdminError;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, MAX_LIMIT, decode_cursor, encode_cursor};
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
    /// The channel-wide state first, then topics by key.
    states: Vec<ConsentStateBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetConsentRequest {
    /// `email`.
    #[schema(example = "email")]
    channel: String,
    /// The topic to change; absent changes the channel as a whole.
    topic_key: Option<String>,
    /// `subscribed`, `unsubscribed` or `pending_confirmation`. `unknown` cannot be set.
    #[schema(example = "unsubscribed")]
    status: String,
    /// What proves the decision, such as `ip`, `user_agent`, `form` or `text`. A JSON object
    /// of at most 4 KiB.
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
    }))
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
    let topic = match &request.topic_key {
        Some(key) => Some(
            zirv_flow_store::find_topic_by_key(pool, workspace, key)
                .await?
                .ok_or_else(|| invalid("topic_key", "is not a topic of this workspace"))?
                .id,
        ),
        None => None,
    };

    let mut tx = zirv_flow_store::begin(pool).await?;
    let outcome = zirv_flow_store::set_consent(
        &mut tx,
        workspace,
        &ConsentChange {
            person,
            channel,
            topic,
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
        ConsentOutcome::Unchanged | ConsentOutcome::Changed { .. } => {}
    }
    zirv_flow_store::commit(tx).await?;
    states_of(pool, workspace, person)
        .await?
        .ok_or(AdminError::NotFound("person"))
}

/// A person's consent: the channel-wide state and the state of each topic.
#[utoipa::path(
    get,
    operation_id = "get_person_consent",
    path = "/api/v1/workspaces/{ws}/people/{id}/consent",
    tag = "consent",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id")
    ),
    responses(
        (status = 200, description = "The states held for the person", body = ConsentBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<ConsentBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let body = states_of(&state.pool, workspace.id, path_id(&id)?)
        .await?
        .ok_or_else(ApiError::not_found)?;
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
        (status = 422, description = "An unknown channel, status or topic, `unknown` as the status, or bad evidence", body = Problem, content_type = "application/problem+json")
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

fn parse_history_query(query: &str) -> Result<(u32, Option<(OffsetDateTime, Uuid)>), ApiError> {
    let (mut limit, mut before) = (DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                limit = value
                    .parse::<u32>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| {
                        ApiError::bad_request(format!("limit must be from 1 to {MAX_LIMIT}"))
                    })?;
            }
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            _ => {}
        }
    }
    Ok((limit, before))
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
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of changes", body = HistoryBody),
        (status = 400, description = "limit is out of range, or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json")
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
    let (limit, before) = parse_history_query(query.as_deref().unwrap_or_default())?;
    let (entries, more) =
        zirv_flow_store::consent_history(&state.pool, workspace.id, path_id(&id)?, limit, before)
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
    fn the_history_page_defaults_to_fifty_and_refuses_bad_values() {
        assert_eq!(parse_history_query("").unwrap(), (50, None));
        assert_eq!(parse_history_query("limit=200").unwrap().0, 200);
        for bad in ["limit=0", "limit=201", "limit=x", "before=%21"] {
            assert_eq!(
                parse_history_query(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_history_cursor_is_the_one_the_list_hands_out() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_000_001_000).unwrap();
        let id = Uuid::now_v7();

        let (_, before) =
            parse_history_query(&format!("before={}", encode_cursor((at, id)))).unwrap();

        assert_eq!(before, Some((at, id)));
    }
}
