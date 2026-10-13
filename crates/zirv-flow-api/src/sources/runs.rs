//! Sync runs of a source: start, cancel, list and read. Admins start and cancel; members read,
//! rejects included. A response holds counters, codes and ids, and a reject carries a sample
//! masked down to the values of non-PII traits. Start and cancel are audited with ids only.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::mapping::{Mapping, reject_sample};
use zirv_flow_core::{TraceId, WorkspaceId};
use zirv_flow_store::{
    CancelOutcome, RunCursor, RunKind, RunReject, RunTrigger, SyncRun, cancel_run, find_run,
    find_source, find_stream, list_run_rejects, list_runs, list_trait_definitions, queue_run,
    queue_runs_for_source,
};

use crate::AppState;
use crate::admin::{self, Context};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, decode_cursor, encode_cursor, parse_limit};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// The rejects a run's detail shows.
const DETAIL_REJECTS: u32 = 100;

/// One sync run: a read of one stream.
#[derive(Serialize, ToSchema)]
pub(crate) struct RunBody {
    id: Uuid,
    source_id: Uuid,
    stream_id: Uuid,
    /// The name of the stream.
    stream: String,
    /// `queued`, `running`, `succeeded`, `failed` or `cancelled`.
    state: String,
    /// `manual` or `schedule`.
    trigger: String,
    /// `sync` for a normal run, or `reconcile` for a reconciliation: it lists the keys of an
    /// incremental stream, applies nothing and removes the live records the source no longer has.
    kind: String,
    /// The stream's sync mode when the run was queued: `full_refresh` or `incremental`.
    sync_mode: String,
    /// Records the source delivered and the run has committed.
    records_read: i64,
    records_applied: i64,
    /// Records that repeated what is stored already.
    records_unchanged: i64,
    records_rejected: i64,
    records_deleted: i64,
    /// The rejected share above which the run stops, once at least 100 records are read.
    reject_threshold: f64,
    /// Why a failed run failed, such as `reject_threshold`, `stream_not_mapped`,
    /// `stream_settings_invalid`, `source_error` or `job_failed`; never a value.
    error_code: Option<String>,
    /// How many times a worker started the run; more than 1 means an earlier attempt was lost
    /// and the run resumed from its checkpoint.
    attempt: i32,
    /// The trace of the run: 32 lowercase hex characters. Every change the run made is under it.
    trace_id: String,
    created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    finished_at: Option<OffsetDateTime>,
}

impl From<&SyncRun> for RunBody {
    fn from(run: &SyncRun) -> Self {
        Self {
            id: run.id,
            source_id: run.source_id,
            stream_id: run.stream_id,
            stream: run.stream_name.clone(),
            state: run.state.clone(),
            trigger: run.trigger.clone(),
            kind: run.run_kind.clone(),
            sync_mode: run.sync_mode.clone(),
            records_read: run.records_read,
            records_applied: run.records_applied,
            records_unchanged: run.records_unchanged,
            records_rejected: run.records_rejected,
            records_deleted: run.records_deleted,
            reject_threshold: run.reject_threshold,
            error_code: run.error_code.clone(),
            attempt: run.attempt,
            trace_id: TraceId::from_uuid(run.trace_id).to_string(),
            created_by: run.created_by,
            created_at: run.created_at,
            started_at: run.started_at,
            finished_at: run.finished_at,
        }
    }
}

/// A page of runs, newest first.
#[derive(Serialize, ToSchema)]
pub(crate) struct RunsBody {
    runs: Vec<RunBody>,
    /// Pass as `cursor` for the next page; null when there is none.
    next_cursor: Option<String>,
}

/// A field of a refused record and why it was refused.
#[derive(Serialize, ToSchema)]
pub(crate) struct RunRejectReasonBody {
    /// The field of the record; empty when no one field is at fault. Never its value.
    property: String,
    /// A static code such as `invalid_identifier`, `wrong_type` or `missing_identifier`.
    reason: String,
}

