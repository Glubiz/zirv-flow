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
