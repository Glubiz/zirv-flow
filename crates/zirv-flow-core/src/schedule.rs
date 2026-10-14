//! Pure scheduling math: the retry backoff and the next cron slot.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::Cron;
use croner::parser::{CronParser, Seconds};
use time::OffsetDateTime;

/// The exponent stops growing here; the cap bounds the delay long before this matters.
const MAX_DOUBLINGS: u32 = 30;

/// How long to wait before retrying a job that has failed `attempts` times:
/// `min(base * 2^attempts, cap) * jitter`, with the jitter in `[0.5, 1.0]`.
///
/// `random_unit` is a random number in `[0, 1]` supplied by the caller, so the math stays
/// deterministic under test. A value outside that range is clamped.
pub fn backoff(attempts: u32, base: Duration, cap: Duration, random_unit: f64) -> Duration {
    let unit = if random_unit.is_nan() {
        1.0
    } else {
        random_unit.clamp(0.0, 1.0)
    };
    let jitter = 0.5 + 0.5 * unit;
    let doubled = base.as_secs_f64() * 2_f64.powi(attempts.min(MAX_DOUBLINGS) as i32);
    let seconds = doubled.min(cap.as_secs_f64()) * jitter;
    Duration::from_secs_f64(seconds)
}

/// Why a schedule could not be evaluated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    InvalidCron,
    InvalidTimezone,
    NoNextSlot,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCron => "the cron expression is not valid",
            Self::InvalidTimezone => "the time zone is not a known IANA zone",
            Self::NoNextSlot => "the schedule has no later slot",
        })
    }
}

impl std::error::Error for ScheduleError {}

/// The first slot of `cron` strictly after `after`, evaluated in the IANA zone `timezone`.
///
/// `cron` has five fields, or six when it starts with seconds. A wall-clock time that falls
/// in a DST gap runs at the first instant after the gap, and one that occurs twice in an
/// overlap runs once, so the same slot is never produced twice.
pub fn next_after(
    cron: &str,
    timezone: &str,
    after: OffsetDateTime,
) -> Result<OffsetDateTime, ScheduleError> {
    let pattern = parse_cron(cron)?;
    let zone = parse_timezone(timezone)?;
    let start = in_zone(after, zone)?;

    let next = pattern
        .find_next_occurrence(&start, false)
        .map_err(|_| ScheduleError::NoNextSlot)?;
    if next <= start {
        return Err(ScheduleError::NoNextSlot);
    }
    OffsetDateTime::from_unix_timestamp(next.timestamp())
        .map_err(|_| ScheduleError::NoNextSlot)?
        .replace_nanosecond(next.timestamp_subsec_nanos())
        .map_err(|_| ScheduleError::NoNextSlot)
}

/// How many slots [`latest_slot`] walks through before it jumps straight to the latest one.
const MAX_CATCH_UP: u64 = 10_000;

