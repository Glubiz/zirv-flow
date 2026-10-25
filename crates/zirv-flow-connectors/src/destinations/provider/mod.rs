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
mod profile;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{Method, Request};
use serde_json::{Map, Value};
use url::Url;

pub use self::domain::{ProviderRecord, ProviderRecordStatus};
pub use self::feedback::{Feedback, FeedbackKind, FeedbackRead};
use super::context::DestinationContext;
use super::destination::{Delivered, Destination, Outbound, ProfileCall};
use super::error::DestinationError;
use super::spec::{Capabilities, DestinationSpec};
use crate::manifest::{
    Auth, ErrorAction, ErrorRule, FeedbackSpec, KeyLocation, KeySource, Manifest, ProfileMethod,
    ProfileSpec, Provider, SendMethod, VerificationSpec, validate,
};
use crate::sources::{Credential, FieldError, FieldKind, FieldSpec};
use zirv_flow_core::flow::SyncOperation;

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
            channels: &["email", "profile_sync"],
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
        if let Some(feedback) = &provider.feedback {
            let field = &feedback.verification.secret_field;
            if credential.get(field).is_none() {
                return Err(missing(field));
            }
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
        // A manifest of profile operations only has no email to send.
        let send = provider
            .send
            .as_ref()
            .ok_or_else(|| missing("manifest/destination/send"))?;
        let body = body::build(send, message.payload, message.to, message.idempotency_key)?;
        let url = joined(&manifest.base_url, &send.path)?;
        let method = match send.method {
            SendMethod::Post => Method::POST,
            SendMethod::Put => Method::PUT,
            SendMethod::Patch => Method::PATCH,
        };
        let mut request = prepared(
            method,
            url,
            &manifest,
            (&send.idempotency_header, message.idempotency_key),
            credential,
        )?;
        add_headers(&mut request, [(CONTENT_TYPE.as_str(), "application/json")])?;
        *request.body_mut() = Some(body.to_string().into_bytes().into());

        // The body of a success is dropped unread.
        respond(ctx, request, &provider.errors).await
    }

    async fn sync(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        call: &ProfileCall<'_>,
    ) -> Result<Delivered, DestinationError> {
        let (manifest, provider) = provider_of(config)?;
        let spec = match call.operation {
            SyncOperation::Upsert => provider.upsert_profile.as_ref(),
            SyncOperation::Remove => provider.remove_profile.as_ref(),
        }
        .ok_or(DestinationError::Unsupported)?;
        run_profile(
            ctx,
            (&manifest, &provider, spec),
            credential,
            (call.key, call.traits),
            call.idempotency_key,
        )
        .await
    }

    async fn erase(
        &self,
        ctx: &DestinationContext,
        config: &Value,
        credential: &Credential,
        subject: &str,
        idempotency_key: &str,
    ) -> Result<(), DestinationError> {
        let (manifest, provider) = provider_of(config)?;
        let spec = provider
            .erase_profile
            .as_ref()
            .ok_or(DestinationError::Unsupported)?;
        run_profile(
            ctx,
            (&manifest, &provider, spec),
            credential,
            (subject, &Map::new()),
            idempotency_key,
        )
        .await
        .map(|_| ())
    }
}

