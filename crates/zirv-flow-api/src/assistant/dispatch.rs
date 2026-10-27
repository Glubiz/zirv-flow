//! The gated dispatch: the only way product code calls a model.
//!
//! A call is refused before anything is sent when the assistant is off, the user is over the
//! per-minute limit or the workspace's monthly budget is spent. Both counts are read from the
//! call log in the database, so they hold across instances, and a call is reserved in that log as a
//! `pending` row at its worst-case cost before the request is sent, so concurrent calls cannot all
//! pass the gates. The reservation, the call and the bookkeeping run in one task of their own,
//! started on the first poll, so a dropped handler cannot leave the row pending or reserved
//! without a flight. A panic in the provider call is caught in that task and, like a process that
//! dies mid-call, leaves the row at the worst-case cost: a caught panic marks it `abandoned` at
//! once, and a reservation marks this workspace's `pending` rows
//! older than the client timeout plus a margin `abandoned`, keeping their cost, which is
//! conservative and intended. A dispatched call always leaves a content-free row in `ai_calls`,
//! finished with the real tokens and cost; a transcript is stored only when an admin opted in. The
//! key is opened right before the request and dropped after it.

use std::panic::AssertUnwindSafe;
use std::time::Instant;

use futures_util::FutureExt;
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_ai::{
    AiError, BoxFuture, ChatRequest, ChatResponse, Endpoint, Message, Part, Privacy, Sender,
};
use zirv_flow_core::{
    SecretId, SecretPurpose, SecretString, UserId, WorkspaceId, open, secret_aad,
};
use zirv_flow_store::{
    AiSettings, CallOutcome, FinishedAiCall, NewAiCall, Reservation, ReservationLimits, StoreError,
    secrets,
};

use super::config::{AiModelConfig, AiProviderConfig};
use crate::AppState;
use crate::problem::ApiError;

/// Why a dispatch did not return a response.
#[derive(Debug)]
pub enum DispatchError {
    /// A gate refused it, or the provider failed.
    Ai(AiError),
    /// The call log or the settings could not be read or written.
    Store(StoreError),
    /// The provider key cannot be opened: the deployment has no master key.
    SecretsUnavailable,
    /// The stored provider key could not be opened.
    KeyUnusable,
}

impl From<AiError> for DispatchError {
    fn from(error: AiError) -> Self {
        Self::Ai(error)
    }
}

impl From<StoreError> for DispatchError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<DispatchError> for ApiError {
    fn from(error: DispatchError) -> Self {
        match error {
            DispatchError::Ai(error) => match error {
                AiError::Disabled => Self::ai_disabled(),
                AiError::BudgetExceeded => Self::ai_budget_exceeded(),
                AiError::RateLimited => Self::ai_rate_limited(),
                AiError::PersonalDataNotAllowed => Self::ai_personal_data_not_allowed(),
                AiError::Schema(_) => Self::internal(),
                other => Self::ai_provider_failed(other.to_string()),
            },
            DispatchError::Store(error) => error.into(),
            DispatchError::SecretsUnavailable => Self::secrets_unavailable(),
            DispatchError::KeyUnusable => Self::internal(),
        }
    }
}

/// Counts a gate refusal.
fn refused(reason: &'static str) {
    metrics::counter!("zirv_flow_ai_refusals_total", "reason" => reason).increment(1);
}

/// What the call log records for a failed call.
fn outcome_of(error: &AiError) -> CallOutcome {
    match error {
        AiError::Http(_) => CallOutcome::HttpError,
        AiError::Timeout => CallOutcome::Timeout,
        AiError::Egress { .. } => CallOutcome::EgressDenied,
        AiError::Capability(_) => CallOutcome::Capability,
        AiError::Malformed => CallOutcome::Malformed,
        _ => CallOutcome::Transport,
    }
}

/// Opens a workspace's provider key. Call it right before the request that needs it.
pub(crate) async fn open_provider_key(
    state: &AppState,
    workspace: WorkspaceId,
    secret: SecretId,
) -> Result<SecretString, DispatchError> {
    let keyring = state.keyring().ok_or(DispatchError::SecretsUnavailable)?;
    let stored = secrets::get(&state.pool, workspace, secret)
        .await?
        .ok_or(DispatchError::KeyUnusable)?;
    let bytes = open(
        keyring,
        &stored.sealed,
        &secret_aad(workspace, secret, stored.purpose),
    )
    .map_err(|_| DispatchError::KeyUnusable)?;
    if stored.purpose != SecretPurpose::AiProviderKey {
        return Err(DispatchError::KeyUnusable);
    }
    SecretString::try_from(bytes).map_err(|_| DispatchError::KeyUnusable)
}

