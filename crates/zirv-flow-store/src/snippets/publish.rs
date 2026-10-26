//! Publishing a snippet's draft into an immutable version, and reading versions.
//!
//! This crate never sees a block or the validator: the caller locks the snippet with
//! [`lock_snippet`], validates the draft it holds, and passes the same [`LockedSnippet`] to
//! [`publish_snippet`], so the content that was checked is the content that is published. A caller
//! that finds blocking issues drops its transaction, which writes no version.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::crud::{
    Snippet, SnippetDraft, SnippetError, find_snippet_draft, lock_live_snippet, snippet_columns,
};
use crate::StoreError;

/// One immutable published version of a snippet.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SnippetVersion {
    pub snippet_id: Uuid,
    pub version: i32,
    /// The content as it was published.
    pub content: Value,
    /// The issues found at publish time: warnings only, because a blocking one refuses the
    /// publish.
    pub validation: Value,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
    /// The trace of the request that published the version.
    pub trace_id: Uuid,
}

macro_rules! version_columns {
    () => {
        "snippet_id, version, content, validation, published_at, published_by, trace_id"
    };
}

/// A snippet whose row this transaction holds `FOR UPDATE`, with the draft read under that lock.
/// Only [`lock_snippet`] makes one, so a publish can only act on content that was read under the
/// lock.
#[derive(Debug)]
pub struct LockedSnippet {
    snippet: Snippet,
    draft: SnippetDraft,
}

impl LockedSnippet {
    pub fn snippet(&self) -> &Snippet {
        &self.snippet
    }

    /// The draft the publish would copy.
    pub fn draft(&self) -> &SnippetDraft {
        &self.draft
    }
}

/// Locks a snippet that is not archived and reads its draft. Writes to the snippet wait here, so
/// two publishes of one snippet run one after the other.
pub async fn lock_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<LockedSnippet, SnippetError> {
    let snippet = lock_live_snippet(connection, workspace, id).await?;
    let draft = find_snippet_draft(&mut *connection, workspace, id)
        .await?
        .ok_or(SnippetError::NotFound)?;
    Ok(LockedSnippet { snippet, draft })
}

/// Publishes the locked snippet's draft as version n + 1 and makes it current, in the caller's
/// transaction. `validation` is the issues the caller found, stored with the version.
pub async fn publish_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    locked: &LockedSnippet,
    validation: &Value,
    published_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Snippet, SnippetVersion), SnippetError> {
    let snippet = &locked.snippet;
    let number = snippet.current_version.unwrap_or(0) + 1;
    let version: SnippetVersion = sqlx::query_as(concat!(
        "INSERT INTO snippet_versions (workspace_id, snippet_id, version, content, validation, \
            published_by, trace_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        version_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(snippet.id)
    .bind(number)
    .bind(&locked.draft.content)
    .bind(validation)
    .bind(published_by.map(|user| user.as_uuid()))
    .bind(trace.trace_id.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let updated: Snippet = sqlx::query_as(concat!(
        "UPDATE snippets SET current_version = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        snippet_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(snippet.id)
    .bind(number)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok((updated, version))
}

/// A published version without its content: what a history list shows.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SnippetVersionSummary {
    pub version: i32,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
    /// The warnings stored with the version.
    pub warnings: i64,
}

/// The versions of a snippet of the workspace, newest first, without their content.
pub async fn list_snippet_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<SnippetVersionSummary>, StoreError> {
    sqlx::query_as(
        "SELECT version, published_at, published_by, \
            (SELECT count(*) FROM jsonb_array_elements(CASE \
                WHEN jsonb_typeof(validation) = 'array' THEN validation ELSE '[]'::jsonb END) i \
                WHERE i ->> 'severity' = 'warning') AS warnings \
         FROM snippet_versions WHERE workspace_id = $1 AND snippet_id = $2 \
         ORDER BY version DESC",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One version of a snippet of the workspace.
pub async fn find_snippet_version<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    version: i32,
) -> Result<Option<SnippetVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM snippet_versions WHERE workspace_id = $1 AND snippet_id = $2 AND version = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(version)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::snippets::save_snippet_draft;
    use crate::snippets::tests_support::*;
    use crate::templates::tests_support::{trace, workspace};
    use crate::{begin, commit};

    async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
    ) -> Result<(Snippet, SnippetVersion), SnippetError> {
        let mut tx = begin(pool).await?;
        let locked = lock_snippet(&mut tx, ws, id).await?;
        let published = publish_snippet(&mut tx, ws, &locked, &json!([]), None, &trace()).await?;
        commit(tx).await?;
        Ok(published)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_publish_makes_version_two_and_version_one_stays_readable(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;

        let (after_one, one) = publish(&pool, ws, snippet.id).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        save_snippet_draft(
            &mut connection,
            ws,
            snippet.id,
            &content("second"),
            None,
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let (after_two, two) = publish(&pool, ws, snippet.id).await.unwrap();

        assert_eq!((one.version, after_one.current_version), (1, Some(1)));
        assert_eq!((two.version, after_two.current_version), (2, Some(2)));
        assert_eq!(one.content, content("draft"));
        assert_eq!(two.content, content("second"));
        let all = list_snippet_versions(&pool, ws, snippet.id).await.unwrap();
        assert_eq!(all.iter().map(|v| v.version).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(
            find_snippet_version(&pool, ws, snippet.id, 1)
                .await
                .unwrap(),
            Some(one)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_version_cannot_be_changed_or_deleted_while_its_snippet_exists(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        publish(&pool, ws, snippet.id).await.unwrap();

        let update = sqlx::query("UPDATE snippet_versions SET content = '{}'::jsonb")
            .execute(&pool)
            .await
            .unwrap_err();
        let delete = sqlx::query("DELETE FROM snippet_versions")
            .execute(&pool)
            .await
            .unwrap_err();

        assert!(
            update
                .to_string()
                .contains("snippet versions are immutable")
        );
        assert!(delete.to_string().contains("cannot be deleted"));
        let still = find_snippet_version(&pool, ws, snippet.id, 1)
            .await
            .unwrap();
        assert_eq!(still.map(|v| v.content), Some(content("draft")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_abandoned_publish_writes_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_snippet(&mut tx, ws, snippet.id).await.unwrap();
        publish_snippet(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        drop(tx);

        assert!(
            list_snippet_versions(&pool, ws, snippet.id)
                .await
                .unwrap()
                .is_empty()
        );
        let after = find_snippet(&pool, ws, snippet.id).await.unwrap().unwrap();
        assert_eq!(after.current_version, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_version_list_counts_warnings_and_carries_no_content(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_snippet(&mut tx, ws, snippet.id).await.unwrap();
        let warnings = json!([
            {"code": "image_without_alt", "severity": "warning", "at": "/blocks/block/i"},
            {"code": "insecure_link", "severity": "warning", "at": "/blocks/block/b"}
        ]);
        publish_snippet(&mut tx, ws, &locked, &warnings, None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let all = list_snippet_versions(&pool, ws, snippet.id).await.unwrap();

        assert_eq!(all.len(), 1);
        assert_eq!((all[0].version, all[0].warnings), (1, 2));
    }

    use crate::find_snippet;
}
