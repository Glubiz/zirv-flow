//! The rows of the flow runtime (architecture 9.2 and 9.3): enrollments with their
//! `state_version`, the step executions that make a step idempotent, the event waits, and the
//! payload of the `flow.step` job. Every function runs in the caller's transaction.
//!
//! The runtime in `zirv-flow-jobs` locks an enrollment, computes a transition with the pure
//! engine and writes it through [`save_enrollment`]; the compare-and-set on `state_version` is the
//! rule that the first commit wins.

use serde_json::{Value, json};
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use crate::people::lock_people_with_zones;
use crate::subtransactions::{begin_savepoint, release_savepoint, roll_back_savepoint};
use crate::{NewJob, StoreError};

/// The kind of the job that advances an enrollment.
pub const STEP_JOB_KIND: &str = "flow.step";
/// The kind of the job a webhook action enqueues; its handler is the webhook action's own.
pub const WEBHOOK_JOB_KIND: &str = "webhook.call";
/// The kind of the job a profile sync action enqueues (architecture 10.2).
pub const SYNC_JOB_KIND: &str = "delivery.sync";

/// Where an enrollment is in its life. A yield at a branch is never stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentStatus {
    /// Ready to run its node.
    Active,
    /// Asleep at a wait node.
    Waiting,
    /// Its step fell due while the flow was paused; it runs again when the flow resumes.
    Held,
    Completed,
    Exited,
    /// Stopped at its node until an operator retries or skips it.
    Failed,
}

impl EnrollmentStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Waiting => "waiting",
            Self::Held => "held",
            Self::Completed => "completed",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Active,
            Self::Waiting,
            Self::Held,
            Self::Completed,
            Self::Exited,
            Self::Failed,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
    }

    /// Whether the enrollment has ended for good.
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Completed | Self::Exited)
    }
}

/// An enrollment as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Enrollment {
    pub id: Uuid,
    pub flow_id: Uuid,
    /// The version the person entered by; it never changes.
    pub flow_version: i32,
    pub person_id: Uuid,
    pub status: EnrollmentStatus,
    pub node: String,
    pub context: Value,
    pub wake_at: Option<OffsetDateTime>,
    pub state_version: i64,
    /// The event a `wait.until_event` is waiting for, from its wait row.
    pub awaiting: Option<String>,
    pub exit_reason: Option<String>,
    pub error: Option<String>,
    /// The node a failure stopped at; `node` is where the failed step started.
    pub failed_node: Option<String>,
    /// Whether the engine placed the failure at `failed_node` (a node it was walking), not just
    /// stored the enrollment's own node for a failure it could not attribute (a missing pinned
    /// version, a database error on the last attempt). Only a placed failure can be skipped.
    pub failure_placed: bool,
    /// The `flow.step` input a held enrollment runs again on resume, and the input of the attempt
    /// that failed for a failed one (none for a failure the engine reported), in the form of
    /// [`StepJobInput::to_stored`].
    pub resume_input: Option<Value>,
    /// The trace of this enrollment's journey and the cause of the trigger that enrolled it.
    pub trace: TraceContext,
    pub created_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct EnrollmentRow {
    id: Uuid,
    flow_id: Uuid,
    flow_version: i32,
    person_id: Uuid,
    state: String,
    node: String,
    context: Value,
    wake_at: Option<OffsetDateTime>,
    state_version: i64,
    awaiting: Option<String>,
    exit_reason: Option<String>,
    error: Option<String>,
    failed_node: Option<String>,
    failure_placed: bool,
    resume_input: Option<Value>,
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
    created_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
}

/// An enrollment as its lock reads it, with the workspace's time zone.
#[derive(FromRow)]
struct LockedRow {
    #[sqlx(flatten)]
    enrollment: EnrollmentRow,
    workspace_zone: String,
}

impl EnrollmentRow {
    fn into_enrollment(self) -> Result<Enrollment, StoreError> {
        let decode = |what: &'static str| StoreError::query(sqlx::Error::Decode(what.into()));
        let status = EnrollmentStatus::parse(&self.state).ok_or_else(|| decode("unknown state"))?;
        let kind: CauseKind = self
            .cause_kind
            .parse()
            .map_err(|_| decode("unknown cause kind"))?;
        Ok(Enrollment {
            id: self.id,
            flow_id: self.flow_id,
            flow_version: self.flow_version,
            person_id: self.person_id,
            status,
            node: self.node,
            context: self.context,
            wake_at: self.wake_at,
            state_version: self.state_version,
            awaiting: self.awaiting,
            exit_reason: self.exit_reason,
            error: self.error,
            failed_node: self.failed_node,
            failure_placed: self.failure_placed,
            resume_input: self.resume_input,
            trace: TraceContext {
                trace_id: TraceId::from_uuid(self.trace_id),
                cause: Cause::from_stored(kind, self.cause_id),
            },
            created_at: self.created_at,
            ended_at: self.ended_at,
        })
    }
}

const ENROLLMENT_COLUMNS: &str = "e.id, e.flow_id, e.flow_version, e.person_id, e.state, e.node, \
     e.context, e.wake_at, e.state_version, \
     (SELECT w.event_name FROM enrollment_waits w WHERE w.enrollment_id = e.id LIMIT 1) \
         AS awaiting, \
     e.exit_reason, e.error, e.failed_node, e.failure_placed, e.resume_input, e.trace_id, e.cause_kind, e.cause_id, e.created_at, e.ended_at";

/// The stored columns [`ENROLLMENT_COLUMNS`] reads, and the workspace.
const ENROLLMENT_ROW: &str = "id, workspace_id, flow_id, flow_version, person_id, state, node, \
     context, wake_at, state_version, exit_reason, error, failed_node, failure_placed, \
     resume_input, trace_id, cause_kind, cause_id, created_at, ended_at";

/// The read before an enrollment's lock: `$1` is the workspace, `$2` the enrollment. The
/// enrollment and its person are found by primary key and the workspace is checked on the rows
/// found, so the plan never walks `enrollments_person_id_idx` or `people_workspace_idx` (the
/// person of another workspace reads as not merged; its lock then finds nobody).
const ENROLLMENT_READ: &str = "WITH found AS MATERIALIZED ( \
         SELECT workspace_id, flow_id, person_id FROM enrollments WHERE id = $2) \
     SELECT f.flow_id, f.person_id, \
         CASE WHEN p.workspace_id = f.workspace_id THEN p.merged_into END \
     FROM found f LEFT JOIN people p ON p.id = f.person_id \
     WHERE f.workspace_id = $1";

/// [`ENROLLMENT_READ`] for many enrollments at once: `$1` is the workspace, `$2` the enrollments.
const ENROLLMENTS_READ: &str = "WITH found AS MATERIALIZED ( \
         SELECT id, workspace_id, flow_id, person_id FROM enrollments WHERE id = ANY($2)) \
     SELECT f.id, f.flow_id, f.person_id, \
         CASE WHEN p.workspace_id = f.workspace_id THEN p.merged_into END \
     FROM found f LEFT JOIN people p ON p.id = f.person_id \
     WHERE f.workspace_id = $1";

/// The enrollment's lock in a round of [`lock_enrollment`]: `$1` is the workspace, `$2` the
/// enrollment and `$3` the person the round locked. The row is locked by primary key and checked
/// after, so the plan never walks `enrollments_person_id_idx`. The workspace's time zone is read
/// here, after the step's flow and person locks.
///
/// A wait keeps the zone it was scheduled with: changing a workspace's zone re-arms nothing.
/// This statement's snapshot predates its own wait for the enrollment lock, so a zone change
/// committed during that wait is not seen. The step then orders before the change: it read
/// the old workspace row and the change reads nothing the step writes, so the outcome is the
/// one a step committed just before the change would have had.
fn enrollment_lock() -> String {
    format!(
        "WITH locked AS MATERIALIZED ( \
             SELECT {ENROLLMENT_ROW} FROM enrollments WHERE id = $2 FOR UPDATE) \
         SELECT {ENROLLMENT_COLUMNS}, ws.timezone AS workspace_zone \
         FROM locked e JOIN workspaces ws ON ws.id = e.workspace_id \
         WHERE e.workspace_id = $1 AND e.person_id = $3"
    )
}

