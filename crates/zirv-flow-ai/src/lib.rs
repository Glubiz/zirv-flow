//! The model-provider layer for zirv-flow.
//!
//! Every provider is reduced to text parts, structured tool calls and usage ([`ChatResponse`]), so
//! product code never branches on the provider. Requests are built through [`RequestBuilder`],
//! which refuses personal data unless the workspace opted in. Every outbound call goes through
//! [`AiHttp`], which enforces the egress policy of `zirv_flow_core::egress`; this crate exposes
//! no plain `reqwest::Client`. The gates (enablement, rate limit, budget, call log) live with the
//! caller, in `zirv-flow-api`.

mod error;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
mod http;
mod probe;
mod provider;
mod providers;
mod repair;
mod request;
mod response;
mod schema;
mod tool;

pub use error::AiError;
pub use http::{AiHttp, AiHttpLimits, DEFAULT_MAX_BODY_BYTES, Lookup};
pub use probe::{Capability, MIN_CONTEXT_WINDOW_TOKENS, Missing, check_context_window, probe};
pub use provider::{BoxFuture, Endpoint, Provider, ProviderKind, provider_for};
pub use providers::{AnthropicProvider, OpenAiCompatibleProvider};
pub use repair::{ProviderSender, RepairError, RepairOutcome, Sender, Verdict, repair};
pub use request::{
    ChatRequest, Content, DEFAULT_MAX_OUTPUT_TOKENS, FRAMING_BASE_TOKENS, FRAMING_TOKENS_PER_UNIT,
    Message, Privacy, RequestBuilder, ToolSpec, UNTRUSTED_NOTICE,
};
pub use response::{ChatResponse, Part, Usage};
pub use schema::JsonSchema;
pub use tool::{AiTool, ToolEffect, ToolError, ToolOutput};
