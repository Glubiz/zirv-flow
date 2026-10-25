//! The `http` source: any HTTP API a valid manifest v1 describes (architecture 7.2). The manifest
//! is JSON data stored as the source's `config.manifest`; this module interprets it, nothing
//! more. There is no expression language and no scripting.
//!
//! Every request goes through [`SourceContext::send`] (via the runtime's pager), so auth, retries,
//! the rate limit, the origin binding and the egress guard are the runtime's. Errors are static
//! codes and statuses: none holds a URL, a header, a response body or a record value.

mod auth;
mod checkpoint;
mod request;

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::Value;
use zirv_flow_core::mapping::resolve_path;

use self::checkpoint::{Highest, checkpoint, since_of};
use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::record_key::key_of;
use super::source::Source;
use super::spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
use super::types::{
    Batch, Checkpoint, FieldDescriptor, Record, StreamConfig, StreamDescriptor, SyncMode,
};
use super::validate::FieldError;
use crate::manifest::{Manifest, StreamSpec, validate};
use crate::runtime::{Pager, RateLimit};

pub use self::auth::{REFRESH_TOKEN_KEY, active_binding, seed_refresh_token};

pub(crate) struct Http;

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

impl Source for Http {
    const SPEC: SourceSpec = SourceSpec {
        kind: "http",
        label: "HTTP API (manifest)",
        description: "Any HTTP API, described by a manifest: its address, how to authenticate, how it pages, which field is the cursor and how its fields map to people.",
        config_fields: &[FieldSpec {
            key: "manifest",
            label: "Manifest",
            help: "A JSON description of the API (manifest v1). It holds no secret: the credential fields below hold those.",
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
            secret(
                "client_id",
                "Client ID",
                "For an OAuth2 manifest: the client the provider registered.",
            ),
            secret(
                "client_secret",
                "Client secret",
                "For an OAuth2 manifest. A public client with PKCE may leave it empty.",
            ),
        ],
        rate_limit: None,
        config_check: Some(check_config),
        capabilities: Capabilities {
            incremental: true,
            full_refresh: true,
            emits_deletes: true,
            push: false,
        },
    };

    fn rate_limit(&self, config: &Value) -> Option<RateLimit> {
        let limit = manifest_of(config).ok()?.rate_limit?;
        Some(RateLimit {
            requests: limit.requests,
            per: Duration::from_secs(u64::from(limit.per_seconds)),
        })
    }

    async fn check(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        let manifest = manifest_of(config)?;
        let stream = &manifest.streams[0];
        let auth = auth::build(&manifest.auth, credential)?;
        let first = request::first(&manifest, stream, None)?;
        // The first page is requested and dropped; the runtime bounds its size.
        ctx.send(&auth, first).await?;
        Ok(())
    }

    async fn discover(
        &self,
        _ctx: &SourceContext,
        config: &Value,
        _credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        // The manifest declares the streams and their fields: no request is made.
        let manifest = manifest_of(config)?;
        Ok(manifest.streams.iter().map(descriptor).collect())
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        match Reader::new(config, credential, stream, checkpoint.as_ref()) {
            Ok(reader) => stream::unfold(Phase::Reading(Box::new(reader)), move |phase| {
                step(ctx, credential, phase)
            })
            .boxed(),
            Err(error) => stream::once(async move { Err(error) }).boxed(),
        }
    }
}

/// The settings problems of `config`, as the manifest's issues: `manifest<pointer>` and the
/// issue code. Pure; `validate_config` runs it for an `http` source.
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
            // A provider manifest sends mail; a source reads streams.
            Ok(parsed) if parsed.provider.is_some() => vec![FieldError {
                field: "manifest/destination".to_owned(),
                reason: "is for a provider destination, not a source".to_owned(),
            }],
            Ok(_) => Vec::new(),
        },
    }
}

fn manifest_of(config: &Value) -> Result<Manifest, SourceError> {
    let value = config.get("manifest").ok_or_else(|| SourceError::Config {
        field: "manifest".to_owned(),
        reason: "is required".to_owned(),
    })?;
    let manifest = validate(value).map_err(|issues| {
        let first = issues.first();
        SourceError::Config {
            field: format!("manifest{}", first.map_or("", |issue| issue.path.as_str())),
            reason: format!(
                "is invalid ({})",
                first.map_or("unknown", |issue| issue.code)
            ),
        }
    })?;
    if manifest.provider.is_some() {
        return Err(SourceError::Config {
            field: "manifest/destination".to_owned(),
            reason: "is for a provider destination, not a source".to_owned(),
        });
    }
    Ok(manifest)
}

fn descriptor(stream: &StreamSpec) -> StreamDescriptor {
    let mut sync_modes = vec![SyncMode::FullRefresh];
    if stream.incremental.is_some() {
        sync_modes.push(SyncMode::Incremental);
    }
    StreamDescriptor {
        name: stream.name.clone(),
        fields: stream
            .fields
            .iter()
            .map(|field| FieldDescriptor {
                name: field.name.clone(),
                field_type: field.field_type,
            })
            .collect(),
        sync_modes,
        default_cursor_field: stream
            .incremental
            .as_ref()
            .map(|incremental| incremental.cursor_field.clone()),
        default_primary_key: stream.primary_key.clone(),
        suggested_mapping: stream
            .mapping
            .as_ref()
            .and_then(|mapping| serde_json::to_value(mapping).ok()),
    }
}

