//! Image assets (architecture 11.3): the bytes of an uploaded image kept per workspace, the
//! published versions that reference it, and the lookup that serves it on `/a/`.
//!
//! The bytes live in `assets.bytes`. Deleting an asset marks it deleted and empties the bytes in
//! one statement, so the space is freed. An asset a published version references cannot be
//! deleted.
//!
//! Neither table is person-scoped: an asset is a file, `created_by` is a user, and a reference
//! names a template, theme or snippet version. The erasure scan and the identity merge have
//! nothing to do here.

mod refs;
mod serve;
mod upload;

use std::fmt::Write as _;

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

pub use refs::{AssetReferenceError, record_references};
pub use serve::{ServedAsset, find_served_asset};
pub use upload::{AssetError, NewAsset, StoredAsset, store_asset};

/// The owners that may reference an asset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetOwnerKind {
    Template,
    Theme,
    Snippet,
}

impl AssetOwnerKind {
    /// The value the database stores.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Theme => "theme",
            Self::Snippet => "snippet",
        }
    }
}

/// An asset without its bytes.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct Asset {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// The SHA-256 of the bytes.
    pub sha256: Vec<u8>,
    /// `png`, `jpg`, `gif` or `webp`.
    pub ext: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub width: i32,
    pub height: i32,
    pub name: String,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
}

/// An asset of the list, with how many published versions reference it.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct AssetListing {
    #[sqlx(flatten)]
    pub asset: Asset,
    pub references: i64,
}

/// A published version that references an asset.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct AssetOwner {
    /// `template`, `theme` or `snippet`.
    pub owner_kind: String,
    pub owner_id: Uuid,
    pub version: i32,
}

pub(crate) const ASSET_COLUMNS: &str = "id, workspace_id, sha256, ext, content_type, size_bytes, \
     width, height, name, created_by, created_at";

/// The lowercase hex of a hash.
pub fn sha256_hex(hash: &[u8]) -> String {
    let mut text = String::with_capacity(hash.len() * 2);
    for byte in hash {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// The hash a 64-character lowercase hex string stands for.
pub fn parse_sha256_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    (0..64)
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// Where an asset is served: `<public_url>/a/<sha256>.<ext>`, or the path alone when the server
/// has no public URL. `public_url` has no trailing slash.
pub fn asset_url(public_url: Option<&str>, sha256: &[u8], ext: &str) -> String {
    format!(
        "{}/a/{}.{ext}",
        public_url.unwrap_or_default(),
        sha256_hex(sha256)
    )
}

/// The bytes the workspace's live assets take.
pub async fn workspace_asset_bytes<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<i64, StoreError> {
    sqlx::query_scalar(
        "SELECT COALESCE(sum(size_bytes), 0)::bigint FROM assets \
         WHERE workspace_id = $1 AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// A page of the workspace's live assets, newest first: those with an id below `before`. The flag
/// says whether more follow.
pub async fn list_assets<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    before: Option<Uuid>,
    limit: u32,
) -> Result<(Vec<AssetListing>, bool), StoreError> {
    let mut assets: Vec<AssetListing> = sqlx::query_as(
        "SELECT id, workspace_id, sha256, ext, content_type, size_bytes, width, height, name, \
                created_by, created_at, \
                (SELECT count(*) FROM asset_references r WHERE r.asset_id = assets.id) \
                    AS \"references\" \
         FROM assets WHERE workspace_id = $1 AND deleted_at IS NULL \
           AND ($2::uuid IS NULL OR id < $2) \
         ORDER BY id DESC LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = assets.len() > limit as usize;
    assets.truncate(limit as usize);
    Ok((assets, more))
}

/// A live asset of the workspace.
pub async fn find_asset<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Asset>, StoreError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ASSET_COLUMNS} FROM assets \
         WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The published versions that reference an asset, in a fixed order.
pub async fn asset_owners<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
) -> Result<Vec<AssetOwner>, StoreError> {
    sqlx::query_as(
        "SELECT owner_kind, owner_id, version FROM asset_references WHERE asset_id = $1 \
         ORDER BY owner_kind, owner_id, version",
    )
    .bind(id)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Why a delete was refused.
#[derive(Debug)]
pub enum DeleteAssetError {
    NotFound,
    /// Published versions reference the asset.
    InUse(Vec<AssetOwner>),
    Store(StoreError),
}

impl From<StoreError> for DeleteAssetError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Deletes an asset: it is marked deleted and its bytes emptied in one statement. Refused while a
/// published version references it. Runs in the caller's transaction, together with its audit
/// entry; the row is held until the transaction ends, so a reference recorded meanwhile waits.
pub async fn delete_asset(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Asset, DeleteAssetError> {
    let held: Option<Asset> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ASSET_COLUMNS} FROM assets \
         WHERE workspace_id = $1 AND id = $2 AND deleted_at IS NULL FOR UPDATE"
    )))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let asset = held.ok_or(DeleteAssetError::NotFound)?;
    let owners = asset_owners(&mut *connection, id).await?;
    if !owners.is_empty() {
        return Err(DeleteAssetError::InUse(owners));
    }
    sqlx::query("UPDATE assets SET deleted_at = now(), bytes = ''::bytea WHERE id = $1")
        .bind(id)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(asset)
}

