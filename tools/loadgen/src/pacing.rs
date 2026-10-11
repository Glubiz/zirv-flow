//! Open-loop pacing: requests are due at fixed offsets from the phase start, whatever the
//! server's speed, so a slow server shows as latency and a lower achieved rate rather than
//! as a quietly reduced offered load.

use std::time::Duration;

/// The longest `Retry-After` wait honoured, in seconds.
pub const MAX_RETRY_AFTER: u64 = 60;

/// When the request whose first item is item number `sent_items` is due, counted from the
/// phase start in active time (time spent obeying a `Retry-After` pause does not count).
pub fn request_due_at(sent_items: u64, rate: u32) -> Duration {
    Duration::from_secs_f64(sent_items as f64 / f64::from(rate.max(1)))
}

/// The `Retry-After` wait in whole seconds, at least 1 and at most [`MAX_RETRY_AFTER`]; a
/// missing or unreadable header waits 1 second.
pub fn retry_after(header: Option<&str>) -> Duration {
    let seconds = header
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds.clamp(1, MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_due_at_even_offsets_of_the_item_rate() {
        assert_eq!(request_due_at(0, 2000), Duration::ZERO);
        assert_eq!(request_due_at(500, 2000), Duration::from_millis(250));
        assert_eq!(request_due_at(2000, 2000), Duration::from_secs(1));
        assert_eq!(request_due_at(10_000, 10_000), Duration::from_secs(1));
    }

    #[test]
    fn a_zero_rate_does_not_divide_by_zero() {
        assert_eq!(request_due_at(5, 0), Duration::from_secs(5));
    }

    #[test]
    fn retry_after_is_whole_seconds_clamped_to_one_to_sixty() {
        assert_eq!(retry_after(Some("3")), Duration::from_secs(3));
        assert_eq!(retry_after(Some(" 7 ")), Duration::from_secs(7));
        assert_eq!(retry_after(Some("0")), Duration::from_secs(1));
        assert_eq!(retry_after(Some("100000")), Duration::from_secs(60));
        assert_eq!(retry_after(Some("soon")), Duration::from_secs(1));
        assert_eq!(retry_after(None), Duration::from_secs(1));
    }
}
