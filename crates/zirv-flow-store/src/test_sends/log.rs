//! The test-send log: one row per test send, reserved under the rate limit, ended by the job.

use serde_json::{Value, json};
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::{NewJob, StoreError, enqueue};

/// The job kind that carries a test send out.
pub const TEST_SEND_JOB_KIND: &str = "template.test_send";

/// The most test sends one member may start in any rolling hour.
pub const MEMBER_HOURLY_LIMIT: usize = 10;
/// The most test sends one workspace may start in any rolling hour.
pub const WORKSPACE_HOURLY_LIMIT: usize = 100;
/// The window both limits count in.
const WINDOW: Duration = Duration::hours(1);

/// How long the log is kept: the proposed message retention (architecture 16.3).
pub const TEST_SEND_RETENTION_MONTHS: i32 = 13;

/// Rows deleted per statement by the lifecycle deletes.
const BATCH: i64 = 5_000;
/// Rows one abandon sweep ends.
const ABANDON_BATCH: i64 = 100;

/// How a test send ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestSendOutcome {
    Queued,
    Sent,
    Suppressed,
    SenderUnverified,
    RenderFailed,
    Unconfirmed,
    Failed,
}

impl TestSendOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Suppressed => "suppressed",
            Self::SenderUnverified => "sender_unverified",
            Self::RenderFailed => "render_failed",
            Self::Unconfirmed => "unconfirmed",
            Self::Failed => "failed",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        Ok(match text {
            "queued" => Self::Queued,
            "sent" => Self::Sent,
            "suppressed" => Self::Suppressed,
            "sender_unverified" => Self::SenderUnverified,
            "render_failed" => Self::RenderFailed,
            "unconfirmed" => Self::Unconfirmed,
            "failed" => Self::Failed,
            _ => {
                return Err(StoreError::query(sqlx::Error::Decode(
                    "unknown test-send outcome".into(),
                )));
            }
        })
    }
}

/// A log row. It holds no address; the recipient hash is for the job and the lifecycle deletes and
/// is never shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestSend {
    pub id: Uuid,
    pub template_id: Uuid,
    pub template_version: Option<i32>,
    /// What the job rendered with, set when it renders: the theme in effect, its version and the
    /// snippet versions by snippet id.
    pub theme_id: Option<Uuid>,
    pub theme_version: Option<i32>,
    pub snippet_versions: Option<Value>,
    pub member_id: Option<Uuid>,
    pub destination_id: Uuid,
    pub sender_identity_id: Uuid,
    pub recipient_hash: Vec<u8>,
    pub recipient_member_id: Option<Uuid>,
    /// `person` or `sample`.
    pub render_context: String,
    /// The draft revision a draft test rendered; none for a version test.
    pub draft_revision: Option<i32>,
    pub outcome: TestSendOutcome,
    pub reason: Option<String>,
    pub created_at: OffsetDateTime,
    pub completed_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct Row {
    id: Uuid,
    template_id: Uuid,
    template_version: Option<i32>,
    theme_id: Option<Uuid>,
    theme_version: Option<i32>,
    snippet_versions: Option<Value>,
    member_id: Option<Uuid>,
    destination_id: Uuid,
    sender_identity_id: Uuid,
    recipient_hash: Vec<u8>,
    recipient_member_id: Option<Uuid>,
    render_context: String,
    draft_revision: Option<i32>,
    outcome: String,
    reason: Option<String>,
    created_at: OffsetDateTime,
    completed_at: Option<OffsetDateTime>,
}

impl Row {
    fn into_send(self) -> Result<TestSend, StoreError> {
        Ok(TestSend {
            id: self.id,
            template_id: self.template_id,
            template_version: self.template_version,
            theme_id: self.theme_id,
            theme_version: self.theme_version,
            snippet_versions: self.snippet_versions,
            member_id: self.member_id,
            destination_id: self.destination_id,
            sender_identity_id: self.sender_identity_id,
            recipient_hash: self.recipient_hash,
            recipient_member_id: self.recipient_member_id,
            render_context: self.render_context,
            draft_revision: self.draft_revision,
            outcome: TestSendOutcome::parse(&self.outcome)?,
            reason: self.reason,
            created_at: self.created_at,
            completed_at: self.completed_at,
        })
    }
}

const COLUMNS: &str = "id, template_id, template_version, theme_id, theme_version, snippet_versions, \n    member_id, destination_id, sender_identity_id, recipient_hash, recipient_member_id, \n    render_context, draft_revision, outcome, reason, created_at, completed_at";

/// [`COLUMNS`] qualified for the joined list query.
const LISTED_COLUMNS: &str = "t.id, t.template_id, t.template_version, t.theme_id, t.theme_version, \n    t.snippet_versions, t.member_id, t.destination_id, t.sender_identity_id, t.recipient_hash, \n    t.recipient_member_id, t.render_context, t.draft_revision, t.outcome, t.reason, t.created_at, \n    t.completed_at";

