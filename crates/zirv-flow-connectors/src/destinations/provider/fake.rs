//! A local fake email provider for tests, behind `test-support` (#223): an HTTP server on
//! 127.0.0.1 that
//!
//! - honours the idempotency key (a repeated key returns the first answer and does not count a
//!   second message),
//! - can be told to answer with an error status, a 429 with `Retry-After`, or to accept a message
//!   and then answer too late (a timeout whose message was accepted),
//! - answers the domain-status request with a chosen document, and
//! - can post a signed feedback event to a URL.
//!
//! It understands what the `provider` destination sends and nothing more. It is a plain HTTP/1.1
//! server over a socket (one request per connection), so it adds no dependency to a production
//! build. It names addresses and bodies: for assertions, never for logs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hashbrown::HashMap;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use zirv_flow_core::egress::EgressPolicy;
use zirv_flow_core::signature::{Scheme, sign};

use crate::http::{GuardedClient, HttpError, HttpLimits};

/// The most bytes of one request the fake reads.
const MAX_REQUEST_BYTES: usize = 1 << 20;

/// How a fake provider behaves.
#[derive(Clone, Debug)]
pub struct FakeProviderOptions {
    /// The header that carries the idempotency key.
    pub idempotency_header: String,
    /// The document the domain-status request answers; none answers 404.
    pub domain_status: Option<Value>,
}

impl Default for FakeProviderOptions {
    fn default() -> Self {
        Self {
            idempotency_header: "Idempotency-Key".to_owned(),
            domain_status: None,
        }
    }
}

/// What the fake does with the next send request that carries a key it has not seen.
#[derive(Clone, Debug)]
pub enum Script {
    /// Answers with this status and body; the message is not accepted.
    Status {
        status: u16,
        body: Value,
        retry_after: Option<u64>,
    },
    /// Accepts the message, then waits this long before it answers: a client with a shorter
    /// timeout loses the reply of a message the provider holds.
    HangAfterAccept(Duration),
    /// Accepts the message, then closes the connection without answering: the reply of a message
    /// the provider holds is lost at once.
    DropAfterAccept,
}

/// A message the fake accepted.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// The idempotency key.
    pub key: String,
    /// How many idempotency headers the request carried.
    pub key_headers: usize,
    /// The HTTP method of the request.
    pub method: String,
    /// The path as it arrived on the wire, percent-encoding included.
    pub path: String,
    pub body: Value,
    /// The `Authorization` header, if any.
    pub authorization: Option<String>,
    /// The `X-Api-Key` header, if any.
    pub api_key: Option<String>,
}

#[derive(Default)]
struct State {
    idempotency_header: String,
    domain_status: Option<Value>,
    script: VecDeque<Script>,
    /// The first answer to each key: status and body text.
    answers: HashMap<String, (u16, String)>,
    accepted: Vec<Accepted>,
    requests: usize,
    domain_queries: Vec<String>,
    /// When set, the domain-status request is answered with a redirect to this location.
    domain_redirect: Option<String>,
}

