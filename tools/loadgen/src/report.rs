//! The run report and its math. Percentiles are exact (nearest rank over every recorded
//! latency), so no histogram precision caveat applies.

use serde::Serialize;

/// Latency in milliseconds over the accepted (202) responses of a phase or a run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Latency {
    pub count: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// Nearest-rank percentile of an ascending slice: the smallest value with at least `p` percent
/// of the samples at or below it.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl Latency {
    /// From latencies in microseconds, in any order.
    pub fn from_micros(mut micros: Vec<u64>) -> Self {
        micros.sort_unstable();
        let ms = |value: u64| value as f64 / 1000.0;
        Self {
            count: micros.len() as u64,
            p50_ms: ms(percentile(&micros, 50.0)),
            p95_ms: ms(percentile(&micros, 95.0)),
            p99_ms: ms(percentile(&micros, 99.0)),
            max_ms: ms(micros.last().copied().unwrap_or(0)),
        }
    }
}

/// Response counts. `transport_errors` (no response at all) are also counted in `other`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StatusCounts {
    pub accepted_202: u64,
    pub rate_limited_429: u64,
    pub server_5xx: u64,
    pub other: u64,
    pub transport_errors: u64,
}

impl StatusCounts {
    pub fn record(&mut self, status: Option<u16>) {
        match status {
            Some(202) => self.accepted_202 += 1,
            Some(429) => self.rate_limited_429 += 1,
            Some(500..=599) => self.server_5xx += 1,
            Some(_) => self.other += 1,
            None => {
                self.other += 1;
                self.transport_errors += 1;
            }
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.accepted_202 += other.accepted_202;
        self.rate_limited_429 += other.rate_limited_429;
        self.server_5xx += other.server_5xx;
        self.other += other.other;
        self.transport_errors += other.transport_errors;
    }
}

/// Everything measured in one phase (or, merged, in the whole run).
#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseReport {
    pub name: String,
    pub target_rate: u32,
    pub planned_duration_s: f64,
    /// Wall time from the first request to the last response, pauses included.
    pub elapsed_s: f64,
    pub requests: u64,
    pub items_offered: u64,
    pub items_accepted: u64,
    /// Accepted items per wall second.
    pub achieved_rate: f64,
    pub retries_after_429: u64,
    pub status: StatusCounts,
    pub latency: Latency,
}

pub fn achieved_rate(items_accepted: u64, elapsed_s: f64) -> f64 {
    if elapsed_s <= 0.0 {
        0.0
    } else {
        items_accepted as f64 / elapsed_s
    }
}

#[derive(Serialize)]
pub struct Report {
    pub tool: &'static str,
    pub mode: String,
    pub seed: u64,
    pub run_id: String,
    pub started_unix_s: u64,
    pub batch_size: u32,
    pub concurrency: u32,
    pub people: u64,
    pub total: PhaseReport,
    pub phases: Vec<PhaseReport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let latency = Latency::from_micros((1..=100).map(|n| n * 1000).rev().collect());
        assert_eq!(latency.count, 100);
        assert_eq!(latency.p50_ms, 50.0);
        assert_eq!(latency.p95_ms, 95.0);
        assert_eq!(latency.p99_ms, 99.0);
        assert_eq!(latency.max_ms, 100.0);
    }

    #[test]
    fn a_small_sample_rounds_the_rank_up() {
        let latency = Latency::from_micros(vec![4000, 1000, 3000, 2000]);
        assert_eq!(latency.p50_ms, 2.0);
        assert_eq!(latency.p95_ms, 4.0);
        assert_eq!(latency.p99_ms, 4.0);
        assert_eq!(latency.max_ms, 4.0);
    }

    #[test]
    fn no_samples_report_zeros() {
        assert_eq!(Latency::from_micros(Vec::new()), Latency::default());
    }

    #[test]
    fn statuses_are_classified_and_transport_errors_count_as_other() {
        let mut counts = StatusCounts::default();
        for status in [Some(202), Some(202), Some(429), Some(503), Some(400), None] {
            counts.record(status);
        }
        assert_eq!(
            counts,
            StatusCounts {
                accepted_202: 2,
                rate_limited_429: 1,
                server_5xx: 1,
                other: 2,
                transport_errors: 1,
            }
        );
    }

    #[test]
    fn the_achieved_rate_is_accepted_items_per_elapsed_second() {
        assert_eq!(achieved_rate(1800, 0.9), 2000.0);
        assert_eq!(achieved_rate(10, 0.0), 0.0);
    }
}
