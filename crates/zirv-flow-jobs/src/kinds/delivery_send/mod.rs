//! `delivery.send`: one message of an `action.send` node, through the gates of architecture 10.1
//! to a destination.
//!
//! `flow.step` enqueues the job in the step's transaction under the effect key, so the message is
//! queued exactly when the step commits and at most once per step. The message id is minted then
//! and travels in the payload: it is the idempotency key at the destination on every attempt,
//! deferral and retry, and the id of the `messages` row the job writes when the message ends.
//!
//! The gates, in the order they run (the `gate` of each entry in `messages.gate_results`):
//!
//! 1. `enrollment`: the enrollment is not over and its flow is not paused or archived; otherwise
//!    the message is dropped (a decision, no row).
//! 2. `template`: the template is a live email template with a published version; otherwise
//!    `failed: template_unavailable`.
//! 3. `sender`: the version's sender identity is live and its domain verified; otherwise
//!    `skipped: sender_unverified`. It runs before the gates that read the person, because it needs
//!    nothing about them and a template that cannot send is the template's fault.
//! 4. `address`: the person's address (a `channel_address` trait, else the newest email identity);
//!    otherwise `skipped: no_address`.
//! 5. `consent`: recorded consent under the topic's policy; otherwise `skipped: no_consent`. A
//!    transactional template skips it (OQ-11).
//! 6. `suppression`: the address hash in the workspace and the organization (OQ-19), for marketing
//!    and transactional mail; otherwise `skipped: suppressed`.
//! 7. `frequency_cap` and `quiet_hours`: stubs that pass; #221 fills them.
//! 8. `render`: the pool renders the template; any failure is `failed: render_error`.
//! 9. `rate_limit`: the destination is loaded again (an archived one is refused whatever a held
//!    block of tokens says) and a token is taken; with none, the send is queued again at the
//!    instant the bucket will hold one.
//! 10. `delivery`: the destination is called with the message id as idempotency key.
//!
//! **Idempotency.** The first run writes the `messages` row, `pending`, in a statement of its own
//! (so a retry that rolls the job's transaction back leaves it), and the message id is the
//! idempotency key on every attempt. A job that finds its message ended does nothing. Otherwise it
//! reads, renders and calls the destination holding no lock, then locks the person (`FOR KEY
//! SHARE`, skipped when the person was erased or merged in the meantime) and ends the row, with the
//! event and the decisions, in one go. A retry, a lost lease or a crash before that commit sends
//! again with the same key, which a destination that honours keys treats as the same message.
//!
//! **Retries.** A 408, a 429, a 5xx, a timeout or a transport error is retried with the worker's
//! backoff, or after the `Retry-After` the destination named when that is longer; on the job's
//! last attempt the message fails. A 429 also slows the destination's bucket (`throttle_bucket`).
//! An attempt row is recorded for every call, and counted after it commits.

mod deliver;
mod gates;
mod payload;
mod render;

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::Registry;
use zirv_flow_core::TraceContext;
use zirv_flow_core::consent::Dimensions;
use zirv_flow_store::{
    Admission, DELIVERY_UNCONFIRMED, Destination, FlowState, GateRecord, MessageStatus, NewJob,
    NewMessage, PgConnection, PgPool, SEND_JOB_KIND, SendOutcome, SubjectKind, WebhookPerson,
    WebhookSubject, document_sender, find_destination, find_flow, find_template,
    find_template_version, insert_message, lock_webhook_person, mark_message_sending,
    message_status, read_template_field_catalog, read_webhook_subject, record_send_outcome,
    throttle_bucket,
};

pub(crate) use payload::Queued;

use self::gates::Blocked;
use self::payload::SendJob;
use crate::delivery::attempt::AttemptTarget;
use crate::delivery::{self, Verdict};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct DeliverySend;

/// How long a send waits for a rate-limit token before it fails as `rate_limited`: a destination
/// that grants none for this long is not coming back for this message.
const MAX_RATE_WAIT: time::Duration = time::Duration::hours(6);

/// What a run of the job came to.
enum Step {
    /// The job is over and writes nothing: the message was dropped, or queued again.
    Over,
    /// The message ended; the outcome is written under the person's lock.
    Finished(Trail),
}

/// What the gates found on the way to the end of a message.
struct Trail {
    gates: Vec<GateRecord>,
    status: MessageStatus,
    reason: Option<&'static str>,
    rule: Option<&'static str>,
    template_version: Option<i32>,
    sender: Option<Uuid>,
    category: Option<String>,
    /// The delivery attempts the destination saw.
    attempts: i32,
}

impl Trail {
    fn new(attempts: i32) -> Self {
        Self {
            gates: Vec::new(),
            status: MessageStatus::Sent,
            reason: None,
            rule: None,
            template_version: None,
            sender: None,
            category: None,
            attempts,
        }
    }

    fn pass(&mut self, gate: GateRecord) {
        self.gates.push(gate);
    }

    /// Ends the message here, skipped by the gate that blocked it.
    fn skip(mut self, blocked: Blocked) -> Step {
        self.gates.push(blocked.gate);
        self.status = MessageStatus::Skipped;
        self.reason = blocked.gate.reason;
        self.rule = blocked.rule.or(self.rule);
        Step::Finished(self)
    }

    /// Ends the message here, failed at `gate` for `reason`.
    fn fail(mut self, gate: &'static str, reason: &'static str) -> Step {
        self.gates.push(GateRecord::blocked(gate, reason));
        self.status = MessageStatus::Failed;
        self.reason = Some(reason);
        Step::Finished(self)
    }
}

impl JobKind for DeliverySend {
    const KIND: &'static str = SEND_JOB_KIND;

    async fn run(
        &self,
        ctx: &mut JobContext<'_>,
        payload: serde_json::Value,
    ) -> Result<(), JobError> {
        let job = SendJob::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a message to send"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        // A message that ended and committed, and whose job ran again (a lease lost between the
        // commit and the acknowledgement): there is nothing left to do.
        let recorded = message_status(ctx.conn(), job.workspace, job.message).await?;
        if recorded.is_some_and(MessageStatus::has_ended) {
            return Ok(());
        }
        // A message that was being delivered when an earlier run failed may have gone out: it is
        // delivered again under the same key, without judging it by the gates once more.
        let resuming = recorded == Some(MessageStatus::Sending);
        match decide(ctx, &job, &payload, now, resuming).await? {
            Step::Over => Ok(()),
            Step::Finished(trail) => finish(ctx, &job, trail, now).await,
        }
    }
}

