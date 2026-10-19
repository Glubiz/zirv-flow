//! Flow test mode (architecture 9.4, ruling 90fa390a): a stateless, deterministic walk of one
//! person through a draft, a version or an inline graph. It reads (a real person, the graph and
//! the catalog, in one read-only snapshot under a statement timeout) and writes nothing: no
//! enrollment, job, event, decision or audit row.
//!
//! Bounds, each a problem+json with a stable code: a script of at most
//! [`walk::MAX_SCRIPT_STEPS`] steps (`flow_test_script_too_long`), times that never go back
//! (`flow_test_script_invalid`), at most [`walk::MAX_STEPS`] engine steps
//! (`flow_test_step_limit`), a snapshot of at most `EXPLAIN_EVENT_CAP` events
//! (`flow_test_snapshot_too_large`), and the read of a real person under the segment preview's
//! statement timeout (503 `flow_test_timeout`).

pub(crate) mod body;
mod walk;

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use chrono_tz::Tz;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, NodeKind, validate_graph};
use zirv_flow_segments::Catalog;
use zirv_flow_segments::event_lower_bounds;
use zirv_flow_store::{EXPLAIN_EVENT_CAP, ExplainEventScope, StoreError};

use self::body::{ScriptStepBody, SnapshotBody, TestBody, TestRequest};
use self::walk::{
    MAX_SCRIPT_STEPS, MAX_STEPS, Refusal, Step, Subject, Walk, parse_condition, to_chrono, to_time,
};
use super::{draft_of, issue_problems, parse_graph};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, may_see_values, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::segments::catalog_of;
use crate::segments::explain::snapshot_of;

const REQUEST_INVALID: &str = "flow_test_request_invalid";
const SCRIPT_TOO_LONG: &str = "flow_test_script_too_long";
const SCRIPT_INVALID: &str = "flow_test_script_invalid";
const STEP_LIMIT: &str = "flow_test_step_limit";
const SNAPSHOT_TOO_LARGE: &str = "flow_test_snapshot_too_large";

fn request_invalid(detail: &str) -> ApiError {
    ApiError::flow_test_refused(REQUEST_INVALID, detail)
}

/// A read past its time limit is `flow_test_timeout`, never a 500.
fn read_error(error: StoreError) -> ApiError {
    if error.is_timeout() {
        ApiError::flow_test_timeout()
    } else {
        error.into()
    }
}

/// The script as the walk runs it, or the refusal for its length or a step that is not exactly
/// one thing.
fn script_of(script: &[ScriptStepBody]) -> Result<Vec<Step>, ApiError> {
    if script.len() > MAX_SCRIPT_STEPS {
        return Err(ApiError::flow_test_refused(
            SCRIPT_TOO_LONG,
            format!("a script has at most {MAX_SCRIPT_STEPS} steps"),
        ));
    }
    let instant = |at: OffsetDateTime| {
        to_chrono(at)
            .ok_or_else(|| ApiError::flow_test_refused(SCRIPT_INVALID, "a time is out of range"))
    };
    script
        .iter()
        .map(|step| match (step.advance_to, &step.inject) {
            (Some(at), None) => Ok(Step::AdvanceTo(instant(at)?)),
            (None, Some(inject)) => Ok(Step::Inject {
                name: inject.name.clone(),
                properties: inject.properties.clone(),
                at: instant(inject.at)?,
            }),
            _ => Err(ApiError::flow_test_refused(
                SCRIPT_INVALID,
                "a script step has exactly one of advance_to and inject",
            )),
        })
        .collect()
}

/// The events every branch condition of the graph can read, with the widest lower bound at the
/// start of the walk. Windows only move forward, so no later condition reads further back.
/// A condition that does not validate is left out: the walk stops there.
fn event_scope(
    graph: &FlowGraph,
    catalog: &Catalog,
    start: OffsetDateTime,
) -> Vec<ExplainEventScope> {
    let mut scope: BTreeMap<String, Option<OffsetDateTime>> = BTreeMap::new();
    for node in &graph.nodes {
        let NodeKind::Branch { conditions } = &node.kind else {
            continue;
        };
        for condition in conditions {
            let Ok(definition) = parse_condition(condition, catalog) else {
                continue;
            };
            for (name, bound) in event_lower_bounds(&definition, start) {
                scope
                    .entry(name)
                    .and_modify(|held| {
                        *held = match (*held, bound) {
                            (Some(held), Some(bound)) => Some(held.min(bound)),
                            _ => None,
                        };
                    })
                    .or_insert(bound);
            }
        }
    }
    scope
        .into_iter()
        .map(|(name, since)| ExplainEventScope { name, since })
        .collect()
}

