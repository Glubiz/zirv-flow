//! Quiet hours (gate 6): which zone a recipient is in, and when a quiet period that holds `now`
//! ends. Pure: the gate reads the settings and the clock and hands them here.
//!
//! A period starts at the workspace's start time, local to the recipient, on one of its days and
//! ends at its end time: the same day when the end is after the start, else the next. A local time
//! that does not exist (the clocks jump forward over it) is read with the offset before the jump,
//! and one that happens twice takes the first occurrence, both by `resolve_local`, the rules of
//! `wait.until_time`.

use chrono::{DateTime, Datelike, Days, NaiveTime, Utc};
use chrono_tz::Tz;
use time::{OffsetDateTime, Time};
use zirv_flow_engine::resolve_local;
use zirv_flow_store::QuietHours;

/// The zone the recipient's quiet hours are read in, and whether the person's `timezone` trait was
/// not a zone so the workspace's was used. A missing trait is not a fallback; it is the workspace
/// zone as configured. A workspace zone that does not parse reads as UTC, as `flow.step` does.
pub(super) fn recipient_zone(workspace: &str, person: Option<&str>) -> (Tz, bool) {
    let workspace_zone = workspace.parse::<Tz>().unwrap_or(Tz::UTC);
    match person {
        None => (workspace_zone, false),
        Some(name) => name
            .parse::<Tz>()
            .map_or((workspace_zone, true), |zone| (zone, false)),
    }
}

/// When the quiet period that holds `now` ends, in `zone`; none when `now` is not in one.
///
/// A period belongs to the day it starts on, so the one that started yesterday and wraps past
/// midnight is the one that holds the small hours of today.
pub(super) fn quiet_until(
    now: OffsetDateTime,
    zone: Tz,
    quiet: &QuietHours,
) -> Option<OffsetDateTime> {
    let now = to_chrono(now)?;
    let today = now.with_timezone(&zone).date_naive();
    let (start, end) = (naive(quiet.start)?, naive(quiet.end)?);
    for back in [1, 0] {
        let day = today.checked_sub_days(Days::new(back))?;
        if !quiet.days.iter().any(|name| name == weekday_name(day)) {
            continue;
        }
        let end_day = if end > start { day } else { day.succ_opt()? };
        let began = resolve_local(zone, day.and_time(start));
        let ends = resolve_local(zone, end_day.and_time(end));
        if began <= now && now < ends {
            return from_chrono(ends);
        }
    }
    None
}

fn weekday_name(day: chrono::NaiveDate) -> &'static str {
    match day.weekday() {
        chrono::Weekday::Mon => "mon",
        chrono::Weekday::Tue => "tue",
        chrono::Weekday::Wed => "wed",
        chrono::Weekday::Thu => "thu",
        chrono::Weekday::Fri => "fri",
        chrono::Weekday::Sat => "sat",
        chrono::Weekday::Sun => "sun",
    }
}

fn naive(time: Time) -> Option<NaiveTime> {
    NaiveTime::from_hms_opt(u32::from(time.hour()), u32::from(time.minute()), 0)
}

fn to_chrono(at: OffsetDateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(at.unix_timestamp(), at.nanosecond())
}

fn from_chrono(at: DateTime<Utc>) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(at.timestamp()).ok()
}

#[cfg(test)]
mod tests {
    use time::macros::{datetime, time};

    use super::*;

    const COPENHAGEN: Tz = Tz::Europe__Copenhagen;

    fn quiet(start: Time, end: Time, days: &[&str]) -> QuietHours {
        QuietHours {
            start,
            end,
            days: days.iter().map(|day| (*day).to_owned()).collect(),
        }
    }

    const EVERY_DAY: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

    #[test]
    fn a_person_zone_wins_and_an_invalid_one_falls_back_and_says_so() {
        assert_eq!(
            recipient_zone("Europe/Copenhagen", Some("Asia/Tokyo")),
            (Tz::Asia__Tokyo, false)
        );
        assert_eq!(
            recipient_zone("Europe/Copenhagen", None),
            (COPENHAGEN, false)
        );
        assert_eq!(
            recipient_zone("Europe/Copenhagen", Some("Not/AZone")),
            (COPENHAGEN, true)
        );
        assert_eq!(recipient_zone("garbage", None), (Tz::UTC, false));
    }

    #[test]
    fn inside_an_overnight_period_the_end_is_the_next_morning_in_the_local_zone() {
        let overnight = quiet(time!(21:00), time!(08:00), &EVERY_DAY);

        // 23:30 in Copenhagen in January (UTC+1): the period ends 08:00 local, 07:00 UTC.
        assert_eq!(
            quiet_until(datetime!(2026-01-14 22:30 UTC), COPENHAGEN, &overnight),
            Some(datetime!(2026-01-15 07:00 UTC))
        );
        // 03:00 local: the period that started the evening before.
        assert_eq!(
            quiet_until(datetime!(2026-01-15 02:00 UTC), COPENHAGEN, &overnight),
            Some(datetime!(2026-01-15 07:00 UTC))
        );
        // The same instant is daytime in Tokyo.
        assert_eq!(
            quiet_until(datetime!(2026-01-15 02:00 UTC), Tz::Asia__Tokyo, &overnight),
            None
        );
    }

