//! The send-throughput report: the rate at the sink over a steady window, the delay from
//! enrollment to the sink as percentiles, and one line each for target, result and verdict.
//! Percentiles are exact (nearest rank over every sample).
//!
//! Only real, unique deliveries count: an arrival for an address that is not in this run's
//! enrollment export, or a second arrival for an address, is counted apart and fails the run.

use hashbrown::{HashMap, HashSet};
use serde::Serialize;

use crate::sink::Arrival;

/// Nearest-rank percentile of an ascending slice.
pub fn percentile<T: Copy + Default>(sorted: &[T], p: f64) -> T {
    if sorted.is_empty() {
        return T::default();
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Seconds at the percentiles; `None` when there are no samples (never 0, which would read as a
/// real result).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50_s: Option<f64>,
    pub p95_s: Option<f64>,
    pub p99_s: Option<f64>,
    pub max_s: Option<f64>,
}

impl Percentiles {
    /// From signed microseconds, in any order.
    pub fn from_micros(mut micros: Vec<i64>) -> Self {
        if micros.is_empty() {
            return Self::default();
        }
        micros.sort_unstable();
        let s = |p: f64| Some(percentile(&micros, p) as f64 / 1e6);
        Self {
            count: micros.len() as u64,
            p50_s: s(50.0),
            p95_s: s(95.0),
            p99_s: s(99.0),
            max_s: micros.last().map(|v| *v as f64 / 1e6),
        }
    }
}

/// The steady window: from the arrival of the message at rank `trim` percent to the one at rank
/// `100 - trim` percent, which leaves the ramp-up and the tail out of the rate.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Window {
    pub messages: u64,
    pub start_us: u64,
    pub end_us: u64,
    pub seconds: f64,
    pub rate: f64,
}

/// `at_us` ascending. The messages after the first edge message, up to and including the last
/// edge message, are counted over the time between the two.
pub fn steady_window(at_us: &[u64], trim_percent: f64) -> Window {
    if at_us.len() < 3 {
        return Window::default();
    }
    let n = at_us.len();
    let low = ((trim_percent / 100.0) * n as f64).floor() as usize;
    let high = n - 1 - low.min(n - 1);
    if high <= low {
        return Window::default();
    }
    let (start, end) = (at_us[low], at_us[high]);
    let seconds = (end - start) as f64 / 1e6;
    let messages = (high - low) as u64;
    Window {
        messages,
        start_us: start,
        end_us: end,
        seconds,
        rate: if seconds > 0.0 {
            messages as f64 / seconds
        } else {
            0.0
        },
    }
}

/// The busiest one-second span: the most arrivals in any `[t, t + 1 s)`. `at_us` ascending.
pub fn peak_per_second(at_us: &[u64]) -> u64 {
    let mut best = 0;
    let mut tail = 0;
    for head in 0..at_us.len() {
        while at_us[head] - at_us[tail] >= 1_000_000 {
            tail += 1;
        }
        best = best.max(head - tail + 1);
    }
    best as u64
}

/// The database clock against the host clock, measured right before and right after the run:
/// `offset = database - host`, each from the midpoint of a host-time bracket around the query.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Clocks {
    pub offset_before_us: i64,
    pub offset_after_us: i64,
    /// The wider of the two brackets: how far a midpoint can be from the truth.
    pub uncertainty_us: u64,
}

impl Clocks {
    fn offset_us(&self) -> i64 {
        (self.offset_before_us + self.offset_after_us) / 2
    }
}

