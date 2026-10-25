//! `webhook.call`: one signed HTTP call of an `action.webhook` node (architecture 9.1, 9.3 and
//! 10.1, rulings 9cb0bea9 and a37fbac4).
//!
//! `flow.step` enqueues the job in the step's transaction under the effect key, so the call is
//! queued exactly when the step commits and at most once per step. The job reads the person's
//! listed traits as they are when it runs and delivers through the built-in `webhook`
//! destination, which signs the exact body bytes and sends them through the guarded client, so
//! the egress policy is enforced where the connection is made. The verdict of the answer comes
//! from the delivery code every outbound job shares (`crate::delivery`).
//!
//! The body is the fixed envelope, byte for byte as before, or, when the node names a `template`,
//! the headers and body a published webhook template renders for the person. A templated payload
//! is rendered by the render host pool, never in this process; a template that is gone or
//! unpublished (`template_unavailable`) or a render that fails (`render_error`) ends the call for
//! good.
//!
//! An attempt ends in one of three ways:
//!
//! - **A 2xx response succeeds.**
//! - **A 408, a 429, a 5xx, a timeout or a transport error is retried** with the worker's
//!   backoff, or after the `Retry-After` the destination named when that is longer. The job's
//!   last attempt records a failure instead of dying with the enrollment never told.
//! - **Anything else fails at once:** any other 4xx (and any unexpected 1xx or 3xx; a redirect is
//!   never followed), a destination the egress policy refuses, a workspace with no signing
//!   secret, and a template that cannot be rendered. Retrying cannot help.
//!
//! Succeeding and failing both end the job as done and write an outcome event
//! (`webhook.succeeded` or `webhook.failed`) with a decision, so a flow can wait on either. The
//! response body is never read, logged or stored; only the status code is. The rendered body is
//! not stored either.
//!
//! The person is read without a lock before the call and locked `FOR KEY SHARE` only for the
//! outcome write, which is skipped when the enrollment was erased in between. A signing secret the
//! worker cannot open is retried like a transport error and recorded as `webhook.failed`
//! (`signing_secret_unavailable`) on the last attempt.
//!
//! A raw URL has no destination row, so the attempt is judged but writes no `delivery_attempts`
//! row and takes no rate-limit token (architecture 10.1).
//!
//! The call is at-least-once: a lease lost between the response and the commit sends it again
//! with the same idempotency key, which is the receiver's cue to treat it as a repeat.

use std::time::Duration;

use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::{DestinationContext, Outbound, Registry};
use zirv_flow_connectors::http::HttpLimits;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::flow::Envelope;
use zirv_flow_core::{Keyring, SecretPurpose, SecretString, TraceId, WorkspaceId};
use zirv_flow_render::{RenderContext, TemplateDocument};
use zirv_flow_store::secrets::OpenError;
use zirv_flow_store::{
    SigningKey, WEBHOOK_JOB_KIND, WebhookOutcome, WebhookPerson, WebhookResult, WebhookSubject,
    find_template, find_template_version, list_identities, live_signing_keys, lock_webhook_person,
    read_template_field_catalog, read_webhook_subject, record_webhook_outcome, secrets,
};

use crate::delivery::{self, Attempt, Verdict};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct WebhookCall;

/// The destination type this job delivers through.
const DESTINATION: &str = "webhook";

/// The destination id of a call to a raw URL, which has no row: the label of the client.
const INLINE: &str = "inline";

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
    /// The published webhook template to render instead of the envelope.
    template: Option<Uuid>,
}

impl Call {
    fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let id = |key: &str| text(key).and_then(|text| Uuid::parse_str(text).ok());
        let template = match payload.get("template") {
            None | Some(Value::Null) => None,
            Some(value) => Some(Uuid::parse_str(value.as_str()?).ok()?),
        };
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
            template,
        })
    }
}

