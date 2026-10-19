//! `flow.lifecycle`: the batches behind a flow's archive, its resume and an operator's bulk exit
//! (architecture 9.4).
//!
//! One run is one batch of [`LIFECYCLE_BATCH`] enrollments in one transaction: the people are
//! locked in id order, then the enrollments are changed and a decision is recorded for each, under
//! the trace of the action (an operator's `manual` cause). A full batch enqueues the next round
//! with its own unique key, so a flow of any size is done in bounded transactions, and a run that
//! is retried does the same batch again.
//!
//! - `archive` exits every active, waiting, held and failed enrollment with reason
//!   `flow_archived`.
//! - `resume` runs every held enrollment again (see `zirv_flow_store::resume_batch`). It stops when
//!   the flow is no longer published: a flow paused again holds its steps once more.
//! - `exit_version` exits the in-flight enrollments pinned to one older version with reason
//!   `manual` (ruling 469cfc51).

use serde_json::Value;
use zirv_flow_store::{
    ExitBatch, FlowState, LIFECYCLE_BATCH, LIFECYCLE_JOB_KIND, LifecycleAction, LifecyclePayload,
    enqueue, exit_batch, find_flow, resume_batch,
};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct FlowLifecycle;

impl JobKind for FlowLifecycle {
    const KIND: &'static str = LIFECYCLE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = LifecyclePayload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a flow lifecycle batch"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let trace = ctx.trace().clone();
        let conn = ctx.conn();
        let now = clock.now(conn).await?;
        let Some(flow) = find_flow(&mut *conn, job.workspace, job.flow).await? else {
            return Ok(());
        };
        let batch = match job.action {
            LifecycleAction::Archive | LifecycleAction::ExitVersion { .. } => {
                let (version, reason) = match job.action {
                    LifecycleAction::ExitVersion { version } => (Some(version), "manual"),
                    _ => (None, "flow_archived"),
                };
                exit_batch(
                    conn,
                    &ExitBatch {
                        workspace: job.workspace,
                        flow: job.flow,
                        version,
                        reason,
                        trace: &trace,
                    },
                    now,
                    LIFECYCLE_BATCH,
                )
                .await?
            }
            LifecycleAction::Resume => {
                if flow.state != FlowState::Published {
                    return Ok(());
                }
                resume_batch(conn, job.workspace, job.flow, &trace, LIFECYCLE_BATCH).await?
            }
        };
        // A full batch may leave more behind; one that changed nothing would only repeat itself.
        if batch.read >= LIFECYCLE_BATCH && batch.changed > 0 {
            let next = LifecyclePayload {
                round: job.round.saturating_add(1),
                ..job
            };
            enqueue(&mut *conn, &next.job(&trace)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Cause, TraceContext, UserId};
    use zirv_flow_store::{
        ControlError, Enrollment, EnrollmentStatus, FlowError, SETTLE_JOB_KIND, StepJobInput,
        StepJobPayload, enqueue, enqueue_lifecycle, find_enrollment, resume_failed,
    };

    use super::super::flow_step::tests::{Env, edge};
    use super::*;

    #[test]
    fn the_kind_name_is_flow_lifecycle_and_it_is_registered() {
        assert_eq!(FlowLifecycle::KIND, "flow.lifecycle");
        assert!(crate::Registry::built_in().get("flow.lifecycle").is_some());
    }

    /// A wait of an hour, then the end.
    fn parked() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 3600},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "end")]})
    }

    fn operator() -> TraceContext {
        TraceContext::root(Cause::manual(UserId::from_uuid(Uuid::new_v4())))
    }

    async fn lifecycle_jobs(env: &Env) -> usize {
        env.run_kind(LIFECYCLE_JOB_KIND, &FlowLifecycle).await
    }

    /// Enrolls a person and runs the start, so the enrollment waits at the hour.
    async fn parked_person(env: &Env, flow: Uuid, email: &str) -> Uuid {
        let person = env.person(email, json!({})).await;
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        env.run_matching(Some("start"), Some(id)).await;
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Waiting);
        id
    }

    async fn ended_nodes(env: &Env, node: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM step_executions WHERE node_id = $1")
            .bind(node)
            .fetch_one(&env.pool)
            .await
            .unwrap()
    }

    async fn decisions_of(env: &Env, trace: &TraceContext, outcome: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM decision_records \
             WHERE workspace_id = $1 AND trace_id = $2 AND outcome = $3 \
               AND action = 'flow.enrollment'",
        )
        .bind(env.ws.as_uuid())
        .bind(trace.trace_id.as_uuid())
        .bind(outcome)
        .fetch_one(&env.pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_paused_flow_holds_a_due_timer_and_resume_runs_it_once(pool: PgPool) {
        let env = Env::new(pool).await;
        let flow = env.flow("Wait", parked()).await;
        let id = parked_person(&env, flow, "ada@example.com").await;
        let before = env.enrollment(id).await;

        env.pause(flow).await;
        env.advance(3601);
        env.run_due().await;
        let held = env.enrollment(id).await;
        // The same timer delivered again, by another job, changes nothing and records nothing.
        let payload = StepJobPayload {
            workspace: env.ws,
            enrollment: id,
            state_version: before.state_version,
            input: StepJobInput::Wake,
        };
        enqueue(
            &env.pool,
            &payload.job(&before.trace, None, "again".to_owned()),
        )
        .await
        .unwrap();
        env.run_due().await;

        assert_eq!(held.status, EnrollmentStatus::Held);
        assert_eq!(
            (held.node.as_str(), held.state_version),
            ("w", before.state_version),
            "a held enrollment keeps its node and version"
        );
        assert_eq!(held.resume_input, Some(json!({"input": "wake"})));
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Held);
        assert_eq!(
            ended_nodes(&env, "end").await,
            0,
            "nothing ran while paused"
        );
        assert_eq!(decisions_of(&env, &before.trace, "held").await, 1);

        let trace = env.resume(flow).await;
        assert_eq!(lifecycle_jobs(&env).await, 1);
        let resumed = env.enrollment(id).await;
        assert_eq!(resumed.status, EnrollmentStatus::Waiting);
        assert!(resumed.resume_input.is_none());
        env.run_due().await;

        let done = env.enrollment(id).await;
        assert_eq!(done.status, EnrollmentStatus::Completed);
        assert_eq!(ended_nodes(&env, "end").await, 1, "the timer ran once");
        assert_eq!(decisions_of(&env, &trace, "resumed").await, 1);
        let cause: String = sqlx::query_scalar(
            "SELECT cause_kind FROM decision_records WHERE trace_id = $1 AND outcome = 'resumed'",
        )
        .bind(trace.trace_id.as_uuid())
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(cause, "manual");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_start_that_falls_due_while_paused_is_held_and_runs_after_the_resume(pool: PgPool) {
        let env = Env::new(pool).await;
        let flow = env.flow("Wait", parked()).await;
        let person = env.person("ada@example.com", json!({})).await;
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;

        env.pause(flow).await;
        env.run_due().await;
        let held = env.enrollment(id).await;
        assert_eq!(held.status, EnrollmentStatus::Held);
        assert_eq!(held.resume_input, Some(json!({"input": "start"})));
        assert_eq!(
            env.enrollment(id).await.state_version,
            0,
            "the start has not run"
        );

        env.resume(flow).await;
        lifecycle_jobs(&env).await;
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Active);
        env.run_due().await;

        let started = env.enrollment(id).await;
        assert_eq!(started.status, EnrollmentStatus::Waiting);
        assert_eq!(started.state_version, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_for_a_held_wait_replaces_the_held_timer_and_a_timer_never_replaces_it(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        env.define_trait("path", TraitType::String).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.until_event", "event": "order", "timeout_seconds": 3600},
            {"id": "by_event", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "event"}},
            {"id": "by_timeout", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "timeout"}},
            {"id": "end_event", "type": "exit"},
            {"id": "end_timeout", "type": "exit"}
        ], "edges": [
            edge("e1", "wait", "event", "by_event"),
            edge("e2", "wait", "timeout", "by_timeout"),
            edge("e3", "by_event", "next", "end_event"),
            edge("e4", "by_timeout", "next", "end_timeout")
        ]});
        let flow = env.flow("Race", graph).await;
        let person = env.person("ada@example.com", json!({})).await;
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        env.run_matching(Some("start"), Some(id)).await;

        env.pause(flow).await;
        env.advance(3601);
        env.run_matching(Some("wake"), Some(id)).await;
        assert_eq!(
            env.enrollment(id).await.resume_input,
            Some(json!({"input": "wake"}))
        );
        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        env.run_matching(Some("event"), Some(id)).await;
        let held = env.enrollment(id).await;
        assert_eq!(held.status, EnrollmentStatus::Held);
        assert_eq!(
            held.resume_input.as_ref().and_then(|v| v.get("input")),
            Some(&json!("event")),
            "the event wins the race against the timeout"
        );

        env.resume(flow).await;
        lifecycle_jobs(&env).await;
        env.run_due().await;

        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        assert_eq!(env.traits(person).await["path"], json!("event"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_goal_exits_a_held_enrollment_like_the_state_it_was_held_from(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        let settings = json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"},
            "goal": {"type": "event", "event": "order", "filters": []}, "exit_on_goal": true});
        let flow = env.flow_with("Goal", parked(), settings).await;
        let id = parked_person(&env, flow, "ada@example.com").await;
        env.pause(flow).await;
        env.advance(3601);
        env.run_due().await;
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Held);

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        env.run_kind(SETTLE_JOB_KIND, &super::super::flow_settle::FlowSettle)
            .await;

        let exited = env.enrollment(id).await;
        assert_eq!(exited.status, EnrollmentStatus::Exited);
        assert_eq!(exited.exit_reason.as_deref(), Some("goal"));
        assert!(exited.resume_input.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archive_exits_every_in_flight_enrollment_and_an_archived_flow_cannot_resume(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let flow = env.flow("Wait", parked()).await;
        let fresh = {
            let person = env.person("fresh@example.com", json!({})).await;
            env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment
        };
        let asleep = parked_person(&env, flow, "asleep@example.com").await;
        let held = parked_person(&env, flow, "held@example.com").await;
        let failed = parked_person(&env, flow, "failed@example.com").await;
        sqlx::query(
            "UPDATE enrollments SET state = 'failed', error = 'boom', failed_node = node \
             WHERE id = $1",
        )
        .bind(failed)
        .execute(&env.pool)
        .await
        .unwrap();
        env.pause(flow).await;
        env.advance(3601);
        env.run_matching(Some("wake"), Some(held)).await;
        assert_eq!(env.enrollment(held).await.status, EnrollmentStatus::Held);
        let versions: Vec<i64> = {
            let mut found = Vec::new();
            for id in [fresh, asleep, held, failed] {
                found.push(env.enrollment(id).await.state_version);
            }
            found
        };

        let trace = env.archive(flow).await;
        assert_eq!(lifecycle_jobs(&env).await, 1);

        for (index, id) in [fresh, asleep, held, failed].into_iter().enumerate() {
            let exited = env.enrollment(id).await;
            assert_eq!(exited.status, EnrollmentStatus::Exited, "{id}");
            assert_eq!(exited.exit_reason.as_deref(), Some("flow_archived"));
            assert_eq!(exited.state_version, versions[index] + 1);
            assert!(exited.ended_at.is_some() && exited.wake_at.is_none());
            assert!(exited.error.is_none() && exited.resume_input.is_none());
        }
        assert_eq!(decisions_of(&env, &trace, "exited").await, 4);
        let cause: String = sqlx::query_scalar(
            "SELECT DISTINCT cause_kind FROM decision_records \
             WHERE trace_id = $1 AND outcome = 'exited'",
        )
        .bind(trace.trace_id.as_uuid())
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(cause, "manual");
        // The timers and starts that were waiting find nothing to do.
        env.advance(7200);
        env.run_due().await;
        assert_eq!(ended_nodes(&env, "end").await, 0);

        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        let again = zirv_flow_store::resume_flow(&mut tx, env.ws, flow, &trace).await;
        assert!(matches!(again, Err(FlowError::IllegalTransition { .. })));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bulk_exit_of_an_older_version_leaves_the_current_versions_enrollments_alone(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        let flow = env.flow("Wait", parked()).await;
        let old_one = parked_person(&env, flow, "one@example.com").await;
        let old_two = parked_person(&env, flow, "two@example.com").await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 7200},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "end")]});
        assert_eq!(env.republish(flow, graph).await, 2);
        let person = env.person("three@example.com", json!({})).await;
        let current = env.enroll(flow, 2, &[person]).await.enrolled[0].enrollment;
        env.run_matching(Some("start"), Some(current)).await;

        let trace = operator();
        let mut connection = env.pool.acquire().await.unwrap();
        enqueue_lifecycle(
            &mut connection,
            env.ws,
            flow,
            LifecycleAction::ExitVersion { version: 1 },
            &trace,
        )
        .await
        .unwrap();
        drop(connection);
        assert_eq!(lifecycle_jobs(&env).await, 1);

        for id in [old_one, old_two] {
            let exited = env.enrollment(id).await;
            assert_eq!(exited.status, EnrollmentStatus::Exited);
            assert_eq!(exited.exit_reason.as_deref(), Some("manual"));
        }
        let kept = env.enrollment(current).await;
        assert_eq!(kept.status, EnrollmentStatus::Waiting);
        assert_eq!(kept.flow_version, 2);
        assert_eq!(decisions_of(&env, &trace, "exited").await, 2);
        let cause: String = sqlx::query_scalar(
            "SELECT DISTINCT cause_kind FROM decision_records WHERE trace_id = $1",
        )
        .bind(trace.trace_id.as_uuid())
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(cause, "manual");
    }

    /// A trait increment that fails while the person's trait is not a number, then the end.
    fn bump() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "bump", "type": "action.update_trait", "key": "visits",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "bump", "next", "end")]})
    }

    /// A person whose increment fails at once: the flow's only attempt ends `failed`.
    async fn failed_bump(env: &Env, email: &str) -> (Uuid, Uuid, Uuid) {
        env.define_trait("visits", TraitType::Number).await;
        let flow = env.flow("Bump", bump()).await;
        let person = env.person(email, json!({"visits": 1})).await;
        sqlx::query("UPDATE people SET traits = '{\"visits\": \"many\"}' WHERE id = $1")
            .bind(person)
            .execute(&env.pool)
            .await
            .unwrap();
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'flow.step'")
            .execute(&env.pool)
            .await
            .unwrap();
        env.run_due().await;
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Failed);
        (flow, id, person)
    }

    async fn operate(
        env: &Env,
        flow: Uuid,
        enrollment: Uuid,
        skip: bool,
        trace: &TraceContext,
    ) -> Result<(), ControlError> {
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        resume_failed(&mut tx, env.ws, flow, enrollment, skip, trace).await?;
        zirv_flow_store::commit(tx).await.unwrap();
        Ok(())
    }

    async fn fix_visits(env: &Env, person: Uuid) {
        sqlx::query("UPDATE people SET traits = '{\"visits\": 1}' WHERE id = $1")
            .bind(person)
            .execute(&env.pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retry_runs_the_failed_step_again_from_the_state_it_started_from(pool: PgPool) {
        let env = Env::new(pool).await;
        let (flow, id, person) = failed_bump(&env, "ada@example.com").await;
        let failed = env.enrollment(id).await;
        fix_visits(&env, person).await;
        let trace = operator();

        operate(&env, flow, id, false, &trace).await.unwrap();
        let restored = env.enrollment(id).await;
        env.run_due().await;

        assert_eq!(restored.status, EnrollmentStatus::Active);
        assert_eq!(restored.state_version, failed.state_version);
        assert!(restored.error.is_none() && restored.failed_node.is_none());
        let done = env.enrollment(id).await;
        assert_eq!(done.status, EnrollmentStatus::Completed);
        assert_eq!(env.traits(person).await["visits"], json!(2));
        assert_eq!(ended_nodes(&env, "bump").await, 1);
        assert_eq!(decisions_of(&env, &trace, "retried").await, 1);
        let again = operate(&env, flow, id, false, &trace).await;
        assert!(matches!(
            again,
            Err(ControlError::NotFailed(EnrollmentStatus::Completed))
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_skip_leaves_the_failed_node_along_its_next_port(pool: PgPool) {
        let env = Env::new(pool).await;
        let (flow, id, person) = failed_bump(&env, "ada@example.com").await;
        let trace = operator();

        operate(&env, flow, id, true, &trace).await.unwrap();
        assert_eq!(
            env.enrollment(id).await.status,
            EnrollmentStatus::Failed,
            "the skip is a job, and runs when the worker reaches it"
        );
        env.run_due().await;

        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        assert_eq!(
            env.traits(person).await["visits"],
            json!("many"),
            "the skipped node did not run"
        );
        assert_eq!(ended_nodes(&env, "bump").await, 0);
        assert_eq!(ended_nodes(&env, "end").await, 1);
        assert_eq!(decisions_of(&env, &trace, "skipped").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_engine_failure_is_retried_and_skipped_through_the_engines_resume(pool: PgPool) {
        let env = Env::new(pool).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "t", "type": "wait.until_time", "when": {"kind": "date_time", "at": "soon"}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "t", "next", "end")]});
        let flow = env.flow("Bad time", graph).await;
        let people = [
            env.person("a@example.com", json!({})).await,
            env.person("b@example.com", json!({})).await,
        ];
        let entries = env.enroll(flow, 1, &people).await.enrolled;
        env.run_due().await;
        let (retried, skipped) = (entries[0].enrollment, entries[1].enrollment);
        let failed = env.enrollment(retried).await;
        assert_eq!(failed.status, EnrollmentStatus::Failed);
        assert!(failed.resume_input.is_none(), "the engine reported it");
        let trace = operator();

        operate(&env, flow, retried, false, &trace).await.unwrap();
        env.run_due().await;
        operate(&env, flow, skipped, true, &trace).await.unwrap();
        env.run_due().await;

        let again = env.enrollment(retried).await;
        assert_eq!(again.status, EnrollmentStatus::Failed, "still a bad time");
        assert_eq!(again.state_version, failed.state_version + 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM step_executions \
                 WHERE enrollment_id = $1 AND node_id = 't' AND outcome = 'failed'"
            )
            .bind(retried)
            .fetch_one(&env.pool)
            .await
            .unwrap(),
            1,
            "the retry recorded the node afresh"
        );
        assert_eq!(
            env.enrollment(skipped).await.status,
            EnrollmentStatus::Completed
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retry_or_skip_of_the_wrong_enrollment_is_refused(pool: PgPool) {
        let env = Env::new(pool).await;
        let flow = env.flow("Wait", parked()).await;
        let waiting = parked_person(&env, flow, "ada@example.com").await;
        let trace = operator();

        let not_failed = operate(&env, flow, waiting, false, &trace).await;
        let skip_waiting = operate(&env, flow, waiting, true, &trace).await;
        let unknown = operate(&env, flow, Uuid::new_v4(), false, &trace).await;
        let other_flow = operate(&env, Uuid::new_v4(), waiting, false, &trace).await;

        assert!(matches!(
            not_failed,
            Err(ControlError::NotFailed(EnrollmentStatus::Waiting))
        ));
        assert!(matches!(skip_waiting, Err(ControlError::NotFailed(_))));
        assert!(matches!(unknown, Err(ControlError::NotFound)));
        assert!(matches!(other_flow, Err(ControlError::NotFound)));
        assert_eq!(decisions_of(&env, &trace, "retried").await, 0);
        let kept: Enrollment =
            find_enrollment(&mut env.pool.acquire().await.unwrap(), env.ws, waiting)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(kept.status, EnrollmentStatus::Waiting);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_late_in_a_chain_can_be_retried_but_not_skipped(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_trait("visits", TraitType::Number).await;
        env.define_trait("tier", TraitType::String).await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "a", "type": "action.update_trait", "key": "tier",
             "change": {"op": "set", "value": "gold"}},
            {"id": "inc", "type": "action.update_trait", "key": "visits",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "a", "next", "inc"), edge("e2", "inc", "next", "end")]});
        let flow = env.flow("Chain", graph).await;
        let person = env.person("ada@example.com", json!({"visits": 1})).await;
        sqlx::query("UPDATE people SET traits = '{\"visits\": \"many\"}' WHERE id = $1")
            .bind(person)
            .execute(&env.pool)
            .await
            .unwrap();
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'flow.step'")
            .execute(&env.pool)
            .await
            .unwrap();
        env.run_due().await;
        let failed = env.enrollment(id).await;
        assert_eq!(
            (failed.node.as_str(), failed.failed_node.as_deref()),
            ("a", Some("inc"))
        );
        let trace = operator();

        let skip = operate(&env, flow, id, true, &trace).await;
        assert!(matches!(skip, Err(ControlError::SkipUnavailable)));
        fix_visits(&env, person).await;
        operate(&env, flow, id, false, &trace).await.unwrap();
        env.run_due().await;

        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        let traits = env.traits(person).await;
        assert_eq!(
            (&traits["tier"], &traits["visits"]),
            (&json!("gold"), &json!(2))
        );
    }
}
