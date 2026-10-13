//! The assistant's tool seam (architecture 5.3, 13.2). The trait lives here so that this crate
//! stays free of validators and the store; the implementations live with the caller, which owns
//! the context they run in.

use serde_json::Value;

use crate::error::AiError;
use crate::provider::BoxFuture;
use crate::request::{Content, ToolSpec};

/// What a tool may do. There are exactly two effects: a tool can read, or it can produce a draft
/// for a human to review. A tool that publishes, sends, deletes or records consent would need a
/// new variant, which every exhaustive `match` on this type (the registry test among them) would
/// then refuse to compile until it is looked at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolEffect {
    /// Reads workspace data the caller is allowed to see and returns it as request content.
    Read,
    /// Validates and returns a draft artifact. Writes nothing.
    Draft,
}

/// What a successful tool call returns.
#[derive(Debug, Eq, PartialEq)]
pub enum ToolOutput {
    /// Content for the model's context, each part classified by what it may contain.
    Context(Vec<Content>),
    /// A validated, normalised draft artifact.
    Draft(Value),
}

/// Why a tool call did not produce an output.
#[derive(Debug, Eq, PartialEq)]
pub enum ToolError<E> {
    /// The arguments or the draft are not valid. These lines are shown to the model as feedback,
    /// so they name fields and codes and never carry a value.
    Invalid(Vec<String>),
    /// The call failed for a reason the model cannot fix: a refusal, a missing record, an outage.
    Failed(E),
}

/// A tool the assistant can call. `C` is the caller's context: who is asking, in which workspace.
pub trait AiTool<C>: Send + Sync {
    /// The caller's error type for a failed call.
    type Error;

    /// The name the model calls it by.
    fn name(&self) -> &'static str;

    fn effect(&self) -> ToolEffect;

    /// The description (an instruction) and the argument schema offered to the model.
    fn spec(&self) -> Result<ToolSpec, AiError>;

    /// Runs the tool for the context's user. It authorises again on every call.
    fn call<'a>(
        &'a self,
        context: &'a C,
        arguments: &'a Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError<Self::Error>>>;
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::JsonSchema;

    #[test]
    fn the_effect_enum_has_only_read_and_draft() {
        // An exhaustive match: a third effect (publish, send, delete, consent) fails to compile.
        for effect in [ToolEffect::Read, ToolEffect::Draft] {
            let name = match effect {
                ToolEffect::Read => "read",
                ToolEffect::Draft => "draft",
            };
            assert!(!name.is_empty());
        }
    }

    struct Echo;

    impl AiTool<u32> for Echo {
        type Error = &'static str;

        fn name(&self) -> &'static str {
            "echo"
        }

        fn effect(&self) -> ToolEffect {
            ToolEffect::Draft
        }

        fn spec(&self) -> Result<ToolSpec, AiError> {
            Ok(ToolSpec {
                name: self.name().to_owned(),
                description: Content::Instruction("Echoes.".into()),
                parameters: JsonSchema::from_value(json!({"type": "object"}))?,
            })
        }

        fn call<'a>(
            &'a self,
            context: &'a u32,
            arguments: &'a Value,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError<Self::Error>>> {
            Box::pin(async move {
                if *context == 0 {
                    return Err(ToolError::Failed("refused"));
                }
                Ok(ToolOutput::Draft(arguments.clone()))
            })
        }
    }

    #[tokio::test]
    async fn a_tool_runs_against_the_callers_context() {
        let tool: Box<dyn AiTool<u32, Error = &'static str>> = Box::new(Echo);
        assert_eq!(tool.spec().unwrap().name, "echo");
        let args = json!({"a": 1});
        assert_eq!(
            tool.call(&1, &args).await,
            Ok(ToolOutput::Draft(json!({"a": 1})))
        );
        assert_eq!(
            tool.call(&0, &args).await,
            Err(ToolError::Failed("refused"))
        );
    }
}
