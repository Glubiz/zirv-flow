//! Typed slot columns for hot traits (architecture sections 6 and 21.1; spec #125).
//!
//! A trait is promoted to one of the 30 slot columns of `people`. The slot is a copy of the value
//! in `people.traits`, so a backfill racing the writers must never leave it stale. The proof is
//! the lock order:
//!
//! - Every transaction that writes `people.traits` (or a slot) takes
//!   `pg_advisory_xact_lock_shared(SLOT_LOCK_CLASS, hashtext(workspace))` in a statement of its
//!   own, and only then reads the workspace's assignments ([`lock_and_read_writer_slots`]). The
//!   transaction is READ COMMITTED, so that read takes a fresh snapshot after the lock wait.
//! - Every change of an assignment (assign, retire, ready, free) takes the exclusive form of the
//!   same lock first, which waits for every in-flight writer to commit.
//!
//! So once an assignment commits, every later change of a row also writes the slot, and the
//! backfill only has to bring the rows that exist to `conv(traits -> key)`. A writer that has no
//! transaction id yet is held by the lock like any other, which an xmin fence would miss.

use std::fmt::Write as _;

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::people::{SlotColumn, SlotKind, SlotState};

use super::registry::find_trait_definition_for_update;
use crate::StoreError;

/// The class (first key) of the advisory lock that orders slot assignment against trait writers.
pub const SLOT_LOCK_CLASS: i32 = 7000;

/// How many people one backfill or clear batch covers.
pub const SLOT_BATCH: i64 = 500;

/// A trait's slot and where it is in its life.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SlotAssignment {
    pub slot: SlotColumn,
    pub state: SlotState,
}

/// A refused assignment change.
#[derive(Debug)]
pub enum SlotError {
    /// No such trait.
    NoSuchTrait,
    /// The trait is archived.
    Archived,
    /// A `string_list` trait stays `jsonb`.
    NotSlottable,
    /// Every slot of the trait's type is taken.
    Full {
        kind: SlotKind,
        capacity: usize,
    },
    /// The slot is being retired; it can be assigned again once it is free.
    Retiring,
    /// The trait holds a slot, so its type is locked.
    TypeLocked,
    /// The trait holds no slot.
    NotIndexed,
    Store(StoreError),
}

impl From<StoreError> for SlotError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// What assigning did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Assigned {
    /// A free slot was taken and its backfill must be enqueued.
    New(SlotColumn),
    /// The trait already holds this slot, `backfilling` or `ready`.
    Existing(SlotAssignment),
}

fn lock_error(error: sqlx::Error) -> StoreError {
    StoreError::query(error)
}

/// Takes the writer lock of the workspace until the transaction ends.
pub(crate) async fn lock_slots_shared(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1, hashtext($2::text))")
        .bind(SLOT_LOCK_CLASS)
        .bind(workspace.as_uuid().to_string())
        .execute(connection)
        .await
        .map_err(lock_error)?;
    Ok(())
}

/// Holds the writer lock of the workspace until the transaction ends, so a slot the caller read as
/// `ready` cannot be retired, or one it read as `backfilling` made ready, before it commits. A
/// segment refresh calls it before it reads the catalog and runs the compiled query.
pub async fn hold_trait_slots(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    lock_slots_shared(connection, workspace).await
}

/// Takes the assignment lock of the workspace until the transaction ends. It waits for every
/// writer in flight and queues the ones that start after.
pub(crate) async fn lock_slots_exclusive(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
        .bind(SLOT_LOCK_CLASS)
        .bind(workspace.as_uuid().to_string())
        .execute(connection)
        .await
        .map_err(lock_error)?;
    Ok(())
}

fn decode(slot: &str, state: &str) -> Result<SlotAssignment, StoreError> {
    let corrupt = || StoreError::query(sqlx::Error::Decode("unknown trait slot".into()));
    Ok(SlotAssignment {
        slot: slot.parse().map_err(|_| corrupt())?,
        state: state.parse().map_err(|_| corrupt())?,
    })
}

/// Every assignment of the workspace, with its trait key, whatever its state.
pub async fn trait_slot_assignments(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<(String, SlotAssignment)>, StoreError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT key, slot, slot_state FROM trait_definitions \
         WHERE workspace_id = $1 AND slot IS NOT NULL ORDER BY key",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(|(key, slot, state)| Ok((key, decode(&slot, &state)?)))
        .collect()
}

