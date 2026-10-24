//! The `provider` destination: an email provider defined by a manifest (architecture
//! "Provider-manifest destinations", #223). The destination's configuration is the manifest
//! (`config.manifest`, the same JSON the `http` source takes) with a `destination` section; this
//! module executes that section and nothing else. There is no expression language.
//!
//! **Sending.** The email payload of the send pipeline is placed in a JSON body at the pointers
//! the manifest names, with the message id in the idempotency header, and sent once with the
//! manifest's auth through the guarded client. The delivery pipeline owns retries, so a call
//! here is a single request (no hidden retry would multiply a send). A 2xx answer is a delivery;
//! any other is classified by the manifest's rules, then the defaults (see [`classify`]).
//!
//! **Credential.** The credential holds the fields `auth.credential` declares: what the auth type
//! reads (`token`, `api_key`, `username` and `password`) and the feedback signing secret. A secret
//! never reaches an error, a log line or a metric: errors name a field, never a value.
//!
//! **Feedback and domain status.** [`read_feedback`] and [`feedback_scheme`] are the pure halves
//! of the API's feedback route, which verifies before it parses; [`domain_status`] asks the
//! provider about a sending domain and maps the reply, read-only.

mod body;
mod classify;
mod domain;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod fake;
mod feedback;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{Method, Request};
use serde_json::Value;
use url::Url;

pub use self::domain::{ProviderRecord, ProviderRecordStatus};
pub use self::feedback::{Feedback, FeedbackKind, FeedbackRead};
use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::manifest::{
    Auth, FeedbackSpec, KeyLocation, Manifest, Provider, SendMethod, VerificationSpec, validate,
};
use crate::sources::{Credential, FieldError, FieldKind, FieldSpec};

pub(crate) struct ProviderDestination;

const fn secret(key: &'static str, label: &'static str, help: &'static str) -> FieldSpec {
    FieldSpec {
        key,
        label,
        help,
        kind: FieldKind::Text,
        required: false,
        secret: true,
    }
}

impl Destination for ProviderDestination {
    const SPEC: DestinationSpec = DestinationSpec {
        kind: "provider",
        label: "Email provider (manifest)",
        description: "An email provider's HTTP API, described by a manifest: how to send a message, how to read its errors and its feedback webhook.",
        config_fields: &[FieldSpec {
            key: "manifest",
            label: "Manifest",
            help: "A JSON description of the provider (manifest v1 with a destination section). It holds no secret: the credential holds those.",
            kind: FieldKind::Json,
            required: true,
            secret: false,
        }],
        credential_fields: &[
            secret(
                "api_key",
                "API key",
                "For an api_key manifest: sent in the header or query parameter the manifest names.",
            ),
            secret("token", "Bearer token", "For a bearer manifest."),
            secret("username", "User name", "For a basic manifest."),
            secret("password", "Password", "For a basic manifest."),
        ],
        capabilities: Capabilities {
            channels: &["email"],
            idempotency_key: true,
            batching: false,
            rate_limit: true,
            erase: false,
            signs_dkim: false,
        },
        config_check: Some(check_config),
    };

    async fn check(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), DestinationError> {
        let (manifest, provider) = provider_of(config)?;
        // The auth and the signing secret must be there; nothing is sent.
        let probe_url = joined(&manifest.base_url, "/")?;
        let mut probe = Request::new(Method::GET, probe_url);
        apply_auth(&mut probe, &manifest.auth, credential)?;
        let field = &provider.feedback.verification.secret_field;
        if credential.get(field).is_none() {
            return Err(missing(field));
        }
        if let Some(host) = manifest.base_url.host_str() {
            ctx.resolve_checked(host.trim_matches(['[', ']'])).await?;
        }
        Ok(())
    }

