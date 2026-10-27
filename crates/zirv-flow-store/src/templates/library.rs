//! The template library read (architecture 11.7): the workspace's templates with the filters,
//! the sort and the derived facts the library page shows. One query lists them; the number of
//! active flows per template is a second, grouped query, never one per row.

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{AssertSqlSafe, PgExecutor, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::crud::Template;
use crate::StoreError;
use crate::themes::follows_default;

/// Which templates to list, by status. Without one, everything except archived templates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateStatus {
    /// Published, with a draft equal to the current version.
    Live,
    /// Published, with a draft that differs from the current version.
    Changes,
    /// Never published.
    Draft,
    Archived,
}

impl TemplateStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Changes => "changes",
            Self::Draft => "draft",
            Self::Archived => "archived",
        }
    }
}

/// The theme a template's draft follows, as a filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TemplateThemeFilter {
    /// Block emails that follow the workspace default theme.
    Default,
    /// Templates with no theme: code mode, webhooks and block emails that name none.
    None,
    /// Block emails that name this theme.
    Id(Uuid),
}

/// How to order the list.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TemplateSort {
    /// The draft's last save, newest first.
    #[default]
    Edited,
    /// The name, ignoring case.
    Name,
    /// Messages sent in the last 30 days, test sends excluded, most first.
    MostUsed,
}

/// The filters and the sort of a library read.
#[derive(Clone, Debug, Default)]
pub struct TemplateListQuery {
    /// A case-insensitive substring of the name, the draft's subject or a merge field it uses.
    pub search: Option<String>,
    pub channel: Option<String>,
    pub status: Option<TemplateStatus>,
    pub theme: Option<TemplateThemeFilter>,
    pub sort: TemplateSort,
    /// Lists archived templates too when no status is given.
    pub include_archived: bool,
}

/// The theme choice a draft makes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TemplateTheme {
    /// Follows the workspace default theme.
    Default,
    /// Follows no theme.
    None,
    Named {
        id: Uuid,
        name: String,
    },
}

/// A template with the facts the library shows.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateSummary {
    pub template: Template,
    /// The draft differs from the current version; false before the first publish.
    pub draft_changed: bool,
    /// The blocking and warning counts of the draft's last validation, if it has one.
    pub checks: Option<(i32, i32)>,
    pub theme: TemplateTheme,
    /// The email of the member who saved the draft last; none for an API key or a removed member.
    pub edited_by_email: Option<String>,
    pub edited_at: OffsetDateTime,
}

#[derive(FromRow)]
struct SummaryRow {
    #[sqlx(flatten)]
    template: Template,
    draft_changed: bool,
    checks_blocking: Option<i32>,
    checks_warnings: Option<i32>,
    theme_kind: String,
    theme_id: Option<Uuid>,
    theme_name: Option<String>,
    edited_by_email: Option<String>,
    edited_at: OffsetDateTime,
}

fn theme_of(kind: &str, id: Option<Uuid>, name: Option<String>) -> TemplateTheme {
    match (kind, id, name) {
        ("default", _, _) => TemplateTheme::Default,
        ("named", Some(id), Some(name)) => TemplateTheme::Named { id, name },
        _ => TemplateTheme::None,
    }
}

impl From<SummaryRow> for TemplateSummary {
    fn from(row: SummaryRow) -> Self {
        Self {
            template: row.template,
            draft_changed: row.draft_changed,
            checks: row.checks_blocking.zip(row.checks_warnings),
            theme: theme_of(&row.theme_kind, row.theme_id, row.theme_name),
            edited_by_email: row.edited_by_email,
            edited_at: row.edited_at,
        }
    }
}

