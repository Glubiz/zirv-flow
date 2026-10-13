//! Bounded repair: when a model's answer fails validation, show it the errors and ask again, a
//! fixed number of times at most.

use std::fmt;

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

/// What a validator decided about one answer.
#[derive(Debug, Eq, PartialEq)]
pub enum Verdict<T, I, E> {
    /// The answer is good.
    Valid(T),
    /// The answer failed; these problems are shown to the model for the next attempt.
    Invalid(Vec<I>),
    /// The loop must end now with the caller's error (a refusal is not feedback).
    Abort(E),
}

/// How a repair loop ended.
#[derive(Debug, Eq, PartialEq)]
pub enum RepairOutcome<T, I> {
    /// The answer validated on this dispatch (the first is 1).
    Valid(T, u32),
    /// Every attempt failed validation; these are the last attempt's issues and the number of
    /// dispatches made.
    Exhausted(Vec<I>, u32),
}

/// Why a repair loop ended without an outcome.
#[derive(Debug, Eq, PartialEq)]
pub enum RepairError<E> {
    /// A dispatch failed.
    Dispatch(AiError),
    /// The validator aborted the loop.
    Aborted(E),
}

/// Dispatches `request`, validates the answer and, on failure, dispatches again with the issues,
/// at most `max_repairs` times after the first dispatch. A dispatch error or an abort ends the
/// loop at once. The validator may await.
pub async fn repair<T, I, E, S, V>(
    max_repairs: u32,
    request: &ChatRequest,
    sender: &S,
    mut validate: V,
) -> Result<RepairOutcome<T, I>, RepairError<E>>
where
    S: Sender + ?Sized,
    V: for<'r> FnMut(&'r ChatResponse) -> BoxFuture<'r, Verdict<T, I, E>>,
    I: fmt::Display,
{
    let mut current = request.clone();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let response = sender.send(&current).await.map_err(RepairError::Dispatch)?;
        match validate(&response).await {
            Verdict::Valid(value) => return Ok(RepairOutcome::Valid(value, attempts)),
            Verdict::Abort(error) => return Err(RepairError::Aborted(error)),
            Verdict::Invalid(issues) if attempts > max_repairs => {
                return Ok(RepairOutcome::Exhausted(issues, attempts));
            }
            Verdict::Invalid(issues) => {
                let lines: Vec<String> = issues.iter().map(ToString::to_string).collect();
                current = current.with_repair(&response, &lines);
            }
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

    fn wants_ok(response: &ChatResponse) -> Verdict<String, String, &'static str> {
        match response.text().as_str() {
            "ok" => Verdict::Valid("ok".to_owned()),
            other => Verdict::Invalid(vec![format!("expected ok, got {other}")]),
        }
    }

    #[tokio::test]
    async fn an_abort_ends_the_loop_with_the_callers_error() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(text_response("bad", 1, 1)))
            .push(Ok(text_response("never", 1, 1)));

        let error = repair(5, &request(), &fixture.sender(), |_| {
            Box::pin(async { Verdict::<(), String, &str>::Abort("refused") })
        })
        .await
        .unwrap_err();

        assert_eq!(error, RepairError::Aborted("refused"));
        assert_eq!(fixture.fake.call_count(), 1);
    }

    #[tokio::test]
    async fn exhaustion_returns_typed_issues_and_the_attempt_count() {
        #[derive(Debug, Eq, PartialEq)]
        struct Issue(u32);
        impl fmt::Display for Issue {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "issue {}", self.0)
            }
        }
        let fixture = Fixture::new();
        for word in ["a", "b", "c"] {
            fixture.fake.push(Ok(text_response(word, 1, 1)));
        }
        let mut seen = 0;

        let outcome = repair(1, &request(), &fixture.sender(), |_| {
            seen += 1;
            let number = seen;
            Box::pin(async move { Verdict::<(), Issue, ()>::Invalid(vec![Issue(number)]) })
        })
        .await
        .unwrap();

        assert_eq!(outcome, RepairOutcome::Exhausted(vec![Issue(2)], 2));
        let feedback = fixture.fake.requests()[1]
            .contents()
            .last()
            .unwrap()
            .render();
        assert!(feedback.contains("issue 1"), "{feedback}");
    }

    #[tokio::test]
    async fn the_validator_may_await() {
        let fixture = Fixture::new();
        fixture.fake.push(Ok(text_response("ok", 1, 1)));

        let outcome = repair(0, &request(), &fixture.sender(), |response| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Verdict::<String, String, ()>::Valid(response.text())
            })
        })
        .await
        .unwrap();

        assert_eq!(outcome, RepairOutcome::Valid("ok".to_owned(), 1));
    }

    #[tokio::test]
    async fn a_valid_first_answer_costs_one_dispatch() {
        let fixture = Fixture::new();
        fixture.fake.push(Ok(text_response("ok", 1, 1)));

        let outcome = repair(2, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
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

        let outcome = repair(2, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
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

        let outcome = repair(2, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
        .await
        .unwrap();

        assert_eq!(
            outcome,
            RepairOutcome::Exhausted(vec!["expected ok, got c".to_owned()], 3)
        );
        assert_eq!(fixture.fake.call_count(), 3);
    }

    #[tokio::test]
    async fn zero_repairs_means_a_single_dispatch() {
        let fixture = Fixture::new();
        fixture.fake.push(Ok(text_response("bad", 1, 1)));

        let outcome = repair(0, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
        .await
        .unwrap();

        assert!(matches!(outcome, RepairOutcome::Exhausted(..)));
        assert_eq!(fixture.fake.call_count(), 1);
    }

    #[tokio::test]
    async fn a_dispatch_error_ends_the_loop_at_once() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(text_response("bad", 1, 1)))
            .push(Err(AiError::BudgetExceeded));

        let error = repair(5, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
        .await
        .unwrap_err();

        assert_eq!(error, RepairError::Dispatch(AiError::BudgetExceeded));
        assert_eq!(fixture.fake.call_count(), 2);
    }

    #[tokio::test]
    async fn a_tool_call_answer_is_replayed_with_a_tool_result() {
        let fixture = Fixture::new();
        fixture
            .fake
            .push(Ok(tool_call_response("map", json!({}), 1, 1)))
            .push(Ok(text_response("ok", 1, 1)));

        repair(1, &request(), &fixture.sender(), |r| {
            Box::pin(async move { wants_ok(r) })
        })
        .await
        .unwrap();

        let second = &fixture.fake.requests()[1];
        assert!(matches!(second.messages()[2], Message::ToolResult { .. }));
    }
}
