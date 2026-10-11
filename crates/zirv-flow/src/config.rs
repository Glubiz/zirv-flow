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
const CONFIG_KEYS: [&str; 8] = [
    "server.bind",
    "server.cookie_secure",
    "database.url",
    "database.max_connections",
    "database.migrate_on_start",
    "worker.concurrency",
    "worker.lease_seconds",
    "worker.max_running_per_workspace",
];

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    server: ServerConfig,
    database: DatabaseConfig,
    worker: WorkerConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerConfig {
    bind: SocketAddr,
    cookie_secure: bool,
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
            .merge(Serialized::default("database.max_connections", 10_u32))
            .merge(Serialized::default("database.migrate_on_start", false))
            .merge(Serialized::default("worker.concurrency", 4_u32))
            .merge(Serialized::default("worker.lease_seconds", 30_u32));
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

        Ok(config)
    }

    pub(crate) fn server_bind(&self) -> SocketAddr {
        self.server.bind
    }

    /// Whether the session cookie is marked `Secure`; turn off only for plain-HTTP development.
    pub(crate) fn server_cookie_secure(&self) -> bool {
        self.server.cookie_secure
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
}
