//! Running a compiled segment's count and sample queries for a preview. The store never sees the
//! compiler: it takes SQL text and typed values and runs them in the caller's snapshot
//! transaction, under its statement timeout.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::postgres::PgArguments;
use sqlx::query::QueryScalar;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::bounded::begin_snapshot_for;
use crate::people::strongest_identifiers;
use crate::{StoreError, Tx};

/// One value bound to a compiled query. The SQL casts each placeholder to the matching type.
#[derive(Clone, Debug, PartialEq)]
pub enum SqlParam {
    Text(String),
    TextArray(Vec<String>),
    Float(f64),
    FloatArray(Vec<f64>),
    Int(i64),
    Timestamp(OffsetDateTime),
    Uuid(Uuid),
    UuidArray(Vec<Uuid>),
    Bool(bool),
    Jsonb(Value),
}

/// A compiled query: `$1` is the workspace id, `params` are bound from `$2`.
#[derive(Clone, Debug, PartialEq)]
pub struct SqlQuery {
    pub sql: String,
    pub params: Vec<SqlParam>,
}

/// A person in the sample.
#[derive(Clone, PartialEq)]
pub struct SampleRow {
    pub id: Uuid,
    /// The strongest identifier as `(kind, value)`; the caller masks the value by role.
    pub display: Option<(String, String)>,
}

// The display value is personal data.
impl std::fmt::Debug for SampleRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SampleRow")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Starts the preview's transaction: read-only, one snapshot, cancelled at `timeout` per
/// statement. Preview runs on the primary (ruling 3724f651).
pub async fn begin_segment_snapshot(
    pool: &sqlx::PgPool,
    timeout: Duration,
) -> Result<Tx, StoreError> {
    begin_snapshot_for(pool, timeout).await
}

/// The database's clock, inside the snapshot.
pub async fn snapshot_now(connection: &mut PgConnection) -> Result<OffsetDateTime, StoreError> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(connection)
        .await
        .map_err(StoreError::bounded)
}

pub(super) fn bind_all<'q, O>(
    mut query: QueryScalar<'q, sqlx::Postgres, O, PgArguments>,
    workspace: WorkspaceId,
    params: &'q [SqlParam],
) -> QueryScalar<'q, sqlx::Postgres, O, PgArguments> {
    query = query.bind(workspace.as_uuid());
    for param in params {
        query = match param {
            SqlParam::Text(value) => query.bind(value.as_str()),
            SqlParam::TextArray(values) => query.bind(values.as_slice()),
            SqlParam::Float(value) => query.bind(*value),
            SqlParam::FloatArray(values) => query.bind(values.as_slice()),
            SqlParam::Int(value) => query.bind(*value),
            SqlParam::Timestamp(value) => query.bind(*value),
            SqlParam::Uuid(value) => query.bind(*value),
            SqlParam::UuidArray(values) => query.bind(values.as_slice()),
            SqlParam::Bool(value) => query.bind(*value),
            SqlParam::Jsonb(value) => query.bind(value),
        };
    }
    query
}

/// Runs the count and the sample, and finds each sampled person's display identifier. A statement
/// cancelled at the timeout is [`StoreError::is_timeout`].
pub async fn run_segment_preview(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    count: &SqlQuery,
    sample: &SqlQuery,
) -> Result<(i64, Vec<SampleRow>), StoreError> {
    // The SQL comes from the compiler, which binds every value.
    let total: i64 = bind_all(
        sqlx::query_scalar(sqlx::AssertSqlSafe(count.sql.clone())),
        workspace,
        &count.params,
    )
    .persistent(false)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let ids: Vec<Uuid> = bind_all(
        sqlx::query_scalar(sqlx::AssertSqlSafe(sample.sql.clone())),
        workspace,
        &sample.params,
    )
    .persistent(false)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::bounded)?;
    let mut display = strongest_identifiers(connection, workspace, &ids).await?;
    let rows = ids
        .into_iter()
        .map(|id| SampleRow {
            display: display.remove(&id),
            id,
        })
        .collect();
    Ok((total, rows))
}

