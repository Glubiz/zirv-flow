//! The rows an import refused. A reject carries codes; the row's cells (`raw`) are personal
//! data, only selected when the caller asks for them, kept 30 days.

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{PgConnection, StoreError};

/// Which pass wrote a reject.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportPhase {
    DryRun,
    Run,
}

impl ImportPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Run => "run",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "dry_run" => Some(Self::DryRun),
            "run" => Some(Self::Run),
            _ => None,
        }
    }
}

/// One thing wrong with a row: the column it concerns (empty when none) and a code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectReason {
    pub column: String,
    pub reason: &'static str,
}

/// A row to keep as rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedRow {
    /// The row as a spreadsheet numbers it: the header is row 1.
    pub row_number: i64,
    pub reasons: Vec<RejectReason>,
    /// The row's cells.
    pub raw: Vec<String>,
}

/// Keeps `rows` as the rejects of `phase`; a row seen again replaces its earlier reject.
pub(super) async fn insert_rejects(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    import: Uuid,
    phase: ImportPhase,
    rows: &[RejectedRow],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = rows.iter().map(|row| row.row_number).collect();
    let errors: Vec<Value> = rows
        .iter()
        .map(|row| {
            Value::Array(
                row.reasons
                    .iter()
                    .map(|reason| json!({"column": reason.column, "reason": reason.reason}))
                    .collect(),
            )
        })
        .collect();
    let raw: Vec<Value> = rows.iter().map(|row| json!(row.raw)).collect();
    sqlx::query(
        "INSERT INTO import_rejects (import_id, row_number, workspace_id, phase, errors, raw) \
         SELECT $1, t.row_number, $2, $3, t.errors, t.raw \
         FROM unnest($4::bigint[], $5::jsonb[], $6::jsonb[]) AS t(row_number, errors, raw) \
         ON CONFLICT (import_id, row_number) DO UPDATE \
            SET phase = EXCLUDED.phase, errors = EXCLUDED.errors, raw = EXCLUDED.raw, \
                created_at = now()",
    )
    .bind(import)
    .bind(workspace.as_uuid())
    .bind(phase.as_str())
    .bind(&numbers)
    .bind(&errors)
    .bind(&raw)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// A stored reason.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct StoredReason {
    pub column: String,
    pub reason: String,
}

/// A reject as read back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportReject {
    pub row_number: i64,
    pub phase: String,
    pub errors: Vec<StoredReason>,
    /// The row's cells; only present when asked for.
    pub raw: Option<Vec<String>>,
}

#[derive(FromRow)]
struct RejectRow {
    row_number: i64,
    phase: String,
    errors: Json<Vec<StoredReason>>,
    raw: Option<Json<Vec<String>>>,
}

/// The import's rejects in row order, at most `limit`, after row `after` when given. `phase`
/// narrows to one pass. The cells are selected only with `with_raw`. The flag is true when
/// more existed.
pub async fn list_import_rejects<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    import: Uuid,
    phase: Option<ImportPhase>,
    after: Option<i64>,
    limit: u32,
    with_raw: bool,
) -> Result<(Vec<ImportReject>, bool), StoreError> {
    let rows: Vec<RejectRow> = sqlx::query_as(
        "SELECT row_number, phase, errors, CASE WHEN $6 THEN raw END AS raw \
         FROM import_rejects \
         WHERE workspace_id = $1 AND import_id = $2 \
           AND ($3::text IS NULL OR phase = $3) AND ($4::bigint IS NULL OR row_number > $4) \
         ORDER BY row_number LIMIT $5",
    )
    .bind(workspace.as_uuid())
    .bind(import)
    .bind(phase.map(ImportPhase::as_str))
    .bind(after)
    .bind(i64::from(limit) + 1)
    .bind(with_raw)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let mut rejects: Vec<ImportReject> = rows
        .into_iter()
        .map(|row| ImportReject {
            row_number: row.row_number,
            phase: row.phase,
            errors: row.errors.0,
            raw: row.raw.map(|raw| raw.0),
        })
        .collect();
    let more = rejects.len() > limit as usize;
    rejects.truncate(limit as usize);
    Ok((rejects, more))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_round_trip_through_text() {
        for phase in [ImportPhase::DryRun, ImportPhase::Run] {
            assert_eq!(ImportPhase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(ImportPhase::parse("later"), None);
    }
}
