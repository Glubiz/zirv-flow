//! `webhook.call`: one signed HTTP call of an `action.webhook` node (architecture 9.1 and 9.3,
//! rulings 9cb0bea9 and a37fbac4).
//!
//! `flow.step` enqueues the job in the step's transaction under the effect key, so the call is
//! queued exactly when the step commits and at most once per step. The job reads the person's
//! listed traits as they are when it runs, signs the exact body bytes and sends them through the
//! guarded client, so the egress policy is enforced where the connection is made.
//!
//! An attempt ends in one of three ways:
//!
//! - **A 2xx response succeeds.**
//! - **A 408, a 429, a 5xx, a timeout or a transport error is retried** with the worker's
//!   backoff, or after the `Retry-After` the destination named when that is longer. The job's
//!   last attempt records a failure instead of dying with the enrollment never told.
//! - **Anything else fails at once:** any other 4xx (and any unexpected 1xx or 3xx), a destination
//!   the egress policy refuses, and a workspace with no signing secret. Retrying cannot help.
//!
//! Succeeding and failing both end the job as done and write an outcome event
//! (`webhook.succeeded` or `webhook.failed`) with a decision, so a flow can wait on either. The
//! response body is never read, logged or stored; only the status code is.
//!
//! The call is at-least-once: a lease lost between the response and the commit sends it again
//! with the same idempotency key, which is the receiver's cue to treat it as a repeat.

use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::http::{GuardedClient, HttpError, HttpLimits};
use zirv_flow_core::flow::{
    Envelope, IDEMPOTENCY_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER, signature_value,
};
use zirv_flow_core::{Keyring, SecretPurpose, TraceId, WorkspaceId};
use zirv_flow_store::{
    SigningKey, WEBHOOK_JOB_KIND, WebhookOutcome, WebhookResult, WebhookSubject, live_signing_keys,
    lock_webhook_subject, record_webhook_outcome, secrets,
};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct WebhookCall;

/// The content type of the body.
const CONTENT_TYPE: &str = "application/json";

/// The payload `flow.step` enqueues.
struct Call {
    workspace: WorkspaceId,
    flow: Uuid,
    version: i32,
    node: String,
    enrollment: Uuid,
    trace: TraceId,
    idempotency_key: String,
    occurred_at: String,
    url: String,
    trait_keys: Vec<String>,
}

impl Call {
    fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let id = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            flow: id("flow_id")?,
            version: i32::try_from(payload.get("version")?.as_i64()?).ok()?,
            node: text("node_id")?.to_owned(),
            enrollment: id("enrollment_id")?,
            trace: text("trace_id")?.parse().ok()?,
            idempotency_key: text("idempotency_key")?.to_owned(),
            occurred_at: text("occurred_at")?.to_owned(),
            url: text("url")?.to_owned(),
            trait_keys: payload
                .get("trait_keys")?
                .as_array()?
                .iter()
                .map(|key| key.as_str().map(str::to_owned))
                .collect::<Option<_>>()?,
        })
    }
}

/// How an attempt went.
#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    /// A 2xx response.
    Succeeded { status: u16 },
    /// Retrying cannot help.
    Terminal {
        status: Option<u16>,
        reason: &'static str,
    },
    /// Worth another attempt, after at least `retry_after` when the destination named one.
    Retry {
        status: Option<u16>,
        reason: &'static str,
        message: &'static str,
        retry_after: Option<Duration>,
    },
}

/// What a response status means. `retry_after` is the destination's `Retry-After`, used only by
/// a status that is retried.
fn judge(status: u16, retry_after: Option<Duration>) -> Verdict {
    let retry = |reason, message| Verdict::Retry {
        status: Some(status),
        reason,
        message,
        retry_after,
    };
    match status {
        200..=299 => Verdict::Succeeded { status },
        408 => retry("http_408", "the destination answered 408 request timeout"),
        429 => retry("http_429", "the destination answered 429 too many requests"),
        500..=599 => retry("http_5xx", "the destination answered with a server error"),
        400..=499 => Verdict::Terminal {
            status: Some(status),
            reason: "http_4xx",
        },
        _ => Verdict::Terminal {
            status: Some(status),
            reason: "http_status",
        },
    }
}

/// What a failed send means.
fn judge_error(error: &HttpError) -> Verdict {
    match error {
        HttpError::EgressDenied { .. } => Verdict::Terminal {
            status: None,
            reason: "egress_denied",
        },
        HttpError::Timeout => Verdict::Retry {
            status: None,
            reason: "timeout",
            message: "the request to the destination timed out",
            retry_after: None,
        },
        HttpError::BodyTooLarge | HttpError::Transport(_) => Verdict::Retry {
            status: None,
            reason: "transport",
            message: "the request to the destination failed",
            retry_after: None,
        },
    }
}