/// The slots the compiler may read: those in state `ready`. Read it in the same snapshot as the
/// catalog.
pub async fn ready_trait_slots(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<(String, SlotColumn)>, StoreError> {
    Ok(trait_slot_assignments(connection, workspace)
        .await?
        .into_iter()
        .filter(|(_, assignment)| assignment.state == SlotState::Ready)
        .map(|(key, assignment)| (key, assignment.slot))
        .collect())
}

/// Takes the writer lock in a statement of its own, then reads the slots every trait writer must
/// fill: those `backfilling` or `ready`. Call it before the statement that writes
/// `people.traits`, in the same transaction.
pub(crate) async fn lock_and_read_writer_slots(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<(String, SlotColumn)>, StoreError> {
    lock_slots_shared(connection, workspace).await?;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, slot FROM trait_definitions \
         WHERE workspace_id = $1 AND slot IS NOT NULL AND slot_state IN ('backfilling', 'ready') \
         ORDER BY key",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter()
        .map(|(key, slot)| {
            let slot = slot
                .parse()
                .map_err(|_| StoreError::query(sqlx::Error::Decode("unknown trait slot".into())))?;
            Ok((key, slot))
        })
        .collect()
}

/// `, trait_s1 = <conv>` for each slot, over the `jsonb` object `traits_sql`. The keys are bound
/// as `$first`, `$first + 1`, and so on, in `slots` order; bind them after the other parameters.
pub(crate) fn slot_assignments_sql(
    slots: &[(String, SlotColumn)],
    traits_sql: &str,
    first: usize,
) -> String {
    let mut sql = String::new();
    for (offset, (_, slot)) in slots.iter().enumerate() {
        let key = format!("${}::text", first + offset);
        let _ = write!(
            sql,
            ", {} = {}",
            slot.as_str(),
            slot.value_sql(traits_sql, &key)
        );
    }
    sql
}

/// `, trait_s1 = NULL` for each slot.
pub(crate) fn slot_nulls_sql(slots: &[(String, SlotColumn)]) -> String {
    slots
        .iter()
        .map(|(_, slot)| format!(", {} = NULL", slot.as_str()))
        .collect()
}

/// Assigns the lowest free slot of the trait's type in state `backfilling`, under the exclusive
/// lock. The caller enqueues the backfill when the result is [`Assigned::New`].
pub async fn assign_trait_slot(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Assigned, SlotError> {
    lock_slots_exclusive(connection, workspace).await?;
    let definition = find_trait_definition_for_update(&mut *connection, workspace, key)
        .await?
        .ok_or(SlotError::NoSuchTrait)?;
    if let Some(assignment) = assignment_of(connection, workspace, key).await? {
        return match assignment.state {
            SlotState::Retiring => Err(SlotError::Retiring),
            _ => Ok(Assigned::Existing(assignment)),
        };
    }
    if definition.archived {
        return Err(SlotError::Archived);
    }
    let kind = SlotKind::of(definition.trait_type).ok_or(SlotError::NotSlottable)?;
    let taken: Vec<String> = sqlx::query_scalar(
        "SELECT slot FROM trait_definitions WHERE workspace_id = $1 AND slot IS NOT NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let free = SlotColumn::of_kind(kind).find(|slot| !taken.iter().any(|t| t == slot.as_str()));
    let Some(slot) = free else {
        return Err(SlotError::Full {
            kind,
            capacity: kind.capacity(),
        });
    };
    sqlx::query(
        "UPDATE trait_definitions SET slot = $3, slot_state = 'backfilling', updated_at = now() \
         WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(slot.as_str())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Assigned::New(slot))
}

/// The assignment of one trait, if it has one.
pub async fn trait_slot_of(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<SlotAssignment>, StoreError> {
    assignment_of(connection, workspace, key).await
}

async fn assignment_of(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Option<SlotAssignment>, StoreError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT slot, slot_state FROM trait_definitions WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    match row {
        Some((Some(slot), Some(state))) => decode(&slot, &state).map(Some),
        _ => Ok(None),
    }
}

/// What retiring did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retired {
    /// The slot went from `backfilling` or `ready` to `retiring`; its clear must be enqueued.
    Now(SlotColumn),
    /// The slot was already retiring.
    Already(SlotColumn),
}

/// Sets the trait's slot to `retiring` under the exclusive lock: the compiler stops reading it
/// and writers stop writing it.
pub async fn retire_trait_slot(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
) -> Result<Retired, SlotError> {
    lock_slots_exclusive(connection, workspace).await?;
    find_trait_definition_for_update(&mut *connection, workspace, key)
        .await?
        .ok_or(SlotError::NoSuchTrait)?;
    let assignment = assignment_of(connection, workspace, key)
        .await?
        .ok_or(SlotError::NotIndexed)?;
    if assignment.state == SlotState::Retiring {
        return Ok(Retired::Already(assignment.slot));
    }
    sqlx::query(
        "UPDATE trait_definitions SET slot_state = 'retiring', updated_at = now() \
         WHERE workspace_id = $1 AND key = $2",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Retired::Now(assignment.slot))
}

/// The progress of one backfill or clear batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotStep {
    /// More people may remain: run another batch after this cursor.
    More(Option<Uuid>),
    /// Every person is done: run the finishing step.
    Finish,
    /// The assignment is no longer in the state this job works on; nothing to do.
    Stale,
}

/// Backfills up to [`SLOT_BATCH`] people after `after`, in id order, with `conv(traits -> key)`.
///
/// It takes the writer lock first, so a retire cannot commit mid-batch and the assignment it
/// reads stays true to commit. The value is computed from the row version the batch locks: under
/// READ COMMITTED a row a writer changed meanwhile is re-read after that writer commits, so the
/// batch writes the latest value or the one the writer wrote.
pub async fn backfill_slot_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
    after: Option<Uuid>,
) -> Result<SlotStep, StoreError> {
    lock_slots_shared(connection, workspace).await?;
    if !holds(connection, workspace, key, slot, SlotState::Backfilling).await? {
        return Ok(SlotStep::Stale);
    }
    let sql = format!(
        "UPDATE people SET {column} = {value} \
         WHERE workspace_id = $1 AND id IN ( \
             SELECT id FROM people WHERE workspace_id = $1 AND ($2::uuid IS NULL OR id > $2) \
             ORDER BY id LIMIT $3 FOR UPDATE) \
         RETURNING id",
        column = slot.as_str(),
        value = slot.value_sql("traits", "$4::text"),
    );
    let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(after)
        .bind(SLOT_BATCH)
        .bind(key)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)?;
    let full = i64::try_from(ids.len()).is_ok_and(|n| n == SLOT_BATCH);
    Ok(if full {
        SlotStep::More(ids.into_iter().max())
    } else {
        SlotStep::Finish
    })
}

/// Sets the slot `ready` under the exclusive lock, after the last batch. Writers already write
/// it, so the compiler can read it as soon as this commits.
pub async fn finish_backfill(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
) -> Result<bool, StoreError> {
    lock_slots_exclusive(connection, workspace).await?;
    let done = sqlx::query(
        "UPDATE trait_definitions SET slot_state = 'ready', updated_at = now() \
         WHERE workspace_id = $1 AND key = $2 AND slot = $3 AND slot_state = 'backfilling'",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(slot.as_str())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// NULLs the slot column of up to [`SLOT_BATCH`] people of a `retiring` slot. A cleared row
/// leaves the predicate, so no cursor is needed.
pub async fn clear_slot_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
) -> Result<SlotStep, StoreError> {
    lock_slots_shared(connection, workspace).await?;
    if !holds(connection, workspace, key, slot, SlotState::Retiring).await? {
        return Ok(SlotStep::Stale);
    }
    let sql = format!(
        "UPDATE people SET {column} = NULL \
         WHERE workspace_id = $1 AND id IN ( \
             SELECT id FROM people WHERE workspace_id = $1 AND {column} IS NOT NULL \
             ORDER BY id LIMIT $2 FOR UPDATE) \
         RETURNING id",
        column = slot.as_str(),
    );
    let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(SLOT_BATCH)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)?;
    let full = i64::try_from(ids.len()).is_ok_and(|n| n == SLOT_BATCH);
    Ok(if full {
        SlotStep::More(None)
    } else {
        SlotStep::Finish
    })
}

/// Frees a cleared slot under the exclusive lock, so another trait of the type can take it.
pub async fn free_trait_slot(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
) -> Result<bool, StoreError> {
    lock_slots_exclusive(connection, workspace).await?;
    let done = sqlx::query(
        "UPDATE trait_definitions SET slot = NULL, slot_state = NULL, updated_at = now() \
         WHERE workspace_id = $1 AND key = $2 AND slot = $3 AND slot_state = 'retiring'",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(slot.as_str())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

async fn holds(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    slot: SlotColumn,
    state: SlotState,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM trait_definitions \
         WHERE workspace_id = $1 AND key = $2 AND slot = $3 AND slot_state = $4)",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(slot.as_str())
    .bind(state.as_str())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_assignment_sql_binds_each_key_in_order_and_names_only_fixed_columns() {
        let slots = vec![
            ("plan".to_owned(), SlotColumn::S1),
            ("age".to_owned(), SlotColumn::N1),
        ];

        let sql = slot_assignments_sql(&slots, "$3::jsonb", 5);

        assert!(sql.starts_with(", trait_s1 = CASE WHEN jsonb_typeof(($3::jsonb -> $5::text))"));
        assert!(sql.contains(", trait_n1 = CASE WHEN jsonb_typeof(($3::jsonb -> $6::text))"));
        assert_eq!(slot_assignments_sql(&[], "$3::jsonb", 5), "");
        assert_eq!(slot_nulls_sql(&slots), ", trait_s1 = NULL, trait_n1 = NULL");
    }
}

#[cfg(test)]
mod db_tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use time::{Duration, OffsetDateTime};
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::{
        NewTraitDefinition, ProfileRecord, apply_profile_batch, begin, commit,
        create_trait_definition, create_workspace,
    };

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("slots").unwrap();
        let slug = Slug::parse("slots").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn define(pool: &PgPool, ws: WorkspaceId, key: &str, trait_type: TraitType) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: key.into(),
                trait_type,
                precedence: zirv_flow_core::people::Precedence::Latest,
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

    fn record(person: usize, traits: Value, at: OffsetDateTime) -> ProfileRecord {
        ProfileRecord {
            source: "crm".into(),
            stream: "s".into(),
            record_key: format!("r{person}"),
            identifiers: vec![("email".into(), format!("p{person}@example.com"))],
            traits: traits.as_object().cloned().unwrap(),
            observed_at: at,
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    const START: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);

    async fn apply(pool: &PgPool, ws: WorkspaceId, records: &[ProfileRecord]) {
        loop {
            match apply_profile_batch(pool, ws, records).await {
                Ok(_) => return,
                Err(error) if error.is_retryable() => continue,
                Err(error) => panic!("apply failed: {error}"),
            }
        }
    }

    async fn assign(pool: &PgPool, ws: WorkspaceId, key: &str) -> Assigned {
        loop {
            let mut tx = begin(pool).await.unwrap();
            match assign_trait_slot(&mut tx, ws, key).await {
                Ok(assigned) => {
                    commit(tx).await.unwrap();
                    return assigned;
                }
                Err(SlotError::Store(error)) if error.is_retryable() => continue,
                Err(error) => panic!("assign failed: {error:?}"),
            }
        }
    }

    async fn retire(pool: &PgPool, ws: WorkspaceId, key: &str) -> Retired {
        let mut tx = begin(pool).await.unwrap();
        let retired = retire_trait_slot(&mut tx, ws, key).await.unwrap();
        commit(tx).await.unwrap();
        retired
    }

    async fn ready(pool: &PgPool, ws: WorkspaceId) -> Vec<(String, SlotColumn)> {
        let mut tx = begin(pool).await.unwrap();
        ready_trait_slots(&mut tx, ws).await.unwrap()
    }

    /// Backfills to the end and makes the slot ready, a batch per transaction like the job.
    async fn backfill(pool: &PgPool, ws: WorkspaceId, key: &str, slot: SlotColumn) {
        let mut after = None;
        loop {
            let mut tx = begin(pool).await.unwrap();
            let step = match backfill_slot_batch(&mut tx, ws, key, slot, after).await {
                Ok(step) => step,
                Err(error) if error.is_retryable() => continue,
                Err(error) => panic!("backfill failed: {error}"),
            };
            commit(tx).await.unwrap();
            match step {
                SlotStep::More(cursor) => after = cursor,
                SlotStep::Finish => break,
                SlotStep::Stale => panic!("stale backfill"),
            }
        }
        let mut tx = begin(pool).await.unwrap();
        assert!(finish_backfill(&mut tx, ws, key, slot).await.unwrap());
        commit(tx).await.unwrap();
    }

    async fn mismatches(pool: &PgPool, ws: WorkspaceId, key: &str, slot: SlotColumn) -> i64 {
        let sql = format!(
            "SELECT count(*) FROM people WHERE workspace_id = $1 \
             AND {col} IS DISTINCT FROM {value}",
            col = slot.as_str(),
            value = slot.value_sql("traits", "$2::text"),
        );
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(ws.as_uuid())
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn set_all_slots(pool: &PgPool, slot: SlotColumn, value: &str) {
        let sql = format!("UPDATE people SET {} = '{value}'", slot.as_str());
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_backfill_racing_writers_leaves_every_slot_equal_to_its_trait(pool: PgPool) {
        let ws = workspace(&pool).await;
        define(&pool, ws, "plan", TraitType::String).await;
        // More people than two batches, so the backfill spans transactions the writers
        // interleave with.
        let people = 1_100usize;
        let all: Vec<usize> = (0..people).collect();
        for chunk in all.chunks(250) {
            let records: Vec<_> = chunk
                .iter()
                .map(|&p| record(p, json!({"plan": format!("v0-{p}")}), START))
                .collect();
            apply(&pool, ws, &records).await;
        }
        let Assigned::New(slot) = assign(&pool, ws, "plan").await else {
            panic!("not assigned");
        };

        // Four writers each rewrite their quarter of the people six times while the backfill
        // runs; each rewrite is a later observation, so it wins.
        let mut tasks = Vec::new();
        for writer in 0..4usize {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                for round in 1..=6i64 {
                    let at = START + Duration::minutes(round);
                    let records: Vec<_> = (0..people)
                        .filter(|p| p % 4 == writer)
                        .map(|p| record(p, json!({"plan": format!("w{writer}-{round}-{p}")}), at))
                        .collect();
                    for chunk in records.chunks(40) {
                        apply(&pool, ws, chunk).await;
                    }
                }
            }));
        }
        let filler = {
            let pool = pool.clone();
            tokio::spawn(async move { backfill(&pool, ws, "plan", slot).await })
        };
        for task in tasks {
            task.await.unwrap();
        }
        filler.await.unwrap();

        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 0);
        assert_eq!(ready(&pool, ws).await, vec![("plan".to_owned(), slot)]);
        let filled: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM people WHERE {} IS NOT NULL",
            slot.as_str()
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(filled, i64::try_from(people).unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn writers_fill_a_backfilling_and_ready_slot_but_not_a_retiring_one(pool: PgPool) {
        let ws = workspace(&pool).await;
        define(&pool, ws, "plan", TraitType::String).await;
        apply(&pool, ws, &[record(1, json!({"plan": "a"}), START)]).await;
        let Assigned::New(slot) = assign(&pool, ws, "plan").await else {
            panic!("not assigned");
        };
        let at = |n| START + Duration::minutes(n);

        apply(&pool, ws, &[record(1, json!({"plan": "b"}), at(1))]).await;
        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 0, "backfilling");

        backfill(&pool, ws, "plan", slot).await;
        apply(&pool, ws, &[record(1, json!({"plan": "c"}), at(2))]).await;
        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 0, "ready");

        assert_eq!(retire(&pool, ws, "plan").await, Retired::Now(slot));
        apply(&pool, ws, &[record(1, json!({"plan": "d"}), at(3))]).await;
        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 1, "not written");
        assert!(ready(&pool, ws).await.is_empty(), "not read");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_slot_is_retired_cleared_freed_and_reassigned_with_leftovers_overwritten(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        define(&pool, ws, "plan", TraitType::String).await;
        define(&pool, ws, "tier", TraitType::String).await;
        apply(
            &pool,
            ws,
            &[record(1, json!({"plan": "gold", "tier": "t1"}), START)],
        )
        .await;
        let Assigned::New(slot) = assign(&pool, ws, "plan").await else {
            panic!("not assigned");
        };
        backfill(&pool, ws, "plan", slot).await;

        // Marking again while indexed changes nothing.
        let again = assign(&pool, ws, "plan").await;
        assert!(matches!(again, Assigned::Existing(a) if a.state == SlotState::Ready));
        retire(&pool, ws, "plan").await;
        assert_eq!(retire(&pool, ws, "plan").await, Retired::Already(slot));
        let mut tx = begin(&pool).await.unwrap();
        assert!(matches!(
            assign_trait_slot(&mut tx, ws, "plan").await,
            Err(SlotError::Retiring)
        ));
        drop(tx);

        loop {
            let mut tx = begin(&pool).await.unwrap();
            let step = clear_slot_batch(&mut tx, ws, "plan", slot).await.unwrap();
            commit(tx).await.unwrap();
            if step == SlotStep::Finish {
                break;
            }
        }
        let mut tx = begin(&pool).await.unwrap();
        assert!(free_trait_slot(&mut tx, ws, "plan", slot).await.unwrap());
        commit(tx).await.unwrap();

        // A leftover in a free slot must not survive the next backfill.
        set_all_slots(&pool, slot, "stale").await;
        let Assigned::New(reused) = assign(&pool, ws, "tier").await else {
            panic!("not assigned");
        };
        assert_eq!(reused, slot, "the lowest free slot is reused");
        backfill(&pool, ws, "tier", reused).await;
        assert_eq!(mismatches(&pool, ws, "tier", reused).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_slots_of_a_type_run_out_and_a_string_list_has_none(pool: PgPool) {
        let ws = workspace(&pool).await;
        for n in 0..9 {
            define(&pool, ws, &format!("s{n}"), TraitType::String).await;
        }
        define(&pool, ws, "tags", TraitType::StringList).await;
        for n in 0..8 {
            assign(&pool, ws, &format!("s{n}")).await;
        }
        let mut tx = begin(&pool).await.unwrap();
        assert!(matches!(
            assign_trait_slot(&mut tx, ws, "s8").await,
            Err(SlotError::Full {
                kind: SlotKind::String,
                capacity: 8
            })
        ));
        assert!(matches!(
            assign_trait_slot(&mut tx, ws, "tags").await,
            Err(SlotError::NotSlottable)
        ));
        assert!(matches!(
            assign_trait_slot(&mut tx, ws, "ghost").await,
            Err(SlotError::NoSuchTrait)
        ));
        assert!(matches!(
            retire_trait_slot(&mut tx, ws, "s8").await,
            Err(SlotError::NotIndexed)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_empties_the_losers_slots(pool: PgPool) {
        let ws = workspace(&pool).await;
        define(&pool, ws, "plan", TraitType::String).await;
        apply(
            &pool,
            ws,
            &[
                record(1, json!({"plan": "y"}), START),
                record(3, json!({"plan": "z"}), START),
            ],
        )
        .await;
        let Assigned::New(slot) = assign(&pool, ws, "plan").await else {
            panic!("not assigned");
        };
        backfill(&pool, ws, "plan", slot).await;
        // A record carrying both identifiers merges the two people.
        let mut both = record(4, json!({"plan": "m"}), START + Duration::minutes(1));
        both.identifiers = vec![
            ("email".into(), "p1@example.com".into()),
            ("email".into(), "p3@example.com".into()),
        ];
        apply(&pool, ws, &[both]).await;
        let merged: i64 =
            sqlx::query_scalar("SELECT count(*) FROM people WHERE merged_into IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(merged, 1);
        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 0);
    }

    /// Staged interleaving of the window the lock closes: a writer reads the assignments before
    /// the assignment commits and writes after the backfill would have finished. With the shared
    /// lock the assignment waits for the writer; without it the slot is left stale.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_writer_that_read_the_assignments_before_they_committed_cannot_leave_a_stale_slot(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        define(&pool, ws, "plan", TraitType::String).await;
        apply(&pool, ws, &[record(1, json!({"plan": "old"}), START)]).await;

        // The writer takes the lock and reads the assignments: none yet.
        let mut writer = begin(&pool).await.unwrap();
        let slots = lock_and_read_writer_slots(&mut writer, ws).await.unwrap();
        assert!(slots.is_empty());

        let assigner = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let Assigned::New(slot) = assign(&pool, ws, "plan").await else {
                    panic!("not assigned");
                };
                backfill(&pool, ws, "plan", slot).await;
                slot
            })
        };
        // Long enough for an unordered assignment and backfill to finish.
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;

        // The writer now changes the trait, writing the slots it saw (none).
        let sql = format!(
            "UPDATE people SET traits = $1{} WHERE workspace_id = $2",
            slot_assignments_sql(&slots, "$1::jsonb", 3)
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(json!({"plan": "new"}))
            .bind(ws.as_uuid())
            .execute(&mut *writer)
            .await
            .unwrap();
        commit(writer).await.unwrap();

        let slot = assigner.await.unwrap();
        assert_eq!(mismatches(&pool, ws, "plan", slot).await, 0);
    }
}
