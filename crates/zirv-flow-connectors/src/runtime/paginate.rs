//! Pagination drivers: one stream of pages for each style an HTTP API uses, so no connector
//! writes its own loop. Every page is fetched with [`SourceContext::send`], so each one gets the
//! auth, the retries, the rate limit and the origin binding.

use std::collections::HashSet;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use reqwest::header::LINK;
use serde_json::Value;
use url::Url;

use super::auth::Auth;
use super::error::RuntimeError;
use super::send::{RequestSpec, RuntimeResponse};
use crate::sources::SourceContext;

/// The most pages one stream reads. An API that pages without end, or that ignores the offset or
/// page parameter and sends the same full page forever, ends here with
/// [`RuntimeError::PaginationLoop`].
const MAX_PAGES: usize = 100_000;

/// How an API pages its results. The parameter names are the API's own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pagination<'a> {
    /// The response holds the next cursor at the `next` JSON pointer, and the request passes it
    /// back as the `param` query parameter. The end is a missing, null or empty cursor.
    Cursor { param: &'a str, next: &'a str },
    /// The request says where to start (`param`, from 0) and how many (`limit_param`). The end is
    /// a page with fewer than `limit` items.
    Offset {
        param: &'a str,
        limit_param: &'a str,
        limit: u32,
    },
    /// The request names the page (`param`, from `first`) and optionally its size. The end is an
    /// empty page, or with a size, a short one.
    Page {
        param: &'a str,
        first: u32,
        size_param: Option<(&'a str, u32)>,
    },
    /// The next page is the `rel="next"` target of the `Link` header (RFC 8288). The end is a
    /// response without one.
    LinkHeader,
}

/// One page: the parsed body and the items found in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// The body with the items moved out into [`Page::items`], so they are not held twice.
    pub body: Value,
    pub items: Vec<Value>,
}

/// Reads the pages of `first` to the end, in order. `items` is the JSON pointer of the array of
/// records in each body. The stream ends after the first error.
///
/// Pages are fetched lazily, one at a time, so a consumer that stops early stops the requests.
pub fn paginate<'a>(
    ctx: &'a SourceContext,
    auth: &'a Auth<'a>,
    first: RequestSpec,
    style: Pagination<'a>,
    items: &'a str,
) -> BoxStream<'a, Result<Page, RuntimeError>> {
    let driver = Driver::new(ctx, auth, first, style, items);
    stream::unfold(driver, |mut driver| async move {
        let page = driver.step().await?;
        Some((page, driver))
    })
    .boxed()
}

struct Driver<'a> {
    ctx: &'a SourceContext,
    auth: &'a Auth<'a>,
    style: Pagination<'a>,
    items: &'a str,
    /// The request to send next; `None` once the last page has been read or an error ended it.
    next: Option<RequestSpec>,
    /// An error found while reading the page just returned, reported on the next poll.
    deferred: Option<RuntimeError>,
    /// The cursors or URLs already requested.
    seen: HashSet<String>,
    pages: usize,
    max_pages: usize,
    /// The next offset or page number.
    position: u64,
}

impl<'a> Driver<'a> {
    fn new(
        ctx: &'a SourceContext,
        auth: &'a Auth<'a>,
        first: RequestSpec,
        style: Pagination<'a>,
        items: &'a str,
    ) -> Self {
        let mut seen = HashSet::new();
        let (first, position) = match style {
            Pagination::Cursor { .. } => (first, 0),
            Pagination::Offset {
                param,
                limit_param,
                limit,
            } => {
                let url = with_param(first.url(), limit_param, &limit.max(1).to_string());
                (first.with_url(with_param(&url, param, "0")), 0)
            }
            Pagination::Page {
                param,
                first: number,
                size_param,
            } => {
                let mut url = with_param(first.url(), param, &number.to_string());
                if let Some((size, count)) = size_param {
                    url = with_param(&url, size, &count.to_string());
                }
                (first.with_url(url), u64::from(number))
            }
            Pagination::LinkHeader => {
                seen.insert(first.url().to_string());
                (first, 0)
            }
        };
        Self {
            ctx,
            auth,
            style,
            items,
            next: Some(first),
            deferred: None,
            seen,
            pages: 0,
            max_pages: MAX_PAGES,
            position,
        }
    }

