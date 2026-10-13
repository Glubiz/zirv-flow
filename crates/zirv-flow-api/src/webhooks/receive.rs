//! `POST /hooks/{id}`: the public edge of an inbound webhook (architecture sections 7.1, 15
//! and 16.1). It verifies an HMAC-SHA256 signature in constant time, enforces a body cap and a
//! per-hook rate, and appends the payload to the durable inbox; mapping happens in the drain.
//!
//! The URL id only locates the hook; the signature authorizes. Nothing is written before the
//! signature verifies, and no log line, metric label or problem body holds the payload, a
//! header value, a signature, a secret or the hook id.

use axum::Json;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::mapping::walk_path;
use zirv_flow_core::signature::{self, Scheme, SignatureError};
use zirv_flow_core::{SecretId, SecretPurpose};
use zirv_flow_store::HookReceive;

use crate::AppState;
use crate::ingest::{declared_too_large, read_body, too_large};
use crate::problem::{ApiError, Problem};
use crate::trace::request_trace;

/// The longest dedupe value, in characters.
const MAX_DEDUPE_CHARS: usize = 128;

#[derive(Serialize, ToSchema)]
pub(crate) struct WebhookAccepted {
    /// Payloads appended to the inbox: always 1. They are durable, not yet applied.
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

/// Receives one JSON document for a webhook. The request carries the hook's signature header
/// (and timestamp header, unless the hook signs the body alone); the names, the prefix and the
/// tolerance are the hook's, shown in the sources UI. The signed message is `"{timestamp}.{body}"`
/// for a timestamped hook and the raw body otherwise, signed with HMAC-SHA256 under the hook's
/// secret and sent as the prefix followed by 64 hex digits.
#[utoipa::path(
    post,
    operation_id = "receive_webhook",
    path = "/hooks/{id}",
    tag = "webhooks",
    params(("id" = Uuid, Path, description = "The id in the hook's URL")),
    request_body(content = Object, description = "One JSON object, at most ingest.max_webhook_bytes", content_type = "application/json"),
    responses(
        (status = 202, description = "Appended to the inbox: durable, not yet applied", body = WebhookAccepted),
        (status = 400, description = "The body is not a JSON object (code invalid_payload)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "The signature or timestamp is missing, malformed, outside the tolerance or wrong (code invalid_signature)", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such hook, or it is not receiving (disabled or unmapped)", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_webhook_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The payload has no usable value at the hook's dedupe field (code dedupe_missing)", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The hook is over its rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until a request fits"))),
        (status = 503, description = "The database or the secret keyring is unavailable; nothing was stored", body = Problem, content_type = "application/problem+json")
    ),
    security()
)]
pub(crate) async fn receive(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    match accept(&state, &id, request).await {
        Ok(()) => {
            count("accepted");
            (StatusCode::ACCEPTED, Json(WebhookAccepted { accepted: 1 })).into_response()
        }
        Err(refusal) => {
            count(refusal.outcome);
            refusal.error.into_response()
        }
    }
}

fn count(outcome: &'static str) {
    metrics::counter!("zirv_flow_webhook_requests_total", "outcome" => outcome).increment(1);
}

fn unavailable() -> Refusal {
    Refusal::new(
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the webhook is unavailable; nothing was stored, so retry",
        ),
        "unavailable",
    )
}

