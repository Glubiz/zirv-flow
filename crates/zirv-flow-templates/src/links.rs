//! The pure post-render steps (architecture 11.5 step 7): click-tracking link rewrite, the
//! unsubscribe link check and the `List-Unsubscribe` headers.
//!
//! Nothing here signs or hashes. The caller passes the token function, and the signed
//! unsubscribe URL arrives in the render context.

use std::ops::Range;

use crate::error::RenderError;
use crate::utm::UtmTags;

/// The token every rewritten link carries in inert (preview) mode.
pub const INERT_TOKEN: &str = "inert";

/// The caller's token function: `(link_index, url) -> token`. It is `Sync` so a render call that
/// holds the hooks across an await (the isolated render host's pool) is still `Send`.
type TokenFn<'a> = dyn Fn(usize, &str) -> String + Sync + 'a;

/// What to do with the links in the rendered HTML.
#[derive(Clone, Copy)]
pub enum Tracking<'a> {
    /// Leave links alone (transactional mail without click tracking).
    Off,
    /// Rewrite each `http` or `https` link to `{base_url}/{token}`, where the token is
    /// `token(link_index, url)`. `link_index` counts the rewritten links from 0, in document
    /// order; the caller signs the token.
    Rewrite {
        base_url: &'a str,
        token: &'a TokenFn<'a>,
    },
    /// Preview: rewrite every link to `{base_url}/inert`, so the structure matches a real send
    /// and no link is live.
    Inert { base_url: &'a str },
}

/// The caller's post-render choices.
#[derive(Clone, Copy)]
pub struct Hooks<'a> {
    pub tracking: Tracking<'a>,
    /// Automatic UTM parameters (architecture 10.2), applied before click tracking. `None`
    /// leaves the output exactly as it was without the step.
    pub utm: Option<UtmTags<'a>>,
}

impl Hooks<'_> {
    /// No click tracking.
    pub const fn untracked() -> Hooks<'static> {
        Hooks {
            tracking: Tracking::Off,
            utm: None,
        }
    }
}

/// The byte span of one `href` value inside an `<a>` tag.
pub(crate) struct Href {
    /// The value, without quotes.
    pub(crate) value: Range<usize>,
    /// The span to replace: the value with its quotes, if it had any.
    pub(crate) outer: Range<usize>,
}

/// A `data-zf-utm="off"` attribute on an `<a>` tag, with or without an `href`.
pub(crate) struct OptOut {
    /// The span of the attribute, with the whitespace before it.
    pub(crate) attribute: Range<usize>,
    /// The value span of the first `href` of the same tag, if it has one.
    pub(crate) href: Option<Range<usize>>,
}

/// The elements whose content is text, not markup: an `<a>` inside is not a link.
const RAW_TEXT_ELEMENTS: [&str; 4] = ["script", "style", "textarea", "title"];

