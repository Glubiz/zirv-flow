//! First-run setup: claiming a fresh instance from the browser.
//!
//! While no owner exists, the `serve` role mints a one-time setup token, keeps only its
//! SHA-256 and logs the token once, so nobody who merely reaches the instance can claim it.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use zirv_flow_core::token::{hash_matches, random_token, sha256};
use zirv_flow_store::StoreError;

use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::{AppState, admin, request_id, session};

/// The same answer for a missing, wrong and never-minted token.
const REFUSED: &str = "invalid setup token";

impl AppState {
    /// Mints the setup token when no owner exists, replacing any earlier one, and returns it
    /// for the caller to log. `None` when an owner exists.
    pub async fn mint_setup_token(&self) -> Result<Option<String>, StoreError> {
        if zirv_flow_store::count_owners(&self.pool).await? > 0 {
            return Ok(None);
        }
        let token = random_token(32);
        *self.setup_token() = Some(sha256(&token));
        Ok(Some(token))
    }

    pub(crate) fn setup_token(&self) -> std::sync::MutexGuard<'_, Option<[u8; 32]>> {
        self.setup_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn setup_token_matches(&self, presented: &str) -> bool {
        self.setup_token()
            .as_ref()
            .is_some_and(|stored| hash_matches(presented, stored))
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SetupStatus {
    /// True only while no owner exists.
    required: bool,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SetupRequest {
    /// The setup token the server logged at startup. Absent counts as wrong.
    #[serde(default)]
    token: String,
    #[schema(example = "owner@example.com")]
    email: String,
    password: String,
}

/// Whether the instance still needs its first-run setup.
#[utoipa::path(
    get,
    operation_id = "get_setup_status",
    path = "/api/v1/setup",
    tag = "setup",
    responses(
        (status = 200, description = "Whether setup is required", body = SetupStatus)
    ),
    security(())
)]
pub(crate) async fn status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let required = zirv_flow_store::count_owners(&state.pool).await? == 0;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(SetupStatus { required }),
    )
        .into_response())
}

/// Creates the instance owner and signs them in.
///
/// Needs the setup token that `serve` logs while no owner exists. Answers like sign-in:
/// it sets the `zf_session` cookie and returns the user with the CSRF token. An existing
/// owner is 409, checked before the token; a missing or wrong token is 403.
#[utoipa::path(
    post,
    operation_id = "complete_setup",
    path = "/api/v1/setup",
    tag = "setup",
    request_body = SetupRequest,
    responses(
        (status = 200, description = "The owner was created and signed in", body = crate::session::SessionBody),
        (status = 403, description = "Missing or wrong setup token", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "An owner already exists", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Malformed email or weak password", body = Problem, content_type = "application/problem+json")
    ),
    security(())
)]
pub(crate) async fn complete(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SetupRequest>,
) -> Result<Response, ApiError> {
    if zirv_flow_store::count_owners(&state.pool).await? > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "an owner already exists",
        ));
    }
    if !state.setup_token_matches(&body.token) {
        return Err(ApiError::forbidden(REFUSED));
    }
    let anonymous = admin::Context::anonymous().with_request_id(request_id::current());
    let user =
        admin::create_first_owner(&state.pool, &anonymous, &body.email, &body.password).await?;
    *state.setup_token() = None;
    let new = session::start(&state, user.id, "setup").await?;
    Ok(session::started(&state, &user, new))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn state() -> AppState {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        AppState::new(pool, Arc::new(String::new))
    }

    #[tokio::test]
    async fn no_token_matches_until_one_is_minted_and_only_the_minted_one_does() {
        let state = state();
        assert!(!state.setup_token_matches(""));
        assert!(!state.setup_token_matches("anything"));

        *state.setup_token() = Some(sha256("the-token"));

        assert!(state.setup_token_matches("the-token"));
        assert!(!state.setup_token_matches("the-token "));
        assert!(!state.setup_token_matches(""));
    }

    #[tokio::test]
    async fn the_state_keeps_the_hash_of_the_token_and_not_the_token() {
        let state = state();

        *state.setup_token() = Some(sha256("the-token"));

        let stored = state.setup_token().unwrap();
        assert_eq!(stored, sha256("the-token"));
        assert!(!stored.windows(9).any(|window| window == b"the-token"));
    }

    #[test]
    fn a_setup_request_without_a_token_reads_as_an_empty_one_but_needs_email_and_password() {
        let parse = |json: &str| serde_json::from_str::<SetupRequest>(json).ok();

        assert_eq!(
            parse(r#"{"token":"t","email":"a@b.c","password":"x"}"#)
                .unwrap()
                .token,
            "t"
        );
        assert_eq!(
            parse(r#"{"email":"a@b.c","password":"x"}"#).unwrap().token,
            ""
        );
        assert!(parse(r#"{"token":"t","password":"x"}"#).is_none());
        assert!(parse(r#"{"token":"t","email":"a@b.c"}"#).is_none());
    }
}
