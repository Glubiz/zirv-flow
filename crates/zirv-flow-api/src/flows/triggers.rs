//! Starting a flow from outside: the API trigger and the manual start (architecture 9.1).
//!
//! Both only queue a `flow.enroll` job, so they share the path every other trigger takes: the
//! job enrolls through `store::enroll_entries`, which enforces the re-entry rule and records each
//! refusal. The API trigger takes `Edit` (ruling 92d5483d); a manual start for an audience takes
//! `Publish`, since it can enroll a whole segment or list at once.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::flow::{FlowSettings, Trigger};
use zirv_flow_core::people::Identifier;
use zirv_flow_core::{Cause, TraceContext, TraceId, WorkspaceId};
use zirv_flow_store::{
    AudienceSource, EnrollJobPayload, EnrollWork, Flow, FlowState, TriggerKind, enqueue,
    occurrence_of,
};

use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

/// The longest idempotency key.
const MAX_KEY_CHARS: usize = 200;

/// An identifier of a person.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PersonIdentifier {
    /// `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    #[schema(example = "email")]
    kind: String,
    /// The identifier value. It is looked up and not stored.
    value: String,
}

/// The person to enroll: give a `person_id` or an `identifier`, not both.
#[derive(Deserialize, ToSchema)]
pub(crate) struct EnrollPersonRequest {
    /// The person, or the person they were merged into.
    person_id: Option<Uuid>,
    /// An identifier of the person; it resolves to their current person.
    identifier: Option<PersonIdentifier>,
    /// Makes a retry of this request enroll nobody twice: the same key for the same person is one
    /// occurrence. Without a key every request is its own occurrence.
    idempotency_key: Option<String>,
}

/// A request that was queued.
#[derive(Serialize, ToSchema)]
pub(crate) struct QueuedBody {
    /// The occurrence the enrollment is idempotent on.
    occurrence_id: Uuid,
}

/// The audience of a manual start: give a `segment_id` or a `list_id`, not both.
#[derive(Deserialize, ToSchema)]
pub(crate) struct StartFlowRequest {
    /// Enroll the current members of this segment.
    segment_id: Option<Uuid>,
    /// Enroll the members of this list.
    list_id: Option<Uuid>,
}

