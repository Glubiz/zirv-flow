//! Failure injection. Every job goes through a seeded plan of at least one failure before its
//! final successful run:
//!
//! - **rollback**: the handler runs in its transaction, which is then rolled back; the lease
//!   expires and the reaper returns the job;
//! - **commit without the ack**: the handler's work commits but the job is not completed, so the
//!   lease expires, the reaper returns the job and it runs again, which must change nothing;
//! - **concurrent duplicate**: worker A runs the handler and holds its transaction open while
//!   its lease expires and worker B claims the job and runs it to commit; A's completion must
//!   then find the lease gone, and A rolls back. The overlap is read from the lock state, not
//!   timed (see `concurrent_duplicate`).
//!
//! A `webhook.call` also meets the destination's own failures (see `mock`): a retryable answer
//! ends the attempt with the handler's error, and the worker's reaction follows, `fail_job`,
//! which spends an attempt like the others. The job keeps the production `max_attempts`, so the
//! lease expiries and the retries together can use them up: the last attempt records a failed
//! outcome, and a job whose attempts run out through lease expiry goes dead.
//!
//! The runs follow the worker's `execute`: begin, `lock_lease`, the handler, `complete_job`,
//! commit.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::schedule::backoff;
use zirv_flow_engine::sim::rng::{Rng, mix};
use zirv_flow_store::{
    ClaimedJob, Clock, ENROLL_JOB_KIND, FailOutcome, STEP_JOB_KIND, Tx, WEBHOOK_JOB_KIND,
    claim_job, commit, complete_job, fail_job, lock_lease, reap_expired,
};

use super::driver::{Driver, Due, LANE_PLAN, WakeMove};
use super::mock::Script;
use super::world::{EPOCH, World};
use crate::worker::retry_delay;
use crate::{JobContext, JobError, WorkerConfig};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Rollback,
    CommitWithoutAck,
}

impl Mode {
    const fn name(self) -> &'static str {
        match self {
            Self::Rollback => "rollback",
            Self::CommitWithoutAck => "commit without ack",
        }
    }
}

/// What the enrollment of a `flow.step` job looked like before the job ran.
struct Watch {
    enrollment: Uuid,
    version: i64,
    /// The version the job expects to find, from its payload.
    job_version: i64,
    wake_at: Option<OffsetDateTime>,
    is_wake: bool,
    /// The name of the event the job answers, when it answers one.
    event: Option<String>,
    /// For a webhook outcome event: the event, and the call (enrollment and node) it reports.
    outcome: Option<(Uuid, Uuid, String)>,
    /// The virtual time the job was claimed at, which is when its handler runs.
    at: OffsetDateTime,
}

/// Why a run did not end in a commit.
#[derive(Debug)]
pub(super) enum RunError {
    /// The destination's answer was one to retry: the handler returned its error, as a worker
    /// retries it, with the wait the destination asked for, if it did.
    Retry(String, Option<Duration>),
    /// Anything else.
    Failed(String),
}

/// How a job's final attempts ended.
enum Final {
    Done,
    /// A retryable answer: the job is back in the queue with an attempt spent. The flag is
    /// whether the destination's `Retry-After` set the delay.
    Retried(bool),
    /// The attempts ran out.
    Dead,
    Failed(String),
}

/// How the reaper left a job whose lease just ran out.
#[derive(Debug, Eq, PartialEq)]
enum Reaped {
    Requeued,
    Dead,
    Untouched,
}

/// The lease the driver gives a claim, in seconds.
const LEASE_SECONDS: i64 = 30;

/// The most attempts a job is given in its final stage before the run calls it stuck.
const MAX_FINAL_ROUNDS: usize = 8;

impl World {
    /// Runs the handler of `claimed` in `tx` the way the worker does.
    pub(super) async fn run_handler(
        &self,
        tx: &mut Tx,
        claimed: &ClaimedJob,
    ) -> Result<(), JobError> {
        let kind = crate::Registry::built_in()
            .get(&claimed.kind)
            .expect("the job kind is registered");
        let mut ctx = JobContext::new(tx, claimed)
            .with_clock(Clock::Manual(self.env.clock.clone()))
            .with_outbound(
                Some(self.keyring.clone()),
                self.egress.clone(),
                Arc::default(),
            );
        kind.run(&mut ctx, claimed.payload.clone()).await
    }

    /// Whether a handler's error is the retry of a `webhook.call`: the destination's answer, or
    /// a failed send. The worker retries every error; the other kinds have none to retry.
    fn is_retry(kind: &str, message: &str) -> bool {
        kind == WEBHOOK_JOB_KIND
            && (message.starts_with("the destination answered")
                || message.starts_with("the request to the destination"))
    }

