//! The handler of the feedback route; see the module above for the order of its steps.

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{
    ProviderFeedbackKind, ProviderFeedbackRead, provider_feedback_of, provider_feedback_scheme,
    provider_read_feedback,
};
use zirv_flow_core::signature::{self, SignatureError};
use zirv_flow_store::{
    FeedbackEvent, FeedbackOutcome, ProviderFeedback, SuppressionMade, find_destination,
    find_destination_workspace, record_provider_feedback,
};

use crate::AppState;
use crate::ingest::{declared_too_large, read_body, too_large};
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;
use crate::workspace_credentials::resolve;

#[derive(Serialize, ToSchema)]
pub(crate) struct FeedbackAccepted {
    /// Always 1: the event was recorded, or dropped as one that needs nothing (an event type the
    /// manifest does not map, a repeat, a message that is not ours).
    accepted: usize,
}

/// A refused request and the metric label that counts it.
struct Refusal {
    error: ApiError,
    outcome: &'static str,
}

impl Refusal {
    fn new(error: ApiError, outcome: &'static str) -> Self {
        Self { error, outcome }
    }
}

/// Receives one signed feedback event of an email provider. The destination's manifest names the
/// signature header, its prefix and the optional timestamp header and tolerance; the signed
/// message is `"{timestamp}.{body}"` for a timestamped provider and the raw body otherwise, signed
/// with HMAC-SHA256 under the signing secret the destination's credential holds. A delivery is
/// recorded as `email.delivered`, a bounce as `email.bounced` (with `kind` `hard` or `soft`) and a
/// complaint as `email.complained`. A hard bounce suppresses the address the message was sent to
/// in every workspace of the organization, a complaint in the destination's workspace only, and a
/// soft bounce nobody. A repeat of an event writes nothing.
#[utoipa::path(
    post,
    operation_id = "receive_destination_feedback",
    path = "/api/v1/destinations/{id}/feedback",
    tag = "destinations",
    params(("id" = Uuid, Path, description = "The id of the provider destination, in the URL given to the provider")),
    request_body(content = Object, description = "One JSON event of the provider, at most ingest.max_webhook_bytes", content_type = "application/json"),
    responses(
        (status = 202, description = "Recorded, or dropped as needing nothing: an event type the manifest does not map, a repeat, or a message that is not this destination's", body = FeedbackAccepted),
        (status = 400, description = "The signed body is not JSON (code invalid_payload)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "The signature or timestamp is missing, malformed, outside the tolerance or wrong (code invalid_signature); nothing was written", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such destination, or it is not a provider destination", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_webhook_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The destination is over its rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until a request fits"))),
        (status = 503, description = "The database or the secret keyring is unavailable; nothing was recorded, so retry", body = Problem, content_type = "application/problem+json")
    ),
    security()
)]
pub(crate) async fn receive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match accept(&state, &id, request).await {
        Ok(outcome) => {
            count(outcome);
            (StatusCode::ACCEPTED, Json(FeedbackAccepted { accepted: 1 })).into_response()
        }
        Err(refusal) => {
            count(refusal.outcome);
            refusal.error.into_response()
        }
    }
}

/// Counts a request by its outcome, a fixed label: never an id, an address or a value.
fn count(outcome: &'static str) {
    metrics::counter!("zirv_flow_provider_feedback_total", "outcome" => outcome).increment(1);
}

fn unavailable() -> Refusal {
    Refusal::new(
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "feedback is unavailable; nothing was recorded, so retry",
        ),
        "unavailable",
    )
}

fn not_found() -> Refusal {
    Refusal::new(
        ApiError::new(StatusCode::NOT_FOUND, "no such destination"),
        "not_found",
    )
}

/// The 401 of a signature that does not verify. Every cause shares one type and one detail but
/// for a stale timestamp, which tells a sender with a bad clock; the cause is a metric label.
fn invalid_signature(error: SignatureError) -> Refusal {
    let (detail, outcome) = match error {
        SignatureError::OutsideTolerance => (
            "signature timestamp is outside the tolerance",
            "signature_stale",
        ),
        SignatureError::MissingSignature | SignatureError::MissingTimestamp => {
            ("signature is missing or invalid", "signature_missing")
        }
        SignatureError::MalformedSignature | SignatureError::MalformedTimestamp => {
            ("signature is missing or invalid", "signature_malformed")
        }
        SignatureError::Mismatch => ("signature is missing or invalid", "signature_mismatch"),
    };
    Refusal::new(
        ApiError::new(StatusCode::UNAUTHORIZED, detail).with_code("invalid_signature"),
        outcome,
    )
}

fn event_of(kind: ProviderFeedbackKind) -> FeedbackEvent {
    match kind {
        ProviderFeedbackKind::Delivered => FeedbackEvent::Delivered,
        ProviderFeedbackKind::Bounced { hard: true } => FeedbackEvent::BouncedHard,
        ProviderFeedbackKind::Bounced { hard: false } => FeedbackEvent::BouncedSoft,
        ProviderFeedbackKind::Complained => FeedbackEvent::Complained,
    }
}

