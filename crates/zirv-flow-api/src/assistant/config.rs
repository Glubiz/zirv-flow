//! The deployment's `ai.*` configuration: the allowlist of providers, models and base URLs that a
//! workspace may choose from, with context windows and prices. Empty by default, which means the
//! assistant is unavailable.

use hashbrown::HashSet;

use serde::{Deserialize, Serialize};
use url::Url;
use zirv_flow_ai::ProviderKind;

const DEFAULT_RATE_LIMIT_PER_USER_PER_MINUTE: u32 = 20;
const DEFAULT_MAX_REPAIR_ATTEMPTS: u32 = 2;
/// The most repair attempts a deployment may ask for; each is a paid model call.
const MAX_REPAIR_ATTEMPTS: u32 = 5;

/// One model a workspace may use, with what the budget needs to know about it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiModelConfig {
    pub name: String,
    /// The context window the operator vouches for; below 32768 the model cannot be enabled.
    pub context_window_tokens: u32,
    /// Micro-units of the deployment currency per million input tokens.
    pub input_price_per_mtok: u64,
    /// Micro-units of the deployment currency per million output tokens.
    pub output_price_per_mtok: u64,
}

/// One provider endpoint and the models it serves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiProviderConfig {
    pub id: String,
    pub kind: ProviderKind,
    /// An http(s) URL such as `https://api.openai.com/v1`. A local model server on a private
    /// address also needs an `egress.allow` entry.
    pub base_url: String,
    pub models: Vec<AiModelConfig>,
}

/// The `ai` section of the configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiConfig {
    #[serde(default)]
    pub providers: Vec<AiProviderConfig>,
    /// Calls one user may make per minute, counted from the call log across all instances.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_user_per_minute: u32,
    /// Repairs after a first answer fails validation; each is a separate, gated call.
    #[serde(default = "default_repair_attempts")]
    pub max_repair_attempts: u32,
}

fn default_rate_limit() -> u32 {
    DEFAULT_RATE_LIMIT_PER_USER_PER_MINUTE
}

fn default_repair_attempts() -> u32 {
    DEFAULT_MAX_REPAIR_ATTEMPTS
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            rate_limit_per_user_per_minute: DEFAULT_RATE_LIMIT_PER_USER_PER_MINUTE,
            max_repair_attempts: DEFAULT_MAX_REPAIR_ATTEMPTS,
        }
    }
}

/// A setting that cannot be used: its key (with an index for list entries) and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiConfigError {
    pub key: String,
    pub reason: String,
}

impl AiConfigError {
    fn new(key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            reason: reason.into(),
        }
    }
}