/// The destination's own token bucket, when it is what holds the run to the target.
#[derive(Clone, Debug, Serialize)]
pub struct Bucket {
    pub rate: f64,
    pub burst: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Inputs {
    pub label: String,
    pub target_rate: f64,
    /// Fraction under the target that still counts as met and, with a bucket, over the bucket's
    /// rate that is still believable.
    pub tolerance: f64,
    pub expected: u64,
    pub trim_percent: f64,
    pub clocks: Clocks,
    pub bucket: Option<Bucket>,
}

/// The upper bound of a bucket-limited run: the window rate may not exceed
/// `rate * (1 + tolerance) + burst / window seconds` (the burst's tokens spent in the window), nor
/// the busiest second `rate * (1 + tolerance) + burst`.
#[derive(Clone, Debug, Serialize)]
pub struct BucketCheck {
    pub rate: f64,
    pub burst_slack_messages: f64,
    pub tolerance: f64,
    pub window_rate_limit: f64,
    pub peak_limit: f64,
    pub within: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct QueueStats {
    pub samples: u64,
    pub max_depth: f64,
    pub max_oldest_s: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CpuStats {
    pub samples: u64,
    pub mean_pct: Option<f64>,
    pub max_pct: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub met: bool,
    /// Every enrolled person has exactly one delivery, and nothing else arrived.
    pub complete: bool,
    /// False when the run proves nothing: no matched delivery, or clocks that contradict.
    pub valid: bool,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub inputs: Inputs,
    /// Every arrival in the log.
    pub arrivals: u64,
    /// Distinct addresses of this run's enrollments that got a message: what the rate counts.
    pub unique_delivered: u64,
    pub duplicates: u64,
    pub unmatched_arrivals: u64,
    pub overall_rate: f64,
    pub peak_per_second: u64,
    pub window: Window,
    pub delay: Percentiles,
    pub negative_delays: u64,
    /// Delays below minus the clock uncertainty: impossible, so the clocks or the join are wrong.
    pub clock_violations: u64,
    pub bucket_check: Option<BucketCheck>,
    pub queue: Option<QueueStats>,
    pub db_cpu: Option<CpuStats>,
    pub verdict: Verdict,
}

/// `enrolled`: address (lower case) to enrollment time in unix microseconds of the DATABASE
/// clock.
pub fn build(inputs: Inputs, arrivals: &[Arrival], enrolled: &HashMap<String, u64>) -> Report {
    let mut sorted: Vec<&Arrival> = arrivals.iter().collect();
    sorted.sort_by_key(|a| a.at_us);
    let offset = inputs.clocks.offset_us();
    let uncertainty = inputs.clocks.uncertainty_us as i64;
    let mut seen: HashSet<String> = HashSet::new();
    let (mut duplicates, mut unmatched) = (0u64, 0u64);
    let mut at: Vec<u64> = Vec::new();
    let mut delays: Vec<i64> = Vec::new();
    for arrival in sorted {
        let rcpt = arrival.rcpt.to_ascii_lowercase();
        let Some(enrolled_db) = enrolled.get(&rcpt) else {
            unmatched += 1;
            continue;
        };
        if !seen.insert(rcpt) {
            duplicates += 1;
            continue;
        }
        at.push(arrival.at_us);
        delays.push(arrival.at_us as i64 - (*enrolled_db as i64 - offset));
    }
    let unique = at.len() as u64;
    let window = steady_window(&at, inputs.trim_percent);
    let total_s = match (at.first(), at.last()) {
        (Some(first), Some(last)) if last > first => (last - first) as f64 / 1e6,
        _ => 0.0,
    };
    let negative = delays.iter().filter(|d| **d < 0).count() as u64;
    let violations = delays.iter().filter(|d| **d < -uncertainty).count() as u64;
    let peak = peak_per_second(&at);

    let mut problems = Vec::new();
    if unique == 0 {
        problems.push("no delivery matches an enrollment of this run".to_owned());
    }
    if violations > 0 {
        problems.push(format!(
            "{violations} deliveries precede their enrollment by more than the clock uncertainty"
        ));
    }
    if duplicates > 0 {
        problems.push(format!("{duplicates} duplicate deliveries"));
    }
    if unmatched > 0 {
        problems.push(format!(
            "{unmatched} arrivals match no enrollment of this run"
        ));
    }
    let bucket_check = inputs.bucket.as_ref().map(|bucket| {
        let ceiling = bucket.rate * (1.0 + inputs.tolerance);
        let window_rate_limit = ceiling
            + if window.seconds > 0.0 {
                bucket.burst / window.seconds
            } else {
                bucket.burst
            };
        let peak_limit = ceiling + bucket.burst;
        let within = window.rate <= window_rate_limit && peak as f64 <= peak_limit;
        if !within {
            problems.push(format!(
                "the rate exceeds the destination bucket ({:.1}/s window limit {:.1}, busiest second {} limit {:.1}): the run is not what it says",
                window.rate, window_rate_limit, peak, peak_limit
            ));
        }
        BucketCheck {
            rate: bucket.rate,
            burst_slack_messages: bucket.burst,
            tolerance: inputs.tolerance,
            window_rate_limit,
            peak_limit,
            within,
        }
    });
    let valid = unique > 0 && violations == 0;
    let complete = unique >= inputs.expected && duplicates == 0 && unmatched == 0;
    let lower_ok = window.rate >= inputs.target_rate * (1.0 - inputs.tolerance);
    let met = valid
        && duplicates == 0
        && unmatched == 0
        && window.messages > 0
        && lower_ok
        && bucket_check.as_ref().is_none_or(|check| check.within);
    Report {
        arrivals: arrivals.len() as u64,
        unique_delivered: unique,
        duplicates,
        unmatched_arrivals: unmatched,
        overall_rate: if total_s > 0.0 {
            unique as f64 / total_s
        } else {
            0.0
        },
        peak_per_second: peak,
        window,
        delay: Percentiles::from_micros(delays),
        negative_delays: negative,
        clock_violations: violations,
        bucket_check,
        queue: None,
        db_cpu: None,
        verdict: Verdict {
            met,
            complete,
            valid,
            problems,
        },
        inputs,
    }
}

impl Report {
    /// Adds the destination queue samples (`t_us,depth,oldest_s`) that fall in the steady window.
    pub fn with_queue(mut self, samples: &[(u64, f64, f64)]) -> Self {
        let inside = samples
            .iter()
            .filter(|(t, _, _)| *t >= self.window.start_us && *t <= self.window.end_us);
        let mut stats = QueueStats::default();
        for (_, depth, oldest) in inside {
            stats.samples += 1;
            stats.max_depth = stats.max_depth.max(*depth);
            stats.max_oldest_s = stats.max_oldest_s.max(*oldest);
        }
        self.queue = Some(stats);
        self
    }

    /// Adds the database CPU samples (`t_us`, percent of one core) inside the steady window.
    pub fn with_db_cpu(mut self, samples: &[(u64, f64)]) -> Self {
        let values: Vec<f64> = samples
            .iter()
            .filter(|(t, _)| *t >= self.window.start_us && *t <= self.window.end_us)
            .map(|(_, v)| *v)
            .collect();
        self.db_cpu = Some(CpuStats {
            samples: values.len() as u64,
            mean_pct: (!values.is_empty())
                .then(|| values.iter().sum::<f64>() / values.len() as f64),
            max_pct: values.iter().copied().reduce(f64::max),
        });
        self
    }

    /// The text form: target, result and verdict, one line each, then the detail.
    pub fn text(&self) -> String {
        let i = &self.inputs;
        let w = &self.window;
        let secs =
            |value: Option<f64>| value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.2} s"));
        let mut out = String::new();
        out.push_str(&format!(
            "Target: {:.0} messages/s per deployment (architecture 19), the sink standing in for provider limits\n",
            i.target_rate
        ));
        out.push_str(&format!(
            "Result: {:.1} messages/s over the steady window ({} unique deliveries in {:.1} s)\n",
            w.rate, w.messages, w.seconds
        ));
        out.push_str(&format!(
            "Verdict: {}{}{}\n",
            if self.verdict.met { "met" } else { "missed" },
            if self.verdict.complete {
                ""
            } else {
                " (incomplete: not exactly one delivery per enrolled person)"
            },
            if self.verdict.valid {
                ""
            } else {
                " (INVALID: nothing can be concluded)"
            }
        ));
        out.push_str(&format!(
            "Detail [{}]: {} unique of {} expected ({} arrivals, {} duplicates, {} unmatched); overall {:.1}/s; busiest second {}; delay enrollment to sink p50 {}, p95 {}, p99 {}, max {} ({} matched)\n",
            i.label,
            self.unique_delivered,
            i.expected,
            self.arrivals,
            self.duplicates,
            self.unmatched_arrivals,
            self.overall_rate,
            self.peak_per_second,
            secs(self.delay.p50_s),
            secs(self.delay.p95_s),
            secs(self.delay.p99_s),
            secs(self.delay.max_s),
            self.delay.count,
        ));
        out.push_str(&format!(
            "Clocks: database minus host {:+.3} s before, {:+.3} s after the run; delay uncertainty {:.3} s; {} negative delays, {} beyond the uncertainty\n",
            i.clocks.offset_before_us as f64 / 1e6,
            i.clocks.offset_after_us as f64 / 1e6,
            i.clocks.uncertainty_us as f64 / 1e6,
            self.negative_delays,
            self.clock_violations
        ));
        if let Some(check) = &self.bucket_check {
            out.push_str(&format!(
                "Bucket: held to {:.0}/s; upper bound = rate x (1 + tolerance {:.0}%) + burst slack ({:.0} messages): window rate at most {:.1}/s, busiest second at most {:.1}: {}\n",
                check.rate,
                check.tolerance * 100.0,
                check.burst_slack_messages,
                check.window_rate_limit,
                check.peak_limit,
                if check.within { "within" } else { "EXCEEDED" }
            ));
        }
        if let Some(queue) = &self.queue {
            out.push_str(&format!(
                "Queue (scheduler gauges, steady window, {} samples): max depth {:.0}, max oldest age {:.1} s{}\n",
                queue.samples,
                queue.max_depth,
                queue.max_oldest_s,
                if queue.max_depth > 0.0 {
                    ": a queue stood in the window, so the destination or the workers were the limit"
                } else {
                    ""
                }
            ));
        }
        if let Some(cpu) = &self.db_cpu {
            out.push_str(&format!(
                "Database CPU (steady window, {} samples): mean {}, max {} percent of one core\n",
                cpu.samples,
                cpu.mean_pct.map_or("n/a".to_owned(), |v| format!("{v:.0}")),
                cpu.max_pct.map_or("n/a".to_owned(), |v| format!("{v:.0}"))
            ));
        }
        for problem in &self.verdict.problems {
            out.push_str(&format!("Problem: {problem}\n"));
        }
        out
    }
}

/// Reads the sink log: one `{"t_us":..,"rcpt":".."}` object per line. Bad lines are counted.
pub fn parse_arrivals(text: &str) -> (Vec<Arrival>, u64) {
    let mut arrivals = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let parsed: Option<Arrival> = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| {
                Some(Arrival {
                    at_us: v.get("t_us")?.as_u64()?,
                    rcpt: v.get("rcpt")?.as_str()?.to_owned(),
                })
            });
        match parsed {
            Some(arrival) => arrivals.push(arrival),
            None => bad += 1,
        }
    }
    (arrivals, bad)
}

