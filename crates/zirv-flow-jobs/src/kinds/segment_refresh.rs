//! `segment.refresh`: the full refresh of one segment's members (architecture 8.3 and 8.4).
//!
//! The payload is `{workspace_id, segment_id, trigger}`. A refresh runs in one transaction,
//! behind the segment's refresh lock: it reads the current version, compiles it at one `now`
//! from the worker's clock and has the store diff the new set against the open rows in SQL. The
//! entries and exits, their events and decisions, the segment's `computed_*` columns and the
//! refreshes of the segments that reference this one all commit together.

use serde_json::Value;
use zirv_flow_segments::{Catalog, Param, TraitStorage, compile, parse_and_validate};
use zirv_flow_store::{
    CatalogSnapshot, REFRESH_JOB_KIND, RefreshApply, RefreshTrigger, SqlParam, SqlQuery,
    apply_refresh, find_segment, find_segment_version, lock_segment_refresh, membership_cause,
    membership_trace, segment_catalog,
};

use super::workspace_and_id;
use crate::{JobContext, JobError, JobKind};

pub(crate) struct SegmentRefresh;

const ACTION: &str = "segment.refresh";

impl JobKind for SegmentRefresh {
    const KIND: &'static str = REFRESH_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, segment_id) = workspace_and_id(
            &payload,
            "segment_id",
            "the payload needs a workspace_id and a segment_id",
        )?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let trigger = payload
            .get("trigger")
            .and_then(Value::as_str)
            .and_then(RefreshTrigger::parse)
            .ok_or_else(|| {
                JobError::new("the trigger is not save, schedule, manual or dependent")
            })?;

        lock_segment_refresh(ctx.conn(), segment_id).await?;
        let segment = find_segment(&mut *ctx.conn(), workspace, segment_id).await?;
        let Some(segment) = segment.filter(|segment| segment.archived_at.is_none()) else {
            return ctx
                .record_decision(
                    ACTION,
                    "skipped",
                    Some("archived_or_gone"),
                    vec![("segment", segment_id)],
                )
                .await;
        };
        let version = find_segment_version(
            &mut *ctx.conn(),
            workspace,
            segment_id,
            segment.current_version,
        )
        .await?
        .ok_or_else(|| JobError::new("the current version of the segment is missing"))?;

        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let catalog = catalog_of(segment_catalog(ctx.conn(), workspace).await?);
        let compiled = parse_and_validate(&version.definition, &catalog)
            .and_then(|parsed| compile(&parsed, &catalog, &TraitStorage::default(), now));
        let compiled = match compiled {
            Ok(compiled) => compiled,
            // Something the definition reads was archived since the save: the members stay as
            // they were and a retry would fail the same way.
            Err(_) => {
                return ctx
                    .record_decision(
                        ACTION,
                        "failed",
                        Some("definition_invalid"),
                        vec![("segment", segment_id)],
                    )
                    .await;
            }
        };

        let cause = membership_cause(segment.computed_version, version.version, trigger);
        let job_trace = ctx.trace().clone();
        let trace = membership_trace(cause, &job_trace, segment_id, version.trace_id);
        let members = sql_query(compiled.members_sql());
        let job = (ctx.job_id(), ctx.attempts());
        let outcome = apply_refresh(
            ctx.conn(),
            &RefreshApply {
                workspace,
                segment: segment_id,
                version: version.version,
                members: &members,
                now,
                cause,
                trace: &trace,
                job,
                job_trace: &job_trace,
            },
        )
        .await?;
        tracing::info!(
            entered = outcome.entered,
            exited = outcome.exited,
            cause = cause.as_str(),
            "segment refreshed"
        );
        Ok(())
    }
}

/// The compiler's catalog from the store's snapshot.
fn catalog_of(snapshot: CatalogSnapshot) -> Catalog {
    Catalog {
        traits: snapshot.traits,
        events: snapshot.events,
        lists: snapshot.lists,
        segments: snapshot.segments,
        topics: snapshot.topics,
    }
}

fn param(param: Param) -> SqlParam {
    match param {
        Param::Text(value) => SqlParam::Text(value),
        Param::TextArray(values) => SqlParam::TextArray(values),
        Param::Float(value) => SqlParam::Float(value),
        Param::FloatArray(values) => SqlParam::FloatArray(values),
        Param::Int(value) => SqlParam::Int(value),
        Param::Timestamp(value) => SqlParam::Timestamp(value),
        Param::Uuid(value) => SqlParam::Uuid(value),
        Param::UuidArray(values) => SqlParam::UuidArray(values),
        Param::Bool(value) => SqlParam::Bool(value),
        Param::Jsonb(value) => SqlParam::Jsonb(value),
    }
}

