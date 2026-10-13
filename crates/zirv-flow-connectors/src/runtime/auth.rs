//! How a request proves who is asking: the auth styles, and the OAuth2 client-credentials token
//! the runtime fetches, caches in the context and refreshes.
//!
//! [`Auth`] borrows from the source's `Credential`, so the plaintext lives only for the call.
//! Nothing here formats a secret into a message: the authorization header is built, marked
//! sensitive and sent, and a failure names a status and nothing else.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Method, Request};
use serde_json::Value;
use url::Url;
use zirv_flow_core::SecretString;

use super::error::RuntimeError;
use crate::sources::SourceContext;

/// A token with less than this left is treated as expired, so it does not lapse in flight.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

/// Where an API key goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiKeyPlacement {
    /// In the named request header.
    Header(&'static str),
    /// In the named query parameter.
    Query(&'static str),
}

/// How the client proves itself to an OAuth2 token endpoint.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientAuth {
    /// `client_id` and `client_secret` in the form body.
    #[default]
    Body,
    /// HTTP basic auth with the client id and secret.
    Basic,
}

/// The auth a request carries. It is attached only to requests for the source's base origin.
pub enum Auth<'a> {
    None,
    ApiKey {
        placement: ApiKeyPlacement,
        value: &'a SecretString,
    },
    Basic {
        username: &'a str,
        password: &'a SecretString,
    },
    Bearer {
        token: &'a SecretString,
    },
    /// The OAuth2 client-credentials grant. The runtime fetches the token from `token_url`, which
    /// may be on another origin than the source, and keeps it in the context's token slot.
    OAuth2ClientCredentials {
        token_url: Url,
        client_id: &'a str,
        client_secret: &'a SecretString,
        scopes: &'a [String],
        client_auth: ClientAuth,
    },
}

/// Names the style only: a URL, a username or a key would not belong in a log line.
impl fmt::Debug for Auth<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "Auth::None",
            Self::ApiKey { .. } => "Auth::ApiKey",
            Self::Basic { .. } => "Auth::Basic",
            Self::Bearer { .. } => "Auth::Bearer",
            Self::OAuth2ClientCredentials { .. } => "Auth::OAuth2ClientCredentials",
        })
    }
}

impl Auth<'_> {
    pub(crate) fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth2ClientCredentials { .. })
    }
}

/// An OAuth2 access token and when it lapses. The caller stores it between runs (through
/// `encrypted_secrets`) and seeds the next context with it. `Debug` shows no token.
#[derive(Clone)]
pub struct AccessToken {
    token: Arc<SecretString>,
    expires_at: Option<DateTime<Utc>>,
}

impl AccessToken {
    /// `expires_at` is `None` for a token the endpoint gave no lifetime for; it is then used
    /// until the upstream answers 401.
    pub fn new(token: SecretString, expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            token: Arc::new(token),
            expires_at,
        }
    }

    pub fn token(&self) -> &SecretString {
        &self.token
    }

    pub fn expires_at(&self) -> Option<DateTime<Utc>> {
        self.expires_at
    }

    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        let skew = TimeDelta::from_std(EXPIRY_SKEW).unwrap_or(TimeDelta::zero());
        self.expires_at.is_some_and(|at| at - skew <= now)
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken([redacted])")
    }
}

/// The token a context holds, and the one it fetched that the caller has not read yet.
#[derive(Debug, Default)]
pub(crate) struct TokenSlot {
    pub(crate) current: Option<AccessToken>,
    pub(crate) refreshed: Option<AccessToken>,
}

/// The token for the next request: the cached one while it is good, else a fresh one. `force`
/// skips the cache (after a 401). The flag says whether the token was fetched now.
pub(crate) async fn ensure_token(
    ctx: &SourceContext,
    auth: &Auth<'_>,
    force: bool,
) -> Result<(AccessToken, bool), RuntimeError> {
    if !force
        && let Some(token) = ctx.cached_token()
        && !token.is_expired(Utc::now())
    {
        return Ok((token, false));
    }
    let token = fetch_token(ctx, auth).await?;
    ctx.store_token(token.clone());
    Ok((token, true))
}

