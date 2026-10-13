//! Source freshness: whether the data behind a stream is current (architecture 7.5).
//!
//! The state is derived, never stored: [`freshness`] is a pure function of when the stream last
//! succeeded, whether its last run failed, its threshold and the time. Time is a parameter, so
//! the verdict is testable with a fixed clock.

use std::fmt;
use std::str::FromStr;

use time::{Duration, OffsetDateTime};

use crate::schedule;

/// How many upcoming slots the default threshold looks at to find the longest gap.
const INTERVAL_SLOTS: usize = 8;

/// The verdict on a stream. Ordered by severity: `Fresh < Stale < Failing`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Freshness {
    Fresh,
    Stale,
    Failing,
}

impl Freshness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Failing => "failing",
        }
    }

    /// The number published on the per-stream state gauge: 0 fresh, 1 stale, 2 failing.
    pub fn gauge_value(self) -> f64 {
        match self {
            Self::Fresh => 0.0,
            Self::Stale => 1.0,
            Self::Failing => 2.0,
        }
    }
}

impl fmt::Display for Freshness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The text is not one of the three states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownFreshness;

impl fmt::Display for UnknownFreshness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the freshness state is not fresh, stale or failing")
    }
}

impl std::error::Error for UnknownFreshness {}

impl FromStr for Freshness {
    type Err = UnknownFreshness;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "fresh" => Ok(Self::Fresh),
            "stale" => Ok(Self::Stale),
            "failing" => Ok(Self::Failing),
            _ => Err(UnknownFreshness),
        }
    }
}

/// What the verdict is computed from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FreshnessInput {
    /// When the stream was last enabled; the grace period of a never-run stream starts here.
    pub enabled_at: OffsetDateTime,
    pub last_success_at: Option<OffsetDateTime>,
    pub last_run_failed: bool,
    /// `None` means the stream is never stale.
    pub threshold: Option<Duration>,
}

/// The time since the stream last proved itself current: `now - max(last_success_at,
/// enabled_at)`, never negative.
pub fn age(input: &FreshnessInput, now: OffsetDateTime) -> Duration {
    let reference = match input.last_success_at {
        Some(success) if success > input.enabled_at => success,
        _ => input.enabled_at,
    };
    (now - reference).max(Duration::ZERO)
}

/// The verdict. A failed last run is `Failing` whatever the age; with no threshold the stream is
/// never stale; otherwise it is stale once the age is past the threshold (equal is fresh).
pub fn freshness(input: &FreshnessInput, now: OffsetDateTime) -> Freshness {
    if input.last_run_failed {
        return Freshness::Failing;
    }
    match input.threshold {
        Some(threshold) if age(input, now) > threshold => Freshness::Stale,
        _ => Freshness::Fresh,
    }
}

/// The threshold in force and whether it is the schedule default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Threshold {
    pub duration: Duration,
    pub is_default: bool,
}

/// The explicit threshold when set; otherwise twice the longest gap between consecutive slots
/// among the next few slots of `schedule` (UTC); otherwise none.
///
/// The longest gap, not the gap between the next two, so a weekdays-only schedule counts the
/// weekend. An unparsable schedule, or one with fewer than two later slots, gives none.
pub fn effective_threshold(
    explicit_seconds: Option<i32>,
    schedule: Option<&str>,
    now: OffsetDateTime,
) -> Option<Threshold> {
    if let Some(seconds) = explicit_seconds {
        return Some(Threshold {
            duration: Duration::seconds(i64::from(seconds)),
            is_default: false,
        });
    }
    let interval = schedule_interval(schedule?, now)?;
    Some(Threshold {
        duration: interval.saturating_mul(2),
        is_default: true,
    })
}

