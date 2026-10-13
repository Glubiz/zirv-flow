//! The capability probe (ruling OQ-27): one request that shows whether a model can call a tool
//! with valid arguments and reports usage. A model that cannot is refused when it is enabled.

use serde_json::json;
use zirv_flow_core::SecretString;

use crate::error::AiError;
use crate::http::AiHttp;
use crate::provider::{Endpoint, Provider};
use crate::request::{Content, Privacy, RequestBuilder, ToolSpec};
use crate::schema::JsonSchema;

/// The smallest context window a model may be configured with.
pub const MIN_CONTEXT_WINDOW_TOKENS: u32 = 32_768;

/// A capability the product requires of a model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    ToolCalls,
    Usage,
    ContextWindow,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolCalls => "tool_calls",
            Self::Usage => "usage",
            Self::ContextWindow => "context_window",
        }
    }
}

/// A missing capability and the reason it is missing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Missing {
    pub capability: Capability,
    pub reason: String,
}

impl Missing {
    fn new(capability: Capability, reason: &str) -> Self {
        Self {
            capability,
            reason: reason.to_owned(),
        }
    }
}

/// The configured context window as a capability check.
pub fn check_context_window(context_window_tokens: u32) -> Option<Missing> {
    (context_window_tokens < MIN_CONTEXT_WINDOW_TOKENS).then(|| {
        Missing::new(
            Capability::ContextWindow,
            &format!(
                "the configured context window is {context_window_tokens} tokens; at least \
                 {MIN_CONTEXT_WINDOW_TOKENS} are required"
            ),
        )
    })
}

fn probe_request() -> Result<(crate::request::ChatRequest, JsonSchema), AiError> {
    let parameters = JsonSchema::from_value(json!({
        "type": "object",
        "description": "Report the probe result.",
        "properties": {"status": {"enum": ["ready", "blocked"]}},
        "required": ["status"]
    }))?;
    let request = RequestBuilder::new(Privacy::SchemaOnly)
        .system(Content::Instruction(
            "This is a capability check. Answer only by calling the tool.".into(),
        ))?
        .user(vec![Content::Instruction(
            "Call the probe tool with status set to ready.".into(),
        )])?
        .tool(ToolSpec {
            name: "probe".to_owned(),
            description: "Reports the result of a capability check.".to_owned(),
            parameters: parameters.clone(),
        })
        .max_output_tokens(64)
        .build();
    Ok((request, parameters))
}

