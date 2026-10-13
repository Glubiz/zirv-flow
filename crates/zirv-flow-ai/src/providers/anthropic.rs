//! The Anthropic adapter: `POST {base_url}/v1/messages` with `x-api-key` and a pinned
//! `anthropic-version`.

use serde_json::{Value, json};
use zirv_flow_core::SecretString;

use super::user_text;
use crate::error::AiError;
use crate::http::{AiHttp, Auth, endpoint_url};
use crate::provider::{BoxFuture, Endpoint, Provider, ProviderKind};
use crate::request::{ChatRequest, Message};
use crate::response::{ChatResponse, Part, Usage};

const API_VERSION: &str = "2023-06-01";

#[derive(Clone, Copy, Debug, Default)]
pub struct AnthropicProvider;

impl Provider for AnthropicProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Anthropic
    }

    fn complete<'a>(
        &'a self,
        http: &'a AiHttp,
        endpoint: &'a Endpoint,
        key: &'a SecretString,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        Box::pin(async move {
            let url = endpoint_url(&endpoint.base_url, "/v1/messages")?;
            let body = serde_json::to_vec(&build_body(&endpoint.model, request))
                .map_err(|_| AiError::Malformed)?;
            let reply = http
                .post_json(
                    &url,
                    Auth::Header("x-api-key", key),
                    &[("anthropic-version", API_VERSION)],
                    body,
                )
                .await?;
            parse_response(&reply, &endpoint.model)
        })
    }
}

fn build_body(model: &str, request: &ChatRequest) -> Value {
    // Anthropic wants strictly alternating roles, and every tool result of one turn in the single
    // user message that follows it, so adjacent user-side blocks are merged.
    let mut messages: Vec<(&'static str, Vec<Value>)> = Vec::new();
    let mut push = |role: &'static str, blocks: Vec<Value>| match messages.last_mut() {
        Some((last, existing)) if *last == role => existing.extend(blocks),
        _ => messages.push((role, blocks)),
    };
    for message in request.messages() {
        match message {
            Message::User(contents) => {
                push(
                    "user",
                    vec![json!({"type": "text", "text": user_text(contents)})],
                );
            }
            Message::Assistant(parts) => {
                let blocks = parts
                    .iter()
                    .map(|part| match part {
                        Part::Text { text } => json!({"type": "text", "text": text}),
                        Part::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            json!({"type": "tool_use", "id": id, "name": name, "input": arguments})
                        }
                    })
                    .collect();
                push("assistant", blocks);
            }
            Message::ToolResult { call_id, content } => push(
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content.render(),
                })],
            ),
        }
    }
    let mut body = json!({
        "model": model,
        "max_tokens": request.max_output_tokens(),
        "messages": messages
            .into_iter()
            .map(|(role, content)| json!({"role": role, "content": content}))
            .collect::<Vec<_>>(),
    });
    if !request.system().is_empty() {
        body["system"] = Value::String(request.system_text());
    }
    if !request.tools().is_empty() {
        body["tools"] = request
            .tools()
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters.as_value(),
                })
            })
            .collect();
    }
    body
}

