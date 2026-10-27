//! `explain_segment`: checks a model's plain-language explanation of a saved segment version
//! against that version's definition (architecture 13.2). Every pointer it cites must be a node
//! of the definition, every condition must be cited exactly once, and the texts are plain text
//! with a length cap. It returns the explanation to the caller and stores nothing.
//!
//! The version comes from the authorized request ([`DraftTarget::Explain`]), never from the
//! model. The model gets the definition with its personal literals masked, so the texts it writes
//! carry placeholders; the caller puts the literals back for the admin who owns the segment.

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};

use super::draft_segment::line;
use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::schema::tool_parameters;
use crate::assistant::segment_context::nodes;
use crate::auth::Need;
use crate::problem::ApiError;

pub(crate) struct ExplainSegment;

/// The longest text for one node, in characters.
pub(super) const MAX_NODE_TEXT_CHARS: usize = 500;
/// The longest summary, in characters.
pub(super) const MAX_SUMMARY_CHARS: usize = 1000;

/// What the model says about one node.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeText {
    /// The JSON pointer of the node in the definition; empty for the root.
    pub(crate) pointer: String,
    /// One short plain-text sentence, at most 500 characters.
    pub(crate) text: String,
}

/// The arguments of `explain_segment`.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExplainArgs {
    /// One or two plain-text sentences on who the segment holds, at most 1000 characters.
    pub(crate) summary: String,
    /// One entry for every condition of the definition, and optionally for all, any and not
    /// nodes. Every condition appears exactly once.
    pub(crate) nodes: Vec<NodeText>,
}

/// Whether a text is plain text: no control characters. It is shown as text and never as markup.
fn is_plain(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Trims a text and notes in `issues` what is wrong with it: empty (`empty`), longer than `max`
/// characters, or not plain. Returns the trimmed text.
pub(super) fn check_text<'a>(
    issues: &mut Vec<String>,
    text: &'a str,
    empty: &str,
    at: &str,
    max: usize,
) -> &'a str {
    let text = text.trim();
    if text.is_empty() {
        issues.push(line(empty, at));
    } else if text.chars().count() > max {
        issues.push(line("text_too_long", at));
    } else if !is_plain(text) {
        issues.push(line("text_not_plain", at));
    }
    text
}

/// The pure part of the tool: the arguments against the schema and the definition. It returns the
/// explanation, trimmed, or the feedback lines. Pointers in the lines name where in the arguments
/// the problem is (`/nodes/2/pointer`), except `leaf_not_explained`, which names the condition.
pub(crate) fn check_explanation(
    arguments: &Value,
    definition: &Value,
) -> Result<Value, Vec<String>> {
    let schema = ExplainSegment::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let explained: ExplainArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    let all = nodes(definition);
    let known: HashSet<&str> = all.iter().map(|node| node.pointer.as_str()).collect();

    let mut issues = Vec::new();
    let summary = check_text(
        &mut issues,
        &explained.summary,
        "summary_empty",
        "/summary",
        MAX_SUMMARY_CHARS,
    );
    if explained.nodes.len() > known.len() {
        issues.push(line("too_many_nodes", "/nodes"));
    }
    let mut cited: HashSet<&str> = HashSet::new();
    let mut texts = Vec::new();
    for (index, entry) in explained.nodes.iter().enumerate().take(known.len()) {
        let at = |field: &str| format!("/nodes/{index}/{field}");
        if !known.contains(entry.pointer.as_str()) {
            issues.push(line("unknown_pointer", &at("pointer")));
        } else if !cited.insert(entry.pointer.as_str()) {
            issues.push(line("duplicate_pointer", &at("pointer")));
        }
        let text = check_text(
            &mut issues,
            &entry.text,
            "text_empty",
            &at("text"),
            MAX_NODE_TEXT_CHARS,
        );
        texts.push(json!({"pointer": entry.pointer, "text": text}));
    }
    // The leaves in document order, so the feedback reads in the order of the definition.
    for node in &all {
        if node.is_leaf() && !cited.contains(node.pointer.as_str()) {
            issues.push(line("leaf_not_explained", &node.pointer));
        }
    }
    if issues.is_empty() {
        Ok(json!({"summary": summary, "nodes": texts}))
    } else {
        Err(issues)
    }
}

impl ExplainSegment {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<ExplainArgs>()
    }
}