/// The outcome label of a recorded feedback.
fn outcome_label(outcome: FeedbackOutcome) -> &'static str {
    match outcome {
        FeedbackOutcome::Recorded {
            suppression: SuppressionMade::Unable,
            ..
        } => "unable_to_suppress",
        FeedbackOutcome::Recorded { .. } => "recorded",
        FeedbackOutcome::Duplicate => "duplicate",
        FeedbackOutcome::UnknownMessage => "unknown_message",
    }
}

fn verify(
    now: OffsetDateTime,
    scheme: &signature::Scheme<'_>,
    secret: &[u8],
    headers: &HeaderMap,
    body: &[u8],
) -> Result<signature::Verified, SignatureError> {
    // Every value of a header counts: a sender in a rotation grace window signs with two secrets
    // and sends one signature value for each.
    let lookup = |name: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>()
    };
    signature::verify(scheme, secret, &lookup, body, now.unix_timestamp())
}

async fn accept(state: &AppState, id: &str, request: Request) -> Result<&'static str, Refusal> {
    let max = state.ingest.limits().max_webhook_bytes;
    if declared_too_large(request.headers(), max) {
        return Err(Refusal::new(too_large(), "too_large"));
    }
    let id = Uuid::parse_str(id).map_err(|_| not_found())?;
    let workspace = find_destination_workspace(&state.pool, id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(not_found)?;
    let destination = find_destination(&state.pool, workspace, id)
        .await
        .map_err(|_| unavailable())?
        .filter(|destination| destination.kind == "provider")
        .ok_or_else(not_found)?;
    let (spec, verification) = provider_feedback_of(&destination.config).ok_or_else(not_found)?;
    // Charged only now, for a destination that exists: an id that resolves to nothing never gets
    // a bucket, so the limiter holds at most one entry per stored destination.
    state
        .ingest
        .charge_hook(destination.id)
        .map_err(|error| Refusal::new(error, "rate_limited"))?;

    let headers = request.headers().clone();
    let body = read_body(request, max)
        .await
        .map_err(|error| Refusal::new(error, "too_large"))?;

    // The signature first. The secret is opened right before the compare and wiped when dropped.
    {
        let credential = resolve::load(state, workspace, destination.credential_id)
            .await
            .map_err(|_| unavailable())?;
        let secret = credential
            .get(&verification.secret_field)
            .ok_or_else(unavailable)?;
        verify(
            state.clock.now_in_process(),
            &provider_feedback_scheme(&verification),
            secret.expose().as_bytes(),
            &headers,
            &body,
        )
        .map_err(invalid_signature)?;
    }

    let event: Value = serde_json::from_slice(&body).map_err(|_| {
        Refusal::new(
            ApiError::bad_request("the body must be JSON").with_code("invalid_payload"),
            "invalid_payload",
        )
    })?;
    let feedback = match provider_read_feedback(&spec, &event) {
        ProviderFeedbackRead::Event(feedback) => feedback,
        ProviderFeedbackRead::Ignored => return Ok("ignored"),
        ProviderFeedbackRead::Malformed => return Ok("malformed"),
    };
    let Ok(message) = Uuid::parse_str(&feedback.message_id) else {
        return Ok("unknown_message");
    };
    let outcome = record_provider_feedback(
        &state.pool,
        &ProviderFeedback {
            workspace,
            destination: destination.id,
            message,
            event: event_of(feedback.kind),
            provider_event_id: feedback.event_id.as_deref(),
            now: state.clock.now_in_process(),
            fallback_trace: &request_trace(),
        },
    )
    .await
    .map_err(|_| unavailable())?;
    Ok(outcome_label(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_signature_cause_is_a_401_with_one_code_and_a_stale_timestamp_says_so() {
        for error in [
            SignatureError::MissingSignature,
            SignatureError::MalformedSignature,
            SignatureError::MissingTimestamp,
            SignatureError::MalformedTimestamp,
            SignatureError::OutsideTolerance,
            SignatureError::Mismatch,
        ] {
            let refusal = invalid_signature(error);
            assert_eq!(refusal.error.status_code(), StatusCode::UNAUTHORIZED);
            assert_eq!(refusal.error.code(), Some("invalid_signature"));
        }
        assert_eq!(
            invalid_signature(SignatureError::OutsideTolerance).outcome,
            "signature_stale"
        );
        assert_eq!(
            invalid_signature(SignatureError::Mismatch).error.detail(),
            invalid_signature(SignatureError::MissingSignature)
                .error
                .detail()
        );
    }

    #[test]
    fn a_kind_maps_to_its_event_and_an_outcome_to_a_fixed_label() {
        assert_eq!(
            event_of(ProviderFeedbackKind::Bounced { hard: true }),
            FeedbackEvent::BouncedHard
        );
        assert_eq!(
            event_of(ProviderFeedbackKind::Bounced { hard: false }),
            FeedbackEvent::BouncedSoft
        );
        assert_eq!(
            event_of(ProviderFeedbackKind::Complained),
            FeedbackEvent::Complained
        );
        assert_eq!(
            outcome_label(FeedbackOutcome::Recorded {
                event: None,
                suppression: SuppressionMade::Unable
            }),
            "unable_to_suppress"
        );
        assert_eq!(outcome_label(FeedbackOutcome::Duplicate), "duplicate");
        assert_eq!(
            outcome_label(FeedbackOutcome::UnknownMessage),
            "unknown_message"
        );
    }
}
