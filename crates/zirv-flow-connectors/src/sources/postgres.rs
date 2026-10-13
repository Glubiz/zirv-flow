//! The PostgreSQL source: tables and queries of one schema, read by keyset in short read-only
//! transactions (architecture 7.2). Connects only to addresses the egress policy allowed, over
//! verified TLS by default, and never forwards a driver or server message.

use std::net::IpAddr;
use std::time::Duration;

use futures_util::stream::BoxStream;
use serde_json::Value;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, IsolationLevel, NoTls, Transaction};
use tokio_postgres_rustls::MakeRustlsConnect;

use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
use super::sql::connect::{CONNECT_TIMEOUT, backstop, resolve, within};
use super::sql::discover::{MAX_TABLES, default_cursor, descriptor, from_clause};
use super::sql::error::{from_postgres, query};
use super::sql::keyset::{BATCH_ROWS, KeySpec, Paged, Pager, Row, batches, parse_checkpoint};
use super::sql::metrics::metered;
use super::sql::settings::{
    Flavor, Login, QUERY_PREFIX, Settings, TlsMode, check_config, config_error,
};
use super::sql::tls;
use super::types::{Batch, Checkpoint, FieldDescriptor, FieldType, StreamConfig, StreamDescriptor};

const TYPE: &str = "postgres";
/// A transaction that stalls between statements is ended by the server after this long.
const IDLE_IN_TRANSACTION_MS: u64 = 60_000;

pub(crate) struct Postgres;

impl Source for Postgres {
    const SPEC: SourceSpec = SourceSpec {
        kind: TYPE,
        label: "PostgreSQL",
        description: "Reads tables and queries of one schema of a PostgreSQL database, \
            through a read-only role.",
        config_fields: &[
            FieldSpec {
                key: "host",
                label: "Host",
                help: "The database server's DNS name or IP address.",
                kind: FieldKind::Text,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "port",
                label: "Port",
                help: "Defaults to 5432.",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "database",
                label: "Database",
                help: "The database to connect to.",
                kind: FieldKind::Text,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "schema",
                label: "Schema",
                help: "The schema whose tables are streams. Defaults to public.",
                kind: FieldKind::Text,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "tls",
                label: "TLS",
                help: "verify (default) checks the server's certificate; off is accepted only \
                    toward addresses the egress allowlist names.",
                kind: FieldKind::Select(&["verify", "off"]),
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "ca_certificate",
                label: "CA certificate",
                help: "A PEM certificate to trust instead of the built-in public roots.",
                kind: FieldKind::LongText,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "statement_timeout_seconds",
                label: "Statement timeout (seconds)",
                help: "The longest one query may run, 1 to 600. Defaults to 60.",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "queries",
                label: "Queries",
                help: "Named SELECT statements, each offered as a stream named query:<name>.",
                kind: FieldKind::Queries,
                required: false,
                secret: false,
            },
        ],
        credential_fields: &[
            FieldSpec {
                key: "user",
                label: "User",
                help: "A role that can only SELECT what should be read.",
                kind: FieldKind::Text,
                required: true,
                secret: true,
            },
            FieldSpec {
                key: "password",
                label: "Password",
                help: "The role's password.",
                kind: FieldKind::Text,
                required: true,
                secret: true,
            },
        ],
        rate_limit: None,
        config_check: Some(|config| check_config(config, Flavor::Postgres)),
        capabilities: Capabilities {
            incremental: true,
            full_refresh: true,
            emits_deletes: false,
        },
    };

    async fn check(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        metered(TYPE, async {
            let (settings, mut conn) = connect(ctx, config, credential).await?;
            within(backstop(settings.statement_timeout), async {
                let tx = begin_batch(&mut conn.client, settings.statement_timeout)
                    .await
                    .map_err(failed)?;
                tx.query_one("SELECT 1", &[]).await.map_err(failed)?;
                tx.commit().await.map_err(failed)
            })
            .await
        })
        .await
    }

    async fn discover(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        metered(TYPE, async {
            let (settings, mut conn) = connect(ctx, config, credential).await?;
            within(
                backstop(settings.statement_timeout) * 2,
                discover_streams(&mut conn.client, &settings),
            )
            .await
        })
        .await
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
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

fn failed(error: tokio_postgres::Error) -> SourceError {
    from_postgres(TYPE, &error)
}

/// A connection and the task that drives it, which ends with the connection.
struct Conn {
    client: Client,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(
    ctx: &SourceContext,
    config: &Value,
    credential: &Credential,
) -> Result<(Settings, Conn), SourceError> {
    let settings = Settings::parse(config, Flavor::Postgres)?;
    let login = Login::from_credential(credential)?;
    let conn = open(ctx, &settings, &login).await?;
    Ok((settings, conn))
}

/// Resolves and checks the host, then connects to exactly the checked addresses.
async fn open(
    ctx: &SourceContext,
    settings: &Settings,
    login: &Login<'_>,
) -> Result<Conn, SourceError> {
    let ips = resolve(ctx, settings).await?;
    let options = options(settings, login, &ips);
    within(CONNECT_TIMEOUT, async {
        match settings.tls {
            TlsMode::Off => {
                let (client, connection) = options.connect(NoTls).await.map_err(failed)?;
                Ok(spawn(client, connection))
            }
            TlsMode::Verify => {
                let config = tls::client_config(settings.ca_certificate.as_deref())?;
                let (client, connection) = options
                    .connect(MakeRustlsConnect::new(config))
                    .await
                    .map_err(failed)?;
                Ok(spawn(client, connection))
            }
        }
    })
    .await
}

fn spawn<S, T>(client: Client, connection: tokio_postgres::Connection<S, T>) -> Conn
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let task = tokio::spawn(async move {
        // The outcome reaches the caller through the client's own errors.
        let _ = connection.await;
    });
    Conn { client, task }
}

/// The driver options. Built in code, never from a connection string. The host name stays the
/// TLS name and each checked address is the TCP target (`hostaddr`); the driver tries them in
/// order and follows no redirect, so nothing else is dialled.
fn options(settings: &Settings, login: &Login<'_>, ips: &[IpAddr]) -> tokio_postgres::Config {
    let mut options = tokio_postgres::Config::new();
    options
        .user(login.user)
        .password(login.password)
        .dbname(&settings.database)
        .port(settings.port)
        .connect_timeout(CONNECT_TIMEOUT)
        .application_name("zirv-flow")
        .ssl_mode(match settings.tls {
            TlsMode::Verify => SslMode::Require,
            TlsMode::Off => SslMode::Disable,
        });
    for ip in ips {
        options.host(settings.host.as_str());
        options.hostaddr(*ip);
    }
    options
}

/// Starts a transaction that can only read, with the statement timeout, an idle limit and UTC
/// as the time zone (so a timestamp's text form, the checkpoint, is stable). Every statement the
/// source runs goes through one of these; the server refuses a write inside it (25006).
async fn begin_batch(
    client: &mut Client,
    statement_timeout: Duration,
) -> Result<Transaction<'_>, tokio_postgres::Error> {
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::ReadCommitted)
        .read_only(true)
        .start()
        .await?;
    tx.batch_execute(&format!(
        "SET LOCAL statement_timeout = {}; \
         SET LOCAL idle_in_transaction_session_timeout = {IDLE_IN_TRANSACTION_MS}; \
         SET LOCAL TimeZone = 'UTC'",
        statement_timeout.as_millis()
    ))
    .await?;
    Ok(tx)
}

