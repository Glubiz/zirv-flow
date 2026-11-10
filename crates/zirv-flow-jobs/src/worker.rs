//! The worker pool: claims jobs, runs them with a heartbeat, and completes each in the same
//! transaction as the handler's own writes. The `flow.step` jobs of one workspace that a claim
//! returns together share one transaction, a savepoint each (#397).

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hashbrown::HashMap;
use sqlx::Connection;
use tokio::sync::{Notify, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{Instrument, Span, info, info_span, warn};
use uuid::Uuid;
use zirv_flow_connectors::dns::DnsResolver;
use zirv_flow_connectors::runtime::RateLimits;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::schedule::backoff;
use zirv_flow_render::RenderPool;
use zirv_flow_store::{
    ClaimFloors, ClaimedJob, Clock, DRAIN_JOB_KIND, DecisionRetention, EventRetention, FailOutcome,
    Heard, HmacKey, Keyring, MessageRetention, PgConnection, PgPool, RetentionSettings,
    STEP_JOB_KIND, begin, cache_change_readers, claim_jobs_from, complete_job, complete_jobs,
    confirm_lease, enqueue_drain_if_pending, fail_job, heartbeat_job, listen_for_jobs, lock_lease,
    observe_pool, payload_shard, release_jobs, sized_like,
};

use crate::JobError;
use crate::context::{IngestSettings, JobContext};
use crate::delivery::rate::RateGates;
use crate::kinds::{WakeBatch, plan_wakes};
use crate::metrics::{self, IdleEnd, Outcome};
use crate::registry::{ErasedKind, Registry};

/// How long a claim or a handler transaction waits for a connection of the worker's own.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the worker's pools are sampled for the pool gauges.
const POOL_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// How many due jobs a slot claims with one statement and one commit, to run one after another:
/// each in its own transaction, but the `flow.step` jobs of a workspace in one (#397). A claim
/// costs about the same whatever it returns, since most of it is the walk over the head of the
/// ready index, so a larger batch spreads that walk, and a workspace's share of the batch shares
/// one transaction (ruling 48be9592). The hold limit bounds how long the batch keeps work from the
/// other slots.
pub(crate) const CLAIM_BATCH: u32 = 32;

/// The longest a slot holds a claimed job it has not started; see [`hold_limit`].
const MAX_HOLD: Duration = Duration::from_secs(4);

/// How a worker pool behaves.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Jobs run at once.
    pub concurrency: usize,
    /// Slots beside `concurrency` that claim only `ingest.drain` jobs, which the general slots
    /// then leave alone, so a drain never waits behind the other kinds in the queue. None keeps
    /// the drain with the general slots.
    pub drain_slots: usize,
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
    /// The most uncompressed bytes a person export may hold (`privacy.export_max_bytes`).
    pub export_max_bytes: u64,
    /// What the `messages.retention` job keeps and for how long.
    pub message_retention: MessageRetention,
    /// What the `decisions.retention` job keeps and for how long.
    pub decision_retention: DecisionRetention,
    /// The other retention windows (`retention.import_file_days`, `retention.export_hours` and
    /// `retention.test_send_months`).
    pub retention_settings: RetentionSettings,
    /// How many days the `jobs.retention` job keeps dead jobs (`retention.job_history_days`).
    pub job_history_days: u32,
    /// How many days the `audit.retention` job keeps audit rows (`retention.audit_days`).
    pub audit_days: u32,
    /// The key `ingest.drain` redacts rejected payloads with; without it that kind fails.
    pub redaction_key: Option<Arc<HmacKey>>,
    /// The key `privacy.erase` hashes identifiers with; without it that kind fails.
    pub identifier_key: Option<Arc<HmacKey>>,
    /// Where `privacy.erase` copies each completed ledger entry; none writes no file.
    pub ledger_dir: Option<Arc<PathBuf>>,
    /// The master keys that open stored secrets; `sync.run` opens a source credential with them.
    pub keyring: Option<Arc<Keyring>>,
    /// Inbox rows `ingest.drain` applies per transaction.
    pub ingest_drain_batch_size: u32,
    /// The shard count a pre-split `ingest.drain` job (one without a shard) enqueues the shard
    /// jobs of. A shard job drains the shard its payload names, whatever this is.
    pub ingest_drain_shards: u32,
    /// Messages a `kafka.consume` unit holds before it commits.
    pub kafka_unit_messages: usize,
    /// Records a `sync.run` unit holds before it commits.
    pub sync_unit_records: usize,
    /// Which non-public destinations outbound connections may reach.
    pub egress: Arc<EgressPolicy>,
    /// The rate-limit buckets of outbound source calls, shared by every run in this process.
    pub rate_limits: Arc<RateLimits>,
    /// The rate gates of the destinations this process sends to, shared by every send job. Give
    /// every worker of a process the same set, so a destination's block of tokens is spent once.
    pub rate_gates: Arc<RateGates>,
    /// The clock segment refreshes read: the database's in production, a manual one in tests.
    pub clock: Clock,
    /// The isolated renderer that send handlers render through; none leaves them without one.
    pub render_pool: Option<RenderPool>,
    /// The resolver `sending_domain.verify` reads DNS through; without one that kind fails until
    /// one is configured.
    pub dns: Option<Arc<dyn DnsResolver>>,
    /// Where browsers reach the server (`server.public_url`), without a trailing slash. Tracking
    /// links are built on it; without it a marketing message fails to render.
    pub public_url: Option<String>,
    /// The key that signs open, click and unsubscribe tokens; without it a marketing message fails
    /// to render.
    pub tracking_key: Option<Arc<HmacKey>>,
}

impl WorkerConfig {
    pub fn new(concurrency: usize) -> Self {
        Self {
            concurrency: concurrency.max(1),
            drain_slots: 1,
            lease: Duration::from_secs(30),
            poll_interval: Duration::from_secs(1),
            max_running_per_workspace: None,
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(3600),
            event_retention: EventRetention::default(),
            export_max_bytes: zirv_flow_store::EXPORT_MAX_BYTES_DEFAULT,
            message_retention: MessageRetention::default(),
            decision_retention: DecisionRetention::default(),
            retention_settings: RetentionSettings::default(),
            job_history_days: crate::context::DEFAULT_JOB_HISTORY_DAYS,
            audit_days: crate::context::DEFAULT_AUDIT_DAYS,
            redaction_key: None,
            identifier_key: None,
            ledger_dir: None,
            keyring: None,
            ingest_drain_batch_size: 500,
            ingest_drain_shards: 8,
            kafka_unit_messages: crate::context::DEFAULT_UNIT_SIZE,
            sync_unit_records: crate::context::DEFAULT_UNIT_SIZE,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            rate_gates: Arc::default(),
            clock: Clock::default(),
            render_pool: None,
            dns: None,
            public_url: None,
            tracking_key: None,
        }
    }
}

/// A pool of workers over one database. It runs on pools of its own, so neither the HTTP
/// server nor running handlers can starve a claim, a handler transaction or a heartbeat:
/// `slots * 2 + 1` connections for claims, handler transactions, the transaction a handler
/// such as the ingest drain commits on its own beside its job transaction (one per drain
/// shard job), and failures, and `slots` for heartbeats and for
/// giving back the claimed jobs a slot has not started; `slots` is `concurrency` plus the
/// drain slots.
pub struct Worker {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
}

struct Shared {
    /// For claims, handler transactions and failures.
    pool: PgPool,
    /// For heartbeats, and for giving back claimed jobs not started while a handler runs.
    heartbeat_pool: PgPool,
    /// The pool the worker was given, for the listener.
    listener_pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
    /// What the general slots claim: every kind, less `ingest.drain` while drain slots run.
    kinds: Vec<&'static str>,
    /// How many slots claim only `ingest.drain`; none when the registry has no such kind.
    drain_slots: usize,
    /// Woken when a job of a kind the general slots claim is enqueued: one idle slot per
    /// notification. Polling covers a missed wake-up.
    wake_general: Notify,
    /// The same for the drain slots.
    wake_drain: Notify,
    /// Where the general slots' claims start in each band, past the jobs claimed before.
    general_floors: ClaimFloors,
    /// The same for the drain slots.
    drain_floors: ClaimFloors,
    /// Claim statements run, for tests that count them.
    #[cfg(test)]
    claims: std::sync::atomic::AtomicU64,
    /// Woken once for each job of a workspace that leaves running under a per-workspace cap,
    /// so that one slot whose claim found the workspaces at their caps claims again (#397).
    /// Only this worker's own jobs wake it; polling covers the room another worker frees.
    room: Notify,
}

impl Shared {
    fn new(
        pool: PgPool,
        heartbeat_pool: PgPool,
        listener_pool: PgPool,
        registry: Registry,
        config: WorkerConfig,
    ) -> Self {
        let drain_slots = effective_drain_slots(&registry, &config);
        let mut kinds = registry.kinds();
        if drain_slots > 0 {
            kinds.retain(|kind| *kind != DRAIN_JOB_KIND);
        }
        Self {
            kinds,
            drain_slots,
            pool,
            heartbeat_pool,
            listener_pool,
            registry,
            config,
            wake_general: Notify::new(),
            wake_drain: Notify::new(),
            general_floors: ClaimFloors::default(),
            drain_floors: ClaimFloors::default(),
            #[cfg(test)]
            claims: std::sync::atomic::AtomicU64::new(0),
            room: Notify::new(),
        }
    }

    /// Whether claims leave the workspaces at their caps behind, so a job leaving running frees
    /// room that a slot may be waiting for.
    fn capped(&self) -> bool {
        self.config.max_running_per_workspace.is_some()
    }

    /// Wakes one idle slot of each lane that claims `kind`, for one notification. A notification
    /// can stand for many jobs, so a slot whose claim comes back full wakes the next (see
    /// [`work`]). A kind no lane of this worker claims wakes nobody.
    fn wake_for(&self, kind: &str) {
        for lane in [Lane::General, Lane::Drain] {
            if lane.kinds(self).contains(&kind) && self.slots_in(lane) > 0 {
                lane.wake(self).notify_one();
            }
        }
    }

    /// Wakes every idle slot: the listener's connection was lost, so anything may be waiting.
    fn wake_all(&self) {
        self.wake_general.notify_waiters();
        self.wake_drain.notify_waiters();
    }

