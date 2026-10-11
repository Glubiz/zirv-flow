//! Merging people (architecture section 7.3): what moves to the survivor in the transaction
//! of the record that joined them, and the job that re-points the rest afterwards.
//!
//! Consent and list memberships move in [`merge_people`] too once they exist.

use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::{mark_dirty, recompute, record_decision};
use crate::{NewDecision, NewJob, StoreError, append_decision, enqueue};

/// The job kind that re-points what [`merge_people`] leaves behind.
pub const REWRITE_JOB_KIND: &str = "person.merge.rewrite";

/// Rows re-pointed per statement by the rewrite.
pub const REWRITE_BATCH: i64 = 1000;

/// Person-scoped columns that name a merged person and are re-pointed in batches rather than
/// in the merge transaction. The events table joins this list with its `person_id` column.
const REWRITE_TARGETS: &[(&str, &str)] = &[
    ("identity_conflicts", "person_id"),
    ("identity_conflicts", "other_person_id"),
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
        sqlx::query("DELETE FROM person_changes WHERE workspace_id = $1 AND person_id = $2")
            .bind(workspace.as_uuid())
            .bind(loser)
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        // The loser keeps no traits: reads follow merged_into to the survivor's.
        sqlx::query(
            "UPDATE people SET merged_into = $3, traits = '{}'::jsonb, \
                trait_sources = '{}'::jsonb, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
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
        record_decision(
            connection,
            workspace,
            trace,
            "person.merge",
            "merged",
            None,
            vec![
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
    mark_dirty(connection, workspace, survivor).await?;
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

/// Re-points what still names `merged` to the person it was merged into, in batches of
/// [`REWRITE_BATCH`] rows per statement, on `connection`. Returns how many rows moved; a
/// second run moves none. Appends a `person.merge.rewrite` decision once the person is
/// merged and every batch is done; a person that was not merged is left alone.
pub async fn rewrite_merged_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    merged: Uuid,
    trace: &TraceContext,
    job: Option<(Uuid, i32)>,
) -> Result<u64, StoreError> {
    let survivor: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT merged_into FROM people WHERE workspace_id = $1 AND id = $2")
            .bind(workspace.as_uuid())
            .bind(merged)
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    let Some(Some(survivor)) = survivor else {
        return Ok(0);
    };
    let mut total = 0;
    for (table, column) in REWRITE_TARGETS {
        loop {
            // Names come from the constant above, never from input.
            let moved = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET {column} = $3 WHERE workspace_id = $1 AND id IN \
                    (SELECT id FROM {table} WHERE workspace_id = $1 AND {column} = $2 LIMIT $4)"
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
    append_decision(
        connection,
        &NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "person.merge.rewrite",
            outcome: "completed",
            reason: None,
            subject_ids: vec![("survivor", survivor), ("merged", merged)],
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
        let dirty: i64 =
            sqlx::query_scalar("SELECT count(*) FROM person_changes WHERE person_id = $1")
                .bind(anonymous)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dirty, 0);
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
            assert_eq!(person_of(&one[0]), person_of(&two[0]));
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
            .bind(person_of(&one[0]))
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
        let third = apply_one(
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
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO identity_conflicts \
                    (id, workspace_id, {column}, {other}, kind, source_record_id, reason) \
                 SELECT gen_random_uuid(), $1, $2, $3, 'email', $4, 'unique_kind' \
                 FROM generate_series(1, 2500)"
            )))
            .bind(ws.as_uuid())
            .bind(loser)
            .bind(third)
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
}
