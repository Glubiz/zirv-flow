//! Creating, reading, changing and archiving snippets and saving their drafts. Every function is
//! scoped to one workspace. A write runs in the caller's transaction, together with its audit
//! entry.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use super::where_used::{SnippetThemeUse, templates_holding, themes_using};
use crate::StoreError;

/// A snippet: its name and which version is current. The content is in its draft and its
/// versions.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Snippet {
    pub id: Uuid,
    pub name: String,
    /// The newest published version; none before the first publish.
    pub current_version: Option<i32>,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The one editable copy of a snippet's content.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SnippetDraft {
    pub snippet_id: Uuid,
    /// The content. It may be incomplete; only a publish validates it.
    pub content: Value,
    /// How many times the draft has been saved; the first save is 1.
    pub revision: i32,
    pub updated_by: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

macro_rules! snippet_columns {
    () => {
        "id, name, current_version, archived_at, created_by, created_at, updated_at"
    };
}

macro_rules! draft_columns {
    () => {
        "snippet_id, content, revision, updated_by, updated_at"
    };
}

pub(super) use snippet_columns;

/// A snippet to create, with its first draft.
#[derive(Clone, Debug)]
pub struct NewSnippet {
    pub id: Uuid,
    pub name: String,
    /// The first draft.
    pub content: Value,
}

/// The settings to change; `None` leaves one as it is.
#[derive(Clone, Debug, Default)]
pub struct SnippetPatch {
    pub name: Option<String>,
}

/// A template that holds a snippet block for a snippet.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct SnippetTemplate {
    pub id: Uuid,
    pub name: String,
    /// Whether the template's draft holds the block.
    pub in_draft: bool,
    /// Whether the template's current version holds the block.
    pub in_current: bool,
}

/// Why a snippet write was refused.
#[derive(Debug)]
pub enum SnippetError {
    /// No such snippet in the workspace.
    NotFound,
    /// The snippet is archived and no longer changes.
    Archived,
    /// The draft was saved since the caller read it.
    RevisionMismatch,
    /// Template drafts hold a block for the snippet, or the published version of a theme uses it
    /// as its footer, so it cannot be archived.
    InUse {
        templates: Vec<SnippetTemplate>,
        themes: Vec<SnippetThemeUse>,
    },
    /// A name already taken is [`StoreError::is_conflict`].
    Store(StoreError),
}

impl From<StoreError> for SnippetError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for SnippetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such snippet"),
            Self::Archived => formatter.write_str("the snippet is archived"),
            Self::RevisionMismatch => formatter.write_str("the snippet draft changed"),
            Self::InUse { .. } => {
                formatter.write_str("template drafts or theme footers use the snippet")
            }
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for SnippetError {}

