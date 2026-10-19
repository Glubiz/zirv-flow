//! `flow.enroll`: enrolls up to 1,000 people in a flow (architecture 9.1 and 9.4).
//!
//! Every trigger ends here: a segment's entry or exit, a matching event, a schedule slot, a
//! manual start and an API request. The payload either lists `(person, occurrence)` pairs or
//! names an audience, which the job pages through, a page per job. The job calls
//! `store::enroll_entries`, which is idempotent on `(flow, person, occurrence)`, so a job
//! delivered twice, or two jobs that overlap, enroll nobody twice; and which enforces the
//! re-entry rule of the version, so a refusal is a decision with its reason and not an error.
//!
//! A paused flow takes no new enrollments. A job for people records each person's skip as a
//! decision; a job for an audience records one skip for the occurrence and does not page, since
//! naming every member of an audience a paused flow turns away would only copy the audience into
//! the decision log. Both are counted.

use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::Cause;
use zirv_flow_core::flow::{FlowSettings, Trigger};
use zirv_flow_segments::{
    Outcome as Verdict, PersonSnapshot, SnapshotEvent, evaluate, parse_and_validate,
};
use zirv_flow_store::{
    AudienceSource, ENROLL_JOB_KIND, EnrollEntries, EnrollEntry, EnrollJobPayload, EnrollOutcome,
    EnrollWork, FlowState, MAX_ENROLL_PEOPLE, TriggerEvent, TriggerKind, audience_page,
    enroll_entries, enroll_page_key, find_events, find_flow, find_flow_version, occurrence_of,
    resolve_survivors, segment_catalog,
};

use super::segment_refresh::catalog_of;
use crate::{JobContext, JobError, JobKind, metrics};

pub(crate) struct FlowEnroll;

impl JobKind for FlowEnroll {
    const KIND: &'static str = ENROLL_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = EnrollJobPayload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a flow enroll"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let Some(flow) = find_flow(&mut *ctx.conn(), job.workspace, job.flow).await? else {
            // Deleted with its workspace, or never there: nothing to enroll into.
            return Ok(());
        };
        let is_audience = matches!(job.work, EnrollWork::Audience { .. });
        let live = flow.state == FlowState::Published;
        // An audience for a flow that takes nobody is one skip; a flow with no version has
        // nothing to enroll into.
        let Some(version) = job
            .version
            .or(flow.current_version)
            .filter(|_| live || !is_audience)
        else {
            return skip_whole(ctx, &job, flow.state).await;
        };
        match &job.work {
            EnrollWork::People(pairs) => {
                run_people(ctx, &job, version, pairs, now).await?;
            }
            EnrollWork::Audience {
                source,
                occurrence,
                after,
                entered_by,
            } => {
                let occurrence = occurrence.unwrap_or_else(|| slot_occurrence(ctx, job.flow));
                let page = (*source, occurrence, *after, *entered_by);
                run_audience(ctx, &job, version, page, now).await?;
            }
        }
        Ok(())
    }
}

/// The occurrence of a job that has none: its schedule slot, which the slot's unique key names,
/// so a slot that fires twice is one occurrence. A job with no such key is its own occurrence.
fn slot_occurrence(ctx: &JobContext<'_>, flow: Uuid) -> Uuid {
    match ctx.unique_key() {
        Some(key) if key.starts_with("schedule:") => occurrence_of(&format!("{key}:{flow}")),
        _ => Uuid::now_v7(),
    }
}

/// The skip of a whole job: one decision on the flow, counted once.
async fn skip_whole(
    ctx: &mut JobContext<'_>,
    job: &EnrollJobPayload,
    state: FlowState,
) -> Result<(), JobError> {
    let reason = match state {
        FlowState::Paused => "flow_paused",
        _ => "flow_not_live",
    };
    metrics::record_enroll_skips(job.trigger.as_str(), reason, 1);
    ctx.record_decision(
        "flow.enroll",
        "skipped",
        Some(reason),
        vec![("flow", job.flow)],
    )
    .await
}

async fn run_people(
    ctx: &mut JobContext<'_>,
    job: &EnrollJobPayload,
    version: i32,
    pairs: &[(Uuid, Uuid)],
    now: OffsetDateTime,
) -> Result<(), JobError> {
    if pairs.len() > MAX_ENROLL_PEOPLE {
        return Err(JobError::new("a flow enroll takes at most 1000 people"));
    }
    // The entries keep the ids the job names: `enroll_entries` resolves a person merged into
    // another since the job was queued to the survivor, and tells a redelivery from a new
    // occurrence by the named id as well as the survivor's.
    let mut entries: Vec<EnrollEntry> = pairs
        .iter()
        .map(|(person, occurrence)| EnrollEntry {
            person: *person,
            occurrence: *occurrence,
            // An event starts a flow on its own cause; the other triggers share the job's.
            cause: (job.trigger == TriggerKind::Event).then(|| Cause::event(*occurrence)),
        })
        .collect();
    if job.trigger == TriggerKind::Event {
        entries = matching_events(ctx, job, version, entries, now).await?;
    }
    enroll(ctx, job, version, &entries, now).await?;
    Ok(())
}

/// The entries whose event passes the trigger's name and property filters. The store cannot read
/// the segment AST, so the filters are applied here with the reference evaluator over a snapshot
/// that holds just that event, as a wait's are.
async fn matching_events(
    ctx: &mut JobContext<'_>,
    job: &EnrollJobPayload,
    version: i32,
    entries: Vec<EnrollEntry>,
    now: OffsetDateTime,
) -> Result<Vec<EnrollEntry>, JobError> {
    let stored = find_flow_version(&mut *ctx.conn(), job.workspace, job.flow, version)
        .await?
        .ok_or_else(|| JobError::new("the flow version is missing"))?;
    let settings: FlowSettings = serde_json::from_value(stored.settings)
        .map_err(|_| JobError::new("the flow settings are unusable"))?;
    let Trigger::Event(wanted) = settings.trigger else {
        // The flow was republished with another trigger since the event: it starts nothing.
        return Ok(Vec::new());
    };
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.occurrence).collect();
    let events = find_events(ctx.conn(), job.workspace, &ids).await?;
    let (catalog, _) = catalog_of(segment_catalog(ctx.conn(), job.workspace).await?);
    let wire = serde_json::json!({"v": 1, "event": wanted.name, "where": wanted.filters});
    // Something the filters read was archived since the publish: no event can match.
    let definition = if wanted.filters.is_empty() {
        None
    } else {
        match parse_and_validate(&wire, &catalog) {
            Ok(definition) => Some(definition),
            Err(_) => return Ok(Vec::new()),
        }
    };
    // The entries name the survivor; an event may still name the person it was recorded for,
    // until the merge's rewrite job re-points it. Both sides are compared as the survivor.
    let recorded_for: Vec<Uuid> = events
        .iter()
        .map(|event| event.person_id)
        .chain(entries.iter().map(|entry| entry.person))
        .collect();
    let survivors = resolve_survivors(ctx.conn(), job.workspace, &recorded_for).await?;
    let passing: hashbrown::HashSet<(Uuid, Uuid)> = events
        .iter()
        .filter(|event| event.name == wanted.name)
        .filter(|event| {
            definition.as_ref().is_none_or(|definition| {
                let snapshot = PersonSnapshot {
                    events: vec![snapshot_event(event)],
                    ..PersonSnapshot::default()
                };
                evaluate(definition, &catalog, &snapshot, now).outcome == Verdict::Pass
            })
        })
        .map(|event| {
            let person = survivors.get(&event.person_id).copied();
            (person.unwrap_or(event.person_id), event.id)
        })
        .collect();
    Ok(entries
        .into_iter()
        .filter(|entry| {
            let person = survivors.get(&entry.person).copied();
            passing.contains(&(person.unwrap_or(entry.person), entry.occurrence))
        })
        .collect())
}

