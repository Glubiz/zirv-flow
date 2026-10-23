//! What a destination's answer means: succeeded, failed for good, or worth another attempt. The
//! rules are those of `webhook.call` (architecture 9.3, rulings 9cb0bea9 and a37fbac4), and every
//! outbound job uses them.
//!
//! - **A 2xx response succeeds.**
//! - **A 408, a 429, a 5xx, a timeout or a transport error is retried**, after the `Retry-After`
//!   the destination named when that is longer than the worker backoff.
//! - **Anything else fails at once:** any other 4xx, an unexpected 1xx or 3xx (a redirect is never
//!   followed), a destination the egress policy refuses, and a setting or credential that cannot
//!   work.

use std::time::Duration;

use chrono::{DateTime, Utc};
use time::OffsetDateTime;
use zirv_flow_connectors::destinations::{
    Delivered, DestinationError, Refusal, parse_retry_after as connector_parse,
};
use zirv_flow_connectors::http::HttpError;
use zirv_flow_store::{DELIVERY_UNCONFIRMED, HARD_BOUNCE};

/// The reason of a message a mail server refused for a cause that is not the recipient.
pub(crate) const SMTP_REJECTED: &str = "smtp_rejected";
/// The reason of an attempt a mail server asked to repeat later (a 4xx reply).
pub(crate) const SMTP_DEFERRED: &str = "smtp_4xx";

/// How an attempt went.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    /// A 2xx response.
    Succeeded { status: u16 },
    /// Retrying cannot help.
    Terminal {
        status: Option<u16>,
        reason: &'static str,
    },
    /// Worth another attempt, after at least `retry_after` when the destination named one.
    Retry {
        status: Option<u16>,
        reason: &'static str,
        message: &'static str,
        retry_after: Option<Duration>,
    },
}

/// What a response status means. `retry_after` is the destination's `Retry-After`, used only by
/// a status that is retried.
pub(crate) fn judge(status: u16, retry_after: Option<Duration>) -> Verdict {
    let retry = |reason, message| Verdict::Retry {
        status: Some(status),
        reason,
        message,
        retry_after,
    };
    match status {
        200..=299 => Verdict::Succeeded { status },
        408 => retry("http_408", "the destination answered 408 request timeout"),
        429 => retry("http_429", "the destination answered 429 too many requests"),
        500..=599 => retry("http_5xx", "the destination answered with a server error"),
        400..=499 => Verdict::Terminal {
            status: Some(status),
            reason: "http_4xx",
        },
        _ => Verdict::Terminal {
            status: Some(status),
            reason: "http_status",
        },
    }
}

/// What a failed send means.
pub(crate) fn judge_error(error: &HttpError) -> Verdict {
    match error {
        HttpError::EgressDenied { .. } => Verdict::Terminal {
            status: None,
            reason: "egress_denied",
        },
        HttpError::Timeout => Verdict::Retry {
            status: None,
            reason: "timeout",
            message: "the request to the destination timed out",
            retry_after: None,
        },
        HttpError::BodyTooLarge | HttpError::Transport(_) => transport_failed(),
    }
}

/// The request never got an answer.
fn transport_failed() -> Verdict {
    Verdict::Retry {
        status: None,
        reason: "transport",
        message: "the request to the destination failed",
        retry_after: None,
    }
}

/// The wait a `Retry-After` header asks for: delta-seconds, or an HTTP date counted from `now`.
/// A date in the past, or text that is neither (a signed number included), asks for nothing.
///
/// The reading is the connectors' ([`connector_parse`]), the one parser every destination type
/// shares; only the "a past date asks for nothing" rule is added here, where the connectors'
/// answer a past date with a wait of zero.
pub(crate) fn parse_retry_after(value: &str, now: OffsetDateTime) -> Option<Duration> {
    let at = DateTime::<Utc>::from_timestamp(now.unix_timestamp(), now.nanosecond())?;
    let wait = connector_parse(Some(value), at)?;
    let seconds = value.trim().bytes().all(|byte| byte.is_ascii_digit());
    (seconds || !wait.is_zero()).then_some(wait)
}