/// Creates a snippet with its first draft. A live name already taken, ignoring case, is a
/// conflict.
pub async fn create_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewSnippet,
    created_by: Option<UserId>,
) -> Result<(Snippet, SnippetDraft), StoreError> {
    let snippet: Snippet = sqlx::query_as(concat!(
        "INSERT INTO snippets (id, workspace_id, name, created_by) VALUES ($1, $2, $3, $4) \
         RETURNING ",
        snippet_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(&new.name)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let draft: SnippetDraft = sqlx::query_as(concat!(
        "INSERT INTO snippet_drafts (workspace_id, snippet_id, content, updated_by) \
         VALUES ($1, $2, $3, $4) RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(new.id)
    .bind(&new.content)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok((snippet, draft))
}

/// The snippet, if the workspace has it.
pub async fn find_snippet<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Snippet>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        snippet_columns!(),
        " FROM snippets WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's snippets by name; archived ones only when asked for.
pub async fn list_snippets<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Snippet>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        snippet_columns!(),
        " FROM snippets WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The draft of a snippet of the workspace.
pub async fn find_snippet_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<SnippetDraft>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        draft_columns!(),
        " FROM snippet_drafts WHERE workspace_id = $1 AND snippet_id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Reads the snippet and holds its row until the transaction ends, so two writes to one snippet
/// run one after the other. Refuses an archived one.
pub(super) async fn lock_live_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Snippet, SnippetError> {
    let held: Snippet = sqlx::query_as(concat!(
        "SELECT ",
        snippet_columns!(),
        " FROM snippets WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(SnippetError::NotFound)?;
    if held.archived_at.is_some() {
        return Err(SnippetError::Archived);
    }
    Ok(held)
}

/// Changes the given settings of a snippet that is not archived.
pub async fn update_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &SnippetPatch,
) -> Result<Snippet, SnippetError> {
    lock_live_snippet(connection, workspace, id).await?;
    let snippet = sqlx::query_as(concat!(
        "UPDATE snippets SET name = COALESCE($3, name), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        snippet_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.name.as_deref())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(snippet)
}

/// Replaces the draft of a snippet that is not archived and counts the save. The content is not
/// validated: a snippet is edited in steps, and the publish checks it. With `expected_revision`,
/// the save only happens if the draft is still at that revision, else it is
/// [`SnippetError::RevisionMismatch`].
pub async fn save_snippet_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    content: &Value,
    expected_revision: Option<i32>,
    updated_by: Option<UserId>,
) -> Result<SnippetDraft, SnippetError> {
    lock_live_snippet(connection, workspace, id).await?;
    let draft = sqlx::query_as(concat!(
        "UPDATE snippet_drafts SET content = $3, revision = revision + 1, updated_by = $4, \
            updated_at = now() \
         WHERE workspace_id = $1 AND snippet_id = $2 AND ($5::integer IS NULL OR revision = $5) \
         RETURNING ",
        draft_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(content)
    .bind(updated_by.map(|user| user.as_uuid()))
    .bind(expected_revision)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(SnippetError::RevisionMismatch)?;
    Ok(draft)
}

/// Archives a snippet. Nothing is deleted: the draft and the versions stay readable. Refused while
/// the draft of a template that is not archived holds a block for the snippet, or the published
/// version of a theme that is not archived uses it as its footer.
pub async fn archive_snippet(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Snippet, SnippetError> {
    lock_live_snippet(connection, workspace, id).await?;
    let in_drafts: Vec<SnippetTemplate> = templates_holding(&mut *connection, workspace, id)
        .await?
        .into_iter()
        .filter(|template| template.in_draft)
        .collect();
    let themes = themes_using(&mut *connection, workspace, id, true).await?;
    if !in_drafts.is_empty() || !themes.is_empty() {
        return Err(SnippetError::InUse {
            templates: in_drafts,
            themes,
        });
    }
    let snippet = sqlx::query_as(concat!(
        "UPDATE snippets SET archived_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        snippet_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(snippet)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::snippets::tests_support::*;
    use crate::templates::tests_support::workspace;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_snippet_starts_with_its_draft_and_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Footer note").await;

        let draft = find_snippet_draft(&pool, ws, snippet.id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(snippet.current_version, None);
        assert_eq!((draft.revision, draft.content), (1, content("draft")));
        assert_eq!(
            find_snippet(&pool, ws, snippet.id).await.unwrap(),
            Some(snippet)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_ignoring_case_and_an_archived_name_is_free(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = create(&pool, ws, "Offer").await;
        let mut connection = pool.acquire().await.unwrap();

        let clash = create_snippet(&mut connection, ws, &new_snippet("OFFER"), None).await;
        archive_snippet(&mut connection, ws, first.id)
            .await
            .unwrap();
        let reuse = create_snippet(&mut connection, ws, &new_snippet("offer"), None).await;

        assert!(clash.unwrap_err().is_conflict());
        assert!(reuse.is_ok());
        let live = list_snippets(&pool, ws, false).await.unwrap();
        let all = list_snippets(&pool, ws, true).await.unwrap();
        assert_eq!((live.len(), all.len()), (1, 2));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn saving_counts_the_revision_and_a_stale_revision_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let mut connection = pool.acquire().await.unwrap();

        let second = save_snippet_draft(
            &mut connection,
            ws,
            snippet.id,
            &content("two"),
            Some(1),
            None,
        )
        .await
        .unwrap();
        let stale = save_snippet_draft(
            &mut connection,
            ws,
            snippet.id,
            &content("three"),
            Some(1),
            None,
        )
        .await;
        let unconditional = save_snippet_draft(
            &mut connection,
            ws,
            snippet.id,
            &content("four"),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!((second.revision, second.content), (2, content("two")));
        assert!(matches!(stale, Err(SnippetError::RevisionMismatch)));
        assert_eq!(unconditional.revision, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_archived_snippet_no_longer_changes(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let snippet = create(&pool, ws, "Offer").await;
        let mut connection = pool.acquire().await.unwrap();
        archive_snippet(&mut connection, ws, snippet.id)
            .await
            .unwrap();

        let rename = update_snippet(
            &mut connection,
            ws,
            snippet.id,
            &SnippetPatch {
                name: Some("New".into()),
            },
        )
        .await;
        let save = save_snippet_draft(
            &mut connection,
            ws,
            snippet.id,
            &json!({"blocks": []}),
            None,
            None,
        )
        .await;
        let again = archive_snippet(&mut connection, ws, snippet.id).await;

        assert!(matches!(rename, Err(SnippetError::Archived)));
        assert!(matches!(save, Err(SnippetError::Archived)));
        assert!(matches!(again, Err(SnippetError::Archived)));
        assert!(
            find_snippet_draft(&pool, ws, snippet.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspace_has_no_such_snippet(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let snippet = create(&pool, ws, "Offer").await;
        let mut connection = pool.acquire().await.unwrap();

        assert_eq!(find_snippet(&pool, other, snippet.id).await.unwrap(), None);
        assert!(matches!(
            archive_snippet(&mut connection, other, snippet.id).await,
            Err(SnippetError::NotFound)
        ));
    }
}
