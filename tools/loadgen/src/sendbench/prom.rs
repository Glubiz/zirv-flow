//! Just enough of the Prometheus text format to summarize the metrics a worker exposes at
//! `/metrics`: counters, gauges and bucketed histograms. Samples of several processes are
//! concatenated and summed, which is right for counters and histogram buckets.

use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub name: String,
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn matches(&self, filter: &[(&str, &str)]) -> bool {
        filter
            .iter()
            .all(|(key, value)| self.label(key) == Some(*value))
    }
}

/// Parses the exposition text; comments and unreadable lines are skipped.
pub fn parse(text: &str) -> Vec<Sample> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (head, value) = line.rsplit_once(' ')?;
    let value: f64 = match value {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        other => other.parse().ok()?,
    };
    let (name, labels) = match head.split_once('{') {
        Some((name, rest)) => (name, parse_labels(rest.strip_suffix('}')?)?),
        None => (head, Vec::new()),
    };
    Some(Sample {
        name: name.to_owned(),
        labels,
        value,
    })
}

fn parse_labels(text: &str) -> Option<Vec<(String, String)>> {
    let mut labels = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let (key, after) = rest.split_once("=\"")?;
        let end = after.find('"')?;
        labels.push((
            key.trim_start_matches(',').to_owned(),
            after[..end].to_owned(),
        ));
        rest = after[end + 1..].trim_start_matches(',');
    }
    Some(labels)
}

/// A histogram summed over every sample set that matches: cumulative buckets by upper bound.
#[derive(Debug, Default, PartialEq)]
pub struct Histogram {
    pub buckets: Vec<(f64, f64)>,
    pub sum: f64,
    pub count: f64,
}

pub fn histogram(samples: &[Sample], base: &str, filter: &[(&str, &str)]) -> Histogram {
    let bucket_name = format!("{base}_bucket");
    let sum_name = format!("{base}_sum");
    let count_name = format!("{base}_count");
    let mut out = Histogram::default();
    for sample in samples.iter().filter(|s| s.matches(filter)) {
        if sample.name == bucket_name {
            let Some(le) = sample.label("le") else {
                continue;
            };
            let bound = if le == "+Inf" {
                f64::INFINITY
            } else if let Ok(bound) = le.parse::<f64>() {
                bound
            } else {
                continue;
            };
            match out.buckets.iter_mut().find(|(b, _)| *b == bound) {
                Some((_, total)) => *total += sample.value,
                None => out.buckets.push((bound, sample.value)),
            }
        } else if sample.name == sum_name {
            out.sum += sample.value;
        } else if sample.name == count_name {
            out.count += sample.value;
        }
    }
    out.buckets.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

impl Histogram {
    /// The `q` quantile (0 to 1) as Prometheus estimates it: linear inside the bucket. A value
    /// past the last finite bound reports that bound. `None` with no observations.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.buckets.last()?.1;
        if total <= 0.0 {
            return None;
        }
        let rank = q * total;
        let mut lower_bound = 0.0;
        let mut lower_count = 0.0;
        for (bound, count) in &self.buckets {
            if *count >= rank {
                if bound.is_infinite() {
                    return Some(lower_bound);
                }
                let span = count - lower_count;
                if span <= 0.0 {
                    return Some(*bound);
                }
                return Some(lower_bound + (bound - lower_bound) * (rank - lower_count) / span);
            }
            lower_bound = *bound;
            lower_count = *count;
        }
        Some(lower_bound)
    }

    fn summary(&self) -> Value {
        json!({
            "count": self.count,
            "mean_s": if self.count > 0.0 { Some(self.sum / self.count) } else { None },
            "p50_s": self.quantile(0.50),
            "p95_s": self.quantile(0.95),
            "p99_s": self.quantile(0.99),
            "note": "quantiles are estimated from the exposed buckets",
        })
    }
}

/// The sum of a counter or gauge over matching samples.
pub fn total(samples: &[Sample], name: &str, filter: &[(&str, &str)]) -> f64 {
    samples
        .iter()
        .filter(|s| s.name == name && s.matches(filter))
        .map(|s| s.value)
        .sum()
}

