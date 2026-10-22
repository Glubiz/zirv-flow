//! A destination's credential as a sealed secret of purpose `destination_credential`: a JSON
//! object of field name to value, opened right before a call and handed to the connector as a
//! typed, zeroizing [`Credential`] (architecture 16.2). Sealing is the sources' (the shared
//! `to_plaintext` and `keyring`); only the purpose differs.

use uuid::Uuid;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::{SecretId, SecretPurpose, WorkspaceId};
use zirv_flow_store::secrets;

use crate::AppState;
use crate::problem::ApiError;
use crate::sources::credential::keyring;

/// Opens the credential secret of a destination. A destination with none gets an empty
/// credential.
pub(super) async fn load(
    state: &AppState,
    workspace: WorkspaceId,
    secret: Option<Uuid>,
) -> Result<Credential, ApiError> {
    let Some(secret) = secret else {
        return Ok(Credential::new());
    };
    let bytes = secrets::open_json(
        &state.pool,
        keyring(state)?,
        workspace,
        SecretId::from_uuid(secret),
        SecretPurpose::DestinationCredential,
    )
    .await
    .map_err(|error| {
        tracing::warn!(?error, "a destination credential could not be opened");
        ApiError::internal()
    })?;
    Credential::from_json(&bytes).map_err(|_| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;

    #[tokio::test]
    async fn a_destination_without_a_secret_has_an_empty_credential() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/none").unwrap();
        let state = AppState::new(pool, std::sync::Arc::new(String::new));

        let credential = load(&state, WorkspaceId::new(), None).await.unwrap();

        assert!(credential.is_empty());
    }

    #[tokio::test]
    async fn a_secret_without_a_master_key_is_unavailable() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/none").unwrap();
        let state = AppState::new(pool, std::sync::Arc::new(String::new));

        let error = load(&state, WorkspaceId::new(), Some(Uuid::nil()))
            .await
            .err()
            .unwrap();

        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
