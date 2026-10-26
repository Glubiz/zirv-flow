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
            outcome: TestSendOutcome::parse(&self.outcome)?,
            reason: self.reason,
            created_at: self.created_at,
            completed_at: self.completed_at,
        })
    }
}

const COLUMNS: &str = "id, template_id, template_version, theme_id, theme_version, \n    snippet_versions, member_id, destination_id, \
    sender_identity_id, recipient_hash, recipient_member_id, render_context, outcome, reason, created_at, \
    completed_at";

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
    let since = new.now - WINDOW;
    let in_workspace: Vec<OffsetDateTime> = sqlx::query_scalar(
        "SELECT created_at FROM test_sends WHERE workspace_id = $1 AND created_at > $2 \
         ORDER BY created_at",
    )
    .bind(new.workspace.as_uuid())
    .bind(since)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let by_member: Vec<OffsetDateTime> = match new.member {
        Some(member) => sqlx::query_scalar(
            "SELECT created_at FROM test_sends \
             WHERE workspace_id = $1 AND member_id = $2 AND created_at > $3 ORDER BY created_at",
        )
        .bind(new.workspace.as_uuid())
        .bind(member)
        .bind(since)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?,
        None => Vec::new(),
    };
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
            render_context, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING {COLUMNS}"
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

/// Seconds until a slot frees, when `counted` (oldest first, all inside the window) fills `limit`:
/// the slot of the `counted.len() - limit + 1`-th oldest row, which leaves the window an hour
/// after it was made. At least one second.
fn wait_for(counted: &[OffsetDateTime], limit: usize, now: OffsetDateTime) -> Option<u64> {
    if counted.len() < limit {
        return None;
    }
    let frees = counted[counted.len() - limit] + WINDOW;
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

/// A page of the workspace's log, newest first, at most `limit`, after the `before` cursor,
/// optionally of one template. The flag is true when more exist.
pub async fn list_test_sends(
    pool: &PgPool,
    workspace: WorkspaceId,
    template: Option<Uuid>,
    limit: u32,
    before: Option<TestSendCursor>,
) -> Result<(Vec<TestSend>, bool), StoreError> {
    let mut rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM test_sends \
         WHERE workspace_id = $1 AND ($2::uuid IS NULL OR template_id = $2) \
           AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4)) \
         ORDER BY created_at DESC, id DESC LIMIT $5"
    )))
    .bind(workspace.as_uuid())
    .bind(template)
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
        .map(Row::into_send)
        .collect::<Result<_, _>>()?;
    Ok((sends, more))
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

/// Deletes the rows older than the retention as of `now`, in batches; how many.
pub async fn delete_test_sends_before(
    connection: &mut PgConnection,
    now: OffsetDateTime,
) -> Result<u64, StoreError> {
    let mut total = 0;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM test_sends WHERE id IN ( \
                SELECT id FROM test_sends \
                WHERE created_at < $1::timestamptz - make_interval(months => $2) LIMIT $3)",
        )
        .bind(now)
        .bind(TEST_SEND_RETENTION_MONTHS)
        .bind(BATCH)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        total += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < BATCH {
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
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::{begin, commit};

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
        let (page, more) = list_test_sends(&pool, ws, None, 2, None).await.unwrap();
        assert_eq!(
            page.iter().map(|send| send.id).collect::<Vec<_>>(),
            [other_hash.id, recent.id]
        );
        assert!(more);
        assert_eq!(page[1].outcome, TestSendOutcome::Sent);
        assert!(
            list_test_sends(&pool, ws, Some(Uuid::now_v7()), 10, None)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(is_test_send(&pool, recent.id).await.unwrap());
        assert!(!is_test_send(&pool, Uuid::now_v7()).await.unwrap());

        assert_eq!(
            delete_test_sends_before(&mut connection, now)
                .await
                .unwrap(),
            1
        );
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
}
