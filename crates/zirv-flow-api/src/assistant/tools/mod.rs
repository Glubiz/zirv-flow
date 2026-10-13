//! The assistant's tools (architecture 5.3, 13.2): the only things a model can ask the product to
//! do. There are three, and the registry below is the only list of them:
//!
//! - `read_source` (read): what the model may know about a source's stream or manifest;
//! - `draft_mapping` (draft): checks a mapping with the validator a stream save uses;
//! - `draft_manifest` (draft): checks a manifest with the validator a source save uses.
//!
//! A tool never writes. It is told who is asking and in which workspace, and it authorizes that
//! user again on every call; the model cannot name a target, only fill in the artifact. A new
//! tool is one module and one line in [`registry`].

pub(crate) mod draft_manifest;
pub(crate) mod draft_mapping;
pub(crate) mod read_source;

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_ai::{AiTool, JsonSchema, Privacy, ToolError};
use zirv_flow_core::WorkspaceId;

use crate::AppState;
use crate::auth::{Need, Principal, authorize};
use crate::problem::ApiError;

/// What a draft run targets. The model cannot change it.
#[derive(Clone, Debug)]
pub(crate) enum DraftTarget {
    /// A stream's mapping.
    Mapping { source_id: Uuid, stream: String },
    /// An `http` source's manifest. Whether it is saved yet only matters to the dry run.
    Manifest,
}

/// The caller of a tool: the signed-in user and the workspace, taken from the authorized request.
pub(crate) struct ToolContext {
    pub(crate) state: AppState,
    pub(crate) principal: Principal,
    pub(crate) workspace: WorkspaceId,
    pub(crate) privacy: Privacy,
    pub(crate) target: DraftTarget,
    /// For a webhook stream: the sample the admin pasted, whose top-level keys are the stream's
    /// fields for this run.
    pub(crate) sample: Option<Vec<Value>>,
}

pub(crate) type Tool = Box<dyn AiTool<ToolContext, Error = ApiError>>;

/// Every tool there is, one line each.
pub(crate) fn registry() -> Vec<Tool> {
    vec![
        Box::new(read_source::ReadSource),
        Box::new(draft_mapping::DraftMapping),
        Box::new(draft_manifest::DraftManifest),
    ]
}

/// The tool called `name`, if there is one. A name the model made up is none.
pub(crate) fn find(name: &str) -> Option<Tool> {
    registry().into_iter().find(|tool| tool.name() == name)
}

/// Authorizes the context's user as a workspace admin, again. A user whose role was taken away
/// since the run began is refused here, at the next call.
pub(super) async fn authorize_again(context: &ToolContext) -> Result<(), ToolError<ApiError>> {
    authorize(
        &context.state,
        &context.principal,
        context.workspace,
        Need::Admin,
    )
    .await
    .map(|_| ())
    .map_err(ToolError::Failed)
}

/// Checks arguments against the tool's schema. The lines name a path and a rule, never a value.
pub(super) fn check_arguments(
    schema: &JsonSchema,
    arguments: &Value,
) -> Result<(), ToolError<ApiError>> {
    schema.validate_value(arguments).map_err(ToolError::Invalid)
}

/// The text of a refused argument, fixed so that it never echoes what the model sent.
pub(super) fn arguments_invalid() -> ToolError<ApiError> {
    ToolError::Invalid(vec!["arguments_invalid".to_owned()])
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use zirv_flow_ai::{Content, ToolEffect};

    use super::*;

    #[test]
    fn the_registry_holds_only_read_and_draft_tools() {
        let tools = registry();

        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["read_source", "draft_mapping", "draft_manifest"]);
        assert_eq!(names.iter().collect::<HashSet<_>>().len(), names.len());
        for name in &names {
            for forbidden in [
                "publish",
                "send",
                "delete",
                "remove",
                "erase",
                "consent",
                "suppress",
                "subscribe",
                "run",
                "sync",
                "update",
                "set",
            ] {
                assert!(!name.contains(forbidden), "{name} contains {forbidden}");
            }
        }
        // `ToolEffect` has two variants, so this match is exhaustive: a third effect would not
        // compile here.
        let reads: Vec<&str> = tools
            .iter()
            .filter(|tool| match tool.effect() {
                ToolEffect::Read => true,
                ToolEffect::Draft => false,
            })
            .map(|tool| tool.name())
            .collect();
        assert_eq!(reads, ["read_source"]);
        for tool in &tools {
            let spec = tool.spec().unwrap();
            assert_eq!(spec.name, tool.name());
            assert!(matches!(spec.description, Content::Instruction(_)));
        }
    }

    #[test]
    fn a_name_the_model_made_up_finds_no_tool() {
        for name in ["publish_mapping", "delete_source", "send_campaign", ""] {
            assert!(find(name).is_none(), "{name}");
        }
        assert!(find("draft_mapping").is_some());
    }

    #[test]
    fn tool_modules_write_nothing() {
        let sources = [
            include_str!("read_source.rs"),
            include_str!("draft_mapping.rs"),
            include_str!("draft_manifest.rs"),
        ];
        for source in sources {
            // The tests of a module may name what it must not do.
            let code = source.split("#[cfg(test)]").next().unwrap();
            for forbidden in [
                "begin(",
                "commit(",
                "admin::",
                "insert_",
                "delete_",
                "set_stream_mapping",
                "configure(",
                "queue_run",
            ] {
                assert!(!code.contains(forbidden), "{forbidden}");
            }
        }
    }

    #[test]
    fn invalid_arguments_are_a_fixed_line() {
        match arguments_invalid() {
            ToolError::Invalid(lines) => assert_eq!(lines, ["arguments_invalid"]),
            ToolError::Failed(_) => panic!("not a failure"),
        }
    }
}