/// How many times [`lock_enrollment`] re-reads an enrollment whose person kept changing under it
/// before it gives the step back as a retryable race.
const LOCK_ROUNDS: usize = 8;

/// What is read of an enrollment, without a lock, before it is locked: the flow a step locks
/// first, and the person and the one they were merged into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnrollmentRead {
    pub flow_id: Uuid,
    person_id: Uuid,
    merged_into: Option<Uuid>,
}

impl EnrollmentRead {
    /// Where a step of this enrollment comes in the person lock order: the lowest id of the group
    /// its lock takes first (the person and the one they were merged into), then the person.
    pub fn lock_order(&self) -> (Uuid, Uuid) {
        let survivor = self.merged_into.unwrap_or(self.person_id);
        (self.person_id.min(survivor), self.person_id)
    }
}

/// An enrollment a step locked, with the zones it runs in, both read after its locks.
#[derive(Clone, Debug, PartialEq)]
pub struct StepEnrollment {
    pub enrollment: Enrollment,
    /// The workspace's time zone.
    pub workspace_zone: String,
    /// The person's `timezone` trait as written (it may not be a zone; the engine falls back
    /// from an invalid one), read under the person's lock.
    pub person_zone: Option<String>,
}

/// Reads an enrollment's flow and person without a lock, for [`lock_read_enrollment`]. An
/// enrollment that is gone, or of another workspace, reads as missing.
pub async fn read_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<EnrollmentRead>, StoreError> {
    let row: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(ENROLLMENT_READ)
        .bind(workspace.as_uuid())
        .bind(id)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(row.map(|(flow_id, person_id, merged_into)| EnrollmentRead {
        flow_id,
        person_id,
        merged_into,
    }))
}

/// [`read_enrollment`] for many enrollments in one statement, for the steps that run in one
/// transaction (#397), by the enrollments' and the people's primary keys as that read is. An
/// enrollment that is gone, or of another workspace, is absent.
pub async fn read_enrollments(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, EnrollmentRead)>, StoreError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(Uuid, Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(ENROLLMENTS_READ)
        .bind(workspace.as_uuid())
        .bind(ids)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(id, flow_id, person_id, merged_into)| {
            (
                id,
                EnrollmentRead {
                    flow_id,
                    person_id,
                    merged_into,
                },
            )
        })
        .collect())
}

/// [`lock_enrollment`] for a step that read the enrollment with [`read_enrollment`] before it
/// locked the enrollment's flow: the first round locks the person that read found instead of
/// reading again. Each round checks under its locks what the read found, so a read that went
/// stale while the flow's lock was waited for costs a round, as a merge in any round does.
pub async fn lock_read_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    read: EnrollmentRead,
) -> Result<Option<StepEnrollment>, StoreError> {
    lock_enrollment_within(connection, workspace, id, Some(read), LOCK_ROUNDS).await
}

/// Locks an enrollment `FOR UPDATE` and reads it. Two steps of one enrollment run one after the
/// other: the second reads the state the first committed.
///
/// A step that may write people takes the shared segment guard before calling. Row lock order:
/// the person first (`FOR UPDATE`, the strength a trait write's recompute needs,
/// so it is never upgraded), then the enrollment. Person erasure locks the person before it
/// deletes the enrollment and the jobs, so a step and an erasure never wait for each other in a
/// cycle, and an erasure that waits for a step sees the jobs the step enqueued.
///
/// A merged-away person's enrollment (one a merge left, or one enrolled for the loser just
/// before the merge committed) takes the survivor's lock too, in id order with the person's: a
/// trait or list effect of the step follows `merged_into` and locks the survivor, and erasures,
/// merges and ingestion lock a merged group in id order, so taking both here, lowest id first,
/// keeps every one of them on the same order.
///
/// The person is found with an unlocked read and locked in a statement of its own (a join does
/// not fix the order rows are locked in), inside a savepoint. If a merge or a repoint committed
/// while the lock was waited for, the group or the enrollment's person is no longer the one
/// read: the round is rolled back and its savepoint released, which releases its locks and gives
/// back its subtransaction id (`roll_back_savepoint`), and the enrollment is
/// read again and the new group locked from nothing, in id order, rather than the step dropped
/// or the new group (a survivor can have a lower id) locked after the old one. An enrollment
/// that is gone (erased) reads as missing.
///
/// The enrollment is locked by primary key and its workspace and person are checked on the
/// locked row; a row that fails the check is held only until its round rolls back. The locks are
/// the caller's transaction's: `connection` must be in one.
pub async fn lock_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Enrollment>, StoreError> {
    let locked = lock_enrollment_within(connection, workspace, id, None, LOCK_ROUNDS).await?;
    Ok(locked.map(|locked| locked.enrollment))
}

/// [`lock_enrollment`] with its number of rounds given, starting from `read` when the caller
/// has one; a retryable race when the rounds run out. Returns the zones beside the enrollment.
async fn lock_enrollment_within(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    mut read: Option<EnrollmentRead>,
    rounds: usize,
) -> Result<Option<StepEnrollment>, StoreError> {
    for _ in 0..rounds {
        let found = match read.take() {
            Some(read) => read,
            None => match read_enrollment(&mut *connection, workspace, id).await? {
                Some(read) => read,
                None => return Ok(None),
            },
        };
        let person = found.person_id;
        let survivor = found.merged_into.unwrap_or(person);
        // A savepoint for the round: rolling it back releases the round's locks, and gives back
        // its subtransaction id, so a round lost to a merge costs the transaction none.
        begin_savepoint(connection, ROUND).await?;
        match lock_round(connection, workspace, id, person, survivor).await {
            Ok(Round::Erased) => {
                release_savepoint(connection, ROUND).await?;
                return Ok(None);
            }
            Ok(Round::Locked(row, zone)) => {
                release_savepoint(connection, ROUND).await?;
                return Ok(Some(StepEnrollment {
                    enrollment: row.enrollment.into_enrollment()?,
                    workspace_zone: row.workspace_zone,
                    person_zone: zone,
                }));
            }
            // Let go of this round's locks and read which in the next.
            Ok(Round::Moved) => roll_back_savepoint(connection, ROUND).await?,
            Err(error) => {
                // As a dropped nested transaction would, so the connection stays usable; the
                // round's own error is the one that counts.
                let _ = roll_back_savepoint(connection, ROUND).await;
                return Err(error);
            }
        }
    }
    Err(StoreError::retryable_race())
}

/// The savepoint of a round of [`lock_enrollment_within`].
const ROUND: &str = "enrollment_round";

/// What a round of [`lock_enrollment_within`] found under its locks.
enum Round {
    /// The enrollment, locked behind its person's group, and the person's `timezone` trait.
    Locked(Box<LockedRow>, Option<String>),
    /// The person is gone (erased), and the enrollment with it.
    Erased,
    /// The group changed while its locks were waited for, or the enrollment moved to another
    /// person, or it is gone.
    Moved,
}

