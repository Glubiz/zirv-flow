//! Reject replay (architecture section 12.2): judge rejected events again after a schema or
//! policy fix. A dry run predicts the outcome of each reject and writes nothing; a real run
//! enqueues an `ingest.replay` job and audits it with counts. Admin only.
//!
//! Nothing here returns or audits a name, an identifier or a value.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::{Cause, TraceContext};
use zirv_flow_store::{MAX_REPLAY, ReplayPrediction, ReplaySelection};

use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::events::rejects::ReasonBody;
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};

/// Which rejects, other than by id.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ReplayFilter {
    /// Only rejects of this event name.
    name: Option<String>,
    /// Only rejects with this reason code.
    reason: Option<String>,
    /// Only rejects received at or after this time.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    from: Option<OffsetDateTime>,
    /// Only rejects received before this time.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    to: Option<OffsetDateTime>,
}

fn no() -> bool {
    false
}

/// Exactly one of `ids` and `filter`, at most 10,000 rejects.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ReplayRequest {
    ids: Option<Vec<Uuid>>,
    /// The workspace's open event rejects that match.
    filter: Option<ReplayFilter>,
    /// `manual` for a person's decision, or `definition_change` after a schema or policy
    /// fix. It is recorded on the `event.replayed` decisions.
    #[schema(example = "definition_change")]
    cause: String,
    /// Whether downstream event triggers may fire for the replayed events. Off by default:
    /// the events are flagged `no_triggers`, which the trigger path skips.
    #[serde(default = "no")]
    emit_triggers: bool,
    /// Predict the outcomes and write nothing. Default false.
    #[serde(default = "no")]
    dry_run: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PredictionBody {
    reject_id: Uuid,
    /// `accept`, `flag`, `still_rejected`, `duplicate` or `already_replayed`.
    outcome: String,
    /// For `still_rejected`: `validation_failed`, `unplanned_event`, `no_person` or
    /// `not_replayable`.
    reason: Option<String>,
    reasons: Vec<ReasonBody>,
    /// Properties the policy hashes that the schema types as something other than a string.
    /// The stored payload holds their hash, so they fail validation.
    redacted_type_changed: Vec<String>,
}

impl From<ReplayPrediction> for PredictionBody {
    fn from(prediction: ReplayPrediction) -> Self {
        Self {
            reject_id: prediction.reject_id,
            outcome: prediction.outcome.to_owned(),
            reason: prediction.reason.map(str::to_owned),
            reasons: prediction
                .reasons
                .into_iter()
                .map(|reason| ReasonBody::new(reason.property, reason.reason.as_str()))
                .collect(),
            redacted_type_changed: prediction.redacted_type_changed,
        }
    }
}