/// A test send to reserve a slot for. It carries no address: the job finds the recipient by the
/// row's hash.
pub struct NewTestSend<'a> {
    pub id: Uuid,
    pub workspace: WorkspaceId,
    pub template: Uuid,
    pub template_version: Option<i32>,
    /// The user who asked; none for an API key.
    pub member: Option<Uuid>,
    pub destination: Uuid,
    pub sender_identity: Uuid,
    pub recipient_hash: [u8; 32],
    pub recipient_member: Option<Uuid>,
    /// The draft revision a draft test renders; none for a version test.
    pub draft_revision: Option<i32>,
    /// The person whose values render the template; none for the sample. Travels in the job
    /// payload only, so the log names no person.
    pub person: Option<Uuid>,
    pub now: OffsetDateTime,
    pub trace: &'a TraceContext,
}

/// What [`reserve_test_send`] came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestSendReservation {
    /// The row is written `queued` and its job is enqueued.
    Reserved(Box<TestSend>),
    /// A limit is reached; a slot frees `retry_after` seconds from now.
    Limited { retry_after: u64 },
}

/// Counts the log rows in the rolling hour and, under the limits, inserts the new `queued` row
/// and enqueues its job. Run it in a transaction: it takes a transaction advisory lock keyed by the
/// workspace, so two requests for the last slot cannot both take it. The row is the reservation;
/// the job does not count again.
pub async fn reserve_test_send(
    connection: &mut PgConnection,
    new: &NewTestSend<'_>,
) -> Result<TestSendReservation, StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('test_sends/' || $1::text, 0))")
        .bind(new.workspace.as_uuid())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let (in_workspace, by_member) =
        counted_in_window(&mut *connection, new.workspace, new.member, new.now).await?;
    let waits = [
        wait_for(&by_member, MEMBER_HOURLY_LIMIT, new.now),
        wait_for(&in_workspace, WORKSPACE_HOURLY_LIMIT, new.now),
    ];
    if let Some(retry_after) = waits.into_iter().flatten().max() {
        return Ok(TestSendReservation::Limited { retry_after });
    }

    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO test_sends (id, workspace_id, template_id, template_version, member_id, \
            destination_id, sender_identity_id, recipient_hash, recipient_member_id, \
            render_context, draft_revision, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING {COLUMNS}"
    )))
    .bind(new.id)
    .bind(new.workspace.as_uuid())
    .bind(new.template)
    .bind(new.template_version)
    .bind(new.member)
    .bind(new.destination)
    .bind(new.sender_identity)
    .bind(new.recipient_hash.as_slice())
    .bind(new.recipient_member)
    .bind(if new.person.is_some() {
        "person"
    } else {
        "sample"
    })
    .bind(new.draft_revision)
    .bind(new.now)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut job = NewJob::new(TEST_SEND_JOB_KIND, new.trace.clone());
    job.workspace_id = Some(new.workspace);
    job.unique_key = Some(format!("{TEST_SEND_JOB_KIND}:{}", new.id));
    job.payload = json!({
        "workspace_id": new.workspace.as_uuid(),
        "test_send_id": new.id,
        "person_id": new.person,
    });
    enqueue(&mut *connection, &job).await?;
    row.into_send()
        .map(|send| TestSendReservation::Reserved(Box::new(send)))
}

/// The creation times, oldest first, of the sends inside the rolling hour before `now`: the
/// workspace's, then the member's (empty without a member). Enforcement and the quota both count
/// with this, so they cannot disagree.
async fn counted_in_window(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    member: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<(Vec<OffsetDateTime>, Vec<OffsetDateTime>), StoreError> {
    let since = now - WINDOW;
    let in_workspace: Vec<OffsetDateTime> = sqlx::query_scalar(
        "SELECT created_at FROM test_sends WHERE workspace_id = $1 AND created_at > $2 \
         ORDER BY created_at",
    )
    .bind(workspace.as_uuid())
    .bind(since)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let by_member: Vec<OffsetDateTime> = match member {
        Some(member) => sqlx::query_scalar(
            "SELECT created_at FROM test_sends \
             WHERE workspace_id = $1 AND member_id = $2 AND created_at > $3 ORDER BY created_at",
        )
        .bind(workspace.as_uuid())
        .bind(member)
        .bind(since)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?,
        None => Vec::new(),
    };
    Ok((in_workspace, by_member))
}

/// Test sends counted in the rolling hour against a limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaUsage {
    pub used: usize,
    pub limit: usize,
}

/// What a caller may still send this hour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TestSendQuota {
    pub member: QuotaUsage,
    pub workspace: QuotaUsage,
    /// When the limit that blocks the caller frees a slot (the later one when both do), as the
    /// reservation's refusal computes it. None when the caller can send now.
    pub resets_at: Option<OffsetDateTime>,
}

