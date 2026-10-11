use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub(crate) const UNMATCHED: &str = "unmatched";

pub(crate) async fn render(State(state): State<AppState>) -> Response {
    zirv_flow_store::observe_pool(&state.pool, "main");
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        (state.render_metrics)(),
    )
        .into_response()
}

/// Records request count and latency, labelled by the matched route template so label
/// cardinality stays bounded.
pub(crate) async fn record(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = method_label(request.method());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| UNMATCHED.to_owned(), |path| path.as_str().to_owned());

    let response = next.run(request).await;

    metrics::counter!(
        "zirv_flow_http_requests_total",
        "method" => method,
        "route" => route.clone(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "zirv_flow_http_request_duration_seconds",
        "method" => method,
        "route" => route,
    )
    .record(started.elapsed().as_secs_f64());
    response
}

/// Clients may send any method token, so only the standard methods get their own label.
pub(crate) fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => "OTHER",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_methods_keep_their_label_and_others_share_one() {
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(method_label(&Method::DELETE), "DELETE");
        assert_eq!(
            method_label(&Method::from_bytes(b"FOO1234").unwrap()),
            "OTHER"
        );
    }
}
