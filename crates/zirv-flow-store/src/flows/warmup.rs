//! Flow warm-up (architecture 9.4): for a window after a flow starts taking enrollments, entries
//! are admitted along a ramp and the rest wait for their turn. Nobody is dropped.
//!
//! Each entry has a stable place in the window, `hash(flow, person, occurrence)` as a fraction of
//! it, so a backlog spreads evenly and the result is reproducible. An entry whose place has not
//! come is stored as waiting and gets a `flow.enroll` job at its admission time. The window
//! starts at the first publish and at a resume, never at a new version; the setting sits on the
//! flow row, so changing it and opening the flow early need no publish.

use hashbrown::HashSet;
use sqlx::{FromRow, PgConnection, PgExecutor};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::crud::FlowError;
use super::triggers::{ENROLL_JOB_KIND, EnrollJobPayload, EnrollWork, TriggerKind};
use crate::jobs::NOTIFY_CHANNEL;
use crate::{NewJob, StoreError};

/// The shortest warm-up window.
pub const MIN_WARMUP_SECONDS: i32 = 60;
/// The longest warm-up window.
pub const MAX_WARMUP_SECONDS: i32 = 86_400;
/// The window of a flow that has not chosen one.
pub const DEFAULT_WARMUP_SECONDS: i32 = 300;

/// An offset is a fraction of this, so it is exact in a `bigint`.
const OFFSET_SCALE_BITS: u32 = 53;

/// The warm-up of a flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Warmup {
    pub enabled: bool,
    pub seconds: i32,
    /// When the window last started: the first publish or the last resume. None for a flow that
    /// never started one.
    pub started_at: Option<OffsetDateTime>,
    /// When an Admin ended the window early.
    pub opened_at: Option<OffsetDateTime>,
}

impl Warmup {
    /// When the window ends, or none when there is none.
    pub fn ends_at(&self) -> Option<OffsetDateTime> {
        self.started_at
            .map(|started| started + Duration::seconds(i64::from(self.seconds)))
    }

    /// Whether entries are being held back at `now`.
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.enabled && self.opened_at.is_none() && self.ends_at().is_some_and(|ends| now < ends)
    }

    /// When the entry with `offset` is admitted. Whole microseconds, rounded down, so the SQL that
    /// re-queues waiting entries computes the same instant.
    pub fn admit_at(&self, offset: i64) -> Option<OffsetDateTime> {
        let started = self.started_at?;
        let window = u128::try_from(self.seconds).ok()? * 1_000_000;
        let offset = u128::try_from(offset).ok()?;
        let micros = i64::try_from((window * offset) >> OFFSET_SCALE_BITS).ok()?;
        Some(started + Duration::microseconds(micros))
    }
}

impl Default for Warmup {
    fn default() -> Self {
        Self {
            enabled: true,
            seconds: DEFAULT_WARMUP_SECONDS,
            started_at: None,
            opened_at: None,
        }
    }
}

/// The stable place of an entry in the window, in `0..2^53`.
pub(crate) fn admission_offset(flow: Uuid, person: Uuid, occurrence: Uuid) -> i64 {
    let digest = sha256(&format!("warmup:{flow}:{person}:{occurrence}"));
    let mut head = [0_u8; 8];
    head.copy_from_slice(&digest[..8]);
    i64::try_from(u64::from_be_bytes(head) >> (64 - OFFSET_SCALE_BITS)).unwrap_or(0)
}

/// How many entries of a flow have been admitted since its window started, and how many wait.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WarmupCounts {
    pub admitted: i64,
    pub waiting: i64,
}

/// An entry to hold back.
#[derive(Clone, Debug)]
pub(crate) struct Deferred {
    pub person: Uuid,
    pub occurrence: Uuid,
    pub offset: i64,
    pub admit_at: OffsetDateTime,
    pub trace: TraceContext,
}

#[derive(FromRow)]
pub(super) struct WarmupRow {
    warmup_enabled: bool,
    warmup_seconds: i32,
    warmup_started_at: Option<OffsetDateTime>,
    warmup_opened_at: Option<OffsetDateTime>,
}

impl From<WarmupRow> for Warmup {
    fn from(row: WarmupRow) -> Self {
        Self {
            enabled: row.warmup_enabled,
            seconds: row.warmup_seconds,
            started_at: row.warmup_started_at,
            opened_at: row.warmup_opened_at,
        }
    }
}

