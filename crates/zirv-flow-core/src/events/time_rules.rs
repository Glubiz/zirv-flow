//! `occurred_at`, clamping and lateness (architecture section 12.2).

use time::{Duration, OffsetDateTime};

/// A flag stored with an event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventFlag {
    /// The client timestamp lay beyond the future-skew limit and was set to `received_at`.
    Clamped,
    /// The event is older than the lateness window.
    Late,
}

impl EventFlag {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clamped => "clamped",
            Self::Late => "late",
        }
    }
}

/// The time of an event after the rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Timing {
    pub occurred_at: OffsetDateTime,
    pub flags: Vec<EventFlag>,
}

impl Timing {
    pub fn is_late(&self) -> bool {
        self.flags.contains(&EventFlag::Late)
    }
}

/// `occurred_at` is the client timestamp, or `received_at` when there is none. A timestamp
/// later than `received_at + future_skew_seconds` becomes `received_at` and is flagged
/// `clamped`; one earlier than `received_at - lateness_seconds` is flagged `late`.
pub fn apply_time_rules(
    timestamp: Option<OffsetDateTime>,
    received_at: OffsetDateTime,
    future_skew_seconds: i64,
    lateness_seconds: i64,
) -> Timing {
    let mut flags = Vec::new();
    let mut occurred_at = timestamp.unwrap_or(received_at);
    if occurred_at > received_at.saturating_add(Duration::seconds(future_skew_seconds)) {
        occurred_at = received_at;
        flags.push(EventFlag::Clamped);
    }
    if occurred_at < received_at.saturating_sub(Duration::seconds(lateness_seconds)) {
        flags.push(EventFlag::Late);
    }
    Timing { occurred_at, flags }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    #[test]
    fn no_timestamp_means_received_at() {
        let timing = apply_time_rules(None, NOW, 300, 86_400);

        assert_eq!(timing.occurred_at, NOW);
        assert!(timing.flags.is_empty());
    }

    #[test]
    fn a_timestamp_within_the_skew_is_kept() {
        let timestamp = NOW + Duration::seconds(300);

        let timing = apply_time_rules(Some(timestamp), NOW, 300, 86_400);

        assert_eq!(timing.occurred_at, timestamp);
        assert!(timing.flags.is_empty());
    }

    #[test]
    fn an_excessive_future_timestamp_is_clamped_and_flagged() {
        let timing = apply_time_rules(Some(NOW + Duration::seconds(301)), NOW, 300, 86_400);

        assert_eq!(timing.occurred_at, NOW);
        assert_eq!(timing.flags, [EventFlag::Clamped]);
    }

    #[test]
    fn an_old_timestamp_is_kept_and_flagged_late() {
        let timestamp = NOW - Duration::seconds(86_401);

        let timing = apply_time_rules(Some(timestamp), NOW, 300, 86_400);

        assert_eq!(timing.occurred_at, timestamp);
        assert_eq!(timing.flags, [EventFlag::Late]);
        assert!(timing.is_late());
        let on_the_edge = apply_time_rules(Some(NOW - Duration::seconds(86_400)), NOW, 300, 86_400);
        assert!(!on_the_edge.is_late());
    }

    #[test]
    fn flags_have_stable_names() {
        assert_eq!(EventFlag::Clamped.as_str(), "clamped");
        assert_eq!(EventFlag::Late.as_str(), "late");
    }
}
