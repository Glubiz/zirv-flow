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
    state: Mutex<State>,
}

/// The buckets and the reservation counter they share. The counter belongs to the registry, not
/// to a bucket, so a number is never handed out twice even when a bucket is replaced: a cancelled
/// reservation of an old bucket can never match a re-created one.
#[derive(Debug, Default)]
struct State {
    buckets: HashMap<String, Bucket>,
    next_number: u64,
}

impl State {
    fn fresh_number(&mut self) -> u64 {
        self.next_number += 1;
        self.next_number
    }
}

#[derive(Debug)]
struct Bucket {
    rate: RateLimit,
    /// May go negative: a caller that has to wait has already taken its token.
    tokens: f64,
    updated: Instant,
    /// The number of the latest reservation, so a cancelled one can tell whether it is the tail.
    latest: u64,
}

impl Bucket {
    fn new(rate: RateLimit, now: Instant) -> Self {
        Self {
            rate,
            tokens: capacity(rate),
            updated: now,
            latest: 0,
        }
    }

    /// Takes a token and returns how long until it is available.
    fn take(&mut self, now: Instant, number: u64) -> Duration {
        let per_second = per_second(self.rate);
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_second).min(capacity(self.rate));
        self.updated = now;
        self.tokens -= 1.0;
        self.latest = number;
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
    ///
    /// Cancelling the wait (a timeout, a dropped request) gives the token back when no other call
    /// queued behind it, so a call that never made its request does not leave a debt. When others
    /// did queue, its slot stays idle, so the rate is never exceeded; the debt is bounded by the
    /// number of queued waiters.
    pub(crate) async fn acquire(&self, source: &str, rate: RateLimit) -> Duration {
        let (wait, number) = self.reserve_numbered(source, rate, Instant::now());
        if wait.is_zero() {
            return wait;
        }
        let mut reservation = Reservation {
            limits: self,
            source,
            rate,
            number,
            used: false,
        };
        tokio::time::sleep(wait).await;
        reservation.used = true;
        wait
    }

    /// Gives back the token of reservation `number` if it was never used and no reservation came
    /// after it. A later one is already scheduled behind it, so the token cannot be handed out
    /// again without two callers sharing a slot: the abandoned slot is idle time instead.
    fn refund(&self, source: &str, rate: RateLimit, number: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let fresh = state.fresh_number();
        if let Some(bucket) = state
            .buckets
            .get_mut(source)
            .filter(|bucket| bucket.rate == rate && bucket.latest == number)
        {
            bucket.tokens = (bucket.tokens + 1.0).min(capacity(rate));
            // The next reservation is the tail's successor, not a replacement for it, and the
            // fresh number cannot match a reservation of any bucket, old or new.
            bucket.latest = fresh;
        }
    }

    #[cfg(test)]
    fn reserve(&self, source: &str, rate: RateLimit, now: Instant) -> Duration {
        self.reserve_numbered(source, rate, now).0
    }

    /// As `reserve`, with the number of the reservation for `refund`.
    fn reserve_numbered(&self, source: &str, rate: RateLimit, now: Instant) -> (Duration, u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let number = state.fresh_number();
        let bucket = state
            .buckets
            .entry(source.to_owned())
            .or_insert_with(|| Bucket::new(rate, now));
        if bucket.rate != rate {
            *bucket = Bucket::new(rate, now);
        }
        let wait = bucket.take(now, number);
        (wait, number)
    }
}