/// What goes on the wire, or why nothing can.
enum Prepared {
    /// The exact body text, and the extra headers a template rendered.
    Ready {
        body: String,
        headers: Vec<(String, String)>,
    },
    /// The call ends here with this verdict.
    Failed(Verdict),
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
        // A template reads the traits of the workspace's registry, so those are read too.
        let catalog = match call.template {
            Some(_) => Some(read_template_field_catalog(ctx.conn(), call.workspace).await?),
            None => None,
        };
        let mut keys = call.trait_keys.clone();
        if let Some(catalog) = &catalog {
            keys.extend(
                catalog
                    .traits
                    .keys()
                    .filter(|key| !call.trait_keys.contains(key))
                    .cloned(),
            );
        }
        // Read with no lock: the call below can take up to 30 s per hop, and nothing an ingest
        // batch or an erasure needs may be held for that long.
        let Some(WebhookSubject { person, traits }) =
            read_webhook_subject(ctx.conn(), call.workspace, call.enrollment, &keys).await?
        else {
            // Erased with the person, exited (a goal, an operator, a merge) or merged away:
            // there is nobody to call about, and nothing to retry. The call is not sent for the
            // survivor of a merge, who was never the person this enrollment was about.
            return Ok(());
        };

        let prepared = match call.template {
            None => {
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
                Prepared::Ready {
                    body: String::from_utf8(envelope.body())
                        .map_err(|_| JobError::new("the envelope is not text"))?,
                    headers: Vec::new(),
                }
            }
            Some(template) => {
                let people = catalog.map(|catalog| catalog.traits).unwrap_or_default();
                render(ctx, &call, template, person, &traits, people.keys()).await?
            }
        };
        let done = match prepared {
            Prepared::Ready { body, headers } => deliver(ctx, &call, &body, headers, now).await?,
            Prepared::Failed(verdict) => Attempt {
                verdict,
                elapsed: Duration::ZERO,
            },
        };

        let attempts = ctx.attempts();
        // A raw URL has no destination row, so no attempt row is written for it.
        delivery::settle(ctx, None, attempts, &done).await?;
        let last_attempt = attempts >= ctx.max_attempts();
        let (result, status, reason) = match done.verdict {
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
        // The person is locked only now, for the short outcome write, and the enrollment checked
        // again: an erasure that committed during the call leaves no event and no decision. A call
        // already on the wire when it committed was allowed to complete.
        match lock_webhook_person(ctx.conn(), call.workspace, call.enrollment, person).await? {
            WebhookPerson::Held => {}
            WebhookPerson::Gone => return Ok(()),
            // Merged away during the call, which was already on the wire: nothing is written for
            // an id that is no longer the person, and a retry would only read the same.
            WebhookPerson::Moved => return Ok(()),
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
                sync: None,
            },
        )
        .await?;
        Ok(())
    }
}

/// A call that cannot be rendered ends for good, with a fixed reason.
fn render_failed(reason: &'static str) -> Prepared {
    Prepared::Failed(Verdict::Terminal {
        status: None,
        reason,
    })
}

/// Renders the node's template for the person in the render host pool. `registry_traits` are the
/// keys of the workspace's registry traits, the ones a template may read; `traits` holds the
/// person's value for each (null when they have none).
async fn render(
    ctx: &mut JobContext<'_>,
    call: &Call,
    template: Uuid,
    person: Uuid,
    traits: &Map<String, Value>,
    registry_traits: impl Iterator<Item = &String>,
) -> Result<Prepared, JobError> {
    let unavailable = || render_failed("template_unavailable");
    let Some(found) = find_template(ctx.conn(), call.workspace, template).await? else {
        return Ok(unavailable());
    };
    let Some(current) = found.current_version else {
        return Ok(unavailable());
    };
    if found.channel != "webhook" || found.archived_at.is_some() {
        return Ok(unavailable());
    }
    let Some(version) =
        find_template_version(ctx.conn(), call.workspace, template, current).await?
    else {
        return Ok(unavailable());
    };
    let Ok(doc) = serde_json::from_value::<TemplateDocument>(version.document) else {
        return Ok(unavailable());
    };

    let mut context = RenderContext::default();
    for key in registry_traits {
        if let Some(value) = traits.get(key).filter(|value| !value.is_null()) {
            context.person.insert(key.clone(), value.clone());
        }
    }
    if let Some(email) = list_identities(ctx.conn(), call.workspace, person)
        .await?
        .into_iter()
        .find(|identity| identity.kind == "email")
    {
        context
            .person
            .insert("email".to_owned(), Value::from(email.value));
    }
    context.message.id.clone_from(&call.idempotency_key);

    let Some(pool) = ctx.render_pool().cloned() else {
        warn!("webhook template not rendered: the worker has no render pool");
        return Ok(render_failed("render_error"));
    };
    match pool.render_webhook(&doc, &context).await {
        Ok(rendered) => Ok(Prepared::Ready {
            body: rendered.body,
            headers: rendered.headers,
        }),
        Err(failure) => {
            warn!(reason = failure.reason(), "webhook template not rendered");
            Ok(render_failed("render_error"))
        }
    }
}

