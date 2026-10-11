//! Static lists (architecture sections 6 and 7.3): the lists of a workspace, their members, and
//! the lists and membership history of a person.

pub(crate) mod crud;
pub(crate) mod import;
pub(crate) mod members;
pub(crate) mod person;

use zirv_flow_core::ValidationError;
use zirv_flow_store::{ListSource, MembershipError};

use crate::admin::AdminError;
use crate::auth::Principal;

/// The most entries of a report list (`not_found`, `unmatched`) a response carries; the
/// matching `*_count` field says how many there were.
pub(crate) const MAX_REPORTED: usize = 1000;

/// The first [`MAX_REPORTED`] of `items`, and how many there were in all.
fn capped<T>(mut items: Vec<T>) -> (Vec<T>, usize) {
    let total = items.len();
    items.truncate(MAX_REPORTED);
    (items, total)
}

/// Where a change came from: the web app signs in with a session, everything else with a key.
fn source_of(principal: &Principal) -> ListSource {
    match principal {
        Principal::Session(_) => ListSource::Ui,
        Principal::Key(_) => ListSource::Api,
    }
}

fn invalid(field: &'static str, message: &'static str) -> AdminError {
    AdminError::Invalid(ValidationError::new(field, message))
}

fn membership_error(error: MembershipError) -> AdminError {
    match error {
        MembershipError::NoSuchList => AdminError::NotFound("list"),
        MembershipError::TooMany => invalid("people", "a call takes at most 1000 people"),
        MembershipError::Store(error) if error.is_retryable() => {
            AdminError::Conflict("the list or the people changed at the same moment, try again")
        }
        MembershipError::Store(error) => AdminError::Store(error),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::problem::ApiError;

    #[test]
    fn a_report_list_is_cut_at_the_cap_and_keeps_its_total() {
        let (short, total) = capped((0..MAX_REPORTED + 5).collect::<Vec<_>>());

        assert_eq!((short.len(), total), (MAX_REPORTED, MAX_REPORTED + 5));
        assert_eq!(short[0], 0);
        let (all, total) = capped(vec![1, 2]);
        assert_eq!((all, total), (vec![1, 2], 2));
    }

    #[test]
    fn a_missing_list_is_a_404_and_too_many_people_a_422() {
        let status = |error| ApiError::from(membership_error(error)).status();

        assert_eq!(status(MembershipError::NoSuchList), StatusCode::NOT_FOUND);
        assert_eq!(
            status(MembershipError::TooMany),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
}
