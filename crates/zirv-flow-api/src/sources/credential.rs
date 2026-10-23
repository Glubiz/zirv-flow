//! A source's credential as a sealed secret: a JSON object of field name to value. It is
//! decrypted right before a call and handed to the connector as a typed, zeroizing
//! [`Credential`] (architecture 16.2).

use std::collections::BTreeMap;

use axum::http::StatusCode;
#[cfg(test)]
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::{Keyring, SecretBytes, SecretString};

use crate::AppState;
use crate::problem::ApiError;

/// The plaintext of a credential secret.
pub(crate) fn to_plaintext(
    values: &BTreeMap<String, SecretString>,
) -> Result<SecretBytes, ApiError> {
    let fields: BTreeMap<&str, &str> = values
        .iter()
        .map(|(key, value)| (key.as_str(), value.expose()))
        .collect();
    serde_json::to_vec(&fields)
        .map(SecretBytes::new)
        .map_err(|_| ApiError::internal())
}

#[cfg(test)]
fn from_plaintext(bytes: &SecretBytes) -> Result<Credential, ApiError> {
    Credential::from_json(bytes).map_err(|_| ApiError::internal())
}

/// The keys that open stored secrets, or 503 when the server was started without them.
pub(crate) fn keyring(state: &AppState) -> Result<&std::sync::Arc<Keyring>, ApiError> {
    state.keyring().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "stored credentials are unavailable until the master key is configured",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_survives_the_trip_through_its_plaintext() {
        let mut values = BTreeMap::new();
        values.insert("token".to_owned(), SecretString::new("abc".to_owned()));
        values.insert("user".to_owned(), SecretString::new("u".to_owned()));

        let credential = from_plaintext(&to_plaintext(&values).unwrap()).unwrap();

        assert_eq!(credential.get("token").unwrap().expose(), "abc");
        assert_eq!(credential.get("user").unwrap().expose(), "u");
        assert_eq!(credential.len(), 2);
    }

    #[test]
    fn a_plaintext_that_is_not_a_string_map_is_an_internal_error() {
        let bytes = SecretBytes::new(b"[1]".to_vec());

        let error = from_plaintext(&bytes).unwrap_err();

        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
