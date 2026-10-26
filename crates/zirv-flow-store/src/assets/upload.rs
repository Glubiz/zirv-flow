//! Storing an uploaded image under the workspace quota.

use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use super::{ASSET_COLUMNS, Asset, AssetListing, LISTING_COLUMNS, workspace_asset_bytes};
use crate::StoreError;

/// An image to store. The caller has checked the type and read the dimensions from the header.
#[derive(Clone, Debug)]
pub struct NewAsset<'a> {
    pub name: &'a str,
    /// `png`, `jpg`, `gif` or `webp`.
    pub ext: &'a str,
    pub content_type: &'a str,
    pub bytes: &'a [u8],
    pub width: i32,
    pub height: i32,
    pub created_by: Option<UserId>,
}

/// A stored asset, and whether this upload created it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredAsset {
    pub asset: Asset,
    /// False when the workspace already had these bytes and the existing row came back.
    pub created: bool,
    /// The number of published versions that reference it.
    pub references: i64,
}

/// Why a store was refused.
#[derive(Debug)]
pub enum AssetError {
    /// The workspace's live assets plus this one would pass the limit.
    QuotaExceeded,
    Store(StoreError),
}

impl From<StoreError> for AssetError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Stores the image in the caller's transaction, together with its audit entry. The same bytes
/// already live in the workspace return the existing row. The quota check and the insert run
/// under a per-workspace advisory lock held to the end of the transaction, so two uploads cannot
/// both take the last bytes.
pub async fn store_asset(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewAsset<'_>,
    max_workspace_bytes: u64,
) -> Result<StoredAsset, AssetError> {
    let digest: [u8; 32] = Sha256::digest(new.bytes).into();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('assets:' || $1::text, 0))")
        .bind(workspace.as_uuid())
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let existing: Option<AssetListing> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LISTING_COLUMNS} \
         FROM assets WHERE workspace_id = $1 AND sha256 = $2 AND deleted_at IS NULL"
    )))
    .bind(workspace.as_uuid())
    .bind(digest.as_slice())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if let Some(listing) = existing {
        return Ok(StoredAsset {
            asset: listing.asset,
            created: false,
            references: listing.references,
        });
    }
    let size = i64::try_from(new.bytes.len()).unwrap_or(i64::MAX);
    let used = workspace_asset_bytes(&mut *connection, workspace).await?;
    let limit = i64::try_from(max_workspace_bytes).unwrap_or(i64::MAX);
    if used.saturating_add(size) > limit {
        return Err(AssetError::QuotaExceeded);
    }
    let asset: Asset = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO assets (id, workspace_id, sha256, ext, content_type, bytes, size_bytes, \
                             width, height, name, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING {ASSET_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(digest.as_slice())
    .bind(new.ext)
    .bind(new.content_type)
    .bind(new.bytes)
    .bind(size)
    .bind(new.width)
    .bind(new.height)
    .bind(new.name)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(StoredAsset {
        asset,
        created: true,
        references: 0,
    })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::assets::tests_support::{upload, workspace};
    use crate::assets::workspace_asset_bytes;
    use crate::{begin, commit};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_bytes_in_one_workspace_return_the_existing_row(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;

        let first = upload(&pool, ws, "a", b"bytes").await;
        let again = upload(&pool, ws, "b", b"bytes").await;
        let elsewhere = upload(&pool, other, "a", b"bytes").await;

        assert!(first.created);
        assert!(!again.created);
        assert_eq!(again.asset.id, first.asset.id);
        assert_eq!(again.asset.name, "a");
        assert!(elsewhere.created);
        assert_ne!(elsewhere.asset.id, first.asset.id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_parallel_uploads_at_the_quota_edge_admit_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let attempt = |bytes: &'static [u8]| {
            let pool = pool.clone();
            async move {
                let mut tx = begin(&pool).await.unwrap();
                let result = store_asset(
                    &mut tx,
                    ws,
                    &NewAsset {
                        name: "x",
                        ext: "png",
                        content_type: "image/png",
                        bytes,
                        width: 1,
                        height: 1,
                        created_by: None,
                    },
                    8,
                )
                .await;
                commit(tx).await.unwrap();
                result
            }
        };

        let (one, two) = tokio::join!(attempt(b"aaaaaa"), attempt(b"bbbbbb"));

        let admitted = [&one, &two].iter().filter(|r| r.is_ok()).count();
        let refused = [&one, &two]
            .iter()
            .filter(|r| matches!(r, Err(AssetError::QuotaExceeded)))
            .count();
        assert_eq!((admitted, refused), (1, 1));
        assert_eq!(workspace_asset_bytes(&pool, ws).await.unwrap(), 6);
    }
}