    async fn deliver(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        message: &Outbound<'_>,
    ) -> Result<Delivered, DestinationError> {
        let (manifest, provider) = provider_of(config)?;
        let body = body::build(
            &provider.send,
            message.payload,
            message.to,
            message.idempotency_key,
        )?;
        let url = joined(&manifest.base_url, &provider.send.path)?;
        let method = match provider.send.method {
            SendMethod::Post => Method::POST,
            SendMethod::Put => Method::PUT,
            SendMethod::Patch => Method::PATCH,
        };
        let mut request = Request::new(method, url);
        let headers = manifest
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain([(CONTENT_TYPE.as_str(), "application/json")]);
        add_headers(&mut request, headers)?;
        // The idempotency header replaces any header of the same name: the request carries exactly
        // one value, the message id.
        request.headers_mut().insert(
            HeaderName::from_bytes(provider.send.idempotency_header.as_bytes())
                .map_err(|_| unbuildable())?,
            HeaderValue::from_str(message.idempotency_key).map_err(|_| unbuildable())?,
        );
        *request.body_mut() = Some(body.to_string().into_bytes().into());
        apply_auth(&mut request, &manifest.auth, credential)?;

        let response = ctx.http().execute(request).await?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            // The body is dropped unread.
            return Ok(Delivered {
                status: Some(status),
                remote_id: None,
            });
        }
        Err(failure(&provider, response).await)
    }
}

/// The settings problems of `config`: the manifest's issues, and a manifest without a
/// `destination` section. Pure; `validate_config` runs it for a `provider` destination.
pub(crate) fn check_config(config: &Value) -> Vec<FieldError> {
    match config.get("manifest") {
        None | Some(Value::Null) => Vec::new(),
        Some(manifest) => match validate(manifest) {
            Err(issues) => issues
                .into_iter()
                .map(|issue| FieldError {
                    field: format!("manifest{}", issue.path),
                    reason: issue.code.to_owned(),
                })
                .collect(),
            Ok(parsed) if parsed.provider.is_none() => vec![FieldError {
                field: "manifest/destination".to_owned(),
                reason: "is required for a provider destination".to_owned(),
            }],
            Ok(_) => Vec::new(),
        },
    }
}

/// The validated manifest of `config` and its `destination` section, or the setting that is
/// wrong. The error names the first issue's code and pointer, never a value.
fn provider_of(config: &Value) -> Result<(Manifest, Provider), DestinationError> {
    let value = config.get("manifest").ok_or_else(|| missing("manifest"))?;
    let manifest = validate(value).map_err(|issues| {
        let first = issues.first();
        DestinationError::Config {
            field: format!("manifest{}", first.map_or("", |issue| issue.path.as_str())),
            reason: format!(
                "is invalid ({})",
                first.map_or("unknown", |issue| issue.code)
            ),
        }
    })?;
    let provider = manifest
        .provider
        .clone()
        .ok_or_else(|| DestinationError::Config {
            field: "manifest/destination".to_owned(),
            reason: "is required for a provider destination".to_owned(),
        })?;
    Ok((manifest, provider))
}

/// The feedback section and the signature scheme of a provider destination's configuration, for
/// the route that receives its webhook. `None` when `config` is not a valid provider manifest.
pub fn feedback_of(config: &Value) -> Option<(FeedbackSpec, VerificationSpec)> {
    let (_, provider) = provider_of(config).ok()?;
    let verification = provider.feedback.verification.clone();
    Some((provider.feedback, verification))
}

/// The signature scheme of a verification section, as `zirv_flow_core::signature` takes it.
pub fn feedback_scheme(verification: &VerificationSpec) -> zirv_flow_core::signature::Scheme<'_> {
    feedback::scheme(verification)
}

/// Reads one event from a feedback body whose signature verified.
pub fn read_feedback(spec: &FeedbackSpec, body: &Value) -> FeedbackRead {
    feedback::read(spec, body)
}

/// The rate and burst a new destination of this manifest starts with, from the manifest's
/// `destination.rate_limit` (deliveries per second, rounded up, and the most at once). `None`
/// when the manifest declares none.
pub fn default_rate(config: &Value) -> Option<(i32, i32)> {
    let (_, provider) = provider_of(config).ok()?;
    let limit = provider.rate_limit?;
    let per_second = limit.requests.div_ceil(limit.per_seconds).clamp(1, 100_000);
    let burst = limit.requests.clamp(per_second, 100_000);
    Some((i32::try_from(per_second).ok()?, i32::try_from(burst).ok()?))
}

