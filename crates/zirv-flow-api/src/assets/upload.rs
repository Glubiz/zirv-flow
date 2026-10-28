//! Uploading an image. The declared type is judged before the body is read, the body is cut off
//! at the size limit while it streams, and only its header is read to check the type and the
//! pixel size.

use axum::Json;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::json;
use zirv_flow_store::{AssetError, NewAsset, begin, commit, store_asset};

use super::sniff::{ImageKind, SniffError, sniff};
use super::{AssetBody, AssetLimits, MAX_PIXELS, MAX_SIDE_PIXELS};
use crate::AppState;
use crate::admin;
use crate::auth::{Need, Principal, authorize, path_id};
use crate::idempotency::{self, Keyed, RequestHash};
use crate::problem::{ApiError, Problem};

const MAX_NAME_CHARS: usize = 200;

/// The name the query gives the image.
fn parse_name(query: &str) -> Result<String, ApiError> {
    let name = url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.trim().to_owned())
        .ok_or_else(|| ApiError::bad_request("name is required"))?;
    let length = name.chars().count();
    if length == 0
        || length > MAX_NAME_CHARS
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(ApiError::unprocessable(
            "name: must be 1 to 200 characters without path separators or control characters",
        ));
    }
    Ok(name)
}

fn unsupported() -> ApiError {
    ApiError::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "expected content-type image/png, image/jpeg, image/gif or image/webp",
    )
    .with_code("asset_type_unsupported")
}

fn mismatch(detail: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, detail).with_code("asset_type_mismatch")
}

fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "the image is too large")
        .with_code("asset_too_large")
}

/// The image type the `Content-Type` header declares.
fn declared_kind(headers: &HeaderMap) -> Result<ImageKind, ApiError> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .and_then(|media| ImageKind::from_media_type(media.trim()))
        .ok_or_else(unsupported)
}

/// Reads the body up to the limit, refusing one that is longer, stalls or breaks off.
async fn receive(
    body: axum::body::Body,
    limits: AssetLimits,
    declared: Option<u64>,
) -> Result<Vec<u8>, ApiError> {
    let reserve = declared.unwrap_or(0).min(limits.max_file_bytes);
    let mut bytes = Vec::with_capacity(usize::try_from(reserve).unwrap_or(0));
    let mut stream = body.into_data_stream();
    loop {
        let next = tokio::time::timeout(limits.idle_timeout, stream.next())
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::REQUEST_TIMEOUT,
                    "no data arrived for too long; the upload was abandoned",
                )
            })?;
        let Some(piece) = next else { break };
        let piece = piece.map_err(|_| ApiError::bad_request("the upload was interrupted"))?;
        if (bytes.len() as u64).saturating_add(piece.len() as u64) > limits.max_file_bytes {
            return Err(too_large());
        }
        bytes.extend_from_slice(&piece);
    }
    Ok(bytes)
}

/// Checks the file against the declared type and the size limits, from its header alone.
fn check(bytes: &[u8], declared: ImageKind) -> Result<(u32, u32), ApiError> {
    let sniffed = sniff(bytes).map_err(|error| match error {
        SniffError::NotAnImage => mismatch("the body is not a PNG, JPEG, GIF or WebP image"),
        SniffError::Unreadable => mismatch("the image header is cut off or malformed"),
    })?;
    if sniffed.kind != declared {
        return Err(mismatch(
            "the body is a different image type than its content-type says",
        ));
    }
    let (width, height) = (sniffed.width, sniffed.height);
    if width > MAX_SIDE_PIXELS
        || height > MAX_SIDE_PIXELS
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(ApiError::unprocessable(
            "the image is over 8192 pixels on a side or 40 megapixels",
        )
        .with_code("asset_dimensions_exceeded"));
    }
    Ok((width, height))
}

