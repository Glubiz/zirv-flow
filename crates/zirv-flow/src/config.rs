use hashbrown::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use figment::Figment;
use figment::error::Kind;
use figment::providers::{Format, Serialized, Toml};
use figment::value::Value;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::{Url, form_urlencoded};

const ENV_PREFIX: &str = "ZIRV_FLOW_";

/// The most concurrent drain loops per workspace `ingest.drain_shards` may ask for.
const MAX_DRAIN_SHARDS: u32 = 16;

/// The most render-host children `render.pool_size` may ask for.
const MAX_RENDER_POOL: u32 = 32;

/// The longest `sending.recheck_hours`: thirty days.
const MAX_SENDING_RECHECK_HOURS: u32 = 720;

/// The longest `render.deadline_ms`: ten minutes.
const MAX_RENDER_DEADLINE_MS: u64 = 600_000;

/// The least address space a render-host child needs to start and render: the loaded binary and
/// its libraries count against `RLIMIT_AS` as well as the heap.
const MIN_RENDER_MEMORY_MIB: u64 = 64;

/// The most `render.memory_limit_mib`: 1 TiB, which keeps the byte count far from overflowing.
const MAX_RENDER_MEMORY_MIB: u64 = 1_048_576;

const CONFIG_KEYS: [&str; 53] = [
    "server.bind",
    "server.cookie_secure",
    "server.public_url",
    "ops.bind",
    "auth.local.enabled",
    "auth.oidc.issuer_url",
    "auth.oidc.client_id",
    "auth.oidc.client_secret",
    "auth.oidc.scopes",
    "auth.oidc.auto_create_users",
    "secrets.master_key_file",
    "secrets.master_key",
    "secrets.previous_master_key_file",
    "secrets.previous_master_key",
    "database.url",
    "database.max_connections",
    "database.migrate_on_start",
    "worker.concurrency",
    "worker.lease_seconds",
    "worker.max_running_per_workspace",
    "events.retention_days",
    "events.min_retention_days",
    "events.max_retention_days",
    "events.reject_retention_days",
    "ingest.max_request_bytes",
    "ingest.max_batch_items",
    "ingest.rate_per_key",
    "ingest.burst_per_key",
    "ingest.max_webhook_bytes",
    "ingest.rate_per_hook",
    "ingest.burst_per_hook",
    "ingest.drain_batch_size",
    "ingest.drain_shards",
    "imports.max_file_bytes",
    "assets.max_file_bytes",
    "assets.max_workspace_bytes",
    "explorer.interactive_timeout_ms",
    "explorer.export_timeout_ms",
    "explorer.interactive_row_limit",
    "explorer.export_row_limit",
    "render.pool_size",
    "render.deadline_ms",
    "render.memory_limit_mib",
    "sending.recheck_hours",
    "egress.allow",
    "tracking.prefetch_user_agents",
    "privacy.ledger_dir",
    "ai.providers",
    "ai.rate_limit_per_user_per_minute",
    "ai.max_repair_attempts",
    "log.filter",
    "log.format",
    "log.otlp.endpoint",
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    server: ServerConfig,
    #[serde(default)]
    ops: OpsConfig,
    auth: AuthConfig,
    /// Shown by `config check` only while a master key is configured.
    #[serde(
        default,
        rename = "secrets",
        skip_serializing_if = "MasterKeyConfig::is_unset"
    )]
    master_key: MasterKeyConfig,
    database: DatabaseConfig,
    worker: WorkerConfig,
    events: EventsConfig,
    ingest: IngestConfig,
    imports: ImportsConfig,
    assets: AssetsConfig,
    explorer: ExplorerConfig,
    render: RenderConfig,
    sending: SendingConfig,
    #[serde(default)]
    egress: EgressConfig,
    #[serde(default)]
    tracking: TrackingConfig,
    #[serde(default)]
    privacy: PrivacyConfig,
    /// The model allowlist of the AI assistant; empty means the assistant is unavailable.
    #[serde(default)]
    ai: zirv_flow_api::AiConfig,
    log: LogConfig,
}

/// Which non-public destinations outbound connections may reach.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EgressConfig {
    /// CIDR ranges such as `10.0.5.7/32` that outbound connections may reach although they are
    /// loopback, private, link-local and so on. Cloud metadata addresses are unlocked only by an
    /// entry naming exactly that address.
    #[serde(default)]
    allow: Vec<String>,
}

/// Privacy settings (architecture 16.3).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyConfig {
    /// A directory on storage shared by every worker where each completed erasure is also
    /// written as a file, so it survives a restore of an older backup. Unset writes no file.
    #[serde(default)]
    ledger_dir: Option<PathBuf>,
}

/// Open and click tracking (architecture 10.2).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackingConfig {
    /// Extra user-agent fragments that mark an open as automated, added to the built-in generic
    /// markers (`proxy`, `prefetch`, `preview`, `scanner`, `crawler`, `bot`). Compared
    /// case-insensitively as a substring. An open that matches is recorded with `machine = true`.
    #[serde(default)]
    prefetch_user_agents: Vec<String>,
}

/// The longest `tracking.prefetch_user_agents` entry, in characters.
const MAX_USER_AGENT_FRAGMENT: usize = 128;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogConfig {
    /// A `tracing` filter directive, such as `info` or `warn,zirv_flow_jobs=debug`.
    filter: String,
    format: LogFormat,
    /// Trace export is off while this section is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    otlp: Option<OtlpConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OtlpConfig {
    /// The collector's OTLP/HTTP base URL, such as `http://collector:4318`.
    #[serde(deserialize_with = "any_secret_url")]
    endpoint: SecretUrl,
}

/// How log lines are written.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogFormat {
    /// One JSON object per line.
    Json,
    /// Human-readable lines, for local development.
    Text,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    bind: SocketAddr,
    cookie_secure: bool,
    /// The URL users reach the server at; the OIDC redirect URI is built from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_url: Option<String>,
}

/// The metrics and health listener of worker-only and scheduler-only processes.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpsConfig {
    /// Unset means no listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bind: Option<SocketAddr>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthConfig {
    local: LocalAuthConfig,
    /// OIDC sign-in is off while this section is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oidc: Option<OidcConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalAuthConfig {
    /// Whether e-mail and password sign-in is available.
    enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OidcConfig {
    issuer_url: String,
    client_id: String,
    client_secret: SecretValue,
    #[serde(default = "default_scopes")]
    scopes: Vec<String>,
    #[serde(default)]
    auto_create_users: bool,
}

fn default_scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(str::to_owned).into()
}

/// A secret that is never printed, serialised or put in a Debug dump.
struct SecretValue(String);

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("\"***\"")
    }
}

impl Serialize for SecretValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("***")
    }
}

impl<'de> Deserialize<'de> for SecretValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            return Err(serde::de::Error::custom("must not be empty"));
        }
        Ok(Self(value))
    }
}

/// Where the operator master key comes from, and during a rotation the key it replaces. At most
/// one of each pair is set.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MasterKeyConfig {
    /// A file holding the base64 key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    master_key_file: Option<PathBuf>,
    /// The base64 key itself.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_redacted"
    )]
    master_key: Option<SecretValue>,
    /// A file holding the base64 key the current one replaces, while a rotation is under way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_master_key_file: Option<PathBuf>,
    /// The base64 previous key itself.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_redacted"
    )]
    previous_master_key: Option<SecretValue>,
}

impl MasterKeyConfig {
    fn is_unset(&self) -> bool {
        self.master_key_file.is_none()
            && self.master_key.is_none()
            && self.previous_master_key_file.is_none()
            && self.previous_master_key.is_none()
    }
}

fn serialize_redacted<S>(_: &Option<SecretValue>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str("<redacted>")
}

/// Where the master key is to be read from.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum MasterKeySource<'a> {
    File(&'a Path),
    Inline(&'a str),
    Unset,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseConfig {
    url: SecretUrl,
    max_connections: u32,
    migrate_on_start: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerConfig {
    concurrency: u32,
    lease_seconds: u32,
    /// Unset means unlimited.
    #[serde(default)]
    max_running_per_workspace: Option<u32>,
}

/// How long the event store keeps what it holds, in days.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsConfig {
    /// How long events are kept unless their name has an override.
    retention_days: u32,
    /// The shortest and the longest per-name override a workspace may set.
    min_retention_days: u32,
    max_retention_days: u32,
    /// How long ingest rejects are kept.
    reject_retention_days: u32,
}

/// What the ingestion API accepts and how fast the drain applies it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestConfig {
    /// The largest request body, in bytes.
    max_request_bytes: u64,
    /// The most items in one `batch` request.
    max_batch_items: u32,
    /// Items per second each API key may send, per `serve` process.
    rate_per_key: u32,
    /// Items a key may send at once after a quiet period, per `serve` process.
    burst_per_key: u32,
    /// The largest webhook request body, in bytes.
    max_webhook_bytes: u64,
    /// Requests per second each webhook may receive, per `serve` process.
    rate_per_hook: u32,
    /// Requests a webhook may receive at once after a quiet period, per `serve` process.
    burst_per_hook: u32,
    /// Inbox rows applied per transaction by `ingest.drain`.
    drain_batch_size: u32,
    /// Concurrent drain loops per workspace in `ingest.drain`.
    drain_shards: u32,
}

/// The render host: the child processes that render templates with an OS memory limit
/// (architecture 11.9).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderConfig {
    /// Child processes, and so renders at once, in each process that renders (`serve`, `worker`).
    pool_size: u32,
    /// Milliseconds one render may take before its child is killed and replaced.
    deadline_ms: u64,
    /// Mebibytes of address space a child may use (`RLIMIT_AS`, Linux only).
    memory_limit_mib: u64,
}

/// Sending domains (architecture 10.2).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendingConfig {
    /// Hours a sending domain may go without a DNS check before the scheduler leader queues one.
    recheck_hours: u32,
}

/// What a CSV upload may be.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportsConfig {
    /// The largest uploaded file, in bytes.
    max_file_bytes: u64,
}