/// One stream read in progress. It owns the manifest and the pager, borrows nothing between
/// pages, and builds the runtime auth and style anew for each page.
struct Reader {
    manifest: Manifest,
    index: usize,
    pager: Pager,
    /// The checkpoint the run started with: what every batch but the last carries.
    start: Option<String>,
    /// Tracks the highest cursor value; present only for an incremental read.
    highest: Option<Highest>,
    /// The batch read last, held back so the final one can be told from the rest.
    pending: Option<Vec<Record>>,
}

enum Phase {
    Reading(Box<Reader>),
    /// An error to report once the batch read before it has been handed over.
    Failed(SourceError),
    Done,
}

impl Reader {
    fn new(
        config: &Value,
        credential: &Credential,
        stream: &StreamConfig,
        checkpoint: Option<&Checkpoint>,
    ) -> Result<Self, SourceError> {
        let manifest = manifest_of(config)?;
        let index = manifest
            .streams
            .iter()
            .position(|candidate| candidate.name == stream.name)
            .ok_or_else(|| SourceError::Config {
                field: "stream".to_owned(),
                reason: "is not in the manifest".to_owned(),
            })?;
        // The credential is checked before any request.
        auth::build(&manifest.auth, credential)?;
        let spec = &manifest.streams[index];
        let incremental = match (&spec.incremental, stream.sync_mode) {
            (Some(incremental), SyncMode::Incremental) => Some(incremental),
            _ => None,
        };
        let start = incremental.and(since_of(checkpoint));
        let highest =
            incremental.map(|incremental| Highest::new(incremental.cursor_type, start.as_deref()));
        let first = request::first(&manifest, spec, start.as_deref())?;
        let pager = Pager::new(first, request::pagination(spec));
        Ok(Self {
            manifest,
            index,
            pager,
            start,
            highest,
            pending: None,
        })
    }

    /// The records of the next page, or `None` after the last.
    async fn next_page(
        &mut self,
        ctx: &SourceContext,
        credential: &Credential,
    ) -> Option<Result<Vec<Record>, SourceError>> {
        let stream = &self.manifest.streams[self.index];
        let auth = match auth::build(&self.manifest.auth, credential) {
            Ok(auth) => auth,
            Err(error) => return Some(Err(error)),
        };
        let page = self
            .pager
            .next_page(
                ctx,
                &auth,
                request::pagination(stream),
                &stream.records_path,
            )
            .await?;
        Some(match page {
            Ok(page) => records(stream, self.highest.as_mut(), page.items),
            Err(error) => Err(error.into()),
        })
    }

    fn batch(&self, records: Vec<Record>, last: bool) -> Batch {
        let since = match (&self.highest, last) {
            (Some(highest), true) => highest.text().or(self.start.as_deref()),
            _ => self.start.as_deref(),
        };
        Batch {
            records,
            checkpoint: checkpoint(since),
            refused: Vec::new(),
        }
    }
}

async fn step(
    ctx: &SourceContext,
    credential: &Credential,
    phase: Phase,
) -> Option<(Result<Batch, SourceError>, Phase)> {
    let mut reader = match phase {
        Phase::Done => return None,
        Phase::Failed(error) => return Some((Err(error), Phase::Done)),
        Phase::Reading(reader) => reader,
    };
    loop {
        match reader.next_page(ctx, credential).await {
            Some(Ok(page)) => {
                if let Some(previous) = reader.pending.replace(page) {
                    let batch = reader.batch(previous, false);
                    return Some((Ok(batch), Phase::Reading(reader)));
                }
            }
            Some(Err(error)) => {
                return Some(match reader.pending.take() {
                    Some(previous) => (Ok(reader.batch(previous, false)), Phase::Failed(error)),
                    None => (Err(error), Phase::Done),
                });
            }
            None => {
                // A stream with no records still ends with one batch, so the run finishes.
                let last = reader.pending.take().unwrap_or_default();
                return Some((Ok(reader.batch(last, true)), Phase::Done));
            }
        }
    }
}

fn other(code: &str) -> SourceError {
    SourceError::Other(code.to_owned())
}

