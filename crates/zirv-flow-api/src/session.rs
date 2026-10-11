//! Sign-in, the current session, and sign-out.

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
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
    if !state.local_sign_in {
        return Err(ApiError::not_found());
    }
    let email = Email::parse(&body.email)?;
    let invalid = || ApiError::new(StatusCode::UNAUTHORIZED, INVALID_CREDENTIALS);
    let anonymous = admin::Context::anonymous().with_request_id(request_id::current());

    let Some(user) = zirv_flow_store::find_user_by_email(&state.pool, &email).await? else {
        passwords::dummy_verify().await;
        record_failure(&state, &anonymous, None, "invalid_credentials").await?;
        return Err(invalid());
    };

    // The database clock stamped `locked_until`, so it decides whether the lock still holds.
    if zirv_flow_store::is_locked(&state.pool, user.id).await? {
        // No verification, but the same time spent, so a lock is not detectable by timing.
        passwords::dummy_verify().await;
        record_failure(&state, &anonymous, Some((user.id, false)), "locked").await?;
        return Err(invalid());
    }

    // An over-long password cannot be valid, and a user who signs in only through an
    // identity provider has none to match; both spend the same time as any other attempt.
    let verified = match &user.password_hash {
        Some(hash) if body.password.chars().count() <= MAX_PASSWORD_CHARS => {
            passwords::verify(body.password, hash.clone()).await
        }
        _ => {
            passwords::dummy_verify().await;
            false
        }
    };
    if !verified {
        record_failure(
            &state,
            &anonymous,
            Some((user.id, true)),
            "invalid_credentials",
        )
        .await?;
        return Err(invalid());
    }
    let new = start(&state, user.id, "password").await?;
    Ok(started(&state, &user, new))
}

/// The response that hands a freshly started session to the browser: the cookie, and the
/// user with the CSRF token in the body. Sign-in and first-run setup answer alike.
pub(crate) fn started(state: &AppState, user: &User, new: NewSession) -> Response {
    (
        [
            (
                header::SET_COOKIE,
                cookies::session(&new.token, state.cookie_secure),
            ),
            (
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            ),
        ],
        Json(SessionBody {
            user: UserBody::from(user),
            csrf_token: new.csrf,
        }),
    )
        .into_response()
}

/// A freshly started session: the cookie value and the CSRF token that goes with it.
pub(crate) struct NewSession {
    pub(crate) token: String,
    pub(crate) csrf: String,
}

/// Starts a session for a user who has just proved who they are, whichever way (`method` is
/// `"password"` or `"oidc"`): resets their failure counter, sweeps expired sessions, stores
/// the new one and audits the sign-in. Every method makes identical sessions.
pub(crate) async fn start(
    state: &AppState,
    user: UserId,
    method: &'static str,
) -> Result<NewSession, ApiError> {
    let context = admin::Context::user(user).with_request_id(request_id::current());
    let token = random_token(32);
    let csrf = credentials::csrf_token(&token);
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::record_successful_sign_in(&mut *tx, user).await?;
    zirv_flow_store::delete_expired_sessions(&mut *tx, SESSION_IDLE_SECONDS).await?;
    zirv_flow_store::create_session(
        &mut *tx,
        &sha256(&token),
        user,
        &sha256(&csrf),
        SESSION_ABSOLUTE_SECONDS,
    )
    .await?;
    admin::audit(
        &mut tx,
        &context,
        None,
        "session.signed_in",
        Some(("user", user.as_uuid())),
        json!({ "method": method }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(NewSession { token, csrf })
}

/// Audits a failed sign-in, and counts it against the account when `account` is
/// `(id, true)`, atomically. The actor is the anonymous `context`: nobody has proved who
/// they are, so a known account is recorded only as the object.
async fn record_failure(
    state: &AppState,
    context: &admin::Context,
    account: Option<(UserId, bool)>,
    reason: &'static str,
) -> Result<(), ApiError> {
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    if let Some((user, true)) = account {
        zirv_flow_store::record_failed_sign_in(&mut *tx, user).await?;
    }
    admin::audit(
        &mut tx,
        context,
        None,
        "session.sign_in_failed",
        account.map(|(user, _)| ("user", user.as_uuid())),
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
pub(crate) async fn current(principal: Principal) -> Result<impl IntoResponse, ApiError> {
    let session = principal.require_user()?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(SessionBody {
            user: UserBody::from(&session.user),
            csrf_token: session.csrf_token.clone(),
        }),
    ))
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
) -> Result<impl IntoResponse, ApiError> {
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
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    #[test]
    fn the_user_body_has_no_password_hash() {
        let user = User {
            id: UserId::new(),
            email: "a@example.com".into(),
            password_hash: Some("$argon2id$secret".into()),
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

    #[tokio::test]
    async fn a_started_session_sets_the_cookie_and_shows_the_user_and_csrf_token() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/zirv_flow")
            .unwrap();
        let state = AppState::new(pool, std::sync::Arc::new(String::new));
        let user = User {
            id: UserId::new(),
            email: "a@example.com".into(),
            password_hash: None,
            is_owner: true,
            failed_sign_ins: 0,
            locked_until: None,
            created_at: OffsetDateTime::now_utc(),
        };
        let new = NewSession {
            token: "cookie-value".into(),
            csrf: "csrf-value".into(),
        };

        let response = started(&state, &user, new);

        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with("zf_session=cookie-value"), "{cookie}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["csrf_token"], "csrf-value");
        assert_eq!(json["user"]["email"], "a@example.com");
    }

    #[test]
    fn a_sign_in_request_without_a_password_does_not_deserialise() {
        assert!(serde_json::from_str::<SignInRequest>(r#"{"email":"a@b.c"}"#).is_err());
        assert!(
            serde_json::from_str::<SignInRequest>(r#"{"email":"a@b.c","password":"x"}"#).is_ok()
        );
    }
}
