//! The consent endpoints (architecture sections 6 and 10.1): a person's consent and its
//! history, the topic catalog, the consent policy, and identifier suppressions.

pub(crate) mod csv;
pub(crate) mod dimensions;
pub(crate) mod person;
pub(crate) mod policy;
pub(crate) mod suppressions;
pub(crate) mod topics;
pub(crate) mod transfer;

use uuid::Uuid;
use zirv_flow_core::ValidationError;
use zirv_flow_core::consent::ChangeSource;

use crate::admin::AdminError;
use crate::auth::Principal;
use crate::problem::ApiError;

fn invalid(field: &'static str, message: &'static str) -> AdminError {
    AdminError::Invalid(ValidationError::new(field, message))
}

/// Where a write by `principal` is recorded as coming from, and the user behind it: `ui` and
/// the signed-in user for a session, `api` and nobody for a key.
fn origin(principal: &Principal) -> (ChangeSource, Option<Uuid>) {
    match principal {
        Principal::Session(session) => (ChangeSource::Ui, Some(session.user.id.as_uuid())),
        Principal::Key(_) => (ChangeSource::Api, None),
    }
}

/// The boolean query parameter `name`: false when absent, a bad request unless `true` or
/// `false`.
pub(crate) fn bool_param(query: &str, name: &str) -> Result<bool, ApiError> {
    let mut flag = false;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == name {
            flag = match value.as_ref() {
                "true" => true,
                "false" => false,
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "{name} must be true or false"
                    )));
                }
            };
        }
    }
    Ok(flag)
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_is_lowercase_and_two_digits_per_byte() {
        assert_eq!(hex(&[0, 15, 16, 255]), "000f10ff");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn an_invalid_field_is_a_validation_error_naming_it() {
        assert!(matches!(
            invalid("key", "bad"),
            AdminError::Invalid(error) if error.field() == "key"
        ));
    }
}
