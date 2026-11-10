//! `flow.settle`: what a flow's goal and exit criteria do to the enrollments in flight
//! (architecture 9.4, ruling 4d134de2).
//!
//! The store finds the candidates in batched queries, over the events an apply-batch inserted and
//! the membership changes a refresh wrote, and names them in the payload: an enrollment, its
//! person and what was seen (an event by name, entering the goal segment, leaving the trigger
//! segment). This job applies what the store cannot: the property filters of an event goal or
//! criterion, with the reference evaluator over the stored event. It then hands the result to the
//! pure `engine::settle`, which decides, and commits the outcome.
//!
//! An exit ends the enrollment `exited` with its reason, clears its waits and grows
//! `state_version`, so a timer or an event for the older state finds it stale. A conversion is a
//! `settings.goal` step row (no node can have that id, so it is collision-free and counted with
//! the nodes) and a `flow.goal` decision, written once: a goal reached again finds the row.
//!
//! The people are locked in id order and each person before the enrollment, as every flow path
//! does. An enrollment that moved on since the store looked is read again under its lock, and
//! `settle` acts only on one that is active or waiting (a held one counts as the state it was
//! held from).
//!
//! Each lock round is a released savepoint whose subtransaction id the job keeps until it commits,
//! so the store names at most 60 enrollments a job. A job queued before that cap can name more:
//! it settles the first 60 and hands the rest on, in its own transaction, to jobs of at most 60,
//! keyed by the job and their place, so each enrollment is settled once.

use hashbrown::HashMap;
use serde_json::Value;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::flow::{ExitCriterion, FlowSettings, Goal};
use zirv_flow_engine::{EnrollmentState, Matched, Outcome, Status, WaitChange, settle};
use zirv_flow_segments::{Catalog, TraitStorage};
use zirv_flow_store::{
    Enrollment, EnrollmentStatus, EnrollmentUpdate, GraphPoint, NewDecision, NewStepExecution,
    SETTLE_JOB_KIND, SettleItem, SettlePayload, Signal, StepJobInput, StepOutcome,
    append_decisions, clear_waits, find_event_of_person, find_flow_version, hand_on_settle_rest,
    lock_enrollment, record_steps, save_enrollment, segment_catalog,
};

use super::flow_step::{exit_reason_text, filters_pass};
use super::segment_refresh::catalog_of;
use crate::{JobContext, JobError, JobKind};

pub(crate) struct FlowSettle;

/// The step row of a conversion by the flow's settings.
const GOAL_NODE: &str = "settings.goal";

impl JobKind for FlowSettle {
    const KIND: &'static str = SETTLE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let mut job = SettlePayload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a flow settle"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        // A job queued before the cap can name more than one transaction may lock in rounds.
        let trace = ctx.trace().clone();
        let job_id = ctx.job_id();
        hand_on_settle_rest(ctx.conn(), job_id, &trace, &mut job).await?;
        let clock = ctx.clock().clone();
        let at = (ctx.job_id(), ctx.attempts());
        let conn = ctx.conn();
        let now = clock.now(conn).await?;
        let mut run = Run {
            workspace: job.workspace,
            now,
            at,
            settings: HashMap::new(),
            catalog: None,
        };
        let mut items = job.items;
        items.sort_by_key(|item| (item.person, item.enrollment));
        for item in &items {
            run.settle_one(conn, item).await?;
        }
        Ok(())
    }
}

struct Run {
    workspace: WorkspaceId,
    now: OffsetDateTime,
    at: (Uuid, i32),
    /// The settings of each flow version this job met, parsed once.
    settings: HashMap<(Uuid, i32), Option<FlowSettings>>,
    catalog: Option<(Catalog, TraitStorage)>,
}

impl Run {
    async fn settings_of(
        &mut self,
        conn: &mut PgConnection,
        enrollment: &Enrollment,
    ) -> Result<Option<FlowSettings>, JobError> {
        let key = (enrollment.flow_id, enrollment.flow_version);
        if let Some(known) = self.settings.get(&key) {
            return Ok(known.clone());
        }
        let parsed = find_flow_version(conn, self.workspace, key.0, key.1)
            .await?
            .and_then(|version| serde_json::from_value::<FlowSettings>(version.settings).ok());
        self.settings.insert(key, parsed.clone());
        Ok(parsed)
    }

