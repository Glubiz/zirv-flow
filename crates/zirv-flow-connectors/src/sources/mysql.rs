//! The MySQL source: tables and queries of one database, read by keyset in short read-only
//! transactions (architecture 7.2). Connects only to addresses the egress policy allowed, over
//! verified TLS by default, and never forwards a driver or server message.

use std::net::IpAddr;

use futures_util::stream::BoxStream;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Opts, OptsBuilder, Params, SslOpts, Transaction, TxOpts, Value};
use serde_json::Value as Json;

use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, SourceSpec};
use super::sql::connect::{CONNECT_TIMEOUT, backstop, resolve, within};
use super::sql::discover::{from_clause, query_stream, table_streams};
use super::sql::error::{from_mysql, query};
use super::sql::keyset::{
    self, KeySpec, Paged, Pager, Row, batches, key_types as keys_of, parse_checkpoint,
};
use super::sql::metrics::metered;
use super::sql::settings::{Flavor, Login, Settings, TlsMode, check_config, config_error, fields};
use super::sql::tls;
use super::types::{Batch, Checkpoint, FieldDescriptor, FieldType, StreamConfig, StreamDescriptor};

const TYPE: &str = "mysql";
/// The character set number of binary strings.
const BINARY_CHARSET: u16 = 63;

pub(crate) struct Mysql;

impl Source for Mysql {
    const SPEC: SourceSpec = SourceSpec {
        kind: TYPE,
        label: "MySQL",
        description: "Reads tables and queries of one MySQL database, through a read-only user.",
        config_fields: &[
            fields::HOST,
            fields::port("Defaults to 3306."),
            fields::database("The database to connect to; its tables are the streams."),
            fields::TLS,
            fields::CA_CERTIFICATE,
            fields::STATEMENT_TIMEOUT,
            fields::QUERIES,
        ],
        credential_fields: &[
            fields::credential(
                "username",
                "User",
                "A user that can only SELECT what should be read.",
            ),
            fields::credential("password", "Password", "The user's password."),
        ],
        rate_limit: None,
        config_check: Some(|config| check_config(config, Flavor::MySql)),
        capabilities: Capabilities {
            push: false,
            incremental: true,
            full_refresh: true,
            emits_deletes: false,
        },
    };

    async fn check(
        &self,
        ctx: &SourceContext,
        config: &Json,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        metered(TYPE, async {
            let (settings, mut conn) = connect(ctx, config, credential).await?;
            within(backstop(settings.statement_timeout), async {
                let mut tx = begin_batch(&mut conn).await.map_err(failed)?;
                let _: Option<i64> = tx.query_first("SELECT 1").await.map_err(failed)?;
                tx.commit().await.map_err(failed)
            })
            .await
        })
        .await
    }

    async fn discover(
        &self,
        ctx: &SourceContext,
        config: &Json,
        credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        metered(TYPE, async {
            let (settings, mut conn) = connect(ctx, config, credential).await?;
            within(
                backstop(settings.statement_timeout) * 2,
                discover_streams(&mut conn, &settings),
            )
            .await
        })
        .await
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Json,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        batches(
            TYPE,
            open_paged(ctx, config, credential, stream, checkpoint),
        )
    }
}

fn failed(error: mysql_async::Error) -> SourceError {
    from_mysql(TYPE, &error)
}

async fn connect(
    ctx: &SourceContext,
    config: &Json,
    credential: &Credential,
) -> Result<(Settings, mysql_async::Conn), SourceError> {
    let settings = Settings::parse(config, Flavor::MySql)?;
    let login = Login::from_credential(credential)?;
    let conn = open(ctx, &settings, &login).await?;
    Ok((settings, conn))
}

/// Resolves and checks the host, then connects to exactly the checked addresses.
async fn open(
    ctx: &SourceContext,
    settings: &Settings,
    login: &Login<'_>,
) -> Result<mysql_async::Conn, SourceError> {
    let ips = resolve(ctx, settings).await?;
    let opts = options(settings, login, &ips)?;
    // The driver builds its TLS configuration on the process-wide provider; ring is the only one
    // this build has, and another install is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // The driver has no connect timeout of its own.
    within(CONNECT_TIMEOUT, async {
        mysql_async::Conn::new(opts).await.map_err(failed)
    })
    .await
}

/// The driver options. Built in code, never from a URL (the URL form ignores `resolved_ips`) and
/// with no `danger` setting. `resolved_ips` are the checked addresses and the host name stays the
/// TLS name; no Unix socket is preferred and no local-infile handler is set.
fn options(settings: &Settings, login: &Login<'_>, ips: &[IpAddr]) -> Result<Opts, SourceError> {
    let ssl = match settings.tls {
        TlsMode::Off => None,
        TlsMode::Verify => Some(ssl_opts(settings)?),
    };
    let init = vec![
        format!(
            "SET SESSION max_execution_time = {}",
            settings.statement_timeout.as_millis()
        ),
        "SET SESSION TRANSACTION READ ONLY".to_owned(),
        "SET time_zone = '+00:00'".to_owned(),
        "SET NAMES utf8mb4".to_owned(),
    ];
    Ok(OptsBuilder::default()
        .ip_or_hostname(settings.host.clone())
        .tcp_port(settings.port)
        .resolved_ips(Some(ips.to_vec()))
        .prefer_socket(false)
        .user(Some(login.user))
        .pass(Some(login.password))
        .db_name(Some(settings.database.as_str()))
        .ssl_opts(ssl)
        .init(init)
        .into())
}

/// Verification against the configured CA alone, else the compiled-in roots. The same
/// certificates and the same rule as the PostgreSQL source.
fn ssl_opts(settings: &Settings) -> Result<SslOpts, SourceError> {
    let Some(pem) = settings.ca_certificate.as_deref() else {
        return Ok(SslOpts::default());
    };
    // Parsed here as well, so a bad CA is a setting error and not a driver error.
    tls::root_store(Some(pem))?;
    Ok(SslOpts::default()
        .with_root_certs(vec![pem.as_bytes().to_vec().into()])
        .with_disable_built_in_roots(true))
}

/// Starts a transaction that can only read; the server refuses a write inside it (1792). Every
/// statement the source runs goes through one of these.
async fn begin_batch(conn: &mut mysql_async::Conn) -> mysql_async::Result<Transaction<'_>> {
    let mut options = TxOpts::default();
    options.with_readonly(Some(true));
    conn.start_transaction(options).await
}

/// How a column is read and paged on.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Column {
    name: String,
    /// A binary column is not text and arrives as `NULL`.
    binary: bool,
    key: Option<KeyType>,
    field_type: FieldType,
}

/// The types a stream can be paged on. A checkpoint value is bound as what the column holds, so the
/// comparison is exact: integers as integers (a string compared to a BIGINT is compared as a float
/// and loses precision above 2^53, and `CAST(? AS UNSIGNED)` of a string saturates at the signed
/// maximum), decimals as a string cast to a decimal of the column's own scale (so no value of the
/// column overflows the cast), text and temporal types as the string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyType {
    Signed,
    Unsigned,
    /// An exact decimal with this many digits after the point.
    Decimal(u8),
    Text,
    Temporal,
}

impl KeyType {
    /// The placeholder for a checkpoint value of this type. From this fixed table, never from
    /// user text.
    fn placeholder(self) -> String {
        match self {
            Self::Decimal(scale) => format!("CAST(? AS DECIMAL(65,{scale}))"),
            Self::Signed | Self::Unsigned | Self::Text | Self::Temporal => "?".to_owned(),
        }
    }

