//! The ingestion endpoints (architecture sections 4.4, 7.1 and 15): `identify`, `track` and
//! `batch`, called with a write-only API key. A request is validated for structure, charged
//! against the key's rate limit, and appended to the durable inbox; `202` is returned once the
//! append has committed. Applying the items is the drain job's work.
//!
//! No log line, metric label or problem body holds an identifier, a trait or property value
//! or an event name.

mod limiter;
mod payload;

use hashbrown::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use crate::AppState;
use crate::auth::Principal;
use crate::problem::{ApiError, ItemProblem, Problem};
use crate::trace::request_trace;
use limiter::RateLimiter;
use payload::{ItemError, Kind, batch_items, parse_items};

/// What an ingestion request may carry and how fast a key may send. The rate is per
/// `serve` process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngestLimits {
    /// The largest request body, in bytes.
    pub max_request_bytes: usize,
    /// The most items in one `batch` request.
    pub max_batch_items: usize,
    /// Items per second each key may send, sustained.
    pub rate_per_key: u32,
    /// Items a key may send at once after a quiet period.
    pub burst_per_key: u32,
    /// The largest body of a webhook request, in bytes.
    pub max_webhook_bytes: usize,
    /// Requests per second each webhook may receive, sustained.
    pub rate_per_hook: u32,
    /// Requests a webhook may receive at once after a quiet period.
    pub burst_per_hook: u32,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_batch_items: 500,
            rate_per_key: 10_000,
            burst_per_key: 20_000,
            max_webhook_bytes: 256 * 1024,
            rate_per_hook: 50,
            burst_per_hook: 200,
        }
    }
}

/// The limits and the buckets they are enforced with; clones share the buckets.
#[derive(Clone)]
pub(crate) struct IngestState {
    limits: IngestLimits,
    limiter: Arc<RateLimiter>,
    /// One bucket per hook that resolved to a stored hook, so unknown ids never reach it.
    hook_limiter: Arc<RateLimiter>,
    /// When each hook last reported its freshness from this process.
    reported: Arc<Mutex<HashMap<Uuid, Instant>>>,
}

impl IngestState {
    pub(crate) fn new(limits: IngestLimits) -> Self {
        let limiter = Arc::new(RateLimiter::new(
            limits.rate_per_key,
            limits.burst_per_key,
            Instant::now(),
        ));
        let hook_limiter = Arc::new(RateLimiter::new(
            limits.rate_per_hook,
            limits.burst_per_hook,
            Instant::now(),
        ));
        Self {
            limits,
            limiter,
            hook_limiter,
            reported: Arc::default(),
        }
    }

    pub(crate) fn limits(&self) -> &IngestLimits {
        &self.limits
    }

    /// Charges `items` against the bucket of `id` (an API key id, or a user id for a session):
    /// a 429 with `Retry-After` when they do not fit.
    pub(crate) fn charge(&self, id: Uuid, items: usize) -> Result<(), ApiError> {
        self.limiter
            .take(id, items, Instant::now())
            .map_err(|seconds| {
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "the API key is over its rate limit",
                )
                .with_retry_after(seconds)
            })
    }

    /// Charges one request against the bucket of a hook that exists: a 429 with `Retry-After`
    /// when it does not fit. Only ids that resolved to a stored hook get a bucket, so a flood of
    /// random ids cannot grow the limiter.
    pub(crate) fn charge_hook(&self, hook: Uuid) -> Result<(), ApiError> {
        self.hook_limiter
            .take(hook, 1, Instant::now())
            .map_err(|seconds| {
                ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "the webhook is over its rate limit",
                )
                .with_retry_after(seconds)
            })
    }

    /// How many hooks hold a rate-limit bucket.
    #[cfg(test)]
    pub(crate) fn hook_buckets(&self) -> usize {
        self.hook_limiter.tracked()
    }

    /// The slot this request takes when it should report the hook's freshness: at most once per
    /// window per hook per process, so a busy hook does not serialize on its stream row.
    pub(crate) fn report_due(&self, hook: Uuid) -> Option<Instant> {
        self.report_due_at(hook, Instant::now())
    }

    /// Gives back the slot [`Self::report_due`] took, when the append it was for failed, so the
    /// next request reports instead. A slot a later request has taken since stays.
    pub(crate) fn release_report(&self, hook: Uuid, taken: Instant) {
        let mut reported = self
            .reported
            .lock()
            .expect("the report lock is never held across a panic");
        if reported.get(&hook) == Some(&taken) {
            reported.remove(&hook);
        }
    }

    fn report_due_at(&self, hook: Uuid, now: Instant) -> Option<Instant> {
        let mut reported = self
            .reported
            .lock()
            .expect("the report lock is never held across a panic");
        if reported
            .get(&hook)
            .is_some_and(|last| now.saturating_duration_since(*last) < REPORT_EVERY)
        {
            return None;
        }
        reported.retain(|_, last| now.saturating_duration_since(*last) < REPORT_EVERY);
        reported.insert(hook, now);
        Some(now)
    }
}