    /// The final run: the handler, the ack and the commit in one transaction. The error is the
    /// handler's, after the transaction was rolled back.
    pub(super) async fn final_run(&self, claimed: &ClaimedJob) -> Result<(), RunError> {
        let mut tx = zirv_flow_store::begin(&self.env.pool).await.unwrap();
        if !lock_lease(&mut tx, claimed.id, claimed.lease_token)
            .await
            .unwrap()
        {
            return Err(RunError::Failed(
                "the lease was lost before the handler ran".to_owned(),
            ));
        }
        if let Err(error) = self.run_handler(&mut tx, claimed).await {
            drop(tx);
            let message = error.message().to_owned();
            return Err(if Self::is_retry(&claimed.kind, &message) {
                RunError::Retry(message, error.retry_after())
            } else {
                RunError::Failed(message)
            });
        }
        if !complete_job(&mut *tx, claimed.id, claimed.lease_token)
            .await
            .unwrap()
        {
            return Err(RunError::Failed(
                "the ack found the lease gone in the final run".to_owned(),
            ));
        }
        commit(tx).await.unwrap();
        Ok(())
    }

    /// The final run, and the worker's reaction to a retry: `fail_job`.
    async fn finish(&self, claimed: &ClaimedJob) -> Final {
        match self.final_run(claimed).await {
            Ok(()) => Final::Done,
            Err(RunError::Failed(message)) => Final::Failed(message),
            Err(RunError::Retry(message, asked)) => self.retry(claimed, &message, asked).await,
        }
    }