    fn slots_in(&self, lane: Lane) -> usize {
        match lane {
            Lane::General => self.config.concurrency,
            Lane::Drain => self.drain_slots,
        }
    }

    /// Wakes one slot waiting for room when `job`, which held its workspace's room under a cap,
    /// no longer runs: it completed, failed, lost its lease or went back to the queue. With no
    /// slot waiting, the next slot to wait claims again at once. Nothing happens without a cap.
    fn left_running(&self, job: &ClaimedJob) {
        if job.workspace_id.is_some() && self.capped() {
            self.room.notify_one();
        }
    }
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
        let drain_slots = effective_drain_slots(&self.registry, &self.config);
        let slots =
            u32::try_from(self.config.concurrency.saturating_add(drain_slots)).unwrap_or(u32::MAX);
        let shared = Arc::new(Shared::new(
            sized_like(&self.pool, pool_size(slots), 0, ACQUIRE_TIMEOUT),
            // A warm pool keeps connecting off the heartbeat's path; a beat may wait one
            // period, and a failed one is retried on the next, before the lease ends.
            sized_like(&self.pool, slots, slots, self.config.lease / 3),
            self.pool,
            self.registry,
            self.config,
        ));

        let listener = AbortOnDrop(tokio::spawn(listen(Arc::clone(&shared), stop.clone())));
        let sampler = AbortOnDrop(tokio::spawn(sample_pools(Arc::clone(&shared))));
        let mut slots = JoinSet::new();
        for slot in 0..shared.config.concurrency {
            slots.spawn(work(Arc::clone(&shared), slot, Lane::General, stop.clone()));
        }
        for slot in 0..shared.drain_slots {
            let slot = shared.config.concurrency + slot;
            slots.spawn(work(Arc::clone(&shared), slot, Lane::Drain, stop.clone()));
        }
        while slots.join_next().await.is_some() {}
        drop(listener);
        drop(sampler);
    }
}

/// The drain slots a worker runs: those configured, when the registry has the kind they claim.
fn effective_drain_slots(registry: &Registry, config: &WorkerConfig) -> usize {
    if registry.get(DRAIN_JOB_KIND).is_some() {
        config.drain_slots
    } else {
        0
    }
}

/// What a slot claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// Every kind the worker runs, less `ingest.drain` when the worker has drain slots.
    General,
    /// Only `ingest.drain`: a drain slot never waits behind another kind.
    Drain,
}

impl Lane {
    /// The kinds a slot of this lane claims.
    fn kinds(self, shared: &Shared) -> &[&'static str] {
        match self {
            Self::General => &shared.kinds,
            Self::Drain => &DRAIN_KINDS,
        }
    }

    /// What wakes an idle slot of this lane.
    fn wake(self, shared: &Shared) -> &Notify {
        match self {
            Self::General => &shared.wake_general,
            Self::Drain => &shared.wake_drain,
        }
    }

    /// Where this lane's claims start.
    fn floors(self, shared: &Shared) -> &ClaimFloors {
        match self {
            Self::General => &shared.general_floors,
            Self::Drain => &shared.drain_floors,
        }
    }

    /// How many jobs a claim takes: a drain slot runs one drain at a time, so the rest stay
    /// ready for the other drain slots instead of being held behind a long run.
    fn batch(self) -> u32 {
        match self {
            Self::General => CLAIM_BATCH,
            Self::Drain => 1,
        }
    }
}

const DRAIN_KINDS: [&str; 1] = [DRAIN_JOB_KIND];

/// The shared pool's size: each slot holds its job transaction plus one drain-batch
/// transaction, and one connection is kept for failures.
fn pool_size(slots: u32) -> u32 {
    slots.saturating_mul(2).saturating_add(1)
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
            result = connected.hear() => match result {
                // One idle slot per notification; a full claim wakes the next.
                Ok(Heard::Kind(kind)) => shared.wake_for(&kind),
                // A dropped and restored connection: notifications may have been missed, so
                // everyone polls.
                Ok(Heard::Reconnected) => shared.wake_all(),
                Err(error) => {
                    warn!("the job listener failed: {error}");
                    listener = None;
                    shared.wake_all();
                    pause(Duration::from_secs(1), &mut stop).await;
                }
            },
            _ = stop.changed() => {}
        }
    }
}

/// One worker: claim a batch of jobs, run them, repeat; when a claim finds nothing, wait for a
/// job to be enqueued, for the poll interval, or, under a per-workspace cap, for a job of this
/// worker to leave running (#397): a claim finds nothing while the due jobs' workspaces are at
/// their caps, and each job that leaves running wakes one waiting slot, so the room it frees is
/// claimed without waiting for the poll.
async fn work(shared: Arc<Shared>, slot: usize, lane: Lane, mut stop: watch::Receiver<bool>) {
    let name = format!("{}:{slot}", std::process::id());
    let capped = shared.capped();
    while !*stop.borrow() {
        let batch = {
            // Arm the wake-ups before looking, so one sent in between is not missed. They end
            // with this block, before the batch runs: a wake-up for room that reaches a slot
            // which then claimed something goes on to the next waiting slot.
            let woken = lane.wake(&shared).notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            let room = shared.room.notified();
            tokio::pin!(room);
            if capped {
                room.as_mut().enable();
            }

            #[cfg(test)]
            shared
                .claims
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match claim_jobs_from(
                &shared.pool,
                &name,
                lane.kinds(&shared),
                shared.config.lease,
                shared.config.max_running_per_workspace,
                lane.batch(),
                lane.floors(&shared),
            )
            .await
            {
                Ok(batch) if !batch.is_empty() => batch,
                claimed => {
                    if let Err(error) = claimed {
                        warn!("could not claim jobs: {error}");
                    }
                    let ended = tokio::select! {
                        () = &mut woken => Some(IdleEnd::Enqueued),
                        () = &mut room, if capped => Some(IdleEnd::Freed),
                        () = tokio::time::sleep(shared.config.poll_interval) => {
                            Some(IdleEnd::Polled)
                        }
                        _ = stop.changed() => None,
                    };
                    if let Some(ended) = ended {
                        metrics::record_idle_wait(ended);
                    }
                    continue;
                }
            }
        };
        // A full claim may mean more is ready, since one notification stands for the jobs a
        // transaction enqueued of a kind: pass the wake on before running, until a claim comes
        // back short. The wake-ups above ended with the block, so it cannot reach this slot.
        if batch.len() >= lane.batch() as usize {
            lane.wake(&shared).notify_one();
        }
        run_batch(&shared, batch, &mut stop).await;
    }
}

/// How long a slot may hold the claimed jobs it has not started: a sixth of the lease, and at
/// most [`MAX_HOLD`].
///
/// A job claimed with others starts within this of its claim's return or is given back. Its
/// lease runs from the claim's update, however long the claim waited before it, so it starts
/// with five sixths of its lease left, less the commit and the reply between the update and the
/// return. Its heartbeat beats a third and two thirds of a lease after the start, both before the
/// lease taken at the claim ends, so a held job survives one failed beat as a job claimed alone
/// does. The cap keeps the jobs claimed behind a slow handler from waiting long for it.
fn hold_limit(lease: Duration) -> Duration {
    (lease / 6).min(MAX_HOLD)
}

/// What a slot runs in one transaction.
enum Unit {
    /// A job in a transaction of its own.
    Alone(ClaimedJob),
    /// The `flow.step` jobs of one workspace that one claim returned, in one transaction (#397).
    Wakes(WorkspaceId, Vec<ClaimedJob>),
}

impl Unit {
    fn into_jobs(self) -> Vec<ClaimedJob> {
        match self {
            Self::Alone(job) => vec![job],
            Self::Wakes(_, jobs) => jobs,
        }
    }
}

/// Splits a claimed batch into what runs in one transaction each, in claim order. The `flow.step`
/// jobs of a workspace join the first of them, so a slot runs a workspace's wakes in one
/// transaction (#397); every other job, and a workspace's only wake, runs alone as before.
fn units(batch: Vec<ClaimedJob>) -> VecDeque<Unit> {
    let mut units: Vec<Unit> = Vec::with_capacity(batch.len());
    let mut wakes: HashMap<Uuid, usize> = HashMap::new();
    for job in batch {
        let Some(workspace) = job.workspace_id.filter(|_| job.kind == STEP_JOB_KIND) else {
            units.push(Unit::Alone(job));
            continue;
        };
        match wakes
            .get(&workspace)
            .and_then(|index| units.get_mut(*index))
        {
            Some(Unit::Wakes(_, jobs)) => jobs.push(job),
            _ => {
                wakes.insert(workspace, units.len());
                units.push(Unit::Wakes(WorkspaceId::from_uuid(workspace), vec![job]));
            }
        }
    }
    units
        .into_iter()
        .flat_map(|unit| match unit {
            Unit::Wakes(workspace, jobs) if jobs.len() > 1 => vec![Unit::Wakes(workspace, jobs)],
            unit => unit.into_jobs().into_iter().map(Unit::Alone).collect(),
        })
        .collect()
}

/// Runs a claimed batch in claim order. Each job runs in its own transaction exactly as if it
/// had been claimed alone, so its lease check, heartbeat and completion are its own, except the
/// `flow.step` jobs of one workspace, which share one transaction ([`execute_wakes`]) and keep a
/// lease check, heartbeat and completion each.
///
/// The jobs not started yet go back to the queue as soon as the pool is told to stop or
/// [`hold_limit`] has passed since the claim returned, even while a handler runs. The hold does
/// not count the claim's own wait, since the leases run from the claim's update: counting it
/// gave back unstarted every batch whose claim had queued on the claim lock past the hold, so
/// under a cap nothing ran (bench phase 4a, #397). A wake of a shared transaction starts when it
/// begins, so the wakes not begun by then go back too. Should the slot die first, they run again
/// once their leases expire, as a running job does.
async fn run_batch(shared: &Arc<Shared>, batch: Vec<ClaimedJob>, stop: &mut watch::Receiver<bool>) {
    let claimed = tokio::time::Instant::now();
    let deadline = claimed + hold_limit(shared.config.lease);
    let mut held = units(batch);
    while let Some(unit) = held.pop_front() {
        if *stop.borrow() || tokio::time::Instant::now() >= deadline {
            held.push_front(unit);
            break;
        }
        match unit {
            Unit::Alone(job) => {
                // The lag runs to the start, so a wait behind the jobs claimed with this one
                // counts.
                metrics::record_claim_lag(
                    &job.kind,
                    job.claim_lag_seconds + claimed.elapsed().as_secs_f64(),
                );
                let attempt = process(shared, job);
                hold_during(shared, attempt, &mut held, None, deadline, stop).await;
            }
            Unit::Wakes(workspace, jobs) => {
                let queue = Arc::new(Queue::new(jobs));
                let wakes = Wakes {
                    workspace,
                    queue: Arc::clone(&queue),
                    claimed,
                    deadline,
                    stop: stop.clone(),
                };
                let attempt = process_wakes(shared, wakes);
                hold_during(shared, attempt, &mut held, Some(&queue), deadline, stop).await;
            }
        }
    }
    release(shared, held_jobs(&mut held, None)).await;
}

