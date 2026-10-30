//! Debugger watchers. Receipts use UTC Unix microseconds; the SQL report joins them to the
//! durable inbox arrival copied into `events.received_at`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout_at};

use crate::pacing::retry_after;

#[derive(Debug, Parser)]
#[command(about = "Watch API debugger events and write receipts for the latency SQL report")]
pub(crate) struct Args {
    /// Base URL of a serve process; plain http:// only.
    #[arg(long)]
    url: String,
    /// File holding a workspace API key with read access.
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long)]
    workspace: String,
    /// Concurrent watchers (the server allows 20 per workspace per process).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) clients: u32,
    /// Seconds to watch, including connection and reconnect waits.
    #[arg(long)]
    duration: f64,
    /// JSONL receipts, run boundaries and one summary per client.
    #[arg(long)]
    out: PathBuf,
}

struct Frame {
    event: String,
    data: String,
}

struct Decoder {
    line: Vec<u8>,
    skip_lf: bool,
    seen_line: bool,
    event: String,
    data: String,
    id: String,
    last_event_id: String,
    retry: Duration,
}

impl Decoder {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            skip_lf: false,
            seen_line: false,
            event: String::new(),
            data: String::new(),
            id: String::new(),
            last_event_id: String::new(),
            retry: Duration::from_secs(3),
        }
    }

    fn reconnect(&mut self) {
        self.line.clear();
        self.skip_lf = false;
        self.seen_line = false;
        self.event.clear();
        self.data.clear();
        self.id.clone_from(&self.last_event_id);
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        for &byte in bytes {
            if self.skip_lf && byte == b'\n' {
                self.skip_lf = false;
                continue;
            }
            self.skip_lf = byte == b'\r';
            if matches!(byte, b'\r' | b'\n') {
                if let Some(frame) = self.end_line() {
                    frames.push(frame);
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        frames
    }

    fn end_line(&mut self) -> Option<Frame> {
        let line = String::from_utf8_lossy(&self.line);
        let line = if self.seen_line {
            line.as_ref()
        } else {
            self.seen_line = true;
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        };
        if line.is_empty() {
            self.last_event_id.clone_from(&self.id);
            let event = std::mem::take(&mut self.event);
            if self.data.is_empty() {
                return None;
            }
            self.data.pop();
            return Some(Frame {
                event: if event.is_empty() {
                    "message".into()
                } else {
                    event
                },
                data: std::mem::take(&mut self.data),
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_owned(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "id" if !value.contains('\0') => self.id = value.to_owned(),
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                if let Ok(ms) = value.parse() {
                    self.retry = Duration::from_millis(ms);
                }
            }
            _ => {}
        }
        None
    }
}

#[derive(Default, Serialize)]
struct Counts {
    items: u64,
    items_without_event_id: u64,
    gap_frames: u64,
    skipped: u64,
    resets: u64,
    last_status: Option<Value>,
    connections: u64,
    rate_limited: u64,
    transport_errors: u64,
}

impl Counts {
    /// Outer `Some` means an item; its event id can be null for a non-stored outcome.
    fn accept(&mut self, frame: Frame) -> Result<Option<Option<String>>, String> {
        if !matches!(frame.event.as_str(), "item" | "gap" | "status") {
            return Ok(None);
        }
        let body: Value = serde_json::from_str(&frame.data)
            .map_err(|error| format!("invalid {} frame: {error}", frame.event))?;
        match frame.event.as_str() {
            "item" => {
                let event_id = match body.get("event_id") {
                    Some(Value::String(id)) => Some(id.clone()),
                    Some(Value::Null) => None,
                    _ => return Err("item frame has no valid event_id field".into()),
                };
                self.items += 1;
                self.items_without_event_id += u64::from(event_id.is_none());
                Ok(Some(event_id))
            }
            "gap" => {
                let skipped = body["skipped"]
                    .as_u64()
                    .ok_or("invalid gap skipped count")?;
                let reset = body["reset"].as_bool().ok_or("invalid gap reset flag")?;
                self.gap_frames += 1;
                self.skipped += skipped;
                self.resets += u64::from(reset);
                Ok(None)
            }
            _ => {
                body["degraded"]
                    .as_bool()
                    .ok_or("invalid status degraded flag")?;
                self.last_status = Some(body);
                Ok(None)
            }
        }
    }
}

type Output = Mutex<BufWriter<File>>;

fn write_record(output: &Output, record: Value) -> Result<(), String> {
    let mut writer = output.lock().map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut *writer, &record).map_err(|error| error.to_string())?;
    writer.write_all(b"\n").map_err(|error| error.to_string())
}

fn wall_time() -> Result<u64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    u64::try_from(now.as_micros()).map_err(|error| error.to_string())
}

async fn watch(
    client: reqwest::Client,
    url: reqwest::Url,
    key: Arc<String>,
    number: u32,
    end: Instant,
    output: Arc<Output>,
) -> Result<(), String> {
    let mut decoder = Decoder::new();
    let mut counts = Counts::default();
    while Instant::now() < end {
        decoder.reconnect();
        let mut request = client
            .get(url.clone())
            .bearer_auth(key.as_str())
            .header(ACCEPT, "text/event-stream");
        if !decoder.last_event_id.is_empty() {
            request = request.header("last-event-id", &decoder.last_event_id);
        }
        let response = timeout_at(
            end.min(Instant::now() + Duration::from_secs(10)),
            request.send(),
        )
        .await;
        let mut wait = decoder.retry;
        match response {
            Ok(Ok(mut response)) => {
                let status = response.status();
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    counts.rate_limited += 1;
                    wait = retry_after(
                        response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|h| h.to_str().ok()),
                    );
                } else if status.is_server_error() {
                    counts.transport_errors += 1;
                } else if status != reqwest::StatusCode::OK {
                    return Err(format!("client {number}: debugger returned {status}"));
                } else {
                    let content_type = response
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("");
                    if !content_type
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                    {
                        return Err(format!(
                            "client {number}: debugger did not return text/event-stream"
                        ));
                    }
                    counts.connections += 1;
                    while Instant::now() < end {
                        let bytes = match timeout_at(end, response.chunk()).await {
                            Ok(Ok(Some(bytes))) => bytes,
                            Ok(Err(_)) => {
                                counts.transport_errors += 1;
                                break;
                            }
                            Ok(Ok(None)) | Err(_) => break,
                        };
                        let received_at = wall_time()?;
                        for frame in decoder.push(&bytes) {
                            if let Some(event_id) = counts.accept(frame)? {
                                write_record(
                                    &output,
                                    json!({"kind": "item", "client": number,
                                    "event_id": event_id, "received_at": received_at}),
                                )?;
                            }
                        }
                    }
                    wait = decoder.retry;
                }
            }
            _ => counts.transport_errors += 1,
        }
        let remaining = end.saturating_duration_since(Instant::now());
        sleep(wait.min(remaining)).await;
    }
    write_record(
        &output,
        json!({"kind": "client", "client": number, "counts": counts,
        "last_event_id": decoder.last_event_id}),
    )?;
    if counts.connections == 0 {
        return Err(format!("client {number}: never connected to the debugger"));
    }
    Ok(())
}

