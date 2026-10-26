//! Publishing a theme's draft into an immutable version, and reading versions.
//!
//! This crate never sees a block or the validator: the caller locks the theme with
//! [`lock_theme`], validates the draft it holds, and passes the same [`LockedTheme`] to
//! [`publish_theme`], so the tokens that were checked are the tokens that are published. A caller
//! that finds blocking issues drops its transaction, which writes no version.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::crud::{
    Theme, ThemeDraft, ThemeError, find_theme_draft, lock_live_theme, theme_columns,
};
use crate::StoreError;

/// One immutable published version of a theme.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct ThemeVersion {
    pub theme_id: Uuid,
    pub version: i32,
    /// The tokens as they were published.
    pub tokens: Value,
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
        "theme_id, version, tokens, validation, published_at, published_by, trace_id"
    };
}

/// A theme whose row this transaction holds `FOR UPDATE`, with the draft read under that lock.
/// Only [`lock_theme`] makes one, so a publish can only act on tokens that were read under the
/// lock.
#[derive(Debug)]
pub struct LockedTheme {
    theme: Theme,
    draft: ThemeDraft,
}

impl LockedTheme {
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The draft the publish would copy.
    pub fn draft(&self) -> &ThemeDraft {
        &self.draft
    }
}

/// Locks a theme that is not archived and reads its draft. Writes to the theme wait here, so
/// two publishes of one theme run one after the other.
pub async fn lock_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<LockedTheme, ThemeError> {
    let theme = lock_live_theme(connection, workspace, id).await?;
    let draft = find_theme_draft(&mut *connection, workspace, id)
        .await?
        .ok_or(ThemeError::NotFound)?;
    Ok(LockedTheme { theme, draft })
}

/// Whether the workspace has a snippet that a theme may use as its footer: not archived, with a
/// published version. It holds the snippet row `FOR SHARE` until the transaction ends, so the
/// snippet cannot be archived between this check and the publish.
pub async fn footer_snippet_usable(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    snippet: Uuid,
) -> Result<bool, StoreError> {
    let usable: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM snippets WHERE workspace_id = $1 AND id = $2 \
            AND archived_at IS NULL AND current_version IS NOT NULL FOR SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(snippet)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(usable.is_some())
}

