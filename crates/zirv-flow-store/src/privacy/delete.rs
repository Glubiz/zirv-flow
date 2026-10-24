//! Person deletion: removes every row that refers to a person and to the people merged into
//! them. Deletion is not erasure: it adds no suppression and no ledger entry.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::decisions::ID_SUBJECTS;
use crate::{NewDecision, StoreError, append_decision};

/// Why a person is being deleted; recorded as the reason of the `person.deleted` decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteReason {
    /// An operator asked for the person to be deleted.
    Requested,
    /// An erasure request is being carried out.
    Erasure,
    /// The workspace orphan policy deleted a person left with nothing.
    Orphan,
}

impl DeleteReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Erasure => "erasure",
            Self::Orphan => "orphan",
        }
    }
}

/// Deletes the person and every person merged into them, with every row that refers to
/// them, in the caller's transaction. A person that was itself merged is replaced by their
/// survivor. Returns false when the workspace has no such person.
///
/// This function is the one list of person-scoped tables: every milestone that adds a table
/// with a person column extends it, and the erasure scan test fails until it does. The
/// decision it appends carries the reason and no person id, because the person's decisions
/// are deleted here.
pub async fn delete_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    reason: DeleteReason,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let Some(current) = current_of(connection, workspace, person).await? else {
        return Ok(false);
    };
    // The person and the people merged into them, locked in id order.
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM people WHERE workspace_id = $1 AND (id = $2 OR merged_into = $2) \
         ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(current)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if people.is_empty() {
        return Ok(false);
    }

    let enrollments: Vec<String> = sqlx::query_scalar(
        "SELECT id::text FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2)",
    )
    .bind(workspace.as_uuid())
    .bind(&people)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    remove_from_lists(connection, workspace, &people).await?;
    crate::segments::forget_people(connection, workspace, &people).await?;
    for statement in DELETIONS {
        sqlx::query(*statement)
            .bind(workspace.as_uuid())
            .bind(&people)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM decision_records d WHERE d.workspace_id = $1 AND EXISTS \
            (SELECT 1 FROM {ID_SUBJECTS} AND s.value = ANY($2::text[]))"
    )))
    .bind(workspace.as_uuid())
    .bind(people.iter().map(Uuid::to_string).collect::<Vec<_>>())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // The merged people point at the survivor and the survivor is deleted with them, so one
    // statement removes the whole group.
    sqlx::query("DELETE FROM people WHERE workspace_id = $1 AND id = ANY($2)")
        .bind(workspace.as_uuid())
        .bind(&people)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    // Last, so a cursor can fall back to a person who is still there.
    strip_enrolls(connection, workspace, &people).await?;
    // The last job-table work, after every statement that can wait on a handler's row locks:
    // the marks are held until commit, and a handler waiting on one while the erasure waits on
    // the handler's rows would deadlock. `flow.step` names an enrollment, which is gone by now.
    purge_jobs(connection, workspace, &people, &enrollments).await?;

    append_decision(
        connection,
        &NewDecision {
            graph: None,
            workspace_id: workspace,
            trace: trace.clone(),
            action: "person.deleted",
            outcome: "deleted",
            reason: Some(reason.as_str()),
            subject_ids: Vec::new(),
            job: None,
        },
    )
    .await?;
    Ok(true)
}

/// Every statement binds `$1` the workspace and `$2` the people to delete, and runs in this
/// order: a row goes before the rows it refers to.
const DELETIONS: &[&str] = &[
    "DELETE FROM enrollment_waits WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM step_executions WHERE workspace_id = $1 AND enrollment_id IN \n        (SELECT id FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2))",
    "DELETE FROM flow_entries WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM message_links WHERE workspace_id = $1 AND message_id IN 
        (SELECT id FROM messages WHERE workspace_id = $1 AND person_id = ANY($2))",
    "DELETE FROM messages WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM segment_members WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM list_member_changes WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM consent WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM consent_changes WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM debugger_feed WHERE workspace_id = $1 AND (event_id IN         (SELECT id FROM events WHERE workspace_id = $1 AND person_id = ANY($2))         OR reject_id IN         (SELECT id FROM ingest_rejects WHERE workspace_id = $1 AND person_id = ANY($2)))",
    "DELETE FROM forwarding_outbox WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM events WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM event_dedupe WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM ingest_rejects WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM privacy_exports WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM identity_conflicts WHERE workspace_id = $1 \
        AND (person_id = ANY($2) OR other_person_id = ANY($2))",
    "DELETE FROM person_merges WHERE workspace_id = $1 \
        AND (survivor_person_id = ANY($2) OR merged_person_id = ANY($2))",
    "DELETE FROM person_changes WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM source_records WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM identities WHERE workspace_id = $1 AND person_id = ANY($2)",
];

/// The jobs that name an erased person: `$1` is the workspace, `$2` the people and `$3` their
/// enrollment ids as text. It is the one
/// predicate for deleting a pending job and for marking a running one, so the two cannot drift.
macro_rules! names_erased_person {
    () => {
        "( \
            (kind = 'flow.step' AND payload ->> 'enrollment_id' = ANY($3)) \
            OR (kind = 'webhook.call' AND payload ->> 'person_id' IN \
                (SELECT p::text FROM unnest($2::uuid[]) AS p)) \
            OR (kind = 'delivery.send' AND payload ->> 'person_id' IN \
                (SELECT p::text FROM unnest($2::uuid[]) AS p)) \
            OR (kind = 'person.merge.rewrite' AND payload ->> 'merged_person_id' IN \
                (SELECT p::text FROM unnest($2::uuid[]) AS p)))"
    };
}

