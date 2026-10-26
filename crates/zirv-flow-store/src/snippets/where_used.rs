//! Where a snippet is used: the templates whose draft or current version holds a block for it, the
//! themes that use it as a footer, and the active flows that send those templates.
//!
//! A snippet block is `{"type": "snippet", "snippet_id": <id>}` at the top level of a section
//! column of a block document. The read looks for it in the stored JSON, so the store never learns
//! the block types.
//!
//! A theme's footer is appended to every email that follows the theme, so when the default theme
//! uses the snippet, the templates that follow the default count as using it too.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::crud::SnippetTemplate;
use crate::StoreError;
use crate::templates::{TemplateFlowUse, active_flows_using};
use crate::themes::{default_following_templates, is_default_theme};

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

/// Everything that uses a snippet.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SnippetUse {
    pub templates: Vec<SnippetTemplate>,
    pub themes: Vec<SnippetThemeUse>,
    pub active_flows: Vec<TemplateFlowUse>,
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

/// The themes of the workspace, not archived, that name the snippet as their footer, by name. With
/// `published_only`, only a theme whose current version does; else a draft naming it counts too.
pub(super) async fn themes_using<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    snippet: Uuid,
    published_only: bool,
) -> Result<Vec<SnippetThemeUse>, StoreError> {
    sqlx::query_as(
        "SELECT t.id, t.name FROM themes t \
         WHERE t.workspace_id = $1 AND t.archived_at IS NULL AND ( \
            (NOT $3 AND EXISTS (SELECT 1 FROM theme_drafts d WHERE d.theme_id = t.id \
                AND d.tokens ->> 'footer_snippet' = $2)) \
            OR EXISTS (SELECT 1 FROM theme_versions v WHERE v.theme_id = t.id \
                AND v.version = t.current_version AND v.tokens ->> 'footer_snippet' = $2)) \
         ORDER BY lower(t.name), t.id",
    )
    .bind(workspace.as_uuid())
    .bind(snippet.to_string())
    .bind(published_only)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Where the snippet is used. When the default theme uses it as its footer, the templates that
/// follow the default are listed among the templates. An active flow is one whose running version
/// names one of the templates, by the rule of [`crate::template_where_used`]; an archived flow is
/// not.
pub async fn snippet_where_used(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    snippet: Uuid,
) -> Result<SnippetUse, StoreError> {
    let mut templates = templates_holding(&mut *connection, workspace, snippet).await?;
    let themes = themes_using(&mut *connection, workspace, snippet, false).await?;
    let mut default_in_use = false;
    for theme in &themes {
        default_in_use |= is_default_theme(&mut *connection, workspace, theme.id).await?;
    }
    if default_in_use {
        for follower in default_following_templates(&mut *connection, workspace).await? {
            match templates.iter_mut().find(|held| held.id == follower.id) {
                Some(held) => {
                    held.in_draft |= follower.in_draft;
                    held.in_current |= follower.in_current;
                }
                None => templates.push(SnippetTemplate {
                    id: follower.id,
                    name: follower.name,
                    in_draft: follower.in_draft,
                    in_current: follower.in_current,
                }),
            }
        }
        templates.sort_by_key(|template| (template.name.to_lowercase(), template.id));
    }
    let ids: Vec<Uuid> = templates.iter().map(|template| template.id).collect();
    let active_flows = active_flows_using(connection, workspace, &ids).await?;
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
        let Err(SnippetError::InUse {
            templates: list, ..
        }) = refused
        else {
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

    /// A snippet with a published version, used as the footer of a published theme.
    async fn theme_with_footer(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        snippet: Uuid,
        publish_it: bool,
    ) -> crate::Theme {
        use crate::themes::tests_support::{create as create_theme, publish as publish_theme};
        let theme = create_theme(pool, ws, name).await;
        let mut connection = pool.acquire().await.unwrap();
        crate::save_theme_draft(
            &mut connection,
            ws,
            theme.id,
            &json!({"footer_snippet": snippet.to_string()}),
            None,
            None,
        )
        .await
        .unwrap();
        drop(connection);
        if publish_it {
            publish_theme(pool, ws, theme.id).await;
        }
        theme
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_theme_footer_blocks_the_archive_and_a_draft_one_does_not(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let footer = create(&pool, ws, "Footer").await;
        let spare = create(&pool, ws, "Spare").await;
        let published = theme_with_footer(&pool, ws, "Live", footer.id, true).await;
        theme_with_footer(&pool, ws, "Draft only", spare.id, false).await;
        let mut connection = pool.acquire().await.unwrap();

        let refused = archive_snippet(&mut connection, ws, footer.id).await;
        let allowed = archive_snippet(&mut connection, ws, spare.id).await;

        let Err(SnippetError::InUse { templates, themes }) = refused else {
            panic!("expected InUse");
        };
        assert!(templates.is_empty());
        assert_eq!(
            themes.iter().map(|t| t.id).collect::<Vec<_>>(),
            [published.id]
        );
        assert!(allowed.is_ok());
        crate::archive_theme(&mut connection, ws, published.id)
            .await
            .unwrap();
        assert!(
            archive_snippet(&mut connection, ws, footer.id)
                .await
                .is_ok()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_default_themes_followers_use_the_footer_and_their_flows_are_active(pool: PgPool) {
        use crate::flows::tests_support as flows;
        let ws = flows::workspace(&pool, "one").await;
        let footer = create(&pool, ws, "Footer").await;
        let theme = theme_with_footer(&pool, ws, "Brand", footer.id, false).await;
        let follower = create_template(&pool, ws, "Follower").await;
        let named = create_template(&pool, ws, "Named").await;
        let mut document = json!({"schema": 2, "channel": "email", "mode": "blocks",
            "blocks": {"subject": "Hi", "sections": []}});
        set_draft(&pool, ws, follower.id, &document).await;
        document["blocks"]["theme"] = json!({"id": theme.id});
        set_draft(&pool, ws, named.id, &document).await;
        let flow = flows::create(&pool, ws, "Sends").await;
        let graph = json!({"v": 1, "nodes": [
            {"id": "send", "type": "action.send", "template": follower.id.to_string()},
            {"id": "end", "type": "exit"}], "edges": []});
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
        // Not the default yet: the theme is listed, its followers are not.
        let before = used(&pool, ws, footer.id).await;
        crate::themes::tests_support::publish(&pool, ws, theme.id).await;
        crate::set_default_theme(&mut pool.acquire().await.unwrap(), ws, theme.id)
            .await
            .unwrap();

        let after = used(&pool, ws, footer.id).await;

        assert_eq!(
            before.themes.iter().map(|t| t.id).collect::<Vec<_>>(),
            [theme.id]
        );
        assert!(before.templates.is_empty() && before.active_flows.is_empty());
        assert_eq!(
            after.templates.iter().map(|t| t.id).collect::<Vec<_>>(),
            [follower.id]
        );
        assert_eq!(
            after
                .active_flows
                .iter()
                .map(|f| f.flow_id)
                .collect::<Vec<_>>(),
            [flow.id]
        );
    }
}
