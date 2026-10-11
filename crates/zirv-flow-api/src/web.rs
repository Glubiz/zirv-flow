//! The web UI: static files embedded in the binary, with history fallback.
//!
//! `build.rs` points `ZIRV_FLOW_WEB_ROOT` at `web/dist` when the app was built, else at the
//! committed placeholder page. Unknown `/api` paths stay problem+json errors.

use std::borrow::Cow;

use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::problem::ApiError;

#[derive(RustEmbed)]
#[folder = "$ZIRV_FLOW_WEB_ROOT"]
struct Assets;

const INDEX: &str = "index.html";
const ASSETS_PREFIX: &str = "assets/";

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

struct Asset {
    data: Cow<'static, [u8]>,
    mime: String,
}

fn embedded(path: &str) -> Option<Asset> {
    Assets::get(path).map(|file| Asset {
        mime: file.metadata.mimetype().to_owned(),
        data: file.data,
    })
}

/// The fallback of the router: the web app for everything that is not an API route.
pub(crate) async fn fallback(method: Method, uri: Uri) -> Response {
    respond(&method, uri.path(), embedded)
}

fn not_found() -> Response {
    ApiError::not_found().into_response()
}

fn respond(method: &Method, path: &str, lookup: impl Fn(&str) -> Option<Asset>) -> Response {
    if path == "/api" || path.starts_with("/api/") {
        return not_found();
    }
    if method != Method::GET && method != Method::HEAD {
        let mut response =
            ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return response;
    }

    let key = path.trim_start_matches('/');
    let safe = !key.contains('\\') && key.split('/').all(|part| part != "..");
    let file = if key.is_empty() || !safe {
        None
    } else {
        lookup(key).map(|asset| (key, asset))
    };
    let (cache, asset) = match file {
        Some((key, asset)) if key.starts_with(ASSETS_PREFIX) => (IMMUTABLE, asset),
        Some((_, asset)) => ("no-cache", asset),
        None if key.starts_with(ASSETS_PREFIX) => return not_found(),
        None => match lookup(INDEX) {
            Some(index) => ("no-cache", index),
            None => return not_found(),
        },
    };

    let mime = if asset.mime == "text/html" {
        "text/html; charset=utf-8".to_owned()
    } else {
        asset.mime
    };
    let body = if method == Method::HEAD {
        Vec::new()
    } else {
        asset.data.into_owned()
    };
    let mut response = body.into_response();
    let headers = response.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(&mime) {
        headers.insert(header::CONTENT_TYPE, mime);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::AppState;

    fn fake(path: &str) -> Option<Asset> {
        let (data, mime) = match path {
            "index.html" => ("<div id=\"root\"></div>", "text/html"),
            "assets/app-1a2b.js" => ("console.log(1)", "text/javascript"),
            "favicon.svg" => ("<svg/>", "image/svg+xml"),
            _ => return None,
        };
        Some(Asset {
            data: Cow::Borrowed(data.as_bytes()),
            mime: mime.to_owned(),
        })
    }

    async fn body(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn header_of(response: &Response, name: header::HeaderName) -> &str {
        response.headers()[name].to_str().unwrap()
    }

    #[tokio::test]
    async fn the_root_serves_index_html_with_the_security_headers() {
        let response = respond(&Method::GET, "/", fake);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "text/html; charset=utf-8"
        );
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
        assert_eq!(
            header_of(&response, header::CONTENT_SECURITY_POLICY),
            CONTENT_SECURITY_POLICY
        );
        assert_eq!(
            header_of(&response, header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(header_of(&response, header::REFERRER_POLICY), "same-origin");
        assert!(body(response).await.contains("id=\"root\""));
    }

    #[tokio::test]
    async fn a_deep_link_falls_back_to_index_html() {
        let response = respond(&Method::GET, "/w/abc/members", fake);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
        assert!(body(response).await.contains("id=\"root\""));
    }

    #[tokio::test]
    async fn a_hashed_asset_is_immutable_and_keeps_its_content_type() {
        let response = respond(&Method::GET, "/assets/app-1a2b.js", fake);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_of(&response, header::CACHE_CONTROL),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "text/javascript"
        );
        assert_eq!(body(response).await, "console.log(1)");
    }

    #[test]
    fn another_existing_file_is_served_without_the_immutable_header() {
        let response = respond(&Method::GET, "/favicon.svg", fake);

        assert_eq!(header_of(&response, header::CONTENT_TYPE), "image/svg+xml");
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
    }

    #[test]
    fn a_missing_asset_is_a_404_problem_not_index_html() {
        let response = respond(&Method::GET, "/assets/gone.js", fake);

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "application/problem+json"
        );
    }

    #[tokio::test]
    async fn head_has_the_headers_and_no_body() {
        let response = respond(&Method::HEAD, "/", fake);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_of(&response, header::CACHE_CONTROL), "no-cache");
        assert_eq!(body(response).await, "");
    }

    #[test]
    fn a_post_to_a_web_path_is_a_405_problem_with_allow() {
        let response = respond(&Method::POST, "/w/abc", fake);

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header_of(&response, header::ALLOW), "GET, HEAD");
        assert_eq!(
            header_of(&response, header::CONTENT_TYPE),
            "application/problem+json"
        );
    }

    #[tokio::test]
    async fn a_path_that_climbs_out_of_the_root_gets_index_html() {
        let response = respond(&Method::GET, "/docs/../secret", |key| {
            assert!(!key.contains(".."), "{key} was looked up");
            fake(key)
        });

        assert!(body(response).await.contains("id=\"root\""));
    }

    #[test]
    fn an_api_path_is_never_answered_with_the_web_app() {
        for path in ["/api", "/api/v1/nope"] {
            let response = respond(&Method::GET, path, fake);

            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                header_of(&response, header::CONTENT_TYPE),
                "application/problem+json"
            );
            assert!(
                response
                    .headers()
                    .get(header::CONTENT_SECURITY_POLICY)
                    .is_none()
            );
        }
    }

    #[test]
    fn the_embedded_root_holds_an_index_page() {
        let index = embedded(INDEX).expect("the placeholder or the built app has index.html");

        assert_eq!(index.mime, "text/html");
        assert!(!index.data.is_empty());
    }

    #[tokio::test]
    async fn the_router_serves_the_web_app_for_non_api_gets_only() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let app = crate::router(AppState::new(pool, Arc::new(String::new)));
        let send = |uri: &'static str| {
            let app = app.clone();
            async move {
                app.oneshot(Request::get(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        };

        let root = send("/").await;
        let deep = send("/workspaces/abc/members").await;
        let api = send("/api/v1/nope").await;
        let health = send("/healthz").await;

        assert_eq!(root.status(), StatusCode::OK);
        assert!(header_of(&root, header::CONTENT_TYPE).starts_with("text/html"));
        assert!(root.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(deep.status(), StatusCode::OK);
        assert!(deep.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_of(&api, header::CONTENT_TYPE),
            "application/problem+json"
        );
        assert!(api.headers().get(header::CONTENT_SECURITY_POLICY).is_none());
        assert_eq!(health.status(), StatusCode::OK);
        assert!(
            health
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_none()
        );
    }
}