/// The records of one page: each an object with a key, flagged deleted by the delete signal.
fn records(
    stream: &StreamSpec,
    mut highest: Option<&mut Highest>,
    items: Vec<Value>,
) -> Result<Vec<Record>, SourceError> {
    let mut records = Vec::with_capacity(items.len());
    for item in items {
        let Some(object) = item.as_object() else {
            return Err(other("record_not_object"));
        };
        let key = key_of(&stream.primary_key, object).ok_or_else(|| other("record_key_missing"))?;
        if let (Some(highest), Some(incremental)) = (highest.as_deref_mut(), &stream.incremental) {
            highest.observe(object.get(&incremental.cursor_field));
        }
        let deleted = stream
            .delete_signal
            .as_ref()
            .is_some_and(|signal| resolve_path(&item, &signal.path) == Some(&signal.equals));
        records.push(Record {
            key,
            data: item,
            deleted,
            occurrence: None,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use hashbrown::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::{RawQuery, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::*;
    use crate::http::HttpLimits;
    use crate::runtime::testing::{Mock, context, serve};
    use crate::sources::{Registry, validate_config};

    const CURSOR: &str = include_str!("../../../tests/fixtures/manifests/cursor_oauth2_code.json");
    const OFFSET: &str = include_str!("../../../tests/fixtures/manifests/offset_api_key.json");
    const PAGE: &str =
        include_str!("../../../tests/fixtures/manifests/page_basic_soft_delete.json");
    const LINK: &str = include_str!("../../../tests/fixtures/manifests/link_header_bearer.json");

    /// A reference manifest pointed at the mock: its API and its identity provider are one server.
    fn config_for(fixture: &str, mock: &Mock) -> Value {
        let base = format!("http://127.0.0.1:{}", mock.port);
        let text = fixture
            .replace("https://api.example.test", &base)
            .replace("https://auth.example.test", &base);
        json!({"manifest": serde_json::from_str::<Value>(&text).unwrap()})
    }

    fn credential(fields: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in fields {
            credential.insert(*key, SecretString::new((*value).to_owned()));
        }
        credential
    }

    fn stream_config(name: &str, mode: SyncMode) -> StreamConfig {
        StreamConfig {
            name: name.to_owned(),
            sync_mode: mode,
            cursor_field: None,
            primary_key: Vec::new(),
        }
    }

    async fn read_all(
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
        stream: &StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> Vec<Result<Batch, SourceError>> {
        Source::read(&Http, ctx, config, credential, stream, checkpoint)
            .collect()
            .await
    }

    fn ok(results: Vec<Result<Batch, SourceError>>) -> Vec<Batch> {
        results.into_iter().map(Result::unwrap).collect()
    }

    fn keys(batches: &[Batch]) -> Vec<String> {
        batches
            .iter()
            .flat_map(|batch| batch.records.iter().map(|record| record.key.clone()))
            .collect()
    }

    fn since(batch: &Batch) -> Option<&str> {
        batch.checkpoint.0["since"].as_str()
    }

    // ---- A mock API with cursor pagination, OAuth2 refresh and a 429 -----------------------

    struct ApiState {
        records: Mutex<Vec<Value>>,
        /// The access token the API accepts.
        valid_access: Mutex<String>,
        /// The refresh token the identity provider accepts; a reused one revokes everything.
        valid_refresh: Mutex<String>,
        refreshes: AtomicUsize,
        /// Whether the next request for a second page answers 429 with `Retry-After`.
        rate_limit_once: Mutex<bool>,
        /// After this many data requests the API stops accepting the token it has issued.
        revoke_at: AtomicUsize,
        data_requests: AtomicUsize,
        queries: Mutex<Vec<String>>,
        authorizations: Mutex<Vec<String>>,
    }

    fn contact(id: u32, status: &str) -> Value {
        json!({
            "id": format!("c{id}"),
            "email": format!("person{id}@example.com"),
            "updated_at": format!("2026-01-{id:02}T00:00:00Z"),
            "status": status,
        })
    }

    impl ApiState {
        fn new(records: Vec<Value>) -> Arc<Self> {
            Arc::new(Self {
                records: Mutex::new(records),
                valid_access: Mutex::new(String::new()),
                valid_refresh: Mutex::new("rt-0".to_owned()),
                refreshes: AtomicUsize::new(0),
                rate_limit_once: Mutex::new(false),
                revoke_at: AtomicUsize::new(0),
                data_requests: AtomicUsize::new(0),
                queries: Mutex::default(),
                authorizations: Mutex::default(),
            })
        }
    }

    fn query_map(query: &Option<String>) -> HashMap<String, String> {
        url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
            .into_owned()
            .collect()
    }

    async fn contacts(
        State(api): State<Arc<ApiState>>,
        headers: HeaderMap,
        RawQuery(query): RawQuery,
    ) -> axum::response::Response {
        let n = api.data_requests.fetch_add(1, Ordering::SeqCst) + 1;
        api.queries
            .lock()
            .unwrap()
            .push(query.clone().unwrap_or_default());
        let revoke_at = api.revoke_at.load(Ordering::SeqCst);
        if revoke_at != 0 && n == revoke_at {
            *api.valid_access.lock().unwrap() = "revoked".to_owned();
        }
        let sent = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        api.authorizations.lock().unwrap().push(sent.clone());
        if sent != format!("Bearer {}", api.valid_access.lock().unwrap()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let params = query_map(&query);
        if params.contains_key("cursor")
            && std::mem::take(&mut *api.rate_limit_once.lock().unwrap())
        {
            let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("1"));
            return response;
        }
        let records: Vec<Value> = api
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| {
                params.get("since").is_none_or(|since| {
                    record["updated_at"].as_str().unwrap_or_default() > since.as_str()
                })
            })
            .cloned()
            .collect();
        let size: usize = params["limit"].parse().unwrap();
        let start: usize = params
            .get("cursor")
            .map_or(0, |cursor| cursor.parse().unwrap());
        let page: Vec<Value> = records.iter().skip(start).take(size).cloned().collect();
        let next = (start + size < records.len()).then(|| (start + size).to_string());
        axum::Json(json!({"data": page, "next_cursor": next})).into_response()
    }

    async fn token(State(api): State<Arc<ApiState>>, body: String) -> axum::response::Response {
        let form = query_map(&Some(body));
        let mut valid = api.valid_refresh.lock().unwrap();
        if form.get("grant_type").map(String::as_str) != Some("refresh_token")
            || form.get("refresh_token") != Some(&*valid)
        {
            *valid = "revoked".to_owned();
            return (StatusCode::BAD_REQUEST, "no").into_response();
        }
        let n = api.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        *valid = format!("rt-{n}");
        *api.valid_access.lock().unwrap() = format!("acc-{n}");
        axum::Json(json!({
            "access_token": format!("acc-{n}"),
            "refresh_token": format!("rt-{n}"),
            "expires_in": 3600,
        }))
        .into_response()
    }

    async fn api(state: &Arc<ApiState>) -> Mock {
        serve(
            Router::new()
                .route("/v1/contacts", get(contacts))
                .route("/token", post(token))
                .with_state(Arc::clone(state)),
        )
        .await
    }

    fn oauth_credential(refresh: &str) -> Credential {
        credential(&[
            ("client_id", "client"),
            ("client_secret", "shh"),
            (REFRESH_TOKEN_KEY, refresh),
        ])
    }

    #[tokio::test]
    async fn a_mock_api_with_cursor_paging_oauth_refresh_and_a_429_syncs_incrementally() {
        // Acceptance 7, the exit criterion of M3.
        let state = ApiState::new(vec![
            contact(1, "active"),
            contact(2, "active"),
            contact(4, "active"),
            contact(3, "active"),
            contact(5, "active"),
        ]);
        *state.rate_limit_once.lock().unwrap() = true;
        let mock = api(&state).await;
        let config = config_for(CURSOR, &mock);
        let stream = stream_config("contacts", SyncMode::Incremental);

        // Run 1: all pages, the token refreshed once, the 429 waited out.
        let ctx = context("http", "src-1");
        let started = std::time::Instant::now();
        let first = ok(read_all(&ctx, &config, &oauth_credential("rt-0"), &stream, None).await);
        assert!(
            started.elapsed() >= Duration::from_millis(900),
            "the Retry-After of one second was waited out"
        );
        assert_eq!(keys(&first), ["c1", "c2", "c4", "c3", "c5"]);
        assert_eq!(state.refreshes.load(Ordering::SeqCst), 1, "refreshed once");
        // The 429 was one extra request for the second page.
        assert_eq!(state.data_requests.load(Ordering::SeqCst), 4);
        assert!(
            state
                .queries
                .lock()
                .unwrap()
                .iter()
                .all(|query| !query.contains("since")),
            "run 1 starts from the beginning"
        );
        // The checkpoint moves only with the last batch, to the highest value seen: c5, although
        // c4 came before c3.
        assert_eq!(first.len(), 3);
        assert_eq!(since(&first[0]), None);
        assert_eq!(since(&first[1]), None);
        assert_eq!(since(&first[2]), Some("2026-01-05T00:00:00Z"));
        // The rotated refresh token is what the caller stores for the next run.
        let rotated = ctx.take_refreshed_token().unwrap();
        assert_eq!(rotated.refresh_token().unwrap().expose(), "rt-1");

        // Run 2 from the stored checkpoint sends `since` and receives only the newer record.
        state.records.lock().unwrap().push(contact(6, "active"));
        state.queries.lock().unwrap().clear();
        let ctx = context("http", "src-1");
        let second = ok(read_all(
            &ctx,
            &config,
            &oauth_credential("rt-1"),
            &stream,
            Some(first[2].checkpoint.clone()),
        )
        .await);

        assert_eq!(keys(&second), ["c6"]);
        assert!(
            state
                .queries
                .lock()
                .unwrap()
                .iter()
                .all(|query| query.contains("since=2026-01-05T00%3A00%3A00Z")),
            "{:?}",
            state.queries.lock().unwrap()
        );
        assert_eq!(since(second.last().unwrap()), Some("2026-01-06T00:00:00Z"));
        assert_eq!(state.refreshes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_token_revoked_mid_sync_is_refreshed_once_and_the_call_retried_once() {
        let state = ApiState::new((1..=5).map(|id| contact(id, "active")).collect());
        // The API stops accepting acc-1 on its second data request.
        state.revoke_at.store(2, Ordering::SeqCst);
        let mock = api(&state).await;
        let config = config_for(CURSOR, &mock);
        let ctx = context("http", "src-2");

        let batches = ok(read_all(
            &ctx,
            &config,
            &oauth_credential("rt-0"),
            &stream_config("contacts", SyncMode::FullRefresh),
            None,
        )
        .await);

        assert_eq!(keys(&batches).len(), 5);
        assert_eq!(
            state.refreshes.load(Ordering::SeqCst),
            2,
            "one more refresh"
        );
        // Three pages, and the second page asked twice: the revoked token, then the new one.
        assert_eq!(state.data_requests.load(Ordering::SeqCst), 4);
        let auths = state.authorizations.lock().unwrap().clone();
        assert_eq!(auths[1], "Bearer acc-1");
        assert_eq!(auths[2], "Bearer acc-2");
        // The token store, if any, would persist the rotated refresh token; with none the
        // caller reads it back.
        let rotated = ctx.take_refreshed_token().unwrap();
        assert_eq!(rotated.refresh_token().unwrap().expose(), "rt-2");
    }

    // ---- discovery, check, settings --------------------------------------------------------

    #[tokio::test]
    async fn discovery_reports_the_manifest_streams_and_makes_no_request() {
        let mock = serve(Router::new()).await;
        let config = config_for(CURSOR, &mock);

        let found = Source::discover(&Http, &context("http", "s"), &config, &Credential::new())
            .await
            .unwrap();

        assert_eq!(mock.hits(), 0);
        assert_eq!(found.len(), 1);
        let contacts = &found[0];
        assert_eq!(contacts.name, "contacts");
        assert_eq!(
            contacts
                .fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "email", "updated_at", "status"]
        );
        assert_eq!(
            contacts.sync_modes,
            [SyncMode::FullRefresh, SyncMode::Incremental]
        );
        assert_eq!(contacts.default_cursor_field.as_deref(), Some("updated_at"));
        assert_eq!(contacts.default_primary_key, ["id"]);
        let seed = contacts.suggested_mapping.as_ref().unwrap();
        assert_eq!(seed["key_column"], "id");

        // A stream with no `incremental` offers full refresh only, and suggests no mapping.
        let plain = config_for(OFFSET, &mock);
        let found = Source::discover(&Http, &context("http", "s"), &plain, &Credential::new())
            .await
            .unwrap();
        assert_eq!(found[0].sync_modes, [SyncMode::FullRefresh]);
        assert_eq!(found[0].default_cursor_field, None);
        assert!(found[0].suggested_mapping.is_none());
        assert_eq!(mock.hits(), 0);
    }

    #[tokio::test]
    async fn the_connection_test_requests_the_first_page_of_the_first_stream() {
        let state = ApiState::new(vec![contact(1, "active")]);
        let mock = api(&state).await;
        let config = config_for(CURSOR, &mock);

        let result = Source::check(
            &Http,
            &context("http", "s"),
            &config,
            &oauth_credential("rt-0"),
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(state.data_requests.load(Ordering::SeqCst), 1);

        // A refused refresh token is an auth failure, not a crash.
        let refused = Source::check(
            &Http,
            &context("http", "s2"),
            &config,
            &oauth_credential("rt-wrong"),
        )
        .await;
        assert_eq!(refused, Err(SourceError::Auth));
    }

    #[test]
    fn the_settings_are_checked_with_the_manifests_issues_and_never_a_value() {
        let good = json!({"manifest": serde_json::from_str::<Value>(OFFSET).unwrap()});
        assert_eq!(validate_config(&Http::SPEC, &good), Ok(()));

        let mut bad = good.clone();
        bad["manifest"]["streams"][0]["path"] = json!("SECRET-nope");
        let errors = validate_config(&Http::SPEC, &bad).unwrap_err();
        assert_eq!(
            errors,
            [FieldError {
                field: "manifest/streams/0/path".to_owned(),
                reason: "path_invalid".to_owned()
            }]
        );
        assert!(!format!("{errors:?}").contains("SECRET"));

        // The manifest must be an object, and is required.
        assert!(validate_config(&Http::SPEC, &json!({"manifest": "text"})).is_err());
        assert!(validate_config(&Http::SPEC, &json!({})).is_err());
    }

    #[test]
    fn a_provider_manifest_is_not_a_source_and_is_refused_before_any_request() {
        let provider = include_str!("../../../tests/fixtures/providers/bearer_timestamped.json");
        let config = json!({"manifest": serde_json::from_str::<Value>(provider).unwrap()});

        let errors = validate_config(&Http::SPEC, &config).unwrap_err();

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "manifest/destination");
        assert!(matches!(
            manifest_of(&config),
            Err(SourceError::Config { ref field, .. }) if field == "manifest/destination"
        ));
    }

    #[test]
    fn the_manifests_rate_limit_is_the_sources_rate_limit() {
        let with = json!({"manifest": serde_json::from_str::<Value>(CURSOR).unwrap()});
        let without = json!({"manifest": serde_json::from_str::<Value>(OFFSET).unwrap()});

        assert_eq!(
            Source::rate_limit(&Http, &with),
            Some(RateLimit {
                requests: 100,
                per: Duration::from_secs(60)
            })
        );
        assert_eq!(Source::rate_limit(&Http, &without), None);
        assert_eq!(Source::rate_limit(&Http, &json!({"manifest": 5})), None);

        let mut edited = with.clone();
        edited["manifest"]["rate_limit"] = json!({"requests": 7, "per_seconds": 2});
        assert_eq!(
            Source::rate_limit(&Http, &edited),
            Some(RateLimit {
                requests: 7,
                per: Duration::from_secs(2)
            })
        );
    }

    #[test]
    fn a_bearer_token_credential_is_offered_to_and_accepted_by_the_http_source() {
        use std::collections::BTreeSet;

        use zirv_flow_core::credentials::CredentialKind;

        let schema = CredentialKind::BearerToken.schema();
        let present: BTreeSet<String> = schema.fields.iter().map(|f| f.key.clone()).collect();
        let declared: Vec<_> = Http::SPEC.credential_fields.iter().map(|f| f.key).collect();

        for read in ["token"] {
            assert!(present.contains(read), "the kind lacks {read}");
        }
        for key in &present {
            assert!(
                declared.contains(&key.as_str()),
                "{key} is not a field of the owner"
            );
        }
        assert!(crate::sources::validate_credential_names(&Http::SPEC, &present).is_ok());
    }

    #[test]
    fn an_oauth2_client_credential_without_a_token_url_is_accepted_by_the_http_source() {
        use std::collections::BTreeSet;

        use zirv_flow_core::credentials::CredentialKind;

        let schema = CredentialKind::OAuth2Client.schema();
        let without: BTreeSet<String> = schema
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| f.key.clone())
            .collect();

        assert_eq!(
            without.iter().map(String::as_str).collect::<Vec<_>>(),
            ["client_id", "client_secret"]
        );
        for read in ["client_id", "client_secret"] {
            assert!(without.contains(read), "the kind lacks {read}");
        }
        assert!(crate::sources::validate_credential_names(&Http::SPEC, &without).is_ok());
    }

    #[test]
    fn the_spec_declares_secret_credentials_a_json_manifest_and_every_capability() {
        let spec = &Http::SPEC;

        assert_eq!(spec.kind, "http");
        assert_eq!(spec.config_fields.len(), 1);
        assert_eq!(spec.config_fields[0].kind, FieldKind::Json);
        assert!(spec.config_fields[0].required);
        assert!(spec.credential_fields.iter().all(|field| field.secret));
        assert!(spec.credential_fields.iter().all(|field| !field.required));
        let names: Vec<_> = spec
            .credential_fields
            .iter()
            .map(|field| field.key)
            .collect();
        for needed in [
            "api_key",
            "token",
            "username",
            "password",
            "client_id",
            "client_secret",
        ] {
            assert!(names.contains(&needed), "{needed}");
        }
        assert!(
            !names.contains(&REFRESH_TOKEN_KEY),
            "the consent writes it, nobody types it"
        );
        assert!(spec.capabilities.incremental && spec.capabilities.emits_deletes);
    }

    #[test]
    fn the_source_is_registered_in_every_build_with_one_registry_line() {
        let registry = Registry::built_in();

        assert_eq!(registry.get("http").unwrap().spec().kind, "http");
        assert_eq!(include_str!("../mod.rs").matches("http::Http").count(), 1);
    }

    #[test]
    fn the_module_never_names_the_http_client_it_sends_through() {
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("auth.rs", include_str!("auth.rs")),
            ("checkpoint.rs", include_str!("checkpoint.rs")),
            ("request.rs", include_str!("request.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap();
            for forbidden in ["reqwest", "GuardedClient"] {
                assert!(!production.contains(forbidden), "{name} names {forbidden}");
            }
        }
    }

    // ---- the other pagination styles and auth types ----------------------------------------

    #[tokio::test]
    async fn offset_paging_with_an_api_key_header_reads_every_record() {
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let log = Arc::clone(&seen);
        let mock = serve(Router::new().route(
            "/customers",
            get(move |headers: HeaderMap, RawQuery(query): RawQuery| {
                let log = Arc::clone(&log);
                async move {
                    log.lock().unwrap().push((
                        query.clone().unwrap_or_default(),
                        headers
                            .get("x-api-key")
                            .map(|value| value.to_str().unwrap().to_owned()),
                    ));
                    let params = query_map(&query);
                    let offset: usize = params["offset"].parse().unwrap();
                    let limit: usize = params["limit"].parse().unwrap();
                    let all: Vec<Value> = (1..=5)
                        .map(|id| {
                            json!({"id": id, "email": format!("p{id}@example.com"),
                                          "address": {"city": "Aarhus"}})
                        })
                        .collect();
                    let page: Vec<Value> = all.into_iter().skip(offset).take(limit).collect();
                    axum::Json(json!({"items": page}))
                }
            }),
        ))
        .await;
        let config = config_for(OFFSET, &mock);

        let batches = ok(read_all(
            &context("http", "s"),
            &config,
            &credential(&[("api_key", "the-key")]),
            &stream_config("customers", SyncMode::FullRefresh),
            None,
        )
        .await);

        assert_eq!(keys(&batches), ["1", "2", "3", "4", "5"]);
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|(_, key)| key.as_deref() == Some("the-key"))
        );
        // A full-refresh read leaves the checkpoint empty.
        assert!(batches.iter().all(|batch| since(batch).is_none()));
        // Nested objects stay in the record data for the mapping to reach.
        assert_eq!(batches[0].records[0].data["address"]["city"], "Aarhus");
    }

    #[tokio::test]
    async fn page_numbers_with_basic_auth_flag_the_soft_deleted_records() {
        let mock = serve(Router::new().route(
            "/api/members",
            get(|headers: HeaderMap, RawQuery(query): RawQuery| async move {
                let params = query_map(&query);
                assert_eq!(
                    headers.get("authorization").unwrap(),
                    // base64("user:pass")
                    "Basic dXNlcjpwYXNz"
                );
                assert_eq!(params["active_only"], "false");
                let page: usize = params["page"].parse().unwrap();
                let rows: Vec<Value> = (1..=3)
                    .map(|id| {
                        json!({"id": format!("m{id}"), "email": format!("m{id}@example.com"),
                                "removed": id == 2, "profile": {"tier": "gold"}})
                    })
                    .skip((page - 1) * 2)
                    .take(2)
                    .collect();
                axum::Json(json!({"members": rows}))
            }),
        ))
        .await;
        let config = config_for(PAGE, &mock);

        let batches = ok(read_all(
            &context("http", "s"),
            &config,
            &credential(&[("username", "user"), ("password", "pass")]),
            &stream_config("members", SyncMode::FullRefresh),
            None,
        )
        .await);

        let flags: Vec<(String, bool)> = batches
            .iter()
            .flat_map(|batch| batch.records.iter().map(|r| (r.key.clone(), r.deleted)))
            .collect();
        assert_eq!(
            flags,
            [
                ("m1".to_owned(), false),
                ("m2".to_owned(), true),
                ("m3".to_owned(), false)
            ]
        );
    }

    #[tokio::test]
    async fn link_headers_with_a_bearer_token_and_an_integer_cursor_read_incrementally() {
        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&queries);
        let mock = serve(Router::new().route(
            "/events",
            get(move |headers: HeaderMap, RawQuery(query): RawQuery| {
                let log = Arc::clone(&log);
                async move {
                    assert_eq!(headers.get("authorization").unwrap(), "Bearer tok");
                    log.lock().unwrap().push(query.clone().unwrap_or_default());
                    let params = query_map(&query);
                    let after: i64 = params.get("after").map_or(0, |v| v.parse().unwrap());
                    let page: usize = params.get("p").map_or(1, |v| v.parse().unwrap());
                    let all: Vec<Value> = (1..=5)
                        .filter(|seq| *seq > after)
                        .map(|seq| json!({"id": format!("e{seq}"), "seq": seq, "payload": {}}))
                        .collect();
                    let rows: Vec<Value> =
                        all.iter().skip((page - 1) * 2).take(2).cloned().collect();
                    let mut response = axum::Json(rows).into_response();
                    if page * 2 < all.len() {
                        let next = format!("</events?after={after}&p={}>; rel=\"next\"", page + 1);
                        response
                            .headers_mut()
                            .insert("link", HeaderValue::from_str(&next).unwrap());
                    }
                    response
                }
            }),
        ))
        .await;
        // The reference manifest reads the top-level array.
        let config = config_for(LINK, &mock);
        let stream = stream_config("events", SyncMode::Incremental);

        let first = ok(read_all(
            &context("http", "s"),
            &config,
            &credential(&[("token", "tok")]),
            &stream,
            None,
        )
        .await);
        assert_eq!(keys(&first), ["e1", "e2", "e3", "e4", "e5"]);
        assert_eq!(since(first.last().unwrap()), Some("5"));

        queries.lock().unwrap().clear();
        let second = ok(read_all(
            &context("http", "s"),
            &config,
            &credential(&[("token", "tok")]),
            &stream,
            Some(Checkpoint(json!({"since": "3"}))),
        )
        .await);
        assert_eq!(keys(&second), ["e4", "e5"]);
        assert!(queries.lock().unwrap()[0].contains("after=3"));
    }

    #[tokio::test]
    async fn a_full_refresh_read_ignores_a_stored_checkpoint() {
        let state = ApiState::new(vec![contact(1, "active"), contact(2, "active")]);
        let mock = api(&state).await;
        let config = config_for(CURSOR, &mock);

        let batches = ok(read_all(
            &context("http", "s"),
            &config,
            &oauth_credential("rt-0"),
            &stream_config("contacts", SyncMode::FullRefresh),
            Some(Checkpoint(json!({"since": "2026-01-02T00:00:00Z"}))),
        )
        .await);

        assert_eq!(keys(&batches), ["c1", "c2"]);
        assert!(!state.queries.lock().unwrap()[0].contains("since"));
    }

    #[tokio::test]
    async fn a_stream_with_no_records_ends_with_one_empty_batch_and_the_unchanged_checkpoint() {
        let state = ApiState::new(Vec::new());
        let mock = api(&state).await;
        let config = config_for(CURSOR, &mock);
        let stored = Checkpoint(json!({"since": "2026-01-02T00:00:00Z"}));

        let batches = ok(read_all(
            &context("http", "s"),
            &config,
            &oauth_credential("rt-0"),
            &stream_config("contacts", SyncMode::Incremental),
            Some(stored.clone()),
        )
        .await);

        assert_eq!(batches.len(), 1);
        assert!(batches[0].records.is_empty());
        assert_eq!(batches[0].checkpoint, stored);
    }

    #[tokio::test]
    async fn the_checkpoint_never_moves_back_below_where_the_run_started() {
        // The API ignores `since` and returns an old record: the checkpoint stays put.
        let state = ApiState::new(vec![contact(1, "active")]);
        let mock = serve(Router::new().route(
            "/v1/contacts",
            get(|| async {
                axum::Json(json!({"data": [contact(1, "active")], "next_cursor": null}))
            }),
        ))
        .await;
        let _ = state;
        let mut config = config_for(CURSOR, &mock);
        config["manifest"]["auth"] = json!({"type": "none"});

        let batches = ok(read_all(
            &context("http", "s"),
            &config,
            &Credential::new(),
            &stream_config("contacts", SyncMode::Incremental),
            Some(Checkpoint(json!({"since": "2026-01-09T00:00:00Z"}))),
        )
        .await);

        assert_eq!(since(batches.last().unwrap()), Some("2026-01-09T00:00:00Z"));
    }

    // ---- failures --------------------------------------------------------------------------

    async fn plain_api(body: Value) -> Mock {
        serve(Router::new().route(
            "/v1/contacts",
            get(move || {
                let body = body.clone();
                async move { axum::Json(body) }
            }),
        ))
        .await
    }

    fn no_auth(mock: &Mock) -> Value {
        let mut config = config_for(CURSOR, mock);
        config["manifest"]["auth"] = json!({"type": "none"});
        config
    }

    #[tokio::test]
    async fn a_record_that_is_not_an_object_or_has_no_key_fails_the_read_with_a_code() {
        for (body, code) in [
            (json!({"data": ["just text"]}), "record_not_object"),
            (
                json!({"data": [{"email": "a@example.com"}]}),
                "record_key_missing",
            ),
            (json!({"data": [{"id": ""}]}), "record_key_missing"),
        ] {
            let mock = plain_api(body).await;

            let results = read_all(
                &context("http", "s"),
                &no_auth(&mock),
                &Credential::new(),
                &stream_config("contacts", SyncMode::FullRefresh),
                None,
            )
            .await;

            assert_eq!(
                results.into_iter().last().unwrap(),
                Err(SourceError::Other(code.to_owned()))
            );
        }
    }

    #[tokio::test]
    async fn a_page_over_the_wrong_shape_or_a_stream_not_in_the_manifest_is_an_error() {
        let mock = plain_api(json!({"unexpected": []})).await;
        let results = read_all(
            &context("http", "s"),
            &no_auth(&mock),
            &Credential::new(),
            &stream_config("contacts", SyncMode::FullRefresh),
            None,
        )
        .await;
        assert!(matches!(results.as_slice(), [Err(SourceError::Other(_))]));

        let missing = read_all(
            &context("http", "s"),
            &no_auth(&mock),
            &Credential::new(),
            &stream_config("ghost", SyncMode::FullRefresh),
            None,
        )
        .await;
        assert_eq!(
            missing,
            [Err(SourceError::Config {
                field: "stream".into(),
                reason: "is not in the manifest".into()
            })]
        );
        assert_eq!(mock.hits(), 1, "the unknown stream made no request");
    }

    #[tokio::test]
    async fn an_invalid_stored_manifest_fails_the_read_before_any_request() {
        let mock = serve(Router::new()).await;
        let mut config = config_for(CURSOR, &mock);
        config["manifest"]["streams"][0]["path"] = json!("no-slash");

        let results = read_all(
            &context("http", "s"),
            &config,
            &oauth_credential("rt-0"),
            &stream_config("contacts", SyncMode::FullRefresh),
            None,
        )
        .await;

        assert_eq!(
            results,
            [Err(SourceError::Config {
                field: "manifest/streams/0/path".into(),
                reason: "is invalid (path_invalid)".into()
            })]
        );
        assert_eq!(mock.hits(), 0);
    }

    #[tokio::test]
    async fn a_missing_credential_field_is_a_config_error_naming_it_and_no_request_is_made() {
        let mock = serve(Router::new()).await;
        let config = config_for(PAGE, &mock);

        let results = read_all(
            &context("http", "s"),
            &config,
            &credential(&[("username", "user")]),
            &stream_config("members", SyncMode::FullRefresh),
            None,
        )
        .await;

        assert_eq!(
            results,
            [Err(SourceError::Config {
                field: "credential.password".into(),
                reason: "is required".into()
            })]
        );
        assert_eq!(mock.hits(), 0);
    }

    #[tokio::test]
    async fn a_link_to_another_origin_is_refused_and_never_requested() {
        let elsewhere = serve(Router::new().route("/events", get(|| async { "stolen" }))).await;
        let target = format!("<http://127.0.0.1:{}/events>; rel=\"next\"", elsewhere.port);
        let mock = serve(Router::new().route(
            "/events",
            get(move || {
                let target = target.clone();
                async move {
                    let mut response = axum::Json(json!([{"id": "e1", "seq": 1}])).into_response();
                    response
                        .headers_mut()
                        .insert("link", HeaderValue::from_str(&target).unwrap());
                    response
                }
            }),
        ))
        .await;
        let config = config_for(LINK, &mock);

        let results = read_all(
            &context("http", "s"),
            &config,
            &credential(&[("token", "tok")]),
            &stream_config("events", SyncMode::FullRefresh),
            None,
        )
        .await;

        assert!(results.last().unwrap().is_err());
        assert_eq!(elsewhere.hits(), 0, "the bearer token went nowhere else");
    }

    #[tokio::test]
    async fn the_default_egress_policy_blocks_loopback_and_no_request_arrives() {
        let state = ApiState::new(vec![contact(1, "active")]);
        let mock = api(&state).await;
        let config = no_auth(&mock);
        let blocked = SourceContext::new(
            Arc::new(EgressPolicy::default()),
            HttpLimits::default(),
            "http",
            "s",
            Arc::default(),
        )
        .unwrap();

        let results = read_all(
            &blocked,
            &config,
            &Credential::new(),
            &stream_config("contacts", SyncMode::FullRefresh),
            None,
        )
        .await;

        assert!(matches!(results.as_slice(), [Err(SourceError::Egress(_))]));
        assert_eq!(mock.hits(), 0);
    }

    #[tokio::test]
    async fn no_error_text_holds_a_url_a_token_or_a_record_value() {
        let state = ApiState::new(vec![contact(1, "active")]);
        let mock = api(&state).await;
        let base = format!("127.0.0.1:{}", mock.port);
        let mut errors: Vec<SourceError> = Vec::new();

        // A refused refresh token, a server error, a bad record, a bad manifest.
        let config = config_for(CURSOR, &mock);
        for refresh in ["rt-wrong"] {
            errors.extend(
                read_all(
                    &context("http", "s"),
                    &config,
                    &oauth_credential(refresh),
                    &stream_config("contacts", SyncMode::FullRefresh),
                    None,
                )
                .await
                .into_iter()
                .filter_map(Result::err),
            );
        }
        let failing = serve(Router::new().route(
            "/v1/contacts",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "body person1@example.com",
                )
            }),
        ))
        .await;
        errors.extend(
            read_all(
                &context("http", "s"),
                &no_auth(&failing),
                &Credential::new(),
                &stream_config("contacts", SyncMode::FullRefresh),
                None,
            )
            .await
            .into_iter()
            .filter_map(Result::err),
        );
        let bad_record = plain_api(json!({"data": [{"email": "person1@example.com"}]})).await;
        errors.extend(
            read_all(
                &context("http", "s"),
                &no_auth(&bad_record),
                &Credential::new(),
                &stream_config("contacts", SyncMode::FullRefresh),
                None,
            )
            .await
            .into_iter()
            .filter_map(Result::err),
        );

        assert!(errors.len() >= 3, "{errors:?}");
        for error in errors {
            let shown = format!("{error} {error:?}");
            for forbidden in [
                &base, "rt-0", "rt-wrong", "acc-1", "person1", "client", "shh",
            ] {
                assert!(!shown.contains(forbidden), "{forbidden} in {shown}");
            }
        }
    }
}
