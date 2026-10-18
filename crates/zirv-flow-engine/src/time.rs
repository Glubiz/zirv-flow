//! `wait.until_time`: a local time in a zone to an instant, with fixed rules for the two
//! daylight-saving cases.
//!
//! - A local time that does not exist (the clocks jump forward over it) is read with the offset
//!   in force before the jump, so 02:30 in a 02:00 to 03:00 gap is the instant 03:30 shows.
//! - A local time that happens twice (the clocks fall back over it) is the first occurrence.

use chrono::{
    DateTime, Datelike, Days, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset,
    Utc,
};
use chrono_tz::Tz;
use zirv_flow_core::flow::{UntilTime, Weekday};

/// How far ahead a time of day looks for an allowed weekday: a week plus slack for the zone.
const LOOKAHEAD_DAYS: u64 = 14;

/// How far back to probe for the offset in force before a gap, in hours.
const GAP_PROBES_HOURS: [i64; 5] = [1, 3, 12, 24, 48];

/// A local time in `zone` as an instant, by the rules in the module docs.
pub fn resolve_local(zone: Tz, local: NaiveDateTime) -> DateTime<Utc> {
    match local.and_local_timezone(zone) {
        LocalResult::Single(at) => at.with_timezone(&Utc),
        LocalResult::Ambiguous(first, _) => first.with_timezone(&Utc),
        LocalResult::None => {
            let before = GAP_PROBES_HOURS.iter().find_map(|hours| {
                (local - Duration::hours(*hours))
                    .and_local_timezone(zone)
                    .single()
            });
            match before {
                Some(probe) => {
                    let offset = i64::from(probe.offset().fix().local_minus_utc());
                    (local - Duration::seconds(offset)).and_utc()
                }
                None => local.and_utc(),
            }
        }
    }
}

/// Why a `wait.until_time` could not resolve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimeError {
    /// The node's time or date does not parse (the validator refuses these).
    Unparseable,
    /// No allowed day within the look-ahead.
    NoDay,
}

/// The instant a `wait.until_time` resumes at, from `now`, in `zone`. A time of day is the next
/// one strictly after `now`; a date and time may be in the past, which makes the wait due at once.
pub(crate) fn until(
    when: &UntilTime,
    now: DateTime<Utc>,
    zone: Tz,
) -> Result<DateTime<Utc>, TimeError> {
    match when {
        UntilTime::DateTime { at } => NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
            .map(|local| resolve_local(zone, local))
            .map_err(|_| TimeError::Unparseable),
        UntilTime::TimeOfDay { time, weekdays } => {
            let time =
                NaiveTime::parse_from_str(time, "%H:%M").map_err(|_| TimeError::Unparseable)?;
            let today: NaiveDate = now.with_timezone(&zone).date_naive();
            (0..=LOOKAHEAD_DAYS)
                .filter_map(|ahead| today.checked_add_days(Days::new(ahead)))
                .filter(|day| weekdays.is_empty() || weekdays.contains(&weekday(*day)))
                .map(|day| resolve_local(zone, day.and_time(time)))
                .find(|at| *at > now)
                .ok_or(TimeError::NoDay)
        }
    }
}

