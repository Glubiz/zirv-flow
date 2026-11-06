//! Where a destination is used: the published flows whose running version has a node naming it.
//!
//! A node names a destination in its `destination` field (the send node and the sync node). Like
//! the template read, this looks at any node of the stored graph JSON, so the store never learns
//! node types. The answer has one entry per flow.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A published flow whose running version names a destination.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DestinationFlowUse {
    pub flow_id: Uuid,
    pub flow_name: String,
}

/// The `published` flows of the workspace whose running version has a node with `destination` set
/// to `destination`, by name. A draft, paused or archived flow is not listed, nor is a flow whose
/// running version no longer names it; the archive of a destination is refused while there is one.
pub async fn active_published_flows_using_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    destination: Uuid,
) -> Result<Vec<DestinationFlowUse>, StoreError> {
    sqlx::query_as(
        "SELECT f.id AS flow_id, f.name AS flow_name \
         FROM flows f \
         JOIN flow_versions v ON v.workspace_id = f.workspace_id AND v.flow_id = f.id \
            AND v.version = f.current_version \
         WHERE f.workspace_id = $1 AND f.state = 'published' AND EXISTS ( \
            SELECT 1 FROM jsonb_array_elements(CASE \
                WHEN jsonb_typeof(v.graph -> 'nodes') = 'array' THEN v.graph -> 'nodes' \
                ELSE '[]'::jsonb END) node \
            WHERE node ->> 'destination' = $2) \
         ORDER BY lower(f.name), f.id",
    )
    .bind(workspace.as_uuid())
    .bind(destination.to_string())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// An active forwarding rule that delivers to a destination.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct DestinationRuleUse {
    pub rule_id: Uuid,
    pub rule_name: String,
}

/// The active forwarding rules of the workspace that deliver to `destination`, by name. An
/// inactive rule is not listed; the archive of a destination is refused while there is one,
/// because an active rule of an archived destination is silently dropped.
pub async fn active_rules_using_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    destination: Uuid,
) -> Result<Vec<DestinationRuleUse>, StoreError> {
    sqlx::query_as(
        "SELECT id AS rule_id, name AS rule_name FROM forwarding_rules \
         WHERE workspace_id = $1 AND destination_id = $2 AND active \
         ORDER BY lower(name), id",
    )
    .bind(workspace.as_uuid())
    .bind(destination)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Locks the destinations among `ids` `FOR SHARE` for the rest of the transaction, in id
/// order. A flow publish calls it before it validates the destinations it names: an archive in
/// progress (which holds the row exclusively) makes the publish wait and then see the destination
/// archived, and a publish in progress makes an archive wait and then see the flow. An id that
/// names no destination is skipped.
pub async fn lock_destinations_shared(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    ids: &[Uuid],
) -> Result<(), StoreError> {
    sqlx::query(
        "SELECT id FROM destinations WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(ids)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("ws").unwrap();
        let slug = Slug::parse("ws").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    /// Inserts a flow row in `state` with a version 1 that holds `graph`; a draft has no running version.
    async fn flow(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        state: &str,
        graph: serde_json::Value,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO flows (id, workspace_id, name, state, draft_graph, draft_settings, \
                current_version, archived_at) \
             VALUES ($1, $2, $3, $4, '{}'::jsonb, '{}'::jsonb, \
                CASE WHEN $4 = 'draft' THEN NULL ELSE 1 END, \
                CASE WHEN $4 = 'archived' THEN now() END)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(name)
        .bind(state)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, trace_id) \
             VALUES ($1, $2, 1, $3, '{}'::jsonb, $4)",
        )
        .bind(ws.as_uuid())
        .bind(id)
        .bind(graph)
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_published_flows_running_a_version_that_names_the_destination_are_listed(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        let named = Uuid::now_v7();
        let other = Uuid::now_v7();
        let uses = |destination: Uuid| json!({"v": 1, "nodes": [{"id": "s", "type": "action.send", "destination": destination}]});
        let published = flow(&pool, ws, "Beta", "published", uses(named)).await;
        let first = flow(&pool, ws, "alpha", "published", uses(named)).await;
        flow(&pool, ws, "Paused", "paused", uses(named)).await;
        flow(&pool, ws, "Draft", "draft", uses(named)).await;
        flow(&pool, ws, "Gone", "archived", uses(named)).await;
        flow(&pool, ws, "Elsewhere", "published", uses(other)).await;

        let found = active_published_flows_using_destination(&pool, ws, named)
            .await
            .unwrap();

        let ids: Vec<Uuid> = found.iter().map(|used| used.flow_id).collect();
        assert_eq!(ids, [first, published]);
        assert_eq!(found[0].flow_name, "alpha");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_of_another_workspace_is_not_listed(pool: PgPool) {
        let ws = workspace(&pool).await;
        let name = WorkspaceName::parse("two").unwrap();
        let slug = Slug::parse("two").unwrap();
        let two = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let destination = Uuid::now_v7();
        let graph = json!({"v": 1, "nodes": [{"id": "s", "destination": destination}]});
        flow(&pool, two, "Theirs", "published", graph).await;

        let found = active_published_flows_using_destination(&pool, ws, destination)
            .await
            .unwrap();

        assert!(found.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_older_version_that_names_the_destination_does_not_count(pool: PgPool) {
        let ws = workspace(&pool).await;
        let named = Uuid::now_v7();
        let flow_id = flow(
            &pool,
            ws,
            "Moved on",
            "published",
            json!({"v": 1, "nodes": [{"id": "s", "destination": named}]}),
        )
        .await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, trace_id) \
             VALUES ($1, $2, 2, $3, '{}'::jsonb, $4)",
        )
        .bind(ws.as_uuid())
        .bind(flow_id)
        .bind(json!({"v": 1, "nodes": [{"id": "s", "destination": Uuid::now_v7()}]}))
        .bind(Uuid::now_v7())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE flows SET current_version = 2 WHERE id = $1")
            .bind(flow_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let found = active_published_flows_using_destination(&pool, ws, named)
            .await
            .unwrap();

        assert!(found.is_empty());
    }

    async fn destination(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let id = Uuid::now_v7();
        crate::create_destination(
            pool,
            ws,
            &crate::NewDestination {
                id,
                kind: "webhook",
                name: &format!("D {id}"),
                config: &json!({"url": "https://hooks.example.com/h"}),
                credential_id: None,
                rate: 10,
                burst: 10,
                created_by: None,
            },
        )
        .await
        .unwrap();
        id
    }

    async fn rule(pool: &PgPool, ws: WorkspaceId, destination: Uuid, name: &str, active: bool) {
        sqlx::query(
            "INSERT INTO forwarding_rules \
                (id, workspace_id, destination_id, name, event_name, mapping, active) \
             VALUES ($1, $2, $3, $4, 'order.completed', '{}'::jsonb, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(name)
        .bind(active)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_active_rules_of_that_destination_are_listed_by_name(pool: PgPool) {
        let ws = workspace(&pool).await;
        let used = destination(&pool, ws).await;
        let other = destination(&pool, ws).await;
        rule(&pool, ws, used, "b rule", true).await;
        rule(&pool, ws, used, "A rule", true).await;
        rule(&pool, ws, used, "off", false).await;
        rule(&pool, ws, other, "elsewhere", true).await;

        let found = active_rules_using_destination(&pool, ws, used)
            .await
            .unwrap();

        let names: Vec<&str> = found.iter().map(|rule| rule.rule_name.as_str()).collect();
        assert_eq!(names, ["A rule", "b rule"]);
    }
}
