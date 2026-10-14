//! Reads a time string a person or an event stores the way the compiled SQL does (#118,
//! `notes.md` of #119): valid exactly when it has the write path's RFC 3339 shape and
//! PostgreSQL also accepts it. Everything else is missing.
//!
//! Instants are microseconds since the Unix epoch, which is PostgreSQL's own resolution and
//! reaches every year from 0001 to 9999 whatever the offset, where `OffsetDateTime` would not.
//!
//! Where the two sides disagree, this module follows PostgreSQL: it refuses year 0000 and a UTC
//! offset of 16 hours or more (chrono and RFC 3339 accept both), and it accepts hour 24 at
//! exactly midnight and second 60, which roll over into the next day or minute. Fractional
//! seconds are rounded to microseconds by the seventh digit (half up); PostgreSQL rounds the
//! parsed double, so a tie such as `.0000005` can differ, which is why the generators keep whole
//! seconds.

use time::OffsetDateTime;

const MICROS: i64 = 1_000_000;
const DAY: i64 = 86_400 * MICROS;

/// The offset limit: PostgreSQL refuses `+16:00` and beyond.
const MAX_OFFSET_HOURS: i64 = 15;

/// A timestamp trait or property: the RFC 3339 shape with `T`, `t` or a space between the date
/// and the time, and `Z`, `z` or a numeric offset.
pub(crate) fn timestamp(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let date = date_part(&bytes[..10])?;
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let hour = two(&bytes[11..13])?;
    let minute = two(&bytes[14..16])?;
    let second = two(&bytes[17..19])?;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let mut at = 19;
    let mut micros = 0;
    if bytes[at] == b'.' {
        at += 1;
        let start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at == start {
            return None;
        }
        micros = fraction(&bytes[start..at]);
    }
    let offset = offset(&bytes[at..])?;
    if hour > 24 || minute > 59 || second > 60 {
        return None;
    }
    if hour == 24 && (minute > 0 || second > 0 || micros > 0) {
        return None;
    }
    let of_day = ((hour * 60 + minute) * 60 + second) * MICROS + micros;
    Some(date * DAY + of_day - offset * MICROS)
}

/// A date trait or property: `YYYY-MM-DD` at UTC midnight.
pub(crate) fn date(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    Some(date_part(bytes)? * DAY)
}

/// An instant as microseconds since the epoch.
pub(crate) fn micros(at: OffsetDateTime) -> i64 {
    // Whole microseconds, like the `timestamptz` the compiler binds.
    i64::try_from(at.unix_timestamp_nanos().div_euclid(1_000)).unwrap_or(i64::MAX)
}

fn two(bytes: &[u8]) -> Option<i64> {
    match bytes {
        [tens, ones] if tens.is_ascii_digit() && ones.is_ascii_digit() => {
            Some(i64::from(tens - b'0') * 10 + i64::from(ones - b'0'))
        }
        _ => None,
    }
}

/// Days since the epoch of a valid `YYYY-MM-DD`; year 0000 is refused.
fn date_part(bytes: &[u8]) -> Option<i64> {
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = two(&bytes[0..2])? * 100 + two(&bytes[2..4])?;
    let month = two(&bytes[5..7])?;
    let day = two(&bytes[8..10])?;
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The proleptic Gregorian day number relative to 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year.rem_euclid(400);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let of_year = (153 * shifted + 2) / 5 + day - 1;
    let of_day = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    era * 146_097 + of_day - 719_468
}

/// The digits after the point as microseconds, rounded half up at the seventh digit.
fn fraction(digits: &[u8]) -> i64 {
    let mut micros = 0;
    for index in 0..6 {
        let digit = digits.get(index).map_or(0, |byte| i64::from(byte - b'0'));
        micros = micros * 10 + digit;
    }
    if digits.get(6).is_some_and(|byte| *byte >= b'5') {
        micros += 1;
    }
    micros
}

