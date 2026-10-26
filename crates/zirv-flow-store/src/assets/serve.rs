//! The lookup behind the public `/a/{sha256}.{ext}` route.

use sqlx::PgExecutor;

use crate::StoreError;

/// What the public route sends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServedAsset {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A live asset with this hash and extension, from any workspace.
pub async fn find_served_asset<'e>(
    executor: impl PgExecutor<'e>,
    sha256: &[u8],
    ext: &str,
) -> Result<Option<ServedAsset>, StoreError> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT content_type, bytes FROM assets \
         WHERE sha256 = $1 AND ext = $2 AND deleted_at IS NULL LIMIT 1",
    )
    .bind(sha256)
    .bind(ext)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|(content_type, bytes)| ServedAsset {
        content_type,
        bytes,
    }))
}
