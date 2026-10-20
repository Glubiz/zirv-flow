//! The virtual-time loop. The clock moves from one instant to the next at which something is
//! due (a job's `run_at` or a scheduled event); at each instant every due job runs, in a seeded
//! order, until nothing is due. Which jobs are claimable is controlled by rewriting `run_at`, as
//! the existing tests do: the database's own `now()` is the real time, so a job at a virtual
//! instant is held back until the driver makes it due.

use std::collections::BTreeMap;

use hashbrown::{HashMap, HashSet};

use serde_json::{Value, json};
use time::Duration as Span;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_engine::sim::generate::EVENTS;
use zirv_flow_engine::sim::rng::{Rng, mix};
use zirv_flow_store::{ENROLL_JOB_KIND, STEP_JOB_KIND, WEBHOOK_JOB_KIND};

use super::world::{EPOCH, World, end};

/// Streams of the seed, so each decision draws from its own and one never shifts another.
pub(super) const LANE_PLAN: u64 = 20;
pub(super) const LANE_ORDER: u64 = 21;
pub(super) const LANE_EARLY: u64 = 22;
const LANE_EVENTS: u64 = 23;
const LANE_NOISE: u64 = 24;

/// The kinds of job the driver runs. Anything else the runtime enqueues (a segment re-evaluation
/// request, say) is left alone.
pub(super) const KINDS: [&str; 3] = [ENROLL_JOB_KIND, STEP_JOB_KIND, WEBHOOK_JOB_KIND];

/// A wake that moved an enrollment on: the timer it answered, and the virtual time it ran.
#[derive(Clone, Debug)]
pub(super) struct WakeMove {
    pub(super) enrollment: Uuid,
    pub(super) wake_at: OffsetDateTime,
    pub(super) at: OffsetDateTime,
}

/// A job that is due.
#[derive(Clone, Debug)]
pub(super) struct Due {
    pub(super) id: Uuid,
    pub(super) kind: &'static str,
    /// A name for the job that does not depend on random ids, so a seed gives the same plan.
    pub(super) label: u64,
}

struct Pending {
    email: String,
    name: String,
    total: u64,
}

pub(super) struct Driver<'w> {
    pub(super) w: &'w World,
    pub(super) now: OffsetDateTime,
    pending: BTreeMap<(OffsetDateTime, u64), Pending>,
    seq: u64,
    scheduled: HashSet<(Uuid, i64)>,
    person_index: HashMap<Uuid, usize>,
    owner: HashMap<Uuid, usize>,
    /// How often each thing happened, by name.
    pub(super) counts: BTreeMap<String, usize>,
    pub(super) wake_moves: Vec<WakeMove>,
    /// What went wrong during the run itself (a handler that failed, a fence that let a stale
    /// execution through), apart from what the tables show afterwards.
    pub(super) problems: Vec<String>,
}

/// A stable 64-bit name for a text.
pub(super) fn hash_text(text: &str) -> u64 {
    text.bytes()
        .fold(0x9e37_79b9, |state, byte| mix(state ^ u64::from(byte)))
}

/// A virtual instant as seconds since the Unix epoch, for seeding.
pub(super) fn seconds_of(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp()).unwrap_or(0)
}

impl<'w> Driver<'w> {
    pub(super) fn new(w: &'w World) -> Self {
        let person_index = w
            .people
            .iter()
            .enumerate()
            .map(|(index, person)| (person.id, index))
            .collect();
        let mut driver = Self {
            w,
            now: EPOCH,
            pending: BTreeMap::new(),
            seq: 0,
            scheduled: HashSet::new(),
            person_index,
            owner: HashMap::new(),
            counts: BTreeMap::new(),
            wake_moves: Vec::new(),
            problems: Vec::new(),
        };
        driver.schedule_noise();
        driver
    }

    pub(super) fn bump(&mut self, key: impl Into<String>) {
        *self.counts.entry(key.into()).or_default() += 1;
    }

    pub(super) fn problem(&mut self, text: String) {
        println!("PROBLEM: {text}");
        if self.problems.len() < 50 {
            self.problems.push(text);
        }
    }

