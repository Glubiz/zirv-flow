//! Where a template is used: the flows with a version whose graph names it.
//!
//! A node names a template in its `template` field (the send node of the sending work and the
//! webhook node's template). No node type carries the field yet, so the read looks at any node of
//! the stored graph JSON and the store never learns node types. The answer has one entry per
//! flow, so its size is bounded by the number of flows and not by how often they were published.

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// One flow that has a version naming a template.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct TemplateUse {
    pub flow_id: Uuid,
    pub flow_name: String,
    /// The flow's lifecycle state: `draft`, `published`, `paused` or `archived`.
    pub flow_state: String,
    /// The numbers of the flow's versions that name the template, newest first.
    pub versions: Vec<i32>,
    /// Whether the version the flow runs now is one of them.
    pub active: bool,
}

/// The flows of the workspace with a version whose graph has a node with `template` set to
/// `template`, by name.
pub async fn template_where_used<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    template: Uuid,
) -> Result<Vec<TemplateUse>, StoreError> {
    sqlx::query_as(
        "SELECT f.id AS flow_id, f.name AS flow_name, f.state AS flow_state, \
            array_agg(v.version ORDER BY v.version DESC) AS versions, \
            COALESCE(bool_or(f.current_version = v.version), false) AS active \
         FROM flow_versions v \
         JOIN flows f ON f.workspace_id = v.workspace_id AND f.id = v.flow_id \
         WHERE v.workspace_id = $1 AND EXISTS ( \
            SELECT 1 FROM jsonb_array_elements(CASE \
                WHEN jsonb_typeof(v.graph -> 'nodes') = 'array' THEN v.graph -> 'nodes' \
                ELSE '[]'::jsonb END) node \
            WHERE node ->> 'template' = $2) \
         GROUP BY f.id, f.name, f.state \
         ORDER BY lower(f.name), f.id",
    )
    .bind(workspace.as_uuid())
    .bind(template.to_string())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The flows that send the template now: the `published` flows whose running version names it, by
/// the rule of [`template_where_used`]. A draft, paused or archived flow is not listed; the
/// archive of a template is refused while there is one.
pub async fn active_published_flows<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    template: Uuid,
) -> Result<Vec<TemplateUse>, StoreError> {
    let mut used = template_where_used(executor, workspace, template).await?;
    used.retain(|used| used.active && used.flow_state == "published");
    Ok(used)
}

/// An active flow that sends one of a set of templates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateFlowUse {
    pub flow_id: Uuid,
    pub flow_name: String,
    /// The flow's lifecycle state.
    pub flow_state: String,
    /// The templates of the set that the flow sends, by id.
    pub template_ids: Vec<Uuid>,
}