/// Asks the provider for the state of `domain`, with the request the manifest's `domain_status`
/// declares: a GET with the domain as its one query parameter. Read-only; it never changes the
/// provider or our records. A manifest that declares no such request is
/// [`DestinationError::Unsupported`].
pub async fn domain_status(
    ctx: &DestinationContext,
    config: &Value,
    credential: &Credential,
    domain: &str,
) -> Result<Vec<ProviderRecord>, DestinationError> {
    let (manifest, provider) = provider_of(config)?;
    let spec = provider
        .domain_status
        .as_ref()
        .ok_or(DestinationError::Unsupported)?;
    let mut url = joined(&manifest.base_url, &spec.path)?;
    url.query_pairs_mut()
        .append_pair(&spec.domain_param, domain);
    let mut request = Request::new(Method::GET, url);
    add_headers(
        &mut request,
        manifest
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )?;
    apply_auth(&mut request, &manifest.auth, credential)?;

    let response = ctx.http().execute(request).await?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(failure(&provider, response).await);
    }
    let bytes = response.body().await.map_err(|_| {
        DestinationError::Other("the provider's reply could not be read".to_owned())
    })?;
    let reply: Value = serde_json::from_slice(&bytes)
        .map_err(|_| DestinationError::Other("the provider's reply is not JSON".to_owned()))?;
    Ok(domain::map(spec, &reply))
}

/// The error of a response that was not a success.
async fn failure(provider: &Provider, response: crate::http::GuardedResponse) -> DestinationError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // The body only matters to a rule that reads it; one that cannot be read is no body.
    let body = response
        .body()
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let action = classify::classify(&provider.errors, status, body.as_ref());
    classify::error_for(action, status, retry_after)
}

fn missing(field: &str) -> DestinationError {
    DestinationError::Config {
        field: field.to_owned(),
        reason: "is required".to_owned(),
    }
}

fn unbuildable() -> DestinationError {
    DestinationError::Config {
        field: "manifest".to_owned(),
        reason: "does not make a valid request".to_owned(),
    }
}

/// `base` with `path` appended to its own path (`/v1` + `/messages` is `/v1/messages`).
fn joined(base: &Url, path: &str) -> Result<Url, DestinationError> {
    let mut url = base.clone();
    let prefix = base.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{path}"));
    url.set_query(None);
    url.set_fragment(None);
    if url.cannot_be_a_base() {
        return Err(unbuildable());
    }
    Ok(url)
}

fn add_headers<'a>(
    request: &mut Request,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), DestinationError> {
    for (name, value) in headers {
        request.headers_mut().append(
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| unbuildable())?,
            HeaderValue::from_str(value).map_err(|_| unbuildable())?,
        );
    }
    Ok(())
}