/// The wait a `Retry-After` header asks for: delta-seconds, or an HTTP date counted from `now`.
/// A date in the past, or text that is neither, asks for nothing.
fn parse_retry_after(value: &str, now: OffsetDateTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = OffsetDateTime::parse(value, &Rfc2822).ok()?;
    let wait = at - now;
    u64::try_from(wait.whole_seconds())
        .ok()
        .map(Duration::from_secs)
}

impl JobKind for WebhookCall {
    const KIND: &'static str = WEBHOOK_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let call = Call::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a webhook call"))?;
        if ctx.workspace_id() != Some(call.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let (person, traits) = match lock_webhook_subject(
            ctx.conn(),
            call.workspace,
            call.enrollment,
            &call.trait_keys,
        )
        .await?
        {
            WebhookSubject::Present { person, traits } => (person, traits),
            // Erased with the person: there is nobody to call about, and nothing to retry.
            WebhookSubject::Gone => return Ok(()),
            WebhookSubject::Moved => {
                return Err(JobError::new("the enrollment moved to another person"));
            }
        };

        let envelope = Envelope {
            flow_id: call.flow,
            version: call.version,
            node_id: &call.node,
            enrollment_id: call.enrollment,
            person_id: person,
            trace_id: call.trace,
            idempotency_key: &call.idempotency_key,
            occurred_at: &call.occurred_at,
            traits,
        };
        let verdict = deliver(ctx, &call, &envelope.body(), now).await?;

        let attempts = ctx.attempts();
        let last_attempt = attempts >= ctx.max_attempts();
        let (result, status, reason) = match verdict {
            Verdict::Succeeded { status } => (WebhookResult::Succeeded, Some(status), None),
            Verdict::Terminal { status, reason } => (WebhookResult::Failed, status, Some(reason)),
            Verdict::Retry {
                status,
                reason,
                message,
                retry_after,
            } => {
                warn!(
                    status,
                    attempt = attempts,
                    reason,
                    "webhook call will be retried"
                );
                if !last_attempt {
                    let error = JobError::new(message);
                    return Err(retry_after.map_or(error, |wait| error.with_retry_after(wait)));
                }
                (WebhookResult::Failed, status, Some(reason))
            }
        };
        if result == WebhookResult::Failed {
            warn!(status, attempt = attempts, reason, "webhook call failed");
        }
        let trace = ctx.trace().clone();
        let job = (ctx.job_id(), attempts);
        record_webhook_outcome(
            ctx.conn(),
            &WebhookOutcome {
                workspace: call.workspace,
                enrollment: call.enrollment,
                person,
                node: &call.node,
                result,
                status,
                attempts,
                reason,
                trace: &trace,
                now,
                job,
            },
        )
        .await?;
        Ok(())
    }
}

/// Signs `body` with every secret that signs now and sends it. An error is a failure of this
/// worker or of the database, not of the destination; the destination's answer is a [`Verdict`].
async fn deliver(
    ctx: &mut JobContext<'_>,
    call: &Call,
    body: &[u8],
    now: OffsetDateTime,
) -> Result<Verdict, JobError> {
    let Some(keys) = live_signing_keys(ctx.conn(), call.workspace, now).await? else {
        return Ok(Verdict::Terminal {
            status: None,
            reason: "no_signing_secret",
        });
    };
    let keyring = ctx
        .keyring()
        .cloned()
        .ok_or_else(|| JobError::new("the worker has no master key to open the signing secret"))?;
    let timestamp = now.unix_timestamp();
    let mut signatures = Vec::with_capacity(2);
    // The current secret must open; a previous one that does not is skipped, since the receiver
    // that still trusts it also trusts the current one the call is signed with.
    for (index, key) in [Some(keys.current), keys.previous]
        .into_iter()
        .flatten()
        .enumerate()
    {
        match open_signature(ctx, &keyring, call.workspace, key, timestamp, body).await {
            Ok(signature) => signatures.push(signature),
            Err(error) if index == 0 => return Err(error),
            Err(_) => {}
        }
    }

    let timestamp = timestamp.to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Content-Type", CONTENT_TYPE),
        (TIMESTAMP_HEADER, &timestamp),
        (IDEMPOTENCY_HEADER, &call.idempotency_key),
    ];
    headers.extend(
        signatures
            .iter()
            .map(|signature| (SIGNATURE_HEADER, signature.as_str())),
    );
    let client = GuardedClient::new(ctx.egress().clone(), HttpLimits::default())
        .map_err(|_| JobError::new("the outbound client could not be built"))?;
    match client.post(&call.url, &headers, body.to_vec()).await {
        Ok(response) => {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_retry_after(value, now));
            // The body is dropped unread.
            Ok(judge(response.status().as_u16(), retry_after))
        }
        Err(error) => Ok(judge_error(&error)),
    }
}

