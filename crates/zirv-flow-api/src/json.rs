//! A JSON body extractor whose rejections are problem documents.
//!
//! The rejection text of the stock extractor can quote the offending input, which for
//! these endpoints may be a password, so the detail here is fixed text.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, OptionalFromRequest, Request};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;

use crate::problem::ApiError;

pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match <axum::Json<T> as FromRequest<S>>::from_request(request, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(reject(&rejection)),
        }
    }
}

/// A body that may be left out: a request without a content type is `None`, one with a body is
/// read like [`ApiJson`].
impl<T, S> OptionalFromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        match <axum::Json<T> as OptionalFromRequest<S>>::from_request(request, state).await {
            Ok(value) => Ok(value.map(|axum::Json(value)| Self(value))),
            Err(rejection) => Err(reject(&rejection)),
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
    async fn valid_json_is_extracted() {
        let name = extract(Some("application/json"), r#"{"name":"x"}"#).await;

        assert_eq!(name.unwrap(), "x");
    }

    #[tokio::test]
    async fn an_optional_body_may_be_left_out_but_not_be_malformed() {
        let read = |content_type: Option<&'static str>, body: &'static str| async move {
            let mut request = Request::post("/");
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            let request = request.body(Body::from(body)).unwrap();
            <ApiJson<Named> as OptionalFromRequest<()>>::from_request(request, &()).await
        };

        assert!(read(None, "").await.unwrap().is_none());
        let some = read(Some("application/json"), r#"{"name":"x"}"#).await;
        assert_eq!(
            some.unwrap().map(|ApiJson(body)| body.name).as_deref(),
            Some("x")
        );
        let bad = read(Some("application/json"), "{not json")
            .await
            .err()
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
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
