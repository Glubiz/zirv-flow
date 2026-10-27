//! Locking people rows in id order, by primary key.
//!
//! The lock is taken on `people_pkey` and the workspace is checked on the locked rows, so the
//! plan never depends on planner statistics: a filter on `workspace_id` beside `id = ANY(..)`
//! lets a table without statistics (after a crash or a restore) choose `people_workspace_idx`
//! and walk the workspace's people to find the few that were asked for.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// The row-level lock a caller takes on the people it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersonLock {
    /// Keeps the row from being deleted or its key changed; other updates may go on.
    KeyShare,
    /// Keeps the row from being updated or deleted.
    Share,
    /// Excludes every other writer.
    Update,
}

impl PersonLock {
    /// The statement for this lock: `$1` is the workspace, `$2` the person ids. The ids are
    /// locked in a materialised step that sees no other condition, then the workspace is
    /// checked; a person of another workspace is neither returned nor acted on.
    pub(crate) const fn sql(self) -> &'static str {
        match self {
            Self::KeyShare => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR KEY SHARE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
            Self::Share => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR SHARE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
            Self::Update => {
                "WITH locked AS MATERIALIZED ( \
                     SELECT id, workspace_id, merged_into FROM people \
                     WHERE id = ANY($2) ORDER BY id FOR UPDATE) \
                 SELECT id, merged_into FROM locked WHERE workspace_id = $1 ORDER BY id"
            }
        }
    }
}

/// Locks the people of `workspace` among `ids` in id order, and returns each with the person it
/// was merged into. A missing person, and one of another workspace, is absent.
pub(crate) async fn lock_people_in_order(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
    lock: PersonLock,
) -> Result<Vec<(Uuid, Option<Uuid>)>, StoreError> {
    sqlx::query_as(lock.sql())
        .bind(workspace.as_uuid())
        .bind(ids)
        .fetch_all(connection)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn insert_people(pool: &PgPool, workspace: WorkspaceId, ids: &[Uuid]) {
        sqlx::query("INSERT INTO people (id, workspace_id) SELECT unnest($2::uuid[]), $1")
            .bind(workspace.as_uuid())
            .bind(ids)
            .execute(pool)
            .await
            .unwrap();
    }

    fn node_types(plan: &Value, out: &mut Vec<(String, Option<String>)>) {
        if let Some(object) = plan.as_object() {
            if let Some(kind) = object.get("Node Type").and_then(Value::as_str) {
                let index = object
                    .get("Index Name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                out.push((kind.to_owned(), index));
            }
            object.values().for_each(|child| node_types(child, out));
        } else if let Some(array) = plan.as_array() {
            array.iter().for_each(|child| node_types(child, out));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lock_plan_uses_the_primary_key_on_a_table_without_statistics(pool: PgPool) {
        let ws = workspace(&pool, "plan").await;
        // Enough rows that a sequential scan or the workspace index would be tempting, and
        // never analysed: a fresh database has no statistics.
        let ids: Vec<Uuid> = (0..5000).map(|_| Uuid::now_v7()).collect();
        insert_people(&pool, ws, &ids).await;
        let analysed: Option<time::OffsetDateTime> = sqlx::query_scalar(
            "SELECT coalesce(last_analyze, last_autoanalyze) FROM pg_stat_user_tables \
             WHERE relname = 'people'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(analysed.is_none(), "the table must have no statistics");
        for lock in [PersonLock::KeyShare, PersonLock::Share, PersonLock::Update] {
            let sql = format!("EXPLAIN (FORMAT JSON) {}", lock.sql());
            let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(ws.as_uuid())
                .bind(&ids[..2])
                .fetch_one(&pool)
                .await
                .unwrap();
            let mut nodes = Vec::new();
            node_types(&plan, &mut nodes);
            let indexes: Vec<&str> = nodes
                .iter()
                .filter_map(|(_, index)| index.as_deref())
                .collect();
            assert_eq!(indexes, ["people_pkey"], "{lock:?}: {plan}");
            assert!(
                nodes.iter().all(|(kind, _)| !kind.contains("Seq Scan")),
                "{lock:?}: {plan}"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lock_returns_the_workspaces_people_in_id_order_with_their_merge(pool: PgPool) {
        let ws = workspace(&pool, "mine").await;
        let other = workspace(&pool, "theirs").await;
        let mut ids: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        ids.sort_unstable();
        let foreign = Uuid::now_v7();
        insert_people(&pool, ws, &ids).await;
        insert_people(&pool, other, &[foreign]).await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(ids[0])
            .bind(ids[2])
            .execute(&pool)
            .await
            .unwrap();
        let asked = [ids[2], foreign, ids[0], Uuid::now_v7(), ids[1]];
        for lock in [PersonLock::KeyShare, PersonLock::Share, PersonLock::Update] {
            let mut connection = pool.acquire().await.unwrap();

            let locked = lock_people_in_order(&mut connection, ws, &asked, lock)
                .await
                .unwrap();

            assert_eq!(
                locked,
                [(ids[0], Some(ids[2])), (ids[1], None), (ids[2], None)],
                "{lock:?}"
            );
        }
    }
}