/// The warm-up of a flow, or the default for one that is gone.
pub(super) async fn read(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<Warmup, StoreError> {
    let row: Option<WarmupRow> = sqlx::query_as(
        "SELECT warmup_enabled, warmup_seconds, warmup_started_at, warmup_opened_at \
         FROM flows WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Warmup::from).unwrap_or_default())
}

/// The tail of a queued entry's key: a window is named by its start and length, so a restarted or
/// resized window queues its entries again while a job of the old one may still be waiting.
pub(super) fn window_key(started: OffsetDateTime, seconds: i32) -> String {
    format!("{}:{seconds}", started.unix_timestamp_nanos() / 1000)
}

fn unique_key(flow: Uuid, person: Uuid, occurrence: Uuid, window: &str) -> String {
    format!("flow.enroll:warmup:{flow}:{person}:{occurrence}:{window}")
}

/// Stores the entries as waiting (a stored entry is left as it is) and queues each one's
/// `flow.enroll` job for its admission time.
pub(crate) async fn defer(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    version: i32,
    trigger: TriggerKind,
    window: (OffsetDateTime, i32),
    entries: &[Deferred],
) -> Result<(), StoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let people: Vec<Uuid> = entries.iter().map(|e| e.person).collect();
    let occurrences: Vec<Uuid> = entries.iter().map(|e| e.occurrence).collect();
    let offsets: Vec<i64> = entries.iter().map(|e| e.offset).collect();
    let traces: Vec<Uuid> = entries.iter().map(|e| e.trace.trace_id.as_uuid()).collect();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| e.trace.cause.kind.as_str())
        .collect();
    let causes: Vec<&str> = entries.iter().map(|e| e.trace.cause.id.as_str()).collect();
    sqlx::query(
        "INSERT INTO flow_warmup_waiting \
            (workspace_id, flow_id, person_id, occurrence_id, flow_version, trigger, \
             admission_offset, trace_id, cause_kind, cause_id) \
         SELECT $1, $2, t.person_id, t.occurrence_id, $3, $4, t.admission_offset, t.trace_id, \
                t.cause_kind, t.cause_id \
         FROM unnest($5::uuid[], $6::uuid[], $7::int8[], $8::uuid[], $9::text[], $10::text[]) \
              AS t(person_id, occurrence_id, admission_offset, trace_id, cause_kind, cause_id) \
         ON CONFLICT DO NOTHING",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(version)
    .bind(trigger.as_str())
    .bind(&people)
    .bind(&occurrences)
    .bind(&offsets)
    .bind(&traces)
    .bind(&kinds)
    .bind(&causes)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let window = window_key(window.0, window.1);
    let jobs: Vec<NewJob> = entries
        .iter()
        .map(|entry| {
            let payload = EnrollJobPayload {
                workspace,
                flow,
                version: Some(version),
                trigger,
                work: EnrollWork::People(vec![(entry.person, entry.occurrence)]),
            };
            let key = unique_key(flow, entry.person, entry.occurrence, &window);
            let mut job = payload.job(&entry.trace, key);
            job.run_at = Some(entry.admit_at);
            job
        })
        .collect();
    crate::enqueue_many(connection, &jobs).await?;
    Ok(())
}

/// Which of the `(person, occurrence)` pairs are waiting.
pub(crate) async fn waiting_of(
    connection: &mut PgConnection,
    flow: Uuid,
    pairs: &[(Uuid, Uuid)],
) -> Result<HashSet<(Uuid, Uuid)>, StoreError> {
    let (people, occurrences): (Vec<Uuid>, Vec<Uuid>) = pairs.iter().copied().unzip();
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT w.person_id, w.occurrence_id FROM flow_warmup_waiting w \
         JOIN unnest($2::uuid[], $3::uuid[]) AS t(person_id, occurrence_id) \
           ON w.person_id = t.person_id AND w.occurrence_id = t.occurrence_id \
         WHERE w.flow_id = $1",
    )
    .bind(flow)
    .bind(&people)
    .bind(&occurrences)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

/// Removes the waiting rows of entries that were admitted or refused.
pub(crate) async fn release(
    connection: &mut PgConnection,
    flow: Uuid,
    pairs: &[(Uuid, Uuid)],
) -> Result<(), StoreError> {
    if pairs.is_empty() {
        return Ok(());
    }
    let (people, occurrences): (Vec<Uuid>, Vec<Uuid>) = pairs.iter().copied().unzip();
    sqlx::query(
        "DELETE FROM flow_warmup_waiting w USING unnest($2::uuid[], $3::uuid[]) \
              AS t(person_id, occurrence_id) \
         WHERE w.flow_id = $1 AND w.person_id = t.person_id AND w.occurrence_id = t.occurrence_id",
    )
    .bind(flow)
    .bind(&people)
    .bind(&occurrences)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Queues a `flow.enroll` job for every waiting entry of the flow, in one statement. With
/// `window` the jobs run at the entries' admission times in the window that started then; without
/// it they run now.
async fn queue_waiting(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    window: Option<(OffsetDateTime, i32)>,
) -> Result<u64, StoreError> {
    let started = window.map(|(started, _)| started);
    let key_tail = window.map_or_else(|| "open".to_owned(), |(s, n)| window_key(s, n));
    let seconds = window.map(|(_, seconds)| seconds);
    let queued = sqlx::query(
        "WITH queued AS ( \
             INSERT INTO jobs (id, workspace_id, kind, payload, run_at, unique_key, trace_id, \
                               cause_kind, cause_id) \
             SELECT gen_random_uuid(), w.workspace_id, $3, \
                    jsonb_build_object( \
                        'workspace_id', w.workspace_id::text, 'flow_id', w.flow_id::text, \
                        'trigger', w.trigger, 'version', w.flow_version, \
                        'people', jsonb_build_array(jsonb_build_array( \
                            w.person_id::text, w.occurrence_id::text))), \
                    COALESCE($4::timestamptz + (trunc($5::numeric * 1000000 * w.admission_offset \
                                                      / 9007199254740992)::bigint) \
                                               * interval '1 microsecond', now()), \
                    'flow.enroll:warmup:' || w.flow_id || ':' || w.person_id || ':' \
                        || w.occurrence_id || ':' || $6, \
                    w.trace_id, w.cause_kind, w.cause_id \
             FROM flow_warmup_waiting w \
             WHERE w.workspace_id = $1 AND w.flow_id = $2 \
             ON CONFLICT DO NOTHING \
             RETURNING 1) \
         SELECT pg_notify($7, $3), (SELECT count(*) FROM queued)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(ENROLL_JOB_KIND)
    .bind(started)
    .bind(seconds)
    .bind(key_tail)
    .bind(NOTIFY_CHANNEL)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    let count: i64 = sqlx::Row::try_get(&queued, 1).map_err(StoreError::query)?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// Starts a window now: at the first publish and at a resume. The waiting entries of a resumed
/// flow keep their places in the new window and are queued for their new admission times.
pub(crate) async fn start(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    super::enroll::lock_enrolls(connection, flow).await?;
    let (enabled, seconds): (bool, i32) = sqlx::query_as(
        "UPDATE flows SET warmup_started_at = $3, warmup_opened_at = NULL \
         WHERE workspace_id = $1 AND id = $2 RETURNING warmup_enabled, warmup_seconds",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(now)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let window = enabled.then_some((now, seconds));
    queue_waiting(connection, workspace, flow, window).await?;
    Ok(())
}

/// Changes the setting. A window already running keeps its start; a new length counts from it.
pub async fn set_warmup(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    enabled: bool,
    seconds: i32,
) -> Result<Warmup, FlowError> {
    if !(MIN_WARMUP_SECONDS..=MAX_WARMUP_SECONDS).contains(&seconds) {
        return Err(FlowError::WarmupOutOfBounds);
    }
    let locked = super::lock_flow(connection, workspace, flow).await?;
    if locked.flow().state == super::FlowState::Archived {
        return Err(FlowError::Archived);
    }
    super::enroll::lock_enrolls(connection, flow).await?;
    let old = locked.flow().warmup;
    sqlx::query("UPDATE flows SET warmup_enabled = $3, warmup_seconds = $4, updated_at = now() WHERE workspace_id = $1 AND id = $2")
        .bind(workspace.as_uuid())
        .bind(flow)
        .bind(enabled)
        .bind(seconds)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let changed = Warmup {
        enabled,
        seconds,
        ..old
    };
    // A new length moves every place in the window, so the waiting entries are queued again for
    // their new admission times. Turning it off lets them in now.
    if !enabled {
        queue_waiting(connection, workspace, flow, None).await?;
    } else if let (Some(started), None) = (changed.started_at, changed.opened_at) {
        queue_waiting(connection, workspace, flow, Some((started, seconds))).await?;
    }
    Ok(changed)
}

/// Ends the window early and queues every waiting entry to run now. Returns how many waited.
pub async fn open_warmup(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    now: OffsetDateTime,
) -> Result<u64, FlowError> {
    let locked = super::lock_flow(connection, workspace, flow).await?;
    // A paused flow admits nobody, and its resume restarts the window, so opening it would only
    // be forgotten.
    let state = locked.flow().state;
    if state != super::FlowState::Published {
        return Err(FlowError::WarmupNotOpenable { state });
    }
    super::enroll::lock_enrolls(connection, flow).await?;
    sqlx::query(
        "UPDATE flows SET warmup_opened_at = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(now)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(queue_waiting(connection, workspace, flow, None).await?)
}

/// Drops the waiting entries of an archived flow and the jobs queued for them: an archived flow
/// enrolls nobody, so they will never be admitted.
pub(crate) async fn discard(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<(), StoreError> {
    super::enroll::lock_enrolls(connection, flow).await?;
    sqlx::query("DELETE FROM flow_warmup_waiting WHERE workspace_id = $1 AND flow_id = $2")
        .bind(workspace.as_uuid())
        .bind(flow)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    sqlx::query(
        "DELETE FROM jobs WHERE workspace_id = $1 AND kind = $2 AND state = 'ready' \
         AND unique_key LIKE $3",
    )
    .bind(workspace.as_uuid())
    .bind(ENROLL_JOB_KIND)
    .bind(format!("flow.enroll:warmup:{flow}:%"))
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The admitted and waiting counts of a flow's current window.
pub async fn warmup_counts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<WarmupCounts, StoreError> {
    let (admitted, waiting): (i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM enrollments e, flows f \
             WHERE f.workspace_id = $1 AND f.id = $2 AND e.flow_id = f.id \
               AND f.warmup_started_at IS NOT NULL AND e.created_at >= f.warmup_started_at), \
            (SELECT count(*) FROM flow_warmup_waiting WHERE workspace_id = $1 AND flow_id = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(WarmupCounts { admitted, waiting })
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const START: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);

    fn warmup(seconds: i32) -> Warmup {
        Warmup {
            seconds,
            started_at: Some(START),
            ..Warmup::default()
        }
    }

    #[test]
    fn an_offset_is_stable_and_inside_the_scale() {
        let (flow, person, occurrence) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        let first = admission_offset(flow, person, occurrence);

        assert_eq!(first, admission_offset(flow, person, occurrence));
        assert!((0..1_i64 << OFFSET_SCALE_BITS).contains(&first));
        assert_ne!(first, admission_offset(flow, Uuid::now_v7(), occurrence));
    }

    #[test]
    fn offsets_spread_over_the_whole_window() {
        let flow = Uuid::now_v7();
        let mut early = 0;
        for _ in 0..2000 {
            let offset = admission_offset(flow, Uuid::now_v7(), Uuid::now_v7());
            if offset < 1_i64 << (OFFSET_SCALE_BITS - 1) {
                early += 1;
            }
        }

        assert!(
            (850..1150).contains(&early),
            "{early} of 2000 in the first half"
        );
    }

    #[test]
    fn the_admission_time_is_the_offset_share_of_the_window() {
        let window = warmup(300);

        assert_eq!(window.admit_at(0), Some(START));
        assert_eq!(
            window.admit_at(1_i64 << (OFFSET_SCALE_BITS - 1)),
            Some(START + Duration::seconds(150))
        );
        let last = window.admit_at((1_i64 << OFFSET_SCALE_BITS) - 1).unwrap();
        assert!(last < START + Duration::seconds(300));
        assert_eq!(Warmup::default().admit_at(5), None);
    }

    #[test]
    fn a_window_holds_entries_back_only_while_enabled_unopened_and_running() {
        let window = warmup(300);
        let inside = START + Duration::seconds(299);
        let after = START + Duration::seconds(300);

        assert!(window.is_active(inside));
        assert!(!window.is_active(after));
        assert!(
            !Warmup {
                enabled: false,
                ..window
            }
            .is_active(inside)
        );
        assert!(
            !Warmup {
                opened_at: Some(inside),
                ..window
            }
            .is_active(inside)
        );
        assert!(!Warmup::default().is_active(inside), "never started");
    }

    #[test]
    fn the_bounds_are_a_minute_and_a_day() {
        assert_eq!((MIN_WARMUP_SECONDS, MAX_WARMUP_SECONDS), (60, 86_400));
        assert_eq!(DEFAULT_WARMUP_SECONDS, 300);
    }

    use sqlx::PgPool;
    use zirv_flow_core::Cause;

    use super::super::enroll::{EnrollOutcome, EnrollRequest, Occurrence, enroll};
    use super::super::tests_support::{create, trace, workspace};
    use super::super::{lock_flow, pause_flow, publish_flow, resume_flow};
    use super::FlowError;
    use crate::{Clock, ManualClock, begin, commit};

    fn at(seconds: i64) -> OffsetDateTime {
        START + Duration::seconds(seconds)
    }

    async fn publish_at(pool: &PgPool, ws: WorkspaceId, flow: Uuid, now: OffsetDateTime) {
        let mut tx = begin(pool).await.unwrap();
        let locked = lock_flow(&mut tx, ws, flow).await.unwrap();
        let clock = Clock::Manual(ManualClock::new(now));
        publish_flow(&mut tx, &clock, ws, &locked, None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    async fn pause(pool: &PgPool, ws: WorkspaceId, flow: Uuid) {
        let mut tx = begin(pool).await.unwrap();
        pause_flow(&mut tx, ws, flow, &trace()).await.unwrap();
        commit(tx).await.unwrap();
    }

    async fn resume_at(pool: &PgPool, ws: WorkspaceId, flow: Uuid, now: OffsetDateTime) {
        let mut tx = begin(pool).await.unwrap();
        let clock = Clock::Manual(ManualClock::new(now));
        resume_flow(&mut tx, &clock, ws, flow, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    async fn people(pool: &PgPool, ws: WorkspaceId, count: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for _ in 0..count {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(id)
                .bind(ws.as_uuid())
                .execute(pool)
                .await
                .unwrap();
            ids.push(id);
        }
        ids
    }

    /// A flow published at `START` and the people who are about to enter it for one occurrence.
    struct Backlog {
        ws: WorkspaceId,
        flow: Uuid,
        people: Vec<Uuid>,
        occurrence: Uuid,
    }

    impl Backlog {
        async fn new(pool: &PgPool, enabled: bool, seconds: i32) -> Self {
            let ws = workspace(pool, "warm").await;
            let flow = create(pool, ws, "Welcome").await.id;
            let mut connection = pool.acquire().await.unwrap();
            set_warmup(&mut connection, ws, flow, enabled, seconds)
                .await
                .unwrap();
            drop(connection);
            publish_at(pool, ws, flow, START).await;
            Self {
                ws,
                flow,
                people: people(pool, ws, 24).await,
                occurrence: Uuid::now_v7(),
            }
        }

        /// Enters everyone at `now`, as a trigger that fires again would.
        async fn enter(&self, pool: &PgPool, now: OffsetDateTime) -> EnrollOutcome {
            let mut tx = begin(pool).await.unwrap();
            let outcome = enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: self.ws,
                    flow: self.flow,
                    version: 1,
                    people: &self.people,
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: self.occurrence,
                    },
                    now,
                    trace: &TraceContext::root(Cause::api("enter")),
                },
            )
            .await
            .unwrap();
            commit(tx).await.unwrap();
            outcome
        }

        async fn counts(&self, pool: &PgPool) -> WarmupCounts {
            warmup_counts(pool, self.ws, self.flow).await.unwrap()
        }

        /// When each waiting person is admitted, by the jobs queued for the window that started
        /// at `started` and lasts `seconds`.
        async fn admissions(
            &self,
            pool: &PgPool,
            started: OffsetDateTime,
            seconds: i32,
        ) -> Vec<OffsetDateTime> {
            sqlx::query_scalar(
                "SELECT run_at FROM jobs WHERE kind = 'flow.enroll' \
                 AND unique_key LIKE '%:' || $1 ORDER BY run_at",
            )
            .bind(window_key(started, seconds))
            .fetch_all(pool)
            .await
            .unwrap()
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_flow_spreads_a_backlog_over_its_window(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;

        let first = backlog.enter(&pool, START).await;
        let queued = backlog.admissions(&pool, START, 300).await;

        assert!(first.enrolled.is_empty(), "nobody enters before their turn");
        assert_eq!(backlog.counts(&pool).await.waiting, 24);
        assert_eq!(queued.len(), 24);
        assert!(queued.iter().all(|turn| *turn >= START && *turn < at(300)));
        assert!(queued.first() != queued.last(), "the turns are spread");
        let mut entered = 0;
        for checkpoint in [60, 150, 299] {
            let now = at(checkpoint);
            let due = queued.iter().filter(|turn| **turn <= now).count();
            entered += backlog.enter(&pool, now).await.enrolled.len();
            assert_eq!(entered, due, "{due} are due {checkpoint} seconds in");
        }
        let end = backlog.enter(&pool, at(300)).await;
        assert_eq!(
            entered + end.enrolled.len(),
            24,
            "all have entered by the end"
        );
        assert_eq!(backlog.counts(&pool).await.waiting, 0);
        assert_eq!(backlog.enter(&pool, at(301)).await.replayed, 24);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn with_the_warm_up_off_every_entry_enrolls_at_once(pool: PgPool) {
        let backlog = Backlog::new(&pool, false, 300).await;

        let outcome = backlog.enter(&pool, START).await;

        assert_eq!(outcome.enrolled.len(), 24);
        assert_eq!(backlog.counts(&pool).await.waiting, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn open_now_admits_everyone_waiting(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        backlog.enter(&pool, START).await;

        let mut connection = pool.acquire().await.unwrap();
        let waited = open_warmup(&mut connection, backlog.ws, backlog.flow, at(1))
            .await
            .unwrap();
        drop(connection);
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'flow.enroll' AND unique_key LIKE '%:open'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let admitted = backlog.enter(&pool, at(1)).await;

        assert_eq!((waited, queued), (24, 24));
        assert_eq!(admitted.enrolled.len(), 24);
        assert_eq!(backlog.counts(&pool).await.waiting, 0);
        // A later arrival is not held back either.
        let late = people(&pool, backlog.ws, 1).await;
        let mut tx = begin(&pool).await.unwrap();
        let outcome = enroll(
            &mut tx,
            &EnrollRequest {
                workspace: backlog.ws,
                flow: backlog.flow,
                version: 1,
                people: &late,
                occurrence: Occurrence {
                    trigger: "manual",
                    id: Uuid::now_v7(),
                },
                now: at(2),
                trace: &TraceContext::root(Cause::api("late")),
            },
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(outcome.enrolled.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pause_keeps_entries_waiting_and_a_resume_restarts_the_window(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        backlog.enter(&pool, START).await;

        pause(&pool, backlog.ws, backlog.flow).await;
        // The window would be over, but a paused flow takes nobody and skips nobody.
        let paused = backlog.enter(&pool, at(400)).await;
        assert!(paused.enrolled.is_empty() && paused.skipped.is_empty());
        assert_eq!(backlog.counts(&pool).await.waiting, 24);

        resume_at(&pool, backlog.ws, backlog.flow, at(400)).await;
        let queued = backlog.admissions(&pool, at(400), 300).await;
        assert_eq!(queued.len(), 24, "queued again for the new window");
        assert!(
            queued
                .iter()
                .all(|turn| *turn >= at(400) && *turn < at(700))
        );
        let early = backlog.enter(&pool, at(410)).await;
        let due = queued.iter().filter(|turn| **turn <= at(410)).count();
        assert_eq!(early.enrolled.len(), due);
        let rest = backlog.enter(&pool, at(700)).await;
        assert_eq!(early.enrolled.len() + rest.enrolled.len(), 24);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn waiting_entries_are_counted_and_in_the_decision_log(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        backlog.enter(&pool, START).await;
        let queued = backlog.admissions(&pool, START, 300).await;
        let now = at(100);
        let due = queued.iter().filter(|turn| **turn <= now).count();
        let admitted = backlog.enter(&pool, now).await.enrolled.len();

        let counts = backlog.counts(&pool).await;

        assert_eq!(admitted, due);
        assert_eq!(counts.waiting, i64::try_from(24 - due).unwrap());
        let deferred: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT reason FROM decision_records \
             WHERE action = 'flow.enroll' AND outcome = 'deferred'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(deferred.len(), 24);
        assert!(
            deferred
                .iter()
                .all(|reason| reason.as_deref() == Some("warm_up"))
        );
        let first: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'flow.enroll' \
             AND outcome = 'deferred' AND subject_ids ->> 'person' = $1",
        )
        .bind(backlog.people[0].to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first, 1, "each person's log has it");
        let times: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT subject_ids ->> 'admit_at' FROM decision_records \n             WHERE action = 'flow.enroll' AND outcome = 'deferred'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut times: Vec<OffsetDateTime> = times
            .iter()
            .map(|time| {
                OffsetDateTime::parse(
                    time.as_deref().unwrap(),
                    &time::format_description::well_known::Rfc3339,
                )
                .unwrap()
            })
            .collect();
        times.sort();
        assert_eq!(times, queued, "the decision carries the admission time");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_version_does_not_restart_the_window(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;

        publish_at(&pool, backlog.ws, backlog.flow, at(120)).await;

        let flow = crate::find_flow(&pool, backlog.ws, backlog.flow)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(flow.warmup.started_at, Some(START));
        assert_eq!(flow.current_version, Some(2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_length_is_kept_between_a_minute_and_a_day(pool: PgPool) {
        let ws = workspace(&pool, "bounds").await;
        let flow = create(&pool, ws, "Welcome").await.id;
        let mut connection = pool.acquire().await.unwrap();

        let results = [
            set_warmup(&mut connection, ws, flow, true, 59)
                .await
                .is_err(),
            set_warmup(&mut connection, ws, flow, true, 60)
                .await
                .is_ok(),
            set_warmup(&mut connection, ws, flow, true, 86_400)
                .await
                .is_ok(),
            set_warmup(&mut connection, ws, flow, true, 86_401)
                .await
                .is_err(),
        ];

        assert_eq!(results, [true; 4]);
        let stored = crate::find_flow(&pool, ws, flow).await.unwrap().unwrap();
        assert_eq!(
            (stored.warmup.enabled, stored.warmup.seconds),
            (true, 86_400)
        );
        let direct = sqlx::query("UPDATE flows SET warmup_seconds = 30 WHERE id = $1")
            .bind(flow)
            .execute(&pool)
            .await;
        assert!(direct.is_err(), "the table refuses it too");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archiving_drops_the_waiting_entries_and_their_jobs(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        backlog.enter(&pool, START).await;
        assert_eq!(backlog.counts(&pool).await.waiting, 24);

        let mut tx = begin(&pool).await.unwrap();
        super::super::archive_flow(&mut tx, backlog.ws, backlog.flow, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert_eq!(backlog.counts(&pool).await.waiting, 0);
        assert!(backlog.admissions(&pool, START, 300).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn open_now_is_refused_unless_the_flow_is_published(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        backlog.enter(&pool, START).await;
        pause(&pool, backlog.ws, backlog.flow).await;

        let mut connection = pool.acquire().await.unwrap();
        let refused = open_warmup(&mut connection, backlog.ws, backlog.flow, at(1)).await;
        drop(connection);

        assert!(matches!(
            refused,
            Err(FlowError::WarmupNotOpenable {
                state: super::super::FlowState::Paused
            })
        ));
        assert_eq!(backlog.counts(&pool).await.waiting, 24);
        let flow = crate::find_flow(&pool, backlog.ws, backlog.flow)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(flow.warmup.opened_at, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resume_waits_for_an_enroll_in_flight(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        pause(&pool, backlog.ws, backlog.flow).await;
        let mut enrolling = begin(&pool).await.unwrap();
        super::super::enroll::lock_enrolls(&mut enrolling, backlog.flow)
            .await
            .unwrap();

        let (ws, flow, other) = (backlog.ws, backlog.flow, pool.clone());
        let mut resume = tokio::spawn(async move { resume_at(&other, ws, flow, at(10)).await });
        let early = tokio::time::timeout(std::time::Duration::from_millis(500), &mut resume).await;
        assert!(early.is_err(), "the resume waits for the enroll");

        commit(enrolling).await.unwrap();
        resume.await.unwrap();
    }
}