/// What an uploaded image and a workspace's images may be.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetsConfig {
    /// The largest image, in bytes.
    max_file_bytes: u64,
    /// The most bytes the images of one workspace may take together.
    max_workspace_bytes: u64,
}

/// How long the event explorer may run and how much it may return.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExplorerConfig {
    /// Milliseconds a count, a breakdown or a page of rows may run before it is cancelled.
    interactive_timeout_ms: u64,
    /// Milliseconds one page query of a CSV export may run.
    export_timeout_ms: u64,
    /// The most buckets or rows an interactive query returns.
    interactive_row_limit: u32,
    /// The most rows a CSV export writes.
    export_row_limit: u32,
}

struct SecretUrl(Url);

impl SecretUrl {
    fn parse(value: &str) -> Result<Self, ()> {
        let url = Url::parse(value).map_err(|_| ())?;
        if !matches!(url.scheme(), "postgres" | "postgresql") {
            return Err(());
        }

        Ok(Self(url))
    }

    fn redacted(&self) -> String {
        let mut url = self.0.clone();
        if url.password().is_some() {
            url.set_password(Some("***"))
                .expect("postgres URLs support passwords");
        }
        // Postgres also accepts `password` and `sslpassword` as query parameters.
        // Other pairs are kept byte for byte.
        if let Some(query) = url.query() {
            let query = query
                .split('&')
                .map(
                    |pair| match form_urlencoded::parse(pair.as_bytes()).next() {
                        Some((name, _)) if is_password_param(&name) => {
                            format!("{}=***", pair.split('=').next().unwrap_or_default())
                        }
                        _ => pair.to_owned(),
                    },
                )
                .collect::<Vec<_>>()
                .join("&");
            url.set_query(Some(&query));
        }

        url.to_string()
    }
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

fn is_password_param(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("password")
}

impl fmt::Debug for SecretUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SecretUrl")
            .field(&self.redacted())
            .finish()
    }
}

impl Serialize for SecretUrl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.redacted())
    }
}

impl<'de> Deserialize<'de> for SecretUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(|()| serde::de::Error::custom("not a valid postgres URL"))
    }
}

/// A [`SecretUrl`] of any scheme; the caller checks the scheme.
fn any_secret_url<'de, D>(deserializer: D) -> Result<SecretUrl, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    Url::parse(&value)
        .map(SecretUrl)
        .map_err(|_| serde::de::Error::custom("not a valid URL"))
}

#[derive(Debug)]
pub(crate) struct ConfigError {
    key: String,
    reason: String,
    source: String,
}

