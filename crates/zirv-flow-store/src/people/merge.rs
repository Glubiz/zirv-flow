//! Merging people (architecture section 7.3): what moves to the survivor in the transaction
//! of the record that joined them, and the job that re-points the rest afterwards.
//!
//! List memberships and consent move in [`merge_people`] too.

use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::{recompute, record_decision};
use super::slots::{lock_and_read_writer_slots, slot_nulls_sql};
use crate::changes::mark_people;
use crate::consent;
use crate::lists::{lock_lists_for_merge, move_on_merge};
use crate::{NewDecision, NewJob, StoreError, append_decision, enqueue};

/// The job kind that re-points what [`merge_people`] leaves behind.
pub const REWRITE_JOB_KIND: &str = "person.merge.rewrite";

/// Rows re-pointed per statement by the rewrite.
pub const REWRITE_BATCH: i64 = 1000;

/// Person-scoped columns that name a merged person and are re-pointed in batches rather than
/// in the merge transaction, with the columns that identify a row within its workspace. The
/// event table is partitioned, so its rows are found by `(occurred_at, id)`.
const REWRITE_TARGETS: &[(&str, &str, &[&str])] = &[
    ("identity_conflicts", "person_id", &["id"]),
    ("identity_conflicts", "other_person_id", &["id"]),
    ("events", "person_id", &["occurred_at", "id"]),
    ("event_dedupe", "person_id", &["message_id"]),
    ("ingest_rejects", "person_id", &["id"]),
];

/// The person that currently stands for `person`: the survivor when it was merged, else
/// itself. `None` when the workspace has no such person. Merges are compressed, so one hop is
/// the whole chain.
pub async fn current_person<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Merges each of `losers` into `survivor`, all locked by the caller. For each loser, in the
/// caller's transaction: its identities and ledger rows move, it points at the survivor, a
/// provenance row and a `person.merge` decision are written and the rewrite job is enqueued.
/// Afterwards the survivor's traits are recomputed from every contribution it now holds.
pub(super) async fn merge_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    survivor: Uuid,
    losers: &[Uuid],
    trace: &TraceContext,
) -> Result<(), StoreError> {
    // A trait writer: the loser's traits are emptied below, so its slots are emptied with them.
    let slots = lock_and_read_writer_slots(connection, workspace).await?;
    lock_lists_for_merge(connection, workspace, losers).await?;
    // Both sides are marked before anything touches the segments. The next pass then sees the
    // loser as merged, so a row that a concurrent full refresh opens for it after this merge
    // closed its rows is closed by that pass. Marking first also keeps a pass that holds a
    // mark from waiting on a segment row this merge holds.
    let mut marked = losers.to_vec();
    marked.push(survivor);
    mark_people(
        connection,
        workspace,
        &marked,
        ChangeClasses::MERGE,
        trace,
        None,
    )
    .await?;
    // The segments the losers are open members of are locked in the pass's order before any of
    // their rows is written, so this merge and a pass can never wait on each other.
    crate::segments::lock_segments_of_people(connection, workspace, losers).await?;
    for &loser in losers {
        let mut identity_ids: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE identities SET person_id = $3 \
             WHERE workspace_id = $1 AND person_id = $2 RETURNING id",
        )
        .bind(workspace.as_uuid())
        .bind(loser)
        .bind(survivor)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let mut record_ids: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE source_records SET person_id = $3 \
             WHERE workspace_id = $1 AND person_id = $2 RETURNING id",
        )
        .bind(workspace.as_uuid())
        .bind(loser)
        .bind(survivor)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        identity_ids.sort();
        record_ids.sort();
        move_on_merge(connection, workspace, loser, survivor, trace).await?;
        crate::segments::close_on_merge(connection, workspace, loser, trace).await?;
        // The loser keeps no traits: reads follow merged_into to the survivor's.
        let reset = format!(
            "UPDATE people SET merged_into = $3, traits = '{{}}'::jsonb, \
                trait_sources = '{{}}'::jsonb, updated_at = now(){} \
             WHERE workspace_id = $1 AND id = $2",
            slot_nulls_sql(&slots)
        );
        sqlx::query(sqlx::AssertSqlSafe(reset))
            .bind(workspace.as_uuid())
            .bind(loser)
            .bind(survivor)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        sqlx::query(
            "UPDATE people SET merged_into = $3 WHERE workspace_id = $1 AND merged_into = $2",
        )
        .bind(workspace.as_uuid())
        .bind(loser)
        .bind(survivor)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;

        let merge_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO person_merges \
                (id, workspace_id, survivor_person_id, merged_person_id, identity_ids, \
                 source_record_ids, trace_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(merge_id)
        .bind(workspace.as_uuid())
        .bind(survivor)
        .bind(loser)
        .bind(&identity_ids)
        .bind(&record_ids)
        .bind(trace.trace_id.as_uuid())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        consent::move_on_merge(connection, workspace, loser, survivor, trace).await?;
        record_decision(
            connection,
            workspace,
            trace,
            "person.merge",
            "merged",
            None,
            vec![
                ("person", survivor),
                ("survivor", survivor),
                ("merged", loser),
                ("merge", merge_id),
            ],
        )
        .await?;
        let mut job = NewJob::new(REWRITE_JOB_KIND, trace.clone());
        job.workspace_id = Some(workspace);
        job.payload = serde_json::json!({ "workspace_id": workspace, "merged_person_id": loser });
        enqueue(&mut *connection, &job).await?;
    }

    // What the merged-away people were in the middle of ends with them: the survivor's own
    // enrollments are not touched, and a merged-away person is never enrolled again (an enroll
    // follows `merged_into`). The enrollments are the last rows of the lock order (flow row,
    // people in id order, enrollments), after the people the caller locked.
    crate::flows::exit_people(connection, workspace, losers, "merge", trace).await?;
    // The losers' flow entries join the survivor's here, with the group locked, and not in the
    // rewrite job: until the job ran, a `once` flow (or `always` inside its minimum interval)
    // would have enrolled the survivor again. Entries are summed and the later entry kept; the
    // statement deletes the losers' rows, so a person who is merged again has none left to fold.
    sqlx::query(
        "WITH moved AS ( \
             DELETE FROM flow_entries WHERE workspace_id = $1 AND person_id = ANY($2) \
             RETURNING flow_id, entries, last_entered_at), \
         summed AS ( \
             SELECT flow_id, sum(entries)::integer AS entries, max(last_entered_at) AS last_entered_at \
             FROM moved GROUP BY flow_id) \
         INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, last_entered_at) \
         SELECT $1, flow_id, $3, entries, last_entered_at FROM summed \
         ON CONFLICT (flow_id, person_id) DO UPDATE SET \
             entries = flow_entries.entries + EXCLUDED.entries, \
             last_entered_at = GREATEST(flow_entries.last_entered_at, EXCLUDED.last_entered_at)",
    )
    .bind(workspace.as_uuid())
    .bind(losers)
    .bind(survivor)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    // Conflicts between the people that are now one person have nothing left to review.
    let mut group = losers.to_vec();
    group.push(survivor);
    sqlx::query(
        "UPDATE identity_conflicts SET resolved_at = now(), resolution = 'merged' \
         WHERE workspace_id = $1 AND resolved_at IS NULL \
           AND person_id = ANY($2) AND other_person_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(&group)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT jsonb_object_keys(traits) FROM source_records \
         WHERE workspace_id = $1 AND person_id = $2 AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(survivor)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if recompute(connection, workspace, survivor, &keys).await? {
        record_decision(
            connection,
            workspace,
            trace,
            "trait.change",
            "changed",
            Some("merged"),
            vec![("person", survivor)],
        )
        .await?;
    }
    Ok(())
}