/// A search term as a `LIKE` pattern that matches it literally anywhere.
fn like_pattern(term: &str) -> String {
    let mut pattern = String::with_capacity(term.len() + 2);
    pattern.push('%');
    for c in term.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

/// The workspace's templates for the library, filtered and sorted by `query`.
pub async fn list_templates<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    query: &TemplateListQuery,
) -> Result<Vec<TemplateSummary>, StoreError> {
    let (theme_kind, theme_id) = match query.theme {
        None => (None, None),
        Some(TemplateThemeFilter::Default) => (Some("default"), None),
        Some(TemplateThemeFilter::None) => (Some("none"), None),
        Some(TemplateThemeFilter::Id(id)) => (Some("named"), Some(id)),
    };
    let (order, sort) = match query.sort {
        TemplateSort::Edited => ("edited_at DESC, id", "edited"),
        TemplateSort::Name => ("lower(name), id", "name"),
        TemplateSort::MostUsed => ("uses DESC, lower(name), id", "most_used"),
    };
    // Only fixed fragments are spliced in: the theme rule and one of three ORDER BY clauses.
    let sql = format!(
        "SELECT id, name, channel, category, sender_identity_id, current_version, archived_at, \
             created_by, created_at, updated_at, utm, draft_changed, checks_blocking, \
             checks_warnings, theme_kind, theme_id, theme_name, edited_by_email, edited_at \
          FROM ( \
            SELECT t.id, t.name, t.channel, t.category, t.sender_identity_id, t.current_version, \
                t.archived_at, t.created_by, t.created_at, t.updated_at, t.utm, \
                (t.current_version IS NOT NULL AND d.document IS DISTINCT FROM ( \
                    SELECT v.document FROM template_versions v \
                    WHERE v.template_id = t.id AND v.version = t.current_version)) \
                    AS draft_changed, \
                (d.checks ->> 'blocking')::int AS checks_blocking, \
                (d.checks ->> 'warnings')::int AS checks_warnings, \
                CASE WHEN {follows} THEN 'default' \
                     WHEN th.id IS NOT NULL THEN 'named' ELSE 'none' END AS theme_kind, \
                CASE WHEN {follows} THEN NULL ELSE th.id END AS theme_id, \
                CASE WHEN {follows} THEN NULL ELSE th.name END AS theme_name, \
                CASE WHEN mb.user_id IS NOT NULL THEN u.email END AS edited_by_email, \
                d.updated_at AS edited_at, d.document AS document, \
                CASE WHEN $8 = 'most_used' THEN ( \
                    SELECT count(*) FROM messages m WHERE m.workspace_id = t.workspace_id \
                        AND m.template_id = t.id AND NOT m.test \
                        AND m.created_at >= now() - interval '30 days') ELSE 0 END AS uses \
            FROM templates t \
            JOIN template_drafts d ON d.template_id = t.id \
            LEFT JOIN themes th ON th.workspace_id = t.workspace_id \
                AND th.id::text = d.document #>> '{{blocks,theme,id}}' \
            LEFT JOIN users u ON u.id = d.updated_by \
            LEFT JOIN memberships mb ON mb.workspace_id = t.workspace_id \
                AND mb.user_id = d.updated_by \
            WHERE t.workspace_id = $1 AND ($2::text IS NULL OR t.channel = $2)) library \
          WHERE CASE $3::text \
                WHEN 'archived' THEN archived_at IS NOT NULL \
                WHEN 'live' THEN archived_at IS NULL AND current_version IS NOT NULL \
                    AND NOT draft_changed \
                WHEN 'changes' THEN archived_at IS NULL AND current_version IS NOT NULL \
                    AND draft_changed \
                WHEN 'draft' THEN archived_at IS NULL AND current_version IS NULL \
                ELSE ($4 OR archived_at IS NULL) END \
            AND ($5::text IS NULL OR (theme_kind = $5 AND ($5 <> 'named' OR theme_id = $6))) \
            AND ($7::text IS NULL OR name ILIKE $7 OR document #>> '{{email,subject}}' ILIKE $7 \
                OR document #>> '{{blocks,subject}}' ILIKE $7 OR EXISTS ( \
                    SELECT 1 FROM regexp_matches(document::text, \
                        '\\{{\\{{-?\\s*([A-Za-z0-9_.]+)', 'g') field WHERE field[1] ILIKE $7)) \
          ORDER BY {order}",
        follows = follows_default!("d.document"),
    );
    let rows: Vec<SummaryRow> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(query.channel.as_deref())
        .bind(query.status.map(TemplateStatus::as_str))
        .bind(query.include_archived)
        .bind(theme_kind)
        .bind(theme_id)
        .bind(query.search.as_deref().map(like_pattern))
        .bind(sort)
        .fetch_all(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(rows.into_iter().map(TemplateSummary::from).collect())
}

/// How many active flows send each template: flows in the `published` state whose running
/// version names the template, by the rule of [`super::active_published_flows`]. Templates no
/// flow sends are absent.
pub async fn template_active_flow_counts<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<HashMap<Uuid, i64>, StoreError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT node ->> 'template', count(DISTINCT f.id) \
         FROM flows f \
         JOIN flow_versions v ON v.workspace_id = f.workspace_id AND v.flow_id = f.id \
            AND v.version = f.current_version, \
         jsonb_array_elements(CASE \
            WHEN jsonb_typeof(v.graph -> 'nodes') = 'array' THEN v.graph -> 'nodes' \
            ELSE '[]'::jsonb END) node \
         WHERE f.workspace_id = $1 AND f.state = 'published' \
            AND node ->> 'template' IS NOT NULL \
         GROUP BY 1",
    )
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .filter_map(|(template, count)| Some((template.parse().ok()?, count)))
        .collect())
}

