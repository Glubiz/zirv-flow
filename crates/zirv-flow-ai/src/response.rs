//! What every provider's answer is reduced to: text, tool calls and usage. Product code never
//! branches on the provider.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One piece of a model's answer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// Tokens the provider reported for one call.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// A provider's answer, the same shape for every adapter. The fields are private and only this
/// crate builds one (the adapters and the fake), so a value of this type always came from a
/// provider: a caller cannot forge one to replay as assistant history.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ChatResponse {
    parts: Vec<Part>,
    usage: Usage,
    model: String,
}

/// The text parts, joined.
pub(crate) fn joined_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.as_str()),
            Part::ToolCall { .. } => None,
        })
        .collect()
}

impl ChatResponse {
    pub(crate) fn new(parts: Vec<Part>, usage: Usage, model: String) -> Self {
        Self {
            parts,
            usage,
            model,
        }
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// The model that answered, as the provider reports it.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The text parts, joined.
    pub fn text(&self) -> String {
        joined_text(&self.parts)
    }

    /// The tool calls as `(id, name, arguments)`.
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.parts.iter().filter_map(|part| match part {
            Part::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.as_str(), name.as_str(), arguments)),
            Part::Text { .. } => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn response() -> ChatResponse {
        ChatResponse::new(
            vec![
                Part::Text { text: "a".into() },
                Part::ToolCall {
                    id: "c1".into(),
                    name: "map".into(),
                    arguments: json!({"x": 1}),
                },
                Part::Text { text: "b".into() },
            ],
            Usage {
                input_tokens: 3,
                output_tokens: 4,
            },
            "m".into(),
        )
    }

    #[test]
    fn text_joins_the_text_parts_and_tool_calls_are_listed() {
        let response = response();
        assert_eq!(response.text(), "ab");
        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls, vec![("c1", "map", &json!({"x": 1}))]);
    }

    #[test]
    fn parts_serialize_with_a_type_tag() {
        let json = serde_json::to_value(response()).unwrap();
        assert_eq!(json["parts"][0]["type"], "text");
        assert_eq!(json["parts"][1]["type"], "tool_call");
        assert_eq!(json["model"], "m");
    }
}