/// Waits for `attempt`, and gives back the jobs held behind it, the units in `held` and the wakes
/// of `queue` not begun, once the hold has passed or the pool is told to stop.
async fn hold_during<T>(
    shared: &Shared,
    attempt: impl Future<Output = T>,
    held: &mut VecDeque<Unit>,
    queue: Option<&Queue>,
    deadline: tokio::time::Instant,
    stop: &mut watch::Receiver<bool>,
) {
    tokio::pin!(attempt);
    loop {
        let holding = !held.is_empty() || queue.is_some_and(Queue::has_pending);
        tokio::select! {
            _ = &mut attempt => break,
            () = tokio::time::sleep_until(deadline), if holding => {
                release(shared, held_jobs(held, queue)).await;
            }
            _ = stop.changed(), if holding => release(shared, held_jobs(held, queue)).await,
        }
    }
}

/// Every job in `held`, and the wakes of `queue` not begun, taken out to be given back.
fn held_jobs(held: &mut VecDeque<Unit>, queue: Option<&Queue>) -> Vec<ClaimedJob> {
    let mut jobs: Vec<ClaimedJob> = held.drain(..).flat_map(Unit::into_jobs).collect();
    if let Some(queue) = queue {
        jobs.extend(queue.take_pending());
    }
    jobs
}

/// Gives claimed jobs a slot has not started back to the queue, on the heartbeat pool, as the
/// slot's own connection may still be in its handler's transaction, and wakes a waiting slot for
/// each, as their room is free again. A failure leaves them to lease expiry, the path of a slot
/// that died. A job given back keeps its `run_at`, which may sit below the floors the claims
/// have moved past it, so the next claim of each lane walks every band from its start.
async fn release(shared: &Shared, held: Vec<ClaimedJob>) {
    if held.is_empty() {
        return;
    }
    let jobs: Vec<_> = held.iter().map(|job| (job.id, job.lease_token)).collect();
    match release_jobs(&shared.heartbeat_pool, &jobs).await {
        Ok(_) => {
            shared.general_floors.expire();
            shared.drain_floors.expire();
            held.iter().for_each(|job| shared.left_running(job));
        }
        Err(error) => warn!(
            "could not give back {} claimed jobs, which run again once their leases expire: {error}",
            jobs.len()
        ),
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
    Failed(String, Option<Duration>),
    /// The handler of a wake failed and the failure was written in its batch transaction, which
    /// has not ended yet (#397): the outcome stands once that commits; when it does not, the
    /// failure is settled again as [`Attempt::Failed`].
    Recorded(String, Option<Duration>, Outcome),
}

/// What a job whose transaction did not commit failed with.
const NOT_COMMITTED: &str = "could not commit the job transaction";

/// The span a job runs and is settled in.
fn job_span(job: &ClaimedJob) -> Span {
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
    span
}

/// Runs one claimed job inside its `job` span and records what happened to it.
async fn process(shared: &Arc<Shared>, job: ClaimedJob) {
    let span = job_span(&job);
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
        Err(_) => Attempt::Failed("the job handler panicked".into(), None),
    };
    let outcome = settle(shared, &job, attempt, started.elapsed()).await;
    shared.left_running(&job);
    if outcome == Outcome::Completed {
        requeue_drain(shared, &job).await;
    }
}

/// With drain slots, enqueues the next drain of the shard whose run just completed while its
/// buckets still hold rows, as the job's unique key is free only now: the run ended on its time
/// budget, or rows were appended behind it. An append that raced the run's last empty check had
/// its enqueue dropped by that key, so this check, made after the key is free, is what keeps a
/// runnable job for the row. Without this the next run waits for the scheduler's safety net. A
/// job from before the split names no shard and enqueued the shard jobs itself.
async fn requeue_drain(shared: &Shared, job: &ClaimedJob) {
    let Some(workspace) = job.workspace_id.filter(|_| job.kind == DRAIN_JOB_KIND) else {
        return;
    };
    if shared.drain_slots == 0 {
        return;
    }
    let Ok(Some(shard)) = payload_shard(&job.payload) else {
        return;
    };
    if let Err(error) = enqueue_drain_if_pending(
        &shared.pool,
        WorkspaceId::from_uuid(workspace),
        shard,
        &job.trace,
    )
    .await
    {
        warn!("could not enqueue the next ingest drain: {error}");
    }
}

/// Settles an attempt that ended, then logs and counts it; `ran` is how long it ran before.
async fn settle(shared: &Shared, job: &ClaimedJob, attempt: Attempt, ran: Duration) -> Outcome {
    let settling = Instant::now();
    let mut failure = None;
    let outcome = match attempt {
        Attempt::Completed => Outcome::Completed,
        Attempt::LeaseLost => Outcome::LeaseLost,
        Attempt::Failed(reason, retry_after) => {
            let outcome = settle_failure(shared, job, &reason, retry_after).await;
            failure = Some(reason);
            outcome
        }
        Attempt::Recorded(reason, _, outcome) => {
            failure = Some(reason);
            outcome
        }
    };
    let elapsed = ran + settling.elapsed();
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    match (outcome, failure) {
        (Outcome::Completed, _) => info!(duration_ms, "job completed"),
        (Outcome::Purged, _) => info!(
            duration_ms,
            "job dropped: a person it named was erased while it ran"
        ),
        (Outcome::Retried | Outcome::Dead, Some(reason)) => warn!(
            duration_ms,
            reason = %reason,
            retry = outcome == Outcome::Retried,
            "job attempt failed"
        ),
        _ => warn!(duration_ms, "job lost its lease; its work was discarded"),
    }
    metrics::record_attempt(&job.kind, outcome, elapsed.as_secs_f64());
    outcome
}

/// How long to wait before the next attempt: the backoff, or the wait the handler asked for
/// (a `Retry-After`) when that is longer, but never past the backoff cap.
pub(crate) fn retry_delay(backoff: Duration, asked: Option<Duration>, cap: Duration) -> Duration {
    asked.map_or(backoff, |asked| backoff.max(asked.min(cap)))
}

/// When a failed job runs again: its backoff, or the wait its handler asked for.
fn failure_delay(shared: &Shared, job: &ClaimedJob, retry_after: Option<Duration>) -> Duration {
    retry_delay(
        backoff(
            u32::try_from(job.attempts).unwrap_or(u32::MAX),
            shared.config.backoff_base,
            shared.config.backoff_cap,
            random_unit(),
        ),
        retry_after,
        shared.config.backoff_cap,
    )
}

fn failed_outcome(failed: FailOutcome) -> Outcome {
    match failed {
        FailOutcome::Retried => Outcome::Retried,
        FailOutcome::Dead => Outcome::Dead,
        FailOutcome::Purged => Outcome::Purged,
    }
}

/// Retries or buries a job whose handler failed, fenced on its lease.
async fn settle_failure(
    shared: &Shared,
    job: &ClaimedJob,
    reason: &str,
    retry_after: Option<Duration>,
) -> Outcome {
    let delay = failure_delay(shared, job, retry_after);
    match fail_job(&shared.pool, job.id, job.lease_token, delay, reason).await {
        Ok(Some(failed)) => failed_outcome(failed),
        Ok(None) => Outcome::LeaseLost,
        Err(error) => {
            // The lease runs out and the reaper takes the job from here.
            warn!("could not record the failure of job {}: {error}", job.id);
            Outcome::LeaseLost
        }
    }
}

/// The handler's context, with everything the worker's configuration gives a handler.
fn configured<'a>(shared: &Shared, ctx: JobContext<'a>) -> JobContext<'a> {
    let config = &shared.config;
    ctx.with_event_retention(config.event_retention)
        .with_export_max_bytes(config.export_max_bytes)
        .with_message_retention(config.message_retention)
        .with_decision_retention(config.decision_retention)
        .with_retention_settings(config.retention_settings)
        .with_log_retention_days(config.job_history_days, config.audit_days)
        .with_identifier_key(config.identifier_key.clone())
        .with_ledger_dir(config.ledger_dir.clone())
        .with_clock(config.clock.clone())
        .with_rate_gates(Arc::clone(&config.rate_gates))
        .with_render_pool(config.render_pool.clone())
        .with_dns(config.dns.clone())
        .with_tracking(config.public_url.clone(), config.tracking_key.clone())
        .with_outbound(
            config.keyring.clone(),
            Arc::clone(&config.egress),
            Arc::clone(&config.rate_limits),
        )
        .with_ingest(IngestSettings {
            pool: shared.pool.clone(),
            redaction_key: config.redaction_key.clone(),
            batch_size: config.ingest_drain_batch_size,
            shards: config.ingest_drain_shards,
        })
        .with_unit_sizes(crate::context::UnitSizes {
            kafka_messages: config.kafka_unit_messages,
            sync_records: config.sync_unit_records,
        })
}