    async fn catalog(&mut self, conn: &mut PgConnection) -> Result<&Catalog, JobError> {
        if self.catalog.is_none() {
            let snapshot = segment_catalog(conn, self.workspace).await?;
            self.catalog = Some(catalog_of(snapshot));
        }
        self.catalog
            .as_ref()
            .map(|(catalog, _)| catalog)
            .ok_or_else(|| JobError::new("the catalog could not be read"))
    }

    /// What `item` matched, against the settings of the enrollment's pinned version.
    async fn matched(
        &mut self,
        conn: &mut PgConnection,
        enrollment: &Enrollment,
        settings: &FlowSettings,
        item: &SettleItem,
    ) -> Result<Matched, JobError> {
        match &item.signal {
            Signal::GoalSegment => Ok(Matched {
                goal: matches!(settings.goal, Some(Goal::Segment { .. })),
                ..Matched::default()
            }),
            Signal::LeftSegment => Ok(Matched {
                left_segment: true,
                ..Matched::default()
            }),
            Signal::Event {
                event_id,
                name,
                goal,
                exit,
            } => {
                let Some(event) =
                    find_event_of_person(conn, self.workspace, enrollment.person_id, *event_id)
                        .await?
                else {
                    return Ok(Matched::default());
                };
                let now = self.now;
                let catalog = self.catalog(conn).await?;
                let goal = *goal
                    && matches!(&settings.goal, Some(Goal::Event(wanted))
                        if wanted.name == *name
                            && filters_pass(catalog, name, &wanted.filters, &event, now));
                let exit_event = *exit
                    && settings.exit_criteria.iter().any(|criterion| {
                        matches!(criterion, ExitCriterion::Event(wanted)
                            if wanted.name == *name
                                && filters_pass(catalog, name, &wanted.filters, &event, now))
                    });
                Ok(Matched {
                    goal,
                    exit_event,
                    left_segment: false,
                })
            }
        }
    }

    async fn settle_one(
        &mut self,
        conn: &mut PgConnection,
        item: &SettleItem,
    ) -> Result<(), JobError> {
        let Some(enrollment) = lock_enrollment(conn, self.workspace, item.enrollment).await? else {
            // Erased with the person.
            return Ok(());
        };
        if enrollment.person_id != item.person {
            return Ok(());
        }
        let Some(state) = engine_state(&enrollment) else {
            return Ok(());
        };
        let Some(settings) = self.settings_of(conn, &enrollment).await? else {
            return Ok(());
        };
        let matched = self.matched(conn, &enrollment, &settings, item).await?;
        let Some(transition) = settle(&settings, &state, matched) else {
            return Ok(());
        };
        let mut decisions = Vec::new();
        if !transition.effects.is_empty() {
            let rows = record_steps(
                &mut *conn,
                self.workspace,
                enrollment.id,
                &[NewStepExecution {
                    node_id: GOAL_NODE,
                    outcome: StepOutcome::Done,
                    error: None,
                    attempt: self.at.1,
                }],
            )
            .await?;
            if let Some(row) = rows.first().filter(|row| row.fresh) {
                let mut decision = self.decision(
                    &enrollment,
                    &decision_actions::FLOW_GOAL_CONVERTED,
                    None,
                    GOAL_NODE,
                );
                decision.subject_ids.push(("step_execution", row.id));
                decisions.push(decision);
            }
        }
        if let Outcome::Exited { reason } = transition.outcome {
            let text = exit_reason_text(reason);
            let version = i64::try_from(transition.state.state_version)
                .map_err(|_| JobError::new("the state version does not fit"))?;
            let saved = save_enrollment(
                &mut *conn,
                self.workspace,
                enrollment.id,
                enrollment.state_version,
                &EnrollmentUpdate {
                    status: EnrollmentStatus::Exited,
                    node: &transition.state.node,
                    state_version: version,
                    wake_at: None,
                    exit_reason: Some(text),
                    error: None,
                    failed_node: None,
                    failure_placed: false,
                    resume_input: None,
                    now: self.now,
                },
            )
            .await?;
            if !saved {
                return Err(JobError::new("the enrollment moved under its lock"));
            }
            for change in &transition.waits {
                if let WaitChange::Clear { event_name } = change {
                    clear_waits(&mut *conn, enrollment.id, Some(event_name)).await?;
                }
            }
            // A held enrollment keeps its waits in the row, not the engine state.
            clear_waits(&mut *conn, enrollment.id, None).await?;
            decisions.push(self.decision(
                &enrollment,
                &decision_actions::FLOW_ENROLLMENT_EXITED,
                Some(text),
                &transition.state.node,
            ));
        }
        append_decisions(conn, &decisions).await?;
        Ok(())
    }

