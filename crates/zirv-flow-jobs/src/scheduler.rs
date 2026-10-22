//! The scheduler leader: one instance at a time, elected by an advisory lock, enqueues due
//! schedules, reaps expired leases and exports queue metrics.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{info, warn};
use zirv_flow_core::schedule::latest_slot;
use zirv_flow_core::{Cause, CauseKind, TraceContext, WorkspaceId};
use zirv_flow_store::{
    Leadership, NewJob, PgPool, QueueCounts, Reaped, Schedule, StoreError, advance_schedule, begin,
    database_now, delete_expired_import_data, destination_report, dirty_backlog, dirty_heads,
    disable_schedule, due_schedules, due_sending_domains, enqueue, enqueue_domain_verify,
    enqueue_drain, enqueue_reevaluate, ensure_decision_partitions, ensure_event_partitions,
    expire_contact_idempotency, expire_exports, fail_orphaned_runs, inbox_heads,
    prune_debugger_feed, queue_depths, reap_expired, reconcile_flow_schedules,
    reconcile_segment_schedules, stream_freshness, try_lead,
};

use crate::metrics::{
    DestinationGauges, FreshnessGauges, QueueGauges, RunOutcome, record_sync_run, set_dirty_age,
    set_dirty_backlog, set_inbox_age,
};

/// Schedules handled per tick; more than this carry over to the next tick.
const DUE_BATCH: i64 = 100;

/// The longest between checks that the decision log and the event store have partitions for
/// this and next month.
const PARTITION_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// The longest between deletes of the data imports keep past their end (architecture 16.3).
const IMPORT_CHECK_INTERVAL: Duration = Duration::from_secs(3600);
/// The longest between prunes of the debugger feed.
const FEED_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
/// How often runs whose job is gone are failed.
const SYNC_RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
/// How often the stream freshness gauges are refreshed. Thresholds are hours, so half a minute of
/// lag is immaterial.
const FRESHNESS_INTERVAL: Duration = Duration::from_secs(30);
/// How often the destination gauges and counters are refreshed.
const DESTINATION_INTERVAL: Duration = Duration::from_secs(15);
/// How often the leader looks for sending domains that are due a DNS check.
const SENDING_DOMAIN_SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Domains queued per scan; more than this carry over to the next one.
const SENDING_DOMAIN_BATCH: i64 = 100;
/// How long after a check was queued for a domain the scan leaves it alone, whether or not the
/// check finished: a failing check (a DNS outage) is not re-queued, and a batch not refilled with
/// the same domains, until this has passed.
const SENDING_DOMAIN_RETRY_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// How old a sending domain last DNS check may be before the leader queues another: 6 hours.
pub const DEFAULT_SENDING_DOMAIN_RECHECK: Duration = Duration::from_secs(6 * 3600);

/// How long an inbox row waits before the leader enqueues a drain for its workspace.
const STALE_INBOX: Duration = Duration::from_secs(2);

/// How long a dirty mark waits before the leader enqueues a re-evaluation for its workspace.
const STALE_DIRTY: Duration = Duration::from_secs(2);

/// How often the scheduler works.
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Between leader ticks.
    pub tick_interval: Duration,
    /// Between attempts of a non-leader to take over.
    pub standby_interval: Duration,
    /// How long a sending domain may go without a DNS check before the leader queues one.
    pub sending_domain_recheck: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(1),
            standby_interval: Duration::from_secs(5),
            sending_domain_recheck: DEFAULT_SENDING_DOMAIN_RECHECK,
        }
    }
}

/// What one pass of the leader did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LeaderReport {
    /// Schedule slots enqueued.
    pub fired: u64,
    /// Slots of far-behind schedules that were skipped.
    pub skipped: u64,
    pub reaped: Reaped,
    /// Ingest drains the safety net enqueued for inboxes that had waited too long.
    pub drains: u64,
    /// Re-evaluations the safety net enqueued for dirty marks that had waited too long.
    pub reevaluations: u64,
    pub counts: QueueCounts,
}

/// The result of one scheduler tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tick {
    /// Another instance leads.
    Standby,
    Led(LeaderReport),
}

/// One scheduler instance.
pub struct Scheduler {
    pool: PgPool,
    config: SchedulerConfig,
    leadership: Option<Leadership>,
    gauges: Mutex<QueueGauges>,
    /// When the partitions were last checked; none until this instance leads.
    partitions_checked: Option<Instant>,
    /// When the import retention last ran; none until this instance leads.
    imports_checked: Option<Instant>,
    /// When the debugger feed was last pruned; none until this instance leads.
    feed_pruned: Option<Instant>,
    /// When the sync runs were last reconciled with their jobs; none until this instance leads.
    sync_reconciled: Option<Instant>,
    /// The stream freshness gauges.
    freshness: FreshnessGauges,
    /// When the freshness gauges were last refreshed; none until this instance leads.
    freshness_checked: Option<Instant>,
    /// The destination queue gauges and attempt counters.
    destinations: DestinationGauges,
    /// When the destination gauges were last refreshed; none until this instance leads.
    destinations_checked: Option<Instant>,
    /// When the sending domains were last scanned for due checks; none until this instance leads.
    domains_scanned: Option<Instant>,
}

impl Scheduler {
    pub fn new(pool: PgPool, config: SchedulerConfig) -> Self {
        Self {
            pool,
            config,
            leadership: None,
            gauges: Mutex::new(QueueGauges::default()),
            partitions_checked: None,
            imports_checked: None,
            feed_pruned: None,
            sync_reconciled: None,
            freshness: FreshnessGauges::default(),
            freshness_checked: None,
            destinations: DestinationGauges::default(),
            destinations_checked: None,
            domains_scanned: None,
        }
    }

