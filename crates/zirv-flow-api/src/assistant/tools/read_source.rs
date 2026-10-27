//! `read_source`: what the model may know about a source (architecture 13.3).
//!
//! For a stream it is the stream's fields, sync mode, primary key and saved mapping, the trait
//! registry and the workspace's lists. For a source without a stream it is the source's name and
//! saved manifest. Sample records are added only when the workspace opted in to personal data;
//! without it the source is not read at all.
//!
//! The run calls this tool itself with the target it authorized, so the model never picks a
//! source. It is a registered tool all the same, so every call authorizes the user again and
//! refuses a source of another workspace.

use serde::Deserialize;
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{
    AiError, AiTool, BoxFuture, Content, Privacy, ToolEffect, ToolError, ToolOutput, ToolSpec,
};
use zirv_flow_connectors::sources::StreamDescriptor;

use super::{ToolContext, arguments_invalid, authorize_again, check_arguments};
use crate::assistant::prompt::{
    MAX_SAMPLE_RECORDS, ManifestContext, StreamContext, TraitInfo, render_manifest_context,
    render_stream_context, sample_fields,
};
use crate::assistant::schema::tool_parameters;
use crate::auth::Need;
use crate::problem::ApiError;
use crate::sources::{is_push, read_stream_records, saved_mapping_body};

/// The arguments of `read_source`.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadSourceArgs {
    /// The source to read about. It must belong to the workspace.
    pub(crate) source_id: Uuid,
    /// One of the source's streams, by name. Absent, the source itself is described.
    #[serde(default)]
    pub(crate) stream: Option<String>,
}

pub(crate) struct ReadSource;

impl ReadSource {
    fn parameters() -> Result<zirv_flow_ai::JsonSchema, AiError> {
        tool_parameters::<ReadSourceArgs>()
    }
}

impl AiTool<ToolContext> for ReadSource {
    type Error = ApiError;

    fn name(&self) -> &'static str {
        "read_source"
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn spec(&self) -> Result<ToolSpec, AiError> {
        Ok(ToolSpec {
            name: self.name().to_owned(),
            description: Content::Instruction(
                "Reads what is known about a source's stream or manifest: field names and \
                 types, the trait registry and the lists. It returns sample records only when \
                 the workspace allows personal data."
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
            let schema = Self::parameters().map_err(|_| ToolError::Failed(ApiError::internal()))?;
            check_arguments(&schema, arguments)?;
            let args: ReadSourceArgs =
                serde_json::from_value(arguments.clone()).map_err(|_| arguments_invalid())?;
            let source = zirv_flow_store::find_source(
                &context.state.pool,
                context.workspace,
                args.source_id,
            )
            .await
            .map_err(|error| ToolError::Failed(error.into()))?
            .ok_or_else(|| ToolError::Failed(ApiError::not_found()))?;
            let Some(name) = args.stream else {
                let saved = (source.kind == "http")
                    .then(|| source.config.get("manifest").cloned())
                    .flatten();
                return Ok(ToolOutput::Context(render_manifest_context(
                    &ManifestContext {
                        source_name: Some(source.name),
                        saved_manifest: saved,
                    },
                )));
            };
            let stream = zirv_flow_store::find_stream(
                &context.state.pool,
                context.workspace,
                args.source_id,
                &name,
            )
            .await
            .map_err(|error| ToolError::Failed(error.into()))?
            .ok_or_else(|| ToolError::Failed(ApiError::not_found()))?;
            let descriptor: StreamDescriptor = serde_json::from_value(stream.discovered.clone())
                .map_err(|_| ToolError::Failed(ApiError::internal()))?;
            let push = is_push(&source.kind);

            // A webhook stream has no fields of its own: the pasted sample's keys are them.
            let fields = match (push, &context.sample) {
                (true, Some(sample)) => sample_fields(sample),
                (true, None) => Vec::new(),
                (false, _) => descriptor
                    .fields
                    .iter()
                    .map(|field| {
                        (
                            field.name.clone(),
                            serde_json::to_value(field.field_type)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "unknown".to_owned()),
                        )
                    })
                    .collect(),
            };
            let traits =
                zirv_flow_store::list_trait_definitions(&context.state.pool, context.workspace)
                    .await
                    .map_err(|error| ToolError::Failed(error.into()))?
                    .into_iter()
                    .filter(|definition| !definition.archived)
                    .map(|definition| TraitInfo {
                        key: definition.key,
                        trait_type: definition.trait_type.as_str().to_owned(),
                        description: definition.description,
                    })
                    .collect();
            let lists = zirv_flow_store::list_lists(&context.state.pool, context.workspace)
                .await
                .map_err(|error| ToolError::Failed(error.into()))?
                .into_iter()
                .map(|list| (list.id, list.name))
                .collect();
            // Without the opt-in the source is not read at all.
            let samples = if context.privacy == Privacy::PersonalAllowed {
                if push {
                    context
                        .sample
                        .iter()
                        .flatten()
                        .take(MAX_SAMPLE_RECORDS)
                        .cloned()
                        .collect()
                } else {
                    read_stream_records(
                        &context.state,
                        context.workspace,
                        args.source_id,
                        &stream,
                        MAX_SAMPLE_RECORDS,
                    )
                    .await
                    .map_err(ToolError::Failed)?
                    .into_iter()
                    .map(|record| record.data)
                    .collect()
                }
            } else {
                Vec::new()
            };
            let rendered = StreamContext {
                fields,
                sync_mode: Some(stream.sync_mode.clone()),
                primary_key: stream.primary_key.clone(),
                saved_mapping: saved_mapping_body(&stream)
                    .and_then(|body| serde_json::to_value(body).ok()),
                traits,
                lists,
                samples,
            };
            Ok(ToolOutput::Context(render_stream_context(
                &rendered,
                context.privacy,
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_arguments_name_a_source_and_optionally_a_stream_and_nothing_else() {
        let schema = ReadSource::parameters().unwrap();
        let id = Uuid::now_v7().to_string();

        assert_eq!(
            schema.validate_value(&json!({"source_id": id, "stream": "contacts"})),
            Ok(())
        );
        assert_eq!(schema.validate_value(&json!({"source_id": id})), Ok(()));
        assert!(
            schema
                .validate_value(&json!({"stream": "contacts"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ReadSourceArgs>(json!({"source_id": id, "url": "x"})).is_err()
        );
        assert!(
            serde_json::from_value::<ReadSourceArgs>(json!({"source_id": "not-a-uuid"})).is_err()
        );
    }

    #[test]
    fn the_tool_reads_and_says_so() {
        let tool = ReadSource;
        assert_eq!(tool.name(), "read_source");
        assert_eq!(tool.effect(), ToolEffect::Read);
        let spec = tool.spec().unwrap();
        assert!(matches!(spec.description, Content::Instruction(_)));
        assert_eq!(
            spec.parameters.as_value()["properties"]["source_id"]["type"],
            "string"
        );
    }
}
