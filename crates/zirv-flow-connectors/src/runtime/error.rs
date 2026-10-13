//! What can go wrong in a runtime call. Neither `Display` nor `Debug` ever carries a URL (its
//! query can hold an API key), a header, a request body or a response body: only the variant,
//! a status and an attempt count.

use std::error::Error;
use std::fmt;

use crate::http::HttpError;
use crate::sources::SourceError;

/// Why a runtime call failed.
#[derive(Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// The request was for an origin other than the source's base origin. Nothing was sent.
    CrossOrigin,
    /// The upstream refused the credential, after the one token refresh an OAuth2 source gets.
    Unauthorized,
    /// The OAuth2 token endpoint did not give a token.
    TokenEndpoint { status: Option<u16> },
    /// The upstream answered with a status the runtime does not retry.
    Status(u16),
    /// Every attempt failed with a retryable outcome. `last_status` is `None` for a transport
    /// failure.
    RetriesExhausted {
        last_status: Option<u16>,
        attempts: u32,
    },
    /// `Retry-After` asked for a longer wait than the policy allows. The call did not sleep.
    RetryAfterTooLong { seconds: u64 },
    /// A cursor or next link came back that was already followed.
    PaginationLoop,
    /// A cursor or next link was longer than the driver keeps track of (8 KiB).
    PaginationTokenTooLong,
    /// A body was not the JSON shape the caller expected.
    UnexpectedShape,
    /// The request could not be built, for example a header value with a control character.
    InvalidRequest,
    /// The guarded client refused or failed the request, and it is not worth retrying.
    Http(HttpError),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CrossOrigin => {
                f.write_str("the request was for an origin the source does not own")
            }
            Self::Unauthorized => f.write_str("the source refused the credential"),
            Self::TokenEndpoint {
                status: Some(status),
            } => write!(f, "the token endpoint answered with status {status}"),
            Self::TokenEndpoint { status: None } => {
                f.write_str("the token endpoint did not give a usable token")
            }
            Self::Status(status) => write!(f, "the source answered with status {status}"),
            Self::RetriesExhausted {
                last_status: Some(status),
                attempts,
            } => write!(
                f,
                "the source still answered with status {status} after {attempts} attempts"
            ),
            Self::RetriesExhausted {
                last_status: None,
                attempts,
            } => write!(
                f,
                "the source could not be reached after {attempts} attempts"
            ),
            Self::RetryAfterTooLong { seconds } => write!(
                f,
                "the source asked to wait {seconds} seconds, which is too long"
            ),
            Self::PaginationLoop => f.write_str("the source repeated a page it had already sent"),
            Self::PaginationTokenTooLong => {
                f.write_str("the source sent a cursor or link that is too long to follow")
            }
            Self::UnexpectedShape => f.write_str("the source answered with an unexpected shape"),
            Self::InvalidRequest => f.write_str("the request could not be built"),
            Self::Http(error) => error.fmt(f),
        }
    }
}

impl Error for RuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HttpError> for RuntimeError {
    fn from(error: HttpError) -> Self {
        Self::Http(error)
    }
}

impl From<RuntimeError> for SourceError {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::Unauthorized | RuntimeError::TokenEndpoint { .. } => Self::Auth,
            RuntimeError::Status(status) => Self::Upstream(status),
            RuntimeError::RetriesExhausted {
                last_status: Some(status),
                ..
            } => Self::Upstream(status),
            RuntimeError::RetriesExhausted {
                last_status: None, ..
            } => Self::Unreachable,
            RuntimeError::Http(error) => Self::Egress(error),
            other => Self::Other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_reads_with_a_status_or_count_and_nothing_else() {
        assert_eq!(
            RuntimeError::RetriesExhausted {
                last_status: Some(503),
                attempts: 5
            }
            .to_string(),
            "the source still answered with status 503 after 5 attempts"
        );
        assert_eq!(
            RuntimeError::RetriesExhausted {
                last_status: None,
                attempts: 5
            }
            .to_string(),
            "the source could not be reached after 5 attempts"
        );
        assert_eq!(
            RuntimeError::RetryAfterTooLong { seconds: 900 }.to_string(),
            "the source asked to wait 900 seconds, which is too long"
        );
        assert_eq!(
            RuntimeError::TokenEndpoint { status: Some(400) }.to_string(),
            "the token endpoint answered with status 400"
        );
        assert_eq!(
            RuntimeError::Status(404).to_string(),
            "the source answered with status 404"
        );
    }

    #[test]
    fn a_runtime_error_maps_to_the_source_error_a_caller_acts_on() {
        assert_eq!(
            SourceError::from(RuntimeError::Unauthorized),
            SourceError::Auth
        );
        assert_eq!(
            SourceError::from(RuntimeError::TokenEndpoint { status: None }),
            SourceError::Auth
        );
        assert_eq!(
            SourceError::from(RuntimeError::Status(404)),
            SourceError::Upstream(404)
        );
        assert_eq!(
            SourceError::from(RuntimeError::RetriesExhausted {
                last_status: Some(429),
                attempts: 5
            }),
            SourceError::Upstream(429)
        );
        assert_eq!(
            SourceError::from(RuntimeError::RetriesExhausted {
                last_status: None,
                attempts: 5
            }),
            SourceError::Unreachable
        );
        assert_eq!(
            SourceError::from(RuntimeError::Http(HttpError::Timeout)),
            SourceError::Egress(HttpError::Timeout)
        );
        assert!(matches!(
            SourceError::from(RuntimeError::CrossOrigin),
            SourceError::Other(text) if !text.contains("http")
        ));
    }
}
