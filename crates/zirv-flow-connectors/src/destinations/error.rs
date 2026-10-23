//! What can go wrong in a destination call, and what the caller does about it. Messages never
//! carry credentials, addresses, URLs or query strings.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use crate::http::HttpError;

/// Why a destination call failed.
#[derive(Debug, Eq, PartialEq)]
pub enum DestinationError {
    /// A configuration field is unusable. Names the field and a reason, never its value.
    Config { field: String, reason: String },
    /// The remote system refused the credential.
    Auth,
    /// The remote system could not be reached.
    Unreachable,
    /// The guarded client refused or failed the request.
    Egress(HttpError),
    /// The remote system answered with an error status.
    Upstream(u16),
    /// As [`Upstream`](Self::Upstream), with the raw `Retry-After` header the answer carried. The
    /// pipeline reads it against its own clock, so a date is counted from the time it judges.
    Answered {
        status: u16,
        retry_after: Option<String>,
    },
    /// The remote system asked for fewer requests (HTTP 429), optionally for how long.
    RateLimited { retry_after: Option<Duration> },
    /// A mail server refused the message or asked to try later, as a classified verdict
    /// (architecture 10.2, OQ-12). Only a mail destination returns it.
    Refused(Refusal),
    /// A mail server was sent the end of the message and then the connection was lost, or no
    /// readable answer came: the message may or may not have been accepted. Only a mail
    /// destination returns it. It is retried under the same message id and idempotency key; when
    /// the attempts run out the message ends unconfirmed, not failed for a transport reason.
    Unconfirmed,
    /// The type does not do this (an erase call on a type without the capability).
    Unsupported,
    /// Any other failure. The text must not contain a credential, an address or a URL.
    Other(String),
}

/// What a mail server's refusal means for the message (OQ-12). The reply text is not kept: it can
/// name the recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// A permanent failure of the recipient's address, at `RCPT` or `DATA`: the message bounces
    /// hard and the address is suppressed.
    HardBounce,
    /// A permanent failure that is not about the recipient (authentication, relay policy, content,
    /// size): the message fails and nobody is suppressed.
    Rejected,
    /// A temporary failure (a 4xx reply): try again; when the attempts run out the message is a
    /// soft bounce, with no suppression.
    Deferred,
}

/// What the delivery pipeline does with a failed call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    /// No retry can succeed: record the failure and stop.
    Terminal,
    /// Try again later with the job retry machinery.
    Retryable,
    /// Slow the destination down (halve its rate until `retry_after`) and try again.
    Throttled { retry_after: Option<Duration> },
}

impl DestinationError {
    /// How the pipeline treats this failure.
    ///
    /// An egress denial, a bad setting, a refused credential and a missing capability are
    /// terminal: waiting changes nothing. An unreachable host, a transport failure, a 408, a 5xx
    /// or an unclassified failure retries. A 429 throttles. Any other 4xx is terminal, and so is a
    /// status outside 200 to 599 that is not a success (a redirect is never followed).
    pub fn disposition(&self) -> Disposition {
        match self {
            Self::Config { .. }
            | Self::Auth
            | Self::Unsupported
            | Self::Egress(HttpError::EgressDenied { .. }) => Disposition::Terminal,
            Self::Egress(_) | Self::Unreachable | Self::Unconfirmed | Self::Other(_) => {
                Disposition::Retryable
            }
            Self::Refused(Refusal::Deferred) => Disposition::Retryable,
            Self::Refused(Refusal::HardBounce | Refusal::Rejected) => Disposition::Terminal,
            Self::RateLimited { retry_after } => Disposition::Throttled {
                retry_after: *retry_after,
            },
            Self::Upstream(status) | Self::Answered { status, .. } => match status {
                429 => Disposition::Throttled { retry_after: None },
                408 | 500..=599 => Disposition::Retryable,
                _ => Disposition::Terminal,
            },
        }
    }
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config { field, reason } => write!(f, "the setting {field} {reason}"),
            Self::Auth => f.write_str("the destination refused the credential"),
            Self::Unreachable => f.write_str("the destination could not be reached"),
            Self::Egress(error) => error.fmt(f),
            Self::Upstream(status) | Self::Answered { status, .. } => {
                write!(f, "the destination answered with status {status}")
            }
            Self::RateLimited { .. } => f.write_str("the destination asked for a slower rate"),
            Self::Refused(Refusal::HardBounce) => {
                f.write_str("the mail server refused the recipient for good")
            }
            Self::Refused(Refusal::Rejected) => {
                f.write_str("the mail server refused the message for good")
            }
            Self::Refused(Refusal::Deferred) => {
                f.write_str("the mail server asked to try again later")
            }
            Self::Unconfirmed => {
                f.write_str("the mail server did not confirm whether it accepted the message")
            }
            Self::Unsupported => f.write_str("the destination type does not support this"),
            Self::Other(reason) => f.write_str(reason),
        }
    }
}