    #[test]
    fn the_period_starts_inclusive_and_ends_exclusive() {
        let overnight = quiet(time!(21:00), time!(08:00), &EVERY_DAY);

        assert!(quiet_until(datetime!(2026-01-14 20:00 UTC), COPENHAGEN, &overnight).is_some());
        assert!(quiet_until(datetime!(2026-01-14 19:59 UTC), COPENHAGEN, &overnight).is_none());
        assert!(quiet_until(datetime!(2026-01-15 07:00 UTC), COPENHAGEN, &overnight).is_none());
        assert!(quiet_until(datetime!(2026-01-15 06:59 UTC), COPENHAGEN, &overnight).is_some());
    }

    #[test]
    fn a_period_that_wraps_midnight_belongs_to_the_day_it_starts_on() {
        // Friday nights only: 2026-01-16 is a Friday.
        let fridays = quiet(time!(21:00), time!(08:00), &["fri"]);

        assert!(
            quiet_until(datetime!(2026-01-16 22:30 UTC), COPENHAGEN, &fridays).is_some(),
            "Friday 23:30"
        );
        assert_eq!(
            quiet_until(datetime!(2026-01-17 02:00 UTC), COPENHAGEN, &fridays),
            Some(datetime!(2026-01-17 07:00 UTC)),
            "Saturday 03:00 is still Friday's period"
        );
        assert!(
            quiet_until(datetime!(2026-01-16 02:00 UTC), COPENHAGEN, &fridays).is_none(),
            "Friday 03:00 is Thursday's period, which is not a quiet day"
        );
        assert!(
            quiet_until(datetime!(2026-01-17 22:30 UTC), COPENHAGEN, &fridays).is_none(),
            "Saturday 23:30"
        );
    }

    #[test]
    fn a_period_inside_one_day_ends_the_same_day() {
        let lunch = quiet(time!(12:00), time!(13:30), &EVERY_DAY);

        assert_eq!(
            quiet_until(datetime!(2026-01-14 11:15 UTC), COPENHAGEN, &lunch),
            Some(datetime!(2026-01-14 12:30 UTC))
        );
        assert!(quiet_until(datetime!(2026-01-14 12:30 UTC), COPENHAGEN, &lunch).is_none());
    }

    #[test]
    fn an_end_in_the_gap_of_a_spring_forward_uses_the_offset_before_the_jump() {
        // Copenhagen jumps from 02:00 to 03:00 on 2026-03-29. The night's period ends at 02:30
        // local, which does not exist: it reads with the offset before the jump (UTC+1), so
        // 01:30 UTC, which the clocks show as 03:30.
        let night = quiet(time!(22:00), time!(02:30), &EVERY_DAY);

        assert_eq!(
            quiet_until(datetime!(2026-03-28 23:00 UTC), COPENHAGEN, &night),
            Some(datetime!(2026-03-29 01:30 UTC))
        );
        assert!(quiet_until(datetime!(2026-03-29 01:30 UTC), COPENHAGEN, &night).is_none());
    }

    #[test]
    fn an_end_in_the_overlap_of_a_fall_back_takes_the_first_occurrence() {
        // Copenhagen falls back from 03:00 to 02:00 on 2026-10-25, so 02:30 happens twice. The
        // period ends at the first, 00:30 UTC (UTC+2).
        let night = quiet(time!(22:00), time!(02:30), &EVERY_DAY);

        assert_eq!(
            quiet_until(datetime!(2026-10-24 22:00 UTC), COPENHAGEN, &night),
            Some(datetime!(2026-10-25 00:30 UTC))
        );
        // The second 02:30 (01:45 UTC shows 02:45) is after the end: not quiet.
        assert!(quiet_until(datetime!(2026-10-25 01:45 UTC), COPENHAGEN, &night).is_none());
    }

    #[test]
    fn a_start_in_the_gap_still_begins_the_period_at_the_instant_the_clocks_show_it() {
        // A period from 02:30 to 05:00 on the spring-forward night starts at 01:30 UTC.
        let early = quiet(time!(2:30), time!(5:00), &EVERY_DAY);

        assert!(quiet_until(datetime!(2026-03-29 01:29 UTC), COPENHAGEN, &early).is_none());
        assert_eq!(
            quiet_until(datetime!(2026-03-29 01:30 UTC), COPENHAGEN, &early),
            Some(datetime!(2026-03-29 03:00 UTC))
        );
    }
}