/// A running fake provider. The server stops when this is dropped.
pub struct FakeProvider {
    base_url: String,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl FakeProvider {
    /// Starts a fake provider on an unused port of 127.0.0.1.
    pub async fn start(options: FakeProviderOptions) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback port is free");
        let base_url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("a bound socket has an address")
        );
        let state = Arc::new(Mutex::new(State {
            idempotency_header: options.idempotency_header,
            domain_status: options.domain_status,
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self {
            base_url,
            state,
            task,
        }
    }

    /// `http://127.0.0.1:port`: put it where a manifest has its base URL.
    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    /// Queues what the fake does with a later send request of a new key, in order.
    pub fn script(&self, step: Script) {
        self.lock().script.push_back(step);
    }

    /// Makes the domain-status request answer a redirect to `location`, or stops doing so.
    pub fn set_domain_redirect(&self, location: Option<&str>) {
        self.lock().domain_redirect = location.map(str::to_owned);
    }

    /// Replaces the document the domain-status request answers.
    pub fn set_domain_status(&self, document: Option<Value>) {
        self.lock().domain_status = document;
    }

    /// The messages accepted, one per distinct key.
    pub fn accepted(&self) -> Vec<Accepted> {
        self.lock().accepted.clone()
    }

    /// How many send requests arrived, repeated keys included.
    pub fn requests(&self) -> usize {
        self.lock().requests
    }

    /// The raw query string of each domain-status request.
    pub fn domain_queries(&self) -> Vec<String> {
        self.lock().domain_queries.clone()
    }

    /// Posts `event` to `url` as this provider's feedback webhook would: JSON, signed under
    /// `secret` by `scheme` at `timestamp` (unix seconds). Returns the HTTP status of the answer.
    pub async fn post_feedback(
        &self,
        url: &str,
        scheme: &Scheme<'_>,
        secret: &str,
        event: &Value,
        timestamp: i64,
    ) -> Result<u16, HttpError> {
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).map_err(|_| {
            HttpError::Transport("the fake provider's egress policy is invalid".to_owned())
        })?;
        let client = GuardedClient::new(
            Arc::new(policy),
            HttpLimits {
                max_redirects: 0,
                ..HttpLimits::default()
            },
        )?;
        let body = event.to_string().into_bytes();
        let headers = sign_feedback(scheme, secret, timestamp, &body);
        let borrowed: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let response = client.post(url, &borrowed, body).await?;
        Ok(response.status().as_u16())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The headers a provider sends with a signed feedback event: the content type, the signature
/// (the scheme's prefix and the digest) and, for a timestamped scheme, the timestamp.
pub fn sign_feedback(
    scheme: &Scheme<'_>,
    secret: &str,
    timestamp: i64,
    body: &[u8],
) -> Vec<(String, String)> {
    let stamp = scheme.timestamp_header.map(|_| timestamp);
    let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    headers.push((
        scheme.signature_header.to_owned(),
        format!(
            "{}{}",
            scheme.signature_prefix,
            sign(secret.as_bytes(), stamp, body)
        ),
    ));
    if let Some(name) = scheme.timestamp_header {
        headers.push((name.to_owned(), timestamp.to_string()));
    }
    headers
}

/// Reads one request: the head up to the blank line, then `Content-Length` bytes.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Some(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    })
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str, retry_after: Option<u64>) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    if let Some(seconds) = retry_after {
        head.push_str(&format!("retry-after: {seconds}\r\n"));
    }
    head.push_str("\r\n");
    // The client may be gone already (a timeout): nothing to do about it.
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// What to do with one request, decided under the lock.
enum Plan {
    Answer(u16, String, Option<u64>),
    AnswerLate(Duration, u16, String),
    Drop,
    Redirect(String),
}

fn decide(state: &Mutex<State>, request: &Request) -> Plan {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if request.method == "GET" {
        state.domain_queries.push(request.query.clone());
        if let Some(location) = state.domain_redirect.clone() {
            return Plan::Redirect(location);
        }
        return match state.domain_status.clone() {
            Some(document) => Plan::Answer(200, document.to_string(), None),
            None => Plan::Answer(404, json!({"error": "not found"}).to_string(), None),
        };
    }
    state.requests += 1;
    let key = request
        .header(&state.idempotency_header)
        .unwrap_or_default()
        .to_owned();
    if key.is_empty() {
        return Plan::Answer(
            400,
            json!({"error": "no idempotency key"}).to_string(),
            None,
        );
    }
    if let Some((status, body)) = state.answers.get(&key) {
        return Plan::Answer(*status, body.clone(), None);
    }
    let step = state.script.pop_front();
    if let Some(Script::Status {
        status,
        body,
        retry_after,
    }) = &step
    {
        return Plan::Answer(*status, body.to_string(), *retry_after);
    }
    let body = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let remote = format!("remote-{}", state.accepted.len() + 1);
    let answer = json!({"id": remote}).to_string();
    let key_headers = request
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(&state.idempotency_header))
        .count();
    state.accepted.push(Accepted {
        key: key.clone(),
        key_headers,
        method: request.method.clone(),
        path: request.path.clone(),
        body,
        authorization: request.header("authorization").map(str::to_owned),
        api_key: request.header("x-api-key").map(str::to_owned),
    });
    state.answers.insert(key, (200, answer.clone()));
    match step {
        Some(Script::HangAfterAccept(wait)) => Plan::AnswerLate(wait, 200, answer),
        Some(Script::DropAfterAccept) => Plan::Drop,
        _ => Plan::Answer(200, answer, None),
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    match decide(&state, &request) {
        Plan::Answer(status, body, retry_after) => {
            respond(&mut stream, status, &body, retry_after).await;
        }
        Plan::AnswerLate(wait, status, body) => {
            tokio::time::sleep(wait).await;
            respond(&mut stream, status, &body, None).await;
        }
        // Dropping the stream closes the connection with nothing sent.
        Plan::Drop => {}
        Plan::Redirect(location) => {
            let head = format!(
                "HTTP/1.1 302 X\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use zirv_flow_core::signature::verify;

    use super::*;

    async fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> (u16, Value) {
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        let client = GuardedClient::new(Arc::new(policy), HttpLimits::default()).unwrap();
        let response = client
            .post(url, headers, body.to_string().into_bytes())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.body().await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn a_repeated_key_returns_the_first_answer_and_counts_one_message() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let url = format!("{}/send", fake.base_url());
        let headers = [
            ("Idempotency-Key", "k-1"),
            ("Content-Type", "application/json"),
        ];

        let first = post_json(&url, &headers, &json!({"n": 1})).await;
        let again = post_json(&url, &headers, &json!({"n": 2})).await;
        let other = post_json(&url, &[("Idempotency-Key", "k-2")], &json!({"n": 3})).await;

        assert_eq!(first, again);
        assert_eq!(first.0, 200);
        assert_ne!(first.1, other.1);
        assert_eq!(fake.requests(), 3);
        let accepted = fake.accepted();
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].body, json!({"n": 1}), "the first body is kept");
    }

    #[tokio::test]
    async fn a_request_without_a_key_is_refused_and_a_scripted_status_is_not_accepted() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let url = format!("{}/send", fake.base_url());
        fake.script(Script::Status {
            status: 503,
            body: json!({"error": "busy"}),
            retry_after: Some(2),
        });

        let keyless = post_json(&url, &[], &json!({})).await;
        let scripted = post_json(&url, &[("Idempotency-Key", "k-1")], &json!({})).await;
        let after = post_json(&url, &[("Idempotency-Key", "k-1")], &json!({})).await;

        assert_eq!(keyless.0, 400);
        assert_eq!(scripted, (503, json!({"error": "busy"})));
        assert_eq!(
            after.0, 200,
            "the script was used up and the key was not kept"
        );
        assert_eq!(fake.accepted().len(), 1);
    }

    #[tokio::test]
    async fn the_domain_status_request_is_recorded_and_answers_the_document_or_404() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        let client = GuardedClient::new(Arc::new(policy), HttpLimits::default()).unwrap();
        let get = |query: &str| {
            let url = format!("{}/domains?{query}", fake.base_url());
            let client = client.clone();
            async move {
                client
                    .execute(reqwest::Request::new(
                        reqwest::Method::GET,
                        url.parse().unwrap(),
                    ))
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            }
        };

        assert_eq!(get("domain=a.test").await, 404);
        fake.set_domain_status(Some(json!({"records": []})));
        assert_eq!(get("domain=b.test").await, 200);
        assert_eq!(fake.domain_queries(), ["domain=a.test", "domain=b.test"]);
        assert_eq!(fake.requests(), 0, "a domain request is not a send");
    }

    #[tokio::test]
    async fn signed_feedback_reaches_a_url_and_verifies_under_the_scheme() {
        type Seen = Arc<StdMutex<Option<(HeaderMap, Vec<u8>)>>>;
        async fn receive(State(seen): State<Seen>, headers: HeaderMap, body: Bytes) {
            *seen.lock().unwrap() = Some((headers, body.to_vec()));
        }
        let seen = Seen::default();
        let app = Router::new()
            .route("/feedback", post(receive))
            .with_state(Arc::clone(&seen));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/feedback", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let scheme = Scheme {
            signature_header: "X-Sig",
            signature_prefix: "sha256=",
            timestamp_header: Some("X-Ts"),
            tolerance_seconds: 300,
        };
        let event = json!({"event": "bounced"});

        let status = fake
            .post_feedback(&url, &scheme, "whsec", &event, 1_700_000_000)
            .await
            .unwrap();

        assert_eq!(status, 200);
        let (headers, body) = seen.lock().unwrap().take().unwrap();
        let lookup = |name: &str| {
            headers
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .collect::<Vec<_>>()
        };
        assert!(verify(&scheme, b"whsec", &lookup, &body, 1_700_000_100).is_ok());
        assert!(verify(&scheme, b"other", &lookup, &body, 1_700_000_100).is_err());
        assert!(
            verify(&scheme, b"whsec", &lookup, &body, 1_700_001_000).is_err(),
            "a replayed old timestamp is outside the tolerance"
        );
    }

