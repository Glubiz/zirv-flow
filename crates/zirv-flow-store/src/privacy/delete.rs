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
    strip_pending_enrolls(connection, workspace, &people).await?;

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
    // Pending flow jobs and merge rewrites carry the person's ids in their payloads (`flow.enroll`
    // ones are stripped after the people are gone, below). A running job keeps its lease; a
    // running rewrite holds the group's rows `FOR KEY SHARE`, so this erasure waits for it.
    "DELETE FROM jobs WHERE workspace_id = $1 AND state <> 'running' AND ( \n        (kind = 'flow.step' AND payload ->> 'enrollment_id' IN \n            (SELECT id::text FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2))) \n        OR (kind = 'webhook.call' AND payload ->> 'person_id' IN \n            (SELECT p::text FROM unnest($2::uuid[]) AS p)) \n        OR (kind = 'person.merge.rewrite' AND payload ->> 'merged_person_id' IN \n            (SELECT p::text FROM unnest($2::uuid[]) AS p)))",
    "DELETE FROM enrollment_waits WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM step_executions WHERE workspace_id = $1 AND enrollment_id IN \n        (SELECT id FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2))",
    "DELETE FROM flow_entries WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM enrollments WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM segment_members WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM list_member_changes WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM consent WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM consent_changes WHERE workspace_id = $1 AND person_id = ANY($2)",
    "DELETE FROM debugger_feed WHERE workspace_id = $1 AND (event_id IN         (SELECT id FROM events WHERE workspace_id = $1 AND person_id = ANY($2))         OR reject_id IN         (SELECT id FROM ingest_rejects WHERE workspace_id = $1 AND person_id = ANY($2)))",
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

/// Removes `people` from the pending `flow.enroll` jobs of the workspace, after the people are
/// deleted. A running job keeps its lease, as for the other flow jobs.
///
/// - A per-person job (`people`: `(person, occurrence)` pairs) loses the pairs of the erased
///   people; a job left with no pair is deleted.
/// - An audience page carries an `after` cursor, which can be an erased id. The page reads the
///   people with an id greater than it, so the cursor becomes the greatest id that is still a
///   person in the workspace and is below the erased one: the page then starts at the same
///   person. With no such person the cursor is dropped and the audience pages from the start.
async fn strip_pending_enrolls(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<(), StoreError> {
    let erased: Vec<String> = people.iter().map(Uuid::to_string).collect();
    sqlx::query(
        "UPDATE jobs j SET payload = jsonb_set(j.payload, '{people}', \
             COALESCE((SELECT jsonb_agg(pair) FROM jsonb_array_elements(j.payload -> 'people') pair \
                       WHERE pair ->> 0 <> ALL($2)), '[]'::jsonb)) \
         WHERE j.workspace_id = $1 AND j.kind = 'flow.enroll' AND j.state <> 'running' \
           AND jsonb_typeof(j.payload -> 'people') = 'array' \
           AND EXISTS (SELECT 1 FROM jsonb_array_elements(j.payload -> 'people') pair \
                       WHERE pair ->> 0 = ANY($2))",
    )
    .bind(workspace.as_uuid())
    .bind(&erased)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "DELETE FROM jobs WHERE workspace_id = $1 AND kind = 'flow.enroll' AND state <> 'running' \
           AND payload -> 'people' = '[]'::jsonb",
    )
    .bind(workspace.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    // The cursor of a page job is the erased id; the job's unique key names it too, so both are
    // rewritten together, with the same helper that built the key.
    let pages: Vec<(Uuid, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT id, payload, unique_key FROM jobs \
         WHERE workspace_id = $1 AND kind = 'flow.enroll' AND state <> 'running' \
           AND payload ->> 'after' = ANY($2) ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&erased)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    for (id, payload, key) in pages {
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
        if let Some(new_key) = &new_key {
            // The page that starts at the fallback cursor is already queued: this one is the
            // same work, and the unique key allows one.
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM jobs WHERE unique_key = $1 AND id <> $2 \
                   AND state IN ('ready', 'running'))",
            )
            .bind(new_key)
            .bind(id)
            .fetch_one(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            if taken {
                sqlx::query("DELETE FROM jobs WHERE id = $1")
                    .bind(id)
                    .execute(&mut *connection)
                    .await
                    .map_err(StoreError::query)?;
                continue;
            }
        }
        sqlx::query(
            "UPDATE jobs SET payload = $2, unique_key = COALESCE($3, unique_key) WHERE id = $1",
        )
        .bind(id)
        .bind(job.to_value())
        .bind(new_key)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
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
            "pending flow jobs go, a running lease stays"
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
}