impl Error for DestinationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Egress(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HttpError> for DestinationError {
    fn from(error: HttpError) -> Self {
        Self::Egress(error)
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::egress::AddressClass;

    use super::*;

    #[test]
    fn each_error_reads_without_values() {
        let config = DestinationError::Config {
            field: "host".into(),
            reason: "is required".into(),
        };

        assert_eq!(config.to_string(), "the setting host is required");
        assert_eq!(
            DestinationError::Auth.to_string(),
            "the destination refused the credential"
        );
        assert_eq!(
            DestinationError::Upstream(503).to_string(),
            "the destination answered with status 503"
        );
        assert!(
            DestinationError::Egress(HttpError::Timeout)
                .source()
                .is_some()
        );
    }

    #[test]
    fn an_egress_denial_is_terminal_and_a_transport_failure_retries() {
        let denied = DestinationError::from(HttpError::EgressDenied {
            host: "db.test".into(),
            class: AddressClass::Private,
        });

        assert_eq!(denied.disposition(), Disposition::Terminal);
        assert_eq!(
            DestinationError::from(HttpError::Timeout).disposition(),
            Disposition::Retryable
        );
    }

    #[test]
    fn a_408_retries_and_an_unexpected_status_is_terminal_and_the_header_does_not_change_it() {
        for status in [408, 500, 503, 599] {
            assert_eq!(
                DestinationError::Upstream(status).disposition(),
                Disposition::Retryable,
                "{status}"
            );
        }
        for status in [100, 301, 302, 307, 404, 600] {
            assert_eq!(
                DestinationError::Upstream(status).disposition(),
                Disposition::Terminal,
                "{status}"
            );
        }
        let answered = DestinationError::Answered {
            status: 503,
            retry_after: Some("120".into()),
        };
        assert_eq!(answered.disposition(), Disposition::Retryable);
        assert_eq!(
            DestinationError::Answered {
                status: 429,
                retry_after: None
            }
            .disposition(),
            Disposition::Throttled { retry_after: None }
        );
        assert_eq!(
            answered.to_string(),
            "the destination answered with status 503"
        );
    }

    #[test]
    fn a_mail_servers_refusal_is_terminal_unless_it_is_temporary_and_reads_without_the_reply() {
        assert_eq!(
            DestinationError::Refused(Refusal::HardBounce).disposition(),
            Disposition::Terminal
        );
        assert_eq!(
            DestinationError::Refused(Refusal::Rejected).disposition(),
            Disposition::Terminal
        );
        assert_eq!(
            DestinationError::Refused(Refusal::Deferred).disposition(),
            Disposition::Retryable
        );
        assert_eq!(
            DestinationError::Refused(Refusal::HardBounce).to_string(),
            "the mail server refused the recipient for good"
        );
    }

    #[test]
    fn statuses_are_classified() {
        let pause = Some(Duration::from_secs(3));

        assert_eq!(
            DestinationError::Upstream(429).disposition(),
            Disposition::Throttled { retry_after: None }
        );
        assert_eq!(
            DestinationError::RateLimited { retry_after: pause }.disposition(),
            Disposition::Throttled { retry_after: pause }
        );
        assert_eq!(
            DestinationError::Upstream(550).disposition(),
            Disposition::Retryable
        );
        assert_eq!(
            DestinationError::Upstream(422).disposition(),
            Disposition::Terminal
        );
        assert_eq!(DestinationError::Auth.disposition(), Disposition::Terminal);
        assert_eq!(
            DestinationError::Unsupported.disposition(),
            Disposition::Terminal
        );
        assert_eq!(
            DestinationError::Unreachable.disposition(),
            Disposition::Retryable
        );
    }
}
