//! The OpenAI-compatible adapter: `POST {base_url}/chat/completions` with tools as functions.
//! Local model servers speak the same protocol, so they use this adapter with their own base URL.

use serde_json::{Value, json};
use zirv_flow_core::SecretString;

use super::{parse_arguments, user_text};
use crate::error::AiError;
use crate::http::{AiHttp, Auth, endpoint_url};
use crate::provider::{BoxFuture, Endpoint, Provider, ProviderKind};
use crate::request::{ChatRequest, Message};
use crate::response::{ChatResponse, Part, Usage, joined_text};

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenAiCompatibleProvider;

impl Provider for OpenAiCompatibleProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompatible
    }

    fn complete<'a>(
        &'a self,
        http: &'a AiHttp,
        endpoint: &'a Endpoint,
        key: &'a SecretString,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        Box::pin(async move {
            let url = endpoint_url(&endpoint.base_url, "/chat/completions")?;
            let body = serde_json::to_vec(&build_body(&endpoint.model, request))
                .map_err(|_| AiError::Malformed)?;
            let reply = http.post_json(&url, Auth::Bearer(key), &[], body).await?;
            parse_response(&reply, &endpoint.model)
        })
    }
}

fn build_body(model: &str, request: &ChatRequest) -> Value {
    let mut messages = Vec::new();
    if !request.system().is_empty() {
        messages.push(json!({"role": "system", "content": request.system_text()}));
    }
    for message in request.messages() {
        messages.push(match message {
            Message::User(contents) => json!({"role": "user", "content": user_text(contents)}),
            Message::Assistant(parts) => assistant_message(parts),
            Message::ToolResult { call_id, content } => {
                json!({"role": "tool", "tool_call_id": call_id, "content": content.render()})
            }
        });
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": request.max_output_tokens(),
    });
    if !request.tools().is_empty() {
        body["tools"] = request
            .tools()
            .iter()
            .map(|tool| {
                json!({"type": "function", "function": {
                    "name": tool.name,
                    "description": tool.description.render(),
                    "parameters": tool.parameters.as_value(),
                }})
            })
            .collect();
    }
    body
}

fn assistant_message(parts: &[Part]) -> Value {
    let text = joined_text(parts);
    let calls: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part {
            Part::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            })),
            Part::Text { .. } => None,
        })
        .collect();
    let mut message = json!({"role": "assistant", "content": text});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    message
}

