//! Explaining a flow and a trace with the assistant (architecture 13.2).
//!
//! `POST .../flows/{id}/ai/explain` asks the model, with the workspace's catalog and the flow's
//! draft or a published version (its personal literals masked), for a plain-language explanation,
//! checks it with the `explain_flow` tool, repairs it at most `ai.max_repair_attempts` times and
//! answers with it, every node cited by id.
//!
//! `POST .../traces/{trace_id}/ai/explain` does the same for a trace: the decision records of the
//! trace (the read-only trace API's records, at most 60 of them) are shown to the model by label,
//! checked with the `explain_trace` tool, and the answer names each record by its decision-record
//! id. No person's id and no value of any person is sent.
//!
//! Neither stores anything or writes anything: the usage of the call, which dispatch records, is
//! all there is. The call is made as the signed-in user, who must be a workspace admin: every
//! attempt spends the workspace's budget.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    BoxFuture, ChatRequest, ChatResponse, Privacy, RepairError, RepairOutcome, ToolError,
    ToolOutput, Verdict,
};
use zirv_flow_core::TraceId;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::Decision;

use super::dispatch::{DispatchError, Dispatcher};
use super::drafts::{DraftIssue, expected_call, segment_details, wrong_answer};
use super::flow_context;
use super::segment_context::Placeholders;
use super::tools::{self, DraftTarget, ToolContext};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem, SegmentIssueProblem};

/// The tool a flow explanation offers.
const FLOW_TOOL: &str = "explain_flow";
/// The tool a trace explanation offers.
const TRACE_TOOL: &str = "explain_trace";
/// The feature the call log records the model calls of a flow explanation under.
const FLOW_FEATURE: &str = "flow_explain";
/// The feature the call log records the model calls of a trace explanation under.
const TRACE_FEATURE: &str = "trace_explain";
/// The most records the trace read asks for (the trace API's own cap).
const MAX_RECORDS: u32 = 1000;

/// Which flow document to explain.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiFlowExplainRequest {
    /// The published version to explain; the flow's current draft when absent.
    #[serde(default)]
    version: Option<i32>,
}

/// What the model said about one node of the flow.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiExplainedFlowNode {
    /// The id of the node in the explained graph.
    id: String,
    /// Plain text. Show it as text, never as markup.
    text: String,
}

/// The explanation of a flow draft or version.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiFlowExplainBody {
    /// `draft` or `version`.
    source: &'static str,
    /// The version that was explained; absent for the draft.
    version: Option<i32>,
    /// Who enters the flow and what happens to them, in plain text.
    summary: String,
    /// One entry for every node but the exit nodes, and for some of those.
    nodes: Vec<AiExplainedFlowNode>,
}

/// What the model said about one decision record.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiExplainedRecord {
    /// The decision-record id, as in `GET .../traces/{trace_id}`.
    id: Uuid,
    /// Plain text. Show it as text, never as markup.
    text: String,
}

/// The explanation of a trace.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiTraceExplainBody {
    /// The trace id: 32 lowercase hex characters.
    trace_id: String,
    /// What happened under the trace, in plain text.
    summary: String,
    /// One entry for every record the model was shown, each citing the record's id.
    records: Vec<AiExplainedRecord>,
    /// How many later records of the trace the model was not shown (the trace is long).
    omitted_records: usize,
}

/// Reads the optional body: an empty one means the current draft.
fn read_request(body: &Bytes) -> Result<AiFlowExplainRequest, ApiError> {
    if body.is_empty() {
        return Ok(AiFlowExplainRequest::default());
    }
    serde_json::from_slice(body).map_err(|_| {
        ApiError::unprocessable("version: the body must be {} or {\"version\": <number>}")
    })
}

/// Checks one answer: exactly one call of `tool`, run through the registry as the signed-in user.
/// Anything else is feedback, and a refusal ends the run.
async fn explain_turn(
    context: &ToolContext,
    tool: &'static str,
    response: &ChatResponse,
) -> Verdict<Value, DraftIssue, ApiError> {
    let feedback = || Verdict::Invalid(vec![wrong_answer(tool)]);
    let Some(arguments) = expected_call(response, tool) else {
        return feedback();
    };
    let Some(found) = tools::find(tool) else {
        return feedback();
    };
    match found.call(context, arguments).await {
        Ok(ToolOutput::Draft(explained)) => Verdict::Valid(explained),
        Ok(ToolOutput::Context(_)) => feedback(),
        Err(ToolError::Invalid(lines)) => Verdict::Invalid(DraftIssue::from_lines(&lines)),
        Err(ToolError::Failed(error)) => Verdict::Abort(error),
    }
}

