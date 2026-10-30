//! `loadgen`: a deterministic ingestion load generator for the zirv-flow benchmarks. It talks to
//! a running instance over HTTP only (plain `http://`; the benchmarks run on localhost) and
//! writes an ingestion JSON report or debugger receipts for the latency SQL report.

mod pacing;
mod payload;
mod report;
mod runner;
mod sse;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{CommandFactory, FromArgMatches, Parser};

use report::{Latency, PhaseReport, Report, StatusCounts, achieved_rate};
use runner::{Config, Mode, Phase, PhaseOutcome, run_phase};

/// Latency samples of a whole run, in microseconds.
#[derive(Default)]
struct Samples {
    schedule_us: Vec<u64>,
    service_us: Vec<u64>,
    lag_us: Vec<u64>,
}

const LATENCY_NOTE: &str = "One sample per request at its final outcome of any status (202, 4xx, 5xx, timeout or transport error, 429 retries exhausted). latency_from_schedule runs from the request's scheduled due time to its final response, retry waits and dispatch lag included: judge API latency targets on it. dispatch_lag is the final send minus the scheduled time and service_latency the final attempt's send to response, so latency_from_schedule is about dispatch_lag plus service_latency.";

/// The largest batch the server accepts by default (`ingest.max_batch_items`).
const MAX_BATCH: u32 = 500;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Base URL of a `serve` process, such as http://localhost:8080.
    #[arg(long)]
    url: String,
    /// File holding the `zfk_...` ingest API key.
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long, value_enum)]
    mode: Mode,
    /// Target items per second in the base phase.
    #[arg(long, default_value_t = 0)]
    rate: u32,
    /// Seconds of the base phase; 0 skips it.
    #[arg(long, default_value_t = 0.0)]
    duration: f64,
    /// Target items per second in the burst phase, which follows the base phase.
    #[arg(long, default_value_t = 0)]
    burst_rate: u32,
    /// Seconds of the burst phase; 0 skips it.
    #[arg(long, default_value_t = 0.0)]
    burst_duration: f64,
    /// Size of the pool of people the items are spread over.
    #[arg(long, default_value_t = 100_000)]
    people: u64,
    /// Items per request in batch mode (1 to 500); ignored by identify and track.
    #[arg(long, default_value_t = 100)]
    batch_size: u32,
    /// Safety cap on requests in flight. Pacing never waits on it unless it is reached, and then
    /// the request goes out as soon as a slot frees and counts as `late_dispatch`.
    #[arg(long, alias = "concurrency", default_value_t = 10_000)]
    max_in_flight: u32,
    /// Seed of the payloads. The same seed and run id give the same items.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Prefix of every messageId. Defaults to the start time, so a rerun with the same seed does
    /// not collide with the earlier run's deduplication records; pass one to repeat a run exactly.
    #[arg(long)]
    run_id: Option<String>,
    /// Write the JSON report here instead of standard output.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// reqwest is built without a bundled TLS provider; installing the one the tree already uses
/// keeps client construction from panicking, even though only `http://` is used.
pub(crate) fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The trimmed, non-empty API key held in `path`.
pub(crate) fn read_key(path: &std::path::Path) -> Result<String, String> {
    let key = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read --key-file: {error}"))?
        .trim()
        .to_owned();
    if key.is_empty() {
        return Err("--key-file is empty".into());
    }
    Ok(key)
}

fn validate(args: &Args) -> Result<(), String> {
    let phase_ok = |rate: u32, seconds: f64| seconds == 0.0 || (rate > 0 && seconds > 0.0);
    if !(args.duration.is_finite() && args.burst_duration.is_finite()) {
        return Err("durations must be finite".into());
    }
    if !phase_ok(args.rate, args.duration) {
        return Err("--duration needs a positive --rate".into());
    }
    if !phase_ok(args.burst_rate, args.burst_duration) {
        return Err("--burst-duration needs a positive --burst-rate".into());
    }
    if args.duration == 0.0 && args.burst_duration == 0.0 {
        return Err("give --rate with --duration, or --burst-rate with --burst-duration".into());
    }
    if !(1..=MAX_BATCH).contains(&args.batch_size) {
        return Err(format!("--batch-size must be 1 to {MAX_BATCH}"));
    }
    if args.max_in_flight == 0 || args.people == 0 {
        return Err("--max-in-flight and --people must be at least 1".into());
    }
    Ok(())
}

