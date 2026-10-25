//! The send-throughput report: the rate at the sink over a steady window, the delay from
//! enrollment to the sink as percentiles, and one line each for target, result and verdict.
//! Percentiles are exact (nearest rank over every sample).

use hashbrown::HashMap;
use serde::Serialize;

use crate::sink::Arrival;

/// Nearest-rank percentile of an ascending slice.
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50_s: f64,
    pub p95_s: f64,
    pub p99_s: f64,
    pub max_s: f64,
}

impl Percentiles {
    /// From microseconds, in any order.
    pub fn from_micros(mut micros: Vec<u64>) -> Self {
        micros.sort_unstable();
        let s = |value: u64| value as f64 / 1e6;
        Self {
            count: micros.len() as u64,
            p50_s: s(percentile(&micros, 50.0)),
            p95_s: s(percentile(&micros, 95.0)),
            p99_s: s(percentile(&micros, 99.0)),
            max_s: s(micros.last().copied().unwrap_or(0)),
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

#[derive(Clone, Debug, Serialize)]
pub struct Inputs {
    pub label: String,
    pub target_rate: f64,
    /// Fraction under the target that still counts as met, such as 0.03 for a run limited to
    /// the target by the destination's own bucket.
    pub tolerance: f64,
    pub expected: u64,
    pub trim_percent: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub met: bool,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub inputs: Inputs,
    pub accepted: u64,
    pub overall_rate: f64,
    pub peak_per_second: u64,
    pub window: Window,
    pub delay: Percentiles,
    /// Arrivals with no enrollment row, left out of `delay`.
    pub unmatched_arrivals: u64,
    pub verdict: Verdict,
}

/// `enrolled`: recipient (lower case) to enrollment time in unix microseconds.
pub fn build(inputs: Inputs, arrivals: &[Arrival], enrolled: &HashMap<String, u64>) -> Report {
    let mut at: Vec<u64> = arrivals.iter().map(|a| a.at_us).collect();
    at.sort_unstable();
    let window = steady_window(&at, inputs.trim_percent);
    let total_s = match (at.first(), at.last()) {
        (Some(first), Some(last)) if last > first => (last - first) as f64 / 1e6,
        _ => 0.0,
    };
    let accepted = at.len() as u64;
    let mut delays = Vec::with_capacity(arrivals.len());
    let mut unmatched = 0;
    for arrival in arrivals {
        match enrolled.get(&arrival.rcpt.to_ascii_lowercase()) {
            Some(start) => delays.push(arrival.at_us.saturating_sub(*start)),
            None => unmatched += 1,
        }
    }
    let met = window.rate >= inputs.target_rate * (1.0 - inputs.tolerance) && window.messages > 0;
    let complete = accepted >= inputs.expected;
    Report {
        overall_rate: if total_s > 0.0 {
            accepted as f64 / total_s
        } else {
            0.0
        },
        peak_per_second: peak_per_second(&at),
        accepted,
        window,
        delay: Percentiles::from_micros(delays),
        unmatched_arrivals: unmatched,
        verdict: Verdict { met, complete },
        inputs,
    }
}

impl Report {
    /// The text form: target, result and verdict, one line each, then the detail.
    pub fn text(&self) -> String {
        let i = &self.inputs;
        let w = &self.window;
        let mut out = String::new();
        out.push_str(&format!(
            "Target: {:.0} messages/s per deployment (architecture 19), the sink standing in for provider limits\n",
            i.target_rate
        ));
        out.push_str(&format!(
            "Result: {:.1} messages/s over the steady window ({} messages in {:.1} s)\n",
            w.rate, w.messages, w.seconds
        ));
        out.push_str(&format!(
            "Verdict: {}{}\n",
            if self.verdict.met { "met" } else { "missed" },
            if self.verdict.complete {
                ""
            } else {
                " (incomplete: fewer messages than enrolled people)"
            }
        ));
        out.push_str(&format!(
            "Detail [{}]: {} of {} expected accepted; overall {:.1}/s; busiest second {}; delay enrollment to sink p50 {:.2} s, p95 {:.2} s, p99 {:.2} s, max {:.2} s ({} matched, {} unmatched)\n",
            i.label,
            self.accepted,
            i.expected,
            self.overall_rate,
            self.peak_per_second,
            self.delay.p50_s,
            self.delay.p95_s,
            self.delay.p99_s,
            self.delay.max_s,
            self.delay.count,
            self.unmatched_arrivals
        ));
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

/// Reads `address,epoch_microseconds` lines (the enrollment export).
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
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50.0), 50);
        assert_eq!(percentile(&sorted, 95.0), 95);
        assert_eq!(percentile(&sorted, 99.0), 99);
        assert_eq!(percentile(&[1, 2, 3, 4], 50.0), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 99.0), 4);
        assert_eq!(percentile(&[], 50.0), 0);
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
        assert!(met.verdict.met && met.verdict.complete);
        // The delay is 1.000 s plus 0 to 1.998 s in 2 ms steps: nearest ranks of 1000 samples.
        assert_eq!(met.delay.count, 1000);
        assert!(
            (met.delay.p50_s - 1.998).abs() < 1e-9,
            "{}",
            met.delay.p50_s
        );
        assert!(
            (met.delay.p95_s - 2.898).abs() < 1e-9,
            "{}",
            met.delay.p95_s
        );
        assert!(
            (met.delay.p99_s - 2.978).abs() < 1e-9,
            "{}",
            met.delay.p99_s
        );
        assert!((met.delay.max_s - 2.998).abs() < 1e-9);
        let text = met.text();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("Target: 300 messages/s"));
        assert_eq!(
            lines[1],
            "Result: 500.0 messages/s over the steady window (799 messages in 1.6 s)"
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
    fn arrivals_without_an_enrollment_are_left_out_of_the_delay() {
        let (arrivals, mut enrolled) = fixed();
        enrolled.remove("p0@x.test");
        let report = build(inputs(300.0), &arrivals, &enrolled);
        assert_eq!(report.unmatched_arrivals, 1);
        assert_eq!(report.delay.count, 999);
    }

    #[test]
    fn too_few_messages_have_no_window() {
        assert_eq!(steady_window(&[1, 2], 10.0), Window::default());
        let report = build(inputs(300.0), &[], &HashMap::new());
        assert!(!report.verdict.met);
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