/// What a destination's result means at `now`. The one function every outbound job judges
/// with: a status goes through [`judge`] and a transport failure through [`judge_error`].
pub(crate) fn verdict_of(
    result: &Result<Delivered, DestinationError>,
    now: OffsetDateTime,
) -> Verdict {
    match result {
        // A delivery with no status (a type that does not speak HTTP) is a success.
        Ok(delivered) => judge(delivered.status.unwrap_or(200), None),
        Err(DestinationError::Answered {
            status,
            retry_after,
        }) => judge(
            *status,
            retry_after
                .as_deref()
                .and_then(|value| parse_retry_after(value, now)),
        ),
        Err(DestinationError::Upstream(status)) => judge(*status, None),
        Err(DestinationError::RateLimited { retry_after }) => Verdict::Retry {
            status: Some(429),
            reason: "http_429",
            message: "the destination answered 429 too many requests",
            retry_after: *retry_after,
        },
        Err(DestinationError::Egress(error)) => judge_error(error),
        Err(DestinationError::Unreachable) => transport_failed(),
        Err(DestinationError::Refused(Refusal::HardBounce)) => Verdict::Terminal {
            status: None,
            reason: HARD_BOUNCE,
        },
        Err(DestinationError::Refused(Refusal::Rejected)) => Verdict::Terminal {
            status: None,
            reason: SMTP_REJECTED,
        },
        Err(DestinationError::Refused(Refusal::Deferred)) => Verdict::Retry {
            status: None,
            reason: SMTP_DEFERRED,
            message: "the mail server asked to try again later",
            retry_after: None,
        },
        // The server may have accepted the message: retried under the same key, and when the
        // attempts run out the message ends `delivery_unconfirmed`, never a transport failure.
        Err(DestinationError::Unconfirmed) => Verdict::Retry {
            status: None,
            reason: DELIVERY_UNCONFIRMED,
            message: "the mail server did not confirm the message",
            retry_after: None,
        },
        Err(DestinationError::Other(_)) => Verdict::Retry {
            status: None,
            reason: "other",
            message: "the destination call failed",
            retry_after: None,
        },
        Err(DestinationError::Config { .. }) => Verdict::Terminal {
            status: None,
            reason: "destination_config",
        },
        Err(DestinationError::Auth) => Verdict::Terminal {
            status: None,
            reason: "destination_auth",
        },
        Err(DestinationError::Unsupported) => Verdict::Terminal {
            status: None,
            reason: "destination_unsupported",
        },
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;
    use zirv_flow_connectors::destinations::Disposition;
    use zirv_flow_core::egress::AddressClass;

    use super::*;

    #[test]
    fn every_error_of_a_destination_has_a_verdict_that_agrees_with_its_disposition() {
        let now = datetime!(2031-05-06 07:08:09 UTC);
        let errors = [
            DestinationError::Config {
                field: "url".into(),
                reason: "is required".into(),
            },
            DestinationError::Auth,
            DestinationError::Unreachable,
            DestinationError::Egress(HttpError::Timeout),
            DestinationError::Egress(HttpError::Transport("x".into())),
            DestinationError::Upstream(404),
            DestinationError::Upstream(408),
            DestinationError::Upstream(429),
            DestinationError::Upstream(503),
            DestinationError::Upstream(302),
            DestinationError::Answered {
                status: 500,
                retry_after: None,
            },
            DestinationError::RateLimited { retry_after: None },
            DestinationError::Unsupported,
            DestinationError::Other("x".into()),
            DestinationError::Refused(Refusal::HardBounce),
            DestinationError::Refused(Refusal::Rejected),
            DestinationError::Refused(Refusal::Deferred),
            DestinationError::Unconfirmed,
        ];

        for error in errors {
            let disposition = error.disposition();
            let verdict = verdict_of(&Err(error), now);

            let retried = matches!(verdict, Verdict::Retry { .. });
            assert_eq!(
                retried,
                disposition != Disposition::Terminal,
                "{verdict:?} {disposition:?}"
            );
        }
    }

    #[test]
    fn a_delivery_succeeds_and_a_retry_after_is_read_against_the_given_time() {
        let now = datetime!(1994-11-06 08:49:00 UTC);

        assert_eq!(
            verdict_of(
                &Ok(Delivered {
                    status: Some(204),
                    remote_id: None
                }),
                now
            ),
            Verdict::Succeeded { status: 204 }
        );
        assert_eq!(
            verdict_of(&Ok(Delivered::default()), now),
            Verdict::Succeeded { status: 200 }
        );
        for (header, wait) in [
            ("120", Some(Duration::from_secs(120))),
            (
                "Sun, 06 Nov 1994 08:49:37 GMT",
                Some(Duration::from_secs(37)),
            ),
            ("soon", None),
        ] {
            let answered = Err(DestinationError::Answered {
                status: 503,
                retry_after: Some(header.to_owned()),
            });

            assert!(
                matches!(
                    verdict_of(&answered, now),
                    Verdict::Retry { status: Some(503), retry_after, .. } if retry_after == wait
                ),
                "{header}"
            );
        }
    }

    #[test]
    fn a_mail_servers_refusals_have_their_own_reasons_and_only_a_deferral_retries() {
        let now = datetime!(2031-05-06 07:08:09 UTC);

        assert_eq!(
            verdict_of(&Err(DestinationError::Refused(Refusal::HardBounce)), now),
            Verdict::Terminal {
                status: None,
                reason: "hard_bounce"
            }
        );
        assert_eq!(
            verdict_of(&Err(DestinationError::Refused(Refusal::Rejected)), now),
            Verdict::Terminal {
                status: None,
                reason: "smtp_rejected"
            }
        );
        assert!(matches!(
            verdict_of(&Err(DestinationError::Refused(Refusal::Deferred)), now),
            Verdict::Retry {
                status: None,
                reason: "smtp_4xx",
                retry_after: None,
                ..
            }
        ));
    }

    #[test]
    fn a_reply_lost_after_the_final_dot_retries_under_the_unconfirmed_reason() {
        let now = datetime!(2031-05-06 07:08:09 UTC);

        assert!(matches!(
            verdict_of(&Err(DestinationError::Unconfirmed), now),
            Verdict::Retry {
                status: None,
                reason: "delivery_unconfirmed",
                retry_after: None,
                ..
            }
        ));
    }

    #[test]
    fn the_egress_denial_and_the_settings_that_cannot_work_are_terminal() {
        let now = datetime!(2031-05-06 07:08:09 UTC);
        let denied = HttpError::EgressDenied {
            host: "10.0.0.5".into(),
            class: AddressClass::Private,
        };

        assert_eq!(
            verdict_of(&Err(DestinationError::Egress(denied)), now),
            Verdict::Terminal {
                status: None,
                reason: "egress_denied"
            }
        );
        assert_eq!(
            verdict_of(&Err(DestinationError::Auth), now),
            Verdict::Terminal {
                status: None,
                reason: "destination_auth"
            }
        );
    }
}
