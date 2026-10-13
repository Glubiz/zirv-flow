//! OIDC sign-in: the two legs of the authorization-code flow.
//!
//! `start` sends the browser to the identity provider and remembers the login in progress
//! (server-side, keyed by the hash of `state`, single-use, ten minutes). `callback` checks
//! that the browser that comes back is the one that left (the `zf_oidc_state` cookie), has
//! the identity provider's answer verified, maps it to a user and starts the same session a
//! password sign-in starts. Every failure gives one generic 401; the reason goes only to the
//! audit log, and never carries a token, code, address or claim value. The reasons are
//! `state_mismatch`, `unknown_state`, `provider_error`, `exchange_failed`, `invalid_token`,
//! `no_matching_user` and `link_conflict` (the address belongs to a user already linked to a
//! different subject at this issuer).

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde_json::json;
use zirv_flow_core::Email;
use zirv_flow_core::token::{hash_matches, sha256};
use zirv_flow_identity::{IdentityError, VerifiedIdentity};
use zirv_flow_store::{StoreError, User};

use crate::problem::{ApiError, Problem};
use crate::state::OidcState;
use crate::{AppState, admin, cookies, request_id, session};

/// Where the identity provider sends the browser back to.
const CALLBACK_PATH: &str = "/api/v1/session/oidc/callback";
const FAILED: &str = "sign-in failed";
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");

/// The redirect URI to register at the identity provider for a server at `public_url`.
pub fn redirect_url(public_url: &str) -> String {
    format!("{}{CALLBACK_PATH}", public_url.trim_end_matches('/'))
}

/// The page to land on after signing in, derived from the redirect URI.
pub(crate) fn home_url(redirect_url: &str) -> String {
    match redirect_url.strip_suffix(CALLBACK_PATH) {
        Some(base) => format!("{base}/"),
        None => "/".to_owned(),
    }
}

/// Why a callback did not sign anybody in. The reason is an audit code, not for callers.
enum Failure {
    Reason(&'static str),
    /// A unique constraint was hit while resolving the user; see [`resolve_user`].
    Conflict,
    Error(ApiError),
}

impl From<StoreError> for Failure {
    fn from(error: StoreError) -> Self {
        Self::Error(error.into())
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self::Error(error)
    }
}

/// A write that hit a unique constraint means the identity or the user was claimed by
/// somebody else in the meantime; that is a refusal, not a server fault.
fn conflict_or_error(error: StoreError) -> Failure {
    if error.is_conflict() {
        Failure::Conflict
    } else {
        error.into()
    }
}

/// The query of the callback, all of it optional so a malformed request is a plain failure.
#[derive(Default)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl Callback {
    fn parse(query: &str) -> Self {
        let mut parsed = Self::default();
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let slot = match &*key {
                "code" => &mut parsed.code,
                "state" => &mut parsed.state,
                "error" => &mut parsed.error,
                _ => continue,
            };
            // The first occurrence wins, so a repeated parameter cannot override it.
            slot.get_or_insert_with(|| value.into_owned());
        }
        parsed
    }
}

/// Starts an OIDC sign-in: redirects the browser to the identity provider.
///
/// Sets the `zf_oidc_state` cookie (HttpOnly, SameSite=Lax) that the callback needs.
#[utoipa::path(
    get,
    operation_id = "start_oidc_sign_in",
    path = "/api/v1/session/oidc/start",
    tag = "session",
    responses(
        (status = 303, description = "Redirect to the identity provider; sets the zf_oidc_state cookie"),
        (status = 404, description = "OIDC sign-in is not configured", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The identity provider cannot be reached", body = Problem, content_type = "application/problem+json")
    ),
    security(())
)]
pub(crate) async fn start(State(state): State<AppState>) -> Result<Response, ApiError> {
    let oidc = state.oidc.as_ref().ok_or_else(ApiError::not_found)?;
    let authorization = oidc.provider.begin().await.map_err(|error| {
        tracing::warn!(%error, "OIDC sign-in cannot start");
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "the identity provider is unavailable",
        )
    })?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    zirv_flow_store::delete_expired_oidc_logins(&mut *tx).await?;
    zirv_flow_store::create_oidc_login(
        &mut *tx,
        &sha256(&authorization.state),
        &authorization.nonce,
        &authorization.pkce_verifier,
        cookies::OIDC_LOGIN_SECONDS,
    )
    .await?;
    zirv_flow_store::commit(tx).await?;

    Ok((
        [
            (
                header::SET_COOKIE,
                cookies::oidc_state(&authorization.state, state.cookie_secure),
            ),
            (header::CACHE_CONTROL, NO_STORE),
        ],
        Redirect::to(&authorization.url),
    )
        .into_response())
}

