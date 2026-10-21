//! `explain_flow`: checks a model's plain-language explanation of a flow, the current draft or a
//! published version, against that flow's graph (architecture 13.2). Every node id it cites must
//! be a node of the graph, every node but an exit must be cited exactly once, and the texts are
//! plain text with a length cap. It returns the explanation to the caller and stores nothing.
//!
//! The graph comes from the authorized request ([`DraftTarget::ExplainFlow`]), never from the
//! model. The model gets the flow with its personal literals masked, so the texts it writes carry
//! placeholders; the caller puts the literals back for the admin who owns the flow.

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};

use super::draft_segment::line;
use super::explain_segment::{MAX_NODE_TEXT_CHARS, MAX_SUMMARY_CHARS, is_plain};
use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::schema::tool_parameters;
use crate::problem::ApiError;

pub(crate) struct ExplainFlow;

/// What the model says about one node.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlowNodeText {
    /// The id of the node in the graph.
    pub(crate) id: String,
    /// One short plain-text sentence, at most 500 characters.
    pub(crate) text: String,
}

/// The arguments of `explain_flow`.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExplainFlowArgs {
    /// One or two plain-text sentences on who enters the flow and what happens to them, at most
    /// 1000 characters.
    pub(crate) summary: String,
    /// One entry for every node of the graph but the exit nodes, each exactly once.
    pub(crate) nodes: Vec<FlowNodeText>,
}

/// The pure part of the tool: the arguments against the schema and the graph's nodes. `known` is
/// every node id of the graph and `required` the ones that have to be cited. It returns the
/// explanation, trimmed, or the feedback lines. Pointers in the lines name where in the arguments
/// the problem is (`/nodes/2/id`), except `node_not_explained`, which names the node.
pub(crate) fn check_explanation(
    arguments: &Value,
    known: &[String],
    required: &[String],
) -> Result<Value, Vec<String>> {
    let schema = ExplainFlow::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let explained: ExplainFlowArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    let ids: HashSet<&str> = known.iter().map(String::as_str).collect();

    let mut issues = Vec::new();
    let summary = explained.summary.trim();
    if summary.is_empty() {
        issues.push(line("summary_empty", "/summary"));
    } else if summary.chars().count() > MAX_SUMMARY_CHARS {
        issues.push(line("text_too_long", "/summary"));
    } else if !is_plain(summary) {
        issues.push(line("text_not_plain", "/summary"));
    }
    if explained.nodes.len() > ids.len() {
        issues.push(line("too_many_nodes", "/nodes"));
    }
    let mut cited: HashSet<&str> = HashSet::new();
    let mut texts = Vec::new();
    for (index, entry) in explained.nodes.iter().enumerate().take(ids.len()) {
        let at = |field: &str| format!("/nodes/{index}/{field}");
        if !ids.contains(entry.id.as_str()) {
            issues.push(line("unknown_node", &at("id")));
        } else if !cited.insert(entry.id.as_str()) {
            issues.push(line("duplicate_node", &at("id")));
        }
        let text = entry.text.trim();
        if text.is_empty() {
            issues.push(line("text_empty", &at("text")));
        } else if text.chars().count() > MAX_NODE_TEXT_CHARS {
            issues.push(line("text_too_long", &at("text")));
        } else if !is_plain(text) {
            issues.push(line("text_not_plain", &at("text")));
        }
        texts.push(json!({"id": entry.id, "text": text}));
    }
    // The nodes in the order of the graph, so the feedback reads in that order.
    for id in required {
        if !cited.contains(id.as_str()) {
            issues.push(format!("node_not_explained at nodes/{id}"));
        }
    }
    if issues.is_empty() {
        Ok(json!({"summary": summary, "nodes": texts}))
    } else {
        Err(issues)
    }
}

impl ExplainFlow {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<ExplainFlowArgs>()
    }
}

