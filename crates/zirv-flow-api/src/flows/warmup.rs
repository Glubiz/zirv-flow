//! The warm-up of a flow (architecture 9.4): changing the setting, and ending the window early.
//! The setting lives on the flow, so neither needs a publish.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{ValidationError, WorkspaceId};
use zirv_flow_store::{MAX_WARMUP_SECONDS, MIN_WARMUP_SECONDS};

use super::crud::{FlowDetailBody, flow_detail};
use super::flow_error;
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

#[derive(Deserialize, ToSchema)]
pub(crate) struct WarmupRequest {
    /// Whether a window holds back enrollments when the flow starts taking them.
    enabled: bool,
    /// The length of the window in seconds, 60 to 86400.
    #[schema(example = 300, minimum = 60, maximum = 86400)]
    seconds: i32,
}

/// Turns the warm-up of a flow on or off and sets its length. A window already running keeps its
/// start and counts the new length from it; turning it off lets everyone waiting in at once.
/// Needs the publisher role and is audited as `flow.warmup_changed`.
#[utoipa::path(
    put,
    operation_id = "set_flow_warmup",
    path = "/api/v1/workspaces/{ws}/flows/{id}/warmup",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = WarmupRequest,
    responses(
        (status = 200, description = "The flow with its new warm-up", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a publisher, a key without the publish scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is archived (`code` is `flow_archived`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A length outside 60 to 86400 seconds", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn set(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<WarmupRequest>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Publish).await?;
    let id = path_id(&id)?;
    check_seconds(body.seconds)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::set_warmup(&mut tx, workspace.id, id, body.enabled, body.seconds)
        .await
        .map_err(flow_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.warmup_changed",
        Some(("flow", id)),
        json!({ "enabled": body.enabled, "seconds": body.seconds }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    reread(&state, workspace.id, id).await
}

/// Ends the warm-up window early: everyone waiting is admitted now, through the same checks as
/// any entry. Needs the admin role and is audited as `flow.warmup_opened`.
#[utoipa::path(
    post,
    operation_id = "open_flow_warmup",
    path = "/api/v1/workspaces/{ws}/flows/{id}/warmup/open",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The flow, no longer warming up", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not an admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is not published: a draft, paused or archived flow cannot be opened", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn open(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let now = state.clock.now(&mut tx).await?;
    let waiting = zirv_flow_store::open_warmup(&mut tx, workspace.id, id, now)
        .await
        .map_err(flow_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.warmup_opened",
        Some(("flow", id)),
        json!({ "waiting": waiting }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    reread(&state, workspace.id, id).await
}

async fn reread(
    state: &AppState,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let flow = zirv_flow_store::find_flow(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(flow_detail(state, workspace, &flow).await?))
}

fn check_seconds(seconds: i32) -> Result<(), ApiError> {
    if (MIN_WARMUP_SECONDS..=MAX_WARMUP_SECONDS).contains(&seconds) {
        Ok(())
    } else {
        Err(ValidationError::new("seconds", "must be 60 to 86400").into())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_length_is_kept_between_a_minute_and_a_day() {
        for seconds in [60, 300, 86_400] {
            assert!(check_seconds(seconds).is_ok(), "{seconds}");
        }
        for seconds in [i32::MIN, -1, 0, 59, 86_401, i32::MAX] {
            assert!(check_seconds(seconds).is_err(), "{seconds}");
        }
    }

    #[test]
    fn the_request_needs_both_fields() {
        let both: Result<WarmupRequest, _> =
            serde_json::from_value(json!({"enabled": false, "seconds": 120}));
        let missing: Result<WarmupRequest, _> = serde_json::from_value(json!({"enabled": true}));

        assert!(both.is_ok());
        assert!(missing.is_err());
    }
}
