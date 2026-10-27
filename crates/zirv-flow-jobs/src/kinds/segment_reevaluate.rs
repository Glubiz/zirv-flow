//! `segment.reevaluate`: the incremental re-evaluation of a workspace's segments for the people
//! whose data changed (architecture 7.1 and 8.4).
//!
//! The payload is `{workspace_id}`. The job loops until the dirty set is empty or its time
//! budget runs out. Each pass is one transaction of its own: it claims a batch of marks, takes
//! the refresh lock of every segment the batch reaches in topological order, reads the clock
//! once the locks are held and diffs each segment's membership of the batch's people in SQL. The
//! entries and exits, their events and decisions, and the claim itself commit together or not at
//! all, so a failed pass leaves its marks for the next. The job's own transaction only holds the
//! lease. The job never re-enqueues itself, since it still holds its unique key; the scheduler
//! leader's safety net picks up what is left.
//!
//! Nothing here logs an identifier or a value.

use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::decision_actions;
use zirv_flow_core::{TraceContext, WorkspaceId};
use zirv_flow_segments::{compile, parse_and_validate};
use zirv_flow_store::{
    Clock, DirtyBatch, NewDecision, PgPool, REEVALUATE_JOB_KIND, ReevaluateApply, append_decision,
    apply_reevaluation, begin, claim_dirty, commit, find_segment, find_segment_version,
    lock_reevaluation, segment_catalog,
};

use super::segment_refresh::{catalog_of, sql_query};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct SegmentReevaluate;

/// The people one pass claims.
const BATCH: i64 = 500;

/// How long one run works before it ends, below the job lease.
const TIME_BUDGET: Duration = Duration::from_secs(20);

impl JobKind for SegmentReevaluate {
    const KIND: &'static str = REEVALUATE_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let workspace = payload
            .get("workspace_id")
            .and_then(Value::as_str)
            .and_then(|text| text.parse::<WorkspaceId>().ok())
            .ok_or(JobError::new("the payload has no workspace_id"))?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let pool = ctx
            .pool()
            .cloned()
            .ok_or(JobError::new("the worker has no pool for a re-evaluation"))?;
        let run = Run {
            workspace,
            clock: ctx.clock().clone(),
            job: (ctx.job_id(), ctx.attempts()),
            trace: ctx.trace().clone(),
        };

        let started = Instant::now();
        loop {
            let claimed = run.pass(&pool).await?;
            tracing::info!(claimed, "segments re-evaluated");
            if i64::try_from(claimed).is_ok_and(|claimed| claimed < BATCH) {
                return Ok(());
            }
            if started.elapsed() >= TIME_BUDGET {
                return Ok(());
            }
        }
    }
}

/// What every pass of one run shares.
struct Run {
    workspace: WorkspaceId,
    clock: Clock,
    /// The job and attempt the decisions are recorded under.
    job: (Uuid, i32),
    /// The job's trace, which a mark that predates traces falls back to.
    trace: TraceContext,
}

