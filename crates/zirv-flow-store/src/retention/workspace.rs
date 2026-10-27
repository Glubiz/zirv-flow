//! A workspace's own, shorter windows for its messages and its decision log. A null column means
//! the deployment value. The API checks the bounds; the jobs read the lookup at the start of a
//! run and never keep a window longer than the deployment's.

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// A workspace's overrides, in days.
#[derive(Clone, Copy, Debug, Eq, FromRow, PartialEq)]
pub struct WorkspaceRetention {
    pub messages_days: Option<i32>,
    pub decision_days: Option<i32>,
    pub updated_at: OffsetDateTime,
    pub updated_by: Option<Uuid>,
}

/// The overrides of every workspace that has one, each capped at the deployment window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceWindows {
    pub messages: HashMap<WorkspaceId, u32>,
    pub decisions: HashMap<WorkspaceId, u32>,
}

/// The workspace's overrides; none when it never set any.
pub async fn find_workspace_retention(
    executor: impl PgExecutor<'_>,
    workspace: WorkspaceId,
) -> Result<Option<WorkspaceRetention>, StoreError> {
    sqlx::query_as(
        "SELECT messages_days, decision_days, updated_at, updated_by \
         FROM workspace_retention WHERE workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Replaces the workspace's overrides. Locks the workspace row first, so two writers queue and a
/// workspace that is gone is reported as `None`.
pub async fn set_workspace_retention(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    messages_days: Option<i32>,
    decision_days: Option<i32>,
    updated_by: Option<Uuid>,
) -> Result<Option<WorkspaceRetention>, StoreError> {
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.as_uuid())
            .fetch_optional(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    if locked.is_none() {
        return Ok(None);
    }
    sqlx::query_as(
        "INSERT INTO workspace_retention (workspace_id, messages_days, decision_days, updated_by) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (workspace_id) DO UPDATE SET messages_days = EXCLUDED.messages_days, \
            decision_days = EXCLUDED.decision_days, updated_at = now(), \
            updated_by = EXCLUDED.updated_by \
         RETURNING messages_days, decision_days, updated_at, updated_by",
    )
    .bind(workspace.as_uuid())
    .bind(messages_days)
    .bind(decision_days)
    .bind(updated_by)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Every workspace's own windows, each the shorter of the override and the deployment window
/// (`messages_cap` and `decision_cap`), so a deployment window lowered after an override was set
/// still wins. This is the lookup the `messages.retention` and `decisions.retention` jobs call.
pub async fn workspace_windows(
    executor: impl PgExecutor<'_>,
    messages_cap: u32,
    decision_cap: u32,
) -> Result<WorkspaceWindows, StoreError> {
    let rows: Vec<(Uuid, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT workspace_id, messages_days, decision_days FROM workspace_retention \
         WHERE messages_days IS NOT NULL OR decision_days IS NOT NULL",
    )
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let mut windows = WorkspaceWindows::default();
    for (workspace, messages, decisions) in rows {
        let workspace = WorkspaceId::from_uuid(workspace);
        if let Some(days) = messages {
            windows.messages.insert(
                workspace,
                u32::try_from(days).unwrap_or(1).min(messages_cap),
            );
        }
        if let Some(days) = decisions {
            windows.decisions.insert(
                workspace,
                u32::try_from(days).unwrap_or(1).min(decision_cap),
            );
        }
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{begin, commit, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_with_no_row_has_no_overrides(pool: PgPool) {
        let ws = workspace(&pool, "alpha").await;

        assert_eq!(find_workspace_retention(&pool, ws).await.unwrap(), None);
        assert_eq!(
            workspace_windows(&pool, 395, 395).await.unwrap(),
            WorkspaceWindows::default()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setting_replaces_both_overrides_and_null_returns_to_the_deployment(pool: PgPool) {
        let ws = workspace(&pool, "alpha").await;
        let mut tx = begin(&pool).await.unwrap();
        let first = set_workspace_retention(&mut tx, ws, Some(60), Some(90), None)
            .await
            .unwrap()
            .unwrap();
        let second = set_workspace_retention(&mut tx, ws, None, Some(45), None)
            .await
            .unwrap()
            .unwrap();
        commit(tx).await.unwrap();

        assert_eq!(
            (first.messages_days, first.decision_days),
            (Some(60), Some(90))
        );
        assert_eq!(
            (second.messages_days, second.decision_days),
            (None, Some(45))
        );
        let windows = workspace_windows(&pool, 395, 395).await.unwrap();
        assert!(windows.messages.is_empty());
        assert_eq!(windows.decisions.get(&ws), Some(&45));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lookup_never_exceeds_the_deployment_window(pool: PgPool) {
        let ws = workspace(&pool, "alpha").await;
        let mut tx = begin(&pool).await.unwrap();
        set_workspace_retention(&mut tx, ws, Some(300), Some(100), None)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        let windows = workspace_windows(&pool, 200, 400).await.unwrap();

        assert_eq!(windows.messages.get(&ws), Some(&200));
        assert_eq!(windows.decisions.get(&ws), Some(&100));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_workspace_is_reported_as_none(pool: PgPool) {
        let mut tx = begin(&pool).await.unwrap();

        let saved = set_workspace_retention(&mut tx, WorkspaceId::new(), Some(60), None, None)
            .await
            .unwrap();

        assert_eq!(saved, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn workspace_retention_has_no_person_column(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'workspace_retention' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name ~ '(email|phone|recipient|\\mto\\M)') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }
}
