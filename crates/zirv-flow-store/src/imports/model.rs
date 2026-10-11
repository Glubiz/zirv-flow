//! The import row: what it holds, how to read it, and the state changes the API makes.
//!
//! Nothing here logs or returns a cell of the file.

use std::fmt;

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::Mapping;

use crate::{PgConnection, StoreError};

/// Where an import is in its life.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportStatus {
    /// The file is still streaming in; the import is hidden from the history.
    Uploading,
    /// Uploaded; no mapping yet.
    Uploaded,
    Mapped,
    DryRunning,
    DryRunDone,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl ImportStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::Mapped => "mapped",
            Self::DryRunning => "dry_running",
            Self::DryRunDone => "dry_run_done",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "uploading" => Self::Uploading,
            "uploaded" => Self::Uploaded,
            "mapped" => Self::Mapped,
            "dry_running" => Self::DryRunning,
            "dry_run_done" => Self::DryRunDone,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Whether a job is working on the import.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::DryRunning | Self::Running)
    }

    /// Whether the import has ended for good.
    pub const fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for ImportStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An import as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub source: String,
    pub stream: String,
    pub file_name: String,
    pub file_sha256: [u8; 32],
    pub size_bytes: i64,
    pub delimiter: char,
    /// The column names, in file order.
    pub header: Vec<String>,
    pub mapping: Option<Mapping>,
    pub status: ImportStatus,
    pub rows_total: i64,
    pub rows_done: i64,
    pub rows_applied: i64,
    pub rows_unchanged: i64,
    pub rows_rejected: i64,
    pub reject_threshold: f64,
    pub error_code: Option<String>,
    pub created_by: Option<Uuid>,
    pub trace_id: Uuid,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct ImportRow {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    source: String,
    stream: String,
    file_name: String,
    file_sha256: Vec<u8>,
    size_bytes: i64,
    delimiter: String,
    header: Json<Vec<String>>,
    mapping: Option<Json<Mapping>>,
    status: String,
    rows_total: i64,
    rows_done: i64,
    rows_applied: i64,
    rows_unchanged: i64,
    rows_rejected: i64,
    reject_threshold: f64,
    error_code: Option<String>,
    created_by: Option<Uuid>,
    trace_id: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

/// The columns of [`ImportRow`].
macro_rules! import_columns {
    () => {
        "id, workspace_id, name, source, stream, file_name, file_sha256, size_bytes, \
         delimiter, header, mapping, status, rows_total, rows_done, rows_applied, \
         rows_unchanged, rows_rejected, reject_threshold, error_code, created_by, trace_id, \
         created_at, updated_at, started_at, finished_at"
    };
}

impl TryFrom<ImportRow> for Import {
    type Error = StoreError;

    fn try_from(row: ImportRow) -> Result<Self, StoreError> {
        let status = ImportStatus::parse(&row.status).ok_or_else(StoreError::poison)?;
        let file_sha256 =
            <[u8; 32]>::try_from(row.file_sha256.as_slice()).map_err(|_| StoreError::poison())?;
        let delimiter = row
            .delimiter
            .chars()
            .next()
            .ok_or_else(StoreError::poison)?;
        Ok(Self {
            id: row.id,
            workspace_id: row.workspace_id,
            name: row.name,
            source: row.source,
            stream: row.stream,
            file_name: row.file_name,
            file_sha256,
            size_bytes: row.size_bytes,
            delimiter,
            header: row.header.0,
            mapping: row.mapping.map(|mapping| mapping.0),
            status,
            rows_total: row.rows_total,
            rows_done: row.rows_done,
            rows_applied: row.rows_applied,
            rows_unchanged: row.rows_unchanged,
            rows_rejected: row.rows_rejected,
            reject_threshold: row.reject_threshold,
            error_code: row.error_code,
            created_by: row.created_by,
            trace_id: row.trace_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

pub(super) async fn fetch_import<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Import>, StoreError> {
    let row: Option<ImportRow> = sqlx::query_as(concat!(
        "SELECT ",
        import_columns!(),
        " FROM imports WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(Import::try_from).transpose()
}

/// One import of the workspace.
pub async fn find_import<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Import>, StoreError> {
    fetch_import(executor, workspace, id).await
}

/// The workspace's imports, newest first, at most `limit`, after the import `before` when
/// given. The flag is true when more existed.
pub async fn list_imports<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    before: Option<Uuid>,
    limit: u32,
) -> Result<(Vec<Import>, bool), StoreError> {
    let rows: Vec<ImportRow> = sqlx::query_as(concat!(
        "SELECT ",
        import_columns!(),
        " FROM imports WHERE workspace_id = $1 AND status <> 'uploading' \
           AND ($2::uuid IS NULL OR id < $2) \
         ORDER BY id DESC LIMIT $3"
    ))
    .bind(workspace.as_uuid())
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let mut imports = rows
        .into_iter()
        .map(Import::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let more = imports.len() > limit as usize;
    imports.truncate(limit as usize);
    Ok((imports, more))
}

/// Why a state change was refused.
#[derive(Debug)]
pub enum TransitionError {
    NotFound,
    /// The import is in a state that does not allow the change.
    WrongState(ImportStatus),
    Store(StoreError),
}

impl fmt::Display for TransitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such import"),
            Self::WrongState(status) => write!(formatter, "the import is {status}"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TransitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::NotFound | Self::WrongState(_) => None,
        }
    }
}

impl From<StoreError> for TransitionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// The counters a new phase starts from.
const RESET: &str = ", rows_done = 0, rows_applied = 0, rows_unchanged = 0, rows_rejected = 0";

/// A state change: the states it accepts, the state it leads to and what else it sets.
struct Change<'a> {
    from: &'a [ImportStatus],
    to: ImportStatus,
    /// More `SET` assignments, each led by a comma.
    set: &'a str,
    /// Whether the import must have a mapping.
    needs_mapping: bool,
    /// Kept as the import's `error_code` when given.
    error_code: Option<&'a str>,
}

impl<'a> Change<'a> {
    const fn new(from: &'a [ImportStatus], to: ImportStatus) -> Self {
        Self {
            from,
            to,
            set: "",
            needs_mapping: false,
            error_code: None,
        }
    }
}

/// Runs a state change. When no row changed, says whether the import is missing or in
/// another state.
async fn transition(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    change: &Change<'_>,
) -> Result<Import, TransitionError> {
    let allowed: Vec<&str> = change.from.iter().map(|status| status.as_str()).collect();
    let sql = format!(
        "UPDATE imports SET status = $4, updated_at = now(), \
            error_code = COALESCE($6::text, error_code){set} \
         WHERE workspace_id = $1 AND id = $2 AND status = ANY($3) \
           AND (NOT $5 OR mapping IS NOT NULL) \
         RETURNING {columns}",
        set = change.set,
        columns = import_columns!(),
    );
    let row: Option<ImportRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(workspace.as_uuid())
        .bind(id)
        .bind(&allowed)
        .bind(change.to.as_str())
        .bind(change.needs_mapping)
        .bind(change.error_code)
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    if let Some(row) = row {
        return Ok(Import::try_from(row)?);
    }
    match fetch_import(&mut *connection, workspace, id).await? {
        None => Err(TransitionError::NotFound),
        Some(import) => Err(TransitionError::WrongState(import.status)),
    }
}

/// Saves the mapping and moves the import to `mapped`. The counters and the rejects of an
/// earlier dry run are cleared. Allowed while the import is uploaded, mapped or dry-run.
pub async fn set_import_mapping(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    mapping: &Mapping,
) -> Result<Import, TransitionError> {
    let saved: Option<Uuid> = sqlx::query_scalar(
        "UPDATE imports SET mapping = $3, status = 'mapped', updated_at = now(), \
            rows_done = 0, rows_applied = 0, rows_unchanged = 0, rows_rejected = 0, \
            error_code = NULL \
         WHERE workspace_id = $1 AND id = $2 \
           AND status IN ('uploaded', 'mapped', 'dry_run_done') RETURNING id",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(Json(mapping))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if saved.is_some() {
        delete_rejects(connection, workspace, id).await?;
    }
    match fetch_import(&mut *connection, workspace, id).await? {
        None => Err(TransitionError::NotFound),
        Some(import) if saved.is_some() => Ok(import),
        Some(import) => Err(TransitionError::WrongState(import.status)),
    }
}

async fn delete_rejects(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM import_rejects WHERE workspace_id = $1 AND import_id = $2")
        .bind(workspace.as_uuid())
        .bind(id)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Moves a mapped import to `dry_running` and clears the counters and rejects of an earlier
/// dry run. The caller enqueues the job in the same transaction.
pub async fn start_import_dry_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Import, TransitionError> {
    let import = transition(
        connection,
        workspace,
        id,
        &Change {
            set: RESET,
            needs_mapping: true,
            ..Change::new(
                &[ImportStatus::Mapped, ImportStatus::DryRunDone],
                ImportStatus::DryRunning,
            )
        },
    )
    .await?;
    delete_rejects(connection, workspace, id).await?;
    Ok(import)
}

/// Moves a mapped import to `running`, stamps the start (the time its records are observed
/// at) and clears the counters and the dry run's rejects. The caller enqueues the job in the
/// same transaction.
pub async fn start_import_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Import, TransitionError> {
    let import = transition(
        connection,
        workspace,
        id,
        &Change {
            set: ", started_at = now(), rows_done = 0, rows_applied = 0, rows_unchanged = 0, \
                  rows_rejected = 0",
            needs_mapping: true,
            ..Change::new(
                &[ImportStatus::Mapped, ImportStatus::DryRunDone],
                ImportStatus::Running,
            )
        },
    )
    .await?;
    delete_rejects(connection, workspace, id).await?;
    Ok(import)
}

/// Cancels an import that has not ended. A running job stops at its next batch boundary,
/// since its checkpoint no longer matches.
pub async fn cancel_import(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Import, TransitionError> {
    transition(
        connection,
        workspace,
        id,
        &Change {
            set: ", finished_at = now()",
            ..Change::new(
                &[
                    ImportStatus::Uploaded,
                    ImportStatus::Mapped,
                    ImportStatus::DryRunning,
                    ImportStatus::DryRunDone,
                    ImportStatus::Running,
                ],
                ImportStatus::Cancelled,
            )
        },
    )
    .await
}

/// Ends a dry run that read the whole file.
pub async fn finish_import_dry_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Import, TransitionError> {
    transition(
        connection,
        workspace,
        id,
        &Change::new(&[ImportStatus::DryRunning], ImportStatus::DryRunDone),
    )
    .await
}

/// Ends a run that read the whole file.
pub async fn finish_import_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Import, TransitionError> {
    transition(
        connection,
        workspace,
        id,
        &Change {
            set: ", finished_at = now()",
            ..Change::new(&[ImportStatus::Running], ImportStatus::Succeeded)
        },
    )
    .await
}

/// Ends a dry run or a run that cannot go on, with a code that says why.
pub async fn fail_import(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    code: &'static str,
) -> Result<Import, TransitionError> {
    transition(
        connection,
        workspace,
        id,
        &Change {
            set: ", finished_at = now()",
            error_code: Some(code),
            ..Change::new(
                &[ImportStatus::DryRunning, ImportStatus::Running],
                ImportStatus::Failed,
            )
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_round_trip_through_text() {
        for status in [
            ImportStatus::Uploaded,
            ImportStatus::Mapped,
            ImportStatus::DryRunning,
            ImportStatus::DryRunDone,
            ImportStatus::Running,
            ImportStatus::Succeeded,
            ImportStatus::Failed,
            ImportStatus::Cancelled,
        ] {
            assert_eq!(ImportStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(ImportStatus::parse("paused"), None);
    }

    #[test]
    fn active_and_final_statuses_are_told_apart() {
        assert!(ImportStatus::Running.is_active());
        assert!(ImportStatus::DryRunning.is_active());
        assert!(!ImportStatus::Mapped.is_active());
        assert!(ImportStatus::Succeeded.is_final());
        assert!(ImportStatus::Cancelled.is_final());
        assert!(!ImportStatus::DryRunDone.is_final());
    }
}