/// Runs the gates and, when they let the message through, delivers it.
async fn decide(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    payload: &serde_json::Value,
    now: OffsetDateTime,
    resuming: bool,
) -> Result<Step, JobError> {
    let workspace = job.workspace;
    let pool = ctx
        .pool()
        .cloned()
        .ok_or_else(|| JobError::new("the worker has no pool"))?;
    let mut trail = Trail::new(job.attempt_base + ctx.attempts() - 1);

    // Gate 1. Read with no lock: the delivery below can take as long as the destination does,
    // and nothing an ingest batch, a resume or an erasure needs may be held for that long.
    match find_flow(ctx.conn(), workspace, job.flow)
        .await?
        .map(|flow| flow.state)
    {
        Some(FlowState::Paused) => return stopped(ctx, job, trail, "flow_paused").await,
        Some(FlowState::Archived) | None => {
            return stopped(ctx, job, trail, "flow_inactive").await;
        }
        Some(FlowState::Draft | FlowState::Published) => {}
    }
    let catalog = read_template_field_catalog(ctx.conn(), workspace).await?;
    let keys: Vec<String> = catalog.traits.keys().cloned().collect();
    let Some(WebhookSubject { person, traits }) =
        read_webhook_subject(ctx.conn(), workspace, job.enrollment, &keys).await?
    else {
        // Erased, exited (a goal, an operator, a merge) or merged away: nobody to send to.
        return stopped(ctx, job, trail, "enrollment_ended").await;
    };
    if person != job.person {
        return stopped(ctx, job, trail, "enrollment_ended").await;
    }
    trail.pass(GateRecord::passed("enrollment"));
    // The first run writes the row, pending, in a statement of its own: it stays whatever this
    // attempt comes to (a retry, a wait for a token), and shows the message in the log meanwhile.
    start_message(&pool, job, now).await?;

    // The template, as published now.
    let template = find_template(ctx.conn(), workspace, job.template).await?;
    let version = match template.as_ref().and_then(|template| {
        (template.channel == "email" && template.archived_at.is_none())
            .then_some(template.current_version)
            .flatten()
    }) {
        Some(version) => {
            find_template_version(ctx.conn(), workspace, job.template, version).await?
        }
        None => None,
    };
    let (Some(template), Some(version)) = (template, version) else {
        return Ok(trail.fail("template", "template_unavailable"));
    };
    trail.pass(GateRecord::passed("template"));
    trail.template_version = Some(version.version);
    trail.category = Some(template.category.clone());
    let transactional = gates::is_transactional(&template.category);

    // The sender.
    let sender = match gates::sender(&pool, workspace, document_sender(&version.document)).await? {
        Ok(sender) => sender,
        Err(_) if resuming => return Ok(trail.fail("sender", DELIVERY_UNCONFIRMED)),
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.sender = Some(sender.identity_id);
    trail.pass(GateRecord::passed("sender"));

    // Gate 2: the address.
    let address = match gates::address(ctx.conn(), workspace, person).await? {
        Ok(address) => address,
        Err(_) if resuming => return Ok(trail.fail("address", DELIVERY_UNCONFIRMED)),
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.pass(GateRecord::passed("address"));

    // Gates 3 to 6 judged the message when it first went out. A retry of a message that may have
    // gone out does not judge it again: a person who withdrew consent since was sent this one.
    if !resuming
        && let Some(blocked) =
            person_gates(ctx, job, &pool, &address, transactional, &mut trail).await?
    {
        return Ok(trail.skip(blocked));
    }

    // Gate 7: render, in the pool.
    let context = render::context_of(job, &traits, &address);
    let pool_for_render = ctx.render_pool().cloned();
    let rendered = match render::render(pool_for_render.as_ref(), version.document, &context).await
    {
        Ok(rendered) => rendered,
        Err(reason) => return Ok(trail.fail("render", reason)),
    };
    trail.pass(GateRecord::passed("render"));
    let email = render::email_payload(&sender, &rendered);

    // Gate 8: the destination, loaded for this message, and a token from its bucket.
    if job
        .first_wait
        .is_some_and(|first| now - first > MAX_RATE_WAIT)
    {
        // A resumed message may have gone out on an earlier call: it is not `rate_limited`.
        let reason = if resuming {
            DELIVERY_UNCONFIRMED
        } else {
            "rate_limited"
        };
        return Ok(trail.fail("rate_limit", reason));
    }
    let Some(destination) = find_destination(ctx.conn(), workspace, job.destination)
        .await?
        .filter(|destination| destination.archived_at.is_none())
    else {
        return Ok(trail.fail("rate_limit", "destination_unavailable"));
    };
    let registry = Registry::built_in();
    let Some(driver) = registry.get(&destination.kind) else {
        return Ok(trail.fail("rate_limit", "destination_unavailable"));
    };
    if !driver.spec().capabilities.channels.contains(&"email") {
        return Ok(trail.fail("rate_limit", "destination_unsupported"));
    }
    let gates = Arc::clone(ctx.rate_gates());
    let clock = ctx.clock().clone();
    match gates.admit(&pool, destination.id, &clock).await? {
        Admission::Granted => trail.pass(GateRecord::passed("rate_limit")),
        Admission::Wait(wait) => return defer(ctx, job, payload, wait, now).await,
        Admission::Gone => return Ok(trail.fail("rate_limit", "destination_unavailable")),
    }

    // Gate 9: deliver, holding no lock, then record the attempt. The row is marked `sending`
    // first, in a statement of its own: from here on the message may go out, and a retry knows it.
    let attempts = ctx.attempts();
    let number = job.attempt_base + attempts;
    mark_message_sending(&pool, workspace, job.message, number - 1).await?;
    let call = deliver::Call {
        workspace,
        destination: &destination,
        driver,
        message: job.message,
        address: &address,
        payload: &email,
    };
    let done = deliver::deliver(ctx, &call, now).await?;
    let target = AttemptTarget {
        destination_id: destination.id,
        subject_kind: SubjectKind::Message,
        subject_id: job.message,
    };
    delivery::settle(ctx, Some(&target), number, &done).await?;
    trail.attempts = number;
    match done.verdict {
        Verdict::Succeeded { .. } => {
            trail.pass(GateRecord::passed("delivery"));
            Ok(Step::Finished(trail))
        }
        Verdict::Terminal { reason, .. } => Ok(trail.fail("delivery", reason)),
        Verdict::Retry {
            reason,
            message,
            retry_after,
            ..
        } => {
            warn!(attempt = number, reason, "message will be retried");
            if reason == "http_429" {
                slow_down(&pool, &destination, retry_after, ctx).await;
            }
            if attempts < ctx.max_attempts() {
                let error = JobError::new(message);
                return Err(retry_after.map_or(error, |wait| error.with_retry_after(wait)));
            }
            Ok(trail.fail("delivery", reason))
        }
    }
}

/// Gates 3 to 6: consent, suppression and the two caps. The gate that blocks, if one does.
async fn person_gates(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    pool: &PgPool,
    address: &str,
    transactional: bool,
    trail: &mut Trail,
) -> Result<Option<Blocked>, JobError> {
    match gates::consent(pool, job, transactional).await? {
        Ok(passed) => {
            trail.pass(passed.gate);
            trail.rule = passed.rule;
        }
        Err(blocked) => return Ok(Some(blocked)),
    }
    let key = ctx
        .identifier_key()
        .ok_or_else(|| JobError::new("the worker has no identifier key"))?;
    match gates::suppression(ctx.conn(), job.workspace, &key, address).await? {
        Ok(passed) => trail.pass(passed),
        Err(blocked) => return Ok(Some(blocked)),
    }
    // Gates 5 and 6, which #221 fills.
    trail.pass(gates::frequency_cap());
    trail.pass(gates::quiet_hours());
    Ok(None)
}

/// A destination answered 429: halve its bucket's rate until its `Retry-After` has passed (the
/// store caps the wait). A failure to record it is logged and does not change the job's result.
async fn slow_down(
    pool: &PgPool,
    destination: &Destination,
    retry_after: Option<Duration>,
    ctx: &JobContext<'_>,
) {
    if throttle_bucket(pool, destination.id, retry_after, ctx.clock())
        .await
        .is_err()
    {
        warn!(destination = %destination.id, "a destination's rate could not be slowed");
    }
}

/// Gate 1 stopped the message. On the first run nobody is waiting for it: it is dropped, and a
/// decision says so (ids only) with no row, event or attempt. A later run finds the row the first
/// one wrote, pending: that message ends as skipped, for the same reason, so no row stays pending.
async fn stopped(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    trail: Trail,
    reason: &'static str,
) -> Result<Step, JobError> {
    let recorded = message_status(ctx.conn(), job.workspace, job.message).await?;
    if recorded == Some(MessageStatus::Sending) {
        // It may have gone out already: it is not skipped, which would say it was not sent.
        return Ok(trail.fail("enrollment", DELIVERY_UNCONFIRMED));
    }
    if recorded == Some(MessageStatus::Pending) {
        return Ok(trail.skip(Blocked::new("enrollment", reason)));
    }
    dropped(ctx, job, reason).await
}

/// Writes the message `pending`, if it has no row yet. The statement is its own transaction on the
/// worker's pool, so the row outlives a job transaction that rolls back.
async fn start_message(pool: &PgPool, job: &SendJob, now: OffsetDateTime) -> Result<(), JobError> {
    let started = NewMessage {
        id: job.message,
        workspace: job.workspace,
        person: job.person,
        enrollment: job.enrollment,
        flow: Some(job.flow),
        flow_version: Some(job.version),
        node: &job.node,
        template: job.template,
        template_version: None,
        destination: job.destination,
        sender_identity: None,
        category: None,
        topic: job.topic,
        dimensions: &job.dimensions,
        status: MessageStatus::Pending,
        reason: None,
        deciding_rule: None,
        gates: &[],
        attempts: 0,
        now,
    };
    insert_message(pool, &started).await?;
    Ok(())
}

/// Gate 1 stopped the first run of the message: nobody is waiting for it. A decision says so (ids
/// only) and no row, event or attempt is written.
async fn dropped(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    reason: &'static str,
) -> Result<Step, JobError> {
    ctx.record_decision("message.send", "dropped", Some(reason), decision_ids(job))
        .await?;
    Ok(Step::Over)
}

/// The ids a send decision names, and nothing else.
fn decision_ids(job: &SendJob) -> Vec<(&'static str, Uuid)> {
    vec![
        ("message", job.message),
        ("enrollment", job.enrollment),
        ("template", job.template),
        ("destination", job.destination),
    ]
}

/// No token yet: this job ends and a copy of it runs at the instant the bucket will hold one. The
/// copy has its own unique key, since this job still holds the original while it runs, and its
/// attempts continue this job's (`attempt_base`, and a budget of what is left), so a destination
/// that keeps the send waiting cannot buy it more attempts than the job had.
async fn defer(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    payload: &serde_json::Value,
    wait: Duration,
    now: OffsetDateTime,
) -> Result<Step, JobError> {
    let attempts = ctx.attempts();
    let mut next = NewJob::new(SEND_JOB_KIND, ctx.trace().clone());
    next.workspace_id = Some(job.workspace);
    next.payload = job.deferred(payload, attempts, now);
    next.run_at = Some(now + wait);
    next.max_attempts = (ctx.max_attempts() - (attempts - 1)).max(1);
    next.unique_key = Some(format!("{}:defer:{}", job.key, job.deferrals + 1));
    ctx.enqueue(&next).await?;
    ctx.record_decision(
        "message.send",
        "deferred",
        Some("rate_limit"),
        decision_ids(job),
    )
    .await?;
    Ok(Step::Over)
}

/// Writes the end of the message: locks the person for the short write, and ends the row, with the
/// event and the decisions.
///
/// A person erased while the message was being read, rendered and delivered has no row left (the
/// erasure deleted it) and nothing is written. A person merged away meanwhile keeps the message
/// on the id it was sent to, as every message does, so the row is ended; the event and the
/// decisions are not written, since the merge's rewrite has passed the id they would sit on.
async fn finish(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    trail: Trail,
    now: OffsetDateTime,
) -> Result<(), JobError> {
    let trace = ctx.trace().clone();
    let job_ref = (ctx.job_id(), ctx.attempts());
    write_end(ctx.conn(), &trace, job_ref, job, trail, now).await
}

/// [`finish`] on a connection: locks the person and ends the row.
async fn write_end(
    connection: &mut PgConnection,
    trace: &TraceContext,
    job_ref: (Uuid, i32),
    job: &SendJob,
    trail: Trail,
    now: OffsetDateTime,
) -> Result<(), JobError> {
    let held =
        match lock_webhook_person(&mut *connection, job.workspace, job.enrollment, job.person)
            .await?
        {
            WebhookPerson::Held => true,
            WebhookPerson::Moved => false,
            WebhookPerson::Gone => return Ok(()),
        };
    let dimensions: &Dimensions = &job.dimensions;
    let message = NewMessage {
        id: job.message,
        workspace: job.workspace,
        person: job.person,
        enrollment: job.enrollment,
        flow: Some(job.flow),
        flow_version: Some(job.version),
        node: &job.node,
        template: job.template,
        template_version: trail.template_version,
        destination: job.destination,
        sender_identity: trail.sender,
        category: trail.category.as_deref(),
        topic: job.topic,
        dimensions,
        status: trail.status,
        reason: trail.reason,
        deciding_rule: trail.rule,
        gates: &trail.gates,
        attempts: trail.attempts,
        now,
    };
    if !held {
        insert_message(&mut *connection, &message).await?;
        return Ok(());
    }
    record_send_outcome(
        connection,
        &SendOutcome {
            message,
            trace,
            job: Some(job_ref),
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::Duration as Span;
    use time::macros::datetime;
    use zirv_flow_connectors::destinations::{fake_ledger_calls, fake_ledger_sent};
    use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
    use zirv_flow_core::egress::EgressPolicy;
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceId, WorkspaceName,
    };
    use zirv_flow_render::{ChildCommand, PoolConfig, RenderPool};
    use zirv_flow_store::sending_fixtures::{plant_domain, plant_identity, plant_verified_sender};
    use zirv_flow_store::{
        ClaimedJob, Clock, ConsentChange, EnrollRequest, Enrolled, ManualClock, NewDestination,
        NewFlow, NewSuppression, NewTemplate, Occurrence, ProfileRecord,
        add_organization_suppression, add_suppression, create_destination, create_template,
        end_orphaned_messages, lock_template, publish_template, set_consent,
    };

    use super::*;
    use crate::context::IngestSettings;
    use crate::delivery::rate::RateGates;

    // The render host is this test binary run again, filtered to `render_host_child`, which does
    // nothing unless the pool started it.

    /// What the child does: `serve` is a render host, `oom_once` dies of an allocation failure on
    /// its first request (the marker file says whether this is the first child) and serves after.
    #[test]
    fn render_host_child() {
        let Ok(mode) = std::env::var("ZIRV_FLOW_TEST_SEND_RENDER_HOST") else {
            return;
        };
        if mode == "oom_once" {
            let marker = std::env::var("ZIRV_FLOW_TEST_SEND_RENDER_MARKER").unwrap_or_default();
            if !std::path::Path::new(&marker).exists() {
                std::fs::write(&marker, b"x").expect("the marker can be written");
                let ready = br#"{"kind":"ready"}"#;
                let mut stdout = std::io::stdout().lock();
                stdout
                    .write_all(&u32::try_from(ready.len()).unwrap().to_be_bytes())
                    .unwrap();
                stdout.write_all(ready).unwrap();
                stdout.flush().unwrap();
                let mut stdin = std::io::stdin().lock();
                let mut length = [0_u8; 4];
                stdin.read_exact(&mut length).unwrap();
                let mut request = vec![0; u32::from_be_bytes(length) as usize];
                stdin.read_exact(&mut request).unwrap();
                eprintln!("memory allocation of 8589934592 bytes failed");
                std::process::abort();
            }
        }
        // The production default limit, so a runaway template fails in the child, not the runner.
        let limit = 512 * 1024 * 1024;
        std::process::exit(i32::from(zirv_flow_render::host::run(Some(limit))));
    }

    async fn start_pool(mode: &str, marker: &str) -> RenderPool {
        let command = ChildCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kinds::delivery_send::tests::render_host_child",
                "--nocapture",
                "--test-threads",
                "1",
                "-q",
            ])
            .env("ZIRV_FLOW_TEST_SEND_RENDER_HOST", mode)
            .env("ZIRV_FLOW_TEST_SEND_RENDER_MARKER", marker)
            .skip_stdout_until("running 1 test\n");
        let config = PoolConfig {
            size: 1,
            deadline: Duration::from_secs(60),
        };
        RenderPool::start(command, config).await.unwrap()
    }

    fn loopback() -> Arc<EgressPolicy> {
        Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap())
    }

    fn identifier_key() -> Arc<zirv_flow_core::HmacKey> {
        let master = MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap();
        Arc::new(HmacRoot::bootstrap_from(&master).hmac_key(HmacPurpose::Identifier))
    }

    /// One workspace with a verified sender, a fake destination that keeps a ledger, a published
    /// flow, and a worker that runs `delivery.send` jobs by hand.
    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        clock: ManualClock,
        gates: Arc<RateGates>,
        render: Option<RenderPool>,
        /// A verified sender identity.
        sender: Uuid,
        destination: Uuid,
        ledger: String,
        flow: Uuid,
        version: i32,
        /// The worker has no identifier key, so a send fails after its row was written.
        no_key: bool,
        /// The master keys the worker opens credentials with.
        keyring: Option<Arc<zirv_flow_core::Keyring>>,
    }

    /// A message queued: its id, and the enrollment it is for.
    struct Queued1 {
        message: Uuid,
        enrolled: Enrolled,
    }

    impl Env {
        async fn new(pool: PgPool, name: &str) -> Self {
            Self::with(pool, name, json!({"mode": "ok"}), 100, 100).await
        }

        async fn with(pool: PgPool, name: &str, config: Value, rate: i32, burst: i32) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &WorkspaceName::parse(name).unwrap(),
                &Slug::parse(name).unwrap(),
            )
            .await
            .unwrap()
            .id;
            let sender = plant_verified_sender(&pool, ws, "example.com")
                .await
                .unwrap();
            let ledger = format!("send-{name}-{}", Uuid::now_v7());
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
                    rate,
                    burst,
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .id;
            let mut env = Self {
                pool,
                ws,
                clock: ManualClock::new(datetime!(2031-05-06 07:08:09 UTC)),
                gates: Arc::default(),
                render: None,
                sender,
                destination,
                ledger,
                flow: Uuid::now_v7(),
                no_key: false,
                keyring: None,
                version: 1,
            };
            env.publish_flow().await;
            env
        }

        async fn publish_flow(&mut self) {
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id: self.flow,
                    name: "Mail".to_owned(),
                    graph: json!({"v": 1, "nodes": [{"id": "mail", "type": "exit"}], "edges": []}),
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "always"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, self.flow)
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
            self.version = version.version;
        }

        /// A person with an email identity and the traits given.
        async fn person(&self, email: &str) -> Uuid {
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: email.into(),
                identifiers: vec![("email".into(), email.into())],
                traits: serde_json::Map::new(),
                observed_at: self.clock.now() - Span::days(1),
                trace: TraceContext::root(Cause::api("t")),
            };
            self.profile(record).await
        }

        /// A person who has a phone number and no email address.
        async fn person_without_address(&self) -> Uuid {
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: "phone-only".into(),
                identifiers: vec![("phone".into(), "+4512345678".into())],
                traits: serde_json::Map::new(),
                observed_at: self.clock.now() - Span::days(1),
                trace: TraceContext::root(Cause::api("t")),
            };
            self.profile(record).await
        }

        async fn profile(&self, record: ProfileRecord) -> Uuid {
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

        /// The person subscribes to email as a whole.
        async fn subscribe(&self, person: Uuid) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            set_consent(
                &mut tx,
                self.ws,
                &ConsentChange {
                    dimensions: Default::default(),
                    person,
                    channel: Channel::Email,
                    topic: None,
                    status: ConsentStatus::Subscribed,
                    source: ChangeSource::Api,
                    evidence: json!({}),
                    occurred_at: self.clock.now() - Span::hours(1),
                    actor: None,
                },
                &TraceContext::root(Cause::api("t")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// A published email template of `category` from the verified sender.
        async fn template(&self, category: &str, html: &str) -> Uuid {
            self.template_from(self.sender, category, html).await
        }

        async fn template_from(&self, sender: Uuid, category: &str, html: &str) -> Uuid {
            let document = json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Hello", "sender": sender, "lang": "en", "category": category,
                "html": html, "text": "Hello {{ system.unsubscribe_url }}",
            }});
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            create_template(
                &mut connection,
                self.ws,
                &NewTemplate {
                    id,
                    name: format!("T {id}"),
                    channel: "email".into(),
                    category: category.into(),
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

        /// A template whose HTML is fine.
        async fn plain_template(&self, category: &str) -> Uuid {
            self.template(category, PLAIN).await
        }

        /// Enrolls the person; an enrollment of theirs still in flight has ended, as a person who
        /// is sent a second message by the same flow has finished the first walk.
        async fn enroll(&self, person: Uuid) -> Enrolled {
            sqlx::query(
                "UPDATE enrollments SET state = 'completed', ended_at = now() WHERE flow_id = $1 AND person_id = $2 AND state IN ('active', 'waiting')",
            )
            .bind(self.flow)
            .bind(person)
            .execute(&self.pool)
            .await
            .unwrap();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let outcome = zirv_flow_store::enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: self.ws,
                    flow: self.flow,
                    version: self.version,
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

        /// Queues the job `flow.step` would for `person`, to the env's destination.
        async fn queue(&self, person: Uuid, template: Uuid, max_attempts: i32) -> Queued1 {
            self.queue_to(person, template, self.destination, None, max_attempts)
                .await
        }

        async fn queue_to(
            &self,
            person: Uuid,
            template: Uuid,
            destination: Uuid,
            topic: Option<Uuid>,
            max_attempts: i32,
        ) -> Queued1 {
            let enrolled = self.enroll(person).await;
            let message = Uuid::now_v7();
            let key = format!("{}:mail:send", enrolled.enrollment);
            let dimensions = Dimensions::new();
            let mut job = NewJob::new(SEND_JOB_KIND, enrolled.trace.clone());
            job.workspace_id = Some(self.ws);
            job.unique_key = Some(key.clone());
            job.max_attempts = max_attempts;
            job.payload = Queued {
                workspace: self.ws,
                message,
                key: &key,
                flow: self.flow,
                version: self.version,
                node: "mail",
                enrollment: enrolled.enrollment,
                person,
                template,
                destination,
                topic,
                dimensions: &dimensions,
                queued_at: self.clock.now(),
            }
            .payload()
            .unwrap();
            zirv_flow_store::enqueue(&self.pool, &job).await.unwrap();
            Queued1 { message, enrolled }
        }

        async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[SEND_JOB_KIND],
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
                    .with_ingest(IngestSettings {
                        pool: self.pool.clone(),
                        redaction_key: None,
                        batch_size: 1,
                        shards: 1,
                    })
                    .with_identifier_key((!self.no_key).then(identifier_key))
                    .with_render_pool(self.render.clone())
                    .with_rate_gates(Arc::clone(&self.gates))
                    .with_outbound(self.keyring.clone(), loopback(), Arc::default());
                DeliverySend.run(&mut ctx, claimed.payload.clone()).await
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

        /// Claims the next job and runs one attempt of it.
        async fn attempt(&self) -> Result<(), JobError> {
            let claimed = self.claim().await.expect("a delivery.send job is ready");
            self.execute(claimed).await
        }

        /// Queues a send and runs it to its end; the message it recorded.
        async fn send(&self, person: Uuid, template: Uuid) -> zirv_flow_store::Message {
            let queued = self.queue(person, template, 5).await;
            self.attempt().await.unwrap();
            self.message(queued.message).await
        }

        async fn message(&self, id: Uuid) -> zirv_flow_store::Message {
            zirv_flow_store::find_message(&self.pool, self.ws, id)
                .await
                .unwrap()
                .expect("the message was recorded")
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

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

        /// The (action, outcome, reason) of every decision of the workspace for the actions.
        async fn decisions(&self, actions: &[&str]) -> Vec<(String, String, Option<String>)> {
            let actions: Vec<String> = actions.iter().map(|action| (*action).to_owned()).collect();
            sqlx::query_as(
                "SELECT action, outcome, reason FROM decision_records \
                 WHERE workspace_id = $1 AND action = ANY($2) ORDER BY recorded_at, id",
            )
            .bind(self.ws.as_uuid())
            .bind(&actions)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn attempts_of(&self, message: Uuid) -> Vec<(String, i32)> {
            sqlx::query_as(
                "SELECT outcome, attempt FROM delivery_attempts WHERE subject_id = $1 \
                 ORDER BY attempt",
            )
            .bind(message)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        fn sent(&self) -> Vec<zirv_flow_connectors::destinations::FakeSent> {
            fake_ledger_sent(&self.ledger)
        }
    }

    const PLAIN: &str =
        "<p>Hello {{ person.email }}</p><a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a>";

    fn gate_names(message: &zirv_flow_store::Message) -> Vec<String> {
        message
            .gate_results
            .as_array()
            .unwrap()
            .iter()
            .map(|gate| {
                format!(
                    "{}:{}",
                    gate["gate"].as_str().unwrap(),
                    gate["result"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_passes_every_gate_and_is_sent_once_with_its_log_event_and_decisions(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.plain_template("marketing").await;
        let queued = env.queue(person, template, 5).await;
        let waiting = zirv_flow_store::destination_report(&env.pool)
            .await
            .unwrap();
        assert_eq!(
            waiting
                .iter()
                .find(|stat| stat.destination_id == env.destination)
                .map(|stat| stat.queue_depth),
            Some(1),
            "the queued send is the destination's queue depth"
        );

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "sent");
        assert_eq!(message.reason, None);
        assert_eq!(message.attempts, 1);
        assert_eq!(message.person_id, queued.enrolled.person);
        assert_eq!(message.template_version, Some(1));
        assert_eq!(message.sender_identity_id, Some(env.sender));
        assert_eq!(message.category.as_deref(), Some("marketing"));
        assert_eq!(message.deciding_rule.as_deref(), Some("recorded_consent"));
        assert_eq!(
            gate_names(&message),
            [
                "enrollment:passed",
                "template:passed",
                "sender:passed",
                "address:passed",
                "consent:passed",
                "suppression:passed",
                "frequency_cap:passed",
                "quiet_hours:passed",
                "render:passed",
                "rate_limit:passed",
                "delivery:passed",
            ]
        );
        let sent = env.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].key,
            queued.message.to_string(),
            "the message id is the key"
        );
        assert_eq!(sent[0].to, "ada@example.com");
        assert_eq!(sent[0].payload["from"]["address"], "news@example.com");
        assert_eq!(sent[0].payload["subject"], "Hello");
        assert!(
            sent[0].payload["html"]
                .as_str()
                .unwrap()
                .contains("Hello ada@example.com"),
            "{}",
            sent[0].payload
        );
        assert!(
            sent[0].payload["headers"]["List-Unsubscribe"].is_string(),
            "marketing mail carries the unsubscribe header"
        );
        let events = env.events("email.sent").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["message_id"], json!(queued.message));
        assert!(!events[0].to_string().contains('@'), "{}", events[0]);
        assert_eq!(
            env.attempts_of(queued.message).await,
            [("delivered".to_owned(), 1)]
        );
        let trail = env.decisions(&["send.gate", "message.send"]).await;
        assert_eq!(trail.len(), 12, "{trail:?}");
        assert_eq!(
            trail.last().unwrap(),
            &("message.send".to_owned(), "sent".to_owned(), None)
        );
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'delivery.send'")
                .await,
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_gate_stops_a_message_with_its_own_reason_and_stores_it_exactly(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let ok_template = env.plain_template("marketing").await;

        // no_address: a person with a phone number only.
        let nobody = env.person_without_address().await;
        env.subscribe(nobody).await;
        let no_address = env.send(nobody, ok_template).await;

        // no_consent: an address and no recorded consent under the opt-in default.
        let silent = env.person("silent@example.com").await;
        let no_consent = env.send(silent, ok_template).await;

        // suppressed: the address is suppressed in the workspace.
        let blocked = env.person("blocked@example.com").await;
        env.subscribe(blocked).await;
        add_suppression(
            &env.pool,
            env.ws,
            &NewSuppression {
                identifier_hash: crate_hash("blocked@example.com"),
                kind: "email",
                reason: zirv_flow_core::consent::SuppressionReason::Manual,
                source: "api",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let suppressed = env.send(blocked, ok_template).await;

        // sender_unverified: the template names a sender on a domain whose records are not in place.
        let domain = plant_domain(&env.pool, env.ws, "unverified.example")
            .await
            .unwrap();
        let identity = plant_identity(&env.pool, env.ws, domain, "news@unverified.example")
            .await
            .unwrap();
        let unverified_template = env.template_from(identity, "marketing", PLAIN).await;
        let fine = env.person("fine@example.com").await;
        env.subscribe(fine).await;
        let sender_unverified = env.send(fine, unverified_template).await;

        // render_error: strict undefined in the template.
        let broken = env
            .template(
                "marketing",
                "<a href=\"{{ system.unsubscribe_url }}\">{{ person.nothing_here }}</a>",
            )
            .await;
        let render_error = env.send(fine, broken).await;

        let table = [
            (&no_address, "skipped", "no_address", "address:blocked"),
            (&no_consent, "skipped", "no_consent", "consent:blocked"),
            (&suppressed, "skipped", "suppressed", "suppression:blocked"),
            (
                &sender_unverified,
                "skipped",
                "sender_unverified",
                "sender:blocked",
            ),
            (&render_error, "failed", "render_error", "render:blocked"),
        ];
        for (message, status, reason, last_gate) in table {
            assert_eq!(message.status, status, "{reason}");
            assert_eq!(message.reason.as_deref(), Some(reason));
            assert_eq!(
                gate_names(message).last().map(String::as_str),
                Some(last_gate)
            );
        }
        assert_eq!(no_consent.deciding_rule.as_deref(), Some("topic_default"));
        assert_eq!(
            suppressed.deciding_rule.as_deref(),
            Some("suppression_workspace")
        );
        assert!(
            env.sent().is_empty(),
            "no skipped or failed message reached the destination"
        );
        assert_eq!(env.events("email.skipped").await.len(), 4);
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert_eq!(env.events("email.sent").await.len(), 0);
        let reasons: Vec<Option<String>> = env
            .decisions(&["message.send"])
            .await
            .into_iter()
            .map(|(_, _, reason)| reason)
            .collect();
        assert_eq!(
            reasons,
            [
                Some("no_address".into()),
                Some("no_consent".into()),
                Some("suppressed".into()),
                Some("sender_unverified".into()),
                Some("render_error".into()),
            ]
        );
    }

    /// The suppression hash of an address, as the pipeline takes it.
    fn crate_hash(address: &str) -> [u8; 32] {
        zirv_flow_core::people::identifier_hash(
            &identifier_key(),
            &zirv_flow_core::people::Identifier::parse("email", address).unwrap(),
        )
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retried_send_reuses_the_message_id_and_a_destination_that_honours_keys_sees_one_message(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "lose_first_answer"}), 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.plain_template("marketing").await;
        let queued = env.queue(person, template, 5).await;

        // The first attempt reaches the destination, which accepts the message; the answer is a
        // 503 and the job is retried.
        let first = env.attempt().await.unwrap_err();
        assert_eq!(
            first.message(),
            "the destination answered with a server error"
        );
        let pending = env.message(queued.message).await;
        assert_eq!(
            (pending.status.as_str(), pending.completed_at),
            ("sending", None),
            "the row is marked before the call and the retry left it"
        );
        assert_eq!(env.events("email.sent").await.len(), 0);
        // The retry sends again, under the same key, and succeeds.
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!((message.status.as_str(), message.attempts), ("sent", 2));
        let key = queued.message.to_string();
        assert_eq!(fake_ledger_calls(&env.ledger), [key.clone(), key.clone()]);
        assert_eq!(
            env.sent().len(),
            1,
            "zero duplicates: the destination accepted one message"
        );
        assert_eq!(env.sent()[0].key, key);
        assert_eq!(
            env.attempts_of(queued.message).await,
            [("retryable".to_owned(), 1), ("delivered".to_owned(), 2)]
        );
        assert_eq!(env.events("email.sent").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_that_runs_again_after_its_outcome_committed_sends_nothing(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.plain_template("marketing").await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap();
        assert_eq!(env.sent().len(), 1);

        // The lease was lost after the commit and the job comes back: same payload.
        let mut again = NewJob::new(SEND_JOB_KIND, queued.enrolled.trace.clone());
        again.workspace_id = Some(env.ws);
        again.payload = {
            let dimensions = Dimensions::new();
            Queued {
                workspace: env.ws,
                message: queued.message,
                key: "again",
                flow: env.flow,
                version: env.version,
                node: "mail",
                enrollment: queued.enrolled.enrollment,
                person,
                template,
                destination: env.destination,
                topic: None,
                dimensions: &dimensions,
                queued_at: env.clock.now(),
            }
            .payload()
            .unwrap()
        };
        zirv_flow_store::enqueue(&env.pool, &again).await.unwrap();
        env.attempt().await.unwrap();

        assert_eq!(env.sent().len(), 1);
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1);
        assert_eq!(env.count("SELECT count(*) FROM messages").await, 1);
        assert_eq!(env.events("email.sent").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transactional_send_skips_consent_but_not_suppression(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        // Nobody has subscribed to anything.
        let receipt = env.person("receipt@example.com").await;
        let sent = env.send(receipt, template).await;
        let blocked = env.person("blocked@example.com").await;
        add_suppression(
            &env.pool,
            env.ws,
            &NewSuppression {
                identifier_hash: crate_hash("blocked@example.com"),
                kind: "email",
                reason: zirv_flow_core::consent::SuppressionReason::Complaint,
                source: "api",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let skipped = env.send(blocked, template).await;
        // The same two people through a marketing template: neither has consented.
        let marketing = env.plain_template("marketing").await;
        let refused = env.send(receipt, marketing).await;

        assert_eq!(sent.status, "sent");
        assert_eq!(sent.deciding_rule.as_deref(), Some("transactional"));
        assert_eq!(skipped.status, "skipped");
        assert_eq!(skipped.reason.as_deref(), Some("suppressed"));
        assert_eq!(refused.reason.as_deref(), Some("no_consent"));
        assert_eq!(env.sent().len(), 1);
        assert!(
            !env.sent()[0].payload["headers"]
                .as_object()
                .unwrap()
                .contains_key("List-Unsubscribe"),
            "transactional mail has no unsubscribe header"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_organization_wide_suppression_blocks_a_send_from_another_workspace(pool: PgPool) {
        let mut one = Env::new(pool.clone(), "one").await;
        let mut two = Env::new(pool, "two").await;
        for env in [&mut one, &mut two] {
            env.render = Some(start_pool("serve", "").await);
        }
        // A hard bounce in workspace one suppresses the address for the whole organization.
        add_organization_suppression(
            &one.pool,
            &NewSuppression {
                identifier_hash: crate_hash("gone@example.com"),
                kind: "email",
                reason: zirv_flow_core::consent::SuppressionReason::HardBounce,
                source: "bounce",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let template = two.plain_template("transactional").await;
        let person = two.person("gone@example.com").await;

        let message = two.send(person, template).await;

        assert_eq!(message.status, "skipped");
        assert_eq!(message.reason.as_deref(), Some("suppressed"));
        assert_eq!(
            message.deciding_rule.as_deref(),
            Some("suppression_organization")
        );
        assert!(two.sent().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_destination_is_refused_even_while_a_token_block_is_held(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let first = env.person("first@example.com").await;
        let second = env.person("second@example.com").await;

        // The first send takes a block of tokens from the bucket and keeps it in the gate.
        let sent = env.send(first, template).await;
        assert_eq!(sent.status, "sent");
        zirv_flow_store::archive_destination(&env.pool, env.ws, env.destination)
            .await
            .unwrap();
        let refused = env.send(second, template).await;

        assert_eq!(refused.status, "failed");
        assert_eq!(refused.reason.as_deref(), Some("destination_unavailable"));
        assert_eq!(
            fake_ledger_calls(&env.ledger).len(),
            1,
            "the destination was not called"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_429_with_a_retry_after_slows_the_bucket_and_asks_the_worker_to_wait(pool: PgPool) {
        let mut env = Env::with(pool, "one", json!({"mode": "throttle_retry_after"}), 10, 10).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;

        let error = env.attempt().await.unwrap_err();

        assert_eq!(error.retry_after(), Some(Duration::from_secs(120)));
        let bucket = zirv_flow_store::bucket_state(&env.pool, env.destination)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bucket.rate, 5.0, "the rate is halved");
        assert_eq!(
            bucket.penalty_until,
            Some(env.clock.now() + Span::seconds(120)),
            "until the Retry-After has passed"
        );
        assert_eq!(
            env.attempts_of(queued.message).await,
            [("throttled".to_owned(), 1)]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_that_finds_no_token_runs_again_when_the_bucket_will_hold_one(pool: PgPool) {
        // One token a second and a burst of one.
        let mut env = Env::with(pool, "one", json!({"mode": "ok"}), 1, 1).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let first = env.person("first@example.com").await;
        let second = env.person("second@example.com").await;
        assert_eq!(env.send(first, template).await.status, "sent");
        let queued = env.queue(second, template, 5).await;

        env.attempt().await.unwrap();

        // No event, no call: the job was replaced by one that runs after the wait, and the message
        // waits, pending.
        assert_eq!(env.count("SELECT count(*) FROM messages").await, 2);
        assert_eq!(env.message(queued.message).await.status, "pending");
        assert_eq!(env.events("email.sent").await.len(), 1);
        assert_eq!(env.sent().len(), 1);
        let next: Vec<(Option<String>, Value, bool)> = sqlx::query_as(
            "SELECT unique_key, payload, run_at > $1 FROM jobs WHERE kind = 'delivery.send'",
        )
        .bind(env.clock.now())
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(next.len(), 1);
        assert!(
            next[0]
                .0
                .as_deref()
                .unwrap()
                .ends_with(":mail:send:defer:1")
        );
        assert_eq!(next[0].1["deferrals"], 1);
        assert_eq!(next[0].1["message_id"], json!(queued.message));
        assert!(next[0].2, "it runs later than now");
        assert_eq!(
            env.decisions(&["message.send"]).await.last().unwrap(),
            &(
                "message.send".to_owned(),
                "deferred".to_owned(),
                Some("rate_limit".to_owned())
            )
        );

        // The bucket refills; the deferred job sends the same message.
        env.clock.advance(Span::seconds(5));
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'delivery.send'")
            .execute(&env.pool)
            .await
            .unwrap();
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "sent");
        assert_eq!(env.sent().len(), 2);
        assert_eq!(env.sent()[1].key, queued.message.to_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_for_an_ended_enrollment_or_a_paused_flow_is_dropped_with_a_decision_only(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        env.queue(person, template, 5).await;
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(env.flow)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        assert_eq!(env.count("SELECT count(*) FROM messages").await, 0);
        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'email.%'")
                .await,
            0
        );
        assert!(env.sent().is_empty());
        assert_eq!(
            env.decisions(&["message.send"]).await,
            [(
                "message.send".to_owned(),
                "dropped".to_owned(),
                Some("flow_paused".to_owned())
            )]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_that_may_have_gone_out_is_not_skipped_when_its_flow_is_paused_before_the_retry(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "lose_first_answer"}), 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(env.flow)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(
            fake_ledger_calls(&env.ledger).len(),
            1,
            "no second call was made"
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert_eq!(env.events("email.skipped").await.len(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_email_that_exhausts_the_render_hosts_memory_fails_as_a_render_error_and_the_next_is_sent(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        let marker = std::env::temp_dir().join(format!("zirv-send-oom-{}", Uuid::now_v7()));
        env.render = Some(start_pool("oom_once", marker.to_str().unwrap()).await);
        let template = env.plain_template("transactional").await;
        let first = env.person("first@example.com").await;
        let second = env.person("second@example.com").await;
        let queued_first = env.queue(first, template, 5).await;
        let queued_second = env.queue(second, template, 5).await;

        // Both jobs run on the same worker: the first meets the child that dies of an
        // allocation failure, the second the child the pool started in its place.
        env.attempt().await.unwrap();
        env.attempt().await.unwrap();

        let failed = env.message(queued_first.message).await;
        assert_eq!(
            (failed.status.as_str(), failed.reason.as_deref()),
            ("failed", Some("render_error"))
        );
        let sent = env.message(queued_second.message).await;
        assert_eq!(sent.status, "sent");
        assert_eq!(env.sent().len(), 1);
        assert_eq!(env.sent()[0].to, "second@example.com");
        assert_eq!(env.events("email.failed").await.len(), 1);
        let _ = std::fs::remove_file(marker);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_that_doubles_a_string_fails_as_a_render_error_and_the_next_is_sent(
        pool: PgPool,
    ) {
        if !cfg!(target_os = "linux") {
            eprintln!("skipped: the host's OS memory limit is applied on Linux only");
            return;
        }
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let doubling = env
            .template(
                "transactional",
                "{% set ns = namespace(s='xxxxxxxx') %}{% for i in range(60) %}{% set ns.s = ns.s ~ ns.s %}{% endfor %}{{ ns.s | length }}",
            )
            .await;
        let plain = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;

        let failed = env.send(person, doubling).await;
        let sent = env.send(env.person("bob@example.com").await, plain).await;

        assert_eq!(failed.reason.as_deref(), Some("render_error"));
        assert_eq!(sent.status, "sent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_message_whose_person_was_merged_away_while_it_was_sent_is_ended_without_an_event(
        pool: PgPool,
    ) {
        let env = Env::new(pool, "one").await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let survivor = env.person("survivor@example.com").await;
        let queued = env.queue(person, template, 5).await;
        let claimed = env.claim().await.expect("the send is ready");
        let job = SendJob::parse(&claimed.payload).unwrap();
        let now = env.clock.now();
        start_message(&env.pool, &job, now).await.unwrap();
        // The merge commits while the message is on the wire.
        sqlx::query("UPDATE people SET merged_into = $1 WHERE id = $2")
            .bind(survivor)
            .bind(person)
            .execute(&env.pool)
            .await
            .unwrap();
        let mut trail = Trail::new(1);
        trail.pass(GateRecord::passed("delivery"));

        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        {
            let mut ctx =
                JobContext::new(&mut tx, &claimed).with_clock(Clock::Manual(env.clock.clone()));
            finish(&mut ctx, &job, trail, now).await.unwrap();
        }
        zirv_flow_store::commit(tx).await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            message.status, "sent",
            "the message was sent, and the log says so"
        );
        assert_eq!(
            message.person_id, person,
            "it stays on the id it was sent to"
        );
        assert!(env.events("email.sent").await.is_empty());
        assert!(
            env.decisions(&["send.gate", "message.send"])
                .await
                .is_empty()
        );
    }

    /// Makes every `delivery.send` job dead, as the reaper does after its last lease expiry.
    async fn bury_the_jobs(env: &Env) {
        sqlx::query("UPDATE jobs SET state = 'dead' WHERE kind = 'delivery.send'")
            .execute(&env.pool)
            .await
            .unwrap();
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_opens_the_workspace_credential_its_destination_links(pool: PgPool) {
        use zirv_flow_core::{Keyring, MasterKey, SecretId, SecretPurpose, seal, secret_aad};
        use zirv_flow_store::secrets::insert_unchecked;
        use zirv_flow_store::{NewWorkspaceCredential, create_credential};

        let config = json!({"mode": "ok", "require_token": "s3cret"});
        let mut env = Env::with(pool, "one", config, 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        env.keyring = Some(Arc::clone(&keys));
        let secret = SecretId::new();
        let purpose = SecretPurpose::WorkspaceCredential;
        let sealed = seal(
            &keys,
            br#"{"token":"s3cret"}"#,
            &secret_aad(env.ws, secret, purpose),
        );
        insert_unchecked(&env.pool, env.ws, purpose, secret, &sealed)
            .await
            .unwrap();
        let credential = create_credential(
            &env.pool,
            env.ws,
            &NewWorkspaceCredential {
                id: Uuid::now_v7(),
                key: "mail",
                name: "Mail",
                kind: "custom",
                schema: None,
                fields: &json!({}),
                secret_id: secret,
                hints: &json!({"token": ""}),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE destinations SET credential_id = $1 WHERE id = $2")
            .bind(credential.id)
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;

        let sent = env.send(person, template).await;

        assert_eq!(sent.status, "sent");
        assert_eq!(env.sent().len(), 1, "the destination saw the token");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_ends_a_pending_message_whose_job_is_dead_as_an_internal_error(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.no_key = true;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 1).await;
        env.attempt().await.unwrap_err();
        bury_the_jobs(&env).await;
        assert_eq!(env.message(queued.message).await.status, "pending");

        let ended = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        assert_eq!(ended, [queued.message]);
        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("internal_error"))
        );
        assert!(message.completed_at.is_some());
        let events = env.events("email.failed").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["reason"], "internal_error");
        assert_eq!(
            env.decisions(&["message.send"]).await.last().unwrap(),
            &(
                "message.send".to_owned(),
                "failed".to_owned(),
                Some("internal_error".to_owned())
            )
        );
        assert!(env.sent().is_empty());
        let again = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();
        assert!(again.is_empty(), "an ended message is not ended twice");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_ends_a_sending_message_whose_job_is_dead_as_delivery_unconfirmed(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "lose_first_answer"}), 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        bury_the_jobs(&env).await;

        let ended = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        assert_eq!(ended, [queued.message]);
        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_leaves_a_message_open_while_its_job_is_ready_or_running(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.no_key = true;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap_err();
        let now = env.clock.now();

        // Ready, waiting for its retry.
        assert!(
            end_orphaned_messages(&env.pool, now)
                .await
                .unwrap()
                .is_empty()
        );
        // Running with a lease that has expired: live until the reaper buries it.
        sqlx::query(
            "UPDATE jobs SET state = 'running', lease_token = gen_random_uuid(), \
                lease_expires_at = now() - interval '1 hour' WHERE kind = 'delivery.send'",
        )
        .execute(&env.pool)
        .await
        .unwrap();
        assert!(
            end_orphaned_messages(&env.pool, now)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(env.message(queued.message).await.status, "pending");
        assert_eq!(env.events("email.failed").await.len(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_leaves_a_message_open_while_a_deferral_copy_is_ready(pool: PgPool) {
        let mut env = Env::with(pool, "one", json!({"mode": "ok"}), 1, 1).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let first = env.person("first@example.com").await;
        let second = env.person("second@example.com").await;
        assert_eq!(env.send(first, template).await.status, "sent");
        let queued = env.queue(second, template, 5).await;
        env.attempt().await.unwrap();
        let copy: Value =
            sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'delivery.send'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(copy["deferrals"], 1);

        let ended = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        assert!(ended.is_empty());
        assert_eq!(env.message(queued.message).await.status, "pending");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resumed_message_past_the_rate_wait_ends_delivery_unconfirmed_not_rate_limited(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "lose_first_answer"}), 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        let long_ago = (env.clock.now() - Span::hours(7))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        sqlx::query(
            "UPDATE jobs SET run_at = now(), \
                payload = jsonb_set(payload, '{first_wait}', to_jsonb($1::text)) \
             WHERE kind = 'delivery.send'",
        )
        .bind(long_ago)
        .execute(&env.pool)
        .await
        .unwrap();

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1, "no second call");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_queued_again_before_first_wait_existed_counts_its_wait_from_queued_at(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 5).await;
        // One deferral already, no `first_wait`, queued seven hours ago.
        sqlx::query(
            "UPDATE jobs SET payload = jsonb_set(payload, '{deferrals}', '1'::jsonb) \
             WHERE kind = 'delivery.send'",
        )
        .execute(&env.pool)
        .await
        .unwrap();
        age_queued(&env, 7).await;

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("rate_limited"))
        );
        let copies: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'delivery.send'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(copies, 0, "no fresh deferral");
    }

    /// The `queued_at` of the job in the queue, moved to `hours` before the clock.
    async fn age_queued(env: &Env, hours: i64) {
        let at = (env.clock.now() - Span::hours(hours))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        sqlx::query(
            "UPDATE jobs SET payload = jsonb_set(payload, '{queued_at}', to_jsonb($1::text)) \
             WHERE kind = 'delivery.send'",
        )
        .bind(at)
        .execute(&env.pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_six_hours_a_send_may_wait_for_a_token_count_from_its_first_wait_not_from_the_queue(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "ok"}), 1, 1).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let first = env.person("first@example.com").await;
        let second = env.person("second@example.com").await;
        assert_eq!(env.send(first, template).await.status, "sent");
        let queued = env.queue(second, template, 5).await;
        // It sat in a backed-up queue for seven hours.
        age_queued(&env, 7).await;

        env.attempt().await.unwrap();

        let copy: Value =
            sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'delivery.send'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            copy["deferrals"], 1,
            "its first wait is a deferral, not a failure"
        );
        assert!(copy["first_wait"].is_string());
        assert_eq!(env.message(queued.message).await.status, "pending");

        // The copy runs when the bucket has a token: the long time in the queue is not held
        // against it.
        env.clock.advance(Span::seconds(5));
        sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'delivery.send'")
            .execute(&env.pool)
            .await
            .unwrap();
        env.attempt().await.unwrap();
        assert_eq!(env.message(queued.message).await.status, "sent");

        // A copy whose first wait was over six hours ago fails as rate limited.
        let third = env.person("third@example.com").await;
        let late = env.queue(third, template, 5).await;
        age_queued(&env, 7).await;
        env.attempt().await.unwrap();
        let long_ago = (env.clock.now() - Span::hours(7))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        sqlx::query(
            "UPDATE jobs SET run_at = now(), \
                payload = jsonb_set(payload, '{first_wait}', to_jsonb($1::text)) \
             WHERE kind = 'delivery.send'",
        )
        .bind(long_ago)
        .execute(&env.pool)
        .await
        .unwrap();
        env.attempt().await.unwrap();

        let message = env.message(late.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("rate_limited"))
        );
        assert_eq!(env.sent().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retry_after_an_accepted_delivery_ends_sent_even_if_the_person_was_suppressed_since(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 5).await;
        // The write of the outcome fails once, after the destination accepted the message.
        for statement in [
            "CREATE FUNCTION fail_the_outcome() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'injected'; END $$",
            "CREATE TRIGGER fail_the_outcome BEFORE INSERT ON events FOR EACH ROW \
             WHEN (NEW.name = 'email.sent') EXECUTE FUNCTION fail_the_outcome()",
        ] {
            sqlx::query(statement).execute(&env.pool).await.unwrap();
        }
        env.attempt().await.unwrap_err();
        assert_eq!(env.sent().len(), 1, "the destination accepted the message");
        assert_eq!(env.message(queued.message).await.status, "sending");
        sqlx::query("DROP TRIGGER fail_the_outcome ON events")
            .execute(&env.pool)
            .await
            .unwrap();
        // Before the retry, the person is suppressed.
        add_suppression(
            &env.pool,
            env.ws,
            &NewSuppression {
                identifier_hash: crate_hash("ada@example.com"),
                kind: "email",
                reason: zirv_flow_core::consent::SuppressionReason::Complaint,
                source: "api",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            message.status, "sent",
            "the gates do not judge a message that went out"
        );
        assert_eq!(message.attempts, 2);
        let key = queued.message.to_string();
        assert_eq!(fake_ledger_calls(&env.ledger), [key.clone(), key]);
        assert_eq!(env.sent().len(), 1, "the second call was deduplicated");
        assert_eq!(env.events("email.sent").await.len(), 1);
        assert_eq!(
            env.attempts_of(queued.message).await,
            [("delivered".to_owned(), 1), ("delivered".to_owned(), 2)]
        );
    }

    #[test]
    fn every_failure_of_the_render_host_is_a_render_error() {
        use zirv_flow_render::RenderFailure;
        for failure in [
            RenderFailure::MemoryLimit,
            RenderFailure::Deadline,
            RenderFailure::ChildExited,
            RenderFailure::Protocol,
            RenderFailure::RequestTooLarge,
            RenderFailure::UnsupportedDocument,
        ] {
            assert_eq!(render::failed(failure), "render_error", "{failure}");
        }
    }
}