    #[tokio::test]
    async fn a_dropped_reply_still_counts_the_accepted_message_once_under_its_key() {
        let fake = FakeProvider::start(FakeProviderOptions::default()).await;
        let url = format!("{}/send", fake.base_url());
        let policy = EgressPolicy::parse(&["127.0.0.0/8".to_owned()]).unwrap();
        let client = GuardedClient::new(Arc::new(policy), HttpLimits::default()).unwrap();
        fake.script(Script::DropAfterAccept);

        let lost = client
            .post(&url, &[("Idempotency-Key", "k-1")], b"{}".to_vec())
            .await;
        let retried = post_json(&url, &[("Idempotency-Key", "k-1")], &json!({})).await;

        assert!(lost.is_err(), "no reply came");
        assert_eq!(retried.0, 200);
        assert_eq!(fake.requests(), 2);
        assert_eq!(fake.accepted().len(), 1);
    }

    #[test]
    fn a_body_only_scheme_sends_no_timestamp_header() {
        let scheme = Scheme {
            signature_header: "X-Sig",
            signature_prefix: "",
            timestamp_header: None,
            tolerance_seconds: 300,
        };

        let headers = sign_feedback(&scheme, "whsec", 5, b"{}");

        assert_eq!(headers.len(), 2);
        assert_eq!(headers[1].0, "X-Sig");
        assert_eq!(headers[1].1, sign(b"whsec", None, b"{}"));
    }
}