    /// Events that no wait asked for, at seeded instants: most match nothing, some arrive while
    /// a person waits for them.
    fn schedule_noise(&mut self) {
        let w = self.w;
        let mut rng = Rng::derive(w.seed, &[LANE_NOISE]);
        for _ in 0..w.people.len() / 2 {
            let person = rng.index(w.people.len());
            let name = (*rng.pick(&EVENTS)).to_owned();
            let at = EPOCH + Span::seconds(i64::try_from(rng.below(80 * 86_400)).unwrap_or(0));
            let total = rng.below(600);
            let email = w.people[person].email.clone();
            self.add_event(at, email, name, total);
        }
    }

    fn add_event(&mut self, at: OffsetDateTime, email: String, name: String, total: u64) {
        self.seq += 1;
        self.pending
            .insert((at, self.seq), Pending { email, name, total });
    }

    /// Runs the 90 days.
    pub(super) async fn run(&mut self) {
        let stop = end();
        let mut at = EPOCH;
        loop {
            self.now = at;
            self.w.env.clock.set(at);
            self.deliver_events(at).await;
            self.drain(at).await;
            let next_job: Option<OffsetDateTime> = sqlx::query_scalar(
                "SELECT min(run_at) FROM jobs \
                 WHERE workspace_id = $1 AND state = 'ready' AND kind = ANY($2) AND run_at > $3",
            )
            .bind(self.w.env.ws.as_uuid())
            .bind(KINDS.as_slice())
            .bind(at)
            .fetch_one(&self.w.env.pool)
            .await
            .unwrap();
            let next_event = self.pending.keys().next().map(|key| key.0);
            match [next_job, next_event].into_iter().flatten().min() {
                Some(next) if next <= stop => at = next.max(at),
                _ => break,
            }
        }
    }

