//! What a destination's delivery attempts did lately: counts by outcome and by status class over
//! a window, read from `delivery_attempts`. Counts only; no item id and nothing person-scoped.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use utoipa::ToSchema;
use zirv_flow_store::AttemptStats;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};
use crate::sources::invalid;

const DEFAULT_HOURS: i32 = 24;
const MAX_HOURS: i32 = 168;

#[derive(Serialize, ToSchema)]
pub(crate) struct OutcomeCountsBody {
    delivered: i64,
    retryable: i64,
    terminal: i64,
    throttled: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct StatusClassCountsBody {
    /// The call produced no answer.
    none: i64,
    #[serde(rename = "2xx")]
    #[schema(rename = "2xx")]
    success: i64,
    #[serde(rename = "3xx")]
    #[schema(rename = "3xx")]
    redirect: i64,
    #[serde(rename = "4xx")]
    #[schema(rename = "4xx")]
    client_error: i64,
    #[serde(rename = "5xx")]
    #[schema(rename = "5xx")]
    server_error: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DestinationStatsBody {
    /// The length of the window, ending now.
    window_hours: i32,
    /// Every attempt in the window.
    attempts: i64,
    outcomes: OutcomeCountsBody,
    status_classes: StatusClassCountsBody,
}

impl DestinationStatsBody {
    fn new(window_hours: i32, stats: AttemptStats) -> Self {
        Self {
            window_hours,
            attempts: stats.attempts,
            outcomes: OutcomeCountsBody {
                delivered: stats.delivered,
                retryable: stats.retryable,
                terminal: stats.terminal,
                throttled: stats.throttled,
            },
            status_classes: StatusClassCountsBody {
                none: stats.class_none,
                success: stats.class_2xx,
                redirect: stats.class_3xx,
                client_error: stats.class_4xx,
                server_error: stats.class_5xx,
            },
        }
    }
}

/// The `hours` of the query string: 1 to 168, 24 when absent.
fn hours_of(query: Option<&str>) -> Result<i32, ApiError> {
    let value = query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .find_map(|pair| pair.strip_prefix("hours="));
    match value {
        None => Ok(DEFAULT_HOURS),
        Some(text) => text
            .parse::<i32>()
            .ok()
            .filter(|hours| (1..=MAX_HOURS).contains(hours))
            .ok_or_else(|| invalid("hours", "must be a whole number from 1 to 168")),
    }
}

/// The delivery attempts of a destination over the last `hours` hours, counted by outcome and by
/// status class. Workspace admins only.
#[utoipa::path(
    get,
    operation_id = "get_destination_stats",
    path = "/api/v1/workspaces/{ws}/destinations/{id}/stats",
    tag = "destinations",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Destination id"),
        ("hours" = Option<u32>, Query, description = "The window in hours, 1 to 168; default 24")
    ),
    responses(
        (status = 200, description = "The counts", body = DestinationStatsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or destination, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "hours is outside 1 to 168", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<DestinationStatsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id = path_id(&id)?;
    let hours = hours_of(query.as_deref())?;
    zirv_flow_store::find_destination(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let stats =
        zirv_flow_store::destination_attempt_stats(&state.pool, workspace.id, id, hours).await?;
    Ok(Json(DestinationStatsBody::new(hours, stats)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_default_to_a_day_and_must_be_one_to_168() {
        assert_eq!(hours_of(None).unwrap(), 24);
        assert_eq!(hours_of(Some("other=1")).unwrap(), 24);
        assert_eq!(hours_of(Some("hours=1")).unwrap(), 1);
        assert_eq!(hours_of(Some("x=1&hours=168")).unwrap(), 168);
        for bad in ["hours=0", "hours=169", "hours=-3", "hours=", "hours=abc"] {
            assert_eq!(
                hours_of(Some(bad)).unwrap_err().status(),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}"
            );
        }
    }

    #[test]
    fn the_counts_serialize_under_their_status_class_names() {
        let stats = AttemptStats {
            attempts: 3,
            class_2xx: 2,
            class_5xx: 1,
            delivered: 2,
            retryable: 1,
            ..AttemptStats::default()
        };

        let json = serde_json::to_value(DestinationStatsBody::new(6, stats)).unwrap();

        assert_eq!(json["window_hours"], 6);
        assert_eq!(json["status_classes"]["2xx"], 2);
        assert_eq!(json["status_classes"]["5xx"], 1);
        assert_eq!(json["status_classes"]["none"], 0);
        assert_eq!(json["outcomes"]["throttled"], 0);
    }
}
