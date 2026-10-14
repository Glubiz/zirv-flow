//! The worker pool: claims jobs, runs them with a heartbeat, and completes each in the same
//! transaction as the handler's own writes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{Instrument, Span, info, info_span, warn};
use zirv_flow_connectors::runtime::RateLimits;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::schedule::backoff;
use zirv_flow_store::{
    ClaimedJob, Clock, EventRetention, FailOutcome, HmacKey, Keyring, PgPool, begin, claim_job,
    complete_job, fail_job, heartbeat_job, listen_for_jobs, lock_lease, observe_pool, sized_like,
};

use crate::context::{IngestSettings, JobContext};
use crate::metrics::{self, Outcome};
use crate::registry::Registry;

/// How long a claim or a handler transaction waits for a connection of the worker's own.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the worker's pools are sampled for the pool gauges.
const POOL_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

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
    /// What the `events.retention` job keeps and for how long.
    pub event_retention: EventRetention,
    /// The key `ingest.drain` redacts rejected payloads with; without it that kind fails.
    pub redaction_key: Option<Arc<HmacKey>>,
    /// The key `privacy.erase` hashes identifiers with; without it that kind fails.
    pub identifier_key: Option<Arc<HmacKey>>,
    /// The master keys that open stored secrets; `sync.run` opens a source credential with them.
    pub keyring: Option<Arc<Keyring>>,
    /// Inbox rows `ingest.drain` applies per transaction.
    pub ingest_drain_batch_size: u32,
    /// Concurrent drain loops per workspace in `ingest.drain`.
    pub ingest_drain_shards: u32,
    /// Which non-public destinations outbound connections may reach.
    pub egress: Arc<EgressPolicy>,
    /// The rate-limit buckets of outbound source calls, shared by every run in this process.
    pub rate_limits: Arc<RateLimits>,
    /// The clock segment refreshes read: the database's in production, a manual one in tests.
    pub clock: Clock,
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
            event_retention: EventRetention::default(),
            redaction_key: None,
            identifier_key: None,
            keyring: None,
            ingest_drain_batch_size: 500,
            ingest_drain_shards: 8,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            clock: Clock::default(),
        }
    }
}

/// A pool of workers over one database. It runs on pools of its own, so neither the HTTP
/// server nor running handlers can starve a claim, a handler transaction or a heartbeat:
/// `concurrency * (1 + drain_shards) + 1` connections for claims, handler transactions, the
/// transactions a handler such as the ingest drain commits on its own beside its job
/// transaction (one per drain shard), and failures, and `concurrency` for heartbeats only.
pub struct Worker {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
}

struct Shared {
    /// For claims, handler transactions and failures.
    pool: PgPool,
    /// For heartbeats only.
    heartbeat_pool: PgPool,
    /// The pool the worker was given, for the listener.
    listener_pool: PgPool,
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
        let slots = u32::try_from(self.config.concurrency).unwrap_or(u32::MAX);
        let shared = Arc::new(Shared {
            kinds: self.registry.kinds(),
            pool: sized_like(
                &self.pool,
                pool_size(slots, self.config.ingest_drain_shards),
                0,
                ACQUIRE_TIMEOUT,
            ),
            // A warm pool keeps connecting off the heartbeat's path; a beat may wait one
            // period, and a failed one is retried on the next, before the lease ends.
            heartbeat_pool: sized_like(&self.pool, slots, slots, self.config.lease / 3),
            listener_pool: self.pool,
            registry: self.registry,
            config: self.config,
            wake: Notify::new(),
        });

        let listener = AbortOnDrop(tokio::spawn(listen(Arc::clone(&shared), stop.clone())));
        let sampler = AbortOnDrop(tokio::spawn(sample_pools(Arc::clone(&shared))));
        let mut slots = JoinSet::new();
        for slot in 0..shared.config.concurrency {
            slots.spawn(work(Arc::clone(&shared), slot, stop.clone()));
        }
        while slots.join_next().await.is_some() {}
        drop(listener);
        drop(sampler);
    }
}

/// The shared pool's size: each slot holds its job transaction plus up to `drain_shards`
/// drain-batch transactions, and one connection is kept for failures.
fn pool_size(slots: u32, drain_shards: u32) -> u32 {
    slots
        .saturating_mul(drain_shards.max(1).saturating_add(1))
        .saturating_add(1)
}

