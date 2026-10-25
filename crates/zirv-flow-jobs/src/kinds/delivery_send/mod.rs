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
//! 7. `quiet_hours`: inside the workspace's quiet period in the recipient's zone (the `timezone`
//!    trait, else the workspace's), the message waits for its end and gives up a slot it holds
//!    from an earlier run.
//!    `frequency_cap`: the person's caps on the channel, checked only for a message that sends
//!    now. The check counts the slots of the person's other messages in each cap's window and
//!    reserves this message's own (`messages.cap_slot_at`, moved to now on a re-run) in a short
//!    transaction of its own under a per-person advisory lock, so the lock is gone before
//!    delivery; otherwise `skipped: frequency_cap`, or the message waits until a slot leaves
//!    the window when the cap says `defer`.
//!    A transactional message skips both.
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

pub(crate) mod deliver;
pub(crate) mod gates;
mod payload;
mod policy;
mod quiet;
pub(crate) mod render;

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tracing::warn;
use uuid::Uuid;
use zirv_flow_connectors::destinations::Registry;
use zirv_flow_core::consent::{Dimensions, SuppressionReason};
use zirv_flow_core::{HmacKey, TraceContext};
use zirv_flow_store::{
    Admission, DELIVERY_UNCONFIRMED, Destination, FlowState, GateRecord, HARD_BOUNCE,
    MessageStatus, NewJob, NewMessage, NewSuppression, PgConnection, PgPool, SEND_JOB_KIND,
    SOFT_BOUNCE, SendOutcome, SubjectKind, WebhookPerson, WebhookSubject,
    add_organization_suppression, clear_message_in_flight, document_sender, find_destination,
    find_flow, find_message_links, find_template, find_template_version, insert_message,
    lock_webhook_person, mark_message_sending, mark_message_unconfirmed, message_address_hash,
    message_in_flight, message_status, message_unconfirmed, read_template_field_catalog,
    read_webhook_subject, record_message_deferral, record_message_links, record_send_outcome,
    throttle_bucket,
};

pub(crate) use payload::Queued;

use self::gates::Blocked;
use self::payload::SendJob;
use crate::delivery::attempt::AttemptTarget;
use crate::delivery::verdict::SMTP_DEFERRED;
use crate::delivery::{self, Verdict};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct DeliverySend;

