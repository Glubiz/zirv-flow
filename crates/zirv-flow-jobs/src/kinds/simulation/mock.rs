//! The destination of the generated webhook nodes: an in-process axum server on a loopback port
//! that keeps every request it receives, so the checks can compare what each retry sent, and
//! answers from the seed.
//!
//! Each call belongs to a seeded class, by a name the driver gives it from the person and the
//! node (the idempotency key holds random ids, and a seed must name the same scenario every run), and the answer to a send is a
//! function of the class and of how many times the call was sent before, so a seed gives the
//! same answers whatever the order of the sends:
//!
//! - **down** (6%): every send is answered with a 500, 502 or 503, so the call is retried until
//!   its last attempt records a failure;
//! - **refusing** (3%): every send is answered with a 400, 404 or 410, which fails the call at
//!   once;
//! - **flaky** (14%): the first one to three sends are answered with a retryable status (500,
//!   502, 503, 408 or 429), the rest with a 200;
//! - **healthy** (the rest): a 200, with the odd retryable status or a 302 (terminal, since the
//!   client follows no redirects).
//!
//! The client's own timeout, 30 s for the whole exchange, is not exercised: a destination that
//! answers slower than that would hold the run for as long, which no test budget allows.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use hashbrown::HashMap;
use zirv_flow_core::flow::IDEMPOTENCY_HEADER;
use zirv_flow_engine::sim::rng::mix;

use super::driver::hash_text;

/// The stream of the seed the destination draws from.
const LANE_DESTINATION: u64 = 30;
/// The statuses a destination answers when it is overloaded or failing; each is retried.
const RETRYABLE: [u16; 5] = [500, 502, 503, 408, 429];

/// One request the destination received, and the status it answered with.
#[derive(Clone, Debug)]
pub(super) struct Hit {
    pub(super) key: String,
    pub(super) body: Vec<u8>,
    pub(super) status: u16,
}

/// The destination's log, shared with its server.
#[derive(Clone)]
pub(super) struct Mock {
    seed: u64,
    hits: Arc<Mutex<Vec<Hit>>>,
    /// A name per call that does not depend on random ids, by idempotency key.
    names: Arc<Mutex<HashMap<String, u64>>>,
}

impl Mock {
    pub(super) fn new(seed: u64) -> Self {
        Self {
            seed,
            hits: Arc::default(),
            names: Arc::default(),
        }
    }

    /// Names the call under `key` before its first send, so its class follows the seed.
    pub(super) fn name(&self, key: &str, name: u64) {
        self.names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), name);
    }

    pub(super) fn hits(&self) -> Vec<Hit> {
        self.hits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The body of the latest request that carried `key`.
    pub(super) fn last_body(&self, key: &str) -> Option<Vec<u8>> {
        self.hits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|hit| hit.key == key)
            .map(|hit| hit.body.clone())
    }

    /// Starts the server on a free loopback port and returns the port.
    pub(super) async fn serve(&self) -> u16 {
        let router = Router::new()
            .route("/hook", post(receive))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        port
    }
}

/// The status the destination answers the `nth` send (from 0) of the call named `name` with.
pub(super) fn answer(seed: u64, name: u64, nth: usize) -> u16 {
    let call = mix(seed ^ LANE_DESTINATION ^ name);
    let class = call % 100;
    let roll = usize::try_from(mix(call ^ (nth as u64 + 1)) % 100).unwrap_or(0);
    let retryable = RETRYABLE[roll % RETRYABLE.len()];
    match class {
        0..=5 => [500, 502, 503][roll % 3],
        6..=8 => [400, 404, 410][roll % 3],
        9..=22 => {
            let failing = 1 + usize::try_from(class % 3).unwrap_or(0);
            if nth < failing { retryable } else { 200 }
        }
        _ => match roll {
            0..=4 => retryable,
            5 => 302,
            _ => 200,
        },
    }
}

async fn receive(State(mock): State<Mock>, headers: HeaderMap, body: Bytes) -> Response {
    let key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    // Counting the earlier sends and recording this one is one step, so concurrent sends of one
    // call get different positions.
    let (status, name, nth_for_wait) = {
        let mut hits = mock.hits.lock().unwrap_or_else(PoisonError::into_inner);
        let nth = hits.iter().filter(|hit| hit.key == key).count();
        let name = mock
            .names
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .copied()
            .unwrap_or_else(|| hash_text(&key));
        let status = answer(mock.seed, name, nth);
        hits.push(Hit {
            key,
            body: body.to_vec(),
            status,
        });
        (status, name, nth as u64)
    };
    let mut response = StatusCode::from_u16(status)
        .unwrap_or(StatusCode::OK)
        .into_response();
    if status == 429 {
        // Longer than the worker's backoff at the attempts a call has, so that honouring it is
        // visible: 60 to 299 s.
        let wait = 60 + mix(name ^ nth_for_wait) % 240;
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&wait.to_string()).unwrap_or(HeaderValue::from_static("60")),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn the_answers_are_a_function_of_the_seed_the_call_and_the_send() {
        for nth in 0..4 {
            assert_eq!(answer(7, 99, nth), answer(7, 99, nth));
        }
    }

    #[test]
    fn every_class_of_destination_shows_up_across_calls() {
        let mut first = BTreeSet::new();
        let mut later = BTreeSet::new();
        for call in 0..400 {
            let name = hash_text(&format!("call {call}"));
            first.insert(answer(11, name, 0));
            later.insert(answer(11, name, 5));
        }
        // A server error, a refusal, a rate limit and a success are all answered.
        for status in [200, 400, 429, 503] {
            assert!(
                first.contains(&status),
                "{status} is never answered: {first:?}"
            );
        }
        assert!(later.contains(&200) && later.contains(&503));
    }
}
