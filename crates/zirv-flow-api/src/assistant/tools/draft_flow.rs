//! `draft_flow`: checks a drafted flow, its graph and its settings, with the validation a publish
//! applies (the structural checks of `core::flow` and then the catalog, the egress policy and the
//! signing secret), and returns it or the problems as codes and places, which hold no value
//! (architecture 13.2). It writes nothing: accepting the draft is a separate step that copies it
//! into the flow's ordinary draft, and publishing stays a human action under its own permission.
//!
//! The flow a draft is for comes from the authorized request ([`DraftTarget::Flow`]), never from
//! the model, which can only fill in the two documents.

use serde::Deserialize;
use serde_json::{Value, json};
use utoipa::ToSchema;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_core::flow::{
    Edge, ExitCriterion, FlowGraph, FlowNode, FlowSettings, Goal, NODE_TYPES, Reentry, Trigger,
};

use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::flow_context::issue_line;
use crate::assistant::schema::tool_parameters;
use crate::auth::Need;
use crate::flows::crud::check_document_read;
use crate::problem::ApiError;

pub(crate) struct DraftFlow;

/// The most feedback lines one refusal carries.
const MAX_LINES: usize = 50;

/// The arguments of `draft_flow`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftFlowArgs {
    /// The whole flow graph: `{"v": 1, "nodes": [...], "edges": [...]}`.
    #[schema(value_type = Object)]
    pub(crate) graph: Value,
    /// The whole flow settings: the trigger, re-entry and, optionally, the goal and exit criteria.
    #[schema(value_type = Object)]
    pub(crate) settings: Value,
}

/// A node or edge id as it is safe to put in a feedback line: an id that is not one is its index.
fn label(item: &Value, index: usize) -> String {
    item.get("id")
        .and_then(Value::as_str)
        .filter(|id| {
            (1..=64).contains(&id.chars().count())
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .map_or_else(|| index.to_string(), str::to_owned)
}

/// Where a graph that does not read as a flow graph is wrong. A serde error can echo a value, so
/// it is never shown: the lines name the node or edge and say whether its type is unknown.
fn graph_lines(graph: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    match graph.get("nodes").and_then(Value::as_array) {
        Some(nodes) => {
            for (index, node) in nodes.iter().enumerate() {
                if serde_json::from_value::<FlowNode>(node.clone()).is_ok() {
                    continue;
                }
                let known = node
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| NODE_TYPES.contains(&kind));
                let code = if known {
                    "node_malformed"
                } else {
                    "unknown_node_type"
                };
                lines.push(format!("{code} at nodes/{}", label(node, index)));
            }
        }
        None => lines.push("flow_malformed at graph/nodes".to_owned()),
    }
    match graph.get("edges").and_then(Value::as_array) {
        Some(edges) => {
            for (index, edge) in edges.iter().enumerate() {
                if serde_json::from_value::<Edge>(edge.clone()).is_err() {
                    lines.push(format!("edge_malformed at edges/{}", label(edge, index)));
                }
            }
        }
        None => lines.push("flow_malformed at graph/edges".to_owned()),
    }
    if lines.is_empty() {
        lines.push("flow_malformed at graph".to_owned());
    }
    lines
}

/// Where a settings document that does not read as flow settings is wrong.
fn settings_lines(settings: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let mut part = |name: &str, bad: bool| {
        if bad {
            lines.push(format!("settings_malformed at settings/{name}"));
        }
    };
    let field = |name: &str| settings.get(name);
    part(
        "trigger",
        field("trigger").is_none_or(|v| serde_json::from_value::<Trigger>(v.clone()).is_err()),
    );
    part(
        "reentry",
        field("reentry").is_none_or(|v| serde_json::from_value::<Reentry>(v.clone()).is_err()),
    );
    part(
        "goal",
        field("goal")
            .is_some_and(|v| !v.is_null() && serde_json::from_value::<Goal>(v.clone()).is_err()),
    );
    part(
        "exit_criteria",
        field("exit_criteria").is_some_and(|v| {
            v.as_array().is_none_or(|items| {
                items
                    .iter()
                    .any(|item| serde_json::from_value::<ExitCriterion>(item.clone()).is_err())
            })
        }),
    );
    if lines.is_empty() {
        lines.push("flow_malformed at settings".to_owned());
    }
    lines
}

/// The pure part of the tool: the arguments against the schema, then the two documents as flow
/// documents. It returns the graph, the settings and the document `{"graph", "settings"}` in the
/// form a draft stores, or the feedback lines.
pub(crate) fn parse_document(
    arguments: &Value,
) -> Result<(FlowGraph, FlowSettings, Value), Vec<String>> {
    let schema = DraftFlow::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let args: DraftFlowArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    let graph = FlowGraph::from_value(args.graph.clone());
    let settings = serde_json::from_value::<FlowSettings>(args.settings.clone());
    match (graph, settings) {
        (Ok(graph), Ok(settings)) => {
            let document = json!({
                "graph": serde_json::to_value(&graph).map_err(|_| arguments_invalid_lines())?,
                "settings": serde_json::to_value(&settings).map_err(|_| arguments_invalid_lines())?,
            });
            Ok((graph, settings, document))
        }
        (graph, settings) => {
            let mut lines = Vec::new();
            if graph.is_err() {
                lines.extend(graph_lines(&args.graph));
            }
            if settings.is_err() {
                lines.extend(settings_lines(&args.settings));
            }
            lines.truncate(MAX_LINES);
            Err(lines)
        }
    }
}

impl DraftFlow {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<DraftFlowArgs>()
    }
}