/// Opens the signing secrets, hands the exact body to the `webhook` destination and judges its
/// answer. An error is a failure of this worker or of the database, not of the destination; the
/// destination's answer is a [`Verdict`].
async fn deliver(
    ctx: &mut JobContext<'_>,
    call: &Call,
    body: &str,
    headers: Vec<(String, String)>,
    now: OffsetDateTime,
) -> Result<Attempt, JobError> {
    let early = |verdict| Attempt {
        verdict,
        elapsed: Duration::ZERO,
    };
    let Some(keys) = live_signing_keys(ctx.conn(), call.workspace, now).await? else {
        return Ok(early(Verdict::Terminal {
            status: None,
            reason: "no_signing_secret",
        }));
    };
    let Some(keyring) = ctx.keyring().cloned() else {
        return Ok(early(signing_secret_unavailable()));
    };
    let mut credential = Credential::new();
    // The current secret must open; a previous one that does not is skipped, since the receiver
    // that still trusts it also trusts the current one the call is signed with.
    let named = [
        ("signing_secret", Some(keys.current)),
        ("previous_signing_secret", keys.previous),
    ];
    for (index, (name, key)) in named.into_iter().enumerate() {
        let Some(key) = key else { continue };
        match open_secret(ctx, &keyring, call.workspace, key).await? {
            Some(secret) => credential.insert(name, secret),
            None if index == 0 => return Ok(early(signing_secret_unavailable())),
            None => {}
        }
    }

    let registry = Registry::built_in();
    let destination = registry
        .get(DESTINATION)
        .ok_or_else(|| JobError::new("the webhook destination is not registered"))?;
    // No redirects: a 301, 302 or 303 would turn the POST into a GET without the body, and the
    // call would be judged by the answer to something that was never the call. The 3xx itself is
    // judged, and is terminal.
    let limits = HttpLimits {
        max_redirects: 0,
        ..HttpLimits::default()
    };
    let destination_context =
        DestinationContext::new(ctx.egress().clone(), limits, DESTINATION, INLINE)
            .map_err(|_| JobError::new("the outbound client could not be built"))?;
    let payload = json!({
        "body": body,
        "timestamp": now.unix_timestamp(),
        "headers": headers.into_iter().map(|(name, value)| (name, Value::String(value))).collect::<Map<String, Value>>(),
    });
    let message = Outbound {
        idempotency_key: &call.idempotency_key,
        channel: "webhook",
        to: "",
        payload: &payload,
        dkim: None,
    };
    Ok(delivery::send(
        destination,
        &destination_context,
        &json!({ "url": call.url }),
        &credential,
        &message,
        now,
    )
    .await)
}

/// The current signing secret cannot be opened (the worker has no keyring, or a wrong one). It is
/// retried, and on the last attempt it is recorded as a failure, so a flow waiting on
/// `webhook.failed` is told instead of the job dying unseen.
fn signing_secret_unavailable() -> Verdict {
    Verdict::Retry {
        status: None,
        reason: "signing_secret_unavailable",
        message: "the signing secret could not be opened",
        retry_after: None,
    }
}

