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

    /// The source reported field names no mapping can address (blank or duplicate). The code names
    /// the rule, never a field.
    pub(crate) fn source_fields_invalid(code: &'static str) -> Self {
        Self {
            code: Some(code),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the source reported field names that cannot be mapped",
            )
        }
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

    /// An `Idempotency-Key` that was first used for a different request.
    pub(crate) fn idempotency_key_reused() -> Self {
        Self {
            code: Some("idempotency_key_reused"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the Idempotency-Key was already used for a different request",
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

    /// The stream already has a queued or running sync run.
    pub(crate) fn sync_run_active() -> Self {
        Self {
            code: Some("sync_run_active"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the stream already has a run that is queued or running",
            )
        }
    }

    /// A change of a sync mode, cursor field, primary key or source setting while a sync run is
    /// queued or running: the run would undo it.
    pub(crate) fn sync_run_blocks_change() -> Self {
        Self {
            code: Some("sync_run_active"),
            ..Self::new(
                StatusCode::CONFLICT,
                "a sync run is queued or running; wait for it to finish or cancel it, then change the sync mode, cursor field, primary key or source settings",
            )
        }
    }

    /// A consent was asked for before the operator set `server.public_url`: the provider could not be
    /// told where to send the browser back.
    pub(crate) fn public_url_not_configured() -> Self {
        Self {
            code: Some("public_url_not_configured"),
            ..Self::new(
                StatusCode::CONFLICT,
                "connecting an account needs server.public_url to be set",
            )
        }
    }

    /// A consent was asked for a source whose manifest does not use the authorization-code grant.
    pub(crate) fn oauth_not_configured() -> Self {
        Self {
            code: Some("oauth_not_configured"),
            ..Self::new(
                StatusCode::CONFLICT,
                "this source does not use an OAuth2 authorization-code manifest",
            )
        }
    }

    /// A run was asked for a stream that has no mapping, or for a source with none mapped.
    pub(crate) fn stream_not_mapped(detail: impl Into<String>) -> Self {
        Self {
            code: Some("stream_not_mapped"),
            ..Self::new(StatusCode::UNPROCESSABLE_ENTITY, detail)
        }
    }

    /// A run was asked for a stream that is not enabled.
    pub(crate) fn stream_not_enabled() -> Self {
        Self {
            code: Some("stream_not_enabled"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the stream is not enabled; enable it before running it",
            )
        }
    }

    /// A reconciliation was asked for or scheduled on a stream that is not incremental. A full
    /// refresh already sweeps after every run.
    pub(crate) fn reconcile_needs_incremental() -> Self {
        Self {
            code: Some("reconcile_needs_incremental"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "only an incremental stream can be reconciled; a full refresh removes what the source dropped after every run",
            )
        }
    }

    /// The sweep limit of a stream is not a share above 0 and at most 1.
    pub(crate) fn invalid_sweep_limit() -> Self {
        Self {
            code: Some("invalid_sweep_limit"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "sweep_limit must be above 0 and at most 1",
            )
        }
    }

    /// A CSV import was given a mapping with a delete signal, which only a synced stream reads.
    pub(crate) fn delete_signal_unsupported() -> Self {
        Self {
            code: Some("delete_signal_unsupported"),
            ..Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "mapping: delete_when is for a synced stream; a CSV import has no deletes",
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

impl ApiError {
    /// The assistant is off for the workspace, or its model is no longer offered.
    pub(crate) fn ai_disabled() -> Self {
        Self {
            code: Some("ai_disabled"),
            ..Self::new(
                StatusCode::CONFLICT,
                "the AI assistant is not enabled for this workspace",
            )
        }
    }

    /// The workspace's monthly AI budget is spent; nothing was sent to the provider.
    pub(crate) fn ai_budget_exceeded() -> Self {
        Self {
            code: Some("ai_budget_exceeded"),
            ..Self::new(
                StatusCode::PAYMENT_REQUIRED,
                "the workspace's monthly AI budget is spent",
            )
        }
    }

    /// A user is over the per-minute AI call limit; nothing was sent to the provider.
    pub(crate) fn ai_rate_limited() -> Self {
        Self {
            code: Some("ai_rate_limited"),
            ..Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many AI calls; try again in a minute",
            )
            .with_retry_after(60)
        }
    }

    /// A request carried personal data the workspace has not opted in to sending.
    pub(crate) fn ai_personal_data_not_allowed() -> Self {
        Self {
            code: Some("personal_data_not_allowed"),
            ..Self::unprocessable("personal data is not allowed in this workspace's AI requests")
        }
    }

    /// The provider could not be reached or answered unusably. The detail is fixed text, never
    /// the provider's own message.
    pub(crate) fn ai_provider_failed(detail: impl Into<String>) -> Self {
        Self {
            code: Some("ai_provider_error"),
            ..Self::new(StatusCode::BAD_GATEWAY, detail)
        }
    }

    /// The chosen model lacks required capabilities; each is listed with its reason.
    pub(crate) fn ai_capability_missing(errors: Vec<ItemProblem>) -> Self {
        Self {
            code: Some("ai_capability_missing"),
            ..Self::unprocessable("the model lacks capabilities the assistant requires")
                .with_errors(errors)
        }
    }

    /// Stored secrets need the master key, which this deployment has not been given.
    pub(crate) fn secrets_unavailable() -> Self {
        Self {
            code: Some("secrets_unavailable"),
            ..Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "stored secrets are not configured on this deployment",
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
        } else if error.is_run_active() {
            Self::sync_run_blocks_change()
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
            AdminError::Secret(_) => Self::internal(),
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
    fn the_consent_refusals_are_conflicts_with_a_code() {
        for (error, code) in [
            (
                ApiError::public_url_not_configured(),
                "public_url_not_configured",
            ),
            (ApiError::oauth_not_configured(), "oauth_not_configured"),
        ] {
            assert_eq!(error.status(), StatusCode::CONFLICT);
            assert_eq!(error.code, Some(code));
        }
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
    async fn the_ai_problems_carry_their_status_code_and_retry_hint() {
        let cases = [
            (ApiError::ai_disabled(), StatusCode::CONFLICT, "ai_disabled"),
            (
                ApiError::ai_budget_exceeded(),
                StatusCode::PAYMENT_REQUIRED,
                "ai_budget_exceeded",
            ),
            (
                ApiError::ai_rate_limited(),
                StatusCode::TOO_MANY_REQUESTS,
                "ai_rate_limited",
            ),
            (
                ApiError::ai_personal_data_not_allowed(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "personal_data_not_allowed",
            ),
            (
                ApiError::ai_provider_failed("the request timed out"),
                StatusCode::BAD_GATEWAY,
                "ai_provider_error",
            ),
            (
                ApiError::ai_capability_missing(vec![ItemProblem {
                    index: 0,
                    field: "usage".into(),
                    reason: "no usage".into(),
                }]),
                StatusCode::UNPROCESSABLE_ENTITY,
                "ai_capability_missing",
            ),
            (
                ApiError::secrets_unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "secrets_unavailable",
            ),
        ];
        for (error, status, code) in cases {
            let response = error.into_response();
            assert_eq!(response.status(), status, "{code}");
            let retry = response.headers().get(header::RETRY_AFTER).cloned();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], code);
            assert_eq!(retry.is_some(), code == "ai_rate_limited", "{code}");
        }
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