/// A column of a stream: its name and the type the server reported.
#[derive(Clone, Debug)]
struct Column {
    name: String,
    ty: Type,
}

/// The types a stream can be paged on, each with the type its checkpoint text is cast back to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyType {
    Int2,
    Int4,
    Int8,
    Numeric,
    Text,
    Varchar,
    Bpchar,
    Uuid,
    Date,
    Timestamp,
    Timestamptz,
}

impl KeyType {
    fn of(ty: &Type) -> Option<Self> {
        Some(match *ty {
            Type::INT2 => Self::Int2,
            Type::INT4 => Self::Int4,
            Type::INT8 => Self::Int8,
            Type::NUMERIC => Self::Numeric,
            Type::TEXT => Self::Text,
            Type::VARCHAR => Self::Varchar,
            Type::BPCHAR => Self::Bpchar,
            Type::UUID => Self::Uuid,
            Type::DATE => Self::Date,
            Type::TIMESTAMP => Self::Timestamp,
            Type::TIMESTAMPTZ => Self::Timestamptz,
            _ => return None,
        })
    }

    /// The name the text parameter is cast to. From this fixed table, never from user text.
    fn cast(self) -> &'static str {
        match self {
            Self::Int2 => "int2",
            Self::Int4 => "int4",
            Self::Int8 => "int8",
            Self::Numeric => "numeric",
            Self::Text => "text",
            Self::Varchar => "varchar",
            Self::Bpchar => "bpchar",
            Self::Uuid => "uuid",
            Self::Date => "date",
            Self::Timestamp => "timestamp",
            Self::Timestamptz => "timestamptz",
        }
    }
}

