//! What can go wrong in a source call. Messages never carry credentials, URLs or query strings.

use std::error::Error;
use std::fmt;

use crate::http::HttpError;

/// Why a source call failed.
#[derive(Debug, Eq, PartialEq)]
pub enum SourceError {
    /// A configuration field is unusable. Names the field and a reason, never its value.
    Config { field: String, reason: String },
    /// The upstream refused the credential.
    Auth,
    /// The upstream could not be reached.
    Unreachable,
    /// The guarded client refused or failed the request.
    Egress(HttpError),
    /// The upstream answered with an error status.
    Upstream(u16),
    /// Any other failure. The text must not contain a credential or a URL.
    Other(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config { field, reason } => write!(f, "the setting {field} {reason}"),
            Self::Auth => f.write_str("the source refused the credential"),
            Self::Unreachable => f.write_str("the source could not be reached"),
            Self::Egress(error) => error.fmt(f),
            Self::Upstream(status) => write!(f, "the source answered with status {status}"),
            Self::Other(reason) => f.write_str(reason),
        }
    }
}

impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Egress(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HttpError> for SourceError {
    fn from(error: HttpError) -> Self {
        Self::Egress(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_reads_without_values() {
        let config = SourceError::Config {
            field: "url".into(),
            reason: "is required".into(),
        };

        assert_eq!(config.to_string(), "the setting url is required");
        assert_eq!(
            SourceError::Auth.to_string(),
            "the source refused the credential"
        );
        assert_eq!(
            SourceError::Unreachable.to_string(),
            "the source could not be reached"
        );
        assert_eq!(
            SourceError::Upstream(503).to_string(),
            "the source answered with status 503"
        );
    }

    #[test]
    fn an_http_error_is_wrapped_and_its_text_kept() {
        let error = SourceError::from(HttpError::Timeout);

        assert_eq!(error.to_string(), HttpError::Timeout.to_string());
        assert!(error.source().is_some());
    }
}
