use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use figment::Figment;
use figment::error::Kind;
use figment::providers::{Format, Serialized, Toml};
use figment::value::Value;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use url::{Url, form_urlencoded};

const ENV_PREFIX: &str = "ZIRV_FLOW_";
const CONFIG_KEYS: [&str; 18] = [
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
    "database.url",
    "database.max_connections",
    "database.migrate_on_start",
    "worker.concurrency",
    "worker.lease_seconds",
    "worker.max_running_per_workspace",
    "log.filter",
    "log.format",
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    server: ServerConfig,
    #[serde(default)]
    ops: OpsConfig,
    auth: AuthConfig,
    database: DatabaseConfig,
    worker: WorkerConfig,
    log: LogConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogConfig {
    /// A `tracing` filter directive, such as `info` or `warn,zirv_flow_jobs=debug`.
    filter: String,
    format: LogFormat,
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

        if !crate::log::is_valid_filter(&config.log.filter) {
            return Err(ConfigError::new(
                "log.filter",
                "not a valid filter directive",
                source_for("log.filter", path, &env_sources),
            ));
        }

        config.validate_auth(path, &env_sources)?;
        Ok(config)
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
}