/// Runs one profile operation of the manifest: the request of `spec` with `key` and `traits`
/// placed, sent once. Shared by `sync` and `erase`.
async fn run_profile(
    ctx: &DestinationContext,
    (manifest, provider, spec): (&Manifest, &Provider, &ProfileSpec),
    credential: &Credential,
    (key, traits): (&str, &Map<String, Value>),
    idempotency_key: &str,
) -> Result<Delivered, DestinationError> {
    if key.is_empty() || key.len() > profile::MAX_KEY_BYTES {
        return Err(DestinationError::Config {
            field: "key".to_owned(),
            reason: "is not usable".to_owned(),
        });
    }
    // The URL parser reads `.` and `..` (also as `%2e`) as dot segments and would move the
    // request off the mapped path. The encoder percent-encodes `%`, so no other spelling of a
    // key reaches the path as a dot segment: matching the raw key is enough.
    if profile::is_dot_segment(key) {
        return Err(DestinationError::Config {
            field: "key".to_owned(),
            reason: "is a path dot segment".to_owned(),
        });
    }
    let body = profile::build_body(spec, key, traits)?;
    let url = joined(&manifest.base_url, &profile::path_for(spec, key))?;
    let method = match spec.method {
        ProfileMethod::Post => Method::POST,
        ProfileMethod::Put => Method::PUT,
        ProfileMethod::Patch => Method::PATCH,
        ProfileMethod::Delete => Method::DELETE,
    };
    let mut request = prepared(
        method,
        url,
        manifest,
        (&spec.idempotency_header, idempotency_key),
        credential,
    )?;
    if let Some(body) = body {
        request
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        *request.body_mut() = Some(body.to_string().into_bytes().into());
    }

    // The operation's own rules come first, then the destination's. A profile has no
    // recipient address: a rule that says so ends the operation, as any terminal answer does.
    let rules: Vec<ErrorRule> = spec
        .errors
        .iter()
        .chain(&provider.errors)
        .map(|rule| ErrorRule {
            action: match rule.action {
                ErrorAction::RecipientFailure => ErrorAction::Terminal,
                other => other,
            },
            ..rule.clone()
        })
        .collect();
    respond(ctx, request, &rules).await
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
    let feedback = provider.feedback?;
    let verification = feedback.verification.clone();
    Some((feedback, verification))
}

/// The profile operations a destination's manifest declares, and whether it also carries email.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProfileSupport {
    pub upsert: bool,
    pub remove: bool,
    /// Whether the manifest declares the email pair (`send` with `feedback`).
    pub email: bool,
}

impl ProfileSupport {
    /// Whether the operation is declared.
    pub fn supports(self, operation: SyncOperation) -> bool {
        match operation {
            SyncOperation::Upsert => self.upsert,
            SyncOperation::Remove => self.remove,
        }
    }
}

/// What the manifest of a provider destination's `config` declares. Nothing for a configuration
/// that is not a valid provider manifest.
pub fn profile_support(config: &Value) -> ProfileSupport {
    match provider_of(config) {
        Ok((_, provider)) => ProfileSupport {
            upsert: provider.upsert_profile.is_some(),
            remove: provider.remove_profile.is_some(),
            email: provider.send.is_some(),
        },
        Err(_) => ProfileSupport::default(),
    }
}

/// Where the external key of `operation` comes from, per the manifest of `config`.
pub fn profile_key_source(config: &Value, operation: SyncOperation) -> Option<KeySource> {
    let (_, provider) = provider_of(config).ok()?;
    match operation {
        SyncOperation::Upsert => provider.upsert_profile,
        SyncOperation::Remove => provider.remove_profile,
    }
    .map(|spec| spec.key_from)
}

/// Where the external key of the erase call comes from, per the manifest of `config`. `None` when
/// the manifest declares no `erase_profile`, which is how a provider destination declares the
/// erase capability.
pub fn erase_key_source(config: &Value) -> Option<KeySource> {
    let (_, provider) = provider_of(config).ok()?;
    provider.erase_profile.map(|spec| spec.key_from)
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
    // The destination's own limit, else the strictest an operation declares.
    let limit = provider.rate_limit.or_else(|| {
        [
            &provider.upsert_profile,
            &provider.remove_profile,
            &provider.erase_profile,
        ]
        .into_iter()
        .flatten()
        .filter_map(|spec| spec.rate_limit)
        .min_by(|a, b| {
            (u64::from(a.requests) * u64::from(b.per_seconds))
                .cmp(&(u64::from(b.requests) * u64::from(a.per_seconds)))
        })
    })?;
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
        return Err(failure(&provider.errors, response).await);
    }
    let bytes = response.body().await.map_err(|_| {
        DestinationError::Other("the provider's reply could not be read".to_owned())
    })?;
    let reply: Value = serde_json::from_slice(&bytes)
        .map_err(|_| DestinationError::Other("the provider's reply is not JSON".to_owned()))?;
    Ok(domain::map(spec, &reply))
}

