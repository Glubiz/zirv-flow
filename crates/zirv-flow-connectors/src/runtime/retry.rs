//! Which outcomes are retried, how long to wait, and how `Retry-After` is read.

use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use zirv_flow_core::schedule::backoff;

use super::error::RuntimeError;
use crate::http::HttpError;

/// How a call is retried. The defaults are 5 attempts, a 1 s base doubling to a 60 s cap, and a
/// refusal to wait out a `Retry-After` of more than 300 s.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Attempts in all, the first included.
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
    /// The longest `Retry-After` the call waits out.
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            max_wait: Duration::from_secs(300),
        }
    }
}

/// Why an attempt is retried; the label of `zirv_flow_source_retries_total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetryReason {
    RateLimited,
    Unavailable,
    ServerError,
    Transport,
}

impl RetryReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::ServerError => "server_error",
            Self::Transport => "transport",
        }
    }

    /// Whether a `Retry-After` header counts for this reason.
    fn honours_retry_after(self) -> bool {
        matches!(self, Self::RateLimited | Self::Unavailable)
    }
}

/// The retry reason of a status, if it is worth retrying: 429, 500, 502, 503 and 504.
pub(crate) fn status_reason(status: StatusCode) -> Option<RetryReason> {
    match status.as_u16() {
        429 => Some(RetryReason::RateLimited),
        503 => Some(RetryReason::Unavailable),
        500 | 502 | 504 => Some(RetryReason::ServerError),
        _ => None,
    }
}

/// The retry reason of a failed exchange: a timeout or a connection failure. A refusal by the
/// egress policy or an oversized body would fail the same way again.
pub(crate) fn error_reason(error: &HttpError) -> Option<RetryReason> {
    match error {
        HttpError::Timeout | HttpError::Transport(_) | HttpError::NotSent { .. } => {
            Some(RetryReason::Transport)
        }
        HttpError::EgressDenied { .. } | HttpError::BodyTooLarge => None,
    }
}

/// Reads `Retry-After`: delta-seconds, or an HTTP date relative to `now`. A date in the past is
/// no wait. Anything unreadable is `None`.
pub(crate) fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = DateTime::parse_from_rfc2822(value).ok()?;
    let wait = date.with_timezone(&Utc) - now;
    Some(wait.to_std().unwrap_or(Duration::ZERO))
}

impl RetryPolicy {
    /// How long to wait before the next attempt, after `failed` attempts have failed.
    /// `Retry-After` takes precedence over the backoff when the reason honours it, and one over
    /// `max_wait` ends the call. `random_unit` is a random number in `[0, 1]` for the jitter.
    pub(crate) fn delay(
        &self,
        failed: u32,
        reason: RetryReason,
        retry_after: Option<Duration>,
        random_unit: f64,
    ) -> Result<Duration, RuntimeError> {
        if let Some(wait) = retry_after.filter(|_| reason.honours_retry_after()) {
            if wait > self.max_wait {
                return Err(RuntimeError::RetryAfterTooLong {
                    seconds: wait.as_secs(),
                });
            }
            return Ok(wait);
        }
        Ok(backoff(
            failed.saturating_sub(1),
            self.base,
            self.cap,
            random_unit,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc2822(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn exactly_the_transient_statuses_are_retried() {
        for retried in [429, 500, 502, 503, 504] {
            assert!(status_reason(StatusCode::from_u16(retried).unwrap()).is_some());
        }
        for final_status in [200, 301, 400, 401, 403, 404, 410, 422, 501, 505] {
            assert!(status_reason(StatusCode::from_u16(final_status).unwrap()).is_none());
        }
        assert_eq!(
            status_reason(StatusCode::TOO_MANY_REQUESTS),
            Some(RetryReason::RateLimited)
        );
        assert_eq!(
            status_reason(StatusCode::SERVICE_UNAVAILABLE),
            Some(RetryReason::Unavailable)
        );
    }

    #[test]
    fn a_timeout_and_a_connect_failure_are_retried_but_a_policy_refusal_is_not() {
        assert!(error_reason(&HttpError::Timeout).is_some());
        assert!(error_reason(&HttpError::Transport("could not connect".into())).is_some());
        assert!(error_reason(&HttpError::BodyTooLarge).is_none());
        let denied = HttpError::EgressDenied {
            host: "h".into(),
            class: zirv_flow_core::egress::AddressClass::Private,
        };
        assert!(error_reason(&denied).is_none());
    }

    #[test]
    fn retry_after_reads_delta_seconds_and_http_dates() {
        let now = at("Wed, 21 Oct 2015 07:28:00 GMT");

        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after(" 0 ", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:30 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
        assert_eq!(parse_retry_after("", now), None);
    }

    #[test]
    fn the_delay_backs_off_from_the_base_and_stops_at_the_cap() {
        let policy = RetryPolicy::default();
        let delay = |failed| {
            policy
                .delay(failed, RetryReason::Transport, None, 1.0)
                .unwrap()
        };

        assert_eq!(delay(1), Duration::from_secs(1));
        assert_eq!(delay(2), Duration::from_secs(2));
        assert_eq!(delay(4), Duration::from_secs(8));
        assert_eq!(delay(30), Duration::from_secs(60));
    }

    #[test]
    fn retry_after_beats_the_backoff_only_for_a_429_or_a_503() {
        let policy = RetryPolicy::default();
        let wait = Some(Duration::from_secs(7));

        for reason in [RetryReason::RateLimited, RetryReason::Unavailable] {
            assert_eq!(
                policy.delay(1, reason, wait, 1.0).unwrap(),
                Duration::from_secs(7)
            );
        }
        assert_eq!(
            policy
                .delay(1, RetryReason::ServerError, wait, 1.0)
                .unwrap(),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn a_retry_after_over_the_cap_is_an_error_and_one_at_the_cap_is_waited() {
        let policy = RetryPolicy::default();
        let over = Some(Duration::from_secs(301));
        let at_cap = Some(Duration::from_secs(300));

        assert_eq!(
            policy.delay(1, RetryReason::RateLimited, over, 1.0),
            Err(RuntimeError::RetryAfterTooLong { seconds: 301 })
        );
        assert_eq!(
            policy
                .delay(1, RetryReason::RateLimited, at_cap, 1.0)
                .unwrap(),
            Duration::from_secs(300)
        );
    }
}
