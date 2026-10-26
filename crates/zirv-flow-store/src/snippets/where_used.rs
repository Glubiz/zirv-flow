//! Where a snippet is used: the templates whose draft or current version holds a block for it, the
//! themes that use it as a footer, and the active flows that send those templates.
//!
//! A snippet block is `{"type": "snippet", "snippet_id": <id>}` at the top level of a section
//! column of a block document. The read looks for it in the stored JSON, so the store never learns
//! the block types.

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::crud::SnippetTemplate;
use crate::{StoreError, template_where_used};

/// The path of a snippet block for `$id` in a block document.
macro_rules! block_path {
    () => {
        "'$.blocks.sections[*].columns[*][*] ? (@.type == \"snippet\" && @.snippet_id == $id)'"
    };
}

/// A theme that uses a snippet as its footer.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct SnippetThemeUse {
    pub id: Uuid,
    pub name: String,
}

/// An active flow that sends a template holding the snippet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnippetFlowUse {
    pub flow_id: Uuid,
    pub flow_name: String,
    /// The flow's lifecycle state.
    pub flow_state: String,
    /// The templates of the flow that hold the snippet, by id.
    pub template_ids: Vec<Uuid>,
}

/// Everything that uses a snippet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnippetUse {
    pub templates: Vec<SnippetTemplate>,
    pub themes: Vec<SnippetThemeUse>,
    pub active_flows: Vec<SnippetFlowUse>,
}

