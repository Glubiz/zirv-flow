//! A deterministic provider for tests: it replays a queue of scripted answers and records every
//! request it receives, so tests can assert what was (or was not) sent. It never touches the
//! network, which keeps CI away from real model APIs.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use zirv_flow_core::SecretString;

use crate::error::AiError;
use crate::http::AiHttp;
use crate::provider::{BoxFuture, Endpoint, Provider, ProviderKind};
use crate::request::ChatRequest;
use crate::response::{ChatResponse, Part, Usage};

/// The scripted provider. Each call takes the next scripted answer; with none left it fails.
#[derive(Default)]
pub struct FakeProvider {
    script: Mutex<VecDeque<Result<ChatResponse, AiError>>>,
    seen: Mutex<Vec<ChatRequest>>,
    delay: Duration,
}

impl FakeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes every call take this long to answer, as a slow provider would; the request is
    /// recorded when the call starts.
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Queues an answer.
    pub fn push(&self, answer: Result<ChatResponse, AiError>) -> &Self {
        self.script.lock().unwrap().push_back(answer);
        self
    }

    /// The requests received so far, oldest first.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// A text answer with the given usage.
pub fn text_response(text: &str, input_tokens: u32, output_tokens: u32) -> ChatResponse {
    ChatResponse {
        parts: vec![Part::Text {
            text: text.to_owned(),
        }],
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        model: "fake-model".to_owned(),
    }
}

/// An answer made of the given parts, with no usage.
pub fn reply(parts: Vec<Part>) -> ChatResponse {
    ChatResponse {
        parts,
        usage: Usage::default(),
        model: "fake-model".to_owned(),
    }
}

/// A one-tool-call answer with the given usage.
pub fn tool_call_response(
    name: &str,
    arguments: Value,
    input_tokens: u32,
    output_tokens: u32,
) -> ChatResponse {
    ChatResponse {
        parts: vec![Part::ToolCall {
            id: "call_1".to_owned(),
            name: name.to_owned(),
            arguments,
        }],
        usage: Usage {
            input_tokens,
            output_tokens,
        },
        model: "fake-model".to_owned(),
    }
}

impl Provider for FakeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompatible
    }

    fn complete<'a>(
        &'a self,
        _http: &'a AiHttp,
        _endpoint: &'a Endpoint,
        _key: &'a SecretString,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(request.clone());
            tokio::time::sleep(self.delay).await;
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(AiError::Transport("no scripted answer".to_owned())))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::http::AiHttpLimits;
    use crate::request::{Content, Privacy, RequestBuilder};

    fn http() -> AiHttp {
        AiHttp::new(Arc::new(EgressPolicy::default()), AiHttpLimits::default()).unwrap()
    }

    fn request(text: &str) -> ChatRequest {
        RequestBuilder::new(Privacy::SchemaOnly)
            .user(vec![Content::Schema(text.to_owned())])
            .unwrap()
            .build()
    }

    async fn call(fake: &FakeProvider, text: &str) -> Result<ChatResponse, AiError> {
        let endpoint = Endpoint {
            base_url: "http://unused.test".into(),
            model: "m".into(),
        };
        let key = SecretString::new("k".into());
        fake.complete(&http(), &endpoint, &key, &request(text))
            .await
    }

    #[tokio::test]
    async fn it_replays_the_script_in_order_and_records_every_request() {
        let fake = FakeProvider::new();
        fake.push(Ok(text_response("one", 1, 2)))
            .push(Err(AiError::Timeout))
            .push(Ok(tool_call_response("t", json!({"a": 1}), 3, 4)));

        assert_eq!(call(&fake, "first").await.unwrap().text(), "one");
        assert_eq!(call(&fake, "second").await.unwrap_err(), AiError::Timeout);
        let third = call(&fake, "third").await.unwrap();
        assert_eq!(third.tool_calls().count(), 1);

        assert_eq!(fake.call_count(), 3);
        let rendered: Vec<String> = fake
            .requests()
            .iter()
            .map(|request| request.contents().next().unwrap().render())
            .collect();
        assert_eq!(rendered, ["first", "second", "third"]);
    }

    #[tokio::test]
    async fn an_empty_script_fails_instead_of_inventing_an_answer() {
        let fake = FakeProvider::new();
        assert!(matches!(
            call(&fake, "x").await.unwrap_err(),
            AiError::Transport(_)
        ));
        assert_eq!(fake.call_count(), 1);
    }
}