/// The handler's transaction: take its person-change guard if needed, confirm the lease (with
/// the job row locked, unless the kind says otherwise), run the handler beside a heartbeat, then
/// delete the job and commit with everything the handler wrote.
async fn execute(shared: Arc<Shared>, job: ClaimedJob) -> Attempt {
    let Some(kind) = shared.registry.get(&job.kind) else {
        return Attempt::Failed("no handler is registered for this job kind".into(), None);
    };
    let Ok(mut tx) = begin(&shared.pool).await else {
        return Attempt::Failed("could not start the job transaction".into(), None);
    };
    metrics::record_transaction(&job.kind);
    if kind.needs_change_readers()
        && let Some(workspace) = job.workspace_id
        && cache_change_readers(&mut tx, WorkspaceId::from_uuid(workspace))
            .await
            .is_err()
    {
        return Attempt::Failed("could not guard person changes".into(), None);
    }
    let held = if kind.locks_lease() {
        lock_lease(&mut tx, job.id, job.lease_token).await
    } else {
        confirm_lease(&mut *tx, job.id, job.lease_token).await
    };
    match held {
        Ok(true) => {}
        Ok(false) => return Attempt::LeaseLost,
        Err(_) => return Attempt::Failed("could not confirm the lease".into(), None),
    }

    let (lost, mut lease_lost) = oneshot::channel();
    let _heartbeat = AbortOnDrop(tokio::spawn(heartbeat(
        Arc::clone(&shared),
        job.clone(),
        lost,
    )));
    let result = {
        let mut ctx = configured(&shared, JobContext::new(&mut tx, &job));
        let handler = kind.run(&mut ctx, job.payload.clone());
        tokio::select! {
            result = handler => Some(result),
            // Dropping the handler cancels it; its transaction rolls back below.
            Ok(()) = &mut lease_lost => None,
        }
    };

    match result {
        None => Attempt::LeaseLost,
        Some(Err(error)) => Attempt::Failed(error.to_string(), error.retry_after()),
        Some(Ok(())) => match complete_job(&mut *tx, job.id, job.lease_token).await {
            Ok(true) => match tx.commit().await {
                Ok(()) => Attempt::Completed,
                Err(_) => Attempt::Failed(NOT_COMMITTED.into(), None),
            },
            // Another worker owns the job now: roll back this attempt's writes.
            Ok(false) => Attempt::LeaseLost,
            Err(_) => Attempt::Failed("could not complete the job".into(), None),
        },
    }
}

/// The wakes of a batch transaction: the ones not begun, which the transaction begins one at a
/// time and the slot gives back once the hold has passed, and the ones begun.
#[derive(Default)]
struct WakeQueue {
    pending: VecDeque<ClaimedJob>,
    begun: Vec<ClaimedJob>,
}

/// A [`WakeQueue`] that the transaction's task and the slot share.
struct Queue(Mutex<WakeQueue>);

impl Queue {
    fn new(jobs: Vec<ClaimedJob>) -> Self {
        Self(Mutex::new(WakeQueue {
            pending: jobs.into(),
            begun: Vec::new(),
        }))
    }

    /// Nothing panics while it holds the lock, so a poisoned lock still holds a whole queue.
    fn lock(&self) -> MutexGuard<'_, WakeQueue> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    fn pending(&self) -> Vec<ClaimedJob> {
        self.lock().pending.iter().cloned().collect()
    }

    /// Puts the wakes not begun in the order of `key`; a stable sort keeps the claim's order
    /// between ties.
    fn order<K: Ord>(&self, key: impl FnMut(&ClaimedJob) -> K) {
        self.lock().pending.make_contiguous().sort_by_key(key);
    }

    /// The next wake, which counts as begun from here.
    fn begin_next(&self) -> Option<ClaimedJob> {
        let mut queue = self.lock();
        let job = queue.pending.pop_front()?;
        queue.begun.push(job.clone());
        Some(job)
    }

    /// Every wake not begun, which all count as begun from here.
    fn begin_all(&self) -> Vec<ClaimedJob> {
        let mut queue = self.lock();
        let jobs: Vec<ClaimedJob> = queue.pending.drain(..).collect();
        queue.begun.extend(jobs.iter().cloned());
        jobs
    }

    /// The wakes not begun, taken out to be given back.
    fn take_pending(&self) -> Vec<ClaimedJob> {
        self.lock().pending.drain(..).collect()
    }

    fn begun(&self) -> Vec<ClaimedJob> {
        self.lock().begun.clone()
    }
}

/// A batch transaction about to run: the `flow.step` jobs of one workspace that one claim
/// returned (#397).
struct Wakes {
    workspace: WorkspaceId,
    queue: Arc<Queue>,
    /// When the claim returned, for the claim lag.
    claimed: tokio::time::Instant,
    /// No wake begins at or past it: [`hold_limit`] after the claim returned.
    deadline: tokio::time::Instant,
    stop: watch::Receiver<bool>,
}

/// A wake of a batch transaction and how its attempt ended. `took` is the time of its own work,
/// from its savepoint to its release or rollback.
struct Ran {
    job: ClaimedJob,
    attempt: Attempt,
    took: Duration,
    span: Span,
}

impl Ran {
    fn failed(job: ClaimedJob, reason: &str) -> Self {
        let span = job_span(&job);
        Self {
            job,
            attempt: Attempt::Failed(reason.to_owned(), None),
            took: Duration::ZERO,
            span,
        }
    }
}

/// Runs a batch transaction, gives back the wakes it did not begin, then settles, logs and counts
/// each wake it began in that wake's span, as [`process`] does a job. A job's time is its own wake
/// and an equal share of what the transaction spent around the wakes (its beginning, guard, plan
/// and commit), so the durations still add up to the slot's time. Returns how each begun wake
/// ended.
async fn process_wakes(shared: &Arc<Shared>, wakes: Wakes) -> Vec<(Uuid, Outcome)> {
    let started = Instant::now();
    let queue = Arc::clone(&wakes.queue);
    let mut task = AbortOnDrop(tokio::spawn(
        execute_wakes(Arc::clone(shared), wakes).instrument(Span::current()),
    ));
    let ran = match (&mut task.0).await {
        Ok(ran) => ran,
        // A panicking handler fails every wake begun, as it fails a job alone: the transaction
        // rolled back with the task.
        Err(_) => queue
            .begun()
            .into_iter()
            .map(|job| Ran::failed(job, "the job handler panicked"))
            .collect(),
    };
    release(shared, queue.take_pending()).await;
    let own: Duration = ran.iter().map(|wake| wake.took).sum();
    let count = u32::try_from(ran.len().max(1)).unwrap_or(u32::MAX);
    let share = started.elapsed().saturating_sub(own) / count;
    let mut outcomes = Vec::with_capacity(ran.len());
    for wake in ran {
        let Ran {
            job,
            attempt,
            took,
            span,
        } = wake;
        let outcome = settle(shared, &job, attempt, took + share)
            .instrument(span)
            .await;
        shared.left_running(&job);
        outcomes.push((job.id, outcome));
    }
    outcomes
}

/// The batch transaction of the `flow.step` jobs of one workspace (#397): begin, take the
/// person-change guard, read every wake's enrollment and hold their flows ([`plan_wakes`]), run
/// the wakes one after another in the order their steps lock people in, each inside a savepoint
/// of its own, then complete the wakes that completed, append the batch's decisions and commit
/// once ([`complete_wakes`]).
///
/// A wake runs as a job alone does, its lease check and handler beside its heartbeat, in its
/// savepoint, which is released when the handler succeeds; its heartbeat beats on until the
/// batch ends, through the fenced delete, which can wait behind a lock on a job row. A wake that
/// fails or loses its lease rolls back to its savepoint, which
/// undoes its writes, drops its decisions and frees its locks, and the next wake goes on. A
/// failure is then written in the transaction ([`record_failure`]), with the attempt counted as a
/// job alone's is, and commits with the batch. A wake begins only before the deadline and while
/// the pool runs; the rest stay in the queue, to be given back.
///
/// When the transaction cannot go on (a savepoint that cannot be begun, released or rolled back),
/// a wake that completed lost its lease by the completion, or the completion, the append or the
/// commit fails, nothing of it is kept: a wake that lost its lease is left to the reaper, as a
/// job alone's is, every other wake that completed fails as a job alone whose commit failed, back
/// to the queue with an attempt counted or dead on its last attempt, and every failure it
/// recorded is settled again after it, as a job alone's is.
async fn execute_wakes(shared: Arc<Shared>, wakes: Wakes) -> Vec<Ran> {
    let Wakes {
        workspace,
        queue,
        claimed,
        deadline,
        stop,
    } = wakes;
    let fail_all = |reason: &str| -> Vec<Ran> {
        queue
            .begin_all()
            .into_iter()
            .map(|job| Ran::failed(job, reason))
            .collect()
    };
    let Some(kind) = shared.registry.get(STEP_JOB_KIND) else {
        return fail_all("no handler is registered for this job kind");
    };
    let Ok(mut tx) = begin(&shared.pool).await else {
        return fail_all("could not start the job transaction");
    };
    metrics::record_transaction(STEP_JOB_KIND);
    if kind.needs_change_readers() && cache_change_readers(&mut tx, workspace).await.is_err() {
        return fail_all("could not guard person changes");
    }
    let mut batch =
        match plan_wakes(&mut tx, &shared.config.clock, workspace, &queue.pending()).await {
            Ok(batch) => batch,
            Err(error) => return fail_all(&JobError::from(error).to_string()),
        };
    queue.order(|job| batch.lock_order(job));

    let mut ran = Vec::new();
    let mut beating = Vec::new();
    let mut usable = true;
    while usable && !*stop.borrow() && tokio::time::Instant::now() < deadline {
        let Some(job) = queue.begin_next() else {
            break;
        };
        // The lag runs to the start, so a wait behind the jobs claimed with this one counts.
        metrics::record_claim_lag(
            &job.kind,
            job.claim_lag_seconds + claimed.elapsed().as_secs_f64(),
        );
        let span = job_span(&job);
        let started = Instant::now();
        let attempt = run_wake(&shared, kind, &mut tx, &job, &mut batch)
            .instrument(span.clone())
            .await;
        let attempt = match attempt {
            Some((Attempt::Failed(reason, retry_after), _)) => {
                let (attempt, went_on) =
                    record_failure(&shared, &mut tx, &job, reason, retry_after)
                        .instrument(span.clone())
                        .await;
                usable = went_on;
                attempt
            }
            Some((attempt, heartbeat)) => {
                beating.extend(heartbeat);
                attempt
            }
            None => {
                usable = false;
                Attempt::Failed(NOT_COMMITTED.into(), None)
            }
        };
        ran.push(Ran {
            job,
            attempt,
            took: started.elapsed(),
            span,
        });
    }
    let lost = if usable {
        complete_wakes(&mut tx, &mut batch, &ran).await
    } else {
        None
    };
    let committed = match lost {
        Some(ref lost) if lost.is_empty() => tx.commit().await.is_ok(),
        _ => {
            if tx.rollback().await.is_err() {
                // The connection is broken; the server ends the transaction when it goes.
                warn!("could not roll back a batch transaction");
            }
            false
        }
    };
    // The heartbeats beat until here: a delete that waits behind a lock on a job row past the
    // lease would otherwise find the reaper's token on it. A beat on a row the delete took waits
    // for the transaction, which has ended now.
    drop(beating);
    if committed {
        return ran;
    }
    let lost = lost.unwrap_or_default();
    // Rolled back before the failures are settled, so its row locks are gone by then, and so are
    // the failures it recorded: their leases are as they were, and they are settled again.
    for wake in &mut ran {
        let attempt = std::mem::replace(&mut wake.attempt, Attempt::LeaseLost);
        wake.attempt = match attempt {
            Attempt::Completed if lost.contains(&wake.job.id) => Attempt::LeaseLost,
            Attempt::Completed => Attempt::Failed(NOT_COMMITTED.into(), None),
            Attempt::Recorded(reason, retry_after, _) => Attempt::Failed(reason, retry_after),
            attempt => attempt,
        };
    }
    ran
}