/// Locks the group of `person` (and `survivor`, the person it was read merged into) in id order,
/// then the enrollment, and checks under the locks what the read found.
async fn lock_round(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    person: Uuid,
    survivor: Uuid,
) -> Result<Round, StoreError> {
    let locked = lock_people_with_zones(&mut *connection, workspace, &[person, survivor]).await?;
    let Some((_, now_merged, zone)) = locked.into_iter().find(|(locked, ..)| *locked == person)
    else {
        // Erased: the enrollment went with the person.
        return Ok(Round::Erased);
    };
    if now_merged.unwrap_or(person) != survivor {
        // Merged (or merged again) while the lock was waited for.
        return Ok(Round::Moved);
    }
    let row = sqlx::query_as::<_, LockedRow>(sqlx::AssertSqlSafe(enrollment_lock()))
        .bind(workspace.as_uuid())
        .bind(id)
        .bind(person)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(row.map_or(Round::Moved, |row| Round::Locked(Box::new(row), zone)))
}

/// Reads an enrollment without locking it.
pub async fn find_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Enrollment>, StoreError> {
    let row: Option<EnrollmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ENROLLMENT_COLUMNS} FROM enrollments e WHERE e.workspace_id = $1 AND e.id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(EnrollmentRow::into_enrollment).transpose()
}

/// The enrollments of a flow that have not ended and so stay on the version they entered when a
/// new one is published (ruling 469cfc51): active, waiting and failed ones.
pub async fn count_in_flight_enrollments(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT count(*) FROM enrollments \
         WHERE workspace_id = $1 AND flow_id = $2 AND state IN ('active', 'waiting', 'held', 'failed')",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// The longest one of the live-enrollment sampler's statements may run.
const SAMPLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Up to this many enrollments in a state are counted; above it the count is an estimate.
const SAMPLE_EXACT: i64 = 10_000;

/// The `active` and the `waiting` enrollments, each read from `enrollments_live_idx` up to `$1`
/// of them. The states are literals, so the plan always proves the index's predicate.
const SAMPLE_COUNTS: &str = "SELECT \
        (SELECT count(*) FROM (SELECT 1 FROM enrollments WHERE state = 'active' \
                               ORDER BY wake_at LIMIT $1) a), \
        (SELECT count(*) FROM (SELECT 1 FROM enrollments WHERE state = 'waiting' \
                               ORDER BY wake_at LIMIT $1) w)";

/// The planner's estimate of the enrollments in a state, for a count above [`SAMPLE_EXACT`].
const SAMPLE_ESTIMATE: &str = "SELECT 1 FROM enrollments WHERE state = $1";

/// What the leader samples of the live enrollments, over all workspaces.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EnrollmentSample {
    pub active: i64,
    pub waiting: i64,
    /// How long the oldest `waiting` enrollment has been past its `wake_at`, in seconds; 0 when
    /// no timer is overdue.
    pub oldest_late_seconds: i64,
}

/// Counts the `active` and `waiting` enrollments and finds the oldest overdue timer, from
/// `enrollments_live_idx`, under a statement timeout of a second. Each state is counted up to
/// 10,000 enrollments; above that its count is the planner's estimate from the table
/// statistics, never below 10,001, so millions of waiting rows cost no more to sample than ten
/// thousand do (#397). The oldest overdue timer is the first entry of the `waiting` range.
pub async fn sample_enrollments(pool: &PgPool) -> Result<EnrollmentSample, StoreError> {
    sample_up_to(pool, SAMPLE_EXACT).await
}

async fn sample_up_to(pool: &PgPool, exact: i64) -> Result<EnrollmentSample, StoreError> {
    let mut tx = crate::bounded::begin_bounded_for(pool, SAMPLE_TIMEOUT).await?;
    // Planned for the limit's value each time, never with a generic plan.
    let (active, waiting): (i64, i64) = sqlx::query_as(SAMPLE_COUNTS)
        .persistent(false)
        .bind(exact + 1)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    let active =
        crate::bounded::count_or_estimate(&mut tx, active, exact, SAMPLE_ESTIMATE, &["active"])
            .await?;
    let waiting =
        crate::bounded::count_or_estimate(&mut tx, waiting, exact, SAMPLE_ESTIMATE, &["waiting"])
            .await?;
    let late: Option<i64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM now() - min(wake_at))::bigint FROM enrollments          WHERE state = 'waiting' AND wake_at <= now()",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    tx.commit().await.map_err(StoreError::query)?;
    Ok(EnrollmentSample {
        active,
        waiting,
        oldest_late_seconds: late.unwrap_or(0).max(0),
    })
}

/// The time zones a step reads: the workspace's, and the person's `timezone` trait as written
/// (it may not be a zone; the engine falls back from an invalid one).
pub async fn step_zones(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<(String, Option<String>), StoreError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT w.timezone, p.traits ->> 'timezone' FROM workspaces w \n         LEFT JOIN people p ON p.workspace_id = w.id AND p.id = $2 WHERE w.id = $1",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.unwrap_or_else(|| ("UTC".to_owned(), None)))
}

/// The state an enrollment moves to.
#[derive(Clone, Debug)]
pub struct EnrollmentUpdate<'a> {
    pub status: EnrollmentStatus,
    pub node: &'a str,
    /// The new `state_version`; the stored one must be `expected_version`.
    pub state_version: i64,
    pub wake_at: Option<OffsetDateTime>,
    pub exit_reason: Option<&'static str>,
    pub error: Option<&'a str>,
    /// The node a failure stopped at; set exactly when `error` is.
    pub failed_node: Option<&'a str>,
    /// Whether the engine placed the failure at `failed_node`; false in every other state.
    pub failure_placed: bool,
    /// The input a failed enrollment stopped on, for an operator retry; null in every other
    /// state (see [`Enrollment::resume_input`]).
    pub resume_input: Option<&'a Value>,
    pub now: OffsetDateTime,
}

/// Moves an enrollment to `next` if its `state_version` is still `expected_version`. Returns
/// whether it did: `false` means another transaction committed first and nothing was written.
pub async fn save_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    expected_version: i64,
    next: &EnrollmentUpdate<'_>,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE enrollments SET state = $4, node = $5, state_version = $6, wake_at = $7, \
             exit_reason = $8, error = $9, failed_node = $11, failure_placed = $12, resume_input = $13, \
             updated_at = now(), \
             ended_at = CASE WHEN $4 IN ('completed', 'exited') THEN $10 ELSE NULL END \
         WHERE workspace_id = $1 AND id = $2 AND state_version = $3",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(expected_version)
    .bind(next.status.as_str())
    .bind(next.node)
    .bind(next.state_version)
    .bind(next.wake_at)
    .bind(next.exit_reason)
    .bind(next.error)
    .bind(next.now)
    .bind(next.failed_node)
    .bind(next.failure_placed)
    .bind(next.resume_input)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// How a step ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    Done,
    /// A wait node: the enrollment is asleep at it.
    Waiting,
    Failed,
}

impl StepOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Waiting => "waiting",
            Self::Failed => "failed",
        }
    }
}

/// A node an enrollment ran in a step.
#[derive(Clone, Debug)]
pub struct NewStepExecution<'a> {
    pub node_id: &'a str,
    pub outcome: StepOutcome,
    pub error: Option<&'a str>,
    /// The attempt of the job that ran it.
    pub attempt: i32,
}

/// A step execution row and whether this call wrote it. Only the call that wrote it applies the
/// step's effects, so a step that ran before (a redelivered job after a partial commit cannot
/// exist, but a retried failure can) never applies them twice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StepRecord {
    pub id: Uuid,
    pub fresh: bool,
}

