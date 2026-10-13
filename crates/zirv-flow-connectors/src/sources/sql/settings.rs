//! Parses and validates the settings and the credential of a database source. Reasons name the
//! setting and never repeat a value.

use std::time::Duration;

use serde_json::Value;

use crate::sources::{Credential, FieldError, SourceError};

/// The most query streams one source can define.
pub(crate) const MAX_QUERIES: usize = 20;
const MAX_QUERY_NAME: usize = 64;
const MAX_QUERY_SQL: usize = 20_000;
const MAX_HOST: usize = 253;
const MAX_NAME: usize = 128;
const DEFAULT_STATEMENT_TIMEOUT_SECONDS: u64 = 60;
const MAX_STATEMENT_TIMEOUT_SECONDS: u64 = 600;

/// The prefix of a query stream's name.
pub(crate) const QUERY_PREFIX: &str = "query:";

/// How the connection is secured.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TlsMode {
    /// TLS, verified against the configured CA or the compiled-in roots. The default.
    Verify,
    /// No TLS. Accepted only toward addresses the operator allowlisted.
    Off,
}

/// A query stream: a named `SELECT`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Query {
    pub(crate) name: String,
    pub(crate) sql: String,
}

/// What a database source's configuration says.
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) database: String,
    /// PostgreSQL only; `public` unless set. MySQL reads the database it connects to.
    pub(crate) schema: String,
    pub(crate) tls: TlsMode,
    pub(crate) ca_certificate: Option<String>,
    pub(crate) statement_timeout: Duration,
    pub(crate) queries: Vec<Query>,
}

/// Which database the settings are for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Flavor {
    Postgres,
    MySql,
}

impl Flavor {
    pub(crate) fn default_port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::MySql => 3306,
        }
    }
}

pub(crate) fn config_error(field: &str, reason: &str) -> SourceError {
    SourceError::Config {
        field: field.to_owned(),
        reason: reason.to_owned(),
    }
}

impl Settings {
    pub(crate) fn parse(config: &Value, flavor: Flavor) -> Result<Self, SourceError> {
        let text = |key: &str| config.get(key).and_then(Value::as_str);
        let host = text("host").ok_or_else(|| config_error("host", "is required"))?;
        if let Some(reason) = host_problem(host) {
            return Err(config_error("host", reason));
        }
        let port = match config.get("port") {
            None | Some(Value::Null) => flavor.default_port(),
            Some(value) => value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port != 0)
                .ok_or_else(|| config_error("port", "must be a whole number from 1 to 65535"))?,
        };
        let database = text("database").ok_or_else(|| config_error("database", "is required"))?;
        if let Some(reason) = name_problem(database) {
            return Err(config_error("database", reason));
        }
        let schema = match text("schema") {
            None => "public",
            Some(schema) => {
                if let Some(reason) = name_problem(schema) {
                    return Err(config_error("schema", reason));
                }
                schema
            }
        };
        let tls = match text("tls") {
            None | Some("verify") => TlsMode::Verify,
            Some("off") => TlsMode::Off,
            Some(_) => return Err(config_error("tls", "must be verify or off")),
        };
        let ca_certificate = match text("ca_certificate") {
            Some(pem) if !pem.trim().is_empty() => {
                if let Some(reason) = super::tls::pem_problem(pem) {
                    return Err(config_error("ca_certificate", reason));
                }
                Some(pem.to_owned())
            }
            _ => None,
        };
        let seconds = match config.get("statement_timeout_seconds") {
            None | Some(Value::Null) => DEFAULT_STATEMENT_TIMEOUT_SECONDS,
            Some(value) => value
                .as_u64()
                .filter(|seconds| (1..=MAX_STATEMENT_TIMEOUT_SECONDS).contains(seconds))
                .ok_or_else(|| {
                    config_error(
                        "statement_timeout_seconds",
                        "must be a whole number from 1 to 600",
                    )
                })?,
        };
        let queries = match config.get("queries") {
            None | Some(Value::Null) => Vec::new(),
            Some(value) => {
                parse_queries(value).map_err(|reason| config_error("queries", reason))?
            }
        };
        Ok(Self {
            host: host.to_owned(),
            port,
            database: database.to_owned(),
            schema: schema.to_owned(),
            tls,
            ca_certificate,
            statement_timeout: Duration::from_secs(seconds),
            queries,
        })
    }

    /// The query stream `stream` names (`query:<name>`), if it is one.
    pub(crate) fn query_for(&self, stream: &str) -> Option<&Query> {
        let name = stream.strip_prefix(QUERY_PREFIX)?;
        self.queries.iter().find(|query| query.name == name)
    }
}