fn weekday(day: NaiveDate) -> Weekday {
    match day.weekday() {
        chrono::Weekday::Mon => Weekday::Mon,
        chrono::Weekday::Tue => Weekday::Tue,
        chrono::Weekday::Wed => Weekday::Wed,
        chrono::Weekday::Thu => Weekday::Thu,
        chrono::Weekday::Fri => Weekday::Fri,
        chrono::Weekday::Sat => Weekday::Sat,
        chrono::Weekday::Sun => Weekday::Sun,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const COPENHAGEN: Tz = Tz::Europe__Copenhagen;

    fn local(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn an_ordinary_local_time_uses_the_zone_offset() {
        assert_eq!(
            resolve_local(COPENHAGEN, local("2030-01-15T09:00:00")),
            utc(2030, 1, 15, 8, 0)
        );
        assert_eq!(
            resolve_local(COPENHAGEN, local("2030-07-15T09:00:00")),
            utc(2030, 7, 15, 7, 0)
        );
    }

    #[test]
    fn a_time_in_the_spring_gap_is_read_with_the_offset_before_the_jump() {
        // 2030-03-31: 02:00 CET jumps to 03:00 CEST; 02:30 does not exist.
        let at = resolve_local(COPENHAGEN, local("2030-03-31T02:30:00"));
        assert_eq!(at, utc(2030, 3, 31, 1, 30));
        // Which is 03:30 on the wall clock after the jump.
        assert_eq!(
            at.with_timezone(&COPENHAGEN).naive_local(),
            local("2030-03-31T03:30:00")
        );
        // The edge of the gap.
        assert_eq!(
            resolve_local(COPENHAGEN, local("2030-03-31T02:00:00")),
            utc(2030, 3, 31, 1, 0)
        );
    }

    #[test]
    fn a_time_in_the_autumn_overlap_is_the_first_occurrence() {
        // 2030-10-27: 03:00 CEST falls back to 02:00 CET; 02:30 happens twice.
        assert_eq!(
            resolve_local(COPENHAGEN, local("2030-10-27T02:30:00")),
            utc(2030, 10, 27, 0, 30)
        );
    }

    #[test]
    fn a_half_hour_gap_and_a_zone_without_dst_resolve() {
        // Lord Howe jumps 02:00 to 02:30 on 2030-10-06.
        let lord_howe = Tz::Australia__Lord_Howe;
        let at = resolve_local(lord_howe, local("2030-10-06T02:10:00"));
        assert_eq!(
            at.with_timezone(&lord_howe).naive_local(),
            local("2030-10-06T02:40:00")
        );
        assert_eq!(
            resolve_local(Tz::UTC, local("2030-03-31T02:30:00")),
            utc(2030, 3, 31, 2, 30)
        );
    }

    #[test]
    fn a_time_of_day_is_the_next_one_strictly_after_now() {
        let when = UntilTime::TimeOfDay {
            time: "09:00".into(),
            weekdays: vec![],
        };
        // 08:00 UTC is 09:00 in Copenhagen in January: not strictly after, so tomorrow.
        assert_eq!(
            until(&when, utc(2030, 1, 15, 8, 0), COPENHAGEN),
            Ok(utc(2030, 1, 16, 8, 0))
        );
        assert_eq!(
            until(&when, utc(2030, 1, 15, 7, 59), COPENHAGEN),
            Ok(utc(2030, 1, 15, 8, 0))
        );
    }

    #[test]
    fn weekdays_restrict_the_day_in_the_local_zone() {
        // 2030-01-15 is a Tuesday.
        let when = UntilTime::TimeOfDay {
            time: "09:00".into(),
            weekdays: vec![Weekday::Fri],
        };
        assert_eq!(
            until(&when, utc(2030, 1, 15, 0, 0), COPENHAGEN),
            Ok(utc(2030, 1, 18, 8, 0))
        );
        // 23:30 UTC on Thursday is already Friday 00:30 in Copenhagen.
        assert_eq!(
            until(&when, utc(2030, 1, 17, 23, 30), COPENHAGEN),
            Ok(utc(2030, 1, 18, 8, 0))
        );
    }

    #[test]
    fn a_time_of_day_across_a_dst_change_keeps_the_local_time() {
        let when = UntilTime::TimeOfDay {
            time: "09:00".into(),
            weekdays: vec![],
        };
        // Saturday 2030-03-30 10:00 local; the next 09:00 is Sunday, after the jump (CEST).
        assert_eq!(
            until(&when, utc(2030, 3, 30, 9, 0), COPENHAGEN),
            Ok(utc(2030, 3, 31, 7, 0))
        );
        // A time of day inside the gap lands on the shifted instant.
        let gap = UntilTime::TimeOfDay {
            time: "02:30".into(),
            weekdays: vec![],
        };
        assert_eq!(
            until(&gap, utc(2030, 3, 30, 23, 0), COPENHAGEN),
            Ok(utc(2030, 3, 31, 1, 30))
        );
    }

    #[test]
    fn a_date_time_may_be_in_the_past_and_bad_text_is_refused() {
        let past = UntilTime::DateTime {
            at: "2020-01-01T00:00:00".into(),
        };
        assert_eq!(
            until(&past, utc(2030, 1, 1, 0, 0), Tz::UTC),
            Ok(utc(2020, 1, 1, 0, 0))
        );
        let bad = UntilTime::DateTime { at: "soon".into() };
        assert_eq!(
            until(&bad, utc(2030, 1, 1, 0, 0), Tz::UTC),
            Err(TimeError::Unparseable)
        );
        let bad_time = UntilTime::TimeOfDay {
            time: "25:00".into(),
            weekdays: vec![],
        };
        assert_eq!(
            until(&bad_time, utc(2030, 1, 1, 0, 0), Tz::UTC),
            Err(TimeError::Unparseable)
        );
    }
}