/// Records the steps of one transition. A node that has a row keeps it.
pub async fn record_steps(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    steps: &[NewStepExecution<'_>],
) -> Result<Vec<StepRecord>, StoreError> {
    let mut records = Vec::with_capacity(steps.len());
    for step in steps {
        let (id, inserted): (Uuid, bool) = sqlx::query_as(
            "INSERT INTO step_executions \
                (id, workspace_id, enrollment_id, node_id, outcome, error, attempts) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (enrollment_id, node_id) DO UPDATE SET attempts = step_executions.attempts \
             RETURNING id, (xmax = 0)",
        )
        .bind(Uuid::now_v7())
        .bind(workspace.as_uuid())
        .bind(enrollment)
        .bind(step.node_id)
        .bind(step.outcome.as_str())
        .bind(step.error)
        .bind(step.attempt)
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        records.push(StepRecord {
            id,
            fresh: inserted,
        });
    }
    Ok(records)
}

/// A wait node ends when its timer or event wakes the enrollment.
pub async fn settle_wait_step(
    connection: &mut PgConnection,
    enrollment: Uuid,
    node: &str,
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar(
        "UPDATE step_executions SET outcome = 'done' \
         WHERE enrollment_id = $1 AND node_id = $2 AND outcome = 'waiting' RETURNING id",
    )
    .bind(enrollment)
    .bind(node)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// A wait to register.
#[derive(Clone, Debug)]
pub struct NewWait<'a> {
    pub person: Uuid,
    pub event_name: &'a str,
    /// The property filters in the segment AST's wire form.
    pub filters: &'a [Value],
    /// The `state_version` the enrollment has after the step that registers this wait.
    pub wake_version: i64,
    pub timeout_at: OffsetDateTime,
}

/// Registers the wait of an enrollment, replacing any it had for the event.
pub async fn register_wait(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    wait: &NewWait<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO enrollment_waits \
            (workspace_id, enrollment_id, person_id, event_name, filters, wake_version, timeout_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (enrollment_id, event_name) DO UPDATE SET \
             filters = EXCLUDED.filters, wake_version = EXCLUDED.wake_version, \
             timeout_at = EXCLUDED.timeout_at",
    )
    .bind(workspace.as_uuid())
    .bind(enrollment)
    .bind(wait.person)
    .bind(wait.event_name)
    .bind(Value::Array(wait.filters.to_vec()))
    .bind(wait.wake_version)
    .bind(wait.timeout_at)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Removes the wait of an enrollment for an event; with no name, every wait it has.
pub async fn clear_waits(
    connection: &mut PgConnection,
    enrollment: Uuid,
    event_name: Option<&str>,
) -> Result<(), StoreError> {
    sqlx::query(
        "DELETE FROM enrollment_waits \
         WHERE enrollment_id = $1 AND ($2::text IS NULL OR event_name = $2)",
    )
    .bind(enrollment)
    .bind(event_name)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The property filters of the wait an enrollment has for an event.
pub async fn wait_filters(
    connection: &mut PgConnection,
    enrollment: Uuid,
    event_name: &str,
) -> Result<Option<Vec<Value>>, StoreError> {
    let filters: Option<Value> = sqlx::query_scalar(
        "SELECT filters FROM enrollment_waits WHERE enrollment_id = $1 AND event_name = $2",
    )
    .bind(enrollment)
    .bind(event_name)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(filters.map(|filters| filters.as_array().cloned().unwrap_or_default()))
}

/// What a `flow.step` job asks of the enrollment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepJobInput {
    /// Run the first node.
    Start,
    /// The timer of the wait the enrollment sleeps at is due.
    Wake,
    /// An event arrived that the enrollment's wait names.
    Event { event_id: Uuid, name: String },
    /// An operator retries (`skip: false`) or skips (`skip: true`) the node a failed enrollment
    /// stopped at.
    Resume { skip: bool },
}

impl StepJobInput {
    /// The input as a payload and a stored `resume_input` carry it: `{"input": "start"}`,
    /// `{"input": "wake"}`, `{"input": "event", "event_id": .., "event": ..}` or
    /// `{"input": "resume", "skip": ..}`.
    pub fn to_stored(&self) -> Value {
        match self {
            Self::Start => json!({"input": "start"}),
            Self::Wake => json!({"input": "wake"}),
            Self::Event { event_id, name } => {
                json!({"input": "event", "event_id": event_id, "event": name})
            }
            Self::Resume { skip } => json!({"input": "resume", "skip": skip}),
        }
    }

    /// The input of a payload or a stored `resume_input`.
    pub fn from_stored(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        Some(match text("input")? {
            "start" => Self::Start,
            "wake" => Self::Wake,
            "event" => Self::Event {
                event_id: Uuid::parse_str(text("event_id")?).ok()?,
                name: text("event")?.to_owned(),
            },
            "resume" => Self::Resume {
                skip: value.get("skip")?.as_bool()?,
            },
            _ => return None,
        })
    }

    /// The status an enrollment that stopped on this input was in before: `start` runs an active
    /// enrollment, the rest a waiting one.
    pub const fn status_before(&self) -> EnrollmentStatus {
        match self {
            Self::Start => EnrollmentStatus::Active,
            Self::Wake | Self::Event { .. } => EnrollmentStatus::Waiting,
            Self::Resume { .. } => EnrollmentStatus::Failed,
        }
    }
}

/// The payload of a `flow.step` job: which enrollment, in which state it expects to find it, and
/// what happened. A job whose `state_version` is no longer the enrollment's is stale.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepJobPayload {
    pub workspace: WorkspaceId,
    pub enrollment: Uuid,
    pub state_version: i64,
    pub input: StepJobInput,
}

impl StepJobPayload {
    pub fn to_value(&self) -> Value {
        let mut payload = json!({
            "workspace_id": self.workspace,
            "enrollment_id": self.enrollment,
            "state_version": self.state_version,
        });
        if let (Some(target), Value::Object(input)) =
            (payload.as_object_mut(), self.input.to_stored())
        {
            target.extend(input);
        }
        payload
    }

    pub fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            enrollment: Uuid::parse_str(text("enrollment_id")?).ok()?,
            state_version: payload.get("state_version")?.as_i64()?,
            input: StepJobInput::from_stored(payload)?,
        })
    }

    /// The job that carries this payload, under the enrollment's trace. `run_at` is when it
    /// becomes runnable (the database's now when absent) and `unique_key` makes a second
    /// enqueue of the same work a no-op.
    pub fn job(
        &self,
        trace: &TraceContext,
        run_at: Option<OffsetDateTime>,
        unique_key: String,
    ) -> NewJob {
        let mut job = NewJob::new(STEP_JOB_KIND, trace.clone());
        job.workspace_id = Some(self.workspace);
        job.payload = self.to_value();
        job.run_at = run_at;
        job.unique_key = Some(unique_key);
        job
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support::{create, publish, trace, workspace};

    #[test]
    fn the_text_of_a_status_round_trips() {
        for status in [
            EnrollmentStatus::Active,
            EnrollmentStatus::Waiting,
            EnrollmentStatus::Held,
            EnrollmentStatus::Completed,
            EnrollmentStatus::Exited,
            EnrollmentStatus::Failed,
        ] {
            assert_eq!(EnrollmentStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(EnrollmentStatus::parse("yielded"), None);
        assert!(EnrollmentStatus::Completed.is_final());
        assert!(!EnrollmentStatus::Failed.is_final());
    }

    #[test]
    fn a_step_payload_round_trips_for_every_input() {
        let workspace = WorkspaceId::from_uuid(Uuid::from_u128(1));
        for input in [
            StepJobInput::Start,
            StepJobInput::Wake,
            StepJobInput::Resume { skip: false },
            StepJobInput::Resume { skip: true },
            StepJobInput::Event {
                event_id: Uuid::from_u128(9),
                name: "order".into(),
            },
        ] {
            let payload = StepJobPayload {
                workspace,
                enrollment: Uuid::from_u128(2),
                state_version: 4,
                input,
            };
            assert_eq!(StepJobPayload::parse(&payload.to_value()), Some(payload));
        }
        assert_eq!(StepJobPayload::parse(&json!({"input": "start"})), None);
        assert_eq!(StepJobPayload::parse(&json!({"input": "dance"})), None);
    }

    #[test]
    fn a_step_job_carries_the_trace_the_key_and_the_time() {
        let payload = StepJobPayload {
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            enrollment: Uuid::from_u128(2),
            state_version: 0,
            input: StepJobInput::Start,
        };
        let trace = TraceContext::root(Cause::api("t"));
        let job = payload.job(&trace, None, "start:x".into());
        assert_eq!(job.kind, STEP_JOB_KIND);
        assert_eq!(job.unique_key.as_deref(), Some("start:x"));
        assert_eq!(job.trace, trace);
        assert_eq!(job.payload, payload.to_value());
    }

    /// An enrollment row for a published flow, written directly.
    async fn enrollment_row(pool: &PgPool, ws: WorkspaceId, flow: Uuid, person: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let trace = trace();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id) \
             VALUES ($1, $2, $3, 1, $4, 'active', 'draft', $5, $6, 'api', 't')",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(Uuid::now_v7())
        .bind(trace.trace_id.as_uuid())
        .execute(pool)
        .await
        .unwrap();
        id
    }

    /// An enrollment of a new person in `state`, with its timer `wake_in_seconds` from now.
    async fn timed_enrollment(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        state: &str,
        wake_in_seconds: Option<i32>,
    ) {
        let person = person(pool, ws).await;
        sqlx::query(
            "INSERT INTO enrollments                 (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id,                  trace_id, cause_kind, cause_id, wake_at, ended_at)              VALUES ($1, $2, $3, 1, $4, $5, 'draft', $6, $7, 'api', 't',                      now() + make_interval(secs => $8),                      CASE WHEN $5 IN ('completed', 'exited') THEN now() END)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(state)
        .bind(Uuid::now_v7())
        .bind(trace().trace_id.as_uuid())
        .bind(wake_in_seconds.map(f64::from))
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sample_counts_live_enrollments_and_finds_the_oldest_overdue_timer(pool: PgPool) {
        let ws = workspace(&pool, "sample").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();

        let none = sample_enrollments(&pool).await.unwrap();

        timed_enrollment(&pool, ws, flow.id, "active", None).await;
        timed_enrollment(&pool, ws, flow.id, "waiting", Some(3600)).await;
        timed_enrollment(&pool, ws, flow.id, "waiting", Some(-30)).await;
        timed_enrollment(&pool, ws, flow.id, "waiting", Some(-90)).await;
        timed_enrollment(&pool, ws, flow.id, "completed", Some(-500)).await;
        let seeded = sample_enrollments(&pool).await.unwrap();

        assert_eq!(none, EnrollmentSample::default());
        assert_eq!((seeded.active, seeded.waiting), (1, 3));
        assert!(
            (90..100).contains(&seeded.oldest_late_seconds),
            "{seeded:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sample_reports_no_lateness_while_every_timer_is_in_the_future(pool: PgPool) {
        let ws = workspace(&pool, "sample-future").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        timed_enrollment(&pool, ws, flow.id, "waiting", Some(120)).await;

        let sample = sample_enrollments(&pool).await.unwrap();

        assert_eq!(
            sample,
            EnrollmentSample {
                active: 0,
                waiting: 1,
                oldest_late_seconds: 0
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_state_above_the_exact_limit_is_sampled_from_the_planners_estimate(pool: PgPool) {
        let ws = workspace(&pool, "sample-estimate").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        for _ in 0..40 {
            timed_enrollment(&pool, ws, flow.id, "waiting", Some(-60)).await;
        }
        timed_enrollment(&pool, ws, flow.id, "active", None).await;
        sqlx::query("ANALYZE enrollments")
            .execute(&pool)
            .await
            .unwrap();

        let sample = sample_up_to(&pool, 5).await.unwrap();

        // ANALYZE reads every row of a table this small, so the estimate is the count; the
        // `active` state is under the limit and counted.
        assert_eq!((sample.active, sample.waiting), (1, 40));
        assert!((60..70).contains(&sample.oldest_late_seconds), "{sample:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_sample_gives_up_after_a_second(pool: PgPool) {
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE enrollments IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let started = std::time::Instant::now();

        let blocked = sample_enrollments(&pool).await;

        let waited = started.elapsed();
        blocker.rollback().await.unwrap();
        assert!(blocked.is_err());
        assert!(
            waited >= std::time::Duration::from_millis(900)
                && waited < std::time::Duration::from_secs(4),
            "{waited:?}"
        );
    }

    async fn person(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(id)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_save_applies_only_to_the_version_it_expects(pool: PgPool) {
        let ws = workspace(&pool, "runtime").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        let mut connection = pool.acquire().await.unwrap();
        let now = OffsetDateTime::now_utc();
        let update = |version, status| EnrollmentUpdate {
            status,
            node: "draft",
            state_version: version,
            wake_at: None,
            exit_reason: None,
            error: None,
            failed_node: None,
            failure_placed: false,
            resume_input: None,
            now,
        };

        let first = save_enrollment(
            &mut connection,
            ws,
            id,
            0,
            &update(1, EnrollmentStatus::Completed),
        )
        .await
        .unwrap();
        let second = save_enrollment(
            &mut connection,
            ws,
            id,
            0,
            &update(1, EnrollmentStatus::Waiting),
        )
        .await
        .unwrap();

        assert!(first);
        assert!(!second, "the second writer expected version 0");
        let stored = find_enrollment(&mut connection, ws, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, EnrollmentStatus::Completed);
        assert_eq!(stored.state_version, 1);
        assert!(stored.ended_at.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_is_fresh_once_and_a_wait_is_settled_once(pool: PgPool) {
        let ws = workspace(&pool, "steps").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        let mut connection = pool.acquire().await.unwrap();
        let step = NewStepExecution {
            node_id: "w",
            outcome: StepOutcome::Waiting,
            error: None,
            attempt: 1,
        };

        let first = record_steps(&mut connection, ws, id, std::slice::from_ref(&step))
            .await
            .unwrap();
        let again = record_steps(&mut connection, ws, id, std::slice::from_ref(&step))
            .await
            .unwrap();
        let settled = settle_wait_step(&mut connection, id, "w").await.unwrap();
        let settled_again = settle_wait_step(&mut connection, id, "w").await.unwrap();

        assert!(first[0].fresh && !again[0].fresh);
        assert_eq!(first[0].id, again[0].id);
        assert_eq!(settled, Some(first[0].id));
        assert_eq!(settled_again, None);
    }

    /// No index holds `outcome` (#397), so the settle of a wait whose page has room, as this one
    /// has, is a HOT update and writes no index entry. On a full page it is not: the table keeps
    /// the default fillfactor, and a bench-shaped run measured 59% of settles HOT.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn settling_a_wait_step_on_a_page_with_room_is_a_hot_update(pool: PgPool) {
        let ws = workspace(&pool, "hot-settle").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        let mut tx = crate::begin(&pool).await.unwrap();
        let step = NewStepExecution {
            node_id: "w",
            outcome: StepOutcome::Waiting,
            error: None,
            attempt: 1,
        };
        record_steps(&mut tx, ws, id, std::slice::from_ref(&step))
            .await
            .unwrap();

        let settled = settle_wait_step(&mut tx, id, "w").await.unwrap();

        // This transaction's own counts, so no other test's writes are in them.
        let (updated, hot): (i64, i64) = sqlx::query_as(
            "SELECT pg_stat_get_xact_tuples_updated('step_executions'::regclass), \
                    pg_stat_get_xact_tuples_hot_updated('step_executions'::regclass)",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(settled.is_some());
        assert_eq!((updated, hot), (1, 1), "the settle wrote index entries");
    }

    /// The indexes of `step_executions` by name, and how many of them hold `outcome`.
    async fn step_indexes(pool: &PgPool) -> (Vec<String>, i64) {
        let names = sqlx::query_scalar(
            "SELECT indexname::text FROM pg_indexes WHERE tablename = 'step_executions' \
             ORDER BY indexname",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let holding: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_index i JOIN pg_attribute a \
               ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey::int2[]) \
             WHERE i.indrelid = 'step_executions'::regclass AND a.attname = 'outcome'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        (names, holding)
    }

    const STEP_INDEXES: [&str; 3] = [
        "step_executions_id_key",
        "step_executions_pkey",
        "step_executions_started_idx",
    ];

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_index_of_the_step_rows_holds_the_outcome(pool: PgPool) {
        let (names, holding) = step_indexes(&pool).await;

        assert_eq!(names, STEP_INDEXES);
        assert_eq!(holding, 0);
    }

    /// The definitions of the person indexes of `enrollments`, the old and the new.
    async fn person_indexes(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'enrollments' \
               AND indexname LIKE 'enrollments_person%' ORDER BY indexname",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    const PERSON_INDEXES: [&str; 1] =
        ["CREATE INDEX enrollments_person_id_idx ON public.enrollments USING btree (person_id)"];

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migration_0090_accepts_the_indexes_changed_by_hand(pool: PgPool) {
        crate::reset_database(&pool).await;
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 90)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        for statement in [
            "CREATE INDEX step_executions_started_idx \
                 ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id)",
            "DROP INDEX step_executions_workspace_node_started_idx",
            "DROP INDEX step_executions_workspace_idx",
            "CREATE INDEX enrollments_person_id_idx ON enrollments (person_id)",
            "DROP INDEX enrollments_person_idx",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        crate::migrate(&pool).await.unwrap();

        let (names, holding) = step_indexes(&pool).await;
        assert_eq!(names, STEP_INDEXES);
        assert_eq!(holding, 0);
        assert_eq!(person_indexes(&pool).await, PERSON_INDEXES);
    }

    /// The blocks of `enrollments` and of its indexes this transaction has read.
    async fn enrollment_blocks(connection: &mut PgConnection) -> i64 {
        sqlx::query_scalar(
            "SELECT sum(pg_stat_get_xact_blocks_fetched(oid))::bigint FROM pg_class \
             WHERE oid = 'enrollments'::regclass \
                OR oid IN (SELECT indexrelid FROM pg_index WHERE indrelid = 'enrollments'::regclass)",
        )
        .fetch_one(&mut *connection)
        .await
        .unwrap()
    }

    /// A person's delete finds the enrollments it cascades to by `person_id` (#510), so a person
    /// with none reads a few index blocks and not the table or a whole index.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_delete_finds_the_enrollments_it_cascades_to_by_index(pool: PgPool) {
        let ws = workspace(&pool, "person-delete").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        sqlx::query(
            "INSERT INTO people (id, workspace_id) \
             SELECT gen_random_uuid(), $1 FROM generate_series(1, 20000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, $2, 1, id, 'waiting', 'w', gen_random_uuid(), \
                 gen_random_uuid(), 'api', 't' \
             FROM people WHERE workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .bind(flow.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE people, enrollments")
            .execute(&pool)
            .await
            .unwrap();
        let (first, lonely) = (person(&pool, ws).await, person(&pool, ws).await);
        let delete = "DELETE FROM people WHERE workspace_id = $1 AND id = $2";
        let mut tx = crate::begin(&pool).await.unwrap();
        // The first delete plans the cascade, which reads the metapage of every index once.
        sqlx::query(delete)
            .bind(ws.as_uuid())
            .bind(first)
            .execute(&mut *tx)
            .await
            .unwrap();
        // This transaction's own counts, so no other test's reads are in them.
        let before = enrollment_blocks(&mut tx).await;

        let deleted = sqlx::query(delete)
            .bind(ws.as_uuid())
            .bind(lonely)
            .execute(&mut *tx)
            .await
            .unwrap();

        let read = enrollment_blocks(&mut tx).await - before;
        assert_eq!(deleted.rows_affected(), 1);
        assert!(read < 10, "the delete read {read} blocks of enrollments");
        assert_eq!(person_indexes(&pool).await, PERSON_INDEXES);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn waits_are_registered_replaced_found_and_cleared(pool: PgPool) {
        let ws = workspace(&pool, "waits").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        let mut connection = pool.acquire().await.unwrap();
        let filter = json!({"prop": "total", "op": "gt", "value": 5});
        let wait = |version| NewWait {
            person,
            event_name: "order",
            filters: std::slice::from_ref(&filter),
            wake_version: version,
            timeout_at: OffsetDateTime::now_utc(),
        };

        register_wait(&mut connection, ws, id, &wait(1))
            .await
            .unwrap();
        register_wait(&mut connection, ws, id, &wait(3))
            .await
            .unwrap();
        let filters = wait_filters(&mut connection, id, "order").await.unwrap();
        let version: i64 = sqlx::query_scalar("SELECT wake_version FROM enrollment_waits")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        let awaiting = find_enrollment(&mut connection, ws, id)
            .await
            .unwrap()
            .unwrap()
            .awaiting;
        clear_waits(&mut connection, id, Some("other"))
            .await
            .unwrap();
        let kept = wait_filters(&mut connection, id, "order").await.unwrap();
        clear_waits(&mut connection, id, None).await.unwrap();

        assert_eq!(filters, Some(vec![filter]));
        assert_eq!(version, 3);
        assert_eq!(awaiting.as_deref(), Some("order"));
        assert!(kept.is_some());
        assert_eq!(
            wait_filters(&mut connection, id, "order").await.unwrap(),
            None
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_zones_are_the_workspaces_and_the_persons_trait_as_written(pool: PgPool) {
        let ws = workspace(&pool, "zones").await;
        let person = person(&pool, ws).await;
        let mut connection = pool.acquire().await.unwrap();
        let plain = step_zones(&mut connection, ws, person).await.unwrap();
        sqlx::query("UPDATE workspaces SET timezone = 'Europe/Copenhagen' WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET traits = '{\"timezone\": \"Not/AZone\"}' WHERE id = $1")
            .bind(person)
            .execute(&pool)
            .await
            .unwrap();

        let set = step_zones(&mut connection, ws, person).await.unwrap();

        assert_eq!(plain, ("UTC".to_owned(), None));
        assert_eq!(
            set,
            ("Europe/Copenhagen".to_owned(), Some("Not/AZone".to_owned()))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_in_flight_count_skips_ended_enrollments(pool: PgPool) {
        let ws = workspace(&pool, "count").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let (one, two) = (person(&pool, ws).await, person(&pool, ws).await);
        let kept = enrollment_row(&pool, ws, flow.id, one).await;
        let ended = enrollment_row(&pool, ws, flow.id, two).await;
        // A transaction: the enrollment's lock is taken in one.
        let mut connection = pool.begin().await.unwrap();
        save_enrollment(
            &mut connection,
            ws,
            ended,
            0,
            &EnrollmentUpdate {
                status: EnrollmentStatus::Exited,
                node: "draft",
                state_version: 1,
                wake_at: None,
                exit_reason: Some("goal"),
                error: None,
                failed_node: None,
                failure_placed: false,
                resume_input: None,
                now: OffsetDateTime::now_utc(),
            },
        )
        .await
        .unwrap();

        let count = count_in_flight_enrollments(&mut connection, ws, flow.id)
            .await
            .unwrap();

        assert_eq!(count, 1);
        assert!(
            lock_enrollment(&mut connection, ws, kept)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasure_waits_for_a_running_step_and_deletes_the_job_it_enqueued(pool: PgPool) {
        let ws = workspace(&pool, "lock-order").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let enrollment = enrollment_row(&pool, ws, flow.id, person).await;

        // A step holds the enrollment (and, by the lock order, the person) and has enqueued its
        // next job, uncommitted.
        let mut step = pool.begin().await.unwrap();
        lock_enrollment(&mut step, ws, enrollment)
            .await
            .unwrap()
            .unwrap();
        let payload = StepJobPayload {
            workspace: ws,
            enrollment,
            state_version: 1,
            input: StepJobInput::Wake,
        };
        let key = format!("wake:{enrollment}:1");
        crate::enqueue(&mut *step, &payload.job(&trace(), None, key))
            .await
            .unwrap();

        let erasing = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                let deleted = crate::delete_person(
                    &mut tx,
                    ws,
                    person,
                    crate::DeleteReason::Erasure,
                    &trace(),
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
                deleted
            })
        };
        // Erasure must be blocked on the step's lock, not finished, before the step commits.
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            assert!(!erasing.is_finished(), "erasure finished without waiting");
            tokio::task::yield_now().await;
        }
        step.commit().await.unwrap();
        assert!(erasing.await.unwrap());

        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'flow.step' AND payload ->> 'enrollment_id' = $1",
        )
        .bind(enrollment.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(left, 0, "the job the step enqueued survived the erasure");
    }

    /// Waits until a backend of this test's database waits for a lock.
    async fn a_backend_waits_for_a_lock(pool: &PgPool) {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_lock_re_reads_an_enrollment_whose_person_changed_instead_of_dropping_the_step(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "reread").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let (first, second) = (person(&pool, ws).await, person(&pool, ws).await);
        let enrollment = enrollment_row(&pool, ws, flow.id, first).await;

        // A holds the first person, so the step reads the enrollment and waits for the lock.
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(first)
            .execute(&mut *holder)
            .await
            .unwrap();
        let step = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                lock_enrollment(&mut tx, ws, enrollment).await.unwrap()
            })
        };
        a_backend_waits_for_a_lock(&pool).await;
        sqlx::query("UPDATE enrollments SET person_id = $2 WHERE id = $1")
            .bind(enrollment)
            .bind(second)
            .execute(&mut *holder)
            .await
            .unwrap();
        holder.commit().await.unwrap();

        let found = step.await.unwrap().expect("the step is not dropped");
        assert_eq!((found.id, found.person_id), (enrollment, second));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_on_a_merged_away_person_and_an_erasure_of_the_survivor_do_not_deadlock(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "merged-step").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        // The survivor's id is the lower one: an erasure takes it first.
        let survivor = person(&pool, ws).await;
        let loser = person(&pool, ws).await;
        assert!(survivor < loser);
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();
        let enrollment = enrollment_row(&pool, ws, flow.id, loser).await;

        let mut step = pool.begin().await.unwrap();
        lock_enrollment(&mut step, ws, enrollment)
            .await
            .unwrap()
            .expect("the enrollment is found");
        let erasing = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                crate::delete_person(
                    &mut tx,
                    ws,
                    survivor,
                    crate::DeleteReason::Erasure,
                    &trace(),
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            })
        };
        a_backend_waits_for_a_lock(&pool).await;

        // The step's trait write follows `merged_into` and locks the survivor.
        let write = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(survivor)
            .execute(&mut *step)
            .await;

        assert!(
            write.is_ok(),
            "the step deadlocked with the erasure: {write:?}"
        );
        step.commit().await.unwrap();
        erasing.await.expect("the erasure finished");
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM people")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    /// Waits until the backend `pid` is blocked by the backend `by`.
    async fn wait_until_blocked_by(pool: &PgPool, pid: i32, by: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(pid)
                .bind(by)
                .fetch_one(pool)
                .await
                .unwrap();
            if blocked {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the step never waited for the lock"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_lock_that_finds_the_person_merged_releases_its_lock_before_locking_the_survivor(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "out-of-order").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        // The survivor has the lower id, so the group's order is survivor, then person.
        let survivor = person(&pool, ws).await;
        let loser = person(&pool, ws).await;
        assert!(survivor < loser);
        let enrollment = enrollment_row(&pool, ws, flow.id, loser).await;

        // A merge is under way: it holds the person and points them at the survivor.
        let mut merge = pool.begin().await.unwrap();
        let merge_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *merge)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(loser)
            .execute(&mut *merge)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&mut *merge)
            .await
            .unwrap();
        // Another transaction holds the survivor (a strength that lets the merge's foreign key in).
        let mut holder = pool.begin().await.unwrap();
        let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR NO KEY UPDATE")
            .bind(survivor)
            .execute(&mut *holder)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let step_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let step = tokio::spawn(async move {
            let found = lock_enrollment(&mut tx, ws, enrollment).await.unwrap();
            tx.commit().await.unwrap();
            found
        });
        // The step waits for the person first, then, the merge committed, for the survivor.
        wait_until_blocked_by(&pool, step_pid, merge_pid).await;
        merge.commit().await.unwrap();
        wait_until_blocked_by(&pool, step_pid, holder_pid).await;

        // While it waits for the survivor, the step holds nothing of the person's.
        let free = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(loser)
            .fetch_one(&pool)
            .await;
        assert!(
            free.is_ok(),
            "the step kept the person's lock while it waited for a lower id: {free:?}"
        );
        holder.commit().await.unwrap();
        let found = step.await.unwrap().expect("the step is not dropped");
        assert_eq!((found.id, found.person_id), (enrollment, loser));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_lock_whose_rounds_run_out_gives_the_step_back_as_a_retryable_race(pool: PgPool) {
        let ws = workspace(&pool, "rounds").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let survivor = person(&pool, ws).await;
        let loser = person(&pool, ws).await;
        let enrollment = enrollment_row(&pool, ws, flow.id, loser).await;

        let mut merge = pool.begin().await.unwrap();
        let merge_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *merge)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(loser)
            .execute(&mut *merge)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let step_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        // One round only: the merge that lands while the step waits uses it up.
        let step =
            tokio::spawn(
                async move { lock_enrollment_within(&mut tx, ws, enrollment, None, 1).await },
            );
        wait_until_blocked_by(&pool, step_pid, merge_pid).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&mut *merge)
            .await
            .unwrap();
        merge.commit().await.unwrap();

        let error = step.await.unwrap().expect_err("the rounds ran out");
        assert!(error.is_retryable());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_lock_returns_the_zones_and_an_enrollment_of_another_workspace_reads_as_missing(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "step-lock").await;
        let other = workspace(&pool, "foreign").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        sqlx::query("UPDATE workspaces SET timezone = 'Europe/Copenhagen' WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET traits = '{\"timezone\": \"Not/AZone\"}' WHERE id = $1")
            .bind(person)
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();

        let foreign = read_enrollment(&mut tx, other, id).await.unwrap();
        let read = read_enrollment(&mut tx, ws, id).await.unwrap().unwrap();
        let read_flow = read.flow_id;
        let locked = lock_read_enrollment(&mut tx, ws, id, read)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(foreign, None);
        assert_eq!(read_flow, flow.id);
        assert_eq!(
            (locked.enrollment.id, locked.enrollment.person_id),
            (id, person)
        );
        assert_eq!(locked.workspace_zone, "Europe/Copenhagen");
        assert_eq!(locked.person_zone.as_deref(), Some("Not/AZone"));
        for (table, row) in [("people", person), ("enrollments", id)] {
            let free = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM {table} WHERE id = $1 FOR UPDATE NOWAIT"
            )))
            .bind(row)
            .fetch_one(&pool)
            .await;
            assert!(free.is_err(), "the step does not hold its {table} row");
        }
    }

    #[test]
    fn a_step_locks_at_the_lowest_id_of_its_group_then_at_its_person() {
        let (low, high) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let read = |person_id, merged_into| EnrollmentRead {
            flow_id: Uuid::nil(),
            person_id,
            merged_into,
        };

        assert_eq!(read(high, None).lock_order(), (high, high));
        assert_eq!(read(high, Some(low)).lock_order(), (low, high));
        assert_eq!(read(low, Some(high)).lock_order(), (low, low));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_read_of_many_enrollments_finds_what_the_read_of_each_finds(pool: PgPool) {
        let ws = workspace(&pool, "batch-read").await;
        let other = workspace(&pool, "batch-foreign").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let foreign_flow = create(&pool, other, "Foreign").await;
        publish(&pool, other, foreign_flow.id).await.unwrap();
        let survivor = person(&pool, ws).await;
        let loser = person(&pool, ws).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();
        let plain = enrollment_row(&pool, ws, flow.id, survivor).await;
        let merged = enrollment_row(&pool, ws, flow.id, loser).await;
        let foreign_person = person(&pool, other).await;
        let foreign = enrollment_row(&pool, other, foreign_flow.id, foreign_person).await;
        let mut tx = pool.begin().await.unwrap();

        let mut found = read_enrollments(&mut tx, ws, &[merged, foreign, plain, Uuid::now_v7()])
            .await
            .unwrap();

        found.sort_by_key(|(id, _)| *id);
        let mut each = Vec::new();
        for id in [plain, merged] {
            each.push((id, read_enrollment(&mut tx, ws, id).await.unwrap().unwrap()));
        }
        each.sort_by_key(|(id, _)| *id);
        assert_eq!(found, each);
        assert!(read_enrollments(&mut tx, ws, &[]).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_lock_from_a_read_that_went_stale_locks_the_group_it_finds_under_the_lock(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "stale-read").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let survivor = person(&pool, ws).await;
        let loser = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, loser).await;
        let mut tx = pool.begin().await.unwrap();
        let read = read_enrollment(&mut tx, ws, id).await.unwrap().unwrap();
        // A merge commits between the step's read and its lock.
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();

        let locked = lock_read_enrollment(&mut tx, ws, id, read)
            .await
            .unwrap()
            .expect("the step is not dropped");

        assert_eq!(
            (locked.enrollment.id, locked.enrollment.person_id),
            (id, loser)
        );
        let free = sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(survivor)
            .fetch_one(&pool)
            .await;
        assert!(free.is_err(), "the step does not hold the survivor");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_lock_reads_the_workspace_zone_committed_after_the_steps_read(pool: PgPool) {
        let ws = workspace(&pool, "zone-race").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let person = person(&pool, ws).await;
        let id = enrollment_row(&pool, ws, flow.id, person).await;
        let mut tx = pool.begin().await.unwrap();
        let read = read_enrollment(&mut tx, ws, id).await.unwrap().unwrap();
        // An Admin's new zone commits between the step's read and its locks.
        sqlx::query("UPDATE workspaces SET timezone = 'Europe/Copenhagen' WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        let locked = lock_read_enrollment(&mut tx, ws, id, read)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(locked.workspace_zone, "Europe/Copenhagen");
    }

    /// The plans of `sql` for `binds`: the one planned for the values, and the generic one a
    /// prepared statement settles on after its fifth execution.
    async fn plans_of(pool: &PgPool, sql: &str, binds: &[Uuid]) -> String {
        let binds: Vec<Vec<Uuid>> = binds.iter().map(|bind| vec![*bind]).collect();
        plans_with(pool, sql, &binds, false).await
    }

    /// [`plans_of`] for a statement whose last parameter is an array of `ids`, after `ws`.
    async fn plans_of_many(pool: &PgPool, sql: &str, ws: Uuid, ids: &[Uuid]) -> String {
        plans_with(pool, sql, &[vec![ws], ids.to_vec()], true).await
    }

    /// The plans of `sql`, each bind one value, or an array when `last_is_array` says so of the last.
    async fn plans_with(
        pool: &PgPool,
        sql: &str,
        binds: &[Vec<Uuid>],
        last_is_array: bool,
    ) -> String {
        let mut connection = pool.acquire().await.unwrap();
        let mut custom = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (COSTS OFF) {sql}"
        )));
        for (index, bind) in binds.iter().enumerate() {
            custom = if last_is_array && index + 1 == binds.len() {
                custom.bind(bind.clone())
            } else {
                custom.bind(bind[0])
            };
        }
        let custom = custom.fetch_all(&mut *connection).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "SET plan_cache_mode = force_generic_plan; PREPARE planned AS {sql}"
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        let values: Vec<String> = binds
            .iter()
            .enumerate()
            .map(|(index, bind)| {
                let text: Vec<String> = bind.iter().map(Uuid::to_string).collect();
                if last_is_array && index + 1 == binds.len() {
                    format!("'{{{}}}'", text.join(","))
                } else {
                    format!("'{}'", text.join(","))
                }
            })
            .collect();
        let generic: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (COSTS OFF) EXECUTE planned({})",
            values.join(", ")
        )))
        .fetch_all(&mut *connection)
        .await
        .unwrap();
        sqlx::raw_sql("DEALLOCATE planned; RESET plan_cache_mode")
            .execute(&mut *connection)
            .await
            .unwrap();
        format!("{}\n--- generic\n{}", custom.join("\n"), generic.join("\n"))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_step_reads_and_locks_its_enrollment_and_person_by_primary_key(pool: PgPool) {
        // Tables without statistics first, as after a restore or before their first analyze.
        sqlx::raw_sql(
            "ALTER TABLE people SET (autovacuum_enabled = false); \
             ALTER TABLE enrollments SET (autovacuum_enabled = false)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let ours = workspace(&pool, "ours").await;
        let other = workspace(&pool, "other").await;
        for ws in [ours, other] {
            let flow = create(&pool, ws, "Flow").await;
            publish(&pool, ws, flow.id).await.unwrap();
            sqlx::query(
                "INSERT INTO people (id, workspace_id) \
                 SELECT gen_random_uuid(), $1 FROM generate_series(1, 20000)",
            )
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO enrollments \
                    (id, workspace_id, flow_id, flow_version, person_id, state, node, \
                     occurrence_id, trace_id, cause_kind, cause_id) \
                 SELECT gen_random_uuid(), $1, $2, 1, id, 'waiting', 'w', gen_random_uuid(), \
                     gen_random_uuid(), 'api', 't' \
                 FROM people WHERE workspace_id = $1",
            )
            .bind(ws.as_uuid())
            .bind(flow.id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let (enrollment, person): (Uuid, Uuid) =
            sqlx::query_as("SELECT id, person_id FROM enrollments WHERE workspace_id = $1 LIMIT 1")
                .bind(ours.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        // A wake transaction reads the enrollments of the eight jobs a slot claims.
        let batch: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM enrollments WHERE workspace_id = $1 LIMIT 8")
                .bind(ours.as_uuid())
                .fetch_all(&pool)
                .await
                .unwrap();
        let ws = ours.as_uuid();

        // The bench's plans took the other indexes; at this size the planner picks either for
        // the old statements, so only the current ones are checked, with and without statistics.
        let mut plans = Vec::new();
        for analysed in [false, true] {
            if analysed {
                sqlx::query("ANALYZE people, enrollments")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            plans.push((
                plans_of(&pool, ENROLLMENT_READ, &[ws, enrollment]).await,
                plans_of(&pool, &enrollment_lock(), &[ws, enrollment, person]).await,
                plans_of_many(&pool, ENROLLMENTS_READ, ws, &batch).await,
            ));
        }

        for (read, lock, batch_read) in &plans {
            assert!(read.contains("enrollments_pkey"), "{read}");
            assert!(read.contains("people_pkey"), "{read}");
            assert!(lock.contains("enrollments_pkey"), "{lock}");
            assert!(batch_read.contains("enrollments_pkey"), "{batch_read}");
            assert!(batch_read.contains("people_pkey"), "{batch_read}");
            for plan in [read, lock, batch_read] {
                for path in [
                    "people_workspace_idx",
                    "enrollments_person_id_idx",
                    "Seq Scan on people",
                    "Seq Scan on enrollments",
                ] {
                    assert!(!plan.contains(path), "{path}\n{plan}");
                }
            }
        }
    }
}
