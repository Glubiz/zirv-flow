//! Explaining a saved segment with the assistant (architecture 13.2): `POST
//! .../segments/{id}/ai/explain` asks the model, with the workspace's catalog and the saved
//! definition (its personal literals masked), for a plain-language explanation, checks it with the
//! `explain_segment` tool, repairs it at most `ai.max_repair_attempts` times and answers with
//! it. Nothing is stored and nothing is audited beyond what dispatch already records, the usage
//! of the call.
//!
//! The call is made as the signed-in user, who must be a workspace admin. The model gets no trait
//! or event value, no preview count or sample, no member and no explain result of any person.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use zirv_flow_ai::{
    BoxFuture, ChatResponse, RepairError, RepairOutcome, ToolError, ToolOutput, Verdict,
};

use super::dispatch::{DispatchError, Dispatcher};
use super::drafts::{DraftIssue, expected_call, segment_details, wrong_answer};
use super::segment_context::{self, Placeholders};
use super::tools::{self, DraftTarget, ToolContext};
use crate::AppState;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::problem::{ApiError, Problem, SegmentIssueProblem};

/// The name of the tool a run offers.
const TOOL: &str = "explain_segment";
/// The feature the call log records the model calls of an explanation under.
const FEATURE: &str = "segment_explain";

/// Which version to explain.
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiExplainRequest {
    /// The version of the segment to explain; the current one when absent.
    #[serde(default)]
    version: Option<i32>,
}

/// What the model said about one node of the definition.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiExplainedNode {
    /// The JSON pointer of the node in the version's definition; empty for the root.
    pointer: String,
    /// Plain text. Show it as text, never as markup.
    text: String,
}

/// The explanation of one segment version.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AiExplainBody {
    /// The version that was explained.
    version: i32,
    /// Who the segment holds, in plain text.
    summary: String,
    /// One entry for every condition of the definition, and for some compositions.
    nodes: Vec<AiExplainedNode>,
}

/// Reads the optional body: an empty one means the current version.
fn read_request(body: &Bytes) -> Result<AiExplainRequest, ApiError> {
    if body.is_empty() {
        return Ok(AiExplainRequest::default());
    }
    serde_json::from_slice(body).map_err(|_| {
        ApiError::unprocessable("version: the body must be {} or {\"version\": <number>}")
    })
}

/// Checks one answer: exactly one call of `explain_segment`, run through the registry as the
/// signed-in user. Anything else is feedback, and a refusal ends the run.
async fn explain_turn(
    context: &ToolContext,
    response: &ChatResponse,
) -> Verdict<Value, DraftIssue, ApiError> {
    let feedback = || Verdict::Invalid(vec![wrong_answer(TOOL)]);
    let Some(arguments) = expected_call(response, TOOL) else {
        return feedback();
    };
    let Some(tool) = tools::find(TOOL) else {
        return feedback();
    };
    match tool.call(context, arguments).await {
        Ok(ToolOutput::Draft(explained)) => Verdict::Valid(explained),
        Ok(ToolOutput::Context(_)) => feedback(),
        Err(ToolError::Invalid(lines)) => Verdict::Invalid(DraftIssue::from_lines(&lines)),
        Err(ToolError::Failed(error)) => Verdict::Abort(error),
    }
}

fn turn(
    context: Arc<ToolContext>,
    response: &ChatResponse,
) -> BoxFuture<'_, Verdict<Value, DraftIssue, ApiError>> {
    Box::pin(async move { explain_turn(&context, response).await })
}

/// The explanation as the response: the model's texts with the literals it was not shown put back,
/// for the admin who owns the segment.
fn body_of(version: i32, explained: &Value, table: &Placeholders) -> AiExplainBody {
    let text = |value: Option<&Value>| {
        table.restore_text(value.and_then(Value::as_str).unwrap_or_default())
    };
    AiExplainBody {
        version,
        summary: text(explained.get("summary")),
        nodes: explained
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|node| AiExplainedNode {
                pointer: node
                    .get("pointer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                text: text(node.get("text")),
            })
            .collect(),
    }
}

/// The issues of the last attempt as the problem lists them: a code and where.
fn problem_issues(issues: &[DraftIssue]) -> Vec<SegmentIssueProblem> {
    issues
        .iter()
        .map(|issue| SegmentIssueProblem {
            pointer: issue
                .at()
                .filter(|at| at.starts_with('/'))
                .unwrap_or_default()
                .to_owned(),
            code: issue.code().to_owned(),
        })
        .collect()
}