    fn decision(
        &self,
        enrollment: &Enrollment,
        spec: &'static DecisionSpec,
        detail: Option<&'static str>,
        node: &str,
    ) -> NewDecision {
        NewDecision {
            graph: Some(GraphPoint::at(enrollment.flow_version, node)),
            workspace_id: self.workspace,
            trace: enrollment.trace.clone(),
            spec,
            detail,
            subject_ids: vec![
                ("flow", enrollment.flow_id),
                ("enrollment", enrollment.id),
                ("person", enrollment.person_id),
            ],
            job: Some(self.at),
        }
    }
}

/// The engine's view of an enrollment the settings can act on. A held one is the state it was
/// held from; a failed, ended or awaiting one is not settled.
fn engine_state(enrollment: &Enrollment) -> Option<EnrollmentState> {
    let status = match enrollment.status {
        EnrollmentStatus::Active => Status::Active,
        EnrollmentStatus::Waiting => Status::Waiting,
        EnrollmentStatus::Held => {
            let input = enrollment
                .resume_input
                .as_ref()
                .and_then(StepJobInput::from_stored)?;
            match input.status_before() {
                EnrollmentStatus::Active => Status::Active,
                _ => Status::Waiting,
            }
        }
        EnrollmentStatus::Failed | EnrollmentStatus::Completed | EnrollmentStatus::Exited => {
            return None;
        }
    };
    Some(EnrollmentState {
        enrollment_id: enrollment.id,
        node: enrollment.node.clone(),
        state_version: u64::try_from(enrollment.state_version).ok()?,
        status,
        wake_at: None,
        awaiting: enrollment.awaiting.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Cause, TraceContext};

    use super::super::flow_step::tests::{Env, edge};
    use super::*;

    #[test]
    fn the_kind_name_is_flow_settle_and_it_is_registered() {
        assert_eq!(FlowSettle::KIND, "flow.settle");
        assert!(crate::Registry::built_in().get("flow.settle").is_some());
    }

    /// A wait of an hour, then the end: an enrollment parked where the settings can act on it.
    fn parked() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 3600},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "end")]})
    }

    fn settings(trigger: Value, extra: Value) -> Value {
        let mut wire = json!({"trigger": trigger, "reentry": {"mode": "once"}});
        if let (Some(target), Value::Object(more)) = (wire.as_object_mut(), extra) {
            target.extend(more);
        }
        wire
    }

    fn manual() -> Value {
        json!({"type": "manual"})
    }

    fn goal_event(filters: Value) -> Value {
        json!({"type": "event", "event": "order", "filters": filters})
    }

    /// Enrolls a person and runs the start, so the enrollment waits at the hour.
    async fn parked_person(env: &Env, flow: Uuid, email: &str) -> (Uuid, Uuid) {
        let person = env.person(email, json!({})).await;
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        env.run_matching(Some("start"), Some(id)).await;
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Waiting);
        (id, person)
    }

    async fn settle_jobs(env: &Env) -> usize {
        env.run_kind(SETTLE_JOB_KIND, &FlowSettle).await
    }

    async fn conversions(env: &Env) -> (i64, i64) {
        (
            env.count(
                "SELECT count(*) FROM decision_records \
                 WHERE action = 'flow.goal' AND outcome = 'converted'",
            )
            .await,
            env.count("SELECT count(*) FROM step_executions WHERE node_id = 'settings.goal'")
                .await,
        )
    }

    /// `count` people parked in a flow whose goal is an `order`, and one arrival batch with an
    /// order of each, which queues their settle jobs.
    async fn ordered(env: &Env, count: usize) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        use zirv_flow_store::EventRecord;

        env.define_event("order").await;
        let flow = env
            .flow_with(
                "Goal",
                parked(),
                settings(manual(), json!({"goal": goal_event(json!([]))})),
            )
            .await;
        let emails: Vec<String> = (0..count)
            .map(|n| format!("buyer{n}@example.com"))
            .collect();
        let mut people = Vec::with_capacity(emails.len());
        for email in &emails {
            people.push(env.person(email, json!({})).await);
        }
        env.enroll(flow, 1, &people).await;
        env.run_matching(Some("start"), None).await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let orders: Vec<EventRecord> = emails
            .iter()
            .map(|email| EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), email.clone())],
                name: "order".into(),
                properties: serde_json::Map::new(),
                timestamp: None,
                received_at: env.clock.now(),
                message_id: None,
                trace: TraceContext::root(Cause::api("order")),
            })
            .collect();
        zirv_flow_store::apply_event_batch(&env.pool, env.ws, &key, &orders)
            .await
            .unwrap();
    }

    /// The most subtransaction ids a recorded transaction cached stays within the 64 a backend
    /// caches.
    async fn assert_within_the_cache(env: &Env) {
        let (subxacts, overflowed, rows) =
            zirv_flow_store::subtransaction_testing::recorded(&env.pool).await;
        assert!(rows > 0, "no transaction recorded");
        assert!(
            !overflowed && subxacts <= 64,
            "a transaction cached {subxacts} subtransaction ids, overflowed: {overflowed}"
        );
    }

    /// One arrival batch that converts a hundred enrollments is settled in transactions that each
    /// stay within the 64 subtransaction ids a backend caches: an item takes one, its lock round,
    /// and a released one keeps it until the commit.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn settling_a_hundred_enrollments_never_overflows_the_subtransaction_cache(pool: PgPool) {
        let env = Env::new(pool).await;
        ordered(&env, 100).await;
        zirv_flow_store::subtransaction_testing::record_at_commit(&env.pool, "jobs").await;

        settle_jobs(&env).await;

        assert_within_the_cache(&env).await;
        assert_eq!(conversions(&env).await, (100, 100));
    }

    /// A settle job queued before the cap names up to 500 enrollments. It settles the first 60
    /// and hands the rest on in jobs of 60, so every transaction stays within the cache, and each
    /// enrollment converts once.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_settle_job_of_five_hundred_from_before_the_cap_hands_on_its_rest_and_settles_each_once(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        ordered(&env, 500).await;
        // The capped jobs the batch queued, made into one as an older version queued it.
        let payloads: Vec<Value> = sqlx::query_scalar(
            "WITH gone AS (DELETE FROM jobs WHERE kind = 'flow.settle' RETURNING id, payload) \
             SELECT payload FROM gone ORDER BY id",
        )
        .fetch_all(&env.pool)
        .await
        .unwrap();
        let items: Vec<Value> = payloads
            .iter()
            .flat_map(|payload| payload["items"].as_array().cloned().unwrap_or_default())
            .collect();
        assert_eq!(items.len(), 500);
        let mut legacy =
            zirv_flow_store::NewJob::new(SETTLE_JOB_KIND, TraceContext::root(Cause::api("order")));
        legacy.workspace_id = Some(env.ws);
        legacy.payload = json!({"workspace_id": env.ws, "items": items});
        legacy.unique_key = Some("flow.settle:before-the-cap".into());
        assert!(zirv_flow_store::enqueue(&env.pool, &legacy).await.unwrap());
        zirv_flow_store::subtransaction_testing::record_at_commit(&env.pool, "jobs").await;

        let ran = settle_jobs(&env).await;

        assert_within_the_cache(&env).await;
        assert_eq!(conversions(&env).await, (500, 500));
        assert_eq!(
            ran,
            1 + 440_usize.div_ceil(60),
            "the job and the ones it handed on"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_goal_converts_once_and_leaves_the_enrollment_running(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        let flow = env
            .flow_with(
                "Goal",
                parked(),
                settings(manual(), json!({"goal": goal_event(json!([]))})),
            )
            .await;
        let (id, _) = parked_person(&env, flow, "ada@example.com").await;
        let before = env.enrollment(id).await;

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        assert_eq!(settle_jobs(&env).await, 1);
        let after = env.enrollment(id).await;
        env.apply_event("ada@example.com", "order", json!({"total": 9}))
            .await;
        assert_eq!(settle_jobs(&env).await, 1);

        assert_eq!(
            (after.status, after.state_version, &after.node),
            (before.status, before.state_version, &before.node),
            "a goal without an exit leaves the state as it is"
        );
        assert_eq!(
            conversions(&env).await,
            (1, 1),
            "a goal reached twice converts once, as a decision and in the counts"
        );
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Waiting);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_goal_with_exit_on_goal_ends_the_enrollment_and_its_timer_then_does_nothing(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        let flow = env
            .flow_with(
                "Goal",
                parked(),
                settings(
                    manual(),
                    json!({"goal": goal_event(json!([])), "exit_on_goal": true}),
                ),
            )
            .await;
        let (id, _) = parked_person(&env, flow, "ada@example.com").await;
        let before = env.enrollment(id).await;

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        settle_jobs(&env).await;
        let exited = env.enrollment(id).await;
        env.advance(3601);
        env.run_due().await;

        assert_eq!(exited.status, EnrollmentStatus::Exited);
        assert_eq!(exited.exit_reason.as_deref(), Some("goal"));
        assert_eq!(exited.state_version, before.state_version + 1);
        assert!(exited.wake_at.is_none() && exited.ended_at.is_some());
        assert_eq!(
            conversions(&env).await,
            (1, 1),
            "the conversion is recorded"
        );
        let after = env.enrollment(id).await;
        assert_eq!(
            (after.status, after.state_version),
            (EnrollmentStatus::Exited, exited.state_version),
            "the timer of the older state found it stale"
        );
        assert_eq!(
            env.count("SELECT count(*) FROM step_executions WHERE node_id = 'end'")
                .await,
            0
        );
        let decisions = env.decisions(&exited.trace).await;
        assert!(decisions.contains(&("flow.enrollment".into(), "exited".into())));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_goal_applies_its_property_filters(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        let big = json!([{"prop": "total", "op": "gt", "value": 100}]);
        let flow = env
            .flow_with(
                "Big",
                parked(),
                settings(
                    manual(),
                    json!({"goal": goal_event(big), "exit_on_goal": true}),
                ),
            )
            .await;
        let (id, _) = parked_person(&env, flow, "ada@example.com").await;

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        settle_jobs(&env).await;
        let small = env.enrollment(id).await.status;
        env.apply_event("ada@example.com", "order", json!({"total": 500}))
            .await;
        settle_jobs(&env).await;

        assert_eq!(small, EnrollmentStatus::Waiting);
        assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Exited);
        assert_eq!(conversions(&env).await, (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_exit_criterion_ends_the_enrollment_without_a_conversion(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        env.define_event("unsubscribed").await;
        let leave = json!({"type": "event", "event": "unsubscribed", "filters": []});
        let flow = env
            .flow_with(
                "Exit",
                parked(),
                settings(manual(), json!({"exit_criteria": [leave]})),
            )
            .await;
        let (id, _) = parked_person(&env, flow, "ada@example.com").await;

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        settle_jobs(&env).await;
        let other = env.enrollment(id).await.status;
        env.apply_event("ada@example.com", "unsubscribed", json!({}))
            .await;
        settle_jobs(&env).await;

        assert_eq!(
            other,
            EnrollmentStatus::Waiting,
            "another event is no reason"
        );
        let exited = env.enrollment(id).await;
        assert_eq!(exited.status, EnrollmentStatus::Exited);
        assert_eq!(exited.exit_reason.as_deref(), Some("exit_event"));
        assert_eq!(conversions(&env).await, (0, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_goal_reached_criterion_exits_with_its_own_reason_and_converts(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_event("order").await;
        let flow = env
            .flow_with(
                "Criterion",
                parked(),
                settings(
                    manual(),
                    json!({"goal": goal_event(json!([])),
                           "exit_criteria": [{"type": "goal_reached"}]}),
                ),
            )
            .await;
        let (id, _) = parked_person(&env, flow, "ada@example.com").await;

        env.apply_event("ada@example.com", "order", json!({"total": 5}))
            .await;
        settle_jobs(&env).await;

        let exited = env.enrollment(id).await;
        assert_eq!(exited.status, EnrollmentStatus::Exited);
        assert_eq!(exited.exit_reason.as_deref(), Some("goal_criterion"));
        assert_eq!(conversions(&env).await, (1, 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn leaving_the_trigger_segment_exits_but_a_definition_change_exits_nothing(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_trait("plan", TraitType::String).await;
        let ann = env.person("ann@example.com", json!({"plan": "pro"})).await;
        let bob = env.person("bob@example.com", json!({"plan": "pro"})).await;
        let segment = env.plan_segment("Pros", "pro").await;
        let flow = env
            .flow_with(
                "Leaves",
                parked(),
                settings(
                    json!({"type": "segment", "segment_id": segment, "on": "entered"}),
                    json!({"exit_criteria": [{"type": "leaves_segment"}]}),
                ),
            )
            .await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        let enrolled = env.enroll(flow, 1, &[ann, bob]).await;
        for entry in &enrolled.enrolled {
            env.run_matching(Some("start"), Some(entry.enrollment))
                .await;
        }
        let of = |person: Uuid| {
            enrolled
                .enrolled
                .iter()
                .find(|entry| entry.person == person)
                .map(|entry| entry.enrollment)
                .unwrap()
        };

        // Ann's plan changes, and the next scheduled refresh sees her leave.
        env.set_plan(ann, "free").await;
        let schedule = TraceContext::root(Cause::schedule(Uuid::new_v4()));
        env.refresh(segment, "schedule", schedule).await;
        assert_eq!(settle_jobs(&env).await, 1);
        let left = env.enrollment(of(ann)).await;
        assert_eq!(left.status, EnrollmentStatus::Exited);
        assert_eq!(left.exit_reason.as_deref(), Some("left_segment"));
        assert_eq!(
            env.enrollment(of(bob)).await.status,
            EnrollmentStatus::Waiting
        );

        // An edit of the definition moves Bob out, and it is the edit's doing: nothing exits.
        env.resave_plan_segment(segment, "gold").await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        assert_eq!(settle_jobs(&env).await, 0, "no job for a definition change");
        assert_eq!(
            env.enrollment(of(bob)).await.status,
            EnrollmentStatus::Waiting
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn entering_the_goal_segment_converts_and_a_definition_change_does_not(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_trait("plan", TraitType::String).await;
        let ann = env.person("ann@example.com", json!({"plan": "free"})).await;
        let bob = env.person("bob@example.com", json!({"plan": "free"})).await;
        let gold = env.plan_segment("Gold", "gold").await;
        env.refresh(gold, "save", TraceContext::root(Cause::api("job")))
            .await;
        let flow = env
            .flow_with(
                "Upgrade",
                parked(),
                settings(
                    manual(),
                    json!({"goal": {"type": "segment", "segment_id": gold}, "exit_on_goal": true}),
                ),
            )
            .await;
        let enrolled = env.enroll(flow, 1, &[ann, bob]).await;
        for entry in &enrolled.enrolled {
            env.run_matching(Some("start"), Some(entry.enrollment))
                .await;
        }
        let of = |person: Uuid| {
            enrolled
                .enrolled
                .iter()
                .find(|entry| entry.person == person)
                .map(|entry| entry.enrollment)
                .unwrap()
        };

        env.set_plan(ann, "gold").await;
        let schedule = TraceContext::root(Cause::schedule(Uuid::new_v4()));
        env.refresh(gold, "schedule", schedule).await;
        assert_eq!(settle_jobs(&env).await, 1);

        let converted = env.enrollment(of(ann)).await;
        assert_eq!(converted.status, EnrollmentStatus::Exited);
        assert_eq!(converted.exit_reason.as_deref(), Some("goal"));
        assert_eq!(conversions(&env).await, (1, 1));
        assert_eq!(
            env.enrollment(of(bob)).await.status,
            EnrollmentStatus::Waiting
        );

        // The edit now matches the free people, and Bob enters the goal because of the edit.
        env.resave_plan_segment(gold, "free").await;
        env.refresh(gold, "save", TraceContext::root(Cause::api("job")))
            .await;
        assert_eq!(settle_jobs(&env).await, 0);
        assert_eq!(
            env.enrollment(of(bob)).await.status,
            EnrollmentStatus::Waiting
        );
        assert_eq!(conversions(&env).await, (1, 1));
    }
}