/// Records the failure of a wake whose savepoint rolled back in the batch transaction, outside
/// that savepoint, in a savepoint of its own (supervisor ruling 4e9d24be). The job row is then
/// `ready` (or `dead`, or deleted for an erasure) in the transaction, so a reaper that waits for
/// the batch finds it settled once the batch commits, instead of a running job whose lease ran
/// out while the later wakes ran. A failure that cannot be recorded here (the statement failed,
/// say a deadlock with a reaper that holds the row) is rolled back and settled after the
/// transaction, as a job alone's is; no row means the lease was lost. Returns the attempt and
/// whether the transaction can go on.
async fn record_failure(
    shared: &Shared,
    tx: &mut PgConnection,
    job: &ClaimedJob,
    reason: String,
    retry_after: Option<Duration>,
) -> (Attempt, bool) {
    let Ok(mut savepoint) = tx.begin().await else {
        return (Attempt::Failed(reason, retry_after), false);
    };
    let delay = failure_delay(shared, job, retry_after);
    match fail_job(&mut *savepoint, job.id, job.lease_token, delay, &reason).await {
        Ok(Some(failed)) => {
            let recorded = Attempt::Recorded(reason.clone(), retry_after, failed_outcome(failed));
            match savepoint.commit().await {
                Ok(()) => (recorded, true),
                Err(_) => (Attempt::Failed(reason, retry_after), false),
            }
        }
        Ok(None) => (Attempt::LeaseLost, savepoint.rollback().await.is_ok()),
        Err(_) => {
            let went_on = savepoint.rollback().await.is_ok();
            (Attempt::Failed(reason, retry_after), went_on)
        }
    }
}

/// The end of a batch transaction that can still commit: deletes the jobs of the wakes that
/// completed, each fenced on its lease, in one statement ([`complete_jobs`]), then appends the
/// decisions of the wakes kept, in one statement, and moves the counts of the lists they changed,
/// by each list's net. Returns the wakes that completed but lost their lease, so the batch must not
/// commit, or `None` when a statement failed.
async fn complete_wakes(
    tx: &mut PgConnection,
    batch: &mut WakeBatch,
    ran: &[Ran],
) -> Option<Vec<Uuid>> {
    let completed: Vec<(Uuid, Uuid)> = ran
        .iter()
        .filter(|wake| matches!(wake.attempt, Attempt::Completed))
        .map(|wake| (wake.job.id, wake.job.lease_token))
        .collect();
    let deleted = complete_jobs(&mut *tx, &completed).await.ok()?;
    let lost: Vec<Uuid> = completed
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| !deleted.contains(id))
        .collect();
    if lost.is_empty() {
        batch.flush(tx).await.ok()?;
    }
    Some(lost)
}

/// One wake of a batch transaction, inside a savepoint of its own, with the heartbeat of a wake
/// that completed. `None` means the savepoint could not be begun, released or rolled back, which
/// leaves the transaction unusable.
async fn run_wake(
    shared: &Arc<Shared>,
    kind: &'static dyn ErasedKind,
    tx: &mut PgConnection,
    job: &ClaimedJob,
    batch: &mut WakeBatch,
) -> Option<(Attempt, Option<AbortOnDrop<()>>)> {
    let mark = batch.mark();
    let mut savepoint = tx.begin().await.ok()?;
    let (attempt, heartbeat) = match lock_lease(&mut savepoint, job.id, job.lease_token).await {
        Ok(true) => attempt_wake(shared, kind, &mut savepoint, job, batch).await,
        Ok(false) => (Attempt::LeaseLost, None),
        Err(_) => (
            Attempt::Failed("could not confirm the lease".into(), None),
            None,
        ),
    };
    let ended = match attempt {
        Attempt::Completed => savepoint.commit().await,
        Attempt::LeaseLost | Attempt::Failed(..) | Attempt::Recorded(..) => {
            batch.discard_after(mark);
            savepoint.rollback().await
        }
    };
    ended.ok().map(|()| (attempt, heartbeat))
}

/// A wake's handler beside its heartbeat, which a wake that completed hands back to beat on until
/// the batch completes it.
async fn attempt_wake(
    shared: &Arc<Shared>,
    kind: &'static dyn ErasedKind,
    connection: &mut PgConnection,
    job: &ClaimedJob,
    batch: &mut WakeBatch,
) -> (Attempt, Option<AbortOnDrop<()>>) {
    let (lost, mut lease_lost) = oneshot::channel();
    let heartbeat = AbortOnDrop(tokio::spawn(heartbeat(
        Arc::clone(shared),
        job.clone(),
        lost,
    )));
    let result = {
        let mut ctx = configured(shared, JobContext::new(&mut *connection, job)).with_wakes(batch);
        let handler = kind.run(&mut ctx, job.payload.clone());
        tokio::select! {
            result = handler => Some(result),
            // Dropping the handler cancels it; its savepoint rolls back.
            Ok(()) = &mut lease_lost => None,
        }
    };
    match result {
        None => (Attempt::LeaseLost, None),
        Some(Err(error)) => (
            Attempt::Failed(error.to_string(), error.retry_after()),
            None,
        ),
        Some(Ok(())) => (Attempt::Completed, Some(heartbeat)),
    }
}

