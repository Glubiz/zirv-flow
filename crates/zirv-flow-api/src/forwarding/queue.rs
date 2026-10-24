//! The forwarding queue as a workspace admin sees it: the dead letters, the depth and lag per
//! destination, and re-drive. A dead letter shows ids, a fixed error code, attempt counts and
//! times; never the payload, which may hold personal data.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{DeadLetter, QueueStatus, Redrive};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{decode_cursor, encode_cursor, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::sources::invalid;

const DEFAULT_LIMIT: u32 = 50;

#[derive(Serialize, ToSchema)]
pub(crate) struct DeadLetterBody {
    /// The outbox row; the id to re-drive it by.
    id: Uuid,
    destination_id: Uuid,
    rule_id: Uuid,
    event_id: Uuid,
    /// A fixed code, such as `http_4xx` or `egress_denied`.
    #[schema(example = "http_4xx")]
    error: Option<String>,
    /// Attempts in all, re-drives included.
    attempts: i32,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    died_at: OffsetDateTime,
}

impl From<DeadLetter> for DeadLetterBody {
    fn from(letter: DeadLetter) -> Self {
        Self {
            id: letter.id,
            destination_id: letter.destination_id,
            rule_id: letter.rule_id,
            event_id: letter.event_id,
            error: letter.last_error,
            attempts: letter.attempts,
            created_at: letter.created_at,
            died_at: letter.died_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DeadLettersBody {
    /// Newest first.
    letters: Vec<DeadLetterBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QueueStatusBody {
    destination_id: Uuid,
    /// Rows waiting for delivery, a row backed off for a retry included.
    depth: i64,
    /// Seconds the oldest waiting row has been past its due time; 0 when none is.
    oldest_lag_seconds: i64,
    /// Dead letters waiting for a re-drive.
    dead: i64,
}

impl From<QueueStatus> for QueueStatusBody {
    fn from(status: QueueStatus) -> Self {
        Self {
            destination_id: status.destination_id,
            depth: status.depth,
            oldest_lag_seconds: status.oldest_lag_seconds,
            dead: status.dead,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ForwardingStatusBody {
    destinations: Vec<QueueStatusBody>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RedriveRequest {
    /// Re-drives this one row, dead or pending.
    id: Option<Uuid>,
    /// Re-drives every dead row of this destination.
    destination_id: Option<Uuid>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RedriveBody {
    /// Rows put back to pending.
    redriven: u64,
}

fn parse_query(query: &str) -> Result<(u32, Option<(OffsetDateTime, Uuid)>, Option<Uuid>), ApiError> {
    let mut limit = DEFAULT_LIMIT;
    let mut before = None;
    let mut destination = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => limit = parse_limit(&value)?,
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            "destination_id" => {
                destination = Some(
                    value
                        .parse::<Uuid>()
                        .map_err(|_| ApiError::bad_request("destination_id is not an id"))?,
                );
            }
            _ => {}
        }
    }
    Ok((limit, before, destination))
}

/// The workspace's dead letters, newest first, without their payloads.
#[utoipa::path(
    get,
    operation_id = "list_forwarding_dead_letters",
    path = "/api/v1/workspaces/{ws}/forwarding/dead-letters",
    tag = "forwarding",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Letters per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("destination_id" = Option<Uuid>, Query, description = "Only this destination's letters")
    ),
    responses(
        (status = 200, description = "A page of dead letters", body = DeadLettersBody),
        (status = 400, description = "A malformed limit, cursor or destination id", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn dead_letters(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<DeadLettersBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (limit, before, destination) = parse_query(query.as_deref().unwrap_or_default())?;
    let mut connection = state.pool.acquire().await.map_err(|_| ApiError::internal())?;
    let mut letters = zirv_flow_store::list_dead_letters(
        &mut connection,
        workspace.id,
        destination,
        before,
        i64::from(limit) + 1,
    )
    .await?;
    let more = letters.len() > limit as usize;
    letters.truncate(limit as usize);
    let next_before = letters
        .last()
        .filter(|_| more)
        .map(|last| encode_cursor((last.died_at, last.id)));
    Ok(Json(DeadLettersBody {
        letters: letters.into_iter().map(DeadLetterBody::from).collect(),
        next_before,
    }))
}

/// The queue depth and the oldest pending lag of each destination that forwards events.
#[utoipa::path(
    get,
    operation_id = "get_forwarding_status",
    path = "/api/v1/workspaces/{ws}/forwarding/status",
    tag = "forwarding",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "Per destination status", body = ForwardingStatusBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn status(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<ForwardingStatusBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let mut connection = state.pool.acquire().await.map_err(|_| ApiError::internal())?;
    let rows = zirv_flow_store::forwarding_status(&mut connection, workspace.id).await?;
    Ok(Json(ForwardingStatusBody {
        destinations: rows.into_iter().map(QueueStatusBody::from).collect(),
    }))
}

/// Puts dead rows back in the queue: one row (`id`), every dead row of a destination
/// (`destination_id`), or every dead row of the workspace (an empty body). Attempts start again.
/// A re-drive writes no event and starts no flow. Audited as `forwarding.redriven`.
#[utoipa::path(
    post,
    operation_id = "redrive_forwarding",
    path = "/api/v1/workspaces/{ws}/forwarding/re-drive",
    tag = "forwarding",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = RedriveRequest,
    responses(
        (status = 200, description = "How many rows were put back", body = RedriveBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Both `id` and `destination_id` were given", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn redrive(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<RedriveRequest>,
) -> Result<Json<RedriveBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (scope, kind) = match (body.id, body.destination_id) {
        (Some(_), Some(_)) => {
            return Err(invalid("body", "names either id or destination_id, not both"));
        }
        (Some(id), None) => (Redrive::Row(id), "row"),
        (None, Some(id)) => (Redrive::Destination(id), "destination"),
        (None, None) => (Redrive::Workspace, "workspace"),
    };
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let redriven = zirv_flow_store::redrive(&mut tx, workspace.id, scope).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "forwarding.redriven",
        None,
        json!({"scope": kind, "id": body.id.or(body.destination_id), "redriven": redriven}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(RedriveBody { redriven }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_has_defaults_and_refuses_what_it_cannot_read() {
        assert_eq!(parse_query("").unwrap(), (50, None, None));
        let id = Uuid::from_u128(7);
        assert_eq!(
            parse_query(&format!("limit=2&destination_id={id}")).unwrap(),
            (2, None, Some(id))
        );
        for bad in ["limit=0", "limit=201", "before=%21", "destination_id=x"] {
            assert_eq!(
                parse_query(bad).unwrap_err().status(),
                axum::http::StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_dead_letter_body_has_ids_codes_and_times_but_no_payload() {
        let body = DeadLetterBody::from(DeadLetter {
            id: Uuid::nil(),
            destination_id: Uuid::nil(),
            rule_id: Uuid::nil(),
            event_id: Uuid::nil(),
            last_error: Some("http_4xx".into()),
            attempts: 3,
            created_at: OffsetDateTime::UNIX_EPOCH,
            died_at: OffsetDateTime::UNIX_EPOCH,
        });
        let json = serde_json::to_value(body).unwrap();

        assert_eq!(json["error"], "http_4xx");
        assert!(json.get("payload").is_none());
        assert!(json.get("idempotency_key").is_none());
    }
}
