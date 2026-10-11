//! The person timeline (architecture section 12.3): a person's events and the decisions the
//! system made about them, newest first, each linking to its trace.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::TraceId;
use zirv_flow_core::events::mask;
use zirv_flow_store::{
    TimelineCursor, TimelineDecision, TimelineEntry, TimelineEvent, TimelineFilter, TimelineKind,
};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::consent::bool_param;
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct EventEntryBody {
    id: Uuid,
    /// When the event occurred.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    at: OffsetDateTime,
    #[schema(example = "order_completed")]
    name: String,
    /// The stored properties. The values of properties the published schema flags as PII are
    /// null for callers who may not see values; so is every value of an event without a
    /// published schema.
    #[schema(value_type = Object)]
    properties: Map<String, Value>,
    /// The names of the properties whose values were replaced by null.
    masked: Vec<String>,
    /// `valid`, `flagged` or `unplanned`.
    #[schema(example = "valid")]
    validation_state: String,
    /// Timing flags such as `late` or `future`.
    flags: Vec<String>,
    /// Look it up with `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    trace_id: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DecisionEntryBody {
    id: Uuid,
    /// When the decision was recorded.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    at: OffsetDateTime,
    /// What was decided, such as `trait.change`, `person.merge`, `consent.change`,
    /// `list.membership` or `identity.conflict`.
    #[schema(example = "trait.change")]
    action: String,
    #[schema(example = "changed")]
    outcome: String,
    /// A reason code; never personal data.
    reason: Option<String>,
    /// `event`, `trait_change`, `schedule`, `manual`, `api` or `definition_change`.
    cause_kind: String,
    /// Durable domain ids by name; never personal data.
    #[schema(value_type = Object)]
    subject_ids: Value,
    /// Look it up with `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    trace_id: String,
}

#[derive(Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TimelineEntryBody {
    Event(EventEntryBody),
    Decision(DecisionEntryBody),
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TimelineBody {
    /// Newest first.
    entries: Vec<TimelineEntryBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

fn trace_text(trace: Uuid) -> String {
    TraceId::from_uuid(trace).to_string()
}

fn event_body(event: TimelineEvent, reveal: bool) -> EventEntryBody {
    let stored = match event.properties {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let (properties, masked) = match (&event.schema, reveal) {
        (_, true) => (stored, Vec::new()),
        (Some(schema), false) => {
            let masked = mask(&stored, schema, false);
            (masked.properties, masked.masked)
        }
        // Without a published schema nothing says which values are personal.
        (None, false) => {
            let mut names: Vec<String> = stored.keys().cloned().collect();
            names.sort();
            let nulled = stored
                .keys()
                .map(|name| (name.clone(), Value::Null))
                .collect();
            (nulled, names)
        }
    };
    EventEntryBody {
        id: event.id,
        at: event.at,
        name: event.name,
        properties,
        masked,
        validation_state: event.validation_state,
        flags: event.flags,
        trace_id: trace_text(event.trace_id),
    }
}

fn decision_body(decision: TimelineDecision) -> DecisionEntryBody {
    DecisionEntryBody {
        id: decision.id,
        at: decision.at,
        action: decision.action,
        outcome: decision.outcome,
        reason: decision.reason,
        cause_kind: decision.cause_kind,
        subject_ids: decision.subject_ids,
        trace_id: trace_text(decision.trace_id),
    }
}

fn entry_body(entry: TimelineEntry, reveal: bool) -> TimelineEntryBody {
    match entry {
        TimelineEntry::Event(event) => TimelineEntryBody::Event(event_body(event, reveal)),
        TimelineEntry::Decision(decision) => TimelineEntryBody::Decision(decision_body(decision)),
    }
}

/// Opaque to callers: base64url of the microseconds since the epoch, the kind and the id.
pub(crate) fn encode_cursor(cursor: TimelineCursor) -> String {
    let micros = cursor.at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}:{}", cursor.kind.as_str(), cursor.id))
}

pub(crate) fn decode_cursor(text: &str) -> Option<TimelineCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.splitn(3, ':');
    let micros: i128 = parts.next()?.parse().ok()?;
    let kind: TimelineKind = parts.next()?.parse().ok()?;
    let id: Uuid = parts.next()?.parse().ok()?;
    let at = OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1000)?).ok()?;
    Some(TimelineCursor { at, kind, id })
}

type Page = (TimelineFilter, u32, Option<TimelineCursor>);