fn sql_query(query: zirv_flow_segments::Query) -> SqlQuery {
    SqlQuery {
        sql: query.sql,
        params: query.params.into_iter().map(param).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, UserId, WorkspaceId,
        WorkspaceName,
    };
    use zirv_flow_segments::normalize;
    use zirv_flow_store::{
        Clock, EventRecord, ManualClock, NewEventSchema, NewJob, NewSegment, NewSegmentVersion,
        NewTraitDefinition, ProfileRecord, Segment, SegmentPatch, TraitDefinitionUpdate,
    };

    use super::*;

    #[test]
    fn the_kind_name_is_segment_refresh() {
        assert_eq!(SegmentRefresh::KIND, "segment.refresh");
        assert!(crate::Registry::built_in().get("segment.refresh").is_some());
    }

    #[test]
    fn every_compiler_value_becomes_the_same_store_value() {
        let id = Uuid::nil();
        let query = sql_query(zirv_flow_segments::Query {
            sql: "SELECT 1".into(),
            params: vec![
                Param::Text("a".into()),
                Param::Timestamp(OffsetDateTime::UNIX_EPOCH),
                Param::Uuid(id),
                Param::Bool(true),
            ],
        });

        assert_eq!(query.sql, "SELECT 1");
        assert_eq!(
            query.params,
            [
                SqlParam::Text("a".into()),
                SqlParam::Timestamp(OffsetDateTime::UNIX_EPOCH),
                SqlParam::Uuid(id),
                SqlParam::Bool(true),
            ]
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

    async fn person(pool: &PgPool, ws: WorkspaceId, email: &str, plan: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: json!({"plan": plan}).as_object().cloned().unwrap(),
            observed_at: OffsetDateTime::now_utc(),
            trace: TraceContext::root(Cause::api("t")),
        };
        match &zirv_flow_store::apply_profile_batch(pool, ws, &[record])
            .await
            .unwrap()[0]
        {
            zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
            | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            zirv_flow_store::ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    /// What the API does when it saves a definition.
    async fn checked(pool: &PgPool, ws: WorkspaceId, definition: Value) -> NewSegmentVersion {
        let mut connection = pool.acquire().await.unwrap();
        let catalog = catalog_of(segment_catalog(&mut connection, ws).await.unwrap());
        let parsed = parse_and_validate(&definition, &catalog).unwrap();
        let compiled = compile(
            &parsed,
            &catalog,
            &TraitStorage::default(),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        NewSegmentVersion {
            definition,
            normalized: normalize(parsed).to_value(),
            deps: compiled.deps.to_value(),
            time_sensitive: compiled.time_sensitive,
        }
    }

    async fn create(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        definition: Value,
        trace: &TraceContext,
    ) -> Segment {
        let content = checked(pool, ws, definition).await;
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut tx, ws).await.unwrap();
        let (segment, _) = zirv_flow_store::create_segment(
            &mut tx,
            ws,
            &NewSegment {
                id: Uuid::now_v7(),
                name: name.into(),
                description: String::new(),
                cadence_minutes: 60,
            },
            &content,
            None,
            trace,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        segment
    }

    async fn resave(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
        definition: Value,
        trace: &TraceContext,
    ) -> Segment {
        let content = checked(pool, ws, definition).await;
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut tx, ws).await.unwrap();
        let (segment, _) = zirv_flow_store::update_segment(
            &mut tx,
            ws,
            id,
            &SegmentPatch {
                version: Some(content),
                ..SegmentPatch::default()
            },
            None,
            trace,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        segment
    }

    /// Enqueues a refresh with its own unique key and runs it as a worker would.
    async fn run(
        pool: &PgPool,
        ws: WorkspaceId,
        segment: Uuid,
        trigger: &str,
        trace: TraceContext,
        clock: &Clock,
    ) {
        let mut job = NewJob::new(SegmentRefresh::KIND, trace);
        job.workspace_id = Some(ws);
        job.payload = json!({"workspace_id": ws, "segment_id": segment, "trigger": trigger});
        job.unique_key = Some(format!("test:{}", Uuid::new_v4()));
        // The save that created the segment queued a refresh of its own; this one is the test.
        sqlx::query("DELETE FROM jobs").execute(pool).await.unwrap();
        zirv_flow_store::enqueue(pool, &job).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[SegmentRefresh::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_clock(clock.clone());
        SegmentRefresh
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
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

    fn manual(at: OffsetDateTime) -> (Clock, ManualClock) {
        let clock = ManualClock::new(at);
        (Clock::Manual(clock.clone()), clock)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_time_window_segment_gains_members_as_the_clock_advances_with_no_new_data(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_order_event(&pool, ws).await;
        let start = OffsetDateTime::now_utc();
        let start = start
            .replace_nanosecond(start.microsecond() * 1000)
            .unwrap();
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let ada_order = EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "ada@example.com".into())],
            name: "order".into(),
            properties: json!({"total": 10}).as_object().cloned().unwrap(),
            timestamp: Some(start - time::Duration::days(20)),
            received_at: start,
            message_id: None,
            trace: TraceContext::root(Cause::api("t")),
        };
        zirv_flow_store::apply_event_batch(&pool, ws, &key, &[ada_order])
            .await
            .unwrap();
        let ada: Uuid =
            sqlx::query_scalar("SELECT person_id FROM identities WHERE workspace_id = $1")
                .bind(ws.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        let (clock, handle) = manual(start);
        let seg = create(
            &pool,
            ws,
            "Lapsed",
            json!({"not": {"event": "order", "window": {"last": "P30D"}}, "v": 1}),
            &TraceContext::root(Cause::api("save")),
        )
        .await;

        run(
            &pool,
            ws,
            seg.id,
            "save",
            TraceContext::root(Cause::api("job")),
            &clock,
        )
        .await;
        assert_eq!(open(&pool, seg.id).await, [bob]);

        handle.advance(time::Duration::days(11));
        run(
            &pool,
            ws,
            seg.id,
            "schedule",
            TraceContext::root(Cause::schedule(Uuid::new_v4())),
            &clock,
        )
        .await;

        let mut both = vec![ada, bob];
        both.sort();
        assert_eq!(open(&pool, seg.id).await, both);
        let (cause, at): (String, OffsetDateTime) = sqlx::query_as(
            "SELECT properties ->> 'cause', occurred_at FROM events \
             WHERE name = 'segment.entered' AND person_id = $1",
        )
        .bind(ada)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cause, "time");
        assert_eq!(at, start + time::Duration::days(11));
        let computed: Option<OffsetDateTime> =
            sqlx::query_scalar("SELECT computed_at FROM segments WHERE id = $1")
                .bind(seg.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(computed, Some(start + time::Duration::days(11)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_definition_change_keeps_the_saves_trace_and_cause_even_when_a_scheduled_refresh_applies_it(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let pro = person(&pool, ws, "pro@example.com", "pro").await;
        let free = person(&pool, ws, "free@example.com", "free").await;
        let (clock, _) = manual(OffsetDateTime::now_utc());
        let first_save = TraceContext::root(Cause::api("save-1"));
        let seg = create(
            &pool,
            ws,
            "Plan",
            json!({"op": "eq", "trait": "plan", "value": "pro", "v": 1}),
            &first_save,
        )
        .await;

        run(
            &pool,
            ws,
            seg.id,
            "save",
            TraceContext::root(Cause::api("job-1")),
            &clock,
        )
        .await;
        assert_eq!(open(&pool, seg.id).await, [pro]);
        let initial: (String, Uuid) = sqlx::query_as(
            "SELECT properties ->> 'cause', trace_id FROM events WHERE name = 'segment.entered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(initial, ("initial".into(), first_save.trace_id.as_uuid()));

        let second_save = TraceContext::root(Cause::api("save-2"));
        resave(
            &pool,
            ws,
            seg.id,
            json!({"op": "eq", "trait": "plan", "value": "free", "v": 1}),
            &second_save,
        )
        .await;
        // A scheduled refresh gets there before the save's own job.
        let scheduled = TraceContext::root(Cause::schedule(Uuid::new_v4()));
        run(&pool, ws, seg.id, "schedule", scheduled.clone(), &clock).await;
        // The save's job then finds nothing left to change.
        run(&pool, ws, seg.id, "save", second_save.clone(), &clock).await;

        assert_eq!(open(&pool, seg.id).await, [free]);
        let changes: Vec<(String, String, Uuid)> = sqlx::query_as(
            "SELECT name, properties ->> 'cause', trace_id FROM events \
             WHERE name LIKE 'segment.%' AND trace_id = $1 ORDER BY name",
        )
        .bind(second_save.trace_id.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        let expected = second_save.trace_id.as_uuid();
        assert_eq!(
            changes,
            [
                (
                    "segment.entered".into(),
                    "definition_change".into(),
                    expected
                ),
                (
                    "segment.exited".into(),
                    "definition_change".into(),
                    expected
                ),
            ]
        );
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, second_save.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| {
            record.cause.kind == zirv_flow_core::CauseKind::DefinitionChange
                && record.reason.as_deref() == Some("definition_change")
        }));
        let (computed, count): (Option<i32>, i32) =
            sqlx::query_as("SELECT computed_version, member_count FROM segments WHERE id = $1")
                .bind(seg.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((computed, count), (Some(2), 1));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_manual_refresh_carries_the_manual_cause_and_the_requests_trace(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let (clock, _) = manual(OffsetDateTime::now_utc());
        let seg = create(
            &pool,
            ws,
            "Plan",
            json!({"op": "eq", "trait": "plan", "value": "pro", "v": 1}),
            &TraceContext::root(Cause::api("save")),
        )
        .await;
        run(
            &pool,
            ws,
            seg.id,
            "save",
            TraceContext::root(Cause::api("job")),
            &clock,
        )
        .await;
        let pro = person(&pool, ws, "pro@example.com", "pro").await;
        let request = TraceContext::root(Cause::manual(UserId::from_uuid(Uuid::new_v4())));

        run(&pool, ws, seg.id, "manual", request.clone(), &clock).await;

        assert_eq!(open(&pool, seg.id).await, [pro]);
        let (cause, trace): (String, Uuid) = sqlx::query_as(
            "SELECT properties ->> 'cause', trace_id FROM events WHERE name = 'segment.entered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (cause, trace),
            ("manual".into(), request.trace_id.as_uuid())
        );
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, request.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(records[0].cause, request.cause);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_definition_that_no_longer_compiles_leaves_the_members_and_records_a_failure(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        let pro = person(&pool, ws, "pro@example.com", "pro").await;
        let (clock, _) = manual(OffsetDateTime::now_utc());
        let seg = create(
            &pool,
            ws,
            "Plan",
            json!({"op": "eq", "trait": "plan", "value": "pro", "v": 1}),
            &TraceContext::root(Cause::api("save")),
        )
        .await;
        run(
            &pool,
            ws,
            seg.id,
            "save",
            TraceContext::root(Cause::api("job")),
            &clock,
        )
        .await;
        zirv_flow_store::update_trait_definition(
            &pool,
            ws,
            "plan",
            &TraitDefinitionUpdate {
                archived: Some(true),
                ..TraitDefinitionUpdate::default()
            },
        )
        .await
        .unwrap();
        let job = TraceContext::root(Cause::schedule(Uuid::new_v4()));

        run(&pool, ws, seg.id, "schedule", job.clone(), &clock).await;

        assert_eq!(open(&pool, seg.id).await, [pro]);
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, job.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            (records[0].outcome.as_str(), records[0].reason.as_deref()),
            ("failed", Some("definition_invalid"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_segment_is_skipped_with_a_decision_and_changes_nothing(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = workspace(&pool).await;
        define_trait(&pool, ws, "plan").await;
        person(&pool, ws, "pro@example.com", "pro").await;
        let (clock, _) = manual(OffsetDateTime::now_utc());
        let seg = create(
            &pool,
            ws,
            "Plan",
            json!({"op": "eq", "trait": "plan", "value": "pro", "v": 1}),
            &TraceContext::root(Cause::api("save")),
        )
        .await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::lock_segments(&mut tx, ws).await.unwrap();
        zirv_flow_store::archive_segment(&mut tx, ws, seg.id)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let job = TraceContext::root(Cause::schedule(Uuid::new_v4()));

        run(&pool, ws, seg.id, "schedule", job.clone(), &clock).await;

        assert!(open(&pool, seg.id).await.is_empty());
        let (records, _) = zirv_flow_store::list_trace(&pool, ws, job.trace_id, 10)
            .await
            .unwrap();
        assert_eq!(
            (records[0].outcome.as_str(), records[0].reason.as_deref()),
            ("skipped", Some("archived_or_gone"))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_context_reads_the_clock_it_was_given_and_the_database_by_default(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let mut job = NewJob::new("a.kind", TraceContext::root(Cause::api("r")));
        job.unique_key = Some("k".into());
        zirv_flow_store::enqueue(&pool, &job).await.unwrap();
        let claimed =
            zirv_flow_store::claim_job(&pool, "w", &["a.kind"], Duration::from_secs(30), None)
                .await
                .unwrap()
                .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(3);

        let ctx = JobContext::new(&mut tx, &claimed);
        assert!(matches!(ctx.clock(), Clock::Database));
        let mut ctx = ctx.with_clock(Clock::Manual(ManualClock::new(at)));
        let clock = ctx.clock().clone();

        assert_eq!(clock.now(ctx.conn()).await.unwrap(), at);
    }
}
