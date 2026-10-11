//! `loadgen`: a deterministic ingestion load generator for the zirv-flow benchmarks. It talks to
//! a running instance over HTTP only (plain `http://`; the benchmarks run on localhost) and
//! writes a JSON report: achieved rate, status counts and API latency percentiles.

mod pacing;
mod payload;
mod report;
mod runner;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;

use report::{Latency, PhaseReport, Report, StatusCounts, achieved_rate};
use runner::{Config, Mode, Phase, PhaseOutcome, run_phase};

/// Latency samples of a whole run, in microseconds.
#[derive(Default)]
struct Samples {
    schedule_us: Vec<u64>,
    service_us: Vec<u64>,
    lag_us: Vec<u64>,
}

const LATENCY_NOTE: &str = "latency_from_schedule runs from each request's scheduled due time to its final \nresponse, retry waits and dispatch lag included: judge API latency targets on it. service_latency \nis the send-to-response time of the last attempt only. Both cover 202 responses.";

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
    let key = std::fs::read_to_string(&args.key_file)
        .map_err(|error| format!("cannot read --key-file: {error}"))?
        .trim()
        .to_owned();
    if key.is_empty() {
        return Err("--key-file is empty".into());
    }
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
    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("loadgen: {message}");
            ExitCode::from(2)
        }
    }
}