impl AiTool<ToolContext> for ExplainSegment {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "explain_segment"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Explains the saved segment in plain text: a `summary`, and one entry in `nodes` \
                 for every condition of the definition, named by its JSON pointer. Nothing is \
                 saved."
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
            authorize_again(context, Need::Admin).await?;
            let DraftTarget::Explain { definition } = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            check_explanation(arguments, definition)
                .map(ToolOutput::Draft)
                .map_err(ToolError::Invalid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> Value {
        json!({"v": 1, "all": [
            {"trait": "plan", "op": "eq", "value": "pro"},
            {"not": {"list": "0190d0f2-0000-7000-8000-0000000000a1"}},
            {"event": "order", "agg": {"fn": "count", "op": "gte", "value": 2}}
        ]})
    }

    fn good() -> Value {
        json!({"summary": "People on the pro plan with orders.", "nodes": [
            {"pointer": "/all/0", "text": "The plan is pro."},
            {"pointer": "/all/1/not", "text": "They are not on the list."},
            {"pointer": "/all/2", "text": "They ordered at least twice."}
        ]})
    }

    #[test]
    fn the_tool_reads_and_its_parameters_are_the_explanation() {
        let tool = ExplainSegment;
        assert_eq!(tool.name(), "explain_segment");
        assert_eq!(tool.effect(), ToolEffect::Read);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(schema.validate_value(&good()), Ok(()));
        assert!(schema.validate_value(&json!({"summary": "x"})).is_err());
        assert!(
            schema
                .validate_value(&json!({"summary": "x", "nodes": [{"pointer": "/all/0"}]}))
                .is_err()
        );
    }

    #[test]
    fn an_explanation_that_cites_every_condition_once_comes_back_trimmed() {
        let mut arguments = good();
        arguments["nodes"][0]["text"] = json!("  The plan is pro.  ");
        // A composition may be explained too.
        arguments["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"pointer": "", "text": "All of these."}));

        let explained = check_explanation(&arguments, &definition()).unwrap();

        assert_eq!(explained["nodes"][0]["text"], "The plan is pro.");
        assert_eq!(explained["nodes"].as_array().unwrap().len(), 4);
        assert_eq!(explained["summary"], "People on the pro plan with orders.");
    }

    #[test]
    fn a_pointer_that_is_no_node_is_an_issue_without_echoing_it() {
        let mut arguments = good();
        arguments["nodes"][1]["pointer"] = json!("/all/9/SECRET");

        let lines = check_explanation(&arguments, &definition()).unwrap_err();

        assert_eq!(
            lines,
            [
                "unknown_pointer at /nodes/1/pointer",
                "leaf_not_explained at /all/1/not"
            ]
        );
        assert!(!lines.join("").contains("SECRET"));
    }

    #[test]
    fn an_uncited_leaf_and_a_twice_cited_one_are_issues() {
        let mut arguments = good();
        arguments["nodes"].as_array_mut().unwrap().remove(2);
        arguments["nodes"][1]["pointer"] = json!("/all/0");

        let lines = check_explanation(&arguments, &definition()).unwrap_err();

        assert_eq!(
            lines,
            [
                "duplicate_pointer at /nodes/1/pointer",
                "leaf_not_explained at /all/1/not",
                "leaf_not_explained at /all/2"
            ]
        );
    }

    #[test]
    fn a_text_that_is_empty_long_or_not_plain_is_an_issue() {
        let mut arguments = good();
        arguments["nodes"][0]["text"] = json!("   ");
        arguments["nodes"][1]["text"] = json!("x".repeat(MAX_NODE_TEXT_CHARS + 1));
        arguments["nodes"][2]["text"] = json!("two\nlines\u{7}");
        arguments["summary"] = json!("y".repeat(MAX_SUMMARY_CHARS + 1));

        let lines = check_explanation(&arguments, &definition()).unwrap_err();

        assert_eq!(
            lines,
            [
                "text_too_long at /summary",
                "text_empty at /nodes/0/text",
                "text_too_long at /nodes/1/text",
                "text_not_plain at /nodes/2/text"
            ]
        );
        // The caps are inclusive.
        let mut edge = good();
        edge["nodes"][0]["text"] = json!("x".repeat(MAX_NODE_TEXT_CHARS));
        edge["summary"] = json!("y".repeat(MAX_SUMMARY_CHARS));
        assert!(check_explanation(&edge, &definition()).is_ok());
        let mut blank = good();
        blank["summary"] = json!(" ");
        assert_eq!(
            check_explanation(&blank, &definition()).unwrap_err(),
            ["summary_empty at /summary"]
        );
    }

    #[test]
    fn markup_is_plain_text_to_the_check_and_text_to_the_ui() {
        let mut arguments = good();
        arguments["nodes"][0]["text"] = json!("<b>pro</b> & \"quoted\"");

        let explained = check_explanation(&arguments, &definition()).unwrap();

        assert_eq!(explained["nodes"][0]["text"], "<b>pro</b> & \"quoted\"");
    }

    #[test]
    fn more_entries_than_nodes_is_refused() {
        let mut arguments = good();
        for _ in 0..10 {
            arguments["nodes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"pointer": "/all/0", "text": "again"}));
        }

        let lines = check_explanation(&arguments, &definition()).unwrap_err();

        assert_eq!(lines[0], "too_many_nodes at /nodes");
    }

    #[test]
    fn arguments_that_are_not_an_explanation_are_refused() {
        for arguments in [json!({}), json!({"summary": 1, "nodes": []}), json!(null)] {
            let lines = check_explanation(&arguments, &definition()).unwrap_err();
            assert!(!lines.is_empty());
        }
    }
}