impl AiConfig {
    /// Checks the allowlist: unique ids and model names, usable base URLs, positive limits.
    pub fn validate(&self) -> Result<(), AiConfigError> {
        if self.rate_limit_per_user_per_minute == 0 {
            return Err(AiConfigError::new(
                "ai.rate_limit_per_user_per_minute",
                "must be at least 1",
            ));
        }
        if self.max_repair_attempts > MAX_REPAIR_ATTEMPTS {
            return Err(AiConfigError::new(
                "ai.max_repair_attempts",
                format!("must be at most {MAX_REPAIR_ATTEMPTS}"),
            ));
        }
        let mut ids = HashSet::new();
        for (index, provider) in self.providers.iter().enumerate() {
            let key = |field: &str| format!("ai.providers[{index}].{field}");
            let id_ok = !provider.id.is_empty()
                && provider.id.len() <= 64
                && provider
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !id_ok {
                return Err(AiConfigError::new(
                    key("id"),
                    "must be 1 to 64 letters, digits, '-' or '_'",
                ));
            }
            if !ids.insert(provider.id.as_str()) {
                return Err(AiConfigError::new(
                    key("id"),
                    format!("{:?} is used by another provider", provider.id),
                ));
            }
            if !usable_base_url(&provider.base_url) {
                return Err(AiConfigError::new(
                    key("base_url"),
                    "must be an http or https URL without credentials, a query or a fragment",
                ));
            }
            if provider.models.is_empty() {
                return Err(AiConfigError::new(key("models"), "must list a model"));
            }
            let mut names = HashSet::new();
            for (position, model) in provider.models.iter().enumerate() {
                let model_key =
                    |field: &str| format!("ai.providers[{index}].models[{position}].{field}");
                if model.name.trim().is_empty() {
                    return Err(AiConfigError::new(model_key("name"), "must not be empty"));
                }
                if !names.insert(model.name.as_str()) {
                    return Err(AiConfigError::new(
                        model_key("name"),
                        format!("{:?} is listed twice for this provider", model.name),
                    ));
                }
                if model.context_window_tokens == 0 {
                    return Err(AiConfigError::new(
                        model_key("context_window_tokens"),
                        "must be at least 1",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&AiProviderConfig> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    /// A provider and one of its models, when both are allowlisted.
    pub fn model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Option<(&AiProviderConfig, &AiModelConfig)> {
        let provider = self.provider(provider_id)?;
        let model = provider.models.iter().find(|entry| entry.name == model)?;
        Some((provider, model))
    }
}

fn usable_base_url(text: &str) -> bool {
    Url::parse(text).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

impl AiModelConfig {
    /// The cost of a call in micro-units, rounded up so a call is never free.
    pub fn cost_micros(&self, input_tokens: u32, output_tokens: u32) -> i64 {
        let total = u128::from(input_tokens) * u128::from(self.input_price_per_mtok)
            + u128::from(output_tokens) * u128::from(self.output_price_per_mtok);
        let micros = total.div_ceil(1_000_000);
        i64::try_from(micros).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str) -> AiModelConfig {
        AiModelConfig {
            name: name.to_owned(),
            context_window_tokens: 128_000,
            input_price_per_mtok: 3_000_000,
            output_price_per_mtok: 15_000_000,
        }
    }

    fn provider(id: &str) -> AiProviderConfig {
        AiProviderConfig {
            id: id.to_owned(),
            kind: ProviderKind::OpenAiCompatible,
            base_url: "https://api.example.com/v1".to_owned(),
            models: vec![model("m1"), model("m2")],
        }
    }

    fn config(providers: Vec<AiProviderConfig>) -> AiConfig {
        AiConfig {
            providers,
            ..AiConfig::default()
        }
    }

    fn error(config: &AiConfig) -> AiConfigError {
        config.validate().unwrap_err()
    }

    #[test]
    fn the_defaults_offer_no_providers_and_twenty_calls_and_two_repairs() {
        let config = AiConfig::default();
        assert!(config.providers.is_empty());
        assert_eq!(config.rate_limit_per_user_per_minute, 20);
        assert_eq!(config.max_repair_attempts, 2);
        assert!(config.validate().is_ok());
        let empty: AiConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, config);
    }

    #[test]
    fn a_valid_allowlist_passes_and_is_looked_up_by_provider_and_model() {
        let config = config(vec![provider("openai"), provider("local")]);
        assert!(config.validate().is_ok());
        let (found, model) = config.model("local", "m2").unwrap();
        assert_eq!(found.id, "local");
        assert_eq!(model.name, "m2");
        assert!(config.model("local", "missing").is_none());
        assert!(config.model("missing", "m1").is_none());
    }

    #[test]
    fn provider_ids_and_model_names_must_be_unique() {
        let duplicate = error(&config(vec![provider("a"), provider("a")]));
        assert_eq!(duplicate.key, "ai.providers[1].id");

        let mut twice = provider("a");
        twice.models.push(model("m1"));
        let duplicate = error(&config(vec![twice]));
        assert_eq!(duplicate.key, "ai.providers[0].models[2].name");
        assert!(duplicate.reason.contains("listed twice"));
    }

    #[test]
    fn the_same_model_name_may_be_listed_under_two_providers() {
        assert!(
            config(vec![provider("a"), provider("b")])
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn unusable_ids_urls_and_models_name_the_key() {
        let mut bad_id = provider("has space");
        assert_eq!(
            error(&config(vec![bad_id.clone()])).key,
            "ai.providers[0].id"
        );
        bad_id.id = String::new();
        assert_eq!(error(&config(vec![bad_id])).key, "ai.providers[0].id");

        for url in [
            "ftp://x.test",
            "nonsense",
            "https://user:pw@x.test",
            "https://x.test/?k=1",
            "https://x.test/#f",
        ] {
            let mut bad = provider("a");
            bad.base_url = url.to_owned();
            assert_eq!(
                error(&config(vec![bad])).key,
                "ai.providers[0].base_url",
                "{url}"
            );
        }

        let mut none = provider("a");
        none.models.clear();
        assert_eq!(error(&config(vec![none])).key, "ai.providers[0].models");

        let mut zero = provider("a");
        zero.models[1].context_window_tokens = 0;
        assert_eq!(
            error(&config(vec![zero])).key,
            "ai.providers[0].models[1].context_window_tokens"
        );

        let mut unnamed = provider("a");
        unnamed.models[0].name = " ".to_owned();
        assert_eq!(
            error(&config(vec![unnamed])).key,
            "ai.providers[0].models[0].name"
        );
    }

    #[test]
    fn a_small_context_window_is_allowed_in_the_config_and_refused_at_enablement() {
        let mut small = provider("a");
        small.models[0].context_window_tokens = 8_192;
        assert!(config(vec![small]).validate().is_ok());
    }

    #[test]
    fn the_limits_are_bounded() {
        let zero = AiConfig {
            rate_limit_per_user_per_minute: 0,
            ..AiConfig::default()
        };
        assert_eq!(error(&zero).key, "ai.rate_limit_per_user_per_minute");

        let many = |attempts| AiConfig {
            max_repair_attempts: attempts,
            ..AiConfig::default()
        };
        assert_eq!(error(&many(6)).key, "ai.max_repair_attempts");
        assert!(many(0).validate().is_ok());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let text = r#"{"providers":[{"id":"a","kind":"anthropic","base_url":"https://x.test",
            "models":[],"api_key":"sk-oops"}]}"#;
        assert!(serde_json::from_str::<AiConfig>(text).is_err());
        let kind = r#"{"providers":[{"id":"a","kind":"gemini","base_url":"https://x.test",
            "models":[]}]}"#;
        assert!(serde_json::from_str::<AiConfig>(kind).is_err());
    }

    #[test]
    fn the_cost_of_a_call_is_prorated_from_the_prices_and_rounded_up() {
        let model = model("m");
        // 1000 input tokens at 3.00 per million and 500 output tokens at 15.00 per million.
        assert_eq!(model.cost_micros(1_000, 500), 3_000 + 7_500);
        assert_eq!(model.cost_micros(0, 0), 0);
        // A single token still costs at least one micro-unit.
        assert_eq!(model.cost_micros(1, 0), 3);
        let cheap = AiModelConfig {
            input_price_per_mtok: 1,
            ..model
        };
        assert_eq!(cheap.cost_micros(1, 0), 1);
        let huge = AiModelConfig {
            input_price_per_mtok: u64::MAX,
            output_price_per_mtok: u64::MAX,
            ..cheap
        };
        assert!(huge.cost_micros(u32::MAX, u32::MAX) > 0);
    }
}
