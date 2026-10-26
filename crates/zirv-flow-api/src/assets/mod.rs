//! Image assets (architecture 11.3): upload a PNG, JPEG, GIF or WebP image, list and read the
//! workspace's images, delete one no published version uses, and serve them to the world from
//! `/a/{sha256}.{ext}`.
//!
//! The type comes from the declared `Content-Type` and must match the file's magic bytes; SVG is
//! refused. The pixel size is read from the header, never by decoding. No audit entry, log line
//! or problem body holds the bytes.

#[cfg(test)]
mod acceptance;
pub(crate) mod crud;
pub(crate) mod serve;
pub(crate) mod sniff;
pub(crate) mod upload;

use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_store::{Asset, AssetOwner, asset_url, sha256_hex};

use crate::problem::ApiError;

/// What the assets of a workspace may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssetLimits {
    /// The largest image, in bytes.
    pub max_file_bytes: u64,
    /// The most bytes the live images of one workspace may take.
    pub max_workspace_bytes: u64,
    /// How long an upload may go without a byte arriving before it is abandoned.
    pub idle_timeout: Duration,
}

impl Default for AssetLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 5 * 1024 * 1024,
            max_workspace_bytes: 1024 * 1024 * 1024,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

/// The widest or tallest an image may be, in pixels.
pub(crate) const MAX_SIDE_PIXELS: u32 = 8192;
/// The most pixels an image may have.
pub(crate) const MAX_PIXELS: u64 = 40_000_000;

/// An image asset.
#[derive(Serialize, ToSchema)]
pub(crate) struct AssetBody {
    id: Uuid,
    #[schema(example = "Spring banner")]
    name: String,
    #[schema(example = "image/png")]
    content_type: String,
    size_bytes: i64,
    width: i32,
    height: i32,
    /// The lowercase hex SHA-256 of the file.
    sha256: String,
    /// Where the world reaches the file: absolute when `server.public_url` is set, else the
    /// path `/a/<sha256>.<ext>`.
    url: String,
    /// How many published versions of templates, themes and snippets use it.
    references: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    created_at: OffsetDateTime,
}

impl AssetBody {
    pub(crate) fn new(asset: &Asset, references: i64, public_url: Option<&str>) -> Self {
        Self {
            id: asset.id,
            name: asset.name.clone(),
            content_type: asset.content_type.clone(),
            size_bytes: asset.size_bytes,
            width: asset.width,
            height: asset.height,
            sha256: sha256_hex(&asset.sha256),
            url: asset_url(public_url, &asset.sha256, &asset.ext),
            references,
            created_at: asset.created_at,
        }
    }
}

/// A published version that uses an asset.
#[derive(Serialize, ToSchema)]
pub(crate) struct AssetOwnerBody {
    /// `template`, `theme` or `snippet`.
    kind: String,
    id: Uuid,
    version: i32,
    /// The owner's current name; null when the owner no longer exists.
    name: Option<String>,
    /// Whether the owner is archived.
    archived: bool,
}

impl From<&AssetOwner> for AssetOwnerBody {
    fn from(owner: &AssetOwner) -> Self {
        Self {
            kind: owner.owner_kind.clone(),
            id: owner.owner_id,
            version: owner.version,
            name: owner.name.clone(),
            archived: owner.archived,
        }
    }
}

/// An asset with the versions that use it.
#[derive(Serialize, ToSchema)]
pub(crate) struct AssetDetailBody {
    #[serde(flatten)]
    asset: AssetBody,
    used_by: Vec<AssetOwnerBody>,
}

impl AssetDetailBody {
    pub(crate) fn new(asset: &Asset, owners: &[AssetOwner], public_url: Option<&str>) -> Self {
        let references = i64::try_from(owners.len()).unwrap_or(i64::MAX);
        Self {
            asset: AssetBody::new(asset, references, public_url),
            used_by: owners.iter().map(AssetOwnerBody::from).collect(),
        }
    }
}

/// A page of assets with the workspace's quota.
#[derive(Serialize, ToSchema)]
pub(crate) struct AssetsBody {
    /// Newest first.
    assets: Vec<AssetBody>,
    /// Pass as `before` for the next page; null when there is none.
    next_before: Option<Uuid>,
    /// The bytes the workspace's live assets take.
    used_bytes: i64,
    /// The most bytes they may take (`assets.max_workspace_bytes`).
    limit_bytes: u64,
    /// The largest single image (`assets.max_file_bytes`).
    max_file_bytes: u64,
}

impl AssetsBody {
    pub(crate) fn new(
        assets: Vec<AssetBody>,
        next_before: Option<Uuid>,
        used_bytes: i64,
        limits: AssetLimits,
    ) -> Self {
        Self {
            assets,
            next_before,
            used_bytes,
            limit_bytes: limits.max_workspace_bytes,
            max_file_bytes: limits.max_file_bytes,
        }
    }
}

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

/// The `limit` and `before` of a list request.
pub(crate) fn parse_page(query: &str) -> Result<(u32, Option<Uuid>), ApiError> {
    let mut limit = DEFAULT_LIMIT;
    let mut before = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "limit" => {
                limit = value
                    .parse::<u32>()
                    .ok()
                    .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                    .ok_or_else(|| ApiError::bad_request("limit must be 1 to 200"))?;
            }
            "before" => {
                before = Some(
                    value
                        .parse::<Uuid>()
                        .map_err(|_| ApiError::bad_request("before is not a valid cursor"))?,
                );
            }
            _ => {}
        }
    }
    Ok((limit, before))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_5_mib_a_file_and_1_gib_a_workspace() {
        let limits = AssetLimits::default();

        assert_eq!(limits.max_file_bytes, 5_242_880);
        assert_eq!(limits.max_workspace_bytes, 1_073_741_824);
    }

    #[test]
    fn a_page_query_has_defaults_and_bounds() {
        let id = Uuid::now_v7();

        assert_eq!(parse_page("").unwrap(), (50, None));
        assert_eq!(
            parse_page(&format!("limit=7&before={id}")).unwrap(),
            (7, Some(id))
        );
        assert!(parse_page("limit=0").is_err());
        assert!(parse_page("limit=201").is_err());
        assert!(parse_page("before=nope").is_err());
    }
}