    async fn step(&mut self) -> Option<Result<Page, RuntimeError>> {
        if let Some(error) = self.deferred.take() {
            return Some(Err(error));
        }
        let spec = self.next.take()?;
        self.pages += 1;
        let result = self.fetch(spec).await;
        if result.is_err() {
            self.next = None;
        }
        Some(result)
    }

    async fn fetch(&mut self, spec: RequestSpec) -> Result<Page, RuntimeError> {
        let response = self.ctx.send(self.auth, spec.clone()).await?;
        let mut body = response.json()?;
        let found = take_items(&mut body, self.items)?;
        let count = found.len() as u64;

        match self.advance(&spec, &response, &body, count) {
            Ok(next) => self.next = next,
            // The page itself is good: hand it over, then report why there is no next one.
            Err(error) => self.deferred = Some(error),
        }
        if self.next.is_some() && self.pages >= self.max_pages {
            self.next = None;
            self.deferred = Some(RuntimeError::PaginationLoop);
        }
        Ok(Page { body, items: found })
    }

    /// The request for the page after the one just read, or `None` at the end.
    fn advance(
        &mut self,
        spec: &RequestSpec,
        response: &RuntimeResponse,
        body: &Value,
        count: u64,
    ) -> Result<Option<RequestSpec>, RuntimeError> {
        match self.style {
            Pagination::Cursor { param, next } => {
                let Some(cursor) = cursor_at(body, next)? else {
                    return Ok(None);
                };
                if !self.seen.insert(cursor.clone()) {
                    return Err(RuntimeError::PaginationLoop);
                }
                Ok(Some(spec.with_url(with_param(spec.url(), param, &cursor))))
            }
            Pagination::Offset { param, limit, .. } => {
                if count < u64::from(limit.max(1)) {
                    return Ok(None);
                }
                self.position += count;
                let url = with_param(spec.url(), param, &self.position.to_string());
                Ok(Some(spec.with_url(url)))
            }
            Pagination::Page {
                param, size_param, ..
            } => {
                let short = size_param.is_some_and(|(_, size)| count < u64::from(size));
                if count == 0 || short {
                    return Ok(None);
                }
                self.position += 1;
                let url = with_param(spec.url(), param, &self.position.to_string());
                Ok(Some(spec.with_url(url)))
            }
            Pagination::LinkHeader => {
                let Some(target) = next_link(response, spec.url()) else {
                    return Ok(None);
                };
                if !spec.owns(&target) {
                    return Err(RuntimeError::CrossOrigin);
                }
                if !self.seen.insert(target.to_string()) {
                    return Err(RuntimeError::PaginationLoop);
                }
                Ok(Some(spec.with_url(target)))
            }
        }
    }
}

/// Moves the array at `pointer` out of `body`. Anything else there is an unexpected shape.
fn take_items(body: &mut Value, pointer: &str) -> Result<Vec<Value>, RuntimeError> {
    match body.pointer_mut(pointer) {
        Some(Value::Array(items)) => Ok(std::mem::take(items)),
        _ => Err(RuntimeError::UnexpectedShape),
    }
}

/// The next cursor: a non-empty string or a number. Missing, null and empty end the stream.
fn cursor_at(body: &Value, pointer: &str) -> Result<Option<String>, RuntimeError> {
    match body.pointer(pointer) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if cursor.is_empty() => Ok(None),
        Some(Value::String(cursor)) => Ok(Some(cursor.clone())),
        Some(Value::Number(cursor)) => Ok(Some(cursor.to_string())),
        Some(_) => Err(RuntimeError::UnexpectedShape),
    }
}

/// `url` with the query parameter `key` set to `value`, replacing any it had.
fn with_param(url: &Url, key: &str, value: &str) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != key)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut url = url.clone();
    {
        let mut query = url.query_pairs_mut();
        query.clear();
        query.extend_pairs(kept);
        query.append_pair(key, value);
    }
    url
}

