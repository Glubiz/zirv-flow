//! Drives one load phase, open loop: each request is sent at its scheduled due time whatever the
//! server is doing, never held back by a small pool of permits. Latency is measured from the
//! scheduled time, so a stalled server cannot hide behind a generator that stopped offering load
//! (coordinated omission). A `429` sets one shared pause; the refused request goes back to the
//! dispatcher, which resends the throttled requests after the pause, ahead of new ones and paced
//! at the target rate, so retries never stampede.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::pacing::{request_due_at, retry_after};
use crate::payload::{Kind, batch_kind, item};
use crate::report::{Latency, PhaseReport, StatusCounts, achieved_rate};

/// How many times a refused request is sent before it is given up on.
const MAX_ATTEMPTS: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Identify,
    Track,
    Batch,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Identify => "identify",
            Self::Track => "track",
            Self::Batch => "batch",
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Identify => "/api/v1/ingest/identify",
            Self::Track => "/api/v1/ingest/track",
            Self::Batch => "/api/v1/ingest/batch",
        }
    }
}

pub struct Config {
    /// Base URL without a trailing slash.
    pub url: String,
    pub key: String,
    pub mode: Mode,
    pub seed: u64,
    pub run_id: String,
    pub people: u64,
    pub batch_size: u32,
    /// A safety cap on requests in flight, not a pacing device.
    pub max_in_flight: u32,
}

impl Config {
    /// Items per request: one for identify and track, the batch size for batch.
    pub fn items_per_request(&self) -> u64 {
        match self.mode {
            Mode::Batch => u64::from(self.batch_size),
            _ => 1,
        }
    }

    fn body(&self, first_seq: u64, count: u64) -> Vec<u8> {
        let value = match self.mode {
            Mode::Identify | Mode::Track => {
                let kind = if self.mode == Mode::Identify {
                    Kind::Identify
                } else {
                    Kind::Track
                };
                item(self.seed, &self.run_id, self.people, first_seq, kind, false)
            }
            Mode::Batch => {
                let items: Vec<_> = (first_seq..first_seq + count)
                    .map(|seq| {
                        let kind = batch_kind(self.seed, seq);
                        item(self.seed, &self.run_id, self.people, seq, kind, true)
                    })
                    .collect();
                json!({ "batch": items })
            }
        };
        serde_json::to_vec(&value).expect("a JSON value serialises")
    }
}

pub struct Phase {
    pub name: &'static str,
    pub rate: u32,
    pub duration_s: f64,
}

/// The samples of a phase, for merging into the run total.
pub struct PhaseOutcome {
    pub report: PhaseReport,
    pub schedule_us: Vec<u64>,
    pub service_us: Vec<u64>,
    pub lag_us: Vec<u64>,
    pub next_seq: u64,
}

/// One request: what it carries, when it was due and how often it has been sent.
struct Job {
    count: u64,
    body: Arc<Vec<u8>>,
    scheduled_at: Instant,
    attempts: u32,
}

#[derive(Default)]
struct Collector {
    status: StatusCounts,
    schedule_us: Vec<u64>,
    service_us: Vec<u64>,
    lag_us: Vec<u64>,
    requests: u64,
    items_accepted: u64,
    retries: u64,
    late_dispatch: u64,
    abandoned: u64,
    gave_up: u64,
}

struct Shared {
    /// Set by a 429 to the latest `Retry-After` seen; the dispatcher sends nothing before it.
    pause_until: Mutex<Option<Instant>>,
    /// Refused requests waiting for the dispatcher to resend them.
    retries: Mutex<VecDeque<Job>>,
    in_flight: AtomicU64,
    collector: Mutex<Collector>,
}

impl Shared {
    fn pause_for(&self, wait: Duration) {
        let until = Instant::now() + wait;
        let mut guard = self.pause_until.lock().expect("pause lock");
        if guard.is_none_or(|current| current < until) {
            *guard = Some(until);
        }
    }

