//! A source's credential as a sealed secret: a JSON object of field name to value. It is
//! decrypted right before a call and handed to the connector as a typed, zeroizing
//! [`Credential`] (architecture 16.2).

use std::collections::BTreeMap;

use axum::http::StatusCode;
use uuid::Uuid;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::{
    Keyring, SecretBytes, SecretId, SecretPurpose, SecretString, open, secret_aad,
};
use zirv_flow_store::secrets;

use crate::AppState;
use crate::problem::ApiError;

/// The plaintext of a credential secret.
pub(super) fn to_plaintext(
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

fn from_plaintext(bytes: &SecretBytes) -> Result<Credential, ApiError> {
    let values: BTreeMap<String, SecretString> =
        serde_json::from_slice(bytes.expose()).map_err(|_| ApiError::internal())?;
    let mut credential = Credential::new();
    for (key, value) in values {
        credential.insert(key, value);
    }
    Ok(credential)
}

/// The keys that open stored secrets, or 503 when the server was started without them.
pub(super) fn keyring(state: &AppState) -> Result<&std::sync::Arc<Keyring>, ApiError> {
    state.keyring().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "stored credentials are unavailable until the master key is configured",
        )
    })
}

/// Opens the credential secret of a source. No source has one when `secret` is `None`, and the
/// connector then gets an empty credential.
pub(super) async fn load(
    state: &AppState,
    workspace: WorkspaceId,
    secret: Option<Uuid>,
) -> Result<Credential, ApiError> {
    let Some(secret) = secret else {
        return Ok(Credential::new());
    };
    let keyring = keyring(state)?;
    let id = SecretId::from_uuid(secret);
    let stored = secrets::get(&state.pool, workspace, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    let aad = secret_aad(workspace, id, SecretPurpose::ConnectorCredential);
    let bytes = open(keyring, &stored.sealed, &aad).map_err(|error| {
        tracing::warn!(%error, "a source credential could not be opened");
        ApiError::internal()
    })?;
    from_plaintext(&bytes)
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
