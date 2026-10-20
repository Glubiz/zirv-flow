//! The destination of the generated webhook nodes: an in-process axum server on a loopback port
//! that answers 200 and keeps every request it receives, so the checks can compare what each
//! retry sent.

use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use zirv_flow_core::flow::IDEMPOTENCY_HEADER;

/// One request the destination received.
#[derive(Clone, Debug)]
pub(super) struct Hit {
    pub(super) key: String,
    pub(super) body: Vec<u8>,
}

/// The destination's log, shared with its server.
#[derive(Clone, Default)]
pub(super) struct Mock {
    hits: Arc<Mutex<Vec<Hit>>>,
}

impl Mock {
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

async fn receive(State(mock): State<Mock>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let key = headers
        .get(IDEMPOTENCY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    mock.hits
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Hit {
            key,
            body: body.to_vec(),
        });
    StatusCode::OK
}
