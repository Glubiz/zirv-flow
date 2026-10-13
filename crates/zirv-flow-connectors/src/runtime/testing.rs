//! Helpers the runtime tests share: an in-process mock upstream and a context that may reach it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use url::Url;
use zirv_flow_core::egress::EgressPolicy;

use super::{RateLimits, RetryPolicy};
use crate::http::HttpLimits;
use crate::sources::SourceContext;

/// What a mock upstream saw of one request.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path_and_query: String,
    pub(crate) headers: HeaderMap,
}

/// A mock upstream on a loopback port, with a log of the requests it served.
pub(crate) struct Mock {
    pub(crate) port: u16,
    pub(crate) hits: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    pub(crate) fn base(&self) -> Url {
        self.url("/")
    }

    pub(crate) fn url(&self, path: &str) -> Url {
        format!("http://127.0.0.1:{}{path}", self.port)
            .parse()
            .unwrap()
    }

    pub(crate) fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Serves `router` on a free loopback port.
pub(crate) async fn serve(router: Router) -> Mock {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = router.layer(axum::middleware::from_fn_with_state(
        (Arc::clone(&hits), Arc::clone(&seen)),
        |State((hits, seen)): State<(Arc<AtomicUsize>, Arc<Mutex<Vec<Seen>>>)>,
         request: Request,
         next: Next| async move {
            hits.fetch_add(1, Ordering::SeqCst);
            seen.lock().unwrap().push(Seen {
                method: request.method().to_string(),
                path_and_query: request
                    .uri()
                    .path_and_query()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                headers: request.headers().clone(),
            });
            next.run(request).await
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Mock { port, hits, seen }
}

/// A port nothing listens on.
pub(crate) async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// The retry policy tests use: the real attempt count, with waits short enough to run.
pub(crate) fn fast_retries() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 5,
        base: Duration::from_millis(5),
        cap: Duration::from_millis(20),
        max_wait: Duration::from_secs(300),
    }
}

/// A context for source `source_id` of type `kind` that may reach loopback, which is allowlisted
/// in this test policy only.
pub(crate) fn context(kind: &'static str, source_id: &str) -> SourceContext {
    context_sharing(kind, source_id, Arc::default())
}

pub(crate) fn context_sharing(
    kind: &'static str,
    source_id: &str,
    rate_limits: Arc<RateLimits>,
) -> SourceContext {
    let policy = Arc::new(EgressPolicy::parse(&["127.0.0.1/32".to_owned()]).unwrap());
    SourceContext::new(policy, HttpLimits::default(), kind, source_id, rate_limits)
        .unwrap()
        .with_retry_policy(fast_retries())
}
