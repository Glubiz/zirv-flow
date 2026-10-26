//! Conditions outside a segment (a flow branch case, a block's `visible_if`), evaluated for one
//! person in the caller's transaction. The compiler lives in `zirv-flow-segments`, which this
//! crate never sees; the caller compiles each condition and passes in the SQL.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::preview::{SqlParam, SqlQuery, bind_all};
use crate::StoreError;

/// A condition lowered to SQL: `CompiledSegment::batch_sql` and its bound values.
pub type CompiledCondition = SqlQuery;

/// Whether `person` meets each condition, in the order given, in one statement. The statement
/// holds one `EXISTS` per condition over that condition's own query, with its placeholders moved
/// past those of the conditions before it (`$1`, the workspace, is shared). It runs in the
/// caller's transaction, so it sees what it has written. A person who is not in the workspace
/// meets none. No condition runs no query.
pub async fn evaluate_conditions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    conditions: &[CompiledCondition],
) -> Result<Vec<bool>, StoreError> {
    if conditions.is_empty() {
        return Ok(Vec::new());
    }
    let combined = combine(conditions, person);
    // The SQL comes from the compiler, which binds every value.
    let met: Vec<bool> = bind_all(
        sqlx::query_scalar(sqlx::AssertSqlSafe(combined.sql)),
        workspace,
        &combined.params,
    )
    .persistent(false)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(met)
}

/// One query that returns an array with one boolean per condition. Each condition's values are
/// followed by the batch of one person, as `run_person_batch` binds it.
fn combine(conditions: &[CompiledCondition], person: Uuid) -> SqlQuery {
    let mut params: Vec<SqlParam> = Vec::new();
    let mut columns = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let moved_by = params.len();
        columns.push(format!(
            "EXISTS ({})",
            shift_placeholders(&condition.sql, moved_by)
        ));
        params.extend(condition.params.iter().cloned());
        params.push(SqlParam::UuidArray(vec![person]));
    }
    SqlQuery {
        sql: format!("SELECT ARRAY[{}]", columns.join(", ")),
        params,
    }
}

/// `sql` with every placeholder above `$1` moved up by `by`.
fn shift_placeholders(sql: &str, by: usize) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut rest = sql;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let digits = rest[at + 1..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let number: Option<usize> = rest[at + 1..at + 1 + digits].parse().ok();
        match number {
            Some(n) if n >= 2 => out.push_str(&format!("${}", n + by)),
            _ => out.push_str(&rest[at..at + 1 + digits]),
        }
        rest = &rest[at + 1 + digits..];
    }
    out.push_str(rest);
    out
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

    #[test]
    fn placeholders_above_the_workspace_move_past_the_earlier_conditions() {
        let first = SqlQuery {
            sql: "a = $1 AND b = $2 AND c = ANY($3) AND d = $12".into(),
            params: vec![SqlParam::Text("x".into())],
        };
        let second = SqlQuery {
            sql: "e = $1 AND f = $2::text AND g = ANY($3)".into(),
            params: vec![SqlParam::Text("y".into())],
        };

        let combined = combine(&[first, second], Uuid::nil());

        assert_eq!(
            combined.sql,
            "SELECT ARRAY[EXISTS (a = $1 AND b = $2 AND c = ANY($3) AND d = $12), EXISTS (e = $1 AND f = $4::text AND g = ANY($5))]"
        );
        assert_eq!(combined.params.len(), 4);
        assert_eq!(shift_placeholders("x '$' $a $", 5), "x '$' $a $");
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
