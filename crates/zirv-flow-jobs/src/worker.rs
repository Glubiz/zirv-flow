//! The worker pool: claims jobs, runs them with a heartbeat, and completes each in the same
//! transaction as the handler's own writes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use zirv_flow_core::schedule::backoff;
use zirv_flow_store::{
    ClaimedJob, FailOutcome, PgPool, begin, claim_job, complete_job, fail_job, heartbeat_job,
    listen_for_jobs, lock_lease,
};

use crate::context::JobContext;
use crate::metrics::{self, Outcome};
use crate::registry::Registry;

/// How a worker pool behaves.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Jobs run at once.
    pub concurrency: usize,
    /// How long a claim lasts without a heartbeat.
    pub lease: Duration,
    /// How long an idle worker waits before looking again, when no notification arrives.
    pub poll_interval: Duration,
    /// A workspace with this many running jobs is skipped by claims; none means unlimited.
    pub max_running_per_workspace: Option<u32>,
    /// The retry delay for the first failure; it doubles per attempt.
    pub backoff_base: Duration,
    /// The longest retry delay.
    pub backoff_cap: Duration,
}

impl WorkerConfig {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency: concurrency.max(1),
            lease: Duration::from_secs(30),
            poll_interval: Duration::from_secs(1),
            max_running_per_workspace: None,
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(3600),
        }
    }
}

/// A pool of workers over one database.
pub struct Worker {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
}

struct Shared {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
    kinds: Vec<&'static str>,
    /// Woken when a job is enqueued; polling covers a missed wake-up.
    wake: Notify,
}

impl Worker {
    pub fn new(pool: PgPool, registry: Registry, config: WorkerConfig) -> Self {
        Self {
            pool,
            registry,
            config,
        }
    }

    /// Runs until `stop` becomes true, then stops claiming and lets running handlers
    /// finish. The caller bounds the wait; a handler still running when this future is
    /// dropped is cancelled and left to the reaper.
    pub async fn run(self, stop: watch::Receiver<bool>) {
        let shared = Arc::new(Shared {
            kinds: self.registry.kinds(),
            pool: self.pool,
            registry: self.registry,
            config: self.config,
            wake: Notify::new(),
        });

        let listener = AbortOnDrop(tokio::spawn(listen(Arc::clone(&shared), stop.clone())));
        let mut slots = JoinSet::new();
        for slot in 0..shared.config.concurrency {
            slots.spawn(work(Arc::clone(&shared), slot, stop.clone()));
        }
        while slots.join_next().await.is_some() {}
        drop(listener);
    }
}

/// Aborts the task when dropped, so nothing outlives the future that started it.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Wakes the workers when a job is enqueued.
async fn listen(shared: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut listener = None;
    loop {
        if *stop.borrow() {
            return;
        }
        let Some(connected) = listener.as_mut() else {
            match listen_for_jobs(&shared.pool).await {
                Ok(connected) => listener = Some(connected),
                Err(error) => {
                    warn!("could not listen for new jobs, polling instead: {error}");
                    pause(Duration::from_secs(1), &mut stop).await;
                }
            }
            continue;
        };
        tokio::select! {
            result = connected.wait() => match result {
                Ok(true) => shared.wake.notify_waiters(),
                // The connection dropped and came back, so notifications may have been
                // missed: wake everyone to poll.
                Ok(false) => shared.wake.notify_waiters(),
                Err(error) => {
                    warn!("the job listener failed: {error}");
                    listener = None;
                    shared.wake.notify_waiters();
                    pause(Duration::from_secs(1), &mut stop).await;
                }
            },
            _ = stop.changed() => {}
        }
    }
}

/// One worker: claim a job, run it, repeat; wait for a wake-up when the queue is empty.
async fn work(shared: Arc<Shared>, slot: usize, mut stop: watch::Receiver<bool>) {
    let name = format!("{}:{slot}", std::process::id());
    while !*stop.borrow() {
        // Arm the wake-up before looking, so one sent in between is not missed.
        let woken = shared.wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();

        match claim_job(
            &shared.pool,
            &name,
            &shared.kinds,
            shared.config.lease,
            shared.config.max_running_per_workspace,
        )
        .await
        {
            Ok(Some(job)) => {
                process(&shared, job).await;
                continue;
            }
            Ok(None) => {}
            Err(error) => warn!("could not claim a job: {error}"),
        }

        tokio::select! {
            () = &mut woken => {}
            () = tokio::time::sleep(shared.config.poll_interval) => {}
            _ = stop.changed() => {}
        }
    }
}

/// Sleeps, unless the pool is told to stop first.
async fn pause(duration: Duration, stop: &mut watch::Receiver<bool>) {
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        _ = stop.changed() => {}
    }
}