    /// Applies the events that are due, as ingestion does. Each event wakes the enrollments
    /// that wait for it by enqueueing `flow.step` jobs.
    async fn deliver_events(&mut self, at: OffsetDateTime) {
        let due: Vec<(OffsetDateTime, u64)> = self
            .pending
            .range(..=(at, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in due {
            if let Some(event) = self.pending.remove(&key) {
                self.w
                    .env
                    .apply_event(&event.email, &event.name, json!({"total": event.total}))
                    .await;
                self.bump("events applied");
            }
        }
    }

    /// Runs every job that is due at `at` until none is, in a seeded order that mixes the jobs
    /// of an instant: an event that arrives at a timeout's instant and the timeout are both
    /// due, and either may run first.
    async fn drain(&mut self, at: OffsetDateTime) {
        self.early_wake(at).await;
        for round in 0..200_u64 {
            let mut due = self.due_jobs(at).await;
            if due.is_empty() {
                return;
            }
            let seed = self.w.seed;
            due.sort_by_key(|job| {
                (
                    mix(seed ^ LANE_ORDER ^ job.label ^ seconds_of(at) ^ round),
                    job.label,
                )
            });
            for job in due {
                self.run_job(&job, false).await;
            }
        }
        self.problem(format!("jobs were still due after 200 rounds at {at}"));
    }

    async fn due_jobs(&mut self, at: OffsetDateTime) -> Vec<Due> {
        let rows: Vec<(Uuid, String, Value)> = sqlx::query_as(
            "SELECT id, kind, payload FROM jobs \
             WHERE workspace_id = $1 AND state = 'ready' AND kind = ANY($2) AND run_at <= $3",
        )
        .bind(self.w.env.ws.as_uuid())
        .bind(KINDS.as_slice())
        .bind(at)
        .fetch_all(&self.w.env.pool)
        .await
        .unwrap();
        let mut due = Vec::with_capacity(rows.len());
        for (id, kind, payload) in rows {
            let Some(kind) = KINDS.iter().copied().find(|known| *known == kind) else {
                continue;
            };
            let label = self.label(kind, &payload).await;
            due.push(Due { id, kind, label });
        }
        due
    }

    /// Which person a job is about, in the population's order.
    async fn person_of_enrollment(&mut self, enrollment: Uuid) -> usize {
        if let Some(index) = self.owner.get(&enrollment) {
            return *index;
        }
        let rows: Vec<(Uuid, Uuid)> =
            sqlx::query_as("SELECT id, person_id FROM enrollments WHERE workspace_id = $1")
                .bind(self.w.env.ws.as_uuid())
                .fetch_all(&self.w.env.pool)
                .await
                .unwrap();
        for (id, person) in rows {
            if let Some(index) = self.person_index.get(&person) {
                self.owner.insert(id, *index);
            }
        }
        self.owner.get(&enrollment).copied().unwrap_or(0)
    }

    async fn label(&mut self, kind: &str, payload: &Value) -> u64 {
        let uuid = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .and_then(|text| Uuid::parse_str(text).ok())
        };
        let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or_default();
        if kind == ENROLL_JOB_KIND {
            let first = uuid(payload.pointer("/people/0/0")).unwrap_or_default();
            let index = self.person_index.get(&first).copied().unwrap_or(0);
            return mix(index as u64 ^ hash_text(ENROLL_JOB_KIND));
        }
        let enrollment = uuid(payload.get("enrollment_id")).unwrap_or_default();
        let index = self.person_of_enrollment(enrollment).await;
        let version = payload
            .get("state_version")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let rest = hash_text(kind) ^ hash_text(text("input")) ^ hash_text(text("node_id"));
        mix(mix(index as u64 ^ rest) ^ u64::try_from(version).unwrap_or(0))
    }

    /// Some wakes are claimed before they are due, as a worker with a fast clock would: the
    /// handler must put them back and not fire them.
    async fn early_wake(&mut self, at: OffsetDateTime) {
        let mut rng = Rng::derive(self.w.seed, &[LANE_EARLY, seconds_of(at)]);
        if !rng.chance(20) {
            return;
        }
        let rows: Vec<(Uuid, Value)> = sqlx::query_as(
            "SELECT id, payload FROM jobs \
             WHERE workspace_id = $1 AND state = 'ready' AND kind = $2 \
               AND payload ->> 'input' = 'wake' AND run_at > $3 \
             ORDER BY run_at, payload ->> 'enrollment_id' LIMIT 6",
        )
        .bind(self.w.env.ws.as_uuid())
        .bind(STEP_JOB_KIND)
        .bind(at)
        .fetch_all(&self.w.env.pool)
        .await
        .unwrap();
        if rows.is_empty() {
            return;
        }
        let (id, payload) = &rows[rng.index(rows.len())];
        let label = self.label(STEP_JOB_KIND, payload).await;
        let job = Due {
            id: *id,
            kind: STEP_JOB_KIND,
            label,
        };
        self.run_job(&job, true).await;
    }

    /// After a step committed: if the enrollment now waits for an event, decide whether and
    /// when one arrives. Some arrive early, some at the timeout's instant, some a second
    /// either side of it, so the event and the timeout race.
    pub(super) async fn plan_events(&mut self, enrollment: Uuid) {
        let waits: Vec<(String, i64, OffsetDateTime, Uuid)> = sqlx::query_as(
            "SELECT event_name, wake_version, timeout_at, person_id FROM enrollment_waits \
             WHERE enrollment_id = $1",
        )
        .bind(enrollment)
        .fetch_all(&self.w.env.pool)
        .await
        .unwrap();
        for (name, version, timeout, person) in waits {
            if !self.scheduled.insert((enrollment, version)) {
                continue;
            }
            let Some(index) = self.person_index.get(&person).copied() else {
                continue;
            };
            let mut rng = Rng::derive(
                self.w.seed,
                &[
                    LANE_EVENTS,
                    index as u64,
                    u64::try_from(version).unwrap_or(0),
                ],
            );
            let now = self.now;
            let window = (timeout - now).whole_seconds().max(2);
            let second = Span::seconds(1);
            let mut times = Vec::new();
            match rng.below(100) {
                0..=29 => {}
                30..=54 => {
                    times.push(
                        now + Span::seconds(
                            1 + i64::try_from(rng.below(window.unsigned_abs())).unwrap_or(0),
                        ),
                    );
                }
                55..=66 => times.push(timeout - second),
                67..=79 => times.push(timeout),
                80..=89 => times.push(timeout + second),
                _ => {
                    times.push(timeout);
                    times.push(timeout + second);
                }
            }
            let email = self.w.people[index].email.clone();
            for at in times {
                let total = rng.below(600);
                self.add_event(at.max(now), email.clone(), name.clone(), total);
                self.bump("events planned for a wait");
            }
        }
    }
}
