//! Storing an uploaded file in chunks, and reading it back in order.
//!
//! No transaction is open while the body streams in. The import row goes in first, in the
//! `uploading` state with placeholders for what the file will tell; each full chunk is a
//! statement of its own on the pool; the end is one statement that fills in the hash, the size,
//! the header, the row count and the delimiter and moves the import to `uploaded`. An upload
//! that fails, breaks off or is dropped deletes the import, and the chunks go with it.

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgExecutor;
use sqlx::types::Json;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::model::{Import, fetch_import};
use crate::{PgPool, StoreError, Tx, begin};

/// The size of one stored chunk of the file.
pub const CHUNK_BYTES: usize = 1 << 20;

/// What is known about an upload before its body arrives.
#[derive(Clone, Debug)]
pub struct UploadStart<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub stream: &'a str,
    pub file_name: &'a str,
    pub created_by: Option<UserId>,
    pub trace: &'a TraceContext,
}

/// What the body turned out to hold.
#[derive(Clone, Debug)]
pub struct UploadScan<'a> {
    pub delimiter: char,
    pub header: &'a [String],
    /// Data rows, the header not counted.
    pub rows_total: i64,
}

/// An upload in progress. Dropping it unfinished deletes the import.
pub struct ImportUpload {
    pool: PgPool,
    id: Uuid,
    workspace: WorkspaceId,
    seq: i32,
    pending: Vec<u8>,
    hasher: Sha256,
    size: i64,
    settled: bool,
}

impl ImportUpload {
    /// Inserts the import in the `uploading` state.
    pub async fn begin(
        pool: &PgPool,
        workspace: WorkspaceId,
        start: &UploadStart<'_>,
    ) -> Result<Self, StoreError> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO imports \
                (id, workspace_id, name, source, stream, file_name, file_sha256, size_bytes, \
                 delimiter, header, rows_total, status, created_by, trace_id) \
             VALUES ($1, $2, $3, $4, $5, $6, decode(repeat('00', 32), 'hex'), 0, ',', '[]', 0, \
                     'uploading', $7, $8)",
        )
        .bind(id)
        .bind(workspace.as_uuid())
        .bind(start.name)
        .bind(start.source)
        .bind(start.stream)
        .bind(start.file_name)
        .bind(start.created_by.map(|user| user.as_uuid()))
        .bind(start.trace.trace_id.as_uuid())
        .execute(pool)
        .await
        .map_err(StoreError::query)?;
        Ok(Self {
            pool: pool.clone(),
            id,
            workspace,
            seq: 0,
            pending: Vec::with_capacity(CHUNK_BYTES),
            hasher: Sha256::new(),
            size: 0,
            settled: false,
        })
    }

    /// The id the import has.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Takes the next bytes of the file, storing every full chunk.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        self.hasher.update(bytes);
        self.size += i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let mut rest = bytes;
        while !rest.is_empty() {
            let room = CHUNK_BYTES - self.pending.len();
            let (now, later) = rest.split_at(room.min(rest.len()));
            self.pending.extend_from_slice(now);
            rest = later;
            if self.pending.len() == CHUNK_BYTES {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(self.id)
        .bind(self.seq)
        .bind(self.workspace.as_uuid())
        .bind(&self.pending)
        .execute(&self.pool)
        .await
        .map_err(StoreError::query)?;
        self.seq += 1;
        self.pending.clear();
        Ok(())
    }

    /// Stores the last chunk and, in one statement, the file's SHA-256, size, header, row count
    /// and delimiter; the import becomes `uploaded`. Returns it with a short transaction that
    /// has the statement's effect in it, so the caller can audit and commit together.
    pub async fn finish(mut self, scan: &UploadScan<'_>) -> Result<(Import, Tx), StoreError> {
        self.flush().await?;
        let digest: [u8; 32] = self.hasher.clone().finalize().into();
        let mut tx = begin(&self.pool).await?;
        let updated = sqlx::query(
            "UPDATE imports SET status = 'uploaded', file_sha256 = $3, size_bytes = $4, \
                delimiter = $5, header = $6, rows_total = $7, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2 AND status = 'uploading'",
        )
        .bind(self.workspace.as_uuid())
        .bind(self.id)
        .bind(digest.as_slice())
        .bind(self.size)
        .bind(scan.delimiter.to_string())
        .bind(Json(json!(scan.header)))
        .bind(scan.rows_total)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        if updated != 1 {
            return Err(StoreError::poison());
        }
        let import = fetch_import(&mut *tx, self.workspace, self.id)
            .await?
            .ok_or_else(StoreError::poison)?;
        self.settled = true;
        Ok((import, tx))
    }

    /// Stores a whole file held in memory: begin, write, finish and commit.
    pub async fn store(
        pool: &PgPool,
        workspace: WorkspaceId,
        start: &UploadStart<'_>,
        bytes: &[u8],
        scan: &UploadScan<'_>,
    ) -> Result<Import, StoreError> {
        let mut upload = Self::begin(pool, workspace, start).await?;
        let written = upload.write(bytes).await;
        if let Err(error) = written {
            upload.abort().await;
            return Err(error);
        }
        let (import, tx) = upload.finish(scan).await?;
        crate::commit(tx).await?;
        Ok(import)
    }

    /// Deletes the import and, by cascade, the chunks written so far.
    pub async fn abort(mut self) {
        self.settled = true;
        delete_unfinished(&self.pool, self.workspace, self.id).await;
    }
}