fn parse_response(reply: &[u8], requested_model: &str) -> Result<ChatResponse, AiError> {
    let value: Value = serde_json::from_slice(reply).map_err(|_| AiError::Malformed)?;
    let message = value
        .pointer("/choices/0/message")
        .filter(|message| message.is_object())
        .ok_or(AiError::Malformed)?;

    let mut parts = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        parts.push(Part::Text {
            text: text.to_owned(),
        });
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = call.pointer("/function/name").and_then(Value::as_str);
        let (Some(id), Some(name)) = (call.get("id").and_then(Value::as_str), name) else {
            return Err(AiError::Malformed);
        };
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .ok_or(AiError::Capability("tool_calls".to_owned()))?;
        parts.push(Part::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: parse_arguments(arguments)?,
        });
    }

    let count = |field: &str| {
        value
            .pointer(&format!("/usage/{field}"))
            .and_then(Value::as_u64)
            .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX))
    };
    let (Some(input_tokens), Some(output_tokens)) =
        (count("prompt_tokens"), count("completion_tokens"))
    else {
        return Err(AiError::Capability("usage".to_owned()));
    };
    Ok(ChatResponse {
        parts,
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::reply;
    use crate::providers::testing::{loopback_http, serve};
    use crate::request::{Content, Privacy, RequestBuilder, ToolSpec};
    use crate::schema::JsonSchema;

    const TEXT: &str = r#"{"id":"chatcmpl-1","model":"gpt-test-2025","choices":[{"index":0,
        "message":{"role":"assistant","content":"Hello there"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}"#;

    const TOOL_CALL: &str = r#"{"id":"chatcmpl-2","model":"gpt-test-2025","choices":[{"index":0,
        "message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_9","type":"function",
        "function":{"name":"map_columns","arguments":"{\"email\":\"Email\"}"}}]},
        "finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":40,"completion_tokens":11,"total_tokens":51}}"#;

    fn endpoint(base: &str) -> Endpoint {
        Endpoint {
            base_url: format!("{base}/v1"),
            model: "gpt-test".to_owned(),
        }
    }

    fn request() -> ChatRequest {
        let parameters = JsonSchema::from_value(json!({
            "type": "object",
            "properties": {"email": {"type": "string"}},
            "required": ["email"]
        }))
        .unwrap();
        RequestBuilder::new(Privacy::SchemaOnly)
            .system(Content::Instruction("You map columns.".into()))
            .unwrap()
            .user(vec![Content::Schema("Columns: Email".into())])
            .unwrap()
            .tool(ToolSpec {
                name: "map_columns".into(),
                description: Content::Instruction("Maps columns".into()),
                parameters,
            })
            .unwrap()
            .max_output_tokens(256)
            .build()
    }

    async fn call(base: &str) -> Result<ChatResponse, AiError> {
        let key = SecretString::new("sk-test-key".to_owned());
        OpenAiCompatibleProvider
            .complete(&loopback_http(), &endpoint(base), &key, &request())
            .await
    }

    #[tokio::test]
    async fn a_text_answer_becomes_a_text_part_with_usage() {
        let (base, _) = serve(200, TEXT).await;
        let response = call(&base).await.unwrap();

        assert_eq!(
            response.parts,
            vec![Part::Text {
                text: "Hello there".into()
            }]
        );
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 3
            }
        );
        assert_eq!(response.model, "gpt-test-2025");
    }

    #[tokio::test]
    async fn a_tool_call_becomes_a_tool_call_part_with_parsed_arguments() {
        let (base, _) = serve(200, TOOL_CALL).await;
        let response = call(&base).await.unwrap();

        assert_eq!(
            response.parts,
            vec![Part::ToolCall {
                id: "call_9".into(),
                name: "map_columns".into(),
                arguments: json!({"email": "Email"}),
            }]
        );
        assert_eq!(response.usage.output_tokens, 11);
    }

    #[tokio::test]
    async fn the_request_is_a_chat_completion_with_a_bearer_key_and_function_tools() {
        let (base, seen) = serve(200, TEXT).await;
        call(&base).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let recorded = &seen[0];
        assert_eq!(recorded.path, "/v1/chat/completions");
        assert_eq!(recorded.headers["authorization"], "Bearer sk-test-key");
        let body = &recorded.body;
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You map columns.");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Columns: Email");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "map_columns");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["required"],
            json!(["email"])
        );
    }

    #[tokio::test]
    async fn a_repair_conversation_replays_the_tool_call_and_its_result() {
        let (base, seen) = serve(200, TEXT).await;
        let repaired = request().with_repair(
            &reply(vec![Part::ToolCall {
                id: "call_9".into(),
                name: "map_columns".into(),
                arguments: json!({}),
            }]),
            &["$.email: is required".to_owned()],
        );
        let key = SecretString::new("k".to_owned());
        OpenAiCompatibleProvider
            .complete(&loopback_http(), &endpoint(&base), &key, &repaired)
            .await
            .unwrap();

        let body = seen.lock().unwrap()[0].body.clone();
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_9");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{}"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "call_9");
        assert!(
            body["messages"][3]["content"]
                .as_str()
                .unwrap()
                .contains("$.email: is required")
        );
    }

    #[tokio::test]
    async fn an_error_status_is_reported_without_the_body() {
        let (base, _) = serve(429, r#"{"error":{"message":"PROMPT-ECHO"}}"#).await;
        let error = call(&base).await.unwrap_err();

        assert_eq!(error, AiError::Http(429));
        assert!(!format!("{error:?}").contains("PROMPT-ECHO"));
    }

    #[tokio::test]
    async fn an_answer_without_usage_lacks_the_usage_capability() {
        let reply = r#"{"model":"m","choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;
        let (base, _) = serve(200, reply).await;

        assert_eq!(
            call(&base).await.unwrap_err(),
            AiError::Capability("usage".into())
        );
    }

    #[tokio::test]
    async fn a_tool_call_that_is_not_json_lacks_the_tool_call_capability() {
        let reply = r#"{"model":"m","choices":[{"message":{"role":"assistant","content":null,
            "tool_calls":[{"id":"c","type":"function","function":{"name":"map_columns",
            "arguments":"{oops"}}]}}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
        let (base, _) = serve(200, reply).await;

        assert_eq!(
            call(&base).await.unwrap_err(),
            AiError::Capability("tool_calls".into())
        );
    }

    #[tokio::test]
    async fn unusable_bodies_are_malformed() {
        for reply in ["not json", r#"{"choices":[]}"#, r#"{"choices":[{"x":1}]}"#] {
            let (base, _) = serve(200, reply).await;
            assert_eq!(
                call(&base).await.unwrap_err(),
                AiError::Malformed,
                "{reply}"
            );
        }
    }

    #[tokio::test]
    async fn a_private_base_url_is_refused_before_any_request() {
        let key = SecretString::new("k".to_owned());
        let endpoint = Endpoint {
            base_url: "http://10.0.0.5:8080/v1".to_owned(),
            model: "m".to_owned(),
        };
        let error = OpenAiCompatibleProvider
            .complete(&loopback_http(), &endpoint, &key, &request())
            .await
            .unwrap_err();
        assert!(matches!(error, AiError::Egress { .. }), "{error}");
    }
}
