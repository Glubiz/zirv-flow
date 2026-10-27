//! Where a theme is used: the templates whose draft or current version names it, the templates
//! that follow the workspace default (listed apart, and only for the default theme), and the
//! active flows that send those templates.
//!
//! A block document names its theme at `blocks.theme`: `"default"`, `"none"` or `{"id": <id>}`.
//! The read looks for it in the stored JSON, so the store never learns the block types. A
//! document follows the default when it is a schema 2 block document whose theme is `"default"`
//! or absent; stored schema 1 content migrates to `"none"`, and code-mode content has no theme,
//! so neither follows.

use std::collections::BTreeMap;

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{Acquire, PgConnection, PgExecutor, Postgres};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;
use crate::templates::{LiveTemplate, TemplateFlowUse, active_flows_using};

/// Whether the block document in `$doc` follows the workspace default theme.
macro_rules! follows_default {
    ($doc:literal) => {
        concat!(
            "(CASE WHEN jsonb_typeof(",
            $doc,
            " -> 'schema') = 'number' THEN (",
            $doc,
            " ->> 'schema')::numeric >= 2 ELSE false END \
              AND ",
            $doc,
            " ->> 'mode' = 'blocks' \
              AND COALESCE(",
            $doc,
            " #>> '{blocks,theme}', 'default') = 'default')"
        )
    };
}

pub(crate) use follows_default;

/// Whether the document in `$doc` names the theme in `$2`.
macro_rules! names_theme {
    ($doc:literal) => {
        concat!($doc, " #>> '{blocks,theme,id}' = $2")
    };
}

/// The theme the document in `$doc` names, or null when it names none.
macro_rules! named_theme {
    ($doc:literal) => {
        concat!($doc, " #>> '{blocks,theme,id}'")
    };
}

/// A template that is not archived and uses a theme in its draft or its current version.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct ThemeTemplate {
    pub id: Uuid,
    pub name: String,
    /// Whether the template's draft uses the theme.
    pub in_draft: bool,
    /// Whether the template's current version uses the theme.
    pub in_current: bool,
}

/// Everything that uses a theme.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ThemeUse {
    /// Templates whose draft or current version names the theme.
    pub named_templates: Vec<ThemeTemplate>,
    /// Templates that follow the workspace default: empty unless the theme is the default.
    pub default_templates: Vec<ThemeTemplate>,
    /// Active flows that send a template of either list.
    pub active_flows: Vec<TemplateFlowUse>,
}

