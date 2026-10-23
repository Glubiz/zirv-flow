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
//! **Idempotency.** A job that finds its message recorded does nothing. Otherwise it reads, renders
//! and calls the destination holding no lock, then locks the person (`FOR KEY SHARE`, skipped when
//! the person was erased or merged in the meantime) and writes the row, the event and the
//! decisions in one go. A retry, a lost lease or a crash before that commit sends again with the
//! same key, which a destination that honours keys treats as the same message.
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
use zirv_flow_core::consent::Dimensions;
use zirv_flow_store::{
    Admission, Destination, FlowState, GateRecord, MessageStatus, NewJob, NewMessage, PgPool,
    SEND_JOB_KIND, SendOutcome, SubjectKind, WebhookPerson, WebhookSubject, document_sender,
    find_destination, find_flow, find_template, find_template_version, lock_webhook_person,
    message_exists, read_template_field_catalog, read_webhook_subject, record_send_outcome,
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
        if message_exists(ctx.conn(), job.workspace, job.message).await? {
            return Ok(());
        }
        match decide(ctx, &job, &payload, now).await? {
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
        Some(FlowState::Paused) => return dropped(ctx, job, "flow_paused").await,
        Some(FlowState::Archived) | None => return dropped(ctx, job, "flow_inactive").await,
        Some(FlowState::Draft | FlowState::Published) => {}
    }
    let catalog = read_template_field_catalog(ctx.conn(), workspace).await?;
    let keys: Vec<String> = catalog.traits.keys().cloned().collect();
    let Some(WebhookSubject { person, traits }) =
        read_webhook_subject(ctx.conn(), workspace, job.enrollment, &keys).await?
    else {
        // Erased, exited (a goal, an operator, a merge) or merged away: nobody to send to.
        return dropped(ctx, job, "enrollment_ended").await;
    };
    if person != job.person {
        return dropped(ctx, job, "enrollment_ended").await;
    }
    trail.pass(GateRecord::passed("enrollment"));

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
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.sender = Some(sender.identity_id);
    trail.pass(GateRecord::passed("sender"));

    // Gate 2: the address.
    let address = match gates::address(ctx.conn(), workspace, person).await? {
        Ok(address) => address,
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.pass(GateRecord::passed("address"));

    // Gate 3: consent.
    match gates::consent(&pool, job, transactional).await? {
        Ok(passed) => {
            trail.pass(passed.gate);
            trail.rule = passed.rule;
        }
        Err(blocked) => return Ok(trail.skip(blocked)),
    }

    // Gate 4: suppression.
    let key = ctx
        .identifier_key()
        .ok_or_else(|| JobError::new("the worker has no identifier key"))?;
    match gates::suppression(ctx.conn(), workspace, &key, &address).await? {
        Ok(passed) => trail.pass(passed),
        Err(blocked) => return Ok(trail.skip(blocked)),
    }

    // Gates 5 and 6, which #221 fills.
    trail.pass(gates::frequency_cap());
    trail.pass(gates::quiet_hours());

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
    if job.deferrals > 0 && now - job.queued_at > MAX_RATE_WAIT {
        return Ok(trail.fail("rate_limit", "rate_limited"));
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

    // Gate 9: deliver, holding no lock, then record the attempt.
    let attempts = ctx.attempts();
    let number = job.attempt_base + attempts;
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

/// Gate 1 stopped the message: nobody is waiting for it. A decision says so (ids only) and no
/// row, event or attempt is written.
async fn dropped(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    reason: &'static str,
) -> Result<Step, JobError> {
    ctx.record_decision(
        "message.send",
        "dropped",
        Some(reason),
        vec![
            ("message", job.message),
            ("enrollment", job.enrollment),
            ("template", job.template),
            ("destination", job.destination),
        ],
    )
    .await?;
    Ok(Step::Over)
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
    next.payload = job.deferred(payload, attempts);
    next.run_at = Some(now + wait);
    next.max_attempts = (ctx.max_attempts() - (attempts - 1)).max(1);
    next.unique_key = Some(format!("{}:defer:{}", job.key, job.deferrals + 1));
    ctx.enqueue(&next).await?;
    ctx.record_decision(
        "message.send",
        "deferred",
        Some("rate_limit"),
        vec![
            ("message", job.message),
            ("enrollment", job.enrollment),
            ("template", job.template),
            ("destination", job.destination),
        ],
    )
    .await?;
    Ok(Step::Over)
}

/// Writes the end of the message: locks the person for the short write, and records the row, the
/// event and the decisions, unless the person was erased or merged away while the message was
/// being read, rendered and delivered, in which case nothing is written for them.
async fn finish(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    trail: Trail,
    now: OffsetDateTime,
) -> Result<(), JobError> {
    match lock_webhook_person(ctx.conn(), job.workspace, job.enrollment, job.person).await? {
        WebhookPerson::Held => {}
        WebhookPerson::Gone | WebhookPerson::Moved => return Ok(()),
    }
    let trace = ctx.trace().clone();
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
    let job_ref = (ctx.job_id(), ctx.attempts());
    record_send_outcome(
        ctx.conn(),
        &SendOutcome {
            message,
            trace: &trace,
            job: job_ref,
        },
    )
    .await?;
    Ok(())
}
