//! The provider seam: one trait that reduces every provider to the same request and response.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zirv_flow_core::SecretString;

use crate::error::AiError;
use crate::http::AiHttp;
use crate::providers::{AnthropicProvider, OpenAiCompatibleProvider};
use crate::request::ChatRequest;
use crate::response::ChatResponse;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The wire protocols the product speaks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The OpenAI chat-completions protocol, which local model servers also speak.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
    Anthropic,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::Anthropic => "anthropic",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text {
            "openai_compatible" => Ok(Self::OpenAiCompatible),
            "anthropic" => Ok(Self::Anthropic),
            _ => Err(()),
        }
    }
}

/// Where to send a call: the provider's base URL and the model to ask for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
}

/// A model provider. Adapters translate to and from the wire format and nothing else; the gates
/// (budget, rate limit, privacy, the call log) live with the caller.
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;

    fn complete<'a>(
        &'a self,
        http: &'a AiHttp,
        endpoint: &'a Endpoint,
        key: &'a SecretString,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>>;
}

/// The adapter for a protocol.
pub fn provider_for(kind: ProviderKind) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::OpenAiCompatible => Arc::new(OpenAiCompatibleProvider),
        ProviderKind::Anthropic => Arc::new(AnthropicProvider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_text_and_json() {
        for kind in [ProviderKind::OpenAiCompatible, ProviderKind::Anthropic] {
            assert_eq!(kind.as_str().parse::<ProviderKind>(), Ok(kind));
            assert_eq!(kind.to_string(), kind.as_str());
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(serde_json::from_str::<ProviderKind>(&json).unwrap(), kind);
        }
        assert!("gemini".parse::<ProviderKind>().is_err());
    }

    #[test]
    fn each_kind_gets_its_own_adapter() {
        for kind in [ProviderKind::OpenAiCompatible, ProviderKind::Anthropic] {
            assert_eq!(provider_for(kind).kind(), kind);
        }
    }
}
