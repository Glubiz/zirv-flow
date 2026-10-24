//! `template.test_send`: one template sent to one workspace member or allowlisted address through
//! a real destination (architecture 11.7, #224).
//!
//! The API reserves the `test_sends` row (`queued`) and enqueues this job in one transaction; the
//! row's id is the idempotency key at the destination. The job reuses the send job's sender gate,
//! render host call, rate token and destination call, and writes only the row: no `messages` row,
//! no `email.*` event, no link rewriting, no open pixel, and no frequency cap, cooldown or stat
//! sees it (supervisor ruling 1d3048b4). `system.unsubscribe_url` and the `List-Unsubscribe`
//! header point to a signed `/u/{token}` URL that carries the test send's id; the route answers it
//! with an inert page.
//!
//! The gates, in order, and the ending each gives the row:
//!
//! 1. the template is a live email template, its version (or the draft) exists:
//!    `failed: template_unavailable`;
//! 2. the sender identity is live on a verified domain: `sender_unverified`;
//! 3. the recipient: a member's `users.email` (read now) or the address the job carries, whose
//!    keyed hash must be the row's: `failed: recipient_unavailable`;
//! 4. suppression of the recipient hash, in the workspace and the organization, is never skipped:
//!    `suppressed`, no call. Consent, policy, quiet hours and the frequency cap are skipped;
//! 5. the render host renders the chosen version (or the draft) for the sample person or the
//!    person named in the request, and the subject gets the prefix `[TEST] `: `render_failed`;
//! 6. the destination exists, is not archived and does email: `failed: destination_unavailable`
//!    or `destination_unsupported`;
//! 7. a rate token from the destination's bucket; with none, the job runs again when the bucket
//!    will hold one, and after six hours of waiting the row ends `failed: rate_limited`;
//! 8. the call. A lost reply after the message was handed over ends the row `unconfirmed` with no
//!    retry; any other retryable verdict retries the job under the same key, and on the last
//!    attempt the row ends `failed` with the verdict's fixed reason.
//!
//! A job that finds its row no longer `queued` sends nothing. The only address the system holds
//! for an allowlisted recipient is in the payload of the queued job, which is deleted when the
//! job completes; the job always completes on its last attempt.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::Registry;
use zirv_flow_core::{UserId, WorkspaceId};
use zirv_flow_render::RenderContext;
use zirv_flow_store::{
    Admission, DELIVERY_UNCONFIRMED, NewJob, SOFT_BOUNCE, TEST_SEND_JOB_KIND, TestSend,
    TestSendOutcome, find_destination, find_person, find_template, find_template_draft,
    find_template_version, find_user, finish_test_send, list_identities,
    read_template_field_catalog, suppressing_scope,
};
use zirv_flow_templates::FieldCatalog;

use super::delivery_send::deliver::{self, Call};
use super::delivery_send::gates;
use super::delivery_send::render::{self, Tracker};
use super::delivery_send::slow_down;
use crate::delivery::Verdict;
use crate::delivery::verdict::SMTP_DEFERRED;
use crate::{JobContext, JobError, JobKind};

/// The prefix of every test send's subject.
pub(crate) const SUBJECT_PREFIX: &str = "[TEST] ";

/// How long a test send waits for a rate-limit token before it ends `rate_limited`.
const MAX_RATE_WAIT: time::Duration = time::Duration::hours(6);

pub(crate) struct TemplateTestSend;

/// The payload of the job: ids, the optional person whose values render the template, and, for an
/// allowlisted recipient, the address (the one place it is held, until the job completes).
struct Payload {
    workspace: WorkspaceId,
    id: Uuid,
    person: Option<Uuid>,
    address: Option<String>,
    deferrals: u32,
    first_wait: Option<OffsetDateTime>,
}

impl Payload {
    fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let optional_id = |key: &str| match payload.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(value) => Some(Some(Uuid::parse_str(value.as_str()?).ok()?)),
        };
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            id: Uuid::parse_str(text("test_send_id")?).ok()?,
            person: optional_id("person_id")?,
            address: text("address").map(str::to_owned),
            deferrals: payload
                .get("deferrals")
                .and_then(Value::as_u64)
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0),
            first_wait: text("first_wait").and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok()),
        })
    }
}