/// TEST ONLY: runs a compiled query that selects `p.id` and returns the ids, in the order the
/// database gives them. The comparison tests of the compiler use it to read the members of a
/// segment and of a batch (`$1` is the workspace, `params` follow, a batch is the last param).
#[cfg(any(test, feature = "test-support"))]
pub async fn run_segment_ids(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    query: &SqlQuery,
) -> Result<Vec<Uuid>, StoreError> {
    // The SQL comes from the compiler, which binds every value.
    bind_all(
        sqlx::query_scalar(sqlx::AssertSqlSafe(query.sql.clone())),
        workspace,
        &query.params,
    )
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

/// Runs a compiled `batch_sql` query (`CompiledSegment::batch_sql`) for `people` and returns the
/// ones it selects: who among them is in the segment. The batch is bound as the last parameter.
/// The query reads the live tables, so it sees what the caller's transaction wrote.
pub async fn run_person_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    query: &SqlQuery,
    people: &[Uuid],
) -> Result<Vec<Uuid>, StoreError> {
    let mut params = query.params.clone();
    params.push(SqlParam::UuidArray(people.to_vec()));
    // The SQL comes from the compiler, which binds every value.
    bind_all(
        sqlx::query_scalar(sqlx::AssertSqlSafe(query.sql.clone())),
        workspace,
        &params,
    )
    .persistent(false)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::create_workspace;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn person(pool: &PgPool, ws: WorkspaceId, email: &str, plan: &str) -> Uuid {
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: email.into(),
            identifiers: vec![("email".into(), email.into())],
            traits: json!({"plan": plan}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(pool, ws, &[record]).await.unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        person_id
    }

    fn query(predicate: &str, tail: &str, params: Vec<SqlParam>) -> SqlQuery {
        SqlQuery {
            sql: format!(
                "SELECT {} FROM people p WHERE p.workspace_id = $1 AND p.merged_into IS NULL \
                 AND ({predicate}){tail}",
                if tail.is_empty() { "count(*)" } else { "p.id" }
            ),
            params,
        }
    }

    #[test]
    fn a_query_keeps_its_values_apart_from_its_text() {
        let query = query(
            "p.traits ->> 'plan' = $2::text",
            "",
            vec![SqlParam::Text("pro".into())],
        );

        assert!(!query.sql.contains("pro"));
        assert_eq!(query.params, [SqlParam::Text("pro".into())]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_count_and_the_sample_match_the_seeded_people_of_the_workspace(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let pro_a = person(&pool, ws, "a@example.com", "pro").await;
        let pro_b = person(&pool, ws, "b@example.com", "pro").await;
        person(&pool, ws, "c@example.com", "free").await;
        person(&pool, other, "d@example.com", "pro").await;
        let predicate = "p.traits ->> 'plan' = $2::text";
        let params = vec![SqlParam::Text("pro".into())];
        let count = query(predicate, "", params.clone());
        let mut sample_params = params;
        sample_params.push(SqlParam::Int(1));
        let sample = query(predicate, " ORDER BY p.id LIMIT $3::bigint", sample_params);
        let mut tx = begin_segment_snapshot(&pool, Duration::from_secs(10))
            .await
            .unwrap();

        let (total, rows) = run_segment_preview(&mut tx, ws, &count, &sample)
            .await
            .unwrap();

        assert_eq!(total, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, pro_a.min(pro_b));
        assert_eq!(
            rows[0].display.as_ref().map(|(kind, _)| kind.as_str()),
            Some("email")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_preview_leaves_no_prepared_statement_on_its_connection(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        person(&pool, ws, "a@example.com", "pro").await;
        let predicate = "p.traits ->> 'plan' = $2::text";
        let params = vec![SqlParam::Text("pro".into())];
        let count = query(predicate, "", params.clone());
        let mut sample_params = params;
        sample_params.push(SqlParam::Int(1));
        let sample = query(predicate, " ORDER BY p.id LIMIT $3::bigint", sample_params);
        let mut tx = begin_segment_snapshot(&pool, Duration::from_secs(10))
            .await
            .unwrap();

        run_segment_preview(&mut tx, ws, &count, &sample)
            .await
            .unwrap();

        // A persistent sqlx statement is a named prepared statement that the server keeps, and
        // after a few executions it plans without the bound values. Neither query may stay.
        let kept: Vec<String> = sqlx::query_scalar(
            "SELECT statement FROM pg_prepared_statements WHERE statement LIKE '%FROM people p%'",
        )
        .persistent(false)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert!(kept.is_empty(), "kept prepared statements: {kept:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_ids_of_a_query_are_those_of_its_workspace_and_a_batch_narrows_them(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let pro_a = person(&pool, ws, "a@example.com", "pro").await;
        let pro_b = person(&pool, ws, "b@example.com", "pro").await;
        person(&pool, ws, "c@example.com", "free").await;
        person(&pool, other, "d@example.com", "pro").await;
        let predicate = "p.traits ->> 'plan' = $2::text";
        let pro = SqlParam::Text("pro".into());
        let mut connection = pool.acquire().await.unwrap();

        let all = query(predicate, " ", vec![pro.clone()]);
        let mut ids = run_segment_ids(&mut connection, ws, &all).await.unwrap();
        ids.sort();
        let mut expected = vec![pro_a, pro_b];
        expected.sort();
        assert_eq!(ids, expected);

        let batch = query(
            predicate,
            " AND p.id = ANY($3::uuid[])",
            vec![pro, SqlParam::UuidArray(vec![pro_a, Uuid::nil()])],
        );
        let ids = run_segment_ids(&mut connection, ws, &batch).await.unwrap();
        assert_eq!(ids, [pro_a]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_statement_past_the_timeout_is_a_timeout_and_a_write_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        person(&pool, ws, "a@example.com", "pro").await;
        let slow = query("(SELECT pg_sleep(2)) IS NOT NULL", "", Vec::new());
        let mut tx = begin_segment_snapshot(&pool, Duration::from_millis(50))
            .await
            .unwrap();

        let error = run_segment_preview(&mut tx, ws, &slow, &slow)
            .await
            .unwrap_err();

        assert!(error.is_timeout());
        let mut tx = begin_segment_snapshot(&pool, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(snapshot_now(&mut tx).await.is_ok());
        let write = sqlx::query("DELETE FROM people").execute(&mut *tx).await;
        assert!(write.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_batch_selects_the_people_a_query_matches_in_the_open_transaction(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let pro = person(&pool, ws, "a@example.com", "pro").await;
        let free = person(&pool, ws, "b@example.com", "free").await;
        let batch = query(
            "p.traits ->> 'plan' = $2",
            " AND p.id = ANY($3::uuid[])",
            vec![SqlParam::Text("pro".into())],
        );
        let mut tx = crate::begin(&pool).await.unwrap();

        let before = run_person_batch(&mut tx, ws, &batch, &[pro, free])
            .await
            .unwrap();
        sqlx::query("UPDATE people SET traits = '{\"plan\": \"pro\"}' WHERE id = $1")
            .bind(free)
            .execute(&mut *tx)
            .await
            .unwrap();
        let mut after = run_person_batch(&mut tx, ws, &batch, &[pro, free])
            .await
            .unwrap();
        after.sort();

        let mut both = vec![pro, free];
        both.sort();
        assert_eq!(before, [pro]);
        assert_eq!(after, both, "the write of this transaction is visible");
    }
}