/// The enrollment id a walk runs under when the request names none: a fixed function of the flow
/// and the person, so the same request gives the same walk.
fn derived_enrollment(flow: Uuid, person: Option<Uuid>) -> Uuid {
    let person = person.map_or(0, |id| id.as_u128().rotate_left(64));
    Uuid::from_u128(flow.as_u128() ^ person)
}

/// Walks one person through a flow on a virtual clock, with nothing executed and nothing
/// written.
///
/// The graph is the saved draft, or `version`, or an inline `graph` (a graph that fails the
/// structural publish checks is 422 `flow_invalid`). The person is `person_id` (read only, newest
/// events first up to the explain cap; needs the right to see personal values) or a `snapshot`,
/// synthetic or the one a previous response returned. The walk starts at `start_at` (the database
/// time by default) and applies the `script`: `{"advance_to": T}` moves the clock and fires
/// every due timer, `{"inject": {name, properties, at}}` delivers an event, which satisfies the
/// wait it names when its filters match and is added to the snapshot's events either way.
/// Branch conditions are decided by the reference evaluator over the snapshot with the walk's
/// own internal effects overlaid. A condition on an event name the snapshot capped stops the
/// walk with `halted.reason = unable_to_evaluate`, naming the condition and the cap, and the
/// partial path is returned. Webhook effects show the envelope that would be sent, never a
/// secret or a signature. The response is a pure function of the request and the data read, so
/// re-posting `snapshot` with the same `start_at`, `enrollment_id` and `script` gives the same
/// response.
#[utoipa::path(
    post,
    operation_id = "test_flow",
    path = "/api/v1/workspaces/{ws}/flows/{id}/test",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = TestRequest,
    responses(
        (status = 200, description = "The walk", body = TestBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, a missing CSRF token, or a real person for a caller who may not see personal values (`flow_test_values_forbidden`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow, version or person, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A request that is not one person and one graph (`flow_test_request_invalid`), a graph that is not a flow document (`flow_malformed`) or fails the structural checks (`flow_invalid`), or a bound: `flow_test_script_too_long`, `flow_test_script_invalid`, `flow_test_step_limit`, `flow_test_snapshot_too_large`", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Reading the person ran past its time limit (`flow_test_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn test(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<TestRequest>,
) -> Result<Json<TestBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Write).await?;
    let flow_id: Uuid = path_id(&id)?;
    let script = script_of(&body.script)?;
    if body.version.is_some() && body.graph.is_some() {
        return Err(request_invalid("give a version or a graph, not both"));
    }
    match (&body.person_id, &body.snapshot) {
        (Some(_), None) | (None, Some(_)) => {}
        _ => {
            return Err(request_invalid(
                "give exactly one of person_id and snapshot",
            ));
        }
    }
    if body.person_id.is_some() && !may_see_values(&state, &principal, workspace.id).await {
        return Err(ApiError::flow_test_values_forbidden());
    }
    if let Some(snapshot) = &body.snapshot
        && snapshot.events.len() > EXPLAIN_EVENT_CAP
    {
        return Err(ApiError::flow_test_refused(
            SNAPSHOT_TOO_LARGE,
            format!("a snapshot holds at most {EXPLAIN_EVENT_CAP} events"),
        ));
    }
    let given = body
        .snapshot
        .as_ref()
        .map(SnapshotBody::to_snapshot)
        .transpose()?;

    let mut tx =
        zirv_flow_store::begin_segment_snapshot(&state.pool, state.segment_preview_timeout)
            .await
            .map_err(read_error)?;
    let database_now = state.clock.now(&mut tx).await.map_err(read_error)?;
    let flow = zirv_flow_store::find_flow(&mut *tx, workspace.id, flow_id)
        .await
        .map_err(|error| read_error(error.into_bounded()))?
        .ok_or_else(ApiError::not_found)?;
    let (graph, settings, source, version) = match (body.version, body.graph) {
        (Some(number), _) => {
            let saved = zirv_flow_store::find_flow_version(&mut *tx, workspace.id, flow_id, number)
                .await
                .map_err(|error| read_error(error.into_bounded()))?
                .ok_or_else(ApiError::not_found)?;
            let graph = FlowGraph::from_value(saved.graph).map_err(|_| ApiError::internal())?;
            let settings: FlowSettings =
                serde_json::from_value(saved.settings).map_err(|_| ApiError::internal())?;
            (graph, settings, "version", Some(number))
        }
        (None, Some(inline)) => (parse_graph(inline)?.0, draft_of(&flow)?.1, "inline", None),
        (None, None) => {
            let (graph, settings) = draft_of(&flow)?;
            (graph, settings, "draft", None)
        }
    };
    let issues = validate_graph(&graph);
    if !issues.is_empty() {
        return Err(ApiError::flow_invalid(issue_problems(&issues)));
    }
    let (catalog, _) = catalog_of(
        zirv_flow_store::segment_catalog(&mut tx, workspace.id)
            .await
            .map_err(read_error)?,
    );
    let start_at = to_chrono(body.start_at.unwrap_or(database_now))
        .ok_or_else(|| request_invalid("start_at is out of range"))?;
    let named_person = body
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.person_id);
    let (snapshot, person_id, merged_from) = match (body.person_id, given) {
        (Some(requested), _) => {
            let scope = event_scope(&graph, &catalog, to_time(start_at));
            let person =
                zirv_flow_store::load_explain_person(&mut tx, workspace.id, requested, &scope)
                    .await
                    .map_err(read_error)?
                    .ok_or_else(ApiError::not_found)?;
            (snapshot_of(&person), Some(person.id), person.merged_from)
        }
        (None, Some(snapshot)) => (snapshot, named_person, None),
        (None, None) => return Err(ApiError::internal()),
    };
    drop(tx);

    let subject = Subject {
        flow_id,
        version,
        person_id,
        enrollment_id: body
            .enrollment_id
            .unwrap_or_else(|| derived_enrollment(flow_id, person_id)),
    };
    let mut used = SnapshotBody::of(&snapshot);
    used.person_id = person_id;
    let walked = walk::walk(Walk {
        graph: &graph,
        settings: &settings,
        catalog: &catalog,
        subject: &subject,
        zone: workspace.timezone.parse::<Tz>().unwrap_or(Tz::UTC),
        snapshot,
        start_at,
        script: &script,
        max_steps: MAX_STEPS,
        event_cap: EXPLAIN_EVENT_CAP,
    })
    .map_err(|refusal| match refusal {
        Refusal::StepLimit => ApiError::flow_test_refused(
            STEP_LIMIT,
            format!("a walk runs at most {MAX_STEPS} engine steps"),
        ),
        Refusal::ScriptOrder { index } => ApiError::flow_test_refused(
            SCRIPT_INVALID,
            format!("script step {index} goes back in time"),
        ),
        Refusal::NoEntry => ApiError::flow_invalid(Vec::new()),
    })?;
    Ok(Json(TestBody {
        flow_id,
        source,
        version,
        person_id,
        merged_from,
        enrollment_id: subject.enrollment_id,
        start_at: to_time(start_at),
        snapshot: used,
        path: walked.path,
        internal_effects: walked.internal_effects,
        external_effects: walked.external_effects,
        waits: walked.waits,
        wakes: walked.wakes,
        script: walked.script,
        final_state: walked.final_state,
        halted: walked.halted,
        person_after: walked.person_after,
        not_simulated: walked.not_simulated,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn step(value: serde_json::Value) -> ScriptStepBody {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_script_step_is_exactly_one_thing_and_the_script_is_bounded() {
        assert!(script_of(&[step(json!({"advance_to": "2025-01-01T00:00:00Z"}))]).is_ok());
        let neither = script_of(&[step(json!({}))]).unwrap_err();
        assert_eq!(neither.code(), Some(SCRIPT_INVALID));
        let both = script_of(&[step(json!({
            "advance_to": "2025-01-01T00:00:00Z",
            "inject": {"name": "x", "at": "2025-01-01T00:00:00Z"}
        }))])
        .unwrap_err();
        assert_eq!(both.code(), Some(SCRIPT_INVALID));

        let long = vec![step(json!({"advance_to": "2025-01-01T00:00:00Z"})); MAX_SCRIPT_STEPS + 1];
        assert_eq!(script_of(&long).unwrap_err().code(), Some(SCRIPT_TOO_LONG));
        assert!(script_of(&long[..MAX_SCRIPT_STEPS]).is_ok());
    }

    #[test]
    fn the_derived_enrollment_is_fixed_by_the_flow_and_the_person() {
        let flow = Uuid::from_u128(7);
        let person = Uuid::from_u128(9);

        assert_eq!(derived_enrollment(flow, None), flow);
        assert_eq!(
            derived_enrollment(flow, Some(person)),
            derived_enrollment(flow, Some(person))
        );
        assert_ne!(
            derived_enrollment(flow, Some(person)),
            derived_enrollment(flow, None)
        );
    }
}

#[cfg(test)]
mod acceptance;