/// Deletes an import that is still `uploading`; failures are left to the hourly upkeep.
async fn delete_unfinished(pool: &PgPool, workspace: WorkspaceId, id: Uuid) {
    let _ = sqlx::query(
        "DELETE FROM imports WHERE workspace_id = $1 AND id = $2 AND status = 'uploading'",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .execute(pool)
    .await;
}

impl Drop for ImportUpload {
    /// A request dropped mid-body (the client went away) takes its half file with it.
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let (pool, workspace, id) = (self.pool.clone(), self.workspace, self.id);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { delete_unfinished(&pool, workspace, id).await });
        }
    }
}

/// Chunk `seq` of the import's file, or none past the end (or once the file was deleted).
pub async fn read_import_chunk<'e>(
    executor: impl PgExecutor<'e>,
    import: Uuid,
    seq: i32,
) -> Result<Option<Vec<u8>>, StoreError> {
    sqlx::query_scalar("SELECT data FROM import_chunks WHERE import_id = $1 AND seq = $2")
        .bind(import)
        .bind(seq)
        .fetch_optional(executor)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    #[test]
    fn a_chunk_is_a_mebibyte() {
        assert_eq!(CHUNK_BYTES, 1_048_576);
    }

    async fn chunk_sizes(pool: &PgPool, bytes: &[u8], pieces: usize) -> Vec<usize> {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse(&format!("w{}", Uuid::now_v7().simple())).unwrap();
        let ws = create_workspace(pool, &name, &slug).await.unwrap().id;
        let header = vec!["a".to_owned()];
        let trace = TraceContext::root(Cause::api("t"));
        let start = UploadStart {
            name: "n",
            source: "csv",
            stream: "n",
            file_name: "n.csv",
            created_by: None,
            trace: &trace,
        };
        let mut upload = ImportUpload::begin(pool, ws, &start).await.unwrap();
        for piece in bytes.chunks(bytes.len().div_ceil(pieces).max(1)) {
            upload.write(piece).await.unwrap();
        }
        let (import, tx) = upload
            .finish(&UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: 0,
            })
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        let sizes: Vec<i32> = sqlx::query_scalar(
            "SELECT octet_length(data) FROM import_chunks WHERE import_id = $1 ORDER BY seq",
        )
        .bind(import.id)
        .fetch_all(pool)
        .await
        .unwrap();
        sizes.into_iter().map(|size| size as usize).collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn chunks_are_full_whatever_the_size_of_the_pieces_written(pool: PgPool) {
        let data = vec![7_u8; CHUNK_BYTES * 2 + 5];

        let one_piece = chunk_sizes(&pool, &data, 1).await;
        let many = chunk_sizes(&pool, &data, 37).await;

        assert_eq!(one_piece, [CHUNK_BYTES, CHUNK_BYTES, 5]);
        assert_eq!(many, one_piece);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_of_exactly_one_chunk_makes_one_chunk(pool: PgPool) {
        let sizes = chunk_sizes(&pool, &vec![1_u8; CHUNK_BYTES], 3).await;

        assert_eq!(sizes, [CHUNK_BYTES]);
    }
}