/// The flow, or the 404; it must take the trigger `wanted` and be one that can take people.
async fn startable(
    state: &AppState,
    workspace: WorkspaceId,
    id: Uuid,
    wanted: &'static str,
) -> Result<Flow, ApiError> {
    let flow = zirv_flow_store::find_flow(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    match flow.state {
        FlowState::Published | FlowState::Paused => {}
        FlowState::Draft => {
            return Err(ApiError::flow_state(
                "the flow is a draft and has not been published",
            ));
        }
        FlowState::Archived => return Err(ApiError::flow_archived()),
    }
    let version = flow.current_version.ok_or_else(ApiError::internal)?;
    let published = zirv_flow_store::find_flow_version(&state.pool, workspace, id, version)
        .await?
        .ok_or_else(ApiError::internal)?;
    let settings: FlowSettings =
        serde_json::from_value(published.settings).map_err(|_| ApiError::internal())?;
    let matches = match settings.trigger {
        Trigger::Api {} => wanted == "api",
        Trigger::Manual {} => wanted == "manual",
        _ => false,
    };
    if !matches {
        return Err(ApiError::flow_state(format!(
            "the flow's trigger is not {wanted}"
        )));
    }
    Ok(flow)
}

/// The person a request names, after merges.
async fn resolve(
    state: &AppState,
    workspace: WorkspaceId,
    body: &EnrollPersonRequest,
) -> Result<Uuid, ApiError> {
    let found = match (&body.person_id, &body.identifier) {
        (Some(id), None) => zirv_flow_store::current_person(&state.pool, workspace, *id).await?,
        (None, Some(identifier)) => {
            let identifier =
                Identifier::parse(&identifier.kind, &identifier.value).map_err(|error| {
                    ApiError::unprocessable(format!("identifier: {}", error.reason()))
                })?;
            match zirv_flow_store::find_person_by_identifier(&state.pool, workspace, &identifier)
                .await?
            {
                Some(person) => {
                    zirv_flow_store::current_person(&state.pool, workspace, person.id).await?
                }
                None => None,
            }
        }
        _ => {
            return Err(ApiError::unprocessable(
                "body: must give either person_id or identifier",
            ));
        }
    };
    found.ok_or_else(ApiError::not_found)
}

/// Enrolls one person in a flow whose trigger is `api`. The request is queued as a `flow.enroll`
/// job and answered at once with its occurrence; the re-entry rule decides, in the job, whether
/// the person enters, and a refusal is a `flow.enroll` decision with its reason. A paused flow
/// takes nobody and records the skip. Retrying with the same `idempotency_key` enrolls nobody
/// twice.
#[utoipa::path(
    post,
    operation_id = "enroll_person",
    path = "/api/v1/workspaces/{ws}/flows/{id}/enrollments",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = EnrollPersonRequest,
    responses(
        (status = 202, description = "Queued", body = QueuedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is not published or paused, or its trigger is not `api` (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The body names no person or both, or the key is too long", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn enroll_person(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<EnrollPersonRequest>,
) -> Result<(StatusCode, Json<QueuedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let id = path_id(&id)?;
    if body
        .idempotency_key
        .as_ref()
        .is_some_and(|key| key.is_empty() || key.chars().count() > MAX_KEY_CHARS)
    {
        return Err(ApiError::unprocessable(format!(
            "idempotency_key: must be 1 to {MAX_KEY_CHARS} characters"
        )));
    }
    let flow = startable(&state, workspace.id, id, "api").await?;
    let person = resolve(&state, workspace.id, &body).await?;
    let occurrence = match &body.idempotency_key {
        Some(key) => occurrence_of(&format!("flow.api:{}:{person}:{key}", flow.id)),
        None => Uuid::now_v7(),
    };
    let payload = EnrollJobPayload {
        workspace: workspace.id,
        flow: flow.id,
        version: None,
        trigger: TriggerKind::Api,
        work: EnrollWork::People(vec![(person, occurrence)]),
    };
    let unique = format!("flow.enroll:api:{}:{occurrence}", flow.id);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    enqueue(&mut *tx, &payload.job(&request_trace(), unique)).await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(QueuedBody {
            occurrence_id: occurrence,
        }),
    ))
}

/// Starts a flow whose trigger is `manual` for an audience: the current members of a segment or
/// the members of a list. Publisher and above, and audited as `flow.started`. The audience is queued as a
/// `flow.enroll` job that pages through it, a thousand people at a time, under the cause
/// `manual`. A paused flow takes nobody and records the skip.
#[utoipa::path(
    post,
    operation_id = "start_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/start",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = StartFlowRequest,
    responses(
        (status = 202, description = "Queued", body = QueuedBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a publisher, a key without the publish scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow, segment or list, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The flow is not published or paused, or its trigger is not `manual` (`code` is `flow_state`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The body names no audience or both", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn start(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<StartFlowRequest>,
) -> Result<(StatusCode, Json<QueuedBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Publish).await?;
    let id = path_id(&id)?;
    let flow = startable(&state, workspace.id, id, "manual").await?;
    let source = match (body.segment_id, body.list_id) {
        (Some(segment), None) => {
            zirv_flow_store::find_segment(&state.pool, workspace.id, segment)
                .await?
                .filter(|segment| segment.archived_at.is_none())
                .ok_or_else(ApiError::not_found)?;
            AudienceSource::Segment(segment)
        }
        (None, Some(list)) => {
            zirv_flow_store::find_list(&state.pool, workspace.id, list)
                .await?
                .ok_or_else(ApiError::not_found)?;
            AudienceSource::List(list)
        }
        _ => {
            return Err(ApiError::unprocessable(
                "body: must give either segment_id or list_id",
            ));
        }
    };
    let occurrence = Uuid::now_v7();
    let cause = match principal.user_id() {
        Some(user) => Cause::manual(user),
        None => request_trace().cause,
    };
    let trace = TraceContext {
        trace_id: TraceId::new(),
        cause,
    };
    let payload = EnrollJobPayload {
        workspace: workspace.id,
        flow: flow.id,
        version: None,
        trigger: TriggerKind::Manual,
        work: EnrollWork::Audience {
            source,
            occurrence: Some(occurrence),
            after: None,
            entered_by: None,
        },
    };
    let unique = format!("flow.enroll:manual:{}:{occurrence}", flow.id);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    enqueue(&mut *tx, &payload.job(&trace, unique)).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "flow.started",
        Some(("flow", flow.id)),
        json!({ "audience": source.to_value() }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(QueuedBody {
            occurrence_id: occurrence,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_names_a_segment_or_a_list() {
        let by_segment: StartFlowRequest =
            serde_json::from_str(r#"{"segment_id": "00000000-0000-7000-8000-000000000000"}"#)
                .unwrap();
        let by_list: StartFlowRequest =
            serde_json::from_str(r#"{"list_id": "00000000-0000-7000-8000-000000000000"}"#).unwrap();

        assert!(by_segment.segment_id.is_some() && by_segment.list_id.is_none());
        assert!(by_list.list_id.is_some() && by_list.segment_id.is_none());
    }

    #[test]
    fn an_enroll_names_a_person_or_an_identifier_and_may_carry_a_key() {
        let body: EnrollPersonRequest = serde_json::from_str(
            r#"{"identifier": {"kind": "email", "value": "a@b.c"}, "idempotency_key": "k"}"#,
        )
        .unwrap();

        assert!(body.person_id.is_none() && body.identifier.is_some());
        assert_eq!(body.idempotency_key.as_deref(), Some("k"));
    }
}
