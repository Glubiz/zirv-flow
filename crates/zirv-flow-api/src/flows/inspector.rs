//! The flow enrollment inspector (architecture 12.3 and 12.4): a flow's enrollments, one
//! enrollment's path with its decisions, and the per-node counts of a version. All three need the
//! `read` role. They run in bounded snapshot reads; one that runs past its limit is a `503` with
//! the code `search_timeout`, and a bad parameter is a `400`.

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::TraceId;
use zirv_flow_core::flow::{FlowGraph, FlowSettings};
use zirv_flow_store::{
    EnrollmentCursor, EnrollmentFilter, EnrollmentPath, EnrollmentStatus, EnrollmentSummary,
    NodeCount, SETTINGS_GOAL_NODE,
};

use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::jobs_admin::{DEFAULT_LIMIT, parse_limit};
use crate::problem::{ApiError, Problem};
use crate::traces::DecisionBody;

/// The longest node id a filter may name (the graph's own bound).
const MAX_NODE_CHARS: usize = 200;

/// The node type of the settings goal's row.
const SETTINGS_GOAL_TYPE: &str = "settings.goal";

#[derive(Serialize, ToSchema)]
pub(crate) struct EnrollmentItemBody {
    id: Uuid,
    person_id: Uuid,
    /// The version the person entered by; an enrollment never moves to another.
    version: i32,
    /// `active`, `waiting`, `held`, `completed`, `exited` or `failed`.
    #[schema(example = "waiting")]
    state: &'static str,
    /// The node to run next, or the node the enrollment stopped at.
    node: String,
    /// The node a failure stopped at.
    failed_node: Option<String>,
    /// Why the enrollment ended other than by reaching an exit node.
    exit_reason: Option<String>,
    /// The error of a failure.
    error: Option<String>,
    /// When the timer of a wait fires.
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    wake_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    ended_at: Option<OffsetDateTime>,
    /// The trace of the enrollment's journey: `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    trace_id: String,
}

