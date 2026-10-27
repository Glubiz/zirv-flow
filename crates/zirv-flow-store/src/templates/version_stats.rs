//! What a template version sent: the count of its non-test messages and the theme and snippet
//! versions those messages recorded (#307).
//!
//! The answer holds counts only: no person, address or value. A person's erasure deletes their
//! messages, so the counts drop with them.

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// The most theme and snippet combinations a stats read returns.
pub(crate) const MAX_RENDER_ROWS: usize = 20;

/// One distinct theme and snippet combination recorded on the messages of a version.
#[derive(Clone, Debug, PartialEq)]
pub struct VersionRender {
    /// The theme in effect, or none.
    pub theme_id: Option<Uuid>,
    pub theme_version: Option<i32>,
    /// A map from snippet id to version number, empty when the render used no snippet.
    pub snippet_versions: Value,
    pub count: i64,
}

/// The non-test messages sent with one template version.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateVersionStats {
    /// Every non-test message sent with the version, including those that recorded no render.
    pub messages: i64,
    /// The combinations of rendered messages, most used first, at most [`MAX_RENDER_ROWS`].
    pub renders: Vec<VersionRender>,
    /// Whether more combinations exist than `renders` holds.
    pub more: bool,
}

#[derive(FromRow)]
struct Group {
    theme_id: Option<Uuid>,
    theme_version: Option<i32>,
    snippet_versions: Option<Value>,
    n: i64,
    total: i64,
    rendered_groups: i64,
}