/// Opens one signing secret as text. None when it does not open; only a database error is an
/// error.
async fn open_secret(
    ctx: &mut JobContext<'_>,
    keyring: &Keyring,
    workspace: WorkspaceId,
    key: SigningKey,
) -> Result<Option<SecretString>, JobError> {
    match secrets::open_json(
        ctx.conn(),
        keyring,
        workspace,
        key.secret_id,
        SecretPurpose::WebhookSigning,
    )
    .await
    {
        Ok(secret) => Ok(SecretString::try_from(secret).ok()),
        Err(OpenError::Store(error)) => Err(error.into()),
        Err(OpenError::NotFound | OpenError::Unopenable) => Ok(None),
    }
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
    use tokio::sync::{Notify, watch};
    use zirv_flow_connectors::http::HttpError;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::flow::{SIGNATURE_HEADER, SIGNING_GRACE_SECONDS, TIMESTAMP_HEADER};
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
    use crate::delivery::verdict::{judge, judge_error, parse_retry_after};
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

        /// Whether the request verifies with `secret` over the bytes received, through the verifier
        /// an inbound hook uses, which accepts a request when any one signature value verifies.
        fn verifies_with(&self, secret: &str, now: i64) -> bool {
            let scheme = Scheme {
                signature_header: SIGNATURE_HEADER,
                signature_prefix: "sha256=",
                timestamp_header: Some(TIMESTAMP_HEADER),
                tolerance_seconds: 300,
            };
            let lookup = |name: &str| self.all(name);
            verify(&scheme, secret.as_bytes(), &lookup, &self.body, now).is_ok()
        }
    }

    /// What the mock answers next.
    #[derive(Clone)]
    struct Answer {
        status: u16,
        retry_after: Option<&'static str>,
    }

    /// Holds the next answer until the test lets it go.
    #[derive(Clone)]
    struct Gate {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    /// A destination that records what it receives and answers from a script, then 200.
    #[derive(Clone, Default)]
    struct Mock {
        hits: Arc<Mutex<Vec<Hit>>>,
        script: Arc<Mutex<VecDeque<Answer>>>,
        gate: Arc<Mutex<Option<Gate>>>,
    }

    impl Mock {
        fn hits(&self) -> Vec<Hit> {
            self.hits.lock().unwrap().clone()
        }

        /// Makes the next request wait after it is recorded; the gate says when it arrived and
        /// lets it be answered.
        fn hold(&self) -> Gate {
            let gate = Gate {
                entered: Arc::default(),
                release: Arc::default(),
            };
            *self.gate.lock().unwrap() = Some(gate.clone());
            gate
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
        let gate = mock.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
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
            .route(
                "/moved",
                post(|| async { (StatusCode::FOUND, [(header::LOCATION, "/hook")]) }),
            )
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
        /// The keyring the worker holds; the workspace's secrets are sealed with `keyring`.
        worker_keyring: Option<Arc<Keyring>>,
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
            let keyring = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
            let env = Self {
                pool,
                ws,
                clock: ManualClock::new(datetime!(2031-05-06 07:08:09 UTC)),
                worker_keyring: Some(keyring.clone()),
                keyring,
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
            self.queue_to(enrolled, person, max_attempts, &self.url())
                .await;
        }

        async fn queue_to(&self, enrolled: &Enrolled, person: Uuid, max_attempts: i32, url: &str) {
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
                "url": url,
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
                        self.worker_keyring.clone(),
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
    async fn the_person_is_not_locked_while_the_call_is_in_flight_and_an_erasure_during_it_writes_nothing(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        let gate = env.mock.hold();
        let leased = env.claim().await.expect("the job is leased");

        let run = env.execute(leased);
        let meanwhile = async {
            gate.entered.notified().await;
            // The call is on the wire. An ingest-style row lock must not wait for it.
            let mut other = env.pool.acquire().await.unwrap();
            sqlx::query("SET lock_timeout = '300ms'")
                .execute(&mut *other)
                .await
                .unwrap();
            let locked = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
                .bind(person)
                .fetch_optional(&mut *other)
                .await;
            // Erased while the destination is answering.
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
            gate.release.notify_one();
            locked
        };
        let (result, locked) = tokio::join!(run, meanwhile);

        assert!(
            locked.is_ok(),
            "an ingest-style lock on the person is not blocked by the call"
        );
        result.unwrap();
        assert_eq!(env.mock.hits().len(), 1, "the call on the wire completed");
        assert_eq!(env.jobs_left().await, 0, "the job ended as done");
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'webhook.%'")
                .await,
            0
        );
        assert!(env.decisions(&enrolled.trace).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_redirect_is_never_followed_and_fails_the_call_without_a_retry(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        let moved = format!("http://127.0.0.1:{}/moved", env.port);
        env.queue_to(&enrolled, person, 5, &moved).await;

        env.attempt().await.unwrap();

        assert!(
            env.mock.hits().is_empty(),
            "the redirect target is never called"
        );
        assert_eq!(env.jobs_left().await, 0, "done, not retried");
        assert!(env.events("webhook.succeeded").await.is_empty());
        assert_eq!(
            env.events("webhook.failed").await,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": 302, "attempts": 1})
            ]
        );
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [("failed".to_owned(), Some("http_status".to_owned()))]
        );
    }

    /// A worker holding `worker_keyring` instead of the workspace's, over two attempts.
    async fn last_attempt_without_a_usable_keyring(pool: PgPool, wrong: bool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        env.worker_keyring =
            wrong.then(|| Arc::new(Keyring::new(MasterKey::generate(), None).unwrap()));
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 2).await;

        let first = env.attempt().await;
        let events_after_first = env.events("webhook.failed").await.len();
        let last = env.attempt().await;

        assert_eq!(
            first.unwrap_err().message(),
            "the signing secret could not be opened"
        );
        assert_eq!(events_after_first, 0, "an early attempt retries");
        last.unwrap();
        assert!(env.mock.hits().is_empty());
        assert_eq!(env.jobs_left().await, 0, "done, not dead");
        assert_eq!(
            env.events("webhook.failed").await,
            [
                json!({"node_id": "hook", "enrollment_id": enrolled.enrollment,
                    "status": null, "attempts": 2})
            ]
        );
        assert_eq!(
            env.decisions(&enrolled.trace).await,
            [(
                "failed".to_owned(),
                Some("signing_secret_unavailable".to_owned())
            )]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_without_a_keyring_tells_the_flow_on_the_last_attempt(pool: PgPool) {
        last_attempt_without_a_usable_keyring(pool, false).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_with_a_wrong_master_key_tells_the_flow_on_the_last_attempt(pool: PgPool) {
        last_attempt_without_a_usable_keyring(pool, true).await;
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

    /// A call queued for an enrollment that is then exited, or whose person is then merged into
    /// another, ends done: nothing is sent, the job is not retried and no outcome is written.
    async fn no_call_after(pool: PgPool, merged: bool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        if merged {
            let survivor = env.person("survivor@example.com", json!({})).await;
            sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
                .bind(person)
                .bind(survivor)
                .execute(&env.pool)
                .await
                .unwrap();
        } else {
            sqlx::query(
                "UPDATE enrollments SET state = 'exited', exit_reason = 'goal', \
                    state_version = state_version + 1, ended_at = now() WHERE id = $1",
            )
            .bind(enrolled.enrollment)
            .execute(&env.pool)
            .await
            .unwrap();
        }

        env.attempt().await.expect("the job ends done, not retried");

        assert!(env.mock.hits().is_empty(), "no call was made");
        assert_eq!(env.jobs_left().await, 0, "the job ended as done");
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'webhook.%'")
                .await,
            0
        );
        assert!(env.decisions(&enrolled.trace).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_for_an_exited_enrollment_ends_done_without_a_call(pool: PgPool) {
        no_call_after(pool, false).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_for_a_merged_away_person_ends_done_without_a_call(pool: PgPool) {
        no_call_after(pool, true).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_for_an_enrollment_that_completed_is_still_sent(pool: PgPool) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;
        sqlx::query("UPDATE enrollments SET state = 'completed', ended_at = now() WHERE id = $1")
            .bind(enrolled.enrollment)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        assert_eq!(env.mock.hits().len(), 1, "the flow ended after the node");
        assert_eq!(env.events("webhook.succeeded").await.len(), 1);
    }

    // Templated payloads (#216). The render host is this test binary run again, filtered to
    // `render_host_child`, which does nothing unless the pool started it.

    use zirv_flow_render::{ChildCommand, PoolConfig, RenderPool};
    use zirv_flow_store::{NewTemplate, create_template, lock_template, publish_template};

    #[test]
    fn render_host_child() {
        if std::env::var("ZIRV_FLOW_TEST_WEBHOOK_RENDER_HOST").is_err() {
            return;
        }
        std::process::exit(i32::from(zirv_flow_render::host::run(None)));
    }

    async fn start_pool() -> RenderPool {
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kinds::webhook_call::tests::render_host_child",
                "--nocapture",
                "--test-threads",
                "1",
                "-q",
            ])
            .env("ZIRV_FLOW_TEST_WEBHOOK_RENDER_HOST", "1")
            .skip_stdout_until("running 1 test\n");
        let config = PoolConfig {
            size: 1,
            deadline: Duration::from_secs(60),
        };
        RenderPool::start(command, config).await.unwrap()
    }

    impl Env {
        async fn define_number_trait(&self, key: &str) {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &NewTraitDefinition {
                    key: key.into(),
                    trait_type: TraitType::Number,
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

        /// A template of `channel` with `document` as its draft, published when `publish`.
        async fn template(&self, channel: &str, document: Value, publish: bool) -> Uuid {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            create_template(
                &mut connection,
                self.ws,
                &NewTemplate {
                    id,
                    name: format!("T {id}"),
                    channel: channel.into(),
                    category: "transactional".into(),
                    document,
                },
                None,
            )
            .await
            .unwrap();
            if publish {
                let locked = lock_template(&mut connection, self.ws, id).await.unwrap();
                publish_template(
                    &mut connection,
                    self.ws,
                    &locked,
                    &json!([]),
                    None,
                    &TraceContext::root(Cause::api("publish")),
                )
                .await
                .unwrap();
            }
            id
        }

        /// Queues the job `flow.step` would for a node that names `template`.
        async fn queue_templated(&self, enrolled: &Enrolled, person: Uuid, template: Uuid) {
            let key = format!("{}:hook:webhook", enrolled.enrollment);
            let mut job = NewJob::new(WEBHOOK_JOB_KIND, enrolled.trace.clone());
            job.workspace_id = Some(self.ws);
            job.unique_key = Some(key.clone());
            job.max_attempts = 5;
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
                "trait_keys": ["plan"],
                "template": template,
            });
            zirv_flow_store::enqueue(&self.pool, &job).await.unwrap();
        }

        /// Runs one attempt of the claimed job with `render_pool` given to the worker.
        async fn attempt_rendering(&self, render_pool: Option<RenderPool>) {
            let claimed = self.claim().await.expect("a webhook.call job is ready");
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_clock(Clock::Manual(self.clock.clone()))
                    .with_render_pool(render_pool)
                    .with_outbound(
                        self.worker_keyring.clone(),
                        self.egress.clone(),
                        Arc::default(),
                    );
                WebhookCall
                    .run(&mut ctx, claimed.payload.clone())
                    .await
                    .unwrap();
            }
            zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }
    }

    fn webhook_document(headers: Value, body: &str) -> Value {
        json!({"schema": 1, "channel": "webhook", "mode": "code",
            "webhook": {"headers": headers, "body": body}})
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_templated_node_sends_the_rendered_headers_and_body_and_signs_the_exact_bytes(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        env.define_number_trait("age").await;
        let person = env
            .person("ada@example.com", json!({"plan": "pro", "age": 41}))
            .await;
        let enrolled = env
            .enroll(hook_graph(&env.url(), json!(["plan"])), person)
            .await;
        let template = env
            .template(
                "webhook",
                webhook_document(
                    json!({"X-Plan": "{{ person.plan }}"}),
                    r#"{"age": "{{ person.age }}", "label": "Plan {{ person.plan }}", "to": "{{ person.email }}"}"#,
                ),
                true,
            )
            .await;
        env.queue_templated(&enrolled, person, template).await;

        env.attempt_rendering(Some(start_pool().await)).await;

        let hits = env.mock.hits();
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.header("x-plan"), Some("pro"));
        assert_eq!(hit.header("content-type"), Some("application/json"));
        let body: Value = serde_json::from_slice(&hit.body).unwrap();
        assert_eq!(
            body,
            json!({"age": 41, "label": "Plan pro", "to": "ada@example.com"})
        );
        assert!(
            body["age"].is_number(),
            "a single tag keeps its number type"
        );
        assert!(
            hit.verifies_with(&env.secrets[0], now_seconds(&env)),
            "the signature covers the bytes received"
        );
        assert!(!hit.verifies_with("zfwh_other", now_seconds(&env)));
        assert_eq!(env.events("webhook.succeeded").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_that_is_unpublished_or_of_another_channel_is_a_terminal_failure_with_no_call(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let email = json!({"schema": 1, "channel": "email", "mode": "code",
            "email": {"subject": "s", "html": "<p>x</p>"}});
        let archived = env
            .template("webhook", webhook_document(json!({}), "{}"), true)
            .await;
        zirv_flow_store::archive_template(&mut env.pool.acquire().await.unwrap(), env.ws, archived)
            .await
            .unwrap();
        let unusable = [
            env.template("webhook", webhook_document(json!({}), "{}"), false)
                .await,
            env.template("email", email, true).await,
            Uuid::now_v7(),
            archived,
        ];
        let render_host = start_pool().await;

        for template in unusable {
            let (person, enrolled) = env.enrolled().await;
            env.queue_templated(&enrolled, person, template).await;

            env.attempt_rendering(Some(render_host.clone())).await;

            assert_eq!(
                env.decisions(&enrolled.trace).await,
                [("failed".to_owned(), Some("template_unavailable".to_owned()))]
            );
        }
        assert!(env.mock.hits().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_render_failure_is_a_terminal_render_error_for_that_call_and_sends_nothing(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let template = env
            .template(
                "webhook",
                webhook_document(json!({}), r#"{"x": "{{ person.nothing_here }}"}"#),
                true,
            )
            .await;
        let render_host = start_pool().await;

        for pool in [Some(render_host), None] {
            let (person, enrolled) = env.enrolled().await;
            env.queue_templated(&enrolled, person, template).await;

            env.attempt_rendering(pool).await;

            assert_eq!(
                env.decisions(&enrolled.trace).await,
                [("failed".to_owned(), Some("render_error".to_owned()))]
            );
            assert_eq!(env.jobs_left().await, 0, "the job ended; it did not crash");
        }
        assert!(env.mock.hits().is_empty());
        assert_eq!(env.events("webhook.failed").await.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_node_without_a_template_sends_the_old_envelope_byte_for_byte_and_records_no_attempt(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool).await;
        env.sign_with_a_secret().await;
        let (person, enrolled) = env.enrolled().await;
        env.queue(&enrolled, person, 5).await;

        env.attempt().await.unwrap();

        let key = format!("{}:hook:webhook", enrolled.enrollment);
        let expected = format!(
            concat!(
                r#"{{"flow_id":"{flow}","version":1,"node_id":"hook","enrollment_id":"{enrollment}","#,
                r#""person_id":"{person}","trace_id":"{trace}","idempotency_key":"{key}","#,
                r#""occurred_at":"2031-05-06T07:08:09Z","traits":{{"age":null,"plan":"pro"}}}}"#
            ),
            flow = Uuid::from_u128(9),
            enrollment = enrolled.enrollment,
            person = person,
            trace = enrolled.trace.trace_id,
            key = key,
        );
        let hits = env.mock.hits();
        assert_eq!(String::from_utf8(hits[0].body.clone()).unwrap(), expected);
        assert_eq!(
            env.count("SELECT count(*) FROM delivery_attempts").await,
            0,
            "a raw URL has no destination row to record an attempt against"
        );
    }
}