/// A record the run refused.
#[derive(Serialize, ToSchema)]
pub(crate) struct RunRejectBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    received_at: OffsetDateTime,
    /// The first reason code.
    reason: String,
    reasons: Vec<RunRejectReasonBody>,
    /// `{properties, masked}`: a value stays only when the stream's mapping sends its field to
    /// a trait the field registry marks non-PII. Every other value is null and its field is
    /// listed in `masked`.
    #[schema(value_type = Object)]
    sample: Value,
}

/// A run with its first 100 rejects.
#[derive(Serialize, ToSchema)]
pub(crate) struct RunDetailBody {
    #[serde(flatten)]
    run: RunBody,
    /// Oldest first, at most 100; `records_rejected` is the total.
    rejects: Vec<RunRejectBody>,
}

/// Which stream to run.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartRunRequest {
    /// The name of one stream. Without it every enabled stream with a mapping is run.
    stream: Option<String>,
    /// `sync` (the default) or `reconcile`. A reconciliation needs `stream`, which must be
    /// incremental.
    #[serde(default)]
    kind: RunKindBody,
}

/// What kind of run to start.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunKindBody {
    #[default]
    Sync,
    Reconcile,
}

impl RunKindBody {
    fn core(self) -> RunKind {
        match self {
            Self::Sync => RunKind::Sync,
            Self::Reconcile => RunKind::Reconcile,
        }
    }
}