/// One workspace's gated access to its configured model for one feature.
pub struct Dispatcher<'a> {
    state: &'a AppState,
    user: Option<UserId>,
    workspace: WorkspaceId,
    feature: &'static str,
    settings: AiSettings,
    provider: &'a AiProviderConfig,
    model: &'a AiModelConfig,
    key: SecretId,
    draft_id: Option<Uuid>,
}

impl<'a> Dispatcher<'a> {
    /// Step 1: loads the settings and refuses with `Disabled` when the workspace is not enabled,
    /// has no key, or its model is no longer on the deployment's allowlist.
    pub async fn open(
        state: &'a AppState,
        user: Option<UserId>,
        workspace: WorkspaceId,
        feature: &'static str,
    ) -> Result<Dispatcher<'a>, DispatchError> {
        let settings = zirv_flow_store::find_ai_settings(&state.pool, workspace)
            .await?
            .filter(|settings| settings.enabled);
        let chosen = settings.as_ref().and_then(|settings| {
            let (provider, model) = state
                .ai
                .config
                .model(settings.provider_id.as_deref()?, settings.model.as_deref()?)?;
            Some((provider, model, settings.provider_key_secret_id?))
        });
        let (Some(settings), Some((provider, model, key))) = (settings, chosen) else {
            refused("disabled");
            return Err(AiError::Disabled.into());
        };
        Ok(Self {
            state,
            user,
            workspace,
            feature,
            settings,
            provider,
            model,
            key,
            draft_id: None,
        })
    }

    /// What a call of `input_tokens` and `output_tokens` costs on the chosen model, in micro-units.
    pub(crate) fn cost_micros(&self, input_tokens: u32, output_tokens: u32) -> i64 {
        self.model.cost_micros(input_tokens, output_tokens)
    }

    /// The privacy level the workspace's opt-in allows.
    pub fn privacy(&self) -> Privacy {
        Privacy::from_opt_in(self.settings.pii_opt_in)
    }

    /// Links the calls of this dispatcher to a draft (`ai_calls.draft_id`). The column has no
    /// foreign key, so a run that fails part-way leaves calls whose draft has no row.
    #[must_use]
    pub fn for_draft(mut self, id: Uuid) -> Self {
        self.draft_id = Some(id);
        self
    }

    /// Steps 2 to 8, all in a task of their own: reserves the call, opens the key, calls the
    /// provider, finishes the call's row and, when the workspace opted in, stores a transcript.
    /// The reservation and the flight are one task, started on this future's first poll, so a
    /// handler that is dropped (a client that went away) at any later point cannot cancel the
    /// call, leave a reserved row without a flight, or leave the row pending. A refused call sends
    /// nothing.
    pub async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, DispatchError> {
        let flight = Flight {
            state: self.state.clone(),
            workspace: self.workspace,
            user: self.user,
            feature: self.feature,
            draft_id: self.draft_id,
            transcript_days: self.settings.transcript_days,
            budget_micros: self.settings.monthly_budget_micros,
            provider: self.provider.clone(),
            model: self.model.clone(),
            key: self.key,
            request: request.clone(),
        };
        match tokio::spawn(flight.run()).await {
            Ok(result) => result,
            // A panic outside the guarded provider call (the reservation or the bookkeeping): the
            // row, if one was reserved, keeps its worst-case cost until stale recovery.
            Err(_) => Err(interrupted()),
        }
    }
}

/// The error for a call that was cut short by a panic.
fn interrupted() -> DispatchError {
    AiError::Transport("the call was interrupted".to_owned()).into()
}

/// How often the bookkeeping of a finished call is tried before it is left to stale recovery.
const FINISH_ATTEMPTS: u32 = 3;

/// A transcript to store with the finished call.
struct NewTranscript {
    workspace: WorkspaceId,
    user: Option<UserId>,
    content: Value,
    expires_at: OffsetDateTime,
}

