//! The one call path: [`send`] takes a token from the rate limit, attaches the auth, executes
//! through the guarded client and classifies the result, retrying as the policy says.

use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, LOCATION, RETRY_AFTER};
use reqwest::{Method, Request, StatusCode};
use serde_json::Value;
use url::{Origin, Url};

use super::auth::{self, Auth};
use super::error::RuntimeError;
use super::retry::{self, RetryReason, parse_retry_after};
use crate::sources::SourceContext;

/// One request a source wants made. It names the source's base URL, whose origin is the only one
/// the auth is ever sent to: a request for any other origin is refused before it is sent.
#[derive(Clone, Debug)]
pub struct RequestSpec {
    method: Method,
    url: Url,
    base: Origin,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    idempotent: bool,
}

impl RequestSpec {
    /// A request for `url`, on a source whose base URL is `base`. `GET` and `HEAD` are
    /// idempotent, and so retried; call [`RequestSpec::idempotent`] to say otherwise.
    pub fn new(method: Method, base: &Url, url: Url) -> Self {
        let idempotent = method == Method::GET || method == Method::HEAD;
        Self {
            method,
            url,
            base: base.origin(),
            headers: Vec::new(),
            body: None,
            idempotent,
        }
    }

    pub fn get(base: &Url, url: Url) -> Self {
        Self::new(Method::GET, base, url)
    }

    /// Marks the request safe to repeat, for example a `POST` that only reads.
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Appends a query parameter.
    pub fn query(mut self, name: &str, value: &str) -> Self {
        self.url.query_pairs_mut().append_pair(name, value);
        self
    }

    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// A JSON body, with its content type.
    pub fn json(mut self, value: &Value) -> Self {
        self.body = Some(Bytes::from(value.to_string()));
        self.headers.push((
            CONTENT_TYPE.as_str().to_owned(),
            "application/json".to_owned(),
        ));
        self
    }

    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    /// The same request for another URL on the same source, as a pagination step makes.
    pub(crate) fn with_url(&self, url: Url) -> Self {
        Self {
            url,
            ..self.clone()
        }
    }

    /// Whether `url` is on the source's base origin. An origin that is not a tuple of scheme,
    /// host and port (a `file:` or `data:` URL) never is.
    pub(crate) fn owns(&self, url: &Url) -> bool {
        self.base.is_tuple() && url.origin() == self.base
    }

    fn build(
        &self,
        auth: &Auth<'_>,
        token: Option<&auth::AccessToken>,
    ) -> Result<Request, RuntimeError> {
        let mut request = Request::new(self.method.clone(), self.url.clone());
        for (name, value) in &self.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| RuntimeError::InvalidRequest)?;
            let value = HeaderValue::from_str(value).map_err(|_| RuntimeError::InvalidRequest)?;
            request.headers_mut().append(name, value);
        }
        if let Some(body) = &self.body {
            *request.body_mut() = Some(body.clone().into());
        }
        auth::apply(auth, token, &mut request)?;
        Ok(request)
    }
}

/// A successful answer: its status, headers and body, read under the client's size cap.
#[derive(Debug)]
pub struct RuntimeResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl RuntimeResponse {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body as JSON; anything else is [`RuntimeError::UnexpectedShape`].
    pub fn json(&self) -> Result<Value, RuntimeError> {
        serde_json::from_slice(&self.body).map_err(|_| RuntimeError::UnexpectedShape)
    }
}

/// The `outcome` label of `zirv_flow_source_requests_total`.
fn outcome(status: StatusCode) -> &'static str {
    if status.is_server_error() {
        "server_error"
    } else if status.is_client_error() {
        "client_error"
    } else {
        "success"
    }
}

fn count_request(kind: &'static str, outcome: &'static str) {
    metrics::counter!(
        "zirv_flow_source_requests_total",
        "source_type" => kind,
        "outcome" => outcome
    )
    .increment(1);
}

/// Whether a redirect response points off the source's base origin.
fn leaves_origin(spec: &RequestSpec, from: &Url, headers: &HeaderMap) -> bool {
    headers
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|target| from.join(target).ok())
        .is_some_and(|target| !spec.owns(&target))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?;
    parse_retry_after(value, Utc::now())
}

