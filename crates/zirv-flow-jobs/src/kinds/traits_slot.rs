//! `traits.slot_backfill` and `traits.slot_clear`: the two long steps of a trait slot's life
//! (architecture section 6; spec #125).
//!
//! The payload is `{workspace_id, key, slot}`, plus an `after` person-id cursor on a follow-up
//! backfill batch. Each run covers one batch in its own transaction and enqueues the next on the
//! same trace. The last batch enqueues a run with `finish: true`, which takes the exclusive slot
//! lock as its first statement (holding no row locks) and moves the slot to `ready` (backfill)
//! or frees it (clear). A job whose assignment has since changed does nothing.
//!
//! A finish that made the slot `ready` also enqueues a run with `analyze: true`. It runs after the
//! finish has committed and refreshes the planner statistics of the new column, which the backfill
//! leaves empty. A failed `ANALYZE` only logs a warning: the slot stays `ready`.

use serde_json::{Value, json};
use tracing::warn;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::SlotColumn;
use zirv_flow_store::{
    NewJob, SlotStep, analyze_slot_column, backfill_slot_batch, clear_slot_batch, finish_backfill,
    free_trait_slot,
};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct TraitsSlotBackfill;
pub(crate) struct TraitsSlotClear;

struct Work {
    workspace: WorkspaceId,
    key: String,
    slot: SlotColumn,
    generation: i64,
    after: Option<Uuid>,
    finish: bool,
    analyze: bool,
}

impl JobKind for TraitsSlotBackfill {
    const KIND: &'static str = "traits.slot_backfill";

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let work = parse(ctx, &payload)?;
        if work.analyze {
            // The slot is already `ready`, so a failure only logs: autovacuum catches up later.
            if let Err(error) = analyze_slot_column(ctx.conn(), work.slot).await {
                warn!(slot = work.slot.as_str(), %error, "could not analyze a ready trait slot");
            }
            return Ok(());
        }
        if work.finish {
            let ready = finish_backfill(
                ctx.conn(),
                work.workspace,
                &work.key,
                work.slot,
                work.generation,
            )
            .await?;
            if ready {
                enqueue_analyze(ctx, &work).await?;
            }
            return Ok(());
        }
        let step = backfill_slot_batch(
            ctx.conn(),
            work.workspace,
            &work.key,
            work.slot,
            work.generation,
            work.after,
        )
        .await?;
        follow_up(ctx, Self::KIND, &work, step).await
    }
}

impl JobKind for TraitsSlotClear {
    const KIND: &'static str = "traits.slot_clear";

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let work = parse(ctx, &payload)?;
        if work.finish {
            free_trait_slot(
                ctx.conn(),
                work.workspace,
                &work.key,
                work.slot,
                work.generation,
            )
            .await?;
            return Ok(());
        }
        let step = clear_slot_batch(
            ctx.conn(),
            work.workspace,
            &work.key,
            work.slot,
            work.generation,
        )
        .await?;
        follow_up(ctx, Self::KIND, &work, step).await
    }
}

async fn follow_up(
    ctx: &mut JobContext<'_>,
    kind: &'static str,
    work: &Work,
    step: SlotStep,
) -> Result<(), JobError> {
    match step {
        SlotStep::Stale => Ok(()),
        SlotStep::More(cursor) => enqueue_next(ctx, kind, work, "after", json!(cursor)).await,
        SlotStep::Finish => enqueue_next(ctx, kind, work, "finish", json!(true)).await,
    }
}

/// Queues the statistics refresh of a slot that has just become `ready`. It is a job of its own so
/// that it runs after the transaction that set `ready` (holding the exclusive slot lock) has
/// committed: the job commits with that transaction.
async fn enqueue_analyze(ctx: &mut JobContext<'_>, work: &Work) -> Result<(), JobError> {
    enqueue_next(ctx, TraitsSlotBackfill::KIND, work, "analyze", json!(true)).await
}

/// Enqueues the next job of the same assignment, with one extra payload field.
async fn enqueue_next(
    ctx: &mut JobContext<'_>,
    kind: &'static str,
    work: &Work,
    field: &str,
    value: Value,
) -> Result<(), JobError> {
    let mut payload = json!({
        "workspace_id": work.workspace,
        "key": work.key,
        "slot": work.slot.as_str(),
        "generation": work.generation,
    });
    payload[field] = value;
    let mut next = NewJob::new(kind, ctx.trace().clone());
    next.workspace_id = Some(work.workspace);
    next.payload = payload;
    ctx.enqueue(&next).await?;
    Ok(())
}