/// Closes the open conflicts naming `merged` in `column` whose re-pointed copy would match an
/// open conflict that already exists: one open conflict per person pair and source record.
async fn close_conflicts_the_move_would_duplicate(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    column: &str,
    merged: Uuid,
    survivor: Uuid,
) -> Result<(), StoreError> {
    // The column is one of the constants above, never input.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE identity_conflicts c SET resolved_at = now(), resolution = 'merged' \
         WHERE c.workspace_id = $1 AND c.resolved_at IS NULL AND c.{column} = $2 \
           AND EXISTS (SELECT 1 FROM identity_conflicts o \
                       WHERE o.workspace_id = c.workspace_id AND o.resolved_at IS NULL \
                         AND o.id <> c.id \
                         AND o.person_id = {person} AND o.other_person_id = {other} \
                         AND COALESCE(o.source_record_id, '00000000-0000-0000-0000-000000000000'::uuid) \
                           = COALESCE(c.source_record_id, '00000000-0000-0000-0000-000000000000'::uuid) \
                         AND COALESCE(o.identity_id, '00000000-0000-0000-0000-000000000000'::uuid) \
                           = COALESCE(c.identity_id, '00000000-0000-0000-0000-000000000000'::uuid))",
        person = if column == "person_id" { "$3" } else { "c.person_id" },
        other = if column == "other_person_id" { "$3" } else { "c.other_person_id" },
    )))
    .bind(workspace.as_uuid())
    .bind(merged)
    .bind(survivor)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Rounds [`lock_merged_and_survivor`] re-reads the survivor before it gives up as a race.
const LOCK_ROUNDS: usize = 4;

/// Locks `merged` and the person it was merged into `FOR KEY SHARE`, in id order, and returns
/// the survivor; `None` when the person is gone or is not merged. An erasure of the group locks
/// both `FOR UPDATE`, so it waits for the transaction that holds these locks, and this waits
/// for an erasure already under way and then finds the rows gone.
///
/// The survivor is read before it is locked, and a merge of the survivor can commit in between
/// and move `merged_into` (a merge locks the group `FOR UPDATE`, but only once it runs). So the
/// locked read is checked against the survivor that was locked: when they differ the locks of
/// the round are released with a savepoint rollback and the next round locks the new pair in id
/// order. After [`LOCK_ROUNDS`] rounds it is a retryable race and the job runs again.
async fn lock_merged_and_survivor(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    merged: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    for _ in 0..LOCK_ROUNDS {
        let survivor: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(merged)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        let Some(Some(survivor)) = survivor else {
            return Ok(None);
        };
        sqlx::query("SAVEPOINT rewrite_group")
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        let locked: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE workspace_id = $1 AND id = ANY($2) \
             ORDER BY id FOR KEY SHARE",
        )
        .bind(workspace.as_uuid())
        .bind(vec![merged, survivor])
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        // A locking read hands back the row version it locked, and a `KEY SHARE` lock does not
        // follow a committed update that left the key alone (`merged_into` is not part of it), so
        // it can be the version from before a merge of the survivor. A plain read in a new
        // statement sees what committed. With the survivor locked, no further merge of it can
        // start (it locks the group `FOR UPDATE` first), so the answer holds for this round.
        let merged_into: Option<Option<Uuid>> = sqlx::query_scalar(
            "SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace.as_uuid())
        .bind(merged)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        // `Some(result)` ends the loop: `None` when the erasure of the group committed first or
        // the person is no longer merged, the survivor when it is the one that was locked.
        let settled = match merged_into {
            None | Some(None) => Some(None),
            Some(Some(now)) if now == survivor && locked.len() == 2 => Some(Some(survivor)),
            Some(Some(_)) => None,
        };
        let end = if settled.is_some() {
            "RELEASE SAVEPOINT rewrite_group"
        } else {
            "ROLLBACK TO SAVEPOINT rewrite_group"
        };
        sqlx::query(end)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        if let Some(result) = settled {
            return Ok(result);
        }
    }
    Err(StoreError::retryable_race())
}

