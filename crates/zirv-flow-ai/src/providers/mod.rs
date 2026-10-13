//! The provider adapters: one module per wire protocol.

mod anthropic;
mod openai;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiCompatibleProvider;

use crate::error::AiError;
use crate::request::Content;

/// The text of a user message: its contents rendered and joined.
pub(crate) fn user_text(contents: &[Content]) -> String {
    contents
        .iter()
        .map(Content::render)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// A tool call's JSON arguments from the text a provider sent. Text that is not JSON means the
/// model cannot call tools reliably.
pub(crate) fn parse_arguments(text: &str) -> Result<serde_json::Value, AiError> {
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(text).map_err(|_| AiError::Capability("tool_calls".to_owned()))
}

/// Local mock provider servers for the adapter tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::routing::post;
    use serde_json::Value;
    use zirv_flow_core::egress::EgressPolicy;

    use crate::http::{AiHttp, AiHttpLimits};

    /// One request the mock received.
    #[derive(Clone, Debug)]
    pub(crate) struct Recorded {
        pub(crate) path: String,
        pub(crate) headers: HeaderMap,
        pub(crate) body: Value,
    }

    #[derive(Clone)]
    struct Mock {
        status: StatusCode,
        reply: String,
        seen: Arc<Mutex<Vec<Recorded>>>,
    }

    /// Serves `reply` with `status` for every POST; returns the base URL and what it received.
    pub(crate) async fn serve(status: u16, reply: &str) -> (String, Arc<Mutex<Vec<Recorded>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            status: StatusCode::from_u16(status).unwrap(),
            reply: reply.to_owned(),
            seen: Arc::clone(&seen),
        };
        let router = Router::new()
            .fallback(post(
                |State(mock): State<Mock>, uri: Uri, headers: HeaderMap, body: Bytes| async move {
                    mock.seen.lock().unwrap().push(Recorded {
                        path: uri.path().to_owned(),
                        headers,
                        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                    });
                    (mock.status, mock.reply)
                },
            ))
            .with_state(mock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}"), seen)
    }

    /// A client whose policy allows the loopback the mock listens on.
    pub(crate) fn loopback_http() -> AiHttp {
        let policy = EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap();
        AiHttp::new(Arc::new(policy), AiHttpLimits::default()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_joins_rendered_contents() {
        let text = user_text(&[Content::Schema("a".into()), Content::Untrusted("b".into())]);
        assert_eq!(text, "a\n\n<untrusted_data>\nb\n</untrusted_data>");
    }

    #[test]
    fn arguments_must_be_json_and_empty_means_no_arguments() {
        assert_eq!(
            parse_arguments("{\"a\":1}").unwrap(),
            serde_json::json!({"a": 1})
        );
        assert_eq!(parse_arguments("  ").unwrap(), serde_json::json!({}));
        assert_eq!(
            parse_arguments("{not json").unwrap_err(),
            AiError::Capability("tool_calls".to_owned())
        );
    }
}