    /// The parameter for the checkpoint text `text`. A value that is not what the column holds
    /// means the checkpoint was written for other settings.
    fn parameter(self, text: &str) -> Result<Value, SourceError> {
        let stale = || config_error("checkpoint", "does not fit the stream settings");
        Ok(match self {
            Self::Signed => Value::Int(text.parse().map_err(|_| stale())?),
            Self::Unsigned => Value::UInt(text.parse().map_err(|_| stale())?),
            Self::Decimal(_) | Self::Text | Self::Temporal => {
                Value::Bytes(text.as_bytes().to_vec())
            }
        })
    }
}

fn describe_column(
    name: String,
    column_type: ColumnType,
    flags: ColumnFlags,
    charset: u16,
    decimals: u8,
) -> Column {
    use ColumnType::*;
    let binary_charset = charset == BINARY_CHARSET;
    // A DECIMAL has at most 30 digits after the point, which leaves 35 before it in 65.
    let scale = decimals.min(30);
    let textual =
        !binary_charset && !flags.intersects(ColumnFlags::ENUM_FLAG | ColumnFlags::SET_FLAG);
    let (binary, key, field_type) = match column_type {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24
        | MYSQL_TYPE_LONGLONG => (
            false,
            Some(if flags.contains(ColumnFlags::UNSIGNED_FLAG) {
                KeyType::Unsigned
            } else {
                KeyType::Signed
            }),
            FieldType::Number,
        ),
        MYSQL_TYPE_YEAR | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => (false, None, FieldType::Number),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
            (false, Some(KeyType::Decimal(scale)), FieldType::Number)
        }
        MYSQL_TYPE_DATE
        | MYSQL_TYPE_NEWDATE
        | MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => (false, Some(KeyType::Temporal), FieldType::Timestamp),
        MYSQL_TYPE_JSON => (false, None, FieldType::Json),
        MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
            if binary_charset =>
        {
            (true, None, FieldType::Unknown)
        }
        MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB => (false, textual.then_some(KeyType::Text), FieldType::String),
        MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY => (true, None, FieldType::Unknown),
        _ => (false, None, FieldType::Unknown),
    };
    Column {
        name,
        binary,
        key,
        field_type,
    }
}

