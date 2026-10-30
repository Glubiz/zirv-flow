//! Closed-loop explorer clients over the synthetic M8 seed. Responses are drained, never saved.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout};

use crate::report::Latency;

const MIX_NOTE: &str = "Name plus source is replaced by name plus a seeded person: the explorer supports person_id, not source. The M8/M4 seed writes source=api on every event. category is low cardinality (six values); amount is high cardinality (0..200 at two decimal places).";

#[derive(Debug, Parser)]
#[command(about = "Run concurrent explorer clients against the M8 seed")]
pub(crate) struct Args {
    #[arg(long)]
    url: String,
    /// File holding a workspace API key with read access.
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long)]
    workspace: String,
    /// A person from this workspace's M8 seed (md5(workspace || '/p/1') as UUID).
    #[arg(long)]
    person_id: String,
    /// Exclusive end of the 90-day window, RFC3339; use m8bench.config.seed_at.
    #[arg(long)]
    to: String,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    clients: u32,
    /// Seconds to launch requests; in-flight requests finish afterwards.
    #[arg(long)]
    duration: f64,
    /// Client deadline for a whole response, including CSV download (server export cap is per query).
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..))]
    request_timeout_seconds: u32,
    #[arg(long)]
    out: PathBuf,
}

struct Query {
    shape: &'static str,
    endpoint: &'static str,
    body: Value,
}

fn query_set(to: OffsetDateTime, person: &str) -> Result<Vec<Query>, String> {
    let from = to
        .checked_sub(time::Duration::days(90))
        .ok_or("--to is too early for a 90-day window")?;
    let base = json!({
        "name": "bench_order_placed",
        "from": from.format(&Rfc3339).map_err(|error| error.to_string())?,
        "to": to.format(&Rfc3339).map_err(|error| error.to_string())?,
    });
    let shapes = [
        ("count_name", "count", json!({"bucket": "day"})),
        (
            "count_property",
            "count",
            json!({"bucket": "day", "filters": [
            {"key": "category", "op": "eq", "value": "books"}]}),
        ),
        (
            "count_person",
            "count",
            json!({"bucket": "day", "person_id": person}),
        ),
        (
            "breakdown_low",
            "breakdown",
            json!({"property": "category"}),
        ),
        ("breakdown_high", "breakdown", json!({"property": "amount"})),
        ("list", "list", json!({"limit": 100})),
        ("export", "export", json!({})),
    ];
    Ok(shapes
        .into_iter()
        .map(|(shape, endpoint, extra)| {
            let mut body = base.clone();
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            Query {
                shape,
                endpoint,
                body,
            }
        })
        .collect())
}

#[derive(Default, Serialize)]
struct Outcomes {
    status: BTreeMap<u16, u64>,
    query_timeouts: u64,
    transport_errors: u64,
    body_errors: u64,
    client_timeouts: u64,
}

#[derive(Default)]
struct Samples {
    outcomes: Outcomes,
    all: Vec<u64>,
    successful: Vec<u64>,
}

impl Samples {
    fn append(&mut self, other: Self) {
        for (status, count) in other.outcomes.status {
            *self.outcomes.status.entry(status).or_default() += count;
        }
        self.outcomes.query_timeouts += other.outcomes.query_timeouts;
        self.outcomes.transport_errors += other.outcomes.transport_errors;
        self.outcomes.body_errors += other.outcomes.body_errors;
        self.outcomes.client_timeouts += other.outcomes.client_timeouts;
        self.all.extend(other.all);
        self.successful.extend(other.successful);
    }
}

fn is_query_timeout(status: u16, body: &[u8]) -> bool {
    status == 503
        && serde_json::from_slice::<Value>(body).is_ok_and(|value| value["code"] == "query_timeout")
}

async fn request(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    query: &Query,
    counts: &mut Outcomes,
) -> bool {
    let response = client
        .post(url)
        .bearer_auth(key)
        .header(CONTENT_TYPE, "application/json")
        .body(query.body.to_string())
        .send()
        .await;
    let Ok(mut response) = response else {
        counts.transport_errors += 1;
        return false;
    };
    let status = response.status().as_u16();
    *counts.status.entry(status).or_default() += 1;
    let mut problem = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if status == 503 && problem.len() < 65_536 {
                    problem.extend_from_slice(&chunk[..chunk.len().min(65_536 - problem.len())]);
                }
            }
            Ok(None) => break,
            Err(_) => {
                counts.body_errors += 1;
                return false;
            }
        }
    }
    counts.query_timeouts += u64::from(is_query_timeout(status, &problem));
    status == 200
}