/// Fixtures for the tests of this module.
#[cfg(test)]
pub(crate) mod tests_support {
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};

    use super::{NewAsset, StoredAsset, store_asset};
    use crate::{begin, commit, create_workspace};

    pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    pub(crate) async fn upload(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        bytes: &[u8],
    ) -> StoredAsset {
        let mut tx = begin(pool).await.unwrap();
        let stored = store_asset(
            &mut tx,
            ws,
            &NewAsset {
                name,
                ext: "png",
                content_type: "image/png",
                bytes,
                width: 2,
                height: 3,
                created_by: None,
            },
            1 << 30,
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        stored
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::tests_support::{upload, workspace};
    use super::*;

    #[test]
    fn a_hash_round_trips_through_hex_and_builds_the_url() {
        let hash: Vec<u8> = (0..32).collect();
        let hex = sha256_hex(&hash);

        assert_eq!(hex.len(), 64);
        assert_eq!(parse_sha256_hex(&hex), Some(hash.clone()));
        assert_eq!(parse_sha256_hex(&hex.to_uppercase()), None);
        assert_eq!(parse_sha256_hex("abc"), None);
        assert_eq!(asset_url(None, &hash, "png"), format!("/a/{hex}.png"));
        assert_eq!(
            asset_url(Some("https://flow.example.com"), &hash, "jpg"),
            format!("https://flow.example.com/a/{hex}.jpg")
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_asset_tables_hold_no_person_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('assets', 'asset_references') \
               AND column_name LIKE '%person%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_referenced_asset_cannot_be_deleted_and_a_free_one_loses_its_bytes(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let used = upload(&pool, ws, "used", b"used-bytes").await.asset;
        let free = upload(&pool, ws, "free", b"free-bytes").await.asset;
        let owner = Uuid::now_v7();
        let mut connection = pool.acquire().await.unwrap();
        record_references(
            &mut connection,
            AssetOwnerKind::Template,
            owner,
            3,
            &[zirv_flow_core::AssetId::from_uuid(used.id)],
        )
        .await
        .unwrap();

        let refused = delete_asset(&mut connection, ws, used.id).await;
        let Err(DeleteAssetError::InUse(owners)) = refused else {
            panic!("expected InUse");
        };
        assert_eq!(
            owners,
            [AssetOwner {
                owner_kind: "template".into(),
                owner_id: owner,
                version: 3
            }]
        );
        delete_asset(&mut connection, ws, free.id).await.unwrap();

        let bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM assets WHERE id = $1")
            .bind(free.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(bytes.is_empty());
        assert!(find_asset(&pool, ws, free.id).await.unwrap().is_none());
        assert_eq!(workspace_asset_bytes(&pool, ws).await.unwrap(), 10);
        assert!(matches!(
            delete_asset(&mut connection, ws, free.id).await,
            Err(DeleteAssetError::NotFound)
        ));
        let missing = record_references(
            &mut connection,
            AssetOwnerKind::Theme,
            owner,
            1,
            &[zirv_flow_core::AssetId::from_uuid(free.id)],
        )
        .await;
        assert!(matches!(missing, Err(AssetReferenceError::Missing(_))));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_list_is_newest_first_in_pages_with_reference_counts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let a = upload(&pool, ws, "a", b"1").await.asset;
        let b = upload(&pool, ws, "b", b"2").await.asset;
        let c = upload(&pool, ws, "c", b"3").await.asset;
        let mut connection = pool.acquire().await.unwrap();
        record_references(
            &mut connection,
            AssetOwnerKind::Snippet,
            Uuid::now_v7(),
            1,
            &[zirv_flow_core::AssetId::from_uuid(b.id)],
        )
        .await
        .unwrap();

        let (page, more) = list_assets(&pool, ws, None, 2).await.unwrap();
        let (rest, more_rest) = list_assets(&pool, ws, Some(b.id), 2).await.unwrap();

        assert!(more);
        assert_eq!(
            page.iter().map(|l| l.asset.id).collect::<Vec<_>>(),
            [c.id, b.id]
        );
        assert_eq!(page[1].references, 1);
        assert_eq!(page[0].references, 0);
        assert!(!more_rest);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].asset.id, a.id);
    }
}