impl ConfigError {
    fn new(key: impl Into<String>, reason: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            reason: reason.into(),
            source: source.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: {} (from {})",
            self.key, self.reason, self.source
        )
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub(crate) fn load(
        path: Option<&Path>,
        env: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ConfigError> {
        let mut figment = Figment::new()
            .merge(Serialized::default("server.bind", "127.0.0.1:8080"))
            .merge(Serialized::default("server.cookie_secure", true))
            .merge(Serialized::default("auth.local.enabled", true))
            .merge(Serialized::default("database.max_connections", 10_u32))
            .merge(Serialized::default("database.migrate_on_start", false))
            .merge(Serialized::default("worker.concurrency", 4_u32))
            .merge(Serialized::default("worker.lease_seconds", 30_u32))
            .merge(Serialized::default("events.retention_days", 760_u32))
            .merge(Serialized::default("events.min_retention_days", 30_u32))
            .merge(Serialized::default("events.max_retention_days", 1095_u32))
            .merge(Serialized::default("events.reject_retention_days", 30_u32))
            .merge(Serialized::default(
                "ingest.max_request_bytes",
                1_048_576_u64,
            ))
            .merge(Serialized::default("ingest.max_batch_items", 500_u32))
            .merge(Serialized::default("ingest.rate_per_key", 10_000_u32))
            .merge(Serialized::default("ingest.burst_per_key", 20_000_u32))
            .merge(Serialized::default("ingest.max_webhook_bytes", 262_144_u64))
            .merge(Serialized::default("ingest.rate_per_hook", 50_u32))
            .merge(Serialized::default("ingest.burst_per_hook", 200_u32))
            .merge(Serialized::default("ingest.drain_batch_size", 500_u32))
            .merge(Serialized::default("ingest.drain_shards", 8_u32))
            .merge(Serialized::default(
                "imports.max_file_bytes",
                536_870_912_u64,
            ))
            .merge(Serialized::default("assets.max_file_bytes", 5_242_880_u64))
            .merge(Serialized::default(
                "assets.max_workspace_bytes",
                1_073_741_824_u64,
            ))
            .merge(Serialized::default(
                "explorer.interactive_timeout_ms",
                5_000_u64,
            ))
            .merge(Serialized::default(
                "explorer.export_timeout_ms",
                60_000_u64,
            ))
            .merge(Serialized::default(
                "explorer.interactive_row_limit",
                10_000_u32,
            ))
            .merge(Serialized::default(
                "explorer.export_row_limit",
                1_000_000_u32,
            ))
            .merge(Serialized::default("render.pool_size", 2_u32))
            .merge(Serialized::default("render.deadline_ms", 5_000_u64))
            .merge(Serialized::default("render.memory_limit_mib", 512_u64))
            .merge(Serialized::default("sending.recheck_hours", 6_u32))
            .merge(Serialized::default("log.filter", "info"))
            .merge(Serialized::default("log.format", "json"));
        if let Some(path) = path {
            figment = figment.merge(Toml::string(&read_file(path)?));
        }

        let mut env_sources = HashMap::new();
        for (name, value) in env {
            let name = name.to_string_lossy();
            let Some(key) = env_key(&name) else {
                continue;
            };
            if !CONFIG_KEYS.contains(&key.as_str()) {
                return Err(ConfigError::new(key, "unknown key", name));
            }
            if let Some(other) = env_sources.get(&key) {
                let reason = format!("is also set by {other}");
                return Err(ConfigError::new(key, reason, name));
            }
            let Ok(value) = value.into_string() else {
                return Err(ConfigError::new(key, "is not valid UTF-8", name));
            };

            let parsed = value
                .parse::<Value>()
                .expect("environment parsing is infallible");
            figment = figment.merge(Serialized::default(&key, parsed));
            env_sources.insert(key, name.into_owned());
        }

        let config = figment
            .extract::<Self>()
            .map_err(|error| convert_figment_error(error, path, &env_sources))?;

        for (key, value) in [
            ("database.max_connections", config.database.max_connections),
            ("worker.concurrency", config.worker.concurrency),
            ("worker.lease_seconds", config.worker.lease_seconds),
            ("events.retention_days", config.events.retention_days),
            (
                "events.min_retention_days",
                config.events.min_retention_days,
            ),
            (
                "events.max_retention_days",
                config.events.max_retention_days,
            ),
            (
                "events.reject_retention_days",
                config.events.reject_retention_days,
            ),
            ("ingest.max_batch_items", config.ingest.max_batch_items),
            ("ingest.rate_per_key", config.ingest.rate_per_key),
            ("ingest.burst_per_key", config.ingest.burst_per_key),
            ("ingest.rate_per_hook", config.ingest.rate_per_hook),
            ("ingest.burst_per_hook", config.ingest.burst_per_hook),
            ("ingest.drain_batch_size", config.ingest.drain_batch_size),
            (
                "explorer.interactive_row_limit",
                config.explorer.interactive_row_limit,
            ),
            (
                "explorer.export_row_limit",
                config.explorer.export_row_limit,
            ),
        ]
        .into_iter()
        .chain(
            config
                .worker
                .max_running_per_workspace
                .map(|cap| ("worker.max_running_per_workspace", cap)),
        ) {
            if value == 0 {
                return Err(ConfigError::new(
                    key,
                    "must be at least 1",
                    source_for(key, path, &env_sources),
                ));
            }
        }

        for (key, value) in [
            (
                "explorer.interactive_timeout_ms",
                config.explorer.interactive_timeout_ms,
            ),
            (
                "explorer.export_timeout_ms",
                config.explorer.export_timeout_ms,
            ),
        ] {
            if value == 0 {
                return Err(ConfigError::new(
                    key,
                    "must be at least 1",
                    source_for(key, path, &env_sources),
                ));
            }
        }

        if let Err(error) = zirv_flow_api::EgressPolicy::parse(&config.egress.allow) {
            return Err(ConfigError::new(
                "egress.allow",
                error.to_string(),
                source_for("egress.allow", path, &env_sources),
            ));
        }

        for entry in &config.tracking.prefetch_user_agents {
            let reason = if entry.trim().is_empty() {
                Some("must not hold an empty entry")
            } else if entry.chars().count() > MAX_USER_AGENT_FRAGMENT {
                Some("must not hold an entry over 128 characters")
            } else if entry.chars().any(char::is_control) {
                Some("must not hold a control character")
            } else {
                None
            };
            if let Some(reason) = reason {
                return Err(ConfigError::new(
                    "tracking.prefetch_user_agents",
                    reason,
                    source_for("tracking.prefetch_user_agents", path, &env_sources),
                ));
            }
        }

        if let Err(error) = config.ai.validate() {
            let top = if error.key.starts_with("ai.providers") {
                "ai.providers"
            } else {
                error.key.as_str()
            };
            let source = source_for(top, path, &env_sources);
            return Err(ConfigError::new(error.key, error.reason, source));
        }

        config.validate_render(path, &env_sources)?;
        config.validate_sending(path, &env_sources)?;
        config.validate_events(path, &env_sources)?;
        config.validate_ingest(path, &env_sources)?;
        if config.imports.max_file_bytes == 0 {
            return Err(ConfigError::new(
                "imports.max_file_bytes",
                "must be at least 1",
                source_for("imports.max_file_bytes", path, &env_sources),
            ));
        }

        for key in ["assets.max_file_bytes", "assets.max_workspace_bytes"] {
            let value = match key {
                "assets.max_file_bytes" => config.assets.max_file_bytes,
                _ => config.assets.max_workspace_bytes,
            };
            if value == 0 {
                return Err(ConfigError::new(
                    key,
                    "must be at least 1",
                    source_for(key, path, &env_sources),
                ));
            }
        }

        if !crate::log::is_valid_filter(&config.log.filter) {
            return Err(ConfigError::new(
                "log.filter",
                "not a valid filter directive",
                source_for("log.filter", path, &env_sources),
            ));
        }

        if let Some(otlp) = &config.log.otlp {
            let url = &otlp.endpoint.0;
            let valid = matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.query().is_none()
                && url.fragment().is_none();
            if !valid {
                return Err(ConfigError::new(
                    "log.otlp.endpoint",
                    "must be an http or https URL without a query or fragment",
                    source_for("log.otlp.endpoint", path, &env_sources),
                ));
            }
        }

        if config.master_key.master_key_file.is_some() && config.master_key.master_key.is_some() {
            let reason = "is set together with secrets.master_key_file; set only one";
            return Err(ConfigError::new(
                "secrets.master_key",
                reason,
                source_for("secrets.master_key", path, &env_sources),
            ));
        }

        if config.master_key.previous_master_key_file.is_some()
            && config.master_key.previous_master_key.is_some()
        {
            let reason = "is set together with secrets.previous_master_key_file; set only one";
            return Err(ConfigError::new(
                "secrets.previous_master_key",
                reason,
                source_for("secrets.previous_master_key", path, &env_sources),
            ));
        }

        config.validate_auth(path, &env_sources)?;
        Ok(config)
    }

    /// The pool needs a child and a deadline, and a child needs room to run at all.
    fn validate_render(
        &self,
        path: Option<&Path>,
        env_sources: &HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        let render = &self.render;
        let fail = |key: &str, reason: &str| {
            ConfigError::new(key, reason, source_for(key, path, env_sources))
        };
        if !(1..=MAX_RENDER_POOL).contains(&render.pool_size) {
            return Err(fail("render.pool_size", "must be between 1 and 32"));
        }
        if !(1..=MAX_RENDER_DEADLINE_MS).contains(&render.deadline_ms) {
            return Err(fail("render.deadline_ms", "must be between 1 and 600000"));
        }
        if !(MIN_RENDER_MEMORY_MIB..=MAX_RENDER_MEMORY_MIB).contains(&render.memory_limit_mib) {
            return Err(fail(
                "render.memory_limit_mib",
                "must be between 64 and 1048576",
            ));
        }
        Ok(())
    }

    /// The re-check interval must be at least an hour and at most thirty days.
    fn validate_sending(
        &self,
        path: Option<&Path>,
        env_sources: &HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        if !(1..=MAX_SENDING_RECHECK_HOURS).contains(&self.sending.recheck_hours) {
            return Err(ConfigError::new(
                "sending.recheck_hours",
                "must be between 1 and 720",
                source_for("sending.recheck_hours", path, env_sources),
            ));
        }
        Ok(())
    }

    /// The retention bounds must be ordered, and the default must lie within them.
    fn validate_events(
        &self,
        path: Option<&Path>,
        env_sources: &HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        let events = &self.events;
        let fail = |key: &str, reason: &str| {
            ConfigError::new(key, reason, source_for(key, path, env_sources))
        };
        if events.min_retention_days > events.max_retention_days {
            return Err(fail(
                "events.min_retention_days",
                "must not exceed events.max_retention_days",
            ));
        }
        if !(events.min_retention_days..=events.max_retention_days).contains(&events.retention_days)
        {
            return Err(fail(
                "events.retention_days",
                "must lie between events.min_retention_days and events.max_retention_days",
            ));
        }
        Ok(())
    }

    /// The body limit must be positive, and the burst must let a full batch through.
    fn validate_ingest(
        &self,
        path: Option<&Path>,
        env_sources: &HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        let ingest = &self.ingest;
        let fail = |key: &str, reason: &str| {
            ConfigError::new(key, reason, source_for(key, path, env_sources))
        };
        if ingest.max_request_bytes == 0 {
            return Err(fail("ingest.max_request_bytes", "must be at least 1"));
        }
        if ingest.max_webhook_bytes == 0 {
            return Err(fail("ingest.max_webhook_bytes", "must be at least 1"));
        }
        if ingest.burst_per_key < ingest.max_batch_items {
            return Err(fail(
                "ingest.burst_per_key",
                "must be at least ingest.max_batch_items, or a full batch could never pass",
            ));
        }
        if !(1..=MAX_DRAIN_SHARDS).contains(&ingest.drain_shards) {
            return Err(fail("ingest.drain_shards", "must be between 1 and 16"));
        }
        Ok(())
    }

    /// The checks that need more than a type: the OIDC URLs and the public URL they rely on.
    fn validate_auth(
        &self,
        path: Option<&Path>,
        env_sources: &HashMap<String, String>,
    ) -> Result<(), ConfigError> {
        let fail = |key: &str, reason: &str| {
            ConfigError::new(key, reason, source_for(key, path, env_sources))
        };
        if let Some(public_url) = &self.server.public_url {
            match Url::parse(public_url) {
                Ok(url) if matches!(url.scheme(), "http" | "https") && url.has_host() => {}
                _ => {
                    return Err(fail("server.public_url", "must be an http or https URL"));
                }
            }
        }
        let Some(oidc) = &self.auth.oidc else {
            return Ok(());
        };
        if self.server.public_url.is_none() {
            return Err(fail(
                "server.public_url",
                "is required when auth.oidc is configured",
            ));
        }
        let issuer = Url::parse(&oidc.issuer_url).ok().filter(|url| {
            url.has_host()
                && match url.scheme() {
                    "https" => true,
                    "http" => url.host_str().is_some_and(is_loopback),
                    _ => false,
                }
        });
        if issuer.is_none() {
            return Err(fail(
                "auth.oidc.issuer_url",
                "must be an https URL (http is allowed only for localhost)",
            ));
        }
        if oidc.client_id.trim().is_empty() {
            return Err(fail("auth.oidc.client_id", "must not be empty"));
        }
        Ok(())
    }

    pub(crate) fn server_bind(&self) -> SocketAddr {
        self.server.bind
    }

    pub(crate) fn ops_bind(&self) -> Option<SocketAddr> {
        self.ops.bind
    }

    /// The configured public URL without a trailing slash, when there is one. Unlike
    /// [`Config::base_url`] it never falls back to the bind address: an OAuth2 provider is told
    /// this address as the redirect URI.
    pub(crate) fn public_url(&self) -> Option<String> {
        self.server
            .public_url
            .as_deref()
            .map(|url| url.trim_end_matches('/').to_owned())
    }

    /// Where browsers reach the server: the public URL without a trailing slash, else the
    /// bind address over plain HTTP.
    pub(crate) fn base_url(&self) -> String {
        match &self.server.public_url {
            Some(public_url) => public_url.trim_end_matches('/').to_owned(),
            None => format!("http://{}", self.server.bind),
        }
    }

    /// Whether the session cookie is marked `Secure`; turn off only for plain-HTTP development.
    pub(crate) fn server_cookie_secure(&self) -> bool {
        self.server.cookie_secure
    }

    /// Whether e-mail and password sign-in is available.
    pub(crate) fn auth_local_enabled(&self) -> bool {
        self.auth.local.enabled
    }

    /// The OIDC settings and whether unknown users are created, when OIDC is configured.
    pub(crate) fn oidc(&self) -> Option<(zirv_flow_api::OidcSettings, bool)> {
        let oidc = self.auth.oidc.as_ref()?;
        let public_url = self.server.public_url.as_deref()?;
        // `openid` is always requested, first.
        let mut scopes = vec!["openid".to_owned()];
        for scope in &oidc.scopes {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        let settings = zirv_flow_api::OidcSettings {
            issuer_url: oidc.issuer_url.clone(),
            client_id: oidc.client_id.clone(),
            client_secret: zirv_flow_api::SecretString::new(oidc.client_secret.0.clone()),
            scopes,
            redirect_url: zirv_flow_api::oidc_redirect_url(public_url),
        };
        Some((settings, oidc.auto_create_users))
    }

    /// Where the operator master key is read from.
    pub(crate) fn master_key_source(&self) -> MasterKeySource<'_> {
        match (
            &self.master_key.master_key_file,
            &self.master_key.master_key,
        ) {
            (Some(path), _) => MasterKeySource::File(path),
            (None, Some(key)) => MasterKeySource::Inline(&key.0),
            (None, None) => MasterKeySource::Unset,
        }
    }

    /// Where the previous master key is read from, while a rotation is under way.
    pub(crate) fn previous_master_key_source(&self) -> MasterKeySource<'_> {
        match (
            &self.master_key.previous_master_key_file,
            &self.master_key.previous_master_key,
        ) {
            (Some(path), _) => MasterKeySource::File(path),
            (None, Some(key)) => MasterKeySource::Inline(&key.0),
            (None, None) => MasterKeySource::Unset,
        }
    }

    pub(crate) fn database_url(&self) -> &str {
        self.database.url.0.as_str()
    }

    pub(crate) fn database_max_connections(&self) -> u32 {
        self.database.max_connections
    }

    pub(crate) fn database_migrate_on_start(&self) -> bool {
        self.database.migrate_on_start
    }

    /// The `tracing` filter directive for the long-running commands' logs.
    pub(crate) fn log_filter(&self) -> &str {
        &self.log.filter
    }

    /// The collector's OTLP/HTTP base URL, when trace export is configured.
    pub(crate) fn log_otlp_endpoint(&self) -> Option<&str> {
        self.log.otlp.as_ref().map(|otlp| otlp.endpoint.0.as_str())
    }

    pub(crate) fn log_format(&self) -> LogFormat {
        self.log.format
    }

    /// Jobs a worker process runs at once.
    pub(crate) fn worker_concurrency(&self) -> u32 {
        self.worker.concurrency
    }

    /// How long a claimed job's lease lasts without a heartbeat.
    pub(crate) fn worker_lease_seconds(&self) -> u32 {
        self.worker.lease_seconds
    }