/// How often a hook reports its freshness from one process.
const REPORT_EVERY: Duration = Duration::from_secs(60);

#[derive(Serialize, ToSchema)]
pub(crate) struct AcceptedBody {
    /// Items appended to the inbox. They are durable, not yet applied.
    accepted: usize,
}

/// The body of `POST /api/v1/ingest/identify`: one profile update.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name fields"
)]
pub(crate) struct IdentifyRequest {
    /// Maps to the identifier `external:user_id`.
    #[schema(rename = "userId")]
    user_id: Option<String>,
    /// Maps to the identifier `anonymous_id`.
    #[schema(rename = "anonymousId")]
    anonymous_id: Option<String>,
    /// Identifier kind to value: `email`, `phone`, `anonymous_id` or `external:<namespace>`.
    /// At least one of `userId`, `anonymousId` and `identifiers` is required.
    identifiers: Option<std::collections::BTreeMap<String, String>>,
    /// RFC 3339 time the update was observed; the arrival time when absent.
    timestamp: Option<String>,
    /// 1 to 200 characters. Resending the same `messageId` is safe; without one, a retry can
    /// be applied twice.
    #[schema(rename = "messageId")]
    message_id: Option<String>,
    /// Trait key to value. A `null` value removes the trait this source set.
    #[schema(value_type = Option<Object>)]
    traits: Option<serde_json::Map<String, Value>>,
}

/// The body of `POST /api/v1/ingest/track`: one event.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name fields"
)]
pub(crate) struct TrackRequest {
    /// Maps to the identifier `external:user_id`.
    #[schema(rename = "userId")]
    user_id: Option<String>,
    /// Maps to the identifier `anonymous_id`.
    #[schema(rename = "anonymousId")]
    anonymous_id: Option<String>,
    /// Identifier kind to value. At least one of `userId`, `anonymousId` and `identifiers`
    /// is required.
    identifiers: Option<std::collections::BTreeMap<String, String>>,
    /// RFC 3339 time the event occurred; the arrival time when absent.
    timestamp: Option<String>,
    /// 1 to 200 characters. Resending the same `messageId` stores the event once.
    #[schema(rename = "messageId")]
    message_id: Option<String>,
    /// The event name, 1 to 128 characters.
    event: String,
    #[schema(value_type = Option<Object>)]
    properties: Option<serde_json::Map<String, Value>>,
}

/// One item of a batch: an identify or a track, named in `type`.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name fields"
)]
pub(crate) struct BatchItemRequest {
    /// `identify` or `track`.
    #[schema(rename = "type")]
    item_type: String,
    #[schema(rename = "userId")]
    user_id: Option<String>,
    #[schema(rename = "anonymousId")]
    anonymous_id: Option<String>,
    identifiers: Option<std::collections::BTreeMap<String, String>>,
    timestamp: Option<String>,
    #[schema(rename = "messageId")]
    message_id: Option<String>,
    /// For an `identify`.
    #[schema(value_type = Option<Object>)]
    traits: Option<serde_json::Map<String, Value>>,
    /// For a `track`: required.
    event: Option<String>,
    /// For a `track`.
    #[schema(value_type = Option<Object>)]
    properties: Option<serde_json::Map<String, Value>>,
}

/// The body of `POST /api/v1/ingest/batch`.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the request in OpenAPI; the body is parsed by hand so errors name fields"
)]
pub(crate) struct BatchRequest {
    /// At least one item, at most `ingest.max_batch_items`. All are validated before any is
    /// appended.
    batch: Vec<BatchItemRequest>,
}

/// Which endpoint a request came in on, for the request counter's label.
#[derive(Clone, Copy)]
enum Endpoint {
    Identify,
    Track,
    Batch,
}

