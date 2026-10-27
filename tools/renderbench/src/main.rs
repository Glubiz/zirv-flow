//! `renderbench`: the render benchmark of architecture section 19 (issue #317). It renders a
//! typical block email (about 20 blocks, a theme, a snippet, an asset image, pre-evaluated
//! conditions) through the real render pool, with every child busy, and times each call end to
//! end; then it renders the same message in-process to show what the host boundary costs. The
//! children are this same executable run as `render-host`. It touches no database.

mod bench;
mod stats;
mod workload;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use serde_json::{Value, json};
use zirv_flow_render::ChildCommand;

use bench::{Run, production_pool};
use stats::{Summary, millis};
use workload::Workload;

/// The section 19 target: p95 under this many milliseconds per message.
const TARGET_P95_MS: f64 = 50.0;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Renders thrown away before each run.
    #[arg(long, default_value_t = 200)]
    warmup: usize,
    /// Renders measured in each run.
    #[arg(long, default_value_t = 5000)]
    count: usize,
    /// Pool sizes to run, comma separated. Defaults to 1 and the production `render.pool_size`.
    #[arg(long, value_delimiter = ',')]
    pool_sizes: Vec<usize>,
    /// The machine, as the report should name it.
    #[arg(long, default_value = "unknown")]
    machine: String,
    /// A note on how the process is constrained, such as a CPU pin.
    #[arg(long, default_value = "none stated")]
    constraint: String,
    /// The commit measured.
    #[arg(long, default_value = "unknown")]
    commit: String,
    /// Write the JSON report here instead of standard output.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// What this process was started to be.
#[derive(Debug, Eq, PartialEq)]
enum Launch {
    /// A render pool child: the first argument is `render-host`.
    RenderHost,
    Bench,
}

fn launch(first_argument: Option<&OsStr>) -> Launch {
    if first_argument.is_some_and(|argument| argument == "render-host") {
        Launch::RenderHost
    } else {
        Launch::Bench
    }
}

/// The child's memory limit from `--memory-limit-bytes N`, as the pool passes it.
fn memory_limit(args: &[std::ffi::OsString]) -> Option<u64> {
    let at = args.iter().position(|arg| arg == "--memory-limit-bytes")?;
    args.get(at + 1)?.to_str()?.parse().ok()
}

fn pool_sizes(requested: &[usize]) -> Vec<usize> {
    let mut sizes = if requested.is_empty() {
        vec![1, production_pool().size]
    } else {
        requested.to_vec()
    };
    sizes.retain(|size| *size > 0);
    sizes.dedup();
    sizes
}

fn summary_json(summary: &Summary) -> Value {
    json!({
        "count": summary.count,
        "p50_ms": millis(summary.p50),
        "p95_ms": millis(summary.p95),
        "p99_ms": millis(summary.p99),
        "max_ms": millis(summary.max),
    })
}

/// The report of a finished benchmark. The verdict is on the production pool size.
fn report(
    args: &Args,
    sizes: &[usize],
    runs: &[Run],
    in_process: &Summary,
    html_bytes: usize,
    workload: &Workload,
) -> Value {
    let production = production_pool().size;
    let verdict_run = runs
        .iter()
        .find(|run| run.pool_size == production)
        .or_else(|| runs.last());
    let verdict = verdict_run.map(|run| {
        let p95 = millis(run.summary.p95);
        json!({
            "pool_size": run.pool_size,
            "p95_ms": p95,
            "target_p95_ms": TARGET_P95_MS,
            "met": p95 < TARGET_P95_MS,
        })
    });
    json!({
        "benchmark": "render",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "commit": args.commit,
        "machine": args.machine,
        "constraint": args.constraint,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map_or(0, usize::from),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "workload": {
            "blocks": workload.block_count(),
            "snippet_blocks": workload.snippet_blocks(),
            "conditions": workload.extras.conditions.len(),
            "html_bytes": html_bytes,
        },
        "method": {
            "warmup": args.warmup,
            "count": args.count,
            "pool_sizes": sizes,
            "production_pool_size": production,
            "in_flight": "as many as the pool has children",
            "timing": "RenderPool::render_email_with, whole call",
        },
        "pool_runs": runs.iter().map(|run| {
            let mut value = summary_json(&run.summary);
            value["pool_size"] = json!(run.pool_size);
            value
        }).collect::<Vec<_>>(),
        "in_process": summary_json(in_process),
        "verdict": verdict,
    })
}