/// The templates of the workspace, not archived, whose draft or current version holds a block for
/// the snippet, by name.
pub(super) async fn templates_holding<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    snippet: Uuid,
) -> Result<Vec<SnippetTemplate>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT id, name, in_draft, in_current FROM ( \
            SELECT t.id, t.name, \
                EXISTS (SELECT 1 FROM template_drafts d WHERE d.template_id = t.id AND \
                    jsonb_path_exists(d.document, ",
        block_path!(),
        ", jsonb_build_object('id', $2::text))) AS in_draft, \
                EXISTS (SELECT 1 FROM template_versions v WHERE v.template_id = t.id \
                    AND v.version = t.current_version AND \
                    jsonb_path_exists(v.document, ",
        block_path!(),
        ", jsonb_build_object('id', $2::text))) AS in_current \
            FROM templates t WHERE t.workspace_id = $1 AND t.archived_at IS NULL) held \
         WHERE in_draft OR in_current ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(snippet.to_string())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The snippet ids a block document names, by the rule of [`block_path!`] without its id test.
macro_rules! named_snippets {
    ($document:literal) => {
        concat!(
            "jsonb_path_query(",
            $document,
            ", '$.blocks.sections[*].columns[*][*] ? (@.type == \"snippet\").snippet_id') #>> '{}'"
        )
    };
}

/// How many templates of the workspace, not archived, hold a block for each snippet in their draft
/// or current version. A snippet no template holds is absent. One grouped read, not one per
/// snippet.
pub async fn snippet_template_counts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<HashMap<Uuid, i64>, StoreError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(concat!(
        "SELECT snippet, count(*) FROM ( \
            SELECT d.template_id, ",
        named_snippets!("d.document"),
        " AS snippet FROM template_drafts d JOIN templates t ON t.id = d.template_id \
              WHERE t.workspace_id = $1 AND t.archived_at IS NULL \
            UNION \
            SELECT v.template_id, ",
        named_snippets!("v.document"),
        " AS snippet FROM template_versions v JOIN templates t ON t.id = v.template_id \
              AND v.version = t.current_version \
              WHERE t.workspace_id = $1 AND t.archived_at IS NULL \
         ) held GROUP BY snippet"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .filter_map(|(snippet, count)| Some((snippet.parse().ok()?, count)))
        .collect())
}

/// The themes that use the snippet as their footer. No themes exist yet: #304 adds the table and
/// this read, and the templates that follow a default theme which uses the snippet.
async fn themes_using(
    _connection: &mut PgConnection,
    _workspace: WorkspaceId,
    _snippet: Uuid,
) -> Result<Vec<SnippetThemeUse>, StoreError> {
    Ok(Vec::new())
}

/// Where the snippet is used. An active flow is one whose running version names a template that
/// holds the snippet, by the rule of [`template_where_used`]; an archived flow is not.
pub async fn snippet_where_used(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    snippet: Uuid,
) -> Result<SnippetUse, StoreError> {
    let templates = templates_holding(&mut *connection, workspace, snippet).await?;
    let themes = themes_using(connection, workspace, snippet).await?;
    let mut flows: HashMap<Uuid, SnippetFlowUse> = HashMap::new();
    for template in &templates {
        for used in template_where_used(&mut *connection, workspace, template.id).await? {
            if !used.active || used.flow_state == "archived" {
                continue;
            }
            flows
                .entry(used.flow_id)
                .or_insert_with(|| SnippetFlowUse {
                    flow_id: used.flow_id,
                    flow_name: used.flow_name.clone(),
                    flow_state: used.flow_state.clone(),
                    template_ids: Vec::new(),
                })
                .template_ids
                .push(template.id);
        }
    }
    let mut active_flows: Vec<SnippetFlowUse> = flows.into_values().collect();
    active_flows.sort_by(|a, b| {
        (a.flow_name.to_lowercase(), a.flow_id).cmp(&(b.flow_name.to_lowercase(), b.flow_id))
    });
    Ok(SnippetUse {
        templates,
        themes,
        active_flows,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::snippets::tests_support::create;
    use crate::snippets::{SnippetError, archive_snippet};
    use crate::templates::tests_support::{create as create_template, workspace};
    use crate::{FlowPatch, save_template_draft, update_flow_draft};

    fn holding(snippet: Uuid, nested: bool) -> Value {
        let block = json!({"type": "snippet", "id": "s", "snippet_id": snippet.to_string()});
        let columns = if nested {
            json!([[{"type": "divider", "id": "d"}], [block]])
        } else {
            json!([[block]])
        };
        json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": {
            "subject": "Hi", "sections": [{"id": "sec", "columns": columns}]}})
    }

    async fn set_draft(pool: &PgPool, ws: WorkspaceId, template: Uuid, document: &Value) {
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(&mut connection, ws, template, document, None)
            .await
            .unwrap();
    }

    async fn used(pool: &PgPool, ws: WorkspaceId, snippet: Uuid) -> SnippetUse {
        let mut connection = pool.acquire().await.unwrap();
        snippet_where_used(&mut connection, ws, snippet)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_block_in_a_section_column_is_found_in_the_draft_and_the_current_version(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let other = create(&pool, ws, "Other").await;
        let plain = create_template(&pool, ws, "Plain").await;
        let first = create_template(&pool, ws, "First").await;
        let second = create_template(&pool, ws, "Second").await;
        set_draft(&pool, ws, first.id, &holding(snippet.id, true)).await;
        set_draft(&pool, ws, second.id, &holding(snippet.id, false)).await;
        // `second` publishes the block and then drops it from its draft.
        sqlx::query(
            "INSERT INTO template_versions (workspace_id, template_id, version, document, \
                validation, trace_id) \
             SELECT workspace_id, template_id, 1, document, '[]'::jsonb, gen_random_uuid() \
             FROM template_drafts WHERE template_id = $1",
        )
        .bind(second.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE templates SET current_version = 1 WHERE id = $1")
            .bind(second.id)
            .execute(&pool)
            .await
            .unwrap();
        set_draft(&pool, ws, second.id, &holding(other.id, false)).await;

        let found = used(&pool, ws, snippet.id).await;

        let by_name: Vec<(&str, bool, bool)> = found
            .templates
            .iter()
            .map(|t| (t.name.as_str(), t.in_draft, t.in_current))
            .collect();
        assert_eq!(by_name, [("First", true, false), ("Second", false, true)]);
        assert!(found.themes.is_empty());
        assert!(!found.templates.iter().any(|t| t.id == plain.id));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_counts_cover_draft_and_current_templates_and_skip_archived_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let unused = create(&pool, ws, "Unused").await;
        let in_draft = create_template(&pool, ws, "Draft").await;
        let in_current = create_template(&pool, ws, "Current").await;
        let archived = create_template(&pool, ws, "Gone").await;
        for template in [&in_draft, &in_current, &archived] {
            set_draft(&pool, ws, template.id, &holding(snippet.id, false)).await;
        }
        // `in_current` publishes the block and then drops it from its draft.
        sqlx::query(
            "INSERT INTO template_versions (workspace_id, template_id, version, document, \
                validation, trace_id) \
             SELECT workspace_id, template_id, 1, document, '[]'::jsonb, gen_random_uuid() \
             FROM template_drafts WHERE template_id = $1",
        )
        .bind(in_current.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE templates SET current_version = 1 WHERE id = $1")
            .bind(in_current.id)
            .execute(&pool)
            .await
            .unwrap();
        set_draft(&pool, ws, in_current.id, &holding(Uuid::now_v7(), false)).await;
        let mut connection = pool.acquire().await.unwrap();
        crate::archive_template(&mut connection, ws, archived.id)
            .await
            .unwrap();
        drop(connection);

        let counts = snippet_template_counts(&pool, ws).await.unwrap();

        assert_eq!(counts.get(&snippet.id), Some(&2));
        assert_eq!(counts.get(&unused.id), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_template_is_not_listed(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let template = create_template(&pool, ws, "Gone").await;
        set_draft(&pool, ws, template.id, &holding(snippet.id, false)).await;
        let mut connection = pool.acquire().await.unwrap();
        crate::archive_template(&mut connection, ws, template.id)
            .await
            .unwrap();
        drop(connection);

        assert!(used(&pool, ws, snippet.id).await.templates.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_a_flow_that_runs_the_template_now_is_active(pool: PgPool) {
        use crate::flows::tests_support as flows;
        let ws = flows::workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let template = create_template(&pool, ws, "Mail").await;
        set_draft(&pool, ws, template.id, &holding(snippet.id, false)).await;
        let graph = |template: Option<Uuid>| {
            let mut node = json!({"id": "send", "type": "action.send"});
            if let Some(template) = template {
                node["template"] = json!(template.to_string());
            }
            json!({"v": 1, "nodes": [node, {"id": "end", "type": "exit"}], "edges": []})
        };
        let publish = |name: &'static str, template: Option<Uuid>| {
            let pool = pool.clone();
            let graph = graph(template);
            async move {
                let flow = flows::create(&pool, ws, name).await;
                let mut connection = pool.acquire().await.unwrap();
                let patch = FlowPatch {
                    graph: Some(graph),
                    ..FlowPatch::default()
                };
                update_flow_draft(&mut connection, ws, flow.id, &patch)
                    .await
                    .unwrap();
                drop(connection);
                flows::publish(&pool, ws, flow.id).await.unwrap();
                flow.id
            }
        };
        let running = publish("Running", Some(template.id)).await;
        // Its draft names the template, but no published version does.
        let unpublished = flows::create(&pool, ws, "Draft only").await;
        let _ = unpublished;
        // A flow that stopped naming the template in its newest version.
        let moved_on = publish("Moved on", Some(template.id)).await;
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            moved_on,
            &FlowPatch {
                graph: Some(graph(None)),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        flows::publish(&pool, ws, moved_on).await.unwrap();

        let found = used(&pool, ws, snippet.id).await;

        let flow_ids: Vec<Uuid> = found.active_flows.iter().map(|f| f.flow_id).collect();
        assert_eq!(flow_ids, [running]);
        assert_eq!(found.active_flows[0].template_ids, [template.id]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_template_draft_blocks_the_archive_until_it_lets_go(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let template = create_template(&pool, ws, "Mail").await;
        set_draft(&pool, ws, template.id, &holding(snippet.id, false)).await;
        let mut connection = pool.acquire().await.unwrap();

        let refused = archive_snippet(&mut connection, ws, snippet.id).await;
        let Err(SnippetError::InUse(list)) = refused else {
            panic!("expected InUse");
        };
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), [template.id]);

        let mut other = pool.acquire().await.unwrap();
        save_template_draft(
            &mut other,
            ws,
            template.id,
            &holding(Uuid::now_v7(), false),
            None,
        )
        .await
        .unwrap();
        assert!(
            archive_snippet(&mut connection, ws, snippet.id)
                .await
                .is_ok()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_snippet_tables_hold_no_person_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('snippets', 'snippet_drafts', 'snippet_versions') \
               AND column_name LIKE '%person%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }
}
