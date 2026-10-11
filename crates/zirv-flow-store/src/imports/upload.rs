//! Storing an uploaded file in chunks, and reading it back in order.
//!
//! The upload runs in one transaction: the chunks go in as the body arrives, the import row
//! goes in last, and a failure anywhere rolls the lot back, so no half file is ever kept.

use sha2::{Digest, Sha256};
use sqlx::PgExecutor;
use sqlx::types::Json;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, UserId, WorkspaceId};

use super::model::{Import, fetch_import};
use crate::{PgPool, StoreError, Tx, begin, commit};

/// The size of one stored chunk of the file.
pub const CHUNK_BYTES: usize = 1 << 20;

/// What the API learned about the file while it streamed in.
#[derive(Clone, Debug)]
pub struct NewImport<'a> {
    pub name: &'a str,
    pub source: &'a str,
    pub stream: &'a str,
    pub file_name: &'a str,
    pub delimiter: char,
    pub header: &'a [String],
    /// Data rows, the header not counted.
    pub rows_total: i64,
    pub created_by: Option<UserId>,
    pub trace: &'a TraceContext,
}

/// An upload in progress. Dropping it rolls everything back.
pub struct ImportUpload {
    tx: Tx,
    id: Uuid,
    workspace: WorkspaceId,
    seq: i32,
    pending: Vec<u8>,
    hasher: Sha256,
    size: i64,
}

impl ImportUpload {
    pub async fn begin(pool: &PgPool, workspace: WorkspaceId) -> Result<Self, StoreError> {
        Ok(Self {
            tx: begin(pool).await?,
            id: Uuid::now_v7(),
            workspace,
            seq: 0,
            pending: Vec::with_capacity(CHUNK_BYTES),
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// The id the import will have.
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// The upload's transaction, so the caller can audit before the commit.
    pub fn tx(&mut self) -> &mut Tx {
        &mut self.tx
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
        .execute(&mut *self.tx)
        .await
        .map_err(StoreError::query)?;
        self.seq += 1;
        self.pending.clear();
        Ok(())
    }

    /// Stores the last chunk and the import row, which holds the file's SHA-256 and size.
    pub async fn insert(&mut self, new: &NewImport<'_>) -> Result<Import, StoreError> {
        self.flush().await?;
        let digest: [u8; 32] = self.hasher.clone().finalize().into();
        let delimiter = new.delimiter.to_string();
        sqlx::query(
            "INSERT INTO imports \
                (id, workspace_id, name, source, stream, file_name, file_sha256, size_bytes, \
                 delimiter, header, rows_total, created_by, trace_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(self.id)
        .bind(self.workspace.as_uuid())
        .bind(new.name)
        .bind(new.source)
        .bind(new.stream)
        .bind(new.file_name)
        .bind(digest.as_slice())
        .bind(self.size)
        .bind(&delimiter)
        .bind(Json(new.header))
        .bind(new.rows_total)
        .bind(new.created_by.map(|user| user.as_uuid()))
        .bind(new.trace.trace_id.as_uuid())
        .execute(&mut *self.tx)
        .await
        .map_err(StoreError::query)?;
        fetch_import(&mut *self.tx, self.workspace, self.id)
            .await?
            .ok_or_else(StoreError::poison)
    }

    pub async fn commit(self) -> Result<(), StoreError> {
        commit(self.tx).await
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
        let mut upload = ImportUpload::begin(pool, ws).await.unwrap();
        for piece in bytes.chunks(bytes.len().div_ceil(pieces).max(1)) {
            upload.write(piece).await.unwrap();
        }
        let import = upload
            .insert(&NewImport {
                name: "n",
                source: "csv",
                stream: "n",
                file_name: "n.csv",
                delimiter: ',',
                header: &header,
                rows_total: 0,
                created_by: None,
                trace: &trace,
            })
            .await
            .unwrap();
        upload.commit().await.unwrap();
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