fn turn<'a>(
    context: Arc<ToolContext>,
    tool: &'static str,
    response: &'a ChatResponse,
) -> BoxFuture<'a, Verdict<Value, DraftIssue, ApiError>> {
    Box::pin(async move { explain_turn(&context, tool, response).await })
}

/// The issues of the last attempt as the problem lists them: a code and where.
fn problem_issues(issues: &[DraftIssue]) -> Vec<SegmentIssueProblem> {
    issues
        .iter()
        .map(|issue| SegmentIssueProblem {
            pointer: issue.at().unwrap_or_default().to_owned(),
            code: issue.code().to_owned(),
        })
        .collect()
}

/// Runs the repair loop of an explanation and returns the checked answer.
async fn run(
    state: &AppState,
    dispatcher: &Dispatcher<'_>,
    context: ToolContext,
    tool: &'static str,
    chat: &ChatRequest,
    what: &str,
) -> Result<Value, ApiError> {
    let context = Arc::new(context);
    let outcome = zirv_flow_ai::repair(
        state.ai.config.max_repair_attempts,
        chat,
        dispatcher,
        move |response| turn(Arc::clone(&context), tool, response),
    )
    .await;
    match outcome {
        Ok(RepairOutcome::Valid(explained, _)) => Ok(explained),
        Ok(RepairOutcome::Exhausted(issues, _)) => Err(ApiError::ai_explain_failed_for(
            what,
            problem_issues(&issues),
        )),
        Err(RepairError::Dispatch(error)) => Err(DispatchError::Ai(error).into()),
        Err(RepairError::Aborted(error)) => Err(error),
    }
}

fn tool_context(
    state: &AppState,
    principal: Principal,
    workspace: WorkspaceId,
    privacy: Privacy,
    target: DraftTarget,
) -> ToolContext {
    ToolContext {
        state: state.clone(),
        principal,
        workspace,
        privacy,
        target,
        sample: None,
    }
}

/// The explanation as the response: the model's texts with the literals it was not shown put back,
/// for the admin who owns the flow.
fn flow_body(
    source: &'static str,
    version: Option<i32>,
    explained: &Value,
    table: &Placeholders,
) -> AiFlowExplainBody {
    let text = |value: Option<&Value>| {
        table.restore_text(value.and_then(Value::as_str).unwrap_or_default())
    };
    AiFlowExplainBody {
        source,
        version,
        summary: text(explained.get("summary")),
        nodes: explained
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|node| AiExplainedFlowNode {
                id: node
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                text: text(node.get("text")),
            })
            .collect(),
    }
}

/// The explanation of a trace as the response.
fn trace_body(
    trace_id: TraceId,
    explained: &Value,
    omitted_records: usize,
) -> AiTraceExplainBody {
    AiTraceExplainBody {
        trace_id: trace_id.to_string(),
        summary: explained
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        records: explained
            .get("records")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|record| {
                Some(AiExplainedRecord {
                    id: record.get("id")?.as_str()?.parse().ok()?,
                    text: record
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })
            })
            .collect(),
        omitted_records,
    }
}