pub(crate) async fn run(args: Args) -> Result<(), String> {
    let duration = Duration::try_from_secs_f64(args.duration)
        .map_err(|_| "--duration must be finite and positive")?;
    if duration.is_zero() {
        return Err("--duration must be positive".into());
    }
    let to = OffsetDateTime::parse(&args.to, &Rfc3339).map_err(|error| format!("--to: {error}"))?;
    let queries = Arc::new(query_set(to, &args.person_id)?);
    let mut base = reqwest::Url::parse(&args.url).map_err(|error| error.to_string())?;
    if base.scheme() != "http" {
        return Err("--url must use plain http://".into());
    }
    base.path_segments_mut()
        .map_err(|()| "--url needs a host")?
        .pop_if_empty()
        .extend([
            "api",
            "v1",
            "workspaces",
            &args.workspace,
            "events",
            "explore",
        ]);
    base.set_query(None);
    base.set_fragment(None);
    let key = Arc::new(crate::read_key(&args.key_file)?);
    crate::init_tls();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    let end = started
        .checked_add(duration)
        .ok_or("--duration is too large")?;
    let mut tasks = JoinSet::new();
    for number in 0..args.clients {
        let client = client.clone();
        let key = Arc::clone(&key);
        let queries = Arc::clone(&queries);
        let base = base.to_string();
        let deadline = Duration::from_secs(u64::from(args.request_timeout_seconds));
        tasks.spawn(async move {
            let mut samples: Vec<Samples> = queries.iter().map(|_| Samples::default()).collect();
            let mut index = number as usize % queries.len();
            while Instant::now() < end {
                let query = &queries[index];
                let sample = &mut samples[index];
                let sent = Instant::now();
                let url = format!("{base}/{}", query.endpoint);
                let success = match timeout(
                    deadline,
                    request(&client, &url, &key, query, &mut sample.outcomes),
                )
                .await
                {
                    Ok(success) => success,
                    Err(_) => {
                        sample.outcomes.client_timeouts += 1;
                        false
                    }
                };
                let micros = sent.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                sample.all.push(micros);
                if success {
                    sample.successful.push(micros);
                }
                index = (index + 1) % queries.len();
            }
            samples
        });
    }
    let mut samples: Vec<Samples> = queries.iter().map(|_| Samples::default()).collect();
    while let Some(result) = tasks.join_next().await {
        for (total, client) in samples
            .iter_mut()
            .zip(result.map_err(|error| error.to_string())?)
        {
            total.append(client);
        }
    }
    let mut valid = true;
    let shapes: Vec<Value> = queries
        .iter()
        .zip(samples)
        .map(|(query, samples)| {
            let count = samples.all.len() as u64;
            valid &= count > 0
                && count == samples.successful.len() as u64 + samples.outcomes.query_timeouts;
            json!({"shape": query.shape, "endpoint": query.endpoint,
            "latency": Latency::from_micros(samples.all),
            "successful_latency": Latency::from_micros(samples.successful),
            "outcomes": samples.outcomes})
        })
        .collect();
    let report = json!({"tool": "zirv-flow-loadgen", "mode": "explorer", "clients": args.clients,
        "duration_s": args.duration, "elapsed_s": started.elapsed().as_secs_f64(),
        "request_timeout_seconds": args.request_timeout_seconds,
        "from": queries[0].body["from"], "to": queries[0].body["to"], "mix_note": MIX_NOTE,
        "latency_note": "Closed loop: one outstanding request per client, cycling through shapes with staggered starting shapes. Latency includes the entire body (CSV drained and discarded). Requests started before duration finish afterwards; latency includes all outcomes, successful_latency only complete 200 responses. No retries. Empty shapes invalidate the run.",
        "valid": valid, "shapes": shapes});
    std::fs::write(
        &args.out,
        serde_json::to_string_pretty(&report).map_err(|error| error.to_string())? + "\n",
    )
    .map_err(|error| format!("cannot write --out: {error}"))?;
    if !valid {
        return Err(
            "incomplete query mix or errors other than 503 query_timeout; see --out".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_has_the_same_ninety_day_window_and_seed_filters() {
        let to = OffsetDateTime::parse("2026-10-30T12:34:56Z", &Rfc3339).unwrap();
        // Person 1 of M8 workspace 1, using the seed's two md5 formulas.
        let person = "062dc6b1-dc0c-3aa9-3ef9-8af269921179";
        let queries = query_set(to, person).unwrap();
        let seed = include_str!("../../../scripts/bench/m4/seed.sql");
        let m8 = include_str!("../../../scripts/bench/m8/seed.sql");
        assert_eq!(queries.len(), 7);
        for query in &queries {
            let body = &query.body;
            let from = OffsetDateTime::parse(body["from"].as_str().unwrap(), &Rfc3339).unwrap();
            let end = OffsetDateTime::parse(body["to"].as_str().unwrap(), &Rfc3339).unwrap();
            assert_eq!(end, to);
            assert_eq!(end - from, time::Duration::days(90));
            let name = body["name"].as_str().unwrap();
            assert_eq!(name, "bench_order_placed");
            assert!(seed.contains(&format!("'{name}'")) && m8.contains(&format!("'{name}'")));
            assert!(body.get("source").is_none());
            if let Some(filters) = body.get("filters") {
                assert_eq!(
                    *filters,
                    json!([{"key": "category", "op": "eq", "value": "books"}])
                );
                assert!(seed.contains("'category'") && seed.contains("'books'"));
            }
            if let Some(property) = body.get("property") {
                let key = property.as_str().unwrap();
                assert!(matches!(key, "category" | "amount"));
                assert!(seed.contains(&format!("'{key}'")));
                assert!(m8.contains(&format!("\"name\":\"{key}\"")));
            }
        }
        assert_eq!(queries[2].body["person_id"], person);
        assert_eq!(queries[3].body["property"], "category");
        assert_eq!(queries[4].body["property"], "amount");
        assert_eq!(queries[5].endpoint, "list");
        assert_eq!(queries[6].endpoint, "export");
        assert!(MIX_NOTE.contains("replaced"));
    }

    #[tokio::test]
    async fn a_csv_response_is_drained_and_a_broken_body_is_not_a_success() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        crate::init_tls();
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let query = &query_set(
            OffsetDateTime::parse("2026-10-30T00:00:00Z", &Rfc3339).unwrap(),
            "seed-person",
        )
        .unwrap()[6];
        for complete in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/export", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut head = String::new();
                while !head.ends_with("\r\n\r\n") {
                    assert_ne!(socket.read_line(&mut head).await.unwrap(), 0);
                }
                assert!(head.starts_with("POST /export HTTP/1.1"));
                let lower = head.to_ascii_lowercase();
                assert!(lower.contains("authorization: bearer synthetic-key"));
                let length: usize = lower
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["name"],
                    "bench_order_placed"
                );
                let response = if complete {
                    "HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nx,y\n"
                } else {
                    "HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nx,y\n"
                };
                socket
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                socket.get_mut().shutdown().await.unwrap();
            });
            let mut outcomes = Outcomes::default();
            let success = request(&client, &url, "synthetic-key", query, &mut outcomes).await;
            server.await.unwrap();
            assert_eq!(success, complete);
            assert_eq!(outcomes.status.get(&200), Some(&1));
            assert_eq!(outcomes.body_errors, u64::from(!complete));
            assert_eq!(outcomes.query_timeouts, 0);
        }
    }

    #[test]
    fn only_a_503_with_the_problem_code_counts_as_a_query_timeout() {
        let problem = br#"{"code":"query_timeout"}"#;
        assert!(is_query_timeout(503, problem));
        assert!(!is_query_timeout(500, problem));
        assert!(!is_query_timeout(503, br#"{"code":"overloaded"}"#));
        assert!(!is_query_timeout(503, b"broken body"));
    }
}