/// Finishes a reserved call's row, and stores its transcript, in one transaction, trying again
/// after a short pause when the database fails.
async fn finish(
    pool: &zirv_flow_store::PgPool,
    call: Uuid,
    finished: &FinishedAiCall,
    transcript: Option<&NewTranscript>,
) -> Result<(), StoreError> {
    let mut attempt = 1;
    loop {
        let tried = async {
            let mut tx = zirv_flow_store::begin(pool).await?;
            zirv_flow_store::finish_ai_call(&mut tx, call, finished).await?;
            if let Some(transcript) = transcript {
                zirv_flow_store::insert_ai_transcript(
                    &mut tx,
                    transcript.workspace,
                    call,
                    transcript.user,
                    &transcript.content,
                    transcript.expires_at,
                )
                .await?;
            }
            zirv_flow_store::commit(tx).await
        }
        .await;
        match tried {
            Err(error) if attempt < FINISH_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    "finishing an AI call failed, trying again: {error}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)))
                    .await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Everything a call needs once it runs on its own, the reservation included.
struct Flight {
    state: AppState,
    workspace: WorkspaceId,
    user: Option<UserId>,
    feature: &'static str,
    draft_id: Option<Uuid>,
    transcript_days: Option<i32>,
    budget_micros: i64,
    provider: AiProviderConfig,
    model: AiModelConfig,
    key: SecretId,
    request: ChatRequest,
}

impl Flight {
    /// Steps 2 and 3: reserves the call before anything is sent. In one transaction under a lock on
    /// the workspace it gives up on this workspace's calls that have been pending for longer than
    /// the client's timeout, then checks the per-user rate limit and the monthly budget, and
    /// records the call as `pending` at its worst-case cost: the model's price for the request's
    /// input-token upper bound and the full output cap. A call is admitted only when the month's
    /// spend plus that cost stays within the budget. A provider that reports usage above these
    /// bounds can overrun the budget by the calls in flight; its reported usage is still billed.
    /// Pending rows count in both limits, so calls still in flight are counted on every instance.
    /// Nothing is sent and nothing is recorded when a limit refuses. Returns the call's id
    /// and its worst-case cost.
    async fn reserve(&self) -> Result<(Uuid, i64), DispatchError> {
        let call = Uuid::now_v7();
        let worst_case = self.model.cost_micros(
            self.request.input_tokens_upper_bound(),
            self.request.max_output_tokens(),
        );
        let mut tx = zirv_flow_store::begin(&self.state.pool).await?;
        let reservation = zirv_flow_store::reserve_ai_call(
            &mut tx,
            &NewAiCall {
                id: call,
                workspace_id: self.workspace,
                user_id: self.user,
                feature: self.feature,
                provider_id: &self.provider.id,
                model: &self.model.name,
                input_tokens: 0,
                output_tokens: 0,
                estimated_cost_micros: worst_case,
                latency_ms: 0,
                outcome: CallOutcome::Pending,
                draft_id: self.draft_id,
            },
            ReservationLimits {
                rate_limit: i64::from(self.state.ai.config.rate_limit_per_user_per_minute),
                budget_micros: self.budget_micros,
                stale_after_secs: self.state.ai.stale_after().as_secs_f64(),
            },
            OffsetDateTime::now_utc(),
        )
        .await?;
        match reservation {
            Reservation::Granted => {
                zirv_flow_store::commit(tx).await?;
                Ok((call, worst_case))
            }
            Reservation::RateLimited => {
                refused("rate_limited");
                Err(AiError::RateLimited.into())
            }
            Reservation::BudgetExceeded => {
                refused("budget_exceeded");
                Err(AiError::BudgetExceeded.into())
            }
        }
    }

    async fn run(self) -> Result<ChatResponse, DispatchError> {
        let (call, worst_case) = self.reserve().await?;
        let started = Instant::now();
        // The key is opened right before the request. When it cannot be, nothing was sent: the
        // row is finished at no cost. The whole section is guarded, so a panic in a provider
        // adapter finishes the row here, whether or not the handler is still waiting.
        let guarded = AssertUnwindSafe(async {
            let key = open_provider_key(&self.state, self.workspace, self.key).await?;
            let http = self.state.ai.http(self.state.egress_policy())?;
            let provider = self.state.ai.provider(self.provider.kind);
            let endpoint = Endpoint {
                base_url: self.provider.base_url.clone(),
                model: self.model.name.clone(),
            };
            Ok::<_, DispatchError>(
                provider
                    .complete(http, &endpoint, &key, &self.request)
                    .await,
            )
        })
        .catch_unwind()
        .await;
        let Ok(sent) = guarded else {
            // Whether the provider billed the call is unknown, so the row keeps its worst-case
            // cost.
            let finished = FinishedAiCall {
                outcome: CallOutcome::Abandoned,
                input_tokens: 0,
                output_tokens: 0,
                estimated_cost_micros: worst_case,
                latency_ms: 0,
            };
            if let Err(error) = finish(&self.state.pool, call, &finished, None).await {
                tracing::warn!("finishing an abandoned AI call failed: {error}");
            }
            metrics::counter!(
                "zirv_flow_ai_calls_total",
                "feature" => self.feature,
                "outcome" => CallOutcome::Abandoned.as_str(),
            )
            .increment(1);
            return Err(interrupted());
        };
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        let (result, outcome, usage) = match sent {
            Ok(result) => {
                let (outcome, usage) = match &result {
                    Ok(response) => (CallOutcome::Ok, Some(response.usage())),
                    Err(error) => (outcome_of(error), None),
                };
                (result.map_err(DispatchError::from), outcome, usage)
            }
            Err(error) => (Err(error), CallOutcome::Transport, None),
        };
        let (input_tokens, output_tokens) =
            usage.map_or((0, 0), |usage| (usage.input_tokens, usage.output_tokens));
        if input_tokens > self.request.input_tokens_upper_bound()
            || output_tokens > self.request.max_output_tokens()
        {
            metrics::counter!(
                "zirv_flow_ai_provider_faults_total",
                "feature" => self.feature,
                "reason" => "usage_over_reservation",
            )
            .increment(1);
            tracing::warn!(
                feature = self.feature,
                reason = "usage_over_reservation",
                "AI provider reported usage above the request's reserved bounds"
            );
        }
        let transcript = self.transcript_days.map(|days| NewTranscript {
            workspace: self.workspace,
            user: self.user,
            content: transcript(&self.request, result.as_ref().ok()),
            expires_at: OffsetDateTime::now_utc() + Duration::days(i64::from(days)),
        });
        let finished = FinishedAiCall {
            outcome,
            input_tokens,
            output_tokens,
            estimated_cost_micros: self.model.cost_micros(input_tokens, output_tokens),
            latency_ms,
        };
        finish(&self.state.pool, call, &finished, transcript.as_ref()).await?;
        metrics::counter!(
            "zirv_flow_ai_calls_total",
            "feature" => self.feature,
            "outcome" => outcome.as_str(),
        )
        .increment(1);
        record_usage(self.feature, &finished);
        result
    }
}

/// Counts what a finished call used: `zirv_flow_ai_tokens_total{feature,direction}`,
/// `zirv_flow_ai_estimated_cost_micros_total{feature}` and the `zirv_flow_ai_latency_seconds{feature}`
/// histogram. The labels are the feature (a fixed name) and the direction (`input` or `output`):
/// never a key, a model, a prompt or an output.
fn record_usage(feature: &'static str, finished: &FinishedAiCall) {
    for (direction, tokens) in [
        ("input", finished.input_tokens),
        ("output", finished.output_tokens),
    ] {
        metrics::counter!(
            "zirv_flow_ai_tokens_total",
            "feature" => feature,
            "direction" => direction,
        )
        .increment(u64::from(tokens));
    }
    metrics::counter!("zirv_flow_ai_estimated_cost_micros_total", "feature" => feature)
        .increment(u64::try_from(finished.estimated_cost_micros).unwrap_or(0));
    metrics::histogram!("zirv_flow_ai_latency_seconds", "feature" => feature)
        .record(f64::from(finished.latency_ms) / 1000.0);
}

/// Every repair attempt is a separate dispatch and passes the gates again.
impl Sender for Dispatcher<'_> {
    fn send<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AiError>> {
        Box::pin(async move {
            let sent = async { self.call(request).await }.await;
            sent.map_err(|error| match error {
                DispatchError::Ai(error) => error,
                DispatchError::Store(_) => {
                    AiError::Transport("the call log could not be used".to_owned())
                }
                DispatchError::SecretsUnavailable | DispatchError::KeyUnusable => AiError::Disabled,
            })
        })
    }
}

/// Dispatches one request: the settings, the request built under the workspace's privacy level,
/// then the gated call. A refusal at a gate sends and records nothing.
pub async fn dispatch(
    state: &AppState,
    user: Option<UserId>,
    workspace: WorkspaceId,
    feature: &'static str,
    build: impl FnOnce(Privacy) -> Result<ChatRequest, AiError>,
) -> Result<ChatResponse, DispatchError> {
    let dispatcher = Dispatcher::open(state, user, workspace, feature).await?;
    let request = build(dispatcher.privacy()).inspect_err(|error| {
        if *error == AiError::PersonalDataNotAllowed {
            refused("personal_data");
        }
    })?;
    dispatcher.call(&request).await
}

/// The request and the response as stored in a transcript. Only called for a workspace that opted
/// in, and never includes the key.
fn transcript(request: &ChatRequest, response: Option<&ChatResponse>) -> Value {
    let messages: Vec<Value> = request
        .messages()
        .iter()
        .map(|message| match message {
            Message::User(contents) => json!({
                "role": "user",
                "content": contents.iter().map(|content| content.render()).collect::<Vec<_>>(),
            }),
            Message::Assistant(parts) => json!({"role": "assistant", "parts": parts}),
            Message::ToolResult { call_id, content } => json!({
                "role": "tool", "call_id": call_id, "content": content.render(),
            }),
        })
        .collect();
    json!({
        "request": {
            "system": request.system_text(),
            "messages": messages,
            "tools": request.tools().iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        },
        "response": response.map(|response| json!({
            "parts": response.parts().iter().collect::<Vec<&Part>>(),
            "model": response.model(),
        })),
    })
}

#[cfg(test)]
mod tests {
    use zirv_flow_ai::{Content, RequestBuilder};

    use super::*;

    #[test]
    fn failures_are_logged_under_their_outcome() {
        let outcome = |error: AiError| outcome_of(&error).as_str();
        assert_eq!(outcome(AiError::Http(500)), "http_error");
        assert_eq!(outcome(AiError::Timeout), "timeout");
        assert_eq!(
            outcome(AiError::Egress {
                host: "h".into(),
                class: zirv_flow_core::egress::AddressClass::Private
            }),
            "egress_denied"
        );
        assert_eq!(outcome(AiError::Capability("usage".into())), "capability");
        assert_eq!(outcome(AiError::Malformed), "malformed");
        assert_eq!(outcome(AiError::InvalidEndpoint), "transport");
    }

    #[test]
    fn dispatch_errors_become_the_documented_problems() {
        let status = |error: DispatchError| ApiError::from(error).status_code().as_u16();
        assert_eq!(status(AiError::Disabled.into()), 409);
        assert_eq!(status(AiError::BudgetExceeded.into()), 402);
        assert_eq!(status(AiError::RateLimited.into()), 429);
        assert_eq!(status(AiError::PersonalDataNotAllowed.into()), 422);
        assert_eq!(status(AiError::Http(500).into()), 502);
        assert_eq!(status(AiError::Schema("x".into()).into()), 500);
        assert_eq!(status(DispatchError::SecretsUnavailable), 503);
        assert_eq!(status(DispatchError::KeyUnusable), 500);
    }

    #[test]
    fn a_provider_failure_detail_is_fixed_text_without_the_provider_message() {
        let error = ApiError::from(DispatchError::Ai(AiError::Http(503)));
        assert_eq!(error.detail(), "the provider answered with status 503");
    }

    #[test]
    fn a_transcript_holds_the_request_and_response_text_and_tool_names_only() {
        let request = RequestBuilder::new(Privacy::PersonalAllowed)
            .system(Content::Instruction("Map columns.".into()))
            .unwrap()
            .user(vec![Content::Personal("ada@example.com".into())])
            .unwrap()
            .build();
        let response = zirv_flow_ai::fake::text_response("done", 0, 0);

        let stored = transcript(&request, Some(&response));

        // Personal text is fenced as data, so the notice comes first.
        let system = stored["request"]["system"].as_str().unwrap();
        assert!(
            system.starts_with(zirv_flow_ai::UNTRUSTED_NOTICE),
            "{system}"
        );
        assert!(system.ends_with("Map columns."), "{system}");
        assert_eq!(
            stored["request"]["messages"][0]["content"][0],
            "<untrusted_data>
ada@example.com
</untrusted_data>"
        );
        assert_eq!(stored["response"]["parts"][0]["text"], "done");
        assert!(transcript(&request, None)["response"].is_null());
    }
}