/// Puts the manifest's auth on `request`, from the credential. The value is marked sensitive so a
/// debug print of the request cannot show it. A credential field the auth reads and the
/// destination lacks is a setting error naming the field.
fn apply_auth(
    request: &mut Request,
    auth: &Auth,
    credential: &Credential,
) -> Result<(), DestinationError> {
    let required = |name: &str| {
        credential
            .get(name)
            .ok_or_else(|| missing(&format!("credential.{name}")))
    };
    let sensitive = |text: String| -> Result<HeaderValue, DestinationError> {
        let mut value = HeaderValue::from_str(&text).map_err(|_| unbuildable())?;
        value.set_sensitive(true);
        Ok(value)
    };
    match auth {
        Auth::None => {}
        Auth::Bearer => {
            let token = required("token")?;
            request.headers_mut().insert(
                AUTHORIZATION,
                sensitive(format!("Bearer {}", token.expose()))?,
            );
        }
        Auth::Basic => {
            let user = required("username")?;
            let password = required("password")?;
            let pair = format!("{}:{}", user.expose(), password.expose());
            request.headers_mut().insert(
                AUTHORIZATION,
                sensitive(format!("Basic {}", STANDARD.encode(pair)))?,
            );
        }
        Auth::ApiKey { location, name } => {
            let key = required("api_key")?;
            match location {
                KeyLocation::Header => {
                    request.headers_mut().insert(
                        HeaderName::from_bytes(name.as_bytes()).map_err(|_| unbuildable())?,
                        sensitive(key.expose().to_owned())?,
                    );
                }
                KeyLocation::Query => {
                    request
                        .url_mut()
                        .query_pairs_mut()
                        .append_pair(name, key.expose());
                }
            }
        }
        // A destination manifest with an OAuth2 grant is refused when it is validated.
        Auth::OAuth2ClientCredentials { .. } | Auth::OAuth2AuthorizationCode { .. } => {
            return Err(unbuildable());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::fake::{FakeProvider, FakeProviderOptions, Script};
    use super::*;
    use crate::destinations::error::{Disposition, Refusal};
    use crate::http::HttpLimits;

    const ONE: &str = include_str!("../../../tests/fixtures/providers/bearer_timestamped.json");
    const TWO: &str = include_str!("../../../tests/fixtures/providers/api_key_body_only.json");

    fn config_for(fixture: &str, fake: &FakeProvider) -> Value {
        let text = fixture.replace(
            "https://api.fixture-provider.example.test",
            &fake.base_url(),
        );
        json!({"manifest": serde_json::from_str::<Value>(&text).unwrap()})
    }

    fn ctx(timeout: Duration) -> DestinationContext {
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        DestinationContext::new(
            Arc::new(policy),
            HttpLimits {
                max_redirects: 0,
                timeout,
                ..HttpLimits::default()
            },
            "provider",
            "destination-1",
        )
        .unwrap()
    }

    fn credential(fields: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in fields {
            credential.insert(*key, SecretString::new((*value).to_owned()));
        }
        credential
    }

    fn payload() -> Value {
        json!({
            "from": {"name": "News", "address": "news@example.test"},
            "reply_to": null,
            "subject": "Hello",
            "html": "<p>Hi</p>",
            "text": "Hi",
            "headers": {}
        })
    }

    fn message<'a>(key: &'a str, payload: &'a Value) -> Outbound<'a> {
        Outbound {
            idempotency_key: key,
            channel: "email",
            to: "ada@example.test",
            payload,
            dkim: None,
        }
    }

    async fn deliver(
        config: &Value,
        cred: &Credential,
        key: &str,
        timeout: Duration,
    ) -> Result<Delivered, DestinationError> {
        let payload = payload();
        ProviderDestination
            .deliver(&ctx(timeout), config, cred, &message(key, &payload))
            .await
    }

    fn token() -> Credential {
        credential(&[("token", "tok-1"), ("feedback_secret", "whsec-1")])
    }

    #[test]
    fn the_spec_takes_a_manifest_and_optional_secret_credentials_and_rate_limits_by_bucket() {
        let spec = &ProviderDestination::SPEC;

        assert_eq!(spec.kind, "provider");
        assert_eq!(spec.capabilities.channels, ["email"]);
        assert!(spec.capabilities.idempotency_key && spec.capabilities.rate_limit);
        assert!(!spec.capabilities.signs_dkim);
        assert!(spec.credential_fields.iter().all(|field| field.secret));
        assert!(spec.credential_fields.iter().all(|field| !field.required));
        assert!(spec.config_fields.iter().all(|field| !field.secret));
    }

    #[tokio::test]
    async fn the_request_carries_exactly_one_idempotency_header_with_the_message_id() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);

        deliver(&config, &token(), "msg-1", Duration::from_secs(5))
            .await
            .unwrap();

        let accepted = fake.accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!((accepted[0].key.as_str(), accepted[0].key_headers), ("msg-1", 1));
    }

    #[test]
    fn a_static_header_named_like_the_idempotency_header_is_refused_ignoring_case() {
        let mut manifest = serde_json::from_str::<Value>(ONE).unwrap();
        manifest["headers"] = json!({"idempotency-KEY": "constant"});

        let issues = check_config(&json!({"manifest": manifest}));

        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].field, "manifest/headers/idempotency-KEY");
        assert_eq!(issues[0].reason, "forbidden_header");
    }

    #[test]
    fn the_settings_are_the_manifests_issues_and_a_manifest_needs_its_destination_section() {
        let good = json!({"manifest": serde_json::from_str::<Value>(ONE).unwrap()});
        assert!(check_config(&good).is_empty());

        let mut bad = good.clone();
        bad["manifest"]["destination"]["feedback"]
            .as_object_mut()
            .unwrap()
            .remove("verification");
        assert_eq!(
            check_config(&bad),
            [FieldError {
                field: "manifest/destination/feedback/verification".to_owned(),
                reason: "required".to_owned()
            }]
        );

        let mut source = good.clone();
        source["manifest"]
            .as_object_mut()
            .unwrap()
            .remove("destination");
        source["manifest"]["streams"] = json!([{
            "name": "s", "path": "/s", "fields": [{"name": "id", "type": "string"}],
            "primary_key": ["id"]
        }]);
        assert_eq!(check_config(&source).len(), 1);
        assert!(check_config(&json!({})).is_empty());
    }

    #[test]
    fn a_manifests_rate_limit_is_the_default_rate_of_a_new_destination() {
        let config = json!({"manifest": serde_json::from_str::<Value>(ONE).unwrap()});
        let none = json!({"manifest": serde_json::from_str::<Value>(TWO).unwrap()});

        assert_eq!(default_rate(&config), Some((20, 20)));
        assert_eq!(default_rate(&none), None);
        assert_eq!(default_rate(&json!({})), None);
    }

    #[tokio::test]
    async fn a_message_is_sent_with_the_idempotency_key_the_auth_and_the_mapped_body() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);

        let delivered = deliver(&config, &token(), "msg-1", Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(delivered.status, Some(200));
        let sent = fake.accepted();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].key, "msg-1");
        assert_eq!(sent[0].path, "/v1/messages");
        assert_eq!(sent[0].authorization.as_deref(), Some("Bearer tok-1"));
        assert_eq!(sent[0].body["subject"], "Hello");
        assert_eq!(sent[0].body["from"]["email"], "news@example.test");
        assert_eq!(sent[0].body["to"][0]["email"], "ada@example.test");
        assert_eq!(sent[0].body["custom"]["message_id"], "msg-1");
        assert_eq!(sent[0].body["stream"], "outbound");
    }

    #[tokio::test]
    async fn an_api_key_in_a_header_and_a_body_only_manifest_send_too() {
        let fake = FakeProvider::start(FakeProviderOptions {
            idempotency_header: "X-Request-Id".to_owned(),
            ..FakeProviderOptions::default()
        })
        .await;
        let config = config_for(TWO, &fake);
        let cred = credential(&[("api_key", "key-1"), ("hook_secret", "whsec-2")]);

        deliver(&config, &cred, "msg-2", Duration::from_secs(5))
            .await
            .unwrap();

        let sent = fake.accepted();
        assert_eq!(sent[0].key, "msg-2");
        assert_eq!(sent[0].api_key.as_deref(), Some("key-1"));
        assert_eq!(sent[0].body["message"]["tag"], "msg-2");
    }

    #[tokio::test]
    async fn a_missing_credential_field_is_a_setting_error_naming_the_field_and_nothing_is_sent() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);

        let error = deliver(&config, &credential(&[]), "msg-1", Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_eq!(
            error,
            DestinationError::Config {
                field: "credential.token".to_owned(),
                reason: "is required".to_owned()
            }
        );
        assert_eq!(error.disposition(), Disposition::Terminal);
        assert_eq!(fake.requests(), 0);
    }

    #[tokio::test]
    async fn a_5xx_and_a_timeout_retry_and_a_429_throttles_with_its_retry_after() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);
        fake.script(Script::Status {
            status: 503,
            body: json!({}),
            retry_after: None,
        });
        fake.script(Script::Status {
            status: 429,
            body: json!({}),
            retry_after: Some(7),
        });

        let unavailable = deliver(&config, &token(), "m-1", Duration::from_secs(5))
            .await
            .unwrap_err();
        let limited = deliver(&config, &token(), "m-2", Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_eq!(unavailable.disposition(), Disposition::Retryable);
        assert_eq!(
            limited,
            DestinationError::Answered {
                status: 429,
                retry_after: Some("7".to_owned())
            }
        );
        assert_eq!(
            limited.disposition(),
            Disposition::Throttled { retry_after: None }
        );
        assert!(fake.accepted().is_empty());

        fake.script(Script::HangAfterAccept(Duration::from_secs(3)));
        let timed_out = deliver(&config, &token(), "m-3", Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(timed_out.disposition(), Disposition::Retryable);
    }

    #[tokio::test]
    async fn a_4xx_is_terminal_and_a_recipient_failure_code_is_a_hard_bounce() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);
        fake.script(Script::Status {
            status: 401,
            body: json!({"error": "no"}),
            retry_after: None,
        });
        fake.script(Script::Status {
            status: 422,
            body: json!({"error": {"code": "mailbox_unknown"}}),
            retry_after: None,
        });
        fake.script(Script::Status {
            status: 422,
            body: json!({"error": {"code": "too_large"}}),
            retry_after: None,
        });

        let unauthorized = deliver(&config, &token(), "m-1", Duration::from_secs(5))
            .await
            .unwrap_err();
        let bounced = deliver(&config, &token(), "m-2", Duration::from_secs(5))
            .await
            .unwrap_err();
        let other = deliver(&config, &token(), "m-3", Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_eq!(
            unauthorized,
            DestinationError::Answered {
                status: 401,
                retry_after: None
            }
        );
        assert_eq!(unauthorized.disposition(), Disposition::Terminal);
        assert_eq!(bounced, DestinationError::Refused(Refusal::HardBounce));
        assert_eq!(other.disposition(), Disposition::Terminal);
        assert_ne!(other, DestinationError::Refused(Refusal::HardBounce));
    }

    #[tokio::test]
    async fn a_delivery_retried_after_a_lost_reply_is_one_message_at_a_provider_that_honours_keys()
    {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);
        fake.script(Script::HangAfterAccept(Duration::from_secs(3)));

        let first = deliver(&config, &token(), "msg-1", Duration::from_millis(300)).await;
        let second = deliver(&config, &token(), "msg-1", Duration::from_secs(5)).await;

        assert!(first.is_err(), "the reply was lost");
        assert!(second.is_ok());
        assert_eq!(fake.requests(), 2);
        assert_eq!(fake.accepted().len(), 1, "zero duplicates");
    }

    #[tokio::test]
    async fn the_domain_status_request_asks_for_the_domain_and_maps_the_reply() {
        let fake = FakeProvider::start(FakeProviderOptions {
            domain_status: Some(json!({"records": [
                {"host": "s1._domainkey.news.example.test", "kind": "TXT", "state": "verified"},
                {"host": "news.example.test", "kind": "MX", "state": "pending"}
            ]})),
            ..FakeProviderOptions::default()
        })
        .await;
        let config = config_for(ONE, &fake);

        let records = domain_status(
            &ctx(Duration::from_secs(5)),
            &config,
            &token(),
            "news.example.test",
        )
        .await
        .unwrap();

        assert_eq!(fake.domain_queries(), ["domain=news.example.test"]);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].status, ProviderRecordStatus::Verified);
        assert_eq!(records[1].status, ProviderRecordStatus::Pending);
    }

    #[tokio::test]
    async fn a_manifest_without_a_domain_status_request_does_not_support_it() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(TWO, &fake);

        let result = domain_status(
            &ctx(Duration::from_secs(5)),
            &config,
            &credential(&[("api_key", "k")]),
            "news.example.test",
        )
        .await;

        assert_eq!(result, Err(DestinationError::Unsupported));
        assert_eq!(fake.requests(), 0);
    }

    #[tokio::test]
    async fn the_check_needs_the_auth_and_the_signing_secret_and_sends_nothing() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(ONE, &fake);
        let context = ctx(Duration::from_secs(5));

        assert_eq!(
            ProviderDestination.check(&context, &config, &token()).await,
            Ok(())
        );
        assert!(matches!(
            ProviderDestination
                .check(&context, &config, &credential(&[("token", "t")]))
                .await,
            Err(DestinationError::Config { ref field, .. }) if field == "feedback_secret"
        ));
        assert_eq!(fake.requests(), 0);
    }

    #[test]
    fn feedback_helpers_read_the_manifests_section() {
        let config = json!({"manifest": serde_json::from_str::<Value>(ONE).unwrap()});

        let (spec, verification) = feedback_of(&config).unwrap();

        assert_eq!(verification.secret_field, "feedback_secret");
        assert_eq!(
            feedback_scheme(&verification).timestamp_header,
            Some("X-Fixture-Timestamp")
        );
        let body = json!({"event": "delivered", "meta": {"message_id": "m-1"}});
        assert!(matches!(
            read_feedback(&spec, &body),
            FeedbackRead::Event(Feedback {
                kind: FeedbackKind::Delivered,
                ..
            })
        ));
        assert!(feedback_of(&json!({"manifest": {"version": "v1"}})).is_none());
    }
}