impl Run {
    /// One pass; returns how many people it claimed.
    async fn pass(&self, pool: &PgPool) -> Result<usize, JobError> {
        let mut tx = begin(pool).await?;
        // The slot lock comes before every other lock of the pass: see the store's `slots`.
        zirv_flow_store::hold_trait_slots(&mut tx, self.workspace).await?;
        let claimed = claim_dirty(&mut tx, self.workspace, BATCH).await?;
        if claimed.is_empty() {
            return Ok(0);
        }
        let batch = DirtyBatch::new(&claimed, &self.trace);

        // Lock every segment the batch reaches before reading anything about it, in an order
        // that is the same for every pass, so two passes and a full refresh cannot deadlock.
        let order = lock_reevaluation(&mut tx, self.workspace, batch.classes()).await?;
        if !order.is_empty() {
            // Read once the locks are held, so the time is no earlier than the refresh that
            // held them committed.
            let now = self.clock.now(&mut tx).await?;
            let (catalog, storage) = catalog_of(segment_catalog(&mut tx, self.workspace).await?);
            for segment_id in order {
                // A segment is judged by what it is now that its lock is held.
                let Some(segment) = find_segment(&mut *tx, self.workspace, segment_id)
                    .await?
                    .filter(|segment| segment.archived_at.is_none())
                else {
                    continue;
                };
                // Never computed, or computed from an older version: its pending full refresh
                // covers everyone.
                if segment.computed_version != Some(segment.current_version) {
                    continue;
                }
                let version = find_segment_version(
                    &mut *tx,
                    self.workspace,
                    segment_id,
                    segment.current_version,
                )
                .await?
                .ok_or(JobError::new(
                    "the current version of the segment is missing",
                ))?;
                let compiled = parse_and_validate(&version.definition, &catalog)
                    .and_then(|parsed| compile(&parsed, &catalog, &storage, now));
                let Ok(compiled) = compiled else {
                    // Something the definition reads was archived since the save: the members
                    // stay as they were, and the other segments still get their pass.
                    append_decision(
                        &mut *tx,
                        &NewDecision {
                            graph: None,
                            workspace_id: self.workspace,
                            trace: self.trace.clone(),
                            spec: &decision_actions::SEGMENT_REEVALUATE_FAILED,
                            detail: None,
                            subject_ids: vec![("segment", segment_id)],
                            job: Some(self.job),
                        },
                    )
                    .await?;
                    continue;
                };
                let members = sql_query(compiled.batch_sql());
                apply_reevaluation(
                    &mut tx,
                    &ReevaluateApply {
                        workspace: self.workspace,
                        segment: segment_id,
                        version: version.version,
                        members: &members,
                        now,
                        batch: &batch,
                        job: self.job,
                    },
                )
                .await?;
            }
        }
        commit(tx).await?;
        Ok(claimed.len())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::segment::ChangeClasses;
    use zirv_flow_core::{Cause, HmacPurpose, HmacRoot, MasterKey, Slug, WorkspaceName};
    use zirv_flow_segments::normalize;
    use zirv_flow_store::{
        EventRecord, ManualClock, NewEventSchema, NewJob, NewSegment, NewSegmentVersion,
        NewTraitDefinition, ProfileOutcome, ProfileRecord, Segment, SegmentPatch,
        TraitDefinitionUpdate,
    };

    use super::*;
    use crate::context::IngestSettings;
    use crate::kinds::segment_refresh::SegmentRefresh;

    #[test]
    fn the_kind_name_is_segment_reevaluate() {
        assert_eq!(SegmentReevaluate::KIND, "segment.reevaluate");
        assert!(
            crate::Registry::built_in()
                .get("segment.reevaluate")
                .is_some()
        );
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn define_trait(pool: &PgPool, ws: WorkspaceId, key: &str) {
        zirv_flow_store::create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.into(),
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

    /// Applies one profile: the person gets `plan`, and the mark carries `trace`.
    async fn identify(
        pool: &PgPool,
        ws: WorkspaceId,
        email: &str,
        plan: &str,
        trace: &TraceContext,
    ) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: json!({"plan": plan}).as_object().cloned().unwrap(),
            observed_at: OffsetDateTime::now_utc(),
            trace: trace.clone(),
        };
        match &zirv_flow_store::apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()[0]
        {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn content(pool: &PgPool, ws: WorkspaceId, definition: Value) -> NewSegmentVersion {
        let mut connection = pool.acquire().await.unwrap();
        let (catalog, storage) = catalog_of(segment_catalog(&mut connection, ws).await.unwrap());
        let parsed = parse_and_validate(&definition, &catalog).unwrap();
        let compiled = compile(&parsed, &catalog, &storage, OffsetDateTime::now_utc()).unwrap();
        NewSegmentVersion {
            definition,
            normalized: normalize(parsed).to_value(),
            deps: compiled.deps.to_value(),
            time_sensitive: compiled.time_sensitive,
        }
    }

    async fn create(pool: &PgPool, ws: WorkspaceId, name: &str, definition: Value) -> Segment {
        create_with_id(pool, ws, Uuid::now_v7(), name, definition).await
    }

    async fn create_with_id(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        name: &str,
        definition: Value,
    ) -> Segment {
        let content = content(pool, ws, definition).await;
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut tx, ws).await.unwrap();
        let (segment, _) = zirv_flow_store::create_segment(
            &mut tx,
            ws,
            &NewSegment {
                id,
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
        segment
    }

    async fn resave(pool: &PgPool, ws: WorkspaceId, id: Uuid, definition: Value) {
        let content = content(pool, ws, definition).await;
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut tx, ws).await.unwrap();
        zirv_flow_store::update_segment(
            &mut tx,
            ws,
            id,
            &SegmentPatch {
                version: Some(content),
                ..SegmentPatch::default()
            },
            None,
            &TraceContext::root(Cause::api("resave")),
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
    }

    /// Runs a job of `kind` as a worker would: enqueued, claimed and run in a transaction.
    async fn run_kind<K: JobKind>(
        kind: &K,
        pool: &PgPool,
        ws: WorkspaceId,
        payload: Value,
        clock: &Clock,
    ) {
        sqlx::query("DELETE FROM jobs WHERE kind = $1")
            .bind(K::KIND)
            .execute(pool)
            .await
            .unwrap();
        let mut job = NewJob::new(K::KIND, TraceContext::root(Cause::schedule(Uuid::new_v4())));
        job.workspace_id = Some(ws);
        job.payload = payload;
        zirv_flow_store::enqueue(pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[K::KIND],
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_clock(clock.clone())
            .with_ingest(IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 500,
                shards: 1,
            });
        kind.run(&mut ctx, claimed.payload.clone()).await.unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
    }

    async fn full_refresh(pool: &PgPool, ws: WorkspaceId, segment: Uuid, clock: &Clock) {
        let payload = json!({"workspace_id": ws, "segment_id": segment, "trigger": "manual"});
        run_kind(&SegmentRefresh, pool, ws, payload, clock).await;
    }

    async fn reevaluate(pool: &PgPool, ws: WorkspaceId, clock: &Clock) {
        run_kind(
            &SegmentReevaluate,
            pool,
            ws,
            json!({"workspace_id": ws}),
            clock,
        )
        .await;
    }

    async fn open(pool: &PgPool, segment: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar(
            "SELECT person_id FROM segment_members WHERE segment_id = $1 AND exited_at IS NULL \
             ORDER BY person_id",
        )
        .bind(segment)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn count_of(pool: &PgPool, segment: Uuid) -> i32 {
        sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(segment)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn dirty(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM person_changes")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn clock() -> Clock {
        Clock::Manual(ManualClock::new(OffsetDateTime::now_utc()))
    }

    fn plan_is(plan: &str) -> Value {
        json!({"op": "eq", "trait": "plan", "value": plan, "v": 1})
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_change_enters_and_exits_a_person_on_the_trace_that_marked_it(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;
        assert_eq!(dirty(&pool).await, 0);

        let enter = TraceContext::root(Cause::api("enter"));
        let ada = identify(&pool, ws, "ada@example.com", "pro", &enter).await;
        reevaluate(&pool, ws, &clock).await;

        assert_eq!(open(&pool, seg.id).await, [ada]);
        assert_eq!(count_of(&pool, seg.id).await, 1);
        assert_eq!(dirty(&pool).await, 0);
        let entered: (Uuid, String) = sqlx::query_as(
            "SELECT trace_id, properties ->> 'cause' FROM events \
             WHERE name = 'segment.entered' AND person_id = $1",
        )
        .bind(ada)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entered, (enter.trace_id.as_uuid(), "data_change".into()));

        let leave = TraceContext::root(Cause::api("leave"));
        identify(&pool, ws, "ada@example.com", "free", &leave).await;
        reevaluate(&pool, ws, &clock).await;

        assert!(open(&pool, seg.id).await.is_empty());
        assert_eq!(count_of(&pool, seg.id).await, 0);
        let exited: (Uuid, String, String) = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id FROM decision_records \
             WHERE action = 'segment.exited' AND subject_ids ->> 'person' = $1",
        )
        .bind(ada.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            exited,
            (leave.trace_id.as_uuid(), "api".into(), "leave".into())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_of_segments_sees_the_membership_the_same_pass_wrote(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let inner = create(&pool, ws, "Pro", plan_is("pro")).await;
        let outer = create(&pool, ws, "In Pro", json!({"segment": inner.id, "v": 1})).await;
        full_refresh(&pool, ws, inner.id, &clock).await;
        full_refresh(&pool, ws, outer.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;

        let ada = identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("t")),
        )
        .await;
        reevaluate(&pool, ws, &clock).await;

        assert_eq!(open(&pool, inner.id).await, [ada]);
        assert_eq!(open(&pool, outer.id).await, [ada]);
        assert_eq!(count_of(&pool, outer.id).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn segments_outside_the_changed_classes_are_not_evaluated(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let by_trait = create(&pool, ws, "Pro", plan_is("pro")).await;
        let by_list = create(&pool, ws, "Everyone", json!({"all": [], "v": 1})).await;
        full_refresh(&pool, ws, by_trait.id, &clock).await;
        full_refresh(&pool, ws, by_list.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;
        // A consent-only change reaches the segments whose classes include consent or merge.
        // Every segment carries the merge class; a trait-only definition does not carry consent.
        let classes: Vec<(Uuid, i32)> = sqlx::query_as(
            "SELECT s.id, (v.deps ->> 'classes')::int FROM segments s \
             JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(classes.iter().all(|(_, bits)| bits & 8 == 0));

        let ada = identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("t")),
        )
        .await;
        sqlx::query("UPDATE person_changes SET classes = 8 WHERE person_id = $1")
            .bind(ada)
            .execute(&pool)
            .await
            .unwrap();
        reevaluate(&pool, ws, &clock).await;

        // The mark was consumed, and no segment looked at it.
        assert_eq!(dirty(&pool).await, 0);
        assert!(open(&pool, by_trait.id).await.is_empty());
        assert!(open(&pool, by_list.id).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_pending_its_full_refresh_is_skipped(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let never = create(&pool, ws, "Never", plan_is("pro")).await;
        let stale = create(&pool, ws, "Stale", plan_is("pro")).await;
        full_refresh(&pool, ws, stale.id, &clock).await;
        resave(
            &pool,
            ws,
            stale.id,
            json!({"op": "eq", "trait": "plan", "value": "free", "v": 1}),
        )
        .await;
        identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("t")),
        )
        .await;

        reevaluate(&pool, ws, &clock).await;

        assert!(open(&pool, never.id).await.is_empty());
        assert!(open(&pool, stale.id).await.is_empty());
        assert_eq!(dirty(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_definition_that_no_longer_compiles_is_recorded_and_the_others_still_commit(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        define_trait(&pool, ws, "tier").await;
        let clock = clock();
        let broken = create(
            &pool,
            ws,
            "Tier",
            json!({"op": "eq", "trait": "tier", "value": "gold", "v": 1}),
        )
        .await;
        let fine = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, broken.id, &clock).await;
        full_refresh(&pool, ws, fine.id, &clock).await;
        zirv_flow_store::update_trait_definition(
            &pool,
            ws,
            "tier",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..TraitDefinitionUpdate::default()
            },
        )
        .await
        .unwrap();
        let ada = identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("t")),
        )
        .await;
        reevaluate(&pool, ws, &clock).await;

        assert_eq!(open(&pool, fine.id).await, [ada]);
        let failed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'segment.reevaluate' \
             AND outcome = 'failed' AND reason = 'definition_invalid' \
             AND subject_ids ->> 'segment' = $1",
        )
        .bind(broken.id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failed, 1);
        assert_eq!(dirty(&pool).await, 0);
        zirv_flow_store::decisions_testing::seed_personal_values(&pool, ws).await;
        zirv_flow_store::decisions_testing::assert_decisions_hold_no_personal_data(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_pass_restores_its_marks_and_a_mark_made_during_one_survives_it(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;
        let trace = TraceContext::root(Cause::api("t"));
        identify(&pool, ws, "ada@example.com", "pro", &trace).await;
        // A pass whose rows cannot be written fails after it claimed.
        sqlx::query(
            "ALTER TABLE segment_members ADD CONSTRAINT never CHECK (person_id IS NULL) NOT VALID",
        )
        .execute(&pool)
        .await
        .unwrap();

        let run = Run {
            workspace: ws,
            clock: clock.clone(),
            job: (Uuid::new_v4(), 1),
            trace: trace.clone(),
        };
        assert!(run.pass(&pool).await.is_err());

        assert_eq!(dirty(&pool).await, 1);
        assert!(open(&pool, seg.id).await.is_empty());

        // With the fault gone, a mark made after a claim stays after the claim commits.
        sqlx::query("ALTER TABLE segment_members DROP CONSTRAINT never")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(run.pass(&pool).await.unwrap(), 1);
        identify(&pool, ws, "bob@example.com", "pro", &trace).await;
        assert_eq!(dirty(&pool).await, 1);
        assert_eq!(run.pass(&pool).await.unwrap(), 1);
        assert_eq!(open(&pool, seg.id).await.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_that_raced_a_full_refresh_is_closed_by_the_next_pass(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        let trace = TraceContext::root(Cause::api("t"));
        let ada = identify(&pool, ws, "ada@example.com", "pro", &trace).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;
        assert_eq!(open(&pool, seg.id).await, [ada]);

        // A second record joins Ada with a second person through a shared identifier.
        let bob = identify(&pool, ws, "bob@example.com", "pro", &trace).await;
        reevaluate(&pool, ws, &clock).await;
        assert_eq!(count_of(&pool, seg.id).await, 2);
        let joined = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "joined".into(),
            identifiers: vec![
                ("email".into(), "ada@example.com".into()),
                ("email".into(), "bob@example.com".into()),
            ],
            traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            observed_at: OffsetDateTime::now_utc(),
            trace: trace.clone(),
        };
        zirv_flow_store::apply_profile_batch(&pool, ws, &[joined])
            .await
            .unwrap();
        let merged: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE workspace_id = $1 AND merged_into IS NOT NULL",
        )
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(merged.len(), 1);
        let loser = merged[0];
        assert!([ada, bob].contains(&loser));

        // The refresh that was running beside the merge opened a row for the loser after the
        // merge closed its rows. Count is as that refresh left it.
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                 entered_at, entered_trace_id, entered_cause) \
             VALUES ($1, $2, $3, 1, now(), gen_random_uuid(), 'time')",
        )
        .bind(ws.as_uuid())
        .bind(seg.id)
        .bind(loser)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE segments SET member_count = member_count + 1 WHERE id = $1")
            .bind(seg.id)
            .execute(&pool)
            .await
            .unwrap();
        // The merge marked the loser before; the next pass consumes that mark.
        reevaluate(&pool, ws, &clock).await;

        let open_rows = open(&pool, seg.id).await;
        assert!(!open_rows.contains(&loser));
        assert_eq!(
            i64::from(count_of(&pool, seg.id).await),
            i64::try_from(open_rows.len()).unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_runs_at_once_take_disjoint_people_and_leave_exact_membership(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        let trace = TraceContext::root(Cause::api("t"));
        for n in 0..12 {
            identify(&pool, ws, &format!("p{n}@example.com"), "pro", &trace).await;
        }
        let run = Arc::new(Run {
            workspace: ws,
            clock: clock.clone(),
            job: (Uuid::new_v4(), 1),
            trace,
        });

        let (one, two) = tokio::join!(
            {
                let (run, pool) = (Arc::clone(&run), pool.clone());
                async move { run.pass(&pool).await }
            },
            {
                let (run, pool) = (Arc::clone(&run), pool.clone());
                async move { run.pass(&pool).await }
            }
        );

        // Each pass took what the other did not, and together they took everyone once.
        assert_eq!(one.unwrap() + two.unwrap(), 12);
        assert_eq!(open(&pool, seg.id).await.len(), 12);
        assert_eq!(count_of(&pool, seg.id).await, 12);
        let entered: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE name = 'segment.entered'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entered, 12);
    }

    async fn define_order_event(pool: &PgPool, ws: WorkspaceId) {
        zirv_flow_store::create_event_schema(
            pool,
            ws,
            &NewEventSchema {
                name: "order".into(),
                mode: EventMode::Warn,
                description: String::new(),
                properties: serde_json::from_str(r#"[{"name": "total", "type": "number"}]"#)
                    .unwrap(),
            },
        )
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        zirv_flow_store::publish_schema_draft(&mut connection, ws, "order", None)
            .await
            .unwrap();
    }

    fn order_segment() -> Value {
        json!({"event": "order", "window": {"last": "P7D"}, "v": 1})
    }

    /// The done-when of the incremental path: track events sent through the real ingest path
    /// (the inbox append that the API makes), with a worker pool and the leader running at
    /// their default configuration, enter their senders into an event segment within a minute.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn twenty_people_sending_qualifying_events_enter_the_segment_within_a_minute_at_p95(
        pool: PgPool,
    ) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        use zirv_flow_store::{IngestItem, IngestPayload, append_inbox};

        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_order_event(&pool, ws).await;
        let seg = create(&pool, ws, "Buyers", order_segment()).await;
        full_refresh(&pool, ws, seg.id, &Clock::Database).await;
        let mut config = crate::WorkerConfig::new(4);
        config.redaction_key = Some(Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction),
        ));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = crate::Worker::new(pool.clone(), crate::Registry::built_in(), config);
        let leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        let tasks = [
            tokio::spawn(worker.run(stopped.clone())),
            tokio::spawn(leader.run(stopped)),
        ];

        let mut pending: Vec<(String, std::time::Instant)> = Vec::new();
        for n in 0..20 {
            let item = IngestItem {
                identifiers: vec![("email".into(), format!("p{n}@example.com"))],
                timestamp: None,
                message_id: Some(format!("m-{n}")),
                payload: IngestPayload::Track {
                    event: "order".into(),
                    properties: json!({"total": 10}).as_object().cloned().unwrap(),
                },
            };
            append_inbox(
                &pool,
                ws,
                Uuid::now_v7(),
                &TraceContext::root(Cause::api(format!("req-{n}"))),
                &[item],
            )
            .await
            .unwrap();
            pending.push((format!("p{n}@example.com"), std::time::Instant::now()));
        }

        // Each wait runs from the person's own ingest call to the first poll that sees them in.
        let mut waits: Vec<Duration> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !pending.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "{} people never entered",
                pending.len()
            );
            let entered: Vec<String> = sqlx::query_scalar(
                "SELECT i.value FROM segment_members m \
                 JOIN identities i ON i.person_id = m.person_id AND i.kind = 'email' \
                 WHERE m.segment_id = $1 AND m.exited_at IS NULL",
            )
            .bind(seg.id)
            .fetch_all(&pool)
            .await
            .unwrap();
            let seen = std::time::Instant::now();
            pending.retain(|(email, at)| {
                let done = entered.contains(email);
                if done {
                    waits.push(seen - *at);
                }
                !done
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.send(true).unwrap();
        for task in tasks {
            task.await.unwrap();
        }

        waits.sort();
        let p95 = waits[(waits.len() * 95).div_ceil(100) - 1];
        eprintln!("observed p95 from ingest to segment.entered: {p95:?} over {waits:?}");
        assert_eq!(waits.len(), 20);
        assert!(p95 <= Duration::from_secs(60), "{p95:?}");
        // Every entry is event-driven.
        let causes: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT cause_kind, reason FROM decision_records WHERE action = 'segment.entered'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(causes.len(), 20);
        assert!(
            causes
                .iter()
                .all(|(kind, reason)| kind == "event" && reason.as_deref() == Some("data_change"))
        );
    }

    /// An event-driven entry carries the trace of the event that caused it, with an `event`
    /// cause on its decision and `data_change` on the segment event.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_driven_entry_carries_the_events_trace_and_cause(pool: PgPool) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        use zirv_flow_store::EventRecord;

        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_order_event(&pool, ws).await;
        let seg = create(&pool, ws, "Buyers", order_segment()).await;
        full_refresh(&pool, ws, seg.id, &Clock::Database).await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let sent = TraceContext::root(Cause::api("order-request"));
        let record = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: "order".into(),
            properties: json!({"total": 10}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: None,
            trace: sent.clone(),
        };
        zirv_flow_store::apply_event_batch(&pool, ws, &key, &[record])
            .await
            .unwrap();
        let event_id: Uuid = sqlx::query_scalar("SELECT id FROM events WHERE name = 'order'")
            .fetch_one(&pool)
            .await
            .unwrap();

        reevaluate(&pool, ws, &Clock::Database).await;

        let decision: (Uuid, String, String, Option<String>) = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, reason FROM decision_records \
             WHERE action = 'segment.entered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            decision,
            (
                sent.trace_id.as_uuid(),
                "event".to_owned(),
                event_id.to_string(),
                Some("data_change".to_owned())
            )
        );
        let entered: (Uuid, String) = sqlx::query_as(
            "SELECT trace_id, properties ->> 'cause' FROM events WHERE name = 'segment.entered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entered, (sent.trace_id.as_uuid(), "data_change".to_owned()));
    }

    /// A late event reaches the time-window segment of its person through the events class.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_late_event_recomputes_its_persons_time_window_segment(pool: PgPool) {
        use zirv_flow_core::{HmacPurpose, HmacRoot, MasterKey};
        use zirv_flow_store::EventRecord;

        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_order_event(&pool, ws).await;
        let seg = create(&pool, ws, "Recent", order_segment()).await;
        full_refresh(&pool, ws, seg.id, &Clock::Database).await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let now = OffsetDateTime::now_utc();

        // Three days old: late, and still inside the window.
        let record = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: "order".into(),
            properties: json!({"total": 10}).as_object().cloned().unwrap(),
            timestamp: Some(now - time::Duration::days(3)),
            received_at: now,
            message_id: None,
            trace: TraceContext::root(Cause::api("late")),
        };
        zirv_flow_store::apply_event_batch(&pool, ws, &key, &[record])
            .await
            .unwrap();
        let late_from: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT late_from FROM person_changes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(late_from.is_some(), "the event should have been late");

        reevaluate(&pool, ws, &Clock::Database).await;

        assert_eq!(open(&pool, seg.id).await.len(), 1);
        assert_eq!(dirty(&pool).await, 0);
    }

    /// A segment and the segment of its members, the referenced one with the larger id, so the
    /// order a pass locks in (the referenced first) is the opposite of id order.
    async fn nested_segments(pool: &PgPool, ws: WorkspaceId, clock: &Clock) -> (Segment, Segment) {
        // The outer row is written first, so a statement that updates both reaches it first,
        // against the order a pass locks in (the referenced segment first).
        let outer = create_with_id(pool, ws, Uuid::from_u128(1), "In Pro", plan_is("free")).await;
        let inner = create_with_id(
            pool,
            ws,
            Uuid::from_u128(u128::MAX - 1),
            "Pro",
            plan_is("pro"),
        )
        .await;
        resave(pool, ws, outer.id, json!({"segment": inner.id, "v": 1})).await;
        full_refresh(pool, ws, inner.id, clock).await;
        full_refresh(pool, ws, outer.id, clock).await;
        reevaluate(pool, ws, clock).await;
        (inner, outer)
    }

    /// How often a writer racing a pass failed, over `ROUNDS` rounds. Each round gives a pass
    /// a crowd to enter into both segments while `writer` changes two people who are open
    /// members of both.
    const ROUNDS: usize = 30;
    /// The leading rounds that stage the interleaving instead of leaving it to chance.
    const STAGED_ROUNDS: usize = 5;

    async fn race_a_pass_against<W, F>(pool: PgPool, writer: W) -> usize
    where
        W: Fn(PgPool, WorkspaceId, Uuid, Uuid, usize) -> F,
        F: Send + 'static,
        F: std::future::Future<Output = bool>,
    {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let (inner, outer) = nested_segments(&pool, ws, &clock).await;
        let trace = TraceContext::root(Cause::api("t"));
        let run = Run {
            workspace: ws,
            clock: clock.clone(),
            job: (Uuid::new_v4(), 1),
            trace: trace.clone(),
        };
        let mut failures = 0;
        for round in 0..ROUNDS {
            let one = identify(&pool, ws, &format!("a{round}@example.com"), "pro", &trace).await;
            let two = identify(&pool, ws, &format!("b{round}@example.com"), "pro", &trace).await;
            reevaluate(&pool, ws, &clock).await;
            for crowd in 0..40 {
                let email = format!("c{round}-{crowd}@example.com");
                identify(&pool, ws, &email, "pro", &trace).await;
            }
            if round < STAGED_ROUNDS {
                // A pass that has written the inner segment and not yet the outer one, with
                // the writer let loose in between: the interleaving a deadlock needs.
                let mut pass = zirv_flow_store::begin(&pool).await.unwrap();
                let mut failed = false;
                let order =
                    zirv_flow_store::lock_reevaluation(&mut pass, ws, ChangeClasses::TRAITS)
                        .await
                        .unwrap();
                assert_eq!(order, [inner.id, outer.id]);
                let held =
                    sqlx::query("UPDATE segments SET member_count = member_count WHERE id = $1")
                        .bind(inner.id)
                        .execute(&mut *pass)
                        .await;
                failed |= held.is_err();
                let racer = tokio::spawn(writer(pool.clone(), ws, one, two, round));
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let next =
                    sqlx::query("UPDATE segments SET member_count = member_count WHERE id = $1")
                        .bind(outer.id)
                        .execute(&mut *pass)
                        .await;
                failed |= next.is_err();
                if failed {
                    drop(pass);
                } else {
                    zirv_flow_store::commit(pass).await.unwrap();
                }
                failures += usize::from(failed) + usize::from(!racer.await.unwrap());
            } else {
                let (pass, wrote) =
                    tokio::join!(run.pass(&pool), writer(pool.clone(), ws, one, two, round));
                failures += usize::from(pass.is_err()) + usize::from(!wrote);
            }
            // Whatever failed, a quiet pass settles the rest.
            run.pass(&pool).await.unwrap();
            while run.pass(&pool).await.unwrap() > 0 {}
        }
        println!("failed rounds: {failures} of {ROUNDS}");
        for segment in [inner.id, outer.id] {
            let rows = open(&pool, segment).await;
            assert_eq!(
                i64::from(count_of(&pool, segment).await),
                i64::try_from(rows.len()).unwrap()
            );
            let gone: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM segment_members m JOIN people p ON p.id = m.person_id \
                 WHERE m.segment_id = $1 AND m.exited_at IS NULL AND p.merged_into IS NOT NULL",
            )
            .bind(segment)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(gone, 0, "a merged person stays an open member");
        }
        let (inner_rows, outer_rows) = (open(&pool, inner.id).await, open(&pool, outer.id).await);
        assert_eq!(
            inner_rows, outer_rows,
            "the outer segment follows the inner"
        );
        failures
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_racing_a_pass_over_nested_segments_never_deadlocks(pool: PgPool) {
        let failures = race_a_pass_against(pool, |pool, ws, one, two, round| async move {
            let joined = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: format!("joined-{round}"),
                identifiers: vec![
                    ("email".into(), format!("a{round}@example.com")),
                    ("email".into(), format!("b{round}@example.com")),
                ],
                traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
                observed_at: OffsetDateTime::now_utc(),
                trace: TraceContext::root(Cause::api("join")),
            };
            let _ = (one, two);
            zirv_flow_store::apply_profile_batch(&pool, ws, &[joined])
                .await
                .is_ok()
        })
        .await;
        assert_eq!(failures, 0, "a transaction failed, a deadlock (40P01)");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_racing_a_pass_over_nested_segments_never_deadlocks(pool: PgPool) {
        let failures = race_a_pass_against(pool, |pool, ws, one, _two, _round| async move {
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let deleted = zirv_flow_store::delete_person(
                &mut tx,
                ws,
                one,
                zirv_flow_store::DeleteReason::Requested,
                &TraceContext::root(Cause::api("erase")),
            )
            .await;
            match deleted {
                Ok(_) => zirv_flow_store::commit(tx).await.is_ok(),
                Err(_) => false,
            }
        })
        .await;
        assert_eq!(failures, 0, "a transaction failed, a deadlock (40P01)");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pass_that_writes_segment_events_does_not_mark_anyone_again(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        let ada = identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("t")),
        )
        .await;

        reevaluate(&pool, ws, &clock).await;

        let entered: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE name = 'segment.entered'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entered, 1);
        assert_eq!(open(&pool, seg.id).await, [ada]);
        let marks: i64 =
            sqlx::query_scalar("SELECT count(*) FROM person_changes WHERE person_id = $1")
                .bind(ada)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(marks, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_client_event_with_a_reserved_segment_name_is_refused_and_marks_no_one(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let record = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: "segment.entered".into(),
            properties: serde_json::Map::new(),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: None,
            trace: TraceContext::root(Cause::api("forged")),
        };

        let outcomes = zirv_flow_store::apply_event_batch(&pool, ws, &key, &[record])
            .await
            .unwrap();

        assert!(matches!(
            outcomes[0],
            zirv_flow_store::EventOutcome::Rejected {
                reason: "reserved_event_name",
                ..
            }
        ));
        assert_eq!(dirty(&pool).await, 0);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0);
    }

