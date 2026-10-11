use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{request_id, trace};

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
    /// The `x-trace-id` of the failed request; look it up with
    /// `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.
    pub trace_id: Option<String>,
    /// The invalid fields of an ingestion request: field names and reason codes, never values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ItemProblem>>,
    /// A stable machine-readable code for a failure a client acts on, such as
    /// `search_timeout`; absent for the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// One invalid field of one item of an ingestion request.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ItemProblem {
    /// Position of the item in the request; 0 for a single-item endpoint.
    pub index: u32,
    /// The field, or empty when the item itself is the wrong type.
    pub field: String,
    /// A stable code such as `unknown_field`, `required` or `invalid_email`.
    pub reason: String,
}

/// An error that renders as `application/problem+json`.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    detail: String,
    errors: Option<Vec<ItemProblem>>,
    retry_after: Option<u64>,
    code: Option<&'static str>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
            errors: None,
            retry_after: None,
            code: None,
        }
    }

    /// Lists the invalid fields in the body.
    pub(crate) fn with_errors(mut self, errors: Vec<ItemProblem>) -> Self {
        self.errors = Some(errors);
        self
    }

    /// Adds `Retry-After`, in whole seconds.
    pub(crate) fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn status_code(&self) -> StatusCode {
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

    pub(crate) fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, detail)
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

    /// A caller without access to values asked for an order or prefix search on a PII trait.
    pub(crate) fn pii_trait_not_searchable() -> Self {
        Self {
            code: Some("pii_trait_not_searchable"),
            ..Self::new(
                StatusCode::FORBIDDEN,
                "trait.op: only eq and exists can search a PII trait without access to values",
            )
        }
    }

    /// A bounded read that ran past its time limit and was cancelled.
    pub(crate) fn search_timeout() -> Self {
        Self {
            code: Some("search_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the query took too long; narrow it and try again",
            )
        }
    }

    /// An explorer query that ran past its time limit and was cancelled.
    pub(crate) fn query_timeout() -> Self {
        Self {
            code: Some("query_timeout"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "the query took too long; narrow the time range or the filters and try again",
            )
        }
    }

    /// A caller without access to values asked to probe or break down a PII property.
    pub(crate) fn pii_property_not_explorable() -> Self {
        Self {
            code: Some("pii_property_not_explorable"),
            ..Self::new(
                StatusCode::FORBIDDEN,
                "only eq and exists apply to a PII or undeclared property without access to                  values, and it cannot be broken down",
            )
        }
    }
}

impl From<zirv_flow_store::ExploreError> for ApiError {
    fn from(error: zirv_flow_store::ExploreError) -> Self {
        use zirv_flow_store::ExploreError;

        match error {
            ExploreError::Invalid { field, reason } => {
                Self::unprocessable(format!("{field}: {reason}"))
            }
            ExploreError::PiiPropertyNotExplorable => Self::pii_property_not_explorable(),
            ExploreError::Store(error) if error.is_timeout() => Self::query_timeout(),
            ExploreError::Store(_) => Self::internal(),
        }
    }
}

impl From<zirv_flow_core::ValidationError> for ApiError {
    fn from(error: zirv_flow_core::ValidationError) -> Self {
        Self::unprocessable(error.to_string())
    }
}

impl From<zirv_flow_store::StoreError> for ApiError {
    fn from(error: zirv_flow_store::StoreError) -> Self {
        if error.is_timeout() {
            Self::search_timeout()
        } else {
            Self::internal()
        }
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
            trace_id: trace::current().map(|trace| trace.trace_id.to_string()),
            errors: self.errors,
            code: self.code.map(str::to_owned),
        };
        let body = serde_json::to_vec(&problem).unwrap_or_default();
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, "application/problem+json")],
            body,
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, seconds.into());
        }
        response
    }
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
    async fn a_problem_inside_a_traced_request_carries_the_trace_id() {
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("req"));
        let expected = trace.trace_id.to_string();

        let response =
            trace::with_trace(trace, async { ApiError::not_found().into_response() }).await;

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["trace_id"], expected.as_str());
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
                "request_id": null,
                "trace_id": null
            })
        );
    }

    #[tokio::test]
    async fn errors_and_retry_after_are_rendered_and_absent_otherwise() {
        let response = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "slow down")
            .with_retry_after(3)
            .with_errors(vec![ItemProblem {
                index: 2,
                field: "email".into(),
                reason: "invalid_email".into(),
            }])
            .into_response();

        assert_eq!(response.headers()[header::RETRY_AFTER], "3");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            problem["errors"],
            serde_json::json!([{"index": 2, "field": "email", "reason": "invalid_email"}])
        );
        let plain = ApiError::not_found().into_response();
        assert!(plain.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn a_timeout_is_a_503_problem_with_the_search_timeout_code() {
        let response = ApiError::search_timeout().into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "search_timeout");
        assert_eq!(problem["status"], 503);
    }

    #[tokio::test]
    async fn an_explorer_timeout_is_a_503_with_the_query_timeout_code() {
        let response = ApiError::query_timeout().into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "query_timeout");
    }

    #[test]
    fn explorer_refusals_map_to_422_and_403() {
        use zirv_flow_store::ExploreError;

        let invalid = ApiError::from(ExploreError::Invalid {
            field: "to",
            reason: "must be after from",
        });
        let pii = ApiError::from(ExploreError::PiiPropertyNotExplorable);

        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(invalid.detail(), "to: must be after from");
        assert_eq!(pii.status(), StatusCode::FORBIDDEN);
    }
}