/// A token taken for a wait in progress. Dropped before the wait ended, it is refunded if no
/// reservation was made after it.
struct Reservation<'a> {
    limits: &'a RateLimits,
    source: &'a str,
    rate: RateLimit,
    number: u64,
    used: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.used {
            self.limits.refund(self.source, self.rate, self.number);
        }
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
        assert!(waited >= Duration::from_millis(1400), "{waited:?}");
    }

    #[tokio::test]
    async fn cancelled_waits_give_their_tokens_back() {
        let one_per_second = RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
        };
        let limits = RateLimits::new();
        assert_eq!(limits.acquire("a", one_per_second).await, Duration::ZERO);

        // Five callers queue behind it and give up, as a request timeout does.
        for _ in 0..5 {
            let queued = tokio::time::timeout(
                Duration::from_millis(10),
                limits.acquire("a", one_per_second),
            )
            .await;
            assert!(queued.is_err());
        }

        // The wait is computed from the bucket, never longer than for a bucket nobody queued on.
        let wait = limits.reserve("a", one_per_second, Instant::now());
        assert!(wait <= Duration::from_secs(1), "{wait:?}");
    }

    #[test]
    fn a_cancelled_wait_with_waiters_behind_it_leaves_an_idle_slot_not_a_shared_one() {
        let one_per_second = RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
        };
        let limits = RateLimits::new();
        let start = Instant::now();
        let second = Duration::from_secs(1);
        limits.reserve("a", one_per_second, start);
        let (a, a_number) = limits.reserve_numbered("a", one_per_second, start);
        let (b, _) = limits.reserve_numbered("a", one_per_second, start);
        assert_eq!((a, b), (second, 2 * second));

        // A gives up while B is queued behind it, then C asks one second later.
        limits.refund("a", one_per_second, a_number);
        let (c, _) = limits.reserve_numbered("a", one_per_second, start + second);

        // B goes at t=2 and C a full interval after it, at t=3: they do not share a slot.
        assert!(start + second + c >= start + b + second);
    }

    #[test]
    fn a_cancelled_reservation_of_a_replaced_bucket_cannot_touch_the_recreated_one() {
        let one_per_second = RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
        };
        let other = RateLimit {
            requests: 5,
            per: Duration::from_secs(1),
        };
        let limits = RateLimits::new();
        let start = Instant::now();
        let second = Duration::from_secs(1);
        // Rate A: the first reservation is free, the second is the queued tail.
        limits.reserve("a", one_per_second, start);
        let (_, stale) = limits.reserve_numbered("a", one_per_second, start);
        // A -> B -> A: each change replaces the bucket.
        limits.reserve("a", other, start);
        let (free, _) = limits.reserve_numbered("a", one_per_second, start);
        let (queued, _) = limits.reserve_numbered("a", one_per_second, start);
        assert_eq!((free, queued), (Duration::ZERO, second));

        // The stale reservation is dropped. With numbers restarting per bucket it would carry the
        // number of the new bucket's tail and refund the token of a reservation it never made.
        limits.refund("a", one_per_second, stale);

        // The next caller goes a full interval after the queued one: they do not share a slot.
        let (next, _) = limits.reserve_numbered("a", one_per_second, start);
        assert_eq!(next, 2 * second);
    }

    #[test]
    fn reservation_numbers_never_repeat_across_bucket_replacement() {
        let a = RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
        };
        let b = RateLimit {
            requests: 2,
            per: Duration::from_secs(1),
        };
        let limits = RateLimits::new();
        let start = Instant::now();
        let mut seen = Vec::new();
        for rate in [a, b, a, b, a] {
            seen.push(limits.reserve_numbered("s", rate, start).1);
        }
        let mut sorted = seen.clone();
        sorted.dedup();
        assert_eq!(sorted, seen);
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn the_tail_reservation_gives_its_token_back_once() {
        let one_per_second = RateLimit {
            requests: 1,
            per: Duration::from_secs(1),
        };
        let limits = RateLimits::new();
        let start = Instant::now();
        limits.reserve("a", one_per_second, start);
        let (_, number) = limits.reserve_numbered("a", one_per_second, start);

        limits.refund("a", one_per_second, number);
        limits.refund("a", one_per_second, number);

        // One refund, not two: the next caller waits one interval, as if the tail never came.
        assert_eq!(
            limits.reserve("a", one_per_second, start),
            Duration::from_secs(1)
        );
    }
}
