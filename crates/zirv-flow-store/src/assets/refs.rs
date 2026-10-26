//! The published versions that use an asset.

use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::AssetId;

use super::AssetOwnerKind;
use crate::StoreError;

/// Why a reference was refused.
#[derive(Debug)]
pub enum AssetReferenceError {
    /// This asset is deleted or does not exist.
    Missing(AssetId),
    Store(StoreError),
}

impl From<StoreError> for AssetReferenceError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Records that the published `version` of the owner uses these assets. The publish paths of
/// templates, themes and snippets call it inside their publish transaction. It holds each asset
/// row until that transaction ends, so a delete cannot slip in between; recording the same
/// reference twice changes nothing.
pub async fn record_references(
    connection: &mut PgConnection,
    owner_kind: AssetOwnerKind,
    owner_id: Uuid,
    version: i32,
    assets: &[AssetId],
) -> Result<(), AssetReferenceError> {
    for asset in assets {
        let live: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM assets WHERE id = $1 AND deleted_at IS NULL FOR SHARE",
        )
        .bind(asset.as_uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if live.is_none() {
            return Err(AssetReferenceError::Missing(*asset));
        }
        sqlx::query(
            "INSERT INTO asset_references (asset_id, owner_kind, owner_id, version) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(asset.as_uuid())
        .bind(owner_kind.as_str())
        .bind(owner_id)
        .bind(version)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::assets::tests_support::{upload, workspace};
    use crate::assets::{asset_owners, delete_asset};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn recording_a_reference_twice_keeps_one_row_and_a_deleted_asset_is_refused(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let asset = upload(&pool, ws, "a", b"bytes").await.asset;
        let owner = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        for _ in 0..2 {
            record_references(
                &mut connection,
                AssetOwnerKind::Template,
                owner,
                1,
                &[AssetId::from_uuid(asset.id)],
            )
            .await
            .unwrap();
        }
        record_references(
            &mut connection,
            AssetOwnerKind::Template,
            owner,
            2,
            &[AssetId::from_uuid(asset.id)],
        )
        .await
        .unwrap();

        let owners = asset_owners(&pool, asset.id).await.unwrap();

        assert_eq!(owners.iter().map(|o| o.version).collect::<Vec<_>>(), [1, 2]);
        let gone = upload(&pool, ws, "b", b"other").await.asset;
        delete_asset(&mut connection, ws, gone.id).await.unwrap();
        let refused = record_references(
            &mut connection,
            AssetOwnerKind::Snippet,
            owner,
            1,
            &[AssetId::from_uuid(gone.id)],
        )
        .await;
        assert!(matches!(refused, Err(AssetReferenceError::Missing(_))));
    }
}
