//! The `Retry-After` header of a 429 or 503 answer (RFC 9110 section 10.2.3), parsed once for
//! every destination type. The value is either delta-seconds or an HTTP date in the IMF-fixdate
//! form (`Sun, 06 Nov 1994 08:49:37 GMT`).
//!
//! The parser is pure: the caller passes the current time, so a test needs no clock.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// The longest wait the parser returns, however large the header's value is. The store caps a
/// penalty window at the same hour (`MAX_RETRY_AFTER`), so a hostile or broken header cannot park
/// a destination.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// The wait a `Retry-After` value asks for.
///
/// - `None` when there is no usable value: absent, empty or unparseable. The caller then uses its
///   own default wait.
/// - `Some(Duration::ZERO)` for a date in the past or a negative number of seconds: no wait.
/// - Otherwise the wait, at most [`MAX_RETRY_AFTER`].
pub fn parse_retry_after(value: Option<&str>, now: DateTime<Utc>) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(digits) = value.strip_prefix('-') {
        return digits
            .chars()
            .all(|c| c.is_ascii_digit())
            .then_some(Duration::ZERO)
            .filter(|_| !digits.is_empty());
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        // More digits than a u64 holds is far past the cap.
        let seconds = value.parse::<u64>().unwrap_or(u64::MAX);
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER));
    }
    let date = imf_fixdate(value)?;
    let wait = date.signed_duration_since(now).to_std().unwrap_or_default();
    Some(wait.min(MAX_RETRY_AFTER))
}

/// Parses `Sun, 06 Nov 1994 08:49:37 GMT` and nothing else: a day name, a two-digit day, a month
/// name, a four-digit year, a time of day and the literal `GMT`.
fn imf_fixdate(value: &str) -> Option<DateTime<Utc>> {
    let bytes = value.as_bytes();
    let shape_ok = value.len() == 29
        && bytes[3] == b','
        && bytes[4] == b' '
        && bytes[7] == b' '
        && bytes[11] == b' '
        && bytes[16] == b' '
        && bytes[19] == b':'
        && bytes[22] == b':'
        && bytes[25] == b' '
        && value.ends_with(" GMT")
        && value.is_ascii();
    if !shape_ok {
        return None;
    }
    DateTime::parse_from_rfc2822(value)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(1994, 11, 6, 8, 49, 37).unwrap()
    }

    fn secs(value: u64) -> Option<Duration> {
        Some(Duration::from_secs(value))
    }

    #[test]
    fn every_form_of_the_header_maps_to_a_wait() {
        let cases: [(Option<&str>, Option<Duration>); 21] = [
            // Absent or unusable: the caller's default applies.
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (Some("soon"), None),
            (Some("1.5"), None),
            (Some("12 seconds"), None),
            (Some("-"), None),
            (Some("--5"), None),
            // Delta-seconds.
            (Some("0"), secs(0)),
            (Some("120"), secs(120)),
            (Some(" 120 "), secs(120)),
            // Negative: no wait.
            (Some("-5"), secs(0)),
            // Huge: capped at the hour, even past u64.
            (Some("3600"), secs(3600)),
            (Some("3601"), secs(3600)),
            (Some("99999999999999999999999999"), secs(3600)),
            // HTTP date, relative to the injected now.
            (Some("Sun, 06 Nov 1994 08:50:37 GMT"), secs(60)),
            (Some("Sun, 06 Nov 1994 08:49:37 GMT"), secs(0)),
            // A past date: no wait.
            (Some("Sun, 06 Nov 1994 08:00:00 GMT"), secs(0)),
            // A date far ahead: capped.
            (Some("Sun, 06 Nov 2033 08:49:37 GMT"), secs(3600)),
            // Not IMF-fixdate: other date forms and zones are refused.
            (Some("Sunday, 06-Nov-94 08:50:37 GMT"), None),
            (Some("Sun, 06 Nov 1994 08:50:37 +0100"), None),
        ];

        for (value, expected) in cases {
            assert_eq!(parse_retry_after(value, now()), expected, "{value:?}");
        }
    }

    #[test]
    fn a_date_that_looks_right_but_is_not_a_date_is_refused() {
        for value in [
            "Sun, 31 Nov 1994 08:50:37 GMT",
            "Sun, 06 Nov 1994 25:50:37 GMT",
            "Sun, 06 Xyz 1994 08:50:37 GMT",
        ] {
            assert_eq!(parse_retry_after(Some(value), now()), None, "{value}");
        }
    }
}