#[derive(Default, Serialize, ToSchema)]
pub(crate) struct TotalsBody {
    accept: u64,
    flag: u64,
    still_rejected: u64,
    duplicate: u64,
    already_replayed: u64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReplayBody {
    dry_run: bool,
    /// How many rejects the request covers.
    selected: usize,
    /// The predictions of a dry run, in the order of the rejects; empty for a real run.
    rejects: Vec<PredictionBody>,
    /// The predicted totals of a dry run; zero for a real run.
    totals: TotalsBody,
    /// The trace of a real run: its `event.replayed` decisions are on it. Null for a dry run.
    trace_id: Option<String>,
}

fn totals(predictions: &[PredictionBody]) -> TotalsBody {
    let mut totals = TotalsBody::default();
    for prediction in predictions {
        match prediction.outcome.as_str() {
            "accept" => totals.accept += 1,
            "flag" => totals.flag += 1,
            "still_rejected" => totals.still_rejected += 1,
            "duplicate" => totals.duplicate += 1,
            _ => totals.already_replayed += 1,
        }
    }
    totals
}

/// Replays rejected events after a schema or policy fix. With `dry_run` it validates each
/// reject's stored, redacted payload against the current schema and policy and predicts the
/// outcome, writing nothing. Without it, a job stores each event that now passes, once, with
/// the flag `replayed` (and `no_triggers` unless `emit_triggers`), sets `replayed_at` and
/// `replay_event_id` on the reject and records an `event.replayed` decision with the cause.
/// Replaying a reject again changes nothing. A reject that named no person cannot be
/// replayed (`still_rejected` with `no_person`).
#[utoipa::path(
    post,
    operation_id = "replay_ingest_rejects",
    path = "/api/v1/workspaces/{ws}/ingest-rejects/replay",
    tag = "events",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = ReplayRequest,
    responses(
        (status = 200, description = "A dry run: the predicted outcomes", body = ReplayBody),
        (status = 202, description = "The replay job was enqueued", body = ReplayBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, a key without the admin scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Not exactly one of ids and filter, an unknown cause, or more than 10,000 rejects", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn replay(
    State(state): State<crate::AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(request): ApiJson<ReplayRequest>,
) -> Result<(StatusCode, Json<ReplayBody>), ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let cause = match request.cause.as_str() {
        "manual" => match principal.user_id() {
            Some(user) => Cause::manual(user),
            None => Cause::api(crate::request_id::current().unwrap_or_default()),
        },
        "definition_change" => Cause::definition_change(Uuid::now_v7()),
        _ => {
            return Err(ApiError::unprocessable(
                "cause: must be manual or definition_change",
            ));
        }
    };
    let ids = match (&request.ids, &request.filter) {
        (Some(ids), None) => {
            zirv_flow_store::select_rejects(&state.pool, workspace.id, ReplaySelection::Ids(ids))
                .await?
        }
        (None, Some(filter)) => {
            zirv_flow_store::select_rejects(
                &state.pool,
                workspace.id,
                ReplaySelection::Filter {
                    name: filter.name.as_deref(),
                    reason: filter.reason.as_deref(),
                    from: filter.from,
                    to: filter.to,
                },
            )
            .await?
        }
        _ => {
            return Err(ApiError::unprocessable(
                "exactly one of ids and filter is required",
            ));
        }
    };
    let Some(ids) = ids else {
        return Err(ApiError::unprocessable(format!(
            "a replay covers at most {MAX_REPLAY} rejects"
        )));
    };

    if request.dry_run {
        let rejects: Vec<PredictionBody> =
            zirv_flow_store::preview_replay(&state.pool, workspace.id, &ids)
                .await?
                .into_iter()
                .map(PredictionBody::from)
                .collect();
        let totals = totals(&rejects);
        return Ok((
            StatusCode::OK,
            Json(ReplayBody {
                dry_run: true,
                selected: ids.len(),
                rejects,
                totals,
                trace_id: None,
            }),
        ));
    }

    let trace = TraceContext::root(cause);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::enqueue_replay(&mut *tx, workspace.id, &ids, request.emit_triggers, &trace)
        .await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        Some(workspace.id),
        "ingest_rejects.replayed",
        None,
        serde_json::json!({
            "selected": ids.len(),
            "cause": request.cause,
            "emit_triggers": request.emit_triggers,
        }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ReplayBody {
            dry_run: false,
            selected: ids.len(),
            rejects: Vec::new(),
            totals: TotalsBody::default(),
            trace_id: Some(trace.trace_id.to_string()),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::events::{Reason, ReasonCode};

    use super::*;

    #[test]
    fn a_request_defaults_to_no_triggers_and_a_real_run() {
        let request: ReplayRequest =
            serde_json::from_value(json!({"ids": [], "cause": "manual"})).unwrap();

        assert!(!request.emit_triggers);
        assert!(!request.dry_run);
    }

    #[test]
    fn a_prediction_shows_codes_and_property_names_only() {
        let prediction = ReplayPrediction {
            reject_id: Uuid::nil(),
            outcome: "still_rejected",
            reason: Some("validation_failed"),
            reasons: vec![Reason {
                property: "total".into(),
                reason: ReasonCode::WrongType,
            }],
            redacted_type_changed: vec!["total".into()],
        };

        let shown = serde_json::to_value(PredictionBody::from(prediction)).unwrap();

        assert_eq!(
            shown["reasons"],
            json!([{"property": "total", "reason": "wrong_type"}])
        );
        assert_eq!(shown["redacted_type_changed"], json!(["total"]));
        assert_eq!(shown["reason"], "validation_failed");
    }

    #[test]
    fn totals_count_each_outcome() {
        let body = |outcome: &str| PredictionBody {
            reject_id: Uuid::nil(),
            outcome: outcome.into(),
            reason: None,
            reasons: Vec::new(),
            redacted_type_changed: Vec::new(),
        };
        let predictions = [
            body("accept"),
            body("accept"),
            body("flag"),
            body("still_rejected"),
            body("duplicate"),
            body("already_replayed"),
        ];

        let counted = totals(&predictions);

        assert_eq!(
            (
                counted.accept,
                counted.flag,
                counted.still_rejected,
                counted.duplicate,
                counted.already_replayed
            ),
            (2, 1, 1, 1, 1)
        );
    }
}
