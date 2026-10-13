//! A token bucket per source, shared by every call the process makes for that source.
//!
//! The limit is per process. Workers are separate processes, so concurrent workers can exceed it
//! in aggregate. Honouring `Retry-After` mitigates the overshoot (an upstream that has had enough
//! says so, and the runtime waits) but does not prevent it. Enforcing the limit across workers
//! is revisited with OQ-9 at M6.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// How many requests a source tolerates in a period. The bucket holds `requests` tokens, so a
/// burst of that size goes through at once and the average stays at `requests` per `per`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RateLimit {
    pub requests: u32,
    pub per: Duration,
}

/// The buckets of one process, keyed by source id. Share one `Arc<RateLimits>` between every
/// [`SourceContext`](crate::sources::SourceContext) in the process, so a sync run, a connection
/// test and a discovery of the same source draw from the same bucket.
#[derive(Debug, Default)]
pub struct RateLimits {
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug)]
struct Bucket {
    rate: RateLimit,
    /// May go negative: a caller that has to wait has already taken its token.
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn new(rate: RateLimit, now: Instant) -> Self {
        Self {
            rate,
            tokens: capacity(rate),
            updated: now,
        }
    }

    /// Takes a token and returns how long until it is available.
    fn take(&mut self, now: Instant) -> Duration {
        let per_second = per_second(self.rate);
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_second).min(capacity(self.rate));
        self.updated = now;
        self.tokens -= 1.0;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / per_second)
        }
    }
}

fn capacity(rate: RateLimit) -> f64 {
    f64::from(rate.requests.max(1))
}

fn per_second(rate: RateLimit) -> f64 {
    capacity(rate) / rate.per.as_secs_f64().max(0.001)
}

impl RateLimits {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for a token of `source`'s bucket and returns how long it waited. A rate that
    /// differs from the bucket's replaces the bucket, so a changed setting takes effect.
    pub(crate) async fn acquire(&self, source: &str, rate: RateLimit) -> Duration {
        let wait = self.reserve(source, rate, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        wait
    }

    fn reserve(&self, source: &str, rate: RateLimit, now: Instant) -> Duration {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket = buckets
            .entry(source.to_owned())
            .or_insert_with(|| Bucket::new(rate, now));
        if bucket.rate != rate {
            *bucket = Bucket::new(rate, now);
        }
        bucket.take(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_PER_SECOND: RateLimit = RateLimit {
        requests: 2,
        per: Duration::from_secs(1),
    };

    #[test]
    fn a_burst_of_the_bucket_size_goes_through_and_the_next_waits_for_a_refill() {
        let limits = RateLimits::new();
        let start = Instant::now();

        assert_eq!(limits.reserve("a", TWO_PER_SECOND, start), Duration::ZERO);
        assert_eq!(limits.reserve("a", TWO_PER_SECOND, start), Duration::ZERO);
        assert_eq!(
            limits.reserve("a", TWO_PER_SECOND, start),
            Duration::from_millis(500)
        );
        // The waiter already holds its token, so the one behind it waits a slot longer.
        assert_eq!(
            limits.reserve("a", TWO_PER_SECOND, start),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn the_bucket_refills_with_time_but_never_past_its_size() {
        let limits = RateLimits::new();
        let start = Instant::now();
        limits.reserve("a", TWO_PER_SECOND, start);
        limits.reserve("a", TWO_PER_SECOND, start);

        let later = start + Duration::from_secs(10);
        assert_eq!(limits.reserve("a", TWO_PER_SECOND, later), Duration::ZERO);
        assert_eq!(limits.reserve("a", TWO_PER_SECOND, later), Duration::ZERO);
        assert!(limits.reserve("a", TWO_PER_SECOND, later) > Duration::ZERO);
    }

    #[test]
    fn sources_have_buckets_of_their_own() {
        let limits = RateLimits::new();
        let start = Instant::now();
        for _ in 0..2 {
            limits.reserve("a", TWO_PER_SECOND, start);
        }

        assert!(limits.reserve("a", TWO_PER_SECOND, start) > Duration::ZERO);
        assert_eq!(limits.reserve("b", TWO_PER_SECOND, start), Duration::ZERO);
    }

    #[test]
    fn a_changed_rate_replaces_the_bucket() {
        let limits = RateLimits::new();
        let start = Instant::now();
        for _ in 0..3 {
            limits.reserve("a", TWO_PER_SECOND, start);
        }
        let faster = RateLimit {
            requests: 10,
            per: Duration::from_secs(1),
        };

        assert_eq!(limits.reserve("a", faster, start), Duration::ZERO);
    }

    #[test]
    fn a_zero_rate_does_not_divide_by_zero() {
        let limits = RateLimits::new();
        let zero = RateLimit {
            requests: 0,
            per: Duration::ZERO,
        };
        let start = Instant::now();

        limits.reserve("a", zero, start);
        let wait = limits.reserve("a", zero, start);

        assert!(wait > Duration::ZERO);
    }

    #[tokio::test]
    async fn five_acquires_at_two_per_second_take_about_a_second_and_a_half() {
        let limits = RateLimits::new();
        let started = std::time::Instant::now();
        let mut waited = Duration::ZERO;

        for _ in 0..5 {
            waited += limits.acquire("a", TWO_PER_SECOND).await;
        }

        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(1450), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(2500), "{elapsed:?}");
        assert!(waited >= Duration::from_millis(1400), "{waited:?}");
    }
}
