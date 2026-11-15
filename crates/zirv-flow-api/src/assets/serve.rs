//! The public route of an uploaded image: `GET /a/{sha256}.{ext}`. No authentication; the hash
//! of the bytes is the address. Anything that is not a live asset's exact address is a plain 404,
//! never the web app.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use zirv_flow_store::{find_served_asset, parse_sha256_hex, served_asset_type, sha256_hex};

use crate::AppState;
use crate::problem::ApiError;

const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; sandbox";

/// The hash and extension an `/a/` file name holds.
fn parse_file(file: &str) -> Option<(Vec<u8>, &str)> {
    let (hash, ext) = file.split_once('.')?;
    matches!(ext, "png" | "jpg" | "gif" | "webp").then_some(())?;
    Some((parse_sha256_hex(hash)?, ext))
}

/// Whether an `If-None-Match` value lists `etag`, or `*`, in the weak comparison a conditional GET
/// uses: a `W/` prefix is ignored.
fn matches_etag(if_none_match: &str, etag: &str) -> bool {
    if_none_match
        .split(',')
        .map(str::trim)
        .any(|listed| listed == "*" || listed.strip_prefix("W/").unwrap_or(listed) == etag)
}

/// Serves an uploaded image from any workspace with a live asset of this hash, with an immutable
/// cache lifetime, `nosniff`, a locked-down content security policy and `inline` disposition. The
/// hash of the bytes is the strong `ETag`, so a revalidation that lists it is answered 304 from a
/// lookup that does not read the bytes; a missing or deleted asset is still 404.
pub(crate) async fn serve(
    State(state): State<AppState>,
    Path(file): Path<String>,
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (hash, ext) = parse_file(&file).ok_or_else(ApiError::not_found)?;
    let etag = format!("\"{}\"", sha256_hex(&hash));
    let revalidating = request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| matches_etag(value, &etag));
    let (content_type, mut response) = if revalidating {
        let content_type = served_asset_type(&state.pool, &hash, ext)
            .await?
            .ok_or_else(ApiError::not_found)?;
        (content_type, StatusCode::NOT_MODIFIED.into_response())
    } else {
        let served = find_served_asset(&state.pool, &hash, ext)
            .await?
            .ok_or_else(ApiError::not_found)?;
        (served.content_type, served.bytes.into_response())
    };
    let content_type = HeaderValue::from_str(&content_type).map_err(|_| ApiError::internal())?;
    let headers = response.headers_mut();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(|_| ApiError::internal())?,
    );
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lowercase_hash_with_a_known_extension_is_an_address() {
        let hash = "ab".repeat(32);

        assert_eq!(parse_file(&format!("{hash}.png")).unwrap().1, "png");
        assert_eq!(parse_file(&format!("{hash}.webp")).unwrap().0.len(), 32);
        for bad in [
            format!("{hash}.svg"),
            format!("{hash}.png.png"),
            hash.clone(),
            format!("{}.png", hash.to_uppercase()),
            "unknown".to_owned(),
            ".png".to_owned(),
            format!("{}.png", &hash[2..]),
        ] {
            assert!(parse_file(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_listed_tag_matches_with_or_without_the_weak_prefix_and_a_star_matches_any() {
        let etag = "\"abc\"";

        for listed in ["\"abc\"", "W/\"abc\"", "\"x\", \"abc\"", "*", " \"abc\" "] {
            assert!(matches_etag(listed, etag), "{listed}");
        }
        for listed in ["", "\"abd\"", "abc", "\"abc\"x"] {
            assert!(!matches_etag(listed, etag), "{listed}");
        }
    }
}