    /// Runs until `stop` becomes true, then gives the lock up.
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) {
        while !*stop.borrow() {
            let wait = match self.tick().await {
                Ok(Tick::Led(_)) => self.config.tick_interval,
                Ok(Tick::Standby) => self.config.standby_interval,
                Err(error) => {
                    warn!("scheduler tick failed: {error}");
                    self.config.tick_interval
                }
            };
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                _ = stop.changed() => {}
            }
        }
        if let Some(leadership) = self.leadership.take() {
            leadership.release().await;
        }
        self.gauges
            .get_mut()
            .expect("the gauge lock is never held across a panic")
            .zero_all();
        self.freshness.zero_all();
        self.destinations.zero_all();
    }

    /// One pass: take or keep the lock, and if this instance leads, do the leader's work
    /// against the database's clock.
    pub async fn tick(&mut self) -> Result<Tick, StoreError> {
        if !self.lead().await? {
            return Ok(Tick::Standby);
        }
        let now = database_now(&self.pool).await?;
        self.keep_partitions(now).await;
        self.keep_imports().await;
        self.keep_debugger_feed().await;
        self.keep_sync_runs().await;
        self.keep_freshness(now).await;
        self.keep_destinations().await;
        self.keep_sending_domains().await;
        Ok(Tick::Led(self.work_at(now).await?))
    }

    /// Like [`tick`](Self::tick) with the clock given, for tests that walk through slots
    /// without waiting for them.
    #[cfg(test)]
    pub(crate) async fn tick_at(&mut self, now: OffsetDateTime) -> Result<Tick, StoreError> {
        if !self.lead().await? {
            return Ok(Tick::Standby);
        }
        self.keep_partitions(now).await;
        self.keep_imports().await;
        self.keep_debugger_feed().await;
        self.keep_sync_runs().await;
        self.keep_freshness(now).await;
        self.keep_destinations().await;
        self.keep_sending_domains().await;
        Ok(Tick::Led(self.work_at(now).await?))
    }

    /// The hourly upkeep: makes sure the decision log and the event store have partitions for
    /// this and next month, expires the person exports that are past their 24 hours and forgets
    /// contact idempotency keys older than that. Runs
    /// on gaining leadership, then at most hourly. A failure is logged and retried at the next
    /// check.
    async fn keep_partitions(&mut self, now: OffsetDateTime) {
        let due = self
            .partitions_checked
            .is_none_or(|checked| checked.elapsed() >= PARTITION_CHECK_INTERVAL);
        if !due {
            return;
        }
        self.partitions_checked = Some(Instant::now());
        if let Err(error) = ensure_decision_partitions(&self.pool, now.date()).await {
            warn!("could not create the decision-log partitions: {error}");
        }
        if let Err(error) = ensure_event_partitions(&self.pool, now.date()).await {
            warn!("could not create the event partitions: {error}");
        }
        // Segments saved before schedules existed get their schedule and a first refresh.
        match reconcile_segments(&self.pool).await {
            Ok(_) => {}
            Err(error) => warn!("could not reconcile the segment schedules: {error}"),
        }
        // Published flows with a schedule trigger get the schedule they lack.
        match reconcile_flows(&self.pool).await {
            Ok(_) => {}
            Err(error) => warn!("could not reconcile the flow schedules: {error}"),
        }
        // Contact idempotency keys are remembered for 24 hours.
        if let Err(error) = expire_contact_idempotency(&self.pool).await {
            warn!("could not expire the contact idempotency keys: {error}");
        }
        // Person exports expire 24 hours after they are ready; their content goes with them.
        if let Err(error) = expire_exports(&self.pool).await {
            warn!("could not expire the person exports: {error}");
        }
    }

    /// Deletes the files and rejects of imports that are past their retention: on gaining
    /// leadership, then at most hourly. A failure is logged and retried at the next check.
    async fn keep_imports(&mut self) {
        let due = self
            .imports_checked
            .is_none_or(|checked| checked.elapsed() >= IMPORT_CHECK_INTERVAL);
        if !due {
            return;
        }
        self.imports_checked = Some(Instant::now());
        match delete_expired_import_data(&self.pool).await {
            Ok(report) if report.chunks + report.rejects > 0 => {
                info!(
                    chunks = report.chunks,
                    rejects = report.rejects,
                    "deleted import data past its retention"
                );
            }
            Ok(_) => {}
            Err(error) => warn!("could not delete expired import data: {error}"),
        }
    }

    /// Deletes debugger feed rows past their 15 minutes: on gaining leadership, then at most
    /// every minute. A failure is logged and retried at the next check.
    async fn keep_debugger_feed(&mut self) {
        let due = self
            .feed_pruned
            .is_none_or(|pruned| pruned.elapsed() >= FEED_PRUNE_INTERVAL);
        if !due {
            return;
        }
        self.feed_pruned = Some(Instant::now());
        if let Err(error) = prune_debugger_feed(&self.pool).await {
            warn!("could not prune the debugger feed: {error}");
        }
    }

    /// Fails the sync runs whose job is dead or gone: on gaining leadership, then at most every
    /// minute. The generic job system does not know about runs, so a job it buried or deleted
    /// would leave its run active, and its stream blocked, for good. A failure is logged and
    /// retried at the next check.
    async fn keep_sync_runs(&mut self) {
        let due = self
            .sync_reconciled
            .is_none_or(|checked| checked.elapsed() >= SYNC_RECONCILE_INTERVAL);
        if !due {
            return;
        }
        self.sync_reconciled = Some(Instant::now());
        match fail_orphaned_runs(&self.pool).await {
            Ok(runs) => {
                for _ in &runs {
                    record_sync_run(RunOutcome::Failed, 0.0);
                }
                if !runs.is_empty() {
                    warn!(
                        "failed {} sync run(s) whose job is dead or gone",
                        runs.len()
                    );
                }
            }
            Err(error) => warn!("could not reconcile the sync runs: {error}"),
        }
    }

    /// Refreshes the stream freshness gauges from the state `stream_freshness` computes at
    /// `now`: on gaining leadership, then at most every [`FRESHNESS_INTERVAL`]. The state is
    /// derived on every call and never stored, so there is nothing to mark. A failure is logged
    /// and retried at the next check.
    async fn keep_freshness(&mut self, now: OffsetDateTime) {
        let due = self
            .freshness_checked
            .is_none_or(|checked| checked.elapsed() >= FRESHNESS_INTERVAL);
        if !due {
            return;
        }
        self.freshness_checked = Some(Instant::now());
        match stream_freshness(&self.pool, None, None, now).await {
            Ok(report) => self.freshness.update(&report),
            Err(error) => warn!("could not refresh the stream freshness gauges: {error}"),
        }
    }

    /// Refreshes the destination gauges and counters: on gaining leadership, then at most every
    /// [`DESTINATION_INTERVAL`]. A failure is logged and retried at the next check.
    async fn keep_destinations(&mut self) {
        let due = self
            .destinations_checked
            .is_none_or(|checked| checked.elapsed() >= DESTINATION_INTERVAL);
        if !due {
            return;
        }
        self.destinations_checked = Some(Instant::now());
        match destination_report(&self.pool).await {
            Ok(report) => self.destinations.update(&report),
            Err(error) => warn!("could not refresh the destination gauges: {error}"),
        }
    }

    /// Queues a DNS check for every sending domain whose last check is older than
    /// `sending_domain_recheck` (or never happened): on gaining leadership, then at most every
    /// [`SENDING_DOMAIN_SCAN_INTERVAL`]. A domain with a check already queued or running is left
    /// alone by the job's unique key. A failure is logged and retried at the next scan.
    async fn keep_sending_domains(&mut self) {
        let due = self
            .domains_scanned
            .is_none_or(|scanned| scanned.elapsed() >= SENDING_DOMAIN_SCAN_INTERVAL);
        if !due {
            return;
        }
        self.domains_scanned = Some(Instant::now());
        let stale = match due_sending_domains(
            &self.pool,
            self.config.sending_domain_recheck.as_secs_f64(),
            SENDING_DOMAIN_RETRY_BACKOFF.as_secs_f64(),
            SENDING_DOMAIN_BATCH,
        )
        .await
        {
            Ok(stale) => stale,
            Err(error) => {
                warn!("could not look for sending domains due a check: {error}");
                return;
            }
        };
        let mut connection = match self.pool.acquire().await {
            Ok(connection) => connection,
            Err(error) => {
                warn!("could not queue sending domain checks: {error}");
                return;
            }
        };
        for (workspace, domain) in stale {
            let trace = TraceContext::root(Cause::schedule(domain));
            if let Err(error) =
                enqueue_domain_verify(&mut connection, workspace, domain, trace).await
            {
                warn!("could not queue a sending domain check: {error}");
            }
        }
    }

    /// Whether this instance leads after the call. A lock connection that died has lost
    /// the lock, so a leader steps down before it works.
    async fn lead(&mut self) -> Result<bool, StoreError> {
        if let Some(leadership) = self.leadership.as_mut() {
            if leadership.is_held().await {
                return Ok(true);
            }
            warn!("the scheduler lost its leadership lock");
            self.leadership = None;
            self.partitions_checked = None;
            self.imports_checked = None;
            self.feed_pruned = None;
            self.sync_reconciled = None;
            self.freshness_checked = None;
            self.freshness.zero_all();
            self.destinations_checked = None;
            self.domains_scanned = None;
            self.destinations.zero_all();
            self.gauges
                .get_mut()
                .expect("the gauge lock is never held across a panic")
                .zero_all();
        }
        self.leadership = try_lead(&self.pool).await?;
        Ok(self.leadership.is_some())
    }

    /// The leader's work: due schedules, expired leases, queue gauges.
    pub(crate) async fn work_at(&self, now: OffsetDateTime) -> Result<LeaderReport, StoreError> {
        let (fired, skipped) = self.enqueue_due(now).await?;
        let reaped = reap_expired(&self.pool).await?;
        if reaped.requeued + reaped.dead + reaped.purged > 0 {
            warn!(
                "reaped {} job(s) with expired leases ({} dead, {} deleted for an erasure)",
                reaped.requeued + reaped.dead + reaped.purged,
                reaped.dead,
                reaped.purged
            );
        }
        let drains = self.wake_stale_inboxes().await?;
        let reevaluations = self.wake_stale_dirty().await?;
        let depths = queue_depths(&self.pool).await?;
        self.gauges
            .lock()
            .expect("the gauge lock is never held across a panic")
            .update(&depths);
        let mut counts = QueueCounts::default();
        for depth in &depths {
            match depth.state.as_str() {
                "ready" => counts.ready += depth.count,
                "running" => counts.running += depth.count,
                "dead" => counts.dead += depth.count,
                _ => {}
            }
        }
        Ok(LeaderReport {
            fired,
            skipped,
            reaped,
            drains,
            reevaluations,
            counts,
        })
    }

    /// The ingest safety net: enqueues `ingest.drain` for every workspace whose oldest
    /// inbox row has waited more than [`STALE_INBOX`]. The unique key makes it a no-op
    /// while a drain is ready or running. It covers an append that landed as a drain
    /// finished, and a drain that ran out of its time budget. Also publishes the age of
    /// the oldest row anywhere. Returns the drains enqueued.
    async fn wake_stale_inboxes(&self) -> Result<u64, StoreError> {
        let heads = inbox_heads(&self.pool).await?;
        let oldest = heads
            .iter()
            .map(|head| head.age_seconds)
            .fold(0.0_f64, f64::max);
        set_inbox_age(oldest);
        let mut enqueued = 0;
        for head in heads
            .iter()
            .filter(|head| head.age_seconds > STALE_INBOX.as_secs_f64())
        {
            let trace = TraceContext::root(Cause::from_stored(
                CauseKind::Api,
                "ingest.safety_net".to_owned(),
            ));
            if enqueue_drain(&self.pool, head.workspace_id, &trace).await? {
                enqueued += 1;
            }
        }
        Ok(enqueued)
    }

    /// The dirty-set safety net: enqueues `segment.reevaluate` for every workspace whose oldest
    /// mark is more than [`STALE_DIRTY`] older than the database clock. The unique key makes it a
    /// no-op while a re-evaluation is ready or running. It covers a mark that landed after the
    /// job's last claim, a request that rolled back and a run that ran out of its time budget.
    /// Also publishes the dirty backlog and the age of the oldest mark anywhere. Returns the
    /// re-evaluations enqueued.
    async fn wake_stale_dirty(&self) -> Result<u64, StoreError> {
        let heads = dirty_heads(&self.pool).await?;
        let oldest = heads
            .iter()
            .map(|head| head.age_seconds)
            .fold(0.0_f64, f64::max);
        set_dirty_age(oldest);
        set_dirty_backlog(dirty_backlog(&self.pool).await?);
        let mut enqueued = 0;
        for head in heads
            .iter()
            .filter(|head| head.age_seconds > STALE_DIRTY.as_secs_f64())
        {
            let trace = TraceContext::root(Cause::from_stored(
                CauseKind::Api,
                "segment.safety_net".to_owned(),
            ));
            if enqueue_reevaluate(&self.pool, head.workspace_id, &trace).await? {
                enqueued += 1;
            }
        }
        Ok(enqueued)
    }

    /// Enqueues the latest due slot of every due schedule; returns (fired, skipped).
    async fn enqueue_due(&self, now: OffsetDateTime) -> Result<(u64, u64), StoreError> {
        let mut fired = 0;
        let mut skipped = 0;
        for schedule in due_schedules(&self.pool, now, DUE_BATCH).await? {
            let Ok((slot, behind)) = latest_slot(
                &schedule.cron,
                &schedule.timezone,
                schedule.next_run_at,
                now,
            ) else {
                warn!("schedule {} cannot be evaluated; disabling it", schedule.id);
                disable_schedule(&self.pool, schedule.id).await?;
                continue;
            };
            if behind > 0 {
                warn!(
                    "schedule {} is behind: skipping {behind} missed slot(s)",
                    schedule.id
                );
            }
            if self.fire(&schedule, slot, now).await? {
                fired += 1;
                skipped += behind;
            }
        }
        Ok((fired, skipped))
    }

    /// Enqueues one slot and advances the schedule past it, in one transaction. The advance
    /// is a compare-and-set on the slot this instance read, so when two instances race (a
    /// failover overlap) exactly one wins, even if the slot's job has already completed
    /// and been deleted. False when another instance won.
    async fn fire(
        &self,
        schedule: &Schedule,
        slot: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let Ok(next) =
            zirv_flow_core::schedule::next_after(&schedule.cron, &schedule.timezone, slot.max(now))
        else {
            warn!("schedule {} has no later slot; disabling it", schedule.id);
            disable_schedule(&self.pool, schedule.id).await?;
            return Ok(false);
        };

        let trace = TraceContext::root(Cause::schedule(schedule.id));
        let mut job = NewJob::new(schedule.kind.clone(), trace);
        job.workspace_id = schedule.workspace_id.map(WorkspaceId::from_uuid);
        job.payload = schedule.payload.clone();
        job.unique_key = Some(slot_key(&schedule.id, slot));

        let mut tx = begin(&self.pool).await?;
        if !advance_schedule(&mut *tx, schedule.id, schedule.next_run_at, next).await? {
            return Ok(false);
        }
        enqueue(&mut *tx, &job).await?;
        zirv_flow_store::commit(tx).await?;
        Ok(true)
    }
}

