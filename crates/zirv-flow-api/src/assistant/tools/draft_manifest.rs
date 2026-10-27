//! `draft_manifest`: checks a drafted `http` source manifest with the validator a source save
//! uses, `zirv_flow_connectors::manifest::validate` (architecture 13.2). It returns the manifest
//! or the problems as codes and JSON pointers, which hold no value. It writes nothing: saving
//! the manifest stays the human action `PATCH .../sources/{id}`.

use serde_json::Value;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_connectors::manifest;

use super::{ToolContext, arguments_invalid_lines, authorize_again};
use crate::assistant::schema::tool_parameters;
use crate::auth::Need;
use crate::problem::ApiError;
use crate::sources::ValidateRequest;

pub(crate) struct DraftManifest;

/// The pure part of the tool: the arguments against the schema, then the manifest validator a
/// save runs. It returns the manifest, or the feedback lines. The golden-prompt harness runs it
/// without a database.
pub(crate) fn check_draft(arguments: &Value) -> Result<Value, Vec<String>> {
    let schema = DraftManifest::parameters().map_err(|_| arguments_invalid_lines())?;
    schema.validate_value(arguments)?;
    let Some(draft) = arguments.get("manifest") else {
        return Err(vec![line("required", "/manifest")]);
    };
    match manifest::validate(draft) {
        Ok(_) => Ok(draft.clone()),
        Err(issues) => Err(issues
            .iter()
            .map(|issue| line(issue.code, &issue.path))
            .collect()),
    }
}

impl DraftManifest {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<ValidateRequest>()
    }
}

/// An issue as a feedback line: its code and the JSON pointer of the key it concerns.
fn line(code: &str, path: &str) -> String {
    if path.is_empty() {
        format!("{code} at the manifest root")
    } else {
        format!("{code} at {path}")
    }
}

impl AiTool<ToolContext> for DraftManifest {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "draft_manifest"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Draft
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Proposes the manifest of an HTTP API as the `manifest` argument. The manifest \
                 is checked with the rules a save applies; the problems come back with their \
                 JSON pointers if it is refused. Nothing is saved."
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
            check_draft(arguments)
                .map(ToolOutput::Draft)
                .map_err(ToolError::Invalid)
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::assistant::prompt::MANIFEST_EXAMPLE;

    #[test]
    fn the_parameters_hold_the_manifest_as_an_object() {
        let tool = DraftManifest;
        assert_eq!(tool.name(), "draft_manifest");
        assert_eq!(tool.effect(), ToolEffect::Draft);
        let schema = tool.spec().unwrap().parameters;

        assert_eq!(
            schema.as_value()["properties"]["manifest"]["type"],
            "object"
        );
        let example: Value = serde_json::from_str(MANIFEST_EXAMPLE).unwrap();
        assert_eq!(schema.validate_value(&json!({"manifest": example})), Ok(()));
        assert!(schema.validate_value(&json!({"manifest": "text"})).is_err());
    }

    #[test]
    fn an_issue_is_a_code_and_a_pointer() {
        assert_eq!(
            line("path_invalid", "/streams/0/path"),
            "path_invalid at /streams/0/path"
        );
        assert_eq!(line("required", ""), "required at the manifest root");
        let broken = manifest::validate(&json!({"version": "v1", "extra": 1})).unwrap_err();
        let lines: Vec<String> = broken
            .iter()
            .map(|issue| line(issue.code, &issue.path))
            .collect();
        assert!(
            lines.contains(&"unknown_field at /extra".to_owned()),
            "{lines:?}"
        );
    }
}