    /// Hands a failed attempt back to the queue as the worker does: after the delay the worker
    /// computes (`retry_delay`: its backoff, or the destination's `Retry-After` when that is
    /// longer, never past the cap). The job is then not claimable before the delay, which is
    /// checked, and the virtual clock moves on by it, as the time before the next attempt.
    async fn retry(&self, claimed: &ClaimedJob, message: &str, asked: Option<Duration>) -> Final {
        let config = WorkerConfig::new(1);
        let unit = f64::from(
            u32::try_from(
                mix(self.seed
                    ^ self.mock.name_of(key_of(&claimed.payload))
                    ^ u64::try_from(claimed.attempts).unwrap_or(0))
                    % 1000,
            )
            .unwrap_or(0),
        ) / 1000.0;
        let backoff = backoff(
            u32::try_from(claimed.attempts).unwrap_or(u32::MAX),
            config.backoff_base,
            config.backoff_cap,
            unit,
        );
        let delay = retry_delay(backoff, asked, config.backoff_cap);
        let before: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&self.env.pool)
            .await
            .unwrap();
        let failed = fail_job(
            &self.env.pool,
            claimed.id,
            claimed.lease_token,
            delay,
            message,
        )
        .await
        .unwrap();
        if failed == Some(FailOutcome::Retried) {
            let run_at: OffsetDateTime =
                sqlx::query_scalar("SELECT run_at FROM jobs WHERE id = $1")
                    .bind(claimed.id)
                    .fetch_one(&self.env.pool)
                    .await
                    .unwrap();
            let wanted = before + delay;
            if run_at < wanted - time::Duration::milliseconds(1) {
                return Final::Failed(format!(
                    "a retried attempt is claimable at {run_at}, before its delay of {delay:?} \
                     (not before {wanted}) is over"
                ));
            }
            // The wait passes before the next attempt.
            self.env
                .clock
                .advance(time::Duration::seconds_f64(delay.as_secs_f64()));
        }
        let by_retry_after = asked.is_some_and(|asked| asked.min(config.backoff_cap) > backoff);
        match failed {
            // The handler's last attempt records a failed outcome and returns Ok; a retry that
            // finds no attempt left is the handler dying with the enrollment never told.
            Some(FailOutcome::Dead) => Final::Failed(
                "the last attempt returned a retry instead of recording the failure".to_owned(),
            ),
            Some(FailOutcome::Retried) => Final::Retried(by_retry_after),
            None => Final::Failed("the lease was lost before the attempt was failed".to_owned()),
        }
    }

    /// Lets the lease of a running job run out and has the reaper return it.
    async fn expire_and_reap(&self, id: Uuid) -> Reaped {
        sqlx::query(
            "UPDATE jobs SET lease_expires_at = now() - interval '1 second' \
             WHERE id = $1 AND state = 'running'",
        )
        .bind(id)
        .execute(&self.env.pool)
        .await
        .unwrap();
        // The lease ran out in virtual time too: whatever claims the job next does so later than
        // the lease, as production's lease expiry puts it, so a repeat of the work is stamped
        // with a later instant and cannot hide behind the store's skip of an identical write.
        self.env
            .clock
            .advance(time::Duration::seconds(LEASE_SECONDS + 1));
        let reaped = reap_expired(&self.env.pool).await.unwrap();
        if reaped.dead >= 1 {
            Reaped::Dead
        } else if reaped.requeued >= 1 {
            Reaped::Requeued
        } else {
            Reaped::Untouched
        }
    }

    /// Makes the jobs the runtime enqueued "now" (the database's real time, long before the
    /// virtual dates) wait for the virtual instant like the rest, so only the job under test can
    /// be claimed.
    async fn hold_real_time_jobs(&self, at: OffsetDateTime) {
        sqlx::query(
            "UPDATE jobs SET run_at = $1 \
             WHERE workspace_id = $2 AND state = 'ready' AND run_at < $3",
        )
        .bind(at)
        .bind(self.env.ws.as_uuid())
        .bind(EPOCH - time::Duration::days(30))
        .execute(&self.env.pool)
        .await
        .unwrap();
    }

    /// A string that changes when the job's subject changes: the enrollment row, its steps,
    /// decisions, waits and the other jobs about it; for an enrollment job, the counts.
    async fn fingerprint(&self, claimed: &ClaimedJob) -> String {
        let pool = &self.env.pool;
        match claimed.kind.as_str() {
            STEP_JOB_KIND => {
                let enrollment = enrollment_of(&claimed.payload);
                sqlx::query_scalar(
                    "SELECT (to_jsonb(e) - 'updated_at')::text \
                         || '|' || (SELECT count(*) FROM step_executions s \
                                    WHERE s.enrollment_id = e.id) \
                         || '|' || (SELECT count(*) FROM decision_records d \
                                    WHERE d.workspace_id = e.workspace_id \
                                      AND d.subject_ids ->> 'enrollment' = e.id::text \
                                      AND d.action <> 'job.attempt') \
                         || '|' || (SELECT count(*) FROM enrollment_waits w \
                                    WHERE w.enrollment_id = e.id) \
                         || '|' || coalesce((SELECT string_agg(j.unique_key, ',' \
                                                    ORDER BY j.unique_key) FROM jobs j \
                                             WHERE j.payload ->> 'enrollment_id' = e.id::text \
                                               AND j.id <> $2), '') \
                     FROM enrollments e WHERE e.id = $1",
                )
                .bind(enrollment)
                .bind(claimed.id)
                .fetch_optional(pool)
                .await
                .unwrap()
                .unwrap_or_default()
            }
            // A call may be sent again and then records its outcome again (at-least-once), so
            // the events are not compared; the call itself is: its key and the request body the
            // destination last received must be what a re-run sends again.
            WEBHOOK_JOB_KIND => {
                let key = key_of(&claimed.payload);
                let body = self.mock.last_body(key).unwrap_or_default();
                format!("{key}|{}", String::from_utf8_lossy(&body))
            }
            _ => sqlx::query_scalar(
                "SELECT (SELECT count(*) FROM enrollments WHERE workspace_id = $1)::text \
                     || '|' || (SELECT count(*) FROM jobs \
                                WHERE workspace_id = $1 AND kind = 'flow.step' AND id <> $2)",
            )
            .bind(self.env.ws.as_uuid())
            .bind(claimed.id)
            .fetch_one(pool)
            .await
            .unwrap(),
        }
    }

    /// How many outcome events (`webhook.succeeded` or `webhook.failed`) are committed for the
    /// call of a `webhook.call` payload.
    async fn outcome_events(&self, payload: &Value) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM events \
             WHERE workspace_id = $1 AND name IN ('webhook.succeeded', 'webhook.failed') \
               AND properties ->> 'enrollment_id' = $2 AND properties ->> 'node_id' = $3",
        )
        .bind(self.env.ws.as_uuid())
        .bind(enrollment_of(payload).to_string())
        .bind(
            payload
                .get("node_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
        .fetch_one(&self.env.pool)
        .await
        .unwrap()
    }
}

