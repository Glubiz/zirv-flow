//! Bringing materialized traits back in line with the registry after a definition changes
//! its precedence, source order or archived flag.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::apply::{mark_dirty, recompute};
use crate::changes::request_reevaluation;
use crate::{NewDecision, StoreError, append_decision};

/// How many people one recompute batch covers.
pub const RECOMPUTE_BATCH: i64 = 500;

/// Recomputes `key` for up to [`RECOMPUTE_BATCH`] people who hold a live contribution for it,
/// in person-id order after `after`, on `connection` (the caller's transaction). A person
/// whose materialized value changed is marked dirty and gets a `trait.change` decision
/// (ids only). Returns the cursor for the next batch, or `None` when this was the last.
pub async fn recompute_trait_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    key: &str,
    after: Option<Uuid>,
    trace: &TraceContext,
    job: Option<(Uuid, i32)>,
) -> Result<Option<Uuid>, StoreError> {
    let people: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT person_id FROM source_records \
         WHERE workspace_id = $1 AND deleted_at IS NULL AND traits ? $2 \
           AND ($3::uuid IS NULL OR person_id > $3) \
         ORDER BY person_id LIMIT $4",
    )
    .bind(workspace.as_uuid())
    .bind(key)
    .bind(after)
    .bind(RECOMPUTE_BATCH)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let keys = [key.to_owned()];
    for &person in &people {
        if recompute(connection, workspace, person, &keys).await? {
            mark_dirty(connection, workspace, person, ChangeClasses::TRAITS, trace).await?;
            append_decision(
                &mut *connection,
                &NewDecision {
                    workspace_id: workspace,
                    trace: trace.clone(),
                    action: "trait.change",
                    outcome: "changed",
                    reason: Some("definition_changed"),
                    subject_ids: vec![("person", person)],
                    job,
                },
            )
            .await?;
        }
    }
    if !people.is_empty() {
        request_reevaluation(connection, workspace).await?;
    }
    let full = i64::try_from(people.len()).is_ok_and(|n| n == RECOMPUTE_BATCH);
    Ok(if full { people.last().copied() } else { None })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::people::Precedence;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::people::{
        ProfileRecord, TraitDefinitionUpdate, apply_profile_batch, find_person,
        update_trait_definition,
    };
    use crate::{begin, commit, create_workspace};

    fn record(source: &str, key: &str, tag: &str, month: u8) -> ProfileRecord {
        ProfileRecord {
            source: source.into(),
            stream: "s".into(),
            record_key: key.into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: json!({ "tier": tag }).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC)
                .replace_month(time::Month::try_from(month).unwrap())
                .unwrap(),
            trace: TraceContext::root(Cause::api("t")),
        }
    }

    async fn setup(pool: &PgPool) -> (WorkspaceId, Uuid) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(pool, &name, &slug).await.unwrap().id;
        let outcomes = apply_profile_batch(
            pool,
            ws,
            &[
                record("crm", "1", "crm-old", 1),
                record("shop", "2", "shop-new", 2),
            ],
        )
        .await
        .unwrap();
        let crate::ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        (ws, person_id)
    }

    async fn run(pool: &PgPool, ws: WorkspaceId) {
        let trace = TraceContext::root(Cause::api("recompute"));
        let mut tx = begin(pool).await.unwrap();
        let next = recompute_trait_batch(&mut tx, ws, "tier", None, &trace, None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        assert_eq!(next, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_precedence_change_is_applied_to_existing_people_by_the_batch(pool: PgPool) {
        let (ws, person) = setup(&pool).await;
        let before = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(before.traits["tier"], "shop-new");
        update_trait_definition(
            &pool,
            ws,
            "tier",
            &TraitDefinitionUpdate {
                precedence: Some(Precedence::First),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // Until the batch runs the old value stands.
        let stale = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(stale.traits["tier"], "shop-new");

        run(&pool, ws).await;

        let after = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(after.traits["tier"], "crm-old");
        assert_eq!(after.trait_sources["tier"], "crm");
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'trait.change' \
               AND reason = 'definition_changed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn archiving_removes_the_value_and_unarchiving_restores_it(pool: PgPool) {
        let (ws, person) = setup(&pool).await;
        let set = |archived| TraitDefinitionUpdate {
            archived: Some(archived),
            ..Default::default()
        };

        update_trait_definition(&pool, ws, "tier", &set(true))
            .await
            .unwrap();
        run(&pool, ws).await;
        let archived = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert!(archived.traits.get("tier").is_none());
        assert!(archived.trait_sources.get("tier").is_none());

        update_trait_definition(&pool, ws, "tier", &set(false))
            .await
            .unwrap();
        run(&pool, ws).await;
        let restored = find_person(&pool, ws, person).await.unwrap().unwrap();
        assert_eq!(restored.traits["tier"], "shop-new");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_full_batch_returns_a_cursor_and_the_next_batch_finishes(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let records: Vec<ProfileRecord> = (0..501)
            .map(|n| {
                let mut rec = record("crm", &n.to_string(), "x", 1);
                rec.identifiers = vec![("external:crm".into(), n.to_string())];
                rec
            })
            .collect();
        apply_profile_batch(&pool, ws, &records).await.unwrap();
        let trace = TraceContext::root(Cause::api("recompute"));
        let mut tx = begin(&pool).await.unwrap();

        let first = recompute_trait_batch(&mut tx, ws, "tier", None, &trace, None)
            .await
            .unwrap();
        let second = recompute_trait_batch(&mut tx, ws, "tier", first, &trace, None)
            .await
            .unwrap();
        let missing = recompute_trait_batch(&mut tx, ws, "nope", None, &trace, None)
            .await
            .unwrap();

        assert!(first.is_some());
        assert_eq!(second, None);
        assert_eq!(missing, None);
    }
}