/// The position of `needle` (lower-case ASCII) in `html` from `from`, ignoring ASCII case.
pub(crate) fn find_ignore_case(html: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let needle = needle.as_bytes();
    (from..bytes.len().saturating_sub(needle.len().saturating_sub(1)))
        .find(|&i| bytes[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// If a comment or raw-text element starts at `start` (a `<`), the position after it.
fn skip_non_markup(html: &str, start: usize) -> Option<usize> {
    let rest = &html[start..];
    if rest.starts_with("<!--") {
        // `<!-->` and `<!--->` end the comment at once; `--!>` also closes one.
        let from = start + 2;
        let close = [
            html[from..].find("-->").map(|n| (from + n, 3)),
            html[from..].find("--!>").map(|n| (from + n, 4)),
        ]
        .into_iter()
        .flatten()
        .min();
        return Some(close.map_or(html.len(), |(at, len)| at + len));
    }
    let bytes = html.as_bytes();
    for name in RAW_TEXT_ELEMENTS {
        let after = start + 1 + name.len();
        let named = bytes
            .get(start + 1..after)
            .is_some_and(|b| b.eq_ignore_ascii_case(name.as_bytes()));
        let ends_name = bytes
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || matches!(*b, b'>' | b'/' | 0));
        if named && ends_name {
            let closer = format!("</{name}");
            return Some(find_ignore_case(html, &closer, after).unwrap_or(html.len()));
        }
    }
    None
}

/// The first `href` of every `<a>` tag, in document order. Comments and the content of raw-text
/// elements hold no links.
pub(crate) fn hrefs(html: &str) -> Vec<Href> {
    anchors(html).0
}

/// The `data-zf-utm="off"` attribute of every `<a>` tag, in document order.
pub(crate) fn opt_outs(html: &str) -> Vec<OptOut> {
    anchors(html).1
}

/// The links and the opt-out attributes of every `<a>` tag, from one scan.
fn anchors(html: &str) -> (Vec<Href>, Vec<OptOut>) {
    let bytes = html.as_bytes();
    let mut found = Vec::new();
    let mut opted_out = Vec::new();
    let mut at = 0;
    while let Some(offset) = html[at..].find('<') {
        let start = at + offset;
        at = start + 1;
        if let Some(end) = skip_non_markup(html, start) {
            at = end.max(at);
            continue;
        }
        let is_anchor = bytes
            .get(start + 1)
            .is_some_and(|b| b.eq_ignore_ascii_case(&b'a'))
            && bytes
                .get(start + 2)
                .is_some_and(|b| b.is_ascii_whitespace() || matches!(*b, b'/' | 0));
        if !is_anchor {
            continue;
        }
        at = scan_tag(html, start + 2, &mut found, &mut opted_out);
    }
    (found, opted_out)
}

/// Scan the attributes of one tag from `from`; returns the position after the tag.
fn scan_tag(html: &str, from: usize, found: &mut Vec<Href>, opted_out: &mut Vec<OptOut>) -> usize {
    let bytes = html.as_bytes();
    let mut at = from;
    // Browsers use the first `href` of a tag and ignore the rest.
    let mut seen_href = false;
    let first_found = found.len();
    let mut offs: Vec<Range<usize>> = Vec::new();
    let end = loop {
        let gap_start = at;
        while bytes
            .get(at)
            .is_some_and(|b| b.is_ascii_whitespace() || matches!(*b, b'/' | 0))
        {
            at += 1;
        }
        match bytes.get(at) {
            None => break at,
            Some(b'>') => break at + 1,
            Some(_) => {}
        }
        let name_start = at;
        while bytes
            .get(at)
            .is_some_and(|b| !b.is_ascii_whitespace() && !matches!(*b, b'=' | b'>' | b'/'))
        {
            at += 1;
        }
        let name = &html[name_start..at];
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            continue;
        }
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let (value, outer) = match bytes.get(at) {
            Some(&quote) if quote == b'"' || quote == b'\'' => {
                let value_start = at + 1;
                let value_end = html[value_start..]
                    .find(quote as char)
                    .map_or(html.len(), |n| value_start + n);
                let outer_end = (value_end + 1).min(html.len());
                at = outer_end;
                (value_start..value_end, (value_start - 1)..outer_end)
            }
            Some(_) => {
                let value_start = at;
                while bytes
                    .get(at)
                    .is_some_and(|b| !b.is_ascii_whitespace() && *b != b'>')
                {
                    at += 1;
                }
                (value_start..at, value_start..at)
            }
            None => break at,
        };
        if name.eq_ignore_ascii_case("href") && !seen_href {
            seen_href = true;
            found.push(Href { value, outer });
        } else if name.eq_ignore_ascii_case("data-zf-utm")
            && html[value.clone()].trim().eq_ignore_ascii_case("off")
        {
            offs.push(gap_start..at);
        }
    };
    let href = found.get(first_found).map(|href| href.value.clone());
    opted_out.extend(offs.into_iter().map(|attribute| OptOut {
        attribute,
        href: href.clone(),
    }));
    end
}

/// Decode the entities an attribute value is escaped with: the named `amp`, `lt`, `gt`, `quot`
/// and `apos`, and numeric ones (the sandbox writes `/` as `&#x2f;` inside HTML fields).
pub(crate) fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let character = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                // The one named entity `ammonia` writes besides the five: a no-break space.
                "nbsp" => Some(' '),
                _ => name.strip_prefix('#').and_then(|number| {
                    let code = match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            character.map(|c| (c, end + 1))
        });
        match decoded {
            Some((character, used)) => {
                out.push(character);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Whether `text` starts with `prefix`, ignoring ASCII case.
pub(crate) fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

/// Whether a URL is an `http` or `https` link (the only ones that are tracked).
pub(crate) fn is_web_url(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// The decoded `href` values of the `<a>` tags in `html`, in document order.
pub(crate) fn link_urls(html: &str) -> Vec<String> {
    hrefs(html)
        .into_iter()
        .map(|href| unescape(html[href.value].trim()))
        .collect()
}

/// Whether `html` has an `<a>` whose `href` is exactly `url`.
pub(crate) fn links_to(html: &str, url: &str) -> bool {
    !url.is_empty() && link_urls(html).iter().any(|found| found == url)
}

/// Whether an `<a>` `href` of `html` contains `text`. Used on template source, where the href is
/// not rendered yet.
pub(crate) fn href_mentions(html: &str, text: &str) -> bool {
    link_urls(html).iter().any(|found| found.contains(text))
}

/// Rewrite the `http` and `https` links of `html` for click tracking.
///
/// Links that are not web URLs (`mailto:`, `tel:`, `#anchor`) and links to a URL in `keep` (the
/// message's own unsubscribe and preference URLs, which are already signed) are left alone.
pub fn rewrite_links(html: &str, tracking: &Tracking<'_>, keep: &[&str]) -> String {
    let (base_url, token): (&str, Option<&TokenFn<'_>>) = match tracking {
        Tracking::Off => return html.to_owned(),
        Tracking::Rewrite { base_url, token } => (base_url, Some(*token)),
        Tracking::Inert { base_url } => (base_url, None),
    };
    let base = base_url.trim_end_matches('/');
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut index = 0;
    for href in hrefs(html) {
        let url = unescape(html[href.value.clone()].trim());
        if !is_web_url(&url) || keep.contains(&url.as_str()) {
            continue;
        }
        let token = match token {
            Some(token) => token(index, &url),
            None => INERT_TOKEN.to_owned(),
        };
        index += 1;
        out.push_str(&html[copied..href.outer.start]);
        out.push('"');
        out.push_str(&escape(&format!("{base}/{token}")));
        out.push('"');
        copied = href.outer.end;
    }
    out.push_str(&html[copied..]);
    out
}

/// The `List-Unsubscribe` and `List-Unsubscribe-Post` headers for one-click unsubscribe
/// (RFC 8058). The URL must be `https`, because the mail client POSTs to it.
pub fn unsubscribe_headers(unsubscribe_url: &str) -> Result<Vec<(String, String)>, RenderError> {
    let https = unsubscribe_url
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"));
    let clean = !unsubscribe_url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'));
    if !https || !clean || unsubscribe_url.len() <= 8 {
        return Err(RenderError::InvalidUnsubscribeUrl);
    }
    Ok(vec![
        (
            "List-Unsubscribe".to_owned(),
            format!("<{unsubscribe_url}>"),
        ),
        (
            "List-Unsubscribe-Post".to_owned(),
            "List-Unsubscribe=One-Click".to_owned(),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(index: usize, url: &str) -> String {
        format!("t{index}-{}", url.len())
    }

    fn live<'a>() -> Tracking<'a> {
        Tracking::Rewrite {
            base_url: "https://t.example/c/",
            token: &token,
        }
    }

    #[test]
    fn rewrites_web_links_in_order() {
        let html = r#"<p><a href="https://a.example/x?a=1&amp;b=2">A</a> <a class=b href='http://b.example'>B</a></p>"#;
        assert_eq!(
            rewrite_links(html, &live(), &[]),
            r#"<p><a href="https://t.example/c/t0-27">A</a> <a class=b href="https://t.example/c/t1-16">B</a></p>"#
        );
    }

    #[test]
    fn token_function_sees_the_decoded_url() {
        let seen = std::sync::Mutex::new(Vec::new());
        let record = |index: usize, url: &str| {
            seen.lock().unwrap().push((index, url.to_owned()));
            "x".to_owned()
        };
        let tracking = Tracking::Rewrite {
            base_url: "https://t.example",
            token: &record,
        };
        rewrite_links(
            r#"<a href="https://a.example/?a=1&amp;b=2">"#,
            &tracking,
            &[],
        );
        assert_eq!(
            seen.into_inner().unwrap(),
            vec![(0, "https://a.example/?a=1&b=2".to_owned())]
        );
    }

    #[test]
    fn other_schemes_and_kept_urls_are_untouched_and_not_counted() {
        let html = concat!(
            r##"<a href="mailto:a@b.example">m</a><a href="#top">t</a>"##,
            r#"<a href="https://u.example/unsub?s=1&amp;t=2">u</a>"#,
            r#"<a href="https://a.example">a</a>"#,
        );
        let out = rewrite_links(html, &live(), &["https://u.example/unsub?s=1&t=2"]);
        assert!(out.contains(r#"href="mailto:a@b.example""#));
        assert!(out.contains(r##"href="#top""##));
        assert!(out.contains(r#"href="https://u.example/unsub?s=1&amp;t=2""#));
        assert!(out.contains(r#"href="https://t.example/c/t0-17""#));
    }

    #[test]
    fn inert_mode_uses_one_fixed_token_and_never_calls_the_function() {
        let html = r#"<a href="https://a.example/1">1</a><a href="https://b.example/2">2</a>"#;
        let out = rewrite_links(
            html,
            &Tracking::Inert {
                base_url: "https://preview.example",
            },
            &[],
        );
        assert_eq!(
            out,
            r#"<a href="https://preview.example/inert">1</a><a href="https://preview.example/inert">2</a>"#
        );
    }

    #[test]
    fn slash_and_nul_separate_the_anchor_name_from_its_attributes() {
        let html = "<a/href=\"https://a.example/1\">1</a><a\0href=\"https://a.example/2\">2</a>";
        assert_eq!(
            link_urls(html),
            ["https://a.example/1", "https://a.example/2"]
        );
        assert_eq!(
            rewrite_links(html, &live(), &[]),
            "<a/href=\"https://t.example/c/t0-19\">1</a><a\0href=\"https://t.example/c/t1-19\">2</a>"
        );
    }

    #[test]
    fn comments_and_raw_text_elements_hold_no_links() {
        let html = concat!(
            r#"<!-- <a href="https://a.example/1">x</a> -->"#,
            r#"<SCRIPT>var s = "<a href='https://a.example/2'>";</SCRIPT>"#,
            r#"<style>a::after { content: "<a href=https://a.example/3>" }</style>"#,
            r#"<textarea><a href="https://a.example/4"></textarea>"#,
            r#"<title><a href="https://a.example/5"></title>"#,
            r#"<!--> <a href="https://a.example/6">six</a>"#,
            r#"<p><a href="https://a.example/7">seven</a>"#,
        );
        assert_eq!(
            link_urls(html),
            ["https://a.example/6", "https://a.example/7"]
        );
        assert!(!links_to(
            r#"<!-- <a href="https://u.example">u</a> -->"#,
            "https://u.example"
        ));
        let out = rewrite_links(html, &live(), &[]);
        assert!(out.contains(r#"<a href="https://a.example/1">"#));
        assert!(out.contains(r#"href="https://t.example/c/t0-19""#));
        assert!(out.contains(r#"href="https://t.example/c/t1-19""#));
    }

    #[test]
    fn only_the_first_href_of_a_tag_counts() {
        let html = r#"<a href="https://a.example/first" href="https://a.example/second">x</a>"#;
        assert_eq!(link_urls(html), ["https://a.example/first"]);
        assert!(!links_to(html, "https://a.example/second"));
        assert_eq!(
            rewrite_links(html, &live(), &[]),
            r#"<a href="https://t.example/c/t0-23" href="https://a.example/second">x</a>"#
        );
    }

    #[test]
    fn off_changes_nothing() {
        let html = r#"<a href="https://a.example">a</a>"#;
        assert_eq!(rewrite_links(html, &Tracking::Off, &[]), html);
    }

    #[test]
    fn non_anchor_tags_and_text_are_not_rewritten() {
        let html = r#"<link href="https://a.example/s.css"><abbr href="https://b.example">x</abbr> https://c.example"#;
        assert_eq!(rewrite_links(html, &live(), &[]), html);
    }

    #[test]
    fn unterminated_markup_does_not_panic() {
        for html in [
            "<a href=\"https://a.example",
            "<a href=",
            "<a ",
            "<a href='x",
            "<",
        ] {
            let _ = rewrite_links(html, &live(), &[]);
        }
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            unescape("a&amp;b&#x2f;c&#47;d&lt;&quot;&#39;&bogus; &"),
            "a&b/c/d<\"'&bogus; &"
        );
    }

    #[test]
    fn links_to_matches_the_decoded_href() {
        let html = r#"<a href="https://u.example/?a=1&amp;b=2">u</a>"#;
        assert!(links_to(html, "https://u.example/?a=1&b=2"));
        assert!(!links_to(html, "https://u.example/"));
        assert!(!links_to(html, ""));
    }

    #[test]
    fn unsubscribe_headers_follow_rfc_8058() {
        let headers = unsubscribe_headers("https://u.example/u/abc").unwrap();
        assert_eq!(
            headers,
            vec![
                (
                    "List-Unsubscribe".to_owned(),
                    "<https://u.example/u/abc>".to_owned()
                ),
                (
                    "List-Unsubscribe-Post".to_owned(),
                    "List-Unsubscribe=One-Click".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn unsubscribe_url_must_be_clean_https() {
        for url in [
            "",
            "https://",
            "http://u.example/u",
            "https://u.example/u\r\nBcc: x",
            "https://u.example/<u>",
            "https://u.example/a b",
        ] {
            assert_eq!(
                unsubscribe_headers(url),
                Err(RenderError::InvalidUnsubscribeUrl),
                "{url:?}"
            );
        }
    }
}
