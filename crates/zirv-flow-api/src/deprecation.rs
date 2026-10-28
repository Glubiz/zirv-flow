//! The headers of a deprecated operation ([`docs/api.md`](../../../docs/api.md), "Deprecation").
//!
//! A deprecated operation keeps working for at least one minor release and answers with a
//! `Deprecation` header (RFC 9745: `@` and the unix time it was deprecated) and a `Sunset`
//! header (RFC 8594: the HTTP date of the earliest removal). No operation is deprecated yet, so
//! nothing calls this; the first deprecation does, from the handler, next to the
//! `deprecated = true` in its `utoipa::path`.

use std::fmt::Write;

use axum::http::{HeaderMap, HeaderName, HeaderValue, header::InvalidHeaderValue};
use time::{Month, OffsetDateTime, Weekday};

/// The `Deprecation` response header.
pub const DEPRECATION: HeaderName = HeaderName::from_static("deprecation");
/// The `Sunset` response header.
pub const SUNSET: HeaderName = HeaderName::from_static("sunset");

/// Marks a response as coming from a deprecated operation: `deprecated_at` is when it was
/// deprecated, `sunset` the earliest date it may be removed.
pub fn mark_deprecated(
    headers: &mut HeaderMap,
    deprecated_at: OffsetDateTime,
    sunset: OffsetDateTime,
) -> Result<(), InvalidHeaderValue> {
    let deprecation = HeaderValue::from_str(&format!("@{}", deprecated_at.unix_timestamp()))?;
    let sunset = HeaderValue::from_str(&http_date(sunset))?;
    headers.insert(DEPRECATION, deprecation);
    headers.insert(SUNSET, sunset);
    Ok(())
}

/// An HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`), always in GMT.
fn http_date(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let day = match at.weekday() {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    };
    let month = match at.month() {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    };
    let mut text = String::with_capacity(29);
    // Writing to a String cannot fail.
    let _ = write!(
        text,
        "{day}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT",
        at.day(),
        at.year(),
        at.hour(),
        at.minute(),
        at.second()
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_deprecated_response_carries_the_deprecation_and_sunset_headers() {
        let mut headers = HeaderMap::new();
        mark_deprecated(
            &mut headers,
            datetime!(2027-03-01 12:00:00 UTC),
            datetime!(1994-11-06 10:49:37 +02:00),
        )
        .unwrap();
        assert_eq!(headers["deprecation"], "@1803902400");
        assert_eq!(headers["sunset"], "Sun, 06 Nov 1994 08:49:37 GMT");
    }
}
