//! Flows (architecture sections 9.1, 14 and 16.2): drafts, validation, publish into immutable
//! versions with a dry-run impact preview, and the lifecycle. The runtime is not here.
//!
//! Permissions (ruling 43163028): `Read` reads, `Write` saves drafts and validates, `Admin`
//! publishes, pauses, resumes and archives.

pub(crate) mod crud;
pub(crate) mod inspector;
pub(crate) mod operator;
pub(crate) mod publish;
mod schema;
pub(crate) mod signing;
pub(crate) mod test_mode;
pub(crate) mod triggers;
pub(crate) mod validation;
pub(crate) mod warmup;

use serde_json::Value;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, Issue};
use zirv_flow_core::{Cause, TraceContext, TraceId, ValidationError};
use zirv_flow_store::{Flow, FlowError, StoreError};

use crate::auth::Principal;
use crate::problem::{ApiError, FlowIssueProblem};
use crate::trace::request_trace;

pub(crate) use schema::FlowSchemas;

/// The longest text of a parse error returned to the caller.
const MAX_PARSE_ERROR_CHARS: usize = 300;

/// The trace of an operator's action on a flow: a trace of its own, caused by the signed-in user
/// (`manual`) or, for an API key, by the request. The decisions of the action are recorded under
/// it and the audit entry names it.
pub(crate) fn operator_trace(principal: &Principal) -> TraceContext {
    TraceContext {
        trace_id: TraceId::new(),
        cause: principal
            .user_id()
            .map_or_else(|| request_trace().cause, Cause::manual),
    }
}

/// The empty graph a new flow starts with.
fn empty_graph() -> Value {
    serde_json::json!({"v": 1, "nodes": [], "edges": []})
}

/// The settings a new flow starts with: started by hand, entered once.
fn default_settings() -> Value {
    serde_json::json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})
}

fn malformed(what: &str, error: &serde_json::Error) -> ApiError {
    let text: String = error
        .to_string()
        .chars()
        .take(MAX_PARSE_ERROR_CHARS)
        .collect();
    ApiError::flow_malformed(format!("the {what} is not a flow {what} document: {text}"))
}

/// The graph document, or the 422 that says it is not one. A draft may be incomplete; it cannot
/// be a document the vocabulary does not parse, so the editor and the publish read the same
/// thing. Returns the normalized form to store.
pub(crate) fn parse_graph(value: Value) -> Result<(FlowGraph, Value), ApiError> {
    let graph = FlowGraph::from_value(value).map_err(|error| malformed("graph", &error))?;
    let stored = serde_json::to_value(&graph).map_err(|_| ApiError::internal())?;
    Ok((graph, stored))
}

/// The settings document, as [`parse_graph`] reads the graph.
pub(crate) fn parse_settings(value: Value) -> Result<(FlowSettings, Value), ApiError> {
    let settings: FlowSettings =
        serde_json::from_value(value).map_err(|error| malformed("settings", &error))?;
    let stored = serde_json::to_value(&settings).map_err(|_| ApiError::internal())?;
    Ok((settings, stored))
}

/// The draft of a stored flow. It was parsed when it was saved; one that no longer parses is a
/// fault of ours, not of the caller.
pub(crate) fn draft_of(flow: &Flow) -> Result<(FlowGraph, FlowSettings), ApiError> {
    let graph =
        FlowGraph::from_value(flow.draft_graph.clone()).map_err(|_| ApiError::internal())?;
    let settings: FlowSettings =
        serde_json::from_value(flow.draft_settings.clone()).map_err(|_| ApiError::internal())?;
    Ok((graph, settings))
}

/// The problem for a refused flow write.
pub(crate) fn flow_error(error: FlowError) -> ApiError {
    match error {
        FlowError::NotFound => ApiError::not_found(),
        FlowError::Archived => ApiError::flow_archived(),
        FlowError::WarmupOutOfBounds => {
            ValidationError::new("seconds", "must be 60 to 86400").into()
        }
        illegal @ FlowError::IllegalTransition { .. } => ApiError::flow_state(illegal.to_string()),
        FlowError::Store(error) => name_taken(error),
    }
}

/// A unique violation on a flow write is a name that is taken.
pub(crate) fn name_taken(error: StoreError) -> ApiError {
    if error.is_conflict() {
        ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "a flow with that name already exists",
        )
        .with_code("name_taken")
    } else {
        error.into()
    }
}

/// The issues as the problem document and the response bodies carry them.
pub(crate) fn issue_problems(issues: &[Issue]) -> Vec<FlowIssueProblem> {
    issues
        .iter()
        .map(|issue| FlowIssueProblem {
            code: issue.code.to_owned(),
            node: issue.node.clone(),
            edge: issue.edge.clone(),
            at: issue.at.clone(),
        })
        .collect()
}

#[cfg(test)]
mod acceptance;