/// Sends `spec` for `ctx`. See the module documentation; the loop is bounded because a retry
/// uses an attempt and a 401 refreshes the token at most once.
pub(crate) async fn send(
    ctx: &SourceContext,
    auth: &Auth<'_>,
    spec: RequestSpec,
) -> Result<RuntimeResponse, RuntimeError> {
    if !spec.owns(spec.url()) {
        return Err(RuntimeError::CrossOrigin);
    }
    let kind = ctx.kind();
    let policy = ctx.retry_policy();
    // Attempts that failed in a way worth retrying.
    let mut failed = 0_u32;
    // Whether this call has fetched a token, so a 401 after it is final.
    let mut fetched = false;
    let mut force_refresh = false;
    loop {
        let token = if auth.is_oauth() {
            let (token, new) = auth::ensure_token(ctx, auth, force_refresh).await?;
            force_refresh = false;
            fetched |= new;
            Some(token)
        } else {
            None
        };
        ctx.wait_for_rate_limit().await;
        let request = spec.build(auth, token.as_ref())?;

        let (reason, last_status, wait) = match ctx.http().execute(request).await {
            Err(error) => {
                count_request(kind, "transport_error");
                match retry::error_reason(&error) {
                    Some(reason) if spec.idempotent => (reason, None, None),
                    _ => return Err(RuntimeError::Http(error)),
                }
            }
            Ok(response) => {
                let status = response.status();
                count_request(kind, outcome(status));
                // A redirect can have led off the base origin, past the check above.
                if !spec.owns(response.url()) {
                    return Err(RuntimeError::CrossOrigin);
                }
                // The guarded client does not follow a redirect to another origin; it hands the
                // 3xx back, and the credential was never sent there.
                if status.is_redirection()
                    && leaves_origin(&spec, response.url(), response.headers())
                {
                    return Err(RuntimeError::CrossOrigin);
                }
                if status == StatusCode::UNAUTHORIZED {
                    if auth.is_oauth() && !fetched {
                        force_refresh = true;
                        continue;
                    }
                    return Err(RuntimeError::Unauthorized);
                }
                match retry::status_reason(status) {
                    Some(reason) if spec.idempotent => (
                        reason,
                        Some(status.as_u16()),
                        retry_after(response.headers()),
                    ),
                    _ if status.is_client_error() || status.is_server_error() => {
                        return Err(RuntimeError::Status(status.as_u16()));
                    }
                    _ => {
                        let headers = response.headers().clone();
                        let body = response.body().await?;
                        return Ok(RuntimeResponse {
                            status,
                            headers,
                            body,
                        });
                    }
                }
            }
        };

        failed += 1;
        let delay = policy.delay(failed, reason, wait, rand::random())?;
        if failed >= policy.max_attempts {
            return Err(RuntimeError::RetriesExhausted {
                last_status,
                attempts: failed,
            });
        }
        retrying(kind, reason, failed, delay);
        tokio::time::sleep(delay).await;
    }
}