/// Asks the token endpoint for a token. One attempt: a failure is the caller's to retry.
async fn fetch_token(ctx: &SourceContext, auth: &Auth<'_>) -> Result<AccessToken, RuntimeError> {
    let Auth::OAuth2ClientCredentials {
        token_url,
        client_id,
        client_secret,
        scopes,
        client_auth,
    } = auth
    else {
        return Err(RuntimeError::InvalidRequest);
    };
    let body = {
        let mut form = url::form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "client_credentials");
        if !scopes.is_empty() {
            form.append_pair("scope", &scopes.join(" "));
        }
        if *client_auth == ClientAuth::Body {
            form.append_pair("client_id", client_id);
            form.append_pair("client_secret", client_secret.expose());
        }
        form.finish()
    };
    let mut request = Request::new(Method::POST, token_url.clone());
    let headers = request.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if *client_auth == ClientAuth::Basic {
        headers.insert(AUTHORIZATION, basic_value(client_id, client_secret)?);
    }
    *request.body_mut() = Some(Bytes::from(body).into());

    let response = ctx.http().execute(request).await?;
    let status = response.status();
    if !status.is_success() {
        return Err(RuntimeError::TokenEndpoint {
            status: Some(status.as_u16()),
        });
    }
    let body = response.body().await?;
    // The body is parsed for the token and dropped: it never reaches an error.
    let parsed: Value =
        serde_json::from_slice(&body).map_err(|_| RuntimeError::TokenEndpoint { status: None })?;
    let token = parsed
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(RuntimeError::TokenEndpoint { status: None })?;
    let expires_at = parsed
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| TimeDelta::try_seconds(seconds.min(MAX_LIFETIME_SECONDS)))
        .map(|lifetime| Utc::now() + lifetime);
    Ok(AccessToken::new(
        SecretString::new(token.to_owned()),
        expires_at,
    ))
}

/// A token's stated lifetime is capped at a year, so a absurd value cannot overflow the date.
const MAX_LIFETIME_SECONDS: i64 = 365 * 24 * 60 * 60;

fn sensitive(text: String) -> Result<HeaderValue, RuntimeError> {
    let mut value = HeaderValue::from_str(&text).map_err(|_| RuntimeError::InvalidRequest)?;
    value.set_sensitive(true);
    Ok(value)
}

fn basic_value(username: &str, password: &SecretString) -> Result<HeaderValue, RuntimeError> {
    let encoded = STANDARD.encode(format!("{username}:{}", password.expose()));
    sensitive(format!("Basic {encoded}"))
}

