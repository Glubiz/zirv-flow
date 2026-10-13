//! Sources and their streams (architecture sections 5.3 and 7.2): the configured external
//! systems of a workspace, their credentials, and the streams each offers. Members read;
//! admins create, change, test and discover. A credential is stored sealed and never appears in
//! a response, an audit entry or a log line.

pub(crate) mod actions;
pub(crate) mod credential;
pub(crate) mod crud;
pub(crate) mod kinds;
pub(crate) mod manifest;
pub(crate) mod oauth;
pub(crate) mod preview;
pub(crate) mod runs;
pub(crate) mod streams;
pub(crate) mod tokens;

pub(crate) use actions::read_stream_records;
pub(crate) use manifest::{SampleBody, ValidateRequest, keeps_connection, sample_manifest};
pub(crate) use preview::{PreviewBody, check_sample, preview_mapping};
pub(crate) use streams::{MappingIssue, load_mapping_check, saved_mapping_body};

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod http_acceptance;

use axum::http::StatusCode;
use zirv_flow_connectors::sources::{FieldError, Registry};

use crate::problem::{ApiError, ItemProblem};

/// A 422 that names one invalid field and the rule it broke, never the value.
pub(crate) fn invalid(field: &str, reason: &str) -> ApiError {
    problems(vec![(field.to_owned(), reason.to_owned())])
}

/// A 422 that lists every invalid field.
pub(crate) fn problems(fields: Vec<(String, String)>) -> ApiError {
    let detail = fields
        .iter()
        .map(|(field, reason)| format!("{field} {reason}"))
        .collect::<Vec<_>>()
        .join("; ");
    ApiError::unprocessable(detail).with_errors(
        fields
            .into_iter()
            .map(|(field, reason)| ItemProblem {
                index: 0,
                field,
                reason,
            })
            .collect(),
    )
}

/// The field errors of a source's settings, with the field names under `prefix`.
pub(crate) fn field_problems(prefix: &str, errors: Vec<FieldError>) -> ApiError {
    problems(
        errors
            .into_iter()
            .map(|error| (format!("{prefix}.{}", error.field), error.reason))
            .collect(),
    )
}

/// The source types this build ships.
pub(crate) fn registry() -> Registry {
    Registry::built_in()
}

/// Whether records of this source type arrive by push (a webhook) instead of being read.
pub(crate) fn is_push(kind: &str) -> bool {
    registry()
        .get(kind)
        .is_some_and(|driver| driver.spec().capabilities.push)
}

/// A push source is created, changed and removed only through the webhooks API, which keeps its
/// secret, its hook row and its stream together.
pub(crate) fn managed_by_webhooks() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "a webhook source is created, changed and deleted through the webhooks API",
    )
    .with_code("managed_by_webhooks")
}

/// A push source has no schedule and no run: its records arrive as they are sent.
pub(crate) fn push_source() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "a webhook source receives records as they are sent; it has no run",
    )
    .with_code("push_source")
}

/// The answer for a stored source whose type this build does not ship.
pub(crate) fn kind_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "this source type is not available in this build",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_problem_lists_each_field_and_the_detail_joins_them() {
        let error = problems(vec![
            ("name".into(), "is required".into()),
            ("config.url".into(), "must be an http or https URL".into()),
        ]);

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            error.detail(),
            "name is required; config.url must be an http or https URL"
        );
    }

    #[test]
    fn field_errors_are_prefixed_with_their_group() {
        let error = field_problems(
            "credential",
            vec![FieldError {
                field: "token".into(),
                reason: "is required".into(),
            }],
        );

        assert_eq!(error.detail(), "credential.token is required");
    }

    #[test]
    fn the_registry_lists_the_fake_with_test_support() {
        assert!(registry().get("fake").is_some());
        assert!(registry().get("nonesuch").is_none());
    }

    #[test]
    fn an_unavailable_kind_is_a_conflict() {
        assert_eq!(kind_unavailable().status(), StatusCode::CONFLICT);
    }
}