/// Reads `address,epoch_microseconds` lines (the enrollment export, database clock).
pub fn parse_enrollments(text: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some((address, micros)) = line.rsplit_once(',')
            && let Ok(micros) = micros.trim().parse::<u64>()
        {
            map.insert(address.trim().to_ascii_lowercase(), micros);
        }
    }
    map
}

/// Reads `t_us,depth,oldest_s` lines (the scheduler's destination queue gauges).
pub fn parse_queue(text: &str) -> Vec<(u64, f64, f64)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.trim().split(',');
            Some((
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            ))
        })
        .collect()
}

/// Unix microseconds of `YYYY-MM-DDTHH:MM:SS[.ffffff]Z`.
pub fn parse_iso_us(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut t = clock.split(':');
    let (h, mi, s): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let mut micros: i64 = 0;
    for (n, c) in fraction.chars().take(6).enumerate() {
        micros += i64::from(c.to_digit(10)?) * 10_i64.pow(5 - n as u32);
    }
    // Days from the civil date (proleptic Gregorian).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 86_400 + h * 3600 + mi * 60 + s) * 1_000_000) + micros).ok()
}

/// The `docker stats` samples of one container: `(t_us, cpu percent)`. Lines are the JSON objects
/// the run script writes (`Name`, `CPUPerc` like `123.45%`, and `t`).
pub fn parse_docker_cpu(text: &str, container: &str) -> Vec<(u64, f64)> {
    text.lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("Name")?.as_str()? != container {
                return None;
            }
            let pct = v
                .get("CPUPerc")?
                .as_str()?
                .trim_end_matches('%')
                .parse()
                .ok()?;
            Some((parse_iso_us(v.get("t")?.as_str()?)?, pct))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(at_us: u64, rcpt: &str) -> Arrival {
        Arrival {
            at_us,
            rcpt: rcpt.to_owned(),
        }
    }

    /// 1000 messages, one every 2 ms (500/s) from t = 10 s, to p0..p999, all enrolled at t = 9 s.
    fn fixed() -> (Vec<Arrival>, HashMap<String, u64>) {
        let arrivals = (0..1000)
            .map(|n| arrival(10_000_000 + n * 2000, &format!("p{n}@x.test")))
            .collect();
        let enrolled = (0..1000)
            .map(|n| (format!("p{n}@x.test"), 9_000_000))
            .collect();
        (arrivals, enrolled)
    }

    fn inputs(target: f64) -> Inputs {
        Inputs {
            label: "fixed".into(),
            target_rate: target,
            tolerance: 0.0,
            expected: 1000,
            trim_percent: 10.0,
            clocks: Clocks::default(),
            bucket: None,
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50.0), 50);
        assert_eq!(percentile(&sorted, 95.0), 95);
        assert_eq!(percentile(&sorted, 99.0), 99);
        assert_eq!(percentile(&[1u64, 2, 3, 4], 50.0), 2);
        assert_eq!(percentile(&[1u64, 2, 3, 4], 99.0), 4);
        assert_eq!(percentile::<u64>(&[], 50.0), 0);
    }

    #[test]
    fn the_steady_window_trims_ten_percent_each_side() {
        let (arrivals, _) = fixed();
        let at: Vec<u64> = arrivals.iter().map(|a| a.at_us).collect();
        let window = steady_window(&at, 10.0);
        // Ranks 100 to 899: 799 gaps of 2 ms.
        assert_eq!(window.messages, 799);
        assert_eq!(window.start_us, 10_200_000);
        assert_eq!(window.end_us, 11_798_000);
        assert!((window.seconds - 1.598).abs() < 1e-9);
        assert!((window.rate - 500.0).abs() < 1e-6, "{}", window.rate);
    }

    #[test]
    fn the_busiest_second_counts_arrivals_in_a_sliding_span() {
        let (arrivals, _) = fixed();
        let at: Vec<u64> = arrivals.iter().map(|a| a.at_us).collect();
        assert_eq!(peak_per_second(&at), 500);
        assert_eq!(peak_per_second(&[]), 0);
    }

    #[test]
    fn the_report_states_target_result_and_verdict_and_the_delay_percentiles() {
        let (arrivals, enrolled) = fixed();

        let met = build(inputs(300.0), &arrivals, &enrolled);
        assert!(met.verdict.met && met.verdict.complete && met.verdict.valid);
        // The delay is 1.000 s plus 0 to 1.998 s in 2 ms steps: nearest ranks of 1000 samples.
        assert_eq!(met.delay.count, 1000);
        assert!((met.delay.p50_s.unwrap_or(-1.0) - 1.998).abs() < 1e-9);
        assert!((met.delay.p95_s.unwrap_or(-1.0) - 2.898).abs() < 1e-9);
        assert!((met.delay.p99_s.unwrap_or(-1.0) - 2.978).abs() < 1e-9);
        assert!((met.delay.max_s.unwrap_or(-1.0) - 2.998).abs() < 1e-9);
        let text = met.text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Target: 300 messages/s"));
        assert_eq!(
            lines[1],
            "Result: 500.0 messages/s over the steady window (799 unique deliveries in 1.6 s)"
        );
        assert_eq!(lines[2], "Verdict: met");

        let missed = build(inputs(600.0), &arrivals, &enrolled);
        assert!(!missed.verdict.met);
        assert_eq!(missed.text().lines().nth(2), Some("Verdict: missed"));
    }

    #[test]
    fn a_tolerance_lets_a_rate_limited_run_hold_and_missing_messages_are_flagged() {
        let (arrivals, enrolled) = fixed();
        let mut at_target = inputs(510.0);
        assert!(!build(at_target.clone(), &arrivals, &enrolled).verdict.met);
        at_target.tolerance = 0.03;
        assert!(build(at_target, &arrivals, &enrolled).verdict.met);

        let mut short = inputs(300.0);
        short.expected = 1200;
        let report = build(short, &arrivals, &enrolled);
        assert!(!report.verdict.complete);
        assert!(report.text().contains("incomplete"));
    }

    #[test]
    fn duplicates_and_arrivals_of_other_runs_are_not_counted_and_fail_the_run() {
        let (mut arrivals, enrolled) = fixed();
        // A second delivery to p5, and a late message of an earlier run.
        arrivals.push(arrival(13_000_000, "p5@x.test"));
        arrivals.push(arrival(13_000_100, "old-run@x.test"));
        let report = build(inputs(300.0), &arrivals, &enrolled);
        assert_eq!(report.arrivals, 1002);
        assert_eq!(report.unique_delivered, 1000);
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.unmatched_arrivals, 1);
        assert_eq!(report.delay.count, 1000);
        assert!(!report.verdict.met && !report.verdict.complete);
        assert!(report.text().contains("1 duplicate deliveries"));
    }

    #[test]
    fn the_clock_offset_moves_the_enrollment_and_a_delivery_before_it_is_flagged() {
        let (arrivals, enrolled) = fixed();
        // The database clock runs 5 s ahead of the host: enrollment at db 9 s is host 4 s, so the
        // delay of the first message grows from 1 s to 6 s.
        let mut ahead = inputs(300.0);
        ahead.clocks = Clocks {
            offset_before_us: 5_000_000,
            offset_after_us: 5_000_000,
            uncertainty_us: 100_000,
        };
        let report = build(ahead, &arrivals, &enrolled);
        assert!((report.delay.p50_s.unwrap_or(0.0) - 6.998).abs() < 1e-9);
        assert_eq!(report.clock_violations, 0);

        // Database 3 s behind the host: enrollment at db 9 s is host 12 s, after the first
        // deliveries at 10 s. Those are impossible beyond a 0.1 s uncertainty.
        let mut behind = inputs(300.0);
        behind.clocks = Clocks {
            offset_before_us: -3_000_000,
            offset_after_us: -3_000_000,
            uncertainty_us: 100_000,
        };
        let report = build(behind, &arrivals, &enrolled);
        assert!(report.negative_delays > 0);
        assert!(report.clock_violations > 0);
        assert!(!report.verdict.valid && !report.verdict.met);
        // Inside the uncertainty it is only counted.
        let mut close = inputs(300.0);
        close.clocks = Clocks {
            offset_before_us: -1_050_000,
            offset_after_us: -1_050_000,
            uncertainty_us: 100_000,
        };
        let report = build(close, &arrivals, &enrolled);
        assert!(report.negative_delays > 0);
        assert_eq!(report.clock_violations, 0);
    }

    #[test]
    fn queue_gauges_count_only_samples_inside_the_steady_window() {
        let (arrivals, enrolled) = fixed();
        let report = build(inputs(300.0), &arrivals, &enrolled);
        let samples = parse_queue(
            "9000000,900,30\n10300000,12,0.5\n11000000,7,0.9\n12500000,500,20\nbroken\n",
        );
        assert_eq!(samples.len(), 4);
        let queue = report.with_queue(&samples).queue.unwrap_or_default();
        assert_eq!(queue.samples, 2);
        assert_eq!(queue.max_depth, 12.0);
        assert_eq!(queue.max_oldest_s, 0.9);
    }

    #[test]
    fn a_bucket_limited_run_that_beats_its_bucket_is_not_met() {
        let (arrivals, enrolled) = fixed();
        // 500 messages/s through a 300/s bucket with a burst of 100 cannot be.
        let mut held = inputs(300.0);
        held.tolerance = 0.03;
        held.bucket = Some(Bucket {
            rate: 300.0,
            burst: 100.0,
        });
        let report = build(held.clone(), &arrivals, &enrolled);
        let check = report
            .bucket_check
            .clone()
            .map(|c| (c.within, c.window_rate_limit));
        assert_eq!(check.map(|c| c.0), Some(false));
        assert!(!report.verdict.met);
        assert!(report.text().contains("EXCEEDED"));

        // The same rate through a 1000/s bucket is inside it.
        held.bucket = Some(Bucket {
            rate: 1000.0,
            burst: 100.0,
        });
        assert!(build(held, &arrivals, &enrolled).verdict.met);
    }

    #[test]
    fn no_matched_delivery_is_not_a_result() {
        let (arrivals, _) = fixed();
        let report = build(inputs(300.0), &arrivals, &HashMap::new());
        assert_eq!(report.delay, Percentiles::default());
        assert!(report.delay.p50_s.is_none());
        assert!(!report.verdict.valid && !report.verdict.met);
        let text = report.text();
        assert!(text.contains("p50 n/a"));
        assert!(!text.contains("0.00 s"));
        assert!(text.contains("INVALID"));
        let empty = build(inputs(300.0), &[], &HashMap::new());
        assert!(!empty.verdict.valid);
    }

    #[test]
    fn database_cpu_is_taken_over_the_steady_window_only() {
        let (arrivals, enrolled) = fixed();
        let report = build(inputs(300.0), &arrivals, &enrolled);
        let stats = "\
{\"Name\":\"pg\",\"CPUPerc\":\"400.00%\",\"t\":\"1970-01-01T00:00:09.500000Z\"}
{\"Name\":\"pg\",\"CPUPerc\":\"100.00%\",\"t\":\"1970-01-01T00:00:10.500000Z\"}
{\"Name\":\"pg\",\"CPUPerc\":\"200.00%\",\"t\":\"1970-01-01T00:00:11.500000Z\"}
{\"Name\":\"other\",\"CPUPerc\":\"900.00%\",\"t\":\"1970-01-01T00:00:11.000000Z\"}
{\"Name\":\"pg\",\"CPUPerc\":\"50.00%\",\"t\":\"1970-01-01T00:00:13.000000Z\"}
";
        let cpu = parse_docker_cpu(stats, "pg");
        assert_eq!(cpu.len(), 4);
        let cpu = report.with_db_cpu(&cpu).db_cpu.unwrap_or_default();
        assert_eq!(cpu.samples, 2);
        assert_eq!(cpu.mean_pct, Some(150.0));
        assert_eq!(cpu.max_pct, Some(200.0));
    }

    #[test]
    fn iso_times_parse_to_unix_microseconds() {
        assert_eq!(parse_iso_us("1970-01-01T00:00:09.500000Z"), Some(9_500_000));
        assert_eq!(
            parse_iso_us("2026-10-25T12:00:00.123456Z"),
            Some(1_792_929_600_123_456)
        );
        assert_eq!(parse_iso_us("nonsense"), None);
    }

    #[test]
    fn the_log_and_the_enrollment_export_parse() {
        let (arrivals, bad) = parse_arrivals(
            "{\"t_us\":5,\"rcpt\":\"A@x.test\"}\nnot json\n\n{\"t_us\":7,\"rcpt\":\"b@x.test\"}\n",
        );
        assert_eq!(arrivals.len(), 2);
        assert_eq!(bad, 1);
        let enrolled = parse_enrollments("A@x.test,100\nb@x.test, 200\nbroken\n");
        assert_eq!(enrolled.get("a@x.test"), Some(&100));
        assert_eq!(enrolled.get("b@x.test"), Some(&200));
        assert_eq!(enrolled.len(), 2);
    }
}
