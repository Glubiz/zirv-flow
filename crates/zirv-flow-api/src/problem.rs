use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::request_id;

/// An RFC 9457 problem document.
#[derive(Debug, Serialize, ToSchema)]
pub struct Problem {
    /// Problem type URI; `about:blank` until specific types are defined.
    #[serde(rename = "type")]
    #[schema(rename = "type", example = "about:blank")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    /// The `x-request-id` of the failed request.
    pub request_id: Option<String>,
}

/// An error that renders as `application/problem+json`.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    detail: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    #[cfg(test)]
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    /// Missing, wrong, expired or revoked credentials. One body for every cause, so a
    /// response never says which part of a credential was wrong.
    pub(crate) fn unauthenticated() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication required")
    }

    pub(crate) fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, detail)
    }

    /// Also the answer for a resource the caller may not know exists.
    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "no such resource")
    }

    /// A validation failure. The detail names the field and the rule, never the value.
    pub(crate) fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
    }

    /// An unexpected failure; the cause is deliberately not shown to the caller.
    pub(crate) fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl From<zirv_flow_core::ValidationError> for ApiError {
    fn from(error: zirv_flow_core::ValidationError) -> Self {
        Self::unprocessable(error.to_string())
    }
}

impl From<zirv_flow_store::StoreError> for ApiError {
    fn from(_: zirv_flow_store::StoreError) -> Self {
        Self::internal()
    }
}

impl From<crate::admin::AdminError> for ApiError {
    fn from(error: crate::admin::AdminError) -> Self {
        use crate::admin::AdminError;

        match error {
            AdminError::Invalid(error) => error.into(),
            AdminError::Conflict(detail) => Self::new(StatusCode::CONFLICT, detail),
            AdminError::NotFound(_) => Self::not_found(),
            AdminError::Store(error) => error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let problem = Problem {
            kind: "about:blank".into(),
            title: self.status.canonical_reason().unwrap_or("Error").into(),
            status: self.status.as_u16(),
            detail: self.detail,
            request_id: request_id::current(),
        };
        let body = serde_json::to_vec(&problem).unwrap_or_default();
        (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response()
    }
}

pub(crate) async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "no such resource")
}

pub(crate) async fn method_not_allowed() -> ApiError {
    ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_errors_carry_the_expected_status() {
        assert_eq!(ApiError::unauthenticated().status, StatusCode::UNAUTHORIZED);
        assert_eq!(ApiError::forbidden("no").status, StatusCode::FORBIDDEN);
        assert_eq!(ApiError::not_found().status, StatusCode::NOT_FOUND);
        assert_eq!(
            ApiError::unprocessable("x").status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::internal().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn an_unauthenticated_error_does_not_say_why() {
        assert_eq!(
            ApiError::unauthenticated().detail,
            "authentication required"
        );
    }

    #[test]
    fn a_validation_error_becomes_a_422_naming_the_field() {
        let error = ApiError::from(zirv_flow_core::password::validate("short").unwrap_err());

        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.detail.starts_with("password:"));
        assert!(!error.detail.contains("short"));
    }

    #[test]
    fn admin_errors_map_to_their_statuses() {
        use crate::admin::AdminError;

        let status = |error| ApiError::from(error).status;
        assert_eq!(status(AdminError::Conflict("taken")), StatusCode::CONFLICT);
        assert_eq!(status(AdminError::NotFound("user")), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_api_error_renders_as_a_problem_document() {
        let response = ApiError::new(StatusCode::NOT_FOUND, "no such resource").into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/problem+json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            problem,
            serde_json::json!({
                "type": "about:blank",
                "title": "Not Found",
                "status": 404,
                "detail": "no such resource",
                "request_id": null
            })
        );
    }
}
