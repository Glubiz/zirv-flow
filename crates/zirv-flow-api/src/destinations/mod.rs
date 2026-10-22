//! Destinations (architecture 5.3, 10.1 and 14): the configured external systems a workspace
//! delivers messages to, their sealed credentials, and a check that proves a destination works.
//! Every endpoint is for workspace admins: a destination's settings and credential are not for
//! members to read. A credential is stored sealed and never appears in a response, an audit entry
//! or a log line.

pub(crate) mod actions;
pub(crate) mod credential;
pub(crate) mod crud;

#[cfg(test)]
mod acceptance;

use axum::http::StatusCode;
use zirv_flow_connectors::destinations::Registry;

use crate::problem::ApiError;

/// The destination types this build ships.
pub(crate) fn registry() -> Registry {
    Registry::built_in()
}

/// The answer for a stored destination whose type this build does not ship.
pub(crate) fn kind_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "this destination type is not available in this build",
    )
    .with_code("destination_kind_unavailable")
}

/// An archived destination is read-only history: it cannot be changed or checked.
pub(crate) fn archived() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "this destination is archived")
        .with_code("destination_archived")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_lists_the_webhook_and_with_test_support_the_fake_and_nothing_else() {
        let kinds: Vec<_> = registry().specs().iter().map(|spec| spec.kind).collect();

        assert_eq!(kinds, ["webhook", "fake"]);
        assert!(registry().get("nonesuch").is_none());
    }

    #[test]
    fn an_unavailable_and_an_archived_destination_are_conflicts_with_their_own_code() {
        assert_eq!(kind_unavailable().status(), StatusCode::CONFLICT);
        assert_eq!(archived().status(), StatusCode::CONFLICT);
        assert_eq!(archived().code(), Some("destination_archived"));
        assert_eq!(
            kind_unavailable().code(),
            Some("destination_kind_unavailable")
        );
    }
}