    /// What the event store keeps and for how long.
    pub(crate) fn events_retention(&self) -> zirv_flow_store::EventRetention {
        zirv_flow_store::EventRetention {
            default_days: self.events.retention_days,
            min_days: self.events.min_retention_days,
            max_days: self.events.max_retention_days,
            reject_days: self.events.reject_retention_days,
        }
    }

    /// What the ingestion API accepts and how fast a key may send.
    pub(crate) fn ingest_limits(&self) -> zirv_flow_api::IngestLimits {
        zirv_flow_api::IngestLimits {
            max_request_bytes: usize::try_from(self.ingest.max_request_bytes).unwrap_or(usize::MAX),
            max_batch_items: self.ingest.max_batch_items as usize,
            rate_per_key: self.ingest.rate_per_key,
            burst_per_key: self.ingest.burst_per_key,
            max_webhook_bytes: usize::try_from(self.ingest.max_webhook_bytes).unwrap_or(usize::MAX),
            rate_per_hook: self.ingest.rate_per_hook,
            burst_per_hook: self.ingest.burst_per_hook,
        }
    }

    /// What a CSV upload may be.
    pub(crate) fn import_limits(&self) -> zirv_flow_api::ImportLimits {
        zirv_flow_api::ImportLimits {
            max_file_bytes: self.imports.max_file_bytes,
            ..zirv_flow_api::ImportLimits::default()
        }
    }

    /// What an uploaded image and a workspace's images may be.
    pub(crate) fn asset_limits(&self) -> zirv_flow_api::AssetLimits {
        zirv_flow_api::AssetLimits {
            max_file_bytes: self.assets.max_file_bytes,
            max_workspace_bytes: self.assets.max_workspace_bytes,
            ..zirv_flow_api::AssetLimits::default()
        }
    }

    /// The event explorer's query timeouts and row limits.
    pub(crate) fn explorer_limits(&self) -> zirv_flow_api::ExplorerLimits {
        zirv_flow_api::ExplorerLimits {
            interactive_timeout: std::time::Duration::from_millis(
                self.explorer.interactive_timeout_ms,
            ),
            export_timeout: std::time::Duration::from_millis(self.explorer.export_timeout_ms),
            interactive_row_limit: self.explorer.interactive_row_limit,
            export_row_limit: self.explorer.export_row_limit,
        }
    }

    /// Inbox rows `ingest.drain` applies per transaction.
    pub(crate) fn ingest_drain_batch_size(&self) -> u32 {
        self.ingest.drain_batch_size
    }

    /// Concurrent drain loops per workspace in `ingest.drain`.
    pub(crate) fn ingest_drain_shards(&self) -> u32 {
        self.ingest.drain_shards
    }

    /// How long a sending domain may go without a DNS check before the scheduler queues one.
    pub(crate) fn sending_domain_recheck(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.sending.recheck_hours) * 3600)
    }

    /// The render pool's size and per-render deadline, and the children's memory limit in bytes.
    pub(crate) fn render_pool(&self) -> (zirv_flow_render::PoolConfig, u64) {
        (
            zirv_flow_render::PoolConfig {
                size: self.render.pool_size as usize,
                deadline: std::time::Duration::from_millis(self.render.deadline_ms),
            },
            self.render.memory_limit_mib * 1024 * 1024,
        )
    }

    /// The directory of the erasure ledger files (`privacy.ledger_dir`), when set.
    pub(crate) fn ledger_dir(&self) -> Option<&Path> {
        self.privacy.ledger_dir.as_deref()
    }

    /// The operator's extra user-agent fragments for machine opens, lower-cased and trimmed.
    pub(crate) fn prefetch_user_agents(&self) -> Vec<String> {
        self.tracking
            .prefetch_user_agents
            .iter()
            .map(|entry| entry.trim().to_lowercase())
            .collect()
    }

    /// The egress policy built from `egress.allow`, which `load` has validated.
    pub(crate) fn egress_policy(&self) -> Arc<zirv_flow_api::EgressPolicy> {
        let policy = zirv_flow_api::EgressPolicy::parse(&self.egress.allow)
            .expect("egress.allow is validated when the configuration loads");
        Arc::new(policy)
    }

    /// The AI assistant's allowlist and limits, which `load` has validated.
    pub(crate) fn ai_config(&self) -> zirv_flow_api::AiConfig {
        self.ai.clone()
    }

    /// The most jobs of one workspace that run at once; none means unlimited.
    pub(crate) fn worker_max_running_per_workspace(&self) -> Option<u32> {
        self.worker.max_running_per_workspace
    }
}

fn read_file(path: &Path) -> Result<String, ConfigError> {
    let source = path.display().to_string();
    let contents = fs::read_to_string(path).map_err(|error| {
        let reason = if error.kind() == std::io::ErrorKind::NotFound {
            "file does not exist"
        } else {
            "could not read file"
        };
        ConfigError::new("config", reason, source.clone())
    })?;

    // The parser's own message quotes the offending line, which may hold a
    // secret, so only the line number is reported.
    if let Err(error) = toml::from_str::<toml::Value>(&contents) {
        let reason = match error.span() {
            Some(span) => format!(
                "could not parse TOML at line {}",
                contents[..span.start].matches('\n').count() + 1
            ),
            None => "could not parse TOML".into(),
        };
        return Err(ConfigError::new("config", reason, source));
    }
    Ok(contents)
}

fn env_key(name: &str) -> Option<String> {
    let uppercase = name.to_ascii_uppercase();
    let remainder = uppercase.strip_prefix(ENV_PREFIX)?;
    if !remainder.contains("__") {
        return None;
    }

    Some(remainder.replace("__", ".").to_ascii_lowercase())
}

fn convert_figment_error(
    error: figment::Error,
    path: Option<&Path>,
    env_sources: &HashMap<String, String>,
) -> ConfigError {
    let error = error
        .into_iter()
        .next()
        .expect("figment errors are non-empty");
    let mut key = error.path.join(".");
    let reason = match error.kind {
        Kind::UnknownField(field, _) => {
            if !key.is_empty() {
                key.push('.');
            }
            key.push_str(&field);
            "unknown key".into()
        }
        Kind::MissingField(field) => {
            if !key.is_empty() {
                key.push('.');
            }
            key.push_str(&field);
            "is required".into()
        }
        _ if key == "database.url" => "not a valid postgres URL".into(),
        Kind::InvalidType(_, expected) | Kind::InvalidValue(_, expected) => {
            format!("invalid value; expected {expected}")
        }
        kind => kind.to_string(),
    };
    if key.is_empty() {
        key = "config".into();
    }

    ConfigError::new(key.clone(), reason, source_for(&key, path, env_sources))
}