/// The templates, not archived, that name the theme, by name.
async fn named_templates<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    theme: Uuid,
) -> Result<Vec<ThemeTemplate>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT id, name, in_draft, in_current FROM ( \
            SELECT t.id, t.name, \
                EXISTS (SELECT 1 FROM template_drafts d WHERE d.template_id = t.id AND ",
        names_theme!("d.document"),
        ") AS in_draft, \
                EXISTS (SELECT 1 FROM template_versions v WHERE v.template_id = t.id \
                    AND v.version = t.current_version AND ",
        names_theme!("v.document"),
        ") AS in_current \
            FROM templates t WHERE t.workspace_id = $1 AND t.archived_at IS NULL) used \
         WHERE in_draft OR in_current ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(theme.to_string())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The templates of the workspace, not archived, whose draft or current version follows the
/// default theme, by name.
pub(crate) async fn default_following_templates<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<ThemeTemplate>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT id, name, in_draft, in_current FROM ( \
            SELECT t.id, t.name, \
                EXISTS (SELECT 1 FROM template_drafts d WHERE d.template_id = t.id AND ",
        follows_default!("d.document"),
        ") AS in_draft, \
                EXISTS (SELECT 1 FROM template_versions v WHERE v.template_id = t.id \
                    AND v.version = t.current_version AND ",
        follows_default!("v.document"),
        ") AS in_current \
            FROM templates t WHERE t.workspace_id = $1 AND t.archived_at IS NULL) used \
         WHERE in_draft OR in_current ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Whether the theme is the workspace default.
pub(crate) async fn is_default_theme<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    theme: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workspace_default_theme \
         WHERE workspace_id = $1 AND theme_id = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(theme)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// Where the theme is used. The default templates are the ones that follow the default and are
/// listed only while this theme is the default. An active flow is one whose running version names
/// one of the templates, by the rule of [`crate::template_where_used`].
pub async fn theme_where_used(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    theme: Uuid,
) -> Result<ThemeUse, StoreError> {
    let named_templates = named_templates(&mut *connection, workspace, theme).await?;
    let default_templates = if is_default_theme(&mut *connection, workspace, theme).await? {
        default_following_templates(&mut *connection, workspace).await?
    } else {
        Vec::new()
    };
    let mut ids: Vec<Uuid> = named_templates
        .iter()
        .chain(&default_templates)
        .map(|template| template.id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let active_flows = active_flows_using(connection, workspace, &ids).await?;
    Ok(ThemeUse {
        named_templates,
        default_templates,
        active_flows,
    })
}

/// The templates, not archived, whose current published version renders with the theme: those
/// that name it, and those that follow the default while it is the default. By template id.
pub async fn theme_live_templates(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    theme: Uuid,
) -> Result<Vec<LiveTemplate>, StoreError> {
    let used = theme_where_used(connection, workspace, theme).await?;
    let live: BTreeMap<Uuid, String> = used
        .named_templates
        .into_iter()
        .chain(used.default_templates)
        .filter(|template| template.in_current)
        .map(|template| (template.id, template.name))
        .collect();
    Ok(live
        .into_iter()
        .map(|(id, name)| LiveTemplate { id, name })
        .collect())
}

/// How many templates of the workspace, not archived, use each theme, by the rule of
/// [`theme_where_used`]: they name it in their draft or current version, and, for the default
/// theme only, they follow the default. A theme no template uses is absent. A fixed number of
/// reads, not one per theme.
pub async fn theme_template_counts<'e, A>(
    executor: A,
    workspace: WorkspaceId,
) -> Result<HashMap<Uuid, i64>, StoreError>
where
    A: Acquire<'e, Database = Postgres>,
{
    let mut connection = executor.acquire().await.map_err(StoreError::query)?;
    let rows: Vec<(String, i64)> = sqlx::query_as(concat!(
        "SELECT theme, count(*) FROM ( \
            SELECT d.template_id, ",
        named_theme!("d.document"),
        " AS theme FROM template_drafts d JOIN templates t ON t.id = d.template_id \
              WHERE t.workspace_id = $1 AND t.archived_at IS NULL \
            UNION \
            SELECT v.template_id, ",
        named_theme!("v.document"),
        " AS theme FROM template_versions v JOIN templates t ON t.id = v.template_id \
              AND v.version = t.current_version \
              WHERE t.workspace_id = $1 AND t.archived_at IS NULL \
         ) held WHERE theme IS NOT NULL GROUP BY theme"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut counts: HashMap<Uuid, i64> = rows
        .into_iter()
        .filter_map(|(theme, count)| Some((theme.parse().ok()?, count)))
        .collect();
    let default: Option<Uuid> =
        sqlx::query_scalar("SELECT theme_id FROM workspace_default_theme WHERE workspace_id = $1")
            .bind(workspace.as_uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    if let Some(default) = default {
        let mut users: hashbrown::HashSet<Uuid> =
            named_templates(&mut *connection, workspace, default)
                .await?
                .into_iter()
                .map(|template| template.id)
                .collect();
        users.extend(
            default_following_templates(&mut *connection, workspace)
                .await?
                .into_iter()
                .map(|template| template.id),
        );
        counts.insert(default, i64::try_from(users.len()).unwrap_or(i64::MAX));
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::{create as create_template, workspace};
    use crate::themes::set_default_theme;
    use crate::themes::tests_support::*;
    use crate::{FlowPatch, archive_template, save_template_draft, update_flow_draft};

    fn blocks(theme: Value) -> Value {
        let mut content = json!({"subject": "Hi", "sections": []});
        if !theme.is_null() {
            content["theme"] = theme;
        }
        json!({"schema": 2, "channel": "email", "mode": "blocks", "blocks": content})
    }

    async fn set_draft(pool: &PgPool, ws: WorkspaceId, template: Uuid, document: &Value) {
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(&mut connection, ws, template, document, None)
            .await
            .unwrap();
    }

    async fn used(pool: &PgPool, ws: WorkspaceId, theme: Uuid) -> ThemeUse {
        let mut connection = pool.acquire().await.unwrap();
        theme_where_used(&mut connection, ws, theme).await.unwrap()
    }

    fn names(list: &[ThemeTemplate]) -> Vec<&str> {
        list.iter().map(|template| template.name.as_str()).collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn follow_default_templates_are_listed_apart_from_named_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let brand = create(&pool, ws, "Brand").await;
        let other = create(&pool, ws, "Other").await;
        publish(&pool, ws, brand.id).await;
        publish(&pool, ws, other.id).await;
        set_default_theme(&mut pool.acquire().await.unwrap(), ws, brand.id)
            .await
            .unwrap();
        let follows = create_template(&pool, ws, "Follows").await;
        let absent = create_template(&pool, ws, "Absent").await;
        let named = create_template(&pool, ws, "Named").await;
        let elsewhere = create_template(&pool, ws, "Elsewhere").await;
        let opted_out = create_template(&pool, ws, "Opted out").await;
        let code = create_template(&pool, ws, "Code").await;
        set_draft(&pool, ws, follows.id, &blocks(json!("default"))).await;
        set_draft(&pool, ws, absent.id, &blocks(Value::Null)).await;
        set_draft(&pool, ws, named.id, &blocks(json!({"id": brand.id}))).await;
        set_draft(&pool, ws, elsewhere.id, &blocks(json!({"id": other.id}))).await;
        set_draft(&pool, ws, opted_out.id, &blocks(json!("none"))).await;
        // Stored schema 1 content has no theme and never follows the default.
        let mut v1 = blocks(Value::Null);
        v1["schema"] = json!(1);
        set_draft(&pool, ws, code.id, &v1).await;

        let on_default = used(&pool, ws, brand.id).await;
        let on_other = used(&pool, ws, other.id).await;

        assert_eq!(names(&on_default.default_templates), ["Absent", "Follows"]);
        assert_eq!(names(&on_default.named_templates), ["Named"]);
        assert_eq!(names(&on_other.named_templates), ["Elsewhere"]);
        assert!(on_other.default_templates.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_code_template_and_an_archived_template_are_not_listed(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let brand = create(&pool, ws, "Brand").await;
        publish(&pool, ws, brand.id).await;
        set_default_theme(&mut pool.acquire().await.unwrap(), ws, brand.id)
            .await
            .unwrap();
        // `create_template` makes a code-mode document.
        create_template(&pool, ws, "Code").await;
        let gone = create_template(&pool, ws, "Gone").await;
        set_draft(&pool, ws, gone.id, &blocks(json!("default"))).await;
        let mut connection = pool.acquire().await.unwrap();
        archive_template(&mut connection, ws, gone.id)
            .await
            .unwrap();
        drop(connection);

        let found = used(&pool, ws, brand.id).await;

        assert!(found.default_templates.is_empty() && found.named_templates.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_current_version_counts_when_the_draft_moved_on(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let brand = create(&pool, ws, "Brand").await;
        let template = create_template(&pool, ws, "Mail").await;
        set_draft(&pool, ws, template.id, &blocks(json!({"id": brand.id}))).await;
        sqlx::query(
            "INSERT INTO template_versions (workspace_id, template_id, version, document, \
                validation, trace_id) \
             SELECT workspace_id, template_id, 1, document, '[]'::jsonb, gen_random_uuid() \
             FROM template_drafts WHERE template_id = $1",
        )
        .bind(template.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE templates SET current_version = 1 WHERE id = $1")
            .bind(template.id)
            .execute(&pool)
            .await
            .unwrap();
        set_draft(&pool, ws, template.id, &blocks(json!("none"))).await;

        let found = used(&pool, ws, brand.id).await;

        let flags: Vec<(bool, bool)> = found
            .named_templates
            .iter()
            .map(|t| (t.in_draft, t.in_current))
            .collect();
        assert_eq!(flags, [(false, true)]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn active_flows_come_from_both_lists(pool: PgPool) {
        use crate::flows::tests_support as flows;
        let ws = flows::workspace(&pool, "one").await;
        let brand = create(&pool, ws, "Brand").await;
        publish(&pool, ws, brand.id).await;
        set_default_theme(&mut pool.acquire().await.unwrap(), ws, brand.id)
            .await
            .unwrap();
        let follows = create_template(&pool, ws, "Follows").await;
        let named = create_template(&pool, ws, "Named").await;
        set_draft(&pool, ws, follows.id, &blocks(json!("default"))).await;
        set_draft(&pool, ws, named.id, &blocks(json!({"id": brand.id}))).await;
        let mut running = Vec::new();
        for (name, template) in [("Follow flow", follows.id), ("Named flow", named.id)] {
            let flow = flows::create(&pool, ws, name).await;
            let graph = json!({"v": 1, "nodes": [
                {"id": "send", "type": "action.send", "template": template.to_string()},
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
            running.push(flow.id);
        }

        let found = used(&pool, ws, brand.id).await;

        let ids: Vec<Uuid> = found.active_flows.iter().map(|f| f.flow_id).collect();
        assert_eq!(ids, running);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_counts_match_the_where_used_lists(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let brand = create(&pool, ws, "Brand").await;
        let other = create(&pool, ws, "Other").await;
        let unused = create(&pool, ws, "Unused").await;
        publish(&pool, ws, brand.id).await;
        set_default_theme(&mut pool.acquire().await.unwrap(), ws, brand.id)
            .await
            .unwrap();
        let named = create_template(&pool, ws, "Named").await;
        let follows = create_template(&pool, ws, "Follows").await;
        let both = create_template(&pool, ws, "Both").await;
        let elsewhere = create_template(&pool, ws, "Elsewhere").await;
        let gone = create_template(&pool, ws, "Gone").await;
        set_draft(&pool, ws, named.id, &blocks(json!({"id": brand.id}))).await;
        set_draft(&pool, ws, follows.id, &blocks(json!("default"))).await;
        set_draft(&pool, ws, both.id, &blocks(json!({"id": brand.id}))).await;
        sqlx::query(
            "INSERT INTO template_versions (workspace_id, template_id, version, document, \
                validation, trace_id) \
             SELECT workspace_id, template_id, 1, $2, '[]'::jsonb, gen_random_uuid() \
             FROM template_drafts WHERE template_id = $1",
        )
        .bind(both.id)
        .bind(blocks(json!("default")))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE templates SET current_version = 1 WHERE id = $1")
            .bind(both.id)
            .execute(&pool)
            .await
            .unwrap();
        set_draft(&pool, ws, elsewhere.id, &blocks(json!({"id": other.id}))).await;
        set_draft(&pool, ws, gone.id, &blocks(json!({"id": other.id}))).await;
        let mut connection = pool.acquire().await.unwrap();
        archive_template(&mut connection, ws, gone.id)
            .await
            .unwrap();
        drop(connection);

        let counts = theme_template_counts(&pool, ws).await.unwrap();

        assert_eq!(counts.get(&brand.id), Some(&3));
        assert_eq!(counts.get(&other.id), Some(&1));
        assert_eq!(counts.get(&unused.id), None);
        let listed = used(&pool, ws, brand.id).await;
        assert_eq!(names(&listed.named_templates), ["Both", "Named"]);
        assert_eq!(names(&listed.default_templates), ["Both", "Follows"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_theme_tables_hold_no_person_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('themes', 'theme_drafts', 'theme_versions', \
                                  'workspace_default_theme') \
               AND column_name LIKE '%person%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }
}
