//! The test-send log: one row per test send, reserved under the rate limit, ended by the job.

use serde_json::json;
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

/// A log row. It holds no address and no hash.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestSend {
    pub id: Uuid,
    pub template_id: Uuid,
    pub template_version: Option<i32>,
    pub member_id: Option<Uuid>,
    pub destination_id: Uuid,
    pub sender_identity_id: Uuid,
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
    member_id: Option<Uuid>,
    destination_id: Uuid,
    sender_identity_id: Uuid,
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
            member_id: self.member_id,
            destination_id: self.destination_id,
            sender_identity_id: self.sender_identity_id,
            recipient_member_id: self.recipient_member_id,
            render_context: self.render_context,
            outcome: TestSendOutcome::parse(&self.outcome)?,
            reason: self.reason,
            created_at: self.created_at,
            completed_at: self.completed_at,
        })
    }
}

const COLUMNS: &str = "id, template_id, template_version, member_id, destination_id, \
    sender_identity_id, recipient_member_id, render_context, outcome, reason, created_at, \
    completed_at";

/// A test send to reserve a slot for. The address, when the recipient is an allowlisted one, is
/// passed to the job in its payload only: no table keeps it.
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
    /// The allowlisted address to send to; none when the recipient is a member.
    pub address: Option<&'a str>,
    pub now: OffsetDateTime,
    pub trace: &'a TraceContext,
}

/// What [`reserve_test_send`] came to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestSendReservation {
    /// The row is written `queued` and its job is enqueued.
    Reserved(TestSend),
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
        "address": new.address,
    });
    enqueue(&mut *connection, &job).await?;
    row.into_send().map(TestSendReservation::Reserved)
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
            address: None,
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
}