fn source_for(key: &str, path: Option<&Path>, env_sources: &HashMap<String, String>) -> String {
    env_sources.get(key).cloned().unwrap_or_else(|| {
        path.map_or_else(|| "configuration".into(), |path| path.display().to_string())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestFile(std::path::PathBuf);

    impl TestFile {
        fn new(contents: &str) -> Self {
            let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("zirv-flow-config-{}-{id}.toml", std::process::id()));
            fs::write(&path, contents).unwrap();
            Self(path)
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn database_env(url: &str) -> Vec<(OsString, OsString)> {
        vec![("ZIRV_FLOW_DATABASE__URL".into(), url.into())]
    }

    #[cfg(unix)]
    fn non_utf8() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xff])
    }

    #[cfg(windows)]
    fn non_utf8() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xd800])
    }

    #[test]
    fn a_non_utf8_environment_value_names_its_variable() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_WORKER__CONCURRENCY".into(), non_utf8()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert_eq!(
            error,
            "worker.concurrency: is not valid UTF-8 (from ZIRV_FLOW_WORKER__CONCURRENCY)"
        );
    }

    #[test]
    fn an_unrelated_non_utf8_variable_is_ignored() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("UNRELATED".into(), non_utf8()));

        assert!(Config::load(None, env).is_ok());
    }

    #[test]
    fn a_key_set_by_two_differently_cased_variables_is_an_error() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_WORKER__CONCURRENCY".into(), "2".into()));
        env.push(("zirv_flow_worker__concurrency".into(), "3".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("worker.concurrency: is also set by ZIRV_FLOW_WORKER__CONCURRENCY"));
    }

    #[test]
    fn query_string_passwords_are_redacted() {
        let config = Config::load(
            None,
            database_env(
                "postgres://localhost/database?application_name=%FF&Pass%77ord=hunter2&sslpassword=hunter3",
            ),
        )
        .unwrap();

        let output = format!("{config:?}\n{}", toml::to_string(&config).unwrap());
        assert!(!output.contains("hunter2"));
        assert!(!output.contains("hunter3"));
        assert!(output.contains("application_name=%FF&Pass%77ord=***&sslpassword=***"));
    }

    #[test]
    fn an_unknown_file_key_names_the_key_and_file() {
        let file = TestFile::new(
            "[database]\nurl = \"postgres://localhost/database\"\n[server]\nport = 8080\n",
        );

        let error = Config::load(Some(&file.0), []).unwrap_err().to_string();

        assert!(error.contains("server.port"));
        assert!(error.contains(&file.0.display().to_string()));
    }

    #[test]
    fn the_master_key_is_unset_by_default_and_read_from_the_environment_or_a_file() {
        let none = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        assert_eq!(none.master_key_source(), MasterKeySource::Unset);

        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SECRETS__MASTER_KEY".into(), "abc".into()));
        let inline = Config::load(None, env).unwrap();
        assert_eq!(inline.master_key_source(), MasterKeySource::Inline("abc"));

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__MASTER_KEY_FILE".into(),
            "/k/master.key".into(),
        ));
        let file = Config::load(None, env).unwrap();
        assert_eq!(
            file.master_key_source(),
            MasterKeySource::File(Path::new("/k/master.key"))
        );
    }

    #[test]
    fn setting_both_master_key_keys_is_an_error_naming_the_key() {
        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[secrets]
master_key_file = \"/k\"
",
        );
        let env = vec![("ZIRV_FLOW_SECRETS__MASTER_KEY".into(), "abc".into())];

        let error = Config::load(Some(&file.0), env).unwrap_err().to_string();

        assert!(
            error.starts_with("secrets.master_key: is set together with secrets.master_key_file"),
            "{error}"
        );
        assert!(error.contains("ZIRV_FLOW_SECRETS__MASTER_KEY"), "{error}");
        assert!(!error.contains("abc"));
    }

    #[test]
    fn the_previous_master_key_is_unset_by_default_and_read_from_the_environment_or_a_file() {
        let none = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        assert_eq!(none.previous_master_key_source(), MasterKeySource::Unset);

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY".into(),
            "abc".into(),
        ));
        let inline = Config::load(None, env).unwrap();
        assert_eq!(
            inline.previous_master_key_source(),
            MasterKeySource::Inline("abc")
        );
        assert_eq!(inline.master_key_source(), MasterKeySource::Unset);

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY_FILE".into(),
            "/k/old.key".into(),
        ));
        let file = Config::load(None, env).unwrap();
        assert_eq!(
            file.previous_master_key_source(),
            MasterKeySource::File(Path::new("/k/old.key"))
        );
    }

    #[test]
    fn setting_both_previous_master_key_keys_is_an_error_naming_the_key() {
        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[secrets]
previous_master_key_file = \"/k\"
",
        );
        let env = vec![(
            "ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY".into(),
            "abc".into(),
        )];

        let error = Config::load(Some(&file.0), env).unwrap_err().to_string();

        assert!(
            error.starts_with(
                "secrets.previous_master_key: is set together with secrets.previous_master_key_file"
            ),
            "{error}"
        );
        assert!(
            error.contains("ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY"),
            "{error}"
        );
        assert!(!error.contains("abc"));
    }

    #[test]
    fn the_previous_master_key_is_redacted_and_config_check_reports_both_pairs() {
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__PREVIOUS_MASTER_KEY".into(),
            "old-secret-key".into(),
        ));
        env.push((
            "ZIRV_FLOW_SECRETS__MASTER_KEY_FILE".into(),
            "/k/master.key".into(),
        ));
        let config = Config::load(None, env).unwrap();

        let output = format!(
            "{config:?}
{}",
            toml::to_string(&config).unwrap()
        );

        assert!(!output.contains("old-secret-key"), "{output}");
        assert!(
            output.contains("previous_master_key = \"<redacted>\""),
            "{output}"
        );
        assert!(
            output.contains("master_key_file = \"/k/master.key\""),
            "{output}"
        );
    }

    #[test]
    fn the_master_key_is_redacted_in_debug_and_serialised_output() {
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_SECRETS__MASTER_KEY".into(),
            "super-secret-key".into(),
        ));
        let config = Config::load(None, env).unwrap();

        let output = format!(
            "{config:?}
{}",
            toml::to_string(&config).unwrap()
        );

        assert!(!output.contains("super-secret-key"), "{output}");
        assert!(output.contains("master_key = \"<redacted>\""), "{output}");
    }

    #[test]
    fn an_unknown_secrets_key_is_refused() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SECRETS__PEPPER".into(), "x".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.starts_with("secrets.pepper: unknown key"), "{error}");
    }

    #[test]
    fn an_unknown_environment_key_names_the_key_and_variable() {
        let error = Config::load(
            None,
            [
                (
                    "ZIRV_FLOW_DATABASE__URL".into(),
                    "postgres://localhost/database".into(),
                ),
                ("ZIRV_FLOW_WORKER__THREADS".into(), "2".into()),
            ],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("worker.threads"));
        assert!(error.contains("ZIRV_FLOW_WORKER__THREADS"));
    }

    #[test]
    fn an_invalid_socket_address_names_its_key() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SERVER__BIND".into(), "nope".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("server.bind"));
        assert!(error.contains("ZIRV_FLOW_SERVER__BIND"));
    }

    #[test]
    fn ops_bind_is_unset_by_default_and_read_from_file_or_environment() {
        let unset = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_OPS__BIND".into(), "127.0.0.1:9100".into()));
        let from_env = Config::load(None, env).unwrap();
        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[ops]
bind = \"127.0.0.1:9101\"
",
        );
        let from_file = Config::load(Some(&file.0), []).unwrap();

        assert_eq!(unset.ops_bind(), None);
        assert_eq!(from_env.ops_bind(), Some("127.0.0.1:9100".parse().unwrap()));
        assert_eq!(
            from_file.ops_bind(),
            Some("127.0.0.1:9101".parse().unwrap())
        );
        assert!(
            toml::to_string_pretty(&from_env)
                .unwrap()
                .contains("127.0.0.1:9100")
        );
    }

    #[test]
    fn an_invalid_ops_bind_names_its_key() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_OPS__BIND".into(), "nope".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("ops.bind"));
        assert!(error.contains("ZIRV_FLOW_OPS__BIND"));
    }

    #[test]
    fn session_cookies_are_secure_by_default() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert!(config.server_cookie_secure());
    }

    #[test]
    fn cookie_secure_can_be_turned_off_by_environment_or_file() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SERVER__COOKIE_SECURE".into(), "false".into()));
        let from_env = Config::load(None, env).unwrap();
        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[server]