/// Attaches the auth to a request about to be sent. The caller has already checked the origin.
/// `token` is the OAuth2 token for [`Auth::OAuth2ClientCredentials`].
pub(crate) fn apply(
    auth: &Auth<'_>,
    token: Option<&AccessToken>,
    request: &mut Request,
) -> Result<(), RuntimeError> {
    match auth {
        Auth::None => {}
        Auth::ApiKey {
            placement: ApiKeyPlacement::Header(name),
            value,
        } => {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| RuntimeError::InvalidRequest)?;
            request
                .headers_mut()
                .insert(name, sensitive(value.expose().to_owned())?);
        }
        Auth::ApiKey {
            placement: ApiKeyPlacement::Query(name),
            value,
        } => {
            request
                .url_mut()
                .query_pairs_mut()
                .append_pair(name, value.expose());
        }
        Auth::Basic { username, password } => {
            request
                .headers_mut()
                .insert(AUTHORIZATION, basic_value(username, password)?);
        }
        Auth::Bearer { token } => {
            request.headers_mut().insert(
                AUTHORIZATION,
                sensitive(format!("Bearer {}", token.expose()))?,
            );
        }
        Auth::OAuth2ClientCredentials { .. } => {
            let token = token.ok_or(RuntimeError::InvalidRequest)?;
            request.headers_mut().insert(
                AUTHORIZATION,
                sensitive(format!("Bearer {}", token.token().expose()))?,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_expired_a_minute_before_it_lapses() {
        let now = Utc::now();
        let token = |seconds: i64| {
            AccessToken::new(
                SecretString::new("t".into()),
                Some(now + TimeDelta::seconds(seconds)),
            )
        };

        assert!(!token(120).is_expired(now));
        assert!(token(59).is_expired(now));
        assert!(token(-5).is_expired(now));
        assert!(!AccessToken::new(SecretString::new("t".into()), None).is_expired(now));
    }

    #[test]
    fn debug_shows_the_style_and_no_token() {
        let secret = SecretString::new("hunter2".into());
        let auth = Auth::Bearer { token: &secret };
        let token = AccessToken::new(SecretString::new("hunter2".into()), None);

        assert_eq!(format!("{auth:?}"), "Auth::Bearer");
        assert_eq!(format!("{token:?}"), "AccessToken([redacted])");
    }

    #[test]
    fn each_style_lands_where_it_belongs() {
        let key = SecretString::new("k3y".into());
        let mut request = Request::new(Method::GET, "http://h/path?a=1".parse().unwrap());

        apply(
            &Auth::ApiKey {
                placement: ApiKeyPlacement::Query("api_key"),
                value: &key,
            },
            None,
            &mut request,
        )
        .unwrap();
        assert_eq!(request.url().query(), Some("a=1&api_key=k3y"));

        let mut request = Request::new(Method::GET, "http://h/".parse().unwrap());
        apply(
            &Auth::ApiKey {
                placement: ApiKeyPlacement::Header("x-api-key"),
                value: &key,
            },
            None,
            &mut request,
        )
        .unwrap();
        let header = request.headers().get("x-api-key").unwrap();
        assert_eq!(header, "k3y");
        assert!(header.is_sensitive());

        let mut request = Request::new(Method::GET, "http://h/".parse().unwrap());
        let password = SecretString::new("pw".into());
        apply(
            &Auth::Basic {
                username: "user",
                password: &password,
            },
            None,
            &mut request,
        )
        .unwrap();
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Basic dXNlcjpwdw=="
        );
    }

    #[test]
    fn a_header_value_with_a_control_character_is_an_invalid_request() {
        let bad = SecretString::new("line\nbreak".into());
        let mut request = Request::new(Method::GET, "http://h/".parse().unwrap());

        let result = apply(&Auth::Bearer { token: &bad }, None, &mut request);

        assert_eq!(result, Err(RuntimeError::InvalidRequest));
    }

    // The upstream calls below run against two mock servers: an API that accepts one bearer
    // token, and an identity provider that issues tokens and makes the latest one the valid one.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};

    use super::super::testing::{Mock, context, serve};
    use super::super::{RequestSpec, RuntimeError};

    type TokenLog = Arc<Mutex<Vec<(Option<String>, String)>>>;

    struct Idp {
        mock: Mock,
        issued: Arc<AtomicUsize>,
        /// The authorization header and the form body of each token request.
        requests: TokenLog,
    }

    /// An identity provider whose tokens are `tok-1`, `tok-2`, ... and which says `expires_in`.
    async fn idp(valid: Arc<Mutex<String>>, expires_in: u64, status: StatusCode) -> Idp {
        let issued = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (counted, logged) = (Arc::clone(&issued), Arc::clone(&requests));
        let router = Router::new().route(
            "/token",
            post(move |headers: HeaderMap, body: String| {
                let (issued, requests) = (Arc::clone(&counted), Arc::clone(&logged));
                let valid = Arc::clone(&valid);
                async move {
                    let authorization = headers
                        .get("authorization")
                        .map(|value| value.to_str().unwrap().to_owned());
                    requests.lock().unwrap().push((authorization, body));
                    if status != StatusCode::OK {
                        return (status, "idp says: CLIENTSECRETECHO").into_response();
                    }
                    let n = issued.fetch_add(1, Ordering::SeqCst) + 1;
                    *valid.lock().unwrap() = format!("tok-{n}");
                    axum::Json(serde_json::json!({
                        "access_token": format!("tok-{n}"),
                        "token_type": "Bearer",
                        "expires_in": expires_in,
                    }))
                    .into_response()
                }
            }),
        );
        Idp {
            mock: serve(router).await,
            issued,
            requests,
        }
    }

    /// An API that accepts `Bearer <valid>` and answers 401 to anything else, or to everything.
    async fn api(valid: Arc<Mutex<String>>, always_401: bool) -> Mock {
        serve(Router::new().route(
            "/data",
            get(move |headers: HeaderMap| {
                let valid = Arc::clone(&valid);
                async move {
                    let expected = format!("Bearer {}", valid.lock().unwrap());
                    let ok = headers
                        .get("authorization")
                        .is_some_and(|value| value == expected.as_str());
                    if ok && !always_401 {
                        "data".into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                }
            }),
        ))
        .await
    }

    fn oauth<'a>(idp: &Idp, secret: &'a SecretString, style: ClientAuth) -> Auth<'a> {
        Auth::OAuth2ClientCredentials {
            token_url: idp.mock.url("/token"),
            client_id: "the-client",
            client_secret: secret,
            scopes: &[],
            client_auth: style,
        }
    }

    async fn call(
        ctx: &SourceContext,
        api: &Mock,
        auth: &Auth<'_>,
    ) -> Result<super::super::RuntimeResponse, RuntimeError> {
        ctx.send(auth, RequestSpec::get(&api.base(), api.url("/data")))
            .await
    }

    fn token(value: &str, lifetime: i64) -> AccessToken {
        AccessToken::new(
            SecretString::new(value.into()),
            Some(Utc::now() + TimeDelta::seconds(lifetime)),
        )
    }

    #[tokio::test]
    async fn the_first_call_fetches_a_token_and_the_caller_can_read_it() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());
        let ctx = context("oauth", "s");

        let response = call(&ctx, &api, &oauth(&idp, &secret, ClientAuth::Body))
            .await
            .unwrap();

        assert_eq!(response.body(), "data");
        assert_eq!(idp.issued.load(Ordering::SeqCst), 1);
        let refreshed = ctx.take_refreshed_token().unwrap();
        assert_eq!(refreshed.token().expose(), "tok-1");
        assert!(refreshed.expires_at().unwrap() > Utc::now() + TimeDelta::seconds(3000));
        assert!(
            ctx.take_refreshed_token().is_none(),
            "a token is handed back once"
        );
    }

    #[tokio::test]
    async fn a_cached_token_is_reused_and_nothing_is_handed_back() {
        let valid = Arc::new(Mutex::new("seeded".to_owned()));
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());
        let ctx = context("oauth", "s").with_access_token(token("seeded", 3600));
        let auth = oauth(&idp, &secret, ClientAuth::Body);

        call(&ctx, &api, &auth).await.unwrap();
        call(&ctx, &api, &auth).await.unwrap();

        assert_eq!(idp.issued.load(Ordering::SeqCst), 0);
        assert!(ctx.take_refreshed_token().is_none());
    }

    #[tokio::test]
    async fn an_expired_token_is_refreshed_once_before_the_call() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());
        // Valid for 30 s more, which is inside the 60 s skew.
        let ctx = context("oauth", "s").with_access_token(token("old", 30));

        call(&ctx, &api, &oauth(&idp, &secret, ClientAuth::Body))
            .await
            .unwrap();

        assert_eq!(idp.issued.load(Ordering::SeqCst), 1, "exactly one refresh");
        assert_eq!(api.hits(), 1, "and the call needed no retry");
        assert_eq!(
            ctx.take_refreshed_token().unwrap().token().expose(),
            "tok-1"
        );
    }

    #[tokio::test]
    async fn a_401_on_a_cached_token_refreshes_exactly_once_and_retries_the_call_once() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());
        // Not expired as far as the clock knows, but the upstream has revoked it.
        let ctx = context("oauth", "s").with_access_token(token("revoked", 3600));

        let response = call(&ctx, &api, &oauth(&idp, &secret, ClientAuth::Body))
            .await
            .unwrap();

        assert_eq!(response.body(), "data");
        assert_eq!(idp.issued.load(Ordering::SeqCst), 1, "exactly one refresh");
        assert_eq!(api.hits(), 2, "the revoked token, then the new one");
        assert_eq!(
            ctx.take_refreshed_token().unwrap().token().expose(),
            "tok-1"
        );
    }

    #[tokio::test]
    async fn a_second_401_is_unauthorized_and_triggers_no_further_refresh() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, true).await;
        let secret = SecretString::new("client-secret".into());
        let auth = oauth(&idp, &secret, ClientAuth::Body);

        let cached = context("oauth", "s").with_access_token(token("revoked", 3600));
        assert_eq!(
            call(&cached, &api, &auth).await.unwrap_err(),
            RuntimeError::Unauthorized
        );
        assert_eq!(idp.issued.load(Ordering::SeqCst), 1, "one refresh, not two");
        assert_eq!(api.hits(), 2, "one retry, not two");

        // A token fetched by this very call is not refreshed again when it is refused.
        let empty = context("oauth", "s");
        assert_eq!(
            call(&empty, &api, &auth).await.unwrap_err(),
            RuntimeError::Unauthorized
        );
        assert_eq!(idp.issued.load(Ordering::SeqCst), 2);
        assert_eq!(api.hits(), 3);
    }

    #[tokio::test]
    async fn the_client_proves_itself_in_the_body_or_with_basic_auth_as_configured() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::OK).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());
        let scopes = vec!["read".to_owned(), "write".to_owned()];
        let with = |style| Auth::OAuth2ClientCredentials {
            token_url: idp.mock.url("/token"),
            client_id: "the-client",
            client_secret: &secret,
            scopes: &scopes,
            client_auth: style,
        };

        call(&context("oauth", "a"), &api, &with(ClientAuth::Body))
            .await
            .unwrap();
        call(&context("oauth", "b"), &api, &with(ClientAuth::Basic))
            .await
            .unwrap();

        let requests = idp.requests.lock().unwrap();
        let (body_header, body) = &requests[0];
        assert!(body_header.is_none());
        assert!(body.contains("grant_type=client_credentials"), "{body}");
        assert!(body.contains("scope=read+write"), "{body}");
        assert!(body.contains("client_id=the-client"), "{body}");
        assert!(body.contains("client_secret=client-secret"), "{body}");
        let (basic_header, basic_body) = &requests[1];
        // base64("the-client:client-secret")
        assert_eq!(
            basic_header.as_deref(),
            Some("Basic dGhlLWNsaWVudDpjbGllbnQtc2VjcmV0")
        );
        assert!(
            basic_body.contains("grant_type=client_credentials"),
            "{basic_body}"
        );
        assert!(!basic_body.contains("client_secret"), "{basic_body}");
    }

    #[tokio::test]
    async fn a_token_endpoint_failure_is_reported_by_status_and_never_echoes_its_body() {
        let valid = Arc::default();
        let idp = idp(Arc::clone(&valid), 3600, StatusCode::BAD_REQUEST).await;
        let api = api(valid, false).await;
        let secret = SecretString::new("client-secret".into());

        let error = call(
            &context("oauth", "s"),
            &api,
            &oauth(&idp, &secret, ClientAuth::Body),
        )
        .await
        .unwrap_err();

        assert_eq!(error, RuntimeError::TokenEndpoint { status: Some(400) });
        assert!(!format!("{error} {error:?}").contains("CLIENTSECRETECHO"));
        assert_eq!(api.hits(), 0, "no call is made without a token");
    }

    #[tokio::test]
    async fn the_api_key_basic_and_bearer_styles_arrive_as_the_upstream_sees_them() {
        let upstream = serve(Router::new().route("/r", get(|| async { "ok" }))).await;
        let key = SecretString::new("the-key".into());
        let ctx = context("styles", "s");
        let get_r = |auth| {
            let ctx = ctx.clone();
            let spec = RequestSpec::get(&upstream.base(), upstream.url("/r?page=2"));
            async move { ctx.send(&auth, spec).await.unwrap() }
        };
        get_r(Auth::ApiKey {
            placement: ApiKeyPlacement::Header("x-api-key"),
            value: &key,
        })
        .await;
        get_r(Auth::ApiKey {
            placement: ApiKeyPlacement::Query("api_key"),
            value: &key,
        })
        .await;
        get_r(Auth::Basic {
            username: "user",
            password: &key,
        })
        .await;
        get_r(Auth::Bearer { token: &key }).await;
        get_r(Auth::None).await;

        let seen = upstream.seen();
        let header = |at: usize, name: &str| {
            seen[at]
                .headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        assert_eq!(header(0, "x-api-key").as_deref(), Some("the-key"));
        assert_eq!(seen[0].path_and_query, "/r?page=2");
        assert_eq!(seen[1].path_and_query, "/r?page=2&api_key=the-key");
        assert_eq!(header(1, "x-api-key"), None);
        // base64("user:the-key")
        assert_eq!(
            header(2, "authorization").as_deref(),
            Some("Basic dXNlcjp0aGUta2V5")
        );
        assert_eq!(
            header(3, "authorization").as_deref(),
            Some("Bearer the-key")
        );
        assert_eq!(header(4, "authorization"), None);
        assert_eq!(seen[4].path_and_query, "/r?page=2");
    }
}