/// What a run of the job came to.
enum Step {
    /// The job is over and the row stays `queued`: the send was queued again.
    Over,
    /// The row ends.
    End {
        outcome: TestSendOutcome,
        reason: Option<&'static str>,
    },
}

fn end(outcome: TestSendOutcome, reason: &'static str) -> Result<Step, JobError> {
    Ok(Step::End {
        outcome,
        reason: Some(reason),
    })
}

impl JobKind for TemplateTestSend {
    const KIND: &'static str = TEST_SEND_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = Payload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a test send"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        // A row that is gone (its workspace, or the retention) or already ended has nothing left
        // to send.
        let Some(row) = zirv_flow_store::find_test_send(ctx.conn(), job.workspace, job.id).await?
        else {
            return Ok(());
        };
        if row.outcome != TestSendOutcome::Queued {
            return Ok(());
        }
        let last = ctx.attempts() >= ctx.max_attempts();
        match decide(ctx, &job, &row, &payload, now).await {
            Ok(Step::Over) => Ok(()),
            Ok(Step::End { outcome, reason }) => {
                finish_test_send(ctx.conn(), job.workspace, job.id, outcome, reason, now).await?;
                Ok(())
            }
            Err(error) if last => {
                // The job completes either way, so the payload (and the address in it) goes.
                warn!("a test send ends failed on its last attempt");
                if let Some(pool) = ctx.pool() {
                    let mut connection = pool.acquire().await.map_err(|_| error)?;
                    finish_test_send(
                        &mut connection,
                        job.workspace,
                        job.id,
                        TestSendOutcome::Failed,
                        Some("internal_error"),
                        now,
                    )
                    .await?;
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

/// Runs the gates and, when they let the test through, delivers it.
async fn decide(
    ctx: &mut JobContext<'_>,
    job: &Payload,
    row: &TestSend,
    payload: &Value,
    now: OffsetDateTime,
) -> Result<Step, JobError> {
    let workspace = job.workspace;
    let pool = ctx
        .pool()
        .cloned()
        .ok_or_else(|| JobError::new("the worker has no pool"))?;

    // The template and the document asked for.
    let template = find_template(ctx.conn(), workspace, row.template_id).await?;
    let usable = template
        .as_ref()
        .is_some_and(|template| template.channel == "email" && template.archived_at.is_none());
    let document = if !usable {
        None
    } else if let Some(version) = row.template_version {
        find_template_version(ctx.conn(), workspace, row.template_id, version)
            .await?
            .map(|version| version.document)
    } else {
        find_template_draft(ctx.conn(), workspace, row.template_id)
            .await?
            .map(|draft| draft.document)
    };
    let Some(document) = document else {
        return end(TestSendOutcome::Failed, "template_unavailable");
    };

    // The sender, as the send job checks it.
    let sender = match gates::sender(&pool, workspace, Some(row.sender_identity_id)).await? {
        Ok(sender) => sender,
        Err(_) => return end(TestSendOutcome::SenderUnverified, gates::SENDER_UNVERIFIED),
    };

    // The recipient: a member's verified address now, or the address the job carries. Either must
    // hash to the row's recipient hash.
    let key = ctx
        .identifier_key()
        .ok_or_else(|| JobError::new("the worker has no identifier key"))?;
    let address = match (&job.address, row.recipient_member_id) {
        (Some(address), _) => Some(address.clone()),
        (None, Some(member)) => find_user(ctx.conn(), UserId::from_uuid(member))
            .await?
            .map(|user| user.email),
        (None, None) => None,
    };
    let Some((address, hash)) = address.and_then(|address| {
        let hash = gates::address_hash(&key, &address)?;
        (hash.as_slice() == row.recipient_hash.as_slice()).then_some((address, hash))
    }) else {
        return end(TestSendOutcome::Failed, "recipient_unavailable");
    };

    // Suppression is the one person gate a test never skips.
    if suppressing_scope(ctx.conn(), workspace, &hash)
        .await?
        .is_some()
    {
        return end(TestSendOutcome::Suppressed, gates::SUPPRESSED);
    }

    // Render, in the pool, for the sample person or the person named. No link is rewritten and no
    // pixel is added: the unsubscribe URLs carry the test send's id and are inert.
    let read = read_template_field_catalog(ctx.conn(), workspace).await?;
    let catalog = FieldCatalog {
        person: read.traits.into_iter().collect(),
        event: read.event_properties.into_iter().collect(),
    }
    .with_reserved_email();
    let mut context = match job.person {
        None => RenderContext::sample_person(&catalog),
        Some(person) => {
            let Some(found) = find_person(ctx.conn(), workspace, person).await? else {
                return end(TestSendOutcome::RenderFailed, "person_unavailable");
            };
            let email = list_identities(ctx.conn(), workspace, found.id)
                .await?
                .into_iter()
                .find(|identity| identity.kind == "email")
                .map(|identity| identity.value);
            RenderContext::real_person(&catalog, &found.traits, email)
        }
    };
    let tracker = ctx.tracking().map(|(base, key)| Tracker {
        base: base.to_owned(),
        key: Arc::clone(key),
    });
    render::address_links(&mut context, row.id, tracker.as_ref());
    let render_pool = ctx.render_pool().cloned();
    let tracked = match render::render(render_pool.as_ref(), document, &context, None, &[]).await {
        Ok(tracked) => tracked,
        Err(reason) => return end(TestSendOutcome::RenderFailed, reason),
    };
    let mut rendered = tracked.rendered;
    rendered.subject = format!("{SUBJECT_PREFIX}{}", rendered.subject);
    let email = render::email_payload(&sender, &rendered);

    // The destination, loaded for this test, and a token from its bucket.
    if job
        .first_wait
        .is_some_and(|first| now - first > MAX_RATE_WAIT)
    {
        return end(TestSendOutcome::Failed, "rate_limited");
    }
    let Some(destination) = find_destination(ctx.conn(), workspace, row.destination_id)
        .await?
        .filter(|destination| destination.archived_at.is_none())
    else {
        return end(TestSendOutcome::Failed, "destination_unavailable");
    };
    let registry = Registry::built_in();
    let Some(driver) = registry.get(&destination.kind) else {
        return end(TestSendOutcome::Failed, "destination_unavailable");
    };
    if !driver.spec().capabilities.channels.contains(&"email") {
        return end(TestSendOutcome::Failed, "destination_unsupported");
    }
    let gates = Arc::clone(ctx.rate_gates());
    let clock = ctx.clock().clone();
    match gates.admit(&pool, destination.id, &clock).await? {
        Admission::Granted => {}
        Admission::Wait(wait) => return defer(ctx, job, payload, wait, now).await,
        Admission::Gone => return end(TestSendOutcome::Failed, "destination_unavailable"),
    }

    // The call, holding no lock. The log id is the idempotency key.
    let call = Call {
        workspace,
        destination: &destination,
        driver,
        sender: &sender,
        message: row.id,
        address: &address,
        payload: &email,
    };
    let done = deliver::deliver(ctx, &call, now).await?;
    match done.verdict {
        Verdict::Succeeded { .. } => Ok(Step::End {
            outcome: TestSendOutcome::Sent,
            reason: None,
        }),
        // A hard bounce of a test adds no suppression: the row records the reason and no more.
        Verdict::Terminal { reason, .. } => end(TestSendOutcome::Failed, reason),
        Verdict::Retry { reason, .. } if reason == DELIVERY_UNCONFIRMED => {
            end(TestSendOutcome::Unconfirmed, DELIVERY_UNCONFIRMED)
        }
        Verdict::Retry {
            reason,
            message,
            retry_after,
            ..
        } => {
            if reason == "http_429" {
                slow_down(&pool, &destination, retry_after, ctx).await;
            }
            if ctx.attempts() < ctx.max_attempts() {
                let error = JobError::new(message);
                return Err(retry_after.map_or(error, |wait| error.with_retry_after(wait)));
            }
            end(
                TestSendOutcome::Failed,
                if reason == SMTP_DEFERRED {
                    SOFT_BOUNCE
                } else {
                    reason
                },
            )
        }
    }
}

/// No token yet: this job ends and a copy runs when the bucket will hold one. The copy has its own
/// unique key and the attempts this job has left.
async fn defer(
    ctx: &mut JobContext<'_>,
    job: &Payload,
    payload: &Value,
    wait: Duration,
    now: OffsetDateTime,
) -> Result<Step, JobError> {
    let attempts = ctx.attempts();
    let mut next_payload = payload.clone();
    if let Some(map) = next_payload.as_object_mut() {
        map.insert("deferrals".to_owned(), (job.deferrals + 1).into());
        if let Ok(first) = job.first_wait.unwrap_or(now).format(&Rfc3339) {
            map.insert("first_wait".to_owned(), first.into());
        }
    }
    let mut next = NewJob::new(TEST_SEND_JOB_KIND, ctx.trace().clone());
    next.workspace_id = Some(job.workspace);
    next.payload = next_payload;
    next.run_at = Some(now + wait);
    next.max_attempts = (ctx.max_attempts() - (attempts - 1)).max(1);
    next.unique_key = Some(format!(
        "{TEST_SEND_JOB_KIND}:{}:defer:{}",
        job.id,
        job.deferrals + 1
    ));
    ctx.enqueue(&next).await?;
    Ok(Step::Over)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_connectors::destinations::{FakeSent, fake_ledger_calls, fake_ledger_sent};
    use zirv_flow_core::consent::SuppressionReason;
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::tracking::{TokenKind, verify};
    use zirv_flow_core::{
        Cause, HmacKey, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_render::{ChildCommand, PoolConfig, RenderPool};
    use zirv_flow_store::sending_fixtures::plant_verified_sender;
    use zirv_flow_store::{
        CapMode, ClaimedJob, Clock, ManualClock, NewDestination, NewFrequencyCap, NewSuppression,
        NewTemplate, NewTestSend, TestSendReservation, add_organization_suppression,
        add_suppression, create_destination, create_template, lock_template, publish_template,
        reserve_test_send,
    };

    use crate::context::IngestSettings;
    use crate::delivery::rate::RateGates;

    use super::*;

    #[test]
    fn the_kind_name_is_template_test_send() {
        assert_eq!(TemplateTestSend::KIND, "template.test_send");
    }

    #[test]
    fn a_payload_is_parsed_with_or_without_a_person_and_an_address() {
        let ws = WorkspaceId::new();
        let id = Uuid::now_v7();
        let person = Uuid::now_v7();

        let plain = Payload::parse(&json!({
            "workspace_id": ws, "test_send_id": id, "person_id": null, "address": null
        }))
        .unwrap();
        let full = Payload::parse(&json!({
            "workspace_id": ws, "test_send_id": id, "person_id": person,
            "address": "a@example.test", "deferrals": 2, "first_wait": "2031-05-06T07:08:09Z"
        }))
        .unwrap();

        assert_eq!(
            (plain.person, plain.address, plain.deferrals),
            (None, None, 0)
        );
        assert_eq!(full.person, Some(person));
        assert_eq!(full.address.as_deref(), Some("a@example.test"));
        assert_eq!(full.deferrals, 2);
        assert!(full.first_wait.is_some());
        assert!(Payload::parse(&json!({"workspace_id": ws})).is_none());
        assert!(
            Payload::parse(&json!({
                "workspace_id": ws, "test_send_id": id, "person_id": "not-a-uuid"
            }))
            .is_none()
        );
    }

    // The render host is this test binary run again, filtered to `render_host_child`, which does
    // nothing unless the pool started it.
    #[test]
    fn render_host_child() {
        if std::env::var("ZIRV_FLOW_TEST_TSEND_RENDER_HOST").is_err() {
            return;
        }
        let limit = 512 * 1024 * 1024;
        std::process::exit(i32::from(zirv_flow_render::host::run(Some(limit))));
    }

    async fn start_pool() -> RenderPool {
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kinds::template_test_send::tests::render_host_child",
                "--nocapture",
                "--test-threads",
                "1",
                "-q",
            ])
            .env("ZIRV_FLOW_TEST_TSEND_RENDER_HOST", "serve")
            .skip_stdout_until("running 1 test\n");
        let config = PoolConfig {
            size: 1,
            deadline: Duration::from_secs(60),
        };
        RenderPool::start(command, config).await.unwrap()
    }

    const PUBLIC_URL: &str = "https://flow.example.test";

    fn root() -> HmacRoot {
        HmacRoot::bootstrap_from(
            &MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap(),
        )
    }

    fn identifier_key() -> Arc<HmacKey> {
        Arc::new(root().hmac_key(HmacPurpose::Identifier))
    }

    fn tracking_key() -> Arc<HmacKey> {
        Arc::new(root().hmac_key(HmacPurpose::Tracking))
    }

    fn hash_of(address: &str) -> [u8; 32] {
        gates::address_hash(&identifier_key(), address).unwrap()
    }

    const HTML: &str = "<p>Hello</p><a href=\"https://shop.example.test/a\">Shop</a>\
        <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>";

    /// A workspace with a verified sender, a fake destination that keeps a ledger and a worker
    /// that runs `template.test_send` jobs by hand.
    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        clock: ManualClock,
        gates: Arc<RateGates>,
        render: Option<RenderPool>,
        sender: Uuid,
        destination: Uuid,
        ledger: String,
        trace: TraceContext,
    }

    impl Env {
        async fn new(pool: PgPool, config: Value) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse("ours").unwrap(),
                &Slug::parse("ours").unwrap(),
            )
            .await
            .unwrap()
            .id;
            let sender = plant_verified_sender(&pool, ws, "example.com")
                .await
                .unwrap();
            let ledger = format!("tsend-{}", Uuid::now_v7());
            let mut config = config;
            config["ledger"] = json!(ledger);
            let destination = create_destination(
                &pool,
                ws,
                &NewDestination {
                    id: Uuid::now_v7(),
                    kind: "fake",
                    name: "Mail",
                    config: &config,
                    credential_id: None,
                    rate: 100,
                    burst: 100,
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .id;
            Self {
                pool,
                ws,
                clock: ManualClock::new(datetime!(2031-05-06 07:08:09 UTC)),
                gates: Arc::default(),
                render: Some(start_pool().await),
                sender,
                destination,
                ledger,
                trace: TraceContext::root(Cause::api("t")),
            }
        }

        async fn member(&self, email: &str) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
                .bind(id)
                .bind(email)
                .execute(&self.pool)
                .await
                .unwrap();
            id
        }

        /// A published email template with this document; its id.
        async fn template(&self, document: Value) -> Uuid {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            create_template(
                &mut connection,
                self.ws,
                &NewTemplate {
                    id,
                    name: format!("T {id}"),
                    channel: "email".into(),
                    category: "marketing".into(),
                    document,
                },
                None,
            )
            .await
            .unwrap();
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
            id
        }

        async fn html_template(&self) -> Uuid {
            self.template(
                json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                    "subject": "Hello", "sender": self.sender, "lang": "en",
                    "category": "marketing", "html": HTML,
                    "text": "Hello {{ system.unsubscribe_url }}",
                }}),
            )
            .await
        }

        /// Reserves a test send to `address` (a member's when `member` is given, else carried in
        /// the job) as the API does.
        async fn reserve(&self, template: Uuid, address: &str, member: Option<Uuid>) -> Uuid {
            self.reserve_with(template, address, member, None, self.sender, Some(1))
                .await
        }

        async fn reserve_with(
            &self,
            template: Uuid,
            address: &str,
            member: Option<Uuid>,
            person: Option<Uuid>,
            sender: Uuid,
            version: Option<i32>,
        ) -> Uuid {
            let id = Uuid::now_v7();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let reservation = reserve_test_send(
                &mut tx,
                &NewTestSend {
                    id,
                    workspace: self.ws,
                    template,
                    template_version: version,
                    member: None,
                    destination: self.destination,
                    sender_identity: sender,
                    recipient_hash: hash_of(address),
                    recipient_member: member,
                    person,
                    address: member.is_none().then_some(address),
                    now: self.clock.now(),
                    trace: &self.trace,
                },
            )
            .await
            .unwrap();
            assert!(matches!(reservation, TestSendReservation::Reserved(_)));
            zirv_flow_store::commit(tx).await.unwrap();
            id
        }

        async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[TEST_SEND_JOB_KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
        }

        /// Runs one claimed job as a worker does.
        async fn execute(&self, claimed: ClaimedJob) -> Result<(), JobError> {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let result = {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_clock(Clock::Manual(self.clock.clone()))
                    .with_ingest(IngestSettings {
                        pool: self.pool.clone(),
                        redaction_key: None,
                        batch_size: 1,
                        shards: 1,
                    })
                    .with_identifier_key(Some(identifier_key()))
                    .with_render_pool(self.render.clone())
                    .with_tracking(Some(PUBLIC_URL.to_owned()), Some(tracking_key()))
                    .with_rate_gates(Arc::clone(&self.gates))
                    .with_outbound(
                        None,
                        Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap()),
                        Arc::default(),
                    );
                TemplateTestSend
                    .run(&mut ctx, claimed.payload.clone())
                    .await
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

        async fn attempt(&self) -> Result<(), JobError> {
            let claimed = self
                .claim()
                .await
                .expect("a template.test_send job is ready");
            self.execute(claimed).await
        }

        async fn row(&self, id: Uuid) -> TestSend {
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::find_test_send(&mut connection, self.ws, id)
                .await
                .unwrap()
                .expect("the row exists")
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        fn sent(&self) -> Vec<FakeSent> {
            fake_ledger_sent(&self.ledger)
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_test_to_a_member_arrives_marked_untracked_and_leaves_no_trace_in_the_message_log(
        pool: PgPool,
    ) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let member = env.member("ada@example.com").await;
        let template = env.html_template().await;
        // A cap of one per day that is already spent must not stop a test, and a test spends none.
        let mut connection = env.pool.acquire().await.unwrap();
        zirv_flow_store::replace_send_limits(
            &mut connection,
            env.ws,
            &[NewFrequencyCap {
                channel: "email".into(),
                window_hours: 24,
                limit: 1,
                mode: CapMode::Skip,
            }],
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let first = env.reserve(template, "ada@example.com", Some(member)).await;
        let second = env.reserve(template, "ada@example.com", Some(member)).await;

        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        for id in [first, second] {
            let row = env.row(id).await;
            assert_eq!(row.outcome, TestSendOutcome::Sent);
            assert_eq!(row.reason, None);
            assert!(row.completed_at.is_some());
        }
        let sent = env.sent();
        assert_eq!(sent.len(), 2);
        let mut keys: Vec<&str> = sent.iter().map(|sent| sent.key.as_str()).collect();
        keys.sort_unstable();
        let mut ids = [first.to_string(), second.to_string()];
        ids.sort_unstable();
        assert_eq!(
            keys,
            [ids[0].as_str(), ids[1].as_str()],
            "the log id is the key"
        );
        let one = sent
            .iter()
            .find(|sent| sent.key == first.to_string())
            .unwrap();
        assert_eq!(one.to, "ada@example.com");
        assert_eq!(one.payload["subject"], "[TEST] Hello");
        let html = one.payload["html"].as_str().unwrap();
        assert!(html.contains("https://shop.example.test/a"), "{html}");
        assert!(!html.contains("/t/c/") && !html.contains("/t/o/"), "{html}");
        let header = one.payload["headers"]["List-Unsubscribe"].as_str().unwrap();
        let token = header
            .trim_matches(['<', '>'])
            .strip_prefix(&format!("{PUBLIC_URL}/u/"))
            .expect("the header points to an unsubscribe URL");
        let claims = verify(&tracking_key(), TokenKind::Unsubscribe, token).expect("signed");
        assert_eq!(claims.message, first);
        assert_eq!(env.count("SELECT count(*) FROM messages").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'email.%'")
                .await,
            0
        );
        assert_eq!(env.count("SELECT count(*) FROM message_links").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM jobs").await,
            0,
            "the jobs completed"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_test_to_an_allowlisted_address_arrives(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let template = env.html_template().await;
        let id = env.reserve(template, "qa@example.org", None).await;

        env.attempt().await.unwrap();

        assert_eq!(env.row(id).await.outcome, TestSendOutcome::Sent);
        let sent = env.sent();
        assert_eq!((sent.len(), sent[0].to.as_str()), (1, "qa@example.org"));
        assert_eq!(env.count("SELECT count(*) FROM jobs").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_html_an_mjml_and_a_blocks_template_each_test_send(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let unsubscribe = r#"<a href="{{ system.unsubscribe_url }}">Unsubscribe</a>"#;
        let documents = [
            json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Html", "sender": env.sender, "lang": "en", "category": "marketing",
                "html": format!("<p>Html</p>{unsubscribe}"), "text": "t {{ system.unsubscribe_url }}"
            }}),
            json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Mjml", "sender": env.sender, "category": "marketing", "syntax": "mjml",
                "html": format!(
                    "<mjml lang=\"en\"><mj-body><mj-section><mj-column><mj-text>Mjml \
                     {unsubscribe}</mj-text></mj-column></mj-section></mj-body></mjml>"
                ),
                "text": "t {{ system.unsubscribe_url }}"
            }}),
            json!({"schema": 1, "channel": "email", "mode": "blocks", "blocks": {
            "subject": "Blocks", "sender": "snd_1", "lang": "en", "category": "marketing",
            "sections": [
                {"id": "intro", "columns": [[
                    {"type": "heading", "id": "title", "level": 1, "text": "Blocks"}
                ]]},
                {"id": "foot", "columns": [[
                    {"type": "text", "id": "legal", "content": format!("<p>{unsubscribe}</p>")}
                ]]}
            ]}}),
        ];
        let mut ids = Vec::new();
        for document in documents {
            let template = env.template(document).await;
            ids.push(env.reserve(template, "qa@example.org", None).await);
        }

        for _ in &ids {
            env.attempt().await.unwrap();
        }

        for id in ids {
            let row = env.row(id).await;
            assert_eq!((row.outcome, row.reason), (TestSendOutcome::Sent, None));
        }
        let mut subjects: Vec<String> = env
            .sent()
            .iter()
            .map(|sent| sent.payload["subject"].as_str().unwrap().to_owned())
            .collect();
        subjects.sort();
        assert_eq!(subjects, ["[TEST] Blocks", "[TEST] Html", "[TEST] Mjml"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_draft_and_a_chosen_person_render_the_template(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let template = env
            .template(
                json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                    "subject": "Published", "sender": env.sender, "lang": "en",
                    "category": "marketing",
                    "html": "<a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
                    "text": "t {{ system.unsubscribe_url }}"
                }}),
            )
            .await;
        let mut connection = env.pool.acquire().await.unwrap();
        zirv_flow_store::save_template_draft(
            &mut connection,
            env.ws,
            template,
            &json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Draft {{ person.email }}", "sender": env.sender, "lang": "en",
                "category": "marketing",
                "html": "<a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>",
                "text": "t {{ system.unsubscribe_url }}"
            }}),
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(env.ws.as_uuid())
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value) \
             VALUES ($1, $2, $3, 'email', 'real@example.org')",
        )
        .bind(Uuid::now_v7())
        .bind(env.ws.as_uuid())
        .bind(person)
        .execute(&env.pool)
        .await
        .unwrap();
        let sample = env
            .reserve_with(template, "qa@example.org", None, None, env.sender, None)
            .await;
        let real = env
            .reserve_with(
                template,
                "qa@example.org",
                None,
                Some(person),
                env.sender,
                None,
            )
            .await;

        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        let subject_of = |id: Uuid| {
            env.sent()
                .into_iter()
                .find(|sent| sent.key == id.to_string())
                .unwrap()
                .payload["subject"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(subject_of(sample), "[TEST] Draft sample@example.invalid");
        assert_eq!(subject_of(real), "[TEST] Draft real@example.org");
        let row = env.row(real).await;
        assert_eq!(
            (row.render_context.as_str(), row.template_version),
            ("person", None)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_suppressed_recipient_ends_suppressed_with_no_call(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let template = env.html_template().await;
        add_suppression(
            &env.pool,
            env.ws,
            &NewSuppression {
                identifier_hash: hash_of("qa@example.org"),
                kind: "email",
                reason: SuppressionReason::Manual,
                source: "api",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let workspace_wide = env.reserve(template, "qa@example.org", None).await;
        add_organization_suppression(
            &mut *env.pool.acquire().await.unwrap(),
            &NewSuppression {
                identifier_hash: hash_of("org@example.org"),
                kind: "email",
                reason: SuppressionReason::HardBounce,
                source: "bounce",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let organization_wide = env.reserve(template, "org@example.org", None).await;

        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        for id in [workspace_wide, organization_wide] {
            let row = env.row(id).await;
            assert_eq!(row.outcome, TestSendOutcome::Suppressed);
            assert_eq!(row.reason.as_deref(), Some("suppressed"));
        }
        assert!(fake_ledger_calls(&env.ledger).is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unverified_sender_and_a_render_failure_end_their_rows_with_no_call(pool: PgPool) {
        let mut env = Env::new(pool, json!({"mode": "ok"})).await;
        let template = env.html_template().await;
        let unverified = env
            .reserve_with(
                template,
                "qa@example.org",
                None,
                None,
                Uuid::now_v7(),
                Some(1),
            )
            .await;
        env.attempt().await.unwrap();
        env.render = None;
        let unrendered = env.reserve(template, "qa@example.org", None).await;
        env.attempt().await.unwrap();

        let sender = env.row(unverified).await;
        let render = env.row(unrendered).await;
        assert_eq!(sender.outcome, TestSendOutcome::SenderUnverified);
        assert_eq!(sender.reason.as_deref(), Some("sender_unverified"));
        assert_eq!(render.outcome, TestSendOutcome::RenderFailed);
        assert_eq!(render.reason.as_deref(), Some("render_error"));
        assert!(fake_ledger_calls(&env.ledger).is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_lost_reply_ends_unconfirmed_without_a_retry_and_a_rerun_sends_nothing(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "lose_reply"})).await;
        let template = env.html_template().await;
        let id = env.reserve(template, "qa@example.org", None).await;

        env.attempt().await.unwrap();

        let row = env.row(id).await;
        assert_eq!(row.outcome, TestSendOutcome::Unconfirmed);
        assert_eq!(row.reason.as_deref(), Some(DELIVERY_UNCONFIRMED));
        assert_eq!(fake_ledger_calls(&env.ledger), [id.to_string()]);
        assert_eq!(env.count("SELECT count(*) FROM jobs").await, 0, "no retry");

        // The same job, delivered again, finds its row ended and sends nothing.
        let mut again = NewJob::new(TEST_SEND_JOB_KIND, env.trace.clone());
        again.workspace_id = Some(env.ws);
        again.unique_key = Some("again".into());
        again.payload = json!({
            "workspace_id": env.ws.as_uuid(), "test_send_id": id, "address": "qa@example.org"
        });
        zirv_flow_store::enqueue(&env.pool, &again).await.unwrap();
        env.attempt().await.unwrap();
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retryable_answer_retries_under_the_same_key_and_the_last_attempt_fails_the_row(
        pool: PgPool,
    ) {
        let env = Env::new(pool, json!({"mode": "server_error"})).await;
        let template = env.html_template().await;
        let id = env.reserve(template, "qa@example.org", None).await;
        sqlx::query("UPDATE jobs SET max_attempts = 2")
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap_err();
        assert_eq!(env.row(id).await.outcome, TestSendOutcome::Queued);
        env.attempt().await.unwrap();

        let row = env.row(id).await;
        assert_eq!(row.outcome, TestSendOutcome::Failed);
        assert_eq!(row.reason.as_deref(), Some("http_5xx"));
        assert_eq!(
            env.count("SELECT count(*) FROM jobs").await,
            0,
            "the payload is gone"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_without_a_rate_token_runs_again_when_the_bucket_holds_one(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        sqlx::query("UPDATE destinations SET rate = 1, burst = 1 WHERE id = $1")
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();
        let template = env.html_template().await;
        let first = env.reserve(template, "qa@example.org", None).await;
        let second = env.reserve(template, "qa@example.org", None).await;

        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        assert_eq!(env.row(first).await.outcome, TestSendOutcome::Sent);
        assert_eq!(env.row(second).await.outcome, TestSendOutcome::Queued);
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1);
        let (waiting, deferrals): (i64, Option<String>) = sqlx::query_as(
            "SELECT count(*), max(payload ->> 'deferrals') FROM jobs WHERE kind = $1",
        )
        .bind(TEST_SEND_JOB_KIND)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!((waiting, deferrals.as_deref()), (1, Some("1")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_destination_ends_the_row_failed(pool: PgPool) {
        let env = Env::new(pool, json!({"mode": "ok"})).await;
        let template = env.html_template().await;
        let id = env.reserve(template, "qa@example.org", None).await;
        sqlx::query("UPDATE destinations SET archived_at = now() WHERE id = $1")
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        let row = env.row(id).await;
        assert_eq!(row.outcome, TestSendOutcome::Failed);
        assert_eq!(row.reason.as_deref(), Some("destination_unavailable"));
        assert!(fake_ledger_calls(&env.ledger).is_empty());
    }
}
