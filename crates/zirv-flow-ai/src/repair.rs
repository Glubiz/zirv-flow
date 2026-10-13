//! Bounded repair: when a model's answer fails validation, show it the errors and ask again, a
//! fixed number of times at most.

use zirv_flow_core::SecretString;

use crate::error::AiError;
use crate::http::AiHttp;
use crate::provider::{BoxFuture, Endpoint, Provider};
use crate::request::ChatRequest;
use crate::response::ChatResponse;

/// Something that dispatches one request. The API implements it with the gates in front, so every
/// repair attempt is a separate, gated dispatch; [`ProviderSender`] calls a provider directly.
pub trait Sender: Sync {
    fn send<'a>(&'a self, request: &'a ChatRequest)
    -> BoxFuture<'a, Result<ChatResponse, AiError>>;
}

/// Sends straight to a provider, with no gates.
pub struct ProviderSender<'a> {
    pub provider: &'a dyn Provider,
    pub http: &'a AiHttp,
    pub endpoint: &'a Endpoint,
    pub key: &'a SecretString,
}

impl Sender for ProviderSender<'_> {
    fn send<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        self.provider
            .complete(self.http, self.endpoint, self.key, request)
    }
}

/// How a repair loop ended.
#[derive(Debug, Eq, PartialEq)]
pub enum RepairOutcome<T> {
    /// The answer validated on this dispatch (the first is 1).
    Valid(T, u32),
    /// Every attempt failed validation; these are the last attempt's errors.
    Exhausted(Vec<String>),
}

/// Dispatches `request`, validates the answer and, on failure, dispatches again with the errors,
/// at most `max_repairs` times after the first dispatch. A dispatch error ends the loop at once.
pub async fn repair<T, S, V>(
    max_repairs: u32,
    request: &ChatRequest,
    sender: &S,
    validate: V,
) -> Result<RepairOutcome<T>, AiError>
where
    S: Sender + ?Sized,
    V: Fn(&ChatResponse) -> Result<T, Vec<String>>,
{
    let mut current = request.clone();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = sender.send(&current).await?;
        match validate(&response) {
            Ok(value) => return Ok(RepairOutcome::Valid(value, attempts)),
            Err(errors) if attempts > max_repairs => return Ok(RepairOutcome::Exhausted(errors)),
            Err(errors) => current = current.with_repair(&response, &errors),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::fake::{FakeProvider, text_response, tool_call_response};
    use crate::http::AiHttpLimits;
    use crate::request::{Content, Message, Privacy, RequestBuilder};
    use std::sync::Arc;
    use zirv_flow_core::egress::EgressPolicy;

    struct Fixture {
        fake: FakeProvider,
        http: AiHttp,
        endpoint: Endpoint,
        key: SecretString,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                fake: FakeProvider::new(),
                http: AiHttp::new(Arc::new(EgressPolicy::default()), AiHttpLimits::default())
                    .unwrap(),
                endpoint: Endpoint {
                    base_url: "http://unused.test".into(),
                    model: "m".into(),
                },
                key: SecretString::new("k".into()),
            }
        }

        fn sender(&self) -> ProviderSender<'_> {
            ProviderSender {
                provider: &self.fake,
                http: &self.http,
                endpoint: &self.endpoint,
                key: &self.key,
            }
        }
    }

    fn request() -> ChatRequest {
        RequestBuilder::new(Privacy::SchemaOnly)
            .user(vec![Content::Schema("map".into())])
            .unwrap()
            .build()
    }

    fn wants_ok(response: &ChatResponse) -> Result<String, Vec<String>> {
        match response.text().as_str() {
            "ok" => Ok("ok".to_owned()),
            other => Err(vec![format!("expected ok, got {other}")]),
        }
    }

    #[tokio::test]
    async fn a_valid_first_answer_costs_one_dispatch() {
        let fixture = Fixture::new();
        fixture.fake.push(Ok(text_response("ok", 1, 1)));

        let outcome = repair(2, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap();

        assert_eq!(outcome, RepairOutcome::Valid("ok".to_owned(), 1));
        assert_eq!(fixture.fake.call_count(), 1);
    }

    #[tokio::test]
    async fn an_answer_that_validates_on_the_second_attempt_is_returned_with_the_count() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(text_response("nope", 1, 1)))
            .push(Ok(text_response("ok", 1, 1)));

        let outcome = repair(2, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap();

        assert_eq!(outcome, RepairOutcome::Valid("ok".to_owned(), 2));
        let second = &fixture.fake.requests()[1];
        assert!(matches!(second.messages()[1], Message::Assistant(_)));
        let feedback = second.contents().last().unwrap().render();
        assert!(feedback.contains("expected ok, got nope"), "{feedback}");
    }

    #[tokio::test]
    async fn repair_stops_after_the_configured_attempts_and_returns_the_last_errors() {
        let fixture = Fixture::new();
        for word in ["a", "b", "c", "d", "e"] {
            fixture.fake.push(Ok(text_response(word, 1, 1)));
        }

        let outcome = repair(2, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RepairOutcome::Exhausted(vec!["expected ok, got c".to_owned()])
        );
        assert_eq!(fixture.fake.call_count(), 3);
    }

    #[tokio::test]
    async fn zero_repairs_means_a_single_dispatch() {
        let fixture = Fixture::new();
        fixture.fake.push(Ok(text_response("bad", 1, 1)));

        let outcome = repair(0, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap();

        assert!(matches!(outcome, RepairOutcome::Exhausted(_)));
        assert_eq!(fixture.fake.call_count(), 1);
    }

    #[tokio::test]
    async fn a_dispatch_error_ends_the_loop_at_once() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(text_response("bad", 1, 1)))
            .push(Err(AiError::BudgetExceeded));

        let error = repair(5, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap_err();

        assert_eq!(error, AiError::BudgetExceeded);
        assert_eq!(fixture.fake.call_count(), 2);
    }

    #[tokio::test]
    async fn a_tool_call_answer_is_replayed_with_a_tool_result() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(tool_call_response("map", json!({}), 1, 1)))
            .push(Ok(text_response("ok", 1, 1)));

        repair(1, &request(), &fixture.sender(), wants_ok)
            .await
            .unwrap();

        let second = &fixture.fake.requests()[1];
        assert!(matches!(second.messages()[2], Message::ToolResult { .. }));
    }
}
