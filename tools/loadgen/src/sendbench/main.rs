//! `sendbench`: the tooling of the send-throughput benchmark (#233, architecture 19).
//!
//! - `sink`: an SMTP server that accepts every message and logs when and for whom, standing in
//!   for a provider. It offers `STARTTLS` with a generated authority (the released image refuses
//!   plaintext) and writes the authority's PEM for the destination's `ca_certificate`.
//! - `people`: identifies N people with an email address through the ingest batch endpoint.
//! - `report`: joins the sink log with the enrollment times and the workers' metrics, and prints
//!   target, result and verdict.

mod people;
mod prom;
mod report;
mod sink;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// reqwest is built without a bundled TLS provider; installing the one the tree already uses
/// keeps client construction from panicking, even though only `http://` is used.
pub(crate) fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the SMTP sink until interrupted.
    Sink {
        /// Where to listen. Containers reach it through the host, so use 0.0.0.0.
        #[arg(long, default_value = "0.0.0.0:2525")]
        listen: SocketAddr,
        /// `starttls`, `implicit` or `plain`.
        #[arg(long, default_value = "starttls")]
        tls: String,
        /// A host name the certificate is valid for (repeatable).
        #[arg(long = "name")]
        names: Vec<String>,
        /// Directory that receives `ca.pem`.
        #[arg(long)]
        dir: PathBuf,
        /// The arrivals log (one JSON line per accepted message), appended to.
        #[arg(long)]
        log: PathBuf,
        /// Milliseconds added to every final reply: a provider that is not instant.
        #[arg(long, default_value_t = 0)]
        response_delay_ms: u64,
    },
    /// Identify people `prefix-0 .. prefix-(count-1)` through the ingest batch endpoint.
    People {
        #[arg(long)]
        url: String,
        /// File holding the `zfk_...` ingest API key.
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        count: u64,
        #[arg(long, default_value = "sb")]
        prefix: String,
        /// The address domain, such as `bench.example`.
        #[arg(long)]
        domain: String,
        #[arg(long, default_value_t = 500)]
        batch_size: u64,
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
    /// Compute the report of one run.
    Report {
        /// The sink's arrivals log.
        #[arg(long)]
        arrivals: PathBuf,
        /// Lines of `address,epoch_microseconds`: when each person was enrolled.
        #[arg(long)]
        enrollments: PathBuf,
        /// A worker's `/metrics` scrape (repeatable, one per worker process).
        #[arg(long = "metrics")]
        metrics: Vec<PathBuf>,
        #[arg(long, default_value = "run")]
        label: String,
        /// Messages per second to compare with.
        #[arg(long, default_value_t = 300.0)]
        target: f64,
        /// The share under the target that still counts as met (0.03 for a run held to the
        /// target by the destination's own bucket).
        #[arg(long, default_value_t = 0.0)]
        tolerance: f64,
        /// People enrolled, so a run that lost messages says so.
        #[arg(long)]
        expected: u64,
        /// Percent of messages left out at each end of the steady window.
        #[arg(long, default_value_t = 10.0)]
        trim: f64,
        /// Database clock minus host clock before the run, microseconds.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        offset_before_us: i64,
        /// The same after the run.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        offset_after_us: i64,
        /// The wider of the two host-time brackets around the clock queries, microseconds.
        #[arg(long, default_value_t = 0)]
        uncertainty_us: u64,
        /// The destination's rate when its bucket is what holds the run to the target.
        #[arg(long)]
        bucket_rate: Option<f64>,
        /// The destination's burst (used with --bucket-rate).
        #[arg(long, default_value_t = 0.0)]
        bucket_burst: f64,
        /// The scheduler queue samples: lines of `t_us,depth,oldest_s`.
        #[arg(long)]
        queue: Option<PathBuf>,
        /// The `docker stats` samples (JSON lines with `Name`, `CPUPerc` and `t`).
        #[arg(long)]
        docker_stats: Option<PathBuf>,
        /// The container name whose CPU is the database's.
        #[arg(long, default_value = "")]
        db_container: String,
        /// Write the JSON report here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn read(path: &PathBuf, what: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("cannot read {what}: {error}"))
}

async fn run_sink(command: Command) -> Result<(), String> {
    let Command::Sink {
        listen,
        tls,
        names,
        dir,
        log,
        response_delay_ms,
    } = command
    else {
        return Err("not the sink command".into());
    };
    let tls = sink::TlsMode::parse(&tls).ok_or("--tls is starttls, implicit or plain")?;
    let sink = sink::Sink::start(
        listen,
        sink::SinkConfig {
            tls,
            names,
            response_delay: Duration::from_millis(response_delay_ms),
            log: Some(log),
            keep: false,
        },
    )
    .await?;
    sink::write_ca(&dir, sink.ca_pem())?;
    println!("port={}", sink.port());
    let mut last = 0;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            () = tokio::time::sleep(Duration::from_secs(10)) => {
                let now = sink.accepted();
                eprintln!("sink: {now} accepted, {} per second, {} connections", (now - last) / 10, sink.connections());
                last = now;
            }
        }
    }
    eprintln!("sink: {} accepted in total", sink.accepted());
    Ok(())
}

