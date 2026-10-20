//! Failure injection. Every job goes through a seeded plan before its final successful run:
//!
//! - **rollback**: the handler runs in its transaction, which is then rolled back; the lease
//!   expires and the reaper returns the job;
//! - **commit without the ack**: the handler's work commits but the job is not completed, so the
//!   lease expires, the reaper returns the job and it runs again, which must change nothing;
//! - **concurrent duplicate**: worker A runs the handler and holds its transaction open while
//!   its lease expires and worker B claims the job and runs it to commit; A's completion must
//!   then find the lease gone, and A rolls back.
//!
//! The runs follow the worker's `execute`: begin, `lock_lease`, the handler, `complete_job`,
//! commit. The final run of a plan also stands for the success of a job nothing was injected into.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_engine::sim::rng::Rng;
use zirv_flow_store::{
    ClaimedJob, Clock, STEP_JOB_KIND, Tx, WEBHOOK_JOB_KIND, claim_job, commit, complete_job,
    fail_job, lock_lease, reap_expired,
};

use super::driver::{Driver, Due, LANE_PLAN, WakeMove, seconds_of};
use super::world::{EPOCH, World};
use crate::{JobContext, JobError};

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
    wake_at: Option<OffsetDateTime>,
    is_wake: bool,
}

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

    /// The final run: the handler, the ack and the commit in one transaction. The error is the
    /// handler's, after the transaction was rolled back.
    pub(super) async fn final_run(&self, claimed: &ClaimedJob) -> Result<(), String> {
        let mut tx = zirv_flow_store::begin(&self.env.pool).await.unwrap();
        if !lock_lease(&mut tx, claimed.id, claimed.lease_token)
            .await
            .unwrap()
        {
            return Err("the lease was lost before the handler ran".to_owned());
        }
        if let Err(error) = self.run_handler(&mut tx, claimed).await {
            drop(tx);
            return Err(error.message().to_owned());
        }
        if !complete_job(&mut *tx, claimed.id, claimed.lease_token)
            .await
            .unwrap()
        {
            return Err("the ack found the lease gone in the final run".to_owned());
        }
        commit(tx).await.unwrap();
        Ok(())
    }

    /// Lets the lease of a running job run out and has the reaper return it.
    async fn expire_and_reap(&self, id: Uuid) -> bool {
        sqlx::query(
            "UPDATE jobs SET lease_expires_at = now() - interval '1 second' \
             WHERE id = $1 AND state = 'running'",
        )
        .bind(id)
        .execute(&self.env.pool)
        .await
        .unwrap();
        reap_expired(&self.env.pool).await.unwrap().requeued >= 1
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
                let key = claimed
                    .payload
                    .get("idempotency_key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
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
}

fn enrollment_of(payload: &Value) -> Uuid {
    payload
        .get("enrollment_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .unwrap_or_default()
}

impl Driver<'_> {
    /// Tries to claim exactly the job `due` names. The outer `None` means the job is gone; the
    /// inner one that the claim query skipped it (a transaction holds its row).
    async fn try_claim(&self, due: &Due) -> Option<Option<ClaimedJob>> {
        let w = self.w;
        w.hold_real_time_jobs(self.now).await;
        let made = sqlx::query(
            "UPDATE jobs SET run_at = now() - interval '1 hour', max_attempts = 50 \
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
        Some(Watch {
            enrollment,
            version,
            wake_at,
            is_wake: payload.get("input").and_then(Value::as_str) == Some("wake"),
        })
    }

    /// Runs one job through its seeded plan. `early` marks a wake claimed before it was due.
    pub(super) async fn run_job(&mut self, due: &Due, early: bool) {
        let w = self.w;
        let mut rng = Rng::derive(
            w.seed,
            &[LANE_PLAN, due.label, seconds_of(self.now), u64::from(early)],
        );
        let failures = match rng.below(100) {
            0..=39 => 0,
            40..=69 => 1,
            70..=89 => 2,
            _ => 3,
        };
        let modes: Vec<Mode> = (0..failures)
            .map(|_| {
                if rng.chance(50) {
                    Mode::Rollback
                } else {
                    Mode::CommitWithoutAck
                }
            })
            .collect();
        let concurrent = rng.chance(25);
        let fenced = rng.chance(50);
        if early {
            self.bump("early wake claimed");
        }

        let mut kept: Option<(ClaimedJob, String)> = None;
        for mode in modes {
            let Some(claimed) = self.claim(due).await else {
                self.bump("job gone before its turn");
                return;
            };
            self.bump(format!("{} {}", mode.name(), due.kind));
            let before = self.watch(&claimed).await;
            let Some(print) = self.failed_attempt(&claimed, mode).await else {
                continue;
            };
            if mode == Mode::CommitWithoutAck {
                if let Some(before) = &before {
                    self.after_step(before, false).await;
                }
                kept = Some((claimed, print));
            }
        }

        let Some(claimed) = self.claim(due).await else {
            self.bump("job gone before its turn");
            return;
        };
        let before = self.watch(&claimed).await;
        let outcome = if concurrent {
            self.bump(format!("concurrent duplicate {}", due.kind));
            self.concurrent_duplicate(claimed, due, fenced).await
        } else {
            w.final_run(&claimed).await
        };
        if let Err(error) = outcome {
            self.problem(format!("{} job {} failed: {error}", due.kind, due.id));
            // Retire the job so the run goes on and the end state shows the harm.
            sqlx::query("UPDATE jobs SET state = 'dead', dead_at = now() WHERE id = $1")
                .bind(due.id)
                .execute(&w.env.pool)
                .await
                .unwrap();
            return;
        }
        self.bump(format!("completed {}", due.kind));

        if let Some((first, print)) = kept {
            let after = w.fingerprint(&first).await;
            if after != print {
                self.problem(format!(
                    "the re-run of {} {} after a commit without the ack changed state:\n  before {print}\n  after  {after}",
                    due.kind, due.id
                ));
            }
        }
        if let Some(before) = &before {
            self.after_step(before, early).await;
        }
    }

    /// One failed attempt: the handler ran and its transaction ended without the job being
    /// completed. Returns the fingerprint of the committed state for a commit without the ack.
    async fn failed_attempt(&mut self, claimed: &ClaimedJob, mode: Mode) -> Option<String> {
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
            self.problem(format!(
                "{} job {} failed in a {} attempt: {}",
                claimed.kind,
                claimed.id,
                mode.name(),
                error.message()
            ));
            let _ = fail_job(
                &w.env.pool,
                claimed.id,
                claimed.lease_token,
                Duration::ZERO,
                error.message(),
            )
            .await;
            return None;
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
        if !w.expire_and_reap(claimed.id).await {
            self.problem(format!("the reaper did not return job {}", claimed.id));
        }
        Some(print)
    }

    /// Two workers hold the same job. Worker A runs the handler and keeps its transaction open
    /// while its lease runs out and the reaper returns the job. Then:
    ///
    /// - **fenced** (A took the lease lock first, as the worker does): the claim query skips the
    ///   job while A's transaction holds the key-share lock on its row, so B cannot claim it.
    ///   A's ack finds the lease gone and A rolls back; only then does B claim and run.
    /// - **unfenced** (A reached its handler without the lease lock, which no worker does, so
    ///   the data fences are all that stand between the two): B claims and runs the job to
    ///   commit while A is open. On a `flow.step` B waits for the row lock A holds on the
    ///   enrollment, and on a `webhook.call` B commits first, since A holds no lock on the job
    ///   row. A's ack must find the lease gone either way, and A rolls back.
    async fn concurrent_duplicate(
        &mut self,
        claimed_a: ClaimedJob,
        due: &Due,
        fenced: bool,
    ) -> Result<(), String> {
        let w = self.w;
        let id = claimed_a.id;
        let a_token = claimed_a.lease_token;
        let mut tx_a = zirv_flow_store::begin(&w.env.pool).await.unwrap();
        if fenced {
            assert!(
                lock_lease(&mut tx_a, id, a_token).await.unwrap(),
                "the lease of a fresh claim is held"
            );
        }
        if let Err(error) = w.run_handler(&mut tx_a, &claimed_a).await {
            drop(tx_a);
            return Err(format!("worker A's handler failed: {}", error.message()));
        }
        if !w.expire_and_reap(id).await {
            return Err("the reaper did not return the job under worker A".to_owned());
        }
        if fenced {
            match self.try_claim(due).await {
                Some(None) => self.bump(format!("claim skipped under a lease lock {}", due.kind)),
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
            self.bump("zombie ack refused");
            let Some(claimed_b) = self.claim(due).await else {
                return Err("worker B could not claim the job".to_owned());
            };
            return w.final_run(&claimed_b).await;
        }

        let Some(claimed_b) = self.claim(due).await else {
            return Err("worker B could not claim the job".to_owned());
        };
        // A webhook's B commits without waiting for A, so A is slow; a step's B waits for A.
        let pause = Duration::from_millis(if due.kind == WEBHOOK_JOB_KIND {
            120
        } else {
            15
        });
        let b = w.final_run(&claimed_b);
        let a = async move {
            tokio::time::sleep(pause).await;
            let acked = complete_job(&mut *tx_a, id, a_token).await.unwrap();
            tx_a.rollback().await.unwrap();
            acked
        };
        let (result_b, acked) =
            tokio::time::timeout(Duration::from_secs(120), async { tokio::join!(b, a) })
                .await
                .map_err(|_| "workers A and B did not finish in 120 s: a deadlock".to_owned())?;
        if acked {
            return Err("worker A's ack succeeded after its lease was taken over".to_owned());
        }
        self.bump("zombie ack refused");
        result_b
    }

    /// After a `flow.step` job: record a wake that moved the enrollment, and let an enrollment
    /// that now waits for an event be sent one.
    async fn after_step(&mut self, watch: &Watch, early: bool) {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT state_version FROM enrollments WHERE id = $1")
                .bind(watch.enrollment)
                .fetch_optional(&self.w.env.pool)
                .await
                .unwrap();
        let moved = row.is_some_and(|(version,)| version != watch.version);
        if watch.is_wake {
            match (moved, watch.wake_at) {
                (true, Some(wake_at)) => {
                    self.bump("wake moved an enrollment");
                    self.wake_moves.push(WakeMove {
                        enrollment: watch.enrollment,
                        wake_at,
                        at: self.now,
                    });
                }
                (false, _) if early => self.bump("early wake put back"),
                _ => {}
            }
        } else if moved {
            self.bump("event or start moved an enrollment");
        }
        if moved {
            self.plan_events(watch.enrollment).await;
        }
    }
}