impl AiTool<ToolContext> for DraftFlow {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "draft_flow"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Draft
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Proposes a flow as the `graph` and `settings` arguments, each whole. The flow \
                 is checked with the validation a publish applies; the problems come back with \
                 the node, edge or settings place of each if it is refused. Nothing is saved."
                    .into(),
            ),
            parameters: Self::parameters()?,
        })
    }

    fn call<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError<ApiError>>> {
        Box::pin(async move {
            authorize_again(context, Need::Edit).await?;
            let DraftTarget::Flow = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            let (graph, settings, document) =
                parse_document(arguments).map_err(ToolError::Invalid)?;
            let checked = check_document_read(&context.state, context.workspace, &graph, &settings)
                .await
                .map_err(ToolError::Failed)?;
            if checked.issues.is_empty() {
                Ok(ToolOutput::Draft(document))
            } else {
                Err(ToolError::Invalid(
                    checked
                        .issues
                        .iter()
                        .take(MAX_LINES)
                        .map(issue_line)
                        .collect(),
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::flow::validate;

    use super::*;

    fn flow() -> Value {
        json!({
            "graph": {"v": 1, "nodes": [
                {"id": "wait", "type": "wait.duration", "seconds": 60},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "wait", "port": "next", "to": "end"}]},
            "settings": {"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}
        })
    }

    #[test]
    fn the_tool_drafts_and_its_parameters_are_the_two_documents() {
        let tool = DraftFlow;
        assert_eq!(tool.name(), "draft_flow");
        assert_eq!(tool.effect(), ToolEffect::Draft);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(schema.as_value()["properties"]["graph"]["type"], "object");
        assert_eq!(schema.validate_value(&flow()), Ok(()));
        assert!(schema.validate_value(&json!({"graph": {}})).is_err());
        assert!(
            schema
                .validate_value(&json!({"graph": {}, "settings": 3}))
                .is_err()
        );
    }

    #[test]
    fn a_flow_that_reads_comes_back_in_its_stored_form() {
        let mut arguments = flow();
        // An optional field that is spelled out comes back the way a save stores it.
        arguments["settings"]["exit_on_goal"] = json!(false);

        let (graph, settings, document) = parse_document(&arguments).unwrap();

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(settings.trigger.type_name(), "manual");
        assert_eq!(document["graph"], flow()["graph"]);
        assert_eq!(document["settings"]["trigger"], json!({"type": "manual"}));
    }

    #[test]
    fn a_node_that_does_not_read_is_named_by_id_and_never_echoed() {
        let mut arguments = flow();
        arguments["graph"]["nodes"][0] = json!({"id": "wait", "type": "wait.duration",
            "seconds": "SECRET-LITERAL"});
        arguments["graph"]["nodes"][1] = json!({"id": "end", "type": "teleport"});
        arguments["graph"]["edges"][0] = json!({"id": "e1", "from": "wait"});

        let lines = parse_document(&arguments).unwrap_err();

        assert_eq!(
            lines,
            [
                "node_malformed at nodes/wait",
                "unknown_node_type at nodes/end",
                "edge_malformed at edges/e1"
            ]
        );
        assert!(!lines.join("").contains("SECRET"));
    }

    #[test]
    fn settings_that_do_not_read_are_named_by_part() {
        let mut arguments = flow();
        arguments["settings"] = json!({"trigger": {"type": "carrier_pigeon"},
            "reentry": {"mode": "once"}, "exit_criteria": [{"type": "nope"}]});

        let lines = parse_document(&arguments).unwrap_err();

        assert_eq!(
            lines,
            [
                "settings_malformed at settings/trigger",
                "settings_malformed at settings/exit_criteria"
            ]
        );
    }

    #[test]
    fn arguments_that_are_not_the_two_documents_are_refused() {
        for arguments in [
            json!({}),
            json!({"graph": 3, "settings": {}}),
            json!({"graph": {}, "settings": {}, "extra": 1}),
        ] {
            assert!(!parse_document(&arguments).unwrap_err().is_empty());
        }
        // An id that is not safe to print is replaced by its position.
        let mut arguments = flow();
        arguments["graph"]["nodes"][0] = json!({"id": "a b\nc", "type": "wait.duration"});
        assert_eq!(
            parse_document(&arguments).unwrap_err(),
            ["node_malformed at nodes/0"]
        );
    }

    #[test]
    fn a_structural_issue_is_a_code_and_a_place() {
        let (graph, settings, _) = parse_document(&json!({
            "graph": {"v": 1, "nodes": [
                {"id": "b", "type": "branch", "conditions": [{"trait": "plan", "op": "exists"}]},
                {"id": "end", "type": "exit"}
            ], "edges": [{"id": "e1", "from": "b", "port": "case_0", "to": "end"}]},
            "settings": {"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}
        }))
        .unwrap();

        let lines: Vec<String> = validate(&graph, &settings).iter().map(issue_line).collect();

        assert_eq!(lines, ["port_not_wired at nodes/b/ports/else"]);
    }
}