/// Removes the jobs that carry an erased person's ids from their payloads (`flow.enroll` ones are
/// handled after the people are gone, in [`strip_enrolls`]).
///
/// A pending job is deleted. A running job holds its row `FOR KEY SHARE` (`lock_lease`) and a
/// `DELETE` would wait for it while the handler waits for the people this erasure holds, so it is
/// marked `purge_on_release` instead: a plain `UPDATE` takes `FOR NO KEY UPDATE`, which does not
/// conflict. `fail_job` and `reap_expired` then delete a marked job rather than requeue it or
/// bury it dead, and `complete_job` deletes it as always.
///
/// One statement marks every matching job, whatever its state, locking the rows in id order (the
/// order `strip_enrolls` locks in). The erasure then holds each row until commit: `claim_job`
/// and `reap_expired` (both `FOR UPDATE SKIP LOCKED`) skip it, and `fail_job` and `heartbeat_job`
/// wait for it. So after the mark no matching job changes state, and the delete that follows sees
/// the rows as they were marked. Marking only the running jobs would miss one that is ready at
/// the mark, claimed before the delete's snapshot and failed back to ready after it.
async fn purge_jobs(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
    enrollments: &[String],
) -> Result<(), StoreError> {
    sqlx::query(concat!(
        "UPDATE jobs SET purge_on_release = true WHERE id IN ( \
             SELECT id FROM jobs WHERE workspace_id = $1 AND ",
        names_erased_person!(),
        " ORDER BY id FOR NO KEY UPDATE)"
    ))
    .bind(workspace.as_uuid())
    .bind(people)
    .bind(enrollments)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(concat!(
        "DELETE FROM jobs WHERE workspace_id = $1 AND state <> 'running' AND ",
        names_erased_person!()
    ))
    .bind(workspace.as_uuid())
    .bind(people)
    .bind(enrollments)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Removes `people` from the `flow.enroll` jobs of the workspace, after the people are deleted.
///
/// A running job keeps its lease and its row lock, and is rewritten in place like a pending one:
/// `payload` and `unique_key` are not key columns (the key is a partial unique index, which does
/// not count as one), so the update takes `FOR NO KEY UPDATE` and does not wait for the handler.
/// A running handler holds its old payload in memory, but its page locks the people it read
/// (`lock_audience_page`) before it picks the next cursor, so it waits for this erasure and
/// never writes a follow-up page carrying an erased id.
///
/// - A per-person job (`people`: `(person, occurrence)` pairs) loses the pairs of the erased
///   people; a job left with no pair is deleted.
/// - An audience page carries an `after` cursor, which can be an erased id. The page reads the
///   people with an id greater than it, so the cursor becomes the greatest id that is still a
///   person in the workspace and is below the erased one: the page then starts at the same
///   person. With no such person the cursor is dropped and the audience pages from the start.
async fn strip_enrolls(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<(), StoreError> {
    let erased: Vec<String> = people.iter().map(Uuid::to_string).collect();
    // Every multi-row statement on `jobs` picks its rows first, `ORDER BY id` with the lock it
    // needs, so two erasures in one workspace take shared `flow.enroll` rows in the same order
    // and cannot deadlock.
    sqlx::query(
        "UPDATE jobs j SET payload = jsonb_set(j.payload, '{people}', \
             COALESCE((SELECT jsonb_agg(pair) FROM jsonb_array_elements(j.payload -> 'people') pair \
                       WHERE pair ->> 0 <> ALL($2)), '[]'::jsonb)) \
         WHERE j.id IN (SELECT t.id FROM jobs t \
             WHERE t.workspace_id = $1 AND t.kind = 'flow.enroll' \
               AND NOT t.purge_on_release AND jsonb_typeof(t.payload -> 'people') = 'array' \
               AND EXISTS (SELECT 1 FROM jsonb_array_elements(t.payload -> 'people') pair \
                           WHERE pair ->> 0 = ANY($2)) \
             ORDER BY t.id FOR NO KEY UPDATE)",
    )
    .bind(workspace.as_uuid())
    .bind(&erased)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // A job left with nobody: deleted when pending, marked when running (it has no work to put
    // back, and a marked job is deleted on any release).
    sqlx::query(
        "DELETE FROM jobs WHERE id IN (SELECT id FROM jobs \
             WHERE workspace_id = $1 AND kind = 'flow.enroll' AND state <> 'running' \
               AND payload -> 'people' = '[]'::jsonb \
             ORDER BY id FOR UPDATE)",
    )
    .bind(workspace.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "UPDATE jobs SET purge_on_release = true WHERE id IN (SELECT id FROM jobs \
             WHERE workspace_id = $1 AND kind = 'flow.enroll' AND state = 'running' \
               AND NOT purge_on_release AND payload -> 'people' = '[]'::jsonb \
             ORDER BY id FOR NO KEY UPDATE)",
    )
    .bind(workspace.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // The cursor of a page job is the erased id; the job's unique key names it too, so both are
    // rewritten together, with the same helper that built the key.
    // `FOR NO KEY UPDATE` does not wait for a running handler's `FOR KEY SHARE`.
    let pages: Vec<(Uuid, serde_json::Value, Option<String>, bool)> = sqlx::query_as(
        "SELECT id, payload, unique_key, state = 'running' FROM jobs \
         WHERE workspace_id = $1 AND kind = 'flow.enroll' AND NOT purge_on_release \
           AND payload ->> 'after' = ANY($2) ORDER BY id FOR NO KEY UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&erased)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for (id, payload, key, running) in pages {
        let Some(mut job) = crate::EnrollJobPayload::parse(&payload) else {
            continue;
        };
        let crate::EnrollWork::Audience {
            occurrence, after, ..
        } = &mut job.work
        else {
            continue;
        };
        let Some(old) = *after else { continue };
        let previous: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM people WHERE workspace_id = $1 AND id < $2 ORDER BY id DESC LIMIT 1",
        )
        .bind(workspace.as_uuid())
        .bind(old)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        *after = previous;
        // A page queued by the audience job carries the key of its cursor; any other key (the
        // first page's trigger key) names no cursor and stays.
        let new_key = match (&key, *occurrence) {
            (Some(key), Some(occurrence))
                if *key == crate::enroll_page_key(job.flow, occurrence, Some(old)) =>
            {
                Some(crate::enroll_page_key(job.flow, occurrence, previous))
            }
            _ => None,
        };
        // The rewrite runs in a savepoint: if the page that starts at the fallback cursor is
        // queued already, the unique key refuses the new key (23505) and only the savepoint is
        // undone. Asking first would leave a gap between the check and the update.
        sqlx::query("SAVEPOINT strip_page")
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        let rewritten = sqlx::query(
            "UPDATE jobs SET payload = $2, unique_key = COALESCE($3, unique_key) WHERE id = $1",
        )
        .bind(id)
        .bind(job.to_value())
        .bind(new_key)
        .execute(&mut *connection)
        .await;
        match rewritten {
            Ok(_) => {
                sqlx::query("RELEASE SAVEPOINT strip_page")
                    .execute(&mut *connection)
                    .await
                    .map_err(StoreError::query)?;
            }
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
                sqlx::query("ROLLBACK TO SAVEPOINT strip_page")
                    .execute(&mut *connection)
                    .await
                    .map_err(StoreError::query)?;
                // The same page is queued already, and the unique key allows one. A pending
                // page goes. A running one is not deleted (the delete would wait for its
                // handler's lease while the handler waits for the people this erasure holds):
                // it is marked, and its release deletes it.
                let sql = if running {
                    "UPDATE jobs SET purge_on_release = true WHERE id = $1"
                } else {
                    "DELETE FROM jobs WHERE id = $1"
                };
                sqlx::query(sql)
                    .bind(id)
                    .execute(&mut *connection)
                    .await
                    .map_err(StoreError::query)?;
            }
            Err(error) => return Err(StoreError::query(error)),
        }
    }
    Ok(())
}

/// Removes the people from their lists and keeps each list's member count exact. The lists
/// are locked in id order first, like a merge does.
async fn remove_from_lists(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<(), StoreError> {
    sqlx::query(
        "SELECT id FROM lists WHERE workspace_id = $1 AND id IN \
            (SELECT list_id FROM list_members WHERE workspace_id = $1 AND person_id = ANY($2)) \
         ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "WITH gone AS ( \
             DELETE FROM list_members WHERE workspace_id = $1 AND person_id = ANY($2) \
             RETURNING list_id), \
         counts AS (SELECT list_id, count(*) AS removed FROM gone GROUP BY list_id) \
         UPDATE lists l SET member_count = l.member_count - counts.removed \
         FROM counts WHERE l.id = counts.list_id",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

async fn current_of(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
) -> Result<Option<Uuid>, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(person)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::people::erasure::rows_referencing_person;
    use crate::privacy::testing::{Seeded, seed_everything, trace};
    use crate::{begin, commit};

    #[test]
    fn a_reason_has_a_stable_name() {
        assert_eq!(DeleteReason::Requested.as_str(), "requested");
        assert_eq!(DeleteReason::Erasure.as_str(), "erasure");
        assert_eq!(DeleteReason::Orphan.as_str(), "orphan");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_person_leaves_no_row_that_refers_to_them_or_their_merged_people(
        pool: PgPool,
    ) {
        let Seeded {
            ws,
            person,
            merged,
            bystander,
            ..
        } = seed_everything(&pool).await;
        assert!(!rows_referencing_person(&pool, person).await.is_empty());
        assert!(!rows_referencing_person(&pool, merged).await.is_empty());

        let mut tx = begin(&pool).await.unwrap();
        let deleted = delete_person(&mut tx, ws, person, DeleteReason::Requested, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert!(deleted);
        assert_eq!(rows_referencing_person(&pool, person).await, []);
        let left: Vec<String> =
            sqlx::query_scalar("SELECT unique_key FROM jobs WHERE unique_key LIKE 'seed:%'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            left,
            ["seed:running"],
            "pending flow jobs go, a running lease stays (marked, deleted on release)"
        );
        assert_eq!(rows_referencing_person(&pool, merged).await, []);
        // The other person is untouched.
        assert!(
            rows_referencing_person(&pool, bystander)
                .await
                .iter()
                .any(|(place, _)| place == "people.id")
        );
        let drifted: Vec<i64> = sqlx::query_scalar(
            "SELECT member_count FROM lists l WHERE member_count <> \
                (SELECT count(*) FROM list_members m WHERE m.list_id = l.id)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(drifted.is_empty(), "member counts drifted: {drifted:?}");
        let segment_drift: Vec<i32> = sqlx::query_scalar(
            "SELECT member_count FROM segments s WHERE member_count <> \
                (SELECT count(*) FROM segment_members m \
                 WHERE m.segment_id = s.id AND m.exited_at IS NULL)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            segment_drift.is_empty(),
            "segment counts drifted: {segment_drift:?}"
        );
        let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM segment_members")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "only the bystander's membership is left");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_deletes_the_pending_merge_rewrite_job_of_the_group(pool: PgPool) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let of_merged = "SELECT count(*) FROM jobs WHERE kind = 'person.merge.rewrite' \
                         AND payload ->> 'merged_person_id' = $1";
        let queued: i64 = sqlx::query_scalar(of_merged)
            .bind(merged.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(queued, 1, "the merge queued its rewrite");
        // The rewrite of somebody else's merge stays queued.
        let other = Uuid::now_v7();
        let mut job = crate::NewJob::new(crate::REWRITE_JOB_KIND, trace());
        job.workspace_id = Some(ws);
        job.payload = serde_json::json!({ "workspace_id": ws, "merged_person_id": other });
        crate::enqueue(&pool, &job).await.unwrap();

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let naming: Vec<String> = sqlx::query_scalar(
            "SELECT id::text FROM jobs WHERE payload::text LIKE '%' || $1 || '%' \
                OR payload::text LIKE '%' || $2 || '%' \
                OR unique_key LIKE '%' || $1 || '%' OR unique_key LIKE '%' || $2 || '%'",
        )
        .bind(person.to_string())
        .bind(merged.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(naming, Vec::<String>::new());
        let left: i64 = sqlx::query_scalar(of_merged)
            .bind(other.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_merged_person_deletes_the_survivor_and_the_group(pool: PgPool) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;

        let mut tx = begin(&pool).await.unwrap();
        let deleted = delete_person(&mut tx, ws, merged, DeleteReason::Requested, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert!(deleted);
        assert_eq!(rows_referencing_person(&pool, person).await, []);
        assert_eq!(rows_referencing_person(&pool, merged).await, []);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_node_id_that_looks_like_another_persons_id_is_not_a_reference_to_them(pool: PgPool) {
        use crate::GraphPoint;

        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let scanned = |found: Vec<(String, i64)>| {
            found
                .into_iter()
                .find(|(place, _)| place == "decision_records.subject_ids")
                .map_or(0, |(_, count)| count)
        };
        let before = scanned(rows_referencing_person(&pool, bystander).await);
        // A decision about `person` at a node whose author-chosen id is the bystander's id.
        append_decision(
            &pool,
            &NewDecision {
                workspace_id: ws,
                trace: trace(),
                action: "flow.step",
                outcome: "bait",
                reason: None,
                subject_ids: vec![("person", person)],
                job: None,
                graph: Some(GraphPoint::at(1, &bystander.to_string())),
            },
        )
        .await
        .unwrap();
        let bait = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM decision_records WHERE outcome = 'bait'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        assert_eq!(
            scanned(rows_referencing_person(&pool, bystander).await),
            before,
            "the scan does not count a node id"
        );
        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, bystander, DeleteReason::Requested, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(bait().await, 1, "erasing the bystander leaves it");

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Requested, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(bait().await, 0, "erasing its own person deletes it");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_an_unknown_person_changes_nothing_and_says_so(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;

        let mut tx = begin(&pool).await.unwrap();
        let deleted = delete_person(
            &mut tx,
            ws,
            Uuid::now_v7(),
            DeleteReason::Requested,
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();

        assert!(!deleted);
        assert!(!rows_referencing_person(&pool, person).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_deletion_adds_no_suppression_and_records_its_reason_without_a_person(pool: PgPool) {
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let trace = trace();

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Orphan, &trace)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let suppressions: i64 = sqlx::query_scalar("SELECT count(*) FROM suppressions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(suppressions, 0);
        let (records, _) = crate::list_trace(&pool, ws, trace.trace_id, 10)
            .await
            .unwrap();
        let deleted: Vec<_> = records
            .iter()
            .filter(|record| record.action == "person.deleted")
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].reason.as_deref(), Some("orphan"));
        assert_eq!(deleted[0].subject_ids, serde_json::json!({}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_removes_every_flow_row_of_the_group_and_none_of_the_bystanders(
        pool: PgPool,
    ) {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let flow: Uuid = sqlx::query_scalar("SELECT id FROM flows")
            .fetch_one(&pool)
            .await
            .unwrap();
        // The bystander has a full set of flow rows too.
        let enrollment = Uuid::now_v7();
        for statement in [
            "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, state, \
                node, occurrence_id, state_version, trace_id, cause_kind, cause_id) \
             VALUES ($3, $2, $1, 1, $4, 'waiting', 'n', gen_random_uuid(), 1, \
                gen_random_uuid(), 'api', 't')",
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome) \
             VALUES (gen_random_uuid(), $2, $3, 'n', 'waiting')",
            "INSERT INTO enrollment_waits (workspace_id, enrollment_id, person_id, event_name, \
                wake_version, timeout_at) VALUES ($2, $3, $4, 'seen', 1, now())",
            "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, \
                last_entered_at) VALUES ($2, $1, $4, 1, now())",
        ] {
            sqlx::query(statement)
                .bind(flow)
                .bind(ws.as_uuid())
                .bind(enrollment)
                .bind(bystander)
                .execute(&pool)
                .await
                .unwrap();
        }

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        for table in [
            "enrollments",
            "step_executions",
            "enrollment_waits",
            "flow_entries",
        ] {
            let left: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(left, 1, "{table}: only the bystander's row is left");
        }
    }

    fn enroll_payload(ws: WorkspaceId, work: crate::EnrollWork) -> serde_json::Value {
        crate::EnrollJobPayload {
            workspace: ws,
            flow: Uuid::from_u128(77),
            version: Some(1),
            trigger: crate::TriggerKind::Api,
            work,
        }
        .to_value()
    }

    async fn queue_enroll(pool: &PgPool, ws: WorkspaceId, key: &str, payload: serde_json::Value) {
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, cause_id, \
                trace_id) \
             VALUES (gen_random_uuid(), $1, 'flow.enroll', $2, $3, 'api', 't', gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(payload)
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn queued_enroll(pool: &PgPool, key: &str) -> Option<crate::EnrollJobPayload> {
        let payload: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT payload FROM jobs WHERE unique_key = $1")
                .bind(key)
                .fetch_optional(pool)
                .await
                .unwrap();
        payload.map(|payload| crate::EnrollJobPayload::parse(&payload).unwrap())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_strips_the_person_from_pending_enroll_jobs_and_leaves_the_others(
        pool: PgPool,
    ) {
        use crate::{AudienceSource, EnrollWork};
        let Seeded {
            ws,
            person,
            merged,
            bystander,
            ..
        } = seed_everything(&pool).await;
        // A person older than everyone seeded: the id an erased cursor falls back to.
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let occurrence = Uuid::from_u128(5);
        queue_enroll(
            &pool,
            ws,
            "mixed",
            enroll_payload(
                ws,
                EnrollWork::People(vec![
                    (person, occurrence),
                    (bystander, occurrence),
                    (merged, occurrence),
                ]),
            ),
        )
        .await;
        queue_enroll(
            &pool,
            ws,
            "only-erased",
            enroll_payload(ws, EnrollWork::People(vec![(person, occurrence)])),
        )
        .await;
        let audience = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        queue_enroll(
            &pool,
            ws,
            "cursor-erased",
            enroll_payload(ws, audience(Some(person))),
        )
        .await;
        queue_enroll(
            &pool,
            ws,
            "cursor-other",
            enroll_payload(ws, audience(Some(bystander))),
        )
        .await;
        queue_enroll(&pool, ws, "cursor-none", enroll_payload(ws, audience(None))).await;

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let mixed = queued_enroll(&pool, "mixed").await.unwrap();
        assert_eq!(
            mixed.work,
            EnrollWork::People(vec![(bystander, occurrence)])
        );
        assert!(
            queued_enroll(&pool, "only-erased").await.is_none(),
            "a job left with nobody is removed"
        );
        let erased_cursor = queued_enroll(&pool, "cursor-erased").await.unwrap();
        assert_eq!(
            erased_cursor.work,
            audience(Some(older)),
            "the cursor falls back to the last person before the erased one, which pages the same"
        );
        assert_eq!(
            queued_enroll(&pool, "cursor-other").await.unwrap().work,
            audience(Some(bystander))
        );
        assert_eq!(
            queued_enroll(&pool, "cursor-none").await.unwrap().work,
            audience(None)
        );
        let named: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'flow.enroll' \
               AND (payload::text LIKE '%' || $1::text || '%' \
                    OR payload::text LIKE '%' || $2::text || '%')",
        )
        .bind(person)
        .bind(merged)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(named, 0, "no pending enroll names the erased group");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_cursor_with_nobody_before_it_is_dropped_so_the_audience_pages_from_the_start(
        pool: PgPool,
    ) {
        use crate::{AudienceSource, EnrollWork};
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        queue_enroll(
            &pool,
            ws,
            "first",
            enroll_payload(
                ws,
                EnrollWork::Audience {
                    source: AudienceSource::Everyone,
                    occurrence: None,
                    after: Some(person),
                    entered_by: None,
                },
            ),
        )
        .await;

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let after = match queued_enroll(&pool, "first").await.unwrap().work {
            EnrollWork::Audience { after, .. } => after,
            EnrollWork::People(_) => panic!("an audience page"),
        };
        assert_eq!(after, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_rewrites_the_unique_key_of_a_page_with_its_cursor(pool: PgPool) {
        use crate::{AudienceSource, EnrollWork, enroll_page_key};
        let Seeded {
            ws,
            person,
            merged,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let flow = Uuid::from_u128(77);
        let occurrence = Uuid::from_u128(5);
        let page = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        // A page named by the erased person, one by their merged person (the cursor falls back
        // to the same person, so the two pages are one) and one by a bystander.
        for after in [person, merged, bystander] {
            queue_enroll(
                &pool,
                ws,
                &enroll_page_key(flow, occurrence, Some(after)),
                enroll_payload(ws, page(Some(after))),
            )
            .await;
        }

        let mut tx = begin(&pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let named: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'flow.enroll' \
               AND (payload::text LIKE '%' || $1::text || '%' \
                    OR unique_key LIKE '%' || $1::text || '%' \
                    OR payload::text LIKE '%' || $2::text || '%' \
                    OR unique_key LIKE '%' || $2::text || '%')",
        )
        .bind(person)
        .bind(merged)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(named, 0, "no pending enroll names the erased group");
        let rewritten = queued_enroll(&pool, &enroll_page_key(flow, occurrence, Some(older))).await;
        assert_eq!(
            rewritten.map(|payload| payload.work),
            Some(page(Some(older))),
            "the key follows the cursor, and the duplicate page is gone"
        );
        let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.enroll'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(keys, 2, "the rewritten page and the bystander's page");
    }

    // A job that is running when its person is erased: the erasure marks it, and every release
    // (a failure with attempts left, the last failure, an expired lease) deletes it.

    const LEASE: std::time::Duration = std::time::Duration::from_secs(60);

    async fn enrollment_of(pool: &PgPool, person: Uuid) -> Uuid {
        sqlx::query_scalar("SELECT id FROM enrollments WHERE person_id = $1")
            .bind(person)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Replaces the seeded jobs with one job of `kind`, and leases it.
    async fn lease_alone(
        pool: &PgPool,
        ws: WorkspaceId,
        kind: &str,
        payload: serde_json::Value,
        key: &str,
        max_attempts: i32,
        lease: std::time::Duration,
    ) -> crate::ClaimedJob {
        sqlx::query("DELETE FROM jobs").execute(pool).await.unwrap();
        lease_more(pool, ws, kind, payload, key, max_attempts, lease).await
    }

    async fn lease_more(
        pool: &PgPool,
        ws: WorkspaceId,
        kind: &str,
        payload: serde_json::Value,
        key: &str,
        max_attempts: i32,
        lease: std::time::Duration,
    ) -> crate::ClaimedJob {
        let mut new = crate::NewJob::new(kind, trace());
        new.workspace_id = Some(ws);
        new.payload = payload;
        new.unique_key = Some(key.to_owned());
        new.max_attempts = max_attempts;
        assert!(crate::enqueue(pool, &new).await.unwrap());
        crate::claim_job(pool, "w", &[kind], lease, None)
            .await
            .unwrap()
            .expect("the job is claimable")
    }

    async fn erase(pool: &PgPool, ws: WorkspaceId, person: Uuid) {
        let mut tx = begin(pool).await.unwrap();
        delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    /// The jobs whose payload or key contains any of `ids`.
    async fn jobs_naming(pool: &PgPool, ids: &[Uuid]) -> i64 {
        let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        sqlx::query_scalar(
            "SELECT count(*) FROM jobs j WHERE EXISTS (SELECT 1 FROM unnest($1::text[]) AS i \
               WHERE j.payload::text LIKE '%' || i || '%' OR j.unique_key LIKE '%' || i || '%')",
        )
        .bind(&ids)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn fail(pool: &PgPool, claimed: &crate::ClaimedJob) -> Option<crate::FailOutcome> {
        crate::fail_job(
            pool,
            claimed.id,
            claimed.lease_token,
            std::time::Duration::ZERO,
            "boom",
        )
        .await
        .unwrap()
    }

    #[derive(Clone, Copy)]
    enum Release {
        Fail,
        Reap,
    }

    /// Leases a job of `kind` that names the erased group, erases the group, releases the job and
    /// checks that no job is left.
    async fn erased_while_running(
        pool: PgPool,
        kind: &'static str,
        max_attempts: i32,
        release: Release,
    ) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let enrollment = enrollment_of(&pool, person).await;
        let payload = match kind {
            "flow.step" => serde_json::json!({ "enrollment_id": enrollment }),
            "webhook.call" | "delivery.send" => serde_json::json!({ "person_id": person }),
            "person.merge.rewrite" => {
                serde_json::json!({ "workspace_id": ws, "merged_person_id": merged })
            }
            "flow.enroll" => enroll_payload(
                ws,
                crate::EnrollWork::People(vec![(person, Uuid::from_u128(5))]),
            ),
            other => panic!("no payload for {other}"),
        };
        let lease = match release {
            Release::Fail => LEASE,
            Release::Reap => std::time::Duration::from_millis(1),
        };
        let claimed = lease_alone(&pool, ws, kind, payload, "running:1", max_attempts, lease).await;

        erase(&pool, ws, person).await;

        let marked: bool = sqlx::query_scalar("SELECT purge_on_release FROM jobs WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(marked, "the erasure marks the running job");
        match release {
            Release::Fail => {
                assert_eq!(
                    fail(&pool, &claimed).await,
                    Some(crate::FailOutcome::Purged)
                );
            }
            Release::Reap => {
                crate::privacy::testing::wait_until_lease_expired(&pool, claimed.id).await;
                let reaped = crate::reap_expired(&pool).await.unwrap();
                assert_eq!(
                    reaped,
                    crate::Reaped {
                        requeued: 0,
                        dead: 0,
                        purged: 1
                    }
                );
            }
        }
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "no job is left");
        assert_eq!(
            jobs_naming(&pool, &[person, merged, enrollment]).await,
            0,
            "no job names the erased group"
        );
    }

    macro_rules! erased_while_running_tests {
        ($($name:ident: $kind:literal,)*) => {$(
            mod $name {
                use super::*;

                #[sqlx::test]
                #[ignore = "requires PostgreSQL; set DATABASE_URL"]
                async fn a_failure_with_attempts_left_deletes_the_job(pool: PgPool) {
                    erased_while_running(pool, $kind, 3, Release::Fail).await;
                }

                #[sqlx::test]
                #[ignore = "requires PostgreSQL; set DATABASE_URL"]
                async fn the_last_failed_attempt_deletes_the_job(pool: PgPool) {
                    erased_while_running(pool, $kind, 1, Release::Fail).await;
                }

                #[sqlx::test]
                #[ignore = "requires PostgreSQL; set DATABASE_URL"]
                async fn an_expired_lease_with_attempts_left_deletes_the_job(pool: PgPool) {
                    erased_while_running(pool, $kind, 3, Release::Reap).await;
                }

                #[sqlx::test]
                #[ignore = "requires PostgreSQL; set DATABASE_URL"]
                async fn an_expired_lease_on_the_last_attempt_deletes_the_job(pool: PgPool) {
                    erased_while_running(pool, $kind, 1, Release::Reap).await;
                }
            }
        )*};
    }

    erased_while_running_tests! {
        running_flow_step: "flow.step",
        running_webhook_call: "webhook.call",
        running_delivery_send: "delivery.send",
        running_merge_rewrite: "person.merge.rewrite",
        running_flow_enroll: "flow.enroll",
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erased_person_loses_their_messages_and_their_pending_sends_but_not_a_bystanders(
        pool: PgPool,
    ) {
        let Seeded {
            ws,
            person,
            merged,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let kept = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, completed_at) \
             VALUES ($1, $2, $3, gen_random_uuid(), 'n', gen_random_uuid(), gen_random_uuid(), \
                'sent', now())",
        )
        .bind(kept)
        .bind(ws.as_uuid())
        .bind(bystander)
        .execute(&pool)
        .await
        .unwrap();
        let messages = |people: Vec<Uuid>| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM messages WHERE person_id = ANY($1)",
                )
                .bind(people)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(messages(vec![person, merged]).await, 2);
        sqlx::query(
            "INSERT INTO message_links (workspace_id, message_id, link_index, url)              SELECT workspace_id, id, 0, 'https://links.test/x' FROM messages",
        )
        .execute(&pool)
        .await
        .unwrap();
        let sends = "SELECT count(*) FROM jobs WHERE kind = 'delivery.send'";
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sends)
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );

        erase(&pool, ws, person).await;

        assert_eq!(messages(vec![person, merged]).await, 0);
        assert_eq!(
            messages(vec![bystander]).await,
            1,
            "a bystander keeps their log"
        );
        let links: Vec<Uuid> = sqlx::query_scalar("SELECT message_id FROM message_links")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            links,
            [kept],
            "erasing the person removes their messages' links"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sends)
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "the pending send is purged"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_job_that_names_nobody_erased_is_not_marked_and_still_requeues(pool: PgPool) {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let other = lease_alone(
            &pool,
            ws,
            "webhook.call",
            serde_json::json!({ "person_id": bystander }),
            "running:other",
            3,
            LEASE,
        )
        .await;
        let named = lease_more(
            &pool,
            ws,
            "webhook.call",
            serde_json::json!({ "person_id": person }),
            "running:named",
            3,
            LEASE,
        )
        .await;

        erase(&pool, ws, person).await;

        assert_eq!(fail(&pool, &other).await, Some(crate::FailOutcome::Retried));
        assert_eq!(fail(&pool, &named).await, Some(crate::FailOutcome::Purged));
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [other.id]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_neither_waits_for_nor_deadlocks_with_a_handler_holding_the_lease(
        pool: PgPool,
    ) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let claimed = lease_alone(
            &pool,
            ws,
            "person.merge.rewrite",
            serde_json::json!({ "workspace_id": ws, "merged_person_id": merged }),
            "running:held",
            3,
            LEASE,
        )
        .await;
        // The handler's transaction holds the row `FOR KEY SHARE`, as `lock_lease` does.
        let mut handler = begin(&pool).await.unwrap();
        assert!(
            crate::lock_lease(&mut handler, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );

        let erased =
            tokio::time::timeout(std::time::Duration::from_secs(10), erase(&pool, ws, person))
                .await;
        assert!(erased.is_ok(), "the erasure waited for the handler's lease");

        // The handler fails and rolls back; its release deletes the marked job.
        handler.rollback().await.unwrap();
        assert_eq!(
            fail(&pool, &claimed).await,
            Some(crate::FailOutcome::Purged)
        );
        assert_eq!(jobs_naming(&pool, &[person, merged]).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_enroll_job_loses_the_erased_person_in_place_and_keeps_the_others(
        pool: PgPool,
    ) {
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        let occurrence = Uuid::from_u128(5);
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.enroll",
            enroll_payload(
                ws,
                crate::EnrollWork::People(vec![(person, occurrence), (bystander, occurrence)]),
            ),
            "running:people",
            3,
            LEASE,
        )
        .await;

        erase(&pool, ws, person).await;

        assert_eq!(
            fail(&pool, &claimed).await,
            Some(crate::FailOutcome::Retried)
        );
        assert_eq!(
            queued_enroll(&pool, "running:people")
                .await
                .map(|payload| payload.work),
            Some(crate::EnrollWork::People(vec![(bystander, occurrence)])),
            "the bystander's enrollment is still queued"
        );
        assert_eq!(jobs_naming(&pool, &[person]).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_audience_page_with_an_erased_cursor_is_rewritten_in_place(pool: PgPool) {
        use crate::{AudienceSource, EnrollWork, enroll_page_key};
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let flow = Uuid::from_u128(77);
        let occurrence = Uuid::from_u128(5);
        let page = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        let key = enroll_page_key(flow, occurrence, Some(person));
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.enroll",
            enroll_payload(ws, page(Some(person))),
            &key,
            3,
            LEASE,
        )
        .await;

        erase(&pool, ws, person).await;

        assert_eq!(
            fail(&pool, &claimed).await,
            Some(crate::FailOutcome::Retried)
        );
        assert_eq!(jobs_naming(&pool, &[person]).await, 0);
        let pages: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.enroll'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pages, 1, "the page is rewritten, not replaced");
        assert_eq!(
            queued_enroll(&pool, &enroll_page_key(flow, occurrence, Some(older)))
                .await
                .map(|payload| payload.work),
            Some(page(Some(older))),
            "the audience still pages on from the person before the erased cursor"
        );
    }

    /// A `flow.step` handler as the worker runs it: it holds the job row and writes the
    /// enrollment of the person. The erasure then waits for the enrollment row.
    async fn handler_on_enrollment(
        pool: &PgPool,
        claimed: &crate::ClaimedJob,
        enrollment: Uuid,
    ) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut handler = begin(pool).await.unwrap();
        assert!(
            crate::lock_lease(&mut handler, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE enrollments SET state_version = state_version + 1 WHERE id = $1")
            .bind(enrollment)
            .execute(&mut *handler)
            .await
            .unwrap();
        handler
    }

    /// Starts an erasure and returns once its backend waits on a lock the handler holds.
    async fn erasing(pool: &PgPool, ws: WorkspaceId, person: Uuid) -> tokio::task::JoinHandle<()> {
        let (erasure, pid) = spawn_erasure(pool, ws, person).await;
        crate::privacy::testing::wait_until_blocked(pool, pid).await;
        erasure
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_that_wrote_the_persons_rows_can_complete_while_the_erasure_waits_for_them(
        pool: PgPool,
    ) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let enrollment = enrollment_of(&pool, person).await;
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.step",
            serde_json::json!({ "enrollment_id": enrollment }),
            "running:step",
            3,
            LEASE,
        )
        .await;
        let mut handler = handler_on_enrollment(&pool, &claimed, enrollment).await;
        let erasure = erasing(&pool, ws, person).await;

        // The worker's completion: delete the job and commit.
        let bound = std::time::Duration::from_secs(10);
        let done = tokio::time::timeout(
            bound,
            crate::complete_job(&mut *handler, claimed.id, claimed.lease_token),
        )
        .await;
        assert!(done.expect("complete_job waited for the erasure").unwrap());
        commit(handler).await.unwrap();
        tokio::time::timeout(bound, erasure)
            .await
            .expect("the erasure did not finish")
            .unwrap();

        assert_eq!(jobs_naming(&pool, &[person, merged, enrollment]).await, 0);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_that_fails_while_the_erasure_waits_for_its_rows_leaves_no_job(pool: PgPool) {
        let Seeded {
            ws, person, merged, ..
        } = seed_everything(&pool).await;
        let enrollment = enrollment_of(&pool, person).await;
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.step",
            serde_json::json!({ "enrollment_id": enrollment }),
            "running:step",
            3,
            LEASE,
        )
        .await;
        let handler = handler_on_enrollment(&pool, &claimed, enrollment).await;
        let erasure = erasing(&pool, ws, person).await;

        let bound = std::time::Duration::from_secs(10);
        handler.rollback().await.unwrap();
        let outcome = tokio::time::timeout(bound, fail(&pool, &claimed))
            .await
            .expect("fail_job waited for the erasure");
        assert!(
            matches!(
                outcome,
                Some(crate::FailOutcome::Purged | crate::FailOutcome::Retried)
            ),
            "{outcome:?}"
        );
        tokio::time::timeout(bound, erasure)
            .await
            .expect("the erasure did not finish")
            .unwrap();

        assert_eq!(jobs_naming(&pool, &[person, merged, enrollment]).await, 0);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            left, 0,
            "a job that was requeued first is deleted by the erasure"
        );
    }

    async fn insert_webhook_job(
        pool: &PgPool,
        ws: WorkspaceId,
        id: u128,
        person: Uuid,
        priority: i16,
        running: bool,
    ) {
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, priority, state, lease_token, \
                lease_expires_at, attempts, unique_key, cause_kind, cause_id, trace_id) \
             VALUES ($1, $2, 'webhook.call', jsonb_build_object('person_id', $3::text), $4, \
                CASE WHEN $5 THEN 'running' ELSE 'ready' END, \
                CASE WHEN $5 THEN gen_random_uuid() END, \
                CASE WHEN $5 THEN now() + interval '1 hour' END, \
                CASE WHEN $5 THEN 1 ELSE 0 END, $1::text, 'api', 't', gen_random_uuid())",
        )
        .bind(Uuid::from_u128(id))
        .bind(ws.as_uuid())
        .bind(person)
        .bind(priority)
        .bind(running)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn lock_job(
        pool: &PgPool,
        id: u128,
    ) -> (sqlx::Transaction<'static, sqlx::Postgres>, i32) {
        let mut tx = begin(pool).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(Uuid::from_u128(id))
            .execute(&mut *tx)
            .await
            .unwrap();
        (tx, pid)
    }

    /// The erasure statements run at different snapshots. Two other transactions hold a running
    /// job and a ready one that name the person, so the erasure stops twice; while it is stopped
    /// a worker claims the third job and fails it back to ready, once in each gap. The job must
    /// not survive: the erasure locks every matching job in its first statement, so the claim
    /// finds nothing.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_that_changes_state_while_the_erasure_waits_cannot_outlive_it(pool: PgPool) {
        use crate::privacy::testing::wait_until_blocked_by;
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        // The ids fix the order the erasure locks in: the racing job first.
        insert_webhook_job(&pool, ws, 1, person, -5, false).await;
        insert_webhook_job(&pool, ws, 2, person, 0, true).await;
        insert_webhook_job(&pool, ws, 3, person, 0, false).await;
        let (running_blocker, running_pid) = lock_job(&pool, 2).await;
        let (ready_blocker, ready_pid) = lock_job(&pool, 3).await;

        // The erasure marks the racing job first (lowest id), then waits for the running
        // blocker's job.
        let (erasure, erasure_pid) = spawn_erasure(&pool, ws, person).await;
        wait_until_blocked_by(&pool, erasure_pid, running_pid).await;

        // The worker cannot claim the racing job: the erasure holds it.
        let claimed = crate::claim_job(&pool, "w", &["webhook.call"], LEASE, None)
            .await
            .unwrap();
        assert!(
            claimed.is_none(),
            "the erasure left a job claimable while it waited"
        );
        commit(running_blocker).await.unwrap();
        // Now it waits for the ready blocker's job.
        wait_until_blocked_by(&pool, erasure_pid, ready_pid).await;
        commit(ready_blocker).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), erasure)
            .await
            .expect("the erasure did not finish")
            .unwrap();

        // Only the job that was running all along is left, and it is marked.
        let left: Vec<(Uuid, bool)> =
            sqlx::query_as("SELECT id, purge_on_release FROM jobs ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            left,
            [(Uuid::from_u128(2), true)],
            "a job survived the erasure"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reaped_audience_page_with_an_erased_cursor_comes_back_at_the_fallback_cursor(
        pool: PgPool,
    ) {
        use crate::{AudienceSource, EnrollWork, enroll_page_key};
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let flow = Uuid::from_u128(77);
        let occurrence = Uuid::from_u128(5);
        let page = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.enroll",
            enroll_payload(ws, page(Some(person))),
            &enroll_page_key(flow, occurrence, Some(person)),
            3,
            std::time::Duration::from_millis(1),
        )
        .await;

        erase(&pool, ws, person).await;
        crate::privacy::testing::wait_until_lease_expired(&pool, claimed.id).await;
        let reaped = crate::reap_expired(&pool).await.unwrap();

        assert_eq!(
            reaped,
            crate::Reaped {
                requeued: 1,
                dead: 0,
                purged: 0
            }
        );
        assert_eq!(jobs_naming(&pool, &[person]).await, 0);
        assert_eq!(
            queued_enroll(&pool, &enroll_page_key(flow, occurrence, Some(older)))
                .await
                .map(|payload| payload.work),
            Some(page(Some(older)))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_running_page_whose_fallback_page_is_queued_already_is_marked_and_dropped(
        pool: PgPool,
    ) {
        use crate::{AudienceSource, EnrollWork, enroll_page_key};
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let flow = Uuid::from_u128(77);
        let occurrence = Uuid::from_u128(5);
        let page = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        let claimed = lease_alone(
            &pool,
            ws,
            "flow.enroll",
            enroll_payload(ws, page(Some(person))),
            &enroll_page_key(flow, occurrence, Some(person)),
            3,
            LEASE,
        )
        .await;
        queue_enroll(
            &pool,
            ws,
            &enroll_page_key(flow, occurrence, Some(older)),
            enroll_payload(ws, page(Some(older))),
        )
        .await;

        erase(&pool, ws, person).await;

        assert_eq!(
            fail(&pool, &claimed).await,
            Some(crate::FailOutcome::Purged)
        );
        assert_eq!(jobs_naming(&pool, &[person]).await, 0);
        let pages: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'flow.enroll'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(pages, 1, "the queued fallback page is the only one left");
    }

    /// Starts an erasure on a connection of its own and returns once its backend pid is known,
    /// so a test can wait for that backend to block on a lock.
    async fn spawn_erasure(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
    ) -> (tokio::task::JoinHandle<()>, i32) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let pool = pool.clone();
        let handle = tokio::spawn(async move {
            let mut tx = begin(&pool).await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            sender.send(pid).unwrap();
            delete_person(&mut tx, ws, person, DeleteReason::Erasure, &trace())
                .await
                .unwrap();
            commit(tx).await.unwrap();
        });
        (handle, receiver.await.unwrap())
    }

    /// The enroll jobs an erasure strips are locked in id order, whatever order they sit in on
    /// disk, so two erasures cannot take shared rows in opposite orders. The heap order here is
    /// the reverse of the id order; the erasure, blocked at job 2, must already hold job 1.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erasure_locks_the_enroll_jobs_it_strips_in_id_order(pool: PgPool) {
        use crate::privacy::testing::wait_until_blocked;
        let Seeded {
            ws,
            person,
            bystander,
            ..
        } = seed_everything(&pool).await;
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let occurrence = Uuid::from_u128(5);
        for id in [3_u128, 2, 1] {
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, \
                    cause_id, trace_id) \
                 VALUES ($1, $2, 'flow.enroll', $3, $1::text, 'api', 't', gen_random_uuid())",
            )
            .bind(Uuid::from_u128(id))
            .bind(ws.as_uuid())
            .bind(enroll_payload(
                ws,
                crate::EnrollWork::People(vec![(person, occurrence), (bystander, occurrence)]),
            ))
            .execute(&pool)
            .await
            .unwrap();
        }
        let (blocker, _) = lock_job(&pool, 2).await;

        let (erasure, pid) = spawn_erasure(&pool, ws, person).await;
        wait_until_blocked(&pool, pid).await;
        let first_is_held = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE NOWAIT")
            .bind(Uuid::from_u128(1))
            .fetch_optional(&pool)
            .await
            .is_err();
        commit(blocker).await.unwrap();
        erasure.await.unwrap();

        assert!(first_is_held, "the erasure locks the lowest id first");
        assert_eq!(jobs_naming(&pool, &[person]).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_page_whose_fallback_page_is_queued_already_is_dropped_without_an_error(
        pool: PgPool,
    ) {
        use crate::{AudienceSource, EnrollWork, enroll_page_key};
        let Seeded { ws, person, .. } = seed_everything(&pool).await;
        let older = Uuid::from_u128(1);
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(older)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let flow = Uuid::from_u128(77);
        let occurrence = Uuid::from_u128(5);
        let page = |after| EnrollWork::Audience {
            source: AudienceSource::Everyone,
            occurrence: Some(occurrence),
            after,
            entered_by: None,
        };
        for after in [person, older] {
            queue_enroll(
                &pool,
                ws,
                &enroll_page_key(flow, occurrence, Some(after)),
                enroll_payload(ws, page(Some(after))),
            )
            .await;
        }

        // The erasure unwraps every statement: a unique violation would fail it here.
        erase(&pool, ws, person).await;

        let keys: Vec<String> =
            sqlx::query_scalar("SELECT unique_key FROM jobs WHERE kind = 'flow.enroll'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(keys, [enroll_page_key(flow, occurrence, Some(older))]);
    }
}