/// Explains a saved version of a segment in plain language: a summary and a sentence for every
/// condition, each named by its JSON pointer. The model's answer is checked (every pointer must
/// be a node, every condition cited once, plain text with a length cap) and, when it fails,
/// asked again at most `ai.max_repair_attempts` times. Nothing is stored. Workspace admins who
/// are signed in; an API key is refused. Every attempt is a gated model call that counts against
/// the budget and the rate limit, and with the assistant off nothing is read or sent. The model
/// is not told any trait or event value, count, sample or member.
#[utoipa::path(
    post,
    operation_id = "explain_segment_with_ai",
    path = "/api/v1/workspaces/{ws}/segments/{id}/ai/explain",
    tag = "ai",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id")
    ),
    request_body = Option<AiExplainRequest>,
    responses(
        (status = 200, description = "The explanation", body = AiExplainBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 402, description = "The workspace's monthly AI budget is spent (`ai_budget_exceeded`); nothing was sent", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or an API key (`ai_user_required`)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, segment or version, or no access", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The assistant is off (`ai_disabled`)", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The model's explanation did not pass its checks within the repair attempts (`ai_explain_failed`, with the issue codes), or the body is not a version", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many AI calls (`ai_rate_limited`); try again in a minute", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The model provider failed (`ai_provider_error`)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The server has no master key to open the provider key", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn explain(
    State(state): State<AppState>,
    principal: Principal,
    Path((ws, id)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<AiExplainBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let user = principal.user_id().ok_or_else(ApiError::ai_user_required)?;
    let id = path_id(&id)?;
    let request = read_request(&body)?;
    // The assistant being off is the first answer, before anything is read.
    let dispatcher = Dispatcher::open(&state, Some(user), workspace.id, FEATURE)
        .await
        .map_err(ApiError::from)?;

    let segment = zirv_flow_store::find_segment(&state.pool, workspace.id, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let number = request.version.unwrap_or(segment.current_version);
    let version = zirv_flow_store::find_segment_version(&state.pool, workspace.id, id, number)
        .await?
        .ok_or_else(ApiError::not_found)?;

    let privacy = dispatcher.privacy();
    let details = segment_details(&state, workspace.id).await?;
    let (parts, table) =
        segment_context::explain_context(&details, id, &version.definition, privacy);
    let tool = tools::find(TOOL).ok_or_else(ApiError::internal)?;
    let chat = segment_context::build_explain_request(
        privacy,
        parts,
        tool.spec().map_err(|_| ApiError::internal())?,
    )
    .map_err(|error| ApiError::from(DispatchError::Ai(error)))?;
    let context = Arc::new(ToolContext {
        state: state.clone(),
        principal,
        workspace: workspace.id,
        privacy,
        target: DraftTarget::Explain {
            definition: version.definition.clone(),
        },
        sample: None,
    });

    let outcome = zirv_flow_ai::repair(
        state.ai.config.max_repair_attempts,
        &chat,
        &dispatcher,
        move |response| turn(Arc::clone(&context), response),
    )
    .await;
    match outcome {
        Ok(RepairOutcome::Valid(explained, _)) => {
            Ok(Json(body_of(version.version, &explained, &table)))
        }
        Ok(RepairOutcome::Exhausted(issues, _)) => {
            Err(ApiError::ai_explain_failed(problem_issues(&issues)))
        }
        Err(RepairError::Dispatch(error)) => Err(DispatchError::Ai(error).into()),
        Err(RepairError::Aborted(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_empty_body_means_the_current_version_and_a_version_names_one() {
        assert_eq!(read_request(&Bytes::new()).unwrap().version, None);
        assert_eq!(read_request(&Bytes::from("{}")).unwrap().version, None);
        assert_eq!(
            read_request(&Bytes::from(r#"{"version": 3}"#))
                .unwrap()
                .version,
            Some(3)
        );
    }

    #[test]
    fn a_body_that_is_not_a_version_is_refused_without_echoing_it() {
        for text in [r#"{"version": "x"}"#, r#"{"other": 1}"#, "nope", "3"] {
            let error = read_request(&Bytes::from(text)).unwrap_err();
            assert_eq!(error.status_code().as_u16(), 422, "{text}");
            assert!(!error.detail().contains("nope"));
        }
    }

    #[test]
    fn the_texts_get_the_literals_the_model_was_not_shown_back() {
        let saved = json!({"v": 1, "trait": "city", "op": "eq", "value": "Aarhus"});
        let (_, table) = segment_context::mask(&saved, &segment_context::ClearKeys::default());
        let explained = json!({
            "summary": "People in (kept:1).",
            "nodes": [{"pointer": "", "text": "The city is (kept:1)."}]
        });

        let body = body_of(4, &explained, &table);

        assert_eq!(body.version, 4);
        assert_eq!(body.summary, "People in Aarhus.");
        assert_eq!(body.nodes[0].pointer, "");
        assert_eq!(body.nodes[0].text, "The city is Aarhus.");
    }

    #[test]
    fn the_failure_lists_codes_and_definition_pointers_only() {
        let issues = DraftIssue::from_lines(&[
            "leaf_not_explained at /all/1".to_owned(),
            "unknown_pointer at /nodes/0/pointer".to_owned(),
            "leaf_not_explained at the definition root".to_owned(),
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
                ("/all/1", "leaf_not_explained"),
                ("/nodes/0/pointer", "unknown_pointer"),
                ("", "leaf_not_explained"),
                ("", "summary_empty"),
            ]
        );
    }
}