/// The seconds east of UTC of `Z`, `z` or `+HH:MM`, which must end the text.
fn offset(bytes: &[u8]) -> Option<i64> {
    match bytes {
        [b'Z' | b'z'] => Some(0),
        [sign @ (b'+' | b'-'), rest @ ..] if rest.len() == 5 && rest[2] == b':' => {
            let hours = two(&rest[0..2])?;
            let minutes = two(&rest[3..5])?;
            if hours > MAX_OFFSET_HOURS || minutes > 59 {
                return None;
            }
            let seconds = hours * 3_600 + minutes * 60;
            Some(if *sign == b'-' { -seconds } else { seconds })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn at(value: OffsetDateTime) -> Option<i64> {
        Some(micros(value))
    }

    #[test]
    fn a_timestamp_reads_to_its_instant_in_every_spelling() {
        let expected = at(datetime!(2025-06-15 12:00:00 UTC));
        for text in [
            "2025-06-15T12:00:00Z",
            "2025-06-15t12:00:00z",
            "2025-06-15 12:00:00Z",
            "2025-06-15T14:00:00+02:00",
            "2025-06-15T09:30:00-02:30",
            "2025-06-15T12:00:00.000Z",
        ] {
            assert_eq!(timestamp(text), expected, "{text}");
        }
    }

    #[test]
    fn dates_are_at_utc_midnight() {
        assert_eq!(date("2024-02-29"), at(datetime!(2024-02-29 0:00 UTC)));
        assert_eq!(date("1970-01-01"), Some(0));
        assert_eq!(date("0001-01-01"), at(datetime!(0001-01-01 0:00 UTC)));
        assert_eq!(date("9999-12-31"), at(datetime!(9999-12-31 0:00 UTC)));
    }

    #[test]
    fn calendar_days_must_exist() {
        for text in [
            "2023-02-29",
            "1900-02-29",
            "2024-04-31",
            "2024-13-01",
            "2024-00-10",
        ] {
            assert_eq!(date(text), None, "{text}");
        }
        assert!(date("2000-02-29").is_some());
    }

    #[test]
    fn year_zero_and_long_offsets_are_refused_though_rfc_3339_allows_them() {
        assert_eq!(timestamp("0000-06-15T12:00:00Z"), None);
        assert_eq!(date("0000-06-15"), None);
        assert_eq!(timestamp("2025-06-15T12:00:00+16:00"), None);
        assert_eq!(timestamp("2025-06-15T12:00:00-23:59"), None);
        assert!(timestamp("2025-06-15T12:00:00+15:59").is_some());
        assert_eq!(timestamp("2025-06-15T12:00:00+05:60"), None);
    }

    #[test]
    fn the_shape_refuses_what_postgresql_alone_would_take() {
        for text in [
            "infinity",
            "-infinity",
            "now",
            "epoch",
            "10000-01-01",
            "2025-06-15T12:00:00Z\n",
            "2025-06-15",
            "2025-06-15T12:00:00",
            "2025-06-15T12:00Z",
            "2025-06-15T12:00:00.Z",
            "2025-06-15T12:00:00 +02:00",
            " 2025-06-15T12:00:00Z",
            "2025-06-15T12:00:00+0200",
            "2025-6-15T12:00:00Z",
            "２０２５-06-15T12:00:00Z",
        ] {
            assert_eq!(timestamp(text), None, "{text:?}");
        }
        assert_eq!(date("2025-06-15\n"), None);
        assert_eq!(date("2025-06-15T00:00:00Z"), None);
        assert_eq!(date("now"), None);
    }

    #[test]
    fn hour_24_is_midnight_and_second_60_rolls_over() {
        assert_eq!(
            timestamp("2025-06-15T24:00:00Z"),
            at(datetime!(2025-06-16 0:00 UTC))
        );
        assert_eq!(timestamp("2025-06-15T24:00:01Z"), None);
        assert_eq!(timestamp("2025-06-15T24:01:00Z"), None);
        assert_eq!(timestamp("2025-06-15T25:00:00Z"), None);
        assert_eq!(
            timestamp("2025-06-15T23:59:60Z"),
            at(datetime!(2025-06-16 0:00 UTC))
        );
        assert_eq!(timestamp("2025-06-15T12:00:61Z"), None);
        assert_eq!(timestamp("2025-06-15T12:60:00Z"), None);
    }

    #[test]
    fn fractions_round_to_microseconds() {
        let base = micros(datetime!(2025-06-15 12:00:00 UTC));
        assert_eq!(timestamp("2025-06-15T12:00:00.5Z"), Some(base + 500_000));
        assert_eq!(
            timestamp("2025-06-15T12:00:00.123456Z"),
            Some(base + 123_456)
        );
        assert_eq!(
            timestamp("2025-06-15T12:00:00.1234564Z"),
            Some(base + 123_456)
        );
        assert_eq!(
            timestamp("2025-06-15T12:00:00.1234565Z"),
            Some(base + 123_457)
        );
        assert_eq!(
            timestamp("2025-06-15T12:00:00.9999999Z"),
            Some(base + MICROS)
        );
    }

    #[test]
    fn the_far_years_keep_their_order() {
        let first = timestamp("0001-01-01T00:00:00+15:00").unwrap();
        let last = timestamp("9999-12-31T23:59:59-15:00").unwrap();
        assert!(first < 0 && last > 0);
        assert!(first < timestamp("0001-01-01T00:00:00Z").unwrap());
    }

    #[test]
    fn instants_are_whole_microseconds() {
        assert_eq!(
            micros(datetime!(1970-01-01 0:00:01.000_001_9 UTC)),
            1_000_001
        );
        assert_eq!(micros(datetime!(1969-12-31 23:59:59 UTC)), -MICROS);
    }
}