/// The `config_check` of the database source types: what [`Settings::parse`] refuses, as the
/// setting that failed.
pub(crate) fn check_config(config: &Value, flavor: Flavor) -> Vec<FieldError> {
    match Settings::parse(config, flavor) {
        Err(SourceError::Config { field, reason }) => vec![FieldError { field, reason }],
        Ok(_) | Err(_) => Vec::new(),
    }
}

/// The user and password of a database source.
pub(crate) struct Login<'a> {
    pub(crate) user: &'a str,
    pub(crate) password: &'a str,
}

impl<'a> Login<'a> {
    pub(crate) fn from_credential(credential: &'a Credential) -> Result<Self, SourceError> {
        let field = |key: &str| {
            credential
                .get(key)
                .map(|secret| secret.expose())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| config_error(key, "is required"))
        };
        Ok(Self {
            user: field("user")?,
            password: field("password")?,
        })
    }
}

fn host_problem(host: &str) -> Option<&'static str> {
    if host.is_empty() {
        return Some("must not be empty");
    }
    if host.len() > MAX_HOST {
        return Some("is too long");
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-'))
    {
        return Some("may contain only letters, digits, dots, colons and hyphens");
    }
    None
}

fn name_problem(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("must not be empty");
    }
    if name.len() > MAX_NAME {
        return Some("is too long");
    }
    if name.contains('\0') {
        return Some("must not contain a NUL character");
    }
    None
}

/// Why `value` is not an acceptable `queries` setting, for `validate_config`.
pub(crate) fn queries_problem(value: &Value) -> Option<&'static str> {
    parse_queries(value).err()
}

fn parse_queries(value: &Value) -> Result<Vec<Query>, &'static str> {
    let list = value.as_array().ok_or("must be a list")?;
    if list.len() > MAX_QUERIES {
        return Err("has more than 20 queries");
    }
    let mut queries: Vec<Query> = Vec::with_capacity(list.len());
    for entry in list {
        let object = entry.as_object().ok_or("must be a list of queries")?;
        if object.keys().any(|key| key != "name" && key != "sql") {
            return Err("has a query with a key other than name and sql");
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or("has a query without a name")?;
        let sql = object
            .get("sql")
            .and_then(Value::as_str)
            .ok_or("has a query without sql")?;
        if name.is_empty()
            || name.len() > MAX_QUERY_NAME
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err("has a query name that is not 1 to 64 characters of a-z, 0-9 and _");
        }
        if queries.iter().any(|query| query.name == name) {
            return Err("has two queries with one name");
        }
        if let Some(reason) = sql_problem(sql) {
            return Err(reason);
        }
        queries.push(Query {
            name: name.to_owned(),
            sql: sql.to_owned(),
        });
    }
    Ok(queries)
}

