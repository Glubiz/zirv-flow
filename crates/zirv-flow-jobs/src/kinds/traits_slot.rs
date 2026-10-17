//! `traits.slot_backfill` and `traits.slot_clear`: the two long steps of a trait slot's life
//! (architecture section 6; spec #125).
//!
//! The payload is `{workspace_id, key, slot}`, plus an `after` person-id cursor on a follow-up
//! backfill batch. Each run covers one batch in its own transaction and enqueues the next on the
//! same trace. The last batch enqueues a run with `finish: true`, which takes the exclusive slot
//! lock as its first statement (holding no row locks) and moves the slot to `ready` (backfill)
//! or frees it (clear). A job whose assignment has since changed does nothing.

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::SlotColumn;
use zirv_flow_store::{
    NewJob, SlotStep, backfill_slot_batch, clear_slot_batch, finish_backfill, free_trait_slot,
};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct TraitsSlotBackfill;
pub(crate) struct TraitsSlotClear;

struct Work {
    workspace: WorkspaceId,
    key: String,
    slot: SlotColumn,
    after: Option<Uuid>,
    finish: bool,
}

impl JobKind for TraitsSlotBackfill {
    const KIND: &'static str = "traits.slot_backfill";

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let work = parse(ctx, &payload)?;
        if work.finish {
            finish_backfill(ctx.conn(), work.workspace, &work.key, work.slot).await?;
            return Ok(());
        }
        let step =
            backfill_slot_batch(ctx.conn(), work.workspace, &work.key, work.slot, work.after)
                .await?;
        follow_up(ctx, Self::KIND, &work, step).await
    }
}

impl JobKind for TraitsSlotClear {
    const KIND: &'static str = "traits.slot_clear";

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let work = parse(ctx, &payload)?;
        if work.finish {
            free_trait_slot(ctx.conn(), work.workspace, &work.key, work.slot).await?;
            return Ok(());
        }
        let step = clear_slot_batch(ctx.conn(), work.workspace, &work.key, work.slot).await?;
        follow_up(ctx, Self::KIND, &work, step).await
    }
}

async fn follow_up(
    ctx: &mut JobContext<'_>,
    kind: &'static str,
    work: &Work,
    step: SlotStep,
) -> Result<(), JobError> {
    let mut payload = json!({
        "workspace_id": work.workspace,
        "key": work.key,
        "slot": work.slot.as_str(),
    });
    match step {
        SlotStep::Stale => return Ok(()),
        SlotStep::More(cursor) => {
            payload["after"] = json!(cursor);
        }
        SlotStep::Finish => {
            payload["finish"] = json!(true);
        }
    }
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
    Ok(Work {
        workspace,
        key,
        slot,
        after,
        finish,
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

        let first =
            parse_payload(&json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1"})).unwrap();
        let next = parse_payload(
            &json!({"workspace_id": ws, "key": "plan", "slot": "trait_n2", "after": person}),
        )
        .unwrap();
        let last = parse_payload(
            &json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1", "finish": true}),
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
        assert!(parse_payload(&json!({"key": "plan", "slot": "trait_s1"})).is_err());
        assert!(
            parse_payload(
                &json!({"workspace_id": ws, "key": "plan", "slot": "trait_s1", "after": 5})
            )
            .is_err()
        );
    }
}