/// The caller's and the workspace's hourly counts as of `now`.
pub async fn test_send_quota(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    member: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<TestSendQuota, StoreError> {
    let (in_workspace, by_member) = counted_in_window(connection, workspace, member, now).await?;
    Ok(TestSendQuota {
        member: QuotaUsage {
            used: by_member.len(),
            limit: MEMBER_HOURLY_LIMIT,
        },
        workspace: QuotaUsage {
            used: in_workspace.len(),
            limit: WORKSPACE_HOURLY_LIMIT,
        },
        resets_at: [
            frees_at(&by_member, MEMBER_HOURLY_LIMIT),
            frees_at(&in_workspace, WORKSPACE_HOURLY_LIMIT),
        ]
        .into_iter()
        .flatten()
        .max(),
    })
}

/// When a slot frees, when `counted` (oldest first, all inside the window) fills `limit`: the
/// `counted.len() - limit + 1`-th oldest row leaves the window an hour after it was made.
fn frees_at(counted: &[OffsetDateTime], limit: usize) -> Option<OffsetDateTime> {
    (counted.len() >= limit).then(|| counted[counted.len() - limit] + WINDOW)
}

/// Seconds until a slot frees, when `counted` (oldest first, all inside the window) fills `limit`:
/// the slot of the `counted.len() - limit + 1`-th oldest row, which leaves the window an hour
/// after it was made. At least one second.
fn wait_for(counted: &[OffsetDateTime], limit: usize, now: OffsetDateTime) -> Option<u64> {
    let frees = frees_at(counted, limit)?;
    let seconds = (frees - now).as_seconds_f64().ceil();
    Some(if seconds < 1.0 { 1 } else { seconds as u64 })
}

/// One row of the workspace.
pub async fn find_test_send(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<TestSend>, StoreError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM test_sends WHERE workspace_id = $1 AND id = $2"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(Row::into_send).transpose()
}

/// Whether the call may still be made: the row exists and is `queued`, and, for a render of a
/// real person, that person still exists. One statement, run right before the destination call,
/// so an erasure that committed while the job rendered is seen; the erasure deletes the person
/// and the row of their address hash in one transaction.
pub async fn test_send_may_go(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    person: Option<Uuid>,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM test_sends \
                        WHERE workspace_id = $1 AND id = $2 AND outcome = 'queued') \
            AND ($3::uuid IS NULL \
                 OR EXISTS (SELECT 1 FROM people WHERE workspace_id = $1 AND id = $3))",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(person)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// Whether a test send has this id. The unsubscribe routes ask it of a token whose message does
/// not exist.
pub async fn is_test_send(pool: &PgPool, id: Uuid) -> Result<bool, StoreError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM test_sends WHERE id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
        .map_err(StoreError::query)
}

/// Where the next page starts: strictly before this `(created_at, id)`.
pub type TestSendCursor = (OffsetDateTime, Uuid);

/// A log row with the two names the log shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedTestSend {
    pub send: TestSend,
    /// A member's email, an allowlist entry's hint, or [`REMOVED_ADDRESS`].
    pub recipient_display: String,
    /// The sender member's email; none for an API key or a removed member.
    pub sent_by_email: Option<String>,
}

/// What the log shows for a recipient that is no member and matches no allowlist entry.
pub const REMOVED_ADDRESS: &str = "removed address";

#[derive(FromRow)]
struct ListedRow {
    #[sqlx(flatten)]
    row: Row,
    recipient_display: String,
    sent_by_email: Option<String>,
}

