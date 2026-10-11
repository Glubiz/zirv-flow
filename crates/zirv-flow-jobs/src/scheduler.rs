//! The scheduler leader: one instance at a time, elected by an advisory lock, enqueues due
//! schedules, reaps expired leases and exports queue metrics.

use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::watch;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::schedule::latest_slot;
use zirv_flow_store::{
    Leadership, NewJob, PgPool, QueueCounts, Reaped, Schedule, StoreError, advance_schedule, begin,
    database_now, disable_schedule, due_schedules, enqueue, queue_counts, reap_expired, try_lead,
};

use crate::metrics;

/// Schedules handled per tick; more than this carry over to the next tick.
const DUE_BATCH: i64 = 100;

/// How often the scheduler works.
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Between leader ticks.
    pub tick_interval: Duration,
    /// Between attempts of a non-leader to take over.
    pub standby_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            tick_interval: Duration::from_secs(1),
            standby_interval: Duration::from_secs(5),
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
}

impl Scheduler {
    pub fn new(pool: PgPool, config: SchedulerConfig) -> Self {
        Self {
            pool,
            config,
            leadership: None,
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
    }

    /// One pass: take or keep the lock, and if this instance leads, do the leader's work
    /// against the database's clock.
    pub async fn tick(&mut self) -> Result<Tick, StoreError> {
        if !self.lead().await? {
            return Ok(Tick::Standby);
        }
        let now = database_now(&self.pool).await?;
        Ok(Tick::Led(self.work_at(now).await?))
    }

    /// Like [`tick`](Self::tick) with the clock given, for tests that walk through slots
    /// without waiting for them.
    #[cfg(test)]
    pub(crate) async fn tick_at(&mut self, now: OffsetDateTime) -> Result<Tick, StoreError> {
        if !self.lead().await? {
            return Ok(Tick::Standby);
        }
        Ok(Tick::Led(self.work_at(now).await?))
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
        }
        self.leadership = try_lead(&self.pool).await?;
        Ok(self.leadership.is_some())
    }

    /// The leader's work: due schedules, expired leases, queue gauges.
    pub(crate) async fn work_at(&self, now: OffsetDateTime) -> Result<LeaderReport, StoreError> {
        let (fired, skipped) = self.enqueue_due(now).await?;
        let reaped = reap_expired(&self.pool).await?;
        if reaped.requeued + reaped.dead > 0 {
            warn!(
                "reaped {} job(s) with expired leases ({} dead)",
                reaped.requeued + reaped.dead,
                reaped.dead
            );
        }
        let counts = queue_counts(&self.pool).await?;
        metrics::set_queue_gauges(counts);
        Ok(LeaderReport {
            fired,
            skipped,
            reaped,
            counts,
        })
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

        let mut job = NewJob::new(schedule.kind.clone());
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
        zirv_flow_store::enqueue(&pool, &NewJob::new("test.tick"))
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
}