/// The whole benchmark against the given render-host command. Returns the report.
async fn run(args: &Args, command: &ChildCommand) -> Result<Value, String> {
    let workload = Arc::new(Workload::build()?);
    let html_bytes = bench::check_same_output(command, &workload).await?;
    let sizes = pool_sizes(&args.pool_sizes);
    let mut runs = Vec::new();
    for size in &sizes {
        runs.push(bench::through_pool(command, *size, args.warmup, args.count, &workload).await?);
    }
    let in_process = bench::in_process(&workload, args.warmup, args.count)?;
    Ok(report(
        args,
        &sizes,
        &runs,
        &in_process,
        html_bytes,
        &workload,
    ))
}

fn main() -> ExitCode {
    let raw: Vec<_> = std::env::args_os().collect();
    if launch(raw.get(1).map(|arg| arg.as_os_str())) == Launch::RenderHost {
        return ExitCode::from(zirv_flow_render::host::run(memory_limit(&raw)));
    }
    let args = Args::parse();
    let outcome = std::env::current_exe()
        .map_err(|e| format!("cannot find this executable: {e}"))
        .and_then(|exe| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("could not start the async runtime: {e}"))?;
            let command = ChildCommand::render_host(exe, bench::MEMORY_LIMIT_BYTES);
            runtime.block_on(run(&args, &command))
        });
    let report = match outcome {
        Ok(report) => report,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };
    let text = serde_json::to_string_pretty(&report).unwrap_or_default();
    let written = match &args.out {
        Some(path) => std::fs::write(path, format!("{text}\n")).map_err(|e| e.to_string()),
        None => {
            println!("{text}");
            Ok(())
        }
    };
    if let Err(error) = written {
        eprintln!("error: cannot write the report: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    const BANNER: &[u8] = b"running 1 test\n";

    /// A render-host child that is this test binary re-run filtered to [`child_entry`]; libtest
    /// prints its banner first, which the pool skips.
    fn test_child() -> ChildCommand {
        let exe = std::env::current_exe().expect("the test binary has a path");
        ChildCommand::new(exe)
            .args([
                "--exact",
                "tests::child_entry",
                "--nocapture",
                "--test-threads",
                "1",
                "-q",
            ])
            .env("RENDERBENCH_TEST_CHILD", "1")
            .skip_stdout_until(BANNER)
    }

    /// The child: a no-op in a normal run, a render host when the parent asks for one.
    #[test]
    fn child_entry() {
        if std::env::var_os("RENDERBENCH_TEST_CHILD").is_none() {
            return;
        }
        let code = zirv_flow_render::host::run(None);
        std::process::exit(i32::from(code));
    }

    fn args(extra: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("renderbench").chain(extra.iter().copied()))
            .expect("the arguments parse")
    }

    #[tokio::test]
    async fn a_smoke_run_of_twenty_renders_through_the_real_pool_produces_a_report() {
        let args = args(&[
            "--warmup",
            "3",
            "--count",
            "20",
            "--pool-sizes",
            "1,2",
            "--machine",
            "test",
        ]);
        let report = run(&args, &test_child()).await.expect("the run finishes");
        let runs = report["pool_runs"].as_array().expect("pool runs");
        assert_eq!(runs.len(), 2);
        for run in runs {
            assert_eq!(run["count"], 20);
            assert!(run["p95_ms"].as_f64().expect("p95") > 0.0);
        }
        assert_eq!(report["in_process"]["count"], 20);
        assert_eq!(report["workload"]["blocks"], 20);
        assert_eq!(report["method"]["warmup"], 3);
        assert_eq!(report["machine"], "test");
        assert_eq!(report["verdict"]["pool_size"], 2);
        assert!(report["verdict"]["met"].is_boolean());
    }

    #[test]
    fn the_render_host_argument_makes_a_pool_child_and_nothing_else_does() {
        assert_eq!(launch(Some(OsStr::new("render-host"))), Launch::RenderHost);
        assert_eq!(launch(Some(OsStr::new("--count"))), Launch::Bench);
        assert_eq!(launch(None), Launch::Bench);
    }

    #[test]
    fn the_child_reads_its_memory_limit_from_the_pool_arguments() {
        let given: Vec<OsString> = ["renderbench", "render-host", "--memory-limit-bytes", "1024"]
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(memory_limit(&given), Some(1024));
        assert_eq!(memory_limit(&given[..2]), None);
    }

    #[test]
    fn the_default_runs_are_pool_size_one_and_the_production_size() {
        assert_eq!(pool_sizes(&[]), [1, 2]);
        assert_eq!(pool_sizes(&[2, 0, 4]), [2, 4]);
    }

    #[test]
    fn the_method_defaults_are_the_issue_ones() {
        let defaults = args(&[]);
        assert_eq!((defaults.warmup, defaults.count), (200, 5000));
    }
}
