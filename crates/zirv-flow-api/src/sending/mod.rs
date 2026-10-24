//! Sending domains, sender identities and send settings (architecture 10.1, 10.2 and 16.2): the
//! domains a workspace sends from with their DKIM, SPF and DMARC records and the status of each,
//! the sender identities (from name and address) bound to a domain, and the frequency caps and
//! quiet hours of the send pipeline. Domains and identities are for workspace admins, and a caller
//! with no access gets 404; the send settings may be read by anyone who can read the workspace.
//!
//! Zirv Flow generates each domain's DKIM key. The private half is sealed into `encrypted_secrets`
//! (purpose `dkim_key`) in the transaction that creates the domain and is never returned, audited
//! or logged; the response carries the public half only, inside the record text to publish.
//! DNS is read by the `sending_domain.verify` job, which the API queues when a domain is created
//! and when an admin asks for a re-check.

pub(crate) mod domains;
pub(crate) mod identities;
pub(crate) mod settings;

#[cfg(test)]
mod acceptance;

use zirv_flow_core::ValidationError;
use zirv_flow_store::SenderError;

use crate::problem::ApiError;
use crate::sources::invalid;

/// The problem for a sender that may not send: `sender_unverified` (422) in every case, with a
/// detail that says which.
pub(crate) fn sender_error(error: SenderError) -> ApiError {
    match error {
        SenderError::Unknown => {
            ApiError::sender_unverified("the sender is not a sender identity of this workspace")
        }
        SenderError::Archived => ApiError::sender_unverified("the sender identity is archived"),
        SenderError::Unverified { domain } => ApiError::sender_unverified(format!(
            "the sending domain {domain} is not verified: its DKIM, SPF and DMARC records are not all in place"
        )),
        SenderError::Store(error) => error.into(),
    }
}

/// A 422 for a core validation failure, naming its field.
pub(super) fn invalid_field(error: ValidationError) -> ApiError {
    invalid(error.field(), error.message())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[test]
    fn every_refused_sender_is_a_422_sender_unverified_with_its_own_detail() {
        let unknown = sender_error(SenderError::Unknown);
        let archived = sender_error(SenderError::Archived);
        let unverified = sender_error(SenderError::Unverified {
            domain: "example.com".into(),
        });

        for error in [&unknown, &archived, &unverified] {
            assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(error.code(), Some("sender_unverified"));
        }
        assert!(format!("{unverified:?}").contains("example.com"));
    }
}
