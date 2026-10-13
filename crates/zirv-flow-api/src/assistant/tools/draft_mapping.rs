//! `draft_mapping`: checks a drafted mapping with the validator a stream save uses (architecture
//! 13.2). It returns the mapping, normalized, or the problems, as codes and column names. It
//! writes nothing: saving a mapping stays the human action `PUT .../streams/{name}`.
//!
//! The target (source and stream) comes from the run's context, so the model cannot retarget the
//! draft. Its arguments can only name the stream's fields, the registry's trait keys, identifier
//! kinds and lists of this workspace; anything else fails the same checks a save runs.

use serde_json::Value;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, JsonSchema, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_core::mapping::Header;

use super::{DraftTarget, ToolContext, arguments_invalid, authorize_again, check_arguments};
use crate::assistant::prompt::sample_fields;
use crate::assistant::schema::tool_parameters;
use crate::imports::MappingBody;
use crate::problem::ApiError;
use crate::sources::load_mapping_check;

pub(crate) struct DraftMapping;

impl DraftMapping {
    fn parameters() -> Result<JsonSchema, AiError> {
        tool_parameters::<MappingBody>()
    }
}

impl AiTool<ToolContext> for DraftMapping {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "draft_mapping"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Draft
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Proposes the mapping of the stream's fields to identifiers, traits and lists. \
                 The mapping is checked with the rules a save applies; the problems come back if \
                 it is refused. Nothing is saved."
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
            let schema = Self::parameters().map_err(|_| ToolError::Failed(ApiError::internal()))?;
            check_arguments(&schema, arguments)?;
            // A serde failure can echo a value, so it is never shown: the fixed line stands in.
            let body: MappingBody =
                serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid())?;
            let DraftTarget::Mapping { source_id, stream } = &context.target else {
                return Err(ToolError::Failed(ApiError::internal()));
            };
            let mut check =
                load_mapping_check(&context.state, context.workspace, *source_id, stream)
                    .await
                    .map_err(ToolError::Failed)?;
            if let Some(sample) = &context.sample {
                // The pasted sample's keys are this stream's fields for the run.
                let keys = sample_fields(sample)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect();
                let header = Header::new(keys).map_err(|error| {
                    ToolError::Failed(ApiError::unprocessable(format!("sample: {}", error.code())))
                })?;
                check = check.with_sample_keys(header);
            }
            let mapping = check
                .check(&body)
                .map_err(|issue| ToolError::Invalid(vec![issue.to_string()]))?;
            let normalized = MappingBody::from_core(&mapping)
                .and_then(|body| serde_json::to_value(body).ok())
                .ok_or_else(|| ToolError::Failed(ApiError::internal()))?;
            Ok(ToolOutput::Draft(normalized))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_parameters_are_the_mapping_body_and_a_draft_is_not_a_read() {
        let tool = DraftMapping;
        assert_eq!(tool.name(), "draft_mapping");
        assert_eq!(tool.effect(), ToolEffect::Draft);
        let schema = tool.spec().unwrap().parameters;
        let mapping = json!({"columns": [
            {"column": "Email", "target": {"type": "identifier", "kind": "email"}}]});

        assert_eq!(schema.validate_value(&mapping), Ok(()));
        // A column rule without its target is refused with a path and no value.
        let errors = schema
            .validate_value(&json!({"columns": [{"column": "SECRET-VALUE"}]}))
            .unwrap_err();
        assert!(!errors.is_empty());
        assert!(
            errors.iter().all(|line| !line.contains("SECRET-VALUE")),
            "{errors:?}"
        );
    }
}