cookie_secure = false
",
        );
        let from_file = Config::load(Some(&file.0), []).unwrap();

        assert!(!from_env.server_cookie_secure());
        assert!(!from_file.server_cookie_secure());
    }

    #[test]
    fn a_non_boolean_cookie_secure_names_its_key() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_SERVER__COOKIE_SECURE".into(), "maybe".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("server.cookie_secure"));
        assert!(error.contains("ZIRV_FLOW_SERVER__COOKIE_SECURE"));
    }

    #[test]
    fn zero_worker_concurrency_names_its_key() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_WORKER__CONCURRENCY".into(), "0".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("worker.concurrency"));
        assert!(error.contains("must be at least 1"));
    }

    #[test]
    fn the_worker_defaults_are_a_thirty_second_lease_and_no_workspace_cap() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert_eq!(config.worker_concurrency(), 4);
        assert_eq!(config.worker_lease_seconds(), 30);
        assert_eq!(config.worker_max_running_per_workspace(), None);
        assert!(!toml::to_string(&config).unwrap().contains("max_running"));
    }

    #[test]
    fn the_event_retention_defaults_are_25_months_and_30_to_1095_day_overrides() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert_eq!(
            config.events_retention(),
            zirv_flow_store::EventRetention::default()
        );
        let shown = toml::to_string(&config).unwrap();
        assert!(shown.contains("[events]") && shown.contains("retention_days = 760"));
    }

    #[test]
    fn the_event_retention_can_be_set_from_the_environment() {
        let mut env = database_env("postgres://localhost/database");
        for (key, value) in [
            ("RETENTION_DAYS", "100"),
            ("MIN_RETENTION_DAYS", "10"),
            ("MAX_RETENTION_DAYS", "200"),
            ("REJECT_RETENTION_DAYS", "7"),
        ] {
            env.push((format!("ZIRV_FLOW_EVENTS__{key}").into(), value.into()));
        }

        let retention = Config::load(None, env).unwrap().events_retention();

        assert_eq!(
            (
                retention.default_days,
                retention.min_days,
                retention.max_days,
                retention.reject_days
            ),
            (100, 10, 200, 7)
        );
    }

    #[test]
    fn the_event_retention_must_be_positive_ordered_and_contain_the_default() {
        let cases = [
            (
                "ZIRV_FLOW_EVENTS__REJECT_RETENTION_DAYS",
                "0",
                "must be at least 1",
            ),
            (
                "ZIRV_FLOW_EVENTS__MIN_RETENTION_DAYS",
                "2000",
                "must not exceed events.max_retention_days",
            ),
            ("ZIRV_FLOW_EVENTS__RETENTION_DAYS", "10", "must lie between"),
            (
                "ZIRV_FLOW_EVENTS__RETENTION_DAYS",
                "2000",
                "must lie between",
            ),
        ];
        for (key, value, expected) in cases {
            let mut env = database_env("postgres://localhost/database");
            env.push((key.into(), value.into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains(expected), "{key}: {error}");
        }
    }

    #[test]
    fn the_lease_and_the_workspace_cap_can_be_set_and_must_be_at_least_one() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_WORKER__LEASE_SECONDS".into(), "90".into()));
        env.push((
            "ZIRV_FLOW_WORKER__MAX_RUNNING_PER_WORKSPACE".into(),
            "2".into(),
        ));
        let config = Config::load(None, env).unwrap();

        assert_eq!(config.worker_lease_seconds(), 90);
        assert_eq!(config.worker_max_running_per_workspace(), Some(2));
        for key in [
            "ZIRV_FLOW_WORKER__LEASE_SECONDS",
            "ZIRV_FLOW_WORKER__MAX_RUNNING_PER_WORKSPACE",
        ] {
            let mut env = database_env("postgres://localhost/database");
            env.push((key.into(), "0".into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains("must be at least 1"), "{error}");
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn a_missing_database_url_names_its_key() {
        let error = Config::load(None, []).unwrap_err().to_string();

        assert!(error.contains("database.url"));
        assert!(error.contains("is required"));
    }

    #[test]
    fn environment_values_override_file_values_case_insensitively() {
        let file = TestFile::new(
            "[database]\nurl = \"postgres://localhost/database\"\nmax_connections = 3\n",
        );

        let config = Config::load(
            Some(&file.0),
            [("zirv_flow_database__max_connections".into(), "12".into())],
        )
        .unwrap();

        assert_eq!(config.database.max_connections, 12);
    }

    #[test]
    fn an_explicit_missing_file_is_an_error() {
        let path = std::env::temp_dir().join(format!(
            "zirv-flow-missing-config-{}.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);

        let error = Config::load(Some(&path), database_env("postgres://localhost/database"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("file does not exist"));
        assert!(error.contains(&path.display().to_string()));
    }

    #[test]
    fn a_toml_syntax_error_reports_its_line_without_the_line_text() {
        let file = TestFile::new(
            "[server]\nbind = \"127.0.0.1:8080\"\n[database]\nurl = \"postgres://user:hunter2@localhost/database\n",
        );

        let error = Config::load(Some(&file.0), []).unwrap_err().to_string();

        assert!(error.contains("could not parse TOML at line 4"));
        assert!(!error.contains("hunter2"));
    }

    #[test]
    fn an_invalid_database_url_does_not_echo_its_password() {
        let error = Config::load(
            None,
            database_env("not-postgres://user:hunter2@localhost/database"),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("database.url"));
        assert!(error.contains("not a valid postgres URL"));
        assert!(!error.contains("hunter2"));
    }

    #[test]
    fn debug_output_redacts_the_database_password() {
        let config = Config::load(
            None,
            database_env("postgres://user:hunter2@localhost/database"),
        )
        .unwrap();

        let output = format!("{config:?}");
        assert!(!output.contains("hunter2"));
        assert!(output.contains("***"));
    }

    const OIDC_FILE: &str = "[database]
url = \"postgres://localhost/database\"
[server]
public_url = \"https://flow.example.com/\"
[auth.local]
enabled = false
[auth.oidc]
issuer_url = \"https://idp.example.com/realms/main\"
client_id = \"zirv-flow\"
client_secret = \"file-secret\"
scopes = [\"email\", \"groups\"]
auto_create_users = true
";

    #[test]
    fn the_base_url_is_the_public_url_without_a_trailing_slash_else_the_bind_address() {
        let file = TestFile::new(OIDC_FILE);

        let public = Config::load(Some(&file.0), []).unwrap();
        let bound = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert_eq!(public.base_url(), "https://flow.example.com");
        assert_eq!(bound.base_url(), "http://127.0.0.1:8080");
        // The public URL itself never falls back to the bind address.
        assert_eq!(
            public.public_url().as_deref(),
            Some("https://flow.example.com")
        );
        assert_eq!(bound.public_url(), None);
    }

    #[test]
    fn local_sign_in_is_on_and_oidc_is_off_by_default() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert!(config.auth_local_enabled());
        assert!(config.oidc().is_none());
    }

    #[test]
    fn the_oidc_keys_load_from_a_file() {
        let file = TestFile::new(OIDC_FILE);

        let config = Config::load(Some(&file.0), []).unwrap();

        assert!(!config.auth_local_enabled());
        let (settings, auto_create_users) = config.oidc().unwrap();
        assert!(auto_create_users);
        assert_eq!(settings.issuer_url, "https://idp.example.com/realms/main");
        assert_eq!(settings.client_id, "zirv-flow");
        assert_eq!(
            settings.redirect_url,
            "https://flow.example.com/api/v1/session/oidc/callback"
        );
        assert_eq!(settings.scopes, ["openid", "email", "groups"]);
    }

    #[test]
    fn the_oidc_keys_load_from_the_environment_with_defaults() {
        let mut env = database_env("postgres://localhost/database");
        env.extend([
            (
                "ZIRV_FLOW_SERVER__PUBLIC_URL".into(),
                "http://localhost:8080".into(),
            ),
            (
                "ZIRV_FLOW_AUTH__OIDC__ISSUER_URL".into(),
                "http://localhost:8081/realms/main".into(),
            ),
            ("ZIRV_FLOW_AUTH__OIDC__CLIENT_ID".into(), "zirv-flow".into()),
            (
                "ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET".into(),
                "env-secret".into(),
            ),
            ("ZIRV_FLOW_AUTH__LOCAL__ENABLED".into(), "false".into()),
        ]);

        let config = Config::load(None, env).unwrap();

        let (settings, auto_create_users) = config.oidc().unwrap();
        assert!(!auto_create_users);
        assert!(!config.auth_local_enabled());
        assert_eq!(settings.scopes, ["openid", "email", "profile"]);
        assert_eq!(
            settings.redirect_url,
            "http://localhost:8080/api/v1/session/oidc/callback"
        );
    }

    #[test]
    fn the_environment_can_supply_just_the_secret_of_a_file_section() {
        let file = TestFile::new(&OIDC_FILE.replace(
            "client_secret = \"file-secret\"
",
            "",
        ));

        let missing = Config::load(Some(&file.0), []).unwrap_err().to_string();
        let supplied = Config::load(
            Some(&file.0),
            [(
                "ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET".into(),
                "env-secret".into(),
            )],
        );

        assert!(missing.contains("auth.oidc.client_secret: is required"));
        assert!(supplied.is_ok());
    }

    #[test]
    fn oidc_without_a_public_url_names_the_key() {
        let mut env = database_env("postgres://localhost/database");
        env.extend([
            (
                "ZIRV_FLOW_AUTH__OIDC__ISSUER_URL".into(),
                "https://idp.example.com".into(),
            ),
            ("ZIRV_FLOW_AUTH__OIDC__CLIENT_ID".into(), "zirv-flow".into()),
            ("ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET".into(), "s".into()),
        ]);

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("server.public_url: is required when auth.oidc is configured"));
    }

    #[test]
    fn a_bad_public_url_or_issuer_names_its_key() {
        let file = TestFile::new(&OIDC_FILE.replace("https://flow.example.com/", "ftp://flow"));
        let public = Config::load(Some(&file.0), []).unwrap_err().to_string();
        let file =
            TestFile::new(&OIDC_FILE.replace("https://idp.example.com", "http://idp.example.com"));
        let issuer = Config::load(Some(&file.0), []).unwrap_err().to_string();
        let file =
            TestFile::new(&OIDC_FILE.replace("https://idp.example.com", "http://localhost:8081"));
        let local = Config::load(Some(&file.0), []);

        assert!(
            public.contains("server.public_url: must be an http or https URL"),
            "{public}"
        );
        assert!(
            issuer.contains("auth.oidc.issuer_url: must be an https URL"),
            "{issuer}"
        );
        assert!(local.is_ok());
    }

    #[test]
    fn an_empty_client_secret_or_id_is_refused() {
        let file = TestFile::new(&OIDC_FILE.replace("file-secret", ""));
        let secret = Config::load(Some(&file.0), []).unwrap_err().to_string();
        let file =
            TestFile::new(&OIDC_FILE.replace("client_id = \"zirv-flow\"", "client_id = \" \""));
        let id = Config::load(Some(&file.0), []).unwrap_err().to_string();

        assert!(secret.contains("auth.oidc.client_secret"), "{secret}");
        assert!(
            id.contains("auth.oidc.client_id: must not be empty"),
            "{id}"
        );
    }

    #[test]
    fn an_unknown_oidc_key_is_refused() {
        let file = TestFile::new(&format!(
            "{OIDC_FILE}group_claim = \"groups\"
"
        ));

        let error = Config::load(Some(&file.0), []).unwrap_err().to_string();

        assert!(error.contains("auth.oidc.group_claim"), "{error}");
    }

    #[test]
    fn the_client_secret_is_redacted_in_debug_and_serialised_output() {
        let file = TestFile::new(OIDC_FILE);
        let config = Config::load(Some(&file.0), []).unwrap();

        let output = format!(
            "{config:?}
{}",
            toml::to_string(&config).unwrap()
        );

        assert!(!output.contains("file-secret"));
        assert!(output.contains("client_secret = \"***\""));
        assert!(output.contains("https://idp.example.com/realms/main"));
    }

    #[test]
    fn logs_default_to_json_at_info_and_both_keys_can_be_set_from_the_environment() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_LOG__FILTER".into(),
            "warn,zirv_flow_jobs=debug".into(),
        ));
        env.push(("ZIRV_FLOW_LOG__FORMAT".into(), "text".into()));

        let set = Config::load(None, env).unwrap();

        assert_eq!(defaults.log_filter(), "info");
        assert_eq!(defaults.log_format(), LogFormat::Json);
        assert_eq!(set.log_filter(), "warn,zirv_flow_jobs=debug");
        assert_eq!(set.log_format(), LogFormat::Text);
    }

    #[test]
    fn an_invalid_log_filter_or_format_is_a_config_error_naming_its_key() {
        let load = |name: &str, value: &str| {
            let mut env = database_env("postgres://localhost/database");
            env.push((name.into(), value.into()));
            Config::load(None, env).unwrap_err().to_string()
        };

        assert_eq!(
            load("ZIRV_FLOW_LOG__FILTER", "zirv=notalevel"),
            "log.filter: not a valid filter directive (from ZIRV_FLOW_LOG__FILTER)"
        );
        assert!(load("ZIRV_FLOW_LOG__FORMAT", "yaml").starts_with("log.format:"));
    }

    #[test]
    fn otlp_export_is_off_unless_an_endpoint_is_set_from_a_file_or_the_environment() {
        let dir = std::env::temp_dir().join(format!("zirv-flow-otlp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("zirv-flow.toml");
        std::fs::write(
            &file,
            "[log.otlp]
endpoint = \"http://collector:4318\"
",
        )
        .unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_LOG__FORMAT".into(), "text".into()));
        let from_file = Config::load(Some(&file), env).unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_LOG__OTLP__ENDPOINT".into(),
            "https://otel.example.com".into(),
        ));
        let from_env = Config::load(None, env).unwrap();
        let off = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            from_file.log_otlp_endpoint(),
            Some("http://collector:4318/")
        );
        assert_eq!(
            from_env.log_otlp_endpoint(),
            Some("https://otel.example.com/")
        );
        assert_eq!(off.log_otlp_endpoint(), None);
    }

    #[test]
    fn an_unusable_otlp_endpoint_is_a_config_error_naming_its_key() {
        let load = |value: &str| {
            let mut env = database_env("postgres://localhost/database");
            env.push(("ZIRV_FLOW_LOG__OTLP__ENDPOINT".into(), value.into()));
            Config::load(None, env).unwrap_err().to_string()
        };

        assert_eq!(
            load("grpc://collector:4317"),
            "log.otlp.endpoint: must be an http or https URL without a query or fragment (from ZIRV_FLOW_LOG__OTLP__ENDPOINT)"
        );
        assert!(load("not a url").starts_with("log.otlp.endpoint:"));
        assert!(load("http://h:4318?x=1").starts_with("log.otlp.endpoint:"));
        assert!(load("http://h:4318/otlp#frag").starts_with("log.otlp.endpoint:"));
    }

    #[test]
    fn an_endpoint_with_userinfo_is_redacted_in_the_dump_and_debug_but_kept_for_export() {
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_LOG__OTLP__ENDPOINT".into(),
            "http://user:secret@host:4318".into(),
        ));
        let config = Config::load(None, env).unwrap();

        let dump = toml::to_string_pretty(&config).unwrap();

        assert!(!dump.contains("secret"), "{dump}");
        assert!(dump.contains("http://user:***@host:4318"), "{dump}");
        assert!(!format!("{config:?}").contains("secret"));
        assert!(config.log_otlp_endpoint().unwrap().contains("secret"));
    }

    #[test]
    fn the_ingest_defaults_are_one_mib_500_items_10k_per_second_and_a_20k_burst() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        let limits = config.ingest_limits();

        assert_eq!(limits, zirv_flow_api::IngestLimits::default());
        assert_eq!(config.ingest_drain_batch_size(), 500);
        assert_eq!(config.ingest_drain_shards(), 8);
        assert!(toml::to_string(&config).unwrap().contains("[ingest]"));
    }

    #[test]
    fn the_ingest_settings_can_be_set_from_the_environment() {
        let mut env = database_env("postgres://localhost/database");
        for (key, value) in [
            ("MAX_REQUEST_BYTES", "2048"),
            ("MAX_BATCH_ITEMS", "10"),
            ("RATE_PER_KEY", "5"),
            ("BURST_PER_KEY", "50"),
            ("DRAIN_BATCH_SIZE", "25"),
            ("DRAIN_SHARDS", "6"),
        ] {
            env.push((format!("ZIRV_FLOW_INGEST__{key}").into(), value.into()));
        }

        let config = Config::load(None, env).unwrap();

        let limits = config.ingest_limits();
        assert_eq!(
            (
                limits.max_request_bytes,
                limits.max_batch_items,
                limits.rate_per_key,
                limits.burst_per_key
            ),
            (2048, 10, 5, 50)
        );
        assert_eq!(config.ingest_drain_batch_size(), 25);
        assert_eq!(config.ingest_drain_shards(), 6);
    }

    #[test]
    fn the_webhook_limits_default_to_256_kib_50_per_second_and_a_200_burst_and_can_be_set() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let limits = defaults.ingest_limits();
        assert_eq!(
            (
                limits.max_webhook_bytes,
                limits.rate_per_hook,
                limits.burst_per_hook
            ),
            (262_144, 50, 200)
        );

        let mut env = database_env("postgres://localhost/database");
        for (key, value) in [
            ("MAX_WEBHOOK_BYTES", "4096"),
            ("RATE_PER_HOOK", "7"),
            ("BURST_PER_HOOK", "9"),
        ] {
            env.push((format!("ZIRV_FLOW_INGEST__{key}").into(), value.into()));
        }
        let limits = Config::load(None, env).unwrap().ingest_limits();
        assert_eq!(
            (
                limits.max_webhook_bytes,
                limits.rate_per_hook,
                limits.burst_per_hook
            ),
            (4096, 7, 9)
        );
    }

    #[test]
    fn a_zero_webhook_limit_names_its_key() {
        for (key, name) in [
            ("MAX_WEBHOOK_BYTES", "ingest.max_webhook_bytes"),
            ("RATE_PER_HOOK", "ingest.rate_per_hook"),
            ("BURST_PER_HOOK", "ingest.burst_per_hook"),
        ] {
            let mut env = database_env("postgres://localhost/database");
            env.push((format!("ZIRV_FLOW_INGEST__{key}").into(), "0".into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains(name), "{error}");
        }
    }

    #[test]
    fn drain_shards_outside_one_to_sixteen_name_their_key() {
        for value in ["0", "17"] {
            let mut env = database_env("postgres://localhost/database");
            env.push(("ZIRV_FLOW_INGEST__DRAIN_SHARDS".into(), value.into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains("ingest.drain_shards"), "{error}");
            assert!(error.contains("between 1 and 16"), "{error}");
        }
    }

    #[test]
    fn the_import_upload_limit_defaults_to_512_mib_and_can_be_set() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_IMPORTS__MAX_FILE_BYTES".into(), "2048".into()));
        let set = Config::load(None, env).unwrap();

        assert_eq!(
            defaults.import_limits(),
            zirv_flow_api::ImportLimits::default()
        );
        assert_eq!(defaults.import_limits().max_file_bytes, 536_870_912);
        assert_eq!(set.import_limits().max_file_bytes, 2048);
        assert!(toml::to_string(&defaults).unwrap().contains("[imports]"));
    }

    #[test]
    fn the_egress_allowlist_is_empty_by_default_and_read_from_a_file_or_the_environment() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let loopback = "127.0.0.1".parse().unwrap();
        let office = "10.0.5.7".parse().unwrap();
        assert!(defaults.egress.allow.is_empty());
        assert_ne!(
            defaults.egress_policy().decide(loopback),
            zirv_flow_api::EgressDecision::Allow
        );
        assert!(toml::to_string(&defaults).unwrap().contains("[egress]"));

        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[egress]
allow = [\"10.0.5.7/32\", \"192.168.0.0/16\"]
",
        );
        let from_file = Config::load(Some(&file.0), []).unwrap();
        assert_eq!(from_file.egress.allow, ["10.0.5.7/32", "192.168.0.0/16"]);
        assert_eq!(
            from_file.egress_policy().decide(office),
            zirv_flow_api::EgressDecision::Allow
        );

        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_EGRESS__ALLOW".into(), "[\"10.0.5.7/32\"]".into()));
        let from_env = Config::load(None, env).unwrap();
        assert_eq!(from_env.egress.allow, ["10.0.5.7/32"]);
    }

    #[test]
    fn an_invalid_egress_entry_fails_naming_the_key_and_the_reason() {
        for (entry, reason) in [
            ("10.0.5.7", "is not a CIDR range"),
            ("10.0.0.9/8", "has host bits set; write it as 10.0.0.0/8"),
        ] {
            let mut env = database_env("postgres://localhost/database");
            env.push((
                "ZIRV_FLOW_EGRESS__ALLOW".into(),
                format!("[\"10.0.0.0/8\", \"{entry}\"]").into(),
            ));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.starts_with("egress.allow: "), "{error}");
            assert!(error.contains(entry), "{error}");
            assert!(error.contains(reason), "{error}");
            assert!(error.contains("ZIRV_FLOW_EGRESS__ALLOW"), "{error}");
        }
    }

    #[test]
    fn the_prefetch_user_agent_list_is_empty_by_default_and_read_from_a_file_or_the_environment() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        assert!(defaults.prefetch_user_agents().is_empty());
        assert!(toml::to_string(&defaults).unwrap().contains("[tracking]"));

        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[tracking]
