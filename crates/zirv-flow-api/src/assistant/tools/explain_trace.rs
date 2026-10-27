//! `explain_trace`: checks a model's plain-language explanation of a trace against the decision
//! records the model was shown (architecture 13.2). The first read-only consumer of the trace API:
//! it cites each record by the short label it was shown under (`r1`, `r2`, ...), every label must
//! be a record of the trace, every record must be cited exactly once, and the texts are plain text
//! with a length cap. The checked explanation names the records by their decision-record ids. It
//! stores nothing.
//!
//! The records come from the authorized request ([`DraftTarget::ExplainTrace`]), never from the
//! model, and they are the ones of the caller's workspace.

use hashbrown::HashSet;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};

use super::draft_segment::line;
use super::explain_segment::{MAX_NODE_TEXT_CHARS, MAX_SUMMARY_CHARS, check_text};
use super::{DraftTarget, ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::flow_context::record_label;
use crate::assistant::schema::tool_parameters;
use crate::auth::Need;
use crate::problem::ApiError;

pub(crate) struct ExplainTrace;

/// What the model says about one record.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordText {
    /// The label of the record as it was shown, such as `r3`.
    pub(crate) record: String,
    /// One short plain-text sentence, at most 500 characters.
    pub(crate) text: String,
}

/// The arguments of `explain_trace`.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExplainTraceArgs {
    /// One or two plain-text sentences on what happened under the trace, at most 1000 characters.
    pub(crate) summary: String,
    /// One entry for every record shown, each exactly once.
    pub(crate) records: Vec<RecordText>,
}

/// The pure part of the tool: the arguments against the schema and the records shown, whose ids
/// are in the order of their labels. It returns the explanation with each record named by its
/// decision-record id, or the feedback lines. Pointers in the lines name where in the arguments
/// the problem is (`/records/2/record`), except `record_not_explained`, which names the label.
pub(crate) fn check_explanation(arguments: &Value, shown: &[Uuid]) -> Result<Value, Vec<String>> {
    let schema = ExplainTrace::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let explained: ExplainTraceArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid_lines())?;
    let labels: Vec<String> = (0..shown.len()).map(record_label).collect();

    let mut issues = Vec::new();
    let summary = check_text(
        &mut issues,
        &explained.summary,
        "summary_empty",
        "/summary",
        MAX_SUMMARY_CHARS,
    );
    if explained.records.len() > shown.len() {
        issues.push(line("too_many_records", "/records"));
    }
    let mut cited: HashSet<&str> = HashSet::new();
    let mut texts = Vec::new();
    for (index, entry) in explained.records.iter().enumerate().take(shown.len()) {
        let at = |field: &str| format!("/records/{index}/{field}");
        let position = labels.iter().position(|label| *label == entry.record);
        match position {
            None => issues.push(line("unknown_record", &at("record"))),
            Some(_) if !cited.insert(entry.record.as_str()) => {
                issues.push(line("duplicate_record", &at("record")));
            }
            Some(_) => {}
        }
        let text = check_text(
            &mut issues,
            &entry.text,
            "text_empty",
            &at("text"),
            MAX_NODE_TEXT_CHARS,
        );
        if let Some(position) = position {
            texts.push(json!({"id": shown[position], "text": text}));
        }
    }
    // The records in the order of the trace, so the feedback reads in that order.
    for label in &labels {
        if !cited.contains(label.as_str()) {
            issues.push(format!("record_not_explained at {label}"));
        }
    }
    if issues.is_empty() {
        Ok(json!({"summary": summary, "records": texts}))
    } else {
        Err(issues)
    }
}

impl ExplainTrace {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<ExplainTraceArgs>()
    }
}

impl AiTool<ToolContext> for ExplainTrace {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "explain_trace"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Explains a trace in plain text: a `summary`, and one entry in `records` for \
                 every decision record shown, named by its label. Nothing is saved."
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
            let DraftTarget::ExplainTrace { record_ids } = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            check_explanation(arguments, record_ids)
                .map(ToolOutput::Draft)
                .map_err(ToolError::Invalid)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown() -> Vec<Uuid> {
        (0..3).map(|_| Uuid::now_v7()).collect()
    }

    fn good() -> Value {
        json!({"summary": "A step failed twice and was then skipped.", "records": [
            {"record": "r1", "text": "The step failed."},
            {"record": "r2", "text": "It was retried and failed again."},
            {"record": "r3", "text": "An operator skipped it."}
        ]})
    }

    #[test]
    fn the_tool_reads_and_its_parameters_are_the_explanation() {
        let tool = ExplainTrace;
        assert_eq!(tool.name(), "explain_trace");
        assert_eq!(tool.effect(), ToolEffect::Read);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(schema.validate_value(&good()), Ok(()));
        assert!(schema.validate_value(&json!({"summary": "x"})).is_err());
    }

    #[test]
    fn the_checked_explanation_names_the_records_by_their_decision_record_ids() {
        let ids = shown();
        let mut arguments = good();
        // The order of the citations is the model's; the ids follow the labels.
        arguments["records"].as_array_mut().unwrap().reverse();
        arguments["records"][0]["text"] = json!("  An operator skipped it.  ");

        let explained = check_explanation(&arguments, &ids).unwrap();

        assert_eq!(explained["records"][0]["id"], json!(ids[2]));
        assert_eq!(explained["records"][0]["text"], "An operator skipped it.");
        assert_eq!(explained["records"][2]["id"], json!(ids[0]));
        assert_eq!(
            explained["summary"],
            "A step failed twice and was then skipped."
        );
    }

    #[test]
    fn a_record_that_was_not_shown_is_an_issue_without_echoing_it() {
        let mut arguments = good();
        arguments["records"][1]["record"] = json!("r9-SECRET");

        let lines = check_explanation(&arguments, &shown()).unwrap_err();

        assert_eq!(
            lines,
            [
                "unknown_record at /records/1/record",
                "record_not_explained at r2"
            ]
        );
        assert!(!lines.join("").contains("SECRET"));
    }

    #[test]
    fn a_record_cited_twice_a_missing_one_and_bad_texts_are_each_named() {
        let arguments = json!({"summary": "", "records": [
            {"record": "r1", "text": "ok"},
            {"record": "r1", "text": ""},
            {"record": "r3", "text": "x".repeat(MAX_NODE_TEXT_CHARS + 1)}
        ]});

        let lines = check_explanation(&arguments, &shown()).unwrap_err();

        assert_eq!(
            lines,
            [
                "summary_empty at /summary",
                "duplicate_record at /records/1/record",
                "text_empty at /records/1/text",
                "text_too_long at /records/2/text",
                "record_not_explained at r2"
            ]
        );
        let mut extra = good();
        extra["records"]
            .as_array_mut()
            .unwrap()
            .push(json!({"record": "r1", "text": "again"}));
        assert_eq!(
            check_explanation(&extra, &shown()).unwrap_err()[0],
            "too_many_records at /records"
        );
    }
}
