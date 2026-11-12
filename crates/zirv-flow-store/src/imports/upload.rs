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

/// What one workspace may keep: the bytes of its stored import files and the imports that are
/// not yet running (`uploading`, `uploaded`, `mapped`, `dry_running` or `dry_run_done`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UploadQuota {
    pub max_workspace_bytes: u64,
    pub max_open: u32,
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
    /// The most bytes this upload may reach, from the quota when it began; none without one.
    allowed: Option<i64>,
    /// The workspace limit, for the check [`Self::finish`] repeats.
    limit: Option<i64>,
    settled: bool,
}

impl ImportUpload {
    /// Inserts the import in the `uploading` state, with no quota.
    pub async fn begin(
        pool: &PgPool,
        workspace: WorkspaceId,
        start: &UploadStart<'_>,
    ) -> Result<Self, StoreError> {
        Self::insert(pool, workspace, start, None).await
    }

    /// Like [`Self::begin`], but refuses the upload when the workspace already has
    /// `quota.max_open` open imports ([`StoreError::is_import_open_limit`]) or no room is left in
    /// its stored files ([`StoreError::is_import_quota`]). [`Self::write`] and [`Self::finish`]
    /// refuse a body that outgrows the room left.
    pub async fn begin_within(
        pool: &PgPool,
        workspace: WorkspaceId,
        start: &UploadStart<'_>,
        quota: &UploadQuota,
    ) -> Result<Self, StoreError> {
        Self::insert(pool, workspace, start, Some(quota)).await
    }