fn not_found() -> Refusal {
    Refusal::new(
        ApiError::new(StatusCode::NOT_FOUND, "no such webhook"),
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

/// The dedupe value of a payload: a string, integer or float at `field`, 1 to 128 characters
/// without control characters, scalars as their JSON text.
pub(crate) fn dedupe_value(payload: &Map<String, Value>, field: &str) -> Option<String> {
    let text = match walk_path(payload, field)? {
        Value::String(text) => text.clone(),
        number @ Value::Number(_) => number.to_string(),
        _ => return None,
    };
    let length = text.chars().count();
    let usable = (1..=MAX_DEDUPE_CHARS).contains(&length) && !text.chars().any(char::is_control);
    usable.then_some(text)
}

fn scheme(hook: &HookReceive) -> Scheme<'_> {
    Scheme {
        signature_header: &hook.signature_header,
        signature_prefix: &hook.signature_prefix,
        timestamp_header: hook.timestamp_header.as_deref(),
        tolerance_seconds: i64::from(hook.tolerance_seconds),
    }
}

/// Opens the hook's secret right before the compare; it is wiped when dropped.
async fn open_secret(
    state: &AppState,
    hook: &HookReceive,
) -> Result<zirv_flow_core::SecretBytes, Refusal> {
    let keyring = state.keyring().ok_or_else(unavailable)?;
    zirv_flow_store::secrets::open_json(
        &state.pool,
        keyring,
        hook.workspace(),
        SecretId::from_uuid(hook.secret_id),
        SecretPurpose::WebhookSecret,
    )
    .await
    .map_err(|_| unavailable())
}

fn verify(
    hook: &HookReceive,
    secret: &zirv_flow_core::SecretBytes,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<signature::Verified, SignatureError> {
    let lookup = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    signature::verify(
        &scheme(hook),
        secret.expose(),
        &lookup,
        body,
        OffsetDateTime::now_utc().unix_timestamp(),
    )
}

async fn accept(state: &AppState, id: &str, request: Request) -> Result<(), Refusal> {
    let max = state.ingest.limits().max_webhook_bytes;
    if declared_too_large(request.headers(), max) {
        return Err(Refusal::new(too_large(), "too_large"));
    }
    let id = Uuid::parse_str(id).map_err(|_| not_found())?;
    let hook = zirv_flow_store::find_for_receive(&state.pool, id)
        .await
        .map_err(|_| unavailable())?
        .filter(|hook| hook.receiving)
        .ok_or_else(not_found)?;
    // Charged only now, for a hook that exists: an id that resolves to nothing never gets a
    // bucket, so the limiter holds at most one entry per stored hook.
    state
        .ingest
        .charge_hook(hook.id)
        .map_err(|error| Refusal::new(error, "rate_limited"))?;

    let headers = request.headers().clone();
    let body = read_body(request, max)
        .await
        .map_err(|error| Refusal::new(error, "too_large"))?;
    let verified = {
        let secret = open_secret(state, &hook).await?;
        verify(&hook, &secret, &headers, &body).map_err(invalid_signature)?
    };

    let invalid_payload = || {
        Refusal::new(
            ApiError::bad_request("the body must be one JSON object").with_code("invalid_payload"),
            "invalid_payload",
        )
    };
    let Value::Object(payload) = serde_json::from_slice(&body).map_err(|_| invalid_payload())?
    else {
        return Err(invalid_payload());
    };
    let value = dedupe_value(&payload, &hook.dedupe_field).ok_or_else(|| {
        Refusal::new(
            ApiError::unprocessable("the payload has no usable value at the dedupe field")
                .with_code("dedupe_missing"),
            "dedupe_missing",
        )
    })?;
    let signed_at = verified
        .signed_at
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok());
    let message_id = format!("wh:{}:{value}", hook.id);
    zirv_flow_store::append_webhook(
        &state.pool,
        &hook,
        &request_trace(),
        &Value::Object(payload),
        signed_at,
        &message_id,
        state.ingest.report_due(hook.id),
    )
    .await
    .map_err(|_| unavailable())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn payload(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn the_dedupe_value_is_a_string_or_number_of_one_to_128_printable_characters() {
        let object =
            payload(json!({"id": "o-1", "n": 42, "f": 1.5, "nest": {"id": "deep"}, "list": ["x"]}));

        assert_eq!(dedupe_value(&object, "id").as_deref(), Some("o-1"));
        assert_eq!(dedupe_value(&object, "n").as_deref(), Some("42"));
        assert_eq!(dedupe_value(&object, "f").as_deref(), Some("1.5"));
        assert_eq!(dedupe_value(&object, "nest.id").as_deref(), Some("deep"));
        assert_eq!(dedupe_value(&object, "list.0").as_deref(), Some("x"));
        assert_eq!(dedupe_value(&object, "missing"), None);
        assert_eq!(dedupe_value(&object, "nest"), None);
    }

    #[test]
    fn an_unusable_dedupe_value_is_none() {
        for value in [
            json!({"id": ""}),
            json!({"id": true}),
            json!({"id": null}),
            json!({"id": "a\nb"}),
            json!({"id": "x".repeat(129)}),
        ] {
            assert_eq!(dedupe_value(&payload(value), "id"), None);
        }
        assert!(dedupe_value(&payload(json!({"id": "x".repeat(128)})), "id").is_some());
    }

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
        }
        assert_eq!(
            invalid_signature(SignatureError::OutsideTolerance).outcome,
            "signature_stale"
        );
        assert_eq!(
            invalid_signature(SignatureError::Mismatch).outcome,
            "signature_mismatch"
        );
        assert_eq!(
            invalid_signature(SignatureError::MissingTimestamp).outcome,
            "signature_missing"
        );
        assert_eq!(
            invalid_signature(SignatureError::MalformedTimestamp).outcome,
            "signature_malformed"
        );
        assert_eq!(
            invalid_signature(SignatureError::Mismatch).error.detail(),
            invalid_signature(SignatureError::MissingSignature)
                .error
                .detail()
        );
    }
}