fn merge(phases: &[PhaseReport], samples: Samples) -> PhaseReport {
    let mut total = PhaseReport {
        name: "total".to_owned(),
        ..PhaseReport::default()
    };
    let mut status = StatusCounts::default();
    for phase in phases {
        total.planned_duration_s += phase.planned_duration_s;
        total.elapsed_s += phase.elapsed_s;
        total.requests += phase.requests;
        total.items_offered += phase.items_offered;
        total.items_accepted += phase.items_accepted;
        total.retries_after_429 += phase.retries_after_429;
        total.late_dispatch += phase.late_dispatch;
        total.abandoned_requests += phase.abandoned_requests;
        total.gave_up_after_retries += phase.gave_up_after_retries;
        status.add(&phase.status);
    }
    total.target_rate = phases
        .iter()
        .map(|phase| phase.target_rate)
        .max()
        .unwrap_or(0);
    total.achieved_rate = achieved_rate(total.items_accepted, total.elapsed_s);
    total.status = status;
    total.latency_from_schedule = Latency::from_micros(samples.schedule_us);
    total.service_latency = Latency::from_micros(samples.service_us);
    total.dispatch_lag = Latency::from_micros(samples.lag_us);
    total
}

async fn run(args: Args) -> Result<(), String> {
    validate(&args)?;
    let key = crate::read_key(&args.key_file)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    let run_id = args
        .run_id
        .clone()
        .unwrap_or_else(|| format!("{:x}", started.as_millis()));
    init_tls();
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.max_in_flight.min(1024) as usize)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let config = Arc::new(Config {
        url: args.url.trim_end_matches('/').to_owned(),
        key,
        mode: args.mode,
        seed: args.seed,
        run_id: run_id.clone(),
        people: args.people,
        batch_size: args.batch_size,
        max_in_flight: args.max_in_flight,
    });

    let mut phases = Vec::new();
    let mut samples = Samples::default();
    let mut seq = 0;
    for phase in [
        Phase {
            name: "base",
            rate: args.rate,
            duration_s: args.duration,
        },
        Phase {
            name: "burst",
            rate: args.burst_rate,
            duration_s: args.burst_duration,
        },
    ] {
        if phase.duration_s == 0.0 {
            continue;
        }
        let PhaseOutcome {
            report,
            schedule_us,
            service_us,
            lag_us,
            next_seq: next,
        } = run_phase(&client, &config, &phase, seq).await;
        eprintln!(
            "{}: {:.0} items/s achieved of {} target, {} accepted, p99 {:.1} ms from schedule, {:.1} ms service, {} late",
            report.name,
            report.achieved_rate,
            report.target_rate,
            report.items_accepted,
            report.latency_from_schedule.p99_ms,
            report.service_latency.p99_ms,
            report.late_dispatch
        );
        seq = next;
        samples.schedule_us.extend(schedule_us);
        samples.service_us.extend(service_us);
        samples.lag_us.extend(lag_us);
        phases.push(report);
    }

    let report = Report {
        tool: "zirv-flow-loadgen",
        mode: args.mode.name().to_owned(),
        seed: args.seed,
        run_id,
        started_unix_s: started.as_secs(),
        batch_size: if args.mode == Mode::Batch {
            args.batch_size
        } else {
            1
        },
        max_in_flight: args.max_in_flight,
        people: args.people,
        latency_note: LATENCY_NOTE,
        total: merge(&phases, samples),
        phases,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    match &args.out {
        Some(path) => std::fs::write(path, json + "\n")
            .map_err(|error| format!("cannot write --out: {error}"))?,
        None => println!("{json}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let matches = command().get_matches();
    let result = match matches.subcommand() {
        Some(("debugger", matches)) => {
            sse::run(sse::Args::from_arg_matches(matches).unwrap_or_else(|error| error.exit()))
                .await
        }
        _ => run(Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("loadgen: {message}");
            ExitCode::from(2)
        }
    }
}

fn command() -> clap::Command {
    Args::command()
        .subcommand(sse::Args::command().name("debugger"))
        .subcommand_negates_reqs(true)
        .args_conflicts_with_subcommands(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debugger_subcommand_preserves_the_ingestion_cli() {
        let ingest = command()
            .try_get_matches_from([
                "loadgen",
                "--url",
                "http://localhost:8080",
                "--key-file",
                "ingest.key",
                "--mode",
                "track",
                "--rate",
                "1",
                "--duration",
                "1",
            ])
            .unwrap();
        assert_eq!(Args::from_arg_matches(&ingest).unwrap().mode, Mode::Track);
        let debugger = command()
            .try_get_matches_from([
                "loadgen",
                "debugger",
                "--url",
                "http://localhost:8080",
                "--key-file",
                "read.key",
                "--workspace",
                "workspace",
                "--clients",
                "20",
                "--duration",
                "60",
                "--out",
                "receive.jsonl",
            ])
            .unwrap();
        let (_, matches) = debugger.subcommand().unwrap();
        assert_eq!(sse::Args::from_arg_matches(matches).unwrap().clients, 20);
        assert!(
            command()
                .try_get_matches_from(["loadgen", "debugger"])
                .is_err()
        );
    }
}
