//! Drives one load phase: dispatches requests on the open-loop schedule, bounded by the
//! concurrency, and honours `429 Retry-After` by pausing the whole schedule, not just the
//! request that was refused.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde_json::json;
use tokio::sync::Semaphore;

use crate::pacing::{request_due_at, retry_after};
use crate::payload::{Kind, batch_kind, item};
use crate::report::{Latency, PhaseReport, StatusCounts, achieved_rate};

/// How often a refused request is resent after waiting before it is given up on.
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
    pub concurrency: u32,
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
            Mode::Identify => item(
                self.seed,
                &self.run_id,
                self.people,
                first_seq,
                Kind::Identify,
                false,
            ),
            Mode::Track => item(
                self.seed,
                &self.run_id,
                self.people,
                first_seq,
                Kind::Track,
                false,
            ),
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

#[derive(Default)]
struct Collector {
    status: StatusCounts,
    latencies_us: Vec<u64>,
    requests: u64,
    items_accepted: u64,
    retries: u64,
}

struct Shared {
    /// Set by a 429; the dispatcher sends nothing before it.
    pause_until: Mutex<Option<Instant>>,
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

/// Sends one request, resending it unchanged after a 429's `Retry-After`.
async fn send_request(
    client: reqwest::Client,
    config: Arc<Config>,
    shared: Arc<Shared>,
    body: Vec<u8>,
    items: u64,
) {
    let url = format!("{}{}", config.url, config.mode.path());
    let mut attempt = 0;
    loop {
        attempt += 1;
        let started = Instant::now();
        let response = client
            .post(&url)
            .bearer_auth(&config.key)
            .header(CONTENT_TYPE, "application/json")
            .body(body.clone())
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
        let latency = started.elapsed();
        let retry = status == Some(429) && attempt < MAX_ATTEMPTS;
        {
            let mut collector = shared.collector.lock().expect("collector lock");
            collector.status.record(status);
            if attempt == 1 {
                collector.requests += 1;
            }
            if status == Some(202) {
                collector.items_accepted += items;
                collector
                    .latencies_us
                    .push(u64::try_from(latency.as_micros()).unwrap_or(u64::MAX));
            }
            if retry {
                collector.retries += 1;
            }
        }
        if !retry {
            return;
        }
        shared.pause_for(wait);
        tokio::time::sleep(wait).await;
    }
}

/// Runs one phase to completion and returns its report, its 202 latencies in microseconds
/// and the next free item number.
pub async fn run_phase(
    client: &reqwest::Client,
    config: &Arc<Config>,
    phase: &Phase,
    first_seq: u64,
) -> (PhaseReport, Vec<u64>, u64) {
    let shared = Arc::new(Shared {
        pause_until: Mutex::new(None),
        collector: Mutex::new(Collector::default()),
    });
    let permits = config.concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(permits as usize));
    let per_request = config.items_per_request();
    let planned = Duration::from_secs_f64(phase.duration_s);
    // A server that refuses everything must not hold the run forever.
    let wall_limit = planned * 2 + Duration::from_secs(30);

    let start = Instant::now();
    let mut paused = Duration::ZERO;
    let mut offered = 0u64;
    loop {
        if let Some(wait) = shared.remaining_pause() {
            tokio::time::sleep(wait).await;
            paused += wait;
            continue;
        }
        let wall = start.elapsed();
        let active = wall.saturating_sub(paused);
        if active >= planned || wall >= wall_limit {
            break;
        }
        let due = request_due_at(offered, phase.rate);
        if active < due {
            // Wake early enough to notice a pause and the end of the phase.
            tokio::time::sleep((due - active).min(Duration::from_millis(20))).await;
            continue;
        }
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let body = config.body(first_seq + offered, per_request);
        let (client, config, shared) = (client.clone(), config.clone(), shared.clone());
        tokio::spawn(async move {
            send_request(client, config, shared, body, per_request).await;
            drop(permit);
        });
        offered += per_request;
    }
    // Every permit back means every request has finished.
    let _all = semaphore
        .acquire_many(permits)
        .await
        .expect("the semaphore is never closed");
    let elapsed_s = start.elapsed().as_secs_f64();

    let collector = std::mem::take(&mut *shared.collector.lock().expect("collector lock"));
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
        latency: Latency::from_micros(collector.latencies_us.clone()),
    };
    (report, collector.latencies_us, first_seq + offered)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A one-connection-per-request HTTP server: it answers 429 with `Retry-After: 1` to the
    /// first `refuse` requests and 202 to the rest, and returns the bodies it saw.
    async fn serve(refuse: usize) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut chunk = [0u8; 4096];
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
                        let mut log = log.lock().unwrap();
                        log.push(body);
                        log.len()
                    };
                    let response = if number <= refuse {
                        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\n\
                         content-length: 0\r\nconnection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    };
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        (url, seen)
    }

    fn config(url: String, mode: Mode) -> Arc<Config> {
        Arc::new(Config {
            url,
            key: "zfk_test".to_owned(),
            mode,
            seed: 5,
            run_id: "t".to_owned(),
            people: 10,
            batch_size: 4,
            concurrency: 4,
        })
    }

    fn client() -> reqwest::Client {
        crate::init_tls();
        reqwest::Client::new()
    }

    #[tokio::test]
    async fn a_phase_offers_the_planned_items_and_counts_every_202() {
        let (url, seen) = serve(0).await;
        let phase = Phase {
            name: "base",
            rate: 100,
            duration_s: 0.5,
        };
        let (report, latencies, next) =
            run_phase(&client(), &config(url, Mode::Batch), &phase, 0).await;

        // 100 items/s for 0.5 s in batches of 4: requests are due every 40 ms.
        assert!((12..=14).contains(&report.requests), "{}", report.requests);
        assert_eq!(report.items_offered, report.requests * 4);
        assert_eq!(report.items_accepted, report.items_offered);
        assert_eq!(report.status.accepted_202, report.requests);
        assert_eq!(report.status.rate_limited_429, 0);
        assert_eq!(latencies.len() as u64, report.requests);
        assert_eq!(report.latency.count, report.requests);
        assert_eq!(next, report.items_offered);
        assert_eq!(seen.lock().unwrap().len() as u64, report.requests);
    }

    #[tokio::test]
    async fn a_429_pauses_the_schedule_and_resends_the_same_body_after_retry_after() {
        let (url, seen) = serve(1).await;
        let phase = Phase {
            name: "base",
            rate: 10,
            duration_s: 0.25,
        };
        let (report, _, _) = run_phase(&client(), &config(url, Mode::Identify), &phase, 0).await;

        assert_eq!(report.status.rate_limited_429, 1);
        assert_eq!(report.retries_after_429, 1);
        assert_eq!(report.status.accepted_202, report.requests);
        assert!(report.elapsed_s >= 1.0, "{}", report.elapsed_s);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], seen[1], "the refused body is resent unchanged");
        // Nothing was sent during the pause: the 429 and its retry are the first two requests.
        assert!(seen.len() as u64 == report.requests + 1);
    }

    #[tokio::test]
    async fn an_unreachable_server_counts_transport_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let phase = Phase {
            name: "base",
            rate: 10,
            duration_s: 0.2,
        };
        let (report, _, _) = run_phase(&client(), &config(url, Mode::Track), &phase, 0).await;

        assert!(report.requests > 0);
        assert_eq!(report.status.transport_errors, report.requests);
        assert_eq!(report.items_accepted, 0);
    }
}