/// Sends the probe and returns the capabilities the model lacks; empty means it passed. A
/// transport, status or egress failure is an error, not a missing capability.
pub async fn probe(
    provider: &dyn Provider,
    http: &AiHttp,
    endpoint: &Endpoint,
    key: &SecretString,
) -> Result<Vec<Missing>, AiError> {
    let (request, parameters) = probe_request()?;
    let response = match provider.complete(http, endpoint, key, &request).await {
        Ok(response) => response,
        Err(AiError::Capability(name)) if name == Capability::Usage.as_str() => {
            return Ok(vec![Missing::new(
                Capability::Usage,
                "the model did not report token usage",
            )]);
        }
        Err(AiError::Capability(name)) if name == Capability::ToolCalls.as_str() => {
            return Ok(vec![Missing::new(
                Capability::ToolCalls,
                "the model returned a tool call that was not valid JSON",
            )]);
        }
        Err(error) => return Err(error),
    };

    let mut missing = Vec::new();
    match response.tool_calls().find(|(_, name, _)| *name == "probe") {
        None => missing.push(Missing::new(
            Capability::ToolCalls,
            "the model did not call the probe tool",
        )),
        Some((_, _, arguments)) => {
            if let Err(errors) = parameters.validate_value(arguments) {
                missing.push(Missing::new(
                    Capability::ToolCalls,
                    &format!(
                        "the probe tool call had invalid arguments: {}",
                        errors.join("; ")
                    ),
                ));
            }
        }
    }
    if response.usage.input_tokens == 0 && response.usage.output_tokens == 0 {
        missing.push(Missing::new(
            Capability::Usage,
            "the model reported zero token usage",
        ));
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::fake::{FakeProvider, text_response, tool_call_response};
    use crate::http::AiHttpLimits;

    async fn run(fake: &FakeProvider) -> Result<Vec<Missing>, AiError> {
        let http = AiHttp::new(Arc::new(EgressPolicy::default()), AiHttpLimits::default()).unwrap();
        let endpoint = Endpoint {
            base_url: "http://unused.test".into(),
            model: "m".into(),
        };
        probe(fake, &http, &endpoint, &SecretString::new("k".into())).await
    }

    fn capabilities(missing: &[Missing]) -> Vec<Capability> {
        missing.iter().map(|m| m.capability).collect()
    }

    #[tokio::test]
    async fn a_model_that_calls_the_tool_with_valid_arguments_and_reports_usage_passes() {
        let fake = FakeProvider::new();
        fake.push(Ok(tool_call_response(
            "probe",
            json!({"status": "ready"}),
            20,
            5,
        )));

        assert_eq!(run(&fake).await.unwrap(), vec![]);

        let request = &fake.requests()[0];
        assert_eq!(request.tools()[0].name, "probe");
        assert!(request.contents().all(|content| !content.is_personal()));
    }

    #[tokio::test]
    async fn a_model_that_answers_in_text_lacks_tool_calls() {
        let fake = FakeProvider::new();
        fake.push(Ok(text_response("ready", 20, 5)));

        let missing = run(&fake).await.unwrap();

        assert_eq!(capabilities(&missing), vec![Capability::ToolCalls]);
        assert!(missing[0].reason.contains("did not call"));
    }

    #[tokio::test]
    async fn a_tool_call_with_invalid_arguments_lacks_tool_calls() {
        for arguments in [json!({}), json!({"status": "maybe"}), json!({"status": 3})] {
            let fake = FakeProvider::new();
            fake.push(Ok(tool_call_response("probe", arguments, 20, 5)));

            let missing = run(&fake).await.unwrap();

            assert_eq!(capabilities(&missing), vec![Capability::ToolCalls]);
            assert!(missing[0].reason.contains("invalid arguments"));
        }
    }

    #[tokio::test]
    async fn zero_usage_lacks_usage() {
        let fake = FakeProvider::new();
        fake.push(Ok(tool_call_response(
            "probe",
            json!({"status": "ready"}),
            0,
            0,
        )));

        assert_eq!(
            capabilities(&run(&fake).await.unwrap()),
            vec![Capability::Usage]
        );
    }

    #[tokio::test]
    async fn adapter_capability_errors_become_missing_capabilities() {
        for (name, expected) in [
            ("usage", Capability::Usage),
            ("tool_calls", Capability::ToolCalls),
        ] {
            let fake = FakeProvider::new();
            fake.push(Err(AiError::Capability(name.to_owned())));
            assert_eq!(capabilities(&run(&fake).await.unwrap()), vec![expected]);
        }
    }

    #[tokio::test]
    async fn other_failures_are_errors_and_not_missing_capabilities() {
        let fake = FakeProvider::new();
        fake.push(Err(AiError::Http(401)));

        assert_eq!(run(&fake).await.unwrap_err(), AiError::Http(401));
    }

    #[test]
    fn a_context_window_below_32k_is_missing_with_the_reason() {
        let missing = check_context_window(8_192).unwrap();
        assert_eq!(missing.capability, Capability::ContextWindow);
        assert!(missing.reason.contains("8192"), "{}", missing.reason);
        assert!(check_context_window(32_767).is_some());
        assert!(check_context_window(32_768).is_none());
        assert!(check_context_window(200_000).is_none());
    }

    #[test]
    fn capabilities_have_stable_names() {
        assert_eq!(Capability::ToolCalls.as_str(), "tool_calls");
        assert_eq!(Capability::Usage.as_str(), "usage");
        assert_eq!(Capability::ContextWindow.as_str(), "context_window");
    }
}
