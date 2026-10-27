//! Latency samples and their percentiles.

use std::time::Duration;

/// The nearest-rank percentile of `sorted` (ascending): the smallest sample with at least `p`
/// percent of the samples at or below it. `None` for no samples.
pub(crate) fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted.get(rank - 1).or(sorted.last()).copied()
}

/// What a run reports about its measured samples, in microseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Summary {
    pub(crate) count: usize,
    pub(crate) p50: u64,
    pub(crate) p95: u64,
    pub(crate) p99: u64,
    pub(crate) max: u64,
}

impl Summary {
    /// The summary of `samples` in the order they were taken, leaving out the first `warmup`.
    pub(crate) fn of(samples: &[u64], warmup: usize) -> Option<Self> {
        let mut measured: Vec<u64> = samples.iter().skip(warmup).copied().collect();
        measured.sort_unstable();
        Some(Self {
            count: measured.len(),
            p50: percentile(&measured, 50.0)?,
            p95: percentile(&measured, 95.0)?,
            p99: percentile(&measured, 99.0)?,
            max: *measured.last()?,
        })
    }
}

pub(crate) fn micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

pub(crate) fn millis(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_to_hundred() -> Vec<u64> {
        (1..=100).collect()
    }

    #[test]
    fn nearest_rank_percentiles_of_a_hundred_samples_are_the_sample_at_that_rank() {
        let samples = one_to_hundred();
        assert_eq!(percentile(&samples, 50.0), Some(50));
        assert_eq!(percentile(&samples, 95.0), Some(95));
        assert_eq!(percentile(&samples, 99.0), Some(99));
        assert_eq!(percentile(&samples, 100.0), Some(100));
    }

    #[test]
    fn a_percentile_rounds_the_rank_up_and_never_leaves_the_samples() {
        let samples = [10, 20, 30, 40];
        assert_eq!(percentile(&samples, 50.0), Some(20));
        assert_eq!(percentile(&samples, 51.0), Some(30));
        assert_eq!(percentile(&samples, 0.0), Some(10));
        assert_eq!(percentile(&samples, 100.0), Some(40));
        assert_eq!(percentile(&[7], 99.0), Some(7));
    }

    #[test]
    fn no_samples_have_no_percentile() {
        assert_eq!(percentile(&[], 95.0), None);
        assert_eq!(Summary::of(&[], 0), None);
    }

    #[test]
    fn the_summary_sorts_the_samples_it_is_given_in_arrival_order() {
        let mut samples = one_to_hundred();
        samples.reverse();
        let summary = Summary::of(&samples, 0).expect("samples");
        assert_eq!(
            summary,
            Summary {
                count: 100,
                p50: 50,
                p95: 95,
                p99: 99,
                max: 100
            }
        );
    }

    #[test]
    fn the_warm_up_samples_do_not_count() {
        // Ten slow warm-up renders, then ninety fast ones: the slow ones must not show anywhere.
        let mut samples = vec![9_000; 10];
        samples.extend(1..=90);
        let summary = Summary::of(&samples, 10).expect("samples");
        assert_eq!(summary.count, 90);
        assert_eq!(summary.max, 90);
        assert_eq!(summary.p99, 90);
        assert_eq!(summary.p50, 45);
        assert_eq!(Summary::of(&samples, 90 + 10), None);
    }

    #[test]
    fn microseconds_print_as_milliseconds_to_three_places() {
        assert_eq!(millis(12_345), 12.345);
        assert_eq!(millis(50_000), 50.0);
        assert_eq!(micros(Duration::from_millis(3)), 3_000);
    }
}