async fn run_people(command: Command) -> Result<(), String> {
    let Command::People {
        url,
        key_file,
        count,
        prefix,
        domain,
        batch_size,
        concurrency,
    } = command
    else {
        return Err("not the people command".into());
    };
    let key = read(&key_file, "--key-file")?.trim().to_owned();
    if key.is_empty() {
        return Err("--key-file is empty".into());
    }
    let accepted = people::run(people::Seed {
        url,
        key,
        prefix,
        domain,
        count,
        batch_size,
        concurrency,
    })
    .await?;
    println!("accepted={accepted}");
    Ok(())
}

fn run_report(command: Command) -> Result<(), String> {
    let Command::Report {
        arrivals,
        enrollments,
        metrics,
        label,
        target,
        tolerance,
        expected,
        trim,
        offset_before_us,
        offset_after_us,
        uncertainty_us,
        bucket_rate,
        bucket_burst,
        queue,
        docker_stats,
        db_container,
        out,
    } = command
    else {
        return Err("not the report command".into());
    };
    let (arrivals, bad) = report::parse_arrivals(&read(&arrivals, "--arrivals")?);
    let enrolled = report::parse_enrollments(&read(&enrollments, "--enrollments")?);
    let built = report::build(
        report::Inputs {
            label,
            target_rate: target,
            tolerance,
            expected,
            trim_percent: trim,
            clocks: report::Clocks {
                offset_before_us,
                offset_after_us,
                uncertainty_us,
            },
            bucket: bucket_rate.map(|rate| report::Bucket {
                rate,
                burst: bucket_burst,
            }),
        },
        &arrivals,
        &enrolled,
    );
    let mut built = built;
    if let Some(path) = &queue {
        built = built.with_queue(&report::parse_queue(&read(path, "--queue")?));
    }
    if let Some(path) = &docker_stats {
        built = built.with_db_cpu(&report::parse_docker_cpu(
            &read(path, "--docker-stats")?,
            &db_container,
        ));
    }
    let mut samples = Vec::new();
    for path in &metrics {
        samples.extend(prom::parse(&read(path, "--metrics")?));
    }
    print!("{}", built.text());
    if bad > 0 {
        eprintln!("sendbench: {bad} unreadable lines in the arrivals log");
    }
    if let Some(path) = out {
        let mut json = serde_json::to_value(&built).map_err(|error| error.to_string())?;
        json["metrics"] = prom::summarize(&samples);
        json["unreadable_arrival_lines"] = bad.into();
        let text = serde_json::to_string_pretty(&json).map_err(|error| error.to_string())?;
        std::fs::write(path, text + "\n")
            .map_err(|error| format!("cannot write --out: {error}"))?;
    }
    if !built.verdict.valid {
        return Err(format!(
            "the run is invalid: {}",
            built.verdict.problems.join("; ")
        ));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = Args::parse().command;
    let result = match command {
        Command::Sink { .. } => run_sink(command).await,
        Command::People { .. } => run_people(command).await,
        Command::Report { .. } => run_report(command),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("sendbench: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::HashMap;

    use super::*;

    /// The smoke run of the scenario's sink side, with no database and no network beyond
    /// loopback: 200 messages go through `STARTTLS` into an in-process sink, and the report
    /// generator computes the figures from what the sink recorded.
    #[tokio::test]
    async fn smoke_two_hundred_messages_reach_the_sink_and_the_report_counts_them() {
        let started = sink::Sink::start(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            sink::tests::test_config(sink::TlsMode::StartTls),
        )
        .await;
        assert!(started.is_ok());
        let Ok(sink) = started else { return };
        let enrolled_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_micros() as u64);

        sink::tests::send_many_starttls(&sink, 200).await;

        let enrolled: HashMap<String, u64> = (0..200)
            .map(|n| (format!("person-{n}@example.com"), enrolled_at))
            .collect();
        let report = report::build(
            report::Inputs {
                label: "smoke".into(),
                target_rate: 1.0,
                tolerance: 0.0,
                expected: 200,
                trim_percent: 10.0,
                clocks: report::Clocks::default(),
                bucket: None,
            },
            &sink.arrivals(),
            &enrolled,
        );
        assert_eq!(report.unique_delivered, 200);
        assert!(report.verdict.complete);
        assert_eq!(report.delay.count, 200);
        assert_eq!(report.unmatched_arrivals, 0);
        assert!(report.window.messages > 0 && report.window.rate > 0.0);
        assert!(report.verdict.met, "{}", report.text());
    }
}