impl Endpoint {
    const fn label(self) -> &'static str {
        match self {
            Self::Identify => "identify",
            Self::Track => "track",
            Self::Batch => "batch",
        }
    }

    const fn kind(self) -> Option<Kind> {
        match self {
            Self::Identify => Some(Kind::Identify),
            Self::Track => Some(Kind::Track),
            Self::Batch => None,
        }
    }
}

/// Items appended by one request, by kind.
struct Accepted {
    identify: usize,
    track: usize,
}

/// Sends one profile update. Needs an API key with the `ingest` scope.
#[utoipa::path(
    post,
    operation_id = "ingest_identify",
    path = "/api/v1/ingest/identify",
    tag = "ingest",
    request_body = IdentifyRequest,
    responses(
        (status = 202, description = "Appended to the inbox: durable, not yet applied", body = AcceptedBody),
        (status = 400, description = "An invalid field; errors lists them by index, field name and reason code", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Missing or invalid API key", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A session, or a key without the ingest scope", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_request_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The key is over its rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until the request's items fit"))),
        (status = 503, description = "The database is unavailable; nothing was stored", body = Problem, content_type = "application/problem+json")
    ),
    security(("api_key" = []))
)]
pub(crate) async fn identify(
    State(state): State<AppState>,
    principal: Result<Principal, ApiError>,
    request: Request,
) -> Response {
    run(Endpoint::Identify, &state, principal, request).await
}

/// Sends one event. Needs an API key with the `ingest` scope.
#[utoipa::path(
    post,
    operation_id = "ingest_track",
    path = "/api/v1/ingest/track",
    tag = "ingest",
    request_body = TrackRequest,
    responses(
        (status = 202, description = "Appended to the inbox: durable, not yet applied", body = AcceptedBody),
        (status = 400, description = "An invalid field; errors lists them by index, field name and reason code", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Missing or invalid API key", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A session, or a key without the ingest scope", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_request_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The key is over its rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until the request's items fit"))),
        (status = 503, description = "The database is unavailable; nothing was stored", body = Problem, content_type = "application/problem+json")
    ),
    security(("api_key" = []))
)]
pub(crate) async fn track(
    State(state): State<AppState>,
    principal: Result<Principal, ApiError>,
    request: Request,
) -> Response {
    run(Endpoint::Track, &state, principal, request).await
}

/// Sends identify and track items together. All items are validated before any is
/// appended, and they are applied in order. Needs an API key with the `ingest` scope.
#[utoipa::path(
    post,
    operation_id = "ingest_batch",
    path = "/api/v1/ingest/batch",
    tag = "ingest",
    request_body = BatchRequest,
    responses(
        (status = 202, description = "Appended to the inbox: durable, not yet applied", body = AcceptedBody),
        (status = 400, description = "An invalid item or too many items; errors lists them by index, field name and reason code", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Missing or invalid API key", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A session, or a key without the ingest scope", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is over ingest.max_request_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "The key is over its rate limit", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Whole seconds until the request's items fit"))),
        (status = 503, description = "The database is unavailable; nothing was stored", body = Problem, content_type = "application/problem+json")
    ),
    security(("api_key" = []))
)]
pub(crate) async fn batch(
    State(state): State<AppState>,
    principal: Result<Principal, ApiError>,
    request: Request,
) -> Response {
    run(Endpoint::Batch, &state, principal, request).await
}

