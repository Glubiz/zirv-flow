//! Import retention (architecture section 16.3): the uploaded file goes 7 days after the
//! import ends, the refused rows' cells 30 days after they were written. The import row, its
//! counters and its reject codes' count stay.

use zirv_flow_core::WorkspaceId;

use crate::by_value::field_pattern;
use crate::{PgConnection, PgPool, StoreError};

/// Days the stored file is kept after its import ended.
pub const CHUNK_RETENTION_DAYS: i32 = 7;

/// Days a reject is kept.
pub const REJECT_RETENTION_DAYS: i32 = 30;

/// Days an import that is not running may sit unchanged before it is given up on.
pub const ABANDONED_AFTER_DAYS: i32 = 7;

/// Days an upload that never finished may sit before it is deleted (a crash mid-body).
pub const UPLOAD_ABANDONED_AFTER_DAYS: i32 = 1;

/// Rejects deleted per statement, so a large backlog never holds one long lock.
const REJECT_BATCH: i64 = 10_000;

/// What a pass deleted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportRetention {
    pub chunks: u64,
    pub rejects: u64,
    /// Imports given up on: uploaded, mapped or dry-run and untouched for a week.
    pub abandoned: u64,
    /// Uploads that never finished and were deleted.
    pub uploads: u64,
}

/// Deletes the files of imports that ended more than [`CHUNK_RETENTION_DAYS`] days ago and the
/// rejects older than [`REJECT_RETENTION_DAYS`] days, by the database clock.
pub async fn delete_expired_import_data(pool: &PgPool) -> Result<ImportRetention, StoreError> {
    let uploads = sqlx::query(
        "DELETE FROM imports WHERE status = 'uploading' \n           AND created_at < now() - make_interval(days => $1)",
    )
    .bind(UPLOAD_ABANDONED_AFTER_DAYS)
    .execute(pool)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    // An import nobody is working on and nobody touched is cancelled, and its file goes now.
    let abandoned: Vec<uuid::Uuid> = sqlx::query_scalar(
        "UPDATE imports SET status = 'cancelled', error_code = 'abandoned', \n            finished_at = now(), updated_at = now() \n         WHERE status IN ('uploaded', 'mapped', 'dry_run_done') \n           AND updated_at < now() - make_interval(days => $1) RETURNING id",
    )
    .bind(ABANDONED_AFTER_DAYS)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let abandoned_chunks = sqlx::query("DELETE FROM import_chunks WHERE import_id = ANY($1)")
        .bind(&abandoned)
        .execute(pool)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
    let finished_chunks = sqlx::query(
        "DELETE FROM import_chunks WHERE import_id IN ( \
            SELECT id FROM imports \
            WHERE finished_at IS NOT NULL \
              AND finished_at < now() - make_interval(days => $1))",
    )
    .bind(CHUNK_RETENTION_DAYS)
    .execute(pool)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    let mut rejects = 0;
    loop {
        let deleted = sqlx::query(
            "DELETE FROM import_rejects WHERE ctid IN ( \
                SELECT ctid FROM import_rejects \
                WHERE created_at < now() - make_interval(days => $1) LIMIT $2)",
        )
        .bind(REJECT_RETENTION_DAYS)
        .bind(REJECT_BATCH)
        .execute(pool)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        rejects += deleted;
        if i64::try_from(deleted).unwrap_or(i64::MAX) < REJECT_BATCH {
            break;
        }
    }
    Ok(ImportRetention {
        chunks: finished_chunks + abandoned_chunks,
        rejects,
        abandoned: abandoned.len() as u64,
        uploads,
    })
}

/// What [`erase_import_values`] deleted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImportErasure {
    /// Chunks deleted: the whole file of every import with a chunk that held a needle.
    pub chunks: u64,
    pub rejects: u64,
}

