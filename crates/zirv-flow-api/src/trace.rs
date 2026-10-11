//! Request tracing: a fresh trace per request, echoed as `x-trace-id`, with a span around
//! the request and one event when it completes.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use tracing::{Instrument, debug, info, info_span};
use zirv_flow_core::{Cause, TraceContext};

use crate::{metrics, request_id};

pub(crate) const HEADER: HeaderName = HeaderName::from_static("x-trace-id");

tokio::task_local! {
    static TRACE: TraceContext;
}

/// The trace of the request being handled, if inside the middleware.
pub(crate) fn current() -> Option<TraceContext> {
    TRACE.try_with(Clone::clone).ok()
}

#[cfg(test)]
pub(crate) async fn with_trace<F: std::future::Future>(
    trace: TraceContext,
    future: F,
) -> F::Output {
    TRACE.scope(trace, future).await
}

/// Routes polled so often that logging each completion at `info` would drown the rest.
fn is_chatty(route: &str) -> bool {
    matches!(route, "/healthz" | "/readyz" | "/metrics")
}

/// Starts a trace for the request. An incoming trace header is deliberately ignored: a
/// client must not be able to graft its requests onto another trace.
pub(crate) async fn assign(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = request_id::current().unwrap_or_default();
    let trace = TraceContext::root(Cause::api(request_id.clone()));
    let method = metrics::method_label(request.method());
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || metrics::UNMATCHED.to_owned(),
        |path| path.as_str().to_owned(),
    );
    let span = info_span!(
        "http.request",
        request_id = %request_id,
        trace_id = %trace.trace_id,
        method,
        route = %route,
    );
    let header = HeaderValue::from_str(&trace.trace_id.to_string()).ok();

    async move {
        let mut response = TRACE.scope(trace, next.run(request)).await;
        let status = response.status().as_u16();
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if is_chatty(&route) {
            debug!(status, duration_ms, "request completed");
        } else {
            info!(status, duration_ms, "request completed");
        }
        if let Some(header) = header {
            response.headers_mut().insert(HEADER, header);
        }
        response
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    fn app() -> Router {
        Router::new()
            .route(
                "/things/{id}",
                get(|| async { current().unwrap().trace_id.to_string() }),
            )
            .route("/healthz", get(|| async { "ok" }))
            .layer(from_fn(assign))
            .layer(from_fn(request_id::assign))
    }

    async fn call(app: &Router, request: Request<Body>) -> (axum::http::HeaderMap, String) {
        let response = app.clone().oneshot(request).await.unwrap();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn every_request_gets_a_fresh_trace_that_the_handler_sees_and_the_response_echoes() {
        let app = app();

        let (first, seen) =
            call(&app, Request::get("/things/1").body(Body::empty()).unwrap()).await;
        let (second, _) = call(&app, Request::get("/things/2").body(Body::empty()).unwrap()).await;

        assert_eq!(first[&HEADER].to_str().unwrap(), seen);
        assert_eq!(seen.len(), 32);
        assert_ne!(first[&HEADER], second[&HEADER]);
    }

    #[tokio::test]
    async fn an_incoming_trace_header_is_ignored() {
        let incoming = "0123456789abcdef0123456789abcdef";
        let request = Request::get("/things/1")
            .header("x-trace-id", incoming)
            .header("traceparent", format!("00-{incoming}-0123456789abcdef-01"))
            .body(Body::empty())
            .unwrap();

        let (headers, seen) = call(&app(), request).await;

        assert_ne!(headers[&HEADER].to_str().unwrap(), incoming);
        assert_ne!(seen, incoming);
    }

    #[test]
    fn outside_the_middleware_there_is_no_current_trace() {
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn the_completion_line_has_the_route_template_and_both_ids_and_probes_stay_quiet() {
        let captured = Captured::default();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::INFO)
                .with_writer(captured.clone())
                .finish(),
        );
        let app = app();

        let (headers, _) = call(
            &app,
            Request::get("/things/secret-1234")
                .header("x-request-id", "req-7")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        call(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;

        let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{output}");
        let line = &lines[0];
        assert_eq!(line["fields"]["message"], "request completed");
        assert_eq!(line["fields"]["status"], 200);
        assert!(line["fields"]["duration_ms"].is_u64());
        assert_eq!(line["span"]["route"], "/things/{id}");
        assert_eq!(line["span"]["method"], "GET");
        assert_eq!(line["span"]["request_id"], "req-7");
        assert_eq!(
            line["span"]["trace_id"].as_str().unwrap(),
            headers[&HEADER].to_str().unwrap()
        );
        assert!(!output.contains("secret-1234"));
    }
}