/// How one attempt ended, before the job row is settled.
enum Attempt {
    Completed,
    LeaseLost,
    /// The handler failed; the text is safe to store.
    Failed(String),
}

/// Runs one claimed job and records what happened to it.
async fn process(shared: &Arc<Shared>, job: ClaimedJob) {
    let started = Instant::now();
    let mut task = AbortOnDrop(tokio::spawn(execute(Arc::clone(shared), job.clone())));
    // A panicking handler fails the attempt instead of taking the worker down.
    let attempt = match (&mut task.0).await {
        Ok(attempt) => attempt,
        Err(_) => Attempt::Failed("the job handler panicked".into()),
    };

    let outcome = match attempt {
        Attempt::Completed => Outcome::Completed,
        Attempt::LeaseLost => Outcome::LeaseLost,
        Attempt::Failed(reason) => settle_failure(shared, &job, &reason).await,
    };
    if outcome == Outcome::LeaseLost {
        warn!("job {} lost its lease; its work was discarded", job.id);
    }
    metrics::record_attempt(&job.kind, outcome, started.elapsed().as_secs_f64());
}

/// Retries or buries a job whose handler failed, fenced on its lease.
async fn settle_failure(shared: &Shared, job: &ClaimedJob, reason: &str) -> Outcome {
    let delay = backoff(
        u32::try_from(job.attempts).unwrap_or(u32::MAX),
        shared.config.backoff_base,
        shared.config.backoff_cap,
        random_unit(),
    );
    match fail_job(&shared.pool, job.id, job.lease_token, delay, reason).await {
        Ok(Some(FailOutcome::Retried)) => Outcome::Retried,
        Ok(Some(FailOutcome::Dead)) => Outcome::Dead,
        Ok(None) => Outcome::LeaseLost,
        Err(error) => {
            // The lease runs out and the reaper takes the job from here.
            warn!("could not record the failure of job {}: {error}", job.id);
            Outcome::LeaseLost
        }
    }
}

/// The handler's transaction: confirm the lease, run the handler beside a heartbeat, then
/// delete the job and commit with everything the handler wrote.
async fn execute(shared: Arc<Shared>, job: ClaimedJob) -> Attempt {
    let Some(kind) = shared.registry.get(&job.kind) else {
        return Attempt::Failed("no handler is registered for this job kind".into());
    };
    let Ok(mut tx) = begin(&shared.pool).await else {
        return Attempt::Failed("could not start the job transaction".into());
    };
    match lock_lease(&mut tx, job.id, job.lease_token).await {
        Ok(true) => {}
        Ok(false) => return Attempt::LeaseLost,
        Err(_) => return Attempt::Failed("could not confirm the lease".into()),
    }

    let (lost, mut lease_lost) = oneshot::channel();
    let _heartbeat = AbortOnDrop(tokio::spawn(heartbeat(
        Arc::clone(&shared),
        job.clone(),
        lost,
    )));
    let result = {
        let mut ctx = JobContext::new(&mut tx, &job);
        let handler = kind.run(&mut ctx, job.payload.clone());
        tokio::select! {
            result = handler => Some(result),
            // Dropping the handler cancels it; its transaction rolls back below.
            Ok(()) = &mut lease_lost => None,
        }
    };

    match result {
        None => Attempt::LeaseLost,
        Some(Err(error)) => Attempt::Failed(error.to_string()),
        Some(Ok(())) => match complete_job(&mut *tx, job.id, job.lease_token).await {
            Ok(true) => match tx.commit().await {
                Ok(()) => Attempt::Completed,
                Err(_) => Attempt::Failed("could not commit the job transaction".into()),
            },
            // Another worker owns the job now: roll back this attempt's writes.
            Ok(false) => Attempt::LeaseLost,
            Err(_) => Attempt::Failed("could not complete the job".into()),
        },
    }
}

/// Extends the lease every third of its length, and says so when the lease is gone.
async fn heartbeat(shared: Arc<Shared>, job: ClaimedJob, lost: oneshot::Sender<()>) {
    let period = shared.config.lease / 3;
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        ticker.tick().await;
        match heartbeat_job(&shared.pool, job.id, job.lease_token, shared.config.lease).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = lost.send(());
                return;
            }
            // A blip is not a lost lease; the next beat tries again.
            Err(error) => warn!("heartbeat for job {} failed: {error}", job.id),
        }
    }
}