/// Quotes an identifier: wrapped in double quotes, with embedded ones doubled.
fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The select list: every column as text (one decoding path for every type). Binary columns are
/// not text and arrive as `NULL`.
fn select_list(columns: &[Column]) -> String {
    columns
        .iter()
        .map(|column| {
            let name = quote(&column.name);
            if column.ty == Type::BYTEA {
                format!("NULL::text AS {name}")
            } else {
                format!("s.{name}::text AS {name}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The page query. `from` is a quoted table or a parenthesised query, `keys` the key columns in
/// comparison order (the cursor first, when `cursor` is set) with their types, and `after`
/// whether a checkpoint applies. The key is compared expanded, not as a row constructor, with
/// the checkpoint bound as text parameters `$1..$n` and cast on the server.
fn page_sql(
    from: &str,
    columns: &[Column],
    keys: &[(String, KeyType)],
    cursor: bool,
    after: bool,
) -> String {
    let mut conditions = Vec::new();
    if cursor {
        conditions.push(format!("s.{} IS NOT NULL", quote(&keys[0].0)));
    }
    if after {
        let operand = |index: usize| {
            let (name, key_type) = &keys[index];
            (
                format!("s.{}", quote(name)),
                format!("${}::text::{}", index + 1, key_type.cast()),
            )
        };
        let alternatives: Vec<String> = (0..keys.len())
            .map(|i| {
                let mut terms: Vec<String> = (0..i)
                    .map(|j| {
                        let (column, parameter) = operand(j);
                        format!("{column} = {parameter}")
                    })
                    .collect();
                let (column, parameter) = operand(i);
                terms.push(format!("{column} > {parameter}"));
                if terms.len() == 1 {
                    terms.remove(0)
                } else {
                    format!("({})", terms.join(" AND "))
                }
            })
            .collect();
        conditions.push(format!("({})", alternatives.join(" OR ")));
    }
    let filter = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let order = keys
        .iter()
        .map(|(name, _)| format!("s.{}", quote(name)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {} FROM {from} AS s{filter} ORDER BY {order} LIMIT {BATCH_ROWS}",
        select_list(columns)
    )
}

/// What a stream reads from: a table of the configured schema or a configured query.
fn from_stream(settings: &Settings, stream: &str) -> Result<String, SourceError> {
    from_clause(settings, stream, |table| {
        format!("{}.{}", quote(&settings.schema), quote(table))
    })
}

/// Prepares `SELECT * FROM <from>` (parsed and planned, not run) to learn the columns.
async fn describe(
    client: &mut Client,
    timeout: Duration,
    from: &str,
) -> Result<Vec<Column>, SourceError> {
    let tx = begin_batch(client, timeout).await.map_err(failed)?;
    let statement = tx
        .prepare(&format!("SELECT * FROM {from} AS s"))
        .await
        .map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok(statement
        .columns()
        .iter()
        .map(|column| Column {
            name: column.name().to_owned(),
            ty: column.type_().clone(),
        })
        .collect())
}

struct PostgresPager {
    conn: Conn,
    statement_timeout: Duration,
    first: String,
    next: String,
}

impl Pager for PostgresPager {
    async fn page(&mut self, after: Option<&[String]>) -> Result<Vec<Row>, SourceError> {
        let (sql, parameters) = match after {
            None => (&self.first, &[][..]),
            Some(key) => (&self.next, key),
        };
        let parameters: Vec<&(dyn ToSql + Sync)> = parameters
            .iter()
            .map(|value| value as &(dyn ToSql + Sync))
            .collect();
        let timeout = self.statement_timeout;
        within(backstop(timeout), async {
            let tx = begin_batch(&mut self.conn.client, timeout)
                .await
                .map_err(failed)?;
            let rows = tx.query(sql.as_str(), &parameters).await.map_err(failed)?;
            tx.commit().await.map_err(failed)?;
            rows.iter()
                .map(|row| {
                    (0..row.len())
                        .map(|index| row.try_get::<_, Option<String>>(index).map_err(|_| query()))
                        .collect()
                })
                .collect()
        })
        .await
    }
}

async fn open_paged(
    ctx: &SourceContext,
    config: &Value,
    credential: &Credential,
    stream: &StreamConfig,
    checkpoint: Option<Checkpoint>,
) -> Result<Paged<PostgresPager>, SourceError> {
    let settings = Settings::parse(config, Flavor::Postgres)?;
    let login = Login::from_credential(credential)?;
    let spec = KeySpec::from_stream(stream)?;
    let after = parse_checkpoint(checkpoint.as_ref(), &spec)?;
    let from = from_stream(&settings, &stream.name)?;
    let mut conn = open(ctx, &settings, &login).await?;
    let columns = within(
        backstop(settings.statement_timeout),
        describe(&mut conn.client, settings.statement_timeout, &from),
    )
    .await?;
    let keys = key_types(&columns, &spec)?;
    let pager = PostgresPager {
        conn,
        statement_timeout: settings.statement_timeout,
        first: page_sql(&from, &columns, &keys, spec.cursor.is_some(), false),
        next: page_sql(&from, &columns, &keys, spec.cursor.is_some(), true),
    };
    let names = columns.into_iter().map(|column| column.name).collect();
    Paged::new(pager, names, &spec, after)
}

/// The key columns with the type each is paged on. A column the stream lacks or a type that
/// cannot be paged on is a configuration error naming the setting.
fn key_types(columns: &[Column], spec: &KeySpec) -> Result<Vec<(String, KeyType)>, SourceError> {
    let mut keys = Vec::new();
    let mut add = |name: &str, setting: &str| {
        let column = columns
            .iter()
            .find(|column| column.name == name)
            .ok_or_else(|| config_error(setting, "names a column the stream does not have"))?;
        let key_type = KeyType::of(&column.ty)
            .ok_or_else(|| config_error(setting, "has a type that cannot be paged on"))?;
        keys.push((name.to_owned(), key_type));
        Ok::<_, SourceError>(())
    };
    if let Some(cursor) = &spec.cursor {
        add(cursor, "cursor_field")?;
    }
    for name in &spec.primary_key {
        add(name, "primary_key")?;
    }
    Ok(keys)
}

fn field_type(type_name: &str) -> FieldType {
    match type_name {
        "int2" | "int4" | "int8" | "numeric" | "float4" | "float8" | "money" => FieldType::Number,
        "bool" => FieldType::Boolean,
        "date" | "timestamp" | "timestamptz" => FieldType::Timestamp,
        "json" | "jsonb" => FieldType::Json,
        "text" | "varchar" | "bpchar" | "uuid" | "name" | "citext" => FieldType::String,
        _ => FieldType::Unknown,
    }
}

async fn discover_streams(
    client: &mut Client,
    settings: &Settings,
) -> Result<Vec<StreamDescriptor>, SourceError> {
    let tx = begin_batch(client, settings.statement_timeout)
        .await
        .map_err(failed)?;
    let columns = tx
        .query(
            "SELECT t.table_name::text, c.column_name::text, c.udt_name::text \
             FROM information_schema.tables t \
             JOIN information_schema.columns c \
               ON c.table_schema = t.table_schema AND c.table_name = t.table_name \
             WHERE t.table_schema = $1::text AND t.table_type = 'BASE TABLE' \
             ORDER BY t.table_name, c.ordinal_position",
            &[&settings.schema],
        )
        .await
        .map_err(failed)?;
    // The catalog (not information_schema, which hides constraints from a role that can only
    // SELECT) lists the primary keys.
    let keys = tx
        .query(
            "SELECT c.relname::text, a.attname::text \
             FROM pg_catalog.pg_index i \
             JOIN pg_catalog.pg_class c ON c.oid = i.indrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = ANY (i.indkey) \
             WHERE i.indisprimary AND n.nspname = $1::text \
             ORDER BY c.relname, array_position(i.indkey::int2[], a.attnum)",
            &[&settings.schema],
        )
        .await
        .map_err(failed)?;
    let mut streams: Vec<StreamDescriptor> = Vec::new();
    for row in &columns {
        let (table, column, type_name): (String, String, String) = (
            row.try_get(0).map_err(|_| query())?,
            row.try_get(1).map_err(|_| query())?,
            row.try_get(2).map_err(|_| query())?,
        );
        if streams.last().is_none_or(|stream| stream.name != table) {
            if streams.len() == MAX_TABLES {
                return Err(config_error(
                    "schema",
                    "has more than 1000 tables; grant the role fewer",
                ));
            }
            streams.push(descriptor(table));
        }
        let stream = streams.last_mut().expect("a stream was just pushed");
        stream.fields.push(FieldDescriptor {
            name: column,
            field_type: field_type(&type_name),
        });
    }
    for row in &keys {
        let (table, column): (String, String) = (
            row.try_get(0).map_err(|_| query())?,
            row.try_get(1).map_err(|_| query())?,
        );
        if let Some(stream) = streams.iter_mut().find(|stream| stream.name == table) {
            stream.default_primary_key.push(column);
        }
    }
    for stream in &mut streams {
        stream.default_cursor_field = default_cursor(&stream.fields);
    }
    for query_stream in &settings.queries {
        let name = format!("{QUERY_PREFIX}{}", query_stream.name);
        // Prepared as written, so the columns are the query's own and a second statement is the
        // driver's error.
        let statement = tx
            .prepare(&query_stream.sql)
            .await
            .map_err(|error| rejected_query(&error))?;
        let mut stream = descriptor(name);
        for column in statement.columns() {
            if stream
                .fields
                .iter()
                .any(|field| field.name == column.name())
            {
                return Err(config_error("queries", "returns a column name twice"));
            }
            stream.fields.push(FieldDescriptor {
                name: column.name().to_owned(),
                field_type: field_type(column.type_().name()),
            });
        }
        streams.push(stream);
    }
    tx.commit().await.map_err(failed)?;
    Ok(streams)
}

/// A query the server would not prepare: a fixed message, with a deadline or a permission
/// problem kept as what it is.
fn rejected_query(error: &tokio_postgres::Error) -> SourceError {
    let mapped = from_postgres(TYPE, error);
    match error.code() {
        Some(code) if *code == SqlState::QUERY_CANCELED => mapped,
        Some(_) => config_error("queries", "has a query the database rejected"),
        None => mapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::SyncMode;

    fn column(name: &str, ty: Type) -> Column {
        Column {
            name: name.to_owned(),
            ty,
        }
    }

    fn columns() -> Vec<Column> {
        vec![
            column("id", Type::INT8),
            column("updated_at", Type::TIMESTAMPTZ),
            column("payload", Type::BYTEA),
        ]
    }

    fn key_list(names: &[(&str, KeyType)]) -> Vec<(String, KeyType)> {
        names.iter().map(|(n, t)| ((*n).to_owned(), *t)).collect()
    }

    const SELECT: &str = r#"SELECT s."id"::text AS "id", s."updated_at"::text AS "updated_at", NULL::text AS "payload""#;

    #[test]
    fn the_first_incremental_page_skips_null_cursors_and_orders_by_cursor_then_key() {
        let sql = page_sql(
            r#""public"."contacts""#,
            &columns(),
            &key_list(&[("updated_at", KeyType::Timestamptz), ("id", KeyType::Int8)]),
            true,
            false,
        );

        assert_eq!(
            sql,
            format!(
                r#"{SELECT} FROM "public"."contacts" AS s WHERE s."updated_at" IS NOT NULL ORDER BY s."updated_at", s."id" LIMIT 500"#
            )
        );
    }

    #[test]
    fn an_incremental_page_after_a_checkpoint_compares_the_expanded_key() {
        let sql = page_sql(
            r#""public"."contacts""#,
            &columns(),
            &key_list(&[("updated_at", KeyType::Timestamptz), ("id", KeyType::Int8)]),
            true,
            true,
        );

        assert_eq!(
            sql,
            format!(
                r#"{SELECT} FROM "public"."contacts" AS s WHERE s."updated_at" IS NOT NULL AND (s."updated_at" > $1::text::timestamptz OR (s."updated_at" = $1::text::timestamptz AND s."id" > $2::text::int8)) ORDER BY s."updated_at", s."id" LIMIT 500"#
            )
        );
    }

    #[test]
    fn a_full_refresh_orders_by_the_key_alone_and_two_key_columns_expand_to_three_terms() {
        let first = page_sql(
            "t",
            &columns()[..1],
            &key_list(&[("id", KeyType::Int8)]),
            false,
            false,
        );
        let composite = page_sql(
            "t",
            &columns()[..1],
            &key_list(&[("a", KeyType::Int4), ("b", KeyType::Text)]),
            false,
            true,
        );

        assert_eq!(
            first,
            r#"SELECT s."id"::text AS "id" FROM t AS s ORDER BY s."id" LIMIT 500"#
        );
        assert_eq!(
            composite,
            r#"SELECT s."id"::text AS "id" FROM t AS s WHERE (s."a" > $1::text::int4 OR (s."a" = $1::text::int4 AND s."b" > $2::text::text)) ORDER BY s."a", s."b" LIMIT 500"#
        );
    }

    #[test]
    fn three_key_columns_expand_to_three_alternatives() {
        let sql = page_sql(
            "t",
            &columns()[..1],
            &key_list(&[
                ("c", KeyType::Date),
                ("a", KeyType::Uuid),
                ("b", KeyType::Numeric),
            ]),
            true,
            true,
        );

        assert!(
            sql.contains(
                r#"(s."c" > $1::text::date OR (s."c" = $1::text::date AND s."a" > $2::text::uuid) OR (s."c" = $1::text::date AND s."a" = $2::text::uuid AND s."b" > $3::text::numeric))"#
            ),
            "{sql}"
        );
    }

    #[test]
    fn an_identifier_with_quotes_is_quoted_by_doubling_them() {
        assert_eq!(quote(r#"we"ird"#), r#""we""ird""#);
        let sql = page_sql(
            &quote(r#"t"; DROP TABLE x; --"#),
            &[column(r#"i"d"#, Type::INT8)],
            &key_list(&[(r#"i"d"#, KeyType::Int8)]),
            false,
            false,
        );
        assert!(
            sql.contains(r#"s."i""d"::text AS "i""d" FROM "t""; DROP TABLE x; --" AS s"#),
            "{sql}"
        );
    }

    #[test]
    fn a_query_stream_is_wrapped_on_its_own_lines_and_a_table_is_qualified() {
        let mut settings = Settings::parse(
            &serde_json::json!({"host": "h", "database": "d", "schema": "shop",
                "queries": [{"name": "vip", "sql": "SELECT 1 -- note"}]}),
            Flavor::Postgres,
        )
        .unwrap();

        assert_eq!(
            from_stream(&settings, "query:vip").unwrap(),
            "(\nSELECT 1 -- note\n)"
        );
        assert_eq!(
            from_stream(&settings, "contacts").unwrap(),
            r#""shop"."contacts""#
        );
        assert_eq!(
            from_stream(&settings, "query:other").unwrap_err(),
            config_error("queries", "has no query for this stream")
        );
        settings.schema = r#"a"b"#.to_owned();
        assert_eq!(from_stream(&settings, "t").unwrap(), r#""a""b"."t""#);
    }

    #[test]
    fn the_key_types_are_a_fixed_table_and_anything_else_cannot_be_paged_on() {
        let supported = [
            (Type::INT2, "int2"),
            (Type::INT4, "int4"),
            (Type::INT8, "int8"),
            (Type::NUMERIC, "numeric"),
            (Type::TEXT, "text"),
            (Type::VARCHAR, "varchar"),
            (Type::BPCHAR, "bpchar"),
            (Type::UUID, "uuid"),
            (Type::DATE, "date"),
            (Type::TIMESTAMP, "timestamp"),
            (Type::TIMESTAMPTZ, "timestamptz"),
        ];
        for (ty, cast) in supported {
            assert_eq!(KeyType::of(&ty).unwrap().cast(), cast);
        }
        for ty in [
            Type::FLOAT8,
            Type::BOOL,
            Type::BYTEA,
            Type::JSONB,
            Type::TIME,
        ] {
            assert_eq!(KeyType::of(&ty), None, "{ty}");
        }
        let spec = KeySpec {
            cursor: Some("score".into()),
            primary_key: vec!["id".into()],
        };
        let cols = vec![column("id", Type::INT8), column("score", Type::FLOAT8)];
        assert_eq!(
            key_types(&cols, &spec).unwrap_err(),
            config_error("cursor_field", "has a type that cannot be paged on")
        );
        let spec = KeySpec {
            cursor: None,
            primary_key: vec!["gone".into()],
        };
        assert_eq!(
            key_types(&cols, &spec).unwrap_err(),
            config_error("primary_key", "names a column the stream does not have")
        );
    }

    #[test]
    fn catalog_types_map_to_field_types() {
        assert_eq!(field_type("int8"), FieldType::Number);
        assert_eq!(field_type("numeric"), FieldType::Number);
        assert_eq!(field_type("bool"), FieldType::Boolean);
        assert_eq!(field_type("timestamptz"), FieldType::Timestamp);
        assert_eq!(field_type("date"), FieldType::Timestamp);
        assert_eq!(field_type("jsonb"), FieldType::Json);
        assert_eq!(field_type("uuid"), FieldType::String);
        assert_eq!(field_type("varchar"), FieldType::String);
        assert_eq!(field_type("bytea"), FieldType::Unknown);
    }

    fn login() -> Login<'static> {
        Login {
            user: "reader",
            password: "pw",
        }
    }

    fn settings(extra: Value) -> Settings {
        let mut config = serde_json::json!({"host": "db.test", "database": "shop"});
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        Settings::parse(&config, Flavor::Postgres).unwrap()
    }

    #[test]
    fn the_options_dial_each_checked_address_under_the_host_name_and_require_tls() {
        let ips: Vec<IpAddr> = vec!["10.0.0.5".parse().unwrap(), "10.0.0.6".parse().unwrap()];

        let options = options(&settings(serde_json::json!({})), &login(), &ips);

        assert_eq!(options.get_hostaddrs(), ips.as_slice());
        assert_eq!(options.get_hosts().len(), 2);
        for host in options.get_hosts() {
            assert!(
                matches!(host, tokio_postgres::config::Host::Tcp(name) if name == "db.test"),
                "{host:?}"
            );
        }
        assert_eq!(options.get_ssl_mode(), SslMode::Require);
        assert_eq!(options.get_ports(), &[5432]);
        assert_eq!(options.get_connect_timeout(), Some(&CONNECT_TIMEOUT));
    }

    #[test]
    fn tls_off_disables_tls_and_nothing_else_does() {
        let ips: Vec<IpAddr> = vec!["10.0.0.5".parse().unwrap()];

        let off = options(&settings(serde_json::json!({"tls": "off"})), &login(), &ips);

        assert_eq!(off.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn the_source_is_declared_with_secret_credentials_and_the_documented_settings() {
        let spec = &Postgres::SPEC;

        assert_eq!(spec.kind, "postgres");
        assert!(spec.credential_fields.iter().all(|field| field.secret));
        let keys: Vec<_> = spec.config_fields.iter().map(|field| field.key).collect();
        assert_eq!(
            keys,
            [
                "host",
                "port",
                "database",
                "schema",
                "tls",
                "ca_certificate",
                "statement_timeout_seconds",
                "queries"
            ]
        );
        assert!(!spec.capabilities.emits_deletes);
    }

    // --- egress and TLS at the connect point, against in-process servers -------------------

    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::super::sql::error::{QUERY, TIMEOUT, TLS, timeout, tls};
    use super::super::sql::testing::{
        context, credential, fixed, keys, loopback_context, pki, read_all, required_env, suffix,
    };

    fn source_config(port: u16, extra: Value) -> Value {
        let mut config = serde_json::json!({"host": "db.test", "database": "shop", "port": port});
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        config
    }

    #[tokio::test]
    async fn a_private_address_is_refused_before_any_connection() {
        let ctx = context(TYPE, &[], fixed(&["10.0.0.5"]));
        let started = Instant::now();

        let error = Postgres
            .check(
                &ctx,
                &source_config(5432, serde_json::json!({})),
                &credential("u", "p"),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
        // A connection attempt to 10.0.0.5 would have waited out the connect timeout.
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_mixed_answer_is_refused_whole() {
        let ctx = context(TYPE, &[], fixed(&["8.8.8.8", "10.0.0.5"]));

        let error = Postgres
            .discover(
                &ctx,
                &source_config(5432, serde_json::json!({})),
                &credential("u", "p"),
            )
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

        let result = Postgres
            .check(
                &ctx,
                &source_config(port, serde_json::json!({})),
                &credential("u", "p"),
            )
            .await;

        assert_eq!(
            accepted.await.unwrap(),
            "127.0.0.1".parse::<IpAddr>().unwrap()
        );
        // The listener drops the connection, so the check fails after connecting.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn a_refusal_is_counted_in_the_egress_denied_metric() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        let ctx = context(TYPE, &[], fixed(&["10.0.0.5"]));

        let _ = Postgres
            .check(
                &ctx,
                &source_config(5432, serde_json::json!({})),
                &credential("u", "p"),
            )
            .await;

        let rendered = handle.render();
        assert!(
            rendered.contains("zirv_flow_egress_denied_total{class=\"private\"} 1"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "zirv_flow_source_db_errors_total{source_type=\"postgres\",reason=\"egress\"} 1"
            ),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn tls_off_toward_a_public_address_is_refused_before_any_connection() {
        let ctx = context(TYPE, &[], fixed(&["8.8.8.8"]));

        let error = Postgres
            .check(
                &ctx,
                &source_config(5432, serde_json::json!({"tls": "off"})),
                &credential("u", "p"),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            config_error(
                "tls",
                "cannot be off toward a public address; allowlist the range in the egress settings"
            )
        );
    }

    /// What the fake server saw.
    #[derive(Debug, Default)]
    struct Seen {
        peer: Option<IpAddr>,
        handshake: bool,
        sni: Option<String>,
        /// Bytes the client sent after the server declined TLS, which would be plaintext.
        plaintext_after: usize,
    }

    /// A PostgreSQL server that answers the SSL request with `answer` and, on `S`, speaks TLS.
    async fn fake_server(
        answer: u8,
        acceptor: Option<TlsAcceptor>,
    ) -> (u16, tokio::task::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut stream, peer) = listener.accept().await.unwrap();
            let mut seen = Seen {
                peer: Some(peer.ip()),
                ..Seen::default()
            };
            let mut request = [0u8; 8];
            stream.read_exact(&mut request).await.unwrap();
            // Length 8, then the SSL request code 80877103.
            assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
            stream.write_all(&[answer]).await.unwrap();
            let mut buffer = [0u8; 1024];
            match acceptor {
                Some(acceptor) if answer == b'S' => {
                    if let Ok(mut secured) = acceptor.accept(stream).await {
                        seen.handshake = true;
                        seen.sni = secured.get_ref().1.server_name().map(str::to_owned);
                        // The startup message, then the connection closes.
                        let _ = secured.read(&mut buffer).await;
                    }
                }
                _ => seen.plaintext_after = stream.read(&mut buffer).await.unwrap_or(0),
            }
            seen
        });
        (port, handle)
    }

    async fn check_against(port: u16, extra: Value) -> Result<(), SourceError> {
        let ctx = context(TYPE, &["127.0.0.1/32"], fixed(&["127.0.0.1"]));
        tokio::time::timeout(
            Duration::from_secs(10),
            Postgres.check(&ctx, &source_config(port, extra), &credential("u", "p")),
        )
        .await
        .expect("the check must not hang")
    }

    #[tokio::test]
    async fn the_checked_ip_is_dialled_and_the_host_name_is_the_verified_tls_name() {
        let pki = pki("db.test");
        let (port, server) = fake_server(b'S', Some(pki.acceptor.clone())).await;

        let result = check_against(port, serde_json::json!({"ca_certificate": pki.ca_pem})).await;

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
        let (port, server) = fake_server(b'S', Some(pki.acceptor.clone())).await;

        let result = check_against(port, serde_json::json!({"ca_certificate": pki.ca_pem})).await;

        assert!(!server.await.unwrap().handshake);
        assert_eq!(result, Err(tls()));
    }

    #[tokio::test]
    async fn a_certificate_from_an_untrusted_ca_fails_the_handshake() {
        // The CA is not configured, so only the public roots are trusted.
        let pki = pki("db.test");
        let (port, server) = fake_server(b'S', Some(pki.acceptor.clone())).await;

        let result = check_against(port, serde_json::json!({})).await;

        assert!(!server.await.unwrap().handshake);
        assert_eq!(result, Err(tls()));
    }

    #[tokio::test]
    async fn a_server_that_offers_no_tls_is_refused_and_nothing_is_sent_in_plaintext() {
        let (port, server) = fake_server(b'N', None).await;

        let result = check_against(port, serde_json::json!({})).await;

        let seen = server.await.unwrap();
        assert_eq!(
            seen.plaintext_after, 0,
            "the startup message must not be sent"
        );
        assert_eq!(result, Err(tls()));
    }

    // --- the fixture suite, against a running PostgreSQL (DATABASE_URL) ----------------------

    #[derive(Clone)]
    struct Admin {
        host: String,
        port: u16,
        user: String,
        password: String,
        database: String,
    }

    fn admin() -> Admin {
        let url = required_env("DATABASE_URL");
        let parsed: tokio_postgres::Config = url.parse().expect("DATABASE_URL must parse");
        let host = match &parsed.get_hosts()[0] {
            tokio_postgres::config::Host::Tcp(host) => host.clone(),
            #[cfg(unix)]
            other => panic!("DATABASE_URL must name a TCP host, not {other:?}"),
        };
        Admin {
            host,
            port: parsed.get_ports().first().copied().unwrap_or(5432),
            user: parsed.get_user().unwrap_or("postgres").to_owned(),
            password: String::from_utf8(parsed.get_password().unwrap_or_default().to_vec())
                .unwrap(),
            database: parsed.get_dbname().unwrap_or("postgres").to_owned(),
        }
    }

    async fn admin_exec(admin: &Admin, sql: &str) {
        let mut options = tokio_postgres::Config::new();
        options
            .host(admin.host.as_str())
            .port(admin.port)
            .user(&admin.user)
            .password(&admin.password)
            .dbname(&admin.database);
        let (client, connection) = options.connect(NoTls).await.expect("admin connect");
        let task = tokio::spawn(connection);
        client.batch_execute(sql).await.expect("admin statement");
        drop(client);
        let _ = task.await;
    }

    /// A seeded schema and a SELECT-only role, dropped when the test ends.
    struct Fixture {
        admin: Admin,
        schema: String,
        role: String,
        ctx: SourceContext,
    }

    impl Fixture {
        async fn new() -> Self {
            let admin = admin();
            let id = suffix();
            let (schema, role) = (format!("zf_{id}"), format!("zf_ro_{id}"));
            let seed = include_str!("../../tests/fixtures/postgres_seed.sql")
                .replace("__SCHEMA__", &schema)
                .replace("__ROLE__", &role);
            admin_exec(&admin, &seed).await;
            Self {
                admin,
                schema,
                role,
                ctx: loopback_context(TYPE),
            }
        }

        fn config(&self, extra: Value) -> Value {
            let mut config = serde_json::json!({
                "host": self.admin.host,
                "port": self.admin.port,
                "database": self.admin.database,
                "schema": self.schema,
                "tls": "off",
            });
            for (key, value) in extra.as_object().unwrap() {
                config[key] = value.clone();
            }
            config
        }

        fn reader(&self) -> Credential {
            credential(&self.role, "zf_test_pw")
        }

        async fn read(
            &self,
            config: &Value,
            stream: &StreamConfig,
            checkpoint: Option<Checkpoint>,
        ) -> Result<Vec<Batch>, SourceError> {
            read_all(
                &Postgres,
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
            let (admin, schema, role) =
                (self.admin.clone(), self.schema.clone(), self.role.clone());
            let _ = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(admin_exec(
                        &admin,
                        &format!(
                            "DROP SCHEMA IF EXISTS {schema} CASCADE; \
                             DROP SCHEMA IF EXISTS {schema}_other CASCADE; \
                             DROP ROLE IF EXISTS {role}"
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_full_refresh_reads_every_row_once_in_key_order_in_batches_of_at_most_500() {
        let fixture = Fixture::new().await;

        let batches = fixture
            .read(
                &fixture.config(serde_json::json!({})),
                &contacts_full(),
                None,
            )
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
            Value::Null,
            "binary columns arrive as null"
        );
        assert_eq!(
            batches[0].records[7].data["updated_at"],
            "2024-01-01 00:08:00+00"
        );
        assert!(!first.deleted);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_incremental_read_skips_the_null_cursor_row_a_full_refresh_has() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));

        let incremental = fixture
            .read(&config, &contacts_incremental(), None)
            .await
            .unwrap();
        let full = fixture.read(&config, &contacts_full(), None).await.unwrap();

        assert_eq!(keys(&incremental), ids(1..=1199));
        assert!(keys(&full).contains(&"1200".to_owned()));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sixteen_rows_sharing_a_cursor_across_a_batch_boundary_are_neither_skipped_nor_repeated()
     {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));

        let batches = fixture
            .read(&config, &contacts_incremental(), None)
            .await
            .unwrap();

        // The first batch ends inside the 16 equal cursor values (ids 493 to 508).
        assert_eq!(batches[0].records.last().unwrap().key, "500");
        assert_eq!(
            batches[0].checkpoint.0,
            serde_json::json!({"v": 1, "key": ["2024-01-01 08:13:00+00", "500"]})
        );
        assert_eq!(keys(&batches), ids(1..=1199));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn resuming_from_the_checkpoint_of_every_batch_returns_exactly_the_remaining_rows() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_composite_primary_key_pages_and_keys_by_all_its_columns() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));
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
        assert_eq!(
            batches[0].checkpoint.0,
            serde_json::json!({"v": 1, "key": ["zz", "1"]})
        );
        // Incremental orders by the cursor first: eu1, us1, eu2, us2, ...
        assert_eq!(
            keys(&by_cursor)[..3],
            [r#"["eu","1"]"#, r#"["us","1"]"#, r#"["eu","2"]"#]
        );
        assert_eq!(keys(&by_cursor).len(), 12);
        let after =
            Checkpoint(serde_json::json!({"v": 1, "key": ["2024-02-01 06:00:00", "us", "6"]}));
        assert!(
            fixture
                .read(&config, &incremental, Some(after))
                .await
                .unwrap()
                .is_empty()
        );
        let after =
            Checkpoint(serde_json::json!({"v": 1, "key": ["2024-02-01 05:00:00", "us", "5"]}));
        let rest = fixture
            .read(&config, &incremental, Some(after))
            .await
            .unwrap();
        assert_eq!(keys(&rest), [r#"["eu","6"]"#, r#"["us","6"]"#]);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_without_a_primary_key_or_with_a_stale_checkpoint_fails_instead_of_guessing() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));

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
                Some(Checkpoint(serde_json::json!({"v": 1, "key": ["7"]}))),
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dropped_cursor_column_fails_the_read_with_a_fixed_message() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));
        let gone = stream("contacts", SyncMode::Incremental, Some("no_such"), &["id"]);

        let error = fixture.read(&config, &gone, None).await.unwrap_err();

        assert_eq!(
            error,
            config_error("cursor_field", "names a column the stream does not have")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_key_of_a_type_that_cannot_be_paged_on_is_refused() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "floaty", "sql": format!("SELECT id, id::float8 AS score FROM {}.contacts", fixture.schema)}]
        }));
        let float_cursor = stream(
            "query:floaty",
            SyncMode::Incremental,
            Some("score"),
            &["id"],
        );
        let bytea_key = stream("contacts", SyncMode::FullRefresh, None, &["payload"]);

        let on_cursor = fixture.read(&config, &float_cursor, None).await;
        let on_key = fixture.read(&config, &bytea_key, None).await;

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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn discovery_lists_tables_with_columns_types_keys_and_the_default_cursor() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "recent", "sql": format!("SELECT id, email, updated_at FROM {}.contacts", fixture.schema)}]
        }));

        let streams = Postgres
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap();

        let names: Vec<_> = streams.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["contacts", "no_key", "pairs", "query:recent"]);
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
        assert_eq!(
            contacts.sync_modes,
            [SyncMode::FullRefresh, SyncMode::Incremental]
        );
        assert_eq!(streams[1].default_primary_key, Vec::<String>::new());
        assert_eq!(streams[2].default_primary_key, ["region", "code"]);
        assert_eq!(
            streams[2].default_cursor_field.as_deref(),
            Some("updated_at")
        );
        let query = &streams[3];
        assert_eq!(query.fields.len(), 3);
        assert_eq!(query.fields[2].field_type, FieldType::Timestamp);
        assert!(query.default_primary_key.is_empty() && query.default_cursor_field.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_query_is_described_with_a_fixed_message_that_does_not_repeat_it() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "bad", "sql": "SELECT sentinel_column FROM nowhere"}]
        }));

        let error = Postgres
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_query_that_returns_a_column_name_twice_is_refused() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "dup", "sql": format!("SELECT id, id FROM {}.contacts", fixture.schema)}]
        }));

        let error = Postgres
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap_err();

        assert_eq!(
            error,
            config_error("queries", "returns a column name twice")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_query_stream_is_read_like_a_table() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "evens", "sql": format!("SELECT id, email FROM {}.contacts WHERE id % 2 = 0", fixture.schema)}]
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_query_slower_than_the_statement_timeout_fails_with_the_fixed_timeout_error() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({
            "statement_timeout_seconds": 1,
            "queries": [{"name": "slow", "sql": format!("SELECT c.id FROM {}.contacts c, pg_sleep(5)", fixture.schema)}]
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
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_write_inside_the_batch_transaction_is_refused_even_for_a_role_that_can_write() {
        let fixture = Fixture::new().await;
        let admin = &fixture.admin;
        let writer = credential(&admin.user, &admin.password);
        let settings =
            Settings::parse(&fixture.config(serde_json::json!({})), Flavor::Postgres).unwrap();
        let login = Login::from_credential(&writer).unwrap();
        let mut conn = open(&fixture.ctx, &settings, &login).await.unwrap();

        let tx = begin_batch(&mut conn.client, settings.statement_timeout)
            .await
            .unwrap();
        let error = tx
            .batch_execute(&format!(
                "INSERT INTO {}.no_key VALUES (2, 'y')",
                fixture.schema
            ))
            .await
            .unwrap_err();

        assert_eq!(error.code(), Some(&SqlState::READ_ONLY_SQL_TRANSACTION));
        assert_eq!(error.code().map(|c| c.code()), Some("25006"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wrong_password_is_auth_and_a_closed_port_is_unreachable() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));
        let closed = fixture.config(serde_json::json!({"host": "127.0.0.1", "port": 1}));

        let wrong = Postgres
            .check(
                &fixture.ctx,
                &config,
                &credential(&fixture.role, "not-the-password"),
            )
            .await;
        let unreachable = Postgres
            .check(&fixture.ctx, &closed, &fixture.reader())
            .await;

        assert_eq!(wrong, Err(SourceError::Auth));
        assert_eq!(unreachable, Err(SourceError::Unreachable));
        let text = format!("{wrong:?}{unreachable:?}");
        for secret in [
            fixture.admin.host.as_str(),
            "not-the-password",
            fixture.role.as_str(),
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn tls_verify_against_a_server_without_tls_is_refused_and_check_passes_with_it_off() {
        let fixture = Fixture::new().await;
        let verify = fixture.config(serde_json::json!({"tls": "verify"}));

        let refused = Postgres
            .check(&fixture.ctx, &verify, &fixture.reader())
            .await;
        let allowed = Postgres
            .check(
                &fixture.ctx,
                &fixture.config(serde_json::json!({})),
                &fixture.reader(),
            )
            .await;

        assert_eq!(refused, Err(tls()));
        assert_eq!(allowed, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn no_error_text_carries_a_seeded_value() {
        let fixture = Fixture::new().await;
        // note holds 'sentinel-secret-value' on id 7; casting it to int fails with a message the
        // server builds from that value.
        let config = fixture.config(serde_json::json!({
            "queries": [{"name": "boom", "sql": format!("SELECT id, note::int AS n FROM {}.contacts", fixture.schema)}]
        }));
        let boom = stream("query:boom", SyncMode::FullRefresh, None, &["id"]);

        let error = fixture.read(&config, &boom, None).await.unwrap_err();

        assert_eq!(error.to_string(), QUERY);
        assert!(!format!("{error:?}").contains("sentinel"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_view_and_a_foreign_schema_are_not_streams_and_a_keyless_table_is() {
        let fixture = Fixture::new().await;
        let config = fixture.config(serde_json::json!({}));

        let streams = Postgres
            .discover(&fixture.ctx, &config, &fixture.reader())
            .await
            .unwrap();

        let names: Vec<_> = streams.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"contacts_view"), "{names:?}");
        assert!(!names.contains(&"hidden"), "{names:?}");
        assert!(names.contains(&"no_key"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_connection_test_passes_for_the_read_only_role() {
        let fixture = Fixture::new().await;

        let result = Postgres
            .check(
                &fixture.ctx,
                &fixture.config(serde_json::json!({})),
                &fixture.reader(),
            )
            .await;

        assert_eq!(result, Ok(()));
    }

    #[test]
    fn the_fixed_messages_read_as_documented() {
        assert_eq!(TIMEOUT, "the database did not answer in time");
        assert_eq!(TLS, "the database connection could not be secured");
        assert_eq!(QUERY, "the database query failed");
    }
}
