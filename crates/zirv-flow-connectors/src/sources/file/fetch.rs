//! One byte-range window of the file, fetched through the source context.
//!
//! Every request goes through [`SourceContext::send`], so auth, retries with `Retry-After`, the
//! per-source rate limit, the egress guard and the redirect re-check are the runtime's. Errors
//! never hold the URL.

use bytes::Bytes;
use url::Url;

use super::error;
use crate::http::HttpError;
use crate::runtime::{Auth, Method, RequestSpec, RuntimeError, RuntimeResponse};
use crate::sources::{SourceContext, SourceError};

/// What an earlier window of the same read saw, which a later one must still see.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Expect {
    pub(super) validator: Option<String>,
    pub(super) size: Option<u64>,
}

/// One window of the file.
#[derive(Debug)]
pub(super) struct Window {
    pub(super) bytes: Bytes,
    /// The size of the whole file, when the server said.
    pub(super) total: Option<u64>,
    /// `ETag`, else `Last-Modified`.
    pub(super) validator: Option<String>,
    /// Whether the window reaches the end of the file.
    pub(super) ended: bool,
}

/// Fetches `window` bytes from `offset`. The first window of a read passes no expectation; every
/// later one passes what the first saw, and the file changing is [`error::file_changed`].
pub(super) async fn fetch_window(
    ctx: &SourceContext,
    auth: &Auth<'_>,
    url: &Url,
    offset: u64,
    window: u64,
    expect: &Expect,
) -> Result<Window, SourceError> {
    let last = offset.saturating_add(window).saturating_sub(1);
    let mut request = RequestSpec::new(Method::GET, url, url.clone())
        .header("range", format!("bytes={offset}-{last}"))
        // A compressed body would make the byte positions the server's, not the file's.
        .header("accept-encoding", "identity");
    if offset > 0
        && let Some(validator) = &expect.validator
    {
        request = request.header("if-range", validator.clone());
    }
    let response = match ctx.send(auth, request).await {
        Ok(response) => response,
        Err(RuntimeError::Status(412)) => return Err(error::file_changed()),
        // The range starts at or past the end: an empty file, or a file that shrank.
        Err(RuntimeError::Status(416)) if offset == 0 => {
            return Ok(Window {
                bytes: Bytes::new(),
                total: Some(0),
                validator: None,
                ended: true,
            });
        }
        Err(RuntimeError::Status(416)) => return Err(error::file_changed()),
        Err(RuntimeError::Http(HttpError::BodyTooLarge)) => {
            return Err(error::file_range_unsupported());
        }
        Err(other) => return Err(other.into()),
    };
    read_window(&response, offset, window, expect)
}

fn read_window(
    response: &RuntimeResponse,
    offset: u64,
    window: u64,
    expect: &Expect,
) -> Result<Window, SourceError> {
    let validator = validator(response);
    let bytes = response.body().clone();
    let status = response.status().as_u16();
    let (total, ended) = match status {
        206 => {
            let range = content_range(response).ok_or_else(error::file_range_unsupported)?;
            if range.first != offset {
                return Err(error::file_range_unsupported());
            }
            let ended = match range.total {
                Some(total) => range.last.saturating_add(1) >= total,
                None => (bytes.len() as u64) < window,
            };
            (range.total, ended)
        }
        // The server ignored `Range` and sent the file whole, which is the first window or a
        // changed file: a read in progress asked with `If-Range` and expects 206.
        200 if offset == 0 => (Some(bytes.len() as u64), true),
        200 => return Err(error::file_changed()),
        other => return Err(SourceError::Upstream(other)),
    };
    if let (Some(known), Some(now)) = (expect.size, total)
        && known != now
    {
        return Err(error::file_changed());
    }
    if let (Some(known), Some(now)) = (&expect.validator, &validator)
        && known != now
    {
        return Err(error::file_changed());
    }
    Ok(Window {
        bytes,
        total,
        validator,
        ended,
    })
}

/// A strong `ETag`, else `Last-Modified`: the two a later `If-Range` can use. A weak `ETag` never
/// matches in `If-Range`.
fn validator(response: &RuntimeResponse) -> Option<String> {
    let text = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    text("etag")
        .filter(|etag| !etag.starts_with("W/"))
        .or_else(|| text("last-modified"))
}

#[derive(Debug, Eq, PartialEq)]
struct ContentRange {
    first: u64,
    last: u64,
    total: Option<u64>,
}

fn content_range(response: &RuntimeResponse) -> Option<ContentRange> {
    let value = response.headers().get("content-range")?.to_str().ok()?;
    parse_content_range(value)
}

/// `bytes <first>-<last>/<total>` where the total may be `*`.
fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        digits => Some(digits.parse().ok()?),
    };
    let (first, last): (u64, u64) = (first.trim().parse().ok()?, last.trim().parse().ok()?);
    (first <= last).then_some(ContentRange { first, last, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_range_is_read_with_its_total_or_without() {
        assert_eq!(
            parse_content_range("bytes 0-1023/5000"),
            Some(ContentRange {
                first: 0,
                last: 1023,
                total: Some(5000)
            })
        );
        assert_eq!(
            parse_content_range(" bytes 10-20/* "),
            Some(ContentRange {
                first: 10,
                last: 20,
                total: None
            })
        );
        for bad in [
            "",
            "bytes */100",
            "items 0-1/2",
            "bytes 5-1/9",
            "bytes a-b/c",
            "bytes 0-1",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }
}
