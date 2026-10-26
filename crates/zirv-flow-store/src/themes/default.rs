//! The workspace default theme and the theme a send resolves.

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::crud::{Theme, ThemeError, lock_theme_row};
use crate::StoreError;

/// Makes a published theme of the workspace its default, replacing the previous one, in the
/// caller's transaction. It holds the theme's row, so the theme cannot be archived meanwhile.
/// A theme that is not the workspace's is [`ThemeError::NotFound`], an archived one
/// [`ThemeError::Archived`] and one with no published version [`ThemeError::NotPublished`].
pub async fn set_default_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Theme, ThemeError> {
    let held = lock_theme_row(connection, workspace, id).await?;
    if held.archived_at.is_some() {
        return Err(ThemeError::Archived);
    }
    if held.current_version.is_none() {
        return Err(ThemeError::NotPublished);
    }
    sqlx::query(
        "INSERT INTO workspace_default_theme (workspace_id, theme_id) VALUES ($1, $2) \
         ON CONFLICT (workspace_id) DO UPDATE SET theme_id = EXCLUDED.theme_id, \
            updated_at = now()",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Theme {
        is_default: true,
        ..held
    })
}

/// Which theme an email asks for: what its document's `theme` says.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThemeChoice {
    /// `"default"`: the workspace default.
    Default,
    /// `{"id": <theme id>}`.
    Named(Uuid),
    /// `"none"`: no theme.
    None,
}

/// The current published version of a theme.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct EffectiveTheme {
    pub theme_id: Uuid,
    pub name: String,
    pub version: i32,
    /// The tokens as published.
    pub tokens: Value,
}

/// The theme an email renders with, as its current published version, or `None` when it has none.
///
/// `Default` follows the workspace default. `Named` resolves even when the theme is archived:
/// archiving hides a theme from pickers but must not break a send. A theme with no published
/// version, an unknown id, a workspace with no default and `None` all give `None`; the caller
/// tells a missing theme from a deliberate `None` by the choice it passed.
pub async fn effective_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    choice: ThemeChoice,
) -> Result<Option<EffectiveTheme>, StoreError> {
    let named = match choice {
        ThemeChoice::None => return Ok(None),
        ThemeChoice::Named(id) => Some(id),
        ThemeChoice::Default => None,
    };
    sqlx::query_as(
        "SELECT t.id AS theme_id, t.name, v.version, v.tokens FROM themes t \
         JOIN theme_versions v ON v.theme_id = t.id AND v.version = t.current_version \
         WHERE t.workspace_id = $1 AND CASE WHEN $2::uuid IS NULL \
            THEN t.id = (SELECT theme_id FROM workspace_default_theme WHERE workspace_id = $1) \
            ELSE t.id = $2 END",
    )
    .bind(workspace.as_uuid())
    .bind(named)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::workspace;
    use crate::themes::tests_support::*;
    use crate::themes::{archive_theme, find_theme};

    async fn effective(
        pool: &PgPool,
        ws: WorkspaceId,
        choice: ThemeChoice,
    ) -> Option<EffectiveTheme> {
        let mut connection = pool.acquire().await.unwrap();
        effective_theme(&mut connection, ws, choice).await.unwrap()
    }

    async fn set_default(pool: &PgPool, ws: WorkspaceId, id: Uuid) -> Result<Theme, ThemeError> {
        let mut connection = pool.acquire().await.unwrap();
        set_default_theme(&mut connection, ws, id).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setting_a_new_default_moves_the_default_choice_and_leaves_a_named_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = create(&pool, ws, "First").await;
        let second = create(&pool, ws, "Second").await;
        publish(&pool, ws, first.id).await;
        publish(&pool, ws, second.id).await;

        set_default(&pool, ws, first.id).await.unwrap();
        let before = effective(&pool, ws, ThemeChoice::Default).await.unwrap();
        let moved = set_default(&pool, ws, second.id).await.unwrap();
        let after = effective(&pool, ws, ThemeChoice::Default).await.unwrap();
        let named = effective(&pool, ws, ThemeChoice::Named(first.id))
            .await
            .unwrap();

        assert_eq!((before.theme_id, before.name.as_str()), (first.id, "First"));
        assert!(moved.is_default);
        assert_eq!((after.theme_id, after.version), (second.id, 1));
        assert_eq!(named.theme_id, first.id);
        assert!(
            !find_theme(&pool, ws, first.id)
                .await
                .unwrap()
                .unwrap()
                .is_default
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_with_no_default_and_the_none_choice_resolve_to_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Brand").await;
        publish(&pool, ws, theme.id).await;

        assert_eq!(effective(&pool, ws, ThemeChoice::Default).await, None);
        assert_eq!(effective(&pool, ws, ThemeChoice::None).await, None);
        assert_eq!(
            effective(&pool, ws, ThemeChoice::Named(Uuid::now_v7())).await,
            None
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_named_archived_theme_resolves_and_a_named_unpublished_one_does_not(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let archived = create(&pool, ws, "Old").await;
        let unpublished = create(&pool, ws, "Draft").await;
        publish(&pool, ws, archived.id).await;
        let mut connection = pool.acquire().await.unwrap();
        archive_theme(&mut connection, ws, archived.id)
            .await
            .unwrap();
        drop(connection);

        let old = effective(&pool, ws, ThemeChoice::Named(archived.id)).await;
        let none = effective(&pool, ws, ThemeChoice::Named(unpublished.id)).await;

        assert_eq!(old.map(|theme| theme.theme_id), Some(archived.id));
        assert_eq!(none, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_default_is_refused_for_an_unpublished_archived_or_foreign_theme(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let unpublished = create(&pool, ws, "Draft").await;
        let archived = create(&pool, ws, "Old").await;
        let foreign = create(&pool, other, "Foreign").await;
        publish(&pool, ws, archived.id).await;
        publish(&pool, other, foreign.id).await;
        let mut connection = pool.acquire().await.unwrap();
        archive_theme(&mut connection, ws, archived.id)
            .await
            .unwrap();
        drop(connection);

        assert!(matches!(
            set_default(&pool, ws, unpublished.id).await,
            Err(ThemeError::NotPublished)
        ));
        assert!(matches!(
            set_default(&pool, ws, archived.id).await,
            Err(ThemeError::Archived)
        ));
        assert!(matches!(
            set_default(&pool, ws, foreign.id).await,
            Err(ThemeError::NotFound)
        ));
        assert_eq!(effective(&pool, ws, ThemeChoice::Default).await, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_schema_itself_refuses_a_default_from_another_workspace(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let foreign = create(&pool, other, "Foreign").await;

        let refused = sqlx::query(
            "INSERT INTO workspace_default_theme (workspace_id, theme_id) VALUES ($1, $2)",
        )
        .bind(ws.as_uuid())
        .bind(foreign.id)
        .execute(&pool)
        .await;

        assert!(refused.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_default_theme_cannot_be_archived(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Brand").await;
        publish(&pool, ws, theme.id).await;
        set_default(&pool, ws, theme.id).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let refused = archive_theme(&mut connection, ws, theme.id).await;

        assert!(matches!(refused, Err(ThemeError::IsDefault)));
    }
}