async fn run(
    endpoint: Endpoint,
    state: &AppState,
    principal: Result<Principal, ApiError>,
    request: Request,
) -> Response {
    let response = match accept(endpoint, state, principal, request).await {
        Ok(accepted) => {
            for (kind, count) in [("identify", accepted.identify), ("track", accepted.track)] {
                if count > 0 {
                    metrics::counter!("zirv_flow_ingest_items_total", "kind" => kind)
                        .increment(count as u64);
                }
            }
            let body = AcceptedBody {
                accepted: accepted.identify + accepted.track,
            };
            (StatusCode::ACCEPTED, Json(body)).into_response()
        }
        Err(error) => error.into_response(),
    };
    metrics::counter!(
        "zirv_flow_ingest_requests_total",
        "endpoint" => endpoint.label(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    response
}

async fn accept(
    endpoint: Endpoint,
    state: &AppState,
    principal: Result<Principal, ApiError>,
    request: Request,
) -> Result<Accepted, ApiError> {
    // A database that cannot answer the key lookup is the same outage as one that cannot
    // take the append.
    let principal = principal.map_err(|error| {
        if error.status_code() == StatusCode::INTERNAL_SERVER_ERROR {
            unavailable()
        } else {
            error
        }
    })?;
    let key = principal.require_ingest_key()?;
    let limits = &state.ingest.limits;

    let body = read_json(request, limits.max_request_bytes).await?;

    let values: &[Value] = match endpoint {
        Endpoint::Batch => batch_items(&body, limits.max_batch_items).map_err(invalid)?,
        Endpoint::Identify | Endpoint::Track => std::slice::from_ref(&body),
    };
    state.ingest.charge(key.key.id.as_uuid(), values.len())?;
    let items = parse_items(values, endpoint.kind()).map_err(invalid)?;

    let accepted = Accepted {
        identify: items
            .iter()
            .filter(|item| item.payload.kind() == "identify")
            .count(),
        track: items
            .iter()
            .filter(|item| item.payload.kind() == "track")
            .count(),
    };
    zirv_flow_store::append_inbox(
        &state.pool,
        key.key.workspace_id,
        key.key.id.as_uuid(),
        &request_trace(),
        &items,
    )
    .await
    .map_err(|_| unavailable())?;
    Ok(accepted)
}

pub(crate) fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large")
}

/// Reads the JSON body of an ingestion request: `413` over `max_bytes`, declared or not, or
/// when it cannot be read, and `400` when it is not JSON.
pub(crate) async fn read_json(request: Request, max_bytes: usize) -> Result<Value, ApiError> {
    let bytes = read_body(request, max_bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("request body is not valid JSON"))
}

/// Whether the declared `Content-Length` is over `max_bytes`.
pub(crate) fn declared_too_large(headers: &axum::http::HeaderMap, max_bytes: usize) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > max_bytes)
}

/// Reads a request body of at most `max_bytes`: `413` when it is declared larger, streams
/// larger, or cannot be read.
pub(crate) async fn read_body(
    request: Request,
    max_bytes: usize,
) -> Result<axum::body::Bytes, ApiError> {
    if declared_too_large(request.headers(), max_bytes) {
        return Err(too_large());
    }
    axum::body::to_bytes(request.into_body(), max_bytes)
        .await
        .map_err(|_| too_large())
}

pub(crate) fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "ingestion is unavailable; nothing was stored, so retry",
    )
}

