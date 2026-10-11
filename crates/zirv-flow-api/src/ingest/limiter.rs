//! The per-key rate limit: an in-process token bucket that counts items. It is per `serve`
//! process, so a deployment with several of them allows each key their sum.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use uuid::Uuid;

/// The least a bucket stays tracked after its last touch. A bucket is forgotten only once it
/// has been idle long enough to be full again: this, or `burst / rate` when that is longer.
const IDLE: Duration = Duration::from_secs(600);

/// How often a request sweeps the idle buckets.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct Buckets {
    by_key: HashMap<Uuid, Bucket>,
    swept: Instant,
}

/// Token buckets keyed by API key. Only authenticated keys reach it, so the map is bounded
/// by the keys that sent anything within the idle threshold (ten minutes, or the time to
/// refill a full burst when that is longer).
pub(crate) struct RateLimiter {
    rate: f64,
    burst: f64,
    idle: Duration,
    buckets: Mutex<Buckets>,
}

impl RateLimiter {
    /// `rate` items refill per second up to `burst` items.
    pub(crate) fn new(rate: u32, burst: u32, now: Instant) -> Self {
        let (rate, burst) = (f64::from(rate.max(1)), f64::from(burst.max(1)));
        Self {
            rate,
            burst,
            idle: IDLE.max(Duration::from_secs_f64(burst / rate)),
            buckets: Mutex::new(Buckets {
                by_key: HashMap::new(),
                swept: now,
            }),
        }
    }

    /// Takes `items` tokens from the key's bucket, or says how long until they fit, in
    /// whole seconds and at least one. A request larger than the burst is charged the burst,
    /// so it can pass when the bucket is full instead of never.
    pub(crate) fn take(&self, key: Uuid, items: usize, now: Instant) -> Result<(), u64> {
        let cost = (items as f64).min(self.burst);
        let mut buckets = self
            .buckets
            .lock()
            .expect("the limiter lock is never held across a panic");
        if now.saturating_duration_since(buckets.swept) >= SWEEP_EVERY {
            let idle = self.idle;
            buckets
                .by_key
                .retain(|_, bucket| now.saturating_duration_since(bucket.updated) < idle);
            buckets.swept = now;
        }
        let bucket = buckets.by_key.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            return Ok(());
        }
        let wait = (cost - bucket.tokens) / self.rate;
        Err((wait.ceil() as u64).max(1))
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets.lock().unwrap().by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(now: Instant) -> RateLimiter {
        RateLimiter::new(10, 20, now)
    }

    #[test]
    fn a_full_bucket_allows_a_burst_and_then_refuses() {
        let now = Instant::now();
        let limiter = limiter(now);
        let key = Uuid::new_v4();

        assert_eq!(limiter.take(key, 20, now), Ok(()));
        assert!(limiter.take(key, 1, now).is_err());
    }

    #[test]
    fn the_bucket_refills_at_the_rate_up_to_the_burst() {
        let now = Instant::now();
        let limiter = limiter(now);
        let key = Uuid::new_v4();
        limiter.take(key, 20, now).unwrap();

        assert!(
            limiter
                .take(key, 5, now + Duration::from_millis(400))
                .is_err()
        );
        assert_eq!(
            limiter.take(key, 5, now + Duration::from_millis(600)),
            Ok(())
        );
        assert_eq!(
            limiter.take(key, 20, now + Duration::from_secs(3600)),
            Ok(())
        );
        assert!(
            limiter
                .take(key, 1, now + Duration::from_secs(3600))
                .is_err()
        );
    }

    #[test]
    fn the_retry_after_rounds_up_to_whole_seconds_and_is_at_least_one() {
        let now = Instant::now();
        let limiter = limiter(now);
        let key = Uuid::new_v4();
        limiter.take(key, 20, now).unwrap();

        assert_eq!(limiter.take(key, 1, now), Err(1));
        assert_eq!(limiter.take(key, 10, now), Err(1));
        assert_eq!(limiter.take(key, 11, now), Err(2));
        assert_eq!(limiter.take(key, 20, now), Err(2));
    }

    #[test]
    fn a_refused_request_takes_nothing() {
        let now = Instant::now();
        let limiter = limiter(now);
        let key = Uuid::new_v4();
        limiter.take(key, 15, now).unwrap();

        assert!(limiter.take(key, 10, now).is_err());

        assert_eq!(limiter.take(key, 5, now), Ok(()));
    }

    #[test]
    fn keys_have_buckets_of_their_own() {
        let now = Instant::now();
        let limiter = limiter(now);
        let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
        limiter.take(one, 20, now).unwrap();

        assert!(limiter.take(one, 1, now).is_err());
        assert_eq!(limiter.take(two, 20, now), Ok(()));
    }

    #[test]
    fn a_request_larger_than_the_burst_is_charged_the_burst() {
        let now = Instant::now();
        let limiter = limiter(now);
        let key = Uuid::new_v4();

        assert_eq!(limiter.take(key, 500, now), Ok(()));
        assert!(limiter.take(key, 1, now).is_err());
    }

    #[test]
    fn buckets_idle_for_ten_minutes_are_evicted_and_active_ones_kept() {
        let start = Instant::now();
        let limiter = limiter(start);
        let (idle, active) = (Uuid::new_v4(), Uuid::new_v4());
        limiter.take(idle, 1, start).unwrap();
        limiter.take(active, 1, start).unwrap();
        limiter
            .take(active, 1, start + Duration::from_secs(550))
            .unwrap();
        assert_eq!(limiter.tracked(), 2);

        limiter
            .take(active, 1, start + Duration::from_secs(660))
            .unwrap();

        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn a_bucket_is_kept_until_it_would_be_full_again() {
        let start = Instant::now();
        let limiter = RateLimiter::new(1, 1000, start);
        let key = Uuid::new_v4();
        limiter.take(key, 1000, start).unwrap();

        let later = start + Duration::from_secs(601);

        assert!(limiter.take(key, 1000, later).is_err());
        assert_eq!(limiter.take(key, 600, later), Ok(()));
    }
}
