//! The public route of an uploaded image: `GET /a/{sha256}.{ext}`. No authentication; the hash
//! of the bytes is the address. Anything that is not a live asset's exact address is a plain 404,
//! never the web app.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use zirv_flow_store::{find_served_asset, parse_sha256_hex};

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

/// Serves an uploaded image from any workspace with a live asset of this hash, with an immutable
/// cache lifetime, `nosniff`, a locked-down content security policy and `inline` disposition.
pub(crate) async fn serve(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    let (hash, ext) = parse_file(&file).ok_or_else(ApiError::not_found)?;
    let served = find_served_asset(&state.pool, &hash, ext)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let content_type =
        HeaderValue::from_str(&served.content_type).map_err(|_| ApiError::internal())?;
    let mut response = served.bytes.into_response();
    let headers = response.headers_mut();
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
}