/// The active flows that send any of `templates`: flows whose running version names one of them,
/// by the rule of [`template_where_used`]. An archived flow is not listed. By flow name.
pub async fn active_flows_using(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    templates: &[Uuid],
) -> Result<Vec<TemplateFlowUse>, StoreError> {
    let mut flows: HashMap<Uuid, TemplateFlowUse> = HashMap::new();
    for &template in templates {
        for used in template_where_used(&mut *connection, workspace, template).await? {
            if !used.active || used.flow_state == "archived" {
                continue;
            }
            flows
                .entry(used.flow_id)
                .or_insert_with(|| TemplateFlowUse {
                    flow_id: used.flow_id,
                    flow_name: used.flow_name.clone(),
                    flow_state: used.flow_state.clone(),
                    template_ids: Vec::new(),
                })
                .template_ids
                .push(template);
        }
    }
    let mut active: Vec<TemplateFlowUse> = flows.into_values().collect();
    active.sort_by(|a, b| {
        (a.flow_name.to_lowercase(), a.flow_id).cmp(&(b.flow_name.to_lowercase(), b.flow_id))
    });
    Ok(active)
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
    async fn republish_naming(pool: &PgPool, ws: WorkspaceId, flow: Uuid, template: Uuid) {
        let mut connection = pool.acquire().await.unwrap();
        let patch = FlowPatch {
            graph: Some(graph_naming(Some(template))),
            ..FlowPatch::default()
        };
        update_flow_draft(&mut connection, ws, flow, &patch)
            .await
            .unwrap();
        drop(connection);
        flows::publish(pool, ws, flow).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_is_one_entry_per_flow_with_the_versions_that_name_the_template(
        pool: PgPool,
    ) {
        let ws = flows::workspace(&pool, "one").await;
        let template = Uuid::now_v7();
        let other = Uuid::now_v7();
        let user = publish_flow_naming(&pool, ws, "Welcome", Some(template)).await;
        publish_flow_naming(&pool, ws, "Elsewhere", Some(other)).await;
        publish_flow_naming(&pool, ws, "Nothing", None).await;
        // Version 2 names it again, version 3 drops it: two old versions name it, the active one
        // does not, and the answer is still one entry for the flow.
        republish_naming(&pool, ws, user, template).await;
        flows::edit(&pool, ws, user, "other").await;
        flows::publish(&pool, ws, user).await.unwrap();
        let second = publish_flow_naming(&pool, ws, "Zeta", Some(template)).await;
        for _ in 0..3 {
            republish_naming(&pool, ws, second, template).await;
        }

        let used = template_where_used(&pool, ws, template).await.unwrap();

        assert_eq!(used.len(), 2);
        assert_eq!(used[0].flow_id, user);
        assert_eq!(used[0].flow_name, "Welcome");
        assert_eq!(
            (used[0].versions.clone(), used[0].active),
            (vec![2, 1], false)
        );
        assert_eq!(used[0].flow_state, "published");
        assert_eq!(
            (used[1].flow_id, used[1].versions.clone(), used[1].active),
            (second, vec![4, 3, 2, 1], true)
        );
        assert!(
            template_where_used(&pool, ws, Uuid::now_v7())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn where_used_marks_the_active_version_and_stays_inside_the_workspace(pool: PgPool) {
        let one = flows::workspace(&pool, "one").await;
        let two = flows::workspace(&pool, "two").await;
        let template = Uuid::now_v7();
        let flow = publish_flow_naming(&pool, one, "Welcome", Some(template)).await;

        let used = template_where_used(&pool, one, template).await.unwrap();

        assert_eq!(used.len(), 1);
        assert_eq!(
            (used[0].flow_id, used[0].versions.clone(), used[0].active),
            (flow, vec![1], true)
        );
        assert!(
            template_where_used(&pool, two, template)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_a_published_flow_running_the_template_counts_as_active(pool: PgPool) {
        use crate::FlowAction;
        use crate::templates::template_active_flow_counts;

        let ws = flows::workspace(&pool, "one").await;
        let template = Uuid::now_v7();
        let running = publish_flow_naming(&pool, ws, "Running", Some(template)).await;
        let also = publish_flow_naming(&pool, ws, "Also", Some(template)).await;
        let paused = publish_flow_naming(&pool, ws, "Paused", Some(template)).await;
        flows::lifecycle(&pool, ws, paused, FlowAction::Pause)
            .await
            .unwrap();
        let moved_on = publish_flow_naming(&pool, ws, "Moved on", Some(template)).await;
        flows::edit(&pool, ws, moved_on, "other").await;
        flows::publish(&pool, ws, moved_on).await.unwrap();
        flows::create(&pool, ws, "Draft").await;

        let blocking = active_published_flows(&pool, ws, template).await.unwrap();
        let counts = template_active_flow_counts(&pool, ws).await.unwrap();

        let ids: Vec<Uuid> = blocking.iter().map(|used| used.flow_id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&running) && ids.contains(&also));
        assert_eq!(counts.get(&template), Some(&2));
        assert_eq!(counts.len(), 1);
    }
}