/// Runs `jobs`, the `flow.step` jobs of `workspace` that one claim returned, in one batch
/// transaction, and settles each as a slot does, with no hold: the test seat of
/// [`process_wakes`].
#[cfg(test)]
pub(crate) async fn run_wakes(
    pool: &PgPool,
    config: WorkerConfig,
    workspace: WorkspaceId,
    jobs: Vec<ClaimedJob>,
) -> Vec<(Uuid, Outcome)> {
    let shared = Arc::new(Shared::new(
        pool.clone(),
        pool.clone(),
        pool.clone(),
        Registry::built_in(),
        config,
    ));
    let now = tokio::time::Instant::now();
    let (_running, stop) = watch::channel(false);
    let wakes = Wakes {
        workspace,
        queue: Arc::new(Queue::new(jobs)),
        claimed: now,
        deadline: now + Duration::from_secs(3600),
        stop,
    };
    process_wakes(&shared, wakes).await
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
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{NewJob, QueueCounts, claim_job, enqueue, find_job, queue_counts};

    use super::*;
    use crate::{JobError, JobKind};

    #[test]
    fn a_requested_wait_lengthens_the_retry_but_never_past_the_cap() {
        let second = Duration::from_secs(1);
        let cap = Duration::from_secs(60);

        assert_eq!(retry_delay(second, None, cap), second);
        assert_eq!(
            retry_delay(second, Some(Duration::from_secs(30)), cap),
            Duration::from_secs(30)
        );
        assert_eq!(retry_delay(second, Some(Duration::ZERO), cap), second);
        assert_eq!(
            retry_delay(second, Some(Duration::from_secs(9999)), cap),
            cap
        );
        assert_eq!(
            retry_delay(
                Duration::from_secs(90),
                Some(Duration::from_secs(9999)),
                cap
            ),
            Duration::from_secs(90),
            "the backoff itself is never shortened"
        );
    }

    #[test]
    fn the_pool_holds_a_job_transaction_and_a_batch_for_every_slot_and_one_spare() {
        assert_eq!(pool_size(4), 9);
        assert_eq!(pool_size(1), 3);
        assert_eq!(pool_size(72), 145);
        assert_eq!(pool_size(u32::MAX), u32::MAX);
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
            ctx.record_decision(
                &zirv_flow_core::decision_actions::FLOW_WAKE_TIMER,
                None,
                Vec::new(),
            )
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

    /// Records, as it starts, the lease its job holds and how much of it is left, then works
    /// for ten milliseconds.
    struct RecordsLease;

    impl JobKind for RecordsLease {
        const KIND: &'static str = "test.lease";

        async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
            sqlx::query(
                "INSERT INTO leases SELECT id, lease_expires_at, \
                    EXTRACT(EPOCH FROM lease_expires_at - clock_timestamp()) \
                 FROM jobs WHERE id = $1",
            )
            .bind(ctx.job_id())
            .execute(ctx.conn())
            .await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
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
            .with(&RecordsLease)
    }

    fn test_config(concurrency: usize) -> WorkerConfig {
        WorkerConfig {
            concurrency,
            drain_slots: 0,
            lease: Duration::from_secs(30),
            poll_interval: Duration::from_millis(20),
            max_running_per_workspace: None,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(5),
            event_retention: EventRetention::default(),
            export_max_bytes: zirv_flow_store::EXPORT_MAX_BYTES_DEFAULT,
            message_retention: MessageRetention::default(),
            decision_retention: DecisionRetention::default(),
            retention_settings: RetentionSettings::default(),
            job_history_days: crate::context::DEFAULT_JOB_HISTORY_DAYS,
            audit_days: crate::context::DEFAULT_AUDIT_DAYS,
            redaction_key: None,
            identifier_key: None,
            ledger_dir: None,
            keyring: None,
            ingest_drain_batch_size: 500,
            ingest_drain_shards: 4,
            kafka_unit_messages: 100,
            sync_unit_records: 100,
            egress: Arc::default(),
            rate_limits: Arc::default(),
            rate_gates: Arc::default(),
            clock: Clock::default(),
            render_pool: None,
            dns: None,
            public_url: None,
            tracking_key: None,
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
    ///
    /// On a timeout the panic carries the last counts, so a slow drain (jobs still moving)
    /// can be told from a stalled one (the same counts as before).
    async fn wait_for(pool: &PgPool, done: impl Fn(QueueCounts) -> bool) {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            let counts = queue_counts(pool).await.unwrap();
            if done(counts) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {}s waiting for the queue to reach the expected counts; last counts {counts:?}, {} effects written",
                WAIT_LIMIT.as_secs(),
                effect_count(pool).await
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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

    fn claimed(kind: &str, workspace: Option<u128>, index: u128) -> ClaimedJob {
        ClaimedJob {
            id: Uuid::from_u128(index),
            workspace_id: workspace.map(Uuid::from_u128),
            kind: kind.to_owned(),
            payload: json!({}),
            attempts: 1,
            max_attempts: 5,
            lease_token: Uuid::from_u128(index),
            unique_key: None,
            trace: TraceContext::root(Cause::api("t")),
            claim_lag_seconds: 0.0,
        }
    }

    /// Each unit as `None` for a job alone or the workspace of a shared transaction, with the
    /// indexes of its jobs.
    fn shapes(units: VecDeque<Unit>) -> Vec<(Option<u128>, Vec<u128>)> {
        units
            .into_iter()
            .map(|unit| match unit {
                Unit::Alone(job) => (None, vec![job.id.as_u128()]),
                Unit::Wakes(workspace, jobs) => (
                    Some(workspace.as_uuid().as_u128()),
                    jobs.iter().map(|job| job.id.as_u128()).collect(),
                ),
            })
            .collect()
    }

    #[test]
    fn the_wakes_of_a_workspace_share_a_transaction_at_the_first_ones_place_and_the_rest_run_alone()
    {
        let batch = vec![
            claimed("test.effect", Some(1), 1),
            claimed(STEP_JOB_KIND, Some(1), 2),
            claimed(STEP_JOB_KIND, Some(2), 3),
            claimed(STEP_JOB_KIND, Some(1), 4),
            claimed("test.effect", Some(1), 5),
            claimed(STEP_JOB_KIND, None, 6),
            claimed(STEP_JOB_KIND, Some(1), 7),
            claimed(STEP_JOB_KIND, Some(3), 8),
        ];

        assert_eq!(
            shapes(units(batch)),
            [
                (None, vec![1]),
                (Some(1), vec![2, 4, 7]),
                (None, vec![3]),
                (None, vec![5]),
                (None, vec![6]),
                (None, vec![8]),
            ],
            "workspace 2 and 3 have one wake each, and a wake without a workspace runs alone"
        );
        assert!(units(Vec::new()).is_empty());
    }

    /// The handler p99.7 measured at 10 M waiting enrollments (#397).
    const HANDLER_P997: Duration = Duration::from_millis(25);

    #[test]
    fn a_held_job_starts_with_most_of_its_lease_left_and_a_batch_of_typical_handlers_is_not_cut() {
        for seconds in [1, 2, 3, 6, 30, 90, 600, 3600] {
            let lease = Duration::from_secs(seconds);
            // The latest start, then the heartbeat's second beat: still inside the claim's lease.
            assert!(
                hold_limit(lease) + lease * 2 / 3 < lease,
                "a {seconds} s lease"
            );
            assert!(hold_limit(lease) <= MAX_HOLD);
        }
        let lease = WorkerConfig::new(1).lease;
        assert_eq!(hold_limit(lease), MAX_HOLD);
        assert!(
            HANDLER_P997 * CLAIM_BATCH * 5 <= hold_limit(lease),
            "a batch of slow handlers uses a fifth of the hold at most"
        );
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
    async fn a_person_writer_waits_for_the_segment_guard_before_locking_its_job(pool: PgPool) {
        use zirv_flow_core::{Slug, WorkspaceName};

        migrated(&pool).await;
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let mut job = NewJob::new("traits.recompute", TraceContext::root(Cause::api("t")));
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "key": "plan"});
        enqueue(&pool, &job).await.unwrap();
        let claimed = claim_job(
            &pool,
            "w",
            &["traits.recompute"],
            Duration::from_secs(60),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let job_id = claimed.id;
        let mut save = begin(&pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut save, ws).await.unwrap();
        let save_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *save)
            .await
            .unwrap();
        let shared = Arc::new(Shared::new(
            pool.clone(),
            pool.clone(),
            pool.clone(),
            Registry::built_in(),
            test_config(1),
        ));
        let writer = tokio::spawn(execute(shared, claimed));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' \
                     AND NOT granted AND $1 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(save_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                assert!(!writer.is_finished());
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the writer must wait for the segment guard");
        let mut probe = begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(job_id)
            .fetch_one(&mut *probe)
            .await
            .expect("a writer waiting for the guard must not hold its lease row lock");
        probe.rollback().await.unwrap();
        save.commit().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(10), writer)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Attempt::Completed));
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

    /// #397: a drain's job transaction confirms its lease without locking the job row, so for as
    /// long as the run lasts (here, until its batches may take the segment guard the test holds)
    /// it holds neither a transaction id nor a snapshot, and so does not hold the xmin horizon
    /// back for the claims of every other job.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_drains_job_transaction_holds_no_transaction_id(pool: PgPool) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        use zirv_flow_store::{IngestItem, IngestPayload, append_inbox};

        migrated(&pool).await;
        let ws = workspace(&pool).await;
        let items: Vec<IngestItem> = (0..3)
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
            Uuid::now_v7(),
            &TraceContext::root(Cause::api("req")),
            &items,
        )
        .await
        .unwrap();
        let mut guard = begin(&pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut guard, ws)
            .await
            .unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *guard)
            .await
            .unwrap();
        let mut config = test_config(1);
        config.redaction_key = Some(Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction),
        ));
        let (stop, handle) = start(&pool, config);
        // Scoped to the guard: pg_locks spans the cluster, where other tests' waits show too.
        eventually("a drain batch to wait for the segment guard", || async {
            sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' \
                 AND NOT granted AND $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(holder)
            .fetch_one(&pool)
            .await
            .unwrap()
        })
        .await;

        let job_transactions: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT backend_xid::text, backend_xmin::text FROM pg_stat_activity \
             WHERE datname = current_database() AND state = 'idle in transaction' \
             AND query LIKE 'SELECT 1 FROM jobs WHERE id = $1 AND lease_token = $2%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        guard.rollback().await.unwrap();
        wait_until(
            &pool,
            "the inbox to drain",
            "SELECT NOT EXISTS (SELECT 1 FROM ingest_inbox)",
        )
        .await;
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        assert_eq!(job_transactions, [(None, None)]);
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
                    Some("job_error".to_owned())
                ),
                ("flow.wake".to_owned(), "timer".to_owned(), None),
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
    async fn a_slot_claims_a_batch_in_one_statement_and_starts_each_job_well_inside_its_lease(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        sqlx::query(
            "CREATE TABLE leases (job uuid PRIMARY KEY, expires timestamptz, left_s float8)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for _ in 0..8 {
            enqueue(&pool, &new_job("test.lease")).await.unwrap();
        }
        let mut config = test_config(1);
        config.lease = Duration::from_secs(6);
        let (stop, handle) = start(&pool, config);

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        let (started, claims, least_left): (i64, i64, f64) =
            sqlx::query_as("SELECT count(*), count(DISTINCT expires), min(left_s) FROM leases")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(started, 8);
        assert_eq!(claims, 1, "one claim statement leased all eight at once");
        // Each started within the hold of a second, so with at least five of its six seconds.
        assert!(
            least_left > 4.5,
            "{least_left} s of the lease left at a start"
        );
    }

    /// Notes in `given_back` each job that goes back from running to ready unstarted.
    async fn note_jobs_given_back(pool: &PgPool) {
        sqlx::raw_sql(
            "CREATE TABLE given_back (job uuid); \
             CREATE FUNCTION note_given_back() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN INSERT INTO given_back VALUES (NEW.id); RETURN NULL; END $$; \
             CREATE TRIGGER given_back AFTER UPDATE OF state ON jobs FOR EACH ROW \
             WHEN (OLD.state = 'running' AND NEW.state = 'ready') \
             EXECUTE FUNCTION note_given_back();",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn given_back(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM given_back")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Under a cap a claim is a transaction that can wait, here for a table lock. Its leases run
    /// from its update and the hold from its return, so the slot runs the batch it waited for,
    /// each job with five sixths of its lease left, and gives none of it back. Before, the
    /// leases ran from the transaction's start and the hold from the claim's sending, so a batch
    /// that waited past the hold went back unstarted (bench phase 4a, #397).
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_claim_that_waited_keeps_its_leases_and_runs_its_batch(pool: PgPool) {
        migrated(&pool).await;
        note_jobs_given_back(&pool).await;
        sqlx::query(
            "CREATE TABLE leases (job uuid PRIMARY KEY, expires timestamptz, left_s float8)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for _ in 0..2 {
            enqueue(&pool, &new_job("test.lease")).await.unwrap();
        }
        // The worker's claim waits for this lock.
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE jobs IN EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let mut config = test_config(1);
        config.lease = Duration::from_secs(6);
        config.max_running_per_workspace = Some(8);
        let (stop, handle) = start(&pool, config);
        wait_until(
            &pool,
            "the claim to wait for the table lock",
            "SELECT EXISTS (SELECT 1 FROM pg_locks \
             WHERE relation = 'jobs'::regclass AND NOT granted)",
        )
        .await;
        // Three seconds pass before the claim returns, well past the hold of one.
        tokio::time::sleep(Duration::from_secs(3)).await;
        blocker.commit().await.unwrap();

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;

        let (started, least_left): (i64, f64) =
            sqlx::query_as("SELECT count(*), min(left_s) FROM leases")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(started, 2);
        assert_eq!(
            given_back(&pool).await,
            0,
            "the batch that waited went back"
        );
        assert!(
            least_left > 4.5,
            "a job started with {least_left} s of its lease left"
        );
    }

    /// Bench phase 4a's livelock (#397): capped claims queue on the claim lock, so with sixteen
    /// slots and slow claims (a quarter of a second each, here from a trigger) every claim
    /// waits seconds, far past the hold of half a second. The jobs still run: each slot starts
    /// what it claimed, and every job completes once.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn capped_claims_queued_past_the_hold_still_run_their_jobs(pool: PgPool) {
        migrated(&pool).await;
        let ws = workspace(&pool).await;
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, 'test.effect', jsonb_build_object('index', index), \
                    gen_random_uuid(), 'manual', '' \
             FROM generate_series(0, 39) AS index",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        // Every statement on `jobs` in a transaction holding an advisory lock, which under a cap
        // only a claim does, takes a quarter of a second more.
        sqlx::raw_sql(
            "CREATE FUNCTION slow_claim() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 IF EXISTS (SELECT FROM pg_locks WHERE pid = pg_backend_pid() \
                            AND locktype = 'advisory' AND granted) THEN \
                     PERFORM pg_sleep(0.25); \
                 END IF; \
                 RETURN NULL; \
             END $$; \
             CREATE TRIGGER slow_claim AFTER UPDATE ON jobs FOR EACH STATEMENT \
             EXECUTE FUNCTION slow_claim();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut config = test_config(16);
        config.lease = Duration::from_secs(3);
        config.max_running_per_workspace = Some(4);
        let (stop, handle) = start(&pool, config);

        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
        assert_eq!(effect_count(&pool).await, 40);
    }

    /// How many idle waits a scrape counts as ended by `ended`; none when it has no such series.
    fn idle_waits(scrape: &str, ended: &str) -> u64 {
        let series = format!("zirv_flow_job_idle_waits_total{{ended=\"{ended}\"}} ");
        scrape
            .lines()
            .find_map(|line| line.strip_prefix(series.as_str()))
            .map_or(0, |count| count.trim().parse().unwrap())
    }

    /// Bench phase 4's empty-claim sleeps (#397): under a cap of four, most of sixteen slots find
    /// the workspace at its cap and wait. Polling alone left each such slot asleep for the whole
    /// poll interval while jobs were due; every job that leaves running wakes one of them, so no
    /// wait runs out the poll before the queue is empty, and the cap still holds.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn under_a_cap_each_finished_job_wakes_a_waiting_slot_before_the_poll(pool: PgPool) {
        const JOBS: i64 = 2_000;
        migrated(&pool).await;
        let ws = workspace(&pool).await;
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, 'maintenance.noop', '{}', gen_random_uuid(), \
                    'manual', '' \
             FROM generate_series(1, $2::bigint)",
        )
        .bind(ws.as_uuid())
        .bind(JOBS)
        .execute(&pool)
        .await
        .unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let scrape = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut config = test_config(16);
        config.poll_interval = Duration::from_secs(1);
        config.max_running_per_workspace = Some(4);
        let started = Instant::now();
        let (stop, handle) = start(&pool, config);

        let mut most_running = 0;
        loop {
            let (running, left): (i64, i64) = sqlx::query_as(
                "SELECT count(*) FILTER (WHERE state = 'running'), count(*) FROM jobs",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            most_running = most_running.max(running);
            if left == 0 {
                break;
            }
            assert!(
                started.elapsed() < WAIT_LIMIT,
                "{left} of {JOBS} jobs left after {}s",
                WAIT_LIMIT.as_secs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let drained = started.elapsed();
        let output = scrape.render();
        stop_and_join(stop, handle).await;

        let polled = idle_waits(&output, "poll");
        eprintln!(
            "{JOBS} jobs in {drained:?} ({:.0} jobs/s); idle waits ended by poll {polled}, by a \
             freed job {}, by an enqueue {}",
            JOBS as f64 / drained.as_secs_f64(),
            idle_waits(&output, "freed"),
            idle_waits(&output, "enqueued"),
        );
        assert!(most_running <= 4, "{most_running} jobs ran at once");
        // Without the wake about a hundred waits run out; the margin takes one stall of more
        // than the poll on a loaded runner, when each of the 12 idle slots runs out once.
        assert!(
            polled <= 24,
            "{polled} idle waits ran out the poll interval while jobs were due"
        );
    }

    /// The slot claims the gated job and the effect behind it, and runs the gated one, which
    /// waits; the effect goes back to the queue at the hold limit, while the gated job still runs.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_slot_gives_back_the_jobs_held_behind_a_slow_one_while_it_runs(pool: PgPool) {
        migrated(&pool).await;
        sqlx::query("CREATE TABLE gate (open boolean)")
            .execute(&pool)
            .await
            .unwrap();
        let mut slow = new_job("test.gated");
        slow.payload = json!({"index": 1});
        enqueue(&pool, &slow).await.unwrap();
        let mut behind = new_job("test.effect");
        behind.payload = json!({"index": 2});
        enqueue(&pool, &behind).await.unwrap();
        let mut config = test_config(1);
        config.lease = Duration::from_secs(3);
        let (stop, handle) = start(&pool, config);

        wait_until(
            &pool,
            "the effect to be claimed with the gated job",
            "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'test.effect' AND state = 'running')",
        )
        .await;
        wait_until(
            &pool,
            "the held effect to be given back while the gated job runs",
            "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'test.effect' AND state = 'ready' \
             AND attempts = 0 AND lease_token IS NULL AND locked_by IS NULL) \
             AND EXISTS (SELECT 1 FROM jobs WHERE kind = 'test.gated' AND state = 'running')",
        )
        .await;
        assert_eq!(effect_count(&pool).await, 0);

        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop_and_join(stop, handle).await;
        assert_eq!(effect_count(&pool).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stopping_gives_back_the_held_jobs_and_starts_none_of_them(pool: PgPool) {
        migrated(&pool).await;
        sqlx::query("CREATE TABLE gate (open boolean)")
            .execute(&pool)
            .await
            .unwrap();
        let mut slow = new_job("test.gated");
        slow.payload = json!({"index": 1});
        enqueue(&pool, &slow).await.unwrap();
        let mut behind = new_job("test.effect");
        behind.payload = json!({"index": 2});
        enqueue(&pool, &behind).await.unwrap();
        let (stop, handle) = start(&pool, test_config(1));
        wait_until(
            &pool,
            "the effect to be claimed with the gated job",
            "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'test.effect' AND state = 'running')",
        )
        .await;
        // Both are claimed together: a stop before the gated job starts gives both back.
        wait_until(
            &pool,
            "the gated job to poll its gate",
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname = current_database()              AND query LIKE 'SELECT EXISTS (SELECT 1 FROM gate)%')",
        )
        .await;

        stop.send(true).unwrap();
        wait_until(
            &pool,
            "the held effect to be given back",
            "SELECT EXISTS (SELECT 1 FROM jobs WHERE kind = 'test.effect' AND state = 'ready' \
             AND attempts = 0 AND lease_token IS NULL)",
        )
        .await;
        assert!(!handle.is_finished(), "the gated job still runs");
        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        tokio::time::timeout(WAIT_LIMIT, handle)
            .await
            .expect("the worker stops once its running job ends")
            .unwrap();

        // The gated job finished; the effect was never started.
        assert_eq!(effect_count(&pool).await, 1);
        let left: (String, String, i32) = sqlx::query_as("SELECT kind, state, attempts FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, ("test.effect".to_owned(), "ready".to_owned(), 0));
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

    /// Starts the listener and `slots` general slots on a shared state the test can read.
    fn start_slots(
        pool: &PgPool,
        config: WorkerConfig,
        slots: usize,
    ) -> (Arc<Shared>, watch::Sender<bool>, JoinSet<()>) {
        let shared = Arc::new(Shared::new(
            pool.clone(),
            pool.clone(),
            pool.clone(),
            registry(),
            config,
        ));
        let (stop, stopped) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tasks.spawn(listen(Arc::clone(&shared), stopped.clone()));
        for slot in 0..slots {
            tasks.spawn(work(
                Arc::clone(&shared),
                slot,
                Lane::General,
                stopped.clone(),
            ));
        }
        (shared, stop, tasks)
    }

    /// One notification wakes one idle slot, not all of them: with eight idle slots, one
    /// enqueued job costs one claim that finds it and one more by the slot that ran it, where
    /// waking every slot cost eight claims that found nothing.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_notification_wakes_one_idle_slot(pool: PgPool) {
        migrated(&pool).await;
        let mut config = test_config(8);
        // Only the notification can wake a slot in time.
        config.poll_interval = Duration::from_secs(3600);
        let (shared, stop, mut tasks) = start_slots(&pool, config, 8);
        wait_until(&pool, "the listener to subscribe", LISTENING).await;
        eventually("every slot to have claimed once", || async {
            shared.claims.load(std::sync::atomic::Ordering::Relaxed) >= 8
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let before = shared.claims.load(std::sync::atomic::Ordering::Relaxed);

        let mut job = new_job("test.effect");
        job.payload = json!({"index": 1});
        enqueue(&pool, &job).await.unwrap();
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        let claims = shared.claims.load(std::sync::atomic::Ordering::Relaxed) - before;
        assert!(claims <= 3, "{claims} claims for one notification");
        stop.send(true).unwrap();
        while tasks.join_next().await.is_some() {}
    }

    /// A burst that arrives under one notification is still shared: a slot whose claim came back
    /// full wakes another before it runs.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_burst_under_one_notification_reaches_more_than_one_slot(pool: PgPool) {
        migrated(&pool).await;
        sqlx::query("CREATE TABLE gate (open boolean)")
            .execute(&pool)
            .await
            .unwrap();
        let mut config = test_config(4);
        config.poll_interval = Duration::from_secs(3600);
        let (_shared, stop, mut tasks) = start_slots(&pool, config, 4);
        wait_until(&pool, "the listener to subscribe", LISTENING).await;

        // More than one claim's batch, in one statement: one notification for the kind.
        let jobs: Vec<NewJob> = (0..i64::from(CLAIM_BATCH) + 8)
            .map(|index| {
                let mut job = new_job("test.gated");
                job.payload = json!({ "index": index });
                job
            })
            .collect();
        zirv_flow_store::enqueue_many(&pool, &jobs).await.unwrap();

        eventually("a second slot to claim", || async {
            let slots: i64 = sqlx::query_scalar("SELECT count(DISTINCT locked_by) FROM jobs")
                .fetch_one(&pool)
                .await
                .unwrap();
            slots >= 2
        })
        .await;
        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        wait_for(&pool, |counts| counts == QueueCounts::default()).await;
        stop.send(true).unwrap();
        while tasks.join_next().await.is_some() {}
    }

    /// A config whose `ingest.drain` runs to completion on an empty inbox.
    fn draining_config(concurrency: usize, drain_slots: usize) -> WorkerConfig {
        let mut config = test_config(concurrency);
        config.drain_slots = drain_slots;
        config.redaction_key = Some(Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction),
        ));
        config
    }

    /// Enqueues an `ingest.drain` job for a new workspace with an empty inbox.
    async fn enqueue_drain_job(pool: &PgPool) {
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let workspace = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let trace = TraceContext::root(Cause::api("test-request"));
        assert!(
            zirv_flow_store::enqueue_drain_shard(
                pool,
                workspace,
                zirv_flow_store::Shard::ALL,
                &trace
            )
            .await
            .unwrap()
        );
    }

    async fn drain_jobs(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'ingest.drain'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Gives each of `slots` running general slots a `test.gated` job that waits on its shut gate.
    /// The jobs are enqueued one at a time, so no slot claims two.
    async fn busy_on_gated_jobs(pool: &PgPool, slots: usize) {
        sqlx::query("CREATE TABLE gate (open boolean)")
            .execute(pool)
            .await
            .unwrap();
        for index in 0..slots {
            let mut job = new_job("test.gated");
            job.payload = json!({ "index": index });
            enqueue(pool, &job).await.unwrap();
            let running = i64::try_from(index + 1).unwrap();
            wait_for(pool, |counts| counts.running == running).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_drain_slot_runs_a_drain_while_every_general_slot_is_busy_on_another_kind(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let (stop, handle) = start(&pool, draining_config(2, 1));
        busy_on_gated_jobs(&pool, 2).await;

        enqueue_drain_job(&pool).await;

        eventually("the drain to be claimed and run", || async {
            drain_jobs(&pool).await == 0
        })
        .await;
        let counts = queue_counts(&pool).await.unwrap();
        assert_eq!(counts.running, 2, "the general slots still hold their jobs");
        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        stop_and_join(stop, handle).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_drain_slots_a_drain_waits_behind_busy_general_slots(pool: PgPool) {
        migrated(&pool).await;
        let (stop, handle) = start(&pool, draining_config(2, 0));
        busy_on_gated_jobs(&pool, 2).await;

        enqueue_drain_job(&pool).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(drain_jobs(&pool).await, 1, "the drain is still queued");
        sqlx::query("INSERT INTO gate VALUES (true)")
            .execute(&pool)
            .await
            .unwrap();
        eventually("the general slots to claim the freed drain", || async {
            drain_jobs(&pool).await == 0
        })
        .await;
        stop_and_join(stop, handle).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn general_slots_leave_the_drain_to_the_drain_slots(pool: PgPool) {
        migrated(&pool).await;
        let given = pool.clone();
        let shared = Arc::new(Shared::new(
            pool.clone(),
            pool.clone(),
            given,
            registry(),
            draining_config(2, 1),
        ));
        assert!(!shared.kinds.contains(&DRAIN_JOB_KIND));
        assert_eq!(Lane::Drain.kinds(&shared), [DRAIN_JOB_KIND]);
        // Only general slots run: an effect completes, the drain beside it is never claimed.
        let (stop, stopped) = watch::channel(false);
        let slot = tokio::spawn(work(Arc::clone(&shared), 0, Lane::General, stopped));
        enqueue_drain_job(&pool).await;
        let mut effect = new_job("test.effect");
        effect.payload = json!({"index": 1});
        enqueue(&pool, &effect).await.unwrap();

        eventually("the effect to run", || async {
            effect_count(&pool).await == 1
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(drain_jobs(&pool).await, 1, "the drain was not claimed");
        stop.send(true).unwrap();
        slot.await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_workers_with_drain_slots_drain_different_shards_of_one_workspace_in_order(
        pool: PgPool,
    ) {
        use zirv_flow_store::{IngestItem, IngestPayload, append_inbox};

        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            migrated(&pool).await;
            let name = WorkspaceName::parse("ours").unwrap();
            let slug = Slug::parse("ours").unwrap();
            let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
                .await
                .unwrap()
                .id;
            let mut items = Vec::new();
            for round in ["free", "basic", "pro"] {
                for n in 0..300 {
                    items.push(IngestItem {
                        identifiers: vec![("external:user_id".into(), format!("u{n}"))],
                        timestamp: None,
                        message_id: Some(format!("{round}-{n}")),
                        payload: IngestPayload::Identify {
                            traits: json!({ "plan": round }).as_object().unwrap().clone(),
                        },
                    });
                }
            }
            let trace = TraceContext::root(Cause::api("test-request"));
            append_inbox(&pool, ws, uuid::Uuid::now_v7(), &trace, &items)
                .await
                .unwrap();
            // Slots are named by process and slot number, so the two workers' drain slots
            // (1 and 2 against 3 and 4) tell their claims apart.
            let mut first = draining_config(1, 2);
            let mut second = draining_config(3, 2);
            first.ingest_drain_batch_size = 5;
            second.ingest_drain_batch_size = 5;
            let (stop_first, first) = start(&pool, first);
            let (stop_second, second) = start(&pool, second);

            let pid = std::process::id();
            let mut owners = std::collections::BTreeSet::new();
            let mut together = 0;
            loop {
                let running: Vec<String> = sqlx::query_scalar(
                    "SELECT locked_by FROM jobs WHERE kind = 'ingest.drain' AND state = 'running'",
                )
                .fetch_all(&pool)
                .await
                .unwrap();
                together = together.max(running.len());
                owners.extend(running);
                let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_inbox")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                if left == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }

            let by = |slots: [usize; 2]| {
                slots
                    .iter()
                    .any(|slot| owners.contains(&format!("{pid}:{slot}")))
            };
            assert!(
                by([1, 2]) && by([3, 4]),
                "both workers ran shards: {owners:?}"
            );
            assert!(together >= 2, "shards of one workspace ran at once");
            let plans: Vec<Option<String>> = sqlx::query_scalar(
                "SELECT p.traits->>'plan' FROM people p JOIN identities i ON i.person_id = p.id",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(plans.len(), 300);
            assert!(
                plans.iter().all(|plan| plan.as_deref() == Some("pro")),
                "every key ended on its last value"
            );
            stop_and_join(stop_first, first).await;
            stop_and_join(stop_second, second).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_completed_shard_job_is_followed_by_the_next_when_a_row_landed_behind_it(
        pool: PgPool,
    ) {
        use zirv_flow_store::{IngestItem, IngestPayload, append_inbox};

        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            migrated(&pool).await;
            let name = WorkspaceName::parse("ours").unwrap();
            let slug = Slug::parse("ours").unwrap();
            let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
                .await
                .unwrap()
                .id;
            let item = IngestItem {
                identifiers: vec![("external:user_id".into(), "u1".into())],
                timestamp: None,
                message_id: Some("first".into()),
                payload: IngestPayload::Identify {
                    traits: json!({ "plan": "free" }).as_object().unwrap().clone(),
                },
            };
            let trace = TraceContext::root(Cause::api("test-request"));
            append_inbox(&pool, ws, uuid::Uuid::now_v7(), &trace, &[item])
                .await
                .unwrap();
            // The run's first batch deletes its row, and in that transaction a row of the same
            // bucket lands behind it, once: the run ends with the shard no longer empty and the
            // key held, as when an append races the last empty check.
            for statement in [
                "CREATE TABLE once (x int)",
                "CREATE FUNCTION again() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
                   IF NOT EXISTS (SELECT 1 FROM once) THEN \
                     INSERT INTO once VALUES (1); \
                     INSERT INTO ingest_inbox (workspace_id, api_key_id, kind, item, message_id, \
                                               trace_id, affinity) \
                     VALUES (OLD.workspace_id, OLD.api_key_id, OLD.kind, OLD.item, 'behind', \
                             OLD.trace_id, OLD.affinity); \
                   END IF; RETURN OLD; END $$",
                "CREATE TRIGGER again AFTER DELETE ON ingest_inbox \
                 FOR EACH ROW EXECUTE FUNCTION again()",
            ] {
                sqlx::query(statement).execute(&pool).await.unwrap();
            }
            let (stop, handle) = start(&pool, draining_config(1, 1));

            eventually(
                "the row behind the run to be drained by the next job",
                || async {
                    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_inbox")
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                    let once: i64 = sqlx::query_scalar("SELECT count(*) FROM once")
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                    once == 1 && left == 0 && drain_jobs(&pool).await == 0
                },
            )
            .await;
            stop_and_join(stop, handle).await;
        })
        .await;
    }

    #[test]
    fn drain_slots_default_to_one_and_a_drain_lane_claims_one_job_at_a_time() {
        assert_eq!(WorkerConfig::new(4).drain_slots, 1);
        assert_eq!(
            effective_drain_slots(&Registry::built_in(), &WorkerConfig::new(4)),
            1
        );
        assert_eq!(Lane::Drain.batch(), 1);
        assert_eq!(Lane::General.batch(), CLAIM_BATCH);
    }

    /// A job a slot gives back keeps its `run_at`, below the floor the claim that took it moved
    /// to, so giving it back expires the floors: the lane's next claim walks from the start and
    /// takes it again, oldest first.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_given_back_below_the_floor_is_claimed_by_the_next_claim(pool: PgPool) {
        migrated(&pool).await;
        let shared = Shared::new(
            pool.clone(),
            pool.clone(),
            pool.clone(),
            Registry::built_in(),
            test_config(1),
        );
        let mut ids = Vec::new();
        for age in [10, 5] {
            let mut new = new_job("given.back");
            new.run_at = Some(time::OffsetDateTime::now_utc() - time::Duration::seconds(age));
            enqueue(&pool, &new).await.unwrap();
            ids.push(
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM jobs ORDER BY created_at DESC, run_at DESC LIMIT 1",
                )
                .fetch_one(&pool)
                .await
                .unwrap(),
            );
        }
        let floors = Lane::General.floors(&shared);
        let claim = |n| {
            claim_jobs_from(
                &pool,
                "w",
                &["given.back"],
                Duration::from_secs(30),
                None,
                n,
                floors,
            )
        };
        let mut claimed = claim(1).await.unwrap();
        assert_eq!(claimed[0].id, ids[0]);
        claimed.extend(claim(1).await.unwrap());
        assert_eq!(claimed[1].id, ids[1]);

        release(&shared, vec![claimed.remove(0)]).await;

        let again = claim(1).await.unwrap();
        assert_eq!(
            again.iter().map(|job| job.id).collect::<Vec<_>>(),
            [ids[0]],
            "the job given back is claimed again"
        );
    }
}