/// The slot to fire for a schedule whose `first` slot is due at `now`, and how many slots
/// were skipped to get there.
///
/// A schedule that is far behind fires only its latest due slot. `skipped` counts the
/// missed slots between `first` and the one returned, up to a limit of 10,000.
pub fn latest_slot(
    cron: &str,
    timezone: &str,
    first: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<(OffsetDateTime, u64), ScheduleError> {
    let mut slot = first;
    let mut skipped = 0;
    while skipped < MAX_CATCH_UP {
        let next = next_after(cron, timezone, slot)?;
        if next > now {
            return Ok((slot, skipped));
        }
        slot = next;
        skipped += 1;
    }

    // Too far behind to walk: take the last slot at or before `now` directly.
    let pattern = parse_cron(cron)?;
    let zone = parse_timezone(timezone)?;
    let at = in_zone(now, zone)?;
    let latest = pattern
        .find_previous_occurrence(&at, true)
        .map_err(|_| ScheduleError::NoNextSlot)?;
    let latest = OffsetDateTime::from_unix_timestamp(latest.timestamp())
        .map_err(|_| ScheduleError::NoNextSlot)?;
    Ok((latest.max(slot), skipped))
}

/// Checks that `cron` and `timezone` are usable, without computing a slot.
pub fn validate(cron: &str, timezone: &str) -> Result<(), ScheduleError> {
    parse_cron(cron)?;
    parse_timezone(timezone)?;
    Ok(())
}

fn parse_cron(cron: &str) -> Result<Cron, ScheduleError> {
    CronParser::builder()
        .seconds(Seconds::Optional)
        .build()
        .parse(cron)
        .map_err(|_| ScheduleError::InvalidCron)
}

fn in_zone(instant: OffsetDateTime, zone: Tz) -> Result<DateTime<Tz>, ScheduleError> {
    DateTime::<Utc>::from_timestamp(instant.unix_timestamp(), instant.nanosecond())
        .map(|utc| utc.with_timezone(&zone))
        .ok_or(ScheduleError::NoNextSlot)
}

fn parse_timezone(timezone: &str) -> Result<Tz, ScheduleError> {
    Tz::from_str(timezone).map_err(|_| ScheduleError::InvalidTimezone)
}

/// The UTC cron of a segment refresh every `cadence_minutes`, shifted by a stable phase taken
/// from `phase` (the segment's id), so a workspace's segments do not all refresh on the same
/// minute (ruling 303ac6ac). The interval stays exact: slots are `cadence_minutes` apart across
/// hour, day and week boundaries. `None` for a cadence outside the ruled set of 5, 10, 15, 30,
/// 60, 120, 180, 240, 360, 480, 720, 1440 and 10080 minutes.
///
/// The phase spreads the minute within the interval up to an hour, then the hour within the
/// interval up to a day, and the weekday for a week.
pub fn cadence_cron(cadence_minutes: i32, phase: u64) -> Option<String> {
    let minute = phase % 60;
    let hour = (phase / 60) % 24;
    let weekday = (phase / 1440) % 7;
    Some(match cadence_minutes {
        5 | 10 | 15 | 30 => {
            let every = u64::try_from(cadence_minutes).ok()?;
            format!("{}-59/{every} * * * *", phase % every)
        }
        60 => format!("{minute} * * * *"),
        120 | 180 | 240 | 360 | 480 | 720 => {
            let every = u64::try_from(cadence_minutes / 60).ok()?;
            format!("{minute} {}-23/{every} * * *", hour % every)
        }
        1440 => format!("{minute} {hour} * * *"),
        10080 => format!("{minute} {hour} * * {weekday}"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const BASE: Duration = Duration::from_secs(2);
    const CAP: Duration = Duration::from_secs(600);

    const CADENCES: [i32; 13] = [5, 10, 15, 30, 60, 120, 180, 240, 360, 480, 720, 1440, 10080];

    /// Phases that put the offset in every position of every field.
    fn phases() -> Vec<u64> {
        let mut phases: Vec<u64> = (0..100).map(|n| n * 1013 + 3).collect();
        phases.extend([0, 1, 59, 60, 1439, 10079, u64::MAX, 0x018f_0000_0000_7000]);
        phases
    }

    #[test]
    fn a_cadence_slot_comes_exactly_one_interval_after_the_last_across_hours_days_and_weeks() {
        let start = datetime!(2030-02-27 21:41:13 UTC);
        for cadence in CADENCES {
            for phase in phases() {
                let cron = cadence_cron(cadence, phase).unwrap();
                let mut slot = next_after(&cron, "UTC", start).unwrap();
                // The first slot is within one interval of the start.
                assert!(
                    slot - start <= time::Duration::minutes(i64::from(cadence)),
                    "{cron}"
                );
                let span = if cadence >= 1440 { 4 } else { 30 };
                for _ in 0..span {
                    let next = next_after(&cron, "UTC", slot).unwrap();
                    assert_eq!(
                        next - slot,
                        time::Duration::minutes(i64::from(cadence)),
                        "{cron} after {slot}"
                    );
                    slot = next;
                }
            }
        }
    }

    #[test]
    fn the_phase_spreads_segments_over_the_interval_and_is_stable() {
        let distinct = |cadence: i32| {
            let mut crons: Vec<String> = (0..10_080)
                .map(|phase| cadence_cron(cadence, phase).unwrap())
                .collect();
            crons.sort();
            crons.dedup();
            crons.len()
        };
        assert_eq!(distinct(5), 5);
        assert_eq!(distinct(30), 30);
        assert_eq!(distinct(60), 60);
        assert_eq!(distinct(120), 60 * 2);
        assert_eq!(distinct(720), 60 * 12);
        assert_eq!(distinct(1440), 60 * 24);
        assert_eq!(distinct(10080), 60 * 24 * 7);
        assert_eq!(cadence_cron(60, 4242), cadence_cron(60, 4242));
    }

    #[test]
    fn one_example_cron_per_cadence_class() {
        // phase 4242 = 70 h 42 min: minute 42, hour 22, weekday 2.
        assert_eq!(cadence_cron(15, 4242).as_deref(), Some("12-59/15 * * * *"));
        assert_eq!(cadence_cron(60, 4242).as_deref(), Some("42 * * * *"));
        assert_eq!(cadence_cron(360, 4242).as_deref(), Some("42 4-23/6 * * *"));
        assert_eq!(cadence_cron(1440, 4242).as_deref(), Some("42 22 * * *"));
        assert_eq!(cadence_cron(10080, 4242).as_deref(), Some("42 22 * * 2"));
    }

    #[test]
    fn a_cadence_outside_the_ruled_set_has_no_cron() {
        for cadence in [-5, 0, 1, 7, 45, 90, 1441, 10081] {
            assert_eq!(cadence_cron(cadence, 1), None, "{cadence}");
        }
        for cadence in CADENCES {
            let cron = cadence_cron(cadence, 99).unwrap();
            assert!(validate(&cron, "UTC").is_ok(), "{cron}");
        }
    }

    #[test]
    fn the_backoff_doubles_until_the_cap() {
        // With the jitter at its top (1.0) the delay is exactly min(base * 2^n, cap).
        assert_eq!(backoff(1, BASE, CAP, 1.0), Duration::from_secs(4));
        assert_eq!(backoff(3, BASE, CAP, 1.0), Duration::from_secs(16));
        assert_eq!(backoff(20, BASE, CAP, 1.0), CAP);
        assert_eq!(backoff(u32::MAX, BASE, CAP, 1.0), CAP);
    }

    #[test]
    fn the_jitter_scales_the_delay_between_half_and_whole() {
        assert_eq!(backoff(3, BASE, CAP, 0.0), Duration::from_secs(8));
        assert_eq!(backoff(3, BASE, CAP, 1.0), Duration::from_secs(16));
        for step in 0..=100 {
            let delay = backoff(3, BASE, CAP, f64::from(step) / 100.0);
            assert!(delay >= Duration::from_secs(8) && delay <= Duration::from_secs(16));
        }
    }

    #[test]
    fn an_out_of_range_random_unit_is_clamped() {
        assert_eq!(backoff(3, BASE, CAP, -5.0), backoff(3, BASE, CAP, 0.0));
        assert_eq!(backoff(3, BASE, CAP, 5.0), backoff(3, BASE, CAP, 1.0));
        assert_eq!(backoff(3, BASE, CAP, f64::NAN), backoff(3, BASE, CAP, 1.0));
    }

    #[test]
    fn the_next_slot_is_strictly_after_the_given_instant() {
        let slot = datetime!(2026-05-01 10:00 UTC);

        let next = next_after("0 * * * *", "UTC", slot).unwrap();

        assert_eq!(next, datetime!(2026-05-01 11:00 UTC));
    }

    #[test]
    fn a_six_field_expression_includes_seconds() {
        let next = next_after("*/15 * * * * *", "UTC", datetime!(2026-05-01 10:00:00 UTC));

        assert_eq!(next.unwrap(), datetime!(2026-05-01 10:00:15 UTC));
    }

    #[test]
    fn the_slot_follows_the_wall_clock_of_the_zone() {
        // 09:00 in Copenhagen is 07:00 UTC in summer and 08:00 UTC in winter.
        let summer = next_after(
            "0 9 * * *",
            "Europe/Copenhagen",
            datetime!(2026-07-01 00:00 UTC),
        );
        let winter = next_after(
            "0 9 * * *",
            "Europe/Copenhagen",
            datetime!(2026-12-01 00:00 UTC),
        );

        assert_eq!(summer.unwrap(), datetime!(2026-07-01 07:00 UTC));
        assert_eq!(winter.unwrap(), datetime!(2026-12-01 08:00 UTC));
    }

    #[test]
    fn a_wall_clock_time_in_the_spring_gap_runs_right_after_the_gap() {
        // On 2026-03-29 the clocks in Copenhagen jump from 02:00 to 03:00 (01:00 UTC), so
        // 02:30 does not exist.
        let before = datetime!(2026-03-28 02:30 +1);

        let in_gap = next_after("30 2 * * *", "Europe/Copenhagen", before).unwrap();
        let after_gap = next_after("30 2 * * *", "Europe/Copenhagen", in_gap).unwrap();

        assert_eq!(in_gap, datetime!(2026-03-29 03:00 +2));
        assert_eq!(after_gap, datetime!(2026-03-30 02:30 +2));
    }

    #[test]
    fn a_wall_clock_time_in_the_autumn_overlap_runs_once() {
        // On 2026-10-25 the clocks in Copenhagen go from 03:00 back to 02:00, so 02:30
        // happens twice. The slot must be produced once and the next one is the next day.
        let before = datetime!(2026-10-24 02:30 +2);

        let first = next_after("30 2 * * *", "Europe/Copenhagen", before).unwrap();
        let second = next_after("30 2 * * *", "Europe/Copenhagen", first).unwrap();

        assert_eq!(first, datetime!(2026-10-25 02:30 +2));
        assert_eq!(second, datetime!(2026-10-26 02:30 +1));
    }

    #[test]
    fn repeated_slots_across_a_dst_change_strictly_increase() {
        let mut slot = datetime!(2026-10-24 00:00 UTC);
        for _ in 0..80 {
            let next = next_after("*/30 * * * *", "Europe/Copenhagen", slot).unwrap();
            assert!(next > slot, "{next} is not after {slot}");
            slot = next;
        }
    }

    #[test]
    fn a_schedule_that_is_not_behind_fires_its_own_slot() {
        let first = datetime!(2026-05-01 10:00 UTC);

        let (slot, skipped) =
            latest_slot("0 * * * *", "UTC", first, datetime!(2026-05-01 10:30 UTC)).unwrap();

        assert_eq!((slot, skipped), (first, 0));
    }

    #[test]
    fn a_schedule_that_is_behind_fires_only_the_latest_slot() {
        let first = datetime!(2026-05-01 10:00 UTC);

        let (slot, skipped) =
            latest_slot("0 * * * *", "UTC", first, datetime!(2026-05-01 14:30 UTC)).unwrap();

        assert_eq!(slot, datetime!(2026-05-01 14:00 UTC));
        assert_eq!(skipped, 4);
    }

    #[test]
    fn a_schedule_far_behind_jumps_to_the_latest_slot() {
        let first = datetime!(2026-05-01 10:00 UTC);
        let now = datetime!(2026-05-01 10:00 UTC) + time::Duration::days(100);

        let (slot, skipped) = latest_slot("* * * * *", "UTC", first, now).unwrap();

        assert_eq!(slot, now);
        assert_eq!(skipped, MAX_CATCH_UP);
    }

    #[test]
    fn an_invalid_expression_or_zone_is_refused() {
        let now = datetime!(2026-05-01 10:00 UTC);

        assert_eq!(
            next_after("not a cron", "UTC", now),
            Err(ScheduleError::InvalidCron)
        );
        assert_eq!(
            next_after("0 * * * *", "Mars/Olympus", now),
            Err(ScheduleError::InvalidTimezone)
        );
        assert_eq!(validate("0 * * * *", "UTC"), Ok(()));
        assert_eq!(validate("", "UTC"), Err(ScheduleError::InvalidCron));
        assert_eq!(
            validate("0 * * * *", "nowhere"),
            Err(ScheduleError::InvalidTimezone)
        );
    }

    #[test]
    fn errors_describe_the_problem_without_echoing_input() {
        assert_eq!(
            ScheduleError::InvalidCron.to_string(),
            "the cron expression is not valid"
        );
        assert_eq!(
            ScheduleError::NoNextSlot.to_string(),
            "the schedule has no later slot"
        );
    }
}
