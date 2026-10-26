//! Creating, reading, changing and archiving themes and saving their drafts. Every function is
//! scoped to one workspace. A write runs in the caller's transaction, together with its audit
//! entry.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// A theme: its name and which version is current. The tokens are in its draft and its versions.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Theme {
    pub id: Uuid,
    pub name: String,
    /// The newest published version; none before the first publish.
    pub current_version: Option<i32>,
    pub archived_at: Option<OffsetDateTime>,
    /// Whether this is the workspace's default theme.
    pub is_default: bool,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The one editable copy of a theme's tokens.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct ThemeDraft {
    pub theme_id: Uuid,
    /// The tokens. The caller checks their shape; only a publish validates them fully.
    pub tokens: Value,
    /// How many times the draft has been saved; the first save is 1.
    pub revision: i32,
    pub updated_by: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

macro_rules! theme_columns {
    () => {
        "id, name, current_version, archived_at, \
         EXISTS (SELECT 1 FROM workspace_default_theme d \
                 WHERE d.workspace_id = themes.workspace_id AND d.theme_id = themes.id) \
            AS is_default, \
         created_by, created_at, updated_at"
    };
}

macro_rules! draft_columns {
    () => {
        "theme_id, tokens, revision, updated_by, updated_at"
    };
}

pub(super) use theme_columns;

/// A theme to create, with its first draft.
#[derive(Clone, Debug)]
pub struct NewTheme {
    pub id: Uuid,
    pub name: String,
    /// The first draft.
    pub tokens: Value,
}

/// The settings to change; `None` leaves one as it is.
#[derive(Clone, Debug, Default)]
pub struct ThemePatch {
    pub name: Option<String>,
}

/// Why a theme write was refused.
#[derive(Debug)]
pub enum ThemeError {
    /// No such theme in the workspace.
    NotFound,
    /// The theme is archived and no longer changes.
    Archived,
    /// The draft was saved since the caller read it.
    RevisionMismatch,
    /// The workspace default cannot be archived.
    IsDefault,
    /// The theme has no published version, so it cannot be the default.
    NotPublished,
    /// A name already taken is [`StoreError::is_conflict`].
    Store(StoreError),
}

