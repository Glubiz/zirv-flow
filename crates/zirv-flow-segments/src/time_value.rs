//! Durations, timestamps and dates as they appear in a definition.

use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, UtcOffset};

const MAX_HOURS: u64 = 1_095 * 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationError {
    Invalid,
    TooLong,
}

/// Parses `P<n>D`, `PT<n>H` or `P<n>DT<n>H` into hours. Between 1 hour and 1,095 days.
pub fn parse_duration(text: &str) -> Result<u64, DurationError> {
    let rest = text.strip_prefix('P').ok_or(DurationError::Invalid)?;
    let (days_part, time_part) = match rest.split_once('T') {
        Some((days, hours)) => (days, Some(hours)),
        None => (rest, None),
    };
    let days = match days_part {
        "" => None,
        part => Some(number(
            part.strip_suffix('D').ok_or(DurationError::Invalid)?,
        )?),
    };
    let hours = match time_part {
        None => None,
        Some(part) => Some(number(
            part.strip_suffix('H').ok_or(DurationError::Invalid)?,
        )?),
    };
    if days.is_none() && hours.is_none() {
        return Err(DurationError::Invalid);
    }
    let total = days
        .unwrap_or(0)
        .checked_mul(24)
        .and_then(|hours_from_days| hours_from_days.checked_add(hours.unwrap_or(0)))
        .ok_or(DurationError::TooLong)?;
    match total {
        0 => Err(DurationError::Invalid),
        total if total > MAX_HOURS => Err(DurationError::TooLong),
        total => Ok(total),
    }
}

fn number(digits: &str) -> Result<u64, DurationError> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DurationError::Invalid);
    }
    digits.parse().map_err(|_| DurationError::TooLong)
}

/// The canonical text of a duration: whole days as `P<n>D`, anything else as `PT<n>H`.
pub fn format_duration(hours: u64) -> String {
    if hours.is_multiple_of(24) {
        format!("P{}D", hours / 24)
    } else {
        format!("PT{hours}H")
    }
}

pub(crate) fn parse_timestamp(text: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok()
}

pub(crate) fn parse_date(text: &str) -> Option<Date> {
    Date::parse(text, format_description!("[year]-[month]-[day]")).ok()
}

/// RFC 3339 in UTC.
pub(crate) fn format_timestamp(timestamp: OffsetDateTime) -> Option<String> {
    timestamp.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_in_days_and_hours_parse_to_hours() {
        assert_eq!(parse_duration("P90D"), Ok(2160));
        assert_eq!(parse_duration("PT1H"), Ok(1));
        assert_eq!(parse_duration("P1DT12H"), Ok(36));
        assert_eq!(parse_duration("P1095D"), Ok(1095 * 24));
    }

    #[test]
    fn other_shapes_are_invalid() {
        for text in [
            "", "P", "PT", "P1Y", "P1W", "PT30M", "P1D2H", "p1d", "P-1D", "P1.5D", "PD", "P1DT",
            "PT1D", "P0D", "PT0H", " P1D", "90D",
        ] {
            assert_eq!(parse_duration(text), Err(DurationError::Invalid), "{text}");
        }
    }

    #[test]
    fn a_duration_past_three_years_is_too_long() {
        assert_eq!(parse_duration("P1096D"), Err(DurationError::TooLong));
        assert_eq!(
            parse_duration("P99999999999999999999D"),
            Err(DurationError::TooLong)
        );
        assert_eq!(
            parse_duration("P999999999999999999D"),
            Err(DurationError::TooLong)
        );
    }

    #[test]
    fn durations_are_written_canonically() {
        assert_eq!(format_duration(48), "P2D");
        assert_eq!(format_duration(36), "PT36H");
        assert_eq!(format_duration(1), "PT1H");
    }

    #[test]
    fn timestamps_are_written_in_utc() {
        let parsed = parse_timestamp("2024-03-01T02:00:00+02:00").unwrap();
        assert_eq!(
            format_timestamp(parsed).as_deref(),
            Some("2024-03-01T00:00:00Z")
        );
        assert!(parse_timestamp("2024-03-01").is_none());
        assert!(parse_timestamp("yesterday").is_none());
    }

    #[test]
    fn dates_need_the_full_padded_form() {
        assert!(parse_date("2024-03-01").is_some());
        assert!(parse_date("2024-3-1").is_none());
        assert!(parse_date("2024-02-30").is_none());
        assert!(parse_date("2024-03-01T00:00:00Z").is_none());
    }
}
