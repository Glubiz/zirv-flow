//! Flow warm-up (architecture 9.4): for a window after a flow starts taking enrollments, entries
//! are admitted along a ramp and the rest wait for their turn. Nobody is dropped.
//!
//! Each entry has a stable place in the window, `hash(flow, person, occurrence)` as a fraction of
//! it, so a backlog spreads evenly and the result is reproducible. An entry whose place has not
//! come is stored as waiting and gets a `flow.enroll` job at its admission time; a resume or a new
//! window queues them again, one job per second of the window and trace. The window
//! starts at the first publish and at a resume, never at a new version; the setting sits on the
//! flow row, so changing it and opening the flow early need no publish.

use hashbrown::HashSet;
use sqlx::{FromRow, PgConnection};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::token::sha256;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::crud::FlowError;
use super::enroll::MAX_ENROLL_PEOPLE;
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

/// Queues the `flow.enroll` jobs of every waiting entry of the flow, in one statement. With
/// `window` the jobs run at the entries' admission times in the window that started then;
/// without it they run now. Entries of one trace and cause that are admitted in the same second
/// share a job of at most [`MAX_ENROLL_PEOPLE`] people, which runs at the last of their times, so
/// nobody is admitted early and a million waiting entries are a few thousand jobs, not a million.
/// A job runs under its entries' trace and cause (`run_people`), so entries of different traces
/// never share one. Returns how many people were queued.
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
        "WITH due AS ( \
             SELECT w.*, a.admit_at, date_trunc('second', a.admit_at) AS slot \
             FROM flow_warmup_waiting w, LATERAL (SELECT \
                    COALESCE($4::timestamptz + (trunc($5::numeric * 1000000 * w.admission_offset \
                                                      / 9007199254740992)::bigint) \
                                               * interval '1 microsecond', now()) AS admit_at) a \
             WHERE w.workspace_id = $1 AND w.flow_id = $2), \
         numbered AS ( \
             SELECT d.*, \
                    (row_number() OVER (PARTITION BY d.flow_version, d.trigger, d.trace_id, \
                                                     d.cause_kind, d.cause_id, d.slot \
                                        ORDER BY d.person_id, d.occurrence_id) - 1) / $8 AS chunk \
             FROM due d), \
         bucketed AS ( \
             SELECT n.workspace_id, n.flow_id, n.trigger, n.flow_version, n.trace_id, \
                    n.cause_kind, n.cause_id, max(n.admit_at) AS run_at, \
                    (array_agg(n.person_id ORDER BY n.person_id, n.occurrence_id))[1] AS first_person, \
                    (array_agg(n.occurrence_id ORDER BY n.person_id, n.occurrence_id))[1] AS first_occurrence, \
                    jsonb_agg(jsonb_build_array(n.person_id::text, n.occurrence_id::text) \
                              ORDER BY n.person_id, n.occurrence_id) AS people \
             FROM numbered n \
             GROUP BY n.workspace_id, n.flow_id, n.trigger, n.flow_version, n.trace_id, \
                      n.cause_kind, n.cause_id, n.slot, n.chunk), \
         queued AS ( \
             INSERT INTO jobs (id, workspace_id, kind, payload, run_at, unique_key, trace_id, \
                               cause_kind, cause_id) \
             SELECT gen_random_uuid(), b.workspace_id, $3, \
                    jsonb_build_object( \
                        'workspace_id', b.workspace_id::text, 'flow_id', b.flow_id::text, \
                        'trigger', b.trigger, 'version', b.flow_version, 'people', b.people), \
                    b.run_at, \
                    'flow.enroll:warmup:' || b.flow_id || ':' || $6 || ':' || b.first_person \
                        || ':' || b.first_occurrence, \
                    b.trace_id, b.cause_kind, b.cause_id \
             FROM bucketed b \
             ON CONFLICT DO NOTHING \
             RETURNING jsonb_array_length(payload -> 'people') AS people) \
         SELECT pg_notify($7, $3), COALESCE((SELECT sum(people) FROM queued), 0)::int8",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(ENROLL_JOB_KIND)
    .bind(started)
    .bind(seconds)
    .bind(key_tail)
    .bind(NOTIFY_CHANNEL)
    .bind(i64::try_from(MAX_ENROLL_PEOPLE).unwrap_or(i64::MAX))
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
    Ok(
        set_warmup_counted(connection, workspace, flow, enabled, seconds)
            .await?
            .0,
    )
}