prefetch_user_agents = [\" Example-Fetcher \", \"agent-x\"]
",
        );
        let from_file = Config::load(Some(&file.0), []).unwrap();
        assert_eq!(
            from_file.prefetch_user_agents(),
            ["example-fetcher", "agent-x"]
        );

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_TRACKING__PREFETCH_USER_AGENTS".into(),
            "[\"agent-y\"]".into(),
        ));
        assert_eq!(
            Config::load(None, env).unwrap().prefetch_user_agents(),
            ["agent-y"]
        );
    }

    #[test]
    fn the_ledger_dir_is_unset_by_default_and_read_from_a_file_or_the_environment() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        assert_eq!(defaults.ledger_dir(), None);

        let file = TestFile::new(
            "[database]
url = \"postgres://localhost/database\"
[privacy]
ledger_dir = \"/mnt/ledger\"
",
        );
        let from_file = Config::load(Some(&file.0), []).unwrap();
        assert_eq!(from_file.ledger_dir(), Some(Path::new("/mnt/ledger")));

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_PRIVACY__LEDGER_DIR".into(),
            "\"/var/ledger\"".into(),
        ));
        let from_env = Config::load(None, env).unwrap();
        assert_eq!(from_env.ledger_dir(), Some(Path::new("/var/ledger")));
        assert!(
            toml::to_string(&from_env)
                .unwrap()
                .contains("ledger_dir = \"/var/ledger\"")
        );
    }

    #[test]
    fn an_empty_or_oversized_prefetch_user_agent_fails_naming_the_key() {
        for entry in ["", "  ", &"x".repeat(129)] {
            let mut env = database_env("postgres://localhost/database");
            env.push((
                "ZIRV_FLOW_TRACKING__PREFETCH_USER_AGENTS".into(),
                format!("[\"{entry}\"]").into(),
            ));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(
                error.starts_with("tracking.prefetch_user_agents: "),
                "{error}"
            );
        }
    }

    const AI_FILE: &str = "[database]
