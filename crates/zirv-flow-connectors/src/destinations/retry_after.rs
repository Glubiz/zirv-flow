//! The `Retry-After` header of a 429 or 503 answer (RFC 9110 section 10.2.3), parsed once for
//! every destination type. The value is either delta-seconds or an HTTP date. A date is read in
//! the preferred IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`) and in the two obsolete forms
//! RFC 9110 still requires a recipient to accept: RFC 850 (`Sunday, 06-Nov-94 08:49:37 GMT`) and
//! asctime (`Sun Nov  6 08:49:37 1994`).
//!
//! The parser is pure: the caller passes the current time, so a test needs no clock. It saturates
//! and does not cap: the store caps a penalty window (`MAX_RETRY_AFTER`), the single place that
//! decides how long a destination may be held back.

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

/// The wait a `Retry-After` value asks for.
///
/// - `None` when there is no usable value: absent, empty, signed (`-1`, `+5`) or unparseable. The
///   caller then uses its own default wait.
/// - `Some(Duration::ZERO)` for a date in the past: no wait.
/// - Otherwise the wait, saturating at the largest representable number of seconds.
pub fn parse_retry_after(value: Option<&str>, now: DateTime<Utc>) -> Option<Duration> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if value.chars().all(|c| c.is_ascii_digit()) {
        // More digits than a u64 holds saturates.
        return Some(Duration::from_secs(
            value.parse::<u64>().unwrap_or(u64::MAX),
        ));
    }
    let date = http_date(value)?;
    Some(date.signed_duration_since(now).to_std().unwrap_or_default())
}

/// Parses the three HTTP date forms, all in GMT, and nothing else.
fn http_date(value: &str) -> Option<DateTime<Utc>> {
    if !value.is_ascii() {
        return None;
    }
    // IMF-fixdate: a day name, a two-digit day, a month name, a four-digit year, a time, `GMT`.
    // `%a` and `%b` take the abbreviated names; the shape check keeps chrono's lenient padding out.
    let bytes = value.as_bytes();
    if value.len() == 29 && bytes[3] == b',' && value.ends_with(" GMT") {
        return NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
            .ok()
            .map(|date| date.and_utc());
    }
    // RFC 850: the full day name, `dd-Mon-yy`; a two-digit year is read by chrono's `%y` rule.
    if value.ends_with(" GMT") && value.contains(", ") && value.contains('-') {
        return NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT")
            .ok()
            .map(|date| date.and_utc());
    }
    // asctime: no zone, a space-padded day.
    if !value.contains(',') && !value.ends_with("GMT") {
        return NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
            .ok()
            .map(|date| date.and_utc());
    }
    None
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
        let cases: [(Option<&str>, Option<Duration>); 28] = [
            // Absent or unusable: the caller's default applies.
            (None, None),
            (Some(""), None),
            (Some("   "), None),
            (Some("soon"), None),
            (Some("1.5"), None),
            (Some("12 seconds"), None),
            // Signed values are refused.
            (Some("-1"), None),
            (Some("-0"), None),
            (Some("+5"), None),
            (Some("-"), None),
            (Some("--5"), None),
            // Delta-seconds.
            (Some("0"), secs(0)),
            (Some("120"), secs(120)),
            (Some(" 120 "), secs(120)),
            // Large values saturate; the store caps the window.
            (Some("3601"), secs(3601)),
            (Some("99999999999999999999999999"), secs(u64::MAX)),
            // IMF-fixdate, relative to the injected now.
            (Some("Sun, 06 Nov 1994 08:50:37 GMT"), secs(60)),
            (Some("Sun, 06 Nov 1994 08:49:37 GMT"), secs(0)),
            (Some("Sun, 06 Nov 1994 08:00:00 GMT"), secs(0)),
            // RFC 850 (obsolete).
            (Some("Sunday, 06-Nov-94 08:50:37 GMT"), secs(60)),
            (Some("Sunday, 06-Nov-94 08:00:00 GMT"), secs(0)),
            // asctime (obsolete), with its space-padded day.
            (Some("Sun Nov  6 08:50:37 1994"), secs(60)),
            (Some("Sun Nov  6 08:00:00 1994"), secs(0)),
            (Some("Wed Nov 16 08:50:37 1994"), secs(864_060)),
            // A zone other than GMT is refused.
            (Some("Sun, 06 Nov 1994 08:50:37 +0100"), None),
            (Some("Sun, 06 Nov 1994 08:50:37 EST"), None),
            (Some("Sun Nov  6 08:50:37 1994 GMT"), None),
            (Some("Sunday, 06-Nov-94 08:50:37 +0100"), None),
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
            "Sun Nov 31 08:50:37 1994",
            "Sunday, 31-Nov-94 08:50:37 GMT",
        ] {
            assert_eq!(parse_retry_after(Some(value), now()), None, "{value}");
        }
    }
}