fn snapshot_event(event: &TriggerEvent) -> SnapshotEvent {
    SnapshotEvent {
        name: event.name.clone(),
        occurred_at: event.occurred_at,
        properties: event.properties.as_object().cloned().unwrap_or_default(),
    }
}

/// One page of an audience, and the job for the next page when this one was full. The next job
/// carries the occurrence and the version, so every page enrolls for one occurrence into one
/// version, and its key names the cursor, so a page queued twice is one job.
async fn run_audience(
    ctx: &mut JobContext<'_>,
    job: &EnrollJobPayload,
    version: i32,
    (source, occurrence, after, entered_by): (
        AudienceSource,
        Uuid,
        Option<Uuid>,
        Option<OffsetDateTime>,
    ),
    now: OffsetDateTime,
) -> Result<(), JobError> {
    let page = audience_page(ctx.conn(), job.workspace, source, after, entered_by).await?;
    let entries: Vec<EnrollEntry> = page
        .iter()
        .map(|person| EnrollEntry {
            person: *person,
            occurrence,
            cause: None,
        })
        .collect();
    enroll(ctx, job, version, &entries, now).await?;
    if page.len() >= MAX_ENROLL_PEOPLE
        && let Some(last) = page.last()
    {
        let next = EnrollJobPayload {
            workspace: job.workspace,
            flow: job.flow,
            version: Some(version),
            trigger: job.trigger,
            work: EnrollWork::Audience {
                source,
                occurrence: Some(occurrence),
                after: Some(*last),
                entered_by,
            },
        };
        let key = enroll_page_key(job.flow, occurrence, Some(*last));
        let trace = ctx.trace().clone();
        ctx.enqueue(&next.job(&trace, key)).await?;
    }
    Ok(())
}