url = \"postgres://localhost/database\"
[ai]
rate_limit_per_user_per_minute = 5
[[ai.providers]]
id = \"openai\"
kind = \"openai_compatible\"
base_url = \"https://api.openai.com/v1\"
[[ai.providers.models]]
name = \"gpt-x\"
context_window_tokens = 128000
input_price_per_mtok = 2500000
output_price_per_mtok = 10000000
";

    #[test]
    fn the_ai_section_is_empty_by_default_and_read_from_a_file() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        assert!(defaults.ai_config().providers.is_empty());
        assert_eq!(defaults.ai_config().rate_limit_per_user_per_minute, 20);
        assert_eq!(defaults.ai_config().max_repair_attempts, 2);
        assert!(toml::to_string(&defaults).unwrap().contains("[ai]"));

        let file = TestFile::new(AI_FILE);
        let loaded = Config::load(Some(&file.0), []).unwrap();
        let ai = loaded.ai_config();
        assert_eq!(ai.rate_limit_per_user_per_minute, 5);
        assert_eq!(ai.providers.len(), 1);
        let (provider, model) = ai.model("openai", "gpt-x").unwrap();
        assert_eq!(provider.base_url, "https://api.openai.com/v1");
        assert_eq!(model.context_window_tokens, 128_000);
        // The shown configuration round-trips, so `config check` prints what is in force.
        let shown = toml::to_string(&loaded).unwrap();
        assert!(shown.contains("gpt-x"), "{shown}");
    }

    #[test]
    fn the_ai_limits_can_be_set_from_the_environment() {
        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_AI__RATE_LIMIT_PER_USER_PER_MINUTE".into(),
            "7".into(),
        ));
        env.push(("ZIRV_FLOW_AI__MAX_REPAIR_ATTEMPTS".into(), "0".into()));

        let ai = Config::load(None, env).unwrap().ai_config();

        assert_eq!(ai.rate_limit_per_user_per_minute, 7);
        assert_eq!(ai.max_repair_attempts, 0);
    }

    #[test]
    fn an_invalid_ai_setting_fails_naming_the_key_and_where_it_came_from() {
        let duplicate = AI_FILE.to_owned()
            + "[[ai.providers]]
id = \"openai\"
kind = \"anthropic\"
base_url = \"https://api.anthropic.com\"
[[ai.providers.models]]
name = \"claude-x\"
context_window_tokens = 200000
input_price_per_mtok = 3000000
output_price_per_mtok = 15000000
";
        let file = TestFile::new(&duplicate);
        let error = Config::load(Some(&file.0), []).unwrap_err().to_string();
        assert!(error.starts_with("ai.providers[1].id: "), "{error}");
        assert!(error.contains("used by another provider"), "{error}");
        assert!(error.contains(&file.0.display().to_string()), "{error}");

        let mut env = database_env("postgres://localhost/database");
        env.push((
            "ZIRV_FLOW_AI__RATE_LIMIT_PER_USER_PER_MINUTE".into(),
            "0".into(),
        ));
        let error = Config::load(None, env).unwrap_err().to_string();
        assert!(
            error.contains("ai.rate_limit_per_user_per_minute"),
            "{error}"
        );
        assert!(error.contains("must be at least 1"), "{error}");
        assert!(
            error.contains("ZIRV_FLOW_AI__RATE_LIMIT_PER_USER_PER_MINUTE"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_ai_key_or_provider_field_is_refused() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_AI__API_KEY".into(), "sk-nope".into()));
        let error = Config::load(None, env).unwrap_err().to_string();
        assert!(error.contains("unknown key"), "{error}");

        let file = TestFile::new(&AI_FILE.replace("gpt-x", "gpt-x\\\"\napi_key = \\\"sk-oops"));
        assert!(Config::load(Some(&file.0), []).is_err());
    }

    #[test]
    fn the_asset_limits_default_to_5_mib_and_1_gib_and_can_be_set() {
        let defaults = Config::load(None, database_env("postgres://localhost/database")).unwrap();
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_ASSETS__MAX_FILE_BYTES".into(), "2048".into()));
        env.push((
            "ZIRV_FLOW_ASSETS__MAX_WORKSPACE_BYTES".into(),
            "4096".into(),
        ));
        let set = Config::load(None, env).unwrap();

        assert_eq!(
            defaults.asset_limits(),
            zirv_flow_api::AssetLimits::default()
        );
        assert_eq!(defaults.asset_limits().max_file_bytes, 5_242_880);
        assert_eq!(defaults.asset_limits().max_workspace_bytes, 1_073_741_824);
        assert_eq!(set.asset_limits().max_file_bytes, 2048);
        assert_eq!(set.asset_limits().max_workspace_bytes, 4096);
    }

    #[test]
    fn the_asset_limits_must_be_positive() {
        for key in ["MAX_FILE_BYTES", "MAX_WORKSPACE_BYTES"] {
            let mut env = database_env("postgres://localhost/database");
            env.push((format!("ZIRV_FLOW_ASSETS__{key}").into(), "0".into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains("assets.max_"), "{error}");
            assert!(error.contains("must be at least 1"), "{error}");
        }
    }

    #[test]
    fn the_import_upload_limit_must_be_positive() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_IMPORTS__MAX_FILE_BYTES".into(), "0".into()));

        let error = Config::load(None, env).unwrap_err().to_string();

        assert!(error.contains("imports.max_file_bytes"), "{error}");
        assert!(error.contains("must be at least 1"), "{error}");
    }

    #[test]
    fn the_ingest_settings_must_be_positive_and_the_burst_must_fit_a_full_batch() {
        let cases = [
            (
                "ZIRV_FLOW_INGEST__MAX_REQUEST_BYTES",
                "0",
                "must be at least 1",
            ),
            (
                "ZIRV_FLOW_INGEST__MAX_BATCH_ITEMS",
                "0",
                "must be at least 1",
            ),
            ("ZIRV_FLOW_INGEST__RATE_PER_KEY", "0", "must be at least 1"),
            ("ZIRV_FLOW_INGEST__BURST_PER_KEY", "0", "must be at least 1"),
            (
                "ZIRV_FLOW_INGEST__DRAIN_BATCH_SIZE",
                "0",
                "must be at least 1",
            ),
            (
                "ZIRV_FLOW_INGEST__BURST_PER_KEY",
                "499",
                "must be at least ingest.max_batch_items",
            ),
        ];
        for (key, value, expected) in cases {
            let mut env = database_env("postgres://localhost/database");
            env.push((key.into(), value.into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains(expected), "{key}: {error}");
        }
    }

    #[test]
    fn the_explorer_defaults_are_five_seconds_a_minute_ten_thousand_and_a_million() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        assert_eq!(
            config.explorer_limits(),
            zirv_flow_api::ExplorerLimits::default()
        );
        assert!(toml::to_string(&config).unwrap().contains("[explorer]"));
    }

    #[test]
    fn the_explorer_settings_can_be_set_from_the_environment_and_must_be_positive() {
        let mut env = database_env("postgres://localhost/database");
        for (key, value) in [
            ("INTERACTIVE_TIMEOUT_MS", "250"),
            ("EXPORT_TIMEOUT_MS", "9000"),
            ("INTERACTIVE_ROW_LIMIT", "50"),
            ("EXPORT_ROW_LIMIT", "77"),
        ] {
            env.push((format!("ZIRV_FLOW_EXPLORER__{key}").into(), value.into()));
        }

        let limits = Config::load(None, env).unwrap().explorer_limits();

        assert_eq!(
            limits.interactive_timeout,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(limits.export_timeout, std::time::Duration::from_secs(9));
        assert_eq!(
            (limits.interactive_row_limit, limits.export_row_limit),
            (50, 77)
        );
        for key in [
            "INTERACTIVE_TIMEOUT_MS",
            "EXPORT_TIMEOUT_MS",
            "INTERACTIVE_ROW_LIMIT",
            "EXPORT_ROW_LIMIT",
        ] {
            let mut env = database_env("postgres://localhost/database");
            env.push((format!("ZIRV_FLOW_EXPLORER__{key}").into(), "0".into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains("must be at least 1"), "{key}: {error}");
        }
    }

    #[test]
    fn sending_domains_are_rechecked_every_six_hours_unless_configured() {
        let mut env = database_env("postgres://localhost/database");
        assert_eq!(
            Config::load(None, env.clone())
                .unwrap()
                .sending_domain_recheck(),
            std::time::Duration::from_secs(6 * 3600)
        );

        env.push(("ZIRV_FLOW_SENDING__RECHECK_HOURS".into(), "12".into()));
        assert_eq!(
            Config::load(None, env).unwrap().sending_domain_recheck(),
            std::time::Duration::from_secs(12 * 3600)
        );
        for value in ["0", "721"] {
            let mut env = database_env("postgres://localhost/database");
            env.push(("ZIRV_FLOW_SENDING__RECHECK_HOURS".into(), value.into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(
                error.contains("sending.recheck_hours: must be between 1 and 720"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn the_render_defaults_are_two_children_five_seconds_and_512_mib() {
        let config = Config::load(None, database_env("postgres://localhost/database")).unwrap();

        let (pool, memory) = config.render_pool();

        assert_eq!(pool.size, 2);
        assert_eq!(pool.deadline, std::time::Duration::from_secs(5));
        assert_eq!(memory, 512 * 1024 * 1024);
        assert!(toml::to_string(&config).unwrap().contains("[render]"));
    }

    #[test]
    fn the_render_settings_come_from_the_environment_or_the_file_and_are_bounded() {
        let mut env = database_env("postgres://localhost/database");
        env.push(("ZIRV_FLOW_RENDER__POOL_SIZE".into(), "4".into()));
        env.push(("ZIRV_FLOW_RENDER__DEADLINE_MS".into(), "250".into()));
        let (pool, memory) = Config::load(None, env).unwrap().render_pool();
        assert_eq!(pool.size, 4);
        assert_eq!(pool.deadline, std::time::Duration::from_millis(250));
        assert_eq!(memory, 512 * 1024 * 1024);

        let file = TestFile::new(
            "[database]\nurl = \"postgres://localhost/database\"\n\n[render]\nmemory_limit_mib = 128\n",
        );
        let (_, memory) = Config::load(Some(&file.0), Vec::new())
            .unwrap()
            .render_pool();
        assert_eq!(memory, 128 * 1024 * 1024);

        for (key, value, expected) in [
            (
                "POOL_SIZE",
                "0",
                "render.pool_size: must be between 1 and 32",
            ),
            (
                "POOL_SIZE",
                "33",
                "render.pool_size: must be between 1 and 32",
            ),
            ("DEADLINE_MS", "0", "render.deadline_ms: must be between"),
            (
                "DEADLINE_MS",
                "600001",
                "render.deadline_ms: must be between",
            ),
            (
                "MEMORY_LIMIT_MIB",
                "63",
                "render.memory_limit_mib: must be between 64",
            ),
            (
                "MEMORY_LIMIT_MIB",
                "1048577",
                "render.memory_limit_mib: must be between 64",
            ),
        ] {
            let mut env = database_env("postgres://localhost/database");
            env.push((format!("ZIRV_FLOW_RENDER__{key}").into(), value.into()));

            let error = Config::load(None, env).unwrap_err().to_string();

            assert!(error.contains(expected), "{key}={value}: {error}");
        }
    }
}
