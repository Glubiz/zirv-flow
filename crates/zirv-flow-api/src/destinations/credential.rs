//! A destination's workspace credential, opened right before a call and handed to the connector as
//! a typed, zeroizing [`Credential`] (architecture 16.2).

use uuid::Uuid;
use zirv_flow_connectors::sources::Credential;
use zirv_flow_core::WorkspaceId;

use crate::AppState;
use crate::problem::ApiError;

/// Opens the workspace credential of a destination. A destination with none gets an empty
/// credential.
pub(super) async fn load(
    state: &AppState,
    workspace: WorkspaceId,
    credential: Option<Uuid>,
) -> Result<Credential, ApiError> {
    crate::workspace_credentials::resolve::load(state, workspace, credential).await
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
