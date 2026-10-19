//! Publishing a flow into an immutable version, the dry-run impact preview, and pausing, resuming
//! and archiving. All of them need the workspace admin role (ruling 43163028).

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::flow::{FlowSettings, Trigger};
use zirv_flow_store::{Flow, FlowAction, FlowVersion};

use super::crud::{FlowBody, FlowDetailBody, FlowVersionBody, check_flow, issues_schema};
use super::{draft_of, flow_error, issue_problems, operator_trace};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, FlowIssueProblem, Problem};
use crate::trace::request_trace;

/// A source that feeds what the flow reads and is not fresh.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnhealthyInputBody {
    source_id: Uuid,
    name: String,
    /// `stale` or `failing`: the worst state among the source's enabled streams.
    state: &'static str,
    /// When its oldest feeding stream last succeeded; null when one never has.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    last_success_at: Option<OffsetDateTime>,
}

/// The people a segment trigger with `enroll_current` would enroll at publish.
#[derive(Serialize, ToSchema)]
pub(crate) struct AudienceBody {
    segment_id: Uuid,
    /// The members at the last refresh of the segment.
    members: i64,
    /// When the members were last computed; null before the first refresh.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    computed_at: Option<OffsetDateTime>,
}

/// What publishing would do, computed and not written.
#[derive(Serialize, ToSchema)]
pub(crate) struct ImpactBody {
    /// True exactly when there are no issues: a publish would succeed.
    valid: bool,
    #[schema(schema_with = issues_schema)]
    issues: Vec<FlowIssueProblem>,
    /// The version a publish would create.
    next_version: i32,
    /// The members a segment trigger with `enroll_current` would enroll; null for any other
    /// trigger, and when the segment does not exist.
    audience: Option<AudienceBody>,
    /// Active enrollments that stay on the version they entered when the new version goes live
    /// (ruling 469cfc51: in-flight enrollments are never migrated): the flow's active, waiting and
    /// failed enrollments.
    enrollments_staying_on_older_versions: i64,
    /// Sources that feed what the flow reads and are stale or failing. They are recorded for the
    /// operator and never pause the flow (ruling 5c679ecb).
    unhealthy_inputs: Vec<UnhealthyInputBody>,
}

/// The new version and the flow after the publish.
#[derive(Serialize, ToSchema)]
pub(crate) struct PublishedBody {
    flow: FlowBody,
    version: FlowVersionBody,
}

/// Whether the query asks for a dry run: `dry_run`, `dry_run=true`, or `dry_run=false`.
fn dry_run_param(query: Option<&str>) -> Result<bool, ApiError> {
    let mut flag = false;
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or_default().as_bytes()) {
        if key == "dry_run" {
            flag = match value.as_ref() {
                "" | "true" => true,
                "false" => false,
                _ => return Err(ApiError::bad_request("dry_run must be true or false")),
            };
        }
    }
    Ok(flag)
}

fn refuse_if_not_allowed(flow: &Flow, action: FlowAction) -> Result<(), ApiError> {
    flow.state.after(action).map(|_| ()).ok_or_else(|| {
        flow_error(zirv_flow_store::FlowError::IllegalTransition {
            state: flow.state,
            action,
        })
    })
}

/// The members a segment trigger with `enroll_current` enrolls at publish.
async fn audience_of(
    connection: &mut zirv_flow_store::PgConnection,
    workspace: WorkspaceId,
    settings: &FlowSettings,
) -> Result<Option<AudienceBody>, ApiError> {
    let Trigger::Segment {
        segment_id,
        enroll_current: true,
        ..
    } = settings.trigger
    else {
        return Ok(None);
    };
    let segment = zirv_flow_store::find_segment(&mut *connection, workspace, segment_id)
        .await?
        .filter(|segment| segment.archived_at.is_none());
    Ok(segment.map(|segment| AudienceBody {
        segment_id,
        members: i64::from(segment.member_count),
        computed_at: segment.computed_at,
    }))
}