/// Publishes the locked theme's draft as version n + 1 and makes it current, in the caller's
/// transaction. `validation` is the issues the caller found, stored with the version.
pub async fn publish_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    locked: &LockedTheme,
    validation: &Value,
    published_by: Option<UserId>,
    trace: &TraceContext,
) -> Result<(Theme, ThemeVersion), ThemeError> {
    let theme = &locked.theme;
    let number = theme.current_version.unwrap_or(0) + 1;
    let version: ThemeVersion = sqlx::query_as(concat!(
        "INSERT INTO theme_versions (workspace_id, theme_id, version, tokens, validation, \
            published_by, trace_id) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        version_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(theme.id)
    .bind(number)
    .bind(&locked.draft.tokens)
    .bind(validation)
    .bind(published_by.map(|user| user.as_uuid()))
    .bind(trace.trace_id.as_uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let updated: Theme = sqlx::query_as(concat!(
        "UPDATE themes SET current_version = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        theme_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(theme.id)
    .bind(number)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok((updated, version))
}

/// A published version without its tokens: what a history list shows.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct ThemeVersionSummary {
    pub version: i32,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
    /// The warnings stored with the version.
    pub warnings: i64,
}

/// The versions of a theme of the workspace, newest first, without their tokens.
pub async fn list_theme_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<ThemeVersionSummary>, StoreError> {
    sqlx::query_as(
        "SELECT version, published_at, published_by, \
            (SELECT count(*) FROM jsonb_array_elements(CASE \
                WHEN jsonb_typeof(validation) = 'array' THEN validation ELSE '[]'::jsonb END) i \
                WHERE i ->> 'severity' = 'warning') AS warnings \
         FROM theme_versions WHERE workspace_id = $1 AND theme_id = $2 \
         ORDER BY version DESC",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One version of a theme of the workspace.
pub async fn find_theme_version<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    version: i32,
) -> Result<Option<ThemeVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM theme_versions WHERE workspace_id = $1 AND theme_id = $2 AND version = $3"
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
    use crate::find_theme;
    use crate::templates::tests_support::{trace, workspace};
    use crate::themes::save_theme_draft;
    use crate::themes::tests_support::*;
    use crate::{begin, commit};

    async fn publish(
        pool: &PgPool,
        ws: WorkspaceId,
        id: Uuid,
    ) -> Result<(Theme, ThemeVersion), ThemeError> {
        let mut tx = begin(pool).await?;
        let locked = lock_theme(&mut tx, ws, id).await?;
        let published = publish_theme(&mut tx, ws, &locked, &json!([]), None, &trace()).await?;
        commit(tx).await?;
        Ok(published)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_publish_makes_version_two_and_version_one_stays_readable(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Offer").await;

        let (after_one, one) = publish(&pool, ws, theme.id).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        save_theme_draft(
            &mut connection,
            ws,
            theme.id,
            &tokens("#222222"),
            None,
            None,
        )
        .await
        .unwrap();
        drop(connection);
        let (after_two, two) = publish(&pool, ws, theme.id).await.unwrap();

        assert_eq!((one.version, after_one.current_version), (1, Some(1)));
        assert_eq!((two.version, after_two.current_version), (2, Some(2)));
        assert_eq!(one.tokens, tokens("#112233"));
        assert_eq!(two.tokens, tokens("#222222"));
        let all = list_theme_versions(&pool, ws, theme.id).await.unwrap();
        assert_eq!(all.iter().map(|v| v.version).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(
            find_theme_version(&pool, ws, theme.id, 1).await.unwrap(),
            Some(one)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_version_cannot_be_changed_or_deleted_while_its_theme_exists(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Offer").await;
        publish(&pool, ws, theme.id).await.unwrap();

        let update = sqlx::query("UPDATE theme_versions SET tokens = '{}'::jsonb")
            .execute(&pool)
            .await
            .unwrap_err();
        let delete = sqlx::query("DELETE FROM theme_versions")
            .execute(&pool)
            .await
            .unwrap_err();

        assert!(update.to_string().contains("theme versions are immutable"));
        assert!(delete.to_string().contains("cannot be deleted"));
        let still = find_theme_version(&pool, ws, theme.id, 1).await.unwrap();
        assert_eq!(still.map(|v| v.tokens), Some(tokens("#112233")));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_abandoned_publish_writes_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Offer").await;
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_theme(&mut tx, ws, theme.id).await.unwrap();
        publish_theme(&mut tx, ws, &locked, &json!([]), None, &trace())
            .await
            .unwrap();
        drop(tx);

        assert!(
            list_theme_versions(&pool, ws, theme.id)
                .await
                .unwrap()
                .is_empty()
        );
        let after = find_theme(&pool, ws, theme.id).await.unwrap().unwrap();
        assert_eq!(after.current_version, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_version_list_counts_warnings_and_carries_no_tokens(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Offer").await;
        let mut tx = begin(&pool).await.unwrap();
        let locked = lock_theme(&mut tx, ws, theme.id).await.unwrap();
        let warnings = json!([
            {"code": "image_without_alt", "severity": "warning", "at": "/blocks/block/i"},
            {"code": "insecure_link", "severity": "warning", "at": "/blocks/block/b"}
        ]);
        publish_theme(&mut tx, ws, &locked, &warnings, None, &trace())
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let all = list_theme_versions(&pool, ws, theme.id).await.unwrap();

        assert_eq!(all.len(), 1);
        assert_eq!((all[0].version, all[0].warnings), (1, 2));
    }
}