fn sql_problem(sql: &str) -> Option<&'static str> {
    if sql.trim().is_empty() || sql.chars().count() > MAX_QUERY_SQL {
        return Some("has a query that is empty or longer than 20,000 characters");
    }
    if sql.contains('\0') {
        return Some("has a query with a NUL character");
    }
    if sql.trim_end().ends_with(';') {
        return Some("has a query that ends with a semicolon");
    }
    let trimmed = sql.trim_start();
    let word: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let boundary = trimmed[word.len()..]
        .chars()
        .next()
        .is_none_or(|c| c.is_whitespace() || c == '(');
    if !(word.eq_ignore_ascii_case("select") || word.eq_ignore_ascii_case("with")) || !boundary {
        return Some("has a query that does not start with SELECT or WITH");
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::SecretString;

    use super::*;

    fn base() -> Value {
        json!({"host": "db.example.com", "database": "shop"})
    }

    fn with(key: &str, value: Value) -> Value {
        let mut config = base();
        config[key] = value;
        config
    }

    fn field_of(result: Result<Settings, SourceError>) -> String {
        match result.unwrap_err() {
            SourceError::Config { field, reason } => format!("{field} {reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn defaults_apply_when_only_the_required_settings_are_given() {
        let settings = Settings::parse(&base(), Flavor::Postgres).unwrap();

        assert_eq!(settings.port, 5432);
        assert_eq!(settings.schema, "public");
        assert_eq!(settings.tls, TlsMode::Verify);
        assert_eq!(settings.statement_timeout, Duration::from_secs(60));
        assert!(settings.queries.is_empty() && settings.ca_certificate.is_none());
        assert_eq!(Settings::parse(&base(), Flavor::MySql).unwrap().port, 3306);
    }

    #[test]
    fn a_host_with_a_path_a_credential_or_a_space_is_refused() {
        for host in [
            "",
            "/var/run/postgresql",
            "user@host",
            "my host",
            "a/b",
            "h;x",
        ] {
            let error = field_of(Settings::parse(
                &with("host", json!(host)),
                Flavor::Postgres,
            ));
            assert!(error.starts_with("host "), "{host}: {error}");
            assert!(!error.contains("var/run"), "{error}");
        }
        for host in ["db.example.com", "10.0.0.5", "::1", "my-db.internal"] {
            assert!(Settings::parse(&with("host", json!(host)), Flavor::Postgres).is_ok());
        }
    }

    #[test]
    fn the_port_must_be_a_whole_number_in_range() {
        for port in [
            json!(0),
            json!(65536),
            json!(-1),
            json!(5432.5),
            json!("5432"),
        ] {
            let error = field_of(Settings::parse(&with("port", port), Flavor::Postgres));
            assert!(error.starts_with("port "), "{error}");
        }
        let settings = Settings::parse(&with("port", json!(65535)), Flavor::Postgres).unwrap();
        assert_eq!(settings.port, 65535);
    }

    #[test]
    fn the_statement_timeout_is_one_to_six_hundred_seconds() {
        for seconds in [json!(0), json!(601), json!(1.5)] {
            let config = with("statement_timeout_seconds", seconds);
            let error = field_of(Settings::parse(&config, Flavor::MySql));
            assert!(error.starts_with("statement_timeout_seconds "), "{error}");
        }
        let config = with("statement_timeout_seconds", json!(600));
        let settings = Settings::parse(&config, Flavor::MySql).unwrap();
        assert_eq!(settings.statement_timeout, Duration::from_secs(600));
    }

    #[test]
    fn tls_is_verify_or_off_and_nothing_else() {
        let off = Settings::parse(&with("tls", json!("off")), Flavor::Postgres).unwrap();
        assert_eq!(off.tls, TlsMode::Off);
        let error = field_of(Settings::parse(
            &with("tls", json!("prefer")),
            Flavor::Postgres,
        ));
        assert_eq!(error, "tls must be verify or off");
    }

    #[test]
    fn the_config_check_reports_the_failing_setting_without_its_value() {
        let bad = with("host", json!("db/../x"));

        let errors = check_config(&bad, Flavor::Postgres);

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "host");
        assert!(!errors[0].reason.contains("db"), "{}", errors[0].reason);
        assert!(check_config(&base(), Flavor::MySql).is_empty());
    }

    #[test]
    fn a_ca_that_is_not_a_certificate_is_refused_without_repeating_it() {
        let config = with("ca_certificate", json!("hunter2 not a pem"));

        let error = field_of(Settings::parse(&config, Flavor::Postgres));

        assert!(error.starts_with("ca_certificate "), "{error}");
        assert!(!error.contains("hunter2"));
    }

    fn query(name: &str, sql: &str) -> Value {
        json!({"name": name, "sql": sql})
    }

    #[test]
    fn valid_queries_are_parsed_and_found_by_stream_name() {
        let config = with(
            "queries",
            json!([
                query("vip", "SELECT id FROM customers"),
                query("recent", "with x as (select 1) select * from x")
            ]),
        );

        let settings = Settings::parse(&config, Flavor::Postgres).unwrap();

        assert_eq!(settings.queries.len(), 2);
        assert_eq!(
            settings.query_for("query:vip").unwrap().sql,
            "SELECT id FROM customers"
        );
        assert!(settings.query_for("vip").is_none());
        assert!(settings.query_for("query:other").is_none());
    }

    #[test]
    fn each_query_rule_is_enforced() {
        let long_sql = format!("SELECT '{}'", "x".repeat(20_000));
        let cases: Vec<(Value, &str)> = vec![
            (json!("select 1"), "must be a list"),
            (json!([query("a", "SELECT 1; SELECT 2;")]), "semicolon"),
            (json!([query("a", "SELECT 1 ;  ")]), "semicolon"),
            (json!([query("a", "DELETE FROM t")]), "SELECT or WITH"),
            (json!([query("a", "SELECTED 1")]), "SELECT or WITH"),
            (json!([query("a", "-- hi\nSELECT 1")]), "SELECT or WITH"),
            (json!([query("a", "SELECT '\0'")]), "NUL"),
            (json!([query("a", "")]), "empty or longer"),
            (json!([query("a", &long_sql)]), "empty or longer"),
            (json!([query("", "SELECT 1")]), "query name"),
            (json!([query("Upper", "SELECT 1")]), "query name"),
            (json!([query("has space", "SELECT 1")]), "query name"),
            (json!([query(&"a".repeat(65), "SELECT 1")]), "query name"),
            (
                json!([query("a", "SELECT 1"), query("a", "SELECT 2")]),
                "one name",
            ),
            (json!([{"name": "a"}]), "without sql"),
            (json!([{"sql": "SELECT 1"}]), "without a name"),
            (
                json!([{"name": "a", "sql": "SELECT 1", "extra": 1}]),
                "other than",
            ),
            (json!([1]), "list of queries"),
        ];
        for (queries, expected) in cases {
            let reason = queries_problem(&queries).unwrap_or_else(|| panic!("{queries}"));
            assert!(reason.contains(expected), "{reason} vs {expected}");
        }
    }

    #[test]
    fn at_most_twenty_queries_are_accepted() {
        let twenty: Vec<Value> = (0..20)
            .map(|n| query(&format!("q{n}"), "SELECT 1"))
            .collect();
        let mut twenty_one = twenty.clone();
        twenty_one.push(query("q20", "SELECT 1"));

        assert_eq!(queries_problem(&json!(twenty)), None);
        assert_eq!(
            queries_problem(&json!(twenty_one)),
            Some("has more than 20 queries")
        );
    }

    #[test]
    fn a_login_needs_both_fields_and_the_error_names_the_missing_one_only() {
        let mut credential = Credential::new();
        credential.insert("user", SecretString::new("reader".into()));

        let error = Login::from_credential(&credential).err().unwrap();
        assert_eq!(error, config_error("password", "is required"));

        credential.insert("password", SecretString::new("pw".into()));
        let login = Login::from_credential(&credential).unwrap();
        assert_eq!((login.user, login.password), ("reader", "pw"));
    }
}