fn parse_response(reply: &[u8], requested_model: &str) -> Result<ChatResponse, AiError> {
    let value: Value = serde_json::from_slice(reply).map_err(|_| AiError::Malformed)?;
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or(AiError::Malformed)?;

    let mut parts = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(AiError::Malformed)?;
                if !text.is_empty() {
                    parts.push(Part::Text {
                        text: text.to_owned(),
                    });
                }
            }
            Some("tool_use") => {
                let field = |name: &str| block.get(name).and_then(Value::as_str);
                let (Some(id), Some(name), Some(input)) =
                    (field("id"), field("name"), block.get("input"))
                else {
                    return Err(AiError::Malformed);
                };
                parts.push(Part::ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: input.clone(),
                });
            }
            // Thinking and other block types carry nothing the product uses.
            _ => {}
        }
    }

    let count = |field: &str| {
        value
            .pointer(&format!("/usage/{field}"))
            .and_then(Value::as_u64)
            .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX))
    };
    let (Some(input_tokens), Some(output_tokens)) = (count("input_tokens"), count("output_tokens"))
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
    use crate::providers::testing::{loopback_http, serve};
    use crate::request::{Content, Privacy, RequestBuilder, ToolSpec};
    use crate::schema::JsonSchema;

    const TEXT: &str = r#"{"id":"msg_1","type":"message","role":"assistant",
        "model":"claude-test-1","content":[{"type":"text","text":"Hello there"}],
        "stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":3}}"#;

    const TOOL_USE: &str = r#"{"id":"msg_2","type":"message","role":"assistant",
        "model":"claude-test-1","content":[{"type":"text","text":"Mapping."},
        {"type":"tool_use","id":"toolu_9","name":"map_columns","input":{"email":"Email"}}],
        "stop_reason":"tool_use","usage":{"input_tokens":40,"output_tokens":11}}"#;

    fn endpoint(base: &str) -> Endpoint {
        Endpoint {
            base_url: base.to_owned(),
            model: "claude-test".to_owned(),
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
                description: "Maps columns".into(),
                parameters,
            })
            .max_output_tokens(256)
            .build()
    }

    async fn call(base: &str) -> Result<ChatResponse, AiError> {
        let key = SecretString::new("sk-ant-test".to_owned());
        AnthropicProvider
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
        assert_eq!(response.model, "claude-test-1");
    }

    #[tokio::test]
    async fn a_tool_use_block_becomes_a_tool_call_part() {
        let (base, _) = serve(200, TOOL_USE).await;
        let response = call(&base).await.unwrap();

        assert_eq!(response.parts.len(), 2);
        assert_eq!(
            response.parts[1],
            Part::ToolCall {
                id: "toolu_9".into(),
                name: "map_columns".into(),
                arguments: json!({"email": "Email"}),
            }
        );
        assert_eq!(response.text(), "Mapping.");
    }

    #[tokio::test]
    async fn the_request_is_a_messages_call_with_the_key_header_and_input_schema_tools() {
        let (base, seen) = serve(200, TEXT).await;
        call(&base).await.unwrap();

        let seen = seen.lock().unwrap();
        let recorded = &seen[0];
        assert_eq!(recorded.path, "/v1/messages");
        assert_eq!(recorded.headers["x-api-key"], "sk-ant-test");
        assert_eq!(recorded.headers["anthropic-version"], API_VERSION);
        assert!(recorded.headers.get("authorization").is_none());
        let body = &recorded.body;
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["system"], "You map columns.");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "Columns: Email");
        assert_eq!(body["tools"][0]["name"], "map_columns");
        assert_eq!(
            body["tools"][0]["input_schema"]["required"],
            json!(["email"])
        );
    }

    #[tokio::test]
    async fn a_repair_conversation_replays_tool_use_and_answers_in_one_user_message() {
        let (base, seen) = serve(200, TEXT).await;
        let repaired = request().with_repair(
            &[
                Part::ToolCall {
                    id: "toolu_1".into(),
                    name: "map_columns".into(),
                    arguments: json!({}),
                },
                Part::ToolCall {
                    id: "toolu_2".into(),
                    name: "map_columns".into(),
                    arguments: json!({}),
                },
            ],
            &["$.email: is required".to_owned()],
        );
        let key = SecretString::new("k".to_owned());
        AnthropicProvider
            .complete(&loopback_http(), &endpoint(&base), &key, &repaired)
            .await
            .unwrap();

        let body = seen.lock().unwrap()[0].body.clone();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[2]["content"][1]["tool_use_id"], "toolu_2");
    }

    #[tokio::test]
    async fn an_error_status_is_reported_without_the_body() {
        let (base, _) = serve(529, r#"{"error":{"message":"PROMPT-ECHO"}}"#).await;
        let error = call(&base).await.unwrap_err();

        assert_eq!(error, AiError::Http(529));
        assert!(!format!("{error:?}").contains("PROMPT-ECHO"));
    }

    #[tokio::test]
    async fn an_answer_without_usage_lacks_the_usage_capability() {
        let reply = r#"{"model":"m","content":[{"type":"text","text":"hi"}]}"#;
        let (base, _) = serve(200, reply).await;

        assert_eq!(
            call(&base).await.unwrap_err(),
            AiError::Capability("usage".into())
        );
    }

    #[tokio::test]
    async fn unusable_bodies_are_malformed() {
        for reply in [
            "not json",
            r#"{"usage":{"input_tokens":1,"output_tokens":1}}"#,
            r#"{"content":[{"type":"tool_use","id":"x"}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
        ] {
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
            base_url: "http://192.168.1.10:9000".to_owned(),
            model: "m".to_owned(),
        };
        let error = AnthropicProvider
            .complete(&loopback_http(), &endpoint, &key, &request())
            .await
            .unwrap_err();
        assert!(matches!(error, AiError::Egress { .. }), "{error}");
    }
}
