//! A JSON body extractor whose rejections are problem documents.
//!
//! The rejection text of the stock extractor can quote the offending input, which for
//! these endpoints may be a password, so the detail here is fixed text.

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, OptionalFromRequest, Request};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::problem::ApiError;

pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match <axum::Json<Value> as FromRequest<S>>::from_request(request, state).await {
            Ok(axum::Json(value)) => {
                if zirv_flow_core::no_nul::find_nul(&value).is_some() {
                    return Err(ApiError::contains_nul("request body"));
                }
                serde_json::from_value(value).map(Self).map_err(|_| {
                    ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "request body is missing a field or has a field of the wrong type",
                    )
                })
            }
            Err(rejection) => Err(reject(&rejection)),
        }
    }
}

/// PostgreSQL stores no NUL in `text` or `jsonb`, so a query string that decodes to one is
/// refused here for every route at once, as a body is in [`ApiJson`].
pub(crate) async fn refuse_nul_query(request: Request, next: Next) -> Response {
    if request.uri().query().is_some_and(has_encoded_nul) {
        return ApiError::contains_nul("query").into_response();
    }
    next.run(request).await
}

/// Whether percent-decoding `query` yields a NUL: a `%00`.
fn has_encoded_nul(query: &str) -> bool {
    query.as_bytes().windows(3).any(|window| window == b"%00")
}

/// A body that may be left out: an empty body, whatever its headers say, and a literal `null`
/// are `None`. Any other body must be JSON of `T`, whatever its content type.
impl<T, S> OptionalFromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        let bytes = <Bytes as FromRequest<S>>::from_request(request, state)
            .await
            .map_err(|rejection| {
                let detail = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    "request body is too large"
                } else {
                    "request body could not be read"
                };
                ApiError::new(rejection.status(), detail)
            })?;
        if bytes.is_empty() {
            return Ok(None);
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && zirv_flow_core::no_nul::find_nul(&value).is_some()
        {
            return Err(ApiError::contains_nul("request body"));
        }
        match serde_json::from_slice::<Option<T>>(&bytes) {
            Ok(value) => Ok(value.map(Self)),
            Err(error) if error.is_data() => Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "request body is missing a field or has a field of the wrong type",
            )),
            Err(_) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "request body is not valid JSON",
            )),
        }
    }
}

fn reject(rejection: &JsonRejection) -> ApiError {
    let status = rejection.status();
    let detail = match status {
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            "expected a JSON body (content-type: application/json)"
        }
        StatusCode::PAYLOAD_TOO_LARGE => "request body is too large",
        StatusCode::UNPROCESSABLE_ENTITY => {
            "request body is missing a field or has a field of the wrong type"
        }
        _ => "request body is not valid JSON",
    };
    ApiError::new(status, detail)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::header;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Named {
        name: String,
    }

    async fn extract(content_type: Option<&str>, body: &str) -> Result<String, ApiError> {
        let mut request = Request::post("/");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let request = request.body(Body::from(body.to_owned())).unwrap();
        <ApiJson<Named> as FromRequest<()>>::from_request(request, &())
            .await
            .map(|ApiJson(body)| body.name)
    }

    #[tokio::test]
    async fn a_nul_in_a_value_or_a_key_is_a_422_that_names_no_value() {
        for body in [r#"{"name":"a\u0000b"}"#, r#"{"name":"x","k\u0000":1}"#] {
            let error = extract(Some("application/json"), body).await.unwrap_err();

            assert_eq!(error.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error.code(), Some("contains_nul"));
            assert_eq!(
                error.detail(),
                "request body: must not contain a NUL character"
            );
        }
    }

    #[test]
    fn only_a_percent_encoded_nul_is_a_nul_in_a_query() {
        assert!(has_encoded_nul("q=a%00b"));
        assert!(has_encoded_nul("%00"));
        assert!(!has_encoded_nul("q=a%2500b&r=%0A"));
    }

    #[tokio::test]
    async fn valid_json_is_extracted() {
        let name = extract(Some("application/json"), r#"{"name":"x"}"#).await;

        assert_eq!(name.unwrap(), "x");
    }

    async fn optional(
        content_type: Option<&'static str>,
        body: &'static str,
    ) -> Result<Option<String>, ApiError> {
        let mut request = Request::post("/");
        if let Some(content_type) = content_type {
            request = request.header(header::CONTENT_TYPE, content_type);
        }
        let request = request.body(Body::from(body)).unwrap();
        <ApiJson<Named> as OptionalFromRequest<()>>::from_request(request, &())
            .await
            .map(|read| read.map(|ApiJson(body)| body.name))
    }

    #[tokio::test]
    async fn an_empty_optional_body_is_none_whatever_the_headers_say() {
        assert_eq!(optional(None, "").await.unwrap(), None);
        assert_eq!(optional(Some("application/json"), "").await.unwrap(), None);
        assert_eq!(
            optional(Some("application/json"), "null").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_non_empty_optional_body_must_be_valid_json_with_or_without_a_header() {
        let read = optional(None, r#"{"name":"x"}"#).await.unwrap();
        assert_eq!(read.as_deref(), Some("x"));
        let read = optional(Some("application/json"), r#"{"name":"y"}"#).await;
        assert_eq!(read.unwrap().as_deref(), Some("y"));
        for (content_type, body, status) in [
            (None, "{not json", StatusCode::BAD_REQUEST),
            (
                Some("application/json"),
                "{not json",
                StatusCode::BAD_REQUEST,
            ),
            (None, "{}", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            let error = optional(content_type, body).await.err().unwrap();
            assert_eq!(error.status(), status, "{body}");
        }
    }

    #[tokio::test]
    async fn rejections_use_fixed_text_and_the_right_status() {
        let cases = [
            (
                Some("application/json"),
                "{not json",
                StatusCode::BAD_REQUEST,
            ),
            (
                Some("application/json"),
                r#"{"name": 12345678}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                Some("application/json"),
                r#"{}"#,
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (None, r#"{"name":"x"}"#, StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ];
        for (content_type, body, status) in cases {
            let error = extract(content_type, body).await.err().unwrap();

            assert_eq!(error.status(), status, "{body}");
            assert!(!error.detail().contains("12345678"), "{}", error.detail());
        }
    }
}