/// Erases the erased person's values from the workspace's stored import data: the uploaded file
/// and the refused rows' cells. `needles` are the person's ids and identifier values, matched
/// as whole fields and ignoring case (an email also as a substring), by the rule in
/// [`crate::by_value`]: an external id `42` finds a `42` cell, never `1427`.
///
/// A file is chunked at 1 MiB and a value can straddle a boundary, so each chunk is matched
/// with the byte before it and the start of the next one, and an import with one matching chunk
/// loses all of its chunks: a file with a hole in it could not be run anyway. In the same
/// transaction, before the chunks go, every such import that has not ended is set `failed` with
/// the code `erased` (the file held an erased person's data). A worker that already read rows
/// then cannot commit them: the batch checkpoint locks the import row and requires `running`.
///
/// Cost: the workspace's chunks (a 7 day window) are read once per erase, each window decoded
/// once and tested against one combined pattern for all needles, never once per needle. The
/// CSV is matched as `escape` text, so a value with non-ASCII bytes matches itself, and a value
/// written in another spelling in the file is missed. The refused rows' cells (30 days) are
/// tested the same way, one scalar at a time.
///
/// Both deletes are bounded by the workspace (`import_chunks_workspace_idx`,
/// `import_rejects_workspace_idx`).
pub async fn erase_import_values(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    needles: &[String],
) -> Result<ImportErasure, StoreError> {
    if needles.is_empty() {
        return Ok(ImportErasure::default());
    }
    let chunks = sqlx::query(sqlx::AssertSqlSafe(format!(
        r"WITH pat AS (SELECT {encoded} AS re,
                (SELECT max(octet_length(x)) FROM unnest($2::text[]) AS x) AS len),
         hit AS (
            SELECT DISTINCT c.import_id FROM import_chunks c
            LEFT JOIN import_chunks p ON p.import_id = c.import_id AND p.seq = c.seq - 1
            LEFT JOIN import_chunks n ON n.import_id = c.import_id AND n.seq = c.seq + 1
            CROSS JOIN pat
            WHERE c.workspace_id = $1 AND encode(
                CASE WHEN c.seq = 0 THEN ''::bytea ELSE '\x58'::bytea
                    || coalesce(substring(p.data FROM octet_length(p.data)), ''::bytea) END
                || c.data
                || coalesce(substring(n.data FROM 1 FOR pat.len + 1)
                    || CASE WHEN octet_length(n.data) > pat.len + 1
                            THEN '\x58'::bytea ELSE ''::bytea END, ''::bytea),
                'escape') ~* pat.re),
         fenced AS (
            UPDATE imports SET status = 'failed', error_code = 'erased',
                finished_at = coalesce(finished_at, now()), updated_at = now()
            WHERE workspace_id = $1 AND id IN (SELECT import_id FROM hit)
              AND status NOT IN ('succeeded', 'failed', 'cancelled')
            RETURNING id)
         DELETE FROM import_chunks WHERE workspace_id = $1
            AND import_id IN (SELECT import_id FROM hit)",
        encoded = field_pattern("$2", true)
    )))
    .bind(workspace.as_uuid())
    .bind(needles)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    let rejects = sqlx::query(sqlx::AssertSqlSafe(format!(
        r"DELETE FROM import_rejects r WHERE r.workspace_id = $1 AND EXISTS (
            SELECT 1 FROM jsonb_path_query(r.raw, 'strict $.**') AS cell
            WHERE jsonb_typeof(cell) NOT IN ('object', 'array')
              AND (cell #>> '{{}}') ~* {plain})",
        plain = field_pattern("$2", false)
    )))
    .bind(workspace.as_uuid())
    .bind(needles)
    .execute(connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(ImportErasure { chunks, rejects })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_go_after_a_week_and_the_rejects_after_a_month() {
        assert_eq!(CHUNK_RETENTION_DAYS, 7);
        assert_eq!(REJECT_RETENTION_DAYS, 30);
        assert_eq!((ABANDONED_AFTER_DAYS, UPLOAD_ABANDONED_AFTER_DAYS), (7, 1));
    }
}