/// Stores the counts of a validation on the draft, if the draft is still at `revision`. A draft
/// saved again since is left alone: that save cleared its checks. Returns whether it stored them.
pub async fn set_template_draft_checks(
    pool: &PgPool,
    workspace: WorkspaceId,
    id: Uuid,
    revision: i32,
    blocking: i32,
    warnings: i32,
) -> Result<bool, StoreError> {
    let checks = serde_json::json!({"blocking": blocking, "warnings": warnings});
    let result = sqlx::query(
        "UPDATE template_drafts SET checks = $4 \
         WHERE workspace_id = $1 AND template_id = $2 AND revision = $3",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(revision)
    .bind(checks)
    .execute(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::templates::tests_support::*;
    use crate::templates::{lock_template, publish_template, save_template_draft};
    use crate::{archive_template, begin, commit};

    #[test]
    fn a_search_term_matches_literally() {
        assert_eq!(like_pattern("50%_off\\"), "%50\\%\\_off\\\\%");
        assert_eq!(like_pattern("hi"), "%hi%");
    }

    #[test]
    fn a_status_has_its_wire_name() {
        assert_eq!(TemplateStatus::Changes.as_str(), "changes");
        assert_eq!(TemplateStatus::Archived.as_str(), "archived");
    }

    #[test]
    fn a_theme_without_a_row_to_name_follows_none() {
        let id = Uuid::nil();
        assert_eq!(theme_of("named", Some(id), None), TemplateTheme::None);
        assert_eq!(theme_of("default", None, None), TemplateTheme::Default);
        assert_eq!(
            theme_of("named", Some(id), Some("Brand".into())),
            TemplateTheme::Named {
                id,
                name: "Brand".into()
            }
        );
    }

    async fn publish(pool: &PgPool, ws: WorkspaceId, id: Uuid) {
        let mut tx = begin(pool).await.unwrap();
        let locked = lock_template(&mut tx, ws, id).await.unwrap();
        publish_template(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    fn query() -> TemplateListQuery {
        TemplateListQuery::default()
    }

    async fn names(pool: &PgPool, ws: WorkspaceId, query: &TemplateListQuery) -> Vec<String> {
        list_templates(pool, ws, query)
            .await
            .unwrap()
            .into_iter()
            .map(|summary| summary.template.name)
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_status_filters_and_the_default_hide_archived_templates(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let live = create(&pool, ws, "Live").await;
        let changed = create(&pool, ws, "Changed").await;
        create(&pool, ws, "Fresh").await;
        let gone = create(&pool, ws, "Gone").await;
        publish(&pool, ws, live.id).await;
        publish(&pool, ws, changed.id).await;
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(&mut connection, ws, changed.id, &document("v2"), None)
            .await
            .unwrap();
        archive_template(&mut connection, ws, gone.id)
            .await
            .unwrap();
        let with = |status| TemplateListQuery {
            status: Some(status),
            sort: TemplateSort::Name,
            ..query()
        };

        assert_eq!(
            names(&pool, ws, &with(TemplateStatus::Live)).await,
            ["Live"]
        );
        assert_eq!(
            names(&pool, ws, &with(TemplateStatus::Changes)).await,
            ["Changed"]
        );
        assert_eq!(
            names(&pool, ws, &with(TemplateStatus::Draft)).await,
            ["Fresh"]
        );
        assert_eq!(
            names(&pool, ws, &with(TemplateStatus::Archived)).await,
            ["Gone"]
        );
        let default = TemplateListQuery {
            sort: TemplateSort::Name,
            ..query()
        };
        assert_eq!(
            names(&pool, ws, &default).await,
            ["Changed", "Fresh", "Live"]
        );
        let archived = TemplateListQuery {
            include_archived: true,
            ..default
        };
        assert_eq!(names(&pool, ws, &archived).await.len(), 4);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn draft_changed_and_checks_follow_the_draft(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let template = create(&pool, ws, "Welcome").await;
        let fresh = list_templates(&pool, ws, &query()).await.unwrap();
        assert_eq!((fresh[0].draft_changed, fresh[0].checks), (false, None));
        publish(&pool, ws, template.id).await;
        let mut connection = pool.acquire().await.unwrap();
        let saved = save_template_draft(&mut connection, ws, template.id, &document("v2"), None)
            .await
            .unwrap();
        let stored = set_template_draft_checks(&pool, ws, template.id, saved.revision, 1, 2)
            .await
            .unwrap();
        let stale = set_template_draft_checks(&pool, ws, template.id, saved.revision + 1, 9, 9)
            .await
            .unwrap();

        let listed = list_templates(&pool, ws, &query()).await.unwrap();
        assert!(stored && !stale);
        assert_eq!(
            (listed[0].draft_changed, listed[0].checks),
            (true, Some((1, 2)))
        );
        save_template_draft(&mut connection, ws, template.id, &document("v3"), None)
            .await
            .unwrap();
        let cleared = list_templates(&pool, ws, &query()).await.unwrap();
        assert_eq!(cleared[0].checks, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_search_covers_the_name_the_subject_and_the_merge_fields(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let by_name = create(&pool, ws, "Spring Sale").await;
        let by_field = create(&pool, ws, "Other").await;
        create(&pool, ws, "Plain").await;
        let mut connection = pool.acquire().await.unwrap();
        let body = |subject: &str, html: &str| {
            json!({"schema": 1, "channel": "email", "mode": "code",
                   "email": {"subject": subject, "html": html}})
        };
        save_template_draft(
            &mut connection,
            ws,
            by_name.id,
            &body("Big news", "<p>x</p>"),
            None,
        )
        .await
        .unwrap();
        save_template_draft(
            &mut connection,
            ws,
            by_field.id,
            &body("Hello", "<p>{{ person.nickname }}</p>"),
            None,
        )
        .await
        .unwrap();
        let search = |term: &str| TemplateListQuery {
            search: Some(term.into()),
            sort: TemplateSort::Name,
            ..query()
        };

        assert_eq!(names(&pool, ws, &search("SPRING")).await, ["Spring Sale"]);
        assert_eq!(names(&pool, ws, &search("big n")).await, ["Spring Sale"]);
        assert_eq!(names(&pool, ws, &search("nickname")).await, ["Other"]);
        assert!(names(&pool, ws, &search("%")).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_channel_filter_and_the_sorts_order_the_list(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let b = create(&pool, ws, "Bravo").await;
        create(&pool, ws, "alpha").await;
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(&mut connection, ws, b.id, &document("v2"), None)
            .await
            .unwrap();
        let sorted = |sort| TemplateListQuery { sort, ..query() };
        let webhook = TemplateListQuery {
            channel: Some("webhook".into()),
            ..query()
        };

        assert_eq!(
            names(&pool, ws, &sorted(TemplateSort::Edited)).await,
            ["Bravo", "alpha"]
        );
        assert_eq!(
            names(&pool, ws, &sorted(TemplateSort::Name)).await,
            ["alpha", "Bravo"]
        );
        assert_eq!(
            names(&pool, ws, &sorted(TemplateSort::MostUsed)).await,
            ["alpha", "Bravo"]
        );
        assert!(names(&pool, ws, &webhook).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_theme_filter_tells_default_none_and_a_named_theme_apart(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = crate::themes::tests_support::create(&pool, ws, "Brand").await;
        create(&pool, ws, "Code").await;
        let following = create(&pool, ws, "Following").await;
        let named = create(&pool, ws, "Named").await;
        let blocks = |theme: Value| {
            json!({"schema": 2, "channel": "email", "mode": "blocks",
                   "blocks": {"subject": "Hi", "theme": theme}})
        };
        let mut connection = pool.acquire().await.unwrap();
        save_template_draft(
            &mut connection,
            ws,
            following.id,
            &blocks(json!("default")),
            None,
        )
        .await
        .unwrap();
        save_template_draft(
            &mut connection,
            ws,
            named.id,
            &blocks(json!({"id": theme.id.to_string()})),
            None,
        )
        .await
        .unwrap();
        let by = |theme| TemplateListQuery {
            theme: Some(theme),
            ..query()
        };

        assert_eq!(
            names(&pool, ws, &by(TemplateThemeFilter::Default)).await,
            ["Following"]
        );
        assert_eq!(
            names(&pool, ws, &by(TemplateThemeFilter::None)).await,
            ["Code"]
        );
        let listed = list_templates(&pool, ws, &by(TemplateThemeFilter::Id(theme.id)))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].theme,
            TemplateTheme::Named {
                id: theme.id,
                name: "Brand".into()
            }
        );
    }
}