/// The stats of one version of a template. One grouped read over the
/// `(workspace_id, template_id, created_at)` index. The caller checks that the version exists.
pub async fn template_version_stats<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    template: Uuid,
    version: i32,
) -> Result<TemplateVersionStats, StoreError> {
    let groups: Vec<Group> = sqlx::query_as(
        "WITH g AS ( \
            SELECT theme_id, theme_version, snippet_versions, count(*) AS n FROM messages \
            WHERE workspace_id = $1 AND template_id = $2 AND template_version = $3 AND NOT test \
            GROUP BY theme_id, theme_version, snippet_versions) \
         SELECT theme_id, theme_version, snippet_versions, n, \
            sum(n) OVER ()::bigint AS total, \
            (count(*) FILTER (WHERE snippet_versions IS NOT NULL) OVER ())::bigint \
                AS rendered_groups \
         FROM g ORDER BY snippet_versions IS NULL, n DESC, theme_id, theme_version LIMIT $4",
    )
    .bind(workspace.as_uuid())
    .bind(template)
    .bind(version)
    .bind(MAX_RENDER_ROWS as i64 + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let messages = groups.first().map_or(0, |group| group.total);
    let more = groups
        .first()
        .is_some_and(|group| group.rendered_groups > MAX_RENDER_ROWS as i64);
    let renders = groups
        .into_iter()
        .filter_map(|group| {
            Some(VersionRender {
                theme_id: group.theme_id,
                theme_version: group.theme_version,
                snippet_versions: group.snippet_versions?,
                count: group.n,
            })
        })
        .take(MAX_RENDER_ROWS)
        .collect();
    Ok(TemplateVersionStats {
        messages,
        renders,
        more,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::workspace;

    async fn person(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, traits) VALUES ($1, $2, '{}')")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        person
    }

    struct Sent {
        template: Uuid,
        version: i32,
        test: bool,
        theme: Option<(Uuid, i32)>,
        snippets: Option<Value>,
    }

    async fn send(pool: &PgPool, ws: WorkspaceId, person: Uuid, sent: Sent, copies: i32) {
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, node_id, \
                template_id, template_version, destination_id, status, test, theme_id, \
                theme_version, snippet_versions, created_at, completed_at) \
             SELECT gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), 'n', $3, $4, \
                gen_random_uuid(), 'sent', $5, $6, $7, $8, now(), now() \
             FROM generate_series(1, $9)",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(sent.template)
        .bind(sent.version)
        .bind(sent.test)
        .bind(sent.theme.map(|(id, _)| id))
        .bind(sent.theme.map(|(_, version)| version))
        .bind(sent.snippets)
        .bind(copies)
        .execute(pool)
        .await
        .unwrap();
    }

    fn sent(template: Uuid, version: i32) -> Sent {
        Sent {
            template,
            version,
            test: false,
            theme: None,
            snippets: Some(json!({})),
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stats_count_only_the_non_test_messages_of_that_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other_ws = workspace(&pool, "two").await;
        let person = person(&pool, ws).await;
        let template = Uuid::now_v7();
        send(&pool, ws, person, sent(template, 2), 3).await;
        let test = Sent {
            test: true,
            ..sent(template, 2)
        };
        send(&pool, ws, person, test, 4).await;
        send(&pool, ws, person, sent(template, 1), 5).await;
        send(&pool, ws, person, sent(Uuid::now_v7(), 2), 6).await;

        let stats = template_version_stats(&pool, ws, template, 2)
            .await
            .unwrap();

        assert_eq!(stats.messages, 3);
        assert_eq!(stats.renders.len(), 1);
        assert_eq!(stats.renders[0].count, 3);
        let elsewhere = template_version_stats(&pool, other_ws, template, 2)
            .await
            .unwrap();
        assert_eq!((elsewhere.messages, elsewhere.renders.len()), (0, 0));
        let unsent = template_version_stats(&pool, ws, template, 9)
            .await
            .unwrap();
        assert!(!unsent.more && unsent.messages == 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stats_group_the_theme_and_snippet_versions_most_used_first(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person = person(&pool, ws).await;
        let template = Uuid::now_v7();
        let theme = Uuid::now_v7();
        let snippet = Uuid::now_v7();
        let rendered = |version: i32, snippets: Value| Sent {
            theme: Some((theme, version)),
            snippets: Some(snippets),
            ..sent(template, 1)
        };
        send(&pool, ws, person, rendered(1, json!({})), 2).await;
        send(
            &pool,
            ws,
            person,
            rendered(2, json!({ snippet.to_string(): 3 })),
            5,
        )
        .await;
        send(
            &pool,
            ws,
            person,
            rendered(2, json!({ snippet.to_string(): 4 })),
            1,
        )
        .await;
        // Not rendered: counted as a message, listed as no combination.
        let skipped = Sent {
            snippets: None,
            ..sent(template, 1)
        };
        send(&pool, ws, person, skipped, 7).await;

        let stats = template_version_stats(&pool, ws, template, 1)
            .await
            .unwrap();

        assert_eq!(stats.messages, 15);
        assert!(!stats.more);
        let seen: Vec<(Option<i32>, i64)> = stats
            .renders
            .iter()
            .map(|render| (render.theme_version, render.count))
            .collect();
        assert_eq!(seen, [(Some(2), 5), (Some(1), 2), (Some(2), 1)]);
        assert_eq!(stats.renders[0].theme_id, Some(theme));
        assert_eq!(
            stats.renders[0].snippet_versions,
            json!({ snippet.to_string(): 3 })
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stats_cap_the_combinations_and_say_there_are_more(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person = person(&pool, ws).await;
        let template = Uuid::now_v7();
        for version in 1..=(MAX_RENDER_ROWS as i32 + 3) {
            let batch = Sent {
                theme: Some((Uuid::nil(), version)),
                ..sent(template, 1)
            };
            send(&pool, ws, person, batch, 1).await;
        }

        let stats = template_version_stats(&pool, ws, template, 1)
            .await
            .unwrap();

        assert_eq!(stats.messages, MAX_RENDER_ROWS as i64 + 3);
        assert_eq!(stats.renders.len(), MAX_RENDER_ROWS);
        assert!(stats.more);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_stats_read_uses_the_template_index(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let person = person(&pool, ws).await;
        // Many templates, so the planner has a template to be selective about.
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, node_id, \
                template_id, template_version, destination_id, status, test, created_at, \
                completed_at) \
             SELECT gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), 'n', \
                ('00000000-0000-7000-8000-' || lpad((n % 200)::text, 12, '0'))::uuid, 1, \
                gen_random_uuid(), 'sent', false, now(), now() \
             FROM generate_series(1, 4000) n",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE messages")
            .execute(&pool)
            .await
            .unwrap();

        let plan: Vec<(String,)> = sqlx::query_as(
            "EXPLAIN WITH g AS ( \
                SELECT theme_id, theme_version, snippet_versions, count(*) AS n FROM messages \
                WHERE workspace_id = $1 AND template_id = $2 AND template_version = $3 \
                    AND NOT test \
                GROUP BY theme_id, theme_version, snippet_versions) \
             SELECT n, sum(n) OVER () FROM g ORDER BY n DESC LIMIT 21",
        )
        .bind(ws.as_uuid())
        .bind(Uuid::parse_str("00000000-0000-7000-8000-000000000007").unwrap())
        .bind(1)
        .fetch_all(&pool)
        .await
        .unwrap();

        let plan: Vec<String> = plan.into_iter().map(|(line,)| line).collect();
        assert!(
            plan.iter()
                .any(|line| line.contains("messages_template_idx")),
            "{plan:?}"
        );
    }
}
