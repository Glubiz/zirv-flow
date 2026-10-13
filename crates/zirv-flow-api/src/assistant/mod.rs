//! The AI assistant's provider layer (architecture sections 13.1 and 13.4): the deployment
//! allowlist, the gated dispatch that is the only way product code calls a model, and the
//! admin-only settings, models and usage endpoints.

pub(crate) mod config;
pub(crate) mod diff;
pub(crate) mod dispatch;
pub(crate) mod drafts;
pub(crate) mod evaluation;
pub(crate) mod prompt;
pub(crate) mod runtime;
pub(crate) mod schema;
pub(crate) mod settings;
pub(crate) mod tools;

#[cfg(test)]
mod acceptance;

pub use config::{AiConfig, AiConfigError, AiModelConfig, AiProviderConfig};
pub use dispatch::{DispatchError, Dispatcher, dispatch};
pub use evaluation::{CaseReport, EvaluationError, evaluate_assistant};