pub(crate) async fn run(args: Args) -> Result<(), String> {
    let duration = Duration::try_from_secs_f64(args.duration)
        .map_err(|_| "--duration must be finite and positive")?;
    if duration.is_zero() {
        return Err("--duration must be finite and positive".into());
    }
    let mut url = reqwest::Url::parse(&args.url).map_err(|error| error.to_string())?;
    if url.scheme() != "http" {
        return Err("--url must use plain http://".into());
    }
    url.path_segments_mut()
        .map_err(|()| "--url needs a host")?
        .pop_if_empty()
        .extend([
            "api",
            "v1",
            "workspaces",
            &args.workspace,
            "debugger",
            "stream",
        ]);
    url.set_query(Some("source=api"));
    url.set_fragment(None);
    let key = crate::read_key(&args.key_file)?;
    crate::init_tls();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    let output = Arc::new(Mutex::new(BufWriter::new(
        File::create(&args.out).map_err(|error| format!("cannot write --out: {error}"))?,
    )));
    let start = Instant::now();
    let end = start
        .checked_add(duration)
        .ok_or("--duration is too large")?;
    write_record(
        &output,
        json!({"kind": "started", "workspace": args.workspace,
        "clients": args.clients, "received_at": wall_time()?}),
    )?;
    let key = Arc::new(key);
    let mut tasks = JoinSet::new();
    for number in 1..=args.clients {
        tasks.spawn(watch(
            client.clone(),
            url.clone(),
            Arc::clone(&key),
            number,
            end,
            Arc::clone(&output),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        result.map_err(|error| error.to_string())??;
    }
    write_record(
        &output,
        json!({"kind": "finished", "received_at": wall_time()?,
        "elapsed_us": u64::try_from(start.elapsed().as_micros()).map_err(|error| error.to_string())?}),
    )?;
    output
        .lock()
        .map_err(|error| error.to_string())?
        .flush()
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_gaps_and_status_survive_every_chunk_boundary() {
        let stream = concat!(
            "\u{feff}: keep-alive\r\n\r\n",
            "event: item\r\nid: epoch:7\r\ndata: {\"event_id\":\"event-1\",\r\n",
            "data: \"name\":\"café 🦀\"}\r\n\r\n",
            ": comment\nevent: gap\ndata: {\"skipped\":4,\"reset\":true}\n\n",
            "event: status\rdata: {\"degraded\":true}\r\r",
            "event: status\ndata: {\"degraded\":false}\n\n",
            "event: item\nid: epoch:8\ndata: {\"event_id\":null}\n\n"
        );
        for split in 0..=stream.len() {
            let mut decoder = Decoder::new();
            let mut frames = decoder.push(&stream.as_bytes()[..split]);
            frames.extend(decoder.push(&stream.as_bytes()[split..]));
            assert_eq!(frames.len(), 5, "split {split}");
            assert!(frames[0].data.contains("café 🦀"));
            let mut counts = Counts::default();
            let items: Vec<_> = frames
                .into_iter()
                .filter_map(|frame| counts.accept(frame).unwrap())
                .collect();
            assert_eq!(items, [Some("event-1".into()), None]);
            assert_eq!((counts.items, counts.items_without_event_id), (2, 1));
            assert_eq!(
                (counts.gap_frames, counts.skipped, counts.resets),
                (1, 4, 1)
            );
            assert_eq!(counts.last_status, Some(json!({"degraded": false})));
            assert_eq!(decoder.last_event_id, "epoch:8");
        }
        let mut decoder = Decoder::new();
        let frames: Vec<_> = stream
            .as_bytes()
            .iter()
            .flat_map(|byte| decoder.push(&[*byte]))
            .collect();
        assert_eq!(frames.len(), 5);
        assert!(frames[0].data.contains("café 🦀"));
    }

    #[test]
    fn a_disconnect_keeps_the_last_dispatched_id_and_discards_the_partial_frame() {
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder
                .push(b"event: item\nid: epoch:7\ndata: {}\n\n")
                .len(),
            1
        );
        assert!(decoder.push(b"id: epoch:8\ndata: unfinished").is_empty());
        decoder.reconnect();
        assert_eq!(decoder.last_event_id, "epoch:7");
        let frames = decoder.push(b"event: status\ndata: {}\n\n");
        assert_eq!(frames[0].data, "{}");
        assert_eq!(decoder.last_event_id, "epoch:7");
        decoder.push(b"id: invalid\0id\n\n");
        assert_eq!(decoder.last_event_id, "epoch:7");
        decoder.push(b"id\n\n");
        assert!(decoder.last_event_id.is_empty());
    }

    #[test]
    fn retry_fields_accept_only_digits_and_survive_reconnects() {
        let mut decoder = Decoder::new();
        decoder.push(b"retry: 25\nretry: -1\nretry: 2.5\nretry:\n\n");
        decoder.reconnect();
        assert_eq!(decoder.retry, Duration::from_millis(25));
    }

    #[test]
    fn malformed_debugger_payloads_fail_instead_of_silently_losing_receipts() {
        for (event, data) in [
            ("item", "{"),
            ("item", "{}"),
            ("gap", "{\"skipped\":-1,\"reset\":false}"),
            ("status", "{}"),
        ] {
            assert!(
                Counts::default()
                    .accept(Frame {
                        event: event.into(),
                        data: data.into()
                    })
                    .is_err()
            );
        }
    }
}