async fn enroll(
    ctx: &mut JobContext<'_>,
    job: &EnrollJobPayload,
    version: i32,
    entries: &[EnrollEntry],
    now: OffsetDateTime,
) -> Result<EnrollOutcome, JobError> {
    let trace = ctx.trace().clone();
    let outcome = enroll_entries(
        ctx.conn(),
        &EnrollEntries {
            workspace: job.workspace,
            flow: job.flow,
            version,
            trigger: job.trigger.as_str(),
            entries,
            now,
            trace: &trace,
        },
    )
    .await
    .map_err(|error| match error {
        zirv_flow_store::EnrollError::NoSuchVersion => JobError::new("the flow version is missing"),
        zirv_flow_store::EnrollError::TooMany => {
            JobError::new("a flow enroll takes at most 1000 people")
        }
        zirv_flow_store::EnrollError::Corrupt(_) => JobError::new("the stored flow is unusable"),
        zirv_flow_store::EnrollError::Store(error) => JobError::from(error),
    })?;
    metrics::record_enroll(job.trigger.as_str(), &outcome);
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use metrics_exporter_prometheus::PrometheusBuilder;
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as Span;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{
        HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceId, WorkspaceName,
    };
    use zirv_flow_segments::{compile, normalize};
    use zirv_flow_store::{
        ClaimedJob, Clock, EventRecord, ManualClock, NewEventSchema, NewFlow, NewJob, NewSegment,
        NewSegmentVersion, NewTraitDefinition, REFRESH_JOB_KIND, SegmentPatch, claim_job,
        complete_job, enqueue,
    };

    use super::super::segment_refresh::SegmentRefresh;
    use super::*;
    use crate::{Scheduler, SchedulerConfig};

    #[test]
    fn the_kind_name_is_flow_enroll_and_it_is_registered() {
        assert_eq!(FlowEnroll::KIND, "flow.enroll");
        assert!(crate::Registry::built_in().get("flow.enroll").is_some());
    }

    /// One workspace, one clock and workers that run `flow.enroll` jobs by hand.
    struct Env {
        pool: PgPool,
        ws: WorkspaceId,
        clock: ManualClock,
    }

    fn exit_graph() -> Value {
        json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []})
    }

    fn settings(trigger: Value, reentry: Value) -> Value {
        json!({"trigger": trigger, "reentry": reentry})
    }

    /// Runs one claimed `flow.enroll` as a worker does: its work commits with its completion.
    async fn execute(pool: &PgPool, clock: &Clock, claimed: ClaimedJob) -> Result<(), JobError> {
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let result = {
            let mut ctx = JobContext::new(&mut tx, &claimed).with_clock(clock.clone());
            FlowEnroll.run(&mut ctx, claimed.payload.clone()).await
        };
        result?;
        complete_job(&mut *tx, claimed.id, claimed.lease_token)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        Ok(())
    }

    impl Env {
        async fn new(pool: PgPool) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let name = WorkspaceName::parse("ours").unwrap();
            let slug = Slug::parse("ours").unwrap();
            let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
                .await
                .unwrap()
                .id;
            let now = OffsetDateTime::now_utc();
            let now = now.replace_nanosecond(now.microsecond() * 1000).unwrap();
            Self {
                pool,
                ws,
                clock: ManualClock::new(now),
            }
        }

        fn clock(&self) -> Clock {
            Clock::Manual(self.clock.clone())
        }

        async fn define_plan(&self) {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &NewTraitDefinition {
                    key: "plan".into(),
                    trait_type: TraitType::String,
                    precedence: Precedence::Latest,
                    source_order: Vec::new(),
                    pii: false,
                    channel_address: false,
                    inferred: false,
                    description: String::new(),
                },
            )
            .await
            .unwrap();
        }

        async fn define_order(&self) {
            zirv_flow_store::create_event_schema(
                &self.pool,
                self.ws,
                &NewEventSchema {
                    name: "order".into(),
                    mode: EventMode::Warn,
                    description: String::new(),
                    properties: serde_json::from_value(
                        json!([{"name": "total", "type": "number"}]),
                    )
                    .unwrap(),
                },
            )
            .await
            .unwrap();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::publish_schema_draft(&mut connection, self.ws, "order", None)
                .await
                .unwrap();
        }

        /// People with a `plan` trait, with one set-based statement.
        async fn people(&self, count: i32, plan: &str) {
            sqlx::query(
                "INSERT INTO people (id, workspace_id, traits) \
                 SELECT gen_random_uuid(), $1, jsonb_build_object('plan', $2::text) \
                 FROM generate_series(1, $3)",
            )
            .bind(self.ws.as_uuid())
            .bind(plan)
            .bind(count)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        async fn person(&self, plan: &str) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO people (id, workspace_id, traits) \
                 VALUES ($1, $2, jsonb_build_object('plan', $3::text))",
            )
            .bind(id)
            .bind(self.ws.as_uuid())
            .bind(plan)
            .execute(&self.pool)
            .await
            .unwrap();
            id
        }

        async fn set_plan(&self, from: &str, to: &str) {
            sqlx::query(
                "UPDATE people SET traits = jsonb_build_object('plan', $3::text) \
                 WHERE workspace_id = $1 AND traits ->> 'plan' = $2",
            )
            .bind(self.ws.as_uuid())
            .bind(from)
            .bind(to)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        /// Creates a flow and publishes its first version.
        async fn flow(&self, name: &str, settings: Value) -> Uuid {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id,
                    name: name.into(),
                    graph: exit_graph(),
                    settings,
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            self.publish(id).await;
            id
        }

        async fn publish(&self, id: Uuid) -> i32 {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, id)
                .await
                .unwrap();
            let trace = TraceContext::root(Cause::api("publish"));
            let (_, version) = zirv_flow_store::publish_flow(
                &mut tx,
                &self.clock(),
                self.ws,
                &locked,
                None,
                &trace,
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            version.version
        }

        async fn pause(&self, id: Uuid) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::pause_flow(&mut tx, self.ws, id, &TraceContext::root(Cause::api("p")))
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        async fn resume(&self, id: Uuid) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::resume_flow(
                &mut tx,
                self.ws,
                id,
                &TraceContext::root(Cause::api("r")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// What the API does when it saves a definition.
        async fn checked(&self, definition: Value) -> NewSegmentVersion {
            let mut connection = self.pool.acquire().await.unwrap();
            let (catalog, storage) =
                catalog_of(segment_catalog(&mut connection, self.ws).await.unwrap());
            let parsed = parse_and_validate(&definition, &catalog).unwrap();
            let compiled = compile(&parsed, &catalog, &storage, OffsetDateTime::now_utc()).unwrap();
            NewSegmentVersion {
                definition,
                normalized: normalize(parsed).to_value(),
                deps: compiled.deps.to_value(),
                time_sensitive: compiled.time_sensitive,
            }
        }

        async fn segment(&self, name: &str, definition: Value) -> Uuid {
            let content = self.checked(definition).await;
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::lock_segments(&mut tx, self.ws)
                .await
                .unwrap();
            let (segment, _) = zirv_flow_store::create_segment(
                &mut tx,
                self.ws,
                &NewSegment {
                    id: Uuid::now_v7(),
                    name: name.into(),
                    description: String::new(),
                    cadence_minutes: 60,
                },
                &content,
                None,
                &TraceContext::root(Cause::api("save")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            segment.id
        }

        async fn resave(&self, segment: Uuid, definition: Value) {
            let content = self.checked(definition).await;
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::lock_segments(&mut tx, self.ws)
                .await
                .unwrap();
            zirv_flow_store::update_segment(
                &mut tx,
                self.ws,
                segment,
                &SegmentPatch {
                    version: Some(content),
                    ..SegmentPatch::default()
                },
                None,
                &TraceContext::root(Cause::api("save")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// Refreshes a segment as its job does, under `trace`.
        async fn refresh(&self, segment: Uuid, trigger: &str, trace: TraceContext) {
            sqlx::query("DELETE FROM jobs WHERE kind = 'segment.refresh'")
                .execute(&self.pool)
                .await
                .unwrap();
            let mut job = NewJob::new(REFRESH_JOB_KIND, trace);
            job.workspace_id = Some(self.ws);
            job.payload =
                json!({"workspace_id": self.ws, "segment_id": segment, "trigger": trigger});
            job.unique_key = Some(format!("test:{}", Uuid::new_v4()));
            enqueue(&self.pool, &job).await.unwrap();
            let claimed = claim_job(
                &self.pool,
                "w",
                &[REFRESH_JOB_KIND],
                Duration::from_secs(60),
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed).with_clock(self.clock());
            SegmentRefresh
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// Runs the ready `flow.enroll` jobs with `workers` concurrent workers, until none is
        /// left, and returns how many ran.
        async fn drain(&self, workers: usize) -> usize {
            if workers == 1 {
                // On this task, so a test that watches the metrics sees them.
                let clock = self.clock();
                let mut ran = 0;
                while let Some(claimed) = claim_job(
                    &self.pool,
                    "test",
                    &[ENROLL_JOB_KIND],
                    Duration::from_secs(120),
                    None,
                )
                .await
                .unwrap()
                {
                    execute(&self.pool, &clock, claimed).await.unwrap();
                    ran += 1;
                }
                return ran;
            }
            let tasks: Vec<_> = (0..workers)
                .map(|_| {
                    let pool = self.pool.clone();
                    let clock = self.clock();
                    tokio::spawn(async move {
                        let mut ran = 0;
                        while let Some(claimed) = claim_job(
                            &pool,
                            "test",
                            &[ENROLL_JOB_KIND],
                            Duration::from_secs(120),
                            None,
                        )
                        .await
                        .unwrap()
                        {
                            execute(&pool, &clock, claimed).await.unwrap();
                            ran += 1;
                        }
                        ran
                    })
                })
                .collect();
            let mut total = 0;
            for task in tasks {
                total += task.await.unwrap();
            }
            total
        }

        /// Queues a payload as the API or an operator would, and runs it.
        async fn run(&self, payload: &EnrollJobPayload, trace: &TraceContext) {
            let unique = format!("test:{}", Uuid::new_v4());
            enqueue(&self.pool, &payload.job(trace, unique))
                .await
                .unwrap();
            assert_eq!(self.drain(1).await, 1);
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        async fn enroll_jobs(&self) -> i64 {
            self.count("SELECT count(*) FROM jobs WHERE kind = 'flow.enroll'")
                .await
        }

        async fn decisions(&self, outcome: &str, reason: Option<&str>) -> i64 {
            sqlx::query_scalar(
                "SELECT count(*) FROM decision_records WHERE workspace_id = $1 \
                 AND action = 'flow.enroll' AND outcome = $2 AND reason IS NOT DISTINCT FROM $3",
            )
            .bind(self.ws.as_uuid())
            .bind(outcome)
            .bind(reason)
            .fetch_one(&self.pool)
            .await
            .unwrap()
        }

        /// The people with an enrollment in the flow, sorted.
        async fn enrolled(&self, flow: Uuid) -> Vec<Uuid> {
            sqlx::query_scalar(
                "SELECT person_id FROM enrollments WHERE flow_id = $1 ORDER BY person_id",
            )
            .bind(flow)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        /// The live members of a segment, sorted.
        async fn members(&self, segment: Uuid) -> Vec<Uuid> {
            sqlx::query_scalar(
                "SELECT person_id FROM segment_members                  WHERE segment_id = $1 AND exited_at IS NULL ORDER BY person_id",
            )
            .bind(segment)
            .fetch_all(&self.pool)
            .await
            .unwrap()
        }

        async fn end_enrollments(&self, flow: Uuid) {
            sqlx::query(
                "UPDATE enrollments SET state = 'completed', ended_at = now() WHERE flow_id = $1",
            )
            .bind(flow)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    fn plan_is(plan: &str) -> Value {
        json!({"op": "eq", "trait": "plan", "value": plan, "v": 1})
    }

    fn entered_trigger(segment: Uuid, on: &str) -> Value {
        json!({"type": "segment", "segment_id": segment, "on": on})
    }

    fn once() -> Value {
        json!({"mode": "once"})
    }

    fn schedule_trace() -> (Uuid, TraceContext) {
        let id = Uuid::new_v4();
        (id, TraceContext::root(Cause::schedule(id)))
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn entering_a_segment_enrolls_each_newcomer_for_the_event_that_recorded_it(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        let (ann, bob, cy) = (
            env.person("free").await,
            env.person("free").await,
            env.person("pro").await,
        );
        let segment = env.segment("Pros", plan_is("pro")).await;
        let flow = env
            .flow(
                "Welcome",
                settings(entered_trigger(segment, "entered"), once()),
            )
            .await;
        // The first computation is caused by the definition: Cy was there from the start.
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        assert_eq!(env.enroll_jobs().await, 0);
        env.set_plan("free", "pro").await;
        let (schedule, trace) = schedule_trace();

        env.refresh(segment, "schedule", trace).await;

        assert_eq!(env.enroll_jobs().await, 1, "one job for the two newcomers");
        assert_eq!(env.drain(1).await, 1);
        let mut expected = vec![ann, bob];
        expected.sort_unstable();
        assert_eq!(env.enrolled(flow).await, expected);
        assert!(!env.enrolled(flow).await.contains(&cy));
        // Each enrolled for the id of the event that recorded their entry, under the cause of
        // the membership change.
        let rows: Vec<(Uuid, Uuid, String, String, String)> = sqlx::query_as(
            "SELECT e.person_id, e.occurrence_id, e.cause_kind, e.cause_id, e.context ->> 'trigger' \
             FROM enrollments e WHERE e.flow_id = $1",
        )
        .bind(flow)
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        for (person, occurrence, kind, cause, trigger) in rows {
            let event: Uuid = sqlx::query_scalar(
                "SELECT id FROM events WHERE name = 'segment.entered' AND person_id = $1 \
                 ORDER BY occurred_at DESC LIMIT 1",
            )
            .bind(person)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(occurrence, event);
            assert_eq!(
                (kind.as_str(), cause.as_str(), trigger.as_str()),
                ("schedule", schedule.to_string().as_str(), "segment")
            );
        }
        assert_eq!(env.decisions("enrolled", None).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn leaving_a_segment_enrolls_for_the_exit_event_and_entering_enrolls_nobody(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        let (ann, bob) = (env.person("pro").await, env.person("pro").await);
        let segment = env.segment("Pros", plan_is("pro")).await;
        let leaving = env
            .flow(
                "Win back",
                settings(entered_trigger(segment, "exited"), once()),
            )
            .await;
        let entering = env
            .flow(
                "Welcome",
                settings(entered_trigger(segment, "entered"), once()),
            )
            .await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        sqlx::query("UPDATE people SET traits = '{\"plan\": \"free\"}' WHERE id = $1")
            .bind(ann)
            .execute(&env.pool)
            .await
            .unwrap();
        let (_, trace) = schedule_trace();

        env.refresh(segment, "schedule", trace).await;
        env.drain(1).await;

        assert_eq!(env.enrolled(leaving).await, [ann]);
        assert!(env.enrolled(entering).await.is_empty());
        let exit_event: Uuid =
            sqlx::query_scalar("SELECT id FROM events WHERE name = 'segment.exited'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let occurrence: Uuid = sqlx::query_scalar("SELECT occurrence_id FROM enrollments")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        assert_eq!(occurrence, exit_event);
        assert_ne!(ann, bob);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_definition_edit_triggers_no_flow(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        env.person("pro").await;
        env.person("free").await;
        let segment = env.segment("Plan", plan_is("pro")).await;
        env.flow("In", settings(entered_trigger(segment, "entered"), once()))
            .await;
        env.flow("Out", settings(entered_trigger(segment, "exited"), once()))
            .await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;

        // The edit moves the free person in and the pro person out, and it is the edit's doing.
        env.resave(segment, plan_is("free")).await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;

        assert_eq!(
            env.count("SELECT count(*) FROM events WHERE name LIKE 'segment.%'")
                .await,
            3
        );
        assert_eq!(env.enroll_jobs().await, 0);
        assert_eq!(env.count("SELECT count(*) FROM enrollments").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publishing_enrolls_the_current_members_in_batches_for_the_publish(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        env.people(2_500, "pro").await;
        let segment = env.segment("Pros", plan_is("pro")).await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        assert_eq!(
            env.count("SELECT member_count::bigint FROM segments").await,
            2_500
        );
        let trigger = json!({
            "type": "segment", "segment_id": segment, "on": "entered", "enroll_current": true
        });

        let flow = env.flow("Welcome", settings(trigger, once())).await;
        let ran = env.drain(1).await;

        assert_eq!(ran, 3, "2,500 members make pages of 1,000, 1,000 and 500");
        assert_eq!(env.enrolled(flow).await.len(), 2_500);
        let publish_trace: Uuid =
            sqlx::query_scalar("SELECT trace_id FROM flow_versions WHERE flow_id = $1")
                .bind(flow)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(
            env.count(&format!(
                "SELECT count(DISTINCT occurrence_id) FROM enrollments \
                 WHERE occurrence_id = '{publish_trace}'"
            ))
            .await,
            1
        );
        // A second publish fans out again, and `once` refuses everyone who entered, even once
        // their enrollment has ended.
        env.end_enrollments(flow).await;
        env.publish(flow).await;
        assert_eq!(env.drain(1).await, 3);
        assert_eq!(env.enrolled(flow).await.len(), 2_500);
        assert_eq!(
            env.decisions("skipped", Some("already_entered")).await,
            2_500
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_who_joins_after_the_publish_is_not_enrolled_for_the_publish(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        env.people(1_200, "pro").await;
        let segment = env.segment("Pros", plan_is("pro")).await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        let trigger = json!({
            "type": "segment", "segment_id": segment, "on": "entered", "enroll_current": true
        });
        let flow = env.flow("Welcome", settings(trigger, once())).await;
        let at_publish = env.members(segment).await;
        assert_eq!(at_publish.len(), 1_200);

        // The first page runs, then a person whose id is after its cursor joins the segment.
        let claimed = claim_job(
            &env.pool,
            "test",
            &[ENROLL_JOB_KIND],
            Duration::from_secs(120),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        execute(&env.pool, &env.clock(), claimed).await.unwrap();
        env.clock.advance(time::Duration::seconds(5));
        let joiner = Uuid::from_u128(u128::MAX);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(joiner)
            .bind(env.ws.as_uuid())
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version,                  entered_at, entered_trace_id, entered_cause)              VALUES ($1, $2, $3, 1, $4, gen_random_uuid(), 'time')",
        )
        .bind(env.ws.as_uuid())
        .bind(segment)
        .bind(joiner)
        .bind(env.clock.now())
        .execute(&env.pool)
        .await
        .unwrap();
        assert_eq!(env.drain(1).await, 1, "the second page");

        let enrolled = env.enrolled(flow).await;
        assert!(
            !enrolled.contains(&joiner),
            "the joiner is not a publish enrollment"
        );
        assert_eq!(enrolled, at_publish, "every member at the publish, once");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_publish_waits_for_an_open_refresh_and_enrolls_the_member_it_adds(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_plan().await;
        env.people(1, "pro").await;
        let segment = env.segment("Pros", plan_is("pro")).await;
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        let early = env.members(segment).await;
        assert_eq!(early.len(), 1);
        let late = env.person("pro").await;
        let trigger = json!({
            "type": "segment", "segment_id": segment, "on": "entered", "enroll_current": true
        });
        let flow = Uuid::now_v7();
        let mut connection = env.pool.acquire().await.unwrap();
        zirv_flow_store::create_flow(
            &mut connection,
            env.ws,
            &NewFlow {
                id: flow,
                name: "Welcome".into(),
                graph: exit_graph(),
                settings: settings(trigger, once()),
            },
            None,
        )
        .await
        .unwrap();
        drop(connection);

        // Connection A refreshes the segment as its job does, and stays open: it holds the
        // refresh lock and has read no flow that triggers on the segment.
        sqlx::query("DELETE FROM jobs WHERE kind = 'segment.refresh'")
            .execute(&env.pool)
            .await
            .unwrap();
        let mut job = NewJob::new(REFRESH_JOB_KIND, TraceContext::root(Cause::api("job")));
        job.workspace_id = Some(env.ws);
        job.payload = json!({
            "workspace_id": env.ws, "segment_id": segment, "trigger": "save"
        });
        job.unique_key = Some(format!("test:{}", Uuid::new_v4()));
        enqueue(&env.pool, &job).await.unwrap();
        let claimed = claim_job(
            &env.pool,
            "w",
            &[REFRESH_JOB_KIND],
            Duration::from_secs(60),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut refresh = zirv_flow_store::begin(&env.pool).await.unwrap();
        {
            let mut ctx = JobContext::new(&mut refresh, &claimed).with_clock(env.clock());
            SegmentRefresh
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
        }

        // Connection B publishes after the refresh stamped the new member.
        env.clock.advance(time::Duration::seconds(5));
        let (backend_tx, backend_rx) = tokio::sync::oneshot::channel::<i32>();
        let publisher = {
            let (pool, clock, ws) = (env.pool.clone(), env.clock(), env.ws);
            tokio::spawn(async move {
                let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
                // The backend that publishes, so the test can ask whether it is the one waiting.
                let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                backend_tx.send(backend).unwrap();
                let locked = zirv_flow_store::lock_flow(&mut tx, ws, flow).await.unwrap();
                let trace = TraceContext::root(Cause::api("publish"));
                zirv_flow_store::publish_flow(&mut tx, &clock, ws, &locked, None, &trace)
                    .await
                    .unwrap();
                zirv_flow_store::commit(tx).await.unwrap();
            })
        };
        let backend = backend_rx.await.expect("the publisher reports its backend");
        let deadline = Instant::now() + Duration::from_secs(30);
        let finished = loop {
            if publisher.is_finished() {
                break true;
            }
            // The publisher's own backend, not any waiter on the server: other tests lock in
            // databases of their own at the same time.
            let blockers: i32 = sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1))")
                .bind(backend)
                .fetch_one(&env.pool)
                .await
                .unwrap();
            if blockers > 0 {
                break false;
            }
            assert!(
                Instant::now() < deadline,
                "the publish neither ended nor waited"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        if finished {
            // The publish did not wait: its pages ran without the new member.
            publisher.await.unwrap();
            env.drain(1).await;
            zirv_flow_store::commit(refresh).await.unwrap();
        } else {
            zirv_flow_store::commit(refresh).await.unwrap();
            publisher.await.unwrap();
            env.drain(1).await;
        }

        let mut expected = vec![early[0], late];
        expected.sort();
        assert_eq!(env.enrolled(flow).await, expected, "each member, once");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_trigger_enrolls_the_people_whose_event_passes_its_filters(pool: PgPool) {
        let env = Env::new(pool).await;
        env.define_order().await;
        let trigger = json!({
            "type": "event", "event": "order",
            "filters": [{"prop": "total", "op": "gt", "value": 100}]
        });
        let flow = env.flow("Big orders", settings(trigger, once())).await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let record = |email: &str, name: &str, total: i64| EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), email.into())],
            name: name.into(),
            properties: json!({"total": total}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: env.clock.now(),
            message_id: None,
            trace: TraceContext::root(Cause::api("event")),
        };
        // One batch: a big order, a small one, and an event of another name.
        zirv_flow_store::apply_event_batch(
            &env.pool,
            env.ws,
            &key,
            &[
                record("ada@example.com", "order", 150),
                record("bob@example.com", "order", 50),
                record("cy@example.com", "refund", 500),
            ],
        )
        .await
        .unwrap();
        assert_eq!(env.enroll_jobs().await, 1, "one job for the whole batch");
        // The job is delivered twice, as a lost lease would.
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), workspace_id, kind, payload, trace_id, cause_kind, cause_id \
             FROM jobs WHERE kind = 'flow.enroll'",
        )
        .execute(&env.pool)
        .await
        .unwrap();

        assert_eq!(env.drain(1).await, 2);

        let ada: Uuid = sqlx::query_scalar(
            "SELECT person_id FROM identities WHERE kind = 'email' AND value = 'ada@example.com'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(env.enrolled(flow).await, [ada]);
        let (occurrence, kind, cause): (Uuid, String, String) = sqlx::query_as(
            "SELECT occurrence_id, cause_kind, cause_id FROM enrollments WHERE flow_id = $1",
        )
        .bind(flow)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let event: Uuid =
            sqlx::query_scalar("SELECT id FROM events WHERE name = 'order' AND person_id = $1")
                .bind(ada)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(occurrence, event);
        assert_eq!((kind.as_str(), cause), ("event", event.to_string()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_slot_enrolls_everyone_once_even_when_it_fires_twice(pool: PgPool) {
        let env = Env::new(pool).await;
        sqlx::query("DELETE FROM schedules WHERE flow_id IS NULL")
            .execute(&env.pool)
            .await
            .unwrap();
        let (ann, bob, cy) = (
            env.person("a").await,
            env.person("b").await,
            env.person("c").await,
        );
        let trigger = json!({
            "type": "schedule", "cron": "0 * * * *", "timezone": "UTC",
            "audience": {"type": "everyone"}
        });
        let flow = env
            .flow("Hourly", settings(trigger, json!({"mode": "always"})))
            .await;
        let (schedule, slot): (Uuid, OffsetDateTime) =
            sqlx::query_as("SELECT id, next_run_at FROM schedules WHERE flow_id = $1")
                .bind(flow)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let mut leader = Scheduler::new(env.pool.clone(), SchedulerConfig::default());

        leader.tick_at(slot).await.unwrap();
        assert_eq!(env.drain(1).await, 1);
        // The slot fires again, as after a failover that lost the schedule's advance.
        sqlx::query("UPDATE schedules SET next_run_at = $1 WHERE id = $2")
            .bind(slot)
            .bind(schedule)
            .execute(&env.pool)
            .await
            .unwrap();
        leader.tick_at(slot).await.unwrap();
        assert_eq!(env.drain(1).await, 1);

        let mut expected = vec![ann, bob, cy];
        expected.sort_unstable();
        assert_eq!(env.enrolled(flow).await, expected);
        let occurrences = env
            .count("SELECT count(DISTINCT occurrence_id) FROM enrollments")
            .await;
        assert_eq!(occurrences, 1, "the slot is the one occurrence");
        let causes: Vec<(String, String)> =
            sqlx::query_as("SELECT DISTINCT cause_kind, cause_id FROM enrollments")
                .fetch_all(&env.pool)
                .await
                .unwrap();
        assert_eq!(causes, [("schedule".to_owned(), schedule.to_string())]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_manual_start_enrolls_a_list_and_an_api_request_one_person(pool: PgPool) {
        let env = Env::new(pool).await;
        let (ann, bob, cy) = (
            env.person("a").await,
            env.person("b").await,
            env.person("c").await,
        );
        let list = zirv_flow_store::create_list(&env.pool, env.ws, "VIPs", "", None)
            .await
            .unwrap()
            .id;
        for person in [ann, bob] {
            sqlx::query(
                "INSERT INTO list_members (workspace_id, list_id, person_id, source) \
                 VALUES ($1, $2, $3, 'api')",
            )
            .bind(env.ws.as_uuid())
            .bind(list)
            .bind(person)
            .execute(&env.pool)
            .await
            .unwrap();
        }
        let manual = env
            .flow("By hand", settings(json!({"type": "manual"}), once()))
            .await;
        let api = env
            .flow("By api", settings(json!({"type": "api"}), once()))
            .await;
        let occurrence = Uuid::now_v7();

        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow: manual,
                version: None,
                trigger: TriggerKind::Manual,
                work: EnrollWork::Audience {
                    source: AudienceSource::List(list),
                    occurrence: Some(occurrence),
                    after: None,
                    entered_by: None,
                },
            },
            &TraceContext::root(Cause::api("start")),
        )
        .await;
        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow: api,
                version: None,
                trigger: TriggerKind::Api,
                work: EnrollWork::People(vec![(cy, occurrence_of("key"))]),
            },
            &TraceContext::root(Cause::api("req")),
        )
        .await;

        let mut members = vec![ann, bob];
        members.sort_unstable();
        assert_eq!(env.enrolled(manual).await, members);
        assert_eq!(env.enrolled(api).await, [cy]);
        let triggers: Vec<(String, String, Uuid)> = sqlx::query_as(
            "SELECT DISTINCT context ->> 'trigger', cause_kind, occurrence_id FROM enrollments \
             ORDER BY 1",
        )
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(
            triggers,
            [
                ("api".to_owned(), "api".to_owned(), occurrence_of("key")),
                ("manual".to_owned(), "api".to_owned(), occurrence)
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn once_at_a_time_lets_a_person_in_again_only_after_the_enrollment_ended(pool: PgPool) {
        let env = Env::new(pool).await;
        let ann = env.person("a").await;
        let flow = env
            .flow(
                "Again",
                settings(json!({"type": "api"}), json!({"mode": "once_at_a_time"})),
            )
            .await;
        let request = || EnrollJobPayload {
            workspace: env.ws,
            flow,
            version: None,
            trigger: TriggerKind::Api,
            work: EnrollWork::People(vec![(ann, Uuid::now_v7())]),
        };
        let trace = TraceContext::root(Cause::api("req"));

        env.run(&request(), &trace).await;
        env.run(&request(), &trace).await;
        assert_eq!(env.count("SELECT count(*) FROM enrollments").await, 1);
        assert_eq!(env.decisions("skipped", Some("already_active")).await, 1);
        env.end_enrollments(flow).await;
        env.run(&request(), &trace).await;

        assert_eq!(env.count("SELECT count(*) FROM enrollments").await, 2);
        assert_eq!(
            env.count("SELECT entries::bigint FROM flow_entries").await,
            2
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn always_waits_out_its_minimum_interval_and_once_never_lets_a_person_back(pool: PgPool) {
        let env = Env::new(pool).await;
        let ann = env.person("a").await;
        let always = env
            .flow(
                "Often",
                settings(
                    json!({"type": "api"}),
                    json!({"mode": "always", "min_interval_seconds": 3600}),
                ),
            )
            .await;
        let once_only = env
            .flow("Once", settings(json!({"type": "api"}), once()))
            .await;
        let request = |flow| EnrollJobPayload {
            workspace: env.ws,
            flow,
            version: None,
            trigger: TriggerKind::Api,
            work: EnrollWork::People(vec![(ann, Uuid::now_v7())]),
        };
        let trace = TraceContext::root(Cause::api("req"));

        for flow in [always, once_only] {
            env.run(&request(flow), &trace).await;
        }
        env.end_enrollments(always).await;
        env.end_enrollments(once_only).await;
        env.clock.advance(Span::minutes(59));
        for flow in [always, once_only] {
            env.run(&request(flow), &trace).await;
        }
        assert_eq!(env.enrolled(always).await.len(), 1);
        assert_eq!(env.decisions("skipped", Some("reentry_interval")).await, 1);
        assert_eq!(env.decisions("skipped", Some("already_entered")).await, 1);
        env.clock.advance(Span::minutes(2));
        for flow in [always, once_only] {
            env.run(&request(flow), &trace).await;
        }

        assert_eq!(env.enrolled(always).await.len(), 2);
        assert_eq!(env.enrolled(once_only).await.len(), 1);
        assert_eq!(env.decisions("skipped", Some("already_entered")).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_paused_flow_skips_every_person_with_a_decision_and_a_count_and_a_whole_audience_once(
        pool: PgPool,
    ) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        let env = Env::new(pool).await;
        let (ann, bob) = (env.person("a").await, env.person("b").await);
        env.people(3, "c").await;
        let flow = env
            .flow("Paused", settings(json!({"type": "api"}), once()))
            .await;
        env.pause(flow).await;
        let trace = TraceContext::root(Cause::api("req"));

        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow,
                version: None,
                trigger: TriggerKind::Api,
                work: EnrollWork::People(vec![(ann, Uuid::now_v7()), (bob, Uuid::now_v7())]),
            },
            &trace,
        )
        .await;
        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow,
                version: None,
                trigger: TriggerKind::Schedule,
                work: EnrollWork::Audience {
                    source: AudienceSource::Everyone,
                    occurrence: None,
                    after: None,
                    entered_by: None,
                },
            },
            &trace,
        )
        .await;

        assert!(env.enrolled(flow).await.is_empty());
        assert_eq!(env.decisions("skipped", Some("flow_paused")).await, 3);
        let output = handle.render();
        assert!(
            output.contains(
                "zirv_flow_flow_enroll_skips_total{trigger=\"api\",reason=\"flow_paused\"} 2"
            ),
            "{output}"
        );
        assert!(
            output.contains(
                "zirv_flow_flow_enroll_skips_total{trigger=\"schedule\",reason=\"flow_paused\"} 1"
            ),
            "{output}"
        );
        // Resumed, the same flow takes people again.
        env.resume(flow).await;
        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow,
                version: None,
                trigger: TriggerKind::Api,
                work: EnrollWork::People(vec![(ann, Uuid::now_v7())]),
            },
            &trace,
        )
        .await;
        assert_eq!(env.enrolled(flow).await, [ann]);
    }

    /// Exit criterion 4 of milestone 5: a segment-entry trigger for 100,000 people enrolls each
    /// one exactly once under `once`, however the work is delivered.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_entry_trigger_for_100000_people_enrolls_each_exactly_once(pool: PgPool) {
        const PEOPLE: i32 = 100_000;
        let started = Instant::now();
        let env = Env::new(pool).await;
        env.define_plan().await;
        env.people(PEOPLE, "free").await;
        let segment = env.segment("Pros", plan_is("pro")).await;
        let flow = env
            .flow(
                "Welcome",
                settings(entered_trigger(segment, "entered"), once()),
            )
            .await;
        // The first computation is the definition's and finds nobody; then everyone upgrades.
        env.refresh(segment, "save", TraceContext::root(Cause::api("job")))
            .await;
        env.set_plan("free", "pro").await;
        let seeded = started.elapsed();

        let (_, trace) = schedule_trace();
        env.refresh(segment, "schedule", trace).await;
        let refreshed = started.elapsed();
        let jobs = env.enroll_jobs().await;
        assert_eq!(jobs, 100, "a thousand pairs to a job");

        // Ten of the jobs are delivered twice, and four workers race for them.
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), workspace_id, kind, payload, trace_id, cause_kind, cause_id \
             FROM jobs WHERE kind = 'flow.enroll' ORDER BY id LIMIT 10",
        )
        .execute(&env.pool)
        .await
        .unwrap();
        assert_eq!(env.drain(4).await, 110);
        let enrolled = started.elapsed();

        let (rows, people, versions): (i64, i64, i64) = sqlx::query_as(
            "SELECT count(*), count(DISTINCT person_id), count(DISTINCT occurrence_id) \
             FROM enrollments WHERE flow_id = $1",
        )
        .bind(flow)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!((rows, people), (100_000, 100_000));
        assert_eq!(
            versions, 100_000,
            "each person enrolled for their own entry event"
        );
        assert_eq!(env.decisions("enrolled", None).await, 100_000);
        assert_eq!(
            env.count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                .await,
            100_000,
            "a start job for each, and no more"
        );

        // The enrollments end, the segment is left and entered again: new events, so new
        // occurrences, and the re-entry rule refuses all of them.
        env.end_enrollments(flow).await;
        env.set_plan("pro", "free").await;
        let (_, trace) = schedule_trace();
        env.refresh(segment, "schedule", trace).await;
        env.set_plan("free", "pro").await;
        let (_, trace) = schedule_trace();
        env.refresh(segment, "schedule", trace).await;
        assert_eq!(env.enroll_jobs().await, 100);
        assert_eq!(env.drain(4).await, 100);
        let finished = started.elapsed();

        assert_eq!(env.enrolled(flow).await.len(), 100_000);
        assert_eq!(
            env.count("SELECT count(*) FROM enrollments").await,
            100_000,
            "nobody entered twice"
        );
        assert_eq!(
            env.count("SELECT max(entries)::bigint FROM flow_entries")
                .await,
            1
        );
        assert_eq!(
            env.decisions("skipped", Some("already_entered")).await,
            100_000
        );
        eprintln!(
            "criterion 4: seeded {seeded:?}, refreshed {refreshed:?}, enrolled {enrolled:?}, \
             re-entry refused {finished:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_meeting_an_erased_person_writes_no_decision_naming_them_and_counts_the_skip(
        pool: PgPool,
    ) {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        let env = Env::new(pool).await;
        let ann = env.person("a").await;
        // Erased after the job was queued: no row, no id to write down.
        let erased = Uuid::now_v7();
        let flow = env
            .flow("By api", settings(json!({"type": "api"}), once()))
            .await;

        env.run(
            &EnrollJobPayload {
                workspace: env.ws,
                flow,
                version: None,
                trigger: TriggerKind::Api,
                work: EnrollWork::People(vec![(ann, Uuid::now_v7()), (erased, Uuid::now_v7())]),
            },
            &TraceContext::root(Cause::api("req")),
        )
        .await;

        assert_eq!(env.enrolled(flow).await, [ann]);
        let naming = env
            .count(&format!(
                "SELECT count(*) FROM decision_records WHERE subject_ids::text LIKE '%{erased}%'"
            ))
            .await;
        assert_eq!(naming, 0, "no decision names the erased person");
        assert_eq!(env.decisions("skipped", Some("no_such_person")).await, 0);
        let output = handle.render();
        assert!(
            output.contains(
                "zirv_flow_flow_enroll_skips_total{trigger=\"api\",reason=\"no_such_person\"} 1"
            ),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_naming_a_merged_away_person_enrolls_the_survivor_once(pool: PgPool) {
        let env = Env::new(pool).await;
        let (survivor, loser, other) = (
            env.person("a").await,
            env.person("b").await,
            env.person("c").await,
        );
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&env.pool)
            .await
            .unwrap();
        let only_loser = env
            .flow("Loser only", settings(json!({"type": "api"}), once()))
            .await;
        let both = env
            .flow("Both", settings(json!({"type": "api"}), once()))
            .await;
        let trace = TraceContext::root(Cause::api("req"));
        let job = |flow, pairs| EnrollJobPayload {
            workspace: env.ws,
            flow,
            version: None,
            trigger: TriggerKind::Api,
            work: EnrollWork::People(pairs),
        };

        env.run(&job(only_loser, vec![(loser, Uuid::now_v7())]), &trace)
            .await;
        env.run(
            &job(
                both,
                vec![
                    (loser, Uuid::now_v7()),
                    (survivor, Uuid::now_v7()),
                    (other, Uuid::now_v7()),
                ],
            ),
            &trace,
        )
        .await;

        assert_eq!(env.enrolled(only_loser).await, [survivor]);
        let mut expected = vec![survivor, other];
        expected.sort_unstable();
        assert_eq!(env.enrolled(both).await, expected, "the survivor once");
        // The re-entry rule decides: the survivor is already in the flow, so a second job is refused.
        env.run(&job(only_loser, vec![(loser, Uuid::now_v7())]), &trace)
            .await;
        assert_eq!(env.enrolled(only_loser).await, [survivor]);
        assert_eq!(env.decisions("skipped", Some("already_active")).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_job_for_a_merged_away_person_enrolls_the_survivor_whether_or_not_the_event_moved(
        pool: PgPool,
    ) {
        let env = Env::new(pool).await;
        env.define_order().await;
        let flow = env
            .flow(
                "Orders",
                settings(
                    json!({"type": "event", "event": "order", "filters": []}),
                    once(),
                ),
            )
            .await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let record = |email: &str| EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), email.into())],
            name: "order".into(),
            properties: json!({"total": 5}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: env.clock.now(),
            message_id: None,
            trace: TraceContext::root(Cause::api("event")),
        };
        zirv_flow_store::apply_event_batch(
            &env.pool,
            env.ws,
            &key,
            &[record("ada@example.com"), record("bea@example.com")],
        )
        .await
        .unwrap();
        assert_eq!(env.enroll_jobs().await, 1);
        let person_of = |email: &'static str| {
            let pool = env.pool.clone();
            async move {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT person_id FROM identities WHERE kind = 'email' AND value = $1",
                )
                .bind(email)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let (ada, bea) = (
            person_of("ada@example.com").await,
            person_of("bea@example.com").await,
        );
        let (ada_survivor, bea_survivor) = (env.person("x").await, env.person("y").await);
        // Both are merged after the job was queued. Ada's event was re-pointed to the survivor
        // by the rewrite job; Bea's was not yet.
        for (loser, survivor) in [(ada, ada_survivor), (bea, bea_survivor)] {
            sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
                .bind(loser)
                .bind(survivor)
                .execute(&env.pool)
                .await
                .unwrap();
        }
        sqlx::query("UPDATE events SET person_id = $2 WHERE person_id = $1")
            .bind(ada)
            .bind(ada_survivor)
            .execute(&env.pool)
            .await
            .unwrap();

        assert_eq!(env.drain(1).await, 1);

        let mut expected = vec![ada_survivor, bea_survivor];
        expected.sort_unstable();
        assert_eq!(env.enrolled(flow).await, expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_redelivered_occurrence_after_a_merge_is_a_replay_under_the_losers_id(pool: PgPool) {
        let env = Env::new(pool).await;
        let (survivor, loser) = (env.person("a").await, env.person("b").await);
        let flow = env
            .flow(
                "By api",
                settings(json!({"type": "api"}), json!({"mode": "once_at_a_time"})),
            )
            .await;
        let occurrence = Uuid::now_v7();
        let trace = TraceContext::root(Cause::api("req"));
        let job = EnrollJobPayload {
            workspace: env.ws,
            flow,
            version: None,
            trigger: TriggerKind::Api,
            work: EnrollWork::People(vec![(loser, occurrence)]),
        };
        env.run(&job, &trace).await;
        assert_eq!(env.enrolled(flow).await, [loser]);

        // The merge exits the loser's enrollment and points the loser at the survivor.
        sqlx::query("UPDATE enrollments SET state = 'exited', ended_at = now() WHERE flow_id = $1")
            .bind(flow)
            .execute(&env.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&env.pool)
            .await
            .unwrap();

        env.run(&job, &trace).await;

        assert_eq!(
            env.enrolled(flow).await,
            [loser],
            "the redelivery enrolls nobody new"
        );
        assert_eq!(env.decisions("enrolled", None).await, 1);
    }
}
