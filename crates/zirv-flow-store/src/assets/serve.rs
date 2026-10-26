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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::assets::delete_asset;
    use crate::assets::tests_support::{upload, workspace};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn serving_finds_a_live_asset_in_any_workspace_by_hash_and_extension(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let first = upload(&pool, one, "a", b"shared").await.asset;
        let second = upload(&pool, two, "a", b"shared").await.asset;

        let served = find_served_asset(&pool, &first.sha256, "png")
            .await
            .unwrap();
        assert_eq!(served.unwrap().bytes, b"shared");
        assert!(
            find_served_asset(&pool, &first.sha256, "gif")
                .await
                .unwrap()
                .is_none()
        );
        let mut connection = pool.acquire().await.unwrap();
        delete_asset(&mut connection, one, first.id).await.unwrap();
        assert!(
            find_served_asset(&pool, &first.sha256, "png")
                .await
                .unwrap()
                .is_some()
        );
        delete_asset(&mut connection, two, second.id).await.unwrap();
        assert!(
            find_served_asset(&pool, &first.sha256, "png")
                .await
                .unwrap()
                .is_none()
        );
    }
}