/// [`set_warmup`], also returning how many waiting entries it queued again. Only when `enabled` is
/// false is that the number released: a resize or an enable queues the waiting entries for new
/// admission times and releases nobody.
pub async fn set_warmup_counted(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    enabled: bool,
    seconds: i32,
) -> Result<(Warmup, u64), FlowError> {
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
    let queued = if !enabled {
        queue_waiting(connection, workspace, flow, None).await?
    } else if let (Some(started), None) = (changed.started_at, changed.opened_at) {
        queue_waiting(connection, workspace, flow, Some((started, seconds))).await?
    } else {
        0
    };
    Ok((changed, queued))
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

/// Up to this many entries are counted exactly; above it a count is the planner's estimate.
const COUNT_EXACT: i64 = 10_000;

/// Each count read up to `$4` rows: the enrollments created in the window (`$2` to `$3`) and the
/// waiting entries. Planned for the limit's value each time, never with a generic plan.
const COUNTS: &str = "SELECT \
        (SELECT count(*) FROM (SELECT 1 FROM enrollments \
                               WHERE flow_id = $1 AND created_at >= $2 AND created_at < $3 \
                               LIMIT $4) a), \
        (SELECT count(*) FROM (SELECT 1 FROM flow_warmup_waiting WHERE flow_id = $1 LIMIT $4) w)";

/// The planner's estimates for the two counts above, for a count above [`COUNT_EXACT`].
const ADMITTED_ESTIMATE: &str = "SELECT 1 FROM enrollments WHERE flow_id = $1::uuid \
     AND created_at >= $2::timestamptz AND created_at < $3::timestamptz";
const WAITING_ESTIMATE: &str = "SELECT 1 FROM flow_warmup_waiting WHERE flow_id = $1::uuid";

/// About how many entries of a flow were admitted in its window and how many wait. `admitted`
/// counts the enrollments created from the window's start to its end, or to the moment it was
/// opened early; after the window it no longer grows. Each count is exact up to 10,000 and the
/// planner's estimate above, so a flow with millions of entries costs no more to read than one
/// with ten thousand. Both are 0 for a flow whose window never started.
pub async fn warmup_counts(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<WarmupCounts, StoreError> {
    let warmup = read(connection, workspace, flow).await?;
    let (Some(started), Some(ends)) = (warmup.started_at, warmup.ends_at()) else {
        return Ok(WarmupCounts::default());
    };
    let until = warmup.opened_at.map_or(ends, |opened| opened.min(ends));
    let (admitted, waiting): (i64, i64) = sqlx::query_as(COUNTS)
        .persistent(false)
        .bind(flow)
        .bind(started)
        .bind(until)
        .bind(COUNT_EXACT + 1)
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let text = |at: OffsetDateTime| {
        at.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    };
    let flow = flow.to_string();
    let (started, until) = (text(started), text(until));
    let admitted = crate::bounded::count_or_estimate(
        connection,
        admitted,
        COUNT_EXACT,
        ADMITTED_ESTIMATE,
        &[&flow, &started, &until],
    )
    .await?;
    let waiting = crate::bounded::count_or_estimate(
        connection,
        waiting,
        COUNT_EXACT,
        WAITING_ESTIMATE,
        &[&flow],
    )
    .await?;
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
            let mut connection = pool.acquire().await.unwrap();
            warmup_counts(&mut connection, self.ws, self.flow)
                .await
                .unwrap()
        }

        /// When each waiting person is admitted, by the jobs queued for the window that started
        /// at `started` and lasts `seconds`: a job of one person, from a deferral, or a job of
        /// the people of one second, from a resume or a resize, counted once for each.
        async fn admissions(
            &self,
            pool: &PgPool,
            started: OffsetDateTime,
            seconds: i32,
        ) -> Vec<OffsetDateTime> {
            sqlx::query_scalar(
                "SELECT j.run_at FROM jobs j, \
                    LATERAL jsonb_array_elements(j.payload -> 'people') AS person \
                 WHERE j.kind = 'flow.enroll' \
                   AND (j.unique_key LIKE '%:' || $1 OR j.unique_key LIKE '%:' || $1 || ':%') \
                 ORDER BY j.run_at",
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
        // One job for each trace and second, so far fewer jobs than people.
        let queued: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(jsonb_array_length(payload -> 'people')), 0)::int8 FROM jobs \
             WHERE kind = 'flow.enroll' AND unique_key LIKE '%:open:%'",
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_writer_and_an_enroll_do_not_deadlock(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        let mut writer = begin(&pool).await.unwrap();
        lock_flow(&mut writer, backlog.ws, backlog.flow)
            .await
            .unwrap();

        let (ws, flow, people, other) = (
            backlog.ws,
            backlog.flow,
            backlog.people.clone(),
            pool.clone(),
        );
        let enrolling = tokio::spawn(async move {
            let mut tx = begin(&other).await.unwrap();
            let outcome = enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: ws,
                    flow,
                    version: 1,
                    people: &people,
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: START,
                    trace: &TraceContext::root(Cause::api("enter")),
                },
            )
            .await;
            match outcome {
                Ok(_) => commit(tx).await.map_err(|error| format!("{error:?}")),
                Err(error) => Err(format!("{error:?}")),
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        super::super::enroll::lock_enrolls(&mut writer, backlog.flow)
            .await
            .unwrap();
        commit(writer).await.unwrap();

        let done = tokio::time::timeout(std::time::Duration::from_secs(20), enrolling)
            .await
            .expect("the enroll finishes");
        assert_eq!(done.unwrap(), Ok(()));
        assert_eq!(backlog.counts(&pool).await.waiting, 24);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_counts_cover_the_window_only_and_are_estimated_above_a_limit(pool: PgPool) {
        let backlog = Backlog::new(&pool, true, 300).await;
        let mut connection = pool.acquire().await.unwrap();
        let unstarted = create(&pool, backlog.ws, "Unstarted").await.id;
        // Enrollments created inside the window, just at its end and long after it.
        for (person, seconds) in backlog.people.iter().zip([10, 299, 300, 3600]) {
            sqlx::query(
                "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, \
                     state, node, occurrence_id, trace_id, cause_kind, cause_id, created_at, \
                     ended_at) \
                 VALUES (gen_random_uuid(), $1, $2, 1, $3, 'completed', 'x', gen_random_uuid(), \
                     gen_random_uuid(), 'api', 't', $4, now())",
            )
            .bind(backlog.ws.as_uuid())
            .bind(backlog.flow)
            .bind(person)
            .bind(at(seconds))
            .execute(&pool)
            .await
            .unwrap();
        }

        let counts = warmup_counts(&mut connection, backlog.ws, backlog.flow)
            .await
            .unwrap();
        let never = warmup_counts(&mut connection, backlog.ws, unstarted)
            .await
            .unwrap();

        assert_eq!(
            counts,
            WarmupCounts {
                admitted: 2,
                waiting: 0
            }
        );
        assert_eq!(never, WarmupCounts::default());

        // More waiting entries than the exact limit: the count is the planner's estimate.
        let extra = COUNT_EXACT + 50;
        sqlx::query("INSERT INTO people (id, workspace_id) SELECT gen_random_uuid(), $1 FROM generate_series(1, $2)")
            .bind(backlog.ws.as_uuid())
            .bind(extra)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO flow_warmup_waiting (workspace_id, flow_id, person_id, occurrence_id, \
                 flow_version, trigger, admission_offset, trace_id, cause_kind, cause_id) \
             SELECT $1, $2, p.id, gen_random_uuid(), 1, 'manual', 5, gen_random_uuid(), 'api', 't' \
             FROM (SELECT id FROM people WHERE workspace_id = $1 ORDER BY id DESC LIMIT $3) p",
        )
        .bind(backlog.ws.as_uuid())
        .bind(backlog.flow)
        .bind(extra)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE flow_warmup_waiting")
            .execute(&pool)
            .await
            .unwrap();
        let many = warmup_counts(&mut connection, backlog.ws, backlog.flow)
            .await
            .unwrap();

        let (low, high) = (COUNT_EXACT + 1, extra + 1000);
        assert!((low..=high).contains(&many.waiting), "{many:?}");
    }

    /// The jobs the window queued for `flow`: how many people each carries and when it runs.
    async fn queued_jobs(
        pool: &PgPool,
        flow: Uuid,
    ) -> Vec<(Vec<(Uuid, Uuid)>, OffsetDateTime, Uuid)> {
        let rows: Vec<(serde_json::Value, OffsetDateTime, Uuid)> = sqlx::query_as(
            "SELECT payload -> 'people', run_at, trace_id FROM jobs WHERE kind = 'flow.enroll' \
             AND payload ->> 'flow_id' = $1 ORDER BY run_at, unique_key",
        )
        .bind(flow.to_string())
        .fetch_all(pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(people, run_at, trace)| {
                let pairs = people
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|pair| {
                        let pair = pair.as_array().unwrap();
                        (
                            pair[0].as_str().unwrap().parse().unwrap(),
                            pair[1].as_str().unwrap().parse().unwrap(),
                        )
                    })
                    .collect();
                (pairs, run_at, trace)
            })
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resume_queues_one_job_per_trace_and_second_and_admits_nobody_early(pool: PgPool) {
        let ws = workspace(&pool, "buckets").await;
        let flow = create(&pool, ws, "Welcome").await.id;
        publish_at(&pool, ws, flow, START).await;
        // 2,500 people in two traces, waiting for the window that starts at the resume.
        let count = 2_500_i64;
        sqlx::query("INSERT INTO people (id, workspace_id) SELECT gen_random_uuid(), $1 FROM generate_series(1, $2)")
            .bind(ws.as_uuid())
            .bind(count)
            .execute(&pool)
            .await
            .unwrap();
        let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
        sqlx::query(
            "INSERT INTO flow_warmup_waiting (workspace_id, flow_id, person_id, occurrence_id, \
                 flow_version, trigger, admission_offset, trace_id, cause_kind, cause_id) \
             SELECT $1, $2, p.id, gen_random_uuid(), 1, 'manual', \
                    (9007199254740991 * random())::int8, \
                    CASE WHEN row_number() OVER () % 5 = 0 THEN $3 ELSE $4 END, 'api', 't' \
             FROM people p WHERE p.workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(second)
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();

        start(&mut tx, ws, flow, at(100)).await.unwrap();
        commit(tx).await.unwrap();
        let jobs = queued_jobs(&pool, flow).await;

        let people: usize = jobs.iter().map(|(pairs, ..)| pairs.len()).sum();
        assert_eq!(people, 2_500, "everyone is queued once");
        assert!(jobs.len() <= 2 * 300, "{} jobs", jobs.len());
        assert!(
            jobs.iter()
                .all(|(pairs, ..)| pairs.len() <= MAX_ENROLL_PEOPLE)
        );
        assert!(jobs.iter().any(|(pairs, ..)| pairs.len() > 1));
        for (trace, expected) in [(first, 2_000), (second, 500)] {
            let carried: usize = jobs
                .iter()
                .filter(|(_, _, job_trace)| *job_trace == trace)
                .map(|(pairs, ..)| pairs.len())
                .sum();
            assert_eq!(carried, expected, "a job carries one trace");
        }
        // A job runs at the latest admission time of its people, inside the window.
        let window = (at(100), at(400));
        assert!(
            jobs.iter()
                .all(|(_, run_at, _)| *run_at >= window.0 && *run_at < window.1)
        );
        let mut again = begin(&pool).await.unwrap();
        start(&mut again, ws, flow, at(100)).await.unwrap();
        commit(again).await.unwrap();
        assert_eq!(
            queued_jobs(&pool, flow).await.len(),
            jobs.len(),
            "the same window queues the same jobs"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn waiting_entries_of_one_second_are_split_into_jobs_of_at_most_the_enroll_limit(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one-slot").await;
        let flow = create(&pool, ws, "Welcome").await.id;
        publish_at(&pool, ws, flow, START).await;
        // 2,500 entries of one trace, all admitted at the start of the window: one second.
        let count = 2_500_i64;
        sqlx::query("INSERT INTO people (id, workspace_id) SELECT gen_random_uuid(), $1 FROM generate_series(1, $2)")
            .bind(ws.as_uuid())
            .bind(count)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO flow_warmup_waiting (workspace_id, flow_id, person_id, occurrence_id, \
                 flow_version, trigger, admission_offset, trace_id, cause_kind, cause_id) \
             SELECT $1, $2, p.id, gen_random_uuid(), 1, 'manual', 0, $3, 'api', 't' \
             FROM people p WHERE p.workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();

        start(&mut tx, ws, flow, at(100)).await.unwrap();
        commit(tx).await.unwrap();
        let jobs = queued_jobs(&pool, flow).await;

        let mut sizes: Vec<usize> = jobs.iter().map(|(pairs, ..)| pairs.len()).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [500, MAX_ENROLL_PEOPLE, MAX_ENROLL_PEOPLE]);
        let mut people: Vec<Uuid> = jobs
            .iter()
            .flat_map(|(pairs, ..)| pairs.iter().map(|(person, _)| *person))
            .collect();
        people.sort_unstable();
        people.dedup();
        assert_eq!(people.len(), 2_500, "each person is in exactly one job");
        let mut again = begin(&pool).await.unwrap();
        start(&mut again, ws, flow, at(100)).await.unwrap();
        commit(again).await.unwrap();
        let repeated = queued_jobs(&pool, flow).await;
        let carried: usize = repeated.iter().map(|(pairs, ..)| pairs.len()).sum();
        assert_eq!((repeated.len(), carried), (3, 2_500), "no extra jobs");
    }
}