fn parse(ctx: &JobContext<'_>, payload: &Value) -> Result<Work, JobError> {
    let work = parse_payload(payload)?;
    if ctx.workspace_id() != Some(work.workspace) {
        return Err(JobError::new(
            "the payload names another workspace than the job",
        ));
    }
    Ok(work)
}

fn parse_payload(payload: &Value) -> Result<Work, JobError> {
    let bad = || JobError::new("the payload needs a workspace_id, a key and a slot");
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok())
        .ok_or_else(bad)?;
    let key = payload
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(bad)?
        .to_owned();
    let slot = payload
        .get("slot")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<SlotColumn>().ok())
        .ok_or_else(bad)?;
    let generation = payload
        .get("generation")
        .and_then(Value::as_i64)
        .ok_or_else(bad)?;
    let after = match payload.get("after") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|text| Uuid::parse_str(text).ok())
                .ok_or_else(|| JobError::new("the cursor is not a person id"))?,
        ),
    };
    let finish = payload.get("finish").and_then(Value::as_bool) == Some(true);
    let analyze = payload.get("analyze").and_then(Value::as_bool) == Some(true);
    Ok(Work {
        workspace,
        key,
        slot,
        generation,
        after,
        finish,
        analyze,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_names_are_stable() {
        assert_eq!(TraitsSlotBackfill::KIND, "traits.slot_backfill");
        assert_eq!(TraitsSlotClear::KIND, "traits.slot_clear");
    }

    #[test]
    fn the_payload_needs_a_workspace_a_key_and_a_known_slot() {
        let ws = WorkspaceId::new();
        let person = Uuid::now_v7();

        let first = parse_payload(
            &json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1", "generation": 1}),
        )
        .unwrap();
        let next = parse_payload(
            &json!({"workspace_id": ws, "key": "plan", "slot": "trait_n2", "generation": 2, "after": person}),
        )
        .unwrap();
        let last = parse_payload(
            &json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1", "generation": 1, "finish": true}),
        )
        .unwrap();

        assert_eq!(first.slot, SlotColumn::S1);
        assert!(first.after.is_none() && !first.finish);
        assert_eq!((next.slot, next.after), (SlotColumn::N2, Some(person)));
        assert!(last.finish);
        assert!(parse_payload(&json!({"workspace_id": ws, "key": "plan"})).is_err());
        assert!(
            parse_payload(&json!({"workspace_id": ws, "key": "plan", "slot": "traits"})).is_err()
        );
        assert!(
            parse_payload(&json!({"key": "plan", "slot": "trait_s1", "generation": 1})).is_err()
        );
        assert!(
            parse_payload(
                &json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1", "generation": 1, "after": 5})
            )
            .is_err()
        );
    }

    async fn run_next(pool: &sqlx::PgPool) {
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[TraitsSlotBackfill::KIND],
            std::time::Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .expect("a queued slot job");
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);
        TraitsSlotBackfill
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
    }

    async fn slot_stats(pool: &sqlx::PgPool, slot: SlotColumn) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_stats WHERE schemaname = current_schema() \
             AND tablename = 'people' AND attname = $1",
        )
        .bind(slot.as_str())
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_slot_that_becomes_ready_gets_statistics_from_a_job_after_the_finish(
        pool: sqlx::PgPool,
    ) {
        use zirv_flow_core::people::{Precedence, TraitType};
        use zirv_flow_core::{Slug, WorkspaceName};
        use zirv_flow_store::{Assigned, NewTraitDefinition};

        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        zirv_flow_store::create_trait_definition(
            &pool,
            ws,
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
        for _ in 0..3 {
            sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, $3)")
                .bind(Uuid::now_v7())
                .bind(ws.as_uuid())
                .bind(json!({"plan": "pro"}))
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut connection = pool.acquire().await.unwrap();
        let Ok(Assigned::New(slot, generation)) =
            zirv_flow_store::assign_trait_slot(&mut connection, ws, "plan").await
        else {
            panic!("not assigned");
        };
        drop(connection);
        let mut first = NewJob::new(
            TraitsSlotBackfill::KIND,
            zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t")),
        );
        first.workspace_id = Some(ws);
        first.payload = json!({
            "workspace_id": ws,
            "key": "plan",
            "slot": slot.as_str(),
            "generation": generation,
        });
        zirv_flow_store::enqueue(&pool, &first).await.unwrap();

        // The batch, then the finish, which queues the statistics refresh.
        run_next(&pool).await;
        run_next(&pool).await;
        let state: String = sqlx::query_scalar(
            "SELECT slot_state FROM trait_definitions WHERE workspace_id = $1 AND key = 'plan'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, "ready");
        assert_eq!(slot_stats(&pool, slot).await, 0, "not before the finish");

        run_next(&pool).await;

        assert_eq!(slot_stats(&pool, slot).await, 1);
    }
}