/// Finishes an OIDC sign-in: the identity provider sends the browser here with `code` and
/// `state`.
///
/// On success redirects to the server's public URL with the `zf_session` cookie set. Any
/// failure is the same 401, and clears the `zf_oidc_state` cookie.
#[utoipa::path(
    get,
    operation_id = "finish_oidc_sign_in",
    path = "/api/v1/session/oidc/callback",
    tag = "session",
    params(
        ("code" = Option<String>, Query, description = "The authorization code"),
        ("state" = Option<String>, Query, description = "Must match the zf_oidc_state cookie"),
        ("error" = Option<String>, Query, description = "Set by the identity provider when it refused")
    ),
    responses(
        (status = 303, description = "Signed in; redirect to the server's public URL, sets the zf_session cookie"),
        (status = 401, description = "Sign-in failed", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "OIDC sign-in is not configured", body = Problem, content_type = "application/problem+json")
    ),
    security(())
)]
pub(crate) async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let oidc = state.oidc.as_ref().ok_or_else(ApiError::not_found)?;
    let query = Callback::parse(query.as_deref().unwrap_or_default());
    match complete(&state, oidc, &headers, query).await {
        Ok(response) => Ok(response),
        Err(Failure::Reason(reason)) => {
            record_failure(&state, reason).await?;
            Ok(failed(&state))
        }
        Err(Failure::Conflict) => {
            record_failure(&state, "link_conflict").await?;
            Ok(failed(&state))
        }
        Err(Failure::Error(error)) => Err(error),
    }
}

async fn complete(
    state: &AppState,
    oidc: &OidcState,
    headers: &HeaderMap,
    query: Callback,
) -> Result<Response, Failure> {
    // The browser that comes back must be the one that left: its cookie holds the `state`.
    let presented = query.state.ok_or(Failure::Reason("state_mismatch"))?;
    let cookie = cookies::read(headers, cookies::OIDC_STATE_COOKIE)
        .ok_or(Failure::Reason("state_mismatch"))?;
    let state_hash = sha256(&presented);
    if !hash_matches(cookie, &state_hash) {
        return Err(Failure::Reason("state_mismatch"));
    }
    // Taking the login deletes it, so a replayed callback finds nothing.
    let login = zirv_flow_store::take_oidc_login(&state.pool, &state_hash)
        .await?
        .ok_or(Failure::Reason("unknown_state"))?;
    if query.error.is_some() {
        return Err(Failure::Reason("provider_error"));
    }
    let code = query
        .code
        .filter(|code| !code.is_empty())
        .ok_or(Failure::Reason("provider_error"))?;

    let identity = oidc
        .provider
        .finish(&code, &login.pkce_verifier, &login.nonce)
        .await
        .map_err(|error| {
            let reason = match error {
                IdentityError::Exchange => "exchange_failed",
                IdentityError::InvalidToken => "invalid_token",
                IdentityError::Discovery | IdentityError::Settings(_) => "provider_error",
            };
            tracing::warn!(reason, %error, "OIDC sign-in refused");
            Failure::Reason(reason)
        })?;
    let user = resolve_user(state, oidc, &identity).await?;
    let new = session::start(state, user.id, "oidc").await?;

    let mut response = Redirect::to(&oidc.home_url).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        cookies::session(&new.token, state.cookie_secure),
    );
    headers.append(
        header::SET_COOKIE,
        cookies::clear_oidc_state(state.cookie_secure),
    );
    headers.insert(header::CACHE_CONTROL, NO_STORE);
    Ok(response)
}