/// The impact of publishing the flow's draft, in a transaction that is dropped.
async fn impact(
    state: &AppState,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<ImpactBody, ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let flow = zirv_flow_store::find_flow(&mut *tx, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    refuse_if_not_allowed(&flow, FlowAction::Publish)?;
    let (_, settings) = draft_of(&flow)?;
    let (checked, now) = check_flow(state, &mut tx, workspace, &flow).await?;
    let audience = audience_of(&mut tx, workspace, &settings).await?;
    let unhealthy =
        zirv_flow_store::unhealthy_inputs(&mut tx, workspace, &checked.reads, now).await?;
    let staying = zirv_flow_store::count_in_flight_enrollments(&mut tx, workspace, id).await?;
    drop(tx);
    Ok(ImpactBody {
        valid: checked.issues.is_empty(),
        issues: issue_problems(&checked.issues),
        next_version: flow.current_version.unwrap_or(0) + 1,
        audience,
        enrollments_staying_on_older_versions: staying,
        unhealthy_inputs: unhealthy
            .into_iter()
            .map(|input| UnhealthyInputBody {
                source_id: input.source_id,
                name: input.name,
                state: input.state.as_str(),
                last_success_at: input.last_success_at,
            })
            .collect(),
    })
}

/// Validates the draft under the flow's lock and publishes it as the next version, or changes
/// nothing.
async fn publish_now(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(Flow, FlowVersion), ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let locked = zirv_flow_store::lock_flow(&mut tx, workspace, id)
        .await
        .map_err(flow_error)?;
    refuse_if_not_allowed(locked.flow(), FlowAction::Publish)?;
    let (checked, _) = check_flow(state, &mut tx, workspace, locked.flow()).await?;
    if !checked.issues.is_empty() {
        // Dropping the transaction writes nothing.
        return Err(ApiError::flow_invalid(issue_problems(&checked.issues)));
    }
    let (flow, version) = zirv_flow_store::publish_flow(
        &mut tx,
        &state.clock,
        workspace,
        &locked,
        principal.user_id(),
        &request_trace(),
    )
    .await
    .map_err(flow_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace),
        "flow.published",
        Some(("flow", flow.id)),
        json!({ "name": flow.name, "version": version.version }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((flow, version))
}

/// Publishes the draft as the next immutable version. With `dry_run` (`?dry_run=true`) nothing is
/// written and the response is the impact: the issues, the audience a segment trigger would
/// enroll, how many active enrollments stay on older versions, and the stale or failing inputs.
/// Without it, a draft with issues is a 422 `flow_invalid` that lists them. A draft becomes
/// `published`; a published or paused flow keeps its state. Records a `flow.publish` decision and
/// is audited as `flow.published`.
#[utoipa::path(
    post,
    operation_id = "publish_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/publish",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("dry_run" = Option<bool>, Query, description = "Only compute the impact; write nothing")
    ),
    responses(
        (status = 200, description = "A dry run: what publishing would do", body = ImpactBody),
        (status = 201, description = "The flow and its new version", body = PublishedBody),
        (status = 400, description = "dry_run is not true or false", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is archived (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The draft has issues (`code` is `flow_invalid`, with `flow_issues`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn publish(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let id = path_id(&id)?;
    if dry_run_param(query.as_deref())? {
        let body = impact(&state, workspace.id, id).await?;
        return Ok(Json(body).into_response());
    }
    let (flow, version) = publish_now(&state, &principal, workspace.id, id).await?;
    let body = PublishedBody {
        flow: FlowBody::from(&flow),
        version: FlowVersionBody::from(&version),
    };
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// Runs a lifecycle action and audits it.
async fn lifecycle(
    state: &AppState,
    principal: &Principal,
    workspace: WorkspaceId,
    id: Uuid,
    action: FlowAction,
) -> Result<Flow, ApiError> {
    let trace = operator_trace(principal);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let (flow, audited) = match action {
        FlowAction::Pause => (
            zirv_flow_store::pause_flow(&mut tx, workspace, id, &trace).await,
            "flow.paused",
        ),
        FlowAction::Resume => (
            zirv_flow_store::resume_flow(&mut tx, workspace, id, &trace).await,
            "flow.resumed",
        ),
        FlowAction::Archive => (
            zirv_flow_store::archive_flow(&mut tx, workspace, id, &trace).await,
            "flow.archived",
        ),
        FlowAction::Publish => return Err(ApiError::internal()),
    };
    let flow = flow.map_err(flow_error)?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace),
        audited,
        Some(("flow", flow.id)),
        json!({ "name": flow.name, "state": flow.state.as_str() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(flow)
}

/// Pauses a published flow: no new enrollments until it is resumed. Records a `flow.pause`
/// decision and is audited as `flow.paused`.
#[utoipa::path(
    post,
    operation_id = "pause_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/pause",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The paused flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is not published (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn pause(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let flow = lifecycle(
        &state,
        &principal,
        workspace.id,
        path_id(&id)?,
        FlowAction::Pause,
    )
    .await?;
    Ok(Json(FlowDetailBody::from(&flow)))
}

/// Resumes a paused flow. Records a `flow.resume` decision and is audited as `flow.resumed`.
#[utoipa::path(
    post,
    operation_id = "resume_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/resume",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The resumed flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is not paused (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn resume(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let flow = lifecycle(
        &state,
        &principal,
        workspace.id,
        path_id(&id)?,
        FlowAction::Resume,
    )
    .await?;
    Ok(Json(FlowDetailBody::from(&flow)))
}

/// Archives a flow. Nothing is deleted: the versions stay readable. Records a `flow.archive`
/// decision and is audited as `flow.archived`.
#[utoipa::path(
    post,
    operation_id = "archive_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/archive",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    responses(
        (status = 200, description = "The archived flow", body = FlowDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is already archived (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn archive(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
) -> Result<Json<FlowDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let flow = lifecycle(
        &state,
        &principal,
        workspace.id,
        path_id(&id)?,
        FlowAction::Archive,
    )
    .await?;
    Ok(Json(FlowDetailBody::from(&flow)))
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::FlowState;

    use super::*;

    #[test]
    fn dry_run_is_off_unless_asked_for_and_a_bare_flag_asks() {
        assert!(!dry_run_param(None).unwrap());
        assert!(!dry_run_param(Some("")).unwrap());
        assert!(!dry_run_param(Some("other=true")).unwrap());
        assert!(!dry_run_param(Some("dry_run=false")).unwrap());
        assert!(dry_run_param(Some("dry_run")).unwrap());
        assert!(dry_run_param(Some("dry_run=true")).unwrap());
        assert!(dry_run_param(Some("a=b&dry_run=true")).unwrap());
        assert!(dry_run_param(Some("dry_run=1")).is_err());
        assert!(dry_run_param(Some("dry_run=yes")).is_err());
    }

    #[test]
    fn an_archived_flow_cannot_be_published_and_the_others_can() {
        let flow = |state| Flow {
            id: Uuid::nil(),
            name: "A".into(),
            state,
            draft_graph: json!({}),
            draft_settings: json!({}),
            current_version: None,
            archived_at: None,
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };

        for state in [FlowState::Draft, FlowState::Published, FlowState::Paused] {
            assert!(refuse_if_not_allowed(&flow(state), FlowAction::Publish).is_ok());
        }
        let refused =
            refuse_if_not_allowed(&flow(FlowState::Archived), FlowAction::Publish).unwrap_err();
        assert_eq!(refused.code(), Some("flow_state"));
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        assert_eq!(
            refused.detail(),
            "the flow is archived and cannot be published"
        );
    }
}