fn enrollment_of(payload: &Value) -> Uuid {
    payload
        .get("enrollment_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .unwrap_or_default()
}

fn key_of(payload: &Value) -> &str {
    payload
        .get("idempotency_key")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The plan of one job: the failures that come before its final run, and whether the final run
/// is a concurrent duplicate.
struct Plan {
    modes: Vec<Mode>,
    concurrent: bool,
    fenced: bool,
}

impl Plan {
    /// Drawn from the seed. At least one failure before the final run, for every kind of job: the
    /// seed picks how many and which, never none. A `webhook.call` may be given enough of them
    /// for the reaper to use up its attempts; the other kinds stay below, as a stuck enrollment
    /// is not a result the run accepts.
    fn draw(rng: &mut Rng, kind: &str) -> Self {
        let roll = rng.below(100);
        let failures = if kind == WEBHOOK_JOB_KIND {
            match roll {
                0..=34 => 1,
                35..=64 => 2,
                65..=84 => 3,
                85..=94 => 4,
                _ => 5,
            }
        } else {
            match roll {
                0..=44 => 1,
                45..=79 => 2,
                _ => 3,
            }
        };
        let modes = (0..failures)
            .map(|_| {
                if rng.chance(50) {
                    Mode::Rollback
                } else {
                    Mode::CommitWithoutAck
                }
            })
            .collect();
        Self {
            modes,
            // An enrollment job is rare, so every one of them takes part in a concurrent
            // duplicate, half of them fenced: each variant is then seen for that kind too.
            concurrent: kind == ENROLL_JOB_KIND || rng.chance(25),
            fenced: rng.chance(50),
        }
    }
}

/// How a failed attempt ended.
enum Attempt {
    /// The handler's transaction ended as planned. The fingerprint is that of the committed
    /// state for a commit without the ack.
    Ended(String),
    /// The destination's answer was retried; the flag is whether its `Retry-After` set the
    /// delay.
    Retried(bool),
    /// The attempts ran out.
    Dead,
    /// The handler failed some other way; the run records it as a problem.
    Broken,
}

impl Driver<'_> {
    /// Tries to claim exactly the job `due` names. The outer `None` means the job is gone; the
    /// inner one that the claim query skipped it (a transaction holds its row).
    async fn try_claim(&self, due: &Due) -> Option<Option<ClaimedJob>> {
        let w = self.w;
        w.hold_real_time_jobs(self.now).await;
        let made = sqlx::query(
            "UPDATE jobs SET run_at = now() - interval '1 hour' \
             WHERE id = $1 AND state = 'ready'",
        )
        .bind(due.id)
        .execute(&w.env.pool)
        .await
        .unwrap()
        .rows_affected();
        if made == 0 {
            return None;
        }
        // `claim_job` runs with no per-workspace cap (`None`), as the worker pool does when the
        // setting is left out: a cap would only add a serialising advisory lock.
        let claimed = claim_job(
            &w.env.pool,
            "sim",
            &[due.kind],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap();
        if let Some(claimed) = &claimed {
            assert_eq!(claimed.id, due.id, "another job was claimable");
        }
        Some(claimed)
    }

    /// Claims exactly the job `due` names, or none when it is gone.
    async fn claim(&self, due: &Due) -> Option<ClaimedJob> {
        let claimed = self.try_claim(due).await?;
        Some(claimed.unwrap_or_else(|| panic!("the claim query skipped {due:?}")))
    }

    /// The enrollment of a claimed `flow.step` job, as it is before the job runs.
    async fn watch(&self, claimed: &ClaimedJob) -> Option<Watch> {
        if claimed.kind != STEP_JOB_KIND {
            return None;
        }
        let payload = &claimed.payload;
        let enrollment = enrollment_of(payload);
        let (version, wake_at): (i64, Option<OffsetDateTime>) =
            sqlx::query_as("SELECT state_version, wake_at FROM enrollments WHERE id = $1")
                .bind(enrollment)
                .fetch_optional(&self.w.env.pool)
                .await
                .unwrap()?;
        let input = payload.get("input").and_then(Value::as_str);
        let event = (input == Some("event"))
            .then(|| payload.get("event").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned);
        let mut outcome = None;
        if event
            .as_deref()
            .is_some_and(|name| name.starts_with("webhook."))
            && let Some(id) = payload
                .get("event_id")
                .and_then(Value::as_str)
                .and_then(|text| Uuid::parse_str(text).ok())
        {
            let properties: Option<Value> =
                sqlx::query_scalar("SELECT properties FROM events WHERE id = $1")
                    .bind(id)
                    .fetch_optional(&self.w.env.pool)
                    .await
                    .unwrap();
            outcome = properties.and_then(|properties| {
                let text = |key: &str| properties.get(key).and_then(Value::as_str);
                Some((
                    id,
                    Uuid::parse_str(text("enrollment_id")?).ok()?,
                    text("node_id")?.to_owned(),
                ))
            });
        }
        Some(Watch {
            enrollment,
            version,
            job_version: payload
                .get("state_version")
                .and_then(Value::as_i64)
                .unwrap_or(-1),
            wake_at,
            is_wake: input == Some("wake"),
            event,
            outcome,
            at: self.w.env.clock.now(),
        })
    }

    /// What a `webhook.call` committed: the outcome events it wrote, at most one per execution.
    async fn note_outcomes(&mut self, claimed: &ClaimedJob, before: Option<i64>) {
        let Some(before) = before else {
            return;
        };
        let after = self.w.outcome_events(&claimed.payload).await;
        match after - before {
            0 => {}
            1 => {
                *self
                    .outcomes
                    .entry(key_of(&claimed.payload).to_owned())
                    .or_default() += 1;
            }
            more => self.problem(format!(
                "one execution of the call {} wrote {more} outcome events",
                key_of(&claimed.payload)
            )),
        }
    }

    /// A job's attempts ran out. A `webhook.call` that does is accounted for; any other job is
    /// a stuck flow.
    fn job_died(&mut self, due: &Due, claimed: &ClaimedJob) {
        if due.kind == WEBHOOK_JOB_KIND {
            self.bump("webhook.call jobs dead");
            self.dead_webhooks
                .insert(key_of(&claimed.payload).to_owned());
        } else {
            self.problem(format!("{} job {} ran out of attempts", due.kind, due.id));
        }
    }

    /// Records that a real crash was injected into the execution: a rollback or a commit without
    /// the ack that happened, or an overlap with a second worker. A retry the destination's
    /// answer forces is not one. Counted once per execution.
    fn crash_injected(&mut self, injected: &mut bool) {
        if !*injected {
            *injected = true;
            self.bump("injected executions");
        }
    }

    /// Runs one job through its seeded plan. `early` marks a wake claimed before it was due.
    ///
    /// An execution starts with the job's first claim. Each planned crash is repeated until it
    /// really happens: an attempt the destination answers with a retryable status ends in the
    /// handler's error and crashes nothing, so the same mode is tried again with the attempt
    /// spent. Every execution therefore meets at least one real crash before its final run,
    /// including the ones whose attempts later run out.
    pub(super) async fn run_job(&mut self, due: &Due, early: bool) {
        let w = self.w;
        // The plan follows the job's label and how often that label was planned before, never the
        // clock: how far a retry delay or a lease moved it must not change what is injected.
        let times = self.planned.entry(due.label).or_default();
        *times += 1;
        let mut rng = Rng::derive(w.seed, &[LANE_PLAN, due.label, *times, u64::from(early)]);
        let mut plan = Plan::draw(&mut rng, due.kind);
        self.arrange(&mut plan, due, &mut rng);
        if early {
            self.bump("early wake claimed");
        }

        let mut kept: Option<(ClaimedJob, String)> = None;
        let mut injected = false;
        let mut started = false;
        let mut next = 0;
        let mut claims = 0;
        while next < plan.modes.len() {
            claims += 1;
            if claims > MAX_FINAL_ROUNDS {
                self.problem(format!(
                    "{} job {} met no real crash in {MAX_FINAL_ROUNDS} attempts",
                    due.kind, due.id
                ));
                return;
            }
            let mode = plan.modes[next];
            let Some(claimed) = self.claim(due).await else {
                self.bump("job gone before its turn");
                return;
            };
            if !started {
                started = true;
                self.bump("executions");
            }
            let before = self.watch(&claimed).await;
            let events = self.events_before(&claimed).await;
            match self.failed_attempt(&claimed, mode).await {
                Attempt::Ended(print) => {
                    self.bump(format!("{} {}", mode.name(), due.kind));
                    self.crash_injected(&mut injected);
                    next += 1;
                    if mode == Mode::CommitWithoutAck {
                        self.note_outcomes(&claimed, events).await;
                        if let Some(before) = &before {
                            self.after_step(before, false, before.at).await;
                        }
                        kept = Some((claimed, print));
                    }
                }
                Attempt::Retried(by_retry_after) => self.count_retry(by_retry_after),
                Attempt::Dead => {
                    self.bump(format!("{} {}", mode.name(), due.kind));
                    self.crash_injected(&mut injected);
                    if mode == Mode::CommitWithoutAck {
                        self.note_outcomes(&claimed, events).await;
                    }
                    self.job_died(due, &claimed);
                    return;
                }
                Attempt::Broken => next += 1,
            }
        }

        let mut concurrent = plan.concurrent;
        for _ in 0..MAX_FINAL_ROUNDS {
            let Some(claimed) = self.claim(due).await else {
                self.bump("job gone before its turn");
                return;
            };
            let before = self.watch(&claimed).await;
            let events = self.events_before(&claimed).await;
            let ended = if concurrent {
                concurrent = false;
                self.bump(format!("concurrent duplicate {}", due.kind));
                self.crash_injected(&mut injected);
                self.concurrent_duplicate(&claimed, due, plan.fenced).await
            } else {
                w.finish(&claimed).await
            };
            match ended {
                Final::Done => {
                    self.note_outcomes(&claimed, events).await;
                    self.bump(format!("completed {}", due.kind));
                    self.after_done(due, kept, before, early).await;
                    return;
                }
                Final::Retried(by_retry_after) => self.count_retry(by_retry_after),
                Final::Dead => {
                    self.job_died(due, &claimed);
                    return;
                }
                Final::Failed(error) => {
                    self.problem(format!("{} job {} failed: {error}", due.kind, due.id));
                    // Retire the job so the run goes on and the end state shows the harm.
                    sqlx::query("UPDATE jobs SET state = 'dead', dead_at = now() WHERE id = $1")
                        .bind(due.id)
                        .execute(&w.env.pool)
                        .await
                        .unwrap();
                    return;
                }
            }
        }
        self.problem(format!(
            "{} job {} was still not done after {MAX_FINAL_ROUNDS} attempts",
            due.kind, due.id
        ));
    }

    /// Settles what the seed must not leave to chance: the calls with a scenario take their
    /// part, and the concurrent duplicates are spread over both variants for every kind (every
    /// other one fenced), over every fourth webhook call.
    fn arrange(&mut self, plan: &mut Plan, due: &Due, rng: &mut Rng) {
        if due.scenario.dead {
            while plan.modes.len() < 5 {
                plan.modes.push(if rng.chance(50) {
                    Mode::Rollback
                } else {
                    Mode::CommitWithoutAck
                });
            }
        }
        if due.scenario.duplicate {
            plan.modes[0] = Mode::CommitWithoutAck;
        }
        if due.scenario.script == Some(Script::Down) {
            // Every send fails, so the last attempt records the failure; its crash must be a commit
            // without the ack for that record to survive (a rollback would take it back).
            plan.modes = vec![Mode::CommitWithoutAck];
            plan.concurrent = false;
        }
        if due.kind == WEBHOOK_JOB_KIND {
            self.webhook_runs += 1;
            plan.concurrent |= self.webhook_runs % 4 == 0;
        }
        if plan.concurrent {
            let seen = self.concurrents.entry(due.kind).or_default();
            plan.fenced = *seen % 2 == 0;
            *seen += 1;
        }
    }

    /// A retryable answer sent the job back to the queue after the worker's delay.
    fn count_retry(&mut self, by_retry_after: bool) {
        self.bump("webhook.call attempts retried");
        if by_retry_after {
            self.bump("retries delayed by the Retry-After of the destination");
        }
    }

    /// The outcome events of a call before an execution, for a `webhook.call`.
    async fn events_before(&self, claimed: &ClaimedJob) -> Option<i64> {
        if claimed.kind == WEBHOOK_JOB_KIND {
            Some(self.w.outcome_events(&claimed.payload).await)
        } else {
            None
        }
    }

    /// A job's final run committed: compare a re-run after a commit without the ack with the
    /// state that commit left, and look at what the step did to its enrollment.
    async fn after_done(
        &mut self,
        due: &Due,
        kept: Option<(ClaimedJob, String)>,
        before: Option<Watch>,
        early: bool,
    ) {
        if let Some((first, print)) = kept {
            let after = self.w.fingerprint(&first).await;
            if after != print {
                self.problem(format!(
                    "the re-run of {} {} after a commit without the ack changed state:\n  before {print}\n  after  {after}",
                    due.kind, due.id
                ));
            }
        }
        if let Some(before) = &before {
            let at = self.w.env.clock.now();
            self.after_step(before, early, at).await;
        }
    }

    /// One failed attempt: the handler ran and its transaction ended without the job being
    /// completed.
    async fn failed_attempt(&mut self, claimed: &ClaimedJob, mode: Mode) -> Attempt {
        let w = self.w;
        let mut tx = zirv_flow_store::begin(&w.env.pool).await.unwrap();
        assert!(
            lock_lease(&mut tx, claimed.id, claimed.lease_token)
                .await
                .unwrap(),
            "the lease of a fresh claim is held"
        );
        if let Err(error) = w.run_handler(&mut tx, claimed).await {
            drop(tx);
            let message = error.message().to_owned();
            if World::is_retry(&claimed.kind, &message) {
                return match w.retry(claimed, &message, error.retry_after()).await {
                    Final::Failed(why) => {
                        self.problem(format!("job {}: {why}", claimed.id));
                        Attempt::Broken
                    }
                    Final::Retried(by_retry_after) => Attempt::Retried(by_retry_after),
                    Final::Done | Final::Dead => Attempt::Retried(false),
                };
            }
            self.problem(format!(
                "{} job {} failed in a {} attempt: {message}",
                claimed.kind,
                claimed.id,
                mode.name()
            ));
            let _ = fail_job(
                &w.env.pool,
                claimed.id,
                claimed.lease_token,
                Duration::ZERO,
                &message,
            )
            .await;
            return Attempt::Broken;
        }
        let print = match mode {
            Mode::Rollback => {
                tx.rollback().await.unwrap();
                String::new()
            }
            Mode::CommitWithoutAck => {
                commit(tx).await.unwrap();
                w.fingerprint(claimed).await
            }
        };
        match w.expire_and_reap(claimed.id).await {
            Reaped::Requeued => Attempt::Ended(print),
            // The commit, if there was one, stays; the job is gone from the queue.
            Reaped::Dead => Attempt::Dead,
            Reaped::Untouched => {
                self.problem(format!("the reaper did not return job {}", claimed.id));
                Attempt::Ended(print)
            }
        }
    }

    /// Two workers hold the same job. Worker A runs the handler and keeps its transaction open
    /// while its lease runs out and the reaper returns the job. Then:
    ///
    /// - **fenced** (A took the lease lock first, as the worker does): the claim query skips the
    ///   job while A's transaction holds the key-share lock on its row, so B cannot claim it.
    ///   The driver checks that A's transaction is still open when B's claim runs, and that the
    ///   claim was skipped. A's ack finds the lease gone and A rolls back; only then does B
    ///   claim and run.
    /// - **unfenced** (A reached its handler without the lease lock, which no worker does, so
    ///   the data fences are all that stand between the two): B claims and runs the job while A
    ///   is open. A acts only once B is provably blocked on a lock A holds (the lock state is
    ///   read from `pg_stat_activity` and `pg_blocking_pids`), or once B has finished without
    ///   ever waiting on A (a `webhook.call`: A holds no lock on anything B needs). A's ack must
    ///   find the lease gone either way, and A rolls back, which lets a blocked B go on.
    ///
    /// A is always the one rolled back, which is what production allows: the ack and the commit
    /// are one transaction, so a worker whose ack finds its lease gone has nothing to commit.
    /// "The first execution may still finish" is the commit-without-ack mode of the plan.
    ///
    /// Nothing here is decided by a sleep: the only waits are polls of the lock state, bounded
    /// and loud.
    async fn concurrent_duplicate(
        &mut self,
        claimed_a: &ClaimedJob,
        due: &Due,
        fenced: bool,
    ) -> Final {
        match self.overlap(claimed_a, due, fenced).await {
            Ok(ended) => ended,
            Err(error) => Final::Failed(error),
        }
    }

    async fn overlap(
        &mut self,
        claimed_a: &ClaimedJob,
        due: &Due,
        fenced: bool,
    ) -> Result<Final, String> {
        let w = self.w;
        let id = claimed_a.id;
        let a_token = claimed_a.lease_token;
        let kind = due.kind;
        let mut tx_a = zirv_flow_store::begin(&w.env.pool).await.unwrap();
        let a_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx_a)
            .await
            .unwrap();
        if fenced {
            assert!(
                lock_lease(&mut tx_a, id, a_token).await.unwrap(),
                "the lease of a fresh claim is held"
            );
        }
        if let Err(error) = w.run_handler(&mut tx_a, claimed_a).await {
            drop(tx_a);
            let message = error.message().to_owned();
            if World::is_retry(kind, &message) {
                // A's attempt ended on the destination's answer before there was anything to
                // overlap: the job goes back with an attempt spent, and the next round runs it.
                return Ok(w.retry(claimed_a, &message, error.retry_after()).await);
            }
            return Err(format!("worker A's handler failed: {message}"));
        }
        match w.expire_and_reap(id).await {
            Reaped::Requeued => {}
            Reaped::Dead => return Ok(Final::Dead),
            Reaped::Untouched => {
                return Err("the reaper did not return the job under worker A".to_owned());
            }
        }
        if fenced {
            // B's claim runs while A's transaction is open: A is idle inside it.
            let state: Option<String> =
                sqlx::query_scalar("SELECT state FROM pg_stat_activity WHERE pid = $1")
                    .bind(a_pid)
                    .fetch_optional(&w.env.pool)
                    .await
                    .unwrap();
            if state.as_deref() != Some("idle in transaction") {
                return Err(format!(
                    "worker A's transaction was not open when B claimed: {state:?}"
                ));
            }
            self.bump(format!("fenced: A open while B claimed {kind}"));
            match self.try_claim(due).await {
                Some(None) => self.bump(format!("fenced: claim skipped {kind}")),
                Some(Some(_)) => {
                    return Err(
                        "a second worker claimed the job while the first held its lease lock"
                            .to_owned(),
                    );
                }
                None => return Err("the job vanished under worker A".to_owned()),
            }
            let acked = complete_job(&mut *tx_a, id, a_token).await.unwrap();
            tx_a.rollback().await.unwrap();
            if acked {
                return Err("worker A's ack succeeded after its lease was returned".to_owned());
            }
            self.bump(format!("fenced: A's ack refused {kind}"));
            let Some(claimed_b) = self.claim(due).await else {
                return Err("worker B could not claim the job".to_owned());
            };
            return Ok(w.finish(&claimed_b).await);
        }

        let Some(claimed_b) = self.claim(due).await else {
            return Err("worker B could not claim the job".to_owned());
        };
        let finished = Cell::new(false);
        let b = async {
            let result = w.finish(&claimed_b).await;
            finished.set(true);
            result
        };
        let a = async {
            let overlap = wait_for_overlap(&w.env.pool, a_pid, &finished).await;
            let acked = complete_job(&mut *tx_a, id, a_token).await.unwrap();
            tx_a.rollback().await.unwrap();
            (overlap, acked)
        };
        let (result_b, (overlap, acked)) =
            tokio::time::timeout(Duration::from_secs(120), async { tokio::join!(b, a) })
                .await
                .map_err(|_| "workers A and B did not finish in 120 s: a deadlock".to_owned())?;
        match overlap {
            Overlap::Blocked => self.bump(format!("unfenced: B blocked on A {kind}")),
            Overlap::FinishedFirst => {
                self.bump(format!("unfenced: B finished without waiting on A {kind}"));
            }
            Overlap::Stuck => {
                return Err(format!(
                    "worker B neither blocked on worker A nor finished in {} s",
                    OVERLAP_WAIT.as_secs()
                ));
            }
        }
        if acked {
            return Err("worker A's ack succeeded after its lease was taken over".to_owned());
        }
        self.bump(format!("unfenced: A's ack refused {kind}"));
        Ok(result_b)
    }

    /// After a `flow.step` job: record a wake that moved the enrollment, check that a job only
    /// ever moves the enrollment it expected, and let an enrollment that now waits for an event
    /// be sent one.
    async fn after_step(&mut self, watch: &Watch, early: bool, at: OffsetDateTime) {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT state_version FROM enrollments WHERE id = $1")
                .bind(watch.enrollment)
                .fetch_optional(&self.w.env.pool)
                .await
                .unwrap();
        let moved = row.is_some_and(|(version,)| version != watch.version);
        if moved {
            // A wait settles once: the job that moves an enrollment out of its version is the
            // one that expected that version, and nothing else moves it out of that version.
            if watch.job_version != watch.version {
                self.problem(format!(
                    "enrollment {} moved from version {} by a job that expected version {}",
                    watch.enrollment, watch.version, watch.job_version
                ));
            }
            let advances = self
                .advances
                .entry((watch.enrollment, watch.version))
                .or_default();
            *advances += 1;
            if *advances > 1 {
                let text = format!(
                    "enrollment {} was moved out of version {} {advances} times: a wait settled \
                     more than once",
                    watch.enrollment, watch.version
                );
                self.problem(text);
            }
        }
        if let Some(name) = &watch.event
            && name.starts_with("webhook.")
        {
            self.bump(if moved {
                "outcome events that settled a wait"
            } else {
                "outcome events that settled nothing"
            });
            // The same call reported again: a second outcome event for a call whose first one
            // already settled a wait must settle nothing, and a re-run of the job that settled
            // it (the same event) is not that.
            if let Some((event, enrollment, node)) = &watch.outcome {
                let settled = self
                    .outcome_settlers
                    .get(&(*enrollment, node.clone()))
                    .copied();
                match (moved, settled) {
                    (true, Some(first)) if first != *event => self.problem(format!(
                        "a duplicate outcome event {event} of the call of {enrollment}/{node} \
                         advanced an enrollment its first event {first} had already advanced"
                    )),
                    (true, _) => {
                        self.outcome_settlers
                            .insert((*enrollment, node.clone()), *event);
                    }
                    (false, Some(first)) if first != *event => {
                        self.bump("duplicate outcome events that settled nothing");
                    }
                    (false, _) => {}
                }
            }
        }
        if watch.is_wake {
            if moved {
                self.bump("wake moved an enrollment");
                // A wake with no timer to answer must not move anything either.
                self.wake_moves.push(WakeMove {
                    enrollment: watch.enrollment,
                    wake_at: watch.wake_at,
                    at,
                });
            } else if early {
                self.bump("early wake put back");
            }
        } else if moved {
            self.bump("event or start moved an enrollment");
        }
        if moved {
            self.plan_events(watch.enrollment).await;
        }
    }
}

/// How long worker A waits for worker B to block on it or finish before the run reports a stall.
const OVERLAP_WAIT: Duration = Duration::from_secs(30);
/// How often the lock state is read while waiting. It only paces the polls.
const OVERLAP_POLL: Duration = Duration::from_millis(5);

/// How worker B stood towards worker A's open transaction when A acted.
enum Overlap {
    /// A backend was waiting on a lock that A holds.
    Blocked,
    /// B ran to the end without waiting on A.
    FinishedFirst,
    /// Neither happened within the bound.
    Stuck,
}

/// Waits until some backend is blocked on a lock held by backend `holder`, or `finished` is set.
async fn wait_for_overlap(pool: &PgPool, holder: i32, finished: &Cell<bool>) -> Overlap {
    let started = Instant::now();
    loop {
        if finished.get() {
            return Overlap::FinishedFirst;
        }
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND $1 = ANY(pg_blocking_pids(pid))",
        )
        .bind(holder)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked > 0 {
            return Overlap::Blocked;
        }
        if started.elapsed() > OVERLAP_WAIT {
            return Overlap::Stuck;
        }
        tokio::time::sleep(OVERLAP_POLL).await;
    }
}
