use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub(crate) const UNMATCHED: &str = "unmatched";

pub(crate) async fn render(State(state): State<AppState>) -> Response {
    respond(&state.pool, &state.render_metrics)
}

/// The exposition text, after refreshing the pool gauges it includes.
pub(crate) fn respond(
    pool: &zirv_flow_store::PgPool,
    render: &std::sync::Arc<dyn Fn() -> String + Send + Sync>,
) -> Response {
    zirv_flow_store::observe_pool(pool, "main");
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        render(),
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
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use super::*;

    /// The metric names passed as the first literal of a `counter!`, `gauge!` or `histogram!`
    /// call in `source`.
    fn series_in(source: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for macro_name in ["counter!(", "gauge!(", "histogram!("] {
            for (at, _) in source.match_indices(macro_name) {
                let rest = source[at + macro_name.len()..].trim_start();
                let Some(rest) = rest.strip_prefix("\"zirv_flow_") else {
                    continue;
                };
                if let Some(end) = rest.find('"') {
                    found.insert(format!("zirv_flow_{}", &rest[..end]));
                }
            }
        }
        found
    }

    fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Every series emitted anywhere under `crates/`.
    fn series_in_crates() -> BTreeSet<String> {
        let mut files = Vec::new();
        rust_files(&root().join("crates"), &mut files);
        files
            .iter()
            .flat_map(|path| series_in(&std::fs::read_to_string(path).unwrap()))
            .collect()
    }

    /// The `zirv_flow_` names that appear in `text`, `prefix` excluded.
    fn names_in(text: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for (at, _) in text.match_indices("zirv_flow_") {
            let name: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if name.len() > "zirv_flow_".len() && !name.ends_with('_') {
                found.insert(name);
            }
        }
        found
    }

    /// The series of `emitted` that no table row of `reference` names.
    fn missing_from_table(emitted: &BTreeSet<String>, reference: &str) -> Vec<String> {
        let table: String = reference
            .lines()
            .filter(|line| line.starts_with('|'))
            .collect::<Vec<_>>()
            .join("\n");
        let listed = names_in(&table);
        emitted.difference(&listed).cloned().collect()
    }

    #[test]
    fn every_series_the_code_emits_is_in_the_runtime_table() {
        let emitted = series_in_crates();
        assert!(emitted.len() > 50, "the scan found only {emitted:?}");
        let reference = std::fs::read_to_string(root().join("docs/operations/runtime.md")).unwrap();

        let missing = missing_from_table(&emitted, &reference);

        assert!(
            missing.is_empty(),
            "the runtime metric table lacks these series: {}",
            missing.join(", ")
        );
    }

    #[test]
    fn every_series_the_architecture_row_names_is_emitted() {
        let emitted = series_in_crates();
        let architecture = std::fs::read_to_string(root().join("docs/architecture.md")).unwrap();
        let row = architecture
            .lines()
            .find(|line| line.starts_with("| Metrics |"))
            .expect("the architecture 17 Metrics row");

        let unknown: Vec<String> = names_in(row)
            .into_iter()
            .filter(|name| !emitted.contains(name))
            .collect();

        assert!(
            unknown.is_empty(),
            "architecture 17 names series the code does not emit: {}",
            unknown.join(", ")
        );
    }

    #[test]
    fn the_drift_guard_names_a_series_the_table_lacks() {
        let call = format!(
            "{}!(
    \"zirv_flow_{}\",
    \"a\" => b,
)",
            "counter", "drifted_total"
        );
        let other = format!("{}!(\"zirv_flow_{}\")", "gauge", "listed");
        let ignored = format!("{}!(\"not_ours\")", "gauge");
        let emitted = series_in(&format!(
            "{call}
{other}
{ignored}"
        ));
        assert_eq!(
            emitted.iter().map(String::as_str).collect::<Vec<_>>(),
            ["zirv_flow_drifted_total", "zirv_flow_listed"]
        );

        let reference = "| `zirv_flow_listed` | gauge |
prose about zirv_flow_drifted_total
";

        assert_eq!(
            missing_from_table(&emitted, reference),
            ["zirv_flow_drifted_total"]
        );
    }

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
