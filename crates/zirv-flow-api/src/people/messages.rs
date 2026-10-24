//! The per-person message log (architecture 10.1, 12.3 and 15): the messages sent to a person,
//! newest first, each with the gates it went through and the exact reason it was or was not sent.
//! It needs the `read` role. A row holds no address and no identifier hash.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::TraceId;
use zirv_flow_store::{LoggedMessage, MessageLogCursor, MessageLogFilter, WaitFilter};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageTemplateBody {
    id: Uuid,
    /// None when the template no longer exists.
    name: Option<String>,
    /// The version that was sent; none when the message ended before one was read.
    version: Option<i32>,
    /// `marketing` or `transactional`.
    category: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageFlowBody {
    id: Uuid,
    name: Option<String>,
    version: Option<i32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageNodeBody {
    id: String,
    /// The step's name, when the flow version gives it one.
    name: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageDestinationBody {
    id: Uuid,
    name: Option<String>,
}

/// What one gate of the send found. Codes and ids only.
#[derive(Serialize, ToSchema)]
pub(crate) struct GateResultBody {
    /// `enrollment`, `template`, `sender`, `address`, `consent`, `suppression`, `quiet_hours`,
    /// `frequency_cap`, `render`, `rate_limit` or `delivery`.
    #[schema(example = "consent")]
    gate: String,
    /// `passed`, `blocked`, `deferred`, `in_flight` or `unconfirmed`.
    #[schema(example = "blocked")]
    result: String,
    /// The code that stopped the message.
    reason: Option<String>,
    /// A code that adds to the reason.
    note: Option<String>,
    /// For a deferral, when the message runs again.
    run_at: Option<String>,
    /// The consent policy rule that decided, by id.
    rule_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessageBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    completed_at: Option<OffsetDateTime>,
    template: MessageTemplateBody,
    flow: Option<MessageFlowBody>,
    node: MessageNodeBody,
    destination: MessageDestinationBody,
    /// `pending`, `sending`, `sent`, `skipped` or `failed`.
    #[schema(example = "skipped")]
    status: String,
    /// The code a skipped or failed message ended with, such as `no_consent`.
    #[schema(example = "no_consent")]
    reason: Option<String>,
    /// What decided the consent gate: `recorded_consent`, `topic_default`, `transactional`, a
    /// suppression's scope, or the name of the consent policy rule that denied the send.
    deciding_rule: Option<String>,
    /// The consent policy rule that denied the send, by id.
    consent_rule_id: Option<Uuid>,
    /// The workspace's consent policy, when a rule of it denied the send.
    consent_policy_id: Option<Uuid>,
    /// In the order the send ran them.
    gate_results: Vec<GateResultBody>,
    /// For a pending message that waits: when it runs again.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    deferred_until: Option<OffsetDateTime>,
    /// For a pending message that waits: `quiet_hours`, `frequency_cap` or `rate_limit`.
    deferred_why: Option<&'static str>,
    /// Look it up with `GET /api/v1/workspaces/{ws}/traces/{trace_id}`; none when the message has
    /// no recorded decisions.
    trace_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MessagesBody {
    /// Newest first.
    messages: Vec<MessageBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

fn gate_body(entry: &Value) -> Option<GateResultBody> {
    let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(GateResultBody {
        gate: text("gate")?,
        result: text("result")?,
        reason: text("reason"),
        note: text("note"),
        run_at: text("run_at"),
        rule_id: entry
            .get("rule")
            .and_then(Value::as_str)
            .and_then(|id| id.parse().ok()),
    })
}

impl From<LoggedMessage> for MessageBody {
    fn from(message: LoggedMessage) -> Self {
        let gate_results = message
            .gate_results
            .as_array()
            .map(|entries| entries.iter().filter_map(gate_body).collect())
            .unwrap_or_default();
        Self {
            id: message.id,
            created_at: message.created_at,
            completed_at: message.completed_at,
            template: MessageTemplateBody {
                id: message.template_id,
                name: message.template_name,
                version: message.template_version,
                category: message.category,
            },
            flow: message.flow_id.map(|id| MessageFlowBody {
                id,
                name: message.flow_name,
                version: message.flow_version,
            }),
            node: MessageNodeBody {
                id: message.node_id,
                name: message.node_name,
            },
            destination: MessageDestinationBody {
                id: message.destination_id,
                name: message.destination_name,
            },
            status: message.status,
            reason: message.reason,
            deciding_rule: message.deciding_rule,
            consent_rule_id: message.rule_id,
            consent_policy_id: message.policy_id,
            gate_results,
            deferred_until: message.deferred_until,
            deferred_why: message.deferred_why.map(|why| why.as_str()),
            trace_id: message
                .trace_id
                .map(|trace| TraceId::from_uuid(trace).to_string()),
        }
    }
}

/// Opaque to callers: base64url of the microseconds since the epoch and the id.
fn encode_cursor(cursor: MessageLogCursor) -> String {
    let micros = cursor.created_at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}", cursor.id))
}

fn decode_cursor(text: &str) -> Option<MessageLogCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (micros, id) = text.split_once(':')?;
    let micros: i128 = micros.parse().ok()?;
    let created_at = OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1000)?).ok()?;
    Some(MessageLogCursor {
        created_at,
        id: id.parse().ok()?,
    })
}

type Page = (MessageLogFilter, u32, Option<MessageLogCursor>);

fn parse_page(query: &str) -> Result<Page, ApiError> {
    let mut filter = MessageLogFilter::default();
    let (mut limit, mut before) = (DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => limit = parse_limit(&value)?,
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            "result" => {
                (filter.statuses, filter.wait) = match value.as_ref() {
                    "sent" => (Some(vec!["sent"]), None),
                    "skipped" => (Some(vec!["skipped"]), None),
                    "deferred" => (Some(vec!["pending"]), Some(WaitFilter::Deferred)),
                    "in_progress" => (
                        Some(vec!["pending", "sending"]),
                        Some(WaitFilter::InProgress),
                    ),
                    "failed" => (Some(vec!["failed"]), None),
                    _ => {
                        return Err(ApiError::bad_request(
                            "result must be sent, skipped, deferred, in_progress or failed",
                        ));
                    }
                };
            }
            "template_id" => {
                filter.template = Some(
                    value
                        .parse()
                        .map_err(|_| ApiError::bad_request("template_id is not an id"))?,
                );
            }
            _ => {}
        }
    }
    Ok((filter, limit, before))
}

