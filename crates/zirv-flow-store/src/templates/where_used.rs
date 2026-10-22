//! Where a template is used: the flow versions whose graph names it.
//!
//! A node names a template in its `template` field (the send node of the sending work and the
//! webhook node's template). No node type carries the field yet, so the read looks at any node of
//! the stored graph JSON and the store never learns node types.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// One flow version that names a template.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct TemplateUse {
    pub flow_id: Uuid,
    pub flow_name: String,
    /// The flow's lifecycle state: `draft`, `published`, `paused` or `archived`.
    pub flow_state: String,
    pub version: i32,
    /// Whether this is the version the flow runs now.
    pub current: bool,
    pub published_at: OffsetDateTime,
}

/// The flow versions of the workspace whose graph has a node with `template` set to `template`,
/// by flow name, newest version first.
pub async fn template_where_used<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    template: Uuid,
) -> Result<Vec<TemplateUse>, StoreError> {
    sqlx::query_as(
        "SELECT f.id AS flow_id, f.name AS flow_name, f.state AS flow_state, v.version, \
            COALESCE(f.current_version = v.version, false) AS current, v.published_at \
         FROM flow_versions v \
         JOIN flows f ON f.workspace_id = v.workspace_id AND f.id = v.flow_id \
         WHERE v.workspace_id = $1 AND EXISTS ( \
            SELECT 1 FROM jsonb_array_elements(CASE \
                WHEN jsonb_typeof(v.graph -> 'nodes') = 'array' THEN v.graph -> 'nodes' \
                ELSE '[]'::jsonb END) node \
            WHERE node ->> 'template' = $2) \
         ORDER BY lower(f.name), f.id, v.version DESC",
    )
    .bind(workspace.as_uuid())
    .bind(template.to_string())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support as flows;
    use crate::{FlowPatch, update_flow_draft};

    fn graph_naming(template: Option<Uuid>) -> Value {
        let mut node = json!({"id": "send", "type": "action.send"});
        if let Some(template) = template {
            node["template"] = json!(template.to_string());
        }
        json!({"v": 1, "nodes": [node, {"id": "end", "type": "exit"}], "edges": []})
    }

    async fn publish_flow_naming(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        template: Option<Uuid>,
    ) -> Uuid {
        let flow = flows::create(pool, ws, name).await;
        let mut connection = pool.acquire().await.unwrap();
        let patch = FlowPatch {
            graph: Some(graph_naming(template)),
            ..FlowPatch::default()
        };
        update_flow_draft(&mut connection, ws, flow.id, &patch)
            .await
            .unwrap();
        drop(connection);
        flows::publish(pool, ws, flow.id).await.unwrap();
        flow.id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_lists_the_flow_versions_that_name_the_template(pool: PgPool) {
        let ws = flows::workspace(&pool, "one").await;
        let template = Uuid::now_v7();
        let other = Uuid::now_v7();
        let user = publish_flow_naming(&pool, ws, "Welcome", Some(template)).await;
        publish_flow_naming(&pool, ws, "Elsewhere", Some(other)).await;
        publish_flow_naming(&pool, ws, "Nothing", None).await;
        // A second version of the first flow that drops the template: the old version still names
        // it, and only the new one is current.
        flows::edit(&pool, ws, user, "other").await;
        flows::publish(&pool, ws, user).await.unwrap();

        let used = template_where_used(&pool, ws, template).await.unwrap();

        assert_eq!(used.len(), 1);
        assert_eq!(used[0].flow_id, user);
        assert_eq!(used[0].flow_name, "Welcome");
        assert_eq!((used[0].version, used[0].current), (1, false));
        assert_eq!(used[0].flow_state, "published");
        assert!(
            template_where_used(&pool, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_marks_the_current_version_and_stays_inside_the_workspace(pool: PgPool) {
        let one = flows::workspace(&pool, "one").await;
        let two = flows::workspace(&pool, "two").await;
        let template = Uuid::now_v7();
        let flow = publish_flow_naming(&pool, one, "Welcome", Some(template)).await;

        let used = template_where_used(&pool, one, template).await.unwrap();

        assert_eq!(used.len(), 1);
        assert_eq!(
            (used[0].flow_id, used[0].version, used[0].current),
            (flow, 1, true)
        );
        assert!(
            template_where_used(&pool, two, template)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