    fn remaining_pause(&self) -> Option<Duration> {
        let guard = self.pause_until.lock().expect("pause lock");
        guard.and_then(|until| until.checked_duration_since(Instant::now()))
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Sends a job once. A 429 sets the shared pause and hands the job back to the dispatcher.
async fn send_once(
    client: reqwest::Client,
    config: Arc<Config>,
    shared: Arc<Shared>,
    mut job: Job,
) {
    let url = format!("{}{}", config.url, config.mode.path());
    let started = Instant::now();
    let response = client
        .post(&url)
        .bearer_auth(&config.key)
        .header(CONTENT_TYPE, "application/json")
        .body(job.body.as_ref().clone())
        .send()
        .await;
    let (status, wait) = match response {
        Ok(response) => {
            let status = response.status().as_u16();
            let wait = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            // Read the body so the latency covers the whole response.
            let _ = response.bytes().await;
            (Some(status), retry_after(wait.as_deref()))
        }
        Err(_) => (None, Duration::ZERO),
    };
    let done = Instant::now();
    let retry = status == Some(429) && job.attempts + 1 < MAX_ATTEMPTS;
    {
        let mut collector = shared.collector.lock().expect("collector lock");
        collector.status.record(status);
        if job.attempts == 0 {
            collector.requests += 1;
        }
        if status == Some(202) {
            collector.items_accepted += job.count;
        }
        if retry {
            collector.retries += 1;
        } else {
            // One sample per request at its final outcome, whatever it was: an overloaded
            // server's timeouts, 5xx and exhausted retries must move the percentiles too.
            // schedule = dispatch lag + service time.
            collector
                .schedule_us
                .push(micros(done.saturating_duration_since(job.scheduled_at)));
            collector.service_us.push(micros(done - started));
            collector
                .lag_us
                .push(micros(started.saturating_duration_since(job.scheduled_at)));
            if status == Some(429) {
                collector.gave_up += 1;
            }
        }
    }
    if retry {
        shared.pause_for(wait);
        job.attempts += 1;
        shared.retries.lock().expect("retry lock").push_back(job);
    }
    shared.in_flight.fetch_sub(1, Ordering::SeqCst);
}

/// Runs one phase to completion.
pub async fn run_phase(
    client: &reqwest::Client,
    config: &Arc<Config>,
    phase: &Phase,
    first_seq: u64,
) -> PhaseOutcome {
    let shared = Arc::new(Shared {
        pause_until: Mutex::new(None),
        retries: Mutex::new(VecDeque::new()),
        in_flight: AtomicU64::new(0),
        collector: Mutex::new(Collector::default()),
    });
    let cap = Arc::new(Semaphore::new(config.max_in_flight.max(1) as usize));
    let per_request = config.items_per_request();
    let planned = Duration::from_secs_f64(phase.duration_s);
    // A server that refuses everything must not hold the run forever.
    let wall_limit = planned * 2 + Duration::from_secs(30);

    // Sends `job` now. At the in-flight cap it waits for a slot and counts as late.
    let dispatch = |job: Job| {
        let (client, config, shared, cap) =
            (client.clone(), config.clone(), shared.clone(), cap.clone());
        async move {
            let permit = match cap.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    shared
                        .collector
                        .lock()
                        .expect("collector lock")
                        .late_dispatch += 1;
                    cap.acquire_owned().await.expect("the cap is never closed")
                }
            };
            shared.in_flight.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                send_once(client, config, shared, job).await;
                drop(permit);
            });
            tokio::task::yield_now().await;
        }
    };

    let start = Instant::now();
    // Time the schedule was held back: pauses obeyed and the slots retries used.
    let mut held = Duration::ZERO;
    let mut offered = 0u64;
    let mut abandoned = 0u64;
    loop {
        if let Some(wait) = shared.remaining_pause() {
            tokio::time::sleep(wait).await;
            held += wait;
            continue;
        }
        let wall = start.elapsed();
        let retry = shared.retries.lock().expect("retry lock").pop_front();
        if let Some(job) = retry {
            // Throttled requests first, one slot of the schedule each.
            let slot = request_due_at(job.count, phase.rate);
            dispatch(job).await;
            tokio::time::sleep(slot).await;
            held += slot;
            continue;
        }
        let active = wall.saturating_sub(held);
        if active >= planned || wall >= wall_limit {
            let pending = shared.in_flight.load(Ordering::SeqCst) > 0;
            if !pending && shared.retries.lock().expect("retry lock").is_empty() {
                break;
            }
            if wall >= wall_limit {
                let mut queue = shared.retries.lock().expect("retry lock");
                abandoned += queue.len() as u64;
                queue.clear();
                if !pending {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        let due = request_due_at(offered, phase.rate);
        if active < due {
            // Wake early enough to notice a pause and the end of the phase.
            tokio::time::sleep((due - active).min(Duration::from_millis(20))).await;
            continue;
        }
        dispatch(Job {
            count: per_request,
            body: Arc::new(config.body(first_seq + offered, per_request)),
            scheduled_at: start + due + held,
            attempts: 0,
        })
        .await;
        offered += per_request;
    }
    let elapsed_s = start.elapsed().as_secs_f64();

    let mut collector = std::mem::take(&mut *shared.collector.lock().expect("collector lock"));
    collector.abandoned += abandoned;
    let report = PhaseReport {
        name: phase.name.to_owned(),
        target_rate: phase.rate,
        planned_duration_s: phase.duration_s,
        elapsed_s,
        requests: collector.requests,
        items_offered: offered,
        items_accepted: collector.items_accepted,
        achieved_rate: achieved_rate(collector.items_accepted, elapsed_s),
        retries_after_429: collector.retries,
        status: collector.status,
        latency_from_schedule: Latency::from_micros(collector.schedule_us.clone()),
        service_latency: Latency::from_micros(collector.service_us.clone()),
        dispatch_lag: Latency::from_micros(collector.lag_us.clone()),
        late_dispatch: collector.late_dispatch,
        abandoned_requests: collector.abandoned,
        gave_up_after_retries: collector.gave_up,
    };
    PhaseOutcome {
        report,
        schedule_us: collector.schedule_us,
        service_us: collector.service_us,
        lag_us: collector.lag_us,
        next_seq: first_seq + offered,
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// What the mock server does with requests, by arrival order.
    #[derive(Clone, Copy, Default)]
    struct Behavior {
        /// Answer the first `refuse` requests with 429 and `Retry-After: 1`, all together once
        /// the last of them has arrived.
        refuse: usize,
        /// Delay before every other response.
        delay: Duration,
        /// Status of every other response; 0 means 202.
        status: u16,
    }

    struct Mock {
        url: String,
        bodies: Arc<Mutex<Vec<String>>>,
        arrivals: Arc<Mutex<Vec<Instant>>>,
    }

    async fn serve(behavior: Behavior) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let (log, times) = (bodies.clone(), arrivals.clone());
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (log, times) = (log.clone(), times.clone());
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head_end, length) = loop {
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..read]);
                        let text = String::from_utf8_lossy(&data).to_lowercase();
                        if let Some(end) = text.find("\r\n\r\n") {
                            let length = text
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while data.len() < head_end + length {
                        let read = socket.read(&mut chunk).await.unwrap();
                        if read == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..read]);
                    }
                    let body = String::from_utf8_lossy(&data[head_end..]).into_owned();
                    let number = {
                        times.lock().unwrap().push(Instant::now());
                        let mut log = log.lock().unwrap();
                        log.push(body);
                        log.len()
                    };
                    let refused = number <= behavior.refuse;
                    if refused {
                        let waited = Instant::now();
                        while log.lock().unwrap().len() < behavior.refuse
                            && waited.elapsed() < Duration::from_millis(500)
                        {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    } else {
                        tokio::time::sleep(behavior.delay).await;
                    }
                    let response = if refused {
                        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\n\
                         content-length: 0\r\nconnection: close\r\n\r\n"
                    } else if behavior.status == 503 {
                        "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\
                         connection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Mock {
            url,
            bodies,
            arrivals,
        }
    }

    fn config(url: String, mode: Mode, max_in_flight: u32) -> Arc<Config> {
        Arc::new(Config {
            url,
            key: "zfk_test".to_owned(),
            mode,
            seed: 5,
            run_id: "t".to_owned(),
            people: 10,
            batch_size: 4,
            max_in_flight,
        })
    }

    fn client() -> reqwest::Client {
        crate::init_tls();
        reqwest::Client::new()
    }

    fn phase(rate: u32, duration_s: f64) -> Phase {
        Phase {
            name: "base",
            rate,
            duration_s,
        }
    }

    #[tokio::test]
    async fn a_phase_offers_the_planned_items_and_counts_every_202() {
        let mock = serve(Behavior::default()).await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Batch, 10_000),
            &phase(100, 0.5),
            0,
        )
        .await;
        let report = outcome.report;

        // 100 items/s for 0.5 s in batches of 4: requests are due every 40 ms.
        assert!((12..=14).contains(&report.requests), "{}", report.requests);
        assert_eq!(report.items_offered, report.requests * 4);
        assert_eq!(report.items_accepted, report.items_offered);
        assert_eq!(report.status.accepted_202, report.requests);
        assert_eq!(report.status.rate_limited_429, 0);
        assert_eq!(report.late_dispatch, 0);
        assert_eq!(outcome.schedule_us.len() as u64, report.requests);
        assert_eq!(outcome.service_us.len() as u64, report.requests);
        assert_eq!(report.dispatch_lag.count, report.requests);
        assert_eq!(outcome.next_seq, report.items_offered);
        assert_eq!(mock.bodies.lock().unwrap().len() as u64, report.requests);
    }

    #[tokio::test]
    async fn slow_responses_do_not_throttle_the_offered_rate() {
        // 200 ms responses at 100 requests/s need 20 in flight. A pool of 4 permits would have
        // held the offered rate to about 20 requests/s; open loop keeps to the schedule.
        let mock = serve(Behavior {
            refuse: 0,
            delay: Duration::from_millis(200),
            ..Behavior::default()
        })
        .await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Identify, 10_000),
            &phase(100, 1.0),
            0,
        )
        .await;
        let report = outcome.report;

        assert!(report.requests >= 90, "{}", report.requests);
        assert_eq!(report.late_dispatch, 0);
        assert!(
            report.dispatch_lag.p99_ms < 100.0,
            "{:?}",
            report.dispatch_lag
        );
        assert!(report.service_latency.p50_ms >= 200.0);
    }

    #[tokio::test]
    async fn latency_is_measured_from_the_schedule_when_the_cap_holds_requests_back() {
        // Cap of 2 with 200 ms responses at 50 requests/s: far more is due than may fly, so
        // requests go out late. The service time stays near 200 ms; the schedule-based latency
        // carries the lag and is much larger, which a send-to-response clock would hide.
        let mock = serve(Behavior {
            refuse: 0,
            delay: Duration::from_millis(200),
            ..Behavior::default()
        })
        .await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Identify, 2),
            &phase(50, 1.0),
            0,
        )
        .await;
        let report = outcome.report;

        assert!(report.late_dispatch > 0);
        assert!(
            report.dispatch_lag.max_ms > 300.0,
            "{:?}",
            report.dispatch_lag
        );
        assert!(
            report.latency_from_schedule.p99_ms > report.service_latency.p99_ms + 300.0,
            "{:?} vs {:?}",
            report.latency_from_schedule,
            report.service_latency
        );
        assert!(report.service_latency.p99_ms < 500.0);
    }

    #[tokio::test]
    async fn a_429_pauses_the_schedule_and_the_retry_keeps_the_original_clock() {
        let mock = serve(Behavior {
            refuse: 1,
            ..Behavior::default()
        })
        .await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Identify, 10_000),
            &phase(10, 0.25),
            0,
        )
        .await;
        let report = outcome.report;

        assert_eq!(report.status.rate_limited_429, 1);
        assert_eq!(report.retries_after_429, 1);
        assert_eq!(report.status.accepted_202, report.requests);
        assert!(report.elapsed_s >= 1.0, "{}", report.elapsed_s);
        let seen = mock.bodies.lock().unwrap();
        assert_eq!(seen[0], seen[1], "the refused body is resent unchanged");
        assert_eq!(seen.len() as u64, report.requests + 1);
        // The throttled request waited out Retry-After: that is in its latency from the
        // schedule, and not in its service time.
        assert!(
            report.latency_from_schedule.max_ms >= 1000.0,
            "{:?}",
            report.latency_from_schedule
        );
        assert!(report.service_latency.max_ms < 500.0);
        // The final send came about one Retry-After after the schedule: that is the lag.
        assert_eq!(report.dispatch_lag.count, report.requests);
        assert!(
            (900.0..3000.0).contains(&report.dispatch_lag.max_ms),
            "{:?}",
            report.dispatch_lag
        );
    }

    #[tokio::test]
    async fn slow_failures_move_the_latency_percentiles() {
        // Every response is a 503 after 300 ms. Counting only 202s would leave all three latency
        // blocks empty and hide the overload.
        let mock = serve(Behavior {
            delay: Duration::from_millis(300),
            status: 503,
            ..Behavior::default()
        })
        .await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Identify, 10_000),
            &phase(20, 0.5),
            0,
        )
        .await;
        let report = outcome.report;

        assert!(report.requests >= 8, "{}", report.requests);
        assert_eq!(report.status.server_5xx, report.requests);
        assert_eq!(report.items_accepted, 0);
        assert_eq!(report.latency_from_schedule.count, report.requests);
        assert_eq!(report.service_latency.count, report.requests);
        assert_eq!(report.dispatch_lag.count, report.requests);
        assert!(report.latency_from_schedule.p99_ms >= 300.0);
        assert!(report.service_latency.p50_ms >= 300.0);
    }

    #[tokio::test]
    async fn simultaneous_429s_are_resent_paced_after_one_shared_pause() {
        // 60 requests are in flight when the server answers all of them 429 at once. Each waited
        // its own Retry-After and re-entered together, they would arrive as one burst a second
        // later; handed back to the dispatcher they arrive at the 600/s target rate.
        let mock = serve(Behavior {
            refuse: 60,
            ..Behavior::default()
        })
        .await;
        let outcome = run_phase(
            &client(),
            &config(mock.url.clone(), Mode::Identify, 10_000),
            &phase(600, 0.12),
            0,
        )
        .await;
        let report = outcome.report;

        assert_eq!(report.status.rate_limited_429, 60);
        assert_eq!(report.retries_after_429, 60);
        assert_eq!(report.status.accepted_202, report.requests);
        let arrivals = mock.arrivals.lock().unwrap().clone();
        let first_wave = arrivals[59];
        let later: Vec<Instant> = arrivals
            .iter()
            .copied()
            .filter(|at| at.duration_since(first_wave) >= Duration::from_millis(500))
            .collect();
        assert!(later.len() >= 60, "{}", later.len());
        assert!(later[0].duration_since(first_wave) >= Duration::from_millis(900));
        let span = later[59].duration_since(later[0]);
        assert!(
            span >= Duration::from_millis(60),
            "retries spanned {span:?}"
        );
        // No 20 ms window holds anything like the whole wave.
        let busiest = later
            .iter()
            .map(|from| {
                later
                    .iter()
                    .filter(|at| {
                        **at >= *from && at.duration_since(*from) < Duration::from_millis(20)
                    })
                    .count()
            })
            .max()
            .unwrap();
        assert!(busiest <= 30, "{busiest} retries inside 20 ms");
    }

    #[tokio::test]
    async fn an_unreachable_server_counts_transport_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let outcome = run_phase(
            &client(),
            &config(url, Mode::Track, 10_000),
            &phase(10, 0.2),
            0,
        )
        .await;
        let report = outcome.report;

        assert!(report.requests > 0);
        assert_eq!(report.status.transport_errors, report.requests);
        assert_eq!(report.items_accepted, 0);
    }
}