/// The error of a response that was not a success.
async fn failure(rules: &[ErrorRule], response: crate::http::GuardedResponse) -> DestinationError {
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
    let action = classify::classify(rules, status, body.as_ref());
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

/// A request to `url` with the manifest's headers, the idempotency header (which replaces any
/// header of the same name: the request carries exactly one value) and the manifest's auth. The
/// caller places the body.
fn prepared(
    method: Method,
    url: Url,
    manifest: &Manifest,
    (idempotency_header, idempotency_key): (&str, &str),
    credential: &Credential,
) -> Result<Request, DestinationError> {
    let mut request = Request::new(method, url);
    add_headers(
        &mut request,
        manifest
            .headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    )?;
    request.headers_mut().insert(
        HeaderName::from_bytes(idempotency_header.as_bytes()).map_err(|_| unbuildable())?,
        HeaderValue::from_str(idempotency_key).map_err(|_| unbuildable())?,
    );
    apply_auth(&mut request, &manifest.auth, credential)?;
    Ok(request)
}

/// Sends `request`: a 2xx is delivered (its body dropped unread), anything else is classified by
/// `rules`.
async fn respond(
    ctx: &DestinationContext,
    request: Request,
    rules: &[ErrorRule],
) -> Result<Delivered, DestinationError> {
    let response = ctx.http().execute(request).await?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(Delivered {
            status: Some(status),
            remote_id: None,
        });
    }
    Err(failure(rules, response).await)
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
        assert_eq!(spec.capabilities.channels, ["email", "profile_sync"]);
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
        assert_eq!(
            (accepted[0].key.as_str(), accepted[0].key_headers),
            ("msg-1", 1)
        );
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

    const PROFILES: &str = include_str!("../../../tests/fixtures/providers/profile_sync.json");

    async fn sync(
        config: &Value,
        operation: SyncOperation,
        key: &str,
        traits: &serde_json::Map<String, Value>,
        idempotency_key: &str,
    ) -> Result<Delivered, DestinationError> {
        let call = ProfileCall {
            operation,
            key,
            traits,
            idempotency_key,
        };
        ProviderDestination
            .sync(
                &ctx(Duration::from_secs(5)),
                config,
                &credential(&[("token", "tok-1")]),
                &call,
            )
            .await
    }

    fn traits(value: Value) -> serde_json::Map<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn an_upsert_and_a_remove_call_the_mapped_requests_with_the_key_in_one_path_segment() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(PROFILES, &fake);

        sync(
            &config,
            SyncOperation::Upsert,
            "a/b?c#d@x.test",
            &traits(json!({"plan": "pro", "age": null})),
            "effect-1",
        )
        .await
        .unwrap();
        sync(
            &config,
            SyncOperation::Remove,
            "a/b?c#d@x.test",
            &traits(json!({})),
            "effect-2",
        )
        .await
        .unwrap();

        let accepted = fake.accepted();
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].method, "PUT");
        assert_eq!(accepted[0].path, "/v1/profiles/a%2Fb%3Fc%23d%40x.test");
        assert_eq!(
            accepted[0].body,
            json!({
                "source": "zirv-flow",
                "profile": {"id": "a/b?c#d@x.test", "attributes": {"plan": "pro", "age": null}}
            })
        );
        assert_eq!(accepted[0].authorization.as_deref(), Some("Bearer tok-1"));
        assert_eq!(
            (accepted[1].method.as_str(), accepted[1].key.as_str()),
            ("DELETE", "effect-2")
        );
        assert_eq!(accepted[1].path, accepted[0].path);
        assert_eq!(
            accepted[1].body,
            Value::Null,
            "a remove with nothing to place has no body"
        );
        assert_eq!(accepted[0].key_headers, 1);
    }

    #[tokio::test]
    async fn a_retried_sync_under_the_same_key_is_one_change_at_a_key_honouring_provider() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(PROFILES, &fake);
        let plan = traits(json!({"plan": "pro"}));

        for _ in 0..3 {
            sync(&config, SyncOperation::Upsert, "k", &plan, "effect-1")
                .await
                .unwrap();
        }

        assert_eq!(fake.accepted().len(), 1);
        assert_eq!(fake.requests(), 3);
    }

    #[tokio::test]
    async fn a_sync_failure_is_classified_by_the_operation_rules_then_the_defaults() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(PROFILES, &fake);
        let none = traits(json!({}));
        for (status, retry_after) in [(409, None), (503, None), (429, Some(7)), (422, None)] {
            fake.script(Script::Status {
                status,
                body: json!({}),
                retry_after,
            });
        }

        let mut dispositions = Vec::new();
        for n in 0..4 {
            let error = sync(
                &config,
                SyncOperation::Upsert,
                "k",
                &none,
                &format!("e-{n}"),
            )
            .await
            .unwrap_err();
            dispositions.push(error.disposition());
        }

        assert_eq!(
            dispositions,
            [
                Disposition::Retryable,
                Disposition::Retryable,
                Disposition::Throttled { retry_after: None },
                Disposition::Terminal
            ]
        );
        assert!(fake.accepted().is_empty());
    }

    #[tokio::test]
    async fn an_operation_the_manifest_lacks_or_a_bad_key_makes_no_request() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let mut manifest = serde_json::from_str::<Value>(PROFILES).unwrap();
        manifest["destination"]
            .as_object_mut()
            .unwrap()
            .remove("remove_profile");
        let text = manifest.to_string().replace(
            "https://api.fixture-provider.example.test",
            &fake.base_url(),
        );
        let config = json!({"manifest": serde_json::from_str::<Value>(&text).unwrap()});
        let none = traits(json!({}));

        let remove = sync(&config, SyncOperation::Remove, "k", &none, "e-1").await;
        let empty = sync(&config, SyncOperation::Upsert, "", &none, "e-2").await;
        let long = sync(
            &config,
            SyncOperation::Upsert,
            &"k".repeat(600),
            &none,
            "e-3",
        )
        .await;

        assert_eq!(remove, Err(DestinationError::Unsupported));
        assert!(matches!(empty, Err(DestinationError::Config { .. })));
        assert!(matches!(long, Err(DestinationError::Config { .. })));
        assert_eq!(fake.requests(), 0);
    }

    /// The profile fixture with an `erase_profile` that places the key and a literal.
    fn config_with_erase(fake: &FakeProvider) -> Value {
        let mut config = config_for(PROFILES, fake);
        config["manifest"]["destination"]["erase_profile"] = json!({
            "method": "POST",
            "path": "/v1/erasures",
            "idempotency_header": "Idempotency-Key",
            "key_from": {"identity": "email"},
            "body": {"key": "/subject", "static": {"/reason": "erasure"}}
        });
        config
    }

    async fn erase(
        config: &Value,
        subject: &str,
        idempotency_key: &str,
    ) -> Result<(), DestinationError> {
        ProviderDestination
            .erase(
                &ctx(Duration::from_secs(5)),
                config,
                &credential(&[("token", "tok-1")]),
                subject,
                idempotency_key,
            )
            .await
    }

    #[tokio::test]
    async fn an_erase_calls_the_mapped_request_with_the_key_and_statics_and_no_traits() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_with_erase(&fake);

        erase(&config, "ada@x.test", "erase:1").await.unwrap();

        let accepted = fake.accepted();
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            (accepted[0].method.as_str(), accepted[0].path.as_str()),
            ("POST", "/v1/erasures")
        );
        assert_eq!(accepted[0].key, "erase:1");
        assert_eq!(
            accepted[0].body,
            json!({"subject": "ada@x.test", "reason": "erasure"})
        );
        assert_eq!(accepted[0].authorization.as_deref(), Some("Bearer tok-1"));
    }

    #[tokio::test]
    async fn a_retried_erase_under_the_same_key_is_one_change_at_a_key_honouring_provider() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_with_erase(&fake);

        for _ in 0..3 {
            erase(&config, "k", "erase:1").await.unwrap();
        }

        assert_eq!(fake.accepted().len(), 1);
        assert_eq!(fake.requests(), 3);
    }

    #[tokio::test]
    async fn a_manifest_without_an_erase_profile_does_not_erase_and_a_bad_key_makes_no_request() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let plain = config_for(PROFILES, &fake);
        let config = config_with_erase(&fake);

        let none = erase(&plain, "k", "erase:1").await;
        let empty = erase(&config, "", "erase:2").await;
        let dot = erase(&config, "..", "erase:3").await;

        assert_eq!(none, Err(DestinationError::Unsupported));
        assert!(matches!(empty, Err(DestinationError::Config { .. })));
        assert!(matches!(dot, Err(DestinationError::Config { .. })));
        assert_eq!(fake.requests(), 0);
        assert!(erase_key_source(&plain).is_none());
        assert_eq!(
            erase_key_source(&config),
            Some(KeySource::Identity("email".to_owned()))
        );
    }

    #[tokio::test]
    async fn an_erase_failure_is_classified_like_a_sync_failure() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_with_erase(&fake);
        fake.script(Script::Status {
            status: 503,
            body: json!({}),
            retry_after: None,
        });
        fake.script(Script::Status {
            status: 422,
            body: json!({}),
            retry_after: None,
        });

        let retry = erase(&config, "k", "erase:1").await.unwrap_err();
        let terminal = erase(&config, "k", "erase:2").await.unwrap_err();

        assert_eq!(retry.disposition(), Disposition::Retryable);
        assert_eq!(terminal.disposition(), Disposition::Terminal);
    }

    #[tokio::test]
    async fn a_dot_segment_key_is_refused_without_a_call() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(PROFILES, &fake);
        let none = traits(json!({}));

        for key in [".", ".."] {
            for operation in [SyncOperation::Upsert, SyncOperation::Remove] {
                match sync(&config, operation, key, &none, "e-1").await {
                    Err(error @ DestinationError::Config { .. }) => {
                        assert_eq!(error.disposition(), Disposition::Terminal);
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
        assert_eq!(fake.requests(), 0);
    }

    #[tokio::test]
    async fn a_profile_only_manifest_sends_no_email_and_has_no_feedback() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let config = config_for(PROFILES, &fake);

        let sent = deliver(&config, &token(), "m-1", Duration::from_secs(5)).await;

        assert!(matches!(sent, Err(DestinationError::Config { .. })));
        assert!(feedback_of(&config).is_none());
        assert_eq!(fake.requests(), 0);
        ProviderDestination
            .check(
                &ctx(Duration::from_secs(5)),
                &config,
                &credential(&[("token", "t")]),
            )
            .await
            .unwrap();
    }

    #[test]
    fn the_manifest_says_which_operations_exist_where_the_key_comes_from_and_the_default_rate() {
        let profiles = json!({"manifest": serde_json::from_str::<Value>(PROFILES).unwrap()});
        let email = json!({"manifest": serde_json::from_str::<Value>(ONE).unwrap()});

        let support = profile_support(&profiles);
        assert_eq!(
            support,
            ProfileSupport {
                upsert: true,
                remove: true,
                email: false
            }
        );
        assert!(support.supports(SyncOperation::Remove));
        assert_eq!(
            profile_support(&email),
            ProfileSupport {
                upsert: false,
                remove: false,
                email: true
            }
        );
        assert_eq!(profile_support(&json!({})), ProfileSupport::default());
        assert_eq!(
            profile_key_source(&profiles, SyncOperation::Upsert),
            Some(KeySource::Identity("email".to_owned()))
        );
        assert_eq!(profile_key_source(&email, SyncOperation::Upsert), None);
        assert_eq!(default_rate(&profiles), Some((10, 10)));
    }

    #[test]
    fn the_default_rate_counts_an_erase_profiles_limit_and_an_erase_only_manifest_has_one() {
        let erase = |limit: Option<Value>| {
            let mut manifest = serde_json::from_str::<Value>(PROFILES).unwrap();
            manifest["destination"] = json!({"erase_profile": {
                "path": "/erasures/{key}",
                "idempotency_header": "Idempotency-Key",
                "key_from": {"identity": "email"}
            }});
            if let Some(limit) = limit {
                manifest["destination"]["erase_profile"]["rate_limit"] = limit;
            }
            json!({"manifest": manifest})
        };
        let strictest = serde_json::from_str::<Value>(PROFILES).unwrap();
        let mut both = strictest;
        both["destination"]["erase_profile"] = json!({
            "path": "/erasures/{key}",
            "idempotency_header": "Idempotency-Key",
            "key_from": {"identity": "email"},
            "rate_limit": {"requests": 2, "per_seconds": 1}
        });

        assert_eq!(
            default_rate(&erase(Some(json!({"requests": 4, "per_seconds": 2})))),
            Some((2, 4))
        );
        assert_eq!(default_rate(&erase(None)), None);
        assert_eq!(
            default_rate(&json!({"manifest": both})),
            Some((2, 2)),
            "the erase limit is stricter than the upsert's 10 per second"
        );
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
