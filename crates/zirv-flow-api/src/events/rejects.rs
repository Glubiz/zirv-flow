//! Ingest rejects: what was refused and why, with a masked sample. The stored payload is
//! for replay and is never part of a response.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{IngestReject, RejectCursor, RejectFilter};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::problem::{ApiError, Problem};

#[derive(Serialize, ToSchema)]
pub(crate) struct ReasonBody {
    /// The property that failed; never its value.
    property: String,
    /// `missing_required`, `wrong_type`, `not_in_enum` or `unplanned_property`.
    reason: String,
}

impl ReasonBody {
    pub(crate) fn new(property: String, reason: &str) -> Self {
        Self {
            property,
            reason: reason.to_owned(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IngestRejectBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    received_at: OffsetDateTime,
    source: String,
    /// `event` today.
    kind: String,
    /// Null when the name itself was invalid.
    name: Option<String>,
    /// `validation_failed`, `unplanned_event`, `no_identifier` or another static code.
    reason: String,
    reasons: Vec<ReasonBody>,
    /// `{properties, masked}`: the values of PII-flagged properties, of unplanned properties
    /// and of every property of an unplanned event are null and listed in `masked`.
    #[schema(value_type = Object)]
    sample: serde_json::Value,
    person_id: Option<Uuid>,
    message_id: Option<String>,
    /// The trace of the ingest call: 32 lowercase hex characters.
    trace_id: String,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    replayed_at: Option<OffsetDateTime>,
}

impl From<IngestReject> for IngestRejectBody {
    fn from(reject: IngestReject) -> Self {
        Self {
            id: reject.id,
            received_at: reject.received_at,
            source: reject.source,
            kind: reject.kind,
            name: reject.name,
            reason: reject.reason,
            reasons: reject
                .reasons
                .into_iter()
                .map(|reason| ReasonBody {
                    property: reason.property,
                    reason: reason.reason.as_str().to_owned(),
                })
                .collect(),
            sample: reject.sample,
            person_id: reject.person_id,
            message_id: reject.message_id,
            trace_id: zirv_flow_core::TraceId::from_uuid(reject.trace_id).to_string(),
            replayed_at: reject.replayed_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IngestRejectsBody {
    /// Newest first.
    rejects: Vec<IngestRejectBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// The `name` and `reason` filters of the query string.
fn parse_filters(query: &str) -> (Option<String>, Option<String>) {
    let (mut name, mut reason) = (None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "name" => name = Some(value.into_owned()),
            "reason" => reason = Some(value.into_owned()),
            _ => {}
        }
    }
    (name, reason)
}

/// Ingest rejects, newest first, with their masked samples and reasons. The payload is never
/// returned.
#[utoipa::path(
    get,
    operation_id = "list_ingest_rejects",
    path = "/api/v1/workspaces/{ws}/ingest-rejects",
    tag = "events",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Rejects per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("name" = Option<String>, Query, description = "Only rejects of this event name"),
        ("reason" = Option<String>, Query, description = "Only rejects with this reason code")
    ),
    responses(
        (status = 200, description = "A page of rejects", body = IngestRejectsBody),
        (status = 400, description = "limit is out of range or before is malformed", body = Problem, content_type = "application/problem+json"),
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
) -> Result<Json<IngestRejectsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let query = query.unwrap_or_default();
    let (limit, before) = parse_page(&query)?;
    let (name, reason) = parse_filters(&query);
    let filter = RejectFilter {
        name: name.as_deref(),
        reason: reason.as_deref(),
    };
    let cursor = before.map(|(received_at, id)| RejectCursor { received_at, id });
    let (rejects, more) =
        zirv_flow_store::list_ingest_rejects(&state.pool, workspace.id, filter, cursor, limit)
            .await?;
    let next_before = match rejects.last() {
        Some(last) if more => Some(encode_cursor((last.received_at, last.id))),
        _ => None,
    };
    Ok(Json(IngestRejectsBody {
        rejects: rejects.into_iter().map(IngestRejectBody::from).collect(),
        next_before,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::events::{Reason, ReasonCode};

    use super::*;

    #[test]
    fn filters_are_read_from_the_query_string() {
        assert_eq!(parse_filters(""), (None, None));
        assert_eq!(
            parse_filters("limit=5&name=Order%20Completed&reason=validation_failed"),
            (
                Some("Order Completed".to_owned()),
                Some("validation_failed".to_owned())
            )
        );
    }

    #[test]
    fn a_reject_is_shown_with_its_sample_and_no_payload() {
        let reject = IngestReject {
            id: Uuid::nil(),
            received_at: OffsetDateTime::UNIX_EPOCH,
            source: "web".into(),
            kind: "event".into(),
            name: Some("order".into()),
            reason: "validation_failed".into(),
            reasons: vec![Reason {
                property: "total".into(),
                reason: ReasonCode::WrongType,
            }],
            sample: json!({"properties": {"total": null}, "masked": ["total"]}),
            person_id: None,
            message_id: Some("m-1".into()),
            trace_id: Uuid::nil(),
            replayed_at: None,
        };

        let shown = serde_json::to_value(IngestRejectBody::from(reject)).unwrap();

        assert_eq!(
            shown["reasons"],
            json!([{"property": "total", "reason": "wrong_type"}])
        );
        assert_eq!(shown["received_at"], "1970-01-01T00:00:00Z");
        assert_eq!(shown["trace_id"].as_str().unwrap().len(), 32);
        assert!(shown.get("payload").is_none());
    }
}
