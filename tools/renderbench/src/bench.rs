//! The measured runs: the real render pool with every child busy, and the same render in-process.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use zirv_flow_render::{ChildCommand, Hooks, PoolConfig, RenderPool};
use zirv_flow_templates::{Rendered, Tracking, render_email_with};

use crate::stats::{Summary, micros};
use crate::workload::Workload;

const TRACKING_BASE: &str = "https://flow.example/t/c";

/// The default `render.memory_limit_mib` (512), in bytes: the limit a production child asks for.
pub(crate) const MEMORY_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

fn token(index: usize, _url: &str) -> String {
    format!("tok{index}")
}

/// The hooks of a marketing send: click tracking on.
fn hooks() -> Hooks<'static> {
    Hooks {
        tracking: Tracking::Rewrite {
            base_url: TRACKING_BASE,
            token: &token,
        },
        utm: None,
    }
}

/// The production pool settings: `render.pool_size`, `render.deadline_ms` and
/// `render.memory_limit_mib` default to the pool's own defaults and 512 MiB (the config test
/// `the_render_defaults_are_two_children_five_seconds_and_512_mib` pins them).
pub(crate) fn production_pool() -> PoolConfig {
    PoolConfig::default()
}

/// Run `render` `warmup + count` times with `workers` calls in flight at once, each timed from
/// before the call to after it, and return the samples of the counted renders only. The warm-up
/// finishes before the counted renders start.
pub(crate) async fn drive<F, Fut>(
    workers: usize,
    warmup: usize,
    count: usize,
    render: F,
) -> Result<Vec<u64>, String>
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    phase(workers, warmup, render.clone()).await?;
    phase(workers, count, render).await
}

async fn phase<F, Fut>(workers: usize, total: usize, render: F) -> Result<Vec<u64>, String>
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let next = Arc::new(AtomicUsize::new(0));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..workers.max(1) {
        let next = Arc::clone(&next);
        let render = render.clone();
        tasks.spawn(async move {
            let mut samples = Vec::new();
            while next.fetch_add(1, Ordering::Relaxed) < total {
                let started = Instant::now();
                render().await?;
                samples.push(micros(started.elapsed()));
            }
            Ok::<_, String>(samples)
        });
    }
    let mut samples = Vec::with_capacity(total);
    while let Some(joined) = tasks.join_next().await {
        samples.extend(joined.map_err(|e| e.to_string())??);
    }
    Ok(samples)
}

/// The result of one run.
pub(crate) struct Run {
    pub(crate) pool_size: usize,
    pub(crate) summary: Summary,
}

/// Render through a pool of `pool_size` children, as many calls in flight as children.
pub(crate) async fn through_pool(
    command: &ChildCommand,
    pool_size: usize,
    warmup: usize,
    count: usize,
    workload: &Arc<Workload>,
) -> Result<Run, String> {
    let config = PoolConfig {
        size: pool_size,
        ..production_pool()
    };
    let pool = RenderPool::start(command.clone(), config)
        .await
        .map_err(|e| format!("the render pool did not start: {e}"))?;
    let call = {
        let pool = pool.clone();
        let workload = Arc::clone(workload);
        move || {
            let pool = pool.clone();
            let workload = Arc::clone(&workload);
            async move {
                let rendered = {
                    pool.render_email_with(
                        &workload.doc,
                        &workload.context,
                        &hooks(),
                        &workload.extras,
                    )
                    .await
                    .map_err(|e| format!("a render failed: {e}"))?
                };
                std::hint::black_box(rendered);
                Ok(())
            }
        }
    };
    let samples = drive(pool_size, warmup, count, call).await;
    pool.shutdown().await;
    let summary = Summary::of(&samples?, 0).ok_or("no renders were counted")?;
    Ok(Run { pool_size, summary })
}

/// The same render on the calling thread, one after another: what the pool adds is the
/// difference.
pub(crate) fn in_process(
    workload: &Workload,
    warmup: usize,
    count: usize,
) -> Result<Summary, String> {
    let mut samples = Vec::with_capacity(warmup + count);
    for _ in 0..warmup + count {
        let started = Instant::now();
        std::hint::black_box(render_here(workload)?);
        samples.push(micros(started.elapsed()));
    }
    Summary::of(&samples, warmup).ok_or_else(|| "no renders were counted".to_owned())
}

fn render_here(workload: &Workload) -> Result<Rendered, String> {
    render_email_with(&workload.doc, &workload.context, &hooks(), &workload.extras)
        .map_err(|e| format!("the in-process render failed: {e:?}"))
}

/// Before measuring: the pool's bytes must equal the in-process render's, so both time the same
/// work. Returns the size of the HTML.
pub(crate) async fn check_same_output(
    command: &ChildCommand,
    workload: &Workload,
) -> Result<usize, String> {
    let pool = RenderPool::start(
        command.clone(),
        PoolConfig {
            size: 1,
            ..production_pool()
        },
    )
    .await
    .map_err(|e| format!("the render pool did not start: {e}"))?;
    let pooled = {
        pool.render_email_with(&workload.doc, &workload.context, &hooks(), &workload.extras)
            .await
            .map_err(|e| format!("a render failed: {e}"))
    };
    pool.shutdown().await;
    let pooled = pooled?;
    let here = render_here(workload)?;
    if pooled != here {
        return Err("the pool and the in-process render gave different bytes".to_owned());
    }
    Ok(here.html.len())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn warm_up_calls_run_but_their_samples_are_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let samples = drive(3, 7, 20, move || {
            let counter = Arc::clone(&counter);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
        .expect("the drive finishes");
        assert_eq!(calls.load(Ordering::SeqCst), 27);
        assert_eq!(samples.len(), 20);
    }

    #[tokio::test]
    async fn as_many_calls_are_in_flight_as_workers_and_no_more() {
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (now_in, peak_in) = (Arc::clone(&now), Arc::clone(&peak));
        drive(4, 0, 40, move || {
            let (now, peak) = (Arc::clone(&now_in), Arc::clone(&peak_in));
            async move {
                let inside = now.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(inside, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await
        .expect("the drive finishes");
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(now.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn each_sample_covers_the_whole_call() {
        let samples = drive(1, 0, 3, || async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(())
        })
        .await
        .expect("the drive finishes");
        assert!(samples.iter().all(|&us| us >= 19_000), "{samples:?}");
    }

    #[tokio::test]
    async fn a_failed_render_ends_the_run_with_its_reason() {
        let failed = drive(2, 0, 10, || async { Err("boom".to_owned()) }).await;
        assert_eq!(failed.expect_err("a failure"), "boom");
    }

    #[test]
    fn the_in_process_run_counts_only_the_renders_after_the_warm_up() {
        let workload = Workload::build().expect("the workload builds");
        let summary = in_process(&workload, 2, 5).expect("renders");
        assert_eq!(summary.count, 5);
        assert!(summary.p50 > 0 && summary.p50 <= summary.p95 && summary.p95 <= summary.max);
    }

    #[test]
    fn the_production_pool_is_two_children_with_a_five_second_deadline() {
        let config = production_pool();
        assert_eq!(config.size, 2);
        assert_eq!(config.deadline, Duration::from_secs(5));
    }
}