/// A person's message log, newest first, in pages. Each message names its template, flow step and
/// destination, the gates it went through in order, the exact reason it was skipped or failed, the
/// consent rule that decided, and, for a message that waits, when it runs again. A person that was
/// merged into another reads as the survivor, with the merged people's messages included. No
/// address and no identifier hash is returned. Filter by `result` (`sent`, `skipped`, `deferred`
/// (waiting for a known time), `in_progress` (not yet sent and not waiting, or being sent) or
/// `failed`) and by `template_id`.
#[utoipa::path(
    get,
    operation_id = "list_person_messages",
    path = "/api/v1/workspaces/{ws}/people/{id}/messages",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("limit" = Option<u32>, Query, description = "Messages per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("result" = Option<String>, Query, description = "`sent`, `skipped`, `deferred`, `in_progress` or `failed`"),
        ("template_id" = Option<Uuid>, Query, description = "Only messages of this template")
    ),
    responses(
        (status = 200, description = "A page of messages", body = MessagesBody),
        (status = 400, description = "A parameter is out of range or malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person (an erased person included), or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<MessagesBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let person: Uuid = path_id(&id)?;
    let (filter, limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let page = zirv_flow_store::load_person_message_log(
        &state.pool,
        workspace.id,
        person,
        &filter,
        limit,
        before,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(MessagesBody {
        messages: page.messages.into_iter().map(Into::into).collect(),
        next_before: page.next.map(encode_cursor),
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_cursor_round_trips_to_the_microsecond() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap();
        let cursor = MessageLogCursor {
            created_at: at,
            id: Uuid::now_v7(),
        };

        assert_eq!(decode_cursor(&encode_cursor(cursor)), Some(cursor));
        assert_eq!(decode_cursor("!!"), None);
        assert_eq!(decode_cursor(&URL_SAFE_NO_PAD.encode("1:nope")), None);
    }

    #[test]
    fn the_page_defaults_to_fifty_and_maps_results_to_statuses() {
        let (filter, limit, before) = parse_page("").unwrap();
        assert_eq!(filter, MessageLogFilter::default());
        assert_eq!((limit, before), (50, None));
        for (result, status) in [
            ("sent", "sent"),
            ("skipped", "skipped"),
            ("deferred", "pending"),
            ("failed", "failed"),
        ] {
            let (filter, ..) = parse_page(&format!("result={result}")).unwrap();
            assert_eq!(filter.statuses, Some(vec![status]));
        }
        let (deferred, ..) = parse_page("result=deferred").unwrap();
        let (progress, ..) = parse_page("result=in_progress").unwrap();
        assert_eq!(deferred.wait, Some(WaitFilter::Deferred));
        assert_eq!(progress.wait, Some(WaitFilter::InProgress));
        assert_eq!(progress.statuses, Some(vec!["pending", "sending"]));
        let id = Uuid::now_v7();
        let (filter, ..) = parse_page(&format!("template_id={id}")).unwrap();
        assert_eq!(filter.template, Some(id));
    }

    #[test]
    fn a_bad_parameter_is_a_400() {
        for bad in [
            "limit=0",
            "limit=201",
            "before=%21",
            "result=delivered",
            "template_id=nope",
        ] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_gate_entry_keeps_codes_and_ids_only() {
        let rule = Uuid::from_u128(7);
        let entry = json!({
            "gate": "consent", "result": "blocked", "reason": "no_consent",
            "rule": rule, "change": Uuid::from_u128(8), "address": "never@example.test",
        });

        let body = serde_json::to_value(gate_body(&entry).unwrap()).unwrap();

        assert_eq!(body["gate"], "consent");
        assert_eq!(body["rule_id"], rule.to_string());
        assert!(!body.to_string().contains("never@example.test"));
        assert!(gate_body(&json!({"result": "passed"})).is_none());
    }
}
