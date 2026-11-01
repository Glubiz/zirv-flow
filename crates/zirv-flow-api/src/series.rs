//! What the per-day series routes (flow entries, segment size) share: the `days` parameter and
//! the day format.

use crate::problem::ApiError;

/// The days of a series when `days` is not given.
pub(crate) const DEFAULT_DAYS: u32 = 30;

/// A day as `YYYY-MM-DD`.
pub(crate) fn format_day(day: time::Date) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    day.format(&format).unwrap_or_default()
}

/// The `days` query value: a whole number from 1 to `max`.
pub(crate) fn parse_days(value: &str, max: u32) -> Result<u32, ApiError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|days| (1..=max).contains(days))
        .ok_or_else(|| {
            ApiError::bad_request(format!("days must be a whole number from 1 to {max}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_is_written_as_year_month_day() {
        assert_eq!(
            format_day(time::macros::date!(2026 - 03 - 07)),
            "2026-03-07"
        );
    }

    #[test]
    fn days_are_a_whole_number_from_1_to_the_maximum() {
        assert_eq!(parse_days("1", 90).unwrap(), 1);
        assert_eq!(parse_days("90", 90).unwrap(), 90);
        for bad in ["0", "91", "x", "-1", ""] {
            assert!(parse_days(bad, 90).is_err(), "{bad}");
        }
    }
}