/// A 400 that lists the invalid fields.
fn invalid(errors: Vec<ItemError>) -> ApiError {
    ApiError::bad_request("one or more items are invalid").with_errors(
        errors
            .into_iter()
            .map(|error| ItemProblem {
                index: u32::try_from(error.index).unwrap_or(u32::MAX),
                field: error.field,
                reason: error.reason.to_owned(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_limits_are_the_documented_ones() {
        let limits = IngestLimits::default();

        assert_eq!(limits.max_request_bytes, 1_048_576);
        assert_eq!(limits.max_batch_items, 500);
        assert_eq!(
            (limits.rate_per_key, limits.burst_per_key),
            (10_000, 20_000)
        );
    }

    #[test]
    fn an_invalid_request_lists_index_field_and_reason_only() {
        let error = invalid(vec![ItemError {
            index: 3,
            field: "email".into(),
            reason: "invalid_email",
        }]);

        assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(error.detail(), "one or more items are invalid");
    }

    #[test]
    fn the_outage_answer_is_a_503_that_says_nothing_was_stored() {
        let error = unavailable();

        assert_eq!(error.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(error.detail().contains("nothing was stored"));
    }

    #[test]
    fn endpoints_label_themselves_and_only_the_batch_names_its_kinds() {
        assert_eq!(Endpoint::Identify.label(), "identify");
        assert_eq!(Endpoint::Track.label(), "track");
        assert_eq!(Endpoint::Batch.label(), "batch");
        assert_eq!(Endpoint::Identify.kind(), Some(Kind::Identify));
        assert_eq!(Endpoint::Track.kind(), Some(Kind::Track));
        assert_eq!(Endpoint::Batch.kind(), None);
    }

    #[test]
    fn a_released_report_slot_is_due_again_at_once() {
        let state = IngestState::new(IngestLimits::default());
        let hook = Uuid::from_u128(1);
        let now = Instant::now();

        let taken = state.report_due_at(hook, now).unwrap();
        assert_eq!(state.report_due_at(hook, now), None);
        state.release_report(hook, taken);
        assert_eq!(state.report_due_at(hook, now), Some(now));
    }

    #[test]
    fn a_stale_release_leaves_a_later_requests_slot() {
        let state = IngestState::new(IngestLimits::default());
        let hook = Uuid::from_u128(1);
        let first = Instant::now();
        let later = first + REPORT_EVERY;

        let stale = state.report_due_at(hook, first).unwrap();
        assert_eq!(state.report_due_at(hook, later), Some(later));
        state.release_report(hook, stale);
        assert_eq!(state.report_due_at(hook, later), None);
    }

    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::json;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_core::{Scope, Slug, WorkspaceId, WorkspaceName};

    use crate::admin::{self, Context};
    use crate::router;

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Value,
    }

    struct Fixture {
        router: Router,
        pool: PgPool,
        workspace: WorkspaceId,
        key: String,
    }

    async fn fixture_with(pool: &PgPool, scopes: &[Scope], limits: IngestLimits) -> Fixture {
        zirv_flow_store::migrate(pool).await.unwrap();
        let context = Context::system();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let workspace = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let created = admin::create_api_key(pool, &context, workspace, "ingest", scopes)
            .await
            .unwrap();
        let state = AppState::new(pool.clone(), Arc::new(String::new)).with_ingest_limits(limits);
        Fixture {
            router: router(state),
            pool: pool.clone(),
            workspace,
            key: created.plaintext,
        }
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        fixture_with(pool, &[Scope::Ingest], IngestLimits::default()).await
    }

    async fn post(router: &Router, path: &str, auth: Option<&str>, body: Body) -> Reply {
        let mut request = Request::post(path).header("content-type", "application/json");
        if let Some(auth) = auth {
            request = request.header("authorization", auth);
        }
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    impl Fixture {
        async fn send(&self, path: &str, body: Value) -> Reply {
            let auth = format!("Bearer {}", self.key);
            post(
                &self.router,
                path,
                Some(&auth),
                Body::from(body.to_string()),
            )
            .await
        }

        async fn inbox(&self) -> Vec<(String, String, uuid::Uuid)> {
            sqlx::query_as("SELECT kind, message_id, trace_id FROM ingest_inbox ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identify_is_accepted_with_a_202_and_lands_in_the_inbox_with_the_request_trace(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let reply = fx
            .send(
                "/api/v1/ingest/identify",
                json!({"userId": "u-1", "traits": {"plan": "pro"}, "messageId": "m-1"}),
            )
            .await;

        assert_eq!(reply.status, StatusCode::ACCEPTED);
        assert_eq!(reply.body, json!({"accepted": 1}));
        let rows = fx.inbox().await;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1.as_str()),
            ("identify", "m-1")
        );
        let header = reply.headers["x-trace-id"].to_str().unwrap();
        assert_eq!(rows[0].2.simple().to_string(), header);
        let job: (String, uuid::Uuid, Option<uuid::Uuid>) =
            sqlx::query_as("SELECT kind, trace_id, workspace_id FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(job.0, "ingest.drain");
        assert_eq!(job.1, rows[0].2);
        assert_eq!(job.2, Some(fx.workspace.as_uuid()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_track_without_a_message_id_gets_a_server_assigned_one(pool: PgPool) {
        let fx = fixture(&pool).await;

        let reply = fx
            .send(
                "/api/v1/ingest/track",
                json!({"anonymousId": "a-1", "event": "seen", "properties": {"n": 1}}),
            )
            .await;

        assert_eq!(reply.status, StatusCode::ACCEPTED);
        let rows = fx.inbox().await;
        assert_eq!(rows[0].0, "track");
        assert!(uuid::Uuid::parse_str(&rows[0].1).is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_appends_every_item_in_order_under_one_trace_and_one_drain_job(pool: PgPool) {
        let fx = fixture(&pool).await;

        let reply = fx
            .send(
                "/api/v1/ingest/batch",
                json!({"batch": [
                    {"type": "identify", "userId": "u-1", "messageId": "m-1"},
                    {"type": "track", "userId": "u-1", "event": "seen", "messageId": "m-2"},
                    {"type": "track", "userId": "u-1", "event": "seen", "messageId": "m-3"},
                ]}),
            )
            .await;

        assert_eq!(reply.status, StatusCode::ACCEPTED);
        assert_eq!(reply.body, json!({"accepted": 3}));
        let rows = fx.inbox().await;
        let order: Vec<&str> = rows.iter().map(|row| row.1.as_str()).collect();
        assert_eq!(order, ["m-1", "m-2", "m-3"]);
        assert!(rows.iter().all(|row| row.2 == rows[0].2));
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'ingest.drain'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jobs, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_under_the_reserved_segment_prefix_is_refused_and_nothing_is_stored(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let single = fx
            .send(
                "/api/v1/ingest/track",
                json!({"userId": "u-1", "event": "segment.entered"}),
            )
            .await;
        let batch = fx
            .send(
                "/api/v1/ingest/batch",
                json!({"batch": [
                    {"type": "track", "userId": "u-1", "event": "seen"},
                    {"type": "track", "userId": "u-1", "event": "Segment.exited"},
                ]}),
            )
            .await;

        assert_eq!(single.status, StatusCode::BAD_REQUEST, "{:?}", single.body);
        assert!(single.body.to_string().contains("reserved_event_name"));
        assert_eq!(batch.status, StatusCode::BAD_REQUEST, "{:?}", batch.body);
        assert!(batch.body.to_string().contains("reserved_event_name"));
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_missing_or_wrong_key_is_a_401(pool: PgPool) {
        let fx = fixture(&pool).await;
        let body = || Body::from(json!({"userId": "u"}).to_string());

        let anonymous = post(&fx.router, "/api/v1/ingest/identify", None, body()).await;
        let wrong = post(
            &fx.router,
            "/api/v1/ingest/identify",
            Some("Bearer zfk_nonsense"),
            body(),
        )
        .await;

        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_without_the_ingest_scope_is_a_403_and_stores_nothing(pool: PgPool) {
        let fx = fixture_with(
            &pool,
            &[Scope::Read, Scope::Write, Scope::Admin],
            IngestLimits::default(),
        )
        .await;

        let reply = fx
            .send("/api/v1/ingest/track", json!({"userId": "u", "event": "e"}))
            .await;

        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        assert_eq!(reply.body["status"], 403);
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_signed_in_session_is_a_403_even_with_a_csrf_token(pool: PgPool) {
        let fx = fixture(&pool).await;
        admin::create_owner(
            &pool,
            &Context::system(),
            "owner@example.com",
            "correct horse battery",
        )
        .await
        .unwrap();
        let login = fx
            .router
            .clone()
            .oneshot(
                Request::post("/api/v1/session")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"email": "owner@example.com", "password": "correct horse battery"})
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = login.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let bytes = login.into_body().collect().await.unwrap().to_bytes();
        let csrf = serde_json::from_slice::<Value>(&bytes).unwrap()["csrf_token"]
            .as_str()
            .unwrap()
            .to_owned();

        let response = fx
            .router
            .clone()
            .oneshot(
                Request::post("/api/v1/ingest/identify")
                    .header("content-type", "application/json")
                    .header("cookie", cookie)
                    .header("x-csrf-token", csrf)
                    .body(Body::from(json!({"userId": "u"}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_body_over_the_limit_is_a_413_whether_or_not_it_declares_its_length(pool: PgPool) {
        let limits = IngestLimits {
            max_request_bytes: 64,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, &[Scope::Ingest], limits).await;
        let big = json!({"userId": "u", "traits": {"note": "x".repeat(200)}}).to_string();
        let auth = format!("Bearer {}", fx.key);

        let undeclared = post(
            &fx.router,
            "/api/v1/ingest/identify",
            Some(&auth),
            Body::from(big.clone()),
        )
        .await;
        let declared = fx
            .router
            .clone()
            .oneshot(
                Request::post("/api/v1/ingest/identify")
                    .header("authorization", &auth)
                    .header("content-length", big.len())
                    .body(Body::from(big))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(undeclared.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(declared.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invalid_item_is_a_400_listing_index_field_and_reason_and_nothing_is_appended(
        pool: PgPool,
    ) {
        let fx = fixture(&pool).await;

        let reply = fx
            .send(
                "/api/v1/ingest/batch",
                json!({"batch": [
                    {"type": "identify", "userId": "secret-user"},
                    {"type": "track", "userId": "secret-user"},
                    {"type": "identify", "userId": "secret-user", "nickname": "n"},
                ]}),
            )
            .await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            reply.body["errors"],
            json!([
                {"index": 1, "field": "event", "reason": "required"},
                {"index": 2, "field": "nickname", "reason": "unknown_field"},
            ])
        );
        assert!(!reply.body.to_string().contains("secret"));
        assert!(fx.inbox().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn more_items_than_the_limit_is_a_400_with_too_many_items(pool: PgPool) {
        let limits = IngestLimits {
            max_batch_items: 2,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, &[Scope::Ingest], limits).await;
        let item = json!({"type": "identify", "userId": "u"});

        let reply = fx
            .send("/api/v1/ingest/batch", json!({"batch": [item, item, item]}))
            .await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            reply.body["errors"],
            json!([{"index": 0, "field": "batch", "reason": "too_many_items"}])
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn malformed_json_is_a_400_with_fixed_text(pool: PgPool) {
        let fx = fixture(&pool).await;
        let auth = format!("Bearer {}", fx.key);

        let reply = post(
            &fx.router,
            "/api/v1/ingest/identify",
            Some(&auth),
            Body::from("{secret"),
        )
        .await;

        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.body["detail"], "request body is not valid JSON");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_over_its_rate_limit_gets_a_429_with_retry_after_and_stores_nothing_more(
        pool: PgPool,
    ) {
        let limits = IngestLimits {
            rate_per_key: 1,
            burst_per_key: 2,
            max_batch_items: 2,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, &[Scope::Ingest], limits).await;

        let first = fx
            .send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;
        let second = fx
            .send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;
        let third = fx
            .send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;

        assert_eq!(first.status, StatusCode::ACCEPTED);
        assert_eq!(second.status, StatusCode::ACCEPTED);
        assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
        let wait: u64 = third.headers["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=2).contains(&wait), "{wait}");
        assert_eq!(third.body["status"], 429);
        assert_eq!(fx.inbox().await.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rate_limit_is_per_key(pool: PgPool) {
        let limits = IngestLimits {
            rate_per_key: 1,
            burst_per_key: 1,
            ..IngestLimits::default()
        };
        let fx = fixture_with(&pool, &[Scope::Ingest], limits).await;
        let other = admin::create_api_key(
            &pool,
            &Context::system(),
            fx.workspace,
            "other",
            &[Scope::Ingest],
        )
        .await
        .unwrap();
        fx.send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;

        let blocked = fx
            .send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;
        let auth = format!("Bearer {}", other.plaintext);
        let allowed = post(
            &fx.router,
            "/api/v1/ingest/identify",
            Some(&auth),
            Body::from(json!({"userId": "u"}).to_string()),
        )
        .await;

        assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(allowed.status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn an_unreachable_database_is_a_503_not_a_buffered_202() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(300))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let router = router(AppState::new(pool, Arc::new(String::new)));
        let key = crate::credentials::new_key(zirv_flow_core::ApiKeyId::new());

        let reply = post(
            &router,
            "/api/v1/ingest/track",
            Some(&format!("Bearer {}", key.plaintext)),
            Body::from(json!({"userId": "u", "event": "e"}).to_string()),
        )
        .await;

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.body["status"], 503);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn requests_and_items_are_counted_by_endpoint_status_and_kind(pool: PgPool) {
        use metrics_exporter_prometheus::PrometheusBuilder;

        let fx = fixture(&pool).await;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        fx.send("/api/v1/ingest/identify", json!({"userId": "u"}))
            .await;
        fx.send("/api/v1/ingest/track", json!({"userId": "u"}))
            .await;
        fx.send(
            "/api/v1/ingest/batch",
            json!({"batch": [
                {"type": "track", "userId": "u", "event": "e"},
                {"type": "track", "userId": "u", "event": "e"},
            ]}),
        )
        .await;

        let output = handle.render();
        for expected in [
            r#"zirv_flow_ingest_requests_total{endpoint="identify",status="202"} 1"#,
            r#"zirv_flow_ingest_requests_total{endpoint="track",status="400"} 1"#,
            r#"zirv_flow_ingest_requests_total{endpoint="batch",status="202"} 1"#,
            r#"zirv_flow_ingest_items_total{kind="identify"} 1"#,
            r#"zirv_flow_ingest_items_total{kind="track"} 2"#,
        ] {
            assert!(output.contains(expected), "{expected} in {output}");
        }
    }
}
