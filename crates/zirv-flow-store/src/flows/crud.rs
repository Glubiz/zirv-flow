//! Creating, reading, listing and editing the draft of flows, and reading their versions. Every
//! function is scoped to one workspace. A write runs in the caller's transaction.

use std::fmt;

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use super::state::{FlowAction, FlowState};
use super::warmup::{Warmup, WarmupRow};
use crate::StoreError;

/// A flow: its lifecycle state and its editable draft. Published content is in its versions.
#[derive(Clone, Debug, PartialEq)]
pub struct Flow {
    pub id: Uuid,
    pub name: String,
    pub state: FlowState,
    /// The editable graph. It may be incomplete; only a publish validates it.
    pub draft_graph: Value,
    pub draft_settings: Value,
    /// The newest published version; none before the first publish.
    pub current_version: Option<i32>,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    /// The warm-up setting and the window it last started.
    pub warmup: Warmup,
}

/// One immutable published version of a flow.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct FlowVersion {
    pub flow_id: Uuid,
    pub version: i32,
    pub graph: Value,
    pub settings: Value,
    pub published_at: OffsetDateTime,
    pub published_by: Option<Uuid>,
    /// The trace of the request that published the version.
    pub trace_id: Uuid,
}

#[derive(FromRow)]
pub(super) struct FlowRow {
    id: Uuid,
    name: String,
    state: String,
    draft_graph: Value,
    draft_settings: Value,
    current_version: Option<i32>,
    archived_at: Option<OffsetDateTime>,
    created_by: Option<Uuid>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    #[sqlx(flatten)]
    warmup: WarmupRow,
}

impl FlowRow {
    pub(super) fn into_flow(self) -> Result<Flow, StoreError> {
        let state = FlowState::parse(&self.state).ok_or_else(|| {
            StoreError::query(sqlx::Error::Decode("a flow has an unknown state".into()))
        })?;
        Ok(Flow {
            id: self.id,
            name: self.name,
            state,
            draft_graph: self.draft_graph,
            draft_settings: self.draft_settings,
            current_version: self.current_version,
            archived_at: self.archived_at,
            created_by: self.created_by,
            created_at: self.created_at,
            updated_at: self.updated_at,
            warmup: self.warmup.into(),
        })
    }
}

macro_rules! flow_columns {
    () => {
        "id, name, state, draft_graph, draft_settings, current_version, archived_at, created_by, \
         created_at, updated_at, warmup_enabled, warmup_seconds, warmup_started_at, \
         warmup_opened_at"
    };
}

macro_rules! version_columns {
    () => {
        "flow_id, version, graph, settings, published_at, published_by, trace_id"
    };
}

pub(super) use flow_columns;
pub(super) use version_columns;

/// A flow to create. It starts as a draft.
#[derive(Clone, Debug)]
pub struct NewFlow {
    pub id: Uuid,
    pub name: String,
    pub graph: Value,
    pub settings: Value,
}

/// The draft fields to change; `None` leaves one as it is.
#[derive(Clone, Debug, Default)]
pub struct FlowPatch {
    pub name: Option<String>,
    pub graph: Option<Value>,
    pub settings: Option<Value>,
}

/// Why a flow write was refused.
#[derive(Debug)]
pub enum FlowError {
    /// No such flow in the workspace.
    NotFound,
    /// The lifecycle does not allow `action` in `state`.
    IllegalTransition {
        state: FlowState,
        action: FlowAction,
    },
    /// The flow is archived and its draft no longer changes.
    Archived,
    /// A warm-up length outside `MIN_WARMUP_SECONDS..=MAX_WARMUP_SECONDS`.
    WarmupOutOfBounds,
    /// Open now is refused: only a published flow admits the entries that wait.
    WarmupNotOpenable { state: FlowState },
    /// A name already taken is [`StoreError::is_conflict`].
    Store(StoreError),
}

impl From<StoreError> for FlowError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for FlowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such flow"),
            Self::IllegalTransition { state, action } => {
                write!(
                    formatter,
                    "the flow is {state} and cannot be {}",
                    action.past()
                )
            }
            Self::Archived => formatter.write_str("the flow is archived"),
            Self::WarmupOutOfBounds => {
                formatter.write_str("the warm-up length is outside its bounds")
            }
            Self::WarmupNotOpenable { state } => {
                write!(
                    formatter,
                    "the flow is {state}; only a published flow can be opened"
                )
            }
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for FlowError {}

/// A flow whose row this transaction holds `FOR UPDATE`. Only [`lock_flow`] makes one, so a
/// publish can only act on content that was read under the lock.
#[derive(Debug)]
pub struct LockedFlow {
    pub(super) flow: Flow,
}

impl LockedFlow {
    pub fn flow(&self) -> &Flow {
        &self.flow
    }
}