/// The user an identity signs in as: the one it is linked to; else the one with the same
/// verified address, which is then linked; else, when allowed, a new one.
///
/// Two first sign-ins for the same subject can race past the pre-checks; the loser hits a
/// unique constraint. It resolves once more in a fresh transaction, where the winner's link
/// is visible. A second conflict means the address belongs to a user linked to a different
/// subject, which is `Failure::Conflict`.
async fn resolve_user(
    state: &AppState,
    oidc: &OidcState,
    identity: &VerifiedIdentity,
) -> Result<User, Failure> {
    match resolve_once(state, oidc, identity).await {
        Err(Failure::Conflict) => resolve_once(state, oidc, identity).await,
        result => result,
    }
}

async fn resolve_once(
    state: &AppState,
    oidc: &OidcState,
    identity: &VerifiedIdentity,
) -> Result<User, Failure> {
    if let Some(user) =
        zirv_flow_store::find_identity_user(&state.pool, &identity.issuer, &identity.subject)
            .await?
    {
        return Ok(user);
    }
    // An unverified address proves nothing about who owns it, so it never matches or creates.
    let email = identity
        .email
        .as_deref()
        .filter(|_| identity.email_verified)
        .and_then(|email| Email::parse(email).ok())
        .ok_or(Failure::Reason("no_matching_user"))?;

    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    let context =
        |user: &User| admin::Context::user(user.id).with_request_id(request_id::current());
    let (user, matched) = match zirv_flow_store::find_user_by_email(&mut *tx, &email).await? {
        Some(user) => (user, "verified_email"),
        None if oidc.auto_create_users => {
            let user = zirv_flow_store::create_user(&mut *tx, &email, None, false)
                .await
                .map_err(conflict_or_error)?;
            admin::audit(
                &mut tx,
                &context(&user),
                None,
                "user.created",
                Some(("user", user.id.as_uuid())),
                json!({ "owner": false, "method": "oidc" }),
            )
            .await?;
            (user, "new_user")
        }
        None => return Err(Failure::Reason("no_matching_user")),
    };
    zirv_flow_store::link_identity(&mut *tx, &identity.issuer, &identity.subject, user.id)
        .await
        .map_err(conflict_or_error)?;
    admin::audit(
        &mut tx,
        &context(&user),
        None,
        "user.identity_linked",
        Some(("user", user.id.as_uuid())),
        json!({ "method": "oidc", "match": matched }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(user)
}

async fn record_failure(state: &AppState, reason: &'static str) -> Result<(), ApiError> {
    let context = admin::Context::anonymous().with_request_id(request_id::current());
    let mut tx = zirv_flow_store::begin(&state.pool).await?;
    admin::audit(
        &mut tx,
        &context,
        None,
        "session.sign_in_failed",
        None,
        json!({ "method": "oidc", "reason": reason }),
    )
    .await?;
    zirv_flow_store::commit(tx).await?;
    Ok(())
}

/// The one answer to every failed callback.
fn failed(state: &AppState) -> Response {
    let mut response = ApiError::new(StatusCode::UNAUTHORIZED, FAILED).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        cookies::clear_oidc_state(state.cookie_secure),
    );
    headers.insert(header::CACHE_CONTROL, NO_STORE);
    response
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use zirv_flow_identity::mock::{CLIENT_ID, MockProvider, Token};
    use zirv_flow_identity::{OidcSettings, SecretString};

    use super::*;
    use crate::router;

    const PUBLIC_URL: &str = "http://flow.test";
    const CODE: &str = "an-authorization-code-0451";
    const PASSWORD: &str = "correct horse battery";

    #[test]
    fn the_redirect_url_is_the_public_url_plus_the_callback_path() {
        assert_eq!(
            redirect_url("https://flow.example.com"),
            "https://flow.example.com/api/v1/session/oidc/callback"
        );
        assert_eq!(
            redirect_url("https://flow.example.com/"),
            "https://flow.example.com/api/v1/session/oidc/callback"
        );
    }

    #[test]
    fn the_home_url_is_the_public_url_with_a_trailing_slash() {
        assert_eq!(
            home_url(&redirect_url("https://flow.example.com/")),
            "https://flow.example.com/"
        );
        assert_eq!(home_url("https://elsewhere/cb"), "/");
    }

    #[test]
    fn the_callback_query_is_parsed_and_the_first_occurrence_wins() {
        let query = Callback::parse("code=a%20b&state=s&state=other&error=access_denied&x=1");

        assert_eq!(query.code.as_deref(), Some("a b"));
        assert_eq!(query.state.as_deref(), Some("s"));
        assert_eq!(query.error.as_deref(), Some("access_denied"));
        let empty = Callback::parse("");
        assert!(empty.code.is_none() && empty.state.is_none() && empty.error.is_none());
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    struct Setup {
        router: Router,
        mock: MockProvider,
    }

    async fn setup(pool: &PgPool, auto_create_users: bool) -> Setup {
        zirv_flow_store::migrate(pool).await.unwrap();
        let mock = MockProvider::start().await;
        let settings = OidcSettings {
            issuer_url: mock.issuer().to_owned(),
            client_id: CLIENT_ID.into(),
            client_secret: SecretString::new("secret"),
            scopes: vec!["openid".into(), "email".into()],
            redirect_url: redirect_url(PUBLIC_URL),
        };
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_oidc(settings, auto_create_users)
            .unwrap();
        Setup {
            router: router(state),
            mock,
        }
    }

    async fn get(router: &Router, uri: &str, cookie: Option<&str>) -> Reply {
        let mut request = Request::get(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        send(router, request.body(Body::empty()).unwrap()).await
    }

    async fn send(router: &Router, request: Request<Body>) -> Reply {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    fn set_cookies(reply: &Reply) -> Vec<String> {
        reply
            .headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap().to_owned())
            .collect()
    }

    /// The `name=value` pair of the cookie a reply sets.
    fn cookie_pair(reply: &Reply, name: &str) -> String {
        set_cookies(reply)
            .iter()
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} cookie"))
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    /// A login in progress: what the browser holds and what the provider will echo.
    struct Started {
        state: String,
        nonce: String,
        cookie: String,
    }

    async fn started(setup: &Setup) -> Started {
        let reply = get(&setup.router, "/api/v1/session/oidc/start", None).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let location = reply.headers[header::LOCATION].to_str().unwrap().to_owned();
        let url = url::Url::parse(&location).unwrap();
        let query: hashbrown::HashMap<_, _> = url.query_pairs().into_owned().collect();
        Started {
            state: query["state"].clone(),
            nonce: query["nonce"].clone(),
            cookie: cookie_pair(&reply, "zf_oidc_state"),
        }
    }

    fn identity(nonce: &str, subject: &str, email: Option<&str>, verified: bool) -> Token {
        let mut token = Token::valid(nonce);
        token.subject = subject.into();
        token.email = email.map(str::to_owned);
        token.email_verified = Some(verified);
        token
    }

    async fn callback_with(setup: &Setup, started: &Started, token: Token) -> Reply {
        setup.mock.set_token(token);
        get(
            &setup.router,
            &format!(
                "/api/v1/session/oidc/callback?code={CODE}&state={}",
                started.state
            ),
            Some(&started.cookie),
        )
        .await
    }

    async fn sign_in_as(
        setup: &Setup,
        subject: &str,
        email: Option<&str>,
        verified: bool,
    ) -> Reply {
        let started = started(setup).await;
        let token = identity(&started.nonce, subject, email, verified);
        callback_with(setup, &started, token).await
    }

    async fn failure_reasons(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT summary->>'reason' FROM audit_log WHERE action = 'session.sign_in_failed' \
             ORDER BY id",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn existing_user(pool: &PgPool, email: &str) -> zirv_flow_store::User {
        admin::create_user(pool, &admin::Context::system(), email, PASSWORD)
            .await
            .unwrap()
    }

    async fn password_sign_in(router: &Router, email: &str, password: &str) -> Reply {
        send(
            router,
            Request::post("/api/v1/session")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
    }

    fn assert_generic_failure(reply: &Reply) {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["detail"], FAILED);
        assert_eq!(
            reply.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert!(
            set_cookies(reply)
                .iter()
                .any(|cookie| cookie.starts_with("zf_oidc_state=;") && cookie.contains("Max-Age=0"))
        );
        assert!(
            !set_cookies(reply)
                .iter()
                .any(|c| c.starts_with("zf_session="))
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn start_redirects_to_the_provider_and_remembers_the_login(pool: PgPool) {
        let setup = setup(&pool, false).await;

        let reply = get(&setup.router, "/api/v1/session/oidc/start", None).await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        let location = reply.headers[header::LOCATION].to_str().unwrap();
        assert!(location.starts_with(&format!("{}/auth?", setup.mock.issuer())));
        assert!(location.contains("code_challenge_method=S256"));
        let cookie = set_cookies(&reply).remove(0);
        for attribute in [
            "HttpOnly",
            "SameSite=Lax",
            "Path=/api/v1/session/oidc",
            "Max-Age=600",
            "Secure",
        ] {
            assert!(cookie.contains(attribute), "{cookie}");
        }
        let started = started(&setup).await;
        assert!(cookie.starts_with("zf_oidc_state="));
        assert_eq!(started.cookie, format!("zf_oidc_state={}", started.state));
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_logins")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn start_is_unavailable_while_the_provider_is_down(pool: PgPool) {
        let state = AppState::new(pool.clone(), Arc::new(String::new))
            .with_oidc(
                OidcSettings {
                    issuer_url: "http://127.0.0.1:1".into(),
                    client_id: CLIENT_ID.into(),
                    client_secret: SecretString::new("secret"),
                    scopes: vec![],
                    redirect_url: redirect_url(PUBLIC_URL),
                },
                false,
            )
            .unwrap();

        let reply = get(&router(state), "/api/v1/session/oidc/start", None).await;

        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            reply.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn both_endpoints_are_404_problems_when_oidc_is_off(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));

        for uri in [
            "/api/v1/session/oidc/start",
            "/api/v1/session/oidc/callback?code=x&state=y",
        ] {
            let reply = get(&router, uri, None).await;

            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(
                reply.headers[header::CONTENT_TYPE],
                "application/problem+json"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_verified_email_links_the_existing_user_and_signs_in(pool: PgPool) {
        let setup = setup(&pool, false).await;
        let jane = existing_user(&pool, "jane@example.com").await;

        let reply = sign_in_as(&setup, "subject-1", Some("Jane@Example.com"), true).await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert_eq!(reply.headers[header::LOCATION], "http://flow.test/");
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        let cookies = set_cookies(&reply);
        assert_eq!(cookies.len(), 2);
        assert!(cookies[0].starts_with("zf_session="));
        assert!(cookies[0].contains("HttpOnly") && cookies[0].contains("SameSite=Lax"));
        assert!(cookies[1].starts_with("zf_oidc_state=;"));
        let session = cookie_pair(&reply, "zf_session");
        let current = get(&setup.router, "/api/v1/session", Some(&session)).await;
        assert_eq!(current.status, StatusCode::OK);
        assert_eq!(current.body["user"]["email"], "jane@example.com");
        assert!(current.body["csrf_token"].as_str().unwrap().len() == 64);
        let linked = zirv_flow_store::find_identity_user(&pool, setup.mock.issuer(), "subject-1")
            .await
            .unwrap();
        assert_eq!(linked.unwrap().id, jane.id);
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action || ' ' || (summary->>'method') FROM audit_log \
             WHERE action IN ('user.identity_linked', 'session.signed_in') ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            actions,
            ["user.identity_linked oidc", "session.signed_in oidc"]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unverified_email_never_matches_a_user(pool: PgPool) {
        let setup = setup(&pool, true).await;
        existing_user(&pool, "jane@example.com").await;

        let unverified = sign_in_as(&setup, "subject-1", Some("jane@example.com"), false).await;
        let no_email = sign_in_as(&setup, "subject-2", None, true).await;

        assert_generic_failure(&unverified);
        assert_generic_failure(&no_email);
        assert_eq!(
            failure_reasons(&pool).await,
            ["no_matching_user", "no_matching_user"]
        );
        let identities: i64 = sqlx::query_scalar("SELECT count(*) FROM user_identities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(identities, 0);
        assert_eq!(zirv_flow_store::list_users(&pool).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn auto_create_makes_a_passwordless_user_without_memberships(pool: PgPool) {
        let setup = setup(&pool, true).await;

        let reply = sign_in_as(&setup, "subject-9", Some("new@example.com"), true).await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let users = zirv_flow_store::list_users(&pool).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].email, "new@example.com");
        assert!(users[0].password_hash.is_none());
        assert!(!users[0].is_owner);
        let memberships: i64 = sqlx::query_scalar("SELECT count(*) FROM memberships")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(memberships, 0);
        let linked = zirv_flow_store::find_identity_user(&pool, setup.mock.issuer(), "subject-9")
            .await
            .unwrap();
        assert_eq!(linked.unwrap().id, users[0].id);
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'user.%' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actions, ["user.created", "user.identity_linked"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_auto_create_an_unknown_user_is_refused(pool: PgPool) {
        let setup = setup(&pool, false).await;

        let reply = sign_in_as(&setup, "subject-9", Some("new@example.com"), true).await;

        assert_generic_failure(&reply);
        assert!(zirv_flow_store::list_users(&pool).await.unwrap().is_empty());
        assert_eq!(failure_reasons(&pool).await, ["no_matching_user"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_sign_in_follows_the_linked_identity_even_when_the_email_changed(
        pool: PgPool,
    ) {
        let setup = setup(&pool, false).await;
        let jane = existing_user(&pool, "jane@example.com").await;
        sign_in_as(&setup, "subject-1", Some("jane@example.com"), true).await;

        let reply = sign_in_as(&setup, "subject-1", Some("renamed@example.com"), false).await;

        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let session = cookie_pair(&reply, "zf_session");
        let current = get(&setup.router, "/api/v1/session", Some(&session)).await;
        assert_eq!(current.body["user"]["id"], jane.id.as_uuid().to_string());
        assert_eq!(current.body["user"]["email"], "jane@example.com");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_provider_account_cannot_take_over_a_linked_user(pool: PgPool) {
        let setup = setup(&pool, false).await;
        existing_user(&pool, "jane@example.com").await;
        sign_in_as(&setup, "subject-1", Some("jane@example.com"), true).await;

        let reply = sign_in_as(&setup, "subject-2", Some("jane@example.com"), true).await;

        assert_generic_failure(&reply);
        assert_eq!(failure_reasons(&pool).await, ["link_conflict"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_state_cookie_that_does_not_match_is_refused(pool: PgPool) {
        let setup = setup(&pool, true).await;
        let started = started(&setup).await;
        setup
            .mock
            .set_token(identity(&started.nonce, "s", Some("a@example.com"), true));
        let uri = format!(
            "/api/v1/session/oidc/callback?code={CODE}&state={}",
            started.state
        );

        let wrong = get(&setup.router, &uri, Some("zf_oidc_state=another-state")).await;
        let missing = get(&setup.router, &uri, None).await;
        let no_state = get(
            &setup.router,
            &format!("/api/v1/session/oidc/callback?code={CODE}"),
            Some(&started.cookie),
        )
        .await;

        for reply in [&wrong, &missing, &no_state] {
            assert_generic_failure(reply);
        }
        assert_eq!(
            failure_reasons(&pool).await,
            ["state_mismatch", "state_mismatch", "state_mismatch"]
        );
        assert!(zirv_flow_store::list_users(&pool).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_replayed_callback_is_refused(pool: PgPool) {
        let setup = setup(&pool, true).await;
        let started = started(&setup).await;
        let token = identity(&started.nonce, "subject-1", Some("a@example.com"), true);
        let first = callback_with(&setup, &started, token.clone()).await;

        let replay = callback_with(&setup, &started, token).await;

        assert_eq!(first.status, StatusCode::SEE_OTHER);
        assert_generic_failure(&replay);
        assert_eq!(failure_reasons(&pool).await, ["unknown_state"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_login_is_refused(pool: PgPool) {
        let setup = setup(&pool, true).await;
        let started = started(&setup).await;
        sqlx::query("UPDATE oidc_logins SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let token = identity(&started.nonce, "subject-1", Some("a@example.com"), true);

        let reply = callback_with(&setup, &started, token).await;

        assert_generic_failure(&reply);
        assert_eq!(failure_reasons(&pool).await, ["unknown_state"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn provider_errors_and_bad_tokens_are_refused_with_their_own_audit_reason(pool: PgPool) {
        let setup = setup(&pool, true).await;
        let denied = started(&setup).await;
        let denied_reply = get(
            &setup.router,
            &format!(
                "/api/v1/session/oidc/callback?error=access_denied&state={}",
                denied.state
            ),
            Some(&denied.cookie),
        )
        .await;
        let no_code = started(&setup).await;
        let no_code_reply = get(
            &setup.router,
            &format!("/api/v1/session/oidc/callback?state={}", no_code.state),
            Some(&no_code.cookie),
        )
        .await;
        let wrong_nonce = started(&setup).await;
        let wrong_nonce_reply = callback_with(
            &setup,
            &wrong_nonce,
            identity("another-nonce", "s", Some("a@example.com"), true),
        )
        .await;
        let refused = started(&setup).await;
        setup.mock.refuse_codes();
        let refused_reply = callback_with(
            &setup,
            &refused,
            identity(&refused.nonce, "s", Some("a@example.com"), true),
        )
        .await;

        for reply in [
            &denied_reply,
            &no_code_reply,
            &wrong_nonce_reply,
            &refused_reply,
        ] {
            assert_generic_failure(reply);
        }
        assert_eq!(
            failure_reasons(&pool).await,
            [
                "provider_error",
                "provider_error",
                "invalid_token",
                "exchange_failed"
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn audit_rows_never_hold_an_address_a_code_or_a_token(pool: PgPool) {
        let setup = setup(&pool, true).await;
        let ok = sign_in_as(
            &setup,
            "subject-77",
            Some("secret.person@example.com"),
            true,
        )
        .await;
        sign_in_as(
            &setup,
            "subject-78",
            Some("other.person@example.com"),
            false,
        )
        .await;
        let started = started(&setup).await;
        callback_with(
            &setup,
            &started,
            identity(
                "wrong",
                "subject-79",
                Some("third.person@example.com"),
                true,
            ),
        )
        .await;

        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT concat_ws(' ', action, object_kind, object_id::text, request_id, summary::text) \
             FROM audit_log",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        let session = cookie_pair(&ok, "zf_session");
        let token = session.strip_prefix("zf_session=").unwrap();
        let all = rows.join("\n");
        for secret in [
            "person@example.com",
            "subject-77",
            CODE,
            token,
            &started.state,
            &started.nonce,
        ] {
            assert!(!all.contains(secret), "{secret} found in the audit log");
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn password_sign_in_is_404_when_local_sign_in_is_disabled(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        existing_user(&pool, "jane@example.com").await;
        let router =
            router(AppState::new(pool.clone(), Arc::new(String::new)).with_local_sign_in(false));

        let reply = password_sign_in(&router, "jane@example.com", PASSWORD).await;

        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(
            reply.headers[header::CONTENT_TYPE],
            "application/problem+json"
        );
        assert!(set_cookies(&reply).is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_passwordless_user_gets_the_same_401_as_a_wrong_password(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        let passwordless = zirv_flow_store::create_user(
            &pool,
            &Email::parse("sso@example.com").unwrap(),
            None,
            false,
        )
        .await
        .unwrap();
        existing_user(&pool, "jane@example.com").await;
        let attempt = |email: &'static str| password_sign_in(&router, email, "not the password");

        let sso = attempt("sso@example.com").await;
        let wrong = attempt("jane@example.com").await;
        let unknown = attempt("nobody@example.com").await;

        assert_eq!(sso.status, StatusCode::UNAUTHORIZED);
        assert_eq!(sso.body["detail"], "invalid email or password");
        assert_eq!(sso.body["detail"], wrong.body["detail"]);
        assert_eq!(sso.body["detail"], unknown.body["detail"]);
        assert_eq!(sso.body["title"], wrong.body["title"]);
        assert!(set_cookies(&sso).is_empty());
        let counted = zirv_flow_store::find_user(&pool, passwordless.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counted.failed_sign_ins, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_password_sign_in_is_audited_with_its_method(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let router = router(AppState::new(pool.clone(), Arc::new(String::new)));
        existing_user(&pool, "jane@example.com").await;

        let reply = password_sign_in(&router, "jane@example.com", PASSWORD).await;

        assert_eq!(reply.status, StatusCode::OK);
        let methods: Vec<String> = sqlx::query_scalar(
            "SELECT summary->>'method' FROM audit_log WHERE action = 'session.signed_in'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(methods, ["password"]);
    }
}
