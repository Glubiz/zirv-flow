//! Conditions outside a segment (a flow branch case, a block's `visible_if`), evaluated for one
//! person in the caller's transaction. The compiler lives in `zirv-flow-segments`, which this
//! crate never sees; the caller compiles each condition and passes in the SQL.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::preview::{SqlQuery, run_person_batch};
use crate::StoreError;

/// A condition lowered to SQL: `CompiledSegment::batch_sql` and its bound values.
pub type CompiledCondition = SqlQuery;

/// Whether `person` meets each condition, in the order given. All the conditions run in the
/// caller's transaction, so they see what it has written. A person who is not in the workspace
/// meets none.
pub async fn evaluate_conditions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    conditions: &[CompiledCondition],
) -> Result<Vec<bool>, StoreError> {
    let mut results = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let members = run_person_batch(&mut *connection, workspace, condition, &[person]).await?;
        results.push(members.contains(&person));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::segments::tests_support::workspace;

    fn condition(predicate: &str) -> CompiledCondition {
        SqlQuery {
            sql: format!(
                "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND p.id = ANY($2) \
                 AND {predicate}"
            ),
            params: Vec::new(),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn n_conditions_in_one_call_equal_n_separate_runs(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person =
            crate::segments::tests_support::person(&pool, ws, "a@example.test", "pro").await;
        let conditions = [
            condition("true"),
            condition("false"),
            condition("p.traits ->> 'plan' = 'pro'"),
            condition("p.traits ->> 'plan' = 'free'"),
        ];
        let mut connection = pool.acquire().await.unwrap();

        let together = evaluate_conditions(&mut connection, ws, person, &conditions)
            .await
            .unwrap();
        let mut apart = Vec::new();
        for one in &conditions {
            let single =
                evaluate_conditions(&mut connection, ws, person, std::slice::from_ref(one))
                    .await
                    .unwrap();
            apart.push(single[0]);
        }

        assert_eq!(together, [true, false, true, false]);
        assert_eq!(together, apart);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_conditions_give_no_results_and_a_stranger_meets_none(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let person =
            crate::segments::tests_support::person(&pool, other, "b@example.test", "pro").await;
        let mut connection = pool.acquire().await.unwrap();

        let none = evaluate_conditions(&mut connection, ws, person, &[])
            .await
            .unwrap();
        let stranger = evaluate_conditions(&mut connection, ws, person, &[condition("true")])
            .await
            .unwrap();

        assert!(none.is_empty());
        assert_eq!(stranger, [false]);
    }
}