/// A page of the workspace's log, newest first, at most `limit`, after the `before` cursor,
/// optionally of one template and of the draft revision a draft test rendered. The flag is true
/// when more exist. The recipient shown is derived now: the member's email, else the hint of the
/// allowlist entry with the row's hash, else [`REMOVED_ADDRESS`].
pub async fn list_test_sends(
    pool: &PgPool,
    workspace: WorkspaceId,
    template: Option<Uuid>,
    draft_revision: Option<i32>,
    limit: u32,
    before: Option<TestSendCursor>,
) -> Result<(Vec<ListedTestSend>, bool), StoreError> {
    let mut rows: Vec<ListedRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LISTED_COLUMNS}, \
            COALESCE(rm.email, a.display_hint, '{REMOVED_ADDRESS}') AS recipient_display, \
            sm.email AS sent_by_email \
         FROM test_sends t \
         LEFT JOIN users rm ON rm.id = t.recipient_member_id \
         LEFT JOIN test_send_allowlist a \
            ON a.workspace_id = t.workspace_id AND a.address_hash = t.recipient_hash \
         LEFT JOIN users sm ON sm.id = t.member_id \
         WHERE t.workspace_id = $1 AND ($2::uuid IS NULL OR t.template_id = $2) \
           AND ($3::int IS NULL OR t.draft_revision = $3) \
           AND ($4::timestamptz IS NULL OR (t.created_at, t.id) < ($4, $5)) \
         ORDER BY t.created_at DESC, t.id DESC LIMIT $6"
    )))
    .bind(workspace.as_uuid())
    .bind(template)
    .bind(draft_revision)
    .bind(before.map(|(at, _)| at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let sends = rows
        .into_iter()
        .map(|listed| {
            Ok(ListedTestSend {
                send: listed.row.into_send()?,
                recipient_display: listed.recipient_display,
                sent_by_email: listed.sent_by_email,
            })
        })
        .collect::<Result<_, StoreError>>()?;
    Ok((sends, more))
}

/// Records the draft revision a job rendered, on its `queued` draft row. A draft saved between
/// the request and the job makes it differ from the one the request saw.
pub async fn set_test_send_draft_revision(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    revision: i32,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE test_sends SET draft_revision = $3 \
         WHERE workspace_id = $1 AND id = $2 AND outcome = 'queued' \
           AND template_version IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(revision)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Ends a `queued` row. False when the row is not `queued` any more (an earlier run ended it).
pub async fn finish_test_send(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    outcome: TestSendOutcome,
    reason: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE test_sends SET outcome = $3, reason = $4, completed_at = $5 \
         WHERE workspace_id = $1 AND id = $2 AND outcome = 'queued'",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(outcome.as_str())
    .bind(reason)
    .bind(now)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() > 0)
}

/// Records what a `queued` test send was rendered with: the theme in effect and the version of
/// each snippet by snippet id (an object). A test send writes no `messages` row, so this is where
/// its versions live. False when the row is not `queued` any more.
pub async fn record_test_send_versions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    theme: Option<(Uuid, i32)>,
    snippet_versions: &Value,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE test_sends SET theme_id = $3, theme_version = $4, snippet_versions = $5          WHERE workspace_id = $1 AND id = $2 AND outcome = 'queued'",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(theme.map(|(theme, _)| theme))
    .bind(theme.map(|(_, version)| version))
    .bind(snippet_versions)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() > 0)
}

/// The reason of a row whose job is dead or gone.
pub const ABANDONED: &str = "abandoned";

/// Ends the `queued` rows that have no live `template.test_send` job: a job the reaper buried or
/// that was deleted never runs again, so nothing else would end its row. Each ends `failed:
/// abandoned`. A `ready` or `running` job counts as live (a running one even with an expired
/// lease, until the reaper buries it), and the job is found by the test send's id in its payload,
/// which a deferral copy carries. Returns the ids, at most one batch.
pub async fn abandon_orphaned_test_sends(
    connection: &mut PgConnection,
    now: OffsetDateTime,
) -> Result<Vec<Uuid>, StoreError> {
    sqlx::query_scalar(
        "UPDATE test_sends t SET outcome = 'failed', reason = $1, completed_at = $2 \
         WHERE t.id IN ( \
            SELECT s.id FROM test_sends s WHERE s.outcome = 'queued' AND NOT EXISTS ( \
                SELECT 1 FROM jobs j WHERE j.kind = $3 AND j.state IN ('ready', 'running') \
                AND j.payload ->> 'test_send_id' = s.id::text) \
            ORDER BY s.created_at, s.id LIMIT $4 FOR UPDATE OF s SKIP LOCKED) \
         AND t.outcome = 'queued' RETURNING t.id",
    )
    .bind(ABANDONED)
    .bind(now)
    .bind(TEST_SEND_JOB_KIND)
    .bind(ABANDON_BATCH)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// Deletes the rows older than the retention as of `now`, committing each batch on the pool;
/// returns how many.
pub async fn delete_test_sends_before(
    pool: &PgPool,
    now: OffsetDateTime,
    months: u32,
) -> Result<u64, StoreError> {
    delete_test_sends_in_batches(pool, now, months, BATCH).await
}

async fn delete_test_sends_in_batches(
    pool: &PgPool,
    now: OffsetDateTime,
    months: u32,
    batch_size: i64,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM test_sends WHERE id IN ( \
                SELECT id FROM test_sends \
                WHERE created_at < $1::timestamptz - make_interval(months => $2) LIMIT $3)",
        )
        .bind(now)
        .bind(i32::try_from(months).unwrap_or(i32::MAX))
        .bind(batch_size)
        .execute(pool)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        total += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < batch_size {
            return Ok(total);
        }
    }
}