impl AiTool<ToolContext> for ExplainFlow {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "explain_flow"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Explains the flow in plain text: a `summary`, and one entry in `nodes` for \
                 every node but the exit nodes, named by its node id. Nothing is saved."
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
            authorize_again(context).await?;
            let DraftTarget::ExplainFlow { node_ids, required } = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            check_explanation(arguments, node_ids, required)
                .map(ToolOutput::Draft)
                .map_err(ToolError::Invalid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        ["wait", "check", "end"].map(str::to_owned).to_vec()
    }

    fn required() -> Vec<String> {
        ["wait", "check"].map(str::to_owned).to_vec()
    }

    fn good() -> Value {
        json!({"summary": "People wait a day and are checked.", "nodes": [
            {"id": "wait", "text": "They wait a day."},
            {"id": "check", "text": "Pro people go on."}
        ]})
    }

    #[test]
    fn the_tool_reads_and_its_parameters_are_the_explanation() {
        let tool = ExplainFlow;
        assert_eq!(tool.name(), "explain_flow");
        assert_eq!(tool.effect(), ToolEffect::Read);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(schema.validate_value(&good()), Ok(()));
        assert!(schema.validate_value(&json!({"summary": "x"})).is_err());
        assert!(
            schema
                .validate_value(&json!({"summary": "x", "nodes": [{"id": "a"}]}))
                .is_err()
        );
    }

    #[test]
    fn an_explanation_that_cites_every_required_node_once_comes_back_trimmed() {
        let mut arguments = good();
        arguments["nodes"][0]["text"] = json!("  They wait a day.  ");
        // An exit node may be explained too.
        arguments["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "end", "text": "They leave."}));

        let explained = check_explanation(&arguments, &ids(), &required()).unwrap();

        assert_eq!(explained["nodes"][0]["text"], "They wait a day.");
        assert_eq!(explained["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(explained["summary"], "People wait a day and are checked.");
    }

    #[test]
    fn a_node_that_is_not_in_the_graph_is_an_issue_without_echoing_it() {
        let mut arguments = good();
        arguments["nodes"][1]["id"] = json!("ghost-SECRET");

        let lines = check_explanation(&arguments, &ids(), &required()).unwrap_err();

        assert_eq!(
            lines,
            [
                "unknown_node at /nodes/1/id",
                "node_not_explained at nodes/check"
            ]
        );
        assert!(!lines.join("").contains("SECRET"));
    }

    #[test]
    fn a_node_cited_twice_a_missing_one_and_bad_texts_are_each_named() {
        let arguments = json!({"summary": "  ", "nodes": [
            {"id": "wait", "text": "They wait."},
            {"id": "wait", "text": ""},
            {"id": "check", "text": "x".repeat(MAX_NODE_TEXT_CHARS + 1)}
        ]});

        let lines = check_explanation(&arguments, &ids(), &required()).unwrap_err();

        assert_eq!(
            lines,
            [
                "summary_empty at /summary",
                "duplicate_node at /nodes/1/id",
                "text_empty at /nodes/1/text",
                "text_too_long at /nodes/2/text"
            ]
        );
        let mut missing = good();
        missing["nodes"].as_array_mut().unwrap().pop();
        assert_eq!(
            check_explanation(&missing, &ids(), &required()).unwrap_err(),
            ["node_not_explained at nodes/check"]
        );
        let mut markup = good();
        markup["summary"] = json!("bad\u{7}text");
        assert_eq!(
            check_explanation(&markup, &ids(), &required()).unwrap_err(),
            ["text_not_plain at /summary"]
        );
    }

    #[test]
    fn more_entries_than_the_graph_has_nodes_are_too_many() {
        let entries: Vec<Value> = (0..4)
            .map(|index| json!({"id": format!("n{index}"), "text": "t"}))
            .collect();
        let arguments = json!({"summary": "s", "nodes": entries});

        let lines = check_explanation(&arguments, &ids(), &required()).unwrap_err();

        assert_eq!(lines[0], "too_many_nodes at /nodes");
    }
}
