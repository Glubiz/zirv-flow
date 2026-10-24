//! Event forwarding (architecture 12.5): rules that pick accepted events for a destination and
//! map them to its payload, the dead-letter list, the queue status and re-drive. Every endpoint is
//! for workspace admins, and every change is audited with ids and names, never a mapped value.
//!
//! This is separate from the instance owner's `jobs_admin`: a workspace admin sees only their
//! own workspace's rows, and only fixed codes, ids and times of them. A payload value reaches
//! no response, audit entry or log line.

pub(crate) mod queue;
pub(crate) mod rules;

#[cfg(test)]
mod acceptance;

use axum::http::StatusCode;
use zirv_flow_core::forwarding::RuleIssue;

use crate::problem::ApiError;
use crate::sources::problems;

/// A 422 for the issues of a mapping or a filter: one problem per issue, naming where and why.
pub(crate) fn issues_problem(field: &str, issues: Vec<RuleIssue>) -> ApiError {
    problems(
        issues
            .into_iter()
            .map(|issue| (format!("{field}{}", issue.pointer), issue.code.to_owned()))
            .collect(),
    )
}

/// The 422 for a rule on a destination that cannot carry events.
pub(crate) fn destination_unsupported() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "destination_id is a destination type that cannot forward events",
    )
    .with_code("destination_unsupported")
}

/// The 422 for the twenty-first active rule.
pub(crate) fn too_many_rules() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "a workspace can have at most 20 active forwarding rules",
    )
    .with_code("too_many_rules")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_refusals_are_422s_with_their_own_code() {
        assert_eq!(destination_unsupported().status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(destination_unsupported().code(), Some("destination_unsupported"));
        assert_eq!(too_many_rules().status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(too_many_rules().code(), Some("too_many_rules"));
    }

    #[test]
    fn issues_become_one_problem_each_without_a_value() {
        let error = issues_problem(
            "mapping",
            vec![RuleIssue {
                pointer: "/id".into(),
                code: "invalid_source",
            }],
        );

        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.detail().contains("mapping/id invalid_source"));
    }
}