/// The note on the unconfirmed ending of a resumed message whose address changed since the call
/// that may have been accepted.
const ADDRESS_CHANGED: &str = "address_changed";

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
    rule: Option<String>,
    template_version: Option<i32>,
    sender: Option<Uuid>,
    category: Option<String>,
    /// The delivery attempts the destination saw.
    attempts: i32,
    /// The suppression hash of an address a mail server refused for good: the write that ends the
    /// message suppresses it in the whole organization (a hard bounce, OQ-12).
    bounced: Option<[u8; 32]>,
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
            bounced: None,
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
        self.rule = blocked.rule.or(self.rule.take());
        Step::Finished(self)
    }

    /// Ends the message here, failed at `gate` for `reason`.
    fn fail(mut self, gate: &'static str, reason: &'static str) -> Step {
        self.gates.push(GateRecord::blocked(gate, reason));
        self.status = MessageStatus::Failed;
        self.reason = Some(reason);
        Step::Finished(self)
    }

    /// Ends the message here as `delivery_unconfirmed`: an earlier call may have been accepted, so
    /// nothing that went wrong since shows it was not. `was` is what really went wrong, kept as the
    /// gate record's note and in the log; it is a code, never an address.
    fn unconfirmed(mut self, gate: &'static str, was: Option<&'static str>) -> Step {
        warn!(gate, was, "message ends unconfirmed");
        let mut record = GateRecord::blocked(gate, DELIVERY_UNCONFIRMED);
        if let Some(was) = was {
            record = record.with_note(was);
        }
        self.gates.push(record);
        self.status = MessageStatus::Failed;
        self.reason = Some(DELIVERY_UNCONFIRMED);
        Step::Finished(self)
    }

    /// Ends the message here for a failure before the destination was called. A resumed message
    /// (its row was `sending` when the attempt began) may have been accepted on an earlier call,
    /// so it ends unconfirmed instead.
    fn fail_unsent(self, resuming: bool, gate: &'static str, reason: &'static str) -> Step {
        if resuming {
            self.unconfirmed(gate, Some(reason))
        } else {
            self.fail(gate, reason)
        }
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
    let Some(flow) = find_flow(ctx.conn(), workspace, job.flow).await? else {
        return stopped(ctx, job, trail, "flow_inactive").await;
    };
    match flow.state {
        FlowState::Paused => return stopped(ctx, job, trail, "flow_paused").await,
        FlowState::Archived => return stopped(ctx, job, trail, "flow_inactive").await,
        FlowState::Draft | FlowState::Published => {}
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
        return Ok(trail.fail_unsent(resuming, "template", "template_unavailable"));
    };
    trail.pass(GateRecord::passed("template"));
    trail.template_version = Some(version.version);
    trail.category = Some(template.category.clone());
    let transactional = gates::is_transactional(&template.category);

    // The sender.
    let sender = match gates::sender(&pool, workspace, document_sender(&version.document)).await? {
        Ok(sender) => sender,
        Err(blocked) if resuming => {
            let was = blocked.gate.reason.unwrap_or("sender_unavailable");
            return Ok(trail.fail_unsent(true, "sender", was));
        }
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.sender = Some(sender.identity_id);
    trail.pass(GateRecord::passed("sender"));

    // Gate 2: the address.
    let address = match gates::address(ctx.conn(), workspace, person).await? {
        Ok(address) => address,
        Err(blocked) if resuming => {
            let was = blocked.gate.reason.unwrap_or("address_unavailable");
            return Ok(trail.fail_unsent(true, "address", was));
        }
        Err(blocked) => return Ok(trail.skip(blocked)),
    };
    trail.pass(GateRecord::passed("address"));
    // The key that hashes the address, fetched once: the suppression gate reads it, and so does
    // the hard bounce that ends a delivery, including that of a resumed message, which skips the
    // gates. It is fetched here so a worker without it can still end a message the steps above
    // stop.
    let key = ctx
        .identifier_key()
        .ok_or_else(|| JobError::new("the worker has no identifier key"))?;

    // Gates 3 to 6 judged the message when it first went out. A retry of a message that may have
    // gone out does not judge it again: a person who withdrew consent since was sent this one.
    if !resuming {
        match person_gates(ctx, job, &key, &address, transactional, now, &mut trail).await? {
            None => {}
            Some(gates::Stop::Skip(blocked)) => return Ok(trail.skip(blocked)),
            Some(gates::Stop::Defer(deferral)) => {
                return defer(ctx, job, payload, Wait::Gate(deferral), now).await;
            }
        }
    }

    // Gate 7: render, in the pool. A marketing message carries signed unsubscribe, click and open
    // links, so it needs the public URL and the tracking key; a transactional one is not tracked and
    // gets the unsubscribe links only when it can.
    let tracker = ctx.tracking().map(|(base, key)| render::Tracker {
        base: base.to_owned(),
        key: Arc::clone(key),
    });
    if tracker.is_none() && !transactional {
        warn!(
            "email not rendered: tracking links need server.public_url and the secrets master key"
        );
        return Ok(trail.fail_unsent(resuming, "render", render::RENDER_ERROR));
    }
    let context = render::context_of(job, &traits, &address, tracker.as_ref());
    let pool_for_render = ctx.render_pool().cloned();
    let tracked = (!transactional).then_some(()).and(tracker.as_ref());
    let recorded = find_message_links(&pool, job.message).await?;
    let utm = render::utm_plan(
        ctx.conn(),
        workspace,
        &template,
        Some(version.version),
        Some(render::UtmFlow {
            name: &flow.name,
            version: job.version,
            step: &job.node,
        }),
    )
    .await?;
    let tracked = match render::render(
        pool_for_render.as_ref(),
        version.document,
        &context,
        tracked,
        &recorded,
        utm.as_ref(),
    )
    .await
    {
        Ok(tracked) => tracked,
        Err(reason) => return Ok(trail.fail_unsent(resuming, "render", reason)),
    };
    // The links are recorded before delivery in a statement of their own, so the redirect works
    // for whichever attempt the person receives and a retry writes the same rows.
    record_message_links(&pool, workspace, job.message, &tracked.links).await?;
    let rendered = tracked.rendered;
    trail.pass(GateRecord::passed("render"));
    let email = render::email_payload(&sender, &rendered);

    // Gate 8: the destination, loaded for this message, and a token from its bucket.
    if job
        .first_wait
        .is_some_and(|first| now - first > MAX_RATE_WAIT)
    {
        return Ok(trail.fail_unsent(resuming, "rate_limit", "rate_limited"));
    }
    let Some(destination) = find_destination(ctx.conn(), workspace, job.destination)
        .await?
        .filter(|destination| destination.archived_at.is_none())
    else {
        return Ok(trail.fail_unsent(resuming, "rate_limit", "destination_unavailable"));
    };
    let registry = Registry::built_in();
    let Some(driver) = registry.get(&destination.kind) else {
        return Ok(trail.fail_unsent(resuming, "rate_limit", "destination_unavailable"));
    };
    if !driver.spec().capabilities.channels.contains(&"email") {
        return Ok(trail.fail_unsent(resuming, "rate_limit", "destination_unsupported"));
    }
    // An earlier call may have been accepted: it lost the server's reply after the whole message
    // was sent (the unconfirmed mark), or it was cut off before its outcome was known (the
    // in-flight mark, written before every call). Both are kept on the row (they survive a job
    // retry), and read once, before this attempt writes its own in-flight mark (gate 9). A crash
    // after a definitive reply but before the clear there also counts as carried: that is
    // deliberately conservative.
    let carried = resuming
        && (message_unconfirmed(&pool, workspace, job.message).await?
            || message_in_flight(&pool, workspace, job.message).await?);
    let current_hash = gates::address_hash(&key, &address);
    // An earlier call that may have been accepted went to the stored address. If the person's
    // address changed since, the resumed call must not go to the new one under the same
    // idempotency key, and the stored hash must keep naming the address that may have received
    // the message (a bounce for it must not suppress the new one). A message with no stored hash
    // keeps the earlier behaviour. This ends the message before a rate token is taken: it makes
    // no call, so it must not spend one.
    if carried
        && let (Some(current), Some(Some(stored))) = (
            current_hash,
            message_address_hash(&pool, workspace, job.message).await?,
        )
        && current != stored
    {
        return Ok(trail.unconfirmed("delivery", Some(ADDRESS_CHANGED)));
    }
    let gates = Arc::clone(ctx.rate_gates());
    let clock = ctx.clock().clone();
    match gates.admit(&pool, destination.id, &clock).await? {
        Admission::Granted => trail.pass(GateRecord::passed("rate_limit")),
        Admission::Wait(wait) => {
            return defer(ctx, job, payload, Wait::RateLimit(wait), now).await;
        }
        Admission::Gone => {
            return Ok(trail.fail_unsent(resuming, "rate_limit", "destination_unavailable"));
        }
    }

    // Gate 9: deliver, holding no lock, then record the attempt. The row is marked `sending`
    // first, in a statement of its own: from here on the message may go out, and a retry knows it.
    let attempts = ctx.attempts();
    let number = job.attempt_base + attempts;
    // The attempt number fences the mark: a stale attempt clears only its own.
    let mark = number - 1;
    mark_message_sending(&pool, workspace, job.message, mark, current_hash.as_ref()).await?;
    let call = deliver::Call {
        workspace,
        destination: &destination,
        driver,
        sender: &sender,
        message: job.message,
        address: &address,
        payload: &email,
    };
    let done = match deliver::deliver(ctx, &call, now).await {
        Ok(done) => done,
        Err(error) => {
            // An error here comes before the destination is called: nothing was sent, so this
            // attempt no longer counts as in flight. A failed clear leaves the original error.
            let _ = clear_message_in_flight(&pool, workspace, job.message, mark).await;
            return Err(error);
        }
    };
    // A reply lost after the final dot, or a timeout or reset after an HTTP request was written,
    // leaves the message possibly accepted. The mark is written before the attempt is settled, so
    // a settle that fails or a lease that is lost cannot skip it.
    let lost_reply = matches!(
        done.verdict,
        Verdict::Retry { reason, .. } if reason == DELIVERY_UNCONFIRMED
    );
    let possibly_accepted = lost_reply || done.may_have_arrived;
    if possibly_accepted {
        mark_message_unconfirmed(&pool, workspace, job.message).await?;
    } else if !matches!(done.verdict, Verdict::Succeeded { .. }) {
        // The outcome proves this call was not accepted: it no longer counts as in flight. A
        // success ends the message, and the ending write replaces the marks.
        clear_message_in_flight(&pool, workspace, job.message, mark).await?;
    }
    let target = AttemptTarget {
        destination_id: destination.id,
        subject_kind: SubjectKind::Message,
        subject_id: job.message,
    };
    delivery::settle(ctx, Some(&target), number, &done).await?;
    trail.attempts = number;
    // Once an earlier call may have been accepted, nothing that goes wrong after it shows that it
    // was not: every ending but a sent message and a verdict about the address is unconfirmed.
    let unconfirmed = carried || possibly_accepted;
    match done.verdict {
        Verdict::Succeeded { .. } => {
            trail.pass(GateRecord::passed("delivery"));
            Ok(Step::Finished(trail))
        }
        Verdict::Terminal { reason, .. } if reason == HARD_BOUNCE => {
            // The server says the address cannot receive mail: it is suppressed with the write
            // that ends the message, whatever an earlier call came to.
            trail.bounced = gates::address_hash(&key, &address);
            Ok(trail.fail("delivery", reason))
        }
        Verdict::Terminal { reason, .. } if unconfirmed => {
            Ok(trail.unconfirmed("delivery", Some(reason)))
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
            if unconfirmed {
                Ok(trail.unconfirmed("delivery", Some(reason)))
            } else if reason == SMTP_DEFERRED {
                // Temporary refusals that outlast the retries are a soft bounce, with no
                // suppression.
                Ok(trail.fail("delivery", SOFT_BOUNCE))
            } else {
                Ok(trail.fail("delivery", reason))
            }
        }
    }
}

/// Gates 3 to 6: consent, suppression and the two caps. What stops the message, if anything does.
async fn person_gates(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    key: &HmacKey,
    address: &str,
    transactional: bool,
    now: OffsetDateTime,
    trail: &mut Trail,
) -> Result<Option<gates::Stop>, JobError> {
    let pool = ctx
        .pool()
        .cloned()
        .ok_or_else(|| JobError::new("the worker has no pool"))?;
    let pool = &pool;
    match gates::consent(pool, ctx.clock(), job, transactional).await? {
        Ok(passed) => {
            trail.pass(passed.gate);
            trail.rule = passed.rule;
        }
        Err(blocked) => return Ok(Some(gates::Stop::Skip(blocked))),
    }
    match gates::suppression(ctx.conn(), job.workspace, key, address).await? {
        Ok(passed) => trail.pass(passed),
        Err(blocked) => return Ok(Some(gates::Stop::Skip(blocked))),
    }
    // A transactional message is not marketing: a receipt is not capped or held overnight.
    if transactional {
        trail.pass(GateRecord::passed("quiet_hours"));
        trail.pass(GateRecord::passed("frequency_cap"));
        return Ok(None);
    }
    // Quiet hours first: a slot is reserved only for a message that sends now, so a concurrent
    // send to the person never counts a slot whose message waits.
    match gates::quiet_hours(ctx.conn(), job, now).await? {
        Ok(passed) => {
            if passed.note == Some(gates::TIMEZONE_FALLBACK) {
                note_zone_fallback(ctx, job).await?;
            }
            trail.pass(passed);
        }
        Err(stop) => {
            if let gates::Stop::Defer(deferral) = &stop
                && deferral.note == Some(gates::TIMEZONE_FALLBACK)
            {
                note_zone_fallback(ctx, job).await?;
            }
            return Ok(Some(stop));
        }
    }
    match gates::frequency_cap(pool, job, now).await? {
        Ok(passed) => trail.pass(passed),
        Err(stop) => return Ok(Some(stop)),
    }
    Ok(None)
}

/// A destination answered 429: halve its bucket's rate until its `Retry-After` has passed (the
/// store caps the wait). A failure to record it is logged and does not change the job's result.
pub(crate) async fn slow_down(
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
        return Ok(trail.unconfirmed("enrollment", Some(reason)));
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

/// Why a send is queued again instead of ending.
enum Wait {
    /// No rate-limit token yet: the bucket will hold one after this long.
    RateLimit(Duration),
    /// A gate (quiet hours, or a frequency cap in `defer` mode) holds the message until a time.
    Gate(gates::Deferral),
}

/// The person's `timezone` trait was not a zone and the workspace's was used for the quiet hours:
/// a decision says so (ids only), apart from the gate record's note.
async fn note_zone_fallback(ctx: &mut JobContext<'_>, job: &SendJob) -> Result<(), JobError> {
    ctx.record_decision(
        "message.send",
        "noted",
        Some(gates::TIMEZONE_FALLBACK),
        decision_ids(job),
    )
    .await
}

/// The send waits: this job ends and a copy of it runs later, at the instant the bucket will hold
/// a token or the gate that deferred it lets the message go. The copy has its own unique key,
/// since this job still holds the original while it runs, and its attempts continue this job's
/// (`attempt_base`, and a budget of what is left), so a deferral never consumes `max_attempts` and
/// a destination that keeps the send waiting cannot buy it more attempts than the job had.
///
/// Only a wait for a token starts the clock of the six hours a send may wait; a gate's deferral
/// clears it, so it starts again after the hold. A gate's deferral is also written to the message's log first, in a statement
/// of its own, and with the quiet hours releases the cap slot the message holds.
async fn defer(
    ctx: &mut JobContext<'_>,
    job: &SendJob,
    payload: &serde_json::Value,
    wait: Wait,
    now: OffsetDateTime,
) -> Result<Step, JobError> {
    let attempts = ctx.attempts();
    let (run_at, reason, rate) = match &wait {
        Wait::RateLimit(wait) => (now + *wait, "rate_limit", true),
        Wait::Gate(deferral) => (deferral.until, deferral.gate, false),
    };
    if let Wait::Gate(deferral) = &wait {
        let pool = ctx
            .pool()
            .cloned()
            .ok_or_else(|| JobError::new("the worker has no pool"))?;
        record_message_deferral(
            &pool,
            job.workspace,
            job.message,
            deferral.gate,
            deferral.until,
            deferral.note,
            deferral.release_slot,
        )
        .await?;
    }
    let mut next = NewJob::new(SEND_JOB_KIND, ctx.trace().clone());
    next.workspace_id = Some(job.workspace);
    next.payload = job.deferred(payload, attempts, now, rate);
    next.run_at = Some(run_at);
    next.max_attempts = (ctx.max_attempts() - (attempts - 1)).max(1);
    next.unique_key = Some(format!("{}:defer:{}", job.key, job.deferrals + 1));
    ctx.enqueue(&next).await?;
    ctx.record_decision("message.send", "deferred", Some(reason), decision_ids(job))
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
        deciding_rule: trail.rule.as_deref(),
        gates: &trail.gates,
        attempts: trail.attempts,
        now,
    };
    if !held {
        insert_message(&mut *connection, &message).await?;
    } else {
        record_send_outcome(
            &mut *connection,
            &SendOutcome {
                message,
                trace,
                job: Some(job_ref),
                keep_gates: false,
            },
        )
        .await?;
    }
    if let Some(identifier_hash) = trail.bounced {
        // A hard bounce suppresses the address in every workspace of the organization (OQ-19). The
        // row holds the HMAC of the address, never the address.
        add_organization_suppression(
            &mut *connection,
            &NewSuppression {
                identifier_hash,
                kind: "email",
                reason: SuppressionReason::HardBounce,
                source: "bounce",
                note: None,
                created_by: None,
            },
        )
        .await?;
    }
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
    use zirv_flow_connectors::destinations::{
        SMTP_SINK_DROP, SMTP_SINK_PASSWORD, SMTP_SINK_USER, SmtpSink, SmtpSinkOptions,
        SmtpSinkStage, fake_ledger_calls, fake_ledger_sent,
    };
    use zirv_flow_core::consent::policy::PolicyRule;
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

    pub(super) const PUBLIC_URL: &str = "https://flow.example.test";

    pub(super) fn tracking_key() -> Arc<zirv_flow_core::HmacKey> {
        let master = MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap();
        Arc::new(HmacRoot::bootstrap_from(&master).hmac_key(HmacPurpose::Tracking))
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
        no_public_url: bool,
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
                no_public_url: false,
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
                    name: format!("Mail {}", self.flow),
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
            self.queue_dims(
                person,
                template,
                destination,
                topic,
                &Dimensions::new(),
                max_attempts,
            )
            .await
        }

        async fn queue_dims(
            &self,
            person: Uuid,
            template: Uuid,
            destination: Uuid,
            topic: Option<Uuid>,
            dimensions: &Dimensions,
            max_attempts: i32,
        ) -> Queued1 {
            let enrolled = self.enroll(person).await;
            let message = Uuid::now_v7();
            let key = format!("{}:mail:send", enrolled.enrollment);
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
                dimensions,
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
                    .with_tracking(
                        (!self.no_public_url).then(|| PUBLIC_URL.to_owned()),
                        Some(tracking_key()),
                    )
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

        /// [`Env::send`] for a message with these dimension values.
        async fn send_with(
            &self,
            person: Uuid,
            template: Uuid,
            dimensions: &Dimensions,
        ) -> zirv_flow_store::Message {
            let queued = self
                .queue_dims(person, template, self.destination, None, dimensions, 5)
                .await;
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
        let stored: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT address_hash FROM messages WHERE id = $1")
                .bind(queued.message)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(crate_hash("ada@example.com").as_slice()),
            "marking the message sending stores the keyed hash of the address it went to"
        );
        assert_eq!(
            gate_names(&message),
            [
                "enrollment:passed",
                "template:passed",
                "sender:passed",
                "address:passed",
                "consent:passed",
                "suppression:passed",
                "quiet_hours:passed",
                "frequency_cap:passed",
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

    const LINKED: &str = "<html><body><p>Hi</p><a href=\"https://shop.example.test/a?x=1\">Shop</a>         <a href=\"mailto:team@example.test\">Mail</a>         <a href=\"https://shop.example.test/b\">Again</a>         <a href=\"{{ system.unsubscribe_url }}\">Unsubscribe</a></body></html>";

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_marketing_message_has_tracked_links_a_pixel_and_the_real_unsubscribe_link(
        pool: PgPool,
    ) {
        use zirv_flow_core::tracking::{TokenKind, verify};

        let mut env = Env::new(pool, "tracked").await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.template("marketing", LINKED).await;
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "sent");
        let sent = env.sent();
        let html = sent[0].payload["html"].as_str().unwrap();
        assert!(!html.contains("shop.example.test"), "{html}");
        assert_eq!(
            html.matches("https://flow.example.test/t/c/").count(),
            2,
            "{html}"
        );
        assert!(html.contains("mailto:team@example.test"), "{html}");
        let pixel = html.find("/t/o/").expect("the open pixel");
        assert!(
            pixel > html.find("Unsubscribe").unwrap(),
            "the pixel is at the end"
        );
        assert!(html.ends_with("</body></html>"), "{html}");
        let links: Vec<(i32, String)> = sqlx::query_as(
            "SELECT link_index, url FROM message_links WHERE message_id = $1 ORDER BY link_index",
        )
        .bind(queued.message)
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            links,
            [
                (0, "https://shop.example.test/a?x=1".to_owned()),
                (1, "https://shop.example.test/b".to_owned())
            ]
        );
        let header = sent[0].payload["headers"]["List-Unsubscribe"]
            .as_str()
            .unwrap();
        let url = header.trim_start_matches('<').trim_end_matches('>');
        let token = url.strip_prefix("https://flow.example.test/u/").expect(url);
        let claims = verify(&tracking_key(), TokenKind::Unsubscribe, token).unwrap();
        assert_eq!(claims.message, queued.message);
        assert!(
            html.contains(&url.replace('/', "&#x2f;")),
            "the unsubscribe link is not rewritten: {html}"
        );
        assert_eq!(
            sent[0].payload["headers"]["List-Unsubscribe-Post"],
            "List-Unsubscribe=One-Click"
        );
        assert!(
            sent[0].payload["text"]
                .as_str()
                .unwrap()
                .contains("https://flow.example.test/u/")
        );
    }

    const UTM_QUERY: &str =
        "utm_source=utm-ws&utm_medium=email&utm_campaign=utm-flow&utm_content=utm-tpl";

    /// Turns the UTM setting on for the env's workspace with `shop.example.test` listed.
    async fn enable_utm(env: &Env) {
        zirv_flow_store::set_workspace_utm(
            &env.pool,
            env.ws,
            &zirv_flow_store::UtmSettings {
                enabled: true,
                source: "utm-ws".into(),
                campaign: "utm-flow".into(),
                content: "utm-tpl".into(),
                domains: vec!["shop.example.test".into()],
                ..zirv_flow_store::UtmSettings::default()
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn with_utm_on_the_recorded_links_and_the_click_targets_carry_the_parameters(
        pool: PgPool,
    ) {
        use zirv_flow_core::tracking::{TokenKind, verify};

        let mut env = Env::new(pool, "utm-on").await;
        env.render = Some(start_pool("serve", "").await);
        enable_utm(&env).await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.template("marketing", LINKED).await;
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "sent");
        let links: Vec<(i32, String)> = sqlx::query_as(
            "SELECT link_index, url FROM message_links WHERE message_id = $1 ORDER BY link_index",
        )
        .bind(queued.message)
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            links,
            [
                (0, format!("https://shop.example.test/a?x=1&{UTM_QUERY}")),
                (1, format!("https://shop.example.test/b?{UTM_QUERY}"))
            ]
        );
        // The click redirect target is the recorded, tagged link.
        let sent = env.sent();
        let html = sent[0].payload["html"].as_str().unwrap();
        let token = html
            .split("/t/c/")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        let claims = verify(&tracking_key(), TokenKind::Click, token).unwrap();
        let target = zirv_flow_store::find_message_link(&env.pool, claims.message, claims.link)
            .await
            .unwrap();
        assert_eq!(
            target,
            Some(format!("https://shop.example.test/a?x=1&{UTM_QUERY}"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_that_opts_out_records_the_links_as_they_were(pool: PgPool) {
        let mut env = Env::new(pool, "utm-off").await;
        env.render = Some(start_pool("serve", "").await);
        enable_utm(&env).await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.template("marketing", LINKED).await;
        sqlx::query("UPDATE templates SET utm = 'off' WHERE id = $1")
            .bind(template)
            .execute(&env.pool)
            .await
            .unwrap();
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();

        let links: Vec<String> = sqlx::query_scalar(
            "SELECT url FROM message_links WHERE message_id = $1 ORDER BY link_index",
        )
        .bind(queued.message)
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            links,
            [
                "https://shop.example.test/a?x=1",
                "https://shop.example.test/b"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_renders_with_reordered_links_each_redirect_to_their_own_displayed_urls(
        pool: PgPool,
    ) {
        use zirv_flow_core::tracking::{TokenKind, verify};

        let env = Env::new(pool, "reorder").await;
        let render_pool = start_pool("serve", "").await;
        let person = env.person("ada@example.com").await;
        let template = env.template("marketing", LINKED).await;
        let queued = env.queue(person, template, 5).await;
        // The links belong to the message row the job's first run writes.
        let claimed = env.claim().await.expect("the send is ready");
        let job = SendJob::parse(&claimed.payload).unwrap();
        start_message(&env.pool, &job, env.clock.now())
            .await
            .unwrap();
        let tracker = render::Tracker {
            base: PUBLIC_URL.to_owned(),
            key: tracking_key(),
        };
        let mut context = zirv_flow_render::RenderContext::default();
        context.message.id = queued.message.to_string();
        // A marketing message must carry its unsubscribe link, which is never rewritten.
        context.system.unsubscribe_url = format!("{PUBLIC_URL}/u/kept");
        let page = |urls: &[&str]| {
            let links: String = urls
                .iter()
                .map(|url| format!("<a href=\"{url}\">x</a>"))
                .collect();
            json!({"schema": 1, "channel": "email", "mode": "code", "email": {
                "subject": "Hello", "sender": env.sender, "lang": "en", "category": "marketing",
                "html": format!(
                    "<html><body>{links}<a href=\"{{{{ system.unsubscribe_url }}}}\">Unsubscribe</a></body></html>"
                ),
                "text": "Hello {{ system.unsubscribe_url }}",
            }})
        };

        let mut displayed = Vec::new();
        for urls in [
            ["https://a.test/", "https://b.test/"],
            ["https://b.test/", "https://c.test/"],
        ] {
            let recorded = zirv_flow_store::find_message_links(&env.pool, queued.message)
                .await
                .unwrap();
            let tracked = render::render(
                Some(&render_pool),
                page(&urls),
                &context,
                Some(&tracker),
                &recorded,
                None,
            )
            .await
            .unwrap();
            record_message_links(&env.pool, env.ws, queued.message, &tracked.links)
                .await
                .unwrap();
            let tokens: Vec<String> = tracked
                .rendered
                .html
                .split("/t/c/")
                .skip(1)
                .map(|rest| rest.split('"').next().unwrap().to_owned())
                .collect();
            displayed.push((urls, tokens));
        }

        for (urls, tokens) in displayed {
            assert_eq!(tokens.len(), urls.len());
            for (url, token) in urls.iter().zip(&tokens) {
                let claims = verify(&tracking_key(), TokenKind::Click, token).unwrap();
                let target =
                    zirv_flow_store::find_message_link(&env.pool, claims.message, claims.link)
                        .await
                        .unwrap();
                assert_eq!(target.as_deref(), Some(*url));
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn after_an_unsubscribe_the_next_send_is_skipped_for_no_consent_and_the_decision_keeps_the_trace(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "unsubnext").await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let template = env.template("marketing", LINKED).await;
        let first = env.queue(person, template, 5).await;
        env.attempt().await.unwrap();
        assert_eq!(env.message(first.message).await.status, "sent");
        let tracked = zirv_flow_store::find_tracked_message(&env.pool, first.message)
            .await
            .unwrap()
            .unwrap();

        let result = zirv_flow_store::unsubscribe_message(
            &env.pool,
            &tracked,
            env.clock.now(),
            &TraceContext::root(Cause::api("unsubscribe")),
        )
        .await
        .unwrap();

        assert_eq!(result, zirv_flow_store::Unsubscribed::Changed);
        let second = env.queue(person, template, 5).await;
        env.attempt().await.unwrap();
        let message = env.message(second.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("skipped", Some("no_consent"))
        );
        let (trace, enrollment_trace): (Uuid, Uuid) = sqlx::query_as(
            "SELECT d.trace_id, e.trace_id FROM decision_records d, enrollments e              WHERE d.action = 'consent.change' AND d.reason = 'unsubscribe_link' AND e.id = $1              ORDER BY d.recorded_at DESC LIMIT 1",
        )
        .bind(first.enrolled.enrollment)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            trace, enrollment_trace,
            "the decision keeps the message's trace"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transactional_message_has_no_tracking(pool: PgPool) {
        let mut env = Env::new(pool, "plainmail").await;
        env.render = Some(start_pool("serve", "").await);
        let person = env.person("ada@example.com").await;
        let template = env.template("transactional", LINKED).await;
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "sent");
        let html = env.sent()[0].payload["html"].as_str().unwrap().to_owned();
        assert_eq!(
            html.matches("https://shop.example.test/").count(),
            2,
            "{html}"
        );
        assert!(!html.contains("/t/o/") && !html.contains("/t/c/"), "{html}");
        assert!(html.contains("flow.example.test&#x2f;u&#x2f;"), "{html}");
        assert_eq!(env.count("SELECT count(*) FROM message_links").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_a_public_url_a_marketing_message_fails_and_a_transactional_one_is_sent(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "nourl").await;
        env.render = Some(start_pool("serve", "").await);
        env.no_public_url = true;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let marketing = env.template("marketing", LINKED).await;
        let transactional = env.template("transactional", LINKED).await;
        let failing = env.queue(person, marketing, 5).await;

        env.attempt().await.unwrap();

        let message = env.message(failing.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("render_error"))
        );
        assert!(env.sent().is_empty());
        let sending = env.queue(person, transactional, 5).await;
        env.attempt().await.unwrap();
        assert_eq!(env.message(sending.message).await.status, "sent");
        let html = env.sent()[0].payload["html"].as_str().unwrap().to_owned();
        assert!(!html.contains("/t/o/") && !html.contains("/t/c/"), "{html}");
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
        let note = message.gate_results.to_string();
        assert!(
            note.contains("flow_paused"),
            "the real reason is kept: {note}"
        );
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

        assert_eq!(
            ended.iter().map(|e| e.message).collect::<Vec<_>>(),
            [queued.message]
        );
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
    async fn the_sweep_ends_a_message_whose_last_attempt_was_refused_for_good_as_an_internal_error(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        assert!(
            !message_in_flight(&env.pool, env.ws, queued.message)
                .await
                .unwrap(),
            "a definitive refusal clears the in-flight mark"
        );
        bury_the_jobs(&env).await;

        let ended = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        assert_eq!(ended.len(), 1);
        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("internal_error")),
            "no attempt was possibly accepted"
        );
        assert!(message.gate_results.to_string().contains("job_dead"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_ends_a_sending_message_whose_job_is_dead_as_delivery_unconfirmed(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_at(SmtpSinkStage::DataEnd, SMTP_SINK_DROP);
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        assert!(
            message_unconfirmed(&env.pool, env.ws, queued.message)
                .await
                .unwrap(),
            "the lost reply left the unconfirmed mark"
        );
        bury_the_jobs(&env).await;

        let ended = end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        assert_eq!(
            ended.iter().map(|e| e.message).collect::<Vec<_>>(),
            [queued.message]
        );
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

    // --- the smtp destination through the pipeline (#218) ---

    /// An env whose destination is an SMTP relay at `sink` (no TLS: the sink is on loopback), whose
    /// sender domain has a real DKIM key sealed under a keyring the worker holds, and whose render
    /// host is running. Returns the env and the public key the domain publishes.
    async fn smtp_env(pool: PgPool, sink: &SmtpSink) -> (Env, String) {
        use zirv_flow_core::sending::generate_dkim_key;
        use zirv_flow_core::{Keyring, SecretId, SecretPurpose, seal, secret_aad};
        use zirv_flow_store::secrets::{insert_unchecked, replace_unchecked};
        use zirv_flow_store::{NewWorkspaceCredential, create_credential};

        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        env.keyring = Some(Arc::clone(&keys));
        let key = generate_dkim_key().unwrap();
        let secret: Uuid = sqlx::query_scalar(
            "SELECT dkim_secret_id FROM sending_domains WHERE workspace_id = $1",
        )
        .bind(env.ws.as_uuid())
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let secret = SecretId::from_uuid(secret);
        let sealed = seal(
            &keys,
            key.private_pkcs8.expose(),
            &secret_aad(env.ws, secret, SecretPurpose::DkimKey),
        );
        assert!(
            replace_unchecked(&env.pool, env.ws, secret, &sealed)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE sending_domains SET dkim_public_key = $2 WHERE workspace_id = $1")
            .bind(env.ws.as_uuid())
            .bind(&key.public_key)
            .execute(&env.pool)
            .await
            .unwrap();
        // The relay's server and login are an `smtp` workspace credential, sealed under the same
        // keyring; the sink is on loopback, so TLS is off.
        let credential_secret = SecretId::new();
        let purpose = SecretPurpose::WorkspaceCredential;
        let sealed = seal(
            &keys,
            json!({"password": SMTP_SINK_PASSWORD})
                .to_string()
                .as_bytes(),
            &secret_aad(env.ws, credential_secret, purpose),
        );
        insert_unchecked(&env.pool, env.ws, purpose, credential_secret, &sealed)
            .await
            .unwrap();
        let credential = create_credential(
            &env.pool,
            env.ws,
            &NewWorkspaceCredential {
                id: Uuid::now_v7(),
                key: "relay",
                name: "Relay",
                kind: "smtp",
                schema: None,
                fields: &json!({
                    "host": "127.0.0.1",
                    "port": sink.port(),
                    "tls": "none",
                    "username": SMTP_SINK_USER,
                }),
                secret_id: credential_secret,
                hints: &json!({"password": ""}),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        env.destination = create_destination(
            &env.pool,
            env.ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "smtp",
                name: "Relay",
                config: &json!({}),
                credential_id: Some(credential.id),
                rate: 100,
                burst: 100,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        (env, key.public_key)
    }

    /// The value of the first header called `name` of a captured message.
    fn header_of(data: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(data);
        let head = text.split("\r\n\r\n").next()?;
        head.split("\r\n").find_map(|line| {
            let (candidate, value) = line.split_once(':')?;
            candidate
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    async fn suppressions(env: &Env) -> i64 {
        env.count("SELECT count(*) FROM suppressions WHERE reason = 'hard_bounce'")
            .await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_smtp_send_is_signed_with_the_sender_domains_sealed_key_and_verifies_against_the_published_record(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::verify_dkim_signature;
        use zirv_flow_core::sending::dkim_record_text;

        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, public_key) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "sent");
        let captured = sink.messages();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].mail_from, "news@example.com");
        assert_eq!(captured[0].recipients, ["ada@example.com"]);
        let data = &captured[0].data;
        assert_eq!(
            verify_dkim_signature(data, &dkim_record_text(&public_key)),
            Ok(())
        );
        let signature = header_of(data, "DKIM-Signature").unwrap();
        assert!(signature.contains("d=example.com;") && signature.contains("s=zirv;"));
        let id = queued.message.to_string();
        assert_eq!(header_of(data, "X-Zirv-Idempotency-Key"), Some(id.clone()));
        assert_eq!(
            header_of(data, "Message-ID"),
            Some(format!("<{id}@example.com>"))
        );
        assert!(
            header_of(data, "List-Unsubscribe")
                .unwrap()
                .starts_with("<https://flow.example.test/u/")
        );
        assert_eq!(
            header_of(data, "List-Unsubscribe-Post").as_deref(),
            Some("List-Unsubscribe=One-Click")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retried_smtp_delivery_reuses_the_message_id_and_the_idempotency_key(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_next(SmtpSinkStage::DataEnd, "451 4.3.0 try again");

        env.attempt().await.unwrap_err();
        env.attempt().await.unwrap();

        let offered = sink.offered();
        assert_eq!(offered.len(), 2, "the message went to the server twice");
        let id = queued.message.to_string();
        for data in &offered {
            assert_eq!(header_of(data, "X-Zirv-Idempotency-Key"), Some(id.clone()));
            assert_eq!(
                header_of(data, "Message-ID"),
                Some(format!("<{id}@example.com>"))
            );
        }
        assert_eq!(sink.messages().len(), 1);
        assert_eq!(env.message(queued.message).await.status, "sent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reply_lost_after_the_final_dot_on_every_attempt_ends_delivery_unconfirmed(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 2).await;
        sink.reply_at(SmtpSinkStage::DataEnd, SMTP_SINK_DROP);

        env.attempt().await.unwrap_err();
        assert_eq!(
            env.message(queued.message).await.status,
            "sending",
            "a first lost reply is retried"
        );
        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert!(env.events("email.bounced").await.is_empty());
        assert_eq!(env.count("SELECT count(*) FROM suppressions").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reply_lost_once_and_a_250_on_the_retry_ends_sent_under_one_message_id(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 3).await;
        sink.reply_next(SmtpSinkStage::DataEnd, SMTP_SINK_DROP);

        env.attempt().await.unwrap_err();
        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "sent");
        let captured = sink.messages();
        assert_eq!(captured.len(), 2, "the server saw the message twice");
        let id = queued.message.to_string();
        for message in &captured {
            assert_eq!(
                header_of(&message.data, "Message-ID"),
                Some(format!("<{id}@example.com>"))
            );
            assert_eq!(
                header_of(&message.data, "X-Zirv-Idempotency-Key"),
                Some(id.clone())
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transport_failure_after_a_lost_reply_still_ends_delivery_unconfirmed(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 2).await;
        sink.reply_next(SmtpSinkStage::DataEnd, SMTP_SINK_DROP);

        env.attempt().await.unwrap_err();
        // The server goes away: the next attempt cannot connect.
        drop(sink);
        tokio::time::sleep(Duration::from_millis(200)).await;
        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed")),
            "a refused connection does not show that the first attempt was refused"
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resumed_message_that_gets_a_hard_bounce_suppresses_the_address(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 3).await;
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        sink.reply_at(SmtpSinkStage::Rcpt, "550 5.1.1 no such user");

        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("hard_bounce"))
        );
        assert_eq!(suppressions(&env).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resumed_message_on_a_worker_without_the_identifier_key_is_retried_not_bounced_unsuppressed(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (mut env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        let queued = env.queue(person, template, 3).await;
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        sink.reply_at(SmtpSinkStage::Rcpt, "550 5.1.1 no such user");
        env.attempt().await.unwrap_err();
        env.no_key = true;

        env.attempt().await.unwrap_err();

        assert_eq!(
            env.message(queued.message).await.status,
            "sending",
            "the bounce is not written without its suppression"
        );
        assert_eq!(suppressions(&env).await, 0);
    }

    /// Runs a first attempt that loses the server's reply, so the message is `sending` and marked
    /// unconfirmed, and returns it queued for more attempts.
    async fn after_a_lost_reply(env: &Env, sink: &SmtpSink, attempts: i32) -> Queued1 {
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, attempts).await;
        sink.reply_next(SmtpSinkStage::DataEnd, SMTP_SINK_DROP);
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        queued
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_login_refused_after_a_lost_reply_ends_delivery_unconfirmed(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let queued = after_a_lost_reply(&env, &sink, 3).await;
        sqlx::query(
            "UPDATE credentials SET fields = jsonb_set(fields, '{username}', '\"nobody\"')",
        )
        .execute(&env.pool)
        .await
        .unwrap();

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed")),
            "a refused login does not show that the first copy was refused"
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert!(env.events("email.bounced").await.is_empty());
        let note = ended.gate_results.to_string();
        assert!(note.contains("destination_auth"), "{note}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn temporary_refusals_after_a_lost_reply_end_delivery_unconfirmed_not_as_a_soft_bounce(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let queued = after_a_lost_reply(&env, &sink, 3).await;
        sink.reply_at(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");

        env.attempt().await.unwrap_err();
        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert!(env.events("email.bounced").await.is_empty());
        assert_eq!(suppressions(&env).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_550_5_1_1_after_a_lost_reply_is_still_a_hard_bounce_and_suppresses(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let queued = after_a_lost_reply(&env, &sink, 3).await;
        sink.reply_at(SmtpSinkStage::Rcpt, "550 5.1.1 no such user");

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("hard_bounce"))
        );
        assert_eq!(env.events("email.bounced").await.len(), 1);
        assert_eq!(suppressions(&env).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_call_cut_off_before_its_outcome_was_known_makes_the_next_definitive_refusal_unconfirmed(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        env.attempt().await.unwrap_err();
        assert!(
            !message_in_flight(&env.pool, env.ws, queued.message)
                .await
                .unwrap(),
            "a 4xx is a definitive non-acceptance and clears the mark"
        );
        // An attempt that wrote its pre-call mark and was cut off before it knew the outcome.
        mark_message_sending(&env.pool, env.ws, queued.message, 1, None)
            .await
            .unwrap();
        sink.reply_at(SmtpSinkStage::Mail, "550 5.7.1 sender refused");

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed")),
            "the cut-off call may have been accepted"
        );
        assert!(ended.gate_results.to_string().contains("smtp_rejected"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn plain_4xx_retries_clear_the_mark_so_a_later_refusal_ends_with_its_real_reason(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        sink.reply_next(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        env.attempt().await.unwrap_err();
        env.attempt().await.unwrap_err();
        sink.reply_at(SmtpSinkStage::Mail, "550 5.7.1 sender refused");

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("smtp_rejected"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_first_attempt_with_a_definitive_refusal_ends_with_its_real_reason(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        sink.reply_at(SmtpSinkStage::Mail, "550 5.7.1 sender refused");

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("smtp_rejected"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resumed_message_whose_destination_was_archived_ends_delivery_unconfirmed(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "lose_first_answer"}), 100, 100).await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        env.attempt().await.unwrap_err();
        assert_eq!(env.message(queued.message).await.status, "sending");
        zirv_flow_store::archive_destination(&env.pool, env.ws, env.destination)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
        assert!(
            message
                .gate_results
                .to_string()
                .contains("destination_unavailable")
        );
        assert_eq!(fake_ledger_calls(&env.ledger).len(), 1, "no second call");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_without_the_identifier_key_still_ends_an_unavailable_template_as_such(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.no_key = true;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 3).await;
        sqlx::query("UPDATE templates SET archived_at = now() WHERE id = $1")
            .bind(template)
            .execute(&env.pool)
            .await
            .unwrap();

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("template_unavailable"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hard_bounce_ends_the_message_as_email_bounced_and_suppresses_the_address_in_the_organization(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        sink.reply_at(SmtpSinkStage::Rcpt, "550 5.1.1 no such user");

        let bounced = env.send(person, template).await;

        assert_eq!(
            (bounced.status.as_str(), bounced.reason.as_deref()),
            ("failed", Some("hard_bounce"))
        );
        let events = env.events("email.bounced").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["bounce"], "hard");
        assert_eq!(events[0]["reason"], "hard_bounce");
        assert!(env.events("email.failed").await.is_empty());
        assert_eq!(suppressions(&env).await, 1);
        assert_eq!(
            env.count(
                "SELECT count(*) FROM suppressions WHERE scope = 'organization' AND source = 'bounce'"
            )
            .await,
            1,
            "organization-wide (OQ-19)"
        );
        let connections = sink.connections();

        // The next message to the address is stopped by the suppression gate: no connection.
        let again = env.send(person, template).await;

        assert_eq!(
            (again.status.as_str(), again.reason.as_deref()),
            ("skipped", Some("suppressed"))
        );
        assert_eq!(
            again.deciding_rule.as_deref(),
            Some("suppression_organization")
        );
        assert_eq!(sink.connections(), connections);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_refusal_that_is_not_about_the_recipient_fails_the_message_without_suppressing(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        sink.reply_at(SmtpSinkStage::Rcpt, "554 5.7.1 relay access denied");

        let failed = env.send(person, template).await;

        assert_eq!(
            (failed.status.as_str(), failed.reason.as_deref()),
            ("failed", Some("smtp_rejected"))
        );
        assert_eq!(env.events("email.failed").await.len(), 1);
        assert!(env.events("email.bounced").await.is_empty());
        assert_eq!(
            env.count("SELECT count(*) FROM suppressions").await,
            0,
            "nobody is suppressed"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn temporary_refusals_retry_and_when_they_run_out_are_a_soft_bounce_without_suppression(
        pool: PgPool,
    ) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("marketing").await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;
        sink.reply_at(SmtpSinkStage::Rcpt, "451 4.7.1 greylisted, try later");
        let queued = env.queue(person, template, 2).await;

        env.attempt().await.unwrap_err();
        assert_eq!(
            env.message(queued.message).await.status,
            "sending",
            "a first 4xx is retried, not ended"
        );
        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (
                ended.status.as_str(),
                ended.reason.as_deref(),
                ended.attempts
            ),
            ("failed", Some("soft_bounce"), 2)
        );
        let events = env.events("email.bounced").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["bounce"], "soft");
        assert_eq!(env.count("SELECT count(*) FROM suppressions").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dkim_key_that_cannot_be_opened_is_retried_as_credential_unavailable_and_nothing_connects(
        pool: PgPool,
    ) {
        use zirv_flow_core::{Keyring, MasterKey};

        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (mut env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;

        // No master key at all, then another one: both are the retry of an unopenable credential.
        env.keyring = None;
        env.attempt().await.unwrap_err();
        env.keyring = Some(Arc::new(Keyring::new(MasterKey::generate(), None).unwrap()));
        env.attempt().await.unwrap_err();

        assert_eq!(sink.connections(), 0);
        assert_eq!(env.message(queued.message).await.status, "sending");
        let attempts = env.attempts_of(queued.message).await;
        assert_eq!(attempts.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_from_an_unverified_domain_is_skipped_before_any_connection(pool: PgPool) {
        let sink = SmtpSink::start(SmtpSinkOptions::default()).await;
        let (env, _) = smtp_env(pool, &sink).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        sqlx::query("UPDATE sending_domains SET status = 'unverified' WHERE workspace_id = $1")
            .bind(env.ws.as_uuid())
            .execute(&env.pool)
            .await
            .unwrap();

        let skipped = env.send(person, template).await;

        assert_eq!(
            (skipped.status.as_str(), skipped.reason.as_deref()),
            ("skipped", Some("sender_unverified"))
        );
        assert_eq!(sink.connections(), 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_destination_that_does_not_sign_never_has_the_domain_key_opened(pool: PgPool) {
        // The fake destination declares no signing, and the worker holds no master key: a key that
        // had to be opened would retry the send. It is sent.
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;

        let sent = env.send(person, template).await;

        assert_eq!(sent.status, "sent");
        assert!(env.keyring.is_none());
    }

    fn rule_id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn policy_rule(n: u128, name: &str, condition: Value) -> PolicyRule {
        PolicyRule {
            id: rule_id(n).to_string(),
            name: name.to_owned(),
            condition: Some(condition),
        }
    }

    /// The cooldown: a message was sent to the person in the last day.
    fn cooldown() -> PolicyRule {
        policy_rule(
            1,
            "Cooldown",
            json!({"v": 1, "messages": {"channel": "email",
                "agg": {"fn": "last_time", "op": "within_last", "value": "P1D"}}}),
        )
    }

    /// The account state: the `account` trait says closed.
    fn closed() -> PolicyRule {
        policy_rule(
            2,
            "Account closed",
            json!({"v": 1, "trait": "account", "op": "eq", "value": "closed"}),
        )
    }

    async fn define_account_trait(env: &Env) {
        zirv_flow_store::create_trait_definition(
            &env.pool,
            env.ws,
            &zirv_flow_store::NewTraitDefinition {
                key: "account".into(),
                trait_type: zirv_flow_core::people::TraitType::String,
                precedence: zirv_flow_core::people::Precedence::Latest,
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

    async fn person_in_account_state(env: &Env, email: &str, account: &str) -> Uuid {
        env.profile(ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: json!({"account": account}).as_object().cloned().unwrap(),
            observed_at: env.clock.now() - Span::days(1),
            trace: TraceContext::root(Cause::api("t")),
        })
        .await
    }

    /// Saves and publishes the policy with `rules` as the next version.
    async fn publish_policy(env: &Env, rules: &[PolicyRule]) {
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        zirv_flow_store::save_consent_policy_draft(&mut tx, env.ws, rules, None)
            .await
            .unwrap();
        let locked = zirv_flow_store::lock_consent_policy(&mut tx, env.ws)
            .await
            .unwrap()
            .unwrap();
        zirv_flow_store::publish_consent_policy(
            &mut tx,
            env.ws,
            &locked,
            None,
            &TraceContext::root(Cause::definition_change(locked.id)),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
    }

    /// The `send.gate` decision of the consent gate of a message: the rule id it names, if any.
    async fn gate_rule(env: &Env, message: Uuid) -> Option<String> {
        sqlx::query_scalar(
            "SELECT subject_ids ->> 'rule' FROM decision_records \
             WHERE workspace_id = $1 AND action = 'send.gate' AND reason = 'consent' \
               AND subject_ids ->> 'message' = $2",
        )
        .bind(env.ws.as_uuid())
        .bind(message.to_string())
        .fetch_one(&env.pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_policy_of_recorded_consent_a_cooldown_and_an_account_state_decides_sends_and_each_skip_names_its_rule(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        define_account_trait(&env).await;
        let template = env.plain_template("marketing").await;
        publish_policy(
            &env,
            &[PolicyRule::recorded_consent(), cooldown(), closed()],
        )
        .await;
        let open = person_in_account_state(&env, "open@example.com", "open").await;
        let shut = person_in_account_state(&env, "shut@example.com", "closed").await;
        let silent = person_in_account_state(&env, "silent@example.com", "open").await;
        for person in [open, shut] {
            env.subscribe(person).await;
        }

        let first = env.send(open, template).await;
        let again = env.send(open, template).await;
        let closed_account = env.send(shut, template).await;
        let unconsented = env.send(silent, template).await;

        assert_eq!(first.status, "sent");
        assert_eq!(first.deciding_rule.as_deref(), Some("recorded_consent"));
        assert_eq!(again.status, "skipped");
        assert_eq!(again.reason.as_deref(), Some("no_consent"));
        assert_eq!(again.deciding_rule.as_deref(), Some("Cooldown"));
        assert_eq!(
            gate_rule(&env, again.id).await,
            Some(rule_id(1).to_string())
        );
        assert_eq!(closed_account.status, "skipped");
        assert_eq!(closed_account.reason.as_deref(), Some("no_consent"));
        assert_eq!(
            closed_account.deciding_rule.as_deref(),
            Some("Account closed")
        );
        assert_eq!(
            gate_rule(&env, closed_account.id).await,
            Some(rule_id(2).to_string())
        );
        // The built-in rule is first, so it names the denial of a person who never consented.
        assert_eq!(unconsented.status, "skipped");
        assert_eq!(unconsented.deciding_rule.as_deref(), Some("topic_default"));
        assert_eq!(env.sent().len(), 1);
        // Once the day has passed the cooldown no longer matches.
        env.clock.advance(Span::days(2));
        let later = env.send(open, template).await;
        assert_eq!(later.status, "sent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reordering_the_rules_changes_which_rule_is_named_not_whether_the_send_is_denied(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        define_account_trait(&env).await;
        let template = env.plain_template("marketing").await;
        // Closed, and never consented: the built-in rule and the account rule both match.
        let person = person_in_account_state(&env, "both@example.com", "closed").await;

        publish_policy(&env, &[PolicyRule::recorded_consent(), closed()]).await;
        let builtin_first = env.send(person, template).await;
        publish_policy(&env, &[closed(), PolicyRule::recorded_consent()]).await;
        let account_first = env.send(person, template).await;

        assert_eq!(builtin_first.status, "skipped");
        assert_eq!(account_first.status, "skipped");
        assert_eq!(builtin_first.reason, account_first.reason);
        assert_eq!(
            builtin_first.deciding_rule.as_deref(),
            Some("topic_default")
        );
        assert_eq!(
            account_first.deciding_rule.as_deref(),
            Some("Account closed")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_vertical_scoped_unsubscribe_blocks_only_that_vertical_with_a_policy_in_place(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        define_account_trait(&env).await;
        zirv_flow_store::create_dimension(
            &env.pool,
            env.ws,
            &zirv_flow_store::NewDimension {
                key: "vertical".into(),
                name: "Vertical".into(),
            },
        )
        .await
        .unwrap();
        for key in ["retail", "b2b"] {
            zirv_flow_store::create_dimension_value(&env.pool, env.ws, "vertical", key, key)
                .await
                .unwrap();
        }
        let template = env.plain_template("marketing").await;
        publish_policy(&env, &[PolicyRule::recorded_consent(), closed()]).await;
        let person = person_in_account_state(&env, "ada@example.com", "open").await;
        env.subscribe(person).await;
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        set_consent(
            &mut tx,
            env.ws,
            &ConsentChange {
                dimensions: [("vertical".to_owned(), "retail".to_owned())]
                    .into_iter()
                    .collect(),
                person,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Unsubscribed,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: env.clock.now() - Span::minutes(5),
                actor: None,
            },
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let vertical = |value: &str| -> Dimensions {
            [("vertical".to_owned(), value.to_owned())]
                .into_iter()
                .collect()
        };

        let retail = env.send_with(person, template, &vertical("retail")).await;
        let b2b = env.send_with(person, template, &vertical("b2b")).await;

        assert_eq!(retail.status, "skipped");
        assert_eq!(retail.deciding_rule.as_deref(), Some("recorded_consent"));
        assert_eq!(b2b.status, "sent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rule_that_can_no_longer_be_compiled_denies_the_send_and_is_named(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        let template = env.plain_template("marketing").await;
        // The `ghost` trait does not exist (it was archived since the policy was published).
        publish_policy(
            &env,
            &[
                PolicyRule::recorded_consent(),
                policy_rule(
                    3,
                    "Ghost",
                    json!({"v": 1, "trait": "ghost", "op": "exists"}),
                ),
            ],
        )
        .await;
        let person = env.person("ada@example.com").await;
        env.subscribe(person).await;

        let message = env.send(person, template).await;

        assert_eq!(message.status, "skipped");
        assert_eq!(message.deciding_rule.as_deref(), Some("Ghost"));
        // The denial says the rule could not be read, apart from a rule whose condition held.
        assert!(
            message
                .gate_results
                .to_string()
                .contains("policy_rule_unevaluable"),
            "{}",
            message.gate_results
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_that_is_not_published_does_not_decide_sends(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        define_account_trait(&env).await;
        let template = env.plain_template("marketing").await;
        let person = person_in_account_state(&env, "ada@example.com", "closed").await;
        env.subscribe(person).await;
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        zirv_flow_store::save_consent_policy_draft(
            &mut tx,
            env.ws,
            &[PolicyRule::recorded_consent(), closed()],
            None,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let message = env.send(person, template).await;

        assert_eq!(message.status, "sent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transactional_send_skips_the_policy_with_consent(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        define_account_trait(&env).await;
        let template = env.plain_template("transactional").await;
        publish_policy(&env, &[PolicyRule::recorded_consent(), closed()]).await;
        let person = person_in_account_state(&env, "ada@example.com", "closed").await;

        let message = env.send(person, template).await;

        assert_eq!(message.status, "sent");
        assert_eq!(message.deciding_rule.as_deref(), Some("transactional"));
    }

    // ---- Frequency caps and quiet hours (gates 5 and 6, #221). ----

    fn hm(hour: u8, minute: u8) -> time::Time {
        time::Time::from_hms(hour, minute, 0).unwrap()
    }

    const EVERY_DAY: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

    impl Env {
        /// Replaces the workspace's caps (window in hours, limit, mode) and quiet hours.
        async fn limits(
            &self,
            caps: &[(i32, i32, zirv_flow_store::CapMode)],
            quiet: Option<(time::Time, time::Time)>,
        ) {
            let caps: Vec<zirv_flow_store::NewFrequencyCap> = caps
                .iter()
                .map(
                    |(window_hours, limit, mode)| zirv_flow_store::NewFrequencyCap {
                        channel: "email".to_owned(),
                        window_hours: *window_hours,
                        limit: *limit,
                        mode: *mode,
                    },
                )
                .collect();
            let quiet = quiet.map(|(start, end)| zirv_flow_store::QuietHours {
                start,
                end,
                days: EVERY_DAY.iter().map(|day| (*day).to_owned()).collect(),
            });
            let mut connection = self.pool.acquire().await.unwrap();
            let revision = zirv_flow_store::read_send_limits(&mut connection, self.ws)
                .await
                .unwrap()
                .revision;
            zirv_flow_store::replace_send_limits(
                &mut connection,
                self.ws,
                revision,
                &caps,
                quiet.as_ref(),
            )
            .await
            .unwrap()
            .unwrap();
        }

        /// A subscribed person with an email address.
        async fn subscriber(&self, email: &str) -> Uuid {
            let person = self.person(email).await;
            self.subscribe(person).await;
            person
        }

        async fn set_zone(&self, person: Uuid, zone: &str) {
            sqlx::query("UPDATE people SET traits = traits || jsonb_build_object('timezone', $2::text) WHERE id = $1")
                .bind(person)
                .bind(zone)
                .execute(&self.pool)
                .await
                .unwrap();
        }

        async fn slot_of(&self, message: Uuid) -> Option<OffsetDateTime> {
            sqlx::query_scalar("SELECT cap_slot_at FROM messages WHERE id = $1")
                .bind(message)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// The queued `delivery.send` jobs: (unique key, payload, run_at, max_attempts).
        async fn send_jobs(&self) -> Vec<(Option<String>, Value, OffsetDateTime, i32)> {
            sqlx::query_as(
                "SELECT unique_key, payload, run_at, max_attempts FROM jobs \
                 WHERE kind = 'delivery.send' ORDER BY run_at",
            )
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// Makes the queued send due now, as the clock reaches its `run_at`.
        async fn due(&self) {
            sqlx::query("UPDATE jobs SET run_at = now() WHERE kind = 'delivery.send'")
                .execute(&self.pool)
                .await
                .unwrap();
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_in_quiet_hours_waits_for_their_end_releases_its_slot_and_is_sent_once(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        // 07:08 UTC, inside 22:00 to 08:00, with a cap in place so the message holds a slot.
        env.limits(
            &[(24, 5, zirv_flow_store::CapMode::Skip)],
            Some((hm(22, 0), hm(8, 0))),
        )
        .await;
        let person = env.subscriber("ada@example.com").await;
        let template = env.plain_template("marketing").await;
        let queued = env.queue(person, template, 3).await;

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "pending");
        assert_eq!(env.sent().len(), 0);
        assert_eq!(
            env.events("email.sent").await.len(),
            0,
            "a deferral is no event"
        );
        assert_eq!(
            env.slot_of(queued.message).await,
            None,
            "the quiet-hours deferral released the slot gate 5 took"
        );
        assert_eq!(gate_names(&message).last().unwrap(), "quiet_hours:deferred");
        let entry = message.gate_results.as_array().unwrap().last().unwrap();
        assert_eq!(entry["run_at"], "2031-05-06T08:00:00Z");
        let jobs = env.send_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].2, datetime!(2031-05-06 08:00 UTC));
        assert_eq!(jobs[0].3, 3, "a deferral keeps the whole attempt budget");
        assert_eq!(jobs[0].1["deferrals"], 1);
        assert!(
            jobs[0].1["first_wait"].is_null(),
            "the rate clock did not start"
        );
        assert_eq!(
            env.decisions(&["message.send"]).await.last().unwrap(),
            &(
                "message.send".to_owned(),
                "deferred".to_owned(),
                Some("quiet_hours".to_owned())
            )
        );

        // At 08:00 the copy runs, takes a slot again and sends the same message once.
        env.clock.set(datetime!(2031-05-06 08:00 UTC));
        env.due().await;
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "sent");
        assert_eq!(message.attempts, 1, "the wait cost no attempt");
        assert_eq!(
            env.slot_of(queued.message).await,
            Some(datetime!(2031-05-06 08:00 UTC))
        );
        assert_eq!(env.sent().len(), 1);
        assert_eq!(env.sent()[0].key, queued.message.to_string());
        assert_eq!(env.events("email.sent").await.len(), 1);
        assert_eq!(
            gate_names(&message),
            [
                "quiet_hours:deferred",
                "enrollment:passed",
                "template:passed",
                "sender:passed",
                "address:passed",
                "consent:passed",
                "suppression:passed",
                "quiet_hours:passed",
                "frequency_cap:passed",
                "render:passed",
                "rate_limit:passed",
                "delivery:passed",
            ],
            "the deferral stays in the log after the message ends"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn quiet_hours_are_judged_before_a_full_cap_and_take_no_slot(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[(24, 1, zirv_flow_store::CapMode::Defer)], None)
            .await;
        let person = env.subscriber("ada@example.com").await;
        let template = env.plain_template("marketing").await;
        assert_eq!(env.send(person, template).await.status, "sent");
        // 07:08 UTC is inside 22:00 to 08:00, and the cap of one is full.
        env.limits(
            &[(24, 1, zirv_flow_store::CapMode::Defer)],
            Some((hm(22, 0), hm(8, 0))),
        )
        .await;
        env.flow = Uuid::now_v7();
        env.publish_flow().await;
        let queued = env.queue(person, template, 3).await;

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "pending");
        assert_eq!(gate_names(&message), ["quiet_hours:deferred"]);
        assert_eq!(
            env.slot_of(queued.message).await,
            None,
            "a message that waits for the night never held a slot"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hold_by_quiet_hours_restarts_the_rate_clock_of_a_send_that_waited_for_a_token(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "ok"}), 1, 1).await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[], Some((hm(21, 0), hm(8, 0)))).await;
        env.clock.set(datetime!(2031-05-06 20:59:30 UTC));
        let template = env.plain_template("marketing").await;
        let first = env.subscriber("first@example.com").await;
        let second = env.subscriber("second@example.com").await;
        assert_eq!(env.send(first, template).await.status, "sent");
        let queued = env.queue(second, template, 5).await;

        // No token yet: the wait for one starts the rate clock.
        env.attempt().await.unwrap();
        assert!(env.send_jobs().await[0].1["first_wait"].is_string());

        // The re-run is inside the quiet period and is held until 08:00.
        env.clock.set(datetime!(2031-05-06 21:01 UTC));
        env.due().await;
        env.attempt().await.unwrap();
        assert!(env.send_jobs().await[0].1["first_wait"].is_null());

        env.clock.set(datetime!(2031-05-07 08:00 UTC));
        env.due().await;
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("sent", None)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_quiet_hours_deferral_longer_than_six_hours_does_not_fail_as_rate_limited(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        // 07:08 UTC, inside 01:00 to 23:00: a wait of almost sixteen hours.
        env.limits(&[], Some((hm(1, 0), hm(23, 0)))).await;
        let person = env.subscriber("ada@example.com").await;
        let template = env.plain_template("marketing").await;
        let queued = env.queue(person, template, 5).await;

        env.attempt().await.unwrap();
        env.clock.set(datetime!(2031-05-06 23:00 UTC));
        env.due().await;
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("sent", None)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_zone_overrides_the_workspace_zone_and_an_invalid_one_falls_back_and_says_so(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        // 07:08 UTC is inside 22:00 to 08:00 in UTC, and 16:08 in Tokyo.
        env.limits(&[], Some((hm(22, 0), hm(8, 0)))).await;
        let template = env.plain_template("marketing").await;
        let tokyo = env.subscriber("tokyo@example.com").await;
        env.set_zone(tokyo, "Asia/Tokyo").await;
        let broken = env.subscriber("broken@example.com").await;
        env.set_zone(broken, "Not/AZone").await;

        let sent = env.send(tokyo, template).await;
        let held = env.queue(broken, template, 5).await;
        env.attempt().await.unwrap();

        assert_eq!(
            sent.status, "sent",
            "the person's zone, not the workspace's"
        );
        assert!(
            sent.gate_results
                .as_array()
                .unwrap()
                .iter()
                .all(|gate| gate.get("note").is_none())
        );
        let message = env.message(held.message).await;
        assert_eq!(
            message.status, "pending",
            "the workspace zone applies and it is night there"
        );
        let entry = message.gate_results.as_array().unwrap().last().unwrap();
        assert_eq!(entry["gate"], "quiet_hours");
        assert_eq!(entry["note"], "timezone_fallback");
        assert!(env.decisions(&["message.send"]).await.contains(&(
            "message.send".to_owned(),
            "noted".to_owned(),
            Some("timezone_fallback".to_owned())
        )));

        // Past the end of the quiet period the fallback is on the passed gate record too.
        env.clock.set(datetime!(2031-05-06 08:00 UTC));
        env.due().await;
        env.attempt().await.unwrap();
        let message = env.message(held.message).await;
        assert_eq!(message.status, "sent");
        let passed = message
            .gate_results
            .as_array()
            .unwrap()
            .iter()
            .find(|gate| gate["gate"] == "quiet_hours" && gate["result"] == "passed")
            .unwrap();
        assert_eq!(passed["note"], "timezone_fallback");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_send_that_waits_for_a_rate_limit_token_keeps_its_slot_and_takes_no_second(
        pool: PgPool,
    ) {
        let mut env = Env::with(pool, "one", json!({"mode": "ok"}), 1, 1).await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[(24, 1, zirv_flow_store::CapMode::Skip)], None)
            .await;
        let template = env.plain_template("marketing").await;
        let first = env.subscriber("first@example.com").await;
        let second = env.subscriber("second@example.com").await;
        assert_eq!(env.send(first, template).await.status, "sent");
        let queued = env.queue(second, template, 5).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(queued.message).await.status, "pending");
        let taken = env.clock.now();
        assert_eq!(env.slot_of(queued.message).await, Some(taken));

        env.clock.advance(Span::seconds(5));
        env.due().await;
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            message.status, "sent",
            "the cap of one counts the message once, not twice"
        );
        assert_eq!(
            env.slot_of(queued.message).await,
            Some(env.clock.now()),
            "the re-run moved its one slot to the time it sends"
        );
        assert_eq!(
            env.count("SELECT count(*) FROM messages WHERE cap_slot_at IS NOT NULL")
                .await,
            2,
            "one slot for each person"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cap_in_defer_mode_holds_the_second_message_until_the_first_leaves_the_window(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[(24, 1, zirv_flow_store::CapMode::Defer)], None)
            .await;
        let person = env.subscriber("ada@example.com").await;
        let template = env.plain_template("marketing").await;
        let first = env.queue(person, template, 3).await;
        env.attempt().await.unwrap();
        env.flow = Uuid::now_v7();
        env.publish_flow().await;
        let second = env.queue(person, template, 3).await;

        env.attempt().await.unwrap();

        assert_eq!(env.message(first.message).await.status, "sent");
        assert_eq!(env.message(second.message).await.status, "pending");
        assert_eq!(env.slot_of(second.message).await, None);
        let jobs = env.send_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].2, env.clock.now() + Span::hours(24));
        assert_eq!(jobs[0].3, 3);
        assert!(jobs[0].1["first_wait"].is_null());
        assert_eq!(
            env.decisions(&["message.send"]).await.last().unwrap(),
            &(
                "message.send".to_owned(),
                "deferred".to_owned(),
                Some("frequency_cap".to_owned())
            )
        );

        env.clock.advance(Span::hours(24));
        env.due().await;
        env.attempt().await.unwrap();

        assert_eq!(env.message(second.message).await.status, "sent");
        assert_eq!(env.sent().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cap_in_skip_mode_skips_the_second_message_and_a_transactional_one_is_exempt(
        pool: PgPool,
    ) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(
            &[(24, 1, zirv_flow_store::CapMode::Skip)],
            Some((hm(1, 0), hm(23, 0))),
        )
        .await;
        env.clock.set(datetime!(2031-05-06 23:30 UTC));
        let person = env.subscriber("ada@example.com").await;
        let marketing = env.plain_template("marketing").await;
        let receipt = env.plain_template("transactional").await;

        assert_eq!(env.send(person, marketing).await.status, "sent");
        env.flow = Uuid::now_v7();
        env.publish_flow().await;
        let skipped = env.send(person, marketing).await;
        // 23:30 is outside the quiet period; the receipt is exempt from the cap as well.
        env.flow = Uuid::now_v7();
        env.publish_flow().await;
        let exempt = env.send(person, receipt).await;

        assert_eq!(
            (skipped.status.as_str(), skipped.reason.as_deref()),
            ("skipped", Some("frequency_cap"))
        );
        assert_eq!(
            gate_names(&skipped).last().unwrap(),
            "frequency_cap:blocked"
        );
        assert_eq!(skipped.attempts, 0);
        assert_eq!(env.slot_of(skipped.id).await, None);
        assert_eq!(exempt.status, "sent");
        assert_eq!(
            env.slot_of(exempt.id).await,
            None,
            "a receipt holds no slot"
        );
        assert_eq!(env.sent().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_transactional_send_is_not_held_by_quiet_hours(pool: PgPool) {
        let mut env = Env::new(pool, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[], Some((hm(1, 0), hm(23, 0)))).await;
        let person = env.person("receipt@example.com").await;
        let template = env.plain_template("transactional").await;

        let sent = env.send(person, template).await;

        assert_eq!(sent.status, "sent");
    }

    /// Sends from `flows` flows to one person at once, each from its own task, with a pool large
    /// enough that no task waits for a connection another holds.
    async fn concurrent_sends(
        pool: PgPool,
        flows: usize,
        mode: zirv_flow_store::CapMode,
    ) -> (Arc<Env>, Vec<Uuid>) {
        let big = sqlx::postgres::PgPoolOptions::new()
            .max_connections(64)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let mut env = Env::new(big, "one").await;
        env.render = Some(start_pool("serve", "").await);
        env.limits(&[(24, 1, mode)], None).await;
        let person = env.subscriber("ada@example.com").await;
        let template = env.plain_template("marketing").await;
        let mut messages = Vec::new();
        for _ in 0..flows {
            env.flow = Uuid::now_v7();
            env.publish_flow().await;
            messages.push(env.queue(person, template, 3).await.message);
        }
        let env = Arc::new(env);
        let runs = (0..flows).map(|_| {
            let env = Arc::clone(&env);
            tokio::spawn(async move {
                let claimed = env.claim().await.expect("a job for every task");
                env.execute(claimed).await
            })
        });
        for run in futures_util::future::join_all(runs).await {
            run.unwrap().unwrap();
        }
        (env, messages)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sixteen_flows_sending_to_one_person_at_once_never_exceed_a_cap_of_one_in_skip_mode(
        pool: PgPool,
    ) {
        let (env, messages) = concurrent_sends(pool, 16, zirv_flow_store::CapMode::Skip).await;

        let mut ended = Vec::new();
        for id in &messages {
            let message = env.message(*id).await;
            ended.push((message.status, message.reason));
        }
        ended.sort();
        let mut expected = vec![("skipped".to_owned(), Some("frequency_cap".to_owned())); 15];
        expected.push(("sent".to_owned(), None));
        expected.sort();
        assert_eq!(ended, expected);
        assert_eq!(env.sent().len(), 1);
        assert_eq!(
            env.count("SELECT count(*) FROM messages WHERE cap_slot_at IS NOT NULL")
                .await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sixteen_flows_sending_to_one_person_at_once_send_one_and_defer_the_rest_in_defer_mode(
        pool: PgPool,
    ) {
        let (env, messages) = concurrent_sends(pool, 16, zirv_flow_store::CapMode::Defer).await;

        let mut statuses = Vec::new();
        for id in &messages {
            statuses.push(env.message(*id).await.status);
        }
        statuses.sort();
        let mut expected = vec!["pending".to_owned(); 15];
        expected.push("sent".to_owned());
        expected.sort();
        assert_eq!(statuses, expected);
        assert_eq!(env.sent().len(), 1);
        let jobs = env.send_jobs().await;
        assert_eq!(
            jobs.len(),
            15,
            "one deferral copy for each message that waits"
        );
        assert!(jobs.iter().all(|job| job.1["deferrals"] == 1));
        assert!(
            jobs.iter()
                .all(|job| job.0.as_deref().unwrap().ends_with(":defer:1")),
            "{jobs:?}"
        );
    }

    // ---- provider destinations (#223) ----

    /// The fixture manifest of a provider with a timestamped feedback signature, pointed at `fake`.
    fn provider_config(fake: &zirv_flow_connectors::destinations::FakeProvider) -> Value {
        let text = include_str!(
            "../../../../zirv-flow-connectors/tests/fixtures/providers/bearer_timestamped.json"
        )
        .replace(
            "https://api.fixture-provider.example.test",
            &fake.base_url(),
        );
        json!({"manifest": serde_json::from_str::<Value>(&text).unwrap()})
    }

    /// An environment whose destination is a `provider` one, with a sealed workspace credential
    /// holding the API token and the feedback signing secret.
    async fn provider_env(
        pool: PgPool,
        name: &str,
        fake: &zirv_flow_connectors::destinations::FakeProvider,
    ) -> Env {
        use zirv_flow_core::{Keyring, SecretId, SecretPurpose, seal, secret_aad};
        use zirv_flow_store::secrets::insert_unchecked;
        use zirv_flow_store::{NewWorkspaceCredential, create_credential};

        let mut env = Env::new(pool, name).await;
        env.render = Some(start_pool("serve", "").await);
        let keys = Arc::new(Keyring::new(MasterKey::generate(), None).unwrap());
        env.keyring = Some(Arc::clone(&keys));
        let secret = SecretId::new();
        let purpose = SecretPurpose::WorkspaceCredential;
        let sealed = seal(
            &keys,
            br#"{"token":"tok-1","feedback_secret":"whsec-1"}"#,
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
                key: "provider",
                name: "Provider",
                kind: "custom",
                schema: None,
                fields: &json!({}),
                secret_id: secret,
                hints: &json!({"token": "", "feedback_secret": ""}),
                fingerprint: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        env.destination = create_destination(
            &env.pool,
            env.ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "provider",
                name: "Provider",
                config: &provider_config(fake),
                credential_id: Some(credential.id),
                rate: 100,
                burst: 100,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        env
    }

    async fn feedback(
        env: &Env,
        message: Uuid,
        event: zirv_flow_store::FeedbackEvent,
    ) -> zirv_flow_store::FeedbackOutcome {
        zirv_flow_store::record_provider_feedback(
            &env.pool,
            &zirv_flow_store::ProviderFeedback {
                workspace: env.ws,
                destination: env.destination,
                message,
                event,
                provider_event_id: None,
                now: env.clock.now(),
                fallback_trace: &TraceContext::root(Cause::api("feedback")),
            },
        )
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_send_maps_the_message_and_stores_the_hash_of_the_address_it_went_to(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;

        let sent = env.send(person, template).await;

        assert_eq!(sent.status, "sent");
        let accepted = fake.accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].key, sent.id.to_string());
        assert_eq!(accepted[0].authorization.as_deref(), Some("Bearer tok-1"));
        assert_eq!(accepted[0].body["to"][0]["email"], "ada@example.com");
        assert_eq!(accepted[0].body["from"]["email"], "news@example.com");
        assert_eq!(accepted[0].body["custom"]["message_id"], json!(sent.id));
        let stored: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT address_hash FROM messages WHERE id = $1")
                .bind(sent.id)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some(crate_hash("ada@example.com").as_slice())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_delivery_retried_after_a_lost_reply_shows_zero_duplicates(pool: PgPool) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        fake.script(FakeProviderScript::DropAfterAccept);

        // The provider holds the message, but the reply is lost: the job is retried.
        let first = env.attempt().await.unwrap_err();
        assert_eq!(first.message(), "the request to the destination failed");
        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!((message.status.as_str(), message.attempts), ("sent", 2));
        assert_eq!(fake.requests(), 2, "the key was sent twice");
        assert_eq!(
            fake.accepted().len(),
            1,
            "zero duplicates: the provider honoured the key"
        );
        assert_eq!(fake.accepted()[0].key, queued.message.to_string());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_call_cut_off_after_the_write_ends_delivery_unconfirmed(pool: PgPool) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        // One attempt, so the retries are exhausted by the cut-off call.
        let queued = env.queue(person, template, 1).await;
        fake.script(FakeProviderScript::DropAfterAccept);

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed")),
            "the provider holds the message, so it is not a definite failure"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_call_refused_at_connect_still_ends_as_a_transport_failure(pool: PgPool) {
        use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 1).await;
        // The provider goes away: nothing listens, so the request never leaves.
        drop(fake);
        tokio::time::sleep(Duration::from_millis(200)).await;

        env.attempt().await.unwrap();

        let ended = env.message(queued.message).await;
        assert_eq!(
            (ended.status.as_str(), ended.reason.as_deref()),
            ("failed", Some("transport"))
        );
        assert!(
            !message_unconfirmed(&env.pool, env.ws, queued.message)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_ends_a_message_after_a_cut_off_provider_call_as_delivery_unconfirmed(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        fake.script(FakeProviderScript::DropAfterAccept);
        env.attempt().await.unwrap_err();
        assert!(
            message_unconfirmed(&env.pool, env.ws, queued.message)
                .await
                .unwrap(),
            "a call cut off after the write leaves the unconfirmed mark"
        );
        bury_the_jobs(&env).await;

        end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("delivery_unconfirmed"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sweep_ends_a_message_whose_provider_call_never_left_as_an_internal_error(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        drop(fake);
        tokio::time::sleep(Duration::from_millis(200)).await;
        env.attempt().await.unwrap_err();
        bury_the_jobs(&env).await;

        end_orphaned_messages(&env.pool, env.clock.now())
            .await
            .unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(
            (message.status.as_str(), message.reason.as_deref()),
            ("failed", Some("internal_error"))
        );
    }

    /// Gives the person a newer email identity, which the send address then follows.
    async fn move_address(env: &Env, person: Uuid, address: &str) {
        sqlx::query(
            "INSERT INTO identities (id, workspace_id, person_id, kind, value, created_at)
             VALUES ($1, $2, $3, 'email', $4, now() + interval '1 hour')",
        )
        .bind(Uuid::now_v7())
        .bind(env.ws.as_uuid())
        .bind(person)
        .bind(address)
        .execute(&env.pool)
        .await
        .unwrap();
    }

    async fn stored_hash(env: &Env, message: Uuid) -> Option<Vec<u8>> {
        sqlx::query_scalar("SELECT address_hash FROM messages WHERE id = $1")
            .bind(message)
            .fetch_one(&env.pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_carried_provider_retry_does_not_send_to_a_changed_address_or_move_the_hash(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        // One token a second and a burst of one, so the gate holds one token per take.
        sqlx::query("UPDATE destinations SET rate = 1, burst = 1 WHERE id = $1")
            .bind(env.destination)
            .execute(&env.pool)
            .await
            .unwrap();
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        fake.script(FakeProviderScript::DropAfterAccept);
        env.attempt().await.unwrap_err();
        // The call was cut off after the write, so the first attempt left the message carried.
        assert!(
            message_unconfirmed(&env.pool, env.ws, queued.message)
                .await
                .unwrap()
        );
        move_address(&env, person, "bob@example.com").await;
        // The first attempt spent the gate's only token; the bucket holds one for the next take.
        sqlx::query(
            "UPDATE destination_rate_buckets SET tokens = 1, refilled_at = $2
             WHERE destination_id = $1",
        )
        .bind(env.destination)
        .bind(env.clock.now())
        .execute(&env.pool)
        .await
        .unwrap();

        env.attempt().await.unwrap();

        let bucket = zirv_flow_store::bucket_state(&env.pool, env.destination)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            bucket.tokens, 1.0,
            "the ending made no call and took no token"
        );
        let message = env.message(queued.message).await;
        assert_eq!(message.status, "failed");
        assert_eq!(message.reason.as_deref(), Some(DELIVERY_UNCONFIRMED));
        assert_eq!(fake.requests(), 1, "no second call");
        assert_eq!(
            stored_hash(&env, queued.message).await.as_deref(),
            Some(crate_hash("ada@example.com").as_slice())
        );
        assert!(
            serde_json::to_string(&message.gate_results)
                .unwrap()
                .contains("address_changed")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_uncarried_provider_retry_sends_to_the_changed_address_and_the_hash_follows(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        fake.script(FakeProviderScript::Status {
            status: 503,
            body: json!({}),
            retry_after: None,
        });
        env.attempt().await.unwrap_err();
        move_address(&env, person, "bob@example.com").await;

        env.attempt().await.unwrap();

        let message = env.message(queued.message).await;
        assert_eq!(message.status, "sent");
        assert_eq!(fake.accepted().len(), 1);
        assert_eq!(fake.accepted()[0].body["to"][0]["email"], "bob@example.com");
        assert_eq!(
            stored_hash(&env, queued.message).await.as_deref(),
            Some(crate_hash("bob@example.com").as_slice())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_429_with_a_retry_after_slows_the_bucket_and_a_5xx_is_retried(pool: PgPool) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let queued = env.queue(person, template, 5).await;
        fake.script(FakeProviderScript::Status {
            status: 429,
            body: json!({}),
            retry_after: Some(30),
        });
        fake.script(FakeProviderScript::Status {
            status: 503,
            body: json!({}),
            retry_after: None,
        });

        let limited = env.attempt().await.unwrap_err();
        let unavailable = env.attempt().await.unwrap_err();
        env.attempt().await.unwrap();

        assert_eq!(limited.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(unavailable.retry_after(), None);
        assert_eq!(
            env.attempts_of(queued.message).await,
            [
                ("throttled".to_owned(), 1),
                ("retryable".to_owned(), 2),
                ("delivered".to_owned(), 3)
            ]
        );
        assert_eq!(fake.accepted().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_provider_recipient_failure_is_a_hard_bounce_that_suppresses_the_stored_hash_in_the_organization(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{
            FakeProvider, FakeProviderOptions, FakeProviderScript,
        };

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        fake.script(FakeProviderScript::Status {
            status: 422,
            body: json!({"error": {"code": "mailbox_unknown"}}),
            retry_after: None,
        });

        let bounced = env.send(person, template).await;

        assert_eq!(
            (bounced.status.as_str(), bounced.reason.as_deref()),
            ("failed", Some("hard_bounce"))
        );
        assert_eq!(env.events("email.bounced").await.len(), 1);
        assert_eq!(
            env.count(
                "SELECT count(*) FROM suppressions WHERE scope = 'organization' AND source = 'bounce'"
            )
            .await,
            1
        );
        assert!(fake.accepted().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hard_bounce_from_the_feedback_webhook_skips_the_next_send_in_every_workspace_and_a_complaint_only_its_own(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};
        use zirv_flow_store::FeedbackEvent;

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let one = provider_env(pool.clone(), "one", &fake).await;
        let mut two = Env::new(pool, "two").await;
        two.render = Some(start_pool("serve", "").await);
        let template_one = one.plain_template("transactional").await;
        let template_two = two.plain_template("transactional").await;
        let bounced = one.person("gone@example.com").await;
        let complained = one.person("angry@example.com").await;
        let sent_bounced = one.send(bounced, template_one).await;
        let sent_complained = one.send(complained, template_one).await;
        assert_eq!(
            (
                sent_bounced.status.as_str(),
                sent_complained.status.as_str()
            ),
            ("sent", "sent")
        );

        feedback(&one, sent_bounced.id, FeedbackEvent::BouncedHard).await;
        feedback(&one, sent_complained.id, FeedbackEvent::Complained).await;

        // The next send to the bounced address is skipped, from either workspace.
        let again = one.send(bounced, template_one).await;
        let gone_in_two = two.person("gone@example.com").await;
        let elsewhere = two.send(gone_in_two, template_two).await;
        for message in [&again, &elsewhere] {
            assert_eq!(
                (message.status.as_str(), message.reason.as_deref()),
                ("skipped", Some("suppressed"))
            );
            assert_eq!(
                message.deciding_rule.as_deref(),
                Some("suppression_organization")
            );
        }
        // The complaint stops its own workspace only.
        let own = one.send(complained, template_one).await;
        let angry_in_two = two.person("angry@example.com").await;
        let other = two.send(angry_in_two, template_two).await;
        assert_eq!(
            (
                own.status.as_str(),
                own.reason.as_deref(),
                own.deciding_rule.as_deref()
            ),
            ("skipped", Some("suppressed"), Some("suppression_workspace"))
        );
        assert_eq!(other.status, "sent");
        assert_eq!(fake.accepted().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn feedback_for_a_message_sent_before_the_hash_existed_suppresses_nobody_and_never_reads_the_current_address(
        pool: PgPool,
    ) {
        use zirv_flow_connectors::destinations::{FakeProvider, FakeProviderOptions};
        use zirv_flow_store::{FeedbackEvent, FeedbackOutcome, SuppressionMade};

        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let env = provider_env(pool, "one", &fake).await;
        let template = env.plain_template("transactional").await;
        let person = env.person("ada@example.com").await;
        let sent = env.send(person, template).await;
        sqlx::query("UPDATE messages SET address_hash = NULL WHERE id = $1")
            .bind(sent.id)
            .execute(&env.pool)
            .await
            .unwrap();

        let outcome = feedback(&env, sent.id, FeedbackEvent::BouncedHard).await;

        assert!(matches!(
            outcome,
            FeedbackOutcome::Recorded {
                suppression: SuppressionMade::Unable,
                ..
            }
        ));
        assert_eq!(env.count("SELECT count(*) FROM suppressions").await, 0);
        let again = env.send(person, template).await;
        assert_eq!(
            again.status, "sent",
            "the person's current address is not suppressed"
        );
    }
}