/// Opens one signing secret and signs `body` with it.
async fn open_signature(
    ctx: &mut JobContext<'_>,
    keyring: &Keyring,
    workspace: WorkspaceId,
    key: SigningKey,
    timestamp: i64,
    body: &[u8],
) -> Result<String, JobError> {
    let secret = secrets::open_json(
        ctx.conn(),
        keyring,
        workspace,
        key.secret_id,
        SecretPurpose::WebhookSigning,
    )
    .await
    .map_err(|_| JobError::new("the signing secret could not be opened"))?;
    Ok(signature_value(secret.expose(), timestamp, body))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as Span;
    use time::macros::datetime;
    use tokio::sync::watch;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::flow::SIGNING_GRACE_SECONDS;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::signature::{Scheme, generate_secret, verify};
    use zirv_flow_core::{
        Cause, MasterKey, SecretId, Slug, TraceContext, WorkspaceName, seal, secret_aad,
    };
    use zirv_flow_store::{
        ClaimedJob, Clock, EnrollRequest, Enrolled, ManualClock, NewFlow, NewJob,
        NewTraitDefinition, Occurrence, ProfileRecord, create_signing_key, rotate_signing_key,
    };

    use super::*;
    use crate::{Registry, Worker, WorkerConfig};

    const SECRET_BODY: &str = "RESPONSE-BODY-MUST-NOT-LAND-ANYWHERE";

    #[test]
    fn the_kind_is_webhook_call_and_it_is_registered() {
        assert_eq!(WebhookCall::KIND, "webhook.call");
        assert!(Registry::built_in().get("webhook.call").is_some());
    }

    #[test]
    fn a_status_is_judged_by_the_retry_policy() {
        let ask = Some(Duration::from_secs(3));
        assert_eq!(judge(200, None), Verdict::Succeeded { status: 200 });
        assert_eq!(judge(204, None), Verdict::Succeeded { status: 204 });
        for status in [408, 429, 500, 502, 503, 599] {
            assert!(
                matches!(judge(status, ask), Verdict::Retry { status: Some(s), retry_after, .. }
                    if s == status && retry_after == ask),
                "{status}"
            );
        }
        for status in [400, 401, 403, 404, 410, 422, 451] {
            assert_eq!(
                judge(status, ask),
                Verdict::Terminal {
                    status: Some(status),
                    reason: "http_4xx"
                },
                "{status}"
            );
        }
        for status in [100, 301, 302, 307, 600] {
            assert_eq!(
                judge(status, ask),
                Verdict::Terminal {
                    status: Some(status),
                    reason: "http_status"
                },
                "{status}"
            );
        }
    }

    #[test]
    fn a_send_error_is_terminal_when_the_egress_policy_refused_it_and_retried_otherwise() {
        let denied = HttpError::EgressDenied {
            host: "10.0.0.5".into(),
            class: zirv_flow_core::egress::AddressClass::Private,
        };
        assert_eq!(
            judge_error(&denied),
            Verdict::Terminal {
                status: None,
                reason: "egress_denied"
            }
        );
        for error in [
            HttpError::Timeout,
            HttpError::Transport("could not connect".into()),
        ] {
            assert!(matches!(
                judge_error(&error),
                Verdict::Retry { status: None, .. }
            ));
        }
    }

    #[test]
    fn a_retry_after_is_read_as_seconds_or_as_an_http_date_and_otherwise_ignored() {
        let now = datetime!(1994-11-06 08:49:00 UTC);

        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(Duration::from_secs(37))
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:40:00 GMT", now),
            None,
            "a date in the past asks for nothing"
        );
        for text in ["", "soon", "-5", "1.5", "12abc"] {
            assert_eq!(parse_retry_after(text, now), None, "{text:?}");
        }
    }

    #[test]
    fn a_payload_with_a_missing_or_mistyped_field_is_not_a_call() {
        let whole = json!({
            "workspace_id": Uuid::from_u128(1), "flow_id": Uuid::from_u128(2), "version": 1,
            "node_id": "hook", "enrollment_id": Uuid::from_u128(3), "person_id": Uuid::from_u128(4),
            "trace_id": "00000000000000000000000000000005", "idempotency_key": "k", "occurred_at": "t",
            "url": "https://example.test/", "trait_keys": ["plan"]
        });
        assert!(Call::parse(&whole).is_some());
        for field in [
            "workspace_id",
            "flow_id",
            "version",
            "node_id",
            "enrollment_id",
            "trace_id",
            "idempotency_key",
            "occurred_at",
            "url",
            "trait_keys",
        ] {
            let mut broken = whole.clone();
            broken.as_object_mut().unwrap().remove(field);
            assert!(Call::parse(&broken).is_none(), "{field}");
        }
        let mut broken = whole.clone();
        broken["trait_keys"] = json!(["plan", 7]);
        assert!(Call::parse(&broken).is_none());
    }

    /// One request the mock destination received.
    #[derive(Clone)]
    struct Hit {
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Hit {
        fn header(&self, name: &str) -> Option<&str> {
            self.all(name).first().copied()
        }

        fn all(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
                .collect()
        }

        /// Whether one of the signature headers verifies with `secret` over the bytes received,
        /// by the same scheme an inbound hook uses.
        fn verifies_with(&self, secret: &str, now: i64) -> bool {
            let scheme = Scheme {
                signature_header: SIGNATURE_HEADER,
                signature_prefix: "sha256=",
                timestamp_header: Some(TIMESTAMP_HEADER),
                tolerance_seconds: 300,
            };
            self.all(SIGNATURE_HEADER).into_iter().any(|signature| {
                let lookup = |name: &str| {
                    if name.eq_ignore_ascii_case(SIGNATURE_HEADER) {
                        Some(signature)
                    } else {
                        self.header(name)
                    }
                };
                verify(&scheme, secret.as_bytes(), &lookup, &self.body, now).is_ok()
            })
        }
    }

    /// What the mock answers next.
    #[derive(Clone)]
    struct Answer {
        status: u16,
        retry_after: Option<&'static str>,
    }

    /// A destination that records what it receives and answers from a script, then 200.
    #[derive(Clone, Default)]
    struct Mock {
        hits: Arc<Mutex<Vec<Hit>>>,
        script: Arc<Mutex<VecDeque<Answer>>>,
    }

    impl Mock {
        fn hits(&self) -> Vec<Hit> {
            self.hits.lock().unwrap().clone()
        }

        fn answer(&self, status: u16, retry_after: Option<&'static str>) {
            self.script.lock().unwrap().push_back(Answer {
                status,
                retry_after,
            });
        }
    }

    async fn receive(State(mock): State<Mock>, headers: HeaderMap, body: Bytes) -> Response {
        mock.hits.lock().unwrap().push(Hit {
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect(),
            body: body.to_vec(),
        });
        let answer = mock.script.lock().unwrap().pop_front().unwrap_or(Answer {
            status: 200,
            retry_after: None,
        });
        let mut response =
            (StatusCode::from_u16(answer.status).unwrap(), SECRET_BODY).into_response();
        if let Some(after) = answer.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static(after));
        }
        response
    }

    async fn serve(mock: &Mock) -> u16 {
        let router = Router::new()
            .route("/hook", post(receive))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }

    fn loopback() -> Arc<EgressPolicy> {
        Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap())
    }

    /// One workspace with a signing secret, a destination, a person and an enrollment, and a
    /// worker that runs `webhook.call` jobs by hand.
    struct Env {
        pool: PgPool,
        ws: zirv_flow_core::WorkspaceId,
        clock: ManualClock,
        keyring: Arc<Keyring>,
        egress: Arc<EgressPolicy>,
        mock: Mock,
        port: u16,
        /// The plaintext of each signing secret made, oldest first.
        secrets: Vec<String>,
    }

    impl Env {
        async fn new(pool: PgPool) -> Self {
            Self::with_egress(pool, loopback()).await
        }

        async fn with_egress(pool: PgPool, egress: Arc<EgressPolicy>) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let name = WorkspaceName::parse("ours").unwrap();
            let slug = Slug::parse("ours").unwrap();
            let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
                .await
                .unwrap()
                .id;
            let mock = Mock::default();
            let port = serve(&mock).await;
            let env = Self {
                pool,
                ws,
                clock: ManualClock::new(datetime!(2031-05-06 07:08:09 UTC)),
                keyring: Arc::new(Keyring::new(MasterKey::generate(), None).unwrap()),
                egress,
                mock,
                port,
                secrets: Vec::new(),
            };
            env.define_trait("plan").await;
            env.define_trait("path").await;
            env
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/hook", self.port)
        }

        async fn define_trait(&self, key: &str) {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &NewTraitDefinition {
                    key: key.into(),
                    trait_type: TraitType::String,
                    precedence: Precedence::Latest,
                    source_order: Vec::new(),
                    pii: false,
                    channel_address: false,
                    inferred: false,
                    description: String::new(),
                },
            )
            .await
            .unwrap();
        }

        /// Seals a fresh secret and returns its id and plaintext.
        async fn seal_secret(&mut self) -> SecretId {
            let secret = generate_secret();
            let id = SecretId::new();
            let sealed = seal(
                &self.keyring,
                secret.expose().as_bytes(),
                &secret_aad(self.ws, id, SecretPurpose::WebhookSigning),
            );
            zirv_flow_store::secrets::insert_unchecked(
                &self.pool,
                self.ws,
                SecretPurpose::WebhookSigning,
                id,
                &sealed,
            )
            .await
            .unwrap();
            self.secrets.push(secret.expose().to_owned());
            id
        }

        /// The workspace's first signing secret.
        async fn sign_with_a_secret(&mut self) {
            let id = self.seal_secret().await;
            let mut connection = self.pool.acquire().await.unwrap();
            assert!(
                create_signing_key(&mut connection, self.ws, id, self.clock.now())
                    .await
                    .unwrap()
            );
        }

        /// What the API does on a rotation.
        async fn rotate(&mut self) {
            let id = self.seal_secret().await;
            let mut connection = self.pool.acquire().await.unwrap();
            rotate_signing_key(&mut connection, self.ws, id, self.clock.now())
                .await
                .unwrap()
                .unwrap();
        }

        async fn person(&self, email: &str, traits: Value) -> Uuid {
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: email.into(),
                identifiers: vec![("email".into(), email.into())],
                traits: traits.as_object().cloned().unwrap(),
                observed_at: self.clock.now() - Span::days(1),
                trace: TraceContext::root(Cause::api("t")),
            };
            match &zirv_flow_store::apply_profile_batch(&self.pool, self.ws, &[record])
                .await
                .unwrap()[0]
            {
                zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
                | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => *person_id,
                zirv_flow_store::ProfileOutcome::Rejected { reason } => {
                    panic!("rejected: {reason}")
                }
            }
        }

        /// Publishes a flow with `graph` and enrolls `person` in it.
        async fn enroll(&self, graph: Value, person: Uuid) -> Enrolled {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id,
                    name: format!("Hook {id}"),
                    graph,
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, id)
                .await
                .unwrap();
            let (_, version) = zirv_flow_store::publish_flow(
                &mut tx,
                &Clock::default(),
                self.ws,
                &locked,
                None,
                &TraceContext::root(Cause::api("publish")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let outcome = zirv_flow_store::enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: self.ws,
                    flow: id,
                    version: version.version,
                    people: &[person],
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: self.clock.now(),
                    trace: &TraceContext::root(Cause::api("enroll")),
                },
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            outcome.enrolled[0].clone()
        }

        /// A person with a plan, enrolled in a flow whose webhook node sends `plan` and `age`.
        async fn enrolled(&self) -> (Uuid, Enrolled) {
            let person = self.person("ada@example.com", json!({"plan": "pro"})).await;
            let graph = hook_graph(&self.url(), json!(["plan", "age"]));
            (person, self.enroll(graph, person).await)
        }

        /// Queues the job `flow.step` would.
        async fn queue(&self, enrolled: &Enrolled, person: Uuid, max_attempts: i32) {
            let key = format!("{}:hook:webhook", enrolled.enrollment);
            let mut job = NewJob::new(WEBHOOK_JOB_KIND, enrolled.trace.clone());
            job.workspace_id = Some(self.ws);
            job.unique_key = Some(key.clone());
            job.max_attempts = max_attempts;
            job.payload = json!({
                "workspace_id": self.ws,
                "flow_id": Uuid::from_u128(9),
                "version": 1,
                "node_id": "hook",
                "enrollment_id": enrolled.enrollment,
                "person_id": person,
                "trace_id": enrolled.trace.trace_id,
                "idempotency_key": key,
                "occurred_at": "2031-05-06T07:08:09Z",
                "url": self.url(),
                "trait_keys": ["plan", "age"],
            });
            zirv_flow_store::enqueue(&self.pool, &job).await.unwrap();
        }

        async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[WEBHOOK_JOB_KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
        }

        /// Runs one claimed job as a worker does: its work commits with its completion, and an
        /// error rolls it back and puts the job up for another attempt at once.
        async fn execute(&self, claimed: ClaimedJob) -> Result<(), JobError> {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let result = {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_clock(Clock::Manual(self.clock.clone()))
                    .with_outbound(
                        Some(self.keyring.clone()),
                        self.egress.clone(),
                        Arc::default(),
                    );
                WebhookCall.run(&mut ctx, claimed.payload.clone()).await
            };
            match result {
                Ok(()) => {
                    zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                        .await
                        .unwrap();
                    zirv_flow_store::commit(tx).await.unwrap();
                    Ok(())
                }
                Err(error) => {
                    drop(tx);
                    zirv_flow_store::fail_job(
                        &self.pool,
                        claimed.id,
                        claimed.lease_token,
                        Duration::ZERO,
                        error.message(),
                    )
                    .await
                    .unwrap();
                    Err(error)
                }
            }
        }

        /// Claims the job and runs one attempt of it.
        async fn attempt(&self) -> Result<(), JobError> {
            let claimed = self.claim().await.expect("a webhook.call job is ready");
            self.execute(claimed).await
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn jobs_left(&self) -> i64 {
            self.count("SELECT count(*) FROM jobs WHERE kind = 'webhook.call'")
                .await
        }

        /// The events of a name: their properties.
        async fn events(&self, name: &str) -> Vec<Value> {
            sqlx::query_scalar(
                "SELECT properties FROM events WHERE workspace_id = $1 AND name = $2",
            )
            .bind(self.ws.as_uuid())
            .bind(name)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// The (outcome, reason) of every `webhook.call` decision on the trace.
        async fn decisions(&self, trace: &TraceContext) -> Vec<(String, Option<String>)> {
            sqlx::query_as(
                "SELECT outcome, reason FROM decision_records \
                 WHERE workspace_id = $1 AND trace_id = $2 AND action = 'webhook.call'",
            )
            .bind(self.ws.as_uuid())
            .bind(trace.trace_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }
    }

    fn edge(id: &str, from: &str, port: &str, to: &str) -> Value {
        json!({"id": id, "from": from, "port": port, "to": to})
    }

    fn hook_graph(url: &str, trait_keys: Value) -> Value {
        json!({"v": 1, "nodes": [
            {"id": "hook", "type": "action.webhook", "url": url, "trait_keys": trait_keys},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "hook", "next", "end")]})
    }

    fn now_seconds(env: &Env) -> i64 {
        env.clock.now().unix_timestamp()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_call_is_signed_over_the_exact_bytes_with_the_fixed_envelope_and_headers(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;

        env.attempt().await.unwrap();

        let hits = env.mock.hits();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        let key = format!("{}:hook:webhook", enrolled.enrollment);
        assert!(hit.verifies_with(&env.secrets[0], now_seconds(&env)));
        assert!(!hit.verifies_with("zfwh_other", now_seconds(&env)));
        assert_eq!(hit.header("x-zirv-idempotency-key"), Some(key.as_str()));
        assert_eq!(
            hit.header("x-zirv-timestamp"),
            Some(now_seconds(&env).to_string().as_str())
        );
        assert_eq!(hit.header("content-type"), Some("application/json"));
        assert!(
            hit.header("x-zirv-signature")
                .unwrap()
                .starts_with("sha256=")
        );
        let body: Value = serde_json::from_slice(&hit.body).unwrap();
        assert_eq!(
            body,
            json!({
                "flow_id": Uuid::from_u128(9),
                "version": 1,
                "node_id": "hook",
                "enrollment_id": enrolled.enrollment,
                "person_id": person,
                "trace_id": enrolled.trace.trace_id,
                "idempotency_key": key,
                "occurred_at": "2031-05-06T07:08:09Z",
                "traits": {"plan": "pro", "age": null},
            })
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_503_is_retried_with_the_same_body_and_key_and_the_outcome_counts_the_attempts(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        env.mock.answer(503, None);

        let first = env.attempt().await;
        let after_first = (
            env.events("webhook.succeeded").await.len(),
            env.events("webhook.failed").await.len(),
        );
        let second = env.attempt().await;

        let error = first.unwrap_err();
        assert_eq!(
            error.message(),
            "the destination answered with a server error"
        );
        assert_eq!(error.retry_after(), None);
        assert_eq!(after_first, (0, 0), "a retried attempt writes no outcome");
        second.unwrap();
        let hits = env.mock.hits();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].body, hits[1].body);
        assert_eq!(
            hits[0].header("x-zirv-idempotency-key"),
            hits[1].header("x-zirv-idempotency-key")
        );
        let succeeded = env.events("webhook.succeeded").await;
        assert_eq!(
            succeeded,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": 200, "attempts": 2})
            ]
        );
        assert!(env.events("webhook.failed").await.is_empty());
        assert_eq!(env.jobs_left().await, 0, "the job is done");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_429_asks_the_worker_to_wait_for_the_retry_after(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        env.mock.answer(429, Some("17"));
        env.mock.answer(408, Some("Sun, 06 Nov 1994 08:49:37 GMT"));

        let limited = env.attempt().await.unwrap_err();
        let timed_out = env.attempt().await.unwrap_err();

        assert_eq!(
            limited.message(),
            "the destination answered 429 too many requests"
        );
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(17)));
        assert_eq!(
            timed_out.message(),
            "the destination answered 408 request timeout"
        );
        assert_eq!(
            timed_out.retry_after(),
            None,
            "a date in the past asks for nothing"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_404_is_terminal_the_job_is_done_and_the_failure_is_an_event_and_a_decision(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        env.mock.answer(404, None);

        env.attempt().await.unwrap();

        assert_eq!(env.mock.hits().len(), 1, "a terminal answer is not retried");
        assert_eq!(env.jobs_left().await, 0);
        assert_eq!(
            env.events("webhook.failed").await,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": 404, "attempts": 1})
            ]
        );
        assert!(env.events("webhook.succeeded").await.is_empty());
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [("failed".to_owned(), Some("http_4xx".to_owned()))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_last_attempt_of_a_retryable_failure_records_the_failure_and_ends_done(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 2).await;
        env.mock.answer(500, None);
        env.mock.answer(502, None);

        let first = env.attempt().await;
        let last = env.attempt().await;

        assert!(first.is_err());
        last.unwrap();
        assert_eq!(env.jobs_left().await, 0, "done, neither retried nor dead");
        assert_eq!(
            env.events("webhook.failed").await,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": 502, "attempts": 2})
            ]
        );
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [("failed".to_owned(), Some("http_5xx".to_owned()))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_private_address_is_refused_by_the_egress_policy_and_nothing_is_sent(pool: PgPool) {
        let mut env = Env::with_egress(pool, Arc::default()).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;

        env.attempt().await.unwrap();

        assert!(
            env.mock.hits().is_empty(),
            "the destination was never contacted"
        );
        assert_eq!(
            env.events("webhook.failed").await,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": null, "attempts": 1})
            ]
        );
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [("failed".to_owned(), Some("egress_denied".to_owned()))]
        );
        assert_eq!(env.jobs_left().await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_connection_that_fails_is_retried(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        // Nothing listens on the discard port of the loopback.
        sqlx::query(
            "UPDATE jobs SET payload = jsonb_set(payload, '{url}', '\"http://127.0.0.1:9/hook\"')",
        )
        .execute(&env.pool)
        .await
        .unwrap();

        let error = env.attempt().await.unwrap_err();

        assert_eq!(error.message(), "the request to the destination failed");
        assert!(env.events("webhook.failed").await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_previous_secret_signs_too_inside_the_grace_window_and_not_after(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        env.rotate().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        env.clock.advance(Span::seconds(SIGNING_GRACE_SECONDS - 1));

        env.attempt().await.unwrap();

        let inside = env.mock.hits().remove(0);
        let now = now_seconds(&env);
        assert_eq!(inside.all(SIGNATURE_HEADER).len(), 2);
        assert!(inside.verifies_with(&env.secrets[1], now), "the new secret");
        assert!(inside.verifies_with(&env.secrets[0], now), "the old secret");

        // The window closes; a new call (another key) is signed with the new secret alone.
        env.clock.advance(Span::seconds(1));
        let other = env.person("bob@example.com", json!({"plan": "free"})).await;
        let graph = hook_graph(&env.url(), json!(["plan"]));
        let second = env.enroll(graph, other).await;
        env.queue(&second, other, 5).await;
        env.attempt().await.unwrap();

        let after = env.mock.hits().remove(1);
        let now = now_seconds(&env);
        assert_eq!(after.all(SIGNATURE_HEADER).len(), 1);
        assert!(after.verifies_with(&env.secrets[1], now));
        assert!(!after.verifies_with(&env.secrets[0], now));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_without_a_signing_secret_fails_the_call_without_sending_it(pool: PgPool) {
        let env = Env::new(pool).await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;

        env.attempt().await.unwrap();

        assert!(env.mock.hits().is_empty());
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [("failed".to_owned(), Some("no_signing_secret".to_owned()))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_response_body_is_never_stored(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        env.mock.answer(500, None);
        env.attempt().await.unwrap_err();
        env.attempt().await.unwrap();
        env.mock.answer(404, None);
        let other = env.person("bob@example.com", json!({})).await;
        let second = env.enroll(hook_graph(&env.url(), json!([])), other).await;
        env.queue(&second, other, 5).await;
        env.attempt().await.unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = current_schema()",
        )
        .fetch_all(&env.pool)
        .await
        .unwrap();
        for table in tables {
            let text: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT string_agg(t::text, E'\\n') FROM \"{table}\" t"
            )))
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert!(
                !text.unwrap_or_default().contains(SECRET_BODY),
                "the response body is in {table}"
            );
        }
        assert_eq!(env.mock.hits().len(), 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_whose_person_was_erased_while_it_was_leased_makes_no_call_and_ends_done(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        let leased = env.claim().await.expect("the job is leased");

        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        assert!(
            zirv_flow_store::delete_person(
                &mut tx,
                env.ws,
                person,
                zirv_flow_store::DeleteReason::Erasure,
                &TraceContext::root(Cause::api("erase")),
            )
            .await
            .unwrap()
        );
        zirv_flow_store::commit(tx).await.unwrap();
        assert_eq!(
            env.jobs_left().await,
            1,
            "the erasure leaves the leased job to run"
        );

        env.execute(leased).await.unwrap();

        assert!(env.mock.hits().is_empty(), "no call was made");
        assert_eq!(env.jobs_left().await, 0, "the job ended as done");
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE state = 'dead'")
                .await,
            0
        );
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'webhook.%'")
                .await,
            0
        );
        assert!(env.decisions(&enrolled.trace).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_for_another_workspace_is_refused(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        sqlx::query(
            "UPDATE jobs SET payload = jsonb_set(payload, '{workspace_id}', to_jsonb($1::text))",
        )
        .bind(Uuid::now_v7().to_string())
        .execute(&env.pool)
        .await
        .unwrap();

        let error = env.attempt().await.unwrap_err();

        assert_eq!(
            error.message(),
            "the payload names another workspace than the job"
        );
        assert!(env.mock.hits().is_empty());
    }

    /// A published flow's webhook node delivers a signed call, and its outcome event resumes
    /// the enrollment that waits for it, all through the real worker.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_flow_delivers_a_verifiable_call_and_its_outcome_resumes_the_wait(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let person = env.person("ada@example.com", json!({"plan": "pro"})).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "hook", "type": "action.webhook", "url": env.url(), "trait_keys": ["plan"]},
            {"id": "wait", "type": "wait.until_event", "event": "webhook.succeeded",
             "timeout_seconds": 3600},
            {"id": "by_event", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "delivered"}},
            {"id": "by_timeout", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "timeout"}},
            {"id": "end_event", "type": "exit"},
            {"id": "end_timeout", "type": "exit"}
        ], "edges": [
            edge("e1", "hook", "next", "wait"),
            edge("e2", "wait", "event", "by_event"),
            edge("e3", "wait", "timeout", "by_timeout"),
            edge("e4", "by_event", "next", "end_event"),
            edge("e5", "by_timeout", "next", "end_timeout")
        ]});
        let enrolled = env.enroll(graph, person).await;
        let mut config = WorkerConfig::new(2);
        config.poll_interval = Duration::from_millis(20);
        config.keyring = Some(env.keyring.clone());
        config.egress = env.egress.clone();
        let (stop, stopped) = watch::channel(false);
        let handle =
            tokio::spawn(Worker::new(env.pool.clone(), Registry::built_in(), config).run(stopped));

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status: String = sqlx::query_scalar("SELECT state FROM enrollments WHERE id = $1")
                .bind(enrolled.enrollment)
                .fetch_one(&env.pool)
                .await
                .unwrap();
            if status == "completed" {
                break;
            }
            assert!(Instant::now() < deadline, "the enrollment is {status}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.send(true).unwrap();
        handle.await.unwrap();

        let hits = env.mock.hits();
        assert_eq!(hits.len(), 1);
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(hits[0].verifies_with(&env.secrets[0], now));
        let body: Value = serde_json::from_slice(&hits[0].body).unwrap();
        assert_eq!(body["traits"], json!({"plan": "pro"}));
        assert_eq!(body["enrollment_id"], json!(enrolled.enrollment));
        let traits: Value = sqlx::query_scalar("SELECT traits FROM people WHERE id = $1")
            .bind(person)
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(
            traits["path"], "delivered",
            "the event path ran, not the timeout"
        );
        assert_eq!(env.events("webhook.succeeded").await.len(), 1);
    }
}