    /// The slot lock comes before the segment locks of a pass: while a retire holds the slot lock,
    /// a pass that has people to evaluate waits for it holding no segment lock.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_pass_waiting_for_the_slot_lock_holds_no_segment_lock(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let clock = clock();
        let seg = create(&pool, ws, "Pro", plan_is("pro")).await;
        full_refresh(&pool, ws, seg.id, &clock).await;
        reevaluate(&pool, ws, &clock).await;
        // A change marks a person, so the next pass has a segment to lock.
        identify(
            &pool,
            ws,
            "ada@example.com",
            "pro",
            &TraceContext::root(Cause::api("enter")),
        )
        .await;
        assert_eq!(dirty(&pool).await, 1);

        let retire = crate::kinds::slot_order::hold_slot_lock_exclusively(&pool, ws).await;
        let job = {
            let pool = pool.clone();
            tokio::spawn(async move { reevaluate(&pool, ws, &clock).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;

        assert!(
            crate::kinds::slot_order::segment_lock_is_free(&pool, seg.id).await,
            "the pass took a segment lock before the slot lock"
        );
        zirv_flow_store::commit(retire).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(20), job)
            .await
            .expect("the pass finishes once the slot lock is free")
            .unwrap();
        assert_eq!(dirty(&pool).await, 0);
    }
}
