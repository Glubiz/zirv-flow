//! The controls an operator has over the enrollments of a flow (architecture 9.4, ruling
//! 43163028): retry or skip a failed enrollment, and end every in-flight enrollment of an older
//! version in bulk. All of them need the workspace admin role, are audited, and record a decision
//! per enrollment under a trace whose cause is `manual` (the signed-in user), which the audit
//! entry names. A paused or an archived flow refuses all three with a 409 (`flow_paused`,
//! `flow_archived`) and writes nothing: an operator resumes a paused flow first.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{ControlError, FlowState, LifecycleAction, enqueue_lifecycle};

use super::operator_trace;
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// A retry or a skip that was queued.
#[derive(Serialize, ToSchema)]
pub(crate) struct EnrollmentActionBody {
    enrollment_id: Uuid,
    /// `retry` or `skip`.
    action: &'static str,
    /// The trace the operator's decision is recorded under: the enrollment's own.
    trace_id: Uuid,
}

/// A bulk exit that was queued.
#[derive(Serialize, ToSchema)]
pub(crate) struct ExitVersionBody {
    /// The version whose in-flight enrollments are ended.
    version: i32,
    /// The trace each enrollment's decision is recorded under.
    trace_id: Uuid,
}

fn control_error(error: ControlError) -> ApiError {
    match error {
        ControlError::NotFound => ApiError::not_found(),
        ControlError::FlowArchived => ApiError::flow_archived(),
        ControlError::FlowPaused => ApiError::flow_paused(),
        refused @ (ControlError::NotFailed(_) | ControlError::SkipUnavailable) => {
            ApiError::enrollment_state(refused.to_string())
        }
        ControlError::Store(error) => error.into(),
    }
}

async fn act(
    state: &AppState,
    principal: &Principal,
    ws: &str,
    ids: (&str, &str),
    skip: bool,
) -> Result<(StatusCode, Json<EnrollmentActionBody>), ApiError> {
    let workspace = authorize(state, principal, path_id(ws)?, Need::Admin).await?;
    let (flow, enrollment): (Uuid, Uuid) = (path_id(ids.0)?, path_id(ids.1)?);
    let trace = operator_trace(principal);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let trace_id =
        zirv_flow_store::resume_failed(&mut tx, workspace.id, flow, enrollment, skip, &trace)
            .await
            .map_err(control_error)?
            .as_uuid();
    let action = if skip { "skip" } else { "retry" };
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        if skip {
            "flow.enrollment_skipped"
        } else {
            "flow.enrollment_retried"
        },
        Some(("enrollment", enrollment)),
        json!({ "flow_id": flow, "trace_id": trace_id }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(EnrollmentActionBody {
            enrollment_id: enrollment,
            action,
            trace_id,
        }),
    ))
}