/// Uploads an image. The body is the raw file with its own `Content-Type`: `image/png`,
/// `image/jpeg`, `image/gif` or `image/webp`; SVG and every other type are refused before the
/// body is read. The type must match the file's magic bytes. The file may be at most
/// `assets.max_file_bytes` (5 MiB by default) and 8192 pixels a side and 40 megapixels, read from
/// its header. The workspace's images may take at most `assets.max_workspace_bytes` together. The
/// same bytes uploaded again to the workspace return the existing asset with 200. Audited as
/// `asset.uploaded` with the id, name and size.
#[utoipa::path(
    post,
    operation_id = "upload_asset",
    path = "/api/v1/workspaces/{ws}/assets",
    tag = "assets",
    params(
        ("ws" = Uuid, Path, description = "Workspace id"),
        ("name" = String, Query, description = "A name for the image, 1 to 200 characters without path separators"),
        ("Idempotency-Key" = Option<String>, Header, description = "1 to 255 characters; makes a retry safe for 24 hours. The same key with the same file replays the first answer, with its status and body")
    ),
    request_body(content = Vec<u8>, content_type = "image/png", description = "The image file; image/jpeg, image/gif and image/webp are accepted too"),
    responses(
        (status = 201, description = "The new asset", body = AssetBody),
        (status = 200, description = "The workspace already had these bytes; the existing asset", body = AssetBody),
        (status = 400, description = "A missing name, or an upload that broke off", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "Not authenticated", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "A key without the write scope, or a missing CSRF token", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such workspace, or no access to it", body = Problem, content_type = "application/problem+json"),
        (status = 408, description = "No byte arrived for 60 seconds", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "asset_too_large: over assets.max_file_bytes; asset_quota_exceeded: the workspace is over assets.max_workspace_bytes", body = Problem, content_type = "application/problem+json"),
        (status = 415, description = "asset_type_unsupported: not a PNG, JPEG, GIF or WebP content type; asset_type_mismatch: the file is not the declared type", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "A bad name, asset_dimensions_exceeded, or an Idempotency-Key that is invalid or was used for another file (`idempotency_key_reused`)", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []), ("api_key" = []))
)]
pub(crate) async fn upload(
    State(state): State<AppState>,
    principal: Principal,
    Path(ws): Path<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Result<Response, ApiError> {
    let workspace = authorize(&state, &principal, path_id(&ws)?, Need::Edit).await?;
    let name = parse_name(&query.unwrap_or_default())?;
    let declared = declared_kind(request.headers())?;
    let keyed = Keyed::of(request.headers(), "upload_asset")?;
    let mut request_hash = RequestHash::new(request.method(), request.uri());
    request_hash.update(declared.content_type().as_bytes());
    let limits = state.assets;
    let length = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if length.is_some_and(|length| length > limits.max_file_bytes) {
        return Err(too_large());
    }
    let bytes = receive(request.into_body(), limits, length).await?;
    let (width, height) = check(&bytes, declared)?;
    request_hash.update(&bytes);
    let request_hash = request_hash.finish();
    let guard = match idempotency::enter(&state.pool, workspace.id, keyed, request_hash).await? {
        Ok(guard) => guard,
        Err(replay) => return Ok(replay),
    };

    let mut tx = begin(&state.pool).await?;
    let stored = store_asset(
        &mut tx,
        workspace.id,
        &NewAsset {
            name: &name,
            ext: declared.ext(),
            content_type: declared.content_type(),
            bytes: &bytes,
            width: i32::try_from(width).unwrap_or(i32::MAX),
            height: i32::try_from(height).unwrap_or(i32::MAX),
            created_by: principal.user_id(),
        },
        limits.max_workspace_bytes,
    )
    .await
    .map_err(|error| match error {
        AssetError::QuotaExceeded => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the workspace's images would pass assets.max_workspace_bytes",
        )
        .with_code("asset_quota_exceeded"),
        AssetError::Store(error) => error.into(),
    })?;
    if stored.created {
        admin::audit(
            &mut tx,
            &principal.context(),
            Some(workspace.id),
            "asset.uploaded",
            Some(("asset", stored.asset.id)),
            json!({
                "id": stored.asset.id,
                "size_bytes": stored.asset.size_bytes
            }),
        )
        .await?;
    }
    commit(tx).await?;
    let status = if stored.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let shown = AssetBody::new(&stored.asset, stored.references, state.public_url());
    let remembered = serde_json::to_value(&shown).map_err(|_| ApiError::internal())?;
    idempotency::finish(
        guard,
        request_hash,
        status,
        stored.asset.id,
        Some(&remembered),
    )
    .await?;
    Ok((status, Json(remembered)).into_response())
}

#[cfg(test)]
mod tests {
    use super::super::sniff::fixtures::{gif, png};
    use super::*;

    #[test]
    fn a_name_is_one_to_200_characters_without_separators_or_controls() {
        assert_eq!(parse_name("name=Spring%20banner").unwrap(), "Spring banner");
        assert_eq!(
            parse_name("").unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        for bad in [
            "name=",
            "name=a%2Fb",
            "name=a%5Cb",
            "name=a%00b",
            "name=%20",
        ] {
            assert_eq!(
                parse_name(bad).unwrap_err().status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{bad}"
            );
        }
        assert!(parse_name(&format!("name={}", "n".repeat(200))).is_ok());
        assert!(parse_name(&format!("name={}", "n".repeat(201))).is_err());
    }

    #[test]
    fn the_declared_type_must_be_an_allowed_image() {
        let headers = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert(CONTENT_TYPE, value.parse().unwrap());
            map
        };

        assert_eq!(
            declared_kind(&headers("image/png; charset=binary")).unwrap(),
            ImageKind::Png
        );
        for refused in ["image/svg+xml", "text/html", "application/octet-stream"] {
            let error = declared_kind(&headers(refused)).unwrap_err();
            assert_eq!(error.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert_eq!(error.code(), Some("asset_type_unsupported"));
        }
        assert!(declared_kind(&HeaderMap::new()).is_err());
    }

    #[test]
    fn the_file_must_be_the_declared_type_and_within_the_pixel_limits() {
        let code = |bytes: &[u8], kind| check(bytes, kind).unwrap_err().code();

        assert_eq!(
            check(&png(8192, 4881), ImageKind::Png).unwrap(),
            (8192, 4881)
        );
        assert_eq!(
            code(&png(8, 8), ImageKind::Jpeg),
            Some("asset_type_mismatch")
        );
        assert_eq!(
            code(&gif(8, 8), ImageKind::Png),
            Some("asset_type_mismatch")
        );
        assert_eq!(code(b"<svg/>", ImageKind::Png), Some("asset_type_mismatch"));
        assert_eq!(
            code(&png(8193, 1), ImageKind::Png),
            Some("asset_dimensions_exceeded")
        );
        assert_eq!(
            code(&png(1, 8193), ImageKind::Png),
            Some("asset_dimensions_exceeded")
        );
        assert_eq!(
            code(&png(8000, 5125), ImageKind::Png),
            Some("asset_dimensions_exceeded")
        );
    }
}
