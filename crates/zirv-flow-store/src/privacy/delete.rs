//! Person deletion: removes every row that refers to a person and to the people merged into
//! them. Deletion is not erasure: it adds no suppression and no ledger entry.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

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
    for statement in DELETIONS {
        sqlx::query(*statement)
            .bind(workspace.as_uuid())
            .bind(&people)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    }
    sqlx::query(
        "DELETE FROM decision_records d WHERE d.workspace_id = $1 AND EXISTS \
            (SELECT 1 FROM jsonb_each_text(d.subject_ids) s WHERE s.value = ANY($2::text[]))",
    )
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

    append_decision(
        connection,
        &NewDecision {
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
}