/// What the send benchmark reads from the workers: claim lag of `delivery.send`, the render
/// pool's wait and restarts, and the destination counters and gauges. There is no dedicated
/// rate-bucket wait metric in the product; the bucket shows as the destination queue and, in the
/// duration of `delivery.send`, as deferrals.
pub fn summarize(samples: &[Sample]) -> Value {
    let send = [("kind", "delivery.send")];
    let mut outcomes = serde_json::Map::new();
    for sample in samples
        .iter()
        .filter(|s| s.name == "zirv_flow_destination_outcomes_total")
    {
        let label = sample
            .labels
            .iter()
            .filter(|(key, _)| key != "workspace" && key != "destination")
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let entry = outcomes.entry(label).or_insert(json!(0.0));
        *entry = json!(entry.as_f64().unwrap_or(0.0) + sample.value);
    }
    json!({
        "delivery_send_claim_lag": histogram(samples, "zirv_flow_job_claim_lag_seconds", &send).summary(),
        "delivery_send_duration": histogram(samples, "zirv_flow_job_duration_seconds", &send).summary(),
        "render_wait": histogram(samples, "zirv_flow_render_wait_seconds", &[]).summary(),
        "render_duration": histogram(samples, "zirv_flow_render_duration_seconds", &[]).summary(),
        "render_restarts_total": total(samples, "zirv_flow_render_restarts_total", &[]),
        "render_failures_total": total(samples, "zirv_flow_render_failures_total", &[]),
        "destination_attempts_total": total(samples, "zirv_flow_destination_attempts_total", &[]),
        "destination_outcomes_total": outcomes,
        "destination_queue_depth": total(samples, "zirv_flow_destination_queue_depth", &[]),
        "destination_queue_oldest_seconds": samples
            .iter()
            .filter(|s| s.name == "zirv_flow_destination_queue_oldest_seconds")
            .map(|s| s.value)
            .fold(0.0, f64::max),
        "attempt_record_failures_total": total(samples, "zirv_flow_delivery_attempt_record_failures_total", &[]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER_A: &str = "\
# TYPE zirv_flow_job_claim_lag_seconds histogram
zirv_flow_job_claim_lag_seconds_bucket{kind=\"delivery.send\",le=\"0.1\"} 50
zirv_flow_job_claim_lag_seconds_bucket{kind=\"delivery.send\",le=\"1\"} 90
zirv_flow_job_claim_lag_seconds_bucket{kind=\"delivery.send\",le=\"+Inf\"} 100
zirv_flow_job_claim_lag_seconds_sum{kind=\"delivery.send\"} 40
zirv_flow_job_claim_lag_seconds_count{kind=\"delivery.send\"} 100
zirv_flow_job_claim_lag_seconds_bucket{kind=\"flow.step\",le=\"+Inf\"} 7
zirv_flow_render_restarts_total 2
zirv_flow_destination_queue_depth{workspace=\"w\",destination=\"d\"} 12
";

    #[test]
    fn lines_parse_with_and_without_labels() {
        let samples = parse(WORKER_A);
        assert_eq!(samples.len(), 8);
        assert_eq!(samples[0].name, "zirv_flow_job_claim_lag_seconds_bucket");
        assert_eq!(
            samples[0].labels,
            [
                ("kind".to_owned(), "delivery.send".to_owned()),
                ("le".to_owned(), "0.1".to_owned())
            ]
        );
        assert_eq!(samples[6].value, 2.0);
        assert!(samples[6].labels.is_empty());
        assert_eq!(parse("garbage line\n# c\n").len(), 0);
    }

    #[test]
    fn quantiles_interpolate_inside_a_bucket_and_filter_by_label() {
        let samples = parse(WORKER_A);
        let hist = histogram(
            &samples,
            "zirv_flow_job_claim_lag_seconds",
            &[("kind", "delivery.send")],
        );
        assert_eq!(hist.count, 100.0);
        assert_eq!(hist.sum, 40.0);
        // Rank 25 of 50 in the first bucket (0, 0.1].
        assert!((hist.quantile(0.25).unwrap_or(-1.0) - 0.05).abs() < 1e-9);
        // Rank 70: 20 of 40 into (0.1, 1].
        assert!((hist.quantile(0.70).unwrap_or(-1.0) - 0.55).abs() < 1e-9);
        // Past the last finite bound: that bound.
        assert_eq!(hist.quantile(0.99), Some(1.0));
        assert_eq!(Histogram::default().quantile(0.5), None);
    }

    #[test]
    fn several_workers_add_up() {
        let mut samples = parse(WORKER_A);
        samples.extend(parse(WORKER_A));
        assert_eq!(total(&samples, "zirv_flow_render_restarts_total", &[]), 4.0);
        let hist = histogram(
            &samples,
            "zirv_flow_job_claim_lag_seconds",
            &[("kind", "delivery.send")],
        );
        assert_eq!(hist.count, 200.0);
        assert!((hist.quantile(0.25).unwrap_or(-1.0) - 0.05).abs() < 1e-9);
        let summary = summarize(&samples);
        assert_eq!(summary["render_restarts_total"], 4.0);
        assert_eq!(summary["destination_queue_depth"], 24.0);
        assert_eq!(summary["delivery_send_claim_lag"]["count"], 200.0);
    }
}
