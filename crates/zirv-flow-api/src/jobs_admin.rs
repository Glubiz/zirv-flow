//! Jobs administration for the instance owner: queue depth, dead letters and retry.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{Cause, TraceContext, WorkspaceId};
use zirv_flow_store::{DeadCursor, JobSummary, KindDepth, NewDecision};

use crate::auth::{Principal, path_id};
use crate::problem::{ApiError, Problem};
use crate::traces::CauseBody;
use crate::{AppState, admin};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

#[derive(Serialize, ToSchema)]
pub(crate) struct DepthBody {
    /// Sorted by kind.
    kinds: Vec<KindDepthBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct KindDepthBody {
    #[schema(example = "maintenance.noop")]
    kind: String,
    ready: i64,
    running: i64,
    dead: i64,
    /// How long the longest-waiting runnable job has waited; null when none is runnable.
    oldest_ready_seconds: Option<f64>,
}

impl From<KindDepth> for KindDepthBody {
    fn from(depth: KindDepth) -> Self {
        Self {
            kind: depth.kind,
            ready: depth.ready,
            running: depth.running,
            dead: depth.dead,
            oldest_ready_seconds: depth.oldest_ready_seconds,
        }
    }
}

/// A job without its payload.
#[derive(Serialize, ToSchema)]
pub(crate) struct JobBody {
    id: Uuid,
    /// Null for instance-level work.
    workspace_id: Option<Uuid>,
    kind: String,
    /// `dead` in the list; `ready` after a retry.
    #[schema(example = "dead")]
    state: String,
    attempts: i32,
    max_attempts: i32,
    last_error: Option<String>,
    /// The trace the job belongs to: 32 lowercase hex characters.
    trace_id: String,
    cause: CauseBody,
    /// When the job went dead; null once retried.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    dead_at: Option<OffsetDateTime>,
}

impl From<JobSummary> for JobBody {
    fn from(job: JobSummary) -> Self {
        Self {
            id: job.id,
            workspace_id: job.workspace_id,
            kind: job.kind,
            state: job.state,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            last_error: job.last_error,
            trace_id: job.trace_id.to_string(),
            cause: CauseBody::new(job.cause.kind.to_string(), job.cause.id),
            dead_at: job.dead_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DeadJobsBody {
    /// Newest first.
    jobs: Vec<JobBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

/// Job counts by kind and state, and how long the oldest runnable job has waited.
#[utoipa::path(
    get,
    operation_id = "get_job_depth",
    path = "/api/v1/admin/jobs",
    tag = "jobs-admin",
    responses(
        (status = 200, description = "Queue depth per kind", body = DepthBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the instance owner, or an API key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn depth(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<DepthBody>, ApiError> {
    principal.require_owner()?;
    let kinds = zirv_flow_store::kind_depths(&state.pool).await?;
    Ok(Json(DepthBody {
        kinds: kinds.into_iter().map(KindDepthBody::from).collect(),
    }))
}

/// Dead jobs, newest first, without their payloads.
#[utoipa::path(
    get,
    operation_id = "list_dead_jobs",
    path = "/api/v1/admin/jobs/dead",
    tag = "jobs-admin",
    params(
        ("limit" = Option<u32>, Query, description = "Jobs per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page")
    ),
    responses(
        (status = 200, description = "A page of dead jobs", body = DeadJobsBody),
        (status = 400, description = "limit is out of range or before is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the instance owner, or an API key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn dead(
    State(state): State<AppState>,
    principal: Principal,
    RawQuery(query): RawQuery,
) -> Result<Json<DeadJobsBody>, ApiError> {
    principal.require_owner()?;
    let (limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let (jobs, more) = zirv_flow_store::list_dead_jobs(&state.pool, limit, before).await?;
    let next_before = match jobs.last() {
        Some(last) if more => last
            .dead_at
            .map(|dead_at| encode_cursor((dead_at, last.id))),
        _ => None,
    };
    Ok(Json(DeadJobsBody {
        jobs: jobs.into_iter().map(JobBody::from).collect(),
        next_before,
    }))
}

/// Puts a dead job back in the queue, to run again now under its original trace.
///
/// Resets its attempts and clears its error. Audited as `job.retried`; a workspace job also
/// gets a `job.retry` decision record in its trace.
#[utoipa::path(
    post,
    operation_id = "retry_job",
    path = "/api/v1/admin/jobs/{id}/retry",
    tag = "jobs-admin",
    params(("id" = Uuid, Path, description = "Job id")),
    responses(
        (status = 200, description = "The job, now ready", body = JobBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not the instance owner, an API key, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such job, or it is not dead", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "An equivalent job is already queued", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn retry(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<JobBody>, ApiError> {
    let owner = principal.require_owner()?.user.id;
    let id: Uuid = path_id(&id)?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let job = match zirv_flow_store::retry_dead_job(&mut tx, id).await {
        Ok(Some(job)) => job,
        Ok(None) => return Err(ApiError::not_found()),
        Err(error) if error.is_conflict() => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "an equivalent job is already queued",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let workspace = job.workspace_id.map(WorkspaceId::from_uuid);
    admin::audit(
        &mut tx,
        &principal.context(),
        workspace,
        "job.retried",
        Some(("job", job.id)),
        json!({ "kind": job.kind }),
    )
    .await?;
    if let Some(workspace_id) = workspace {
        zirv_flow_store::append_decision(
            &mut *tx,
            &NewDecision {
                workspace_id,
                trace: TraceContext {
                    trace_id: job.trace_id,
                    cause: Cause::manual(owner),
                },
                action: "job.retry",
                outcome: "requeued",
                reason: None,
                subject_ids: Vec::new(),
                job: Some((job.id, job.attempts)),
            },
        )
        .await?;
    }
    zirv_flow_store::commit(tx).await?;
    Ok(Json(JobBody::from(job)))
}

fn parse_page(query: &str) -> Result<(u32, Option<DeadCursor>), ApiError> {
    let mut limit = DEFAULT_LIMIT;
    let mut before = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                limit = value
                    .parse::<u32>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| {
                        ApiError::bad_request(format!("limit must be from 1 to {MAX_LIMIT}"))
                    })?;
            }
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            _ => {}
        }
    }
    Ok((limit, before))
}

/// Opaque to callers: base64url of the microseconds since the epoch and the job id.
fn encode_cursor((dead_at, id): DeadCursor) -> String {
    let micros = dead_at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{id}"))
}

fn decode_cursor(cursor: &str) -> Option<DeadCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (micros, id) = text.split_once(':')?;
    let nanos = micros.parse::<i128>().ok()?.checked_mul(1000)?;
    let dead_at = OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()?;
    Some((dead_at, id.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::TraceId;

    use super::*;

    #[test]
    fn a_cursor_round_trips_to_the_microsecond() {
        let dead_at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap();
        let id = Uuid::new_v4();

        let cursor = encode_cursor((dead_at, id));

        assert_eq!(decode_cursor(&cursor), Some((dead_at, id)));
        assert!(!cursor.contains(['=', '+', '/']));
    }

    #[test]
    fn a_malformed_cursor_does_not_decode() {
        let overflowing = format!("1{}:{}", "0".repeat(36), Uuid::nil());
        for bad in [
            "",
            "!!",
            "bm9wZQ",
            &URL_SAFE_NO_PAD.encode("1:not-a-uuid"),
            &URL_SAFE_NO_PAD.encode(overflowing),
        ] {
            assert_eq!(decode_cursor(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_page_defaults_to_fifty_and_limits_outside_one_to_two_hundred_are_refused() {
        assert_eq!(parse_page("").unwrap(), (50, None));
        assert_eq!(parse_page("limit=1").unwrap().0, 1);
        assert_eq!(parse_page("limit=200&x=y").unwrap().0, 200);
        for bad in ["limit=0", "limit=201", "limit=-1", "limit=abc", "limit="] {
            let error = parse_page(bad).unwrap_err();
            assert_eq!(error.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(
            parse_page("before=%21").unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_job_is_shown_without_a_payload_with_its_trace_cause_and_times() {
        let trace_id = TraceId::new();
        let job = JobSummary {
            id: Uuid::nil(),
            workspace_id: None,
            kind: "maintenance.noop".into(),
            state: "dead".into(),
            attempts: 5,
            max_attempts: 5,
            last_error: Some("boom".into()),
            trace_id,
            cause: Cause::api("req-1"),
            dead_at: Some(OffsetDateTime::UNIX_EPOCH),
        };

        let json = serde_json::to_value(JobBody::from(job)).unwrap();

        assert_eq!(json["trace_id"], trace_id.to_string());
        assert_eq!(json["trace_id"].as_str().unwrap().len(), 32);
        assert_eq!(json["cause"], json!({"kind": "api", "id": "req-1"}));
        assert_eq!(json["dead_at"], "1970-01-01T00:00:00Z");
        assert!(json.get("payload").is_none());
    }
}