/// Quotes an identifier: wrapped in backticks, with embedded ones doubled.
fn quote(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

/// The select list: every column as text (one decoding path for every type). Binary columns are
/// not text and arrive as `NULL`.
fn select_list(columns: &[Column]) -> String {
    columns
        .iter()
        .map(|column| {
            let name = quote(&column.name);
            if column.binary {
                format!("NULL AS {name}")
            } else {
                format!("CAST(s.{name} AS CHAR) AS {name}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The page query and, for each `?`, which key value to bind there. `keys` are the key columns
/// in comparison order (the cursor first, when `cursor` is set); the checkpoint values are bound
/// as text parameters, cast on the server for numbers.
fn page_sql(
    from: &str,
    columns: &[Column],
    keys: &[(String, KeyType)],
    cursor: bool,
    after: bool,
) -> (String, Vec<usize>) {
    let mut bindings = Vec::new();
    let sql = keyset::page_sql(
        &select_list(columns),
        from,
        keys,
        cursor,
        after,
        quote,
        |index, column, operator| {
            bindings.push(index);
            format!("{column} {operator} {}", keys[index].1.placeholder())
        },
    );
    (sql, bindings)
}

/// What a stream reads from: a table of the configured database or a configured query.
fn from_stream(settings: &Settings, stream: &str) -> Result<String, SourceError> {
    from_clause(settings, stream, |table| {
        format!("{}.{}", quote(&settings.database), quote(table))
    })
}

/// Prepares `SELECT * FROM <from>` (parsed and planned, not run) to learn the columns.
async fn describe(conn: &mut mysql_async::Conn, from: &str) -> Result<Vec<Column>, SourceError> {
    let mut tx = begin_batch(conn).await.map_err(failed)?;
    let statement = tx
        .prep(format!("SELECT * FROM {from} AS s"))
        .await
        .map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok(statement.columns().iter().map(column_of).collect())
}

fn column_of(column: &mysql_async::Column) -> Column {
    describe_column(
        column.name_str().into_owned(),
        column.column_type(),
        column.flags(),
        column.character_set(),
        column.decimals(),
    )
}

struct MysqlPager {
    conn: mysql_async::Conn,
    statement_timeout: std::time::Duration,
    first: String,
    next: String,
    /// For each `?` of `next`, the key value to bind and the type to bind it as.
    bindings: Vec<(usize, KeyType)>,
}

impl Pager for MysqlPager {
    async fn page(&mut self, after: Option<&[String]>) -> Result<Vec<Row>, SourceError> {
        let (sql, params) = match after {
            None => (self.first.clone(), Params::Empty),
            Some(key) => (
                self.next.clone(),
                Params::Positional(
                    self.bindings
                        .iter()
                        .map(|(index, key_type)| key_type.parameter(&key[*index]))
                        .collect::<Result<_, _>>()?,
                ),
            ),
        };
        within(backstop(self.statement_timeout), async {
            let mut tx = begin_batch(&mut self.conn).await.map_err(failed)?;
            let rows: Vec<mysql_async::Row> = tx.exec(sql, params).await.map_err(failed)?;
            tx.commit().await.map_err(failed)?;
            rows.into_iter()
                .map(|row| row.unwrap().into_iter().map(text_of).collect())
                .collect()
        })
        .await
    }
}

/// A value as text. Every selected column is cast to text on the server, so a value is `NULL` or
/// bytes; anything else, or bytes that are not UTF-8, is a failed query.
fn text_of(value: Value) -> Result<Option<String>, SourceError> {
    match value {
        Value::NULL => Ok(None),
        Value::Bytes(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| query()),
        _ => Err(query()),
    }
}

async fn open_paged(
    ctx: &SourceContext,
    config: &Json,
    credential: &Credential,
    stream: &StreamConfig,
    checkpoint: Option<Checkpoint>,
) -> Result<Paged<MysqlPager>, SourceError> {
    let settings = Settings::parse(config, Flavor::MySql)?;
    let login = Login::from_credential(credential)?;
    let spec = KeySpec::from_stream(stream)?;
    let after = parse_checkpoint(checkpoint.as_ref(), &spec)?;
    let from = from_stream(&settings, &stream.name)?;
    let mut conn = open(ctx, &settings, &login).await?;
    let columns = within(
        backstop(settings.statement_timeout),
        describe(&mut conn, &from),
    )
    .await?;
    let keys = key_types(&columns, &spec)?;
    let cursor = spec.cursor.is_some();
    let (first, _) = page_sql(&from, &columns, &keys, cursor, false);
    let (next, positions) = page_sql(&from, &columns, &keys, cursor, true);
    let bindings = positions
        .into_iter()
        .map(|index| (index, keys[index].1))
        .collect();
    let pager = MysqlPager {
        conn,
        statement_timeout: settings.statement_timeout,
        first,
        next,
        bindings,
    };
    let names = columns.into_iter().map(|column| column.name).collect();
    Paged::new(pager, names, &spec, after)
}

/// The key columns with the type each is paged on.
fn key_types(columns: &[Column], spec: &KeySpec) -> Result<Vec<(String, KeyType)>, SourceError> {
    keys_of(spec, |name| {
        columns
            .iter()
            .find(|column| column.name == name)
            .map(|column| column.key)
    })
}

fn field_type(data_type: &str) -> FieldType {
    match data_type {
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "decimal" | "numeric"
        | "float" | "double" | "year" => FieldType::Number,
        "date" | "datetime" | "timestamp" => FieldType::Timestamp,
        "json" => FieldType::Json,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            FieldType::String
        }
        _ => FieldType::Unknown,
    }
}

async fn discover_streams(
    conn: &mut mysql_async::Conn,
    settings: &Settings,
) -> Result<Vec<StreamDescriptor>, SourceError> {
    let mut tx = begin_batch(conn).await.map_err(failed)?;
    let columns: Vec<(String, String, String)> = tx
        .exec(
            "SELECT c.table_name AS t, c.column_name AS c, c.data_type AS d \
             FROM information_schema.columns c \
             JOIN information_schema.tables t \
               ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
             WHERE c.table_schema = ? AND t.table_type = 'BASE TABLE' \
             ORDER BY c.table_name, c.ordinal_position",
            (settings.database.as_str(),),
        )
        .await
        .map_err(failed)?;
    let keys: Vec<(String, String)> = tx
        .exec(
            "SELECT table_name AS t, column_name AS c \
             FROM information_schema.key_column_usage \
             WHERE table_schema = ? AND constraint_name = 'PRIMARY' \
             ORDER BY table_name, ordinal_position",
            (settings.database.as_str(),),
        )
        .await
        .map_err(failed)?;
    let columns = columns
        .into_iter()
        .map(|(table, column, data_type)| {
            let field = FieldDescriptor {
                name: column,
                field_type: field_type(&data_type.to_lowercase()),
            };
            (table, field)
        })
        .collect::<Vec<_>>();
    let mut streams = table_streams(columns, keys, || {
        config_error(
            "database",
            "has more than 1000 tables; grant the user fewer",
        )
    })?;
    for configured in &settings.queries {
        // Prepared as written, so the columns are the query's own and a second statement is the
        // driver's error.
        let statement = tx
            .prep(configured.sql.as_str())
            .await
            .map_err(|error| rejected_query(&error))?;
        streams.push(query_stream(
            &configured.name,
            statement.columns().iter().map(|column| {
                let column = column_of(column);
                FieldDescriptor {
                    name: column.name,
                    field_type: column.field_type,
                }
            }),
        )?);
    }
    tx.commit().await.map_err(failed)?;
    Ok(streams)
}

/// A query the server would not prepare: a fixed message, with a deadline kept as what it is.
fn rejected_query(error: &mysql_async::Error) -> SourceError {
    let mapped = from_mysql(TYPE, error);
    match mapped {
        SourceError::Other(_) if mapped != query() => mapped,
        SourceError::Other(_) => config_error("queries", "has a query the database rejected"),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::super::sql::error::{QUERY, TIMEOUT, timeout, tls};
    use super::super::sql::testing::{
        context, credential, fixed, keys, loopback_context, pki, read_all, required_env, suffix,
    };
    use super::*;
    use crate::sources::SyncMode;

    fn column(name: &str, column_type: ColumnType, flags: ColumnFlags, charset: u16) -> Column {
        describe_column(name.to_owned(), column_type, flags, charset, 0)
    }

    fn keys_of(names: &[(&str, KeyType)]) -> Vec<(String, KeyType)> {
        names.iter().map(|(n, t)| ((*n).to_owned(), *t)).collect()
    }

    fn cols() -> Vec<Column> {
        vec![
            column(
                "id",
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColumnFlags::empty(),
                63,
            ),
            column(
                "updated_at",
                ColumnType::MYSQL_TYPE_DATETIME,
                ColumnFlags::empty(),
                63,
            ),
            column(
                "payload",
                ColumnType::MYSQL_TYPE_BLOB,
                ColumnFlags::BINARY_FLAG,
                63,
            ),
        ]
    }

    const SELECT: &str = "SELECT CAST(s.`id` AS CHAR) AS `id`, CAST(s.`updated_at` AS CHAR) AS `updated_at`, NULL AS `payload`";

    #[test]
    fn the_first_incremental_page_skips_null_cursors_and_orders_by_cursor_then_key() {
        let (sql, bindings) = page_sql(
            "`d`.`contacts`",
            &cols(),
            &keys_of(&[("updated_at", KeyType::Temporal), ("id", KeyType::Signed)]),
            true,
            false,
        );

        assert_eq!(
            sql,
            format!(
                "{SELECT} FROM `d`.`contacts` AS s WHERE s.`updated_at` IS NOT NULL ORDER BY s.`updated_at`, s.`id` LIMIT 500"
            )
        );
        assert!(bindings.is_empty());
    }

    #[test]
    fn an_incremental_page_after_a_checkpoint_compares_the_expanded_key() {
        let (sql, bindings) = page_sql(
            "`d`.`contacts`",
            &cols(),
            &keys_of(&[("updated_at", KeyType::Temporal), ("id", KeyType::Signed)]),
            true,
            true,
        );

        assert_eq!(
            sql,
            format!(
                "{SELECT} FROM `d`.`contacts` AS s WHERE s.`updated_at` IS NOT NULL AND (s.`updated_at` > ? OR (s.`updated_at` = ? AND s.`id` > ?)) ORDER BY s.`updated_at`, s.`id` LIMIT 500"
            )
        );
        // One binding per `?`: the cursor twice (once to compare, once to tie), then the key.
        assert_eq!(bindings, [0, 0, 1]);
    }

    #[test]
    fn a_full_refresh_orders_by_the_key_alone_and_every_key_type_has_its_placeholder() {
        let (first, _) = page_sql(
            "t",
            &cols()[..1],
            &keys_of(&[("id", KeyType::Signed)]),
            false,
            false,
        );
        let (composite, bindings) = page_sql(
            "t",
            &cols()[..1],
            &keys_of(&[
                ("a", KeyType::Unsigned),
                ("b", KeyType::Decimal(4)),
                ("c", KeyType::Text),
            ]),
            false,
            true,
        );

        assert_eq!(
            first,
            "SELECT CAST(s.`id` AS CHAR) AS `id` FROM t AS s ORDER BY s.`id` LIMIT 500"
        );
        assert!(
            composite.contains(
                "WHERE (s.`a` > ? OR (s.`a` = ? AND s.`b` > CAST(? AS DECIMAL(65,4))) OR (s.`a` = ? AND s.`b` = CAST(? AS DECIMAL(65,4)) AND s.`c` > ?))"
            ),
            "{composite}"
        );
        assert_eq!(bindings, [0, 0, 1, 0, 1, 2]);
    }

    #[test]
    fn a_decimal_is_cast_with_the_columns_own_scale_so_every_integer_digit_fits() {
        use ColumnType::MYSQL_TYPE_NEWDECIMAL;
        let none = ColumnFlags::empty();
        let placeholder = |scale| {
            describe_column("x".to_owned(), MYSQL_TYPE_NEWDECIMAL, none, 63, scale)
                .key
                .unwrap()
                .placeholder()
        };

        // DECIMAL(65,0) keeps all 65 integer digits; the old fixed (65,30) kept 35.
        assert_eq!(placeholder(0), "CAST(? AS DECIMAL(65,0))");
        assert_eq!(placeholder(4), "CAST(? AS DECIMAL(65,4))");
        assert_eq!(placeholder(30), "CAST(? AS DECIMAL(65,30))");
        // A scale the server cannot declare is clamped to its maximum.
        assert_eq!(placeholder(31), "CAST(? AS DECIMAL(65,30))");
    }

    #[test]
    fn an_identifier_with_backticks_is_quoted_by_doubling_them() {
        assert_eq!(quote("we`ird"), "`we``ird`");
        let (sql, _) = page_sql(
            &quote("t`; DROP TABLE x; --"),
            &[column(
                "i`d",
                ColumnType::MYSQL_TYPE_LONG,
                ColumnFlags::empty(),
                63,
            )],
            &keys_of(&[("i`d", KeyType::Signed)]),
            false,
            false,
        );
        assert!(
            sql.contains("CAST(s.`i``d` AS CHAR) AS `i``d` FROM `t``; DROP TABLE x; --` AS s"),
            "{sql}"
        );
    }

    #[test]
    fn column_types_decide_the_key_type_the_field_type_and_whether_a_column_is_binary() {
        use ColumnType::*;
        let none = ColumnFlags::empty();
        let key_of = |t, f, c| column("x", t, f, c).key;
        assert_eq!(key_of(MYSQL_TYPE_LONG, none, 63), Some(KeyType::Signed));
        assert_eq!(
            key_of(MYSQL_TYPE_LONGLONG, ColumnFlags::UNSIGNED_FLAG, 63),
            Some(KeyType::Unsigned)
        );
        assert_eq!(
            key_of(MYSQL_TYPE_NEWDECIMAL, none, 63),
            Some(KeyType::Decimal(0))
        );
        assert_eq!(
            describe_column("x".to_owned(), MYSQL_TYPE_NEWDECIMAL, none, 63, 7).key,
            Some(KeyType::Decimal(7))
        );
        // ENUM and SET are ordered by index on the server, not as text, so they are not key types.
        for flag in [ColumnFlags::ENUM_FLAG, ColumnFlags::SET_FLAG] {
            assert_eq!(key_of(MYSQL_TYPE_STRING, flag, 255), None, "{flag:?}");
        }
        assert_eq!(
            key_of(MYSQL_TYPE_VAR_STRING, none, 255),
            Some(KeyType::Text)
        );
        assert_eq!(key_of(MYSQL_TYPE_BLOB, none, 255), Some(KeyType::Text));
        for temporal in [MYSQL_TYPE_DATE, MYSQL_TYPE_DATETIME, MYSQL_TYPE_TIMESTAMP] {
            assert_eq!(key_of(temporal, none, 63), Some(KeyType::Temporal));
        }
        for unsupported in [
            MYSQL_TYPE_DOUBLE,
            MYSQL_TYPE_FLOAT,
            MYSQL_TYPE_JSON,
            MYSQL_TYPE_TIME,
        ] {
            assert_eq!(key_of(unsupported, none, 63), None, "{unsupported:?}");
        }
        // Binary strings and blobs are not text and are not key types.
        assert!(column("x", MYSQL_TYPE_BLOB, none, 63).binary);
        assert!(column("x", MYSQL_TYPE_VAR_STRING, none, 63).binary);
        assert_eq!(key_of(MYSQL_TYPE_BLOB, none, 63), None);
        assert!(!column("x", MYSQL_TYPE_VAR_STRING, none, 255).binary);
        assert_eq!(
            column("x", MYSQL_TYPE_JSON, none, 255).field_type,
            FieldType::Json
        );
        assert_eq!(
            column("x", MYSQL_TYPE_TIMESTAMP, none, 63).field_type,
            FieldType::Timestamp
        );
        assert_eq!(
            column("x", MYSQL_TYPE_DOUBLE, none, 63).field_type,
            FieldType::Number
        );
        assert_eq!(
            column("x", MYSQL_TYPE_VAR_STRING, none, 255).field_type,
            FieldType::String
        );
    }

    #[test]
    fn a_key_the_stream_lacks_or_cannot_be_paged_on_is_a_config_error() {
        let columns = vec![
            column("id", ColumnType::MYSQL_TYPE_LONG, ColumnFlags::empty(), 63),
            column(
                "score",
                ColumnType::MYSQL_TYPE_DOUBLE,
                ColumnFlags::empty(),
                63,
            ),
        ];
        let float_cursor = KeySpec {
            cursor: Some("score".into()),
            primary_key: vec!["id".into()],
        };
        let missing = KeySpec {
            cursor: None,
            primary_key: vec!["gone".into()],
        };

        assert_eq!(
            key_types(&columns, &float_cursor).unwrap_err(),
            config_error("cursor_field", "has a type that cannot be paged on")
        );
        assert_eq!(
            key_types(&columns, &missing).unwrap_err(),
            config_error("primary_key", "names a column the stream does not have")
        );
    }

    #[test]
    fn data_types_map_to_field_types() {
        assert_eq!(field_type("bigint"), FieldType::Number);
        assert_eq!(field_type("decimal"), FieldType::Number);
        assert_eq!(field_type("datetime"), FieldType::Timestamp);
        assert_eq!(field_type("date"), FieldType::Timestamp);
        assert_eq!(field_type("json"), FieldType::Json);
        assert_eq!(field_type("varchar"), FieldType::String);
        assert_eq!(field_type("text"), FieldType::String);
        assert_eq!(field_type("blob"), FieldType::Unknown);
    }

    #[test]
    fn a_table_is_qualified_by_the_database() {
        let settings =
            Settings::parse(&json!({"host": "h", "database": "sh`op"}), Flavor::MySql).unwrap();

        assert_eq!(
            from_stream(&settings, "contacts").unwrap(),
            "`sh``op`.`contacts`"
        );
    }

    fn login() -> Login<'static> {
        Login {
            user: "reader",
            password: "pw",
        }
    }

    fn settings(extra: Json) -> Settings {
        let mut config = json!({"host": "db.test", "database": "shop"});
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        Settings::parse(&config, Flavor::MySql).unwrap()
    }

    #[test]
    fn the_options_dial_the_checked_addresses_keep_the_name_and_use_no_socket_or_infile() {
        let ips: Vec<IpAddr> = vec!["10.0.0.5".parse().unwrap(), "10.0.0.6".parse().unwrap()];

        let opts = options(&settings(json!({})), &login(), &ips).unwrap();

        assert_eq!(opts.ip_or_hostname(), "db.test");
        assert_eq!(opts.resolved_ips().as_deref(), Some(ips.as_slice()));
        assert!(!opts.prefer_socket());
        assert!(opts.local_infile_handler().is_none());
        assert_eq!(opts.tcp_port(), 3306);
        let ssl = opts.ssl_opts().expect("tls is verified by default");
        assert!(!ssl.skip_domain_validation() && !ssl.accept_invalid_certs());
        assert!(ssl.tls_hostname_override().is_none());
        assert!(!ssl.disable_built_in_roots() && ssl.root_certs().is_empty());
    }

    #[test]
    fn a_configured_ca_is_the_only_root_and_tls_off_is_the_only_way_to_no_tls() {
        let ips: Vec<IpAddr> = vec!["10.0.0.5".parse().unwrap()];
        let pki = pki("db.test");

        let with_ca = options(
            &settings(json!({"ca_certificate": pki.ca_pem})),
            &login(),
            &ips,
        )
        .unwrap();
        let off = options(&settings(json!({"tls": "off"})), &login(), &ips).unwrap();

        let ssl = with_ca.ssl_opts().unwrap();
        assert!(ssl.disable_built_in_roots());
        assert_eq!(ssl.root_certs().len(), 1);
        assert!(off.ssl_opts().is_none());
    }

    #[test]
    fn every_connection_starts_read_only_in_utc_with_a_statement_timeout() {
        let opts = options(
            &settings(json!({"statement_timeout_seconds": 7})),
            &login(),
            &["10.0.0.5".parse().unwrap()],
        )
        .unwrap();

        assert_eq!(
            opts.init(),
            [
                "SET SESSION max_execution_time = 7000",
                "SET SESSION TRANSACTION READ ONLY",
                "SET time_zone = '+00:00'",
                "SET NAMES utf8mb4",
            ]
        );
    }

    #[test]
    fn the_source_is_declared_with_secret_credentials_and_the_documented_settings() {
        let spec = &Mysql::SPEC;

        assert_eq!(spec.kind, "mysql");
        assert!(spec.credential_fields.iter().all(|field| field.secret));
        let keys: Vec<_> = spec.config_fields.iter().map(|field| field.key).collect();
        assert_eq!(
            keys,
            [
                "host",
                "port",
                "database",
                "tls",
                "ca_certificate",
                "statement_timeout_seconds",
                "queries"
            ]
        );
        assert!(!spec.capabilities.emits_deletes);
    }

    #[test]
    fn a_value_is_null_or_utf8_text_and_nothing_else() {
        assert_eq!(text_of(Value::NULL), Ok(None));
        assert_eq!(
            text_of(Value::Bytes(b"abc".to_vec())),
            Ok(Some("abc".into()))
        );
        assert_eq!(text_of(Value::Bytes(vec![0xff, 0xfe])), Err(query()));
        assert_eq!(text_of(Value::Int(1)), Err(query()));
    }

    // --- egress and TLS at the connect point, against in-process servers -------------------

    fn source_config(port: u16, extra: Json) -> Json {
        let mut config = json!({"host": "db.test", "database": "shop", "port": port});
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        config
    }

    #[tokio::test]
    async fn a_private_address_is_refused_before_any_connection() {
        let ctx = context(TYPE, &[], fixed(&["10.0.0.5"]));
        let started = Instant::now();

        let error = Mysql
            .check(&ctx, &source_config(3306, json!({})), &credential("u", "p"))
            .await
            .unwrap_err();

        assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_mixed_answer_is_refused_whole() {
        let ctx = context(TYPE, &[], fixed(&["8.8.8.8", "10.0.0.5"]));

        let error = Mysql
            .discover(&ctx, &source_config(3306, json!({})), &credential("u", "p"))
            .await
            .unwrap_err();

        assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_allowlisted_address_is_dialled_exactly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = tokio::spawn(async move { listener.accept().await.unwrap().1.ip() });
        let ctx = context(TYPE, &["127.0.0.1/32"], fixed(&["127.0.0.1"]));

        let result = Mysql
            .check(&ctx, &source_config(port, json!({})), &credential("u", "p"))
            .await;

        assert_eq!(
            accepted.await.unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
        assert!(result.is_err(), "the listener drops the connection");
    }

    #[tokio::test]
    async fn rebinding_dials_only_the_checked_address_and_rechecks_on_reconnect() {
        use crate::http::testing::Rebinding;

        for later in ["10.0.0.5", "169.254.169.254"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let accepted = async {
                let (stream, _) = listener.accept().await.unwrap();
                // The local endpoint is the destination; the peer is the client's address.
                assert_eq!(stream.local_addr().unwrap(), address);
            };
            let lookup = std::sync::Arc::new(Rebinding::new("127.0.0.1", later));
            let ctx = context(TYPE, &["127.0.0.1/32"], lookup.clone());
            let config = source_config(address.port(), serde_json::json!({}));
            let login = credential("u", "p");
            let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(Mysql.check(&ctx, &config, &login), accepted)
            })
            .await
            .expect("the driver must dial the checked listener");
            assert!(
                result.is_err(),
                "the listener closes without a database handshake"
            );
            assert_eq!(lookup.calls(), 1);
            assert!(matches!(
                Mysql.check(&ctx, &config, &login).await,
                Err(SourceError::Egress(_))
            ));
            assert_eq!(lookup.calls(), 2);
            assert!(
                tokio::time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn a_refusal_is_counted_in_the_egress_denied_metric() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        let ctx = context(TYPE, &[], fixed(&["10.0.0.5"]));

        let _ = Mysql
            .check(&ctx, &source_config(3306, json!({})), &credential("u", "p"))
            .await;

        let rendered = handle.render();
        assert!(
            rendered.contains("zirv_flow_egress_denied_total{class=\"private\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "zirv_flow_source_db_errors_total{source_type=\"mysql\",reason=\"egress\"} 1"
            ),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn tls_off_toward_a_public_address_is_refused_before_any_connection() {
        let ctx = context(TYPE, &[], fixed(&["8.8.8.8"]));

        let error = Mysql
            .check(
                &ctx,
                &source_config(3306, json!({"tls": "off"})),
                &credential("u", "p"),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            config_error(
                "tls",
                "cannot be off toward a public address; a public address always needs TLS, even when allowlisted"
            )
        );
    }

    /// What the fake server saw.
    #[derive(Debug, Default)]
    struct Seen {
        peer: Option<IpAddr>,
        handshake: bool,
        sni: Option<String>,
        /// Bytes the client sent after a handshake that offered no TLS: its credentials.
        plaintext_after: usize,
    }

    const CLIENT_SSL: u32 = 0x0800;

    /// The server's initial handshake (protocol 10), announcing TLS support when `ssl` is set.
    fn handshake_packet(ssl: bool) -> Vec<u8> {
        let capabilities: u32 = 0x0001 // long password
            | 0x0004 // long flag
            | 0x0008 // connect with db
            | 0x0200 // protocol 4.1
            | 0x2000 // transactions
            | 0x8000 // secure connection
            | 0x0008_0000 // plugin auth
            | if ssl { CLIENT_SSL } else { 0 };
        let mut payload = vec![10];
        payload.extend_from_slice(b"8.4.0\0");
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(b"abcdefgh");
        payload.push(0);
        payload.extend_from_slice(&((capabilities & 0xffff) as u16).to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&2u16.to_le_bytes());
        payload.extend_from_slice(&((capabilities >> 16) as u16).to_le_bytes());
        payload.push(21);
        payload.extend_from_slice(&[0; 10]);
        payload.extend_from_slice(b"ijklmnopqrst\0");
        payload.extend_from_slice(b"mysql_native_password\0");
        let mut packet = (payload.len() as u32).to_le_bytes()[..3].to_vec();
        packet.push(0);
        packet.extend(payload);
        packet
    }

    /// A MySQL server that sends a handshake and, when it offers TLS, speaks it.
    async fn fake_server(acceptor: Option<TlsAcceptor>) -> (u16, tokio::task::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.unwrap();
            let mut seen = Seen {
                peer: Some(peer.ip()),
                ..Seen::default()
            };
            stream
                .write_all(&handshake_packet(acceptor.is_some()))
                .await
                .unwrap();
            let mut buffer = [0u8; 1024];
            match acceptor {
                Some(acceptor) => {
                    // The SSL request: a 4-byte packet header and 32 bytes.
                    let mut request = [0u8; 36];
                    stream.read_exact(&mut request).await.unwrap();
                    if let Ok(mut secured) = acceptor.accept(stream).await {
                        seen.handshake = true;
                        seen.sni = secured.get_ref().1.server_name().map(str::to_owned);
                        let _ = secured.read(&mut buffer).await;
                    }
                }
                None => seen.plaintext_after = stream.read(&mut buffer).await.unwrap_or(0),
            }
            seen
        });
        (port, handle)
    }

    async fn check_against(port: u16, extra: Json) -> Result<(), SourceError> {
        let ctx = context(TYPE, &["127.0.0.1/32"], fixed(&["127.0.0.1"]));
        tokio::time::timeout(
            Duration::from_secs(10),
            Mysql.check(&ctx, &source_config(port, extra), &credential("u", "p")),
        )
        .await
        .expect("the check must not hang")
    }

    #[tokio::test]
    async fn the_checked_ip_is_dialled_and_the_host_name_is_the_verified_tls_name() {
        let pki = pki("db.test");
        let (port, server) = fake_server(Some(pki.acceptor.clone())).await;

        let result = check_against(port, json!({"ca_certificate": pki.ca_pem})).await;

        let seen = server.await.unwrap();
        assert_eq!(seen.peer, Some("127.0.0.1".parse().unwrap()));
        assert!(seen.handshake, "{seen:?} {result:?}");
        assert_eq!(seen.sni.as_deref(), Some("db.test"));
        assert!(
            result.is_err(),
            "the fake server closes after the handshake"
        );
    }

    #[tokio::test]
    async fn a_certificate_for_another_name_fails_the_handshake() {
        let pki = pki("other.test");
        let (port, server) = fake_server(Some(pki.acceptor.clone())).await;

        let result = check_against(port, json!({"ca_certificate": pki.ca_pem})).await;

        assert!(!server.await.unwrap().handshake);
        assert_eq!(result, Err(tls()));
    }

    #[tokio::test]
    async fn a_certificate_from_an_untrusted_ca_fails_the_handshake() {
        // The CA is not configured, so only the public roots are trusted.
        let pki = pki("db.test");
        let (port, server) = fake_server(Some(pki.acceptor.clone())).await;

        let result = check_against(port, json!({})).await;

        assert!(!server.await.unwrap().handshake);
        assert_eq!(result, Err(tls()));
    }

    #[tokio::test]
    async fn a_server_that_offers_no_tls_is_refused_and_no_credentials_are_sent() {
        let (port, server) = fake_server(None).await;

        let result = check_against(port, json!({})).await;

        let seen = server.await.unwrap();
        assert_eq!(seen.plaintext_after, 0, "the login must not be sent");
        assert_eq!(result, Err(tls()));
    }

    // --- the fixture suite, against a running MySQL (ZIRV_FLOW_TEST_MYSQL_URL) --------------

    #[derive(Clone)]
    struct Admin {
        host: String,
        port: u16,
        user: String,
        password: String,
    }

    fn admin() -> Admin {
        let url = required_env("ZIRV_FLOW_TEST_MYSQL_URL");
        let opts = Opts::from_url(&url).expect("ZIRV_FLOW_TEST_MYSQL_URL must parse");
        Admin {
            host: opts.ip_or_hostname().to_owned(),
            port: opts.tcp_port(),
            user: opts.user().unwrap_or("root").to_owned(),
            password: opts.pass().unwrap_or_default().to_owned(),
        }
    }

    async fn admin_exec(admin: &Admin, script: &str) {
        let opts = OptsBuilder::default()
            .ip_or_hostname(admin.host.clone())
            .tcp_port(admin.port)
            .user(Some(admin.user.clone()))
            .pass(Some(admin.password.clone()));
        let mut conn = mysql_async::Conn::new(opts).await.expect("admin connect");
        for statement in script.replace("\r\n", "\n").split(";\n") {
            if statement.trim().is_empty() {
                continue;
            }
            conn.query_drop(statement).await.expect("admin statement");
        }
        conn.disconnect().await.expect("admin disconnect");
    }

    /// A seeded database and a SELECT-only user, dropped when the test ends.
    struct Fixture {
        admin: Admin,
        database: String,
        user: String,
        ctx: SourceContext,
    }

    impl Fixture {
        async fn new() -> Self {
            let admin = admin();
            let id = suffix();
            let (database, user) = (format!("zf_{id}"), format!("zf_ro_{id}"));
            let seed = include_str!("../../tests/fixtures/mysql_seed.sql")
                .replace("__DB__", &database)
                .replace("__USER__", &user);
            admin_exec(&admin, &seed).await;
            Self {
                admin,
                database,
                user,
                ctx: loopback_context(TYPE),
            }
        }

        fn config(&self, extra: Json) -> Json {
            let mut config = json!({
                "host": self.admin.host,
                "port": self.admin.port,
                "database": self.database,
                "tls": "off",
            });
            for (key, value) in extra.as_object().unwrap() {
                config[key] = value.clone();
            }
            config
        }

        fn reader(&self) -> Credential {
            credential(&self.user, "zf_test_pw")
        }

        async fn read(
            &self,
            config: &Json,
            stream: &StreamConfig,
            checkpoint: Option<Checkpoint>,
        ) -> Result<Vec<Batch>, SourceError> {
            read_all(
                &Mysql,
                &self.ctx,
                config,
                &self.reader(),
                stream,
                checkpoint,
            )
            .await
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let (admin, database, user) =
                (self.admin.clone(), self.database.clone(), self.user.clone());
            let _ = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(admin_exec(
                        &admin,
                        &format!(
                            "DROP DATABASE IF EXISTS {database};\n\
                             DROP DATABASE IF EXISTS {database}_other;\n\
                             DROP USER IF EXISTS '{user}'@'%';\n"
                        ),
                    ));
            })
            .join();
        }
    }

    fn stream(name: &str, mode: SyncMode, cursor: Option<&str>, key: &[&str]) -> StreamConfig {
        StreamConfig {
            name: name.to_owned(),
            sync_mode: mode,
            cursor_field: cursor.map(str::to_owned),
            primary_key: key.iter().map(|k| (*k).to_owned()).collect(),
        }
    }

    fn contacts_full() -> StreamConfig {
        stream("contacts", SyncMode::FullRefresh, None, &["id"])
    }

    fn contacts_incremental() -> StreamConfig {
        stream(
            "contacts",
            SyncMode::Incremental,
            Some("updated_at"),
            &["id"],
        )
    }

    fn ids(range: std::ops::RangeInclusive<u32>) -> Vec<String> {
        range.map(|id| id.to_string()).collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_full_refresh_reads_every_row_once_in_key_order_in_batches_of_at_most_500() {
        let fixture = Fixture::new().await;

        let batches = fixture
            .read(&fixture.config(json!({})), &contacts_full(), None)
            .await
            .unwrap();

        let sizes: Vec<usize> = batches.iter().map(|b| b.records.len()).collect();
        assert_eq!(sizes, [500, 500, 200]);
        assert_eq!(keys(&batches), ids(1..=1200));
        let first = &batches[0].records[0];
        assert_eq!(first.data["id"], "1");
        assert_eq!(first.data["email"], "user1@example.com");
        assert_eq!(
            first.data["payload"],
            Json::Null,
            "binary columns arrive as null"
        );
        assert_eq!(
            batches[0].records[7].data["updated_at"],
            "2024-01-01 00:08:00.000000"
        );
        assert!(!first.deleted);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn an_incremental_read_skips_the_null_cursor_row_a_full_refresh_has() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));

        let incremental = fixture
            .read(&config, &contacts_incremental(), None)
            .await
            .unwrap();
        let full = fixture.read(&config, &contacts_full(), None).await.unwrap();

        assert_eq!(keys(&incremental), ids(1..=1199));
        assert!(keys(&full).contains(&"1200".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn sixteen_rows_sharing_a_cursor_across_a_batch_boundary_are_neither_skipped_nor_repeated()
     {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));

        let batches = fixture
            .read(&config, &contacts_incremental(), None)
            .await
            .unwrap();

        // The first batch ends inside the 16 equal cursor values (ids 493 to 508).
        assert_eq!(batches[0].records.last().unwrap().key, "500");
        assert_eq!(
            batches[0].checkpoint.0,
            json!({"v": 1, "key": ["2024-01-01 08:13:00.000000", "500"]})
        );
        assert_eq!(keys(&batches), ids(1..=1199));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn resuming_from_the_checkpoint_of_every_batch_returns_exactly_the_remaining_rows() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));
        for stream in [contacts_incremental(), contacts_full()] {
            let all = fixture.read(&config, &stream, None).await.unwrap();
            let all_keys = keys(&all);
            let mut seen = 0;
            for batch in &all {
                seen += batch.records.len();

                let rest = fixture
                    .read(&config, &stream, Some(batch.checkpoint.clone()))
                    .await
                    .unwrap();

                assert_eq!(
                    keys(&rest),
                    all_keys[seen..],
                    "{:?} after {seen}",
                    stream.sync_mode
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_composite_primary_key_pages_and_keys_by_all_its_columns() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));
        let full = stream("pairs", SyncMode::FullRefresh, None, &["region", "code"]);
        let incremental = stream(
            "pairs",
            SyncMode::Incremental,
            Some("updated_at"),
            &["region", "code"],
        );

        let batches = fixture.read(&config, &full, None).await.unwrap();
        let by_cursor = fixture.read(&config, &incremental, None).await.unwrap();

        assert_eq!(keys(&batches).len(), 13);
        assert_eq!(keys(&batches)[0], r#"["eu","1"]"#);
        assert_eq!(keys(&batches)[11], r#"["us","6"]"#);
        assert_eq!(keys(&batches)[12], r#"["zz","1"]"#);
        assert!(
            !keys(&by_cursor).contains(&r#"["zz","1"]"#.to_owned()),
            "a NULL cursor is not read"
        );
        assert_eq!(batches[0].checkpoint.0, json!({"v": 1, "key": ["zz", "1"]}));
        assert_eq!(
            keys(&by_cursor)[..3],
            [r#"["eu","1"]"#, r#"["us","1"]"#, r#"["eu","2"]"#]
        );
        let after = Checkpoint(json!({"v": 1, "key": ["2024-02-01 05:00:00", "us", "5"]}));
        let rest = fixture
            .read(&config, &incremental, Some(after))
            .await
            .unwrap();
        assert_eq!(keys(&rest), [r#"["eu","6"]"#, r#"["us","6"]"#]);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn an_unsigned_key_beyond_two_to_the_fifty_third_is_compared_exactly() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));
        let numbers = stream("numbers", SyncMode::FullRefresh, None, &["big"]);
        let decimals = stream("numbers", SyncMode::Incremental, Some("price"), &["big"]);

        let all = fixture.read(&config, &numbers, None).await.unwrap();
        let after = Checkpoint(json!({"v": 1, "key": ["9007199254740992"]}));
        let rest = fixture.read(&config, &numbers, Some(after)).await.unwrap();
        let by_price = fixture.read(&config, &decimals, None).await.unwrap();
        let after_price = Checkpoint(json!({"v": 1, "key": ["1.5000", "18446744073709551615"]}));
        let rest_price = fixture
            .read(&config, &decimals, Some(after_price))
            .await
            .unwrap();

        assert_eq!(
            keys(&all),
            [
                "9007199254740992",
                "9007199254740993",
                "18446744073709551615"
            ]
        );
        // Compared as floats, 9007199254740993 would not be greater than ...992 and be skipped.
        assert_eq!(keys(&rest), ["9007199254740993", "18446744073709551615"]);
        assert_eq!(
            keys(&by_price),
            [
                "18446744073709551615",
                "9007199254740993",
                "9007199254740992"
            ]
        );
        assert_eq!(keys(&rest_price), ["9007199254740993", "9007199254740992"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_stream_without_a_primary_key_or_with_a_stale_checkpoint_fails_instead_of_guessing() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));

        let no_key = fixture
            .read(
                &config,
                &stream("no_key", SyncMode::FullRefresh, None, &[]),
                None,
            )
            .await;
        let stale = fixture
            .read(
                &config,
                &contacts_incremental(),
                Some(Checkpoint(json!({"v": 1, "key": ["7"]}))),
            )
            .await;

        assert_eq!(
            no_key.unwrap_err(),
            config_error("primary_key", "is required")
        );
        assert_eq!(
            stale.unwrap_err(),
            config_error("checkpoint", "does not fit the stream settings")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_dropped_cursor_column_fails_the_read_with_a_fixed_message() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));
        let gone = stream("contacts", SyncMode::Incremental, Some("no_such"), &["id"]);

        let error = fixture.read(&config, &gone, None).await.unwrap_err();

        assert_eq!(
            error,
            config_error("cursor_field", "names a column the stream does not have")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_key_of_a_type_that_cannot_be_paged_on_is_refused() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "queries": [{"name": "floaty", "sql": format!("SELECT id, CAST(id AS DOUBLE) AS score FROM {}.contacts", fixture.database)}]
        }));
        let float_cursor = stream(
            "query:floaty",
            SyncMode::Incremental,
            Some("score"),
            &["id"],
        );
        let blob_key = stream("contacts", SyncMode::FullRefresh, None, &["payload"]);

        let on_cursor = fixture.read(&config, &float_cursor, None).await;
        let on_key = fixture.read(&config, &blob_key, None).await;

        assert_eq!(
            on_cursor.unwrap_err(),
            config_error("cursor_field", "has a type that cannot be paged on")
        );
        assert_eq!(
            on_key.unwrap_err(),
            config_error("primary_key", "has a type that cannot be paged on")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_decimal_key_above_ten_to_the_thirty_fifth_pages_across_a_batch_boundary_once_each() {
        let fixture = Fixture::new().await;
        admin_exec(
            &fixture.admin,
            &format!(
                "SET SESSION cte_max_recursion_depth = 5000;\n\
                 CREATE TABLE {db}.wide (k DECIMAL(65,0) NOT NULL PRIMARY KEY);\n\
                 INSERT INTO {db}.wide\n\
                 WITH RECURSIVE seq (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1200)\n\
                 SELECT CAST('90000000000000000000000000000000000000000000000000' AS DECIMAL(65,0)) + n FROM seq;\n",
                db = fixture.database
            ),
        )
        .await;
        let config = fixture.config(json!({}));
        let wide = stream("wide", SyncMode::FullRefresh, None, &["k"]);

        let batches = fixture.read(&config, &wide, None).await.unwrap();

        let expected: Vec<String> = (1..=1200u32).map(|n| format!("9{n:049}")).collect();
        assert_eq!(batches.len(), 3);
        assert_eq!(keys(&batches), expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn an_enum_or_set_cursor_or_key_is_refused() {
        let fixture = Fixture::new().await;
        admin_exec(
            &fixture.admin,
            &format!(
                "CREATE TABLE {db}.kinds (id BIGINT NOT NULL PRIMARY KEY, kind ENUM('b','a') NOT NULL, tags SET('x','y') NOT NULL);\n\
                 INSERT INTO {db}.kinds VALUES (1, 'a', 'x'), (2, 'b', 'y');\n",
                db = fixture.database
            ),
        )
        .await;
        let config = fixture.config(json!({}));
        let enum_cursor = stream("kinds", SyncMode::Incremental, Some("kind"), &["id"]);
        let set_cursor = stream("kinds", SyncMode::Incremental, Some("tags"), &["id"]);
        let enum_key = stream("kinds", SyncMode::FullRefresh, None, &["kind"]);

        let on_enum = fixture.read(&config, &enum_cursor, None).await;
        let on_set = fixture.read(&config, &set_cursor, None).await;
        let on_key = fixture.read(&config, &enum_key, None).await;

        let cursor = config_error("cursor_field", "has a type that cannot be paged on");
        assert_eq!(on_enum.unwrap_err(), cursor);
        assert_eq!(on_set.unwrap_err(), cursor);
        assert_eq!(
            on_key.unwrap_err(),
            config_error("primary_key", "has a type that cannot be paged on")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn discovery_lists_tables_with_columns_types_keys_and_the_default_cursor() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "queries": [{"name": "recent", "sql": format!("SELECT id, email, updated_at FROM {}.contacts", fixture.database)}]
        }));

        let streams = Mysql
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap();

        let names: Vec<_> = streams.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["contacts", "no_key", "numbers", "pairs", "query:recent"]
        );
        let contacts = &streams[0];
        let fields: Vec<_> = contacts
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.field_type))
            .collect();
        assert_eq!(
            fields,
            [
                ("id", FieldType::Number),
                ("email", FieldType::String),
                ("updated_at", FieldType::Timestamp),
                ("note", FieldType::String),
                ("payload", FieldType::Unknown),
            ]
        );
        assert_eq!(contacts.default_primary_key, ["id"]);
        assert_eq!(contacts.default_cursor_field.as_deref(), Some("updated_at"));
        assert_eq!(streams[1].default_primary_key, Vec::<String>::new());
        assert_eq!(streams[3].default_primary_key, ["region", "code"]);
        assert_eq!(
            streams[3].default_cursor_field.as_deref(),
            Some("updated_at")
        );
        let query = &streams[4];
        assert_eq!(query.fields.len(), 3);
        assert_eq!(query.fields[2].field_type, FieldType::Timestamp);
        assert!(query.default_primary_key.is_empty() && query.default_cursor_field.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_bad_query_is_described_with_a_fixed_message_that_does_not_repeat_it() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "queries": [{"name": "bad", "sql": "SELECT sentinel_column FROM nowhere"}]
        }));

        let error = Mysql
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap_err();

        assert_eq!(
            error,
            config_error("queries", "has a query the database rejected")
        );
        assert!(!error.to_string().contains("sentinel_column"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_query_that_returns_a_column_name_twice_is_refused() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "queries": [{"name": "dup", "sql": format!("SELECT id, id FROM {}.contacts", fixture.database)}]
        }));

        let error = Mysql
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap_err();

        assert_eq!(
            error,
            config_error("queries", "returns a column name twice")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_query_stream_is_read_like_a_table() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "queries": [{"name": "evens", "sql": format!("SELECT id, email FROM {}.contacts WHERE id % 2 = 0", fixture.database)}]
        }));
        let evens = stream("query:evens", SyncMode::FullRefresh, None, &["id"]);

        let batches = fixture.read(&config, &evens, None).await.unwrap();

        assert_eq!(keys(&batches).len(), 600);
        assert_eq!(keys(&batches)[..2], ["2", "4"]);
        let no_query = stream("query:missing", SyncMode::FullRefresh, None, &["id"]);
        assert_eq!(
            fixture.read(&config, &no_query, None).await.unwrap_err(),
            config_error("queries", "has no query for this stream")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_query_slower_than_the_statement_timeout_fails_with_the_fixed_timeout_error() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({
            "statement_timeout_seconds": 1,
            "queries": [{"name": "slow", "sql": format!("SELECT c.id FROM {}.contacts c WHERE SLEEP(5) = 0", fixture.database)}]
        }));
        let slow = stream("query:slow", SyncMode::FullRefresh, None, &["id"]);
        let started = Instant::now();

        let error = fixture.read(&config, &slow, None).await.unwrap_err();

        assert_eq!(error, timeout());
        assert_eq!(error.to_string(), TIMEOUT);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_write_inside_the_batch_transaction_is_refused_even_for_a_user_that_can_write() {
        let fixture = Fixture::new().await;
        let admin = &fixture.admin;
        let writer = credential(&admin.user, &admin.password);
        let settings = Settings::parse(&fixture.config(json!({})), Flavor::MySql).unwrap();
        let login = Login::from_credential(&writer).unwrap();
        let mut conn = open(&fixture.ctx, &settings, &login).await.unwrap();

        let mut tx = begin_batch(&mut conn).await.unwrap();
        let error = tx
            .query_drop(format!(
                "INSERT INTO {}.no_key VALUES (2, 'y')",
                fixture.database
            ))
            .await
            .unwrap_err();

        assert!(
            matches!(&error, mysql_async::Error::Server(server) if server.code == 1792),
            "{error:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_wrong_password_is_auth_and_a_closed_port_is_unreachable() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));
        let closed = fixture.config(json!({"host": "127.0.0.1", "port": 1}));

        let wrong = Mysql
            .check(
                &fixture.ctx,
                &config,
                &credential(&fixture.user, "not-the-password"),
            )
            .await;
        let unreachable = Mysql.check(&fixture.ctx, &closed, &fixture.reader()).await;

        assert_eq!(wrong, Err(SourceError::Auth));
        assert_eq!(unreachable, Err(SourceError::Unreachable));
        let text = format!("{wrong:?}{unreachable:?}");
        for secret in [
            fixture.admin.host.as_str(),
            "not-the-password",
            fixture.user.as_str(),
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn tls_verify_against_a_self_signed_certificate_is_refused_and_check_passes_with_it_off()
    {
        let fixture = Fixture::new().await;
        let verify = fixture.config(json!({"tls": "verify"}));

        let refused = Mysql.check(&fixture.ctx, &verify, &fixture.reader()).await;
        let allowed = Mysql
            .check(&fixture.ctx, &fixture.config(json!({})), &fixture.reader())
            .await;

        assert_eq!(refused, Err(tls()));
        assert_eq!(allowed, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn no_error_text_carries_a_seeded_value() {
        let fixture = Fixture::new().await;
        // Comparing a DATETIME with a non-date literal fails with a message that quotes it.
        let config = fixture.config(json!({
            "queries": [{"name": "boom", "sql": format!("SELECT c.id FROM {}.contacts c WHERE c.updated_at > 'sentinel-secret-value'", fixture.database)}]
        }));
        let boom = stream("query:boom", SyncMode::FullRefresh, None, &["id"]);

        let error = fixture.read(&config, &boom, None).await.unwrap_err();

        assert_eq!(error.to_string(), QUERY);
        assert!(!format!("{error:?}").contains("sentinel"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn a_view_and_a_foreign_database_are_not_streams_and_a_keyless_table_is() {
        let fixture = Fixture::new().await;
        let config = fixture.config(json!({}));

        let streams = Mysql
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap();

        let names: Vec<_> = streams.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"contacts_view"), "{names:?}");
        assert!(!names.contains(&"hidden"), "{names:?}");
        assert!(names.contains(&"no_key"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires MySQL; set ZIRV_FLOW_TEST_MYSQL_URL"]
    async fn the_connection_test_passes_for_the_read_only_user() {
        let fixture = Fixture::new().await;

        let result = Mysql
            .check(&fixture.ctx, &fixture.config(json!({})), &fixture.reader())
            .await;

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn the_fixed_messages_read_as_documented() {
        assert_eq!(TIMEOUT, "the database did not answer in time");
        assert_eq!(QUERY, "the database query failed");
    }
}
