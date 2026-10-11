//! Import retention (architecture section 16.3): the uploaded file goes 7 days after the
//! import ends, the refused rows' cells 30 days after they were written. The import row, its
//! counters and its reject codes' count stay.

use crate::{PgPool, StoreError};

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