/// The reject as shown: the stored sample, masked again against the registry as it is now, so
/// a trait marked PII after the reject was written hides its value from then on.
fn shown(
    reject: RunReject,
    mapping: Option<&Mapping>,
    plain: &dyn Fn(&str) -> bool,
) -> RunRejectBody {
    let stored = reject
        .sample
        .get("properties")
        .cloned()
        .unwrap_or(Value::Null);
    let sample = match mapping {
        Some(mapping) => {
            let masked = reject_sample(mapping, &stored, plain);
            json!({"properties": masked.properties, "masked": masked.masked})
        }
        None => {
            let names: Vec<String> = stored
                .as_object()
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default();
            let properties: serde_json::Map<String, Value> = names
                .iter()
                .map(|name| (name.clone(), Value::Null))
                .collect();
            json!({"properties": properties, "masked": names})
        }
    };
    let reasons = reject
        .reasons
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| RunRejectReasonBody {
                    property: item["property"].as_str().unwrap_or_default().to_owned(),
                    reason: item["reason"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    RunRejectBody {
        id: reject.id,
        received_at: reject.received_at,
        reason: reject.reason,
        reasons,
        sample,
    }
}

/// Starts runs, audits `sync_run.started` with their ids, and returns them.
pub(crate) async fn start_runs(
    state: &AppState,
    context: &Context,
    workspace: WorkspaceId,
    user: Option<zirv_flow_core::UserId>,
    source_id: Uuid,
    request: &StartRunRequest,
) -> Result<Vec<SyncRun>, ApiError> {
    let source = find_source(&state.pool, workspace, source_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if super::is_push(&source.kind) {
        return Err(super::push_source());
    }
    if request.kind == RunKindBody::Reconcile && request.stream.is_none() {
        return Err(ApiError::unprocessable(
            "a reconciliation runs one stream; name it in stream",
        ));
    }
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let runs = match &request.stream {
        Some(name) => {
            let stream = find_stream(&mut *tx, workspace, source_id, name)
                .await?
                .ok_or_else(ApiError::not_found)?;
            if stream.mapping.is_none() {
                return Err(ApiError::stream_not_mapped(
                    "the stream has no mapping; set one before running it",
                ));
            }
            if !stream.enabled {
                return Err(ApiError::stream_not_enabled());
            }
            if request.kind == RunKindBody::Reconcile && stream.sync_mode != "incremental" {
                return Err(ApiError::reconcile_needs_incremental());
            }
            let run = queue_run(
                &mut tx,
                workspace,
                source_id,
                stream.id,
                request.kind.core(),
                RunTrigger::Manual,
                user,
            )
            .await?
            .ok_or_else(ApiError::sync_run_active)?;
            vec![run]
        }
        None => {
            let fan = queue_runs_for_source(
                &mut tx,
                workspace,
                source_id,
                RunTrigger::Manual,
                user,
                false,
            )
            .await?
            .ok_or_else(ApiError::not_found)?;
            if fan.created.is_empty() {
                return Err(if fan.skipped_active > 0 {
                    ApiError::sync_run_active()
                } else {
                    ApiError::stream_not_mapped("no enabled stream has a mapping")
                });
            }
            fan.created
        }
    };
    let ids: Vec<Uuid> = runs.iter().map(|run| run.id).collect();
    admin::audit(
        &mut tx,
        context,
        Some(workspace),
        "sync_run.started",
        Some(("source", source_id)),
        json!({"runs": ids, "kind": request.kind.core().as_str()}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(runs)
}

/// Starts a sync run of one stream of the source, or of every enabled stream that has a
/// mapping. A run is queued and read by a worker; poll it with `GET .../runs/{run_id}`.
/// Workspace admins only. Audited as `sync_run.started`.
#[utoipa::path(
    post,
    operation_id = "start_sync_runs",
    path = "/api/v1/workspaces/{ws}/sources/{id}/runs",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id")
    ),
    request_body = StartRunRequest,
    responses(
        (status = 202, description = "The queued runs", body = Vec<RunBody>),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or stream, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The stream already has an active run (`code` is `sync_run_active`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The stream has no mapping (`code` is `stream_not_mapped`) or is not enabled (`stream_not_enabled`); a reconciliation of a stream that is not incremental (`reconcile_needs_incremental`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn start(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<StartRunRequest>,
) -> Result<(StatusCode, Json<Vec<RunBody>>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let runs = start_runs(
        &state,
        &principal.context(),
        workspace.id,
        principal.user_id(),
        path_id(&id)?,
        &body,
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(runs.iter().map(RunBody::from).collect()),
    ))
}

/// Cancels a queued or running run. A running run stops at its next batch: the batch in flight
/// is not applied and the checkpoint stays at the last committed one. Workspace admins only.
/// Audited as `sync_run.cancelled`.
#[utoipa::path(
    post,
    operation_id = "cancel_sync_run",
    path = "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id"),
        ("run_id" = Uuid, Path, description = "Run id")
    ),
    responses(
        (status = 200, description = "The cancelled run", body = RunBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a missing scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or run, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The run is not active", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, run_id)): Path<(String, String, String)>,
) -> Result<Json<RunBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let (source_id, run_id): (Uuid, Uuid) = (path_id(&id)?, path_id(&run_id)?);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let run = match cancel_run(&mut tx, workspace.id, source_id, run_id).await? {
        CancelOutcome::Cancelled(run) => run,
        CancelOutcome::NotActive => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the run is not queued or running",
            ));
        }
        CancelOutcome::NotFound => return Err(ApiError::not_found()),
    };
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "sync_run.cancelled",
        Some(("source", source_id)),
        json!({"run": run.id, "records_read": run.records_read}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(Json(RunBody::from(run.as_ref())))
}

/// The `stream`, `cursor` and `limit` of a runs query.
fn parse_query(query: &str) -> Result<(Option<String>, Option<RunCursor>, u32), ApiError> {
    let (mut stream, mut cursor, mut limit) = (None, None, DEFAULT_LIMIT);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "stream" => stream = Some(value.into_owned()),
            "limit" => limit = parse_limit(&value)?,
            "cursor" => {
                let (created_at, id) = decode_cursor(&value)
                    .ok_or_else(|| ApiError::bad_request("cursor is not a valid cursor"))?;
                cursor = Some(RunCursor { created_at, id });
            }
            _ => {}
        }
    }
    Ok((stream, cursor, limit))
}

