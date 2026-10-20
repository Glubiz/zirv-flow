//! The decision log of one trace.

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{CauseKind, TraceId};
use zirv_flow_store::Decision;

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem};

/// The most records one response carries.
const MAX_RECORDS: u32 = 1000;

#[derive(Serialize, ToSchema)]
pub(crate) struct TraceBody {
    /// The trace id: 32 lowercase hex characters.
    trace_id: String,
    /// Oldest first.
    records: Vec<DecisionBody>,
    /// True when the trace has more records than this response carries.
    truncated: bool,
}

/// What kind of thing caused the work (architecture 12.4). Mirrors the core `CauseKind`, which is
/// pure and has no schema derive; the unit test below fails to compile if a kind is added there
/// and not here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CauseKindBody {
    Event,
    TraitChange,
    Schedule,
    Manual,
    Api,
    DefinitionChange,
    SyncRun,
}

impl From<CauseKind> for CauseKindBody {
    fn from(kind: CauseKind) -> Self {
        match kind {
            CauseKind::Event => Self::Event,
            CauseKind::TraitChange => Self::TraitChange,
            CauseKind::Schedule => Self::Schedule,
            CauseKind::Manual => Self::Manual,
            CauseKind::Api => Self::Api,
            CauseKind::DefinitionChange => Self::DefinitionChange,
            CauseKind::SyncRun => Self::SyncRun,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CauseBody {
    kind: CauseKindBody,
    /// What caused the work: a request id for `api`, otherwise a record id.
    id: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DecisionBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    recorded_at: OffsetDateTime,
    #[schema(example = "job.attempt")]
    action: String,
    #[schema(example = "failed")]
    outcome: String,
    reason: Option<String>,
    cause: CauseBody,
    /// Durable domain ids by name; never personal data.
    #[schema(value_type = Object)]
    subject_ids: serde_json::Value,
    job_id: Option<Uuid>,
    attempt: Option<i32>,
}

impl CauseBody {
    pub(crate) fn new(kind: CauseKind, id: String) -> Self {
        Self {
            kind: kind.into(),
            id,
        }
    }
}

impl From<Decision> for DecisionBody {
    fn from(decision: Decision) -> Self {
        Self {
            id: decision.id,
            recorded_at: decision.recorded_at,
            action: decision.action,
            outcome: decision.outcome,
            reason: decision.reason,
            cause: CauseBody {
                kind: decision.cause.kind.into(),
                id: decision.cause.id,
            },
            subject_ids: decision.subject_ids,
            job_id: decision.job_id,
            attempt: decision.attempt,
        }
    }
}

/// What was decided and done under one trace, in the workspace. A trace with no records in
/// this workspace is 404, whether or not it exists elsewhere.
#[utoipa::path(
    get,
    operation_id = "get_trace",
    path = "/api/v1/workspaces/{ws}/traces/{trace_id}",
    tag = "traces",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("trace_id" = String, Path, description = "Trace id: 32 lowercase hex characters, from the x-trace-id response header")
    ),
    responses(
        (status = 200, description = "The trace's records, at most 1000", body = TraceBody),
        (status = 400, description = "The trace id is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The API key lacks the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, no access to it, or no records for the trace in it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, trace)): Path<(String, String)>,
) -> Result<Json<TraceBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let trace_id: TraceId = trace
        .parse()
        .map_err(|error: zirv_flow_core::ValidationError| {
            ApiError::bad_request(error.to_string())
        })?;
    let (records, truncated) =
        zirv_flow_store::list_trace(&state.pool, workspace.id, trace_id, MAX_RECORDS).await?;
    if records.is_empty() {
        return Err(ApiError::not_found());
    }
    Ok(Json(TraceBody {
        trace_id: trace_id.to_string(),
        records: records.into_iter().map(DecisionBody::from).collect(),
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::Cause;

    use super::*;

    #[test]
    fn every_core_cause_kind_maps_to_the_schema_kind_with_the_same_wire_name() {
        let kinds = [
            CauseKind::Event,
            CauseKind::TraitChange,
            CauseKind::Schedule,
            CauseKind::Manual,
            CauseKind::Api,
            CauseKind::DefinitionChange,
            CauseKind::SyncRun,
        ];
        for kind in kinds {
            let wire = serde_json::to_value(CauseKindBody::from(kind)).unwrap();
            assert_eq!(wire, serde_json::json!(kind.as_str()));
        }
    }

    #[test]
    fn a_record_is_shown_with_its_cause_and_an_rfc3339_time() {
        let decision = Decision {
            id: Uuid::nil(),
            trace_id: TraceId::new(),
            recorded_at: OffsetDateTime::UNIX_EPOCH,
            action: "job.attempt".into(),
            outcome: "failed".into(),
            reason: Some("boom".into()),
            cause: Cause::api("req-1"),
            subject_ids: serde_json::json!({"segment_id": "x"}),
            job_id: None,
            attempt: Some(2),
        };

        let json = serde_json::to_value(DecisionBody::from(decision)).unwrap();

        assert_eq!(json["recorded_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            json["cause"],
            serde_json::json!({"kind": "api", "id": "req-1"})
        );
        assert_eq!(json["subject_ids"]["segment_id"], "x");
        assert_eq!(json["job_id"], serde_json::Value::Null);
        assert_eq!(json["attempt"], 2);
    }
}