/// Reconciles the segments saved before schedules existed, in one transaction.
async fn reconcile_segments(pool: &PgPool) -> Result<(u64, u64), StoreError> {
    let mut tx = begin(pool).await?;
    let done = reconcile_segment_schedules(&mut tx).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(done)
}

/// Gives the flows published before their schedule had a row that row, in one transaction.
async fn reconcile_flows(pool: &PgPool) -> Result<u64, StoreError> {
    let mut tx = begin(pool).await?;
    let created = reconcile_flow_schedules(&mut tx).await?;
    zirv_flow_store::commit(tx).await?;
    Ok(created)
}

/// The unique key of one slot of one schedule.
fn slot_key(schedule: &uuid::Uuid, slot: OffsetDateTime) -> String {
    format!("schedule:{schedule}:{}", slot.unix_timestamp())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, TraceContext};
    use zirv_flow_store::{NewSchedule, claim_job, complete_job, create_schedule, find_job};

    use super::*;

    const BASE: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);

    fn hourly(next_run_at: OffsetDateTime) -> NewSchedule {
        NewSchedule {
            workspace_id: None,
            kind: "test.tick".into(),
            cron: "0 * * * *".into(),
            timezone: "UTC".into(),
            next_run_at,
            payload: json!({"from": "schedule"}),
        }
    }

    fn scheduler(pool: &PgPool) -> Scheduler {
        Scheduler::new(pool.clone(), SchedulerConfig::default())
    }

    async fn migrated(pool: &PgPool) {
        zirv_flow_store::migrate(pool).await.unwrap();
        // The tests walk the clock far ahead and count the slots they created themselves.
        sqlx::query("DELETE FROM schedules WHERE kind IN ('events.retention', 'maintenance.ai_transcripts')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE fired (slot_key text PRIMARY KEY)")
            .execute(pool)
            .await
            .unwrap();
    }

    /// Runs every ready job to completion, recording its unique key in `fired`; a key
    /// recorded twice is a primary-key violation.
    async fn complete_all(pool: &PgPool) {
        while let Some(job) = claim_job(pool, "test", &["test.tick"], Duration::from_secs(30), None)
            .await
            .unwrap()
        {
            sqlx::query("INSERT INTO fired (slot_key) VALUES ($1)")
                .bind(job.unique_key.clone().unwrap())
                .execute(pool)
                .await
                .unwrap();
            assert!(complete_job(pool, job.id, job.lease_token).await.unwrap());
        }
    }

    async fn fired_keys(pool: &PgPool) -> BTreeSet<String> {
        sqlx::query_scalar("SELECT slot_key FROM fired")
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .collect()
    }

    fn led(tick: Tick) -> LeaderReport {
        match tick {
            Tick::Led(report) => report,
            Tick::Standby => panic!("expected to lead"),
        }
    }

    #[test]
    fn a_slot_key_names_the_schedule_and_the_instant() {
        let id = Uuid::nil();

        assert_eq!(
            slot_key(&id, datetime!(2030-01-01 00:00 UTC)),
            "schedule:00000000-0000-0000-0000-000000000000:1893456000"
        );
    }

    #[test]
    fn the_default_config_ticks_every_second_and_retries_every_five() {
        let config = SchedulerConfig::default();

        assert_eq!(config.tick_interval, Duration::from_secs(1));
        assert_eq!(config.standby_interval, Duration::from_secs(5));
    }

    #[test]
    fn sending_domains_are_rechecked_every_six_hours_by_default() {
        let config = SchedulerConfig::default();

        assert_eq!(config.sending_domain_recheck, Duration::from_secs(6 * 3600));
        assert_eq!(
            DEFAULT_SENDING_DOMAIN_RECHECK,
            config.sending_domain_recheck
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_queues_a_check_for_a_domain_never_checked_or_stale_but_not_a_fresh_one(
        pool: PgPool,
    ) {
        use zirv_flow_store::sending_fixtures::plant_domain;

        migrated(&pool).await;
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let never = plant_domain(&pool, ws, "never.example.com").await.unwrap();
        let stale = plant_domain(&pool, ws, "stale.example.com").await.unwrap();
        let fresh = plant_domain(&pool, ws, "fresh.example.com").await.unwrap();
        for (id, age) in [(stale, "7 hours"), (fresh, "5 hours")] {
            sqlx::query(
                "UPDATE sending_domains SET last_checked_at = now() - $2::interval WHERE id = $1",
            )
            .bind(id)
            .bind(age)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap();

        leader.tick_at(now).await.unwrap();

        let queued: Vec<String> = sqlx::query_scalar(
            "SELECT payload->>'domain_id' FROM jobs WHERE kind = 'sending_domain.verify' \
             ORDER BY payload->>'domain_id'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut expected = vec![never.to_string(), stale.to_string()];
        expected.sort();
        assert_eq!(queued, expected);

        // A second scan inside the interval queues nothing, and after it a queued check holds
        // its unique key, so nothing is queued twice.
        leader.tick_at(now).await.unwrap();
        leader.domains_scanned = None;
        leader.tick_at(now).await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'sending_domain.verify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }

    async fn domains_due(pool: &PgPool, count: usize) -> (zirv_flow_core::WorkspaceId, Vec<Uuid>) {
        let ws = zirv_flow_store::create_workspace(
            pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let mut ids = Vec::new();
        for n in 0..count {
            ids.push(
                zirv_flow_store::sending_fixtures::plant_domain(
                    pool,
                    ws,
                    &format!("d{n}.example.com"),
                )
                .await
                .unwrap(),
            );
        }
        // Spread the last checks so the order is known: the first domain is the stalest.
        for (n, id) in ids.iter().enumerate() {
            sqlx::query(
                "UPDATE sending_domains SET last_checked_at = now() - interval '2 days' + $2 * interval '1 second' WHERE id = $1",
            )
            .bind(id)
            .bind(n as f64)
            .execute(pool)
            .await
            .unwrap();
        }
        (ws, ids)
    }

    async fn queued_domains(pool: &PgPool) -> usize {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(DISTINCT payload->>'domain_id') FROM jobs \
             WHERE kind = 'sending_domain.verify'",
        )
        .fetch_one(pool)
        .await
        .unwrap() as usize
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn domains_already_queued_do_not_fill_the_batch_so_the_one_hundred_and_first_is_reached(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let (ws, ids) = domains_due(&pool, 101).await;
        let mut connection = pool.acquire().await.unwrap();
        for id in &ids[..100] {
            enqueue_domain_verify(
                &mut connection,
                ws,
                *id,
                TraceContext::root(Cause::schedule(*id)),
            )
            .await
            .unwrap();
        }
        // Only the queued-job exclusion is under test: forget the attempt stamps.
        sqlx::query("UPDATE sending_domains SET last_attempt_at = NULL")
            .execute(&pool)
            .await
            .unwrap();
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap();

        leader.tick_at(now).await.unwrap();

        assert_eq!(queued_domains(&pool).await, 101);
        let last: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE payload->>'domain_id' = $1 \
               AND kind = 'sending_domain.verify'",
        )
        .bind(ids[100].to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(last, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_domain_whose_check_failed_is_not_picked_again_within_the_back_off(pool: PgPool) {
        migrated(&pool).await;
        let (_, ids) = domains_due(&pool, 2).await;
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap();
        leader.tick_at(now).await.unwrap();
        assert_eq!(queued_domains(&pool).await, 2);

        // A DNS outage: both checks ran out of attempts and are dead; no check time was stamped.
        sqlx::query("UPDATE jobs SET state = 'dead' WHERE kind = 'sending_domain.verify'")
            .execute(&pool)
            .await
            .unwrap();
        leader.domains_scanned = None;
        leader.tick_at(now).await.unwrap();
        let jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'sending_domain.verify' AND state = 'ready'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(jobs, 0, "within the back-off nothing is queued again");

        // Past the back-off the stalest domain is due again.
        sqlx::query("UPDATE sending_domains SET last_attempt_at = now() - interval '31 minutes' WHERE id = $1")
            .bind(ids[0])
            .execute(&pool)
            .await
            .unwrap();
        leader.domains_scanned = None;
        leader.tick_at(now).await.unwrap();
        let ready: Vec<String> = sqlx::query_scalar(
            "SELECT payload->>'domain_id' FROM jobs \
             WHERE kind = 'sending_domain.verify' AND state = 'ready'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ready, [ids[0].to_string()]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shorter_recheck_interval_makes_a_fresh_domain_due(pool: PgPool) {
        use zirv_flow_store::sending_fixtures::plant_domain;

        migrated(&pool).await;
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let id = plant_domain(&pool, ws, "example.com").await.unwrap();
        sqlx::query("UPDATE sending_domains SET last_checked_at = now() - interval '2 hours'")
            .execute(&pool)
            .await
            .unwrap();
        let config = SchedulerConfig {
            sending_domain_recheck: Duration::from_secs(3600),
            ..SchedulerConfig::default()
        };
        let mut leader = Scheduler::new(pool.clone(), config);
        let now = database_now(&pool).await.unwrap();

        leader.tick_at(now).await.unwrap();

        let queued: Option<String> = sqlx::query_scalar(
            "SELECT payload->>'domain_id' FROM jobs WHERE kind = 'sending_domain.verify'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert_eq!(queued, Some(id.to_string()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_due_slot_is_enqueued_once_and_the_schedule_moves_on(pool: PgPool) {
        migrated(&pool).await;
        let id = create_schedule(&pool, &hourly(BASE)).await.unwrap();
        let mut leader = scheduler(&pool);

        let first = led(leader.tick_at(BASE).await.unwrap());
        let again = led(leader.tick_at(BASE).await.unwrap());

        assert_eq!((first.fired, first.skipped), (1, 0));
        assert_eq!(again.fired, 0);
        assert_eq!(again.counts.ready, 1);
        let job = claim_job(&pool, "w", &["test.tick"], Duration::from_secs(30), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.payload, json!({"from": "schedule"}));
        assert_eq!(
            job.unique_key.as_deref(),
            Some(slot_key(&id, BASE).as_str())
        );
        let next: OffsetDateTime = sqlx::query_scalar("SELECT next_run_at FROM schedules")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(next, BASE + time::Duration::hours(1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_far_behind_fires_only_its_latest_slot(pool: PgPool) {
        migrated(&pool).await;
        let id = create_schedule(&pool, &hourly(BASE)).await.unwrap();
        let mut leader = scheduler(&pool);

        let report = led(leader
            .tick_at(BASE + time::Duration::hours(5) + time::Duration::minutes(30))
            .await
            .unwrap());

        assert_eq!((report.fired, report.skipped), (1, 5));
        complete_all(&pool).await;
        assert_eq!(
            fired_keys(&pool).await,
            BTreeSet::from([slot_key(&id, BASE + time::Duration::hours(5))])
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_that_cannot_be_evaluated_is_disabled(pool: PgPool) {
        migrated(&pool).await;
        let mut broken = hourly(BASE);
        broken.cron = "not a cron".into();
        create_schedule(&pool, &broken).await.unwrap();
        let mut leader = scheduler(&pool);

        let report = led(leader.tick_at(BASE).await.unwrap());

        assert_eq!(report.fired, 0);
        let enabled: bool = sqlx::query_scalar("SELECT enabled FROM schedules")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!enabled);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_reaps_expired_leases_and_reports_the_queue(pool: PgPool) {
        migrated(&pool).await;
        zirv_flow_store::enqueue(
            &pool,
            &NewJob::new("test.tick", TraceContext::root(Cause::api("test-request"))),
        )
        .await
        .unwrap();
        let claimed = claim_job(
            &pool,
            "gone",
            &["test.tick"],
            Duration::from_millis(1),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut leader = scheduler(&pool);

        let report = loop {
            let report = led(leader.tick().await.unwrap());
            if report.reaped.requeued == 1 {
                break report;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        assert_eq!(report.counts.ready + report.counts.running, 1);
        let job = find_job(&pool, claimed.id).await.unwrap().unwrap();
        assert_eq!(job.state, "ready");
        assert_eq!(job.last_error.as_deref(), Some("lease expired"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_fired_slot_starts_its_own_trace_caused_by_the_schedule(pool: PgPool) {
        migrated(&pool).await;
        let id = create_schedule(&pool, &hourly(BASE)).await.unwrap();
        let mut leader = scheduler(&pool);

        leader.tick_at(BASE).await.unwrap();
        leader
            .tick_at(BASE + time::Duration::hours(1))
            .await
            .unwrap();

        let rows: Vec<(Uuid, String, String)> =
            sqlx::query_as("SELECT trace_id, cause_kind, cause_id FROM jobs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|(_, kind, cause)| kind == "schedule" && *cause == id.to_string())
        );
        assert_ne!(rows[0].0, rows[1].0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leaders_upkeep_expires_exports_past_their_time_and_keeps_the_others(pool: PgPool) {
        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "ours").await;
        let person = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        for (expires, label) in [("-1 minute", "past"), ("+1 hour", "future")] {
            sqlx::query(
                "INSERT INTO privacy_exports \
                    (id, workspace_id, person_id, status, ready_at, expires_at, content, \
                     size_bytes, trace_id, requested_by) \
                 VALUES (gen_random_uuid(), $1, $2, 'ready', now(), \
                         now() + $3::interval, '\\x1f8b'::bytea, 2, gen_random_uuid(), NULL)",
            )
            .bind(ws.as_uuid())
            .bind(person)
            .bind(expires)
            .execute(&pool)
            .await
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        }

        scheduler(&pool).tick().await.unwrap();

        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT status, content IS NULL FROM privacy_exports ORDER BY expires_at",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [("expired".to_owned(), true), ("ready".to_owned(), false)]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leaders_upkeep_forgets_contact_idempotency_keys_older_than_a_day(pool: PgPool) {
        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "ours").await;
        for (key, hours) in [("old", 25_i32), ("new", 1)] {
            sqlx::query(
                "INSERT INTO contact_idempotency \
                    (workspace_id, key, request_hash, status, created_at) \
                 VALUES ($1, $2, '\\x00'::bytea, 201, now() - make_interval(hours => $3))",
            )
            .bind(ws.as_uuid())
            .bind(key)
            .bind(hours)
            .execute(&pool)
            .await
            .unwrap();
        }

        scheduler(&pool).tick().await.unwrap();

        let left: Vec<String> = sqlx::query_scalar("SELECT key FROM contact_idempotency")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, ["new"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_creates_the_decision_log_partitions_for_this_month(pool: PgPool) {
        migrated(&pool).await;
        let now = database_now(&pool).await.unwrap();
        let months_ahead = 24;
        let index = now.year() * 12 + i32::from(u8::from(now.month())) - 1 + months_ahead;
        let (year, month) = (index / 12, index % 12 + 1);
        let base = datetime!(2000-01-01 00:00 UTC)
            .replace_year(year)
            .unwrap()
            .replace_month(time::Month::try_from(u8::try_from(month).unwrap()).unwrap())
            .unwrap();
        let names = [
            format!("decision_records_y{year:04}m{month:02}"),
            if month == 12 {
                format!("decision_records_y{:04}m01", year + 1)
            } else {
                format!("decision_records_y{year:04}m{:02}", month + 1)
            },
        ];
        let mut leader = scheduler(&pool);

        let before = partitions(&pool).await;
        leader.tick_at(base).await.unwrap();
        let after = partitions(&pool).await;

        for name in &names {
            assert!(!before.contains(name), "{name} existed before the tick");
            assert!(
                after.contains(name),
                "{name} missing after the tick: {after:?}"
            );
        }
        let events = partitions_of(&pool, "events").await;
        for name in names.map(|name| name.replace("decision_records", "events")) {
            assert!(events.contains(&name), "{name} missing: {events:?}");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_deletes_import_files_and_rejects_past_their_retention(pool: PgPool) {
        migrated(&pool).await;
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let import = Uuid::now_v7();
        for statement in [
            "INSERT INTO imports (id, workspace_id, name, stream, file_name, file_sha256, \
                size_bytes, delimiter, header, rows_total, trace_id, status, finished_at) \
             VALUES ($1, $2, 'old', 'old', 'old.csv', decode(repeat('00', 32), 'hex'), 1, ',', \
                '[\"email\"]', 0, gen_random_uuid(), 'succeeded', now() - interval '8 days')",
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) \
             VALUES ($1, 0, $2, 'email'::bytea)",
            "INSERT INTO import_rejects (import_id, row_number, workspace_id, phase, errors, \
                raw, created_at) \
             VALUES ($1, 2, $2, 'run', '[]', '[]', now() - interval '31 days')",
        ] {
            sqlx::query(statement)
                .bind(import)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut leader = scheduler(&pool);

        leader.tick_at(BASE).await.unwrap();

        for table in ["import_chunks", "import_rejects"] {
            let left: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, 0, "{table}");
        }
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM imports")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the import and its status stay");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_prunes_the_debugger_feed_of_rows_past_fifteen_minutes(pool: PgPool) {
        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "watched").await;
        for age in [20, 1] {
            sqlx::query(
                "INSERT INTO debugger_feed (workspace_id, at, source, outcome, trace_id)                  VALUES ($1, now() - make_interval(mins => $2), 'web', 'accepted',                          gen_random_uuid())",
            )
            .bind(ws.as_uuid())
            .bind(age)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut leader = scheduler(&pool);

        leader.tick().await.unwrap();

        let left: Vec<i64> = sqlx::query_scalar(
            "SELECT (extract(epoch FROM now() - at) / 60)::bigint FROM debugger_feed",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(left, [1]);
    }

    async fn partitions(pool: &PgPool) -> Vec<String> {
        partitions_of(pool, "decision_records").await
    }

    async fn partitions_of(pool: &PgPool, parent: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = $1::regclass",
        )
        .bind(parent)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn losing_the_leadership_lock_zeroes_the_published_queue_gauges(pool: PgPool) {
        migrated(&pool).await;
        zirv_flow_store::enqueue(
            &pool,
            &NewJob::new("test.tick", TraceContext::root(Cause::api("test-request"))),
        )
        .await
        .unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        assert!(matches!(leader.tick().await.unwrap(), Tick::Led(_)));
        let pair = "zirv_flow_jobs{kind=\"test.tick\",state=\"ready\"}";
        assert!(
            handle.render().contains(&format!("{pair} 1")),
            "{}",
            handle.render()
        );

        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_locks \
             WHERE locktype = 'advisory' AND granted AND pid <> pg_backend_pid() \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let _rival = loop {
            if let Some(rival) = try_lead(&pool).await.unwrap() {
                break rival;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        assert_eq!(leader.tick().await.unwrap(), Tick::Standby);
        assert!(
            handle.render().contains(&format!("{pair} 0")),
            "{}",
            handle.render()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_leaves_a_lease_expired_decision_for_a_workspace_job(pool: PgPool) {
        migrated(&pool).await;
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        let workspace = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let mut job = NewJob::new("test.tick", TraceContext::root(Cause::api("req")));
        job.workspace_id = Some(workspace);
        let trace = job.trace.clone();
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        claim_job(
            &pool,
            "gone",
            &["test.tick"],
            Duration::from_millis(1),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut leader = scheduler(&pool);

        loop {
            if led(leader.tick().await.unwrap()).reaped.requeued == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (records, _) = zirv_flow_store::list_trace(&pool, workspace, trace.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            (records[0].action.as_str(), records[0].outcome.as_str()),
            ("job.attempt", "lease_expired")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_instance_stands_by_while_the_first_leads(pool: PgPool) {
        migrated(&pool).await;
        let mut first = scheduler(&pool);
        let mut second = scheduler(&pool);

        assert!(matches!(first.tick().await.unwrap(), Tick::Led(_)));
        assert_eq!(second.tick().await.unwrap(), Tick::Standby);
        assert!(matches!(first.tick().await.unwrap(), Tick::Led(_)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_leaders_working_the_same_instant_fire_a_slot_once(pool: PgPool) {
        migrated(&pool).await;
        create_schedule(&pool, &hourly(BASE)).await.unwrap();
        let one = scheduler(&pool);
        let two = scheduler(&pool);

        // A failover overlap: both believe they lead and read the same due slot.
        let (a, b) = tokio::join!(one.work_at(BASE), two.work_at(BASE));

        assert_eq!(a.unwrap().fired + b.unwrap().fired, 1);
        complete_all(&pool).await;
        assert_eq!(fired_keys(&pool).await.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn failover_fires_no_slot_twice_even_for_slots_whose_jobs_already_completed(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let id = create_schedule(&pool, &hourly(BASE)).await.unwrap();
        let mut first = scheduler(&pool);
        let mut second = scheduler(&pool);
        let slots: u32 = 12;
        let at = |slot: u32| BASE + time::Duration::hours(i64::from(slot));

        for slot in 0..slots {
            if slot < 4 {
                // The first instance leads; the second only watches.
                assert!(matches!(
                    first.tick_at(at(slot)).await.unwrap(),
                    Tick::Led(_)
                ));
                assert_eq!(second.tick_at(at(slot)).await.unwrap(), Tick::Standby);
            } else {
                if slot == 4 {
                    // The leader dies mid-sequence: its lock connection goes away.
                    first.leadership.take();
                }
                // The follower takes over once the server notices the dead connection.
                let report = loop {
                    if let Tick::Led(report) = second.tick_at(at(slot)).await.unwrap() {
                        break report;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                assert_eq!(report.fired, 1);
            }
            // Every slot's job completes and is deleted before the next one is due.
            complete_all(&pool).await;
            // The dead instance comes back and replays the slot it last saw; the schedule
            // has moved on, so it fires nothing.
            if slot == 6 {
                let stale = scheduler(&pool);
                assert_eq!(stale.work_at(at(slot)).await.unwrap().fired, 0);
            }
        }

        let expected: BTreeSet<String> = (0..slots).map(|slot| slot_key(&id, at(slot))).collect();
        assert_eq!(fired_keys(&pool).await, expected);
        let remaining: Value = sqlx::query_scalar("SELECT to_jsonb(count(*)) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, json!(0));
    }

    async fn inbox_workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = zirv_flow_core::WorkspaceName::parse(slug).unwrap();
        let slug = zirv_flow_core::Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn put_in_inbox(pool: &PgPool, ws: WorkspaceId, age_seconds: i32) {
        sqlx::query(
            "INSERT INTO ingest_inbox \
                (workspace_id, api_key_id, kind, item, message_id, received_at, trace_id) \
             VALUES ($1, gen_random_uuid(), 'track', '{}', gen_random_uuid()::text, \
                     now() - make_interval(secs => $2), gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(f64::from(age_seconds))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn drain_jobs(pool: &PgPool) -> Vec<Option<String>> {
        sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'ingest.drain'")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_safety_net_enqueues_a_drain_for_a_stale_inbox_only_once(pool: PgPool) {
        migrated(&pool).await;
        let stale = inbox_workspace(&pool, "stale").await;
        let fresh = inbox_workspace(&pool, "fresh").await;
        put_in_inbox(&pool, stale, 5).await;
        put_in_inbox(&pool, stale, 0).await;
        put_in_inbox(&pool, fresh, 0).await;
        let mut scheduler = scheduler(&pool);

        let first = led(scheduler.tick_at(BASE).await.unwrap());
        let second = led(scheduler.tick_at(BASE).await.unwrap());

        assert_eq!((first.drains, second.drains), (1, 0));
        assert_eq!(
            drain_jobs(&pool).await,
            [Some(format!("ingest.drain:{}", stale.as_uuid()))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_safety_net_judges_a_workspace_by_its_first_row_and_ignores_an_empty_inbox(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "ours").await;
        let mut scheduler = scheduler(&pool);
        assert_eq!(led(scheduler.tick_at(BASE).await.unwrap()).drains, 0);

        put_in_inbox(&pool, ws, 1).await;
        let young = led(scheduler.tick_at(BASE).await.unwrap());
        sqlx::query("DELETE FROM ingest_inbox")
            .execute(&pool)
            .await
            .unwrap();
        put_in_inbox(&pool, ws, 600).await;
        put_in_inbox(&pool, ws, 0).await;
        let old = led(scheduler.tick_at(BASE).await.unwrap());

        assert_eq!((young.drains, old.drains), (0, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_publishes_the_age_of_the_oldest_inbox_row(pool: PgPool) {
        use metrics_exporter_prometheus::PrometheusBuilder;

        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "ours").await;
        put_in_inbox(&pool, ws, 40).await;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut scheduler = scheduler(&pool);
        let _guard = metrics::set_default_local_recorder(&recorder);

        scheduler.tick_at(BASE).await.unwrap();

        let output = handle.render();
        let age: f64 = output
            .lines()
            .find_map(|line| line.strip_prefix("zirv_flow_ingest_inbox_age_seconds "))
            .unwrap_or_else(|| panic!("no gauge in {output}"))
            .parse()
            .unwrap();
        assert!((39.0..120.0).contains(&age), "{age}");
    }

    async fn mark_dirty_aged(pool: &PgPool, ws: WorkspaceId, age_seconds: i32) {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO person_changes (workspace_id, person_id, classes, changed_at) \
             VALUES ($1, $2, 1, now() - make_interval(secs => $3))",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(f64::from(age_seconds))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn reevaluate_jobs(pool: &PgPool) -> Vec<Option<String>> {
        sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'segment.reevaluate'")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_dirty_safety_net_enqueues_for_a_stale_workspace_and_does_nothing_while_the_job_is_ready_or_running(
        pool: PgPool,
    ) {
        migrated(&pool).await;
        let stale = inbox_workspace(&pool, "stale").await;
        let fresh = inbox_workspace(&pool, "fresh").await;
        mark_dirty_aged(&pool, stale, 5).await;
        mark_dirty_aged(&pool, stale, 0).await;
        mark_dirty_aged(&pool, fresh, 0).await;
        let mut scheduler = scheduler(&pool);

        let first = led(scheduler.tick_at(BASE).await.unwrap());
        // The job is ready now.
        let while_ready = led(scheduler.tick_at(BASE).await.unwrap());
        zirv_flow_store::claim_job(
            &pool,
            "w",
            &["segment.reevaluate"],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        // And now it is running.
        let while_running = led(scheduler.tick_at(BASE).await.unwrap());

        assert_eq!(
            (
                first.reevaluations,
                while_ready.reevaluations,
                while_running.reevaluations
            ),
            (1, 0, 0)
        );
        assert_eq!(
            reevaluate_jobs(&pool).await,
            [Some(format!("segment.reevaluate:{}", stale.as_uuid()))]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_publishes_the_dirty_backlog_and_the_age_of_the_oldest_mark(pool: PgPool) {
        use metrics_exporter_prometheus::PrometheusBuilder;

        migrated(&pool).await;
        let ws = inbox_workspace(&pool, "ours").await;
        mark_dirty_aged(&pool, ws, 40).await;
        mark_dirty_aged(&pool, ws, 1).await;
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut scheduler = scheduler(&pool);
        let _guard = metrics::set_default_local_recorder(&recorder);

        scheduler.tick_at(BASE).await.unwrap();

        let output = handle.render();
        let gauge = |name: &str| -> f64 {
            output
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name} ")))
                .unwrap_or_else(|| panic!("no {name} in {output}"))
                .parse()
                .unwrap()
        };
        assert_eq!(gauge("zirv_flow_segment_dirty_backlog"), 2.0);
        assert!((39.0..120.0).contains(&gauge("zirv_flow_segment_dirty_age_seconds")));
    }

    /// A workspace with one source whose streams `fresh`, `stale`, `failing` and `off` are set
    /// up, as `(workspace, [(name, stream id)])`. `fresh` and `stale` carry a threshold.
    async fn freshness_streams(
        pool: &PgPool,
        slug: &str,
    ) -> (WorkspaceId, Uuid, Vec<(String, Uuid)>) {
        use zirv_flow_core::{Slug, WorkspaceName};
        use zirv_flow_store::{
            DiscoveredStream, NewSource, StreamOutcome, StreamSettings, configure_stream,
            create_source, create_workspace, record_stream_outcome, upsert_discovered_streams,
        };

        let ws = create_workspace(
            pool,
            &WorkspaceName::parse(slug).unwrap(),
            &Slug::parse(slug).unwrap(),
        )
        .await
        .unwrap()
        .id;
        let config = json!({});
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "crm",
                config: &config,
                credential_id: None,
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let descriptor = json!({});
        let names = ["fresh", "stale", "failing", "off"];
        let found: Vec<DiscoveredStream<'_>> = names
            .iter()
            .map(|name| DiscoveredStream {
                name,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                descriptor: &descriptor,
                suggested_mapping: None,
            })
            .collect();
        let mut tx = begin(pool).await.unwrap();
        upsert_discovered_streams(&mut tx, ws, source.id, &found)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let mut streams = Vec::new();
        for (name, enabled, threshold) in [
            ("fresh", true, Some(3600)),
            ("stale", true, Some(60)),
            ("failing", true, Some(60)),
            ("off", false, Some(60)),
        ] {
            let stream = configure_stream(
                &mut pool.acquire().await.unwrap(),
                ws,
                source.id,
                name,
                &StreamSettings {
                    reconcile_schedule: None,
                    sweep_limit: 0.5,
                    enabled,
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &[],
                    staleness_threshold_seconds: threshold,
                },
            )
            .await
            .unwrap()
            .unwrap();
            streams.push((name.to_owned(), stream.id));
        }
        let failing = streams[2].1;
        record_stream_outcome(
            &mut pool.acquire().await.unwrap(),
            ws,
            failing,
            StreamOutcome::Failed,
            BASE,
        )
        .await
        .unwrap();
        (ws, source.id, streams)
    }

    /// The value of the one sample of `metric` whose labels include every `(key, value)`.
    fn sample(output: &str, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
        output.lines().find_map(|line| {
            let rest = line.strip_prefix(metric)?.strip_prefix('{')?;
            let (inside, value) = rest.split_once("} ")?;
            labels
                .iter()
                .all(|(key, value)| inside.contains(&format!("{key}=\"{value}\"")))
                .then(|| value.parse().unwrap())
        })
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_publishes_the_state_of_every_enabled_stream_and_only_those(pool: PgPool) {
        migrated(&pool).await;
        let (ws, _, streams) = freshness_streams(&pool, "ours").await;
        // A stream of a disabled source is not reported either.
        let (_, disabled_source, disabled_streams) = freshness_streams(&pool, "theirs").await;
        sqlx::query("UPDATE sources SET enabled = false WHERE id = $1")
            .bind(disabled_source)
            .execute(&pool)
            .await
            .unwrap();
        let id = |name: &str| {
            streams
                .iter()
                .find(|(n, _)| n == name)
                .unwrap()
                .1
                .to_string()
        };
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap() + time::Duration::minutes(10);

        assert!(matches!(leader.tick_at(now).await.unwrap(), Tick::Led(_)));

        let output = handle.render();
        let state =
            |name: &str| sample(&output, "zirv_flow_stream_state", &[("stream", &id(name))]);
        assert_eq!(state("fresh"), Some(0.0), "{output}");
        assert_eq!(state("stale"), Some(1.0), "{output}");
        assert_eq!(state("failing"), Some(2.0), "{output}");
        assert_eq!(state("off"), None, "a disabled stream has no gauge");
        let age = sample(
            &output,
            "zirv_flow_stream_freshness_seconds",
            &[("stream", &id("stale"))],
        )
        .unwrap();
        assert!((599.0..700.0).contains(&age), "{age}");
        for (_, stream) in &disabled_streams {
            assert!(!output.contains(&stream.to_string()), "{output}");
        }
        let count = |state: &str| {
            sample(
                &output,
                "zirv_flow_streams",
                &[("workspace", &ws.as_uuid().to_string()), ("state", state)],
            )
        };
        assert_eq!(
            (count("fresh"), count("stale"), count("failing")),
            (Some(1.0), Some(1.0), Some(1.0)),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_freshness_labels_are_ids_only(pool: PgPool) {
        migrated(&pool).await;
        let (ws, _, streams) = freshness_streams(&pool, "ours").await;
        sqlx::query("UPDATE sources SET name = 'secret-crm-name', schedule = '0 * * * *'")
            .execute(&pool)
            .await
            .unwrap();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap();

        leader.tick_at(now).await.unwrap();

        let output = handle.render();
        let lines: Vec<&str> = output
            .lines()
            .filter(|line| line.starts_with("zirv_flow_stream") && !line.starts_with('#'))
            .collect();
        assert!(!lines.is_empty(), "{output}");
        for line in lines {
            let inside = line.split_once('{').unwrap().1.split_once('}').unwrap().0;
            let keys: BTreeSet<&str> = inside
                .split(',')
                .map(|pair| pair.split_once('=').unwrap().0)
                .collect();
            let expected: BTreeSet<&str> = if line.starts_with("zirv_flow_streams{") {
                ["workspace", "state"].into()
            } else {
                ["workspace", "stream"].into()
            };
            assert_eq!(keys, expected, "{line}");
            for quoted in inside.split('"').skip(1).step_by(2) {
                let known = quoted == ws.as_uuid().to_string()
                    || streams.iter().any(|(_, id)| quoted == id.to_string())
                    || ["fresh", "stale", "failing"].contains(&quoted);
                assert!(known, "{line}");
            }
        }
        assert!(!output.contains("secret-crm-name"));
        assert!(!output.contains("0 * * * *"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_publishes_the_destination_metrics_and_zeroes_an_archived_destination(
        pool: PgPool,
    ) {
        use zirv_flow_core::{Slug, WorkspaceName};
        use zirv_flow_store::{
            AttemptOutcome, NewDeliveryAttempt, NewDestination, StatusClass, SubjectKind,
            archive_destination, create_destination, create_workspace, record_delivery_attempt,
        };

        migrated(&pool).await;
        let ws = create_workspace(
            &pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let config = json!({});
        let destination = create_destination(
            &pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name: "secret-mail-name",
                config: &config,
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        for attempt in 1..=2 {
            record_delivery_attempt(
                &pool,
                ws,
                &NewDeliveryAttempt {
                    destination_id: destination.id,
                    subject_kind: SubjectKind::Message,
                    subject_id: Uuid::now_v7(),
                    attempt,
                    outcome: AttemptOutcome::Delivered,
                    status_class: StatusClass::Success,
                    duration_ms: 3,
                },
            )
            .await
            .unwrap();
        }
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap();

        leader.tick_at(now).await.unwrap();
        let first = handle.render();
        archive_destination(&pool, ws, destination.id)
            .await
            .unwrap();
        leader.destinations_checked = None;
        leader.tick_at(now).await.unwrap();
        let second = handle.render();

        let labels = format!(
            "workspace=\"{}\",destination=\"{}\"",
            ws.as_uuid(),
            destination.id
        );
        for output in [&first, &second] {
            assert!(!output.contains("secret-mail-name"), "{output}");
        }
        assert!(
            first.contains(&format!("zirv_flow_destination_queue_depth{{{labels}}} 0")),
            "{first}"
        );
        assert!(
            !first.contains("zirv_flow_destination_attempts_total"),
            "the leader publishes no destination counter: {first}"
        );
        // Archived: the gauges are zeroed, not left at their last value.
        assert!(
            second.contains(&format!(
                "zirv_flow_destination_queue_oldest_seconds{{{labels}}} 0"
            )),
            "{second}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_later_tick_follows_a_success_and_a_disable_but_not_before_the_interval(
        pool: PgPool,
    ) {
        use zirv_flow_store::{
            StreamOutcome, StreamSettings, configure_stream, record_stream_outcome,
        };

        migrated(&pool).await;
        let (ws, source, streams) = freshness_streams(&pool, "ours").await;
        let id = |name: &str| streams.iter().find(|(n, _)| n == name).unwrap().1;
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap() + time::Duration::minutes(10);
        leader.tick_at(now).await.unwrap();
        let state = |name: &str| {
            sample(
                &handle.render(),
                "zirv_flow_stream_state",
                &[("stream", &id(name).to_string())],
            )
        };
        assert_eq!((state("stale"), state("failing")), (Some(1.0), Some(2.0)));

        // A success and a disable land, but the next tick is inside the interval.
        for name in ["stale", "failing"] {
            record_stream_outcome(
                &mut pool.acquire().await.unwrap(),
                ws,
                id(name),
                StreamOutcome::Succeeded,
                now,
            )
            .await
            .unwrap();
        }
        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source,
            "fresh",
            &StreamSettings {
                reconcile_schedule: None,
                sweep_limit: 0.5,
                enabled: false,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: Some(3600),
            },
        )
        .await
        .unwrap();
        leader.tick_at(now).await.unwrap();
        assert_eq!((state("stale"), state("failing")), (Some(1.0), Some(2.0)));
        assert_eq!(state("fresh"), Some(0.0));

        // Once the interval has passed the gauges follow the database.
        leader.freshness_checked = None;
        leader.tick_at(now).await.unwrap();
        assert_eq!((state("stale"), state("failing")), (Some(0.0), Some(0.0)));
        assert_eq!(state("fresh"), Some(0.0), "a disabled stream is zeroed");
        let output = handle.render();
        let count = |name: &str| {
            sample(
                &output,
                "zirv_flow_streams",
                &[("workspace", &ws.as_uuid().to_string()), ("state", name)],
            )
        };
        assert_eq!(
            (count("fresh"), count("stale"), count("failing")),
            (Some(2.0), Some(0.0), Some(0.0)),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_standby_instance_publishes_no_freshness(pool: PgPool) {
        migrated(&pool).await;
        freshness_streams(&pool, "ours").await;
        let _leader = try_lead(&pool).await.unwrap().expect("the lock is free");
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut standby = scheduler(&pool);

        let tick = standby.tick_at(BASE).await.unwrap();

        assert_eq!(tick, Tick::Standby);
        assert!(!handle.render().contains("zirv_flow_stream"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn losing_the_leadership_lock_zeroes_the_published_freshness_gauges(pool: PgPool) {
        migrated(&pool).await;
        let (_, _, streams) = freshness_streams(&pool, "ours").await;
        let stale = streams
            .iter()
            .find(|(n, _)| n == "stale")
            .unwrap()
            .1
            .to_string();
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _installed = ::metrics::set_default_local_recorder(&recorder);
        let mut leader = scheduler(&pool);
        let now = database_now(&pool).await.unwrap() + time::Duration::minutes(10);
        leader.tick_at(now).await.unwrap();
        let read = || {
            sample(
                &handle.render(),
                "zirv_flow_stream_state",
                &[("stream", &stale)],
            )
        };
        assert_eq!(read(), Some(1.0));

        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_locks \
             WHERE locktype = 'advisory' AND granted AND pid <> pg_backend_pid() \
             AND database = (SELECT oid FROM pg_database WHERE datname = current_database())",
        )
        .execute(&pool)
        .await
        .unwrap();
        let _rival = loop {
            if let Some(rival) = try_lead(&pool).await.unwrap() {
                break rival;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        assert_eq!(leader.tick_at(now).await.unwrap(), Tick::Standby);
        assert_eq!(read(), Some(0.0));
    }
}
