//! The request for the first page of a stream, from the manifest.

use url::Url;

use super::super::error::SourceError;
use crate::manifest::{Manifest, PaginationSpec, StreamSpec};
use crate::runtime::{Pagination, RequestSpec};

fn invalid() -> SourceError {
    SourceError::Config {
        field: "manifest".to_owned(),
        reason: "does not make a valid request URL".to_owned(),
    }
}

/// `base` with `path` appended to its own path (`/v1` + `/contacts` is `/v1/contacts`).
fn joined(base: &Url, path: &str) -> Result<Url, SourceError> {
    let mut url = base.clone();
    let prefix = base.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{path}"));
    url.set_query(None);
    url.set_fragment(None);
    if url.cannot_be_a_base() {
        return Err(invalid());
    }
    Ok(url)
}

/// The first request of `stream`: its static query, the manifest's static headers, and the
/// incremental parameter when there is a `since`. The auth is added by the runtime.
pub(super) fn first(
    manifest: &Manifest,
    stream: &StreamSpec,
    since: Option<&str>,
) -> Result<RequestSpec, SourceError> {
    let url = joined(&manifest.base_url, &stream.path)?;
    let mut request = RequestSpec::get(&manifest.base_url, url);
    for (name, value) in &stream.query {
        request = request.query(name, value);
    }
    if let (Some(since), Some(incremental)) = (since, &stream.incremental) {
        request = request.query(&incremental.request_param, since);
    }
    for (name, value) in &manifest.headers {
        request = request.header(name, value);
    }
    Ok(request)
}

/// The runtime's pagination style for a stream, borrowing the manifest.
pub(super) fn pagination(stream: &StreamSpec) -> Pagination<'_> {
    match &stream.pagination {
        PaginationSpec::None => Pagination::None,
        PaginationSpec::Cursor { param, next } => Pagination::Cursor { param, next },
        PaginationSpec::Offset {
            param,
            limit_param,
            limit,
        } => Pagination::Offset {
            param,
            limit_param,
            limit: *limit,
        },
        PaginationSpec::Page { param, first, size } => Pagination::Page {
            param,
            first: *first,
            size_param: size.as_ref().map(|(name, size)| (name.as_str(), *size)),
        },
        PaginationSpec::LinkHeader => Pagination::LinkHeader,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::validate;

    fn manifest(base: &str) -> Manifest {
        validate(&json!({
            "version": "v1",
            "base_url": base,
            "auth": {"type": "none"},
            "headers": {"Accept": "application/json"},
            "streams": [{
                "name": "s", "path": "/contacts", "query": {"limit": 2, "q": "a b"},
                "fields": [{"name": "id", "type": "string"}, {"name": "at", "type": "timestamp"}],
                "primary_key": ["id"],
                "incremental": {"cursor_field": "at", "request_param": "since",
                                "cursor_type": "timestamp"},
                "pagination": {"type": "page", "param": "page", "first": 1,
                               "size_param": "per", "size": 5}
            }],
        }))
        .unwrap()
    }

    #[test]
    fn the_stream_path_is_appended_to_the_base_path() {
        for (base, expected) in [
            (
                "https://api.test",
                "https://api.test/contacts?limit=2&q=a+b",
            ),
            (
                "https://api.test/",
                "https://api.test/contacts?limit=2&q=a+b",
            ),
            (
                "https://api.test/v1",
                "https://api.test/v1/contacts?limit=2&q=a+b",
            ),
            (
                "https://api.test/v1/",
                "https://api.test/v1/contacts?limit=2&q=a+b",
            ),
        ] {
            let manifest = manifest(base);

            let request = first(&manifest, &manifest.streams[0], None).unwrap();

            assert_eq!(request.url().as_str(), expected);
        }
    }

    #[test]
    fn the_since_parameter_is_sent_only_when_there_is_a_checkpoint() {
        let manifest = manifest("https://api.test");

        let with = first(
            &manifest,
            &manifest.streams[0],
            Some("2026-01-01T00:00:00Z"),
        )
        .unwrap();
        let without = first(&manifest, &manifest.streams[0], None).unwrap();

        assert!(
            with.url()
                .query()
                .unwrap()
                .contains("since=2026-01-01T00%3A00%3A00Z")
        );
        assert!(!without.url().query().unwrap().contains("since"));
    }

    #[test]
    fn the_pagination_maps_to_the_runtime_style() {
        let manifest = manifest("https://api.test");

        assert_eq!(
            pagination(&manifest.streams[0]),
            Pagination::Page {
                param: "page",
                first: 1,
                size_param: Some(("per", 5))
            }
        );
    }
}