/// Retries a failed enrollment: the step that failed runs again from the state it started from.
/// Refused while the flow is archived or paused. Records a `flow.enrollment` decision (`retried`)
/// and is audited as `flow.enrollment_retried`.
#[utoipa::path(
    post,
    operation_id = "retry_flow_enrollment",
    path = "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("enrollment_id" = Uuid, Path, description = "Enrollment id")
    ),
    responses(
        (status = 202, description = "The retry was queued", body = EnrollmentActionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or enrollment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The enrollment is not failed (`code` is `enrollment_state`), or the flow is archived (`flow_archived`) or paused (`flow_paused`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn retry(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, enrollment)): Path<(String, String, String)>,
) -> Result<(StatusCode, Json<EnrollmentActionBody>), ApiError> {
    act(&state, &principal, &ws, (&id, &enrollment), false).await
}

/// Skips the node a failed enrollment stopped at: it leaves along the node's `next` port. Only a
/// failure the engine placed at its node can be skipped; any other is retried. Refused while the
/// flow is archived or paused. Records a
/// `flow.enrollment` decision (`skipped`) and is audited as `flow.enrollment_skipped`.
#[utoipa::path(
    post,
    operation_id = "skip_flow_enrollment",
    path = "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("enrollment_id" = Uuid, Path, description = "Enrollment id")
    ),
    responses(
        (status = 202, description = "The skip was queued", body = EnrollmentActionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or enrollment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The enrollment is not failed or its failure cannot be skipped (`code` is `enrollment_state`), or the flow is archived (`flow_archived`) or paused (`flow_paused`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn skip(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, enrollment)): Path<(String, String, String)>,
) -> Result<(StatusCode, Json<EnrollmentActionBody>), ApiError> {
    act(&state, &principal, &ws, (&id, &enrollment), true).await
}

/// Ends every in-flight enrollment pinned to an older version of the flow, in batches (ruling
/// 469cfc51). Each exits with reason `manual` and gets its own decision. The version must be older
/// than the flow's current one. A paused or archived flow refuses it. Audited as
/// `flow.enrollments_exited`.
#[utoipa::path(
    post,
    operation_id = "exit_flow_version_enrollments",
    path = "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("n" = i32, Path, description = "Version number; older than the current version")
    ),
    responses(
        (status = 202, description = "The exit was queued", body = ExitVersionBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is a draft or the version is not older than the current one (`code` is `flow_state`), or the flow is archived (`flow_archived`) or paused (`flow_paused`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn exit_version(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<(StatusCode, Json<ExitVersionBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (id, version): (Uuid, i32) = (path_id(&id)?, path_id(&n)?);
    let flow = zirv_flow_store::find_flow(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    match flow.state {
        FlowState::Published => {}
        FlowState::Paused => return Err(ApiError::flow_paused()),
        FlowState::Draft => {
            return Err(ApiError::flow_state(
                "the flow is a draft and has no enrollments",
            ));
        }
        FlowState::Archived => return Err(ApiError::flow_archived()),
    }
    let current = flow.current_version.ok_or_else(ApiError::internal)?;
    zirv_flow_store::find_flow_version(&state.pool, workspace.id, id, version)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if version >= current {
        return Err(ApiError::flow_state(format!(
            "version {version} is not older than the current version {current}"
        )));
    }
    let trace = operator_trace(&principal);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    // The state again, under the lock a pause or an archive waits on: one that commits between
    // the read above and here is seen, and refuses the exit before it writes anything.
    match zirv_flow_store::flow_state_for_step(&mut tx, workspace.id, id).await? {
        Some(FlowState::Paused) => return Err(ApiError::flow_paused()),
        Some(FlowState::Archived) => return Err(ApiError::flow_archived()),
        Some(FlowState::Draft | FlowState::Published) => {}
        None => return Err(ApiError::not_found()),
    }
    enqueue_lifecycle(
        &mut tx,
        workspace.id,
        id,
        LifecycleAction::ExitVersion { version },
        &trace,
    )
    .await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.enrollments_exited",
        Some(("flow", id)),
        json!({ "version": version, "trace_id": trace.trace_id.as_uuid() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ExitVersionBody {
            version,
            trace_id: trace.trace_id.as_uuid(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::EnrollmentStatus;

    use super::*;

    #[test]
    fn a_refused_control_is_a_404_or_a_409_with_the_enrollment_state_code() {
        assert_eq!(
            control_error(ControlError::NotFound).status(),
            StatusCode::NOT_FOUND
        );
        for refused in [
            ControlError::NotFailed(EnrollmentStatus::Waiting),
            ControlError::SkipUnavailable,
        ] {
            let error = control_error(refused);
            assert_eq!(error.status(), StatusCode::CONFLICT);
            assert_eq!(error.code(), Some("enrollment_state"));
        }
        for (refused, code) in [
            (ControlError::FlowArchived, "flow_archived"),
            (ControlError::FlowPaused, "flow_paused"),
        ] {
            let error = control_error(refused);
            assert_eq!(error.status(), StatusCode::CONFLICT);
            assert_eq!(error.code(), Some(code));
        }
    }
}