/// The `rel="next"` target of the response's `Link` headers, resolved against `base`.
fn next_link(response: &RuntimeResponse, base: &Url) -> Option<Url> {
    response
        .headers()
        .get_all(LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(links)
        .find(|(_, rels)| {
            rels.split_ascii_whitespace()
                .any(|rel| rel.eq_ignore_ascii_case("next"))
        })
        .and_then(|(target, _)| base.join(&target).ok())
}

/// The `(target, rel)` pairs of one `Link` header value (RFC 8288 section 3): comma-separated
/// link values, each a `<target>` followed by `;` parameters. Commas and semicolons inside the
/// target or a quoted string do not split.
fn links(header: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut rest = header;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>').map(|at| open + at) else {
            break;
        };
        let target = rest[open + 1..close].trim().to_owned();
        let (parameters, after) = split_parameters(&rest[close + 1..]);
        let rel = parameters
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("rel"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        found.push((target, rel));
        rest = after;
    }
    found
}

/// Reads `; name=value` parameters up to the comma that starts the next link value. Returns them
/// and what follows that comma.
fn split_parameters(text: &str) -> (Vec<(String, String)>, &str) {
    let mut parameters = Vec::new();
    let mut quoted = false;
    let mut end = text.len();
    let mut after = "";
    for (at, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                end = at;
                after = &text[at + 1..];
                break;
            }
            _ => {}
        }
    }
    let section = &text[..end];
    quoted = false;
    let mut start = 0;
    let mut pieces = Vec::new();
    for (at, character) in section.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                pieces.push(&section[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    pieces.push(&section[start..]);
    for piece in pieces {
        if let Some((name, value)) = piece.split_once('=') {
            let value = value.trim().trim_matches('"');
            parameters.push((name.trim().to_owned(), value.to_owned()));
        }
    }
    (parameters, after)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::extract::RawQuery;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use zirv_flow_core::SecretString;

    use super::super::testing::{Mock, context, serve};
    use super::*;

    /// The value of the query parameter `name`, if the request had it.
    fn param(query: &Option<String>, name: &str) -> Option<String> {
        url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    async fn read(
        mock: &Mock,
        style: Pagination<'_>,
        auth: &Auth<'_>,
    ) -> Vec<Result<Page, RuntimeError>> {
        let ctx = context("paged", "s");
        let first = RequestSpec::get(&mock.base(), mock.url("/items"));
        paginate(&ctx, auth, first, style, "/data").collect().await
    }

    fn ids(pages: &[Result<Page, RuntimeError>]) -> Vec<u64> {
        pages
            .iter()
            .flatten()
            .flat_map(|page| page.items.iter().map(|item| item["id"].as_u64().unwrap()))
            .collect()
    }

    fn items(range: std::ops::Range<u64>) -> Vec<Value> {
        range.map(|id| json!({ "id": id })).collect()
    }

    #[tokio::test]
    async fn a_cursor_is_followed_to_an_empty_last_page() {
        let mock = serve(Router::new().route(
            "/items",
            get(|RawQuery(query): RawQuery| async move {
                let body = match param(&query, "after").as_deref() {
                    None => json!({"data": items(0..2), "meta": {"next": "c2"}}),
                    Some("c2") => json!({"data": items(2..4), "meta": {"next": "c3"}}),
                    Some("c3") => json!({"data": [], "meta": {"next": null}}),
                    Some(_) => return StatusCode::BAD_REQUEST.into_response(),
                };
                axum::Json(body).into_response()
            }),
        ))
        .await;

        let pages = read(
            &mock,
            Pagination::Cursor {
                param: "after",
                next: "/meta/next",
            },
            &Auth::None,
        )
        .await;

        assert_eq!(pages.len(), 3);
        assert_eq!(ids(&pages), vec![0, 1, 2, 3]);
        assert_eq!(mock.hits(), 3);
        assert_eq!(pages[2].as_ref().unwrap().items.len(), 0);
    }

    #[tokio::test]
    async fn a_missing_null_or_empty_cursor_ends_the_stream() {
        for ending in [json!({}), json!({"next": null}), json!({"next": ""})] {
            let mock = serve(Router::new().route(
                "/items",
                get(move || {
                    let ending = ending.clone();
                    async move {
                        let mut body = ending;
                        body["data"] = json!([{"id": 1}]);
                        axum::Json(body)
                    }
                }),
            ))
            .await;

            let pages = read(
                &mock,
                Pagination::Cursor {
                    param: "after",
                    next: "/next",
                },
                &Auth::None,
            )
            .await;

            assert_eq!(ids(&pages), vec![1]);
            assert_eq!(pages.len(), 1);
        }
    }

    fn offset_server(total: u64) -> Router {
        Router::new().route(
            "/items",
            get(move |RawQuery(query): RawQuery| async move {
                let offset: u64 = param(&query, "offset").unwrap().parse().unwrap();
                let limit: u64 = param(&query, "limit").unwrap().parse().unwrap();
                let end = (offset + limit).min(total);
                axum::Json(json!({"data": items(offset.min(total)..end)}))
            }),
        )
    }

    #[tokio::test]
    async fn offsets_advance_by_the_page_and_a_short_page_ends_the_stream() {
        let mock = serve(offset_server(5)).await;
        let style = Pagination::Offset {
            param: "offset",
            limit_param: "limit",
            limit: 2,
        };

        let pages = read(&mock, style, &Auth::None).await;

        assert_eq!(ids(&pages), vec![0, 1, 2, 3, 4]);
        assert_eq!(pages.len(), 3);
    }

    #[tokio::test]
    async fn offsets_over_an_exact_multiple_end_on_an_empty_last_page() {
        let mock = serve(offset_server(4)).await;
        let style = Pagination::Offset {
            param: "offset",
            limit_param: "limit",
            limit: 2,
        };

        let pages = read(&mock, style, &Auth::None).await;

        assert_eq!(ids(&pages), vec![0, 1, 2, 3]);
        assert_eq!(pages.len(), 3);
        assert!(pages[2].as_ref().unwrap().items.is_empty());
    }

    fn page_server(total: u64, size: u64) -> Router {
        Router::new().route(
            "/items",
            get(move |RawQuery(query): RawQuery| async move {
                let page: u64 = param(&query, "page").unwrap().parse().unwrap();
                let start = ((page - 1) * size).min(total);
                let end = (start + size).min(total);
                axum::Json(json!({"data": items(start..end)}))
            }),
        )
    }

    #[tokio::test]
    async fn pages_count_from_the_first_and_an_empty_page_ends_the_stream() {
        let mock = serve(page_server(4, 2)).await;
        let style = Pagination::Page {
            param: "page",
            first: 1,
            size_param: None,
        };

        let pages = read(&mock, style, &Auth::None).await;

        assert_eq!(ids(&pages), vec![0, 1, 2, 3]);
        assert_eq!(pages.len(), 3, "two full pages, then the empty one");
    }

    #[tokio::test]
    async fn with_a_page_size_a_short_page_ends_the_stream_without_another_request() {
        let mock = serve(page_server(5, 2)).await;
        let style = Pagination::Page {
            param: "page",
            first: 1,
            size_param: Some(("per_page", 2)),
        };

        let pages = read(&mock, style, &Auth::None).await;

        assert_eq!(ids(&pages), vec![0, 1, 2, 3, 4]);
        assert_eq!(mock.hits(), 3);
        let sizes: Vec<_> = mock
            .seen()
            .iter()
            .map(|seen| seen.path_and_query.clone())
            .collect();
        assert!(
            sizes.iter().all(|path| path.contains("per_page=2")),
            "{sizes:?}"
        );
    }

    #[tokio::test]
    async fn pages_may_start_at_zero() {
        let mock = serve(Router::new().route(
            "/items",
            get(|RawQuery(query): RawQuery| async move {
                let page: u64 = param(&query, "page").unwrap().parse().unwrap();
                axum::Json(json!({"data": if page < 2 { items(page..page + 1) } else { vec![] }}))
            }),
        ))
        .await;
        let style = Pagination::Page {
            param: "page",
            first: 0,
            size_param: None,
        };

        let pages = read(&mock, style, &Auth::None).await;

        assert_eq!(ids(&pages), vec![0, 1]);
    }

    /// A server whose `/items?n=` pages link to the next with a `Link` header, a relative one
    /// from page 2 on, and whose last page, an empty one, has none.
    fn link_server() -> Router {
        Router::new().route(
            "/items",
            get(|RawQuery(query): RawQuery| async move {
                let n: u64 = param(&query, "n").map_or(1, |n| n.parse().unwrap());
                let link = match n {
                    1 => Some("</items?n=2>; rel=\"next\", </items?n=9>; rel=\"last\"".to_owned()),
                    2 => Some(
                        "</items?n=1>; rel=\"prev\"; title=\"a, b\", </items?n=3>; rel=\"next\""
                            .to_owned(),
                    ),
                    _ => None,
                };
                let data = if n < 3 { items(n..n + 1) } else { vec![] };
                let mut response = axum::Json(json!({"data": data})).into_response();
                if let Some(link) = link {
                    response.headers_mut().insert("link", link.parse().unwrap());
                }
                response
            }),
        )
    }

    #[tokio::test]
    async fn a_link_header_is_followed_to_the_page_without_one() {
        let mock = serve(link_server()).await;

        let pages = read(&mock, Pagination::LinkHeader, &Auth::None).await;

        assert_eq!(ids(&pages), vec![1, 2]);
        assert_eq!(pages.len(), 3);
        let paths: Vec<_> = mock
            .seen()
            .iter()
            .map(|s| s.path_and_query.clone())
            .collect();
        assert_eq!(paths, vec!["/items", "/items?n=2", "/items?n=3"]);
    }

    #[test]
    fn link_values_split_on_commas_and_semicolons_outside_targets_and_quotes() {
        let header = "<https://a/x?p=1,2>; rel=\"prev\"; title=\"x; y, z\", <https://a/y>; REL=\"next last\"";

        let found = links(header);

        assert_eq!(
            found,
            vec![
                ("https://a/x?p=1,2".to_owned(), "prev".to_owned()),
                ("https://a/y".to_owned(), "next last".to_owned()),
            ]
        );
        assert!(links("nonsense").is_empty());
        assert!(links("<unterminated").is_empty());
    }

    #[tokio::test]
    async fn a_repeating_cursor_ends_in_a_loop_error_after_the_pages_already_read() {
        let mock = serve(Router::new().route(
            "/items",
            get(|| async { axum::Json(json!({"data": [{"id": 1}], "next": "same"})) }),
        ))
        .await;

        let pages = read(
            &mock,
            Pagination::Cursor {
                param: "after",
                next: "/next",
            },
            &Auth::None,
        )
        .await;

        // The first page asks for "same", the second answers "same" again.
        assert_eq!(pages.len(), 3);
        assert!(pages[0].is_ok() && pages[1].is_ok());
        assert_eq!(
            pages[2].as_ref().unwrap_err(),
            &RuntimeError::PaginationLoop
        );
        assert_eq!(mock.hits(), 2, "the loop is cut before a third request");
    }

    #[tokio::test]
    async fn a_next_link_that_points_back_is_a_loop() {
        let mock = serve(Router::new().route(
            "/items",
            get(|| async {
                let mut response = axum::Json(json!({"data": []})).into_response();
                response
                    .headers_mut()
                    .insert("link", "</items>; rel=\"next\"".parse().unwrap());
                response
            }),
        ))
        .await;

        let pages = read(&mock, Pagination::LinkHeader, &Auth::None).await;

        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[1].as_ref().unwrap_err(),
            &RuntimeError::PaginationLoop
        );
        assert_eq!(mock.hits(), 1);
    }

    #[tokio::test]
    async fn a_next_link_on_another_origin_is_refused_and_never_requested() {
        let other = serve(Router::new().route("/items", get(|| async { "theirs" }))).await;
        let target = other.url("/items").to_string();
        let source = serve(Router::new().route(
            "/items",
            get(move || {
                let target = target.clone();
                async move {
                    let mut response = axum::Json(json!({"data": [{"id": 1}]})).into_response();
                    response
                        .headers_mut()
                        .insert("link", format!("<{target}>; rel=\"next\"").parse().unwrap());
                    response
                }
            }),
        ))
        .await;
        let secret = SecretString::new("bearer-secret".into());

        let pages = read(
            &source,
            Pagination::LinkHeader,
            &Auth::Bearer { token: &secret },
        )
        .await;

        assert_eq!(ids(&pages), vec![1]);
        assert_eq!(pages[1].as_ref().unwrap_err(), &RuntimeError::CrossOrigin);
        assert_eq!(other.hits(), 0, "the other origin saw nothing");
    }

    #[tokio::test]
    async fn items_that_are_not_an_array_are_an_unexpected_shape_and_end_the_stream() {
        for body in [
            json!({"data": {"id": 1}}),
            json!({"other": []}),
            json!([1, 2]),
        ] {
            let mock = serve(Router::new().route(
                "/items",
                get(move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }),
            ))
            .await;

            let pages = read(
                &mock,
                Pagination::Cursor {
                    param: "after",
                    next: "/next",
                },
                &Auth::None,
            )
            .await;

            assert_eq!(pages.len(), 1);
            assert_eq!(
                pages[0].as_ref().unwrap_err(),
                &RuntimeError::UnexpectedShape
            );
        }
    }

    #[tokio::test]
    async fn every_page_carries_the_auth_and_pages_are_read_lazily() {
        let mock = serve(page_server(100, 2)).await;
        let secret = SecretString::new("page-token".into());
        let ctx = context("paged", "s");
        let auth = Auth::Bearer { token: &secret };
        let first = RequestSpec::get(&mock.base(), mock.url("/items"));
        let mut pages = paginate(
            &ctx,
            &auth,
            first,
            Pagination::Page {
                param: "page",
                first: 1,
                size_param: None,
            },
            "/data",
        );

        pages.next().await.unwrap().unwrap();
        pages.next().await.unwrap().unwrap();
        drop(pages);

        assert_eq!(mock.hits(), 2, "no page is fetched before it is asked for");
        let seen = mock.seen();
        let headers: Vec<_> = seen
            .iter()
            .map(|seen| {
                seen.headers
                    .get("authorization")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(headers, vec!["Bearer page-token"; 2]);
    }

    #[tokio::test]
    async fn an_error_from_a_page_ends_the_stream() {
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let mock = serve(Router::new().route(
            "/items",
            get(move || {
                *counted.lock().unwrap() += 1;
                async { StatusCode::NOT_FOUND }
            }),
        ))
        .await;

        let pages = read(
            &mock,
            Pagination::Page {
                param: "page",
                first: 1,
                size_param: None,
            },
            &Auth::None,
        )
        .await;

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].as_ref().unwrap_err(), &RuntimeError::Status(404));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn a_parameter_replaces_the_one_the_url_already_has() {
        let url: Url = "http://h/p?a=1&after=old&b=x+y".parse().unwrap();

        let updated = with_param(&url, "after", "new");

        assert_eq!(updated.query(), Some("a=1&b=x+y&after=new"));
    }

    #[tokio::test]
    async fn an_api_that_ignores_the_offset_ends_at_the_page_cap_as_a_loop() {
        // Always a full page, whatever the offset: nothing else would stop this.
        let mock = serve(Router::new().route(
            "/items",
            get(|| async { axum::Json(json!({"data": [{"id": 1}, {"id": 2}]})) }),
        ))
        .await;
        let ctx = context("paged", "s");
        let first = RequestSpec::get(&mock.base(), mock.url("/items"));
        let style = Pagination::Offset {
            param: "offset",
            limit_param: "limit",
            limit: 2,
        };
        let mut driver = Driver::new(&ctx, &Auth::None, first, style, "/data");
        driver.max_pages = 3;

        let mut results = Vec::new();
        while let Some(result) = driver.step().await {
            results.push(result);
        }

        assert_eq!(results.len(), 4);
        assert!(results[..3].iter().all(Result::is_ok));
        assert_eq!(
            results[3].as_ref().unwrap_err(),
            &RuntimeError::PaginationLoop
        );
        assert_eq!(mock.hits(), 3);
    }
}