/// Publishes the worker pools' gauges, until the task is aborted.
async fn sample_pools(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(POOL_SAMPLE_INTERVAL);
    loop {
        ticker.tick().await;
        observe_pool(&shared.pool, "jobs");
        observe_pool(&shared.heartbeat_pool, "heartbeat");
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
            match listen_for_jobs(&shared.listener_pool).await {
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
                // `Ok(false)` is a dropped and restored connection: notifications may have
                // been missed, so everyone polls.
                Ok(_) => shared.wake.notify_waiters(),
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
                metrics::record_claim_lag(&job.kind, job.claim_lag_seconds);
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

/// Runs one claimed job inside its `job` span and records what happened to it.
async fn process(shared: &Arc<Shared>, job: ClaimedJob) {
    let span = info_span!(
        "job",
        job_id = %job.id,
        kind = %job.kind,
        workspace_id = tracing::field::Empty,
        attempt = job.attempts,
        trace_id = %job.trace.trace_id,
        cause_kind = %job.trace.cause.kind,
        cause_id = %job.trace.cause.id,
    );
    if let Some(workspace) = job.workspace_id {
        span.record("workspace_id", tracing::field::display(workspace));
    }
    run_attempt(shared, job).instrument(span).await;
}

async fn run_attempt(shared: &Arc<Shared>, job: ClaimedJob) {
    let started = Instant::now();
    let mut task = AbortOnDrop(tokio::spawn(
        execute(Arc::clone(shared), job.clone()).instrument(Span::current()),
    ));
    // A panicking handler fails the attempt instead of taking the worker down.
    let attempt = match (&mut task.0).await {
        Ok(attempt) => attempt,
        Err(_) => Attempt::Failed("the job handler panicked".into()),
    };

    let mut failure = None;
    let outcome = match attempt {
        Attempt::Completed => Outcome::Completed,
        Attempt::LeaseLost => Outcome::LeaseLost,
        Attempt::Failed(reason) => {
            let outcome = settle_failure(shared, &job, &reason).await;
            failure = Some(reason);
            outcome
        }
    };
    let elapsed = started.elapsed();
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    match (outcome, failure) {
        (Outcome::Completed, _) => info!(duration_ms, "job completed"),
        (Outcome::Retried | Outcome::Dead, Some(reason)) => warn!(
            duration_ms,
            reason = %reason,
            retry = outcome == Outcome::Retried,
            "job attempt failed"
        ),
        _ => warn!(duration_ms, "job lost its lease; its work was discarded"),
    }
    metrics::record_attempt(&job.kind, outcome, elapsed.as_secs_f64());
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
        let mut ctx = JobContext::new(&mut tx, &job)
            .with_event_retention(shared.config.event_retention)
            .with_identifier_key(shared.config.identifier_key.clone())
            .with_clock(shared.config.clock.clone())
            .with_outbound(
                shared.config.keyring.clone(),
                Arc::clone(&shared.config.egress),
                Arc::clone(&shared.config.rate_limits),
            )
            .with_ingest(IngestSettings {
                pool: shared.pool.clone(),
                redaction_key: shared.config.redaction_key.clone(),
                batch_size: shared.config.ingest_drain_batch_size,
                shards: shared.config.ingest_drain_shards,
            });
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
        match heartbeat_job(
            &shared.heartbeat_pool,
            job.id,
            job.lease_token,
            shared.config.lease,
        )
        .await
        {
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
pub(crate) fn random_unit() -> f64 {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides randomness");
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1_u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, TraceContext};
    use zirv_flow_store::{NewJob, QueueCounts, enqueue, find_job, queue_counts};

    use super::*;
    use crate::{JobError, JobKind};

    #[test]
    fn the_pool_holds_a_job_transaction_and_a_batch_per_shard_for_every_slot_and_one_spare() {
        assert_eq!(pool_size(4, 4), 21);
        assert_eq!(pool_size(4, 8), 37);
        assert_eq!(pool_size(1, 1), 3);
        assert_eq!(pool_size(8, 16), 137);
        assert_eq!(pool_size(2, 0), 5, "no shards still holds one batch");
        assert_eq!(pool_size(u32::MAX, 16), u32::MAX);
    }

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

    /// Records a decision, then fails its first attempt, which rolls the decision back.
    struct RecordsThenFails;

    impl JobKind for RecordsThenFails {
        const KIND: &'static str = "test.records";

        async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            ctx.record_decision("fixture.step", "done", None, Vec::new())
                .await?;
            if ctx.attempts() == 1 {
                return Err(JobError::new("planned failure"));
            }
            Ok(())
        }
    }

    /// Writes an effect, takes a transaction-scoped advisory lock to say so, and never ends.
    struct Hangs;

    impl JobKind for Hangs {
        const KIND: &'static str = "test.hangs";

        async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            sqlx::query("INSERT INTO effects (index) VALUES (99)")
                .execute(ctx.conn())
                .await?;
            sqlx::query("SELECT pg_advisory_xact_lock(4242)")
                .execute(ctx.conn())
                .await?;
            std::future::pending().await
        }
    }

    /// Waits for the `gate` table to hold a row, then records the job's index.
    struct Gated;

    impl JobKind for Gated {
        const KIND: &'static str = "test.gated";

        async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
            loop {
                let open: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gate)")
                    .fetch_one(ctx.conn())
                    .await?;
                if open {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            sqlx::query("INSERT INTO effects (index) VALUES ($1)")
                .bind(payload["index"].as_i64().unwrap())
                .execute(ctx.conn())
                .await?;
            Ok(())
        }
    }

    /// True once some connection of this database is listening.
    const LISTENING: &str = "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database() AND query ILIKE 'LISTEN%')";

    /// True while `Hangs` holds its advisory lock, and once it has let go.
    const HANGING: &str = "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND objid = 4242 AND granted)";
    const NOT_HANGING: &str =
        "SELECT NOT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND objid = 4242)";

    fn new_job(kind: &str) -> NewJob {
        NewJob::new(kind, TraceContext::root(Cause::api("test-request")))
    }

    fn registry() -> Registry {
        Registry::built_in()
            .with(&Effect)
            .with(&AlwaysFails)
            .with(&Panics)
            .with(&Hangs)
            .with(&Gated)
            .with(&RecordsThenFails)
    }

    fn test_config(concurrency: usize) -> WorkerConfig {
        WorkerConfig {
            concurrency,
            lease: Duration::from_secs(30),
            poll_interval: Duration::from_millis(20),
            max_running_per_workspace: None,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(5),
            event_retention: EventRetention::default(),
            redaction_key: None,
            identifier_key: None,
            keyring: None,
            ingest_drain_batch_size: 500,
            ingest_drain_shards: 4,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            clock: Clock::default(),
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

    /// The longest any test waits on a condition before it fails instead of hanging.
    const WAIT_LIMIT: Duration = Duration::from_secs(60);

    /// Polls `check` until it is true, and panics saying `what` if that takes too long.
    async fn eventually<F: Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
        let deadline = Instant::now() + WAIT_LIMIT;
        while !check().await {
            assert!(
                Instant::now() < deadline,
                "timed out after {}s waiting for {what}",
                WAIT_LIMIT.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Polls until `done` is true of the queue's counts.
    async fn wait_for(pool: &PgPool, done: impl Fn(QueueCounts) -> bool) {
        eventually("the queue to reach the expected counts", || async {
            done(queue_counts(pool).await.unwrap())
        })
        .await;
    }

    /// Polls until the single-boolean `query` is true.
    async fn wait_until(pool: &PgPool, what: &str, query: &'static str) {
        eventually(what, || async {
            sqlx::query_scalar(query).fetch_one(pool).await.unwrap()
        })
        .await;
    }

    async fn stop_and_join(stop: watch::Sender<bool>, handle: JoinHandle<()>) {
        stop.send(true).unwrap();
        handle.await.unwrap();
    }

    /// Claims the one `test.effect` job with a short lease, as a worker that is then killed,
    /// and waits for the reaper to requeue it once the lease has passed (the database's
    /// clock).
    async fn claim_and_reap(pool: &PgPool, lease: Duration) -> ClaimedJob {
        let doomed = claim_job(pool, "doomed", &["test.effect"], lease, None)
            .await
            .unwrap()
            .unwrap();
        eventually("the reaper to requeue the expired job", || async {
            zirv_flow_store::reap_expired(pool).await.unwrap().requeued == 1
        })
        .await;
        doomed
    }

    async fn effect_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM effects")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn enqueue_effects(pool: &PgPool, total: i64, fail_every: i64) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, max_attempts, trace_id, cause_kind, cause_id) \
                SELECT gen_random_uuid(), 'test.effect', \
                jsonb_build_object('index', index, 'fail_every', $1::bigint), 3, \
                gen_random_uuid(), 'manual', '' \
                FROM generate_series(0, $2::bigint - 1) AS index",
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
    async fn a_running_worker_publishes_the_jobs_and_heartbeat_pool_gauges(pool: PgPool) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let (stop, joined) = start(&pool, test_config(1));

        eventually("both pool gauges to be published", || async {
            let output = handle.render();
            output.contains("zirv_flow_db_pool_connections{pool=\"jobs\"")
                && output.contains("zirv_flow_db_pool_connections{pool=\"heartbeat\"")
        })
        .await;

        stop_and_join(stop, joined).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_with_the_redaction_key_drains_an_appended_inbox(pool: PgPool) {
        use zirv_flow_core::{
            Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
        };
        use zirv_flow_store::{IngestItem, IngestPayload, append_inbox};

        migrated(&pool).await;
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let items: Vec<IngestItem> = (0..25)
            .map(|n| IngestItem {
                identifiers: vec![("anonymous_id".into(), format!("a{n}"))],
                timestamp: None,
                message_id: Some(format!("m-{n}")),
                payload: IngestPayload::Track {
                    event: "seen".into(),
                    properties: serde_json::Map::new(),
                },
            })
            .collect();
        append_inbox(
            &pool,
            ws,
            uuid::Uuid::now_v7(),
            &TraceContext::root(Cause::api("req")),
            &items,
        )
        .await
        .unwrap();
        let mut config = test_config(2);
        config.redaction_key = Some(Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction),
        ));
        config.ingest_drain_batch_size = 10;
        let (stop, handle) = start(&pool, config);

        wait_until(
            &pool,
            "the inbox to drain",
            "SELECT NOT EXISTS (SELECT 1 FROM ingest_inbox) AND (SELECT count(*) FROM events) = 25",
        )
        .await;
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
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
        let mut job = new_job("test.fails");
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

    async fn workspace(pool: &PgPool) -> zirv_flow_core::WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn trace_of(
        pool: &PgPool,
        workspace: zirv_flow_core::WorkspaceId,
        trace: &TraceContext,
    ) -> Vec<(String, String, Option<String>)> {
        let (records, _) = zirv_flow_store::list_trace(pool, workspace, trace.trace_id, 100)
            .await
            .unwrap();
        records
            .into_iter()
            .map(|record| (record.action, record.outcome, record.reason))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_that_fails_once_leaves_the_failure_and_only_the_surviving_decision_under_its_trace(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let ws = workspace(&pool).await;
        let mut job = new_job("test.records");
        job.workspace_id = Some(ws);
        let trace = job.trace.clone();
        enqueue(&pool, &job).await.unwrap();
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        let seen = trace_of(&pool, ws, &trace).await;
        assert_eq!(
            seen,
            [
                (
                    "job.attempt".to_owned(),
                    "failed".to_owned(),
                    Some("planned failure".to_owned())
                ),
                ("fixture.step".to_owned(), "done".to_owned(), None),
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_that_runs_out_of_attempts_leaves_a_failed_then_an_exhausted_record(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let ws = workspace(&pool).await;
        let mut job = new_job("test.fails");
        job.workspace_id = Some(ws);
        job.max_attempts = 2;
        let trace = job.trace.clone();
        enqueue(&pool, &job).await.unwrap();
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts.dead == 1).await;
        stop_and_join(stop, handle).await;

        let outcomes: Vec<String> = trace_of(&pool, ws, &trace)
            .await
            .into_iter()
            .map(|(_, outcome, _)| outcome)
            .collect();
        assert_eq!(outcomes, ["failed", "exhausted"]);
    }

    /// Collects what a JSON `tracing` subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_runs_in_a_span_that_puts_its_trace_and_job_ids_on_every_log_line(pool: PgPool) {
        migrated(&pool).await;
        let captured = Captured::default();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_writer(captured.clone())
                .finish(),
        );
        let job = new_job("maintenance.noop");
        let trace_id = job.trace.trace_id.to_string();
        enqueue(&pool, &job).await.unwrap();
        let job_id: Uuid = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line: Value = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|line| line["fields"]["message"] == "job completed")
            .unwrap_or_else(|| panic!("no completion line in {output}"));
        assert_eq!(line["span"]["trace_id"], trace_id.as_str());
        assert_eq!(line["span"]["job_id"], job_id.to_string().as_str());
        assert_eq!(line["span"]["kind"], "maintenance.noop");
        assert_eq!(line["span"]["cause_kind"], "api");
        assert!(line["fields"]["duration_ms"].is_u64());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_panicking_handler_fails_the_attempt_and_the_worker_carries_on(pool: PgPool) {
        migrated(&pool).await;
        let mut panics = new_job("test.panics");
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
        let mut job = new_job("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        // The first worker claims the job and is killed: no completion, no heartbeat.
        let doomed = claim_and_reap(&pool, Duration::from_millis(500)).await;
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
        let mut job = new_job("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        let doomed = claim_and_reap(&pool, Duration::from_millis(300)).await;

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
        let mut job = new_job("test.hangs");
        job.max_attempts = 2;
        enqueue(&pool, &job).await.unwrap();
        let mut config = test_config(1);
        config.lease = Duration::from_secs(3);
        let (stop, handle) = start(&pool, config);
        // Wait for the handler to have written and be hanging, then take the lease away.
        wait_until(&pool, "the handler to be hanging", HANGING).await;
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
        // The attempt's transaction ended, and its write went with it.
        wait_until(&pool, "the attempt's transaction to end", NOT_HANGING).await;
        assert_eq!(effect_count(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_late_worker_cannot_complete_or_fail_a_job_another_worker_holds(pool: PgPool) {
        migrated(&pool).await;
        let mut job = new_job("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        // Worker A's lease expires and the reaper requeues the job; worker B claims it.
        let stale = claim_and_reap(&pool, Duration::from_millis(300)).await;
        let current = claim_job(
            &pool,
            "current",
            &["test.effect"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(current.id, stale.id);
        assert_ne!(current.lease_token, stale.lease_token);
        let before = find_job(&pool, current.id).await.unwrap().unwrap();

        // A wakes up and tries to settle the job with its old token.
        assert!(
            !complete_job(&pool, stale.id, stale.lease_token)
                .await
                .unwrap()
        );
        assert!(
            fail_job(&pool, stale.id, stale.lease_token, Duration::ZERO, "late")
                .await
                .unwrap()
                .is_none()
        );

        // B still owns the live row, untouched.
        let after = find_job(&pool, current.id).await.unwrap().unwrap();
        assert_eq!(after.state, "running");
        assert_eq!(after.lease_token, Some(current.lease_token));
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.last_error, before.last_error);
        assert_eq!(after.lease_expires_at, before.lease_expires_at);
        assert!(
            complete_job(&pool, current.id, current.lease_token)
                .await
                .unwrap()
        );
        assert_eq!(queue_counts(&pool).await.unwrap(), QueueCounts::default());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_job_keeps_its_lease_while_the_pool_the_worker_was_given_is_saturated(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        sqlx::query("CREATE TABLE gate (open boolean)")
            .execute(&pool)
            .await
            .unwrap();
        let mut job = new_job("test.gated");
        job.payload = json!({"index": 7});
        enqueue(&pool, &job).await.unwrap();
        // The given pool has one connection and the worker's listener holds it, so
        // nothing else can use it: heartbeats and the handler's transaction must not.
        let given = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with((*pool.connect_options()).clone());
        let mut config = test_config(1);
        // Heartbeats every 2 s against a 6 s lease: only a stall of about 4 s fails this.
        config.lease = Duration::from_secs(6);
        let (stop, stopped) = watch::channel(false);
        let handle = tokio::spawn(Worker::new(given, registry(), config).run(stopped));
        wait_until(&pool, "the listener to subscribe", LISTENING).await;
        wait_for(&pool, |counts| counts.running == 1).await;
        sqlx::query("CREATE TABLE first_lease AS SELECT lease_expires_at AS at FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        // Once the database's clock passes the lease as first granted, only heartbeats can
        // be keeping the job from the reaper.
        wait_until(
            &pool,
            "the first lease to pass",
            "SELECT now() > at FROM first_lease",
        )
        .await;
        let reaped = zirv_flow_store::reap_expired(&pool).await.unwrap();
        assert_eq!((reaped.requeued, reaped.dead), (0, 0));

        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
        assert_eq!(effect_count(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unregistered_kind_is_left_in_the_queue(pool: PgPool) {
        migrated(&pool).await;
        enqueue(&pool, &new_job("sync.unknown")).await.unwrap();
        enqueue(&pool, &new_job("maintenance.noop")).await.unwrap();
        let (stop, handle) = start(&pool, test_config(1));

        wait_for(&pool, |counts| counts.ready == 1).await;
        stop_and_join(stop, handle).await;

        let kind: String = sqlx::query_scalar("SELECT kind FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kind, "sync.unknown");
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
        wait_until(&pool, "the listener to subscribe", LISTENING).await;

        enqueue(&pool, &new_job("maintenance.noop")).await.unwrap();

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
    }
}