fn parse_page(query: &str) -> Result<Page, ApiError> {
    let mut filter = TimelineFilter {
        all_decisions: bool_param(query, "all_decisions")?,
        ..TimelineFilter::default()
    };
    let (mut limit, mut before) = (DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                limit = parse_limit(&value)?;
            }
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            "kinds" => {
                filter.events = false;
                filter.decisions = false;
                for kind in value.split(',') {
                    match kind.parse::<TimelineKind>() {
                        Ok(TimelineKind::Event) => filter.events = true,
                        Ok(TimelineKind::Decision) => filter.decisions = true,
                        Err(()) => {
                            return Err(ApiError::bad_request(
                                "kinds must be event, decision or both separated by a comma",
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok((filter, limit, before))
}

/// A person's timeline, newest first: their events, and the decisions the system made about
/// them (trait changes, merges, consent changes, list membership, identity conflicts). Each
/// entry has a `trace_id` that opens the decisions behind it. A person that was merged into
/// another reads as the survivor, with the merged people's entries included. Event properties
/// the schema flags as PII are masked for callers who may not see values.
///
/// `event.accepted` decisions duplicate the event entry and are left out unless
/// `all_decisions=true`.
#[utoipa::path(
    get,
    operation_id = "get_person_timeline",
    path = "/api/v1/workspaces/{ws}/people/{id}/timeline",
    tag = "people",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Person id"),
        ("limit" = Option<u32>, Query, description = "Entries per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("kinds" = Option<String>, Query, description = "`event`, `decision` or both separated by a comma; default both"),
        ("all_decisions" = Option<bool>, Query, description = "Include the `event.accepted` decisions; default false")
    ),
    responses(
        (status = 200, description = "A page of entries", body = TimelineBody),
        (status = 400, description = "limit is out of range, or before, kinds or all_decisions is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<TimelineBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let person: Uuid = path_id(&id)?;
    let (filter, limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let page =
        zirv_flow_store::load_timeline(&state.pool, workspace.id, person, filter, limit, before)
            .await?
            .ok_or_else(ApiError::not_found)?;
    let reveal = may_see_values(&state, &principal, workspace.id).await;
    Ok(Json(TimelineBody {
        entries: page
            .entries
            .into_iter()
            .map(|entry| entry_body(entry, reveal))
            .collect(),
        next_before: page.next.map(encode_cursor),
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use zirv_flow_core::events::{PropertyType, SchemaProperty};

    use super::*;

    fn declared(name: &str, pii: bool) -> SchemaProperty {
        SchemaProperty {
            name: name.into(),
            property_type: PropertyType::String,
            required: false,
            enum_values: None,
            description: String::new(),
            pii,
        }
    }

    fn event(schema: Option<Vec<SchemaProperty>>) -> TimelineEvent {
        TimelineEvent {
            id: Uuid::nil(),
            at: OffsetDateTime::UNIX_EPOCH,
            name: "signup".into(),
            properties: json!({"email": "a@example.com", "plan": "pro"}),
            schema,
            validation_state: "valid".into(),
            flags: vec!["late".into()],
            trace_id: Uuid::nil(),
        }
    }

    #[test]
    fn a_cursor_round_trips_with_its_kind_to_the_microsecond() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap();
        for kind in [TimelineKind::Event, TimelineKind::Decision] {
            let cursor = TimelineCursor {
                at,
                kind,
                id: Uuid::now_v7(),
            };

            let text = encode_cursor(cursor);

            assert_eq!(decode_cursor(&text), Some(cursor));
            assert!(!text.contains(['=', '+', '/']));
        }
    }

    #[test]
    fn a_malformed_cursor_does_not_decode() {
        let id = Uuid::nil();
        for bad in [
            String::new(),
            "!!".to_owned(),
            URL_SAFE_NO_PAD.encode("1:event"),
            URL_SAFE_NO_PAD.encode(format!("1:job:{id}")),
            URL_SAFE_NO_PAD.encode(format!("x:event:{id}")),
            URL_SAFE_NO_PAD.encode("1:event:not-a-uuid"),
            URL_SAFE_NO_PAD.encode(format!("1{}:event:{id}", "0".repeat(36))),
        ] {
            assert_eq!(decode_cursor(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_page_defaults_to_both_kinds_and_fifty_and_refuses_bad_values() {
        let (filter, limit, before) = parse_page("").unwrap();
        assert_eq!(filter, TimelineFilter::default());
        assert_eq!((limit, before), (50, None));
        assert!(parse_page("all_decisions=true").unwrap().0.all_decisions);
        let events = parse_page("kinds=event").unwrap().0;
        assert!(events.events && !events.decisions);
        let both = parse_page("kinds=decision,event").unwrap().0;
        assert!(both.events && both.decisions);
        for bad in [
            "limit=0",
            "limit=201",
            "limit=x",
            "before=%21",
            "kinds=",
            "kinds=event,flow",
            "kinds=trace",
            "all_decisions=maybe",
        ] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn pii_properties_are_nulled_by_schema_for_those_without_access() {
        let schema = vec![declared("email", true), declared("plan", false)];

        let hidden = event_body(event(Some(schema.clone())), false);
        let shown = event_body(event(Some(schema)), true);

        assert_eq!(hidden.properties["email"], Value::Null);
        assert_eq!(hidden.properties["plan"], "pro");
        assert_eq!(hidden.masked, ["email"]);
        assert_eq!(shown.properties["email"], "a@example.com");
        assert!(shown.masked.is_empty());
        assert_eq!(hidden.flags, ["late"]);
        assert_eq!(hidden.trace_id.len(), 32);
    }

    #[test]
    fn an_event_without_a_schema_is_fully_masked_for_those_without_access() {
        let hidden = event_body(event(None), false);
        let shown = event_body(event(None), true);

        assert!(hidden.properties.values().all(Value::is_null));
        assert_eq!(hidden.properties.len(), 2);
        assert_eq!(hidden.masked, ["email", "plan"]);
        assert_eq!(shown.properties["plan"], "pro");
    }

    #[test]
    fn an_entry_is_tagged_by_its_type() {
        let decision = decision_body(TimelineDecision {
            id: Uuid::nil(),
            at: OffsetDateTime::UNIX_EPOCH,
            action: "person.merge".into(),
            outcome: "merged".into(),
            reason: None,
            cause_kind: "api".into(),
            subject_ids: json!({"person": Uuid::nil()}),
            trace_id: Uuid::nil(),
        });

        let as_event =
            serde_json::to_value(TimelineEntryBody::Event(event_body(event(None), true))).unwrap();
        let as_decision = serde_json::to_value(TimelineEntryBody::Decision(decision)).unwrap();

        assert_eq!(as_event["type"], "event");
        assert_eq!(as_decision["type"], "decision");
        assert_eq!(as_decision["action"], "person.merge");
        assert_eq!(as_decision["at"], "1970-01-01T00:00:00Z");
    }
}
