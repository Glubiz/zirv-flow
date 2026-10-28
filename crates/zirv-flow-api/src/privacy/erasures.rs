//! Erasure requests (architecture section 16.3): record the request in the ledger and enqueue
//! `privacy.erase`, and list the requests. The listing never shows identifier hashes.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{HmacKey, TraceId, WorkspaceId};
use zirv_flow_store::{ErasureCall, ErasureRequest, PgPool};

use super::{PersonTarget, requested_by, resolve};
use crate::AppState;
use crate::admin::{self, AdminError, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::idempotency::{self, Keyed, ReplayedProblem, RequestHash};
use crate::jobs_admin::{encode_cursor, parse_page};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::trace;

#[derive(Serialize, ToSchema)]
pub(crate) struct ErasureBody {
    id: Uuid,
    /// The ledger keeps the person id after the person is gone.
    person_id: Uuid,
    /// `pending`, `completed`, or `failed` once the job ran out of attempts.
    #[schema(example = "pending")]
    status: String,
    /// The user who asked; null for an API key.
    requested_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    requested_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    completed_at: Option<OffsetDateTime>,
    /// The trace of the request and of its job: 32 lowercase hex characters.
    trace_id: String,
    /// The erase call made to each destination that declares erase and handled the person, with
    /// how it went. Empty until the erasure has run, and when no destination applies.
    destination_calls: Vec<ErasureCallBody>,
}

/// One destination's erase call for an erasure request. It names no person and no key.
#[derive(Serialize, ToSchema)]
pub(crate) struct ErasureCallBody {
    destination_id: Uuid,
    /// `pending`, `done` or `failed`.
    #[schema(example = "done")]
    outcome: String,
    /// Calls made so far; zero while pending.
    attempts: i32,
    /// A fixed code, such as `http_4xx`; set exactly when the call failed.
    error_code: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
}

impl From<ErasureCall> for ErasureCallBody {
    fn from(call: ErasureCall) -> Self {
        Self {
            destination_id: call.destination_id,
            outcome: call.outcome.as_str().to_owned(),
            attempts: call.attempts,
            error_code: call.error_code,
            created_at: call.created_at,
            updated_at: call.updated_at,
        }
    }
}

impl From<ErasureRequest> for ErasureBody {
    fn from(request: ErasureRequest) -> Self {
        Self {
            id: request.id,
            person_id: request.person_id,
            status: request.status.as_str().to_owned(),
            requested_by: request.requested_by,
            requested_at: request.requested_at,
            completed_at: request.completed_at,
            trace_id: TraceId::from_uuid(request.trace_id).to_string(),
            destination_calls: Vec::new(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ErasuresBody {
    /// Newest first.
    erasures: Vec<ErasureBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "erasure is unavailable: the server has no master key",
    )
}

/// Records an erasure request for the person the target names and enqueues the job, in one
/// transaction, audited as `privacy.erasure_requested` with the request id only.
pub(crate) async fn request(
    pool: &PgPool,
    key: &HmacKey,
    context: &Context,
    workspace: WorkspaceId,
    target: &PersonTarget,
    requested_by: Option<Uuid>,
    trace: &zirv_flow_core::TraceContext,
) -> Result<ErasureRequest, AdminError> {
    let person = resolve(pool, workspace, target).await?;
    let mut tx = zirv_flow_store::begin(pool).await?;
    let request =
        zirv_flow_store::request_erasure(&mut tx, workspace, person, key, requested_by, trace)
            .await?
            .ok_or(AdminError::NotFound("person"))?;
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "privacy.erasure_requested",
        Some(("erasure_request", request.id)),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(request)
}

/// Asks for a person to be erased: the ledger keeps the person id and the keyed hashes of
/// their identifiers, the email and phone stay suppressed as `erased`, and the person and
/// everything that refers to them is deleted by a background job. The request is given as a
/// `person_id` or as an `identifier`; one that resolves to no person is a 404. Needs the
/// workspace admin role and the CSRF token. Audited as `privacy.erasure_requested`.
#[utoipa::path(
    post,
    operation_id = "request_erasure",
    path = "/api/v1/workspaces/{ws}/privacy/erasures",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours")
    ),
    request_body = PersonTarget,
    responses(
        (status = 202, description = "The request, pending until the job has run", body = ErasureBody),
        (status = 409, description = "The Idempotency-Key already requested an erasure (`idempotency_key_replayed`); resource_id is the request's id and the body is not kept, as it names the person", body = ReplayedProblem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The body names both or neither of person_id and identifier, the identifier is malformed, or the Idempotency-Key is invalid or reused (`idempotency_key_reused`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PersonTarget>,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let key = state.identifier_key.as_deref().ok_or_else(unavailable)?;
    let trace = trace::current().ok_or_else(ApiError::internal)?;
    let request_hash = RequestHash::of_json(&method, &uri, &body)?;
    let keyed = Keyed::of(&headers, "request_erasure")?;
    let guard = match idempotency::enter(&state.pool, workspace.id, keyed, request_hash).await? {
        Ok(guard) => guard,
        Err(replay) => return Ok(replay),
    };
    let request = request(
        &state.pool,
        key,
        &principal.context(),
        workspace.id,
        &body,
        requested_by(&principal),
        &trace,
    )
    .await?;
    idempotency::finish(guard, request_hash, StatusCode::ACCEPTED, request.id, None).await?;
    Ok((StatusCode::ACCEPTED, Json(ErasureBody::from(request))).into_response())
}

/// The workspace's erasure requests, newest first, with their status.
#[utoipa::path(
    get,
    operation_id = "list_erasures",
    path = "/api/v1/workspaces/{ws}/privacy/erasures",
    tag = "privacy",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("limit" = Option<u32>, Query, description = "Requests per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of requests", body = ErasuresBody),
        (status = 400, description = "limit is out of range or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a key without the admin scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<ErasuresBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (limit, before) = parse_page(&query.unwrap_or_default())?;
    let (requests, more) =
        zirv_flow_store::list_erasure_requests(&state.pool, workspace.id, limit, before).await?;
    let next_before = match requests.last() {
        Some(last) if more => Some(encode_cursor((last.requested_at, last.id))),
        _ => None,
    };
    let ids: Vec<Uuid> = requests.iter().map(|request| request.id).collect();
    let mut calls = zirv_flow_store::list_erasure_calls(&state.pool, workspace.id, &ids).await?;
    let erasures = requests
        .into_iter()
        .map(|request| {
            let id = request.id;
            let mut body = ErasureBody::from(request);
            let (mine, rest): (Vec<_>, Vec<_>) =
                calls.drain(..).partition(|call| call.erasure_id == id);
            calls = rest;
            body.destination_calls = mine.into_iter().map(ErasureCallBody::from).collect();
            body
        })
        .collect();
    Ok(Json(ErasuresBody {
        erasures,
        next_before,
    }))
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::ErasureStatus;

    use super::*;

    #[test]
    fn a_request_is_shown_with_its_status_and_a_hex_trace_and_no_hashes() {
        let request = ErasureRequest {
            id: Uuid::nil(),
            person_id: Uuid::nil(),
            status: ErasureStatus::Failed,
            requested_by: None,
            requested_at: OffsetDateTime::UNIX_EPOCH,
            completed_at: None,
            trace_id: Uuid::nil(),
        };

        let shown = serde_json::to_value(ErasureBody::from(request)).unwrap();

        assert_eq!(shown["status"], "failed");
        assert_eq!(shown["requested_at"], "1970-01-01T00:00:00Z");
        assert!(shown["completed_at"].is_null());
        assert_eq!(shown["trace_id"].as_str().unwrap().len(), 32);
        assert!(shown.get("identifier_hashes").is_none());
    }

    #[test]
    fn without_a_master_key_the_answer_is_a_503() {
        assert_eq!(unavailable().status_code(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