fn schedule_interval(cron: &str, now: OffsetDateTime) -> Option<Duration> {
    let mut slots = Vec::with_capacity(INTERVAL_SLOTS);
    let mut cursor = now;
    for _ in 0..INTERVAL_SLOTS {
        match schedule::next_after(cron, "UTC", cursor) {
            Ok(slot) => {
                slots.push(slot);
                cursor = slot;
            }
            Err(_) => break,
        }
    }
    slots
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .max()
        .filter(|gap| gap.is_positive())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const ENABLED: OffsetDateTime = datetime!(2026-05-01 00:00 UTC);

    fn input(
        last_success_at: Option<OffsetDateTime>,
        last_run_failed: bool,
        threshold_seconds: Option<i64>,
    ) -> FreshnessInput {
        FreshnessInput {
            enabled_at: ENABLED,
            last_success_at,
            last_run_failed,
            threshold: threshold_seconds.map(Duration::seconds),
        }
    }

    #[test]
    fn the_state_table() {
        let success = datetime!(2026-05-01 10:00 UTC);
        let threshold = Some(3600);
        let rows: [(&str, FreshnessInput, OffsetDateTime, Freshness); 11] = [
            (
                "failed inside the threshold",
                input(Some(success), true, threshold),
                success + Duration::seconds(10),
                Freshness::Failing,
            ),
            (
                "failed far past the threshold",
                input(Some(success), true, threshold),
                success + Duration::days(9),
                Freshness::Failing,
            ),
            (
                "failed without a threshold",
                input(None, true, None),
                ENABLED + Duration::days(9),
                Freshness::Failing,
            ),
            (
                "success older than the threshold",
                input(Some(success), false, threshold),
                success + Duration::seconds(7200),
                Freshness::Stale,
            ),
            (
                "exactly at the threshold",
                input(Some(success), false, threshold),
                success + Duration::seconds(3600),
                Freshness::Fresh,
            ),
            (
                "one second past the threshold",
                input(Some(success), false, threshold),
                success + Duration::seconds(3601),
                Freshness::Stale,
            ),
            (
                "never run at enabled_at plus the threshold",
                input(None, false, threshold),
                ENABLED + Duration::seconds(3600),
                Freshness::Fresh,
            ),
            (
                "never run one second later",
                input(None, false, threshold),
                ENABLED + Duration::seconds(3601),
                Freshness::Stale,
            ),
            (
                "no threshold at any age",
                input(Some(success), false, None),
                success + Duration::days(3650),
                Freshness::Fresh,
            ),
            (
                "success after a failure",
                input(Some(success), false, threshold),
                success + Duration::seconds(5),
                Freshness::Fresh,
            ),
            (
                "enabled again after an old success",
                input(Some(ENABLED - Duration::days(30)), false, threshold),
                ENABLED + Duration::seconds(60),
                Freshness::Fresh,
            ),
        ];
        for (name, row, now, expected) in rows {
            assert_eq!(freshness(&row, now), expected, "{name}");
        }
    }

    #[test]
    fn the_age_restarts_from_enabled_at_and_is_never_negative() {
        let old = input(Some(ENABLED - Duration::days(30)), false, Some(60));
        assert_eq!(
            age(&old, ENABLED + Duration::seconds(10)),
            Duration::seconds(10)
        );
        assert_eq!(age(&old, ENABLED - Duration::seconds(10)), Duration::ZERO);
    }

    #[test]
    fn the_states_round_trip_and_order_by_severity() {
        for state in [Freshness::Fresh, Freshness::Stale, Freshness::Failing] {
            assert_eq!(state.as_str().parse::<Freshness>(), Ok(state));
        }
        assert_eq!("late".parse::<Freshness>(), Err(UnknownFreshness));
        assert!(Freshness::Fresh < Freshness::Stale);
        assert!(Freshness::Stale < Freshness::Failing);
        assert_eq!(Freshness::Failing.gauge_value(), 2.0);
    }

    #[test]
    fn an_explicit_threshold_wins_over_the_schedule() {
        let now = datetime!(2026-05-01 10:30 UTC);
        let threshold = effective_threshold(Some(90), Some("0 * * * *"), now);
        assert_eq!(
            threshold,
            Some(Threshold {
                duration: Duration::seconds(90),
                is_default: false
            })
        );
    }

    #[test]
    fn an_hourly_schedule_gives_two_hours() {
        let now = datetime!(2026-05-01 10:30 UTC);
        let threshold = effective_threshold(None, Some("0 * * * *"), now).unwrap();
        assert_eq!(threshold.duration, Duration::hours(2));
        assert!(threshold.is_default);
    }

    #[test]
    fn a_weekday_schedule_counts_the_weekend() {
        // 2026-05-04 is a Monday; from any weekday the next 8 slots span a weekend.
        let now = datetime!(2026-05-04 12:00 UTC);
        let threshold = effective_threshold(None, Some("0 9 * * 1-5"), now).unwrap();
        assert_eq!(threshold.duration, Duration::days(6));
    }

    #[test]
    fn no_schedule_or_an_invalid_one_gives_none() {
        let now = datetime!(2026-05-01 10:30 UTC);
        assert_eq!(effective_threshold(None, None, now), None);
        assert_eq!(effective_threshold(None, Some("not a cron"), now), None);
    }
}