/// The runs of a source, newest first. Members of the workspace may read them.
#[utoipa::path(
    get,
    operation_id = "list_sync_runs",
    path = "/api/v1/workspaces/{ws}/sources/{id}/runs",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id"),
        ("stream" = Option<String>, Query, description = "Only runs of the stream with this name"),
        ("limit" = Option<u32>, Query, description = "Runs per page, 1 to 200; default 50"),
        ("cursor" = Option<String>, Query, description = "The next_cursor of the previous page")
    ),
    responses(
        (status = 200, description = "A page of runs", body = RunsBody),
        (status = 400, description = "limit is out of range or cursor is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or source, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<RunsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let source_id: Uuid = path_id(&id)?;
    let (stream, cursor, limit) = parse_query(&query.unwrap_or_default())?;
    find_source(&state.pool, workspace.id, source_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (runs, more) = list_runs(
        &state.pool,
        workspace.id,
        source_id,
        stream.as_deref(),
        cursor,
        limit,
    )
    .await?;
    let next_cursor = match runs.last() {
        Some(last) if more => Some(encode_cursor((last.created_at, last.id))),
        _ => None,
    };
    Ok(Json(RunsBody {
        runs: runs.iter().map(RunBody::from).collect(),
        next_cursor,
    }))
}

/// One run with its counters, state, attempt, error code and trace id, and its first 100
/// rejects. Members of the workspace may read it.
#[utoipa::path(
    get,
    operation_id = "get_sync_run",
    path = "/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}",
    tag = "sources",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Source id"),
        ("run_id" = Uuid, Path, description = "Run id")
    ),
    responses(
        (status = 200, description = "The run", body = RunDetailBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, source or run, or no access", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, run_id)): Path<(String, String, String)>,
) -> Result<Json<RunDetailBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (source_id, run_id): (Uuid, Uuid) = (path_id(&id)?, path_id(&run_id)?);
    let run = find_run(&state.pool, workspace.id, source_id, run_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let rejects = list_run_rejects(&state.pool, workspace.id, run.id, DETAIL_REJECTS).await?;
    let stream =
        zirv_flow_store::find_stream_by_id(&state.pool, workspace.id, source_id, run.stream_id)
            .await?;
    let mapping: Option<Mapping> = stream
        .and_then(|stream| stream.mapping)
        .and_then(|value| serde_json::from_value(value).ok());
    let plain: hashbrown::HashSet<String> = list_trait_definitions(&state.pool, workspace.id)
        .await?
        .into_iter()
        .filter(|definition| !definition.archived && !definition.pii)
        .map(|definition| definition.key)
        .collect();
    let plain = |key: &str| plain.contains(key);
    Ok(Json(RunDetailBody {
        run: RunBody::from(&run),
        rejects: rejects
            .into_iter()
            .map(|reject| shown(reject, mapping.as_ref(), &plain))
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::mapping::{ColumnRule, Target};

    use super::*;

    #[test]
    fn the_query_reads_stream_limit_and_cursor() {
        let id = Uuid::now_v7();
        let cursor = encode_cursor((OffsetDateTime::UNIX_EPOCH, id));
        let (stream, parsed, limit) =
            parse_query(&format!("stream=a%20b&limit=7&cursor={cursor}")).unwrap();
        assert_eq!(stream.as_deref(), Some("a b"));
        assert_eq!(limit, 7);
        assert_eq!(parsed.unwrap().id, id);
        assert_eq!(parse_query("").unwrap().2, DEFAULT_LIMIT);
        assert!(parse_query("limit=0").is_err());
        assert!(parse_query("cursor=zz").is_err());
    }

    #[test]
    fn a_sample_is_masked_again_against_the_mapping_and_the_registry() {
        let mapping = Mapping {
            delete_when: None,
            key_column: None,
            event: None,
            columns: vec![ColumnRule {
                column: "plan".into(),
                target: Target::Trait { key: "plan".into() },
                transforms: Vec::new(),
            }],
        };
        let reject = RunReject {
            id: Uuid::nil(),
            received_at: OffsetDateTime::UNIX_EPOCH,
            reason: "x".into(),
            reasons: json!([]),
            sample: json!({"properties": {"plan": "pro", "email": "a@b.c"}, "masked": []}),
        };
        let open = shown(reject.clone(), Some(&mapping), &|key| key == "plan");
        let closed = shown(reject.clone(), Some(&mapping), &|_| false);
        let unmapped = shown(reject, None, &|_| true);
        assert_eq!(open.sample["properties"]["plan"], "pro");
        assert_eq!(open.sample["properties"]["email"], Value::Null);
        assert_eq!(closed.sample["properties"]["plan"], Value::Null);
        assert_eq!(unmapped.sample["properties"]["plan"], Value::Null);
    }
}