fn retrying(kind: &'static str, reason: RetryReason, attempt: u32, delay: Duration) {
    metrics::counter!(
        "zirv_flow_source_retries_total",
        "source_type" => kind,
        "reason" => reason.as_str()
    )
    .increment(1);
    tracing::warn!(
        source_type = kind,
        reason = reason.as_str(),
        attempt,
        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        "retrying a source request"
    );
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::{get, post};
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use tracing_subscriber::fmt::MakeWriter;
    use zirv_flow_core::SecretString;

    use crate::http::HttpError;
    use crate::sources::SourceError;

    use super::super::auth::{ApiKeyPlacement, ClientAuth};
    use super::super::testing::{closed_port, context, context_sharing, serve};
    use super::super::{RateLimit, RateLimits, RetryPolicy};
    use super::*;

    fn counter() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    /// A route that answers `first` once with a `Retry-After`, then 200.
    fn flaky(calls: &Arc<AtomicUsize>, status: StatusCode, retry_after: String) -> Router {
        let calls = Arc::clone(calls);
        Router::new().route(
            "/r",
            get(move || {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                let retry_after = retry_after.clone();
                async move {
                    if attempt == 0 {
                        (status, [("retry-after", retry_after)], "wait").into_response()
                    } else {
                        "fine".into_response()
                    }
                }
            }),
        )
    }

    async fn get_r(
        context: &SourceContext,
        mock: &super::super::testing::Mock,
    ) -> Result<RuntimeResponse, RuntimeError> {
        context
            .send(&Auth::None, RequestSpec::get(&mock.base(), mock.url("/r")))
            .await
    }

    #[tokio::test]
    async fn a_429_with_retry_after_seconds_is_waited_out_and_retried() {
        let calls = counter();
        let mock = serve(flaky(&calls, StatusCode::TOO_MANY_REQUESTS, "1".into())).await;
        let started = Instant::now();

        let response = get_r(&context("probe", "s"), &mock).await.unwrap();

        assert_eq!(response.body(), "fine");
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_retry_after_http_date_is_waited_out() {
        let calls = counter();
        let date = (Utc::now() + chrono::TimeDelta::seconds(2))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let mock = serve(flaky(&calls, StatusCode::SERVICE_UNAVAILABLE, date)).await;
        let started = Instant::now();

        let response = get_r(&context("probe", "s"), &mock).await.unwrap();

        assert_eq!(response.body(), "fine");
        // The date has whole-second resolution, so the wait is between one and two seconds.
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausted_retries_end_in_a_typed_error_after_the_last_attempt() {
        let mock =
            serve(Router::new().route("/r", get(|| async { StatusCode::SERVICE_UNAVAILABLE })))
                .await;

        let error = get_r(&context("probe", "s"), &mock).await.unwrap_err();

        assert_eq!(
            error,
            RuntimeError::RetriesExhausted {
                last_status: Some(503),
                attempts: 5
            }
        );
        assert_eq!(mock.hits(), 5);
    }

    #[tokio::test]
    async fn a_retry_after_beyond_the_cap_ends_the_call_without_sleeping() {
        let calls = counter();
        let mock = serve(flaky(&calls, StatusCode::TOO_MANY_REQUESTS, "900".into())).await;
        let started = Instant::now();

        let error = get_r(&context("probe", "s"), &mock).await.unwrap_err();

        assert_eq!(error, RuntimeError::RetryAfterTooLong { seconds: 900 });
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_error_is_retried_with_backoff_until_it_recovers() {
        let calls = counter();
        let router = {
            let calls = Arc::clone(&calls);
            Router::new().route(
                "/r",
                get(move || {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if attempt < 2 {
                            StatusCode::BAD_GATEWAY.into_response()
                        } else {
                            "fine".into_response()
                        }
                    }
                }),
            )
        };
        let mock = serve(router).await;

        let response = get_r(&context("probe", "s"), &mock).await.unwrap();

        assert_eq!(response.body(), "fine");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_post_is_not_retried_unless_it_is_marked_idempotent() {
        let mock =
            serve(Router::new().route("/r", post(|| async { StatusCode::SERVICE_UNAVAILABLE })))
                .await;
        let ctx = context("probe", "s");
        let post_r = |idempotent| {
            RequestSpec::new(Method::POST, &mock.base(), mock.url("/r")).idempotent(idempotent)
        };

        let plain = ctx.send(&Auth::None, post_r(false)).await.unwrap_err();
        assert_eq!(plain, RuntimeError::Status(503));
        assert_eq!(mock.hits(), 1);

        let marked = ctx.send(&Auth::None, post_r(true)).await.unwrap_err();
        assert!(matches!(marked, RuntimeError::RetriesExhausted { .. }));
        assert_eq!(mock.hits(), 6);
    }

    #[tokio::test]
    async fn other_client_errors_are_final_on_the_first_answer() {
        let mock = serve(Router::new().route("/r", get(|| async { StatusCode::NOT_FOUND }))).await;

        let error = get_r(&context("probe", "s"), &mock).await.unwrap_err();

        assert_eq!(error, RuntimeError::Status(404));
        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test]
    async fn a_connection_failure_is_retried_then_reported_without_a_status() {
        let port = closed_port().await;
        let base: Url = format!("http://127.0.0.1:{port}/").parse().unwrap();
        let policy = RetryPolicy {
            max_attempts: 2,
            ..super::super::testing::fast_retries()
        };

        let error = context("probe", "s")
            .with_retry_policy(policy)
            .send(&Auth::None, RequestSpec::get(&base, base.clone()))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            RuntimeError::RetriesExhausted {
                last_status: None,
                attempts: 2
            }
        );
    }

    #[tokio::test]
    async fn an_egress_refusal_is_final_and_not_retried() {
        let ctx = SourceContext::new(
            Arc::new(zirv_flow_core::egress::EgressPolicy::default()),
            crate::http::HttpLimits::default(),
            "probe",
            "s",
            Arc::default(),
        )
        .unwrap();
        let base: Url = "http://127.0.0.1:9/".parse().unwrap();

        let error = ctx
            .send(&Auth::None, RequestSpec::get(&base, base.clone()))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            RuntimeError::Http(HttpError::EgressDenied { .. })
        ));
    }

    /// One of every style of auth.
    fn each_auth<'a>(secret: &'a SecretString, token_url: &Url) -> Vec<Auth<'a>> {
        vec![
            Auth::None,
            Auth::ApiKey {
                placement: ApiKeyPlacement::Header("x-api-key"),
                value: secret,
            },
            Auth::ApiKey {
                placement: ApiKeyPlacement::Query("api_key"),
                value: secret,
            },
            Auth::Basic {
                username: "user",
                password: secret,
            },
            Auth::Bearer { token: secret },
            Auth::OAuth2ClientCredentials {
                token_url: token_url.clone(),
                client_id: "id",
                client_secret: secret,
                scopes: &[],
                client_auth: ClientAuth::Body,
            },
        ]
    }

    #[tokio::test]
    async fn a_request_for_another_origin_is_refused_with_every_auth_style_and_nothing_is_sent() {
        let source = serve(Router::new().route("/r", get(|| async { "mine" }))).await;
        let other = serve(Router::new().route("/r", get(|| async { "theirs" }))).await;
        let secret = SecretString::new("s3cret-value".into());
        let ctx = context("probe", "s");
        // The token endpoint is the source's own here, so a fetch would show as a source hit.
        let token_url = source.url("/token");

        for auth in each_auth(&secret, &token_url) {
            let spec = RequestSpec::get(&source.base(), other.url("/r?x=1"));
            assert_eq!(
                ctx.send(&auth, spec).await.unwrap_err(),
                RuntimeError::CrossOrigin,
                "{auth:?}"
            );
        }

        assert_eq!(other.hits(), 0, "the other origin received a request");
        assert_eq!(
            source.hits(),
            0,
            "no token was fetched for a refused request"
        );
    }

    #[tokio::test]
    async fn the_same_host_on_another_port_or_scheme_is_another_origin() {
        let source = serve(Router::new().route("/r", get(|| async { "mine" }))).await;
        let other = serve(Router::new().route("/r", get(|| async { "theirs" }))).await;
        let ctx = context("probe", "s");
        let secret = SecretString::new("k".into());
        let auth = Auth::Bearer { token: &secret };

        let port = ctx
            .send(&auth, RequestSpec::get(&source.base(), other.url("/r")))
            .await;
        let scheme = ctx
            .send(
                &auth,
                RequestSpec::get(
                    &source.base(),
                    format!("https://127.0.0.1:{}/r", source.port)
                        .parse()
                        .unwrap(),
                ),
            )
            .await;
        let own = ctx
            .send(
                &auth,
                RequestSpec::get(&source.base(), source.url("/r?page=2")),
            )
            .await;

        assert_eq!(port.unwrap_err(), RuntimeError::CrossOrigin);
        assert_eq!(scheme.unwrap_err(), RuntimeError::CrossOrigin);
        assert_eq!(own.unwrap().body(), "mine");
        assert_eq!(other.hits(), 0);
    }

    #[tokio::test]
    async fn a_redirect_off_the_base_origin_is_stopped_before_any_credential_reaches_it() {
        let other = serve(Router::new().route("/r", get(|| async { "theirs" }))).await;
        let target = other.url("/r").to_string();
        let source = serve(Router::new().route(
            "/r",
            get(move || {
                let target = target.clone();
                async move { Redirect::temporary(&target) }
            }),
        ))
        .await;

        let key = SecretString::new("redirect-key-5d1".into());
        let ctx = context("probe", "s");
        for auth in [
            Auth::ApiKey {
                placement: ApiKeyPlacement::Header("x-api-key"),
                value: &key,
            },
            Auth::ApiKey {
                placement: ApiKeyPlacement::Query("api_key"),
                value: &key,
            },
        ] {
            let spec = RequestSpec::get(&source.base(), source.url("/r"));
            let error = ctx.send(&auth, spec).await.unwrap_err();

            assert_eq!(error, RuntimeError::CrossOrigin);
            let text = format!("{error} {error:?}");
            assert!(
                !text.contains("http") && !text.contains("127.0.0.1"),
                "{text}"
            );
        }

        assert_eq!(
            other.hits(),
            0,
            "the credential never reached the other origin"
        );
        assert_eq!(source.hits(), 2);
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[tokio::test]
    async fn no_secret_appears_in_any_error_or_in_the_logs() {
        const KEY: &str = "KEY-in-the-query-7f3a";
        const BEARER: &str = "BEARER-token-91bc";
        const CLIENT_SECRET: &str = "CLIENT-secret-44de";
        const BODY: &str = "BODY-text-from-upstream-2e";
        let capture = Capture::default();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .finish(),
        );
        let upstream = serve(
            Router::new()
                .route("/missing", get(|| async { (StatusCode::NOT_FOUND, BODY) }))
                .route(
                    "/down",
                    get(|| async { (StatusCode::SERVICE_UNAVAILABLE, BODY) }),
                )
                .route(
                    "/denied",
                    get(|| async { (StatusCode::UNAUTHORIZED, BODY) }),
                )
                .route("/token", post(|| async { (StatusCode::BAD_REQUEST, BODY) })),
        )
        .await;
        let elsewhere = serve(Router::new().route("/r", get(|| async { "x" }))).await;
        let dead = closed_port().await;
        let dead_url: Url = format!("http://127.0.0.1:{dead}/r").parse().unwrap();
        let ctx = context("leaky", "s");
        let key = SecretString::new(KEY.into());
        let bearer = SecretString::new(BEARER.into());
        let client_secret = SecretString::new(CLIENT_SECRET.into());
        let by_query = Auth::ApiKey {
            placement: ApiKeyPlacement::Query("api_key"),
            value: &key,
        };
        let by_bearer = Auth::Bearer { token: &bearer };
        let oauth = Auth::OAuth2ClientCredentials {
            token_url: upstream.url("/token"),
            client_id: "id",
            client_secret: &client_secret,
            scopes: &[],
            client_auth: ClientAuth::Body,
        };

        let get = |path: &str| RequestSpec::get(&upstream.base(), upstream.url(path));
        let errors = vec![
            ctx.send(&by_query, get("/missing")).await.unwrap_err(),
            ctx.send(&by_query, get("/down")).await.unwrap_err(),
            ctx.send(&by_bearer, get("/denied")).await.unwrap_err(),
            ctx.send(&oauth, get("/missing")).await.unwrap_err(),
            ctx.send(
                &by_query,
                RequestSpec::get(&upstream.base(), elsewhere.url("/r")),
            )
            .await
            .unwrap_err(),
            ctx.clone()
                .with_retry_policy(RetryPolicy {
                    max_attempts: 2,
                    ..super::super::testing::fast_retries()
                })
                .send(&by_query, RequestSpec::get(&dead_url, dead_url.clone()))
                .await
                .unwrap_err(),
        ];

        assert_eq!(errors.len(), 6);
        let mut seen = String::new();
        for error in errors {
            seen.push_str(&format!("{error} | {error:?} | "));
            seen.push_str(&format!("{}\n", SourceError::from(error)));
        }
        seen.push_str(&format!("{by_query:?} {by_bearer:?} {oauth:?}\n"));
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("retrying a source request"),
            "no log was captured: {logs}"
        );
        for secret in [KEY, BEARER, CLIENT_SECRET, BODY, "api_key"] {
            assert!(
                !seen.contains(secret),
                "{secret} leaked into an error: {seen}"
            );
            assert!(
                !logs.contains(secret),
                "{secret} leaked into the logs: {logs}"
            );
        }
        assert!(
            !seen.contains("127.0.0.1") && !seen.contains("http"),
            "{seen}"
        );
    }

    fn recorder() -> &'static PrometheusHandle {
        static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
        HANDLE.get_or_init(|| {
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            metrics::set_global_recorder(recorder).unwrap();
            handle
        })
    }

    /// The rendered lines of `metric` for source type `kind`.
    fn lines(rendered: &str, metric: &str, kind: &str) -> Vec<String> {
        rendered
            .lines()
            .filter(|line| {
                line.starts_with(metric) && line.contains(&format!("source_type=\"{kind}\""))
            })
            .map(str::to_owned)
            .collect()
    }

    #[tokio::test]
    async fn the_counters_move_with_the_source_type_and_no_url_or_host_label() {
        let handle = recorder();
        let calls = counter();
        let mock = serve(
            flaky(&calls, StatusCode::TOO_MANY_REQUESTS, "0".into())
                .route("/missing", get(|| async { StatusCode::NOT_FOUND })),
        )
        .await;
        let ctx = context("metered", "s");

        get_r(&ctx, &mock).await.unwrap();
        ctx.send(
            &Auth::None,
            RequestSpec::get(&mock.base(), mock.url("/missing")),
        )
        .await
        .unwrap_err();

        let rendered = handle.render();
        let requests = lines(&rendered, "zirv_flow_source_requests_total", "metered");
        let has = |lines: &[String], label: &str, value: u64| {
            lines
                .iter()
                .any(|line| line.contains(label) && line.ends_with(&format!(" {value}")))
        };
        assert!(has(&requests, "outcome=\"success\"", 1), "{requests:?}");
        assert!(
            has(&requests, "outcome=\"client_error\"", 2),
            "{requests:?}"
        );
        let retries = lines(&rendered, "zirv_flow_source_retries_total", "metered");
        assert!(has(&retries, "reason=\"rate_limited\"", 1), "{retries:?}");
        for line in requests.iter().chain(&retries) {
            assert!(
                !line.contains("127.0.0.1") && !line.contains(&mock.port.to_string()),
                "{line}"
            );
            assert!(
                !line.contains("http") && !line.contains("/missing"),
                "{line}"
            );
        }
    }

    #[tokio::test]
    async fn contexts_for_one_source_share_the_bucket_and_the_wait_is_metered() {
        let handle = recorder();
        let mock = serve(Router::new().route("/r", get(|| async { "ok" }))).await;
        let rate = Some(RateLimit {
            requests: 2,
            per: Duration::from_secs(1),
        });
        let shared = Arc::<RateLimits>::default();
        let first =
            context_sharing("limited", "same-source", Arc::clone(&shared)).with_rate_limit(rate);
        let second =
            context_sharing("limited", "same-source", Arc::clone(&shared)).with_rate_limit(rate);
        let started = Instant::now();

        for round in 0..5 {
            let ctx = if round % 2 == 0 { &first } else { &second };
            get_r(ctx, &mock).await.unwrap();
        }

        // 2 go through at once, then one every half second: 3 more take 1.5 s.
        assert!(
            started.elapsed() >= Duration::from_millis(1450),
            "{:?}",
            started.elapsed()
        );
        let waits = lines(
            &handle.render(),
            "zirv_flow_source_rate_limit_wait_seconds_total",
            "limited",
        );
        assert_eq!(waits.len(), 1, "{waits:?}");
        let seconds: f64 = waits[0].rsplit(' ').next().unwrap().parse().unwrap();
        assert!(seconds >= 1.0, "{waits:?}");
        assert!(!waits[0].contains("same-source") && !waits[0].contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn a_source_with_no_rate_limit_is_never_delayed() {
        let mock = serve(Router::new().route("/r", get(|| async { "ok" }))).await;
        let ctx = context("unlimited", "s");
        let started = Instant::now();

        for _ in 0..5 {
            get_r(&ctx, &mock).await.unwrap();
        }

        assert!(started.elapsed() < Duration::from_millis(1000));
    }
}
