//! Workspace credentials (architecture 10.1 and 16.2): named, typed credentials that sources and
//! destinations share. One is entered once, sealed, rotated in one place, tested, and shows where
//! it is used. A secret value is sealed on arrival and never appears in a response, an audit
//! entry, a problem or a log line: a response carries the non-secret `fields`, a `hint` per
//! secret field (its last four characters when it has at least 16, else nothing) and the
//! redaction fingerprint. Every endpoint is for workspace admins, and no access answers 404.
//!
//! Decryption stays here and in `jobs`: a source or destination opens the current secret right
//! before use ([`resolve::load`]), so a rotation changes nothing else.

pub(crate) mod check;
pub(crate) mod crud;
pub(crate) mod resolve;
pub(crate) mod schema;

#[cfg(test)]
mod acceptance;

use axum::http::StatusCode;

use crate::problem::ApiError;

/// An archived credential is read-only history: it cannot be changed, rotated, tested or
/// referenced.
pub(crate) fn archived() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "this credential is archived")
        .with_code("credential_archived")
}

/// A credential that live sources or destinations use cannot be archived.
pub(crate) fn in_use() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "this credential is used by a source or destination; stop using it first",
    )
    .with_code("credential_in_use")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archived_and_in_use_are_conflicts_with_their_own_codes() {
        assert_eq!(archived().status(), StatusCode::CONFLICT);
        assert_eq!(archived().code(), Some("credential_archived"));
        assert_eq!(in_use().status(), StatusCode::CONFLICT);
        assert_eq!(in_use().code(), Some("credential_in_use"));
    }
}
