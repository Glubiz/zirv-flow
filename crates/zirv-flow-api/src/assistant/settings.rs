//! The admin-only AI endpoints of a workspace: `ai/settings`, `ai/models` and `ai/usage`. Every
//! response is content-free and never carries the provider key.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_ai::{Endpoint, MIN_CONTEXT_WINDOW_TOKENS, Missing, check_context_window};
use zirv_flow_core::{SecretBytes, WorkspaceId};
use zirv_flow_store::{AiSettings, AiUsage};

use super::config::{AiModelConfig, AiProviderConfig};
use super::dispatch::open_provider_key;
use crate::AppState;
use crate::admin::{self, AiSettingsChange};
use crate::auth::{Need, Principal, authorize, path_id};
use crate::json::ApiJson;
use crate::problem::{ApiError, ItemProblem, Problem};

/// The most recent calls the usage page lists.
const RECENT_CALLS: i64 = 50;
/// The longest provider key accepted, in bytes.
const MAX_KEY_BYTES: usize = 4096;

#[derive(Serialize, ToSchema)]
pub(crate) struct MissingBody {
    /// `tool_calls`, `usage` or `context_window`.
    capability: String,
    reason: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProbeBody {
    /// Whether the model passed the capability probe.
    ok: bool,
    missing: Vec<MissingBody>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    at: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AiSettingsBody {
    enabled: bool,
    /// The chosen provider's id from `ai/models`.
    provider_id: Option<String>,
    model: Option<String>,
    /// Micro-units of the deployment currency per calendar month (UTC); 0 allows no calls.
    monthly_budget_micros: i64,
    /// Whether requests may carry the values of people.
    pii_opt_in: bool,
    /// Days transcripts are kept (1 to 30); null stores none.
    transcript_days: Option<i32>,
    /// Whether a provider key is stored. The key itself is never returned.
    provider_key_set: bool,
    /// The last capability probe that passed, if any.
    last_probe: Option<ProbeBody>,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    updated_at: Option<OffsetDateTime>,
}

impl From<&AiSettings> for AiSettingsBody {
    fn from(settings: &AiSettings) -> Self {
        Self {
            enabled: settings.enabled,
            provider_id: settings.provider_id.clone(),
            model: settings.model.clone(),
            monthly_budget_micros: settings.monthly_budget_micros,
            pii_opt_in: settings.pii_opt_in,
            transcript_days: settings.transcript_days,
            provider_key_set: settings.provider_key_secret_id.is_some(),
            last_probe: settings.last_probe.as_ref().map(|probe| ProbeBody {
                ok: probe.result["ok"].as_bool().unwrap_or(false),
                missing: probe.result["missing"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|item| MissingBody {
                        capability: item["capability"].as_str().unwrap_or_default().to_owned(),
                        reason: item["reason"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect(),
                at: probe.at,
            }),
            updated_at: settings.updated_at,
        }
    }
}

/// The whole settings of the workspace; a PUT replaces them.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutAiSettingsRequest {
    /// Turning this on, or changing the provider, model or key while on, runs a capability probe
    /// that must pass.
    enabled: bool,
    /// A provider id from `ai/models`; set together with `model`.
    provider_id: Option<String>,
    model: Option<String>,
    /// At least 0.
    monthly_budget_micros: i64,
    pii_opt_in: bool,
    /// 1 to 30, or null to store no transcripts.
    transcript_days: Option<i32>,
    /// Creates or replaces the stored provider key. Omit it to keep the current key.
    #[schema(value_type = Option<String>, format = Password)]
    provider_key: Option<String>,
}

fn missing_problems(missing: &[Missing]) -> Vec<ItemProblem> {
    missing
        .iter()
        .map(|item| ItemProblem {
            index: 0,
            field: item.capability.as_str().to_owned(),
            reason: item.reason.clone(),
        })
        .collect()
}

fn probe_json(missing: &[Missing]) -> Value {
    json!({
        "ok": missing.is_empty(),
        "missing": missing.iter().map(|item| json!({
            "capability": item.capability.as_str(),
            "reason": item.reason,
        })).collect::<Vec<_>>(),
    })
}

/// Runs the capability probe against the chosen model with the key it would use, and refuses
/// with the missing capabilities. Nothing is stored for a failed probe.
async fn probe_model(
    state: &AppState,
    workspace: WorkspaceId,
    provider: &AiProviderConfig,
    model: &AiModelConfig,
    key: Option<&str>,
    stored_key: Option<zirv_flow_core::SecretId>,
) -> Result<Value, ApiError> {
    if let Some(missing) = check_context_window(model.context_window_tokens) {
        return Err(ApiError::ai_capability_missing(missing_problems(&[
            missing,
        ])));
    }
    let key = match (key, stored_key) {
        (Some(key), _) => zirv_flow_core::SecretString::new(key.to_owned()),
        (None, Some(id)) => open_provider_key(state, workspace, id).await?,
        (None, None) => return Err(ApiError::unprocessable("provider_key: required")),
    };
    let http = state
        .ai
        .http(state.egress_policy())
        .map_err(|error| ApiError::ai_provider_failed(error.to_string()))?;
    let endpoint = Endpoint {
        base_url: provider.base_url.clone(),
        model: model.name.clone(),
    };
    let adapter = state.ai.provider(provider.kind);
    let missing = zirv_flow_ai::probe(adapter.as_ref(), http, &endpoint, &key)
        .await
        .map_err(|error| ApiError::ai_provider_failed(error.to_string()))?;
    if missing.is_empty() {
        Ok(probe_json(&missing))
    } else {
        Err(ApiError::ai_capability_missing(missing_problems(&missing)))
    }
}

/// The workspace's AI settings. Admins, the owner and `admin` keys only.
#[utoipa::path(
    get,
    operation_id = "get_ai_settings",
    path = "/api/v1/workspaces/{ws}/ai/settings",
    tag = "ai",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The settings; the provider key is never returned", body = AiSettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn get_settings(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<AiSettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let settings = zirv_flow_store::find_ai_settings(&state.pool, workspace.id)
        .await?
        .unwrap_or_else(|| AiSettings::defaults(workspace.id));
    Ok(Json(AiSettingsBody::from(&settings)))
}

/// Replaces the workspace's AI settings. Enabling the assistant, or changing its provider, model
/// or key while it is on, first runs a capability probe: the model must call a tool with valid
/// arguments and report token usage, and its configured context window must be at least 32768
/// tokens. A model that fails is refused with 422 and the missing capabilities. Audited as
/// `ai.settings.updated`, with the changed field names and values, never the key.
#[utoipa::path(
    put,
    operation_id = "put_ai_settings",
    path = "/api/v1/workspaces/{ws}/ai/settings",
    tag = "ai",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    request_body = PutAiSettingsRequest,
    responses(
        (status = 200, description = "The saved settings", body = AiSettingsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Invalid settings, a model that is not offered, or a model that lacks capabilities (code ai_capability_missing, with errors listing each)", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The provider could not be reached for the probe (code ai_provider_error)", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Stored secrets are not configured on this deployment", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn put_settings(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    ApiJson(body): ApiJson<PutAiSettingsRequest>,
) -> Result<Json<AiSettingsBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let current = zirv_flow_store::find_ai_settings(&state.pool, workspace.id)
        .await?
        .unwrap_or_else(|| AiSettings::defaults(workspace.id));

    if body.monthly_budget_micros < 0 {
        return Err(ApiError::unprocessable(
            "monthly_budget_micros: must not be negative",
        ));
    }
    if body
        .transcript_days
        .is_some_and(|days| !(1..=30).contains(&days))
    {
        return Err(ApiError::unprocessable(
            "transcript_days: must be between 1 and 30",
        ));
    }
    if let Some(key) = &body.provider_key
        && (key.trim().is_empty() || key.len() > MAX_KEY_BYTES)
    {
        return Err(ApiError::unprocessable(
            "provider_key: must be 1 to 4096 characters",
        ));
    }
    if body.provider_key.is_some() && state.keyring().is_none() {
        return Err(ApiError::secrets_unavailable());
    }
    let chosen =
        match (&body.provider_id, &body.model) {
            (None, None) => None,
            (Some(provider_id), Some(model)) => {
                Some(state.ai.config.model(provider_id, model).ok_or_else(|| {
                    ApiError::unprocessable("model: not offered by this deployment")
                })?)
            }
            _ => {
                return Err(ApiError::unprocessable(
                    "provider_id and model: set both or neither",
                ));
            }
        };
    let has_key = body.provider_key.is_some() || current.provider_key_secret_id.is_some();
    if body.enabled {
        if chosen.is_none() {
            return Err(ApiError::unprocessable(
                "model: choose a provider and model before enabling the assistant",
            ));
        }
        if !has_key {
            return Err(ApiError::unprocessable(
                "provider_key: a provider key is needed to enable the assistant",
            ));
        }
    }

    let changed_model = current.provider_id != body.provider_id || current.model != body.model;
    let probe = match chosen {
        Some((provider, model))
            if body.enabled
                && (!current.enabled || changed_model || body.provider_key.is_some()) =>
        {
            Some(
                probe_model(
                    &state,
                    workspace.id,
                    provider,
                    model,
                    body.provider_key.as_deref(),
                    current.provider_key_secret_id,
                )
                .await?,
            )
        }
        _ => None,
    };

    let change = AiSettingsChange {
        enabled: body.enabled,
        provider_id: body.provider_id,
        model: body.model,
        monthly_budget_micros: body.monthly_budget_micros,
        pii_opt_in: body.pii_opt_in,
        transcript_days: body.transcript_days,
        provider_key: body
            .provider_key
            .map(|key| SecretBytes::new(key.into_bytes())),
        probe,
    };
    let saved = admin::save_ai_settings(
        &state.pool,
        &principal.context(),
        state.keyring().map(|keyring| keyring.as_ref()),
        workspace.id,
        &change,
    )
    .await?;
    Ok(Json(AiSettingsBody::from(&saved)))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ModelBody {
    name: String,
    context_window_tokens: u32,
    /// Micro-units of the deployment currency per million input tokens.
    input_price_per_mtok: u64,
    /// Micro-units of the deployment currency per million output tokens.
    output_price_per_mtok: u64,
    /// Whether the context window meets the assistant's minimum of 32768 tokens.
    meets_minimum_context: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderBody {
    id: String,
    /// `openai_compatible` or `anthropic`.
    kind: String,
    base_url: String,
    models: Vec<ModelBody>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AiModelsBody {
    /// The deployment's allowlist; empty means the assistant is unavailable.
    providers: Vec<ProviderBody>,
    /// The smallest context window a model may have to be enabled.
    min_context_window_tokens: u32,
}

/// The providers and models this deployment offers, with context windows and prices.
#[utoipa::path(
    get,
    operation_id = "list_ai_models",
    path = "/api/v1/workspaces/{ws}/ai/models",
    tag = "ai",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "The deployment's allowlist", body = AiModelsBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn models(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<AiModelsBody>, ApiError> {
    authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    Ok(Json(models_body(&state)))
}

fn models_body(state: &AppState) -> AiModelsBody {
    AiModelsBody {
        providers: state
            .ai
            .config
            .providers
            .iter()
            .map(|provider| ProviderBody {
                id: provider.id.clone(),
                kind: provider.kind.as_str().to_owned(),
                base_url: provider.base_url.clone(),
                models: provider
                    .models
                    .iter()
                    .map(|model| ModelBody {
                        name: model.name.clone(),
                        context_window_tokens: model.context_window_tokens,
                        input_price_per_mtok: model.input_price_per_mtok,
                        output_price_per_mtok: model.output_price_per_mtok,
                        meets_minimum_context: check_context_window(model.context_window_tokens)
                            .is_none(),
                    })
                    .collect(),
            })
            .collect(),
        min_context_window_tokens: MIN_CONTEXT_WINDOW_TOKENS,
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct FeatureUsageBody {
    feature: String,
    calls: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_micros: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct UserUsageBody {
    /// Null for calls made with an API key.
    user_id: Option<Uuid>,
    calls: i64,
    cost_micros: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AiCallBody {
    id: Uuid,
    user_id: Option<Uuid>,
    feature: String,
    provider_id: String,
    model: String,
    input_tokens: i32,
    output_tokens: i32,
    estimated_cost_micros: i64,
    latency_ms: i32,
    /// `pending` (reserved, not finished), `ok`, `http_error`, `timeout`, `egress_denied`, `transport`, `malformed` or `capability`.
    outcome: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AiUsageBody {
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    month_start: OffsetDateTime,
    /// Spent so far this month (UTC), in micro-units.
    spent_micros: i64,
    /// The monthly budget in micro-units; 0 allows no calls.
    budget_micros: i64,
    by_feature: Vec<FeatureUsageBody>,
    by_user: Vec<UserUsageBody>,
    /// The latest calls, newest first. Counts and names only: never a prompt or a response.
    recent: Vec<AiCallBody>,
}

impl AiUsageBody {
    fn new(usage: AiUsage, budget_micros: i64, now: OffsetDateTime) -> Self {
        Self {
            month_start: zirv_flow_store::month_start(now),
            spent_micros: usage.spent_micros,
            budget_micros,
            by_feature: usage
                .by_feature
                .into_iter()
                .map(|row| FeatureUsageBody {
                    feature: row.feature,
                    calls: row.calls,
                    input_tokens: row.input_tokens,
                    output_tokens: row.output_tokens,
                    cost_micros: row.cost_micros,
                })
                .collect(),
            by_user: usage
                .by_user
                .into_iter()
                .map(|row| UserUsageBody {
                    user_id: row.user_id.map(|id| id.as_uuid()),
                    calls: row.calls,
                    cost_micros: row.cost_micros,
                })
                .collect(),
            recent: usage
                .recent
                .into_iter()
                .map(|call| AiCallBody {
                    id: call.id,
                    user_id: call.user_id.map(|id| id.as_uuid()),
                    feature: call.feature,
                    provider_id: call.provider_id,
                    model: call.model,
                    input_tokens: call.input_tokens,
                    output_tokens: call.output_tokens,
                    estimated_cost_micros: call.estimated_cost_micros,
                    latency_ms: call.latency_ms,
                    outcome: call.outcome,
                    created_at: call.created_at,
                })
                .collect(),
        }
    }
}

/// The month's AI spend against the budget, by feature and by user, with the latest calls.
#[utoipa::path(
    get,
    operation_id = "get_ai_usage",
    path = "/api/v1/workspaces/{ws}/ai/usage",
    tag = "ai",
    params(("ws" = Uuid, Path, description = "Workspace id")),
    responses(
        (status = 200, description = "Month-to-date usage; content-free", body = AiUsageBody),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Not a workspace admin", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn usage(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
) -> Result<Json<AiUsageBody>, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Admin).await?;
    let budget = zirv_flow_store::find_ai_settings(&state.pool, workspace.id)
        .await?
        .map_or(0, |settings| settings.monthly_budget_micros);
    let now = OffsetDateTime::now_utc();
    let mut connection = state
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let usage = zirv_flow_store::ai_usage(&mut connection, workspace.id, now, RECENT_CALLS).await?;
    Ok(Json(AiUsageBody::new(usage, budget, now)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use zirv_flow_ai::{Capability, Missing};
    use zirv_flow_core::WorkspaceId;
    use zirv_flow_store::ProbeRecord;

    use super::*;

    fn settings(probe: Option<Value>) -> AiSettings {
        AiSettings {
            enabled: true,
            provider_id: Some("openai".into()),
            model: Some("gpt-x".into()),
            monthly_budget_micros: 5,
            pii_opt_in: true,
            transcript_days: Some(7),
            provider_key_secret_id: Some(zirv_flow_core::SecretId::new()),
            last_probe: probe.map(|result| ProbeRecord {
                result,
                at: datetime!(2026-05-01 00:00 UTC),
            }),
            updated_at: Some(datetime!(2026-05-01 00:00 UTC)),
            ..AiSettings::defaults(WorkspaceId::new())
        }
    }

    #[test]
    fn the_settings_body_says_whether_a_key_is_set_and_never_carries_it() {
        let body = serde_json::to_value(AiSettingsBody::from(&settings(None))).unwrap();

        assert_eq!(body["provider_key_set"], true);
        assert!(body["last_probe"].is_null());
        let text = body.to_string();
        for field in ["secret", "ciphertext", "provider_key\""] {
            assert!(!text.contains(field), "{field}: {text}");
        }
        let unset = AiSettings::defaults(WorkspaceId::new());
        let body = serde_json::to_value(AiSettingsBody::from(&unset)).unwrap();
        assert_eq!(body["provider_key_set"], false);
        assert_eq!(body["enabled"], false);
    }

    #[test]
    fn the_last_probe_is_shown_with_its_missing_capabilities() {
        let missing = [Missing {
            capability: Capability::Usage,
            reason: "no usage".into(),
        }];
        let body =
            serde_json::to_value(AiSettingsBody::from(&settings(Some(probe_json(&missing)))))
                .unwrap();

        assert_eq!(body["last_probe"]["ok"], false);
        assert_eq!(body["last_probe"]["missing"][0]["capability"], "usage");
        assert_eq!(body["last_probe"]["missing"][0]["reason"], "no usage");
        assert_eq!(probe_json(&[])["ok"], true);
    }

    #[test]
    fn missing_capabilities_become_per_field_problems() {
        let problems = missing_problems(&[Missing {
            capability: Capability::ContextWindow,
            reason: "too small".into(),
        }]);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].field, "context_window");
        assert_eq!(problems[0].reason, "too small");
    }

    #[test]
    fn the_request_refuses_unknown_fields_and_has_no_debug_that_could_print_the_key() {
        let request = json!({
            "enabled": false, "provider_id": null, "model": null,
            "monthly_budget_micros": 0, "pii_opt_in": false, "transcript_days": null,
            "unexpected": 1
        });
        assert!(serde_json::from_value::<PutAiSettingsRequest>(request).is_err());
        let ok = json!({
            "enabled": false, "provider_id": null, "model": null,
            "monthly_budget_micros": 0, "pii_opt_in": false, "transcript_days": null
        });
        let parsed: PutAiSettingsRequest = serde_json::from_value(ok).unwrap();
        assert!(parsed.provider_key.is_none());
    }
}
