//! The rows of the flow runtime (architecture 9.2 and 9.3): enrollments with their
//! `state_version`, the step executions that make a step idempotent, the event waits, and the
//! payload of the `flow.step` job. Every function runs in the caller's transaction.
//!
//! The runtime in `zirv-flow-jobs` locks an enrollment, computes a transition with the pure
//! engine and writes it through [`save_enrollment`]; the compare-and-set on `state_version` is the
//! rule that the first commit wins.

use serde_json::{Value, json};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use crate::{NewJob, StoreError};

/// The kind of the job that advances an enrollment.
pub const STEP_JOB_KIND: &str = "flow.step";
/// The kind of the job a webhook action enqueues; its handler is the webhook action's own.
pub const WEBHOOK_JOB_KIND: &str = "webhook.call";

/// Where an enrollment is in its life. A yield at a branch is never stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnrollmentStatus {
    /// Ready to run its node.
    Active,
    /// Asleep at a wait node.
    Waiting,
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
            Self::Completed => "completed",
            Self::Exited => "exited",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Active,
            Self::Waiting,
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
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
    created_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
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
     e.exit_reason, e.error, e.trace_id, e.cause_kind, e.cause_id, e.created_at, e.ended_at";

/// Locks an enrollment `FOR UPDATE` and reads it. Two steps of one enrollment run one after the
/// other: the second reads the state the first committed.
pub async fn lock_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Enrollment>, StoreError> {
    let row: Option<EnrollmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ENROLLMENT_COLUMNS} FROM enrollments e \
         WHERE e.workspace_id = $1 AND e.id = $2 FOR UPDATE OF e"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(EnrollmentRow::into_enrollment).transpose()
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
         WHERE workspace_id = $1 AND flow_id = $2 AND state IN ('active', 'waiting', 'failed')",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
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
             exit_reason = $8, error = $9, updated_at = now(), \
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

/// Marks the node an enrollment failed at, with the error an operator reads.
pub async fn fail_step(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    node: &str,
    error: &str,
    attempt: i32,
) -> Result<Uuid, StoreError> {
    sqlx::query_scalar(
        "INSERT INTO step_executions \
            (id, workspace_id, enrollment_id, node_id, outcome, error, attempts) \
         VALUES ($1, $2, $3, $4, 'failed', $5, $6) \
         ON CONFLICT (enrollment_id, node_id) DO UPDATE SET \
             outcome = 'failed', error = EXCLUDED.error, attempts = EXCLUDED.attempts \
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(enrollment)
    .bind(node)
    .bind(error)
    .bind(attempt)
    .fetch_one(connection)
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
        match &self.input {
            StepJobInput::Start => payload["input"] = json!("start"),
            StepJobInput::Wake => payload["input"] = json!("wake"),
            StepJobInput::Event { event_id, name } => {
                payload["input"] = json!("event");
                payload["event_id"] = json!(event_id);
                payload["event"] = json!(name);
            }
        }
        payload
    }

    pub fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let input = match text("input")? {
            "start" => StepJobInput::Start,
            "wake" => StepJobInput::Wake,
            "event" => StepJobInput::Event {
                event_id: Uuid::parse_str(text("event_id")?).ok()?,
                name: text("event")?.to_owned(),
            },
            _ => return None,
        };
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            enrollment: Uuid::parse_str(text("enrollment_id")?).ok()?,
            state_version: payload.get("state_version")?.as_i64()?,
            input,
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
    async fn the_in_flight_count_skips_ended_enrollments(pool: PgPool) {
        let ws = workspace(&pool, "count").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let (one, two) = (person(&pool, ws).await, person(&pool, ws).await);
        let kept = enrollment_row(&pool, ws, flow.id, one).await;
        let ended = enrollment_row(&pool, ws, flow.id, two).await;
        let mut connection = pool.acquire().await.unwrap();
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
}
