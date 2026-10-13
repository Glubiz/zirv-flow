//! The AI assistant's provider layer (architecture sections 13.1 and 13.4): the deployment
//! allowlist, the gated dispatch that is the only way product code calls a model, and the
//! admin-only settings, models and usage endpoints.

pub(crate) mod config;
pub(crate) mod runtime;

pub use config::{AiConfig, AiConfigError, AiModelConfig, AiProviderConfig};