/// Creates a draft flow. A live name already taken, ignoring case, is a conflict.
pub async fn create_flow(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewFlow,
    created_by: Option<UserId>,
) -> Result<Flow, StoreError> {
    let row: FlowRow = sqlx::query_as(concat!(
        "INSERT INTO flows (id, workspace_id, name, draft_graph, draft_settings, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING ",
        flow_columns!()
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(&new.name)
    .bind(&new.graph)
    .bind(&new.settings)
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    row.into_flow()
}

/// Reads the flow and locks its row until the transaction ends. Writes to the flow wait here, so
/// two publishes of one flow run one after the other.
pub async fn lock_flow(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<LockedFlow, FlowError> {
    let row: FlowRow = sqlx::query_as(concat!(
        "SELECT ",
        flow_columns!(),
        " FROM flows WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?
    .ok_or(FlowError::NotFound)?;
    Ok(LockedFlow {
        flow: row.into_flow()?,
    })
}

pub async fn find_flow<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Flow>, StoreError> {
    let row: Option<FlowRow> = sqlx::query_as(concat!(
        "SELECT ",
        flow_columns!(),
        " FROM flows WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(FlowRow::into_flow).transpose()
}

/// The workspace's flows by name; archived ones only when asked for.
pub async fn list_flows<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Flow>, StoreError> {
    let rows: Vec<FlowRow> = sqlx::query_as(concat!(
        "SELECT ",
        flow_columns!(),
        " FROM flows WHERE workspace_id = $1 AND ($2 OR state <> 'archived') \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    rows.into_iter().map(FlowRow::into_flow).collect()
}

/// Changes the given draft fields of a flow that is not archived. The draft is not validated: a
/// flow is edited in steps, and the publish checks it.
pub async fn update_flow_draft(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    patch: &FlowPatch,
) -> Result<Flow, FlowError> {
    let held = lock_flow(connection, workspace, id).await?;
    if held.flow.state == FlowState::Archived {
        return Err(FlowError::Archived);
    }
    let row: FlowRow = sqlx::query_as(concat!(
        "UPDATE flows SET name = COALESCE($3, name), draft_graph = COALESCE($4, draft_graph), \
            draft_settings = COALESCE($5, draft_settings), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 RETURNING ",
        flow_columns!()
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(patch.name.as_deref())
    .bind(patch.graph.as_ref())
    .bind(patch.settings.as_ref())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(row.into_flow()?)
}

/// The versions of a flow of the workspace, newest first.
pub async fn list_flow_versions<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Vec<FlowVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM flow_versions WHERE workspace_id = $1 AND flow_id = $2 ORDER BY version DESC"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One version of a flow of the workspace.
pub async fn find_flow_version<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    version: i32,
) -> Result<Option<FlowVersion>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM flow_versions WHERE workspace_id = $1 AND flow_id = $2 AND version = $3"
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
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::flows::tests_support::*;
    use crate::flows::{FlowAction, FlowState};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_flow_is_a_draft_with_no_version(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let flow = create(&pool, ws, "Welcome").await;

        assert_eq!(flow.state, FlowState::Draft);
        assert_eq!(flow.current_version, None);
        assert_eq!(flow.draft_graph, graph("draft"));
        assert!(
            list_flow_versions(&pool, ws, flow.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(find_flow(&pool, ws, flow.id).await.unwrap(), Some(flow));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_ignoring_case_and_an_archived_name_is_free(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let first = create(&pool, ws, "Welcome").await;
        let new = |name: &str| NewFlow {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            graph: graph("x"),
            settings: settings(),
        };
        let mut connection = pool.acquire().await.unwrap();

        let duplicate = create_flow(&mut connection, ws, &new("WELCOME"), None).await;
        let elsewhere = create_flow(&mut connection, other, &new("Welcome"), None).await;
        drop(connection);
        lifecycle(&pool, ws, first.id, FlowAction::Archive)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        let reused = create_flow(&mut connection, ws, &new("Welcome"), None).await;

        assert!(duplicate.unwrap_err().is_conflict());
        assert!(elsewhere.is_ok());
        assert!(reused.is_ok());
        let live: Vec<String> = list_flows(&pool, ws, false)
            .await
            .unwrap()
            .into_iter()
            .map(|flow| flow.name)
            .collect();
        let all = list_flows(&pool, ws, true).await.unwrap();
        assert_eq!(live, ["Welcome"]);
        assert_eq!(all.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_is_scoped_to_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let flow = create(&pool, one, "Welcome").await;
        publish(&pool, one, flow.id).await.unwrap();

        assert!(find_flow(&pool, two, flow.id).await.unwrap().is_none());
        assert!(list_flows(&pool, two, true).await.unwrap().is_empty());
        assert!(
            find_flow_version(&pool, two, flow.id, 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            list_flow_versions(&pool, two, flow.id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            publish(&pool, two, flow.id).await,
            Err(FlowError::NotFound)
        ));
        assert!(matches!(
            lifecycle(&pool, two, flow.id, FlowAction::Archive).await,
            Err(FlowError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_the_flow_or_the_workspace_removes_the_versions(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let gone = create(&pool, one, "A").await;
        publish(&pool, one, gone.id).await.unwrap();
        let kept = create(&pool, one, "B").await;
        publish(&pool, one, kept.id).await.unwrap();
        let other = create(&pool, two, "C").await;
        publish(&pool, two, other.id).await.unwrap();

        sqlx::query("DELETE FROM flows WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(two.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(version_rows(&pool, gone.id).await, 0);
        assert_eq!(version_rows(&pool, kept.id).await, 1);
        assert_eq!(version_rows(&pool, other.id).await, 0);
    }
}