/// Deletes the workspace's rows whose recipient hash is one of `hashes` (an erased person's).
pub async fn delete_test_sends_by_hash(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    hashes: &[Vec<u8>],
) -> Result<u64, StoreError> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let done =
        sqlx::query("DELETE FROM test_sends WHERE workspace_id = $1 AND recipient_hash = ANY($2)")
            .bind(workspace.as_uuid())
            .bind(hashes)
            .execute(connection)
            .await
            .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use zirv_flow_core::{
        Cause, Keyring, MasterKey, SecretId, SecretPurpose, Slug, WorkspaceName, seal, secret_aad,
    };

    use super::*;
    use crate::test_sends::allowlist::{add_allowlist_entry, delete_allowlist_entry, masked_hint};
    use crate::{begin, commit, secrets};

    #[test]
    fn an_outcome_round_trips_and_an_unknown_one_is_refused() {
        for outcome in [
            TestSendOutcome::Queued,
            TestSendOutcome::Sent,
            TestSendOutcome::Suppressed,
            TestSendOutcome::SenderUnverified,
            TestSendOutcome::RenderFailed,
            TestSendOutcome::Unconfirmed,
            TestSendOutcome::Failed,
        ] {
            assert_eq!(TestSendOutcome::parse(outcome.as_str()).unwrap(), outcome);
        }
        assert!(TestSendOutcome::parse("gone").is_err());
    }

    #[test]
    fn a_full_window_waits_for_the_oldest_row_to_leave_it() {
        let now = datetime!(2031-01-01 12:00 UTC);
        let rows: Vec<OffsetDateTime> = (0..3).map(|i| now - Duration::minutes(50 - i)).collect();

        assert_eq!(wait_for(&rows, 4, now), None);
        assert_eq!(wait_for(&rows, 3, now), Some(600));
        assert_eq!(wait_for(&rows, 2, now), Some(660));
        // A row that left the window already still frees a slot at least a second away.
        assert_eq!(wait_for(&[now - Duration::hours(2)], 1, now), Some(1));
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        crate::create_workspace(
            pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    fn new(
        ws: WorkspaceId,
        member: Option<Uuid>,
        now: OffsetDateTime,
        trace: &TraceContext,
    ) -> NewTestSend<'_> {
        NewTestSend {
            id: Uuid::now_v7(),
            workspace: ws,
            template: Uuid::now_v7(),
            template_version: None,
            draft_revision: None,
            member,
            destination: Uuid::now_v7(),
            sender_identity: Uuid::now_v7(),
            recipient_hash: [1; 32],
            recipient_member: None,
            person: None,
            now,
            trace,
        }
    }

    async fn reserve(pool: &PgPool, new: &NewTestSend<'_>) -> TestSendReservation {
        let mut tx = begin(pool).await.unwrap();
        let reservation = reserve_test_send(&mut tx, new).await.unwrap();
        commit(tx).await.unwrap();
        reservation
    }

    async fn user(pool: &PgPool, email: &str) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
            .bind(id)
            .bind(email)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_limits_count_a_rolling_hour_and_a_reservation_enqueues_its_job(pool: PgPool) {
        let ws = workspace(&pool).await;
        let member = user(&pool, "m@example.test").await;
        let other = user(&pool, "o@example.test").await;
        let trace = TraceContext::root(Cause::api("t"));
        let start = datetime!(2031-05-06 07:00 UTC);

        for minute in 0..MEMBER_HOURLY_LIMIT {
            let at = start + Duration::minutes(minute as i64);
            assert!(matches!(
                reserve(&pool, &new(ws, Some(member), at, &trace)).await,
                TestSendReservation::Reserved(_)
            ));
        }
        let now = start + Duration::minutes(30);
        let refused = reserve(&pool, &new(ws, Some(member), now, &trace)).await;

        // The oldest row, made at minute 0, leaves the window at minute 60.
        assert_eq!(
            refused,
            TestSendReservation::Limited {
                retry_after: 30 * 60
            }
        );
        assert!(matches!(
            reserve(&pool, &new(ws, Some(other), now, &trace)).await,
            TestSendReservation::Reserved(_)
        ));
        let after = start + Duration::minutes(61);
        assert!(matches!(
            reserve(&pool, &new(ws, Some(member), after, &trace)).await,
            TestSendReservation::Reserved(_)
        ));
        let jobs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'template.test_send'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            jobs, 12,
            "a reservation is a row and a job, a refusal neither"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_versions_a_test_send_rendered_with_are_kept_on_its_row_until_it_ends(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let TestSendReservation::Reserved(send) = reserve(&pool, &new(ws, None, now, &trace)).await
        else {
            panic!("not reserved");
        };
        let theme = Uuid::now_v7();
        let snippet = Uuid::now_v7().to_string();
        let mut connection = pool.acquire().await.unwrap();

        let recorded = record_test_send_versions(
            &mut connection,
            ws,
            send.id,
            Some((theme, 3)),
            &json!({ snippet.clone(): 2 }),
        )
        .await
        .unwrap();
        finish_test_send(
            &mut connection,
            ws,
            send.id,
            TestSendOutcome::Sent,
            None,
            now,
        )
        .await
        .unwrap();
        let late = record_test_send_versions(&mut connection, ws, send.id, None, &json!({}))
            .await
            .unwrap();
        let row = find_test_send(&mut connection, ws, send.id)
            .await
            .unwrap()
            .unwrap();

        assert!(recorded);
        assert!(!late, "an ended row is not changed");
        assert_eq!((row.theme_id, row.theme_version), (Some(theme), Some(3)));
        assert_eq!(row.snippet_versions, Some(json!({ snippet: 2 })));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_workspace_limit_holds_across_members(pool: PgPool) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        for _ in 0..WORKSPACE_HOURLY_LIMIT {
            let member = user(&pool, &format!("{}@example.test", Uuid::now_v7())).await;
            assert!(matches!(
                reserve(&pool, &new(ws, Some(member), now, &trace)).await,
                TestSendReservation::Reserved(_)
            ));
        }
        let member = user(&pool, "last@example.test").await;

        let refused = reserve(&pool, &new(ws, Some(member), now, &trace)).await;

        assert_eq!(refused, TestSendReservation::Limited { retry_after: 3600 });
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_requests_for_the_last_slot_have_one_winner(pool: PgPool) {
        let ws = workspace(&pool).await;
        let member = user(&pool, "m@example.test").await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        for _ in 0..MEMBER_HOURLY_LIMIT - 1 {
            reserve(&pool, &new(ws, Some(member), now, &trace)).await;
        }
        let one = new(ws, Some(member), now, &trace);
        let two = new(ws, Some(member), now, &trace);

        let (a, b) = tokio::join!(reserve(&pool, &one), reserve(&pool, &two));

        let won = [a, b]
            .iter()
            .filter(|reservation| matches!(reservation, TestSendReservation::Reserved(_)))
            .count();
        assert_eq!(won, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_is_ended_once_listed_newest_first_and_deleted_by_age_and_hash(pool: PgPool) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let old = new(ws, None, now - Duration::days(400), &trace);
        let mut other_hash = new(ws, None, now - Duration::days(1), &trace);
        other_hash.recipient_hash = [2; 32];
        let recent = new(ws, None, now - Duration::days(2), &trace);
        for send in [&old, &other_hash, &recent] {
            reserve(&pool, send).await;
        }
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            finish_test_send(
                &mut connection,
                ws,
                recent.id,
                TestSendOutcome::Sent,
                None,
                now
            )
            .await
            .unwrap()
        );
        assert!(
            !finish_test_send(
                &mut connection,
                ws,
                recent.id,
                TestSendOutcome::Failed,
                Some("x"),
                now
            )
            .await
            .unwrap(),
            "an ended row is not ended twice"
        );
        let (page, more) = list_test_sends(&pool, ws, None, None, 2, None)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|listed| listed.send.id).collect::<Vec<_>>(),
            [other_hash.id, recent.id]
        );
        assert!(more);
        assert_eq!(page[1].send.outcome, TestSendOutcome::Sent);
        assert!(
            list_test_sends(&pool, ws, Some(Uuid::now_v7()), None, 10, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(is_test_send(&pool, recent.id).await.unwrap());
        assert!(!is_test_send(&pool, Uuid::now_v7()).await.unwrap());

        assert_eq!(delete_test_sends_before(&pool, now, 13).await.unwrap(), 1);
        let gone = delete_test_sends_by_hash(&mut connection, ws, &[vec![1; 32]])
            .await
            .unwrap();
        assert_eq!(gone, 1, "the remaining row with hash 1");
        assert!(
            find_test_send(&mut connection, ws, other_hash.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_later_retention_batch_keeps_the_first_test_send_delete(pool: PgPool) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        for _ in 0..2 {
            reserve(&pool, &new(ws, None, now - Duration::days(400), &trace)).await;
        }
        sqlx::raw_sql(
            "CREATE SEQUENCE retention_batches;
             CREATE FUNCTION fail_later_batch() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
                 IF nextval('retention_batches') > 1 THEN
                     RAISE EXCEPTION 'later test send batch failed';
                 END IF;
                 RETURN NULL;
             END $$;
             CREATE TRIGGER fail_later_batch BEFORE DELETE ON test_sends
             FOR EACH STATEMENT EXECUTE FUNCTION fail_later_batch()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let error = delete_test_sends_in_batches(&pool, now, 13, 1)
            .await
            .unwrap_err();
        assert!(format!("{error:?}").contains("later test send batch failed"));
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM test_sends")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1, "the first test send batch committed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_queued_row_whose_job_is_dead_or_gone_ends_abandoned_and_a_live_one_stays(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let [dead, gone, ready, running, ended] =
            [(); 5].map(|()| new(ws, None, now - Duration::minutes(5), &trace));
        for send in [&dead, &gone, &ready, &running, &ended] {
            reserve(&pool, send).await;
        }
        let job_of = |id: Uuid, state: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query("UPDATE jobs SET state = $2 WHERE payload ->> 'test_send_id' = $1")
                    .bind(id.to_string())
                    .bind(state)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        };
        job_of(dead.id, "dead").await;
        sqlx::query("DELETE FROM jobs WHERE payload ->> 'test_send_id' = $1")
            .bind(gone.id.to_string())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE jobs SET state = 'running', lease_token = gen_random_uuid(), \
                lease_expires_at = now() - interval '1 hour', attempts = 1 \
             WHERE payload ->> 'test_send_id' = $1",
        )
        .bind(running.id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        finish_test_send(
            &mut connection,
            ws,
            ended.id,
            TestSendOutcome::Sent,
            None,
            now,
        )
        .await
        .unwrap();
        job_of(ended.id, "dead").await;

        let mut abandoned = abandon_orphaned_test_sends(&mut connection, now)
            .await
            .unwrap();

        abandoned.sort();
        let mut expected = vec![dead.id, gone.id];
        expected.sort();
        assert_eq!(abandoned, expected);
        for id in [dead.id, gone.id] {
            let row = find_test_send(&mut connection, ws, id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.outcome, TestSendOutcome::Failed);
            assert_eq!(row.reason.as_deref(), Some(ABANDONED));
            assert_eq!(row.completed_at, Some(now));
        }
        for id in [ready.id, running.id] {
            let row = find_test_send(&mut connection, ws, id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.outcome, TestSendOutcome::Queued);
        }
        let again = abandon_orphaned_test_sends(&mut connection, now)
            .await
            .unwrap();
        assert!(again.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_quota_counts_the_same_hour_per_member_and_per_workspace(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (mine, theirs) = (
            user(&pool, "m@example.test").await,
            user(&pool, "o@example.test").await,
        );
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let oldest = now - Duration::minutes(40);
        for (member, at) in [
            (mine, oldest),
            (mine, now - Duration::minutes(5)),
            (theirs, now - Duration::minutes(10)),
            // Outside the hour: not counted.
            (mine, now - Duration::minutes(61)),
        ] {
            reserve(&pool, &new(ws, Some(member), at, &trace)).await;
        }
        let mut connection = pool.acquire().await.unwrap();

        let quota = test_send_quota(&mut connection, ws, Some(mine), now)
            .await
            .unwrap();

        assert_eq!(
            quota.member,
            QuotaUsage {
                used: 2,
                limit: MEMBER_HOURLY_LIMIT
            }
        );
        assert_eq!(
            quota.workspace,
            QuotaUsage {
                used: 3,
                limit: WORKSPACE_HOURLY_LIMIT
            }
        );
        assert_eq!(quota.resets_at, None, "the caller can send now");
        let keyless = test_send_quota(&mut connection, ws, None, now)
            .await
            .unwrap();
        assert_eq!(keyless.member.used, 0);
        assert_eq!(keyless.resets_at, None);
        let idle = test_send_quota(&mut connection, ws, Some(mine), now + Duration::hours(3))
            .await
            .unwrap();
        assert_eq!((idle.workspace.used, idle.resets_at), (0, None));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_quota_and_the_reservation_agree_at_the_limit(pool: PgPool) {
        let ws = workspace(&pool).await;
        let member = user(&pool, "m@example.test").await;
        let trace = TraceContext::root(Cause::api("t"));
        let start = datetime!(2031-05-06 07:00 UTC);
        for _ in 0..MEMBER_HOURLY_LIMIT {
            reserve(&pool, &new(ws, Some(member), start, &trace)).await;
        }
        let now = start + Duration::minutes(30);
        let mut connection = pool.acquire().await.unwrap();

        let quota = test_send_quota(&mut connection, ws, Some(member), now)
            .await
            .unwrap();

        assert_eq!(quota.member.used, quota.member.limit);
        let refused = reserve(&pool, &new(ws, Some(member), now, &trace)).await;
        let retry_after = (quota.resets_at.unwrap() - now).whole_seconds() as u64;
        assert_eq!(refused, TestSendReservation::Limited { retry_after });
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_log_shows_a_member_email_an_allowlist_hint_and_removed_address(pool: PgPool) {
        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let ws = workspace(&pool).await;
        secrets::insert_hmac_root_if_absent(&pool, &seal(&keys, b"root", b"aad"))
            .await
            .unwrap();
        let (asker, recipient) = (
            user(&pool, "asker@example.test").await,
            user(&pool, "recipient@example.test").await,
        );
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let entry = Uuid::now_v7();
        let sealed = seal(
            &keys,
            b"qa@example.test",
            &secret_aad(
                ws,
                SecretId::from_uuid(entry),
                SecretPurpose::TestSendAddress,
            ),
        );
        let mut tx = begin(&pool).await.unwrap();
        add_allowlist_entry(
            &mut tx,
            &keys.key_ids(),
            ws,
            entry,
            &[7; 32],
            &masked_hint("qa@example.test"),
            &sealed,
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        let mut to_member = new(ws, Some(asker), now - Duration::minutes(3), &trace);
        to_member.recipient_member = Some(recipient);
        let mut to_entry = new(ws, None, now - Duration::minutes(2), &trace);
        to_entry.recipient_hash = [7; 32];
        for send in [&to_member, &to_entry] {
            reserve(&pool, send).await;
        }

        let (rows, _) = list_test_sends(&pool, ws, None, None, 10, None)
            .await
            .unwrap();
        let shown: Vec<_> = rows
            .into_iter()
            .map(|row| (row.recipient_display, row.sent_by_email))
            .collect();
        assert_eq!(
            shown,
            [
                ("q***@example.test".to_owned(), None),
                (
                    "recipient@example.test".to_owned(),
                    Some("asker@example.test".to_owned())
                ),
            ]
        );

        let mut connection = pool.acquire().await.unwrap();
        delete_allowlist_entry(&mut connection, ws, entry)
            .await
            .unwrap();
        let (rows, _) = list_test_sends(&pool, ws, None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(rows[0].recipient_display, "removed address");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_revision_is_stored_filters_the_list_and_follows_the_job(pool: PgPool) {
        let ws = workspace(&pool).await;
        let trace = TraceContext::root(Cause::api("t"));
        let now = datetime!(2031-05-06 07:00 UTC);
        let mut draft = new(ws, None, now - Duration::minutes(3), &trace);
        draft.draft_revision = Some(4);
        let mut other_draft = new(ws, None, now - Duration::minutes(2), &trace);
        other_draft.draft_revision = Some(5);
        let mut version = new(ws, None, now - Duration::minutes(1), &trace);
        version.template_version = Some(2);
        for send in [&draft, &other_draft, &version] {
            reserve(&pool, send).await;
        }
        let revisions = |rows: Vec<ListedTestSend>| {
            rows.into_iter()
                .map(|row| row.send.draft_revision)
                .collect::<Vec<_>>()
        };

        let (all, _) = list_test_sends(&pool, ws, None, None, 10, None)
            .await
            .unwrap();
        let (only_four, _) = list_test_sends(&pool, ws, None, Some(4), 10, None)
            .await
            .unwrap();

        assert_eq!(revisions(all), [None, Some(5), Some(4)]);
        assert_eq!(revisions(only_four), [Some(4)]);

        let mut connection = pool.acquire().await.unwrap();
        set_test_send_draft_revision(&mut connection, ws, draft.id, 6)
            .await
            .unwrap();
        set_test_send_draft_revision(&mut connection, ws, version.id, 6)
            .await
            .unwrap();
        let (after, _) = list_test_sends(&pool, ws, None, None, 10, None)
            .await
            .unwrap();
        assert_eq!(
            revisions(after),
            [None, Some(5), Some(6)],
            "a version test keeps no revision"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reset_time_is_that_of_the_limit_that_blocks(pool: PgPool) {
        let ws = workspace(&pool).await;
        let me = user(&pool, "m@example.test").await;
        let trace = TraceContext::root(Cause::api("t"));
        let start = datetime!(2031-05-06 07:00 UTC);
        let now = start + Duration::minutes(30);
        let mut connection = pool.acquire().await.unwrap();

        // Others fill the workspace; the caller has none of their own.
        for minute in 0..WORKSPACE_HOURLY_LIMIT {
            let other = user(&pool, &format!("{}@example.test", Uuid::now_v7())).await;
            let at = start + Duration::seconds(minute as i64);
            reserve(&pool, &new(ws, Some(other), at, &trace)).await;
        }
        let workspace_full = test_send_quota(&mut connection, ws, Some(me), now)
            .await
            .unwrap();
        assert_eq!(workspace_full.member.used, 0);
        assert_eq!(workspace_full.resets_at, Some(start + WINDOW));
        let refused = reserve(&pool, &new(ws, Some(me), now, &trace)).await;
        let retry_after = (workspace_full.resets_at.unwrap() - now).whole_seconds() as u64;
        assert_eq!(refused, TestSendReservation::Limited { retry_after });
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reset_time_of_a_caller_at_the_member_limit_is_the_member_release(pool: PgPool) {
        let ws = workspace(&pool).await;
        let me = user(&pool, "m@example.test").await;
        let other = user(&pool, "o@example.test").await;
        let trace = TraceContext::root(Cause::api("t"));
        let start = datetime!(2031-05-06 07:00 UTC);
        let now = start + Duration::minutes(30);
        // Another member's older send is the workspace's oldest, but the workspace is not full.
        reserve(
            &pool,
            &new(ws, Some(other), start - Duration::minutes(10), &trace),
        )
        .await;
        for minute in 0..MEMBER_HOURLY_LIMIT {
            let at = start + Duration::minutes(minute as i64);
            reserve(&pool, &new(ws, Some(me), at, &trace)).await;
        }
        let mut connection = pool.acquire().await.unwrap();

        let quota = test_send_quota(&mut connection, ws, Some(me), now)
            .await
            .unwrap();

        assert_eq!(quota.resets_at, Some(start + WINDOW));
    }
}