impl From<StoreError> for ThemeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for ThemeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such theme"),
            Self::Archived => formatter.write_str("the theme is archived"),
            Self::RevisionMismatch => formatter.write_str("the theme draft changed"),
            Self::IsDefault => formatter.write_str("the theme is the workspace default"),
            Self::NotPublished => formatter.write_str("the theme has no published version"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ThemeError {}

/// Creates a theme with its first draft. A live name already taken, ignoring case, is a conflict.
pub async fn create_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewTheme,
    created_by: Option<UserId>,
) -> Result<(Theme, ThemeDraft), StoreError> {
    sqlx::query("INSERT INTO themes (id, workspace_id, name, created_by) VALUES ($1, $2, $3, $4)")
        .bind(new.id)
        .bind(workspace.as_uuid())
        .bind(&new.name)
        .bind(created_by.map(|user| user.as_uuid()))
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let draft: ThemeDraft = sqlx::query_as(concat!(
        "INSERT INTO theme_drafts (workspace_id, theme_id, tokens, updated_by) \
         VALUES ($1, $2, $3, $4) RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(new.id)
    .bind(&new.tokens)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let theme = reread(connection, workspace, new.id).await?;
    Ok((theme, draft))
}

/// The theme just written, which must exist.
async fn reread(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Theme, StoreError> {
    find_theme(connection, workspace, id)
        .await?
        .ok_or_else(|| StoreError::query(sqlx::Error::RowNotFound))
}

/// The theme, if the workspace has it.
pub async fn find_theme<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Theme>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        theme_columns!(),
        " FROM themes WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's themes by name; archived ones only when asked for.
pub async fn list_themes<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Theme>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        theme_columns!(),
        " FROM themes WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The draft of a theme of the workspace.
pub async fn find_theme_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<ThemeDraft>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        draft_columns!(),
        " FROM theme_drafts WHERE workspace_id = $1 AND theme_id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Reads the theme and holds its row until the transaction ends, so two writes to one theme run
/// one after the other.
pub(super) async fn lock_theme_row(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Theme, ThemeError> {
    sqlx::query_as(concat!(
        "SELECT ",
        theme_columns!(),
        " FROM themes WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(ThemeError::NotFound)
}

/// [`lock_theme_row`], refusing an archived theme.
pub(super) async fn lock_live_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Theme, ThemeError> {
    let held = lock_theme_row(connection, workspace, id).await?;
    if held.archived_at.is_some() {
        return Err(ThemeError::Archived);
    }
    Ok(held)
}

/// Changes the given settings of a theme that is not archived.
pub async fn update_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &ThemePatch,
) -> Result<Theme, ThemeError> {
    lock_live_theme(connection, workspace, id).await?;
    sqlx::query(
        "UPDATE themes SET name = COALESCE($3, name), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.name.as_deref())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(reread(connection, workspace, id).await?)
}

/// Replaces the draft of a theme that is not archived and counts the save. The tokens are not
/// validated here: a theme is edited in steps, and the publish checks it. With
/// `expected_revision`, the save only happens if the draft is still at that revision, else it is
/// [`ThemeError::RevisionMismatch`].
pub async fn save_theme_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    tokens: &Value,
    expected_revision: Option<i32>,
    updated_by: Option<UserId>,
) -> Result<ThemeDraft, ThemeError> {
    lock_live_theme(connection, workspace, id).await?;
    let draft = sqlx::query_as(concat!(
        "UPDATE theme_drafts SET tokens = $3, revision = revision + 1, updated_by = $4, \
            updated_at = now() \
         WHERE workspace_id = $1 AND theme_id = $2 AND ($5::integer IS NULL OR revision = $5) \
         RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(tokens)
    .bind(updated_by.map(|user| user.as_uuid()))
    .bind(expected_revision)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(ThemeError::RevisionMismatch)?;
    Ok(draft)
}

/// Archives a theme. Nothing is deleted: the draft and the versions stay readable, and a template
/// that names the theme still renders with it. Refused while the theme is the workspace default.
pub async fn archive_theme(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Theme, ThemeError> {
    let held = lock_live_theme(connection, workspace, id).await?;
    if held.is_default {
        return Err(ThemeError::IsDefault);
    }
    sqlx::query(
        "UPDATE themes SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(reread(connection, workspace, id).await?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::templates::tests_support::workspace;
    use crate::themes::tests_support::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_theme_starts_with_its_draft_and_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Brand").await;

        let draft = find_theme_draft(&pool, ws, theme.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(theme.current_version, None);
        assert!(!theme.is_default);
        assert_eq!((draft.revision, draft.tokens), (1, tokens("#112233")));
        assert_eq!(find_theme(&pool, ws, theme.id).await.unwrap(), Some(theme));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_ignoring_case_and_an_archived_name_is_free(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = create(&pool, ws, "Brand").await;
        let mut connection = pool.acquire().await.unwrap();

        let clash = create_theme(&mut connection, ws, &new_theme("BRAND"), None).await;
        archive_theme(&mut connection, ws, first.id).await.unwrap();
        let reuse = create_theme(&mut connection, ws, &new_theme("brand"), None).await;

        assert!(clash.unwrap_err().is_conflict());
        assert!(reuse.is_ok());
        let live = list_themes(&pool, ws, false).await.unwrap();
        let all = list_themes(&pool, ws, true).await.unwrap();
        assert_eq!((live.len(), all.len()), (1, 2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn saving_counts_the_revision_and_a_stale_revision_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Brand").await;
        let mut connection = pool.acquire().await.unwrap();

        let second = save_theme_draft(
            &mut connection,
            ws,
            theme.id,
            &tokens("#222222"),
            Some(1),
            None,
        )
        .await
        .unwrap();
        let stale = save_theme_draft(
            &mut connection,
            ws,
            theme.id,
            &tokens("#333333"),
            Some(1),
            None,
        )
        .await;
        let unconditional = save_theme_draft(
            &mut connection,
            ws,
            theme.id,
            &tokens("#444444"),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!((second.revision, second.tokens), (2, tokens("#222222")));
        assert!(matches!(stale, Err(ThemeError::RevisionMismatch)));
        assert_eq!(unconditional.revision, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_theme_no_longer_changes(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let theme = create(&pool, ws, "Brand").await;
        let mut connection = pool.acquire().await.unwrap();
        archive_theme(&mut connection, ws, theme.id).await.unwrap();

        let rename = update_theme(
            &mut connection,
            ws,
            theme.id,
            &ThemePatch {
                name: Some("New".into()),
            },
        )
        .await;
        let save = save_theme_draft(&mut connection, ws, theme.id, &json!({}), None, None).await;
        let again = archive_theme(&mut connection, ws, theme.id).await;

        assert!(matches!(rename, Err(ThemeError::Archived)));
        assert!(matches!(save, Err(ThemeError::Archived)));
        assert!(matches!(again, Err(ThemeError::Archived)));
        assert!(
            find_theme_draft(&pool, ws, theme.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_has_no_such_theme(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let theme = create(&pool, ws, "Brand").await;
        let mut connection = pool.acquire().await.unwrap();

        assert_eq!(find_theme(&pool, other, theme.id).await.unwrap(), None);
        assert!(matches!(
            archive_theme(&mut connection, other, theme.id).await,
            Err(ThemeError::NotFound)
        ));
    }
}