    async fn insert(
        pool: &PgPool,
        workspace: WorkspaceId,
        start: &UploadStart<'_>,
        quota: Option<&UploadQuota>,
    ) -> Result<Self, StoreError> {
        let id = Uuid::now_v7();
        let mut tx = begin(pool).await?;
        let (mut allowed, mut limit) = (None, None);
        if let Some(quota) = quota {
            lock_workspace_imports(&mut tx, workspace).await?;
            let (open, used) = import_usage(&mut tx, workspace).await?;
            if open >= i64::from(quota.max_open) {
                return Err(StoreError::import_open_limit());
            }
            let max = i64::try_from(quota.max_workspace_bytes).unwrap_or(i64::MAX);
            if used >= max {
                return Err(StoreError::import_quota());
            }
            (allowed, limit) = (Some(max - used), Some(max));
        }
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
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        crate::commit(tx).await?;
        Ok(Self {
            pool: pool.clone(),
            id,
            workspace,
            seq: 0,
            pending: Vec::with_capacity(CHUNK_BYTES),
            hasher: Sha256::new(),
            size: 0,
            allowed,
            limit,
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
        self.size = self
            .size
            .saturating_add(i64::try_from(bytes.len()).unwrap_or(i64::MAX));
        if self.allowed.is_some_and(|allowed| self.size > allowed) {
            return Err(StoreError::import_quota());
        }
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
        // The insert share-locks the import row, so an erase that fences the import (an exclusive
        // row lock) either waits for this chunk, which it then deletes, or is seen here, and no
        // chunk is written.
        let written = sqlx::query(
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) \
             SELECT id, $2, workspace_id, $4 FROM imports \
             WHERE workspace_id = $3 AND id = $1 AND status = 'uploading' FOR SHARE",
        )
        .bind(self.id)
        .bind(self.seq)
        .bind(self.workspace.as_uuid())
        .bind(&self.pending)
        .execute(&self.pool)
        .await
        .map_err(StoreError::query)?
        .rows_affected();
        if written != 1 {
            self.pending.clear();
            return Err(StoreError::erased());
        }
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
        if let Some(limit) = self.limit {
            // Uploads that ran side by side each saw the same room, so the total is checked
            // again, under the lock, against what the others have stored since.
            lock_workspace_imports(&mut tx, self.workspace).await?;
            let (_, used) = import_usage(&mut tx, self.workspace).await?;
            if used.saturating_add(self.size) > limit {
                return Err(StoreError::import_quota());
            }
        }
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

/// Serialises the quota checks of one workspace's uploads until the transaction ends.
async fn lock_workspace_imports(tx: &mut Tx, workspace: WorkspaceId) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('imports:' || $1::text, 0))")
        .bind(workspace.as_uuid())
        .execute(&mut **tx)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// The workspace's open imports (not yet running, the one that is uploading included) and the
/// bytes of the finished uploads whose file is still stored.
async fn import_usage(tx: &mut Tx, workspace: WorkspaceId) -> Result<(i64, i64), StoreError> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE status IN                     ('uploading', 'uploaded', 'mapped', 'dry_running', 'dry_run_done')),                 coalesce(sum(size_bytes) FILTER (WHERE status <> 'uploading' AND EXISTS (                     SELECT 1 FROM import_chunks c WHERE c.import_id = i.id AND c.seq = 0)), 0)::bigint          FROM imports i WHERE workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(StoreError::query)
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

/// Deletes an import that is still `uploading`, with its chunks: for a caller whose last step
/// after [`ImportUpload::finish`] failed and rolled the finishing statement back.
pub async fn discard_unfinished_import(pool: &PgPool, workspace: WorkspaceId, id: Uuid) {
    delete_unfinished(pool, workspace, id).await;
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

    async fn quota_workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("quota").unwrap();
        let slug = Slug::parse(&format!("w{}", Uuid::now_v7().simple())).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    /// Uploads `bytes` as one finished file under `quota`, or the error that refused it.
    async fn upload_within(
        pool: &PgPool,
        ws: WorkspaceId,
        bytes: &[u8],
        quota: &UploadQuota,
    ) -> Result<Uuid, StoreError> {
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
        let mut upload = ImportUpload::begin_within(pool, ws, &start, quota).await?;
        if let Err(error) = upload.write(bytes).await {
            upload.abort().await;
            return Err(error);
        }
        let (import, tx) = upload
            .finish(&UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: 0,
            })
            .await?;
        crate::commit(tx).await?;
        Ok(import.id)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_past_the_workspace_quota_is_refused_until_a_file_is_deleted(pool: PgPool) {
        let ws = quota_workspace(&pool).await;
        let quota = UploadQuota {
            max_workspace_bytes: 100,
            max_open: 50,
        };
        let first = upload_within(&pool, ws, &[b'x'; 60], &quota).await.unwrap();

        let refused = upload_within(&pool, ws, &[b'y'; 60], &quota)
            .await
            .unwrap_err();
        assert!(refused.is_import_quota(), "{refused}");
        let uploading: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM imports WHERE workspace_id = $1 AND status = 'uploading'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(uploading, 0, "the refused upload kept nothing");

        sqlx::query("DELETE FROM import_chunks WHERE import_id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        upload_within(&pool, ws, &[b'y'; 60], &quota).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_past_the_open_limit_is_refused_until_one_is_cancelled(pool: PgPool) {
        let ws = quota_workspace(&pool).await;
        let quota = UploadQuota {
            max_workspace_bytes: 1 << 30,
            max_open: 2,
        };
        let first = upload_within(&pool, ws, b"a", &quota).await.unwrap();
        upload_within(&pool, ws, b"b", &quota).await.unwrap();

        let refused = upload_within(&pool, ws, b"c", &quota).await.unwrap_err();
        assert!(refused.is_import_open_limit(), "{refused}");

        sqlx::query("UPDATE imports SET status = 'cancelled' WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        upload_within(&pool, ws, b"c", &quota).await.unwrap();
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
    async fn a_flush_after_the_erase_fenced_the_upload_is_refused_and_leaves_no_chunk(
        pool: PgPool,
    ) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let trace = TraceContext::root(Cause::api("t"));
        let start = UploadStart {
            name: "n",
            source: "csv",
            stream: "n",
            file_name: "n.csv",
            created_by: None,
            trace: &trace,
        };
        let mut upload = ImportUpload::begin(&pool, ws, &start).await.unwrap();
        let mut first = b"id\n42\n".to_vec();
        first.resize(CHUNK_BYTES, b'z');
        upload.write(&first).await.unwrap();
        let count = |pool: PgPool, id: Uuid| async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_chunks WHERE import_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        assert_eq!(count(pool.clone(), upload.id()).await, 1);

        let mut tx = begin(&pool).await.unwrap();
        let erased = crate::erase_import_values(&mut tx, ws, &["42".into()])
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        let refused = upload.write(&first).await.unwrap_err();

        assert_eq!(erased.chunks, 1);
        assert!(refused.is_erased());
        assert_eq!(count(pool.clone(), upload.id()).await, 0);
        let state: (String, Option<String>) =
            sqlx::query_as("SELECT status, error_code FROM imports WHERE id = $1")
                .bind(upload.id())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, ("failed".to_owned(), Some("erased".to_owned())));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_chunk_committed_while_the_fence_waits_is_deleted_with_the_rest(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let trace = TraceContext::root(Cause::api("t"));
        let start = UploadStart {
            name: "n",
            source: "csv",
            stream: "n",
            file_name: "n.csv",
            created_by: None,
            trace: &trace,
        };
        let mut upload = ImportUpload::begin(&pool, ws, &start).await.unwrap();
        let mut first = b"id\n42\n".to_vec();
        first.resize(CHUNK_BYTES, b'z');
        upload.write(&first).await.unwrap();
        // A second connection holds the import row, as an upload's flush does while it writes.
        let mut holder = begin(&pool).await.unwrap();
        sqlx::query("SELECT 1 FROM imports WHERE id = $1 FOR SHARE")
            .bind(upload.id())
            .execute(&mut *holder)
            .await
            .unwrap();
        let erase = tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                crate::erase_import_values(&mut tx, ws, &["42".into()])
                    .await
                    .unwrap();
                crate::commit(tx).await.unwrap();
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // The chunk lands while the fence waits for the share lock.
        sqlx::query(
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) VALUES ($1, 1, $2, $3)",
        )
        .bind(upload.id())
        .bind(ws.as_uuid())
        .bind(b"late".as_slice())
        .execute(&mut *holder)
        .await
        .unwrap();
        crate::commit(holder).await.unwrap();
        erase.await.unwrap();

        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM import_chunks WHERE import_id = $1")
                .bind(upload.id())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_of_exactly_one_chunk_makes_one_chunk(pool: PgPool) {
        let sizes = chunk_sizes(&pool, &vec![1_u8; CHUNK_BYTES], 3).await;

        assert_eq!(sizes, [CHUNK_BYTES]);
    }
}
