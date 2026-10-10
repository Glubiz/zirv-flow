//! Sign-in, the current session, and sign-out.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;
use zirv_flow_core::token::{random_token, sha256};
use zirv_flow_core::{Email, UserId};
use zirv_flow_store::User;

use crate::auth::{Principal, SESSION_ABSOLUTE_SECONDS, SESSION_IDLE_SECONDS};
use crate::json::ApiJson;
use crate::problem::{ApiError, Problem};
use crate::{AppState, admin, cookies, credentials, passwords, request_id};

/// Longest password that is verified at sign-in; anything longer cannot be a valid
/// password, and hashing it would only burn CPU.
const MAX_PASSWORD_CHARS: usize = 1024;
const INVALID_CREDENTIALS: &str = "invalid email or password";

#[derive(Deserialize, ToSchema)]
pub(crate) struct SignInRequest {
    #[schema(example = "owner@example.com")]
    email: String,
    password: String,
}

/// A user as shown to API callers; never includes the password hash.
#[derive(Serialize, ToSchema)]
pub(crate) struct UserBody {
    pub(crate) id: Uuid,
    pub(crate) email: String,
    pub(crate) is_owner: bool,
}

impl From<&User> for UserBody {
    fn from(user: &User) -> Self {
        Self {
            id: user.id.as_uuid(),
            email: user.email.clone(),
            is_owner: user.is_owner,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionBody {
    user: UserBody,
    /// Send this as `x-csrf-token` on every POST, PUT, PATCH and DELETE made with the cookie.
    csrf_token: String,
}

/// Signs in with an e-mail and password and starts a session.
///
/// Sets the `zf_session` cookie (HttpOnly, SameSite=Lax). An unknown e-mail, a wrong
/// password and a locked account all give the same 401. Five failures in a row lock the
/// account for a growing time, up to 15 minutes.
#[utoipa::path(
    post,
    path = "/api/v1/session",
    tag = "session",
    request_body = SignInRequest,
    responses(
        (status = 200, description = "Signed in", body = SessionBody),
        (status = 401, description = "Invalid email or password", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "Malformed email", body = Problem, content_type = "application/problem+json")
    ),
    security(())
)]
pub(crate) async fn sign_in(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<SignInRequest>,
) -> Result<Response, ApiError> {
    let email = Email::parse(&body.email)?;
    let invalid = || ApiError::new(StatusCode::UNAUTHORIZED, INVALID_CREDENTIALS);
    let anonymous = admin::Context::anonymous().with_request_id(request_id::current());

    let Some(user) = zirv_flow_store::find_user_by_email(&state.pool, &email).await? else {
        passwords::dummy_verify().await;
        record_failure(&state, &anonymous, None, "invalid_credentials").await?;
        return Err(invalid());
    };
    let context = admin::Context::user(user.id).with_request_id(request_id::current());

    let locked = user
        .locked_until
        .is_some_and(|until| until > OffsetDateTime::now_utc());
    if locked {
        // No verification, but the same time spent, so a lock is not detectable by timing.
        passwords::dummy_verify().await;
        record_failure(&state, &context, None, "locked").await?;
        return Err(invalid());
    }

    let verified = body.password.chars().count() <= MAX_PASSWORD_CHARS
        && passwords::verify(body.password, user.password_hash.clone()).await;
    if !verified {
        record_failure(&state, &context, Some(user.id), "invalid_credentials").await?;
        return Err(invalid());
    }

    let token = random_token(32);
    let csrf = credentials::csrf_token(&token);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::record_successful_sign_in(&mut *tx, user.id).await?;
    zirv_flow_store::delete_expired_sessions(&mut *tx, SESSION_IDLE_SECONDS).await?;
    zirv_flow_store::create_session(
        &mut *tx,
        &sha256(&token),
        user.id,
        &sha256(&csrf),
        SESSION_ABSOLUTE_SECONDS,
    )
    .await?;
    admin::audit(
        &mut tx,
        &context,
        None,
        "session.signed_in",
        Some(("user", user.id.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    Ok((
        [
            (
                header::SET_COOKIE,
                cookies::session(&token, state.cookie_secure),
            ),
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            ),
        ],
        Json(SessionBody {
            user: UserBody::from(&user),
            csrf_token: csrf,
        }),
    )
        .into_response())
}

/// Counts a failed sign-in (when the account exists) and audits it, atomically.
async fn record_failure(
    state: &AppState,
    context: &admin::Context,
    count_against: Option<UserId>,
    reason: &'static str,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    if let Some(user) = count_against {
        zirv_flow_store::record_failed_sign_in(&mut *tx, user).await?;
    }
    admin::audit(
        &mut tx,
        context,
        None,
        "session.sign_in_failed",
        None,
        json!({ "reason": reason }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The signed-in user and their CSRF token.
#[utoipa::path(
    get,
    path = "/api/v1/session",
    tag = "session",
    responses(
        (status = 200, description = "The current session", body = SessionBody),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "An API key cannot use this endpoint", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn current(principal: Principal) -> Result<Response, ApiError> {
    let session = principal.require_user()?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(SessionBody {
            user: UserBody::from(&session.user),
            csrf_token: session.csrf_token.clone(),
        }),
    )
        .into_response())
}

/// Signs out: ends the session and clears the cookie. Needs the CSRF token.
#[utoipa::path(
    delete,
    path = "/api/v1/session",
    tag = "session",
    responses(
        (status = 204, description = "Signed out"),
        (status = 401, description = "Not signed in", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Missing or invalid CSRF token", body = Problem, content_type = "application/problem+json")
    ),
    security(("session" = []))
)]
pub(crate) async fn sign_out(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Response, ApiError> {
    let session = principal.require_user()?;
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::delete_session(&mut *tx, &session.token_hash).await?;
    admin::audit(
        &mut tx,
        &principal.context(),
        None,
        "session.signed_out",
        Some(("user", session.user.id.as_uuid())),
        json!({}),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, cookies::clear(state.cookie_secure))],
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_body_has_no_password_hash() {
        let user = User {
            id: UserId::new(),
            email: "a@example.com".into(),
            password_hash: "$argon2id$secret".into(),
            is_owner: true,
            failed_sign_ins: 0,
            locked_until: None,
            created_at: OffsetDateTime::now_utc(),
        };

        let json = serde_json::to_string(&UserBody::from(&user)).unwrap();

        assert!(json.contains("a@example.com"));
        assert!(!json.contains("argon2"));
        assert!(!json.contains("password"));
    }

    #[test]
    fn a_sign_in_request_without_a_password_does_not_deserialise() {
        assert!(serde_json::from_str::<SignInRequest>(r#"{"email":"a@b.c"}"#).is_err());
        assert!(
            serde_json::from_str::<SignInRequest>(r#"{"email":"a@b.c","password":"x"}"#).is_ok()
        );
    }
}
