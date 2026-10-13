//! Maps driver errors to the fixed messages of [`SourceError`]. A server's error text can quote the
//! value that violated a constraint or a type, so no driver text is ever forwarded: the log gets
//! the source type and the SQLSTATE or error number, and the caller a fixed message.

use std::error::Error as _;
use std::io;

use crate::sources::SourceError;

pub(crate) const TIMEOUT: &str = "the database did not answer in time";
pub(crate) const TLS: &str = "the database connection could not be secured";
pub(crate) const READ_ONLY: &str = "the database refused a write";
pub(crate) const QUERY: &str = "the database query failed";
pub(crate) const NULL_KEY: &str = "a primary key value is null";

pub(crate) fn timeout() -> SourceError {
    SourceError::Other(TIMEOUT.to_owned())
}

pub(crate) fn tls() -> SourceError {
    SourceError::Other(TLS.to_owned())
}

pub(crate) fn query() -> SourceError {
    SourceError::Other(QUERY.to_owned())
}

/// The metric label of a failure: one of `auth`, `unreachable`, `egress`, `timeout`, `tls`,
/// `read_only`, `config` or `query`.
pub(crate) fn reason_of(error: &SourceError) -> &'static str {
    match error {
        SourceError::Auth => "auth",
        SourceError::Unreachable => "unreachable",
        SourceError::Egress(_) => "egress",
        SourceError::Config { .. } => "config",
        SourceError::Upstream(_) => "query",
        SourceError::Other(text) => match text.as_str() {
            TIMEOUT => "timeout",
            TLS => "tls",
            READ_ONLY => "read_only",
            _ => "query",
        },
    }
}

/// What a PostgreSQL SQLSTATE means for the caller.
pub(crate) fn from_sqlstate(code: &str) -> SourceError {
    match code {
        "28P01" | "28000" => SourceError::Auth,
        "57014" => timeout(),
        "25006" => SourceError::Other(READ_ONLY.to_owned()),
        _ => query(),
    }
}

/// What a MySQL error number means for the caller.
pub(crate) fn from_mysql_code(code: u16) -> SourceError {
    match code {
        1044 | 1045 => SourceError::Auth,
        3024 => timeout(),
        1792 | 1290 => SourceError::Other(READ_ONLY.to_owned()),
        _ => query(),
    }
}

/// Maps a `tokio-postgres` error. Logs the SQLSTATE when the server sent one.
pub(crate) fn from_postgres(
    source_type: &'static str,
    error: &tokio_postgres::Error,
) -> SourceError {
    if let Some(db) = error.as_db_error() {
        let code = db.code().code();
        tracing::warn!(source_type, sqlstate = code, "database call failed");
        return from_sqlstate(code);
    }
    let mapped = if error.is_closed() {
        SourceError::Unreachable
    } else if mentions_tls(error) {
        tls()
    } else if has_io_source(error) {
        SourceError::Unreachable
    } else {
        query()
    };
    tracing::warn!(source_type, "database call failed without a server code");
    mapped
}

/// Maps a `mysql_async` error. Logs the error number when the server sent one.
pub(crate) fn from_mysql(source_type: &'static str, error: &mysql_async::Error) -> SourceError {
    use mysql_async::{DriverError, Error, IoError};
    let mapped = match error {
        Error::Server(server) => {
            tracing::warn!(
                source_type,
                error_number = server.code,
                "database call failed"
            );
            return from_mysql_code(server.code);
        }
        Error::Io(IoError::Tls(_)) => tls(),
        Error::Io(IoError::Io(io)) => {
            if io_is_tls(io) {
                tls()
            } else {
                SourceError::Unreachable
            }
        }
        Error::Driver(DriverError::ConnectionClosed) => SourceError::Unreachable,
        Error::Driver(DriverError::NoClientSslFlagFromServer) => tls(),
        _ => query(),
    };
    tracing::warn!(source_type, "database call failed without a server code");
    mapped
}

fn mentions_tls(error: &tokio_postgres::Error) -> bool {
    // The driver's own wording ("error performing TLS handshake", "server does not support
    // TLS"), used only to classify, never forwarded.
    if error.to_string().contains("TLS") {
        return true;
    }
    let mut source = error.source();
    while let Some(cause) = source {
        if cause.downcast_ref::<rustls::Error>().is_some() {
            return true;
        }
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && io_is_tls(io)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

fn has_io_source(error: &tokio_postgres::Error) -> bool {
    let mut source = error.source();
    while let Some(cause) = source {
        if cause.downcast_ref::<io::Error>().is_some() {
            return true;
        }
        source = cause.source();
    }
    false
}

fn io_is_tls(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sqlstate_maps_to_a_fixed_error() {
        assert_eq!(from_sqlstate("28P01"), SourceError::Auth);
        assert_eq!(from_sqlstate("28000"), SourceError::Auth);
        assert_eq!(from_sqlstate("57014"), timeout());
        assert_eq!(from_sqlstate("25006"), SourceError::Other(READ_ONLY.into()));
        // A unique violation quotes the value in its message; the code alone becomes a fixed text.
        assert_eq!(from_sqlstate("23505"), query());
        assert_eq!(from_sqlstate("22P02"), query());
    }

    #[test]
    fn a_mysql_error_number_maps_to_a_fixed_error() {
        assert_eq!(from_mysql_code(1045), SourceError::Auth);
        assert_eq!(from_mysql_code(3024), timeout());
        assert_eq!(from_mysql_code(1792), SourceError::Other(READ_ONLY.into()));
        assert_eq!(from_mysql_code(1062), query());
    }

    #[test]
    fn no_mapped_message_carries_input_text() {
        let messages = [
            timeout(),
            tls(),
            query(),
            SourceError::Other(READ_ONLY.into()),
            SourceError::Other(NULL_KEY.into()),
        ];
        for message in messages {
            let text = message.to_string();
            for forbidden in ["host", "SELECT", "password", "'", "\""] {
                assert!(!text.contains(forbidden), "{text}");
            }
        }
    }

    #[test]
    fn each_failure_has_a_metric_reason() {
        assert_eq!(reason_of(&SourceError::Auth), "auth");
        assert_eq!(reason_of(&SourceError::Unreachable), "unreachable");
        assert_eq!(reason_of(&timeout()), "timeout");
        assert_eq!(reason_of(&tls()), "tls");
        assert_eq!(reason_of(&from_sqlstate("25006")), "read_only");
        assert_eq!(reason_of(&query()), "query");
        assert_eq!(
            reason_of(&SourceError::Config {
                field: "x".into(),
                reason: "y".into()
            }),
            "config"
        );
        assert_eq!(
            reason_of(&SourceError::Egress(crate::http::HttpError::Timeout)),
            "egress"
        );
    }
}
