//! Reading a response body under a size cap.

use super::error::HttpError;

/// The default cap on a response body: 10 MiB.
pub const DEFAULT_MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// Reads the whole body, failing with [`HttpError::BodyTooLarge`] as soon as it exceeds
/// `max_bytes`. A declared `Content-Length` over the cap fails before any byte is read; a body
/// without one is cut off while streaming.
pub async fn read_body(
    mut response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, HttpError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(HttpError::BodyTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(HttpError::from_reqwest)? {
        if (body.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
            return Err(HttpError::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::routing::get;
    use futures_util::stream;

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn router() -> Router {
        Router::new()
            .route("/fixed", get(|| async { "x".repeat(100) }))
            .route(
                "/stream",
                get(|| async {
                    let chunks =
                        (0..10).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 10])));
                    Body::from_stream(stream::iter(chunks))
                }),
            )
    }

    // A plain client: these tests exercise `read_body` alone, against a local server.
    async fn fetch(url: String) -> reqwest::Response {
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::new().get(url).send().await.unwrap()
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_returned_whole() {
        let base = serve(router()).await;
        let body = read_body(fetch(format!("{base}/fixed")).await, 100).await;
        assert_eq!(body.unwrap().len(), 100);
        let body = read_body(fetch(format!("{base}/stream")).await, 100).await;
        assert_eq!(body.unwrap().len(), 100);
    }

    #[tokio::test]
    async fn a_declared_length_over_the_cap_fails_before_reading() {
        let base = serve(router()).await;
        let response = fetch(format!("{base}/fixed")).await;
        assert_eq!(response.content_length(), Some(100));
        assert_eq!(read_body(response, 99).await, Err(HttpError::BodyTooLarge));
    }

    #[tokio::test]
    async fn a_streamed_body_without_a_length_stops_at_the_cap() {
        let base = serve(router()).await;
        let response = fetch(format!("{base}/stream")).await;
        assert_eq!(response.content_length(), None);
        assert_eq!(read_body(response, 99).await, Err(HttpError::BodyTooLarge));
    }

    #[test]
    fn the_default_cap_is_ten_mebibytes() {
        assert_eq!(DEFAULT_MAX_BODY_BYTES, 10_485_760);
    }
}