/// A random number in `[0, 1)`.
fn random_unit() -> f64 {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1_u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use zirv_flow_store::{NewJob, QueueCounts, enqueue, find_job, queue_counts};

    use super::*;
    use crate::{JobError, JobKind};

    /// Records the job's index in `effects` (primary key: a repeat is an error), failing the
    /// first attempt of every `fail_every`-th job.
    struct Effect;

    impl JobKind for Effect {
        const KIND: &'static str = "test.effect";

        async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
            let index = payload["index"].as_i64().unwrap();
            let fail_every = payload["fail_every"].as_i64().unwrap_or(0);
            if fail_every > 0 && index % fail_every == 0 && ctx.attempts() == 1 {
                // The write below must be rolled back with the failed attempt.
                sqlx::query("INSERT INTO effects (index) VALUES ($1)")
                    .bind(index)
                    .execute(ctx.conn())
                    .await?;
                return Err(JobError::new("planned failure"));
            }
            sqlx::query("INSERT INTO effects (index) VALUES ($1)")
                .bind(index)
                .execute(ctx.conn())
                .await?;
            Ok(())
        }
    }

    struct AlwaysFails;

    impl JobKind for AlwaysFails {
        const KIND: &'static str = "test.fails";

        async fn run(&self, _ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
            // The payload is never echoed into the error.
            let _secret = payload["secret"].as_str();
            Err(JobError::new("the upstream refused the request"))
        }
    }

    struct Panics;

    impl JobKind for Panics {
        const KIND: &'static str = "test.panics";

        async fn run(&self, _ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            panic!("handler bug");
        }
    }

    struct Hangs;

    impl JobKind for Hangs {
        const KIND: &'static str = "test.hangs";

        async fn run(&self, _ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            std::future::pending().await
        }
    }

    fn registry() -> Registry {
        Registry::built_in()
            .with(&Effect)
            .with(&AlwaysFails)
            .with(&Panics)
            .with(&Hangs)
    }

    fn test_config(concurrency: usize) -> WorkerConfig {
        WorkerConfig {
            concurrency,
            lease: Duration::from_secs(30),
            poll_interval: Duration::from_millis(20),
            max_running_per_workspace: None,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(5),
        }
    }

    async fn migrated(pool: &PgPool) {
        zirv_flow_store::migrate(pool).await.unwrap();
        sqlx::query("CREATE TABLE effects (index bigint PRIMARY KEY)")
            .execute(pool)
            .await
            .unwrap();
    }

    fn start(pool: &PgPool, config: WorkerConfig) -> (watch::Sender<bool>, JoinHandle<()>) {
        let (stop, stopped) = watch::channel(false);
        let worker = Worker::new(pool.clone(), registry(), config);
        (stop, tokio::spawn(worker.run(stopped)))
    }

    /// Polls until the queue holds no jobs at all, or `done` is true of its counts.
    async fn wait_for(pool: &PgPool, done: impl Fn(QueueCounts) -> bool) {
        loop {
            if done(queue_counts(pool).await.unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn stop_and_join(stop: watch::Sender<bool>, handle: JoinHandle<()>) {
        stop.send(true).unwrap();
        handle.await.unwrap();
    }

    async fn effect_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM effects")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn enqueue_effects(pool: &PgPool, total: i64, fail_every: i64) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, max_attempts)              SELECT gen_random_uuid(), 'test.effect',                     jsonb_build_object('index', index, 'fail_every', $1::bigint), 3              FROM generate_series(0, $2::bigint - 1) AS index",
        )
        .bind(fail_every)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn the_random_unit_stays_in_range() {
        for _ in 0..1000 {
            let unit = random_unit();
            assert!((0.0..1.0).contains(&unit), "{unit}");
        }
    }

    #[test]
    fn the_default_config_leases_for_thirty_seconds_with_unlimited_workspaces() {
        let config = WorkerConfig::new(0);

        assert_eq!(config.concurrency, 1);
        assert_eq!(config.lease, Duration::from_secs(30));
        assert_eq!(config.max_running_per_workspace, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn ten_thousand_jobs_on_four_workers_each_take_effect_exactly_once(pool: PgPool) {
        migrated(&pool).await;
        enqueue_effects(&pool, 10_000, 0).await;
        let (stop, handle) = start(&pool, test_config(4));

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        assert_eq!(effect_count(&pool).await, 10_000);
        let distinct: i64 = sqlx::query_scalar("SELECT count(DISTINCT index) FROM effects")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(distinct, 10_000);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn jobs_that_fail_once_are_retried_and_still_take_effect_exactly_once(pool: PgPool) {
        migrated(&pool).await;
        // One job in a hundred fails its first attempt, after writing its effect.
        enqueue_effects(&pool, 10_000, 100).await;
        let (stop, handle) = start(&pool, test_config(4));

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        assert_eq!(effect_count(&pool).await, 10_000);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_handler_ends_dead_with_a_safe_error_after_max_attempts(pool: PgPool) {
        migrated(&pool).await;
        let mut job = NewJob::new("test.fails");
        job.payload = json!({"secret": "hunter2"});
        job.max_attempts = 3;
        enqueue(&pool, &job).await.unwrap();
        let (stop, handle) = start(&pool, test_config(2));

        wait_for(&pool, |counts| counts.dead == 1).await;
        stop_and_join(stop, handle).await;

        let id = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        let stored = find_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.state, "dead");
        assert_eq!(stored.attempts, 3);
        assert_eq!(
            stored.last_error.as_deref(),
            Some("the upstream refused the request")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_panicking_handler_fails_the_attempt_and_the_worker_carries_on(pool: PgPool) {
        migrated(&pool).await;
        let mut panics = NewJob::new("test.panics");
        panics.max_attempts = 1;
        enqueue(&pool, &panics).await.unwrap();
        enqueue_effects(&pool, 5, 0).await;
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts.ready == 0 && counts.running == 0).await;
        stop_and_join(stop, handle).await;

        assert_eq!(effect_count(&pool).await, 5);
        let last_error: String = sqlx::query_scalar("SELECT last_error FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(last_error, "the job handler panicked");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_killed_job_runs_again_after_its_lease_expires_and_a_late_completion_changes_nothing(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let mut job = NewJob::new("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        // The first worker claims the job and is killed: no completion, no heartbeat.
        let doomed = claim_job(
            &pool,
            "doomed",
            &["test.effect"],
            Duration::from_millis(500),
            None,
        )
        .await
        .unwrap()
        .unwrap();

        // The reaper runs once the lease has passed, by the database's clock.
        loop {
            let reaped = zirv_flow_store::reap_expired(&pool).await.unwrap();
            if reaped.requeued == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (stop, handle) = start(&pool, test_config(2));
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        assert_eq!(effect_count(&pool).await, 1);
        // The killed worker wakes up and tries to finish with its old token.
        assert!(
            !complete_job(&pool, doomed.id, doomed.lease_token)
                .await
                .unwrap()
        );
        assert!(
            fail_job(&pool, doomed.id, doomed.lease_token, Duration::ZERO, "late")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(effect_count(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rerun_of_a_killed_job_is_its_second_attempt(pool: PgPool) {
        migrated(&pool).await;
        let mut job = NewJob::new("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        let doomed = claim_job(
            &pool,
            "doomed",
            &["test.effect"],
            Duration::from_millis(300),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        loop {
            if zirv_flow_store::reap_expired(&pool).await.unwrap().requeued == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let second = claim_job(
            &pool,
            "second",
            &["test.effect"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(second.id, doomed.id);
        assert_eq!(second.attempts, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_whose_lease_is_taken_away_is_cancelled_and_its_writes_discarded(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let mut job = NewJob::new("test.hangs");
        job.max_attempts = 2;
        enqueue(&pool, &job).await.unwrap();
        let mut config = test_config(1);
        config.lease = Duration::from_millis(300);
        let (stop, handle) = start(&pool, config);
        // Wait for the handler to be running, then take the lease away from it.
        wait_for(&pool, |counts| counts.running == 1).await;
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid()")
            .execute(&pool)
            .await
            .unwrap();

        // The next heartbeat finds the lease gone and cancels the handler, so the worker
        // frees its slot: it can be stopped without waiting for the hanging handler.
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("the cancelled handler freed its worker")
            .unwrap();

        // The job is still the other owner's: nothing was completed or failed.
        let counts = queue_counts(&pool).await.unwrap();
        assert_eq!(counts.running, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unregistered_kind_is_left_in_the_queue(pool: PgPool) {
        migrated(&pool).await;
        enqueue(&pool, &NewJob::new("sync.run")).await.unwrap();
        enqueue(&pool, &NewJob::new("maintenance.noop"))
            .await
            .unwrap();
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts.ready == 1).await;
        stop_and_join(stop, handle).await;

        let kind: String = sqlx::query_scalar("SELECT kind FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "sync.run");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_enqueued_while_workers_are_idle_is_picked_up(pool: PgPool) {
        migrated(&pool).await;
        let mut config = test_config(2);
        // Only the notification can be fast enough to matter; polling is far slower.
        config.poll_interval = Duration::from_secs(3600);
        let (stop, handle) = start(&pool, config);
        // Wait until the listener is subscribed, so the notification cannot be missed.
        loop {
            let listening: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND query ILIKE 'LISTEN%')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if listening {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        enqueue(&pool, &NewJob::new("maintenance.noop"))
            .await
            .unwrap();

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
    }
}