/// Explains a flow in plain language: its current draft, or a published version when `version` is
/// given. The model gets a summary to write and a sentence for every node but the exit nodes,
/// each named by its node id; its answer is checked (every id a node of the graph, every node
/// cited once, plain text with a length cap) and, when it fails, asked again at most
/// `ai.max_repair_attempts` times. Nothing is stored. Workspace admins who are signed in; an API
/// key is refused. Every attempt is a gated model call that counts against the budget and the
/// rate limit, and with the assistant off nothing is read or sent. The model is not told any
/// trait or event value, count, sample or member.
#[utoipa::path(
    post,
    operation_id = "explain_flow_with_ai",
    path = "/api/v1/workspaces/{ws}/flows/{id}/ai/explain",
    tag = "ai",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Flow id")
    ),
    request_body = Option<AiFlowExplainRequest>,
    responses(
        (status = 200, description = "The explanation", body = AiFlowExplainBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 402, description = "The workspace's monthly AI budget is spent (`ai_budget_exceeded`); nothing was sent", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, flow or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The assistant is off (`ai_disabled`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The model's explanation did not pass its checks within the repair attempts (`ai_explain_failed`, with the issue codes), a condition too large to send without personal data (`ai_flow_too_large`), or the body is not a version", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many AI calls (`ai_rate_limited`); try again in a minute", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The model provider failed (`ai_provider_error`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the provider key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn explain_flow(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<AiFlowExplainBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = principal.user_id().ok_or_else(ApiError::ai_user_required)?;
    let id: Uuid = path_id(&id)?;
    let request = read_request(&body)?;
    // The assistant being off is the first answer, before anything is read.
    let dispatcher = Dispatcher::open(&state, Some(user), workspace.id, FLOW_FEATURE)
        .await
        .map_err(ApiError::from)?;

    let flow = zirv_flow_store::find_flow(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let (source, version, document) = match request.version {
        Some(number) => {
            let saved = zirv_flow_store::find_flow_version(&state.pool, workspace.id, id, number)
                .await?
                .ok_or_else(ApiError::not_found)?;
            (
                "version",
                Some(number),
                serde_json::json!({"graph": saved.graph, "settings": saved.settings}),
            )
        }
        None => (
            "draft",
            None,
            serde_json::json!({"graph": flow.draft_graph, "settings": flow.draft_settings}),
        ),
    };

    let privacy = dispatcher.privacy();
    let details = segment_details(&state, workspace.id).await?;
    let (parts, table) = flow_context::flow_context(&details, "The flow", &document, privacy)
        .map_err(|_| ApiError::ai_flow_too_large())?;
    let tool = tools::find(FLOW_TOOL).ok_or_else(ApiError::internal)?;
    let chat = flow_context::build_explain_request(
        privacy,
        parts,
        tool.spec().map_err(|_| ApiError::internal())?,
    )
    .map_err(|error| ApiError::from(DispatchError::Ai(error)))?;
    let graph = document.get("graph").cloned().unwrap_or(Value::Null);
    let context = tool_context(
        &state,
        principal,
        workspace.id,
        privacy,
        DraftTarget::ExplainFlow {
            node_ids: flow_context::node_ids(&graph),
            required: flow_context::nodes_to_cite(&graph),
        },
    );
    let explained = run(&state, &dispatcher, context, FLOW_TOOL, &chat, "flow").await?;
    Ok(Json(flow_body(source, version, &explained, &table)))
}

/// Explains what happened under one trace in plain language: the first read-only consumer of the
/// trace API. The model is shown the decision records of the trace (at most 60, oldest first) by
/// label, with the action, outcome, attempt, job, cause and the ids each concerns, but no person's
/// id and no value of any person; a recorded reason is fenced as data. It writes a summary and a
/// sentence for every record shown, each citing the record, and the answer names the records by
/// their decision-record ids. It is checked (every record a record of the trace, cited once, plain
/// text with a length cap) and, when it fails, asked again at most `ai.max_repair_attempts`
/// times. Nothing is stored. Workspace admins who are signed in; an API key is refused. Every
/// attempt is a gated model call that counts against the budget and the rate limit, and with the
/// assistant off nothing is read or sent.
#[utoipa::path(
    post,
    operation_id = "explain_trace_with_ai",
    path = "/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain",
    tag = "ai",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("trace_id" = String, Path, description = "Trace id: 32 lowercase hex characters")
    ),
    responses(
        (status = 200, description = "The explanation, citing the decision records it describes", body = AiTraceExplainBody),
        (status = 400, description = "The trace id is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 402, description = "The workspace's monthly AI budget is spent (`ai_budget_exceeded`); nothing was sent", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, no access to it, or no records for the trace in it", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The assistant is off (`ai_disabled`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The model's explanation did not pass its checks within the repair attempts (`ai_explain_failed`, with the issue codes)", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many AI calls (`ai_rate_limited`); try again in a minute", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The model provider failed (`ai_provider_error`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the provider key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn explain_trace(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, trace)): Path<(String, String)>,
) -> Result<Json<AiTraceExplainBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = principal.user_id().ok_or_else(ApiError::ai_user_required)?;
    let trace_id: TraceId = trace
        .parse()
        .map_err(|error: zirv_flow_core::ValidationError| {
            ApiError::bad_request(error.to_string())
        })?;
    // The assistant being off is the first answer, before anything is read.
    let dispatcher = Dispatcher::open(&state, Some(user), workspace.id, TRACE_FEATURE)
        .await
        .map_err(ApiError::from)?;

    let (records, truncated) =
        zirv_flow_store::list_trace(&state.pool, workspace.id, trace_id, MAX_RECORDS).await?;
    if records.is_empty() {
        return Err(ApiError::not_found());
    }
    let shown: &[Decision] = flow_context::shown_records(&records);
    let omitted = records.len() - shown.len();
    let privacy = dispatcher.privacy();
    let tool = tools::find(TRACE_TOOL).ok_or_else(ApiError::internal)?;
    let chat = flow_context::build_trace_request(
        privacy,
        flow_context::trace_context(&records, truncated),
        tool.spec().map_err(|_| ApiError::internal())?,
    )
    .map_err(|error| ApiError::from(DispatchError::Ai(error)))?;
    let context = tool_context(
        &state,
        principal,
        workspace.id,
        privacy,
        DraftTarget::ExplainTrace {
            record_ids: shown.iter().map(|record| record.id).collect(),
        },
    );
    let explained = run(&state, &dispatcher, context, TRACE_TOOL, &chat, "trace").await?;
    Ok(Json(trace_body(trace_id, &explained, omitted)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_empty_body_means_the_current_draft_and_a_version_names_one() {
        assert_eq!(read_request(&Bytes::new()).unwrap().version, None);
        assert_eq!(read_request(&Bytes::from("{}")).unwrap().version, None);
        assert_eq!(
            read_request(&Bytes::from(r#"{"version": 2}"#))
                .unwrap()
                .version,
            Some(2)
        );
        for text in [r#"{"version": "x"}"#, r#"{"other": 1}"#, "nope"] {
            let error = read_request(&Bytes::from(text)).unwrap_err();
            assert_eq!(error.status_code().as_u16(), 422, "{text}");
            assert!(!error.detail().contains("nope"));
        }
    }

    #[test]
    fn the_flow_texts_get_the_literals_the_model_was_not_shown_back() {
        let doc = json!({"graph": {"nodes": [
            {"id": "check", "type": "branch", "conditions": [
                {"trait": "city", "op": "eq", "value": "Aarhus"}]}]}, "settings": {}});
        let (_, table) = flow_context::mask_document(
            &doc,
            &super::super::segment_context::ClearKeys::default(),
        )
        .unwrap();
        let explained = json!({
            "summary": "People in (kept:1).",
            "nodes": [{"id": "check", "text": "Only people in (kept:1) go on."}]
        });

        let body = flow_body("draft", None, &explained, &table);

        assert_eq!(body.summary, "People in Aarhus.");
        assert_eq!(body.nodes[0].id, "check");
        assert_eq!(body.nodes[0].text, "Only people in Aarhus go on.");
        assert_eq!(body.source, "draft");
    }

    #[test]
    fn the_trace_explanation_names_records_by_id_and_drops_anything_else() {
        let id = Uuid::now_v7();
        let explained = json!({"summary": "It failed.", "records": [
            {"id": id.to_string(), "text": "The step failed."},
            {"id": "not-an-id", "text": "dropped"}
        ]});

        let body = trace_body(TraceId::new(), &explained, 3);

        assert_eq!(body.records.len(), 1);
        assert_eq!(body.records[0].id, id);
        assert_eq!(body.omitted_records, 3);
        assert_eq!(body.summary, "It failed.");
    }

    #[test]
    fn the_failure_lists_codes_and_places_only() {
        let issues = DraftIssue::from_lines(&[
            "node_not_explained at nodes/check".to_owned(),
            "unknown_node at /nodes/0/id".to_owned(),
            "summary_empty".to_owned(),
        ]);

        let listed = problem_issues(&issues);

        let pairs: Vec<(&str, &str)> = listed
            .iter()
            .map(|issue| (issue.pointer.as_str(), issue.code.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("nodes/check", "node_not_explained"),
                ("/nodes/0/id", "unknown_node"),
                ("", "summary_empty"),
            ]
        );
    }
}