impl From<EnrollmentSummary> for EnrollmentItemBody {
    fn from(item: EnrollmentSummary) -> Self {
        Self {
            id: item.id,
            person_id: item.person_id,
            version: item.flow_version,
            state: item.status.as_str(),
            node: item.node,
            failed_node: item.failed_node,
            exit_reason: item.exit_reason,
            error: item.error,
            wake_at: item.wake_at,
            created_at: item.created_at,
            updated_at: item.updated_at,
            ended_at: item.ended_at,
            trace_id: TraceId::from_uuid(item.trace_id).to_string(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EnrollmentsBody {
    /// Newest first.
    enrollments: Vec<EnrollmentItemBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct StepBody {
    node_id: String,
    /// The node's type in the pinned version's graph (such as `wait.duration`); null for a node the
    /// version does not have.
    node_type: Option<String>,
    /// `done`, `waiting` or `failed`.
    #[schema(example = "done")]
    outcome: String,
    attempts: i32,
    error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    started_at: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum EnrollmentWaitBody {
    /// Asleep at a `wait.until_event`: the event, and when the wait gives up.
    Event {
        event: String,
        #[serde(with = "time::serde::rfc3339")]
        #[schema(value_type = String, format = DateTime)]
        timeout_at: OffsetDateTime,
    },
    /// Asleep at a timed wait: when it wakes.
    Timer {
        #[serde(with = "time::serde::rfc3339")]
        #[schema(value_type = String, format = DateTime)]
        wake_at: OffsetDateTime,
    },
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EnrollmentPathBody {
    enrollment: EnrollmentItemBody,
    /// The nodes the enrollment has run, in the order they started.
    steps: Vec<StepBody>,
    /// What the enrollment is waiting for now; empty when it is not waiting.
    waits: Vec<EnrollmentWaitBody>,
    /// The newest decisions that name the enrollment, oldest first: steps, branches, waits, wakes,
    /// failures, retries, skips, exits and goals.
    decisions: Vec<DecisionBody>,
    /// True when older decisions were left out because the enrollment has more than this
    /// response carries; the newest are always kept.
    truncated: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct NodeCountBody {
    /// A node id of the version's graph, or `settings.goal`.
    node_id: String,
    /// The node's type; `settings.goal` for the goal setting.
    node_type: String,
    /// Step executions of the node.
    entered: i64,
    /// Step executions that ended `done`.
    completed: i64,
    /// Enrollments of this version waiting at the node now.
    waiting: i64,
    /// Step executions that ended `failed`.
    failed: i64,
    /// For a `goal` node its completed count, for `settings.goal` its entered count; zero for the
    /// rest.
    goal_conversions: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct VersionCountsBody {
    version: i32,
    /// The nodes of the version's graph in their order, then `settings.goal` when the version has
    /// a goal setting. A node with no rows has zeros.
    nodes: Vec<NodeCountBody>,
}

/// Opaque to callers: base64url of the microseconds since the epoch and the id.
fn encode_cursor(cursor: EnrollmentCursor) -> String {
    let micros = cursor.created_at.unix_timestamp_nanos() / 1000;
    URL_SAFE_NO_PAD.encode(format!("{micros}:{}", cursor.id))
}

fn decode_cursor(text: &str) -> Option<EnrollmentCursor> {
    let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (micros, id) = text.split_once(':')?;
    let micros: i128 = micros.parse().ok()?;
    let created_at = OffsetDateTime::from_unix_timestamp_nanos(micros.checked_mul(1000)?).ok()?;
    Some(EnrollmentCursor {
        created_at,
        id: id.parse().ok()?,
    })
}

type Page = (EnrollmentFilter, u32, Option<EnrollmentCursor>);

fn parse_page(query: &str) -> Result<Page, ApiError> {
    let mut filter = EnrollmentFilter::default();
    let (mut limit, mut before) = (DEFAULT_LIMIT, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => limit = parse_limit(&value)?,
            "before" => {
                before = Some(
                    decode_cursor(&value)
                        .ok_or_else(|| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            "state" => {
                filter.state = Some(EnrollmentStatus::parse(&value).ok_or_else(|| {
                    ApiError::bad_request(
                        "state must be active, waiting, held, completed, exited or failed",
                    )
                })?);
            }
            "version" => {
                filter.version = Some(
                    value
                        .parse::<i32>()
                        .ok()
                        .filter(|version| *version > 0)
                        .ok_or_else(|| {
                            ApiError::bad_request("version must be a positive whole number")
                        })?,
                );
            }
            "node" => {
                if value.is_empty() || value.chars().count() > MAX_NODE_CHARS {
                    return Err(ApiError::bad_request(format!(
                        "node must be 1 to {MAX_NODE_CHARS} characters"
                    )));
                }
                filter.node = Some(value.into_owned());
            }
            _ => {}
        }
    }
    Ok((filter, limit, before))
}

/// The enrollments of a flow, newest first, in pages. Filter by `state`, by `version` and by
/// `node` (the node an enrollment is at, or the node it failed at).
#[utoipa::path(
    get,
    operation_id = "list_flow_enrollments",
    path = "/api/v1/workspaces/{ws}/flows/{id}/enrollments",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("limit" = Option<u32>, Query, description = "Enrollments per page, 1 to 200; default 50"),
        ("before" = Option<String>, Query, description = "The next_before of the previous page"),
        ("state" = Option<String>, Query, description = "`active`, `waiting`, `held`, `completed`, `exited` or `failed`"),
        ("version" = Option<i32>, Query, description = "Only enrollments pinned to this version"),
        ("node" = Option<String>, Query, description = "Only enrollments at this node or failed at it")
    ),
    responses(
        (status = 200, description = "A page of enrollments", body = EnrollmentsBody),
        (status = 400, description = "A parameter is out of range or malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace or flow, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Result<Json<EnrollmentsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let flow: Uuid = path_id(&id)?;
    let (filter, limit, before) = parse_page(query.as_deref().unwrap_or_default())?;
    let page = zirv_flow_store::list_flow_enrollments(
        &state.pool,
        workspace.id,
        flow,
        &filter,
        limit,
        before,
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(EnrollmentsBody {
        enrollments: page.enrollments.into_iter().map(Into::into).collect(),
        next_before: page.next.map(encode_cursor),
    }))
}

/// The graph of the version an enrollment is pinned to, for node types. One that no longer parses
/// is a fault of ours.
async fn graph_of(
    state: &AppState,
    workspace: zirv_flow_core::WorkspaceId,
    flow: Uuid,
    version: i32,
) -> Result<Option<(FlowGraph, FlowSettings)>, ApiError> {
    let Some(stored) =
        zirv_flow_store::find_flow_version(&state.pool, workspace, flow, version).await?
    else {
        return Ok(None);
    };
    let graph = FlowGraph::from_value(stored.graph).map_err(|_| ApiError::internal())?;
    let settings: FlowSettings =
        serde_json::from_value(stored.settings).map_err(|_| ApiError::internal())?;
    Ok(Some((graph, settings)))
}

fn node_type(graph: Option<&FlowGraph>, node: &str) -> Option<String> {
    if node == SETTINGS_GOAL_NODE {
        return Some(SETTINGS_GOAL_TYPE.to_owned());
    }
    graph?
        .node(node)
        .map(|found| found.kind.type_name().to_owned())
}

/// What the enrollment is waiting for now: the event wait rows, or else the timer it sleeps on.
fn waits_of(path: &EnrollmentPath) -> Vec<EnrollmentWaitBody> {
    if !path.event_waits.is_empty() {
        return path
            .event_waits
            .iter()
            .map(|wait| EnrollmentWaitBody::Event {
                event: wait.event_name.clone(),
                timeout_at: wait.timeout_at,
            })
            .collect();
    }
    let sleeping = matches!(
        path.enrollment.status,
        EnrollmentStatus::Waiting | EnrollmentStatus::Held
    );
    match path.enrollment.wake_at {
        Some(wake_at) if sleeping => vec![EnrollmentWaitBody::Timer { wake_at }],
        _ => Vec::new(),
    }
}

/// One enrollment of a flow: where it is, the nodes it ran in order, what it waits for, and every
/// decision recorded about it. An enrollment of another flow or workspace is 404.
#[utoipa::path(
    get,
    operation_id = "get_flow_enrollment",
    path = "/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("enrollment_id" = Uuid, Path, description = "Enrollment id")
    ),
    responses(
        (status = 200, description = "The enrollment's path", body = EnrollmentPathBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or enrollment, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, enrollment)): Path<(String, String, String)>,
) -> Result<Json<EnrollmentPathBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (flow, enrollment): (Uuid, Uuid) = (path_id(&id)?, path_id(&enrollment)?);
    let path = zirv_flow_store::load_enrollment_path(&state.pool, workspace.id, flow, enrollment)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let pinned = graph_of(&state, workspace.id, flow, path.enrollment.flow_version).await?;
    let graph = pinned.as_ref().map(|(graph, _)| graph);
    let waits = waits_of(&path);
    let steps = path
        .steps
        .iter()
        .map(|step| StepBody {
            node_id: step.node_id.clone(),
            node_type: node_type(graph, &step.node_id),
            outcome: step.outcome.clone(),
            attempts: step.attempts,
            error: step.error.clone(),
            started_at: step.started_at,
        })
        .collect();
    Ok(Json(EnrollmentPathBody {
        enrollment: path.enrollment.into(),
        steps,
        waits,
        decisions: path.decisions.into_iter().map(DecisionBody::from).collect(),
        truncated: path.truncated,
    }))
}

fn count_row(node_id: &str, node_type: String, count: Option<&NodeCount>) -> NodeCountBody {
    let count = count.cloned().unwrap_or_default();
    let goal_conversions = if node_type == "goal" {
        count.completed
    } else if node_id == SETTINGS_GOAL_NODE {
        count.entered
    } else {
        0
    };
    NodeCountBody {
        node_id: node_id.to_owned(),
        node_type,
        entered: count.entered,
        completed: count.completed,
        waiting: count.waiting,
        failed: count.failed,
        goal_conversions,
    }
}

/// How many enrollments entered, finished, wait at and failed at each node of one version,
/// computed from the rows when asked.
#[utoipa::path(
    get,
    operation_id = "get_flow_version_counts",
    path = "/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts",
    tag = "flows",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id"),
        ("n" = i32, Path, description = "Version number")
    ),
    responses(
        (status = 200, description = "The counts per node", body = VersionCountsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the read scope", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The read ran past its time limit (`code` is `search_timeout`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn counts(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id, n)): Path<(String, String, String)>,
) -> Result<Json<VersionCountsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Read).await?;
    let (flow, version): (Uuid, i32) = (path_id(&id)?, path_id(&n)?);
    let (graph, settings) = graph_of(&state, workspace.id, flow, version)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let counts = zirv_flow_store::version_node_counts(&state.pool, workspace.id, flow, version)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let mut nodes: Vec<NodeCountBody> = graph
        .nodes
        .iter()
        .map(|node| {
            count_row(
                &node.id,
                node.kind.type_name().to_owned(),
                counts.get(node.id.as_str()),
            )
        })
        .collect();
    if settings.goal.is_some() {
        nodes.push(count_row(
            SETTINGS_GOAL_NODE,
            SETTINGS_GOAL_TYPE.to_owned(),
            counts.get(SETTINGS_GOAL_NODE),
        ));
    }
    Ok(Json(VersionCountsBody { version, nodes }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use zirv_flow_core::flow::NodeKind;

    use super::*;

    #[test]
    fn a_cursor_round_trips_to_the_microsecond() {
        let created_at =
            OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap();
        let cursor = EnrollmentCursor {
            created_at,
            id: Uuid::now_v7(),
        };

        let text = encode_cursor(cursor);

        assert_eq!(decode_cursor(&text), Some(cursor));
        assert!(!text.contains(['=', '+', '/']));
    }

    #[test]
    fn a_malformed_cursor_does_not_decode() {
        for bad in [
            String::new(),
            "!!".to_owned(),
            URL_SAFE_NO_PAD.encode("1"),
            URL_SAFE_NO_PAD.encode("x:00000000-0000-0000-0000-000000000000"),
            URL_SAFE_NO_PAD.encode("1:not-a-uuid"),
        ] {
            assert_eq!(decode_cursor(&bad), None, "{bad}");
        }
    }

    #[test]
    fn the_page_defaults_and_takes_every_filter() {
        let (filter, limit, before) = parse_page("").unwrap();
        assert_eq!(filter, EnrollmentFilter::default());
        assert_eq!((limit, before), (50, None));
        let (filter, limit, _) =
            parse_page("state=failed&version=2&node=send&limit=200&other=x").unwrap();
        assert_eq!(filter.state, Some(EnrollmentStatus::Failed));
        assert_eq!(filter.version, Some(2));
        assert_eq!(filter.node.as_deref(), Some("send"));
        assert_eq!(limit, 200);
    }

    #[test]
    fn a_bad_parameter_is_a_400() {
        let long = format!("node={}", "n".repeat(201));
        for bad in [
            "limit=0",
            "limit=201",
            "limit=x",
            "before=%21",
            "state=asleep",
            "state=",
            "version=0",
            "version=-1",
            "version=x",
            "node=",
            long.as_str(),
        ] {
            assert_eq!(
                parse_page(bad).unwrap_err().status(),
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_goal_node_counts_its_completions_and_the_settings_goal_its_rows() {
        let count = NodeCount {
            entered: 5,
            completed: 3,
            waiting: 1,
            failed: 1,
        };

        assert_eq!(
            count_row("g", "goal".into(), Some(&count)).goal_conversions,
            3
        );
        assert_eq!(
            count_row(SETTINGS_GOAL_NODE, SETTINGS_GOAL_TYPE.into(), Some(&count)).goal_conversions,
            5
        );
        assert_eq!(
            count_row("a", "exit".into(), Some(&count)).goal_conversions,
            0
        );
        let empty = count_row("a", "exit".into(), None);
        assert_eq!(
            (empty.entered, empty.completed, empty.waiting, empty.failed),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn a_node_type_comes_from_the_pinned_graph() {
        let graph = FlowGraph::from_value(serde_json::json!({
            "v": 1,
            "nodes": [{"id": "end", "type": "exit"}],
            "edges": []
        }))
        .unwrap();

        assert_eq!(node_type(Some(&graph), "end").as_deref(), Some("exit"));
        assert_eq!(node_type(Some(&graph), "gone"), None);
        assert_eq!(node_type(None, "end"), None);
        assert_eq!(
            node_type(None, SETTINGS_GOAL_NODE).as_deref(),
            Some(SETTINGS_GOAL_TYPE)
        );
        assert!(matches!(graph.nodes[0].kind, NodeKind::Exit {}));
    }
}