/// Re-points what still names `merged` to the person it was merged into, in batches of
/// [`REWRITE_BATCH`] rows per statement, on `connection`. Returns how many rows moved; a
/// second run moves none. Conflicts that end up between the survivor and itself are closed
/// as merged. Locks the person and the survivor first ([`lock_merged_and_survivor`]), so an
/// erasure of the group cannot commit between the read and the decision; if it committed
/// first, nothing is written and the result is 0. Appends a `person.merge.rewrite` decision
/// once the person is merged and every batch is done; a person that was not merged is left
/// alone.
///
/// The flow tables, one by one:
/// - `flow_entries` is **folded** into the survivor's row for the same flow in the merge
///   transaction ([`merge_people`]), not here: the two were one human, so a `once` flow that
///   either entered stays entered from the moment of the merge. The rewrite writes no flow row
///   of that table.
/// - `enrollments` is **kept on the old id**. The in-flight ones were exited in the merge
///   transaction (reason `merge`) and later enrolls follow `merged_into`, so what is left are
///   ended enrollments, which are history of the person that was enrolled, and re-pointing them
///   could collide with the survivor's on the in-flight and occurrence unique indexes. They go
///   with the group when it is erased.
/// - `messages` is **kept on the old id**, for the same reason: a message is history of the person it
///   was sent to, and the log reads through `person_merges` ([`crate::list_person_messages`]) so
///   the survivor sees it. A `delivery.send` still queued names the old id and is dropped when the
///   enrollment it belongs to has exited (the merge exits the loser's in-flight enrollments).
/// - `step_executions` has no person column and follows its enrollment.
/// - `enrollment_waits` is **deleted** in the merge transaction with the enrollment that held it:
///   a wait left on the old id would never match an event, which resolves to the survivor, while
///   its timer could still run.
pub async fn rewrite_merged_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    merged: Uuid,
    trace: &TraceContext,
    job: Option<(Uuid, i32)>,
) -> Result<u64, StoreError> {
    let Some(survivor) = lock_merged_and_survivor(connection, workspace, merged).await? else {
        return Ok(0);
    };
    let mut total = 0;
    for (table, column, key) in REWRITE_TARGETS {
        if *table == "identity_conflicts" {
            close_conflicts_the_move_would_duplicate(
                connection, workspace, column, merged, survivor,
            )
            .await?;
        }
        let columns = key.join(", ");
        let same_row: String = key
            .iter()
            .map(|name| format!(" AND x.{name} = d.{name}"))
            .collect();
        loop {
            // Names come from the constant above, never from input.
            let moved = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} x SET {column} = $3 FROM \
                    (SELECT workspace_id, {columns} FROM {table} \
                     WHERE workspace_id = $1 AND {column} = $2 LIMIT $4) d \
                 WHERE x.workspace_id = d.workspace_id{same_row}"
            )))
            .bind(workspace.as_uuid())
            .bind(merged)
            .bind(survivor)
            .bind(REWRITE_BATCH)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?
            .rows_affected();
            total += moved;
            if i64::try_from(moved).is_ok_and(|moved| moved < REWRITE_BATCH) {
                break;
            }
        }
    }
    // A conflict between two people that have both been merged into the survivor is now
    // between one person and itself.
    sqlx::query(
        "UPDATE identity_conflicts SET resolved_at = now(), resolution = 'merged' \
         WHERE workspace_id = $1 AND resolved_at IS NULL \
           AND person_id = $2 AND other_person_id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(survivor)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            action: "person.merge.rewrite",
            outcome: "completed",
            reason: None,
            subject_ids: vec![
                ("person", survivor),
                ("survivor", survivor),
                ("merged", merged),
            ],
            job,
        },
    )
    .await?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{
        ProfileOutcome, ProfileRecord, apply_profile_batch, find_person, list_identities,
        list_identity_conflicts,
    };
    use crate::{create_workspace, list_trace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn record(
        source: &str,
        key: &str,
        identifiers: &[(&str, &str)],
        traits: Value,
    ) -> ProfileRecord {
        ProfileRecord {
            source: source.into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: traits.as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("test")),
        }
    }

    fn person_of(outcome: &ProfileOutcome) -> Uuid {
        match outcome {
            ProfileOutcome::Applied { person_id, .. }
            | ProfileOutcome::Unchanged { person_id, .. } => *person_id,
            ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
        }
    }

    async fn apply_one(pool: &PgPool, ws: WorkspaceId, record: ProfileRecord) -> Uuid {
        let outcomes = apply_profile_batch(pool, ws, &[record]).await.unwrap();
        person_of(&outcomes[0])
    }

    async fn merged_into(pool: &PgPool, person: Uuid) -> Option<Uuid> {
        sqlx::query_scalar("SELECT merged_into FROM people WHERE id = $1")
            .bind(person)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_anonymous_person_is_merged_into_the_identified_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let identified = apply_one(
            &pool,
            ws,
            record(
                "crm",
                "1",
                &[("external:user_id", "u")],
                json!({"plan": "pro"}),
            ),
        )
        .await;
        let anonymous = apply_one(
            &pool,
            ws,
            record(
                "web",
                "2",
                &[("anonymous_id", "a")],
                json!({"city": "Aarhus"}),
            ),
        )
        .await;
        assert_ne!(identified, anonymous);
        let anonymous_identity: Uuid =
            sqlx::query_scalar("SELECT id FROM identities WHERE kind = 'anonymous_id'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let anonymous_record: Uuid =
            sqlx::query_scalar("SELECT id FROM source_records WHERE record_key = '2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // An open conflict between the two stops needing review once they are one person.
        sqlx::query(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
             VALUES ($1, $2, $3, $4, 'anonymous_id', $5, 'merge_deferred')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(identified)
        .bind(anonymous)
        .bind(anonymous_record)
        .execute(&pool)
        .await
        .unwrap();
        let both = record(
            "app",
            "3",
            &[("external:user_id", "u"), ("anonymous_id", "a")],
            json!({}),
        );

        let survivor = apply_one(&pool, ws, both.clone()).await;

        assert_eq!(survivor, identified);
        assert_eq!(merged_into(&pool, anonymous).await, Some(identified));
        assert_eq!(merged_into(&pool, identified).await, None);
        let kinds: Vec<String> = list_identities(&pool, ws, identified)
            .await
            .unwrap()
            .into_iter()
            .map(|identity| identity.kind)
            .collect();
        assert_eq!(kinds, ["anonymous_id", "external:user_id"]);
        let left_behind: i64 =
            sqlx::query_scalar("SELECT count(*) FROM source_records WHERE person_id = $1")
                .bind(anonymous)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left_behind, 0);
        let survivor_person = find_person(&pool, ws, identified).await.unwrap().unwrap();
        assert_eq!(
            survivor_person.traits,
            json!({"plan": "pro", "city": "Aarhus"})
        );
        let through_loser = find_person(&pool, ws, anonymous).await.unwrap().unwrap();
        assert_eq!(through_loser.id, identified);
        let loser_traits: Value = sqlx::query_scalar("SELECT traits FROM people WHERE id = $1")
            .bind(anonymous)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(loser_traits, json!({}));

        let (identity_ids, record_ids, trace_id): (Vec<Uuid>, Vec<Uuid>, Uuid) = sqlx::query_as(
            "SELECT identity_ids, source_record_ids, trace_id FROM person_merges \
             WHERE survivor_person_id = $1 AND merged_person_id = $2",
        )
        .bind(identified)
        .bind(anonymous)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(identity_ids, [anonymous_identity]);
        assert_eq!(record_ids, [anonymous_record]);
        assert_eq!(trace_id, both.trace.trace_id.as_uuid());
        let (decisions, _) = list_trace(&pool, ws, both.trace.trace_id, 20)
            .await
            .unwrap();
        let merge = decisions
            .iter()
            .find(|d| d.action == "person.merge")
            .expect("a person.merge decision");
        assert_eq!(merge.outcome, "merged");
        assert_eq!(merge.subject_ids["survivor"], identified.to_string());
        assert_eq!(merge.subject_ids["merged"], anonymous.to_string());
        assert!(merge.subject_ids["merge"].is_string());
        let payload: Value = sqlx::query_scalar(
            "SELECT payload FROM jobs WHERE kind = 'person.merge.rewrite' AND trace_id = $1",
        )
        .bind(both.trace.trace_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            payload,
            json!({"workspace_id": ws, "merged_person_id": anonymous})
        );
        let conflicts = list_identity_conflicts(&pool, ws, identified)
            .await
            .unwrap();
        assert_eq!(conflicts[0].resolution.as_deref(), Some("merged"));
        assert!(conflicts[0].resolved_at.is_some());
        // Both sides are marked in the merge class on the merge's trace, so the next pass
        // closes any row a concurrent refresh opened for the loser.
        let marks: Vec<(Uuid, bool, Option<Uuid>)> = sqlx::query_as(
            "SELECT person_id, (classes & 32) = 32, trace_id FROM person_changes \
             WHERE person_id = ANY($1) ORDER BY person_id",
        )
        .bind([anonymous, identified])
        .fetch_all(&pool)
        .await
        .unwrap();
        let mut expected = vec![
            (anonymous, true, Some(both.trace.trace_id.as_uuid())),
            (identified, true, Some(both.trace.trace_id.as_uuid())),
        ];
        expected.sort();
        assert_eq!(marks, expected);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_the_max_rule_forbids_records_a_max_per_person_conflict(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let emails: Vec<String> = (0..10).map(|n| format!("e{n}@example.com")).collect();
        let mut full = record("crm", "1", &[], json!({}));
        full.identifiers = emails
            .iter()
            .map(|email| ("email".to_owned(), email.clone()))
            .collect();
        let full_person = apply_one(&pool, ws, full).await;
        let other = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("phone", "+4512345678")], json!({})),
        )
        .await;

        let attached = apply_one(
            &pool,
            ws,
            record(
                "web",
                "3",
                &[
                    ("email", "e0@example.com"),
                    ("phone", "+4512345678"),
                    ("email", "extra@example.com"),
                ],
                json!({}),
            ),
        )
        .await;

        assert_eq!(attached, full_person);
        assert_eq!(merged_into(&pool, other).await, None);
        let conflicts = list_identity_conflicts(&pool, ws, full_person)
            .await
            .unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].reason, "max_per_person");
        assert_eq!(conflicts[0].other_person_id, other);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn chains_are_compressed_and_current_person_follows_one_hop(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let root = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:crm", "1")], json!({})),
        )
        .await;
        let middle = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("email", "m@example.com")], json!({})),
        )
        .await;
        let leaf = apply_one(
            &pool,
            ws,
            record("crm", "3", &[("email", "l@example.com")], json!({})),
        )
        .await;

        apply_one(
            &pool,
            ws,
            record(
                "web",
                "4",
                &[("email", "m@example.com"), ("email", "l@example.com")],
                json!({}),
            ),
        )
        .await;
        assert_eq!(merged_into(&pool, leaf).await, Some(middle));
        apply_one(
            &pool,
            ws,
            record(
                "web",
                "5",
                &[("external:crm", "1"), ("email", "m@example.com")],
                json!({}),
            ),
        )
        .await;

        assert_eq!(merged_into(&pool, middle).await, Some(root));
        assert_eq!(merged_into(&pool, leaf).await, Some(root));
        assert_eq!(current_person(&pool, ws, leaf).await.unwrap(), Some(root));
        assert_eq!(current_person(&pool, ws, root).await.unwrap(), Some(root));
        assert_eq!(
            current_person(&pool, ws, Uuid::now_v7()).await.unwrap(),
            None
        );
        let merges: i64 = sqlx::query_scalar("SELECT count(*) FROM person_merges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(merges, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_whose_own_person_is_merged_away_ends_on_the_survivor(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let identified = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:user_id", "u")], json!({})),
        )
        .await;
        let anonymous = apply_one(
            &pool,
            ws,
            record(
                "web",
                "2",
                &[("anonymous_id", "a")],
                json!({"city": "Aarhus"}),
            ),
        )
        .await;

        let again = apply_one(
            &pool,
            ws,
            record(
                "web",
                "2",
                &[("anonymous_id", "a"), ("external:user_id", "u")],
                json!({"city": "Odense"}),
            ),
        )
        .await;

        assert_eq!(again, identified);
        assert_eq!(merged_into(&pool, anonymous).await, Some(identified));
        let owner: Uuid =
            sqlx::query_scalar("SELECT person_id FROM source_records WHERE record_key = '2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(owner, identified);
        let person = find_person(&pool, ws, identified).await.unwrap().unwrap();
        assert_eq!(person.traits, json!({"city": "Odense"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn overlapping_merges_finish_without_deadlock_and_leave_one_survivor(pool: PgPool) {
        for round in 0..5 {
            let ws = workspace(&pool, &format!("ws{round}")).await;
            for (key, email) in [("1", "a"), ("2", "b"), ("3", "c")] {
                let identifier = format!("{email}@example.com");
                apply_one(
                    &pool,
                    ws,
                    record("crm", key, &[("email", &identifier)], json!({})),
                )
                .await;
            }
            let first = [record(
                "web",
                "x",
                &[("email", "a@example.com"), ("email", "b@example.com")],
                json!({}),
            )];
            let second = [record(
                "web",
                "y",
                &[("email", "b@example.com"), ("email", "c@example.com")],
                json!({}),
            )];

            let (one, two) = tokio::join!(
                apply_profile_batch(&pool, ws, &first),
                apply_profile_batch(&pool, ws, &second),
            );

            let (one, two) = (one.unwrap(), two.unwrap());
            // Whichever transaction commits first returns a person the other may merge away.
            let survivor = current_person(&pool, ws, person_of(&one[0]))
                .await
                .unwrap()
                .unwrap();
            let other = current_person(&pool, ws, person_of(&two[0]))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(survivor, other);
            let survivors: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM people WHERE workspace_id = $1 AND merged_into IS NULL",
            )
            .bind(ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(survivors, 1);
            let identities: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM identities WHERE workspace_id = $1 AND person_id = $2",
            )
            .bind(ws.as_uuid())
            .bind(survivor)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(identities, 3);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rewrite_re_points_in_batches_and_a_second_run_moves_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:crm", "1")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("email", "l@example.com")], json!({})),
        )
        .await;
        apply_one(
            &pool,
            ws,
            record("crm", "3", &[("email", "t@example.com")], json!({})),
        )
        .await;
        let source_record: Uuid =
            sqlx::query_scalar("SELECT id FROM source_records WHERE record_key = '3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // 2,500 conflicts name the loser in each column, so each column needs three batches.
        for (column, other) in [
            ("person_id", "other_person_id"),
            ("other_person_id", "person_id"),
        ] {
            // One open conflict per person pair and source record, so each row names its own
            // other person.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "WITH others AS (INSERT INTO people (id, workspace_id) \
                                 SELECT gen_random_uuid(), $1 FROM generate_series(1, 2500) \
                                 RETURNING id) \
                 INSERT INTO identity_conflicts \
                    (id, workspace_id, {column}, {other}, kind, source_record_id, reason) \
                 SELECT gen_random_uuid(), $1, $2, others.id, 'email', $3, 'unique_kind' \
                 FROM others"
            )))
            .bind(ws.as_uuid())
            .bind(loser)
            .bind(source_record)
            .execute(&pool)
            .await
            .unwrap();
        }
        apply_one(
            &pool,
            ws,
            record(
                "web",
                "4",
                &[("external:crm", "1"), ("email", "l@example.com")],
                json!({}),
            ),
        )
        .await;
        assert_eq!(merged_into(&pool, loser).await, Some(survivor));
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        let first = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        let second = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        assert_eq!(first, 5000);
        assert_eq!(second, 0);
        let naming = "SELECT count(*) FROM identity_conflicts \
                      WHERE person_id = $1 OR other_person_id = $1";
        let on_loser: i64 = sqlx::query_scalar(naming)
            .bind(loser)
            .fetch_one(&pool)
            .await
            .unwrap();
        let on_survivor: i64 = sqlx::query_scalar(naming)
            .bind(survivor)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((on_loser, on_survivor), (0, 5000));
        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        assert!(
            decisions
                .iter()
                .all(|d| d.action == "person.merge.rewrite" && d.outcome == "completed")
        );
        assert!(
            decisions
                .iter()
                .all(|d| d.subject_ids["person"] == survivor.to_string())
        );
    }

    async fn event_of(
        pool: &PgPool,
        ws: WorkspaceId,
        email: &str,
        name: &str,
        message_id: &str,
    ) -> crate::EventOutcome {
        let key = zirv_flow_core::HmacRoot::bootstrap_from(&zirv_flow_core::MasterKey::generate())
            .hmac_key(zirv_flow_core::HmacPurpose::Redaction);
        let event = crate::EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), email.into())],
            name: name.into(),
            properties: json!({}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: time::macros::datetime!(2026-01-01 00:00 UTC),
            message_id: Some(message_id.into()),
            trace: TraceContext::root(Cause::api("event")),
        };
        crate::apply_event_batch(pool, ws, &key, &[event])
            .await
            .unwrap()
            .remove(0)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rewrite_re_points_events_dedupe_and_rejects_and_a_second_run_moves_nothing(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:crm", "1")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("email", "l@example.com")], json!({})),
        )
        .await;
        for index in 0..3 {
            let outcome = event_of(&pool, ws, "l@example.com", "seen", &format!("m-{index}")).await;
            assert!(
                matches!(outcome, crate::EventOutcome::Accepted { person_id, .. } if person_id == loser)
            );
        }
        crate::set_event_policy(
            &pool,
            ws,
            &crate::EventPolicy {
                unplanned: zirv_flow_core::events::UnplannedPolicy::Reject,
                ..crate::EventPolicy::default()
            },
        )
        .await
        .unwrap();
        let rejected = event_of(&pool, ws, "l@example.com", "unknown", "m-reject").await;
        assert!(matches!(rejected, crate::EventOutcome::Rejected { .. }));
        apply_one(
            &pool,
            ws,
            record(
                "web",
                "3",
                &[("external:crm", "1"), ("email", "l@example.com")],
                json!({}),
            ),
        )
        .await;
        assert_eq!(merged_into(&pool, loser).await, Some(survivor));
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        let first = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        let second = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        assert_eq!((first, second), (7, 0));
        for table in ["events", "event_dedupe", "ingest_rejects"] {
            let on = |person: Uuid| {
                let pool = pool.clone();
                async move {
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                        "SELECT count(*) FROM {table} WHERE person_id = $1"
                    )))
                    .bind(person)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                }
            };
            assert_eq!(on(loser).await, 0, "{table}");
            assert!(on(survivor).await >= 1, "{table}");
        }
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE person_id = $1")
            .bind(survivor)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_conflict_the_move_would_duplicate_is_closed_instead_of_failing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:crm", "1")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("email", "l@example.com")], json!({})),
        )
        .await;
        let other = apply_one(
            &pool,
            ws,
            record("crm", "3", &[("email", "o@example.com")], json!({})),
        )
        .await;
        for (person, against) in [
            (loser, other),
            (survivor, other),
            (other, loser),
            (other, survivor),
        ] {
            sqlx::query(
                "INSERT INTO identity_conflicts \
                    (id, workspace_id, person_id, other_person_id, kind, reason) \
                 VALUES (gen_random_uuid(), $1, $2, $3, 'email', 'unique_kind')",
            )
            .bind(ws.as_uuid())
            .bind(person)
            .bind(against)
            .execute(&pool)
            .await
            .unwrap();
        }
        apply_one(
            &pool,
            ws,
            record(
                "web",
                "4",
                &[("external:crm", "1"), ("email", "l@example.com")],
                json!({}),
            ),
        )
        .await;
        assert_eq!(merged_into(&pool, loser).await, Some(survivor));
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM identity_conflicts WHERE resolved_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        let closed: Vec<String> = sqlx::query_scalar(
            "SELECT resolution FROM identity_conflicts WHERE resolved_at IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            open, 2,
            "one per direction between the survivor and the other person"
        );
        assert_eq!(closed, ["merged", "merged"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_that_was_not_merged_is_left_alone(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("email", "a@example.com")], json!({})),
        )
        .await;
        let trace = TraceContext::root(Cause::api("rewrite"));
        let mut tx = crate::begin(&pool).await.unwrap();

        let moved = rewrite_merged_person(&mut tx, ws, person, &trace, None)
            .await
            .unwrap();
        let unknown = rewrite_merged_person(&mut tx, ws, Uuid::now_v7(), &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        assert_eq!((moved, unknown), (0, 0));
        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        assert!(decisions.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_conflict_that_becomes_a_person_with_itself_is_closed_by_the_rewrite(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:crm", "1")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("crm", "2", &[("email", "l@example.com")], json!({})),
        )
        .await;
        let other = apply_one(
            &pool,
            ws,
            record("crm", "3", &[("email", "x@example.com")], json!({})),
        )
        .await;
        let source_record: Uuid =
            sqlx::query_scalar("SELECT id FROM source_records WHERE record_key = '3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
             VALUES ($1, $2, $3, $4, 'email', $5, 'unique_kind')",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(loser)
        .bind(other)
        .bind(source_record)
        .execute(&pool)
        .await
        .unwrap();
        // Both merge into the survivor before either rewrite job has run.
        for (key, email) in [("4", "l@example.com"), ("5", "x@example.com")] {
            apply_one(
                &pool,
                ws,
                record(
                    "web",
                    key,
                    &[("external:crm", "1"), ("email", email)],
                    json!({}),
                ),
            )
            .await;
        }
        assert_eq!(merged_into(&pool, other).await, Some(survivor));
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        rewrite_merged_person(&mut tx, ws, other, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        let (person, other_person, resolution, resolved): (Uuid, Uuid, Option<String>, bool) =
            sqlx::query_as(
                "SELECT person_id, other_person_id, resolution, resolved_at IS NOT NULL \
                 FROM identity_conflicts",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((person, other_person), (survivor, survivor));
        assert!(resolved);
        assert_eq!(resolution.as_deref(), Some("merged"));
    }

    /// One enrollment of `person` in a flow of its own, in `state`, with a wait when it sleeps.
    async fn enroll_in(pool: &PgPool, ws: WorkspaceId, person: Uuid, state: &str) -> Uuid {
        use crate::flows::tests_support::{create, publish, waiting_enrollment};
        let flow = create(pool, ws, &format!("Flow {state} {person}")).await;
        publish(pool, ws, flow.id).await.unwrap();
        let enrollment = waiting_enrollment(pool, ws, flow.id, person, "seen").await;
        sqlx::query(
            "UPDATE enrollments SET state = $2::text, \n                ended_at = CASE WHEN $2::text IN ('completed', 'exited') THEN now() END, \n                resume_input = CASE WHEN $2::text = 'held' THEN '{}'::jsonb END, \n                error = CASE WHEN $2::text = 'failed' THEN 'boom' END, \n                failed_node = CASE WHEN $2::text = 'failed' THEN 'draft' END \n             WHERE id = $1",
        )
            .bind(enrollment)
            .bind(state)
            .execute(pool)
            .await
            .unwrap();
        if state == "completed" {
            sqlx::query("DELETE FROM enrollment_waits WHERE enrollment_id = $1")
                .bind(enrollment)
                .execute(pool)
                .await
                .unwrap();
        }
        enrollment
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_exits_the_merged_away_enrollments_with_merge_and_leaves_the_survivors(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:user_id", "u")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("web", "2", &[("anonymous_id", "a")], json!({})),
        )
        .await;
        let mut in_flight = Vec::new();
        for state in ["active", "waiting", "held", "failed"] {
            in_flight.push(enroll_in(&pool, ws, loser, state).await);
        }
        let completed = enroll_in(&pool, ws, loser, "completed").await;
        let kept = enroll_in(&pool, ws, survivor, "waiting").await;
        let both = record(
            "app",
            "3",
            &[("external:user_id", "u"), ("anonymous_id", "a")],
            json!({}),
        );

        let merged = apply_one(&pool, ws, both.clone()).await;

        assert_eq!(merged, survivor);
        assert_eq!(merged_into(&pool, loser).await, Some(survivor));
        let state_of = |id: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_as::<_, (String, Option<String>, i64)>(
                    "SELECT state, exit_reason, state_version FROM enrollments WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        for id in &in_flight {
            let (state, reason, version) = state_of(*id).await;
            assert_eq!(
                (state.as_str(), reason.as_deref(), version),
                ("exited", Some("merge"), 4)
            );
        }
        assert_eq!(state_of(completed).await.0, "completed");
        assert_eq!(state_of(kept).await, ("waiting".to_owned(), None, 3));
        let waits: Vec<Uuid> = sqlx::query_scalar("SELECT enrollment_id FROM enrollment_waits")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(waits, [kept], "only the survivor's wait is left");
        let (decisions, _) = list_trace(&pool, ws, both.trace.trace_id, 50)
            .await
            .unwrap();
        let exits: Vec<_> = decisions
            .iter()
            .filter(|d| d.action == "flow.enrollment")
            .collect();
        assert_eq!(exits.len(), in_flight.len(), "one decision per exit");
        for decision in exits {
            assert_eq!(decision.outcome, "exited");
            assert_eq!(decision.reason.as_deref(), Some("merge"));
            assert_eq!(decision.subject_ids["person"], loser.to_string());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_folds_flow_entries_into_the_survivor_and_the_rewrite_keeps_ended_enrollments(
        pool: PgPool,
    ) {
        use crate::flows::tests_support::{create, publish};
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:user_id", "u")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("web", "2", &[("anonymous_id", "a")], json!({})),
        )
        .await;
        let both_entered = create(&pool, ws, "Both").await;
        let loser_entered = create(&pool, ws, "Loser").await;
        for flow in [both_entered.id, loser_entered.id] {
            publish(&pool, ws, flow).await.unwrap();
        }
        for (flow, person, entries, at) in [
            (both_entered.id, survivor, 1, "2030-01-01T00:00:00Z"),
            (both_entered.id, loser, 2, "2030-03-01T00:00:00Z"),
            (loser_entered.id, loser, 1, "2030-02-01T00:00:00Z"),
        ] {
            sqlx::query(
                "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, \
                    last_entered_at) VALUES ($1, $2, $3, $4, $5::timestamptz)",
            )
            .bind(ws.as_uuid())
            .bind(flow)
            .bind(person)
            .bind(entries)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
        }
        // An ended enrollment of the loser: history, kept on the old id.
        let ended = enroll_in(&pool, ws, loser, "completed").await;
        apply_one(
            &pool,
            ws,
            record(
                "app",
                "3",
                &[("external:user_id", "u"), ("anonymous_id", "a")],
                json!({}),
            ),
        )
        .await;
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        let first = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        let second = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        let rows: Vec<(Uuid, Uuid, i32, bool)> = sqlx::query_as(
            "SELECT flow_id, person_id, entries, \
                    last_entered_at = '2030-03-01T00:00:00Z'::timestamptz \
             FROM flow_entries ORDER BY entries DESC",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (both_entered.id, survivor, 3, true),
                (loser_entered.id, survivor, 1, false)
            ]
        );
        assert_eq!(
            first, 0,
            "the merge folded the entries; the rewrite writes no flow row"
        );
        assert_eq!(second, 0, "a second run moves nothing");
        let kept: Uuid = sqlx::query_scalar("SELECT person_id FROM enrollments WHERE id = $1")
            .bind(ended)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, loser, "ended enrollments stay on the old id");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_folds_the_losers_flow_entries_before_the_rewrite_job_runs(pool: PgPool) {
        use crate::flows::tests_support::{create, publish};
        use crate::flows::{FlowPatch, update_flow_draft};
        use crate::{EnrollRequest, Occurrence, enroll};
        let ws = workspace(&pool, "one").await;
        let survivor = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("external:user_id", "u")], json!({})),
        )
        .await;
        let loser = apply_one(
            &pool,
            ws,
            record("web", "2", &[("anonymous_id", "a")], json!({})),
        )
        .await;
        let flow = create(&pool, ws, "Once").await;
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                settings: Some(json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}})),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish(&pool, ws, flow.id).await.unwrap();
        sqlx::query(
            "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, last_entered_at) \
             VALUES ($1, $2, $3, 1, now())",
        )
        .bind(ws.as_uuid())
        .bind(flow.id)
        .bind(loser)
        .execute(&pool)
        .await
        .unwrap();
        apply_one(
            &pool,
            ws,
            record(
                "app",
                "3",
                &[("external:user_id", "u"), ("anonymous_id", "a")],
                json!({}),
            ),
        )
        .await;

        // The rewrite job has not run: the merge transaction alone moved the entry.
        let mut tx = crate::begin(&pool).await.unwrap();
        let outcome = enroll(
            &mut tx,
            &EnrollRequest {
                workspace: ws,
                flow: flow.id,
                version: 1,
                people: &[survivor],
                occurrence: Occurrence {
                    trigger: "manual",
                    id: Uuid::now_v7(),
                },
                now: time::OffsetDateTime::now_utc(),
                trace: &TraceContext::root(Cause::api("enroll")),
            },
        )
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();

        assert!(outcome.enrolled.is_empty());
        assert_eq!(outcome.skipped, [(survivor, "already_entered")]);
    }

    /// Two people merged by a third record, with nothing to re-point: the rewrite of the merged
    /// one writes nothing but its decision. Returns `(workspace, survivor, merged)`.
    async fn a_merged_pair_with_nothing_to_move(pool: &PgPool) -> (WorkspaceId, Uuid, Uuid) {
        let ws = workspace(pool, "erased").await;
        let first = apply_one(
            pool,
            ws,
            record("a", "1", &[("email", "a@x.com")], json!({})),
        )
        .await;
        let second = apply_one(
            pool,
            ws,
            record("b", "1", &[("email", "b@x.com")], json!({})),
        )
        .await;
        let joined = apply_one(
            pool,
            ws,
            record(
                "c",
                "1",
                &[("email", "a@x.com"), ("email", "b@x.com")],
                json!({}),
            ),
        )
        .await;
        let merged = if first == joined { second } else { first };
        assert_eq!(merged_into(pool, merged).await, Some(joined));
        (ws, joined, merged)
    }

    /// The ids of the rows that name `ids`: decisions and jobs.
    async fn rows_naming(pool: &PgPool, ids: [Uuid; 2]) -> Vec<String> {
        let mut rows: Vec<String> = Vec::new();
        for id in ids {
            let text = id.to_string();
            let decisions: Vec<String> = sqlx::query_scalar(
                "SELECT 'decision ' || id::text FROM decision_records \
                 WHERE subject_ids::text LIKE '%' || $1 || '%'",
            )
            .bind(&text)
            .fetch_all(pool)
            .await
            .unwrap();
            let jobs: Vec<String> = sqlx::query_scalar(
                "SELECT 'job ' || id::text FROM jobs \
                 WHERE payload::text LIKE '%' || $1 || '%' OR unique_key LIKE '%' || $1 || '%'",
            )
            .bind(&text)
            .fetch_all(pool)
            .await
            .unwrap();
            rows.extend(decisions);
            rows.extend(jobs);
        }
        rows
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_waits_for_a_running_rewrite_and_leaves_no_decision_naming_the_group(
        pool: PgPool,
    ) {
        let (ws, survivor, merged) = a_merged_pair_with_nothing_to_move(&pool).await;
        let trace = TraceContext::root(Cause::api("rewrite"));

        // The rewrite has run and holds its transaction open.
        let mut rewrite = crate::begin(&pool).await.unwrap();
        let rewrite_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *rewrite)
            .await
            .unwrap();
        rewrite_merged_person(&mut rewrite, ws, merged, &trace, None)
            .await
            .unwrap();

        // The erasure of the group, on a second connection.
        let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
        let erasing = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                pid_sender.send(pid).unwrap();
                crate::delete_person(
                    &mut tx,
                    ws,
                    survivor,
                    crate::DeleteReason::Erasure,
                    &TraceContext::root(Cause::api("erase")),
                )
                .await
                .unwrap();
                crate::commit(tx).await.unwrap();
            })
        };
        let erasure_pid = pid_receiver.await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(erasure_pid)
                .bind(rewrite_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the erasure never waited for the rewrite"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        crate::commit(rewrite).await.unwrap();
        erasing.await.expect("the erasure finished");

        assert_eq!(
            rows_naming(&pool, [survivor, merged]).await,
            Vec::<String>::new()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rewrite_that_runs_after_an_erasure_wrote_nothing(pool: PgPool) {
        let (ws, survivor, merged) = a_merged_pair_with_nothing_to_move(&pool).await;
        let mut erase = crate::begin(&pool).await.unwrap();
        crate::delete_person(
            &mut erase,
            ws,
            survivor,
            crate::DeleteReason::Erasure,
            &TraceContext::root(Cause::api("erase")),
        )
        .await
        .unwrap();
        crate::commit(erase).await.unwrap();
        let trace = TraceContext::root(Cause::api("rewrite"));

        let mut tx = crate::begin(&pool).await.unwrap();
        let moved = rewrite_merged_person(&mut tx, ws, merged, &trace, None)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();

        assert_eq!(moved, 0);
        assert_eq!(
            rows_naming(&pool, [survivor, merged]).await,
            Vec::<String>::new()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rewrite_whose_survivor_was_merged_while_it_waited_re_points_to_the_new_survivor(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "relock").await;
        let other = apply_one(
            &pool,
            ws,
            record("crm", "1", &[("email", "o@example.com")], json!({})),
        )
        .await;
        let source_record: Uuid = sqlx::query_scalar("SELECT id FROM source_records LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let [survivor, loser, target] = [(); 3].map(|()| Uuid::now_v7());
        for id in [loser, survivor, target] {
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(id)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }
        let conflict: Uuid = sqlx::query_scalar(
            "INSERT INTO identity_conflicts \
                (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
             VALUES (gen_random_uuid(), $1, $2, $3, 'email', $4, 'unique_kind') RETURNING id",
        )
        .bind(ws.as_uuid())
        .bind(loser)
        .bind(other)
        .bind(source_record)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The loser was merged into the survivor, so a rewrite of it is due.
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();

        // A merge of the survivor into the target, held open: it locks the pair `FOR UPDATE` in
        // id order and moves `merged_into` of the survivor and of the loser (compressed).
        let mut merge = crate::begin(&pool).await.unwrap();
        let merge_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *merge)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM people WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![survivor, target])
            .fetch_all(&mut *merge)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = ANY($1)")
            .bind(vec![survivor, loser])
            .bind(target)
            .execute(&mut *merge)
            .await
            .unwrap();

        // The rewrite reads the survivor (still the old one) and then waits for the lock.
        let trace = TraceContext::root(Cause::api("rewrite"));
        let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
        let rewriting = {
            let pool = pool.clone();
            let trace = trace.clone();
            tokio::spawn(async move {
                let mut tx = crate::begin(&pool).await.unwrap();
                let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
                pid_sender.send(pid).unwrap();
                let moved = rewrite_merged_person(&mut tx, ws, loser, &trace, None)
                    .await
                    .unwrap();
                crate::commit(tx).await.unwrap();
                moved
            })
        };
        let rewrite_pid = pid_receiver.await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(rewrite_pid)
                .bind(merge_pid)
                .fetch_one(&pool)
                .await
                .unwrap();
            if blocked {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the rewrite never waited for the merge"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        crate::commit(merge).await.unwrap();
        let moved = rewriting.await.expect("the rewrite finished");

        assert_eq!(moved, 1);
        let person: Uuid =
            sqlx::query_scalar("SELECT person_id FROM identity_conflicts WHERE id = $1")
                .bind(conflict)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(person, target);
        let (decisions, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].subject_ids["survivor"], target.to_string());
        assert_eq!(decisions[0].subject_ids["merged"], loser.to_string());
    }
}
